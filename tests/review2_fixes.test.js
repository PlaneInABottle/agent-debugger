// Review-2 regression tests (general re-evaluation round).
//
// 1. continue forwards the pre-resume freshness baseline (same invariant
//    as the step race: a park landing while the resume is in flight is a
//    fresh stop, never stale).
// 2. Exception pauses consume a pending step (main + worker + browser):
//    a stale awaitingStep would misclassify a later stray pause as a
//    step landing instead of auto-resuming it.
// 3. Terminal close stays graceful (FIN, never destroy-after-end) on the
//    normal handleConn path, node + browser.
// 4. SIGTERM teardown runs cleanup then exits, exactly once.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const net = require('node:net');
const os = require('node:os');
const path = require('node:path');
const Module = require('node:module');
const { EventEmitter } = require('node:events');

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
  'Session, handleConn, closeFromConn, installSigtermCleanup, CloseSession, BridgeErr, MAX_ACTIVE_HANDLERS');
const browser = loadBridge('bridge/browser/src/browserbridge.js',
  'Session, handleConn, closeFromConn, installSigtermCleanup, CloseSession, BridgeErr, MAX_ACTIVE_HANDLERS');
const framing = require('../bridge/js/framing.js');

function tmpdir(prefix) {
  return fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), prefix)));
}

function mainFrame(lineNumber = 1) {
  return {
    callFrameId: 'm1', functionName: 'f', scopeChain: [],
    location: { scriptId: 'ms1', lineNumber },
  };
}

function nodeSession(dir, over = {}) {
  const st = new node.Session({
    kind: 'attach', dir, host: 'localhost', port: 9229, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20, programArgs: [],
    ...over,
  });
  st.exited = false;
  st.cdp = { request: async () => ({}) };
  return st;
}

function browserSession(dir, over = {}) {
  const st = new browser.Session({
    kind: 'attach', dir, host: 'localhost', port: 9222, tab: null, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20,
    ...over,
  });
  st.exited = false;
  st.verifyTab = async () => {};
  st.cdp = { request: async () => ({}) };
  return st;
}

function workerFrame(callFrameId = 'w1', lineNumber = 4) {
  return {
    callFrameId, functionName: 'work', scopeChain: [],
    location: { scriptId: 'ws1', lineNumber },
  };
}

function addWorker(st, sid) {
  const w = {
    id: `worker:${sid}`, sessionId: sid, state: 'running', paused: null,
    stopInfo: null, lastStop: null, stopStates: [], targetRaws: new Map(),
    inheritedKeys: new Set(), breakKeys: new Map(), breakRecByKey: new Map(),
    logpoints: [], scripts: new Map(), urls: new Map(), seq: 0, stopSeq: 0,
    cachedLocals: [], lastChanged: '[]', lastTop: null, lastFunc: null,
    awaitingStep: false, exited: false,
    observed: { url: `file:///w${sid}.js`, type: 'worker', endpoint: 'ws://x' },
  };
  assert.equal(st.workers.claimId(w.id), true);
  st.workers.track(w);
  return w;
}

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

// ---- 1. continue freshness --------------------------------------------------

test('r2 node: continue park landed during the resume still counts fresh', async () => {
  // Mirrors the step-race shape (m5): the pause handler records the park
  // while the resume request is in flight. Dropping the pre-request
  // baseline (base=null at the pump) takes the baseline after the park,
  // serves it as stale, and times out beside a parked target.
  const dir = tmpdir('r2-node-contrace-');
  const st = nodeSession(dir);
  st.paused = { frames: [mainFrame(1)], stopInfo: null };
  st.trackChanges = async () => {};
  st.pump = async () => 'stopped';
  const p2 = {
    reason: 'other', hitBreakpoints: ['b1'], data: null,
    callFrames: [mainFrame(2)],
  };
  st.req = async (method) => {
    if (method === 'Debugger.resume') {
      st._chainPause(() => st._swapRun(() => st.onPaused(p2))).catch(() => {});
    }
    return {};
  };
  const resp = await st.cmdContinue({}, 1);
  assert.equal(resp.stopped, true);
  assert.equal(resp.target, 'main');
  assert.ok(st.paused, 'fresh park still held after the continue');
});

// ---- 2. exception consumes the pending step ---------------------------------

test('r2 node: exception pause clears a pending step on main', async () => {
  const dir = tmpdir('r2-node-excstep-');
  const st = nodeSession(dir);
  st.awaitingStep = true;
  st.trackChanges = async () => {};
  await st.onPaused({
    reason: 'exception', data: null, hitBreakpoints: [],
    callFrames: [mainFrame(3)],
  });
  assert.ok(st.paused, 'exception parks');
  assert.equal(st.awaitingStep, false, 'exception stop consumes the pending step');
});

test('r2 node: exception pause clears a pending step on a worker', async () => {
  const dir = tmpdir('r2-node-excstepw-');
  const st = nodeSession(dir);
  const w = addWorker(st, 's1');
  w.awaitingStep = true;
  st.trackChanges = async () => {};
  await st.onWorkerPaused(w, {
    reason: 'exception', data: null, hitBreakpoints: [],
    callFrames: [workerFrame('w1', 5)],
  });
  assert.ok(w.paused, 'worker exception parks');
  assert.equal(w.awaitingStep, false, 'exception stop consumes the worker step');
});

test('r2 browser: exception pause clears a pending step', async () => {
  const dir = tmpdir('r2-browser-excstep-');
  const st = browserSession(dir);
  st.awaitingStep = true;
  st.trackChanges = async () => {};
  await st.onPaused({
    reason: 'exception', data: null, hitBreakpoints: [],
    callFrames: [{ functionName: 'f', location: { lineNumber: 3 } }],
  });
  assert.ok(st.paused, 'exception parks');
  assert.equal(st.awaitingStep, false, 'exception stop consumes the pending step');
});

// ---- 3. graceful close (FIN, never destroy-after-end) -----------------------

async function closeStaysGraceful(bridge, makeSession, tag) {
  const dir = tmpdir(`r2-${tag}-graceclose-`);
  const st = makeSession(dir);
  let cleanups = 0;
  st.cleanup = async () => { cleanups += 1; };
  st.dispatch = async () => { throw new bridge.CloseSession(); };
  assert.equal(st.server.tryAcquire(bridge.MAX_ACTIVE_HANDLERS), true);
  const { server, client, serverConn } = await tcpPair();
  let destroyed = false;
  let ended = false;
  const origDestroy = serverConn.destroy.bind(serverConn);
  const origEnd = serverConn.end.bind(serverConn);
  serverConn.destroy = (...a) => { destroyed = true; return origDestroy(...a); };
  serverConn.end = (...a) => { ended = true; return origEnd(...a); };
  try {
    const p = bridge.handleConn(st, serverConn);
    await framing.writeFrame(client, { cmd: 'close' });
    const resp = await framing.readFrame(client, 5000);
    assert.deepEqual(resp, { ok: true, closed: true, target: 'main' });
    await p;
    assert.equal(cleanups, 1, 'teardown runs exactly once');
    assert.equal(ended, true, 'close ends graceful (FIN)');
    assert.equal(destroyed, false, 'no destroy() after end() (would RST the ACK)');
    // The client must observe the FIN: end, not a reset close.
    await new Promise((resolve, reject) => {
      const t = setTimeout(() => reject(new Error('client never saw FIN')), 3000);
      client.once('end', () => { clearTimeout(t); resolve(); });
    });
  } finally {
    client.destroy();
    server.close();
  }
}

test('r2 node: normal close ACKs, ends graceful, never destroys', async () => {
  await closeStaysGraceful(node, nodeSession, 'node');
});

test('r2 browser: normal close ACKs, ends graceful, never destroys', async () => {
  await closeStaysGraceful(browser, browserSession, 'browser');
});

// ---- 4. SIGTERM teardown ----------------------------------------------------

async function sigtermCleansUpOnce(bridge, tag) {
  const proc = new EventEmitter();
  const exits = [];
  proc.exit = (c) => { exits.push(c); };
  let cleanups = 0;
  const st = { cleanup: async () => { cleanups += 1; } };
  bridge.installSigtermCleanup(st, proc);
  proc.emit('SIGTERM');
  proc.emit('SIGTERM');
  await new Promise((r) => setTimeout(r, 100));
  assert.equal(cleanups, 1, `${tag}: cleanup runs exactly once`);
  assert.deepEqual(exits, [0], `${tag}: exits 0 after cleanup`);
}

test('r2 node: SIGTERM runs cleanup once, then exits 0', async () => {
  await sigtermCleansUpOnce(node, 'node');
});

test('r2 browser: SIGTERM runs cleanup once, then exits 0', async () => {
  await sigtermCleansUpOnce(browser, 'browser');
});
