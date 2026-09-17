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

# Spike-flake absorption: shared CI runners stall individual integration
# tests past their budgets (varying set per run, green locally and on
# retry). Failed/errored tests rerun ONCE; a test failing twice stays red,
# so genuine regressions still fail the gate. Strict timeout asserts are
# deliberately NOT loosened — the retry absorbs spikes, the asserts keep
# their signal. Disable with LIVE_RETRY=0.


def _retry_enabled():
    return os.environ.get("LIVE_RETRY", "1").strip().lower() not in ("0", "false", "no")


def _rerun_failed_once(loader, runner, result):
    """Rerun exactly the failed/errored test ids once; drop entries for
    tests that pass on retry. Returns the (possibly new) result to judge."""
    bad_ids = []
    for t, _ in list(result.failures) + list(result.errors):
        try:
            tid = t.id()
        except Exception:
            continue
        if tid not in bad_ids:
            bad_ids.append(tid)
    retry_suite = unittest.TestSuite()
    unloadable = []
    for tid in bad_ids:
        try:
            retry_suite.addTests(loader.loadTestsFromName(tid))
        except Exception:
            unloadable.append(tid)
    if unloadable:
        print(f"live retry: cannot reload {unloadable}; keeping original verdicts",
              flush=True)
    retry_tests = list(_live_home.iter_suite_tests(retry_suite))
    if not retry_tests:
        return result
    # Collect ids BEFORE running: CPython replaces executed suite entries
    # with None after the run, so post-run iteration finds nothing.
    retry_ids = [t.id() for t in retry_tests]
    print(f"live retry: rerunning {len(retry_tests)} failed test(s) once: "
          f"{retry_ids}", flush=True)
    retry_result = runner.run(retry_suite)
    still_bad = {t.id() for t, _ in list(retry_result.failures) + list(retry_result.errors)}
    skipped_on_retry = {t.id() for t, _ in list(getattr(retry_result, "skipped", []))}
    # Heal only tests that actually ran green on retry: a failure that
    # skips on retry never passed, so its original verdict stands.
    healed = set(retry_ids) - still_bad - skipped_on_retry
    # A retried test that errored at setUpClass level may surface under a
    # placeholder id; only drop entries whose exact id healed.
    result.failures = [(t, tr) for t, tr in result.failures if t.id() not in healed]
    result.errors = [(t, tr) for t, tr in result.errors if t.id() not in healed]
    result.testsRun += retry_result.testsRun
    result.skipped = list(getattr(result, "skipped", [])) + list(
        getattr(retry_result, "skipped", []))
    if healed:
        print(f"live retry: healed by retry (spike flakes): {sorted(healed)}", flush=True)
    still = sorted({t.id() for t, _ in list(result.failures) + list(result.errors)})
    if still:
        print(f"live retry: still failing after retry (genuine): {still}", flush=True)
    return result


def main():
    unknown = _live_home.unknown_test_langs()
    if unknown:
        print(f"live FAILED: unknown TEST_LANG entries: {', '.join(unknown)} "
              f"(want py,node,java,browser)", flush=True)
        return 2
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
    runner = unittest.TextTestRunner(verbosity=1)
    result = runner.run(suite)
    if _retry_enabled() and (result.failures or result.errors):
        result = _rerun_failed_once(loader, runner, result)
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
