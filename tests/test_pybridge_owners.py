"""M3 narrow-owner tests for the Python bridge (single-file, no new harness).

Covers only the two retained owners, always through the Session surface
(or the owner's own bounded API where production routes through it):
TargetRegistry lifecycle/swap (incl. the missing-child scope-exit
regression) and ServerState pool/close. Breakpoint bookkeeping and the
stop/wait/capture machine stay Session-owned sections (swapped-context +
thread-local semantics) and are covered by tests/test_pybridge.py,
test_wait_capture.py and the concurrency matrices — no wrapper tests.
"""
import importlib.util
import socket
import subprocess
import sys
import threading
import unittest
from pathlib import Path
from unittest.mock import Mock

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location(
    "pybridge_owners", ROOT / "bridge/py/src/pybridge.py")
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)


def make_session():
    return bridge.Session(bridge.Config())


def make_child(st, tid, pid, suspended=False):
    child = bridge.ChildTarget(tid, pid, Mock(), Mock(), {"pid": pid})
    st.targets[tid] = child
    st.target_order.append(tid)
    st._seen_ids.add(tid)
    child.state = "stopped" if suspended else "running"
    child.suspended = suspended
    return child


class TargetRegistryLifecycleTests(unittest.TestCase):
    def test_note_exit_moves_to_bounded_history(self):
        st = make_session()
        make_child(st, "child:1", 1)
        entry = st._note_exit("child:1", last_stop={"file": "a", "line": 1})
        self.assertEqual(entry["id"], "child:1")
        self.assertNotIn("child:1", st.targets)
        self.assertEqual(len(st.exited_targets), 1)
        self.assertTrue(st.assert_owners())
        with self.assertRaises(bridge.BridgeErr):
            st.resolve_target({"target": "child:1"})
        with self.assertRaises(bridge.BridgeErr):
            st.resolve_target({"target": "nope"})

    def test_exited_history_bound_and_counter(self):
        st = make_session()
        for i in range(bridge.MAX_EXITED_HISTORY + 3):
            make_child(st, f"child:{i}", i)
            st._note_exit(f"child:{i}")
        self.assertEqual(len(st.exited_targets), bridge.MAX_EXITED_HISTORY)
        self.assertEqual(st.targets_reg.dropped_exited, 3)
        self.assertTrue(st.targets_reg.assert_valid())

    def test_ids_never_reused_and_queue_overflow_ignored(self):
        st = make_session()
        st.cfg.subprocess = True
        st.cfg.kind = "launch"
        st._stage_attach({"subProcessId": 7})
        st._stage_attach({"subProcessId": 7})  # dedup: no reuse
        self.assertEqual(len(st._attach_pending), 1)
        for pid in range(100, 100 + bridge.MAX_PENDING_ATTACH + 2):
            st._stage_attach({"subProcessId": pid})
        self.assertLessEqual(len(st._attach_pending),
                             bridge.MAX_PENDING_ATTACH)
        self.assertGreaterEqual(st.targets_reg.ignored, 2)
        self.assertTrue(st.targets_reg.assert_valid())

    def test_auto_select_most_recent_stop(self):
        st = make_session()
        a = make_child(st, "child:1", 1, suspended=True)
        b = make_child(st, "child:2", 2, suspended=True)
        a.stop_seq, b.stop_seq = 1, 2
        st._stop_seq = 2
        self.assertEqual(st.resolve_target({}), "child:2")
        self.assertEqual(st.resolve_target({"target": "child:1"}), "child:1")

    def test_commit_child_attaches(self):
        st = make_session()
        child = bridge.ChildTarget("child:9", 9, None, None, {"pid": 9})

        class FakeSock:
            closed = False

            def close(self):
                self.closed = True

        sock = FakeSock()
        self.assertEqual(st.targets_reg.commit_child(child, sock), "attached")
        self.assertIn("child:9", st.targets)
        self.assertFalse(sock.closed)
        self.assertTrue(st.assert_owners())


class TargetSwapTests(unittest.TestCase):
    def test_scope_roundtrip_restores_main(self):
        st = make_session()
        child = make_child(st, "child:3", 3)
        st.suspended, st.thread_id = True, 11
        child.suspended, child.thread_id = False, None
        with st._TargetScope(st, "child:3"):
            self.assertFalse(st.suspended)
            self.assertEqual(st.targets_reg.serving, "child:3")
        self.assertTrue(st.suspended)
        self.assertEqual(st.thread_id, 11)
        self.assertEqual(st.targets_reg.serving, "main")

    def test_scope_exit_with_missing_child_restores_main(self):
        # Regression: a child removed mid-command (exit snapshotted while
        # serving it) must not corrupt the main context on scope exit, and
        # the removal must stand (no resurrection).
        st = make_session()
        child = make_child(st, "child:61", 61)
        st.suspended, st.thread_id = True, 11
        st.frames = [{"id": 1}]
        child.suspended, child.thread_id = True, 61
        with st._TargetScope(st, "child:61"):
            self.assertEqual(st.thread_id, 61)
            st._note_exit("child:61")  # terminal event mid-command
            self.assertNotIn("child:61", st.targets)
        self.assertTrue(st.suspended)
        self.assertEqual(st.thread_id, 11)
        self.assertEqual(st.frames, [{"id": 1}])
        self.assertEqual(st.targets_reg.serving, "main")
        self.assertNotIn("child:61", st.targets)
        self.assertEqual(st.exited_targets[-1]["id"], "child:61")

    def test_set_serving_accepts_main_or_registered(self):
        st = make_session()
        make_child(st, "child:5", 5)
        st.targets_reg.set_serving("child:5")
        self.assertEqual(st.targets_reg.serving, "child:5")
        self.assertTrue(st.targets_reg.assert_valid())
        st.targets_reg.reset_serving()
        self.assertEqual(st.targets_reg.serving, "main")
        st.targets_reg.set_serving("main")
        self.assertEqual(st.targets_reg.serving, "main")

    def test_set_serving_rejects_unknown_with_existing_error(self):
        # Unknown ids keep the existing resolve_inner vocabulary and are
        # rejected before any write: serving stays main.
        st = make_session()
        with self.assertRaisesRegex(bridge.BridgeErr, "unknown target: nope"):
            st.targets_reg.set_serving("nope")
        self.assertEqual(st.targets_reg.serving, "main")
        self.assertTrue(st.targets_reg.assert_valid())

    def test_reset_serving_unconditional_after_mid_scope_removal(self):
        # Reset must not require the child to still be registered (the
        # missing-child exit path removes it mid-command).
        st = make_session()
        make_child(st, "child:7", 7)
        st.targets_reg.set_serving("child:7")
        st._note_exit("child:7")
        st.targets_reg.reset_serving()
        self.assertEqual(st.targets_reg.serving, "main")
        self.assertTrue(st.targets_reg.assert_valid())

    def test_scope_entry_unknown_tid_raises_before_swap(self):
        # Entry with an unknown id raises the existing KeyError (never a
        # silent serve), leaves serving on main, and releases the gate.
        # The bare-dump churn path depends on skipping exactly this.
        st = make_session()
        with self.assertRaises(KeyError):
            with st._TargetScope(st, "child:ghost"):
                pass
        self.assertEqual(st.targets_reg.serving, "main")
        self.assertTrue(st._gate.acquire(blocking=False))
        st._gate.release()


class ServerStateTests(unittest.TestCase):
    def test_pool_bound_and_release(self):
        st = make_session()
        for _ in range(bridge.MAX_ACTIVE_HANDLERS):
            self.assertTrue(
                st.server_state.try_admit(bridge.MAX_ACTIVE_HANDLERS))
        self.assertFalse(
            st.server_state.try_admit(bridge.MAX_ACTIVE_HANDLERS))
        self.assertTrue(st.server_state.assert_valid())
        for _ in range(bridge.MAX_ACTIVE_HANDLERS):
            st.server_state.release()
        self.assertEqual(st.server_state.active, 0)
        self.assertTrue(st.server_state.assert_valid())

    def test_release_below_zero_rejected(self):
        st = make_session()
        with self.assertRaisesRegex(
                AssertionError, "release without acquire"):
            st.server_state.release()

    def test_release_below_zero_rejected_under_O(self):
        # Asserts are stripped by `python -O`; release must stay loud via
        # an unconditional if/raise (Node/Browser parity). The production
        # module is reloaded under -O in a subprocess seam.
        code = (
            "import importlib.util;"
            f"spec = importlib.util.spec_from_file_location("
            f"'pybridge_O', {str(ROOT / 'bridge/py/src/pybridge.py')!r});"
            "bridge = importlib.util.module_from_spec(spec);"
            "spec.loader.exec_module(bridge);"
            "st = bridge.Session(bridge.Config());"
            "st.server_state.release();"
            "print('NO_RAISE')"
        )
        proc = subprocess.run(
            [sys.executable, "-O", "-c", code],
            capture_output=True, text=True, timeout=120)
        self.assertNotIn("NO_RAISE", proc.stdout)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("AssertionError", proc.stderr)

    def test_over_admit_rejected_by_invariant(self):
        st = make_session()
        st.server_state.active = bridge.MAX_ACTIVE_HANDLERS + 1
        with self.assertRaises(AssertionError):
            st.server_state.assert_valid()

    def test_close_single_winner(self):
        st = make_session()
        self.assertTrue(st.server_state.claim_close())
        self.assertFalse(st.server_state.claim_close())
        self.assertTrue(st.server_state.is_closing())

    def test_close_from_conn_runs_teardown_once(self):
        st = make_session()
        st.cleanup = Mock()
        st.server = Mock()
        for _ in range(2):
            srv, cli = socket.socketpair()
            try:
                bridge._close_from_conn(st, srv)
                resp = bridge.read_frame(cli)
            finally:
                cli.close()
            self.assertEqual(
                resp, {"ok": True, "closed": True, "target": "main"})
        self.assertTrue(st.server_state.is_closing())
        st.cleanup.assert_called_once_with()
        self.assertEqual(st.server_state.active, 0)

    def test_handler_release_on_completion(self):
        st = make_session()
        st.dispatch = Mock(return_value={"ok": True})
        self.assertTrue(
            st.server_state.try_admit(bridge.MAX_ACTIVE_HANDLERS))
        srv, cli = socket.socketpair()
        try:
            t = threading.Thread(target=bridge._handle_one, args=(st, srv),
                                 daemon=True)
            t.start()
            bridge.write_frame(cli, {"cmd": "threads"})
            cli.settimeout(5)
            resp = bridge.read_frame(cli)
            t.join(5)
        finally:
            cli.close()
        self.assertTrue(resp["ok"])
        with st._gate:
            self.assertEqual(st.server_state.active, 0)


if __name__ == "__main__":
    unittest.main()
