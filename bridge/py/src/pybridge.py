"""Python DAP bridge for agent-debugger (phase 3).

Mirrors JdiBridge session mode: a per-session daemon holding the debug
connection, speaking OUR session protocol (Content-Length JSON over TCP on
127.0.0.1) to the Rust CLI, and real DAP to a debugpy adapter child.

    pybridge.py session --kind launch --dir DIR --program app.py [--python PY]
        [--src D]... [--break SPEC]... [--logpoint SPEC]... [--timeout S]
        [-- args...]
    pybridge.py session --kind attach --dir DIR --host H --port P
        [--src D]... [--break SPEC]... [--timeout S]

Break forms: path:line[|cond] | method:funcname | exc (any uncaught).
DAP natively supports conditions (full Python expressions) and logMessage,
so --break conditions and --logpoint map one-to-one. --watch/--exit have no
DAP equivalent in debugpy and fail fast at parse time.

Snapshot shapes intentionally match the Java bridge (location/threads/
frames/locals/changed/stopInfo) so agents see one uniform surface.
"""

import json
import math
import os
import select
import socket
import subprocess
import sys
import threading
import time

MAX_STRING = 200
MAX_FIELDS = 20
MAX_VARS = 20
MAX_FRAMES = 10
MAX_THREADS = 8
MAX_OUTPUT = 4000
MAX_LOG_LINES = 2000
# Unexpected-crash payload cap (sanitized class + message + short traceback
# tail; never env/secrets — only the exception and our own frames).
MAX_ERROR_CHARS = 2048
# debugpy launcher version banners (never user data, just noise in logs).
BANNER_LINES = {"ptvsd", "debugpy"}


class Usage(Exception):
    pass


class BridgeErr(Exception):
    pass


class StopTimeout(BridgeErr):
    """First-stop wait timed out. Typed so attach can fall back to a live
    running session while launch still fails — never match by message."""
    pass


# ---------------------------------------------------------------- framing

def read_frame(conn):
    deadline = time.monotonic() + 5
    def recv(size):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise BridgeErr("frame read timed out")
        conn.settimeout(remaining)
        try:
            return conn.recv(size)
        except OSError as e:
            raise BridgeErr(f"frame read failed: {e}") from e
    header = b""
    while b"\r\n\r\n" not in header:
        chunk = recv(4096)
        if not chunk:
            raise BridgeErr("truncated frame")
        header += chunk
        if b"\r\n\r\n" not in header and len(header) >= 8192:
            raise BridgeErr("frame header too large")
    head, rest = header.split(b"\r\n\r\n", 1)
    if len(head) + 4 > 8192:
        raise BridgeErr("frame header too large")
    length = -1
    for line in head.decode("ascii", "replace").split("\r\n"):
        if ":" in line:
            name, val = line.split(":", 1)
            if name.strip().lower() == "content-length":
                if length != -1 or not val.strip().isascii() or not val.strip().isdigit():
                    raise BridgeErr("bad Content-Length")
                length = int(val.strip())
    if not 0 <= length <= 1024 * 1024:
        raise BridgeErr("invalid or oversized Content-Length")
    body = rest
    while len(body) < length:
        chunk = recv(min(65536, length - len(body)))
        if not chunk:
            raise BridgeErr("truncated frame")
        body += chunk
    try:
        req = json.loads(body[:length].decode("utf-8"))
        if not isinstance(req, dict):
            raise ValueError("expected object")
    except (ValueError, UnicodeError) as e:
        raise BridgeErr(f"bad frame: {e}") from e
    conn.settimeout(5)
    return req


def write_frame(conn, obj):
    body = json.dumps(obj).encode("utf-8")
    conn.sendall(b"Content-Length: %d\r\n\r\n" % len(body) + body)


def try_write_frame(conn, obj):
    """Best-effort response. A dead client (e.g. a connect+drop health
    check like our own `status` probe) must never kill the daemon: without
    this guard the fallback write itself raised out of serve()."""
    try:
        write_frame(conn, obj)
    except Exception:
        pass


# ---------------------------------------------------------------- DAP conn

class DapConn:
    """Minimal synchronous DAP client. Responses match by request_seq;
    unmatched events are stashed for pump() to consume.

    M5: one DAP reader per connection is NOT guaranteed across serve
    threads (concurrent pumps poll every live session), so all socket IO
    and stash access on this object serialize on `mu`. Dispatch/state work
    stays on Session._gate; lock order is always _gate -> mu, never the
    reverse (pump releases mu before dispatching)."""

    def __init__(self, sock):
        self.sock = sock
        self.sock.settimeout(5.0)
        self.seq = 0
        self.buf = b""
        self.stash = []
        self.mu = threading.RLock()

    def pop_stash(self):
        """Pop the oldest stashed message, or None. Mu-protected so two
        concurrent pumps never consume (or lose) the same message."""
        with self.mu:
            if not self.stash:
                return None
            return self.stash.pop(0)

    def _read_msg(self):
        with self.mu:
            return self._read_msg_locked()

    def _read_msg_locked(self):
        # NOTE: socket.timeout propagates RAW on purpose. request() sets the
        # socket timeout to its own deadline and lets it escape (wrapped by
        # dap_request); pump() catches it to re-check its deadline. Converting
        # here would break both deadline semantics.
        while True:
            if len(self.buf) > 64 * 1024 * 1024 + 8192:
                raise BridgeErr("DAP frame too large")
            if b"\r\n\r\n" in self.buf:
                head, rest = self.buf.split(b"\r\n\r\n", 1)
                if len(head) + 4 > 8192:
                    raise BridgeErr("DAP header too large")
                length = -1
                for line in head.decode("ascii", "replace").split("\r\n"):
                    if ":" in line:
                        name, val = line.split(":", 1)
                        if name.strip().lower() == "content-length":
                            try:
                                length = int(val.strip())
                            except ValueError:
                                length = -1
                if not 0 <= length <= 64 * 1024 * 1024:
                    raise BridgeErr("invalid DAP Content-Length")
                if len(rest) >= length:
                    try:
                        msg = json.loads(rest[:length].decode("utf-8"))
                        if not isinstance(msg, dict):
                            raise ValueError("expected object")
                    except (ValueError, UnicodeError) as e:
                        raise BridgeErr(f"invalid DAP message: {e}") from e
                    self.buf = rest[length:]
                    return msg
            elif len(self.buf) >= 8192:
                raise BridgeErr("DAP header too large")
            try:
                chunk = self.sock.recv(65536)
            except (socket.timeout, TimeoutError):
                raise
            if not chunk:
                raise BridgeErr("DAP connection closed by adapter")
            self.buf += chunk

    def send_only(self, command, args=None):
        """Fire a request without waiting (for launch: its response only
        arrives after configurationDone). Returns the seq."""
        with self.mu:
            return self._send_only_locked(command, args)

    def _send_only_locked(self, command, args=None):
        self.seq += 1
        body = json.dumps({"seq": self.seq, "type": "request",
                           "command": command,
                           "arguments": args or {}}).encode()
        try:
            self.sock.sendall(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        except OSError as e:
            raise BridgeErr(f"DAP send failed: {e}")
        return self.seq

    def request(self, command, args=None, timeout=30):
        with self.mu:
            return self._request_locked(command, args, timeout)

    def _request_locked(self, command, args=None, timeout=30):
        self.seq += 1
        mine = self.seq
        body = json.dumps({"seq": mine, "type": "request",
                           "command": command,
                           "arguments": args or {}}).encode()
        try:
            self.sock.sendall(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        except OSError as e:
            raise BridgeErr(f"DAP send failed: {e}")
        deadline = time.time() + timeout
        while True:
            for i, m in enumerate(self.stash):
                if m.get("type") == "response" and m.get("request_seq") == mine:
                    del self.stash[i]
                    if not m.get("success", False):
                        raise BridgeErr(m.get("message", f"{command} failed"))
                    return m.get("body", {})
            if time.time() > deadline:
                raise BridgeErr(f"DAP {command} timed out")
            self.sock.settimeout(max(0.1, deadline - time.time()))
            try:
                msg = self._read_msg()
            except BridgeErr:
                raise
            if msg.get("type") == "response" and msg.get("request_seq") == mine:
                if not msg.get("success", False):
                    raise BridgeErr(msg.get("message", f"{command} failed"))
                return msg.get("body", {})
            self.stash.append(msg)


# ---------------------------------------------------------------- session

class Config:
    def __init__(self):
        self.kind = None
        self.dir = None
        self.program = None
        self.module = None
        self.python = None
        self.host = "localhost"
        self.port = 0
        self.src_dirs = []
        self.breaks = []      # (path, line, cond|None)
        self.logpoints = []   # (path, line, template)
        self.methods = []     # func names
        self.want_exc = False
        self.timeout = 20.0
        self.prog_args = []
        self.observed_target = None  # redacted CLI-observed identity (dict)
        self.observed_hint = ""      # one-line redacted diagnostic hint
        # Stored raw spec per canonical line-break key, for remove/clear
        # echo (the CLI persists exactly these strings in stops.json).
        self.break_raws = {}  # (path, line, cond) -> raw spec
        # Opt-in multi-target: follow debugpyAttach children (launch only).
        # Off by default; launch then carries explicit subProcess:false
        # (debugpy 1.8.21 reports default-on, so silence needs the false).
        self.subprocess = False


def die(msg, code=2):
    sys.stdout.write(json.dumps({"error": msg}) + "\n")
    sys.stdout.flush()
    sys.exit(code)


# ---------------------------------------------------------------- path resolve
# Breakpoint paths are user-facing and relative to the CLI cwd (the target
# cwd never changes). Semantics: a path that names an existing file from
# the cwd wins as-is; otherwise an explicit --src root must locate it.
# Bare basenames (no directory part) get a bounded recursive search under
# each explicit --src root — never an implicit whole-repo scan. Anything
# else is joined onto each --src root (no basename guessing).

def _is_bare_name(raw):
    return ("/" not in raw and os.sep not in raw
            and (os.altsep is None or os.altsep not in raw))


def _find_basenames(name, src_dirs, limit=6):
    """Recursive basename matches under explicit roots (canonical, deduped).

    Skips symlinked directories (no -follow loop/hide surprises) and stops
    once ambiguity is proven (limit reached), so huge trees cost little."""
    found = []
    seen = set()
    for root in src_dirs:
        try:
            if not os.path.isdir(root):
                continue
        except (TypeError, ValueError):
            continue
        for dirpath, dirnames, filenames in os.walk(root, followlinks=False):
            dirnames[:] = sorted(
                d for d in dirnames
                if not os.path.islink(os.path.join(dirpath, d)))
            if name in filenames:
                cand = os.path.realpath(os.path.join(dirpath, name))
                if cand not in seen and os.path.isfile(cand):
                    seen.add(cand)
                    found.append(cand)
                    if len(found) >= limit:
                        return found
    return found


def resolve_source_path(raw, src_dirs):
    """Map a user-given path to a canonical file. Raises Usage (fail fast,
    before the target runs) when nothing — or more than one thing — matches."""
    srcs = [os.path.realpath(os.path.abspath(s)) for s in (src_dirs or [])]
    if os.path.isabs(raw):
        if os.path.isfile(raw):
            return os.path.realpath(os.path.abspath(raw))
        raise Usage(f"no such file: {raw} — check the path and try again")
    cwd_try = os.path.abspath(raw)
    if os.path.isfile(cwd_try):
        # Cwd wins over --src: preserves long-standing behavior for full
        # repo-relative paths like src/tests/.../test_x.py:378.
        return os.path.realpath(cwd_try)
    if _is_bare_name(raw):
        matches = _find_basenames(raw, srcs) if srcs else []
        if len(matches) == 1:
            return matches[0]
        if not matches:
            if srcs:
                where = (f"tried {cwd_try}; searched "
                         f"{', '.join(srcs)} with no match")
            else:
                where = f"tried {cwd_try}; no --src roots given to search"
            raise Usage(
                f"no such file: {raw} ({where}) — use the full path "
                f"relative to the current directory or pass --src <root> "
                f"containing {raw}")
        extra = "+" if len(matches) >= 6 else ""
        shown = "\n  ".join(matches[:5])
        raise Usage(
            f"ambiguous breakpoint path: {raw} matches "
            f"{len(matches)}{extra} files:\n  {shown}\nuse the full path "
            f"relative to the current directory or --src <root> to "
            f"disambiguate")
    tried = [cwd_try]
    for src in srcs:
        cand = os.path.normpath(os.path.join(src, raw))
        tried.append(cand)
        if os.path.isfile(cand):
            return os.path.realpath(cand)
    raise Usage(
        f"no such file: {raw} (tried {', '.join(tried)}) — use the full "
        f"path relative to the current directory or pass --src <root>")


def _physical_line_count(abspath):
    """Physical line total for range checks (UTF-8 with replacement; a file
    that cannot be read skips the check rather than failing the session)."""
    try:
        with open(abspath, encoding="utf-8", errors="replace") as f:
            return sum(1 for _ in f)
    except OSError:
        return None


def _check_line_range(abspath, lineno, raw):
    """Fail fast on lines no file can hold (line<1 or past EOF). DAP would
    otherwise 'verify' them by sliding to a nearby executable line, which
    hides the typo. Names the raw spec, the resolved path, and the total."""
    total = _physical_line_count(abspath)
    if total is None:
        return
    if lineno < 1 or lineno > total:
        raise Usage(f"no such line: {raw} — {abspath} has only "
                    f"{total} lines (line {lineno} out of range)")


def parse_break(spec, cfg):
    raw = spec
    cond = None
    if "|" in spec:
        spec, cond = spec.split("|", 1)
        cond = cond.strip()
        if not cond:
            raise Usage("empty condition after '|'")
    if spec == "exc" or spec.startswith("exc:"):
        if spec != "exc":
            raise Usage("Python adapter stops on any uncaught exception; "
                        f"class filter unsupported: {spec} (use bare 'exc')")
        cfg.want_exc = True
        return
    if spec.startswith("method:"):
        name = spec[len("method:"):]
        if not name:
            raise Usage("--break must look like method:funcname")
        cfg.methods.append(name)
        return
    if ":" not in spec:
        raise Usage("--break must look like path:line, method:func or exc")
    path, line = spec.rsplit(":", 1)
    try:
        lineno = int(line)
    except ValueError:
        raise Usage(f"bad line in --break: {raw}")
    resolved = resolve_source_path(path, cfg.src_dirs)
    _check_line_range(resolved, lineno, raw)
    cfg.breaks.append((resolved, lineno, cond))


def parse_logpoint(spec, cfg):
    # Class:line:template with {expr} holes (paths split like Class files).
    first = spec.find(":")
    second = spec.find(":", first + 1) if first >= 0 else -1
    if first <= 0 or second <= 0:
        raise Usage("--logpoint must look like path:line:template")
    path = resolve_source_path(spec[:first], cfg.src_dirs)
    try:
        lineno = int(spec[first + 1:second])
    except ValueError:
        raise Usage(f"bad line in --logpoint: {spec}")
    _check_line_range(path, lineno, spec)
    cfg.logpoints.append((path, lineno, spec[second + 1:]))


def _dedupe_startup_breaks(cfg):
    """Fold startup specs before any DAP traffic or target run (mirrors the
    live `breaks add` contract): an exact same path/line/cond repeat is
    idempotent (kept once); the same path/line with a different condition
    fails fast. Paths are already canonical (realpath), so different
    spellings of one file still collide."""
    kept = []
    seen = set()
    for path, line, cond in cfg.breaks:
        key = (path, line, cond)
        if key in seen:
            continue
        for other_path, other_line, other_cond in kept:
            if other_path == path and other_line == line:
                raise Usage(
                    f"conflicting condition for {path}:{line} "
                    f"(already requested"
                    f"{' as ' + repr(other_cond) if other_cond else ' plain'}): "
                    f"{path}:{line}"
                    f"{'|' + cond if cond else ''}")
        seen.add(key)
        kept.append((path, line, cond))
    cfg.breaks = kept
    seen_methods = set()
    cfg.methods = [m for m in cfg.methods
                   if not (m in seen_methods or seen_methods.add(m))]


def check_module_name(module):
    """Dotted `python -m` name, validated before the target runs (mirrors
    the CLI gate; each segment `[A-Za-z_][A-Za-z0-9_]*`)."""
    ok = bool(module) and all(
        len(seg) > 0 and (seg[0].isalpha() and seg[0].isascii() or seg[0] == "_")
        and all(c.isascii() and (c.isalnum() or c == "_") for c in seg[1:])
        for seg in module.split(".")
    )
    if not ok:
        raise Usage(f"bad --module '{module}' (want dotted.name like mypkg.mod)")


def parse_args(argv):
    cfg = Config()
    if not argv or argv[0] != "session":
        raise Usage("usage: pybridge.py session [options]")
    # Break/logpoint specs resolve against --src roots, but flags may arrive
    # in any order (--break before --src). Collect raw specs first, resolve
    # after the full flag set is known — a bad path still fails fast here,
    # before any target runs.
    pending_stops = []  # (kind, spec) in flag order
    i = 1
    dashdash = False
    while i < len(argv):
        a = argv[i]
        if dashdash:
            cfg.prog_args.append(a)
            i += 1
            continue
        if a == "--":
            dashdash = True
        elif a == "--kind":
            i += 1
            cfg.kind = argv[i]
        elif a == "--dir":
            i += 1
            cfg.dir = argv[i]
        elif a == "--program":
            i += 1
            cfg.program = os.path.abspath(argv[i])
        elif a == "--module":
            i += 1
            cfg.module = argv[i]
        elif a == "--python":
            i += 1
            cfg.python = argv[i]
        elif a == "--host":
            i += 1
            cfg.host = argv[i]
        elif a == "--port":
            i += 1
            cfg.port = int(argv[i])
        elif a == "--src":
            i += 1
            cfg.src_dirs.append(os.path.abspath(argv[i]))
        elif a == "--break":
            i += 1
            pending_stops.append(("break", argv[i]))
        elif a == "--logpoint":
            i += 1
            pending_stops.append(("logpoint", argv[i]))
        elif a == "--watch":
            raise Usage("--watch has no debugpy equivalent yet (Python)")
        elif a == "--exit":
            raise Usage("--exit has no debugpy equivalent yet (Python)")
        elif a == "--timeout":
            i += 1
            cfg.timeout = float(argv[i])
            if not math.isfinite(cfg.timeout) or not 0 < cfg.timeout <= 3600:
                raise Usage("timeout must be between 0 and 3600 seconds")
        elif a == "--subprocess":
            cfg.subprocess = True
        elif a == "--observed-target":
            i += 1
            try:
                parsed = json.loads(argv[i])
                cfg.observed_target = parsed if isinstance(parsed, dict) else None
            except ValueError:
                cfg.observed_target = None
        elif a == "--observed-hint":
            i += 1
            cfg.observed_hint = argv[i]
        else:
            raise Usage(f"unknown arg: {a}")
        i += 1
    if cfg.kind not in ("launch", "attach"):
        raise Usage("--kind must be launch or attach")
    if not cfg.dir:
        raise Usage("session needs --dir")
    if cfg.kind == "launch":
        if cfg.program and cfg.module:
            raise Usage("launch needs exactly one of --program and --module")
        if not cfg.program and not cfg.module:
            raise Usage("launch needs --program or --module")
        if cfg.module:
            check_module_name(cfg.module)
    if cfg.kind == "attach" and not cfg.port:
        raise Usage("attach needs --port")
    for kind, spec in pending_stops:
        if kind == "break":
            before = len(cfg.breaks)
            parse_break(spec, cfg)
            if len(cfg.breaks) > before:
                # Stored raw for remove/clear echo (dedup keeps first).
                cfg.break_raws.setdefault(cfg.breaks[-1], spec)
        else:
            parse_logpoint(spec, cfg)
    _dedupe_startup_breaks(cfg)
    if not cfg.python:
        cfg.python = sys.executable
    return cfg


# ---------------------------------------------------------------- value fmt

def trunc_str(s, limit=MAX_STRING):
    if len(s) <= limit:
        return s
    return f"{s[:limit]}… (+{len(s) - limit} more chars)"


# Multi-target bounds (frozen M-T contract): at most 8 live non-main
# targets; 16 entries of exited history (evictions count droppedExited).
MAX_ACTIVE_NONMAIN = 8
MAX_EXITED_HISTORY = 16
# Retained ignored (released, disconnected) entries are bounded too: small
# dicts, but a spawn loop must not grow the roster without limit.
MAX_IGNORED_RETAINED = 16
# Failed-handshake sockets can never be closed mid-session (closing a
# half-built debugpy session kills the adapter process, exit 1 — verified):
# they are retained OPEN in session ownership and closed only at overall
# cleanup. The list is bounded; once full, new children are marked ignored
# BEFORE any socket opens, so socket ownership stays bounded truthfully
# (in-flight ≤1 sequential + tracked ≤8 + retired ≤16).
MAX_RETIRED_SOCKETS = 16

# M5 concurrency (frozen): serve accepts connections concurrently on a small
# fixed pool of handler threads (one response per connection); a second
# same-target resume/mutation busy-rejects instead of silently queueing.
MAX_ACTIVE_HANDLERS = 8
RESUME_CMDS = ("continue", "step")
WAIT_CMDS = ("wait",)
CAPTURE_CMDS = ("capture",)
# Parked-stop UX: every parked response carries this warning (suspend
# semantics, HTTP handler impact). No root-cause claim, ever.
PARK_WARNING = ("parked breakpoint suspends target; HTTP handler remains "
                "open until continue/capture-resume/close(detach)")
MUTATION_CMDS = ("breaksAdd", "breaksRemove", "breaksClear")


class ChildTarget:
    """One debugpy child session: its own DAP connection plus its own park.

    Inherited global-break copies live in stop_states/_hitkeys (keyed by the
    same canonical key as the global intent); ephemeral target-scoped breaks
    live in target_raws (key -> raw) with scope "target" records.
    """

    def __init__(self, tid, pid, dap, sock, observed):
        self.id = tid
        self.pid = pid
        self.dap = dap
        self.sock = sock
        self.observed = observed
        self.thread_id = None
        self.frames = []
        self.suspended = False
        self.exited = False
        self.state = "running"
        self.last_stop = None
        self.stop_states = []
        self._hitkeys = []
        self.target_raws = {}    # canonical key -> raw (ephemeral scope)
        self.inherited_keys = set()  # global keys planted here as copies
        self.stop_seq = 0
        self.stop_info = None
        self.last_top = None
        self.last_func = None
        self.last_changed = "[]"
        self.logpoints = []  # inherited snapshot (resend companions)
        self._awaiting_continued = False
        self._suspects = []
        self._co_seen = set()
        self._suspect_warned = False


class Session:
    def __init__(self, cfg):
        self.cfg = cfg
        self.adapter = None
        self.adapter_port = 0
        self.dap = None
        self.server = None
        self.thread_id = None
        self.frames = []      # cached DAP stack frames of current stop
        self.suspended = False
        self.exited = False
        self.main_exited = False  # main DAP session over; children may live on
        self._last_park_target = "main"  # which target the last pump parked
        self.last_top = None
        self.last_func = None
        self.last_changed = "[]"
        self.stop_info = None
        self.output_tail = ""
        self.log_count = 0
        self.log_dropped = 0  # lifetime lines evicted by the log ring
        self.configured = False  # True once launch/attach handshake completes
        self.stop_states = []  # arm-time records served by `breaks`
        # Match keys aligned with stop_states by index: ("break", abspath,
        # requested, bound) | ("logpoint", abspath, requested, bound) |
        # ("method", funcname) | ("exc",). Requested is what the agent asked;
        # bound is where DAP actually planted it (slides differ). Hit
        # counting matches bound; the keys never leave the process (`breaks`
        # serves stop_states only).
        self._hitkeys = []
        self.session_port = 0  # our TCP port (set in main, for republishing)
        self.last_stop = None  # {"file","line","method"} of the latest stop
        # P5 multithread state machine (single DAP reader, no threads). A
        # parked stop is stable: co-stops from other threads count hits but
        # never move the park. After a resume, `stopped` events are suspects
        # until the adapter's `continued` proves the resume took effect
        # (pre-`continued` arrivals predate the resume and are stale).
        self._awaiting_continued = False
        self._suspects = []    # held pre-`continued` stopped events (bounded)
        self._co_seen = set()  # threadIds already attributed this park episode
        self._suspect_warned = False
        # -- multi-target roster (M-T): main keeps its own fields above;
        # children live in targets by opaque id (child:<pid>, never reused).
        self.targets = {}        # tid -> ChildTarget (live + ignored)
        self.target_order = []   # creation order for roster listing
        self._seen_ids = set()   # every id ever issued (no reuse)
        self.exited_targets = []  # bounded last-known entries (max 16)
        self.ignored = 0         # released (over-budget) children, lifetime
        self.helpers_released = 0  # spawn `-c` helpers, lifetime (no budget)
        self.dropped_exited = 0  # exited-history evictions, lifetime
        self._stop_seq = 0       # monotonic park clock for auto-select
        self._main_seq = 0       # seq of the main park (0 = never parked)
        self._serving = "main"   # target id of the in-flight command
        self._pending_target = None  # resume-wait owner for error attribution
        self._retired_sockets = []  # (tid, sock) failed handshakes, kept OPEN
        # -- M5 concurrency: _gate serializes swapped-field sections and
        # event-dispatch mutations (never held across select/pump waits, so
        # live snapshot reads stay prompt). Per-connection DAP IO serializes
        # on each DapConn.mu; lock order is always _gate -> mu.
        self._gate = threading.RLock()
        self._outstanding = {}   # tid -> resume cmd (continue/step) in flight
        self._active = 0         # live connection handlers (bounded)
        self._closing = False    # close accepted: only further closes served
        self._park_local = threading.local()  # per-pump parked target id
        # -- stop diagnostics (UX batch): session-monotonic stop id plus the
        # previous park for same-location/same-thread diagnosis. Globals (not
        # swapped per target): every park carries its own target stamp.
        self._stop_diag_seq = 0    # session-monotonic stop id
        self._prev_park = None     # previous park {target,file,line,threadId,atMs}
        self._stop_reason = None   # reason of the current park
        self._parked_at_ms = 0     # wall clock ms of the current park
        self._last_diag = None     # {target,stopId,sameLocation,sameThread,elapsedMs}

    # -- multi-target helpers

    def live_targets(self):
        """Live (non-exited) children in creation order."""
        return [self.targets[tid] for tid in self.target_order
                if tid in self.targets and not self.targets[tid].exited]

    def active_nonmain(self):
        """Budgeted children: live, tracked (ignored releases stay in the
        table with state ignored and never consume budget; helpers never
        enter the table at all)."""
        return [t for t in self.live_targets() if t.state in ("running", "stopped")]

    def resolve_target(self, req):
        """Which target a command serves: explicit id (validated), else the
        most recently stopped live target, else main. Acceptance-time
        auto-selection rides in req["_auto_target"] (stamped by dispatch)
        so concurrent handlers agree; it is still liveness-validated."""
        with self._gate:
            return self._resolve_target_inner(req)

    def _resolve_target_inner(self, req):
        want = req.get("target") if isinstance(req, dict) else None
        if want is None and isinstance(req, dict):
            auto = req.get("_auto_target")
            if auto is not None:
                inner = dict(req)
                inner["target"] = auto
                del inner["_auto_target"]
                return self._resolve_target_inner(inner)
        if want is not None:
            if want == "main":
                if self.exited:
                    raise BridgeErr("target main has exited — close this session")
                return "main"
            t = self.targets.get(want)
            if t is None:
                for e in self.exited_targets:
                    if e.get("id") == want:
                        raise BridgeErr(
                            f"target {want} has exited — close this session")
                raise BridgeErr(f"unknown target: {want}")
            if t.state == "ignored":
                raise BridgeErr(f"target {want} was released (over budget)")
            if t.exited or t.state == "exited":
                raise BridgeErr(f"target {want} has exited — close this session")
            return want
        best, best_seq = "main", (self._main_seq if self.suspended else 0)
        for t in self.live_targets():
            if t.suspended and t.stop_seq > best_seq:
                best, best_seq = t.id, t.stop_seq
        if best == "main" and self.exited:
            raise BridgeErr("target VM has exited — close this session")
        return best

    def _swap_fields(self, dst, src):
        """Exchange park/DAP fields between main (self) and a child."""
        for f in ("dap", "thread_id", "frames", "suspended", "stop_info",
                  "last_top", "last_func", "last_changed", "stop_states",
                  "_hitkeys", "_awaiting_continued", "_suspects", "_co_seen",
                  "_suspect_warned"):
            dst_v, src_v = getattr(dst, f), getattr(src, f)
            setattr(dst, f, src_v)
            setattr(src, f, dst_v)

    class _TargetScope:
        """Serve one command against a child using the main code paths.
        The scope holds Session._gate for its duration (M5: concurrent
        handlers must never swap shared park fields under each other);
        DAP IO inside takes DapConn.mu (order _gate -> mu, never reverse).
        Restores main state afterwards."""

        def __init__(self, session, tid):
            self.session = session
            self.tid = tid

        def __enter__(self):
            st = self.session
            st._gate.acquire()
            st._serving = self.tid
            if self.tid != "main":
                st._swap_fields(st, st.targets[self.tid])
            return self

        def __exit__(self, *exc):
            st = self.session
            try:
                if self.tid != "main" and self.tid in st.targets:
                    st._swap_fields(st, st.targets[self.tid])
                st._serving = "main"
            finally:
                st._gate.release()
            return False

    def stamp(self, resp, tid):
        """Every served response names its target (no silent rerouting)."""
        if isinstance(resp, dict) and "target" not in resp:
            resp["target"] = tid
        return resp

    def target_entry(self, tid):
        """One roster entry {id,kind,pid,state,lastStop,observed,scope}."""
        if tid == "main":
            return {"id": "main", "kind": "main", "pid": None,
                    "state": "exited" if self.exited
                    else ("stopped" if self.suspended else "running"),
                    "lastStop": self.last_stop,
                    "observed": self.cfg.observed_target,
                    "scope": "global"}
        t = self.targets[tid]
        scope = "target" if t.target_raws else "inherited"
        return {"id": t.id, "kind": "child", "pid": t.pid,
                "state": t.state, "lastStop": t.last_stop,
                "observed": t.observed, "scope": scope}

    def cmd_targets(self):
        with self._gate:
            entries = [self.target_entry("main")]
            for tid in self.target_order:
                if tid in self.targets:
                    entries.append(self.target_entry(tid))
            entries.extend(self.exited_targets)
            resp = {"ok": True, "targets": entries,
                    "selected": self._resolve_target_inner({}),
                    "ignored": self.ignored,
                    "helpersReleased": self.helpers_released,
                    "droppedExited": self.dropped_exited}
            return self.stamp(resp, "main")

    def _note_exit(self, tid, last_stop=None):
        """Move a live child to the bounded exited history (never reused).
        The child's socket is closed here: only fully-established sessions
        (attach drained) ever reach the table, and closing those is
        adapter-contained (Session[2] precedent); half-built sessions are
        never closed — they are abandoned open (closing those kills the
        adapter process, probe-verified)."""
        t = self.targets.pop(tid, None)
        if t is None:
            return
        try:
            if t.sock is not None:
                t.sock.close()
        except Exception:
            pass
        t.exited = True
        t.state = "exited"
        t.dap = None
        t.sock = None
        if last_stop is not None:
            t.last_stop = last_stop
        entry = {"id": t.id, "kind": "child", "pid": t.pid,
                 "state": "exited", "lastStop": t.last_stop,
                 "observed": t.observed,
                 "scope": "target" if t.target_raws else "inherited"}
        self.exited_targets.append(entry)
        while len(self.exited_targets) > MAX_EXITED_HISTORY:
            del self.exited_targets[0]
            self.dropped_exited += 1

    def dap_request(self, command, args=None, timeout=30):
        try:
            return self.dap.request(command, args or {}, timeout)
        except BridgeErr:
            raise
        except (socket.timeout, TimeoutError):
            raise BridgeErr(f"DAP {command} timed out after {timeout:g}s")
        except Exception as e:
            raise BridgeErr(f"DAP {command} failed: {e}")

    def fetch_variables(self, ref):
        body = self.dap_request("variables", {"variablesReference": ref})
        return body.get("variables", [])

    def fmt_dap_value(self, vartype, value, ref, depth=1):
        """Java-parity formatting with the same caps. Complete literals
        (list/dict/str reprs) are used as-is; only opaque `<... object ...>`
        reprs are expanded one level into named children."""
        if value is None:
            return "null"
        v = trunc_str(str(value))
        if not ref or depth >= 2 or not v.startswith("<"):
            return v
        try:
            children = self.fetch_variables(ref)
        except BridgeErr:
            return v
        named = [c for c in children
                 if c.get("name") not in Session.PSEUDO_SCOPES
                 and not c.get("name", "").isdigit()][:MAX_FIELDS]
        if not named:
            return v
        inner = ", ".join(
            f"{c['name']}={self.fmt_dap_value(c.get('type', ''), c.get('value'), 0, depth + 1)}"
            for c in named)
        extra = len(children) - len(named)
        if extra > 0:
            inner += f", … (+{extra} more fields)" if inner else f"… (+{extra} more fields)"
        return f"{v}{{{inner}}}"

    def top_frame_id(self, index=0):
        return self.frame_entry(index)["id"]

    def frame_entry(self, index):
        if index < 0:
            raise BridgeErr(f"no frame {index} (have {len(self.frames)})")
        if index >= len(self.frames):
            self.refresh_frames(levels=index + 16)
        if index >= len(self.frames):
            raise BridgeErr(f"no frame {index} (have {len(self.frames)})")
        return self.frames[index]

    PSEUDO_SCOPES = {"special variables", "function variables", "class variables"}

    def frame_locals(self, index=0, limit=MAX_VARS):
        fid = self.top_frame_id(index)
        body = self.dap_request("scopes", {"frameId": fid})
        scopes = {s.get("name"): s for s in body.get("scopes", [])}
        # Module frames keep names under Globals; functions under Locals.
        # Fall back only when Locals yields nothing real (never mask a
        # legitimately empty function frame with module noise... except that
        # an empty view helps nobody, so Globals still wins over nothing).
        children = []
        if "Locals" in scopes:
            children = [v for v in self.fetch_variables(scopes["Locals"]["variablesReference"])
                        if v.get("name") not in self.PSEUDO_SCOPES]
        if not children and "Globals" in scopes:
            # Names hide one level deeper: function/class pseudo containers.
            merged = []
            for v in self.fetch_variables(scopes["Globals"]["variablesReference"]):
                name = v.get("name", "")
                if name == "special variables":
                    continue
                if name in self.PSEUDO_SCOPES and v.get("variablesReference"):
                    try:
                        merged.extend(
                            c for c in self.fetch_variables(v["variablesReference"])
                            if c.get("name") not in self.PSEUDO_SCOPES)
                    except BridgeErr:
                        pass
                elif name not in self.PSEUDO_SCOPES:
                    merged.append(v)
            children = [v for v in merged if not v.get("name", "").startswith("__")]
        out = []
        for var in children:
            if len(out) >= limit:
                out.append({"name": "…",
                            "note": f"+{len(children) - limit} more"})
                break
            out.append({
                "name": var.get("name", "?"),
                "type": var.get("type", "?"),
                "value": self.fmt_dap_value(var.get("type", ""), var.get("value"), var.get("variablesReference", 0)),
            })
        return out

    # -- snapshot builders (Java shapes)

    def rel_file(self, abspath):
        for src in self.cfg.src_dirs:
            try:
                rel = os.path.relpath(abspath, src)
                if not rel.startswith(".."):
                    return rel
            except ValueError:
                pass
        try:
            rel = os.path.relpath(abspath, os.getcwd())
            if not rel.startswith(".."):
                return rel
        except ValueError:
            pass
        return abspath

    def snippet(self, abspath, line):
        if line is None or line < 0 or not os.path.isfile(abspath):
            return []
        try:
            with open(abspath, encoding="utf-8", errors="replace") as f:
                lines = f.read().splitlines()
        except OSError:
            return []
        out = []
        for n in range(max(1, line - 5), min(len(lines), line + 5) + 1):
            out.append({"line": n, "current": n == line, "text": lines[n - 1]})
        return out

    def location_json(self):
        if not self.frames:
            return {"class": "?", "method": "?", "line": -1, "file": "?", "snippet": []}
        f = self.frames[0]
        src = (f.get("source") or {})
        path = src.get("path", "?")
        line = f.get("line", -1)
        return {"class": os.path.splitext(os.path.basename(path))[0] if path != "?" else "?",
                "method": f.get("name", "?"), "line": line,
                "file": self.rel_file(path), "snippet": self.snippet(path, line)}

    def threads_json(self, current=None):
        try:
            body = self.dap_request("threads", {})
        except BridgeErr:
            return []
        ths = body.get("threads", [])[:MAX_THREADS]
        out = [{"id": t["id"], "name": t.get("name", "?"),
                "current": current is not None and t["id"] == current} for t in ths]
        total = len(body.get("threads", []))
        if total > MAX_THREADS:
            out.append({"id": -1, "name": "…",
                        "note": f"+{total - MAX_THREADS} more threads"})
        return out

    def threads_dump(self):
        try:
            body = self.dap_request("threads", {})
        except BridgeErr as e:
            raise BridgeErr(f"cannot read threads: {e}")
        out = []
        for t in body.get("threads", [])[:15]:
            frames = []
            try:
                stb = self.dap_request("stackTrace", {"threadId": t["id"]})
                for i, f in enumerate(stb.get("stackFrames", [])[:MAX_FRAMES]):
                    frames.append({"index": i, "type": "?",
                                   "method": f.get("name", "?"),
                                   "line": f.get("line", -1)})
            except BridgeErr:
                pass
            out.append({"id": t["id"], "name": t.get("name", "?"),
                        "status": "?", "frames": frames})
        return out

    def frames_json(self, with_locals, max_frames=MAX_FRAMES, max_vars=MAX_VARS):
        out = []
        for i, f in enumerate(self.frames[:max_frames]):
            entry = {"index": i, "type": "?",
                     "method": f.get("name", "?"), "line": f.get("line", -1)}
            if with_locals and i == 0:
                entry["locals"] = self.frame_locals(0, max_vars)
            out.append(entry)
        return out

    def refresh_frames(self, levels=64, timeout=30):
        # Windowed fetch: full stacks on deep recursion cost transport on
        # EVERY stop; frame_entry() widens the window on demand.
        body = self.dap_request("stackTrace", {"threadId": self.thread_id,
                                               "startFrame": 0, "levels": levels},
                                timeout=timeout)
        self.frames = body.get("stackFrames", [])

    def snapshot(self):
        return {"mode": "session", "location": self.location_json(),
                "threads": self.threads_json(self.thread_id),
                "frames": self.frames_json(True),
                "output": self.output_tail[-MAX_OUTPUT:]}

    def track_changes(self):
        try:
            cur = {v["name"]: v["value"] for v in self.frame_locals(0)}
            func = self.frames[0].get("name", "?") if self.frames else "?"
        except BridgeErr:
            cur, func = {}, "?"
        if self.last_top is None or getattr(self, "last_func", None) != func:
            changed = sorted(cur)
        else:
            changed = sorted(n for n, v in cur.items() if self.last_top.get(n) != v)
        self.last_top = cur
        self.last_func = func
        self.last_changed = json.dumps(changed)

    def count_hits(self, reason, frame=None):
        """Attribute a stop to the records it fired (served as `hits` by
        `breaks`). Step landings are NOT hits — counting them would tell the
        agent dead breakpoints fire. Logpoints are DAP-native (fire
        invisibly), so their hits stay null: uncountable, not zero. Slid
        stops match their BOUND line (where DAP planted them), never the
        requested line. `frame` attributes one co-stop frame without touching
        the park; None means the parked top frame."""
        if reason == "step":
            return
        if reason == "exception":
            for rec in self.stop_states:
                if rec["kind"] == "exc" and isinstance(rec.get("hits"), int):
                    rec["hits"] += 1
            return
        if frame is None:
            if not self.frames:
                return
            frame = self.frames[0]
        f0 = frame
        src = (f0.get("source") or {})
        try:
            here = os.path.realpath(os.path.abspath(src.get("path", "")))
        except (TypeError, ValueError):
            return
        name = f0.get("name", "?")
        for rec, key in zip(self.stop_states, self._hitkeys):
            if not isinstance(rec.get("hits"), int):
                continue
            if key[0] == "break" and len(key) == 4:
                try:
                    if (os.path.realpath(os.path.abspath(key[1])) == here
                            and key[3] == f0.get("line")):
                        rec["hits"] += 1
                except (TypeError, ValueError):
                    pass
            elif key[0] == "break" and len(key) == 3:
                # Tolerance for pre-slide-era keys built by older tests.
                try:
                    if (os.path.realpath(os.path.abspath(key[1])) == here
                            and key[2] == f0.get("line")):
                        rec["hits"] += 1
                except (TypeError, ValueError):
                    pass
            elif key[0] == "method" and name == key[1]:
                rec["hits"] += 1

    def timeout_text(self, timeout):
        """Timeout message with the compact observed-identity hint (names
        the target, never claims root cause)."""
        msg = f"timeout: no stop within {timeout:g}s"
        if self.cfg.observed_hint:
            msg += f"; {self.cfg.observed_hint}"
        return msg

    def unresolved_summary(self):
        """One-line diagnosis for stops that never bound usefully.

        Pending line/logpoint records qualify, as do slid stops that never
        fired (a slide off a comment/blank onto nearby code usually means
        the requested line is not executable). A slid stop that fired, and
        an exactly-verified stop that simply was not reached, are not path
        problems and stay out — the latter keeps its plain timeout."""
        pend = [r for r in self.stop_states
                if r.get("kind") in ("break", "logpoint")
                and (r.get("state") == "pending"
                     or (r.get("state") == "slid"
                         and (r.get("hits") is None or r.get("hits") == 0)))]
        if not pend:
            return ""
        parts = []
        for r in pend:
            s = r.get("spec", "?")
            if r.get("detail"):
                s += f" ({r['detail']})"
            parts.append(s)
        return ("unresolved breakpoints: " + "; ".join(parts)
                + " — check the path names the executed file and the line "
                "is executable code (not a blank, comment, or def/class header)"
                + (f"; {self.cfg.observed_hint}" if self.cfg.observed_hint else ""))

    def publish_state(self, stopped, target=None):
        """Rewrite session.json so `status` shows live truth (parked stop +
        time) with zero prior memory. lastStop survives resume/exit — it
        answers 'where was I last', not 'where am I now'. The redacted
        observedTarget rides along verbatim (CLI-computed, atomic write).
        Child target parks/resumes never rewrite the main-focused file: the
        per-target last stop lives in the roster served by `targets`."""
        eff = target if target is not None else self._serving
        if eff != "main":
            if stopped and self.frames:
                try:
                    loc = self.location_json()
                    t = self.targets.get(eff)
                    if t is not None:
                        t.last_stop = {"file": loc.get("file", "?"),
                                       "line": loc.get("line", -1),
                                       "method": loc.get("method", "?")}
                except Exception:
                    pass
            return
        if stopped and self.frames:
            try:
                loc = self.location_json()
                self.last_stop = {"file": loc.get("file", "?"),
                                  "line": loc.get("line", -1),
                                  "method": loc.get("method", "?")}
            except Exception:
                pass
        write_file(os.path.join(self.cfg.dir, "session.json"), json.dumps(
            {"name": os.path.basename(self.cfg.dir), "kind": self.cfg.kind,
             "port": self.session_port, "stopped": stopped,
             "lastStop": self.last_stop, "updatedAt": int(time.time()),
             "observedTarget": self.cfg.observed_target}))

    # -- lifecycle

    def free_port(self):
        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
        s.close()
        return port

    def start_adapter(self):
        port = self.free_port()
        self.adapter_port = port
        log = open(os.path.join(self.cfg.dir, "adapter.log"), "w")
        self.adapter = subprocess.Popen(
            [self.cfg.python, "-m", "debugpy.adapter",
             "--host", "127.0.0.1", "--port", str(port)],
            stdout=log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL)
        deadline = time.time() + 20
        while time.time() < deadline:
            if self.adapter.poll() is not None:
                raise BridgeErr("debugpy adapter exited during startup (see adapter.log)")
            try:
                sock = socket.create_connection(("127.0.0.1", port), timeout=1)
                self.dap = DapConn(sock)
                return
            except OSError:
                time.sleep(0.1)
        raise BridgeErr("debugpy adapter did not listen in time")

    def handshake_launch(self):
        prog_args = ([self.cfg.program or self.cfg.module] + self.cfg.prog_args)
        self.dap_request("initialize", {"adapterID": "agent-debugger",
                                        "pathFormat": "path"})
        # No waiting here: the launch response only arrives after
        # configurationDone. It is collected by _drain_launch_response.
        # subProcess is ALWAYS explicit: True when following children, False
        # when main-only (M0: debugpy 1.8.21 reports default-on, so only an
        # explicit false suppresses child events — our own launch field, not
        # a target injection).
        if self.cfg.module:
            launch_args = {"module": self.cfg.module,
                           "args": self.cfg.prog_args,
                           "justMyCode": True,
                           "console": "internalConsole",
                           "subProcess": bool(self.cfg.subprocess)}
        else:
            launch_args = {"program": self.cfg.program,
                           "args": self.cfg.prog_args,
                           "justMyCode": True,
                           "console": "internalConsole",
                           "subProcess": bool(self.cfg.subprocess)}
        self.dap.send_only("launch", launch_args)
        self.arm_breakpoints()
        self.dap_request("configurationDone", {})
        # launch response arrives after configurationDone; drain it lazily
        # (wait_response inside next request would mismatch — consume now).
        self._drain_launch_response()
        self.configured = True
        _ = prog_args

    def _drain_response(self, command):
        deadline = time.time() + 30
        while time.time() < deadline:
            for i, m in enumerate(self.dap.stash):
                if m.get("type") == "response" and m.get("command") == command:
                    del self.dap.stash[i]
                    if not m.get("success", False):
                        raise BridgeErr(m.get("message", f"{command} failed"))
                    return
            self.dap.sock.settimeout(max(0.1, deadline - time.time()))
            try:
                msg = self.dap._read_msg()
            except (socket.timeout, TimeoutError):
                # Response side handles its own deadline; bubble timeouts as
                # BridgeErr everywhere else.
                raise BridgeErr(f"{command} response never arrived")
            if msg.get("type") == "response" and msg.get("command") == command:
                if not msg.get("success", False):
                    raise BridgeErr(msg.get("message", f"{command} failed"))
                return
            self.dap.stash.append(msg)
        raise BridgeErr(f"{command} response never arrived")

    def _drain_launch_response(self):
        self._drain_response("launch")

    def handshake_attach(self):
        # Direct to the --listen server (no adapter child): same pipelined
        # shape as launch — attach answers only after configurationDone.
        try:
            sock = socket.create_connection((self.cfg.host, self.cfg.port), timeout=10)
        except OSError as e:
            raise BridgeErr(
                f"attach failed ({self.cfg.host}:{self.cfg.port}): {e} — is the target "
                f"started with debugpy --listen {self.cfg.port} ?")
        self.dap = DapConn(sock)
        self.dap_request("initialize", {"adapterID": "agent-debugger",
                                        "pathFormat": "path"})
        self.dap.send_only("attach", {"justMyCode": True})
        self.arm_breakpoints()
        self.dap_request("configurationDone", {})
        self._drain_response("attach")
        self.configured = True

    # -- child targets (M3: debugpyAttach + one DAP session per child)

    def _drain_child_response(self, dap, command, timeout=30):
        """Consume one child's pipelined response (attach answers only after
        configurationDone — waiting earlier deadlocks, per M0)."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            for i, m in enumerate(dap.stash):
                if m.get("type") == "response" and m.get("command") == command:
                    del dap.stash[i]
                    if not m.get("success", False):
                        raise BridgeErr(m.get("message", f"{command} failed"))
                    return
            dap.sock.settimeout(max(0.1, deadline - time.time()))
            try:
                msg = dap._read_msg()
            except (socket.timeout, TimeoutError):
                raise BridgeErr(f"{command} response never arrived")
            if msg.get("type") == "response" and msg.get("command") == command:
                if not msg.get("success", False):
                    raise BridgeErr(msg.get("message", f"{command} failed"))
                return
            dap.stash.append(msg)
        raise BridgeErr(f"{command} response never arrived")

    def _child_cmdline(self, pid):
        """Kernel-observed command line of a child pid (no target eval, no
        env): /proc on Linux, `ps` on macOS. Best-effort — failure reads as
        empty (caller tracks normally, the safe default)."""
        try:
            with open(f"/proc/{pid}/cmdline", "rb") as f:
                return f.read().replace(b"\0", b" ").decode("utf-8", "replace")
        except OSError:
            pass
        try:
            out = subprocess.run(
                ["ps", "-p", str(pid), "-o", "args="],
                capture_output=True, text=True, timeout=5).stdout
            return out.strip()
        except Exception:
            return ""

    def _is_resource_tracker(self, pid):
        """Spot the multiprocessing `resource_tracker` daemon: a `-c` shim
        that runs no user code ever (verified: `... -c ... import pydevd;
        ...; from multiprocessing.resource_tracker import main;main(N)`).
        Deterministic (exec-time cmdline, no import race) unlike a stack
        probe — spawn_main workers also start as `-c` and MUST be tracked.
        A miss simply tracks the child normally (harmless)."""
        try:
            return "resource_tracker" in self._child_cmdline(pid)
        except Exception:
            return False

    def _accept_child(self, body):
        """Fold one debugpyAttach event into the roster (launch + opt-in
        only). Never raises: failures record a bounded history entry instead
        of breaking the pump. Release law (debugpy 1.8.21, adapter-log
        verified): a child DAP connection is NEVER closed while the session
        lives — not via DAP `disconnect` (finalizes the shared session) and
        not via raw close mid-handshake (kills the adapter process,
        exit 1). Release paths: the resource_tracker gets NO connection at
        all (its early server is never suspended — it runs free and exits
        on its own); over-budget real children get a MINIMAL handshake (no
        breaks) plus an established-session close, so they are configured
        (a never-configured server suspends forever) with zero breakpoints
        and can never park. Failed handshakes retire OPEN in bounded
        ownership, closed only at overall cleanup."""
        if not self.cfg.subprocess or self.cfg.kind != "launch":
            return
        if not isinstance(body, dict):
            return
        pid = body.get("subProcessId")
        if isinstance(pid, bool) or not isinstance(pid, int):
            return
        tid = f"child:{pid}"
        if tid in self._seen_ids:
            return  # ids are never reused within a session
        self._seen_ids.add(tid)
        if self._is_resource_tracker(pid):
            # Short-lived daemon, no user code: skip (no socket, no
            # budget). Its server connects before any session matches, so
            # it is never suspended pending configuration — it runs free
            # and exits on its own (every mp run verifies this).
            self.helpers_released += 1
            sys.stderr.write(
                f"warn: released spawn helper for {tid} (no budget used)\n")
            return
        if len(self.active_nonmain()) >= MAX_ACTIVE_NONMAIN:
            # Over budget: minimal handshake (initialize → verbatim attach
            # → NO breaks → configurationDone → drained), then raw close of
            # the ESTABLISHED session (adapter-contained) and release. The
            # child is configured with zero breakpoints so it can never
            # park — skipping the handshake instead would hang it (its
            # server suspends until configured). Counted, never parked.
            self._release_minimal(tid, pid, body)
            return
        if len(self._retired_sockets) >= MAX_RETIRED_SOCKETS:
            # Retired (failed-handshake) ownership is full and nothing may
            # be closed mid-session: mark ignored BEFORE opening. Only
            # reachable after 16 consecutive handshake failures (adapter
            # failure mode); documented residual hang risk there.
            self._mark_ignored(tid, pid, "retired ownership full")
            return
        try:
            sock = socket.create_connection(
                ("127.0.0.1", self.adapter_port), timeout=10)
        except OSError as e:
            sys.stderr.write(f"warn: cannot reach child {pid}: {e}\n")
            return
        dap = DapConn(sock)
        child = ChildTarget(tid, pid, dap, sock,
                            {"pid": pid, "source": "debugpy-subProcessId"})
        child.logpoints = list(self.cfg.logpoints)
        configured = False
        try:
            dap.request("initialize", {"adapterID": "agent-debugger",
                                       "pathFormat": "path"}, timeout=30)
            # The attach response arrives only after configurationDone:
            # send without waiting (waiting here deadlocks, per M0).
            dap.send_only("attach", dict(body))
            # Plant the global intent as inherited copies on the child
            # connection (same replace-per-file semantics as main).
            self._swap_fields(self, child)
            try:
                self._arm_global()
            finally:
                self._swap_fields(self, child)
            child.inherited_keys = set(self.cfg.break_raws)
            dap.request("configurationDone", {}, timeout=30)
            configured = True
            self._drain_child_response(dap, "attach")
            child.state = "running"
            self.targets[tid] = child
            self.target_order.append(tid)
            sys.stderr.write(f"target: {tid} attached (pid {pid})\n")
        except BridgeErr as e:
            # Never strand a pre-configurationDone child (it would wait
            # forever): one configurationDone, then RETIRE the socket OPEN
            # (closing a half-built session kills the adapter process) and
            # record bounded history. Ownership is bounded: the pre-connect
            # gate above stops new opens once the retired list is full.
            if not configured:
                try:
                    dap.request("configurationDone", {}, timeout=5)
                except Exception:
                    pass
            self._retired_sockets.append((tid, sock))
            sys.stderr.write(f"warn: child {tid} handshake failed: {e}\n")
            child.exited = True
            child.state = "exited"
            self.exited_targets.append(
                {"id": tid, "kind": "child", "pid": pid,
                 "state": "exited", "lastStop": None,
                 "observed": child.observed, "scope": "inherited"})
            while len(self.exited_targets) > MAX_EXITED_HISTORY:
                del self.exited_targets[0]
                self.dropped_exited += 1

    def _release_minimal(self, tid, pid, body):
        """Over-budget release: full minimal handshake (initialize →
        verbatim attach → NO setBreakpoints → configurationDone → drained
        attach response) then raw close of the ESTABLISHED session and a
        socket-less ignored record. Established closes are
        adapter-contained (Session[2] precedent); half-built closes are
        not, so ANY failure retires the socket OPEN instead (bounded).
        The child runs free with zero breakpoints and can never park."""
        try:
            sock = socket.create_connection(
                ("127.0.0.1", self.adapter_port), timeout=10)
        except OSError as e:
            sys.stderr.write(f"warn: cannot reach child {pid}: {e}\n")
            return
        dap = DapConn(sock)
        configured = False
        try:
            dap.request("initialize", {"adapterID": "agent-debugger",
                                       "pathFormat": "path"}, timeout=30)
            dap.send_only("attach", dict(body))
            dap.request("configurationDone", {}, timeout=30)
            configured = True
            self._drain_child_response(dap, "attach")
            try:
                sock.close()
            except Exception:
                pass
            self._mark_ignored(tid, pid, "over budget")
        except BridgeErr as e:
            if not configured:
                try:
                    dap.request("configurationDone", {}, timeout=5)
                except Exception:
                    pass
            self._retired_sockets.append((tid, sock))
            sys.stderr.write(f"warn: child {tid} release failed: {e}\n")
            self.exited_targets.append(
                {"id": tid, "kind": "child", "pid": pid,
                 "state": "exited", "lastStop": None,
                 "observed": {"pid": pid, "source": "debugpy-subProcessId"},
                 "scope": "inherited"})
            while len(self.exited_targets) > MAX_EXITED_HISTORY:
                del self.exited_targets[0]
                self.dropped_exited += 1

    def _mark_ignored(self, tid, pid, why):
        """Release without a connection: socket-less roster record (never
        parked, never served), lifetime counter, bounded retention."""
        child = ChildTarget(tid, pid, None, None,
                            {"pid": pid, "source": "debugpy-subProcessId"})
        child.exited = True
        child.state = "ignored"
        self.targets[tid] = child
        self.target_order.append(tid)
        self.ignored += 1
        self._evict_old_ignored()
        sys.stderr.write(f"warn: child {tid} {why}: released\n")

    def _evict_old_ignored(self):
        """Bound retained ignored entries (counters stay lifetime)."""
        kept, dropped = [], []
        for tid in self.target_order:
            t = self.targets.get(tid)
            if t is not None and t.state == "ignored":
                dropped.append(tid)
        while len(dropped) > MAX_IGNORED_RETAINED:
            old = dropped.pop(0)
            self.targets.pop(old, None)
            try:
                self.target_order.remove(old)
            except ValueError:
                pass

    def _apply_verification(self, rec, got, requested, is_logpoint):
        """Fold one setBreakpoints answer into a record. Returns the bound
        line for _hitkeys. verified:true with a numeric line != requested
        means DAP slid the stop (comment/blank/decorator answers land on
        nearby executable code): state=slid with `slid to line X` detail
        (logpoint templates preserved). verified:false or a missing/short
        answer stays pending."""
        if not isinstance(got, dict):
            got = {}
        if not bool(got.get("verified", False)):
            rec["state"] = "pending"
            msg = got.get("message", "pending")
            if is_logpoint and "detail" in rec:
                rec["detail"] = f"{rec['detail']} ({msg})"
            elif not is_logpoint or "detail" not in rec:
                rec["detail"] = msg
            return requested
        bound = got.get("line")
        if isinstance(bound, bool) or not isinstance(bound, int):
            bound = None
        if bound is not None and bound != requested:
            rec["state"] = "slid"
            slide = f"slid to line {bound}"
            if is_logpoint and rec.get("detail"):
                if slide not in rec["detail"]:
                    rec["detail"] = f"{rec['detail']} ({slide})"
            else:
                rec["detail"] = slide
            return bound
        rec["state"] = "verified"
        if not is_logpoint and "detail" in rec:
            del rec["detail"]
        return requested

    def arm_breakpoints(self):
        # Entry point (startup + tests): arms the global intent on the
        # current DAP connection. Child inherits reuse _arm_global through
        # a target scope so copies plant on the child connection.
        self._arm_global()

    def _arm_global(self):
        # DAP setBreakpoints REPLACES a file's breakpoints per call, so line
        # breaks and logpoints for the same file merge into ONE request.
        # Every requested stop lands in self.stop_states (served by `breaks`)
        # — verification used to live only in stderr, invisible after
        # compaction.
        by_file = {}
        for path, line, cond in self.cfg.breaks:
            bp = {"line": line}
            if cond:
                bp["condition"] = cond
            by_file.setdefault(path, []).append(
                {"line": line, "bp": bp, "kind": "break", "cond": cond})
        for path, line, template in self.cfg.logpoints:
            by_file.setdefault(path, []).append(
                {"line": line, "bp": {"line": line, "logMessage": template},
                 "kind": "logpoint", "template": template})
        for path, items in by_file.items():
            body = self.dap_request("setBreakpoints",
                                    {"source": {"path": path},
                                     "breakpoints": [it["bp"] for it in items]})
            got_list = body.get("breakpoints", [])
            for idx, item in enumerate(items):
                # Defensive: if the adapter returns fewer entries than
                # requested, zip() would silently drop stops. Missing entries
                # report pending rather than vanishing.
                got = got_list[idx] if idx < len(got_list) else {}
                spec = f"{self.rel_file(path)}:{item['line']}"
                if item["kind"] == "break" and item.get("cond"):
                    spec += f"|{item['cond']}"
                rec = {"spec": spec, "kind": item["kind"],
                       "hits": 0 if item["kind"] == "break" else None}
                if item["kind"] == "logpoint":
                    # Resume needs the template ("what was I collecting?"),
                    # not just the line. Cheap (one short string).
                    rec["detail"] = item["template"]
                bound = self._apply_verification(
                    rec, got, item["line"], item["kind"] == "logpoint")
                if rec["state"] == "pending":
                    msg = rec.get("detail", "pending")
                    sys.stderr.write(
                        f"warn: breakpoint unverified: {path}:{item['line']} "
                        f"({msg})\n")
                elif rec["state"] == "slid":
                    sys.stderr.write(
                        f"warn: breakpoint slid: {path}:{item['line']} "
                        f"-> {bound}\n")
                self.stop_states.append(rec)
                self._hitkeys.append((item["kind"], path, item["line"], bound))
        if self.cfg.methods:
            # DAP setFunctionBreakpoints REPLACES the whole function-break
            # list per call (same replace semantics as setBreakpoints per
            # file), so all names go in ONE request — one call per method
            # would drop every predecessor. Each answer entry carries the
            # adapter's own receipt (verified/pending/message); methods
            # have no line to slide to, so no slide semantics are invented.
            body = self.dap_request(
                "setFunctionBreakpoints",
                {"breakpoints": [{"name": func}
                                 for func in self.cfg.methods]})
            got_list = body.get("breakpoints", [])
            for idx, func in enumerate(self.cfg.methods):
                # Defensive (same as line breaks): a short answer leaves
                # the missing entry pending rather than dropping the stop.
                got = got_list[idx] if idx < len(got_list) else {}
                if not isinstance(got, dict):
                    got = {}
                rec = {"spec": f"method:{func}", "kind": "method",
                       "hits": 0}
                if bool(got.get("verified", False)):
                    rec["state"] = "verified"
                else:
                    rec["state"] = "pending"
                    rec["detail"] = got.get("message", "pending")
                self.stop_states.append(rec)
                self._hitkeys.append(("method", func))
        if self.cfg.want_exc:
            self.dap_request("setExceptionBreakpoints", {"filters": ["uncaught"]})
            self.stop_states.append(
                {"spec": "exc", "kind": "exc", "state": "armed", "hits": 0})
            self._hitkeys.append(("exc",))

    def _live_conns(self):
        """All readable DAP sessions: main first, then live children. One
        reader (this thread) per connection — never a thread per target."""
        conns = []
        if self.dap is not None and not self.main_exited:
            conns.append(("main", self.dap))
        for t in self.live_targets():
            if t.dap is not None and t.state != "ignored":
                conns.append((t.id, t.dap))
        return conns

    def _conn_entries(self):
        """(tid, conn, sock-or-None) triples; test doubles without a real
        socket still drain through their stash."""
        out = []
        for tid, conn in self._live_conns():
            sock = getattr(conn, "sock", None)
            if sock is not None and not hasattr(sock, "fileno"):
                sock = None
            out.append((tid, conn, sock))
        return out

    def _try_read(self, conn):
        """Non-blocking read: returns a queued/buffered message, or None
        when no COMPLETE message is available without waiting. DapConn
        buffers internally, so a select-first loop would stall on coalesced
        segments forever — buffered data must be consumed before selecting.
        The socket timeout is preserved."""
        sock = getattr(conn, "sock", None)
        if sock is None:
            return None
        try:
            prev = sock.gettimeout()
        except OSError:
            return None
        try:
            try:
                sock.settimeout(0)
            except OSError:
                return None
            try:
                return conn._read_msg()
            except (socket.timeout, TimeoutError, BlockingIOError):
                return None
        finally:
            try:
                sock.settimeout(prev)
            except OSError:
                pass

    def _dispatch_pumped(self, msg, tid):
        """Route one message to its target's handler. Main keeps the
        single-argument call shape; children pass their id. Serialized on
        _gate (M5): concurrent pumps must never interleave park mutations
        or _TargetScope swaps. The wire (mu) is always released before
        entering here — lock order _gate -> mu."""
        with self._gate:
            if tid == "main":
                return self._handle_pumped(msg)
            return self._handle_pumped(msg, tid)

    def _pop_stash(self, conn):
        """Mu-protected stash pop (real DapConns) with a plain-list fallback
        for test doubles. Returns the oldest message or None."""
        pop = getattr(conn, "pop_stash", None)
        if pop is not None:
            return pop()
        stash = getattr(conn, "stash", None)
        if not stash:
            return None
        return stash.pop(0)

    def _conn_dead(self, tid, e):
        """A connection dropped outside an exited/terminated event."""
        with self._gate:
            self._conn_dead_inner(tid, e)

    def _conn_dead_inner(self, tid, e):
        if tid == "main":
            self.main_exited = True
            try:
                self.dap.sock.close()
            except Exception:
                pass
            self.dap = None
            if not self.live_targets():
                self.exited = True
                raise BridgeErr(f"lost connection to target: {e}")
            # Independent child sessions survive the parent conn: keep
            # pumping them; main commands now fail with the exited error.
            self.publish_state(False)
            return
        self._note_exit(tid)

    def _probe_all_suspects(self):
        """Park the first still-live held suspect on ANY target (main first,
        then children in creation order). Post-resume stops that arrive
        before the adapter's `continued` barrier are held per target; each
        target's episode resolves on its own connection. Returns 'stopped'
        on a park (the parked target is stamped by _park_stop), else None.
        Never clears barrier flags itself — callers close the episode (like
        the main-only path always did) so a later genuine stop parks at
        once."""
        if self._awaiting_continued and not self.suspended and self._suspects:
            r = self._probe_suspects()
            if r:
                return r
        for tid in self.target_order:
            t = self.targets.get(tid)
            if t is None or t.exited or t.dap is None:
                continue
            if t._awaiting_continued and not t.suspended and t._suspects:
                with self._TargetScope(self, tid):
                    r = self._probe_suspects()
                    if r:
                        return r
        return None

    def _close_suspect_episodes(self):
        """End every resume episode that produced no park (deadline path):
        mirrors the main-only close so later genuine stops park at once."""
        if self._awaiting_continued and not self.suspended:
            self._awaiting_continued = False
        for tid in self.target_order:
            t = self.targets.get(tid)
            if t is not None and t._awaiting_continued and not t.suspended:
                t._awaiting_continued = False

    def pump(self, timeout):
        """Wait for the next stopped/exited on ANY target; returns the
        parking target id (the parked target is also recorded in
        _last_park_target) or raises. Concurrent pumps (M5: independent
        resumes on different targets) share every connection: stash pops
        and wire reads are mu-protected, dispatch is gate-protected, and
        each message is consumed exactly once — but only the pump that
        consumes a stop parks it, so a rival pump keeps waiting."""
        deadline = time.time() + timeout
        for tid, conn, _sock in self._conn_entries():
            while True:
                msg = self._pop_stash(conn)
                if msg is None:
                    break
                r = self._dispatch_pumped(msg, tid)
                if r:
                    return r
        while True:
            if not am_owner(self.cfg.dir, self._nonce):
                # Abandoned mid-wait: take the target down with us (launch)
                # or detach (attach), then vanish. SystemExit bypasses the
                # `except Exception` error mappers on purpose.
                try:
                    self.cleanup()
                except Exception:
                    pass
                raise SystemExit(0)
            remaining = deadline - time.time()
            if remaining <= 0:
                # The resume produced only held suspects: park the first
                # still-live one on any target instead of timing out, then
                # close every episode either way so later genuine stops park
                # at once.
                r = self._probe_all_suspects()
                if r:
                    return r
                self._close_suspect_episodes()
                raise StopTimeout(self.timeout_text(timeout))
            entries = self._conn_entries()
            for tid, conn, _sock in entries:
                while True:
                    msg = self._pop_stash(conn)
                    if msg is None:
                        break
                    r = self._dispatch_pumped(msg, tid)
                    if r:
                        return r
            # Buffered/coalesced data before selecting: DapConn keeps an
            # internal buffer, so a select-first loop would stall on an
            # already-received message until the next kernel wakeup.
            progressed = False
            for tid, conn, _sock in entries:
                while True:
                    try:
                        msg = self._try_read(conn)
                    except BridgeErr as e:
                        if "closed" in str(e).lower():
                            try:
                                self._conn_dead(tid, e)
                            except BridgeErr as fatal:
                                raise fatal
                            break
                        raise
                    if msg is None:
                        break
                    progressed = True
                    r = self._dispatch_pumped(msg, tid)
                    if r:
                        return r
            if progressed:
                continue  # re-check deadline/owner before selecting
            socks = [s for _, _, s in entries if s is not None]
            if not socks:
                if not self.live_targets():
                    raise BridgeErr("lost connection to target: all sessions closed")
                time.sleep(min(remaining, 0.1))
                continue
            try:
                ready, _, _ = select.select(socks, [], [], min(remaining, 1.0))
            except (OSError, ValueError):
                continue  # a socket closed under us; re-list next tick
            ready_ids = {id(s) for s in ready}
            if not ready:
                if remaining > 2:
                    # No `continued` yet (older adapters may never send one):
                    # bounded probe of the held stops on any target (main or
                    # a resumed child), then keep waiting.
                    r = self._probe_all_suspects()
                    if r:
                        return r
                continue  # idle second; re-check deadline
            for tid, conn, sock in entries:
                if sock is None or id(sock) not in ready_ids:
                    continue
                # Bound split-segment stalls (old behavior): the rest of a
                # half-received message normally follows in milliseconds.
                try:
                    sock.settimeout(min(max(remaining, 0.1), 1.0))
                except OSError:
                    continue
                try:
                    msg = conn._read_msg()
                except (socket.timeout, TimeoutError):
                    continue
                except BridgeErr as e:
                    if "closed" in str(e).lower():
                        try:
                            self._conn_dead(tid, e)
                        except BridgeErr as fatal:
                            raise fatal
                        continue
                    raise
                r = self._dispatch_pumped(msg, tid)
                if r:
                    return r

    @staticmethod
    def _stopped_tid(msg):
        try:
            return (msg.get("body") or {}).get("threadId")
        except AttributeError:
            return None

    @staticmethod
    def _stopped_reason(msg):
        try:
            return (msg.get("body") or {}).get("reason", "")
        except AttributeError:
            return ""

    def _co_stop_frame(self, thread_id):
        """Top frame of another stopped thread (levels:1 probe). None when
        the thread is not inspectably stopped — never raises."""
        try:
            body = self.dap_request("stackTrace",
                                    {"threadId": thread_id,
                                     "startFrame": 0, "levels": 1},
                                    timeout=5)
        except BridgeErr:
            return None
        if not isinstance(body, dict):
            return None
        frames = body.get("stackFrames", [])
        return frames[0] if frames else None

    def _note_suspect(self, msg):
        if len(self._suspects) >= 4:
            del self._suspects[0]
        self._suspects.append(msg)

    def _probe_suspects(self):
        """Park the first held suspect whose thread still probes live.
        Returns "stopped" on a park, None when every suspect is stale.
        Bounded: at most the first two suspects are probed, then all are
        dropped — no unbounded loops, no DAP storms."""
        if not self._suspect_warned:
            sys.stderr.write("warn: no continued event yet; probing held stops\n")
            self._suspect_warned = True
        live = None
        for msg in self._suspects[:2]:
            tid = self._stopped_tid(msg)
            if tid is not None and self._co_stop_frame(tid) is not None:
                live = msg
                break
        self._suspects = []
        if live is None:
            return None
        self._awaiting_continued = False
        return self._park_stop(self._stopped_reason(live),
                               self._stopped_tid(live))

    def _park_stop(self, reason, tid, target=None):
        """Park the session on a genuine stop. The single place that moves
        the park: thread, frames, stopInfo, change tracking, hit counting.
        `target` names the parking session (main or a child id): one
        target's stop never moves another's park. publish_state stays
        main-focused (child parks skip the session.json rewrite). The park
        is also recorded thread-locally (M5) so concurrent resume pumps on
        different handler threads attribute their own stop exactly."""
        eff = target if target is not None else self._serving
        self.thread_id = tid
        self.suspended = True
        self.frames = []
        self.stop_info = None
        self._co_seen = {tid}
        # A failed stack read does not prove the thread died. Preserve
        # the actual suspension and allow an explicit continue/retry.
        self.publish_state(True)
        self.refresh_frames(timeout=5)
        if not self.frames:
            raise BridgeErr("target stopped but stack is unavailable; retry context or continue")
        if reason == "exception":
            self.stop_info = self.exception_info()
        else:
            self.stop_info = None
        self.track_changes()
        self.count_hits(reason)
        self.publish_state(True)
        self._stop_seq += 1
        if eff == "main":
            self._main_seq = self._stop_seq
        else:
            t = self.targets.get(eff)
            if t is not None:
                t.stop_seq = self._stop_seq
                t.state = "stopped"
        self._last_park_target = eff
        self._park_local.parked = eff
        # Diagnostics: monotonic id + previous-park comparison (never
        # fabricate: DAP stopped events carry no breakpoint ids).
        try:
            loc = self.location_json()
            here_file, here_line = loc.get("file", "?"), loc.get("line", -1)
        except Exception:
            here_file, here_line = "?", -1
        now_ms = int(time.time() * 1000)
        prev = self._prev_park
        elapsed = (now_ms - prev["atMs"]) if isinstance(prev, dict) else None
        same_loc = bool(isinstance(prev, dict) and prev.get("file") == here_file
                         and prev.get("line") == here_line)
        same_thr = bool(isinstance(prev, dict) and prev.get("threadId") == tid
                         and prev.get("target") == eff)
        self._stop_diag_seq += 1
        self._prev_park = {"target": eff, "file": here_file, "line": here_line,
                           "threadId": tid, "atMs": now_ms}
        self._stop_reason = reason
        self._parked_at_ms = now_ms
        self._last_diag = {"target": eff, "stopId": self._stop_diag_seq,
                           "sameLocation": same_loc, "sameThread": same_thr,
                           "elapsedMs": elapsed}
        return "stopped"

    def _handle_pumped(self, msg, target="main"):
        # Child sessions reconcile independently: late events for a gone
        # child are ignored, and terminal child events never touch main.
        if target != "main" and target not in self.targets:
            return None
        if not isinstance(msg, dict) or msg.get("type") != "event":
            return None
        ev = msg.get("event")
        body = msg.get("body", {})
        if not isinstance(body, dict):
            return None
        if ev == "debugpyAttach":
            # A second concurrent DAP session for one child (M0: same
            # adapter port, attach = this body verbatim). Launch-only and
            # opt-in; attach sessions stay single-target.
            if target == "main":
                self._accept_child(body)
            return None
        if target != "main":
            if ev in ("exited", "terminated"):
                self._note_exit(target)
                return None
            with self._TargetScope(self, target):
                return self._handle_main_event(msg, target)
        return self._handle_main_event(msg, "main")

    def _handle_main_event(self, msg, target="main"):
        if not isinstance(msg, dict) or msg.get("type") != "event":
            return None
        ev = msg.get("event")
        body = msg.get("body", {})
        if not isinstance(body, dict):
            return None
        if ev == "debugpyAttach":
            return None
        if ev == "stopped":
            reason = body.get("reason", "")
            # Wire trace: which stops the adapter reports (thread/reason).
            # One stderr line per stopped event — the only way to tell a
            # real stop from a replayed/late one when debugging the pump.
            try:
                sys.stderr.write(
                    f"dap: stopped reason={reason} "
                    f"threadId={body.get('threadId')} "
                    f"line={(body.get('stackTrace') or [{}])[0].get('line', '?') if isinstance(body.get('stackTrace'), list) else '?'}\n")
                sys.stderr.flush()
            except Exception:
                pass
            if reason in ("breakpoint", "step", "exception", "function breakpoint",
                          "data breakpoint", "entry", "goto"):
                tid = body.get("threadId")
                if self._awaiting_continued and not self.suspended:
                    # Post-resume suspect: the resume has not taken effect
                    # yet (no `continued`), so this stop predates it. Hold
                    # for the barrier; never park a maybe-stale stop.
                    self._note_suspect(msg)
                    return None
                if self.suspended:
                    # Co-stop while parked: attribute the hit via a targeted
                    # probe, but never move the park out from under the agent.
                    if tid not in self._co_seen:
                        self._co_seen.add(tid)
                        if reason == "exception":
                            self.count_hits(reason)
                        elif tid is not None:
                            frame = self._co_stop_frame(tid)
                            if frame is not None:
                                self.count_hits(reason, frame)
                    return None
                return self._park_stop(reason, tid, target)
            return None
        if ev == "continued":
            # Resume barrier: the adapter confirms the resume took effect.
            # Pre-barrier suspects predate it and are stale. A per-thread
            # resume (explicit false) clears only its own thread's suspects;
            # a missing flag means "everything resumed" (true).
            if not self._awaiting_continued:
                return None
            self._awaiting_continued = False
            if body.get("allThreadsContinued", True):
                self._suspects = []
            else:
                tid = body.get("threadId")
                self._suspects = [m for m in self._suspects
                                  if self._stopped_tid(m) != tid]
            if not self._suspects:
                return None
            self._suspect_warned = True  # normal barrier resolution: no warning
            return self._probe_suspects()
        if ev in ("exited", "terminated"):
            # Main-session terminal event. The parent's end never marks
            # independent child sessions exited: each child reconciles via
            # its own event/close. The session counts fully exited only when
            # main is done AND no live target remains.
            self.main_exited = True
            self.suspended = False
            self.thread_id = None
            self.frames = []
            self.publish_state(False)
            if self.live_targets():
                return None
            self.exited = True
            # A launch that never stopped usually means the breakpoints never
            # bound (wrong path or non-executable line): say which, instead
            # of leaving only "target exited".
            if self.cfg.kind == "launch" and self.last_stop is None:
                extra = self.unresolved_summary()
                if extra:
                    raise BridgeErr(f"target exited before any stop; {extra}")
            raise BridgeErr("target exited")
        if ev == "output" and isinstance(body, dict):
            text = body.get("output", "")
            # Startup banners (ptvsd/debugpy) arrive during the handshake;
            # only collect once the session is configured.
            if isinstance(text, str) and text and self.configured:
                self.output_tail = (self.output_tail + text)[-MAX_OUTPUT * 2:]
                kept = "\n".join(
                    ln for ln in text.splitlines() if ln.strip() not in BANNER_LINES)
                self.append_log(kept.strip() or None)
        return None

    def exception_info(self):
        try:
            body = self.dap_request("exceptionInfo", {"threadId": self.thread_id})
            name = body.get("exceptionId") or (body.get("details") or {}).get("typeName")
            if not name:
                desc = body.get("description", "?")
                name = desc.split(":")[0].strip() or "?"
            return json.dumps({"exception": {"class": str(name)}})
        except BridgeErr:
            return json.dumps({"exception": {"class": "?"}})

    def append_log(self, line):
        """Ring-kept logs: logs.jsonl holds the latest MAX_LOG_LINES physical
        lines; older lines are evicted (counted in log_dropped, surfaced by
        `logs`) instead of silently dropping NEW lines — the old cap froze
        `logs --tail` on stale output once full."""
        if line is None:
            return
        parts = line.splitlines()
        if not parts:
            return
        self._append_log_parts(parts)

    def _append_log_parts(self, parts):
        path = os.path.join(self.cfg.dir, "logs.jsonl")
        with self._gate:
            self._append_log_parts_inner(path, parts)

    def _append_log_parts_inner(self, path, parts):
        try:
            if self.log_count + len(parts) <= MAX_LOG_LINES:
                with open(path, "a") as f:
                    for part in parts:
                        f.write(part + "\n")
                self.log_count += len(parts)
                return
            # Ring trim: keep the latest MAX lines. Bounded rewrite of a
            # ≤2000-line file (published atomically, so concurrent `logs`
            # readers never see a torn file); plain appends stay append-only.
            try:
                with open(path) as f:
                    kept = f.read().splitlines()
            except OSError:
                kept = []
            kept.extend(parts)
            evicted = len(kept) - MAX_LOG_LINES
            if evicted > 0:
                kept = kept[evicted:]
                self.log_dropped += evicted
            write_file(path, "".join(p + "\n" for p in kept))
            self.log_count = len(kept)
        except OSError:
            pass

    # -- commands

    def cmd_context(self):
        self.require_stopped()
        threads = self.threads_json(self.thread_id)
        return {"ok": True, "stopInfo": json.loads(self.stop_info or "null"),
                "location": self.location_json(),
                "threads": threads,
                "frames": self.frames_json(True),
                "diag": self._stop_diag(self._serving, threads),
                "warning": PARK_WARNING}

    def cmd_stack(self):
        self.require_stopped()
        return {"ok": True, "frames": self.frames_json(False)}

    def cmd_vars(self, req):
        self.require_stopped()
        frame = int(req.get("frame", 0))
        self.frame_entry(frame)  # bounds-check (widens fetch if needed)
        return {"ok": True, "frame": frame, "locals": self.frame_locals(frame)}
    def cmd_eval(self, req):
        self.require_stopped()
        expr = req.get("expr")
        if expr is None:
            raise BridgeErr("eval needs an expr")
        frame = int(req.get("frame", 0))
        fid = self.frame_entry(frame)["id"]
        if expr.strip().startswith("refs(") and expr.strip().endswith(")"):
            return {"ok": True, "expr": expr,
                    "value": self.cmd_refs(expr.strip()[5:-1])}
        try:
            body = self.dap_request(
                "evaluate", {"expression": expr,
                             "frameId": fid,
                             "context": "watch"})
        except BridgeErr as e:
            raise BridgeErr(f"cannot evaluate {expr!r}: {e}")
        result = body.get("result", "?")
        ref = body.get("variablesReference", 0)
        if ref:
            try:
                result = self.fmt_dap_value(body.get("type", ""), result, ref)
            except BridgeErr:
                pass
        return {"ok": True, "expr": expr, "value": trunc_str(str(result))}

    def cmd_refs(self, inner):
        parts = [p.strip() for p in inner.split(",")]
        path = parts[0]
        if len(parts) > 1:
            # gc-walk reports direct holders only; deeper chains would need
            # cross-eval object pinning (id-reuse hazards). Honest boundary.
            raise BridgeErr("refs() supports depth 1 on Python (direct holders only)")
        # Python objects don't expose incoming refs via DAP, so use the
        # debuggee itself: gc.get_referrers evaluated in the stopped frame.
        body = self.dap_request(
            "evaluate",
            {"expression": f"[type(r).__name__ for r in __import__('gc').get_referrers({path})]",
             "frameId": self.frames[0]["id"], "context": "watch"})
        raw = body.get("result", "?")
        # Strip our own machinery + debugger internals from the answer.
        import ast
        try:
            names = ast.literal_eval(raw)
            if not isinstance(names, list):
                raise ValueError
        except Exception:
            return f"{path}: held by {raw} (direct referrer types)"
        noise = {"frame", "list"}
        clean = sorted({n for n in names
                        if "pydevd" not in n and "_ObjectVariable" not in n
                        and n not in noise})
        return f"{path}: held by {clean} (direct referrer types)"

    def _resume_and_wait(self, timeout, tid="main"):
        """Resume one target after step/continue and wait for the next stop
        on ANY target. The response names the target that actually parked
        (which may differ from the resumed one)."""
        self._pending_target = tid
        try:
            with self._TargetScope(self, tid):
                was_suspended = self.suspended
                if was_suspended:
                    # Pre-resume stops predate the resume: drop them from the stash
                    # (their hits were already counted on arrival) so the next pump
                    # cannot re-park a running thread. Running-origin stash is never
                    # touched — only a parked resume invalidates queued stops. The
                    # barrier below guards only real resumes: a running `continue`
                    # issues none, so its arrivals stay genuine.
                    stash = getattr(self.dap, "stash", None)
                    if isinstance(stash, list):
                        self.dap.stash = [
                            m for m in stash
                            if not (isinstance(m, dict) and m.get("type") == "event"
                                    and m.get("event") == "stopped")]
                self._suspects = []
                self._co_seen = set()
                self._awaiting_continued = was_suspended
                self._suspect_warned = False
                self.suspended = False
                self.thread_id = None
                self.frames = []
                self.publish_state(False)
            self._park_local.parked = None
            self.pump(timeout)
            # Attribute our own stop: the pump records the parking target
            # thread-locally (concurrent pumps must not share
            # _last_park_target). Legacy/unknown values fall back to the
            # last park, then to the resumed target.
            parked = getattr(self._park_local, "parked", None)
            if isinstance(parked, str) and (parked == "main"
                                            or parked in self.targets):
                stopped = parked
            else:
                stopped = self._last_park_target
                if stopped != "main" and stopped not in self.targets:
                    stopped = tid
            with self._TargetScope(self, stopped):
                snap = self.snapshot()
                resp = {"ok": True, "stopped": True,
                        "changed": json.loads(self.last_changed),
                        "stopInfo": json.loads(self.stop_info or "null"),
                        "snapshot": snap,
                        "diag": self._stop_diag(stopped, snap.get("threads")),
                        "warning": PARK_WARNING}
            return self.stamp(resp, stopped)
        finally:
            self._pending_target = None

    def cmd_step(self, req, timeout):
        tid = self.resolve_target(req)
        with self._TargetScope(self, tid):
            self.require_live()
            # Stepping needs a stopped thread to step from (uniform contract
            # on all bridges); continuing works from running (it waits).
            self.require_stopped()
            mode = req.get("mode", "over")
            cmd = {"over": "next", "into": "stepIn", "out": "stepOut"}.get(mode)
            if not cmd:
                raise BridgeErr(f"bad step mode: {mode}")
            self.dap_request(cmd, {"threadId": self.thread_id,
                                   "singleThread": True,
                                   "granularity": "statement"})
        return self._resume_and_wait(timeout, tid)

    def cmd_continue(self, req, timeout):
        tid = self.resolve_target(req)
        with self._TargetScope(self, tid):
            self.require_live()
            if self.suspended:
                self.dap_request("continue", {"threadId": self.thread_id})
        return self._resume_and_wait(timeout, tid)

    # -- event-driven wait + bounded auto-resuming capture (UX batch)

    def _stop_diag(self, tid, threads):
        """Additive stop diagnostics for one parked target (call in scope).
        Native hit ids stay null on DAP (never fabricated); requested/bound
        attribution is best-effort (None when not attributable)."""
        name = None
        try:
            for t in threads or []:
                if t.get("id") == self.thread_id:
                    name = t.get("name")
                    break
        except Exception:
            name = None
        requested, bound, hits = None, None, None
        try:
            if self.frames:
                f0 = self.frames[0]
                fsrc = (f0.get("source") or {}).get("path", "")
                here = os.path.realpath(os.path.abspath(fsrc))
                line = f0.get("line")
                for i, key in enumerate(self._hitkeys):
                    if key[0] == "break" and len(key) == 4:
                        try:
                            same = (os.path.realpath(os.path.abspath(key[1])) == here
                                    and key[3] == line)
                        except (TypeError, ValueError):
                            continue
                        if same and i < len(self.stop_states):
                            requested = self.stop_states[i].get("spec")
                            bound = key[3]
                            h = self.stop_states[i].get("hits")
                            hits = h if isinstance(h, int) else None
                            break
        except Exception:
            requested, bound, hits = None, None, None
        diag = {"stopId": None, "parkedAtMs": self._parked_at_ms,
                "target": tid, "reason": self._stop_reason,
                "stoppingThread": {"id": self.thread_id, "name": name},
                "hitBreakpoints": None,
                "requestedBreak": requested, "boundLine": bound,
                "hitCount": hits,
                "sameLocation": False, "sameThread": False,
                "elapsedSincePreviousStopMs": None}
        last = self._last_diag
        if isinstance(last, dict) and last.get("target") == tid:
            diag["stopId"] = last.get("stopId")
            diag["sameLocation"] = bool(last.get("sameLocation"))
            diag["sameThread"] = bool(last.get("sameThread"))
            diag["elapsedSincePreviousStopMs"] = last.get("elapsedMs")
        return diag

    def _wait_snapshot(self, tid, waited):
        """Parked response shared by wait (never resumes by construction:
        this helper issues zero DAP resume traffic)."""
        snap = self.snapshot()
        resp = {"ok": True, "stopped": True, "waited": waited,
                "changed": json.loads(self.last_changed),
                "stopInfo": json.loads(self.stop_info or "null"),
                "snapshot": snap,
                "diag": self._stop_diag(tid, snap.get("threads")),
                "warning": PARK_WARNING}
        return self.stamp(resp, tid)

    def _parked_now(self):
        """True when the in-scope target is parked."""
        return bool(self.suspended)

    def cmd_wait(self, req, timeout):
        """Pure long-poll: NEVER resumes. Immediate success when the
        selected target is already parked; otherwise pump for the next
        fresh stop (any target when omitted — the response stamps the
        actual one). Timeout preserves session/intents (typed message)."""
        tid = self.resolve_target(req)
        with self._TargetScope(self, tid):
            self.require_live()
            if self._parked_now():
                return self._wait_snapshot(tid, False)
        # Not parked: wait without issuing any resume.
        self._park_local.parked = None
        self.pump(timeout)
        parked = getattr(self._park_local, "parked", None)
        if not (isinstance(parked, str)
                and (parked == "main" or parked in self.targets)):
            parked = self._last_park_target
            if parked != "main" and parked not in self.targets:
                parked = tid
        with self._TargetScope(self, parked):
            return self._wait_snapshot(parked, True)

    def _capture_bounds(self, req):
        """Validate capture bounds (bridge-side enforcement; CLI mirrors)."""
        try:
            frames_n = int(req.get("frames", 1))
            vars_n = int(req.get("vars", 20))
            budget = int(req.get("pauseBudgetMs", 2000))
        except (TypeError, ValueError):
            raise BridgeErr("capture needs integer frames/vars/pauseBudgetMs")
        if not 1 <= frames_n <= 10:
            raise BridgeErr("capture frames must be between 1 and 10")
        if not 1 <= vars_n <= 20:
            raise BridgeErr("capture vars must be between 1 and 20")
        if not 1 <= budget <= 10000:
            raise BridgeErr("capture pause budget must be between 1 and 10000 ms")
        spec = req.get("break")
        if spec is not None and (not isinstance(spec, str) or not spec):
            raise BridgeErr("capture --break must look like path:line")
        return frames_n, vars_n, budget, spec

    def _capture_plant(self, tid, spec):
        """Plant one ephemeral line-only break for a capture. Returns a
        removal token (kind, ...) with planted=False when the exact line is
        already armed (idempotent — nothing to remove). Raises BridgeErr on
        invalid/conflicting specs BEFORE anything parks (no resume owed).
        Never touches stops.json / the global intent / inheritance."""
        parsed = self._parse_live_breaks([spec])
        _raw, path, line, cond = parsed[0]
        if tid == "main":
            for p, ln, c in self.cfg.breaks:
                if p == path and ln == line:
                    if (c or None) == (cond or None):
                        return ("main-dup",)
                    raise BridgeErr(
                        f"conflicting condition for {self.rel_file(path)}:{line} "
                        f"(already armed{' as ' + repr(c) if c else ' plain'}): {spec}")
            exist_breaks = [(ln, c) for p, ln, c in self.cfg.breaks if p == path]
            exist_logs = [(ln, t) for p, ln, t in self.cfg.logpoints if p == path]
            params = []
            for ln, c in exist_breaks:
                bp = {"line": ln}
                if c:
                    bp["condition"] = c
                params.append(bp)
            for ln, t in exist_logs:
                params.append({"line": ln, "logMessage": t})
            bp = {"line": line}
            if cond:
                bp["condition"] = cond
            params.append(bp)
            with self._TargetScope(self, "main"):
                try:
                    self.dap_request("setBreakpoints",
                                     {"source": {"path": path},
                                      "breakpoints": params},
                                     timeout=5)
                except BridgeErr as e:
                    raise BridgeErr(f"capture break failed to plant: {e}")
            return ("main", path, line, cond)
        t = self.targets.get(tid)
        if t is None or t.exited:
            raise BridgeErr(f"target {tid} has exited — close this session")
        # Idempotent when the exact line is already planted there.
        combined = set(t.inherited_keys) | set(t.target_raws.keys())
        if (path, line, cond) in combined:
            return ("child-dup",)
        for p, ln, _c in combined:
            if p == path and ln == line:
                raise BridgeErr(
                    f"conflicting condition for {self.rel_file(path)}:{line} "
                    f"(already armed): {spec}")
        self._add_ephemeral(tid, [spec])
        key = self._match_child_break(t, spec)
        return ("child", tid, key, spec)

    def _capture_unplant(self, token):
        """Remove a capture ephemeral BEFORE resume. Raises on failure
        (caller still resumes, then reports removeError)."""
        if token is None or token[0] in ("main-dup", "child-dup"):
            return
        if token[0] == "main":
            _, path, _line, _cond = token
            exist_breaks = [(ln, c) for p, ln, c in self.cfg.breaks if p == path]
            exist_logs = [(ln, t) for p, ln, t in self.cfg.logpoints if p == path]
            params = []
            for ln, c in exist_breaks:
                bp = {"line": ln}
                if c:
                    bp["condition"] = c
                params.append(bp)
            for ln, t in exist_logs:
                params.append({"line": ln, "logMessage": t})
            with self._TargetScope(self, "main"):
                self.dap_request("setBreakpoints",
                                 {"source": {"path": path},
                                  "breakpoints": params},
                                 timeout=5)
            return
        if token[0] == "child":
            _, tid, key, _spec = token
            if key is None:
                return
            self._drop_child_keys(tid, [key], [])
            return
        raise BridgeErr(f"bad capture token: {token[0]}")

    def _begin_resume_locked(self, was_suspended=True):
        """Pre-resume invalidation inside the target scope (mirrors the
        running half of _resume_and_wait): pre-resume queued stops predate
        the resume and must never re-park a later pump; the barrier guards
        the real resume below."""
        stash = getattr(self.dap, "stash", None)
        if isinstance(stash, list):
            self.dap.stash = [
                m for m in stash
                if not (isinstance(m, dict) and m.get("type") == "event"
                        and m.get("event") == "stopped")]
        self._suspects = []
        self._co_seen = set()
        self._awaiting_continued = was_suspended
        self._suspect_warned = False
        self.suspended = False
        self.thread_id = None
        self.frames = []
        self.publish_state(False)

    def _bounded_snapshot(self, frames_n, vars_n):
        """Capture snapshot: frames 1..10, frame-0 vars 1..20 (depth and
        string caps reuse the existing 1-2/200 rules)."""
        frames = []
        for i, f in enumerate(self.frames[:frames_n]):
            entry = {"index": i, "type": "?",
                     "method": f.get("name", "?"), "line": f.get("line", -1)}
            if i == 0:
                try:
                    entry["locals"] = self.frame_locals(0, vars_n)
                except BridgeErr:
                    entry["locals"] = []
            frames.append(entry)
        return {"mode": "session", "location": self.location_json(),
                "threads": self.threads_json(self.thread_id),
                "frames": frames,
                "output": self.output_tail[-MAX_OUTPUT:]}

    def cmd_capture(self, req, timeout):
        """One-shot bounded stop. Pre-parked target: collect WITHOUT
        resuming. Fresh park: collect, REMOVE EPHEMERAL BEFORE RESUME,
        auto-resume within the pause budget (overrun still resumes, then
        reports). Any collection/removal failure still resumes; timeout
        never resumes (nothing parked). No eval, no persisted vars."""
        frames_n, vars_n, budget, spec = self._capture_bounds(req)
        tid = self.resolve_target(req)
        with self._TargetScope(self, tid):
            self.require_live()
            prepark = self._parked_now()
        if prepark:
            with self._TargetScope(self, tid):
                snap = self._bounded_snapshot(frames_n, vars_n)
                resp = {"ok": True, "stopped": True,
                        "targetWasPaused": True, "resumed": False,
                        "pauseDurationMs": 0, "pauseBudgetMs": budget,
                        "ephemeralPlanted": False,
                        "truncated": {"frames": len(self.frames) > frames_n,
                                      "vars": False},
                        "snapshot": snap,
                        "diag": self._stop_diag(tid, snap.get("threads")),
                        "warning": PARK_WARNING}
                return self.stamp(resp, tid)
        # Fresh path: plant the ephemeral first (failure here parks
        # nothing, so no resume is owed).
        token = None
        if spec is not None:
            token = self._capture_plant(tid, spec)
        try:
            self._park_local.parked = None
            self.pump(timeout)
        except Exception:
            # Timeout/exit: nothing parked by us — no resume — but the
            # ephemeral must not leak: remove best-effort, then re-raise.
            try:
                self._capture_unplant(token)
            except Exception as e:
                raise BridgeErr(f"{e}; capture ephemeral may still be planted "
                                f"(breaks remove --target {tid} to clear)")
            raise
        parked = getattr(self._park_local, "parked", None)
        if not (isinstance(parked, str)
                and (parked == "main" or parked in self.targets)):
            parked = self._last_park_target
            if parked != "main" and parked not in self.targets:
                parked = tid
        with self._TargetScope(self, parked):
            park_ms = int(time.time() * 1000)
            snap_err, remove_err, resume_err = None, None, None
            try:
                snap = self._bounded_snapshot(frames_n, vars_n)
            except Exception as e:
                snap_err = str(e)
                snap = {"mode": "session", "location": self.location_json(),
                        "threads": [], "frames": [],
                        "output": self.output_tail[-MAX_OUTPUT:]}
            try:
                self._capture_unplant(token)
            except Exception as e:
                remove_err = str(e)
            saved_tid = self.thread_id
            resumed = False
            try:
                # Resume while still marked suspended (honest on failure:
                # the park stands and resumed:false is reported).
                if self.suspended:
                    self.dap_request("continue", {"threadId": saved_tid})
                self._begin_resume_locked(True)
                self._park_local.parked = None
                resumed = True
            except Exception as e:
                resume_err = str(e)
            pause_ms = int(time.time() * 1000) - park_ms
            try:
                diag = self._stop_diag(parked, snap.get("threads"))
            except Exception:
                diag = {"target": parked}
            diag["pauseDurationMs"] = pause_ms
            diag["targetWasPaused"] = False
            diag["resumed"] = resumed
            resp = {"ok": True, "stopped": True,
                    "targetWasPaused": False, "resumed": resumed,
                    "pauseDurationMs": pause_ms, "pauseBudgetMs": budget,
                    "budgetExceeded": pause_ms > budget,
                    "ephemeralPlanted": token is not None and token[0] not in (
                        "main-dup", "child-dup"),
                    "truncated": {"frames": len(self.frames) > frames_n,
                                  "vars": False},
                    "snapshot": snap, "diag": diag,
                    "warning": PARK_WARNING}
            if snap_err is not None:
                resp["snapshotError"] = snap_err
            if remove_err is not None:
                resp["removeError"] = remove_err
            if resume_err is not None:
                resp["resumeError"] = resume_err
            return self.stamp(resp, parked)

    def drain_pending(self, budget=1.0):
        """Consume already-arrived messages (output events etc.) without
        waiting for a stop. Lets `logs` flush recently collected lines.
        Polls every live session; a newly parked stop ends the drain."""
        deadline = time.time() + budget
        while time.time() < deadline:
            drained = False
            for tid, conn, _sock in self._conn_entries():
                while True:
                    msg = self._pop_stash(conn)
                    if msg is None:
                        break
                    try:
                        if self._dispatch_pumped(msg, tid):
                            return
                    except BridgeErr:
                        return
                    drained = True
                # Buffered/coalesced data before selecting (same invariant
                # as pump: never wait on select while bytes sit in the
                # connection buffer).
                while True:
                    try:
                        msg = self._try_read(conn)
                    except BridgeErr:
                        break
                    if msg is None:
                        break
                    drained = True
                    try:
                        if self._dispatch_pumped(msg, tid):
                            return
                    except BridgeErr:
                        return
            if drained:
                continue
            socks = [s for _, _, s in self._conn_entries() if s is not None]
            if not socks:
                return
            try:
                ready, _, _ = select.select(
                    socks, [], [], max(0.01, deadline - time.time()))
            except (OSError, ValueError):
                return
            if not ready:
                return
            ready_ids = {id(s) for s in ready}
            for tid, conn, sock in self._conn_entries():
                if sock is None or id(sock) not in ready_ids:
                    continue
                try:
                    sock.settimeout(max(0.01, deadline - time.time()))
                except OSError:
                    return
                try:
                    msg = conn._read_msg()
                except (socket.timeout, TimeoutError, BridgeErr):
                    return
                try:
                    if self._dispatch_pumped(msg, tid):
                        return
                except BridgeErr:
                    return

    def cmd_threads(self, req=None):
        # Bare threads stays main-focused (unchanged single-target shape);
        # --target X dumps that target's threads instead.
        req = req or {}
        tid = self.resolve_target(req)
        with self._gate:
            busy = tid in self._outstanding
        if busy:
            # M5 live read: never waits behind the outstanding resume and
            # never opens a second DAP reader — serve the published running
            # truth with zero DAP traffic (no stale frames as current).
            return self.stamp({"ok": True, "running": True,
                               "threads": []}, tid)
        with self._TargetScope(self, tid):
            if self.exited:
                raise BridgeErr("target VM has exited — close this session")
            resp = {"ok": True, "running": not self.suspended,
                    "threads": self.threads_dump()}
        return self.stamp(resp, tid)

    def cmd_breaks(self, req=None):
        # Bare breaks aggregates every target: main records plus each live
        # child's records (copies tagged with their target id). Live
        # snapshots only — exited history lives in `targets`.
        req = req or {}
        with self._gate:
            selected = self._resolve_target_inner(req)
            if self.exited:
                raise BridgeErr("target VM has exited — close this session")
            stops = [dict(r, target="main") for r in self.stop_states]
            for tid in self.target_order:
                t = self.targets.get(tid)
                if t is None or t.exited or t.state == "ignored":
                    continue
                for r in t.stop_states:
                    stops.append(dict(r, target=tid))
            return self.stamp({"ok": True, "stops": stops}, selected)

    def cmd_breaks_add(self, req):
        """Additive line breaks on a live session (running or parked — never
        suspended/resumed here). The whole batch validates first (parse,
        canonical dedup/conflict) with no DAP traffic; then each touched file
        is re-sent merged with existing breaks (setBreakpoints replaces per
        file). Per-file DAP successes become the confirmed subset (partial
        ok + warning); total failure is ok:false with nothing mutated.
        `--target X` (child only) plants an ephemeral target-scoped break:
        same validation, no global intent change, no stops.json persistence,
        no inheritance. `--target main` is the global path."""
        scope = req.get("target") if isinstance(req, dict) else None
        if scope is not None and scope != "main":
            tid = self.resolve_target(req)
            return self._add_ephemeral(tid, req.get("breaks"))
        if self.exited:
            raise BridgeErr("target VM has exited — close this session")
        raws = req.get("breaks")
        if not isinstance(raws, list) or not raws:
            raise BridgeErr("breaks add needs at least one --break")
        for r in raws:
            if not isinstance(r, str) or not r:
                raise BridgeErr(f"bad break spec: {r!r}")
        scratch = Config()
        # Live adds resolve exactly like startup specs: against the live
        # session's --src roots (flags may predate --src at startup, but by
        # now the roots are final).
        scratch.src_dirs = list(self.cfg.src_dirs)
        try:
            for r in raws:
                head = r.split("|", 1)[0]
                if head == "exc" or head.startswith("exc:") or head.startswith("method:"):
                    raise Usage(f"breaks add takes line breaks only (got {r!r})")
                parse_break(r, scratch)
            if scratch.methods or scratch.want_exc:
                raise Usage("breaks add takes line breaks only")
        except Usage as e:
            raise BridgeErr(str(e))
        have = {(p, ln, c) for p, ln, c in self.cfg.breaks}
        seen = set()
        batch_locs = {}  # (path, line) -> cond of fresh items this batch
        fresh = []  # (raw, path, line, cond)
        for r, (path, line, cond) in zip(raws, scratch.breaks):
            key = (path, line, cond)
            if key in seen or key in have:
                continue  # idempotent (intra-batch dup or already armed)
            seen.add(key)
            other, hit = None, False
            for p, ln, c in self.cfg.breaks:
                if p == path and ln == line:
                    other, hit = c, True
                    break
            if not hit and (path, line) in batch_locs:
                other, hit = batch_locs[(path, line)], True
            if hit:
                raise BridgeErr(
                    f"conflicting condition for {self.rel_file(path)}:{line} "
                    f"(already armed{' as ' + repr(other) if other else ' plain'}): {r}")
            batch_locs[(path, line)] = cond
            fresh.append((r, path, line, cond))
        if not fresh:
            return self.stamp({"ok": True, "added": [], "stops": self.stop_states}, "main")
        by_file = {}
        for item in fresh:
            by_file.setdefault(item[1], []).append(item)
        added = []
        failed = []
        for path, items in by_file.items():
            # Existing entries ride along (breaks in cfg order, then
            # logpoints): setBreakpoints REPLACES the file, so omitting them
            # would drop live stops.
            exist_breaks = [(ln, c) for p, ln, c in self.cfg.breaks if p == path]
            exist_logs = [(ln, t) for p, ln, t in self.cfg.logpoints if p == path]
            params = []
            for ln, c in exist_breaks:
                bp = {"line": ln}
                if c:
                    bp["condition"] = c
                params.append(bp)
            for ln, t in exist_logs:
                params.append({"line": ln, "logMessage": t})
            for _, _, ln, c in items:
                bp = {"line": ln}
                if c:
                    bp["condition"] = c
                params.append(bp)
            try:
                body = self.dap_request("setBreakpoints",
                                        {"source": {"path": path},
                                         "breakpoints": params},
                                        timeout=5)
            except BridgeErr:
                failed.append(path)
                continue
            got_list = body.get("breakpoints", [])
            # Positional refresh (request order above): existing breaks, then
            # logpoints, then new. Short answers leave old records untouched.
            bi = [i for i, k in enumerate(self._hitkeys)
                  if k[0] == "break" and len(k) >= 3 and k[1] == path]
            li = [i for i, k in enumerate(self._hitkeys)
                  if k[0] == "logpoint" and len(k) >= 3 and k[1] == path]
            for pos, i in enumerate(bi):
                if pos < len(got_list):
                    key = self._hitkeys[i]
                    requested = key[2] if len(key) >= 3 else None
                    prev = self.stop_states[i].get("state")
                    bound = self._refresh_rec(
                        self.stop_states[i], got_list[pos], False, requested)
                    self._hitkeys[i] = ("break", key[1], requested, bound)
                    if (self.stop_states[i].get("state") == "slid"
                            and prev != "slid"):
                        sys.stderr.write(
                            f"warn: breakpoint slid: {key[1]}:{requested} "
                            f"-> {bound}\n")
            for pos, i in enumerate(li):
                idx = len(exist_breaks) + pos
                if idx < len(got_list):
                    # Reset to the template first: repeated refreshes must
                    # not stack "(msg)" suffixes on the detail.
                    key = self._hitkeys[i]
                    requested = key[2] if len(key) >= 3 else None
                    tmpl = next((t for p, ln, t in self.cfg.logpoints
                                 if p == path and ln == requested), None)
                    if tmpl is not None:
                        self.stop_states[i]["detail"] = tmpl
                    prev = self.stop_states[i].get("state")
                    bound = self._refresh_rec(
                        self.stop_states[i], got_list[idx], True, requested)
                    self._hitkeys[i] = ("logpoint", key[1], requested, bound)
                    if (self.stop_states[i].get("state") == "slid"
                            and prev != "slid"):
                        sys.stderr.write(
                            f"warn: breakpoint slid: {key[1]}:{requested} "
                            f"-> {bound}\n")
            base = len(exist_breaks) + len(exist_logs)
            for k, (r, _, ln, c) in enumerate(items):
                got = got_list[base + k] if base + k < len(got_list) else {}
                spec = f"{self.rel_file(path)}:{ln}"
                if c:
                    spec += f"|{c}"
                rec = {"spec": spec, "kind": "break", "hits": 0}
                bound = self._refresh_rec(rec, got, False, ln)
                if rec["state"] == "slid":
                    sys.stderr.write(
                        f"warn: breakpoint slid: {path}:{ln} -> {bound}\n")
                self.stop_states.append(rec)
                self._hitkeys.append(("break", path, ln, bound))
                self.cfg.breaks.append((path, ln, c))
                self.cfg.break_raws.setdefault((path, ln, c), r)
                entry = {"raw": r, "spec": spec, "kind": "break",
                         "state": rec["state"], "hits": 0}
                if "detail" in rec:
                    entry["detail"] = rec["detail"]
                added.append(entry)
        if not added:
            raise BridgeErr(
                f"breaks add failed for {len(failed)} file(s): "
                + ", ".join(self.rel_file(p) for p in failed))
        resp = {"ok": True, "added": added, "stops": self.stop_states}
        if failed:
            resp["warning"] = ("partial add: no change for "
                               + ", ".join(self.rel_file(p) for p in failed))
        # New global intent inherits into every live child as plant copies.
        inherit_warn = self._inherit_global_add(
            [(path, ln, c) for (_r, path, ln, c) in fresh if (path, ln, c) in self.cfg.break_raws])
        if inherit_warn:
            resp["warning"] = ((resp.get("warning", "") + "; ") if resp.get("warning") else "") + inherit_warn
        return self.stamp(resp, "main")

    def _child_combined_keys(self, child):
        """Every line-break key planted on a child: inherited global copies
        plus ephemeral target-scoped records."""
        return list(set(child.inherited_keys) | set(child.target_raws.keys()))

    def _child_file_params(self, child, path, drop):
        """Merged setBreakpoints params for one child file: surviving breaks
        (inherited + ephemeral) then the inherited logpoint companions
        (setBreakpoints replaces per file, so companions ride along)."""
        params = []
        for (p, ln, c) in self._child_combined_keys(child):
            if p != path or (p, ln, c) in drop:
                continue
            bp = {"line": ln}
            if c:
                bp["condition"] = c
            params.append(bp)
        for (p, ln, tmpl) in child.logpoints:
            if p == path:
                params.append({"line": ln, "logMessage": tmpl})
        return params

    def _inherit_global_add(self, keys):
        """Plant newly added global breaks as inherited copies on every live
        child (best-effort per target: a child plant failure warns, while the
        confirmed main mutation stands). Returns a warning string or None."""
        if not keys:
            return None
        by_file = {}
        for key in keys:
            by_file.setdefault(key[0], []).append(key)
        warnings = []
        for tid in self.target_order:
            t = self.targets.get(tid)
            if t is None or t.exited or t.state == "ignored" or t.dap is None:
                continue
            with self._TargetScope(self, tid):
                for path, items in by_file.items():
                    params = self._child_file_params(t, path, set())
                    for (_p, ln, c) in items:
                        bp = {"line": ln}
                        if c:
                            bp["condition"] = c
                        params.append(bp)
                    try:
                        body = self.dap_request(
                            "setBreakpoints",
                            {"source": {"path": path}, "breakpoints": params},
                            timeout=5)
                    except BridgeErr as e:
                        warnings.append(f"{tid} {self.rel_file(path)}: {e}")
                        continue
                    got_list = body.get("breakpoints", [])
                    base = len(params) - len(items)
                    for k, key in enumerate(items):
                        _p, ln, c = key
                        got = got_list[base + k] if base + k < len(got_list) else {}
                        spec = f"{self.rel_file(path)}:{ln}"
                        if c:
                            spec += f"|{c}"
                        rec = {"spec": spec, "kind": "break", "hits": 0}
                        bound = self._refresh_rec(rec, got, False, ln)
                        self.stop_states.append(rec)
                        self._hitkeys.append(("break", path, ln, bound))
                        t.inherited_keys.add(key)
        return "; ".join(warnings) or None

    def _parse_live_breaks(self, raws):
        """Validate a live line-break batch (add-shaped): returns the parsed
        [(raw, path, line, cond)] in batch order. Raises BridgeErr."""
        if not isinstance(raws, list) or not raws:
            raise BridgeErr("breaks add needs at least one --break")
        for r in raws:
            if not isinstance(r, str) or not r:
                raise BridgeErr(f"bad break spec: {r!r}")
        scratch = Config()
        scratch.src_dirs = list(self.cfg.src_dirs)
        try:
            for r in raws:
                head = r.split("|", 1)[0]
                if head == "exc" or head.startswith("exc:") or head.startswith("method:"):
                    raise Usage(f"breaks add takes line breaks only (got {r!r})")
                parse_break(r, scratch)
            if scratch.methods or scratch.want_exc:
                raise Usage("breaks add takes line breaks only")
        except Usage as e:
            raise BridgeErr(str(e))
        return list(zip(raws, [b[0] for b in scratch.breaks],
                        [b[1] for b in scratch.breaks],
                        [b[2] for b in scratch.breaks]))

    def _add_ephemeral(self, tid, raws):
        """Ephemeral target-scoped add on one child: same validation as the
        global path, but matches only that child's planted lines and never
        touches the global intent (no inheritance, no stops.json)."""
        t = self.targets.get(tid)
        if t is None or t.exited:
            raise BridgeErr(f"target {tid} has exited — close this session")
        if t.state == "ignored" or t.dap is None:
            raise BridgeErr(f"target {tid} was released (over budget)")
        parsed = self._parse_live_breaks(raws)
        combined = set(t.inherited_keys) | set(t.target_raws.keys())
        seen = set()
        batch_locs = {}
        fresh = []
        for r, path, line, cond in parsed:
            key = (path, line, cond)
            if key in seen or key in combined:
                continue  # idempotent (intra-batch dup or already planted)
            seen.add(key)
            other, hit = None, False
            for (p, ln, c) in combined:
                if p == path and ln == line:
                    other, hit = c, True
                    break
            if not hit and (path, line) in batch_locs:
                other, hit = batch_locs[(path, line)], True
            if hit:
                raise BridgeErr(
                    f"conflicting condition for {self.rel_file(path)}:{line} "
                    f"(already armed{' as ' + repr(other) if other else ' plain'}): {r}")
            batch_locs[(path, line)] = cond
            fresh.append((r, path, line, cond))
        if not fresh:
            return self.stamp({"ok": True, "added": [], "stops": t.stop_states}, tid)
        by_file = {}
        for item in fresh:
            by_file.setdefault(item[1], []).append(item)
        added, failed = [], []
        with self._TargetScope(self, tid):
            for path, items in by_file.items():
                params = self._child_file_params(t, path, set())
                for (_, _, ln, c) in items:
                    bp = {"line": ln}
                    if c:
                        bp["condition"] = c
                    params.append(bp)
                try:
                    body = self.dap_request(
                        "setBreakpoints",
                        {"source": {"path": path}, "breakpoints": params},
                        timeout=5)
                except BridgeErr:
                    failed.extend(items)
                    continue
                got_list = body.get("breakpoints", [])
                base = len(params) - len(items)
                for k, (r, _, ln, c) in enumerate(items):
                    got = got_list[base + k] if base + k < len(got_list) else {}
                    spec = f"{self.rel_file(path)}:{ln}"
                    if c:
                        spec += f"|{c}"
                    rec = {"spec": spec, "kind": "break", "hits": 0}
                    bound = self._refresh_rec(rec, got, False, ln)
                    self.stop_states.append(rec)
                    self._hitkeys.append(("break", path, ln, bound))
                    t.target_raws[(path, ln, c)] = r
                    entry = {"raw": r, "spec": spec, "kind": "break",
                             "state": rec["state"], "hits": 0}
                    if "detail" in rec:
                        entry["detail"] = rec["detail"]
                    added.append(entry)
        if not added:
            raise BridgeErr(
                "breaks add failed for "
                + ", ".join(f"{self.rel_file(i[1])}:{i[2]}" for i in failed))
        resp = {"ok": True, "added": added, "stops": t.stop_states}
        if failed:
            resp["warning"] = ("partial add: no change for "
                               + ", ".join(f"{self.rel_file(i[1])}:{i[2]}" for i in failed))
        return self.stamp(resp, tid)

    def _refresh_rec(self, rec, got, is_logpoint, requested):
        """Apply one setBreakpoints answer to a record (shared by arm/add).

        Returns the bound line so callers can store it in _hitkeys."""
        bound = self._apply_verification(rec, got, requested, is_logpoint)
        return bound

    def _lexical_break(self, spec):
        """Parse a remove spec like add, minus existence/range checks: the
        source may be deleted or changed and removal still works. Returns
        (candidates, line, cond) where candidates are the lexical canonical
        spellings to try against stored identity (absolute/cwd/--src forms),
        or raises Usage for malformed specs (which then fall back to
        stored-raw match)."""
        raw = spec
        cond = None
        if "|" in spec:
            spec, cond = spec.split("|", 1)
            cond = cond.strip()
            if not cond:
                raise Usage("empty condition after '|'")
        if spec == "exc" or spec.startswith("exc:") or spec.startswith("method:"):
            raise Usage(f"breaks remove takes line breaks only (got {raw!r})")
        if ":" not in spec:
            raise Usage(f"--break must look like path:line (got {raw!r})")
        path, line = spec.rsplit(":", 1)
        try:
            lineno = int(line)
        except ValueError:
            raise Usage(f"bad line in --break: {raw}")
        if lineno < 1:
            raise Usage(f"bad line in --break (must be >= 1): {raw}")
        return (self._lexical_candidates(path), lineno, cond)

    def _realpath_or_abs(self, p):
        """Best-effort canonical spelling (never raises for strings)."""
        try:
            return os.path.realpath(p)
        except (TypeError, ValueError, OSError):
            try:
                return os.path.abspath(p)
            except (TypeError, ValueError, OSError):
                return p

    def _lexical_candidates(self, raw):
        """Every deterministic spelling of a remove path, mirroring add
        precedence without existence checks (a removed file may not exist,
        so cwd-exists short-circuiting would lie): the raw absolute form,
        the cwd-joined form, and each explicit --src join."""
        out = []
        if os.path.isabs(raw):
            out.append(self._realpath_or_abs(raw))
        else:
            out.append(self._realpath_or_abs(os.path.join(os.getcwd(), raw)))
            for src in (self.cfg.src_dirs or []):
                try:
                    base = self._realpath_or_abs(os.path.abspath(src))
                except (TypeError, ValueError, OSError):
                    continue
                out.append(os.path.normpath(os.path.join(base, raw)))
        # De-duplicated, order stable (absolute/cwd first, then --src).
        seen = set()
        return [c for c in out if not (c in seen or seen.add(c))]

    def _match_stored_break(self, raw):
        """Match a remove spec to one stored canonical key. Each candidate
        spelling is tried against stored recs with the same line/cond;
        then stored-raw fallback (covers spellings the lexer cannot
        reproduce). Returns the key or None (missing, never an error)."""
        try:
            candidates, lineno, cond = self._lexical_break(raw)
        except Usage:
            candidates, lineno, cond = None, None, None
        if candidates is not None:
            for key in self.cfg.break_raws:
                if (len(key) == 3 and key[1] == lineno and key[2] == cond
                        and key[0] in candidates):
                    return key
        for stored_key, stored_raw in self.cfg.break_raws.items():
            if stored_raw == raw:
                return stored_key
        return None

    def _match_child_break(self, child, raw):
        """Match a remove spec against one child's ephemeral target-scoped
        records only (never the global intent). Same lexical + stored-raw
        fallback as the global matcher."""
        try:
            candidates, lineno, cond = self._lexical_break(raw)
        except Usage:
            candidates, lineno, cond = None, None, None
        if candidates is not None:
            for key in child.target_raws:
                if (len(key) == 3 and key[1] == lineno and key[2] == cond
                        and key[0] in candidates):
                    return key
        for stored_key, stored_raw in child.target_raws.items():
            if stored_raw == raw:
                return stored_key
        return None

    def _drop_child_keys(self, tid, keys, missing):
        """Phase 2 of scoped remove/clear on one child: per-file DAP replace
        without `keys`, touching only that child's records (inherited copies
        and/or ephemeral entries, never the global intent)."""
        t = self.targets.get(tid)
        if t is None or t.exited:
            raise BridgeErr(f"target {tid} has exited — close this session")
        by_file = {}
        for key in keys:
            by_file.setdefault(key[0], []).append(key)
        removed, failed = [], []
        with self._TargetScope(self, tid):
            for path, items in by_file.items():
                drop = set(items)
                params = self._child_file_params(t, path, drop)
                try:
                    self.dap_request("setBreakpoints",
                                     {"source": {"path": path},
                                      "breakpoints": params},
                                     timeout=5)
                except BridgeErr:
                    for key in items:
                        raw = t.target_raws.get(key) or self.cfg.break_raws.get(key, "")
                        failed.append({"raw": raw, "spec": key,
                                       "error": "backend call failed"})
                    continue
                for key in items:
                    stored_raw = t.target_raws.pop(key, "") or self.cfg.break_raws.get(key, "")
                    t.inherited_keys.discard(key)
                    for i, hk in enumerate(list(self._hitkeys)):
                        if (hk[0] == "break" and len(hk) >= 3
                                and hk[1] == key[0] and hk[2] == key[1]):
                            del self.stop_states[i]
                            del self._hitkeys[i]
                            break
                    removed.append({"raw": stored_raw, "spec": self._remove_spec(key),
                                    "kind": "break", "hits": 0})
        if not removed:
            raise BridgeErr(
                "breaks remove failed for "
                + ", ".join(self.rel_file(k[0]) + f":{k[1]}" for k in keys))
        resp = {"ok": True, "removed": removed, "stops": t.stop_states}
        if missing:
            resp["missing"] = missing
        if failed:
            resp["failed"] = failed
            resp["warning"] = ("partial remove: no change for "
                               + ", ".join(f["raw"] for f in failed if f["raw"]))
        return self.stamp(resp, tid)

    def cmd_breaks_remove(self, req):
        """Remove live line breaks by stored identity (running or parked).
        Phase 1 validates/matches the whole batch with zero backend
        mutation (unmatched specs land in `missing`, never ok:false);
        phase 2 re-sends each touched file merged without the removed
        entries (setBreakpoints replaces per file, so the rest ride along).
        `removed[]` echoes the persisted stored raws so the CLI drops
        exactly the confirmed entries. Total backend failure is ok:false
        with nothing mutated.
        `--target X` matches only X's ephemeral target-scoped records (no
        global intent change). Bare remove drops the global intent plus its
        inherited plant copies on every live child."""
        scope = req.get("target") if isinstance(req, dict) else None
        if scope is not None and scope != "main":
            tid = self.resolve_target(req)
            t = self.targets.get(tid)
            raws = req.get("breaks")
            if not isinstance(raws, list) or not raws:
                raise BridgeErr("breaks remove needs at least one --break")
            for r in raws:
                if not isinstance(r, str) or not r:
                    raise BridgeErr(f"bad break spec: {r!r}")
            seen, matched, missing = set(), [], []
            for r in raws:
                if r in seen:
                    continue
                seen.add(r)
                key = self._match_child_break(t, r)
                if key is None:
                    missing.append(r)
                elif key not in matched:
                    matched.append(key)
            if not matched:
                return self.stamp({"ok": True, "removed": [], "missing": missing,
                                   "stops": t.stop_states}, tid)
            return self._drop_child_keys(tid, matched, missing)
        if self.exited:
            raise BridgeErr("target VM has exited — close this session")
        raws = req.get("breaks")
        if not isinstance(raws, list) or not raws:
            raise BridgeErr("breaks remove needs at least one --break")
        for r in raws:
            if not isinstance(r, str) or not r:
                raise BridgeErr(f"bad break spec: {r!r}")
        seen = set()
        matched = []   # canonical keys, request order, deduped
        missing = []
        for r in raws:
            if r in seen:
                continue
            seen.add(r)
            key = self._match_stored_break(r)
            if key is None:
                missing.append(r)
            elif key not in matched:
                matched.append(key)
        if not matched:
            return self.stamp({"ok": True, "removed": [], "missing": missing,
                               "stops": self.stop_states}, "main")
        resp = self._drop_break_keys(matched, missing)
        # The global intent is gone: drop its inherited copies on every live
        # child (per-target backend rules; a child failure warns while the
        # confirmed main removal stands).
        child_warnings = []
        for tid in self.target_order:
            t = self.targets.get(tid)
            if t is None or t.exited or t.state == "ignored" or t.dap is None:
                continue
            doomed = [k for k in matched if k in t.inherited_keys]
            if not doomed:
                continue
            try:
                self._drop_child_keys(tid, doomed, [])
            except BridgeErr as e:
                child_warnings.append(f"{tid}: {e}")
        if child_warnings:
            resp["warning"] = ((resp.get("warning", "") + "; ") if resp.get("warning") else "") + "; ".join(child_warnings)
        return self.stamp(resp, "main")

    def cmd_breaks_clear(self, req=None):
        """Drop all live line breaks (logpoints ride along untouched).
        Same per-file replace granularity as remove. `--target X` drops only
        X's ephemeral target-scoped records; bare clear is the full reset:
        global intent + every inherited copy + every ephemeral record."""
        req = req or {}
        scope = req.get("target")
        if scope is not None and scope != "main":
            tid = self.resolve_target(req)
            t = self.targets.get(tid)
            ordered = list(t.target_raws.keys())
            if not ordered:
                return self.stamp({"ok": True, "removed": [], "stops": t.stop_states}, tid)
            return self._drop_child_keys(tid, ordered, [])
        if self.exited:
            raise BridgeErr("target VM has exited — close this session")
        # Canonical order: cfg.breaks order (insertion), then any stray
        # raw keys (defensive; normally identical sets).
        ordered = [k for k in self.cfg.breaks if k in self.cfg.break_raws]
        for k in self.cfg.break_raws:
            if k not in ordered:
                ordered.append(k)
        if not ordered:
            # No global intent — but children may still hold ephemeral
            # records: bare clear is the full line-break reset.
            child_warnings = []
            for tid in self.target_order:
                t = self.targets.get(tid)
                if t is None or t.exited or t.state == "ignored" or t.dap is None:
                    continue
                doomed = list(set(t.inherited_keys) | set(t.target_raws.keys()))
                if not doomed:
                    continue
                try:
                    self._drop_child_keys(tid, doomed, [])
                except BridgeErr as e:
                    child_warnings.append(f"{tid}: {e}")
            resp = {"ok": True, "removed": [], "stops": self.stop_states}
            if child_warnings:
                resp["warning"] = "; ".join(child_warnings)
            return self.stamp(resp, "main")
        resp = self._drop_break_keys(ordered, [])
        child_warnings = []
        for tid in self.target_order:
            t = self.targets.get(tid)
            if t is None or t.exited or t.state == "ignored" or t.dap is None:
                continue
            doomed = list(set(t.inherited_keys) | set(t.target_raws.keys()))
            if not doomed:
                continue
            try:
                self._drop_child_keys(tid, doomed, [])
            except BridgeErr as e:
                child_warnings.append(f"{tid}: {e}")
        if child_warnings:
            resp["warning"] = ((resp.get("warning", "") + "; ") if resp.get("warning") else "") + "; ".join(child_warnings)
        return self.stamp(resp, "main")

    def _drop_break_keys(self, keys, missing):
        """Phase 2 of remove/clear: per-file DAP replace without `keys`.
        A file whose DAP call fails keeps every entry (`failed[]`); only
        confirmed files mutate config/state/raws."""
        by_file = {}
        for key in keys:
            by_file.setdefault(key[0], []).append(key)
        removed = []
        failed = []
        for path, items in by_file.items():
            drop = set(items)
            params = []
            for p, ln, c in self.cfg.breaks:
                if p != path or (p, ln, c) in drop:
                    continue
                bp = {"line": ln}
                if c:
                    bp["condition"] = c
                params.append(bp)
            for p, ln, t in self.cfg.logpoints:
                if p != path:
                    continue
                params.append({"line": ln, "logMessage": t})
            try:
                self.dap_request("setBreakpoints",
                                 {"source": {"path": path},
                                  "breakpoints": params},
                                 timeout=5)
            except BridgeErr:
                for key in items:
                    failed.append({"raw": self.cfg.break_raws.get(key, ""),
                                   "spec": key, "error": "backend call failed"})
                continue
            for key in items:
                stored_raw = self.cfg.break_raws.pop(key, "")
                if key in self.cfg.breaks:
                    self.cfg.breaks.remove(key)
                # One live record per file+requested line (same-line
                # differing conds are rejected at add), so the requested
                # line disambiguates without touching conds.
                for i, hk in enumerate(list(self._hitkeys)):
                    if (hk[0] == "break" and len(hk) >= 3
                            and hk[1] == key[0] and hk[2] == key[1]):
                        del self.stop_states[i]
                        del self._hitkeys[i]
                        break
                removed.append({"raw": stored_raw, "spec": self._remove_spec(key),
                                "kind": "break", "hits": 0})
        if not removed:
            raise BridgeErr(
                "breaks remove failed for "
                + ", ".join(self.rel_file(k[0]) + f":{k[1]}" for k in keys))
        resp = {"ok": True, "removed": removed, "stops": self.stop_states}
        if missing:
            resp["missing"] = missing
        if failed:
            resp["failed"] = failed
            resp["warning"] = ("partial remove: no change for "
                               + ", ".join(f["raw"] for f in failed if f["raw"]))
        return resp

    def _remove_spec(self, key):
        """Display spec for a stored key (relative path + cond)."""
        path, line, cond = key[0], key[1], key[2] if len(key) > 2 else None
        spec = f"{self.rel_file(path)}:{line}"
        if cond:
            spec += f"|{cond}"
        return spec

    def cmd_logs(self, req):
        tail = max(1, min(500, int(req.get("tail", 50))))
        # Flush recently arrived output first: in logpoints-only sessions
        # nothing else ever pumps the queue. Skipped while a resume is
        # outstanding (M5: no second DAP reader — the pump owns the wire;
        # the file read below is still prompt and truthful).
        if not self.exited:
            with self._gate:
                busy = bool(self._outstanding)
            if not busy:
                self.drain_pending()
        path = os.path.join(self.cfg.dir, "logs.jsonl")
        try:
            with open(path) as f:
                lines = f.read().splitlines()
        except OSError:
            lines = []
        # total = retained lines on disk (<= MAX_LOG_LINES); dropped =
        # lifetime lines evicted by the ring; truncated = the tail was cut
        # OR any line was ever evicted (historical drops, not just the cut).
        dropped = getattr(self, "log_dropped", 0)
        return {"ok": True, "total": len(lines),
                "truncated": len(lines) > tail or dropped > 0,
                "dropped": dropped, "lines": lines[-tail:]}

    def require_stopped(self):
        if self._serving == "main" and self.main_exited:
            raise BridgeErr("target main has exited — close this session")
        if self.exited:
            raise BridgeErr("target VM has exited — close this session")
        if not self.suspended or self.thread_id is None:
            raise BridgeErr("no stopped thread (target is running — continue first)")
        if not self.frames:
            self.refresh_frames(timeout=5)
            if not self.frames:
                raise BridgeErr("target stopped but stack is unavailable; retry context or continue")

    def require_live(self):
        if self._serving == "main" and self.main_exited:
            raise BridgeErr("target main has exited — close this session")
        if self.exited:
            raise BridgeErr("target VM has exited — close this session")

    def _route(self, req, fn):
        """Serve a frame-bound read against the resolved target."""
        tid = self.resolve_target(req)
        with self._TargetScope(self, tid):
            resp = fn()
        return self.stamp(resp, tid)

    def _busy_error(self, cmd, tid):
        """M5 immediate busy rejection (caller holds _gate): a second resume
        or mutation on the SAME target never silently queues; a global
        breakpoint mutation conflicts with ANY outstanding resume; eval is
        exclusive (it can mutate) and busy-rejects on its target even when
        parked frames exist. Live reads and frame-bound context/vars/stack
        never busy-reject (the latter fail fast via require_stopped once the
        resume publishes running). Returns the error string or None."""
        out = self._outstanding
        # wait never resumes but still occupies its target's slot (a rival
        # resume would steal the stop it long-polls for); capture resumes
        # at the end, so it occupies the slot throughout.
        if cmd in RESUME_CMDS or cmd in WAIT_CMDS or cmd in CAPTURE_CMDS:
            if tid is not None and tid in out:
                return f"busy: {out[tid]} outstanding for {tid}"
            return None
        if cmd in MUTATION_CMDS:
            if tid is not None:
                # Target-scoped mutation conflicts with that target only.
                if tid in out:
                    return f"busy: {out[tid]} outstanding for {tid}"
                return None
            # Global mutation conflicts with any outstanding resume.
            if out:
                first = sorted(out)[0]
                return f"busy: {out[first]} outstanding for {first}"
            return None
        if cmd == "eval":
            if tid is not None and tid in out:
                return f"busy: {out[tid]} outstanding for {tid}"
            return None
        return None

    def _mutation_tid(self, req):
        """Target scope of a breakpoint mutation: explicit non-main target
        (target-scoped, conflicts that target only), else None (global,
        conflicts with any outstanding resume)."""
        scope = req.get("target") if isinstance(req, dict) else None
        if scope is not None and scope != "main":
            return scope
        return None

    def dispatch(self, req):
        cmd = req.get("cmd")
        try:
            timeout = float(req.get("timeout", self.cfg.timeout))
        except (ValueError, TypeError) as e:
            raise BridgeErr("timeout must be between 0 and 3600 seconds") from e
        if not math.isfinite(timeout) or not 0 < timeout <= 3600:
            raise BridgeErr("timeout must be between 0 and 3600 seconds")
        # Close is terminal and always accepted, even with a resume
        # outstanding (bounded tolerance lives on the CLI side; the bridge
        # never deadlocks waiting for a handler that itself awaits a stop).
        if cmd == "close":
            raise _Close()
        # M5 acceptance section (atomic under _gate): stamp deterministic
        # targetless selection, fail fast on closing, busy-reject rival
        # resume/mutation/eval, and register the resume BEFORE any await or
        # DAP traffic so a simultaneous rival observes it.
        resume_tid = None
        if isinstance(req, dict) and req.get("target") is None \
                and "_auto_target" not in req:
            try:
                auto = self.resolve_target(req)
            except BridgeErr:
                auto = None
            if auto is not None:
                req = dict(req)
                req["_auto_target"] = auto
        with self._gate:
            if self._closing:
                raise BridgeErr("session is closing")
            check_tid = None
            if cmd in RESUME_CMDS or cmd in WAIT_CMDS or cmd in CAPTURE_CMDS \
                    or cmd == "eval":
                try:
                    check_tid = self._resolve_target_inner(req)
                except BridgeErr:
                    check_tid = None
            elif cmd in MUTATION_CMDS:
                check_tid = self._mutation_tid(req)
            if check_tid is not None or cmd in MUTATION_CMDS:
                err = self._busy_error(cmd, check_tid)
                if err:
                    raise BridgeErr(err)
            if (cmd in RESUME_CMDS or cmd in WAIT_CMDS
                    or cmd in CAPTURE_CMDS) and check_tid is not None:
                self._outstanding[check_tid] = cmd
                resume_tid = check_tid
        try:
            if cmd == "targets":
                return self.cmd_targets()
            if cmd == "context":
                return self._route(req, self.cmd_context)
            if cmd == "stack":
                return self._route(req, self.cmd_stack)
            if cmd == "vars":
                return self._route(req, lambda: self.cmd_vars(req))
            if cmd == "eval":
                return self._route(req, lambda: self.cmd_eval(req))
            if cmd == "step":
                return self.cmd_step(req, timeout)
            if cmd == "continue":
                return self.cmd_continue(req, timeout)
            if cmd == "wait":
                return self.cmd_wait(req, timeout)
            if cmd == "capture":
                return self.cmd_capture(req, timeout)
            if cmd == "threads":
                return self.cmd_threads(req)
            if cmd == "breaks":
                return self.cmd_breaks(req)
            if cmd == "breaksAdd":
                return self.cmd_breaks_add(req)
            if cmd == "breaksRemove":
                return self.cmd_breaks_remove(req)
            if cmd == "breaksClear":
                return self.cmd_breaks_clear(req)
            if cmd == "logs":
                resp = self.cmd_logs(req)
                return self.stamp(resp, "main")
            raise BridgeErr(f"unknown cmd: {cmd}")
        finally:
            if resume_tid is not None:
                with self._gate:
                    if self._outstanding.get(resume_tid) == cmd:
                        del self._outstanding[resume_tid]

    def cleanup(self):
        # Retired (failed-handshake) sockets first: they are closed ONLY
        # here, at overall teardown — never mid-session (never-close-live
        # debugpy law). Then live table sockets (fully established, so
        # contained), then the parent terminate which reaps the tree.
        for _tid, sock in self._retired_sockets:
            try:
                if sock is not None:
                    sock.close()
            except Exception:
                pass
        self._retired_sockets = []
        # Child sessions next: raw close only, and only here at teardown.
        # (A DAP disconnect would finalize the shared adapter session
        # before the parent's terminate below; raw closes of
        # fully-established sessions are adapter-contained, while the
        # parent terminate reaps the whole tree on launch anyway.)
        for tid in list(self.target_order):
            t = self.targets.pop(tid, None)
            if t is None:
                continue
            try:
                if t.sock is not None:
                    t.sock.close()
            except Exception:
                pass
        if self.dap is not None:
            try:
                if self.cfg.kind == "launch":
                    self.dap_request("disconnect", {"terminateDebuggee": True}, timeout=5)
                else:
                    self.dap_request("disconnect", {}, timeout=5)
            except Exception:
                pass
            try:
                self.dap.sock.close()
            except Exception:
                pass
        if self.adapter is not None:
            try:
                self.adapter.terminate()
            except Exception:
                pass
            # Reap, don't just signal: terminate() alone can leave the
            # adapter lingering (or zombied). Bounded wait, then kill.
            try:
                self.adapter.wait(timeout=5)
            except Exception:
                try:
                    self.adapter.kill()
                except Exception:
                    pass
                try:
                    self.adapter.wait(timeout=5)
                except Exception:
                    pass


class _Close(Exception):
    pass


def write_file(path, content):
    """Atomic same-dir publish: unique temp (create-new) + replace, so a
    concurrent `status` read never sees a torn session.json. Best effort
    (callers treat state files as advisory), but a temp is never left behind
    on failure. Plain log appends stay append-only — only full rewrites
    (state files, log-ring trims) come through here."""
    try:
        data = content.encode("utf-8") if isinstance(content, str) else content
        directory = os.path.dirname(os.path.abspath(path))
        tmp = None
        for _ in range(8):
            cand = os.path.join(
                directory, f".tmp-{os.getpid()}-{time.time_ns()}")
            try:
                fd = os.open(cand, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
            except FileExistsError:
                continue
            except OSError:
                return
            tmp = cand
            try:
                with os.fdopen(fd, "wb") as f:
                    f.write(data)
            except OSError:
                try:
                    os.unlink(cand)
                except OSError:
                    pass
                return
            break
        if tmp is None:
            return
        try:
            os.replace(tmp, path)
        except OSError:
            try:
                os.unlink(tmp)
            except OSError:
                pass
    except OSError:
        pass


def format_unexpected(e):
    """Sanitized unexpected-crash payload: exception class + message plus a
    short traceback tail, capped ~2KB. Never env/secrets — only the
    exception and our own frames. Known Usage/BridgeErr messages bypass
    this (they keep their exact text on the error.json path)."""
    import traceback
    try:
        tb = "".join(traceback.format_exception(type(e), e, e.__traceback__)[-6:])
    except Exception:
        tb = ""
    body = f"{type(e).__name__}: {e}"
    if tb.strip():
        body += "\n" + tb.strip()
    body = body.strip()
    if len(body) > MAX_ERROR_CHARS:
        body = body[:MAX_ERROR_CHARS - 1] + "…"
    return f"internal: {body}"


def write_owner(session_dir):
    """Claim the session dir; see nodebridge owner.json (same contract)."""
    import random
    import time
    nonce = f"{os.getpid()}-{time.time()}-{random.randrange(1_000_000_000)}"
    write_file(os.path.join(session_dir, "owner.json"),
               json.dumps({"pid": os.getpid(), "nonce": nonce}))
    return nonce


def am_owner(session_dir, nonce):
    try:
        with open(os.path.join(session_dir, "owner.json")) as f:
            return json.load(f).get("nonce") == nonce
    except (OSError, ValueError):
        return False


def idle_pump(st):
    """Drain a bounded batch between commands, using the sole DAP reader.
    Polls every live session (main + children): child attaches, stops, and
    exits observed here never move another target's park."""
    if st.exited:
        return
    if st.dap is None and not st.live_targets():
        return
    deadline = time.monotonic() + 0.05
    try:
        for _ in range(256):
            if time.monotonic() >= deadline:
                break
            # Requests may have stashed events while inspecting an earlier
            # stop. Consume those before reading newer socket messages,
            # then buffered/coalesced bytes before selecting (same
            # invariant as pump).
            progressed = False
            for tid, conn, _sock in st._conn_entries():
                while True:
                    msg = st._pop_stash(conn)
                    if msg is None:
                        break
                    try:
                        if st._dispatch_pumped(msg, tid):
                            return
                    except BridgeErr as e:
                        sys.stderr.write(f"dap: {e}\n")
                        return
                    progressed = True
                    if time.monotonic() >= deadline:
                        break
                while time.monotonic() < deadline:
                    try:
                        msg = st._try_read(conn)
                    except BridgeErr as e:
                        if "closed" in str(e).lower():
                            try:
                                st._conn_dead(tid, e)
                            except BridgeErr as fatal:
                                sys.stderr.write(f"dap: {fatal}\n")
                                return
                            break
                        sys.stderr.write(f"dap: {e}\n")
                        return
                    if msg is None:
                        break
                    progressed = True
                    try:
                        if st._dispatch_pumped(msg, tid):
                            return
                    except BridgeErr as e:
                        sys.stderr.write(f"dap: {e}\n")
                        return
                if time.monotonic() >= deadline:
                    break
            if progressed:
                continue
            socks = [s for _, _, s in st._conn_entries() if s is not None]
            if not socks:
                return
            try:
                ready, _, _ = select.select(
                    socks, [], [], max(0.001, deadline - time.monotonic()))
            except (OSError, ValueError):
                return
            if not ready:
                break
            ready_ids = {id(s) for s in ready}
            for tid, conn, sock in st._conn_entries():
                if sock is None or id(sock) not in ready_ids:
                    continue
                try:
                    sock.settimeout(max(0.001, deadline - time.monotonic()))
                except OSError:
                    break
                try:
                    msg = conn._read_msg()
                except (socket.timeout, TimeoutError):
                    break
                except BridgeErr as e:
                    if "closed" in str(e).lower():
                        try:
                            st._conn_dead(tid, e)
                        except BridgeErr as fatal:
                            sys.stderr.write(f"dap: {fatal}\n")
                            return
                        continue
                    st.exited = True
                    try:
                        st.publish_state(False)
                    except Exception:
                        pass
                    break
                try:
                    if st._dispatch_pumped(msg, tid):
                        return
                except BridgeErr as e:
                    sys.stderr.write(f"dap: {e}\n")
                    return
    finally:
        pass


def _handle_one(st, conn):
    """Serve a single CLI connection on a handler thread (M5): exactly one
    request and one response per connection. A client disconnect never
    cancels target-side work — the resume still publishes state; only this
    connection's response is dropped (best-effort write)."""
    try:
        try:
            req = read_frame(conn)
        except BridgeErr as e:
            try_write_frame(conn, {"ok": False, "error": str(e)})
            return
        try:
            resp = st.dispatch(req)
            if isinstance(resp, dict) and "target" not in resp:
                resp["target"] = getattr(st, "_serving", "main")
            try:
                write_frame(conn, resp)
            except Exception:
                pass  # client went away mid-command: work already ran and
                # state already published; only this response drops.
        except _Close:
            # Close is terminal and accepted despite any outstanding resume:
            # never wait for a handler that itself awaits a stop — publish
            # the teardown now (launch kills its tree, attach detaches) and
            # stop accepting. In-flight resume handlers abort on the closed
            # sockets; their responses drop harmlessly (daemon threads).
            with st._gate:
                st._closing = True
            try_write_frame(conn, {"ok": True, "closed": True, "target": "main"})
            try:
                st.cleanup()
            except Exception:
                pass
            try:
                st.server.close()
            except Exception:
                pass
        except BridgeErr as e:
            target = getattr(st, "_pending_target", None) or getattr(st, "_serving", "main")
            try_write_frame(conn, {"ok": False, "error": str(e), "target": target})
        except Exception as e:
            target = getattr(st, "_pending_target", None) or getattr(st, "_serving", "main")
            try_write_frame(conn, {"ok": False, "error": f"internal: {e}",
                                    "target": target})
    finally:
        try:
            conn.close()
        except Exception:
            pass
        with st._gate:
            st._active -= 1


def serve(st, server, nonce):
    # M5: accept loop stays lean — each connection gets one bounded handler
    # thread (max MAX_ACTIVE_HANDLERS; overflow is an immediate rejection,
    # never an unbounded spawn). Idle windows pump only while NO resume is
    # outstanding: the outstanding resume's pump owns wire consumption, and
    # a second consumer would steal its stop.
    st.server = server
    try:
        server.settimeout(0.1)
    except OSError:
        pass
    while True:
        # Abandoned (dir rm'd or respawned under our name)? Quit quietly.
        if not am_owner(st.cfg.dir, nonce):
            with st._gate:
                st._closing = True
            try:
                st.cleanup()
            except Exception:
                pass
            return
        with st._gate:
            if st._closing:
                return
            outstanding = bool(st._outstanding)
        if not outstanding:
            idle_pump(st)
        try:
            conn, _ = server.accept()
        except (socket.timeout, TimeoutError):
            continue
        except OSError:
            return
        with st._gate:
            if st._closing:
                try:
                    conn.close()
                except Exception:
                    pass
                return
            if st._active >= MAX_ACTIVE_HANDLERS:
                try_write_frame(conn, {"ok": False,
                                        "error": "overloaded: too many active handlers",
                                        "target": "main"})
                try:
                    conn.close()
                except Exception:
                    pass
                continue
            st._active += 1
        t = threading.Thread(target=_handle_one, args=(st, conn), daemon=True)
        t.start()


def main(argv):
    cfg_dir = None
    try:
        cfg = parse_args(argv)
    except Usage as e:
        die(str(e), 2)
    except Exception as e:
        die(f"internal: {e}", 1)
    cfg_dir = cfg.dir
    try:
        os.makedirs(cfg.dir, exist_ok=True)
        nonce = write_owner(cfg.dir)
        server = socket.socket()
        server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        server.bind(("127.0.0.1", 0))
        server.listen(5)
        st = Session(cfg)
        st._nonce = nonce
        st.session_port = server.getsockname()[1]
        try:
            if cfg.kind == "launch":
                st.start_adapter()
            if cfg.kind == "launch":
                st.handshake_launch()
            else:
                st.handshake_attach()
            if cfg.breaks or cfg.methods or cfg.want_exc:
                try:
                    st.pump(cfg.timeout)
                except StopTimeout as e:
                    # Attach-only fallback: a live target that never hits
                    # stays a running session with breaks armed; the agent
                    # triggers the stop later via continue. Launch timeouts
                    # re-raise into the error.json path below, and target
                    # exit (plain BridgeErr) never falls back.
                    if cfg.kind != "attach":
                        extra = st.unresolved_summary()
                        if extra:
                            raise BridgeErr(f"{e}; {extra}")
                        raise
                    st.publish_state(False)
                    serve(st, server, nonce)
                    return
            write_file(os.path.join(cfg.dir, "session.json"), json.dumps(
                {"name": os.path.basename(cfg.dir), "kind": cfg.kind,
                 "port": server.getsockname()[1],
                 "stopped": bool(st.suspended or any(
                     t.suspended for t in st.live_targets())),
                 "lastStop": st.last_stop, "updatedAt": int(time.time()),
                 "observedTarget": cfg.observed_target}))
            serve(st, server, nonce)
        except (Usage, BridgeErr) as e:
            # Failed setup must not leak the spawned adapter/target:
            # clean up first, then report (the CLI removes the dir).
            try:
                st.cleanup()
            except Exception:
                pass
            write_file(os.path.join(cfg.dir, "error.json"),
                       json.dumps({"error": str(e)}))
            raise
        except Exception as e:
            # Unexpected setup crash (never a silent exit-1): same cleanup,
            # then a sanitized error.json the CLI surfaces. The name stays
            # reusable — the CLI removes failed-setup dirs wholesale.
            try:
                st.cleanup()
            except Exception:
                pass
            write_file(os.path.join(cfg.dir, "error.json"),
                       json.dumps({"error": format_unexpected(e)}))
            raise
        finally:
            try:
                server.close()
            except Exception:
                pass
    except (Usage, BridgeErr) as e:
        die(str(e), 1)
    except Exception as e:
        if cfg_dir is not None:
            write_file(os.path.join(cfg_dir, "error.json"),
                       json.dumps({"error": format_unexpected(e)}))
        die(format_unexpected(e), 1)


if __name__ == "__main__":
    main(sys.argv[1:])
