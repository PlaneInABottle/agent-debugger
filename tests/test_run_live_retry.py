"""Unit tests for tests/run_live.py retry-once merge (no daemons).

Covers: a test failing once then passing heals the verdict; a test
failing twice stays failed; unloadable ids keep their verdicts. The
flakiness signal is a marker file (module re-import shares nothing, so
in-memory counters would not survive the reload boundary under test).
"""
import io
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import run_live

MARK = Path(tempfile.gettempdir()) / "agent-debugger-retry-probe-marker"


class FailOnce(unittest.TestCase):
    def runTest(self):
        if not MARK.exists():
            MARK.write_text("first attempt failed", encoding="utf-8")
            self.fail("first attempt fails by design")


class FailAlways(unittest.TestCase):
    def runTest(self):
        self.fail("always fails by design")


def _runner():
    return unittest.TextTestRunner(stream=io.StringIO(), verbosity=0)


class RetryMergeTests(unittest.TestCase):
    def setUp(self):
        try:
            MARK.unlink()
        except OSError:
            pass

    def tearDown(self):
        try:
            MARK.unlink()
        except OSError:
            pass

    def test_fail_once_heals_on_retry(self):
        loader = unittest.TestLoader()
        first = loader.loadTestsFromName(f"{__name__}.FailOnce")
        result = _runner().run(first)
        self.assertEqual(len(result.failures), 1)
        merged = run_live._rerun_failed_once(loader, _runner(), result)
        self.assertEqual(merged.failures, [])
        self.assertEqual(merged.errors, [])
        self.assertTrue(merged.wasSuccessful())

    def test_fail_twice_stays_failed(self):
        loader = unittest.TestLoader()
        first = loader.loadTestsFromName(f"{__name__}.FailAlways")
        result = _runner().run(first)
        self.assertEqual(len(result.failures), 1)
        merged = run_live._rerun_failed_once(loader, _runner(), result)
        self.assertEqual(len(merged.failures), 1)
        self.assertFalse(merged.wasSuccessful())

    def test_unloadable_ids_keep_verdicts(self):
        loader = unittest.TestLoader()
        result = unittest.TestResult()
        ghost = mock.Mock()
        ghost.id.return_value = "no.such.TestCase.test_x"
        result.failures.append((ghost, "traceback"))
        merged = run_live._rerun_failed_once(loader, _runner(), result)
        self.assertEqual(len(merged.failures), 1)


if __name__ == "__main__":
    # Only the merge tests run here: FailOnce/FailAlways are fixtures
    # driven through _rerun_failed_once, never directly.
    suite = unittest.TestLoader().loadTestsFromName(f"{__name__}.RetryMergeTests")
    sys.exit(0 if unittest.TextTestRunner(verbosity=1).run(suite).wasSuccessful() else 1)
