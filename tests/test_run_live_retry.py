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


class SkipOnRetrySetup(unittest.TestCase):
    """setUpClass flakes ONLY on retry: the first attempt ran the test
    (and failed), the retry skips at class level. The placeholder skip id
    (`setUpClass (...)`) differs from the test id — the merge must keep
    the original failure."""

    @classmethod
    def setUpClass(cls):
        raise unittest.SkipTest("setup flaked on retry")

    def runTest(self):
        pass


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

    def test_class_skip_on_retry_keeps_failure(self):
        # A failure whose retry skips at setUpClass level never passed:
        # the original verdict must stand (the class-skip placeholder id
        # never matches the test id, so id subtraction alone heals it).
        loader = unittest.TestLoader()
        result = unittest.TestResult()
        ghost = mock.Mock()
        ghost.id.return_value = f"{__name__}.SkipOnRetrySetup.runTest"
        result.failures.append((ghost, "traceback"))
        merged = run_live._rerun_failed_once(loader, _runner(), result)
        self.assertEqual(len(merged.failures), 1)
        self.assertFalse(merged.wasSuccessful())


if __name__ == "__main__":
    # Only the merge tests run here: FailOnce/FailAlways are fixtures
    # driven through _rerun_failed_once, never directly.
    suite = unittest.TestLoader().loadTestsFromName(f"{__name__}.RetryMergeTests")
    sys.exit(0 if unittest.TextTestRunner(verbosity=1).run(suite).wasSuccessful() else 1)
