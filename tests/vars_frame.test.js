const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const Module = require('node:module');

// Milestone B: uniform vars/eval frame validation on Node + Browser.
// Same contract as Python/Java: missing/null reads as 0; finite integer
// numbers ≥ 0 and 1–15 ASCII digit strings are the index; malformed,
// fractional, negative, over-long, or mistyped input is a typed
// `<cmd> needs integer frame` (never coerced to 0); well-formed
// past-the-end is `no frame N (have M)`. requireStopped still runs
// first (covered elsewhere).
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

const node = loadBridge('bridge/node/src/nodebridge.js', 'Session, BridgeErr');
const browser = loadBridge('bridge/browser/src/browserbridge.js', 'Session, BridgeErr');

function tmpdir(prefix) {
  return fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), prefix)));
}

function frame(callFrameId = 'f1') {
  return {
    callFrameId, functionName: 'handler', scopeChain: [],
    location: { scriptId: 's1', lineNumber: 4 },
  };
}

function parkedSession(mk, dir) {
  const st = mk(dir);
  st.paused = { frames: [frame('f1'), frame('f2')], stopInfo: null };
  st.cdp = { request: async () => ({}) };
  return st;
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

const BAD = ['abc', '', '3x', '3.5', '-1', '+1', ' 3', '3 ', '0x3',
  '9999999999999999', '99999999999999999999',
  -1, -2, 3.5, Infinity, NaN, true, false, ['1'], { n: 1 }];

for (const [name, mk] of [['node', nodeSession], ['browser', browserSession]]) {
  test(`${name} vars frame validation matrix`, async () => {
    const st = parkedSession(mk, tmpdir('vf-vars-'));
    for (const [req, want] of [[{}, 0], [{ frame: null }, 0],
      [{ frame: 0 }, 0], [{ frame: 1 }, 1], [{ frame: '1' }, 1],
      [{ frame: '001' }, 1], [{ frame: 1.0 }, 1]]) {
      const resp = await st.cmdVars(req);
      assert.equal(resp.frame, want, `${name} ${JSON.stringify(req)}`);
      assert.equal(resp.target, undefined, 'unstamped at command level (serve layer stamps)');
      assert.deepEqual(resp.locals, [], 'empty scope chain reads clean');
    }
    for (const [raw, msg] of [[2, 'no frame 2 (have 2)'],
      [99, 'no frame 99 (have 2)'],
      ['99999999999', 'no frame 99999999999 (have 2)']]) {
      await assert.rejects(st.cmdVars({ frame: raw }),
        (e) => e.message === msg, `${name} range ${raw}`);
    }
    for (const bad of BAD) {
      await assert.rejects(st.cmdVars({ frame: bad }), (e) => {
        assert.equal(e.message, 'vars needs integer frame', `${name} ${String(bad)}`);
        assert.ok(!e.message.includes('internal'), 'never internal');
        return true;
      });
    }
  });

  test(`${name} eval shares the frame contract with its own word`, async () => {
    const st = parkedSession(mk, tmpdir('vf-eval-'));
    await assert.rejects(st.cmdEval({ expr: '1', frame: '1.5' }),
      (e) => e.message === 'eval needs integer frame');
    await assert.rejects(st.cmdEval({ expr: '1', frame: 9 }),
      (e) => e.message === 'no frame 9 (have 2)');
    // A valid frame reaches evaluation (refs() trap proves the frame
    // passed without any CDP traffic).
    await assert.rejects(st.cmdEval({ expr: 'refs(x)', frame: '0' }), /refs\(\) unsupported/);
  });

  test(`${name} parseFrameIndex helper matrix`, () => {
    const st = mk(tmpdir('vf-helper-'));
    assert.equal(st.parseFrameIndex(undefined, 2, 'vars'), 0);
    assert.equal(st.parseFrameIndex('007', 8, 'vars'), 7);
    assert.throws(() => st.parseFrameIndex('abc', 2, 'vars'),
      (e) => e.message === 'vars needs integer frame');
    assert.throws(() => st.parseFrameIndex(-1, 2, 'vars'),
      (e) => e.message === 'vars needs integer frame');
    assert.throws(() => st.parseFrameIndex(2, 2, 'vars'),
      (e) => e.message === 'no frame 2 (have 2)');
  });
}
