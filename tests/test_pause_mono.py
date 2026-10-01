"""pauseDurationMs / elapsedMs are monotonic durations, not wall clock.

Pre-fix, the park diagnostic and the capture-exit path subtracted
time.time() readings. A backward wall-clock step (NTP correction)
between the park and the measurement then reported a negative/hours-long
pause and could flip budgetExceeded. Reported timestamps (parkedAtMs,
updatedAt) stay wall clock; only durations moved to mono().
"""
import importlib.util
import os
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[1]
_spec = importlib.util.spec_from_file_location("pybridge_pausemono", ROOT / "bridge/py/src/pybridge.py")
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


class PauseMonoTests(unittest.TestCase):
    def test_park_elapsed_monotonic(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            _park(st, path)
            time.sleep(0.03)
            # Second park under a stepped-back wall clock.
            st.thread_id = None
            st.suspended = False
            st.dap_request.side_effect = _live_dap(path)
            real = time.time
            with patch("time.time", side_effect=lambda: real() - 3600):
                st._park_stop("breakpoint", 1)
            e = st._last_diag["elapsedMs"]
            self.assertIsNotNone(e)
            self.assertGreaterEqual(e, 0)
            self.assertLess(e, 60000)

    def test_capture_pause_monotonic(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            order = []

            def fake(command, args=None, timeout=30):
                order.append(command)
                return _live_dap(path)(command, args, timeout)
            st.dap_request.side_effect = fake

            real = time.time

            def fake_pump(timeout):
                st._park_stop("breakpoint", 1)  # real clock ...
                # ... then the wall steps back for the rest of capture.
                p = patch("time.time", side_effect=lambda: real() - 3600)
                p.start()
                self.addCleanup(p.stop)
            st.pump = fake_pump
            resp = st.cmd_capture({"break": f"{path}:5", "pauseBudgetMs": 2000}, 5)
            self.assertGreaterEqual(resp["pauseDurationMs"], 0)
            self.assertLess(resp["pauseDurationMs"], 60000)
            self.assertFalse(resp["budgetExceeded"])


if __name__ == "__main__":
    unittest.main()
