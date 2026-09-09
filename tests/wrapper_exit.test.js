const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

// Exit-code matrix for the real bin/agent-debugger.js wrapper, run from an
// isolated fixture: the wrapper is copied next to a fake `agent-debugger`
// binary so findBinary resolves the fixture (never the repo target dir,
// ~/.cargo/bin, or PATH).
function fixture(childScript) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'wrapper-exit-'));
  fs.copyFileSync(
    path.join(__dirname, '..', 'bin', 'agent-debugger.js'),
    path.join(dir, 'agent-debugger.js'),
  );
  const bin = path.join(dir, 'agent-debugger');
  fs.writeFileSync(bin, childScript, { mode: 0o755 });
  fs.mkdirSync(path.join(dir, 'empty-path'));
  return dir;
}

function runWrapper(dir) {
  return spawnSync(process.execPath, [path.join(dir, 'agent-debugger.js')], {
    encoding: 'utf8',
    env: {
      ...process.env,
      HOME: path.join(dir, 'no-home'),
      USERPROFILE: path.join(dir, 'no-home'),
      PATH: path.join(dir, 'empty-path'),
    },
  });
}

test('normal exits preserve the child code', () => {
  for (const code of [0, 42]) {
    const dir = fixture(`#!/bin/sh\nexit ${code}\n`);
    const r = runWrapper(dir);
    assert.equal(r.status, code, `exit ${code}: stdout=${r.stdout} stderr=${r.stderr}`);
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('signal-terminated child exits nonzero (never 0)', () => {
  for (const [sig, want] of [['TERM', 143], ['INT', 130], ['HUP', 129]]) {
    const dir = fixture(`#!/bin/sh\nkill -${sig} $$\n`);
    const r = runWrapper(dir);
    assert.equal(
      r.status,
      want,
      `SIG${sig}: expected ${want}, got ${r.status} stdout=${r.stdout} stderr=${r.stderr}`,
    );
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
