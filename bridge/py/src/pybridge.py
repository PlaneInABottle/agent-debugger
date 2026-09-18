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

import csv
import faulthandler
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
# Display-independent change tracking bound: top-level values scanned per
# frame for changed/removed detection (same on all adapters). The vars
# display cap (MAX_VARS=20 + sentinel) is unchanged and independent.
CHANGE_TRACK_MAX = 256
# Shallow tracking preview cap (chars) for one value; no recursive child
# fetch, no getters, no target eval.
TRACK_PREVIEW = 200
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


class ConfigErr(BridgeErr):
    """Semantic setup failure: local/spec validation or a valid protocol
    error response (the adapter refused the request itself — not a
    socket/timeout/framing loss). Subclasses BridgeErr so every existing
    `except BridgeErr` still catches; error.json `phase` derives from this
    type, never from message text. DAP/CDP/JDI transport failures
    (socket errors, timeouts, connection drops, target exit) stay plain
    BridgeErr so the CLI keeps endpoint diagnosis for them."""


class RuntimeErr(Exception):
    """Unexpected internal failure after a successful bridge operation
    (post-handshake crash, degraded change tracking, etc.). Distinct from
    BridgeErr (transport) and Usage/ConfigErr (config) so error.json
    `phase` reads `runtime` — truthful internal error, never endpoint
    diagnosis. Never carries variable data/secrets (class + short
    message only)."""


class StopTimeout(BridgeErr):
    """First-stop wait timed out. Typed so attach can fall back to a live
    running session while launch still fails — never match by message.
    Wait/capture timeouts may carry an additive `wait_context` dict
    (triggerStatus unknown, never a fabricated verdict); the message keeps
    the exact frozen `timeout: no stop within Ns` prefix."""
    def __init__(self, message="", wait_context=None):
        super().__init__(message)
        self.wait_context = wait_context


def is_framing_error_text(msg):
    """True for malformed-adapter-frame texts (never target death): the
    framing layer raises plain BridgeErr for these, and the idle pump must
    not translate them into `exited` (contrast "closed"-family transport
    losses, which do mean the connection died)."""
    low = msg.lower()
    return ("header too large" in low or "frame too large" in low
            or "invalid dap" in low or "invalid content-length" in low)


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

    def request(self, command, args=None, timeout=30, semantic=False):
        with self.mu:
            return self._request_locked(command, args, timeout, semantic)

    def _request_locked(self, command, args=None, timeout=30, semantic=False):
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
                        # Valid protocol refusal — semantic only when the
                        # caller opted in (breakpoint installation); session
                        # establishment (initialize/attach/configurationDone)
                        # stays transport so endpoint diagnosis is kept.
                        if semantic:
                            raise ConfigErr(m.get("message", f"{command} failed"))
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
                    if semantic:
                        raise ConfigErr(msg.get("message", f"{command} failed"))
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
        self.target_identity_seed = None  # layered CLI seed identity (dict)
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


def _logpoint_sep(spec, start=0):
    """Index of the next `:` separator at/after `start`, skipping a
    Windows drive prefix (`C:\\...` / `C:/...`): the colon at index 1 of
    a drive-absolute path never separates fields. Plain `C:4:t` (no
    slash: drive-relative or a one-char file) keeps legacy behavior."""
    if (start == 0 and len(spec) >= 3 and spec[1] == ":"
            and spec[0].isalpha() and spec[2] in "/\\"):
        start = 2
    return spec.find(":", start)


def parse_logpoint(spec, cfg):
    # Class:line:template with {expr} holes (paths split like Class files).
    first = _logpoint_sep(spec)
    second = _logpoint_sep(spec, first + 1) if first >= 0 else -1
    if first <= 0 or second <= 0:
        raise Usage("--logpoint must look like path:line:template")
    path = resolve_source_path(spec[:first], cfg.src_dirs)
    try:
        lineno = int(spec[first + 1:second])
    except ValueError:
        raise Usage(f"bad line in --logpoint: {spec}")
    _check_line_range(path, lineno, spec)
    if not spec[second + 1:]:
        raise Usage(f"--logpoint template is empty: {spec}")
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

    def _need(flag):
        """Next argv value or a Usage (never IndexError->traceback/internal).

        Every value flag shares this path so a missing value is always a
        clean `session needs a value for --flag` Usage, matching --port /
        --timeout. `--` program args never reach here (dashdash branch)."""
        nonlocal i
        i += 1
        try:
            return argv[i]
        except IndexError:
            raise Usage(f"session needs a value for {flag}")

    while i < len(argv):
        a = argv[i]
        if dashdash:
            cfg.prog_args.append(a)
            i += 1
            continue
        if a == "--":
            dashdash = True
        elif a == "--kind":
            cfg.kind = _need(a)
        elif a == "--dir":
            cfg.dir = _need(a)
        elif a == "--program":
            cfg.program = os.path.abspath(_need(a))
        elif a == "--module":
            cfg.module = _need(a)
        elif a == "--python":
            cfg.python = _need(a)
        elif a == "--host":
            cfg.host = _need(a)
        elif a == "--port":
            raw_port = _need(a)
            try:
                cfg.port = int(raw_port)
            except ValueError:
                raise Usage(f"bad --port {raw_port!r} (want an integer)")
        elif a == "--src":
            cfg.src_dirs.append(os.path.abspath(_need(a)))
        elif a == "--break":
            pending_stops.append(("break", _need(a)))
        elif a == "--logpoint":
            pending_stops.append(("logpoint", _need(a)))
        elif a == "--watch":
            raise Usage("--watch has no debugpy equivalent yet (Python)")
        elif a == "--exit":
            raise Usage("--exit has no debugpy equivalent yet (Python)")
        elif a == "--timeout":
            raw_timeout = _need(a)
            try:
                cfg.timeout = float(raw_timeout)
            except ValueError:
                raise Usage(f"bad --timeout {raw_timeout!r} "
                            f"(want seconds between 0 and 3600)")
            if not math.isfinite(cfg.timeout) or not 0 < cfg.timeout <= 3600:
                raise Usage("timeout must be between 0 and 3600 seconds")
        elif a == "--subprocess":
            cfg.subprocess = True
        elif a == "--target-identity":
            try:
                parsed = json.loads(_need(a))
                cfg.target_identity_seed = parsed if isinstance(parsed, dict) else None
            except ValueError:
                cfg.target_identity_seed = None
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


# ---------------------------------------------------------------- layered target identity (M-ID)
# `{debuggee, endpoint, adapter}` roles with strict confidence:
# protocol-confirmed only from the DAP `process` event; os-corroborated
# only for the OS-observed listener owner (seeded by the CLI's layered
# `--target-identity` seed); everything else is unavailable (never guessed,
# no parent-tree inference). Every new string is redacted + capped before
# persistence or output; env is never collected.
IDENT_FIELD_CAP = 512   # per-field chars (matches the CLI layered-identity cap)
IDENT_ROLE_CAP = 2048   # per-role serialized chars
IDENT_TOTAL_CAP = 4096  # aggregate chars over the three roles
IDENT_ARRAY_CAP = 32    # elements per array (head kept, dropped tail marked)

_SECRET_SUBSTR = ("password", "passwd", "secret", "apikey",
                  "authorization", "authtoken", "accesstoken")
_SECRET_TOKEN = ("token", "auth", "pwd", "pass", "pw")

WAIT_NOTE = ("external trigger execution is not observed by the debugger; "
             "this timeout means no stop was observed, "
             "not that the code is unreachable")


def _is_secret_flag(flag):
    # Mirrors the CLI redactor: substring keys on the flattened name, short
    # keys only on token boundaries (suffix match — a mere prefix like
    # --author or --passage never matches).
    t = flag.lstrip("-").lower()
    flat = "".join(c for c in t if c not in "-_")
    if any(k in flat for k in _SECRET_SUBSTR):
        return True
    toks = t.replace("_", "-").replace(".", "-").split("-")
    return any(tok == k or tok.endswith(k) for tok in toks for k in _SECRET_TOKEN)


def redact_identity_argv(argv):
    """Redact secret values from an argv (mirrors the CLI redactor):
    `--token <v>` drops the value, `--password=<v>`/`key:<v>` mask it."""
    out = []
    skip_next = False
    for a in argv or []:
        if not isinstance(a, str):
            continue
        if skip_next:
            skip_next = False
            if not (a.startswith("-") and len(a) > 1):
                out.append("[redacted]")
                continue
        split = -1
        for sep in ("=", ":"):
            i = a.find(sep)
            if i > 0 and (split < 0 or i < split):
                split = i
        if split > 0:
            head = a[:split]
            if _is_secret_flag(head):
                out.append(f"{head}{a[split]}[redacted]")
            else:
                out.append(a)
        else:
            out.append(a)
            if _is_secret_flag(a):
                skip_next = True
    return out


def _windows_process_argv(pid):
    """Best-effort argv for a native Windows pid (stdlib only, no psutil).
    tasklist reports the image name only, so argv is [image]: liveness +
    identity corroboration, never a full command line. None when unreadable
    (dead pid, tasklist missing, non-Windows) — callers keep best-effort."""
    if os.name != "nt":
        return None
    try:
        out = subprocess.run(
            ["tasklist", "/FI", f"PID eq {int(pid)}", "/FO", "CSV", "/NH"],
            capture_output=True, text=True, timeout=5).stdout
    except Exception:
        return None
    for line in out.splitlines():
        line = line.strip()
        if not line.startswith('"'):
            continue  # INFO: no matching task — pid is gone
        try:
            row = next(csv.reader([line]))
        except Exception:
            continue
        if len(row) > 1 and row[1].strip() == str(int(pid)) and row[0]:
            return [row[0]]
    return None


def _windows_process_image(pid):
    """Full exe path of a native Windows pid via stdlib ctypes only.
    None when unreadable (dead/reused pid, access denied, non-Windows)."""
    if os.name != "nt":
        return None
    try:
        import ctypes
        kernel = ctypes.windll.kernel32
        handle = kernel.OpenProcess(0x1000, False, int(pid))
        if not handle:
            return None
        try:
            buf = ctypes.create_unicode_buffer(32768)
            size = ctypes.c_ulong(len(buf))
            if kernel.QueryFullProcessImageNameW(handle, 0, buf,
                                                 ctypes.byref(size)):
                return buf.value or None
            return None
        finally:
            kernel.CloseHandle(handle)
    except Exception:
        return None


def _cap_str(s, limit=IDENT_FIELD_CAP):
    if not isinstance(s, str):
        return s
    if len(s) <= limit:
        return s
    return f"{s[:limit]}… (+{len(s) - limit} more chars)"


def _cap_walk(v):
    if isinstance(v, str):
        return _cap_str(v)
    if isinstance(v, list):
        if len(v) > IDENT_ARRAY_CAP:
            dropped = len(v) - IDENT_ARRAY_CAP
            v = v[:IDENT_ARRAY_CAP] + [f"… (+{dropped} more)"]
        return [_cap_walk(e) for e in v]
    if isinstance(v, dict):
        return {k: _cap_walk(e) for k, e in v.items()}
    return v


def _shrink_to_total(obj, total_cap):
    """Shrink the longest string until the object fits total_cap. Each pass
    either truncates the longest string (marker is shorter) or collapses it
    to a 1-char marker (strictly smaller than any picked string, which has
    length > 1), so the loop always terminates."""

    def longest_loc(v, path):
        """(path, length) of the longest shrinkable string under v."""
        best = (None, 1)
        if isinstance(v, str) and len(v) > 1:
            best = (path, len(v))
        elif isinstance(v, list):
            for i, e in enumerate(v):
                cand = longest_loc(e, path + [i])
                if cand[1] > best[1]:
                    best = cand
        elif isinstance(v, dict):
            for k, e in v.items():
                cand = longest_loc(e, path + [k])
                if cand[1] > best[1]:
                    best = cand
        return best

    def get_at(root, path):
        for p in path:
            root = root[p]
        return root

    def set_at(root, path, val):
        for p in path[:-1]:
            root = root[p]
        root[path[-1]] = val

    passes = 0
    while len(json.dumps(obj)) > total_cap and passes < 4096:
        passes += 1
        path, n = longest_loc(obj, [])
        if path is None:
            break
        cur = get_at(obj, path)
        cand = _cap_str(cur, max(1, n - 64))
        set_at(obj, path, cand if len(cand) < n else "…")
    return obj


def _cap_role(role):
    return _shrink_to_total(_cap_walk(role), IDENT_ROLE_CAP)


def _cap_identity(ident):
    capped = {k: (_cap_role(v) if isinstance(v, dict) else v)
              for k, v in ident.items()}
    return _shrink_to_total(capped, IDENT_TOTAL_CAP)


def _is_process_event(msg):
    return (isinstance(msg, dict) and msg.get("type") == "event"
            and msg.get("event") == "process"
            and isinstance(msg.get("body"), dict))


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
# Deferred child admission: debugpyAttach events stage here (bounded) while
# the single in-flight handshake runs outside Session._gate, so M5 live
# reads stay prompt. Overflow marks the child ignored pre-connect (no
# socket, no budget), exactly like the retired-full path.
MAX_PENDING_ATTACH = 16

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
        self.last_removed = "[]"
        self.last_changed_complete = False
        self.last_change_tracking = {"complete": False, "scanned": 0,
                                      "total": None, "truncated": False,
                                      "reason": "first-snapshot"}
        self.last_track_warn = None
        self.last_track_complete = False  # was previous scan exhaustive
        self.last_track_reason = "first-snapshot"  # reason when incomplete
        self.logpoints = []  # inherited snapshot (resend companions)
        self._awaiting_continued = False
        self._suspects = []
        self._co_seen = set()
        self._suspect_warned = False


def _serialized_gate(fn):
    """Serialize one breaks-mutation entry on Session._gate (reentrant, so
    dispatch acceptance, _TargetScope, and nested mutation calls stay
    safe). Serve runs one thread per connection: without this, concurrent
    identical adds both pass the have-check, then plant and record twice.
    With it, check+plant+state mutation is atomic — the second add sees
    the first's state and reports empty-added. DAP under _gate follows the
    existing _TargetScope precedent (bounded 5s per file, order _gate ->
    mu, never reverse)."""
    def wrap(self, *args, **kwargs):
        with self._gate:
            return fn(self, *args, **kwargs)
    wrap.__name__ = fn.__name__
    return wrap


# ---------------------------------------------------------------------------
# M3 in-file state owners, narrow track (single-file MUST: src/bridge.rs
# embeds this file via include_str!, so no physical split). Session stays
# the sole runtime owner: it constructs exactly one TargetRegistry + one
# ServerState, holds Session._gate (threading.RLock; order _gate ->
# DapConn.mu, never reverse, no new locks anywhere), and orchestrates DAP
# transport. Each owner holds its own mutable state and exposes a bounded
# API plus a cheap assert_valid() invariant for tests. Breakpoint
# bookkeeping and the stop/wait/capture machine stay Session-owned sections
# (their state IS the swapped context / thread-local attribution — a
# wrapper only added bypass; rejected, see architecture-map §6).
# ---------------------------------------------------------------------------


class TargetRegistry:
    """Canonical owner of per-target lifecycle + the in-flight serving id.

    Holds: live/ignored child table (tid -> ChildTarget), creation order,
    every id ever issued (never reused), bounded exited history + eviction
    counter, ignored/helpers lifetime counters, the deferred-attach staging
    queue + single in-flight handshake flag, retired (failed-handshake)
    sockets, and _serving (target id of the in-flight command).

    Per-target park/break fields live on Session (main context) and each
    ChildTarget; swap_fields() context-switches them. DAP IO stays Session
    orchestration. Lock precondition: caller must hold Session._gate (all
    methods are gate-serialized sections; none acquire locks, none do
    socket/DAP IO — only a bounded stderr warn on overflow release, as
    before). Invariant: order ⊆ table keys; seen ⊇ issued ids; serving is
    "main" or a table key; exited history ≤ MAX_EXITED_HISTORY.
    """

    def __init__(self):
        self.table = {}          # tid -> ChildTarget (live + ignored)
        self.order = []          # creation order for roster listing
        self.seen_ids = set()    # every id ever issued (no reuse)
        self.exited_history = []  # bounded last-known entries (max 16)
        self.ignored = 0         # released (over-budget) children, lifetime
        self.helpers_released = 0  # spawn `-c` helpers, lifetime (no budget)
        self.dropped_exited = 0  # exited-history evictions, lifetime
        self.serving = "main"    # target id of the in-flight command
        self.retired = []        # (tid, sock) failed handshakes, kept OPEN
        self.attach_pending = []  # staged debugpyAttach events (bounded)
        self.attach_busy = False  # single in-flight handshake flag

    # -- roster reads (caller holds _gate) --

    def live_targets(self):
        """Live (non-exited) children in creation order."""
        return [self.table[tid] for tid in self.order
                if tid in self.table and not self.table[tid].exited]

    def active_nonmain(self):
        """Budgeted children: live, tracked (ignored releases stay in the
        table with state ignored and never consume budget; helpers never
        enter the table at all)."""
        return [t for t in self.live_targets()
                if t.state in ("running", "stopped")]

    # -- target resolution (caller holds _gate) --

    def resolve_inner(self, req, main_suspended, main_exited, main_seq):
        """Which target a command serves: explicit id (validated), else the
        most recently stopped live target, else main. IDs/errors/selection
        semantics unchanged (moved verbatim from Session)."""
        want = req.get("target") if isinstance(req, dict) else None
        if want is None and isinstance(req, dict):
            auto = req.get("_auto_target")
            if auto is not None:
                inner = dict(req)
                inner["target"] = auto
                del inner["_auto_target"]
                return self.resolve_inner(inner, main_suspended,
                                          main_exited, main_seq)
        if want is not None:
            if want == "main":
                if main_exited:
                    raise BridgeErr("target main has exited — close this session")
                return "main"
            t = self.table.get(want)
            if t is None:
                for e in self.exited_history:
                    if e.get("id") == want:
                        raise BridgeErr(
                            f"target {want} has exited — close this session")
                raise BridgeErr(f"unknown target: {want}")
            if t.state == "ignored":
                raise BridgeErr(f"target {want} was released (over budget)")
            if t.exited or t.state == "exited":
                raise BridgeErr(f"target {want} has exited — close this session")
            return want
        best, best_seq = "main", (main_seq if main_suspended else 0)
        for t in self.live_targets():
            if t.suspended and t.stop_seq > best_seq:
                best, best_seq = t.id, t.stop_seq
        if best == "main" and main_exited:
            raise BridgeErr("target VM has exited — close this session")
        return best

    # -- target-local context switch (caller holds _gate via _TargetScope;
    # DAP IO inside takes DapConn.mu: order _gate -> mu, never reverse) --

    @staticmethod
    def swap_fields(dst, src):
        """Exchange park/DAP fields between main (Session) and a child."""
        for f in ("dap", "thread_id", "frames", "suspended", "stop_info",
                  "last_top", "last_func", "last_changed", "last_removed",
                  "last_changed_complete", "last_change_tracking",
                  "last_track_warn", "last_track_complete",
                  "last_track_reason", "stop_states",
                  "_hitkeys", "_awaiting_continued", "_suspects", "_co_seen",
                  "_suspect_warned"):
            dst_v, src_v = getattr(dst, f), getattr(src, f)
            setattr(dst, f, src_v)
            setattr(src, f, dst_v)

    # -- lifecycle mutations (caller holds _gate) --

    def note_exit(self, tid, last_stop=None):
        """Move a live child to the bounded exited history (never reused).
        The child's socket is closed here: only fully-established sessions
        (attach drained) ever reach the table, and closing those is
        adapter-contained; half-built sessions are never closed — they are
        abandoned open (closing those kills the adapter process)."""
        t = self.table.pop(tid, None)
        if t is None:
            return None
        try:
            self.order.remove(tid)
        except ValueError:
            pass
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
        self.exited_history.append(entry)
        while len(self.exited_history) > MAX_EXITED_HISTORY:
            del self.exited_history[0]
            self.dropped_exited += 1
        return entry

    def mark_ignored(self, tid, pid, why):
        """Release without a connection: socket-less roster record (never
        parked, never served), lifetime counter, bounded retention."""
        child = ChildTarget(tid, pid, None, None,
                            {"pid": pid, "source": "debugpy-subProcessId"})
        child.exited = True
        child.state = "ignored"
        self.table[tid] = child
        self.order.append(tid)
        self.ignored += 1
        self.evict_old_ignored()
        sys.stderr.write(f"warn: child {tid} {why}: released\n")

    def evict_old_ignored(self):
        """Bound retained ignored entries (counters stay lifetime)."""
        dropped = [tid for tid in self.order
                   if (t := self.table.get(tid)) is not None
                   and t.state == "ignored"]
        while len(dropped) > MAX_IGNORED_RETAINED:
            old = dropped.pop(0)
            self.table.pop(old, None)
            try:
                self.order.remove(old)
            except ValueError:
                pass

    def release_helper(self):
        self.helpers_released += 1

    # -- in-flight serving target (caller holds _gate; _TargetScope is the
    # sole production writer) --

    def set_serving(self, tid):
        """Mark the in-flight command target (caller holds _gate).

        Bounded: tid must be "main" or a registered table key. Unknown ids
        keep the existing BridgeErr vocabulary and are normally rejected by
        resolve_inner before any scope is entered; _TargetScope additionally
        looks the child up first, so scope entry itself keeps raising the
        existing KeyError (the bare-dump churn path skips exactly that).
        This guard is the backstop so a direct write can never park serving
        on a ghost."""
        if tid != "main" and tid not in self.table:
            raise BridgeErr(f"unknown target: {tid}")
        self.serving = tid

    def reset_serving(self):
        """Restore main after a command (caller holds _gate).

        Unconditional by design: a child removed mid-scope (its exit
        already snapshotted history) must still reset main without
        requiring the child to still be registered."""
        self.serving = "main"

    # -- deferred child admission (caller holds _gate except the blocking
    # private handshake between claim and commit, which touches only the
    # private DapConn) --

    def stage_attach(self, cfg, body):
        """Fast admission: dedup + bounded enqueue, never IO. Queue overflow
        marks the child ignored BEFORE any socket opens (same
        bounded-ownership law as the retired-full path)."""
        if not cfg.subprocess or cfg.kind != "launch":
            return
        if not isinstance(body, dict):
            return
        pid = body.get("subProcessId")
        if isinstance(pid, bool) or not isinstance(pid, int):
            return
        tid = f"child:{pid}"
        if tid in self.seen_ids:
            return  # ids are never reused within a session
        self.seen_ids.add(tid)
        if len(self.attach_pending) >= MAX_PENDING_ATTACH:
            self.mark_ignored(tid, pid, "attach queue full")
            return
        self.attach_pending.append((tid, pid, dict(body)))

    def claim_drain(self):
        """Single in-flight drainer claim (whichever pump staged/claimed —
        no new threads)."""
        if self.attach_busy or not self.attach_pending:
            return False
        self.attach_busy = True
        return True

    def pop_staged(self):
        if not self.attach_pending:
            return None
        return self.attach_pending.pop(0)

    def finish_drain(self):
        self.attach_busy = False

    def retired_full(self):
        return len(self.retired) >= MAX_RETIRED_SOCKETS

    def retire_socket(self, tid, sock):
        self.retired.append((tid, sock))

    def drain_retired(self):
        """Take (and clear) retired sockets for teardown-time close only."""
        out = self.retired
        self.retired = []
        return out

    def pop_all(self):
        """Take every roster entry for teardown (clears table + order)."""
        out = [(tid, self.table.pop(tid)) for tid in list(self.order)
               if tid in self.table]
        self.order = [tid for tid in self.order if tid in self.table]
        return out

    def record_failed_handshake(self, tid, pid, observed):
        """Bounded history for a handshake that never reached the roster
        (socket already retired OPEN by the caller)."""
        self.exited_history.append(
            {"id": tid, "kind": "child", "pid": pid,
             "state": "exited", "lastStop": None,
             "observed": observed, "scope": "inherited"})
        while len(self.exited_history) > MAX_EXITED_HISTORY:
            del self.exited_history[0]
            self.dropped_exited += 1

    def commit_child(self, child, sock):
        """Commit an established child (caller holds _gate). Returns
        "attached", or closes the established session (adapter-contained)
        and releases it when the claim-time budget no longer holds."""
        if child.id in self.table \
                or len(self.active_nonmain()) >= MAX_ACTIVE_NONMAIN:
            # Defensive: the claim-time budget should still hold (only
            # this drainer commits), but if it does not, the session is
            # established, so an adapter-contained close + release keeps
            # every bound exact.
            try:
                sock.close()
            except Exception:
                pass
            self.mark_ignored(child.id, child.pid, "over budget")
            return "ignored"
        child.state = "running"
        self.table[child.id] = child
        self.order.append(child.id)
        return "attached"

    def assert_valid(self):
        """Roster invariant: order ⊆ table; serving known; history bounded;
        active roster within budget+ignored retention."""
        assert all(tid in self.table for tid in self.order), \
            "roster order references missing table entry"
        assert len(self.table) == len(set(self.table)), \
            "duplicate roster ids"
        assert self.serving == "main" or self.serving in self.table, \
            f"serving unknown target: {self.serving}"
        assert len(self.exited_history) <= MAX_EXITED_HISTORY, \
            "exited history over bound"
        assert self.dropped_exited >= 0 and self.ignored >= 0 \
            and self.helpers_released >= 0, "negative lifetime counter"
        assert len(self.attach_pending) <= MAX_PENDING_ATTACH, \
            "attach staging over bound"
        assert len(self.retired) <= MAX_RETIRED_SOCKETS, \
            "retired sockets over bound"
        return True


class ServerState:
    """Canonical owner of framed-server admission + terminal close.

    Holds: the live connection-handler count (bounded by
    MAX_ACTIVE_HANDLERS) and the closing flag (terminal close accepted:
    only further closes served). The overload single-winner rule lives
    here: exactly one closer runs the teardown. No transport/protocol
    extraction — just the counters. Lock precondition: caller must hold
    Session._gate. Invariant: 0 <= active <= MAX_ACTIVE_HANDLERS.
    """

    def __init__(self):
        self.active = 0       # live connection handlers (bounded)
        self.closing = False  # close accepted: only further closes served

    def try_admit(self, limit):
        """Admit one handler when under the pool bound (caller holds _gate)."""
        if self.active >= limit:
            return False
        self.active += 1
        return True

    def release(self):
        """Drop one handler; never negative (close-cleanup-once backstop).

        Loud on unbalanced use like the Node/Browser owners: production
        pairs every release with a successful try_admit (serve admits,
        _handle_one's finally releases), so a zero-active release is a
        bug, never a slow close. Unconditional if/raise (never a bare
        assert: -O strips asserts and would mask drift below zero)."""
        if self.active <= 0:
            raise AssertionError(
                "ServerState invariant: release without acquire")
        self.active -= 1

    def claim_close(self):
        """Terminal-close single winner: first caller runs the teardown."""
        mine = not self.closing
        self.closing = True
        return mine

    def mark_closing(self):
        self.closing = True

    def is_closing(self):
        return self.closing

    def assert_valid(self):
        assert 0 <= self.active <= MAX_ACTIVE_HANDLERS, \
            "handler count outside pool bound"
        return True


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
        self.last_removed = "[]"
        self.last_changed_complete = False
        self.last_change_tracking = {"complete": False, "scanned": 0,
                                      "total": None, "truncated": False,
                                      "reason": "first-snapshot"}
        self.last_track_warn = None
        self.last_track_complete = False  # was previous scan exhaustive
        self.last_track_reason = "first-snapshot"  # reason when incomplete
        self.stop_info = None
        self.output_tail = ""
        self.log_count = 0
        self.log_dropped = 0  # lifetime lines evicted by the log ring
        self.configured = False  # True once launch/attach handshake completes
        # -- breakpoint transaction (Session-owned section, NOT an owner
        # object: these lists ARE the per-target swapped context — the
        # global pair lives here, each child's pair on its ChildTarget, and
        # TargetRegistry.swap_fields exchanges them. A wrapper only added
        # bypass; see architecture-map §6).
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
        # -- stop/wait/capture attribution (Session-owned section, NOT an
        # owner object: the park clock/diag/pending/outstanding ride the
        # swapped context + thread-local pump attribution, so a wrapper only
        # added bypass; see architecture-map §6).
        self._stop_seq = 0       # monotonic park clock for auto-select
        self._main_seq = 0       # seq of the main park (0 = never parked)
        # Per-target park clock for the waiter handoff (M2): target id ->
        # _stop_seq of its latest park, stamped in _park_stop alongside
        # _stop_seq/_last_park_target. A park consumed by a rival pump
        # (idle or concurrent waiter) still satisfies the owning waiter via
        # _shared_park_hit; entries are bounded by lifetime target count.
        self._park_seq = {}
        self._pending_target = None  # resume-wait owner for error attribution
        # -- M3 narrow owners (retained): TargetRegistry owns the roster
        # lifecycle (table/order/seen/history/counters/serving/staging/
        # retired) + resolve/swap/note/commit; ServerState owns the handler
        # pool + terminal-close winner. Lock precondition for owner
        # mutation: hold self._gate (order _gate -> DapConn.mu, never
        # reverse; no new locks).
        self.targets_reg = TargetRegistry()
        self.server_state = ServerState()
        # Same-object roster views for established tests/readers (no
        # duplicate source: these ARE the registry's containers; item-level
        # use only, never wholesale reassign — scalars/counters/serving stay
        # on the registry and are read as targets_reg.X).
        self.targets = self.targets_reg.table
        self.target_order = self.targets_reg.order
        self._seen_ids = self.targets_reg.seen_ids
        self.exited_targets = self.targets_reg.exited_history
        self._attach_pending = self.targets_reg.attach_pending
        # -- M5 concurrency: _gate serializes swapped-field sections and
        # event-dispatch mutations (never held across select/pump waits, so
        # live snapshot reads stay prompt). Per-connection DAP IO serializes
        # on each DapConn.mu; lock order is always _gate -> mu.
        self._gate = threading.RLock()
        self._outstanding = {}   # tid -> resume cmd (continue/step) in flight
        # -- bounded stall forensics (stderr only): each CLI handler
        # registers while in flight and serve() beats every loop; on the
        # first stall past STALL_DUMP_S a daemon dumps every thread's
        # Python stack once. bridge.log ships with live failure bundles,
        # so a hung bridge carries its own evidence.
        self._stall_lock = threading.Lock()
        self._stall_handlers = {}  # token -> (label, started_monotonic)
        self._stall_dumped = set()
        self._accept_beat = time.monotonic()
        self._park_local = threading.local()  # per-pump parked target id
        # -- stop diagnostics (UX batch): session-monotonic stop id plus the
        # previous park for same-location/same-thread diagnosis. Globals (not
        # swapped per target): every park carries its own target stamp.
        self._stop_diag_seq = 0    # session-monotonic stop id
        self._prev_park = None     # previous park {target,file,line,threadId,atMs}
        self._stop_reason = None   # reason of the current park
        self._parked_at_ms = 0     # wall clock ms of the current park
        self._last_diag = None     # {target,stopId,sameLocation,sameThread,elapsedMs}
        # -- layered target identity (M-ID): DAP `process` event (debuggee,
        # protocol-confirmed) + OS-observed listener owner (endpoint/adapter,
        # os-corroborated). Built at handshake, refreshed if the event lands
        # late; published redacted + capped in session.json.
        self._process_event = None   # {"pid","name","startMethod","isLocal"} or None
        self._target_identity = None  # {"debuggee","endpoint","adapter"} or None
        self._identity_hint = ""     # debuggee-first one-liner for timeouts

    def assert_owners(self):
        """Aggregate retained-owner invariants (tests; not a production scan)."""
        self.targets_reg.assert_valid()
        self.server_state.assert_valid()
        return True

    # (M3 narrow: no compatibility properties — roster views live in
    # __init__ as same-object aliases; scalars/counters/serving route
    # through targets_reg / server_state directly.)

    # === Section: target registry + swap (owner: TargetRegistry) ===
    # Roster/history/admission/retired lifecycle + resolve/swap entry points
    # below route into Session.targets_reg. Same-object views (targets,
    # target_order, _seen_ids, exited_targets, _attach_pending) exist for
    # established readers; scalars/counters/serving use targets_reg.X.
    # Routing rule (grep-enforced via scripts/check_pybridge_owners.sh,
    # wired into scripts/run_gates.sh --unit): production code mutates
    # owner state only through owner methods — never direct writes through
    # the views above, never targets_reg.serving / server_state.active /
    # closing writes, never a wholesale owner replacement.
    # -- multi-target helpers (canonical state: TargetRegistry) --

    def live_targets(self):
        """Live (non-exited) children in creation order."""
        return self.targets_reg.live_targets()

    def active_nonmain(self):
        """Budgeted children: live, tracked (ignored releases stay in the
        table with state ignored and never consume budget; helpers never
        enter the table at all)."""
        return self.targets_reg.active_nonmain()

    def resolve_target(self, req):
        """Which target a command serves: explicit id (validated), else the
        most recently stopped live target, else main. Acceptance-time
        auto-selection rides in req["_auto_target"] (stamped by dispatch)
        so concurrent handlers agree; it is still liveness-validated."""
        with self._gate:
            return self._resolve_target_inner(req)

    def _resolve_target_inner(self, req):
        # Canonical roster state lives in TargetRegistry; the main park
        # clock stays a plain Session field (swapped-context semantics).
        # This stays the orchestration entry (callers hold _gate via
        # resolve_target/dispatch).
        return self.targets_reg.resolve_inner(req, self.suspended,
                                              self.exited,
                                              self._main_seq)

    def _swap_fields(self, dst, src):
        """Exchange park/DAP fields between main (self) and a child
        (canonical op: TargetRegistry.swap_fields)."""
        TargetRegistry.swap_fields(dst, src)

    class _TargetScope:
        """Serve one command against a child using the main code paths.
        The scope holds Session._gate for its duration (M5: concurrent
        handlers must never swap shared park fields under each other);
        DAP IO inside takes DapConn.mu (order _gate -> mu, never reverse).
        The child ref is captured on entry so exit ALWAYS restores the
        main context — even when the child was removed mid-command (its
        exit already snapshotted history; the removal stands, the child is
        never resurrected)."""

        def __init__(self, session, tid):
            self.session = session
            self.tid = tid
            self._child = None

        def __enter__(self):
            st = self.session
            st._gate.acquire()
            try:
                if self.tid != "main":
                    # Unknown ids raise the existing KeyError before any
                    # serving write (the bare-dump churn path relies on
                    # skipping exactly this); set_serving below is the
                    # bounded backstop and cannot fail after the lookup
                    # (both under the held _gate).
                    self._child = st.targets[self.tid]
                    st.targets_reg.set_serving(self.tid)
                    st._swap_fields(st, self._child)
                else:
                    st.targets_reg.set_serving("main")
            except Exception:
                st._gate.release()
                raise
            return self

        def __exit__(self, *exc):
            st = self.session
            try:
                if self._child is not None:
                    st._swap_fields(st, self._child)
                # Unconditional: a mid-scope removed child still resets
                # main (its removal stands, never resurrected).
                st.targets_reg.reset_serving()
            finally:
                st._gate.release()
            return False

    def stamp(self, resp, tid):
        """Every served response names its target (no silent rerouting)."""
        if isinstance(resp, dict) and "target" not in resp:
            resp["target"] = tid
        return resp

    def target_entry(self, tid):
        """One roster entry {id,kind,pid,state,lastStop,scope} (+ `observed`
        only for child targets: their protocol facts). Main entries carry
        no `observed` — main identity lives in the top-level
        `targetIdentity`."""
        if tid == "main":
            return {"id": "main", "kind": "main", "pid": None,
                    "state": "exited" if self.main_exited
                    else ("stopped" if self.suspended else "running"),
                    "lastStop": self.last_stop,
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
                    "ignored": self.targets_reg.ignored,
                    "helpersReleased": self.targets_reg.helpers_released,
                    "droppedExited": self.targets_reg.dropped_exited,
                    "targetIdentity": (self._target_identity
                                       if isinstance(self._target_identity, dict)
                                       else None)}
            return self.stamp(resp, "main")

    def _note_exit(self, tid, last_stop=None):
        """Move a live child to the bounded exited history (canonical state:
        TargetRegistry.note_exit; socket/close semantics unchanged)."""
        with self._gate:
            entry = self.targets_reg.note_exit(tid, last_stop)
            # Park epochs are handoff metadata for live targets only. Drop
            # the child entry at retirement so the map stays bounded by the
            # live roster and an old epoch cannot satisfy a later waiter.
            self._park_seq.pop(tid, None)
            return entry

    def dap_request(self, command, args=None, timeout=30, semantic=False):
        try:
            return self.dap.request(command, args or {}, timeout, semantic)
        except BridgeErr:
            raise
        except (socket.timeout, TimeoutError):
            raise BridgeErr(f"DAP {command} timed out after {timeout:g}s")
        except Exception as e:
            raise BridgeErr(f"DAP {command} failed: {e}")

    def fetch_variables(self, ref):
        body = self.dap_request("variables", {"variablesReference": ref})
        if not isinstance(body, dict):
            return []
        vars_ = body.get("variables", [])
        return vars_ if isinstance(vars_, list) else []

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
                 if isinstance(c, dict)
                 and isinstance(c.get("name"), str)
                 and c.get("name") not in Session.PSEUDO_SCOPES
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
        # Malformed scopes (non-dict body, non-list, non-dict entries)
        # degrade to no locals — never AttributeError on the park path.
        raw_scopes = body.get("scopes", []) if isinstance(body, dict) else []
        if not isinstance(raw_scopes, list):
            raw_scopes = []
        scopes = {s.get("name"): s for s in raw_scopes
                  if isinstance(s, dict)}
        # Module frames keep names under Globals; functions under Locals.
        # Fall back only when Locals yields nothing real (never mask a
        # legitimately empty function frame with module noise... except that
        # an empty view helps nobody, so Globals still wins over nothing).
        children = []
        if "Locals" in scopes:
            children = [v for v in self.fetch_variables(scopes["Locals"]["variablesReference"])
                        if isinstance(v, dict)
                        and (not isinstance(v.get("name"), str)
                             or v.get("name") not in self.PSEUDO_SCOPES)]
        if not children and "Globals" in scopes:
            # Names hide one level deeper: function/class pseudo containers.
            merged = []
            for v in self.fetch_variables(scopes["Globals"]["variablesReference"]):
                if not isinstance(v, dict):
                    continue
                name = v.get("name", "")
                if name == "special variables":
                    continue
                # Set membership needs a hashable name: only real strings
                # can be pseudo-scopes; anything else flows through (the
                # dunder filter and track_changes skip non-strings
                # downstream) instead of raising TypeError here.
                is_pseudo = isinstance(name, str) and name in self.PSEUDO_SCOPES
                if is_pseudo and v.get("variablesReference"):
                    try:
                        merged.extend(
                            c for c in self.fetch_variables(v["variablesReference"])
                            if isinstance(c, dict)
                            and (not isinstance(c.get("name"), str)
                                 or c.get("name") not in self.PSEUDO_SCOPES))
                    except BridgeErr:
                        pass
                elif not is_pseudo:
                    merged.append(v)
            # Malformed nested records (non-string names) are kept, never
            # crashed on: the dunder filter only applies to real strings,
            # and track_changes skips non-string names downstream.
            children = [v for v in merged
                        if not (isinstance(v.get("name"), str)
                                and v.get("name", "").startswith("__"))]
        out = []
        for var in children:
            if not isinstance(var, dict):
                continue
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

    def _track_preview(self, value):
        """Shallow tracking preview: raw value string capped at
        TRACK_PREVIEW chars. Never fetches children, never invokes."""
        try:
            s = value if isinstance(value, str) else str(value)
        except Exception:
            s = "?"
        if len(s) > TRACK_PREVIEW:
            s = f"{s[:TRACK_PREVIEW]}… (+{len(s) - TRACK_PREVIEW} more chars)"
        return s

    @staticmethod
    def _track_func_id(frame):
        """Frame identity for change comparison: source path + function
        name. Same-named functions in different files must never compare
        silently (the stored baseline belongs to one frame identity).
        Degrades gracefully when the frame or its source is missing."""
        try:
            name = frame.get("name", "?") if isinstance(frame, dict) else "?"
        except Exception:
            name = "?"
        try:
            src = frame.get("source") if isinstance(frame, dict) else None
            path = src.get("path") if isinstance(src, dict) else None
        except Exception:
            path = None
        if not isinstance(name, str) or not name:
            name = "?"
        if isinstance(path, str) and path:
            return f"{path}#{name}"
        return name

    def _track_valid_names(self, raws):
        """Valid tracking records: dict, string name, non-sentinel,
        non-pseudo, carrying a value. Malformed records are skipped
        safely (never a park crash)."""
        names = {}
        for v in raws or []:
            try:
                if not isinstance(v, dict):
                    continue
                name = v.get("name")
                if (not isinstance(name, str) or name == "…"
                        or name in self.PSEUDO_SCOPES
                        or "value" not in v):
                    continue
                if name in names:
                    continue
                names[name] = self._track_preview(v.get("value"))
            except Exception:
                continue
        return names

    def _track_globals_raws(self, scopes):
        """One-level Globals expansion with the display's precedence:
        pseudo containers expand one level, `special variables` skipped,
        dunder names filtered. Raises BridgeErr on fetch failure (the
        scan degrades to tracking-error, never partial silence)."""
        ref = scopes["Globals"].get("variablesReference", 0)
        top = self.fetch_variables(ref)
        raws = []
        for v in top or []:
            if not isinstance(v, dict):
                continue
            name = v.get("name", "")
            if name == "special variables":
                continue
            is_pseudo = (isinstance(name, str)
                         and name in self.PSEUDO_SCOPES)
            if is_pseudo and v.get("variablesReference"):
                kids = self.fetch_variables(v["variablesReference"])
                raws.extend(
                    c for c in (kids or [])
                    if isinstance(c, dict)
                    and (not isinstance(c.get("name"), str)
                         or c.get("name") not in self.PSEUDO_SCOPES))
            elif not is_pseudo:
                raws.append(v)
        return [v for v in raws
                if not (isinstance(v.get("name"), str)
                        and v.get("name", "").startswith("__"))]

    def track_snapshot(self):
        """Display-independent tracking scan of frame 0.

        Mirrors the display scope precedence (Locals, falling back to
        Globals only when Locals yields zero valid tracking entries —
        empty, all pseudo, or all malformed) with the same one-level
        pseudo expansion, but scans up to CHANGE_TRACK_MAX valid
        top-level values with shallow previews — never the
        display-capped frame_locals path, never fmt_dap_value child
        fetches, never target eval. Only the chosen scope feeds the
        scan (no double counting); total/scanned describe that scope.

        Returns (ordered_map, total, truncated, reason_or_None). reason is
        None when the scan is exhaustive, else "truncated" (total over the
        cap) or "tracking-error". total is None when the scan itself
        failed (unknown, never 0). Malformed records are skipped safely.
        Raises nothing: fetch failures return ({}, None, False,
        "tracking-error") — callers still warn class-only."""
        valid = {}
        total = None
        truncated = False
        reason = None
        try:
            try:
                fid = self.top_frame_id(0)
            except BridgeErr:
                return {}, None, False, "tracking-error"
            body = self.dap_request("scopes", {"frameId": fid})
            raw_scopes = body.get("scopes", []) if isinstance(body, dict) else []
            if not isinstance(raw_scopes, list):
                raw_scopes = []
            scopes = {s.get("name"): s for s in raw_scopes
                      if isinstance(s, dict)}
            raws = None
            if "Locals" in scopes:
                try:
                    ref = scopes["Locals"].get("variablesReference", 0)
                    locals_raws = self.fetch_variables(ref)
                except BridgeErr:
                    return {}, None, False, "tracking-error"
                if self._track_valid_names(locals_raws):
                    raws = locals_raws
            if raws is None and "Globals" in scopes:
                try:
                    raws = self._track_globals_raws(scopes)
                except BridgeErr:
                    return {}, None, False, "tracking-error"
            if raws is None:
                raws = locals_raws if "Locals" in scopes else []
            # Valid records: dict, string name, non-sentinel, non-pseudo.
            # Raw DAP variables carry "value"; a missing value is skipped
            # (malformed) rather than crashing the park.
            names = self._track_valid_names(raws)
            total = len(names)
            truncated = total > CHANGE_TRACK_MAX
            if truncated:
                reason = "truncated"
            for name in sorted(names)[:CHANGE_TRACK_MAX]:
                valid[name] = names[name]
            return valid, total, truncated, reason
        except Exception:
            return {}, None, False, "tracking-error"

    def track_changes(self):
        # Display-independent change tracking (MAX_VARS display cap and its
        # value-less "…" sentinel never feed this path — the scanner above
        # reads raw DAP variables up to CHANGE_TRACK_MAX=256 with shallow
        # previews, one scope/variables fetch level, no child expansion).
        #
        # Contract (uniform on all adapters):
        # - changed: sorted value-changes + new names on complete scans.
        # - removed: sorted names missing from the current scan, complete
        #   scans only; never asserted under incomplete tracking.
        # - changedComplete: both previous and current scans exhaustive,
        #   same function identity, no tracking error.
        # - changeTracking {complete, scanned, total, truncated, reason?}:
        #   reason in first-snapshot|function-changed|truncated|
        #   tracking-error. Empty changed with complete=false is UNKNOWN
        #   (not "no change").
        # - First baseline / function change: changed=[] + complete=false
        #   (no previous snapshot means no change comparison; never report
        #   all locals as changed).
        # - Incomplete/error: only value changes over the name intersection
        #   of the scanned maps; added/removed suppressed; complete=false.
        # - A baseline stored while incomplete can never make the NEXT
        #   comparison complete; after a complete current baseline lands,
        #   the following stop can become complete.
        # Never raises: failures degrade to an empty baseline with a
        # class-only warning (no variable data/secrets).
        try:
            cur, total, truncated, scan_reason = self.track_snapshot()
        except Exception as e:
            self._track_degraded(type(e).__name__, {}, None, False,
                                 "tracking-error")
            return
        if scan_reason is not None and scan_reason != "truncated":
            # Fetch-level failure inside the scan already returned {}; the
            # warn below names the class only.
            self._track_compare({}, total, truncated, scan_reason,
                                degraded=True)
            return
        self._track_compare(cur, total, truncated, scan_reason,
                            degraded=False)

    def _track_warn(self, reason):
        try:
            sys.stderr.write(
                f"warn: change tracking degraded ({reason})\n")
        except Exception:
            pass
        return f"change tracking incomplete ({reason}); changed lists only certain value changes"

    def _track_degraded(self, reason, cur, total, truncated, track_reason):
        warn = self._track_warn(reason)
        self.last_top = cur
        self.last_func = self._track_func_id(
            self.frames[0] if self.frames else None)
        self.last_changed = "[]"
        self.last_removed = "[]"
        self.last_changed_complete = False
        self.last_track_complete = False
        self.last_track_reason = track_reason
        self.last_track_warn = warn
        scanned = min(total, CHANGE_TRACK_MAX) if isinstance(total, int) else 0
        tracking = {"complete": False, "scanned": scanned, "total": total,
                    "truncated": truncated, "reason": track_reason}
        self.last_change_tracking = tracking

    def _track_compare(self, cur, total, truncated, scan_reason, degraded):
        func = self._track_func_id(self.frames[0] if self.frames else None)
        last = self.last_top if isinstance(self.last_top, dict) else None
        prev_complete = bool(getattr(self, "last_track_complete", False))
        prev_reason = getattr(self, "last_track_reason", "first-snapshot")
        cur_complete = (scan_reason is None) and not degraded
        scanned = min(total, CHANGE_TRACK_MAX) if isinstance(total, int) else 0
        if last is None:
            # No previous snapshot means no change comparison. A problem
            # with the CURRENT scan (truncated/error) dominates the
            # reason — it describes the stored baseline the next stop
            # will compare against; first-snapshot only when the current
            # scan itself is exhaustive.
            reason = scan_reason if scan_reason is not None else (
                "first-snapshot" if cur_complete else "tracking-error")
            self._track_store(cur, func, [], [], False, scanned, total,
                              truncated, reason, cur_complete, scan_reason,
                              warn=True)
            return
        if getattr(self, "last_func", None) != func:
            reason = scan_reason if scan_reason is not None else (
                "function-changed" if cur_complete else "tracking-error")
            self._track_store(cur, func, [], [], False, scanned, total,
                              truncated, reason, cur_complete, scan_reason,
                              warn=True)
            return
        if not prev_complete or not cur_complete:
            # Intersection-only value changes; added/removed suppressed.
            try:
                changed = sorted(n for n, v in cur.items()
                                 if n in last and last.get(n) != v)
            except Exception:
                changed = []
            if not cur_complete:
                reason = scan_reason or "tracking-error"
            else:
                reason = prev_reason or "truncated"
            self._track_store(cur, func, changed, [], False, scanned, total,
                              truncated, reason, cur_complete, scan_reason,
                              warn=True)
            return
        try:
            changed = sorted(n for n, v in cur.items()
                             if not (n in last and last.get(n) == v))
            removed = sorted(n for n in last if n not in cur)
        except Exception as e:
            self._track_degraded(type(e).__name__, cur if isinstance(
                cur, dict) else {}, total, truncated, "tracking-error")
            return
        self._track_store(cur, func, changed, removed, True, scanned, total,
                          truncated, None, True, None, warn=False)

    def _track_store(self, cur, func, changed, removed, complete, scanned,
                     total, truncated, reason, cur_complete, scan_reason,
                     warn):
        self.last_top = cur if isinstance(cur, dict) else {}
        self.last_func = func
        try:
            self.last_changed = json.dumps(changed)
        except Exception:
            self.last_changed = "[]"
            changed = []
        try:
            self.last_removed = json.dumps(removed)
        except Exception:
            self.last_removed = "[]"
        self.last_changed_complete = bool(complete)
        self.last_track_complete = bool(cur_complete)
        self.last_track_reason = scan_reason if scan_reason is not None else (
            None if cur_complete else reason)
        # Any incomplete state carries the class-only warning (unknown,
        # never "no change"); complete scans stay quiet. Every incomplete
        # branch passes warn=True, so exactly one warning is written here.
        if warn:
            self.last_track_warn = self._track_warn(reason)
        else:
            self.last_track_warn = None
        tracking = {"complete": bool(complete), "scanned": scanned,
                    "total": total, "truncated": bool(truncated)}
        if reason is not None and not complete:
            tracking["reason"] = reason
        self.last_change_tracking = tracking

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
        """Timeout message with the compact identity hint (debuggee-first,
        names the target, never claims root cause)."""
        msg = f"timeout: no stop within {timeout:g}s"
        hint = self._identity_hint or seed_hint(self.cfg.target_identity_seed)
        if hint:
            msg += f"; {hint}"
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
        seed = seed_hint(self.cfg.target_identity_seed)
        return ("unresolved breakpoints: " + "; ".join(parts)
                + " — check the path names the executed file and the line "
                "is executable code (not a blank, comment, or def/class header)"
                + (f"; {seed}" if seed else ""))

    def publish_state(self, stopped, target=None):
        """Rewrite session.json so `status` shows live truth (parked stop +
        time) with zero prior memory. lastStop survives resume/exit — it
        answers 'where was I last', not 'where am I now'. The v2
        session.json carries schemaVersion 2 plus the layered
        targetIdentity (debuggee/endpoint/adapter, null until the handshake
        builds it). Child target parks/resumes never rewrite the
        main-focused file: the per-target last stop lives in the roster
        served by `targets`."""
        eff = target if target is not None else self.targets_reg.serving
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
             "schemaVersion": 2,
             "targetIdentity": self._target_identity}))

    # -- layered target identity (M-ID)

    def _note_process_event(self, body):
        """Adopt one DAP `process` event as the debuggee identity (first
        wins — the attach-time event names this session's debuggee). Never
        raises; a nameless/pid-less event is ignored (stays unavailable).
        Late arrivals rebuild + republish the identity (bounded enrichment,
        never an attach failure)."""
        try:
            if self._process_event is not None or not isinstance(body, dict):
                return
            pid = body.get("systemProcessId")
            name = body.get("name")
            if not isinstance(pid, int) and not isinstance(name, str):
                return
            self._process_event = {
                "pid": pid if isinstance(pid, int) else None,
                "name": name if isinstance(name, str) else None,
                "startMethod": body.get("startMethod") if isinstance(
                    body.get("startMethod"), str) else None,
                "isLocal": body.get("isLocalProcess"),
            }
            self._build_target_identity()
            self._store_target_identity()
        except Exception:
            pass

    def _consume_process_event(self, timeout=2.0):
        """Adopt the DAP `process` event around the handshake without losing
        anything: stash scan first, then a bounded read that re-stashes every
        non-process message for the pump. Never raises and never delays the
        first stop beyond the small bound; absence reads as unavailable."""
        try:
            for i, m in enumerate(list(self.dap.stash)):
                if _is_process_event(m):
                    del self.dap.stash[i]
                    self._note_process_event(m.get("body", {}))
                    return True
            deadline = time.time() + timeout
            while time.time() < deadline:
                self.dap.sock.settimeout(max(0.05, deadline - time.time()))
                try:
                    msg = self.dap._read_msg()
                except (socket.timeout, TimeoutError, BridgeErr):
                    return self._process_event is not None
                if _is_process_event(msg):
                    self._note_process_event(msg.get("body", {}))
                    return True
                self.dap.stash.append(msg)
            return self._process_event is not None
        except Exception:
            return self._process_event is not None

    def _debuggee_os_details(self, pid):
        """Best-effort OS argv/cwd/exe for a protocol-confirmed debuggee pid.
        The read itself is the liveness check (dead/reused-away pids fail
        here); failures return None. Redacted + capped; nested under
        os-corroborated source — the pid confidence is never upgraded."""
        if not isinstance(pid, int) or pid <= 0:
            return None
        argv, cwd, exe = None, None, None
        os_source = None
        try:
            with open(f"/proc/{pid}/cmdline", "rb") as f:
                parts = [b.decode("utf-8", "replace")
                         for b in f.read().split(b"\0") if b]
            argv = parts or None
            try:
                cwd = os.readlink(f"/proc/{pid}/cwd")
            except OSError:
                cwd = None
            try:
                exe = os.readlink(f"/proc/{pid}/exe")
            except OSError:
                exe = None
            os_source = "os-proc"
        except OSError:
            pass
        if argv is None:
            try:
                out = subprocess.run(
                    ["ps", "-p", str(pid), "-o", "args="],
                    capture_output=True, text=True, timeout=5).stdout.strip()
                if out:
                    argv = out.split()
                    os_source = "os-ps"
            except Exception:
                argv = None
        if argv is None and os.name == "nt":
            # No /proc and MSYS `ps` is blind to native pids: ask the OS
            # directly (stdlib only, no psutil). tasklist corroborates
            # liveness + image name; the full exe path comes from Win32.
            argv = _windows_process_argv(pid)
            if argv is not None:
                os_source = "os-tasklist"
        if exe is None and os.name == "nt":
            exe = _windows_process_image(pid)
            if exe is not None and os_source is None:
                os_source = "os-proc-win"
        if argv is None and cwd is None and exe is None:
            return None
        details = {"source": os_source or "os-proc",
                   "confidence": "os-corroborated",
                   "observedAt": int(time.time())}
        if argv is not None:
            details["argv"] = redact_identity_argv(argv)
        else:
            details["unavailable"] = [{"field": "argv",
                                       "reason": "unreadable process command line"}]
        if cwd is not None:
            details["cwd"] = cwd
        if exe is not None:
            details["executable"] = exe
        return details

    def _role_unavailable(self, reason):
        return {"confidence": "unavailable", "reason": reason,
                "observedAt": int(time.time()), "unavailable": []}

    def _build_target_identity(self):
        """Build the layered {debuggee, endpoint, adapter} identity from the
        DAP process event (protocol-confirmed debuggee) plus the CLI seed's
        layered endpoint role (os-corroborated listener owner). A
        malformed/missing seed degrades to all-unavailable, never a spawn
        failure. Everything is redacted + capped here, before any publish."""
        now = int(time.time())
        seed = (self.cfg.target_identity_seed
                if isinstance(self.cfg.target_identity_seed, dict) else {})
        ep = seed.get("endpoint") if isinstance(seed.get("endpoint"), dict) else {}
        obs = {
            "pid": ep.get("ownerPid"),
            "source": ep.get("source"),
            "argv": ep.get("argv"),
            "executable": ep.get("executable"),
            "cwd": ep.get("cwd"),
        }
        pid = obs.get("pid") if isinstance(obs.get("pid"), int) else None
        source = obs.get("source") if isinstance(obs.get("source"), str) else None
        # Re-redact here (idempotent over the CLI's copy): the bridge never
        # publishes a value it did not redact itself — a raw argv must never
        # leak via the endpoint role (e.g. --server-access-token).
        raw_argv = obs.get("argv") if isinstance(obs.get("argv"), list) else None
        obs_argv = redact_identity_argv(raw_argv) if raw_argv is not None else None
        # -- debuggee: only the DAP process event confirms it.
        pe = self._process_event
        if pe is not None and (pe.get("pid") is not None or pe.get("name") is not None):
            debuggee = {"kind": "process", "pid": pe.get("pid"),
                        "name": pe.get("name"),
                        "startMethod": pe.get("startMethod"),
                        "source": "dap-process-event",
                        "confidence": "protocol-confirmed",
                        "observedAt": now, "unavailable": []}
            if pe.get("pid") is None:
                debuggee["unavailable"] = [
                    {"field": "pid", "reason": "process event carries no systemProcessId"}]
            details = self._debuggee_os_details(pe.get("pid"))
            if details is not None:
                debuggee["osDetails"] = details
            else:
                debuggee["unavailable"].append(
                    {"field": "osDetails",
                     "reason": "debuggee pid not readable from this host"})
        else:
            debuggee = {"kind": "process", "pid": None, "name": None,
                        "startMethod": None, "source": None,
                        "confidence": "unavailable",
                        "observedAt": now,
                        "unavailable": [
                            {"field": "pid",
                             "reason": "no DAP process event observed yet"}]}
        # -- endpoint: the OS-observed listener owner (an adapter in attach,
        # the bridge-spawned adapter endpoint in launch) — never the debuggee.
        if self.cfg.kind == "launch":
            aport = getattr(self, "adapter_port", 0) or None
            apid = None
            try:
                apid = self.adapter.pid if self.adapter is not None else None
            except Exception:
                apid = None
            if not isinstance(apid, int):
                apid = None
            endpoint = {"host": "127.0.0.1", "port": aport,
                        "ownerPid": apid,
                        "role": "listener-owner (not necessarily the debuggee)",
                        "source": "bridge-spawn",
                        "confidence": "os-corroborated" if apid is not None
                                      else "unavailable",
                        "observedAt": now,
                        "unavailable": ([] if apid is not None else [
                            {"field": "ownerPid",
                             "reason": "adapter pid not reported"}])}
        else:
            endpoint = {"host": self.cfg.host, "port": self.cfg.port,
                        "ownerPid": pid,
                        "executable": obs.get("executable"),
                        "argv": obs_argv,
                        "cwd": obs.get("cwd"),
                        "role": "listener-owner (not necessarily the debuggee)",
                        "source": source,
                        "confidence": "os-corroborated" if pid is not None
                                      else "unavailable",
                        "observedAt": now,
                        "unavailable": ([] if pid is not None else [
                            {"field": "ownerPid",
                             "reason": "no independent pid source"}])}
        # -- adapter: the debugpy adapter process, recognized from the
        # redacted listener argv (attach) or our own spawn (launch).
        if self.cfg.kind == "launch":
            try:
                apid = self.adapter.pid if self.adapter is not None else None
            except Exception:
                apid = None
            if isinstance(apid, int):
                adapter = {"name": "debugpy-adapter", "pid": apid,
                           "source": "bridge-spawn",
                           "confidence": "os-corroborated",
                           "observedAt": now, "unavailable": []}
            else:
                adapter = self._role_unavailable("adapter pid not reported")
                adapter["name"] = "debugpy-adapter"
        else:
            recognized = any(isinstance(a, str) and "debugpy" in a
                             for a in (obs_argv or []))
            if recognized:
                adapter = {"name": "debugpy-adapter", "pid": pid,
                           "source": source,
                           "confidence": "os-corroborated" if pid is not None
                                         else "unavailable",
                           "observedAt": now,
                           "unavailable": ([] if pid is not None else [
                               {"field": "pid",
                                "reason": "no independent pid source"}])}
            else:
                adapter = self._role_unavailable(
                    "adapter not recognized from listener argv")
        ident = _cap_identity({"debuggee": debuggee, "endpoint": endpoint,
                               "adapter": adapter})
        self._target_identity = ident
        # Debuggee-first one-liner for timeout diagnostics (concise, no
        # root-cause claim); falls back to the CLI hint when unknown.
        hint = ""
        if isinstance(debuggee, dict) and debuggee.get("confidence") == "protocol-confirmed":
            nm = debuggee.get("name") or "?"
            if debuggee.get("pid") is not None:
                hint = f"debuggee: {nm} (pid {debuggee['pid']}, protocol-confirmed)"
            else:
                hint = f"debuggee: {nm} (protocol-confirmed)"
        if not hint:
            hint = seed_hint(self.cfg.target_identity_seed)
        self._identity_hint = hint[:200]
        return ident

    def _store_target_identity(self):
        """Republish session.json with the current layered identity (late
        process-event enrichment), preserving every other field. Atomic,
        best-effort: a missing/unparseable file simply skips."""
        try:
            path = os.path.join(self.cfg.dir, "session.json")
            try:
                with open(path) as f:
                    cur = json.load(f)
            except (OSError, ValueError):
                return
            if not isinstance(cur, dict):
                return
            cur["targetIdentity"] = self._target_identity
            write_file(path, json.dumps(cur))
        except Exception:
            pass

    def _wait_context(self, timeout, started_at, expected_break=None):
        """Honest timeout context: the debugger never observes the external
        trigger, so triggerStatus is always unknown; success paths never
        fabricate sent/failed. expectedBreak rides only when the capture
        planted one."""
        ctx = {"waitStartedAt": int(started_at),
               "waitedMs": max(0, int((time.time() - started_at) * 1000)),
               "triggerStatus": "unknown"}
        if expected_break is not None:
            ctx["expectedBreak"] = expected_break
        ctx["targetIdentity"] = (self._target_identity
                                 if isinstance(self._target_identity, dict) else None)
        ctx["note"] = WAIT_NOTE
        return ctx

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
        try:
            self.adapter = subprocess.Popen(
                [self.cfg.python, "-m", "debugpy.adapter",
                 "--host", "127.0.0.1", "--port", str(port)],
                stdout=log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL)
        finally:
            # The child holds its own dup of the fd; the parent side must
            # close (previously leaked one fd per session daemon).
            log.close()
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
        # Adopt the DAP process event when the adapter reports one (bounded,
        # nonfatal — absence never fails the launch), then build the layered
        # identity before the first user-visible completion.
        self._consume_process_event()
        self._build_target_identity()
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
        # The process event lands right after the attach response: adopt it
        # (bounded, nonfatal) so the debuggee role is protocol-confirmed
        # before the first user-visible attach completion. A delayed event
        # still enriches later via the pump path; absence stays unavailable.
        self._consume_process_event()
        self._build_target_identity()
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
        ownership, closed only at overall cleanup.
        The gate is held only for staging/claim/commit; the blocking
        private child handshake runs outside it (see _drain_attach_chain),
        so M5 live reads stay prompt through a ~70s handshake."""
        with self._gate:
            self._stage_attach(body)
        self._drain_attach_chain()

    def _stage_attach(self, body):
        """Fast admission (canonical state: TargetRegistry.stage_attach)."""
        self.targets_reg.stage_attach(self.cfg, body)

    def _drain_attach_chain(self):
        """Run staged handshakes with the gate RELEASED (single in-flight
        drainer: whichever pump staged/claimed — no new threads, no queues
        beyond the bounded staging list). Each claim/commit is a short gate
        section; the blocking private handshake between them is not. One
        reader per private DapConn holds: the drainer owns it until commit,
        since uncommitted sessions are invisible to _live_conns. Staging
        state is canonical in TargetRegistry; the phase snapshot comes from
        the inline claim-time tuple below."""
        with self._gate:
            if not self.targets_reg.claim_drain():
                return
        try:
            while True:
                with self._gate:
                    nxt = self.targets_reg.pop_staged()
                    if nxt is None:
                        return
                    tid, pid, body = nxt
                    if self.targets_reg.retired_full():
                        # Retired (failed-handshake) ownership is full and
                        # nothing may be closed mid-session: mark ignored
                        # BEFORE opening. Only reachable after 16
                        # consecutive handshake failures (adapter failure
                        # mode); documented residual hang risk there.
                        self.targets_reg.mark_ignored(
                            tid, pid, "retired ownership full")
                        continue
                    over = len(self.targets_reg.active_nonmain()) >= MAX_ACTIVE_NONMAIN
                    # Phase snapshot under lock: the arm below runs outside
                    # the gate against the private connection only, so it
                    # must not touch shared config/state afterwards.
                    snap = (list(self.cfg.breaks), list(self.cfg.logpoints),
                            list(self.cfg.methods), self.cfg.want_exc,
                            set(self.cfg.break_raws))
                self._run_attach("minimal" if over else "full",
                                 tid, pid, body, snap)
        finally:
            with self._gate:
                self.targets_reg.finish_drain()

    def _run_attach(self, kind, tid, pid, body, snap):
        """One child handshake outside _gate. Never raises (a backstop warn
        guards the pump); expected failures retire bounded history inside
        the kind-specific runners below."""
        try:
            if self._is_resource_tracker(pid):
                # Short-lived daemon, no user code: skip (no socket, no
                # budget). Its server connects before any session matches,
                # so it is never suspended pending configuration — it runs
                # free and exits on its own (every mp run verifies this).
                with self._gate:
                    self.targets_reg.release_helper()
                sys.stderr.write(
                    f"warn: released spawn helper for {tid} (no budget used)\n")
                return
            if kind == "minimal":
                self._run_minimal_attach(tid, pid, body)
            else:
                self._run_full_attach(tid, pid, body, snap)
        except Exception as e:
            sys.stderr.write(f"warn: child {tid} admission failed: {e}\n")

    def _arm_child_from_snap(self, dap, child, snap):
        """Plant the global intent as inherited copies on a private child
        connection OUTSIDE the gate: `snap` is the claim-time phase
        snapshot, so no shared roster/config/state is touched — only the
        private `dap` and the not-yet-shared `child` records. Same
        replace-per-file semantics as _arm_global."""
        breaks, logpoints, methods, want_exc, raw_keys = snap
        try:
            by_file = {}
            for path, line, cond in breaks:
                bp = {"line": line}
                if cond:
                    bp["condition"] = cond
                by_file.setdefault(path, []).append(
                    {"line": line, "bp": bp, "kind": "break", "cond": cond})
            for path, line, template in logpoints:
                by_file.setdefault(path, []).append(
                    {"line": line, "bp": {"line": line, "logMessage": template},
                     "kind": "logpoint", "template": template})
            for path, items in by_file.items():
                body = dap.request(
                    "setBreakpoints",
                    {"source": {"path": path},
                     "breakpoints": [it["bp"] for it in items]})
                got_list = body.get("breakpoints", [])
                for idx, item in enumerate(items):
                    got = got_list[idx] if idx < len(got_list) else {}
                    spec = f"{self.rel_file(path)}:{item['line']}"
                    if item["kind"] == "break" and item.get("cond"):
                        spec += f"|{item['cond']}"
                    rec = {"spec": spec, "kind": item["kind"],
                           "hits": 0 if item["kind"] == "break" else None}
                    if item["kind"] == "logpoint":
                        rec["detail"] = item["template"]
                    bound = self._apply_verification(
                        rec, got, item["line"], item["kind"] == "logpoint")
                    child.stop_states.append(rec)
                    child._hitkeys.append(
                        (item["kind"], path, item["line"], bound))
            if methods:
                body = dap.request(
                    "setFunctionBreakpoints",
                    {"breakpoints": [{"name": func} for func in methods]})
                got_list = body.get("breakpoints", [])
                for idx, func in enumerate(methods):
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
                    child.stop_states.append(rec)
                    child._hitkeys.append(("method", func))
            if want_exc:
                dap.request("setExceptionBreakpoints", {"filters": ["uncaught"]})
                child.stop_states.append(
                    {"spec": "exc", "kind": "exc", "state": "armed", "hits": 0})
                child._hitkeys.append(("exc",))
        except (socket.timeout, TimeoutError) as e:
            # DapConn lets raw timeouts escape (deadline semantics); outside
            # dap_request there is no wrapper, so convert here.
            raise BridgeErr(f"DAP child arm timed out: {e}")
        child.inherited_keys = set(raw_keys)

    def _run_full_attach(self, tid, pid, body, snap):
        """Full child handshake on a private connection (gate released):
        initialize → verbatim attach → inherited global copies →
        configurationDone → drained attach response. Commit (or bounded
        retire) happens under the gate; the socket is never closed here."""
        try:
            sock = socket.create_connection(
                ("127.0.0.1", self.adapter_port), timeout=10)
        except OSError as e:
            sys.stderr.write(f"warn: cannot reach child {pid}: {e}\n")
            return
        dap = DapConn(sock)
        child = ChildTarget(tid, pid, dap, sock,
                            {"pid": pid, "source": "debugpy-subProcessId"})
        child.logpoints = list(snap[1])
        configured = False
        try:
            dap.request("initialize", {"adapterID": "agent-debugger",
                                       "pathFormat": "path"}, timeout=30)
            # The attach response arrives only after configurationDone:
            # send without waiting (waiting here deadlocks, per M0).
            dap.send_only("attach", dict(body))
            self._arm_child_from_snap(dap, child, snap)
            dap.request("configurationDone", {}, timeout=30)
            configured = True
            self._drain_child_response(dap, "attach")
        except Exception as e:
            # Never strand a pre-configurationDone child (it would wait
            # forever): one configurationDone, then RETIRE the socket OPEN
            # (closing a half-built session kills the adapter process) and
            # record bounded history. The claim-time retired-full gate plus
            # the single drainer keeps a slot reserved for this commit.
            if not configured:
                try:
                    dap.request("configurationDone", {}, timeout=5)
                except Exception:
                    pass
            with self._gate:
                self.targets_reg.retire_socket(tid, sock)
            sys.stderr.write(f"warn: child {tid} handshake failed: {e}\n")
            with self._gate:
                self.targets_reg.record_failed_handshake(
                    tid, pid, child.observed)
            return
        with self._gate:
            outcome = self.targets_reg.commit_child(child, sock)
        if outcome == "attached":
            sys.stderr.write(f"target: {tid} attached (pid {pid})\n")

    def _run_minimal_attach(self, tid, pid, body):
        """Over-budget release outside _gate: full minimal handshake
        (initialize → verbatim attach → NO setBreakpoints →
        configurationDone → drained) on the private connection, then the
        established-session close plus a socket-less ignored record. The
        child is configured with zero breakpoints so it can never park —
        skipping the handshake instead would hang it (its server suspends
        until configured). ANY failure retires the socket OPEN (bounded)."""
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
        except Exception as e:
            if not configured:
                try:
                    dap.request("configurationDone", {}, timeout=5)
                except Exception:
                    pass
            with self._gate:
                self.targets_reg.retire_socket(tid, sock)
            sys.stderr.write(f"warn: child {tid} release failed: {e}\n")
            with self._gate:
                self.targets_reg.record_failed_handshake(
                    tid, pid, {"pid": pid, "source": "debugpy-subProcessId"})
            return
        try:
            sock.close()
        except Exception:
            pass
        with self._gate:
            self.targets_reg.mark_ignored(tid, pid, "over budget")

    def _mark_ignored(self, tid, pid, why):
        """Release without a connection (canonical state: TargetRegistry)."""
        self.targets_reg.mark_ignored(tid, pid, why)

    def _evict_old_ignored(self):
        """Bound retained ignored entries (canonical: TargetRegistry)."""
        self.targets_reg.evict_old_ignored()

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

    # === Section: breakpoint store + transaction (Session-owned) ===
    # No owner object here by design (M3 review): the records/keys ARE the
    # per-target swapped context (global pair on Session, copies on each
    # ChildTarget), so a wrapper only added bypass. Serialization stays
    # Session._gate + @_serialized_gate; DAP arming stays orchestration.
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
            # semantic=True: a valid adapter refusal here (bad condition/
            # source the resolver accepted) is a spec error, not a
            # connection loss. Socket/timeout/framing failures stay
            # transport regardless of this flag.
            body = self.dap_request("setBreakpoints",
                                    {"source": {"path": path},
                                     "breakpoints": [it["bp"] for it in items]},
                                    semantic=True)
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
                                 for func in self.cfg.methods]},
                semantic=True)
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
            self.dap_request("setExceptionBreakpoints", {"filters": ["uncaught"]},
                             semantic=True)
            self.stop_states.append(
                {"spec": "exc", "kind": "exc", "state": "armed", "hits": 0})
            self._hitkeys.append(("exc",))

    # === Section: stop/wait/capture machine (Session-owned) ===
    # No owner object here by design (M3 review): the park clock, diag,
    # pending/outstanding attribution ride the swapped context + the
    # thread-local pump id, so a wrapper only added bypass. Freshness gate,
    # timeout prefix, waitContext and captureStage schemas unchanged.
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
        entering here — lock order _gate -> mu. A debugpyAttach only
        STAGES here (bounded enqueue under gate); the drain below runs the
        blocking private child handshake with the gate RELEASED, so M5
        live reads never wait behind it."""
        with self._gate:
            if tid == "main":
                handled = self._handle_pumped(msg)
            else:
                handled = self._handle_pumped(msg, tid)
        # Gate released: run any staged child admission (fast no-op when
        # the staging list is empty).
        self._drain_attach_chain()
        return handled

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

    def _pump_hit_ok(self, want):
        """Whether the just-consumed park satisfies this waiter: omitted
        waits take any park; explicit waits (want pins one target id) take
        only want's own park. Other targets' parks stay parked (visible
        via context/roster) — just never returned here."""
        if want is None:
            return True
        return getattr(self._park_local, "parked", None) == want

    def _pump_start_seq(self):
        """Shared park clock at wait start: the epoch for the handoff
        below. Read under _gate, the same lock _park_stop stamps
        (_stop_seq, _park_seq, _last_park_target) under — so a wait never
        mistakes a pre-existing park for a fresh one. Callers capture this
        BEFORE issuing their resume (or before waiting, when no resume is
        issued): a park stamped between the resume and the capture is fresh
        for this wait and must satisfy it."""
        with self._gate:
            return self._stop_seq

    def _take_wait_epoch(self):
        """One-shot park epoch for pump(): a pre-resume capture stashed on
        this thread's thread-local by the resume caller (same thread runs
        caller then pump synchronously, so the stash is always ours),
        else a fresh entry capture — exact when no resume precedes the
        wait. Popped either way, so a later pump on this thread never
        reuses a stale epoch."""
        start_seq = getattr(self._park_local, "wait_start_seq", None)
        try:
            del self._park_local.wait_start_seq
        except AttributeError:
            pass
        if start_seq is None:
            return self._pump_start_seq()
        return start_seq

    def _shared_park_hit(self, start_seq, want):
        """Handoff for a park consumed by a RIVAL pump (idle or concurrent
        waiter): the consumer's thread-local took the arrival, but
        _park_stop published the park's epoch shared, so the owning waiter
        still observes it instead of running to StopTimeout. Same filter as
        _pump_hit_ok (omitted waits take any fresh park; explicit waits take
        only their own target's fresh park — per-target epochs, so a later
        foreign park never shadows an earlier matching one), and the hit is
        recorded on THIS thread's thread-local so attribution below reads
        our own stop. Other targets' parks stay parked (visible via
        context/roster, never consumed). Unknown/gone targets never hit."""
        with self._gate:
            if want is None:
                seq, target = self._stop_seq, self._last_park_target
            else:
                seq, target = self._park_seq.get(want, 0), want
            if target == "main":
                suspended = self.suspended
                target_exists = True
            else:
                parked_target = self.targets.get(target)
                suspended = parked_target is not None and parked_target.suspended
                target_exists = parked_target is not None
        if seq <= start_seq:
            return None
        if not target_exists:
            return None
        if not suspended:
            return None
        self._park_local.parked = target
        # Dispatch-result shape (like _dispatch_pumped's "stopped"): the
        # target rides on this thread's thread-local for attribution.
        return "stopped"

    def _pump_for(self, timeout, want):
        """Target-scoped pump entry: single-arg call for any-target waits
        (legacy fake_pump(timeout) doubles stay compatible), two-arg only
        when pinned to one target."""
        if want is None:
            return self.pump(timeout)
        return self.pump(timeout, want)

    def pump(self, timeout, want=None):
        """Wait for the next stopped/exited on ANY target (omitted), or only
        on want (explicit --target): stops on other targets are still
        consumed and parked (visible via context/roster, never discarded)
        but never returned — the waiter keeps waiting for want until the
        deadline. Returns the parking target id (also recorded in
        _last_park_target) or raises. Concurrent pumps (M5: independent
        resumes on different targets) share every connection: stash pops
        and wire reads are mu-protected, dispatch is gate-protected, and
        each message is consumed exactly once — and a park consumed by a
        rival pump still satisfies the owning waiter via the shared
        target+epoch handoff (_shared_park_hit below), so no waiter starves
        on another thread's consumption.
        Timeouts carry no context here: wait/capture attach their honest
        trigger-unknown context at their own call sites (continue/step
        timeouts stay bare). The wait's park epoch comes from
        _take_wait_epoch (a pre-resume capture stashed by the resume caller,
        else a fresh entry capture — exact when no resume precedes)."""
        deadline = time.time() + timeout
        start_seq = self._take_wait_epoch()
        for tid, conn, _sock in self._conn_entries():
            while True:
                msg = self._pop_stash(conn)
                if msg is None:
                    break
                r = self._dispatch_pumped(msg, tid)
                if r and self._pump_hit_ok(want):
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
                # at once. An explicit waiter only accepts its own target
                # here — another target's probe park stays parked while we
                # report the honest timeout.
                r = self._probe_all_suspects()
                if r and self._pump_hit_ok(want):
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
                    if r and self._pump_hit_ok(want):
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
                    if r and self._pump_hit_ok(want):
                        return r
            if progressed:
                continue  # re-check deadline/owner before selecting
            # Rival-consumed parks satisfy us here (shared target+epoch
            # handoff): our own drains found nothing, but another pump may
            # have parked our target since our epoch. Checked every tick
            # before blocking, so a handed-off stop waits at most one
            # select/sleep window.
            r = self._shared_park_hit(start_seq, want)
            if r:
                return r
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
                    # a resumed child), then keep waiting. An explicit
                    # waiter skips another target's probe park (it stays
                    # parked) and keeps waiting for its own.
                    r = self._probe_all_suspects()
                    if r and self._pump_hit_ok(want):
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
                if r and self._pump_hit_ok(want):
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
        eff = target if target is not None else self.targets_reg.serving
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
        # Change tracking must never crash the park (degraded track bug:
        # track_changes is total, but this guard is the backstop for any
        # future regression). Safe baseline + sanitized class-only
        # warning, then count_hits/publish/diag continue normally.
        try:
            self.track_changes()
        except Exception as e:
            self.last_top = {}
            self.last_func = self._track_func_id(
                self.frames[0] if self.frames else None)
            self.last_changed = "[]"
            self.last_removed = "[]"
            self.last_changed_complete = False
            self.last_track_complete = False
            self.last_track_reason = "tracking-error"
            self.last_change_tracking = {"complete": False, "scanned": 0,
                                          "total": None, "truncated": False,
                                          "reason": "tracking-error"}
            try:
                sys.stderr.write(
                    f"warn: change tracking degraded "
                    f"({type(e).__name__})\n")
            except Exception:
                pass
            self.last_track_warn = (
                "change tracking incomplete (tracking-error); "
                "changed lists only certain value changes")
        self.count_hits(reason)
        self.publish_state(True)
        self._park_local.parked = eff
        # Publish every shared park field atomically. `_park_stop` is usually
        # reached under `_dispatch_pumped`'s gate, but suspect fallback can
        # reach it from `pump` without that outer section. The RLock makes
        # both paths equivalent and keeps the lock order `_gate -> mu`; all
        # blocking DAP work above remains outside this short section.
        with self._gate:
            self._stop_seq += 1
            if eff == "main":
                self._main_seq = self._stop_seq
            else:
                t = self.targets.get(eff)
                if t is not None:
                    t.stop_seq = self._stop_seq
                    t.state = "stopped"
            self._last_park_target = eff
            self._park_seq[eff] = self._stop_seq
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
            # opt-in; attach sessions stay single-target. Staged only —
            # the dispatcher drains the blocking handshake outside _gate.
            if target == "main":
                self._stage_attach(body)
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
        if ev == "process":
            # Debuggee identity (DAP ProcessEvent): adopt protocol-confirmed
            # name/pid, never park, never disturb the pump or the child
            # debugpyAttach flow.
            self._note_process_event(body)
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
                "diag": self._stop_diag(self.targets_reg.serving, threads),
                "warning": PARK_WARNING}

    def cmd_stack(self):
        self.require_stopped()
        return {"ok": True, "frames": self.frames_json(False)}

    def _frame_index(self, req, what):
        """Uniform frame validation shared by vars/eval (same contract on
        all four bridges): absent/None reads as 0; an int (never bool) ≥ 0,
        an integer-valued finite float ≥ 0, or 1–15 ASCII digits read as the
        index. Malformed, fractional, negative, over-long, or mistyped
        input is `<what> needs integer frame` (a typed BridgeErr — never
        coerced to 0, never an internal ValueError). Range stays with
        frame_entry (`no frame N (have M)`); require_stopped still runs
        first at the call sites."""
        raw = req.get("frame", 0) if isinstance(req, dict) else 0
        if raw is None:
            return 0
        if isinstance(raw, bool):
            raise BridgeErr(f"{what} needs integer frame")
        if isinstance(raw, int):
            if raw < 0:
                raise BridgeErr(f"{what} needs integer frame")
            return raw
        if isinstance(raw, float):
            if not math.isfinite(raw) or not raw.is_integer() or raw < 0:
                raise BridgeErr(f"{what} needs integer frame")
            return int(raw)
        if isinstance(raw, str):
            if not raw or len(raw) > 15 or not raw.isascii() \
                    or not raw.isdigit():
                raise BridgeErr(f"{what} needs integer frame")
            return int(raw)
        raise BridgeErr(f"{what} needs integer frame")

    def cmd_vars(self, req):
        self.require_stopped()
        frame = self._frame_index(req, "vars")
        self.frame_entry(frame)  # bounds-check (widens fetch if needed)
        return {"ok": True, "frame": frame, "locals": self.frame_locals(frame)}
    def cmd_eval(self, req):
        self.require_stopped()
        expr = req.get("expr")
        if expr is None:
            raise BridgeErr("eval needs an expr")
        frame = self._frame_index(req, "eval")
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

    def _change_fields(self):
        """Additive change-tracking response fields (uniform contract).

        changed stays sorted/deterministic; removed is sorted (complete
        scans only, else []); changedComplete names certainty; empty
        changed with changedComplete=false is UNKNOWN, not no-change.
        trackingWarning rides only when incomplete (class-only, no
        values/secrets). Never raises."""
        try:
            changed = json.loads(self.last_changed)
            if not isinstance(changed, list):
                changed = []
        except Exception:
            changed = []
        try:
            removed = json.loads(getattr(self, "last_removed", "[]"))
            if not isinstance(removed, list):
                removed = []
        except Exception:
            removed = []
        complete = bool(getattr(self, "last_changed_complete", False))
        tracking = getattr(self, "last_change_tracking", None)
        if not isinstance(tracking, dict):
            tracking = {"complete": complete, "scanned": 0, "total": None,
                        "truncated": False}
        out = {"changed": changed, "removed": removed,
               "changedComplete": complete, "changeTracking": tracking}
        warn = getattr(self, "last_track_warn", None)
        if warn and not complete:
            out["trackingWarning"] = warn
        return out

    def _resume_and_wait(self, timeout, tid="main", explicit=False):
        """Resume one target after step/continue and wait for a selectable
        stop: only the resumed target's own fresh park when explicit, any
        fresh park when omitted (stale pre-existing parks never satisfy —
        the resume unparked our target first). The response names the
        target that actually parked (which may differ from the resumed one
        only for omitted waits). The park epoch was captured by the caller
        BEFORE the resume was issued (see cmd_step/cmd_continue) — a park
        stamped between the resume and the wait is fresh for this wait."""
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
            self._pump_for(timeout, tid if explicit else None)
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
                        "stopInfo": json.loads(self.stop_info or "null"),
                        "snapshot": snap,
                        "diag": self._stop_diag(stopped, snap.get("threads")),
                        "warning": PARK_WARNING}
                resp.update(self._change_fields())
            return self.stamp(resp, stopped)
        finally:
            self._pending_target = None

    def cmd_step(self, req, timeout):
        tid = self.resolve_target(req)
        # Epoch before the resume below (stashed thread-locally for the
        # wait): a stop parked between the resume issue and the wait is
        # fresh for this wait, never stale.
        self._park_local.wait_start_seq = self._pump_start_seq()
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
        explicit = isinstance(req, dict) and req.get("target") is not None
        return self._resume_and_wait(timeout, tid, explicit)

    def cmd_continue(self, req, timeout):
        tid = self.resolve_target(req)
        # Epoch before the resume below (same reason as cmd_step).
        self._park_local.wait_start_seq = self._pump_start_seq()
        with self._TargetScope(self, tid):
            self.require_live()
            if self.suspended:
                self.dap_request("continue", {"threadId": self.thread_id})
        explicit = isinstance(req, dict) and req.get("target") is not None
        return self._resume_and_wait(timeout, tid, explicit)

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
                "stopInfo": json.loads(self.stop_info or "null"),
                "snapshot": snap,
                "diag": self._stop_diag(tid, snap.get("threads")),
                "warning": PARK_WARNING}
        resp.update(self._change_fields())
        return self.stamp(resp, tid)

    def _parked_now(self):
        """True when the in-scope target is parked."""
        return bool(self.suspended)

    def cmd_wait(self, req, timeout):
        """Pure long-poll: NEVER resumes. Immediate success when the
        selected target is already parked; otherwise pump for the next
        fresh stop (any target when omitted — the response stamps the
        actual one; only the requested target when explicit). Timeout
        preserves session/intents (typed message)."""
        tid = self.resolve_target(req)
        explicit = isinstance(req, dict) and req.get("target") is not None
        with self._TargetScope(self, tid):
            self.require_live()
            if self._parked_now():
                return self._wait_snapshot(tid, False)
        # Not parked: wait without issuing any resume. The honest timeout
        # context attaches here (wait only — continue/step timeouts stay
        # bare); a stubbed pump's bare StopTimeout is enriched, never
        # replaced, so the typed prefix is unchanged either way. No resume
        # precedes the wait: stash the entry epoch for the pump.
        self._park_local.wait_start_seq = self._pump_start_seq()
        self._park_local.parked = None
        started = time.time()
        try:
            self._pump_for(timeout, tid if explicit else None)
        except StopTimeout as e:
            if e.wait_context is None:
                e.wait_context = self._wait_context(timeout, started)
            raise
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

    def _snap_vars_truncated(self, snap):
        """True when the capture snapshot capped frame-0 vars: frame_locals
        marks the cap with a trailing {"name": "…", "note": "+N more"}
        sentinel (never a real variable name)."""
        try:
            frames = snap.get("frames") or []
            locs = frames[0].get("locals", []) if frames else []
        except (AttributeError, IndexError):
            return False
        return any(isinstance(v, dict) and v.get("name") == "…"
                   and "note" in v for v in locs)

    def cmd_capture(self, req, timeout):
        """One-shot bounded stop. Pre-parked target: collect WITHOUT
        resuming. Fresh park: collect, REMOVE EPHEMERAL BEFORE RESUME,
        auto-resume within the pause budget (overrun still resumes, then
        reports). Any collection/removal failure still resumes; timeout
        never resumes (nothing parked). No eval, no persisted vars."""
        frames_n, vars_n, budget, spec = self._capture_bounds(req)
        # Entry time for the early-stage contexts below (session-gone /
        # before-armed): same seconds+ms units as every wait context; the
        # wait never happened, so waitedMs is ~0 — never faked.
        entry = time.time()
        try:
            tid = self.resolve_target(req)
            with self._TargetScope(self, tid):
                self.require_live()
                prepark = self._parked_now()
        except BridgeErr as e:
            # Session already gone before the capture command arrived
            # (short-lived startup target): keep the truthful
            # session/target-exited message verbatim, but attach the
            # additive stage so the CLI never reports "no session".
            if "exited" in str(e).lower() and getattr(
                    e, "wait_context", None) is None:
                try:
                    e.wait_context = {
                        "waitStartedAt": int(entry),
                        "waitedMs": max(0, int((time.time() - entry) * 1000)),
                        "triggerStatus": "unknown",
                        "captureStage": "session-gone",
                        "ephemeralPlanted": False,
                        "targetIdentity": (
                            self._target_identity
                            if isinstance(self._target_identity, dict)
                            else None),
                        "note": WAIT_NOTE}
                    if spec is not None:
                        e.wait_context["expectedBreak"] = spec
                except Exception:
                    pass
            raise
        if prepark:
            with self._TargetScope(self, tid):
                snap = self._bounded_snapshot(frames_n, vars_n)
                resp = {"ok": True, "stopped": True,
                        "targetWasPaused": True, "resumed": False,
                        "pauseDurationMs": 0, "pauseBudgetMs": budget,
                        "ephemeralPlanted": False,
                        "truncated": {"frames": len(self.frames) > frames_n,
                                      "vars": self._snap_vars_truncated(snap)},
                        "snapshot": snap,
                        "diag": self._stop_diag(tid, snap.get("threads")),
                        "warning": PARK_WARNING}
                return self.stamp(resp, tid)
        # Fresh path: plant the ephemeral first (failure here parks
        # nothing, so no resume is owed). A plant failure from a dying
        # session (exit/close mid-plant) means the target exited BEFORE
        # the ephemeral was armed — wrapped truthfully below. Invalid
        # specs and conflicting conditions raise verbatim (no stage
        # rewrite: the target did not exit).
        token = None
        if spec is not None:
            try:
                token = self._capture_plant(tid, spec)
            except BridgeErr as e:
                if ("exited" in str(e).lower() or "closed" in str(e).lower()) \
                        and getattr(e, "wait_context", None) is None:
                    wrapped = BridgeErr(
                        f"capture target exited before ephemeral "
                        f"breakpoint was armed: {e}")
                    try:
                        wrapped.wait_context = {
                            "waitStartedAt": int(entry),
                            "waitedMs": max(0, int((time.time() - entry) * 1000)),
                            "triggerStatus": "unknown",
                            "captureStage": "before-armed",
                            "ephemeralPlanted": False,
                            "expectedBreak": spec,
                            "targetIdentity": (
                                self._target_identity
                                if isinstance(self._target_identity, dict)
                                else None),
                            "note": WAIT_NOTE}
                    except Exception:
                        pass
                    raise wrapped from e
                raise
        started = time.time()
        try:
            # No resume precedes this wait (the ephemeral only plants):
            # stash the entry epoch for the pump.
            self._park_local.wait_start_seq = self._pump_start_seq()
            self._park_local.parked = None
            want = tid if isinstance(req, dict) and req.get("target") is not None else None
            self._pump_for(timeout, want)
        except Exception as orig:
            # Timeout/exit: nothing parked by us — no resume — but the
            # ephemeral must not leak: remove best-effort, then re-raise
            # the ORIGINAL error with the removal failure attached (never
            # masked by it). No intent persistence: the ephemeral never
            # touched stops.json / the global intent. A removal failure
            # on a dead adapter must not mask the exit stage either: the
            # staged error carries the removal note with its wait_context.
            try:
                self._capture_unplant(token)
            except Exception as e:
                staged = self._stage_capture_exit(orig, token, spec, started)
                if staged is not None:
                    note = (f"{staged}; capture ephemeral remove failed: "
                            f"{e} (breaks remove --target {tid} to clear)")
                    err = BridgeErr(note)
                    ctx = getattr(staged, "wait_context", None)
                    if isinstance(ctx, dict):
                        err.wait_context = ctx
                    raise err from e
                raise BridgeErr(
                    f"{orig}; capture ephemeral remove failed: {e} "
                    f"(breaks remove --target {tid} to clear)") from e
            planted = (token is not None
                       and token[0] not in ("main-dup", "child-dup"))
            if isinstance(orig, StopTimeout):
                # Honest timeout context (canonical field order, planted
                # spec only): enrich a bare pump timeout, or rebuild an
                # attached one with the expectedBreak. Other errors pass
                # through untouched.
                old = orig.wait_context if isinstance(
                    getattr(orig, "wait_context", None), dict) else None
                if old is None:
                    orig.wait_context = self._wait_context(timeout, started, spec)
                elif spec is not None:
                    orig.wait_context = {
                        "waitStartedAt": old.get("waitStartedAt"),
                        "waitedMs": old.get("waitedMs"),
                        "triggerStatus": "unknown",
                        "expectedBreak": spec,
                        "targetIdentity": old.get("targetIdentity"),
                        "note": old.get("note", WAIT_NOTE)}
                if isinstance(getattr(orig, "wait_context", None), dict):
                    # Additive stage: reaching the pump means the ephemeral
                    # WAS armed (plant confirmed, or the idempotent dup
                    # where the line was already armed) and the stop
                    # simply never arrived — never an endpoint verdict,
                    # never "unreachable code".
                    orig.wait_context["captureStage"] = "armed-wait-timeout"
                    orig.wait_context["ephemeralPlanted"] = planted
            else:
                staged = self._stage_capture_exit(orig, token, spec, started)
                if staged is not None:
                    raise staged from orig
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
                                  "vars": self._snap_vars_truncated(snap)},
                    "snapshot": snap, "diag": diag,
                    "warning": PARK_WARNING}
            if snap_err is not None:
                resp["snapshotError"] = snap_err
            if remove_err is not None:
                resp["removeError"] = remove_err
            if resume_err is not None:
                resp["resumeError"] = resume_err
            return self.stamp(resp, parked)

    def _stage_capture_exit(self, orig, token, spec, started):
        """Stage a short-lived-target exit during a capture wait as the
        truthful armed-wait error (or None when orig is not an exit).
        Reaching the pump means the ephemeral WAS armed (plant confirmed,
        or the idempotent dup where the line was already armed): the stop
        never arrived in time. The message names the stage — never
        endpoint-rejected/unreachable, never "unreachable code" (an
        unobserved trigger proves nothing about reachability). Exits
        BEFORE arming wrap at the plant site (before-armed) or report
        session-gone at entry."""
        msg_lower = str(orig).lower() if isinstance(orig, BridgeErr) else ""
        if not ("exited" in msg_lower or "closed" in msg_lower):
            return None
        planted = (token is not None
                   and token[0] not in ("main-dup", "child-dup"))
        msg = (f"target exited before capture hit"
               + (f" ({spec})" if spec is not None else "")
               + f": {orig}")
        wrapped = BridgeErr(msg)
        try:
            wrapped.wait_context = {
                "waitStartedAt": int(started),
                "waitedMs": max(0, int((time.time() - started) * 1000)),
                "triggerStatus": "unknown",
                "captureStage": "armed-wait",
                "ephemeralPlanted": planted,
                "targetIdentity": (
                    self._target_identity
                    if isinstance(self._target_identity, dict)
                    else None),
                "note": WAIT_NOTE}
            if spec is not None:
                wrapped.wait_context["expectedBreak"] = spec
        except Exception:
            pass
        return wrapped

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

    def _threads_for(self, tid):
        """One target's live thread dump: (running, threads). Busy targets
        (a resume outstanding) serve the published running truth with zero
        DAP traffic — never waits, never opens a second DAP reader. A dead
        main fails fast with the typed main-exited error: its DAP session
        is gone (None) while children may live on, so reading threads
        through it would only propagate a DAP None failure."""
        if tid == "main" and self.main_exited:
            raise BridgeErr("target main has exited — close this session")
        with self._gate:
            busy = tid in self._outstanding
        if busy:
            # M5 live read: never waits behind the outstanding resume and
            # never opens a second DAP reader — serve the published running
            # truth with zero DAP traffic (no stale frames as current).
            return True, []
        with self._TargetScope(self, tid):
            if self.exited:
                raise BridgeErr("target VM has exited — close this session")
            return (not self.suspended), self.threads_dump()

    def cmd_threads(self, req=None):
        # Explicit --target X (including main) dumps that target only
        # (unchanged single-target shape; an exited main fails fast with
        # the typed main-exited error via _threads_for). Bare threads
        # aggregates every live non-ignored non-exited child in
        # targets/breaks order, with main first only while main itself is
        # live — a dead main is excluded up front so its gone DAP session
        # never fails the call and live children stay visible; exited
        # history is excluded. Top-level running/threads stay the
        # auto-selected target's dump and the per-target entries ride
        # additively under `targets` with a `selected` stamp, so
        # single-target sessions read byte-identical.
        req = req or {}
        if isinstance(req, dict) and req.get("target") is not None:
            tid = self.resolve_target(req)
            running, threads = self._threads_for(tid)
            return self.stamp({"ok": True, "running": running,
                               "threads": threads}, tid)
        with self._gate:
            selected = self._resolve_target_inner(req)
            if self.exited:
                raise BridgeErr("target VM has exited — close this session")
            roster = (([] if self.main_exited else ["main"])
                      + [t.id for t in self.active_nonmain()])
        if len(roster) == 1:
            tid = roster[0]
            running, threads = self._threads_for(tid)
            return self.stamp({"ok": True, "running": running,
                               "threads": threads}, tid)
        # A child that exits (or is released) between the roster snapshot
        # and its dump is skipped — never failing the whole call; the
        # survivors stay attributable. Anything else (e.g. a DAP read
        # failure) still propagates.
        entries = []
        for tid in roster:
            try:
                running, threads = self._threads_for(tid)
            except (KeyError, BridgeErr) as e:
                if tid != "main" and (isinstance(e, KeyError)
                                      or "has exited" in str(e)
                                      or "was released" in str(e)):
                    continue
                raise
            entries.append({"target": tid, "running": running,
                            "threads": threads})
        if not entries:
            # Everything churned: serve the selected target honestly
            # (raises).
            running, threads = self._threads_for(selected)
            return self.stamp({"ok": True, "running": running,
                               "threads": threads}, selected)
        sel = next((e for e in entries if e["target"] == selected), entries[0])
        return self.stamp({"ok": True, "running": sel["running"],
                           "threads": sel["threads"], "targets": entries,
                           "selected": sel["target"]}, sel["target"])

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

    @_serialized_gate
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
                        # Child-swapped context (inside _TargetScope): these
                        # are the target's own lists, not the global store.
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

    @_serialized_gate
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
                    # Child-swapped context (inside _TargetScope): the
                    # target's own lists plus its ephemeral raw map.
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
                    # Child-swapped context (inside _TargetScope): the
                    # target's own aligned pair, not the global store.
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

    @_serialized_gate
    def cmd_breaks_remove(self, req):
        """Remove live line breaks by stored identity (running or parked).
        Serialized on the reentrant gate with add/clear (same-file replace
        races converge: check+plant+state is atomic, no ghost/resurrection).
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
        resp, confirmed = self._drop_break_keys(matched, missing)
        # Propagate only confirmed main removals: a file whose backend
        # call failed keeps its intent AND its inherited copies (dropping
        # the copy while the intent stands would diverge bridge intent,
        # backend, and persistence).
        child_warnings = []
        for tid in self.target_order:
            t = self.targets.get(tid)
            if t is None or t.exited or t.state == "ignored" or t.dap is None:
                continue
            doomed = [k for k in confirmed if k in t.inherited_keys]
            if not doomed:
                continue
            try:
                self._drop_child_keys(tid, doomed, [])
            except BridgeErr as e:
                child_warnings.append(f"{tid}: {e}")
        if child_warnings:
            resp["warning"] = ((resp.get("warning", "") + "; ") if resp.get("warning") else "") + "; ".join(child_warnings)
        return self.stamp(resp, "main")

    @_serialized_gate
    def cmd_breaks_clear(self, req=None):
        """Drop all live line breaks (logpoints ride along untouched).
        Serialized on the reentrant gate with add/remove (same-file replace
        races converge, no ghost/resurrection).
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
        resp, confirmed = self._drop_break_keys(ordered, [])
        confirmed_set = set(confirmed)
        child_warnings = []
        for tid in self.target_order:
            t = self.targets.get(tid)
            if t is None or t.exited or t.state == "ignored" or t.dap is None:
                continue
            # Ephemeral records always reset; inherited copies only for
            # confirmed main removals (a failed file keeps its intent and
            # its copies).
            doomed = list(dict.fromkeys(
                list(t.target_raws.keys())
                + [k for k in t.inherited_keys if k in confirmed_set]))
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
        confirmed files mutate config/state/raws. Returns (resp, confirmed)
        where confirmed lists exactly the canonical keys removed — callers
        propagate inherited copies for confirmed keys only (never the
        requested set)."""
        by_file = {}
        for key in keys:
            by_file.setdefault(key[0], []).append(key)
        removed = []
        failed = []
        confirmed = []
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
                confirmed.append(key)
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
        return resp, confirmed

    def _remove_spec(self, key):
        """Display spec for a stored key (relative path + cond)."""
        path, line, cond = key[0], key[1], key[2] if len(key) > 2 else None
        spec = f"{self.rel_file(path)}:{line}"
        if cond:
            spec += f"|{cond}"
        return spec

    def cmd_logs(self, req):
        try:
            tail = max(1, min(500, int(req.get("tail", 50))))
        except (TypeError, ValueError):
            # Non-numeric tail degrades to the default (Node/Browser/JS
            # parity) instead of surfacing as an internal error.
            tail = 50
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
        if self.targets_reg.serving == "main" and self.main_exited:
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
        if self.targets_reg.serving == "main" and self.main_exited:
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
            if self.server_state.is_closing():
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

    # === Section: framed server + terminal close (owner: ServerState) ===
    # Handler pool + close-winner counters live in Session.server_state and
    # are touched only via try_admit/release/claim_close/is_closing
    # (production + tests; no raw bypass). MAX_THREADS=8 single _gate.
    def cleanup(self):
        # Retired (failed-handshake) sockets first: they are closed ONLY
        # here, at overall teardown — never mid-session (never-close-live
        # debugpy law). Then live table sockets (fully established, so
        # contained), then the parent terminate which reaps the tree.
        # Roster/retired state is canonical in TargetRegistry.
        for _tid, sock in self.targets_reg.drain_retired():
            try:
                if sock is not None:
                    sock.close()
            except Exception:
                pass
        # Child sessions next: raw close only, and only here at teardown.
        # (A DAP disconnect would finalize the shared adapter session
        # before the parent's terminate below; raw closes of
        # fully-established sessions are adapter-contained, while the
        # parent terminate reaps the whole tree on launch anyway.
        # drain_retired cleared the retired list above; pop_all clears the
        # roster — both canonical in TargetRegistry.)
        for _tid, t in self.targets_reg.pop_all():
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


# Setup-failure phases for error.json (additive; `error` text unchanged).
# "transport" = a connection/protocol loss (socket/connect/initialize/
# handshake loss, timeouts, target exit); "config" = a genuine semantic
# validation error (Usage/ConfigErr: bad method/class/line/source/
# condition, unknown args, or a valid protocol error response);
# "runtime" = an unexpected internal failure after a successful bridge
# operation (explicit RuntimeErr, or any other unexpected exception —
# never message-matched). The CLI routes on this type signal instead of
# matching message text: config keeps the semantic message with no
# endpoint diagnosis, runtime surfaces a truthful internal error with no
# endpoint diagnosis, transport keeps evidence-based endpoint diagnosis.
# Default is transport (unbound sessions, unexpected non-exception
# values); a successful connection never globally flips later failures
# to config.


def phase_of_error(e):
    """Validated setup phase for error.json `phase` — derived from the
    exception type, never from message text or a stage timer. Usage and
    ConfigErr read as config; RuntimeErr and any other unexpected
    exception read as runtime; BridgeErr transport losses (socket,
    timeout, target exit) read as transport."""
    try:
        if isinstance(e, (Usage, ConfigErr)):
            return "config"
        if isinstance(e, RuntimeErr):
            return "runtime"
        if isinstance(e, BridgeErr):
            return "transport"
        if isinstance(e, Exception):
            return "runtime"
    except Exception:
        pass
    return "transport"


def setup_error_payload(exc, message):
    """error.json body: schemaVersion 2, the message verbatim, plus the
    additive phase."""
    return {"schemaVersion": 2, "error": message, "phase": phase_of_error(exc)}


def seed_hint(seed):
    """One-line redacted hint derived from the layered CLI seed (debuggee
    launcher args, then endpoint listener details). Empty for a null seed;
    an unavailable note when the seed carries nothing nameable. Never
    raises; never a spawn failure."""
    try:
        if not isinstance(seed, dict):
            return ""
        for role in ("debuggee", "endpoint"):
            r = seed.get(role)
            if not isinstance(r, dict):
                continue
            exe = r.get("executable") if isinstance(r.get("executable"), str) else "?"
            argv = [a for a in (r.get("argv") or []) if isinstance(a, str)][:3]
            cwd = r.get("cwd") if isinstance(r.get("cwd"), str) else "?"
            if exe != "?" or argv:
                return f"target identity: {exe} {' '.join(argv)} (cwd {cwd})"[:200]
        return "target identity unavailable (no independent source)"
    except Exception:
        return ""


def dir_from_argv(argv):
    """--dir value from raw CLI args (parse_args failed, so scan manually)."""
    for i, a in enumerate(argv):
        if a == "--dir" and i + 1 < len(argv):
            return argv[i + 1]
        if a.startswith("--dir="):
            return a[len("--dir="):]
    return None


def write_parse_error(argv, message):
    """Best-effort error.json for CLI-arg failures: config by definition
    (no connection was attempted), so the CLI preserves the semantic
    message instead of diagnosing the endpoint."""
    try:
        d = dir_from_argv(argv)
        if d:
            write_file(os.path.join(d, "error.json"),
                       json.dumps({"schemaVersion": 2, "error": message, "phase": "config"}))
    except Exception:
        pass


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
                    if is_framing_error_text(str(e)):
                        # Malformed adapter frame: never target death.
                        # Warn and stop pumping; the next command's pump
                        # surfaces the same error to its caller instead of
                        # a fabricated session exit.
                        sys.stderr.write(f"dap: {e}\n")
                        break
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


def _error_target(st, req):
    """Request-local error attribution for one connection's envelope.

    Derived synchronously at frame-read time, before any concurrent await
    can mutate shared serving state: an explicit string target is echoed
    verbatim (even unknown — the envelope names what was asked); an
    omitted target uses this request's own resolution where available,
    else the legacy "main" default. Never another handler's shared
    _pending_target/_serving. Success stamps are untouched."""
    try:
        if isinstance(req, dict):
            raw = req.get("target")
            if isinstance(raw, str):
                return raw
            try:
                return st.resolve_target(req)
            except Exception:
                pass
    except Exception:
        pass
    return "main"


def _close_from_conn(st, conn):
    """Terminal close handling for one connection, outside the handler
    pool: every close gets the closed ACK; exactly one winner runs the
    teardown (launch kills its tree, attach detaches). The pool counter
    is untouched (the overload path never counted). Single-winner state
    is canonical in ServerState.claim_close (under Session._gate)."""
    with st._gate:
        mine = st.server_state.claim_close()
    try_write_frame(conn, {"ok": True, "closed": True, "target": "main"})
    if not mine:
        try:
            conn.close()
        except Exception:
            pass
        return
    try:
        st.cleanup()
    except Exception:
        pass
    try:
        st.server.close()
    except Exception:
        pass
    try:
        conn.close()
    except Exception:
        pass


def _handle_overload(st, conn):
    """Pool-full bypass: one bounded frame read under the existing framing
    limits/deadlines (never an unbounded wait). An exact `close` gets
    terminal close handling outside the pool — close can never be starved
    by admitted handlers. Anything else gets the existing overloaded
    rejection; malformed/timeout reads just close the socket."""
    try:
        req = read_frame(conn)
    except BridgeErr:
        try:
            conn.close()
        except Exception:
            pass
        return
    if isinstance(req, dict) and req.get("cmd") == "close":
        _close_from_conn(st, conn)
        return
    try_write_frame(conn, {"ok": False,
                           "error": "overloaded: too many active handlers",
                           "target": "main"})
    try:
        conn.close()
    except Exception:
        pass


# Stall forensics bounds: a CLI handler or the accept loop running/stalled
# past its threshold triggers ONE stack dump of every thread (stderr ->
# bridge.log, which ships with the live failure bundles). The poll period
# only bounds detection latency.
STALL_DUMP_S = 15.0
STALL_POLL_S = 5.0
# Commands whose handler legitimately waits for a target event, with the
# wait budget riding in the request body: their stall threshold becomes
# budget + margin (a prompt command keeps STALL_DUMP_S).
_STALL_WAIT_CMDS = {"continue", "step", "wait", "capture", "reload"}


def _stall_threshold_for(req):
    try:
        cmd = req.get("cmd")
    except AttributeError:
        return STALL_DUMP_S
    if cmd not in _STALL_WAIT_CMDS:
        return STALL_DUMP_S
    try:
        budget = float(req.get("timeout"))
    except (TypeError, ValueError):
        # Body-less wait commands fall back to the session cfg.timeout
        # server-side; 60s keeps that legitimate window quiet.
        budget = 60.0
    return max(STALL_DUMP_S, min(budget, 3600.0) + 10.0)


def _stall_begin(st, label, threshold=STALL_DUMP_S):
    try:
        token = object()
        with st._stall_lock:
            st._stall_handlers[token] = (label, time.monotonic(), threshold)
        return token
    except Exception:
        return None


def _stall_label(st, token, label, threshold=None):
    if token is None:
        return
    try:
        with st._stall_lock:
            entry = st._stall_handlers.get(token)
            if entry is not None:
                st._stall_handlers[token] = (
                    label, entry[1], entry[2] if threshold is None else threshold)
    except Exception:
        pass


def _stall_end(st, token):
    if token is None:
        return
    try:
        with st._stall_lock:
            st._stall_handlers.pop(token, None)
    except Exception:
        pass


def _stall_check(st, now=None, threshold=STALL_DUMP_S):
    """One-shot stall decision (pure lookup + dump bookkeeping): names
    handlers and the accept loop past their threshold. Each key reports
    once; the caller logs the reasons and dumps the stacks."""
    now = time.monotonic() if now is None else now
    reasons = []
    with st._stall_lock:
        for token, (label, started, thresh) in list(st._stall_handlers.items()):
            key = ("handler", id(token))
            if now - started > thresh and key not in st._stall_dumped:
                st._stall_dumped.add(key)
                reasons.append(f"{label} {now - started:.0f}s")
        if (now - st._accept_beat > threshold
                and "accept" not in st._stall_dumped):
            st._stall_dumped.add("accept")
            reasons.append(f"accept loop idle {now - st._accept_beat:.0f}s")
    return reasons


def _stall_watch_loop(st):
    while True:
        time.sleep(STALL_POLL_S)
        try:
            reasons = _stall_check(st)
            if reasons:
                sys.stderr.write(
                    "watchdog: stalled: " + "; ".join(reasons) + "\n")
                sys.stderr.flush()
                faulthandler.dump_traceback(file=sys.stderr, all_threads=True)
        except Exception:
            pass


def _start_stall_watchdog(st):
    threading.Thread(target=_stall_watch_loop, args=(st,),
                     name="stall-watchdog", daemon=True).start()


def _handle_one(st, conn):
    """Serve a single CLI connection on a handler thread (M5): exactly one
    request and one response per connection. A client disconnect never
    cancels target-side work — the resume still publishes state; only this
    connection's response is dropped (best-effort write)."""
    token = _stall_begin(st, "handler")
    try:
        try:
            req = read_frame(conn)
        except BridgeErr as e:
            try_write_frame(conn, {"ok": False, "error": str(e)})
            return
        _stall_label(st, token, f"cmd={req.get('cmd', '?')}",
                     threshold=_stall_threshold_for(req))
        err_target = _error_target(st, req)
        try:
            resp = st.dispatch(req)
            if isinstance(resp, dict) and "target" not in resp:
                resp["target"] = st.targets_reg.serving
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
            _close_from_conn(st, conn)
        except BridgeErr as e:
            resp = {"ok": False, "error": str(e), "target": err_target}
            wc = getattr(e, "wait_context", None)
            if isinstance(wc, dict):
                resp["waitContext"] = wc
            try_write_frame(conn, resp)
        except Exception as e:
            try_write_frame(conn, {"ok": False, "error": f"internal: {e}",
                                    "target": err_target})
    finally:
        try:
            conn.close()
        except Exception:
            pass
        with st._gate:
            st.server_state.release()
        _stall_end(st, token)


def serve(st, server, nonce):
    # M5: accept loop stays lean — each connection gets one bounded handler
    # thread (max MAX_ACTIVE_HANDLERS; overflow is an immediate rejection,
    # never an unbounded spawn). Idle windows pump only while NO resume is
    # outstanding: the outstanding resume's pump owns wire consumption, and
    # a second consumer would steal its stop.
    st.server = server
    st._accept_beat = time.monotonic()
    _start_stall_watchdog(st)
    try:
        server.settimeout(0.1)
    except OSError:
        pass
    while True:
        st._accept_beat = time.monotonic()
        # Abandoned (dir rm'd or respawned under our name)? Quit quietly.
        if not am_owner(st.cfg.dir, nonce):
            with st._gate:
                st.server_state.mark_closing()
            try:
                st.cleanup()
            except Exception:
                pass
            return
        with st._gate:
            if st.server_state.is_closing():
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
            if st.server_state.is_closing():
                try:
                    conn.close()
                except Exception:
                    pass
                return
            overloaded = not st.server_state.try_admit(MAX_ACTIVE_HANDLERS)
        if overloaded:
            # Pool-full bypass outside the gate (the frame read below is
            # bounded by the framing deadline — never an unbounded wait
            # under the lock): an exact close still terminates, everything
            # else is rejected as before.
            _handle_overload(st, conn)
            continue
        t = threading.Thread(target=_handle_one, args=(st, conn), daemon=True)
        t.start()


def main(argv):
    cfg_dir = None
    try:
        cfg = parse_args(argv)
    except Usage as e:
        write_parse_error(argv, str(e))
        die(str(e), 2)
    except Exception as e:
        die(f"internal: {e}", 1)
    cfg_dir = cfg.dir
    try:
        os.makedirs(cfg.dir, exist_ok=True)
        nonce = write_owner(cfg.dir)
        if not am_owner(cfg.dir, nonce):
            # A silent owner-write failure would surface later as a
            # baffling instant self-reap (Node/Browser verify the same
            # claim explicitly) — fail fast instead.
            die("session owner claim not visible; refusing to start", 1)
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
                 "schemaVersion": 2,
                 "targetIdentity": st._target_identity}))
            serve(st, server, nonce)
        except (Usage, BridgeErr) as e:
            # Failed setup must not leak the spawned adapter/target:
            # clean up first, then report (the CLI removes the dir). The
            # phase derives from the exception type (Usage/ConfigErr read
            # as config; transport losses stay transport; unexpected
            # failures read as runtime) — never from message text or a
            # stage timer.
            try:
                st.cleanup()
            except Exception:
                pass
            write_file(os.path.join(cfg.dir, "error.json"),
                       json.dumps(setup_error_payload(e, str(e))))
            raise
        except Exception as e:
            # Unexpected setup crash (never a silent exit-1): same cleanup,
            # then a sanitized error.json the CLI surfaces. The name stays
            # reusable — the CLI removes failed-setup dirs wholesale.
            # Unexpected failures report runtime (truthful internal
            # error, no endpoint diagnosis); transport losses stay
            # transport via the BridgeErr path above.
            try:
                st.cleanup()
            except Exception:
                pass
            write_file(os.path.join(cfg.dir, "error.json"),
                       json.dumps(setup_error_payload(e, format_unexpected(e))))
            raise
        finally:
            try:
                server.close()
            except Exception:
                pass
    except Usage as e:
        die(str(e), 2)
    except BridgeErr as e:
        die(str(e), 1)
    except Exception as e:
        if cfg_dir is not None:
            write_file(os.path.join(cfg_dir, "error.json"),
                       json.dumps(setup_error_payload(e, format_unexpected(e))))
        die(format_unexpected(e), 1)


if __name__ == "__main__":
    main(sys.argv[1:])
