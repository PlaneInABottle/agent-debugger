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


if __name__ == "__main__":
    unittest.main()
