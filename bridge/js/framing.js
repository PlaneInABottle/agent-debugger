// Shared session-protocol framing (Content-Length + JSON over TCP).
// Single source: bridge/js/framing.js.
const { BridgeErr } = require('./cdp_conn.js');

function readFrame(conn, timeoutMs = 5000) {
  return new Promise((resolve, reject) => {
    let buf = Buffer.alloc(0);
    const fail = (message) => {
      cleanup();
      reject(new BridgeErr(message));
    };
    const timer = setTimeout(() => fail('frame read timed out'), timeoutMs);
    const onData = (chunk) => {
      if (buf.length + chunk.length > 1024 * 1024 + 8192) return fail('frame body too large');
      buf = Buffer.concat([buf, chunk]);
      const sep = buf.indexOf('\r\n\r\n');
      if (sep < 0) {
        if (buf.length >= 8192) fail('frame header too large');
        return;
      }
      if (sep + 4 > 8192) return fail('frame header too large');
      let length = -1;
      for (const line of buf.subarray(0, sep).toString('ascii').split('\r\n')) {
        const i = line.indexOf(':');
        if (i > 0 && line.slice(0, i).trim().toLowerCase() === 'content-length') {
          const raw = line.slice(i + 1).trim();
          if (!/^\d+$/.test(raw) || length !== -1) return fail('bad Content-Length');
          length = Number(raw);
        }
      }
      if (!Number.isSafeInteger(length) || length < 0 || length > 1024 * 1024) {
        cleanup();
        reject(new BridgeErr('invalid or oversized Content-Length'));
        return;
      }
      if (buf.length < sep + 4 + length) return;
      const body = buf.subarray(sep + 4, sep + 4 + length);
      cleanup();
      try {
        const req = JSON.parse(body.toString('utf-8'));
        if (!req || typeof req !== 'object' || Array.isArray(req)) throw new Error('expected object');
        resolve(req);
      } catch (e) {
        reject(new BridgeErr('bad frame: ' + e.message));
      }
    };
    const onClose = () => {
      cleanup();
      reject(new BridgeErr('truncated frame'));
    };
    // A socket 'error' with no listener crashes the whole daemon (Node
    // rethrows it). Observed live: a client that disconnects mid-command
    // (EPIPE) killed the session server. Swallow here; the read rejects.
    const onError = () => {
      cleanup();
      reject(new BridgeErr('truncated frame'));
    };
    const cleanup = () => {
      clearTimeout(timer);
      conn.removeListener('data', onData);
      conn.removeListener('close', onClose);
      conn.removeListener('error', onError);
    };
    conn.on('data', onData);
    conn.on('close', onClose);
    conn.on('error', onError);
    if (conn.destroyed || conn.readableEnded) onClose();
  });
}

function writeFrame(conn, obj, timeoutMs = 10000) {
  // Never throws synchronously: writing to a dead socket raises sync
  // (writeAfterFIN) instead of calling back with err. A connect+drop health
  // check (e.g. our own `status` probe) used to kill the whole daemon here.
  // Bounded: a peer that stops reading a large body must not pin a handler
  // slot forever (the slot releases only in the caller's `finally`). On
  // timeout the connection is destroyed so the caller observes a normal
  // write failure instead of hanging.
  return new Promise((resolve, reject) => {
    let msg;
    try {
      const body = Buffer.from(JSON.stringify(obj), 'utf-8');
      msg = Buffer.concat([Buffer.from(`Content-Length: ${body.length}\r\n\r\n`), body]);
    } catch (e) {
      reject(e);
      return;
    }
    let done = false;
    const timer = setTimeout(() => {
      if (done) return;
      done = true;
      try {
        conn.destroy();
      } catch (_) { /* already gone */ }
      reject(new BridgeErr('frame write timed out'));
    }, timeoutMs);
    const settle = (fn) => (arg) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      fn(arg);
    };
    try {
      conn.write(msg, (err) => (err ? settle(reject)(err) : settle(resolve)()));
    } catch (e) {
      settle(reject)(e);
    }
  });
}

module.exports = { readFrame, writeFrame };
