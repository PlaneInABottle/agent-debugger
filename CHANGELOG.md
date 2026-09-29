# Changelog

All notable changes to this project are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.0.0/); versioning follows
[SemVer](https://semver.org/).

## [Unreleased]

### Fixed

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
