"""UX-batch targeted live: event-driven wait, bounded auto-resuming capture,
and additive stop diagnostics on all four adapters.

Run after cargo build; owns all fixtures/processes. Uses existing
debugpy/ws installations without installing dependencies. Not part of the
legacy full suite (final gate runs that); run explicitly:
    python3 tests/test_ux_live.py

Coordination policy: no arbitrary sleeps drive acceptance. Fixture
readiness comes from process output markers (_wait_log) and prompt
file/state reads (status/session.json); long-poll futures (wait/capture
timeouts) bound every wait. The 0.1s polls below are bounded readiness
coordination only, mirroring tests/test_live.py.
"""
import concurrent.futures
import functools
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import threading
import time
import unittest
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / "target/debug/agent-debugger"


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def break_line(path, marker):
    for n, text in enumerate(Path(path).read_text().splitlines(), 1):
        if marker in text:
            return n
    raise AssertionError(f"{marker!r} not found in {path}")


class UxLiveTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory(prefix="debugger-ux-live-")
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
        fx = cls.fixture = cls.home / "uxfixture"
        fx.mkdir()
        # Loop fixtures (fresh stops arrive on their own every ~150ms).
        # Never-called helpers in their own files: verified yet unreachable
        # (dead lines in the loop file would fold/slide onto live lines).
        (fx / "ux_helper.py").write_text(
            'def helper():\n    return 42  # HELPER\n')
        (fx / "ux_helper.js").write_text(
            'function helper() { return 42; }  // HELPER\n'
            'module.exports = { helper };\n')
        (fx / "ux_loop.py").write_text(
            'import time\nimport ux_helper\ntotal = 0\nprint("ready", flush=True)\n'
            'while True:\n    tick = total + 1  # BREAK\n    total = tick  # BREAK2\n'
            '    print(tick, flush=True)\n    time.sleep(0.15)\n')
        (fx / "ux_loop.js").write_text(
            'let total = 0;\nconsole.log("ready");\n'
            'require("./ux_helper.js");  // load once, never called\n'
            'setInterval(() => {\n  const tick = total + 1;  // BREAK\n'
            '  total = tick;  // BREAK2\n  console.log(tick);\n}, 150);\n')
        (fx / "UxLoop.java").write_text(
            'public class UxLoop {\n'
            ' public static void main(String[] args) throws Exception {\n'
            '  System.out.println("ready");\n'
            '  int total = 0;\n'
            '  while (true) {\n'
            '   int tick = total + 1; // BREAK\n'
            '   total = tick; // BREAK2\n'
            '   System.out.println(tick);\n'
            '   Thread.sleep(150);\n'
            '  }\n }\n'
            ' static int never() { return 1; }\n}\n')
        subprocess.run(["javac", "-g", "-d", str(fx), str(fx / "UxLoop.java")],
                       check=True, timeout=30)
        # HTTP fixtures (the reported pain: parked handler + open request).
        (fx / "ux_http.py").write_text(
            'import sys\nfrom http.server import BaseHTTPRequestHandler, HTTPServer\n'
            'port = int(sys.argv[1])\nprint("ready", flush=True)\n'
            'class H(BaseHTTPRequestHandler):\n'
            '    def do_GET(self):\n'
            '        marker = "hit"  # BREAK\n'
            '        self.send_response(200)\n'
            '        self.end_headers()\n'
            '        self.wfile.write(b"ok")\n'
            '    def log_message(self, *a):\n'
            '        pass\n'
            'HTTPServer(("127.0.0.1", port), H).serve_forever()\n')
        (fx / "ux_http.js").write_text(
            "const http = require('node:http');\n"
            'const port = Number(process.argv[2]);\n'
            "console.log('ready');\n"
            'http.createServer((req, res) => {\n'
            "  const marker = 'hit';  // BREAK\n"
            "  res.end('ok');\n"
            "}).listen(port, '127.0.0.1');\n")
        (fx / "UxHttp.java").write_text(
            'import java.net.*;\nimport java.io.*;\n'
            'public class UxHttp {\n'
            ' public static void main(String[] args) throws Exception {\n'
            '  ServerSocket ss = new ServerSocket(Integer.parseInt(args[0]));\n'
            '  System.out.println("ready");\n'
            '  while (true) {\n'
            '   Socket s = ss.accept();\n'
            '   int marker = 1; // BREAK\n'
            '   String body = "ok";\n'
            '   String resp = "HTTP/1.0 200 OK\\r\\nContent-Length: 2\\r\\n\\r\\n" + body;\n'
            '   s.getOutputStream().write(resp.getBytes("UTF-8"));\n'
            '   s.getOutputStream().flush();\n'
            '   s.close();\n'
            '  }\n }\n}\n')
        subprocess.run(["javac", "-g", "-d", str(fx), str(fx / "UxHttp.java")],
                       check=True, timeout=30)
        # Browser tick page (interval-driven fresh stops, no reload needed).
        webdir = fx / "web"
        webdir.mkdir()
        (webdir / "tick.js").write_text(
            'let n = 0;\nfunction tick() {\n  const seen = n + 1; // BREAK\n'
            '  n = seen;\n  document.title = "tick " + n;\n}\n'
            'setInterval(tick, 200);\n')
        (webdir / "tick.html").write_text(
            '<!doctype html><html><head><title>t</title></head>'
            '<body><script src="tick.js"></script></body></html>\n')
        cls.py_loop = fx / "ux_loop.py"
        cls.js_loop = fx / "ux_loop.js"
        cls.py_http = fx / "ux_http.py"
        cls.js_http = fx / "ux_http.js"

    @classmethod
    def cleanup(cls):
        for name in list(cls.sessions):
            try:
                cls.cli(name, "close", timeout=85)
            except Exception:
                pass
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
        result = subprocess.run([str(BIN), "--session", name, *args],
                                env=env or cls.env, cwd=cwd or ROOT,
                                capture_output=True, text=True,
                                timeout=timeout)
        data = json.loads(result.stdout)
        if ok and (result.returncode or not data.get("ok")):
            raise AssertionError(f"{name} {args}: {data}, stderr={result.stderr}")
        return data.get("data", data)

    def track(self, name):
        self.sessions.add(name)
        return name

    def close(self, name):
        self.assertTrue(self.cli(name, "close", timeout=85)["confirmed"])
        self.sessions.remove(name)

    def _wait_log(self, path, marker, timeout=25):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                if marker in Path(path).read_text():
                    return
            except OSError:
                pass
            time.sleep(0.1)
        self.fail(f"{path} never printed {marker!r}")

    def session_file(self, name):
        return self.home / ".agent-debugger/sessions" / name / "session.json"

    def wait_file_stopped(self, name, want=True, timeout=20):
        """Bounded readiness on the prompt session file (no bridge command)."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                meta = json.loads(self.session_file(name).read_text())
                if bool(meta.get("stopped")) == want:
                    return meta
            except OSError:
                pass
            time.sleep(0.1)
        self.fail(f"session {name} file never reached stopped={want}")

    # ---- shared per-adapter flows ----

    def start_spec(self, lang, prog, brk=None, extra=()):
        if lang == "py":
            args = ["py", "start", str(prog), *extra]
        elif lang == "node":
            args = ["node", "start", str(prog), *extra]
        else:
            args = ["java", "start", "--main", prog,
                    "--cp", str(self.fixture), *extra]
        if brk is not None:
            args += ["--break", brk]
        return args

    def loop_prog(self, lang):
        if lang == "py":
            return self.py_loop
        if lang == "node":
            return self.js_loop
        return "UxLoop"

    def loop_break(self, lang):
        if lang == "py":
            return f"{self.py_loop}:{break_line(self.py_loop, '# BREAK')}"
        if lang == "node":
            return f"{self.js_loop}:{break_line(self.js_loop, '// BREAK')}"
        line = break_line(self.fixture / "UxLoop.java", "// BREAK")
        return f"UxLoop:{line}"

    def loop_break2(self, lang):
        if lang == "py":
            return f"{self.py_loop}:{break_line(self.py_loop, '# BREAK2')}"
        if lang == "node":
            return f"{self.js_loop}:{break_line(self.js_loop, '// BREAK2')}"
        line = break_line(self.fixture / "UxLoop.java", "// BREAK2")
        return f"UxLoop:{line}"

    def never_break(self, lang):
        if lang == "py":
            helper = self.fixture / "ux_helper.py"
            return f"{helper}:{break_line(helper, '# HELPER')}"
        if lang == "node":
            helper = self.fixture / "ux_helper.js"
            return f"{helper}:{break_line(helper, '// HELPER')}"
        line = next(n for n, t in
                    enumerate((self.fixture / "UxLoop.java").read_text().splitlines(), 1)
                    if "return 1" in t)
        return f"UxLoop:{line}"

    def check_diag(self, resp, line):
        diag = resp["diag"]
        self.assertIn("stopId", diag)
        self.assertGreaterEqual(diag["stopId"], 1)
        self.assertIn("target", diag)
        self.assertIsNotNone(diag["reason"])
        self.assertIn("stoppingThread", diag)
        self.assertIn("sameLocation", diag)
        self.assertIn("sameThread", diag)
        # Never fabricated: explicit nulls, never invented ids.
        self.assertIn("hitBreakpoints", diag)
        self.assertIn("HTTP handler remains open", resp["warning"])
        self.assertEqual(resp["snapshot"]["location"]["line"], line)

    def flow_wait_capture(self, lang):
        """wait immediate/fresh/timeout + capture prepark/fresh/timeout."""
        loop = self.loop_prog(lang)
        line = int(self.loop_break(lang).rsplit(":", 1)[1])
        brk = self.loop_break(lang)
        # Parked session: immediate wait + prepark capture + same-line diag.
        name = self.track(f"ux-{lang}-parked")
        started = self.cli(name, *self.start_spec(lang, loop, brk), timeout=40)
        self.assertEqual(started["location"]["line"], line)
        waited = self.cli(name, "wait", "--timeout", "10", timeout=20)
        self.assertFalse(waited["waited"])
        self.check_diag(waited, line)
        self.assertTrue(self.cli(name, "context")["location"]["line"] == line)
        cap = self.cli(name, "capture", "--timeout", "10", timeout=20)
        self.assertTrue(cap["targetWasPaused"])
        self.assertFalse(cap["resumed"])
        self.assertEqual(cap["pauseDurationMs"], 0)
        # Still parked: the pre-existing park was never resumed.
        self.assertEqual(self.cli(name, "context")["location"]["line"], line)
        first_id = cap["diag"]["stopId"]
        nxt = self.cli(name, "continue", "--timeout", "15", timeout=25)
        self.assertEqual(nxt["snapshot"]["location"]["line"], line)
        self.assertTrue(nxt["diag"]["sameLocation"],
                        "loop re-hit must diagnose same-line, not suppress it")
        self.assertEqual(nxt["diag"]["stopId"], first_id + 1)
        self.close(name)
        # Running session: fresh wait + fresh capture + timeouts.
        name = self.track(f"ux-{lang}-running")
        self.cli(name, *self.start_spec(lang, loop), timeout=40)
        self.cli(name, "breaks", "add", "--break", brk, timeout=20)
        t0 = time.monotonic()
        fresh = self.cli(name, "wait", "--timeout", "15", timeout=25)
        dt = time.monotonic() - t0
        self.assertTrue(fresh["waited"])
        self.check_diag(fresh, line)
        self.assertLess(dt, 15, "wait woke on the event, not the timeout")
        # Fresh capture auto-resumes inside the pause budget (a second
        # loop line, so the ephemeral really plants instead of dup-ing).
        # The fresh wait above left the target parked: drop the loop line
        # and run free (typed timeout, target running), then capture fresh.
        brk2 = self.loop_break2(lang)
        line2 = int(brk2.rsplit(":", 1)[1])
        self.cli(name, "breaks", "remove", "--break", brk, timeout=20)
        free = self.cli(name, "continue", "--timeout", "3", timeout=15,
                        ok=False)
        self.assertIn("timeout", free.get("error", ""), free)
        cap = self.cli(name, "capture", "--break", brk2, "--timeout", "15",
                       timeout=25)
        self.assertFalse(cap["targetWasPaused"])
        self.assertTrue(cap["resumed"], cap.get("resumeError"))
        self.assertNotIn("removeError", cap)
        self.assertTrue(cap["ephemeralPlanted"])
        self.assertLessEqual(cap["pauseDurationMs"], cap["pauseBudgetMs"])
        self.assertFalse(cap["budgetExceeded"])
        self.check_diag(cap, line2)
        stops = self.cli(name, "breaks", timeout=20)["stops"]
        self.assertEqual(stops, [], "ephemeral left no record behind")
        running = self.cli(name, "threads", timeout=20)
        self.assertTrue(running["running"], "capture resumed the target")
        # Timeout preserves session/intents and leaks no ephemeral.
        never = self.never_break(lang)
        self.cli(name, "breaks", "add", "--break", never, timeout=20)
        err = self.cli(name, "wait", "--timeout", "2", timeout=15, ok=False)
        self.assertIn("timeout: no stop within 2s", err["error"])
        cerr = self.cli(name, "capture", "--break", never, "--timeout", "2",
                        timeout=15, ok=False)
        self.assertIn("timeout: no stop within 2s", cerr["error"])
        stops = self.cli(name, "breaks", timeout=20)["stops"]
        self.assertEqual(len(stops), 1, stops)
        never_line = never.rsplit(":", 1)[1]
        raws = {s.get("spec", "") for s in stops}
        self.assertTrue(any(r.endswith(":" + never_line) for r in raws),
                        f"never-break intent survives timeouts: {raws}")
        self.close(name)

    def test_32_py_wait_capture(self):
        self.flow_wait_capture("py")

    def test_33_node_wait_capture(self):
        self.flow_wait_capture("node")

    def test_34_java_wait_capture(self):
        self.flow_wait_capture("java")

    # ---- the reported HTTP pain, end to end ----

    def flow_http_recipe(self, lang, prog, port_extra, mode):
        """attach short timeout -> trigger HTTP -> wait (no sleep/status
        polling) -> resume promptly so the handler completes.
        mode 'wait-continue': wait parks; continue resumes at once (the
        request completes) and then waits for the NEXT stop — its timeout
        means "resumed, nothing more hit", not a stuck request.
        mode 'capture': one-shot capture parks, collects, and auto-resumes
        with resumed:true (no second command needed)."""
        fx = self.fixture
        if lang == "py":
            path = self.py_http
            line = break_line(path, "# BREAK")
            brk = f"{path}:{line}"
            cli_args = ["py", "start", str(path), "--", str(port_extra)]
        elif lang == "node":
            path = self.js_http
            line = break_line(path, "// BREAK")
            brk = f"{path}:{line}"
            cli_args = ["node", "start", str(path), "--", str(port_extra)]
        else:
            path = fx / "UxHttp.java"
            line = break_line(path, "// BREAK")
            brk = f"UxHttp:{line}"
            cli_args = ["java", "start", "--main", "UxHttp", "--cp", str(fx),
                        "--", str(port_extra)]
        name = self.track(f"ux-{lang}-http")
        started = self.cli(name, *cli_args, timeout=40)
        self.assertTrue(started.get("running", False) or "threads" in started)
        url = f"http://127.0.0.1:{port_extra}/"
        got = {}

        def fetch():
            try:
                with urllib.request.urlopen(url, timeout=25) as r:
                    got["body"] = r.read()
                    got["code"] = r.status
            except Exception as e:  # noqa: BLE001 — recorded, asserted below
                got["error"] = str(e)

        if mode == "capture":
            # One-shot: the capture long-poll (background) is the waiter;
            # the in-flight request fires the park; auto-resume completes it.
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                fut = pool.submit(self.cli, name, "capture", "--break", brk,
                                  "--timeout", "20", timeout=30)
                time.sleep(1.0)  # bounded coordination: plant lands first
                t = threading.Thread(target=fetch, daemon=True)
                t.start()
                cap = fut.result(timeout=30)
            t.join(25)
            self.assertFalse(cap["targetWasPaused"])
            self.assertTrue(cap["resumed"], cap.get("resumeError"))
            self.assertTrue(cap["ephemeralPlanted"])
            self.check_diag(cap, line)
            self.assertEqual(got.get("code"), 200, got)
            self.assertEqual(got.get("body"), b"ok", got)
            self.close(name)
            return
        self.cli(name, "breaks", "add", "--break", brk, timeout=20)
        t = threading.Thread(target=fetch, daemon=True)
        t.start()
        # No sleep/status polling: the long-poll wakes on the fresh stop.
        parked = self.cli(name, "wait", "--timeout", "20", timeout=30)
        self.assertTrue(parked["stopped"])
        self.check_diag(parked, line)
        # Prompt continue resumes at once (the suspended handler completes
        # the request) and then waits for the NEXT stop: the typed timeout
        # below means "resumed, nothing more hit".
        cont = self.cli(name, "continue", "--timeout", "8", timeout=20,
                        ok=False)
        self.assertIn("timeout: no stop within 8s", cont.get("error", ""), cont)
        t.join(25)
        self.assertEqual(got.get("code"), 200, got)
        self.assertEqual(got.get("body"), b"ok", got)
        running = self.cli(name, "threads", timeout=20)
        self.assertTrue(running["running"])
        self.close(name)

    def test_35_py_http_recipe(self):
        self.flow_http_recipe("py", self.py_http, free_port(), "wait-continue")

    def test_36_node_http_recipe(self):
        self.flow_http_recipe("node", self.js_http, free_port(), "capture")

    def test_37_java_http_recipe(self):
        self.flow_http_recipe("java", "UxHttp", free_port(), "wait-continue")

    # ---- busy + close + client-disconnect while a UX wait is outstanding ----

    def test_38_busy_and_close_during_wait(self):
        # Running session, nothing armed: the background wait long-polls
        # (never instant), so it holds the slot for the whole window.
        name = self.track("ux-busy-wait")
        self.cli(name, *self.start_spec("py", self.loop_prog("py")),
                 timeout=40)
        # Retry the race a few times — the point is the rival's immediate
        # busy verdict once the wait's slot is held.
        busy = {}
        for _ in range(3):
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                fut = pool.submit(self.cli, name, "wait", "--timeout", "25",
                                  timeout=35, ok=False)
                time.sleep(1.5)  # bounded coordination: slot lands in ms
                busy = self.cli(name, "continue", "--timeout", "2",
                                timeout=15, ok=False)
                if "busy" in busy.get("error", ""):
                    break
                # The rival won the race (wait went busy instead): drain and
                # retry with a free slot.
                fut.result(timeout=40)
        self.assertIn("busy", busy.get("error", ""), busy)
        # Close is accepted despite the outstanding wait.
        self.assertTrue(self.cli(name, "close", timeout=85)["confirmed"])
        self.sessions.remove(name)
        # The background wait aborts instead of hanging the harness (any
        # outcome — error envelope or transport loss — proves no hang).
        try:
            fut.result(timeout=40)
        except Exception:
            pass

    def flow_disconnect_resumes(self, lang):
        """SIGKILL the capture client inside the park window: the bridge
        still removes the ephemeral and resumes (only the response is
        lost). The park-to-resume window is short, so poll the prompt
        session file fast and retry the capture until the kill lands
        inside a park (bounded attempts; every attempt is a real capture).
        A kill that lands just after a natural resume is harmless: the
        post-state assertions are identical."""
        loop = self.loop_prog(lang)
        brk = self.loop_break(lang)
        name = self.track(f"ux-{lang}-disc")
        self.cli(name, *self.start_spec(lang, loop), timeout=40)
        killed = False
        for _ in range(8):
            proc = subprocess.Popen(
                [str(BIN), "--session", name, "capture", "--break", brk,
                 "--timeout", "30"],
                env=self.env, cwd=ROOT, stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL)
            try:
                deadline = time.monotonic() + 28
                while time.monotonic() < deadline:
                    if proc.poll() is not None:
                        break  # finished before any kill: retry
                    try:
                        meta = json.loads(self.session_file(name).read_text())
                    except OSError:
                        meta = {}
                    if meta.get("stopped"):
                        proc.send_signal(signal.SIGKILL)
                        killed = True
                        break
                    time.sleep(0.005)  # fast readiness poll, bounded above
                proc.wait(timeout=15)
            finally:
                try:
                    proc.kill()
                except Exception:
                    pass
            if killed:
                break
        self.assertTrue(killed, "never landed a kill inside the park window")
        # The target is running again and the ephemeral is gone.
        deadline = time.monotonic() + 20
        while True:
            try:
                if self.cli(name, "threads", timeout=20).get("running"):
                    break
            except AssertionError:
                pass
            if time.monotonic() > deadline:
                self.fail(f"{lang} capture did not resume after client death")
            time.sleep(0.2)
        stops = self.cli(name, "breaks", timeout=20)["stops"]
        self.assertEqual(stops, [], "ephemeral removed despite disconnect")
        # No lingering park from the killed capture: with nothing armed the
        # next wait is a clean typed timeout, never a stale suspended reuse.
        live = self.cli(name, "wait", "--timeout", "2", timeout=15, ok=False)
        self.assertIn("timeout", live.get("error", ""), live)
        self.close(name)

    def test_39_py_disconnect_resumes(self):
        self.flow_disconnect_resumes("py")

    def test_40_node_disconnect_resumes(self):
        self.flow_disconnect_resumes("node")

    def test_41_java_disconnect_resumes(self):
        self.flow_disconnect_resumes("java")

    # ---- browser: interval-driven capture + parked wait ----

    def test_42_browser_wait_capture(self):
        chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
        if not Path(chrome).exists():
            self.skipTest("Chrome unavailable")
        webdir = self.fixture / "web"
        line = break_line(webdir / "tick.js", "// BREAK")
        brk = f"tick.js:{line}"

        class QuietHandler(http.server.SimpleHTTPRequestHandler):
            def log_message(self, *args):
                pass

        handler = functools.partial(QuietHandler, directory=str(webdir))
        type(self).http = http.server.ThreadingHTTPServer(("127.0.0.1", 0),
                                                          handler)
        threading.Thread(target=self.http.serve_forever, daemon=True).start()
        cdp_port = free_port()
        url = f"http://127.0.0.1:{self.http.server_port}/tick.html"
        type(self).chrome = subprocess.Popen(
            [chrome, "--headless", "--disable-gpu", "--no-first-run",
             f"--remote-debugging-port={cdp_port}",
             f"--user-data-dir={self.home}/chrome", url],
            stdout=self.log, stderr=self.log)
        deadline = time.monotonic() + 20
        while True:
            try:
                with urllib.request.urlopen(
                        f"http://127.0.0.1:{cdp_port}/json/list",
                        timeout=1) as response:
                    if any(t.get("url") == url for t in json.load(response)):
                        break
            except OSError:
                pass
            if time.monotonic() > deadline:
                self.fail("Chrome did not expose test tab")
            time.sleep(.1)
        name = self.track("ux-browser")
        self.cli(name, "browser", "attach", "--port", str(cdp_port),
                 "--tab", url, timeout=40)
        # Fresh one-shot capture on the running tab (interval fires it).
        cap = self.cli(name, "capture", "--break", brk, "--timeout", "20",
                       timeout=30)
        self.assertFalse(cap["targetWasPaused"])
        self.assertTrue(cap["resumed"], cap.get("resumeError"))
        self.assertTrue(cap["ephemeralPlanted"])
        self.check_diag(cap, line)
        stops = self.cli(name, "breaks", timeout=20)["stops"]
        self.assertEqual(stops, [], "ephemeral left no record behind")
        # Parked path: arm persistently, reload to park, wait is immediate.
        self.cli(name, "breaks", "add", "--break", brk, timeout=20)
        reloaded = self.cli(name, "reload", "--timeout", "20", timeout=30)
        self.assertTrue(reloaded["stopped"])
        waited = self.cli(name, "wait", "--timeout", "10", timeout=20)
        self.assertFalse(waited["waited"])
        self.check_diag(waited, line)
        pre = self.cli(name, "capture", "--timeout", "10", timeout=20)
        self.assertTrue(pre["targetWasPaused"])
        self.assertFalse(pre["resumed"])
        self.close(name)


if __name__ == "__main__":
    unittest.main()
