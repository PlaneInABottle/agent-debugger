# Current Contract (schema v2)

Single layered `targetIdentity` everywhere; every sidecar carries
`schemaVersion: 2`. No migration framework, no dual schema: old (v1) dirs
are rejected with actionable errors except safe `close`/`status`.

## Sidecars (`~/.agent-debugger/sessions/<name>/`)

All four files are JSON objects written atomically (tmp+rename in the
same dir). CLI-owned: `lang.json`, `stops.json`. Bridge-owned:
`session.json`, `error.json`.

- `lang.json`: `{"lang":"py|node|java|browser","schemaVersion":2}`.
- `stops.json`: spawn intent (`breaks/logpoints/watches/exits/sources/
  timeout/target/requestedTarget`) + `"schemaVersion":2`. `target` is a
  spawn-derived display summary; `requestedTarget` is the endpoint+flags
  (never an observation).
- `session.json` (bridge): `name/kind/port/stopped/lastStop/updatedAt`
  + `"schemaVersion":2` + `"targetIdentity":{debuggee,endpoint,adapter}`.
  No `observedTarget` key (absent, not null).
- `error.json` (bridge, CLI-read): `{"schemaVersion":2,"error":"…",
  "phase":"transport|config|runtime"}` on every early path.
  `transport` = connection/protocol loss (endpoint diagnosis applies);
  `config` = semantic spec error (message stays top-level, no endpoint
  diagnosis); `runtime` = unexpected internal failure after a successful
  bridge operation (truthful internal message top-level, no endpoint
  diagnosis, never called endpoint-rejected). Absent/unparseable
  (or error-less) files keep the transport fallback. A present parseable
  file with a bad version/phase is corrupt:
  `corrupt setup error file in '<name>' (schemaVersion/phase); close and
  retry` (internal, never transport-diagnosed).

## Seed identity

The CLI passes `--target-identity <json>` before `--` (never swallowed by
program args). Malformed/missing seeds degrade bridges to all-unavailable
identity, never a spawn failure. Browser accepts the flag and builds its
own tab identity from `/json/list`.

- launch: `debuggee {source:"launcher-args", confidence:"unavailable",
  pid:null, …}` + explicit reason (protocol event upgrades it);
  endpoint/adapter `unavailable`.
- attach: `debuggee unavailable`; `endpoint {host,port,ownerPid?,
  executable?,argv?(redacted),cwd?,source,confidence:os-corroborated|
  unavailable}` from the localhost port lookup; `adapter unavailable`
  (no name guessing).

## Old sessions

- Gated (reject `unsupported session '<name>' (schema v1; close it and
  recreate)`): `continue wait capture step stack threads vars eval
  reload breaks logs targets context breaks-add/remove/clear`.
  No `logs` exemption: copy `logs.jsonl` aside manually before `close`.
- `close`: no version gate ever — old live/dead dirs clean via the
  version-agnostic framed `{"cmd":"close"}` + port-death check + remove.
- `status`: no gate, never errors a row, sorted by name.
  Current: all existing keys, no `observedTarget`, plus
  `stale:false, unsupported:false, hint:null`.
  Old: same keys, `stale:true, unsupported:true`,
  `hint:"close '<name>' and recreate (unsupported schema v1)"`;
  `lang` from `lang.json` else `"unknown"`; `port` numeric-u16 else `0`;
  `alive` by probe iff port nonzero; `kind/stopped/lastStop/updatedAt`
  from parseable `session.json` else `null`; `armed/target/
  requestedTarget` from parseable `stops.json` else nulls;
  `targetIdentity` when a valid layered object else `null`.
  v2 markers + absent `session.json` = current startup row; present
  `session.json` lacking `schemaVersion==2` = old, never current.
- `spawn` same name: v2-present always bails (`already exists (close it
  first)`). Old-present reclaims ONLY when proven dead (parseable
  `session.json`, numeric nonzero port, silent daemon); otherwise
  `unsupported session '<name>' (schema v1; close it first)`. Live old
  daemons are never killed by spawn — only `close` cleans them.

## Collisions

Endpoint locks still guard concurrent v2-vs-v2 attaches. Additionally,
before any v2 attach of an exclusive lang (`py|node|java`; browser never
collides): endpoint-matched old owners block with
`endpoint-already-attached` (all-unavailable identity, zero pids), then
any remaining live old session (exclusive or unknown lang, fail-closed)
blocks with `unsupported live legacy session(s) '<names>'; close them
first and retry` (`unsupported-legacy-live`).

## Install / upgrade

`cargo install --path .` updates CLI + adapters together (binary:
`/Users/Y_ALTAY1/.cargo/bin/agent-debugger`). A still-running old bridge
stays old code until closed; `close` uses only `port` + the generic close
frame, so closing old sessions after upgrade is safe. Installed-binary
smoke runs only after review, never mid-implementation.
