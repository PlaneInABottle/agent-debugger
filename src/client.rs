//! TCP client for the bridge session server (one request per connection).

use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use crate::dap;

/// Transport-level safety bound. Snapshots are kilobytes (token caps live
/// in the bridges); anything past this is a corrupt or rogue bridge, not
/// data. Without a cap a bogus Content-Length could OOM this CLI.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Send one framed request, return the parsed response object.
pub fn request(port: u16, body: &Value, timeout: Duration) -> anyhow::Result<Value> {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse()?;
    let mut sock = TcpStream::connect_timeout(&addr, timeout).map_err(|e| {
        anyhow::anyhow!(
            "cannot reach debug session on port {port}: {e} (stale session? try `close`)"
        )
    })?;
    sock.set_read_timeout(Some(timeout))?;
    sock.set_write_timeout(Some(timeout))?;
    sock.write_all(&dap::encode_message(body))?;

    let mut buf = Vec::with_capacity(65536);
    let mut tmp = [0u8; 65536];
    loop {
        dap::validate_frame_size(&buf, MAX_FRAME_BYTES)?;
        if let Some((value, _)) = dap::try_decode_message(&buf) {
            return Ok(value);
        }
        match sock.read(&mut tmp) {
            Ok(0) => {
                // EOF: try once more, then fail.
                if let Some((value, _)) = dap::try_decode_message(&buf) {
                    return Ok(value);
                }
                anyhow::bail!("debug session closed the connection mid-response");
            }
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) => anyhow::bail!("read from debug session failed: {e}"),
        }
    }
}
