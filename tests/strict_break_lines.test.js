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

const node = loadBridge('bridge/node/src/nodebridge.js', 'Session, parseBreak, parseLogpoint');
const browser = loadBridge('bridge/browser/src/browserbridge.js', 'Session, parseBreak, parseLogpoint');

function tmpdir(prefix) {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}
function writeJs(dir, name = 'strict.js', lines = 12) {
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

// Startup parseBreak must be strict: trailing junk, floats, and line<1
// reject (Python/Java parity). Previously parseInt('5x') -> 5 silently.
test('node startup parseBreak rejects trailing-junk and non-positive lines', () => {
  const dir = tmpdir('strict-node-');
  const file = writeJs(dir);
  const base = `${file}`;
  for (const bad of [`${base}:5x`, `${base}:5.9`, `${base}:0`, `${base}:-3`, `${base}:`] ) {
    const cfg = { breaks: [], logpoints: [], wantExc: false, srcs: [] };
    assert.throws(() => node.parseBreak(bad, cfg), /bad line|must look like/, bad);
  }
  // Whitespace-padded digits still accept (Number parity with Python int()).
  const cfg = { breaks: [], logpoints: [], wantExc: false, srcs: [] };
  node.parseBreak(`${base}: 5 `, cfg);
  assert.equal(cfg.breaks[0].line, 5);
});

test('browser startup parseBreak rejects trailing-junk and non-positive lines', () => {
  for (const bad of ['app.js:5x', 'app.js:5.9', 'app.js:0', 'app.js:-3', 'app.js:']) {
    const cfg = { breaks: [], logpoints: [], wantExc: false, srcs: [] };
    assert.throws(() => browser.parseBreak(bad, cfg), /bad line|must look like/, bad);
  }
  const cfg = { breaks: [], logpoints: [], wantExc: false, srcs: [] };
  browser.parseBreak('app.js: 5 ', cfg);
  assert.equal(cfg.breaks[0].line, 5);
});

test('node live breaksAdd rejects strict-line specs without CDP traffic', async () => {
  const dir = tmpdir('strict-live-');
  const file = writeJs(dir);
  const st = nodeSession(dir);
  let calls = 0;
  st.cdp = { request: async () => { calls += 1; return {}; } };
  await assert.rejects(st.cmdBreaksAdd({ breaks: [`${file}:5x`] }), /bad line|bad break/);
  await assert.rejects(st.cmdBreaksAdd({ breaks: [`${file}:0`] }), /bad line|bad break/);
  assert.equal(calls, 0);
});

test('browser live breaksAdd rejects strict-line specs without tab traffic', async () => {
  const dir = tmpdir('strict-live-b-');
  const st = browserSession(dir);
  let calls = 0;
  st.cdp = { request: async () => { calls += 1; return {}; } };
  await assert.rejects(st.cmdBreaksAdd({ breaks: ['app.js:5x'] }), /bad line|bad break/);
  await assert.rejects(st.cmdBreaksAdd({ breaks: ['app.js:0'] }), /bad line|bad break/);
  assert.equal(calls, 0);
});

test('node parseLogpoint rejects empty template (Java parity)', () => {
  const dir = tmpdir('strict-log-');
  const file = writeJs(dir);
  const cfg = { breaks: [], logpoints: [], wantExc: false, srcs: [] };
  assert.throws(() => node.parseLogpoint(`${file}:3:`, cfg), /template is empty/);
  node.parseLogpoint(`${file}:3:hit {x}`, cfg);
  assert.equal(cfg.logpoints[0].template, 'hit {x}');
});

test('browser parseLogpoint rejects empty template (Java parity)', () => {
  const cfg = { breaks: [], logpoints: [], wantExc: false, srcs: [] };
  assert.throws(() => browser.parseLogpoint('app.js:3:', cfg), /template is empty/);
  browser.parseLogpoint('app.js:3:hit {x}', cfg);
  assert.equal(cfg.logpoints[0].template, 'hit {x}');
});
