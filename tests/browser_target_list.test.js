// listTargets body bound: a rogue /json/list must not OOM the bridge by
// trickling an unbounded body (same 1MB discipline as framing).
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

const { listTargets } = loadBridge(
  'bridge/browser/src/browserbridge.js', 'listTargets');

function serve(handler) {
  const server = http.createServer(handler);
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => resolve(server));
  });
}

test('browser listTargets serves a small list', async () => {
  const server = await serve((req, res) => {
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify([{ id: 't1', type: 'page', webSocketDebuggerUrl: 'ws://x' }]));
  });
  try {
    const targets = await listTargets('127.0.0.1', server.address().port);
    assert.equal(targets.length, 1);
    assert.equal(targets[0].id, 't1');
  } finally {
    server.close();
  }
});

test('browser listTargets rejects an unbounded body', async () => {
  // Trickles 64KB/10ms forever: past the 1MB cap the bridge must reject
  // (bounded time, bounded memory) instead of accumulating forever.
  const server = await serve((req, res) => {
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
  });
  try {
    const t0 = Date.now();
    await assert.rejects(
      listTargets('127.0.0.1', server.address().port),
      /too large/,
    );
    assert.ok(Date.now() - t0 < 15000, 'rejection stays bounded');
  } finally {
    server.close();
  }
});
