# Release checklist (tag → HN)

First release published: v0.2.1 (2026-09-18, all 11 assets verified
with `scripts/check_release.sh`).

## Cut

1. Bump together: `Cargo.toml` version, `package.json` version,
   `install.sh` `DEFAULT_VERSION`. Move `CHANGELOG.md` `[Unreleased]`
   to the version with today's date.
2. Green gates on a clean tree: `scripts/run_gates.sh` (unit + live).
3. Tag and push: `git tag vX.Y.Z && git push origin vX.Y.Z`.
   The release workflow builds 5 targets (x86_64 + aarch64 linux,
   x86_64 + aarch64 macOS, Windows) and publishes `.sha256` sidecars,
   `SHA256SUMS.txt`, and the npm package.

## Verify (before announcing anything)

4. `scripts/check_release.sh vX.Y.Z` → `ALL RELEASE ASSETS PRESENT`
   (11 artifacts: 5 archives + 5 sidecars + `SHA256SUMS.txt`).
5. Fresh-machine install smoke, one per OS family:
   - `curl -fsSL .../install.sh | sh` prints `Checksum verified.`
   - `npm install -g @planeinabottle/agent-debugger` prints the same.
   - `agent-debugger doctor` shows all four adapters ready.
6. One scripted debug loop: `scripts/bench_debug_loop.sh` exits 0.

## Announce

7. Show HN with: the problem (agent print-debug loops), the
   `docs/benchmark.md` numbers (labeled measured vs estimate), the
   2-minute quickstart, honest limits (no prod attach, uncaught-only,
   early access). Answer `doctor` outputs in the comments.
