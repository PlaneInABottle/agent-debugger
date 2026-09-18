# Changelog

All notable changes to this project are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.0.0/); versioning follows
[SemVer](https://semver.org/).

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
