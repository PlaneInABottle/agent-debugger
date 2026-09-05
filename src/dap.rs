//! Minimal DAP-style message framing for the CLI<->bridge protocol.
//!
//! Each JSON message is framed as:
//! `Content-Length: <bytes>\r\n\r\n<json body>`
//! `Content-Length` counts **bytes**, not chars. (The Python bridge speaks
//! real DAP to debugpy separately; this module is only our transport.)

use serde_json::Value;

pub fn validate_frame_size(buf: &[u8], max_bytes: usize) -> anyhow::Result<()> {
    anyhow::ensure!(
        buf.len() <= max_bytes,
        "bridge response exceeds {max_bytes} bytes"
    );
    if let Some(end) = find_header_end(buf) {
        anyhow::ensure!(end <= 8192, "bridge frame header exceeds 8192 bytes");
        let header = std::str::from_utf8(&buf[..end])?;
        let len = parse_content_length(header)
            .ok_or_else(|| anyhow::anyhow!("invalid Content-Length"))?;
        anyhow::ensure!(
            len <= max_bytes.saturating_sub(end),
            "bridge response exceeds {max_bytes} bytes"
        );
    } else {
        anyhow::ensure!(buf.len() < 8192, "bridge frame header exceeds 8192 bytes");
    }
    Ok(())
}

/// Encode a JSON body into a DAP-framed byte buffer.
pub fn encode_message(body: &Value) -> Vec<u8> {
    let json = serde_json::to_vec(body).expect("value must serialize");
    let header = format!("Content-Length: {}\r\n\r\n", json.len());
    let mut out = header.into_bytes();
    out.extend_from_slice(&json);
    out
}

/// Try to decode one DAP message from the front of `buf`.
///
/// Returns `None` when the buffer holds no complete message yet.
/// On success returns `(body, bytes_consumed)`.
pub fn try_decode_message(buf: &[u8]) -> Option<(Value, usize)> {
    let header_end = find_header_end(buf)?;
    let header = std::str::from_utf8(&buf[..header_end]).ok()?;
    let content_length = parse_content_length(header)?;
    let body_start = header_end;
    let body_end = body_start.checked_add(content_length)?;
    if buf.len() < body_end {
        return None;
    }
    let body: Value = serde_json::from_slice(&buf[body_start..body_end]).ok()?;
    Some((body, body_end))
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| pos + 4)
}

fn parse_content_length(header: &str) -> Option<usize> {
    for line in header.lines() {
        let (name, value) = line.split_once(':')?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frame_limit_checks_complete_and_declared_lengths() {
        let frame = encode_message(&json!({"value": "payload"}));
        assert!(validate_frame_size(&frame, frame.len()).is_ok());
        assert!(validate_frame_size(&frame, frame.len() - 1).is_err());
        assert!(
            validate_frame_size(b"Content-Length: 67108865\r\n\r\n", 64 * 1024 * 1024).is_err()
        );
        assert!(validate_frame_size(&vec![b'x'; 8192], 64 * 1024 * 1024).is_err());
        assert!(validate_frame_size(b"Content-Length: nope\r\n\r\n", 1024).is_err());
    }

    #[test]
    fn roundtrip_single_message() {
        let body = json!({"seq":1,"type":"request","command":"initialize"});
        let framed = encode_message(&body);
        let (decoded, consumed) = try_decode_message(&framed).expect("must decode");
        assert_eq!(decoded, body);
        assert_eq!(consumed, framed.len());
    }

    #[test]
    fn partial_buffer_returns_none() {
        let body = json!({"seq":2,"command":"launch"});
        let framed = encode_message(&body);
        let cut = framed.len() - 5;
        assert!(try_decode_message(&framed[..cut]).is_none());
    }

    #[test]
    fn two_messages_back_to_back() {
        let first = encode_message(&json!({"seq":1}));
        let second = encode_message(&json!({"seq":2}));
        let mut buf = Vec::new();
        buf.extend_from_slice(&first);
        buf.extend_from_slice(&second);

        let (b1, c1) = try_decode_message(&buf).expect("first");
        assert_eq!(b1, json!({"seq":1}));
        let (b2, c2) = try_decode_message(&buf[c1..]).expect("second");
        assert_eq!(b2, json!({"seq":2}));
        assert_eq!(c1 + c2, buf.len());
    }

    #[test]
    fn multibyte_body_uses_byte_length() {
        // "日本語" is 9 bytes in UTF-8, 3 chars — header must say bytes.
        let body = json!({"text":"日本語"});
        let framed = encode_message(&body);
        let header = std::str::from_utf8(&framed).unwrap();
        let body_bytes = serde_json::to_vec(&body).unwrap();
        assert!(header.starts_with(&format!("Content-Length: {}", body_bytes.len())));
        let (decoded, _) = try_decode_message(&framed).expect("must decode");
        assert_eq!(decoded, body);
    }
}
