/* Browser CDP bridge for agent-debugger (B1: debug core).
 *
 * Same shape as nodebridge: a per-session daemon speaking OUR session
 * protocol (Content-Length JSON over TCP on 127.0.0.1) to the Rust CLI, and
 * raw CDP over WebSocket (via the provisioned `ws` package) to a browser
 * tab picked from /json/list. Runs on Node.
 *
 *     browserbridge.js session --kind attach --dir DIR --host H --port P
 *         [--tab SUBSTRING] [--src D]... [--break SPEC]... [--logpoint SPEC]...
 *         [--timeout S]
 *
 * Break forms: frag:line[|cond] | exc (any uncaught). `frag` matches a
 * script URL by path segment (query strings ignored), so 'app.js:12' hits
 * http://host/app.js?v=3. method:NAME, --watch and --exit have no CDP
 * equivalent and fail fast. CDP natively supports breakpoint conditions;
 * logpoints are client-side (pause, evaluate template holes, append to
 * logs.jsonl, resume).
 *
 * Tab model notes (measured):
 * - Tabs do not "exit" like processes: script end is invisible (no child
 *   to watch). Liveness = presence in /json/list (verifyTab) plus the WS
 *   staying open (tab close kills the socket -> exited).
 * - Navigation destroys execution contexts: a pause held across a reload
 *   goes stale (frames dropped, NOT reported as exit).
 * - Page console.* flows in via Runtime.consoleAPICalled -> logs.jsonl, so
 *   `logs` works with no pipes. Inspector noise filtered like the others.
 * - Snippets come from Debugger.getScriptSource (no disk access to URLs).
 *
 * Speed posture: headless-first, attach to a warm shared browser (default
 * 9222 = agent-browser convention), snapshot-first, capped lists, waits
 * bridge-side with deadlines. Evaluations retry boundedly: right after
 * navigation the default execution context is briefly unstable (measured
 * flake), so transient protocol errors retry instead of failing the stop.
 */

'use strict';
const fs = require('fs');
const http = require('http');
const net = require('net');
const os = require('os');
const path = require('path');

class Usage extends Error {}
// Wire core lives in the shared modules (single source under bridge/js/,
// provisioned next to this bridge):
//   ./cdp_conn.js  BridgeErr + CdpConn (id-matched CDP over ws)
//   ./framing.js   readFrame + writeFrame (Content-Length + JSON)
const { BridgeErr, CdpConn } = require('./cdp_conn.js');
const { readFrame, writeFrame } = require('./framing.js');
class CloseSession extends Error {}
class StopTimeout extends BridgeErr {}

const MAX_TABS_LISTED = 10;
const MAX_STRING = 200;
const MAX_FIELDS = 20;
const MAX_VARS = 20;
const MAX_FRAMES = 10;
const MAX_LOG_LINES = 2000;
const MAX_OUTPUT = 4000;
const MAX_SOURCE_CACHE = 100;
// M5 concurrency (frozen, same bounds as nodebridge): concurrent handlers
// capped (one response per connection), bounded connection queue, rival
// resume/mutation busy-rejects. Reload is a resume op.
const MAX_ACTIVE_HANDLERS = 8;
const MAX_QUEUED_CONNS = 16;
const RESUME_CMDS = new Set(['continue', 'step', 'reload']);
const WAIT_CMDS = new Set(['wait']);
const CAPTURE_CMDS = new Set(['capture']);
// Parked-stop UX: every parked response carries this warning (suspend
// semantics, HTTP handler impact). No root-cause claim, ever.
const PARK_WARNING = 'parked breakpoint suspends target; HTTP handler remains ' +
  'open until continue/capture-resume/close(detach)';
const MUTATION_CMDS = new Set(['breaksAdd', 'breaksRemove', 'breaksClear']);
const EVAL_RETRIES = 3;
const EVAL_RETRY_MS = 300;
// Inspector/page chatter (never user data, just noise in logs).
const NOISE_LINES = new Set([
  'Debugger attached.',
  'Waiting for the debugger to disconnect...',
  'For help, see: https://nodejs.org/learn/getting-started/debugging',
]);

// ---------------------------------------------------------------- framing
// (identical to nodebridge: one protocol, three bridges)



// ---------------------------------------------------------------- args

function needInt(flag, raw) {
  const n = parseInt(raw, 10);
  if (Number.isNaN(n)) throw new Usage(`${flag} needs a number (got '${raw}')`);
  return n;
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
      throw new Usage('Browser adapter stops on any uncaught exception; ' +
        `class filter unsupported: ${spec} (use bare 'exc')`);
    }
    cfg.wantExc = true;
    return;
  }
  if (head.startsWith('method:')) {
    throw new Usage(`method: breakpoints unsupported on browser yet: ${spec} (use frag:line)`);
  }
  const colon = head.lastIndexOf(':');
  if (colon <= 0) throw new Usage('--break must look like frag:line, exc');
  const lineno = parseInt(head.slice(colon + 1), 10);
  if (Number.isNaN(lineno)) throw new Usage(`bad line in --break: ${spec}`);
  const frag = normFrag(head.slice(0, colon));
  if (!frag) throw new Usage(`--break must look like frag:line, exc (got '${spec}')`);
  cfg.breaks.push({ frag, line: lineno, cond });
}

function parseLogpoint(spec, cfg) {
  const first = spec.indexOf(':');
  const second = first >= 0 ? spec.indexOf(':', first + 1) : -1;
  if (first <= 0 || second <= 0) throw new Usage('--logpoint must look like frag:line:template');
  const lineno = parseInt(spec.slice(first + 1, second), 10);
  if (Number.isNaN(lineno)) throw new Usage(`bad line in --logpoint: ${spec}`);
  const lfrag = normFrag(spec.slice(0, first));
  if (!lfrag) throw new Usage(`--logpoint must look like frag:line:template (got '${spec}')`);
  cfg.logpoints.push({
    frag: lfrag,
    line: lineno,
    template: spec.slice(second + 1),
  });
}

function parseArgs(argv) {
  // argv === process.argv.slice(2); argv[0] === "session"
  const cfg = {
    kind: 'attach', dir: null,
    host: 'localhost', port: 9222, tab: null,
    srcs: [], breaks: [], logpoints: [],
    wantExc: false, timeout: 20,
    observedTarget: null, observedHint: '', breakRaws: {},
  };
  const rest = argv;
  let i = 0;
  const need = (flag) => {
    if (i >= rest.length) throw new Usage(`missing value for ${flag}`);
    return rest[i++];
  };
  if (rest[0] !== 'session') throw new Usage('first arg must be "session"');
  i = 1;
  while (i < rest.length) {
    const a = rest[i++];
    if (a === '--kind') cfg.kind = need(a);
    else if (a === '--dir') cfg.dir = need(a);
    else if (a === '--host') cfg.host = need(a);
    else if (a === '--port') cfg.port = needInt(a, need(a));
    else if (a === '--tab') cfg.tab = need(a);
    else if (a === '--timeout') cfg.timeout = needInt(a, need(a));
    else if (a === '--src') cfg.srcs.push(need(a));
    else if (a === '--break') {
      const raw = need(a);
      const before = cfg.breaks.length;
      parseBreak(raw, cfg);
      if (cfg.breaks.length > before) {
        // Stored raw for remove/clear echo (dedup keeps first).
        const b = cfg.breaks[cfg.breaks.length - 1];
        const key = `${b.frag}:${b.line}|${b.cond || ''}`;
        if (!(key in cfg.breakRaws)) cfg.breakRaws[key] = raw;
      }
    }
    else if (a === '--logpoint') parseLogpoint(need(a), cfg);
    else if (a === '--watch') throw new Usage(`--watch has no CDP equivalent (browser): ${rest[i] || ''}`);
    else if (a === '--exit') throw new Usage(`--exit has no CDP equivalent (browser): ${rest[i] || ''}`);
    else throw new Usage(`unknown arg: ${a}`);
  }
  if (!cfg.dir) throw new Usage('missing --dir');
  if (cfg.kind !== 'attach') {
    throw new Usage(`browser is attach-only for now (got --kind ${cfg.kind}); launch lands later`);
  }
  if (!Number.isFinite(cfg.timeout) || cfg.timeout <= 0 || cfg.timeout > 3600) throw new Usage('timeout must be between 0 and 3600 seconds');
  // Fold startup specs here (fail-fast before any CDP traffic); armBreakpoints
  // re-applies the same idempotent fold as a belt-and-braces gate.
  dedupeStartupBreaks(cfg);
  return cfg;
}

// ---------------------------------------------------------------- values

function truncStr(s, limit = MAX_STRING) {
  if (s.length <= limit) return s;
  return `${s.slice(0, limit)}… (+${s.length - limit} more chars)`;
}

// Observed-identity caps (mirror the CLI contract): every string field to
// 512 chars, the whole object to 2KB serialized, same `… (+N more chars)`
// idiom — tab titles/URLs are remote-controlled text and must never bloat
// session.json or diagnostics.
const OBSERVED_FIELD_CAP = 512;
const OBSERVED_TOTAL_CAP = 2048;

function truncField(s) {
  if (s === null || s === undefined) return s;
  return truncStr(String(s), OBSERVED_FIELD_CAP);
}

// ---------------------------------------------------------------- layered target identity (M-ID)
// Additive `{debuggee, endpoint, adapter}` roles beside the untouched
// `observedTarget`. The attached tab IS the debuggee (protocol-confirmed
// via /json/list); the endpoint is the debugger listener; there is no
// adapter process and no process claim anywhere. Tab fields reuse the
// redacted + capped tab identity above; the aggregate is bounded here.
const IDENT_TOTAL_CAP = 4096;

const WAIT_NOTE = 'external trigger execution is not observed by the debugger; ' +
  'this timeout means no stop was observed, not that the code is unreachable';

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
    const cand = truncStr(cur, Math.max(1, hit.len - 64));
    setAt(obj, hit.path, cand.length < hit.len ? cand : '…');
  }
  return obj;
}

/** Secret query keys (mirrors the CLI argv redaction): whole/substring
 *  long keys plus boundary/suffix short keys — never a mere prefix, so
 *  `?author=` keeps its value while `?token=` and `?apiToken=` redact.
 *  Short-key set matches Rust is_secret_flag exactly (token/auth/pwd/pass
 *  /pw as whole tokens or suffixes); `?passage=`, `?author=` and `?passed=`
 *  never redact. */
function isSecretQueryKey(key) {
  const flat = key.toLowerCase().replace(/[-_]/g, '');
  const SUBSTR = ['password', 'passwd', 'secret', 'apikey', 'authorization', 'authtoken', 'accesstoken'];
  if (SUBSTR.some((k) => flat.includes(k))) return true;
  const TOKEN = ['token', 'auth', 'pwd', 'pass', 'pw'];
  return key.toLowerCase().split(/[-_.]/).some((tok) => {
    if (tok.length === 0) return false;
    return TOKEN.some((k) => tok === k || tok.endsWith(k));
  });
}

/** Redact secret query values before anything persists or prints. Falls
 *  back to a pattern rewrite when the string is not a parseable URL. */
function redactUrl(raw) {
  if (raw === null || raw === undefined) return raw;
  const s = String(raw);
  try {
    const u = new URL(s);
    let touched = false;
    for (const key of Array.from(u.searchParams.keys())) {
      if (isSecretQueryKey(key)) {
        u.searchParams.set(key, '[redacted]');
        touched = true;
      }
    }
    return touched ? u.toString() : s;
  } catch (_) {
    return s.replace(/([?&][^?&=#]*=)[^&#]*/g, (m, head) => {
      const name = head.slice(1, -1);
      return isSecretQueryKey(name) ? `${head.slice(0, 1)}${name}=[redacted]` : m;
    });
  }
}

/** Tab identity for session.json/status (no process claim). Redacted and
 *  capped before return — raw tab query secrets never persist or print. */
function buildObservedTab(tab, host, port, nowSec) {
  const t = tab || {};
  const obs = {
    kind: 'tab',
    url: truncField(redactUrl(t.url || null)),
    title: truncField(redactUrl(t.title || null)),
    targetId: truncField(redactUrl(t.id || null)),
    debugEndpoint: truncField(`${host}:${port}`),
    cwd: null,
    argv: null,
    notApplicable: ['cwd', 'argv'],
    source: 'cdp-target-list',
    observedAt: nowSec,
    unavailable: [],
    warnings: [],
  };
  // Total cap: shrink the longest free-text field until the object fits.
  for (;;) {
    if (JSON.stringify(obs).length <= OBSERVED_TOTAL_CAP) break;
    const lens = [['url', obs.url], ['title', obs.title]]
      .filter(([, v]) => typeof v === 'string' && v.length > 1)
      .sort((a, b) => b[1].length - a[1].length);
    if (lens.length === 0) break;
    const [field, val] = lens[0];
    obs[field] = truncStr(val, Math.max(1, val.length - 64));
  }
  return obs;
}

/** Compact tab hint for timeout diagnostics (already redacted + capped). */
function tabHint(obs) {
  const s = `target identity: tab ${(obs && obs.url) || '?'}`;
  return s.length <= 200 ? s : `${s.slice(0, 200)}`;
}

function escapeRegex(s) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/** Match a script URL by path segment, ignoring query strings and hashes:
 * 'app.js' hits http://h/app.js?v=3 but not http://h/myapp.js. */
function fragRegex(frag) {
  return `(^|/)${escapeRegex(frag)}([?#]|$)`;
}

/** Normalize a URL frag: strip leading slashes so '/app.js' matches like
 *  'app.js' (a leading slash would otherwise demand a '//' in the URL and
 *  never match). A frag of only slashes fails fast at the call site. */
function normFrag(frag) {
  return frag.replace(/^\/+/, '');
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

/** Display form of a script URL: origin + path, no query/hash noise. */
function displayUrl(url) {
  if (!url) return '?';
  try {
    const u = new URL(url);
    return u.origin === 'null' ? u.pathname : u.origin + u.pathname;
  } catch (_) {
    return url;
  }
}

function tabSummary(t) {
  // Display-only (pick errors, CDP connect errors): secret query values
  // redacted, then capped — raw matching in pickTab still sees the real
  // URL/title, but nothing raw ever prints or persists.
  const tab = t || {};
  return `${truncStr(String(redactUrl(tab.title || '(no title)')), 60)} ` +
    `<${truncStr(String(redactUrl(tab.url || '')), 100)}>`;
}

/** Fold startup specs before any CDP traffic (mirrors nodebridge + the live
 *  `breaks add` contract, keyed on URL frag): exact same frag/line/cond
 *  repeats are idempotent; the same frag/line with a different condition —
 *  or any same-line logpoint — fails fast. */
function dedupeStartupBreaks(cfg) {
  const kept = [];
  const seen = new Set();
  for (const b of cfg.breaks) {
    const key = `${b.frag}:${b.line}|${b.cond || ''}`;
    if (seen.has(key)) continue; // exact duplicate: idempotent
    const other = kept.find((o) => o.frag === b.frag && o.line === b.line);
    if (other) {
      throw new Usage(`conflicting condition for ${b.frag}:${b.line} ` +
        `(already requested${other.cond ? ` as '${other.cond}'` : ' plain'}): ` +
        `${b.frag}:${b.line}${b.cond ? `|${b.cond}` : ''}`);
    }
    seen.add(key);
    kept.push(b);
  }
  cfg.breaks = kept;
  const keptLogs = [];
  const seenLogs = new Set();
  for (const l of cfg.logpoints) {
    const key = `${l.frag}:${l.line}|${l.template}`;
    if (seenLogs.has(key)) continue; // exact duplicate: idempotent
    const brk = cfg.breaks.find((b) => b.frag === l.frag && b.line === l.line);
    if (brk) {
      throw new Usage(`conflicting condition for ${l.frag}:${l.line} ` +
        `(already requested as breakpoint${brk.cond ? ` '${brk.cond}'` : ''}): ` +
        `logpoint ${l.frag}:${l.line}`);
    }
    const other = keptLogs.find((o) => o.frag === l.frag && o.line === l.line);
    if (other) {
      throw new Usage(`conflicting condition for ${l.frag}:${l.line} ` +
        `(already requested as logpoint): logpoint ${l.frag}:${l.line}`);
    }
    seenLogs.add(key);
    keptLogs.push(l);
  }
  cfg.logpoints = keptLogs;
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

// ---------------------------------------------------------------- tabs

async function listTargets(host, port) {
  const url = `http://${host}:${port}/json/list`;
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
    throw new BridgeErr(`attach failed (${host}:${port}): ${detail} — ` +
      `is Chrome running with --remote-debugging-port=${port} ? ` +
      `(agent-browser users: point it at the same port with --cdp ${port})`);
  });
  let targets;
  try {
    targets = JSON.parse(body);
  } catch (_) {
    throw new BridgeErr(`attach failed: ${url} did not return a target list`);
  }
  return targets || [];
}

function pickTab(targets, want) {
  const pages = targets.filter((t) => t.type === 'page' && t.webSocketDebuggerUrl);
  if (pages.length === 0) {
    throw new BridgeErr('attach failed: no open page tabs (only background/service targets visible)');
  }
  if (!want) return pages[0];
  const hits = pages.filter((t) => (t.url || '').includes(want) || (t.title || '').includes(want));
  if (hits.length === 1) return hits[0];
  const listed = pages.slice(0, MAX_TABS_LISTED).map((t) => `  - ${tabSummary(t)}`).join('\n');
  // The selector is our own caller-supplied text, but it can still carry a
  // pasted URL with a secret query — redact it like every other display
  // string. Matching above stays raw on purpose.
  const safeWant = String(redactUrl(want));
  if (hits.length === 0) {
    throw new BridgeErr(`attach failed: no tab matches '${safeWant}'. Open tabs:\n${listed}`);
  }
  throw new BridgeErr(`attach failed: '${safeWant}' matches ${hits.length} tabs, be specific:\n` +
    hits.slice(0, MAX_TABS_LISTED).map((t) => `  - ${tabSummary(t)}`).join('\n'));
}

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

const EXITED_MSG = 'target tab has closed — close this session';

class Session {
  constructor(cfg) {
    this.cfg = cfg;
    this.tab = null;      // picked /json/list entry
    this.cdp = null;
    this.scripts = new Map(); // scriptId -> url
    this.urls = new Map();    // url -> scriptId
    this.sources = new Map(); // scriptId -> source lines (capped)
    this.defaultContextId = null;
    this.logpointIds = new Map(); // breakpointId -> template
    this.breakIdToRec = new Map(); // breakpointId -> {rec, line} for resolve upgrades
    this.breakKeys = new Map(); // `frag:line|cond` -> breakpointId (null when rejected)
    this.breakRecByKey = new Map(); // `frag:line|cond` -> live break rec (incl. rejected)
    this.breakRaws = new Map(Object.entries((cfg && cfg.breakRaws) || {})); // key -> stored raw
    this.shadowedLogs = []; // {frag, line, template} startup logpoints shadowed by a break
    this.stopStates = []; // arm-time records served by `breaks`
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
    this.sessionPort = 0; // our TCP port (set in main, for republishing)
    this.lastStop = null; // {file,line,method} of the latest stop
    // -- stop diagnostics (UX batch): session-monotonic stop id plus the
    // previous park for same-location/same-thread diagnosis. Capture does
    // not survive a reload (navigation drops CDP breakpoints); a reload
    // between plant and park surfaces as a timeout, never a stale resume.
    this.stopDiagSeq = 0; // session-monotonic stop id
    this.prevPark = null; // previous park {target,file,line,threadId,atMs}
    this.stopReason = null; // reason of the current park
    this.stopHitBps = null; // native hitBreakpoint ids of the current park
    this.parkedAtMs = 0; // wall clock ms of the current park
    this.lastDiag = null; // {target,stopId,sameLocation,sameThread,elapsedMs,atMs}
    // -- M5 concurrency: the single outstanding resume op (tid is always
    // 'main': one tab per session); live reads bypass it entirely.
    this.outstanding = new Map(); // tid -> resume cmd in flight
    this.activeConns = 0;         // live connection handlers (bounded)
    // -- layered target identity (M-ID): the attached tab (debuggee,
    // protocol-confirmed) plus the debugger listener (endpoint). Built at
    // handshake; published redacted + capped in session.json.
    this.targetIdentity = null;   // {debuggee,endpoint,adapter} or null
    this.identityHint = '';       // debuggee-first one-liner for timeouts
  }

  /** M5 immediate busy rejection (single target): a second resume or any
   *  breakpoint mutation while one is outstanding never silently queues;
   *  eval is exclusive (it can mutate) and busy-rejects even when parked
   *  frames exist. Live reads and frame-bound context/vars/stack never
   *  busy-reject (the latter fail fast via requireStopped once the resume
   *  publishes running). */
  busyError(cmd) {
    if (this.outstanding.size === 0) return null;
    // wait never resumes but still occupies the slot (a rival resume would
    // steal the stop it long-polls for); capture resumes at the end, so it
    // occupies the slot throughout.
    if (RESUME_CMDS.has(cmd) || WAIT_CMDS.has(cmd) || CAPTURE_CMDS.has(cmd) ||
        MUTATION_CMDS.has(cmd) || cmd === 'eval') {
      const first = [...this.outstanding.keys()].sort()[0];
      return `busy: ${this.outstanding.get(first)} outstanding for ${first}`;
    }
    return null;
  }

  clearOutstanding(cmd, tid) {
    if (this.outstanding.get(tid) === cmd) this.outstanding.delete(tid);
  }

  // -- attach lifecycle

  async handshake() {
    const targets = await listTargets(this.cfg.host, this.cfg.port);
    this.tab = pickTab(targets, this.cfg.tab);
    // Tab identity (no process claim): the /json/list entry we attached
    // to, persisted redacted into session.json and surfaced in
    // status/context. cwd/argv are not applicable to tabs. Immutable for
    // the session (handshake-time only, never re-probed per command).
    this.cfg.observedTarget = buildObservedTab(
      this.tab, this.cfg.host, this.cfg.port, Math.floor(Date.now() / 1000));
    this.cfg.observedHint = tabHint(this.cfg.observedTarget);
    this.buildTargetIdentity();
    const WebSocket = loadWs();
    const ws = new WebSocket(this.tab.webSocketDebuggerUrl, { maxPayload: 256 * 1024 * 1024 });
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
      throw new BridgeErr(`CDP connect failed (tab '${tabSummary(this.tab)}'): ${e.message}`);
    });
    // Tab close kills the socket -> session exit (liveness beyond that is
    // verifyTab's job against /json/list).
    this.cdp = new CdpConn(ws, () => {
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
    await this.cdp.request('Debugger.enable');
    // Runtime.enable arms consoleAPICalled (page console -> logs) and the
    // execution-context events (navigation bookkeeping below).
    await this.cdp.request('Runtime.enable');
    await this.armBreakpoints();
    if (this.cfg.wantExc) {
      await this.cdp.request('Debugger.setPauseOnExceptions', { state: 'uncaught' });
    }
  }

  dispSpec(b, kind) {
    // Display form for `breaks`: url frag + line (+ cond), template for
    // logpoints. Mirrors nodebridge/pybridge reconstructed specs.
    let s = `${b.frag}:${b.line}`;
    if (kind === 'break' && b.cond) s += `|${b.cond}`;
    return s;
  }

  async armBreakpoints() {
    // Startup fold (same contract as nodebridge + live `breaks add`): an
    // exact same frag/line/cond repeat is idempotent (kept once); the same
    // frag/line with a different condition — or any same-line logpoint
    // (one V8 breakpoint per line wins) — fails fast BEFORE any CDP
    // setBreakpointByUrl traffic, instead of silently overwriting.
    dedupeStartupBreaks(this.cfg);
    // Same collision rule as nodebridge: a real break and a logpoint on the
    // same line — the break wins, the logpoint is reported and skipped.
    // Every requested stop lands in this.stopStates (served by `breaks`)
    // — verification used to live only in stderr, invisible after
    // compaction.
    const byLine = new Map(); // `${frag}:${line}` -> {break, logpoint}
    for (const b of this.cfg.breaks) {
      byLine.set(`${b.frag}:${b.line}`, { ...(byLine.get(`${b.frag}:${b.line}`) || {}), brk: b });
    }
    for (const l of this.cfg.logpoints) {
      const key = `${l.frag}:${l.line}`;
      if (byLine.has(key) && byLine.get(key).brk) {
        const msg = `logpoint shadowed by breakpoint: ${l.frag}:${l.line}`;
        process.stderr.write(`warn: ${msg}\n`);
        this.stopStates.push({
          spec: this.dispSpec(l, 'logpoint'), kind: 'logpoint',
          state: 'shadowed', detail: msg, hits: 0,
        });
        // Retained for remove-time re-arm (the break wins; no plant exists).
        this.shadowedLogs.push({ frag: l.frag, line: l.line, template: l.template });
        continue;
      }
      byLine.set(key, { ...(byLine.get(key) || {}), log: l });
    }
    for (const [, item] of byLine) {
      const spec = item.brk || item.log;
      const kind = item.brk ? 'break' : 'logpoint';
      const params = {
        urlRegex: fragRegex(spec.frag),
        lineNumber: spec.line - 1,
      };
      if (item.brk && item.brk.cond) params.condition = item.brk.cond;
      const res = await this.cdp.request('Debugger.setBreakpointByUrl', params);
      const bpId = res.breakpointId;
      const rec = { spec: this.dispSpec(spec, kind), kind, hits: 0 };
      if (kind === 'logpoint') rec.detail = spec.template;
      if (kind === 'break') {
        this.breakKeys.set(`${spec.frag}:${spec.line}|${spec.cond || ''}`, bpId || null);
        this.breakRecByKey.set(`${spec.frag}:${spec.line}|${spec.cond || ''}`, rec);
      }
      if (!bpId) {
        const msg = `breakpoint rejected: ${spec.frag}:${spec.line}`;
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
      if (locs.length === 0) {
        process.stderr.write(`warn: breakpoint unverified (pending): ${spec.frag}:${spec.line}\n`);
      }
      const slidTo = slidLine(locs, spec.line);
      if (slidTo !== null) {
        // V8 slides breakpoints off non-executable lines to the next
        // statement — say so loudly instead of debugging the wrong line.
        process.stderr.write(
          `warn: breakpoint slid: ${spec.frag}:${spec.line} -> ${slidTo}\n`);
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
   *  step landings normally carry none, so they don't inflate counters. */
  countHits(p) {
    for (const id of p.hitBreakpoints || []) {
      const entry = this.breakIdToRec.get(id);
      if (entry && typeof entry.rec.hits === 'number') entry.rec.hits += 1;
    }
    if (p.reason === 'exception') {
      for (const rec of this.stopStates) {
        if (rec.kind === 'exc' && typeof rec.hits === 'number') rec.hits += 1;
      }
    }
  }

  /** Re-resolve our tab against the live target list (B0 contract). */
  async verifyTab() {
    let targets;
    try {
      targets = await listTargets(this.cfg.host, this.cfg.port);
    } catch (e) {
      throw new BridgeErr(`browser gone (${this.cfg.host}:${this.cfg.port} unreachable) — close this session`);
    }
    const still = (targets || []).find(
      (t) => t.id === (this.tab && this.tab.id) && t.webSocketDebuggerUrl);
    if (!still) {
      throw new BridgeErr('tab closed (or navigated beyond reach) — close this session');
    }
    this.tab = still;
  }

  tabJson() {
    // Served in `threads` (and snapshots via threadsJson): redacted and
    // capped like the persisted observed identity — never raw tab text.
    const t = this.tab || {};
    const safe = (v) => (v === null || v === undefined) ? '?' : truncField(redactUrl(v));
    return { id: safe(t.id), title: safe(t.title), url: safe(t.url) };
  }

  /** Build the layered {debuggee, endpoint, adapter} identity from the
   *  attached tab (protocol-confirmed debuggee) plus the debugger listener
   *  (endpoint). The tab fields reuse the redacted + capped tab identity;
   *  the aggregate is bounded here. The `observedTarget` view is untouched. */
  buildTargetIdentity() {
    const now = Math.floor(Date.now() / 1000);
    const obs = (this.cfg && this.cfg.observedTarget) || {};
    // -- debuggee: the attached tab itself (no process claim, ever).
    const debuggee = {
      kind: 'tab',
      url: (typeof obs.url === 'string') ? obs.url : null,
      title: (typeof obs.title === 'string') ? obs.title : null,
      targetId: (typeof obs.targetId === 'string') ? obs.targetId : null,
      source: 'cdp-target-list',
      confidence: 'protocol-confirmed',
      observedAt: now,
      unavailable: [],
      notApplicable: ['cwd', 'argv', 'pid', 'executable'],
    };
    if (debuggee.url === null && debuggee.title === null && debuggee.targetId === null) {
      debuggee.confidence = 'unavailable';
      debuggee.source = null;
      debuggee.unavailable.push({ field: 'url', reason: 'no /json/list entry observed' });
    }
    // -- endpoint: the debugger listener (host/port the CLI attached to).
    // No OS listener observation exists for browser targets, so the owner
    // stays honestly unavailable.
    const endpoint = {
      host: this.cfg.host || 'localhost',
      port: (Number.isInteger(this.cfg.port)) ? this.cfg.port : null,
      debugEndpoint: (typeof obs.debugEndpoint === 'string') ? obs.debugEndpoint : null,
      role: 'debugger listener (Chrome remote-debugging port)',
      source: null,
      confidence: 'unavailable',
      observedAt: now,
      unavailable: [{ field: 'ownerPid', reason: 'no OS listener observation for browser targets' }],
    };
    // -- adapter: CDP attaches directly to the tab — no adapter process.
    const adapter = {
      confidence: 'unavailable',
      reason: 'no adapter process; CDP attaches directly to the tab',
      observedAt: now,
      unavailable: [],
      notApplicable: ['pid', 'argv', 'cwd', 'executable'],
    };
    this.targetIdentity = identShrinkToTotal({ debuggee, endpoint, adapter }, IDENT_TOTAL_CAP);
    // Debuggee-first one-liner for timeout diagnostics (concise, no
    // root-cause claim); falls back to the CLI hint when unknown.
    let hint = '';
    if (debuggee.confidence === 'protocol-confirmed') {
      hint = `debuggee: tab ${debuggee.url || debuggee.title || '?'} (protocol-confirmed)`;
    }
    if (!hint) hint = this.cfg.observedHint || '';
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

  // -- events (always live: logpoints auto-fire even between commands)

  async handleEvent(msg) {
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
      const p = msg.params || {};
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
      return;
    }
    if (msg.method === 'Runtime.executionContextCreated') {
      const ctx = (msg.params && msg.params.context) || {};
      if (ctx.auxData && ctx.auxData.isDefault) this.defaultContextId = ctx.id;
      return;
    }
    if (msg.method === 'Runtime.executionContextsCleared') {
      // Reload/navigation wiped every context: any held stop's frames are
      // stale (their scriptIds mean nothing now). Drop the stop; the next
      // command reports "no stopped thread" instead of failing on dead
      // callFrameIds. Liveness stays verifyTab's job.
      this.paused = null;
      this.defaultContextId = null;
      this.publishState(false);
      return;
    }
    if (msg.method === 'Runtime.executionContextDestroyed') {
      // A single context went away (worker/prerender teardown, or script
      // end in exotic cases). If it was ours, drop possibly-stale frames.
      // NOTE: an open page's main script end is NOT observable, so
      // continue-to-end just burns its timeout (pass a short one unless
      // interaction may still stop it). Liveness stays verifyTab's.
      if ((msg.params || {}).executionContextId === this.defaultContextId) {
        this.paused = null;
        this.defaultContextId = null;
        this.cachedLocals = [];
        this.publishState(false);
      }
      return;
    }
    if (msg.method === 'Runtime.consoleAPICalled') {
      this.onConsole(msg.params || {});
      return;
    }
    if (msg.method === 'Debugger.paused') {
      await this.onPaused(msg.params || {});
      return;
    }
    if (msg.method === 'Debugger.resumed') {
      return;
    }
  }

  onConsole(p) {
    const parts = [];
    for (const a of p.args || []) {
      if (a.value !== undefined) {
        parts.push(typeof a.value === 'string' ? a.value : JSON.stringify(a.value));
      } else if (a.description !== undefined) {
        parts.push(a.description);
      } else {
        parts.push(a.type || '?');
      }
    }
    const line = parts.join(' ');
    if (line) {
      this.outputTail = (this.outputTail + line + '\n').slice(-MAX_OUTPUT * 2);
      this.appendLog(line);
    }
  }

  async onPaused(p) {
    if (this.closing || this.exited || this.paused) {
      // Single target: a second pause cannot arrive while one is held (the
      // page is frozen). If it ever does, hold the first stop; the next
      // resume flushes the rest. The park below is set SYNCHRONOUSLY
      // (before the first await), so even a fire-and-forget duplicate
      // landing in the same tick sees it and never clobbers the stop.
      return;
    }
    const hits = p.hitBreakpoints || [];
    const frames = p.callFrames || [];
    const logHits = hits.filter((id) => this.logpointIds.has(id));
    const realHits = hits.filter((id) => !this.logpointIds.has(id));
    this.countHits(p);
    if (p.reason === 'exception') {
      this.stopInfo = this.excInfo(p.data);
      // Park first, synchronously — trackChanges awaits must never strand
      // a CDP pause with a running state when they throw.
      this.paused = { frames, stopInfo: this.stopInfo };
      this.notePark('main', p);
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
      this.notePark('main', p);
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
    // Stray instrumentation pause: resume.
    await this.cdp.request('Debugger.resume').catch(() => {});
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
          const res = await this.evalFrame(frameId, m[1], true);
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

  /** evaluateOnCallFrame with bounded retries. Right after navigation the
   * default execution context is briefly unstable (measured flake): a
   * protocol error retries, a real evaluation failure (exceptionDetails)
   * returns immediately. */
  async evalFrame(callFrameId, expression, returnByValue) {
    let last = null;
    for (let k = 0; k < EVAL_RETRIES; k++) {
      try {
        return await this.cdp.request('Debugger.evaluateOnCallFrame', {
          callFrameId, expression, returnByValue,
        });
      } catch (e) {
        last = e;
        await sleep(EVAL_RETRY_MS);
      }
    }
    throw last;
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
    // One physical line per entry: multi-line values would shatter structure.
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
    // Timeout message with the compact identity hint (debuggee-first: the
    // tab, never claims root cause).
    let msg = `timeout: no stop within ${fmtTimeout(timeout)}`;
    const hint = this.identityHint || this.cfg.observedHint;
    if (hint) msg += `; ${hint}`;
    return msg;
  }

  async pump(timeout, withWaitContext = false) {
    const startedAt = Date.now();
    const deadline = startedAt + timeout * 1000;
    for (;;) {
      if (!amOwner(this.cfg.dir)) {
        await this.cleanup().catch(() => {});
        process.exit(0);
      }
      if (this.paused) return 'stopped';
      if (this.exited) throw new BridgeErr(EXITED_MSG);
      if (Date.now() > deadline) {
        const err = new StopTimeout(this.timeoutText(timeout));
        // wait/capture only (never continue/step/reload): the honest
        // trigger-unknown context rides structurally; prefix unchanged.
        if (withWaitContext) err.waitContext = this.waitContext(timeout, startedAt);
        throw err;
      }
      await sleep(50);
    }
  }

  /** Trimmed stop locator for session.json (no snippet — source fetches
   *  stay in locationJson()). Mirrors its file/line/method. */
  lastStopJson() {
    const frames = (this.paused && this.paused.frames) || [];
    if (frames.length === 0) return null;
    const f = frames[0];
    return {
      file: this.relFile(this.frameUrl(f)),
      line: (f.location && f.location.lineNumber + 1) || -1,
      method: f.functionName || '(anonymous)',
    };
  }

  /** Rewrite session.json so `status` shows live truth (parked stop +
   *  time) with zero prior memory. lastStop survives resume/exit — it
   *  answers 'where was I last', not 'where am I now'. The redacted
   *  observedTarget (tab identity) rides along verbatim. */
  publishState(stopped) {
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
      targetIdentity: this.targetIdentity || null,
    }));
  }

  // -- snapshot builders (Java/Python/Node shapes)

  relFile(url) {
    // Browser files are URLs, not disk paths: --src maps a URL prefix to a
    // local dir for parity display (B2 grows this into source maps). Until
    // then, show origin+path without query noise.
    if (!url || url === '?') return '?';
    return displayUrl(url);
  }

  async scriptLines(scriptId) {
    if (this.sources.has(scriptId)) return this.sources.get(scriptId);
    let lines = [];
    try {
      const res = await this.cdp.request('Debugger.getScriptSource', { scriptId });
      lines = String(res.scriptSource || '').split('\n');
    } catch (_) {
      lines = [];
    }
    if (this.sources.size >= MAX_SOURCE_CACHE) {
      const first = this.sources.keys().next();
      if (!first.done) this.sources.delete(first.value);
    }
    this.sources.set(scriptId, lines);
    return lines;
  }

  async snippet(scriptId, line) {
    if (!line || line < 1) return [];
    const lines = await this.scriptLines(scriptId);
    if (lines.length === 0) return [];
    const out = [];
    for (let n = Math.max(1, line - 5); n <= Math.min(lines.length, line + 5); n++) {
      out.push({ line: n, current: n === line, text: lines[n - 1] });
    }
    return out;
  }

  frameUrl(frame) {
    if (frame.url) return frame.url;
    return this.scripts.get(frame.location && frame.location.scriptId) || '';
  }

  async locationJson() {
    const frames = (this.paused && this.paused.frames) || [];
    if (frames.length === 0) {
      return { class: '?', method: '?', line: -1, file: '?', snippet: [] };
    }
    const f = frames[0];
    const url = this.frameUrl(f);
    const line = (f.location && f.location.lineNumber + 1) || -1;
    let base = '?';
    try {
      const u = new URL(url);
      const seg = u.pathname.split('/').filter(Boolean).pop() || '';
      base = seg.split('.')[0] || '?';
    } catch (_) {
      base = (url.split('/').pop() || '?').split('.')[0] || '?';
    }
    const scriptId = f.location && f.location.scriptId;
    return {
      class: base,
      method: f.functionName || '(anonymous)',
      line,
      file: this.relFile(url),
      snippet: scriptId ? await this.snippet(scriptId, line) : [],
    };
  }

  threadsJson() {
    const t = this.tab || {};
    return [{
      id: 1,
      name: truncStr(String(redactUrl(t.title || 'tab')), 60),
      current: true,
    }];
  }

  async framesJson(withLocals) {
    const frames = ((this.paused && this.paused.frames) || []).slice(0, MAX_FRAMES);
    const out = [];
    for (let i = 0; i < frames.length; i++) {
      const f = frames[i];
      const entry = {
        index: i,
        type: '?',
        method: f.functionName || '(anonymous)',
        line: (f.location && f.location.lineNumber + 1) || -1,
      };
      if (withLocals && i === 0) entry.locals = this.cachedLocals || [];
      out.push(entry);
    }
    return out;
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
    // Same merge as nodebridge: local/block/closure/module/catch/script,
    // innermost wins.
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
    const res = await this.cdp.request('Runtime.getProperties', {
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
    // One-level expansion for objects (mirrors the other bridges).
    if (!v || v.type !== 'object' || !v.objectId) return this.fmtRemote(v);
    if (v.subtype === 'null') return 'null';
    let res;
    try {
      res = await this.cdp.request('Runtime.getProperties', {
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

  async snapshot() {
    return {
      mode: 'session',
      location: await this.locationJson(),
      threads: this.threadsJson(),
      frames: await this.framesJson(true),
      output: this.outputTail.slice(-MAX_OUTPUT),
    };
  }

  // -- commands

  requireStopped() {
    if (this.exited) throw new BridgeErr(EXITED_MSG);
    if (!this.paused) throw new BridgeErr('no stopped thread (target is running — continue first)');
    if (((this.paused && this.paused.frames) || []).length === 0) {
      throw new BridgeErr('no stopped thread yet in this session');
    }
  }

  requireLive() {
    if (this.exited) throw new BridgeErr(EXITED_MSG);
  }

  async cmdContext() {
    this.requireStopped();
    const location = await this.locationJson();
    const threads = this.threadsJson();
    return {
      ok: true,
      stopInfo: JSON.parse(this.stopInfo || 'null'),
      location,
      threads,
      frames: await this.framesJson(true),
      diag: this.stopDiag(threads),
      warning: PARK_WARNING,
    };
  }

  async cmdStack() {
    this.requireStopped();
    return { ok: true, frames: await this.framesJson(false) };
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
      throw new BridgeErr('refs() unsupported on browser yet (no gc walk via CDP)');
    }
    let res;
    try {
      res = await this.evalFrame(frames[frame].callFrameId, expr, false);
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

  /** Record one genuine park for stop diagnostics (synchronous at
   *  the park). Session-monotonic stopId plus previous-park comparison.
   *  hitBreakpoints ride natively (never fabricated). Location resolves
   *  lazily at response time (frameUrl needs no traffic). */
  notePark(tid, p) {
    const now = Date.now();
    const prev = this.prevPark;
    const elapsed = prev ? now - prev.atMs : null;
    // Location at park time, best-effort without traffic: top-frame url +
    // line (relFile is pure string work).
    let file = '?', line = -1;
    try {
      const frames = (this.paused && this.paused.frames) || [];
      if (frames.length > 0) {
        const f = frames[0];
        file = this.relFile(this.frameUrl(f));
        line = (f.location && f.location.lineNumber + 1) || -1;
      }
    } catch (_) { file = '?'; line = -1; }
    const sameLoc = !!(prev && prev.file === file && prev.line === line);
    const sameThr = !!(prev && prev.target === tid && prev.threadId === 1);
    this.stopDiagSeq += 1;
    this.prevPark = { target: tid, file, line, threadId: 1, atMs: now };
    this.stopReason = (p && p.reason) || null;
    this.stopHitBps = (p && Array.isArray(p.hitBreakpoints)) ? [...p.hitBreakpoints] : null;
    this.parkedAtMs = now;
    this.lastDiag = {
      target: tid, stopId: this.stopDiagSeq,
      sameLocation: sameLoc, sameThread: sameThr, elapsedMs: elapsed, atMs: now,
    };
  }

  /** Additive stop diagnostics for the parked tab. requested/bound
   *  resolve via native hit ids when attributable, else null. */
  stopDiag(threads) {
    let name = null;
    try {
      const hit = (threads || []).find((t) => t.id === 1);
      if (hit) name = hit.name || null;
    } catch (_) { name = null; }
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
      stopId: null, parkedAtMs: this.parkedAtMs, target: 'main',
      reason: this.stopReason, stoppingThread: { id: 1, name },
      hitBreakpoints: this.stopHitBps,
      requestedBreak: requested, boundLine: bound, hitCount,
      sameLocation: false, sameThread: false, elapsedSincePreviousStopMs: null,
    };
    const last = this.lastDiag;
    if (last && last.target === 'main') {
      diag.stopId = last.stopId;
      diag.sameLocation = !!last.sameLocation;
      diag.sameThread = !!last.sameThread;
      diag.elapsedSincePreviousStopMs = last.elapsedMs;
    }
    return diag;
  }

  /** Parked wait response (issues zero resume traffic by construction). */
  async waitSnapshot(waited) {
    const snapshot = await this.snapshot();
    return {
      ok: true, stopped: true, waited, target: 'main',
      changed: JSON.parse(this.lastChanged),
      stopInfo: JSON.parse(this.stopInfo || 'null'),
      snapshot, diag: this.stopDiag(snapshot.threads),
      warning: PARK_WARNING,
    };
  }

  /** Pure long-poll: NEVER resumes. Immediate success when the tab is
   *  already parked; otherwise waits for the next fresh stop. Timeout
   *  preserves session/intents (typed message). */
  async cmdWait(req, timeout) {
    this.requireLive();
    if (this.paused) return this.waitSnapshot(false);
    await this.pump(timeout, true);
    return this.waitSnapshot(true);
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
      throw new BridgeErr('capture --break must look like frag:line');
    }
    return { frames, vars, budget, spec };
  }

  /** Capture snapshot: frames 1..10, frame-0 vars 1..20. */
  async boundedSnapshot(framesN, varsN) {
    const frames = ((this.paused && this.paused.frames) || []).slice(0, framesN);
    const total = ((this.paused && this.paused.frames) || []).length;
    const out = [];
    for (let i = 0; i < frames.length; i++) {
      const f = frames[i];
      const entry = {
        index: i, type: '?',
        method: f.functionName || '(anonymous)',
        line: (f.location && f.location.lineNumber + 1) || -1,
      };
      if (i === 0) {
        try {
          const full = await this.frameLocalsIn(frames, 0);
          entry.locals = full.slice(0, varsN);
          entry._varsTruncated = full.length > varsN;
        } catch (_) { entry.locals = []; entry._varsTruncated = false; }
      }
      out.push(entry);
    }
    const varsTruncated = out.length > 0 ? !!out[0]._varsTruncated : false;
    if (out.length > 0) delete out[0]._varsTruncated;
    return {
      snapshot: {
        mode: 'session', location: await this.locationJson(),
        threads: this.threadsJson(), frames: out,
        output: this.outputTail.slice(-MAX_OUTPUT),
      },
      framesTruncated: total > framesN, varsTruncated,
    };
  }

  /** Plant one ephemeral frag:line break for a capture. Returns a removal
   *  token ({kind:'dup'} when the exact line is already armed). Raises
   *  BEFORE anything parks on invalid/conflicting specs. Never touches
   *  the global intent, stops.json, or inheritance. Does not survive a
   *  reload: a navigation between plant and park surfaces as a timeout,
   *  never a stale resume. */
  async capturePlant(spec) {
    const scratch = { breaks: [], logpoints: [], wantExc: false };
    const bar = spec.indexOf('|');
    const head = bar < 0 ? spec : spec.slice(0, bar);
    if (head === 'exc' || head.startsWith('exc:') || head.startsWith('method:')) {
      throw new BridgeErr(`capture takes line breaks only (got '${spec}')`);
    }
    try {
      parseBreak(spec, scratch);
    } catch (e) {
      throw new BridgeErr(e instanceof Usage ? e.message : String((e && e.message) || e));
    }
    const b = scratch.breaks[0];
    const key = `${b.frag}:${b.line}|${b.cond || ''}`;
    const loc = `${b.frag}:${b.line}`;
    if (this.breakKeys.has(key)) return { kind: 'dup' };
    for (const k of this.breakKeys.keys()) {
      if (k.slice(0, k.lastIndexOf('|')) === loc) {
        throw new BridgeErr(`conflicting condition for ${b.frag}:${b.line} (already armed): ${spec}`);
      }
    }
    for (const l of this.cfg.logpoints || []) {
      if (`${l.frag}:${l.line}` === loc) {
        throw new BridgeErr(`conflicting condition for ${b.frag}:${b.line} (already armed as logpoint): ${spec}`);
      }
    }
    const params = { urlRegex: fragRegex(b.frag), lineNumber: b.line - 1 };
    if (b.cond) params.condition = b.cond;
    let res;
    try {
      res = await this.cdp.request('Debugger.setBreakpointByUrl', params, 5000);
    } catch (e) {
      throw new BridgeErr(`capture break failed to plant: ${(e && e.message) || e}`);
    }
    const bpId = res && res.breakpointId;
    if (!bpId) {
      throw new BridgeErr(`capture break failed to plant: ${b.frag}:${b.line}`);
    }
    // Hit-attribution only (never in stopStates/cfg: `breaks` and
    // stops.json never see the ephemeral).
    const rec = { spec: this.dispSpec(b, 'break'), kind: 'break', hits: 0 };
    this.breakIdToRec.set(bpId, { rec, line: b.line });
    return { kind: 'planted', bpId };
  }

  /** Remove a capture ephemeral BEFORE resume. Throws on failure (the
   *  caller still resumes, then reports removeError). */
  async captureUnplant(token) {
    if (!token || token.kind === 'dup') return;
    if (token.kind !== 'planted') throw new BridgeErr(`bad capture token: ${token.kind}`);
    try {
      await this.cdp.request('Debugger.removeBreakpoint', { breakpointId: token.bpId }, 5000);
    } finally {
      this.breakIdToRec.delete(token.bpId);
    }
  }

  /** One-shot bounded stop. Pre-parked tab: collect WITHOUT resuming.
   *  Fresh park: collect, REMOVE EPHEMERAL BEFORE RESUME, auto-resume
   *  within the pause budget (overrun still resumes, then reports). Any
   *  collection/removal failure still resumes; timeout never resumes
   *  (nothing parked). No eval, no persisted vars. */
  async cmdCapture(req, timeout) {
    const { frames, vars, budget, spec } = this.captureBounds(req);
    await this.verifyTab();
    this.requireLive();
    if (this.paused) {
      const { snapshot, framesTruncated, varsTruncated } =
        await this.boundedSnapshot(frames, vars);
      return {
        ok: true, stopped: true, target: 'main',
        targetWasPaused: true, resumed: false,
        pauseDurationMs: 0, pauseBudgetMs: budget, ephemeralPlanted: false,
        truncated: { frames: framesTruncated, vars: varsTruncated },
        snapshot, diag: this.stopDiag(snapshot.threads),
        warning: PARK_WARNING,
      };
    }
    let token = null;
    if (spec !== null) token = await this.capturePlant(spec);
    try {
      await this.pump(timeout, true);
    } catch (e) {
      // Timeout/exit/reload: nothing parked by us — no resume — but the
      // ephemeral must not leak.
      try {
        await this.captureUnplant(token);
      } catch (ue) {
        throw new BridgeErr(`${(e && e.message) || e}; capture ephemeral may still be planted (breaks remove to clear)`);
      }
      // The planted spec rides the honest timeout context (in canonical
      // field order); other errors pass through untouched.
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
      throw e;
    }
    const parkMs = (this.lastDiag && this.lastDiag.target === 'main' && this.lastDiag.atMs) || Date.now();
    let snapErr = null, removeErr = null, resumeErr = null;
    let snapshot, framesTruncated = false, varsTruncated = false;
    try {
      ({ snapshot, framesTruncated, varsTruncated } =
        await this.boundedSnapshot(frames, vars));
    } catch (e) {
      snapErr = String((e && e.message) || e);
      snapshot = {
        mode: 'session', location: await this.locationJson(), threads: [],
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
          await this.cdp.request('Debugger.resume');
        } catch (e) {
          if (!this.paused) {
            this.paused = saved;
            this.publishState(true);
          }
          throw e;
        }
      }
      if (!this.paused) this.awaitingStep = false;
      resumed = true;
    } catch (e) { resumeErr = String((e && e.message) || e); }
    const pauseMs = Date.now() - parkMs;
    let diag;
    try {
      diag = this.stopDiag((snapshot && snapshot.threads) || []);
    } catch (_) { diag = { target: 'main' }; }
    diag.pauseDurationMs = pauseMs;
    diag.targetWasPaused = false;
    diag.resumed = resumed;
    const resp = {
      ok: true, stopped: true, target: 'main',
      targetWasPaused: false, resumed,
      pauseDurationMs: pauseMs, pauseBudgetMs: budget,
      budgetExceeded: pauseMs > budget,
      ephemeralPlanted: !!(token && token.kind === 'planted'),
      truncated: { frames: savedCount > frames || framesTruncated, vars: varsTruncated },
      snapshot, diag, warning: PARK_WARNING,
    };
    if (snapErr !== null) resp.snapshotError = snapErr;
    if (removeErr !== null) resp.removeError = removeErr;
    if (resumeErr !== null) resp.resumeError = resumeErr;
    return resp;
  }

  async cmdStep(req, timeout) {
    this.requireLive();
    // Stepping needs a stopped frame to step from (uniform contract on
    // all bridges); continuing works from running (it just waits).
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
      await this.cdp.request(method);
    } catch (e) {
      // Synchronous request failure: restore the park (unless a fresh pause
      // already won) so the session file stops lying about running, and
      // clear the flag so the next real stop does not misreport as a step.
      this.awaitingStep = false;
      if (!this.paused) {
        this.paused = saved;
        this.publishState(true);
      }
      throw e;
    }
    return this.resumeAndWait(timeout);
  }

  async cmdContinue(req, timeout) {
    this.requireLive();
    if (this.paused) {
      // Publish running BEFORE the resume request (same ordering as step).
      const saved = this.paused;
      this.paused = null;
      this.cachedLocals = [];
      this.publishState(false);
      try {
        await this.cdp.request('Debugger.resume');
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
    return this.resumeAndWait(timeout);
  }

  async cmdReload(req, timeout) {
    // Agent-side trigger for load-path code: reload the tab and wait for
    // the next stop. Interaction-path code still needs a human click or
    // agent-browser.
    this.requireLive();
    this.paused = null;
    this.cachedLocals = [];
    await this.cdp.request('Page.reload', {});
    // Nothing that can stop: a bare reload is just a refresh — return
    // fast instead of burning the timeout waiting for a stop that cannot
    // come. Logpoints don't count (they auto-resume and never park).
    if (this.cfg.breaks.length === 0 && !this.cfg.wantExc) {
      return { ok: true, reloaded: true };
    }
    this.publishState(false);
    return this.resumeAndWait(timeout);
  }

  async resumeAndWait(timeout) {
    try {
      await this.pump(timeout);
    } catch (e) {
      // Pump timeout (or tab exit) must clear the step flag — otherwise the
      // NEXT real stop misreports as a step landing. Never touch a newly
      // landed pause: if onPaused parked concurrently it already consumed
      // the flag and published.
      if (!this.paused) this.awaitingStep = false;
      throw e;
    }
    const snapshot = await this.snapshot();
    return {
      ok: true,
      stopped: true,
      target: 'main',
      changed: JSON.parse(this.lastChanged),
      stopInfo: JSON.parse(this.stopInfo || 'null'),
      snapshot,
      diag: this.stopDiag(snapshot.threads),
      warning: PARK_WARNING,
    };
  }

  async cmdThreads() {
    await this.verifyTab();
    if (this.exited) throw new BridgeErr(EXITED_MSG);
    const frames = await this.framesJson(false);
    const out = {
      ok: true,
      running: !this.paused,
      threads: [{ id: 1, name: truncStr(String(redactUrl((this.tab && this.tab.title) || 'tab')), 60), status: this.paused ? 'paused' : 'running', frames, tab: this.tabJson() }],
    };
    // Idle-but-armed sessions would otherwise leave agents guessing why
    // nothing stops: report what's armed (breaks + logpoints + exc flag).
    const armed = this.cfg.breaks.length + this.cfg.logpoints.length + (this.cfg.wantExc ? 1 : 0);
    if (!this.paused && armed > 0) out.armed = armed;
    return out;
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

  async cmdBreaks() {
    // Arm-time records: what was requested and whether it planted
    // (verified/pending/slid/shadowed) — no round-trip needed, no stop
    // required. verifyTab first: a dead tab's records would lie.
    await this.verifyTab();
    if (this.exited) throw new BridgeErr(EXITED_MSG);
    return { ok: true, stops: this.stopStates };
  }

  /**
   * Additive line breaks on a live tab (running or parked — never
   * suspended/resumed here). The whole batch validates first (parse,
   * canonical dedup/conflict incl. same-frag-line startup logpoints) with
   * no CDP traffic, then each fresh break installs via setBreakpointByUrl
   * (<=5s). Exact canonical duplicates are idempotent (added empty); the
   * same line with a different condition — or any same-line logpoint —
   * rejects the batch before anything mutates.
   */
  async cmdBreaksAdd(req) {
    await this.verifyTab();
    if (this.exited) throw new BridgeErr(EXITED_MSG);
    const raws = req.breaks;
    if (!Array.isArray(raws) || raws.length === 0) {
      throw new BridgeErr('breaks add needs at least one --break');
    }
    for (const r of raws) {
      if (typeof r !== 'string' || !r) throw new BridgeErr(`bad break spec: ${JSON.stringify(r)}`);
    }
    const scratch = { breaks: [], logpoints: [], wantExc: false };
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
    const canonKey = (frag, line, cond) => `${frag}:${line}|${cond || ''}`;
    // Armed lines: breaks carry their cond, logpoints always conflict.
    const armed = new Map(); // `${frag}:${line}` -> {cond, kind}
    for (const b of this.cfg.breaks) {
      armed.set(`${b.frag}:${b.line}`, { cond: b.cond || null, kind: 'break' });
    }
    for (const l of this.cfg.logpoints) {
      if (!armed.has(`${l.frag}:${l.line}`)) {
        armed.set(`${l.frag}:${l.line}`, { cond: null, kind: 'logpoint' });
      }
    }
    const conflictDetail = (o) => o.kind === 'logpoint'
      ? 'as logpoint'
      : (o.cond ? `as '${o.cond}'` : 'plain');
    const seen = new Set();
    const batchLoc = new Map();
    const fresh = []; // {raw, frag, line, cond}
    for (let i = 0; i < raws.length; i++) {
      const b = scratch.breaks[i];
      const key = canonKey(b.frag, b.line, b.cond);
      if (seen.has(key)) continue; // intra-batch duplicate: idempotent
      seen.add(key);
      const loc = `${b.frag}:${b.line}`;
      if (armed.has(loc)) {
        const o = armed.get(loc);
        if (o.kind === 'break' && (o.cond || null) === (b.cond || null)) continue; // idempotent
        throw new BridgeErr(`conflicting condition for ${b.frag}:${b.line} ` +
          `(already armed ${conflictDetail(o)}): ${raws[i]}`);
      }
      if (batchLoc.has(loc) && (batchLoc.get(loc) || null) !== (b.cond || null)) {
        throw new BridgeErr(`conflicting condition for ${b.frag}:${b.line} (same batch): ${raws[i]}`);
      }
      batchLoc.set(loc, b.cond || null);
      fresh.push({ raw: raws[i], frag: b.frag, line: b.line, cond: b.cond || null });
    }
    if (fresh.length === 0) return { ok: true, added: [], stops: this.stopStates };
    const added = [];
    const failed = [];
    for (const f of fresh) {
      const params = { urlRegex: fragRegex(f.frag), lineNumber: f.line - 1 };
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
      this.breakRecByKey.set(`${f.frag}:${f.line}|${f.cond || ''}`, rec);
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
      this.cfg.breaks.push({ frag: f.frag, line: f.line, cond: f.cond });
      this.breakKeys.set(`${f.frag}:${f.line}|${f.cond || ''}`, bpId);
      if (!this.breakRaws.has(`${f.frag}:${f.line}|${f.cond || ''}`)) {
        this.breakRaws.set(`${f.frag}:${f.line}|${f.cond || ''}`, f.raw);
      }
      const entry = { raw: f.raw, spec: rec.spec, kind: 'break', state: rec.state, hits: 0 };
      if (rec.detail) entry.detail = rec.detail;
      added.push(entry);
    }
    if (added.length === 0) {
      throw new BridgeErr(`breaks add failed for ${failed.length} break(s): ` +
        failed.map((f) => `${f.frag}:${f.line}`).join(', '));
    }
    const resp = { ok: true, added, stops: this.stopStates };
    if (failed.length > 0) {
      resp.warning = 'partial add: no change for ' +
        failed.map((f) => `${f.frag}:${f.line}`).join(', ');
    }
    return resp;
  }

  /** Lexical remove-spec parse (add normalization minus frag existence:
   *  the script may be gone and removal still works). */
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
    const frag = normFrag(head.slice(0, colon));
    if (!frag) throw new BridgeErr(`bad break spec: ${JSON.stringify(raw)}`);
    return { frag, line: lineno, cond };
  }

  breakKeyOf(b) {
    return `${b.frag}:${b.line}|${b.cond || ''}`;
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

  /**
   * Remove live line breaks by stored identity (running or parked).
   * Phase 1 matches the whole batch with zero CDP traffic (unmatched specs
   * land in `missing`, never ok:false); phase 2 drops each confirmed break
   * via Debugger.removeBreakpoint. `removed[]` echoes the persisted stored
   * raws. A removed break re-arms a same-line shadowed startup logpoint via
   * the normal logpoint path (or records pending + warning). Reload never
   * restores removed breaks (plants come from cfg, which already dropped
   * them).
   */
  async cmdBreaksRemove(req) {
    await this.verifyTab();
    if (this.exited) throw new BridgeErr(EXITED_MSG);
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
    if (matched.length === 0) return { ok: true, removed: [], missing, stops: this.stopStates };
    return this.dropBreakKeys(matched, missing);
  }

  async cmdBreaksClear() {
    await this.verifyTab();
    if (this.exited) throw new BridgeErr(EXITED_MSG);
    const ordered = [];
    for (const b of this.cfg.breaks) {
      const key = this.breakKeyOf(b);
      if (this.breakRaws.has(key) && !ordered.includes(key)) ordered.push(key);
    }
    for (const key of this.breakRaws.keys()) {
      if (!ordered.includes(key)) ordered.push(key);
    }
    if (ordered.length === 0) return { ok: true, removed: [], stops: this.stopStates };
    return this.dropBreakKeys(ordered, []);
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
      const kfrag = loc.slice(0, cpos);
      const kline = parseInt(loc.slice(cpos + 1), 10);
      const kcond = sep < 0 ? null : (key.slice(sep + 1) || null);
      const bi = this.cfg.breaks.findIndex((b) => b.frag === kfrag && b.line === kline && (b.cond || null) === kcond);
      if (bi >= 0) this.cfg.breaks.splice(bi, 1);
      if (rec) {
        const ri = this.stopStates.indexOf(rec);
        if (ri >= 0) this.stopStates.splice(ri, 1);
      }
      removed.push({ raw: storedRaw, spec: rec ? rec.spec : loc, kind: 'break', hits: 0 });
      await this.rearmShadowedLogpoint(kfrag, kline);
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
   *  break won; no plant exists). Failures record pending + warning —
   *  never a silent resurrection. */
  async rearmShadowedLogpoint(kfrag, kline) {
    const idx = this.shadowedLogs.findIndex((s) => s.frag === kfrag && s.line === kline);
    if (idx < 0) return;
    const shadow = this.shadowedLogs[idx];
    this.shadowedLogs.splice(idx, 1);
    let res;
    try {
      res = await this.cdp.request('Debugger.setBreakpointByUrl', {
        urlRegex: fragRegex(shadow.frag), lineNumber: shadow.line - 1,
      }, 5000);
    } catch (e) {
      this.stopStates.push({
        spec: `${shadow.frag}:${shadow.line}`, kind: 'logpoint',
        state: 'pending', detail: `${shadow.template} (re-arm failed: ${(e && e.message) || e})`, hits: 0,
      });
      return;
    }
    const bpId = res.breakpointId;
    if (!bpId) {
      this.stopStates.push({
        spec: `${shadow.frag}:${shadow.line}`, kind: 'logpoint',
        state: 'pending', detail: `${shadow.template} (re-arm rejected)`, hits: 0,
      });
      return;
    }
    this.logpointIds.set(bpId, shadow.template);
    const rec = {
      spec: `${shadow.frag}:${shadow.line}`, kind: 'logpoint',
      detail: shadow.template, hits: 0,
    };
    const locs = res.locations || [];
    rec.state = locs.length > 0 ? 'verified' : 'pending';
    if (rec.state === 'pending') rec.detail = `${shadow.template} (no locations yet)`;
    this.breakIdToRec.set(bpId, { rec, line: shadow.line });
    this.stopStates.push(rec);
    this.cfg.logpoints.push({ frag: shadow.frag, line: shadow.line, template: shadow.template });
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
    // immediate busy rejection and resume registration BEFORE any CDP
    // traffic.
    const busy = this.busyError(cmd);
    if (busy) throw new BridgeErr(busy);
    let resumeTid = null;
    if (RESUME_CMDS.has(cmd) || WAIT_CMDS.has(cmd) || CAPTURE_CMDS.has(cmd)) {
      this.outstanding.set('main', cmd);
      resumeTid = 'main';
    }
    try {
      if (cmd === 'context') return await this.cmdContext();
      if (cmd === 'stack') return await this.cmdStack();
      if (cmd === 'vars') return await this.cmdVars(req);
      if (cmd === 'eval') return await this.cmdEval(req);
      if (cmd === 'step') return await this.cmdStep(req, timeout);
      if (cmd === 'continue') return await this.cmdContinue(req, timeout);
      if (cmd === 'wait') return await this.cmdWait(req, timeout);
      if (cmd === 'capture') return await this.cmdCapture(req, timeout);
      if (cmd === 'reload') return await this.cmdReload(req, timeout);
      if (cmd === 'threads') return await this.cmdThreads();
      if (cmd === 'breaks') return await this.cmdBreaks();
      if (cmd === 'breaksAdd') return await this.cmdBreaksAdd(req);
      if (cmd === 'breaksRemove') return await this.cmdBreaksRemove(req);
      if (cmd === 'breaksClear') return await this.cmdBreaksClear();
      if (cmd === 'logs') return this.cmdLogs(req);
      throw new BridgeErr(`unknown cmd: ${cmd}`);
    } finally {
      if (resumeTid !== null) this.clearOutstanding(cmd, resumeTid);
    }
  }

  async cleanup() {
    this.closing = true;
    // Attach semantics: detach only, the tab keeps running.
    if (this.cdp && !this.cdp.closed) {
      try {
        await this.cdp.request('Debugger.disable', {}, 3000);
      } catch (_) { /* best effort */ }
      this.cdp.close();
    }
  }
}

function fmtTimeout(t) {
  return `${t}s`;
}

// ---------------------------------------------------------------- serve
// (same shape as nodebridge, including the permanent connection queue:
// Node emits 'connection' eagerly, even with no listener attached.)

async function serve(st, server, queue) {
  // M5: connections are handled concurrently (one handler per connection,
  // at most MAX_ACTIVE_HANDLERS; overflow is an immediate rejection, never
  // an unbounded spawn). Live reads serve published state while a resume
  // is outstanding; rivals busy-reject in dispatch. A client disconnect
  // drops only its own response.
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
          resp.target = 'main';
        }
        try {
          await writeFrame(conn, resp);
        } catch (_) { /* client went away mid-command: work already ran */ }
      } catch (e) {
        if (e instanceof CloseSession) {
          // Terminal and accepted despite any outstanding resume: never
          // wait for a handler that itself awaits a stop — detach now.
          // The serve loop below gives in-flight handlers a bounded grace
          // to flush their aborts.
          try {
            await writeFrame(conn, { ok: true, closed: true, target: 'main' });
          } catch (_) { /* client already gone */ }
          st.closing = true;
          await st.cleanup().catch(() => {});
          return;
        }
        const msg = e instanceof BridgeErr ? e.message : `internal: ${(e && e.message) || e}`;
        const resp = { ok: false, error: msg, target: 'main' };
        if (e && e.waitContext && typeof e.waitContext === 'object') {
          resp.waitContext = e.waitContext;
        }
        try {
          await writeFrame(conn, resp);
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

function writeSessionFile(dir, obj, cfg, st) {
  // Main-path session.json writes carry the tab observed identity plus the
  // layered targetIdentity; explicit null keeps legacy readers honest.
  if (obj && typeof obj === 'object' && !('observedTarget' in obj)) {
    obj = { ...obj, observedTarget: (cfg && cfg.observedTarget) || null };
  }
  if (obj && typeof obj === 'object' && !('targetIdentity' in obj)) {
    obj = { ...obj, targetIdentity: (st && st.targetIdentity) || null };
  }
  writeFile(path.join(dir, 'session.json'), JSON.stringify(obj));
}

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
  process.stderr.write(`browserbridge: ${msg}${os.EOL}`);
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
    // No startup pump (unlike node/java): an attached tab is usually idle
    // (its load-path code already ran), so waiting here would burn the full
    // timeout on nearly every attach. Breaks stay ARMED for the future;
    // pauses land via events and surface on the next command (context,
    // threads, step, continue) or on `reload`, which re-triggers load-path
    // code. The agent loop: attach --break (fast) -> reload -> stop.
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
  } catch (e) {
    if (e instanceof Usage || e instanceof BridgeErr) {
      writeFile(path.join(cfg.dir, 'error.json'), JSON.stringify({ error: e.message }));
      await st.cleanup().catch(() => {});
      try {
        server.close();
      } catch (_) { /* best effort */ }
      process.exit(e instanceof Usage ? 2 : 1);
    }
    // Unexpected setup crash (never a silent exit-1): same detach, then a
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

main(process.argv.slice(2)).catch((e) => die(`internal: ${(e && e.stack) || e}`, 1));
