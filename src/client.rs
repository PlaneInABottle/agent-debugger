//! TCP client for the bridge session server (one request per connection).

use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use crate::dap;

/// Transport-level safety bound. Snapshots are kilobytes (token caps live
/// in the bridges); anything past this is a corrupt or rogue bridge, not
/// data. Without a cap a bogus Content-Length could OOM this CLI.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Send one framed request, return the parsed response object. Transient
/// read faults (spurious wakeups, signals, slow first byte) retry within
/// the overall deadline instead of surfacing raw `EAGAIN`/`Interrupted`
/// to users; a bridge that stays silent past the deadline reads as a
/// timeout.
pub fn request(port: u16, body: &Value, timeout: Duration) -> anyhow::Result<Value> {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse()?;
    let mut sock = TcpStream::connect_timeout(&addr, timeout).map_err(|e| {
        anyhow::anyhow!(
            "cannot reach debug session on port {port}: {e} (stale session? try `close`)"
        )
    })?;
    sock.set_write_timeout(Some(timeout))?;
    sock.write_all(&dap::encode_message(body))?;

    let deadline = Instant::now() + timeout;
    let mut buf = Vec::with_capacity(65536);
    let mut tmp = [0u8; 65536];
    loop {
        dap::validate_frame_size(&buf, MAX_FRAME_BYTES)?;
        // Corrupt-but-complete JSON errors here at once (no timeout wait);
        // incomplete frames fall through to read more bytes.
        if let Some((value, _)) = dap::try_decode_message(&buf)? {
            return Ok(value);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("timed out waiting for debug session on port {port}");
        }
        // Bound each blocking read by the time left: a single slow read
        // must not outlive the caller's budget, and a transient fault
        // must not end it early either.
        sock.set_read_timeout(Some(remaining))?;
        match sock.read(&mut tmp) {
            Ok(0) => {
                // EOF: try once more, then fail.
                if let Some((value, _)) = dap::try_decode_message(&buf)? {
                    return Ok(value);
                }
                anyhow::bail!("debug session closed the connection mid-response");
            }
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::Interrupted
                    || e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => anyhow::bail!("read from debug session failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_port(listener: &std::net::TcpListener) -> u16 {
        listener.local_addr().unwrap().port()
    }

    #[test]
    fn slow_fragmented_reply_succeeds_within_budget() {
        // One byte, a stall, then the rest: incomplete frames must fall
        // through to further reads, not error.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = test_port(&listener);
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = [0u8; 65536];
            use std::io::Read as _;
            let _ = conn.read(&mut buf);
            let frame = crate::dap::encode_message(&json!({"ok": true}));
            conn.write_all(&frame[..1]).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            conn.write_all(&frame[1..]).unwrap();
        });
        let v = request(port, &json!({"cmd": "breaks"}), Duration::from_secs(5)).unwrap();
        assert_eq!(v, json!({"ok": true}));
    }

    #[test]
    fn silent_bridge_times_out_instead_of_raw_eagain() {
        // Accept but never answer: the call must fail with a timeout
        // verdict inside the budget — never raw `EAGAIN`/`Interrupted`.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = test_port(&listener);
        std::thread::spawn(move || {
            let (_conn, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_secs(30));
        });
        let start = Instant::now();
        let err = request(port, &json!({"cmd": "breaks"}), Duration::from_secs(1)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("timed out"), "must read as timeout: {msg}");
        assert!(
            !msg.contains("temporarily unavailable"),
            "no raw EAGAIN: {msg}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "must not hang past the budget"
        );
    }
}
