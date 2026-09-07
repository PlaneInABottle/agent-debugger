// M4: Node bridge in-file owners — WorkerRegistry, SerialChain, ServerState.
//
// Every test drives the owners through the Session surface (or through the
// exact owner methods production routes through — never an isolated
// class-only path production bypasses). Covers: worker lifecycle
// (admission/retirement/eviction/pending), the worker-removed-during-
// withTarget swap-restore regression (Python _TargetScope analog: main
// context must restore unconditionally), chain ordering + rejection
// safety + swap/mutation independence, and pool/close single-winner
// behavior. No BreakpointStore/StopCoordinator exists (rejected — see
// nodebridge.js M4 owners head note); breakpoint/freshness state stays
// Session-owned and is covered by the existing matrices.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
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
  'Session, WorkerRegistry, SerialChain, ServerState, MAX_ACTIVE_HANDLERS, BridgeErr');

function tmpdir(prefix) {
  return fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), prefix)));
}

function freshSession(dir) {
  const st = new node.Session({
    kind: 'launch', dir, program: path.join(dir, 'main.js'),
    nodeBin: 'node', srcs: [], breaks: [], logpoints: [],
    wantExc: false, timeout: 20, programArgs: [], workers: true,
  });
  st.exited = false;
  st.cdp = { request: async () => ({}) };
  return st;
}

function mkWorker(sid, over = {}) {
  return {
    id: `worker:${sid}`, sessionId: sid, state: 'running',
    paused: null, stopInfo: null, lastStop: null,
    stopStates: [], targetRaws: new Map(), inheritedKeys: new Set(),
    breakKeys: new Map(), breakRecByKey: new Map(),
    logpoints: [], scripts: new Map(), urls: new Map(),
    seq: 0, stopSeq: 0, cachedLocals: [], lastChanged: '[]',
    lastTop: null, lastFunc: null, lastRemoved: '[]',
    lastChangedComplete: false,
    lastChangeTracking: {
      complete: false, scanned: 0, total: null, truncated: false,
      reason: 'first-snapshot',
    },
    lastTrackWarn: null, lastTrackComplete: false,
    lastTrackReason: 'first-snapshot',
    awaitingStep: false, exited: false,
    observed: { url: `file:///w${sid}.js`, type: 'worker', endpoint: 'ws://x' },
    ...over,
  };
}

function mainPark() {
  return {
    frames: [{
      callFrameId: 'm1', functionName: 'main', scopeChain: [],
      location: { scriptId: 's1', lineNumber: 3 },
    }],
    stopInfo: null,
  };
}

// ---- WorkerRegistry: admission is claimed, tracked, never reused ---------

test('owners: worker ids are claimed once and never reused', () => {
  const st = freshSession(tmpdir('m4-own-claim-'));
  assert.equal(st.workers.claimId('worker:s1'), true);
  assert.equal(st.workers.claimId('worker:s1'), false);
  st.workers.track(mkWorker('s1'));
  st.workers.assertValid();
  assert.ok(st.workers.has('worker:s1'));
  assert.equal(st.workers.liveWorkers().length, 1);
  assert.equal(st.workers.activeWorkers().length, 1);
});

test('owners: roster + resolve follow admission and retirement', () => {
  const st = freshSession(tmpdir('m4-own-roster-'));
  st.workers.claimId('worker:a');
  st.workers.track(mkWorker('a'));
  st.workers.claimId('worker:b');
  st.workers.track(mkWorker('b', { state: 'running' }));
  let roster = st.cmdTargets();
  assert.deepEqual(roster.targets.map((e) => e.id), ['main', 'worker:a', 'worker:b']);
  assert.equal(roster.ignored, 0);
  assert.equal(st.resolveTarget({ target: 'worker:a' }), 'worker:a');
  st.workers.noteExit('worker:a');
  st.workers.assertValid();
  roster = st.cmdTargets();
  assert.deepEqual(roster.targets.map((e) => e.id), ['main', 'worker:b', 'worker:a']);
  assert.equal(roster.targets[2].state, 'exited');
  assert.throws(() => st.resolveTarget({ target: 'worker:a' }), /has exited/);
  assert.throws(() => st.resolveTarget({ target: 'worker:ghost' }), /unknown target/);
});

test('owners: released workers resolve as released, retire as exited', () => {
  const st = freshSession(tmpdir('m4-own-rel-'));
  st.workers.claimId('worker:r');
  const w = mkWorker('r');
  w.state = 'ignored';
  st.workers.release(w);
  assert.equal(st.workers.ignored, 1);
  assert.throws(() => st.resolveTarget({ target: 'worker:r' }), /released \(over budget\)/);
  st.workers.noteExit('worker:r');
  assert.throws(() => st.resolveTarget({ target: 'worker:r' }), /has exited/);
  st.workers.assertValid();
});

test('owners: retirement rejects in-flight worker replies', async () => {
  const st = freshSession(tmpdir('m4-own-pend-'));
  st.workers.claimId('worker:p');
  st.workers.track(mkWorker('p'));
  const settled = new Promise((resolve) => {
    st.workers.putPending('p:7', {
      resolve: () => resolve('resolved-unexpectedly'),
      reject: (e) => resolve(`rejected: ${(e && e.message) || e}`),
    });
  });
  st.workers.noteExit('worker:p');
  assert.match(await settled, /rejected: worker session ended/);
  assert.equal(st.workers.pending.size, 0);
  st.workers.assertValid();
});

test('owners: exited history and ignored retention stay bounded', () => {
  const st = freshSession(tmpdir('m4-own-bound-'));
  for (let i = 0; i < 20; i++) {
    st.workers.claimId(`worker:e${i}`);
    st.workers.track(mkWorker(`e${i}`));
    st.workers.noteExit(`worker:e${i}`);
  }
  assert.equal(st.workers.exited.length, 16);
  assert.equal(st.workers.droppedExited, 4);
  for (let i = 0; i < 20; i++) {
    st.workers.claimId(`worker:g${i}`);
    const w = mkWorker(`g${i}`);
    w.state = 'ignored';
    st.workers.release(w);
  }
  const retained = st.workers.order.filter((id) => st.workers.has(id)).length;
  assert.equal(retained, 16);
  st.workers.assertValid();
  // Exact bound that matters: at most 16 ignored entries stay in the table.
  const ignored = st.workers.order.filter((id) => {
    const w = st.workers.get(id);
    return w && w.state === 'ignored';
  });
  assert.equal(ignored.length, 16);
});

// ---- swap-restore regression: worker removed mid-withTarget --------------

test('owners: worker removed during withTarget still restores main state', async () => {
  const st = freshSession(tmpdir('m4-own-swap-'));
  st.workers.claimId('worker:v');
  const w = mkWorker('v');
  w.scripts.set('ws1', 'file:///wv.js');
  st.workers.track(w);
  const mainScripts = new Map([['s1', 'file:///main.js']]);
  st.scripts = mainScripts;
  st.paused = mainPark();
  st.mainSeq = 3;
  const seen = await st.withTarget('worker:v', async () => {
    assert.equal(st.serving, 'worker:v');
    assert.deepEqual(st.scripts, w.scripts);
    // The worker exits mid-command (table entry moves to history).
    st.workers.noteExit('worker:v');
    return 'done';
  });
  assert.equal(seen, 'done');
  // Main context restores unconditionally — never stranded on the worker.
  assert.deepEqual(st.paused, mainPark());
  assert.equal(st.serving, 'main');
  assert.equal(st.sender, null);
  assert.equal(st.scripts, mainScripts);
  assert.throws(() => st.resolveTarget({ target: 'worker:v' }), /has exited/);
  st.workers.assertValid();
  st.server.assertValid(node.MAX_ACTIVE_HANDLERS);
});

test('owners: withTarget on main runs serialized without swapping', async () => {
  const st = freshSession(tmpdir('m4-own-main-'));
  st.paused = mainPark();
  const out = await st.withTarget('main', async () => {
    assert.equal(st.serving, 'main');
    return 42;
  });
  assert.equal(out, 42);
  assert.deepEqual(st.paused, mainPark());
  assert.equal(st.serving, 'main');
});

// ---- SerialChain: order, rejection safety, independence -------------------

test('owners: swap chain serializes and survives rejections', async () => {
  const st = freshSession(tmpdir('m4-own-chain-'));
  const order = [];
  const slow = st._swapRun(async () => {
    await new Promise((r) => setTimeout(r, 50));
    order.push('slow');
  });
  const fast = st._swapRun(async () => { order.push('fast'); });
  await Promise.all([slow, fast]);
  assert.deepEqual(order, ['slow', 'fast']);
  await assert.rejects(st._swapRun(async () => { throw new Error('boom'); }), /boom/);
  st._swapChain.assertValid();
  // The chain still drains after a rejection.
  await st._swapRun(async () => { order.push('after'); });
  assert.deepEqual(order, ['slow', 'fast', 'after']);
});

test('owners: mutation chain is independent of the swap chain', async () => {
  const st = freshSession(tmpdir('m4-own-indep-'));
  let releaseSwap;
  const gate = new Promise((r) => { releaseSwap = r; });
  const blocked = st._swapRun(() => gate);
  let mutated = false;
  await st._mutationRun(async () => { mutated = true; });
  assert.equal(mutated, true);
  releaseSwap();
  await blocked;
  st._swapChain.assertValid();
  st._mutationChain.assertValid();
  st._pauseChain.assertValid();
});

// ---- ServerState: pool bound + close single-winner ------------------------

test('owners: pool admits to the bound, then overloads', () => {
  const st = freshSession(tmpdir('m4-own-pool-'));
  for (let i = 0; i < node.MAX_ACTIVE_HANDLERS; i++) {
    assert.equal(st.server.tryAcquire(node.MAX_ACTIVE_HANDLERS), true);
  }
  assert.equal(st.server.tryAcquire(node.MAX_ACTIVE_HANDLERS), false);
  st.server.release();
  assert.equal(st.server.tryAcquire(node.MAX_ACTIVE_HANDLERS), true);
  for (let i = 0; i < node.MAX_ACTIVE_HANDLERS; i++) st.server.release();
  st.server.assertValid(node.MAX_ACTIVE_HANDLERS);
  assert.equal(st.server.active, 0);
});

test('owners: release without acquire fails loudly, pool unchanged', () => {
  const st = freshSession(tmpdir('m4-own-relneg-'));
  assert.throws(() => st.server.release(), /release without acquire/);
  assert.equal(st.server.active, 0);
  st.server.assertValid(node.MAX_ACTIVE_HANDLERS);
  // Balanced acquire/release still works after the failed release.
  assert.equal(st.server.tryAcquire(node.MAX_ACTIVE_HANDLERS), true);
  st.server.release();
  assert.equal(st.server.active, 0);
});

test('owners: close has exactly one winner through the Session surface', async () => {
  const st = freshSession(tmpdir('m4-own-close-'));
  let cleanups = 0;
  st.cleanup = async () => { cleanups += 1; };
  // First claim wins; the loser still reports closed but never tears down.
  assert.equal(st.server.claimClose(), true);
  assert.equal(st.server.claimClose(), false);
  await st.cleanup().catch(() => {});
  assert.equal(cleanups, 1);
  assert.equal(st.server.closing, true);
  await assert.rejects(st.dispatch({ cmd: 'threads' }), /session is closing/);
  st.server.assertValid(node.MAX_ACTIVE_HANDLERS);
});
