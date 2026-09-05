/* Browser CDP bridge for agent-debugger (B0: attach skeleton).
 *
 * Same shape as nodebridge: a per-session daemon speaking OUR session
 * protocol (Content-Length JSON over TCP on 127.0.0.1) to the Rust CLI, and
 * raw CDP over WebSocket (via the provisioned `ws` package) to a browser
 * tab. Runs on Node — no new runtime provisioning beyond the script itself.
 *
 *     browserbridge.js session --kind attach --dir DIR --host H --port P
 *         [--tab SUBSTRING] [--timeout S]
 *
 * Tab picking: filter type==='page' targets whose url or title contains
 * --tab (default: first page). Multiple matches and no match both fail
 * fast with the (capped) candidate list — never guess a tab. Browsers also
 * expose service_workers/background_pages; those are never candidates.
 *
 * Speed posture (agents pay per token and per step): headless-first (no GUI
 * needed for JS debugging), attach to a WARM shared browser (default port
 * 9222 = agent-browser convention, so interaction + debugging share one
 * browser instead of two), snapshot-first responses, capped lists
 * everywhere (tabs, and later frames/vars). No polling loops burn agent
 * steps: waits happen bridge-side with deadlines.
 *
 * B0: attach + session server only (threads/close + honest B1 stub for the
 * rest). B1 wires the Debugger domain (break/step/eval, nodebridge parity).
 */

'use strict';
const fs = require('fs');
const http = require('http');
const net = require('net');
const os = require('os');
const path = require('path');

class Usage extends Error {}
class BridgeErr extends Error {}
class CloseSession extends Error {}

const MAX_TABS_LISTED = 10;
const MAX_STRING = 200;

// ---------------------------------------------------------------- framing
// (identical to nodebridge: one protocol, three bridges)

function readFrame(conn) {
  return new Promise((resolve, reject) => {
    let buf = Buffer.alloc(0);
    const onData = (chunk) => {
      buf = Buffer.concat([buf, chunk]);
      const sep = buf.indexOf('\r\n\r\n');
      if (sep < 0) return;
      let length = -1;
      for (const line of buf.subarray(0, sep).toString('ascii').split('\r\n')) {
        const i = line.indexOf(':');
        if (i > 0 && line.slice(0, i).trim().toLowerCase() === 'content-length') {
          length = parseInt(line.slice(i + 1).trim(), 10);
        }
      }
      if (Number.isNaN(length) || length < 0) {
        cleanup();
        reject(new BridgeErr('bad frame: no Content-Length'));
        return;
      }
      if (buf.length < sep + 4 + length) return;
      const body = buf.subarray(sep + 4, sep + 4 + length);
      cleanup();
      try {
        resolve(JSON.parse(body.toString('utf-8')));
      } catch (e) {
        reject(new BridgeErr('bad frame: ' + e.message));
      }
    };
    const onClose = () => {
      cleanup();
      reject(new BridgeErr('truncated frame'));
    };
    const cleanup = () => {
      conn.removeListener('data', onData);
      conn.removeListener('close', onClose);
    };
    conn.on('data', onData);
    conn.on('close', onClose);
  });
}

function writeFrame(conn, obj) {
  // Never throws synchronously: writing to a dead socket raises sync
  // (writeAfterFIN) instead of calling back with err. A connect+drop health
  // check (e.g. our own `status` probe) used to kill the whole daemon here.
  return new Promise((resolve, reject) => {
    let msg;
    try {
      const body = Buffer.from(JSON.stringify(obj), 'utf-8');
      msg = Buffer.concat([Buffer.from(`Content-Length: ${body.length}\r\n\r\n`), body]);
    } catch (e) {
      reject(e);
      return;
    }
    try {
      conn.write(msg, (err) => (err ? reject(err) : resolve()));
    } catch (e) {
      reject(e);
    }
  });
}

// ---------------------------------------------------------------- args

function needInt(flag, raw) {
  const n = parseInt(raw, 10);
  if (Number.isNaN(n)) throw new Usage(`${flag} needs a number (got '${raw}')`);
  return n;
}

function parseArgs(argv) {
  // argv === process.argv.slice(2); argv[0] === "session"
  const cfg = {
    kind: 'attach', dir: null,
    host: 'localhost', port: 9222, tab: null,
    timeout: 20,
    breaks: [], logpoints: [],
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
    // Stops parse in B1; until then fail fast rather than silently ignore.
    else if (a === '--break') throw new Usage(`breakpoints land in B1 (skeleton session only): ${rest[i] || ''}`);
    else if (a === '--logpoint') throw new Usage('logpoints land in B1 (skeleton session only)');
    else if (a === '--watch') throw new Usage('--watch has no CDP equivalent (browser)');
    else if (a === '--exit') throw new Usage('--exit has no CDP equivalent (browser)');
    else if (a === '--src') need(a); // accepted for CLI-shape parity, used in B2
    else throw new Usage(`unknown arg: ${a}`);
  }
  if (!cfg.dir) throw new Usage('missing --dir');
  if (cfg.kind !== 'attach') {
    throw new Usage(`browser is attach-only for now (got --kind ${cfg.kind}); launch lands later`);
  }
  return cfg;
}

// ---------------------------------------------------------------- tabs

function truncStr(s, limit = MAX_STRING) {
  if (s.length <= limit) return s;
  return `${s.slice(0, limit)}… (+${s.length - limit} more chars)`;
}

function tabSummary(t) {
  return `${truncStr(t.title || '(no title)', 60)} <${truncStr(t.url || '', 100)}>`;
}

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

function writeFile(p, content) {
  try {
    fs.writeFileSync(p, content);
  } catch (_) { /* best effort */ }
}

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

class Session {
  constructor(cfg) {
    this.cfg = cfg;
    this.tab = null;      // picked /json/list entry
    this.cdp = null;      // B1: live CDP connection
    this.closing = false;
  }

  async handshake() {
    const targets = await listTargets(this.cfg.host, this.cfg.port);
    this.tab = pickTab(targets, this.cfg.tab);
    // B1: connect WS + Debugger.enable here. B0 stops at picking: the
    // session exists (threads/status/close work) with zero tab disturbance.
  }

  tabJson() {
    const t = this.tab || {};
    return { id: t.id || '?', title: t.title || '?', url: t.url || '?' };
  }

  dispatch(req) {
    const cmd = req.cmd;
    if (cmd === 'close') throw new CloseSession();
    if (cmd === 'threads') {
      return {
        ok: true,
        running: true,
        note: 'browser debug core lands in B1 (attached, undisturbed)',
        threads: [{ id: 1, name: 'tab', status: 'running', frames: [], tab: this.tabJson() }],
      };
    }
    throw new BridgeErr(`browser debug core lands in B1 (got '${cmd}')`);
  }

  async cleanup() {
    this.closing = true;
    // Attach semantics (all browsers): detach only, the tab keeps running.
    if (this.cdp) {
      try {
        this.cdp.close();
      } catch (_) { /* best effort */ }
      this.cdp = null;
    }
  }
}

// ---------------------------------------------------------------- serve
// (same shape as nodebridge, including the permanent connection queue:
// Node emits 'connection' eagerly, even with no listener attached.)

async function serve(st, server, queue) {
  for (;;) {
    while (queue.length === 0) {
      await new Promise((resolve) => {
        queue.waiter = resolve;
      });
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
  const server = net.createServer();
  server.on('error', (e) => die(`session socket: ${e.message}`, 1));
  const queue = [];
  queue.waiter = null;
  server.on('connection', (conn) => {
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
  try {
    await st.handshake();
    // B0: attached but undisturbed — no stops by construction.
    writeSessionFile(cfg.dir, {
      name: path.basename(cfg.dir), kind: cfg.kind, port, stopped: false,
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
    throw e;
  }
}

main(process.argv.slice(2)).catch((e) => die(`internal: ${(e && e.stack) || e}`, 1));
