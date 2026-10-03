const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const Module = require('node:module');

// Cross-language contract fixtures: single source for frozen strings.
// Same loadBridge idiom as the other JS suites (no harness change).
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
  'Session, StopTimeout, Usage, BridgeErr, ConfigError, RuntimeError, phaseOfError, setupErrorPayload, closeFromConn, PARK_WARNING, truncStr, MAX_STRING, MAX_VARS, MAX_FRAMES, MAX_LOG_LINES, IDENT_ARRAY_CAP, RESUME_CMDS, WAIT_CMDS, CAPTURE_CMDS, MUTATION_CMDS');
const browser = loadBridge(
  'bridge/browser/src/browserbridge.js',
  'Session, StopTimeout, Usage, BridgeErr, ConfigError, RuntimeError, phaseOfError, setupErrorPayload, closeFromConn, PARK_WARNING, truncStr, MAX_STRING, MAX_VARS, MAX_FRAMES, MAX_LOG_LINES, RESUME_CMDS, WAIT_CMDS, CAPTURE_CMDS, MUTATION_CMDS');

function fixture(name) {
  return JSON.parse(fs.readFileSync(path.join(__dirname, 'contract', name), 'utf-8'));
}

function nodeSession() {
  const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'contract-')));
  const st = new node.Session({
    kind: 'attach', dir, host: '127.0.0.1', port: 9333, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20, programArgs: [],
  });
  st.exited = false;
  return st;
}

function browserSession() {
  const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'contract-')));
  const st = new browser.Session({
    kind: 'attach', dir, host: '127.0.0.1', port: 9334, tab: 'tick',
    srcs: [], breaks: [], logpoints: [], wantExc: false, timeout: 20,
  });
  st.exited = false;
  st.verifyTab = async () => {};
  return st;
}

// In-memory framed conn: captures what closeFromConn writes without TCP.
function fakeConn() {
  const chunks = [];
  return {
    chunks,
    write(msg, cb) { chunks.push(Buffer.from(msg)); cb(null); },
    destroy() {},
  };
}

function readFrame(buf) {
  const head = buf.toString('utf-8');
  const idx = head.indexOf('\r\n\r\n');
  assert.notEqual(idx, -1, 'framed close ACK');
  return JSON.parse(head.slice(idx + 4));
}

test('contract: timeout prefix frozen on node bridge', () => {
  const fx = fixture('timeout_prefix.json');
  const msg = nodeSession().timeoutText(2);
  assert.ok(msg.startsWith(fx.prefix + ' '), `${msg} must start with frozen prefix`);
  assert.ok(msg.includes(fx.example), `${msg} must contain frozen example`);
  const src = fs.readFileSync(path.join(__dirname, '..', 'bridge', 'node', 'src', 'nodebridge.js'), 'utf-8');
  for (const stage of fx.waitContext.captureStageValues) {
    assert.ok(src.includes(`"${stage}"`) || src.includes(`'${stage}'`),
      `captureStage ${stage} must stay frozen`);
  }
});

test('contract: error phases frozen on node + browser bridges', () => {
  const fx = fixture('error_phases.json');
  assert.deepEqual(fx.phases, ['transport', 'config', 'runtime']);
  for (const [name, mod] of [['node', node], ['browser', browser]]) {
    assert.equal(mod.phaseOfError(new mod.BridgeErr('boom')), 'transport', `${name} transport`);
    assert.equal(mod.phaseOfError(new mod.Usage('no such file: x')), 'config', `${name} config`);
    assert.equal(mod.phaseOfError(new mod.RuntimeError('bug')), 'runtime', `${name} runtime`);
  }
  const msg = fx.corruptSetupErrorTemplate.replace('{name}', 'demo');
  assert.ok(msg.includes('demo') && msg.includes('close and retry'));
});

test('contract: identity caps frozen (field shared, totals layer-local)', () => {
  const fx = fixture('identity_caps.json');
  assert.equal(fx.fieldCap, 512);
  // Field cap is 512 in every bridge; the browser tab-identity path spells
  // the symbol OBSERVED_FIELD_CAP (pre-existing name, same frozen value).
  const fieldCapRe = /(IDENT|OBSERVED)_FIELD_CAP = 512/;
  for (const rel of ['bridge/node/src/nodebridge.js', 'bridge/browser/src/browserbridge.js']) {
    const src = fs.readFileSync(path.join(__dirname, '..', rel), 'utf-8');
    assert.ok(fieldCapRe.test(src), `${rel} field cap`);
    assert.ok(src.includes(`IDENT_TOTAL_CAP = ${fx.bridgeTotalCap}`), `${rel} bridge total cap`);
    assert.ok(src.includes(fx.truncSuffixFormat.replace('N', '').slice(0, 4)), `${rel} trunc suffix`);
  }
});

test('contract: close/status shape frozen', async () => {
  const fx = fixture('close_status.json');
  // Behavioral: both daemons ACK a framed close with bridgeCloseAck
  // (boolean closed, target main); the CLI maps it to the cliClose shape
  // with the session name + confirmed flag, held by the live suites.
  for (const [name, mod, make] of [['node', node, nodeSession], ['browser', browser, browserSession]]) {
    const st = make();
    st.cleanup = async () => {};
    const conn = fakeConn();
    await mod.closeFromConn(st, conn);
    assert.deepEqual(readFrame(Buffer.concat(conn.chunks)), fx.bridgeCloseAck);
    assert.equal(fx.close.target, 'main', `${name} cli target`);
  }
  assert.equal(fx.close.confirmedField, 'confirmed');
  assert.ok(fx.closeConfirmedExample.confirmed);
  assert.ok(!fx.closeUnconfirmedExample.confirmed);
});

test('contract: value caps frozen identically on node + browser', () => {
  const fx = fixture('value_caps.json');
  for (const [name, mod] of [['node', node], ['browser', browser]]) {
    assert.equal(mod.MAX_STRING, fx.maxString, `${name} maxString`);
    assert.equal(mod.MAX_VARS, fx.maxVars, `${name} maxVars`);
    assert.equal(mod.MAX_FRAMES, fx.maxFrames, `${name} maxFrames`);
    assert.equal(mod.MAX_LOG_LINES, fx.logsRing, `${name} logsRing`);
    assert.equal(mod.PARK_WARNING, fx.parkWarning, `${name} parkWarning`);
    const capped = mod.truncStr('x'.repeat(600), fx.maxString);
    assert.equal(capped,
      'x'.repeat(fx.maxString) + fx.truncSuffixFormat.replace('N', String(600 - fx.maxString)),
      `${name} trunc idiom`);
  }
  // Byte-identical park warning across the two JS bridges (py/java held
  // by their own harnesses + the Rust textual guard).
  assert.equal(node.PARK_WARNING, browser.PARK_WARNING);
});

test('contract: logs tail clamp [0,500] default 50 on node + browser', () => {
  const fx = fixture('value_caps.json');
  for (const [name, mod, make] of [['node', node, nodeSession], ['browser', browser, browserSession]]) {
    const st = make();
    const lines = Array.from({ length: 100 }, (_, i) => `{"line":${i}}`);
    fs.writeFileSync(path.join(st.cfg.dir, 'logs.jsonl'), lines.join('\n') + '\n');
    assert.equal(st.cmdLogs({}).lines.length, fx.tailDefault, `${name} default`);
    assert.deepEqual(st.cmdLogs({ tail: 0 }).lines, [], `${name} zero`);
    assert.equal(st.cmdLogs({ tail: 9999 }).lines.length, 100, `${name} clamped`);
    assert.deepEqual(st.cmdLogs({ tail: -3 }).lines, [], `${name} negative`);
    assert.equal(st.cmdLogs({ tail: 'nonsense' }).lines.length, fx.tailDefault, `${name} non-numeric`);
    const src = fs.readFileSync(path.join(__dirname, '..',
      name === 'node' ? 'bridge/node/src/nodebridge.js' : 'bridge/browser/src/browserbridge.js'), 'utf-8');
    assert.ok(src.includes('Math.max(0, Math.min(500'), `${name} clamp bound in source`);
  }
});

test('contract: layer-local records (argv cap, timeout text, dispatch groups)', () => {
  const fx = fixture('layer_local.json');
  // argv array cap lives only where argv arrays are kept (node, not browser).
  assert.equal(node.IDENT_ARRAY_CAP, fx.argvArrayCap);
  assert.ok(fx.argvArrayCapHolders.includes('node'));
  assert.ok(fx.argvArrayCapAbsent.includes('browser'));
  const browserSrc = fs.readFileSync(path.join(__dirname, '..', 'bridge/browser/src/browserbridge.js'), 'utf-8');
  assert.ok(!browserSrc.includes('IDENT_ARRAY_CAP'), 'browser drops argv wholesale');
  assert.ok(browserSrc.includes('argv: null'), 'browser argv null record');
  // Timeout message text per bridge; integer domain agrees at max 3600.
  assert.equal(fx.timeoutMax, 3600);
  for (const [name, rel] of [['node', 'bridge/node/src/nodebridge.js'], ['browser', 'bridge/browser/src/browserbridge.js']]) {
    const src = fs.readFileSync(path.join(__dirname, '..', rel), 'utf-8');
    assert.ok(src.includes(`'${fx.timeoutMessages[name]}'`), `${name} timeout text`);
  }
  // Dispatch groups: node resume is continue/step; browser resume adds reload.
  assert.deepEqual([...node.RESUME_CMDS].sort(), fx.dispatchGroups.node.resume.sort());
  assert.deepEqual([...node.WAIT_CMDS], fx.dispatchGroups.node.wait);
  assert.deepEqual([...node.CAPTURE_CMDS], fx.dispatchGroups.node.capture);
  assert.deepEqual([...node.MUTATION_CMDS].sort(), fx.dispatchGroups.node.mutation.sort());
  assert.deepEqual([...browser.RESUME_CMDS].sort(), fx.dispatchGroups.browser.resume.sort());
  assert.ok(browser.RESUME_CMDS.has('reload'), 'browser reload is a resume op');
  assert.ok(!node.RESUME_CMDS.has('reload'), 'node has no reload resume');
});

test('contract: breaks echo rules frozen', async () => {
  const fx = fixture('breaks_echo.json');
  assert.ok(fx.confirmedOnlyPersistence);
  assert.ok(fx.totalFailureOkFalse);
  // Behavioral: total CDP failure throws (framed layer maps the throw
  // to ok:false) with nothing confirmed or persisted.
  const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'contract-')));
  const file = path.join(dir, 'm1_add.js');
  fs.writeFileSync(file, Array.from({ length: 12 }, (_, i) => `const v${i} = ${i};`).join('\n') + '\n');
  const st = nodeSession();
  st.cdp = { request: async () => { throw new Error('CDP gone'); } };
  await assert.rejects(st.cmdBreaksAdd({ breaks: [`${file}:3`] }), /breaks add failed/);
  assert.deepEqual(st.cfg.breaks, []);
});
