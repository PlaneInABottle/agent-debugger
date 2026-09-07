"""Shared isolated-HOME fixture for the three class-level live suites.

Adopted ONLY by tests/test_live.py, tests/test_m5_live.py and
tests/test_ux_live.py (the files with class-level setUpClass isolated HOME
today). Per-test TemporaryDirectory unit files
(test_close_under_load.py, test_main_exit_visibility.py,
test_wait_capture.py, test_breaks_concurrency_matrix.py,
test_error_attribution.py) keep their own tmpdirs untouched.

Ownership/cleanup contract (ai-native-workflow isolated runtime):
- owns a temp HOME, symlinks of the REAL venv/node_modules (never
  installs), one fixture dir, tracked sessions, and task-owned
  chrome/http/log processes only;
- never collects target env, never kills unrelated processes:
  session cleanup goes through the CLI close command for tracked names
  only, chrome/http handles terminate only the processes this class
  started.
"""

import os
import json
import re
import socket
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / "target/debug/agent-debugger"


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


# TEST_LANG=py|node|java|browser (comma-separated, or unset/"all" for the
# full file). A test belongs to a language when its method name carries the
# token (live names embed _py_/_node_/_java_/_browser_ or the full word);
# cross-cutting tests with no token always run. SKIP_BROWSER=1 drops the
# browser tests (unit JS browser suites are stubbed-transport and always
# run; this flag is for live chrome only).
LANG_TOKENS = {
    "py": ("_py_", "python"),
    "node": ("_node_", "node"),
    "java": ("_java_", "java"),
    "browser": ("_browser_", "browser"),
}

_SKIP_TRUE = ("1", "true", "yes")


def _wanted(method_name):
    lname = method_name.lower()
    if os.environ.get("SKIP_BROWSER", "").strip().lower() in _SKIP_TRUE:
        if any(t in lname for t in LANG_TOKENS["browser"]):
            return False
    wanted = {l.strip().lower() for l in os.environ.get("TEST_LANG", "").split(",") if l.strip()}
    wanted = {l for l in wanted if l in LANG_TOKENS}
    if not wanted:
        return True
    if any(any(t in lname for t in LANG_TOKENS[lang]) for lang in wanted):
        return True
    # Cross-cutting (no language token at all): always runs.
    return not any(t in lname for toks in LANG_TOKENS.values() for t in toks)


def load_tests(loader, tests, ignore):
    """unittest hook: honor TEST_LANG / SKIP_BROWSER without a runner."""
    suite = unittest.TestSuite()

    def walk(t):
        if isinstance(t, unittest.TestSuite):
            for sub in t:
                walk(sub)
        elif _wanted(t._testMethodName):
            suite.addTest(t)

    walk(tests)
    return suite


_SECRET_RE = re.compile(
    r"(?i)(password|passwd|secret|token|api[_-]?key)(\s*[:=]\s*)\S+"
)

# JSON form: quoted keys with quoted (possibly spaced) or bare values,
# e.g. {"password": "hunter2"} — the bare pattern above cannot see these
# because the closing quote breaks its key/separator match.
_JSON_SECRET_RE = re.compile(
    r'(?i)("(?:password|passwd|secret|token|api[_-]?key)")(\s*:\s*)("[^"]*"|\S+)'
)


def sanitize(text):
    """Bounded secret masking for failure output (paths print raw: they
    live under the temp HOME and name only sessions/artifacts)."""
    text = _JSON_SECRET_RE.sub(r'\1\2"[redacted]"', text)
    return _SECRET_RE.sub(r"\1\2[redacted]", text)


class LiveHomeMixin:
    """Isolated-HOME setup/cleanup + failure bundle. Mix into the live
    TestCase; call setup_home() from setUpClass, then build fixtures."""

    @classmethod
    def _mod(cls):
        return sys.modules[cls.__module__]

    @classmethod
    def setup_home(cls, prefix, fixture_name):
        cls.tmp = tempfile.TemporaryDirectory(prefix=prefix)
        cls.home = Path(cls.tmp.name)
        cls.env = dict(os.environ, HOME=str(cls.home))
        cls.sessions = set()
        cls.chrome = None
        cls.http = None
        cls.log = (cls.home / "runtime.log").open("w")
        cls.addClassCleanup(cls.cleanup)
        adapters = cls.home / ".agent-debugger/adapters"
        for lang, entry in [("python", "venv"), ("node", "node_modules")]:
            origin = Path.home() / ".agent-debugger/adapters" / lang / entry
            if not origin.exists():
                raise unittest.SkipTest(f"existing dependency required: {origin}")
            dest = adapters / lang / entry
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.symlink_to(origin, target_is_directory=True)
        cls.fixture = cls.home / fixture_name
        cls.fixture.mkdir()

    @classmethod
    def cleanup(cls):
        # Tracked sessions only, via the CLI (never kills foreign trees).
        for name in list(cls.sessions):
            try:
                cls.cli(name, "close", timeout=85)
            except Exception:
                pass
        # Task-owned processes/servers only (started by this class).
        if cls.chrome is not None:
            cls.chrome.terminate()
            try:
                cls.chrome.wait(timeout=10)
            except subprocess.TimeoutExpired:
                cls.chrome.kill()
                cls.chrome.wait()
        if cls.http is not None:
            cls.http.shutdown()
            cls.http.server_close()
        cls.log.close()
        cls.tmp.cleanup()

    @classmethod
    def cli(cls, name, *args, timeout=30, ok=True, env=None, cwd=None):
        mod = cls._mod()
        result = subprocess.run(
            [str(mod.BIN), "--session", name, *args],
            env=env or cls.env, cwd=cwd or mod.ROOT,
            capture_output=True, text=True, timeout=timeout)
        data = json.loads(result.stdout)
        if ok and (result.returncode or not data.get("ok")):
            raise AssertionError(f"{name} {args}: {data}, stderr={result.stderr}")
        return data.get("data", data)

    def track(self, name):
        self.sessions.add(name)
        return name

    # -- failure diagnostics (bounded, redacted, never masks the error) --
    def setUp(self):
        outcome = getattr(self, "_outcome", None)
        result = getattr(outcome, "result", None) if outcome else None
        self._live_problems_before = (
            len(result.failures) + len(result.errors)) if result else 0
        self.addCleanup(self._live_home_dump_on_failure)

    def _live_home_dump_on_failure(self):
        try:
            outcome = getattr(self, "_outcome", None)
            result = getattr(outcome, "result", None) if outcome else None
            if result is None:
                return
            after = len(result.failures) + len(result.errors)
            if after <= getattr(self, "_live_problems_before", 0):
                return
            type(self).dump_failure_bundle()
        except Exception as e:
            print(f"live diagnostics bundle failed: {e}", file=sys.stderr)

    @classmethod
    def dump_failure_bundle(cls):
        """Print artifact paths + redacted bridge.log tails for tracked
        sessions (max 3). Never reads target env; never raises."""
        try:
            sessions_root = cls.home / ".agent-debugger" / "sessions"
            names = sorted(cls.sessions)[:3]
            if not names:
                print("live diagnostics: no tracked sessions", file=sys.stderr)
                return
            for name in names:
                sdir = sessions_root / name
                print(f"live diagnostics: session={name} dir={sdir}",
                      file=sys.stderr)
                for artifact in ("bridge.log", "session.json",
                                 "error.json", "stops.json"):
                    print(f"live diagnostics: {name}/{artifact}: "
                          f"{sdir / artifact}", file=sys.stderr)
                log = sdir / "bridge.log"
                try:
                    lines = log.read_text(errors="replace").splitlines()[-30:]
                    tail = sanitize("\n".join(lines))[-8192:]
                    print(f"live diagnostics: {name}/bridge.log tail:\n{tail}",
                          file=sys.stderr)
                except OSError as e:
                    print(f"live diagnostics: {name}/bridge.log unreadable: {e}",
                          file=sys.stderr)
        except Exception as e:
            print(f"live diagnostics bundle failed: {e}", file=sys.stderr)
