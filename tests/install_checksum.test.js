const { describe, it } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('fs');
const os = require('os');
const path = require('path');
const crypto = require('crypto');
const { execFileSync } = require('node:child_process');

// Network surface is limited to the sidecar fetch: match/mismatch run
// against a local HTTP server, the missing-sidecar case against a dead
// port (must warn-and-continue, never throw).
const postinstall = require('../scripts/postinstall.js');

describe('postinstall checksum', () => {
  it('sha256File matches crypto hash', () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'adb-sum-'));
    try {
      const file = path.join(dir, 'pkg.tar.gz');
      fs.writeFileSync(file, 'hello-installer');
      const expected = crypto.createHash('sha256').update('hello-installer').digest('hex');
      assert.equal(postinstall.sha256File(file), expected);
      // Tamper changes the digest.
      fs.appendFileSync(file, 'tampered');
      assert.notEqual(postinstall.sha256File(file), expected);
    } finally {
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });

  it('TARGET_MAP covers all release triples incl. linux arm', () => {
    const triples = new Set();
    for (const arch of Object.values(postinstall.TARGET_MAP)) {
      for (const t of Object.values(arch)) triples.add(t);
    }
    for (const want of [
      'x86_64-apple-darwin',
      'aarch64-apple-darwin',
      'x86_64-unknown-linux-gnu',
      'aarch64-unknown-linux-gnu',
      'x86_64-pc-windows-msvc',
    ]) {
      assert.ok(triples.has(want), `TARGET_MAP must include ${want}`);
    }
  });

  it('verifyChecksum throws on mismatch, passes on match', async () => {
    const http = require('http');
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'adb-sum-'));
    try {
      const file = path.join(dir, 'pkg.tar.gz');
      fs.writeFileSync(file, 'hello-installer');
      const good = postinstall.sha256File(file);
      const bad = '0'.repeat(64);
      const server = http.createServer((req, res) => {
        res.writeHead(200, { 'Content-Type': 'text/plain' });
        res.end(req.url.includes('good') ? `${good}  pkg.tar.gz\n` : `${bad}  pkg.tar.gz\n`);
      });
      await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
      const port = server.address().port;
      try {
        await postinstall.verifyChecksum(file, `http://127.0.0.1:${port}/good.sha256`);
        await assert.rejects(
          postinstall.verifyChecksum(file, `http://127.0.0.1:${port}/bad.sha256`),
          /Checksum mismatch/,
        );
      } finally {
        server.close();
      }
    } finally {
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });

  it('verifyChecksum skips when sidecar missing (old-release compat)', async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'adb-sum-'));
    try {
      const file = path.join(dir, 'pkg.tar.gz');
      fs.writeFileSync(file, 'x');
      await postinstall.verifyChecksum(file, 'https://127.0.0.1:1/nope.sha256');
    } finally {
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });

  it('requiring postinstall has no side effects (no auto-download)', () => {
    // Regression: main() must only run as the npm script
    // (require.main === module). With SKIP=1 an auto-run would print the
    // skipping line; a guarded require prints nothing.
    const out = execFileSync(
      process.execPath,
      ['-e', `require(${JSON.stringify(require.resolve('../scripts/postinstall.js'))})`],
      {
        encoding: 'utf8',
        env: { ...process.env, AGENT_DEBUGGER_SKIP_DOWNLOAD: '1' },
        timeout: 15000,
      },
    );
    assert.equal(out, '');
  });
});

describe('postinstall error classification', () => {
  it('checksum mismatch is fatal, fetch failures are transient', () => {
    assert.equal(postinstall.isChecksumMismatch(new Error('Checksum mismatch for x (download may be corrupt)')), true);
    assert.equal(postinstall.isChecksumMismatch(new Error('HTTP 404: Not Found')), false);
    assert.equal(postinstall.isChecksumMismatch(new Error('getaddrinfo ENOTFOUND github.com')), false);
    assert.equal(postinstall.isChecksumMismatch(null), false);
  });
});
