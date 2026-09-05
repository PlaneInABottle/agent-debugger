"""Deterministic regressions for DAP ordering and stopped-state handling."""
import importlib.util
import json
import os
from pathlib import Path
import socket
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import Mock

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("pybridge", ROOT / "bridge/py/src/pybridge.py")
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)


class BridgeTests(unittest.TestCase):
    def session(self):
        cfg = bridge.Config()
        return bridge.Session(cfg)

    def test_running_continue_never_resumes_historical_thread(self):
        st = self.session()
        st.thread_id = 42
        st.suspended = False
        st.dap_request = Mock(side_effect=AssertionError("unexpected resume"))
        st._resume_and_wait = Mock(return_value="waiting")
        self.assertEqual(st.cmd_continue({}, 1), "waiting")

    def test_resume_clears_active_frames_even_when_wait_times_out(self):
        st = self.session()
        st.thread_id, st.frames, st.suspended = 42, [{"id": 1}], True
        st.publish_state = Mock()
        st.pump = Mock(side_effect=bridge.BridgeErr("timeout"))
        with self.assertRaises(bridge.BridgeErr):
            st._resume_and_wait(1)
        self.assertIsNone(st.thread_id)
        self.assertEqual(st.frames, [])
        self.assertFalse(st.suspended)

    def test_stash_suffix_survives_first_stop_and_nested_request(self):
        st = self.session()
        stop, output, nested = ({"event": s} for s in ["stopped", "output", "nested"])
        st.dap = SimpleNamespace(stash=[stop, output])
        def handle(msg):
            self.assertEqual(msg, stop)
            st.dap.stash.append(nested)
            return "stopped"
        st._handle_pumped = handle
        self.assertEqual(st.pump(1), "stopped")
        self.assertEqual(st.dap.stash, [output, nested])

    def test_idle_batch_drains_output_before_stop(self):
        st = self.session()
        left, right = socket.socketpair()
        self.addCleanup(left.close)
        self.addCleanup(right.close)
        st.dap = bridge.DapConn(left)
        st.dap.stash = [{"event": "output"}] * 100 + [{"event": "stopped"}]
        seen = []
        def handle(msg):
            seen.append(msg["event"])
            return "stopped" if msg["event"] == "stopped" else None
        st._handle_pumped = handle
        old_timeout = left.gettimeout()
        bridge.idle_pump(st)
        self.assertEqual(len(seen), 101)
        self.assertEqual(left.gettimeout(), old_timeout)

    def test_malformed_event_body_is_contained(self):
        st = self.session()
        for body in [None, "text", [], 3]:
            self.assertIsNone(st._handle_pumped({"type": "event", "event": "stopped", "body": body}))

    def test_uninspectable_stop_remains_suspended_without_auto_resume(self):
        st = self.session()
        st.publish_state = Mock()
        st.refresh_frames = Mock(side_effect=bridge.BridgeErr("stack unavailable"))
        st.dap_request = Mock()
        with self.assertRaises(bridge.BridgeErr):
            st._handle_pumped({"type": "event", "event": "stopped", "body": {"reason": "breakpoint", "threadId": 7}})
        self.assertTrue(st.suspended)
        self.assertEqual(st.thread_id, 7)
        st.dap_request.assert_not_called()
        st.publish_state.assert_called_with(True)

    def test_multiline_log_cap_counts_physical_lines(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.session()
            st.cfg.dir = tmp
            st.log_count = bridge.MAX_LOG_LINES - 2
            st.append_log("one\ntwo\nthree\n")
            self.assertEqual((Path(tmp) / "logs.jsonl").read_text(), "one\ntwo\n")
            self.assertEqual(st.log_count, bridge.MAX_LOG_LINES)

    def test_bad_request_frames_rejected(self):
        for data in [b"Content-Length: 1048577\r\n\r\n", b"Content-Length: bad\r\n\r\n", b"x" * 8192,
                     b"Content-Length: 4\r\n\r\nnull"]:
            left, right = socket.socketpair()
            try:
                right.sendall(data)
                with self.assertRaises(bridge.BridgeErr):
                    bridge.read_frame(left)
            finally:
                left.close()
                right.close()

    def test_malformed_timeout_is_bounded_bridge_err(self):
        st = self.session()
        st.cfg.timeout = 20.0
        for bad in ["fast", None, [], {}, float("nan"), float("inf"), 0, -1, 3601]:
            with self.assertRaises(bridge.BridgeErr) as cm:
                st.dispatch({"cmd": "threads", "timeout": bad})
            self.assertIn("timeout must be between 0 and 3600", str(cm.exception))
        # Numeric strings still classify as timeouts (then fail on cmd).
        with self.assertRaises(bridge.BridgeErr) as cm:
            st.dispatch({"cmd": "nope", "timeout": "5"})
        self.assertIn("unknown cmd", str(cm.exception))

    def test_stop_timeout_is_typed_bridge_err(self):
        self.assertTrue(issubclass(bridge.StopTimeout, bridge.BridgeErr))

    def test_first_stop_wait_timeout_raises_stop_timeout(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.session()
            st.cfg.dir = tmp
            st._nonce = bridge.write_owner(tmp)
            st.dap = SimpleNamespace(stash=[])
            with self.assertRaises(bridge.StopTimeout):
                st.pump(0)
        with tempfile.TemporaryDirectory() as tmp:
            st = self.session()
            st.cfg.dir = tmp
            st._nonce = bridge.write_owner(tmp)
            st.dap = SimpleNamespace(stash=[])
            with self.assertRaises(bridge.StopTimeout):
                st.pump(0)

    def add_session(self, tmp, breaks=()):
        st = self.session()
        st.cfg.dir = tmp
        st.cfg.breaks = list(breaks)
        st.dap_request = Mock()
        return st

    def test_breaks_add_rejects_non_line_specs_without_dap(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.add_session(tmp)
            for raw in ["method:foo", "exc", "exc:ValueError", "nope", "a.py:xx", ""]:
                with self.assertRaises(bridge.BridgeErr):
                    st.cmd_breaks_add({"breaks": [raw]})
            st.dap_request.assert_not_called()
            self.assertEqual(st.cfg.breaks, [])

    def test_breaks_add_conflict_is_atomic(self):
        with tempfile.TemporaryDirectory() as tmp:
            # Canonical seed: the resolver stores realpath, so pre-armed
            # entries must match that form (matters on symlinked /tmp).
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("x = 1\n")
            armed = (path, 5, None)
            st = self.add_session(tmp, breaks=[armed])
            st.stop_states = [{"spec": "a.py:5", "kind": "break", "state": "verified", "hits": 0}]
            st._hitkeys = [("break", path, 5)]
            with self.assertRaises(bridge.BridgeErr) as cm:
                st.cmd_breaks_add({"breaks": [f"{path}:9", f"{path}:5|x > 1"]})
            self.assertIn("conflicting", str(cm.exception))
            st.dap_request.assert_not_called()
            self.assertEqual(st.cfg.breaks, [armed])
            self.assertEqual(len(st.stop_states), 1)

    def test_breaks_add_duplicate_is_idempotent(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("x = 1\n")
            st = self.add_session(tmp, breaks=[(path, 5, None)])
            st.stop_states = [{"spec": "a.py:5", "kind": "break", "state": "verified", "hits": 0}]
            st._hitkeys = [("break", path, 5)]
            resp = st.cmd_breaks_add({"breaks": [f"{path}:5", f"{path}:5"]})
            self.assertEqual(resp, {"ok": True, "added": [], "stops": st.stop_states})
            st.dap_request.assert_not_called()

    def test_breaks_add_success_confirms_subset(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("x = 1\n")
            st = self.add_session(tmp)
            st.dap_request.return_value = {"breakpoints": [{"verified": True}]}
            raw = f"{path}:5"
            resp = st.cmd_breaks_add({"breaks": [raw]})
            self.assertTrue(resp["ok"])
            self.assertEqual(len(resp["added"]), 1)
            self.assertEqual(resp["added"][0]["raw"], raw)
            self.assertEqual(resp["added"][0]["state"], "verified")
            self.assertEqual(st.cfg.breaks, [(path, 5, None)])
            self.assertEqual(len(st.stop_states), 1)
            self.assertEqual(st._hitkeys, [("break", path, 5)])
            # DAP saw the merged file list with a 5s bound.
            _, kwargs = st.dap_request.call_args
            self.assertEqual(kwargs.get("timeout"), 5)

    def test_breaks_add_partial_ok_with_warning(self):
        with tempfile.TemporaryDirectory() as tmp:
            a = os.path.realpath(str(Path(tmp) / "a.py"))
            b = os.path.realpath(str(Path(tmp) / "b.py"))
            Path(a).write_text("x = 1\n")
            Path(b).write_text("y = 2\n")
            st = self.add_session(tmp)

            def fake(command, args=None, timeout=30):
                if args["source"]["path"] == a:
                    return {"breakpoints": [{"verified": False, "message": "pending"}]}
                raise bridge.BridgeErr("adapter exploded")
            st.dap_request.side_effect = fake
            resp = st.cmd_breaks_add({"breaks": [f"{a}:5", f"{b}:6"]})
            self.assertTrue(resp["ok"])
            self.assertEqual([e["raw"] for e in resp["added"]], [f"{a}:5"])
            self.assertIn("warning", resp)
            self.assertEqual(st.cfg.breaks, [(a, 5, None)])

    def test_breaks_add_total_failure_is_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            a = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(a).write_text("x = 1\n")
            st = self.add_session(tmp)
            st.dap_request.side_effect = bridge.BridgeErr("adapter exploded")
            with self.assertRaises(bridge.BridgeErr):
                st.cmd_breaks_add({"breaks": [f"{a}:5"]})
            self.assertEqual(st.cfg.breaks, [])
            self.assertEqual(st.stop_states, [])

    def test_resolve_cwd_existing_wins_over_src(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "root"
            (root / "deep").mkdir(parents=True)
            (root / "deep" / "probe.py").write_text("x = 1\n")
            cwd_file = Path(tmp) / "probe.py"
            cwd_file.write_text("y = 2\n")
            old = os.getcwd()
            os.chdir(tmp)
            self.addCleanup(os.chdir, old)
            got = bridge.resolve_source_path("probe.py", [str(root)])
            self.assertEqual(got, os.path.realpath(str(cwd_file)))

    def test_resolve_missing_basename_no_src_fails_fast(self):
        with tempfile.TemporaryDirectory() as tmp:
            old = os.getcwd()
            os.chdir(tmp)
            self.addCleanup(os.chdir, old)
            with self.assertRaises(bridge.Usage) as cm:
                bridge.resolve_source_path("ghost.py", [])
            msg = str(cm.exception)
            self.assertIn("ghost.py", msg)
            self.assertIn(os.path.abspath("ghost.py"), msg)
            self.assertIn("no --src roots given", msg)

    def test_resolve_basename_unique_under_src(self):
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp) / "root" / "a" / "b" / "only.py"
            target.parent.mkdir(parents=True)
            target.write_text("x = 1\n")
            old = os.getcwd()
            os.chdir(tmp)
            self.addCleanup(os.chdir, old)
            got = bridge.resolve_source_path("only.py", [str(Path(tmp) / "root")])
            self.assertEqual(got, os.path.realpath(str(target)))

    def test_resolve_basename_ambiguous_under_src(self):
        with tempfile.TemporaryDirectory() as tmp:
            for sub in ("a", "b"):
                d = Path(tmp) / "root" / sub
                d.mkdir(parents=True)
                (d / "dup.py").write_text("x = 1\n")
            old = os.getcwd()
            os.chdir(tmp)
            self.addCleanup(os.chdir, old)
            with self.assertRaises(bridge.Usage) as cm:
                bridge.resolve_source_path("dup.py", [str(Path(tmp) / "root")])
            msg = str(cm.exception)
            self.assertIn("ambiguous", msg)
            self.assertIn("dup.py", msg)
            self.assertIn(os.path.join("a", "dup.py"), msg)
            self.assertIn(os.path.join("b", "dup.py"), msg)

    def test_resolve_nested_relative_under_src(self):
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp) / "root" / "tests" / "wf" / "nested.py"
            target.parent.mkdir(parents=True)
            target.write_text("x = 1\n")
            old = os.getcwd()
            os.chdir(tmp)
            self.addCleanup(os.chdir, old)
            got = bridge.resolve_source_path(
                os.path.join("tests", "wf", "nested.py"), [str(Path(tmp) / "root")])
            self.assertEqual(got, os.path.realpath(str(target)))

    def test_resolve_absolute_missing_fails_fast(self):
        with tempfile.TemporaryDirectory() as tmp:
            missing = str(Path(tmp) / "nope.py")
            with self.assertRaises(bridge.Usage) as cm:
                bridge.resolve_source_path(missing, [tmp])
            self.assertIn(missing, str(cm.exception))

    def test_resolve_symlink_canonical_dedup(self):
        with tempfile.TemporaryDirectory() as tmp:
            real = Path(tmp) / "root" / "a"
            real.mkdir(parents=True)
            (real / "same.py").write_text("x = 1\n")
            # Same file reachable twice: direct + symlinked sibling dir.
            # The symlinked dir is skipped, the real one resolves alone.
            try:
                os.symlink(str(real), str(Path(tmp) / "root" / "blink"))
            except OSError:
                self.skipTest("symlinks unavailable")
            old = os.getcwd()
            os.chdir(tmp)
            self.addCleanup(os.chdir, old)
            got = bridge.resolve_source_path("same.py", [str(Path(tmp) / "root")])
            self.assertEqual(got, os.path.realpath(str(real / "same.py")))

    def test_logpoint_uses_same_resolver(self):
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp) / "root" / "lp.py"
            target.parent.mkdir(parents=True)
            target.write_text("x = 1\n")
            old = os.getcwd()
            os.chdir(tmp)
            self.addCleanup(os.chdir, old)
            cfg = bridge.Config()
            cfg.src_dirs = [str(Path(tmp) / "root")]
            bridge.parse_logpoint("lp.py:3:val={x}", cfg)
            self.assertEqual(cfg.logpoints,
                             [(os.path.realpath(str(target)), 3, "val={x}")])
            with self.assertRaises(bridge.Usage):
                bridge.parse_logpoint("ghost.py:3:val={x}", cfg)

    def test_parse_args_resolves_break_before_src_flag(self):
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp) / "root" / "deep" / "early.py"
            target.parent.mkdir(parents=True)
            target.write_text("x = 1\n")
            prog = Path(tmp) / "app.py"
            prog.write_text("print('hi')\n")
            old = os.getcwd()
            os.chdir(tmp)
            self.addCleanup(os.chdir, old)
            cfg = bridge.parse_args(["session", "--kind", "launch",
                                     "--dir", str(Path(tmp) / "sess"),
                                     "--program", str(prog),
                                     "--break", "early.py:1",
                                     "--src", str(Path(tmp) / "root")])
            self.assertEqual(cfg.breaks,
                             [(os.path.realpath(str(target)), 1, None)])

    def test_breaks_add_uses_live_src_dirs(self):
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp) / "root" / "sub" / "live.py"
            target.parent.mkdir(parents=True)
            target.write_text("x = 1\n")
            old = os.getcwd()
            os.chdir(tmp)
            self.addCleanup(os.chdir, old)
            st = self.add_session(tmp)
            st.cfg.src_dirs = [str(Path(tmp) / "root")]
            st.dap_request.return_value = {"breakpoints": [{"verified": True}]}
            resp = st.cmd_breaks_add({"breaks": ["live.py:2"]})
            self.assertTrue(resp["ok"])
            self.assertEqual(len(resp["added"]), 1)
            self.assertEqual(st.cfg.breaks,
                             [(os.path.realpath(str(target)), 2, None)])
            # Missing basename fails before any DAP traffic.
            st.dap_request.reset_mock()
            with self.assertRaises(bridge.BridgeErr) as cm:
                st.cmd_breaks_add({"breaks": ["ghost.py:2"]})
            self.assertIn("ghost.py", str(cm.exception))
            st.dap_request.assert_not_called()

    def test_unresolved_summary_names_specs_details_hint(self):
        st = self.session()
        st.stop_states = [
            {"spec": "probe.py:3", "kind": "break", "state": "pending",
             "hits": 0, "detail": "pending"},
            {"spec": "probe.py:4", "kind": "logpoint", "state": "pending",
             "hits": None, "detail": "val={x} (pending)"},
            {"spec": "method:foo", "kind": "method", "state": "armed", "hits": 0},
        ]
        summary = st.unresolved_summary()
        self.assertIn("probe.py:3", summary)
        self.assertIn("probe.py:4", summary)
        self.assertIn("val={x}", summary)
        self.assertIn("executable", summary)
        self.assertNotIn("method:foo", summary)
        st.stop_states = [{"spec": "a.py:1", "kind": "break",
                           "state": "verified", "hits": 0}]
        self.assertEqual(st.unresolved_summary(), "")

    def test_partial_dap_frame_is_preserved(self):
        left, right = socket.socketpair()
        self.addCleanup(left.close)
        self.addCleanup(right.close)
        dap = bridge.DapConn(left)
        left.settimeout(0.01)
        message = {"type": "event", "event": "output", "body": {"output": "ok"}}
        body = json.dumps(message).encode()
        right.sendall(f"Content-Length: {len(body)}\r\n\r\n".encode() + body[:4])
        with self.assertRaises(socket.timeout):
            dap._read_msg()
        right.sendall(body[4:])
        self.assertEqual(dap._read_msg(), message)


if __name__ == "__main__":
    unittest.main()
