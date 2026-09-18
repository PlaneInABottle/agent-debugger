const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { execFile } = require('node:child_process');

// End-to-end PATH-selection tests for bin/agent-debugger.js. The wrapper
// is copied to an isolated dir (so its __dirname has no native binary
// beside it) and run with a scrubbed HOME/PATH:
//
// 1. shim-only PATH: the only `agent-debugger` on PATH is a symlink to the
//    wrapper itself. Pre-fix the wrapper re-spawned itself without bound
//    (path.resolve() can never equal a shim path); now it must exit 1
//    with the not-found message instead of hanging.
// 2. shim first, real binary later: the wrapper must skip its own shim
//    and exec the later native binary (pre-fix it stopped at the first
//    `which` hit or recursed).

function setup() {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), 'wrapper-find-'));
  const wdir = path.join(base, 'w');
  fs.mkdirSync(wdir);
  fs.copyFileSync(
    path.join(__dirname, '..', 'bin', 'agent-debugger.js'),
    path.join(wdir, 'agent-debugger.js'));
  // A same-dir shim pointing at the wrapper: symlink when the platform
  // allows (faithful npm-shim shape — realpath collapses to the wrapper
  // itself), plain copy otherwise (Windows CI cannot always create
  // symlinks; a copy still terminates post-fix through the same
  // self-checks, and hangs pre-fix, so the regression signal survives).
  const shimDir = path.join(base, 'shim');
  fs.mkdirSync(shimDir);
  const shim = path.join(shimDir, 'agent-debugger');
  try {
    fs.symlinkSync(path.join(wdir, 'agent-debugger.js'), shim);
  } catch {
    fs.copyFileSync(path.join(wdir, 'agent-debugger.js'), shim);
  }
  const fakeDir = path.join(base, 'fakebin');
  fs.mkdirSync(fakeDir);
  const fake = path.join(fakeDir, 'agent-debugger');
  fs.writeFileSync(fake, '#!/bin/sh\necho FAKE_NATIVE_MARKER\nexit 7\n');
  fs.chmodSync(fake, 0o755);
  const emptyHome = path.join(base, 'emptyhome');
  fs.mkdirSync(emptyHome);
  return { base, wdir, shimDir, fakeDir, emptyHome };
}

function runWrapper(t, wdir, pathValue, emptyHome) {
  return new Promise((resolve) => {
    execFile(process.execPath, [path.join(wdir, 'agent-debugger.js'), 'somearg'], {
      timeout: 15000,
      env: { ...process.env, PATH: pathValue, HOME: emptyHome, USERPROFILE: emptyHome },
    }, (err, stdout, stderr) => {
      resolve({ err, stdout: String(stdout), stderr: String(stderr) });
    });
  });
}

test('shim-only PATH exits 1 instead of re-spawning itself', async (t) => {
  const { shimDir, wdir, emptyHome } = setup();
  t.after(() => fs.rmSync(path.dirname(wdir), { recursive: true, force: true }));
  const { err, stderr } = await runWrapper(t, wdir, shimDir, emptyHome);
  assert.ok(err, 'expected nonzero exit');
  assert.equal(err.code, 1);
  assert.match(stderr, /Native executable was not found/);
});

test('own shim is skipped for a later native binary on PATH', { skip: process.platform === 'win32' ? 'fake native binary is a POSIX sh script (no shebang exec on Windows); PATH-skip selection stays covered by the findBinary override test on all platforms' : undefined }, async (t) => {
  const { shimDir, fakeDir, wdir, emptyHome } = setup();
  t.after(() => fs.rmSync(path.dirname(wdir), { recursive: true, force: true }));
  const { err, stdout } = await runWrapper(
    t, wdir, `${shimDir}${path.delimiter}${fakeDir}`, emptyHome);
  assert.ok(err, 'fake binary exits 7');
  assert.equal(err.code, 7);
  assert.match(stdout, /FAKE_NATIVE_MARKER/);
});

test('findBinary isolates steps under overrides (all platforms)', async (t) => {
  const { findBinary } = require('../bin/agent-debugger.js');
  const wrapperJs = path.join(__dirname, '..', 'bin', 'agent-debugger.js');
  const base = fs.mkdtempSync(path.join(os.tmpdir(), 'wrapper-unit-'));
  t.after(() => fs.rmSync(base, { recursive: true, force: true }));
  const nope = path.join(base, 'nope');
  const emptyHome = path.join(base, 'emptyhome');
  fs.mkdirSync(emptyHome);
  const over = { localBin: nope, repoTarget: nope, homeDir: emptyHome };
  const binName = process.platform === 'win32' ? 'agent-debugger.exe' : 'agent-debugger';
  const savedPath = process.env.PATH;
  t.after(() => { process.env.PATH = savedPath; });

  // A wrapper path in the local-bin slot is recognized as self and
  // skipped, not returned.
  process.env.PATH = '';
  assert.equal(findBinary({ ...over, localBin: wrapperJs }), null);

  // A same-named native file later on PATH is returned verbatim.
  const fakeDir = path.join(base, 'fakebin');
  fs.mkdirSync(fakeDir);
  const fake = path.join(fakeDir, binName);
  fs.writeFileSync(fake, 'not-executed');
  process.env.PATH = fakeDir;
  assert.equal(findBinary(over), fake);
});
