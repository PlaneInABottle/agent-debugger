// Regression: every bounded wait must be measured on a MONOTONIC clock.
//
// Pre-fix, node/browser pumps and wait contexts computed deadlines and
// `waitedMs` from Date.now(). A backward wall-clock step (NTP correction,
// manual clock change, VM resume) therefore stretched a bounded wait by the
// step size — a `continue` bounded at 5s could run for hours — or reported
// a negative/oversized waitedMs. Epoch-style timestamps (updatedAt,
// observedAt) correctly stay on the wall clock.
//
// Deterministic: Date.now is faked to jump BACKWARD one hour between the
// deadline capture and the loop check, so a wall-clock implementation runs
// the full hour. The assertions below bound the real elapsed time, so the
// pre-fix code fails fast (its watchdog fires) instead of hanging a suite.
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

const node = loadBridge('bridge/node/src/nodebridge.js', 'Session, StopTimeout, writeOwner');
const browser = loadBridge('bridge/browser/src/browserbridge.js', 'Session, StopTimeout, writeOwner');

function tmpdir(prefix) {
  return fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), prefix)));
}

function nodeSession(dir) {
  // Mirror the real bridge main(): claim the session dir first, or the
  // pump's abandonment guard reads us as an orphaned dir and calls
  // process.exit(0) — which would kill the whole test runner process and
  // make the file "pass" vacuously with every subtest result lost.
  node.writeOwner(dir);
  const st = new node.Session({
    kind: 'attach', dir, host: 'localhost', port: 9229, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20, programArgs: [],
  });
  st.exited = false;
  st.cdp = { request: async () => ({}) };
  return st;
}

function browserSession(dir) {
  // Same ownership claim as above (see nodeSession).
  browser.writeOwner(dir);
  const st = new browser.Session({
    kind: 'attach', dir, host: 'localhost', port: 9222, tab: null, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20,
  });
  st.exited = false;
  st.verifyTab = async () => {};
  st.cdp = { request: async () => ({}) };
  return st;
}

// Backward wall-clock step MID-WAIT (the real NTP shape): Date.now stays
// truthful for a short grace covering synchronous setup (deadline capture),
// then jumps backward by jumpMs. A wall-clock deadline captured pre-step
// then reads as "hours remain" for the whole step size; a monotonic
// deadline ignores it. Time-based (not call-count-based): setup makes an
// unpredictable number of Date.now calls, but always within milliseconds.
function fakeClockStepBack(graceMs = 200, jumpMs = 3600 * 1000) {
  const real = Date.now;
  const realStart = real();
  Date.now = () => {
    const now = real();
    return (now - realStart > graceMs) ? now - jumpMs : now;
  };
  return () => { Date.now = real; };
}

async function boundedRun(fn, boundMs = 4000) {
  // Elapsed on hrtime: Date.now is mocked during the run, so only the
  // monotonic clock measures the real duration here.
  const t0 = Number(process.hrtime.bigint() / 1000000n);
  let out;
  let err;
  try {
    out = await fn();
  } catch (e) {
    err = e;
  }
  return { out, err, elapsed: Number(process.hrtime.bigint() / 1000000n) - t0, boundMs };
}

// Watchdog around a bounded wait under test: setTimeout runs on real
// time (immune to the Date.now mock below), so a wall-clock implementation
// FAILS here in ~15s instead of hanging the suite for the full step size.
// The loser's late rejection stays handled via the race (no unhandled
// rejection noise); the test's finally restores the clock, letting the
// orphaned pump observe its (wall) deadline and exit.
function withWatchdog(promise, ms, label) {
  let timer;
  const guard = new Promise((_, reject) => {
    timer = setTimeout(() => reject(
      new Error(`MONO-FAIL: ${label} ran past ${ms}ms — wall-clock deadline?`)), ms);
  });
  return Promise.race([promise, guard]).finally(() => clearTimeout(timer));
}

test('r3 node: pump timeout is immune to a backward wall-clock step', async () => {
  const dir = tmpdir('r3-node-mono-');
  const st = nodeSession(dir);
  // No park ever lands, no exit: the pump must time out on its own bound.
  const restore = fakeClockStepBack();
  try {
    const r = await boundedRun(() => withWatchdog(st.pump(1, true), 15000, 'node pump(1)'));
    restore();
    assert.ok(r.err instanceof node.StopTimeout, `want StopTimeout, got ${r.err}`);
    assert.ok(r.elapsed < r.boundMs + 3000,
      `pump must end near its 1s budget even after a -1h wall step (took ${r.elapsed}ms)`);
    const wc = r.err.waitContext;
    assert.equal(wc.triggerStatus, 'unknown');
    assert.ok(wc.waitedMs >= 0 && wc.waitedMs < 60000,
      `waitedMs must be a real duration, not a wall-clock artifact: ${wc.waitedMs}`);
  } finally {
    restore();
  }
});

test('r3 browser: pump timeout is immune to a backward wall-clock step', async () => {
  const dir = tmpdir('r3-browser-mono-');
  const st = browserSession(dir);
  const restore = fakeClockStepBack();
  try {
    const r = await boundedRun(() => withWatchdog(st.pump(1, true), 15000, 'browser pump(1)'));
    restore();
    assert.ok(r.err instanceof browser.StopTimeout, `want StopTimeout, got ${r.err}`);
    assert.ok(r.elapsed < r.boundMs + 3000,
      `pump must end near its 1s budget even after a -1h wall step (took ${r.elapsed}ms)`);
    const wc = r.err.waitContext;
    assert.equal(wc.triggerStatus, 'unknown');
    assert.ok(wc.waitedMs >= 0 && wc.waitedMs < 60000,
      `waitedMs must be a real duration, not a wall-clock artifact: ${wc.waitedMs}`);
  } finally {
    restore();
  }
});

test('r3 node: waitContext waitedMs tracks real elapsed time, not the clock', async () => {
  const dir = tmpdir('r3-node-wc-');
  const st = nodeSession(dir);
  // waitStartedAt is an epoch-style reported value (wall clock, must stay
  // a plausible epoch); waitedMs is a duration (monotonic).
  const epochNow = Math.floor(Date.now() / 1000);
  const restore = fakeClockStepBack(0, 3600 * 1000);
  let ctx;
  try {
    await new Promise((r) => setTimeout(r, 30));
    ctx = st.waitContext(5, { epoch: epochNow * 1000, mono: Number(process.hrtime.bigint() / 1000000n) - 30 });
  } finally {
    restore();
  }
  assert.ok(ctx.waitStartedAt >= epochNow - 2,
    `waitStartedAt must stay an epoch value: ${ctx.waitStartedAt}`);
  assert.ok(ctx.waitedMs >= 20 && ctx.waitedMs < 5000,
    `waitedMs must measure the real ~30ms wait: ${ctx.waitedMs}`);
});

test('r3 browser: waitContext waitedMs tracks real elapsed time, not the clock', async () => {
  const dir = tmpdir('r3-browser-wc-');
  const st = browserSession(dir);
  const epochNow = Math.floor(Date.now() / 1000);
  const restore = fakeClockStepBack(0, 3600 * 1000);
  let ctx;
  try {
    await new Promise((r) => setTimeout(r, 30));
    ctx = st.waitContext(5, { epoch: epochNow * 1000, mono: Number(process.hrtime.bigint() / 1000000n) - 30 });
  } finally {
    restore();
  }
  assert.ok(ctx.waitStartedAt >= epochNow - 2,
    `waitStartedAt must stay an epoch value: ${ctx.waitStartedAt}`);
  assert.ok(ctx.waitedMs >= 20 && ctx.waitedMs < 5000,
    `waitedMs must measure the real ~30ms wait: ${ctx.waitedMs}`);
});