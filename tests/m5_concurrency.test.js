const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const Module = require('node:module');

// M5: outstanding-resume concurrency — live reads stay prompt, rivals
// busy-reject, different targets stay independent, close stays terminal.
// Loads the real bridge sources with the main() tail stripped.
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
  'Session, MAX_ACTIVE_HANDLERS, MAX_QUEUED_CONNS');
const browser = loadBridge('bridge/browser/src/browserbridge.js',
  'Session');

function tmpdir(prefix) {
  return fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), prefix)));
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

function mkWorker(sid) {
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

function addWorker(st, sid) {
  const w = mkWorker(sid);
  st.workerTable.set(w.id, w);
  st.workerOrder.push(w.id);
  st.seenWorkerIds.add(w.id);
  return w;
}

// ---- serve bounds ----------------------------------------------------------

test('m5 node: handler and queue bounds are small and fixed', () => {
  assert.equal(node.MAX_ACTIVE_HANDLERS, 8);
  assert.equal(node.MAX_QUEUED_CONNS, 16);
});

// ---- node live reads while a resume is held --------------------------------

test('m5 node: live reads stay prompt with zero CDP while resume held', async () => {
  const dir = tmpdir('m5-node-live-');
  const st = nodeSession(dir);
  st.cdp = { request: async () => { throw new Error('no CDP on live reads'); } };
  st.outstanding.set('main', 'continue');
  for (const cmd of ['threads', 'breaks', 'logs', 'targets']) {
    const t0 = Date.now();
    const resp = await st.dispatch({ cmd });
    assert.ok(Date.now() - t0 < 2000, cmd);
    assert.equal(resp.ok, true, cmd);
  }
  const threads = await st.dispatch({ cmd: 'threads' });
  assert.equal(threads.running, true);
  assert.deepEqual(threads.threads, []);
  // Worker live read is equally prompt.
  const w = addWorker(st, 'a');
  w.paused = null;
  st.outstanding.set(w.id, 'continue');
  const wt = await st.dispatch({ cmd: 'threads', target: w.id });
  assert.equal(wt.ok, true);
  assert.equal(wt.target, w.id);
});

test('m5 node: concurrent live dispatches all succeed', async () => {
  const dir = tmpdir('m5-node-conc-');
  const st = nodeSession(dir);
  st.outstanding.set('main', 'continue');
  const cmds = ['threads', 'breaks', 'logs', 'targets',
    'threads', 'breaks', 'logs', 'targets'];
  const resps = await Promise.all(cmds.map((cmd) => st.dispatch({ cmd })));
  for (let i = 0; i < resps.length; i++) {
    assert.equal(resps[i].ok, true, cmds[i]);
  }
});

// ---- node busy rules ---------------------------------------------------------

test('m5 node: second resume on the same target is busy', async () => {
  const dir = tmpdir('m5-node-busy-');
  const st = nodeSession(dir);
  st.outstanding.set('main', 'continue');
  for (const cmd of ['continue', 'step']) {
    await assert.rejects(st.dispatch({ cmd }),
      /busy: continue outstanding for main/);
  }
});

test('m5 node: global mutation conflicts with any outstanding resume', async () => {
  const dir = tmpdir('m5-node-mut-');
  const st = nodeSession(dir);
  const w = addWorker(st, 'a');
  st.outstanding.set(w.id, 'continue');
  for (const cmd of ['breaksAdd', 'breaksRemove', 'breaksClear']) {
    const req = { cmd };
    if (cmd !== 'breaksClear') req.breaks = ['x:1'];
    await assert.rejects(st.dispatch(req),
      /busy: continue outstanding for worker:a/);
  }
});

test('m5 node: target-scoped mutation conflicts only with its target', async () => {
  const dir = tmpdir('m5-node-scope-');
  const st = nodeSession(dir);
  const a = addWorker(st, 'a');
  const b = addWorker(st, 'b');
  st.outstanding.set(a.id, 'continue');
  await assert.rejects(
    st.dispatch({ cmd: 'breaksAdd', target: a.id, breaks: ['x:1'] }),
    /busy: continue outstanding for worker:a/);
  let called = null;
  st.addWorkerEphemeral = async (tid, raws) => {
    called = [tid, raws];
    return { ok: true };
  };
  const resp = await st.dispatch({ cmd: 'breaksAdd', target: b.id, breaks: ['x:1'] });
  assert.equal(resp.ok, true);
  assert.deepEqual(called, [b.id, ['x:1']]);
});

test('m5 node: different-target resume runs independently', async () => {
  const dir = tmpdir('m5-node-indep-');
  const st = nodeSession(dir);
  const a = addWorker(st, 'a');
  const b = addWorker(st, 'b');
  st.outstanding.set(a.id, 'continue');
  st.lastParkTarget = b.id;
  st.pump = async () => 'stopped';
  await assert.rejects(
    st.dispatch({ cmd: 'continue', target: a.id }),
    /busy: continue outstanding for worker:a/);
  const resp = await st.dispatch({ cmd: 'continue', target: b.id });
  assert.equal(resp.ok, true);
  assert.equal(resp.target, b.id);
  assert.ok(!st.outstanding.has(b.id));
  assert.equal(st.outstanding.get(a.id), 'continue');
});

test('m5 node: eval is busy despite parked frames', async () => {
  const dir = tmpdir('m5-node-eval-');
  const st = nodeSession(dir);
  st.paused = { frames: [workerFrame()], stopInfo: null };
  st.outstanding.set('main', 'continue');
  await assert.rejects(st.dispatch({ cmd: 'eval', expr: '1+1' }),
    /busy: continue outstanding for main/);
});

test('m5 node: frame reads fail fast while running', async () => {
  const dir = tmpdir('m5-node-frame-');
  const st = nodeSession(dir);
  st.paused = null;
  st.outstanding.set('main', 'continue');
  for (const cmd of ['context', 'stack', 'vars']) {
    await assert.rejects(st.dispatch({ cmd }), /no stopped thread/);
  }
});

test('m5 node: close is accepted despite an outstanding resume', async () => {
  const dir = tmpdir('m5-node-close-');
  const st = nodeSession(dir);
  st.outstanding.set('main', 'continue');
  await assert.rejects(st.dispatch({ cmd: 'close' }), (e) => {
    assert.equal(e.constructor.name, 'CloseSession');
    return true;
  });
});

test('m5 node: resume registers outstanding during the wait, then clears', async () => {
  const dir = tmpdir('m5-node-reg-');
  const st = nodeSession(dir);
  st.paused = { frames: [], stopInfo: null };
  let seenDuringWait = null;
  st.pump = async () => {
    seenDuringWait = new Map(st.outstanding);
    return 'stopped';
  };
  const resp = await st.dispatch({ cmd: 'continue' });
  assert.equal(resp.ok, true);
  assert.equal(resp.target, 'main');
  assert.deepEqual([...seenDuringWait.entries()], [['main', 'continue']]);
  assert.equal(st.outstanding.size, 0);
});

// ---- browser: single-target outstanding --------------------------------------

test('m5 browser: live reads prompt, rivals busy, reload is a resume op', async () => {
  const dir = tmpdir('m5-browser-');
  const st = browserSession(dir);
  st.cdp = { request: async () => { throw new Error('no CDP on live reads'); } };
  st.outstanding.set('main', 'continue');
  for (const cmd of ['threads', 'breaks', 'logs']) {
    const t0 = Date.now();
    const resp = await st.dispatch({ cmd });
    assert.ok(Date.now() - t0 < 2000, cmd);
    assert.equal(resp.ok, true, cmd);
  }
  for (const req of [{ cmd: 'continue' }, { cmd: 'step' }, { cmd: 'reload' }]) {
    await assert.rejects(st.dispatch(req),
      /busy: continue outstanding for main/);
  }
  for (const cmd of ['breaksAdd', 'breaksRemove', 'breaksClear']) {
    const req = { cmd };
    if (cmd !== 'breaksClear') req.breaks = ['app.js:1'];
    await assert.rejects(st.dispatch(req),
      /busy: continue outstanding for main/);
  }
  await assert.rejects(st.dispatch({ cmd: 'eval', expr: '1' }),
    /busy: continue outstanding for main/);
  for (const cmd of ['context', 'stack', 'vars']) {
    await assert.rejects(st.dispatch({ cmd }), /no stopped thread/);
  }
  await assert.rejects(st.dispatch({ cmd: 'close' }), (e) => {
    assert.equal(e.constructor.name, 'CloseSession');
    return true;
  });
});
