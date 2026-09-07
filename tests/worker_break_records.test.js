// Milestone B: worker breakpoint record reachability (Node).
//
// Every displayed worker line-break record must be reachable by canonical
// key: keyed in breakKeys/breakRecByKey and admitted via inheritedKeys or
// targetRaws, so global/scoped remove/clear can eliminate it. A plant that
// throws (no backend plant, no intent key) warns without a record — never
// a phantom no command can drop. A V8 explicit reject (null bpId) keeps
// its keyed pending/rejected record, which remove/clear do eliminate.
//
// The invariant helper below inspects the raw Maps/Sets by record
// identity — it never calls the production match/drop helpers it guards.
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

const node = loadBridge('bridge/node/src/nodebridge.js', 'Session, canon');

function tmpdir(prefix) {
  return fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), prefix)));
}

function writeJs(dir, name = 'w.js', lines = 12) {
  const file = path.join(dir, name);
  fs.writeFileSync(file,
    Array.from({ length: lines }, (_, i) => `const v${i} = ${i};`).join('\n') + '\n');
  return file;
}

function nodeSession(dir) {
  const st = new node.Session({
    kind: 'attach', dir, host: 'localhost', port: 9229, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20, programArgs: [],
  });
  st.exited = false;
  return st;
}

// Main-side stub CDP: per-breakpoint plants/removals.
function stubMainCdp(st) {
  let seq = 0;
  st.cdp = {
    request: async (method, params) => {
      if (method === 'Debugger.setBreakpointByUrl') {
        return {
          breakpointId: `bp-${++seq}`,
          locations: [{ lineNumber: params.lineNumber }],
        };
      }
      if (method === 'Debugger.removeBreakpoint') return {};
      return {};
    },
  };
}

// Worker transport stub with per-test plant behavior:
// 'ok' plants, 'reject' answers without a breakpointId (V8 refusal),
// 'throw' raises (dead worker connection).
function stubWorkerSend(st, plantBehavior) {
  let seq = 0;
  st.workerSend = async (w, method, params) => {
    if (method === 'Debugger.setBreakpointByUrl') {
      if (plantBehavior === 'throw') throw new Error('worker gone');
      if (plantBehavior === 'reject') return {};
      return {
        breakpointId: `wbp-${++seq}`,
        locations: [{ lineNumber: params.lineNumber }],
      };
    }
    if (method === 'Debugger.removeBreakpoint') return {};
    return {};
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
  assert.equal(st.workers.claimId(w.id), true);
  st.workers.track(w);
  return w;
}

function workerBreakLines(w) {
  return w.stopStates.filter((r) => r.kind === 'break');
}

// Bookkeeping invariant, derived from the raw structures by record
// identity (never via matchWorkerBreak/workerAllKeys/drop helpers):
// - main: breakKeys/breakRaws/cfg intent agree; every displayed main
//   break record IS a breakRecByKey value whose key is planted.
// - worker: every displayed break record IS a breakRecByKey value whose
//   key is planted AND admitted (inherited or ephemeral); every admitted
//   or planted key resolves to a displayed record.
function assertBreakInvariants(st, workers = []) {
  const mainKeys = new Set(st.cfg.breaks.map((b) => `${b.path}:${b.line}|${b.cond || ''}`));
  assert.deepEqual(new Set(st.breakRaws.keys()), mainKeys);
  assert.deepEqual(new Set(st.breakKeys.keys()), mainKeys);
  for (const rec of st.stopStates.filter((r) => r.kind === 'break')) {
    const hit = [...st.breakRecByKey.entries()].find(([, v]) => v === rec);
    assert.ok(hit, `main record unreachable: ${rec.spec}`);
    assert.ok(st.breakKeys.has(hit[0]), `main key not planted: ${hit[0]}`);
  }
  for (const [key, rec] of st.breakRecByKey) {
    assert.ok(st.stopStates.includes(rec), `main orphan key: ${key}`);
  }
  for (const w of workers) {
    const admitted = new Set([...w.inheritedKeys, ...w.targetRaws.keys()]);
    for (const rec of workerBreakLines(w)) {
      const hit = [...w.breakRecByKey.entries()].find(([, v]) => v === rec);
      assert.ok(hit, `worker ${w.id} record unreachable: ${rec.spec}`);
      assert.ok(w.breakKeys.has(hit[0]), `worker ${w.id} key not planted: ${hit[0]}`);
      assert.ok(admitted.has(hit[0]),
        `worker ${w.id} record has no intent (phantom): ${hit[0]}`);
    }
    for (const key of new Set([...w.breakKeys.keys(), ...admitted])) {
      const rec = w.breakRecByKey.get(key);
      assert.ok(rec, `worker ${w.id} orphan key: ${key}`);
      assert.ok(w.stopStates.includes(rec), `worker ${w.id} key without record: ${key}`);
    }
  }
}

function captureStderr() {
  const lines = [];
  const orig = process.stderr.write;
  process.stderr.write = (s) => { lines.push(String(s)); return true; };
  return { lines, restore: () => { process.stderr.write = orig; } };
}

test('admission success plants keyed copies; invariants hold', async () => {
  const dir = tmpdir('b-rec-');
  const st = nodeSession(dir);
  stubMainCdp(st);
  stubWorkerSend(st, 'ok');
  const file = writeJs(dir);
  const raw = `${file}:5`;
  await st.cmdBreaksAdd({ breaks: [raw] });
  const w = addWorker(st, 'a');
  await st.plantInherited(w);
  assert.equal(w.inheritedKeys.size, 1);
  assert.equal(workerBreakLines(w).length, 1);
  assert.equal(workerBreakLines(w)[0].state, 'verified');
  assertBreakInvariants(st, [w]);
  const shown = (await st.cmdBreaks()).stops.filter((r) => r.target === w.id);
  assert.equal(shown.length, 1);
});

test('admission reject keeps a keyed rejected record; remove/clear eliminate it', async () => {
  const dir = tmpdir('b-rec-');
  const st = nodeSession(dir);
  stubMainCdp(st);
  stubWorkerSend(st, 'reject');
  const file = writeJs(dir);
  const raw = `${file}:5`;
  await st.cmdBreaksAdd({ breaks: [raw] });
  const w = addWorker(st, 'a');
  await st.plantInherited(w);
  // The V8 refusal is explicit and reachable: null plant, keyed record.
  assert.equal(workerBreakLines(w).length, 1);
  assert.equal(workerBreakLines(w)[0].state, 'rejected');
  assertBreakInvariants(st, [w]);
  // Global remove eliminates the worker copy with the main intent.
  const rm = await st.cmdBreaksRemove({ breaks: [raw] });
  assert.equal(rm.removed.length, 1);
  assert.equal(workerBreakLines(w).length, 0);
  assertBreakInvariants(st, [w]);
  assert.deepEqual(st.cfg.breaks, []);
});

test('admission throw warns with no phantom record; intent still removable', async () => {
  const dir = tmpdir('b-rec-');
  const st = nodeSession(dir);
  stubMainCdp(st);
  stubWorkerSend(st, 'throw');
  const file = writeJs(dir);
  const raw = `${file}:5`;
  await st.cmdBreaksAdd({ breaks: [raw] });
  const w = addWorker(st, 'a');
  const cap = captureStderr();
  try {
    await st.plantInherited(w);
  } finally {
    cap.restore();
  }
  assert.ok(cap.lines.some((l) => /inherit failed/.test(l)), 'throw warns');
  // No plant, no key, no record: nothing phantom to display or drop.
  assert.equal(workerBreakLines(w).length, 0);
  assert.equal(w.stopStates.length, 0);
  assert.equal(w.inheritedKeys.size, 0);
  assertBreakInvariants(st, [w]);
  const shown = (await st.cmdBreaks()).stops.filter((r) => r.target === w.id);
  assert.deepEqual(shown, []);
  // The global intent removes and clears cleanly around the failed worker.
  const rm = await st.cmdBreaksRemove({ breaks: [raw] });
  assert.equal(rm.removed.length, 1);
  assertBreakInvariants(st, [w]);
  const cleared = await st.cmdBreaksClear();
  assert.equal(cleared.removed.length, 0);
  assertBreakInvariants(st, [w]);
});

test('ephemeral add, scoped remove, and clear keep invariants', async () => {
  const dir = tmpdir('b-rec-');
  const st = nodeSession(dir);
  stubMainCdp(st);
  stubWorkerSend(st, 'ok');
  const file = writeJs(dir);
  await st.cmdBreaksAdd({ breaks: [`${file}:5`] });
  const w = addWorker(st, 'a');
  await st.plantInherited(w);
  const added = await st.addWorkerEphemeral(w.id, [`${file}:8`]);
  assert.equal(added.added.length, 1);
  assertBreakInvariants(st, [w]);
  const scoped = await st.cmdBreaksRemove({ target: w.id, breaks: [`${file}:8`] });
  assert.equal(scoped.removed.length, 1);
  assertBreakInvariants(st, [w]);
  assert.equal(workerBreakLines(w).length, 1); // inherited copy stands
  const cleared = await st.cmdBreaksClear();
  assert.equal(cleared.removed.length, 1);
  assert.equal(workerBreakLines(w).length, 0);
  assertBreakInvariants(st, [w]);
  const stops = (await st.cmdBreaks()).stops;
  assert.deepEqual(stops.filter((r) => r.kind === 'break'), []);
});

test('global clear eliminates inherited and ephemeral worker records', async () => {
  const dir = tmpdir('b-rec-');
  const st = nodeSession(dir);
  stubMainCdp(st);
  stubWorkerSend(st, 'ok');
  const file = writeJs(dir);
  await st.cmdBreaksAdd({ breaks: [`${file}:5`] });
  const w = addWorker(st, 'a');
  await st.plantInherited(w);
  await st.addWorkerEphemeral(w.id, [`${file}:8`]);
  assert.equal(workerBreakLines(w).length, 2);
  const cleared = await st.cmdBreaksClear();
  assert.equal(cleared.removed.length, 1);
  assert.equal(workerBreakLines(w).length, 0);
  assert.equal(w.inheritedKeys.size, 0);
  assert.equal(w.targetRaws.size, 0);
  assertBreakInvariants(st, [w]);
});

// Two-file backend oracle: plants succeed everywhere; removals fail for
// one main breakpoint while the worker session stays healthy (separate
// transports can fail independently).
function stubFallibleBackends(st) {
  const mainLive = new Map();
  const workerLive = new Map();
  let seq = 0;
  let wseq = 0;
  const failMain = new Set();
  st.cdp = {
    request: async (method, params) => {
      if (method === 'Debugger.setBreakpointByUrl') {
        const id = `bp-${++seq}`;
        mainLive.set(id, params.lineNumber);
        return { breakpointId: id, locations: [{ lineNumber: params.lineNumber }] };
      }
      if (method === 'Debugger.removeBreakpoint') {
        if (failMain.has(params.breakpointId)) throw new Error('backend gone');
        mainLive.delete(params.breakpointId);
        return {};
      }
      return {};
    },
  };
  st.workerSend = async (w, method, params) => {
    if (method === 'Debugger.setBreakpointByUrl') {
      const id = `wbp-${++wseq}`;
      workerLive.set(id, params.lineNumber);
      return { breakpointId: id, locations: [{ lineNumber: params.lineNumber }] };
    }
    if (method === 'Debugger.removeBreakpoint') {
      workerLive.delete(params.breakpointId);
      return {};
    }
    return {};
  };
  return { mainLive, workerLive, failMain };
}

function assertBackendCoherent(st, w, mainLive, workerLive) {
  assert.deepEqual(new Set([...mainLive.keys()]),
    new Set([...st.breakKeys.values()].filter(Boolean)));
  assert.deepEqual(new Set([...workerLive.keys()]),
    new Set([...w.breakKeys.values()].filter(Boolean)));
}

test('remove partial failure keeps failed intent and worker copy', async () => {
  const dir = tmpdir('b-rec-');
  const st = nodeSession(dir);
  const { mainLive, workerLive, failMain } = stubFallibleBackends(st);
  const a = writeJs(dir, 'a.js');
  const b = writeJs(dir, 'b.js');
  await st.cmdBreaksAdd({ breaks: [`${a}:5`, `${b}:6`] });
  const w = addWorker(st, 'a');
  await st.plantInherited(w);
  assertBreakInvariants(st, [w]);
  const keyB = st.breakKeyOf(st.cfg.breaks.find((x) => x.line === 6));
  failMain.add(st.breakKeys.get(keyB));
  const rm = await st.cmdBreaksRemove({ breaks: [`${a}:5`, `${b}:6`] });
  assert.equal(rm.ok, true);
  assert.deepEqual(rm.removed.map((e) => e.raw), [`${a}:5`]);
  assert.deepEqual(rm.failed.map((f) => f.raw), [`${b}:6`]);
  assert.match(rm.warning, /b\.js/);
  // Failed intent stands on main and worker; confirmed copy dropped.
  assertBreakInvariants(st, [w]);
  assert.deepEqual(st.cfg.breaks.map((x) => x.line), [6]);
  assert.ok(w.inheritedKeys.has(keyB), 'failed copy retained');
  assert.equal(workerBreakLines(w).length, 1);
  assertBackendCoherent(st, w, mainLive, workerLive);
  // Displayed records: b on both targets, a gone everywhere.
  const stops = (await st.cmdBreaks()).stops.filter((r) => r.kind === 'break');
  assert.equal(stops.length, 2);
  assert.ok(stops.every((r) => r.spec.includes(':6')));
});

test('clear partial failure resets ephemeral but keeps failed copies', async () => {
  const dir = tmpdir('b-rec-');
  const st = nodeSession(dir);
  const { mainLive, workerLive, failMain } = stubFallibleBackends(st);
  const a = writeJs(dir, 'a.js');
  const b = writeJs(dir, 'b.js');
  await st.cmdBreaksAdd({ breaks: [`${a}:5`, `${b}:6`] });
  const w = addWorker(st, 'a');
  await st.plantInherited(w);
  await st.addWorkerEphemeral(w.id, [`${a}:8`]);
  assert.equal(workerBreakLines(w).length, 3);
  const keyB = st.breakKeyOf(st.cfg.breaks.find((x) => x.line === 6));
  failMain.add(st.breakKeys.get(keyB));
  const cleared = await st.cmdBreaksClear();
  assert.equal(cleared.ok, true);
  assert.deepEqual(cleared.removed.map((e) => e.raw), [`${a}:5`]);
  assert.match(cleared.warning, /b\.js/);
  // Ephemeral always resets; inherited-b retained with the failed intent.
  assertBreakInvariants(st, [w]);
  assert.equal(w.targetRaws.size, 0);
  assert.deepEqual([...w.inheritedKeys], [keyB]);
  assertBackendCoherent(st, w, mainLive, workerLive);
  const stops = (await st.cmdBreaks()).stops.filter((r) => r.kind === 'break');
  assert.equal(stops.length, 2);
  assert.ok(stops.every((r) => r.spec.includes(':6')));
});

test('inherit V8 reject creates one keyed reachable record', async () => {
  // Worker plant refuses while main succeeds: the rejected copy is
  // admitted (reachable by global remove/clear) exactly once.
  const dir = tmpdir('b-rec-');
  const st = nodeSession(dir);
  stubMainCdp(st);
  stubWorkerSend(st, 'reject');
  const file = writeJs(dir);
  const raw = `${file}:5`;
  const w = addWorker(st, 'a');
  const added = await st.cmdBreaksAdd({ breaks: [raw] });
  assert.equal(added.added.length, 1);
  assert.equal(workerBreakLines(w).length, 1);
  assert.equal(workerBreakLines(w)[0].state, 'rejected');
  assertBreakInvariants(st, [w]);
  // Re-inheriting the same intent never duplicates the record.
  const fresh = st.cfg.breaks.map((b) => ({
    raw, path: b.path, line: b.line, cond: b.cond || null,
  }));
  await st.inheritGlobalAdd(fresh);
  assert.equal(workerBreakLines(w).length, 1);
  assertBreakInvariants(st, [w]);
  // Global remove eliminates the rejected copy with the intent.
  const rm = await st.cmdBreaksRemove({ breaks: [raw] });
  assert.equal(rm.removed.length, 1);
  assert.equal(workerBreakLines(w).length, 0);
  assertBreakInvariants(st, [w]);
});
