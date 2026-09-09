#!/usr/bin/env node
const fs = require('fs');
const path = require('path');
const { spawn, execSync } = require('child_process');

const isWin = process.platform === 'win32';
const binName = isWin ? 'agent-debugger.exe' : 'agent-debugger';
const localBin = path.join(__dirname, binName);

function findBinary() {
  // 1. Check local binary unpacked in bin/
  if (fs.existsSync(localBin)) {
    return localBin;
  }

  // 2. Check repo release target (for local development)
  const repoTarget = path.join(__dirname, '..', 'target', 'release', binName);
  if (fs.existsSync(repoTarget)) {
    return repoTarget;
  }

  // 3. Check ~/.cargo/bin/agent-debugger
  const homeDir = process.env.HOME || process.env.USERPROFILE || '';
  if (homeDir) {
    const cargoBin = path.join(homeDir, '.cargo', 'bin', binName);
    if (fs.existsSync(cargoBin)) {
      return cargoBin;
    }
  }

  // 4. Check system PATH
  try {
    const lookupCmd = isWin ? `where ${binName}` : `which ${binName}`;
    const foundPath = execSync(lookupCmd, { stdio: ['pipe', 'pipe', 'ignore'] }).toString().trim().split('\n')[0];
    if (foundPath && fs.existsSync(foundPath) && path.resolve(foundPath) !== path.resolve(__filename)) {
      return foundPath;
    }
  } catch {
    // Ignore PATH lookup failure
  }

  return null;
}

const binaryPath = findBinary();

if (!binaryPath) {
  console.error('\x1b[31m[agent-debugger]\x1b[0m Native executable was not found.');
  console.error(`Platform: ${process.platform} (${process.arch})`);
  console.error('\nPlease install the binary using one of the following methods:');
  console.error('  1. Run postinstall: npm rebuild @planeinabottle/agent-debugger');
  console.error('  2. Install via Cargo: cargo install agent-debugger');
  console.error('  3. Download from GitHub Releases: https://github.com/PlaneInABottle/agent-debugger/releases\n');
  process.exit(1);
}

const child = spawn(binaryPath, process.argv.slice(2), {
  stdio: 'inherit',
  windowsHide: true,
});

child.on('error', (err) => {
  console.error(`\x1b[31m[agent-debugger error]\x1b[0m ${err.message}`);
  process.exit(1);
});

let childExited = false;

child.on('close', (code, signal) => {
  // The child is gone: stop forwarding signals into it. Re-raising the
  // signal on ourselves (process.kill(process.pid, signal)) would hit our
  // own forwarding listener below and exit 0 — a signal death must exit
  // nonzero, using the 128+signo convention (INT 130, TERM 143, HUP 129).
  // Normal exits still preserve the child code.
  childExited = true;
  if (signal) {
    const sigExit = { SIGINT: 130, SIGTERM: 143, SIGHUP: 129 };
    process.exit(sigExit[signal] ?? 128);
  } else {
    process.exit(code ?? 0);
  }
});

// Forward common termination signals to the child process, but only while
// it is still alive — after close the forwarding stops (see above).
['SIGINT', 'SIGTERM', 'SIGHUP'].forEach((signal) => {
  process.on(signal, () => {
    if (!childExited && child && child.exitCode === null && !child.killed) {
      child.kill(signal);
    }
  });
});
