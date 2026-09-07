"""Milestone C: terminal close under full handler load (Python).

All eight handler slots are held by blocking commands through the real
serve() loop: the ninth ordinary command must get the existing overloaded
rejection, while an exact `close` on a tenth connection still terminates
(closed ACK, teardown runs once) instead of starving behind the pool.
"""
import importlib.util
import socket
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import Mock

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location(
    "pybridge_close_load", ROOT / "bridge/py/src/pybridge.py")
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)


def client_roundtrip(port, req, timeout=10):
    s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
    try:
        bridge.write_frame(s, req)
        return bridge.read_frame(s)
    finally:
        s.close()


class CloseUnderLoadTests(unittest.TestCase):
    def serve_session(self, tmp):
        st = bridge.Session(bridge.Config())
        st.cfg.dir = tmp
        st._nonce = bridge.write_owner(tmp)
        st.publish_state = Mock()
        return st

    def test_pool_full_close_ack_and_ordinary_overloaded(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.serve_session(tmp)
            blocker = threading.Event()

            def fake_dispatch(req):
                if req.get("cmd") == "close":
                    raise bridge._Close()
                self.assertTrue(blocker.wait(15), "handler must stay held")
                return {"ok": True}

            st.dispatch = fake_dispatch
            server = socket.socket()
            server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            server.bind(("127.0.0.1", 0))
            server.listen(16)
            port = server.getsockname()[1]
            srv = threading.Thread(target=bridge.serve,
                                   args=(st, server, st._nonce),
                                   daemon=True)
            srv.start()
            # Occupy every handler slot with a blocking command.
            results = [None] * 8

            def run(i):
                try:
                    results[i] = client_roundtrip(port, {"cmd": "noop"})
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
            # Ninth ordinary command: existing overloaded rejection, fast.
            t0 = time.monotonic()
            ninth = client_roundtrip(port, {"cmd": "threads"})
            self.assertLess(time.monotonic() - t0, 5.0)
            self.assertFalse(ninth["ok"])
            self.assertIn("overloaded", ninth["error"])
            self.assertEqual(ninth.get("target"), "main")
            with st._gate:
                self.assertEqual(st.server_state.active, 8)
            # Tenth connection with an exact close: terminal ACK outside
            # the pool — close is never starved by admitted handlers.
            tenth = client_roundtrip(port, {"cmd": "close"})
            self.assertTrue(tenth.get("closed"), tenth)
            self.assertTrue(tenth.get("ok"), tenth)
            with st._gate:
                self.assertTrue(st.server_state.is_closing())
                self.assertEqual(st.server_state.active, 8)
            blocker.set()
            for t in ths:
                t.join(5)
            for r in results:
                self.assertIsInstance(r, dict, r)
                self.assertTrue(r["ok"], r)
            srv.join(5)
            self.assertFalse(srv.is_alive())

    def test_close_from_conn_idempotent_single_cleanup(self):
        # Direct unit seam: two closes both ACK; teardown runs exactly once.
        with tempfile.TemporaryDirectory() as tmp:
            st = self.serve_session(tmp)
            st.cleanup = Mock()
            st.server = Mock()
            self.assertEqual(st.server_state.active, 0)
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
            self.assertEqual(st.server_state.active, 0, "pool counter untouched")


if __name__ == "__main__":
    unittest.main()
