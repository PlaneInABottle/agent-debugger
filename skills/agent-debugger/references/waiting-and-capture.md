# Waiting, capture & long-polling

Full rules behind the wait/capture summary in SKILL.md.

Never `sleep 8; status; context` and never poll `status` in a loop. The
bridges long-poll for you:

- Safe recipe for request/mail/queue triggers (all four adapters):
  `attach` with a short timeout (session stays running, break armed) →
  trigger the request → `wait --timeout 20` (pure long-poll, NEVER
  resumes; immediate success if already parked, typed
  `timeout: no stop within Ns; <hint>` otherwise, session and intents
  preserved) → inspect/`continue` promptly. A parked HTTP handler keeps
  its connection open until you `continue`, a capture auto-resumes, or
  you `close` (detach) — every parked response carries this `warning`.
- A timeout NEVER means "unreachable code": the debugger cannot see your
  external trigger (an HTTP helper that crashed before sending looks
  identical to a quiet target), so every wait/capture timeout carries
  `waitContext` with `triggerStatus: "unknown"`, the waited time, the
  `expectedBreak` (capture only), and the redacted `targetIdentity`. On a
  timeout, verify the trigger path separately (did the helper actually
  send? did the request reach the handler?) instead of re-arming blindly.
  A `verified-but-unhit` break plus `unknown` trigger means "nothing was
  observed", not "this line cannot run".
- Wrapped servers need the layered view, not the endpoint argv. With
  `uv run --with debugpy python -m debugpy --listen 127.0.0.1:5678 -m
  uvicorn app.main:app`, the listener owner is the debugpy adapter
  (site-packages), NOT your app: read `targetIdentity.debuggee`
  (`protocol-confirmed` name/pid from the DAP `process` event) to confirm
  you attached the app, and treat `endpoint`/`adapter` as "where the
  debugger plugged in". Same rule for any launcher wrap (`uv`, `gunicorn`,
  `npm run`, `gradle bootRun`): the debuggee role is the truth, the
  endpoint argv is plumbing.
- `continue` resumes and then waits for the NEXT stop: after it wakes a
  parked handler, the request completes, and the command itself reports
  `timeout: no stop within Ns` when nothing else hits. That timeout
  means "resumed, nothing more hit" — not a stuck request. Verify with
  `threads` (running) instead of re-polling.
- One-shot alternative: `capture --break SPEC --timeout 20` on the
  running target plants an ephemeral line-only break (same parser as
  `breaks add`, never persisted, never inherited), waits for the park,
  collects a bounded snapshot (`--frames 1..10`, `--vars 1..20`),
  removes the ephemeral BEFORE resuming, and auto-resumes within
  `--pause-budget` (default 2000ms, max 10000ms; overrun still resumes,
  then reports `budgetExceeded`). Response: `pauseDurationMs`,
  `targetWasPaused`, `resumed` (+ `resumeError`/`removeError` when
  degraded), `truncated`. On an already-parked target it collects
  WITHOUT resuming (`targetWasPaused:true, resumed:false`) — it never
  resumes a pre-existing park and never leaves a fresh command-caused
  park suspended (collection/removal failure still resumes; timeout
  never resumes). Capture carries no eval expression and captured
  locals live only in the response. Browser capture does not survive a
  reload (navigation drops it into a timeout, never a stale resume).
  One-shot capture cannot observe code that ran before it armed: for a
  short-lived target, pre-arm at startup (`start --break SPEC`, then
  `capture` without `--break` on the parked target) or start `capture`
  before the external trigger fires. If the target exits first, the
  error names the stage truthfully — `capture target exited before
  ephemeral breakpoint was armed` (plant never confirmed), `target
  exited before capture hit` (armed, stop never arrived), or the
  session-gone variant when the session already exited — with additive
  `waitContext.captureStage` (`before-armed`/`armed-wait`/`session-gone`
  plus timeout variants) and `ephemeralPlanted`. A capture timeout or
  exit never means "unreachable code" and is never endpoint-rejected:
  verify the trigger path separately instead of re-arming blindly.
- A logpoint (`--logpoint`) is the true no-park alternative (fires and
  auto-resumes, never parks) but costs expression evaluation per hit;
  use it for tracing, `wait`/`capture` for inspecting.
- Suspend impact is real: while parked, a Java VM is fully suspended
  (all threads) and a Node process holds its event loop — keep the park
  short (inspect, then `continue` or let `capture` auto-resume) or the
  parked request times out on the client side.
- Same-line `continue` that stops immediately on the same line is
  usually legitimate, not a stale replay: a loop re-hit, step re-entry,
  another thread, an async continuation, or a slid plant. Diagnose,
  don't assume: every stop/context/capture response carries additive
  `diag` — session-monotonic `stopId`, `parkedAtMs`, stopping thread
  `{id,name}`, `reason`, native `hitBreakpoints` when the adapter
  reports them (null otherwise — never fabricated), attributed
  `requestedBreak`/`boundLine`/`hitCount` when known (null when not),
  `sameLocation`/`sameThread` vs the previous park, and
  `elapsedSincePreviousStopMs`. A loop re-hit reads
  `sameLocation:true` with `stopId+1` — expected, not a bug.
- Omitted `--target` auto-selects at acceptance time (already-parked
  target first); a fresh wait may be satisfied by the first stop on ANY
  target and the response stamps the actual one.
