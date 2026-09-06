const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const Module = require('node:module');

// Comprehensive-review fixes: browser secret-query redaction on every
// display/persist/error tab surface, leading-slash frag normalization,
// multi-location slide detection (Node main/worker + Browser), and the
// worker admission recheck. Loads the real bridge sources with only the
// trailing main() invocation stripped.
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

const browser = loadBridge('bridge/browser/src/browserbridge.js',
  'Session, parseBreak, parseLogpoint, fragRegex, normFrag, slidLine, ' +
  'redactUrl, pickTab, tabSummary, buildTabIdentity, isSecretQueryKey');
const node = loadBridge('bridge/node/src/nodebridge.js',
  'Session, slidLine');

function tmpdir(prefix) {
  return fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), prefix)));
}

function browserSession(dir, over = {}) {
  const st = new browser.Session({
    kind: 'attach', dir, host: 'localhost', port: 9222, tab: null, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20,
    ...over,
  });
  st.exited = false;
  st.verifyTab = async () => {};
  return st;
}

function nodeSession(dir, over = {}) {
  const st = new node.Session({
    kind: 'attach', dir, host: 'localhost', port: 9229, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20, programArgs: [],
    ...over,
  });
  st.exited = false;
  return st;
}

const SECRET_URL = 'http://h/app.js?token=SECRET123&x=1';
const SECRET_TITLE = 'dashboard ?auth=ZZZ9';

// ---- 1. browser secret-query leak -------------------------------------------

test('browser pickTab: ambiguous error redacts secrets, raw matching still works', () => {
  const pages = [
    { type: 'page', webSocketDebuggerUrl: 'ws://a', url: SECRET_URL, title: 'app one' },
    { type: 'page', webSocketDebuggerUrl: 'ws://b', url: 'http://h/app.js?x=2', title: 'app two' },
  ];
  // Raw matching sees the real URL/title (secret substring still matches).
  const hit = browser.pickTab(pages, 'app.js?token=SECRET123');
  assert.equal(hit.url, SECRET_URL);
  // Ambiguous listed tabs never carry the secret (this text lands in error.json).
  // NOTE: URL-form secrets encode as %5Bredacted%5D via URLSearchParams —
  // match /redacted/ either way; the point is the raw value is gone.
  assert.throws(() => browser.pickTab(pages, 'app'), (e) => {
    assert.ok(!e.message.includes('SECRET123'), 'no raw secret in error');
    assert.ok(e.message.includes('redacted'), 'redacted marker present');
    return true;
  });
});

test('browser pickTab: missing-selector error redacts secrets and caps length', () => {
  const pages = [
    {
      type: 'page', webSocketDebuggerUrl: 'ws://a',
      url: SECRET_URL, title: `${SECRET_TITLE} ${'y'.repeat(1000)}`,
    },
  ];
  assert.throws(() => browser.pickTab(pages, '?token=SECRET123&nomatch-xyz'), (e) => {
    assert.ok(!e.message.includes('SECRET123'), 'no raw secret in error');
    assert.ok(!e.message.includes('ZZZ9'), 'no raw title secret in error');
    assert.ok(e.message.length < 2000, `listed output capped (got ${e.message.length})`);
    return true;
  });
});

test('browser tabJson/threads: redacted + capped, long values truncated', () => {
  const dir = tmpdir('rf-tab-');
  const st = browserSession(dir);
  st.tab = { id: 'tab-1', title: `${SECRET_TITLE} ${'y'.repeat(1000)}`, url: SECRET_URL };
  const j = st.tabJson();
  assert.ok(!j.url.includes('SECRET123'), 'tab url redacted');
  assert.ok(j.url.includes('redacted'), 'redaction marker kept');
  assert.ok(!j.title.includes('ZZZ9'), 'tab title redacted');
  assert.ok(j.title.length <= 512, `title capped (got ${j.title.length})`);
  assert.ok(j.url.length <= 512, `url capped (got ${j.url.length})`);
  const threads = st.threadsJson();
  assert.ok(!threads[0].name.includes('ZZZ9'), 'thread name redacted');
  assert.ok(threads[0].name.length <= 60, `thread name truncated (got ${threads[0].name.length})`);
  const summary = browser.tabSummary(st.tab);
  assert.ok(!summary.includes('SECRET123') && !summary.includes('ZZZ9'), 'summary redacted');
  assert.ok(summary.length <= 60 + 100 + 8, `summary bounded (got ${summary.length})`);
});

test('browser cmdThreads: served tab identity carries no secret', async () => {
  const dir = tmpdir('rf-threads-');
  const st = browserSession(dir);
  st.tab = { id: 'tab-9', title: SECRET_TITLE, url: SECRET_URL };
  st.paused = null;
  const resp = await st.cmdThreads();
  assert.equal(resp.ok, true);
  assert.ok(!resp.threads[0].tab.url.includes('SECRET123'), 'served tab url redacted');
  assert.ok(!resp.threads[0].name.includes('ZZZ9'), 'served thread name redacted');
});

test('browser buildTabIdentity: persisted identity redacts id/url/title', () => {
  const obs = browser.buildTabIdentity(
    { id: 'id?token=SECRET123', title: SECRET_TITLE, url: SECRET_URL },
    'localhost', 9222, 1);
  for (const v of [obs.url, obs.title, obs.targetId]) {
    assert.ok(!String(v).includes('SECRET123') && !String(v).includes('ZZZ9'), 'no raw secret persists');
  }
  assert.ok(JSON.stringify(obs).length <= 2048, 'observed total cap holds');
});

test('browser isSecretQueryKey: short pass/pw redact like Rust, near-misses do not', () => {
  // Exact Rust token/suffix semantics: whole tokens or suffixes redact.
  for (const k of ['pass', 'pw', 'token', 'auth', 'pwd',
    'apiToken', 'db-pass', 'db_pw', 'myToken', 'session.auth']) {
    assert.equal(browser.isSecretQueryKey(k), true, `${k} redacts`);
  }
  // Mere prefixes never redact (Rust parity: --passage/--author/--passed).
  for (const k of ['passage', 'author', 'passed', 'authors']) {
    assert.equal(browser.isSecretQueryKey(k), false, `${k} keeps value`);
  }
});

test('browser short-key secrets redact across tabSummary/tabJson/error path', () => {
  const url = 'http://h/app.js?pass=HUNTER1&pw=HUNTER2&x=1';
  const title = 't ?db-pass=HUNTER3';
  const dir = tmpdir('rf-short-');
  const st = browserSession(dir);
  st.tab = { id: 'tab-s', title, url };
  const summary = browser.tabSummary(st.tab);
  assert.ok(!summary.includes('HUNTER1') && !summary.includes('HUNTER2') &&
    !summary.includes('HUNTER3'), 'tabSummary redacts short keys');
  assert.ok(summary.includes('redacted'), 'summary keeps marker');
  const j = st.tabJson();
  assert.ok(!j.url.includes('HUNTER1') && !j.url.includes('HUNTER2'), 'tabJson redacts');
  assert.ok(!j.title.includes('HUNTER3'), 'tabJson title redacts');
  // Near-miss values survive on every surface.
  const okUrl = 'http://h/app.js?passage=Story&author=Jane&passed=yes';
  const okSummary = browser.tabSummary({ title: 'ok', url: okUrl });
  assert.ok(okSummary.includes('Story') && okSummary.includes('Jane') &&
    okSummary.includes('yes'), 'near-miss values kept');
  assert.ok(browser.redactUrl(okUrl) === okUrl, 'redactUrl keeps near-misses');
  // Ambiguous-error path carries no short-key secret either.
  const pages = [
    { type: 'page', webSocketDebuggerUrl: 'ws://a', url, title: 'app one' },
    { type: 'page', webSocketDebuggerUrl: 'ws://b', url: 'http://h/app.js?x=2', title: 'app two' },
  ];
  assert.throws(() => browser.pickTab(pages, 'app'), (e) => {
    assert.ok(!e.message.includes('HUNTER1') && !e.message.includes('HUNTER2'),
      'error path redacts short keys');
    return true;
  });
});

// ---- 2. leading-slash frag normalization ------------------------------------

test('browser frag: leading slashes normalize, bare-only fails fast', () => {
  const cfg = { breaks: [], logpoints: [], wantExc: false };
  browser.parseBreak('/app.js:8', cfg);
  assert.equal(cfg.breaks[0].frag, 'app.js');
  const cfg2 = { breaks: [], logpoints: [], wantExc: false };
  browser.parseBreak('///deep/app.js:3|x>1', cfg2);
  assert.equal(cfg2.breaks[0].frag, 'deep/app.js');
  assert.equal(cfg2.breaks[0].cond, 'x>1');
  // Existing no-leading behavior preserved byte-for-byte.
  const cfg3 = { breaks: [], logpoints: [], wantExc: false };
  browser.parseBreak('app.js:8', cfg3);
  assert.equal(cfg3.breaks[0].frag, 'app.js');
  // A frag of only slashes can never match: fail fast, not a dead plant.
  assert.throws(() => browser.parseBreak('/:8', { breaks: [], logpoints: [], wantExc: false }),
    /frag:line/);
  const lcfg = { breaks: [], logpoints: [], wantExc: false };
  browser.parseLogpoint('/app.js:8:t={t}', lcfg);
  assert.equal(lcfg.logpoints[0].frag, 'app.js');
  assert.throws(() => browser.parseLogpoint('/:8:t', { breaks: [], logpoints: [], wantExc: false }),
    /frag:line:template/);
  // The normalized frag actually matches a real script URL (query ignored).
  const re = new RegExp(browser.fragRegex(browser.normFrag('/app.js')));
  assert.ok(re.test('http://h/app.js?v=3'), 'normalized frag matches');
  assert.ok(!re.test('http://h/myapp.js'), 'segment rule preserved');
});

test('browser lexBreak: leading-slash remove spec hits the stored key', async () => {
  const dir = tmpdir('rf-lex-');
  const st = browserSession(dir);
  st.cdp = { request: async () => ({ breakpointId: 'bp-1', locations: [{ lineNumber: 7 }] }) };
  const added = await st.cmdBreaksAdd({ breaks: ['app.js:8'] });
  assert.equal(added.added.length, 1);
  // Removal with a leading slash resolves to the same stored break.
  const removed = await st.cmdBreaksRemove({ breaks: ['/app.js:8'] });
  assert.equal(removed.removed.length, 1);
  assert.deepEqual(removed.missing || [], []);
});

// ---- 3. slide detector: multi-location --------------------------------------

test('browser armBreakpoints: multi-location with one leg home is verified, not slid', async () => {
  for (const [locations, want] of [
    [[{ lineNumber: 7 }, { lineNumber: 9 }], 'verified'], // one leg on line 8
    [[{ lineNumber: 9 }, { lineNumber: 10 }], 'slid'], // every leg moved
    [[{ lineNumber: 9 }], 'slid'], // single moved leg still slides
    [[], 'pending'], // no locations still pends
  ]) {
    const dir = tmpdir('rf-slide-b-');
    const st = browserSession(dir, { breaks: [{ frag: 'app.js', line: 8, cond: null }] });
    st.cdp = { request: async () => ({ breakpointId: `bp-${want}`, locations }) };
    await st.armBreakpoints();
    assert.equal(st.stopStates[0].state, want, `locations ${JSON.stringify(locations)}`);
    if (want === 'slid') {
      assert.match(st.stopStates[0].detail, /slid to line 10|slid to line 9/);
    }
  }
});

test('node armBreakpoints: multi-location with one leg home is verified, not slid', async () => {
  const prog = path.join(tmpdir('rf-slide-n-'), 'app.js');
  fs.writeFileSync(prog, Array.from({ length: 12 }, (_, i) => `const v${i}=${i};`).join('\n') + '\n');
  for (const [locations, want] of [
    [[{ lineNumber: 4 }, { lineNumber: 6 }], 'verified'],
    [[{ lineNumber: 6 }, { lineNumber: 7 }], 'slid'],
  ]) {
    const dir = tmpdir('rf-slide-n-');
    const st = nodeSession(dir, { breaks: [{ path: prog, line: 5, cond: null }] });
    st.cdp = { request: async () => ({ breakpointId: `bp-${want}-${Math.random()}`, locations }) };
    await st.armBreakpoints();
    assert.equal(st.stopStates[0].state, want, `locations ${JSON.stringify(locations)}`);
  }
});

test('node plantWorkerBreaks: multi-location with one leg home is verified', async () => {
  const dir = tmpdir('rf-slide-w-');
  const prog = path.join(dir, 'w.js');
  fs.writeFileSync(prog, 'const a = 1;\n'.repeat(12));
  const st = nodeSession(dir);
  const wid = 'worker:sw';
  const w = {
    id: wid, sessionId: 'sw', state: 'running', paused: null, stopInfo: null,
    lastStop: null, stopStates: [], targetRaws: new Map(), inheritedKeys: new Set(),
    breakKeys: new Map(), breakRecByKey: new Map(), logpoints: [],
    scripts: new Map(), urls: new Map(), seq: 0, stopSeq: 0,
    cachedLocals: [], lastChanged: '[]', lastTop: null, lastFunc: null,
    awaitingStep: false, exited: false,
    observed: { url: null, type: 'worker', endpoint: null },
  };
  st.workerTable.set(wid, w);
  st.workerOrder.push(wid);
  // One leg on line 7, one slid away: must read verified (was: slid).
  st.workerSend = async () => ({
    breakpointId: 'bp-multi', locations: [{ lineNumber: 6 }, { lineNumber: 9 }],
  });
  const { added } = await st.plantWorkerBreaks(w,
    [{ raw: `${prog}:7`, path: prog, line: 7, cond: null }]);
  assert.equal(added.length, 1);
  assert.equal(w.stopStates[0].state, 'verified');
  assert.equal(node.slidLine([{ lineNumber: 6 }, { lineNumber: 9 }], 7), null);
  assert.equal(node.slidLine([{ lineNumber: 8 }, { lineNumber: 9 }], 7), 9);
  assert.equal(node.slidLine([], 7), null);
});

// ---- 4. worker admission recheck --------------------------------------------

test('node acceptWorker: detach mid-admission stops the stale resume', async () => {
  const dir = tmpdir('rf-admit-');
  const st = new node.Session({
    kind: 'launch', dir, program: path.join(dir, 'main.js'),
    nodeBin: 'node', srcs: [], breaks: [{ path: '/tmp/admit-w.js', line: 5, cond: null }],
    logpoints: [], wantExc: false, timeout: 20, programArgs: [], workers: true,
  });
  st.exited = false;
  st.cdp = { request: async () => ({}) };
  const sent = [];
  st.workerSend = async (w, method) => {
    sent.push(method);
    if (method === 'Debugger.setBreakpointByUrl') {
      // Rival detach lands while the plant awaits are in flight.
      st.noteWorkerExit(w.id);
      return { breakpointId: 'bp-x', locations: [] };
    }
    return {};
  };
  await st.acceptWorker({ sessionId: 's9', workerInfo: { url: 'file:///w9.js', type: 'worker' } });
  assert.ok(!st.workerTable.has('worker:s9'), 'retired entry stays retired');
  assert.ok(st.exitedWorkers.some((e) => e.id === 'worker:s9' && e.state === 'exited'),
    'detach history preserved');
  assert.ok(!sent.includes('Debugger.resume'), 'no resume to a dead session');
  assert.ok(!sent.includes('Runtime.runIfWaitingForDebugger'), 'no run gate to a dead session');
});
