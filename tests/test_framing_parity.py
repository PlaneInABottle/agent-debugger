"""M2 framing parity: shared contract fixtures across Python/JS/Java.

Drives the real bridge.read_frame against tests/contract/framing.json:
every negative must reject, every valid shape must accept with its body.
Category parity only (reject vs accept), never message text.
"""
import importlib.util
import json
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("pybridge", ROOT / "bridge/py/src/pybridge.py")
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)

FIXTURE = json.loads((ROOT / "tests/contract/framing.json").read_text(encoding="utf-8"))


class FeedConn:
    """Single-payload conn: serves the raw bytes once, then EOF."""

    def __init__(self, payload: bytes):
        self._payload = payload
        self._timeout = 5

    def settimeout(self, t):
        self._timeout = t

    def recv(self, size):
        if not self._payload:
            return b""
        chunk, self._payload = self._payload[:size], self._payload[size:]
        return chunk


def expand(case):
    if "raw" in case:
        return case["raw"].encode("utf-8")
    rep = case["rawRepeat"]
    return (rep["prefix"] + rep["char"] * rep["count"]).encode("utf-8")


class FramingParityTests(unittest.TestCase):
    def test_shared_fixture_categories(self):
        for case in FIXTURE["cases"]:
            with self.subTest(case["name"]):
                payload = expand(case)
                if case["expect"] == "reject":
                    with self.assertRaises(bridge.BridgeErr, msg=case["name"]):
                        bridge.read_frame(FeedConn(payload))
                else:
                    req = bridge.read_frame(FeedConn(payload))
                    self.assertEqual(req, json.loads(case["body"]), case["name"])

    def test_body_at_cap_accepts(self):
        # Identical parameters in all three harnesses: a 1048576-byte body
        # (the exact cap) is accepted, proving the 1M bound itself did not
        # move while negatives tightened.
        inner = "x" * (1048576 - 8)
        body = '{"k":"' + inner + '"}'
        assert len(body.encode("utf-8")) == 1048576
        raw = ("Content-Length: 1048576\r\n\r\n" + body).encode("utf-8")
        req = bridge.read_frame(FeedConn(raw))
        self.assertEqual(req, {"k": inner})


if __name__ == "__main__":
    unittest.main()
