# Session Protocol

Wire contract between the Rust CLI and the four bridge daemons
(py/node/browser/java). Bridges implement this; agents rely on it.
Behavioral rules here are normative — code comments point here, tests pin
the load-bearing ones.

## 1. Transport

- One TCP connection per request on 127.0.0.1, to the port published in
  `<session>/session.json`. Exactly one request and one response per
  connection; the client closes after reading the reply.
- Framing: `Content-Length: <bytes>\r\n\r\n<json>` (bytes, not chars).
  The body must be a JSON object.
- Size discipline: CLI accepts up to 64MB replies; bridges cap inbound
  frames at ~1MB with a ~5s read deadline and reject oversize frames
  instead of accumulating.
- Every served response names its target (`"target"`). Multi-target
  bridges echo the served id; main-only bridges predate the field, so the
  CLI stamps `"main"` on their replies.

## 2. Session lifecycle

- `start`/`attach` spawns a bridge daemon that writes `owner.json`
  (`{pid, nonce}`) first, then publishes `session.json`
  (`{name, kind, port, stopped, lastStop, updatedAt, schemaVersion: 2,
  targetIdentity}`) on every park/resume/exit.
- A daemon whose session dir disappears (owner `rm -rf`) or whose nonce
  mismatches exits quietly instead of orphaning (abandonment guard runs
  on every wait tick).
- `close` is terminal and always accepted, even with a resume
  outstanding: the bridge tears down (launch kills its target, attach
  detaches) and ACKs `{ok:true, closed:true}` over a graceful FIN.
  The CLI waits up to 65s (a close queues behind a blocking
  step/continue), then polls the port dead before deleting the dir.
- Bridges install a SIGTERM teardown identical to close: the CLI reaps
  failure paths TERM-first (bounded grace, then tree-kill), never bare
  kill, so a launched target dies with a reaped bridge.

## 3. Target model

- `main` always exists. py/node add `child:<pid>` / `worker:<sid>`
  targets; java/browser are main-only (an explicit non-main target is
  rejected before any forward).
- Omitted `target` auto-selects (most recently stopped live target, else
  main). Explicit `main` pins main on multi-target bridges (no-op on
  main-only ones). Explicit non-main pins that target or fails
  (`unknown target`, `has exited`, `was released`).
- `targets` lists the roster with per-target state; dead bridges error
  instead of fabricating a roster.

## 4. Wait model

- `continue`/`step` resume then wait; `wait` only waits (never resumes);
  `reload` (browser) reloads then waits. Bounds: every wait carries a
  CLI-checked `1..=3600`s timeout; the bridge deadline is monotonic
  (wall-clock steps must not stretch or shrink it).
- Freshness: the resume caller captures a baseline BEFORE the request
  that can produce the park. A park counts as fresh only if newer than
  the baseline; stale or foreign parks stay parked and the wait
  continues. (Rationale: transport frames coalesce — the pause can land
  before the caller's continuation runs.)
- Park publication is synchronous, before enrichment awaits: the moment
  a stop exists, concurrent waiters and live reads observe it. A
  stack-unavailable failure still publishes the park epoch; only the
  dispatching call reports the error.
- Handoff: a park consumed by a rival pump still satisfies the owning
  waiter through the shared target+epoch. The deadline path rechecks
  the handoff before reporting timeout.
- Concurrency: one resume/wait/capture per target; rivals busy-reject
  (`busy: <cmd> outstanding for <tid>`). Global breakpoint mutations
  conflict with any outstanding resume. Live reads (status/context/
  threads/breaks list) never block behind a wait. `close` bypasses all
  of it.
- Step accounting: a pending step is consumed by the stop it produces —
  including exception stops. Timeouts clear the flag but never disarm a
  concurrently landed park.

## 5. waitContext (wait/capture timeouts only)

Additive structured context on timeout errors; the message keeps the
frozen `timeout: no stop within Ns` prefix. `continue`/`step` timeouts
stay bare.

```
{
  "waitStartedAt": <epoch seconds>,
  "waitedMs": <real milliseconds waited, monotonic>,
  "triggerStatus": "unknown",
  "expectedBreak": "<spec>",        // capture only, when one was planted
  "captureStage": "<stage>",        // capture only (armed-wait, before-armed, ...)
  "ephemeralPlanted": <bool>,       // capture only
  "targetIdentity": {...},          // layered, redacted
  "note": "external trigger execution is not observed by the debugger; ..."
}
```

`triggerStatus` is always `unknown`: the debugger never observes the
external trigger. `waitedMs` is measured monotonically; `waitStartedAt`
is a reported epoch value.

## 6. Error envelope

`{ok:false}` replies map to CLI errors with this shape:

```
{
  "ok": false, "command": "<cmd>", "error": "<display string>",
  "cause": "<sanitized raw adapter text>",       // attach setup only, never a duplicate of error
  "waitContext": {...},                          // wait/capture timeouts only
  "diagnosis": {"code", "confidence",            // diagnosed attach setup only
                "evidence", "recommendation"},
  "targetIdentity": {...}, "requestedTarget": {...}
}
```

Attach diagnosis codes: `endpoint-already-attached`,
`endpoint-closed-during-attach`, `endpoint-rejected`,
`endpoint-not-listening` (high confidence only with a readable local
listener source), `endpoint-unreachable` (low — host not inspectable).
Bridge-internal failures after a successful operation report phase
`runtime`; spec/validation failures report the message verbatim
(`config`) without endpoint diagnosis.

## 7. Breaks, logpoints, watches, exits

- Line breaks `path:line[|cond]`, method breaks `method:Name`,
  exception breaks `exc[:Class]`; logpoints carry a `{var}` template.
  Conditions are target-language expressions evaluated at the stop.
- `breaks add` is transactional per file; `remove`/`clear` drop only
  what the bridge confirms removed (echoed stored raws), persisting the
  subset. The CLI persists intent in `stops.json` under a kernel
  file lock; applied-live-but-unpersisted is an explicit error, never
  silent divergence.
- `capture` (`--break` + budgets) plants one ephemeral breakpoint,
  waits, snapshots bounded (`--frames/--vars/--pause-budget`), then
  removes the ephemeral and auto-resumes. Timeout/exit never resumes;
  the ephemeral never leaks (removal failure is reported, not hidden).

## 8. Logs and status

- `logs` tails bridge-collected logpoint lines (`logs.jsonl`, bounded
  ring); `status` lists sessions with bridge-published
  `stopped`/`lastStop`/`updatedAt` plus a protocol-aware liveness probe
  (a stranger reusing the port never reads as live). `status` never
  fails a row: unparseable files read as nulls.
