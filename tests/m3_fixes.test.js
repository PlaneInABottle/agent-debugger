const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const Module = require('node:module');

// Load the real bridge sources with only the trailing main() invocation
// stripped, so Session/parseArgs/resolver run for real against stubs.
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

const node = loadBridge('bridge/node/src/nodebridge.js',
  'Session, parseArgs, parseBreak, canon, resolveSourcePath, dedupeStartupBreaks');
const browser = loadBridge('bridge/browser/src/browserbridge.js',
  'Session, parseArgs, parseBreak, fragRegex, dedupeStartupBreaks');

function tmpdir(prefix) {
  return fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), prefix)));
}

function writeJs(dir, rel, lines = 12) {
  const file = path.join(dir, rel);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file,
    Array.from({ length: lines }, (_, i) => `const v${i} = ${i};`).join('\n') + '\n');
  return fs.realpathSync(file);
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

function sessionJson(dir) {
  return JSON.parse(fs.readFileSync(path.join(dir, 'session.json'), 'utf-8'));
}

// A minimal parked frame: file URL + line + function name.
function jsFrame(url, line = 5, fn = 'tick') {
  return {
    callFrameId: `cf-${Math.random()}`, functionName: fn,
    location: { scriptId: 's1', lineNumber: line - 1, columnNumber: 0 },
    url, scopeChain: [],
  };
}

// ---- 1. startup duplicate/condition conflict -------------------------------

test('node startup: identical repeats idempotent, diff-cond fails before CDP', async () => {
  const dir = tmpdir('m3-node-');
  const prog = writeJs(dir, 'app.js');
  const cfg = node.parseArgs(['session', '--kind', 'launch', '--dir', dir,
    '--program', prog, '--break', `${prog}:3`, '--break', `${prog}:3`]);
  assert.equal(cfg.breaks.length, 1);
  assert.throws(() => node.parseArgs(['session', '--kind', 'launch', '--dir', dir,
    '--program', prog, '--break', `${prog}:3`, '--break', `${prog}:3|x > 1`]),
  /conflicting condition/);
  // Same-line break + logpoint conflicts fail fast too (one V8 bp per line).
  assert.throws(() => node.parseArgs(['session', '--kind', 'launch', '--dir', dir,
    '--program', prog, '--break', `${prog}:3`, '--logpoint', `${prog}:3:t={t}`]),
  /conflicting condition/);
});

test('node armBreakpoints: conflict throws with zero CDP traffic', async () => {
  const dir = tmpdir('m3-node-');
  const prog = writeJs(dir, 'app.js');
  const st = nodeSession(dir, {
    breaks: [
      { path: prog, line: 3, cond: null },
      { path: prog, line: 3, cond: 'x > 1' },
    ],
  });
  let calls = 0;
  st.cdp = { request: async () => { calls += 1; return {}; } };
  await assert.rejects(st.armBreakpoints(), /conflicting condition/);
  assert.equal(calls, 0);
});

test('browser startup: identical idempotent, diff-cond and logpoint conflict', () => {
  const dir = tmpdir('m3-browser-');
  const cfg = browser.parseArgs(['session', '--dir', dir,
    '--break', 'app.js:8', '--break', 'app.js:8']);
  assert.equal(cfg.breaks.length, 1);
  const dup = { breaks: [{ frag: 'app.js', line: 8, cond: null }], logpoints: [] };
  browser.dedupeStartupBreaks(dup);
  assert.equal(dup.breaks.length, 1);
  assert.throws(() => browser.dedupeStartupBreaks({
    breaks: [{ frag: 'app.js', line: 8, cond: null }, { frag: 'app.js', line: 8, cond: 'x' }],
    logpoints: [],
  }), /conflicting condition/);
  assert.throws(() => browser.dedupeStartupBreaks({
    breaks: [{ frag: 'app.js', line: 8, cond: null }],
    logpoints: [{ frag: 'app.js', line: 8, template: 't' }],
  }), /conflicting condition/);
});

// ---- 2. Node --src resolution ------------------------------------------------

test('node resolver: cwd wins, nested joins --src, basename search, ambiguity, missing', () => {
  const dir = tmpdir('m3-node-');
  const root = path.join(dir, 'rootsrc');
  const nested = writeJs(dir, path.join('rootsrc', 'sub', 'deep.js'));
  const other = path.join(dir, 'other');
  writeJs(dir, path.join('other', 'deep.js'));
  // Absolute existing resolves canonical.
  assert.equal(node.resolveSourcePath(nested, []), nested);
  // Absolute missing fails fast.
  assert.throws(() => node.resolveSourcePath(path.join(dir, 'ghost.js'), []), /no such file/);
  // Bare basename with one root: unique search hit.
  assert.equal(node.resolveSourcePath('deep.js', [root]), nested);
  // Bare basename across two roots: ambiguous.
  assert.throws(() => node.resolveSourcePath('deep.js', [root, other]), /ambiguous breakpoint path/);
  // Bare basename, no roots: fail fast naming --src.
  assert.throws(() => node.resolveSourcePath('deep.js', []), /no --src roots given to search/);
  // Nested relative joins the explicit root.
  assert.equal(node.resolveSourcePath(path.join('sub', 'deep.js'), [root]), nested);
  // Nested relative with no root: tried-list failure.
  assert.throws(() => node.resolveSourcePath(path.join('sub', 'deep.js'), []), /tried/);
  // Symlinked dirs are pruned, canonical dedup collapses alias roots.
  const link = path.join(dir, 'linksrc');
  try {
    fs.symlinkSync(root, link, 'dir');
  } catch (_) { /* Windows CI: skip */ }
  if (fs.existsSync(link)) {
    assert.equal(node.resolveSourcePath('deep.js', [root, link]), nested);
  }
});

test('node parseArgs: --break before --src still resolves via the root', () => {
  const dir = tmpdir('m3-node-');
  const prog = writeJs(dir, 'app.js');
  const nested = writeJs(dir, path.join('rootsrc', 'sub', 'deep.js'));
  const cfg = node.parseArgs(['session', '--kind', 'launch', '--dir', dir,
    '--program', prog, '--break', 'deep.js:4', '--src', path.join(dir, 'rootsrc')]);
  assert.equal(cfg.breaks.length, 1);
  assert.equal(cfg.breaks[0].path, nested);
});

// ---- 3. onPaused: trackChanges failure still parks ---------------------------

test('node onPaused: trackChanges throw still parks and publishes stopped', async () => {
  const dir = tmpdir('m3-node-');
  const st = nodeSession(dir);
  st.frameLocalsIn = async () => { throw new Error('CDP flake'); };
  st.cdp = { request: async () => ({}) };
  const frames = [jsFrame('file:///app.js')];
  await st.onPaused({ callFrames: frames, hitBreakpoints: ['b1'], reason: 'other' });
  assert.ok(st.paused, 'must park despite trackChanges failure');
  assert.deepEqual(JSON.parse(st.lastChanged), []);
  assert.equal(sessionJson(dir).stopped, true);
});

test('browser onPaused: trackChanges throw still parks and publishes stopped', async () => {
  const dir = tmpdir('m3-browser-');
  const st = browserSession(dir);
  st.frameLocalsIn = async () => { throw new Error('CDP flake'); };
  st.cdp = { request: async () => ({}) };
  const frames = [jsFrame('http://h/app.js')];
  await st.onPaused({ callFrames: frames, hitBreakpoints: ['b1'], reason: 'other' });
  assert.ok(st.paused, 'must park despite trackChanges failure');
  assert.deepEqual(JSON.parse(st.lastChanged), []);
  assert.equal(sessionJson(dir).stopped, true);
});

// ---- 4. duplicate/co pause never clobbers the current park -------------------

test('node onPaused: concurrent duplicate pause keeps the first frames', async () => {
  const dir = tmpdir('m3-node-');
  const st = nodeSession(dir);
  let release;
  const gate = new Promise((r) => { release = r; });
  st.frameLocalsIn = async (frames) => {
    await gate;
    return [{ name: 'a', type: 'number', value: '1' }];
  };
  st.cdp = { request: async () => ({}) };
  const first = [jsFrame('file:///first.js', 5, 'one')];
  const second = [jsFrame('file:///second.js', 9, 'two')];
  const p1 = st.onPaused({ callFrames: first, hitBreakpoints: ['b1'], reason: 'other' });
  const p2 = st.onPaused({ callFrames: second, hitBreakpoints: ['b2'], reason: 'other' });
  release();
  await Promise.all([p1, p2]);
  assert.match(st.fileOf(st.paused.frames[0]), /first\.js/);
});

test('browser onPaused: duplicate pause keeps the first frames', async () => {
  const dir = tmpdir('m3-browser-');
  const st = browserSession(dir);
  let release;
  const gate = new Promise((r) => { release = r; });
  st.frameLocalsIn = async () => { await gate; return []; };
  st.cdp = { request: async () => ({}) };
  const first = [jsFrame('http://h/first.js', 5, 'one')];
  const second = [jsFrame('http://h/second.js', 9, 'two')];
  const p1 = st.onPaused({ callFrames: first, hitBreakpoints: ['b1'], reason: 'other' });
  const p2 = st.onPaused({ callFrames: second, hitBreakpoints: ['b2'], reason: 'other' });
  release();
  await Promise.all([p1, p2]);
  assert.equal(st.frameUrl(st.paused.frames[0]), 'http://h/first.js');
});

// ---- 5. awaitingStep resets on pump timeout, never clears a fresh pause ------

test('node resumeAndWait: pump timeout clears awaitingStep, keeps a fresh pause', async () => {
  const dir = tmpdir('m3-node-');
  const st = nodeSession(dir);
  st.awaitingStep = true;
  st.pump = async () => { throw Object.assign(new Error('timeout: no stop within 1'), { name: 'StopTimeout' }); };
  await assert.rejects(st.resumeAndWait(1), /timeout/);
  assert.equal(st.awaitingStep, false);
  // A newly landed pause is never cleared by the failure path.
  const st2 = nodeSession(dir);
  st2.awaitingStep = true;
  st2.paused = { frames: [jsFrame('file:///x.js')], stopInfo: null };
  st2.pump = async () => { throw new Error('timeout: no stop within 1'); };
  await assert.rejects(st2.resumeAndWait(1), /timeout/);
  assert.ok(st2.paused, 'fresh pause must survive pump failure');
});

test('browser resumeAndWait: pump timeout clears awaitingStep, keeps pause', async () => {
  const dir = tmpdir('m3-browser-');
  const st = browserSession(dir);
  st.awaitingStep = true;
  st.pump = async () => { throw new Error('timeout: no stop within 1'); };
  await assert.rejects(st.resumeAndWait(1), /timeout/);
  assert.equal(st.awaitingStep, false);
});

// ---- 6. resume publish ordering + request-failure restore --------------------

test('node cmdContinue: publishes running before resume, restores on failure', async () => {
  const dir = tmpdir('m3-node-');
  const st = nodeSession(dir);
  st.paused = { frames: [jsFrame('file:///app.js')], stopInfo: null };
  let atCall = null;
  st.cdp = {
    request: async () => {
      atCall = sessionJson(dir).stopped;
      return {};
    },
  };
  st.pump = async () => 'stopped';
  await st.cmdContinue({}, 5);
  assert.equal(atCall, false, 'session.json must show running BEFORE the resume request');
  // Synchronous request failure: park restored, file truthful again.
  const dir2 = tmpdir('m3-node-');
  const st2 = nodeSession(dir2);
  st2.paused = { frames: [jsFrame('file:///app.js')], stopInfo: null };
  st2.cdp = { request: async () => { throw new Error('CDP gone'); } };
  await assert.rejects(st2.cmdContinue({}, 5), /CDP gone/);
  assert.ok(st2.paused, 'park must be restored after resume failure');
  assert.equal(sessionJson(dir2).stopped, true);
});

test('node cmdStep: publishes running before step, restores on failure', async () => {
  const dir = tmpdir('m3-node-');
  const st = nodeSession(dir);
  st.paused = { frames: [jsFrame('file:///app.js')], stopInfo: null };
  let atCall = null;
  st.cdp = {
    request: async () => {
      atCall = sessionJson(dir).stopped;
      return {};
    },
  };
  st.pump = async () => 'stopped';
  await st.cmdStep({ mode: 'over' }, 5);
  assert.equal(atCall, false, 'session.json must show running BEFORE the step request');
  const dir2 = tmpdir('m3-node-');
  const st2 = nodeSession(dir2);
  st2.paused = { frames: [jsFrame('file:///app.js')], stopInfo: null };
  st2.cdp = { request: async () => { throw new Error('CDP gone'); } };
  await assert.rejects(st2.cmdStep({ mode: 'over' }, 5), /CDP gone/);
  assert.ok(st2.paused, 'park must be restored after step failure');
  assert.equal(st2.awaitingStep, false);
  assert.equal(sessionJson(dir2).stopped, true);
});

test('browser cmdContinue: publishes running before resume, restores on failure', async () => {
  const dir = tmpdir('m3-browser-');
  const st = browserSession(dir);
  st.paused = { frames: [jsFrame('http://h/app.js')], stopInfo: null };
  let atCall = null;
  st.cdp = {
    request: async () => {
      atCall = sessionJson(dir).stopped;
      return {};
    },
  };
  st.pump = async () => 'stopped';
  // stub snapshot chain (locationJson hits CDP for source)
  st.snapshot = async () => ({ mode: 'session' });
  await st.cmdContinue({}, 5);
  assert.equal(atCall, false);
  const dir2 = tmpdir('m3-browser-');
  const st2 = browserSession(dir2);
  st2.paused = { frames: [jsFrame('http://h/app.js')], stopInfo: null };
  st2.cdp = { request: async () => { throw new Error('CDP gone'); } };
  await assert.rejects(st2.cmdContinue({}, 5), /CDP gone/);
  assert.ok(st2.paused);
  assert.equal(sessionJson(dir2).stopped, true);
});

// ---- 7. scope allowlist: catch + script --------------------------------------

test('node frameLocalsIn: catch and script scopes merge inner-first', async () => {
  const dir = tmpdir('m3-node-');
  const st = nodeSession(dir);
  st.scopeProps = async (scope) => {
    if (scope.object.objectId === 'catch-o') {
      return [{ name: 'e', value: { type: 'object', description: 'Error' } }];
    }
    if (scope.object.objectId === 'script-o') {
      return [{ name: 'top', value: { type: 'number', value: 3 } }];
    }
    if (scope.object.objectId === 'local-o') {
      return [
        { name: 'e', value: { type: 'string', value: 'shadowed' } },
        { name: 'x', value: { type: 'number', value: 1 } },
      ];
    }
    return [];
  };
  const frames = [{
    scopeChain: [
      { type: 'catch', object: { objectId: 'catch-o' } },
      { type: 'local', object: { objectId: 'local-o' } },
      { type: 'script', object: { objectId: 'script-o' } },
    ],
  }];
  const locals = await st.frameLocalsIn(frames, 0);
  const names = locals.map((l) => l.name);
  assert.ok(names.includes('e'), 'catch binding visible');
  assert.ok(names.includes('top'), 'script binding visible');
  assert.ok(names.includes('x'), 'local binding visible');
  assert.equal(locals.find((l) => l.name === 'e').value, 'Error',
    'innermost (catch) wins over the shadowed local');
});

test('browser frameLocalsIn: catch and script scopes merge', async () => {
  const dir = tmpdir('m3-browser-');
  const st = browserSession(dir);
  st.scopeProps = async (scope) => {
    if (scope.object.objectId === 'catch-o') {
      return [{ name: 'e', value: { type: 'string', value: 'boom' } }];
    }
    if (scope.object.objectId === 'script-o') {
      return [{ name: 'top', value: { type: 'number', value: 3 } }];
    }
    return [];
  };
  const frames = [{
    scopeChain: [
      { type: 'catch', object: { objectId: 'catch-o' } },
      { type: 'script', object: { objectId: 'script-o' } },
    ],
  }];
  const locals = await st.frameLocalsIn(frames, 0);
  assert.deepEqual(locals.map((l) => l.name).sort(), ['e', 'top']);
});

// ---- 8. child error lifecycle + bad --node -----------------------------------

test('node startTarget: bad --node binary fails fast with a clean error', async () => {
  const dir = tmpdir('m3-node-');
  const prog = writeJs(dir, 'app.js');
  const st = nodeSession(dir, { program: prog, nodeBin: '/nonexistent-node-xyz-123', programArgs: [] });
  const started = Date.now();
  await assert.rejects(st.startTarget(), /cannot run \/nonexistent-node-xyz-123/);
  assert.ok(Date.now() - started < 15000, 'must fail fast, not at the 20s deadline');
  assert.ok(st.spawnError, 'spawn error captured on the session');
});

test('node parseArgs: bad --node with a .ts program fails cleanly', () => {
  const dir = tmpdir('m3-node-');
  const prog = path.join(dir, 'app.ts');
  fs.writeFileSync(prog, 'const x: number = 1;\nconsole.log(x);\n');
  assert.throws(() => node.parseArgs(['session', '--kind', 'launch', '--dir', dir,
    '--program', prog, '--node', '/nonexistent-node-xyz-123']),
  /cannot run \/nonexistent-node-xyz-123/);
});
