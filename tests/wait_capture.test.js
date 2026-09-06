const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const Module = require('node:module');

// wait/capture/diagnostics UX batch on the Node bridge (real Session,
// stubbed CDP transport, no timers in assertions).
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
  'Session, StopTimeout, Usage, BridgeErr, ConfigError, RuntimeError, setupFailurePayload');

function tmpdir(prefix) {
  return fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), prefix)));
}

function writeJs(dir, name = 'wc.js', lines = 12) {
  const file = path.join(dir, name);
  fs.writeFileSync(file,
    Array.from({ length: lines }, (_, i) => `const v${i} = ${i};`).join('\n') + '\n');
  return file;
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

function parkMain(st, file, line = 5) {
  const url = `file://${file}`;
  st.scripts = new Map([['s1', url]]);
  st.paused = {
    frames: [{
      functionName: 'handler', url,
      location: { scriptId: 's1', lineNumber: line - 1 },
      scopeChain: [],
    }],
    stopInfo: null,
  };
  st.cachedLocals = [];
  st.lastChanged = '[]';
  st.stopInfo = null;
  st.notePark('main', { reason: 'breakpoint', hitBreakpoints: [] },
    { file: st.relFile(file), line });
}

test('node captureBounds: frames/vars/budget/break enforced', () => {
  const st = nodeSession(tmpdir('wc-node-'));
  assert.deepEqual(st.captureBounds({}), { frames: 1, vars: 20, budget: 2000, spec: null });
  for (const req of [{ frames: 0 }, { frames: 11 }, { vars: 0 }, { vars: 21 },
    { pauseBudgetMs: 0 }, { pauseBudgetMs: 10001 }, { break: '' }, { break: 42 },
    { frames: 'x' }]) {
    assert.throws(() => st.captureBounds(req), /capture/);
  }
});

test('node wait: immediate parked success never resumes', async () => {
  const dir = tmpdir('wc-node-');
  const file = writeJs(dir);
  const st = nodeSession(dir);
  parkMain(st, file);
  const calls = [];
  st.cdp = { request: async (m) => { calls.push(m); return {}; } };
  const resp = await st.cmdWait({}, 5);
  assert.equal(resp.ok, true);
  assert.equal(resp.waited, false);
  assert.equal(resp.target, 'main');
  assert.ok(!calls.includes('Debugger.resume'));
  assert.match(resp.warning, /HTTP handler remains open/);
  assert.equal(resp.diag.target, 'main');
  assert.equal(resp.diag.reason, 'breakpoint');
  assert.equal(resp.diag.stoppingThread.id, 1);
  assert.ok(st.paused, 'still parked');
});

test('node wait: fresh park via pump issues no resume', async () => {
  const dir = tmpdir('wc-node-');
  const file = writeJs(dir);
  const st = nodeSession(dir);
  const calls = [];
  st.cdp = { request: async (m) => { calls.push(m); return {}; } };
  st.pump = async () => { parkMain(st, file, 7); return 'stopped'; };
  const resp = await st.cmdWait({}, 5);
  assert.equal(resp.waited, true);
  assert.ok(!calls.includes('Debugger.resume'));
  assert.equal(resp.snapshot.location.line, 7);
});

test('node wait: timeout is typed and preserves the session', async () => {
  const st = nodeSession(tmpdir('wc-node-'));
  st.pump = async () => { throw new Error('timeout: no stop within 7s'); };
  await assert.rejects(st.cmdWait({}, 7), /timeout: no stop within 7s/);
  assert.equal(st.paused, null);
});

test('node capture: prepark collects without resuming', async () => {
  const dir = tmpdir('wc-node-');
  const file = writeJs(dir);
  const st = nodeSession(dir);
  parkMain(st, file);
  const calls = [];
  st.cdp = { request: async (m) => { calls.push(m); return {}; } };
  const resp = await st.cmdCapture({ frames: 2, vars: 3 }, 5);
  assert.equal(resp.targetWasPaused, true);
  assert.equal(resp.resumed, false);
  assert.equal(resp.pauseDurationMs, 0);
  assert.ok(resp.snapshot.frames.length <= 2);
  assert.ok(!calls.includes('Debugger.resume'));
  assert.ok(!calls.includes('Debugger.setBreakpointByUrl'));
  assert.ok(st.paused, 'pre-existing park untouched');
});

test('node capture: fresh park removes ephemeral before resume', async () => {
  const dir = tmpdir('wc-node-');
  const file = writeJs(dir);
  const st = nodeSession(dir);
  const order = [];
  st.cdp = {
    request: async (m) => {
      order.push(m);
      if (m === 'Debugger.setBreakpointByUrl') {
        return { breakpointId: 'bp-1', locations: [{ lineNumber: 4 }] };
      }
      return {};
    },
  };
  st.pump = async () => { parkMain(st, file, 5); return 'stopped'; };
  const resp = await st.cmdCapture({ break: `${file}:5`, pauseBudgetMs: 2000 }, 5);
  assert.equal(resp.targetWasPaused, false);
  assert.equal(resp.resumed, true);
  assert.equal(resp.ephemeralPlanted, true);
  assert.ok(typeof resp.pauseDurationMs === 'number');
  assert.ok('budgetExceeded' in resp);
  const plant = order.indexOf('Debugger.setBreakpointByUrl');
  const unplant = order.lastIndexOf('Debugger.removeBreakpoint');
  const resume = order.indexOf('Debugger.resume');
  assert.ok(plant >= 0 && unplant > plant && resume > unplant,
    `remove-before-resume order: ${order.join(',')}`);
  assert.ok(!resp.removeError);
  assert.equal(st.paused, null, 'resumed');
  assert.equal(st.cfg.breaks.length, 0, 'no intent left behind');
});

test('node capture: timeout removes ephemeral without resume', async () => {
  const dir = tmpdir('wc-node-');
  const file = writeJs(dir);
  const st = nodeSession(dir);
  const order = [];
  st.cdp = {
    request: async (m) => {
      order.push(m);
      if (m === 'Debugger.setBreakpointByUrl') {
        return { breakpointId: 'bp-9', locations: [{ lineNumber: 4 }] };
      }
      return {};
    },
  };
  st.pump = async () => { throw new Error('timeout: no stop within 5s'); };
  await assert.rejects(st.cmdCapture({ break: `${file}:5` }, 5), /timeout/);
  assert.ok(!order.includes('Debugger.resume'));
  assert.ok(order.includes('Debugger.removeBreakpoint'), 'ephemeral removed');
  assert.equal(st.cfg.breaks.length, 0);
});

test('node capture: exit after armed names hit stage, never endpoint verdict', async () => {
  const dir = tmpdir('wc-node-');
  const file = writeJs(dir);
  const st = nodeSession(dir);
  const order = [];
  st.cdp = {
    request: async (m) => {
      order.push(m);
      if (m === 'Debugger.setBreakpointByUrl') {
        return { breakpointId: 'bp-1', locations: [{ lineNumber: 4 }] };
      }
      return {};
    },
  };
  st.pump = async () => { throw new node.BridgeErr('target exited'); };
  await assert.rejects(
    st.cmdCapture({ break: `${file}:5` }, 5),
    (e) => {
      assert.match(e.message, /target exited before capture hit/);
      assert.ok(e.message.includes(`${file}:5`));
      assert.ok(!/endpoint|unreachable|rejected/i.test(e.message));
      assert.equal(e.waitContext.captureStage, 'armed-wait');
      assert.equal(e.waitContext.ephemeralPlanted, true);
      assert.equal(e.waitContext.expectedBreak, `${file}:5`);
      return true;
    });
  assert.ok(order.includes('Debugger.removeBreakpoint'), 'ephemeral removed');
  assert.ok(!order.includes('Debugger.resume'), 'exit never resumes');
  assert.equal(st.cfg.breaks.length, 0, 'no intent left behind');
});

test('node capture: exit before armed at plant', async () => {
  const dir = tmpdir('wc-node-');
  const file = writeJs(dir);
  const st = nodeSession(dir);
  st.cdp = { request: async () => { throw new node.BridgeErr('target exited'); } };
  await assert.rejects(
    st.cmdCapture({ break: `${file}:5` }, 5),
    (e) => {
      assert.match(e.message,
        /capture target exited before ephemeral breakpoint was armed/);
      assert.ok(!/endpoint|rejected/i.test(e.message));
      assert.equal(e.waitContext.captureStage, 'before-armed');
      assert.equal(e.waitContext.ephemeralPlanted, false);
      assert.ok(typeof e.waitContext.waitStartedAt === 'number');
      assert.ok(typeof e.waitContext.waitedMs === 'number');
      return true;
    });
});

test('node capture: collection failure still resumes', async () => {
  const dir = tmpdir('wc-node-');
  const file = writeJs(dir);
  const st = nodeSession(dir);
  const order = [];
  st.cdp = { request: async (m) => { order.push(m); return {}; } };
  st.pump = async () => { parkMain(st, file, 5); return 'stopped'; };
  st.boundedSnapshot = async () => { throw new Error('boom'); };
  const resp = await st.cmdCapture({}, 5);
  assert.equal(resp.resumed, true);
  assert.ok(resp.snapshotError);
  assert.ok(order.includes('Debugger.resume'));
});

for (const [name, mkSession] of [['node', nodeSession], ['browser', browserSession]]) {
  const cdNum = (n, v) => ({ name: n, value: { type: 'number', value: v, description: String(v) } });
  const framesFor = (fn = 'handler') => [{ functionName: fn }];

  test(`${name} trackChanges: first baseline empty+unknown, skips malformed`, async () => {
    const st = mkSession(tmpdir('wc-tc-'));
    st.trackingProps = async () => ([
      cdNum('a', 1),
      { name: '…', value: { type: 'number', value: 0, description: '0' } },
      { name: 42, value: { type: 'number', value: 0, description: '0' } },
      null,
      'junk',
      { name: 'acc', get: { type: 'function' } },
      { name: 'n', value: null },
    ]);
    st.frameLocalsIn = async () => ([{ name: 'a', type: 'number', value: '1' }]);
    await st.trackChanges(framesFor());
    assert.deepEqual(JSON.parse(st.lastChanged), []);
    assert.deepEqual(JSON.parse(st.lastRemoved), []);
    assert.equal(st.lastChangedComplete, false);
    assert.equal(st.lastChangeTracking.reason, 'first-snapshot');
    assert.equal(st.lastChangeTracking.scanned, 3);
    assert.equal(st.lastChangeTracking.total, 3);
    assert.deepEqual(Object.keys(st.lastTop).sort(), ['a', 'acc', 'n']);
    assert.ok(!('…' in st.lastTop));
    assert.ok(st.changeFields().trackingWarning);
  });

  test(`${name} trackChanges: second stop detects change+removal, complete`, async () => {
    const st = mkSession(tmpdir('wc-tc-'));
    let props = [cdNum('a', 1), cdNum('gone', 9)];
    st.trackingProps = async () => props;
    st.frameLocalsIn = async () => [];
    await st.trackChanges(framesFor());
    assert.equal(st.lastChangedComplete, false);
    props = [cdNum('a', 2), cdNum('b', 3)];
    await st.trackChanges(framesFor());
    assert.deepEqual(JSON.parse(st.lastChanged), ['a', 'b']);
    assert.deepEqual(JSON.parse(st.lastRemoved), ['gone']);
    assert.equal(st.lastChangedComplete, true);
    assert.ok(!('reason' in st.lastChangeTracking));
    assert.ok(!('trackingWarning' in st.changeFields()));
    // No-op third stop: complete with empty changed (real no-change).
    await st.trackChanges(framesFor());
    assert.deepEqual(JSON.parse(st.lastChanged), []);
    assert.equal(st.lastChangedComplete, true);
  });

  test(`${name} trackChanges: outside display window still detected, complete`, async () => {
    const st = mkSession(tmpdir('wc-tc-'));
    let totalVal = 11;
    st.trackingProps = async () => ([
      ...Array.from({ length: 30 }, (_, k) => cdNum(`v${k}`, k)),
      cdNum('total', totalVal),
    ]);
    // Display stays capped at MAX_VARS + sentinel, independent of tracking.
    st.frameLocalsIn = async () => ([
      ...Array.from({ length: 20 }, (_, k) => ({ name: `v${k}`, type: 'number', value: `${k}` })),
      { name: '…', note: '+11 more' },
    ]);
    const frames = framesFor();
    await st.trackChanges(frames);
    assert.deepEqual(JSON.parse(st.lastChanged), []);
    totalVal = 12;
    await st.trackChanges(frames);
    assert.deepEqual(JSON.parse(st.lastChanged), ['total']);
    assert.equal(st.lastChangedComplete, true);
    assert.equal(st.lastChangeTracking.total, 31);
    assert.deepEqual(st.cachedLocals[st.cachedLocals.length - 1].name, '…');
  });

  test(`${name} trackChanges: over 256 truncated, intersection only`, async () => {
    const st = mkSession(tmpdir('wc-tc-'));
    let v000 = 0;
    st.trackingProps = async () => ([
      ...Array.from({ length: 300 }, (_, k) => cdNum(`v${String(k).padStart(3, '0')}`, k === 0 ? v000 : k)),
      cdNum('wobble', 0),
    ]);
    st.frameLocalsIn = async () => [];
    await st.trackChanges(framesFor());
    assert.equal(st.lastChangedComplete, false);
    assert.equal(st.lastChangeTracking.reason, 'truncated');
    assert.equal(st.lastChangeTracking.total, 301);
    assert.equal(st.lastChangeTracking.scanned, 256);
    assert.deepEqual(JSON.parse(st.lastChanged), []);
    // v000 sorts inside the scanned window: its change is reported, but
    // added/removed stay suppressed and tracking stays incomplete.
    v000 = 1;
    await st.trackChanges(framesFor());
    assert.deepEqual(JSON.parse(st.lastChanged), ['v000']);
    assert.deepEqual(JSON.parse(st.lastRemoved), []);
    assert.equal(st.lastChangedComplete, false);
    assert.equal(st.lastChangeTracking.reason, 'truncated');
  });

  test(`${name} trackChanges: function change resets baseline`, async () => {
    const st = mkSession(tmpdir('wc-tc-'));
    st.trackingProps = async () => ([cdNum('a', 1)]);
    st.frameLocalsIn = async () => [];
    await st.trackChanges(framesFor('handler'));
    st.trackingProps = async () => ([cdNum('a', 2)]);
    await st.trackChanges(framesFor('other'));
    assert.deepEqual(JSON.parse(st.lastChanged), []);
    assert.equal(st.lastChangedComplete, false);
    assert.equal(st.lastChangeTracking.reason, 'function-changed');
    await st.trackChanges(framesFor('other'));
    assert.deepEqual(JSON.parse(st.lastChanged), []);
    assert.equal(st.lastChangedComplete, true);
  });

  test(`${name} trackChanges: fetch error degrades class-only, park fields intact`, async () => {
    const st = mkSession(tmpdir('wc-tc-'));
    st.trackingProps = async () => { throw new TypeError('synthetic secret=zzz'); };
    st.frameLocalsIn = async () => [];
    await st.trackChanges(framesFor());
    assert.deepEqual(JSON.parse(st.lastChanged), []);
    assert.equal(st.lastChangedComplete, false);
    assert.equal(st.lastChangeTracking.reason, 'tracking-error');
    // Unknown scan: total null (not 0), scanned 0.
    assert.equal(st.lastChangeTracking.total, null);
    assert.equal(st.lastChangeTracking.scanned, 0);
    const warn = st.changeFields().trackingWarning;
    assert.ok(warn);
    assert.ok(!warn.includes('synthetic') && !warn.includes('zzz'));
    // Display cache cleared too (no stale previous-stop locals).
    assert.deepEqual(st.cachedLocals, []);
  });

  test(`${name} trackChanges: display failure clears cache, tracking still true`, async () => {
    const st = mkSession(tmpdir('wc-tc-'));
    let aval = 1;
    st.trackingProps = async () => ([cdNum('a', aval)]);
    st.frameLocalsIn = async () => ([{ name: 'a', type: 'number', value: String(aval) }]);
    const frames = framesFor();
    await st.trackChanges(frames);
    assert.equal(st.cachedLocals.length, 1);
    // Second stop at a new location: tracking fetch succeeds but the
    // display fetch fails — the cache must be empty, never the previous
    // stop's locals, while tracking still compares truthfully.
    st.frameLocalsIn = async () => { throw new Error('CDP display flake'); };
    aval = 2;
    await st.trackChanges(frames);
    assert.deepEqual(st.cachedLocals, []);
    assert.deepEqual(JSON.parse(st.lastChanged), ['a']);
    assert.equal(st.lastChangedComplete, true);
    assert.equal(st.lastChangeTracking.total, 1);
  });

  test(`${name} trackChanges: exactly one stderr warning per failure`, async () => {
    const st = mkSession(tmpdir('wc-tc-'));
    const writes = [];
    const orig = process.stderr.write;
    process.stderr.write = (s) => { writes.push(String(s)); return true; };
    try {
      st.trackingProps = async () => { throw new Error('boom'); };
      st.frameLocalsIn = async () => [];
      await st.trackChanges(framesFor());
      st.degradeTrack('Error', null, framesFor());
    } finally {
      process.stderr.write = orig;
    }
    const warns = writes.filter((s) => s.includes('warn: change tracking degraded'));
    assert.equal(warns.length, 2, JSON.stringify(writes));
    // lastTrackWarn matches the response's trackingWarning.
    assert.equal(st.changeFields().trackingWarning, st.lastTrackWarn);
  });

  test(`${name} trackChanges: same function name, different script is a change`, async () => {
    const st = mkSession(tmpdir('wc-tc-'));
    st.trackingProps = async () => ([cdNum('a', 1)]);
    st.frameLocalsIn = async () => [];
    const at = (scriptId) => ([{ functionName: 'handler', url: 'u', location: { scriptId } }]);
    await st.trackChanges(at('s1'));
    // Same functionName in another script: unknown, never a silent
    // cross-function comparison.
    st.trackingProps = async () => ([cdNum('a', 2)]);
    await st.trackChanges(at('s2'));
    assert.deepEqual(JSON.parse(st.lastChanged), []);
    assert.equal(st.lastChangedComplete, false);
    assert.equal(st.lastChangeTracking.reason, 'function-changed');
    // Same script again: compares truthfully.
    await st.trackChanges(at('s2'));
    assert.deepEqual(JSON.parse(st.lastChanged), []);
    assert.equal(st.lastChangedComplete, true);
  });
}

test('node onPaused degrades when change tracking throws, park stands', async () => {
  const dir = tmpdir('wc-node-');
  const file = writeJs(dir);
  const st = nodeSession(dir);
  const url = `file://${file}`;
  st.scripts = new Map([['s1', url]]);
  st.frameLocalsIn = async () => { throw new Error('synthetic locals boom'); };
  await st.onPaused({
    reason: 'breakpoint', hitBreakpoints: ['bp-1'],
    callFrames: [{
      functionName: 'handler', url,
      location: { scriptId: 's1', lineNumber: 4 }, scopeChain: [],
    }],
  });
  assert.ok(st.paused, 'park stands');
  assert.deepEqual(st.lastChanged, '[]');
  assert.deepEqual(st.cachedLocals, []);
  const sess = JSON.parse(fs.readFileSync(path.join(dir, 'session.json'), 'utf-8'));
  assert.equal(sess.stopped, true);
  assert.equal(sess.schemaVersion, 2);
});

test('node capture session-gone entry keeps exited message with stage', async () => {
  const st = nodeSession(tmpdir('wc-node-'));
  st.exited = true;
  await assert.rejects(st.cmdCapture({ break: 'x.js:1' }, 5), (e) => {
    assert.match(e.message, /exited/);
    assert.ok(!/no session/i.test(e.message));
    assert.equal(e.waitContext.captureStage, 'session-gone');
    assert.equal(e.waitContext.expectedBreak, 'x.js:1');
    assert.equal(e.waitContext.ephemeralPlanted, false);
    assert.equal(e.waitContext.triggerStatus, 'unknown');
    assert.ok(typeof e.waitContext.waitStartedAt === 'number');
    assert.ok(typeof e.waitContext.waitedMs === 'number');
    return true;
  });
});

test('node capture timeout carries armed-wait-timeout stage', async () => {
  const dir = tmpdir('wc-node-');
  const file = writeJs(dir);
  const st = nodeSession(dir);
  st.cdp = {
    request: async (m) => {
      if (m === 'Debugger.setBreakpointByUrl') {
        return { breakpointId: 'bp-9', locations: [{ lineNumber: 4 }] };
      }
      return {};
    },
  };
  st.pump = async () => {
    const e = new node.StopTimeout('timeout: no stop within 5s');
    e.waitContext = {
      waitStartedAt: 1, waitedMs: 2, triggerStatus: 'unknown',
      targetIdentity: null, note: 'n',
    };
    throw e;
  };
  await assert.rejects(st.cmdCapture({ break: `${file}:5` }, 5), (e) => {
    assert.equal(e.waitContext.captureStage, 'armed-wait-timeout');
    assert.equal(e.waitContext.ephemeralPlanted, true);
    assert.equal(e.waitContext.expectedBreak, `${file}:5`);
    return true;
  });
});

test('node capture removal failure on dead target keeps exit stage', async () => {
  const dir = tmpdir('wc-node-');
  const file = writeJs(dir);
  const st = nodeSession(dir);
  st.cdp = {
    request: async (m) => {
      if (m === 'Debugger.setBreakpointByUrl') {
        return { breakpointId: 'bp-7', locations: [{ lineNumber: 4 }] };
      }
      return {};
    },
  };
  st.pump = async () => { throw new node.BridgeErr('target exited'); };
  st.captureUnplant = async () => { throw new Error('remove boom'); };
  await assert.rejects(st.cmdCapture({ break: `${file}:5` }, 5), (e) => {
    assert.match(e.message, /target exited before capture hit/);
    assert.match(e.message, /remove boom/);
    assert.equal(e.waitContext.captureStage, 'armed-wait');
    assert.ok(typeof e.waitContext.waitStartedAt === 'number');
    assert.ok(typeof e.waitContext.waitedMs === 'number');
    return true;
  });
});

test('node setupFailurePayload maps the real setup catch', () => {
  // Typed failures keep their message; unexpected crashes sanitize to an
  // internal error with the runtime phase. Phase derives from the
  // original exception, never message text.
  assert.deepEqual(node.setupFailurePayload(new node.BridgeErr('target exited')),
    { schemaVersion: 2, error: 'target exited', phase: 'transport' });
  assert.deepEqual(node.setupFailurePayload(new node.Usage('bad --break')),
    { schemaVersion: 2, error: 'bad --break', phase: 'config' });
  assert.deepEqual(
    node.setupFailurePayload(new node.ConfigError('CDP refused: bad cond')),
    { schemaVersion: 2, error: 'CDP refused: bad cond', phase: 'config' });
  const p = node.setupFailurePayload(new Error('bug'));
  assert.equal(p.phase, 'runtime');
  assert.equal(p.schemaVersion, 2);
  assert.match(p.error, /^internal: Error: bug/);
});

test('node dispatch: wait occupies slot, rival resume busy', async () => {
  const st = nodeSession(tmpdir('wc-node-'));
  st.outstanding.set('main', 'wait');
  await assert.rejects(st.dispatch({ cmd: 'continue', timeout: 5 }), /busy/);
});

test('node notePark: same-line second park diagnoses, slide does not', () => {
  const st = nodeSession(tmpdir('wc-node-'));
  st.notePark('main', { reason: 'breakpoint', hitBreakpoints: [] }, { file: 'a.js', line: 5 });
  const first = { ...st.lastDiag };
  st.notePark('main', { reason: 'breakpoint', hitBreakpoints: [] }, { file: 'a.js', line: 5 });
  assert.equal(st.lastDiag.stopId, first.stopId + 1);
  assert.equal(st.lastDiag.sameLocation, true);
  assert.equal(st.lastDiag.sameThread, true);
  assert.ok(st.lastDiag.elapsedMs >= 0);
  st.notePark('main', { reason: 'breakpoint', hitBreakpoints: [] }, { file: 'a.js', line: 6 });
  assert.equal(st.lastDiag.sameLocation, false);
});

// ---- browser ----

const browser = loadBridge('bridge/browser/src/browserbridge.js',
  'Session, StopTimeout, Usage, BridgeErr, ConfigError, RuntimeError, setupFailurePayload');

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

function parkTab(st, url = 'http://localhost:3000/app.js', line = 5) {
  st.paused = {
    frames: [{
      functionName: 'handler', url,
      location: { scriptId: 's1', lineNumber: line - 1 },
      scopeChain: [],
    }],
    stopInfo: null,
  };
  st.cachedLocals = [];
  st.lastChanged = '[]';
  st.stopInfo = null;
  st.scriptLines = async () => [];
  st.notePark('main', { reason: 'breakpoint', hitBreakpoints: [] });
}

test('browser wait: immediate parked success never resumes', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  parkTab(st);
  const calls = [];
  st.cdp = { request: async (m) => { calls.push(m); return {}; } };
  const resp = await st.cmdWait({}, 5);
  assert.equal(resp.waited, false);
  assert.equal(resp.target, 'main');
  assert.ok(!calls.includes('Debugger.resume'));
  assert.match(resp.warning, /HTTP handler remains open/);
  assert.equal(resp.diag.stoppingThread.id, 1);
  assert.ok(st.paused);
});

test('browser wait: fresh park via pump issues no resume', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  const calls = [];
  st.cdp = { request: async (m) => { calls.push(m); return {}; } };
  st.pump = async () => { parkTab(st); return 'stopped'; };
  const resp = await st.cmdWait({}, 5);
  assert.equal(resp.waited, true);
  assert.ok(!calls.includes('Debugger.resume'));
});

test('browser capture: prepark collects without resuming', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  parkTab(st);
  const calls = [];
  st.cdp = { request: async (m) => { calls.push(m); return {}; } };
  const resp = await st.cmdCapture({ frames: 2, vars: 3 }, 5);
  assert.equal(resp.targetWasPaused, true);
  assert.equal(resp.resumed, false);
  assert.ok(!calls.includes('Debugger.resume'));
  assert.ok(st.paused);
});

test('browser capture: fresh park removes ephemeral before resume', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  const order = [];
  st.cdp = {
    request: async (m) => {
      order.push(m);
      if (m === 'Debugger.setBreakpointByUrl') {
        return { breakpointId: 'bp-1', locations: [{ lineNumber: 4 }] };
      }
      return {};
    },
  };
  st.pump = async () => { parkTab(st); return 'stopped'; };
  const resp = await st.cmdCapture({ break: 'app.js:5', pauseBudgetMs: 2000 }, 5);
  assert.equal(resp.resumed, true);
  assert.equal(resp.ephemeralPlanted, true);
  const plant = order.indexOf('Debugger.setBreakpointByUrl');
  const unplant = order.lastIndexOf('Debugger.removeBreakpoint');
  const resume = order.indexOf('Debugger.resume');
  assert.ok(plant >= 0 && unplant > plant && resume > unplant,
    `remove-before-resume order: ${order.join(',')}`);
  assert.equal(st.paused, null);
  assert.equal(st.cfg.breaks.length, 0);
});

test('browser capture: timeout removes ephemeral without resume', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  const order = [];
  st.cdp = {
    request: async (m) => {
      order.push(m);
      if (m === 'Debugger.setBreakpointByUrl') {
        return { breakpointId: 'bp-2', locations: [{ lineNumber: 4 }] };
      }
      return {};
    },
  };
  st.pump = async () => { throw new Error('timeout: no stop within 5s'); };
  await assert.rejects(st.cmdCapture({ break: 'app.js:5' }, 5), /timeout/);
  assert.ok(!order.includes('Debugger.resume'));
  assert.ok(order.includes('Debugger.removeBreakpoint'));
});

test('browser capture: reload between plant and park is a timeout, never stale resume', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  const order = [];
  st.cdp = {
    request: async (m) => {
      order.push(m);
      if (m === 'Debugger.setBreakpointByUrl') {
        return { breakpointId: 'bp-3', locations: [{ lineNumber: 4 }] };
      }
      return {};
    },
  };
  // Navigation dropped the ephemeral breakpoint: no park ever comes.
  st.pump = async () => { throw new Error('timeout: no stop within 5s'); };
  await assert.rejects(st.cmdCapture({ break: 'app.js:5' }, 5), /timeout/);
  assert.ok(!order.includes('Debugger.resume'), 'no resume without a park');
});

test('browser dispatch: capture occupies slot, rival mutation busy', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  st.outstanding.set('main', 'capture');
  await assert.rejects(st.dispatch({ cmd: 'breaksAdd', breaks: ['app.js:1'] }), /busy/);
});

test('browser captureBounds enforced', () => {
  const st = browserSession(tmpdir('wc-br-'));
  for (const req of [{ frames: 0 }, { vars: 21 }, { pauseBudgetMs: 10001 }, { break: '' }]) {
    assert.throws(() => st.captureBounds(req), /capture/);
  }
});

test('browser capture session-gone entry on closed tab keeps message with stage', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  st.verifyTab = async () => { throw new browser.BridgeErr('tab closed (or navigated beyond reach) — close this session'); };
  await assert.rejects(st.cmdCapture({ break: 'app.js:5' }, 5), (e) => {
    assert.match(e.message, /tab closed/);
    assert.ok(!/no session/i.test(e.message));
    assert.equal(e.waitContext.captureStage, 'session-gone');
    assert.equal(e.waitContext.expectedBreak, 'app.js:5');
    assert.equal(e.waitContext.ephemeralPlanted, false);
    assert.equal(e.waitContext.triggerStatus, 'unknown');
    assert.ok(typeof e.waitContext.waitStartedAt === 'number');
    assert.ok(typeof e.waitContext.waitedMs === 'number');
    return true;
  });
});

test('browser capture session-gone entry on exited flag keeps message with stage', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  st.exited = true;
  await assert.rejects(st.cmdCapture({ break: 'app.js:5' }, 5), (e) => {
    assert.match(e.message, /closed/);
    assert.equal(e.waitContext.captureStage, 'session-gone');
    assert.equal(e.waitContext.ephemeralPlanted, false);
    assert.ok(typeof e.waitContext.waitStartedAt === 'number');
    assert.ok(typeof e.waitContext.waitedMs === 'number');
    return true;
  });
});

test('browser capture: exit before armed at plant', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  st.cdp = { request: async () => { throw new browser.BridgeErr('tab closed'); } };
  await assert.rejects(st.cmdCapture({ break: 'app.js:5' }, 5), (e) => {
    assert.match(e.message, /capture target exited before ephemeral breakpoint was armed/);
    assert.ok(!/endpoint|rejected/i.test(e.message));
    assert.equal(e.waitContext.captureStage, 'before-armed');
    assert.equal(e.waitContext.ephemeralPlanted, false);
    assert.equal(e.waitContext.expectedBreak, 'app.js:5');
    assert.ok(typeof e.waitContext.waitStartedAt === 'number');
    assert.ok(typeof e.waitContext.waitedMs === 'number');
    return true;
  });
});

test('browser capture: reload before armed never claims exit', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  st.cdp = { request: async () => { throw new browser.BridgeErr('reload dropped execution contexts'); } };
  await assert.rejects(st.cmdCapture({ break: 'app.js:5' }, 5), (e) => {
    assert.match(e.message, /reloaded before ephemeral breakpoint was armed/);
    assert.ok(!/exited/i.test(e.message));
    assert.equal(e.waitContext.captureStage, 'before-armed');
    assert.equal(e.waitContext.ephemeralPlanted, false);
    return true;
  });
});

test('browser capture: exit after armed names hit stage, never endpoint verdict', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  const order = [];
  st.cdp = {
    request: async (m) => {
      order.push(m);
      if (m === 'Debugger.setBreakpointByUrl') {
        return { breakpointId: 'bp-1', locations: [{ lineNumber: 4 }] };
      }
      return {};
    },
  };
  st.pump = async () => { throw new browser.BridgeErr('tab closed'); };
  await assert.rejects(st.cmdCapture({ break: 'app.js:5' }, 5), (e) => {
    assert.match(e.message, /target exited before capture hit/);
    assert.ok(e.message.includes('app.js:5'));
    assert.ok(!/endpoint|unreachable|rejected/i.test(e.message));
    assert.equal(e.waitContext.captureStage, 'armed-wait');
    assert.equal(e.waitContext.ephemeralPlanted, true);
    assert.equal(e.waitContext.expectedBreak, 'app.js:5');
    assert.ok(typeof e.waitContext.waitStartedAt === 'number');
    assert.ok(typeof e.waitContext.waitedMs === 'number');
    return true;
  });
  assert.ok(order.includes('Debugger.removeBreakpoint'), 'ephemeral removed');
  assert.ok(!order.includes('Debugger.resume'), 'exit never resumes');
  assert.equal(st.cfg.breaks.length, 0, 'no intent left behind');
});

test('browser capture: reload mid-wait never claims exit', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  st.cdp = {
    request: async (m) => {
      if (m === 'Debugger.setBreakpointByUrl') {
        return { breakpointId: 'bp-9', locations: [{ lineNumber: 4 }] };
      }
      return {};
    },
  };
  st.pump = async () => { throw new browser.BridgeErr('reload navigated away'); };
  await assert.rejects(st.cmdCapture({ break: 'app.js:5' }, 5), (e) => {
    assert.match(e.message, /target reloaded before capture hit/);
    assert.ok(!/exited/i.test(e.message));
    assert.equal(e.waitContext.captureStage, 'armed-wait');
    assert.equal(e.waitContext.ephemeralPlanted, true);
    return true;
  });
});

test('browser capture timeout carries armed-wait-timeout stage', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  st.cdp = {
    request: async (m) => {
      if (m === 'Debugger.setBreakpointByUrl') {
        return { breakpointId: 'bp-9', locations: [{ lineNumber: 4 }] };
      }
      return {};
    },
  };
  st.pump = async () => {
    const e = new browser.StopTimeout('timeout: no stop within 5s');
    e.waitContext = {
      waitStartedAt: 1, waitedMs: 2, triggerStatus: 'unknown',
      targetIdentity: null, note: 'n',
    };
    throw e;
  };
  await assert.rejects(st.cmdCapture({ break: 'app.js:5' }, 5), (e) => {
    assert.equal(e.waitContext.captureStage, 'armed-wait-timeout');
    assert.equal(e.waitContext.ephemeralPlanted, true);
    assert.equal(e.waitContext.expectedBreak, 'app.js:5');
    return true;
  });
});

test('browser capture removal failure on dead tab keeps exit stage', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  st.cdp = {
    request: async (m) => {
      if (m === 'Debugger.setBreakpointByUrl') {
        return { breakpointId: 'bp-7', locations: [{ lineNumber: 4 }] };
      }
      return {};
    },
  };
  st.pump = async () => { throw new browser.BridgeErr('tab closed'); };
  st.captureUnplant = async () => { throw new Error('remove boom'); };
  await assert.rejects(st.cmdCapture({ break: 'app.js:5' }, 5), (e) => {
    assert.match(e.message, /target exited before capture hit/);
    assert.match(e.message, /remove boom/);
    assert.equal(e.waitContext.captureStage, 'armed-wait');
    assert.ok(typeof e.waitContext.waitStartedAt === 'number');
    assert.ok(typeof e.waitContext.waitedMs === 'number');
    return true;
  });
});

test('browser setupFailurePayload maps the real setup catch', () => {
  assert.deepEqual(browser.setupFailurePayload(new browser.BridgeErr('tab closed')),
    { schemaVersion: 2, error: 'tab closed', phase: 'transport' });
  assert.deepEqual(browser.setupFailurePayload(new browser.Usage('bad --break')),
    { schemaVersion: 2, error: 'bad --break', phase: 'config' });
  assert.deepEqual(
    browser.setupFailurePayload(new browser.ConfigError('CDP refused: bad cond')),
    { schemaVersion: 2, error: 'CDP refused: bad cond', phase: 'config' });
  const p = browser.setupFailurePayload(new Error('bug'));
  assert.equal(p.phase, 'runtime');
  assert.equal(p.schemaVersion, 2);
  assert.match(p.error, /^internal: Error: bug/);
});

// (Node/browser parity suite above covers trackChanges: first-baseline,
// change/removal, outside-window, truncation, function-change, and error
// paths for both bridges.)

test('browser onPaused degrades when change tracking throws, park stands', async () => {
  const st = browserSession(tmpdir('wc-br-'));
  st.frameLocalsIn = async () => { throw new Error('synthetic locals boom'); };
  st.cdp = { request: async () => ({}) };
  await st.onPaused({
    reason: 'breakpoint', hitBreakpoints: ['bp-1'],
    callFrames: [{
      functionName: 'handler',
      url: 'http://localhost:3000/app.js',
      location: { scriptId: 's1', lineNumber: 4 }, scopeChain: [],
    }],
  });
  assert.ok(st.paused, 'park stands');
  assert.deepEqual(st.lastChanged, '[]');
  assert.deepEqual(st.cachedLocals, []);
});

// Capture snapshots preserve the locals truncation sentinel within the
// vars cap (varsN-1 real + sentinel): the bounded frame still reports
// how many locals were cut instead of silently dropping the note.
for (const [name, mod, mkSession] of [['node', node, nodeSession], ['browser', browser, browserSession]]) {
  test(`${name} boundedSnapshot preserves truncation sentinel within vars cap`, async () => {
    const st = mkSession(tmpdir('wc-sentinel-'));
    const real = Array.from({ length: 20 }, (_, i) => ({ name: `v${i}`, type: 'int', value: `${i}` }));
    st.frameLocalsIn = async () => [...real, { name: '…', note: '+5 more' }];
    st.paused = {
      frames: [{
        functionName: 'handler', url: 'u',
        location: { scriptId: 's1', lineNumber: 4 }, scopeChain: [],
      }],
      stopInfo: null,
    };
    // Browser locationJson needs no traffic for file URLs; node needs a
    // script map entry.
    if (name === 'node') st.scripts = new Map([['s1', 'file:///wc.js']]);
    else st.scriptLines = async () => [];
    const { snapshot, varsTruncated } = await st.boundedSnapshot(10, 20);
    const locals = snapshot.frames[0].locals;
    assert.ok(locals.length <= 20, `within cap: ${locals.length}`);
    assert.equal(locals[locals.length - 1].name, '…');
    assert.equal(varsTruncated, true);
  });
}

test('node capture: one pump, one park — no second wait after the first stop', async () => {
  // A single fresh park must be collected, unplanted, and resumed; a
  // second pump would discard it and demand another stop (old code
  // pumped twice: the first park leaked, still suspended).
  const dir = tmpdir('wc-node-');
  const file = writeJs(dir);
  const st = nodeSession(dir);
  st.cdp = { request: async () => ({}) };
  let pumps = 0;
  st.pump = async () => {
    pumps += 1;
    parkMain(st, file, 5);
    return 'stopped';
  };
  const resp = await st.cmdCapture({}, 5);
  assert.equal(pumps, 1, 'exactly one pump call per capture');
  assert.equal(resp.target, 'main');
  assert.equal(resp.resumed, true);
  assert.equal(resp.targetWasPaused, false);
  assert.ok(!st.paused, 'the served park was resumed, not leaked');
});
