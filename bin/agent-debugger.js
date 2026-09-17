#!/usr/bin/env node
const fs = require('fs');
const path = require('path');
const { spawn } = require('child_process');

const isWin = process.platform === 'win32';
const binName = isWin ? 'agent-debugger.exe' : 'agent-debugger';

function findBinary(over = {}) {
  // Real path of this wrapper (symlinks resolved): PATH lookups below
  // return shim paths that resolve to this same file, so comparing with
  // path.resolve() alone can never recognize them (unbounded self-spawn
  // when the native binary is missing — exactly the state this error
  // text targets).
  let selfReal = null;
  try {
    selfReal = fs.realpathSync(__filename);
  } catch {
    selfReal = path.resolve(__filename);
  }
  const isSelf = (p) => {
    try {
      return fs.realpathSync(p) === selfReal;
    } catch {
      return path.resolve(p) === path.resolve(__filename);
    }
  };
  // Overrides exist only so unit tests isolate the PATH scan from the
  // developer machine (local bin/, target/, ~/.cargo/bin may all exist).
  const localBin = over.localBin || path.join(__dirname, binName);
  const repoTarget = over.repoTarget
    || path.join(__dirname, '..', 'target', 'release', binName);
  const homeDir = over.homeDir !== undefined
    ? over.homeDir
    : (process.env.HOME || process.env.USERPROFILE || '');
  // 1. Check local binary unpacked in bin/
  if (fs.existsSync(localBin) && !isSelf(localBin)) {
    return localBin;
  }

  // 2. Check repo release target (for local development)
  if (fs.existsSync(repoTarget)) {
    return repoTarget;
  }

  // 3. Check ~/.cargo/bin/agent-debugger
  if (homeDir) {
    const cargoBin = path.join(homeDir, '.cargo', 'bin', binName);
    if (fs.existsSync(cargoBin)) {
      return cargoBin;
    }
  }

  // 4. Scan system PATH directly (every hit, not just the first: the
  // first hit is ordering-dependent and is often this wrapper's own shim
  // even when a real native binary sits later in PATH). No subprocess:
  // portable across sh implementations and avoids word-splitting a path.
  const pathDirs = (process.env.PATH || '').split(path.delimiter);
  for (const d of pathDirs) {
    if (!d) continue;
    const cand = path.join(d, binName);
    let st = null;
    try {
      st = fs.statSync(cand);
    } catch {
      continue;
    }
    if (st.isFile() && !isSelf(cand)) {
      return cand;
    }
  }

  return null;
}

const binaryPath = findBinary();

if (typeof module !== 'undefined' && module.exports) {
  module.exports = { findBinary };
}

// Only auto-run when executed directly, never on require() (unit tests
// require this file for findBinary; an unconditional spawn would re-exec
// the wrapper — the self-spawn this module guards against).
if (typeof require !== 'undefined' && require.main === module) {
  runBinary(binaryPath);
}

function runBinary(binaryPath) {
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
}
