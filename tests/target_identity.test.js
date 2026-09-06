const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const Module = require('node:module');

// Layered target identity + honest waitContext (node + browser bridges).
// Real Session objects, stubbed transports — no timers in assertions.
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

const node = loadBridge('bridge/node/src/nodebridge.js', 'Session, StopTimeout');
const browser = loadBridge('bridge/browser/src/browserbridge.js', 'Session, StopTimeout');

function tmpdir(prefix) {
  return fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), prefix)));
}

// ---- node ----

function nodeSession(dir, over = {}) {
  const st = new node.Session({
    kind: 'attach', dir, host: '127.0.0.1', port: 9333, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20, programArgs: [],
    ...over,
  });
  st.exited = false;
  return st;
}

test('node discoverAttach keeps the selected /json/list entry', () => {
  const st = nodeSession(tmpdir('ti-node-'));
  const list = [
    { id: 'x1', type: 'page', title: 'nope', url: 'http://x/', webSocketDebuggerUrl: 'ws://h:1/x' },
    { id: 'n7', type: 'node', title: '/srv/sleepy.mjs', url: 'file:///srv/sleepy.mjs', webSocketDebuggerUrl: 'ws://h:2/n' },
  ];
  const picked = st.pickTargetEntry(list);
  assert.equal(picked.id, 'n7');
  st.attachEntry = { id: picked.id, title: picked.title, url: picked.url, type: picked.type };
  st.mainWsUrl = picked.webSocketDebuggerUrl;
  const ident = st.buildTargetIdentity();
  // Debuggee is protocol-confirmed WITHOUT any pid claim.
  assert.equal(ident.debuggee.confidence, 'protocol-confirmed');
  assert.equal(ident.debuggee.title, '/srv/sleepy.mjs');
  assert.equal(ident.debuggee.url, 'file:///srv/sleepy.mjs');
  assert.equal(ident.debuggee.targetId, 'n7');
  assert.ok(!('pid' in ident.debuggee), 'no OS pid presented as debuggee');
  // Adapter is in-process by design.
  assert.equal(ident.adapter.inProcess, true);
  assert.equal(ident.adapter.confidence, 'unavailable');
  // Endpoint names host/port/ws; no pid without an OS observation.
  assert.equal(ident.endpoint.host, '127.0.0.1');
  assert.equal(ident.endpoint.port, 9333);
  assert.equal(ident.endpoint.confidence, 'unavailable');
  // Hint leads with the debuggee, never a root-cause claim.
  assert.match(st.identityHint, /debuggee: .*protocol-confirmed/);
  assert.ok(!/unreachable|missed|wrong/.test(st.identityHint));
});

test('node endpoint corroborates (never confirms) the OS owner pid', () => {
  const st = nodeSession(tmpdir('ti-node-'), {
    observedTarget: {
      kind: 'process', pid: 4242, executable: '/usr/bin/node',
      argv: ['node', '--inspect=9333', 'sleepy.mjs', '--server-access-token', 'HEX'],
      cwd: '/srv', source: 'os-proc',
    },
  });
  st.attachEntry = { id: 'n7', title: 'sleepy.mjs', url: 'file:///srv/sleepy.mjs', type: 'node' };
  st.mainWsUrl = 'ws://127.0.0.1:9333/n7';
  const ident = st.buildTargetIdentity();
  assert.equal(ident.endpoint.ownerPid, 4242);
  assert.equal(ident.endpoint.confidence, 'os-corroborated');
  // The bridge re-redacts what it publishes (idempotent over the CLI copy).
  const blob = JSON.stringify(ident);
  assert.ok(!blob.includes('HEX'));
  assert.ok(blob.includes('[redacted]'));
  // ...but the debuggee still carries no pid.
  assert.ok(!('pid' in ident.debuggee));
});

test('node missing list entry degrades to unavailable, never fabricated', () => {
  const st = nodeSession(tmpdir('ti-node-'));
  st.attachEntry = null;
  st.mainWsUrl = null;
  const ident = st.buildTargetIdentity();
  assert.equal(ident.debuggee.confidence, 'unavailable');
  assert.ok(ident.debuggee.unavailable.some((u) => u.field === 'title'));
  assert.equal(st.identityHint, '');
});

test('node wait timeout carries trigger-unknown waitContext, prefix intact', async () => {
  const st = nodeSession(tmpdir('ti-node-'));
  st.attachEntry = { id: 'n7', title: 't', url: 'file:///s.mjs', type: 'node' };
  st.mainWsUrl = 'ws://127.0.0.1:9333/n7';
  st.buildTargetIdentity();
  st.cdp = { request: async () => ({}) };
  // The real pump's owner-guard would exit(0) on a test dir, so the pump is
  // stubbed exactly the way the bridge's own pump builds the error (real
  // end-to-end timeouts run in the live suite).
  st.pump = async (timeout, withCtx) => {
    assert.equal(withCtx, true);
    const err = new node.StopTimeout(`timeout: no stop within ${timeout}s`);
    err.waitContext = st.waitContext(timeout, Date.now() - 50);
    throw err;
  };
  try {
    await st.cmdWait({}, 5);
    assert.fail('must time out');
  } catch (e) {
    assert.match(e.message, /^timeout: no stop within 5s/);
    assert.ok(e.waitContext, 'waitContext rides the error');
    assert.equal(e.waitContext.triggerStatus, 'unknown');
    assert.ok(!('expectedBreak' in e.waitContext), 'wait plants no break');
    assert.equal(e.waitContext.targetIdentity.debuggee.confidence, 'protocol-confirmed');
    assert.match(e.waitContext.note, /not observed/);
    assert.match(e.waitContext.note, /not that the code is unreachable/);
  }
  assert.equal(st.paused, null);
});

test('node capture timeout carries expectedBreak, session stays armed', async () => {
  const dir = tmpdir('ti-node-');
  const file = path.join(dir, 'wc.js');
  fs.writeFileSync(file, Array.from({ length: 12 }, (_, i) => `const v${i} = ${i};`).join('\n') + '\n');
  const st = nodeSession(dir);
  st.attachEntry = { id: 'n7', title: file, url: `file://${file}`, type: 'node' };
  st.buildTargetIdentity();
  const calls = [];
  st.cdp = { request: async (m) => { calls.push(m); return {}; } };
  st.pump = async (timeout, withCtx) => {
    assert.equal(withCtx, true);
    const err = new node.StopTimeout('timeout: no stop within 5s');
    err.waitContext = st.waitContext(5, Date.now() - 5000);
    throw err;
  };
  // Plant path needs a live break parse; stub the plant to isolate context.
  st.capturePlant = async () => ({ kind: 'main-dup' });
  st.captureUnplant = async () => {};
  const spec = `${file}:5`;
  try {
    await st.cmdCapture({ break: spec }, 5);
    assert.fail('must time out');
  } catch (e) {
    assert.match(e.message, /^timeout: no stop within 5s/);
    assert.equal(e.waitContext.expectedBreak, spec);
    assert.equal(e.waitContext.triggerStatus, 'unknown');
  }
  assert.ok(!calls.includes('Debugger.resume'), 'timeout never resumes');
});

test('node identity is capped and secret-free', () => {
  const st = nodeSession(tmpdir('ti-node-'));
  const long = 'x'.repeat(900);
  st.attachEntry = { id: 'n', title: long, url: `file://${long}`, type: 'node' };
  const ident = st.buildTargetIdentity();
  assert.ok(ident.debuggee.title.length <= 512 + 30);
  assert.ok(JSON.stringify(ident).length <= 4096);
});

// ---- browser ----

function browserSession(dir, over = {}) {
  const st = new browser.Session({
    kind: 'attach', dir, host: 'localhost', port: 9222, tab: null, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20,
    ...over,
  });
  st.exited = false;
  st.verifyTab = async () => {};
  return st;
}

test('browser tab becomes the protocol-confirmed debuggee', () => {
  const st = browserSession(tmpdir('ti-br-'));
  st.cfg.observedTarget = {
    kind: 'tab', url: 'http://h/app.js?token=HEX', title: 'Shop',
    targetId: 'ABC', debugEndpoint: 'localhost:9222',
    cwd: null, argv: null, notApplicable: ['cwd', 'argv'],
    source: 'cdp-target-list', observedAt: 1, unavailable: [], warnings: [],
  };
  const ident = st.buildTargetIdentity();
  assert.equal(ident.debuggee.confidence, 'protocol-confirmed');
  assert.equal(ident.debuggee.targetId, 'ABC');
  assert.equal(ident.debuggee.title, 'Shop');
  assert.ok(!('pid' in ident.debuggee), 'tabs never claim a pid');
  assert.equal(ident.endpoint.debugEndpoint, 'localhost:9222');
  assert.equal(ident.adapter.confidence, 'unavailable');
  assert.match(st.identityHint, /debuggee: tab .*protocol-confirmed/);
  assert.ok(JSON.stringify(ident).length <= 4096);
});

test('browser wait timeout carries trigger-unknown waitContext', async () => {
  const st = browserSession(tmpdir('ti-br-'));
  st.cfg.observedTarget = {
    kind: 'tab', url: 'http://h/app.js', title: 'T', targetId: 'ABC',
    debugEndpoint: 'localhost:9222', cwd: null, argv: null,
    notApplicable: ['cwd', 'argv'], source: 'cdp-target-list',
    observedAt: 1, unavailable: [], warnings: [],
  };
  st.buildTargetIdentity();
  st.cdp = { request: async () => ({}) };
  // Same owner-guard note as node: the pump is stubbed the way the real
  // pump builds the error; live timeouts run in the live suite.
  st.pump = async (timeout, withCtx) => {
    assert.equal(withCtx, true);
    const err = new browser.StopTimeout(`timeout: no stop within ${timeout}s`);
    err.waitContext = st.waitContext(timeout, Date.now() - 50);
    throw err;
  };
  try {
    await st.cmdWait({}, 5);
    assert.fail('must time out');
  } catch (e) {
    assert.match(e.message, /^timeout: no stop within 5s/);
    assert.equal(e.waitContext.triggerStatus, 'unknown');
    assert.equal(e.waitContext.targetIdentity.debuggee.targetId, 'ABC');
  }
});
