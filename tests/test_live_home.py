"""Unit tests for tests/_live_home.py (no daemons, no HOME mutation).

Covers the TEST_LANG/SKIP_BROWSER selector, secret sanitizing, and the
failure-diagnostics bundle (forced-failure drill: fake session artifacts
must print as paths with a redacted tail, never raising).
"""
import contextlib
import io
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import _live_home


class LiveHomeHelperTests(unittest.TestCase):
    def test_wanted_lang_tokens(self):
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("TEST_LANG", None)
            os.environ.pop("SKIP_BROWSER", None)
            self.assertTrue(_live_home._wanted("test_32_py_wait_capture"))
            self.assertTrue(_live_home._wanted("test_01_parallel_sessions"))
        with mock.patch.dict(os.environ, {"TEST_LANG": "py"}):
            os.environ.pop("SKIP_BROWSER", None)
            self.assertTrue(_live_home._wanted("test_32_py_wait_capture"))
            self.assertFalse(_live_home._wanted("test_33_node_wait_capture"))
            # Cross-cutting (no token): always runs.
            self.assertTrue(_live_home._wanted("test_01_parallel_sessions"))
        with mock.patch.dict(os.environ, {"TEST_LANG": "node,java"}):
            os.environ.pop("SKIP_BROWSER", None)
            self.assertTrue(_live_home._wanted("test_33_node_wait_capture"))
            self.assertTrue(_live_home._wanted("test_34_java_wait_capture"))
            self.assertFalse(_live_home._wanted("test_32_py_wait_capture"))
        with mock.patch.dict(os.environ, {"TEST_LANG": "PY, Node"}):
            os.environ.pop("SKIP_BROWSER", None)
            self.assertTrue(_live_home._wanted("test_32_py_wait_capture"))
            self.assertTrue(_live_home._wanted("test_33_node_wait_capture"))
            self.assertFalse(_live_home._wanted("test_34_java_wait_capture"))
        with mock.patch.dict(os.environ, {"SKIP_BROWSER": "1"}):
            os.environ.pop("TEST_LANG", None)
            self.assertFalse(_live_home._wanted("test_42_browser_wait_capture"))
            self.assertTrue(_live_home._wanted("test_32_py_wait_capture"))

    def test_sanitize_masks_secrets(self):
        out = _live_home.sanitize("login token=abc123 ok\npassword: hunter2\nplain line")
        self.assertNotIn("abc123", out)
        self.assertNotIn("hunter2", out)
        self.assertIn("[redacted]", out)
        self.assertIn("plain line", out)

    def test_sanitize_masks_json_quoted_key_secrets(self):
        cases = [
            '{"password": "hunter2", "user": "alice"}',
            '{"api_key":"K9", "nested": {"token": "T-1"}}',
            '{"passwd": barevalue, "ok": 1}',
            'bridge.log: {"secret": "a b spaced"} tail',
        ]
        for raw in cases:
            with self.subTest(raw=raw):
                out = _live_home.sanitize(raw)
                for leaked in ("hunter2", "K9", "T-1", "barevalue", "a b spaced"):
                    self.assertNotIn(leaked, out, f"value leaked from {raw!r}")
                self.assertIn("[redacted]", out)
        # Non-secret content survives, keys stay readable.
        out = _live_home.sanitize('{"password": "x", "user": "alice"}')
        self.assertIn('"user": "alice"', out)
        self.assertIn('"password": "[redacted]"', out)

    def test_failure_bundle_prints_paths_and_redacted_tail(self):
        with tempfile.TemporaryDirectory(prefix="live-home-drill-") as tmp:
            home = Path(tmp)
            sdir = home / ".agent-debugger" / "sessions" / "drill-session"
            sdir.mkdir(parents=True)
            (sdir / "bridge.log").write_text(
                "line1\nconnect password=hunter2\nline3\n")
            (sdir / "session.json").write_text('{"port": 1}')

            class Fake(_live_home.LiveHomeMixin):
                pass

            Fake.home = home
            Fake.sessions = {"drill-session"}
            buf = io.StringIO()
            with contextlib.redirect_stderr(buf):
                Fake.dump_failure_bundle()
            out = buf.getvalue()
            for artifact in ("bridge.log", "session.json",
                             "error.json", "stops.json"):
                self.assertIn(f"drill-session/{artifact}", out)
                self.assertIn(str(sdir / artifact), out)
            self.assertNotIn("hunter2", out)
            self.assertIn("[redacted]", out)

    def test_failure_bundle_never_raises(self):
        class Fake(_live_home.LiveHomeMixin):
            home = Path("/nonexistent/agent-debugger-home")
            sessions = {"ghost"}

        buf = io.StringIO()
        with contextlib.redirect_stderr(buf):
            Fake.dump_failure_bundle()  # must not raise
        self.assertIn("ghost", buf.getvalue())

    def test_setup_home_copies_dependency_dirs_without_symlinks(self):
        # Regression for M3's symlink refusal: setup_home must provide real
        # adapter/dependency directories while retaining the existing
        # dependency prerequisite and never installing or mutating it.
        class Fake(_live_home.LiveHomeMixin, unittest.TestCase):
            pass

        with tempfile.TemporaryDirectory(prefix="live-home-origin-") as origin_tmp:
            origin = Path(origin_tmp)
            for lang, entry, marker in [
                ("python", "venv", "bin/python"),
                ("node", "node_modules", "ws/package.json"),
            ]:
                source = origin / ".agent-debugger" / "adapters" / lang / entry
                (source / Path(marker).parent).mkdir(parents=True)
                (source / marker).write_text("fixture dependency")
            with mock.patch.object(Path, "home", return_value=origin):
                Fake.setup_home("live-home-copy-", "fixture")
            try:
                for lang, entry, marker in [
                    ("python", "venv", "bin/python"),
                    ("node", "node_modules", "ws/package.json"),
                ]:
                    copied = Fake.home / ".agent-debugger" / "adapters" / lang / entry
                    self.assertTrue(copied.is_dir())
                    self.assertFalse(copied.is_symlink())
                    self.assertTrue((copied / marker).is_file())
                    self.assertFalse((copied / marker).is_symlink())
            finally:
                Fake.cleanup()


class _StubResult:
    """Minimal unittest-result double (check_live_nonzero reads .skipped
    only — no console parsing anywhere)."""

    def __init__(self, skipped=()):
        self.skipped = list(skipped)


def _tid(mod="tests.test_live", cls="LiveTests", meth="test_01_py_a"):
    return f"{mod}.{cls}.{meth}"


class LiveNonzeroPolicyTests(unittest.TestCase):
    def test_selected_langs_defaults_and_filters(self):
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("TEST_LANG", None)
            os.environ.pop("SKIP_BROWSER", None)
            self.assertEqual(_live_home.selected_live_langs(),
                             ("py", "node", "java", "browser"))
        with mock.patch.dict(os.environ, {"SKIP_BROWSER": "1"}):
            os.environ.pop("TEST_LANG", None)
            self.assertEqual(_live_home.selected_live_langs(),
                             ("py", "node", "java"))
        with mock.patch.dict(os.environ, {"TEST_LANG": "py"}):
            os.environ.pop("SKIP_BROWSER", None)
            self.assertEqual(_live_home.selected_live_langs(), ("py",))
        with mock.patch.dict(os.environ, {"TEST_LANG": "node,java"}):
            self.assertEqual(_live_home.selected_live_langs(),
                             ("java", "node"))

    def test_lang_of_test_id(self):
        self.assertEqual(
            _live_home.lang_of_test_id(_tid(meth="test_32_py_wait")), "py")
        self.assertEqual(
            _live_home.lang_of_test_id(_tid(meth="test_33_node_x")), "node")
        self.assertEqual(
            _live_home.lang_of_test_id(_tid(meth="test_04_browser_y")),
            "browser")
        self.assertIsNone(
            _live_home.lang_of_test_id(_tid(meth="test_01_parallel")))
        # Class-level placeholder carries no language token (shared).
        self.assertIsNone(_live_home.lang_of_test_id(
            "setUpClass (tests.test_live.LiveTests)"))

    def test_fake_all_skip_fails(self):
        # Every selected test skipped (e.g. missing adapter deps): the
        # gate must fail, naming the starved language.
        selected = [_tid(meth="test_01_py_a"), _tid(meth="test_02_py_b")]
        skipped = [(tid, "existing dependency required: /x/venv")
                   for tid in selected]
        ok, message, summary = _live_home.check_live_nonzero(
            selected, _StubResult(skipped), required=("py",))
        self.assertFalse(ok)
        self.assertIn("py", message)
        self.assertEqual(summary["executed"], 0)
        self.assertEqual(summary["skipped"], 2)

    def test_mixed_skip_and_pass_satisfies_language(self):
        # One browser test unavailable while another executes: normal
        # isolated skip, gate stays green.
        selected = [_tid(meth="test_04_browser_a"),
                    _tid(meth="test_22_browser_b")]
        skipped = [(selected[0], "Chrome unavailable")]
        ok, _message, summary = _live_home.check_live_nonzero(
            selected, _StubResult(skipped), required=("browser",))
        self.assertTrue(ok)
        self.assertEqual(summary["executed"], 1)
        self.assertEqual(summary["skipped"], 1)
        self.assertEqual(
            summary["per_lang"]["browser"],
            {"selected": 2, "skipped": 1, "executed": 1})

    def test_class_level_adapter_skip_covers_whole_class(self):
        # setup_home raising SkipTest reports one placeholder for the
        # class; it must starve every language selected in that class.
        selected = [_tid(meth="test_01_py_a"),
                    _tid(meth="test_19_node_b"),
                    _tid(meth="test_03_shared")]
        skipped = [("setUpClass (tests.test_live.LiveTests)",
                    "existing dependency required: /x/venv")]
        ok, _message, summary = _live_home.check_live_nonzero(
            selected, _StubResult(skipped), required=("py", "node"))
        self.assertFalse(ok)
        self.assertEqual(summary["executed"], 0)
        self.assertEqual(summary["per_lang"]["py"]["executed"], 0)
        self.assertEqual(summary["per_lang"]["node"]["executed"], 0)

    def test_shared_tests_never_satisfy_a_language(self):
        selected = [_tid(meth="test_01_parallel")]
        ok, _message, _summary = _live_home.check_live_nonzero(
            selected, _StubResult(), required=("py",))
        self.assertFalse(ok)

    def test_zero_selected_scope_fails(self):
        ok, message, summary = _live_home.check_live_nonzero(
            [], _StubResult(), required=("py",))
        self.assertFalse(ok)
        self.assertIn("zero tests", message)
        self.assertEqual(summary["selected"], 0)

    def test_real_run_mixed_suite_passes_programmatically(self):
        # End-to-end on real unittest objects (no output parsing): one
        # pass + one skip executes exactly one test for the language.
        class Demo(unittest.TestCase):
            def check_py_pass(self):
                pass

            def check_py_skip(self):
                raise unittest.SkipTest("Chrome unavailable")

        suite = unittest.TestSuite(
            [Demo("check_py_pass"), Demo("check_py_skip")])
        # Collect before run: TestSuite.run replaces entries with None.
        ids = [t.id() for t in _live_home.iter_suite_tests(suite)]
        result = unittest.TestResult()
        suite.run(result)
        self.assertEqual(len(ids), 2)
        ok, _message, summary = _live_home.check_live_nonzero(
            ids, result, required=("py",))
        self.assertTrue(ok)
        self.assertEqual(summary["executed"], 1)
        self.assertEqual(summary["skipped"], 1)

    def test_real_run_class_skip_fails_programmatically(self):
        class DemoCls(unittest.TestCase):
            @classmethod
            def setUpClass(cls):
                raise unittest.SkipTest(
                    "existing dependency required: /x/venv")

            def check_py_a(self):
                pass

        suite = unittest.TestSuite([DemoCls("check_py_a")])
        ids = [t.id() for t in _live_home.iter_suite_tests(suite)]
        result = unittest.TestResult()
        suite.run(result)
        ok, _message, summary = _live_home.check_live_nonzero(
            ids, result, required=("py",))
        self.assertFalse(ok)
        self.assertEqual(summary["executed"], 0)


if __name__ == "__main__":
    unittest.main()
