# Changelog

All notable changes to this project are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.0.0/); versioning follows
[SemVer](https://semver.org/).

## [Unreleased]

### Removed

- Windows support (macOS/Linux only). The Windows-only paths
  (`taskkill` reaps, `Scripts\` venv layout, `;` separators, lsof-free
  listener probes, win32 npm wrapper branches) were untestable without
  a local Windows host and already produced wrong behavior there
  (misdiagnosed attach verdicts, missed venvs, RST'd close ACKs).
  Rust fails fast with `compile_error!` on Windows; npm refuses via
  the `os` field plus a clear postinstall error; CI and release builds
  are POSIX-only (4 tar.gz). The v0.2.1 Windows asset stays published
  as-is; future releases ship macOS + Linux only.

### Fixed

- `doctor` probes the provisioned venv interpreter via the same
  platform-aware helper as provisioning (`Scripts\python.exe` on
  Windows, not a hardcoded `bin/python`), and resolves the system
  Python through the launcher fallback (`python3` → `python`, since
  `python3` is often absent on Windows).
- Every toolchain probe is bounded: `doctor` probes and Chrome lookup
  go through `run_with_timeout` (15s / 10s) — a hung binary reads as
  absent instead of hanging `doctor` forever.
- Chrome lookup covers Windows install paths (Program Files, user
  `LOCALAPPDATA`, PATH names): browser readiness no longer reports
  not-ready on Windows machines with Chrome installed. A dead list
  entry tries the next candidate instead of aborting the search.
- Windows attach diagnosis no longer misreports live endpoints as
  "not listening" with high confidence: with no local listener source
  (`/proc`, lsof) the source reads unreadable, so the verdict is
  unknown instead of a confident negative.
- Timed-out provision subprocesses are force-killed (SIGKILL-grade
  tree-kill, was SIGTERM-only): a TERM-deaf child no longer lingers
  over the venv it was writing while the parent already failed, racing
  the retry — plus its leaked waiter thread.
- Browser `/json/list` fetch caps the body at 1MB: a rogue target
  list can no longer OOM the bridge by trickling forever.
- Node `/json/list` fetches bounded the same way: `discoverAttach`
  rejects past 1MB, `fetchTargetList` degrades to null.
- Java `waitedMs` and capture `pauseMs` are monotonic: `waitContextJson`
  / `captureExitContextJson` / `stageCaptureExit` take (epochMs, nanos)
  pairs and the park records `parkedAtNanos`, so a wall-clock step can
  no longer report a negative/hours-long wait or silently overrun a
  capture pause budget (old overloads kept for instant contexts).
- Unit-test harness closes the vacuous-pass hole: `tests/guard-exit.js`
  traps `process.exit` in test files (a bridge exiting the runner read
  as file-green), `run_gates.sh` runs each test file separately and
  requires at least one real subtest per file, and
  `scripts/check_negative_controls.sh` reverts 7 key fixes to prove
  each regression test fails without its fix (manual pre-release gate).
- `scripts/check_release.sh` default API base now hits the real
  `/repos/<owner>/<repo>/releases` endpoint (previously 404'd on every
  run, reporting "no release" for a fully published release).
- Node `continue` forwards the pre-resume freshness baseline (same rule
  as `step`): a park landing while the resume is in flight counts fresh
  instead of timing out beside a parked target.
- Exception stops consume a pending step (node main/worker, browser):
  a stale `awaitingStep` misclassified a later stray pause as a step
  landing instead of auto-resuming it.
- Terminal close stays graceful on the normal `handleConn` path (node +
  browser): no `destroy()` after `closeFromConn`'s FIN, which RST'd the
  just-written ACK on Windows.
- Failure-path reap SIGTERMs first (bounded 2s grace, then tree-kill)
  instead of a bare `kill()`: every bridge installs the SIGTERM teardown
  (node/browser/python/Java), so a launched target dies with a reaped
  bridge instead of orphaning.
- pybridge publishes the park epoch before frame enrichment: a
  stack-unavailable raise no longer hides a suspended target from
  concurrent waiters, and the deadline path rechecks the shared handoff
  before `StopTimeout`.
- Live retry heals only tests that actually passed: a class-level skip
  on retry no longer erases the original failure.
- npm postinstall fails closed on a fetched-but-unparseable checksum
  sidecar (previously warn-and-continue installed it unverified).
- npm postinstall downloads are bounded: the archive rejects past 64MB
  and the checksum sidecar past 16KB (both with timeouts) — a rogue
  server can no longer OOM or hang `npm install` by trickling forever.
  An oversized sidecar fails verification closed; `install.sh` curl
  calls carry `--max-time` / `--max-filesize` the same way.
- Install extraction is tar-slip safe: `install.sh` and npm
  postinstall extract only the `agent-debugger` member — a tampered
  archive's `../` or symlink members can no longer write outside the
  dest dir or plant a live symlink in it.
- Windows Python provisioning uses the `Scripts\python.exe` venv layout
  (was hardcoded `bin/python`) and falls back from `python3` to `python`;
  `NODE_PATH` joins with the platform separator (`;` on Windows).
- Provisioning probes (`has_debugpy`, `node --version`, `javac`, venv
  creation) are bounded — a hung toolchain fails the start instead of
  hanging it.
- Failed lock-record writes remove the just-created file: no unowned
  lock blocks retries for the stale bound.

## [0.2.1] — 2026-09-18

### Fixed

- Release builds the `aarch64-unknown-linux-gnu` asset (`ubuntu-24.04-arm`),
  matching what `install.sh` and the npm postinstall already requested —
  ARM Linux installs no longer 404.
- `doctor` browser readiness now requires Node.js **and** Chrome
  (previously Node.js alone reported ready).
- `install.sh` and npm postinstall verify downloads against the published
  `.sha256` sidecar: mismatch aborts, missing sidecar warns and continues.
- `requestedTarget` no longer fabricates a `node` entry when `--node` was
  omitted.
- Provisioning timeouts kill the child on Windows too (`taskkill /F`;
  previously Unix-`kill` only, leaking the process).
- Session/sidecar writes use unique tmp files (no shared fixed tmp names).

### Added

- Verified 2-minute quickstart transcript and alternatives comparison in
  `README.md`.
- Coverage tests: release-asset matrix, installer checksum (sh + node),
  timeout kill, unique-tmp hygiene.

## [0.2.0] — 2026-09

Early access. First packaged release.

### Added

- Snapshot-first debugging for Python (debugpy), Node.js (CDP), browser
  tabs (CDP), and Java (embedded JDWP/JDI bridge) behind one JSON protocol.
- Persistent sessions with kernel-locked breakpoint intent (`stops.json`),
  compaction-survival trio (`status` / `breaks` / `context`).
- One-line installer, npm package (`@planeinabottle/agent-debugger`),
  per-archive `.sha256` + `SHA256SUMS.txt` on GitHub Releases.
- Ephemeral `capture`, browser `reload`, pure-polling `wait`, logpoints,
  Java watchpoints / method exits, conditional breakpoints.

### Known limitations

- Live integration suites run locally (`scripts/run_gates.sh --live`) and
  on ubuntu CI (the `live` job warms adapter deps, then runs `--live`);
  the fast CI matrix runs `--unit` only.
- Exception stops are uncaught-only; `justMyCode` always on; Node
  `refs()` unsupported. See `docs/feature-roadmap.md`.
- New project: expect edge cases across OS / runtime / package-manager
  combinations. Please report with `agent-debugger doctor` output.
