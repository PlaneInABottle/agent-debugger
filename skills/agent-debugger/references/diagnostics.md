# Diagnostics: deadlocks, crashes, attach collisions

## Deadlock / No-Stop Sessions

- A session needs no stopping breakpoint: `attach --port X` alone opens a
  live session you can `threads`-dump without ever stopping the VM.
- `threads` freezes the VM for milliseconds, captures every thread + top
  frames, then resumes (on a stopped session it stays stopped — suspend
  counts verified balanced). Look for `MONITOR` state + same method/line on
  two threads = deadlock smell. We show states, not lock owners.
- Eval/vars/step need a stopped thread; on a running session they fail
  fast with "no stopped thread" instead of hanging.

## Logs & Crash Evidence

- `logs` keeps the latest 2000 lines per session (ring): `total` = retained
  lines on disk (≤2000), `dropped` = lifetime lines evicted, `truncated` =
  true when the tail was cut OR any line was ever evicted. New output never
  freezes on stale history — old lines age out.
- Any setup crash on any bridge writes `error.json` with a sanitized
  `internal:` cause (exception class + short stack, ~2KB, no env) and the
  CLI surfaces it; failed starts are removed wholesale so the name stays
   reusable. `session.json` itself is published atomically — concurrent
   `status` never reads a torn file.

## Attach collisions + diagnostics (py/node/java)

- One debug server takes one debugger: a second `attach` to an endpoint
  already owned by a live session fails fast with `attach failed:
  endpoint is already attached by session 'X'` (names the owner — use
  it or `close` it first; the machine `diagnosis.code` stays
  `endpoint-already-attached`). The first session and its target are untouched; `localhost` /
  `127.0.0.1` / `::1` (plus the rest of 127/8) count as the same
  endpoint. This is a conservative safety policy, not a proven fact
  about every server: debugpy observably refuses a second attach (and
  can kill the target), while Node-inspector/JDWP single-client
  behavior is configuration-dependent — we block anyway because the
  risk is refusal or target death. Browser tabs multiplex, so
  `browser attach` is never blocked here. The collision error also
  carries the owner's layered `targetIdentity` (same three roles — a
  reference convenience, not the live-session procedure: for identity
  answers about a live session, read `status`).
- A failed `attach` carries `diagnosis: {code, confidence, evidence,
  recommendation}` next to a concise diagnosis-aligned top-level `error`
  (always `attach failed:`-prefixed: `no debug listener found`,
  `closed while attaching`, `rejected ... may already have another
  debugger client`, `could not reach ... (unverified)`, `already
  attached by session X`). The raw adapter text rides separately as
  sanitized, capped `cause` (never duplicated into `error`); human
  output prints `error`, then `cause:` only when it adds information,
  then the diagnosis recommendation and identities. Semantic setup errors
  (invalid breakpoint/method/line/condition/source — bridge-typed
  `phase: config`, never inferred from message text) stay top-level
  verbatim with no endpoint diagnosis; unexpected internal bridge
  failures (`phase: runtime`) stay top-level verbatim with no endpoint
  diagnosis and are never called endpoint-rejected; connection loss and
  target exit stay `transport` and keep endpoint diagnosis. Undiagnosed
  errors are unchanged.
