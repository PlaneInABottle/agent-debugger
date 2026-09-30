const fs = require('fs');
const path = require('path');
const https = require('https');
const http = require('http');
const crypto = require('crypto');
const { execSync } = require('child_process');

const pkg = require('../package.json');

const REPO = 'PlaneInABottle/agent-debugger';
const VERSION = `v${pkg.version}`;

const TARGET_MAP = {
  darwin: {
    arm64: 'aarch64-apple-darwin',
    x64: 'x86_64-apple-darwin',
  },
  linux: {
    x64: 'x86_64-unknown-linux-gnu',
    arm64: 'aarch64-unknown-linux-gnu',
  },
};

const binName = 'agent-debugger';
const binDir = path.join(__dirname, '..', 'bin');
const destBin = path.join(binDir, binName);

async function main() {
  if (process.platform === 'win32') {
    throw new Error(
      'agent-debugger does not support Windows (macOS/Linux only). ' +
      'The npm package is restricted to darwin/linux via the "os" field; ' +
      'if you see this, install from source on a supported platform.'
    );
  }
  if (process.env.AGENT_DEBUGGER_SKIP_DOWNLOAD === '1') {
    console.log('[agent-debugger] Skipping native binary download (AGENT_DEBUGGER_SKIP_DOWNLOAD=1).');
    return;
  }

  // If binary already exists, nothing to do
  if (fs.existsSync(destBin)) {
    return;
  }

  // In a local development git repository, copy from target/release or ~/.cargo/bin if available
  const localTarget = path.join(__dirname, '..', 'target', 'release', binName);
  if (fs.existsSync(localTarget)) {
    console.log('[agent-debugger] Using locally built target/release binary.');
    fs.copyFileSync(localTarget, destBin);
    fs.chmodSync(destBin, 0o755);
    return;
  }

  const homeDir = process.env.HOME || process.env.USERPROFILE || '';
  if (homeDir) {
    const cargoBin = path.join(homeDir, '.cargo', 'bin', binName);
    if (fs.existsSync(cargoBin)) {
      console.log('[agent-debugger] Found ~/.cargo/bin binary, copying to package bin directory.');
      fs.copyFileSync(cargoBin, destBin);
      fs.chmodSync(destBin, 0o755);
      return;
    }
  }

  const platformTargets = TARGET_MAP[process.platform];
  const targetTriple = platformTargets ? platformTargets[process.arch] : null;

  if (!targetTriple) {
    console.warn(`[agent-debugger] Warning: Pre-built binary not available for ${process.platform}-${process.arch}.`);
    console.warn('[agent-debugger] You can build from source using: cargo install --path .');
    return;
  }

  const archiveExt = 'tar.gz';
  const assetName = `agent-debugger-${targetTriple}.${archiveExt}`;
  const downloadUrl = `https://github.com/${REPO}/releases/download/${VERSION}/${assetName}`;
  const checksumUrl = `${downloadUrl}.sha256`;

  console.log(`[agent-debugger] Downloading pre-built binary for ${targetTriple} (${VERSION})...`);

  const tmpArchive = path.join(binDir, assetName);

  try {
    if (!fs.existsSync(binDir)) {
      fs.mkdirSync(binDir, { recursive: true });
    }

    await downloadFile(downloadUrl, tmpArchive);
    await verifyChecksum(tmpArchive, checksumUrl);

    console.log(`[agent-debugger] Extracting binary to ${binDir}...`);
    execSync(`tar -xzf "${tmpArchive}" -C "${binDir}"`, { stdio: 'ignore' });

    if (fs.existsSync(destBin)) {
      fs.chmodSync(destBin, 0o755);
      console.log(`[agent-debugger] Successfully installed native binary (${destBin}).`);
    } else {
      console.warn('[agent-debugger] Warning: Extraction succeeded but binary file not found at expected location.');
    }
  } catch (err) {
    if (isChecksumMismatch(err)) {
      // Tamper/corruption signal: never downgrade to a warning (a warn
      // leaves npm install "successful" with no binary, hiding the
      // attack). Rethrown to the fatal handler below.
      throw err;
    }
    console.warn(`[agent-debugger] Notice: Could not download pre-built binary from ${downloadUrl}`);
    console.warn(`[agent-debugger] Reason: ${err.message}`);
    console.warn('[agent-debugger] If this release has not yet been published to GitHub, or you are offline,');
    console.warn('[agent-debugger] you can compile directly with: cargo install agent-debugger');
  } finally {
    if (fs.existsSync(tmpArchive)) {
      try {
        fs.unlinkSync(tmpArchive);
      } catch {
        // Ignore cleanup error
      }
    }
  }
}

function downloadFile(url, dest, maxRedirects = 5, opts = {}) {
  const timeoutMs = opts.timeoutMs || 60000;
  const maxBytes = opts.maxBytes || 64 * 1024 * 1024;
  return new Promise((resolve, reject) => {
    if (maxRedirects <= 0) {
      return reject(new Error('Too many redirects'));
    }

    let settled = false;
    const fail = (err) => {
      if (settled) return;
      settled = true;
      try { req.destroy(); } catch (_) { /* already gone */ }
      fs.unlink(dest, () => reject(err));
    };
    const done = () => {
      if (settled) return;
      settled = true;
      resolve();
    };

    const mod = url.startsWith('http://') ? http : https;
    const req = mod.get(url, { headers: { 'User-Agent': 'agent-debugger-npm-installer' } }, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        res.resume();
        try { req.destroy(); } catch (_) { /* redirecting */ }
        if (settled) return;
        settled = true;
        resolve(downloadFile(res.headers.location, dest, maxRedirects - 1, opts));
        return;
      }

      if (res.statusCode !== 200) {
        res.resume();
        fail(new Error(`HTTP ${res.statusCode}: ${res.statusMessage}`));
        return;
      }

      const declared = Number(res.headers['content-length']);
      if (Number.isFinite(declared) && declared > maxBytes) {
        res.resume();
        fail(new Error(`download too large (${declared} bytes declared, cap ${maxBytes})`));
        return;
      }

      const file = fs.createWriteStream(dest);
      let written = 0;
      let tooBig = false;
      res.on('data', (chunk) => {
        if (tooBig) return;
        written += chunk.length;
        if (written > maxBytes) {
          tooBig = true;
          res.resume();
          try { req.destroy(); } catch (_) { /* aborting */ }
          file.destroy();
          fail(new Error(`download too large (cap ${maxBytes} bytes)`));
          return;
        }
        if (!file.write(chunk)) res.pause();
      });
      file.on('drain', () => res.resume());
      res.on('end', () => {
        if (tooBig) return;
        file.end(() => done());
      });
      res.on('error', (err) => {
        file.destroy();
        fail(err);
      });

      file.on('error', (err) => {
        try { req.destroy(); } catch (_) { /* already gone */ }
        fail(err);
      });
    });

    req.setTimeout(timeoutMs, () => {
      fail(new Error(`download timed out after ${Math.round(timeoutMs / 1000)}s`));
    });

    req.on('error', (err) => {
      if (settled) return;
      settled = true;
      fs.unlink(dest, () => reject(err));
    });
  });
}

// Verify a downloaded archive against its published .sha256 sidecar
// (same "<hash>  <filename>" format release.yml writes via shasum).
// Checksum unfetchable (old release without sidecar): warn, continue.
// Mismatch — including a fetched-but-unparseable sidecar, which proves
// nothing about the archive — throws (caller aborts install, archive
// deleted by finally).
async function verifyChecksum(archivePath, checksumUrl) {
  let text;
  try {
    text = await downloadText(checksumUrl);
  } catch (err) {
    if (/too large|timed out/.test(String((err && err.message) || err))) {
      throw new Error(`Checksum mismatch for ${path.basename(archivePath)} (published checksum unfetchable within bounds; refusing unverified archive)`);
    }
    console.warn('[agent-debugger] No published checksum found; skipping verification.');
    return;
  }
  const expected = text.trim().split(/\s+/)[0].toLowerCase();
  if (!/^[0-9a-f]{64}$/.test(expected)) {
    // Fail closed: a present-but-garbled sidecar verifies nothing, so it
    // must abort like a mismatch — never warn-and-continue. The
    // "Checksum mismatch" prefix routes through the fatal classification
    // (isChecksumMismatch) in both install paths.
    throw new Error(`Checksum mismatch for ${path.basename(archivePath)} (published checksum unparseable; refusing unverified archive)`);
  }
  const actual = sha256File(archivePath);
  if (actual !== expected) {
    throw new Error(`Checksum mismatch for ${path.basename(archivePath)} (download may be corrupt or tampered)`);
  }
  console.log('[agent-debugger] Checksum verified.');
}

function sha256File(filePath) {
  const hash = crypto.createHash('sha256');
  hash.update(fs.readFileSync(filePath));
  return hash.digest('hex');
}

// Checksum mismatches (corrupt/tampered download) are fatal: the install
// must fail loudly, never warn-and-continue with no binary. Fetch-
// failures (offline, unpublished release) stay warnings.
function isChecksumMismatch(err) {
  return !!err && /Checksum mismatch/.test(String((err && err.message) || err));
}

function downloadText(url, maxRedirects = 5, opts = {}) {
  const timeoutMs = opts.timeoutMs || 30000;
  const maxChars = opts.maxChars || 16 * 1024;
  return new Promise((resolve, reject) => {
    if (maxRedirects <= 0) {
      return reject(new Error('Too many redirects'));
    }
    const mod = url.startsWith('http://') ? http : https;
    const req = mod.get(url, { headers: { 'User-Agent': 'agent-debugger-npm-installer' } }, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        res.resume();
        return resolve(downloadText(res.headers.location, maxRedirects - 1, opts));
      }
      if (res.statusCode !== 200) {
        res.resume();
        return reject(new Error(`HTTP ${res.statusCode}: ${res.statusMessage}`));
      }
      let body = '';
      let tooBig = false;
      res.setEncoding('utf8');
      res.on('data', (chunk) => {
        if (tooBig) return;
        if (body.length + chunk.length > maxChars) {
          tooBig = true;
          res.resume();
          try { req.destroy(); } catch (_) { /* aborting */ }
          reject(new Error(`response too large (cap ${maxChars} chars)`));
          return;
        }
        body += chunk;
      });
      res.on('end', () => { if (!tooBig) resolve(body); });
      res.on('error', reject);
    });
    req.setTimeout(timeoutMs, () => {
      try { req.destroy(); } catch (_) { /* already gone */ }
      reject(new Error(`download timed out after ${Math.round(timeoutMs / 1000)}s`));
    });
    req.on('error', (err) => reject(err));
  });
}

if (typeof module !== 'undefined' && module.exports) {
  module.exports = { verifyChecksum, sha256File, TARGET_MAP, isChecksumMismatch, main, downloadFile, downloadText };
}

// Only auto-run when executed as the npm postinstall script, never on
// require() (unit tests require this file for verifyChecksum/sha256File;
// an unconditional main() would trigger a real network download attempt
// on fresh checkouts without bin/ or target/release).
if (typeof require !== 'undefined' && require.main === module) {
  main().catch((err) => {
    if (isChecksumMismatch(err)) {
      console.error(`[agent-debugger] ${err.message}`);
      console.error('[agent-debugger] Refusing to finish installation with a corrupt binary.');
      process.exit(1);
    }
    console.warn(`[agent-debugger] Postinstall error: ${err.message}`);
  });
}
