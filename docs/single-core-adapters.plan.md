# Policy-core migration (M1+M2 only) — Implementation Contract

## Problem and Success Criteria
- Pin the policy duplicated across the four debugger bridges (py/node/browser/java) into
  contract fixtures (M1), then move CLI-knowable input validation into the Rust CLI (M2) —
  without touching pump/wait/transport/planting or any live-suite timeout assertion.
- Success: drift in duplicated policy fails a gate by design (M1); out-of-bound tail input is
  clamped CLI-side with byte-identical results (M2). No behavior change in M1; no new errors in M2.

## Decisions
- **Verdict: FEASIBLE-WITH-BOUNDS.** A runtime single-language core with IPC shims is REJECTED
  (breaks in-process park/step timing invariants 1/3/4/8, sheds zero runtimes). Adopted instead:
  policy owned by the Rust CLI, protocol engines stay in place per language.
- **Core host: the Rust CLI (`src/`, `src/session/`).** Already on every command path, already owns
  lifecycle/identity/envelopes, already distributes shared sources (`ensure_js_shared`).
- **M3/M4 evaluated and REJECTED as over-engineering.** Mirroring policy into generated snippets
  across 3 languages trades copy-paste drift for a mirror-sync process while the four native
  protocol engines remain regardless. No further migration phases are planned. (Prior M3/M4
  sections removed deliberately — see git history for the evaluated-and-rejected text.)
- **Frozen values are layer-local where the four differ** (precedent: `identity_caps.json:2`
  "recorded here, not unified"). Verified on today's tree (PLAN review):
  - Identical all four: logs ring 2000; tail clamp `[0,500]` default 50; MAX_STRING 200; MAX_VARS 20;
    MAX_FRAMES 10; park warning text byte-identical; eval truncation `… (+N more chars)` idiom.
  - Layer-local records: argv array cap 32 (py/node/CLI only — browser drops argv wholesale,
    java has no array cap); timeout message text (`"0 and 3600"` py/node/browser vs `"1 and 3600"`
    java) with integer-domain agreement; dispatch-group structure (browser RESUME adds `reload`;
    java conflates resume/wait/capture into one boolean).
- **M2 scope (restated per PLAN review):** CLI-knowable classes ONLY.
  - `tail`: CLI **clamps** to `[0,500]` (never rejects — bridges return `ok` today; rejection would
    break behavior preservation). Frame-index stays bridge-authoritative: the bound is the live
    paused frame count, unknowable pre-forward (`session.json` carries no frame count), so the
    universal "bridges unreachable for out-of-bound inputs" acceptance is explicitly DROPPED.
  - `timeout`: already owned by clap (`range(1..=3600)` on every timeout arg) — no new check.
  - `breaks add/remove/clear` call `client::request` directly (`breaks.rs:35,84,122`), bypassing
    `forward` — they carry no M2-class input, so no seam change required; the "every command path"
    claim is corrected here.

## Verified Context
- `src/bridge.rs:1209-1243` — fixture guard iterates a hardcoded filename list, reads each
  at test time and asserts a flat JSON object (`:1226`). New fixture files MUST be added to that list.
- `tests/contract/` (6 files) + `tests/test_contract_fixtures.py` + `tests/contract_fixtures.test.js`
  — consumers the new fixtures must parse in.
- `scripts/run_gates.sh --unit` auto-discovers `tests/test_*.py` and `tests/*.test.js` — new parity
  files are genuinely gated.
- `bridge/js/framing.js` + `bridge/js/cdp_conn.js` — precedent that sharing works intra-language only.

## Scope
- Allowed: `docs/single-core-adapters.plan.md` (this file); M1: `tests/contract/*`, parity test glue,
  `src/bridge.rs` fixture list; M2: `src/session/*` tail clamp + unit tests.
- Forbidden: any bridge behavior change; any pump/wait/transport/attach change; any live-suite
  timeout assertion change; any new error text (envelopes byte-stable).

## Milestone 1: Freeze the duplicated contract in fixtures (no behavior change)
- Subphases: (1) extend `tests/contract/` with the frozen policy table (identical values +
  layer-local records above); (2) parity assertions in all three harnesses (Rust guard-style,
  py `test_contract_fixtures.py`, JS `contract_fixtures.test.js`) pinning each bridge's current
  values to the table. Fixture layout follows the existing convention (one flat JSON object per
  file, `_note` + scalar/array keys, matching `identity_caps.json`).
- Files/behavior boundary: `tests/contract/*` + test glue + `src/bridge.rs` fixture list only.
- Validation: `cargo build`, `./scripts/run_gates.sh --unit`, `python3 tests/test_contract_fixtures.py`.
- Acceptance: full unit gates green; fixtures parse in all three consumers; malformed-edit guard
  extended to the new files (evidence invalidation by design: any constant changed without a
  fixture update fails the gate).
- Rollback: delete the new fixture/test files (test-only change, no behavior to revert).

## Milestone 2: CLI owns the tail clamp (bridges keep local checks)
- Subphases: (1) pure `clamp_tail`-style function in `src/session/` (CLI-knowable, no I/O) applied to
  the `logs` tail before any bridge send, returning the identical result the bridge clamp would
  produce; (2) unit tests proving clamp values (0 stays 0, >500 → 500, negatives → 0) — the
  byte-equal envelope holds structurally since the bridge receives an already-clamped value and
  the response shape is unchanged.
- Files/behavior boundary: `src/session/*` clamp + unit tests only; bridges untouched.
- Validation: `cargo build`, `./scripts/run_gates.sh --unit`.
- Acceptance: clamp unit tests green; existing live negatives (`test_03`, `test_06`) green with no
  envelope change; no new error text anywhere.
- Rollback: revert the `src/session` clamp (CLI-only change).

## Risks (M1+M2 only)
- Fixture drift during authoring (a frozen value misread from a bridge): parity tests fail on the
  author's own tree before merge — self-proving.
- M2 clamp divergence from a bridge clamp: unit vectors cover the boundaries (0/1/499/500/501,
  negative, huge); non-numeric tails never reach the CLI (`usize` arg).
