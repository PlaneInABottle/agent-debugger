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
import socket
import subprocess
import sys
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
    unmatched events are stashed for pump() to consume."""

    def __init__(self, sock):
        self.sock = sock
        self.sock.settimeout(5.0)
        self.seq = 0
        self.buf = b""
        self.stash = []

    def _read_msg(self):
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
        else:
            raise Usage(f"unknown arg: {a}")
        i += 1
    if cfg.kind not in ("launch", "attach"):
        raise Usage("--kind must be launch or attach")
    if not cfg.dir:
        raise Usage("session needs --dir")
    if cfg.kind == "launch" and not cfg.program:
        raise Usage("launch needs --program")
    if cfg.kind == "attach" and not cfg.port:
        raise Usage("attach needs --port")
    for kind, spec in pending_stops:
        if kind == "break":
            parse_break(spec, cfg)
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


class Session:
    def __init__(self, cfg):
        self.cfg = cfg
        self.adapter = None
        self.dap = None
        self.server = None
        self.thread_id = None
        self.frames = []      # cached DAP stack frames of current stop
        self.suspended = False
        self.exited = False
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

    # -- DAP helpers

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

    def frame_locals(self, index=0):
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
            if len(out) >= MAX_VARS:
                out.append({"name": "…",
                            "note": f"+{len(children) - MAX_VARS} more"})
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

    def frames_json(self, with_locals):
        out = []
        for i, f in enumerate(self.frames[:MAX_FRAMES]):
            entry = {"index": i, "type": "?",
                     "method": f.get("name", "?"), "line": f.get("line", -1)}
            if with_locals and i == 0:
                entry["locals"] = self.frame_locals(0)
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
                "is executable code (not a blank, comment, or def/class header)")

    def publish_state(self, stopped):
        """Rewrite session.json so `status` shows live truth (parked stop +
        time) with zero prior memory. lastStop survives resume/exit — it
        answers 'where was I last', not 'where am I now'."""
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
             "lastStop": self.last_stop, "updatedAt": int(time.time())}))

    # -- lifecycle

    def free_port(self):
        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
        s.close()
        return port

    def start_adapter(self):
        port = self.free_port()
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
        prog_args = ([self.cfg.program] + self.cfg.prog_args)
        self.dap_request("initialize", {"adapterID": "agent-debugger",
                                        "pathFormat": "path"})
        # No waiting here: the launch response only arrives after
        # configurationDone. It is collected by _drain_launch_response.
        self.dap.send_only("launch", {"program": self.cfg.program,
                                      "args": self.cfg.prog_args,
                                      "justMyCode": True,
                                      "console": "internalConsole"})
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

    def pump(self, timeout):
        """Wait for the next stopped/exited; returns 'stopped' or raises."""
        deadline = time.time() + timeout
        while self.dap.stash:
            msg = self.dap.stash.pop(0)
            r = self._handle_pumped(msg)
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
                if self._awaiting_continued and not self.suspended:
                    # The resume produced only held suspects: park the first
                    # still-live one instead of timing out, then close the
                    # episode either way so later genuine stops park at once.
                    r = self._probe_suspects() if self._suspects else None
                    if r:
                        return r
                    self._awaiting_continued = False
                raise StopTimeout(
                    f"timeout: no stop within {timeout:g}s")
            self.dap.sock.settimeout(min(remaining, 1.0))
            try:
                msg = self.dap._read_msg()
            except (socket.timeout, TimeoutError):
                if (self._awaiting_continued and self._suspects
                        and not self.suspended and remaining > 2):
                    # No `continued` yet (older adapters may never send one):
                    # bounded probe of the held stops, then keep waiting.
                    r = self._probe_suspects()
                    if r:
                        return r
                continue  # idle second; re-check deadline
            except BridgeErr as e:
                if "closed" in str(e).lower():
                    self.exited = True
                    raise BridgeErr(f"lost connection to target: {e}")
                raise
            r = self._handle_pumped(msg)
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

    def _park_stop(self, reason, tid):
        """Park the session on a genuine stop. The single place that moves
        the park: thread, frames, stopInfo, change tracking, hit counting."""
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
        return "stopped"

    def _handle_pumped(self, msg):
        if not isinstance(msg, dict) or msg.get("type") != "event":
            return None
        ev = msg.get("event")
        body = msg.get("body", {})
        if not isinstance(body, dict):
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
                return self._park_stop(reason, tid)
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
            self.exited = True
            self.publish_state(False)
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
        return {"ok": True, "stopInfo": json.loads(self.stop_info or "null"),
                "location": self.location_json(),
                "threads": self.threads_json(self.thread_id),
                "frames": self.frames_json(True)}

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

    def _resume_and_wait(self, timeout):
        """Resume after step/continue and wait for the next stop."""
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
        self.pump(timeout)
        return {"ok": True, "stopped": True, "changed": json.loads(self.last_changed),
                "stopInfo": json.loads(self.stop_info or "null"),
                "snapshot": self.snapshot()}

    def cmd_step(self, req, timeout):
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
        return self._resume_and_wait(timeout)

    def cmd_continue(self, req, timeout):
        self.require_live()
        if not self.suspended:
            # Never stopped: nothing to resume (a null threadId only
            # produces adapter errors) — the target already runs,
            # so go straight to waiting for the next stop.
            return self._resume_and_wait(timeout)
        self.dap_request("continue", {"threadId": self.thread_id})
        return self._resume_and_wait(timeout)

    def drain_pending(self, budget=1.0):
        """Consume already-arrived messages (output events etc.) without
        waiting for a stop. Lets `logs` flush recently collected lines."""
        deadline = time.time() + budget
        while time.time() < deadline:
            if self.dap.stash:
                msg = self.dap.stash.pop(0)
            else:
                self.dap.sock.settimeout(max(0.05, deadline - time.time()))
                try:
                    msg = self.dap._read_msg()
                except (socket.timeout, TimeoutError):
                    return
                except BridgeErr:
                    return
            try:
                self._handle_pumped(msg)
            except BridgeErr:
                return

    def cmd_threads(self):
        if self.exited:
            raise BridgeErr("target VM has exited — close this session")
        return {"ok": True, "running": not self.suspended,
                "threads": self.threads_dump()}

    def cmd_breaks(self):
        # Arm-time records: what was requested and whether it planted
        # (verified/pending) — no round-trip needed, no stop required.
        if self.exited:
            raise BridgeErr("target VM has exited — close this session")
        return {"ok": True, "stops": self.stop_states}

    def cmd_breaks_add(self, req):
        """Additive line breaks on a live session (running or parked — never
        suspended/resumed here). The whole batch validates first (parse,
        canonical dedup/conflict) with no DAP traffic; then each touched file
        is re-sent merged with existing breaks (setBreakpoints replaces per
        file). Per-file DAP successes become the confirmed subset (partial
        ok + warning); total failure is ok:false with nothing mutated."""
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
            return {"ok": True, "added": [], "stops": self.stop_states}
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
        return resp

    def _refresh_rec(self, rec, got, is_logpoint, requested):
        """Apply one setBreakpoints answer to a record (shared by arm/add).

        Returns the bound line so callers can store it in _hitkeys."""
        bound = self._apply_verification(rec, got, requested, is_logpoint)
        return bound

    def cmd_logs(self, req):
        tail = max(1, min(500, int(req.get("tail", 50))))
        # Flush recently arrived output first: in logpoints-only sessions
        # nothing else ever pumps the queue.
        if not self.exited:
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
        if self.exited:
            raise BridgeErr("target VM has exited — close this session")
        if not self.suspended or self.thread_id is None:
            raise BridgeErr("no stopped thread (target is running — continue first)")
        if not self.frames:
            self.refresh_frames(timeout=5)
            if not self.frames:
                raise BridgeErr("target stopped but stack is unavailable; retry context or continue")

    def require_live(self):
        if self.exited:
            raise BridgeErr("target VM has exited — close this session")

    def dispatch(self, req):
        cmd = req.get("cmd")
        try:
            timeout = float(req.get("timeout", self.cfg.timeout))
        except (ValueError, TypeError) as e:
            raise BridgeErr("timeout must be between 0 and 3600 seconds") from e
        if not math.isfinite(timeout) or not 0 < timeout <= 3600:
            raise BridgeErr("timeout must be between 0 and 3600 seconds")
        if cmd == "close":
            raise _Close()
        if cmd == "context":
            return self.cmd_context()
        if cmd == "stack":
            return self.cmd_stack()
        if cmd == "vars":
            return self.cmd_vars(req)
        if cmd == "eval":
            return self.cmd_eval(req)
        if cmd == "step":
            return self.cmd_step(req, timeout)
        if cmd == "continue":
            return self.cmd_continue(req, timeout)
        if cmd == "threads":
            return self.cmd_threads()
        if cmd == "breaks":
            return self.cmd_breaks()
        if cmd == "breaksAdd":
            return self.cmd_breaks_add(req)
        if cmd == "logs":
            return self.cmd_logs(req)
        raise BridgeErr(f"unknown cmd: {cmd}")

    def cleanup(self):
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
    """Drain a bounded batch between commands, using the sole DAP reader."""
    if st.exited:
        return
    sock = st.dap.sock
    prev = sock.gettimeout()
    deadline = time.monotonic() + 0.05
    try:
        for _ in range(256):
            if time.monotonic() >= deadline:
                break
            # Requests may have stashed events while inspecting an earlier
            # stop. Consume those before reading newer socket messages.
            if st.dap.stash:
                msg = st.dap.stash.pop(0)
            else:
                sock.settimeout(max(0.001, deadline - time.monotonic()))
                try:
                    msg = st.dap._read_msg()
                except (socket.timeout, TimeoutError):
                    break
                except BridgeErr:
                    st.exited = True
                    st.publish_state(False)
                    break
            try:
                if st._handle_pumped(msg):
                    break
            except BridgeErr as e:
                sys.stderr.write(f"dap: {e}\n")
                break
    finally:
        sock.settimeout(prev)


def serve(st, server, nonce):
    # Poll between commands without another DAP reader thread.
    try:
        server.settimeout(0.1)
    except OSError:
        pass
    while True:
        # Abandoned (dir rm'd or respawned under our name)? Quit quietly.
        if not am_owner(st.cfg.dir, nonce):
            try:
                st.cleanup()
            except Exception:
                pass
            return
        idle_pump(st)
        try:
            conn, _ = server.accept()
        except (socket.timeout, TimeoutError):
            continue
        except OSError:
            return
        try:
            try:
                req = read_frame(conn)
            except BridgeErr as e:
                try_write_frame(conn, {"ok": False, "error": str(e)})
                continue
            try:
                write_frame(conn, st.dispatch(req))
            except _Close:
                try_write_frame(conn, {"ok": True, "closed": True})
                st.cleanup()
                return
            except BridgeErr as e:
                try_write_frame(conn, {"ok": False, "error": str(e)})
            except Exception as e:
                try_write_frame(conn, {"ok": False, "error": f"internal: {e}"})
        finally:
            try:
                conn.close()
            except Exception:
                pass


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
                 "stopped": bool(cfg.breaks or cfg.methods or cfg.want_exc),
                 "lastStop": st.last_stop, "updatedAt": int(time.time())}))
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
