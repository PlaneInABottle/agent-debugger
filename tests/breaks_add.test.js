const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const Module = require('node:module');

// Load the real bridge sources with only the trailing main() invocation
// stripped, so Session/parseBreak run for real against a stubbed CDP.
function loadBridge(rel, exports) {
  const file = path.join(__dirname, '..', rel);
  const jsDir = path.join(__dirname, '..', 'bridge', 'js');
  // Shared wire core is provisioned next to each bridge at install time;
  // resolve the same files from the repo single-source for tests.
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
const browser = loadBridge('bridge/browser/src/browserbridge.js', 'Session, parseBreak, fragRegex');

function tmpdir(prefix) {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
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

// ---- node ----

test('node: rejects non-line specs and empty batches without CDP traffic', async () => {
  const dir = tmpdir('m2-node-');
  const st = nodeSession(dir);
  let calls = 0;
  st.cdp = { request: async () => { calls += 1; return {}; } };
  for (const breaks of [
    ['method:foo'], ['exc'], ['exc:Boom'], ['nope'], ['a.js:xx'], [''], [],
  ]) {
    await assert.rejects(st.cmdBreaksAdd({ breaks }), /break|at least one/);
  }
  await assert.rejects(st.cmdBreaksAdd({}), /at least one/);
  assert.equal(calls, 0);
  assert.deepEqual(st.cfg.breaks, []);
});

test('node: success echoes raw verbatim and bounds CDP to 5s', async () => {
  const dir = tmpdir('m2-node-');
  const st = nodeSession(dir);
  let seenTimeout = null;
  st.cdp = {
    request: async (method, params, timeoutMs) => {
      seenTimeout = timeoutMs;
      assert.equal(method, 'Debugger.setBreakpointByUrl');
      assert.match(params.urlRegex, /m1_add/);
      assert.equal(params.lineNumber, 2);
      return { breakpointId: 'b1', locations: [{ lineNumber: 2 }] };
    },
  };
  const raw = `${dir}/m1_add.js:3`;
  const resp = await st.cmdBreaksAdd({ breaks: [raw] });
  assert.equal(seenTimeout, 5000);
  assert.equal(resp.ok, true);
  assert.equal(resp.added.length, 1);
  assert.equal(resp.added[0].raw, raw);
  assert.equal(resp.added[0].state, 'verified');
  assert.equal(resp.added[0].hits, 0);
  assert.equal(st.stopStates.length, 1);
  assert.equal(st.cfg.breaks.length, 1);
});

test('node: duplicate is idempotent, diff-cond and logpoint lines conflict', async () => {
  const dir = tmpdir('m2-node-');
  const file = `${dir}/m1_add.js`;
  const st = nodeSession(dir, { logpoints: [{ path: file, line: 9, template: 't={t}' }] });
  let calls = 0;
  st.cdp = {
    request: async () => {
      calls += 1;
      return { breakpointId: `b${calls}`, locations: [{ lineNumber: 2 }] };
    },
  };
  const first = await st.cmdBreaksAdd({ breaks: [`${file}:3`] });
  assert.equal(first.added.length, 1);
  const dup = await st.cmdBreaksAdd({ breaks: [`${file}:3`, `${file}:3`] });
  assert.deepEqual(dup.added, []);
  assert.equal(calls, 1);
  assert.equal(st.stopStates.length, 1);
  await assert.rejects(st.cmdBreaksAdd({ breaks: [`${file}:3|x > 1`] }), /conflicting/);
  await assert.rejects(st.cmdBreaksAdd({ breaks: [`${file}:9`] }), /logpoint/);
  await assert.rejects(st.cmdBreaksAdd({ breaks: [`${file}:7`, `${file}:7|y`] }), /conflicting/);
  assert.equal(calls, 1);
  assert.equal(st.stopStates.length, 1);
});

test('node: slid and pending states mirror arm-time reporting', async () => {
  const dir = tmpdir('m2-node-');
  const file = `${dir}/m1_add.js`;
  const st = nodeSession(dir);
  st.cdp = {
    request: async (method, params) => {
      if (params.lineNumber === 6) return { breakpointId: 's1', locations: [{ lineNumber: 9 }] };
      return { breakpointId: 'p1', locations: [] };
    },
  };
  const resp = await st.cmdBreaksAdd({ breaks: [`${file}:7`, `${file}:4`] });
  assert.equal(resp.added[0].state, 'slid');
  assert.match(resp.added[0].detail, /slid to line 10/);
  assert.equal(resp.added[1].state, 'pending');
  assert.ok(resp.added[1].detail);
});

test('node: CDP failure is partial-ok with warning, total failure errors', async () => {
  const dir = tmpdir('m2-node-');
  const file = `${dir}/m1_add.js`;
  const st = nodeSession(dir);
  let n = 0;
  st.cdp = {
    request: async (method, params) => {
      n += 1;
      if (params.lineNumber === 2) return { breakpointId: 'ok1', locations: [{ lineNumber: 2 }] };
      throw new Error('CDP gone');
    },
  };
  const partial = await st.cmdBreaksAdd({ breaks: [`${file}:3`, `${file}:5`] });
  assert.equal(partial.ok, true);
  assert.equal(partial.added.length, 1);
  assert.match(partial.warning, /partial add/);
  assert.equal(n, 2);
  await assert.rejects(st.cmdBreaksAdd({ breaks: [`${file}:9`] }), /breaks add failed/);
});

// ---- browser ----

test('browser: success, duplicate idempotent, diff-cond conflicts', async () => {
  const dir = tmpdir('m2-browser-');
  const st = browserSession(dir);
  let calls = 0;
  st.cdp = {
    request: async (method, params, timeoutMs) => {
      calls += 1;
      assert.equal(timeoutMs, 5000);
      assert.match(params.urlRegex, /app/);
      return { breakpointId: `b${calls}`, locations: [{ lineNumber: 7 }] };
    },
  };
  const resp = await st.cmdBreaksAdd({ breaks: ['app.js:8'] });
  assert.equal(resp.ok, true);
  assert.equal(resp.added[0].raw, 'app.js:8');
  assert.equal(resp.added[0].spec, 'app.js:8');
  assert.equal(resp.added[0].state, 'verified');
  const dup = await st.cmdBreaksAdd({ breaks: ['app.js:8'] });
  assert.deepEqual(dup.added, []);
  assert.equal(calls, 1);
  await assert.rejects(st.cmdBreaksAdd({ breaks: ['app.js:8|x > 1'] }), /conflicting/);
  await assert.rejects(st.cmdBreaksAdd({ breaks: ['method:foo'] }), /line breaks only/);
  assert.equal(calls, 1);
});

test('browser: same-line startup logpoint conflicts', async () => {
  const dir = tmpdir('m2-browser-');
  const st = browserSession(dir, { logpoints: [{ frag: 'app.js', line: 15, template: 't={t}' }] });
  let calls = 0;
  st.cdp = { request: async () => { calls += 1; return { breakpointId: 'b1', locations: [] }; } };
  await assert.rejects(st.cmdBreaksAdd({ breaks: ['app.js:15'] }), /logpoint/);
  assert.equal(calls, 0);
});
