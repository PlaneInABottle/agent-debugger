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

    def test_capture_truncated_vars_propagated(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            many = [{"name": f"v{n}", "type": "int", "value": str(n),
                     "variablesReference": 0} for n in range(10)]

            def fake(command, args=None, timeout=30):
                if command == "stackTrace":
                    return {"stackFrames": [{"id": 11, "name": "handler",
                                             "source": {"path": path},
                                             "line": 5}]}
                if command == "threads":
                    return {"threads": [{"id": 1, "name": "main"}]}
                if command == "scopes":
                    return {"scopes": [{"name": "Locals",
                                        "variablesReference": 1}]}
                if command == "variables":
                    return {"variables": many}
                raise AssertionError(f"unexpected DAP: {command}")
            st.thread_id = None
            st.suspended = False
            st.dap_request.side_effect = fake
            st._park_stop("breakpoint", 1)
            capped = st.cmd_capture({"frames": 2, "vars": 3}, 5)
            self.assertTrue(capped["truncated"]["vars"])
            locs = capped["snapshot"]["frames"][0]["locals"]
            self.assertEqual(locs[-1]["name"], "…")
            # Uncapped fits: stays false.
            roomy = st.cmd_capture({"frames": 2, "vars": 20}, 5)
            self.assertFalse(roomy["truncated"]["vars"])

    def test_capture_exit_preserves_original_error_with_remove_warning(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            st.dap_request.side_effect = _live_dap(path)

            def fake_pump(timeout):
                raise _bridge.BridgeErr("target exited")
            st.pump = fake_pump
            st._capture_unplant = Mock(side_effect=RuntimeError("remove boom"))
            with self.assertRaises(_bridge.BridgeErr) as cm:
                st.cmd_capture({"break": f"{path}:5"}, 5)
            msg = str(cm.exception)
            self.assertIn("target exited", msg)  # original never masked
            self.assertIn("remove boom", msg)  # removal failure attached
            # The exit stage survives the removal failure (dead adapter).
            self.assertIn("target exited before capture hit", msg)
            self.assertEqual(
                cm.exception.wait_context["captureStage"], "armed-wait")

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

    # ---- change-tracking resilience (>MAX_VARS sentinel + malformed) ----

    def _many_vars_dap(self, path, n=25, line=5, tid=1, extra=()):
        """Fake DAP with n locals (over MAX_VARS=20, so frame_locals caps
        with the value-less {"name": "…", "note": "+N more"} sentinel)
        plus caller-supplied malformed entries."""
        many = [{"name": f"v{k}", "type": "int", "value": str(k),
                 "variablesReference": 0} for k in range(n)]
        many.extend(extra)

        def fake(command, args=None, timeout=30):
            if command == "stackTrace":
                return {"stackFrames": [{"id": 11, "name": "handler",
                                         "source": {"path": path},
                                         "line": line}]}
            if command == "threads":
                return {"threads": [{"id": tid, "name": "main"}]}
            if command == "scopes":
                return {"scopes": [{"name": "Locals",
                                    "variablesReference": 1}]}
            if command == "variables":
                return {"variables": many}
            if command == "setBreakpoints":
                return {"breakpoints": [{"verified": True, "line": line}]}
            if command == "continue":
                return {}
            raise AssertionError(f"unexpected DAP: {command}")
        return fake

    def _vars_dap(self, path, get_vars, line=5, tid=1, name="handler"):
        """Fake DAP with a caller-supplied variables list (fresh per call,
        so two parks can observe changed values)."""
        def fake(command, args=None, timeout=30):
            if command == "stackTrace":
                return {"stackFrames": [{"id": 11, "name": name,
                                         "source": {"path": path},
                                         "line": line}]}
            if command == "threads":
                return {"threads": [{"id": tid, "name": "main"}]}
            if command == "scopes":
                return {"scopes": [{"name": "Locals",
                                    "variablesReference": 1}]}
            if command == "variables":
                return {"variables": get_vars()}
            if command == "setBreakpoints":
                return {"breakpoints": [{"verified": True, "line": line}]}
            if command == "continue":
                return {}
            raise AssertionError(f"unexpected DAP: {command}")
        return fake

    def _repark(self, st):
        st.thread_id = None
        st.suspended = False
        st._park_stop("breakpoint", 1)

    def _locals_globals_dap(self, path, locals_vars, globals_vars,
                            line=5, tid=1, name="handler"):
        """Fake DAP with both scopes: Locals (ref 1) plus Globals (ref 7)
        hiding names one pseudo level deeper (ref 8, display shape)."""
        def fake(command, args=None, timeout=30):
            if command == "stackTrace":
                return {"stackFrames": [{"id": 11, "name": name,
                                         "source": {"path": path},
                                         "line": line}]}
            if command == "threads":
                return {"threads": [{"id": tid, "name": "main"}]}
            if command == "scopes":
                return {"scopes": [{"name": "Locals",
                                     "variablesReference": 1},
                                    {"name": "Globals",
                                     "variablesReference": 7}]}
            if command == "variables":
                ref = (args or {}).get("variablesReference")
                if ref == 1:
                    return {"variables": locals_vars()}
                if ref == 7:
                    return {"variables": [
                        {"name": "function variables",
                         "variablesReference": 8},
                        {"name": "special variables",
                         "variablesReference": 9}]}
                if ref == 8:
                    return {"variables": globals_vars()}
                if ref == 9:
                    return {"variables": []}
                raise AssertionError(f"unexpected ref: {ref}")
            if command == "setBreakpoints":
                return {"breakpoints": [{"verified": True, "line": line}]}
            if command == "continue":
                return {}
            raise AssertionError(f"unexpected DAP: {command}")
        return fake

    def test_track_locals_empty_falls_back_to_globals(self):
        """Locals exists but yields nothing: tracking mirrors the display
        fallback and scans Globals (one pseudo level), with total/scan
        reflecting the chosen scope only (no double count). A global
        changing outside the 20-var display window is still detected,
        complete."""
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            state = {"g20": "0"}
            def globals_vars():
                out = [{"name": f"g{k:02d}", "type": "int",
                        "value": (state["g20"] if k == 20 else str(k)),
                        "variablesReference": 0}
                       for k in range(25)]
                return out
            st.dap_request.side_effect = self._locals_globals_dap(
                path, lambda: [], globals_vars)
            self._repark(st)
            self.assertEqual(sorted(st.last_top),
                             [f"g{k:02d}" for k in range(25)])
            self.assertEqual(st.last_change_tracking["total"], 25)
            self.assertFalse(st.last_changed_complete)
            state["g20"] = "1"
            self._repark(st)
            self.assertEqual(json.loads(st.last_changed), ["g20"])
            self.assertEqual(json.loads(st.last_removed), [])
            self.assertTrue(st.last_changed_complete)
            # Display fell back the same way and stays capped.
            locs = st.frame_locals(0)
            self.assertEqual(len(locs), 21)
            self.assertEqual(locs[-1]["name"], "…")

    def test_track_locals_all_pseudo_falls_back_to_globals(self):
        """Locals holding only pseudo containers / malformed records
        (zero valid tracking entries) falls back to Globals too."""
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            locals_vars = lambda: [
                {"name": "function variables", "variablesReference": 8},
                {"name": 42, "value": "x"},
                {"name": "novalue", "type": "int"},
                None,
            ]
            st.dap_request.side_effect = self._locals_globals_dap(
                path, locals_vars,
                lambda: [{"name": "g", "type": "int", "value": "1",
                           "variablesReference": 0}])
            self._repark(st)
            self.assertEqual(sorted(st.last_top), ["g"])
            self.assertEqual(st.last_change_tracking["total"], 1)
            self._repark(st)
            self.assertEqual(json.loads(st.last_changed), [])
            self.assertTrue(st.last_changed_complete)

    def test_track_single_warning_on_failure(self):
        """Exactly one class-only stderr warning per tracking failure,
        on both the track_changes path and the _park_stop backstop."""
        import io
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            st.frames = [{"id": 11, "name": "handler",
                          "source": {"path": path}, "line": 5}]
            st.dap_request.side_effect = _bridge.BridgeErr("boom")
            buf = io.StringIO()
            with patch("sys.stderr", buf):
                st.track_changes()
            warns = [l for l in buf.getvalue().splitlines()
                     if "change tracking degraded" in l]
            self.assertEqual(len(warns), 1, buf.getvalue())
            self.assertNotIn("boom", buf.getvalue())
            self.assertIsNone(st.last_change_tracking["total"])
            # Backstop: a total track_changes failure inside _park_stop
            # also warns exactly once (class-only).
            st2 = _session(tmp)
            st2.dap_request.side_effect = _live_dap(path)
            st2.thread_id = None
            st2.suspended = False
            st2.track_changes = Mock(side_effect=RuntimeError("synthetic"))
            buf2 = io.StringIO()
            with patch("sys.stderr", buf2):
                st2._park_stop("breakpoint", 1)
            warns2 = [l for l in buf2.getvalue().splitlines()
                      if "change tracking degraded" in l]
            self.assertEqual(len(warns2), 1, buf2.getvalue())
            self.assertNotIn("synthetic", buf2.getvalue())
            self.assertIsNone(st2.last_change_tracking["total"])

    def test_track_changes_skips_sentinel_and_malformed(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            malformed = [
                {"name": "…", "note": "+5 more"},  # display-shaped sentinel
                {"name": "novalue", "type": "int"},  # value-less entry
                {"name": 42, "value": "x"},  # non-string name
                {"value": "orphan"},  # nameless entry
                "not-a-dict",
                None,
            ]
            st.dap_request.side_effect = self._many_vars_dap(
                path, n=5, extra=malformed)
            st.thread_id = None
            st.suspended = False
            st._park_stop("breakpoint", 1)  # must not raise KeyError
            self.assertTrue(st.suspended)
            # First baseline: no previous snapshot means no change
            # comparison — changed stays [] with first-snapshot status
            # (unknown, never "all locals changed").
            self.assertEqual(json.loads(st.last_changed), [])
            self.assertEqual(json.loads(st.last_removed), [])
            self.assertFalse(st.last_changed_complete)
            tr = st.last_change_tracking
            self.assertFalse(tr["complete"])
            self.assertEqual(tr["reason"], "first-snapshot")
            self.assertEqual(tr["scanned"], 5)
            self.assertEqual(tr["total"], 5)
            self.assertFalse(tr["truncated"])
            self.assertIn("trackingWarning", st._change_fields())
            # The raw scan saw exactly the 5 real locals (sentinel and
            # malformed records never enter tracking or the baseline).
            self.assertEqual(sorted(st.last_top), [f"v{k}" for k in range(5)])
            # Direct unit filter: display-shaped entries with a value-less
            # sentinel and malformed records degrade safely (no crash).
            st2 = _session(tmp)
            st2.frames = [{"name": "handler"}]
            st2.track_snapshot = Mock(return_value=(
                {"a": "1"}, 1, False, None))
            st2.track_changes()
            self.assertEqual(json.loads(st2.last_changed), [])
            self.assertEqual(st2.last_change_tracking["reason"],
                             "first-snapshot")

    def test_track_changes_second_stop_detects_change_and_removal(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            state = {"vars": [{"name": "a", "type": "int", "value": "1",
                               "variablesReference": 0},
                              {"name": "gone", "type": "int", "value": "9",
                               "variablesReference": 0}]}
            st.dap_request.side_effect = self._vars_dap(
                path, lambda: [dict(v) for v in state["vars"]])
            self._repark(st)
            self.assertFalse(st.last_changed_complete)
            state["vars"] = [{"name": "a", "type": "int", "value": "2",
                              "variablesReference": 0},
                             {"name": "b", "type": "int", "value": "3",
                              "variablesReference": 0}]
            self._repark(st)
            self.assertEqual(json.loads(st.last_changed), ["a", "b"])
            self.assertEqual(json.loads(st.last_removed), ["gone"])
            self.assertTrue(st.last_changed_complete)
            tr = st.last_change_tracking
            self.assertTrue(tr["complete"])
            self.assertNotIn("reason", tr)
            self.assertEqual(tr["scanned"], 2)
            self.assertEqual(tr["total"], 2)
            self.assertNotIn("trackingWarning", st._change_fields())
            # No-op third stop: complete with empty changed (real no-change).
            self._repark(st)
            self.assertEqual(json.loads(st.last_changed), [])
            self.assertEqual(json.loads(st.last_removed), [])
            self.assertTrue(st.last_changed_complete)

    def test_track_changes_outside_display_window(self):
        """30 locals with the mutation past the MAX_VARS=20 display cap:
        the display stays capped + sentinel while tracking still reports
        the change, complete."""
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            state = {"total": "11"}
            def get_vars():
                out = [{"name": f"v{k}", "type": "int",
                        "value": str(k), "variablesReference": 0}
                       for k in range(30)]
                out.append({"name": "total", "type": "int",
                            "value": state["total"],
                            "variablesReference": 0})
                return out
            st.dap_request.side_effect = self._vars_dap(path, get_vars)
            self._repark(st)
            self.assertEqual(json.loads(st.last_changed), [])
            state["total"] = "12"
            self._repark(st)
            self.assertEqual(json.loads(st.last_changed), ["total"])
            self.assertEqual(json.loads(st.last_removed), [])
            self.assertTrue(st.last_changed_complete)
            tr = st.last_change_tracking
            self.assertTrue(tr["complete"])
            self.assertEqual(tr["total"], 31)
            self.assertEqual(tr["scanned"], 31)
            # Display stays capped at MAX_VARS with its sentinel.
            locs = st.frame_locals(0)
            self.assertEqual(locs[-1]["name"], "…")
            self.assertEqual(len(locs), 21)

    def test_track_changes_truncated_over_256(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            state = {"wobble": "0", "v000val": "0"}
            def get_vars():
                out = [{"name": f"v{k:03d}", "type": "int",
                        "value": (state["v000val"] if k == 0 else str(k)),
                        "variablesReference": 0}
                       for k in range(300)]
                out.append({"name": "wobble", "type": "str",
                            "value": state["wobble"],
                            "variablesReference": 0})
                return out
            st.dap_request.side_effect = self._vars_dap(path, get_vars)
            self._repark(st)
            self.assertFalse(st.last_changed_complete)
            tr = st.last_change_tracking
            # First baseline over the cap: unknown, reason truncated.
            self.assertEqual(tr["reason"], "truncated")
            self.assertTrue(tr["truncated"])
            self.assertEqual(tr["total"], 301)
            self.assertEqual(tr["scanned"], 256)
            self.assertEqual(json.loads(st.last_changed), [])
            # A change inside the scanned window (v000 sorts within the
            # first 256) is still reported, but added/removed stay
            # suppressed and the comparison stays incomplete (the baseline
            # itself was truncated).
            state["v000val"] = "1"
            st.dap_request.side_effect = self._vars_dap(path, get_vars)
            self._repark(st)
            changed = json.loads(st.last_changed)
            self.assertEqual(json.loads(st.last_removed), [])
            self.assertFalse(st.last_changed_complete)
            self.assertEqual(st.last_change_tracking["reason"], "truncated")
            self.assertIn("v000", changed)

    def test_track_changes_function_change_resets_baseline(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            st.dap_request.side_effect = self._vars_dap(
                path, lambda: [{"name": "a", "type": "int", "value": "1",
                                "variablesReference": 0}], name="handler")
            self._repark(st)
            st.dap_request.side_effect = self._vars_dap(
                path, lambda: [{"name": "a", "type": "int", "value": "2",
                                "variablesReference": 0}], name="other")
            self._repark(st)
            self.assertEqual(json.loads(st.last_changed), [])
            self.assertEqual(json.loads(st.last_removed), [])
            self.assertFalse(st.last_changed_complete)
            self.assertEqual(st.last_change_tracking["reason"],
                             "function-changed")
            # Same function again with a new value: complete comparison.
            self._repark(st)
            self.assertEqual(json.loads(st.last_changed), [])
            self.assertTrue(st.last_changed_complete)

    def test_track_same_name_different_file_is_function_change(self):
        """Identity is path + function name: same-named functions in
        different files never compare silently."""
        with tempfile.TemporaryDirectory() as tmp:
            path_a = os.path.realpath(str(Path(tmp) / "a.py"))
            path_b = os.path.realpath(str(Path(tmp) / "b.py"))
            for p in (path_a, path_b):
                Path(p).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            st.dap_request.side_effect = self._vars_dap(
                path_a, lambda: [{"name": "a", "type": "int", "value": "1",
                                  "variablesReference": 0}], name="handler")
            self._repark(st)
            st.dap_request.side_effect = self._vars_dap(
                path_b, lambda: [{"name": "a", "type": "int", "value": "1",
                                  "variablesReference": 0}], name="handler")
            self._repark(st)
            # Identical names AND values, but a different file: unknown,
            # never a silent cross-function comparison.
            self.assertEqual(json.loads(st.last_changed), [])
            self.assertFalse(st.last_changed_complete)
            self.assertEqual(st.last_change_tracking["reason"],
                             "function-changed")

    def test_park_over_max_vars_completes_with_truncation(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            st.dap_request.side_effect = self._many_vars_dap(path, n=25)
            st.thread_id = None
            st.suspended = False
            st._park_stop("breakpoint", 1)
            self.assertTrue(st.suspended)
            snap = st.snapshot()
            locs = snap["frames"][0]["locals"]
            self.assertEqual(locs[-1]["name"], "…")
            self.assertIn("note", locs[-1])
            # Display caps at MAX_VARS; tracking baselines the full 25.
            self.assertEqual(st.last_change_tracking["total"], 25)
            self.assertFalse(st.last_changed_complete)
            # Second identical stop: complete comparison, no false changes.
            self._repark(st)
            self.assertEqual(json.loads(st.last_changed), [])
            self.assertTrue(st.last_changed_complete)
            self.assertNotIn("…", json.loads(st.last_changed))

    def test_park_survives_change_tracking_crash(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            st.dap_request.side_effect = _live_dap(path)
            st.thread_id = None
            st.suspended = False
            st.track_snapshot = Mock(side_effect=ValueError("synthetic"))
            st._park_stop("breakpoint", 1)  # must complete, not crash
            self.assertTrue(st.suspended)
            self.assertEqual(st.last_changed, "[]")
            self.assertEqual(st.last_removed, "[]")
            self.assertFalse(st.last_changed_complete)
            self.assertEqual(st.last_change_tracking["reason"],
                             "tracking-error")
            fields = st._change_fields()
            self.assertIn("trackingWarning", fields)
            # Class-only warning: no variable data or values leak.
            self.assertNotIn("synthetic", fields["trackingWarning"])
            self.assertEqual(st.last_top, {})
            # The session is intact: a later good park tracks normally.
            st.track_snapshot = Mock(return_value=(
                {"x": "1"}, 1, False, None))
            st.suspended = False
            st.last_func = "handler"
            st.frames = [{"name": "handler"}]
            st.last_top = {"x": "1"}
            st.last_track_complete = True
            st.last_track_reason = None
            st.track_changes()
            self.assertEqual(json.loads(st.last_changed), [])
            self.assertTrue(st.last_changed_complete)
            # Backstop: even a total track_changes failure inside
            # _park_stop degrades (safe baseline) instead of crashing.
            st.track_changes = Mock(side_effect=RuntimeError("synthetic"))
            st.suspended = False
            st._park_stop("breakpoint", 1)
            self.assertTrue(st.suspended)
            self.assertEqual(st.last_changed, "[]")
            self.assertEqual(st.last_top, {})

    # ---- capture short-lived target stages ----

    def test_capture_exit_after_armed_names_hit_stage(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            st.dap_request.side_effect = _live_dap(path)

            def fake_pump(timeout):
                raise _bridge.BridgeErr("target exited")
            st.pump = fake_pump
            with self.assertRaises(_bridge.BridgeErr) as cm:
                st.cmd_capture({"break": f"{path}:5"}, 5)
            msg = str(cm.exception)
            self.assertIn("target exited before capture hit", msg)
            self.assertIn(f"{path}:5", msg)
            self.assertNotIn("endpoint", msg.lower())
            self.assertNotIn("unreachable", msg.lower())
            self.assertNotIn("rejected", msg.lower())
            ctx = cm.exception.wait_context
            self.assertEqual(ctx["captureStage"], "armed-wait")
            self.assertTrue(ctx["ephemeralPlanted"])
            self.assertEqual(ctx["expectedBreak"], f"{path}:5")
            self.assertEqual(ctx["triggerStatus"], "unknown")
            # Ephemeral removed, nothing persisted, nothing resumed.
            calls = [c[0][0] for c in st.dap_request.call_args_list]
            self.assertEqual(calls.count("setBreakpoints"), 2)
            self.assertNotIn("continue", calls)
            self.assertEqual(st.cfg.breaks, [])

    def test_capture_exit_before_armed_at_plant(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            st.dap_request.side_effect = _live_dap(path)
            st._capture_plant = Mock(
                side_effect=_bridge.BridgeErr("target exited"))
            with self.assertRaises(_bridge.BridgeErr) as cm:
                st.cmd_capture({"break": f"{path}:5"}, 5)
            msg = str(cm.exception)
            self.assertIn(
                "capture target exited before ephemeral breakpoint was armed",
                msg)
            self.assertNotIn("endpoint", msg.lower())
            self.assertNotIn("rejected", msg.lower())
            ctx = cm.exception.wait_context
            self.assertEqual(ctx["captureStage"], "before-armed")
            self.assertFalse(ctx["ephemeralPlanted"])
            self.assertIsInstance(ctx["waitStartedAt"], int)
            self.assertIsInstance(ctx["waitedMs"], int)

    def test_capture_session_gone_before_command(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = _session(tmp)
            st.exited = True  # startup target already gone
            with self.assertRaises(_bridge.BridgeErr) as cm:
                st.cmd_capture({"break": "a.py:5"}, 5)
            # Truthful session/target-exited message, never "no session".
            self.assertIn("exited", str(cm.exception).lower())
            self.assertNotIn("no session", str(cm.exception).lower())
            ctx = cm.exception.wait_context
            self.assertEqual(ctx["captureStage"], "session-gone")
            self.assertIsInstance(ctx["waitStartedAt"], int)
            self.assertIsInstance(ctx["waitedMs"], int)

    def test_capture_timeout_carries_armed_stage(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = _session(tmp)
            st.dap_request.side_effect = _live_dap(path)

            def fake_pump(timeout):
                raise _bridge.StopTimeout(st.timeout_text(timeout))
            st.pump = fake_pump
            with self.assertRaises(_bridge.StopTimeout) as cm:
                st.cmd_capture({"break": f"{path}:5"}, 5)
            ctx = cm.exception.wait_context
            self.assertEqual(ctx["captureStage"], "armed-wait-timeout")
            self.assertTrue(ctx["ephemeralPlanted"])
            self.assertEqual(ctx["expectedBreak"], f"{path}:5")

    def test_frame_locals_skips_malformed_scopes_and_nested(self):
        # Malformed scopes list (non-dict entries) and malformed nested
        # Globals children (non-dict records, non-string names inside the
        # function-variables pseudo-container): skipped, never
        # AttributeError. The direct context/vars path (frames_json /
        # snapshot) and track_changes survive on the same fixture.
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))

            def fake(command, args=None, timeout=30):
                if command == "threads":
                    return {"threads": [{"id": 1, "name": "main"}]}
                if command == "scopes":
                    return {"scopes": ["junk", None, 42,
                                       {"name": "Globals",
                                        "variablesReference": 7}]}
                if command == "variables":
                    ref = (args or {}).get("variablesReference")
                    if ref == 7:
                        return {"variables": [
                            {"name": "function variables",
                             "variablesReference": 8},
                            "junk",
                            None,
                            {"name": 42, "type": "int", "value": "x",
                             "variablesReference": 0},
                            {"name": ["unhashable", "name"], "type": "?",
                             "value": "?", "variablesReference": 0},
                            {"name": "ok", "type": "int", "value": "1",
                             "variablesReference": 0}]}
                    if ref == 8:
                        return {"variables": [
                            {"name": "inner", "type": "int", "value": "2",
                             "variablesReference": 0},
                            None,
                            "junk",
                            {"name": {"weird": 1}, "type": "?",
                             "value": "?", "variablesReference": 0}]}
                    raise AssertionError(f"unexpected ref: {ref}")
                raise AssertionError(f"unexpected DAP: {command}")

            st = _session(tmp)
            st.frames = [{"id": 11, "name": "handler",
                          "source": {"path": path}, "line": 5}]
            st.dap_request.side_effect = fake
            locals_ = st.frame_locals(0)
            names = [v["name"] for v in locals_
                     if isinstance(v, dict)]
            self.assertIn("ok", names)
            self.assertIn("inner", names)
            # Direct context/vars path serves the same locals, no raise.
            snap = st.snapshot()
            snap_names = [v["name"] for v in
                          snap["frames"][0]["locals"]
                          if isinstance(v, dict)]
            self.assertIn("ok", snap_names)
            self.assertIn("inner", snap_names)
            # Change tracking scans the same shape independently of
            # display: first baseline is empty/unknown, and the Globals
            # pseudo expansion feeds tracking (ok + inner baselined).
            st.track_changes()
            self.assertEqual(json.loads(st.last_changed), [])
            self.assertEqual(st.last_change_tracking["reason"],
                             "first-snapshot")
            self.assertEqual(sorted(st.last_top), ["inner", "ok"])
            # Second identical scan: complete, no false changes.
            st.track_changes()
            self.assertEqual(json.loads(st.last_changed), [])
            self.assertTrue(st.last_changed_complete)

    def test_frame_locals_malformed_scopes_body_degrades(self):
        # Non-dict scopes body (or non-list scopes): no locals, no raise;
        # track_changes falls back to an empty baseline.
        with tempfile.TemporaryDirectory() as tmp:
            for body in (["not", "a", "dict"], {"scopes": "junk"}):
                st = _session(tmp)
                st.frames = [{"id": 11, "name": "handler"}]
                st.dap_request.side_effect = (
                    lambda command, args=None, timeout=30: body)
                self.assertEqual(st.frame_locals(0), [])
                st.track_changes()
                self.assertEqual(st.last_changed, "[]")


if __name__ == "__main__":
    unittest.main()
