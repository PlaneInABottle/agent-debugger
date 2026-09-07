const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const Module = require('node:module');

// Load the real nodebridge with only the trailing main() invocation
// stripped, so Session runs for real against stubbed transports.
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

const node = loadBridge('bridge/node/src/nodebridge.js', 'Session, parseBreak, canon');

function tmpdir(prefix) {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

function writeJs(dir, name = 'w_add.js', lines = 12) {
  const file = path.join(dir, name);
  fs.writeFileSync(file,
    Array.from({ length: lines }, (_, i) => `const v${i} = ${i};`).join('\n') + '\n');
  return fs.realpathSync(file);
}

function workerSession(dir, over = {}) {
  const st = new node.Session({
    kind: 'launch', dir, program: path.join(dir, 'main.js'),
    nodeBin: 'node', srcs: [], breaks: [], logpoints: [],
    wantExc: false, timeout: 20, programArgs: [], workers: true,
    ...over,
  });
  st.exited = false;
  st.cdp = { request: async () => ({}) };
  return st;
}

function workerFrame(callFrameId = 'w1', lineNumber = 4) {
  return {
    callFrameId, functionName: 'work', scopeChain: [],
    location: { scriptId: 'ws1', lineNumber },
  };
}

// ---- roster + selection ----------------------------------------------------

test('worker targets: roster shape, auto-select, unknown/exited errors', async () => {
  const dir = tmpdir('wt-roster-');
  const st = workerSession(dir);
  let resp = st.cmdTargets();
  assert.equal(resp.ok, true);
  assert.equal(resp.target, 'main');
  assert.deepEqual(resp.targets.map((t) => t.id), ['main']);
  assert.equal(resp.selected, 'main');
  assert.equal(resp.ignored, 0);
  assert.equal(resp.droppedExited, 0);
  assert.equal(resp.targets[0].kind, 'main');
  assert.equal(resp.targets[0].scope, 'global');

  // Two stopped workers: most recent stop wins auto-select.
  const mk = (sid) => ({
    id: `worker:${sid}`, sessionId: sid, state: 'running', paused: null,
    stopInfo: null, lastStop: null, stopStates: [], targetRaws: new Map(),
    inheritedKeys: new Set(), breakKeys: new Map(), breakRecByKey: new Map(),
    logpoints: [], scripts: new Map(), urls: new Map(), seq: 0, stopSeq: 0,
    cachedLocals: [], lastChanged: '[]', lastTop: null, lastFunc: null,
    awaitingStep: false, exited: false,
    observed: { url: `file:///w${sid}.js`, type: 'worker', endpoint: 'ws://x' },
  });
  const a = mk('a');
  const b = mk('b');
  assert.equal(st.workers.claimId(a.id), true);
  st.workers.track(a);
  assert.equal(st.workers.claimId(b.id), true);
  st.workers.track(b);
  a.paused = { frames: [workerFrame()], stopInfo: null };
  a.state = 'stopped';
  st.stopSeq += 1;
  a.stopSeq = st.stopSeq;
  b.paused = { frames: [workerFrame()], stopInfo: null };
  b.state = 'stopped';
  st.stopSeq += 1;
  b.stopSeq = st.stopSeq;
  assert.equal(st.resolveTarget({}), 'worker:b');
  assert.equal(st.resolveTarget({ target: 'worker:a' }), 'worker:a');
  await assert.rejects(st.routeRead({ target: 'worker:zz' }, async () => ({})), /unknown target/);
  st.workers.noteExit('worker:a');
  await assert.rejects(st.routeRead({ target: 'worker:a' }, async () => ({})), /has exited/);
  resp = st.cmdTargets();
  assert.ok(resp.targets.some((t) => t.id === 'worker:a' && t.state === 'exited'));
});

// ---- accept + inherit + reason-other hit -----------------------------------

test('worker accept: inherits global breaks, resumes; reason-other with hitBreakpoints parks', async () => {
  const dir = tmpdir('wt-accept-');
  const file = writeJs(dir);
  const st = workerSession(dir, { breaks: [{ path: file, line: 5, cond: null }] });
  const sent = [];
  st.workerSend = async (w, method, params) => {
    sent.push(method);
    if (method === 'Debugger.setBreakpointByUrl') {
      return { breakpointId: 'bp-w1', locations: [{ lineNumber: 4 }] };
    }
    return {};
  };
  await st.acceptWorker({ sessionId: 's1', workerInfo: { url: `file://${file}`, type: 'worker' } });
  const w = st.workers.table.get('worker:s1');
  assert.ok(w, 'worker tracked');
  assert.equal(w.state, 'running');
  assert.equal(w.stopStates.length, 1);
  assert.equal(w.stopStates[0].state, 'verified');
  assert.ok(w.inheritedKeys.has(`${file}:5|`));
  assert.deepEqual(sent, ['Debugger.enable', 'Runtime.enable',
    'Debugger.setBreakpointByUrl', 'Debugger.resume', 'Runtime.runIfWaitingForDebugger']);
  assert.equal(w.observed.url, `file://${file}`);
  assert.equal(w.observed.type, 'worker');

  // reason 'other' still parks when hitBreakpoints names our plant (M4 rule).
  await st.onWorkerPaused(w, {
    reason: 'other', hitBreakpoints: ['bp-w1'], callFrames: [workerFrame()],
  });
  assert.ok(w.paused, 'worker parked on hitBreakpoints despite reason other');
  assert.equal(w.state, 'stopped');
  assert.equal(st.lastParkTarget, 'worker:s1');
  assert.equal(w.stopStates[0].hits, 1);
  assert.ok(w.lastStop, 'worker last stop recorded');

  // Bare breaks aggregates main + worker records with target tags.
  const breaks = st.cmdBreaks({});
  const mine = breaks.stops.filter((r) => r.target === 'worker:s1');
  assert.equal(mine.length, 1);
  assert.equal(breaks.target, 'worker:s1', 'auto-select serves the parked worker');
});

// ---- ephemeral add/remove round-trip ---------------------------------------

test('worker ephemeral add/remove touches only that worker', async () => {
  const dir = tmpdir('wt-eph-');
  const file = writeJs(dir);
  const st = workerSession(dir);
  st.workerSend = async () => ({ breakpointId: 'bp-e1', locations: [{ lineNumber: 6 }] });
  const wid = 'worker:e1';
  assert.equal(st.workers.claimId(wid), true);
  st.workers.track({
    id: wid, sessionId: 'e1', state: 'running', paused: null, stopInfo: null,
    lastStop: null, stopStates: [], targetRaws: new Map(), inheritedKeys: new Set(),
    breakKeys: new Map(), breakRecByKey: new Map(), logpoints: [],
    scripts: new Map(), urls: new Map(), seq: 0, stopSeq: 0,
    cachedLocals: [], lastChanged: '[]', lastTop: null, lastFunc: null,
    awaitingStep: false, exited: false,
    observed: { url: null, type: 'worker', endpoint: null },
  });
  const added = await st.cmdBreaksAdd({ target: wid, breaks: [`${file}:7`] });
  assert.equal(added.ok, true);
  assert.equal(added.target, wid);
  assert.equal(added.added[0].raw, `${file}:7`);
  // Ephemeral: global intent untouched.
  assert.deepEqual(st.cfg.breaks, []);
  const w = st.workers.table.get(wid);
  assert.equal(w.stopStates.length, 1);
  assert.ok([...w.targetRaws.keys()].some((k) => k.startsWith(`${file}:7|`)));
  // Scoped remove drops only the ephemeral record.
  const removed = await st.cmdBreaksRemove({ target: wid, breaks: [`${file}:7`] });
  assert.equal(removed.ok, true);
  assert.equal(removed.target, wid);
  assert.equal(removed.removed[0].raw, `${file}:7`);
  assert.equal(w.stopStates.length, 0);
  assert.equal(w.targetRaws.size, 0);
  // Scoped remove of a global-only line reports missing, never global.
  st.cfg.breaks.push({ path: file, line: 5, cond: null });
  st.breakRaws.set(`${file}:5|`, `${file}:5`);
  const miss = await st.cmdBreaksRemove({ target: wid, breaks: [`${file}:5`] });
  assert.deepEqual(miss.removed, []);
  assert.deepEqual(miss.missing, [`${file}:5`]);
  assert.equal(st.cfg.breaks.length, 1);
});

// ---- overflow + exit bounds -------------------------------------------------

test('worker overflow releases with resume+gate; exits bound history', async () => {
  const dir = tmpdir('wt-over-');
  const st = workerSession(dir);
  st.workerSend = async () => ({ breakpointId: 'bp', locations: [] });
  const kicks = [];
  st.cdp = {
    request: async (method, params) => {
      if (method === 'NodeWorker.sendMessageToWorker') {
        kicks.push(JSON.parse(params.message).method);
      }
      return {};
    },
  };
  for (let i = 0; i < 9; i++) {
    await st.acceptWorker({ sessionId: `s${i}`, workerInfo: { url: `file:///w${i}.js`, type: 'worker' } });
  }
  assert.equal(st.workers.activeWorkers().length, 8);
  assert.equal(st.workers.ignored, 1);
  // The release kick carries BOTH the resume and the run gate (either
  // stuck state alone would hang the app).
  assert.deepEqual(kicks.slice(-2).sort(),
    ['Debugger.resume', 'Runtime.runIfWaitingForDebugger'].sort());
  const ninth = st.workers.table.get('worker:s8');
  assert.equal(ninth.state, 'ignored');
  await assert.rejects(st.routeRead({ target: 'worker:s8' }, async () => ({})), /released/);
  // An untracked pause auto-kicks with both messages, never parks.
  kicks.length = 0;
  await st.routeWorkerMessage({
    sessionId: 'ghost', message: JSON.stringify({ method: 'Debugger.paused', params: {} }),
  });
  assert.deepEqual(kicks.sort(),
    ['Debugger.resume', 'Runtime.runIfWaitingForDebugger'].sort());
  assert.ok(!st.workers.table.has('worker:ghost'));
  // Natural exits retire to bounded history.
  for (let i = 0; i < 8; i++) st.workers.noteExit(`worker:s${i}`);
  assert.equal(st.workers.exited.length, 8);
  for (let i = 0; i < 10; i++) {
    const id = `worker:x${i}`;
    assert.equal(st.workers.claimId(id), true);
    st.workers.track({ id, sessionId: `x${i}`, state: 'running', paused: null, lastStop: null, stopStates: [], targetRaws: new Map(), inheritedKeys: new Set(), observed: {} });
    st.workers.noteExit(id);
  }
  assert.equal(st.workers.exited.length, 16);
  assert.ok(st.workers.droppedExited > 0);
});

// ---- response target stamp --------------------------------------------------

test('worker inherit: logpoints plant but never join inherited break keys', async () => {
  const dir = tmpdir('wt-logimp-');
  const file = writeJs(dir);
  const st = workerSession(dir, {
    breaks: [{ path: file, line: 5, cond: null }],
    logpoints: [{ path: file, line: 7, template: 'v={v6}' }],
  });
  st.workerSend = async () => ({ breakpointId: `bp-${Math.random()}`, locations: [{ lineNumber: 4 }] });
  await st.acceptWorker({ sessionId: 'lp', workerInfo: { url: `file://${file}`, type: 'worker' } });
  const w = st.workers.table.get('worker:lp');
  assert.ok(w.stopStates.some((r) => r.kind === 'break'));
  assert.ok(w.stopStates.some((r) => r.kind === 'logpoint'));
  // Break keys tracked; logpoint lines are NOT inherited break keys, so a
  // global remove/clear by break key can never drop the logpoint plant.
  assert.ok(w.inheritedKeys.has(`${file}:5|`));
  assert.ok(![...w.inheritedKeys].some((k) => k.startsWith(`${file}:7|`)));
  const doomed = st.workerAllKeys(w);
  assert.ok(!doomed.some((k) => k.startsWith(`${file}:7|`)));
  await st.dropWorkerKeys('worker:lp', doomed, []);
  assert.ok(w.stopStates.some((r) => r.kind === 'logpoint'),
    'logpoint plant survives a full break-key reset');
  assert.ok(!w.stopStates.some((r) => r.kind === 'break'));
});

test('worker pauses serialize: no cross-target state bleed', async () => {
  // Main pause (two local scopes, first deferred) + worker pause fired
  // without awaiting either. Without serialization the worker link would
  // clobber the shared sender mid-main-locals and the second scope would
  // misroute to the worker session (which rejects foreign objectIds),
  // wiping main locals. With the chain each link completes intact.
  const dir = tmpdir('wt-race-');
  const st = new node.Session({
    kind: 'launch', dir, program: path.join(dir, 'main.js'),
    nodeBin: 'node', srcs: [], breaks: [], logpoints: [],
    wantExc: false, timeout: 20, programArgs: [], workers: true,
  });
  st.exited = false;
  st.sessionPort = 1;
  let resolveMainScope;
  const mainGate = new Promise((r) => { resolveMainScope = r; });
  let resolveWorkerScope;
  const workerGate = new Promise((r) => { resolveWorkerScope = r; });
  st.cdp = {
    request: async (method, params = {}) => {
      if (method === 'Runtime.getProperties') {
        if (params.objectId === 'objMain1') return mainGate.then(() => ({
          result: [{ name: 'mVar1', value: { type: 'number', value: 1 }, enumerable: true }],
        }));
        if (params.objectId === 'objMain2') {
          return { result: [{ name: 'mVar2', value: { type: 'number', value: 2 }, enumerable: true }] };
        }
      }
      return {};
    },
  };
  const mainRec = { spec: 'main.js:2', kind: 'break', hits: 0, state: 'verified' };
  st.breakIdToRec.set('bpM', { rec: mainRec, line: 2 });
  st.stopStates.push(mainRec);
  const wid = 'worker:w';
  const wRec = { spec: 'w.js:3', kind: 'break', hits: 0, state: 'verified' };
  st.breakIdToRec.set('bpW', { rec: wRec, line: 3 });
  assert.equal(st.workers.claimId(wid), true);
  st.workers.track({
    id: wid, sessionId: 'w', state: 'running', paused: null, stopInfo: null,
    lastStop: null, stopStates: [wRec], targetRaws: new Map(), inheritedKeys: new Set(),
    breakKeys: new Map(), breakRecByKey: new Map(), logpoints: [],
    scripts: new Map(), urls: new Map(), seq: 0, stopSeq: 0,
    cachedLocals: [], lastChanged: '[]', lastTop: null, lastFunc: null,
    awaitingStep: false, exited: false,
    observed: { url: 'file:///w.js', type: 'worker', endpoint: null },
  });
  st.workerSend = async (w, method, params = {}) => {
    if (method === 'Runtime.getProperties') {
      if (params.objectId === 'objW') {
        return workerGate.then(() => ({
          result: [{ name: 'wVar', value: { type: 'number', value: 9 }, enumerable: true }],
        }));
      }
      throw new Error('no such object in worker session');
    }
    return {};
  };
  const mainFrame = {
    callFrameId: 'm1', functionName: 'main',
    location: { scriptId: 's', lineNumber: 1 },
    scopeChain: [
      { type: 'local', object: { objectId: 'objMain1' } },
      { type: 'block', object: { objectId: 'objMain2' } },
    ],
    url: 'file:///app.js',
  };
  const workerFrame = {
    callFrameId: 'w1', functionName: 'work',
    location: { scriptId: 'ws', lineNumber: 2 },
    scopeChain: [{ type: 'local', object: { objectId: 'objW' } }],
    url: 'file:///w.js',
  };
  const p1 = st.handleEvent({
    method: 'Debugger.paused',
    params: { reason: 'other', hitBreakpoints: ['bpM'], callFrames: [mainFrame] },
  });
  const p2 = st.routeWorkerMessage({
    sessionId: 'w',
    message: JSON.stringify({
      method: 'Debugger.paused',
      params: { reason: 'other', hitBreakpoints: ['bpW'], callFrames: [workerFrame] },
    }),
  });
  resolveMainScope();
  await p1;
  resolveWorkerScope();
  await p2;
  // Both links complete: main park intact with BOTH scopes' locals (the
  // second scope misroutes without the chain and main locals collapse to
  // []), worker parked with its own attribution.
  assert.ok(st.paused, 'main park survives the interleaved worker pause');
  const mainNames = (st.cachedLocals || []).map((l) => l.name).sort();
  assert.deepEqual(mainNames, ['mVar1', 'mVar2']);
  // Worker parked with its own attribution.
  const w = st.workers.table.get(wid);
  assert.ok(w.paused, 'worker parked');
  assert.deepEqual((w.cachedLocals || []).map((l) => l.name), ['wVar']);
  assert.equal(wRec.hits, 1);
  assert.equal(mainRec.hits, 1);
  assert.equal(st.lastParkTarget, wid);
});

test('worker ignored detach retires to exited history', async () => {
  // A released (ignored) worker that runs out on its own must become
  // observable as exited — detachedFromWorker retires table entries of
  // any state, so progress is visible, not just the ignored count.
  const dir = tmpdir('wt-ign-exit-');
  const st = workerSession(dir);
  const wid = 'worker:zz';
  assert.equal(st.workers.claimId(wid), true);
  st.workers.release({
    id: wid, sessionId: 'zz', state: 'ignored', paused: null, stopInfo: null,
    lastStop: null, stopStates: [], targetRaws: new Map(), inheritedKeys: new Set(),
    breakKeys: new Map(), breakRecByKey: new Map(), logpoints: [],
    scripts: new Map(), urls: new Map(), seq: 0, stopSeq: 0,
    cachedLocals: [], lastChanged: '[]', lastTop: null, lastFunc: null,
    awaitingStep: false, exited: false,
    observed: { url: null, type: 'worker', endpoint: null },
  });
  // Detach arrives as a top-level NodeWorker event (not wrapped).
  await st.handleEvent({ method: 'NodeWorker.detachedFromWorker', params: { sessionId: 'zz' } });
  assert.ok(!st.workers.table.has(wid));
  const hist = st.workers.exited.find((e) => e.id === wid);
  assert.ok(hist && hist.state === 'exited');
  assert.equal(st.workers.ignored, 1, 'lifetime counter survives retirement');
  // Unknown sessions stay untracked (no phantom history).
  await st.handleEvent({ method: 'NodeWorker.detachedFromWorker', params: { sessionId: 'nope' } });
  assert.ok(!st.workers.exited.some((e) => e.id === 'worker:nope'));
});

test('worker bridge: every served response names its target', async () => {
  const dir = tmpdir('wt-stamp-');
  const st = workerSession(dir);
  st.cdp = { request: async () => ({ threads: [] }) };
  const threads = await st.cmdThreads({});
  assert.equal(threads.target, 'main');
  const breaks = st.cmdBreaks({});
  assert.equal(breaks.target, 'main');
  const targets = st.cmdTargets();
  assert.equal(targets.target, 'main');
});

test('bare threads aggregates main plus live workers; explicit stays single', async () => {
  const dir = tmpdir('wt-threads-agg-');
  const st = workerSession(dir);
  st.cdp = { request: async () => ({ threads: [] }) };
  // Single-target bare: legacy shape, no aggregation keys.
  let resp = await st.cmdThreads({});
  assert.equal(resp.ok, true);
  assert.equal(resp.target, 'main');
  assert.ok(!('targets' in resp), 'no aggregation without workers');
  assert.ok(!('selected' in resp), 'no selected without workers');

  const mk = (sid, state = 'running', paused = null) => ({
    id: `worker:${sid}`, sessionId: sid, state, paused,
    stopInfo: null, lastStop: null, stopStates: [], targetRaws: new Map(),
    inheritedKeys: new Set(), breakKeys: new Map(), breakRecByKey: new Map(),
    logpoints: [], scripts: new Map(), urls: new Map(), seq: 0, stopSeq: 0,
    cachedLocals: [], lastChanged: '[]', lastTop: null, lastFunc: null,
    awaitingStep: false, exited: false,
    observed: { url: `file:///w${sid}.js`, type: 'worker', endpoint: 'ws://x' },
  });
  // Parked worker newer than main: auto-select serves the worker.
  const w = mk('a', 'stopped', { frames: [workerFrame()], stopInfo: null });
  st.stopSeq += 1;
  w.stopSeq = st.stopSeq;
  assert.equal(st.workers.claimId(w.id), true);
  st.workers.track(w);
  // Ignored releases never join the aggregate.
  const ign = mk('b', 'ignored');
  assert.equal(st.workers.claimId(ign.id), true);
  st.workers.release(ign);

  resp = await st.cmdThreads({});
  assert.equal(resp.ok, true);
  assert.equal(resp.selected, w.id);
  assert.equal(resp.target, w.id);
  assert.deepEqual(resp.targets.map((e) => e.target), ['main', w.id]);
  const [mainE, workE] = resp.targets;
  assert.equal(mainE.running, true);
  assert.equal(workE.running, false);
  assert.equal(workE.threads.length, 1);
  assert.equal(workE.threads[0].name, 'worker');
  // Top-level stays the selected target's dump.
  assert.equal(resp.running, workE.running);
  assert.deepEqual(resp.threads, workE.threads);

  // Busy worker appears truthfully running without blocking.
  st.outstanding.set(w.id, 'continue');
  resp = await st.cmdThreads({});
  const got = Object.fromEntries(resp.targets.map((e) => [e.target, e]));
  assert.equal(got[w.id].running, true);
  assert.deepEqual(got[w.id].threads, []);
  assert.ok(got.main, 'main still listed');
  st.outstanding.delete(w.id);

  // Explicit main pins main even though the worker stopped later.
  const one = await st.cmdThreads({ target: 'main' });
  assert.equal(one.target, 'main');
  assert.ok(!('targets' in one), 'explicit main keeps the single shape');
  assert.ok(!('selected' in one), 'explicit main keeps the single shape');
  assert.equal(one.threads[0].name, 'main');

  // Explicit worker still serves only that worker.
  const two = await st.cmdThreads({ target: w.id });
  assert.equal(two.target, w.id);
  assert.ok(!('targets' in two), 'explicit worker keeps the single shape');

  // Exited history never joins the aggregate.
  st.workers.noteExit(w.id);
  resp = await st.cmdThreads({});
  assert.ok(!('targets' in resp), 'lone main reads byte-identical');
  assert.equal(resp.target, 'main');
});

function scopedWorker(st, sid) {
  // A live worker entry with everything withTarget/snapshot needs.
  const w = {
    id: `worker:${sid}`, sessionId: sid, state: 'stopped',
    paused: {
      frames: [{
        callFrameId: 'w1', functionName: 'work', scopeChain: [],
        location: { scriptId: 'ws1', lineNumber: 4 },
      }],
      stopInfo: null,
    },
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

function mainPark(st) {
  st.paused = {
    frames: [{
      callFrameId: 'm1', functionName: 'main', scopeChain: [],
      location: { scriptId: 's1', lineNumber: 1 },
    }],
    stopInfo: null,
  };
}

test('explicit waits never serve another target; stale parks never satisfy', async () => {
  // Main running, worker parked BEFORE the wait (stale): explicit main
  // waits must time out main-scoped, never serve the worker, and leave
  // the worker parked and untouched.
  const dir = tmpdir('wt-wait-scope-');
  const st = workerSession(dir);
  st.cdp = { request: async () => ({}) };
  const w = scopedWorker(st, 's');
  // Faithful stale simulation: like the real pump with a pre-existing
  // park, the stub returns at once without parking anything new. (The
  // real pump would exit(0) on an unowned test dir — owner-guard — so
  // waiting loops are tested against this stub; live runs cover the
  // real pump.)
  st.pump = async () => 'stopped';
  await assert.rejects(st.cmdContinue({ target: 'main' }, 1),
    /timeout: no stop within 1/);
  assert.ok(w.paused, 'stale worker park untouched by explicit continue');
  assert.equal(st.paused, null);
  await assert.rejects(st.cmdWait({ target: 'main' }, 1),
    /timeout: no stop within 1/);
  assert.ok(w.paused, 'stale worker park untouched by explicit wait');
  // Omitted resume/step with only the stale park: no instant stale serve.
  await assert.rejects(st.cmdContinue({}, 1), /timeout: no stop within 1/);
  assert.ok(w.paused, 'stale park not consumed by omitted continue');
});

test('explicit continue skips a fresh other-target stop, serves its own', async () => {
  // Worker stops fresh mid-wait, then main stops: the explicit main
  // waiter must skip the worker park (left parked, never resumed) and
  // serve only main's own stop.
  const dir = tmpdir('wt-wait-fresh-');
  const st = workerSession(dir);
  st.cdp = { request: async () => ({}) };
  const w = scopedWorker(st, 'f');
  w.paused = null;
  w.state = 'running';
  let n = 0;
  st.pump = async () => {
    n++;
    if (n === 1) {
      w.paused = {
        frames: [{
          callFrameId: 'w1', functionName: 'work', scopeChain: [],
          location: { scriptId: 'ws1', lineNumber: 4 },
        }],
        stopInfo: null,
      };
      w.state = 'stopped';
    } else {
      mainPark(st);
    }
    return 'stopped';
  };
  const resp = await st.cmdContinue({ target: 'main' }, 5);
  assert.equal(resp.target, 'main');
  assert.ok(w.paused, 'fresh worker park left for its own target');
  assert.equal(w.state, 'stopped');
});

test('explicit capture waits for its own target only', async () => {
  const dir = tmpdir('wt-cap-scope-');
  const st = workerSession(dir);
  st.cdp = { request: async () => ({}) };
  const w = scopedWorker(st, 'c');
  st.pump = async () => {
    // Stale worker park present the whole time; only main newly parks.
    mainPark(st);
    return 'stopped';
  };
  const resp = await st.cmdCapture({ target: 'main' }, 5);
  assert.equal(resp.target, 'main');
  assert.equal(resp.resumed, true);
  assert.ok(w.paused, 'stale worker park untouched by explicit capture');
});

test('bare threads never reads mid-swap: serializes with withTarget', async () => {
  // Hold the swap mutex inside withTarget(worker) across an await (main
  // fields currently show the worker park); a concurrent bare threads
  // must wait for it — not attribute worker frames to main.
  const dir = tmpdir('wt-swap-');
  const st = workerSession(dir);
  st.cdp = { request: async () => ({}) };
  st.paused = {
    frames: [{
      callFrameId: 'm1', functionName: 'main', scopeChain: [],
      location: { scriptId: 's1', lineNumber: 1 },
    }],
    stopInfo: null,
  };
  const w = scopedWorker(st, 'sw');
  let release;
  const gate = new Promise((r) => { release = r; });
  const holderP = st.withTarget(w.id, async () => {
    await gate;
    return 'held';
  });
  await new Promise((r) => setTimeout(r, 20)); // holder has swapped now
  const guard = new Promise((_, rej) =>
    setTimeout(() => rej(new Error('threads deadlocked behind swap')), 5000));
  const aggP = st.cmdThreads({});
  release();
  const agg = await Promise.race([aggP, guard]);
  assert.equal(await holderP, 'held');
  const got = Object.fromEntries(
    agg.targets.map((e) => [e.target, e.threads[0].frames[0].method]));
  assert.deepEqual(got, { main: 'main', 'worker:sw': 'work' });
});

test('bare threads skips a worker that exits mid-dump', async () => {
  // Deterministic churn: the worker exits between the roster snapshot
  // and its own dump. It is skipped (never fails the call), the exit
  // stays recorded, and the served entry is coherent.
  const dir = tmpdir('wt-churn-');
  const st = workerSession(dir);
  st.cdp = { request: async () => ({}) };
  st.paused = {
    frames: [{
      callFrameId: 'm1', functionName: 'main', scopeChain: [],
      location: { scriptId: 's1', lineNumber: 1 },
    }],
    stopInfo: null,
  };
  const w = scopedWorker(st, 'ch');
  w.stopSeq = 1;
  st.stopSeq = 1;
  const realSwap = st._swapRun.bind(st);
  let n = 0;
  st._swapRun = async (fn) => {
    n += 1;
    if (n === 1) st.workers.noteExit(w.id); // churn between snapshot and dump
    return realSwap(fn);
  };
  const resp = await st.cmdThreads({});
  assert.deepEqual(resp.targets.map((e) => e.target), ['main']);
  assert.equal(resp.selected, 'main');
  assert.equal(resp.target, 'main');
  assert.ok(st.workers.exited.some((e) => e.id === w.id), 'exit still recorded');
});
