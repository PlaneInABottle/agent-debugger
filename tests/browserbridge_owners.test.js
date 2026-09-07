// M5.1: Browser bridge in-file owners — SerialChain, ServerState.
//
// Every test drives the owners through the Session surface (or through the
// exact owner methods production routes through — never an isolated
// class-only path production bypasses). Covers: single-tab shape (no
// worker table, no swap/pause chain, no raw tail/counter/closing aliases),
// mutation-chain ordering + rejection safety, identical-add convergence
// through dispatch, and pool/close single-winner behavior. No
// WorkerRegistry/BreakpointStore/StopCoordinator exists (rejected — see
// browserbridge.js M5 owners head note); breakpoint/stop state stays
// Session-owned and is covered by the existing matrices.
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

const browser = loadBridge('bridge/browser/src/browserbridge.js',
  'Session, SerialChain, ServerState, MAX_ACTIVE_HANDLERS, BridgeErr');

function tmpdir(prefix) {
  return fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), prefix)));
}

function freshSession(dir) {
  const st = new browser.Session({
    kind: 'attach', dir, host: 'localhost', port: 9222, tab: null, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20,
  });
  st.exited = false;
  st.verifyTab = async () => {};
  st.cdp = { request: async () => ({}) };
  return st;
}

// ---- single-tab shape: owners exist, aliases do not -----------------------

test('owners: Session holds one chain + one server, no worker/raw aliases', () => {
  const st = freshSession(tmpdir('b5-own-shape-'));
  assert.ok(st._mutationChain instanceof browser.SerialChain);
  assert.ok(st.server instanceof browser.ServerState);
  // No worker state (unlike nodebridge), no raw tails/counters/flags:
  // every production/test write routes through the owner API.
  assert.equal(st.workers, undefined);
  assert.equal(st._mutationTail, undefined);
  assert.equal(st._swapChain, undefined);
  assert.equal(st._pauseChain, undefined);
  assert.equal(st.activeConns, undefined);
  assert.equal(st.closing, undefined);
  assert.equal(st.server.closing, false);
  assert.equal(st.server.active, 0);
  st._mutationChain.assertValid();
  st.server.assertValid(browser.MAX_ACTIVE_HANDLERS);
});

// ---- SerialChain: order, rejection safety ----------------------------------

test('owners: mutation chain serializes and survives rejections', async () => {
  const st = freshSession(tmpdir('b5-own-chain-'));
  const order = [];
  const slow = st._mutationRun(async () => {
    await new Promise((r) => setTimeout(r, 50));
    order.push('slow');
  });
  const fast = st._mutationRun(async () => { order.push('fast'); });
  await Promise.all([slow, fast]);
  assert.deepEqual(order, ['slow', 'fast']);
  await assert.rejects(st._mutationRun(async () => { throw new Error('boom'); }), /boom/);
  st._mutationChain.assertValid();
  // The chain still drains after a rejection.
  await st._mutationRun(async () => { order.push('after'); });
  assert.deepEqual(order, ['slow', 'fast', 'after']);
});

test('owners: identical concurrent adds converge through dispatch', async () => {
  const st = freshSession(tmpdir('b5-own-conv-'));
  let plants = 0;
  st.cdp = {
    request: async (method, params) => {
      if (method === 'Debugger.setBreakpointByUrl') {
        plants += 1;
        return { breakpointId: `bp-${plants}`, locations: [{ lineNumber: params.lineNumber }] };
      }
      return {};
    },
  };
  const req = { cmd: 'breaksAdd', breaks: ['app.js:3'] };
  const [r1, r2] = await Promise.all([st.dispatch(req), st.dispatch(req)]);
  assert.equal(r1.ok, true);
  assert.equal(r2.ok, true);
  // Exactly one plant; the loser rechecks under the chain and is idempotent.
  assert.equal(plants, 1);
  assert.equal(r1.added.length + r2.added.length, 1);
  assert.equal(st.stopStates.filter((s) => s.spec === 'app.js:3').length, 1);
  st._mutationChain.assertValid();
});

// ---- ServerState: pool bound + close single-winner ------------------------

test('owners: pool admits to the bound, then overloads', () => {
  const st = freshSession(tmpdir('b5-own-pool-'));
  for (let i = 0; i < browser.MAX_ACTIVE_HANDLERS; i++) {
    assert.equal(st.server.tryAcquire(browser.MAX_ACTIVE_HANDLERS), true);
  }
  assert.equal(st.server.tryAcquire(browser.MAX_ACTIVE_HANDLERS), false);
  st.server.release();
  assert.equal(st.server.tryAcquire(browser.MAX_ACTIVE_HANDLERS), true);
  for (let i = 0; i < browser.MAX_ACTIVE_HANDLERS; i++) st.server.release();
  st.server.assertValid(browser.MAX_ACTIVE_HANDLERS);
  assert.equal(st.server.active, 0);
});

test('owners: release without acquire fails loudly, pool unchanged', () => {
  const st = freshSession(tmpdir('b5-own-relneg-'));
  assert.throws(() => st.server.release(), /release without acquire/);
  assert.equal(st.server.active, 0);
  st.server.assertValid(browser.MAX_ACTIVE_HANDLERS);
  // Balanced acquire/release still works after the failed release.
  assert.equal(st.server.tryAcquire(browser.MAX_ACTIVE_HANDLERS), true);
  st.server.release();
  assert.equal(st.server.active, 0);
});

test('owners: close has exactly one winner through the Session surface', async () => {
  const st = freshSession(tmpdir('b5-own-close-'));
  let cleanups = 0;
  st.cleanup = async () => { cleanups += 1; };
  // First claim wins; the loser still reports closed but never tears down.
  assert.equal(st.server.claimClose(), true);
  assert.equal(st.server.claimClose(), false);
  await st.cleanup().catch(() => {});
  assert.equal(cleanups, 1);
  assert.equal(st.server.closing, true);
  await assert.rejects(st.dispatch({ cmd: 'threads' }), /session is closing/);
  st.server.assertValid(browser.MAX_ACTIVE_HANDLERS);
});
