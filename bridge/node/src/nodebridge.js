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
  cfg.breaks.push({ path: canon(head.slice(0, colon)), line: lineno, cond });
}

function parseLogpoint(spec, cfg) {
  const first = spec.indexOf(':');
  const second = first >= 0 ? spec.indexOf(':', first + 1) : -1;
  if (first <= 0 || second <= 0) throw new Usage('--logpoint must look like path:line:template');
  const lineno = parseInt(spec.slice(first + 1, second), 10);
  if (Number.isNaN(lineno)) throw new Usage(`bad line in --logpoint: ${spec}`);
  cfg.logpoints.push({
    path: canon(spec.slice(0, first)),
    line: lineno,
    template: spec.slice(second + 1),
  });
}

function parseArgs(argv) {
  // argv === process.argv.slice(2); argv[0] === "session"
  const cfg = {
    kind: 'launch', dir: null, program: null, nodeBin: 'node',
    host: 'localhost', port: 9229, srcs: [], breaks: [], logpoints: [],
    wantExc: false, timeout: 20, programArgs: [],
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
    if (a === '--') { cfg.programArgs = rest.slice(i); break; }
    else if (a === '--kind') cfg.kind = need(a);
    else if (a === '--dir') cfg.dir = need(a);
    else if (a === '--program') cfg.program = path.resolve(need(a));
    else if (a === '--node') cfg.nodeBin = need(a);
    else if (a === '--host') cfg.host = need(a);
    else if (a === '--port') cfg.port = needInt(a, need(a));
    else if (a === '--src') cfg.srcs.push(path.resolve(need(a)));
    else if (a === '--break') parseBreak(need(a), cfg);
    else if (a === '--logpoint') parseLogpoint(need(a), cfg);
    else if (a === '--watch') throw new Usage(`--watch has no CDP equivalent yet (Node): ${rest[i] || ''}`);
    else if (a === '--exit') throw new Usage(`--exit has no CDP equivalent yet (Node): ${rest[i] || ''}`);
    else if (a === '--timeout') cfg.timeout = needInt(a, need(a));
    else throw new Usage(`unknown arg: ${a}`);
  }
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
    // node --check does not apply type-stripping, so it false-positives on
    // valid TypeScript — skip pre-validation there (runtime failures still
    // surface via logs/exit, just without the precise syntax message).
    if (!/\.[mc]?ts$/.test(cfg.program)) {
      let check;
      try {
        check = spawnSync(cfg.nodeBin, ['--check', cfg.program], { encoding: 'utf-8' });
      } catch (e) {
        throw new BridgeErr(`cannot run ${cfg.nodeBin} --check: ${(e && e.message) || e}`);
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
  try {
    fs.writeFileSync(p, content);
  } catch (_) { /* best effort */ }
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
    this.stopStates = []; // arm-time records served by `breaks`
    this.sessionPort = 0; // our TCP port (set in main, for republishing)
    this.lastStop = null; // {file,line,method} of the latest stop
  }

  // -- target lifecycle

  async startTarget() {
    const stderrBuf = [];
    let wsUrl = null;
    this.child = spawn(this.cfg.nodeBin,
      [`--inspect-brk=127.0.0.1:0`, this.cfg.program, ...this.cfg.programArgs],
      { stdio: ['ignore', 'pipe', 'pipe'] });
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
    const dead = new Promise((resolve) => this.child.once('exit', (code) => resolve(code)));
    const deadline = Date.now() + 20000;
    for (;;) {
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
    if (msg.method === 'Runtime.executionContextDestroyed') {
      // Main script ended (process lingers while the inspector is
      // attached). Surface as exited so stops/threads behave like pybridge.
      if ((msg.params || {}).executionContextId === this.defaultContextId) {
        this.markExited();
      }
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

  async onPaused(p) {
    if (this.closing || this.exited || this.paused) {
      // Single target: a second pause cannot arrive while one is held (the
      // target is frozen). If it ever does, hold the first stop; the next
      // resume flushes the rest.
      return;
    }
    const hits = p.hitBreakpoints || [];
    const frames = this.userFrames(p.callFrames || []);
    const logHits = hits.filter((id) => this.logpointIds.has(id));
    const realHits = hits.filter((id) => !this.logpointIds.has(id));
    this.countHits(p);
    for (const id of logHits) {
      await this.fireLogpoint(id, frames);
    }
    if (p.reason === 'exception') {
      this.stopInfo = this.excInfo(p.data);
      await this.trackChanges(frames);
      this.paused = { frames, stopInfo: this.stopInfo };
      this.publishState(true);
      return;
    }
    if (realHits.length > 0 || this.awaitingStep) {
      this.awaitingStep = false;
      this.stopInfo = null;
      await this.trackChanges(frames);
      this.paused = { frames, stopInfo: null };
      this.publishState(true);
      return;
    }
    if (logHits.length > 0) {
      await this.cdp.request('Debugger.resume').catch(() => {});
      return;
    }
    // Entry pause (--inspect-brk) or stray instrumentation pause: resume.
    await this.cdp.request('Debugger.resume').catch(() => {});
  }

  /** Drop node:internal frames (justMyCode spirit). Falls back to the full
   *  stack when filtering would leave nothing (e.g. stepped into loader). */
  userFrames(frames) {
    const kept = frames.filter((f) => {
      const url = f.url || this.scripts.get(f.location && f.location.scriptId) || '';
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
          const res = await this.cdp.request('Debugger.evaluateOnCallFrame', {
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
    if (this.logCount >= MAX_LOG_LINES) return;
    // One physical line per entry: multi-line values (e.g. Error stacks
    // from a logpoint hole) would otherwise shatter logs.jsonl structure.
    const flat = String(line).replace(/\r?\n/g, '⏎');
    try {
      fs.appendFileSync(path.join(this.cfg.dir, 'logs.jsonl'), flat + '\n');
      this.logCount += 1;
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
      if (this.exited) throw new BridgeErr('target exited');
      if (Date.now() > deadline) {
        throw new StopTimeout(`timeout: no stop within ${fmtTimeout(timeout)}`);
      }
      await sleep(50);
    }
  }

  markExited() {
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

  frameUrl(frame) {
    if (frame.url) return frame.url;
    return this.scripts.get(frame.location && frame.location.scriptId) || '';
  }

  fileOf(frame) {
    const url = this.frameUrl(frame);
    if (!url) return '?';
    try {
      if (url.startsWith('file://')) return path.normalize(decodeURI(new URL(url).pathname));
    } catch (_) { /* fall through */ }
    return url;
  }

  locationJson() {
    const frames = (this.paused && this.paused.frames) || [];
    if (frames.length === 0) {
      return { class: '?', method: '?', line: -1, file: '?', snippet: [] };
    }
    const f = frames[0];
    const file = this.fileOf(f);
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
    const frames = ((this.paused && this.paused.frames) || []).slice(0, MAX_FRAMES);
    return frames.map((f, i) => {
      const entry = {
        index: i,
        type: '?',
        method: f.functionName || '(anonymous)',
        line: (f.location && f.location.lineNumber + 1) || -1,
      };
      if (withLocals && i === 0) entry.locals = this.cachedLocals || [];
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
    // variables live in 'closure' scopes. Merge all four kinds in chain
    // order, innermost name wins (an arrow stopped at its first line would
    // otherwise show a lying empty locals list).
    const seen = new Set();
    let props = [];
    for (const s of chain) {
      if (s.type !== 'local' && s.type !== 'block' && s.type !== 'closure' && s.type !== 'module') continue;
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
    // One-level expansion for objects (mirrors pybridge opaque-repr rule).
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
    if (this.exited) throw new BridgeErr('target VM has exited — close this session');
    if (!this.paused) throw new BridgeErr('no stopped thread (target is running — continue first)');
    if (((this.paused && this.paused.frames) || []).length === 0) {
      throw new BridgeErr('no stopped thread yet in this session');
    }
  }

  requireLive() {
    if (this.exited) throw new BridgeErr('target VM has exited — close this session');
  }

  cmdContext() {
    this.requireStopped();
    return {
      ok: true,
      stopInfo: JSON.parse(this.stopInfo || 'null'),
      location: this.locationJson(),
      threads: this.threadsJson(),
      frames: this.framesJson(true),
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
      res = await this.cdp.request('Debugger.evaluateOnCallFrame', {
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

  async cmdStep(req, timeout) {
    this.requireLive();
    // Stepping needs a stopped frame to step from (uniform contract on
    // all bridges); continuing works from running (it just waits).
    this.requireStopped();
    const mode = req.mode || 'over';
    const method = { over: 'Debugger.stepOver', into: 'Debugger.stepInto', out: 'Debugger.stepOut' }[mode];
    if (!method) throw new BridgeErr(`bad step mode: ${mode}`);
    this.paused = null;
    this.cachedLocals = [];
    this.awaitingStep = true;
    try {
      await this.cdp.request(method);
    } catch (e) {
      // Stepping a running target fails at the protocol level — don't leave
      // the flag set or the next real stop misreports as a step landing.
      this.awaitingStep = false;
      throw e;
    }
    this.publishState(false);
    return this.resumeAndWait(timeout);
  }

  async cmdContinue(req, timeout) {
    this.requireLive();
    if (this.paused) {
      this.paused = null;
      this.cachedLocals = [];
      await this.cdp.request('Debugger.resume');
      this.publishState(false);
    }
    // Running already: nothing to resume (a bare resume errors on some
    // targets) — just wait for the next stop.
    return this.resumeAndWait(timeout);
  }

  async resumeAndWait(timeout) {
    await this.pump(timeout);
    return {
      ok: true,
      stopped: true,
      changed: JSON.parse(this.lastChanged),
      stopInfo: JSON.parse(this.stopInfo || 'null'),
      snapshot: this.snapshot(),
    };
  }

  cmdThreads() {
    if (this.exited) throw new BridgeErr('target VM has exited — close this session');
    const frames = this.framesJson(false);
    return {
      ok: true,
      running: !this.paused,
      threads: [{ id: 1, name: 'main', status: this.paused ? 'paused' : 'running', frames }],
    };
  }

  cmdBreaks() {
    // Arm-time records: what was requested and whether it planted
    // (verified/pending/slid/shadowed) — no round-trip needed, no stop
    // required.
    if (this.exited) throw new BridgeErr('target VM has exited — close this session');
    return { ok: true, stops: this.stopStates };
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
   */
  async cmdBreaksAdd(req) {
    if (this.exited) throw new BridgeErr('target VM has exited — close this session');
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
    if (fresh.length === 0) return { ok: true, added: [], stops: this.stopStates };
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
    return resp;
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
    return { ok: true, total: lines.length, truncated: lines.length > tail, lines: lines.slice(-tail) };
  }

  async dispatch(req) {
    const cmd = req.cmd;
    let timeout = Number(req.timeout !== undefined ? req.timeout : this.cfg.timeout);
    if (!Number.isFinite(timeout) || timeout <= 0 || timeout > 3600) throw new BridgeErr('timeout must be between 0 and 3600 seconds');
    if (cmd === 'close') throw new CloseSession();
    if (cmd === 'context') return this.cmdContext();
    if (cmd === 'stack') return this.cmdStack();
    if (cmd === 'vars') return await this.cmdVars(req);
    if (cmd === 'eval') return await this.cmdEval(req);
    if (cmd === 'step') return await this.cmdStep(req, timeout);
    if (cmd === 'continue') return await this.cmdContinue(req, timeout);
    if (cmd === 'threads') return this.cmdThreads();
    if (cmd === 'breaks') return this.cmdBreaks();
    if (cmd === 'breaksAdd') return await this.cmdBreaksAdd(req);
    if (cmd === 'logs') return this.cmdLogs(req);
    throw new BridgeErr(`unknown cmd: ${cmd}`);
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
  return String(t);
}

// ---------------------------------------------------------------- serve

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
          });
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
          });
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
      name: path.basename(cfg.dir), kind: cfg.kind, port, stopped: wantStop,
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
      // Mirror pybridge: exit nonzero; session.rs surfaces error.json.
      process.exit(e instanceof Usage ? 2 : 1);
    }
    throw e;
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
