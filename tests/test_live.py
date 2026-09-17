"""Real bridge regressions. Run after cargo build; owns all fixtures/processes.

Uses existing debugpy/ws installations without installing dependencies.
"""
import concurrent.futures
import functools
import http.server
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import threading
import time
import unittest
import urllib.request

try:
    from _live_home import LiveHomeMixin, find_chrome, free_port, load_tests
except ImportError:
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from _live_home import LiveHomeMixin, find_chrome, free_port, load_tests

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / "target/debug/agent-debugger"
VENV_PY = Path.home() / ".agent-debugger/adapters/python/venv/bin/python"


class _MockOldBridge(threading.Thread):
    """Minimal framed old-protocol session server (Content-Length JSON, no
    schemaVersion anywhere): proves `close` is version-agnostic and a live
    legacy daemon blocks spawn/collisions. Replies ok to everything; on
    `close` it ACKs then shuts its listener down (confirmed close)."""
    daemon = True

    def __init__(self):
        super().__init__()
        self.sock = socket.socket()
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("127.0.0.1", 0))
        self.port = self.sock.getsockname()[1]
        self.sock.listen(5)
        self.sock.settimeout(0.5)
        self.alive = True

    def run(self):
        while self.alive:
            try:
                conn, _ = self.sock.accept()
            except (socket.timeout, OSError):
                if not self.alive:
                    return
                continue
            try:
                self._serve(conn)
            except Exception:
                pass
            finally:
                try:
                    conn.close()
                except Exception:
                    pass

    def _read_frame(self, conn):
        buf = b""
        while b"\r\n\r\n" not in buf:
            chunk = conn.recv(4096)
            if not chunk:
                return None
            buf += chunk
        head, rest = buf.split(b"\r\n\r\n", 1)
        length = int(head.decode().split("Content-Length:")[1].strip().split()[0])
        while len(rest) < length:
            chunk = conn.recv(4096)
            if not chunk:
                return None
            rest += chunk
        return json.loads(rest[:length].decode())

    def _serve(self, conn):
        body = self._read_frame(conn)
        if body is None:
            return
        payload = json.dumps({"ok": True, "target": "main"}).encode()
        conn.sendall(b"Content-Length: %d\r\n\r\n" % len(payload) + payload)
        if body.get("cmd") == "close":
            self.alive = False
            try:
                self.sock.close()
            except Exception:
                pass

    def stop(self):
        self.alive = False
        try:
            self.sock.close()
        except Exception:
            pass


class LiveTests(LiveHomeMixin, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.setup_home("debugger-regression-", "fixture with spaces")
        (cls.fixture / "Repeat.java").write_text(
            'public class Repeat {\n'
            ' public static void main(String[] args) throws Exception {\n'
            '  for (int i=0;i<100;i++) {\n'
            '   int value=i+1;\n'
            '   System.out.println(value);\n'
            '   Thread.sleep(150);\n'
            '  }\n }\n}\n')
        subprocess.run(["javac", "-g", "-d", str(cls.fixture), str(cls.fixture / "Repeat.java")], check=True, timeout=30)
        (cls.fixture / "repeat.py").write_text('import time\nfor i in range(100):\n    value=i+1\n    print(value, flush=True)\n    time.sleep(.15)\n')
        (cls.fixture / "repeat.js").write_text('const captured=7;\nfunction tick() {\n const local=2;\n console.log(local+captured);\n}\nsetInterval(tick,150);\n')

    def test_01_parallel_sessions_and_repeated_steps(self):
        specs = {
            "java-review": ["java", "start", "--main", "Repeat", "--cp", str(self.fixture), "--break", "Repeat:5"],
            "py-review": ["py", "start", str(self.fixture / "repeat.py"), "--break", f"{self.fixture}/repeat.py:4"],
            "node-review": ["node", "start", str(self.fixture / "repeat.js"), "--break", f"{self.fixture}/repeat.js:4"],
        }
        self.sessions.update(specs)
        with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
            starts = list(pool.map(lambda item: self.cli(item[0], *item[1]), specs.items()))
        self.assertEqual(len(starts), 3)
        for name in specs:
            self.assertIn("location", self.cli(name, "context"))
        with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
            results = list(pool.map(lambda name: self.cli(name, "eval", "value" if name != "node-review" else "local+captured"), specs))
        self.assertEqual(results[-1]["value"], "9")
        locals_ = self.cli("node-review", "vars")["locals"]
        self.assertTrue({"local", "captured"} <= {v["name"] for v in locals_})
        for _ in range(12):
            result = self.cli("java-review", "continue", "--timeout", "2")
            self.assertEqual(result["snapshot"]["location"]["line"], 5)
        step = self.cli("java-review", "step", "over", "--timeout", "2")
        self.assertEqual(step["snapshot"]["location"]["line"], 6)
        for name in specs:
            self.assertTrue(self.cli(name, "close")["confirmed"])
            self.sessions.remove(name)

    def test_02_python_idle_burst_and_repeat_continue_timeout(self):
        path = self.fixture / "idle.py"
        gate = self.fixture / "go"
        path.write_text('import pathlib,time\n'
                        'for i in range(2):\n'
                        f'    while not pathlib.Path({str(gate)!r}).exists(): time.sleep(.01)\n'
                        f'    pathlib.Path({str(gate)!r}).unlink()\n'
                        '    for n in range(120): print(n,flush=True)\n'
                        '    value=i+10\n'
                        '    print(value,flush=True)\n'
                        'time.sleep(20)\n')
        name = "idle-review"
        self.sessions.add(name)
        with concurrent.futures.ThreadPoolExecutor() as pool:
            start = pool.submit(self.cli, name, "py", "start", str(path), "--break", f"{path}:7")
            gate.touch()
            self.assertEqual(start.result(timeout=30)["location"]["line"], 7)
        self.assertIn("timeout", self.cli(name, "continue", "--timeout", "1", ok=False)["error"])
        self.assertIn("timeout", self.cli(name, "continue", "--timeout", "1", ok=False)["error"])
        gate.touch()
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            rows = self.cli(name, "status")["sessions"]
            if any(r["name"] == name and r["stopped"] for r in rows):
                break
            time.sleep(.05)
        self.assertEqual(self.cli(name, "eval", "value")["value"], "11")
        self.assertEqual(self.cli(name, "breaks")["stops"][0]["hits"], 2)
        self.cli(name, "close")
        self.sessions.remove(name)

    def test_03_session_path_and_timeout_negatives(self):
        outside = self.home / "outside"
        outside.mkdir()
        marker = outside / "keep"
        marker.write_text("untouched")
        root = self.home / ".agent-debugger/sessions"
        root.mkdir(exist_ok=True)
        (root / "linked").symlink_to(outside, target_is_directory=True)
        for action in [("close",), ("node", "start", "missing.js")]:
            self.assertFalse(self.cli("linked", *action, ok=False)["ok"])
        self.assertEqual(marker.read_text(), "untouched")
        self.assertEqual(list(outside.iterdir()), [marker])
        self.assertFalse(any(s["name"] == "linked" for s in self.cli("unused", "status")["sessions"]))
        for name in ["/tmp", "../outside", "a/", "a\\b"]:
            self.assertFalse(self.cli(name, "close", ok=False)["ok"])
        result = subprocess.run([str(BIN), "continue", "--timeout", str(2**64-1)],
                                env=self.env, capture_output=True, timeout=5)
        self.assertNotEqual(result.returncode, 0)

    def test_04_browser_and_all_bridge_partial_clients(self):
        chrome = find_chrome()
        if chrome is None:
            self.skipTest("Chrome unavailable")
        class QuietHandler(http.server.SimpleHTTPRequestHandler):
            def log_message(self, *args):
                pass
        handler = functools.partial(QuietHandler, directory=str(ROOT / "examples/browser-demo"))
        type(self).http = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        threading.Thread(target=self.http.serve_forever, daemon=True).start()
        cdp_port = free_port()
        url = f"http://127.0.0.1:{self.http.server_port}/index.html"
        type(self).chrome = subprocess.Popen([chrome, "--headless", "--disable-gpu", "--no-first-run",
            f"--remote-debugging-port={cdp_port}", f"--user-data-dir={self.home}/chrome", url],
            stdout=self.log, stderr=self.log)
        deadline = time.monotonic() + 20
        while True:
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{cdp_port}/json/list", timeout=1) as response:
                    if any(t.get("url") == url for t in json.load(response)):
                        break
            except OSError:
                pass
            if time.monotonic() > deadline:
                self.fail("Chrome did not expose test tab")
            time.sleep(.1)
        name = "browser-review"
        self.sessions.add(name)
        self.cli(name, "browser", "attach", "--port", str(cdp_port), "--tab", url,
                 "--logpoint", "app.js:3:sum={sum}")
        self.assertTrue(self.cli(name, "reload", "--timeout", "2")["reloaded"])
        self.assertIn("no stopped thread", self.cli(name, "step", ok=False)["error"])
        specs = {
            "frame-java": ["java", "start", "--main", "Repeat", "--cp", str(self.fixture), "--break", "Repeat:5"],
            "frame-py": ["py", "start", str(self.fixture / "repeat.py"), "--break", f"{self.fixture}/repeat.py:4"],
            "frame-node": ["node", "start", str(self.fixture / "repeat.js"), "--break", f"{self.fixture}/repeat.js:4"],
        }
        for session, args in specs.items():
            self.sessions.add(session)
            self.cli(session, *args)
        def probe(session):
            meta = json.loads((self.home / ".agent-debugger/sessions" / session / "session.json").read_text())
            with socket.create_connection(("127.0.0.1", meta["port"]), timeout=2) as slow:
                slow.sendall(b"Content-Length: 10\r\n\r\n{")
                before = time.monotonic()
                self.assertIn("stops", self.cli(session, "breaks", timeout=14))
                self.assertLess(time.monotonic() - before, 10)
            for payload in [b"Content-Length: 2147483647\r\n\r\n", b"x" * 8193]:
                with socket.create_connection(("127.0.0.1", meta["port"]), timeout=2) as sock:
                    sock.sendall(payload)
                    sock.settimeout(6)
                    self.assertTrue(sock.recv(4096))
            self.assertIn("stops", self.cli(session, "breaks"))
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            list(pool.map(probe, [name, *specs]))
        for session in [name, *specs]:
            self.assertTrue(self.cli(session, "close")["confirmed"])
            self.sessions.remove(session)


    def test_05_delayed_attach_stays_running(self):
        """M1: attach with a never-hit stopping break keeps a live running
        session (stopped:false, breaks armed); touching the gate later
        produces a real stop, proving the session stayed attached."""
        self._ensure_idle_fixtures()
        cases = {
            "m1-py": ("py", self.py_idle, 7, "m1go-py"),
            "m1-node": ("node", self.js_idle, 7, "m1go-js"),
            "m1-java": ("java", None, 10, "m1go-java"),
        }
        for name, (lang, idle, line, gatename) in cases.items():
            port = free_port()
            gate = self.fixture / gatename
            try:
                gate.unlink()
            except OSError:
                pass
            if lang == "py":
                target = [str(VENV_PY), "-Xfrozen_modules=off", "-m", "debugpy",
                          "--listen", f"127.0.0.1:{port}", str(idle)]
                spec = [f"{idle}:{line}"]
            elif lang == "node":
                target = ["node", f"--inspect=127.0.0.1:{port}", str(idle)]
                spec = [f"{idle}:{line}"]
            else:
                target = ["java", f"-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:{port}",
                          "-cp", str(self.fixture), "IdleAttach"]
                spec = ["IdleAttach:10"]
            proc = self._launch_target(target, f"m1-{lang}")
            self._wait_log(f"m1-{lang}", "ready")
            time.sleep(1)  # let the debug listener come up (no socket probe:
            # a raw connect would consume debugpy's single session)
            self.sessions.add(name)
            started = time.monotonic()
            if lang == "java":
                data = self.cli(name, "java", "attach", "--port", str(port),
                                "--break", spec[0], "--timeout", "3")
            elif lang == "py":
                data = self.cli(name, "py", "attach", "--port", str(port),
                                "--break", spec[0], "--timeout", "2")
            else:
                data = self.cli(name, "node", "attach", "--port", str(port),
                                "--break", spec[0], "--timeout", "2")
            self.assertTrue(data["running"], f"{lang} attach must stay running")
            self.assertIn("threads", data)
            stops = self.cli(name, "breaks")["stops"]
            self.assertEqual(len(stops), 1)
            self.assertEqual(stops[0]["hits"], 0)
            self.assertIn(stops[0]["state"], ("verified", "pending"))
            row = next(r for r in self.cli(name, "status")["sessions"] if r["name"] == name)
            self.assertTrue(row["alive"])
            self.assertFalse(row["stopped"])
            self.assertIsNone(row["lastStop"])
            # Trigger: gate stays present so the stop re-fires after resume.
            gate.touch()
            self._wait_stopped(name, True)
            ctx = self.cli(name, "context")
            self.assertEqual(ctx["location"]["line"], line)
            resumed = self.cli(name, "continue", "--timeout", "10")
            self.assertEqual(resumed["snapshot"]["location"]["line"], line)
            stepped = self.cli(name, "step", "over", "--timeout", "10")
            self.assertNotEqual(stepped["snapshot"]["location"]["line"], line)
            self.assertEqual(self.cli(name, "eval", "value")["value"], "99")
            self.assertGreaterEqual(self.cli(name, "breaks")["stops"][0]["hits"], 2)
            try:
                gate.unlink()
            except OSError:
                pass
            self.assertTrue(self.cli(name, "close")["confirmed"])
            self.sessions.remove(name)
            self.assertLess(time.monotonic() - started, 120)

    def test_06_attach_negatives_and_launch_timeout(self):
        """M1 preservation: refused ports fail with no session dir (all
        three adapters); launch never-hit still fails; immediate-hit attach
        still snapshots fast; invalid specs arm before wait and stay errors."""
        self._ensure_idle_fixtures()
        for lang, name in [("java", "m1-ref-java"), ("py", "m1-ref-py"), ("node", "m1-ref-node")]:
            port = free_port()
            data = self.cli(name, lang, "attach", "--port", str(port), "--timeout", "2", ok=False)
            self.assertFalse(data["ok"])
            self.assertIn("attach failed", data["error"])
            self._assert_absent(name)
        gate_launch = self.fixture / "m1go-launch"
        try:
            gate_launch.unlink()
        except OSError:
            pass
        gated = self.fixture / "m1_gated.py"
        gated.write_text(
            "import pathlib\nimport time\n"
            f"GATE = pathlib.Path({str(gate_launch)!r})\n"
            "while not GATE.exists():\n    time.sleep(0.05)\n"
            "value = 1\nprint(value, flush=True)\n")
        data = self.cli("m1-launch-timeout", "py", "start", str(gated),
                        "--break", f"{gated}:7", "--timeout", "2", ok=False)
        self.assertFalse(data["ok"])
        self.assertIn("timeout", data["error"])
        self._assert_absent("m1-launch-timeout")
        # Immediate-hit attach still snapshots fast (py).
        port = free_port()
        repeat = self.fixture / "repeat.py"
        proc = self._launch_target(
            [str(VENV_PY), "-Xfrozen_modules=off", "-m", "debugpy",
             "--listen", f"127.0.0.1:{port}", str(repeat)], "m1-hit")
        self._wait_log("m1-hit", "1\n")
        time.sleep(1)
        name = "m1-hit-py"
        self.sessions.add(name)
        started = time.monotonic()
        data = self.cli(name, "py", "attach", "--port", str(port),
                        "--break", f"{repeat}:4", "--timeout", "10")
        self.assertEqual(data["location"]["line"], 4)
        self.assertLess(time.monotonic() - started, 10)
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)
        # Invalid spec arms before the wait: still an error on attach (java).
        port = free_port()
        proc = self._launch_target(
            ["java", f"-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:{port}",
             "-cp", str(self.fixture), "IdleAttach"], "m1-bad")
        self._wait_log("m1-bad", "ready")
        time.sleep(1)
        data = self.cli("m1-bad-spec", "java", "attach", "--port", str(port),
                        "--break", "method:IdleAttach.noSuchMethod", "--timeout", "2", ok=False)
        self.assertFalse(data["ok"])
        self.assertIn("no method", data["error"])
        self._assert_absent("m1-bad-spec")

    def _ensure_idle_fixtures(self):
        if hasattr(self, "py_idle"):
            return
        gate_py = self.fixture / "m1go-py"
        self.py_idle = self.fixture / "m1_idle.py"
        self.py_idle.write_text(
            "import pathlib\nimport time\nprint(\"ready\", flush=True)\n"
            f"GATE = pathlib.Path({str(gate_py)!r})\nwhile True:\n"
            "    if GATE.exists():\n        value = 99\n        print(\"hit\", flush=True)\n"
            "    time.sleep(0.05)\n")
        gate_js = self.fixture / "m1go-js"
        self.js_idle = self.fixture / "m1_idle.js"
        self.js_idle.write_text(
            "const fs = require(\"fs\");\nconsole.log(\"ready\");\n"
            f"const GATE = {str(gate_js)!r};\nasync function main() {{\n"
            "  for (let i = 0; i < 20000; i++) {\n    if (fs.existsSync(GATE)) {\n"
            "      const value = 99;\n      console.log(\"hit\", value);\n    }\n"
            "    await new Promise((r) => setTimeout(r, 50));\n  }\n}\nmain();\n")
        gate_java = self.fixture / "m1go-java"
        (self.fixture / "IdleAttach.java").write_text(
            "import java.nio.file.Files;\nimport java.nio.file.Path;\n"
            "import java.nio.file.Paths;\npublic class IdleAttach {\n"
            "  public static void main(String[] args) throws Exception {\n"
            "    System.out.println(\"ready\");\n"
            f"    Path gate = Paths.get(\"{gate_java}\");\n"
            "    for (int i = 0; i < 6000; i++) {\n"
            "      if (Files.exists(gate)) {\n"
            "        int value = 99;\n"
            "        System.out.println(\"hit \" + value);\n"
            "      }\n"
            "      Thread.sleep(50);\n"
            "    }\n  }\n}\n")
        subprocess.run(["javac", "-g", "-d", str(self.fixture),
                        str(self.fixture / "IdleAttach.java")],
                       check=True, timeout=30)

    def _launch_target(self, cmd, logname):
        fh = (self.fixture / f"{logname}.log").open("w")
        proc = subprocess.Popen(cmd, stdout=fh, stderr=subprocess.STDOUT, cwd=self.fixture)
        def _kill():
            try:
                proc.terminate()
            except Exception:
                pass
            try:
                proc.wait(timeout=10)
            except Exception:
                try:
                    proc.kill()
                except Exception:
                    pass
                try:
                    proc.wait(timeout=10)
                except Exception:
                    pass
            try:
                fh.close()
            except Exception:
                pass
        self.addCleanup(_kill)
        return proc

    def _wait_log(self, logname, marker, timeout=25):
        path = self.fixture / f"{logname}.log"
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                if marker in path.read_text():
                    return
            except OSError:
                pass
            time.sleep(0.1)
        self.fail(f"{logname}.log never printed {marker!r}")

    def _wait_stopped(self, name, want=True, timeout=15):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            rows = self.cli(name, "status")["sessions"]
            if any(r["name"] == name and bool(r["stopped"]) == want for r in rows):
                return
            time.sleep(0.1)
        self.fail(f"session {name} never reached stopped={want}")

    def _assert_absent(self, name):
        self.assertFalse((self.home / ".agent-debugger/sessions" / name).exists(),
                         f"failed session {name} must leave no dir")
        rows = self.cli("unused", "status")["sessions"]
        self.assertFalse(any(s["name"] == name for s in rows))


    def test_07_breaks_add_live_sessions(self):
        """M2: `breaks add` on running py/node/java sessions arms new line
        breaks (persisted to stops.json); duplicates idempotent; invalid
        batches atomic; added stops fire after the gate; paused adds work."""
        self._ensure_idle_fixtures()
        flows = {
            "m2-py": ("py", f"{self.py_idle}:7", f"{self.py_idle}:8", 7, 8, "m1go-py"),
            "m2-node": ("node", f"{self.js_idle}:7", f"{self.js_idle}:8", 7, 8, "m1go-js"),
            "m2-java": ("java", "IdleAttach:10", "IdleAttach:11", 10, 11, "m1go-java"),
        }
        for name, (lang, first, second, line1, line2, gatename) in flows.items():
            port = free_port()
            gate = self.fixture / gatename
            try:
                gate.unlink()
            except OSError:
                pass
            target = self._m1_target(lang, port)
            self._launch_target(target, f"m2-{lang}")
            self._wait_log(f"m2-{lang}", "ready")
            time.sleep(1)
            self.sessions.add(name)
            if lang == "java":
                self.cli(name, "java", "attach", "--port", str(port), "--timeout", "3")
            elif lang == "py":
                self.cli(name, "py", "attach", "--port", str(port), "--timeout", "2")
            else:
                extra = ["--logpoint", f"{self.js_idle}:6:tick={{i}}"]
                self.cli(name, "node", "attach", "--port", str(port), *extra, "--timeout", "2")
            # Add on the running session.
            added = self.cli(name, "breaks", "add", "--break", first)
            self.assertEqual(len(added["added"]), 1)
            self.assertEqual(added["added"][0]["raw"], first)
            self.assertEqual(added["added"][0]["hits"], 0)
            stops_file = self.home / ".agent-debugger/sessions" / name / "stops.json"
            before = stops_file.read_bytes()
            self.assertIn(first, json.loads(before)["breaks"])
            # Duplicate is idempotent, file untouched.
            dup = self.cli(name, "breaks", "add", "--break", first)
            self.assertEqual(dup["added"], [])
            self.assertEqual(stops_file.read_bytes(), before)
            # Invalid batch (valid new + bad spec) changes nothing.
            if lang == "java":
                badraw = "IdleAttach:9999"  # parses, but no code there (class loaded)
            else:
                badraw = f"{first.rsplit(':', 1)[0]}:xx"  # bad line: parse error
            data = self.cli(name, "breaks", "add", "--break", second,
                            "--break", badraw, ok=False)
            self.assertFalse(data["ok"])
            self.assertEqual(stops_file.read_bytes(), before)
            live = self.cli(name, "breaks")["stops"]
            self.assertFalse(any(s["spec"].endswith(f":{line2}") for s in live
                                 if s["kind"] == "break"))
            self.assertIn("threads", self.cli(name, "threads"))
            # Node: same-line startup logpoint conflicts.
            if lang == "node":
                conflict = self.cli(name, "breaks", "add", "--break",
                                    f"{self.js_idle}:6", ok=False)
                self.assertIn("logpoint", conflict["error"])
                self.assertEqual(stops_file.read_bytes(), before)
            # Trigger the added stop, then add while parked.
            gate.touch()
            self._wait_stopped(name, True)
            self.assertEqual(self.cli(name, "context")["location"]["line"], line1)
            parked = self.cli(name, "breaks", "add", "--break", second)
            self.assertEqual(len(parked["added"]), 1)
            resumed = self.cli(name, "continue", "--timeout", "10")
            self.assertEqual(resumed["snapshot"]["location"]["line"], line2)
            # Convergence: status.armed == stops.json == live recs.
            file_breaks = json.loads(stops_file.read_text())["breaks"]
            row = next(r for r in self.cli(name, "status")["sessions"] if r["name"] == name)
            self.assertEqual(row["armed"]["breaks"], len(file_breaks))
            recs = [s["spec"] for s in self.cli(name, "breaks")["stops"] if s["kind"] == "break"]
            for raw in file_breaks:
                # Raw is verbatim (abspath or Class form); recs may resolve
                # symlinks (/tmp -> /private/tmp), so match the tail.
                short = raw.split("|")[0].rsplit("/", 1)[-1]
                self.assertTrue(any(r == raw or r.endswith(short) for r in recs),
                                f"{raw} missing from {recs}")
            try:
                gate.unlink()
            except OSError:
                pass
            self.assertTrue(self.cli(name, "close")["confirmed"])
            self.sessions.remove(name)
        # Java diff-cond conflict on a cond-armed session.
        port = free_port()
        gate = self.fixture / "m1go-java"
        try:
            gate.unlink()
        except OSError:
            pass
        self._launch_target(self._m1_target("java", port), "m2-javacond")
        self._wait_log("m2-javacond", "ready")
        time.sleep(1)
        name = "m2-javacond"
        self.sessions.add(name)
        self.cli(name, "java", "attach", "--port", str(port),
                 "--break", "IdleAttach:10|value == 99", "--timeout", "3")
        stops_file = self.home / ".agent-debugger/sessions" / name / "stops.json"
        before = stops_file.read_bytes()
        data = self.cli(name, "breaks", "add", "--break", "IdleAttach:10", ok=False)
        self.assertFalse(data["ok"])
        self.assertIn("already armed", data["error"])
        self.assertEqual(stops_file.read_bytes(), before)
        self.assertIn("threads", self.cli(name, "threads"))
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)

    def test_08_breaks_add_browser(self):
        """M2 browser: add on an idle tab, dup/conflict/invalid-batch
        semantics, reload-triggered stop, paused add, convergence."""
        chrome = find_chrome()
        if chrome is None:
            self.skipTest("Chrome unavailable")

        class QuietHandler(http.server.SimpleHTTPRequestHandler):
            def log_message(self, *args):
                pass
        handler = functools.partial(QuietHandler, directory=str(ROOT / "examples/browser-demo"))
        httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        self.addCleanup(httpd.shutdown)
        self.addCleanup(httpd.server_close)
        cdp_port = free_port()
        url = f"http://127.0.0.1:{httpd.server_port}/index.html"
        proc = subprocess.Popen(
            [chrome, "--headless", "--disable-gpu", "--no-first-run",
             f"--remote-debugging-port={cdp_port}",
             f"--user-data-dir={self.home}/m2chrome", url],
            stdout=self.log, stderr=self.log)
        self.addCleanup(proc.terminate)
        deadline = time.monotonic() + 20
        while True:
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{cdp_port}/json/list", timeout=1) as response:
                    if any(t.get("url") == url for t in json.load(response)):
                        break
            except OSError:
                pass
            if time.monotonic() > deadline:
                self.fail("Chrome did not expose test tab")
            time.sleep(.1)
        name = "m2-browser"
        self.sessions.add(name)
        self.cli(name, "browser", "attach", "--port", str(cdp_port), "--tab", url,
                 "--logpoint", "app.js:15:total={total}")
        added = self.cli(name, "breaks", "add", "--break", "app.js:8")
        self.assertEqual(len(added["added"]), 1)
        self.assertEqual(added["added"][0]["raw"], "app.js:8")
        stops_file = self.home / ".agent-debugger/sessions" / name / "stops.json"
        before = stops_file.read_bytes()
        self.assertIn("app.js:8", json.loads(before)["breaks"])
        dup = self.cli(name, "breaks", "add", "--break", "app.js:8")
        self.assertEqual(dup["added"], [])
        self.assertEqual(stops_file.read_bytes(), before)
        conflict = self.cli(name, "breaks", "add", "--break", "app.js:15", ok=False)
        self.assertFalse(conflict["ok"])
        self.assertIn("logpoint", conflict["error"])
        self.assertEqual(stops_file.read_bytes(), before)
        bad = self.cli(name, "breaks", "add", "--break", "app.js:9",
                       "--break", "nope", ok=False)
        self.assertFalse(bad["ok"])
        self.assertEqual(stops_file.read_bytes(), before)
        live = self.cli(name, "breaks")["stops"]
        self.assertFalse(any(s["spec"] == "app.js:9" and s["kind"] == "break" for s in live))
        stop = self.cli(name, "reload", "--timeout", "10")
        self.assertEqual(stop["snapshot"]["location"]["line"], 8)
        ctx = self.cli(name, "context")
        self.assertEqual(ctx["location"]["line"], 8)
        parked = self.cli(name, "breaks", "add", "--break", "app.js:2")
        self.assertEqual(len(parked["added"]), 1)
        onward = self.cli(name, "continue", "--timeout", "10")
        self.assertEqual(onward["snapshot"]["location"]["line"], 2)
        file_breaks = json.loads(stops_file.read_text())["breaks"]
        row = next(r for r in self.cli(name, "status")["sessions"] if r["name"] == name)
        self.assertEqual(row["armed"]["breaks"], len(file_breaks))
        recs = [s["spec"] for s in self.cli(name, "breaks")["stops"] if s["kind"] == "break"]
        for raw in file_breaks:
            self.assertIn(raw, recs)
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)

    def _ensure_py_path_fixtures(self):
        """Nested layout mirroring the basename repro: a probe buried under
        <root>/tests/workflows/nodes plus fast/sleeping targets and a
        stdlib file (justMyCode-excluded, so breaks there stay pending)."""
        if hasattr(self, "py_probe"):
            return
        nested = self.fixture / "pysrc/tests/workflows/nodes"
        nested.mkdir(parents=True, exist_ok=True)
        self.py_probe = nested / "test_probe_unit.py"
        self.py_probe.write_text(
            "import time\nfor i in range(100):\n    value = i + 1\n"
            "    print(value, flush=True)\n    time.sleep(.15)\n")
        self.py_fast = self.fixture / "py_fast.py"
        self.py_fast.write_text('print("done", flush=True)\n')
        self.py_pending_idle = self.fixture / "py_pending_idle.py"
        self.py_pending_idle.write_text(
            'import time\nprint("ready", flush=True)\nwhile True:\n'
            "    time.sleep(0.2)\n")
        out = subprocess.run([str(VENV_PY), "-c",
                              "import threading; print(threading.__file__)"],
                             capture_output=True, text=True, timeout=30)
        self.py_stdlib = out.stdout.strip()
        with open(self.py_stdlib) as f:
            nlines = len(f.read().splitlines())
        self.py_stdlib_line = min(100, nlines)

    def test_09_py_basename_without_src_fails_fast(self):
        """User repro: a bare basename that only exists under a nested tree
        fails before the target runs — naming the raw path, the cwd attempt
        and the --src guidance (never a bare `target exited`)."""
        self._ensure_py_path_fixtures()
        started = time.monotonic()
        data = self.cli("py-bare-nosrc", "py", "start", str(self.py_probe),
                        "--break", "test_probe_unit.py:3", "--timeout", "10",
                        ok=False)
        elapsed = time.monotonic() - started
        self.assertFalse(data["ok"])
        err = data["error"]
        self.assertIn("no such file", err)
        self.assertIn("test_probe_unit.py", err)
        self.assertIn("--src", err)
        self.assertNotIn("target exited", err)
        self.assertLess(elapsed, 15)
        self._assert_absent("py-bare-nosrc")

    def test_10_py_basename_with_src_and_nested_relative_stop(self):
        """Basename + --src resolves to a real stop (locals/eval live); a
        full nested path relative to the cwd keeps working (cwd wins)."""
        self._ensure_py_path_fixtures()
        name = "py-path-src"
        self.sessions.add(name)
        data = self.cli(name, "py", "start", str(self.py_probe),
                        "--src", str(self.fixture / "pysrc"),
                        "--break", "test_probe_unit.py:4", "--timeout", "15")
        self.assertEqual(data["location"]["line"], 4)
        self.assertEqual(self.cli(name, "eval", "value")["value"], "1")
        self.assertIn("value",
                      {v["name"] for v in self.cli(name, "vars")["locals"]})
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)
        name = "py-path-rel"
        self.sessions.add(name)
        rel = os.path.join("pysrc", "tests", "workflows", "nodes",
                           "test_probe_unit.py")
        data = self.cli(name, "py", "start", str(self.py_probe),
                        "--break", f"{rel}:4", "--timeout", "15",
                        cwd=str(self.fixture))
        self.assertEqual(data["location"]["line"], 4)
        self.assertEqual(self.cli(name, "eval", "value")["value"], "1")
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)

    def test_11_py_unverified_exit_and_timeout_report_pending(self):
        """Existing file, never-binding line (stdlib excluded by justMyCode):
        a fast-exiting target reports the pending spec/detail plus the
        wrong-path hint; a launch first-stop timeout does the same."""
        self._ensure_py_path_fixtures()
        spec = f"{self.py_stdlib}:{self.py_stdlib_line}"
        data = self.cli("py-unverified-exit", "py", "start", str(self.py_fast),
                        "--break", spec, "--timeout", "10", ok=False)
        self.assertFalse(data["ok"])
        err = data["error"]
        self.assertIn("target exited", err)
        self.assertIn("unresolved breakpoints", err)
        self.assertIn(os.path.basename(self.py_stdlib), err)
        self.assertIn("excluded", err)  # debugpy's pending detail
        self.assertIn("executable", err)  # wrong-path hint
        self._assert_absent("py-unverified-exit")
        data = self.cli("py-unverified-timeout", "py", "start",
                        str(self.py_pending_idle),
                        "--break", spec, "--timeout", "2", ok=False)
        self.assertFalse(data["ok"])
        self.assertIn("timeout", data["error"])
        self.assertIn("unresolved breakpoints", data["error"])
        self.assertIn(os.path.basename(self.py_stdlib), data["error"])
        self._assert_absent("py-unverified-timeout")

    def test_11b_py_logpoint_only_fast_exit(self):
        """Logpoints-only fast exit keeps the intended collect-logs shape:
        the error-file truth wins when setup fails, but a clean fast run
        never surfaces an outer ok:false. Either live shape is accepted
        (threads when the first read wins the race, warning+logs when the
        exit wins it); both carry target identity, and the collect shape
        carries the already-exited warning with the target output."""
        self._ensure_py_path_fixtures()
        name = "py-logpoint-fast"
        self.sessions.add(name)
        data = self.cli(name, "py", "start", str(self.py_fast),
                        "--logpoint", f"{self.py_fast}:1:fast-exit-marker",
                        "--timeout", "10")
        self.assertIn("requestedTarget", data)
        self.assertIn("targetIdentity", data)
        self.assertNotIn("observedTarget", data)
        if "logs" in data:
            self.assertIn("already exited", data.get("warning", ""))
            lines = data["logs"]["lines"]
            self.assertTrue(any("done" in ln or "fast-exit-marker" in ln
                                for ln in lines), lines)
        else:
            self.assertIn("threads", data)
        try:
            self.cli(name, "close", timeout=85)
        except Exception:
            pass
        self.sessions.remove(name)
        self._assert_absent(name)

    def test_12_py_attach_pending_survives(self):
        """Attach with a never-binding (pending) break stays a live running
        session; bare `breaks` exposes the pending record."""
        self._ensure_py_path_fixtures()
        port = free_port()
        self._launch_target(
            [str(VENV_PY), "-Xfrozen_modules=off", "-m", "debugpy",
             "--listen", f"127.0.0.1:{port}", str(self.py_pending_idle)],
            "py-pending-idle")
        self._wait_log("py-pending-idle", "ready")
        time.sleep(1)
        name = "py-pending-attach"
        self.sessions.add(name)
        data = self.cli(name, "py", "attach", "--port", str(port),
                        "--break",
                        f"{self.py_stdlib}:{self.py_stdlib_line}",
                        "--timeout", "2")
        self.assertTrue(data["running"])
        stops = self.cli(name, "breaks")["stops"]
        self.assertEqual(len(stops), 1)
        self.assertEqual(stops[0]["state"], "pending")
        self.assertTrue(stops[0]["spec"].endswith(
            f"{os.path.basename(self.py_stdlib)}:{self.py_stdlib_line}"))
        self.assertIn("threads", self.cli(name, "threads"))
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)

    def _ensure_py_line_fixtures(self):
        """Line-slide layout: a comment on line 2 (debugpy slides it onto
        nearby executable code) plus a second file for wrong-file specs."""
        if hasattr(self, "py_slide"):
            return
        self.py_slide = self.fixture / "py_slide.py"
        self.py_slide.write_text(
            "import time\n# just a comment\nvalue = 1\n"
            "print(value, flush=True)\ntime.sleep(20)\n")
        self.py_other = self.fixture / "py_other.py"
        self.py_other.write_text("a = 1\nb = 2\nprint(a + b, flush=True)\n")

    def test_13_py_invalid_line_fail_fast(self):
        """Existing file, line past EOF (same-file and wrong-file 99999):
        fails before the target runs — `no such line` naming the spec,
        never a bare `target exited`."""
        self._ensure_py_line_fixtures()
        cases = [("py-bad-same", str(self.py_slide), f"{self.py_slide}:99999"),
                 ("py-bad-other", str(self.py_slide), f"{self.py_other}:99999"),
                 ("py-bad-zero", str(self.py_slide), f"{self.py_slide}:0")]
        for name, program, spec in cases:
            started = time.monotonic()
            data = self.cli(name, "py", "start", program,
                            "--break", spec, "--timeout", "10", ok=False)
            self.assertFalse(data["ok"])
            err = data["error"]
            self.assertIn("no such line", err)
            self.assertIn(spec.rsplit(":", 1)[1], err)
            self.assertNotIn("target exited", err)
            self.assertLess(time.monotonic() - started, 15)
            self._assert_absent(name)

    def test_14_py_comment_slide_reports_slid_and_hits(self):
        """In-range comment line: DAP slides onto nearby executable code —
        `breaks` reports state slid with the real line, the stop lands
        there, and hits count the bound line."""
        self._ensure_py_line_fixtures()
        name = "py-slide"
        self.sessions.add(name)
        data = self.cli(name, "py", "start", str(self.py_slide),
                        "--break", f"{self.py_slide}:2", "--timeout", "15")
        line = data["location"]["line"]
        self.assertNotEqual(line, 2)
        stops = self.cli(name, "breaks")["stops"]
        self.assertEqual(len(stops), 1)
        self.assertEqual(stops[0]["state"], "slid")
        self.assertIn(f"slid to line {line}", stops[0]["detail"])
        self.assertGreaterEqual(stops[0]["hits"], 1)
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)

    def test_15_py_valid_unreached_plain_timeout_or_running(self):
        """Valid exact line, never reached: launch keeps its plain timeout
        (no invalid-line hint, no unresolved note); attach stays running
        with the stop armed."""
        gate = self.fixture / "m1go-line15"
        try:
            gate.unlink()
        except OSError:
            pass
        target = self.fixture / "py_gated15.py"
        target.write_text(
            "import pathlib\nimport time\n"
            f"GATE = pathlib.Path({str(gate)!r})\nwhile not GATE.exists():\n"
            "    time.sleep(0.05)\nvalue = 1\nprint(value, flush=True)\n")
        data = self.cli("py-gated15", "py", "start", str(target),
                        "--break", f"{target}:6", "--timeout", "2", ok=False)
        self.assertFalse(data["ok"])
        self.assertIn("timeout", data["error"])
        self.assertNotIn("no such line", data["error"])
        self.assertNotIn("unresolved breakpoints", data["error"])
        self._assert_absent("py-gated15")
        self._ensure_py_path_fixtures()
        port = free_port()
        self._launch_target(
            [str(VENV_PY), "-Xfrozen_modules=off", "-m", "debugpy",
             "--listen", f"127.0.0.1:{port}", str(self.py_pending_idle)],
            "py-armed-idle")
        self._wait_log("py-armed-idle", "ready")
        time.sleep(1)
        name = "py-armed-attach"
        self.sessions.add(name)
        data = self.cli(name, "py", "attach", "--port", str(port),
                        "--break", f"{self.py_pending_idle}:2",
                        "--timeout", "2")
        self.assertTrue(data["running"])
        stops = self.cli(name, "breaks")["stops"]
        self.assertEqual(len(stops), 1)
        self.assertEqual(stops[0]["hits"], 0)
        self.assertIn(stops[0]["state"], ("verified", "pending"))
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)

    def test_16_py_cothread_costop_stable_and_sequential(self):
        """P5: barrier-synced threads hit one shared line ~simultaneously.
        Whoever parks first stays parked while the other's hit still counts
        (no phantom re-park, no stale `stack unavailable`); a gated second
        round proves post-barrier genuine stops still park."""
        gate = self.fixture / "m1go-co"
        try:
            gate.unlink()
        except OSError:
            pass
        target = self.fixture / "py_cothread.py"
        src = [
            "import pathlib\n",
            "import threading\n",
            "import time\n",
            f"GATE = pathlib.Path({str(gate)!r})\n",
            "B = threading.Barrier(2)\n",
            'print("ready", flush=True)\n',
            "def w(name):\n",
            "    for _ in range(2):\n",
            "        B.wait()\n",
            "        tick = 1\n",
            '        print(name, tick, flush=True)\n',
            "        while not GATE.exists():\n",
            "            time.sleep(0.01)\n",
            "        tick = 2\n",
            '        print(name, tick, flush=True)\n',
            "    time.sleep(30)\n",
            "ta = threading.Thread(target=w, args=(\"a\",))\n",
            "tb = threading.Thread(target=w, args=(\"b\",))\n",
            "ta.start()\n",
            "tb.start()\n",
            "ta.join()\n",
            "tb.join()\n",
            'print("done", flush=True)\n',
        ]
        target.write_text("".join(src))
        line_x = src.index("        tick = 1\n") + 1
        line_y = src.index("        tick = 2\n") + 1

        def hits(sess):
            stops = self.cli(sess, "breaks")["stops"]
            return {s["spec"].rsplit(":", 1)[-1]: s["hits"] for s in stops}

        def parked_tid(sess):
            ctx = self.cli(sess, "context")
            cur = [t["id"] for t in ctx["threads"] if t.get("current")]
            return (cur[0] if cur else None), ctx["location"]

        def await_hits(sess, line, want, budget=20):
            # Either shape converges: a queued co-stop is consumed by the
            # serve loop between commands, while a partner frozen pre-line
            # is released by a resume into a genuine post-barrier park.
            deadline = time.monotonic() + budget
            while time.monotonic() < deadline:
                if hits(sess).get(str(line), 0) >= want:
                    return
                self.cli(sess, "continue", "--timeout", "4", ok=False)
                time.sleep(0.1)
            self.fail(f"line {line} never reached {want} hits: "
                      f"{hits(sess)}")

        name = "py-cothread"
        self.sessions.add(name)
        data = self.cli(name, "py", "start", str(target),
                        "--break", f"{target}:{line_x}",
                        "--break", f"{target}:{line_y}", "--timeout", "15")
        self.assertEqual(data["location"]["line"], line_x)
        await_hits(name, line_x, 2)
        # Stability: extra commands pump the serve loop (the old flip path)
        # yet the first park must not move, with exactly one hit per thread.
        tid_first, _ = parked_tid(name)
        self.assertIsNotNone(tid_first)
        self.assertIn("threads", self.cli(name, "threads"))
        tid_still, loc = parked_tid(name)
        self.assertEqual(tid_still, tid_first)
        self.assertEqual(loc["line"], line_x)
        h = hits(name)
        self.assertEqual(h.get(str(line_x)), 2)
        # Nothing ahead (both gate-waiting): a plain timeout — never a
        # phantom re-park or "stack is unavailable".
        err = self.cli(name, "continue", "--timeout", "3", ok=False)["error"]
        self.assertIn("timeout", err)
        self.assertNotIn("unavailable", err)
        # Sequential genuine stop: the gated round parks after the barrier.
        gate.touch()
        self._wait_stopped(name, True)
        _, loc_b = parked_tid(name)
        self.assertEqual(loc_b["line"], line_y)
        self.assertEqual(loc_b["method"], "w")
        await_hits(name, line_y, 2)
        h = hits(name)
        self.assertEqual(h.get(str(line_y)), 2)
        self.assertEqual(h.get(str(line_x)), 2)
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)

    def _ensure_m2_java_fixtures(self):
        """M2 java fixtures: a static-field loop target (COUNT + same-line
        break/logpoint truth) and a long-running arm-failure target."""
        if hasattr(self, "m2_same"):
            return
        self.m2_same = self.fixture / "M2Same.java"
        self.m2_same.write_text(
            "public class M2Same {\n"
            "  static int COUNT = 42;\n"
            "  public static void main(String[] args) throws Exception {\n"
            "    for (int i = 0; i < 30; i++) {\n"
            "      int value = i + 1;\n"
            "      System.out.println(value);\n"
            "      Thread.sleep(100);\n"
            "    }\n  }\n}\n")
        self.m2_leak = self.fixture / "M2Leak.java"
        self.m2_leak.write_text(
            "public class M2Leak {\n"
            "  public static void main(String[] args) throws Exception {\n"
            "    for (int i = 0; i < 6000; i++) {\n"
            "      int value = i;\n"
            "      System.out.println(value);\n"
            "      Thread.sleep(50);\n"
            "    }\n  }\n}\n")
        subprocess.run(["javac", "-g", "-d", str(self.fixture),
                        str(self.m2_same), str(self.m2_leak)],
                       check=True, timeout=30)

    def _java_procs(self, marker):
        out = subprocess.run(["ps", "aux"], capture_output=True, text=True,
                             timeout=10).stdout
        return [l for l in out.splitlines()
                if marker in l and "grep" not in l]

    def test_17_m2_java_setup_and_validation(self):
        """M2: bad main/cp error quotes target stderr; bad method arm
        leaves no target process and the name is reusable; malformed
        conds (`x ==`, multi-op) and line 0 fail fast."""
        self._ensure_m2_java_fixtures()
        data = self.cli("m2-badmain", "java", "start", "--main", "M2NoSuch",
                        "--cp", str(self.fixture), "--break", "M2Same:6",
                        "--timeout", "8", ok=False)
        self.assertFalse(data["ok"])
        self.assertIn("target output", data["error"])
        self.assertIn("M2NoSuch", data["error"])
        self._assert_absent("m2-badmain")
        data = self.cli("m2-leak", "java", "start", "--main", "M2Leak",
                        "--cp", str(self.fixture),
                        "--break", "method:M2Leak.noSuch", "--timeout", "8",
                        ok=False)
        self.assertFalse(data["ok"])
        self.assertIn("no method", data["error"])
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and self._java_procs("M2Leak"):
            time.sleep(0.2)
        self.assertEqual(self._java_procs("M2Leak"), [])
        self._assert_absent("m2-leak")
        name = "m2-leak"
        self.sessions.add(name)
        data = self.cli(name, "java", "start", "--main", "M2Leak",
                        "--cp", str(self.fixture), "--break", "M2Leak:5",
                        "--timeout", "10")
        self.assertEqual(data["location"]["line"], 5)
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)
        for spec, hint in [("M2Same:6|x ==", "condition"),
                           ("M2Same:6|i > 0 == true", "condition"),
                           ("M2Same:6|== 5", "condition")]:
            data = self.cli("m2-badcond", "java", "start", "--main", "M2Same",
                            "--cp", str(self.fixture), "--break", spec,
                            "--timeout", "5", ok=False)
            self.assertFalse(data["ok"])
            self.assertIn(hint, data["error"])
            self.assertNotIn("target exited", data["error"])
            self._assert_absent("m2-badcond")
        data = self.cli("m2-badline", "java", "start", "--main", "M2Same",
                        "--cp", str(self.fixture), "--break", "M2Same:0",
                        "--timeout", "5", ok=False)
        self.assertFalse(data["ok"])
        self.assertIn("bad line", data["error"])
        self._assert_absent("m2-badline")

    def test_18_m2_java_static_and_sameline(self):
        """M2: static COUNT evals; same-line break+logpoint plants one JDI
        request (break verified, logpoint shadowed, one hit per pass, zero
        logs); live `breaks add` on a logpoint line rejects atomically."""
        self._ensure_m2_java_fixtures()
        name = "m2-sameline"
        self.sessions.add(name)
        data = self.cli(name, "java", "start", "--main", "M2Same",
                        "--cp", str(self.fixture), "--break", "M2Same:6",
                        "--logpoint", "M2Same:6:hit={value}", "--timeout", "10")
        self.assertEqual(data["location"]["line"], 6)
        stops = {s["kind"]: s for s in self.cli(name, "breaks")["stops"]}
        self.assertEqual(stops["break"]["state"], "verified")
        self.assertEqual(stops["break"]["hits"], 1)
        self.assertEqual(stops["logpoint"]["state"], "shadowed")
        self.assertIn("shadowed by breakpoint", stops["logpoint"]["detail"])
        self.assertEqual(stops["logpoint"]["hits"], 0)
        self.assertEqual(self.cli(name, "eval", "COUNT")["value"], "42")
        resumed = self.cli(name, "continue", "--timeout", "10")
        self.assertEqual(resumed["snapshot"]["location"]["line"], 6)
        stops = {s["kind"]: s for s in self.cli(name, "breaks")["stops"]}
        self.assertEqual(stops["break"]["hits"], 2)
        self.assertEqual(stops["logpoint"]["hits"], 0)
        self.assertEqual(self.cli(name, "logs")["total"], 0)
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)
        lone = "m2-logonly"
        self.sessions.add(lone)
        self.cli(lone, "java", "start", "--main", "M2Same",
                 "--cp", str(self.fixture), "--break", "M2Same:5",
                 "--logpoint", "M2Same:6:hit={value}", "--timeout", "10")
        stops_file = self.home / ".agent-debugger/sessions" / lone / "stops.json"
        before = stops_file.read_bytes()
        data = self.cli(lone, "breaks", "add", "--break", "M2Same:6", ok=False)
        self.assertFalse(data["ok"])
        self.assertIn("logpoint", data["error"])
        self.assertEqual(stops_file.read_bytes(), before)
        self.assertTrue(self.cli(lone, "close")["confirmed"])
        self.sessions.remove(lone)

    def _ensure_m3_node_fixtures(self):
        """M3: nested basename target, catch-scope target, dup basenames."""
        if hasattr(self, "m3_deep"):
            return
        if shutil.which("node") is None:
            raise unittest.SkipTest("node unavailable")
        sub = self.fixture / "m3src/sub"
        sub.mkdir(parents=True, exist_ok=True)
        self.m3_deep = sub / "deep.js"
        self.m3_deep.write_text(
            'const fs = require("fs");\nconsole.log("ready");\n'
            'async function main() {\n'
            '  for (let i = 0; i < 20000; i++) {\n    if (i === 3) {\n'
            '      const marker = i * 2;\n      console.log("hit", marker);\n'
            '    }\n'
            '    await new Promise((r) => setTimeout(r, 50));\n  }\n}\nmain();\n')
        self.m3_catch = self.fixture / "m3_catch.js"
        self.m3_catch.write_text(
            'function boom() {\n  try {\n    throw new Error("kaput");\n'
            '  } catch (e) {\n    const after = 1;\n'
            '    console.log("caught", e.message, after);\n  }\n}\nboom();\n')
        self.m3_ts = self.fixture / "m3_app.ts"
        self.m3_ts.write_text('const x: number = 1;\nconsole.log(x);\n')
        for side in ("a", "b"):
            d = self.fixture / "m3src" / side
            d.mkdir(parents=True, exist_ok=True)
            (d / "dup.js").write_text('console.log("dup");\n')

    def test_19_m3_node_src_resolution_and_startup_conflicts(self):
        """M3: bare basename + --src stops; nested cwd-relative works;
        identical startup repeats idempotent; diff-cond / same-line
        logpoint / ambiguous / missing all fail fast with no session dir."""
        self._ensure_m3_node_fixtures()
        root = self.fixture / "m3src"
        name = "m3-nodesrc"
        self.sessions.add(name)
        data = self.cli(name, "node", "start", str(self.m3_deep),
                        "--src", str(root), "--break", "deep.js:6",
                        "--timeout", "15")
        self.assertEqual(data["location"]["line"], 6)
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)
        # Nested path relative to the cwd resolves without --src.
        name = "m3-noderel"
        self.sessions.add(name)
        rel = os.path.join("m3src", "sub", "deep.js")
        data = self.cli(name, "node", "start", str(self.m3_deep),
                        "--break", f"{rel}:6", "--timeout", "15",
                        cwd=str(self.fixture))
        self.assertEqual(data["location"]["line"], 6)
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)
        # Identical startup repeats collapse to one armed stop.
        name = "m3-nodedup"
        self.sessions.add(name)
        data = self.cli(name, "node", "start", str(self.m3_deep),
                        "--src", str(root),
                        "--break", "deep.js:6", "--break", "deep.js:6",
                        "--timeout", "15")
        self.assertEqual(data["location"]["line"], 6)
        stops = self.cli(name, "breaks")["stops"]
        self.assertEqual(len([s for s in stops if s["kind"] == "break"]), 1)
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)
        # Conflicts fail fast before the target runs (no session dir left).
        for args in (
            ["--break", "deep.js:6", "--break", "deep.js:6|i > 1"],
            ["--break", "deep.js:6", "--logpoint", "deep.js:6:t={i}"],
            ["--break", "dup.js:1"],
            ["--break", "ghost.js:2"],
        ):
            bad = self.cli("m3-nodeneg", "node", "start", str(self.m3_deep),
                           "--src", str(root), *args, "--timeout", "10",
                           ok=False)
            self.assertFalse(bad["ok"])
            self.assertTrue(
                (("conflicting" in bad["error"]) and ("deep.js:6" in args[1]))
                or (("ambiguous" in bad["error"]) and ("dup.js" in args[1]))
                or (("no such file" in bad["error"]) and ("ghost" in args[1])),
                f"{args}: {bad['error']}")
            self.assertNotIn("target exited", bad["error"])
            self._assert_absent("m3-nodeneg")

    def test_20_m3_node_catch_scope_and_bad_node(self):
        """M3: catch bindings surface in vars+eval; a bad --node binary
        (js and .ts) fails cleanly naming the binary, never a bare crash."""
        self._ensure_m3_node_fixtures()
        name = "m3-nodecatch"
        self.sessions.add(name)
        data = self.cli(name, "node", "start", str(self.m3_catch),
                        "--break", f"{self.m3_catch}:5", "--timeout", "15")
        self.assertEqual(data["location"]["line"], 5)
        names = {v["name"] for v in self.cli(name, "vars")["locals"]}
        self.assertIn("e", names)
        self.assertIn("after", names)
        self.assertEqual(self.cli(name, "eval", "e.message")["value"], "kaput")
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)
        for prog in (self.m3_catch, self.m3_ts):
            bad = self.cli("m3-badnode", "node", "start", str(prog),
                           "--node", "/nonexistent-node-xyz-123",
                           "--timeout", "10", ok=False)
            self.assertFalse(bad["ok"])
            self.assertIn("cannot run", bad["error"])
            self.assertIn("/nonexistent-node-xyz-123", bad["error"])
            self._assert_absent("m3-badnode")

    def _ensure_m3_step_fixture(self):
        """M3: one-shot head (startup break + 5s sleep a step-over hangs
        in) followed by a gated loop tail (live-added break + logpoint)."""
        if hasattr(self, "m3_step"):
            return
        if shutil.which("node") is None:
            raise unittest.SkipTest("node unavailable")
        gate = self.fixture / "m1go-js"
        self.m3_step = self.fixture / "m3_step.js"
        self.m3_step.write_text(
            'const fs = require("fs");\nconsole.log("ready");\n'
            f'const GATE = {str(gate)!r};\nasync function main() {{\n'
            '  const once = 1;\n'
            '  await new Promise((r) => setTimeout(r, 5000));\n'
            '  for (let i = 0; i < 20000; i++) {\n    const x = i;\n'
            '    if (fs.existsSync(GATE)) {\n      console.log("hit", x);\n'
            '    }\n'
            '    await new Promise((r) => setTimeout(r, 50));\n  }\n}\nmain();\n')

    def test_21_m3_node_step_timeout_and_continue_truth(self):
        """M3: a step that never lands times out and clears the step flag —
        a later logpoint-only pause still auto-resumes (proving no stuck
        step park), the next breakpoint classifies with hits, and an
        immediate re-stop after continue reports stopped truth."""
        self._ensure_m3_step_fixture()
        name = "m3-nodestep"
        gate = self.fixture / "m1go-js"
        try:
            gate.unlink()
        except OSError:
            pass
        self.sessions.add(name)
        # Line 6 = one-shot 5s await; line 10 = gate-guarded logpoint.
        data = self.cli(name, "node", "start", str(self.m3_step),
                        "--break", f"{self.m3_step}:6",
                        "--logpoint", f"{self.m3_step}:10:hit={{x}}",
                        "--timeout", "15")
        self.assertEqual(data["location"]["line"], 6)
        gate.touch()
        # Step over the 5s sleep with a 2s timeout: the step never lands.
        data = self.cli(name, "step", "over", "--timeout", "2", ok=False)
        self.assertFalse(data["ok"])
        self.assertIn("timeout", data["error"])
        threads = self.cli(name, "threads")
        self.assertTrue(threads["running"])
        # The flag did not stick: the one-shot break never fires again and
        # logpoint pauses auto-resume, so this continue (logpoint-only
        # ahead) burns its timeout — a stuck awaitingStep would park at
        # line 10 and return a stop instead.
        data = self.cli(name, "continue", "--timeout", "8", ok=False)
        self.assertFalse(data["ok"])
        self.assertIn("timeout", data["error"])
        stops = {s["kind"]: s for s in self.cli(name, "breaks")["stops"]}
        self.assertGreaterEqual(stops["logpoint"]["hits"], 1)
        # Breakpoints still classify: add line 8, continue lands on it as a
        # real stop (stopInfo null, hits increment, changed names x).
        added = self.cli(name, "breaks", "add", "--break", f"{self.m3_step}:8")
        self.assertEqual(len(added["added"]), 1)
        resumed = self.cli(name, "continue", "--timeout", "10")
        self.assertEqual(resumed["snapshot"]["location"]["line"], 8)
        self.assertIsNone(resumed["stopInfo"])
        self.assertIn("x", resumed["changed"])
        stops = self.cli(name, "breaks")["stops"]
        # Specs carry canonical paths (/private/... on macOS): match tails.
        live = next(s for s in stops
                    if s["kind"] == "break" and s["spec"].endswith("m3_step.js:8"))
        # The loop spins every 50ms, so a park may already have landed in
        # the add->continue gap; either way the stop classifies (hits >= 1).
        self.assertGreaterEqual(live["hits"], 1)
        # The one-shot head break never refired: no phantom parks anywhere.
        head = next(s for s in stops
                    if s["kind"] == "break" and s["spec"].endswith("m3_step.js:6"))
        self.assertEqual(head["hits"], 1)
        # Immediate re-stop after continue: status tells the truth.
        again = self.cli(name, "continue", "--timeout", "10")
        self.assertEqual(again["snapshot"]["location"]["line"], 8)
        row = next(r for r in self.cli(name, "status")["sessions"] if r["name"] == name)
        self.assertTrue(row["stopped"])
        self.assertEqual(row["lastStop"]["line"], 8)
        try:
            gate.unlink()
        except OSError:
            pass
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)

    def test_22_m3_browser_catch_and_step_smoke(self):
        """M3 browser (Chrome only): catch bindings in vars+eval, step
        lands on the next line, status truth after the stop."""
        chrome = find_chrome()
        if chrome is None:
            self.skipTest("Chrome unavailable")

        class QuietHandler(http.server.SimpleHTTPRequestHandler):
            def log_message(self, *args):
                pass
        webdir = self.fixture / "m3web"
        webdir.mkdir(exist_ok=True)
        (webdir / "catchdemo.js").write_text(
            'function load() {\n  let status = "ok";\n  try {\n'
            '    throw new Error("boom");\n  } catch (e) {\n'
            '    const note = "caught";\n'
            '    console.log(note, e.message, status);\n  }\n'
            '  console.log("done", status);\n}\n'
            'window.addEventListener("load", load);\n')
        (webdir / "index.html").write_text(
            '<!doctype html><html><body>m3<script src="catchdemo.js"></script></body></html>\n')
        handler = functools.partial(QuietHandler, directory=str(webdir))
        httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        self.addCleanup(httpd.shutdown)
        self.addCleanup(httpd.server_close)
        cdp_port = free_port()
        url = f"http://127.0.0.1:{httpd.server_port}/index.html"
        proc = subprocess.Popen(
            [chrome, "--headless", "--disable-gpu", "--no-first-run",
             f"--remote-debugging-port={cdp_port}",
             f"--user-data-dir={self.home}/m3chrome", url],
            stdout=self.log, stderr=self.log)
        self.addCleanup(proc.terminate)
        deadline = time.monotonic() + 20
        while True:
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{cdp_port}/json/list", timeout=1) as response:
                    if any(t.get("url") == url for t in json.load(response)):
                        break
            except OSError:
                pass
            if time.monotonic() > deadline:
                self.fail("Chrome did not expose test tab")
            time.sleep(.1)
        name = "m3-browser"
        self.sessions.add(name)
        self.cli(name, "browser", "attach", "--port", str(cdp_port), "--tab", url,
                 "--break", "catchdemo.js:6")
        stop = self.cli(name, "reload", "--timeout", "15")
        self.assertEqual(stop["snapshot"]["location"]["line"], 6)
        names = {v["name"] for v in self.cli(name, "vars")["locals"]}
        self.assertIn("e", names)
        self.assertIn("note", names)
        self.assertIn("status", names)
        self.assertEqual(self.cli(name, "eval", "e.message")["value"], "boom")
        stepped = self.cli(name, "step", "over", "--timeout", "10")
        self.assertEqual(stepped["snapshot"]["location"]["line"], 7)
        row = next(r for r in self.cli(name, "status")["sessions"] if r["name"] == name)
        self.assertTrue(row["stopped"])
        self.assertEqual(row["lastStop"]["line"], 7)
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)

    def _ensure_mod_fixture(self):
        """Project-owned minimal module fixture (no pytest dependency)."""
        if hasattr(self, "mod_runner"):
            return
        pkg = self.fixture / "modpkg"
        pkg.mkdir(exist_ok=True)
        (pkg / "__init__.py").write_text("")
        self.mod_runner = pkg / "runner.py"
        self.mod_runner.write_text(
            "import sys\nimport time\nprint(\"modready\", flush=True)\n"
            "marker = sys.argv[1:]\n"
            "total = 0\n"
            "for i in range(200):\n"
            "    total += i\n"
            "    time.sleep(0.05)\n"
            "print(total, flush=True)\n")

    def test_23_py_module_launch(self):
        """M1: `py start --module dotted.name -- args` launches as python -m
        (real test-body breakpoint + argv passing + cwd import); file launch
        is untouched; bad/ambiguous module forms fail fast with no dir."""
        self._ensure_mod_fixture()
        name = "m1-mod"
        self.sessions.add(name)
        data = self.cli(name, "py", "start", "--module", "modpkg.runner",
                        "--break", f"{self.mod_runner}:5",
                        "--timeout", "15", "--", "--arg", "1", cwd=str(self.fixture))
        self.assertEqual(data["location"]["line"], 5)
        self.assertEqual(self.cli(name, "eval", "marker")["value"], "['--arg', '1']")
        stops_file = self.home / ".agent-debugger/sessions" / name / "stops.json"
        intent = json.loads(stops_file.read_text())
        self.assertEqual(intent["target"].get("module"), "modpkg.runner")
        self.assertIn("requestedTarget", intent)
        self.assertEqual(intent["requestedTarget"].get("module"), "modpkg.runner")
        self.assertIsNone(intent["requestedTarget"].get("pid"))
        row = next(r for r in self.cli(name, "status")["sessions"] if r["name"] == name)
        self.assertEqual(row["target"].get("module"), "modpkg.runner")
        ctx = self.cli(name, "context")
        self.assertEqual(ctx["requestedTarget"].get("module"), "modpkg.runner")
        dbg = ctx["targetIdentity"]["debuggee"]
        self.assertEqual(dbg["confidence"], "protocol-confirmed")
        argv = (dbg.get("osDetails") or {}).get("argv") or []
        self.assertIn("-m", argv)
        self.assertIn("modpkg.runner", argv)
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)
        # Unknown module: launch fails (the break forces module resolution),
        # name reusable.
        data = self.cli("m1-mod-bad", "py", "start", "--module", "nosuchpkg.mod",
                        "--break", f"{self.mod_runner}:5",
                        "--timeout", "10", cwd=str(self.fixture), ok=False)
        self.assertFalse(data["ok"])
        self._assert_absent("m1-mod-bad")
        # Malformed module: CLI fail-fast before any target runs.
        data = self.cli("m1-mod-bad2", "py", "start", "--module", "a-b",
                        cwd=str(self.fixture), ok=False)
        self.assertFalse(data["ok"])
        self.assertIn("module", data["error"])
        self._assert_absent("m1-mod-bad2")
        # File + module together: parse-time conflict (clap exits before
        # the JSON envelope, so assert on the process itself).
        result = subprocess.run(
            [str(BIN), "--session", "m1-mod-bad3", "py", "start", "app.py",
             "--module", "m"],
            env=self.env, cwd=str(self.fixture),
            capture_output=True, text=True, timeout=30)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self._assert_absent("m1-mod-bad3")

    def test_24_attach_target_identity(self):
        """M-I: attach responses + status/context surface requestedTarget
        (endpoint, pid null) and layered targetIdentity (debuggee
        protocol-confirmed from protocol data, endpoint os-corroborated
        listener owner, redacted); secrets never persist; browser reports
        tab metadata with no process claim."""
        self._ensure_idle_fixtures()
        sentinel = "BATCH1SECRET-9f8e7d"
        # Java target carries a secret JVM flag (JVM-accepted, no behavior change).
        port = free_port()
        target = ["java", f"-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:{port}",
                  f"-Dtoken={sentinel}", "-cp", str(self.fixture), "IdleAttach"]
        proc = self._launch_target(target, "mi-java")
        self._wait_log("mi-java", "ready")
        time.sleep(1)
        name = "mi-java"
        self.sessions.add(name)
        data = self.cli(name, "java", "attach", "--port", str(port), "--timeout", "3")
        requested = data["requestedTarget"]
        self.assertEqual(requested["port"], port)
        self.assertIsNone(requested.get("pid"))
        endpoint = data["targetIdentity"]["endpoint"]
        self.assertEqual(endpoint["ownerPid"], proc.pid)
        argv = endpoint["argv"]
        self.assertTrue(any("IdleAttach" in a for a in argv))
        redacted = [a for a in argv if "token" in a.lower()]
        self.assertTrue(redacted and all("[redacted]" in a for a in redacted),
                        f"secret flag must redact: {argv}")
        self.assertNotIn(sentinel, json.dumps(data))
        # No raw secret in any persisted file or surfaced output.
        for fname in ["session.json", "stops.json", "bridge.log"]:
            f = self.home / ".agent-debugger/sessions" / name / fname
            if f.exists():
                self.assertNotIn(sentinel, f.read_text(),
                                 f"raw secret leaked into {fname}")
        ctx = self.cli(name, "context") if data.get("location") else None
        status_row = next(r for r in self.cli(name, "status")["sessions"] if r["name"] == name)
        self.assertEqual(status_row["targetIdentity"]["endpoint"]["ownerPid"], proc.pid)
        self.assertEqual(status_row["requestedTarget"]["port"], port)
        self.assertNotIn("observedTarget", status_row)
        self.assertNotIn(sentinel, json.dumps(status_row))
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)
        # Python attach: same-file listener identity via OS lookup.
        port = free_port()
        proc = self._launch_target(self._m1_target("py", port), "mi-py")
        self._wait_log("mi-py", "ready")
        time.sleep(1)
        name = "mi-py"
        self.sessions.add(name)
        data = self.cli(name, "py", "attach", "--port", str(port), "--timeout", "2")
        endpoint = data["targetIdentity"]["endpoint"]
        # debugpy forks: the kernel-observed listener is a child of the
        # spawned process, so verify it is the debugpy listener itself.
        self.assertTrue(any("debugpy" in a for a in endpoint["argv"]))
        self.assertTrue(any(str(port) in a for a in endpoint["argv"]))
        # Listener-probe source is per-OS (lsof on macOS, /proc on Linux).
        self.assertEqual(
            endpoint["source"], "os-lsof-ps" if sys.platform == "darwin" else "os-proc")
        self.assertEqual(endpoint["unavailable"], [])
        listener = subprocess.run(["ps", "-p", str(endpoint["ownerPid"]), "-o", "args="],
                                  capture_output=True, text=True, timeout=10)
        self.assertIn("debugpy", listener.stdout)
        self.assertIn(str(port), listener.stdout)
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)

    def test_25_wrong_process_same_path(self):
        """M-I: the same script in two processes is distinguishable — each
        attach shows the endpoint ownerPid/argv of its own listener, so a
        break armed on the wrong port is visibly the wrong target."""
        self._ensure_idle_fixtures()
        gate_a = self.fixture / "migo-a"
        gate_b = self.fixture / "migo-b"
        for g in (gate_a, gate_b):
            try:
                g.unlink()
            except OSError:
                pass
        script = self.fixture / "mi_two.py"
        script.write_text(
            "import pathlib\nimport sys\nimport time\nprint(\"ready\", flush=True)\n"
            "GATE = pathlib.Path(sys.argv[1])\nwhile True:\n"
            "    if GATE.exists():\n        value = 99\n        print(\"hit\", flush=True)\n"
            "    time.sleep(0.05)\n")
        port_a, port_b = free_port(), free_port()
        proc_a = self._launch_target(
            [str(VENV_PY), "-Xfrozen_modules=off", "-m", "debugpy",
             "--listen", f"127.0.0.1:{port_a}", str(script), str(gate_a)], "mi-two-a")
        proc_b = self._launch_target(
            [str(VENV_PY), "-Xfrozen_modules=off", "-m", "debugpy",
             "--listen", f"127.0.0.1:{port_b}", str(script), str(gate_b)], "mi-two-b")
        self._wait_log("mi-two-a", "ready")
        self._wait_log("mi-two-b", "ready")
        time.sleep(1)
        seen = {}
        for name, port, proc, gate in [("mi-two-a", port_a, proc_a, gate_a),
                                       ("mi-two-b", port_b, proc_b, gate_b)]:
            self.sessions.add(name)
            data = self.cli(name, "py", "attach", "--port", str(port), "--timeout", "2")
            endpoint = data["targetIdentity"]["endpoint"]
            # debugpy forks: the observed listener is a child process, so
            # each twin is identified by its own port in its own argv.
            self.assertTrue(any("debugpy" in a for a in endpoint["argv"]))
            self.assertTrue(any(str(port) in a for a in endpoint["argv"]),
                            f"{name} identity must name its own port: {endpoint['argv']}")
            seen[name] = endpoint["ownerPid"]
            self.assertTrue(self.cli(name, "close")["confirmed"])
            self.sessions.remove(name)
        self.assertNotEqual(seen["mi-two-a"], seen["mi-two-b"])

    def test_26_breaks_remove_clear_live(self):
        """M2: live add->remove->breaks + stops.json convergence on
        py/node/java, incl. deleted-source removal, cond semantics, and
        missing-as-non-error; clear drops every line break only."""
        self._ensure_idle_fixtures()
        flows = {
            "m2r-py": ("py", str(self.py_idle), 7, 8),
            "m2r-node": ("node", str(self.js_idle), 7, 8),
            "m2r-java": ("java", "IdleAttach", 10, 11),
        }
        for name, (lang, idle, line1, line2) in flows.items():
            port = free_port()
            gate = self.fixture / f"go-{name}"
            try:
                gate.unlink()
            except OSError:
                pass
            if lang == "java":
                target = ["java", f"-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:{port}",
                          "-cp", str(self.fixture), "IdleAttach"]
                first, second = f"{idle}:{line1}", f"{idle}:{line2}"
            else:
                target = self._m1_target(lang, port)
                first, second = f"{idle}:{line1}", f"{idle}:{line2}"
            self._launch_target(target, f"m2r-{lang}")
            self._wait_log(f"m2r-{lang}", "ready")
            time.sleep(1)
            self.sessions.add(name)
            timeout = "3" if lang == "java" else "2"
            self.cli(name, lang, "attach", "--port", str(port), "--timeout", timeout)
            added = self.cli(name, "breaks", "add", "--break", first, "--break", second)
            self.assertEqual(len(added["added"]), 2)
            stops_file = self.home / ".agent-debugger/sessions" / name / "stops.json"
            # Remove one: live recs + file converge, raw echoed verbatim.
            removed = self.cli(name, "breaks", "remove", "--break", first)
            self.assertEqual(len(removed["removed"]), 1)
            self.assertEqual(removed["removed"][0]["raw"], first)
            live = [s["spec"] for s in self.cli(name, "breaks")["stops"] if s["kind"] == "break"]
            self.assertFalse(any(s.endswith(f":{line1}") for s in live))
            self.assertTrue(any(s.endswith(f":{line2}") for s in live))
            file_breaks = json.loads(stops_file.read_text())["breaks"]
            self.assertNotIn(first, file_breaks)
            self.assertIn(second, file_breaks)
            # Missing spec is non-fatal.
            miss = self.cli(name, "breaks", "remove", "--break",
                            first if lang == "java" else f"{idle}:99")
            self.assertTrue(miss["ok"])
            self.assertEqual(miss["removed"], [])
            self.assertEqual(len(miss.get("missing", [])), 1)
            self.assertEqual(json.loads(stops_file.read_text())["breaks"], file_breaks)
            # Clear drops the rest; bare breaks then shows no line breaks.
            cleared = self.cli(name, "breaks", "clear")
            self.assertTrue(cleared["ok"])
            self.assertEqual(len(cleared["removed"]), 1)
            self.assertEqual(cleared["removed"][0]["raw"], second)
            self.assertEqual(json.loads(stops_file.read_text())["breaks"], [])
            live = [s for s in self.cli(name, "breaks")["stops"] if s["kind"] == "break"]
            self.assertEqual(live, [])
            row = next(r for r in self.cli(name, "status")["sessions"] if r["name"] == name)
            self.assertEqual(row["armed"]["breaks"], 0)
            self.assertTrue(self.cli(name, "close")["confirmed"])
            self.sessions.remove(name)
        # Deleted-source removal (py): the file is gone, removal still works.
        port = free_port()
        doomed = self.fixture / "m2r_doomed.py"
        doomed.write_text('import time\nprint("ready", flush=True)\nwhile True:\n    time.sleep(0.05)\n')
        target = [str(VENV_PY), "-Xfrozen_modules=off", "-m", "debugpy",
                  "--listen", f"127.0.0.1:{port}", str(doomed)]
        self._launch_target(target, "m2r-doomed")
        self._wait_log("m2r-doomed", "ready")
        time.sleep(1)
        name = "m2r-doomed"
        self.sessions.add(name)
        self.cli(name, "py", "attach", "--port", str(port), "--timeout", "2")
        raw = f"{doomed}:2"
        self.assertEqual(len(self.cli(name, "breaks", "add", "--break", raw)["added"]), 1)
        doomed.unlink()
        removed = self.cli(name, "breaks", "remove", "--break", raw)
        self.assertEqual([e["raw"] for e in removed["removed"]], [raw])
        stops_file = self.home / ".agent-debugger/sessions" / name / "stops.json"
        self.assertNotIn(raw, json.loads(stops_file.read_text())["breaks"])
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)
        # Cond semantics (py): plain spec never removes a conditional record.
        port = free_port()
        self._launch_target(self._m1_target("py", port), "m2r-cond")
        self._wait_log("m2r-cond", "ready")
        time.sleep(1)
        name = "m2r-cond"
        self.sessions.add(name)
        self.cli(name, "py", "attach", "--port", str(port), "--timeout", "2")
        cond_raw = f"{self.py_idle}:7|value == 99"
        self.assertEqual(len(self.cli(name, "breaks", "add", "--break", cond_raw)["added"]), 1)
        miss = self.cli(name, "breaks", "remove", "--break", f"{self.py_idle}:7")
        self.assertEqual(miss["removed"], [])
        self.assertEqual(miss["missing"], [f"{self.py_idle}:7"])
        gone = self.cli(name, "breaks", "remove", "--break", cond_raw)
        self.assertEqual([e["raw"] for e in gone["removed"]], [cond_raw])
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)
        # Java shadow re-arm: a logpoint shadowed by a break revives through
        # the normal logpoint path once the break is removed.
        port = free_port()
        self._launch_target(
            ["java", f"-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:{port}",
             "-cp", str(self.fixture), "IdleAttach"], "m2r-shadow")
        self._wait_log("m2r-shadow", "ready")
        time.sleep(1)
        name = "m2r-shadow"
        self.sessions.add(name)
        self.cli(name, "java", "attach", "--port", str(port),
                 "--break", "IdleAttach:10",
                 "--logpoint", "IdleAttach:10:hit", "--timeout", "3")
        shadowed = [s for s in self.cli(name, "breaks")["stops"]
                    if s["kind"] == "logpoint"]
        self.assertEqual(len(shadowed), 1)
        self.assertEqual(shadowed[0]["state"], "shadowed")
        removed = self.cli(name, "breaks", "remove", "--break", "IdleAttach:10")
        self.assertEqual([e["raw"] for e in removed["removed"]], ["IdleAttach:10"])
        rearmed = [s for s in self.cli(name, "breaks")["stops"]
                   if s["kind"] == "logpoint"]
        self.assertEqual(len(rearmed), 1)
        self.assertNotEqual(rearmed[0]["state"], "shadowed")
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)

    def test_27_browser_remove_clear_and_identity(self):
        """M2+M-I browser: tab identity in attach/status, add->remove,
        reload never restores removed breaks, clear empties stops.json."""
        chrome = find_chrome()
        if chrome is None:
            self.skipTest("Chrome unavailable")

        class QuietHandler(http.server.SimpleHTTPRequestHandler):
            def log_message(self, *args):
                pass
        handler = functools.partial(QuietHandler, directory=str(ROOT / "examples/browser-demo"))
        httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        self.addCleanup(httpd.shutdown)
        self.addCleanup(httpd.server_close)
        cdp_port = free_port()
        url = f"http://127.0.0.1:{httpd.server_port}/index.html"
        proc = subprocess.Popen(
            [chrome, "--headless", "--disable-gpu", "--no-first-run",
             f"--remote-debugging-port={cdp_port}",
             f"--user-data-dir={self.home}/m2rchrome", url],
            stdout=self.log, stderr=self.log)
        self.addCleanup(proc.terminate)
        deadline = time.monotonic() + 20
        while True:
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{cdp_port}/json/list", timeout=1) as response:
                    if any(t.get("url") == url for t in json.load(response)):
                        break
            except OSError:
                pass
            if time.monotonic() > deadline:
                self.fail("Chrome did not expose test tab")
            time.sleep(.1)
        name = "m2r-browser"
        self.sessions.add(name)
        data = self.cli(name, "browser", "attach", "--port", str(cdp_port), "--tab", url,
                        "--break", "app.js:8")
        self.assertNotIn("observedTarget", data)
        dbg = data["targetIdentity"]["debuggee"]
        self.assertEqual(dbg["kind"], "tab")
        self.assertEqual(dbg["url"], url)
        self.assertTrue(dbg["targetId"])
        self.assertEqual(dbg["confidence"], "protocol-confirmed")
        self.assertIn("cwd", dbg["notApplicable"])
        self.assertIn("argv", dbg["notApplicable"])
        row = next(r for r in self.cli(name, "status")["sessions"] if r["name"] == name)
        self.assertEqual(row["targetIdentity"]["debuggee"]["kind"], "tab")
        self.assertEqual(row["targetIdentity"]["debuggee"]["url"], url)
        self.assertEqual(row["requestedTarget"]["port"], cdp_port)
        added = self.cli(name, "breaks", "add", "--break", "app.js:2")
        self.assertEqual(len(added["added"]), 1)
        removed = self.cli(name, "breaks", "remove", "--break", "app.js:8")
        self.assertEqual([e["raw"] for e in removed["removed"]], ["app.js:8"])
        # Reload lands on the surviving break, never the removed one.
        stop = self.cli(name, "reload", "--timeout", "10")
        self.assertEqual(stop["snapshot"]["location"]["line"], 2)
        stops_file = self.home / ".agent-debugger/sessions" / name / "stops.json"
        self.assertEqual(json.loads(stops_file.read_text())["breaks"], ["app.js:2"])
        cleared = self.cli(name, "breaks", "clear")
        self.assertEqual([e["raw"] for e in cleared["removed"]], ["app.js:2"])
        self.assertEqual(json.loads(stops_file.read_text())["breaks"], [])
        # No breaks armed: reload returns fast instead of burning the timeout.
        quick = self.cli(name, "reload", "--timeout", "10")
        self.assertTrue(quick.get("reloaded"))
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)

    def test_28_py_child_popen_targeting_and_breaks(self):
        """M3/Batch2 Python: Popen child is a visible, addressable target.
        Child-only startup break parks the child (auto-select); context/
        eval/continue route by target; global intent inherits; ephemeral
        target breaks skip stops.json; scoped/global/clear removals obey
        scope; close reaps the tree."""
        self._ensure_idle_fixtures()
        child = self.fixture / "b2_child.py"
        child.write_text(
            "import time\nfor i in range(200):\n    tick = i * 2\n"
            "    print(f\"child tick {tick}\", flush=True)\n    time.sleep(0.2)\n")
        parent = self.fixture / "b2_parent.py"
        parent.write_text(
            "import subprocess, sys, os, time\n"
            "here = os.path.dirname(os.path.abspath(__file__))\n"
            "p = subprocess.Popen([sys.executable, os.path.join(here, \"b2_child.py\")])\n"
            "print(\"parent spawned\", p.pid, flush=True)\n"
            "for i in range(200):\n"
            "    print(f\"parent {i}\", flush=True)\n    time.sleep(0.2)\n"
            "p.wait()\n")
        name = "b2t28-py-child"
        self.sessions.add(name)
        try:
            start = self.cli(name, "py", "start", "--subprocess", str(parent),
                             "--break", f"{child}:4", "--timeout", "40")
            self.assertTrue(start["target"].startswith("child:"))
            self.assertEqual(start["location"]["line"], 4)
            child_id = start["target"]
            roster = self.cli(name, "targets")
            ids = [t["id"] for t in roster["targets"]]
            self.assertIn("main", ids)
            self.assertIn(child_id, ids)
            entry = next(t for t in roster["targets"] if t["id"] == child_id)
            self.assertEqual(entry["kind"], "child")
            self.assertEqual(entry["state"], "stopped")
            self.assertEqual(entry["observed"]["source"], "debugpy-subProcessId")
            self.assertEqual(entry["scope"], "inherited")
            self.assertEqual(roster["selected"], child_id)
            # Explicit main is pinned (Milestone A): bare threads
            # aggregates main + the live child, --target main serves main
            # only, and a main-targeted continue never serves the parked
            # child (main-scoped timeout here — main has no break).
            threads = self.cli(name, "threads")
            agg = [e["target"] for e in threads.get("targets", [])]
            self.assertIn("main", agg)
            self.assertIn(child_id, agg)
            self.assertEqual(threads["selected"], child_id)
            one = self.cli(name, "threads", "--target", "main")
            self.assertEqual(one["target"], "main")
            self.assertNotIn("targets", one)
            cont_main = self.cli(name, "continue", "--target", "main",
                                 "--timeout", "3", ok=False)
            self.assertIn("no stop within", cont_main["error"])
            still = self.cli(name, "targets")
            self.assertEqual(
                next(t for t in still["targets"]
                     if t["id"] == child_id)["state"], "stopped")
            # Targetless commands auto-select the parked child.
            ctx = self.cli(name, "context")
            self.assertEqual(ctx["target"], child_id)
            self.assertEqual(ctx["location"]["line"], 4)
            self.assertEqual(self.cli(name, "eval", "tick")["value"], "0")
            cont = self.cli(name, "continue", "--timeout", "15")
            self.assertEqual(cont["target"], child_id)
            # Global add inherits onto the child; ephemeral does not persist.
            added = self.cli(name, "breaks", "add", "--break", f"{parent}:7")
            self.assertEqual(added["target"], "main")
            live = self.cli(name, "breaks")["stops"]
            self.assertTrue(any(s.get("target") == child_id and s["spec"].endswith("b2_parent.py:7")
                                for s in live))
            stops_file = self.home / ".agent-debugger/sessions" / name / "stops.json"
            before = json.loads(stops_file.read_text())["breaks"]
            eph = self.cli(name, "breaks", "add", "--break", f"{child}:5",
                           "--target", child_id)
            self.assertEqual(eph["target"], child_id)
            self.assertEqual(json.loads(stops_file.read_text())["breaks"], before)
            rm = self.cli(name, "breaks", "remove", "--break", f"{child}:5",
                          "--target", child_id)
            self.assertEqual(rm["target"], child_id)
            # Unknown targets fail explicitly.
            bad = self.cli(name, "context", "--target", "child:999999999", ok=False)
            self.assertIn("unknown target", bad["error"])
            # Global remove drops the intent plus the inherited copy.
            self.cli(name, "breaks", "remove", "--break", f"{parent}:7")
            live = self.cli(name, "breaks")["stops"]
            self.assertFalse(any(s["spec"].endswith("b2_parent.py:7") for s in live))
            cleared = self.cli(name, "breaks", "clear")
            self.assertEqual(cleared["target"], "main")
            self.assertEqual(self.cli(name, "breaks")["stops"], [])
            self.assertTrue(self.cli(name, "close")["confirmed"])
            self.sessions.remove(name)
        finally:
            if name in self.sessions:
                try:
                    self.cli(name, "close", timeout=85)
                except Exception:
                    pass
                self.sessions.remove(name)

    def test_29_py_child_spawn_fork_overflow(self):
        """M3/Batch2 Python: multiprocessing spawn workers are tracked (the
        resource_tracker shim is skipped without budget); os.fork children
        attach; 9 children overflow to ignored+released; exits reconcile
        without killing the session."""
        self._ensure_idle_fixtures()
        mp = self.fixture / "b2_mp.py"
        mp.write_text(
            "import multiprocessing as mp\nimport time\n\n"
            "def worker(n):\n    total = n * 3\n"
            "    print(f\"mp worker total {total}\", flush=True)\n    time.sleep(15)\n\n"
            "def main():\n    ctx = mp.get_context(\"spawn\")\n"
            "    w = ctx.Process(target=worker, args=(1,))\n    w.start()\n"
            "    print(\"mp parent started\", flush=True)\n    w.join()\n\n"
            "if __name__ == \"__main__\":\n    main()\n")
        name = "b2t29-py-spawn"
        self.sessions.add(name)
        try:
            start = self.cli(name, "py", "start", "--subprocess", str(mp),
                             "--break", f"{mp}:5", "--timeout", "60")
            self.assertTrue(start["target"].startswith("child:"))
            roster = self.cli(name, "targets")
            live = [t for t in roster["targets"] if t["state"] in ("running", "stopped")]
            self.assertGreaterEqual(len(live), 2)  # main + at least one worker
            kinds = {t["kind"] for t in roster["targets"]}
            self.assertIn("child", kinds)
            self.assertGreaterEqual(roster.get("helpersReleased", 0), 1)
            self.assertTrue(self.cli(name, "close")["confirmed"])
            self.sessions.remove(name)
        finally:
            if name in self.sessions:
                try:
                    self.cli(name, "close", timeout=85)
                except Exception:
                    pass
                self.sessions.remove(name)
        if not hasattr(__import__("os"), "fork"):
            self.skipTest("os.fork unavailable")
        fork = self.fixture / "b2_fork.py"
        fork.write_text(
            "import os, time\nprint(\"fork parent ready\", flush=True)\n"
            "pid = os.fork()\nif pid == 0:\n"
            "    print(\"fork child here\", flush=True)\n    time.sleep(12)\n"
            "    os._exit(0)\n"
            "print(f\"fork parent spawned {pid}\", flush=True)\n"
            "time.sleep(15)\nos.waitpid(pid, 0)\n")
        name = "b2t29-py-fork"
        self.sessions.add(name)
        try:
            start = self.cli(name, "py", "start", "--subprocess", str(fork),
                             "--break", f"{fork}:5", "--timeout", "40")
            self.assertTrue(start["target"].startswith("child:"))
            self.assertTrue(self.cli(name, "close")["confirmed"])
            self.sessions.remove(name)
        finally:
            if name in self.sessions:
                try:
                    self.cli(name, "close", timeout=85)
                except Exception:
                    pass
                self.sessions.remove(name)
        many = self.fixture / "b2_many.py"
        kids = "".join(
            "p = subprocess.Popen([sys.executable, \"-c\",\n"
            f"                      \"import time; print('kid{i} go', flush=True); time.sleep(20)\"])\n"
            "procs.append(p)\n" for i in range(9))
        many.write_text(
            "import subprocess, sys, os, time\n"
            "here = os.path.dirname(os.path.abspath(__file__))\nprocs = []\n"
            f"{kids}"
            "print(\"many parent spawned 9\", flush=True)\n"
            "time.sleep(30)\nfor p in procs:\n    p.wait()\n")
        parent_line = len(many.read_text().splitlines()) - 3
        name = "b2t29-py-many"
        self.sessions.add(name)
        try:
            start = self.cli(name, "py", "start", "--subprocess", str(many),
                             "--break", f"{many}:{parent_line}", "--timeout", "45")
            self.assertEqual(start["target"], "main")
            deadline = time.monotonic() + 60
            roster = None
            while time.monotonic() < deadline:
                roster = self.cli(name, "targets")
                act = [t for t in roster["targets"]
                       if t["id"] != "main" and t["state"] in ("running", "stopped")]
                if len(act) >= 8 and roster["ignored"] >= 1:
                    break
                time.sleep(2)
            self.assertGreaterEqual(len(act), 8)
            self.assertGreaterEqual(roster["ignored"], 1)
            self.assertLessEqual(len(act), 8)
            # Progress, not just presence: every kid (tracked or ignored)
            # runs free and reaps on its own — nothing hangs parked.
            deadline = time.monotonic() + 90
            while time.monotonic() < deadline:
                gone = subprocess.run(
                    ["pgrep", "-f", "kid[0-8] go"],
                    capture_output=True, text=True, timeout=10)
                if gone.returncode != 0:
                    break
                time.sleep(3)
            else:
                self.fail("spawned kids never reaped")
            self.assertTrue(self.cli(name, "close")["confirmed"])
            self.sessions.remove(name)
        finally:
            if name in self.sessions:
                try:
                    self.cli(name, "close", timeout=85)
                except Exception:
                    pass
                self.sessions.remove(name)

    def test_30_node_worker_targeting_and_breaks(self):
        """M4/Batch2 Node: worker_threads are visible, addressable targets.
        Worker-only startup break parks the worker (hitBreakpoints truth,
        even with reason other); context/eval/step/continue route by
        target; global intent inherits; ephemeral worker breaks skip
        stops.json; scoped/global/clear removals obey scope; natural exit
        reconciles; close reaps."""
        self._ensure_idle_fixtures()
        prog = self.fixture / "b2_worker.js"
        prog.write_text(
            "const { Worker, isMainThread, workerData } = require('worker_threads');\n"
            "if (isMainThread) {\n"
            "  const w = new Worker(__filename, { workerData: { n: 21 } });\n"
            "  w.on('message', (m) => console.log(\"main got\", m));\n"
            "  setInterval(() => console.log(\"main tick\"), 2000);\n"
            "} else {\n"
            "  const result = workerData.n * 2;\n"
            "  for (let i = 0; i < 40; i++) {\n"
            "    const dbl = result + i;\n"
            "    require('worker_threads').parentPort.postMessage(dbl);\n"
            "    const t = Date.now(); while (Date.now() - t < 500);\n"
            "  }\n"
            "}\n")
        name = "b2t30-node-worker"
        self.sessions.add(name)
        try:
            start = self.cli(name, "node", "start", "--workers", str(prog),
                             "--break", f"{prog}:10", "--timeout", "40")
            self.assertTrue(start["target"].startswith("worker:"))
            wid = start["target"]
            roster = self.cli(name, "targets")
            entry = next(t for t in roster["targets"] if t["id"] == wid)
            self.assertEqual(entry["kind"], "worker")
            self.assertEqual(entry["state"], "stopped")
            self.assertEqual(entry["scope"], "inherited")
            self.assertTrue((entry["observed"] or {}).get("url", "").endswith("b2_worker.js"))
            self.assertEqual(roster["selected"], wid)
            # Explicit main is pinned (Milestone A): bare threads
            # aggregates main + the live worker, --target main serves main
            # only, and a main-targeted continue never serves the parked
            # worker (main-scoped timeout here — main has no break).
            threads = self.cli(name, "threads")
            agg = [e["target"] for e in threads.get("targets", [])]
            self.assertIn("main", agg)
            self.assertIn(wid, agg)
            self.assertEqual(threads["selected"], wid)
            one = self.cli(name, "threads", "--target", "main")
            self.assertEqual(one["target"], "main")
            self.assertNotIn("targets", one)
            cont_main = self.cli(name, "continue", "--target", "main",
                                 "--timeout", "3", ok=False)
            self.assertIn("no stop within", cont_main["error"])
            still = self.cli(name, "targets")
            self.assertEqual(
                next(t for t in still["targets"]
                     if t["id"] == wid)["state"], "stopped")
            self.assertEqual(self.cli(name, "eval", "result", "--target", wid)["value"], "42")
            step = self.cli(name, "step", "over", "--target", wid, "--timeout", "15")
            self.assertEqual(step["target"], wid)
            added = self.cli(name, "breaks", "add", "--break", f"{prog}:4")
            self.assertEqual(added["target"], "main")
            live = self.cli(name, "breaks")["stops"]
            self.assertTrue(any(s.get("target") == wid and s["spec"].endswith("b2_worker.js:4")
                                for s in live))
            stops_file = self.home / ".agent-debugger/sessions" / name / "stops.json"
            before = json.loads(stops_file.read_text())["breaks"]
            eph = self.cli(name, "breaks", "add", "--break", f"{prog}:11",
                           "--target", wid)
            self.assertEqual(eph["target"], wid)
            self.assertEqual(json.loads(stops_file.read_text())["breaks"], before)
            bad = self.cli(name, "context", "--target", "worker:missing", ok=False)
            self.assertIn("unknown target", bad["error"])
            self.cli(name, "breaks", "remove", "--break", f"{prog}:10")
            live = self.cli(name, "breaks")["stops"]
            self.assertFalse(any(s["spec"].endswith("b2_worker.js:10") for s in live))
            self.cli(name, "breaks", "clear")
            self.assertEqual(self.cli(name, "breaks")["stops"], [])
            # Let the worker run out, then prove independent exit + reap.
            cont = self.cli(name, "continue", "--target", wid, "--timeout", "50",
                            timeout=70, ok=False)
            roster = self.cli(name, "targets")
            states = {t["id"]: t["state"] for t in roster["targets"]}
            self.assertEqual(states.get(wid), "exited")
            self.assertEqual(states.get("main"), "running")
            _ = cont
            self.assertTrue(self.cli(name, "close")["confirmed"])
            self.sessions.remove(name)
        finally:
            if name in self.sessions:
                try:
                    self.cli(name, "close", timeout=85)
                except Exception:
                    pass
                self.sessions.remove(name)

    def test_31_node_worker_overflow_progress(self):
        """M4/Batch2 Node: 9 workers overflow to 8 tracked + 1 ignored; the
        ignored worker is kicked (resume + run gate) so it RUNS and EXITS on
        its own — assert it reaches exited history, not just the roster
        count (a released-but-hung worker would never retire)."""
        self._ensure_idle_fixtures()
        prog = self.fixture / "b2_wmany.js"
        prog.write_text(
            "const { Worker, isMainThread } = require('worker_threads');\n"
            "if (isMainThread) {\n"
            "  for (let i = 0; i < 9; i++) {\n"
            "    new Worker(__filename);\n"
            "  }\n"
            "  console.log(\"wmain spawned 9\");\n"
            "  setInterval(() => console.log(\"main tick\"), 2000);\n"
            "} else {\n"
            "  const t = Date.now(); while (Date.now() - t < 8000);\n"
            "}\n")
        name = "b2t31-node-overflow"
        self.sessions.add(name)
        try:
            start = self.cli(name, "node", "start", "--workers", str(prog),
                             "--break", f"{prog}:6", "--timeout", "40")
            self.assertEqual(start["target"], "main")
            deadline = time.monotonic() + 60
            roster = None
            ignored_id = None
            while time.monotonic() < deadline:
                roster = self.cli(name, "targets")
                act = [t for t in roster["targets"]
                       if t["id"] != "main" and t["state"] in ("running", "stopped")]
                ign = [t for t in roster["targets"] if t["state"] == "ignored"]
                if len(act) >= 8 and len(ign) >= 1 and roster["ignored"] >= 1:
                    ignored_id = ign[0]["id"]
                    break
                time.sleep(2)
            self.assertIsNotNone(ignored_id, "expected 8 tracked + 1 ignored")
            self.assertLessEqual(len(act), 8)
            # Progress, not just presence: the kicked worker runs out and
            # retires to exited history on its own.
            deadline = time.monotonic() + 90
            retired = False
            while time.monotonic() < deadline:
                roster = self.cli(name, "targets")
                if any(t["id"] == ignored_id and t["state"] == "exited"
                       for t in roster["targets"]):
                    retired = True
                    break
                time.sleep(3)
            self.assertTrue(retired, f"ignored {ignored_id} never exited")
            self.assertTrue(self.cli(name, "close")["confirmed"])
            self.sessions.remove(name)
        finally:
            if name in self.sessions:
                try:
                    self.cli(name, "close", timeout=85)
                except Exception:
                    pass
                self.sessions.remove(name)

    def _write_old_fixture(self, name, kind="attach", port=1, lang="py", requested=None):
        """Legacy v1 session dir: version-less markers + old session file."""
        d = self.home / ".agent-debugger/sessions" / name
        d.mkdir(parents=True, exist_ok=True)
        (d / "lang.json").write_text(json.dumps({"lang": lang}))
        stops = {"breaks": [], "logpoints": [], "watches": [], "exits": [],
                 "sources": [], "timeout": 7,
                 "target": {"host": "127.0.0.1", "port": port},
                 "requestedTarget": requested if requested is not None
                 else {"host": "127.0.0.1", "port": port, "pid": None}}
        (d / "stops.json").write_text(json.dumps(stops))
        (d / "session.json").write_text(json.dumps(
            {"name": name, "kind": kind, "port": port,
             "stopped": True, "lastStop": None, "updatedAt": 1}))
        return d

    def test_32_schema_v1_old_session_reject_status_close(self):
        """Old dirs reject every gated command, stay status-visible as
        unsupported, and close (dead dir, plus a framed-mock old-live
        bridge speaking the old protocol with no version knowledge)."""
        name = "oldneg"
        self._write_old_fixture(name)
        for args in [
            ("continue", "--timeout", "1"), ("wait", "--timeout", "1"),
            ("capture", "--timeout", "1"), ("step", "over", "--timeout", "1"),
            ("stack",), ("threads",), ("vars",), ("eval", "1+1"),
            ("reload", "--timeout", "1"), ("breaks",), ("logs",),
            ("targets",), ("context",),
            ("breaks", "add", "--break", "a.py:1"),
            ("breaks", "remove", "--break", "a.py:1"),
            ("breaks", "clear"),
        ]:
            data = self.cli(name, *args, ok=False)
            self.assertFalse(data["ok"], args)
            self.assertIn(
                f"unsupported session '{name}' (schema v1; close it and recreate)",
                data["error"], args)
        rows = self.cli("unused", "status")["sessions"]
        row = next(r for r in rows if r["name"] == name)
        self.assertTrue(row["unsupported"] and row["stale"])
        self.assertNotIn("observedTarget", row)
        self.assertIn("and recreate (unsupported schema v1)", row["hint"])
        out = self.cli(name, "close")
        self.assertEqual(out["closed"], name)
        self._assert_absent(name)
        # Old-live close through a minimal framed mock (Content-Length JSON,
        # old protocol, no schemaVersion anywhere): version-agnostic close.
        mock = _MockOldBridge()
        mock.start()
        self.addCleanup(mock.stop)
        live = "oldlive"
        self._write_old_fixture(live, port=mock.port)
        out = self.cli(live, "close", timeout=85)
        self.assertEqual(out["closed"], live)
        self.assertTrue(out["confirmed"], "mock ACKed and died: confirmed close")
        self._assert_absent(live)

    def test_33_legacy_reclaim_proven_dead_vs_refuse(self):
        """Same-name start: v2-present bails always; old-present reclaims
        only when proven dead (numeric port + silent daemon); corrupt or
        possibly-live old state refuses for `close`."""
        script = self.fixture / "repeat.py"
        name = "reclaim-me"
        # Proven dead legacy dir reclaims into a live v2 session.
        self._write_old_fixture(name, kind="launch", port=1)
        self.sessions.add(name)
        data = self.cli(name, "py", "start", str(script),
                        "--break", f"{script}:4", "--timeout", "10")
        self.assertIn("location", data)
        markers = json.loads(
            (self.home / ".agent-debugger/sessions" / name / "lang.json").read_text())
        self.assertEqual(markers["schemaVersion"], 2)
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)
        # Corrupt session.json: unprovable, refuse. Close must preserve
        # corrupted state for explicit repair/removal; it must not silently
        # delete a session whose lifecycle state cannot be read.
        self._write_old_fixture(name, kind="launch", port=1)
        corrupt_dir = self.home / ".agent-debugger/sessions" / name
        corrupt_file = corrupt_dir / "session.json"
        corrupt_file.write_text("not json")
        data = self.cli(name, "py", "start", str(script), "--timeout", "2", ok=False)
        self.assertFalse(data["ok"])
        self.assertIn(f"unsupported session '{name}' (schema v1; close it first)",
                      data["error"])
        close_data = self.cli(name, "close", ok=False)
        self.assertFalse(close_data["ok"])
        self.assertIn("corrupt session file", close_data["error"])
        self.assertIn(str(corrupt_file), close_data["error"])
        self.assertTrue(corrupt_dir.is_dir(), "corrupt state must be preserved")
        # This fixture is owned by this test. Remove it explicitly only
        # after asserting preservation so the following same-name v2 case
        # remains deterministic without weakening production close safety.
        shutil.rmtree(corrupt_dir)
        self._assert_absent(name)
        # v2-present dir bails always, live or dead.
        self.sessions.add(name)
        self.cli(name, "py", "start", str(script),
                 "--break", f"{script}:4", "--timeout", "10")
        data = self.cli(name, "py", "start", str(script), "--timeout", "2", ok=False)
        self.assertIn(f"session '{name}' already exists (close it first)", data["error"])
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)

    def test_34_legacy_live_blocks_attach(self):
        """Exclusive attach never proceeds while a live legacy session
        exists: endpoint-matched old owner blocks first (all-unavailable
        identity, zero pids), then the global scan blocks unknowable
        endpoints; closing the legacy dirs unblocks. Also covers
        attach-before-publish layered identity and marker-less session
        staleness."""
        self._ensure_idle_fixtures()
        port = free_port()
        proc = self._launch_target(self._m1_target("py", port), "legacytarget")
        self._wait_log("legacytarget", "ready")
        time.sleep(1)
        match_mock = _MockOldBridge()
        match_mock.start()
        self.addCleanup(match_mock.stop)
        global_mock = _MockOldBridge()
        global_mock.start()
        self.addCleanup(global_mock.stop)
        self._write_old_fixture(
            "legacy-match", kind="attach", port=match_mock.port,
            requested={"host": "127.0.0.1", "port": port, "pid": None})
        self._write_old_fixture(
            "legacy-global", kind="attach", port=global_mock.port,
            requested={"host": "127.0.0.1", "port": 59999, "pid": None})
        # (a) endpoint-matched old owner: names the session, zero pids.
        data = self.cli("legacy-new", "py", "attach", "--port", str(port),
                        "--timeout", "2", ok=False)
        self.assertFalse(data["ok"])
        self.assertIn("already attached by session 'legacy-match'", data["error"])
        self.assertEqual(data["diagnosis"]["code"], "endpoint-already-attached")
        ident = data["targetIdentity"]
        for role in ["debuggee", "endpoint", "adapter"]:
            self.assertIn(role, ident)
        self.assertNotRegex(json.dumps(ident), r'"(ownerPid|pid)":\s*\d',
                            f"legacy owner must carry zero pids: {ident}")
        # (b) after closing the match, the global scan still blocks.
        out = self.cli("legacy-match", "close", timeout=85)
        self.assertEqual(out["closed"], "legacy-match")
        data = self.cli("legacy-new", "py", "attach", "--port", str(port),
                        "--timeout", "2", ok=False)
        self.assertIn("unsupported live legacy session(s) 'legacy-global'", data["error"])
        self.assertEqual(data["diagnosis"]["code"], "unsupported-legacy-live")
        for role in ["debuggee", "endpoint", "adapter"]:
            self.assertEqual(data["targetIdentity"][role]["confidence"], "unavailable")
        # Closing the last legacy dir unblocks the attach.
        out = self.cli("legacy-global", "close", timeout=85)
        self.assertEqual(out["closed"], "legacy-global")
        name = "legacy-new-ok"
        self.sessions.add(name)
        data = self.cli(name, "py", "attach", "--port", str(port), "--timeout", "2")
        self.assertIn("targetIdentity", data)
        row = next(r for r in self.cli(name, "status")["sessions"] if r["name"] == name)
        self.assertFalse(row["unsupported"] or row["stale"])
        self.assertNotIn("observedTarget", row)
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)
        # Attach failure before any publish carries layered attempted identity.
        data = self.cli("deadattach", "py", "attach", "--port", "9",
                        "--timeout", "1", ok=False)
        self.assertTrue(data["error"].startswith("attach failed:"), data)
        self.assertNotIn("observedTarget", data)
        self.assertEqual(data["targetIdentity"]["debuggee"]["confidence"], "unavailable")
        self.assertEqual(data["requestedTarget"]["port"], 9)
        # v2 markers + marker-less session.json: stale, never current.
        weird = "badmarker"
        d = self.home / ".agent-debugger/sessions" / weird
        d.mkdir(parents=True, exist_ok=True)
        (d / "lang.json").write_text(json.dumps({"lang": "py", "schemaVersion": 2}))
        (d / "stops.json").write_text(json.dumps({"breaks": [], "schemaVersion": 2,
                                                  "requestedTarget": {}}))
        (d / "session.json").write_text(json.dumps({"name": weird, "kind": "attach",
                                                    "port": 1, "stopped": True}))
        row = next(r for r in self.cli("unused", "status")["sessions"] if r["name"] == weird)
        self.assertTrue(row["unsupported"] and row["stale"])
        out = self.cli(weird, "close")
        self.assertEqual(out["closed"], weird)
        self._assert_absent(weird)

    def test_35_py_many_locals_attach_capture_and_shortlived_stages(self):
        """User-reported stop/capture regressions, live on debugpy:
        (a) a frame with >MAX_VARS (20) locals stops, tracks, and
        captures successfully — no KeyError, no session loss, snapshot
        carries the truncation sentinel; (b) capture against a
        short-lived target reports truthful stages (session-gone /
        armed-wait) with additive waitContext, never endpoint-rejected;
        (c) closing (detach) while parked resumes the debuggee — the
        target proceeds on its own afterwards."""
        self._ensure_idle_fixtures()
        gate = self.fixture / "manygo"
        done = self.fixture / "manydone"
        for p in (gate, done):
            try:
                p.unlink()
            except OSError:
                pass
        many = self.fixture / "many_locals.py"
        lines = ["import pathlib", "import time",
                 f"GATE = pathlib.Path({str(gate)!r})",
                 f"DONE = pathlib.Path({str(done)!r})",
                 'print("ready", flush=True)',
                 "while True:",
                 "    if GATE.exists():"]
        for k in range(25):
            lines.append(f"        v{k:02d} = {k}")
        lines += ['        print("hit", flush=True)',
                  "        GATE.unlink()",
                  "        DONE.touch()",
                  "    time.sleep(0.05)"]
        many.write_text("\n".join(lines) + "\n")
        hit_line = 7 + 25 + 1  # print("hit") line
        port = free_port()
        proc = self._launch_target(
            [str(VENV_PY), "-Xfrozen_modules=off", "-m", "debugpy",
             "--listen", f"127.0.0.1:{port}", str(many)], "manylocals")
        self._wait_log("manylocals", "ready")
        time.sleep(1)
        name = "many-py"
        self.sessions.add(name)
        data = self.cli(name, "py", "attach", "--port", str(port),
                        "--break", f"{many}:{hit_line}", "--timeout", "3")
        self.assertTrue(data["running"], "never-hit break stays running")
        gate.touch()
        self._wait_stopped(name, True)
        ctx = self.cli(name, "context")
        self.assertEqual(ctx["location"]["line"], hit_line)
        # >20 locals: vars stay bounded with the truncation sentinel, and
        # change tracking completed (no KeyError/session loss).
        locals_ = self.cli(name, "vars")["locals"]
        self.assertEqual(locals_[-1]["name"], "\u2026")
        self.assertIn("note", locals_[-1])
        real = [v for v in locals_ if v["name"] != "\u2026"]
        self.assertEqual(len(real), 20)
        # Capture on the parked target collects without resuming.
        cap = self.cli(name, "capture", "--timeout", "10")
        self.assertTrue(cap["targetWasPaused"])
        self.assertFalse(cap["resumed"])
        cap_locs = cap["snapshot"]["frames"][0]["locals"]
        self.assertEqual(cap_locs[-1]["name"], "\u2026")
        self.assertTrue(cap["truncated"]["vars"])
        # (c) close (detach) while parked resumes the debuggee: the
        # target proceeds past the stop on its own (DONE appears) and
        # stays alive for a fresh attach.
        self.assertTrue(self.cli(name, "close")["confirmed"])
        self.sessions.remove(name)
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline and not done.exists():
            time.sleep(0.1)
        self.assertTrue(done.exists(),
                        "detach must resume the paused debuggee")
        self.assertIsNone(proc.poll(), "target survives detach")
        # (b) short-lived stages. Fresh attach, then kill the target:
        # a capture afterwards is session-gone or armed-wait — truthful
        # target-exited wording, additive stage, never endpoint-rejected.
        name2 = "short-py"
        self.sessions.add(name2)
        data = self.cli(name2, "py", "attach", "--port", str(port),
                        "--timeout", "5")
        self.assertIn("targetIdentity", data)
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=10)
        # Let the bridge observe the death (idle pump between commands).
        deadline = time.monotonic() + 15
        gone = False
        while time.monotonic() < deadline:
            try:
                self.cli(name2, "threads", timeout=5)
            except AssertionError as e:
                if "exited" in str(e).lower():
                    gone = True
                    break
            time.sleep(0.2)
        data = self.cli(name2, "capture", "--timeout", "3", ok=False)
        self.assertFalse(data["ok"])
        self.assertIn("exited", data["error"].lower())
        self.assertNotIn("no session", data["error"].lower())
        self.assertNotIn("rejected", data["error"].lower())
        self.assertNotIn("unreachable", data["error"].lower())
        ctx2 = data.get("waitContext") or {}
        self.assertIn(ctx2.get("captureStage"),
                      ("session-gone", "armed-wait"))
        self.assertEqual(ctx2.get("triggerStatus"), "unknown")
        # Armed-wait via a raced kill: never-hit break keeps the attach
        # running, capture waits, the kill lands mid-wait.
        port2 = free_port()
        proc2 = self._launch_target(
            [str(VENV_PY), "-Xfrozen_modules=off", "-m", "debugpy",
             "--listen", f"127.0.0.1:{port2}", str(self.py_idle)], "shortidle")
        self._wait_log("shortidle", "ready")
        time.sleep(1)
        name3 = "armed-py"
        self.sessions.add(name3)
        data = self.cli(name3, "py", "attach", "--port", str(port2),
                        "--break", f"{self.py_idle}:7", "--timeout", "3")
        self.assertTrue(data["running"])
        import concurrent.futures as _fut
        # A different line than the attach break, so the capture plants
        # its own ephemeral. Line 8 (print "hit") never executes without
        # the gate file, so the kill below lands mid-wait, after the
        # plant confirmed. (Line 6 is the loop condition itself — it
        # executes every iteration and would park immediately.)
        cap_spec = f"{self.py_idle}:8"
        with _fut.ThreadPoolExecutor(max_workers=1) as pool:
            fut = pool.submit(self.cli, name3, "capture", "--break",
                              cap_spec, "--timeout", "15", ok=False)
            time.sleep(3)  # capture is armed and waiting
            proc2.terminate()
            try:
                proc2.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc2.kill()
                proc2.wait(timeout=10)
            data = fut.result(timeout=30)
        self.assertFalse(data["ok"])
        self.assertIn("target exited before capture hit", data["error"])
        self.assertNotIn("rejected", data["error"].lower())
        self.assertNotIn("unreachable", data["error"].lower())
        ctx3 = data.get("waitContext") or {}
        self.assertEqual(ctx3.get("captureStage"), "armed-wait")
        self.assertTrue(ctx3.get("ephemeralPlanted"))
        self.assertEqual(ctx3.get("expectedBreak"), cap_spec)
        for n in (name2, name3):
            try:
                self.cli(n, "close", timeout=85)
            except Exception:
                pass
            try:
                self.sessions.remove(n)
            except KeyError:
                pass

    def test_36_track_outside_display_window_py_and_node(self):
        """Display-independent change tracking, live: 30 frame locals
        (past the MAX_VARS=20 display cap) with `total` outside the
        first-20 window changing 11 -> 12 between two stops. The second
        stop reports total in changed with changedComplete=true, while
        vars/capture stay capped 20 + sentinel. First baseline is []
        + first-snapshot (unknown, never all-locals-as-changed)."""
        if shutil.which("node") is None:
            raise unittest.SkipTest("node unavailable")
        cases = []
        py_prog = self.fixture / "track30.py"
        py_lines = ["import time"]
        for k in range(30):
            py_lines.append(f"v{k:02d} = {k}")
        py_lines += ['total = 11', 'print("ready", flush=True)',
                     'total = 12', 'print(total, flush=True)',
                     'time.sleep(30)']
        py_prog.write_text("\n".join(py_lines) + "\n")
        cases.append(("py", "track36-py", str(py_prog),
                      len(py_lines) - 3, len(py_lines) - 1))
        js_prog = self.fixture / "track30.js"
        js_lines = []
        for k in range(30):
            js_lines.append(f"let v{k:02d} = {k};")
        js_lines += ['let total = 11;', 'console.log("ready");',
                     'total = 12;', 'console.log(total);',
                     'setTimeout(() => {}, 30000);']
        js_prog.write_text("\n".join(js_lines) + "\n")
        cases.append(("node", "track36-node", str(js_prog),
                      len(js_lines) - 3, len(js_lines) - 1))
        for lang, name, prog, b1, b2 in cases:
            self.sessions.add(name)
            try:
                data = self.cli(name, lang, "start", prog,
                                "--break", f"{prog}:{b1}",
                                "--break", f"{prog}:{b2}", "--timeout", "20")
                self.assertEqual(data["location"]["line"], b1)
                locs = data["frames"][0]["locals"]
                self.assertEqual(locs[-1]["name"], "\u2026")
                self.assertEqual(len([v for v in locs
                                      if v["name"] != "\u2026"]), 20)
                parked = self.cli(name, "wait", "--timeout", "5")
                self.assertEqual(parked["changed"], [])
                self.assertFalse(parked["changedComplete"])
                self.assertEqual(parked["changeTracking"]["reason"],
                                 "first-snapshot")
                self.assertIn("trackingWarning", parked)
                resumed = self.cli(name, "continue", "--timeout", "20")
                self.assertEqual(resumed["snapshot"]["location"]["line"], b2)
                self.assertEqual(resumed["changed"], ["total"])
                self.assertEqual(resumed["removed"], [])
                self.assertTrue(resumed["changedComplete"])
                self.assertFalse(
                    resumed["changeTracking"]["truncated"])
                self.assertNotIn("trackingWarning", resumed)
                cap = self.cli(name, "capture", "--timeout", "10")
                cap_locs = cap["snapshot"]["frames"][0]["locals"]
                self.assertEqual(cap_locs[-1]["name"], "\u2026")
            finally:
                try:
                    self.cli(name, "close", timeout=85)
                except Exception:
                    pass
                try:
                    self.sessions.remove(name)
                except KeyError:
                    pass

    def _m1_target(self, lang, port):
        if lang == "py":
            return [str(VENV_PY), "-Xfrozen_modules=off", "-m", "debugpy",
                    "--listen", f"127.0.0.1:{port}", str(self.py_idle)]
        if lang == "node":
            return ["node", f"--inspect=127.0.0.1:{port}", str(self.js_idle)]
        return ["java", f"-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:{port}",
                "-cp", str(self.fixture), "IdleAttach"]


if __name__ == "__main__":
    unittest.main(verbosity=2)
