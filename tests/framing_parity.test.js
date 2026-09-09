const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { EventEmitter } = require('node:events');
const { readFrame } = require('../bridge/js/framing');

const FIXTURE = JSON.parse(
  fs.readFileSync(path.join(__dirname, 'contract', 'framing.json'), 'utf-8'),
);

function expand(c) {
  if (c.raw !== undefined) return Buffer.from(c.raw, 'utf-8');
  return Buffer.from(c.rawRepeat.prefix + c.rawRepeat.char.repeat(c.rawRepeat.count), 'utf-8');
}

function feed(payload) {
  const conn = new EventEmitter();
  const promise = readFrame(conn, 5000);
  conn.emit('data', payload);
  return promise;
}

test('shared fixture categories (reject vs accept)', async () => {
  for (const c of FIXTURE.cases) {
    const payload = expand(c);
    if (c.expect === 'reject') {
      await assert.rejects(feed(payload), c.name);
    } else {
      assert.deepEqual(await feed(payload), JSON.parse(c.body), c.name);
    }
  }
});

test('body at cap accepts (1048576)', async () => {
  // Identical parameters in all three harnesses: a 1048576-byte body
  // (the exact cap) is accepted, proving the 1M bound itself did not
  // move while negatives tightened.
  const inner = 'x'.repeat(1048576 - 8);
  const body = `{"k":"${inner}"}`;
  assert.equal(Buffer.byteLength(body), 1048576);
  const raw = Buffer.from(`Content-Length: 1048576\r\n\r\n${body}`, 'utf-8');
  assert.deepEqual(await feed(raw), { k: inner });
});
