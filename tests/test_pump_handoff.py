"""M2 regression: a contended idle consumer must not starve a resume waiter.

Lost-wakeup shape (pre-fix): the serve loop's stale `not outstanding` check
releases `_gate`, then `idle_pump` consumes the waiter's `stopped` event and
parks it on the IDLE thread's thread-local. The waiter's own pump finds an
empty stash/wire and runs to `StopTimeout` even though the stop was consumed
and parked. Post-fix the park is handed off via the shared target+epoch
(`_stop_seq`/`_last_park_target`), so every waiter observes it.

Deterministic: the waiter settles past its initial drain (0.5s) before the
rival consumes; the rival's consume-then-park window is microseconds on the
same thread, so the waiter can never win it by accident. Looped N=15: a
single starved iteration fails the test.
"""
import importlib.util
import socket
import tempfile
import threading
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("pybridge", ROOT / "bridge/py/src/pybridge.py")
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)

STOP = {"type": "event", "event": "stopped",
        "body": {"reason": "breakpoint", "threadId": 7}}
FRAME = {"id": 1, "name": "m", "source": {"path": "/tmp/a.py"}, "line": 3}

ITERATIONS = 15


class FakeConn:
    """Stash-backed DAP stand-in over a real (never-readable) socketpair
    end: both pumps drain the stash; wire reads always come back empty so
    rivalry is purely over stash consumption — the lost-wakeup shape."""

    def __init__(self):
        self.stash = []
        self.sock, self.peer = socket.socketpair()

    def pop_stash(self):
        return self.stash.pop(0) if self.stash else None

    def _read_msg(self):
        raise socket.timeout()

    def close(self):
        for s in (self.sock, self.peer):
            try:
                s.close()
            except OSError:
                pass


def make_session(tmp):
    cfg = bridge.Config()
    st = bridge.Session(cfg)
    st.cfg.dir = tmp
    st._nonce = bridge.write_owner(tmp)
    st.publish_state = Mock()
    st.refresh_frames = Mock(
        side_effect=lambda timeout=5: st.frames.append(dict(FRAME)))
    st.dap = FakeConn()
    return st


class PumpHandoffTests(unittest.TestCase):
    def test_idle_steal_does_not_starve_waiter(self):
        for i in range(ITERATIONS):
            with tempfile.TemporaryDirectory() as tmp:
                st = make_session(tmp)
                entered = threading.Event()
                outcome = {}

                def waiter():
                    entered.set()
                    try:
                        outcome["r"] = st.pump(4)
                    except Exception as e:  # noqa: BLE001 - recorded, asserted below
                        outcome["e"] = e

                t = threading.Thread(target=waiter, daemon=True)
                t.start()
                self.assertTrue(entered.wait(5), f"iter {i}: waiter never started")
                time.sleep(0.5)  # waiter past its initial drain, idling
                st.dap.stash.append(dict(STOP))
                bridge.idle_pump(st)  # rival consumer parks on ITS thread-local
                t.join(10)
                st.dap.close()
                self.assertFalse(t.is_alive(), f"iter {i}: waiter thread hung")
                self.assertIn("r", outcome,
                              f"iter {i}: waiter lost the parked stop: {outcome.get('e')!r}")
                self.assertEqual(outcome["r"], "stopped", f"iter {i}")
                self.assertTrue(st.suspended, f"iter {i}: stop was never parked")
                self.assertEqual(st._last_park_target, "main", f"iter {i}")

    def test_explicit_waiter_ignores_other_target_park(self):
        # Target isolation through the handoff: a park for another target
        # never satisfies an explicit waiter, and the foreign park stays
        # parked (visible, never consumed).
        with tempfile.TemporaryDirectory() as tmp:
            st = make_session(tmp)
            child = bridge.ChildTarget("child:61", 61, Mock(), Mock(),
                                       {"pid": 61, "source": "t"})
            st.targets["child:61"] = child
            st.target_order.append("child:61")
            st._seen_ids.add("child:61")
            child.dap = FakeConn()
            child.suspended = True
            child.state = "stopped"
            with st._gate:
                st._stop_seq += 1
                child.stop_seq = st._stop_seq
                st._last_park_target = "child:61"
            try:
                with self.assertRaises(bridge.StopTimeout):
                    st.pump(1, "main")
            finally:
                st.dap.close()
                child.dap.close()

    def test_shared_handoff_rejects_resumed_or_foreign_park(self):
        # The shared epoch is not enough on its own: an omitted waiter must
        # also observe the target's current suspended state under the same
        # gate. A park epoch left behind after resume, or a child park while
        # the child is running, is not a fresh handoff.
        with tempfile.TemporaryDirectory() as tmp:
            st = make_session(tmp)
            with st._gate:
                st._stop_seq = 4
                st._last_park_target = "main"
                st._park_seq["main"] = 4
                st.suspended = False  # resumed after the old park
            self.assertIsNone(st._shared_park_hit(3, None))

            child = bridge.ChildTarget("child:61", 61, Mock(), Mock(),
                                       {"pid": 61, "source": "t"})
            st.targets["child:61"] = child
            st.target_order.append("child:61")
            child.suspended = False  # foreign epoch belongs to a running child
            with st._gate:
                st._stop_seq = 5
                st._last_park_target = "child:61"
                st._park_seq["child:61"] = 5
            self.assertIsNone(st._shared_park_hit(4, None))

    def test_retiring_child_drops_park_epoch(self):
        with tempfile.TemporaryDirectory() as tmp:
            st = make_session(tmp)
            child = bridge.ChildTarget("child:61", 61, Mock(), Mock(),
                                       {"pid": 61, "source": "t"})
            st.targets["child:61"] = child
            st.target_order.append("child:61")
            st._park_seq["child:61"] = 9
            st._note_exit("child:61")
            self.assertNotIn("child:61", st._park_seq)


if __name__ == "__main__":
    unittest.main()
