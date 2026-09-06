/* Node CDP bridge for agent-debugger (N1: CDP core).
 *
 * Mirrors pybridge session mode: a per-session daemon speaking OUR session
 * protocol (Content-Length JSON over TCP on 127.0.0.1) to the Rust CLI, and
 * raw CDP over WebSocket (via the provisioned `ws` package) to a
 * `node --inspect` debug target.
 *
 *     nodebridge.js session --kind launch --dir DIR --program app.js [--node BIN]
 *         [--src D]... [--break SPEC]... [--logpoint SPEC]... [--timeout S]
 *         [-- args...]
 *     nodebridge.js session --kind attach --dir DIR --host H --port P
 *         [--src D]... [--break SPEC]... [--timeout S]
 *
 * Break forms: path:line[|cond] | exc (any uncaught). method:NAME,
 * --watch and --exit have no CDP equivalent for Node and fail fast at parse
 * time. CDP natively supports breakpoint conditions; logpoints are
 * client-side (pause, evaluate template holes, append to logs.jsonl, resume).
 *
 * Known V8 boundary (measured, not worked around — every CDP client shares
 * it): conditions may only reference variables LOCAL to the stopped frame.
 * Closure-captured variables in a condition misbehave (spurious stops with
 * an unresolvable scope, or silent misses). Prefer frame-locals in `|cond`;
 * use a plain line break + eval for anything captured.
 *
 * worker_threads: the MAIN thread only. Worker code runs on separate
 * inspector targets (CDP Target domain) which this bridge does not follow —
 * same boundary as subprocesses in the other adapters. Breakpoints on
 * worker-only lines never hit (plain timeout, not an error).
 *
 * Two spike-proven rules shape the handshake: breakpoints go in BEFORE
 * Runtime.runIfWaitingForDebugger (else they sit pending), and scriptId->url
 * is tracked from scriptParsed (paused frames carry no url).
 *
 * Snapshot shapes intentionally match the Java/Python bridges
 * (location/threads/frames/locals/changed/stopInfo) so agents see one
 * uniform surface.
 */

'use strict';
const { spawn, spawnSync } = require('child_process');
const fs = require('fs');
const http = require('http');
const net = require('net');
const os = require('os');
const path = require('path');
const { pathToFileURL } = require('url');

class Usage extends Error {}
// Wire core lives in the shared modules (single source under bridge/js/,
// provisioned next to this bridge):
//   ./cdp_conn.js  BridgeErr + CdpConn (id-matched CDP over ws)
//   ./framing.js   readFrame + writeFrame (Content-Length + JSON)
const { BridgeErr, CdpConn } = require('./cdp_conn.js');
const { readFrame, writeFrame } = require('./framing.js');
class CloseSession extends Error {}
// Typed first-stop timeout: attach falls back to a live running session,
// launch still fails. Never match timeout by message string.
class StopTimeout extends BridgeErr {}

const MAX_STRING = 200;
const MAX_FIELDS = 20;
const MAX_VARS = 20;
const MAX_FRAMES = 10;
const MAX_LOG_LINES = 2000;
const MAX_OUTPUT = 4000;
// M5 concurrency (frozen): serve accepts connections concurrently on a
// small fixed cap of active handlers (one response per connection); the
// connection queue itself is bounded (flood overflow destroys immediately).
// A second same-target resume/mutation busy-rejects instead of queuing.
const MAX_ACTIVE_HANDLERS = 8;
const MAX_QUEUED_CONNS = 16;
const RESUME_CMDS = new Set(['continue', 'step']);
const WAIT_CMDS = new Set(['wait']);
const CAPTURE_CMDS = new Set(['capture']);
// Parked-stop UX: every parked response carries this warning (suspend
// semantics, HTTP handler impact). No root-cause claim, ever.
const PARK_WARNING = 'parked breakpoint suspends target; HTTP handler remains ' +
  'open until continue/capture-resume/close(detach)';
const MUTATION_CMDS = new Set(['breaksAdd', 'breaksRemove', 'breaksClear']);
// Inspector chatter on stderr (never user data, just noise in logs).
const NOISE_LINES = new Set([
  'Debugger attached.',
  'Waiting for the debugger to disconnect...',
  'For help, see: https://nodejs.org/learn/getting-started/debugging',
]);




// ---------------------------------------------------------------- args

function needInt(flag, raw) {
  const n = parseInt(raw, 10);
  if (Number.isNaN(n)) throw new Usage(`${flag} needs a number (got '${raw}')`);
  return n;
}

function isBareName(raw) {
  return !raw.includes('/') && !raw.includes(path.sep)
    && (path.altsep == null || !raw.includes(path.altsep));
}

/** Recursive basename matches under explicit roots (canonical, deduped).
 *  Skips symlinked directories (no loop/hide surprises) and stops once
 *  ambiguity is proven (limit reached), so huge trees cost little. */
function findBasenames(name, srcDirs, limit = 6) {
  const found = [];
  const seen = new Set();
  for (const root of srcDirs) {
    let stat;
    try {
      stat = fs.statSync(root);
    } catch (_) {
      continue;
    }
    if (!stat.isDirectory()) continue;
    const stack = [root];
    while (stack.length > 0 && found.length < limit) {
      const dir = stack.pop();
      let entries;
      try {
        entries = fs.readdirSync(dir, { withFileTypes: true });
      } catch (_) {
        continue;
      }
      const subdirs = [];
      for (const e of entries) {
        const full = path.join(dir, e.name);
        if (e.isSymbolicLink()) continue;
        if (e.isDirectory()) {
          subdirs.push(full);
        } else if (e.isFile() && e.name === name) {
          let cand;
          try {
            cand = fs.realpathSync(full);
          } catch (_) {
            cand = path.resolve(full);
          }
          if (!seen.has(cand)) {
            try {
              if (!fs.statSync(cand).isFile()) continue;
            } catch (_) {
              continue;
            }
            seen.add(cand);
            found.push(cand);
            if (found.length >= limit) break;
          }
        }
      }
      subdirs.sort();
      for (let i = subdirs.length - 1; i >= 0; i--) stack.push(subdirs[i]);
    }
    if (found.length >= limit) break;
  }
  return found;
}

/** Map a user-given path to a canonical file (Python resolver semantics,
 *  JS-native implementation). Raises Usage fail-fast before the target
 *  runs when nothing — or more than one thing — matches:
 *  an existing file from the cwd wins as-is; otherwise an explicit --src
 *  root must locate it. Bare basenames get a bounded recursive search
 *  under explicit roots only; nested relatives join onto each root. */
function resolveSourcePath(raw, srcDirs) {
  const srcs = (srcDirs || []).map((s) => {
    try {
      return fs.realpathSync(path.resolve(s));
    } catch (_) {
      return path.resolve(s);
    }
  });
  if (path.isAbsolute(raw)) {
    try {
      if (fs.statSync(raw).isFile()) return fs.realpathSync(path.resolve(raw));
    } catch (_) { /* fall to Usage below */ }
    throw new Usage(`no such file: ${raw} — check the path and try again`);
  }
  const cwdTry = path.resolve(raw);
  try {
    if (fs.statSync(cwdTry).isFile()) {
      // Cwd wins over --src: preserves behavior for full repo-relative
      // paths; different spellings of one file still collide below
      // because every result is canonical (realpath).
      return fs.realpathSync(cwdTry);
    }
  } catch (_) { /* not in cwd — consult --src */ }
  if (isBareName(raw)) {
    const matches = srcs.length > 0 ? findBasenames(raw, srcs) : [];
    if (matches.length === 1) return matches[0];
    if (matches.length === 0) {
      const where = srcs.length > 0
        ? `tried ${cwdTry}; searched ${srcs.join(', ')} with no match`
        : `tried ${cwdTry}; no --src roots given to search`;
      throw new Usage(`no such file: ${raw} (${where}) — use the full path ` +
        `relative to the current directory or pass --src <root> containing ${raw}`);
    }
    const extra = matches.length >= 6 ? '+' : '';
    const shown = matches.slice(0, 5).join('\n  ');
    throw new Usage(`ambiguous breakpoint path: ${raw} matches ` +
      `${matches.length}${extra} files:\n  ${shown}\nuse the full path ` +
      `relative to the current directory or --src <root> to disambiguate`);
  }
  const tried = [cwdTry];
  for (const src of srcs) {
    const cand = path.normalize(path.join(src, raw));
    tried.push(cand);
    try {
      if (fs.statSync(cand).isFile()) return fs.realpathSync(cand);
    } catch (_) { /* next root */ }
  }
  throw new Usage(`no such file: ${raw} (tried ${tried.join(', ')}) — use the full ` +
    `path relative to the current directory or pass --src <root>`);
}

function canon(p) {
  // V8 reports canonical paths (symlinks resolved: /tmp -> /private/tmp
  // on macOS), so urlRegex matching must use the real path too. Fall back
  // to resolve() for not-yet-existing files (pending breakpoints stay
  // pending, not broken).
  const abs = path.resolve(p);
  try {
    return fs.realpathSync(abs);
  } catch (_) {
    return abs;
  }
}

function parseBreak(spec, cfg) {
  let cond = null;
  let head = spec;
  const bar = spec.indexOf('|');
  if (bar >= 0) {
    head = spec.slice(0, bar);
    cond = spec.slice(bar + 1).trim();
    if (!cond) throw new Usage("empty condition after '|'");
  }
  if (head === 'exc' || head.startsWith('exc:')) {
    if (head !== 'exc') {
      throw new Usage(`Node adapter stops on any uncaught exception; ` +
        `class filter unsupported: ${spec} (use bare 'exc')`);
    }
    cfg.wantExc = true;
    return;
  }
  if (head.startsWith('method:')) {
    throw new Usage(`method: breakpoints unsupported on Node yet: ${spec} (use path:line)`);
  }
  const colon = head.lastIndexOf(':');
  if (colon <= 0) throw new Usage('--break must look like path:line, exc');
  const lineno = parseInt(head.slice(colon + 1), 10);
  if (Number.isNaN(lineno)) throw new Usage(`bad line in --break: ${spec}`);
  const resolved = resolveSourcePath(head.slice(0, colon), cfg.srcs || []);
  cfg.breaks.push({ path: resolved, line: lineno, cond });
}

function parseLogpoint(spec, cfg) {
  const first = spec.indexOf(':');
  const second = first >= 0 ? spec.indexOf(':', first + 1) : -1;
  if (first <= 0 || second <= 0) throw new Usage('--logpoint must look like path:line:template');
  const resolved = resolveSourcePath(spec.slice(0, first), cfg.srcs || []);
  const lineno = parseInt(spec.slice(first + 1, second), 10);
  if (Number.isNaN(lineno)) throw new Usage(`bad line in --logpoint: ${spec}`);
  cfg.logpoints.push({
    path: resolved,
    line: lineno,
    template: spec.slice(second + 1),
  });
}

/** Fold startup specs before any CDP traffic or target run (mirrors the
 *  live `breaks add` contract): an exact same path/line/cond repeat is
 *  idempotent (kept once); the same path/line with a different condition —
 *  or any same-line logpoint (one V8 breakpoint per line wins) — fails
 *  fast. Paths are already canonical, so different spellings of one file
 *  still collide. */
function dedupeStartupBreaks(cfg) {
  const kept = [];
  const seen = new Set();
  for (const b of cfg.breaks) {
    const key = `${b.path}:${b.line}|${b.cond || ''}`;
    if (seen.has(key)) continue; // exact duplicate: idempotent
    const other = kept.find((o) => o.path === b.path && o.line === b.line);
    if (other) {
      throw new Usage(`conflicting condition for ${b.path}:${b.line} ` +
        `(already requested${other.cond ? ` as '${other.cond}'` : ' plain'}): ` +
        `${b.path}:${b.line}${b.cond ? `|${b.cond}` : ''}`);
    }
    seen.add(key);
    kept.push(b);
  }
  cfg.breaks = kept;
  const keptLogs = [];
  const seenLogs = new Set();
  for (const l of cfg.logpoints) {
    const key = `${l.path}:${l.line}|${l.template}`;
    if (seenLogs.has(key)) continue; // exact duplicate: idempotent
    const brk = cfg.breaks.find((b) => b.path === l.path && b.line === l.line);
    if (brk) {
      throw new Usage(`conflicting condition for ${l.path}:${l.line} ` +
        `(already requested as breakpoint${brk.cond ? ` '${brk.cond}'` : ''}): ` +
        `logpoint ${l.path}:${l.line}`);
    }
    const other = keptLogs.find((o) => o.path === l.path && o.line === l.line);
    if (other) {
      throw new Usage(`conflicting condition for ${l.path}:${l.line} ` +
        `(already requested as logpoint): logpoint ${l.path}:${l.line}`);
    }
    seenLogs.add(key);
    keptLogs.push(l);
  }
  cfg.logpoints = keptLogs;
}

function parseArgs(argv) {
  // argv === process.argv.slice(2); argv[0] === "session"
  const cfg = {
    kind: 'launch', dir: null, program: null, nodeBin: 'node',
    host: 'localhost', port: 9229, srcs: [], breaks: [], logpoints: [],
    wantExc: false, timeout: 20, programArgs: [],
    observedTarget: null, observedHint: '', breakRaws: {},
    // Opt-in multi-target: follow worker_threads via NodeWorker
    // (launch only; default is main-only, no flag needed to stay single).
    workers: false,
  };
  const rest = argv;
  let i = 0;
  const need = (flag) => {
    if (i >= rest.length) throw new Usage(`missing value for ${flag}`);
    return rest[i++];
  };
  if (rest[0] !== 'session') throw new Usage('first arg must be "session"');
  i = 1;
  // Break/logpoint specs resolve against --src roots, but flags may arrive
  // in any order (--break before --src). Collect raw specs first, resolve
  // after the full flag set is known — a bad path still fails fast here,
  // before any target runs.
  const pendingStops = []; // [kind, spec] in flag order
  while (i < rest.length) {
    const a = rest[i++];
    if (a === '--') { cfg.programArgs = rest.slice(i); break; }
    else if (a === '--kind') cfg.kind = need(a);
    else if (a === '--dir') cfg.dir = need(a);
    else if (a === '--program') cfg.program = path.resolve(need(a));
    else if (a === '--node') cfg.nodeBin = need(a);
    else if (a === '--host') cfg.host = need(a);
    else if (a === '--port') cfg.port = needInt(a, need(a));
    else if (a === '--src') cfg.srcs.push(path.resolve(need(a)));
    else if (a === '--break') pendingStops.push(['break', need(a)]);
    else if (a === '--logpoint') pendingStops.push(['logpoint', need(a)]);
    else if (a === '--watch') throw new Usage(`--watch has no CDP equivalent yet (Node): ${rest[i] || ''}`);
    else if (a === '--exit') throw new Usage(`--exit has no CDP equivalent yet (Node): ${rest[i] || ''}`);
    else if (a === '--timeout') cfg.timeout = needInt(a, need(a));
    else if (a === '--workers') cfg.workers = true;
    else if (a === '--observed-target') {
      const raw = need(a);
      try {
        const parsed = JSON.parse(raw);
        cfg.observedTarget = (parsed && typeof parsed === 'object') ? parsed : null;
      } catch (_) {
        cfg.observedTarget = null;
      }
    }
    else if (a === '--observed-hint') cfg.observedHint = need(a);
    else throw new Usage(`unknown arg: ${a}`);
  }
  for (const [kind, spec] of pendingStops) {
    if (kind === 'break') {
      const before = cfg.breaks.length;
      parseBreak(spec, cfg);
      if (cfg.breaks.length > before) {
        // Stored raw for remove/clear echo (dedup keeps first).
        const b = cfg.breaks[cfg.breaks.length - 1];
        const key = `${b.path}:${b.line}|${b.cond || ''}`;
        if (!(key in cfg.breakRaws)) cfg.breakRaws[key] = spec;
      }
    }
    else parseLogpoint(spec, cfg);
  }
  dedupeStartupBreaks(cfg);
  if (!cfg.dir) throw new Usage('missing --dir');
  if (cfg.kind !== 'launch' && cfg.kind !== 'attach') {
    throw new Usage(`--kind must be launch|attach (got ${cfg.kind})`);
  }
  if (cfg.kind === 'launch' && !cfg.program) throw new Usage('launch needs --program');
  if (cfg.kind === 'launch') {
    // Fail fast with node's OWN message. Load/parse failures die silently
    // under an attached inspector (measured: no error text, no exit — just
    // a destroyed context), so surface them before spawning anything.
    if (!fs.existsSync(cfg.program) || !fs.statSync(cfg.program).isFile()) {
      throw new Usage(`no such file: ${cfg.program}`);
    }
    // The binary itself must run (clean BridgeErr, not a silent spawn
    // failure or a misleading syntax message): --check proves it for plain
    // JS, --version for TS where --check false-positives on type-stripping.
    if (/\.[mc]?ts$/.test(cfg.program)) {
      // node --check does not apply type-stripping, so it false-positives
      // on valid TypeScript — skip pre-validation there (runtime failures
      // still surface via logs/exit, just without the precise message).
      // Still prove the --node binary runs so a bad --node fails cleanly.
      let probe;
      try {
        probe = spawnSync(cfg.nodeBin, ['--version'], { encoding: 'utf-8' });
      } catch (e) {
        throw new BridgeErr(`cannot run ${cfg.nodeBin}: ${(e && e.message) || e}`);
      }
      if (probe.error || probe.status !== 0) {
        throw new BridgeErr(`cannot run ${cfg.nodeBin}: ` +
          `${(probe.error && probe.error.message) || (probe.stderr || '').trim() || `exit ${probe.status}`}`);
      }
    } else {
      let check;
      try {
        check = spawnSync(cfg.nodeBin, ['--check', cfg.program], { encoding: 'utf-8' });
      } catch (e) {
        throw new BridgeErr(`cannot run ${cfg.nodeBin} --check: ${(e && e.message) || e}`);
      }
      if (check.error) {
        throw new BridgeErr(`cannot run ${cfg.nodeBin}: ${check.error.message}`);
      }
      if (check.status !== 0) {
        throw new Usage(`cannot debug (syntax error?):\n${(check.stderr || '').trim()}`);
      }
    }
  }
  if (!Number.isFinite(cfg.timeout) || cfg.timeout <= 0 || cfg.timeout > 3600) throw new Usage('timeout must be between 0 and 3600 seconds');
  return cfg;
}

// ---------------------------------------------------------------- values

function truncStr(s, limit = MAX_STRING) {
  if (s.length <= limit) return s;
  return `${s.slice(0, limit)}… (+${s.length - limit} more chars)`;
}

// ---------------------------------------------------------------- CDP conn

function loadWs() {
  try {
    return require('ws');
  } catch (e) {
    throw new BridgeErr(`missing 'ws' package (${e.message}) — reinstall via: npm install ws`);
  }
}

/** Minimal CDP client: id-matched requests plus an event handler. */

// ---------------------------------------------------------------- session


// Session ownership: written first thing at startup, verified on every
// serve iteration and pump tick. If the owner deleted our dir (rm -rf
// instead of close) or respawned a new bridge under the same name, our
// nonce no longer matches and we quit quietly instead of orphaning.
// (Legit flows always close — which exits us — before removing the dir,
// so a mismatch unambiguously means abandonment.)
const OWNER_NONCE = `${process.pid}-${Date.now()}-${Math.floor(Math.random() * 1e9)}`;
function writeOwner(dir) {
  writeFile(path.join(dir, 'owner.json'), JSON.stringify({ pid: process.pid, nonce: OWNER_NONCE }));
}
function amOwner(dir) {
  try {
    const raw = fs.readFileSync(path.join(dir, 'owner.json'), 'utf-8');
    return JSON.parse(raw).nonce === OWNER_NONCE;
  } catch (_) {
    return false;
  }
}

function writeFile(p, content) {
  // Atomic same-dir publish: unique temp (create-new) + rename, so a
  // concurrent `status` read never sees a torn session.json. Best effort
  // (callers treat state files as advisory), but a temp is never left
  // behind. Plain log appends stay append-only — only full rewrites (state
  // files, log-ring trims) come through here.
  try {
    const dir = path.dirname(p);
    for (let i = 0; i < 8; i++) {
      const tmp = path.join(dir,
        `.tmp-${process.pid}-${Date.now()}-${Math.floor(Math.random() * 1e9)}`);
      try {
        fs.writeFileSync(tmp, content, { flag: 'wx' });
      } catch (e) {
        if (e && e.code === 'EEXIST') continue;
        return;
      }
      try {
        fs.renameSync(tmp, p);
      } catch (_) {
        try {
          fs.unlinkSync(tmp);
        } catch (_) { /* already gone */ }
      }
      return;
    }
  } catch (_) { /* best effort */ }
}

// Sanitized unexpected-crash payload: class + message + short stack tail,
// ~2KB cap. Never env/secrets — only the failure itself. Known Usage /
// BridgeErr messages bypass this (exact text on the error.json path).
const MAX_ERROR_CHARS = 2048;
function sanitizeUnexpected(e) {
  const kind = (e && e.constructor && e.constructor.name) || 'Error';
  let body = `${kind}: ${(e && e.message) || e}`;
  try {
    const frames = String((e && e.stack) || '').split('\n').slice(1, 7);
    if (frames.length > 0) body += '\n' + frames.join('\n');
  } catch (_) { /* message alone still helps */ }
  body = body.trim();
  if (body.length > MAX_ERROR_CHARS) body = body.slice(0, MAX_ERROR_CHARS - 1) + '…';
  return `internal: ${body}`;
}

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

function escapeRegex(s) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

class Session {
  constructor(cfg) {
    this.cfg = cfg;
    this.child = null;      // launched target process (null on attach)
    this.cdp = null;
    this.defaultContextId = null;
    this.scripts = new Map(); // scriptId -> url
    this.urls = new Map();    // url -> scriptId
    this.logpointIds = new Map(); // breakpointId -> template
    this.breakIdToRec = new Map(); // breakpointId -> {rec, line} for resolve upgrades
    this.breakKeys = new Map(); // `path:line|cond` -> breakpointId (null when rejected)
    this.breakRecByKey = new Map(); // `path:line|cond` -> live break rec (incl. rejected)
    this.breakRaws = new Map(Object.entries((cfg && cfg.breakRaws) || {})); // key -> stored raw
    this.shadowedLogs = []; // {path, line, template} startup logpoints shadowed by a break
    this.paused = null;     // {frames, stopInfo} of the current stop
    this.awaitingStep = false;
    this.exited = false;
    this.closing = false;
    this.lastTop = null;
    this.lastFunc = null;
    this.lastChanged = '[]';
    this.stopInfo = null;
    this.outputTail = '';
    this.logCount = 0;
    this.logDropped = 0; // lifetime lines evicted by the log ring
    this.stopStates = []; // arm-time records served by `breaks`
    this.sessionPort = 0; // our TCP port (set in main, for republishing)
    this.lastStop = null; // {file,line,method} of the latest stop
    // -- worker targets (M-T/M4): main keeps its own fields above;
    // workers live in workerTable by opaque id (worker:<sessionId>).
    this.workersEnabled = !!(cfg && cfg.workers);
    this.workerTable = new Map(); // id -> worker (live + ignored)
    this.workerOrder = [];        // creation order for roster listing
    this.seenWorkerIds = new Set(); // every id ever issued (no reuse)
    this.exitedWorkers = [];      // bounded last-known entries (max 16)
    this.ignoredWorkers = 0;      // released (over-budget) workers, lifetime
    this.droppedWorkerExited = 0; // exited-history evictions, lifetime
    this.workerPending = new Map(); // `${sessionId}:${cdpId}` -> {resolve,reject}
    this.stopSeq = 0;             // monotonic park clock for auto-select
    this.mainSeq = 0;             // seq of the main park (0 = never parked)
    this.lastParkTarget = 'main'; // which target the last pump parked
    // -- stop diagnostics (UX batch): session-monotonic stop id plus the
    // previous park for same-location/same-thread diagnosis.
    this.stopDiagSeq = 0; // session-monotonic stop id
    this.prevPark = null; // previous park {target,file,line,threadId,atMs}
    this.stopReason = null; // reason of the current park
    this.stopHitBps = null; // native hitBreakpoint ids of the current park
    this.parkedAtMs = 0; // wall clock ms of the current park
    this.lastDiag = null; // {target,stopId,sameLocation,sameThread,elapsedMs,atMs}
    this.serving = 'main';        // target id of the in-flight command
    this.pendingTarget = null;    // resume-wait owner for error attribution
    // -- M5 concurrency: outstanding resume ops by target (tid -> cmd);
    // live reads bypass everything below and serve published state.
    this.outstanding = new Map();
    this.activeConns = 0;         // live connection handlers (bounded)
    this._swapTail = null;        // swap/tracking-field mutex chain
    this.mainDead = false;        // main session over; workers may live on
    this.sender = null;           // per-target CDP sender (null = main cdp)
    this.nodeWorkerEnabled = false;
  }

  // -- multi-target helpers (M-T/M4)

  /** Transport for the served target: main speaks raw CDP, workers speak
   *  the NodeWorker wrapper (flat sessionId params would hit the wrong
   *  session — M0). */
  req(method, params = {}, timeoutMs = 30000) {
    if (this.sender) return this.sender(method, params, timeoutMs);
    return this.cdp.request(method, params, timeoutMs);
  }

  liveWorkers() {
    const out = [];
    for (const id of this.workerOrder) {
      const w = this.workerTable.get(id);
      if (w && !w.exited) out.push(w);
    }
    return out;
  }

  activeWorkers() {
    return this.liveWorkers().filter((w) => w.state === 'running' || w.state === 'stopped');
  }

  resolveTarget(req) {
    // Acceptance-time auto-selection rides in req._autoTarget (stamped by
    // dispatch) so concurrent handlers agree; it is still liveness-checked.
    const want = req && typeof req.target === 'string' ? req.target
      : (req && typeof req._autoTarget === 'string' ? req._autoTarget : null);
    if (want !== null) {
      if (want === 'main') {
        if (this.mainDead || this.exited) {
          throw new BridgeErr('target main has exited — close this session');
        }
        return 'main';
      }
      const w = this.workerTable.get(want);
      if (!w) {
        if (this.exitedWorkers.some((e) => e.id === want)) {
          throw new BridgeErr(`target ${want} has exited — close this session`);
        }
        throw new BridgeErr(`unknown target: ${want}`);
      }
      if (w.exited || w.state === 'exited') {
        throw new BridgeErr(`target ${want} has exited — close this session`);
      }
      if (w.state === 'ignored') {
        throw new BridgeErr(`target ${want} was released (over budget)`);
      }
      return want;
    }
    let best = 'main';
    let bestSeq = (this.paused && !this.mainDead) ? this.mainSeq : 0;
    if (this.mainDead) bestSeq = -1;
    for (const w of this.liveWorkers()) {
      if (w.paused && w.stopSeq > bestSeq) {
        best = w.id;
        bestSeq = w.stopSeq;
      }
    }
    if (best === 'main' && (this.exited || this.mainDead)) {
      throw new BridgeErr('target VM has exited — close this session');
    }
    return best;
  }

  /** Serialize every section that borrows shared target fields
   *  (withTarget swaps, pause-handler tracking/sender borrowing). Only
   *  bounded CDP round-trips run inside — never pump waits — so resume
   *  waits on different targets stay parallel. NOT reentrant. */
  _lockSwap() {
    let release;
    const willLock = new Promise((resolve) => {
      release = resolve;
    });
    const waitsFor = this._swapTail || Promise.resolve();
    this._swapTail = waitsFor.then(() => willLock);
    return waitsFor.then(() => release);
  }

  async _swapRun(fn) {
    const release = await this._lockSwap();
    try {
      return await fn();
    } finally {
      release();
    }
  }

  /** M5 immediate busy rejection: a second resume or mutation on the SAME
   *  target never silently queues; a global breakpoint mutation conflicts
   *  with ANY outstanding resume; eval is exclusive (it can mutate) and
   *  busy-rejects on its target even when parked frames exist. Live reads
   *  and frame-bound context/vars/stack never busy-reject (the latter fail
   *  fast via requireStopped once the resume publishes running). */
  busyError(cmd, tid) {
    const out = this.outstanding;
    // wait never resumes but still occupies its target's slot (a rival
    // resume would steal the stop it long-polls for); capture resumes at
    // the end, so it occupies the slot throughout.
    if (RESUME_CMDS.has(cmd) || WAIT_CMDS.has(cmd) || CAPTURE_CMDS.has(cmd)) {
      if (tid !== null && out.has(tid)) {
        return `busy: ${out.get(tid)} outstanding for ${tid}`;
      }
      return null;
    }
    if (MUTATION_CMDS.has(cmd)) {
      if (tid !== null) {
        // Target-scoped mutation conflicts with that target only.
        return out.has(tid) ? `busy: ${out.get(tid)} outstanding for ${tid}` : null;
      }
      // Global mutation conflicts with any outstanding resume.
      if (out.size > 0) {
        const first = [...out.keys()].sort()[0];
        return `busy: ${out.get(first)} outstanding for ${first}`;
      }
      return null;
    }
    if (cmd === 'eval') {
      if (tid !== null && out.has(tid)) {
        return `busy: ${out.get(tid)} outstanding for ${tid}`;
      }
      return null;
    }
    return null;
  }

  /** Target scope of a breakpoint mutation: explicit non-main target
   *  (target-scoped, conflicts that target only), else null (global). */
  mutationTid(req) {
    const scope = req && typeof req.target === 'string' ? req.target : null;
    if (scope !== null && scope !== 'main') return scope;
    return null;
  }

  clearOutstanding(cmd, tid) {
    if (this.outstanding.get(tid) === cmd) this.outstanding.delete(tid);
  }

  saveMain() {
    return {
      paused: this.paused, stopInfo: this.stopInfo,
      cachedLocals: this.cachedLocals, lastChanged: this.lastChanged,
      lastTop: this.lastTop, lastFunc: this.lastFunc,
      awaitingStep: this.awaitingStep, scripts: this.scripts, urls: this.urls,
    };
  }

  loadWorker(w) {
    this.paused = w.paused;
    this.stopInfo = w.stopInfo;
    this.cachedLocals = w.cachedLocals;
    this.lastChanged = w.lastChanged;
    this.lastTop = w.lastTop;
    this.lastFunc = w.lastFunc;
    this.awaitingStep = w.awaitingStep;
    this.scripts = w.scripts;
    this.urls = w.urls;
    this.sender = (method, params, timeoutMs) =>
      this.workerSend(w, method, params, timeoutMs);
    this.serving = w.id;
  }

  storeWorker(w) {
    w.paused = this.paused;
    w.stopInfo = this.stopInfo;
    w.cachedLocals = this.cachedLocals;
    w.lastChanged = this.lastChanged;
    w.lastTop = this.lastTop;
    w.lastFunc = this.lastFunc;
    w.awaitingStep = this.awaitingStep;
    w.scripts = this.scripts;
    w.urls = this.urls;
  }

  loadMain(saved) {
    this.paused = saved.paused;
    this.stopInfo = saved.stopInfo;
    this.cachedLocals = saved.cachedLocals;
    this.lastChanged = saved.lastChanged;
    this.lastTop = saved.lastTop;
    this.lastFunc = saved.lastFunc;
    this.awaitingStep = saved.awaitingStep;
    this.scripts = saved.scripts;
    this.urls = saved.urls;
    this.sender = null;
    this.serving = 'main';
  }

  /** Run fn against one target's park/transport using the main code paths.
   *  The whole swapped section holds the swap mutex (M5: concurrent
   *  handlers must never swap shared park fields under each other, and
   *  pause-event borrowing in onPaused/onWorkerPaused takes the same
   *  mutex). Callers must not nest withTarget (the mutex is not
   *  reentrant); resume pumps run OUTSIDE the scope, so waits stay
   *  parallel across targets. */
  async withTarget(tid, fn) {
    if (tid === 'main' || !this.workerTable.has(tid)) {
      return this._swapRun(fn);
    }
    return this._swapRun(async () => {
      const w = this.workerTable.get(tid);
      const saved = this.saveMain();
      this.loadWorker(w);
      try {
        return await fn();
      } finally {
        // The worker may have exited mid-command (table entry moved to
        // history): only store back while it is still live.
        if (this.workerTable.get(tid) === w) this.storeWorker(w);
        this.loadMain(saved);
      }
    });
  }

  withStamp(resp, tid) {
    if (resp && typeof resp === 'object' && !('target' in resp)) {
      resp.target = tid;
    }
    return resp;
  }

  targetEntry(tid) {
    if (tid === 'main') {
      return {
        id: 'main', kind: 'main', pid: null,
        state: (this.exited || this.mainDead) ? 'exited' : (this.paused ? 'stopped' : 'running'),
        lastStop: this.lastStop,
        observed: (this.cfg && this.cfg.observedTarget) || null,
        scope: 'global',
      };
    }
    const w = this.workerTable.get(tid);
    return {
      id: w.id, kind: 'worker', pid: null,
      state: w.state, lastStop: w.lastStop,
      observed: w.observed,
      scope: w.targetRaws.size > 0 ? 'target' : 'inherited',
    };
  }

  cmdTargets() {
    const entries = [this.targetEntry('main')];
    for (const id of this.workerOrder) {
      if (this.workerTable.has(id)) entries.push(this.targetEntry(id));
    }
    for (const e of this.exitedWorkers) entries.push(e);
    return this.withStamp({
      ok: true, targets: entries,
      selected: this.resolveTarget({}),
      ignored: this.ignoredWorkers,
      droppedExited: this.droppedWorkerExited,
    }, 'main');
  }

  noteWorkerExit(tid) {
    const w = this.workerTable.get(tid);
    if (!w) return;
    this.workerTable.delete(tid);
    for (const key of [...this.workerPending.keys()]) {
      if (key.startsWith(`${w.sessionId}:`)) {
        const p = this.workerPending.get(key);
        this.workerPending.delete(key);
        try {
          p.reject(new BridgeErr('worker session ended'));
        } catch (_) { /* already settled */ }
      }
    }
    w.exited = true;
    w.state = 'exited';
    w.paused = null;
    this.exitedWorkers.push({
      id: w.id, kind: 'worker', pid: null,
      state: 'exited', lastStop: w.lastStop,
      observed: w.observed,
      scope: w.targetRaws.size > 0 ? 'target' : 'inherited',
    });
    while (this.exitedWorkers.length > 16) {
      this.exitedWorkers.shift();
      this.droppedWorkerExited += 1;
    }
  }

  evictOldIgnored() {
    const ignored = this.workerOrder.filter((id) => {
      const w = this.workerTable.get(id);
      return w && w.state === 'ignored';
    });
    while (ignored.length > 16) {
      const old = ignored.shift();
      this.workerTable.delete(old);
      const i = this.workerOrder.indexOf(old);
      if (i >= 0) this.workerOrder.splice(i, 1);
    }
  }

  // -- target lifecycle

  async startTarget() {
    const stderrBuf = [];
    let wsUrl = null;
    this.spawnError = null;
    this.child = spawn(this.cfg.nodeBin,
      [`--inspect-brk=127.0.0.1:0`, this.cfg.program, ...this.cfg.programArgs],
      { stdio: ['ignore', 'pipe', 'pipe'] });
    // Error/close listeners attach IMMEDIATELY after spawn: a bad --node
    // binary emits 'error' (ENOENT) with no 'exit', which without a listener
    // rethrows and crashes the daemon — and would otherwise hang the
    // handshake loop until its 20s deadline with a misleading message.
    this.child.once('error', (e) => {
      this.spawnError = e;
    });
    this.child.stdout.on('data', (d) => this.onTargetOutput(d.toString()));
    this.child.stderr.on('data', (d) => {
      // Before the inspector URL appears, stderr is startup noise (parsed
      // for the ws:// URL). After that it is program output (console.error
      // etc.) and belongs in the logs like stdout.
      if (wsUrl) {
        this.onTargetOutput(d.toString());
        return;
      }
      stderrBuf.push(d.toString());
      if (stderrBuf.join('').length > 8000) stderrBuf.splice(0, 1);
    });
    const dead = new Promise((resolve) => {
      // A failed spawn emits 'error' (+ 'close') but never 'exit': listen
      // to all three so a bad --node surfaces now, not at the deadline.
      this.child.once('exit', (code) => resolve(code));
      this.child.once('close', (code) => resolve(code));
      this.child.once('error', () => resolve('spawn-error'));
    });
    const deadline = Date.now() + 20000;
    for (;;) {
      if (this.spawnError) {
        throw new BridgeErr(`cannot run ${this.cfg.nodeBin}: ` +
          `${(this.spawnError && this.spawnError.message) || this.spawnError}`);
      }
      const text = stderrBuf.join('');
      const m = text.match(/Debugger listening on (ws:\/\/\S+)/);
      if (m) {
        wsUrl = m[1];
        // Same-chunk remainder (e.g. an instant "Cannot find module") is
        // program output too — flush it instead of dropping it with the buf.
        this.onTargetOutput(text.slice(m.index + m[0].length));
        return wsUrl;
      }
      const code = await Promise.race([dead, sleep(100).then(() => null)]);
      if (this.spawnError) {
        throw new BridgeErr(`cannot run ${this.cfg.nodeBin}: ` +
          `${(this.spawnError && this.spawnError.message) || this.spawnError}`);
      }
      if (code !== null && code !== undefined) {
        throw new BridgeErr(`target exited during startup (code ${code}): ${text.trim().slice(-500)}`);
      }
      if (Date.now() > deadline) {
        try {
          this.child.kill('SIGKILL');
        } catch (_) { /* best effort */ }
        throw new BridgeErr(`target did not expose an inspector in 20s: ${text.trim().slice(-300)}`);
      }
    }
  }

  async discoverAttach() {
    const url = `http://${this.cfg.host}:${this.cfg.port}/json/list`;
    const body = await new Promise((resolve, reject) => {
      const req = http.get(url, { timeout: 10000 }, (res) => {
        let raw = '';
        res.on('data', (d) => { raw += d; });
        res.on('end', () => resolve(raw));
      });
      req.on('timeout', () => {
        req.destroy();
        reject(new Error('timed out'));
      });
      req.on('error', reject);
    }).catch((e) => {
      const detail = e.message || e.code || 'connection refused';
      throw new BridgeErr(`attach failed (${this.cfg.host}:${this.cfg.port}): ${detail} — ` +
        `is the target started with node --inspect=${this.cfg.port} ?`);
    });
    let targets;
    try {
      targets = JSON.parse(body);
    } catch (_) {
      throw new BridgeErr(`attach failed: ${url} did not return target list`);
    }
    const node = (targets || []).find((t) => t.type === 'node' && t.webSocketDebuggerUrl)
      || (targets || []).find((t) => t.webSocketDebuggerUrl);
    if (!node) throw new BridgeErr(`attach failed: no debuggable target at ${url}`);
    return node.webSocketDebuggerUrl;
  }

  async connect(wsUrl) {
    const WebSocket = loadWs();
    this.mainWsUrl = wsUrl;
    const ws = new WebSocket(wsUrl, { maxPayload: 256 * 1024 * 1024 });
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('connect timeout')), 20000);
      ws.once('open', () => {
        clearTimeout(timer);
        resolve();
      });
      ws.once('error', (e) => {
        clearTimeout(timer);
        reject(e);
      });
    }).catch((e) => {
      throw new BridgeErr(`CDP connect failed (${wsUrl}): ${e.message}`);
    });
    this.cdp = new CdpConn(ws, () => {
      // Attach target died (launch deaths surface via child 'close', but
      // attach has no child): mark exit now so pump fails fast and the
      // session file stops lying about being parked.
      if (!this.closing) {
        this.exited = true;
        this.paused = null;
        this.publishState(false);
      }
    });
    this.cdp.onEvent = (msg) => {
      this.handleEvent(msg).catch((e) => {
        process.stderr.write(`warn: event handler: ${(e && e.message) || e}\n`);
      });
    };
  }

  async handshake() {
    let wsUrl;
    if (this.cfg.kind === 'launch') {
      wsUrl = await this.startTarget();
    } else {
      wsUrl = await this.discoverAttach();
    }
    await this.connect(wsUrl);
    await this.cdp.request('Debugger.enable');
    // Runtime.enable is not for evaluation — it arms the
    // executionContextDestroyed event, the ONLY signal that a launched
    // script ended (an attached inspector keeps the process alive after).
    await this.cdp.request('Runtime.enable');
    if (this.cfg.workers && this.cfg.kind === 'launch') {
      // Worker following is opt-in and must precede the main resume:
      // every worker then starts paused for the debugger (M0: bayraksız
      // GO, waitForDebuggerOnStart:true). Attach never follows workers.
      await this.cdp.request('NodeWorker.enable', { waitForDebuggerOnStart: true });
      this.nodeWorkerEnabled = true;
    }
    // Breakpoints BEFORE runIfWaitingForDebugger (spike-proven: later
    // installs sit pending and the entry pause swallows the first stop).
    await this.armBreakpoints();
    if (this.cfg.wantExc) {
      await this.cdp.request('Debugger.setPauseOnExceptions', { state: 'uncaught' });
    }
    if (this.cfg.kind === 'launch') {
      await this.cdp.request('Runtime.runIfWaitingForDebugger');
    }
  }

  dispSpec(b, kind) {
    // Display form for `breaks`: workspace-relative path + line (+ cond),
    // template for logpoints. Mirrors pybridge's reconstructed specs.
    let s = `${this.relFile(b.path)}:${b.line}`;
    if (kind === 'break' && b.cond) s += `|${b.cond}`;
    return s;
  }

  async armBreakpoints() {
    // Belt-and-braces: parseArgs already folded startup specs, but the
    // fail-fast must hold before ANY CDP setBreakpointByUrl traffic even if
    // the config was built another way. Identical repeats collapse here;
    // differing cond/logpoint collisions throw before the first request.
    dedupeStartupBreaks(this.cfg);
    // Group per file: CDP setBreakpointByUrl is one line per call, but a
    // logpoint and a line break on the same line would collide — keep the
    // real break (logpoint still fires? no: one breakpoint per line wins).
    // Keep it simple and explicit: real breaks win, logpoint on the same
    // line is reported and skipped.
    // Every requested stop lands in this.stopStates (served by `breaks`)
    // — verification used to live only in stderr, invisible after
    // compaction.
    const byLine = new Map(); // `${path}:${line}` -> {break, logpoint}
    for (const b of this.cfg.breaks) {
      byLine.set(`${b.path}:${b.line}`, { ...(byLine.get(`${b.path}:${b.line}`) || {}), brk: b });
    }
    for (const l of this.cfg.logpoints) {
      const key = `${l.path}:${l.line}`;
      if (byLine.has(key) && byLine.get(key).brk) {
        const msg = `logpoint shadowed by breakpoint: ${l.path}:${l.line}`;
        process.stderr.write(`warn: ${msg}\n`);
        this.stopStates.push({
          spec: this.dispSpec(l, 'logpoint'), kind: 'logpoint',
          state: 'shadowed', detail: msg, hits: 0,
        });
        // Retained for remove-time re-arm (the break wins; no plant exists).
        this.shadowedLogs.push({ path: l.path, line: l.line, template: l.template });
        continue;
      }
      byLine.set(key, { ...(byLine.get(key) || {}), log: l });
    }
    for (const [, item] of byLine) {
      const spec = item.brk || item.log;
      const kind = item.brk ? 'break' : 'logpoint';
      const fileUrl = pathToFileURL(spec.path).href;
      const params = {
        urlRegex: `^${escapeRegex(fileUrl)}$`,
        lineNumber: spec.line - 1,
      };
      if (item.brk && item.brk.cond) params.condition = item.brk.cond;
      const res = await this.cdp.request('Debugger.setBreakpointByUrl', params);
      const bpId = res.breakpointId;
      const rec = { spec: this.dispSpec(spec, kind), kind, hits: 0 };
      if (kind === 'logpoint') rec.detail = spec.template;
      if (kind === 'break') {
        this.breakKeys.set(`${spec.path}:${spec.line}|${spec.cond || ''}`, bpId || null);
        this.breakRecByKey.set(`${spec.path}:${spec.line}|${spec.cond || ''}`, rec);
      }
      if (!bpId) {
        const msg = `breakpoint rejected: ${spec.path}:${spec.line}`;
        process.stderr.write(`warn: ${msg}\n`);
        rec.state = 'rejected';
        rec.detail = kind === 'logpoint' ? `${spec.template} (${msg})` : msg;
        rec.hits = 0;
        this.stopStates.push(rec);
        continue;
      }
      if (item.log && !item.brk) {
        this.logpointIds.set(bpId, item.log.template);
      }
      // breakpointResolved upgrades this record when V8 binds it (arm-time
      // locations:[] is normal — scripts parse after we install).
      this.breakIdToRec.set(bpId, { rec, line: spec.line });
      const locs = res.locations || [];
      if (locs.length === 0 && this.urls.has(fileUrl)) {
        const msg = `breakpoint unverified (pending): ${spec.path}:${spec.line}`;
        process.stderr.write(`warn: ${msg}\n`);
      }
      const slidLoc = (locs.map((l) => l.lineNumber + 1).find((n) => n !== spec.line));
      const slidTo = (slidLoc === undefined) ? null : slidLoc;
      if (slidTo !== null) {
        // V8 slides breakpoints off non-executable lines (e.g. `}`) to the
        // next statement — possibly another scope where conditions/holes
        // stop resolving. Say so loudly instead of silently debugging the
        // wrong line.
        process.stderr.write(
          `warn: breakpoint slid: ${spec.path}:${spec.line} -> ${slidTo}\n`);
        rec.state = 'slid';
        rec.detail = kind === 'logpoint'
          ? `${spec.template} (slid to line ${slidTo})`
          : `slid to line ${slidTo}`;
      } else {
        rec.state = locs.length > 0 ? 'verified' : 'pending';
        if (rec.state === 'pending' && kind === 'break') {
          rec.detail = 'no locations yet (script not parsed or line not executable)';
        }
      }
      this.stopStates.push(rec);
    }
    if (this.cfg.wantExc) {
      this.stopStates.push({ spec: 'exc', kind: 'exc', state: 'armed', hits: 0 });
    }
  }

  /** Attribute a pause to the records it hit (served as `hits` by
   *  `breaks`). Counts exactly what the adapter reports in hitBreakpoints —
   *  step landings normally carry none, so they don't inflate counters.
   *  recList defaults to main; workers pass their own (plus their exc). */
  countHits(p, recList = this.stopStates) {
    for (const id of p.hitBreakpoints || []) {
      const entry = this.breakIdToRec.get(id);
      if (entry && typeof entry.rec.hits === 'number') entry.rec.hits += 1;
    }
    if (p.reason === 'exception') {
      for (const rec of recList) {
        if (rec.kind === 'exc' && typeof rec.hits === 'number') rec.hits += 1;
      }
    }
  }

  // -- events (always live: logpoints auto-fire even between commands)

  async handleEvent(msg) {
    if (msg.method === 'NodeWorker.attachedToWorker') {
      await this.acceptWorker(msg.params || {});
      return;
    }
    if (msg.method === 'NodeWorker.detachedFromWorker') {
      const sid = (msg.params || {}).sessionId;
      // noteWorkerExit retires whatever the table knows — tracked workers
      // AND released (ignored) ones, so a kicked worker that runs out is
      // observable as exited rather than stuck as ignored forever.
      // Unknown sessions (never seen, e.g. evicted) stay untracked.
      if (sid) this.noteWorkerExit(`worker:${sid}`);
      return;
    }
    if (msg.method === 'NodeWorker.receivedMessageFromWorker') {
      await this.routeWorkerMessage(msg.params || {});
      return;
    }
    if (msg.method === 'Debugger.scriptParsed') {
      const { scriptId, url } = msg.params || {};
      if (scriptId) {
        this.scripts.set(scriptId, url || '');
        if (url) this.urls.set(url, scriptId);
      }
      return;
    }
    if (msg.method === 'Debugger.breakpointResolved') {
      // V8 bound a pending breakpoint: promote the `breaks` record from
      // provisional pending to verified (or slid, if it landed elsewhere).
      this.upgradeBreakpoint(msg.params || {});
      return;
    }
    if (msg.method === 'Runtime.executionContextCreated') {
      const ctx = (msg.params && msg.params.context) || {};
      if (ctx.auxData && ctx.auxData.isDefault) this.defaultContextId = ctx.id;
      return;
    }
    if (msg.method === 'Runtime.executionContextDestroyed') {
      // Main script ended (process lingers while the inspector is
      // attached). Surface as exited so stops/threads behave like pybridge.
      if ((msg.params || {}).executionContextId === this.defaultContextId) {
        this.markExited();
      }
      return;
    }
    if (msg.method === 'Debugger.paused') {
      // Pause processing borrows shared target/tracking fields: run it
      // under the swap mutex (M5) so a concurrent connection handler
      // swapping the same fields cannot interleave mid-await.
      await this._chainPause(() => this._swapRun(() => this.onPaused(msg.params || {})));
      return;
    }
    if (msg.method === 'Debugger.resumed') {
      return;
    }
  }

  /** Serialize pause handling across main + workers. onPaused and
   *  onWorkerPaused share `sender` and change-tracking fields across
   *  awaits; concurrent pauses would clobber each other's frames/locals.
   *  Each link is bounded (CDP calls carry timeouts; no link waits on the
   *  pump or CLI), so the chain drains instead of hanging head-of-line.
   *  First-park-per-target is preserved: a link parks synchronously in
   *  its own prefix, so a duplicate landing behind it still sees the park.
   *  Rejections never break the chain (callers still observe them). */
  _chainPause(fn) {
    const prev = this._pauseChain || Promise.resolve();
    const run = prev.then(fn);
    this._pauseChain = run.catch(() => {});
    return run;
  }

  async onPaused(p) {
    if (this.closing || this.exited || this.paused) {
      // Single target: a second pause cannot arrive while one is held (the
      // target is frozen). If it ever does, hold the first stop; the next
      // resume flushes the rest. The park below is set SYNCHRONOUSLY (before
      // the first await), so even a fire-and-forget duplicate landing in the
      // same tick sees it and never clobbers the current stop.
      return;
    }
    const hits = p.hitBreakpoints || [];
    const frames = this.userFrames(p.callFrames || []);
    const logHits = hits.filter((id) => this.logpointIds.has(id));
    const realHits = hits.filter((id) => !this.logpointIds.has(id));
    this.countHits(p);
    if (p.reason === 'exception') {
      this.stopInfo = this.excInfo(p.data);
      // Park first, synchronously — trackChanges awaits must never strand
      // a CDP pause with a running state when they throw.
      this.paused = { frames, stopInfo: this.stopInfo };
      this.stopSeq += 1;
      this.mainSeq = this.stopSeq;
      this.lastParkTarget = 'main';
      this.notePark('main', p, this.locationJsonFor(frames, this.scripts));
      this.publishState(true);
      try {
        for (const id of logHits) {
          await this.fireLogpoint(id, frames);
        }
        await this.trackChanges(frames);
      } catch (_) {
        // Still parked and published; change detail just goes quiet.
        if (!this.cachedLocals) this.cachedLocals = [];
        this.lastChanged = '[]';
      }
      this.publishState(true);
      return;
    }
    if (realHits.length > 0 || this.awaitingStep) {
      this.awaitingStep = false;
      this.stopInfo = null;
      // Park first, synchronously — see above.
      this.paused = { frames, stopInfo: null };
      this.stopSeq += 1;
      this.mainSeq = this.stopSeq;
      this.lastParkTarget = 'main';
      this.notePark('main', p, this.locationJsonFor(frames, this.scripts));
      this.publishState(true);
      try {
        for (const id of logHits) {
          await this.fireLogpoint(id, frames);
        }
        await this.trackChanges(frames);
      } catch (_) {
        if (!this.cachedLocals) this.cachedLocals = [];
        this.lastChanged = '[]';
      }
      this.publishState(true);
      return;
    }
    if (logHits.length > 0) {
      for (const id of logHits) {
        try {
          await this.fireLogpoint(id, frames);
        } catch (_) { /* template holes already degrade to '?' */ }
      }
      await this.cdp.request('Debugger.resume').catch(() => {});
      return;
    }
    // Entry pause (--inspect-brk) or stray instrumentation pause: resume.
    await this.cdp.request('Debugger.resume').catch(() => {});
  }

  /** Promote a pending `breaks` record when V8 binds it (shared by main
   *  and worker sessions: breakpointIds live in one map, records are
   *  per-target). */
  upgradeBreakpoint(p) {
    const entry = this.breakIdToRec.get(p.breakpointId);
    if (entry && entry.rec.state === 'pending') {
      const at = p.location ? p.location.lineNumber + 1 : null;
      if (at !== null && at !== entry.line) {
        entry.rec.state = 'slid';
        entry.rec.detail = entry.rec.kind === 'logpoint' && entry.rec.detail
          ? `${entry.rec.detail} (slid to line ${at})`
          : `slid to line ${at}`;
      } else {
        entry.rec.state = 'verified';
        if (entry.rec.kind === 'break') delete entry.rec.detail;
      }
    }
  }

  // -- worker transport (M4: NodeWorker wrapper, never flat sessionId)

  /** One wrapped CDP round-trip to a worker. Responses arrive as
   *  receivedMessageFromWorker events (not direct replies): matched here
   *  by per-worker sequence numbers. */
  workerSend(w, method, params = {}, timeoutMs = 5000) {
    const id = ++w.seq;
    const key = `${w.sessionId}:${id}`;
    const message = JSON.stringify({ id, method, params });
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        if (this.workerPending.delete(key)) {
          reject(new BridgeErr(`CDP ${method} timed out after ${Math.round(timeoutMs / 1000)}s`));
        }
      }, timeoutMs);
      this.workerPending.set(key, {
        resolve: (result) => {
          clearTimeout(timer);
          resolve(result);
        },
        reject: (e) => {
          clearTimeout(timer);
          reject(e);
        },
      });
      this.cdp.request('NodeWorker.sendMessageToWorker',
        { sessionId: w.sessionId, message }, timeoutMs).catch((e) => {
        if (this.workerPending.delete(key)) {
          clearTimeout(timer);
          reject(e);
        }
      });
    });
  }

  /** Release kick for untracked/released workers: BOTH the wrapped
   *  resume and the run gate, fire-and-forget (release paths never wait).
   *  A worker stuck in waitForDebugger needs the gate lifted; a
   *  breakpoint-paused one needs the resume — sending both covers either
   *  state, so a released worker can never hang the app. This is the ONE
   *  release path (over-budget attach and later auto-kicks share it). */
  kickWorkerFree(sessionId) {
    let n = (Date.now() % 100000) + 1;
    for (const method of ['Debugger.resume', 'Runtime.runIfWaitingForDebugger']) {
      const message = JSON.stringify({ id: n++, method, params: {} });
      this.cdp.request('NodeWorker.sendMessageToWorker',
        { sessionId, message }, 5000).catch(() => {});
    }
  }

  async routeWorkerMessage(p) {
    const sessionId = p.sessionId;
    if (!sessionId) return;
    let inner;
    try {
      inner = JSON.parse(p.message);
    } catch (_) {
      return;
    }
    if (!inner || typeof inner !== 'object') return;
    if (inner.id !== undefined) {
      const key = `${sessionId}:${inner.id}`;
      const pend = this.workerPending.get(key);
      if (pend) {
        this.workerPending.delete(key);
        try {
          if (inner.error) {
            pend.reject(new BridgeErr(
              `worker call failed: ${(inner.error && inner.error.message) || JSON.stringify(inner.error)}`));
          } else {
            pend.resolve(inner.result || {});
          }
        } catch (_) { /* already settled */ }
        return;
      }
    }
    if (!inner.method) return;
    const w = this.workerTable.get(`worker:${sessionId}`);
    if (!w || w.exited || w.state === 'ignored') {
      // Untracked or released workers never park: keep the app running.
      if (inner.method === 'Debugger.paused') {
        this.kickWorkerFree(sessionId);
      }
      return;
    }
    const m = inner.method;
    const ip = inner.params || {};
    if (m === 'Debugger.scriptParsed') {
      const { scriptId, url } = ip;
      if (scriptId) {
        w.scripts.set(scriptId, url || '');
        if (url) w.urls.set(url, scriptId);
      }
      return;
    }
    if (m === 'Debugger.breakpointResolved') {
      this.upgradeBreakpoint(ip);
      return;
    }
    if (m === 'Debugger.paused') {
      await this._chainPause(() => this._swapRun(() => this.onWorkerPaused(w, ip)));
    }
  }

  async acceptWorker(p) {
    // A worker appeared (launch + opt-in only): plant the global intent as
    // inherited copies, then resume it out of waitForDebuggerOnStart.
    if (!this.workersEnabled || this.cfg.kind !== 'launch') return;
    const sessionId = p.sessionId;
    const info = p.workerInfo || {};
    if (!sessionId) return;
    const id = `worker:${sessionId}`;
    if (this.seenWorkerIds.has(id)) return; // ids are never reused
    this.seenWorkerIds.add(id);
    const w = {
      id, sessionId, workerInfo: info,
      state: 'running', paused: null, stopInfo: null, lastStop: null,
      stopStates: [], targetRaws: new Map(), inheritedKeys: new Set(),
      breakKeys: new Map(), breakRecByKey: new Map(),
      logpoints: [], scripts: new Map(), urls: new Map(),
      seq: 0, stopSeq: 0, cachedLocals: [], lastChanged: '[]',
      lastTop: null, lastFunc: null, awaitingStep: false, exited: false,
      observed: {
        url: info.url || null, type: info.type || 'worker',
        endpoint: this.mainWsUrl || null,
      },
    };
    if (this.activeWorkers().length >= 8) {
      // Over budget: kick and release (never parked, never counted as
      // active). Later pauses auto-kick via the untracked path.
      this.kickWorkerFree(sessionId);
      w.state = 'ignored';
      this.workerTable.set(id, w);
      this.workerOrder.push(id);
      this.ignoredWorkers += 1;
      this.evictOldIgnored();
      process.stderr.write(`warn: worker ${id} over budget: released\n`);
      return;
    }
    this.workerTable.set(id, w);
    this.workerOrder.push(id);
    w.logpoints = (this.cfg.logpoints || []).map((l) => ({ ...l }));
    try {
      // Each worker is its own CDP session: enable its Debugger domain
      // first (no events, no honored resume without it), then plant.
      await this.workerSend(w, 'Debugger.enable', {}, 5000);
      await this.workerSend(w, 'Runtime.enable', {}, 5000).catch(() => {});
      await this.plantInherited(w);
      if (this.cfg.wantExc) {
        await this.workerSend(w, 'Debugger.setPauseOnExceptions',
          { state: 'uncaught' }, 5000);
      }
    } catch (e) {
      process.stderr.write(`warn: worker ${id} inherit failed: ${(e && e.message) || e}\n`);
    }
    await this.workerSend(w, 'Debugger.resume', {}, 5000).catch(() => {});
    // Belt-and-braces: a worker that reached waitForDebugger before our
    // resume may also need the run gate lifted (main uses
    // Runtime.runIfWaitingForDebugger for the same purpose).
    await this.workerSend(w, 'Runtime.runIfWaitingForDebugger', {}, 5000).catch(() => {});
    w.state = 'running';
    process.stderr.write(`target: ${id} attached (${info.url || 'worker'})\n`);
  }

  async plantInherited(w) {
    // The global intent as plant copies on a fresh worker (same one-break-
    // per-line wins as main: real breaks beat same-line logpoints).
    const byLine = new Map();
    for (const b of this.cfg.breaks) {
      byLine.set(`${b.path}:${b.line}`, { brk: b });
    }
    for (const l of this.cfg.logpoints) {
      const key = `${l.path}:${l.line}`;
      if (byLine.has(key) && byLine.get(key).brk) continue;
      byLine.set(key, { ...(byLine.get(key) || {}), log: l });
    }
    for (const [, item] of byLine) {
      const spec = item.brk || item.log;
      const kind = item.brk ? 'break' : 'logpoint';
      const fileUrl = pathToFileURL(spec.path).href;
      const params = {
        urlRegex: `^${escapeRegex(fileUrl)}$`,
        lineNumber: spec.line - 1,
      };
      if (item.brk && item.brk.cond) params.condition = item.brk.cond;
      let res;
      try {
        res = await this.workerSend(w, 'Debugger.setBreakpointByUrl', params, 5000);
      } catch (e) {
        const rec = {
          spec: this.dispSpec(spec, kind), kind, hits: 0,
          state: 'pending', detail: `inherit failed: ${(e && e.message) || e}`,
        };
        if (kind === 'logpoint') rec.detail = `${spec.template} (${rec.detail})`;
        else rec.hits = 0;
        w.stopStates.push(rec);
        continue;
      }
      const bpId = res.breakpointId;
      const rec = { spec: this.dispSpec(spec, kind), kind, hits: 0 };
      if (kind === 'logpoint') rec.detail = spec.template;
      const ckey = `${spec.path}:${spec.line}|${(spec.cond || '')}`;
      if (kind === 'break') w.breakKeys.set(ckey, bpId || null);
      if (kind === 'break') w.breakRecByKey.set(ckey, rec);
      if (!bpId) {
        rec.state = 'rejected';
        rec.detail = kind === 'logpoint' ? `${spec.template} (rejected)` : `breakpoint rejected: ${spec.path}:${spec.line}`;
        w.stopStates.push(rec);
        continue;
      }
      if (kind === 'logpoint') this.logpointIds.set(bpId, spec.template);
      else w.inheritedKeys.add(ckey);
      // NB: logpoint lines stay OUT of inheritedKeys on purpose: the
      // remove/clear paths drop by break key, and a logpoint plant must
      // never be dropped as if it were a line break (logpoints ride along
      // untouched, like main).
      this.breakIdToRec.set(bpId, { rec, line: spec.line });
      const locs = res.locations || [];
      const slidLoc = (locs.map((l) => l.lineNumber + 1).find((n) => n !== spec.line));
      if (slidLoc !== undefined) {
        rec.state = 'slid';
        rec.detail = kind === 'logpoint'
          ? `${spec.template} (slid to line ${slidLoc})`
          : `slid to line ${slidLoc}`;
      } else {
        rec.state = locs.length > 0 ? 'verified' : 'pending';
        if (rec.state === 'pending' && kind === 'break') {
          rec.detail = 'no locations yet (script not parsed or line not executable)';
        }
      }
      w.stopStates.push(rec);
    }
    if (this.cfg.wantExc) {
      w.stopStates.push({ spec: 'exc', kind: 'exc', state: 'armed', hits: 0 });
    }
  }

  workerLastStop(w) {
    const frames = w.paused ? (w.paused.frames || []) : [];
    if (frames.length === 0) return w.lastStop;
    const f = frames[0];
    return {
      file: this.relFile(this.fileOf(f, w.scripts)),
      line: (f.location && f.location.lineNumber + 1) || -1,
      method: f.functionName || '(anonymous)',
    };
  }

  /** Park one worker on a genuine pause. Hit truth is hitBreakpoints — even
   *  when V8 labels the reason 'other' (M0/M4 rule). Shared main helpers run
   *  with an explicit scripts map + sender so concurrent worker events can
   *  never clobber main (or each other): no field swapping across awaits. */
  async onWorkerPaused(w, p) {
    if (this.closing || this.exited || w.exited || w.paused) {
      return;
    }
    const sender = (method, params, timeoutMs) =>
      this.workerSend(w, method, params, timeoutMs);
    const prevSender = this.sender;
    this.sender = sender;
    // Change tracking lives on shared fields: run it against the worker's
    // copies, then restore main's.
    const savedTrack = {
      lastTop: this.lastTop, lastFunc: this.lastFunc,
      lastChanged: this.lastChanged, cachedLocals: this.cachedLocals,
    };
    this.lastTop = w.lastTop;
    this.lastFunc = w.lastFunc;
    this.lastChanged = w.lastChanged;
    this.cachedLocals = w.cachedLocals;
    const restoreTrack = () => {
      w.lastTop = this.lastTop;
      w.lastFunc = this.lastFunc;
      w.lastChanged = this.lastChanged;
      w.cachedLocals = this.cachedLocals;
      this.lastTop = savedTrack.lastTop;
      this.lastFunc = savedTrack.lastFunc;
      this.lastChanged = savedTrack.lastChanged;
      this.cachedLocals = savedTrack.cachedLocals;
      this.sender = prevSender;
    };
    try {
      const hits = p.hitBreakpoints || [];
      const frames = this.userFrames(p.callFrames || [], w.scripts);
      const logHits = hits.filter((id) => this.logpointIds.has(id));
      const realHits = hits.filter((id) => !this.logpointIds.has(id));
      this.countHits(p, w.stopStates);
      if (p.reason === 'exception') {
        w.stopInfo = this.excInfo(p.data);
        w.paused = { frames, stopInfo: w.stopInfo };
        try {
          for (const id of logHits) {
            await this.fireLogpoint(id, frames);
          }
          await this.trackChanges(frames);
        } catch (_) {
          if (!this.cachedLocals) this.cachedLocals = [];
          this.lastChanged = '[]';
        }
        restoreTrack();
        w.state = 'stopped';
        this.stopSeq += 1;
        w.stopSeq = this.stopSeq;
        w.lastStop = this.workerLastStop(w);
        this.lastParkTarget = w.id;
        this.notePark(w.id, p, w.lastStop);
        return;
      }
      if (realHits.length > 0 || w.awaitingStep) {
        w.awaitingStep = false;
        w.stopInfo = null;
        w.paused = { frames, stopInfo: null };
        try {
          for (const id of logHits) {
            await this.fireLogpoint(id, frames);
          }
          await this.trackChanges(frames);
        } catch (_) {
          if (!this.cachedLocals) this.cachedLocals = [];
          this.lastChanged = '[]';
        }
        restoreTrack();
        w.state = 'stopped';
        this.stopSeq += 1;
        w.stopSeq = this.stopSeq;
        w.lastStop = this.workerLastStop(w);
        this.lastParkTarget = w.id;
        this.notePark(w.id, p, w.lastStop);
        return;
      }
      restoreTrack();
      if (logHits.length > 0) {
        for (const id of logHits) {
          try {
            await this.fireLogpointWith(sender, id, frames);
          } catch (_) { /* template holes already degrade to '?' */ }
        }
        await sender('Debugger.resume', {}).catch(() => {});
        return;
      }
      // Entry pause (waitForDebugger) or stray pause: resume.
      await sender('Debugger.resume', {}).catch(() => {});
    } catch (_) {
      restoreTrack();
    }
  }

  async fireLogpointWith(sender, bpId, frames) {
    const template = this.logpointIds.get(bpId);
    if (!template) return;
    const prevSender = this.sender;
    this.sender = sender;
    try {
      await this.evalTemplate(template, frames);
    } finally {
      this.sender = prevSender;
    }
  }

  /** Drop node:internal frames (justMyCode spirit). Falls back to the full
   *  stack when filtering would leave nothing (e.g. stepped into loader).
   *  scripts defaults to the main map; workers pass their own. */
  userFrames(frames, scripts = this.scripts) {
    const kept = frames.filter((f) => {
      const url = f.url || scripts.get(f.location && f.location.scriptId) || '';
      return !url.startsWith('node:');
    });
    return kept.length > 0 ? kept : frames;
  }

  excInfo(data) {
    let cls = '?';
    if (data) {
      if (data.className) {
        cls = data.className;
      } else if (typeof data.description === 'string' && data.description) {
        cls = data.description.split(':')[0].trim() || '?';
      }
    }
    return JSON.stringify({ exception: { class: String(cls) } });
  }

  async fireLogpoint(bpId, frames) {
    const template = this.logpointIds.get(bpId);
    if (!template) return;
    await this.evalTemplate(template, frames);
  }

  async evalTemplate(template, frames) {
    const frameId = frames.length > 0 ? frames[0].callFrameId : null;
    let out = template;
    const holes = [...template.matchAll(/\{([^{}]+)\}/g)];
    for (const m of holes) {
      let val = '?';
      if (frameId) {
        try {
          const res = await this.req('Debugger.evaluateOnCallFrame', {
            callFrameId: frameId, expression: m[1], returnByValue: true,
          });
          if (res.result && res.result.type !== 'undefined') {
            val = res.result.value !== undefined ? String(res.result.value)
              : (res.result.description || '?');
          } else if (res.exceptionDetails) {
            val = `!${(res.exceptionDetails.text || 'error')}`;
          }
        } catch (_) {
          val = '?';
        }
      }
      out = out.split(m[0]).join(val);
    }
    this.appendLog(out);
  }

  async trackChanges(frames) {
    const locals = await this.frameLocalsIn(frames, 0);
    this.cachedLocals = locals;
    const cur = {};
    for (const l of locals) {
      if (l.name === '…') continue;
      cur[l.name] = l.value;
    }
    const func = frames.length > 0 ? (frames[0].functionName || '(anonymous)') : '?';
    let changed;
    if (this.lastTop === null || this.lastFunc !== func) {
      changed = Object.keys(cur).sort();
    } else {
      changed = Object.keys(cur).filter((n) => this.lastTop[n] !== cur[n]).sort();
    }
    this.lastTop = cur;
    this.lastFunc = func;
    this.lastChanged = JSON.stringify(changed);
  }

  onTargetOutput(text) {
    if (!text) return;
    this.outputTail = (this.outputTail + text).slice(-MAX_OUTPUT * 2);
    for (const line of text.split('\n')) {
      const t = line.replace(/\r$/, '');
      if (t.trim() && !NOISE_LINES.has(t.trim())) this.appendLog(t);
    }
  }

  appendLog(line) {
    if (line === null || line === undefined) return;
    // One physical line per entry: multi-line values (e.g. Error stacks
    // from a logpoint hole) would otherwise shatter logs.jsonl structure.
    const flat = String(line).replace(/\r?\n/g, '⏎');
    this._appendLogParts([flat]);
  }

  /** Ring-kept logs: logs.jsonl holds the latest MAX_LOG_LINES physical
   *  lines; older lines are evicted (counted in logDropped, surfaced by
   *  `logs`) instead of silently dropping NEW lines — the old cap froze
   *  `logs --tail` on stale output once full. */
  _appendLogParts(parts) {
    if (parts.length === 0) return;
    const file = path.join(this.cfg.dir, 'logs.jsonl');
    try {
      if (this.logCount + parts.length <= MAX_LOG_LINES) {
        fs.appendFileSync(file, parts.join('\n') + '\n');
        this.logCount += parts.length;
        return;
      }
      // Ring trim: keep the latest MAX lines. Bounded rewrite of a
      // ≤2000-line file (published atomically, so concurrent `logs`
      // readers never see a torn file); plain appends stay append-only.
      let kept = [];
      try {
        kept = fs.readFileSync(file, 'utf-8').split('\n');
        if (kept.length > 0 && kept[kept.length - 1] === '') kept.pop();
      } catch (_) {
        kept = [];
      }
      kept.push(...parts);
      const evicted = kept.length - MAX_LOG_LINES;
      if (evicted > 0) {
        kept = kept.slice(evicted);
        this.logDropped = (this.logDropped || 0) + evicted;
      }
      writeFile(file, kept.length > 0 ? kept.join('\n') + '\n' : '');
      this.logCount = kept.length;
    } catch (_) { /* best effort */ }
  }

  // -- pump: wait for the next stop (events arrive on their own)

  timeoutText(timeout) {
    // Timeout message with the compact observed-identity hint (names the
    // target, never claims root cause).
    let msg = `timeout: no stop within ${fmtTimeout(timeout)}`;
    if (this.cfg.observedHint) msg += `; ${this.cfg.observedHint}`;
    return msg;
  }

  async pump(timeout) {
    const deadline = Date.now() + timeout * 1000;
    for (;;) {
      if (!amOwner(this.cfg.dir)) {
        await this.cleanup().catch(() => {});
        process.exit(0);
      }
      // Any target's park satisfies the wait (first stop on any target
      // counts for launch).
      if (this.paused || this.liveWorkers().some((w) => w.paused)) return 'stopped';
      if (this.exited) throw new BridgeErr('target exited');
      if (Date.now() > deadline) {
        throw new StopTimeout(this.timeoutText(timeout));
      }
      await sleep(50);
    }
  }

  anyWorkerParked() {
    return this.liveWorkers().some((w) => w.paused);
  }

  markExited() {
    // Main exit ends every worker too (same process): move the live roster
    // to exited history, then mark the session.
    for (const w of this.liveWorkers()) {
      this.noteWorkerExit(w.id);
    }
    this.mainDead = true;
    this.exited = true;
    this.paused = null;
    this.publishState(false);
  }

  /** Trimmed stop locator for session.json (no snippet — file reads stay
   *  in snapshot()). Mirrors locationJson's file/line/method. */
  lastStopJson() {
    const frames = (this.paused && this.paused.frames) || [];
    if (frames.length === 0) return null;
    const f = frames[0];
    return {
      file: this.relFile(this.fileOf(f)),
      line: (f.location && f.location.lineNumber + 1) || -1,
      method: f.functionName || '(anonymous)',
    };
  }

  /** Rewrite session.json so `status` shows live truth (parked stop +
   *  time) with zero prior memory. lastStop survives resume/exit — it
   *  answers 'where was I last', not 'where am I now'. The redacted
   *  observedTarget rides along verbatim (CLI-computed, atomic write).
   *  Worker target parks/resumes never rewrite the main-focused file:
   *  per-worker last stops live in the roster served by `targets`. */
  publishState(stopped) {
    if (this.serving !== 'main') return;
    if (stopped) {
      try {
        this.lastStop = this.lastStopJson();
      } catch (_) { /* keep previous */ }
    }
    writeFile(path.join(this.cfg.dir, 'session.json'), JSON.stringify({
      name: path.basename(this.cfg.dir), kind: this.cfg.kind,
      port: this.sessionPort, stopped,
      lastStop: this.lastStop, updatedAt: Math.floor(Date.now() / 1000),
      observedTarget: this.cfg.observedTarget || null,
    }));
  }

  // -- snapshot builders (Java/Python shapes)

  relFile(abspath) {
    if (!abspath || abspath === '?') return '?';
    for (const src of this.cfg.srcs) {
      const rel = path.relative(src, abspath);
      if (rel && !rel.startsWith('..') && !path.isAbsolute(rel)) return rel;
    }
    const rel = path.relative(process.cwd(), abspath);
    if (rel && !rel.startsWith('..') && !path.isAbsolute(rel)) return rel;
    return abspath;
  }

  snippet(abspath, line) {
    if (!line || line < 1) return [];
    let lines;
    try {
      if (!fs.statSync(abspath).isFile()) return [];
      lines = fs.readFileSync(abspath, 'utf-8').split('\n');
    } catch (_) {
      return [];
    }
    const out = [];
    for (let n = Math.max(1, line - 5); n <= Math.min(lines.length, line + 5); n++) {
      out.push({ line: n, current: n === line, text: lines[n - 1] });
    }
    return out;
  }

  frameUrl(frame, scripts = this.scripts) {
    if (frame.url) return frame.url;
    return scripts.get(frame.location && frame.location.scriptId) || '';
  }

  fileOf(frame, scripts = this.scripts) {
    const url = this.frameUrl(frame, scripts);
    if (!url) return '?';
    try {
      if (url.startsWith('file://')) return path.normalize(decodeURI(new URL(url).pathname));
    } catch (_) { /* fall through */ }
    return url;
  }

  locationJson() {
    const frames = (this.paused && this.paused.frames) || [];
    return this.locationJsonFor(frames, this.scripts);
  }

  locationJsonFor(frames, scripts) {
    if (frames.length === 0) {
      return { class: '?', method: '?', line: -1, file: '?', snippet: [] };
    }
    const f = frames[0];
    const file = this.fileOf(f, scripts);
    const line = (f.location && f.location.lineNumber + 1) || -1;
    const base = file === '?' ? '?' : path.basename(file, path.extname(file));
    return {
      class: base,
      method: f.functionName || '(anonymous)',
      line,
      file: this.relFile(file),
      snippet: file === '?' ? [] : this.snippet(file, line),
    };
  }

  threadsJson() {
    return [{ id: 1, name: 'main', current: true }];
  }

  framesJson(withLocals) {
    return this.framesJsonFor(this.paused, this.cachedLocals, withLocals);
  }

  /** Target-explicit frame listing (M5): live reads serve a target's
   *  published park directly, never by swapping shared fields. */
  framesJsonFor(paused, cachedLocals, withLocals) {
    const frames = ((paused && paused.frames) || []).slice(0, MAX_FRAMES);
    return frames.map((f, i) => {
      const entry = {
        index: i,
        type: '?',
        method: f.functionName || '(anonymous)',
        line: (f.location && f.location.lineNumber + 1) || -1,
      };
      if (withLocals && i === 0) entry.locals = cachedLocals || [];
      return entry;
    });
  }

  async frameLocals(index = 0) {
    const frames = (this.paused && this.paused.frames) || [];
    return this.frameLocalsIn(frames, index);
  }

  async frameLocalsIn(frames, index = 0) {
    if (index < 0 || index >= frames.length) {
      throw new BridgeErr(`no frame ${index} (have ${frames.length})`);
    }
    const chain = frames[index].scopeChain || [];
    // Innermost first: V8 splits let/const into 'block' scopes (e.g. a
    // for-loop body) apart from the function 'local' scope, and outer
    // variables live in 'closure' scopes; try/catch bindings live in
    // 'catch' scopes and script-level lets in 'script' scopes. Merge all
    // six kinds in chain order, innermost name wins (an arrow stopped at
    // its first line would otherwise show a lying empty locals list).
    const seen = new Set();
    let props = [];
    for (const s of chain) {
      if (s.type !== 'local' && s.type !== 'block' && s.type !== 'closure' && s.type !== 'module' && s.type !== 'catch' && s.type !== 'script') continue;
      for (const pr of await this.scopeProps(s)) {
        if (!seen.has(pr.name)) {
          seen.add(pr.name);
          props.push(pr);
        }
      }
    }
    const out = [];
    for (const pr of props.slice(0, MAX_VARS)) {
      // Accessor properties have no value until invoked (invoking runs user
      // code — never do that for a read-only listing); mark them honestly.
      const val = pr.value !== undefined ? this.fmtRemote(pr.value)
        : (pr.get !== undefined ? '(getter — eval to read)' : '?');
      out.push({
        name: pr.name,
        type: pr.value ? (pr.value.subtype || pr.value.type || '?') : '?',
        value: val,
      });
    }
    if (props.length > MAX_VARS) {
      out.push({ name: '…', note: `+${props.length - MAX_VARS} more` });
    }
    return out;
  }

  async scopeProps(scope) {
    const obj = scope.object;
    if (!obj || !obj.objectId) return [];
    const res = await this.req('Runtime.getProperties', {
      objectId: obj.objectId, ownProperties: true,
    });
    return (res.result || []).filter((p) => p.enumerable !== false || p.value !== undefined);
  }

  fmtRemote(v) {
    if (!v) return '?';
    if (v.type === 'string') return truncStr(v.value);
    if (v.value !== undefined && (v.type === 'number' || v.type === 'boolean' || v.type === 'bigint')) {
      return truncStr(String(v.value));
    }
    if (v.type === 'undefined') return 'undefined';
    if (v.subtype === 'null' || v.type === 'object' && v.subtype === 'null') return 'null';
    if (v.type === 'function') return truncStr(v.description || 'function');
    if (v.type === 'object' || v.type === 'symbol') {
      return truncStr(v.description || v.type);
    }
    return truncStr(v.description || String(v.value));
  }

  async fmtRemoteDeep(v) {
    // One-level expansion for objects (mirrors pybridge opaque-repr rule).
    if (!v || v.type !== 'object' || !v.objectId) return this.fmtRemote(v);
    if (v.subtype === 'null') return 'null';
    let res;
    try {
      res = await this.req('Runtime.getProperties', {
        objectId: v.objectId, ownProperties: true,
      });
    } catch (_) {
      return this.fmtRemote(v);
    }
    const kids = (res.result || []).filter((p) => p.value !== undefined).slice(0, MAX_FIELDS);
    if (kids.length === 0) return this.fmtRemote(v);
    const inner = kids.map((p) => `${p.name}=${this.fmtRemote(p.value)}`).join(', ');
    const extra = (res.result || []).length - kids.length;
    const desc = v.description && v.description !== 'Object' ? v.description : null;
    const head = desc || 'Object';
    return extra > 0 ? `${head}{${inner}, … (+${extra} more fields)}` : `${head}{${inner}}`;
  }

  snapshot() {
    return {
      mode: 'session',
      location: this.locationJson(),
      threads: this.threadsJson(),
      frames: this.framesJson(true),
      output: this.outputTail.slice(-MAX_OUTPUT),
    };
  }

  // -- commands

  requireStopped() {
    if (this.serving === 'main' && this.mainDead) {
      throw new BridgeErr('target main has exited — close this session');
    }
    if (this.exited) throw new BridgeErr('target VM has exited — close this session');
    if (!this.paused) throw new BridgeErr('no stopped thread (target is running — continue first)');
    if (((this.paused && this.paused.frames) || []).length === 0) {
      throw new BridgeErr('no stopped thread yet in this session');
    }
  }

  requireLive() {
    if (this.serving === 'main' && this.mainDead) {
      throw new BridgeErr('target main has exited — close this session');
    }
    if (this.exited) throw new BridgeErr('target VM has exited — close this session');
  }

  async routeRead(req, fn) {
    const tid = this.resolveTarget(req);
    const resp = await this.withTarget(tid, fn);
    return this.withStamp(resp, tid);
  }

  cmdContext() {
    this.requireStopped();
    const threads = this.threadsJson();
    return {
      ok: true,
      stopInfo: JSON.parse(this.stopInfo || 'null'),
      location: this.locationJson(),
      threads,
      frames: this.framesJson(true),
      diag: this.stopDiag(this.serving, threads),
      warning: PARK_WARNING,
    };
  }

  cmdStack() {
    this.requireStopped();
    return { ok: true, frames: this.framesJson(false) };
  }

  async cmdVars(req) {
    this.requireStopped();
    const frame = parseInt(req.frame || 0, 10) || 0;
    return { ok: true, frame, locals: await this.frameLocals(frame) };
  }

  async cmdEval(req) {
    this.requireStopped();
    const expr = req.expr;
    if (expr === undefined || expr === null) throw new BridgeErr('eval needs an expr');
    const frame = parseInt(req.frame || 0, 10) || 0;
    const frames = (this.paused && this.paused.frames) || [];
    if (frame < 0 || frame >= frames.length) {
      throw new BridgeErr(`no frame ${frame} (have ${frames.length})`);
    }
    if (typeof expr === 'string' && expr.trim().startsWith('refs(') && expr.trim().endsWith(')')) {
      throw new BridgeErr('refs() unsupported on Node yet (no gc walk via CDP)');
    }
    let res;
    try {
      res = await this.req('Debugger.evaluateOnCallFrame', {
        callFrameId: frames[frame].callFrameId,
        expression: expr,
        returnByValue: false,
      });
    } catch (e) {
      throw new BridgeErr(`cannot evaluate '${expr}': ${(e && e.message) || e}`);
    }
    if (res.exceptionDetails) {
      const t = res.exceptionDetails.text || 'error';
      const first = (res.exceptionDetails.exception && res.exceptionDetails.exception.description) || t;
      throw new BridgeErr(`cannot evaluate '${expr}': ${String(first).split('\n')[0]}`);
    }
    const value = res.result && res.result.objectId
      ? await this.fmtRemoteDeep(res.result)
      : this.fmtRemote(res.result);
    return { ok: true, expr, value: truncStr(String(value)) };
  }

  /** Record one genuine park for stop diagnostics (synchronous at the
   *  park). Session-monotonic stopId plus previous-park comparison for
   *  same-line diagnosis. hitBreakpoints ride natively (never fabricated);
   *  DAP-less unknowns stay null. loc is {file,line} or null. */
  notePark(tid, p, loc) {
    const now = Date.now();
    const prev = this.prevPark;
    const elapsed = prev ? now - prev.atMs : null;
    const sameLoc = !!(prev && loc && prev.file === loc.file && prev.line === loc.line);
    const sameThr = !!(prev && prev.target === tid && prev.threadId === 1);
    this.stopDiagSeq += 1;
    this.prevPark = {
      target: tid, file: (loc && loc.file) || '?', line: (loc && loc.line) || -1,
      threadId: 1, atMs: now,
    };
    this.stopReason = (p && p.reason) || null;
    this.stopHitBps = (p && Array.isArray(p.hitBreakpoints)) ? [...p.hitBreakpoints] : null;
    this.parkedAtMs = now;
    this.lastDiag = {
      target: tid, stopId: this.stopDiagSeq,
      sameLocation: sameLoc, sameThread: sameThr, elapsedMs: elapsed, atMs: now,
    };
  }

  /** Additive stop diagnostics for one parked target (call in scope).
   *  requested/bound resolve via native hit ids when attributable, else
   *  null (never fabricated). */
  stopDiag(tid, threads) {
    let name = null;
    try {
      const hit = (threads || []).find((t) => t.id === 1);
      if (hit) name = hit.name || null;
    } catch (_) { name = null; }
    // Worker snapshots reuse the main-shaped threads list: keep the
    // worker convention (cmdThreads serves name 'worker' there).
    if (tid !== 'main' && (name === 'main' || name == null)) name = 'worker';
    let requested = null, bound = null, hitCount = null;
    try {
      for (const id of this.stopHitBps || []) {
        const entry = this.breakIdToRec.get(id);
        if (entry && entry.rec) {
          requested = entry.rec.spec || null;
          bound = (typeof entry.line === 'number') ? entry.line : null;
          hitCount = (typeof entry.rec.hits === 'number') ? entry.rec.hits : null;
          break;
        }
      }
    } catch (_) { requested = null; bound = null; hitCount = null; }
    const diag = {
      stopId: null, parkedAtMs: this.parkedAtMs, target: tid,
      reason: this.stopReason, stoppingThread: { id: 1, name },
      hitBreakpoints: this.stopHitBps,
      requestedBreak: requested, boundLine: bound, hitCount,
      sameLocation: false, sameThread: false, elapsedSincePreviousStopMs: null,
    };
    const last = this.lastDiag;
    if (last && last.target === tid) {
      diag.stopId = last.stopId;
      diag.sameLocation = !!last.sameLocation;
      diag.sameThread = !!last.sameThread;
      diag.elapsedSincePreviousStopMs = last.elapsedMs;
    }
    return diag;
  }

  /** Parked wait response (issues zero resume traffic by construction). */
  async waitSnapshot(tid, waited) {
    const snap = this.snapshot();
    return this.withStamp({
      ok: true, stopped: true, waited,
      changed: JSON.parse(this.lastChanged),
      stopInfo: JSON.parse(this.stopInfo || 'null'),
      snapshot: snap, diag: this.stopDiag(tid, snap.threads),
      warning: PARK_WARNING,
    }, tid);
  }

  /** Pure long-poll: NEVER resumes. Immediate success when the selected
   *  target is already parked; otherwise waits for the next fresh stop
   *  (any target — the response stamps the actual one). Timeout preserves
   *  session/intents (typed message). */
  async cmdWait(req, timeout) {
    const tid = this.resolveTarget(req);
    const parked = await this.withTarget(tid, async () => {
      this.requireLive();
      return !!this.paused;
    });
    if (parked) {
      return this.withTarget(tid, async () => this.waitSnapshot(tid, false));
    }
    await this.pump(timeout);
    let stopped = this.lastParkTarget || 'main';
    if (stopped !== 'main' && !this.workerTable.has(stopped)) stopped = tid;
    return this.withTarget(stopped, async () => this.waitSnapshot(stopped, true));
  }

  captureBounds(req) {
    const frames = req.frames !== undefined ? Number(req.frames) : 1;
    const vars = req.vars !== undefined ? Number(req.vars) : 20;
    const budget = req.pauseBudgetMs !== undefined ? Number(req.pauseBudgetMs) : 2000;
    if (!Number.isInteger(frames) || frames < 1 || frames > 10) {
      throw new BridgeErr('capture frames must be between 1 and 10');
    }
    if (!Number.isInteger(vars) || vars < 1 || vars > 20) {
      throw new BridgeErr('capture vars must be between 1 and 20');
    }
    if (!Number.isInteger(budget) || budget < 1 || budget > 10000) {
      throw new BridgeErr('capture pause budget must be between 1 and 10000 ms');
    }
    const spec = req.break !== undefined ? req.break : null;
    if (spec !== null && (typeof spec !== 'string' || !spec)) {
      throw new BridgeErr('capture --break must look like path:line');
    }
    return { frames, vars, budget, spec };
  }

  /** Capture snapshot: frames 1..10, frame-0 vars 1..20. Returns
   *  {snapshot, framesTruncated, varsTruncated}. */
  async boundedSnapshot(framesN, varsN) {
    const paused = this.paused;
    const frames = ((paused && paused.frames) || []).slice(0, framesN);
    const total = ((paused && paused.frames) || []).length;
    const out = frames.map((f, i) => ({
      index: i, type: '?',
      method: f.functionName || '(anonymous)',
      line: (f.location && f.location.lineNumber + 1) || -1,
    }));
    let varsTruncated = false;
    if (out.length > 0) {
      try {
        const full = await this.frameLocalsIn(frames, 0);
        varsTruncated = full.length > varsN;
        out[0].locals = full.slice(0, varsN);
      } catch (_) { out[0].locals = []; }
    }
    return {
      snapshot: {
        mode: 'session', location: this.locationJson(), threads: this.threadsJson(),
        frames: out, output: this.outputTail.slice(-MAX_OUTPUT),
      },
      framesTruncated: total > framesN, varsTruncated,
    };
  }

  /** Plant one ephemeral line-only break for a capture. Returns a removal
   *  token ({kind:'main-dup'|'child-dup'} when the exact line is already
   *  armed — nothing to remove). Raises BEFORE anything parks on invalid
   *  or conflicting specs (no resume owed). Never touches the global
   *  intent, stops.json, or inheritance. */
  async capturePlant(tid, spec) {
    const { parsed } = this.parseLiveBreaks([spec]);
    const b = parsed[0];
    const key = `${b.path}:${b.line}|${b.cond || ''}`;
    const loc = `${b.path}:${b.line}`;
    if (tid === 'main') {
      if (this.breakKeys.has(key)) return { kind: 'main-dup' };
      for (const k of this.breakKeys.keys()) {
        if (k.slice(0, k.lastIndexOf('|')) === loc) {
          throw new BridgeErr(`conflicting condition for ${this.relFile(b.path)}:${b.line} (already armed): ${spec}`);
        }
      }
      for (const l of this.cfg.logpoints || []) {
        if (l.path === b.path && l.line === b.line) {
          throw new BridgeErr(`conflicting condition for ${this.relFile(b.path)}:${b.line} (already armed as logpoint): ${spec}`);
        }
      }
      const fileUrl = pathToFileURL(b.path).href;
      const params = { urlRegex: `^${escapeRegex(fileUrl)}$`, lineNumber: b.line - 1 };
      if (b.cond) params.condition = b.cond;
      let res;
      try {
        res = await this.cdp.request('Debugger.setBreakpointByUrl', params, 5000);
      } catch (e) {
        throw new BridgeErr(`capture break failed to plant: ${(e && e.message) || e}`);
      }
      const bpId = res && res.breakpointId;
      if (!bpId) {
        throw new BridgeErr(`capture break failed to plant: ${this.relFile(b.path)}:${b.line}`);
      }
      // Registered for hit attribution only (never in stopStates/cfg, so
      // `breaks` and stops.json never see the ephemeral).
      const rec = { spec: this.dispSpec(b, 'break'), kind: 'break', hits: 0 };
      this.breakIdToRec.set(bpId, { rec, line: b.line });
      return { kind: 'main', bpId };
    }
    const w = this.workerTable.get(tid);
    if (!w || w.exited) throw new BridgeErr(`target ${tid} has exited — close this session`);
    const combined = new Set([...w.inheritedKeys, ...w.targetRaws.keys()]);
    if (combined.has(key)) return { kind: 'child-dup' };
    for (const k of combined) {
      if (k.slice(0, k.lastIndexOf('|')) === loc) {
        throw new BridgeErr(`conflicting condition for ${this.relFile(b.path)}:${b.line} (already armed): ${spec}`);
      }
    }
    const added = await this.addWorkerEphemeral(tid, [spec]);
    if (!added.added || added.added.length === 0) return { kind: 'child-dup' };
    return { kind: 'child', tid, key: this.matchWorkerBreak(w, spec) };
  }

  /** Remove a capture ephemeral BEFORE resume. Throws on failure (the
   *  caller still resumes, then reports removeError). */
  async captureUnplant(token) {
    if (!token || token.kind === 'main-dup' || token.kind === 'child-dup') return;
    if (token.kind === 'main') {
      try {
        await this.cdp.request('Debugger.removeBreakpoint', { breakpointId: token.bpId }, 5000);
      } finally {
        this.breakIdToRec.delete(token.bpId);
      }
      return;
    }
    if (token.kind === 'child') {
      if (!token.key) return;
      await this.dropWorkerKeys(token.tid, [token.key], []);
      return;
    }
    throw new BridgeErr(`bad capture token: ${token.kind}`);
  }

  /** One-shot bounded stop. Pre-parked target: collect WITHOUT resuming.
   *  Fresh park: collect, REMOVE EPHEMERAL BEFORE RESUME, auto-resume
   *  within the pause budget (overrun still resumes, then reports). Any
   *  collection/removal failure still resumes; timeout never resumes
   *  (nothing parked). No eval, no persisted vars. */
  async cmdCapture(req, timeout) {
    const { frames, vars, budget, spec } = this.captureBounds(req);
    const tid = this.resolveTarget(req);
    const prepark = await this.withTarget(tid, async () => {
      this.requireLive();
      return !!this.paused;
    });
    if (prepark) {
      return this.withTarget(tid, async () => {
        const { snapshot, framesTruncated, varsTruncated } =
          await this.boundedSnapshot(frames, vars);
        return this.withStamp({
          ok: true, stopped: true,
          targetWasPaused: true, resumed: false,
          pauseDurationMs: 0, pauseBudgetMs: budget, ephemeralPlanted: false,
          truncated: { frames: framesTruncated, vars: varsTruncated },
          snapshot, diag: this.stopDiag(tid, snapshot.threads),
          warning: PARK_WARNING,
        }, tid);
      });
    }
    let token = null;
    if (spec !== null) token = await this.capturePlant(tid, spec);
    try {
      await this.pump(timeout);
    } catch (e) {
      // Timeout/exit: nothing parked by us — no resume — but the ephemeral
      // must not leak.
      try {
        await this.captureUnplant(token);
      } catch (ue) {
        throw new BridgeErr(`${(e && e.message) || e}; capture ephemeral may still be planted (breaks remove --target ${tid} to clear)`);
      }
      throw e;
    }
    let stopped = this.lastParkTarget || 'main';
    if (stopped !== 'main' && !this.workerTable.has(stopped)) stopped = tid;
    const parkMs = (this.lastDiag && this.lastDiag.target === stopped && this.lastDiag.atMs) || Date.now();
    return this.withTarget(stopped, async () => {
      let snapErr = null, removeErr = null, resumeErr = null;
      let snapshot, framesTruncated = false, varsTruncated = false;
      try {
        ({ snapshot, framesTruncated, varsTruncated } =
          await this.boundedSnapshot(frames, vars));
      } catch (e) {
        snapErr = String((e && e.message) || e);
        snapshot = {
          mode: 'session', location: this.locationJson(), threads: [],
          frames: [], output: this.outputTail.slice(-MAX_OUTPUT),
        };
      }
      // REMOVE EPHEMERAL BEFORE RESUME — even when collection failed.
      try {
        await this.captureUnplant(token);
      } catch (e) { removeErr = String((e && e.message) || e); }
      // Resume while still marked paused (honest on failure: the park
      // stands and resumed:false is reported).
      const saved = this.paused;
      const savedCount = (saved && saved.frames ? saved.frames.length : 0);
      let resumed = false;
      try {
        if (this.paused) {
          this.paused = null;
          this.cachedLocals = [];
          this.publishState(false);
          try {
            await this.req('Debugger.resume');
          } catch (e) {
            if (!this.paused) {
              this.paused = saved;
              this.publishState(true);
            }
            throw e;
          }
        }
        this.clearResumeFlag(stopped);
        resumed = true;
      } catch (e) { resumeErr = String((e && e.message) || e); }
      const pauseMs = Date.now() - parkMs;
      let diag;
      try {
        diag = this.stopDiag(stopped, (snapshot && snapshot.threads) || []);
      } catch (_) { diag = { target: stopped }; }
      diag.pauseDurationMs = pauseMs;
      diag.targetWasPaused = false;
      diag.resumed = resumed;
      const resp = {
        ok: true, stopped: true, targetWasPaused: false, resumed,
        pauseDurationMs: pauseMs, pauseBudgetMs: budget,
        budgetExceeded: pauseMs > budget,
        ephemeralPlanted: !!(token && token.kind !== 'main-dup' && token.kind !== 'child-dup'),
        truncated: { frames: savedCount > frames || framesTruncated, vars: varsTruncated },
        snapshot, diag, warning: PARK_WARNING,
      };
      if (snapErr !== null) resp.snapshotError = snapErr;
      if (removeErr !== null) resp.removeError = removeErr;
      if (resumeErr !== null) resp.resumeError = resumeErr;
      return this.withStamp(resp, stopped);
    });
  }

  async cmdStep(req, timeout) {
    const tid = this.resolveTarget(req);
    await this.withTarget(tid, async () => {
      this.requireLive();
      // Stepping needs a stopped frame to step from (uniform contract on
      // all bridges); continuing works from running (it waits).
      this.requireStopped();
      const mode = req.mode || 'over';
      const method = { over: 'Debugger.stepOver', into: 'Debugger.stepInto', out: 'Debugger.stepOut' }[mode];
      if (!method) throw new BridgeErr(`bad step mode: ${mode}`);
      // Publish running BEFORE the step request: session.json must show live
      // truth even if the request hangs, and an immediate later pause (landed
      // via events) must never be overwritten by our bookkeeping below.
      const saved = this.paused;
      this.paused = null;
      this.cachedLocals = [];
      this.awaitingStep = true;
      this.publishState(false);
      try {
        await this.req(method);
      } catch (e) {
        // Stepping a running target fails at the protocol level — restore the
        // park (unless a fresh pause already won) so the session file stops
        // lying about running, and clear the flag so the next real stop does
        // not misreport as a step landing.
        this.awaitingStep = false;
        if (!this.paused) {
          this.paused = saved;
          this.publishState(true);
        }
        throw e;
      }
    });
    return this.resumeAndWait(timeout, tid);
  }

  async cmdContinue(req, timeout) {
    const tid = this.resolveTarget(req);
    await this.withTarget(tid, async () => {
      this.requireLive();
      if (this.paused) {
        // Publish running BEFORE the resume request (same ordering as step).
        const saved = this.paused;
        this.paused = null;
        this.cachedLocals = [];
        this.publishState(false);
        try {
          await this.req('Debugger.resume');
        } catch (e) {
          // Synchronous request failure: restore the park unless a fresh
          // pause already won the race.
          if (!this.paused) {
            this.paused = saved;
            this.publishState(true);
          }
          throw e;
        }
      }
      // Running already: nothing to resume (a bare resume errors on some
      // targets) — just wait for the next stop.
    });
    return this.resumeAndWait(timeout, tid);
  }

  /** Clear a resume flag on exactly the resumed holder (main or one
   *  worker): a timeout must never disarm another target's step. */
  clearResumeFlag(tid) {
    if (tid === 'main') {
      if (!this.paused) this.awaitingStep = false;
      return;
    }
    const w = this.workerTable.get(tid);
    if (w && !w.paused) w.awaitingStep = false;
  }

  async resumeAndWait(timeout, tid = 'main') {
    this.pendingTarget = tid;
    try {
      try {
        await this.pump(timeout);
      } catch (e) {
        // Pump timeout (or target exit) must clear the step flag — otherwise
        // the NEXT real stop misreports as a step landing and breakpoint hits
        // misclassify. Never touch a newly landed pause: if a pause parked
        // concurrently it already consumed the flag and published.
        this.clearResumeFlag(tid);
        throw e;
      }
      let stopped = this.lastParkTarget || 'main';
      if (stopped !== 'main' && !this.workerTable.has(stopped)) stopped = tid;
      const resp = await this.withTarget(stopped, async () => {
        const snap = this.snapshot();
        return {
          ok: true,
          stopped: true,
          changed: JSON.parse(this.lastChanged),
          stopInfo: JSON.parse(this.stopInfo || 'null'),
          snapshot: snap,
          diag: this.stopDiag(stopped, snap.threads),
          warning: PARK_WARNING,
        };
      });
      return this.withStamp(resp, stopped);
    } finally {
      this.pendingTarget = null;
    }
  }

  cmdThreads(req = {}) {
    // Bare threads stays main-focused (unchanged single-target shape);
    // --target X dumps that target instead. Served straight from published
    // parks (M5): never swaps shared fields, never waits behind an
    // outstanding resume, and issues zero CDP traffic while its target has
    // a resume outstanding (no second reader on the wire).
    const tid = this.resolveTarget(req);
    if (tid === 'main') {
      if (this.exited || this.mainDead) {
        throw new BridgeErr('target main has exited — close this session');
      }
      if (this.outstanding.has('main')) {
        return this.withStamp({ ok: true, running: true, threads: [] }, tid);
      }
      const frames = this.framesJson(false);
      return this.withStamp({
        ok: true,
        running: !this.paused,
        threads: [{ id: 1, name: 'main', status: this.paused ? 'paused' : 'running', frames }],
      }, tid);
    }
    const w = this.workerTable.get(tid);
    if (!w || w.exited || w.state === 'exited') {
      throw new BridgeErr(`target ${tid} has exited — close this session`);
    }
    if (w.state === 'ignored') {
      throw new BridgeErr(`target ${tid} was released (over budget)`);
    }
    if (this.outstanding.has(tid)) {
      return this.withStamp({ ok: true, running: true, threads: [] }, tid);
    }
    const frames = this.framesJsonFor(w.paused, w.cachedLocals, false);
    return this.withStamp({
      ok: true,
      running: !w.paused,
      threads: [{ id: 1, name: 'worker', status: w.paused ? 'paused' : 'running', frames }],
    }, tid);
  }

  cmdBreaks(req = {}) {
    // Bare breaks aggregates every target: main records plus each live
    // worker's records (copies tagged with their target id). Live
    // snapshots only — exited history lives in `targets`.
    const selected = this.resolveTarget(req);
    if (this.exited) throw new BridgeErr('target VM has exited — close this session');
    const stops = this.stopStates.map((r) => ({ ...r, target: 'main' }));
    for (const id of this.workerOrder) {
      const w = this.workerTable.get(id);
      if (!w || w.exited || w.state === 'ignored') continue;
      for (const r of w.stopStates) stops.push({ ...r, target: id });
    }
    return this.withStamp({ ok: true, stops }, selected);
  }

  /**
   * Additive line breaks on a live session (running or parked — never
   * suspended/resumed here). The whole batch validates first (parse,
   * canonical dedup/conflict incl. same-line startup logpoints) with no CDP
   * traffic, then each fresh break installs via setBreakpointByUrl (<=5s).
   * Exact canonical duplicates are idempotent (added empty); the same line
   * with a different condition — or any same-line logpoint — rejects the
   * batch before anything mutates. One V8 breakpoint per line wins, so
   * conflicts stay explicit instead of shadowing silently.
   *
   * `--target X` (worker only) plants an ephemeral target-scoped break:
   * same validation, no global intent change, no stops.json persistence,
   * no inheritance. `--target main` is the global path.
   */
  async cmdBreaksAdd(req) {
    const scope = req && typeof req.target === 'string' ? req.target : null;
    if (scope !== null && scope !== 'main') {
      const tid = this.resolveTarget(req);
      return this.addWorkerEphemeral(tid, req.breaks);
    }
    if (this.exited) throw new BridgeErr('target VM has exited — close this session');
    const raws = req.breaks;
    if (!Array.isArray(raws) || raws.length === 0) {
      throw new BridgeErr('breaks add needs at least one --break');
    }
    for (const r of raws) {
      if (typeof r !== 'string' || !r) throw new BridgeErr(`bad break spec: ${JSON.stringify(r)}`);
    }
    const scratch = { breaks: [], logpoints: [], wantExc: false, srcs: this.cfg.srcs || [] };
    for (const raw of raws) {
      const bar = raw.indexOf('|');
      const head = bar < 0 ? raw : raw.slice(0, bar);
      if (head === 'exc' || head.startsWith('exc:') || head.startsWith('method:')) {
        throw new BridgeErr(`breaks add takes line breaks only (got '${raw}')`);
      }
      try {
        parseBreak(raw, scratch);
      } catch (e) {
        throw new BridgeErr(e instanceof Usage ? e.message : String((e && e.message) || e));
      }
    }
    const canonKey = (p, line, cond) => `${p}:${line}|${cond || ''}`;
    // Armed lines: breaks carry their cond, logpoints always conflict (one
    // V8 breakpoint per line wins — a same-line logpoint is never "already
    // armed", it shadows).
    const armed = new Map(); // `${path}:${line}` -> {cond, kind}
    for (const b of this.cfg.breaks) {
      armed.set(`${b.path}:${b.line}`, { cond: b.cond || null, kind: 'break' });
    }
    for (const l of this.cfg.logpoints) {
      if (!armed.has(`${l.path}:${l.line}`)) {
        armed.set(`${l.path}:${l.line}`, { cond: null, kind: 'logpoint' });
      }
    }
    const conflictDetail = (o) => o.kind === 'logpoint'
      ? 'as logpoint'
      : (o.cond ? `as '${o.cond}'` : 'plain');
    const seen = new Set();
    const batchLoc = new Map();
    const fresh = []; // {raw, path, line, cond}
    for (let i = 0; i < raws.length; i++) {
      const b = scratch.breaks[i];
      const key = canonKey(b.path, b.line, b.cond);
      if (seen.has(key)) continue; // intra-batch duplicate: idempotent
      seen.add(key);
      const loc = `${b.path}:${b.line}`;
      if (armed.has(loc)) {
        const o = armed.get(loc);
        if (o.kind === 'break' && (o.cond || null) === (b.cond || null)) continue; // idempotent
        throw new BridgeErr(`conflicting condition for ${this.relFile(b.path)}:${b.line} ` +
          `(already armed ${conflictDetail(o)}): ${raws[i]}`);
      }
      if (batchLoc.has(loc) && (batchLoc.get(loc) || null) !== (b.cond || null)) {
        throw new BridgeErr(`conflicting condition for ${this.relFile(b.path)}:${b.line} ` +
          `(same batch): ${raws[i]}`);
      }
      batchLoc.set(loc, b.cond || null);
      fresh.push({ raw: raws[i], path: b.path, line: b.line, cond: b.cond || null });
    }
    if (fresh.length === 0) return this.withStamp({ ok: true, added: [], stops: this.stopStates }, 'main');
    const added = [];
    const failed = [];
    for (const f of fresh) {
      const fileUrl = pathToFileURL(f.path).href;
      const params = { urlRegex: `^${escapeRegex(fileUrl)}$`, lineNumber: f.line - 1 };
      if (f.cond) params.condition = f.cond;
      let res;
      try {
        res = await this.cdp.request('Debugger.setBreakpointByUrl', params, 5000);
      } catch (e) {
        failed.push(f);
        continue;
      }
      const bpId = res.breakpointId;
      if (!bpId) {
        failed.push(f);
        continue;
      }
      const rec = { spec: this.dispSpec(f, 'break'), kind: 'break', hits: 0 };
      this.breakIdToRec.set(bpId, { rec, line: f.line });
      this.breakRecByKey.set(`${f.path}:${f.line}|${f.cond || ''}`, rec);
      const locs = res.locations || [];
      const slidLoc = (locs.map((l) => l.lineNumber + 1).find((n) => n !== f.line));
      if (slidLoc !== undefined) {
        rec.state = 'slid';
        rec.detail = `slid to line ${slidLoc}`;
      } else {
        rec.state = locs.length > 0 ? 'verified' : 'pending';
        if (rec.state === 'pending') rec.detail = 'no locations yet (script not parsed or line not executable)';
      }
      this.stopStates.push(rec);
      this.cfg.breaks.push({ path: f.path, line: f.line, cond: f.cond });
      this.breakKeys.set(`${f.path}:${f.line}|${f.cond || ''}`, bpId);
      if (!this.breakRaws.has(`${f.path}:${f.line}|${f.cond || ''}`)) {
        this.breakRaws.set(`${f.path}:${f.line}|${f.cond || ''}`, f.raw);
      }
      const entry = { raw: f.raw, spec: rec.spec, kind: 'break', state: rec.state, hits: 0 };
      if (rec.detail) entry.detail = rec.detail;
      added.push(entry);
    }
    if (added.length === 0) {
      throw new BridgeErr(`breaks add failed for ${failed.length} break(s): ` +
        failed.map((f) => `${this.relFile(f.path)}:${f.line}`).join(', '));
    }
    const resp = { ok: true, added, stops: this.stopStates };
    if (failed.length > 0) {
      resp.warning = 'partial add: no change for ' +
        failed.map((f) => `${this.relFile(f.path)}:${f.line}`).join(', ');
    }
    // New global intent inherits into every live worker as plant copies.
    const inheritWarn = await this.inheritGlobalAdd(
      fresh.filter((f) =>
        this.breakRaws.has(`${f.path}:${f.line}|${f.cond || ''}`)));
    if (inheritWarn) {
      resp.warning = (resp.warning ? resp.warning + '; ' : '') + inheritWarn;
    }
    return this.withStamp(resp, 'main');
  }

  workerKeyParts(key) {
    const sep = key.lastIndexOf('|');
    const loc = sep < 0 ? key : key.slice(0, sep);
    const cpos = loc.lastIndexOf(':');
    return {
      path: loc.slice(0, cpos),
      line: parseInt(loc.slice(cpos + 1), 10),
      cond: sep < 0 ? null : (key.slice(sep + 1) || null),
    };
  }

  /** Plant one batch on a worker connection (shared by inherit + ephemeral).
   *  Returns {added, failed} with per-item {raw,path,line,cond} shapes. */
  async plantWorkerBreaks(w, fresh) {
    const added = [];
    const failed = [];
    for (const f of fresh) {
      const fileUrl = pathToFileURL(f.path).href;
      const params = { urlRegex: `^${escapeRegex(fileUrl)}$`, lineNumber: f.line - 1 };
      if (f.cond) params.condition = f.cond;
      let res;
      try {
        res = await this.workerSend(w, 'Debugger.setBreakpointByUrl', params, 5000);
      } catch (_) {
        failed.push(f);
        continue;
      }
      const bpId = res.breakpointId;
      if (!bpId) {
        failed.push(f);
        continue;
      }
      const rec = { spec: this.dispSpec(f, 'break'), kind: 'break', hits: 0 };
      this.breakIdToRec.set(bpId, { rec, line: f.line });
      w.breakKeys.set(`${f.path}:${f.line}|${f.cond || ''}`, bpId);
      w.breakRecByKey.set(`${f.path}:${f.line}|${f.cond || ''}`, rec);
      w.breakRecByKey.set(`${f.path}:${f.line}|${f.cond || ''}`, rec);
      const locs = res.locations || [];
      const slidLoc = (locs.map((l) => l.lineNumber + 1).find((n) => n !== f.line));
      if (slidLoc !== undefined) {
        rec.state = 'slid';
        rec.detail = `slid to line ${slidLoc}`;
      } else {
        rec.state = locs.length > 0 ? 'verified' : 'pending';
        if (rec.state === 'pending') rec.detail = 'no locations yet (script not parsed or line not executable)';
      }
      w.stopStates.push(rec);
      const entry = { raw: f.raw, spec: rec.spec, kind: 'break', state: rec.state, hits: 0 };
      if (rec.detail) entry.detail = rec.detail;
      added.push(entry);
    }
    return { added, failed };
  }

  /** Plant newly confirmed global breaks as inherited copies on every live
   *  worker (best-effort per target). Returns a warning string or null. */
  async inheritGlobalAdd(fresh) {
    if (fresh.length === 0) return null;
    const warnings = [];
    for (const id of this.workerOrder) {
      const w = this.workerTable.get(id);
      if (!w || w.exited || w.state === 'ignored') continue;
      const { added, failed } = await this.plantWorkerBreaks(w, fresh);
      for (const e of added) {
        const b = fresh.find((f) => f.raw === e.raw);
        if (b) w.inheritedKeys.add(`${b.path}:${b.line}|${b.cond || ''}`);
      }
      if (failed.length > 0) {
        warnings.push(`${id}: no change for ` +
          failed.map((f) => `${this.relFile(f.path)}:${f.line}`).join(', '));
      }
    }
    return warnings.length > 0 ? warnings.join('; ') : null;
  }

  parseLiveBreaks(raws) {
    if (!Array.isArray(raws) || raws.length === 0) {
      throw new BridgeErr('breaks add needs at least one --break');
    }
    for (const r of raws) {
      if (typeof r !== 'string' || !r) throw new BridgeErr(`bad break spec: ${JSON.stringify(r)}`);
    }
    const scratch = { breaks: [], logpoints: [], wantExc: false, srcs: this.cfg.srcs || [] };
    for (const raw of raws) {
      const bar = raw.indexOf('|');
      const head = bar < 0 ? raw : raw.slice(0, bar);
      if (head === 'exc' || head.startsWith('exc:') || head.startsWith('method:')) {
        throw new BridgeErr(`breaks add takes line breaks only (got '${raw}')`);
      }
      try {
        parseBreak(raw, scratch);
      } catch (e) {
        throw new BridgeErr(e instanceof Usage ? e.message : String((e && e.message) || e));
      }
    }
    return { raws, parsed: scratch.breaks };
  }

  /** Ephemeral target-scoped add on one worker: same validation as global,
   *  but matches only that worker's planted lines and never touches the
   *  global intent (no inheritance, no stops.json). */
  async addWorkerEphemeral(tid, raws) {
    const w = this.workerTable.get(tid);
    if (!w || w.exited) throw new BridgeErr(`target ${tid} has exited — close this session`);
    const { parsed } = this.parseLiveBreaks(raws);
    const combined = new Set([...w.inheritedKeys, ...w.targetRaws.keys()]);
    const armed = new Map(); // `${path}:${line}` -> {cond, kind}
    for (const key of combined) {
      const { path, line, cond } = this.workerKeyParts(key);
      armed.set(`${path}:${line}`, { cond, kind: 'break' });
    }
    for (const l of w.logpoints) {
      if (!armed.has(`${l.path}:${l.line}`)) {
        armed.set(`${l.path}:${l.line}`, { cond: null, kind: 'logpoint' });
      }
    }
    const conflictDetail = (o) => o.kind === 'logpoint'
      ? 'as logpoint'
      : (o.cond ? `as '${o.cond}'` : 'plain');
    const seen = new Set();
    const batchLoc = new Map();
    const fresh = [];
    for (let i = 0; i < raws.length; i++) {
      const b = parsed[i];
      const key = `${b.path}:${b.line}|${b.cond || ''}`;
      if (seen.has(key) || combined.has(key)) continue; // idempotent
      seen.add(key);
      const loc = `${b.path}:${b.line}`;
      if (armed.has(loc)) {
        const o = armed.get(loc);
        throw new BridgeErr(`conflicting condition for ${this.relFile(b.path)}:${b.line} ` +
          `(already armed ${conflictDetail(o)}): ${raws[i]}`);
      }
      if (batchLoc.has(loc) && (batchLoc.get(loc) || null) !== (b.cond || null)) {
        throw new BridgeErr(`conflicting condition for ${this.relFile(b.path)}:${b.line} ` +
          `(same batch): ${raws[i]}`);
      }
      batchLoc.set(loc, b.cond || null);
      fresh.push({ raw: raws[i], path: b.path, line: b.line, cond: b.cond || null });
    }
    if (fresh.length === 0) {
      return this.withStamp({ ok: true, added: [], stops: w.stopStates }, tid);
    }
    const { added, failed } = await this.plantWorkerBreaks(w, fresh);
    for (const e of added) {
      const b = fresh.find((f) => f.raw === e.raw);
      if (b) w.targetRaws.set(`${b.path}:${b.line}|${b.cond || ''}`, b.raw);
    }
    if (added.length === 0) {
      throw new BridgeErr('breaks add failed for ' +
        failed.map((f) => `${this.relFile(f.path)}:${f.line}`).join(', '));
    }
    const resp = { ok: true, added, stops: w.stopStates };
    if (failed.length > 0) {
      resp.warning = 'partial add: no change for ' +
        failed.map((f) => `${this.relFile(f.path)}:${f.line}`).join(', ');
    }
    return this.withStamp(resp, tid);
  }

  /** Lexical remove-spec parse (add normalization minus existence/range:
   *  the source may be deleted or changed and removal still works). */
  lexBreak(raw) {
    let cond = null;
    let head = raw;
    const bar = raw.indexOf('|');
    if (bar >= 0) {
      head = raw.slice(0, bar);
      cond = raw.slice(bar + 1).trim();
      if (!cond) throw new BridgeErr(`bad break spec: ${JSON.stringify(raw)}`);
    }
    if (head === 'exc' || head.startsWith('exc:') || head.startsWith('method:')) {
      throw new BridgeErr(`breaks remove takes line breaks only (got '${raw}')`);
    }
    const colon = head.lastIndexOf(':');
    if (colon <= 0) throw new BridgeErr(`bad break spec: ${JSON.stringify(raw)}`);
    const lineno = parseInt(head.slice(colon + 1), 10);
    if (Number.isNaN(lineno) || lineno < 1) {
      throw new BridgeErr(`bad break spec: ${JSON.stringify(raw)}`);
    }
    return { path: canon(head.slice(0, colon)), line: lineno, cond };
  }

  breakKeyOf(b) {
    return `${b.path}:${b.line}|${b.cond || ''}`;
  }

  matchStoredBreak(raw) {
    try {
      const b = this.lexBreak(raw);
      const key = this.breakKeyOf(b);
      if (this.breakRaws.has(key)) return key;
    } catch (_) { /* fall to stored-raw match */ }
    for (const [key, stored] of this.breakRaws) {
      if (stored === raw) return key;
    }
    return null;
  }

  matchWorkerBreak(w, raw) {
    try {
      const b = this.lexBreak(raw);
      const key = this.breakKeyOf(b);
      if (w.targetRaws.has(key)) return key;
    } catch (_) { /* fall to stored-raw match */ }
    for (const [key, stored] of w.targetRaws) {
      if (stored === raw) return key;
    }
    return null;
  }

  /** Phase 2 of scoped remove/clear on one worker: drops each key via the
   *  wrapped transport, touching only that worker's records (inherited
   *  copies and/or ephemeral entries, never the global intent). */
  async dropWorkerKeys(tid, keys, missing) {
    const w = this.workerTable.get(tid);
    if (!w || w.exited) throw new BridgeErr(`target ${tid} has exited — close this session`);
    const removed = [];
    const failed = [];
    for (const key of keys) {
      const bpId = w.breakKeys.get(key);
      const rec = w.breakRecByKey.get(key)
        || (bpId && this.breakIdToRec.get(bpId) && this.breakIdToRec.get(bpId).rec)
        || null;
      if (bpId) {
        try {
          await this.workerSend(w, 'Debugger.removeBreakpoint', { breakpointId: bpId }, 5000);
        } catch (_) {
          failed.push({
            raw: w.targetRaws.get(key) || this.breakRaws.get(key) || '',
            spec: key, error: 'backend call failed',
          });
          continue;
        }
        this.breakIdToRec.delete(bpId);
      }
      const storedRaw = w.targetRaws.get(key) || this.breakRaws.get(key) || '';
      w.breakKeys.delete(key);
      w.breakRecByKey.delete(key);
      w.targetRaws.delete(key);
      w.inheritedKeys.delete(key);
      if (rec) {
        const ri = w.stopStates.indexOf(rec);
        if (ri >= 0) w.stopStates.splice(ri, 1);
      }
      const { path: kpath, line: kline } = this.workerKeyParts(key);
      removed.push({ raw: storedRaw, spec: rec ? rec.spec : `${this.relFile(kpath)}:${kline}`, kind: 'break', hits: 0 });
    }
    if (removed.length === 0) {
      throw new BridgeErr(`breaks remove failed for ${keys.length} break(s): ` + keys.join(', '));
    }
    const resp = { ok: true, removed, stops: w.stopStates };
    if (missing.length > 0) resp.missing = missing;
    if (failed.length > 0) {
      resp.failed = failed;
      resp.warning = 'partial remove: no change for ' + failed.map((f) => f.raw).filter(Boolean).join(', ');
    }
    return this.withStamp(resp, tid);
  }

  workerAllKeys(w) {
    return [...new Set([...w.inheritedKeys, ...w.targetRaws.keys()])];
  }

  /**
   * Remove live line breaks by stored identity (running or parked).
   * Phase 1 matches the whole batch with zero CDP traffic (unmatched specs
   * land in `missing`, never ok:false); phase 2 drops each confirmed break
   * via Debugger.removeBreakpoint. `removed[]` echoes the persisted stored
   * raws. A removed break re-arms a same-line shadowed startup logpoint via
   * the normal logpoint path (or records pending + warning).
   *
   * `--target X` matches only X's ephemeral target-scoped records (no
   * global intent change). Bare remove drops the global intent plus its
   * inherited plant copies on every live worker.
   */
  async cmdBreaksRemove(req) {
    const scope = req && typeof req.target === 'string' ? req.target : null;
    if (scope !== null && scope !== 'main') {
      const tid = this.resolveTarget(req);
      const w = this.workerTable.get(tid);
      const raws = req.breaks;
      if (!Array.isArray(raws) || raws.length === 0) {
        throw new BridgeErr('breaks remove needs at least one --break');
      }
      for (const r of raws) {
        if (typeof r !== 'string' || !r) throw new BridgeErr(`bad break spec: ${JSON.stringify(r)}`);
      }
      const seen = new Set();
      const matched = [];
      const missing = [];
      for (const r of raws) {
        if (seen.has(r)) continue;
        seen.add(r);
        const key = this.matchWorkerBreak(w, r);
        if (key === null) missing.push(r);
        else if (!matched.includes(key)) matched.push(key);
      }
      if (matched.length === 0) {
        return this.withStamp({ ok: true, removed: [], missing, stops: w.stopStates }, tid);
      }
      return this.dropWorkerKeys(tid, matched, missing);
    }
    if (this.exited) throw new BridgeErr('target VM has exited — close this session');
    const raws = req.breaks;
    if (!Array.isArray(raws) || raws.length === 0) {
      throw new BridgeErr('breaks remove needs at least one --break');
    }
    for (const r of raws) {
      if (typeof r !== 'string' || !r) throw new BridgeErr(`bad break spec: ${JSON.stringify(r)}`);
    }
    const seen = new Set();
    const matched = [];
    const missing = [];
    for (const r of raws) {
      if (seen.has(r)) continue;
      seen.add(r);
      const key = this.matchStoredBreak(r);
      if (key === null) missing.push(r);
      else if (!matched.includes(key)) matched.push(key);
    }
    if (matched.length === 0) {
      return this.withStamp({ ok: true, removed: [], missing, stops: this.stopStates }, 'main');
    }
    const resp = await this.dropBreakKeys(matched, missing);
    // The global intent is gone: drop its inherited copies on every live
    // worker (per-target backend rules; a worker failure warns while the
    // confirmed main removal stands).
    const childWarnings = [];
    for (const id of this.workerOrder) {
      const w = this.workerTable.get(id);
      if (!w || w.exited || w.state === 'ignored') continue;
      const doomed = matched.filter((k) => w.inheritedKeys.has(k));
      if (doomed.length === 0) continue;
      try {
        await this.dropWorkerKeys(id, doomed, []);
      } catch (e) {
        childWarnings.push(`${id}: ${(e && e.message) || e}`);
      }
    }
    if (childWarnings.length > 0) {
      resp.warning = (resp.warning ? resp.warning + '; ' : '') + childWarnings.join('; ');
    }
    return this.withStamp(resp, 'main');
  }

  async cmdBreaksClear(req = {}) {
    const scope = req && typeof req.target === 'string' ? req.target : null;
    if (scope !== null && scope !== 'main') {
      const tid = this.resolveTarget(req);
      const w = this.workerTable.get(tid);
      const ordered = [...w.targetRaws.keys()];
      if (ordered.length === 0) {
        return this.withStamp({ ok: true, removed: [], stops: w.stopStates }, tid);
      }
      return this.dropWorkerKeys(tid, ordered, []);
    }
    if (this.exited) throw new BridgeErr('target VM has exited — close this session');
    const ordered = [];
    for (const b of this.cfg.breaks) {
      const key = this.breakKeyOf(b);
      if (this.breakRaws.has(key) && !ordered.includes(key)) ordered.push(key);
    }
    for (const key of this.breakRaws.keys()) {
      if (!ordered.includes(key)) ordered.push(key);
    }
    const finishClear = async (resp) => {
      // Bare clear is the full line-break reset: global intent plus every
      // inherited copy plus every ephemeral worker record.
      const childWarnings = [];
      for (const id of this.workerOrder) {
        const w = this.workerTable.get(id);
        if (!w || w.exited || w.state === 'ignored') continue;
        const doomed = this.workerAllKeys(w);
        if (doomed.length === 0) continue;
        try {
          await this.dropWorkerKeys(id, doomed, []);
        } catch (e) {
          childWarnings.push(`${id}: ${(e && e.message) || e}`);
        }
      }
      if (childWarnings.length > 0) {
        resp.warning = (resp.warning ? resp.warning + '; ' : '') + childWarnings.join('; ');
      }
      return this.withStamp(resp, 'main');
    };
    if (ordered.length === 0) {
      return finishClear({ ok: true, removed: [], stops: this.stopStates });
    }
    return finishClear(await this.dropBreakKeys(ordered, []));
  }

  async dropBreakKeys(keys, missing) {
    const removed = [];
    const failed = [];
    for (const key of keys) {
      const bpId = this.breakKeys.get(key);
      // The live record by canonical key — exact even for rejected plants
      // (bpId null) whose relative display spec never matches the key.
      const rec = this.breakRecByKey.get(key)
        || (bpId && this.breakIdToRec.get(bpId) && this.breakIdToRec.get(bpId).rec)
        || null;
      if (bpId) {
        try {
          await this.cdp.request('Debugger.removeBreakpoint', { breakpointId: bpId }, 5000);
        } catch (_) {
          failed.push({ raw: this.breakRaws.get(key) || '', spec: key, error: 'backend call failed' });
          continue;
        }
        this.breakIdToRec.delete(bpId);
      }
      const storedRaw = this.breakRaws.get(key) || '';
      this.breakKeys.delete(key);
      this.breakRaws.delete(key);
      this.breakRecByKey.delete(key);
      const sep = key.lastIndexOf('|');
      const loc = sep < 0 ? key : key.slice(0, sep);
      const cpos = loc.lastIndexOf(':');
      const kpath = loc.slice(0, cpos);
      const kline = parseInt(loc.slice(cpos + 1), 10);
      const kcond = sep < 0 ? null : (key.slice(sep + 1) || null);
      const bi = this.cfg.breaks.findIndex((b) => b.path === kpath && b.line === kline && (b.cond || null) === kcond);
      if (bi >= 0) this.cfg.breaks.splice(bi, 1);
      if (rec) {
        const ri = this.stopStates.indexOf(rec);
        if (ri >= 0) this.stopStates.splice(ri, 1);
      }
      removed.push({ raw: storedRaw, spec: rec ? rec.spec : loc, kind: 'break', hits: 0 });
      await this.rearmShadowedLogpoint(kpath, kline);
    }
    if (removed.length === 0) {
      throw new BridgeErr(`breaks remove failed for ${keys.length} break(s): ` + keys.join(', '));
    }
    const resp = { ok: true, removed, stops: this.stopStates };
    if (missing.length > 0) resp.missing = missing;
    if (failed.length > 0) {
      resp.failed = failed;
      resp.warning = 'partial remove: no change for ' + failed.map((f) => f.raw).filter(Boolean).join(', ');
    }
    return resp;
  }

  /** Re-plant a startup-shadowed logpoint freed by a break removal (the
   *  break won; no plant exists). Loaded targets arm now, failures record
   *  pending + warning — never a silent resurrection. */
  async rearmShadowedLogpoint(kpath, kline) {
    const idx = this.shadowedLogs.findIndex((s) => s.path === kpath && s.line === kline);
    if (idx < 0) return;
    const shadow = this.shadowedLogs[idx];
    this.shadowedLogs.splice(idx, 1);
    const fileUrl = pathToFileURL(shadow.path).href;
    let res;
    try {
      res = await this.cdp.request('Debugger.setBreakpointByUrl', {
        urlRegex: `^${escapeRegex(fileUrl)}$`, lineNumber: shadow.line - 1,
      }, 5000);
    } catch (e) {
      this.stopStates.push({
        spec: `${this.relFile(shadow.path)}:${shadow.line}`, kind: 'logpoint',
        state: 'pending', detail: `${shadow.template} (re-arm failed: ${(e && e.message) || e})`, hits: 0,
      });
      return;
    }
    const bpId = res.breakpointId;
    if (!bpId) {
      this.stopStates.push({
        spec: `${this.relFile(shadow.path)}:${shadow.line}`, kind: 'logpoint',
        state: 'pending', detail: `${shadow.template} (re-arm rejected)`, hits: 0,
      });
      return;
    }
    this.logpointIds.set(bpId, shadow.template);
    const rec = {
      spec: `${this.relFile(shadow.path)}:${shadow.line}`, kind: 'logpoint',
      detail: shadow.template, hits: 0,
    };
    const locs = res.locations || [];
    rec.state = locs.length > 0 ? 'verified' : 'pending';
    if (rec.state === 'pending') rec.detail = `${shadow.template} (no locations yet)`;
    this.breakIdToRec.set(bpId, { rec, line: shadow.line });
    this.stopStates.push(rec);
    this.cfg.logpoints.push({ path: shadow.path, line: shadow.line, template: shadow.template });
  }

  cmdLogs(req) {
    const tail = Math.max(1, Math.min(500, parseInt(req.tail || 50, 10) || 50));
    let lines = [];
    try {
      lines = fs.readFileSync(path.join(this.cfg.dir, 'logs.jsonl'), 'utf-8').split('\n');
      if (lines.length > 0 && lines[lines.length - 1] === '') lines.pop();
    } catch (_) {
      lines = [];
    }
    // total = retained lines on disk (<= MAX_LOG_LINES); dropped = lifetime
    // lines evicted by the ring; truncated = the tail was cut OR any line
    // was ever evicted (historical drops, not just the cut).
    const dropped = this.logDropped || 0;
    return {
      ok: true, total: lines.length,
      truncated: lines.length > tail || dropped > 0,
      dropped, lines: lines.slice(-tail),
    };
  }

  async dispatch(req) {
    const cmd = req.cmd;
    let timeout = Number(req.timeout !== undefined ? req.timeout : this.cfg.timeout);
    if (!Number.isFinite(timeout) || timeout <= 0 || timeout > 3600) throw new BridgeErr('timeout must be between 0 and 3600 seconds');
    // Close is terminal and always accepted, even with a resume
    // outstanding (the bridge never deadlocks waiting for a handler that
    // itself awaits a stop).
    if (cmd === 'close') throw new CloseSession();
    if (this.closing) throw new BridgeErr('session is closing');
    // M5 acceptance section: everything below runs synchronously (no
    // await), so concurrent handlers observe one atomic decision —
    // deterministic targetless selection, immediate busy rejection, and
    // resume registration BEFORE any CDP traffic.
    if ((RESUME_CMDS.has(cmd) || WAIT_CMDS.has(cmd) || CAPTURE_CMDS.has(cmd) || cmd === 'eval') && req.target === undefined && req._autoTarget === undefined) {
      try {
        const auto = this.resolveTarget(req);
        req = { ...req, _autoTarget: auto };
      } catch (_) { /* the command path raises the same error */ }
    }
    let checkTid = null;
    if (RESUME_CMDS.has(cmd) || WAIT_CMDS.has(cmd) || CAPTURE_CMDS.has(cmd) || cmd === 'eval') {
      try {
        checkTid = this.resolveTarget(req);
      } catch (_) {
        checkTid = null;
      }
    } else if (MUTATION_CMDS.has(cmd)) {
      checkTid = this.mutationTid(req);
    }
    if (checkTid !== null || MUTATION_CMDS.has(cmd)) {
      const busy = this.busyError(cmd, checkTid);
      if (busy) throw new BridgeErr(busy);
    }
    let resumeTid = null;
    if ((RESUME_CMDS.has(cmd) || WAIT_CMDS.has(cmd) || CAPTURE_CMDS.has(cmd)) && checkTid !== null) {
      this.outstanding.set(checkTid, cmd);
      resumeTid = checkTid;
    }
    try {
      if (cmd === 'targets') return this.cmdTargets();
      if (cmd === 'context') return await this.routeRead(req, () => this.cmdContext());
      if (cmd === 'stack') return await this.routeRead(req, () => this.cmdStack());
      if (cmd === 'vars') return await this.routeRead(req, () => this.cmdVars(req));
      if (cmd === 'eval') return await this.routeRead(req, () => this.cmdEval(req));
      if (cmd === 'step') return await this.cmdStep(req, timeout);
      if (cmd === 'continue') return await this.cmdContinue(req, timeout);
      if (cmd === 'wait') return await this.cmdWait(req, timeout);
      if (cmd === 'capture') return await this.cmdCapture(req, timeout);
      if (cmd === 'threads') return this.cmdThreads(req);
      if (cmd === 'breaks') return this.cmdBreaks(req);
      if (cmd === 'breaksAdd') return await this.cmdBreaksAdd(req);
      if (cmd === 'breaksRemove') return await this.cmdBreaksRemove(req);
      if (cmd === 'breaksClear') return await this.cmdBreaksClear(req);
      if (cmd === 'logs') return this.withStamp(this.cmdLogs(req), 'main');
      throw new BridgeErr(`unknown cmd: ${cmd}`);
    } finally {
      if (resumeTid !== null) this.clearOutstanding(cmd, resumeTid);
    }
  }

  async cleanup() {
    this.closing = true;
    if (this.cdp && !this.cdp.closed) {
      if (this.cfg.kind === 'launch') {
        // Launched target dies with the session (mirrors terminateDebuggee).
        try {
          if (this.child) this.child.kill('SIGKILL');
        } catch (_) { /* best effort */ }
      } else {
        try {
          await this.cdp.request('Debugger.disable', {}, 3000);
        } catch (_) { /* best effort */ }
      }
      this.cdp.close();
    } else if (this.cfg.kind === 'launch' && this.child) {
      try {
        this.child.kill('SIGKILL');
      } catch (_) { /* best effort */ }
    }
  }
}

function fmtTimeout(t) {
  return `${t}s`;
}

// ---------------------------------------------------------------- serve

async function serve(st, server, queue) {
  // M5: connections are handled concurrently (one handler per connection,
  // at most MAX_ACTIVE_HANDLERS; overflow is an immediate rejection, never
  // an unbounded spawn). Resume waits overlap across targets while live
  // reads serve published state; same-target rivals busy-reject in
  // dispatch. A client disconnect drops only its own response: target-side
  // work still publishes.
  async function handleConn(conn) {
    st.activeConns += 1;
    try {
      let req;
      try {
        req = await readFrame(conn);
      } catch (e) {
        try {
          await writeFrame(conn, { ok: false, error: String((e && e.message) || e) });
        } catch (_) { /* client already gone — nothing to answer */ }
        return;
      }
      try {
        const resp = await st.dispatch(req);
        if (resp && typeof resp === 'object' && !('target' in resp)) {
          resp.target = (st && st.serving) || 'main';
        }
        try {
          await writeFrame(conn, resp);
        } catch (_) { /* client went away mid-command: work already ran */ }
      } catch (e) {
        if (e instanceof CloseSession) {
          // Terminal and accepted despite any outstanding resume: never
          // wait for a handler that itself awaits a stop — tear down now
          // (launch kills its process, attach detaches). In-flight resume
          // handlers abort on the torn-down transport; the serve loop
          // below gives them a bounded grace to flush error responses.
          try {
            await writeFrame(conn, { ok: true, closed: true, target: 'main' });
          } catch (_) { /* client already gone */ }
          st.closing = true;
          await st.cleanup().catch(() => {});
          return;
        }
        const msg = e instanceof BridgeErr ? e.message : `internal: ${(e && e.message) || e}`;
        const target = (st && (st.pendingTarget || st.serving)) || 'main';
        try {
          await writeFrame(conn, { ok: false, error: msg, target });
        } catch (_) { /* client already gone */ }
      }
    } finally {
      conn.destroy();
      st.activeConns -= 1;
    }
  }

  for (;;) {
    if (!amOwner(st.cfg.dir)) {
      await st.cleanup().catch(() => {});
      process.exit(0);
    }
    if (st.closing) {
      // Bounded grace for in-flight handlers to flush their aborts, then
      // unconditional exit — responses were already flushed.
      await sleep(500);
      await closeServer(server);
      process.exit(0);
    }
    // Idle wait gets a 1s deadline so rm -rf abandonment is noticed
    // even with zero traffic (an unresolved waiter would orphan forever).
    while (queue.length === 0) {
      await Promise.race([
        new Promise((resolve) => {
          queue.waiter = resolve;
        }),
        sleep(1000),
      ]);
      queue.waiter = null;
      if (st.closing) break;
      if (queue.length === 0 && !amOwner(st.cfg.dir)) {
        await st.cleanup().catch(() => {});
        process.exit(0);
      }
    }
    if (st.closing) continue;
    const conn = queue.shift();
    if (!conn) continue;
    if (st.activeConns >= MAX_ACTIVE_HANDLERS) {
      try {
        await writeFrame(conn, { ok: false, error: 'overloaded: too many active handlers', target: 'main' });
      } catch (_) { /* client already gone */ }
      conn.destroy();
      continue;
    }
    handleConn(conn).catch(() => {});
  }
}

function writeSessionFile(dir, obj, cfg) {
  // Main-path session.json writes carry the redacted observed identity
  // (CLI-computed); explicit null keeps legacy readers honest.
  if (obj && typeof obj === 'object' && !('observedTarget' in obj)) {
    obj = { ...obj, observedTarget: (cfg && cfg.observedTarget) || null };
  }
  writeFile(path.join(dir, 'session.json'), JSON.stringify(obj));
}

/** Stop accepting; never let the graceful wait hang the exit (a WS close
 * handshake against a just-SIGKILLed target may never complete). Exit is
 * unconditional — responses were already flushed. */
async function closeServer(server) {
  await Promise.race([
    new Promise((resolve) => {
      try {
        server.close(resolve);
      } catch (_) {
        resolve();
      }
    }),
    sleep(2000),
  ]);
}

function die(msg, code) {
  process.stderr.write(`nodebridge: ${msg}${os.EOL}`);
  process.exit(code);
}

async function main(argv) {
  let cfg;
  try {
    cfg = parseArgs(argv);
  } catch (e) {
    if (e instanceof Usage) die(e.message, 2);
    die(`internal: ${(e && e.message) || e}`, 1);
  }
  try {
    fs.mkdirSync(cfg.dir, { recursive: true });
  } catch (e) {
    die(`cannot create ${cfg.dir}: ${e.message}`, 1);
  }
  writeOwner(cfg.dir);
  // Verify loudly: a silent owner-write failure would surface later as
  // a baffling instant self-reap (amOwner false → abandonment exit).
  if (!amOwner(cfg.dir)) die(`cannot claim session dir ${cfg.dir} (owner write failed)`, 1);
  const server = net.createServer();
  server.on('error', (e) => die(`session socket: ${e.message}`, 1));
  // Permanent queue: Node emits 'connection' eagerly, even with no listener
  // attached — a connection arriving while a command is being served would
  // be accepted and then DROPPED ON THE FLOOR (zero listeners), hanging the
  // client forever. (Blocking accept() in Python/Java has no such hole.)
  // Attach the queue before the handshake so nothing is ever missed.
  const queue = [];
  queue.waiter = null;
  server.on('connection', (conn) => {
    // Never let a dead client kill the daemon: an unlistened socket
    // 'error' (EPIPE on disconnect) rethrows and crashes the process.
    // framing.readFrame also listens, but cover the pre-read window too.
    conn.on('error', () => {});
    // M5: the queue itself is bounded — flood overflow is destroyed
    // immediately (the client retries) instead of piling unbounded work.
    if (queue.length >= MAX_QUEUED_CONNS) {
      try {
        conn.destroy();
      } catch (_) { /* already gone */ }
      return;
    }
    queue.push(conn);
    if (queue.waiter) {
      const w = queue.waiter;
      queue.waiter = null;
      w();
    }
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const port = server.address().port;
  const st = new Session(cfg);
  st.sessionPort = port;
  try {
    await st.handshake();
    if (st.child) {
      // 'close' (not 'exit'): stdio is drained first, so no tail output is
      // lost to the race between process death and our logs read. Piped
      // stdio always closes after exit — no hang risk.
      st.child.once('close', () => {
        st.markExited();
        st.paused = null;
      });
    }
    const wantStop = st.cfg.breaks.length > 0 || st.cfg.wantExc;
    if (wantStop) {
      try {
        await st.pump(st.cfg.timeout);
      } catch (e) {
        if (e instanceof StopTimeout && st.cfg.kind === 'attach') {
          // Attach-only fallback: a live target that never hits stays a
          // running session with breaks armed; the agent triggers the stop
          // later via continue. Launch timeouts rethrow into the error.json
          // path below, and target exit (plain BridgeErr) never falls back.
          writeSessionFile(cfg.dir, {
            name: path.basename(cfg.dir), kind: cfg.kind, port, stopped: false,
            lastStop: st.lastStop, updatedAt: Math.floor(Date.now() / 1000),
          }, cfg);
          try {
            await serve(st, server, queue);
          } finally {
            await closeServer(server);
          }
          process.exit(0);
        }
        if (e instanceof BridgeErr && e.message === 'target exited' && logLines(cfg.dir).length > 0) {
          // Fast program: exited before/at the first stop, but left logs.
          writeSessionFile(cfg.dir, {
            name: path.basename(cfg.dir), kind: cfg.kind, port, stopped: false,
            lastStop: st.lastStop, updatedAt: Math.floor(Date.now() / 1000),
          }, cfg);
          try {
            await serve(st, server, queue);
          } finally {
            await closeServer(server);
          }
          process.exit(0);
        }
        throw e;
      }
    }
    writeSessionFile(cfg.dir, {
      name: path.basename(cfg.dir), kind: cfg.kind, port,
      stopped: !!(st.paused || st.anyWorkerParked()),
      lastStop: st.lastStop, updatedAt: Math.floor(Date.now() / 1000),
    }, cfg);
    try {
      await serve(st, server, queue);
    } finally {
      await closeServer(server);
    }
    process.exit(0);
  } catch (e) {
    if (e instanceof Usage || e instanceof BridgeErr) {
      writeFile(path.join(cfg.dir, 'error.json'), JSON.stringify({ error: e.message }));
      await st.cleanup().catch(() => {});
      try {
        server.close();
      } catch (_) { /* best effort */ }
      // Mirror pybridge: exit nonzero; session.rs surfaces error.json.
      process.exit(e instanceof Usage ? 2 : 1);
    }
    // Unexpected setup crash (never a silent exit-1): same cleanup, then a
    // sanitized error.json the CLI surfaces. The name stays reusable — the
    // CLI removes failed-setup dirs wholesale.
    const detail = sanitizeUnexpected(e);
    writeFile(path.join(cfg.dir, 'error.json'), JSON.stringify({ error: detail }));
    await st.cleanup().catch(() => {});
    try {
      server.close();
    } catch (_) { /* best effort */ }
    process.exit(1);
  }
}

function logLines(dir) {
  try {
    return fs.readFileSync(path.join(dir, 'logs.jsonl'), 'utf-8').split('\n').filter((l) => l);
  } catch (_) {
    return [];
  }
}

main(process.argv.slice(2)).catch((e) => die(`internal: ${(e && e.stack) || e}`, 1));
