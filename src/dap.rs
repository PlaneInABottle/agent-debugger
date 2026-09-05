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
/// Returns `Ok(None)` when the buffer holds no complete message yet
/// (missing header, unparsable header, or a short body — the caller reads
/// more bytes). Returns `Err` the moment a COMPLETE frame carries corrupt
/// JSON, so the caller fails fast instead of waiting out a read timeout.
/// Header/size probing stays in [`validate_frame_size`]; this function only
/// classifies body completeness vs corruption.
/// On success returns `(body, bytes_consumed)`.
pub fn try_decode_message(buf: &[u8]) -> anyhow::Result<Option<(Value, usize)>> {
    let Some(header_end) = find_header_end(buf) else {
        return Ok(None);
    };
    let Ok(header) = std::str::from_utf8(&buf[..header_end]) else {
        return Ok(None);
    };
    let Some(content_length) = parse_content_length(header) else {
        return Ok(None);
    };
    let Some(body_end) = header_end.checked_add(content_length) else {
        return Ok(None);
    };
    if buf.len() < body_end {
        return Ok(None);
    }
    let text = std::str::from_utf8(&buf[header_end..body_end])
        .map_err(|e| anyhow::anyhow!("bridge sent invalid JSON: {e}"))?;
    let body: Value =
        serde_json::from_str(text).map_err(|e| anyhow::anyhow!("bridge sent invalid JSON: {e}"))?;
    if !body.is_object() {
        anyhow::bail!("bridge sent invalid JSON: expected object");
    }
    Ok(Some((body, body_end)))
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
        let (decoded, consumed) = try_decode_message(&framed)
            .expect("decode must not fail")
            .expect("must decode");
        assert_eq!(decoded, body);
        assert_eq!(consumed, framed.len());
    }

    #[test]
    fn partial_buffer_returns_none() {
        let body = json!({"seq":2,"command":"launch"});
        let framed = encode_message(&body);
        let cut = framed.len() - 5;
        assert!(try_decode_message(&framed[..cut])
            .expect("partial must not fail")
            .is_none());
    }

    #[test]
    fn complete_corrupt_json_errors_immediately() {
        // A complete frame (declared length present) with garbage JSON must
        // fail fast — never Ok(None), which would burn a read timeout.
        let bad = b"Content-Length: 6\r\n\r\nnot-js";
        assert!(try_decode_message(bad).is_err());
        // Declared length present but body short stays incomplete.
        assert!(try_decode_message(&bad[..bad.len() - 2])
            .expect("short body must not fail")
            .is_none());
        // Complete non-object JSON is corrupt on this protocol too.
        let null = b"Content-Length: 4\r\n\r\nnull";
        assert!(try_decode_message(null).is_err());
        // Missing/incomplete headers stay incomplete (size probing lives in
        // validate_frame_size, unchanged).
        assert!(try_decode_message(b"Content-Length: 4")
            .expect("header prefix must not fail")
            .is_none());
    }

    #[test]
    fn two_messages_back_to_back() {
        let first = encode_message(&json!({"seq":1}));
        let second = encode_message(&json!({"seq":2}));
        let mut buf = Vec::new();
        buf.extend_from_slice(&first);
        buf.extend_from_slice(&second);

        let (b1, c1) = try_decode_message(&buf)
            .expect("first must not fail")
            .expect("first");
        assert_eq!(b1, json!({"seq":1}));
        let (b2, c2) = try_decode_message(&buf[c1..])
            .expect("second must not fail")
            .expect("second");
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
        let (decoded, _) = try_decode_message(&framed)
            .expect("decode must not fail")
            .expect("must decode");
        assert_eq!(decoded, body);
    }
}
