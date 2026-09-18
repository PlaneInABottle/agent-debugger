# AGENTS.md — agent-debugger

Headless debugging CLI (Rust) plus four protocol bridges: Node (raw CDP),
Python (debugpy/DAP), Java (JDWP), browser (CDP tab attach). Snapshot-first
sessions, one CLI invocation per round trip.

This file is the operating contract for working IN this repository.
- To USE the CLI against an application: `skills/agent-debugger/SKILL.md`.
- To CHANGE this repo or debug its CI: `skills/agent-debugger-maintainer/SKILL.md`.

## Layout

- `src/` — Rust CLI: session lifecycle, TCP client, spawn, locks, attach probes.
- `bridge/node/src/nodebridge.js`, `bridge/py/src/pybridge.py`,
  `bridge/java/`, `bridge/browser/src/browserbridge.js` — the bridges.
- `tests/` — unit suites (`tests/*.py`, `tests/*.test.js`) and live suites
  (`test_live.py`, `test_m5_live.py`, `test_ux_live.py`) sharing the harness
  `tests/_live_home.py`.
- `scripts/run_gates.sh` — canonical gate runner (`--unit`, `--live`).
- `.github/workflows/ci.yml` — MSRV (1.89), Test ubuntu/macos/windows, Live (ubuntu).

## Canonical commands

```bash
cargo build                                          # build CLI + wire bridges
./scripts/run_gates.sh --unit                        # all unit gates
cargo fmt --check
cargo check --target x86_64-pc-windows-msvc --tests  # Windows compile gate
actionlint .github/workflows/ci.yml

python3 tests/test_live.py LiveTests.<name>          # one live test
python3 tests/test_m5_live.py M5LiveTests.<name>
python3 tests/test_ux_live.py UxLiveTests.<name>
SKIP_BROWSER=1 python3 tests/run_live.py             # full local live set (py/node/java)
LIVE_RETRY=0 python3 tests/run_live.py               # disable retry-once
```

- Live tests need provisioned deps in the real HOME
  (`~/.agent-debugger/adapters/python/venv`, `.../node/node_modules`); the
  harness copies them into an isolated temp HOME. Browser tests need Chrome.
- Run locally before pushing; CI is the only Windows/browser/clean-runner truth.

## CI failure evidence

- Live job runs `scripts/run_gates.sh --live` through `tests/run_live.py` with
  retry-once: "healed by retry" = spike, "still failing after retry" = genuine.
- On failure CI uploads the `live-failure-bundles` artifact: per-session
  `bridge.log`, `session.json`, `error.json`, `stops.json` for the failing
  test's sessions, plus `pressure.log` (host pressure sampler).
  Fetch with `gh run view <id> --json jobs` and `gh api .../artifacts/<id>/zip`.
- Local runs write the same bundles to
  `$TMPDIR/live-failure-bundles/<Class>/<session>/`.

## Diagnostics already installed

- nodebridge: ISO-timestamped `trace:` stderr lines for worker parked/detached
  and step/resume sends; wait timeouts carry
  `[parks: main=... worker:N=... lastPark=...]`.
- pybridge: stall watchdog dumps all thread stacks to stderr once when a CLI
  handler exceeds its own budget+margin or the accept loop stops beating.
- Bridge stderr is `<session dir>/bridge.log`; the live harness refreshes the
  bundle copy on every CLI call (close deletes the session dir).

## Invariants that past CI failures violated

1. A step/continue freshness baseline is captured BEFORE the request that can
   produce the park (`resumeAndWait(..., base)`); CDP frames coalesce and the
   pause can be recorded before the caller's continuation runs.
2. DAP read bounds are applied INSIDE the connection lock
   (`DapConn._read_msg(timeout=...)`); never arm a shared socket outside it.
3. Worker park + selection clock (`stopSeq`) go live synchronously, before
   enrichment awaits.
4. Close uses FIN (`conn.end()`), never RST (`destroy()`).
5. Register both failure paths when changing waits: a timeout must clear the
   step flag but never disarm a concurrent fresh park.

## Boundaries

- Tests must never mutate the real `~/.agent-debugger`; only the temp HOME.
- No local Windows host; POSIX-only tests skip on win32.
- Never loosen strict timeout assertions to green a flake; fix the cause or
  let retry-once heal a genuine spike.
- Keep diagnostics bounded and stderr-only; no unbounded waits, no background
  processes that outlive their CI step.
- Small, one-concern commits; diagnostics, fixes and harness patience separate.
