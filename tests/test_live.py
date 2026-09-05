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


if __name__ == "__main__":
    unittest.main(verbosity=2)
