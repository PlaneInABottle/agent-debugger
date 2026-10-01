// pauseDurationMs / elapsedMs are monotonic durations, not wall clock.
//
// Pre-fix, notePark and the capture-exit path subtracted Date.now()
// readings. A backward wall-clock step (NTP correction) between the park
// and the measurement then reported a negative/hours-long pause and
// could flip budgetExceeded. Reported timestamps (parkedAtMs, updatedAt)
// stay wall clock; only durations moved to monoNow().
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

const node = loadBridge('bridge/node/src/nodebridge.js', 'Session');
const browser = loadBridge('bridge/browser/src/browserbridge.js', 'Session');

function tmpdir(prefix) {
  return fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), prefix)));
}

function writeJs(dir, name = 'pm.js', lines = 12) {
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

function browserSession(dir) {
  const st = new browser.Session({
    kind: 'attach', dir, host: 'localhost', port: 9222, tab: null, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20,
  });
  st.exited = false;
  st.verifyTab = async () => {};
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

// Backward wall-clock step installed at an exact point (park already
// recorded with the real clock): a wall-clock duration then reads
// ~-1h; a monotonic one stays near zero.
function stepBack(jumpMs = 3600 * 1000) {
  const real = Date.now;
  Date.now = () => real() - jumpMs;
  return () => { Date.now = real; };
}

test('node notePark elapsedMs immune to backward wall step', async () => {
  const dir = tmpdir('pm-node-');
  const file = writeJs(dir);
  const st = nodeSession(dir);
  parkMain(st, file, 5);
  await new Promise((r) => setTimeout(r, 30));
  const restore = stepBack();
  try {
    parkMain(st, file, 5);
  } finally {
    restore();
  }
  const e = st.lastDiag.elapsedMs;
  assert.ok(e !== null && e >= 0 && e < 60000,
    `elapsedMs must be a real duration, not a wall artifact: ${e}`);
});

test('node capture-exit pauseDurationMs immune to backward wall step', async () => {
  const dir = tmpdir('pm-node-');
  const file = writeJs(dir);
  const st = nodeSession(dir);
  let restore = null;
  st.cdp = {
    request: async (m) => {
      if (m === 'Debugger.setBreakpointByUrl') {
        return { breakpointId: 'bp-1', locations: [{ lineNumber: 4 }] };
      }
      return {};
    },
  };
  // Park with the real clock, then step the wall back for the rest of
  // the capture: pauseMs spans the step.
  st.pump = async () => { parkMain(st, file, 5); restore = stepBack(); return 'stopped'; };
  try {
    const resp = await st.cmdCapture({ break: `${file}:5`, pauseBudgetMs: 2000 }, 5);
    assert.ok(resp.pauseDurationMs >= 0 && resp.pauseDurationMs < 60000,
      `pauseDurationMs must be a real duration, not a wall artifact: ${resp.pauseDurationMs}`);
    assert.equal(resp.budgetExceeded, false);
  } finally {
    if (restore) restore();
  }
});

test('browser notePark elapsedMs immune to backward wall step', async () => {
  const st = browserSession(tmpdir('pm-br-'));
  parkTab(st);
  await new Promise((r) => setTimeout(r, 30));
  const restore = stepBack();
  try {
    parkTab(st);
  } finally {
    restore();
  }
  const e = st.lastDiag.elapsedMs;
  assert.ok(e !== null && e >= 0 && e < 60000,
    `elapsedMs must be a real duration, not a wall artifact: ${e}`);
});

test('browser capture-exit pauseDurationMs immune to backward wall step', async () => {
  const st = browserSession(tmpdir('pm-br-'));
  let restore = null;
  st.cdp = {
    request: async (m) => {
      if (m === 'Debugger.setBreakpointByUrl') {
        return { breakpointId: 'bp-1', locations: [{ lineNumber: 4 }] };
      }
      return {};
    },
  };
  st.pump = async () => { parkTab(st); restore = stepBack(); return 'stopped'; };
  try {
    const resp = await st.cmdCapture({ break: 'app.js:5', pauseBudgetMs: 2000 }, 5);
    assert.ok(resp.pauseDurationMs >= 0 && resp.pauseDurationMs < 60000,
      `pauseDurationMs must be a real duration, not a wall artifact: ${resp.pauseDurationMs}`);
    assert.equal(resp.budgetExceeded, false);
  } finally {
    if (restore) restore();
  }
});
