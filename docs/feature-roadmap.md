# Feature Roadmap (deferred — NO commitment, NO date)

Status: prioritized deferred-feature list. Nothing here is promised,
scheduled, or authorized for implementation: each item ships only behind its
own FULL plan + FULL review + live-matrix proof, after the maintainability
program (`docs/maintainability-refactor.plan.md`, COMPLETE pending final
global review/gate). History (do not implement from):
`docs/debugger-feature-roadmap.md` (ARCHIVED).

Live contract: `docs/current-contract.md` (schema v2). Ownership/boundaries:
`docs/architecture-map.md` (§4 stateful inventory, §5 invariants, §8 refactor
policy). Every item below crosses a stateful boundary or a security boundary,
so the §8 rules apply (one subsystem/language at a time, zero contract delta
per refactor commit, serial shared-file edits).

Current capabilities (from `skills/agent-debugger/SKILL.md`, accurate as of
HEAD `51e3423`):

- Breakpoints: `--break path:line` (+ `|cond`), `method:` (Java fully; Python
  `method:funcname`; Node unsupported — use path:line), `--logpoint`,
  `--watch` write/read + `--exit` (Java only — Python/Node fail fast),
  `exc` (Python/Node: bare only; Java: `exc:Class` filter). ALL exception
  stops are UNCAUGHT only.
- `justMyCode` is always ON (Python stdlib/site-packages skipped; Node
  `node:` internals filtered in the same spirit). No user flag.
- `eval "refs(obj, N)"`: Java walks the heap (50 refs/node, depth ≤ 4);
  Python reports direct holders (depth 1); Node is unsupported (no GC walk
  via CDP) and honestly reports so.
- `eval`: bare static field names resolve off the frame's declaring type;
  qualified `ClassName.field` form is unsupported.
- Thread states are shown; lock owners are NOT (`threads` shows states, not
  lock owners).
- User cancel = `close` (no separate `cancel` command).

## P1 — caught-exception configuration (risk: HIGH — stop storms)

Today: uncaught-only everywhere. A caught-point mode multiplies stop volume
(stop storm) on every adapter's event pipeline: each bridge's
pump/park/event path (`pybridge.py` pump/park, `nodebridge.js` pause chain,
`browserbridge.js` park-or-drop, `BridgeSession` event queue) must classify,
filter, and attribute caught throws without breaking park stability or the
`no stopped thread` fail-fast rule.

- Discovery gate: per-adapter mechanism probe (debugpy caught-exception
  config; CDP `Debugger.setPauseOnExceptions` state; JDWP exception-request
  `caught` flag) on a temp dir, repo-untouched, before any plan.
- Contract gate: additive `stops.json`/event shape only; frozen timeout
  prefix, confirmed-only persistence, and `busy` semantics unchanged.
- Test gate: new unit (filter classification per adapter) + live
  (caught-storm fixture with bounded stop count + `close confirmed`).
- Non-goals: per-frame "break on all throws including runtime-internal"
  default-ON; changing the uncaught default.

## P1 — Node bounded refs / deep inspect (risk: MEDIUM)

Today: Node `refs()` honestly unsupported; Java/Python carry the heap-walk
precedent (bounded fan-out, depth cap, top-level shallow previews, no
getters, no target eval). A CDP-side design must prove a bounded walk
(`HeapProfiler`/`Runtime` domain queries) that cannot hang the worker,
leak retainers, or mutate the target — MEDIUM risk because the walk runs
against a live V8 heap over the same CDP session that serves pauses.

- Discovery gate: CDP capability probe (which heap-query path returns
  bounded, redacted results on the pinned Node) before any plan.
- Contract gate: same caps idiom as identity (`512 chars` field /
  `… (+N more chars)`, bounded counts); no target-env collection;
  additive response fields only.
- Test gate: new unit (cap/truncation/redaction fixtures in
  `tests/contract/`) + live (bounded-walk fixture, depth/fan-out asserts).
- Non-goals: unbounded transitive closure; invoking target getters during
  the walk; cross-session heap diffing.

## P2 — justMyCode control

Today: always ON. A user flag changes every adapter's frame pipeline
(Python `justMyCode` launch field; Node `node:` filtering; Java step
filters; browser URL filtering) + the redaction surface (user frames expose
more argv/source text). Medium blast radius, no single-adapter rollout.

- Discovery gate: per-adapter flag inventory (which layer owns the filter
  today) before any plan.
- Contract gate: additive CLI flag + `stops.json` intent field; default
  stays ON (no behavior change when omitted).
- Test gate: new unit (flag parsing/intent) + live per adapter
  (filtered vs unfiltered frame sets on one fixture).
- Non-goals: per-frame/per-module include-exclude lists in v1; changing
  the default.

## P2 — richer watches parity (write/read watches + method exit beyond Java)

Today: Java-only; Python/Node fail fast with guidance. Cross-adapter watch
semantics need DAP/CDP mechanism work per bridge (debugpy watch support,
CDP `DOMDebugger`/event-breakpoint equivalents), each with its own stop
attribution (`stopInfo` shape) and park-stability proof.

- Discovery gate: per-language mechanism probe (what the pinned
  debugpy/Node stack can actually watch) before any plan.
- Contract gate: `stopInfo` additive fields only; `verified`/`pending`
  plant vocabulary unchanged.
- Test gate: new unit (watch parse/arm per bridge) + live (write-watch
  writer-thread attribution fixture per newly supported adapter).
- Non-goals: hardware data-breakpoints; watch expressions with side
  effects; changing Java watch behavior.

## P3 — advanced features (each needs its own FULL plan; lowest priority)

- Hot swap / drop frame: mutates live frames — highest correctness risk;
  conflicts with the stale-frame fail-fast invariant. Non-goal until a
  measured user need + per-adapter mechanism proof exists.
- Reverse debugging / time travel: no recording infrastructure exists;
  out of scope for the current transport (one request per connection,
  bounded snapshots, no history).
- Profiler integration: separate concern from stop-based debugging; would
  need its own sampling/attribution contract.
- Full-heap dumps: unbounded by nature; conflicts with the bounded-preview
  invariant (`CHANGE_TRACK_MAX=256`, shallow previews). Non-goal; bounded
  `refs()` (P1) is the approved direction.
- Lock-owner display: `threads` shows states, not lock owners, by design
  (bridge-side owner queries risk target calls). Non-goal until a
  side-effect-free query path is proven per adapter.

## Explicit non-goals (all priorities)

Multiplex/request-id transport; v1 auto-migration; `meta.json`/`intent.json`
consolidation; `cancel` distinct from `close`; stale-snapshot mode; process
inventory/selection UI; silent target-process flag injection; attach-side
child/worker discovery; target-process environment collection; raw
command-line persistence; port-owner kills; `Target.*` flat session transport
(measured NO-GO on the pinned stack); changing launch-kill vs attach-detach
ownership.
