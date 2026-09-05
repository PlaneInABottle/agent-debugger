const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const Module = require('node:module');

// M4: cross-adapter failure/diagnostic/state IO hardening —
// log ring (latest 2000, dropped accounting), atomic state writes,
// sanitized unexpected-crash payloads. Mirrors tests/m3_fixes.test.js
// loading (real bridge sources, main() tail stripped).
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
  'Session, writeFile, sanitizeUnexpected');
const browser = loadBridge('bridge/browser/src/browserbridge.js',
  'Session, writeFile, sanitizeUnexpected');

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

function logLines(dir) {
  const raw = fs.readFileSync(path.join(dir, 'logs.jsonl'), 'utf-8');
  const lines = raw.split('\n');
  if (lines.length > 0 && lines[lines.length - 1] === '') lines.pop();
  return lines;
}

// ---- 1. log ring keeps the latest 2000 -------------------------------------

test('node appendLog: ring keeps latest 2000, first dropped, latest retained', () => {
  const dir = tmpdir('m4-node-');
  const st = nodeSession(dir);
  for (let n = 0; n < 2005; n++) st.appendLog(`line-${n}`);
  const lines = logLines(dir);
  assert.equal(lines.length, 2000);
  assert.ok(!lines.includes('line-0'), 'oldest evicted');
  assert.ok(!lines.includes('line-4'), 'pre-ring evicted');
  assert.equal(lines[0], 'line-5');
  assert.equal(lines[lines.length - 1], 'line-2004');
  assert.equal(st.logCount, 2000);
  assert.equal(st.logDropped, 5);
  const resp = st.cmdLogs({ tail: 50 });
  assert.equal(resp.total, 2000);
  assert.equal(resp.dropped, 5);
  assert.equal(resp.truncated, true);
  assert.equal(resp.lines[resp.lines.length - 1], 'line-2004');
});

test('browser appendLog: ring keeps latest 2000 with dropped accounting', () => {
  const dir = tmpdir('m4-browser-');
  const st = browserSession(dir);
  for (let n = 0; n < 2005; n++) st.appendLog(`b-${n}`);
  const lines = logLines(dir);
  assert.equal(lines.length, 2000);
  assert.equal(lines[0], 'b-5');
  assert.equal(lines[lines.length - 1], 'b-2004');
  assert.equal(st.logDropped, 5);
  const resp = st.cmdLogs({ tail: 50 });
  assert.equal(resp.total, 2000);
  assert.equal(resp.dropped, 5);
  assert.equal(resp.truncated, true);
});

test('node _appendLogParts: oversize burst keeps exactly the latest MAX', () => {
  const dir = tmpdir('m4-node-');
  const st = nodeSession(dir);
  st._appendLogParts(Array.from({ length: 2100 }, (_, n) => `bulk-${n}`));
  const lines = logLines(dir);
  assert.equal(lines.length, 2000);
  assert.equal(lines[0], 'bulk-100');
  assert.equal(st.logDropped, 100);
  assert.equal(st.logCount, 2000);
});

test('node cmdLogs: fresh session has dropped 0 and honest truncated', () => {
  const dir = tmpdir('m4-node-');
  const st = nodeSession(dir);
  st.appendLog('hello');
  const resp = st.cmdLogs({ tail: 50 });
  assert.equal(resp.total, 1);
  assert.equal(resp.dropped, 0);
  assert.equal(resp.truncated, false);
  assert.deepEqual(resp.lines, ['hello']);
});

test('node appendLog: multiline values stay one physical line', () => {
  const dir = tmpdir('m4-node-');
  const st = nodeSession(dir);
  st.appendLog('a\nb\nc');
  assert.deepEqual(logLines(dir), ['a⏎b⏎c']);
  assert.equal(st.logCount, 1);
});

// ---- 2. atomic state writes -------------------------------------------------

test('node writeFile: concurrent write/read loop never observes a partial', () => {
  const dir = tmpdir('m4-node-');
  const target = path.join(dir, 'session.json');
  for (let i = 0; i < 300; i++) {
    node.writeFile(target, JSON.stringify({ n: i, pad: 'x'.repeat(500) }));
    JSON.parse(fs.readFileSync(target, 'utf-8')); // throws on torn content
  }
  JSON.parse(fs.readFileSync(target, 'utf-8'));
  assert.deepEqual(fs.readdirSync(dir).filter((f) => f.startsWith('.tmp-')), []);
});

test('browser writeFile: concurrent write/read loop never observes a partial', () => {
  const dir = tmpdir('m4-browser-');
  const target = path.join(dir, 'session.json');
  for (let i = 0; i < 300; i++) {
    browser.writeFile(target, JSON.stringify({ n: i }));
    JSON.parse(fs.readFileSync(target, 'utf-8')); // throws on torn content
  }
  assert.deepEqual(fs.readdirSync(dir).filter((f) => f.startsWith('.tmp-')), []);
});

test('node publishState: session.json always parses after repeated publish', () => {
  const dir = tmpdir('m4-node-');
  const st = nodeSession(dir);
  for (let i = 0; i < 100; i++) {
    st.publishState(i % 2 === 0);
    const parsed = JSON.parse(fs.readFileSync(path.join(dir, 'session.json'), 'utf-8'));
    assert.equal(parsed.stopped, i % 2 === 0);
  }
});

// ---- 3. sanitized unexpected-crash payloads ---------------------------------

test('node sanitizeUnexpected: capped, prefixed, no env', () => {
  const err = new TypeError('x'.repeat(5000));
  err.stack = `TypeError: ${'x'.repeat(5000)}\n` +
    Array.from({ length: 20 }, (_, i) => `    at frame${i} (/app/bridge.js:${i})`).join('\n');
  const msg = node.sanitizeUnexpected(err);
  assert.ok(msg.startsWith('internal: TypeError:'), 'class + internal prefix');
  assert.ok(msg.length <= 2048 + 'internal: '.length);
  assert.ok(!msg.includes('HOME=') && !msg.includes('PATH='), 'no env dump');
});

test('browser sanitizeUnexpected: capped, prefixed, no env', () => {
  const msg = browser.sanitizeUnexpected(new Error('boom '.repeat(1000)));
  assert.ok(msg.startsWith('internal: Error:'));
  assert.ok(msg.length <= 2048 + 'internal: '.length);
});
