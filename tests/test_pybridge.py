"""Deterministic regressions for DAP ordering and stopped-state handling."""
import importlib.util
import json
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
