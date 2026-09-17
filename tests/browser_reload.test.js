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

const browser = loadBridge('bridge/browser/src/browserbridge.js', 'Session');

function tmpdir(prefix) {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

function session(dir, over = {}) {
  const st = new browser.Session({
    kind: 'attach', dir, host: 'localhost', port: 9222, tab: null, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20,
    ...over,
  });
  st.exited = false;
  st.verifyTab = async () => {};
  return st;
}

function readStop(dir) {
  return JSON.parse(fs.readFileSync(path.join(dir, 'session.json'), 'utf-8')).stopped;
}

test('reload failure restores the park instead of dropping it', async () => {
  const dir = tmpdir('reload-restore-');
  const st = session(dir);
  const park = { frames: [] };
  st.paused = park;
  st.cachedLocals = [{ name: 'x' }];
  st.cdp = { request: async () => { throw new Error('tab gone'); } };
  await assert.rejects(st.cmdReload({}, 5), /tab gone/);
  assert.equal(st.paused, park);
  assert.equal(readStop(dir), true);
});

test('reload with no breaks still publishes running', async () => {
  const dir = tmpdir('reload-publish-');
  const st = session(dir);
  st.paused = { frames: [] };
  st.cdp = { request: async () => ({}) };
  const resp = await st.cmdReload({}, 5);
  assert.equal(resp.reloaded, true);
  assert.equal(st.paused, null);
  assert.equal(readStop(dir), false);
});
