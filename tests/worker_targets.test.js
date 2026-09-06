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
  st.workerTable.set(a.id, a);
  st.workerTable.set(b.id, b);
  st.workerOrder.push(a.id, b.id);
  st.seenWorkerIds.add(a.id);
  st.seenWorkerIds.add(b.id);
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
  st.noteWorkerExit('worker:a');
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
  const w = st.workerTable.get('worker:s1');
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
  st.workerTable.set(wid, {
    id: wid, sessionId: 'e1', state: 'running', paused: null, stopInfo: null,
    lastStop: null, stopStates: [], targetRaws: new Map(), inheritedKeys: new Set(),
    breakKeys: new Map(), breakRecByKey: new Map(), logpoints: [],
    scripts: new Map(), urls: new Map(), seq: 0, stopSeq: 0,
    cachedLocals: [], lastChanged: '[]', lastTop: null, lastFunc: null,
    awaitingStep: false, exited: false,
    observed: { url: null, type: 'worker', endpoint: null },
  });
  st.workerOrder.push(wid);
  st.seenWorkerIds.add(wid);
  const added = await st.cmdBreaksAdd({ target: wid, breaks: [`${file}:7`] });
  assert.equal(added.ok, true);
  assert.equal(added.target, wid);
  assert.equal(added.added[0].raw, `${file}:7`);
  // Ephemeral: global intent untouched.
  assert.deepEqual(st.cfg.breaks, []);
  const w = st.workerTable.get(wid);
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
  assert.equal(st.activeWorkers().length, 8);
  assert.equal(st.ignoredWorkers, 1);
  // The release kick carries BOTH the resume and the run gate (either
  // stuck state alone would hang the app).
  assert.deepEqual(kicks.slice(-2).sort(),
    ['Debugger.resume', 'Runtime.runIfWaitingForDebugger'].sort());
  const ninth = st.workerTable.get('worker:s8');
  assert.equal(ninth.state, 'ignored');
  await assert.rejects(st.routeRead({ target: 'worker:s8' }, async () => ({})), /released/);
  // An untracked pause auto-kicks with both messages, never parks.
  kicks.length = 0;
  await st.routeWorkerMessage({
    sessionId: 'ghost', message: JSON.stringify({ method: 'Debugger.paused', params: {} }),
  });
  assert.deepEqual(kicks.sort(),
    ['Debugger.resume', 'Runtime.runIfWaitingForDebugger'].sort());
  assert.ok(!st.workerTable.has('worker:ghost'));
  // Natural exits retire to bounded history.
  for (let i = 0; i < 8; i++) st.noteWorkerExit(`worker:s${i}`);
  assert.equal(st.exitedWorkers.length, 8);
  for (let i = 0; i < 10; i++) {
    const id = `worker:x${i}`;
    st.seenWorkerIds.add(id);
    st.workerTable.set(id, { id, sessionId: `x${i}`, state: 'running', paused: null, lastStop: null, stopStates: [], targetRaws: new Map(), inheritedKeys: new Set(), observed: {} });
    st.workerOrder.push(id);
    st.noteWorkerExit(id);
  }
  assert.equal(st.exitedWorkers.length, 16);
  assert.ok(st.droppedWorkerExited > 0);
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
  const w = st.workerTable.get('worker:lp');
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
  st.workerTable.set(wid, {
    id: wid, sessionId: 'w', state: 'running', paused: null, stopInfo: null,
    lastStop: null, stopStates: [wRec], targetRaws: new Map(), inheritedKeys: new Set(),
    breakKeys: new Map(), breakRecByKey: new Map(), logpoints: [],
    scripts: new Map(), urls: new Map(), seq: 0, stopSeq: 0,
    cachedLocals: [], lastChanged: '[]', lastTop: null, lastFunc: null,
    awaitingStep: false, exited: false,
    observed: { url: 'file:///w.js', type: 'worker', endpoint: null },
  });
  st.workerOrder.push(wid);
  st.seenWorkerIds.add(wid);
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
  const w = st.workerTable.get(wid);
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
  st.workerTable.set(wid, {
    id: wid, sessionId: 'zz', state: 'ignored', paused: null, stopInfo: null,
    lastStop: null, stopStates: [], targetRaws: new Map(), inheritedKeys: new Set(),
    breakKeys: new Map(), breakRecByKey: new Map(), logpoints: [],
    scripts: new Map(), urls: new Map(), seq: 0, stopSeq: 0,
    cachedLocals: [], lastChanged: '[]', lastTop: null, lastFunc: null,
    awaitingStep: false, exited: false,
    observed: { url: null, type: 'worker', endpoint: null },
  });
  st.workerOrder.push(wid);
  st.seenWorkerIds.add(wid);
  st.ignoredWorkers = 1;
  // Detach arrives as a top-level NodeWorker event (not wrapped).
  await st.handleEvent({ method: 'NodeWorker.detachedFromWorker', params: { sessionId: 'zz' } });
  assert.ok(!st.workerTable.has(wid));
  const hist = st.exitedWorkers.find((e) => e.id === wid);
  assert.ok(hist && hist.state === 'exited');
  assert.equal(st.ignoredWorkers, 1, 'lifetime counter survives retirement');
  // Unknown sessions stay untracked (no phantom history).
  await st.handleEvent({ method: 'NodeWorker.detachedFromWorker', params: { sessionId: 'nope' } });
  assert.ok(!st.exitedWorkers.some((e) => e.id === 'worker:nope'));
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
