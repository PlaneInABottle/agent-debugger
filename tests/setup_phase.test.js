// Setup-failure phase (error.json `phase`): typed, never message-matched.
// Usage (spec validation, conflicts, unknown args) and ConfigError (a
// valid CDP refusal of a breakpoint install) read as config; RuntimeError
// and any other unexpected exception read as runtime (truthful internal
// error, never endpoint-diagnosed); connect loss, request IO/timeout, and
// target exit stay transport. Real Session objects, stubbed transports —
// no timers in assertions.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
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

const node = loadBridge(
  'bridge/node/src/nodebridge.js',
  'Session, StopTimeout, Usage, BridgeErr, ConfigError, RuntimeError, phaseOfError, setupErrorPayload, dirFromArgv, writeParseError');
const browser = loadBridge(
  'bridge/browser/src/browserbridge.js',
  'Session, StopTimeout, Usage, BridgeErr, ConfigError, RuntimeError, phaseOfError, setupErrorPayload, dirFromArgv, writeParseError');

function tmpdir(prefix) {
  return fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), prefix)));
}

function nodeSession(dir, over = {}) {
  const st = new node.Session({
    kind: 'attach', dir, host: '127.0.0.1', port: 9333, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20, programArgs: [],
    ...over,
  });
  st.exited = false;
  return st;
}

function browserSession(dir, over = {}) {
  const st = new browser.Session({
    kind: 'attach', dir, host: '127.0.0.1', port: 9333, tab: 'tick',
    srcs: [], breaks: [], logpoints: [], wantExc: false, timeout: 20,
    ...over,
  });
  st.exited = false;
  return st;
}

for (const [name, mod] of [['node', node], ['browser', browser]]) {
  test(`${name} phase derives from the exception type, error unchanged`, () => {
    assert.deepEqual(
      mod.setupErrorPayload(new mod.BridgeErr('boom'), 'boom'),
      { schemaVersion: 2, error: 'boom', phase: 'transport' });
    assert.deepEqual(
      mod.setupErrorPayload(new mod.Usage('no such file: x'), 'no such file: x'),
      { schemaVersion: 2, error: 'no such file: x', phase: 'config' });
    assert.deepEqual(
      mod.setupErrorPayload(new mod.ConfigError('CDP setBreakpoint failed: bad cond'), 'CDP setBreakpoint failed: bad cond'),
      { schemaVersion: 2, error: 'CDP setBreakpoint failed: bad cond', phase: 'config' });
    // ConfigError is still a BridgeErr: existing catches keep working.
    assert.ok(new mod.ConfigError('x') instanceof mod.BridgeErr);
    // Explicit runtime marker + unexpected exceptions: runtime (truthful
    // internal error, never endpoint-diagnosed). Target exits stay
    // transport (never masked by runtime).
    assert.equal(mod.phaseOfError(new mod.RuntimeError('track boom')), 'runtime');
    assert.equal(mod.phaseOfError(new Error('bug')), 'runtime');
    assert.deepEqual(
      mod.setupErrorPayload(new mod.RuntimeError('track boom'), 'internal: track boom'),
      { schemaVersion: 2, error: 'internal: track boom', phase: 'runtime' });
    assert.deepEqual(
      mod.setupErrorPayload(new Error('bug'), 'internal: Error: bug'),
      { schemaVersion: 2, error: 'internal: Error: bug', phase: 'runtime' });
    for (const e of [new mod.BridgeErr('target exited'),
      null, undefined, 'config', 7, {}]) {
      assert.equal(mod.phaseOfError(e), 'transport', `${name}: ${String(e && e.message || e)}`);
    }
  });
}

test('node arm refusal is config, arm transport loss stays transport', async () => {
  // A valid V8 refusal of the install (ConfigError from the shared CDP
  // core with the arm opt-in) reports config ...
  const refused = nodeSession(tmpdir('phase-'),
    { breaks: [{ path: '/tmp/x.js', line: 3 }] });
  refused.discoverAttach = async () => 'ws://127.0.0.1:9333/n7';
  refused.connect = async () => {
    refused.cdp = {
      request: async (method) => {
        if (method === 'Debugger.setBreakpointByUrl') {
          throw new node.ConfigError('CDP Debugger.setBreakpointByUrl failed: bad condition');
        }
        return {};
      },
    };
  };
  refused.buildTargetIdentity = () => {};
  await assert.rejects(refused.handshake(), /bad condition/);
  // ... while a real transport loss during arming (close/timeout) stays
  // transport even though the connection was established.
  const dropped = nodeSession(tmpdir('phase-'),
    { breaks: [{ path: '/tmp/x.js', line: 3 }] });
  dropped.discoverAttach = async () => 'ws://127.0.0.1:9333/n7';
  dropped.connect = async () => {
    const err = new node.BridgeErr('CDP connection closed');
    dropped.cdp = { request: async () => { throw err; } };
  };
  dropped.buildTargetIdentity = () => {};
  const caught = await dropped.handshake().then(
    () => { throw new Error('handshake should have failed'); },
    (e) => e);
  assert.match(caught.message, /CDP connection closed/);
  assert.equal(node.phaseOfError(caught), 'transport');
});

test('node pre-connect failure stays transport', async () => {
  const st = nodeSession(tmpdir('phase-'));
  st.discoverAttach = async () => {
    throw new node.BridgeErr('attach failed (h:1): refused');
  };
  await assert.rejects(st.handshake(), /attach failed/);
  assert.equal(node.phaseOfError(null), 'transport');
});

for (const [name, mod] of [['node', node], ['browser', browser]]) {
  test(`${name} parse-error scan finds --dir without parsing`, () => {
    assert.equal(mod.dirFromArgv(['session', '--dir', '/s', '--break', 'a:1']), '/s');
    assert.equal(mod.dirFromArgv(['session', '--dir=/s']), '/s');
    assert.equal(mod.dirFromArgv(['session', '--break', 'a:1']), null);
    assert.equal(mod.dirFromArgv(['session', '--dir']), null);
    // Empty --dir= scans as empty and writes nothing (never the cwd).
    assert.equal(mod.dirFromArgv(['session', '--dir=']), '');
    mod.writeParseError(['session', '--dir='], 'x');
  });

  test(`${name} parse-error file is config and best-effort`, () => {
    const dir = tmpdir('phase-');
    mod.writeParseError(['session', '--dir', dir, '--break', 'x'], 'no such line: x');
    const body = JSON.parse(fs.readFileSync(path.join(dir, 'error.json'), 'utf-8'));
    assert.equal(body.schemaVersion, 2);
    assert.equal(body.error, 'no such line: x');
    assert.equal(body.phase, 'config');
    // No --dir or a bad dir: never throws, nothing to read.
    mod.writeParseError(['session'], 'x');
    mod.writeParseError([], 'x');
    mod.writeParseError(['session', '--dir', path.join(dir, 'nope', 'nested')], 'x');
  });
}
