// See `mod.rs` for the one-way dependency DAG.
use super::identity::{is_secret_flag, listener_source_readable, port_lookup, trunc_chars};
use super::paths::{check_name, is_unspecified_host, normalize_attach_host};
use super::sidecar::{cli_markers_v2, session_lang_opt, SpawnSpec};
use crate::dap;
use serde_json::{json, Value};
use std::time::Duration;

/// Capability-aware exclusivity as a conservative safety policy (not a
/// proven universal fact): debugpy observably refuses a second attach —
/// and can kill the target — so Python blocks unconditionally. The Node
/// inspector and JDWP single-debugger behavior is configuration-dependent
/// (some servers/setups may tolerate more), but a second agent-debugger
/// session risks refusal or target death, so they block too, with an
/// actionable message pointing at the existing session. Browser CDP
/// multiplexes tabs/clients, so browser attach is never blocked here
/// (fail open for unknown langs too).
pub fn attach_exclusive(lang: &str) -> bool {
    matches!(lang, "py" | "node" | "java")
}

/// The attach endpoint a spawn targets, from the redacted requested
/// identity (host/port only — never argv). `None` for launch intents and
/// anything without a host/port (browser tab selects don't collide).
/// `Err` only for an unspecified destination (`0.0.0.0`/`::`): dialing it
/// is platform-dependent, so it fails fast with a clear error instead of
/// a misleading handshake failure. Unparseable intents stay `None` (fail
/// open — never block on what we cannot read).
pub(crate) fn attach_endpoint(spec: &SpawnSpec) -> anyhow::Result<Option<(String, u16)>> {
    if spec.kind != "attach" || !attach_exclusive(spec.lang) {
        return Ok(None);
    }
    let host = spec
        .requested
        .get("host")
        .and_then(|h| h.as_str())
        .unwrap_or("");
    let port = spec
        .requested
        .get("port")
        .and_then(|p| p.as_u64())
        .and_then(|p| u16::try_from(p).ok());
    match (host, port) {
        (h, _) if is_unspecified_host(h) => anyhow::bail!(
            "cannot attach to unspecified address '{h}' \
             (use localhost, 127.0.0.1, or an explicit interface address)"
        ),
        (h, Some(p)) if !h.is_empty() => Ok(Some((h.to_string(), p))),
        _ => Ok(None),
    }
}

/// Listener presence without connecting: OS-observed only (`lsof`/proc via
/// `port_lookup`), never a diagnostic socket — a TCP probe could itself
/// consume or perturb a single-client handshake. Remote hosts have no
/// local source: `None` (unknown), never fabricated.
pub(crate) fn listener_present(host: &str, port: u16) -> Option<bool> {
    if normalize_attach_host(host) != "loopback" {
        return None;
    }
    // An unreadable probe source (no /proc in a container, missing lsof)
    // is "unknown", never "not listening": collapsing it into Some(false)
    // misdiagnoses with high confidence (see attach_diagnosis below,
    // which reads Some(false)+Some(false) as endpoint-not-listening).
    if !listener_source_readable() {
        return None;
    }
    Some(port_lookup(port).is_some())
}

/// A session daemon is live when its bridge port answers a short TCP probe
/// (same probe as the status row; the daemon is ours, not the target — no
/// target connection is ever opened here). Two attempts at a 1s bound
/// absorb scheduling flakes on loaded machines; when both fail but the
/// bridge published state moments ago, the session still counts as live
/// (fail closed — a wedged probe must never authorize a second attach
/// onto a live target). The corroboration window is bounded (2min), so a
/// truly dead session stops blocking shortly after its last publish.
///
/// NOTE: this stays deliberately TCP-based (not protocol-aware): a
/// stranger reusing the port must still read as live HERE, otherwise a
/// second attach could land on a live exclusive target the stranger
/// happens to share the port with. Stranger discrimination lives in
/// `probe_session_bridge`, used only by `status` (display) and `close`
/// (cleanup) — never by attach gating or legacy reclaim.
pub(crate) fn daemon_alive_in(dir: &std::path::Path) -> bool {
    const DAEMON_PROBE: Duration = Duration::from_secs(1);
    const RECENT_PUBLISH: Duration = Duration::from_secs(120);
    let parsed: Option<Value> = std::fs::read_to_string(dir.join("session.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok());
    let port = parsed
        .as_ref()
        .and_then(|v| {
            v.get("port")
                .and_then(|p| p.as_u64())
                .and_then(|p| u16::try_from(p).ok())
        })
        .unwrap_or(0);
    if port == 0 {
        return false; // no daemon port recorded: nothing to be live
    }
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    for _ in 0..2 {
        if std::net::TcpStream::connect_timeout(&addr, DAEMON_PROBE).is_ok() {
            return true;
        }
    }
    // Probe failed twice: only a very recent bridge publish keeps the
    // session live (transient probe failure, not a dead daemon).
    if published_recently(&parsed.unwrap_or(serde_json::json!({})), RECENT_PUBLISH) {
        return true;
    }
    false
}

/// Bridge publish recency: `updatedAt` (seconds since epoch, written by
/// the bridge on every publish) is within `bound` of now. Unparseable or
/// future timestamps read as stale, never live.
pub(crate) fn published_recently(session: &Value, bound: Duration) -> bool {
    let age = session
        .get("updatedAt")
        .and_then(|u| u.as_u64())
        .and_then(|s| {
            std::time::UNIX_EPOCH
                .checked_add(Duration::from_secs(s))
                .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
        });
    matches!(age, Some(a) if a < bound)
}

/// Outcome of a protocol-aware liveness probe against a recorded port.
/// Unlike the bare-TCP checks, this distinguishes our debugger bridge
/// from a stranger that reused the port after our bridge died.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Probe {
    /// A valid debugger envelope answered: our bridge is up.
    Ours,
    /// Definitively not our bridge, with a short diagnostic reason
    /// (refused/dead, EOF, corrupt framing, or valid framing with a
    /// non-debugger shape). Safe to treat as gone.
    NotOurs(&'static str),
    /// Ambiguous (timeouts): never proof either way — callers fail safe
    /// toward the pre-existing TCP behavior instead of deciding.
    Unclear,
}

/// Is the listener on `port` OUR debugger bridge? Sends the lightest
/// side-effect-free read — bare `breaks` list (in-memory intent state on
/// every bridge: no resume, no arming, no stop required) — and requires a
/// valid debugger envelope: a JSON object with boolean `ok`, plus a
/// `stops` array on success or an `error` string on refusal. A stranger
/// (HTTP server, DAP adapter, node inspector, bare listener) fails
/// framing or shape and reads as NotOurs; a blackhole that accepts but
/// never answers reads as Unclear. Total bound is `timeout` (connect +
/// write + read); refused connections return fast.
pub(crate) fn probe_session_bridge(port: u16, timeout: Duration) -> Probe {
    use std::io::{Read, Write};
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut sock = match std::net::TcpStream::connect_timeout(&addr, timeout) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            return Probe::NotOurs("connection refused");
        }
        Err(_) => return Probe::Unclear,
    };
    if sock.set_read_timeout(Some(timeout)).is_err()
        || sock.set_write_timeout(Some(timeout)).is_err()
    {
        return Probe::Unclear;
    }
    if sock
        .write_all(&dap::encode_message(&json!({"cmd": "breaks"})))
        .is_err()
    {
        // Accepted then died before our write landed: not serving us.
        return Probe::NotOurs("connection lost");
    }
    let mut buf = Vec::with_capacity(65536);
    let mut tmp = [0u8; 65536];
    loop {
        // Probe cap is small on purpose: a legit `breaks` answer is
        // kilobytes; anything past this is a rogue bridge, not data.
        // (Headerless junk past the header bound is likewise definitively
        // not our framing — a real frame always opens with its short
        // Content-Length header.)
        if buf.len() > 1024 * 1024 {
            return Probe::NotOurs("oversize frame");
        }
        if dap::validate_frame_size(&buf, 1024 * 1024).is_err() {
            return Probe::NotOurs("invalid protocol");
        }
        match dap::try_decode_message(&buf) {
            Err(_) => return Probe::NotOurs("invalid protocol"),
            Ok(Some((v, _))) => {
                return if valid_breaks_envelope(&v) {
                    Probe::Ours
                } else {
                    Probe::NotOurs("unexpected protocol")
                };
            }
            Ok(None) => {}
        }
        match sock.read(&mut tmp) {
            Ok(0) => return Probe::NotOurs("closed connection"),
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Probe::Unclear
            }
            Err(_) => return Probe::NotOurs("connection lost"),
        }
    }
}

/// Debugger-envelope shape for the probe's `breaks` read: success carries
/// a `stops` array, refusal carries an `error` string (an exited bridge
/// still answers — its process is up and speaking our protocol).
pub(crate) fn valid_breaks_envelope(v: &Value) -> bool {
    match v.get("ok").and_then(|o| o.as_bool()) {
        Some(true) => v.get("stops").and_then(|s| s.as_array()).is_some(),
        Some(false) => v.get("error").and_then(|e| e.as_str()).is_some(),
        None => false,
    }
}

/// Attach endpoint from a persisted intent: `requestedTarget` only (exact
/// CLI request, numeric port). The old `target`-summary fallback (string
/// ports) is gone: v2 intents always carry a numeric `requestedTarget`.
/// Anything unparseable reads as absent — never a block.
pub(crate) fn intent_endpoint(stops: &Value) -> Option<(String, u16)> {
    let v = stops.get("requestedTarget")?;
    let host = v.get("host")?.as_str()?;
    let port = v.get("port")?.as_u64()?;
    let port = u16::try_from(port).ok()?;
    Some((host.to_string(), port))
}

/// Proven-dead legacy dir: parseable `session.json` with a numeric nonzero
/// port AND a dead daemon (double TCP fail + no recent publish). Only this
/// proof authorizes spawn-time reclaim; everything else bails for `close`.
pub(crate) fn legacy_proven_dead(dir: &std::path::Path) -> bool {
    let port: Option<u16> = std::fs::read_to_string(dir.join("session.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v.get("port").and_then(|p| p.as_u64()))
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p != 0);
    port.is_some() && !daemon_alive_in(dir)
}

/// A confirmed live owner of an attach endpoint: a session dir (not a
/// symlink, valid name) whose intent names the same normalized endpoint,
/// whose lang is exclusive, whose kind is attach/launch, and whose daemon
/// answers. Stale/dead dirs never block; nothing is deleted as a side
/// effect — this is a read-only scan.
pub(crate) fn find_live_endpoint_owner_in(
    sessions_root: &std::path::Path,
    exclude: &str,
    host: &str,
    port: u16,
) -> Option<String> {
    let norm = normalize_attach_host(host);
    let entries = std::fs::read_dir(sessions_root).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == exclude || check_name(&name).is_err() {
            continue;
        }
        let dir = sessions_root.join(&name);
        // Symlink or non-dir entries are never followed (same guard as the
        // stale-dir clear) and silently skipped here — not our session.
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.file_type().is_dir() => {}
            _ => continue,
        }
        // Noisy siblings (launch intents, malformed or unreadable sidecars,
        // still-starting dirs) are skipped entry by entry: a `?` here would
        // abort the whole scan on an unrelated dir and hide a live owner.
        let stops: Value = match std::fs::read_to_string(dir.join("stops.json"))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
        {
            Some(v) => v,
            None => continue,
        };
        let (eh, ep) = match intent_endpoint(&stops) {
            Some(e) => e,
            None => continue,
        };
        if ep != port || normalize_attach_host(&eh) != norm {
            continue;
        }
        // Capability-aware: only exclusive owners collide. A browser
        // session on the same CDP port (different tab) never blocks.
        if !attach_exclusive(session_lang_opt(&dir).as_deref().unwrap_or("")) {
            continue;
        }
        let sess: Value = match std::fs::read_to_string(dir.join("session.json"))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
        {
            Some(v) => v,
            None => continue,
        };
        match sess.get("kind").and_then(|k| k.as_str()) {
            Some("attach") | Some("launch") => {}
            _ => continue,
        }
        if !daemon_alive_in(&dir) {
            continue;
        }
        return Some(name);
    }
    None
}

/// Global live-legacy scan (fail-safe upgrade rule): every remaining live
/// OLD session dir (CLI markers not v2) whose lang is exclusive or
/// unknown/missing (fail-closed — an unknowable owner may hold our
/// endpoint), excluding browser (CDP multiplexes, never collides) and the
/// session being started. Sorted by name. Read-only: nothing is deleted or
/// touched — a live old daemon is only ever cleaned by `close`.
pub(crate) fn find_live_legacy_sessions_in(
    sessions_root: &std::path::Path,
    exclude: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(sessions_root) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == exclude || check_name(&name).is_err() {
            continue;
        }
        let dir = sessions_root.join(&name);
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.file_type().is_dir() => {}
            _ => continue,
        }
        if cli_markers_v2(&dir) {
            continue;
        }
        match session_lang_opt(&dir).as_deref() {
            Some("browser") => continue,
            Some(l) if attach_exclusive(l) => {}
            // Unknown/missing lang fails closed: it may be an exclusive
            // owner whose markers predate the scan.
            _ => {}
        }
        let kind: Option<String> = std::fs::read_to_string(dir.join("session.json"))
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|v| {
                v.get("kind")
                    .and_then(|k| k.as_str())
                    .map(|s| s.to_string())
            });
        match kind.as_deref() {
            Some("attach") | Some("launch") => {}
            _ => continue,
        }
        if !daemon_alive_in(&dir) {
            continue;
        }
        out.push(name);
    }
    out.sort();
    out
}

// ---- attach failure classification ----

/// Classify an attach setup failure from OS-observed listener state taken
/// immediately pre-attach (`pre`) and immediately after the failure
/// (`post`). `None` is unknown (remote hosts have no local source).
/// Confidence is calibrated, never certain about foreign clients: a
/// present-but-rejecting listener *may* already have another debugger
/// client — it is not claimed as fact.
pub(crate) fn attach_diagnosis(pre: Option<bool>, post: Option<bool>, endpoint: &str) -> Value {
    let (code, confidence, recommendation) = match (pre, post) {
        (Some(true), Some(false)) => (
            "endpoint-closed-during-attach",
            "medium",
            "the debug server was listening before attach but is gone now; \
             the target may have exited — restart it and retry",
        ),
        (Some(true), Some(_)) | (Some(false), Some(true)) => (
            "endpoint-rejected",
            "medium",
            "the listener is up but refused the handshake; it may already \
             have another debugger client — use that session, or restart the \
             target without the existing client and retry",
        ),
        (Some(false), Some(false)) => (
            "endpoint-not-listening",
            "high",
            "nothing listens on this endpoint; verify the target was started \
             with the debug server on this host/port and retry",
        ),
        _ => (
            "endpoint-unreachable",
            "low",
            "the host cannot be inspected locally; verify the host/port are \
             reachable and the debug server is up, then retry",
        ),
    };
    serde_json::json!({
        "code": code,
        "confidence": confidence,
        "evidence": {
            "endpoint": endpoint,
            "listenerBefore": pre.map(Value::from).unwrap_or(Value::Null),
            "listenerAfter": post.map(Value::from).unwrap_or(Value::Null),
        },
        "recommendation": recommendation,
    })
}

/// Bounded `host:port` display for messages and diagnosis evidence: the
/// host is CLI input of unbounded length, so over-long hosts truncate with
/// the shared marker instead of bloating the envelope. The exact requested
/// host still lives in `requestedTarget`; this is display only.
pub(crate) fn display_endpoint(host: &str, port: u16) -> String {
    const HOST_CAP: usize = 128;
    let h = if host.chars().count() > HOST_CAP {
        trunc_chars(host, HOST_CAP)
    } else {
        host.to_string()
    };
    // Bracket bare IPv6 literals so `::1:5678` never reads as host `::1`
    // with a garbage port — `host:port` is only unambiguous for v4/names.
    if h.contains(':') && !(h.starts_with('[') && h.ends_with(']')) {
        format!("[{h}]:{port}")
    } else {
        format!("{h}:{port}")
    }
}

/// Cap (chars) for the sanitized attach-failure `cause` (raw adapter
/// message preserved separately from the concise top-level `error`).
pub const CAUSE_CAP: usize = 2048;

/// Sanitize a raw bridge/adapter message for the additive `cause` field:
/// bounded secret redaction (URL/query `token=`-style values, `Bearer` /
/// `Basic` credentials, `--token` argv forms) then a hard char cap. Never fabricates:
/// an empty input reads as empty, and useful line/class context survives.
pub(crate) fn sanitize_cause(raw: &str) -> String {
    let redacted = redact_cause_secrets(raw);
    if redacted.chars().count() <= CAUSE_CAP {
        return redacted;
    }
    trunc_chars(&redacted, CAUSE_CAP)
}

/// Bounded redaction over free-text adapter output (no regex dep): any
/// `name=value` / `name:value` pair whose name is a secret flag keeps the
/// name and masks the value; a `Bearer <token>` word masks the token; a
/// bare secret `--flag <value>` masks the following word. Delimiters are
/// whitespace, `&`, `;`, `,`, quotes — values never span them.
pub(crate) fn redact_cause_secrets(raw: &str) -> String {
    let masked_eq = mask_keyed_values(raw);
    mask_bare_flag_values(&mask_bearer(&masked_eq))
}

/// Mask `name=value` / `name:value` (incl. URL `?token=abc&x=1`) whose name
/// is a secret flag. Walks `=`/`:` sites, extracts the trailing name token,
/// checks it with the argv redactor's predicate, and masks the value span.
pub(crate) fn mask_keyed_values(s: &str) -> String {
    let bytes: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == '=' || c == ':' {
            // Extract the name token immediately before the separator:
            // [A-Za-z0-9_.-]+ plus URL prefixes (? & ;). A leading run of
            // `?`/`&`/`;`/`#` is skipped, then the name must be non-empty.
            let mut j = out.len();
            while j > 0 && matches!(out.as_bytes()[j - 1], b'?' | b'&' | b';' | b'#') {
                j -= 1;
            }
            // JSON-quoted keys (`{"password": "x"}`): skip the closing
            // quote (and any space before the separator) so the name still
            // classifies — otherwise the head reads empty and the secret
            // is never masked.
            while j > 0 && matches!(out.as_bytes()[j - 1], b'"' | b'\'' | b' ' | b'\t') {
                j -= 1;
            }
            let mut k = j;
            while k > 0 {
                let b = out.as_bytes()[k - 1];
                if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
                    k -= 1;
                } else {
                    break;
                }
            }
            let head = out[k..j].to_string();
            // Mask the value span: up to the next delimiter (whitespace,
            // `&`, `;`, `,`, quote). A leading quote (`"key": "value"`) is
            // stepped over so a quoted secret still masks (the quotes stay
            // in place around `[redacted]`). `://` after a bare scheme
            // (`http:`) is not a secret pair — its "value" starts with
            // `//`, skip it.
            let mut v = i + 1;
            while v < bytes.len() && bytes[v].is_whitespace() {
                v += 1;
            }
            let quote = if v < bytes.len() && matches!(bytes[v], '"' | '\'') {
                let q = bytes[v];
                v += 1;
                Some(q)
            } else {
                None
            };
            let mut vend = v;
            while vend < bytes.len()
                && !bytes[vend].is_whitespace()
                && !matches!(bytes[vend], '&' | ';' | ',' | '"' | '\'' | ')')
            {
                vend += 1;
            }
            let val: String = bytes[v..vend].iter().collect();
            // A `Bearer <token>` / `Basic <credentials>` value belongs to
            // the scheme pass below (which keeps the scheme and masks the
            // secret): masking the scheme word here would orphan the
            // secret into the clear.
            if val.eq_ignore_ascii_case("bearer") || val.eq_ignore_ascii_case("basic") {
                out.push(c);
                i += 1;
                continue;
            }
            if !head.is_empty()
                && is_secret_flag(&head)
                && !val.is_empty()
                && !val.starts_with("//")
            {
                out.push(c);
                // Preserve one skipped whitespace exactly when the input
                // had `key: value` spacing, and the opening quote when the
                // value was quoted (`"key": "[redacted]"`).
                if bytes[i + 1..v].iter().any(|b| b.is_whitespace()) {
                    out.push(' ');
                }
                if let Some(q) = quote {
                    out.push(q);
                }
                out.push_str("[redacted]");
                i = vend;
                continue;
            }
            out.push(c);
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Mask `Bearer <token>` / `Basic <credentials>` (case-insensitive scheme):
/// the secret word becomes `[redacted]`; the scheme itself is kept. Without
/// the Basic arm, `Authorization: Basic dXNlcjpwYXNz` redacted only the
/// scheme word and left the base64 credential in the clear.
pub(crate) fn mask_bearer(s: &str) -> String {
    let mut out = mask_scheme_token(s, "bearer ");
    out = mask_scheme_token(&out, "basic ");
    out
}

/// One scheme pass for `mask_bearer`: keep the scheme casing, mask only a
/// non-empty token word.
fn mask_scheme_token(s: &str, scheme: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.to_ascii_lowercase().find(scheme) {
        let token_start = pos + scheme.len();
        let mut vend = token_start;
        while vend < rest.len() {
            let b = rest.as_bytes()[vend];
            if b.is_ascii_whitespace() || matches!(b, b'"' | b'\'' | b',' | b';' | b')') {
                break;
            }
            vend += 1;
        }
        // Keep the original scheme casing, mask only a non-empty token.
        if vend > token_start {
            out.push_str(&rest[..pos]);
            out.push_str(&rest[pos..token_start]);
            out.push_str("[redacted]");
            rest = &rest[vend..];
        } else {
            out.push_str(&rest[..token_start]);
            rest = &rest[token_start..];
        }
    }
    out.push_str(rest);
    out
}

/// Mask the value word after a bare secret flag (`--token abc123` with no
/// `=`/`:`). Words split on ASCII whitespace; a following word starting
/// with `-` is another flag, never a value.
pub(crate) fn mask_bare_flag_values(s: &str) -> String {
    let mut out = String::new();
    let mut pending_secret = false;
    // Tokenize into alternating word/whitespace spans to preserve spacing.
    let bytes = s.as_bytes();
    let mut spans: Vec<(bool, &str)> = vec![]; // (is_word, text)
    let mut start = 0;
    while start < s.len() {
        let word = !bytes[start].is_ascii_whitespace();
        let mut end = start + 1;
        while end < s.len() && (!bytes[end].is_ascii_whitespace()) == word {
            end += 1;
        }
        spans.push((word, &s[start..end]));
        start = end;
    }
    for (is_word, span) in spans {
        if !is_word {
            out.push_str(span);
            continue;
        }
        if pending_secret {
            pending_secret = false;
            // A flag-looking word was never a value: keep it, re-arm when it
            // is itself a bare secret flag.
            if span.starts_with('-') && span.len() > 1 {
                out.push_str(span);
                if is_secret_flag(span) && !span.contains('=') && !span.contains(':') {
                    pending_secret = true;
                }
                continue;
            }
            out.push_str("[redacted]");
            continue;
        }
        out.push_str(span);
        if is_secret_flag(span) && !span.contains('=') && !span.contains(':') {
            pending_secret = true;
        }
    }
    out
}

/// Concise diagnosis-aligned top-level error for a diagnosed attach
/// failure. Same truth/confidence as the diagnosis code, always prefixed
/// with `attach failed:` so existing prefix matches keep working. The raw
/// adapter text never appears here — it rides in sanitized `cause`.
pub(crate) fn diagnosed_attach_error(code: &str, endpoint: &str) -> String {
    match code {
        "endpoint-not-listening" => {
            format!("attach failed: no debug listener found at {endpoint}")
        }
        "endpoint-closed-during-attach" => {
            format!("attach failed: debug endpoint closed while attaching ({endpoint})")
        }
        "endpoint-rejected" => {
            format!(
                "attach failed: debug endpoint rejected the connection at {endpoint}; \
                 it may already have another debugger client"
            )
        }
        "endpoint-unreachable" => {
            format!(
                "attach failed: could not reach debug endpoint at {endpoint} \
                 (unverified; check host/port)"
            )
        }
        _ => format!("attach failed: could not attach to {endpoint}"),
    }
}

/// True only for the bridge-reported config phase: the connection was
/// established, so the message is semantic, not a transport symptom.
/// Anything else takes the conservative transport path with endpoint
/// diagnosis (corrupt files never reach here — they bail earlier).
pub(crate) fn is_config_phase(phase: Option<&str>) -> bool {
    matches!(phase, Some("config"))
}

/// True only for the bridge-reported runtime phase: an unexpected
/// internal failure after a successful bridge operation. Like config,
/// the message stays top-level verbatim with no endpoint diagnosis and
/// no cause duplication — it is never called endpoint-rejected. The
/// redacted attempted identity and requested endpoint still ride along.
pub(crate) fn is_runtime_phase(phase: Option<&str>) -> bool {
    matches!(phase, Some("runtime"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-debugger-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// One fake session dir: intent names an endpoint, session.json names a
    /// kind + daemon port, lang.json names the adapter.
    fn fake_owner(
        root: &std::path::Path,
        name: &str,
        lang: &str,
        kind: &str,
        host: &str,
        port: serde_json::Value,
        daemon_port: u16,
    ) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lang.json"), format!("{{\"lang\":\"{lang}\"}}")).unwrap();
        std::fs::write(
            dir.join("stops.json"),
            json!({
                "breaks": [], "requestedTarget": {"host": host, "port": port, "pid": Value::Null},
                "target": {"host": host, "port": port},
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.join("session.json"),
            json!({"name": name, "kind": kind, "port": daemon_port}).to_string(),
        )
        .unwrap();
    }

    /// Merge extra keys into a fake session's session.json (updatedAt
    /// control for the liveness corroboration window).
    fn patch_session(root: &std::path::Path, name: &str, patch: Value) {
        let path = root.join(name).join("session.json");
        let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for (k, val) in patch.as_object().unwrap() {
            v[k] = val.clone();
        }
        std::fs::write(&path, v.to_string()).unwrap();
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// One-purpose in-test listener: every connection is read-drained, then
    /// answered once with `reply` (empty reply = close with nothing said).
    /// Returned port is held open by the detached thread for the test.
    fn spawn_stranger(reply: Vec<u8>) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            for mut conn in listener.incoming().flatten() {
                let _ = conn.set_read_timeout(Some(Duration::from_millis(500)));
                let mut buf = [0u8; 4096];
                let _ = conn.read(&mut buf);
                let _ = conn.write_all(&reply);
            }
        });
        port
    }

    /// A listener that accepts and never answers (blackhole stranger).
    fn spawn_blackhole() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                std::thread::sleep(Duration::from_secs(30));
                let _ = conn.shutdown(std::net::Shutdown::Both);
            }
        });
        port
    }

    fn framed(body: &Value) -> Vec<u8> {
        dap::encode_message(body)
    }

    #[test]
    fn cause_redaction_covers_basic_and_quoted_keys() {
        // `Authorization: Basic <b64>` must mask the credential (previously
        // only the scheme word masked, orphaning the secret); JSON-quoted
        // keys (`{"password": "x"}`) must mask the value.
        let basic = sanitize_cause("handshake failed: Authorization: Basic dXNlcjpwYXNz");
        assert!(basic.contains("Basic [redacted]"), "{basic}");
        assert!(!basic.contains("dXNlcjpwYXNz"), "{basic}");
        let bearer = sanitize_cause("denied: Bearer abc123");
        assert!(bearer.contains("Bearer [redacted]"), "{bearer}");
        assert!(!bearer.contains("abc123"), "{bearer}");
        let quoted = sanitize_cause(r#"config {"password": "hunter2"} rejected"#);
        assert!(!quoted.contains("hunter2"), "{quoted}");
        assert!(quoted.contains("[redacted]"), "{quoted}");
        // Non-secrets survive verbatim.
        let plain = sanitize_cause("connection refused on localhost:5678");
        assert!(plain.contains("localhost:5678"), "{plain}");
    }

    #[test]
    fn display_endpoint_brackets_ipv6() {
        assert_eq!(display_endpoint("localhost", 5678), "localhost:5678");
        assert_eq!(display_endpoint("127.0.0.1", 1), "127.0.0.1:1");
        // Bare IPv6 literals bracket so host:port stays unambiguous.
        assert_eq!(display_endpoint("::1", 5678), "[::1]:5678");
        assert_eq!(display_endpoint("2001:db8::1", 9229), "[2001:db8::1]:9229");
        assert_eq!(display_endpoint("[::1]", 5678), "[::1]:5678");
    }

    #[test]
    fn probe_session_bridge_classifies_ports() {
        let t = Duration::from_millis(500);
        // HTTP-shaped garbage: framing validation fails outright —
        // definitive non-debugger (the later EOF would say the same).
        assert_eq!(
            probe_session_bridge(spawn_stranger(b"HTTP/1.0 200 OK\r\n\r\nnope".to_vec()), t),
            Probe::NotOurs("invalid protocol")
        );
        // Immediate EOF with nothing said: nothing serving here.
        assert_eq!(
            probe_session_bridge(spawn_stranger(Vec::new()), t),
            Probe::NotOurs("closed connection")
        );
        // Complete but corrupt frame: definitive non-debugger.
        assert_eq!(
            probe_session_bridge(
                spawn_stranger(b"Content-Length: 5\r\n\r\n{oops".to_vec()),
                t
            ),
            Probe::NotOurs("invalid protocol")
        );
        // Valid framing, wrong shape (no `ok` envelope).
        assert_eq!(
            probe_session_bridge(spawn_stranger(framed(&json!({"hello": 1}))), t),
            Probe::NotOurs("unexpected protocol")
        );
        // Valid debugger envelopes count, success or refusal alike.
        assert_eq!(
            probe_session_bridge(spawn_stranger(framed(&json!({"ok": true, "stops": []}))), t),
            Probe::Ours
        );
        assert_eq!(
            probe_session_bridge(
                spawn_stranger(framed(
                    &json!({"ok": false, "error": "target VM has exited"})
                )),
                t
            ),
            Probe::Ours
        );
        // Nothing listens: definitively gone. The freed port can be
        // grabbed by another parallel test's listener before our probe
        // lands (then it reads as Unclear/Ours instead of refused), so
        // retry with fresh dead ports — a real refused-mapping regression
        // still fails every attempt and trips the assert below.
        let mut refused = false;
        for _ in 0..10 {
            let dead = {
                let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                l.local_addr().unwrap().port()
            };
            if probe_session_bridge(dead, t) == Probe::NotOurs("connection refused") {
                refused = true;
                break;
            }
        }
        assert!(refused, "a genuinely closed port must probe as refused");
        // Blackhole (accepts, never answers): ambiguous, never a verdict.
        assert_eq!(
            probe_session_bridge(spawn_blackhole(), Duration::from_millis(200)),
            Probe::Unclear
        );
    }

    #[test]
    fn attach_exclusivity_matrix_is_capability_aware() {
        // Single-client process adapters collide.
        assert!(attach_exclusive("py"));
        assert!(attach_exclusive("node"));
        assert!(attach_exclusive("java"));
        // Browser CDP multiplexes tabs/clients: never blocked.
        assert!(!attach_exclusive("browser"));
        // Unknown languages fail open (never block what we don't know).
        assert!(!attach_exclusive("mystery"));
        assert!(!attach_exclusive(""));
    }

    #[test]
    fn attach_endpoint_only_for_exclusive_attach() {
        let spec = |lang: &'static str, kind: &'static str, requested: Value| SpawnSpec {
            lang,
            kind,
            bridge_args: vec![],
            wait_secs: 1,
            stops: json!({}),
            requested,
            target_identity: Value::Null,
            identity_hint: String::new(),
        };
        let req = json!({"host": "localhost", "port": 5678, "pid": Value::Null});
        assert_eq!(
            attach_endpoint(&spec("py", "attach", req.clone()))
                .expect("valid endpoint")
                .as_ref()
                .map(|(h, p)| (h.clone(), *p)),
            Some(("localhost".to_string(), 5678))
        );
        // Launch has no pre-known endpoint; browser never collides.
        assert!(attach_endpoint(&spec("py", "launch", req.clone()))
            .unwrap()
            .is_none());
        assert!(attach_endpoint(&spec("browser", "attach", req.clone()))
            .unwrap()
            .is_none());
        assert!(attach_endpoint(&spec("mystery", "attach", req.clone()))
            .unwrap()
            .is_none());
        // Missing/corrupt endpoint reads as absent, never a block.
        assert!(attach_endpoint(&spec("py", "attach", json!({})))
            .unwrap()
            .is_none());
        assert!(
            attach_endpoint(&spec("py", "attach", json!({"host": "h", "port": 99999})))
                .unwrap()
                .is_none()
        );
        // Unspecified destinations fail fast with a clear error (dialing
        // 0.0.0.0 is platform-dependent) — narrow gate, only attach with
        // an unspecified host is affected.
        for bad in ["0.0.0.0", "::", "[::]"] {
            let err = attach_endpoint(&spec(
                "py",
                "attach",
                json!({"host": bad, "port": 5678, "pid": Value::Null}),
            ))
            .expect_err("unspecified destination must fail");
            assert!(format!("{err:#}").contains("unspecified"), "{bad}");
        }
    }

    #[test]
    fn endpoint_scan_finds_only_live_exclusive_owners() {
        let root = tmpdir("endpoint-scan");
        // Live owner: hold a real localhost socket so the daemon probe
        // passes, and point the fake session's daemon port at it.
        let sock = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live = sock.local_addr().unwrap().port();
        fake_owner(
            &root,
            "owner",
            "py",
            "attach",
            "127.0.0.1",
            json!(5678),
            live,
        );
        // Same endpoint via the loopback alias must match.
        assert_eq!(
            find_live_endpoint_owner_in(&root, "new", "127.0.0.1", 5678),
            Some("owner".to_string())
        );
        assert_eq!(
            find_live_endpoint_owner_in(&root, "new", "localhost", 5678),
            Some("owner".to_string())
        );
        // Different port, different host: no block (two endpoints coexist).
        assert!(find_live_endpoint_owner_in(&root, "new", "127.0.0.1", 5679).is_none());
        assert!(find_live_endpoint_owner_in(&root, "new", "example.com", 5678).is_none());
        // The owner's own name is excluded (a session never blocks itself).
        assert!(find_live_endpoint_owner_in(&root, "owner", "127.0.0.1", 5678).is_none());
        // Dead daemon (port 1: nothing listens) never blocks.
        fake_owner(&root, "dead", "py", "attach", "127.0.0.1", json!(9999), 1);
        assert!(find_live_endpoint_owner_in(&root, "new", "127.0.0.1", 9999).is_none());
        // Stale dir without session.json (failed start): no block.
        let stale = root.join("stale");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(
            stale.join("stops.json"),
            r#"{"requestedTarget":{"host":"127.0.0.1","port":7777,"pid":null}}"#,
        )
        .unwrap();
        assert!(find_live_endpoint_owner_in(&root, "new", "127.0.0.1", 7777).is_none());
        // Launch kind with a live daemon and same endpoint still matches
        // (kind attach/launch both count when the endpoint is known).
        fake_owner(
            &root,
            "launcher",
            "py",
            "launch",
            "127.0.0.1",
            json!(8888),
            live,
        );
        assert_eq!(
            find_live_endpoint_owner_in(&root, "new", "127.0.0.1", 8888),
            Some("launcher".to_string())
        );
        // Browser owner on the same port never blocks (shared CDP port,
        // different tab capability).
        fake_owner(
            &root,
            "tab",
            "browser",
            "attach",
            "127.0.0.1",
            json!(9222),
            live,
        );
        fake_owner(&root, "py9222", "py", "attach", "127.0.0.1", json!(9223), 1);
        assert!(find_live_endpoint_owner_in(&root, "new", "127.0.0.1", 9222).is_none());
        drop(sock);
        // Socket closed: the owner reads dead now, no block, no cleanup.
        assert!(find_live_endpoint_owner_in(&root, "new", "127.0.0.1", 5678).is_none());
        assert!(root.join("owner").exists(), "scan must not delete");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn endpoint_scan_skips_noisy_siblings_in_any_order() {
        // Noisy siblings that must never abort the scan: launch-kind intent
        // without an endpoint, malformed stops.json/session.json, an
        // unreadable stops.json (a directory, so read fails for any user),
        // and a starting dir (valid intent, no session.json yet). Every
        // noise entry aborts the pre-fix `?` scan when iterated before the
        // owner, so each fresh root covers oneowner name: across many names
        // at least one owner iterates after noise on any filesystem hash
        // order (read_dir order is never alphabetical/creation order), which
        // makes the pre-fix failure deterministic in practice while the
        // fixed scan passes every root deterministically.
        fn build_noise(root: &std::path::Path) {
            // Launch-kind sibling with no parseable endpoint.
            let dir = root.join("launch-noise");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("lang.json"), r#"{"lang":"py"}"#).unwrap();
            std::fs::write(dir.join("stops.json"), r#"{"requestedTarget":{}}"#).unwrap();
            std::fs::write(
                dir.join("session.json"),
                r#"{"name":"launch-noise","kind":"launch","port":1}"#,
            )
            .unwrap();
            // Malformed stops.json.
            let dir = root.join("malformed-stops");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("stops.json"), "not json").unwrap();
            // Valid intent, malformed session.json.
            let dir = root.join("malformed-session");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("stops.json"),
                r#"{"requestedTarget":{"host":"127.0.0.1","port":5678,"pid":null}}"#,
            )
            .unwrap();
            std::fs::write(dir.join("session.json"), "not json").unwrap();
            // Unreadable stops.json (a directory: read_to_string always fails).
            let dir = root.join("unreadable");
            std::fs::create_dir_all(dir.join("stops.json")).unwrap();
            // Starting dir: valid intent, no session.json yet.
            let dir = root.join("starting");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("stops.json"),
                r#"{"requestedTarget":{"host":"127.0.0.1","port":5678,"pid":null}}"#,
            )
            .unwrap();
        }
        // Owner-first creation order plus one owner-last root per name:
        // iteration order (not creation order) decides who is seen first.
        for owner in [
            "owner", "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
            "india", "juliet", "kilo",
        ] {
            for layout in ["owner-first", "owner-last"] {
                let root = tmpdir(&format!("endpoint-noisy-{layout}-{owner}"));
                let sock = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let live = sock.local_addr().unwrap().port();
                if layout == "owner-first" {
                    fake_owner(&root, owner, "py", "attach", "127.0.0.1", json!(5678), live);
                    build_noise(&root);
                } else {
                    build_noise(&root);
                    fake_owner(&root, owner, "py", "attach", "127.0.0.1", json!(5678), live);
                }
                assert_eq!(
                    find_live_endpoint_owner_in(&root, "new", "127.0.0.1", 5678),
                    Some(owner.to_string()),
                    "live owner must survive noisy siblings ({layout}/{owner})"
                );
                drop(sock);
                let _ = std::fs::remove_dir_all(&root);
            }
        }
    }

    #[test]
    fn endpoint_scan_without_live_owner_is_none() {
        // Only launch/browser-shared/dead/malformed entries: no live
        // exclusive owner, so the scan must return None (never a collision).
        let root = tmpdir("endpoint-no-owner");
        let sock = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live = sock.local_addr().unwrap().port();
        // Browser session on the same port (live daemon, shared CDP port).
        fake_owner(
            &root,
            "tab",
            "browser",
            "attach",
            "127.0.0.1",
            json!(5678),
            live,
        );
        // Dead exclusive owner (nothing listens on port 1).
        fake_owner(&root, "dead", "py", "attach", "127.0.0.1", json!(5678), 1);
        // Malformed siblings.
        let dir = root.join("malformed-stops");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stops.json"), "not json").unwrap();
        let dir = root.join("malformed-session");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("stops.json"),
            r#"{"requestedTarget":{"host":"127.0.0.1","port":5678,"pid":null}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("session.json"), "not json").unwrap();
        // Launch-kind entry with no endpoint.
        let dir = root.join("launcher");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lang.json"), r#"{"lang":"py"}"#).unwrap();
        std::fs::write(dir.join("stops.json"), r#"{"requestedTarget":{}}"#).unwrap();
        assert!(find_live_endpoint_owner_in(&root, "new", "127.0.0.1", 5678).is_none());
        drop(sock);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn endpoint_scan_corroborates_recent_publish_on_probe_failure() {
        // Daemon port is dead in both cases (port 1 refuses fast); only
        // the bridge's publish freshness differs. Recent publish fails
        // closed (a wedged probe must not authorize a second attach onto
        // a live target); an hour-old publish stops blocking.
        let root = tmpdir("endpoint-scan-recent");
        fake_owner(&root, "fresh", "py", "attach", "127.0.0.1", json!(4444), 1);
        patch_session(&root, "fresh", json!({"updatedAt": now_secs() - 10}));
        assert_eq!(
            find_live_endpoint_owner_in(&root, "new", "127.0.0.1", 4444),
            Some("fresh".to_string())
        );
        fake_owner(&root, "old", "py", "attach", "127.0.0.1", json!(4445), 1);
        patch_session(&root, "old", json!({"updatedAt": now_secs() - 3600}));
        assert!(find_live_endpoint_owner_in(&root, "new", "127.0.0.1", 4445).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn endpoint_scan_skips_symlinks_and_bad_names() {
        let root = tmpdir("endpoint-scan-links");
        let sock = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live = sock.local_addr().unwrap().port();
        fake_owner(&root, "owner", "py", "attach", "h", json!(1111), live);
        // A symlink with a colliding name must be skipped, never followed.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("owner"), root.join("evil")).unwrap();
            // "evil" has no intent of its own; even if it resolved, the
            // name is valid — the symlink guard must skip it. Point the
            // real owner elsewhere so any follow would false-positive.
            assert!(find_live_endpoint_owner_in(&root, "new", "h", 2222).is_none());
        }
        // Invalid names never resolve (check_name rejects escapes).
        assert!(
            find_live_endpoint_owner_in(&root, "../owner", "h", 1111) == Some("owner".to_string())
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn attach_classification_covers_four_cases_plus_remote() {
        let ev = |d: &Value| d["evidence"].clone();
        // Closed during attach: present before, gone after.
        let d = attach_diagnosis(Some(true), Some(false), "127.0.0.1:1");
        assert_eq!(d["code"], json!("endpoint-closed-during-attach"));
        assert_eq!(ev(&d)["listenerBefore"], json!(true));
        assert_eq!(ev(&d)["listenerAfter"], json!(false));
        assert!(d["recommendation"]
            .as_str()
            .unwrap()
            .contains("may have exited"));
        // Rejected: still listening after the failure.
        let d = attach_diagnosis(Some(true), Some(true), "h:2");
        assert_eq!(d["code"], json!("endpoint-rejected"));
        assert!(d["recommendation"]
            .as_str()
            .unwrap()
            .contains("another debugger client"));
        // Listener appeared mid-attach: still a refusal, same code.
        let d = attach_diagnosis(Some(false), Some(true), "h:2");
        assert_eq!(d["code"], json!("endpoint-rejected"));
        // Nothing ever listened.
        let d = attach_diagnosis(Some(false), Some(false), "h:3");
        assert_eq!(d["code"], json!("endpoint-not-listening"));
        assert_eq!(d["confidence"], json!("high"));
        // Remote/unknown: generic, low confidence, no certainty claimed.
        for (pre, post) in [(None, None), (None, Some(true)), (Some(true), None)] {
            let d = attach_diagnosis(pre, post, "remote:4");
            assert_eq!(d["code"], json!("endpoint-unreachable"));
            assert_eq!(d["confidence"], json!("low"));
        }
        // Shape contract: every code carries all four fields.
        for (pre, post) in [
            (Some(true), Some(false)),
            (Some(true), Some(true)),
            (Some(false), Some(false)),
            (None, None),
        ] {
            let d = attach_diagnosis(pre, post, "h:5");
            for f in ["code", "confidence", "evidence", "recommendation"] {
                assert!(d.get(f).is_some(), "{f} missing for {pre:?}/{post:?}");
            }
        }
    }

    #[test]
    fn legacy_reclaim_needs_proven_death() {
        // Proven dead: parseable session.json, numeric nonzero port, daemon
        // silent (port 1 refuses, no recent publish).
        let dir = tmpdir("reclaim-dead");
        std::fs::write(
            dir.join("session.json"),
            r#"{"name":"x","kind":"attach","port":1,"updatedAt":1}"#,
        )
        .unwrap();
        assert!(legacy_proven_dead(&dir), "dead daemon + recorded port");
        // Zero port, corrupt file, missing file: unprovable, never reclaim.
        std::fs::write(
            dir.join("session.json"),
            r#"{"name":"x","kind":"attach","port":0}"#,
        )
        .unwrap();
        assert!(!legacy_proven_dead(&dir));
        std::fs::write(dir.join("session.json"), "not json").unwrap();
        assert!(!legacy_proven_dead(&dir));
        std::fs::remove_file(dir.join("session.json")).unwrap();
        assert!(!legacy_proven_dead(&dir));
        // Live daemon (bound socket): never reclaim.
        let sock = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live = sock.local_addr().unwrap().port();
        std::fs::write(
            dir.join("session.json"),
            format!(r#"{{"name":"x","kind":"attach","port":{live}}}"#),
        )
        .unwrap();
        assert!(!legacy_proven_dead(&dir), "live daemon must survive");
        drop(sock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn intent_endpoint_reads_requested_target_only() {
        // Numeric requestedTarget parses; the old `target` summary fallback
        // (string ports) is gone.
        assert_eq!(
            intent_endpoint(&json!({
                "requestedTarget": {"host": "h", "port": 5678},
                "target": {"host": "other", "port": "1234"},
            })),
            Some(("h".to_string(), 5678))
        );
        assert_eq!(
            intent_endpoint(&json!({"target": {"host": "h", "port": "1234"}})),
            None,
            "target-summary fallback removed"
        );
        assert_eq!(
            intent_endpoint(&json!({"requestedTarget": {"host": "h", "port": "12"}})),
            None,
            "string ports removed"
        );
        assert_eq!(intent_endpoint(&json!({})), None);
    }

    #[test]
    fn display_endpoint_caps_host_length() {
        assert_eq!(display_endpoint("h", 1), "h:1");
        let long = "x".repeat(500);
        let shown = display_endpoint(&long, 5678);
        assert!(shown.ends_with(":5678"), "{shown}");
        assert!(
            shown.chars().count() < 200,
            "unbounded CLI host must not bloat the envelope: {shown}"
        );
        assert!(shown.contains("more chars"), "truncation must be marked");
    }

    // ---- schema v2 breaking cutover ----
}
