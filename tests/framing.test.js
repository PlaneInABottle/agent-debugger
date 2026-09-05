const test = require('node:test');
const assert = require('node:assert/strict');
const { EventEmitter } = require('node:events');
const { readFrame } = require('../bridge/js/framing');

test('incomplete request has a fixed deadline and releases listeners', async () => {
  const conn = new EventEmitter();
  const promise = readFrame(conn, 20);
  conn.emit('data', Buffer.from('Content-Length: 10\r\n\r\n{'));
  await assert.rejects(promise, /timed out/);
  assert.equal(conn.listenerCount('data'), 0);
  assert.equal(conn.listenerCount('close'), 0);
});

test('reject oversize, malformed, duplicate headers and non-object requests', async () => {
  for (const data of ['Content-Length: 1048577\r\n\r\n', 'Content-Length: 2x\r\n\r\n{}',
    'Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}', 'x'.repeat(8192),
    'Content-Length: 4\r\n\r\nnull']) {
    const conn = new EventEmitter();
    const promise = readFrame(conn);
    conn.emit('data', Buffer.from(data));
    await assert.rejects(promise);
  }
});

test('fragmented request and client close are handled', async () => {
  const conn = new EventEmitter();
  const promise = readFrame(conn);
  conn.emit('data', Buffer.from('Content-Length: 2\r\n'));
  conn.emit('data', Buffer.from('\r\n{}'));
  assert.deepEqual(await promise, {});
  const closed = new EventEmitter();
  closed.destroyed = true;
  await assert.rejects(readFrame(closed), /truncated/);
});
