"""Milestone C: request-local error target attribution (Python).

A sibling handler holds WRONG shared serving state (another target's
scope) for the whole duration of a failing request served by `_handle_one`
over a real socketpair. The failing envelope must name THIS request's
target — the explicit raw string, or this request's own resolution —
never the sibling's shared `_pending_target`/`_serving`. Barrier-ordered,
no timing dependence.
"""
import importlib.util
import socket
import tempfile
import threading
import unittest
from pathlib import Path
from unittest.mock import Mock

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location(
    "pybridge_attribution", ROOT / "bridge/py/src/pybridge.py")
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)


class ErrorAttributionTests(unittest.TestCase):
    def session(self, tmp):
        cfg = bridge.Config()
        cfg.dir = tmp
        st = bridge.Session(cfg)
        st.publish_state = Mock()
        return st

    def hold_sibling_on_child(self, st):
        """Park a sibling handler on WRONG shared state until released."""
        entered = threading.Event()
        release = threading.Event()

        def fake_dispatch(req):
            if req.get("marker") == "sibling":
                st.targets_reg.serving = "child:61"
                st._pending_target = "child:61"
                entered.set()
                self.assertTrue(release.wait(10), "sibling must stay held")
                return {"ok": True}
            raise bridge.BridgeErr(
                f"unknown target: {req.get('target')}")

        st.dispatch = fake_dispatch
        sib_srv, sib_cli = socket.socketpair()
        bridge.write_frame(sib_cli, {"cmd": "threads", "marker": "sibling"})
        t = threading.Thread(target=bridge._handle_one, args=(st, sib_srv),
                             daemon=True)
        self.assertTrue(st.server_state.try_admit(bridge.MAX_ACTIVE_HANDLERS))  # serve() owns the increment; direct _handle_one
        t.start()
        self.assertTrue(entered.wait(10), "sibling never held state")
        return t, sib_cli, release

    def run_failing(self, st, req):
        srv, cli = socket.socketpair()
        try:
            bridge.write_frame(cli, req)
            self.assertTrue(st.server_state.try_admit(bridge.MAX_ACTIVE_HANDLERS))
            bridge._handle_one(st, srv)
            cli.settimeout(5)
            return bridge.read_frame(cli)
        finally:
            cli.close()

    def test_explicit_unknown_target_names_raw_request(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.session(tmp)
            t, sib_cli, release = self.hold_sibling_on_child(st)
            try:
                resp = self.run_failing(
                    st, {"cmd": "threads", "target": "child:99"})
                self.assertFalse(resp["ok"])
                self.assertIn("unknown target", resp["error"])
                self.assertEqual(resp["target"], "child:99",
                                 "must name the requested raw target, "
                                 "not the sibling's child:61")
            finally:
                sib_cli.close()
                release.set()
                t.join(5)

    def test_omitted_target_uses_own_resolution(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = self.session(tmp)

            def fake_dispatch(req):
                if req.get("marker") == "sibling":
                    st.targets_reg.serving = "child:61"
                    st._pending_target = "child:61"
                    return {"ok": True}
                raise bridge.BridgeErr("no stopped thread")

            st.dispatch = fake_dispatch
            # Sibling holds the wrong state synchronously (no barrier
            # needed: the failing call runs entirely under it).
            st.targets_reg.serving = "child:61"
            st._pending_target = "child:61"
            try:
                resp = self.run_failing(st, {"cmd": "eval", "expr": "1+1"})
            finally:
                st.targets_reg.serving = "main"
                st._pending_target = None
            self.assertFalse(resp["ok"])
            # Omitted target resolves for THIS request (nothing parked ->
            # main), never the sibling's scope.
            self.assertEqual(resp["target"], "main")


if __name__ == "__main__":
    unittest.main()
