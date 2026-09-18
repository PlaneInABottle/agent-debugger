# Sessions, state & freshness

Detailed semantics behind the three-call reorientation in SKILL.md.

## Contents

- status fields, parks and hits
- additive and removing breaks
- target identity and old schema-v1 sessions
- delayed-attach recipe
- state freshness rules

- `status` shows `armed: {breaks/logpoints/watches/exits}` + `target`
  per session, persisted at spawn (`stops.json`). No session? Nothing to
  resume — start fresh.
- `status` also shows live `stopped` + `lastStop{file,line,method}` +
  `updatedAt` (rewritten by the bridge on every stop/resume/exit).
  `lastStop` survives resume and exit — it answers "where was I last",
  `updatedAt` marks the last transition (not every read).
- Stops that fire while no continue/step is waiting PARK visibly (all
  four bridges, within a second or two): `status` flips to `stopped:true`
  with the fresh `lastStop`, and `context`/`eval`/`step` work from the
   parked stop. You never need a blind `continue` to discover a stop —
   but note a parked stop still holds its target (a parked HTTP handler
   keeps its connection open until you continue). A parked stop is stable:
   other threads hitting breakpoints count `hits` without moving the park.
- `breaks` lists every armed stop with its plant state: `verified`,
  `pending` (class/script not loaded yet — normal for deferred code),
  `slid` (runtime moved it, `detail` names the real line),
   `shadowed` (logpoint killed by a same-line break), `armed` (no receipt
   available: exc/watch/exit — Python `method:` reports the adapter's own
   `verified`/`pending` receipt instead), `rejected`. `detail` carries the
  logpoint template / slide target / pending reason.
- `breaks` also reports `hits`: times the stop fired. Step landings
  never count (Python/Java exclude them structurally; Node/browser count
  only adapter-reported hit ids), so a dead breakpoint honestly reads 0.
   Logpoint fires count wherever visible (Java client-side, Node/browser
   auto-resumed pauses); `hits:null` means uncountable, not zero (Python
   logpoints fire inside debugpy, invisibly). After compaction, hits tell
   you which of your breakpoints are actually live.
- Additive breaks: `breaks add --break app.py:55` arms line breaks on the
  live session, running or parked (line breaks with `|cond` allowed;
  method:/exc:/logpoint/watch/exit are rejected). Only confirmed additions
  persist to `stops.json`, so `status`/`breaks`/intent reconverge on their
   own. Duplicates are idempotent; a same-line different-condition (or a
   same-line logpoint on Java/Node/browser) rejects the whole batch atomically.
- While a `continue`/`step`/`reload`/`wait`/`capture` is outstanding,
  live reads (`threads`/`breaks`/`logs`/`targets`) answer immediately
  from published state instead of queueing behind it — poll those, never
  sleep. A second `continue`/`step`/`reload`/`wait`/`capture` or
  breakpoint mutation on the SAME target is rejected at once with
  `busy: <cmd> outstanding for <target>` (no silent queue); different
  Python/Node targets proceed independently. A global `breaks
  add/remove/clear` conflicts with any outstanding resume/wait/capture,
  and `eval` is exclusive to its target while one is outstanding. `context`/
  `vars`/`stack` only succeed on a currently parked target (never stale
  frames); `close` is always accepted and settles the session type (launch
  reaps, attach detaches).
- Removing breaks: `breaks remove --break app.py:55` drops live line
  breaks by stored identity (the source may be deleted or changed and
  removal still works; a plain spec never removes a `|cond` record).
  `breaks clear` drops every live line break (logpoints/watches/exits
  untouched) and takes no args. Only confirmed removals leave
  `stops.json`; unmatched specs report `missing` without failing, backend
  failures keep the entries plus `failed[]`. Removing a Java break re-arms
  a same-line shadowed logpoint (armed now, or deferred when the class is
  not loaded); Node/browser do the same for startup-shadowed logpoints.
  Reload never restores removed breaks.
- Target identity: `start`/`attach` responses, `status` rows, and
  `context` carry `requestedTarget` (what you asked: endpoint/flags, pid
  always null — there is no pid input) and `targetIdentity` (three layered
  roles with strict confidence). `debuggee` is the program under test and
  is `protocol-confirmed` ONLY from protocol data (Python: the DAP
  `process` event name/pid; Node: the kept `/json/list` title/url; Java:
  the JDI VM name; browser: the attached tab). `endpoint` is the
  OS-observed listener owner (`os-corroborated` at most — on Python
  attach that is the debugpy *adapter*, not your code) and `adapter` names
  the adapter process when one exists (debugpy) or `inProcess:true` when
  the inspector lives inside the debuggee (Node/Java/browser). At spawn
  the CLI seeds these roles from launcher args / a localhost port lookup
  (`source: "launcher-args"`, confidence `unavailable` — launcher truth is
  not OS-corroborated); the bridge upgrades them from protocol facts.
  Anything unobserved is `unavailable` with a reason, never guessed —
  there is no parent-process inference. Timeout/unhit hints lead with the
  debuggee; all fields are redacted and capped before they persist or
  print. Identity source of truth is `status`: for a live session, read
  that session's `status` row `targetIdentity.debuggee.pid`. Same file
  attached on the wrong port is visible here: compare the endpoint
  ownerPid/argv before concluding the code is unreachable. `verified`
  still means "planted", never "this code ran". `context` may
  be `unavailable` or fail outright while the target is running — that is
  normal and says nothing about identity, so `context` is never the
  identity source. An `endpoint-already-attached` collision response
  carries the *owner's* layered `targetIdentity` in the same three-role
  shape (a reference convenience, not the live-session procedure: for
  identity answers about a live session, read `status`; owners without a
  layered identity report all roles `unavailable` with zero pids).
- Old sessions (schema v1, created before v2): every command except
  `status` and `close` rejects them with `unsupported session '<name>'
  (schema v1; close it and recreate)`. `status` shows them with
  `stale:true, unsupported:true` and a `hint` naming the close+recreate
  remedy — it never crashes a mixed listing. `close` always cleans an old
  dir (no version gate). There is no `logs` exemption: copy `logs.jsonl`
  aside manually before `close` when the lines matter.
- Delayed recipe: Java/Python/Node `attach --break` first waits up to
  `--timeout` for an immediate stop. If the line is not reached, it then
  returns a live running session with the breakpoint still armed (the
  session and `stops.json` are preserved). For mail/queue/request triggers
  that will happen later, pass a short timeout to avoid dead waiting, then
  `breaks add` any newly discovered lines, trigger the target, and
  `continue` to the stop. Browser attach is different: it arms and returns
  immediately without this initial wait.
- Name sessions after the task (`--session cart-npe`): the name is the
  only "why" that survives, and it costs nothing extra.
- Never `rm -rf` a session dir instead of `close` (bridges self-reap, but
  `close` is the contract).

## State & Freshness (ref lifecycle analog)

- Values go stale after every `step`/`continue`. Re-read; never reason
  from a previous stop's numbers.
- After VM exit every command fails with "close this session" — that is
  signal (program ran to completion), not flakiness: `close` and move on.
- After editing source, recompile BEFORE the next session, otherwise the
  snippet (fresh file) and line numbers (old bytecode) disagree.

