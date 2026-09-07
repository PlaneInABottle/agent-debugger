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
 * worker_threads: opt-in multi-target via --workers (launch only, default
 *  is main-only). Each worker is its own NodeWorker CDP session with the
 *  global break intent planted as inherited copies; worker-only lines need
 *  the flag to ever hit (plain main-only timeout otherwise, not an error).
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
const { BridgeErr, ConfigError, RuntimeError, CdpConn } = require('./cdp_conn.js');
const { readFrame, writeFrame } = require('./framing.js');
class CloseSession extends Error {}
// Typed first-stop timeout: attach falls back to a live running session,
// launch still fails. Never match timeout by message string.
class StopTimeout extends BridgeErr {}
// Setup-failure phase (error.json `phase`) derives from the exception
// type — never from message text or a stage timer: Usage (spec validation,
// conflicts, unknown args) and ConfigError (a valid CDP refusal of a
// breakpoint install) read as config; RuntimeError and any other
// unexpected exception read as runtime (truthful internal error, never
// endpoint-diagnosed); connect loss, request IO/timeout, and target exit
// stay transport so the CLI keeps evidence-based endpoint diagnosis.

const MAX_STRING = 200;
const MAX_FIELDS = 20;
const MAX_VARS = 20;
// Display-independent change tracking bound: top-level values scanned per
// frame for changed/removed detection (same on all adapters). The vars
// display cap (MAX_VARS=20 + sentinel) is unchanged and independent.
const CHANGE_TRACK_MAX = 256;
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
    targetIdentitySeed: null, breakRaws: {},
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
    else if (a === '--target-identity') {
      const raw = need(a);
      try {
        const parsed = JSON.parse(raw);
        cfg.targetIdentitySeed = (parsed && typeof parsed === 'object') ? parsed : null;
      } catch (_) {
        cfg.targetIdentitySeed = null;
      }
    }
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

// ---------------------------------------------------------------- layered target identity (M-ID)
// `{debuggee, endpoint, adapter}` roles with strict confidence:
// protocol-confirmed only from the `/json/list` entry; os-corroborated
// only for the OS-observed listener owner (seeded by the CLI's layered
// `--target-identity` seed); everything else is unavailable (never guessed,
// no parent-tree inference). The inspector runs in-process, so the adapter
// role is always unavailable-by-design. Every new string is redacted +
// capped before persistence or output; env is never collected.
const IDENT_FIELD_CAP = 512;   // per-field chars (matches the CLI cap)
const IDENT_ROLE_CAP = 2048;   // per-role serialized chars
const IDENT_TOTAL_CAP = 4096;  // aggregate chars over the three roles
const IDENT_ARRAY_CAP = 32;    // elements per array (head kept, tail marked)

const WAIT_NOTE = 'external trigger execution is not observed by the debugger; ' +
  'this timeout means no stop was observed, not that the code is unreachable';

const IDENT_SECRET_SUBSTR = ['password', 'passwd', 'secret', 'apikey',
  'authorization', 'authtoken', 'accesstoken'];
const IDENT_SECRET_TOKEN = ['token', 'auth', 'pwd', 'pass', 'pw'];

function identIsSecretFlag(flag) {
  const t = String(flag).replace(/^-+/, '').toLowerCase();
  const flat = t.replace(/[-_]/g, '');
  if (IDENT_SECRET_SUBSTR.some((k) => flat.includes(k))) return true;
  return t.replace(/[_.]/g, '-').split('-')
    .some((tok) => IDENT_SECRET_TOKEN.some((k) => tok === k || tok.endsWith(k)));
}

function redactIdentityArgv(argv) {
  const out = [];
  let skipNext = false;
  for (const a of argv || []) {
    if (typeof a !== 'string') continue;
    if (skipNext) {
      skipNext = false;
      if (!(a.startsWith('-') && a.length > 1)) {
        out.push('[redacted]');
        continue;
      }
    }
    const eq = a.indexOf('=');
    const co = a.indexOf(':');
    let split = -1;
    if (eq > 0) split = eq;
    if (co > 0 && (split < 0 || co < split)) split = co;
    if (split > 0) {
      const head = a.slice(0, split);
      out.push(identIsSecretFlag(head) ? `${head}${a[split]}[redacted]` : a);
    } else {
      out.push(a);
      if (identIsSecretFlag(a)) skipNext = true;
    }
  }
  return out;
}

function identCapStr(s, limit = IDENT_FIELD_CAP) {
  if (typeof s !== 'string') return s;
  if (s.length <= limit) return s;
  return `${s.slice(0, limit)}… (+${s.length - limit} more chars)`;
}

function identCapWalk(v) {
  if (typeof v === 'string') return identCapStr(v);
  if (Array.isArray(v)) {
    let arr = v;
    if (arr.length > IDENT_ARRAY_CAP) {
      arr = arr.slice(0, IDENT_ARRAY_CAP).concat([`… (+${arr.length - IDENT_ARRAY_CAP} more)`]);
    }
    return arr.map(identCapWalk);
  }
  if (v && typeof v === 'object') {
    const o = {};
    for (const k of Object.keys(v)) o[k] = identCapWalk(v[k]);
    return o;
  }
  return v;
}

function identShrinkToTotal(obj, totalCap) {
  let passes = 0;
  const longestPath = (v, prefix) => {
    let best = null;
    if (typeof v === 'string' && v.length > 1) best = { path: prefix, len: v.length };
    const kids = Array.isArray(v) ? v.map((e, i) => [e, prefix.concat([i])])
      : (v && typeof v === 'object') ? Object.keys(v).map((k) => [v[k], prefix.concat([k])]) : [];
    for (const [e, p] of kids) {
      const cand = longestPath(e, p);
      if (cand && (!best || cand.len > best.len)) best = cand;
    }
    return best;
  };
  const getAt = (root, path) => path.reduce((o, p) => o[p], root);
  const setAt = (root, path, val) => {
    const parent = getAt(root, path.slice(0, -1));
    parent[path[path.length - 1]] = val;
  };
  while (JSON.stringify(obj).length > totalCap && passes < 4096) {
    passes += 1;
    const hit = longestPath(obj, []);
    if (!hit) break;
    const cur = getAt(obj, hit.path);
    const cand = identCapStr(cur, Math.max(1, hit.len - 64));
    setAt(obj, hit.path, cand.length < hit.len ? cand : '…');
  }
  return obj;
}

function identCapRole(role) {
  return identShrinkToTotal(identCapWalk(role), IDENT_ROLE_CAP);
}

function identCapIdentity(ident) {
  const capped = {};
  for (const k of Object.keys(ident)) {
    capped[k] = (ident[k] && typeof ident[k] === 'object') ? identCapRole(ident[k]) : ident[k];
  }
  return identShrinkToTotal(capped, IDENT_TOTAL_CAP);
}

function identRoleUnavailable(reason) {
  return {
    confidence: 'unavailable', reason,
    observedAt: Math.floor(Date.now() / 1000), unavailable: [],
  };
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

// Setup-failure phase for error.json (additive; `error` text unchanged):
// derived from the exception type, never from message text or a stage
// timer. Usage (spec validation, conflicts, unknown args) and ConfigError
// (a valid CDP refusal of a breakpoint install) read as config;
// RuntimeError and any other unexpected exception read as runtime
// (truthful internal error, never endpoint-diagnosed); connect loss,
// request IO/timeout, and target exit stay transport so the CLI keeps
// evidence-based endpoint diagnosis for them. Default is transport: a
// successful connection never globally flips later failures to config.
function phaseOfError(e) {
  try {
    if (e instanceof Usage || e instanceof ConfigError) return 'config';
    if (typeof RuntimeError !== 'undefined' && e instanceof RuntimeError) return 'runtime';
    if (e instanceof BridgeErr) return 'transport';
    if (e instanceof Error) return 'runtime';
  } catch (_) { /* no verdict: fall through */ }
  return 'transport';
}

function setupErrorPayload(exc, message) {
  return { schemaVersion: 2, error: message, phase: phaseOfError(exc) };
}

// Map a setup catch to the error.json payload to persist (the exact
// mapping the setup catch uses, so tests drive this helper): Usage and
// BridgeErr keep their message (config vs transport derives from the
// type); anything unexpected is sanitized to an internal error and reads
// as runtime (truthful internal error, never endpoint-diagnosed). Phase
// always derives from the ORIGINAL exception, never message text.
function setupFailurePayload(e) {
  if (e instanceof Usage || e instanceof BridgeErr) return setupErrorPayload(e, e.message);
  return setupErrorPayload(e, sanitizeUnexpected(e));
}

/** One-line redacted hint derived from the layered CLI seed (debuggee
 *  launcher args, then endpoint listener details). Empty for a null seed;
 *  an unavailable note when the seed carries nothing nameable. */
function seedHint(seed) {
  try {
    if (!seed || typeof seed !== 'object') return '';
    for (const role of ['debuggee', 'endpoint']) {
      const r = seed[role];
      if (!r || typeof r !== 'object') continue;
      const exe = (typeof r.executable === 'string') ? r.executable : '?';
      const argv = Array.isArray(r.argv)
        ? r.argv.filter((a) => typeof a === 'string').slice(0, 3) : [];
      const cwd = (typeof r.cwd === 'string') ? r.cwd : '?';
      if (exe !== '?' || argv.length > 0) {
        return `target identity: ${exe} ${argv.join(' ')} (cwd ${cwd})`.slice(0, 200);
      }
    }
    return 'target identity unavailable (no independent source)';
  } catch (_) {
    return '';
  }
}

function dirFromArgv(argv) {
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === '--dir' && i + 1 < argv.length) return argv[i + 1];
    if (typeof argv[i] === 'string' && argv[i].startsWith('--dir=')) {
      return argv[i].slice('--dir='.length);
    }
  }
  return null;
}

// CLI-arg failures are config by definition (no connection attempted):
// best-effort error.json so the CLI preserves the semantic message
// instead of diagnosing the endpoint.
function writeParseError(argv, message) {
  try {
    const d = dirFromArgv(argv);
    if (d) writeFile(path.join(d, 'error.json'), JSON.stringify({ schemaVersion: 2, error: message, phase: 'config' }));
  } catch (_) { /* best effort: die() below still reports */ }
}

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

function escapeRegex(s) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/** Slide target: the line V8 actually bound, or null when the record must
 *  NOT read slid — no locations (pending) or ANY location on the requested
 *  line (multi-location replies where one leg hit home). Only when every
 *  returned location differs did the breakpoint truly move. */
function slidLine(locs, line) {
  if (!locs || locs.length === 0) return null;
  if (locs.some((l) => l.lineNumber + 1 === line)) return null;
  return locs[0].lineNumber + 1;
}

// ---------------------------------------------------------------- M4 owners
//
// Single-file MUST (src/bridge.rs embeds this file via include_str!): the
// three owners below live in-file. Session stays the sole runtime owner —
// it constructs exactly one WorkerRegistry, one ServerState, and three
// SerialChains, and orchestrates CDP transport. Each owner holds its own
// mutable state plus a cheap assertValid() invariant for tests.
//
// Retained (complete production routing, no aliases, no dual writes):
//   SerialChain    — one serialized promise tail (swap / mutation / pause).
//   WorkerRegistry — worker table/order/seen/exited/ignored/pending +
//                    admission/retirement/eviction.
//   ServerState    — handler-pool occupancy + terminal-close single winner.
//
// Rejected (Python M3 lesson applied): BreakpointStore / StopCoordinator.
// Breakpoint bookkeeping (breakKeys/breakRecByKey/breakIdToRec/stopStates)
// and the stop/wait/capture freshness machine (paused/stopInfo/track
// fields, freshBase/selectFreshStop) ARE the swapped worker context — a
// wrapper would only add bypass. They stay Session-owned sections below.
// Target resolve (resolveTarget) and the swap save/load quartet stay
// Session orchestration too: they fuse main liveness (mainDead/exited,
// mainSeq/lastParkTarget) with roster reads, and splitting them would
// dual-own the selection clocks.
//
// Routing rule (grep-enforced via scripts/check_nodebridge_owners.sh,
// wired into scripts/run_gates.sh --unit): production code mutates owner
// state only through owner methods — never `workers.table.set/delete`,
// never `workers.pending.set/delete`, never `server.active`/`server.closing`
// writes outside ServerState. Reads of owned collections (get/has/live
// lists, closing/active counters) stay direct.

/** One serialized promise tail: every run() waits for its predecessor, so
 *  sections that borrow shared target fields never overlap. Rejections
 *  never break the chain (the tail swallows a fork for chaining; callers
 *  still observe their own rejection). NOT reentrant: entry points wrap
 *  once and inner work never re-enters. */
class SerialChain {
  constructor() {
    this.tail = Promise.resolve();
    this.depth = 0; // queued + in-flight runs (invariant: integer >= 0)
  }

  run(fn) {
    this.depth += 1;
    const run = this.tail.then(fn);
    this.tail = run.catch(() => {});
    return run.then(
      (v) => { this.depth -= 1; return v; },
      (e) => { this.depth -= 1; throw e; },
    );
  }

  assertValid() {
    if (!Number.isInteger(this.depth) || this.depth < 0) {
      throw new Error(`SerialChain invariant: depth=${this.depth}`);
    }
  }
}

/** Canonical owner of worker-target lifecycle + the in-flight CDP replies
 *  addressed to workers.
 *
 *  Holds: worker table (id -> worker, live + ignored), creation order,
 *  every id ever issued (never reused), bounded exited history (max 16) +
 *  eviction counter, released (over-budget) lifetime counter, and the
 *  pending worker replies (`${sessionId}:${cdpId}` -> {resolve,reject}).
 *
 *  Per-target park/break fields live on Session (main context) and each
 *  worker object; Session.saveMain/loadWorker/storeWorker/loadMain
 *  context-switch them under the swap chain. CDP IO stays Session
 *  orchestration. Invariant: every table key is in order and seen; exited
 *  history is bounded; counters never go negative.
 *
 *  Pre-existing roster shape (no behavior change): order RETAINS exited
 *  ids — noteExit removes the entry from the table, not from order, so
 *  order is a creation log, not a live set. Roster readers guard with
 *  has()/liveWorkers()/findExited, which is why the invariant is table ⊆
 *  order and not equality. Future bounded-roster trigger: if order growth
 *  or order/table drift is ever attributed a measured defect, evict
 *  retired ids from order inside noteExit (and tighten assertValid to
 *  order ⊆ table keys ∪ exited ids) — until that trigger fires, the
 *  retention stays as-is. */
class WorkerRegistry {
  constructor() {
    this.table = new Map(); // id -> worker (live + ignored)
    this.order = [];        // creation log for roster listing (retains exited ids)
    this.seenIds = new Set(); // every id ever issued (no reuse)
    this.exited = [];       // bounded last-known entries (max 16)
    this.ignored = 0;       // released (over-budget) workers, lifetime
    this.droppedExited = 0; // exited-history evictions, lifetime
    this.pending = new Map(); // `${sessionId}:${cdpId}` -> {resolve,reject}
  }

  static MAX_EXITED_HISTORY = 16;
  static MAX_IGNORED_RETAINED = 16;
  static MAX_ACTIVE_WORKERS = 8;

  has(id) {
    return this.table.has(id);
  }

  get(id) {
    return this.table.get(id);
  }

  isCurrent(id, w) {
    return this.table.get(id) === w;
  }

  liveWorkers() {
    const out = [];
    for (const id of this.order) {
      const w = this.table.get(id);
      if (w && !w.exited) out.push(w);
    }
    return out;
  }

  activeWorkers() {
    return this.liveWorkers().filter((w) => w.state === 'running' || w.state === 'stopped');
  }

  findExited(id) {
    return this.exited.find((e) => e.id === id);
  }

  /** Claim an id for a fresh worker: false when already seen (ids are
   *  never reused — the caller must drop the duplicate). */
  claimId(id) {
    if (this.seenIds.has(id)) return false;
    this.seenIds.add(id);
    return true;
  }

  /** Live admission (table + order together — never one without the
   *  other). The id must be claimed first. */
  track(w) {
    this.table.set(w.id, w);
    this.order.push(w.id);
  }

  /** Over-budget admission: released (never parked, never served), counted
   *  for life, retained bounded. The caller sets w.state = 'ignored'. */
  release(w) {
    this.table.set(w.id, w);
    this.order.push(w.id);
    this.ignored += 1;
    this.evictOldIgnored();
  }

  /** Move a live worker to the bounded exited history (never reused).
   *  In-flight replies addressed to it fail now — nobody will answer. */
  noteExit(tid, failErr = new BridgeErr('worker session ended')) {
    const w = this.table.get(tid);
    if (!w) return;
    this.table.delete(tid);
    this.failSessionPending(w.sessionId, failErr);
    w.exited = true;
    w.state = 'exited';
    w.paused = null;
    this.exited.push({
      id: w.id, kind: 'worker', pid: null,
      state: 'exited', lastStop: w.lastStop,
      observed: w.observed,
      scope: w.targetRaws.size > 0 ? 'target' : 'inherited',
    });
    while (this.exited.length > WorkerRegistry.MAX_EXITED_HISTORY) {
      this.exited.shift();
      this.droppedExited += 1;
    }
  }

  evictOldIgnored() {
    const ignored = this.order.filter((id) => {
      const w = this.table.get(id);
      return w && w.state === 'ignored';
    });
    while (ignored.length > WorkerRegistry.MAX_IGNORED_RETAINED) {
      const old = ignored.shift();
      this.table.delete(old);
      const i = this.order.indexOf(old);
      if (i >= 0) this.order.splice(i, 1);
    }
  }

  putPending(key, handlers) {
    this.pending.set(key, handlers);
  }

  takePending(key) {
    const pend = this.pending.get(key);
    if (pend) this.pending.delete(key);
    return pend;
  }

  dropPending(key) {
    return this.pending.delete(key);
  }

  failSessionPending(sessionId, err) {
    for (const key of [...this.pending.keys()]) {
      if (key.startsWith(`${sessionId}:`)) {
        const p = this.pending.get(key);
        this.pending.delete(key);
        try {
          p.reject(err);
        } catch (_) { /* already settled */ }
      }
    }
  }

  assertValid() {
    for (const id of this.table.keys()) {
      if (!this.order.includes(id)) throw new Error(`WorkerRegistry invariant: table key ${id} not in order`);
      if (!this.seenIds.has(id)) throw new Error(`WorkerRegistry invariant: table key ${id} never seen`);
    }
    if (this.exited.length > WorkerRegistry.MAX_EXITED_HISTORY) {
      throw new Error(`WorkerRegistry invariant: exited history ${this.exited.length} over bound`);
    }
    if (this.ignored < 0 || this.droppedExited < 0) {
      throw new Error('WorkerRegistry invariant: negative lifetime counter');
    }
  }
}

/** Canonical owner of serve-level concurrency state: live-handler
 *  occupancy (bounded by MAX_ACTIVE_HANDLERS) and the terminal-close
 *  single winner. Session.dispatch/concurrency sections and the
 *  module-level serve/handleConn/closeFromConn functions mutate only
 *  through this API. Per-target resume occupancy (`outstanding`) stays
 *  Session-owned dispatch state — it is orthogonal to the pool/close
 *  lifecycle and moving it would only churn its many Session-surface
 *  tests. Invariant: 0 <= active <= limit; closing is boolean. */
class ServerState {
  constructor() {
    this.closing = false; // terminal close accepted (single winner below)
    this.active = 0;      // live connection handlers (bounded)
  }

  /** Bounded pool admission for one connection handler: false when the
   *  pool is full (the caller sends the overload rejection instead). The
   *  check-and-increment is synchronous — no await between — so
   *  concurrent serve iterations can never over-admit. */
  tryAcquire(limit) {
    if (this.active >= limit) return false;
    this.active += 1;
    return true;
  }

  release() {
    // Loud on unbalanced use: production pairs every release with a
    // successful tryAcquire (serve admits, handleConn's finally releases),
    // so a zero-active release is a bug, never a slow close. Failing here
    // keeps assertValid(limit) honest instead of masking drift below zero.
    if (this.active <= 0) {
      throw new Error('ServerState invariant: release without acquire');
    }
    this.active -= 1;
  }

  /** Terminal-close single winner: exactly one closer runs the teardown
   *  (the single-threaded check-and-set is atomic — no await between the
   *  flag read and write). Every close still gets its closed ACK. */
  claimClose() {
    if (this.closing) return false;
    this.closing = true;
    return true;
  }

  /** Non-winner close path (Session.cleanup direct use): idempotent. */
  markClosing() {
    this.closing = true;
  }

  assertValid(limit) {
    if (!Number.isInteger(this.active) || this.active < 0 || this.active > limit) {
      throw new Error(`ServerState invariant: active=${this.active} outside [0, ${limit}]`);
    }
    if (typeof this.closing !== 'boolean') {
      throw new Error('ServerState invariant: closing not boolean');
    }
  }
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
    // (terminal-close flag lives on this.server — see ServerState.)
    this.lastTop = null;
    this.lastFunc = null;
    this.lastChanged = '[]';
    this.lastRemoved = '[]';
    this.lastChangedComplete = false;
    this.lastChangeTracking = {
      complete: false, scanned: 0, total: null, truncated: false,
      reason: 'first-snapshot',
    };
    this.lastTrackWarn = null;
    this.lastTrackComplete = false; // was previous scan exhaustive
    this.lastTrackReason = 'first-snapshot'; // reason when incomplete
    this.stopInfo = null;
    this.outputTail = '';
    this.logCount = 0;
    this.logDropped = 0; // lifetime lines evicted by the log ring
    this.stopStates = []; // arm-time records served by `breaks`
    this.sessionPort = 0; // our TCP port (set in main, for republishing)
    this.lastStop = null; // {file,line,method} of the latest stop
    // -- worker targets (M-T/M4): main keeps its own fields above;
    // workers live in the WorkerRegistry below by opaque id
    // (worker:<sessionId>). Selection clocks (stopSeq/mainSeq/
    // lastParkTarget) stay Session-owned: they fuse main + worker parks.
    this.workersEnabled = !!(cfg && cfg.workers);
    this.workers = new WorkerRegistry();
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
    // -- layered target identity (M-ID): the kept /json/list entry (debuggee,
    // protocol-confirmed) plus the CLI-supplied OS listener observation
    // (endpoint, os-corroborated). Built at handshake; published redacted +
    // capped in session.json. No OS pid is ever claimed as the debuggee.
    this.attachEntry = null;      // {id,title,url,type} kept from /json/list
    this.targetIdentity = null;   // {debuggee,endpoint,adapter} or null
    this.identityHint = '';       // debuggee-first one-liner for timeouts
    // -- M5 concurrency: outstanding resume ops by target (tid -> cmd);
    // live reads bypass everything below and serve published state.
    // Stays Session-owned dispatch state (see ServerState head note).
    this.outstanding = new Map();
    this.server = new ServerState(); // pool occupancy + close single-winner
    this._swapChain = new SerialChain(); // swap/tracking-field mutex
    this._mutationChain = new SerialChain(); // breaks-mutation mutex (all mutations)
    this._pauseChain = new SerialChain(); // pause-event serialization
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

  // -- target resolve (Session orchestration over WorkerRegistry reads).
  // Stays here (not on the registry): explicit validation fuses main
  // liveness (mainDead/exited) with roster reads, and auto-selection fuses
  // the main selection clocks (mainSeq/lastParkTarget) with worker parks —
  // moving either half would dual-own the selection state.

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
      const w = this.workers.get(want);
      if (!w) {
        if (this.workers.findExited(want)) {
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
    for (const w of this.workers.liveWorkers()) {
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
   *  waits on different targets stay parallel. Chain-owned (see
   *  SerialChain); NOT reentrant. */
  async _swapRun(fn) {
    return this._swapChain.run(fn);
  }

  /** Serialize breaks mutations (own chain so swap traffic never waits on
   *  a plant): concurrent identical adds recheck under the chain, so the
   *  second sees the first's state (empty-added, no duplicate records);
   *  concurrent add×remove/clear on a same-file replace converge the same
   *  way (no ghost/resurrection). Mutation paths never swap shared
   *  fields, so this never nests with _swapRun in either order. The chain
   *  is NOT reentrant: entry points wrap once and inner work calls the
   *  _Inner/drop forms directly. */
  async _mutationRun(fn) {
    return this._mutationChain.run(fn);
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
      lastRemoved: this.lastRemoved,
      lastChangedComplete: this.lastChangedComplete,
      lastChangeTracking: this.lastChangeTracking,
      lastTrackWarn: this.lastTrackWarn,
      lastTrackComplete: this.lastTrackComplete,
      lastTrackReason: this.lastTrackReason,
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
    this.lastRemoved = w.lastRemoved !== undefined ? w.lastRemoved : '[]';
    this.lastChangedComplete = !!w.lastChangedComplete;
    this.lastChangeTracking = w.lastChangeTracking || this.lastChangeTracking;
    this.lastTrackWarn = w.lastTrackWarn !== undefined ? w.lastTrackWarn : null;
    this.lastTrackComplete = !!w.lastTrackComplete;
    this.lastTrackReason = w.lastTrackReason !== undefined ? w.lastTrackReason : 'first-snapshot';
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
    w.lastRemoved = this.lastRemoved;
    w.lastChangedComplete = this.lastChangedComplete;
    w.lastChangeTracking = this.lastChangeTracking;
    w.lastTrackWarn = this.lastTrackWarn;
    w.lastTrackComplete = this.lastTrackComplete;
    w.lastTrackReason = this.lastTrackReason;
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
    this.lastRemoved = saved.lastRemoved;
    this.lastChangedComplete = saved.lastChangedComplete;
    this.lastChangeTracking = saved.lastChangeTracking;
    this.lastTrackWarn = saved.lastTrackWarn;
    this.lastTrackComplete = saved.lastTrackComplete;
    this.lastTrackReason = saved.lastTrackReason;
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
    if (tid === 'main' || !this.workers.has(tid)) {
      return this._swapRun(fn);
    }
    return this._swapRun(async () => {
      const w = this.workers.get(tid);
      const saved = this.saveMain();
      this.loadWorker(w);
      try {
        return await fn();
      } finally {
        // The worker may have exited mid-command (table entry moved to
        // history): only store back while it is still live. Main context
        // restores unconditionally either way — an exit mid-command must
        // never strand the swapped fields (regression-covered).
        if (this.workers.isCurrent(tid, w)) this.storeWorker(w);
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
    // Main entries carry no `observed` (main identity lives in the
    // top-level targetIdentity); worker entries carry their protocol facts.
    if (tid === 'main') {
      return {
        id: 'main', kind: 'main', pid: null,
        state: (this.exited || this.mainDead) ? 'exited' : (this.paused ? 'stopped' : 'running'),
        lastStop: this.lastStop,
        scope: 'global',
      };
    }
    const w = this.workers.get(tid);
    return {
      id: w.id, kind: 'worker', pid: null,
      state: w.state, lastStop: w.lastStop,
      observed: w.observed,
      scope: w.targetRaws.size > 0 ? 'target' : 'inherited',
    };
  }

  cmdTargets() {
    const entries = [this.targetEntry('main')];
    for (const id of this.workers.order) {
      if (this.workers.has(id)) entries.push(this.targetEntry(id));
    }
    for (const e of this.workers.exited) entries.push(e);
    return this.withStamp({
      ok: true, targets: entries,
      selected: this.resolveTarget({}),
      ignored: this.workers.ignored,
      droppedExited: this.workers.droppedExited,
      targetIdentity: this.targetIdentity || null,
    }, 'main');
  }

  // -- target lifecycle (admission/retirement owned by WorkerRegistry)

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

  /** Fetch the raw `/json/list` entries for a devtools port (best-effort,
   *  short timeout). Returns the parsed array or null — never throws, so a
   *  missing list degrades the debuggee role to unavailable, never the
   *  attach/launch itself. */
  async fetchTargetList(host, port) {
    const url = `http://${host}:${port}/json/list`;
    const body = await new Promise((resolve) => {
      const req = http.get(url, { timeout: 2000 }, (res) => {
        let raw = '';
        res.on('data', (d) => { raw += d; });
        res.on('end', () => resolve(raw));
      });
      req.on('timeout', () => {
        req.destroy();
        resolve(null);
      });
      req.on('error', () => resolve(null));
    });
    if (typeof body !== 'string') return null;
    try {
      const parsed = JSON.parse(body);
      return Array.isArray(parsed) ? parsed : null;
    } catch (_) {
      return null;
    }
  }

  /** Pick the node inspector entry by the same rule as attach (first node
   *  entry with a debugger URL, else first entry with one). The entry is
   *  the protocol-confirmed debuggee identity — never discarded. */
  pickTargetEntry(targets) {
    return (targets || []).find((t) => t && t.type === 'node' && t.webSocketDebuggerUrl)
      || (targets || []).find((t) => t && t.webSocketDebuggerUrl)
      || null;
  }

  /** Adopt the launch target's /json/list entry (best-effort, bounded).
   *  The inspector port is parsed from the ws:// URL stderr gave us; only
   *  an entry serving that exact URL — or the sole entry on a fresh port —
   *  is adopted, so a shared inspector is never misattributed. */
  async adoptLaunchEntry(wsUrl) {
    try {
      const m = String(wsUrl || '').match(/^ws:\/\/([^/:]+):(\d+)/);
      if (!m) return;
      const list = await this.fetchTargetList(m[1], Number(m[2]));
      if (!list || list.length === 0) return;
      let node = list.find((t) => t && t.webSocketDebuggerUrl === wsUrl) || null;
      if (!node && list.length === 1) node = this.pickTargetEntry(list);
      if (!node) return;
      this.attachEntry = {
        id: node.id || null,
        title: node.title || null,
        url: node.url || null,
        type: node.type || null,
      };
    } catch (_) { /* unavailable, never fatal */ }
  }

  /** Build the layered {debuggee, endpoint, adapter} identity from the kept
   *  /json/list entry (protocol-confirmed debuggee) plus the CLI seed's
   *  layered endpoint role (os-corroborated listener owner). A
   *  malformed/missing seed degrades to all-unavailable, never a spawn
   *  failure. No OS pid is ever presented as the debuggee. Everything is
   *  redacted + capped here, before any publish. */
  buildTargetIdentity() {
    const now = Math.floor(Date.now() / 1000);
    const seed = (this.cfg && this.cfg.targetIdentitySeed) || {};
    const ep = (seed.endpoint && typeof seed.endpoint === 'object') ? seed.endpoint : {};
    const obs = {
      pid: ep.ownerPid,
      source: ep.source,
      argv: ep.argv,
      executable: ep.executable,
      cwd: ep.cwd,
    };
    const pid = (typeof obs.pid === 'number') ? obs.pid : null;
    const source = (typeof obs.source === 'string') ? obs.source : null;
    const obsArgv = Array.isArray(obs.argv)
      ? redactIdentityArgv(obs.argv.filter((a) => typeof a === 'string')) : null;
    // -- debuggee: only the /json/list entry confirms it (it carries no
    // pid, so none is ever claimed here).
    const e = this.attachEntry || {};
    let debuggee;
    if (e.url || e.title || e.id) {
      debuggee = {
        kind: 'process', title: e.title || null, url: e.url || null,
        targetId: e.id || null, source: 'cdp-target-list',
        confidence: 'protocol-confirmed', observedAt: now, unavailable: [],
      };
      if (e.id == null) {
        debuggee.unavailable.push({ field: 'targetId', reason: 'target list entry carries no id' });
      }
    } else {
      debuggee = {
        kind: 'process', title: null, url: null, targetId: null, source: null,
        confidence: 'unavailable', observedAt: now,
        unavailable: [{ field: 'title', reason: 'no /json/list entry observed' }],
      };
    }
    // -- endpoint: host/port/ws of the inspector. The V8 inspector lives in
    // the debuggee process itself (no adapter in between); the OS owner pid
    // corroborates at most, never confirms.
    let port = null;
    if (this.cfg.kind === 'attach' && Number.isInteger(this.cfg.port)) {
      port = this.cfg.port;
    } else {
      const m = String(this.mainWsUrl || '').match(/^ws:\/\/[^/:]+:(\d+)/);
      if (m) port = Number(m[1]);
    }
    const endpoint = {
      host: this.cfg.host || 'localhost', port,
      wsUrl: this.mainWsUrl || null,
      ownerPid: pid,
      executable: (typeof obs.executable === 'string') ? obs.executable : null,
      argv: obsArgv,
      cwd: (typeof obs.cwd === 'string') ? obs.cwd : null,
      role: 'inspector endpoint (served in-process by the debuggee)',
      source, confidence: pid !== null ? 'os-corroborated' : 'unavailable',
      observedAt: now,
      unavailable: pid !== null ? []
        : [{ field: 'ownerPid', reason: 'no independent pid source' }],
    };
    // -- adapter: the inspector runs in-process — there is no separate
    // adapter process by design.
    const adapter = {
      inProcess: true, confidence: 'unavailable',
      reason: 'inspector runs in-process; no separate adapter process',
      observedAt: now, unavailable: [],
    };
    this.targetIdentity = identCapIdentity({ debuggee, endpoint, adapter });
    // Debuggee-first one-liner for timeout diagnostics (concise, no
    // root-cause claim); falls back to the layered seed hint when unknown.
    let hint = '';
    if (debuggee.confidence === 'protocol-confirmed') {
      hint = `debuggee: ${debuggee.title || debuggee.url || '?'} (protocol-confirmed)`;
    }
    if (!hint) hint = seedHint(this.cfg && this.cfg.targetIdentitySeed);
    this.identityHint = hint.slice(0, 200);
    return this.targetIdentity;
  }

  /** Honest timeout context: the debugger never observes the external
   *  trigger, so triggerStatus is always unknown; success paths never
   *  fabricate sent/failed. expectedBreak rides only when the capture
   *  planted one. */
  waitContext(timeout, startedAt, expectedBreak) {
    const ctx = {
      waitStartedAt: Math.floor(startedAt / 1000),
      waitedMs: Math.max(0, Date.now() - startedAt),
      triggerStatus: 'unknown',
    };
    if (expectedBreak !== undefined && expectedBreak !== null) {
      ctx.expectedBreak = expectedBreak;
    }
    ctx.targetIdentity = (this.targetIdentity && typeof this.targetIdentity === 'object')
      ? this.targetIdentity : null;
    ctx.note = WAIT_NOTE;
    return ctx;
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
    const node = this.pickTargetEntry(targets);
    if (!node) throw new BridgeErr(`attach failed: no debuggable target at ${url}`);
    // Keep the selected entry: title/url/id is the protocol-confirmed
    // debuggee identity (it carries no pid — none is ever claimed).
    this.attachEntry = {
      id: (node && node.id) || null,
      title: (node && node.title) || null,
      url: (node && node.url) || null,
      type: (node && node.type) || null,
    };
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
      if (!this.server.closing) {
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
      // Best-effort debuggee entry for launch too (same /json/list source
      // as attach): absence degrades the role, never the launch.
      await this.adoptLaunchEntry(wsUrl);
    } else {
      wsUrl = await this.discoverAttach();
    }
    await this.connect(wsUrl);
    // The identity needs the connected debugger URL (launch endpoint
    // port/ws): build once mainWsUrl is known, before any user-visible
    // completion.
    this.buildTargetIdentity();
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
      // semantic:true: a valid V8 refusal of this install is a spec error
      // (config phase). IO/timeout/close failures stay transport, as do
      // the enable/pause/runIfWaitingForDebugger handshake requests (they
      // establish the session; they never validate a spec).
      const res = await this.cdp.request('Debugger.setBreakpointByUrl', params, 30000, { semantic: true });
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
      const slidTo = slidLine(locs, spec.line);
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
      // workers.noteExit retires whatever the table knows — tracked workers
      // AND released (ignored) ones, so a kicked worker that runs out is
      // observable as exited rather than stuck as ignored forever.
      // Unknown sessions (never seen, e.g. evicted) stay untracked.
      if (sid) this.workers.noteExit(`worker:${sid}`);
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
   *  Chain-owned (see SerialChain); rejections never break the chain
   *  (callers still observe them). */
  _chainPause(fn) {
    return this._pauseChain.run(fn);
  }

  async onPaused(p) {
    if (this.server.closing || this.exited || this.paused) {
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
      } catch (e) {
        // Still parked and published; change detail just goes quiet
        // (class-only warning, never variable data). The display cache
        // is cleared too — a logpoint failure before trackChanges ran
        // must never leave the previous stop's locals at this location.
        this.cachedLocals = [];
        this.degradeTrack((e && e.constructor && e.constructor.name) || 'Error', null, frames);
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
      } catch (e) {
        // Same no-stale-locals rule as above.
        this.cachedLocals = [];
        this.degradeTrack((e && e.constructor && e.constructor.name) || 'Error', null, frames);
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
        if (this.workers.dropPending(key)) {
          reject(new BridgeErr(`CDP ${method} timed out after ${Math.round(timeoutMs / 1000)}s`));
        }
      }, timeoutMs);
      this.workers.putPending(key, {
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
        if (this.workers.dropPending(key)) {
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
      const pend = this.workers.takePending(key);
      if (pend) {
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
    const w = this.workers.get(`worker:${sessionId}`);
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
    if (!this.workers.claimId(id)) return; // ids are never reused
    const w = {
      id, sessionId, workerInfo: info,
      state: 'running', paused: null, stopInfo: null, lastStop: null,
      stopStates: [], targetRaws: new Map(), inheritedKeys: new Set(),
      breakKeys: new Map(), breakRecByKey: new Map(),
      logpoints: [], scripts: new Map(), urls: new Map(),
      seq: 0, stopSeq: 0, cachedLocals: [], lastChanged: '[]',
      lastTop: null, lastFunc: null,
      lastRemoved: '[]', lastChangedComplete: false,
      lastChangeTracking: {
        complete: false, scanned: 0, total: null, truncated: false,
        reason: 'first-snapshot',
      },
      lastTrackWarn: null, lastTrackComplete: false,
      lastTrackReason: 'first-snapshot',
      awaitingStep: false, exited: false,
      observed: {
        url: info.url || null, type: info.type || 'worker',
        endpoint: this.mainWsUrl || null,
      },
    };
    if (this.workers.activeWorkers().length >= WorkerRegistry.MAX_ACTIVE_WORKERS) {
      // Over budget: kick and release (never parked, never counted as
      // active). Later pauses auto-kick via the untracked path.
      this.kickWorkerFree(sessionId);
      w.state = 'ignored';
      this.workers.release(w);
      process.stderr.write(`warn: worker ${id} over budget: released\n`);
      return;
    }
    this.workers.track(w);
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
    // Admission recheck: the worker may have detached while the plant
    // awaits ran (detachedFromWorker retires the table entry). A stale
    // admission must neither resume a dead session nor mark itself running.
    if (!this.workers.isCurrent(id, w)) return;
    await this.workerSend(w, 'Debugger.resume', {}, 5000).catch(() => {});
    // Belt-and-braces: a worker that reached waitForDebugger before our
    // resume may also need the run gate lifted (main uses
    // Runtime.runIfWaitingForDebugger for the same purpose).
    await this.workerSend(w, 'Runtime.runIfWaitingForDebugger', {}, 5000).catch(() => {});
    if (!this.workers.isCurrent(id, w)) return;
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
        // No plant, no intent key, no record: a stopStates entry here would
        // be unreachable — remove/clear drop by canonical key, which was
        // never registered, so no command could ever eliminate it (phantom).
        // Warn only; the next global add re-inherits this line.
        process.stderr.write(`warn: worker ${w.id} inherit failed for ` +
          `${this.dispSpec(spec, kind)}: ${(e && e.message) || e}\n`);
        continue;
      }
      const bpId = res.breakpointId;
      const rec = { spec: this.dispSpec(spec, kind), kind, hits: 0 };
      if (kind === 'logpoint') rec.detail = spec.template;
      const ckey = `${spec.path}:${spec.line}|${(spec.cond || '')}`;
      if (kind === 'break') {
        w.breakKeys.set(ckey, bpId || null);
        w.breakRecByKey.set(ckey, rec);
        // Admitted even when V8 refused the plant (null bpId): the
        // rejected record is displayed, and remove/clear drop by admitted
        // key — without this no command could ever eliminate it (phantom).
        w.inheritedKeys.add(ckey);
      }
      if (!bpId) {
        rec.state = 'rejected';
        rec.detail = kind === 'logpoint' ? `${spec.template} (rejected)` : `breakpoint rejected: ${spec.path}:${spec.line}`;
        w.stopStates.push(rec);
        continue;
      }
      if (kind === 'logpoint') this.logpointIds.set(bpId, spec.template);
      // NB: logpoint lines stay OUT of inheritedKeys on purpose: the
      // remove/clear paths drop by break key, and a logpoint plant must
      // never be dropped as if it were a line break (logpoints ride along
      // untouched, like main).
      this.breakIdToRec.set(bpId, { rec, line: spec.line });
      const locs = res.locations || [];
      const slidTo = slidLine(locs, spec.line);
      if (slidTo !== null) {
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
    if (this.server.closing || this.exited || w.exited || w.paused) {
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
      lastRemoved: this.lastRemoved,
      lastChangedComplete: this.lastChangedComplete,
      lastChangeTracking: this.lastChangeTracking,
      lastTrackWarn: this.lastTrackWarn,
      lastTrackComplete: this.lastTrackComplete,
      lastTrackReason: this.lastTrackReason,
    };
    this.lastTop = w.lastTop;
    this.lastFunc = w.lastFunc;
    this.lastChanged = w.lastChanged;
    this.cachedLocals = w.cachedLocals;
    this.lastRemoved = w.lastRemoved !== undefined ? w.lastRemoved : '[]';
    this.lastChangedComplete = !!w.lastChangedComplete;
    this.lastChangeTracking = w.lastChangeTracking || this.lastChangeTracking;
    this.lastTrackWarn = w.lastTrackWarn !== undefined ? w.lastTrackWarn : null;
    this.lastTrackComplete = !!w.lastTrackComplete;
    this.lastTrackReason = w.lastTrackReason !== undefined ? w.lastTrackReason : 'first-snapshot';
    const restoreTrack = () => {
      w.lastTop = this.lastTop;
      w.lastFunc = this.lastFunc;
      w.lastChanged = this.lastChanged;
      w.cachedLocals = this.cachedLocals;
      w.lastRemoved = this.lastRemoved;
      w.lastChangedComplete = this.lastChangedComplete;
      w.lastChangeTracking = this.lastChangeTracking;
      w.lastTrackWarn = this.lastTrackWarn;
      w.lastTrackComplete = this.lastTrackComplete;
      w.lastTrackReason = this.lastTrackReason;
      this.lastTop = savedTrack.lastTop;
      this.lastFunc = savedTrack.lastFunc;
      this.lastChanged = savedTrack.lastChanged;
      this.cachedLocals = savedTrack.cachedLocals;
      this.lastRemoved = savedTrack.lastRemoved;
      this.lastChangedComplete = savedTrack.lastChangedComplete;
      this.lastChangeTracking = savedTrack.lastChangeTracking;
      this.lastTrackWarn = savedTrack.lastTrackWarn;
      this.lastTrackComplete = savedTrack.lastTrackComplete;
      this.lastTrackReason = savedTrack.lastTrackReason;
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
        } catch (e) {
          // No stale worker display cache at the new park either.
          this.cachedLocals = [];
          this.degradeTrack((e && e.constructor && e.constructor.name) || 'Error', null, frames);
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
        } catch (e) {
          // No stale worker display cache at the new park either.
          this.cachedLocals = [];
          this.degradeTrack((e && e.constructor && e.constructor.name) || 'Error', null, frames);
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

  /** Full merged scope properties for change tracking (display-
   *  independent): same scope-precedence merge as frameLocalsIn but WITHOUT
   *  the MAX_VARS display slice. Uses the full Runtime.getProperties result
   *  already returned (one fetch per scope, no accessor invocation, no
   *  deep expansion). Throws on fetch failure (caller degrades). */
  async trackingProps(frames) {
    if (!frames || frames.length === 0) return [];
    const chain = frames[0].scopeChain || [];
    const seen = new Set();
    const props = [];
    for (const s of chain) {
      if (s.type !== 'local' && s.type !== 'block' && s.type !== 'closure' && s.type !== 'module' && s.type !== 'catch' && s.type !== 'script') continue;
      for (const pr of await this.scopeProps(s)) {
        if (!pr || typeof pr !== 'object') continue;
        if (typeof pr.name !== 'string') continue;
        if (!seen.has(pr.name)) {
          seen.add(pr.name);
          props.push(pr);
        }
      }
    }
    return props;
  }

  trackWarnText(reason) {
    return `change tracking incomplete (${reason}); changed lists only certain value changes`;
  }

  trackWarn(reason) {
    try {
      process.stderr.write(`warn: change tracking degraded (${reason})\n`);
    } catch (_) { /* ignore */ }
    return this.trackWarnText(reason);
  }

  /** Frame identity for change comparison: script (or url) + function
   *  name. Same-named functions in different scripts must never compare
   *  silently. Degrades gracefully when location data is missing. */
  trackFuncId(frames) {
    const f = (frames && frames.length > 0) ? frames[0] : null;
    const func = (f && f.functionName) || '(anonymous)';
    let loc = '';
    try {
      loc = (f && f.location && typeof f.location.scriptId === 'string') ? f.location.scriptId
        : (f && typeof f.url === 'string') ? f.url : '';
    } catch (_) { loc = ''; }
    return loc ? `${loc}#${func}` : func;
  }

  storeTrack(cur, func, changed, removed, complete, scanned, total, truncated, reason, curComplete, scanReason, warn) {
    this.lastTop = cur && typeof cur === 'object' ? cur : {};
    this.lastFunc = func;
    try {
      this.lastChanged = JSON.stringify(changed);
    } catch (_) {
      this.lastChanged = '[]';
    }
    try {
      this.lastRemoved = JSON.stringify(removed);
    } catch (_) {
      this.lastRemoved = '[]';
    }
    this.lastChangedComplete = !!complete;
    this.lastTrackComplete = !!curComplete;
    this.lastTrackReason = scanReason !== null && scanReason !== undefined
      ? scanReason : (curComplete ? null : reason);
    // Incomplete branches always pass warn=true, so exactly one class-only
    // warning is written here; complete scans stay quiet.
    this.lastTrackWarn = warn ? this.trackWarn(reason) : null;
    const tracking = {
      complete: !!complete, scanned, total, truncated: !!truncated,
    };
    if (reason !== null && reason !== undefined && !complete) {
      tracking.reason = reason;
    }
    this.lastChangeTracking = tracking;
  }

  compareTrack(cur, total, truncated, scanReason, degraded, frames) {
    const useFrames = frames || (this.paused && this.paused.frames) || [];
    const func = this.trackFuncId(useFrames);
    const last = (this.lastTop !== null && typeof this.lastTop === 'object') ? this.lastTop : null;
    const prevComplete = !!this.lastTrackComplete;
    const prevReason = this.lastTrackReason || 'truncated';
    const curComplete = (scanReason === null || scanReason === undefined) && !degraded;
    const scanned = (typeof total === 'number') ? Math.min(total, CHANGE_TRACK_MAX) : 0;
    if (last === null) {
      // No previous snapshot means no change comparison. A problem with
      // the CURRENT scan (truncated/error) dominates the reason — it
      // describes the stored baseline the next stop compares against;
      // first-snapshot only when the current scan is itself exhaustive.
      const reason = (scanReason !== null && scanReason !== undefined) ? scanReason
        : (curComplete ? 'first-snapshot' : 'tracking-error');
      this.storeTrack(cur, func, [], [], false, scanned, total, truncated, reason, curComplete, scanReason, true);
      return;
    }
    if (this.lastFunc !== func) {
      const reason = (scanReason !== null && scanReason !== undefined) ? scanReason
        : (curComplete ? 'function-changed' : 'tracking-error');
      this.storeTrack(cur, func, [], [], false, scanned, total, truncated, reason, curComplete, scanReason, true);
      return;
    }
    if (!prevComplete || !curComplete) {
      let changed = [];
      try {
        changed = Object.keys(cur).filter((n) => (n in last) && last[n] !== cur[n]).sort();
      } catch (_) {
        changed = [];
      }
      const reason = !curComplete ? (scanReason || 'tracking-error') : (prevReason || 'truncated');
      this.storeTrack(cur, func, changed, [], false, scanned, total, truncated, reason, curComplete, scanReason, true);
      return;
    }
    let changed = [];
    let removed = [];
    try {
      changed = Object.keys(cur).filter((n) => !(n in last) || last[n] !== cur[n]).sort();
      removed = Object.keys(last).filter((n) => !(n in cur)).sort();
    } catch (e) {
      const cls = (e && e.constructor && e.constructor.name) || 'Error';
      this.storeTrack(
        (cur && typeof cur === 'object') ? cur : {}, func,
        [], [], false, scanned, total, truncated, 'tracking-error', false, 'tracking-error', false);
      // Single class-only warning (storeTrack stayed silent by design).
      this.lastTrackWarn = this.trackWarn(cls);
      return;
    }
    this.storeTrack(cur, func, changed, removed, true, scanned, total, truncated, null, true, null, false);
  }

  degradeTrack(clsName, total, frames) {
    const useFrames = frames || (this.paused && this.paused.frames) || [];
    const func = this.trackFuncId(useFrames);
    this.storeTrack({}, func, [], [], false, 0, (total ?? null), false, 'tracking-error', false, 'tracking-error', false);
    // Single class-only warning (storeTrack stayed silent by design), and
    // it matches the response's trackingWarning.
    this.lastTrackWarn = this.trackWarn(clsName || 'tracking-error');
  }

  /** Additive change-tracking response fields (uniform contract). Never
   *  throws; empty changed with changedComplete=false is UNKNOWN. */
  changeFields() {
    let changed = [];
    let removed = [];
    try {
      const c = JSON.parse(this.lastChanged);
      if (Array.isArray(c)) changed = c;
    } catch (_) { changed = []; }
    try {
      const r = JSON.parse(this.lastRemoved);
      if (Array.isArray(r)) removed = r;
    } catch (_) { removed = []; }
    const complete = !!this.lastChangedComplete;
    const tracking = (this.lastChangeTracking && typeof this.lastChangeTracking === 'object')
      ? this.lastChangeTracking
      : { complete, scanned: 0, total: null, truncated: false };
    const out = { changed, removed, changedComplete: complete, changeTracking: tracking };
    if (this.lastTrackWarn && !complete) out.trackingWarning = this.lastTrackWarn;
    return out;
  }

  async trackChanges(frames) {
    // Display-independent change tracking: the full merged scope props
    // (up to CHANGE_TRACK_MAX=256) feed this path — never the
    // display-capped frameLocalsIn slice. Accessor properties keep their
    // honest placeholder (never invoked); malformed entries are skipped.
    // This method never throws to the park: fetch failures degrade to an
    // empty baseline with a class-only warning (no variable data).
    let props;
    try {
      props = await this.trackingProps(frames);
    } catch (e) {
      const cls = (e && e.constructor && e.constructor.name) || 'Error';
      // The fetch failed, so there is nothing to display either: clear
      // instead of retaining the previous stop's locals.
      this.cachedLocals = [];
      this.degradeTrack(cls, null, frames);
      return;
    }
    const names = {};
    for (const pr of props || []) {
      try {
        if (!pr || typeof pr !== 'object') continue;
        const name = pr.name;
        if (typeof name !== 'string' || name === '…') continue;
        if (name in names) continue;
        const val = pr.value !== undefined ? this.fmtRemote(pr.value)
          : (pr.get !== undefined ? '(getter — eval to read)' : '?');
        names[name] = typeof val === 'string' ? val : String(val);
      } catch (_) {
        continue;
      }
    }
    const total = Object.keys(names).length;
    const truncated = total > CHANGE_TRACK_MAX;
    const scanReason = truncated ? 'truncated' : null;
    const cur = {};
    for (const name of Object.keys(names).sort().slice(0, CHANGE_TRACK_MAX)) {
      cur[name] = names[name];
    }
    // Display locals stay capped (MAX_VARS + sentinel) and independent.
    // A display fetch failure clears the cache (never a stale previous
    // stop's locals at the new location); tracking still compares below.
    try {
      this.cachedLocals = await this.frameLocalsIn(frames, 0);
    } catch (_) {
      this.cachedLocals = [];
    }
    this.compareTrack(cur, total, truncated, scanReason, false, frames);
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
    // Timeout message with the compact identity hint (debuggee-first, names
    // the target, never claims root cause).
    let msg = `timeout: no stop within ${fmtTimeout(timeout)}`;
    const hint = this.identityHint || seedHint(this.cfg && this.cfg.targetIdentitySeed);
    if (hint) msg += `; ${hint}`;
    return msg;
  }

  stopTimeoutErr(timeout, withWaitContext, startedAt) {
    const err = new StopTimeout(this.timeoutText(timeout));
    // wait/capture only (never continue/step): the honest trigger-
    // unknown context rides structurally; the prefix is unchanged.
    if (withWaitContext) err.waitContext = this.waitContext(timeout, startedAt);
    return err;
  }

  async pump(timeout, withWaitContext = false) {
    const startedAt = Date.now();
    const deadline = startedAt + timeout * 1000;
    for (;;) {
      if (!amOwner(this.cfg.dir)) {
        await this.cleanup().catch(() => {});
        process.exit(0);
      }
      // Any target's park satisfies the wait (first stop on any target
      // counts for launch).
      if (this.paused || this.workers.liveWorkers().some((w) => w.paused)) return 'stopped';
      if (this.exited) throw new BridgeErr('target exited');
      if (Date.now() > deadline) {
        throw this.stopTimeoutErr(timeout, withWaitContext, startedAt);
      }
      await sleep(50);
    }
  }

  /** Freshness baseline for one resume/wait/capture wait: the paused
   *  objects observed at entry. A park counts as fresh only when its
   *  target's paused object differs (re-parks install a new object;
   *  untouched pre-existing parks keep theirs). */
  freshBase() {
    const workers = new Map();
    for (const [id, w] of this.workers.table) workers.set(id, w.paused || null);
    return { main: this.paused || null, workers };
  }

  isFreshPark(id, base) {
    if (id === 'main') return !!this.paused && this.paused !== base.main;
    const w = this.workers.get(id);
    return !!(w && w.paused && w.paused !== base.workers.get(id));
  }

  /** One waiter's park selection after a pump return: explicit waits
   *  (req carried a target) serve only their own target's fresh park;
   *  omitted waits keep any-target semantics but never consume a stale
   *  pre-existing park as a new stop. Returns the serving target id or
   *  null (keep waiting). Nothing is unparked or discarded — an ignored
   *  park stays served via its own target reads/roster. */
  selectFreshStop(tid, explicit, base) {
    if (explicit) return this.isFreshPark(tid, base) ? tid : null;
    if (this.isFreshPark(this.lastParkTarget, base)) return this.lastParkTarget;
    if (this.isFreshPark('main', base)) return 'main';
    for (const id of this.workers.order) {
      if (this.isFreshPark(id, base)) return id;
    }
    return null;
  }

  /** Wait for a selectable stop: loops the (stub-compatible) pump until
   *  selectFreshStop names a target or the original budget is spent.
   *  Pump errors propagate at once (a slice timeout with nothing parked
   *  is rebuilt with the exact original envelope so wait/capture
   *  enrichment keeps working); exits and owner loss pass through
   *  untouched. */
  async pumpForStop(timeout, tid, explicit, withWaitContext) {
    const startedAt = Date.now();
    const deadline = startedAt + timeout * 1000;
    const base = this.freshBase();
    for (;;) {
      const remaining = (deadline - Date.now()) / 1000;
      if (remaining <= 0) throw this.stopTimeoutErr(timeout, withWaitContext, startedAt);
      try {
        await this.pump(remaining, withWaitContext);
      } catch (e) {
        if (e instanceof StopTimeout) throw this.stopTimeoutErr(timeout, withWaitContext, startedAt);
        throw e;
      }
      const hit = this.selectFreshStop(tid, explicit, base);
      if (hit) return hit;
      // A stale or other-target park only: leave it parked and keep
      // waiting for our own stop.
      await sleep(50);
    }
  }

  anyWorkerParked() {
    return this.workers.liveWorkers().some((w) => w.paused);
  }

  markExited() {
    // Main exit ends every worker too (same process): move the live roster
    // to exited history, then mark the session.
    for (const w of this.workers.liveWorkers()) {
      this.workers.noteExit(w.id);
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
   *  answers 'where was I last', not 'where am I now'. The v2
   *  session.json carries schemaVersion 2 plus the layered targetIdentity.
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
      schemaVersion: 2,
      targetIdentity: this.targetIdentity || null,
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

  /** Uniform frame validation shared by vars/eval (same contract on
   *  all four bridges): absent/null reads as 0; a finite integer number
   *  ≥ 0 or 1–15 ASCII digits read as the index. Malformed, fractional,
   *  negative, over-long, or mistyped input is `<what> needs integer
   *  frame` (never coerced to 0); a well-formed index past the end is
   *  `no frame N (have M)`. requireStopped still runs first at the
   *  call sites. */
  parseFrameIndex(raw, total, what) {
    if (raw === undefined || raw === null) return 0;
    if (typeof raw === 'number') {
      if (!Number.isFinite(raw) || !Number.isInteger(raw) || raw < 0) {
        throw new BridgeErr(`${what} needs integer frame`);
      }
      if (raw >= total) throw new BridgeErr(`no frame ${raw} (have ${total})`);
      return raw;
    }
    if (typeof raw === 'string') {
      if (!/^[0-9]{1,15}$/.test(raw)) throw new BridgeErr(`${what} needs integer frame`);
      const v = Number(raw);
      if (v >= total) throw new BridgeErr(`no frame ${v} (have ${total})`);
      return v;
    }
    throw new BridgeErr(`${what} needs integer frame`);
  }

  async cmdVars(req) {
    this.requireStopped();
    const frames = (this.paused && this.paused.frames) || [];
    const frame = this.parseFrameIndex(req.frame, frames.length, 'vars');
    return { ok: true, frame, locals: await this.frameLocals(frame) };
  }

  async cmdEval(req) {
    this.requireStopped();
    const expr = req.expr;
    if (expr === undefined || expr === null) throw new BridgeErr('eval needs an expr');
    const frames = (this.paused && this.paused.frames) || [];
    const frame = this.parseFrameIndex(req.frame, frames.length, 'eval');
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
      ...this.changeFields(),
      stopInfo: JSON.parse(this.stopInfo || 'null'),
      snapshot: snap, diag: this.stopDiag(tid, snap.threads),
      warning: PARK_WARNING,
    }, tid);
  }

  /** Pure long-poll: NEVER resumes. Immediate success when the selected
   *  target is already parked; otherwise waits for the next fresh stop
   *  (any target when omitted — the response stamps the actual one;
   *  only the requested target when explicit). Timeout preserves
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
    const stopped = await this.pumpForStop(
      timeout, tid, typeof req.target === 'string', true);
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
   *  {snapshot, framesTruncated, varsTruncated}. The frameLocalsIn
   *  truncation sentinel ({name:'…',note}) is preserved as the last entry
   *  within the vars cap (varsN-1 real + sentinel) so the bounded
   *  snapshot still reports how many locals were cut. */
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
        const last = full.length > 0 ? full[full.length - 1] : null;
        const sentinel = (last && last.name === '…') ? last : null;
        const real = sentinel ? full.slice(0, -1) : full;
        varsTruncated = full.length > varsN || !!sentinel;
        out[0].locals = sentinel && real.length >= varsN
          ? real.slice(0, Math.max(0, varsN - 1)).concat([sentinel])
          : (sentinel ? real.concat([sentinel]) : real.slice(0, varsN));
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
    const w = this.workers.get(tid);
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
    // Entry time for the early-stage contexts below (session-gone /
    // before-armed): same seconds+ms units as every wait context; the
    // wait never happened, so waitedMs is ~0 — never faked.
    const entryStarted = Date.now();
    let tid;
    try {
      tid = this.resolveTarget(req);
    } catch (e) {
      // No live target at all (short-lived session already gone): keep
      // the truthful exited message verbatim, attach the additive stage
      // so the CLI never reports "no session".
      if (e instanceof BridgeErr && /exited/i.test(String((e && e.message) || e)) && !e.waitContext) {
        try {
          e.waitContext = {
            waitStartedAt: Math.floor(entryStarted / 1000),
            waitedMs: Math.max(0, Date.now() - entryStarted),
            triggerStatus: 'unknown',
            captureStage: 'session-gone',
            ephemeralPlanted: false,
            targetIdentity: (this.targetIdentity && typeof this.targetIdentity === 'object') ? this.targetIdentity : null,
            note: WAIT_NOTE,
          };
          if (spec !== null) e.waitContext.expectedBreak = spec;
        } catch (_) { /* best effort */ }
      }
      throw e;
    }
    let prepark = false;
    try {
      prepark = await this.withTarget(tid, async () => {
        this.requireLive();
        return !!this.paused;
      });
    } catch (e) {
      // Session already gone before the capture arrived (short-lived
      // target): keep the truthful exited message verbatim, attach the
      // additive stage so the CLI never reports "no session".
      if (e instanceof BridgeErr && /exited/i.test(String((e && e.message) || e)) && !e.waitContext) {
        try {
          e.waitContext = {
            waitStartedAt: Math.floor(entryStarted / 1000),
            waitedMs: Math.max(0, Date.now() - entryStarted),
            triggerStatus: 'unknown',
            captureStage: 'session-gone',
            ephemeralPlanted: false,
            targetIdentity: (this.targetIdentity && typeof this.targetIdentity === 'object') ? this.targetIdentity : null,
            note: WAIT_NOTE,
          };
          if (spec !== null) e.waitContext.expectedBreak = spec;
        } catch (_) { /* best effort */ }
      }
      throw e;
    }
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
    // Fresh path: plant the ephemeral first (failure parks nothing, so no
    // resume is owed). A plant failure from a dying session means the
    // target exited BEFORE the ephemeral was armed — wrapped truthfully;
    // invalid specs stay verbatim (the target did not exit).
    let token = null;
    if (spec !== null) {
      try {
        token = await this.capturePlant(tid, spec);
      } catch (e) {
        if (e instanceof BridgeErr && /exited|closed/i.test(String((e && e.message) || e)) && !e.waitContext) {
          const wrapped = new BridgeErr(`capture target exited before ephemeral breakpoint was armed: ${(e && e.message) || e}`);
          try {
            wrapped.waitContext = {
              waitStartedAt: Math.floor(entryStarted / 1000),
              waitedMs: Math.max(0, Date.now() - entryStarted),
              triggerStatus: 'unknown',
              captureStage: 'before-armed',
              ephemeralPlanted: false,
              expectedBreak: spec,
              targetIdentity: (this.targetIdentity && typeof this.targetIdentity === 'object') ? this.targetIdentity : null,
              note: WAIT_NOTE,
            };
          } catch (_) { /* best effort */ }
          throw wrapped;
        }
        throw e;
      }
    }
    // Stage a short-lived-target exit as the truthful armed-wait error
    // (null when e is not an exit/close): reaching the pump means the
    // ephemeral WAS armed. Never endpoint-rejected/unreachable, never
    // "unreachable code". waitStartedAt/waitedMs match the timeout
    // context units (seconds since epoch / ms waited).
    const started = Date.now();
    const stageExit = (err) => {
      if (!(err instanceof BridgeErr) || !/exited|closed/i.test(String((err && err.message) || err))) return null;
      const wasPlanted = !!(token && token.kind !== 'main-dup' && token.kind !== 'child-dup');
      const wrapped = new BridgeErr(
        `target exited before capture hit${spec !== null ? ` (${spec})` : ''}: ${(err && err.message) || err}`);
      try {
        wrapped.waitContext = {
          waitStartedAt: Math.floor(started / 1000),
          waitedMs: Math.max(0, Date.now() - started),
          triggerStatus: 'unknown',
          captureStage: 'armed-wait',
          ephemeralPlanted: wasPlanted,
          targetIdentity: (this.targetIdentity && typeof this.targetIdentity === 'object') ? this.targetIdentity : null,
          note: WAIT_NOTE,
        };
        if (spec !== null) wrapped.waitContext.expectedBreak = spec;
      } catch (_) { /* best effort */ }
      return wrapped;
    };
    // Single wait, single budget: the park below is the one collected,
    // unplanted, and resumed. (A second pump would discard the first park
    // and demand another stop.)
    let stopped;
    try {
      stopped = await this.pumpForStop(
        timeout, tid, typeof req.target === 'string', true);
    } catch (e) {
      // Timeout/exit: nothing parked by us — no resume — but the ephemeral
      // must not leak. A removal failure on a dead target must not mask
      // the exit stage either: the staged error carries the removal note
      // with its waitContext.
      try {
        await this.captureUnplant(token);
      } catch (ue) {
        const staged = stageExit(e);
        if (staged) {
          const err = new BridgeErr(`${staged.message}; capture ephemeral remove failed: ${(ue && ue.message) || ue} (breaks remove --target ${tid} to clear)`);
          if (staged.waitContext && typeof staged.waitContext === 'object') err.waitContext = staged.waitContext;
          throw err;
        }
        throw new BridgeErr(`${(e && e.message) || e}; capture ephemeral remove failed: ${(ue && ue.message) || ue} (breaks remove --target ${tid} to clear)`);
      }
      // The planted spec rides the honest timeout context (in canonical
      // field order); reaching the pump means the ephemeral WAS armed, so
      // a target exit here is always armed-wait (the stop never arrived).
      // Exits before arming wrap at the plant site above. No endpoint
      // verdict, never "unreachable code"; other errors pass through
      // untouched.
      const _planted = !!(token && token.kind !== 'main-dup' && token.kind !== 'child-dup');
      if (e instanceof StopTimeout && spec !== null && e.waitContext && typeof e.waitContext === 'object') {
        const old = e.waitContext;
        e.waitContext = {
          waitStartedAt: old.waitStartedAt,
          waitedMs: old.waitedMs,
          triggerStatus: 'unknown',
          expectedBreak: spec,
          targetIdentity: old.targetIdentity,
          note: old.note || WAIT_NOTE,
        };
      }
      if (e instanceof StopTimeout && e.waitContext && typeof e.waitContext === 'object') {
        e.waitContext.captureStage = 'armed-wait-timeout';
        e.waitContext.ephemeralPlanted = _planted;
      }
      const staged = stageExit(e);
      if (staged) throw staged;
      throw e;
    }
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
    return this.resumeAndWait(timeout, tid, typeof req.target === 'string');
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
    return this.resumeAndWait(timeout, tid, typeof req.target === 'string');
  }

  /** Clear a resume flag on exactly the resumed holder (main or one
   *  worker): a timeout must never disarm another target's step. */
  clearResumeFlag(tid) {
    if (tid === 'main') {
      if (!this.paused) this.awaitingStep = false;
      return;
    }
    const w = this.workers.get(tid);
    if (w && !w.paused) w.awaitingStep = false;
  }

  /** Resume one target after step/continue and wait for a selectable
   *  stop: only the requested target's fresh park when explicit, any
   *  fresh park when omitted (stale pre-existing parks never satisfy).
   *  The response names the target that actually parked. */
  async resumeAndWait(timeout, tid = 'main', explicit = false) {
    this.pendingTarget = tid;
    try {
      let stopped;
      try {
        stopped = await this.pumpForStop(timeout, tid, explicit, false);
      } catch (e) {
        // Pump timeout (or target exit) must clear the step flag — otherwise
        // the NEXT real stop misreports as a step landing and breakpoint hits
        // misclassify. Never touch a newly landed pause: if a pause parked
        // concurrently it already consumed the flag and published.
        this.clearResumeFlag(tid);
        throw e;
      }
      const resp = await this.withTarget(stopped, async () => {
        const snap = this.snapshot();
        return {
          ok: true,
          stopped: true,
          ...this.changeFields(),
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

  /** One target's live thread dump as { target, running, threads }.
   *  Served straight from published parks (M5): never swaps shared fields,
   *  never waits behind an outstanding resume, and issues zero CDP traffic
   *  while its target has a resume outstanding (no second reader on the
   *  wire). Busy targets report the published running truth. */
  threadsDumpFor(tid) {
    if (tid === 'main') {
      if (this.exited || this.mainDead) {
        throw new BridgeErr('target main has exited — close this session');
      }
      if (this.outstanding.has('main')) {
        return { target: 'main', running: true, threads: [] };
      }
      const frames = this.framesJson(false);
      return {
        target: 'main',
        running: !this.paused,
        threads: [{ id: 1, name: 'main', status: this.paused ? 'paused' : 'running', frames }],
      };
    }
    const w = this.workers.get(tid);
    if (!w || w.exited || w.state === 'exited') {
      throw new BridgeErr(`target ${tid} has exited — close this session`);
    }
    if (w.state === 'ignored') {
      throw new BridgeErr(`target ${tid} was released (over budget)`);
    }
    if (this.outstanding.has(tid)) {
      return { target: tid, running: true, threads: [] };
    }
    const frames = this.framesJsonFor(w.paused, w.cachedLocals, false);
    return {
      target: tid,
      running: !w.paused,
      threads: [{ id: 1, name: 'worker', status: w.paused ? 'paused' : 'running', frames }],
    };
  }

  async cmdThreads(req = {}) {
    // Explicit --target X (including main) dumps that target only
    // (unchanged single-target shape). Bare threads aggregates main plus
    // every live non-ignored non-exited worker in targets/breaks order
    // (main first, then creation order); exited history is excluded.
    // Top-level running/threads stay the auto-selected target's dump and
    // the per-target entries ride additively under `targets` with a
    // `selected` stamp, so single-target sessions read byte-identical.
    // Every dump runs inside the swap mutex: the published paused/cached
    // fields must never be read mid-swap by a concurrent withTarget (the
    // dumps themselves issue no CDP, so the mutex is held microtask-
    // briefly — never over unrelated long work).
    if (req && typeof req.target === 'string') {
      const tid = this.resolveTarget(req);
      const entry = await this._swapRun(() => this.threadsDumpFor(tid));
      return this.withStamp({ ok: true, running: entry.running, threads: entry.threads }, tid);
    }
    const selected = this.resolveTarget(req);
    if (this.exited) throw new BridgeErr('target VM has exited — close this session');
    const roster = ['main'];
    for (const id of this.workers.order) {
      const w = this.workers.get(id);
      if (!w || w.exited || w.state === 'ignored' || w.state === 'exited') continue;
      if (w.state !== 'running' && w.state !== 'stopped') continue;
      roster.push(id);
    }
    if (roster.length === 1) {
      const entry = await this._swapRun(() => this.threadsDumpFor(selected));
      return this.withStamp({ ok: true, running: entry.running, threads: entry.threads }, selected);
    }
    // A target that exits or is released between the roster snapshot and
    // its dump is skipped — never failing the whole call; the survivors
    // stay attributable. Anything else (e.g. a DAP read failure) still
    // propagates.
    const entries = [];
    for (const tid of roster) {
      try {
        entries.push(await this._swapRun(() => this.threadsDumpFor(tid)));
      } catch (e) {
        if (e instanceof BridgeErr && /has exited|was released/.test(e.message)) continue;
        throw e;
      }
    }
    if (entries.length === 0) {
      // Everything churned: serve the selected target honestly (raises).
      const entry = await this._swapRun(() => this.threadsDumpFor(selected));
      return this.withStamp({ ok: true, running: entry.running, threads: entry.threads }, selected);
    }
    const sel = entries.find((e) => e.target === selected) || entries[0];
    return this.withStamp({
      ok: true, running: sel.running, threads: sel.threads,
      targets: entries, selected: sel.target,
    }, sel.target);
  }

  cmdBreaks(req = {}) {
    // Bare breaks aggregates every target: main records plus each live
    // worker's records (copies tagged with their target id). Live
    // snapshots only — exited history lives in `targets`.
    const selected = this.resolveTarget(req);
    if (this.exited) throw new BridgeErr('target VM has exited — close this session');
    const stops = this.stopStates.map((r) => ({ ...r, target: 'main' }));
    for (const id of this.workers.order) {
      const w = this.workers.get(id);
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
   *
   * Serialized on the mutation chain (concurrent identical adds converge
   * to a single record; rivals report empty-added).
   */
  async cmdBreaksAdd(req) {
    return this._mutationRun(() => this._cmdBreaksAddInner(req));
  }

  async _cmdBreaksAddInner(req) {
    const scope = req && typeof req.target === 'string' ? req.target : null;
    if (scope !== null && scope !== 'main') {
      const tid = this.resolveTarget(req);
      // Already inside the mutation chain (outer wrapper): call the inner
      // form directly — the chain is not reentrant.
      return this._addWorkerEphemeralInner(tid, req.breaks);
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
      const slidTo = slidLine(locs, f.line);
      if (slidTo !== null) {
        rec.state = 'slid';
        rec.detail = `slid to line ${slidTo}`;
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
      const ckey = `${f.path}:${f.line}|${f.cond || ''}`;
      if (!bpId) {
        // V8 explicit refusal: keyed rejected record like admission
        // (callers admit it to inheritedKeys/targetRaws, so global or
        // scoped remove/clear can eliminate it). Exactly one record per
        // canonical key — never a duplicate.
        const rec = {
          spec: this.dispSpec(f, 'break'), kind: 'break', hits: 0,
          state: 'rejected',
          detail: `breakpoint rejected: ${f.path}:${f.line}`,
        };
        w.breakKeys.set(ckey, null);
        if (!w.breakRecByKey.has(ckey)) {
          w.breakRecByKey.set(ckey, rec);
          w.stopStates.push(rec);
        }
        const entry = {
          raw: f.raw, spec: rec.spec, kind: 'break',
          state: 'rejected', hits: 0, detail: rec.detail,
        };
        added.push(entry);
        continue;
      }
      const rec = { spec: this.dispSpec(f, 'break'), kind: 'break', hits: 0 };
      this.breakIdToRec.set(bpId, { rec, line: f.line });
      w.breakKeys.set(ckey, bpId);
      w.breakRecByKey.set(ckey, rec);
      const locs = res.locations || [];
      const slidTo = slidLine(locs, f.line);
      if (slidTo !== null) {
        rec.state = 'slid';
        rec.detail = `slid to line ${slidTo}`;
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
    for (const id of this.workers.order) {
      const w = this.workers.get(id);
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
   *  global intent (no inheritance, no stops.json). Serialized on the
   *  mutation chain like global adds (capture plants funnel here too). */
  async addWorkerEphemeral(tid, raws) {
    return this._mutationRun(() => this._addWorkerEphemeralInner(tid, raws));
  }

  async _addWorkerEphemeralInner(tid, raws) {
    const w = this.workers.get(tid);
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
    const w = this.workers.get(tid);
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

  /** Every admitted worker break key: inherited copies plus ephemeral
   *  entries (logpoint plants never join — remove/clear by break key can
   *  never drop them). */
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
   *
   * Serialized on the mutation chain with add/clear (the chain is not
   * reentrant: this wrapper owns the run, the _Inner form does the work).
   */
  async cmdBreaksRemove(req) {
    return this._mutationRun(() => this._cmdBreaksRemoveInner(req));
  }

  async _cmdBreaksRemoveInner(req) {
    const scope = req && typeof req.target === 'string' ? req.target : null;
    if (scope !== null && scope !== 'main') {
      const tid = this.resolveTarget(req);
      const w = this.workers.get(tid);
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
    const { resp, confirmed } = await this.dropBreakKeys(matched, missing);
    // Propagate only confirmed main removals: a breakpoint whose backend
    // call failed keeps its intent AND its inherited copies (dropping the
    // copy while the intent stands would diverge bridge intent, backend,
    // and persistence).
    const childWarnings = [];
    for (const id of this.workers.order) {
      const w = this.workers.get(id);
      if (!w || w.exited || w.state === 'ignored') continue;
      const doomed = confirmed.filter((k) => w.inheritedKeys.has(k));
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
    return this._mutationRun(() => this._cmdBreaksClearInner(req));
  }

  async _cmdBreaksClearInner(req = {}) {
    const scope = req && typeof req.target === 'string' ? req.target : null;
    if (scope !== null && scope !== 'main') {
      const tid = this.resolveTarget(req);
      const w = this.workers.get(tid);
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
    const finishClear = async (resp, confirmedMain) => {
      // Bare clear resets ephemeral records always, plus inherited copies
      // only for confirmed main removals (a failed file keeps its intent
      // and its copies).
      const confirmedSet = new Set(confirmedMain);
      const childWarnings = [];
      for (const id of this.workers.order) {
        const w = this.workers.get(id);
        if (!w || w.exited || w.state === 'ignored') continue;
        // Admitted keys, but inherited copies only for confirmed main
        // removals (ephemeral records always reset; a failed file keeps
        // its intent and its copies).
        const doomed = this.workerAllKeys(w).filter(
          (k) => w.targetRaws.has(k) || confirmedSet.has(k));
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
      return finishClear({ ok: true, removed: [], stops: this.stopStates }, []);
    }
    const { resp, confirmed } = await this.dropBreakKeys(ordered, []);
    return finishClear(resp, confirmed);
  }

  async dropBreakKeys(keys, missing) {
    const removed = [];
    const failed = [];
    const confirmed = [];
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
      confirmed.push(key);
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
    return { resp, confirmed };
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
    if (this.server.closing) throw new BridgeErr('session is closing');
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
    this.server.markClosing();
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

  for (;;) {
    if (!amOwner(st.cfg.dir)) {
      await st.cleanup().catch(() => {});
      process.exit(0);
      return; // exit never returns; bound stubbed/embedded use instead
      // of spinning cleanup+exit forever.
    }
    if (st.server.closing) {
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
      if (st.server.closing) break;
      if (queue.length === 0 && !amOwner(st.cfg.dir)) {
        await st.cleanup().catch(() => {});
        process.exit(0);
        return; // exit never returns; bound stubbed/embedded use instead
        // of spinning cleanup+exit forever.
      }
    }
    if (st.server.closing) continue;
    const conn = queue.shift();
    if (!conn) continue;
    // Pool admission is one synchronous ServerState decision (no await
    // between check and count), so concurrent iterations never over-admit.
    // handleConn assumes the slot: it releases, never acquires.
    if (!st.server.tryAcquire(MAX_ACTIVE_HANDLERS)) {
      await handleOverload(st, conn);
      continue;
    }
    handleConn(st, conn).catch(() => {});
  }
}

/** Pool-full bypass: one bounded frame read under the existing framing
 *  limits/deadlines (never an unbounded wait). An exact `close` gets
 *  terminal close handling outside the pool — close can never be starved
 *  by admitted handlers. Anything else gets the existing overloaded
 *  rejection; malformed/timeout reads just close the socket. The pool
 *  counter is untouched (this path never counted). */
async function handleOverload(st, conn) {
  let req;
  try {
    req = await readFrame(conn); // existing 5s framing deadline
  } catch (_) {
    try {
      conn.destroy();
    } catch (_) { /* already gone */ }
    return;
  }
  if (req && typeof req === 'object' && !Array.isArray(req) && req.cmd === 'close') {
    await closeFromConn(st, conn);
    return;
  }
  try {
    await writeFrame(conn, { ok: false, error: 'overloaded: too many active handlers', target: 'main' });
  } catch (_) { /* client already gone */ }
  conn.destroy();
}

/** Serve one CLI connection: exactly one request and one response. The
 *  caller (serve) holds one acquired ServerState slot for this handler. */
async function handleConn(st, conn) {
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
    const errTarget = errorTargetFor(st, req);
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
        await closeFromConn(st, conn);
        return;
      }
      const msg = e instanceof BridgeErr ? e.message : `internal: ${(e && e.message) || e}`;
      const resp = { ok: false, error: msg, target: errTarget };
      if (e && e.waitContext && typeof e.waitContext === 'object') {
        resp.waitContext = e.waitContext;
      }
      try {
        await writeFrame(conn, resp);
      } catch (_) { /* client already gone */ }
    }
  } finally {
    conn.destroy();
    st.server.release();
  }
}

/** Request-local error attribution for one connection's envelope.
 *  Derived synchronously at frame-read time, before any concurrent await
 *  can mutate shared serving state: an explicit string target is echoed
 *  verbatim (even unknown — the envelope names what was asked); an
 *  omitted target uses this request's own resolution where available,
 *  else the legacy "main" default. Never another handler's shared
 *  pendingTarget/serving. Success stamps are untouched. */
function errorTargetFor(st, req) {
  try {
    if (req && typeof req.target === 'string') return req.target;
    try {
      return st.resolveTarget(req);
    } catch (_) { /* fall through to the default */ }
  } catch (_) { /* fall through to the default */ }
  return 'main';
}

/** Terminal close handling for one connection, outside the handler pool:
 *  every close gets the closed ACK; exactly one winner runs the teardown
 *  (ServerState.claimClose — the single-threaded check-and-set is atomic,
 *  no await between the flag read and write). The pool counter is
 *  untouched (the overload path never counted). */
async function closeFromConn(st, conn) {
  const mine = st.server.claimClose();
  try {
    await writeFrame(conn, { ok: true, closed: true, target: 'main' });
  } catch (_) { /* client already gone */ }
  if (!mine) {
    try {
      conn.destroy();
    } catch (_) { /* already gone */ }
    return;
  }
  await st.cleanup().catch(() => {});
  try {
    conn.destroy();
  } catch (_) { /* already gone */ }
}

function writeSessionFile(dir, obj, cfg, st) {
  // Main-path session.json writes carry schemaVersion 2 plus the layered
  // targetIdentity (bridge-built); explicit nulls keep readers honest.
  if (obj && typeof obj === 'object' && !('schemaVersion' in obj)) {
    obj = { ...obj, schemaVersion: 2 };
  }
  if (obj && typeof obj === 'object' && !('targetIdentity' in obj)) {
    obj = { ...obj, targetIdentity: (st && st.targetIdentity) || null };
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
    if (e instanceof Usage) { writeParseError(argv, e.message); die(e.message, 2); }
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
          }, cfg, st);
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
          }, cfg, st);
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
    }, cfg, st);
    try {
      await serve(st, server, queue);
    } finally {
      await closeServer(server);
    }
    process.exit(0);
  } catch (e) {
    // One mapping for every setup failure (see setupFailurePayload):
    // typed failures keep their message, unexpected crashes sanitize to
    // an internal error with the runtime phase. Mirror pybridge: exit
    // nonzero; session.rs surfaces error.json.
    writeFile(path.join(cfg.dir, 'error.json'), JSON.stringify(setupFailurePayload(e)));
    await st.cleanup().catch(() => {});
    try {
      server.close();
    } catch (_) { /* best effort */ }
    process.exit(e instanceof Usage ? 2 : 1);
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
