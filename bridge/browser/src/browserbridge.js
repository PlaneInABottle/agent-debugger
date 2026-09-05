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

const MAX_TABS_LISTED = 10;
const MAX_STRING = 200;
const MAX_FIELDS = 20;
const MAX_VARS = 20;
const MAX_FRAMES = 10;
const MAX_LOG_LINES = 2000;
const MAX_OUTPUT = 4000;
const MAX_SOURCE_CACHE = 100;
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
  cfg.breaks.push({ frag: head.slice(0, colon), line: lineno, cond });
}

function parseLogpoint(spec, cfg) {
  const first = spec.indexOf(':');
  const second = first >= 0 ? spec.indexOf(':', first + 1) : -1;
  if (first <= 0 || second <= 0) throw new Usage('--logpoint must look like frag:line:template');
  const lineno = parseInt(spec.slice(first + 1, second), 10);
  if (Number.isNaN(lineno)) throw new Usage(`bad line in --logpoint: ${spec}`);
  cfg.logpoints.push({
    frag: spec.slice(0, first),
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
    else if (a === '--break') parseBreak(need(a), cfg);
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

function escapeRegex(s) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/** Match a script URL by path segment, ignoring query strings and hashes:
 * 'app.js' hits http://h/app.js?v=3 but not http://h/myapp.js. */
function fragRegex(frag) {
  return `(^|/)${escapeRegex(frag)}([?#]|$)`;
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
  return `${truncStr(t.title || '(no title)', 60)} <${truncStr(t.url || '', 100)}>`;
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
  if (hits.length === 0) {
    throw new BridgeErr(`attach failed: no tab matches '${want}'. Open tabs:\n${listed}`);
  }
  throw new BridgeErr(`attach failed: '${want}' matches ${hits.length} tabs, be specific:\n` +
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
  }

  // -- attach lifecycle

  async handshake() {
    const targets = await listTargets(this.cfg.host, this.cfg.port);
    this.tab = pickTab(targets, this.cfg.tab);
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
      const slidLoc = (locs.map((l) => l.lineNumber + 1).find((n) => n !== spec.line));
      const slidTo = (slidLoc === undefined) ? null : slidLoc;
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
    const t = this.tab || {};
    return { id: t.id || '?', title: t.title || '?', url: t.url || '?' };
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

  async pump(timeout) {
    const deadline = Date.now() + timeout * 1000;
    for (;;) {
      if (!amOwner(this.cfg.dir)) {
        await this.cleanup().catch(() => {});
        process.exit(0);
      }
      if (this.paused) return 'stopped';
      if (this.exited) throw new BridgeErr(EXITED_MSG);
      if (Date.now() > deadline) {
        throw new BridgeErr(`timeout: no stop within ${fmtTimeout(timeout)}`);
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
   *  answers 'where was I last', not 'where am I now'. */
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
    return [{ id: 1, name: t.title || 'tab', current: true }];
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
    return {
      ok: true,
      stopInfo: JSON.parse(this.stopInfo || 'null'),
      location: await this.locationJson(),
      threads: this.threadsJson(),
      frames: await this.framesJson(true),
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
    return {
      ok: true,
      stopped: true,
      changed: JSON.parse(this.lastChanged),
      stopInfo: JSON.parse(this.stopInfo || 'null'),
      snapshot: await this.snapshot(),
    };
  }

  async cmdThreads() {
    await this.verifyTab();
    if (this.exited) throw new BridgeErr(EXITED_MSG);
    const frames = await this.framesJson(false);
    const out = {
      ok: true,
      running: !this.paused,
      threads: [{ id: 1, name: (this.tab && this.tab.title) || 'tab', status: this.paused ? 'paused' : 'running', frames, tab: this.tabJson() }],
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
      this.cfg.breaks.push({ frag: f.frag, line: f.line, cond: f.cond });
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

  async dispatch(req) {
    const cmd = req.cmd;
    let timeout = Number(req.timeout !== undefined ? req.timeout : this.cfg.timeout);
    if (!Number.isFinite(timeout) || timeout <= 0 || timeout > 3600) throw new BridgeErr('timeout must be between 0 and 3600 seconds');
    if (cmd === 'close') throw new CloseSession();
    if (cmd === 'context') return await this.cmdContext();
    if (cmd === 'stack') return await this.cmdStack();
    if (cmd === 'vars') return await this.cmdVars(req);
    if (cmd === 'eval') return await this.cmdEval(req);
    if (cmd === 'step') return await this.cmdStep(req, timeout);
    if (cmd === 'continue') return await this.cmdContinue(req, timeout);
    if (cmd === 'reload') return await this.cmdReload(req, timeout);
    if (cmd === 'threads') return await this.cmdThreads();
    if (cmd === 'breaks') return await this.cmdBreaks();
    if (cmd === 'breaksAdd') return await this.cmdBreaksAdd(req);
    if (cmd === 'logs') return this.cmdLogs(req);
    throw new BridgeErr(`unknown cmd: ${cmd}`);
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
  return String(t);
}

// ---------------------------------------------------------------- serve
// (same shape as nodebridge, including the permanent connection queue:
// Node emits 'connection' eagerly, even with no listener attached.)

async function serve(st, server, queue) {
  for (;;) {
    if (!amOwner(st.cfg.dir)) {
      await st.cleanup().catch(() => {});
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
      if (queue.length === 0 && !amOwner(st.cfg.dir)) {
        await st.cleanup().catch(() => {});
        process.exit(0);
      }
    }
    const conn = queue.shift();
    try {
      let req;
      try {
        req = await readFrame(conn);
      } catch (e) {
        try {
          await writeFrame(conn, { ok: false, error: String((e && e.message) || e) });
        } catch (_) { /* client already gone — nothing to answer */ }
        continue;
      }
      try {
        await writeFrame(conn, await st.dispatch(req));
      } catch (e) {
        if (e instanceof CloseSession) {
          try {
            await writeFrame(conn, { ok: true, closed: true });
          } catch (_) { /* client already gone */ }
          await st.cleanup();
          return;
        }
        const msg = e instanceof BridgeErr ? e.message : `internal: ${(e && e.message) || e}`;
        try {
          await writeFrame(conn, { ok: false, error: msg });
        } catch (_) { /* client already gone */ }
      }
    } finally {
      conn.destroy();
    }
  }
}

function writeSessionFile(dir, obj) {
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
    });
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
