# Language & target notes

## Contents

- Python Notes (debugpy)
- Targets (Python child / Node worker)
- Node Notes (raw CDP)
- Browser Notes (CDP tab attach)
- Threads, Async, Timing

## Python Notes (debugpy)

- `py start app.py -- args` launches via an isolated venv (auto-created,
  debugpy auto-installed once). `py attach --port` connects to a target
  started with `python -m debugpy --listen PORT app.py`.
- `py start --module mypkg.mod -- args` runs `python -m` instead of a file
  (exactly one of the positional program and `--module`; the name is a
  dotted identifier validated before anything runs). Module resolution is
  debugpy's, from the target cwd (your CLI cwd) — run from the package
  root. File launch is unchanged.
- `py attach --break ...` is not instant: it waits up to `--timeout` for the
  first hit, then returns running with the break still armed if nothing has
  hit. For a mail/job/request you will trigger later, use a short timeout
  (for example `--timeout 2`) instead of paying the default 20-second wait.
- Breakpoints are `file:line`; conditions are FULL Python expressions
  (`order.price > 1000`) — no allowlist, debugpy compiles them server-side.
- Path semantics: resolved from YOUR cli cwd (the target cwd never
  changes). An existing file wins as given — prefer the full path relative
  to where you run (e.g. `src/tests/.../test_x.py:378`). Otherwise an
  explicit `--src <root>` locates it: nested relatives join onto each root,
  a bare basename (`test_x.py:378`) is searched only beneath explicit
  `--src` roots and must match exactly one file. Zero/ambiguous matches,
  like lines past EOF (`no such line` names the spec and the file's total),
  fail fast before the target runs; a `target exited` with no stop plus an
  `unresolved breakpoints` note means the line never bound (wrong file,
  non-executable line, or an unfired slide — stdlib excluded by justMyCode
  never binds), not a lost session. `breaks` shows the plant state
  (`pending`/`slid` + detail names why / the real line). `breaks add` resolves the same way against the live `--src` roots.
- `eval` runs real Python (comprehensions OK). `method:` = function name,
  bare `exc` = any uncaught exception. `--watch/--exit` do NOT exist for
  Python (no debugpy equivalent) and fail fast — don't try them.
- justMyCode is always on: stdlib/site-packages frames are skipped.
- Startup banners never pollute `logs`; module-frame vars resolve via
  Globals fallback.
- Subprocesses: opt-in multi-target via `py start --subprocess` (default
  stays main-only; `attach` never follows children). `subprocess.Popen`,
  `multiprocessing` spawn, and `os.fork` children are addressable targets
  (see Targets below). The `resource_tracker` daemon is skipped without
  budget. Over-budget children are released (never parked, app never
  hangs); the parent's exit never marks live children exited.

## Targets (Python child / Node worker)

- `targets` lists the roster: `{id, kind: main|child|worker, pid,
  state: running|stopped|exited|ignored, lastStop, scope}` (child/worker
  entries add `observed` with their protocol facts: child pid source,
  worker url/type) plus `selected`/`ignored`/`droppedExited`. Main entries
  carry no `observed` — main identity lives in the top-level
  `targetIdentity`. Ids are opaque (`child:<pid>`,
  `worker:<sessionId>`), never reused. Max 8 live non-main targets + 16
  exited history.
- Every served response names its `"target"` (no silent rerouting).
  Targetless `context`/`eval`/`vars`/`stack`/`threads`/`continue`/`step`
  serve the most recently stopped live target, else main. Bare `breaks`
  aggregates all live targets (each record tagged).
- The first stop on ANY target satisfies launch. Startup `--break` is
  global intent and inherits into later targets; `breaks add --target X`
  is ephemeral (no `stops.json`, no inheritance); `remove --target X`
  matches only X's ephemeral records; bare `remove` drops the global
  intent plus inherited copies; bare `clear` resets all line breaks.
- Unknown/exited targets error explicitly (`unknown target` / `has
  exited`). `status` stays main-focused; `targets` owns the roster.
  `close` on launch reaps the whole tree (Python) / process (Node).

## Node Notes (raw CDP)

- `node start app.js -- args` launches via `--inspect-brk` (ephemeral port,
  auto-discovered); first use installs the tiny `ws` client once into an
  isolated dir. `node attach --port` connects to a target started with
  `node --inspect=PORT app.js`. Both `.js` and ESM `.mjs` work.
- Breakpoints are `file:line`; conditions are FULL JS expressions — but
  frame-locals only (see Conditions above for the closure-capture trap).
- `eval` runs real JS in the stopped frame (objects expand one level);
  bare `exc` = any uncaught exception with its class in `stopInfo`.
  `method:`/`--watch`/`--exit` do NOT exist for Node and fail fast.
- `node:` internal frames are filtered (justMyCode spirit); block scopes
  (`for (let i...)`), catch bindings, script-level lets, and closures merge
  into frame locals, innermost wins.
- Path semantics match Python: an existing file from your cwd wins; otherwise
  an explicit `--src <root>` locates it (nested relatives join onto each
  root, a bare basename is searched only beneath explicit roots and must
  match exactly one file). Zero/ambiguous matches fail fast before the target
  runs. Startup repeats of the exact same `file:line[|cond]` collapse to one;
  the same line with a different condition — or any same-line logpoint (one
  V8 breakpoint per line wins) — fails fast instead of shadowing.
- An attached inspector keeps the target process alive after its script
  ends: `threads` then reports "target VM has exited" (signal, like the
  other adapters) while `logs` keeps serving — `close` reaps the process.
- Node is single-threaded: any stop freezes the whole event loop, and a
  logpoint costs a pause+evaluate+resume round-trip per hit (not
  microseconds). Keep logpoint templates cheap and stopped sessions short.
- Inspector chatter (`Debugger attached.` etc.) never pollutes `logs`;
  program stdout AND stderr are both captured.
- `worker_threads`: opt-in multi-target via `node start --workers`
  (default stays main-only). Each worker is an addressable target (see
  Targets below); worker-only lines hit only with tracking on. Hit truth
  is `hitBreakpoints`, never the `reason` label. Workers that exit retire
  to history; main exit retires every worker.

## Browser Notes (CDP tab attach)

- `browser attach --tab '<url-or-title-fragment>'` attaches to a page in a
  Chrome running with `--remote-debugging-port=9222` (default = the
  agent-browser convention, so interaction and debugging share one warm
  headless browser). Ambiguous/empty matches fail fast with the tab list.
- Breakpoints are `url-frag:line` (`app.js:14` matches `.../app.js?v=3`);
  conditions are FULL JS expressions (frame-locals only, same V8 trap as
  Node). Bare `exc` = any uncaught exception. `method:`/`--watch`/`--exit`
  do NOT exist and fail fast.
- Attach is instant and never waits: an attached tab is usually idle (its
  load code already ran), so `attach --break` only ARMS (`armed: N` in the
  response). The agent loop: attach --break (fast) -> `reload` -> stop.
  `reload` with nothing armed just refreshes (`{reloaded: true}`).
- `logs` serves page `console.*` (captured via CDP, no pipes); snippets come
  from the tab via `getScriptSource` (no disk access). Snippet/step shapes
  match the other adapters. Frame locals merge block/catch/script/closure
  scopes like Node, innermost wins. `--src` is accepted but unused for browser
  (no URL-to-local mapping yet).
- Tabs don't exit like processes: script end is invisible, so a bare
  continue-to-end burns its timeout (pass a short one). Dead browser / closed
  tab surface as errors on `threads` (never stale `running:true`).
- Coordination with agent-browser on the same tab: debugger paused =>
  no clicks (a click/eval waits for page settle that never comes while
  paused — fire-and-forget with a short timeout, then poll our session
  state); interaction running => no step. Pause state is shared.

## Threads, Async, Timing (Java; Python threads similar, no freeze caveat)

- `current: true` marks the stopped thread; `+N more threads` is capped.
  Others stay frozen while you inspect (SUSPEND_ALL) — `continue` resumes all.
- Async (`CompletableFuture`, executors) works: the stop lands on the pool
  thread running that stage, with its stack and locals. Virtual threads work
  too (locals, eval, step verified) but are often **unnamed** — identify by
  thread `id`, not `name`.
- Timing caveat: freezing all threads perturbs races and can trip
  `future.get(timeout)` in the target. For Heisenbugs prefer `logpoint`
  (freezes only the hitting thread, microseconds) over stopping breaks, and
  keep stopped sessions short — the target waits while you think.
