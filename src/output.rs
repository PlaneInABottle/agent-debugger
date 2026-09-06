use serde_json::Value;
use std::io::Write;

/// Stable output envelope for agent consumption.
///
/// JSON mode (default): `{"ok":true,"command":"status","data":{...}}`
/// or `{"ok":false,"command":"step","error":"..."}`.
/// Human mode (`--human`): pretty data or `error: ...` on stderr.
///
/// Broken-pipe safe: agents paginate/truncate output (`| head`), which
/// closes stdout early. A panic there would mask the real result, so EPIPE
/// exits silently with success (the consumer already has what it needs).
pub fn emit(command: &str, result: anyhow::Result<Value>, human: bool) -> i32 {
    match result {
        Ok(data) => {
            if human {
                if data.is_null() {
                    return write_stdout(format!("ok ({command})\n"), 0);
                }
                match serde_json::to_string_pretty(&data) {
                    Ok(pretty) => write_stdout(format!("{pretty}\n"), 0),
                    Err(e) => {
                        let _ = writeln!(std::io::stderr(), "error: failed to render output: {e}");
                        1
                    }
                }
            } else {
                let envelope = serde_json::json!({
                    "ok": true,
                    "command": command,
                    "data": data,
                });
                write_stdout(format!("{envelope}\n"), 0)
            }
        }
        Err(e) => {
            // A bridge failure may carry additive structured context
            // (wait/capture timeouts carry `waitContext`; diagnosed attach
            // setup failures carry a concise diagnosis-aligned `error`, the
            // sanitized raw adapter text as `cause`, plus `diagnosis` and
            // the redacted `targetIdentity` / `requestedTarget`). Machine
            // diagnosis remains the source of code/confidence/evidence/
            // recommendation; undiagnosed errors are unchanged.
            let failure = e.downcast_ref::<crate::session::BridgeFailure>();
            let wait_context = failure.and_then(|b| b.wait_context.clone());
            let cause = failure.and_then(|b| b.cause.clone());
            let diagnosis = failure.and_then(|b| b.diagnosis.clone());
            let target_identity = failure.and_then(|b| b.target_identity.clone());
            let requested_target = failure.and_then(|b| b.requested_target.clone());
            let message = format!("{e:#}");
            if human {
                let _ = writeln!(std::io::stderr(), "error: {message}");
                // Concise raw cause, only when it adds information beyond
                // the aligned error (never a duplicate line).
                if should_show_cause(&message, cause.as_deref()) {
                    let _ = writeln!(std::io::stderr(), "cause: {}", cause.as_deref().unwrap());
                }
                if let Some(ctx) = &wait_context {
                    match serde_json::to_string_pretty(&serde_json::json!({"waitContext": ctx})) {
                        Ok(pretty) => {
                            let _ = writeln!(std::io::stderr(), "{pretty}");
                        }
                        Err(_) => {}
                    }
                }
                if let Some(d) = &diagnosis {
                    match serde_json::to_string_pretty(&serde_json::json!({"diagnosis": d})) {
                        Ok(pretty) => {
                            let _ = writeln!(std::io::stderr(), "{pretty}");
                        }
                        Err(_) => {}
                    }
                }
                // Concise single-line identities (already redacted upstream:
                // observed argv is masked+capped, requested is host/port
                // only) — one line each, no multi-KB pretty dump.
                if let Some(t) = &target_identity {
                    if let Ok(compact) = serde_json::to_string(&serde_json::json!({
                        "targetIdentity": t
                    })) {
                        let _ = writeln!(std::io::stderr(), "{compact}");
                    }
                }
                if let Some(r) = &requested_target {
                    if let Ok(compact) = serde_json::to_string(&serde_json::json!({
                        "requestedTarget": r
                    })) {
                        let _ = writeln!(std::io::stderr(), "{compact}");
                    }
                }
            } else {
                let envelope = build_error_envelope(
                    command,
                    &message,
                    cause,
                    wait_context,
                    diagnosis,
                    target_identity,
                    requested_target,
                );
                let code = write_stdout(format!("{envelope}\n"), 1);
                return code;
            }
            1
        }
    }
}

/// Show the additive `cause` on human stderr only when it is useful and
/// different: present, non-empty, and not a duplicate of the concise
/// top-level error.
pub(crate) fn should_show_cause(error: &str, cause: Option<&str>) -> bool {
    match cause {
        Some(c) if !c.trim().is_empty() && c != error => true,
        _ => false,
    }
}

/// Machine error envelope: `error` is the display string (concise and
/// diagnosis-aligned for diagnosed attach failures), `cause` rides only
/// when present (sanitized raw adapter text, never duplicated into
/// `error`), then the additive structured context.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_error_envelope(
    command: &str,
    error: &str,
    cause: Option<String>,
    wait_context: Option<Value>,
    diagnosis: Option<Value>,
    target_identity: Option<Value>,
    requested_target: Option<Value>,
) -> Value {
    let mut envelope = serde_json::json!({
        "ok": false,
        "command": command,
        "error": error,
    });
    if let Some(c) = cause {
        envelope["cause"] = Value::String(c);
    }
    if let Some(ctx) = wait_context {
        envelope["waitContext"] = ctx;
    }
    if let Some(d) = diagnosis {
        envelope["diagnosis"] = d;
    }
    if let Some(t) = target_identity {
        envelope["targetIdentity"] = t;
    }
    if let Some(r) = requested_target {
        envelope["requestedTarget"] = r;
    }
    envelope
}

fn write_stdout(text: String, code: i32) -> i32 {
    match std::io::stdout().write_all(text.as_bytes()) {
        Ok(()) => code,
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => 0,
        Err(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cause_shows_only_when_useful_and_different() {
        assert!(should_show_cause(
            "attach failed: x",
            Some("raw adapter text")
        ));
        assert!(!should_show_cause("same", Some("same")));
        assert!(!should_show_cause("attach failed: x", None));
        assert!(!should_show_cause("attach failed: x", Some("")));
        assert!(!should_show_cause("attach failed: x", Some("   ")));
    }

    #[test]
    fn error_envelope_separates_error_cause_and_context() {
        // Diagnosed attach shape: concise error, additive cause, then
        // diagnosis + identities. Cause never duplicates error.
        let v = build_error_envelope(
            "attach",
            "attach failed: no debug listener found at h:9",
            Some("Connection refused".to_string()),
            None,
            Some(json!({"code": "endpoint-not-listening"})),
            Some(json!({"kind": "process"})),
            Some(json!({"host": "h", "port": 9})),
        );
        assert_eq!(v["ok"], json!(false));
        assert_eq!(v["command"], json!("attach"));
        assert!(v["error"].as_str().unwrap().starts_with("attach failed:"));
        assert_eq!(v["cause"], json!("Connection refused"));
        assert_ne!(v["error"], v["cause"]);
        assert_eq!(v["diagnosis"]["code"], json!("endpoint-not-listening"));
        assert_eq!(v["requestedTarget"]["port"], json!(9));
        // Undiagnosed shape unchanged: no cause key at all, never null.
        let plain = build_error_envelope("step", "busy: x", None, None, None, None, None);
        assert_eq!(plain["error"], json!("busy: x"));
        assert!(plain.get("cause").is_none());
        assert!(plain.get("diagnosis").is_none());
    }
}
