// Shared session-protocol framing (Content-Length + JSON over TCP).
// Single source: bridge/js/framing.js.
const { BridgeErr } = require('./cdp_conn.js');

function readFrame(conn) {
  return new Promise((resolve, reject) => {
    let buf = Buffer.alloc(0);
    const onData = (chunk) => {
      buf = Buffer.concat([buf, chunk]);
      const sep = buf.indexOf('\r\n\r\n');
      if (sep < 0) return;
      let length = -1;
      for (const line of buf.subarray(0, sep).toString('ascii').split('\r\n')) {
        const i = line.indexOf(':');
        if (i > 0 && line.slice(0, i).trim().toLowerCase() === 'content-length') {
          length = parseInt(line.slice(i + 1).trim(), 10);
        }
      }
      if (Number.isNaN(length) || length < 0) {
        cleanup();
        reject(new BridgeErr('bad frame: no Content-Length'));
        return;
      }
      if (buf.length < sep + 4 + length) return;
      const body = buf.subarray(sep + 4, sep + 4 + length);
      cleanup();
      try {
        resolve(JSON.parse(body.toString('utf-8')));
      } catch (e) {
        reject(new BridgeErr('bad frame: ' + e.message));
      }
    };
    const onClose = () => {
      cleanup();
      reject(new BridgeErr('truncated frame'));
    };
    const cleanup = () => {
      conn.removeListener('data', onData);
      conn.removeListener('close', onClose);
    };
    conn.on('data', onData);
    conn.on('close', onClose);
  });
}

function writeFrame(conn, obj) {
  // Never throws synchronously: writing to a dead socket raises sync
  // (writeAfterFIN) instead of calling back with err. A connect+drop health
  // check (e.g. our own `status` probe) used to kill the whole daemon here.
  return new Promise((resolve, reject) => {
    let msg;
    try {
      const body = Buffer.from(JSON.stringify(obj), 'utf-8');
      msg = Buffer.concat([Buffer.from(`Content-Length: ${body.length}\r\n\r\n`), body]);
    } catch (e) {
      reject(e);
      return;
    }
    try {
      conn.write(msg, (err) => (err ? reject(err) : resolve()));
    } catch (e) {
      reject(e);
    }
  });
}

module.exports = { readFrame, writeFrame };
