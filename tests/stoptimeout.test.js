const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const SRC = fs.readFileSync(
  path.join(__dirname, '..', 'bridge', 'node', 'src', 'nodebridge.js'), 'utf-8');

test('StopTimeout is a typed BridgeErr subclass', () => {
  assert.match(SRC, /class StopTimeout extends BridgeErr/);
});

test('first-stop pump throws the typed timeout, not a generic error', () => {
  // The pump may attach the additive waitContext (wait/capture only), but
  // the error stays a typed StopTimeout with the frozen message — never a
  // generic error or a message-string match.
  assert.match(SRC, /new StopTimeout\(this\.timeoutText\(timeout\)\)/);
  assert.match(SRC, /timeout: no stop within \$\{fmtTimeout\(timeout\)\}/);
});

test('attach-only fallback is checked before generic error handling', () => {
  const fallback = SRC.indexOf("e instanceof StopTimeout && st.cfg.kind === 'attach'");
  assert.notEqual(fallback, -1);
  const generic = SRC.indexOf("e instanceof BridgeErr && e.message === 'target exited'");
  assert.notEqual(generic, -1);
  assert.ok(fallback < generic, 'typed attach fallback must precede generic handling');
});

test('timeout is never matched by message string', () => {
  assert.doesNotMatch(SRC, /=== 'timeout/);
});

test('fallback publishes a running session and keeps serving', () => {
  const fallback = SRC.indexOf("e instanceof StopTimeout && st.cfg.kind === 'attach'");
  const block = SRC.slice(fallback, fallback + 1200);
  assert.match(block, /stopped: false/);
  assert.match(block, /await serve\(st, server, queue\)/);
});
