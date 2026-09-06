const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const Module = require('node:module');

// Same harness as breaks_add.test.js: real bridge sources with only the
// trailing main() invocation stripped, shared wire core resolved from the
// repo single-source.
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

const node = loadBridge('bridge/node/src/nodebridge.js', 'Session, parseBreak, canon');
const browser = loadBridge('bridge/browser/src/browserbridge.js', 'Session, parseBreak, fragRegex, buildTabIdentity, tabHint');

function tmpdir(prefix) {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

function writeJs(dir, name = 'm2_rm.js', lines = 12) {
  const file = path.join(dir, name);
  fs.writeFileSync(file,
    Array.from({ length: lines }, (_, i) => `const v${i} = ${i};`).join('\n') + '\n');
  return file;
}

// A session with one armed line break: cfg entry + stored raw + key map +
// live record, mirroring what armBreakpoints/cmdBreaksAdd produce.
function armedNode(dir, file, line, cond = null, raw = null) {
  const canonFile = fs.realpathSync(file);
  const key = `${canonFile}:${line}|${cond || ''}`;
  const st = new node.Session({
    kind: 'attach', dir, host: 'localhost', port: 9229, srcs: [],
    breaks: [{ path: canonFile, line, cond }], logpoints: [],
    wantExc: false, timeout: 20, programArgs: [],
    breakRaws: { [key]: raw || `${file}:${line}${cond ? `|${cond}` : ''}` },
  });
  st.exited = false;
  const rec = { spec: `${path.basename(file)}:${line}${cond ? `|${cond}` : ''}`, kind: 'break', hits: 0, state: 'verified' };
  st.stopStates.push(rec);
  st.breakKeys.set(key, 'bp-1');
  st.breakIdToRec.set('bp-1', { rec, line });
  return { st, key, canonFile };
}

function stubCdp(st, onRemove) {
  const calls = [];
  st.cdp = {
    request: async (method, params, timeoutMs) => {
      calls.push([method, params, timeoutMs]);
      if (method === 'Debugger.removeBreakpoint') {
        if (onRemove) onRemove(method, params);
        return {};
      }
      if (method === 'Debugger.setBreakpointByUrl') {
        return { breakpointId: 'bp-re', locations: [{ lineNumber: params.lineNumber }] };
      }
      return {};
    },
  };
  return calls;
}

test('node: remove echoes the stored raw and drops cfg/state', async () => {
  const dir = tmpdir('m2-rm-node-');
  const file = writeJs(dir);
  const { st, key } = armedNode(dir, file, 3);
  const calls = stubCdp(st);
  const raw = `${file}:3`;
  const resp = await st.cmdBreaksRemove({ breaks: [raw] });
  assert.equal(resp.ok, true);
  assert.equal(resp.removed.length, 1);
  assert.equal(resp.removed[0].raw, raw);
  assert.deepEqual(st.cfg.breaks, []);
  assert.deepEqual(st.stopStates, []);
  assert.ok(!st.breakKeys.has(key));
  assert.equal(calls.length, 1);
  assert.equal(calls[0][0], 'Debugger.removeBreakpoint');
  assert.equal(calls[0][1].breakpointId, 'bp-1');
  assert.equal(calls[0][2], 5000);
});

test('node: remove missing spec is not an error and sends no CDP', async () => {
  const dir = tmpdir('m2-rm-node-');
  const st = new node.Session({
    kind: 'attach', dir, host: 'localhost', port: 9229, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20, programArgs: {},
  });
  st.exited = false;
  const calls = stubCdp(st);
  const resp = await st.cmdBreaksRemove({ breaks: ['ghost.js:9'] });
  assert.equal(resp.ok, true);
  assert.deepEqual(resp.removed, []);
  assert.deepEqual(resp.missing, ['ghost.js:9']);
  assert.equal(calls.length, 0);
});

test('node: remove works after the source file is deleted', async () => {
  const dir = tmpdir('m2-rm-node-');
  const file = writeJs(dir, 'gone.js');
  const { st } = armedNode(dir, file, 3);
  fs.unlinkSync(file);
  const calls = stubCdp(st);
  const resp = await st.cmdBreaksRemove({ breaks: [`${file}:3`] });
  assert.equal(resp.ok, true);
  assert.equal(resp.removed.length, 1);
  assert.deepEqual(st.cfg.breaks, []);
  assert.equal(calls.length, 1);
});

test('node: plain spec does not remove a conditional record', async () => {
  const dir = tmpdir('m2-rm-node-');
  const file = writeJs(dir);
  const { st } = armedNode(dir, file, 3, 'x > 1');
  const calls = stubCdp(st);
  const resp = await st.cmdBreaksRemove({ breaks: [`${file}:3`] });
  assert.equal(resp.ok, true);
  assert.deepEqual(resp.removed, []);
  assert.deepEqual(resp.missing, [`${file}:3`]);
  assert.equal(st.cfg.breaks.length, 1);
  assert.equal(calls.length, 0);
});

test('node: clear drops all line breaks but keeps logpoints', async () => {
  const dir = tmpdir('m2-rm-node-');
  const file = writeJs(dir);
  const canonFile = fs.realpathSync(file);
  const k1 = `${canonFile}:3|`;
  const k2 = `${canonFile}:5|`;
  const st = new node.Session({
    kind: 'attach', dir, host: 'localhost', port: 9229, srcs: [],
    breaks: [{ path: canonFile, line: 3, cond: null }, { path: canonFile, line: 5, cond: null }],
    logpoints: [{ path: canonFile, line: 7, template: 't={t}' }],
    wantExc: false, timeout: 20, programArgs: [],
    breakRaws: { [k1]: `${file}:3`, [k2]: `${file}:5` },
  });
  st.exited = false;
  st.breakKeys.set(k1, 'bp-1');
  st.breakKeys.set(k2, 'bp-2');
  const calls = stubCdp(st);
  const resp = await st.cmdBreaksClear();
  assert.equal(resp.ok, true);
  assert.equal(resp.removed.length, 2);
  assert.deepEqual(st.cfg.breaks, []);
  assert.equal(st.cfg.logpoints.length, 1);
  assert.equal(calls.filter((c) => c[0] === 'Debugger.removeBreakpoint').length, 2);
});

test('node: backend failure keeps state and reports failed', async () => {
  const dir = tmpdir('m2-rm-node-');
  const file = writeJs(dir);
  const { st } = armedNode(dir, file, 3);
  st.cdp = {
    request: async () => { throw new Error('adapter exploded'); },
  };
  await assert.rejects(st.cmdBreaksRemove({ breaks: [`${file}:3`] }), /failed/);
  assert.equal(st.cfg.breaks.length, 1);
  assert.equal(st.stopStates.length, 1);
});

test('node: removing a break re-arms a shadowed startup logpoint', async () => {
  const dir = tmpdir('m2-rm-node-');
  const file = writeJs(dir);
  const canonFile = fs.realpathSync(file);
  const key = `${canonFile}:3|`;
  const st = new node.Session({
    kind: 'attach', dir, host: 'localhost', port: 9229, srcs: [],
    breaks: [{ path: canonFile, line: 3, cond: null }],
    logpoints: [{ path: canonFile, line: 3, template: 't={t}' }],
    wantExc: false, timeout: 20, programArgs: [],
    breakRaws: { [key]: `${file}:3` },
  });
  st.exited = false;
  st.shadowedLogs.push({ path: canonFile, line: 3, template: 't={t}' });
  const rec = { spec: 'm2_rm.js:3', kind: 'break', hits: 0, state: 'verified' };
  st.stopStates.push(rec);
  st.breakKeys.set(key, 'bp-1');
  st.breakIdToRec.set('bp-1', { rec, line: 3 });
  stubCdp(st);
  const resp = await st.cmdBreaksRemove({ breaks: [`${file}:3`] });
  assert.equal(resp.ok, true);
  assert.equal(resp.removed.length, 1);
  assert.equal(st.cfg.logpoints.length, 2);
  const rearmed = st.stopStates.find((r) => r.kind === 'logpoint');
  assert.ok(rearmed);
  assert.equal(rearmed.state, 'verified');
});

test('node: session.json carries schemaVersion 2 and layered identity', async () => {  const dir = tmpdir('m2-rm-node-');
  const st = new node.Session({
    kind: 'attach', dir, host: 'localhost', port: 9229, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20, programArgs: [],
    targetIdentitySeed: {
      debuggee: { executable: 'node', argv: ['node', 'app.js'], cwd: '/t' },
      endpoint: {}, adapter: {},
    },
  });
  st.sessionPort = 4242;
  st.publishState(false);
  const saved = JSON.parse(fs.readFileSync(path.join(dir, 'session.json'), 'utf-8'));
  assert.equal(saved.schemaVersion, 2);
  assert.ok(!('observedTarget' in saved));
  assert.match(st.timeoutText(5), /target identity: node node app\.js/);
});

// ---- browser ----

function armedBrowser(dir, frag, line, cond = null) {
  const key = `${frag}:${line}|${cond || ''}`;
  const raw = `${frag}:${line}${cond ? `|${cond}` : ''}`;
  const st = new browser.Session({
    kind: 'attach', dir, host: 'localhost', port: 9222, tab: null, srcs: [],
    breaks: [{ frag, line, cond }], logpoints: [],
    wantExc: false, timeout: 20,
    breakRaws: { [key]: raw },
  });
  st.exited = false;
  st.verifyTab = async () => {};
  const rec = { spec: raw, kind: 'break', hits: 0, state: 'verified' };
  st.stopStates.push(rec);
  st.breakKeys.set(key, 'bp-1');
  st.breakIdToRec.set('bp-1', { rec, line });
  return { st, key, raw };
}

test('browser: remove echoes stored raw and reload never restores it', async () => {
  const dir = tmpdir('m2-rm-br-');
  const { st, raw } = armedBrowser(dir, 'app.js', 8);
  const calls = stubCdp(st);
  const resp = await st.cmdBreaksRemove({ breaks: [raw] });
  assert.equal(resp.ok, true);
  assert.equal(resp.removed.length, 1);
  assert.equal(resp.removed[0].raw, raw);
  assert.deepEqual(st.cfg.breaks, []);
  assert.deepEqual(st.stopStates, []);
  assert.equal(calls.length, 1);
  // Reload plants from cfg: the removed break is gone for good.
  assert.ok(!st.cfg.breaks.some((b) => b.frag === 'app.js' && b.line === 8));
});

test('browser: clear drops every line break, missing stays non-fatal', async () => {
  const dir = tmpdir('m2-rm-br-');
  const { st } = armedBrowser(dir, 'app.js', 8);
  stubCdp(st);
  const cleared = await st.cmdBreaksClear();
  assert.equal(cleared.ok, true);
  assert.equal(cleared.removed.length, 1);
  const resp = await st.cmdBreaksRemove({ breaks: ['app.js:8'] });
  assert.equal(resp.ok, true);
  assert.deepEqual(resp.removed, []);
  assert.deepEqual(resp.missing, ['app.js:8']);
});

test('browser: handshake records tab identity, publish persists it', async () => {  const dir = tmpdir('m2-rm-br-');
  const st = new browser.Session({
    kind: 'attach', dir, host: '127.0.0.1', port: 9222, tab: null, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20,
  });
  // Handshake shape without network: tab known, CDP stubbed past attach.
  st.tab = { id: 'ABC', title: 'Shop', url: 'http://h/app.js', webSocketDebuggerUrl: 'ws://x' };
  const t = st.tab;
  st.cfg.tabIdentity = {
    kind: 'tab', url: t.url, title: t.title, targetId: t.id,
    debugEndpoint: '127.0.0.1:9222', cwd: null, argv: null,
    notApplicable: ['cwd', 'argv'], source: 'cdp-target-list',
    observedAt: 1, unavailable: [], warnings: [],
  };
  st.sessionPort = 4242;
  st.buildTargetIdentity();
  st.publishState(false);
  const saved = JSON.parse(fs.readFileSync(path.join(dir, 'session.json'), 'utf-8'));
  assert.equal(saved.schemaVersion, 2);
  assert.ok(!('observedTarget' in saved));
  assert.equal(saved.targetIdentity.debuggee.targetId, 'ABC');
});

test('browser: long tab url/title are capped and query secrets redacted', () => {
  const sentinel = 'BROWSERSECRET-7aa11c';
  // Redaction case: query survives the field cap, secret value is gone.
  const secretTab = {
    id: 'ABC',
    title: 'Shop',
    url: `http://h/app.js?token=${sentinel}&next=1&author=Jane`,
  };
  const red = browser.buildTabIdentity(secretTab, 'localhost', 9222, 1);
  const redDumped = JSON.stringify(red);
  assert.ok(!redDumped.includes(sentinel), 'raw query secret must not persist');
  assert.ok(red.url.includes('token=%5Bredacted%5D'), 'token query value redacted');
  assert.ok(red.url.includes('next=1'), 'non-secret query kept');
  assert.ok(red.url.includes('author=Jane'), 'author is not a secret');
  // Cap case: remote-controlled long text stays bounded with the idiom.
  const longTab = {
    id: 'ABC',
    title: `Shop ${'t'.repeat(600)}`,
    url: `http://h/${'p'.repeat(600)}/app.js?token=${sentinel}`,
  };
  const obs = browser.buildTabIdentity(longTab, 'localhost', 9222, 1);
  const dumped = JSON.stringify(obs);
  assert.ok(!dumped.includes(sentinel), 'raw query secret must not persist');
  assert.ok(obs.url.includes('(+'), 'url capped with shared idiom');
  assert.ok(obs.title.includes('(+'), 'title capped with shared idiom');
  assert.ok(obs.url.length <= 512 + 30 && obs.title.length <= 512 + 30);
  assert.ok(dumped.length <= 2048, `total capped (got ${dumped.length})`);
  const hint = browser.tabHint(obs);
  assert.ok(hint.length <= 200, `hint capped (got ${hint.length})`);
  assert.ok(!hint.includes(sentinel), 'hint never leaks the secret');
  assert.equal(obs.debugEndpoint, 'localhost:9222');
});

test('node: removing a startup-rejected break drops its exact record', async () => {
  const dir = tmpdir('m2-rm-node-');
  const file = writeJs(dir);
  const canonFile = fs.realpathSync(file);
  const key = `${canonFile}:3|`;
  const st = new node.Session({
    kind: 'attach', dir, host: 'localhost', port: 9229, srcs: [],
    breaks: [{ path: canonFile, line: 3, cond: null }], logpoints: [],
    wantExc: false, timeout: 20, programArgs: [],
    breakRaws: { [key]: `${file}:3` },
  });
  st.exited = false;
  // Rejected plant (no breakpointId): relative display spec, absolute key.
  const rec = { spec: 'm2_rm.js:3', kind: 'break', hits: 0, state: 'rejected', detail: 'x' };
  st.stopStates.push(rec);
  st.breakKeys.set(key, null);
  st.breakRecByKey.set(key, rec);
  const calls = stubCdp(st);
  const resp = await st.cmdBreaksRemove({ breaks: [`${file}:3`] });
  assert.equal(resp.ok, true);
  assert.equal(resp.removed.length, 1);
  assert.equal(resp.removed[0].raw, `${file}:3`);
  assert.deepEqual(st.cfg.breaks, []);
  assert.deepEqual(st.stopStates, [], 'no orphan record may survive');
  assert.equal(calls.length, 0, 'nothing to unplant backend-side');
});

test('browser: removing a startup-rejected break drops its exact record', async () => {
  const dir = tmpdir('m2-rm-br-');
  const key = 'app.js:8|';
  const st = new browser.Session({
    kind: 'attach', dir, host: 'localhost', port: 9222, tab: null, srcs: [],
    breaks: [{ frag: 'app.js', line: 8, cond: null }], logpoints: [],
    wantExc: false, timeout: 20,
    breakRaws: { [key]: 'app.js:8' },
  });
  st.exited = false;
  st.verifyTab = async () => {};
  const rec = { spec: 'app.js:8', kind: 'break', hits: 0, state: 'rejected', detail: 'x' };
  st.stopStates.push(rec);
  st.breakKeys.set(key, null);
  st.breakRecByKey.set(key, rec);
  const calls = stubCdp(st);
  const resp = await st.cmdBreaksRemove({ breaks: ['app.js:8'] });
  assert.equal(resp.ok, true);
  assert.equal(resp.removed.length, 1);
  assert.deepEqual(st.cfg.breaks, []);
  assert.deepEqual(st.stopStates, [], 'no orphan record may survive');
  assert.equal(calls.length, 0, 'nothing to unplant backend-side');
});
