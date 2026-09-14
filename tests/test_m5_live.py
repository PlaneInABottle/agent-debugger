"""M5 targeted live concurrency: outstanding resume + prompt live reads +
immediate busy rivals on all four adapters; different-target independence
(py/node); close-during-resume (launch reaps, attach detaches).

Run after cargo build; owns all fixtures/processes. Uses existing
debugpy/ws installations without installing dependencies. Not part of the
legacy full suite (final gate runs that); run explicitly:
    python3 tests/test_m5_live.py
"""
import concurrent.futures
import functools
import http.server
import json
import os
from pathlib import Path
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

PROMPT_BUDGET = 8.0  # live reads / busy rivals must answer well under this


class M5LiveTests(LiveHomeMixin, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.setup_home("debugger-m5-live-", "m5fixture")
        (cls.fixture / "spin.py").write_text(
            'import time\nfor i in range(400):\n    value = i + 1\n'
            '    print(value, flush=True)\n    time.sleep(0.2)\n')
        (cls.fixture / "spin.js").write_text(
            'let v = 0;\nsetInterval(() => { v += 1; console.log(v); }, 200);\n')
        (cls.fixture / "Spin.java").write_text(
            'public class Spin {\n'
            ' public static void main(String[] args) throws Exception {\n'
            '  for (int i=0;i<400;i++) {\n'
            '   int value=i+1;\n'
            '   System.out.println(value);\n'
            '   Thread.sleep(200);\n'
            '  }\n }\n}\n')
        subprocess.run(["javac", "-g", "-d", str(cls.fixture),
                        str(cls.fixture / "Spin.java")],
                       check=True, timeout=30)

    def background(self, pool, name, *args, **kwargs):
        return pool.submit(self.cli, name, *args, **{**kwargs, "ok": False})

    def assert_prompt_ok(self, name, *args, expect=()):
        t0 = time.monotonic()
        data = self.cli(name, *args, timeout=30)
        dt = time.monotonic() - t0
        self.assertLess(dt, PROMPT_BUDGET, f"{args} took {dt:.1f}s")
        for key in expect:
            self.assertIn(key, data)
        return data

    def assert_busy(self, name, *args, target="main", cmd="continue"):
        t0 = time.monotonic()
        data = self.cli(name, *args, timeout=30, ok=False)
        dt = time.monotonic() - t0
        self.assertLess(dt, PROMPT_BUDGET, f"{args} took {dt:.1f}s")
        self.assertIn("error", data)
        self.assertIn(f"busy: {cmd} outstanding for {target}", data["error"])
        return data

    def run_held_resume(self, name, start_args, resume_args=("continue",)):
        """Start a running (unstopped) session, hold a resume outstanding in
        the background, assert prompt live reads + immediate busy rivals +
        fail-fast frame reads, then close through the outstanding resume."""
        self.sessions.add(name)
        try:
            self.cli(name, *start_args, timeout=60)
            pool = concurrent.futures.ThreadPoolExecutor(max_workers=1)
            try:
                bg = self.background(pool, name, *resume_args,
                                     "--timeout", "15", timeout=40)
                time.sleep(1.5)  # let the resume register outstanding
                self.assert_prompt_ok(name, "threads", expect=("threads",))
                self.assert_prompt_ok(name, "breaks", expect=("stops",))
                self.assert_prompt_ok(name, "logs", expect=("lines",))
                self.assert_busy(name, "continue")
                self.assert_busy(name, "step", "over", cmd="continue")
                self.assert_busy(name, "breaks", "add", "--break",
                                 "definitely-not-a-file.py:1", cmd="continue")
                self.assert_busy(name, "eval", "1+1", cmd="continue")
                ctx = self.cli(name, "context", timeout=30, ok=False)
                self.assertIn("no stopped thread", ctx["error"])
                # Close is accepted despite the outstanding resume.
                self.assertTrue(self.cli(name, "close")["confirmed"])
                # The background resume terminates (no hang, no deadlock).
                bg.result(timeout=30)
            finally:
                pool.shutdown(wait=True)
            self.sessions.remove(name)
        finally:
            if name in self.sessions:
                try:
                    self.cli(name, "close", timeout=85)
                except Exception:
                    pass
                self.sessions.remove(name)

    def test_m5_py_held_resume(self):
        self.run_held_resume("m5-py", ["py", "start",
                                       str(self.fixture / "spin.py")])

    def test_m5_node_held_resume(self):
        self.run_held_resume("m5-node", ["node", "start",
                                         str(self.fixture / "spin.js")])

    def test_m5_java_held_resume(self):
        self.run_held_resume("m5-java", ["java", "start", "--main", "Spin",
                                         "--cp", str(self.fixture)])

    def test_m5_browser_held_resume_and_detach(self):
        chrome = find_chrome()
        if chrome is None:
            self.skipTest("Chrome unavailable")

        class QuietHandler(http.server.SimpleHTTPRequestHandler):
            def log_message(self, *args):
                pass

        handler = functools.partial(QuietHandler,
                                    directory=str(ROOT / "examples/browser-demo"))
        type(self).http = http.server.ThreadingHTTPServer(("127.0.0.1", 0),
                                                          handler)
        threading.Thread(target=self.http.serve_forever, daemon=True).start()
        cdp_port = free_port()
        url = f"http://127.0.0.1:{self.http.server_port}/index.html"
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
        name = "m5-browser"
        self.sessions.add(name)
        try:
            # Attach with no stops: the tab is idle after load.
            self.cli(name, "browser", "attach", "--port", str(cdp_port),
                     "--tab", url, timeout=60)
            pool = concurrent.futures.ThreadPoolExecutor(max_workers=1)
            try:
                bg = self.background(pool, name, "continue",
                                     "--timeout", "15", timeout=40)
                time.sleep(1.5)
                self.assert_prompt_ok(name, "threads", expect=("threads",))
                self.assert_prompt_ok(name, "breaks", expect=("stops",))
                self.assert_prompt_ok(name, "logs", expect=("lines",))
                self.assert_busy(name, "continue")
                self.assert_busy(name, "reload", cmd="continue")
                self.assert_busy(name, "breaks", "add", "--break",
                                 "app.js:1", cmd="continue")
                self.assert_busy(name, "eval", "1+1", cmd="continue")
                ctx = self.cli(name, "context", timeout=30, ok=False)
                self.assertIn("no stopped thread", ctx["error"])
                # Attach close detaches: the tab (and chrome) survive.
                self.assertTrue(self.cli(name, "close")["confirmed"])
                self.assertIsNone(self.chrome.poll())
                bg.result(timeout=30)
            finally:
                pool.shutdown(wait=True)
            self.sessions.remove(name)
        finally:
            if name in self.sessions:
                try:
                    self.cli(name, "close", timeout=85)
                except Exception:
                    pass
                self.sessions.remove(name)

    def test_m5_py_different_target_independent(self):
        # NOTE: explicit `--target main` is defined (Batch2/Rust) as
        # omit-to-auto-select, so main can only be held while NO sibling is
        # parked. Shape: park the child, drop its break, then hold BOTH
        # targets with simultaneous resumes and prove each rival busy while
        # live reads stay prompt.
        child = self.fixture / "m5_child.py"
        child.write_text(
            "import time\nfor i in range(200):\n    tick = i * 2\n"
            "    print(f\"child tick {tick}\", flush=True)\n    time.sleep(0.2)\n")
        parent = self.fixture / "m5_parent.py"
        parent.write_text(
            "import subprocess, sys, os, time\n"
            "here = os.path.dirname(os.path.abspath(__file__))\n"
            "p = subprocess.Popen([sys.executable, os.path.join(here, \"m5_child.py\")])\n"
            "print(\"parent spawned\", flush=True)\n"
            "for i in range(200):\n"
            "    print(f\"parent {i}\", flush=True)\n    time.sleep(0.2)\n"
            "p.wait()\n")
        name = "m5-py-diff"
        self.sessions.add(name)
        try:
            start = self.cli(name, "py", "start", "--subprocess", str(parent),
                             "--break", f"{child}:4", timeout=60)
            self.assertTrue(start["target"].startswith("child:"))
            child_id = start["target"]
            # Drop the only break: nothing can stop anywhere afterwards.
            self.cli(name, "breaks", "remove", "--break", f"{child}:4")
            pool = concurrent.futures.ThreadPoolExecutor(max_workers=2)
            try:
                # Hold the child first (explicit target, parked -> resumes).
                bg_child = self.background(pool, name, "continue", "--target",
                                           child_id, "--timeout", "20",
                                           timeout=40)
                time.sleep(1.5)
                # Then hold main too (targetless auto-select: nothing parked
                # anymore, so main). The second resume is ACCEPTED
                # (independent targets never busy-reject each other).
                bg_main = self.background(pool, name, "continue",
                                          "--timeout", "20", timeout=40)
                time.sleep(1.5)
                # Rivals on either held target busy-reject immediately.
                self.assert_busy(name, "continue", "--target", child_id,
                                 target=child_id)
                self.assert_busy(name, "continue", target="main")
                # Global mutation conflicts with ANY outstanding resume: with
                # both held it deterministically names the sorted-first one.
                self.assert_busy(name, "breaks", "add", "--break",
                                 f"{parent}:7", cmd="continue",
                                 target=child_id)
                self.assert_prompt_ok(name, "threads", expect=("threads",))
                self.assert_prompt_ok(name, "targets", expect=("targets",))
                self.assertTrue(self.cli(name, "close")["confirmed"])
                bg_child.result(timeout=30)
                bg_main.result(timeout=30)
            finally:
                pool.shutdown(wait=True)
            self.sessions.remove(name)
        finally:
            if name in self.sessions:
                try:
                    self.cli(name, "close", timeout=85)
                except Exception:
                    pass
                self.sessions.remove(name)

    def test_m5_node_different_target_independent(self):
        prog = self.fixture / "m5_worker.js"
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
        name = "m5-node-diff"
        self.sessions.add(name)
        try:
            start = self.cli(name, "node", "start", "--workers", str(prog),
                             "--break", f"{prog}:10", timeout=60)
            self.assertTrue(start["target"].startswith("worker:"))
            wid = start["target"]
            # Drop the only break: nothing can stop anywhere afterwards.
            self.cli(name, "breaks", "remove", "--break", f"{prog}:10")
            pool = concurrent.futures.ThreadPoolExecutor(max_workers=2)
            try:
                # Hold the worker first (explicit target, parked -> resumes).
                bg_worker = self.background(pool, name, "continue",
                                            "--target", wid, "--timeout",
                                            "20", timeout=40)
                time.sleep(1.5)
                # Then hold main too (targetless auto-select: nothing parked
                # anymore, so main). The second resume is ACCEPTED.
                bg_main = self.background(pool, name, "continue",
                                          "--timeout", "20", timeout=40)
                time.sleep(1.5)
                # Rivals on either held target busy-reject immediately.
                self.assert_busy(name, "continue", "--target", wid,
                                 target=wid)
                self.assert_busy(name, "continue", target="main")
                self.assert_prompt_ok(name, "threads", expect=("threads",))
                self.assert_prompt_ok(name, "targets", expect=("targets",))
                self.assertTrue(self.cli(name, "close")["confirmed"])
                bg_worker.result(timeout=30)
                bg_main.result(timeout=30)
            finally:
                pool.shutdown(wait=True)
            self.sessions.remove(name)
        finally:
            if name in self.sessions:
                try:
                    self.cli(name, "close", timeout=85)
                except Exception:
                    pass
                self.sessions.remove(name)


if __name__ == "__main__":
    unittest.main()
