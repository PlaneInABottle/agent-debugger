const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const Module = require('node:module');

// wait/capture/diagnostics UX batch on the Node bridge (real Session,
// stubbed CDP transport, no timers in assertions).
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

const node = loadBridge('bridge/node/src/nodebridge.js', 'Session');

function tmpdir(prefix) {
  return fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), prefix)));
}

function writeJs(dir, name = 'wc.js', lines = 12) {
  const file = path.join(dir, name);
  fs.writeFileSync(file,
    Array.from({ length: lines }, (_, i) => `const v${i} = ${i};`).join('\n') + '\n');
  return file;
}

function nodeSession(dir, over = {}) {
  const st = new node.Session({
    kind: 'attach', dir, host: 'localhost', port: 9229, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20, programArgs: [],
    ...over,
  });
  st.exited = false;
  return st;
}

function parkMain(st, file, line = 5) {
  const url = `file://${file}`;
  st.scripts = new Map([['s1', url]]);
  st.paused = {
    frames: [{
      functionName: 'handler', url,
      location: { scriptId: 's1', lineNumber: line - 1 },
      scopeChain: [],
    }],
    stopInfo: null,
  };
  st.cachedLocals = [];
  st.lastChanged = '[]';
  st.stopInfo = null;
  st.notePark('main', { reason: 'breakpoint', hitBreakpoints: [] },
    { file: st.relFile(file), line });
}

test('node captureBounds: frames/vars/budget/break enforced', () => {
  const st = nodeSession(tmpdir('wc-node-'));
  assert.deepEqual(st.captureBounds({}), { frames: 1, vars: 20, budget: 2000, spec: null });
  for (const req of [{ frames: 0 }, { frames: 11 }, { vars: 0 }, { vars: 21 },
    { pauseBudgetMs: 0 }, { pauseBudgetMs: 10001 }, { break: '' }, { break: 42 },
    { frames: 'x' }]) {
    assert.throws(() => st.captureBounds(req), /capture/);
  }
});

test('node wait: immediate parked success never resumes', async () => {
  const dir = tmpdir('wc-node-');
  const file = writeJs(dir);
  const st = nodeSession(dir);
  parkMain(st, file);
  const calls = [];
  st.cdp = { request: async (m) => { calls.push(m); return {}; } };
  const resp = await st.cmdWait({}, 5);
  assert.equal(resp.ok, true);
  assert.equal(resp.waited, false);
  assert.equal(resp.target, 'main');
  assert.ok(!calls.includes('Debugger.resume'));
  assert.match(resp.warning, /HTTP handler remains open/);
  assert.equal(resp.diag.target, 'main');
  assert.equal(resp.diag.reason, 'breakpoint');
  assert.equal(resp.diag.stoppingThread.id, 1);
  assert.ok(st.paused, 'still parked');
});

test('node wait: fresh park via pump issues no resume', async () => {
  const dir = tmpdir('wc-node-');
  const file = writeJs(dir);
  const st = nodeSession(dir);
  const calls = [];
  st.cdp = { request: async (m) => { calls.push(m); return {}; } };
  st.pump = async () => { parkMain(st, file, 7); return 'stopped'; };
  const resp = await st.cmdWait({}, 5);
  assert.equal(resp.waited, true);
  assert.ok(!calls.includes('Debugger.resume'));
  assert.equal(resp.snapshot.location.line, 7);
});

test('node wait: timeout is typed and preserves the session', async () => {
  const st = nodeSession(tmpdir('wc-node-'));
  st.pump = async () => { throw new Error('timeout: no stop within 7s'); };
  await assert.rejects(st.cmdWait({}, 7), /timeout: no stop within 7s/);
  assert.equal(st.paused, null);
});

test('node capture: prepark collects without resuming', async () => {
  const dir = tmpdir('wc-node-');
  const file = writeJs(dir);
  const st = nodeSession(dir);
  parkMain(st, file);
  const calls = [];
  st.cdp = { request: async (m) => { calls.push(m); return {}; } };
  const resp = await st.cmdCapture({ frames: 2, vars: 3 }, 5);
  assert.equal(resp.targetWasPaused, true);
  assert.equal(resp.resumed, false);
  assert.equal(resp.pauseDurationMs, 0);
  assert.ok(resp.snapshot.frames.length <= 2);
  assert.ok(!calls.includes('Debugger.resume'));
  assert.ok(!calls.includes('Debugger.setBreakpointByUrl'));
  assert.ok(st.paused, 'pre-existing park untouched');
});

test('node capture: fresh park removes ephemeral before resume', async () => {
  const dir = tmpdir('wc-node-');
  const file = writeJs(dir);
  const st = nodeSession(dir);
  const order = [];
  st.cdp = {
    request: async (m) => {
      order.push(m);
      if (m === 'Debugger.setBreakpointByUrl') {
        return { breakpointId: 'bp-1', locations: [{ lineNumber: 4 }] };
      }
      return {};
    },
  };
  st.pump = async () => { parkMain(st, file, 5); return 'stopped'; };
  const resp = await st.cmdCapture({ break: `${file}:5`, pauseBudgetMs: 2000 }, 5);
  assert.equal(resp.targetWasPaused, false);
  assert.equal(resp.resumed, true);
  assert.equal(resp.ephemeralPlanted, true);
  assert.ok(typeof resp.pauseDurationMs === 'number');
  assert.ok('budgetExceeded' in resp);
  const plant = order.indexOf('Debugger.setBreakpointByUrl');
  const unplant = order.lastIndexOf('Debugger.removeBreakpoint');
  const resume = order.indexOf('Debugger.resume');
  assert.ok(plant >= 0 && unplant > plant && resume > unplant,
    `remove-before-resume order: ${order.join(',')}`);
  assert.ok(!resp.removeError);
  assert.equal(st.paused, null, 'resumed');
  assert.equal(st.cfg.breaks.length, 0, 'no intent left behind');
});

test('node capture: timeout removes ephemeral without resume', async () => {
  const dir = tmpdir('wc-node-');
  const file = writeJs(dir);
  const st = nodeSession(dir);
  const order = [];
  st.cdp = {
    request: async (m) => {
      order.push(m);
      if (m === 'Debugger.setBreakpointByUrl') {
        return { breakpointId: 'bp-9', locations: [{ lineNumber: 4 }] };
      }
      return {};
    },
  };
  st.pump = async () => { throw new Error('timeout: no stop within 5s'); };
  await assert.rejects(st.cmdCapture({ break: `${file}:5` }, 5), /timeout/);
  assert.ok(!order.includes('Debugger.resume'));
  assert.ok(order.includes('Debugger.removeBreakpoint'), 'ephemeral removed');
  assert.equal(st.cfg.breaks.length, 0);
});

test('node capture: collection failure still resumes', async () => {
  const dir = tmpdir('wc-node-');
  const file = writeJs(dir);
  const st = nodeSession(dir);
  const order = [];
  st.cdp = { request: async (m) => { order.push(m); return {}; } };
  st.pump = async () => { parkMain(st, file, 5); return 'stopped'; };
  st.boundedSnapshot = async () => { throw new Error('boom'); };
  const resp = await st.cmdCapture({}, 5);
  assert.equal(resp.resumed, true);
  assert.ok(resp.snapshotError);
  assert.ok(order.includes('Debugger.resume'));
});

test('node dispatch: wait occupies slot, rival resume busy', async () => {
  const st = nodeSession(tmpdir('wc-node-'));
  st.outstanding.set('main', 'wait');
  await assert.rejects(st.dispatch({ cmd: 'continue', timeout: 5 }), /busy/);
});

test('node notePark: same-line second park diagnoses, slide does not', () => {
  const st = nodeSession(tmpdir('wc-node-'));
  st.notePark('main', { reason: 'breakpoint', hitBreakpoints: [] }, { file: 'a.js', line: 5 });
  const first = { ...st.lastDiag };
  st.notePark('main', { reason: 'breakpoint', hitBreakpoints: [] }, { file: 'a.js', line: 5 });
  assert.equal(st.lastDiag.stopId, first.stopId + 1);
  assert.equal(st.lastDiag.sameLocation, true);
  assert.equal(st.lastDiag.sameThread, true);
  assert.ok(st.lastDiag.elapsedMs >= 0);
  st.notePark('main', { reason: 'breakpoint', hitBreakpoints: [] }, { file: 'a.js', line: 6 });
  assert.equal(st.lastDiag.sameLocation, false);
});

// ---- browser ----

const browser = loadBridge('bridge/browser/src/browserbridge.js', 'Session');

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

function parkTab(st, url = 'http://localhost:3000/app.js', line = 5) {
  st.paused = {
    frames: [{
      functionName: 'handler', url,
      location: { scriptId: 's1', lineNumber: line - 1 },
      scopeChain: [],
    }],
    stopInfo: null,
  };
  st.cachedLocals = [];
  st.lastChanged = '[]';
  st.stopInfo = null;
  st.scriptLines = async () => [];
  st.notePark('main', { reason: 'breakpoint', hitBreakpoints: [] });
}

test('browser wait: immediate parked success never resumes', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  parkTab(st);
  const calls = [];
  st.cdp = { request: async (m) => { calls.push(m); return {}; } };
  const resp = await st.cmdWait({}, 5);
  assert.equal(resp.waited, false);
  assert.equal(resp.target, 'main');
  assert.ok(!calls.includes('Debugger.resume'));
  assert.match(resp.warning, /HTTP handler remains open/);
  assert.equal(resp.diag.stoppingThread.id, 1);
  assert.ok(st.paused);
});

test('browser wait: fresh park via pump issues no resume', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  const calls = [];
  st.cdp = { request: async (m) => { calls.push(m); return {}; } };
  st.pump = async () => { parkTab(st); return 'stopped'; };
  const resp = await st.cmdWait({}, 5);
  assert.equal(resp.waited, true);
  assert.ok(!calls.includes('Debugger.resume'));
});

test('browser capture: prepark collects without resuming', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  parkTab(st);
  const calls = [];
  st.cdp = { request: async (m) => { calls.push(m); return {}; } };
  const resp = await st.cmdCapture({ frames: 2, vars: 3 }, 5);
  assert.equal(resp.targetWasPaused, true);
  assert.equal(resp.resumed, false);
  assert.ok(!calls.includes('Debugger.resume'));
  assert.ok(st.paused);
});

test('browser capture: fresh park removes ephemeral before resume', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  const order = [];
  st.cdp = {
    request: async (m) => {
      order.push(m);
      if (m === 'Debugger.setBreakpointByUrl') {
        return { breakpointId: 'bp-1', locations: [{ lineNumber: 4 }] };
      }
      return {};
    },
  };
  st.pump = async () => { parkTab(st); return 'stopped'; };
  const resp = await st.cmdCapture({ break: 'app.js:5', pauseBudgetMs: 2000 }, 5);
  assert.equal(resp.resumed, true);
  assert.equal(resp.ephemeralPlanted, true);
  const plant = order.indexOf('Debugger.setBreakpointByUrl');
  const unplant = order.lastIndexOf('Debugger.removeBreakpoint');
  const resume = order.indexOf('Debugger.resume');
  assert.ok(plant >= 0 && unplant > plant && resume > unplant,
    `remove-before-resume order: ${order.join(',')}`);
  assert.equal(st.paused, null);
  assert.equal(st.cfg.breaks.length, 0);
});

test('browser capture: timeout removes ephemeral without resume', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  const order = [];
  st.cdp = {
    request: async (m) => {
      order.push(m);
      if (m === 'Debugger.setBreakpointByUrl') {
        return { breakpointId: 'bp-2', locations: [{ lineNumber: 4 }] };
      }
      return {};
    },
  };
  st.pump = async () => { throw new Error('timeout: no stop within 5s'); };
  await assert.rejects(st.cmdCapture({ break: 'app.js:5' }, 5), /timeout/);
  assert.ok(!order.includes('Debugger.resume'));
  assert.ok(order.includes('Debugger.removeBreakpoint'));
});

test('browser capture: reload between plant and park is a timeout, never stale resume', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  const order = [];
  st.cdp = {
    request: async (m) => {
      order.push(m);
      if (m === 'Debugger.setBreakpointByUrl') {
        return { breakpointId: 'bp-3', locations: [{ lineNumber: 4 }] };
      }
      return {};
    },
  };
  // Navigation dropped the ephemeral breakpoint: no park ever comes.
  st.pump = async () => { throw new Error('timeout: no stop within 5s'); };
  await assert.rejects(st.cmdCapture({ break: 'app.js:5' }, 5), /timeout/);
  assert.ok(!order.includes('Debugger.resume'), 'no resume without a park');
});

test('browser dispatch: capture occupies slot, rival mutation busy', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  st.outstanding.set('main', 'capture');
  await assert.rejects(st.dispatch({ cmd: 'breaksAdd', breaks: ['app.js:1'] }), /busy/);
});

test('browser captureBounds enforced', () => {
  const st = browserSession(tmpdir('wc-br-'));
  for (const req of [{ frames: 0 }, { vars: 21 }, { pauseBudgetMs: 10001 }, { break: '' }]) {
    assert.throws(() => st.captureBounds(req), /capture/);
  }
});
