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
# debugpy launcher version banners (never user data, just noise in logs).
BANNER_LINES = {"ptvsd", "debugpy"}


class Usage(Exception):
    pass


class BridgeErr(Exception):
    pass


# ---------------------------------------------------------------- framing

def read_frame(conn):
    header = b""
    while b"\r\n\r\n" not in header:
        chunk = conn.recv(4096)
        if not chunk:
            raise BridgeErr("truncated frame")
        header += chunk
    head, rest = header.split(b"\r\n\r\n", 1)
    length = -1
    for line in head.decode("ascii").split("\r\n"):
        if ":" in line:
            name, val = line.split(":", 1)
            if name.strip().lower() == "content-length":
                length = int(val.strip())
    if length < 0:
        raise BridgeErr("bad frame: no Content-Length")
    body = rest
    while len(body) < length:
        chunk = conn.recv(max(4096, length - len(body)))
        if not chunk:
            raise BridgeErr("truncated frame")
        body += chunk
    return json.loads(body.decode("utf-8"))


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
            if b"\r\n\r\n" in self.buf:
                head, rest = self.buf.split(b"\r\n\r\n", 1)
                length = -1
                for line in head.decode("ascii", "replace").split("\r\n"):
                    if ":" in line:
                        name, val = line.split(":", 1)
                        if name.strip().lower() == "content-length":
                            try:
                                length = int(val.strip())
                            except ValueError:
                                length = -1
                if length >= 0 and len(rest) >= length:
                    msg = json.loads(rest[:length].decode("utf-8"))
                    self.buf = rest[length:]
                    return msg
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

    def take_stash(self):
        out, self.stash = self.stash, []
        return out


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


def parse_break(spec, cfg):
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
        raise Usage(f"bad line in --break: {spec}")
    cfg.breaks.append((os.path.abspath(path), lineno, cond))


def parse_logpoint(spec, cfg):
    # Class:line:template with {expr} holes (paths split like Class files).
    first = spec.find(":")
    second = spec.find(":", first + 1) if first >= 0 else -1
    if first <= 0 or second <= 0:
        raise Usage("--logpoint must look like path:line:template")
    path = os.path.abspath(spec[:first])
    try:
        lineno = int(spec[first + 1:second])
    except ValueError:
        raise Usage(f"bad line in --logpoint: {spec}")
    cfg.logpoints.append((path, lineno, spec[second + 1:]))


def parse_args(argv):
    cfg = Config()
    if not argv or argv[0] != "session":
        raise Usage("usage: pybridge.py session [options]")
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
            parse_break(argv[i], cfg)
        elif a == "--logpoint":
            i += 1
            parse_logpoint(argv[i], cfg)
        elif a == "--watch":
            raise Usage("--watch has no debugpy equivalent yet (Python)")
        elif a == "--exit":
            raise Usage("--exit has no debugpy equivalent yet (Python)")
        elif a == "--timeout":
            i += 1
            cfg.timeout = float(argv[i])
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
        self.configured = False  # True once launch/attach handshake completes
        self.stop_states = []  # arm-time records served by `breaks`

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

    def refresh_frames(self, levels=64):
        # Windowed fetch: full stacks on deep recursion cost transport on
        # EVERY stop; frame_entry() widens the window on demand.
        body = self.dap_request("stackTrace", {"threadId": self.thread_id,
                                               "startFrame": 0, "levels": levels})
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
        self.configured = True

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
            for item, got in zip(items, body.get("breakpoints", [])):
                verified = bool(got.get("verified", False))
                spec = f"{self.rel_file(path)}:{item['line']}"
                if item["kind"] == "break" and item.get("cond"):
                    spec += f"|{item['cond']}"
                rec = {"spec": spec, "kind": item["kind"],
                       "state": "verified" if verified else "pending"}
                if item["kind"] == "logpoint":
                    # Resume needs the template ("what was I collecting?"),
                    # not just the line. Cheap (one short string).
                    rec["detail"] = item["template"]
                if not verified:
                    msg = got.get("message", "pending")
                    if item["kind"] == "break":
                        rec["detail"] = msg
                    else:
                        rec["detail"] = f"{item['template']} ({msg})"
                    sys.stderr.write(
                        f"warn: breakpoint unverified: {path}:{item['line']} "
                        f"({msg})\n")
                self.stop_states.append(rec)
        for func in self.cfg.methods:
            self.dap_request("setFunctionBreakpoints",
                             {"breakpoints": [{"name": func}]})
            self.stop_states.append(
                {"spec": f"method:{func}", "kind": "method", "state": "armed"})
        if self.cfg.want_exc:
            self.dap_request("setExceptionBreakpoints", {"filters": ["uncaught"]})
            self.stop_states.append(
                {"spec": "exc", "kind": "exc", "state": "armed"})

    def pump(self, timeout):
        """Wait for the next stopped/exited; returns 'stopped' or raises."""
        deadline = time.time() + timeout
        for msg in self.dap.take_stash():
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
                raise BridgeErr(
                    f"timeout: no stop within {timeout:g}s")
            self.dap.sock.settimeout(min(remaining, 1.0))
            try:
                msg = self.dap._read_msg()
            except (socket.timeout, TimeoutError):
                continue  # idle second; re-check deadline
            except BridgeErr as e:
                if "closed" in str(e).lower():
                    self.exited = True
                    raise BridgeErr(f"lost connection to target: {e}")
                raise
            r = self._handle_pumped(msg)
            if r:
                return r

    def _handle_pumped(self, msg):
        if msg.get("type") != "event":
            return None
        ev = msg.get("event")
        body = msg.get("body", {})
        if ev == "stopped":
            reason = body.get("reason", "")
            if reason in ("breakpoint", "step", "exception", "function breakpoint",
                          "data breakpoint", "entry", "goto"):
                self.thread_id = body.get("threadId")
                self.suspended = True
                self.refresh_frames()
                if reason == "exception":
                    self.stop_info = self.exception_info()
                else:
                    self.stop_info = None
                self.track_changes()
                return "stopped"
            return None
        if ev in ("exited", "terminated"):
            self.exited = True
            raise BridgeErr("target exited")
        if ev == "output" and isinstance(body, dict):
            text = body.get("output", "")
            # Startup banners (ptvsd/debugpy) arrive during the handshake;
            # only collect once the session is configured.
            if text and self.configured:
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
        if line is None:
            return
        if self.log_count >= MAX_LOG_LINES:
            return
        try:
            with open(os.path.join(self.cfg.dir, "logs.jsonl"), "a") as f:
                f.write(line + "\n")
            self.log_count += 1
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
        self.suspended = False
        self.pump(timeout)
        return {"ok": True, "stopped": True, "changed": json.loads(self.last_changed),
                "stopInfo": json.loads(self.stop_info or "null"),
                "snapshot": self.snapshot()}

    def cmd_step(self, req, timeout):
        self.require_live()
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
        self.dap_request("continue", {"threadId": self.thread_id})
        return self._resume_and_wait(timeout)

    def drain_pending(self, budget=1.0):
        """Consume already-arrived messages (output events etc.) without
        waiting for a stop. Lets `logs` flush recently collected lines."""
        deadline = time.time() + budget
        while time.time() < deadline:
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
        return {"ok": True, "total": len(lines),
                "truncated": len(lines) > tail, "lines": lines[-tail:]}

    def require_stopped(self):
        if self.exited:
            raise BridgeErr("target VM has exited — close this session")
        if not self.suspended or self.thread_id is None:
            raise BridgeErr("no stopped thread (target is running — continue first)")
        if not self.frames:
            raise BridgeErr("no stopped thread yet in this session")

    def require_live(self):
        if self.exited:
            raise BridgeErr("target VM has exited — close this session")

    def dispatch(self, req):
        cmd = req.get("cmd")
        timeout = float(req.get("timeout", self.cfg.timeout))
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


class _Close(Exception):
    pass


def write_file(path, content):
    try:
        with open(path, "w") as f:
            f.write(content)
    except OSError:
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


def serve(st, server, nonce):
    # Idle accept gets a 1s timeout so rm -rf abandonment is noticed even
    # with zero traffic (blocking accept would orphan forever).
    try:
        server.settimeout(1.0)
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
    try:
        cfg = parse_args(argv)
    except Usage as e:
        die(str(e), 2)
    except Exception as e:
        die(f"internal: {e}", 1)
    try:
        os.makedirs(cfg.dir, exist_ok=True)
        nonce = write_owner(cfg.dir)
        server = socket.socket()
        server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        server.bind(("127.0.0.1", 0))
        server.listen(5)
        st = Session(cfg)
        st._nonce = nonce
        if cfg.kind == "launch":
            st.start_adapter()
        try:
            if cfg.kind == "launch":
                st.handshake_launch()
            else:
                st.handshake_attach()
            if cfg.breaks or cfg.methods or cfg.want_exc:
                st.pump(cfg.timeout)
            write_file(os.path.join(cfg.dir, "session.json"), json.dumps(
                {"name": os.path.basename(cfg.dir), "kind": cfg.kind,
                 "port": server.getsockname()[1],
                 "stopped": bool(cfg.breaks or cfg.methods or cfg.want_exc)}))
            serve(st, server, nonce)
        except (Usage, BridgeErr) as e:
            write_file(os.path.join(cfg.dir, "error.json"),
                       json.dumps({"error": str(e)}))
            raise
        finally:
            try:
                server.close()
            except Exception:
                pass
    except (Usage, BridgeErr) as e:
        die(str(e), 1)
    except Exception as e:
        die(f"internal: {e}", 1)


if __name__ == "__main__":
    main(sys.argv[1:])
