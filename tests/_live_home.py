"""Shared isolated-HOME fixture for the three class-level live suites.

Adopted ONLY by tests/test_live.py, tests/test_m5_live.py and
tests/test_ux_live.py (the files with class-level setUpClass isolated HOME
today). Per-test TemporaryDirectory unit files
(test_close_under_load.py, test_main_exit_visibility.py,
test_wait_capture.py, test_breaks_concurrency_matrix.py,
test_error_attribution.py) keep their own tmpdirs untouched.

Ownership/cleanup contract (ai-native-workflow isolated runtime):
- owns a temp HOME, isolated copies of the REAL venv/node_modules (never
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
import shutil
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / "target/debug/agent-debugger"


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


MACOS_CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
LINUX_CHROME_BINS = ("google-chrome", "google-chrome-stable",
                     "chromium", "chromium-browser")


def find_chrome():
    """Chrome binary for live browser tests, or None (caller skips).

    macOS app path first (historical priority, unchanged behavior there),
    then Linux PATH binaries (google-chrome on GitHub runners, chromium
    variants on distros). No guessing beyond these well-known names:
    unknown setups keep skipping instead of failing.
    """
    if Path(MACOS_CHROME).exists():
        return MACOS_CHROME
    for name in LINUX_CHROME_BINS:
        found = shutil.which(name)
        if found:
            return found
    return None


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


def unknown_test_langs():
    """TEST_LANG entries outside py|node|java|browser. Typos fail fast
    instead of silently running the full scope (an empty filtered set
    reinterprets as "default full" below)."""
    wanted = {l.strip().lower()
              for l in os.environ.get("TEST_LANG", "").split(",")
              if l.strip()}
    return sorted(wanted - set(LANG_TOKENS))


def load_tests(loader, tests, ignore):
    """unittest hook: honor TEST_LANG / SKIP_BROWSER without a runner."""
    unknown = unknown_test_langs()
    if unknown:
        raise ValueError(
            f"unknown TEST_LANG entries: {', '.join(unknown)} "
            f"(want py,node,java,browser)")
    suite = unittest.TestSuite()

    def walk(t):
        if isinstance(t, unittest.TestSuite):
            for sub in t:
                walk(sub)
        elif _wanted(t._testMethodName):
            suite.addTest(t)

    walk(tests)
    return suite


# Live nonzero policy (final-review hardening): the live gate must never
# pass all-skipped. Missing adapter dependencies surface as skips
# (setup_home raises SkipTest; single tests skip when chrome/node is
# absent), so a bare `unittest` exit 0 proves nothing when every test
# skipped. The canonical live entry (tests/run_live.py, wired into
# scripts/run_gates.sh --live) enforces this programmatically on unittest
# objects — never by parsing locale-dependent console output:
#
# - default full (no TEST_LANG): py + node + java are required given the
#   doctor prerequisites (venv debugpy, node, javac); browser is required
#   unless SKIP_BROWSER=1 explicitly opts out;
# - explicit TEST_LANG=...: every requested language must execute at
#   least one test (shared cross-cutting tests never satisfy a language);
# - the selected scope as a whole must execute at least one test.
#
# Normal isolated skips still pass (e.g. one browser test skipping while
# another browser test executes); only a zero-executed language (or an
# empty scope) fails.
CORE_LIVE_LANGS = ("py", "node", "java")

ADAPTER_DEP_SKIP_MARK = "existing dependency required"


def _skip_browser_on():
    return os.environ.get("SKIP_BROWSER", "").strip().lower() in _SKIP_TRUE


def selected_live_langs():
    """Required languages for this invocation: the explicit TEST_LANG
    selection, or the default-full policy (core + browser unless
    SKIP_BROWSER=1)."""
    wanted = {l.strip().lower()
              for l in os.environ.get("TEST_LANG", "").split(",")
              if l.strip()}
    wanted = {l for l in wanted if l in LANG_TOKENS}
    if wanted:
        return tuple(sorted(wanted))
    langs = list(CORE_LIVE_LANGS)
    if not _skip_browser_on():
        langs.append("browser")
    return tuple(langs)


def lang_of_test_id(test_id):
    """Language owning a test id, or None for cross-cutting (shared)
    tests. Same token rule as _wanted (method/class/module names embed
    _py_/_node_/_java_/_browser_ or the full word)."""
    lname = str(test_id).lower()
    for lang, toks in LANG_TOKENS.items():
        if any(t in lname for t in toks):
            return lang
    return None


def iter_suite_tests(suite):
    """Yield every leaf test in a (possibly nested) suite. Collect before
    TestSuite.run: CPython replaces executed entries with None after the
    run (memory cleanup), so post-run iteration is unreliable; Nones are
    skipped defensively either way."""
    stack = [suite]
    while stack:
        t = stack.pop()
        if t is None:
            continue
        if isinstance(t, unittest.TestSuite):
            stack.extend(list(t))
        else:
            yield t


def _skip_covers(skip_test_str, selected_ids):
    """Test ids covered by one result.skipped entry. Real-test skips cover
    exactly that test (matched via .id(), not str(): str(TestCase) is
    "method (Class)" while .id() is the dotted path); a setUpClass
    placeholder covers that class's selected tests; an unparseable
    placeholder conservatively covers everything (loud, never a silent
    pass)."""
    try:
        sid = skip_test_str.id()
    except AttributeError:
        sid = str(skip_test_str)
    if sid in selected_ids:
        return {sid}
    s = str(skip_test_str)
    if s.startswith("setUpClass (") and s.endswith(")"):
        prefix = s[len("setUpClass ("):-1] + "."
        covered = {tid for tid in selected_ids if tid.startswith(prefix)}
        return covered if covered else set(selected_ids)
    return set(selected_ids)


def check_live_nonzero(selected_ids, result, required=None):
    """Enforce the nonzero policy. selected_ids: post-filter test ids in
    scope; result: the unittest result after the run; required: override
    for selected_live_langs() (tests). Returns (ok, message, summary)
    where summary holds executed/skipped totals + per-language counts and
    is always safe to print (surface the counts even on PASS)."""
    required = tuple(required) if required is not None \
        else selected_live_langs()
    selected_ids = list(selected_ids)
    by_lang = {}
    for tid in selected_ids:
        by_lang.setdefault(lang_of_test_id(tid) or "shared", []).append(tid)
    covered = set()
    for skip_test, _reason in getattr(result, "skipped", []):
        covered |= _skip_covers(skip_test, set(selected_ids))
    per_lang = {}
    for lang in list(LANG_TOKENS) + ["shared"]:
        sel = len(by_lang.get(lang, []))
        sk = sum(1 for tid in by_lang.get(lang, [])
                 if tid in covered)
        per_lang[lang] = {"selected": sel, "skipped": sk,
                          "executed": sel - sk}
    executed_total = sum(1 for tid in selected_ids if tid not in covered)
    skipped_total = sum(1 for tid in selected_ids if tid in covered)
    summary = {"selected": len(selected_ids), "executed": executed_total,
               "skipped": skipped_total, "required": list(required),
               "per_lang": per_lang}
    missing = [lang for lang in required
               if per_lang.get(lang, {}).get("executed", 0) < 1]
    if not selected_ids:
        return (False, "live scope selected zero tests", summary)
    if missing:
        return (False,
                "live nonzero violation: zero executed tests for "
                f"required language(s) {missing} "
                f"(selected={len(selected_ids)} "
                f"executed={executed_total} "
                f"skipped={skipped_total})",
                summary)
    if executed_total < 1:
        return (False,
                "live nonzero violation: selected scope executed zero "
                f"tests (selected={len(selected_ids)} "
                f"skipped={skipped_total})",
                summary)
    return (True,
            f"live nonzero ok: executed={executed_total} "
            f"skipped={skipped_total} "
            f"required={list(required)}",
            summary)


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
        # The production provisioner deliberately rejects symlinked adapter
        # roots and generated dependency dirs. Copy the existing disposable
        # dependencies into the temp HOME instead of linking the real home:
        # the copied venv's internal links remain valid on the same machine,
        # while its adapter root and `venv`/`node_modules` leaves are real
        # directories. No package installation or real-home mutation.
        for lang, entry in [("python", "venv"), ("node", "node_modules")]:
            origin = Path.home() / ".agent-debugger/adapters" / lang / entry
            if not origin.exists():
                raise unittest.SkipTest(f"existing dependency required: {origin}")
            dest = adapters / lang / entry
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copytree(origin, dest, symlinks=True)
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
        try:
            result = subprocess.run(
                [str(mod.BIN), "--session", name, *args],
                env=env or cls.env, cwd=cwd or mod.ROOT,
                capture_output=True, text=True, timeout=timeout)
            data = json.loads(result.stdout)
            if ok and (result.returncode or not data.get("ok")):
                raise AssertionError(f"{name} {args}: {data}, stderr={result.stderr}")
            return data.get("data", data)
        finally:
            # Bundle the session's small artifacts after every call
            # (including the failing one): close deletes the session dir,
            # so a post-hoc copy at cleanup finds nothing. Best effort,
            # never breaks the call.
            try:
                cls._refresh_live_bundle(name)
            except Exception:
                pass

    @classmethod
    def _bundle_root(cls):
        return Path(os.environ.get(
            "LIVE_BUNDLE_DIR",
            str(Path(tempfile.gettempdir()) / "live-failure-bundles")))

    @classmethod
    def _copy_session_artifacts(cls, sdir, bdir):
        """Copy the small text artifacts session dir -> bundle dir
        (atomic renames; bones: bridge.log/session.json/error.json/
        stops.json, each capped). Never raises."""
        for artifact in ("bridge.log", "session.json",
                         "error.json", "stops.json"):
            try:
                data = (sdir / artifact).read_bytes()
            except OSError:
                continue
            try:
                bdir.mkdir(parents=True, exist_ok=True)
                part = bdir / (artifact + ".part")
                part.write_bytes(data[:262144])
                os.replace(part, bdir / artifact)
            except OSError as e:
                print(f"live diagnostics: bundle copy failed: {e}",
                      file=sys.stderr)

    @classmethod
    def _refresh_live_bundle(cls, name):
        home = getattr(cls, "home", None)
        if home is None:
            return
        sdir = Path(home) / ".agent-debugger" / "sessions" / name
        if not sdir.is_dir():
            return
        cls._copy_session_artifacts(
            sdir, cls._bundle_root() / cls.__name__ / name)

    def track(self, name):
        self.sessions.add(name)
        # Instance-level record of THIS test's sessions: the class set
        # loses names in test finally-blocks before cleanup runs, so the
        # failure bundle reads this list, not the set delta.
        try:
            self._live_test_sessions.append(name)
        except AttributeError:
            self._live_test_sessions = [name]
        return name

    # -- failure diagnostics (bounded, redacted, never masks the error) --
    def setUp(self):
        outcome = getattr(self, "_outcome", None)
        result = getattr(outcome, "result", None) if outcome else None
        self._live_problems_before = (
            len(result.failures) + len(result.errors)) if result else 0
        self._live_test_sessions = []
        try:
            self._live_sessions_before = set(type(self).sessions)
        except (AttributeError, TypeError):
            self._live_sessions_before = set()
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
            # This test's sessions from both records: explicit track() hits
            # plus names added straight to the class set (test_live.py
            # never calls track()). The union covers sessions that never
            # answered (no per-call refresh ran) as well as closed ones.
            names = list(getattr(self, "_live_test_sessions", None) or [])
            try:
                fresh = set(type(self).sessions) - set(
                    getattr(self, "_live_sessions_before", set()))
            except (AttributeError, TypeError):
                fresh = set()
            names.extend(sorted(fresh - set(names)))
            type(self).dump_failure_bundle(names or None)
        except Exception as e:
            print(f"live diagnostics bundle failed: {e}", file=sys.stderr)

    @classmethod
    def dump_failure_bundle(cls, names=None):
        """Print artifact paths + redacted bridge.log tails for the given
        sessions (default: first 3 of the class set, max 3). Never reads
        target env; never raises. Also copies
        the small text artifacts into a stable bundle dir outside the temp
        HOME (which vanishes on process exit): CI uploads that dir as an
        artifact, so the next failure ships its own forensics."""
        try:
            sessions_root = cls.home / ".agent-debugger" / "sessions"
            if names is None:
                names = sorted(cls.sessions)[:3]
            else:
                names = sorted(set(names))[:3]
            if not names:
                print("live diagnostics: no tracked sessions", file=sys.stderr)
                return
            bundle_root = cls._bundle_root()
            tag = cls.__name__
            for name in names:
                sdir = sessions_root / name
                print(f"live diagnostics: session={name} dir={sdir}",
                      file=sys.stderr)
                bdir = bundle_root / tag / name
                for artifact in ("bridge.log", "session.json",
                                 "error.json", "stops.json"):
                    print(f"live diagnostics: {name}/{artifact}: "
                          f"{sdir / artifact}", file=sys.stderr)
                # Same copy as the per-call refresh (covers sessions whose
                # dir is already gone: copies silently skip the missing).
                cls._copy_session_artifacts(sdir, bdir)
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
