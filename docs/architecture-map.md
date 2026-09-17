# Architecture Map (durable, documentation only)

Status: APPROVED guidance for future contributors. The maintainability
program (`docs/maintainability-refactor.plan.md`) is COMPLETE pending final
global review/gate: Rust session is `src/session/` (9 one-way modules),
Python/Node/Browser carry explicit in-file owners, Java thins only via
`st`-parameterized helpers under caller-held `sessionLock`. No production
refactor is authorized by this document beyond that program. User priority
remains world-class quality/stability first, features second.

Canonical live contract: `docs/current-contract.md` (schema v2).
Change record: `docs/schema-v2-cleanup.md`.
History (do not implement from): `docs/debugger-feature-roadmap.md` (ARCHIVED).
Package version: `0.2.0` (`Cargo.toml`); every sidecar carries `schemaVersion: 2`.

Rule for this file: references are durable **symbol/file** names, never exact
line numbers (line numbers drift; symbols are grep-stable).

## 1. Dependency / provisioning diagram

```text
CLI (src/cli.rs: Stops, *Cmd) ── dispatch ──► src/main.rs ──┬──► src/spawn.rs
     │                                                        │    (Target enum, cmd_spawn,
     │                                                        │     seed_target_identity)
     │                                                        └──► src/session/
     │                                                             (mod.rs facade; locks,
     │                                                              persistence, forward*,
     │                                                              status/close, seeds,
     │                                                              probes)
     │                                                                   │
     │                                          framed Content-Length JSON│ (src/client.rs:
     │                                          MAX_FRAME_BYTES 64 MiB;  │  one request per
     │                                          src/dap.rs: header ≤8192 │  connection = reply
     │                                          bytes)                   │  correlation, no
     │                                                                   │  multiplex)
     │                                                                   ▼
     │                                              ┌──── language bridge (one daemon
     │                                              │     process per session dir) ────┐
     │                                              │                                  │
     ▼                                              ▼                                  ▼
src/output.rs (BridgeFailure   bridge/py/src/pybridge.py        bridge/node/src/nodebridge.js
 envelope: target_identity +    SINGLE FILE                      + bridge/browser/src/
 requested_target passthrough)  (stdlib DAP client;              browserbridge.js
src/doctor.rs (env diagnosis)   debugpyAttach child              ──► SHARED: bridge/js/framing.js
src/fmt.rs (output formatting)  sessions; _gate -> mu            + bridge/js/cdp_conn.js
                                lock order)                      (CDP framing/conn; NodeWorker
                                                                   wrapper, never flat sessionId)
                                                                bridge/java/src/*.java
                                                                MULTI-FILE COMPILE
                                                                (BridgeCli/Conn/Eval/Model/
                                                                 Proto/Session/Snapshot/
                                                                 JdiBridge)
```

Provisioning (first run provisions the adapter once; `skills/agent-debugger/SKILL.md`
"First ever run" note): Python = isolated auto-provisioned venv running
`pybridge.py`; Node/Browser = CLI provisioning first (`ensure_node`: `node
--version` probe; `ensure_ws`: one `npm install ws` into the isolated adapter
dir, shared with the browser bridge via `NODE_PATH`) — distinct from the
bridge-side target syntax validation that runs later inside `nodebridge.js`
(`node --check` for plain JS, `node --version` for TS where `--check`
false-positives on type-stripping); then `nodebridge.js` / `browserbridge.js`
over CDP (`/json/list` for tabs);
Java = `javac`-compiled bridge classes (~0.5s tiny). `cargo install --path .`
updates CLI + adapters together; a still-running old bridge stays old code
until closed — `close` uses only `port` + the generic framed `{"cmd":"close"}`,
so closing old sessions after upgrade is safe (see `docs/current-contract.md`
"Install / upgrade").

Embedded bridge sources: the Rust binary embeds the bridge files named above
via `include_str!` (`src/bridge.rs`: `PYBRIDGE_SOURCE`, `NODEBRIDGE_SOURCE`,
`BROWSERBRIDGE_SOURCE`, `JAVA_SOURCES`, shared `cdp_conn.js`/`framing.js`)
and materializes them on first use via `ensure_*` (`ensure_pybridge`,
`ensure_nodebridge`/`ensure_browserbridge`, `ensure_compiled`,
`ensure_js_shared`, `ensure_ws`) into `~/.agent-debugger/adapters/`;
`setup_bridge` in `src/session/spawn_lifecycle.rs` spawns that materialized copy. There is no
separate bridge distribution channel.

## 2. Ownership table + non-goals

| Area | Owner file(s) | Owns (writes/decides) | Must NOT touch |
|------|---------------|----------------------|----------------|
| CLI surface | `src/cli.rs`, `src/main.rs` dispatch | flags, `Stops`, subcommand routing to `spawn`/`session` | sidecar bytes, bridge protocol |
| Spawn/targets | `src/spawn.rs` (`Target`, `cmd_spawn`, `seed_target_identity`, `launch_seed`/`attach_seed`) | bridge argv, `--target-identity` seed (before `--`), `stops.json` intent summary | bridge-owned `session.json`/`error.json` |
| Session state | `src/session/` (`mod.rs` facade over `close_status.rs`, `forward.rs`, `breaks.rs`, `spawn_lifecycle.rs`, `attach.rs`, `locks.rs`, `paths.rs`, `sidecar.rs`, `identity.rs`) | locks, atomic sidecar writes, `forward`/`forward_target`, `cmd_breaks_add`/`remove`/`clear`, `cmd_targets`/`cmd_targets_in`, `cmd_context_target`, `cmd_reload`, `spawn_in`, `close`, `status`/`session_entry`, probes, seeds, redaction/caps | DAP/CDP/JDWP wire details |
| Transport | `src/client.rs`, `src/dap.rs` | `Content-Length` framing, 64 MiB / 8192 B bounds | session semantics |
| Failure envelope | `src/output.rs` (`BridgeFailure`) | shape `{target_identity, requested_target}` passthrough | identity content (session's job) |
| Python bridge | `bridge/py/src/pybridge.py` (single file) | DAP sessions, `dispatch`/`serve`, `publish_state`, child roster (`debugpyAttach`), breakpoint merge | CLI sidecars (`lang.json`/`stops.json`) |
| Node bridge | `bridge/node/src/nodebridge.js` + `bridge/js/cdp_conn.js`, `bridge/js/framing.js` | CDP session, `dispatch`/`serve`, `publishState`, worker table (`NodeWorker` wrapper) | same as above |
| Browser bridge | `bridge/browser/src/browserbridge.js` + `bridge/js/*` (shared) | tab session, `verifyTab`, `dispatch`/`serve`, `publishState`, reload interplay | tabs' lives (detach-only close), process claims |
| Java bridge | `bridge/java/src/*.java` | `BridgeSession` owns ALL mutable session state; `dispatch` + pooled `serve`; `publishState`; two-phase `breaksAddJson` | splitting `BridgeSession` state across files |
| Shared JS | `bridge/js/cdp_conn.js`, `bridge/js/framing.js` | CDP conn + framing used by Node AND Browser | language-specific plant/hit logic |
| Agent docs | `skills/agent-debugger/SKILL.md` | user-facing behavior notes | contradicting `docs/current-contract.md` |

Explicit non-goals (never "drive-by" additions): multiplex/request-id
(connection already correlates); auto-migration of v1 dirs; `meta.json` /
`intent.json` consolidation; a `cancel` command (cancel = `close`,
documented); stale-snapshot mode (rejected: fail-fast instead); process
inventory/selection UI; silent target-process flag injection; attach-side
child/worker discovery; splitting any file solely for size (see §6).

## 3. Sidecar ownership + atomicity (schema v2)

Dir: `~/.agent-debugger/sessions/<name>/`. All four files are JSON objects
written atomically (tmp+rename in the same dir — `write_sidecar` /
`writeFile` / `write_file` / `BridgeProto.writeFile`).

- CLI-owned: `lang.json` (`{lang, schemaVersion: 2}`), `stops.json` (spawn
  intent: `breaks/logpoints/watches/exits/sources/timeout/target/` +
  `requestedTarget` + `schemaVersion: 2`). `target` = spawn-derived display
  summary; `requestedTarget` = endpoint+flags, never an observation.
- Bridge-owned: `session.json` (`name/kind/port/stopped/lastStop/updatedAt`
  + `schemaVersion: 2` + `targetIdentity {debuggee, endpoint, adapter}`),
  `error.json` (`{schemaVersion: 2, error, phase: transport|config|runtime}`).
- Gate: `require_schema_v2` runs after `check_name`/`check_dir_real`, before
  lang/port routing, on every mutating/reading command. Exempt: `doctor`,
  `status`, `close`, spawn-create. Old dirs are `status`-visible then
  `close`-only.
- Confirmed-only persistence: `cmd_breaks_add` → `append_confirmed_breaks`,
  `cmd_breaks_remove`/`clear` → `remove_confirmed_breaks`; only
  bridge-confirmed raw specs (`confirmed_removed`) move `stops.json`.
  Transport failure persists nothing + prints the `bare breaks` reconciliation
  hint. `stamp_main` tags responses served by the main-only path.

## 4. Stateful boundary inventory (symbols, not line numbers)

- **Startup lock** (`src/session/locks.rs`): `startup_lock_path`,
  `acquire_startup_lock` / `release_startup_lock` / `startup_lock_is_mine`,
  `claim_stale_startup_lock`, `stale_startup_mtime`, `startup_close_gate`.
  Rule: created atomically before stale-dir clear, held through the wait loop;
  only a provably stale lock (mtime-bound) is stolen via verified
  detach-and-reconcile — never a live starter's.
- **Endpoint locks** (`src/session/locks.rs`): `endpoint_locks_dir` /
  `endpoint_locks_dir_for`, `endpoint_lock_path`, `acquire_endpoint_lock` /
  `release_endpoint_lock`, `endpoint_lock_session_name`,
  `reclaim_stale_lock` / `detach_to_quarantine` / `lock_snapshot_is_stale`,
  `EndpointClaim::{Held,Busy}`. Held under the guard: v2-vs-v2 collision scan
  + legacy scans (endpoint-matched `endpoint-already-attached`, then global
  `unsupported-legacy-live`). Browser never collides (`attach_exclusive`
  covers only `py|node|java`).
- **Breaks lock** (`src/session/breaks.rs`): `breaks_lock_path`,
  `acquire_breaks_lock` (`BreaksGuard` owns the open handle with a kernel
  `File::try_lock` exclusive lock — released on handle close, so a crashed
  holder never wedges the section and there is no stale protocol; the file
  persists and is never deleted). Serializes concurrent `stops.json`
  append/remove (see `breaks_lock_serializes_concurrent_appends` test).
- **Lock order**: Python `DapConn` documents `_gate -> mu`, never the reverse;
  Java holds all session mutation under the single `sessionLock` with ONE
  event-queue consumer (the session thread); evaluation Phase A runs OUTSIDE
  the lock, Phase B commits under it. Any future split must preserve exactly
  one ordering per bridge.
- **Target resolution / local state** (`src/session/forward.rs` + `src/session/identity.rs`): `forward_target`
  (most-recent-stopped live target, else main) + `stamp_main`;
  `cmd_targets` / `cmd_targets_in` / `targets_use_local_roster` (java/browser
  stay CLI-side rosters — no bridge `targets` protocol there);
  `owner_target_identity` (legacy collapses to all-unavailable, never
  promotes a pid); `intent_endpoint` is `requestedTarget`-only.
- **Stop freshness / wait / capture**: bridges publish `stopped`/`lastStop`/
  `updatedAt`; frame-bound reads (`context`/`vars`/`stack`/`eval`/`step`)
  fail fast with `no stopped thread` when unparked — stale frames are never
  served as current. Live reads (`threads`/`breaks`/`logs`) answer unparked.
  Frozen timeout text prefix `timeout: no stop within Ns` (additive fields
  only). Python `wait_context`; `capture already pending` guard; Node
  `NodeWorker` hit verdict is by `hitBreakpoints`, never `reason`.
- **Breakpoint transaction / bookkeeping**: Java `breaksAddJson` validates the
  whole batch first (zero backend mutation), then arms per item (DAP
  file-granular replace semantics); Node/Browser `Debugger.removeBreakpoint`
  + `breakIdToRec` drop; Python `setBreakpoints` merge-without-removed;
  shadow rule (Java `isShadowed`/`shadowDetail`: a removed break does NOT
  auto-resurrect a shadowed logpoint — one explicit re-arm attempt, else
  deferred `pending`); `removed[]` echoes persisted stored raws; batch totals
  (`ok:false` on total backend failure, zero mutations).
- **Serve pool / terminal close**: Python threaded serve (`MAX_THREADS = 8`,
  single `_gate`); Node/Browser concurrent serve (`MAX_ACTIVE_HANDLERS = 8`,
  `MAX_QUEUED_CONNS = 16`); Java fixed pool (8 daemon threads, one
  `sessionLock`, one event-queue consumer). Terminal `close`: framed
  `{"cmd":"close"}` (all bridges: py `dispatch`, node/browser `dispatch`,
  Java `case "close": throw new CloseSession()`), 65 s queued-close tolerance
  + 15 s port-death check (`close` in `src/session/close_status.rs`), then
  `remove_dir_all`. `close` has no version gate, ever.
- **Spawn / close / status / probe** (`src/session/close_status.rs` + `src/session/spawn_lifecycle.rs` + `src/spawn.rs`):
  `cmd_spawn` → `spawn` → `spawn_in` → `setup_bridge` → wait loop
  (`session.json` + first `context`/`threads` validation; failed setup
  wholesale-clears the dir, no leak); same-name v2 always bails
  (`already exists (close it first)`); legacy reclaim ONLY when proven dead
  (`daemon_alive_in == false` + parseable nonzero port). `close` (framed close
  + port-death check + remove). `status`/`session_entry` (never errors a row;
  current vs old/unsupported rows per frozen contract). `probe_session_bridge`
  (protocol-aware envelope check, never bare TCP) vs `daemon_alive_in` (1 s
  TCP liveness for reclaim/collision scans).

## 5. Invariants (must hold after ANY future change)

1. **Close is type-dependent**: launch reaps its own tree (Python parent
   `disconnect {terminateDebuggee:true}`; Node process kill), attach detaches
   only — including Browser (`Debugger.disable` + close, tab lives on).
   Foreign/unowned targets are never killed.
2. **No signal by port**: a port number never identifies a process to kill
   (wrapped servers: the listener-owner pid may be the adapter, not the
   debuggee). Port-owner kills are forbidden; `close` talks to the daemon
   over the framed protocol.
3. **Omitted vs explicit launch selectors**: required selectors (Java
   `--main`, Node `program`) vs optional/exclusive ones (Python
   `program`/`--module` exactly-one-of via `conflicts_with`; Browser `--tab`;
   `--classpath`/`--python`/`--node` binaries). Omitted fields stay
   absent/null — never fabricated defaults; malformed/missing
   `--target-identity` degrades bridges to all-unavailable identity, never a
   spawn failure.
4. **Per-target state, no sticky selection**: `targets` responses carry
   `selected` + per-target `state`; there is NO persistent `selectedTarget`
   in `stops.json`/`status`. Target-less commands route to the most-recent
   stopped live target, else main, and every response echoes the served
   `"target"` (no silent redirect). Bounds: 8 active non-main + 16 exited
   history (`droppedExited`); in-flight ≤1 + tracked ≤8 + retired ≤16 sockets.
5. **Error attribution** (`error.json` phase, `src/output.rs` envelope):
   `transport` = connection/protocol loss (endpoint diagnosis applies);
   `config` = semantic spec error (message top-level verbatim, no endpoint
   diagnosis); `runtime` = unexpected internal failure after a successful
   bridge op (verbatim, never endpoint-diagnosed, never called
   endpoint-rejected). Parseable v2 file with bad version/phase is corrupt:
   `corrupt setup error file in '<name>' (schemaVersion/phase); close and
   retry`. Absent/unparseable/error-less keeps the transport fallback.
6. **Redaction / caps / no target env**: `redact_argv` (+ `reredact_argv_in` on
  collision reads) redacts token/password/authorization/api-key/secret/
  passwd/pwd (`=`/`:`/space, case-insensitive) to `[redacted]`;
  `cap_identity`: every string field ≤512 chars (`IDENTITY_FIELD_CAP`),
  whole identity ≤2 KB (`IDENTITY_TOTAL_CAP`), truncation marked
  `… (+N more chars)` — the same idiom as every bridge's `trunc_str`
  helper; causes ≤2048 (`CAUSE_CAP` / `MAX_ERROR_CHARS`). **No target-process
  environment collection**: the CLI does read its own operational context —
  `HOME` (sessions/adapters dirs), `NODE_PATH` (read + prepend so the browser
  bridge shares the provisioned `ws`), `current_dir` (launch-seed cwd), temp
  dirs — but it never collects or persists the target process's environment
  (`os.environ`-style harvesting); raw command lines are never persisted.
7. **Framing bounds**: `Content-Length` JSON; header ≤8192 bytes; body
   ≤64 MiB (`MAX_FRAME_BYTES`); one request per connection (no multiplex,
   no request-id — v1-out).

## 6. Provisioning constraints + completed ownership program

Measured sizes (informational, not defects): `src/session/` ~6.0k lines
across `mod.rs` + 9 modules, `bridge/py/src/pybridge.py` ~5.9k,
`bridge/node/src/nodebridge.js` ~5.0k,
`bridge/browser/src/browserbridge.js` ~3.2k, `bridge/java/src/` across
8 files, `bridge/js/*` ~0.2k.

Why Python/Node/Browser stay single-file:

- Each embeds ONE lock ordering + ONE serve lifecycle + ONE persistence
  path. Splitting `pybridge.py` would scatter `_gate -> mu`, the in-flight
  handshake exclusion, and the child/retired socket ownership across module
  seams with no seam-safe boundary today. Same for `nodebridge.js` (CDP
  session + `NodeWorker` wrapper + swap mutex + slide/resolve state) and
  `browserbridge.js` (`verifyTab` liveness woven through every command +
  reload interplay).
- `src/session/` co-locates the three lock families, the atomic sidecar
  writes, and the close/status/reclaim paths that must agree on the exact
  same stale/proven-dead definitions. Moving one without the others reopens
  the TOCTOU windows the quarantine + nonce + mtime-bound machinery closed.

Java precedent (the ONLY approved split pattern): Java is multi-file because
its seams are pure/stateless — `BridgeProto` (framing/`writeFile`),
`BridgeModel` (data), `BridgeSnapshot` (frame capture), `BridgeEval`
(condition/logpoint eval), `BridgeConn` (transport), `BridgeCli`
(arg parse) — while ALL mutable session state + the single event-queue
consumer stay in ONE file (`BridgeSession.java`) under ONE lock
(`sessionLock`). A future split elsewhere must copy this shape: extract pure
helpers; keep exactly one state owner + one lock order per bridge. "Reduce
line count" is never alone a justification.

M3 (implemented, narrow track, Python bridge only): `bridge/py/src/pybridge.py`
stays one embedded file (`src/bridge.rs` `PYBRIDGE_SOURCE` untouched) with
exactly two retained in-file owners constructed by `Session` (still holding
`_gate`, order `_gate -> mu`, no new locks): `TargetRegistry` (child
roster/creation order/seen-ids/bounded exited history/ignored+helpers
counters/`serving`/attach staging + in-flight flag/retired sockets +
`resolve_inner`/`swap_fields`/`note_exit`/`commit_child`) and `ServerState`
(handler pool via `try_admit`/`release` + terminal-close single winner via
`claim_close`; `assert_valid` enforces `0 <= active <= MAX_ACTIVE_HANDLERS`).
Breakpoint bookkeeping and the stop/wait/capture machine stay Session-owned
sections, NOT owner objects: their state IS the per-target swapped context
(global `stop_states`/`_hitkeys` on Session, copies on each `ChildTarget`)
and the thread-local pump attribution, so a `BreakpointStore`/
`StopCoordinator` wrapper only added bypass (tried, rejected on FULL review —
no compat-property aliases; roster containers are same-object views,
scalars/counters/serving route through the owners directly). Future
extraction trigger (unchanged bar): a measured defect attributable to the
boundary + FULL plan/review, never size alone. Zero protocol/sidecar/schema
delta; Node/Browser/Java untouched (M4/M5).

M4 (implemented, narrow track, Node bridge only): `bridge/node/src/nodebridge.js`
stays one embedded file (`src/bridge.rs` `NODEBRIDGE_SOURCE`/`ensure_js_shared`
untouched, `bridge/js/*` untouched) with exactly three retained in-file owners
constructed by `Session` (still the sole runtime owner + CDP orchestrator):
`WorkerRegistry` (worker table/creation order/seen-ids/bounded exited history/
ignored+dropped counters/pending worker replies + `claimId`/`track`/`release`/
`noteExit`/`evictOldIgnored`/`takePending`/`liveWorkers`/`activeWorkers`;
`assertValid` enforces table ⊆ order/seen + bounded history), `SerialChain`
(one serialized promise tail; three instances replace the raw `_swapTail`,
`_mutationTail`, `_pauseChain` fields + `_lockSwap`/`_chainPause` bodies —
`_swapRun`/`_mutationRun`/`_chainPause` stay as the named domain entries and
stable test seams, rejection-safe by construction), and `ServerState`
(handler pool via `tryAcquire`/`release` + terminal-close single winner via
`claimClose`; `assertValid` enforces `0 <= active <= MAX_ACTIVE_HANDLERS`).
Rejected with the same M3 rationale (state IS the swapped context, a wrapper
only adds bypass — no aliases, no dual writes, enforced by
`scripts/check_nodebridge_owners.sh` in the `--unit` gate): `BreakpointStore`
/`StopCoordinator` (breakpoint maps/records + freshness machine stay
Session-owned sections), resolve relocation (resolveTarget fuses main liveness
+ selection clocks with roster reads), and swap save/load relocation
(saveMain/loadWorker/storeWorker/loadMain fuse Session main fields). The
worker-removed-during-withTarget case needed no fix — the `isCurrent` guard +
unconditional `loadMain` restore already held (regression-locked in
`tests/nodebridge_owners.test.js`, the Python `_TargetScope` missing-child
analog). Pre-existing roster shape: `WorkerRegistry.order` retains exited ids
(`noteExit` removes from the table, not from order; readers guard with
`has()`/`liveWorkers()`); future bounded-roster trigger is a measured defect
attributed to order growth/drift — no change now. Test fixtures admit through
the production API (`claimId`+`track`/`release`, `tryAcquire`), never raw
collection writes. Zero protocol/sidecar/schema delta; Python/Rust/Browser/Java
untouched (M3 gate reused, M5 next).

M5 (implemented, narrow track, Browser + Java): `bridge/browser/src/browserbridge.js`
stays one embedded file (`src/bridge.rs` `BROWSERBRIDGE_SOURCE`/`ensure_js_shared`
untouched, `bridge/js/*` untouched) with exactly two retained in-file owners
constructed by `Session` (still the sole runtime owner of the single tab +
CDP orchestrator): `SerialChain` (one serialized promise tail replacing the
raw `_mutationTail` field + `_mutationRun` body — `_mutationRun` stays as the
named domain entry and stable test seam, rejection-safe by construction;
there is deliberately NO swap chain and NO pause chain: one tab per session,
no worker table, pauses park-or-drop synchronously in `onPaused`) and
`ServerState` (handler pool via `tryAcquire`/`release` + terminal-close
single winner via `claimClose`/`markClosing`; `assertValid` enforces
`0 <= active <= MAX_ACTIVE_HANDLERS`). Rejected with the same M3/M4 rationale
(state IS the single-tab context, a wrapper only adds bypass — no aliases, no
dual writes, enforced by `scripts/check_browserbridge_owners.sh` in the
`--unit` gate): `BreakpointStore`/`StopCoordinator`/`WorkerRegistry`
(breakpoint maps/records + stop/wait/capture machine + `outstanding` resume
slot stay Session-owned sections), and any Browser/Node shared abstraction
beyond framing/CDP-conn. `verifyTab`-per-command liveness, reload interplay,
and CDP orchestration stay Session methods (they fuse tab identity with
transport reads). Test fixtures admit through the production API
(`tryAcquire`, `_mutationRun`, `claimClose`), never raw field writes.
`bridge/java/src/` keeps ALL mutable session state in `BridgeSession` under
the single `sessionLock` + one event-queue consumer; M5.2 thins it only with
`st`-parameterized helpers moved to existing support classes under the
per-method callgraph gate (all production entries inside `BridgeSession.java`
under `sessionLock`, or pure/read-only with identical call sites/threads; no
`synchronized`/lock/thread/socket in moved bodies, enforced by
`scripts/check_java_owners.sh`): `BridgeSnapshot` owns the tracking group
(`trackChanges`/`storeTrack`/`compareTrack`/`degradeTrack`/`changeFieldsJson`/
`trackWarn`/`jsonTotal`/`jsonStrings`) + the read-only text group
(`timeoutText`/`withCaptureStage`/`captureExitContextJson`/`waitContextJson`),
`BridgeProto` owns the pure builders (`truncField`+`IDENT_FIELD_CAP`/
`jsonLong`/`jsonString`/`jsonStringArray`/`unavailableEntry`/`busyError`/
`overloadedJson`), `BridgeEval` owns `frameIdentity`/`frameIdentityOf`.
Retained in `BridgeSession` regardless: `sessionLock` discipline, `serveLoop`,
`dispatch`/`dispatchInner`, all `awaitStop*`, `parkedRecheck`/`notePark`,
`plantPending`/`armWatch`/`armBreakpoints`/`setMethods`, `handleOne`,
`closeFromConn`/`cleanup`, `buildTargetIdentity*`/`seedHint`. Rejected as one
atomic unit: the setup-phase text group (`setupErrorText`/`setupPhaseOf`/
`phaseOfError`/`setupErrorJson`) — `setupErrorJson` has a production caller
outside `BridgeSession` (`BridgeCli.java`), so the group stays. No new
top-level class/file, `src/bridge.rs` `JAVA_SOURCES`/`JAVA_CLASSES`
untouched. Zero protocol/sidecar/schema delta; Python/Rust/Node untouched.

## 7. Test map (boundary → exact files)

| Boundary | Rust unit (`cargo test`, incl. `src/session/` tests) | Bridge unit | Live / matrix |
|----------|--------------------------------------------------------|-------------|---------------|
| Framing/bounds | `src/dap.rs` tests (8192/header/64 MiB) | `tests/framing.test.js` | — |
| Setup/error phases | `error.json` phase tests (`transport`/`config`/`runtime`, corrupt message) | `tests/setup_phase.test.js`, `tests/test_pybridge.py` | `tests/test_error_attribution.py` |
| Target identity (no `observedTarget`) | `owner_*`, `session_entry` asserts (`observedTarget` absent, flat never promoted) | `tests/target_identity.test.js` | `tests/test_live.py` identity cases |
| Breaks add/remove/clear | `confirmed_*` persistence tests, `breaks_lock_*` | `tests/breaks_add.test.js`, `tests/breaks_remove.test.js`, `tests/m3_fixes.test.js`, `tests/m4_fixes.test.js`, `tests/strict_break_lines.test.js` (strict line/int parity) | `tests/test_live.py` live add/remove |
| Concurrency/serve | lock/quarantine unit tests (`reclaim_*`, `detach_*`, endpoint claim) | `tests/breaks_concurrency_matrix.test.js`, `tests/m5_concurrency.test.js`, `tests/test_pybridge.py` (+12 M5), `tests/test_pybridge_owners.py` (M3 narrow-owner tests: registry lifecycle/swap incl. missing-child exit, server pool/close) + `scripts/check_pybridge_owners.sh` incl. `--self-test` (M3 routing: no view/serving/server writes outside owners) | `tests/test_breaks_concurrency_matrix.py`, `tests/test_m5_live.py` (6 scenarios) |
| Close under load | `close` confirm/port-death tests + `close_deletion_seams_refuse_symlink_swap` (`src/session/close_status.rs` tests: both close deletions refuse a symlink-swapped dir, outside target intact) | `tests/close_under_load.test.js` | `tests/test_close_under_load.py` |
| Wait/capture/timeout | stop-freshness unit tests | `tests/wait_capture.test.js`, `tests/stoptimeout.test.js`, `tests/browser_reload.test.js` (reload park restore + running publish) | `tests/test_wait_capture.py`, `tests/test_ux_live.py` (timeout prefix asserts), `tests/test_main_exit_visibility.py` |
| Workers/targets | `cmd_targets_in` roster tests | `tests/worker_targets.test.js`, `tests/worker_break_records.test.js`, `tests/vars_frame.test.js`, `tests/nodebridge_owners.test.js` (M4 narrow-owner tests: registry lifecycle/swap-restore incl. worker-removed-mid-command + queued-before-swap exit, chains, server pool/close) + `scripts/check_nodebridge_owners.sh` incl. `--self-test` (M4 routing: no container/clear/length-reset/wholesale writes outside owners), `tests/browserbridge_owners.test.js` (M5.1: single-tab shape, mutation chain, pool/close) + `scripts/check_browserbridge_owners.sh` incl. `--self-test` (M5.1 routing: no server/chain replacement or tail/depth writes outside owners) | `tests/test_m5_live.py` |
| Java bridge | `endpoint_lock_path_escapes_without_collision` (host escaping) | `javac` compile + execution: `bridge/java/src/*.java` + `tests/M4JavaCheck.java`, `M5JavaCheck.java`, `M6JavaCheck.java`, `M7JavaCheck.java`, `BJavaCheck.java`, `CJavaCheck.java`, `tests/StrictJavaCheck.java` (port/\u strictness, owner-claim verify, hitCounts prune) (each executed in-gate; fail-fast `System.exit(1)`) + `scripts/check_java_owners.sh` incl. `--self-test` (M5.2 move/retain/reject gate; retained/setup detection is comment-stripped declaration match) | `tests/test_live.py` java adapter |
| Review regressions | `cargo test` full | `tests/review_fixes.test.js` | `tests/test_live.py` (4 adapters, isolated `HOME`, installed binary `target/debug/agent-debugger`) |
| Contract fixtures (frozen strings) | `bridge::tests::contract_fixtures_match_cli_constants` | `tests/contract_fixtures.test.js`, `tests/test_contract_fixtures.py` | — (fixtures only, no live) |
| Provisioning (daemon-free) | `bridge::tests` (stale rewrite, shared-JS no-short-circuit, `NODE_PATH`, venv paths, `JAVA_CLASSES` markers) | — | `tests/_live_home.py` setup (copies real venv/node_modules with symlinks preserved, never installs) + `tests/test_live_home.py` |
| Installer checksum (.sha256 verify-or-skip) | — | `tests/install_checksum.test.js` + `tests/test_install_checksum.sh` (sh gate section in `run_gates.sh`) + `tests/wrapper_find.test.js` (PATH shim self-avoidance) | — (verified at release time per `docs/release-checklist.md`) |
| Release asset matrix | — | `tests/test_release_assets.py` (py gate section; matrix parsed from `release.yml`) | `scripts/check_release.sh <tag>` (manual, release time) |
| Provision timeout kill + tmp hygiene | `run_with_timeout_kills_slow_child`, `atomic_tmp_names_are_unique_per_call`, `atomic_write_uses_unique_tmp_and_leaves_none` | — | — |

Latest matrices live in: `tests/breaks_concurrency_matrix.test.js` (+
`tests/test_breaks_concurrency_matrix.py`), `tests/m5_concurrency.test.js` (+
`tests/test_m5_live.py`), `tests/close_under_load.test.js` (+
`tests/test_close_under_load.py`). Full gate before any release is the
canonical runner `scripts/run_gates.sh` (the command list below is
informative — the script is normative; do not copy this prose into new
runners): unit = `cargo test` + `cargo fmt --check` + every
`tests/test_*.py` except the three live suites (auto-discovered, sorted) +
`node --test tests/*.test.js` + `tests/test_install_checksum.sh` + owner-routing gates
(`check_nodebridge_owners` + `check_browserbridge_owners` +
`check_pybridge_owners` + `check_java_owners`, each incl. `--self-test`) + `javac` bridge/checks + executed
 java checks (B/C/M4-M7 + saturation + framing + strict); live = `cargo build`
+ `tests/run_live.py` (runs `test_live` + `test_m5_live` + `test_ux_live`
in one scope), with `TEST_LANG`/`SKIP_BROWSER` filters for live only.
Live nonzero policy (enforced programmatically in `tests/run_live.py` via
`tests/_live_home.py` `check_live_nonzero` on unittest objects, never log
parsing): every required language must execute ≥1 test — default full
requires py+node+java given the doctor prerequisites plus browser unless
`SKIP_BROWSER=1`; an explicit `TEST_LANG` requires each named language;
the scope must execute ≥1 test overall. An all-skipped scope (e.g. missing
adapter deps) fails instead of passing silently; isolated single-test
skips still pass while another test of that language executes.

Shared test architecture (M2): `tests/contract/*.json` is the single source
for frozen cross-language strings (consumed by
`tests/test_contract_fixtures.py`, `tests/contract_fixtures.test.js`, and
`src/bridge.rs` tests — no golden snapshots, no new dependency);
provisioning is covered daemon-free by `src/bridge.rs` tests (stale rewrite,
shared-JS no-short-circuit, `NODE_PATH` prepend, venv paths,
`JAVA_CLASSES` marker completeness); concurrency matrices keep their local
`threading.Barrier` / promise-barrier idioms (centralizing them would exceed
the duplicated lines — no shared helper); `tests/_live_home.py` owns the
isolated-`HOME` setup/cleanup + failure bundle for exactly the three
class-level live files (`test_live`, `test_m5_live`, `test_ux_live`), with
unit tests in `tests/test_live_home.py`; live failures print artifact paths
(`bridge.log`/`session.json`/`error.json`/`stops.json`) plus a redacted
`bridge.log` tail, never target env.

## 8. Refactor policy (triggers, not permission)

The M1/M2 evidence triggers below were CONSUMED by the completed
maintainability program (Rust 9-module DAG per M1; in-file owners per
M3–M5; shared fixtures/runner per M2). They stay as the bar for any
FUTURE stateful move — size alone is never a justification.

Pure vs stateful, strictly:

- **Pure extraction** (no lock, no sidecar, no socket, no protocol order):
  free to propose; still needs tests proving byte-identical behavior on
  current fixtures.
- **Stateful move** (locks, persistence, serve/dispatch, spawn/close/status,
  identity, error phases): FORBIDDEN without all of the following.

Evidence triggers (M1/M2 — gates, not schedules):

- **M1 (pure-extraction evidence)**: a Java-precedent-shaped proposal naming
  the exact pure seam + the single state owner that keeps its lock; unit
  proof of identical behavior; zero live-matrix diff.
- **M2 (stateful-boundary evidence)**: measured defect or limit (not size)
  attributable to the current boundary + a FULL plan naming every affected
  caller, ordering, and rollback + FULL analyzer review PASS + full gate
  green (§7) on the same diff/env.

Rules for any approved refactor: ONE subsystem/language at a time (serial —
shared files `src/session/`, `src/cli.rs`, `src/main.rs`, `SKILL.md`
forbid parallel edits); mandatory FULL plan + FULL review; NO
behavior/schema change in the same commit (refactor commits carry zero
contract delta); rollback = revert the single commit (v2 test dirs are
disposable; no migration to undo). If a brief requires a new subsystem,
public contract, migration, security boundary, or irreversible-data change,
it is Tier-misclassified: return BLOCKED, do not implement.

## 9. Deferred feature roadmap (stability prerequisite; NO commitment)

Current prioritized list: `docs/feature-roadmap.md` (P1 caught-exception
configuration, P1 Node bounded refs/deep inspect, P2 justMyCode control,
P2 richer watches parity, P3 advanced features). The summary below is
informative — the roadmap file is normative for scope.

The current subset is intentional scope discipline, not backlog neglect:
every item below crosses a stateful boundary (§4) or a security boundary
(§5.6), so each ships only behind its own FULL plan/review + live-matrix
proof, after stability work. Nothing here is promised or scheduled.

- **Caught exceptions**: today all adapters stop on UNCAUGHT only (Python /
  Node take bare `exc`; Java takes `exc:Class` with class filter). Caught-point
  support needs per-adapter event-pipeline changes — deferred.
- **Node `refs()`**: Java walks the heap (50 refs/node, depth ≤ 4), Python
  reports direct holders (depth 1), Node is unsupported (no GC walk via
  CDP) and honestly reports so — deferred, needs a CDP-side design.
- **`justMyCode` control**: always ON today (Python stdlib/site-packages
  skipped; Node `node:` internals filtered in the same spirit). A user flag
  changes every adapter's frame pipeline + redaction surface — deferred.
- **Richer watches**: write/read watches + method `--exit` are Java-only;
  Python/Node fail fast with guidance. Cross-adapter watch semantics need
  DAP/CDP mechanism work per bridge — deferred.
- Also deferred (same bar): qualified `ClassName.field` eval, bare-static
  resolution beyond the frame's declaring type, pytest-under-child tracing
  (UNVERIFIED probe), attach-side child/worker discovery, `Target.*` flat
  session transport (measured NO-GO on the pinned stack), user cancel
  distinct from `close`.

## 10. Stale-claim corrections (binding)

- **No `observedTarget` live key.** The canonical identity is the layered
  `targetIdentity {debuggee, endpoint, adapter}` with confidence vocabulary
  `os-corroborated | protocol-confirmed | unavailable` (+ `source` for
  provenance). v2 `session.json` / `status` rows / `targets` rosters / spawn
  responses carry NO `observedTarget` key — absent, not null (asserted by
  `session_entry` / roster unit tests). Live Rust code never reads the flat
  key — `owner_target_identity` reads only `targetIdentity` and collapses
  anything else to all-unavailable. Remaining `observedTarget` strings
  exist ONLY in: (a) `src/session/` unit-test fixtures / absent-key asserts
  / frozen-contract doc comments, which prove flat values are never promoted; (b) clearly-labeled history —
  `docs/debugger-feature-roadmap.md` (ARCHIVED) and
  `docs/schema-v2-cleanup.md` (change record). Any new live-path
  `observedTarget` reference is a bug.
- **`targetIdentity` is canonical.** `requestedTarget` (CLI input) is never
  presented as observation; launch `debuggee` seeds stay
  `confidence: "unavailable"` (launcher-args only) until a protocol event
  upgrades them; attach `endpoint` is OS-corroborated via `port_lookup` /
  `normalize_attach_host` or explicitly `unavailable` with reason.
- **No target-process environment collection.** Identity/redaction/caps operate on
  argv/cwd/executable/port only; the CLI never harvests or persists the target
   process's environment (its own operational `HOME`/`NODE_PATH`/cwd/temp reads
   aside, see §5.6). Any proposal to collect target env (or persist raw command
   lines) is rejected at review.

## 11. Contributor workflow (canonical gates)

Run gates via the canonical runner — the command list in §7 is informative,
the script is normative:

- Fast unit gate (no daemons/browsers): `scripts/run_gates.sh --unit`
  (`cargo test` + `cargo fmt --check` + Python unit minus the three live
  suites + `node --test tests/*.test.js` + `tests/test_install_checksum.sh`
  + owner-routing gates
  `scripts/check_nodebridge_owners.sh`,
  `scripts/check_browserbridge_owners.sh`,
  `scripts/check_pybridge_owners.sh`,
  `scripts/check_java_owners.sh` (each incl. `--self-test`) + `javac` bridge/checks
  + executed Java checks B/C/M4–M7 + saturation + framing + strict).
- Live gate: `scripts/run_gates.sh --live` (builds first when the binary is
  missing; `TEST_LANG=py|node|java|browser` + `SKIP_BROWSER=1` filter live
  only; JS unit always runs in full). Live entry is `tests/run_live.py`
  (all three live suites, one scope) with the §7 nonzero policy: default
  full requires py+node+java plus browser unless `SKIP_BROWSER=1`;
  without chrome, set `SKIP_BROWSER=1` explicitly — otherwise the gate
  fails instead of passing all-skipped.
- Full release-like gate (default, no args): unit + live.

Rules: one subsystem/language per commit; zero contract delta per refactor
commit (`docs/current-contract.md` byte-identical); serial edits to shared
files (`src/session/*`, `src/cli.rs`, `src/main.rs`, `SKILL.md`); rollback =
revert the single commit. `docs/current-contract.md` needs no gate beyond
"no diff"; `skills/agent-debugger/SKILL.md` first-run note is untouched
unless provisioning changes.
