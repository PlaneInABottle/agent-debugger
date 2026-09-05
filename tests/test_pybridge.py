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
            st.append_log("one\ntwo\nthree\n")
            self.assertEqual((Path(tmp) / "logs.jsonl").read_text(), "one\ntwo\nthree\n")
            self.assertEqual(st.log_count, 3)
            self.assertEqual(st.log_dropped, 0)

    def test_log_ring_keeps_latest_2000(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.session()
            st.cfg.dir = tmp
            for n in range(bridge.MAX_LOG_LINES + 5):
                st.append_log(f"line-{n}")
            lines = (Path(tmp) / "logs.jsonl").read_text().splitlines()
            self.assertEqual(len(lines), bridge.MAX_LOG_LINES)
            self.assertNotIn("line-0", lines)
            self.assertNotIn("line-4", lines)
            self.assertEqual(lines[0], "line-5")
            self.assertEqual(lines[-1], f"line-{bridge.MAX_LOG_LINES + 4}")
            self.assertEqual(st.log_count, bridge.MAX_LOG_LINES)
            self.assertEqual(st.log_dropped, 5)
            st.exited = True  # skip live drain: assert the envelope only
            resp = st.cmd_logs({"tail": 50})
            self.assertEqual(resp["total"], bridge.MAX_LOG_LINES)
            self.assertEqual(resp["dropped"], 5)
            self.assertTrue(resp["truncated"])
            self.assertEqual(resp["lines"][-1], f"line-{bridge.MAX_LOG_LINES + 4}")

    def test_log_ring_trim_is_atomic_and_bounded(self):
        # A trim burst bigger than the ring keeps exactly the latest MAX.
        with tempfile.TemporaryDirectory() as tmp:
            st = self.session()
            st.cfg.dir = tmp
            st._append_log_parts([f"bulk-{n}" for n in range(bridge.MAX_LOG_LINES + 100)])
            lines = (Path(tmp) / "logs.jsonl").read_text().splitlines()
            self.assertEqual(len(lines), bridge.MAX_LOG_LINES)
            self.assertEqual(lines[0], "bulk-100")
            self.assertEqual(st.log_dropped, 100)

    def test_atomic_write_never_leaves_partial_or_temp(self):
        import threading
        with tempfile.TemporaryDirectory() as tmp:
            target = os.path.join(tmp, "session.json")
            stop = threading.Event()
            errors = []

            def writer(n):
                for i in range(200):
                    bridge.write_file(target, json.dumps({"n": n * 1000 + i}))

            def reader():
                while not stop.is_set():
                    try:
                        raw = Path(target).read_text()
                    except OSError:
                        continue
                    try:
                        json.loads(raw)
                    except ValueError as e:
                        errors.append(f"partial read: {e}: {raw[:80]!r}")

            threads = [threading.Thread(target=writer, args=(n,)) for n in range(3)]
            watcher = threading.Thread(target=reader)
            for t in threads:
                t.start()
            watcher.start()
            for t in threads:
                t.join()
            stop.set()
            watcher.join()
            self.assertEqual(errors, [])
            json.loads(Path(target).read_text())  # final state parses
            leftovers = [p for p in os.listdir(tmp) if p.startswith(".tmp-")]
            self.assertEqual(leftovers, [])

    def test_format_unexpected_is_sanitized_and_capped(self):
        try:
            raise RuntimeError("x" * 5000)
        except RuntimeError as e:
            msg = bridge.format_unexpected(e)
        self.assertTrue(msg.startswith("internal: RuntimeError:"))
        self.assertLessEqual(len(msg), bridge.MAX_ERROR_CHARS + len("internal: "))
        # No env dump: payload is the exception + our frames only.
        self.assertNotIn("HOME", msg)
        self.assertNotIn("PATH=", msg)

    def test_unexpected_setup_crash_writes_error_json(self):
        # Forced pre-ready crash: start_adapter blows up with a plain
        # RuntimeError (not Usage/BridgeErr). The unexpected path must still
        # clean up, write a useful error.json, and exit nonzero — never die
        # silent. The session name stays reusable (plain files, no lock).
        with tempfile.TemporaryDirectory() as tmp:
            sess = os.path.join(tmp, "sess")
            prog = Path(tmp) / "app.py"
            prog.write_text("print('hi')\n")
            orig = bridge.Session.start_adapter

            def boom(self):
                raise RuntimeError("adapter exploded unexpectedly")

            bridge.Session.start_adapter = boom
            try:
                with self.assertRaises(SystemExit) as cm:
                    bridge.main(["session", "--kind", "launch", "--dir", sess,
                                 "--program", str(prog)])
            finally:
                bridge.Session.start_adapter = orig
            self.assertEqual(cm.exception.code, 1)
            payload = json.loads((Path(sess) / "error.json").read_text())
            self.assertTrue(payload["error"].startswith("internal: RuntimeError:"))
            self.assertIn("adapter exploded unexpectedly", payload["error"])
            self.assertLessEqual(len(payload["error"]), bridge.MAX_ERROR_CHARS + 32)
            self.assertFalse((Path(sess) / "session.json").exists())

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
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            armed = (path, 5, None)
            st = self.add_session(tmp, breaks=[armed])
            st.stop_states = [{"spec": "a.py:5", "kind": "break", "state": "verified", "hits": 0}]
            st._hitkeys = [("break", path, 5, 5)]
            with self.assertRaises(bridge.BridgeErr) as cm:
                st.cmd_breaks_add({"breaks": [f"{path}:9", f"{path}:5|x > 1"]})
            self.assertIn("conflicting", str(cm.exception))
            st.dap_request.assert_not_called()
            self.assertEqual(st.cfg.breaks, [armed])
            self.assertEqual(len(st.stop_states), 1)

    def test_breaks_add_duplicate_is_idempotent(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = self.add_session(tmp, breaks=[(path, 5, None)])
            st.stop_states = [{"spec": "a.py:5", "kind": "break", "state": "verified", "hits": 0}]
            st._hitkeys = [("break", path, 5, 5)]
            resp = st.cmd_breaks_add({"breaks": [f"{path}:5", f"{path}:5"]})
            self.assertEqual(resp, {"ok": True, "added": [], "stops": st.stop_states})
            st.dap_request.assert_not_called()

    def test_breaks_add_success_confirms_subset(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
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
            self.assertEqual(st._hitkeys, [("break", path, 5, 5)])
            # DAP saw the merged file list with a 5s bound.
            _, kwargs = st.dap_request.call_args
            self.assertEqual(kwargs.get("timeout"), 5)

    def test_breaks_add_partial_ok_with_warning(self):
        with tempfile.TemporaryDirectory() as tmp:
            a = os.path.realpath(str(Path(tmp) / "a.py"))
            b = os.path.realpath(str(Path(tmp) / "b.py"))
            Path(a).write_text("".join(f"line {n}\n" for n in range(12)))
            Path(b).write_text("".join(f"line {n}\n" for n in range(12)))
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
            Path(a).write_text("".join(f"line {n}\n" for n in range(12)))
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
            target.write_text("".join(f"line {n}\n" for n in range(6)))
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
            target.write_text("".join(f"line {n}\n" for n in range(12)))
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
            {"spec": "method:foo", "kind": "method", "state": "verified", "hits": 0},
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

    def test_line_range_rejects_zero_negative_and_past_eof(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "app.py"))
            Path(path).write_text("a = 1\n# note\nb = 2\nprint(b)\n")
            old = os.getcwd()
            os.chdir(tmp)
            self.addCleanup(os.chdir, old)
            for bad in (f"{path}:0", f"{path}:-3", f"{path}:5",
                        f"{path}:99999"):
                cfg = bridge.Config()
                with self.assertRaises(bridge.Usage) as cm:
                    bridge.parse_break(bad, cfg)
                msg = str(cm.exception)
                self.assertIn("no such line", msg)
                self.assertIn(bad, msg)
                self.assertIn("4 lines", msg)
                self.assertEqual(cfg.breaks, [])
            cfg = bridge.Config()
            bridge.parse_break(f"{path}:4", cfg)
            self.assertEqual(cfg.breaks, [(path, 4, None)])
            for bad in (f"{path}:0:x={1}", f"{path}:5:x={1}"):
                cfg = bridge.Config()
                with self.assertRaises(bridge.Usage) as cm:
                    bridge.parse_logpoint(bad, cfg)
                self.assertIn("no such line", str(cm.exception))
                self.assertEqual(cfg.logpoints, [])
            # No trailing newline still counts the last partial line.
            bare = os.path.realpath(str(Path(tmp) / "bare.py"))
            Path(bare).write_text("a = 1\nb = 2")
            cfg = bridge.Config()
            bridge.parse_break(f"{bare}:2", cfg)
            self.assertEqual(cfg.breaks, [(bare, 2, None)])
            with self.assertRaises(bridge.Usage):
                bridge.parse_break(f"{bare}:3", cfg)

    def test_breaks_add_invalid_line_fails_before_dap(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "app.py"))
            Path(path).write_text("a = 1\nb = 2\nprint(b)\n")
            st = self.add_session(tmp)
            with self.assertRaises(bridge.BridgeErr) as cm:
                st.cmd_breaks_add({"breaks": [f"{path}:999"]})
            self.assertIn("no such line", str(cm.exception))
            st.dap_request.assert_not_called()
            self.assertEqual(st.cfg.breaks, [])

    def arm_with(self, tmp, breaks=(), logpoints=(), answers=()):
        st = self.session()
        st.cfg.dir = tmp
        st.cfg.breaks = list(breaks)
        st.cfg.logpoints = list(logpoints)
        st.dap_request = Mock(return_value={"breakpoints": list(answers)})
        st.arm_breakpoints()
        return st

    def test_arm_slid_break_reports_bound_line(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "app.py"))
            Path(path).write_text("import time\n# comment\nvalue = 1\n")
            st = self.arm_with(tmp, breaks=[(path, 2, None)],
                               answers=[{"verified": True, "line": 1}])
            (rec,) = st.stop_states
            self.assertEqual(rec["state"], "slid")
            self.assertEqual(rec["detail"], "slid to line 1")
            self.assertTrue(rec["spec"].endswith(":2"))
            self.assertEqual(rec["hits"], 0)
            self.assertEqual(st._hitkeys, [("break", path, 2, 1)])

    def test_arm_exact_verified_and_missing_line(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "app.py"))
            Path(path).write_text("a = 1\nb = 2\n")
            st = self.arm_with(tmp, breaks=[(path, 1, None)],
                               answers=[{"verified": True, "line": 1}])
            self.assertEqual(st.stop_states[0]["state"], "verified")
            self.assertNotIn("detail", st.stop_states[0])
            self.assertEqual(st._hitkeys, [("break", path, 1, 1)])
            # verified:true without a numeric line stays verified at request.
            st = self.arm_with(tmp, breaks=[(path, 2, None)],
                               answers=[{"verified": True}])
            self.assertEqual(st.stop_states[0]["state"], "verified")
            self.assertEqual(st._hitkeys, [("break", path, 2, 2)])

    def test_arm_pending_and_missing_answers(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "app.py"))
            Path(path).write_text("a = 1\nb = 2\n")
            st = self.arm_with(
                tmp, breaks=[(path, 1, None), (path, 2, None)],
                answers=[{"verified": False, "message": "excluded"}, {}])
            self.assertEqual(st.stop_states[0]["state"], "pending")
            self.assertIn("excluded", st.stop_states[0]["detail"])
            self.assertEqual(st.stop_states[1]["state"], "pending")
            self.assertEqual(st._hitkeys,
                             [("break", path, 1, 1), ("break", path, 2, 2)])
            # Short adapter answer: the missing entry reports pending.
            st = self.arm_with(tmp, breaks=[(path, 1, None), (path, 2, None)],
                               answers=[{"verified": True, "line": 1}])
            self.assertEqual(st.stop_states[0]["state"], "verified")
            self.assertEqual(st.stop_states[1]["state"], "pending")

    def test_arm_slid_logpoint_preserves_template(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "app.py"))
            Path(path).write_text("import time\n# comment\nvalue = 1\n")
            st = self.arm_with(tmp, logpoints=[(path, 2, "val={value}")],
                               answers=[{"verified": True, "line": 1}])
            (rec,) = st.stop_states
            self.assertEqual(rec["state"], "slid")
            self.assertIn("val={value}", rec["detail"])
            self.assertIn("slid to line 1", rec["detail"])
            self.assertIsNone(rec["hits"])
            self.assertEqual(st._hitkeys, [("logpoint", path, 2, 1)])

    def test_count_hits_matches_bound_not_requested(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "app.py"))
            Path(path).write_text("import time\n# comment\nvalue = 1\n")
            st = self.session()
            st.cfg.dir = tmp
            st.stop_states = [
                {"spec": "app.py:2", "kind": "break", "state": "slid",
                 "detail": "slid to line 1", "hits": 0},
                {"spec": "app.py:3", "kind": "break", "state": "verified",
                 "hits": 0},
            ]
            st._hitkeys = [("break", path, 2, 1), ("break", path, 3, 3)]
            st.frames = [{"source": {"path": path}, "name": "<module>",
                          "line": 1}]
            st.count_hits("breakpoint")
            self.assertEqual(st.stop_states[0]["hits"], 1)
            self.assertEqual(st.stop_states[1]["hits"], 0)
            # A landing on the requested (but unbound) line counts nothing.
            st.frames = [{"source": {"path": path}, "name": "<module>",
                          "line": 2}]
            st.count_hits("breakpoint")
            self.assertEqual(st.stop_states[0]["hits"], 1)

    def test_unresolved_summary_slid_filtering(self):
        st = self.session()
        st.stop_states = [
            {"spec": "a.py:1", "kind": "break", "state": "pending",
             "hits": 0, "detail": "pending"},
            {"spec": "a.py:2", "kind": "break", "state": "slid",
             "hits": 0, "detail": "slid to line 1"},
            {"spec": "a.py:3", "kind": "logpoint", "state": "slid",
             "hits": None, "detail": "val={x} (slid to line 4)"},
            {"spec": "a.py:4", "kind": "break", "state": "slid",
             "hits": 2, "detail": "slid to line 5"},
            {"spec": "a.py:6", "kind": "break", "state": "verified",
             "hits": 0},
        ]
        summary = st.unresolved_summary()
        self.assertIn("a.py:1", summary)
        self.assertIn("a.py:2", summary)
        self.assertIn("slid to line 1", summary)
        self.assertIn("a.py:3", summary)
        self.assertNotIn("a.py:4", summary)
        self.assertNotIn("a.py:6", summary)

    def test_breaks_add_refresh_records_slide(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "app.py"))
            Path(path).write_text("import time\n# comment\nvalue = 1\n")
            st = self.add_session(tmp)
            st.dap_request.return_value = {
                "breakpoints": [{"verified": True, "line": 1}]}
            resp = st.cmd_breaks_add({"breaks": [f"{path}:2"]})
            self.assertEqual(resp["added"][0]["state"], "slid")
            self.assertEqual(resp["added"][0]["detail"], "slid to line 1")
            self.assertEqual(st.stop_states[0]["state"], "slid")
            self.assertEqual(st._hitkeys, [("break", path, 2, 1)])

    def test_arm_methods_single_call_with_truthful_mapping(self):
        st = self.session()
        st.cfg.methods = ["fa", "fb"]
        calls = []

        def fake(command, args=None, timeout=30):
            calls.append((command, args))
            return {"breakpoints": [{"verified": True},
                                    {"verified": False,
                                     "message": "no such function"}]}
        st.dap_request = fake
        st.arm_breakpoints()
        fb_calls = [c for c in calls if c[0] == "setFunctionBreakpoints"]
        # One replacing call carrying both specs (never one call per func).
        self.assertEqual(len(fb_calls), 1)
        self.assertEqual(fb_calls[0][1],
                         {"breakpoints": [{"name": "fa"}, {"name": "fb"}]})
        self.assertEqual([r["spec"] for r in st.stop_states],
                         ["method:fa", "method:fb"])
        self.assertEqual(st.stop_states[0]["state"], "verified")
        self.assertNotIn("detail", st.stop_states[0])
        self.assertEqual(st.stop_states[1]["state"], "pending")
        self.assertEqual(st.stop_states[1]["detail"], "no such function")
        self.assertEqual(st._hitkeys, [("method", "fa"), ("method", "fb")])

    def test_arm_methods_short_answer_stays_pending(self):
        st = self.session()
        st.cfg.methods = ["fa", "fb"]
        st.dap_request = Mock(
            return_value={"breakpoints": [{"verified": True}]})
        st.arm_breakpoints()
        self.assertEqual(st.dap_request.call_count, 1)
        self.assertEqual(st.stop_states[0]["state"], "verified")
        self.assertEqual(st.stop_states[1]["state"], "pending")
        self.assertEqual(len(st.stop_states), 2)

    def test_startup_exact_duplicate_break_is_idempotent(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "app.py"))
            Path(path).write_text("a = 1\nb = 2\nprint(b)\n")
            prog = str(Path(tmp) / "app.py")
            cfg = bridge.parse_args(
                ["session", "--kind", "launch", "--dir", tmp,
                 "--program", prog,
                 "--break", f"{path}:2", "--break", f"{path}:2"])
            self.assertEqual(cfg.breaks, [(path, 2, None)])
            st = bridge.Session(cfg)
            st.dap_request = Mock(
                return_value={"breakpoints": [{"verified": True,
                                               "line": 2}]})
            st.arm_breakpoints()
            self.assertEqual(st.dap_request.call_count, 1)
            self.assertEqual(len(st.stop_states), 1)
            self.assertEqual(st.stop_states[0]["state"], "verified")

    def test_startup_conflicting_cond_fails_before_dap(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "app.py"))
            Path(path).write_text("a = 1\nb = 2\nprint(b)\n")
            prog = str(Path(tmp) / "app.py")
            # parse_args runs before adapter launch in main(): a Usage here
            # means no DAP connection and no target process can exist yet.
            with self.assertRaises(bridge.Usage) as cm:
                bridge.parse_args(
                    ["session", "--kind", "launch", "--dir", tmp,
                     "--program", prog,
                     "--break", f"{path}:2",
                     "--break", f"{path}:2|x > 1"])
            self.assertIn("conflicting", str(cm.exception))
            # Zero DAP calls by construction: no Session/DAP exists when
            # parse_args raises. The live counterpart (zero-DAP atomicity)
            # is covered by test_breaks_add_conflict_is_atomic.

    def test_simultaneous_thread_stop_keeps_first_park(self):
        # P5: near-simultaneous stops from two threads are both real; the
        # first park stays put while the co-stop still counts its hit.
        with tempfile.TemporaryDirectory() as tmp:
            a = os.path.realpath(str(Path(tmp) / "a.py"))
            b = os.path.realpath(str(Path(tmp) / "b.py"))
            Path(a).write_text("".join(f"line {n}\n" for n in range(12)))
            Path(b).write_text("".join(f"line {n}\n" for n in range(12)))
            st = self.session()
            st.cfg.dir = tmp
            st.cfg.breaks = [(a, 5, None), (b, 9, None)]
            st.dap_request = Mock(
                return_value={"breakpoints": [{"verified": True}]})
            st.arm_breakpoints()
            self.assertEqual(len(st.stop_states), 2)
            by_thread = {1: (a, 5), 2: (b, 9)}

            def fake_refresh(levels=64, timeout=30):
                path, line = by_thread[st.thread_id]
                st.frames = [{"id": st.thread_id, "name": "w",
                              "source": {"path": path}, "line": line}]
            st.refresh_frames = fake_refresh
            st._co_stop_frame = lambda tid: (
                {"id": tid, "name": "w",
                 "source": {"path": by_thread[tid][0]},
                 "line": by_thread[tid][1]})

            def ev(tid):
                return {"type": "event", "event": "stopped",
                        "body": {"reason": "breakpoint", "threadId": tid}}
            self.assertEqual(st._handle_pumped(ev(1)), "stopped")
            self.assertEqual(st.thread_id, 1)
            parked_frames = list(st.frames)
            self.assertIsNone(st._handle_pumped(ev(2)))
            self.assertEqual(st.thread_id, 1)
            self.assertEqual(st.frames, parked_frames)
            self.assertEqual([r["hits"] for r in st.stop_states], [1, 1])

    def test_duplicate_costop_hit_once(self):
        with tempfile.TemporaryDirectory() as tmp:
            a = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(a).write_text("".join(f"line {n}\n" for n in range(12)))
            st = self.session()
            st.cfg.dir = tmp
            st.cfg.breaks = [(a, 5, None)]
            st.dap_request = Mock(
                return_value={"breakpoints": [{"verified": True}]})
            st.arm_breakpoints()
            st.refresh_frames = Mock(
                side_effect=lambda levels=64, timeout=30: st.frames.append(
                    {"id": 1, "name": "w",
                     "source": {"path": a}, "line": 5}))
            st._co_stop_frame = Mock(return_value={
                "id": 2, "name": "w", "source": {"path": a}, "line": 5})
            park = {"type": "event", "event": "stopped",
                    "body": {"reason": "breakpoint", "threadId": 1}}
            co = {"type": "event", "event": "stopped",
                  "body": {"reason": "breakpoint", "threadId": 2}}
            self.assertEqual(st._handle_pumped(park), "stopped")
            self.assertIsNone(st._handle_pumped(co))
            self.assertIsNone(st._handle_pumped(co))
            self.assertEqual(st.thread_id, 1)
            # Park hit + exactly one co-stop hit (redelivery suppressed).
            self.assertEqual(st.stop_states[0]["hits"], 2)

    def test_resume_discards_stashed_costops_only_when_parked(self):
        out = {"type": "event", "event": "output",
               "body": {"output": "hi"}}
        stop = {"type": "event", "event": "stopped",
                "body": {"reason": "breakpoint", "threadId": 2}}
        # Parked resume: pre-resume stops are stale, output survives.
        st = self.session()
        st.suspended = True
        st.dap = SimpleNamespace(stash=[stop, out])
        st.publish_state = Mock()
        st.pump = Mock(side_effect=bridge.StopTimeout("nope"))
        with self.assertRaises(bridge.StopTimeout):
            st._resume_and_wait(1)
        self.assertEqual(st.dap.stash, [out])
        self.assertTrue(st._awaiting_continued)
        self.assertEqual(st._suspects, [])
        # Running continue issues no resume: stash and barrier untouched.
        st = self.session()
        st.suspended = False
        st.dap = SimpleNamespace(stash=[stop])
        st.publish_state = Mock()
        st.pump = Mock(side_effect=bridge.StopTimeout("nope"))
        with self.assertRaises(bridge.StopTimeout):
            st._resume_and_wait(1)
        self.assertEqual(st.dap.stash, [stop])
        self.assertFalse(st._awaiting_continued)

    def test_stale_before_continued_ignored_then_exit_truth(self):
        st = self.session()
        st.publish_state = Mock()
        st._awaiting_continued = True
        st._co_stop_frame = Mock(return_value=None)
        stop = {"type": "event", "event": "stopped",
                "body": {"reason": "breakpoint", "threadId": 2}}
        self.assertIsNone(st._handle_pumped(stop))
        self.assertEqual(len(st._suspects), 1)
        self.assertFalse(st.suspended)
        self.assertIsNone(st.thread_id)
        # The exit still wins over the held suspect.
        with self.assertRaises(bridge.BridgeErr) as cm:
            st._handle_pumped({"type": "event", "event": "exited",
                               "body": {}})
        self.assertIn("target exited", str(cm.exception))

    def test_continued_then_new_stop_parks(self):
        st = self.session()
        st.publish_state = Mock()
        st.dap_request = Mock(return_value={})
        st._awaiting_continued = True
        dead = {"type": "event", "event": "stopped",
                "body": {"reason": "breakpoint", "threadId": 2}}
        st._handle_pumped(dead)  # held as suspect
        st._co_stop_frame = Mock(return_value=None)
        # Missing flag means "everything resumed" (true).
        cont = {"type": "event", "event": "continued",
                "body": {"threadId": 1}}
        self.assertIsNone(st._handle_pumped(cont))
        self.assertFalse(st._awaiting_continued)
        self.assertEqual(st._suspects, [])
        # A post-barrier stop is genuine and parks.
        st.refresh_frames = Mock(
            side_effect=lambda levels=64, timeout=30: st.frames.append(
                {"id": 3, "name": "w",
                 "source": {"path": "/tmp/x.py"}, "line": 9}))
        fresh = {"type": "event", "event": "stopped",
                 "body": {"reason": "breakpoint", "threadId": 3}}
        self.assertEqual(st._handle_pumped(fresh), "stopped")
        self.assertEqual(st.thread_id, 3)

    def test_continued_single_thread_keeps_other_suspects(self):
        st = self.session()
        st.publish_state = Mock()
        st.dap_request = Mock(return_value={})
        st._awaiting_continued = True
        t2 = {"type": "event", "event": "stopped",
              "body": {"reason": "breakpoint", "threadId": 2}}
        st._handle_pumped(t2)
        st._co_stop_frame = Mock(return_value={
            "id": 2, "name": "w2",
            "source": {"path": "/tmp/b.py"}, "line": 9})
        st.refresh_frames = Mock(
            side_effect=lambda levels=64, timeout=30: st.frames.append(
                {"id": 2, "name": "w2",
                 "source": {"path": "/tmp/b.py"}, "line": 9}))
        cont = {"type": "event", "event": "continued",
                "body": {"threadId": 1, "allThreadsContinued": False}}
        self.assertEqual(st._handle_pumped(cont), "stopped")
        self.assertEqual(st.thread_id, 2)

    def test_deadline_probe_parks_live_suspect(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.session()
            st.cfg.dir = tmp
            st._nonce = bridge.write_owner(tmp)
            st.publish_state = Mock()
            st.dap_request = Mock(return_value={})
            st._awaiting_continued = True
            st.refresh_frames = Mock(
                side_effect=lambda levels=64, timeout=30: st.frames.append(
                    {"id": 2, "name": "w2",
                     "source": {"path": "/tmp/b.py"}, "line": 9}))
            st._co_stop_frame = Mock(return_value={
                "id": 2, "name": "w2",
                "source": {"path": "/tmp/b.py"}, "line": 9})
            st.dap = SimpleNamespace(stash=[
                {"type": "event", "event": "stopped",
                 "body": {"reason": "breakpoint", "threadId": 2}}])
            self.assertEqual(st.pump(0), "stopped")
            self.assertEqual(st.thread_id, 2)

    def test_deadline_probe_drops_dead_suspects_then_times_out(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.session()
            st.cfg.dir = tmp
            st._nonce = bridge.write_owner(tmp)
            st.publish_state = Mock()
            st._awaiting_continued = True
            st._co_stop_frame = Mock(return_value=None)
            st.dap = SimpleNamespace(stash=[
                {"type": "event", "event": "stopped",
                 "body": {"reason": "breakpoint", "threadId": i}}
                for i in (2, 3, 4)])
            with self.assertRaises(bridge.StopTimeout):
                st.pump(0)
            # Bounded: only the first two suspects are ever probed.
            self.assertEqual(st._co_stop_frame.call_count, 2)
            self.assertFalse(st._awaiting_continued)

    def test_bare_timeout_releases_barrier(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.session()
            st.cfg.dir = tmp
            st._nonce = bridge.write_owner(tmp)
            st.publish_state = Mock()
            st._awaiting_continued = True
            st.dap = SimpleNamespace(stash=[])
            with self.assertRaises(bridge.StopTimeout):
                st.pump(0)
            self.assertFalse(st._awaiting_continued)

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
