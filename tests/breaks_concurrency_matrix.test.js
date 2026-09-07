// Milestone A: serialized breakpoint-mutation concurrency matrix (Node/Browser).
//
// Every breaks mutation (add/remove/clear) runs on _mutationRun, so two
// handlers racing conflicting operations converge: backend CDP calls never
// overlap and bridge intent + backend + records agree afterwards (no
// ghost plant, no resurrected removal, no duplicate records).
//
// Each case starts both ops on a start barrier so they genuinely race into
// dispatch, with a stub CDP backend that tracks concurrent entries (max
// must stay 1) and serves as the backend-truth oracle.
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

const node = loadBridge('bridge/node/src/nodebridge.js', 'Session, parseBreak, canon');
const browser = loadBridge('bridge/browser/src/browserbridge.js', 'Session, parseBreak, fragRegex');

function tmpdir(prefix) {
  return fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), prefix)));
}

function writeJs(dir, name, lines = 12) {
  const file = path.join(dir, name);
  fs.writeFileSync(file,
    Array.from({ length: lines }, (_, i) => `const v${i} = ${i};`).join('\n') + '\n');
  return file;
}

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

// Release both racers at once so they genuinely contend on _mutationRun.
function startBarrier(n) {
  let count = 0;
  const waiters = [];
  return () => new Promise((resolve) => {
    count += 1;
    if (count >= n) {
      const ws = waiters.splice(0);
      resolve();
      ws.forEach((f) => f());
    } else {
      waiters.push(resolve);
    }
  });
}

// Stub CDP backend: per-breakpoint plants/removals with concurrency depth
// tracking (serialized mutations must never overlap) plus a live-id oracle.
function stubBackend(st) {
  const rec = {
    cur: 0, max: 0, calls: [], live: new Map(), seq: 0,
  };
  st.cdp = {
    request: async (method, params) => {
      rec.cur += 1;
      rec.max = Math.max(rec.max, rec.cur);
      rec.calls.push(method);
      try {
        await sleep(20); // widen the check-then-mutate race window
        if (method === 'Debugger.setBreakpointByUrl') {
          const id = `bp-${++rec.seq}`;
          rec.live.set(id, params.lineNumber);
          return { breakpointId: id, locations: [{ lineNumber: params.lineNumber }] };
        }
        if (method === 'Debugger.removeBreakpoint') {
          rec.live.delete(params.breakpointId);
          return {};
        }
        return {};
      } finally {
        rec.cur -= 1;
      }
    },
  };
  return rec;
}

async function runPair(st, reqA, reqB) {
  const start = startBarrier(2);
  const run = async (req) => {
    await start();
    return st.dispatch({ timeout: 20, ...req });
  };
  const out = await Promise.all([run(reqA), run(reqB)]);
  for (const r of out) assert.equal(r.ok, true);
  return out;
}

// Lightweight bookkeeping: cfg intent, stored raws, key maps and stop
// records agree exactly, and the backend holds exactly the live plants.
function assertNodeCoherent(st, rec) {
  const cfgKeys = new Set(st.cfg.breaks.map((b) => `${b.path}:${b.line}|${b.cond || ''}`));
  assert.deepEqual(new Set(st.breakRaws.keys()), cfgKeys);
  assert.deepEqual(new Set(st.breakKeys.keys()), cfgKeys);
  for (const [k, id] of st.breakKeys) {
    assert.ok(rec.live.has(id), `backend dropped live break ${k}`);
  }
  assert.equal(rec.live.size, st.breakKeys.size);
  assert.equal(st.breakIdToRec.size, rec.live.size);
  assert.equal(st.stopStates.length, st.breakKeys.size);
  assert.equal(rec.max, 1, 'backend CDP calls must never overlap');
}

function assertBrowserCoherent(st, rec) {
  const cfgKeys = new Set(st.cfg.breaks.map((b) => `${b.frag}:${b.line}|${b.cond || ''}`));
  assert.deepEqual(new Set(st.breakRaws.keys()), cfgKeys);
  assert.deepEqual(new Set(st.breakKeys.keys()), cfgKeys);
  for (const [k, id] of st.breakKeys) {
    assert.ok(rec.live.has(id), `backend dropped live break ${k}`);
  }
  assert.equal(rec.live.size, st.breakKeys.size);
  assert.equal(st.breakIdToRec.size, rec.live.size);
  assert.equal(st.stopStates.length, st.breakKeys.size);
  assert.equal(rec.max, 1, 'backend CDP calls must never overlap');
}

function nodeSession(dir) {
  const st = new node.Session({
    kind: 'attach', dir, host: 'localhost', port: 9229, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20, programArgs: [],
  });
  st.exited = false;
  return st;
}

function browserSession(dir) {
  const st = new browser.Session({
    kind: 'attach', dir, host: 'localhost', port: 9222, tab: null, srcs: [],
    breaks: [], logpoints: [], wantExc: false, timeout: 20,
  });
  st.exited = false;
  st.verifyTab = async () => {};
  return st;
}

// Seed through the real add path (records stay consistent), then reset the
// concurrency oracle while keeping backend truth.
async function seedNode(st, rec, raws) {
  if (raws.length === 0) return;
  const resp = await st.dispatch({ cmd: 'breaksAdd', breaks: raws });
  assert.equal(resp.ok, true);
  rec.max = 0;
  rec.calls = [];
}

async function seedBrowser(st, rec, raws) {
  if (raws.length === 0) return;
  const resp = await st.dispatch({ cmd: 'breaksAdd', breaks: raws });
  assert.equal(resp.ok, true);
  rec.max = 0;
  rec.calls = [];
}

// ---- node matrix ----

test('node matrix: add×add same file converges to both lines', async () => {
  const dir = tmpdir('mx-node-');
  const st = nodeSession(dir);
  const rec = stubBackend(st);
  const a = writeJs(dir, 'a.js');
  const out = await runPair(st,
    { cmd: 'breaksAdd', breaks: [`${a}:5`] },
    { cmd: 'breaksAdd', breaks: [`${a}:6`] });
  assert.equal(out.reduce((n, r) => n + r.added.length, 0), 2);
  assertNodeCoherent(st, rec);
  assert.equal(st.cfg.breaks.length, 2);
});

test('node matrix: add×remove same file leaves the added line only', async () => {
  const dir = tmpdir('mx-node-');
  const st = nodeSession(dir);
  const rec = stubBackend(st);
  const a = writeJs(dir, 'a.js');
  await seedNode(st, rec, [`${a}:5`]);
  const out = await runPair(st,
    { cmd: 'breaksAdd', breaks: [`${a}:6`] },
    { cmd: 'breaksRemove', breaks: [`${a}:5`] });
  assert.deepEqual(out.flatMap((r) => (r.added || []).map((e) => e.raw)), [`${a}:6`]);
  assert.deepEqual(out.flatMap((r) => (r.removed || []).map((e) => e.raw)), [`${a}:5`]);
  assertNodeCoherent(st, rec);
  assert.deepEqual(st.cfg.breaks.map((b) => b.line), [6]);
});

test('node matrix: add×clear converges to empty or the added line', async () => {
  const dir = tmpdir('mx-node-');
  const st = nodeSession(dir);
  const rec = stubBackend(st);
  const a = writeJs(dir, 'a.js');
  await seedNode(st, rec, [`${a}:5`]);
  const out = await runPair(st,
    { cmd: 'breaksAdd', breaks: [`${a}:6`] },
    { cmd: 'breaksClear' });
  assertNodeCoherent(st, rec);
  const lines = st.cfg.breaks.map((b) => b.line).sort();
  assert.ok(lines.length === 0 || (lines.length === 1 && lines[0] === 6),
    `unexpected survivor: ${JSON.stringify(lines)}`);
  const added = out.flatMap((r) => (r.added || []).map((e) => e.raw));
  assert.deepEqual(added, [`${a}:6`]);
  const removed = out.flatMap((r) => (r.removed || []).map((e) => e.raw));
  assert.ok(removed.includes(`${a}:5`), 'the armed line is always removed');
  assert.equal(removed.includes(`${a}:6`), lines.length === 0);
});

test('node matrix: remove×remove same key has exactly one winner', async () => {
  const dir = tmpdir('mx-node-');
  const st = nodeSession(dir);
  const rec = stubBackend(st);
  const a = writeJs(dir, 'a.js');
  await seedNode(st, rec, [`${a}:5`]);
  const out = await runPair(st,
    { cmd: 'breaksRemove', breaks: [`${a}:5`] },
    { cmd: 'breaksRemove', breaks: [`${a}:5`] });
  const winners = out.filter((r) => (r.removed || []).length === 1);
  const losers = out.filter((r) => (r.removed || []).length === 0);
  assert.equal(winners.length, 1);
  assert.equal(losers.length, 1);
  assert.deepEqual(losers[0].missing, [`${a}:5`]);
  assertNodeCoherent(st, rec);
  assert.deepEqual(st.cfg.breaks, []);
});

test('node matrix: remove×clear removes every armed line exactly once', async () => {
  const dir = tmpdir('mx-node-');
  const st = nodeSession(dir);
  const rec = stubBackend(st);
  const a = writeJs(dir, 'a.js');
  await seedNode(st, rec, [`${a}:5`, `${a}:8`]);
  const out = await runPair(st,
    { cmd: 'breaksRemove', breaks: [`${a}:5`] },
    { cmd: 'breaksClear' });
  assertNodeCoherent(st, rec);
  assert.deepEqual(st.cfg.breaks, []);
  const removed = out.flatMap((r) => (r.removed || []).map((e) => e.raw)).sort();
  assert.deepEqual(removed, [`${a}:5`, `${a}:8`].sort());
});

test('node matrix: clear×add converges to empty or the added line', async () => {
  const dir = tmpdir('mx-node-');
  const st = nodeSession(dir);
  const rec = stubBackend(st);
  const a = writeJs(dir, 'a.js');
  await seedNode(st, rec, [`${a}:5`]);
  const out = await runPair(st,
    { cmd: 'breaksClear' },
    { cmd: 'breaksAdd', breaks: [`${a}:6`] });
  assertNodeCoherent(st, rec);
  const lines = st.cfg.breaks.map((b) => b.line).sort();
  assert.ok(lines.length === 0 || (lines.length === 1 && lines[0] === 6),
    `unexpected survivor: ${JSON.stringify(lines)}`);
  const added = out.flatMap((r) => (r.added || []).map((e) => e.raw));
  assert.deepEqual(added, [`${a}:6`]);
});

test('node matrix: add×add different files never cross-corrupt', async () => {
  const dir = tmpdir('mx-node-');
  const st = nodeSession(dir);
  const rec = stubBackend(st);
  const a = writeJs(dir, 'a.js');
  const b = writeJs(dir, 'b.js');
  const out = await runPair(st,
    { cmd: 'breaksAdd', breaks: [`${a}:5`] },
    { cmd: 'breaksAdd', breaks: [`${b}:6`] });
  assert.equal(out.reduce((n, r) => n + r.added.length, 0), 2);
  assertNodeCoherent(st, rec);
  assert.deepEqual(
    st.cfg.breaks.map((x) => `${path.basename(x.path)}:${x.line}`).sort(),
    ['a.js:5', 'b.js:6']);
});

// ---- browser matrix (frag-keyed, single target) ----

test('browser matrix: add×add same frag converges to both lines', async () => {
  const dir = tmpdir('mx-br-');
  const st = browserSession(dir);
  const rec = stubBackend(st);
  const out = await runPair(st,
    { cmd: 'breaksAdd', breaks: ['app.js:8'] },
    { cmd: 'breaksAdd', breaks: ['app.js:10'] });
  assert.equal(out.reduce((n, r) => n + r.added.length, 0), 2);
  assertBrowserCoherent(st, rec);
  assert.equal(st.cfg.breaks.length, 2);
});

test('browser matrix: add×remove same frag leaves the added line only', async () => {
  const dir = tmpdir('mx-br-');
  const st = browserSession(dir);
  const rec = stubBackend(st);
  await seedBrowser(st, rec, ['app.js:8']);
  const out = await runPair(st,
    { cmd: 'breaksAdd', breaks: ['app.js:10'] },
    { cmd: 'breaksRemove', breaks: ['app.js:8'] });
  assert.deepEqual(out.flatMap((r) => (r.added || []).map((e) => e.raw)), ['app.js:10']);
  assert.deepEqual(out.flatMap((r) => (r.removed || []).map((e) => e.raw)), ['app.js:8']);
  assertBrowserCoherent(st, rec);
  assert.deepEqual(st.cfg.breaks.map((b) => b.line), [10]);
});

test('browser matrix: add×clear converges to empty or the added line', async () => {
  const dir = tmpdir('mx-br-');
  const st = browserSession(dir);
  const rec = stubBackend(st);
  await seedBrowser(st, rec, ['app.js:8']);
  const out = await runPair(st,
    { cmd: 'breaksAdd', breaks: ['app.js:10'] },
    { cmd: 'breaksClear' });
  assertBrowserCoherent(st, rec);
  const lines = st.cfg.breaks.map((b) => b.line).sort();
  assert.ok(lines.length === 0 || (lines.length === 1 && lines[0] === 10),
    `unexpected survivor: ${JSON.stringify(lines)}`);
  const added = out.flatMap((r) => (r.added || []).map((e) => e.raw));
  assert.deepEqual(added, ['app.js:10']);
});

test('browser matrix: remove×remove same key has exactly one winner', async () => {
  const dir = tmpdir('mx-br-');
  const st = browserSession(dir);
  const rec = stubBackend(st);
  await seedBrowser(st, rec, ['app.js:8']);
  const out = await runPair(st,
    { cmd: 'breaksRemove', breaks: ['app.js:8'] },
    { cmd: 'breaksRemove', breaks: ['app.js:8'] });
  assert.equal(out.filter((r) => (r.removed || []).length === 1).length, 1);
  assert.equal(out.filter((r) => (r.removed || []).length === 0).length, 1);
  assertBrowserCoherent(st, rec);
  assert.deepEqual(st.cfg.breaks, []);
});

test('browser matrix: remove×clear removes every armed line exactly once', async () => {
  const dir = tmpdir('mx-br-');
  const st = browserSession(dir);
  const rec = stubBackend(st);
  await seedBrowser(st, rec, ['app.js:8', 'app.js:10']);
  const out = await runPair(st,
    { cmd: 'breaksRemove', breaks: ['app.js:8'] },
    { cmd: 'breaksClear' });
  assertBrowserCoherent(st, rec);
  assert.deepEqual(st.cfg.breaks, []);
  const removed = out.flatMap((r) => (r.removed || []).map((e) => e.raw)).sort();
  assert.deepEqual(removed, ['app.js:10', 'app.js:8']);
});

test('browser matrix: clear×add converges to empty or the added line', async () => {
  const dir = tmpdir('mx-br-');
  const st = browserSession(dir);
  const rec = stubBackend(st);
  await seedBrowser(st, rec, ['app.js:8']);
  const out = await runPair(st,
    { cmd: 'breaksClear' },
    { cmd: 'breaksAdd', breaks: ['app.js:10'] });
  assertBrowserCoherent(st, rec);
  const lines = st.cfg.breaks.map((b) => b.line).sort();
  assert.ok(lines.length === 0 || (lines.length === 1 && lines[0] === 10),
    `unexpected survivor: ${JSON.stringify(lines)}`);
  const added = out.flatMap((r) => (r.added || []).map((e) => e.raw));
  assert.deepEqual(added, ['app.js:10']);
});

test('browser matrix: add×add different frags never cross-corrupt', async () => {
  const dir = tmpdir('mx-br-');
  const st = browserSession(dir);
  const rec = stubBackend(st);
  const out = await runPair(st,
    { cmd: 'breaksAdd', breaks: ['app.js:8'] },
    { cmd: 'breaksAdd', breaks: ['other.js:6'] });
  assert.equal(out.reduce((n, r) => n + r.added.length, 0), 2);
  assertBrowserCoherent(st, rec);
  assert.deepEqual(
    st.cfg.breaks.map((b) => `${b.frag}:${b.line}`).sort(),
    ['app.js:8', 'other.js:6']);
});
