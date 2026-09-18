---
name: agent-debugger-maintainer
description: Maintain and debug the agent-debugger repository itself. Use when changing the Rust CLI or the node/python/java/browser bridges, running or extending the live tests, triaging intermittent CI or live failures and hangs, adding bridge diagnostics, or writing deterministic race regressions. Not for using the CLI against an application (use the agent-debugger skill for that).
---

# Skill: agent-debugger-maintainer

Working ON the debugger: the Rust CLI, four protocol bridges, the live
harness and CI. If the task is "debug my app with this CLI", load the
sibling `agent-debugger` skill instead.

## Ground rules

1. **Evidence before hypotheses.** A live failure must produce its own
   forensics in the run that failed: trace lines, log tails, a stack.
   If the failing run cannot explain itself, fix that first — an
   uninstrumented failure cannot be diagnosed, only guessed at.
2. **Run locally what can run locally.** Reserve CI cycles for
   Windows/browser/clean-runner facts and the final push.
3. **Reproduce at the unit level.** A race that shows up once per dozen
   loaded runs gets a deterministic regression that drives the real
   code paths (chains, locks, pauses) — never a sleep/retry workaround.
4. **Never loosen an assertion to green a flake.** Retry-once heals only
   retries that actually pass; "still failing after retry" is genuine
   and must be fixed.

## Canonical commands (verified)

```bash
cargo build                                        # CLI + bridges wiring
./scripts/run_gates.sh --unit                      # all unit gates (cargo/node/py/java/fmt/owners)
cargo fmt --check
cargo check --target x86_64-pc-windows-msvc --tests  # Windows compile gate (no local host)
actionlint .github/workflows/ci.yml

python3 tests/test_live.py LiveTests.<test>         # one live test
python3 tests/test_m5_live.py M5LiveTests.<test>
python3 tests/test_ux_live.py UxLiveTests.<test>
SKIP_BROWSER=1 python3 tests/run_live.py            # full local live set (py/node/java)
LIVE_RETRY=0 python3 tests/run_live.py              # disable retry-once when isolating
```

Local live tests copy the real HOME's provisioned adapter deps
(`~/.agent-debugger/adapters/python/venv`, `~/.agent-debugger/adapters/node/node_modules`) into an
isolated temp HOME; they skip without them. Browser tests need Chrome.

## Flake triage loop

1. **Classify.** Locally reproducible (fix locally), or CI-only?
   CI-only families usually involve cross-process timing (CDP/DAP frame
   coalescing, loaded-runner scheduling) that fast local loopback does
   not reproduce. Passing locally is not evidence of a flake.
2. **Harvest one failure's evidence.** `gh run view <id> --json jobs` →
   failed job id → `gh api repos/<owner>/<repo>/actions/jobs/<job>/logs`.
   Download the `live-failure-bundles` artifact: per-session
   `bridge.log`, `session.json`, `stops.json`, `error.json`, plus the
   host pressure log. Correlate the CLI error envelope (`[parks: ...]`,
   timeout text) with bridge.log event order and ISO timestamps.
3. **If the evidence is silent, instrument before fixing:**
   - nodebridge: one-line `trace:` stderr lines with ISO timestamps at
     state transitions (park/resume/step/detach); keep ordering truthful
     (write at the recorded event, not after enrichment delays).
   - pybridge: the stall watchdog (`STALL_DUMP_S`) dumps all thread
     stacks once when a handler exceeds its own budget+margin or the
     accept loop stops beating; wait commands use their `--timeout` so
     legitimate long waits stay quiet.
   - harness: per-call bundle refresh so a failing call's state survives
     `close` deleting the session dir; ship bundles + pressure as CI
     artifacts on failure.
4. **Write the regression RED first.** Recreate the exact CI signature
   against the current code (see patterns below), then fix.
5. **Fix, verify, push small, watch the run.** Recheck the run's retry
   verdict: healed = spike; still failing = genuine.

## Race patterns this codebase has produced

Signature-first recipes (symptom → first evidence → fix class) live in
[references/ci-flake-playbooks.md](references/ci-flake-playbooks.md);
the invariants they point at:

- **Selection baseline after the fact.** `cmdStep`/`cmdContinue` must
  capture the freshness baseline *before* sending the request and pass
  it to `pumpForStop(..., base)`; a baseline captured at pump entry can
  equal the very park it must serve when the response and the `paused`
  event coalesce in one read (symptom: `timeout ... [parks:
  worker:N=parked lastPark=worker:N]` while the park is right there).
- **Shared socket bounds armed outside a lock.** DAP readers pass their
  bound into `DapConn._read_msg(timeout=...)`, which applies it under
  `mu`; arming the socket first lets a handler re-arm its multi-second
  deadline while a bounded reader waits on the lock, stalling the accept
  loop (symptom: every CLI call reads `timed out waiting for debug
  session` while the bridge is alive).
- **Wait pump blind to a pre-lock park (Java).** A park committed between
  the accept-time suspended check and the pump's lock acquisition strands
  the pump on a suspended VM's empty queue until deadline, when the
  deadline `parkedRecheck` returns the same park at full budget (symptom:
  `wait` ok:true but client-measured dt >= the whole `--timeout`, twice
  in a row). Fix: `parkedRecheck` immediately after `pumpLock` acquisition
  (non-idle only); only pumps park and the lock is held, so anything
  visible is newer than the check.
- **Park visibility vs enrichment.** Worker park + selection clock
  (`stopSeq`) go live synchronously before any `trackChanges`/
  `fireLogpoint` await, so `resolveTarget`/pump see the stop immediately.
- **Close semantics.** Use `conn.end()` (FIN), not `destroy()` (RST):
  RST discards the peer's close ACK (`truncated frame`).
- **Wall-clock assertions vs host pressure.** "wait woke on timeout"
  with the park intact is usually load (check pressure.log), not a
  product bug; release heavy fixtures (Chrome) as soon as their test
  ends instead of at class cleanup.

## Regression-test patterns

- Node bridge: `tests/m5_concurrency.test.js` loads the real bridge via
  `loadBridge(...)`. Drive the real chains — e.g. queue a pause through
  `_chainPause(() => _swapRun(() => onWorkerPaused(w, p)))` while a
  request stub is in flight to recreate same-read delivery. Stub
  `pump`/`trackChanges`, keep `pumpForStop` real when asserting
  selection/freshness.
- Python bridge: `tests/test_pybridge.py` imports `pybridge.py` by path;
  socketpairs + threads recreate lock/read races. Stubs must match the
  production signatures (e.g. `_read_msg(self, timeout=None)`).
- Pump handoff: `tests/test_pump_handoff.py` drives stash/wire rivalry
  between concurrent pumps.
- Live-level cases live in `tests/test_live.py` and keep their strict
  timeout assertions; add new cases rather than editing existing ones.

## Diagnostics inventory

| Evidence | Where |
|---|---|
| bridge stderr, traces, watchdog stacks | `<session dir>/bridge.log` |
| park truth at timeout | `[parks: ...]` suffix / `parkInventory()` |
| session + stop state | `session.json`, `stops.json`, `error.json` |
| live failure bundle (local) | `$TMPDIR/live-failure-bundles/<Class>/<session>/` |
| live failure bundle (CI) | artifact `live-failure-bundles` |
| host pressure (CI) | `pressure.log` in the same artifact |
| CLI request budget | `src/client.rs` (connect + read deadline) |

## Boundaries

- Tests must never mutate the real `~/.agent-debugger`; they copy
  provisioned adapter deps into an isolated temp HOME.
- No local Windows host: validate Windows via
  `cargo check --target x86_64-pc-windows-msvc --tests` and CI; POSIX-only
  tests skip on win32.
- Diagnostics must be bounded and stderr-only (bridge.log ships with
  bundles); never add an unbounded wait or a background process that
  outlives its step.
- Keep commits small and one-concern: diagnostics, fixes and harness
  patience are separate commits.
