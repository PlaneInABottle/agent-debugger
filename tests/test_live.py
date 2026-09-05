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
import tempfile
import threading
import time
import unittest
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / "target/debug/agent-debugger"
VENV_PY = Path.home() / ".agent-debugger/adapters/python/venv/bin/python"


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class LiveTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory(prefix="debugger-regression-")
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
        cls.fixture = cls.home / "fixture with spaces"
        cls.fixture.mkdir()
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
    def cli(cls, name, *args, timeout=30, ok=True, env=None):
        result = subprocess.run([str(BIN), "--session", name, *args], env=env or cls.env,
                                cwd=ROOT, capture_output=True, text=True, timeout=timeout)
        data = json.loads(result.stdout)
        if ok and (result.returncode or not data.get("ok")):
            raise AssertionError(f"{name} {args}: {data}, stderr={result.stderr}")
        return data.get("data", data)

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
        chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
        if not Path(chrome).exists():
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
        chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
        if not Path(chrome).exists():
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
