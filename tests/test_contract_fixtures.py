"""Cross-language contract fixtures: single source for frozen strings.

Loads tests/contract/*.json and asserts the Python bridge honors them.
Mirror consumers: tests/contract_fixtures.test.js (Node), src/bridge.rs +
src/session tests (Rust, via include_str!). No new dependency.
"""
import importlib.util
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock

ROOT = Path(__file__).resolve().parents[1]
CONTRACT = ROOT / "tests/contract"
spec = importlib.util.spec_from_file_location("pybridge", ROOT / "bridge/py/src/pybridge.py")
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)


def fixture(name):
    # Explicit UTF-8: several fixtures carry non-ASCII frozen strings
    # (`…`), and the platform locale must never decide how they decode.
    return json.loads((CONTRACT / name).read_text(encoding="utf-8"))


class ContractFixtureTests(unittest.TestCase):
    def test_timeout_prefix(self):
        fx = fixture("timeout_prefix.json")
        st = bridge.Session(bridge.Config())
        msg = st.timeout_text(2)
        self.assertTrue(msg.startswith(fx["prefix"] + " "),
                        f"{msg!r} must start with frozen prefix")
        self.assertIn(fx["example"], msg)
        src = (ROOT / "bridge/py/src/pybridge.py").read_text()
        for stage in fx["waitContext"]["captureStageValues"]:
            self.assertIn(f'"{stage}"', src, f"captureStage {stage} must stay frozen")

    def test_breaks_echo(self):
        fx = fixture("breaks_echo.json")
        self.assertTrue(fx["confirmedOnlyPersistence"])
        self.assertTrue(fx["totalFailureOkFalse"])
        # Behavioral: total DAP failure raises (framed layer maps the
        # throw to ok:false) with nothing confirmed or persisted.
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.realpath(str(Path(tmp) / "a.py"))
            Path(path).write_text("".join(f"line {n}\n" for n in range(12)))
            st = bridge.Session(bridge.Config())
            st.cfg.dir = tmp
            st.dap_request = Mock(side_effect=bridge.BridgeErr("adapter exploded"))
            with self.assertRaises(bridge.BridgeErr):
                st.cmd_breaks_add({"breaks": [f"{path}:5"]})
            self.assertEqual(st.cfg.breaks, [])
            self.assertEqual(st.stop_states, [])
        # Behavioral: removing an unknown break echoes empty removed[]
        # (confirmed-only: nothing confirmed, nothing persisted).
        st = bridge.Session(bridge.Config())
        resp = st.cmd_breaks_remove({"breaks": ["ghost.py:9"]})
        self.assertTrue(resp["ok"])
        self.assertEqual(resp["removed"], [])

    def test_identity_caps(self):
        fx = fixture("identity_caps.json")
        self.assertEqual(bridge.IDENT_FIELD_CAP, fx["fieldCap"])
        self.assertEqual(bridge.IDENT_TOTAL_CAP, fx["bridgeTotalCap"])
        capped = bridge._cap_str("y" * 600, fx["fieldCap"])
        suffix = fx["truncSuffixFormat"].replace("N", str(600 - fx["fieldCap"]))
        # Codepoint-level diagnostics (ASCII-safe for any log pipeline):
        # a past Windows run produced U+FFFD where U+2026 belongs, with
        # correct file bytes — unreproducible locally, so a recurrence
        # must arrive with ground truth attached.
        self.assertIn(
            suffix,
            capped,
            f"suffix codepoints={[hex(ord(c)) for c in suffix]} "
            f"capped tail codepoints={[hex(ord(c)) for c in capped[505:525]]}",
        )
        redacted = bridge.redact_identity_argv(["run", "--password=hunter2"])
        self.assertNotIn("hunter2", json.dumps(redacted))
        self.assertIn(fx["redacted"], json.dumps(redacted))

    def test_error_phases(self):
        fx = fixture("error_phases.json")
        self.assertEqual(bridge.phase_of_error(bridge.Usage("x")), "config")
        self.assertEqual(bridge.phase_of_error(bridge.RuntimeErr("bug")), "runtime")
        self.assertIn("transport", fx["phases"])
        msg = fx["corruptSetupErrorTemplate"].format(name="demo")
        self.assertIn("demo", msg)
        self.assertIn("close and retry", msg)

    def test_close_status(self):
        fx = fixture("close_status.json")
        # Behavioral: the daemon ACKs a framed close with bridgeCloseAck
        # (boolean closed, target main); the CLI maps it to the cliClose
        # shape with the session name + confirmed flag, held behaviorally
        # by the live suites (test_live asserts close["confirmed"]).
        st = bridge.Session(bridge.Config())
        st.cleanup = lambda: None
        sent = []

        class FakeConn:
            def sendall(self, data):
                sent.append(data)

            def close(self):
                pass

        bridge._close_from_conn(st, FakeConn())
        head, body = b"".join(sent).split(b"\r\n\r\n", 1)
        self.assertEqual(json.loads(body), fx["bridgeCloseAck"])
        self.assertEqual(fx["close"]["target"], "main")
        self.assertEqual(fx["close"]["confirmedField"], "confirmed")
        self.assertEqual(fx["closeConfirmedExample"]["target"], "main")
        self.assertTrue(fx["closeConfirmedExample"]["confirmed"])
        self.assertFalse(fx["closeUnconfirmedExample"]["confirmed"])


if __name__ == "__main__":
    unittest.main()
