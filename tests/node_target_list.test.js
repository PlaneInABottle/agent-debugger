// discoverAttach/fetchTargetList body bound (node mirror of the browser
// listTargets cap): a rogue /json/list must not OOM the bridge by
// trickling an unbounded body. discoverAttach rejects (attach fails
// loudly); fetchTargetList resolves null (best-effort identity path).
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const http = require('node:http');
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

const { Session } = loadBridge('bridge/node/src/nodebridge.js', 'Session');

function serve(handler) {
  const server = http.createServer(handler);
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => resolve(server));
  });
}

function trickle(req, res) {
  res.writeHead(200, { 'Content-Type': 'application/json' });
  res.write('[');
  const chunk = 'x'.repeat(64 * 1024);
  const timer = setInterval(() => {
    try {
      res.write(chunk);
    } catch (_) { /* client gone */ }
  }, 10);
  req.on('close', () => {
    clearInterval(timer);
    try { res.destroy(); } catch (_) { /* already gone */ }
  });
}

const stub = (port) => Object.assign(Object.create(Session.prototype), {
  cfg: { host: '127.0.0.1', port },
});

test('node discoverAttach serves a small list', async () => {
  const server = await serve((req, res) => {
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify([
      { id: 't1', type: 'node', webSocketDebuggerUrl: 'ws://127.0.0.1:1/abc' },
    ]));
  });
  try {
    const url = await Session.prototype.discoverAttach.call(stub(server.address().port));
    assert.equal(url, 'ws://127.0.0.1:1/abc');
  } finally {
    server.close();
  }
});

test('node discoverAttach rejects an unbounded body', async () => {
  const server = await serve(trickle);
  try {
    const t0 = Date.now();
    await assert.rejects(
      Session.prototype.discoverAttach.call(stub(server.address().port)),
      /too large/,
    );
    assert.ok(Date.now() - t0 < 15000, 'rejection stays bounded');
  } finally {
    server.close();
  }
});

test('node fetchTargetList degrades to null on an unbounded body', async () => {
  const server = await serve(trickle);
  try {
    const t0 = Date.now();
    const out = await Session.prototype.fetchTargetList.call(stub(0), '127.0.0.1', server.address().port);
    assert.equal(out, null);
    assert.ok(Date.now() - t0 < 15000, 'degrade stays bounded');
  } finally {
    server.close();
  }
});
