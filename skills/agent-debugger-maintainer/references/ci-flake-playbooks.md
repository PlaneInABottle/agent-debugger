# CI flake playbooks

Signature-first recipes. Each maps the exact failure text to the first
evidence to pull and the invariant to check. Start every playbook from
one failed run's evidence (bundle + job log), never from a local retry
alone — and confirm the run's `headSha` matches local `HEAD` first.

## "timeout: no stop within Ns ... [parks: ... worker:N=parked lastPark=worker:N]"

Symptom: a step/continue on a parked target times out while the park is
right there.

1. Correlate the CLI error time with `<session>/bridge.log` trace lines
   (ISO timestamps): was the park recorded before or after the command
   continuation reached the pump?
2. Suspect the selection-baseline race: a freshness baseline captured
   at pump entry can equal the very park the command must serve (CDP
   response and paused event coalesce in one read).
3. Regression shape: queue the pause through the real chains while the
   request is in flight (`tests/m5_concurrency.test.js` pattern); assert
   RED against current code with the exact signature, then fix by
   capturing the baseline before the request and passing it through.

## "timed out waiting for debug session on port X" (bridge unresponsive)

Symptom: connect succeeds, no response arrives; other sessions run fine.

1. Look for `watchdog: stalled:` plus thread stacks in `bridge.log` (the
   bundle ships them). No dump means handlers stayed under their budget —
   re-read the CLI's own request timeout in `src/main.rs` instead.
2. Stack shows two readers on one DapConn: shared-socket bound armed
   outside the lock; apply bounds under `mu`.
3. Accept-loop stack inside a pump read: a blocking read lost its bound
   to another thread's `settimeout` between arming and `recv`.

## "wait woke on timeout, not the event" / wall-clock assertions

Symptom: a strict `< Ns` assertion fails at N + a few ms while the park
is intact.

1. Pull `pressure.log` from the same run (CI) and check memory/load at
   the failure minute.
2. Usually host pressure: release heavy fixtures when their test ends
   (Chrome after the last browser test), keep the timeout strict, let
   retry-once heal the spike.
3. Recurring with clean pressure data: treat as a product timing bug and
   instrument the wait path (trace + watchdog) before touching code.

## Windows-only unit failures

Symptom: Linux/macOS green, `Test (windows-latest)` red.

1. Locally unreproducible; read the WinError text in the job log.
2. Known classes: concurrent `os.replace` onto one target (WinError 5) →
   serialize writers or use distinct targets; MSYS path/arg munging in
   shell gates → `cygpath -w` + `MSYS2_ARG_CONV_EXCL=*`; MSYS `ps` is
   blind to native PIDs → `tasklist`/ctypes fallback.
3. Keep tests platform-neutral (skip only genuinely POSIX-only cases)
   and let CI verify Windows.
