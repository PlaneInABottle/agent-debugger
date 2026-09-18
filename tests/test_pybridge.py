"""Deterministic regressions for DAP ordering and stopped-state handling."""
import contextlib
import importlib.util
import json
import os
from pathlib import Path
import socket
import tempfile
import threading
import time
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

    @contextlib.contextmanager
    def chdir_tmp(self, tmp):
        """chdir into `tmp`, restoring BEFORE the enclosing
        TemporaryDirectory body exits. Plain addCleanup(os.chdir) runs
        after __exit__ — Windows cannot rmtree the process cwd
        (WinError 32), so every chdir-into-tmp test must use this as
        `with tempfile.TemporaryDirectory() as tmp, self.chdir_tmp(tmp):`."""
        old = os.getcwd()
        os.chdir(tmp)
        try:
            yield tmp
        finally:
            os.chdir(old)

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

    def test_idle_pump_framing_error_never_marks_exited(self):
        # A malformed adapter frame on the select path is not target death:
        # idle_pump must warn and stop, leaving exited False (the next
        # command's pump surfaces the error). Pre-fix this set exited=True
        # with a fabricated session exit.
        st = self.session()
        st.exited = False
        st.dap = object()
        st.publish_state = Mock()
        left, right = socket.socketpair()
        self.addCleanup(left.close)
        self.addCleanup(right.close)
        right.send(b"x")
        fake = SimpleNamespace(
            _read_msg=Mock(side_effect=bridge.BridgeErr("DAP header too large")))
        st._conn_entries = Mock(return_value=[("t1", fake, left)])
        st._pop_stash = Mock(return_value=None)
        st._try_read = Mock(return_value=None)
        bridge.idle_pump(st)
        self.assertFalse(st.exited)
        st.publish_state.assert_not_called()

    def test_cmd_logs_bad_tail_defaults_instead_of_internal(self):
        # Node/Browser/Java parity: a non-numeric tail degrades to 50,
        # never an internal error.
        with tempfile.TemporaryDirectory() as tmp:
            st = self.session()
            st.cfg.dir = tmp
            st.exited = True  # skip the pending-drain pump
            st.append_log("hello")
            for bad in ["xx", None, ""]:
                resp = st.cmd_logs({"tail": bad})
                self.assertTrue(resp["ok"])
                self.assertEqual(resp["lines"], ["hello"])
            self.assertEqual(st.cmd_logs({})["lines"], ["hello"])

    def test_setup_usage_exits_2(self):
        # Java/Node parity: CLI-arg failures exit 2 (Usage), never 1.
        import subprocess
        import sys
        r = subprocess.run(
            [sys.executable, str(ROOT / "bridge/py/src/pybridge.py"), "bogus"],
            capture_output=True, text=True, timeout=30)
        self.assertEqual(r.returncode, 2)
        self.assertIn("usage", r.stdout + r.stderr)

    def test_parse_logpoint_rejects_empty_template(self):        # Java parity: an empty template is a config error, never a
        # silently-armed empty logpoint.
        with tempfile.TemporaryDirectory() as tmp:
            f = os.path.join(tmp, "a.py")
            Path(f).write_text("x = 1\n" * 5)
            cfg = bridge.Config()
            with self.assertRaises(bridge.Usage):
                bridge.parse_logpoint(f + ":2:", cfg)
            bridge.parse_logpoint(f + ":2:hit {x}", cfg)
            self.assertEqual(cfg.logpoints[0][2], "hit {x}")

    def test_parse_logpoint_skips_windows_drive_colon(self):
        # A drive-absolute spec splits path=`C:\...\app.py`, never
        # path=`C` (Windows CI builds these from real tmp paths; the
        # drive itself never exists here, so the failure must name the
        # FULL drive path — proving the split skipped index 1).
        with self.assertRaises(bridge.Usage) as cm:
            bridge.parse_logpoint("C:\\Users\\R\\Temp\\app.py:3:x=1",
                                  bridge.Config())
        self.assertIn("C:\\Users\\R\\Temp\\app.py", str(cm.exception))
        self.assertNotIn("no such file: C ", str(cm.exception))
        # Templates keep their colons; legacy drive-relative/one-char
        # specs keep legacy behavior.
        with tempfile.TemporaryDirectory() as tmp:
            f = os.path.join(tmp, "a.py")
            Path(f).write_text("x = 1\n" * 5)
            cfg = bridge.Config()
            bridge.parse_logpoint(f + ":2:a:b", cfg)
            self.assertEqual(cfg.logpoints[0][2], "a:b")
        with self.assertRaises(bridge.Usage):
            bridge.parse_logpoint("C:4:t", bridge.Config())

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
            self.assertEqual(resp, {"ok": True, "added": [], "stops": st.stop_states,
                                    "target": "main"})
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

            def fake(command, args=None, timeout=30, semantic=False):
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
        with tempfile.TemporaryDirectory() as tmp, self.chdir_tmp(tmp):
            root = Path(tmp) / "root"
            (root / "deep").mkdir(parents=True)
            (root / "deep" / "probe.py").write_text("x = 1\n")
            cwd_file = Path(tmp) / "probe.py"
            cwd_file.write_text("y = 2\n")
            got = bridge.resolve_source_path("probe.py", [str(root)])
            self.assertEqual(got, os.path.realpath(str(cwd_file)))

    def test_resolve_missing_basename_no_src_fails_fast(self):
        with tempfile.TemporaryDirectory() as tmp, self.chdir_tmp(tmp):
            with self.assertRaises(bridge.Usage) as cm:
                bridge.resolve_source_path("ghost.py", [])
            msg = str(cm.exception)
            self.assertIn("ghost.py", msg)
            self.assertIn(os.path.abspath("ghost.py"), msg)
            self.assertIn("no --src roots given", msg)

    def test_resolve_basename_unique_under_src(self):
        with tempfile.TemporaryDirectory() as tmp, self.chdir_tmp(tmp):
            target = Path(tmp) / "root" / "a" / "b" / "only.py"
            target.parent.mkdir(parents=True)
            target.write_text("x = 1\n")
            got = bridge.resolve_source_path("only.py", [str(Path(tmp) / "root")])
            self.assertEqual(got, os.path.realpath(str(target)))

    def test_resolve_basename_ambiguous_under_src(self):
        with tempfile.TemporaryDirectory() as tmp, self.chdir_tmp(tmp):
            for sub in ("a", "b"):
                d = Path(tmp) / "root" / sub
                d.mkdir(parents=True)
                (d / "dup.py").write_text("x = 1\n")
            with self.assertRaises(bridge.Usage) as cm:
                bridge.resolve_source_path("dup.py", [str(Path(tmp) / "root")])
            msg = str(cm.exception)
            self.assertIn("ambiguous", msg)
            self.assertIn("dup.py", msg)
            self.assertIn(os.path.join("a", "dup.py"), msg)
            self.assertIn(os.path.join("b", "dup.py"), msg)

    def test_resolve_nested_relative_under_src(self):
        with tempfile.TemporaryDirectory() as tmp, self.chdir_tmp(tmp):
            target = Path(tmp) / "root" / "tests" / "wf" / "nested.py"
            target.parent.mkdir(parents=True)
            target.write_text("x = 1\n")
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
        with tempfile.TemporaryDirectory() as tmp, self.chdir_tmp(tmp):
            real = Path(tmp) / "root" / "a"
            real.mkdir(parents=True)
            (real / "same.py").write_text("x = 1\n")
            # Same file reachable twice: direct + symlinked sibling dir.
            # The symlinked dir is skipped, the real one resolves alone.
            try:
                os.symlink(str(real), str(Path(tmp) / "root" / "blink"))
            except OSError:
                self.skipTest("symlinks unavailable")
            got = bridge.resolve_source_path("same.py", [str(Path(tmp) / "root")])
            self.assertEqual(got, os.path.realpath(str(real / "same.py")))

    def test_logpoint_uses_same_resolver(self):
        with tempfile.TemporaryDirectory() as tmp, self.chdir_tmp(tmp):
            target = Path(tmp) / "root" / "lp.py"
            target.parent.mkdir(parents=True)
            target.write_text("".join(f"line {n}\n" for n in range(6)))
            cfg = bridge.Config()
            cfg.src_dirs = [str(Path(tmp) / "root")]
            bridge.parse_logpoint("lp.py:3:val={x}", cfg)
            self.assertEqual(cfg.logpoints,
                             [(os.path.realpath(str(target)), 3, "val={x}")])
            with self.assertRaises(bridge.Usage):
                bridge.parse_logpoint("ghost.py:3:val={x}", cfg)

    def test_parse_args_resolves_break_before_src_flag(self):
        with tempfile.TemporaryDirectory() as tmp, self.chdir_tmp(tmp):
            target = Path(tmp) / "root" / "deep" / "early.py"
            target.parent.mkdir(parents=True)
            target.write_text("x = 1\n")
            prog = Path(tmp) / "app.py"
            prog.write_text("print('hi')\n")
            cfg = bridge.parse_args(["session", "--kind", "launch",
                                     "--dir", str(Path(tmp) / "sess"),
                                     "--program", str(prog),
                                     "--break", "early.py:1",
                                     "--src", str(Path(tmp) / "root")])
            self.assertEqual(cfg.breaks,
                             [(os.path.realpath(str(target)), 1, None)])

    def test_parse_args_missing_value_is_usage_for_all_value_flags(self):
        # Every value flag shares the _need path: a missing value is a
        # clean Usage naming the flag, never IndexError->traceback/internal.
        with tempfile.TemporaryDirectory() as tmp:
            base = ["session", "--kind", "launch", "--dir", tmp,
                    "--module", "m"]
            for flag in ["--kind", "--dir", "--program", "--module",
                         "--python", "--host", "--port", "--src",
                         "--break", "--logpoint", "--timeout",
                         "--target-identity"]:
                with self.subTest(flag=flag):
                    with self.assertRaises(bridge.Usage) as cm:
                        bridge.parse_args(base + [flag])
                    self.assertIn("needs a value", str(cm.exception))
                    self.assertIn(flag, str(cm.exception))
            # `--` program args are preserved verbatim, never parsed.
            cfg = bridge.parse_args(
                base + ["--", "--kind", "--dir", "--break"])
            self.assertEqual(cfg.prog_args, ["--kind", "--dir", "--break"])

    def test_breaks_add_uses_live_src_dirs(self):
        with tempfile.TemporaryDirectory() as tmp, self.chdir_tmp(tmp):
            target = Path(tmp) / "root" / "sub" / "live.py"
            target.parent.mkdir(parents=True)
            target.write_text("".join(f"line {n}\n" for n in range(12)))
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
        with tempfile.TemporaryDirectory() as tmp, self.chdir_tmp(tmp):
            path = os.path.realpath(str(Path(tmp) / "app.py"))
            Path(path).write_text("a = 1\n# note\nb = 2\nprint(b)\n")
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

        def fake(command, args=None, timeout=30, semantic=False):
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


    def test_module_launch_args_use_module_not_program(self):
        with tempfile.TemporaryDirectory() as tmp:
            argv = ["session", "--kind", "launch", "--dir", tmp,
                    "--module", "mypkg.mod", "--", "--arg", "1"]
            cfg = bridge.parse_args(argv)
            self.assertEqual(cfg.module, "mypkg.mod")
            self.assertIsNone(cfg.program)
            st = bridge.Session(cfg)
            seen = {}

            class FakeDap:
                def send_only(self, command, args=None):
                    seen["command"] = command
                    seen["args"] = args

                def request(self, command, args=None, timeout=30, semantic=False):
                    return {}

            st.dap = FakeDap()
            st.arm_breakpoints = lambda: None
            st._drain_launch_response = lambda: None
            st.handshake_launch()
            self.assertEqual(seen["command"], "launch")
            self.assertEqual(seen["args"]["module"], "mypkg.mod")
            self.assertEqual(seen["args"]["args"], ["--arg", "1"])
            self.assertNotIn("program", seen["args"])

    def test_module_launch_needs_exactly_one_form(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(bridge.Usage):
                bridge.parse_args(["session", "--kind", "launch", "--dir", tmp])
            with self.assertRaises(bridge.Usage):
                bridge.parse_args(["session", "--kind", "launch", "--dir", tmp,
                                   "--program", "a.py", "--module", "m"])
            for bad in ["", ".mod", "a..b", "a-b", "9lives"]:
                with self.assertRaises(bridge.Usage):
                    bridge.parse_args(["session", "--kind", "launch", "--dir", tmp,
                                       "--module", bad])

    def test_breaks_remove_echoes_stored_raw_and_drops_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = self.add_session(tmp, breaks=[(path, 5, None), (path, 8, None)])
            st.cfg.break_raws = {(path, 5, None): f"{path}:5",
                                 (path, 8, None): f"{path}:8"}
            st.stop_states = [
                {"spec": "a.py:5", "kind": "break", "state": "verified", "hits": 0},
                {"spec": "a.py:8", "kind": "break", "state": "verified", "hits": 0},
            ]
            st._hitkeys = [("break", path, 5, 5), ("break", path, 8, 8)]
            st.dap_request.return_value = {"breakpoints": []}
            resp = st.cmd_breaks_remove({"breaks": [f"{path}:5"]})
            self.assertTrue(resp["ok"])
            self.assertEqual([e["raw"] for e in resp["removed"]], [f"{path}:5"])
            self.assertEqual(st.cfg.breaks, [(path, 8, None)])
            self.assertEqual(len(st.stop_states), 1)
            # DAP re-sent the file merged without the removed line.
            _, kwargs = st.dap_request.call_args
            sent = kwargs["args"] if "args" in kwargs else st.dap_request.call_args[0][1]
            self.assertEqual(sent["breakpoints"], [{"line": 8}])

    def test_breaks_remove_missing_is_not_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.add_session(tmp)
            resp = st.cmd_breaks_remove({"breaks": ["ghost.py:9"]})
            self.assertTrue(resp["ok"])
            self.assertEqual(resp["removed"], [])
            self.assertEqual(resp["missing"], ["ghost.py:9"])
            st.dap_request.assert_not_called()

    def test_breaks_remove_works_after_source_deleted(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "gone.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = self.add_session(tmp, breaks=[(path, 5, None)])
            st.cfg.break_raws = {(path, 5, None): f"{path}:5"}
            st.stop_states = [
                {"spec": "gone.py:5", "kind": "break", "state": "verified", "hits": 0}
            ]
            st._hitkeys = [("break", path, 5, 5)]
            os.unlink(path)
            st.dap_request.return_value = {"breakpoints": []}
            resp = st.cmd_breaks_remove({"breaks": [f"{path}:5"]})
            self.assertTrue(resp["ok"])
            self.assertEqual([e["raw"] for e in resp["removed"]], [f"{path}:5"])
            self.assertEqual(st.cfg.breaks, [])

    def test_breaks_remove_cond_needs_full_spec(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = self.add_session(tmp, breaks=[(path, 5, "x > 1")])
            st.cfg.break_raws = {(path, 5, "x > 1"): f"{path}:5|x > 1"}
            st.stop_states = [
                {"spec": "a.py:5|x > 1", "kind": "break", "state": "verified", "hits": 0}
            ]
            st._hitkeys = [("break", path, 5, 5)]
            # Plain spec does not match the conditional record.
            resp = st.cmd_breaks_remove({"breaks": [f"{path}:5"]})
            self.assertTrue(resp["ok"])
            self.assertEqual(resp["removed"], [])
            self.assertEqual(resp["missing"], [f"{path}:5"])
            self.assertEqual(st.cfg.breaks, [(path, 5, "x > 1")])

    def test_breaks_clear_drops_only_line_breaks(self):
        with tempfile.TemporaryDirectory() as tmp:
            a = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(a).write_text("".join(f"line {n}\n" for n in range(12)))
            st = self.add_session(tmp, breaks=[(a, 5, None), (a, 8, None)])
            st.cfg.break_raws = {(a, 5, None): f"{a}:5", (a, 8, None): f"{a}:8"}
            st.cfg.logpoints = [(a, 3, "tmpl")]
            st.stop_states = [
                {"spec": "a.py:5", "kind": "break", "state": "verified", "hits": 0},
                {"spec": "a.py:8", "kind": "break", "state": "verified", "hits": 0},
            ]
            st._hitkeys = [("break", a, 5, 5), ("break", a, 8, 8)]
            st.dap_request.return_value = {"breakpoints": []}
            resp = st.cmd_breaks_clear()
            self.assertTrue(resp["ok"])
            self.assertEqual(len(resp["removed"]), 2)
            self.assertEqual(st.cfg.breaks, [])
            self.assertEqual(st.cfg.logpoints, [(a, 3, "tmpl")])
            # The logpoint rode along in the replace call.
            sent = st.dap_request.call_args[0][1]
            self.assertEqual(sent["breakpoints"], [{"line": 3, "logMessage": "tmpl"}])

    def test_breaks_remove_backend_failure_keeps_state(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = self.add_session(tmp, breaks=[(path, 5, None)])
            st.cfg.break_raws = {(path, 5, None): f"{path}:5"}
            st.dap_request.side_effect = bridge.BridgeErr("adapter exploded")
            with self.assertRaises(bridge.BridgeErr):
                st.cmd_breaks_remove({"breaks": [f"{path}:5"]})
            self.assertEqual(st.cfg.breaks, [(path, 5, None)])

    def test_breaks_remove_different_spelling_matches(self):
        with tempfile.TemporaryDirectory() as tmp, self.chdir_tmp(tmp):
            Path("a.py").write_text("".join(f"line {n}\n" for n in range(12)))
            st = self.add_session(tmp)
            st.dap_request.return_value = {"breakpoints": [{"verified": True}]}
            added = st.cmd_breaks_add({"breaks": ["a.py:5"]})
            self.assertEqual(len(added["added"]), 1)
            canon = os.path.realpath(os.path.join(tmp, "a.py"))
            self.assertEqual(st.cfg.breaks, [(canon, 5, None)])
            # A different spelling of the same file matches stored identity.
            st.dap_request.return_value = {"breakpoints": []}
            resp = st.cmd_breaks_remove({"breaks": ["./a.py:5"]})
            self.assertTrue(resp["ok"])
            self.assertEqual([e["raw"] for e in resp["removed"]], ["a.py:5"])
            self.assertEqual(st.cfg.breaks, [])

    def test_breaks_remove_src_spelling_and_deleted_file(self):
        with tempfile.TemporaryDirectory() as tmp, self.chdir_tmp(tmp):
            root = Path(tmp) / "srcroot"
            (root / "pkg").mkdir(parents=True)
            target = root / "pkg" / "mod.py"
            target.write_text("".join(f"line {n}\n" for n in range(12)))
            st = self.session()
            st.cfg.dir = tmp
            st.cfg.src_dirs = [str(root)]
            st.dap_request = Mock()
            st.dap_request.return_value = {"breakpoints": [{"verified": True}]}
            added = st.cmd_breaks_add({"breaks": ["pkg/mod.py:5"]})
            self.assertEqual(len(added["added"]), 1)
            # Absolute spelling matches the --src-joined stored identity.
            st.dap_request.return_value = {"breakpoints": []}
            resp = st.cmd_breaks_remove({"breaks": [f"{target}:5"]})
            self.assertTrue(resp["ok"])
            self.assertEqual([e["raw"] for e in resp["removed"]], ["pkg/mod.py:5"])
            self.assertEqual(st.cfg.breaks, [])
            # Re-add, delete the file, remove by yet another spelling.
            st.dap_request.return_value = {"breakpoints": [{"verified": True}]}
            st.cmd_breaks_add({"breaks": ["pkg/mod.py:7"]})
            os.unlink(target)
            st.dap_request.return_value = {"breakpoints": []}
            resp = st.cmd_breaks_remove({"breaks": [f"{target}:7"]})
            self.assertTrue(resp["ok"])
            self.assertEqual([e["raw"] for e in resp["removed"]], ["pkg/mod.py:7"])
            self.assertEqual(st.cfg.breaks, [])

    def test_timeout_text_carries_identity_hint(self):
        st = self.session()
        st.cfg.target_identity_seed = {
            "debuggee": {"executable": "python3", "argv": ["python3", "a.py"], "cwd": "/t"},
            "endpoint": {}, "adapter": {},
        }
        self.assertIn("target identity", st.timeout_text(5))
        st.cfg.target_identity_seed = None
        self.assertNotIn("identity", st.timeout_text(5))

    def test_publish_state_carries_schema_v2_identity(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.session()
            st.cfg.dir = tmp
            st.session_port = 4242
            st.cfg.target_identity_seed = {
                "debuggee": {"kind": "process", "pid": None},
                "endpoint": {}, "adapter": {},
            }
            st.publish_state(False)
            saved = json.loads(Path(tmp, "session.json").read_text())
            self.assertEqual(saved["schemaVersion"], 2)
            self.assertNotIn("observedTarget", saved)
            self.assertIn("targetIdentity", saved)
            # Main roster entries carry no observed (identity is top-level).
            entry = st.target_entry("main")
            self.assertNotIn("observed", entry)

    # -- multi-target roster (M-T/M3) -------------------------------------

    def target_session(self, tmp):
        st = self.session()
        st.cfg.dir = tmp
        st.cfg.subprocess = True
        st.cfg.kind = "launch"
        st.adapter_port = 4711
        return st

    def fake_child(self, st, pid, suspended=False):
        tid = f"child:{pid}"
        child = bridge.ChildTarget(tid, pid, Mock(), Mock(),
                                   {"pid": pid, "source": "debugpy-subProcessId"})
        st.targets[tid] = child
        st.target_order.append(tid)
        st._seen_ids.add(tid)
        child.state = "stopped" if suspended else "running"
        child.suspended = suspended
        if suspended:
            st._stop_seq += 1
            child.stop_seq = st._stop_seq
        return child

    def test_targets_roster_shape_and_selected(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            st.suspended = True
            st._main_seq = 1
            st._stop_seq = 1
            self.fake_child(st, 111, suspended=True)
            st._stop_seq += 1
            st.targets["child:111"].stop_seq = st._stop_seq
            resp = st.cmd_targets()
            self.assertTrue(resp["ok"])
            self.assertEqual(resp["target"], "main")
            ids = [t["id"] for t in resp["targets"]]
            self.assertEqual(ids, ["main", "child:111"])
            main, child = resp["targets"]
            self.assertEqual(main["kind"], "main")
            self.assertEqual(main["scope"], "global")
            self.assertEqual(child["kind"], "child")
            self.assertEqual(child["pid"], 111)
            self.assertEqual(child["state"], "stopped")
            self.assertEqual(child["observed"],
                             {"pid": 111, "source": "debugpy-subProcessId"})
            self.assertEqual(child["scope"], "inherited")
            # Most recent stop wins auto-select.
            self.assertEqual(resp["selected"], "child:111")
            self.assertEqual(resp["ignored"], 0)
            self.assertEqual(resp["droppedExited"], 0)

    def test_resolve_target_auto_and_explicit_errors(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            # Nothing parked: auto is main.
            self.assertEqual(st.resolve_target({}), "main")
            c1 = self.fake_child(st, 1, suspended=True)
            c2 = self.fake_child(st, 2, suspended=True)
            self.assertEqual(st.resolve_target({}), "child:2")
            self.assertEqual(st.resolve_target({"target": "child:1"}), "child:1")
            with self.assertRaises(bridge.BridgeErr) as cm:
                st.resolve_target({"target": "child:9"})
            self.assertIn("unknown target", str(cm.exception))
            st._note_exit("child:1")
            with self.assertRaises(bridge.BridgeErr) as cm:
                st.resolve_target({"target": "child:1"})
            self.assertIn("has exited", str(cm.exception))

    def test_note_exit_bounded_history(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            for pid in range(20):
                self.fake_child(st, pid)
                st._note_exit(f"child:{pid}")
            self.assertEqual(len(st.exited_targets), bridge.MAX_EXITED_HISTORY)
            self.assertEqual(st.targets_reg.dropped_exited, 20 - bridge.MAX_EXITED_HISTORY)
            ids = [e["id"] for e in st.exited_targets]
            self.assertNotIn("child:0", ids)  # oldest evicted first

    def test_exited_targets_bound_all_lists_consistent(self):
        # >16 exits: dead ids leave no trace in the live roster, so pump
        # loops (live_targets/active_nonmain/target_order scans) never walk
        # unbounded dead ids — only live + bounded ignored entries remain.
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            for pid in range(20):
                self.fake_child(st, pid)
                st._note_exit(f"child:{pid}")
            self.assertEqual(len(st.exited_targets), bridge.MAX_EXITED_HISTORY)
            self.assertEqual(st.targets_reg.dropped_exited, 20 - bridge.MAX_EXITED_HISTORY)
            self.assertEqual(st.target_order, [])
            self.assertEqual(st.live_targets(), [])
            self.assertEqual(st.active_nonmain(), [])
            self.assertEqual(st.targets, {})
            resp = st.cmd_targets()
            ids = [t["id"] for t in resp["targets"]]
            self.assertEqual(ids[0], "main")
            self.assertEqual(len(ids), 1 + bridge.MAX_EXITED_HISTORY)
            self.assertEqual(len(set(ids)), len(ids))  # no dupes, no ghosts

    def test_target_entry_main_uses_main_exited(self):
        # Main DAP session over but children live on: the roster must show
        # main exited (not running) while the session itself is not done.
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            self.fake_child(st, 5)
            st.main_exited = True
            self.assertFalse(st.exited)
            entry = st.target_entry("main")
            self.assertEqual(entry["state"], "exited")

    def test_parse_port_timeout_malformed_is_usage(self):
        with tempfile.TemporaryDirectory() as tmp:
            prog = str(Path(tmp) / "app.py")
            Path(prog).write_text("print('hi')\n")
            base = ["session", "--kind", "launch", "--dir", tmp,
                    "--program", prog]
            for argv in [base + ["--timeout", "fast"],
                         base + ["--timeout"],
                         base + ["--timeout", "nan"],
                         base + ["--timeout", "0"]]:
                with self.assertRaises(bridge.Usage):
                    bridge.parse_args(argv)
            attach = ["session", "--kind", "attach", "--dir", tmp,
                      "--host", "127.0.0.1"]
            with self.assertRaises(bridge.Usage):
                bridge.parse_args(attach + ["--port", "fast"])
            with self.assertRaises(bridge.Usage):
                bridge.parse_args(attach + ["--port"])
            # Valid values still parse.
            cfg = bridge.parse_args(base + ["--timeout", "5"])
            self.assertEqual(cfg.timeout, 5.0)
            cfg = bridge.parse_args(attach + ["--port", "1234"])
            self.assertEqual(cfg.port, 1234)

    def test_delayed_handshake_keeps_live_reads_prompt(self):
        # A ~blocked private child handshake must never hold Session._gate:
        # while one pump thread sits in the child handshake, a concurrent
        # breaks prompt is served promptly, and the child commits after.
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            st._child_cmdline = Mock(return_value="/usr/bin/python3 /app/w.py")
            entered = threading.Event()
            release = threading.Event()

            class SlowDap:
                def __init__(self, sock):
                    self.sock = sock
                    self.stash = [{"type": "response", "command": "attach",
                                   "request_seq": 1, "success": True,
                                   "body": {}}]
                def request(self, cmd, args=None, timeout=30):
                    if cmd == "initialize":
                        entered.set()
                        self._release_ok = release.wait(timeout=10)
                    return {}
                def send_only(self, cmd, args=None):
                    pass

            with patch.object(bridge.socket, "create_connection",
                              return_value=Mock()):
                with patch.object(bridge, "DapConn", SlowDap):
                    msg = {"type": "event", "event": "debugpyAttach",
                           "body": {"subProcessId": 4242, "connect": {}}}
                    worker = threading.Thread(
                        target=st._dispatch_pumped, args=(msg, "main"))
                    worker.start()
                    try:
                        self.assertTrue(entered.wait(timeout=10),
                                        "handshake never started")
                        t0 = time.monotonic()
                        resp = st.cmd_breaks({})
                        dt = time.monotonic() - t0
                        self.assertTrue(resp["ok"])
                        self.assertLess(dt, 2.0)
                    finally:
                        release.set()
                        worker.join(timeout=10)
                    self.assertFalse(worker.is_alive())
            self.assertIn("child:4242", st.targets)
            self.assertEqual(st.targets["child:4242"].state, "running")
            self.assertIn("child:4242", st.target_order)

    def test_ephemeral_add_remove_roundtrip(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "w.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = self.target_session(tmp)
            child = self.fake_child(st, 7)
            answers = iter([
                {"breakpoints": [{"verified": True, "line": 5}]},
                {"breakpoints": [{"verified": True, "line": 5}]},
            ])
            real_request = bridge.DapConn.request
            st.dap = Mock()
            st.dap.stash = []
            with st._TargetScope(st, "child:7"):
                pass  # scope enter/exit restores main state
            # Drive the ephemeral path with a stubbed transport.
            st.dap_request = Mock(side_effect=lambda *a, **k: next(answers))
            resp = st.cmd_breaks_add({"target": "child:7",
                                      "breaks": [f"{path}:5"]})
            self.assertTrue(resp["ok"])
            self.assertEqual(resp["target"], "child:7")
            self.assertEqual(resp["added"][0]["raw"], f"{path}:5")
            # Ephemeral: global intent untouched, child carries the record.
            self.assertEqual(st.cfg.breaks, [])
            self.assertEqual(len(child.stop_states), 1)
            self.assertTrue(child.stop_states[0]["spec"].endswith("w.py:5"))
            key = (path, 5, None)
            self.assertIn(key, child.target_raws)
            self.assertNotIn(key, child.inherited_keys)
            # Scoped remove drops only the ephemeral record.
            st.dap_request = Mock(side_effect=lambda *a, **k: next(answers))
            rm = st.cmd_breaks_remove({"target": "child:7",
                                       "breaks": [f"{path}:5"]})
            self.assertTrue(rm["ok"])
            self.assertEqual(rm["target"], "child:7")
            self.assertEqual(rm["removed"][0]["raw"], f"{path}:5")
            self.assertEqual(child.stop_states, [])
            self.assertEqual(child.target_raws, {})
            _ = real_request

    def test_match_child_break_ignores_global_intent(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "g.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = self.target_session(tmp)
            child = self.fake_child(st, 9)
            # Global intent exists, but the child has no ephemeral records:
            # scoped remove reports missing, never touches global.
            st.cfg.breaks = [(path, 5, None)]
            st.cfg.break_raws = {(path, 5, None): f"{path}:5"}
            resp = st.cmd_breaks_remove({"target": "child:9",
                                         "breaks": [f"{path}:5"]})
            self.assertTrue(resp["ok"])
            self.assertEqual(resp["removed"], [])
            self.assertEqual(resp["missing"], [f"{path}:5"])
            self.assertEqual(st.cfg.breaks, [(path, 5, None)])

    def test_accept_child_ignored_when_disabled(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.session()
            st.cfg.dir = tmp
            st.cfg.kind = "launch"
            st.cfg.subprocess = False
            st._accept_child({"subProcessId": 123, "connect": {}})
            self.assertEqual(st.targets, {})

    def test_resource_tracker_skipped_without_connection(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            st._child_cmdline = Mock(return_value=(
                "python -X frozen_modules=off -c import pydevd; "
                "from multiprocessing.resource_tracker import main;main(4)"))
            st._accept_child({"subProcessId": 4242, "connect": {}})
            self.assertEqual(st.targets_reg.helpers_released, 1)
            self.assertEqual(st.targets, {})
            self.assertIn("child:4242", st._seen_ids)

    def test_is_resource_tracker_matches_shim_only(self):
        st = self.session()
        st._child_cmdline = Mock(return_value="/usr/bin/python3 /app/w.py")
        self.assertFalse(st._is_resource_tracker(11))
        st._child_cmdline = Mock(return_value="python -c spawn_main(tracker_fd=5)")
        self.assertFalse(st._is_resource_tracker(12))
        st._child_cmdline = Mock(return_value="python -c from multiprocessing.resource_tracker import main;main(4)")
        self.assertTrue(st._is_resource_tracker(13))
        st._child_cmdline = Mock(side_effect=OSError("gone"))
        self.assertFalse(st._is_resource_tracker(14))

    def test_over_budget_child_minimal_handshake_then_close(self):
        # Over budget: full minimal handshake (initialize, verbatim attach,
        # NO setBreakpoints, configurationDone, drained) then raw close of
        # the established session and a socket-less ignored record. The
        # child is configured (never-configured servers suspend forever)
        # with zero breakpoints, so it can never park.
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            st._child_cmdline = Mock(return_value="/usr/bin/python3 /app/w.py")
            for pid in range(100, 100 + bridge.MAX_ACTIVE_NONMAIN):
                self.fake_child(st, pid)
            calls = []

            class MiniDap:
                def __init__(self, sock):
                    self.sock = sock
                    self.stash = []
                def request(self, cmd, args=None, timeout=30):
                    calls.append(cmd)
                    if cmd == "setBreakpoints":
                        raise AssertionError("release must plant no breaks")
                    return {}
                def send_only(self, cmd, args=None):
                    calls.append("send:" + cmd)
                def _read_msg(self):
                    return {"type": "response", "command": "attach",
                            "request_seq": 1, "success": True, "body": {}}

            sock = Mock()
            from unittest.mock import patch
            with patch.object(bridge.socket, "create_connection",
                              return_value=sock) as cc:
                with patch.object(bridge, "DapConn", MiniDap):
                    st._accept_child({"subProcessId": 999, "connect": {}})
            cc.assert_called_once()
            self.assertIn("initialize", calls)
            self.assertIn("send:attach", calls)
            self.assertIn("configurationDone", calls)
            self.assertNotIn("setBreakpoints", calls)
            # Established close happened; ignored record is socket-less.
            sock.close.assert_called_once_with()
            self.assertEqual(st.targets_reg.ignored, 1)
            t = st.targets["child:999"]
            self.assertEqual(t.state, "ignored")
            self.assertIsNone(t.dap)
            self.assertIsNone(t.sock)
            self.assertEqual(st.targets_reg.retired, [])
            with self.assertRaises(bridge.BridgeErr) as cm:
                st.resolve_target({"target": "child:999"})
            self.assertIn("released", str(cm.exception))

    def test_failed_handshakes_retire_bounded_and_close_only_at_cleanup(self):
        # A fault storm (every handshake fails) must retain each abandoned
        # socket OPEN (closing a half-built debugpy session kills the
        # adapter) in a bounded list; past the bound, children are marked
        # ignored BEFORE any socket opens; cleanup closes everything.
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            st._child_cmdline = Mock(return_value="/usr/bin/python3 /app/w.py")
            opened = []

            def fake_connect(*a, **k):
                sock = Mock()
                opened.append(sock)
                return sock

            class FailDap:
                def __init__(self, sock):
                    self.sock = sock
                    self.stash = []
                def request(self, *a, **k):
                    raise bridge.BridgeErr("boom")
                def send_only(self, *a, **k):
                    pass

            total = bridge.MAX_RETIRED_SOCKETS + 2
            with patch.object(bridge.socket, "create_connection",
                              side_effect=fake_connect):
                with patch.object(bridge, "DapConn", FailDap):
                    for pid in range(5000, 5000 + total):
                        st._accept_child({"subProcessId": pid, "connect": {}})
            # Bounded ownership: exactly MAX opens, rest ignored pre-connect.
            self.assertEqual(len(opened), bridge.MAX_RETIRED_SOCKETS)
            self.assertEqual(len(st.targets_reg.retired), bridge.MAX_RETIRED_SOCKETS)
            self.assertEqual(st.targets_reg.ignored, total - bridge.MAX_RETIRED_SOCKETS)
            for _tid, sock in st.targets_reg.retired:
                sock.close.assert_not_called()
            # No live tracked targets (only socket-less ignored records),
            # bounded exited history from the failures.
            self.assertEqual(st.active_nonmain(), [])
            self.assertEqual(
                sorted(st.targets),
                ["child:5016", "child:5017"])
            self.assertLessEqual(len(st.exited_targets), bridge.MAX_EXITED_HISTORY)
            # Cleanup closes every retired socket exactly once.
            st.adapter = None
            st.dap = None
            st.cleanup()
            self.assertEqual(st.targets_reg.retired, [])
            for sock in opened:
                sock.close.assert_called_once_with()

    def test_retired_full_marks_ignored_without_connect(self):
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            st._child_cmdline = Mock(return_value="/usr/bin/python3 /app/w.py")
            st.targets_reg.retired = [("child:1", Mock())
                                   for _ in range(bridge.MAX_RETIRED_SOCKETS)]
            with patch.object(bridge.socket, "create_connection") as cc:
                st._accept_child({"subProcessId": 777, "connect": {}})
                cc.assert_not_called()
            self.assertEqual(st.targets_reg.ignored, 1)
            self.assertIn("child:777", st._seen_ids)

    def test_every_response_carries_target(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            st.dap_request = Mock(return_value={"threads": []})
            resp = st.cmd_threads({})
            self.assertEqual(resp["target"], "main")
            resp = st.cmd_breaks({})
            self.assertEqual(resp["target"], "main")

    def test_bare_threads_aggregates_main_plus_live_children(self):
        # Bare threads lists main + every live child in roster order, each
        # attributable by target; top-level stays the auto-selected target's
        # dump. Explicit --target main keeps the single-target shape.
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            st.suspended = True
            st._main_seq = 1
            st._stop_seq = 1
            st.dap_request = Mock(return_value={"threads": []})
            # Single-target bare: legacy shape, no aggregation keys.
            resp = st.cmd_threads({})
            self.assertTrue(resp["ok"])
            self.assertEqual(resp["target"], "main")
            self.assertNotIn("targets", resp)
            self.assertNotIn("selected", resp)
            # Parked child newer than main: auto-select serves the child.
            live = self.fake_child(st, 61, suspended=True)
            ignored = self.fake_child(st, 62)
            ignored.state = "ignored"
            resp = st.cmd_threads({})
            self.assertTrue(resp["ok"])
            self.assertEqual(resp["selected"], "child:61")
            self.assertEqual(resp["target"], "child:61")
            ids = [e["target"] for e in resp["targets"]]
            self.assertEqual(ids, ["main", "child:61"])
            main_e, child_e = resp["targets"]
            self.assertFalse(main_e["running"])
            self.assertEqual(main_e["threads"], [])
            self.assertFalse(child_e["running"])
            self.assertEqual(child_e["threads"], [])
            # Top-level stays the selected target's dump.
            self.assertEqual(resp["running"], child_e["running"])
            self.assertEqual(resp["threads"], child_e["threads"])
            # Busy child appears truthfully running without blocking.
            st._outstanding["child:61"] = "continue"
            st.dap_request = Mock(return_value={"threads": []})
            busy = st.cmd_threads({})
            got = {e["target"]: e for e in busy["targets"]}
            self.assertEqual(got["child:61"]["running"], True)
            self.assertEqual(got["child:61"]["threads"], [])
            self.assertIn("main", got)
            del st._outstanding["child:61"]
            # Explicit main pins main even though the child stopped later.
            st.dap_request = Mock(return_value={"threads": []})
            one = st.cmd_threads({"target": "main"})
            self.assertEqual(one["target"], "main")
            self.assertNotIn("targets", one)
            self.assertNotIn("selected", one)
            self.assertIn("running", one)
            self.assertIn("threads", one)
            # Explicit child still serves only that child.
            two = st.cmd_threads({"target": "child:61"})
            self.assertEqual(two["target"], "child:61")
            self.assertNotIn("targets", two)
            # Exited history never joins the aggregate.
            st._note_exit("child:61")
            bare = st.cmd_threads({})
            self.assertEqual([e["target"] for e in bare["targets"]]
                             if "targets" in bare else ["main"], ["main"])

    def test_vars_frame_validation_matrix(self):
        # Uniform frame contract (same on all four bridges): missing/None
        # reads as 0; ints ≥ 0, integer floats, and 1–15 digit strings are
        # the index; malformed/fractional/negative/over-long/mistyped is a
        # typed `<cmd> needs integer frame` (never coerced, never
        # internal); well-formed past-the-end is `no frame N (have M)`.
        # require_stopped still runs first (covered elsewhere).
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            st.thread_id, st.suspended = 7, True
            st.frames = [{"id": 11}, {"id": 12}]
            st.dap_request = Mock(return_value={"stackFrames": [{"id": 11},
                                                                 {"id": 12}]})
            st.frame_locals = Mock(return_value=[{"name": "x"}])
            for req, want in [({}, 0), ({"frame": None}, 0),
                              ({"frame": 0}, 0), ({"frame": 1}, 1),
                              ({"frame": "1"}, 1), ({"frame": "001"}, 1),
                              ({"frame": 1.0}, 1)]:
                resp = st.cmd_vars(req)
                self.assertEqual(resp["frame"], want, req)
            for req, msg in [({"frame": 2}, "no frame 2 (have 2)"),
                             ({"frame": 99}, "no frame 99 (have 2)"),
                             ({"frame": "99999999999"},
                              "no frame 99999999999 (have 2)")]:
                with self.assertRaises(bridge.BridgeErr) as cm:
                    st.cmd_vars(req)
                self.assertEqual(str(cm.exception), msg, req)
            for bad in ["abc", "", "3x", "3.5", "-1", "+1", " 3", "3 ",
                        "0x3", "9999999999999999", "99999999999999999999",
                        -1, -2, 3.5, float("inf"), float("nan"),
                        True, False, ["1"], {"n": 1}]:
                with self.assertRaises(bridge.BridgeErr) as cm:
                    st.cmd_vars({"frame": bad})
                self.assertEqual(str(cm.exception),
                                 "vars needs integer frame", bad)
                self.assertNotIn("internal", str(cm.exception), bad)
            # eval shares the helper with its own command word.
            with self.assertRaises(bridge.BridgeErr) as cm:
                st.cmd_eval({"expr": "1", "frame": "1.5"})
            self.assertEqual(str(cm.exception), "eval needs integer frame")
            with self.assertRaises(bridge.BridgeErr) as cm:
                st.cmd_eval({"expr": "1", "frame": 9})
            self.assertEqual(str(cm.exception), "no frame 9 (have 2)")

    def test_concurrent_identical_adds_converge_to_one_record(self):
        # Eight handler threads racing the same identical add through
        # dispatch: exactly one plant, one live record, one intent entry;
        # the seven rivals report coherent empty-added instead of
        # duplicating bridge state.
        import threading

        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "w.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = self.target_session(tmp)
            st._nonce = bridge.write_owner(tmp)
            st.publish_state = Mock()
            plants = []

            def fake_dap(command, args=None, timeout=30):
                if command == "setBreakpoints":
                    plants.append(1)
                    time.sleep(0.01)  # realistic plant latency: widens the
                    # check-then-mutate race window between rivals
                    n = len(args.get("breakpoints", []))
                    return {"breakpoints": [{"verified": True, "line": 5}] * n}
                raise AssertionError(f"unexpected DAP: {command}")
            st.dap_request = fake_dap
            spec = f"{path}:5"
            out = [None] * 8

            def run(i):
                try:
                    out[i] = st.dispatch({"cmd": "breaksAdd",
                                          "breaks": [spec]})
                except Exception as e:  # noqa: BLE001 — collected below
                    out[i] = e
            ths = [threading.Thread(target=run, args=(i,)) for i in range(8)]
            for t in ths:
                t.start()
            for t in ths:
                t.join(15)
            for r in out:
                self.assertIsInstance(r, dict, r)
                self.assertTrue(r["ok"], r)
            self.assertEqual(len(plants), 1, "a single setBreakpoints plant")
            self.assertEqual(len(st.stop_states), 1)
            self.assertEqual(len(st.cfg.breaks), 1)
            self.assertEqual(len(st._hitkeys), 1)
            winners = [r for r in out if len(r["added"]) == 1]
            self.assertEqual(len(winners), 1)
            self.assertEqual(
                sum(1 for r in out if r["added"] == []), 7)
            self.assertEqual(winners[0]["target"], "main")

    def test_bare_threads_skips_child_that_exits_mid_dump(self):
        # Deterministic churn: the child exits between the roster snapshot
        # and its own dump. It is skipped (never fails the call), the exit
        # stays recorded, and the served entry is coherent.
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            st.suspended = True
            st._main_seq = 1
            st._stop_seq = 1
            st.dap_request = Mock(return_value={"threads": []})
            self.fake_child(st, 61, suspended=True)
            real = st._threads_for
            calls = []

            def spy(tid):
                calls.append(tid)
                if len(calls) == 1:
                    st._note_exit("child:61")
                return real(tid)
            st._threads_for = spy
            resp = st.cmd_threads({})
            self.assertEqual([e["target"] for e in resp["targets"]], ["main"])
            self.assertEqual(resp["selected"], "main")
            self.assertEqual(resp["target"], "main")
            self.assertTrue(any(e["id"] == "child:61"
                                for e in st.exited_targets),
                            "exit still recorded")

    def test_explicit_wait_ignores_stale_child_park(self):
        # Main running, child parked BEFORE the wait (stale): an explicit
        # main wait/continue times out main-scoped, never serves the
        # child, and leaves the child parked and untouched. Real pump
        # over sockless conn doubles (owner written for the owner-guard).
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            st._nonce = bridge.write_owner(tmp)
            st.publish_state = Mock()
            st.dap = SimpleNamespace(stash=[], sock=None)
            child = self.fake_child(st, 61, suspended=True)
            child.dap = SimpleNamespace(stash=[], sock=None)
            with self.assertRaises(bridge.StopTimeout) as cm:
                st.cmd_wait({"target": "main"}, 1)
            self.assertIn("no stop within", str(cm.exception))
            self.assertTrue(child.suspended)
            self.assertEqual(child.state, "stopped")
            # Same through continue on the running main.
            st.dap_request = Mock(return_value={})
            with self.assertRaises(bridge.StopTimeout):
                st.cmd_continue({"target": "main"}, 1)
            self.assertTrue(child.suspended)
            self.assertEqual(child.state, "stopped")

    def test_explicit_pump_skips_other_target_then_serves_own(self):
        # A fresh child stop consumed during an explicit main pump is
        # parked (visible, never discarded) but skipped; main's own stop
        # is served. Omitted pumps keep any-target semantics.
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            st._nonce = bridge.write_owner(tmp)
            st.publish_state = Mock()
            st.dap = SimpleNamespace(stash=[], sock=None)
            child = self.fake_child(st, 61)
            child.dap = SimpleNamespace(stash=[], sock=None)
            script = ["child:61", "main"]

            def fake_dispatch(msg, tid):
                eff = script.pop(0)
                if eff == "child:61":
                    child.suspended = True
                    child.state = "stopped"
                else:
                    st.suspended = True
                st._park_local.parked = eff
                return "stopped"
            st._dispatch_pumped = fake_dispatch
            st.dap.stash.append({"type": "event", "event": "stopped"})
            st.dap.stash.append({"type": "event", "event": "stopped"})
            self.assertEqual(st.pump(5, "main"), "stopped")
            self.assertEqual(st._park_local.parked, "main")
            self.assertTrue(child.suspended,
                            "other-target park stays parked, never consumed")
            # Omitted: the first park serves.
            st2 = self.target_session(tmp)
            st2._nonce = bridge.write_owner(tmp)
            st2.publish_state = Mock()
            st2.dap = SimpleNamespace(stash=[], sock=None)
            c2 = self.fake_child(st2, 62)
            c2.dap = SimpleNamespace(stash=[], sock=None)
            script2 = ["child:62"]

            def fake_dispatch2(msg, tid):
                c2.suspended = True
                c2.state = "stopped"
                st2._park_local.parked = "child:62"
                return "stopped"
            st2._dispatch_pumped = fake_dispatch2
            st2.dap.stash.append({"type": "event", "event": "stopped"})
            self.assertEqual(st2.pump(5), "stopped")
            self.assertEqual(st2._park_local.parked, "child:62")

    def test_explicit_step_scopes_pump_to_its_target(self):
        # Step on parked main with a stale parked child: the pump is
        # pinned to main and the response names main; the child is never
        # resumed or served.
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            st._nonce = bridge.write_owner(tmp)
            st.publish_state = Mock()
            child = self.fake_child(st, 61, suspended=True)
            st.thread_id, st.suspended = 7, True
            st.frames = [{"id": 1, "name": "m",
                          "source": {"path": "/tmp/a.py"}, "line": 3}]
            st.stop_info = None
            st.dap_request = Mock(return_value={})
            seen = {}

            def spy_pump(timeout, want=None):
                seen["want"] = want
                st._park_local.parked = "main"
                return "stopped"
            st.pump = spy_pump
            resp = st.cmd_step({"target": "main", "mode": "over"}, 5)
            self.assertEqual(seen["want"], "main")
            self.assertEqual(resp["target"], "main")
            self.assertTrue(child.suspended)
            self.assertEqual(child.state, "stopped")



    def test_child_suspect_fallback_parks_under_target_scope(self):
        # A child resume whose `continued` never arrives: the pump deadline
        # probes the CHILD's held suspects (not just main's), parks the
        # child, and stamps it — the new stop is served, not timed out.
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            st._nonce = bridge.write_owner(tmp)
            st.publish_state = Mock()
            child = self.fake_child(st, 61)
            child.dap = SimpleNamespace(stash=[])
            child._awaiting_continued = True
            child.suspended = False
            child._suspects = [{"type": "event", "event": "stopped",
                                "body": {"reason": "breakpoint", "threadId": 3}}]
            child._co_seen = set()
            frame = {"id": 9, "name": "w",
                     "source": {"path": "/tmp/k.py"}, "line": 4}
            st._co_stop_frame = Mock(return_value=frame)
            st.refresh_frames = Mock(
                side_effect=lambda levels=64, timeout=30: st.frames.append(frame))
            st.dap = SimpleNamespace(stash=[])
            self.assertEqual(st.pump(0), "stopped")
            self.assertTrue(child.suspended)
            self.assertEqual(child.thread_id, 3)
            self.assertEqual(st._last_park_target, "child:61")
            self.assertFalse(child._awaiting_continued)
            # Main was never awaiting: its flag stays untouched.
            self.assertFalse(st._awaiting_continued)
            # The parked child is now what targetless commands serve.
            self.assertEqual(st.resolve_target({}), "child:61")

    def test_main_suspect_fallback_unchanged_without_children(self):
        # Main-only episode behavior is exactly the legacy path: suspects
        # park, the flag closes, and an empty episode raises StopTimeout.
        with tempfile.TemporaryDirectory() as tmp:
            st = self.session()
            st.cfg.dir = tmp
            st._nonce = bridge.write_owner(tmp)
            st.publish_state = Mock()
            st._awaiting_continued = True
            st.suspended = False
            st._co_stop_frame = Mock(return_value=None)
            stop = {"type": "event", "event": "stopped",
                    "body": {"reason": "breakpoint", "threadId": 2}}
            st._suspects = [stop]
            st.dap = SimpleNamespace(stash=[])
            with self.assertRaises(bridge.StopTimeout):
                st.pump(0)
            self.assertFalse(st._awaiting_continued)

    # -- M5 concurrency: outstanding resume -------------------------------

    def hold_resume(self, st, tid="main", cmd="continue"):
        st._outstanding[tid] = cmd

    def test_m5_live_reads_prompt_while_resume_held(self):
        # While a resume is outstanding, live reads are accepted and
        # answered from published state with zero DAP traffic.
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            st._nonce = bridge.write_owner(tmp)
            st.publish_state = Mock()
            (Path(tmp) / "logs.jsonl").write_text("a\nb\n")
            st.drain_pending = Mock(
                side_effect=AssertionError("no second DAP reader"))
            st.dap_request = Mock(
                side_effect=AssertionError("no DAP traffic on live reads"))
            self.hold_resume(st)
            for cmd in ("threads", "breaks", "logs", "targets"):
                t0 = time.monotonic()
                resp = st.dispatch({"cmd": cmd})
                dt = time.monotonic() - t0
                self.assertTrue(resp["ok"], cmd)
                self.assertLess(dt, 2.0, cmd)
            self.assertEqual(st.dispatch({"cmd": "threads"})["running"], True)
            self.assertEqual(st.dispatch({"cmd": "threads"})["threads"], [])
            st.drain_pending.assert_not_called()
            st.dap_request.assert_not_called()

    def test_m5_live_reads_concurrent_from_threads(self):
        # Eight handler threads hitting live reads at once: all prompt,
        # all ok, no gate deadlock.
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            st._nonce = bridge.write_owner(tmp)
            st.publish_state = Mock()
            (Path(tmp) / "logs.jsonl").write_text("a\n")
            st.drain_pending = Mock()
            st.dap_request = Mock(side_effect=AssertionError("no DAP"))
            self.hold_resume(st)
            cmds = ["threads", "breaks", "logs", "targets"] * 2
            out = [None] * len(cmds)
            def run(i):
                try:
                    out[i] = st.dispatch({"cmd": cmds[i]})
                except Exception as e:  # noqa: BLE001 — collected below
                    out[i] = e
            ths = [threading.Thread(target=run, args=(i,))
                   for i in range(len(cmds))]
            for t in ths:
                t.start()
            for t in ths:
                t.join(5)
            for i, r in enumerate(out):
                self.assertIsInstance(r, dict, (cmds[i], r))
                self.assertTrue(r["ok"], (cmds[i], r))

    def test_m5_second_resume_same_target_busy(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            self.hold_resume(st)
            for cmd in ("continue", "step"):
                with self.assertRaises(bridge.BridgeErr) as cm:
                    st.dispatch({"cmd": cmd})
                self.assertEqual(str(cm.exception),
                                 "busy: continue outstanding for main")

    def test_m5_global_mutation_conflicts_any_resume(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            self.fake_child(st, 61)
            self.hold_resume(st, "child:61")
            for cmd in ("breaksAdd", "breaksRemove", "breaksClear"):
                req = {"cmd": cmd}
                if cmd != "breaksClear":
                    req["breaks"] = ["x:1"]
                with self.assertRaises(bridge.BridgeErr) as cm:
                    st.dispatch(req)
                self.assertEqual(
                    str(cm.exception),
                    "busy: continue outstanding for child:61")

    def test_m5_target_scoped_mutation_conflicts_only_its_target(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            self.fake_child(st, 61)
            other = self.fake_child(st, 62)
            other.dap = SimpleNamespace(stash=[])
            self.hold_resume(st, "child:61")
            # Same target: busy.
            with self.assertRaises(bridge.BridgeErr) as cm:
                st.dispatch({"cmd": "breaksAdd", "target": "child:61",
                             "breaks": ["x:1"]})
            self.assertEqual(str(cm.exception),
                             "busy: continue outstanding for child:61")
            # Different target: independent (ephemeral path runs).
            st._add_ephemeral = Mock(return_value={"ok": True})
            resp = st.dispatch({"cmd": "breaksAdd", "target": "child:62",
                                "breaks": ["x:1"]})
            self.assertTrue(resp["ok"])
            st._add_ephemeral.assert_called_once()

    def test_m5_different_target_resume_independent(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            self.fake_child(st, 61)
            parked = self.fake_child(st, 62)
            parked.dap = SimpleNamespace(stash=[])
            self.hold_resume(st, "child:61")
            seen = []
            def fake_wait(timeout, tid="main", explicit=False):
                seen.append(tid)
                return {"ok": True}
            st._resume_and_wait = fake_wait
            # Same target rival: busy, never reaches the waiter.
            with self.assertRaises(bridge.BridgeErr):
                st.dispatch({"cmd": "continue", "target": "child:61"})
            self.assertEqual(seen, [])
            # Different target: accepted and served.
            resp = st.dispatch({"cmd": "continue", "target": "child:62"})
            self.assertTrue(resp["ok"])
            self.assertEqual(seen, ["child:62"])
            # Registration is released afterwards.
            self.assertNotIn("child:62", st._outstanding)
            self.assertEqual(st._outstanding, {"child:61": "continue"})

    def test_m5_eval_busy_despite_parked_frames(self):
        # eval can mutate: it busy-rejects on its target even when cached
        # frames exist (never served stale as safe).
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            st.thread_id, st.suspended = 7, True
            st.frames = [{"id": 1}]
            self.hold_resume(st)
            with self.assertRaises(bridge.BridgeErr) as cm:
                st.dispatch({"cmd": "eval", "expr": "1+1"})
            self.assertEqual(str(cm.exception),
                             "busy: continue outstanding for main")

    def test_m5_frame_reads_fail_fast_when_running(self):
        # context/vars/stack never busy-reject and never expose stale
        # frames: once the resume publishes running they fail fast.
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            st.suspended, st.thread_id, st.frames = False, None, []
            self.hold_resume(st)
            for cmd in ("context", "stack", "vars"):
                with self.assertRaises(bridge.BridgeErr) as cm:
                    st.dispatch({"cmd": cmd})
                self.assertIn("no stopped thread", str(cm.exception))

    def test_m5_close_accepted_despite_outstanding(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            self.hold_resume(st)
            with self.assertRaises(bridge._Close):
                st.dispatch({"cmd": "close"})

    def test_m5_step_registers_and_releases_outstanding(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            st._nonce = bridge.write_owner(tmp)
            st.publish_state = Mock()
            st.thread_id, st.suspended = 7, True
            st.frames = [{"id": 1}]
            st.dap_request = Mock(return_value={})
            def fake_wait(timeout, tid="main", explicit=False):
                self.assertEqual(st._outstanding.get("main"), "step")
                raise bridge.BridgeErr("timeout: no stop within 1s")
            st._resume_and_wait = fake_wait
            with self.assertRaises(bridge.BridgeErr):
                st.dispatch({"cmd": "step"})
            self.assertEqual(st._outstanding, {})

    def client_roundtrip(self, port, req, timeout=10):
        s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        try:
            bridge.write_frame(s, req)
            return bridge.read_frame(s)
        finally:
            s.close()

    def test_m5_bounded_handlers_close_and_overload(self):
        # Eight handlers may block in dispatch; the ninth is rejected
        # immediately (no unbounded spawn); close is still accepted.
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            st._nonce = bridge.write_owner(tmp)
            blocker = threading.Event()
            real_cleanup = st.cleanup
            def fake_dispatch(req):
                if req.get("cmd") == "close":
                    raise bridge._Close()
                self.assertTrue(blocker.wait(10))
                return {"ok": True}
            st.dispatch = fake_dispatch
            server = socket.socket()
            server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            server.bind(("127.0.0.1", 0))
            server.listen(5)
            port = server.getsockname()[1]
            srv = threading.Thread(target=bridge.serve, args=(st, server,
                                                               st._nonce),
                                   daemon=True)
            srv.start()
            results = [None] * 8
            def run(i):
                try:
                    results[i] = self.client_roundtrip(port, {"cmd": "noop"})
                except Exception as e:  # noqa: BLE001 — collected below
                    results[i] = e
            ths = [threading.Thread(target=run, args=(i,)) for i in range(8)]
            for t in ths:
                t.start()
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                with st._gate:
                    if st.server_state.active >= 8:
                        break
                time.sleep(0.02)
            with st._gate:
                self.assertEqual(st.server_state.active, 8)
            t0 = time.monotonic()
            resp = self.client_roundtrip(port, {"cmd": "noop"})
            self.assertLess(time.monotonic() - t0, 3.0)
            self.assertFalse(resp["ok"])
            self.assertIn("overloaded", resp["error"])
            blocker.set()
            for t in ths:
                t.join(5)
            for r in results:
                self.assertIsInstance(r, dict, r)
                self.assertTrue(r["ok"], r)
            closed = self.client_roundtrip(port, {"cmd": "close"})
            self.assertTrue(closed.get("closed"))
            srv.join(5)
            self.assertFalse(srv.is_alive())
            real_cleanup  # keep linters quiet about the bound method

    def test_m5_disconnect_does_not_cancel(self):
        # The client goes away mid-command: the work still runs, the state
        # still publishes, and only the response drops.
        with tempfile.TemporaryDirectory() as tmp:
            st = self.target_session(tmp)
            published = threading.Event()
            st.publish_state = Mock(side_effect=lambda *a: published.set())
            ran = threading.Event()
            def fake_dispatch(req):
                time.sleep(0.2)
                st.publish_state(False)
                ran.set()
                return {"ok": True}
            st.dispatch = fake_dispatch
            srv, cli = socket.socketpair()
            self.assertTrue(st.server_state.try_admit(bridge.MAX_ACTIVE_HANDLERS))  # serve() owns the increment; direct _handle_one
            t = threading.Thread(target=bridge._handle_one, args=(st, srv),
                                 daemon=True)
            t.start()
            bridge.write_frame(cli, {"cmd": "continue"})
            cli.close()  # gone before the response
            self.assertTrue(ran.wait(5))
            self.assertTrue(published.is_set())
            t.join(5)
            self.assertFalse(t.is_alive())
            with st._gate:
                self.assertEqual(st.server_state.active, 0)


class TargetIdentityTests(unittest.TestCase):
    """Layered endpoint/adapter/debuggee identity + honest waitContext."""

    def session(self, kind="attach"):
        cfg = bridge.Config()
        cfg.kind = kind
        cfg.dir = tempfile.mkdtemp()
        self.addCleanup(lambda: __import__("shutil").rmtree(cfg.dir, ignore_errors=True))
        cfg.host = "127.0.0.1"
        cfg.port = 5678
        return bridge.Session(cfg)

    def test_process_event_from_stash_builds_protocol_debuggee(self):
        st = self.session()
        st.cfg.target_identity_seed = {
            "debuggee": {"kind": "process", "pid": None, "confidence": "unavailable"},
            "endpoint": {
                "host": "127.0.0.1", "port": 5678, "ownerPid": 15298,
                "executable": ".../site-packages/debugpy/adapter",
                "argv": [".../site-packages/debugpy/adapter", "--for-server",
                         "--server-access-token", "SECRET",
                         "--host", "127.0.0.1", "--port", "5678"],
                "cwd": None, "source": "os-lsof-ps",
            },
            "adapter": {"confidence": "unavailable"},
        }
        proc = {"type": "event", "event": "process",
                "body": {"name": "/srv/srv.py", "systemProcessId": 15292,
                         "isLocalProcess": True, "startMethod": "attach"}}
        st.dap = SimpleNamespace(stash=[proc], sock=Mock(), _read_msg=Mock(
            side_effect=socket.timeout("no more")))
        before = json.dumps(st.cfg.target_identity_seed, sort_keys=True)
        self.assertTrue(st._consume_process_event(timeout=0.2))
        self.assertEqual(json.dumps(st.cfg.target_identity_seed, sort_keys=True), before)
        self.assertEqual(st._process_event["pid"], 15292)
        ident = st._target_identity
        # Debuggee is protocol-confirmed and differs from the endpoint owner.
        self.assertEqual(ident["debuggee"]["confidence"], "protocol-confirmed")
        self.assertEqual(ident["debuggee"]["pid"], 15292)
        self.assertEqual(ident["debuggee"]["source"], "dap-process-event")
        self.assertEqual(ident["endpoint"]["ownerPid"], 15298)
        self.assertEqual(ident["endpoint"]["confidence"], "os-corroborated")
        # Adapter recognized from the redacted argv; the raw token is gone.
        self.assertEqual(ident["adapter"]["name"], "debugpy-adapter")
        self.assertEqual(ident["adapter"]["pid"], 15298)
        blob = json.dumps(ident)
        self.assertNotIn("SECRET", blob)
        self.assertIn("[redacted]", blob)
        # Layered seed untouched by the protocol event (still None here
        # is fine); the seed is CLI-owned, the identity is bridge-built.
        self.assertLessEqual(len(blob), bridge.IDENT_TOTAL_CAP)
        # Debuggee-first hint names the program, not the adapter.
        self.assertIn("/srv/srv.py", st._identity_hint)
        self.assertIn("protocol-confirmed", st._identity_hint)
        self.assertTrue(st.timeout_text(2).startswith("timeout: no stop within 2s"))

    def test_process_event_absent_is_unavailable_not_failure(self):
        st = self.session()
        st.dap = SimpleNamespace(stash=[], sock=Mock(),
                                 _read_msg=Mock(side_effect=socket.timeout("quiet")))
        self.assertFalse(st._consume_process_event(timeout=0.1))
        st.cfg.target_identity_seed = None
        ident = st._build_target_identity()
        self.assertEqual(ident["debuggee"]["confidence"], "unavailable")
        self.assertTrue(any(u["field"] == "pid"
                            for u in ident["debuggee"]["unavailable"]))
        self.assertEqual(ident["endpoint"]["confidence"], "unavailable")
        self.assertEqual(st._identity_hint, "")

    def test_handle_main_event_process_never_parks_or_disturbs_child(self):
        st = self.session()
        st.cfg.target_identity_seed = {
            "debuggee": {"kind": "process", "pid": None},
            "endpoint": {"host": "127.0.0.1", "port": 5678, "ownerPid": 9,
                         "argv": ["python", "-m", "debugpy", "--listen", "5678"],
                         "source": "os-proc"},
            "adapter": {"confidence": "unavailable"},
        }
        proc = {"type": "event", "event": "process",
                "body": {"name": "srv.py", "systemProcessId": 7,
                         "startMethod": "attach"}}
        self.assertIsNone(st._handle_main_event(proc))
        self.assertFalse(st.suspended)
        self.assertEqual(st._process_event["pid"], 7)
        # debugpyAttach flow untouched by the process branch.
        st._process_event = None
        self.assertIsNone(st._handle_main_event(
            {"type": "event", "event": "debugpyAttach", "body": {}}))
        self.assertIsNone(st._process_event)
        # Malformed bodies stay contained.
        self.assertIsNone(st._handle_main_event(
            {"type": "event", "event": "process", "body": None}))

    def test_launch_roles_use_own_adapter_pid(self):
        st = self.session(kind="launch")
        st.adapter = SimpleNamespace(pid=4242)
        st.adapter_port = 5555
        st.cfg.target_identity_seed = None
        ident = st._build_target_identity()
        self.assertEqual(ident["endpoint"]["ownerPid"], 4242)
        self.assertEqual(ident["adapter"]["pid"], 4242)
        self.assertEqual(ident["adapter"]["confidence"], "os-corroborated")
        self.assertEqual(ident["debuggee"]["confidence"], "unavailable")

    def test_redact_and_caps_bound_identity(self):
        long = "x" * 900
        argv = ["prog", "--server-access-token", "HEXSECRET",
                "--password=hunter2", long]
        red = bridge.redact_identity_argv(argv)
        joined = " ".join(red)
        self.assertNotIn("HEXSECRET", joined)
        self.assertNotIn("hunter2", joined)
        self.assertIn("[redacted]", joined)
        role = bridge._cap_role({"argv": red, "name": long})
        self.assertLessEqual(len(json.dumps(role)), bridge.IDENT_ROLE_CAP)
        many = [f"--arg{i}" for i in range(120)]
        ident = bridge._cap_identity({"debuggee": {"argv": many},
                                      "endpoint": {}, "adapter": {}})
        self.assertLessEqual(len(json.dumps(ident)), bridge.IDENT_TOTAL_CAP)

    def test_wait_context_is_honest_and_additive(self):
        st = self.session()
        st.cfg.target_identity_seed = {
            "debuggee": {"kind": "process", "pid": None},
            "endpoint": {"host": "127.0.0.1", "port": 5678, "ownerPid": 11,
                         "argv": ["python", "srv.py"], "source": "os-proc"},
            "adapter": {"confidence": "unavailable"},
        }
        st._note_process_event({"name": "srv.py", "systemProcessId": 12,
                                "startMethod": "attach"})
        started = time.time() - 2.0
        ctx = st._wait_context(2, started)
        self.assertEqual(ctx["triggerStatus"], "unknown")
        self.assertNotIn("expectedBreak", ctx)
        self.assertGreaterEqual(ctx["waitedMs"], 1500)
        self.assertIn("not observed", ctx["note"])
        self.assertNotIn("unreachable code", ctx["note"])
        self.assertEqual(ctx["targetIdentity"]["debuggee"]["pid"], 12)
        ctx2 = st._wait_context(2, started, expected_break="srv.py:9")
        self.assertEqual(ctx2["expectedBreak"], "srv.py:9")

    def test_cmd_wait_timeout_carries_wait_context(self):
        st = self.session()
        st.cfg.target_identity_seed = {
            "debuggee": {"kind": "process", "pid": None},
            "endpoint": {"host": "127.0.0.1", "port": 5678, "ownerPid": 11,
                         "argv": ["python", "srv.py"], "source": "os-proc"},
            "adapter": {"confidence": "unavailable"},
        }
        st._build_target_identity()
        ctx = st._wait_context(7, time.time())
        st.pump = Mock(side_effect=bridge.StopTimeout(st.timeout_text(7), ctx))
        with self.assertRaises(bridge.StopTimeout) as cm:
            st.cmd_wait({}, 7)
        self.assertTrue(str(cm.exception).startswith("timeout: no stop within 7s"))
        self.assertEqual(cm.exception.wait_context["triggerStatus"], "unknown")
        self.assertFalse(st.suspended)

    def test_cmd_capture_timeout_carries_expected_break(self):
        st = self.session()
        path = os.path.realpath(os.path.join(st.cfg.dir, "a.py"))
        Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
        st.cfg.target_identity_seed = {
            "debuggee": {"kind": "process", "pid": None},
            "endpoint": {"host": "127.0.0.1", "port": 5678, "ownerPid": 11,
                         "argv": ["python", "a.py"], "source": "os-proc"},
            "adapter": {"confidence": "unavailable"},
        }
        st._build_target_identity()
        st.dap_request = Mock(side_effect=lambda *a, **k: {"breakpoints": [{"verified": True}]})
        ctx = st._wait_context(5, time.time())
        st.pump = Mock(side_effect=bridge.StopTimeout(st.timeout_text(5), ctx))
        spec = f"{path}:5"
        with self.assertRaises(bridge.StopTimeout) as cm:
            st.cmd_capture({"break": spec}, 5)
        wc = cm.exception.wait_context
        self.assertTrue(str(cm.exception).startswith("timeout: no stop within 5s"))
        self.assertEqual(wc["expectedBreak"], spec)
        self.assertEqual(wc["triggerStatus"], "unknown")
        calls = [c[0][0] for c in st.dap_request.call_args_list]
        self.assertNotIn("continue", calls)
        self.assertEqual(st.cfg.breaks, [])

    def test_handle_one_frames_wait_context(self):
        st = self.session()
        left, right = socket.socketpair()
        self.addCleanup(left.close)
        self.addCleanup(right.close)
        ctx = {"waitStartedAt": 1, "waitedMs": 2, "triggerStatus": "unknown",
               "targetIdentity": None, "note": bridge.WAIT_NOTE}
        st.dispatch = Mock(side_effect=bridge.StopTimeout("timeout: no stop within 2s", ctx))
        self.assertTrue(st.server_state.try_admit(bridge.MAX_ACTIVE_HANDLERS))
        t = threading.Thread(target=bridge._handle_one, args=(st, left), daemon=True)
        t.start()
        bridge.write_frame(right, {"cmd": "wait", "timeout": 2})
        right.settimeout(5)
        resp = bridge.read_frame(right)
        t.join(5)
        self.assertFalse(resp["ok"])
        self.assertTrue(resp["error"].startswith("timeout: no stop within 2s"))
        self.assertEqual(resp["waitContext"]["triggerStatus"], "unknown")

    def test_debuggee_os_enrichment_is_best_effort(self):
        st = self.session()
        self.assertIsNone(st._debuggee_os_details(999999999))
        self.assertIsNone(st._debuggee_os_details(None))
        det = st._debuggee_os_details(os.getpid())
        self.assertIsNotNone(det)
        self.assertEqual(det["confidence"], "os-corroborated")
        self.assertNotIn("environ", json.dumps(det))

    @unittest.skipIf(os.name == "nt",
                     "needs a non-Windows host (no windll/tasklist)")
    def test_windows_os_helpers_return_none_off_windows(self):
        # No windll/tasklist here: both helpers must return None, never
        # raise, so non-Windows behavior is byte-identical to before.
        self.assertIsNone(bridge._windows_process_argv(os.getpid()))
        self.assertIsNone(bridge._windows_process_image(os.getpid()))
        self.assertIsNone(bridge._windows_process_argv(999999999))

    @unittest.skipUnless(os.name == "nt",
                         "needs real Windows (tasklist/ctypes)")
    def test_windows_os_helpers_work_on_windows(self):
        # Permissive by design (CI-only): helpers must never raise and
        # must return a sane shape; a live own-pid may corroborate.
        argv = bridge._windows_process_argv(os.getpid())
        self.assertTrue(argv is None or (isinstance(argv, list) and argv))
        self.assertIsNone(bridge._windows_process_argv(999999999))
        img = bridge._windows_process_image(os.getpid())
        self.assertTrue(img is None or isinstance(img, str))
        self.assertIsNone(bridge._windows_process_image(999999999))

    def test_debuggee_os_windows_fallback_wiring(self):
        from unittest.mock import patch
        st = self.session()
        # No /proc (forced: ubuntu CI has one and would short-circuit)
        # and ps forced to fail: the nt branch alone must corroborate,
        # deterministically on every platform.
        no_proc = OSError("no proc here")
        no_ps = OSError("no ps on windows")
        with patch.object(os, "name", "nt"), \
                patch("builtins.open", side_effect=no_proc), \
                patch.object(bridge.subprocess, "run",
                             side_effect=no_ps), \
                patch.object(bridge, "_windows_process_argv",
                             return_value=["python.exe"]), \
                patch.object(bridge, "_windows_process_image",
                             return_value="C:\\Py\\python.exe"):
            det = st._debuggee_os_details(os.getpid())
        self.assertIsNotNone(det)
        self.assertEqual(det["confidence"], "os-corroborated")
        self.assertEqual(det["source"], "os-tasklist")
        self.assertEqual(det["argv"], ["python.exe"])
        self.assertEqual(det["executable"], "C:\\Py\\python.exe")
        self.assertNotIn("environ", json.dumps(det))
        # Dead pid on Windows stays None (liveness check intact).
        with patch.object(os, "name", "nt"), \
                patch("builtins.open", side_effect=no_proc), \
                patch.object(bridge.subprocess, "run",
                             side_effect=no_ps), \
                patch.object(bridge, "_windows_process_argv",
                             return_value=None), \
                patch.object(bridge, "_windows_process_image",
                             return_value=None):
            self.assertIsNone(st._debuggee_os_details(999999999))

    def test_windows_tasklist_csv_parsing(self):
        from unittest.mock import patch
        fake = ('"Image Name","PID","Session Name","Session#","Mem Usage"\r\n'
                '"python.exe","1234","Console","1","45,000 K"\r\n')
        run = Mock(return_value=SimpleNamespace(stdout=fake))
        with patch.object(os, "name", "nt"), patch.object(
                bridge.subprocess, "run", run):
            self.assertEqual(bridge._windows_process_argv(1234),
                             ["python.exe"])
            self.assertIsNone(bridge._windows_process_argv(9999))
        with patch.object(os, "name", "nt"), patch.object(
                bridge.subprocess, "run",
                Mock(return_value=SimpleNamespace(
                    stdout="INFO: No tasks are running\r\n"))):
            self.assertIsNone(bridge._windows_process_argv(1234))

    # ---- setup-failure phase (error.json `phase`) ----
    # Typed, never message-matched: Usage/ConfigErr read as config, every
    # other failure (socket loss, timeout, target exit, unexpected) stays
    # transport. A successful connection never flips later failures.

    def test_phase_of_error_types(self):
        self.assertEqual(bridge.phase_of_error(bridge.Usage("x")), "config")
        self.assertEqual(bridge.phase_of_error(bridge.ConfigErr("x")), "config")
        # ConfigErr is still a BridgeErr: existing catches keep working.
        self.assertIsInstance(bridge.ConfigErr("x"), bridge.BridgeErr)
        # Explicit runtime marker reads as runtime (truthful internal
        # error, never endpoint-diagnosed).
        self.assertEqual(bridge.phase_of_error(bridge.RuntimeErr("bug")), "runtime")
        # Unexpected exceptions after a successful operation read as
        # runtime, not transport.
        for e in [ValueError("bug"), RuntimeError("boom")]:
            self.assertEqual(bridge.phase_of_error(e), "runtime",
                             f"{e!r} must read runtime")
        for e in [bridge.BridgeErr("boom"),
                  bridge.StopTimeout("timeout: no stop within 2s"),
                  None, object(), "config"]:
            self.assertEqual(bridge.phase_of_error(e), "transport",
                             f"{e!r} must stay transport")

    def test_setup_error_payload_from_exception(self):
        self.assertEqual(
            bridge.setup_error_payload(bridge.BridgeErr("boom"), "boom"),
            {"schemaVersion": 2, "error": "boom", "phase": "transport"})
        self.assertEqual(
            bridge.setup_error_payload(
                bridge.ConfigErr("bad cond"), "bad cond"),
            {"schemaVersion": 2, "error": "bad cond", "phase": "config"})
        self.assertEqual(
            bridge.setup_error_payload(
                bridge.Usage("no such line: x"), "no such line: x"),
            {"schemaVersion": 2, "error": "no such line: x", "phase": "config"})

    def test_handshake_attach_refused_stays_transport(self):
        # Nothing listens: socket/connect failure, phase transport, exact
        # message preserved for the CLI's cause.
        cfg = bridge.Config()
        cfg.kind, cfg.host = "attach", "127.0.0.1"
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            cfg.port = s.getsockname()[1]
        st = bridge.Session(cfg)
        with self.assertRaises(bridge.BridgeErr) as cm:
            st.handshake_attach()
        self.assertIn("attach failed", str(cm.exception))
        self.assertNotIsInstance(cm.exception, bridge.ConfigErr)
        self.assertEqual(
            bridge.setup_error_payload(cm.exception, str(cm.exception))["phase"],
            "transport")

    def test_dap_refusal_vs_transport_loss(self):
        # A valid adapter error response is semantic (ConfigErr) only with
        # the arm opt-in; socket timeouts and closed connections stay
        # transport regardless.
        refusal = {"type": "response", "request_seq": 1,
                   "success": False, "message": "bad condition"}
        conn = bridge.DapConn(Mock())
        conn.stash = [dict(refusal)]
        with self.assertRaises(bridge.ConfigErr):
            conn.request("setBreakpoints", {}, timeout=2, semantic=True)
        # Same refusal without the arm opt-in (initialize/attach/drain):
        # transport, so endpoint diagnosis is kept.
        conn.seq = 0
        conn.stash = [dict(refusal)]
        with self.assertRaises(bridge.BridgeErr) as cm:
            conn.request("setBreakpoints", {}, timeout=2)
        self.assertNotIsInstance(cm.exception, bridge.ConfigErr)
        # Timeout and closed-connection losses stay transport even armed
        # (exercised through dap_request, which wraps raw socket errors).
        st = self.session()
        st.dap = conn
        conn.stash = []
        conn.sock.recv = Mock(side_effect=socket.timeout())
        with self.assertRaises(bridge.BridgeErr) as cm:
            st.dap_request("setBreakpoints", {}, timeout=5, semantic=True)
        self.assertNotIsInstance(cm.exception, bridge.ConfigErr)

    def test_handshake_attach_arm_failure_types(self):
        # Connected adapter, arming fails: a semantic refusal reports
        # config; a transport loss during arming reports transport.
        from unittest.mock import patch
        for exc, want in [(bridge.ConfigErr("bad condition"), "config"),
                          (bridge.BridgeErr("DAP connection closed"), "transport")]:
            cfg = bridge.Config()
            cfg.kind, cfg.host, cfg.port = "attach", "127.0.0.1", 1
            st = bridge.Session(cfg)
            st.dap_request = Mock(return_value={})
            st.arm_breakpoints = Mock(side_effect=exc)
            with patch.object(bridge.socket, "create_connection",
                              return_value=Mock()), \
                 patch.object(bridge, "DapConn", return_value=Mock()):
                with self.assertRaises(bridge.BridgeErr):
                    st.handshake_attach()
            payload = bridge.setup_error_payload(exc, str(exc))
            self.assertEqual(payload["phase"], want, f"{exc!r}")
            self.assertEqual(payload["error"], str(exc))

    def test_pump_target_exit_stays_transport(self):
        # Initial-pump target exit is a transport loss, never semantic —
        # even though the connection was established. Runtime must not
        # mask it: only unexpected exceptions read as runtime.
        exc = bridge.BridgeErr("target exited")
        self.assertEqual(bridge.phase_of_error(exc), "transport")
        self.assertEqual(
            bridge.setup_error_payload(exc, str(exc))["phase"], "transport")

    def test_unexpected_setup_crash_reads_runtime(self):
        # Synthetic unexpected failure after a successful operation:
        # sanitized payload, runtime phase (truthful internal error, no
        # endpoint diagnosis). Target exits stay transport (above). The
        # body goes through format_unexpected exactly as the production
        # setup catch does (bridge main, `except Exception` path) — for
        # RuntimeErr too, which is a plain Exception, not a BridgeErr.
        for exc in [bridge.RuntimeErr("track boom"), ValueError("bug")]:
            body = bridge.format_unexpected(exc)
            self.assertTrue(body.startswith("internal:"),
                            f"{exc!r} must sanitize, got: {body[:60]}")
            payload = bridge.setup_error_payload(exc, body)
            self.assertEqual(payload["phase"], "runtime", f"{exc!r}")
            self.assertEqual(payload["schemaVersion"], 2)
            self.assertEqual(payload["error"], body)

    def test_dir_from_argv_scans_without_parsing(self):
        self.assertEqual(
            bridge.dir_from_argv(["session", "--dir", "/s", "--break", "a:1"]),
            "/s")
        self.assertEqual(
            bridge.dir_from_argv(["session", "--dir=/s"]),
            "/s")
        self.assertIsNone(bridge.dir_from_argv(["session", "--break", "a:1"]))
        self.assertIsNone(bridge.dir_from_argv(["session", "--dir"]))
        # Empty --dir= scans as empty and writes nothing (never the cwd).
        self.assertEqual(bridge.dir_from_argv(["session", "--dir="]), "")
        bridge.write_parse_error(["session", "--dir="], "x")

    def test_write_parse_error_is_config_and_best_effort(self):
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            bridge.write_parse_error(
                ["session", "--dir", d, "--break", "x"],
                "no such line: x")
            body = json.loads(Path(d, "error.json").read_text())
            self.assertEqual(body["error"], "no such line: x")
            self.assertEqual(body["phase"], "config")
        # No --dir, bad dir, or empty argv: never throws, nothing written.
        bridge.write_parse_error(["session"], "x")
        bridge.write_parse_error([], "x")
        bridge.write_parse_error(
            ["session", "--dir", "/definitely/not/a/real/dir/xyz"], "x")


if __name__ == "__main__":
    unittest.main()
