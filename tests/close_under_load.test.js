// Milestone C: terminal close under full handler load + request-local
// error attribution (Node + Browser).
//
// Pool-full connections still get one bounded frame read: an exact
// `close` terminates outside the pool (closed ACK, teardown once) while
// ordinary commands get the existing overloaded rejection and malformed
// reads just drop. Error envelopes name THIS request's target even while
// a sibling holds wrong shared serving state (barrier-ordered).
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const net = require('node:net');
const os = require('node:os');
const path = require('node:path');
const Module = require('node:module');

function loadBridge(rel, exports) {
  const file = path.join(__dirname, '..', rel);
  const jsDir = path.join(__dirname, '..', 'bridge', 'js');
  let src = fs.readFileSync(file, 'utf-8')
    .replace(/require\('\.\/cdp_conn\.js'\)/g,
      `require(${JSON.stringify(path.join(jsDir, 'cdp_conn.js'))})`)
    .replace(/require\('\.\/framing\.js'\)/g,
      `require(${JSON.stringify(path.join(jsDir, 'framing.js'))})`)
    .replace(/^main\(process\.argv[\s\S]*$/m, `module.exports = { ${exports} };`);
  assert.ok(src.includes('module.exports'), `${rel}: main() tail not replaced`);
  const m = new Module(file, null);
  m.filename = file;
  m.paths = Module._nodeModulePaths(path.dirname(file));
  m._compile(src, file);
  return m.exports;
}

const node = loadBridge('bridge/node/src/nodebridge.js',
  'Session, handleOverload, handleConn, closeFromConn, errorTargetFor, MAX_ACTIVE_HANDLERS, BridgeErr');
const nodeFull = loadBridge('bridge/node/src/nodebridge.js',
  'Session, serve, writeOwner, CloseSession, BridgeErr, MAX_ACTIVE_HANDLERS, MAX_QUEUED_CONNS');
const browser = loadBridge('bridge/browser/src/browserbridge.js',
  'Session, handleOverload, handleConn, closeFromConn, MAX_ACTIVE_HANDLERS, BridgeErr');
const framing = require('../bridge/js/framing.js');

function tmpdir(prefix) {
  return fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), prefix)));
}

// Real loopback pair: client speaks, the test drives the server side.
async function tcpPair() {
  const server = net.createServer();
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  const port = server.address().port;
  const accepted = new Promise((r) => server.once('connection', r));
  const client = net.connect(port, '127.0.0.1');
  await new Promise((r) => client.once('connect', r));
  const serverConn = await accepted;
  serverConn.on('error', () => {});
  client.on('error', () => {});
  return { server, client, serverConn };
}

async function clientRoundtrip(client, req) {
  await framing.writeFrame(client, req);
  return framing.readFrame(client, 5000);
}

function nodeSession(dir) {
  const st = new node.Session({
    kind: 'attach', dir, host: 'localhost', port: 9229, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20, programArgs: [],
  });
  st.exited = false;
  st.cdp = { request: async () => ({}) };
  return st;
}

function browserSession(dir) {
  const st = new browser.Session({
    kind: 'attach', dir, host: 'localhost', port: 9222, tab: null, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20,
  });
  st.exited = false;
  st.verifyTab = async () => {};
  st.cdp = { request: async () => ({}) };
  return st;
}

function workerShape(sid) {
  return {
    id: `worker:${sid}`, sessionId: sid, state: 'running', paused: null,
    stopInfo: null, lastStop: null, stopStates: [], targetRaws: new Map(),
    inheritedKeys: new Set(), breakKeys: new Map(), breakRecByKey: new Map(),
    logpoints: [], scripts: new Map(), urls: new Map(), seq: 0, stopSeq: 0,
    cachedLocals: [], lastChanged: '[]', lastTop: null, lastFunc: null,
    awaitingStep: false, exited: false,
    observed: { url: `file:///w${sid}.js`, type: 'worker', endpoint: 'ws://x' },
  };
}

// ---- node pool-full close ----

test('node: pool-full ordinary command is overloaded, pool untouched', async () => {
  const dir = tmpdir('c-close-node-');
  const st = nodeSession(dir);
  st.activeConns = node.MAX_ACTIVE_HANDLERS;
  const { server, client, serverConn } = await tcpPair();
  try {
    const p = node.handleOverload(st, serverConn);
    const resp = await clientRoundtrip(client, { cmd: 'threads' });
    await p;
    assert.equal(resp.ok, false);
    assert.match(resp.error, /overloaded/);
    assert.equal(resp.target, 'main');
    assert.equal(st.activeConns, node.MAX_ACTIVE_HANDLERS);
    assert.equal(st.closing, false);
  } finally {
    client.destroy();
    server.close();
  }
});

test('node: pool-full exact close ACKs and tears down once', async () => {
  const dir = tmpdir('c-close-node-');
  const st = nodeSession(dir);
  let cleanups = 0;
  st.cleanup = async () => { cleanups += 1; };
  st.activeConns = node.MAX_ACTIVE_HANDLERS;
  const { server, client, serverConn } = await tcpPair();
  try {
    const p = node.handleOverload(st, serverConn);
    const resp = await clientRoundtrip(client, { cmd: 'close' });
    await p;
    assert.deepEqual(resp, { ok: true, closed: true, target: 'main' });
    assert.equal(st.closing, true);
    assert.equal(cleanups, 1);
    assert.equal(st.activeConns, node.MAX_ACTIVE_HANDLERS);
    // A second close still ACKs but never re-runs teardown.
    const second = await tcpPair();
    try {
      const p2 = node.closeFromConn(st, second.serverConn);
      const resp2 = await clientRoundtrip(second.client, { cmd: 'close' });
      await p2;
      assert.deepEqual(resp2, { ok: true, closed: true, target: 'main' });
      assert.equal(cleanups, 1);
    } finally {
      second.client.destroy();
      second.server.close();
    }
  } finally {
    client.destroy();
    server.close();
  }
});

test('node: pool-full malformed read just drops the socket', async () => {
  const dir = tmpdir('c-close-node-');
  const st = nodeSession(dir);
  st.activeConns = node.MAX_ACTIVE_HANDLERS;
  const { server, client, serverConn } = await tcpPair();
  try {
    const p = node.handleOverload(st, serverConn);
    client.write('junk-bytes');
    client.end();
    await p;
    assert.equal(serverConn.destroyed, true);
    assert.equal(st.closing, false);
    assert.equal(st.activeConns, node.MAX_ACTIVE_HANDLERS);
  } finally {
    client.destroy();
    server.close();
  }
});

// ---- node full serve loop: 8 blocked, 9th overloaded, 10th closes ----

test('node: serve loop overloads 9th ordinary but closes on 10th', async () => {
  const dir = tmpdir('c-loop-node-');
  const st = new nodeFull.Session({
    kind: 'attach', dir, host: 'localhost', port: 9229, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20, programArgs: [],
  });
  st.exited = false;
  let cleanups = 0;
  st.cleanup = async () => { cleanups += 1; };
  nodeFull.writeOwner(dir);
  let release;
  const gate = new Promise((r) => { release = r; });
  st.dispatch = async (req) => {
    if (req.cmd === 'close') throw new nodeFull.CloseSession();
    await gate;
    return { ok: true };
  };
  const server = net.createServer();
  const queue = [];
  queue.waiter = null;
  server.on('connection', (conn) => {
    conn.on('error', () => {});
    if (queue.length >= nodeFull.MAX_QUEUED_CONNS) {
      try { conn.destroy(); } catch (_) { /* already gone */ }
      return;
    }
    queue.push(conn);
    if (queue.waiter) {
      const w = queue.waiter;
      queue.waiter = null;
      w();
    }
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  const port = server.address().port;
  const exits = [];
  const origExit = process.exit;
  process.exit = ((c) => { exits.push(c); });
  const serveP = nodeFull.serve(st, server, queue);
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  try {
    const held = [];
    for (let i = 0; i < 8; i++) {
      const c = net.connect(port, '127.0.0.1');
      c.on('error', () => {});
      held.push((async () => {
        const r = await clientRoundtrip(c, { cmd: 'noop' });
        c.destroy();
        return r;
      })());
    }
    const t0 = Date.now();
    while (st.activeConns < 8 && Date.now() - t0 < 5000) await sleep(10);
    assert.equal(st.activeConns, 8);
    const c9 = net.connect(port, '127.0.0.1');
    c9.on('error', () => {});
    const r9 = await clientRoundtrip(c9, { cmd: 'threads' });
    c9.destroy();
    assert.equal(r9.ok, false);
    assert.match(r9.error, /overloaded/);
    const c10 = net.connect(port, '127.0.0.1');
    c10.on('error', () => {});
    const r10 = await clientRoundtrip(c10, { cmd: 'close' });
    c10.destroy();
    assert.deepEqual(r10, { ok: true, closed: true, target: 'main' });
    assert.equal(st.closing, true);
    assert.equal(cleanups, 1);
    // Stop the loop via abandonment (no process exit inside tests).
    fs.unlinkSync(path.join(dir, 'owner.json'));
    await Promise.race([
      serveP,
      sleep(8000).then(() => { throw new Error('serve loop stuck'); }),
    ]);
    release();
    const resps = await Promise.all(held);
    for (const r of resps) assert.equal(r.ok, true);
  } finally {
    process.exit = origExit;
    release();
    server.close();
  }
});

// ---- node request-local error attribution ----

test('node: failing envelope names this request, not the sibling scope', async () => {
  const dir = tmpdir('c-attr-node-');
  const st = nodeSession(dir);
  let release;
  const gate = new Promise((r) => { release = r; });
  let entered;
  const enteredP = new Promise((r) => { entered = r; });
  st.dispatch = async (req) => {
    if (req.marker === 'sibling') {
      st.serving = 'worker:zzz';
      st.pendingTarget = 'worker:zzz';
      entered();
      await gate;
      return { ok: true };
    }
    throw new node.BridgeErr(`unknown target: ${req.target}`);
  };
  const sib = await tcpPair();
  const sibP = node.handleConn(st, sib.serverConn);
  await framing.writeFrame(sib.client, { cmd: 'threads', marker: 'sibling' });
  await enteredP;
  try {
    // Explicit unknown target: envelope names the requested raw string.
    const b1 = await tcpPair();
    try {
      const p1 = node.handleConn(st, b1.serverConn);
      const r1 = await clientRoundtrip(b1.client, { cmd: 'threads', target: 'worker:bogus' });
      await p1;
      assert.equal(r1.ok, false);
      assert.match(r1.error, /unknown target/);
      assert.equal(r1.target, 'worker:bogus');
    } finally {
      b1.client.destroy();
      b1.server.close();
    }
    // Omitted target: this request's own resolution (main here), never
    // the sibling's worker:zzz.
    const b2 = await tcpPair();
    try {
      const p2 = node.handleConn(st, b2.serverConn);
      const r2 = await clientRoundtrip(b2.client, { cmd: 'threads' });
      await p2;
      assert.equal(r2.ok, false);
      assert.equal(r2.target, 'main');
    } finally {
      b2.client.destroy();
      b2.server.close();
    }
  } finally {
    release();
    await sibP;
    sib.client.destroy();
    sib.server.close();
  }
});

// ---- browser pool-full close ----

test('browser: pool-full ordinary is overloaded; exact close ACKs once', async () => {
  const dir = tmpdir('c-close-br-');
  const st = browserSession(dir);
  let cleanups = 0;
  st.cleanup = async () => { cleanups += 1; };
  st.activeConns = browser.MAX_ACTIVE_HANDLERS;
  const o = await tcpPair();
  try {
    const p = browser.handleOverload(st, o.serverConn);
    const resp = await clientRoundtrip(o.client, { cmd: 'threads' });
    await p;
    assert.equal(resp.ok, false);
    assert.match(resp.error, /overloaded/);
    assert.equal(st.closing, false);
  } finally {
    o.client.destroy();
    o.server.close();
  }
  const c = await tcpPair();
  try {
    const p = browser.handleOverload(st, c.serverConn);
    const resp = await clientRoundtrip(c.client, { cmd: 'close' });
    await p;
    assert.deepEqual(resp, { ok: true, closed: true, target: 'main' });
    assert.equal(st.closing, true);
    assert.equal(cleanups, 1);
    assert.equal(st.activeConns, browser.MAX_ACTIVE_HANDLERS);
  } finally {
    c.client.destroy();
    c.server.close();
  }
});
