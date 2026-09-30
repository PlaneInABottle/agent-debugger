// postinstall download bounds: a rogue server must not OOM/hang npm
// install by trickling an unbounded archive or checksum sidecar.
// downloadFile rejects past 64MB, downloadText rejects past 16KB, both
// with timeouts; an oversized sidecar fails verification closed.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const http = require('node:http');
const path = require('node:path');

const postinstall = require('../scripts/postinstall.js');

function serve(handler) {
  const server = http.createServer(handler);
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => resolve(server));
  });
}

function trickle(req, res, chunk = 'x'.repeat(64 * 1024)) {
  res.writeHead(200, { 'Content-Type': 'application/octet-stream' });
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

function tmpfile() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'adb-dl-'));
  return { dir, file: path.join(dir, 'pkg.tar.gz') };
}

test('downloadFile serves a small body', async () => {
  const server = await serve((req, res) => {
    res.writeHead(200, { 'Content-Type': 'application/octet-stream' });
    res.end('hello-installer');
  });
  const { dir, file } = tmpfile();
  try {
    await postinstall.downloadFile(`http://127.0.0.1:${server.address().port}/pkg`, file);
    assert.equal(fs.readFileSync(file, 'utf8'), 'hello-installer');
  } finally {
    server.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('downloadFile rejects an unbounded body', async () => {
  const server = await serve(trickle);
  const { dir, file } = tmpfile();
  try {
    const t0 = Date.now();
    await assert.rejects(
      postinstall.downloadFile(
        `http://127.0.0.1:${server.address().port}/pkg`, file,
        5, { maxBytes: 256 * 1024 },
      ),
      /too large/,
    );
    assert.ok(Date.now() - t0 < 15000, 'rejection stays bounded');
    assert.ok(!fs.existsSync(file) || fs.statSync(file).size <= 256 * 1024 + 65536);
  } finally {
    server.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('downloadText rejects an unbounded sidecar', async () => {
  const server = await serve(trickle);
  try {
    const t0 = Date.now();
    await assert.rejects(
      postinstall.downloadText(
        `http://127.0.0.1:${server.address().port}/pkg.sha256`,
        5, { maxChars: 4096 },
      ),
      /too large/,
    );
    assert.ok(Date.now() - t0 < 15000, 'rejection stays bounded');
  } finally {
    server.close();
  }
});

test('verifyChecksum fails closed on an oversized sidecar', async () => {
  const server = await serve(trickle);
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'adb-sum-'));
  try {
    const file = path.join(dir, 'pkg.tar.gz');
    fs.writeFileSync(file, 'hello-installer');
    await assert.rejects(
      postinstall.verifyChecksum(file, `http://127.0.0.1:${server.address().port}/pkg.sha256`),
      /Checksum mismatch/,
    );
  } finally {
    server.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
