"""Unit tests for the wait/capture/diagnostics UX batch (pybridge)."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import Mock

ROOT = Path(__file__).resolve().parents[1]
_spec = importlib.util.spec_from_file_location("pybridge_ux", ROOT / "bridge/py/src/pybridge.py")
_bridge = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_bridge)


def _session(tmp, breaks=()):
    cfg = _bridge.Config()
    st = _bridge.Session(cfg)
    st.cfg.dir = tmp
    st.cfg.breaks = list(breaks)
    st.dap_request = Mock()
    return st


def _live_dap(path, line=5, tid=1):
    """Fake DAP answering stack/threads/scopes/variables for a park."""
    def fake(command, args=None, timeout=30):
        if command == "stackTrace":
            return {"stackFrames": [{"id": 11, "name": "handler",
                                     "source": {"path": path}, "line": line}]}
        if command == "threads":
            return {"threads": [{"id": tid, "name": "main"}]}
        if command == "scopes":
            return {"scopes": [{"name": "Locals", "variablesReference": 1}]}
        if command == "variables":
            return {"variables": [{"name": "x", "type": "int",
                                   "value": "1", "variablesReference": 0}]}
        if command == "setBreakpoints":
            return {"breakpoints": [{"verified": True, "line": line}]}
        if command == "continue":
            return {}
        raise AssertionError(f"unexpected DAP: {command}")
    return fake


def _park(st, path, line=5, reason="breakpoint", tid=1):
    st.thread_id = None
    st.suspended = False
    st.dap_request.side_effect = _live_dap(path, line, tid)
    st._park_stop(reason, tid)


class WaitCaptureTests(unittest.TestCase):
    def test_capture_bounds_rejected_before_dap(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = _session(tmp)
            for req in [{"frames": 0}, {"frames": 11}, {"vars": 0},
                        {"vars": 21}, {"pauseBudgetMs": 0},
                        {"pauseBudgetMs": 10001}, {"break": ""},
                        {"break": 42}]:
                with self.assertRaises(_bridge.BridgeErr):
                    st.cmd_capture(req, 5)
            st.dap_request.assert_not_called()

    def test_wait_immediate_parked_never_resumes(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            _park(st, path)
            st.dap_request.side_effect = _live_dap(path)
            st.dap_request.reset_mock()
            resp = st.cmd_wait({}, 5)
            self.assertTrue(resp["ok"])
            self.assertFalse(resp["waited"])
            self.assertEqual(resp["target"], "main")
            calls = [c[0][0] for c in st.dap_request.call_args_list]
            self.assertNotIn("continue", calls)
            self.assertIn("warning", resp)
            self.assertIn("HTTP handler remains open", resp["warning"])
            diag = resp["diag"]
            self.assertEqual(diag["target"], "main")
            self.assertEqual(diag["reason"], "breakpoint")
            self.assertEqual(diag["stoppingThread"]["id"], 1)
            self.assertIsNone(diag["hitBreakpoints"])
            self.assertTrue(st.suspended)  # still parked

    def test_wait_fresh_park_never_resumes(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            st.dap_request.side_effect = _live_dap(path)

            def fake_pump(timeout):
                st._park_stop("breakpoint", 1)
            st.pump = fake_pump
            resp = st.cmd_wait({}, 5)
            self.assertTrue(resp["waited"])
            calls = [c[0][0] for c in st.dap_request.call_args_list]
            self.assertNotIn("continue", calls)

    def test_wait_timeout_preserves_session(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = _session(tmp)
            st.dap_request.side_effect = _live_dap("/nonexistent.py")

            def fake_pump(timeout):
                raise _bridge.StopTimeout(st.timeout_text(timeout))
            st.pump = fake_pump
            with self.assertRaises(_bridge.StopTimeout) as cm:
                st.cmd_wait({}, 7)
            self.assertIn("timeout: no stop within 7s", str(cm.exception))
            self.assertFalse(st.suspended)

    def test_capture_prepark_collects_without_resume(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            _park(st, path)
            st.dap_request.side_effect = _live_dap(path)
            st.dap_request.reset_mock()
            resp = st.cmd_capture({"frames": 2, "vars": 3}, 5)
            self.assertTrue(resp["targetWasPaused"])
            self.assertFalse(resp["resumed"])
            self.assertEqual(resp["pauseDurationMs"], 0)
            self.assertLessEqual(len(resp["snapshot"]["frames"]), 2)
            calls = [c[0][0] for c in st.dap_request.call_args_list]
            self.assertNotIn("continue", calls)
            self.assertNotIn("setBreakpoints", calls)
            self.assertTrue(st.suspended)  # pre-existing park untouched

    def test_capture_fresh_removes_ephemeral_before_resume(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            order = []

            def fake(command, args=None, timeout=30):
                order.append(command)
                return _live_dap(path)(command, args, timeout)
            st.dap_request.side_effect = fake

            def fake_pump(timeout):
                st._park_stop("breakpoint", 1)
            st.pump = fake_pump
            resp = st.cmd_capture({"break": f"{path}:5", "pauseBudgetMs": 2000}, 5)
            self.assertFalse(resp["targetWasPaused"])
            self.assertTrue(resp["resumed"])
            self.assertTrue(resp["ephemeralPlanted"])
            self.assertIn("pauseDurationMs", resp)
            self.assertIn("budgetExceeded", resp)
            plant = order.index("setBreakpoints")
            cont = order.index("continue")
            # The last setBreakpoints (ephemeral removal) precedes continue.
            unplant = len(order) - 1 - order[::-1].index("setBreakpoints")
            self.assertLess(plant, unplant)
            self.assertLess(unplant, cont)
            self.assertNotIn("removeError", resp)
            self.assertFalse(st.suspended)  # resumed
            # Ephemeral left no intent behind.
            self.assertEqual(st.cfg.breaks, [])

    def test_capture_timeout_removes_ephemeral_without_resume(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            st.dap_request.side_effect = _live_dap(path)

            def fake_pump(timeout):
                raise _bridge.StopTimeout(st.timeout_text(timeout))
            st.pump = fake_pump
            with self.assertRaises(_bridge.StopTimeout):
                st.cmd_capture({"break": f"{path}:5"}, 5)
            calls = [c[0][0] for c in st.dap_request.call_args_list]
            self.assertNotIn("continue", calls)
            # Plant + unplant, nothing persisted.
            self.assertEqual(calls.count("setBreakpoints"), 2)
            self.assertEqual(st.cfg.breaks, [])

    def test_capture_collection_failure_still_resumes(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            st.dap_request.side_effect = _live_dap(path)

            def fake_pump(timeout):
                st._park_stop("breakpoint", 1)
            st.pump = fake_pump
            st._bounded_snapshot = Mock(side_effect=RuntimeError("boom"))
            resp = st.cmd_capture({}, 5)
            self.assertTrue(resp["resumed"])
            self.assertIn("snapshotError", resp)
            calls = [c[0][0] for c in st.dap_request.call_args_list]
            self.assertIn("continue", calls)

    def test_wait_occupies_slot_rival_resume_busy(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = _session(tmp)
            with st._gate:
                st._outstanding["main"] = "wait"
            with self.assertRaises(_bridge.BridgeErr) as cm:
                st.dispatch({"cmd": "continue", "timeout": 5})
            self.assertIn("busy", str(cm.exception))

    def test_diag_same_location_second_park(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            _park(st, path, line=5)
            first = dict(st._last_diag)
            st.suspended = False  # resumed between parks
            _park(st, path, line=5)
            second = st._last_diag
            self.assertEqual(second["stopId"], first["stopId"] + 1)
            self.assertTrue(second["sameLocation"])
            self.assertTrue(second["sameThread"])
            self.assertGreaterEqual(second["elapsedMs"], 0)
            st.suspended = False
            _park(st, path, line=6)
            third = st._last_diag
            self.assertFalse(third["sameLocation"])

    def test_diag_attribution_matches_bound_line(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp, breaks=[(path, 5, None)])
            st.stop_states = [{"spec": "a.py:5", "kind": "break",
                               "state": "verified", "hits": 3}]
            st._hitkeys = [("break", path, 5, 5)]
            _park(st, path, line=5)
            diag = st._stop_diag("main", [{"id": 1, "name": "main"}])
            self.assertEqual(diag["requestedBreak"], "a.py:5")
            self.assertEqual(diag["boundLine"], 5)
            self.assertEqual(diag["hitCount"], 4)  # park counted one hit


if __name__ == "__main__":
    unittest.main()
