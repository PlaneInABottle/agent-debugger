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
  win32: {
    x64: 'x86_64-pc-windows-msvc',
  },
};

const isWin = process.platform === 'win32';
const binName = isWin ? 'agent-debugger.exe' : 'agent-debugger';
const binDir = path.join(__dirname, '..', 'bin');
const destBin = path.join(binDir, binName);

async function main() {
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
    if (!isWin) fs.chmodSync(destBin, 0o755);
    return;
  }

  const homeDir = process.env.HOME || process.env.USERPROFILE || '';
  if (homeDir) {
    const cargoBin = path.join(homeDir, '.cargo', 'bin', binName);
    if (fs.existsSync(cargoBin)) {
      console.log('[agent-debugger] Found ~/.cargo/bin binary, copying to package bin directory.');
      fs.copyFileSync(cargoBin, destBin);
      if (!isWin) fs.chmodSync(destBin, 0o755);
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

  const archiveExt = isWin ? 'zip' : 'tar.gz';
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
    if (isWin) {
      execSync(`tar -xf "${tmpArchive}" -C "${binDir}"`, { stdio: 'ignore' });
    } else {
      execSync(`tar -xzf "${tmpArchive}" -C "${binDir}"`, { stdio: 'ignore' });
    }

    if (fs.existsSync(destBin)) {
      if (!isWin) fs.chmodSync(destBin, 0o755);
      console.log(`[agent-debugger] Successfully installed native binary (${destBin}).`);
    } else {
      console.warn('[agent-debugger] Warning: Extraction succeeded but binary file not found at expected location.');
    }
  } catch (err) {
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

function downloadFile(url, dest, maxRedirects = 5) {
  return new Promise((resolve, reject) => {
    if (maxRedirects <= 0) {
      return reject(new Error('Too many redirects'));
    }

    const mod = url.startsWith('http://') ? http : https;
    const req = mod.get(url, { headers: { 'User-Agent': 'agent-debugger-npm-installer' } }, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        return resolve(downloadFile(res.headers.location, dest, maxRedirects - 1));
      }

      if (res.statusCode !== 200) {
        return reject(new Error(`HTTP ${res.statusCode}: ${res.statusMessage}`));
      }

      const file = fs.createWriteStream(dest);
      res.pipe(file);

      file.on('finish', () => {
        file.close(resolve);
      });

      file.on('error', (err) => {
        fs.unlink(dest, () => reject(err));
      });
    });

    req.on('error', (err) => {
      reject(err);
    });
  });
}

// Verify a downloaded archive against its published .sha256 sidecar
// (same "<hash>  <filename>" format release.yml writes via shasum).
// Checksum unfetchable (old release without sidecar): warn, continue.
// Mismatch: throw (caller aborts install, archive deleted by finally).
async function verifyChecksum(archivePath, checksumUrl) {
  let text;
  try {
    text = await downloadText(checksumUrl);
  } catch (err) {
    console.warn('[agent-debugger] No published checksum found; skipping verification.');
    return;
  }
  const expected = text.trim().split(/\s+/)[0].toLowerCase();
  if (!/^[0-9a-f]{64}$/.test(expected)) {
    console.warn('[agent-debugger] Published checksum unparseable; skipping verification.');
    return;
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

function downloadText(url, maxRedirects = 5) {
  return new Promise((resolve, reject) => {
    if (maxRedirects <= 0) {
      return reject(new Error('Too many redirects'));
    }
    const mod = url.startsWith('http://') ? http : https;
    const req = mod.get(url, { headers: { 'User-Agent': 'agent-debugger-npm-installer' } }, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        return resolve(downloadText(res.headers.location, maxRedirects - 1));
      }
      if (res.statusCode !== 200) {
        return reject(new Error(`HTTP ${res.statusCode}: ${res.statusMessage}`));
      }
      let body = '';
      res.setEncoding('utf8');
      res.on('data', (chunk) => { body += chunk; });
      res.on('end', () => resolve(body));
    });
    req.on('error', (err) => reject(err));
  });
}

if (typeof module !== 'undefined' && module.exports) {
  module.exports = { verifyChecksum, sha256File, TARGET_MAP };
}

// Only auto-run when executed as the npm postinstall script, never on
// require() (unit tests require this file for verifyChecksum/sha256File;
// an unconditional main() would trigger a real network download attempt
// on fresh checkouts without bin/ or target/release).
if (typeof require !== 'undefined' && require.main === module) {
  main().catch((err) => {
    console.warn(`[agent-debugger] Postinstall error: ${err.message}`);
  });
}
