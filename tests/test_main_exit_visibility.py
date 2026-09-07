"""Milestone B: main-exit visibility (Python) + break bookkeeping invariants.

When the main DAP session dies while children live on (main_exited, dap
None, session not fully exited), bare `threads` must serve the survivors
instead of failing on main's gone connection, and explicit main reads must
fail fast with the exact typed main-exited error. Adjacent record reads
(targets/breaks) and frame reads (context) are pinned against the same
concrete bug.

The module-level fixtures (make_target_session, add_child, kill_main,
ok_dap) are reusable: they build a main-dead + live-child session through
the real add/inherit paths, never by hand-editing bridge bookkeeping.
"""
import importlib.util
import os
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location(
    "pybridge_main_exit", ROOT / "bridge/py/src/pybridge.py")
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)

MAIN_EXITED_MSG = "target main has exited — close this session"
SESSION_EXITED_MSG = "target VM has exited — close this session"


def write_py(tmp, name, lines=12):
    p = os.path.realpath(os.path.join(tmp, name))
    Path(p).write_text("".join(f"line {n}\n" for n in range(lines)))
    return p


def ok_dap(threads=(), fail_on=None):
    """Stub DAP connection: per-file verified plants + canned threads."""
    def request(command, args=None, timeout=30, semantic=False):
        if fail_on is not None and command in fail_on:
            raise bridge.BridgeErr(f"child backend gone: {command}")
        if command == "setBreakpoints":
            items = (args or {}).get("breakpoints", [])
            return {"breakpoints": [{"verified": True, "line": bp["line"]}
                                    for bp in items]}
        if command == "threads":
            return {"threads": list(threads)}
        if command == "stackTrace":
            return {"stackFrames": []}
        return {}
    return SimpleNamespace(request=request, stash=[], sock=None)


def make_target_session(tmp):
    cfg = bridge.Config()
    cfg.dir = tmp
    cfg.kind = "launch"
    cfg.subprocess = True
    st = bridge.Session(cfg)
    st.publish_state = Mock()
    return st


def add_child(st, pid, suspended=False, dap=None):
    tid = f"child:{pid}"
    child = bridge.ChildTarget(
        tid, pid, dap if dap is not None else ok_dap(), Mock(),
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


def kill_main(st):
    """Simulate a dead main DAP session with survivors: the
    `_conn_dead_inner` + terminal-event end state (main_exited, dap gone,
    park cleared) while `exited` stays False and children live on."""
    st.main_exited = True
    st.dap = None
    st.suspended = False
    st.thread_id = None
    st.frames = []


def global_add(st, raw):
    resp = st.dispatch({"cmd": "breaksAdd", "breaks": [raw]})
    assert resp["ok"], resp
    return resp


class MainExitVisibilityTests(unittest.TestCase):
    def dead_main_with_children(self, tmp, pids=(61, 62), parked=()):
        st = make_target_session(tmp)
        st.dap = ok_dap()
        a = write_py(tmp, "a.py")
        kids = {}
        for pid in pids:
            kids[pid] = add_child(st, pid,
                                  suspended=(pid in parked),
                                  dap=ok_dap(threads=[{"id": 1, "name": "w"}]))
        global_add(st, f"{a}:5")
        kill_main(st)
        return st, a, kids

    def test_bare_threads_serves_live_children_without_main(self):
        with tempfile.TemporaryDirectory() as tmp:
            st, _a, _kids = self.dead_main_with_children(tmp)
            resp = st.cmd_threads({})
            self.assertTrue(resp["ok"], resp)
            ids = [e["target"] for e in resp["targets"]]
            self.assertEqual(ids, ["child:61", "child:62"])
            self.assertNotIn("main", ids)
            # Selected fallback: stale "main" selection is not served;
            # the first survivor answers the top-level dump.
            self.assertEqual(resp["selected"], "child:61")
            self.assertEqual(resp["target"], "child:61")
            self.assertTrue(resp["running"])

    def test_bare_threads_parked_child_reports_stopped(self):
        with tempfile.TemporaryDirectory() as tmp:
            st, _a, _kids = self.dead_main_with_children(
                tmp, pids=(61,), parked=(61,))
            resp = st.cmd_threads({})
            self.assertTrue(resp["ok"], resp)
            # One survivor: the legacy single-target shape names it.
            self.assertEqual(resp["target"], "child:61")
            self.assertNotIn("targets", resp)
            self.assertFalse(resp["running"])
            self.assertEqual(len(resp["threads"]), 1)

    def test_bare_threads_stale_main_park_falls_back_to_child(self):
        # The conn died without the terminal-event cleanup: main still
        # looks parked (stale _main_seq wins auto-select), but the bare
        # read must serve the live child, never the dead main.
        with tempfile.TemporaryDirectory() as tmp:
            st = make_target_session(tmp)
            st.dap = ok_dap()
            a = write_py(tmp, "a.py")
            add_child(st, 61, dap=ok_dap(
                threads=[{"id": 1, "name": "w"}]))
            global_add(st, f"{a}:5")
            kill_main(st)
            st.suspended = True  # stale park the dead main keeps
            st._main_seq = 7
            st._stop_seq = 7
            resp = st.cmd_threads({})
            self.assertTrue(resp["ok"], resp)
            # One survivor: the legacy single-target shape names it.
            self.assertNotIn("targets", resp)
            self.assertEqual(resp["target"], "child:61")

    def test_explicit_main_threads_is_typed_main_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            st, _a, _kids = self.dead_main_with_children(tmp)
            with self.assertRaises(bridge.BridgeErr) as cm:
                st.cmd_threads({"target": "main"})
            self.assertEqual(str(cm.exception), MAIN_EXITED_MSG)

    def test_explicit_child_threads_ok(self):
        with tempfile.TemporaryDirectory() as tmp:
            st, _a, _kids = self.dead_main_with_children(tmp, pids=(61,))
            resp = st.cmd_threads({"target": "child:61"})
            self.assertTrue(resp["ok"], resp)
            self.assertEqual(resp["target"], "child:61")
            self.assertTrue(resp["running"])

    def test_fully_exited_threads_still_session_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            st, _a, _kids = self.dead_main_with_children(tmp)
            st.exited = True
            with self.assertRaises(bridge.BridgeErr) as cm:
                st.cmd_threads({})
            self.assertEqual(str(cm.exception), SESSION_EXITED_MSG)

    def test_dead_main_no_children_is_typed_main_error(self):
        # Nothing live to serve: the selected target (dead main) reports
        # honestly instead of a DAP None failure.
        with tempfile.TemporaryDirectory() as tmp:
            st = make_target_session(tmp)
            st.dap = ok_dap()
            kill_main(st)
            with self.assertRaises(bridge.BridgeErr) as cm:
                st.cmd_threads({})
            self.assertEqual(str(cm.exception), MAIN_EXITED_MSG)

    def test_bare_targets_shows_main_exited_and_live_child(self):
        with tempfile.TemporaryDirectory() as tmp:
            st, _a, _kids = self.dead_main_with_children(tmp, pids=(61,))
            resp = st.cmd_targets()
            self.assertTrue(resp["ok"], resp)
            by_id = {e["id"]: e for e in resp["targets"]}
            self.assertEqual(by_id["main"]["state"], "exited")
            self.assertEqual(by_id["child:61"]["state"], "running")

    def test_bare_breaks_aggregates_without_dap_traffic(self):
        with tempfile.TemporaryDirectory() as tmp:
            st, _a, _kids = self.dead_main_with_children(tmp, pids=(61,))
            st.dap_request = Mock(
                side_effect=AssertionError("bare breaks issues no DAP"))
            resp = st.cmd_breaks({})
            self.assertTrue(resp["ok"], resp)
            seen = {(e.get("target"), e.get("kind")) for e in resp["stops"]}
            self.assertIn(("main", "break"), seen)
            self.assertIn(("child:61", "break"), seen)
            st.dap_request.assert_not_called()

    def test_explicit_main_context_is_typed_main_error(self):
        # Adjacent frame read: the main-exited check fires before any
        # stale-frame logic, with the exact same typed message.
        with tempfile.TemporaryDirectory() as tmp:
            st, _a, _kids = self.dead_main_with_children(tmp, pids=(61,))
            st.suspended = True  # stale park must not mask the exit
            st.thread_id = 7
            st.frames = [{"id": 7}]
            with self.assertRaises(bridge.BridgeErr) as cm:
                st.dispatch({"cmd": "context", "target": "main"})
            self.assertEqual(str(cm.exception), MAIN_EXITED_MSG)


class BreakInvariantTests(unittest.TestCase):
    """Bookkeeping invariant: every displayed line-break record is
    reachable by canonical key, on main and on each child.

    The oracle is maintained by the TEST (expected key sets per target),
    never derived from production matchers: bridge maps/sets/records must
    equal it after every mutation.
    """

    def oracle_state(self, st):
        """(main_keys, {tid: child_keys}) from bridge structures directly."""
        main = {(p, ln) for p, ln, _c in st.cfg.breaks}
        kids = {}
        for tid in st.target_order:
            t = st.targets.get(tid)
            if t is None or t.exited:
                continue
            kids[tid] = (set(t.inherited_keys)
                         | set(t.target_raws.keys()))
        return main, kids

    def assert_invariants(self, st, expect_main, expect_kids):
        # Config intent == stored raws.
        self.assertEqual(set(st.cfg.breaks), set(st.cfg.break_raws.keys()))
        self.assertEqual({(p, ln) for p, ln, _c in st.cfg.breaks},
                         expect_main)
        # Every main break key has exactly one break hitkey + one break
        # record; counts agree (one live record per file+line).
        main_hk = [(k[1], k[2]) for k in st._hitkeys if k[0] == "break"]
        self.assertEqual(sorted(main_hk), sorted(expect_main))
        main_recs = [r for r in st.stop_states if r.get("kind") == "break"]
        self.assertEqual(len(main_recs), len(expect_main))
        # Every child key has a matching break hitkey/record; child
        # records never leak into another target's sets.
        for tid, want in expect_kids.items():
            t = st.targets[tid]
            got = set(t.inherited_keys) | set(t.target_raws.keys())
            self.assertEqual({(p, ln) for p, ln, _c in got}, want)
            for (p, ln, _c) in got:
                hits = [k for k in t._hitkeys
                        if k[0] == "break" and k[1] == p and k[2] == ln]
                self.assertEqual(len(hits), 1, (tid, p, ln))
            recs = [r for r in t.stop_states if r.get("kind") == "break"]
            self.assertEqual(len(recs), len(want), tid)

    def live_session(self, tmp, child_pids=(61,)):
        st = make_target_session(tmp)
        st.dap = ok_dap()
        a = write_py(tmp, "a.py")
        for pid in child_pids:
            add_child(st, pid, dap=ok_dap())
        return st, a

    def test_invariants_across_add_remove_clear(self):
        with tempfile.TemporaryDirectory() as tmp:
            st, a = self.live_session(tmp)
            self.assert_invariants(st, set(), {"child:61": set()})
            global_add(st, f"{a}:5")
            global_add(st, f"{a}:6")
            # New global intent inherits as plant copies on the live child.
            self.assert_invariants(st, {(a, 5), (a, 6)},
                                   {"child:61": {(a, 5), (a, 6)}})
            # Ephemeral target-scoped add touches only the child.
            resp = st.dispatch({"cmd": "breaksAdd", "target": "child:61",
                                "breaks": [f"{a}:8"]})
            self.assertTrue(resp["ok"], resp)
            self.assert_invariants(st, {(a, 5), (a, 6)},
                                   {"child:61": {(a, 5), (a, 6), (a, 8)}})
            # Scoped remove drops only the ephemeral record.
            resp = st.dispatch({"cmd": "breaksRemove", "target": "child:61",
                                "breaks": [f"{a}:8"]})
            self.assertTrue(resp["ok"], resp)
            self.assert_invariants(st, {(a, 5), (a, 6)},
                                   {"child:61": {(a, 5), (a, 6)}})
            # Global remove drops the intent plus inherited copies.
            resp = st.dispatch({"cmd": "breaksRemove",
                                "breaks": [f"{a}:5"]})
            self.assertTrue(resp["ok"], resp)
            self.assert_invariants(st, {(a, 6)}, {"child:61": {(a, 6)}})
            # Bare clear is the full reset.
            resp = st.dispatch({"cmd": "breaksClear"})
            self.assertTrue(resp["ok"], resp)
            self.assert_invariants(st, set(), {"child:61": set()})
            # Every displayed record was eliminated: no break kind left.
            stops = st.dispatch({"cmd": "breaks"})["stops"]
            self.assertEqual([r for r in stops
                              if r.get("kind") == "break"], [])

    def test_inherit_failure_warns_and_holds_invariants(self):
        # The child backend dies: the confirmed main mutation stands, the
        # child gains no keys/records, and the reply warns.
        with tempfile.TemporaryDirectory() as tmp:
            st = make_target_session(tmp)
            st.dap = ok_dap()
            a = write_py(tmp, "a.py")
            add_child(st, 61, dap=ok_dap(
                threads=[{"id": 1, "name": "w"}],
                fail_on=("setBreakpoints",)))
            resp = global_add(st, f"{a}:5")
            self.assertIn("warning", resp)
            self.assertIn("child:61", resp["warning"])
            self.assert_invariants(st, {(a, 5)}, {"child:61": set()})
            # The child stays readable and removable paths stay coherent.
            threads = st.cmd_threads({"target": "child:61"})
            self.assertTrue(threads["ok"], threads)
            rm = st.dispatch({"cmd": "breaksRemove",
                              "breaks": [f"{a}:5"]})
            self.assertTrue(rm["ok"], rm)
            self.assert_invariants(st, set(), {"child:61": set()})

    def file_backend(self, fail_paths=()):
        """Per-file setBreakpoints oracle; files in fail_paths raise."""
        backend = {}

        def request(command, args=None, timeout=30, semantic=False):
            self.assertEqual(command, "setBreakpoints")
            path = args["source"]["path"]
            if path in fail_paths:
                raise bridge.BridgeErr("backend gone")
            items = args.get("breakpoints", [])
            backend[path] = [bp["line"] for bp in items
                             if "logMessage" not in bp]
            return {"breakpoints": [{"verified": True, "line": bp["line"]}
                                    for bp in items]}

        return SimpleNamespace(request=request, stash=[], sock=None), backend

    def two_file_session(self, tmp):
        """Main armed a:5 + b:6 with inherited copies on child:61."""
        st = make_target_session(tmp)
        st.dap = ok_dap()
        a = write_py(tmp, "a.py")
        b = write_py(tmp, "b.py")
        add_child(st, 61, dap=ok_dap())
        global_add(st, f"{a}:5")
        global_add(st, f"{b}:6")
        self.assert_invariants(st, {(a, 5), (b, 6)},
                               {"child:61": {(a, 5), (b, 6)}})
        return st, a, b

    def test_remove_partial_failure_keeps_failed_everywhere(self):
        # Main b-file backend dies while the child's stays healthy:
        # remove [a, b] confirms only a. The failed b intent, its child
        # copy, and its backend plant all survive; persistence sees the
        # confirmed raw only. (If the child copy dropped while main
        # failed, bridge intent, backend, and persistence would diverge.)
        with tempfile.TemporaryDirectory() as tmp:
            st, a, b = self.two_file_session(tmp)
            main_dap, main_be = self.file_backend(fail_paths=(b,))
            child_dap, child_be = self.file_backend()
            st.dap = main_dap
            st.targets["child:61"].dap = child_dap
            resp = st.dispatch({"cmd": "breaksRemove",
                                "breaks": [f"{a}:5", f"{b}:6"]})
            self.assertTrue(resp["ok"], resp)
            self.assertEqual([e["raw"] for e in resp["removed"]],
                             [f"{a}:5"])
            self.assertEqual([f["raw"] for f in resp.get("failed", [])],
                             [f"{b}:6"])
            self.assertIn(f"{b}:6", resp.get("warning", ""))
            self.assertNotIn(f"{a}:5", resp.get("warning", ""))
            # Failed intent untouched on main and child; backend agrees.
            self.assert_invariants(st, {(b, 6)}, {"child:61": {(b, 6)}})
            self.assertEqual(main_be.get(a), [])
            self.assertEqual(child_be.get(a), [])
            self.assertNotIn(b, main_be)
            self.assertNotIn(b, child_be)
            # Displayed records: b survives on both targets, a is gone.
            stops = st.dispatch({"cmd": "breaks"})["stops"]
            shown = {(e.get("target"), e.get("spec"))
                     for e in stops if e.get("kind") == "break"}
            self.assertTrue(any(t == "main" and ":6" in (s or "")
                                for t, s in shown), shown)
            self.assertTrue(any(t == "child:61" for t, s in shown), shown)
            self.assertFalse(any(":5" in (s or "") for t, s in shown),
                             shown)

    def test_clear_partial_failure_resets_ephemeral_keeps_failed(self):
        # Bare clear with a dead b-file: confirmed a intent + inherited
        # copies drop, the failed b intent + copy survive, and the
        # unrelated ephemeral record still resets.
        with tempfile.TemporaryDirectory() as tmp:
            st, a, b = self.two_file_session(tmp)
            resp = st.dispatch({"cmd": "breaksAdd", "target": "child:61",
                                "breaks": [f"{a}:8"]})
            self.assertTrue(resp["ok"], resp)
            main_dap, main_be = self.file_backend(fail_paths=(b,))
            child_dap, child_be = self.file_backend()
            st.dap = main_dap
            st.targets["child:61"].dap = child_dap
            resp = st.dispatch({"cmd": "breaksClear"})
            self.assertTrue(resp["ok"], resp)
            self.assertEqual([e["raw"] for e in resp["removed"]],
                             [f"{a}:5"])
            self.assertIn(f"{b}:6", resp.get("warning", ""))
            self.assert_invariants(st, {(b, 6)}, {"child:61": {(b, 6)}})
            self.assertEqual(main_be.get(a), [])
            self.assertEqual(child_be.get(a), [])
            self.assertNotIn(b, main_be)
            self.assertNotIn(b, child_be)


if __name__ == "__main__":
    unittest.main()
