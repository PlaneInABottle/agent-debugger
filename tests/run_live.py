"""Canonical live gate entry (daemon-backed; needs `cargo build` first).

Runs exactly the three class-level live suites (test_live, test_m5_live,
test_ux_live) under the shared TEST_LANG / SKIP_BROWSER filter
(tests/_live_home.py load_tests), prints executed/skipped counts per
language, and enforces the live nonzero policy: every required language
(default full = py+node+java given the doctor prerequisites, plus browser
unless SKIP_BROWSER=1; or every explicit TEST_LANG entry) must execute at
least one test, and the scope as a whole must execute at least one. An
all-skipped scope (e.g. missing adapter dependencies) fails instead of
passing silently. Exit nonzero on test failures/errors or on a nonzero
violation. Direct `python3 tests/test_live.py` stays available for
single-test debugging (filtering only, no enforcement).
"""
import os
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import _live_home

LIVE_MODULES = ("test_live", "test_m5_live", "test_ux_live")


def main():
    loader = unittest.TestLoader()
    suite = unittest.TestSuite()
    for mod in LIVE_MODULES:
        suite.addTests(loader.loadTestsFromName(mod))
    selected = list(_live_home.iter_suite_tests(suite))
    selected_ids = [t.id() for t in selected]
    required = _live_home.selected_live_langs()
    print(f"live scope: modules={list(LIVE_MODULES)} "
          f"TEST_LANG={os.environ.get('TEST_LANG', '')!r} "
          f"SKIP_BROWSER={os.environ.get('SKIP_BROWSER', '')!r} "
          f"required={list(required)} selected={len(selected_ids)}",
          flush=True)
    result = unittest.TextTestRunner(verbosity=1).run(suite)
    ok, message, summary = _live_home.check_live_nonzero(
        selected_ids, result, required)
    per = ", ".join(
        f"{lang}:exec={c['executed']}/sel={c['selected']}"
        for lang, c in summary["per_lang"].items()
        if c["selected"])
    print(f"live counts: executed={summary['executed']} "
          f"skipped={summary['skipped']} "
          f"selected={summary['selected']} [{per}]", flush=True)
    print(message, flush=True)
    if not result.wasSuccessful():
        print("live FAILED: test failures/errors above", flush=True)
        return 1
    if not ok:
        print(f"live FAILED: {message}", flush=True)
        return 1
    print("live PASSED", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
