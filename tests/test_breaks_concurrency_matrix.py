"""Milestone A: serialized breakpoint-mutation concurrency matrix (Python).

Every breaks mutation (add/remove/clear) runs on Session._gate, so two
handler threads racing conflicting same-file replaces converge: backend
replace calls never overlap and bridge intent + backend + records agree
afterwards (no ghost plant, no resurrected removal).

Each case starts both ops on a threading.Barrier so they genuinely race
into dispatch, with a fake setBreakpoints backend that tracks concurrent
entries (max must stay 1) and serves as the backend-truth oracle.
"""
import importlib.util
import os
import tempfile
import threading
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location(
    "pybridge_concurrency_matrix", ROOT / "bridge/py/src/pybridge.py")
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)


def write_py(tmp, name, lines=12):
    p = os.path.realpath(os.path.join(tmp, name))
    Path(p).write_text("".join(f"line {n}\n" for n in range(lines)))
    return p


class Backend:
    """Fake per-file setBreakpoints replace backend (the intent oracle).

    Records the surviving line set per file on every replace call and
    tracks concurrent entries: serialized mutations must never overlap.
    """

    def __init__(self):
        self.lock = threading.Lock()
        self.cur = 0
        self.max = 0
        self.calls = []    # (path, tuple(lines)) in call order
        self.backend = {}  # path -> [lines]

    def __call__(self, command, args=None, timeout=30, semantic=False):
        assert command == "setBreakpoints", command
        with self.lock:
            self.cur += 1
            self.max = max(self.max, self.cur)
        try:
            time.sleep(0.02)  # widen the check-then-replace race window
            path = args["source"]["path"]
            lines = [bp["line"] for bp in args.get("breakpoints", [])
                     if "logMessage" not in bp]
            with self.lock:
                self.backend[path] = list(lines)
                self.calls.append((path, tuple(lines)))
            return {"breakpoints": [{"verified": True, "line": bp["line"]}
                                    for bp in args.get("breakpoints", [])]}
        finally:
            with self.lock:
                self.cur -= 1


class MatrixTests(unittest.TestCase):
    def session(self, tmp):
        cfg = bridge.Config()
        cfg.dir = tmp
        return bridge.Session(cfg)

    def seed(self, st, armed):
        """Arm breaks through the real add path (records stay consistent)."""
        if not armed:
            return
        by_file = {}
        for path, line in armed:
            by_file.setdefault(path, []).append(line)
        for path, lines in by_file.items():
            resp = st.dispatch({"cmd": "breaksAdd",
                                "breaks": [f"{path}:{ln}" for ln in lines]})
            self.assertTrue(resp["ok"], resp)

    def run_pair(self, st, req_a, req_b):
        bar = threading.Barrier(2)
        out = [None, None]

        def run(i, req):
            try:
                bar.wait(timeout=10)
                out[i] = st.dispatch(dict(req))
            except Exception as e:  # noqa: BLE001 — asserted below
                out[i] = e

        ths = [threading.Thread(target=run, args=(0, req_a)),
               threading.Thread(target=run, args=(1, req_b))]
        for t in ths:
            t.start()
        for t in ths:
            t.join(15)
        for r in out:
            self.assertIsInstance(r, dict, r)
            self.assertTrue(r.get("ok"), r)
        return out

    def assert_coherent(self, st, be, files):
        # Lightweight bookkeeping: config intent, stored raws, hit keys and
        # stop records agree exactly (only line breaks in these cases).
        self.assertEqual(set(st.cfg.breaks), set(st.cfg.break_raws.keys()))
        hk = {(k[1], k[2]) for k in st._hitkeys if k[0] == "break"}
        cfg = {(p, ln) for p, ln, _c in st.cfg.breaks}
        self.assertEqual(hk, cfg)
        self.assertEqual(len(st.stop_states), len(cfg))
        for rec in st.stop_states:
            self.assertEqual(rec["kind"], "break")
        # Backend convergence: every replace serialized, intent == backend.
        self.assertEqual(be.max, 1, "backend replace calls must never overlap")
        for f in files:
            want = sorted(ln for p, ln in cfg if p == f)
            self.assertEqual(sorted(be.backend.get(f, [])), want,
                             f"backend/intent drift on {f}")

    def fresh(self, tmp):
        a = write_py(tmp, "a.py")
        b = write_py(tmp, "b.py")
        st = self.session(tmp)
        be = Backend()
        st.dap_request = be
        return st, be, a, b

    def test_add_add_same_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            st, be, a, _b = self.fresh(tmp)
            out = self.run_pair(st,
                                {"cmd": "breaksAdd", "breaks": [f"{a}:5"]},
                                {"cmd": "breaksAdd", "breaks": [f"{a}:6"]})
            self.assertEqual(sum(len(r["added"]) for r in out), 2)
            self.assert_coherent(st, be, [a])
            self.assertEqual({(p, ln) for p, ln, _c in st.cfg.breaks},
                             {(a, 5), (a, 6)})

    def test_add_remove_same_file(self):
        # The ghost/resurrection case: without serialization the add's
        # merged replace re-plants the removed line (ghost) or the
        # remove's replace drops the added line.
        with tempfile.TemporaryDirectory() as tmp:
            st, be, a, _b = self.fresh(tmp)
            self.seed(st, [(a, 5)])
            be.max = 0
            out = self.run_pair(st,
                                {"cmd": "breaksAdd", "breaks": [f"{a}:6"]},
                                {"cmd": "breaksRemove",
                                 "breaks": [f"{a}:5"]})
            added = [e["raw"] for r in out for e in r.get("added", [])]
            removed = [e["raw"] for r in out for e in r.get("removed", [])]
            self.assertEqual(added, [f"{a}:6"])
            self.assertEqual(removed, [f"{a}:5"])
            self.assert_coherent(st, be, [a])
            self.assertEqual({(p, ln) for p, ln, _c in st.cfg.breaks},
                             {(a, 6)})

    def test_add_clear(self):
        with tempfile.TemporaryDirectory() as tmp:
            st, be, a, _b = self.fresh(tmp)
            self.seed(st, [(a, 5)])
            be.max = 0
            out = self.run_pair(st,
                                {"cmd": "breaksAdd", "breaks": [f"{a}:6"]},
                                {"cmd": "breaksClear"})
            self.assert_coherent(st, be, [a])
            final = {(p, ln) for p, ln, _c in st.cfg.breaks}
            # Order decides: clear-last wipes the add, add-last survives it.
            self.assertIn(final, (set(), {(a, 6)}))
            added = [e["raw"] for r in out for e in r.get("added", [])]
            self.assertEqual(added, [f"{a}:6"])
            removed = [e["raw"] for r in out for e in r.get("removed", [])]
            self.assertIn(f"{a}:5", removed)
            if final == {}:
                self.assertIn(f"{a}:6", removed)
            else:
                self.assertNotIn(f"{a}:6", removed)

    def test_remove_remove_same_key(self):
        with tempfile.TemporaryDirectory() as tmp:
            st, be, a, _b = self.fresh(tmp)
            self.seed(st, [(a, 5)])
            be.max = 0
            out = self.run_pair(st,
                                {"cmd": "breaksRemove",
                                 "breaks": [f"{a}:5"]},
                                {"cmd": "breaksRemove",
                                 "breaks": [f"{a}:5"]})
            winners = [r for r in out if len(r.get("removed", [])) == 1]
            losers = [r for r in out if r.get("removed", []) == []]
            self.assertEqual(len(winners), 1)
            self.assertEqual(len(losers), 1)
            self.assertEqual(losers[0].get("missing"), [f"{a}:5"])
            self.assert_coherent(st, be, [a])
            self.assertEqual(st.cfg.breaks, [])

    def test_remove_clear(self):
        with tempfile.TemporaryDirectory() as tmp:
            st, be, a, _b = self.fresh(tmp)
            self.seed(st, [(a, 5), (a, 8)])
            be.max = 0
            out = self.run_pair(st,
                                {"cmd": "breaksRemove",
                                 "breaks": [f"{a}:5"]},
                                {"cmd": "breaksClear"})
            self.assert_coherent(st, be, [a])
            self.assertEqual(st.cfg.breaks, [])
            # Each armed line is removed exactly once across both replies.
            removed = sorted(e["raw"] for r in out
                             for e in r.get("removed", []))
            self.assertEqual(removed, sorted([f"{a}:5", f"{a}:8"]))

    def test_clear_add(self):
        with tempfile.TemporaryDirectory() as tmp:
            st, be, a, _b = self.fresh(tmp)
            self.seed(st, [(a, 5)])
            be.max = 0
            out = self.run_pair(st,
                                {"cmd": "breaksClear"},
                                {"cmd": "breaksAdd", "breaks": [f"{a}:6"]})
            self.assert_coherent(st, be, [a])
            final = {(p, ln) for p, ln, _c in st.cfg.breaks}
            self.assertIn(final, (set(), {(a, 6)}))
            added = [e["raw"] for r in out for e in r.get("added", [])]
            self.assertEqual(added, [f"{a}:6"])

    def test_add_add_different_files_no_cross_corruption(self):
        with tempfile.TemporaryDirectory() as tmp:
            st, be, a, b = self.fresh(tmp)
            out = self.run_pair(st,
                                {"cmd": "breaksAdd", "breaks": [f"{a}:5"]},
                                {"cmd": "breaksAdd", "breaks": [f"{b}:6"]})
            self.assertEqual(sum(len(r["added"]) for r in out), 2)
            self.assert_coherent(st, be, [a, b])
            self.assertEqual({(p, ln) for p, ln, _c in st.cfg.breaks},
                             {(a, 5), (b, 6)})


if __name__ == "__main__":
    unittest.main()
