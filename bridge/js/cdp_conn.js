// Shared CDP wire core (nodebridge + browserbridge). Single source:
// bridge/js/cdp_conn.js — provisioned next to each bridge, required
// relatively so both adapters share protocol fixes.
class BridgeErr extends Error {}

class CdpConn {
  constructor(ws, onClose) {
    this.ws = ws;
    this.seq = 0;
    this.pending = new Map();
    this.onEvent = null;
    this.closed = false;
    ws.on('message', (data) => {
      let msg;
      try {
        msg = JSON.parse(data.toString());
      } catch (_) {
        return;
      }
      if (msg.id !== undefined && this.pending.has(msg.id)) {
        const { resolve } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        resolve(msg);
      } else if (msg.method && this.onEvent) {
        this.onEvent(msg);
      }
    });
    ws.on('close', () => {
      this.closed = true;
      for (const { reject } of this.pending.values()) {
        reject(new BridgeErr('CDP connection closed'));
      }
      this.pending.clear();
      if (onClose) {
        try {
          onClose();
        } catch (_) { /* best effort */ }
      }
    });
    ws.on('error', () => { /* close follows */ });
  }

  request(method, params = {}, timeoutMs = 30000) {
    if (this.closed) return Promise.reject(new BridgeErr('CDP connection closed'));
    const id = ++this.seq;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new BridgeErr(`CDP ${method} timed out after ${Math.round(timeoutMs / 1000)}s`));
      }, timeoutMs);
      this.pending.set(id, {
        resolve: (msg) => {
          clearTimeout(timer);
          if (msg.error) {
            reject(new BridgeErr(`CDP ${method} failed: ${msg.error.message || JSON.stringify(msg.error)}`));
          } else {
            resolve(msg.result || {});
          }
        },
        reject: (e) => {
          clearTimeout(timer);
          reject(e);
        },
      });
      this.ws.send(JSON.stringify({ id, method, params }), (err) => {
        if (err) {
          this.pending.delete(id);
          clearTimeout(timer);
          reject(new BridgeErr(`CDP ${method} send failed: ${err.message}`));
        }
      });
    });
  }

  close() {
    try {
      this.ws.close();
    } catch (_) { /* best effort */ }
  }
}

module.exports = { BridgeErr, CdpConn };
