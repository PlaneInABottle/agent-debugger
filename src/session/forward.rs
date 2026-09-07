// See `mod.rs` for the one-way dependency DAG.
use super::identity::layered_identity_or_null;
use super::paths::{check_name, checked_session_dir, session_port};
use super::sidecar::{require_schema_v2, session_lang_opt};
use crate::client;
use serde_json::{json, Value};
use std::time::Duration;

/// A bridge-reported failure. `message` is the display string: for
/// diagnosed attach setup failures this is the concise diagnosis-aligned
/// `attach failed: …` statement (never the raw adapter text); the raw
/// adapter message rides separately in sanitized `cause`. Undiagnosed
/// failures keep the exact bridge error string verbatim (timeout prefixes
/// like `timeout: no stop within Ns` are preserved verbatim for
/// compatibility); `wait_context` carries the bridge's additive
/// `waitContext` (wait/capture timeouts only) through the CLI to the
/// JSON/human error envelope. Attach setup failures additionally carry
/// `diagnosis` (`{code, confidence, evidence, recommendation}`), the
/// redacted attempted `target_identity`, and the redacted
/// `requested_target` endpoint. Display is the message alone so every
/// existing string match keeps working.
#[derive(Debug)]
pub struct BridgeFailure {
    pub message: String,
    pub cause: Option<String>,
    pub wait_context: Option<Value>,
    pub diagnosis: Option<Value>,
    pub target_identity: Option<Value>,
    pub requested_target: Option<Value>,
}

impl std::fmt::Display for BridgeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for BridgeFailure {}

/// Build the CLI-side error for a `{ok:false}` bridge response: the message
/// stays the display string, a present `waitContext` object rides along as
/// structured context (anything else reads as absent, never fabricated).
pub(crate) fn bridge_failure(resp: &Value) -> anyhow::Error {
    let message = resp
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown bridge error")
        .to_string();
    let wait_context = resp.get("waitContext").filter(|v| v.is_object()).cloned();
    BridgeFailure {
        message,
        cause: None,
        wait_context,
        diagnosis: None,
        target_identity: None,
        requested_target: None,
    }
    .into()
}

/// Forward one command to the session; map {ok:false} to Err.
/// Both adapters speak the same session protocol, so forwarding is
/// language-agnostic (the session's lang only selects the adapter process).
/// Single-target bridges (java/browser) never named a target: stamp main
/// so every served response carries one uniformly.
pub fn forward(name: &str, body: &Value, timeout: Duration) -> anyhow::Result<Value> {
    forward_target(name, body, timeout, None)
}

/// Routing variant over an optional language: `None` (missing/corrupt
/// lang.json) and unrecognized values forward like multi-target langs.
/// Display callers keep `session_lang_in` (java default); every routing
/// call site uses this so unknown never misroutes as main-only.
/// Omitted target stays auto-select (`None`); explicit `main` is pinned as
/// `Some("main")` on multi-target/unknown langs so bridges serve main
/// instead of auto-selecting a recently stopped child/worker. Main-only
/// adapters (java/browser) accept explicit `main` as a no-op (`None`: they
/// predate the target field and can only ever serve main) and reject
/// anything else before any forward.
pub(crate) fn normalize_target_for_lang_opt(
    lang: Option<&str>,
    target: Option<&str>,
) -> anyhow::Result<Option<String>> {
    match (lang, target) {
        (_, None) => Ok(None),
        (Some("java") | Some("browser"), Some("main")) => Ok(None),
        (Some("java") | Some("browser"), Some(other)) => {
            anyhow::bail!(
                "unsupported target '{other}' (session is {}, main-only)",
                lang.unwrap()
            )
        }
        (_, Some("main")) => Ok(Some("main".to_string())),
        (_, Some(other)) => Ok(Some(other.to_string())),
    }
}

/// Every served response names its target. Multi-target bridges echo the
/// selected id themselves; main-only bridges predate the field, so stamp
/// `main` here (they can only ever serve main).
pub(crate) fn stamp_main(mut resp: Value) -> Value {
    if let Value::Object(ref mut m) = resp {
        m.entry("target".to_string())
            .or_insert(Value::String("main".to_string()));
    }
    resp
}

/// Forward one command with an optional target selector. The selector rides
/// in the framed body (`"target"`); bridges resolve omit-to-auto-select
/// (most recent stopped live target, else main) and echo the served target.
pub fn forward_target(
    name: &str,
    body: &Value,
    timeout: Duration,
    target: Option<&str>,
) -> anyhow::Result<Value> {
    check_name(name)?;
    let dir = checked_session_dir(name)?;
    require_schema_v2(&dir, name)?;
    let target = normalize_target_for_lang_opt(session_lang_opt(&dir).as_deref(), target)?;
    let port = session_port(name)?;
    let mut body = body.clone();
    if let Some(t) = target.as_deref() {
        if let Value::Object(ref mut m) = body {
            m.insert("target".to_string(), Value::String(t.to_string()));
        }
    }
    let resp = client::request(port, &body, timeout)?;
    if resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        Ok(stamp_main(resp))
    } else {
        Err(bridge_failure(&resp))
    }
}

/// Reload the page and wait for the next stop. Browser tabs only (CLI
/// help): gate before any forward so py/node/java fail fast with a stable
/// browser-only error and never spend bridge traffic. Missing, corrupt, or
/// unknown langs fail the same way (a reload is never forwarded elsewhere).
pub fn cmd_reload(name: &str, timeout: u64) -> anyhow::Result<Value> {
    check_name(name)?;
    let dir = checked_session_dir(name)?;
    require_schema_v2(&dir, name)?;
    reload_gate(session_lang_opt(&dir).as_deref())?;
    forward(
        name,
        &json!({"cmd": "reload", "timeout": timeout}),
        Duration::from_secs(timeout.saturating_add(5)),
    )
}

/// Offline-testable reload routing decision: only browser forwards.
pub(crate) fn reload_gate(lang: Option<&str>) -> anyhow::Result<()> {
    match lang {
        Some("browser") => Ok(()),
        _ => anyhow::bail!("reload is browser-only (browser attach sessions only)"),
    }
}

/// List debug targets in this session. Multi-target bridges (py/node, plus
/// unknown/missing langs which forward rather than assume main-only) own
/// their roster; main-only adapters (java/browser) get a uniform main-only
/// roster built from the bridge-maintained session.json (no new bridge
/// protocol needed before Batch3). A dead bridge still errors via forward —
/// no fabricated roster — preserving dead-session behavior.
pub fn cmd_targets(name: &str) -> anyhow::Result<Value> {
    check_name(name)?;
    let dir = checked_session_dir(name)?;
    require_schema_v2(&dir, name)?;
    if targets_use_local_roster(&dir) {
        return Ok(cmd_targets_in(&dir));
    }
    Ok(stamp_main(forward(
        name,
        &json!({"cmd": "targets"}),
        Duration::from_secs(10),
    )?))
}

/// Routing decision for `cmd_targets` (explicit dir so unit tests exercise
/// it without touching the real sessions dir): true = serve the local
/// main-only roster, false = forward to the bridge.
pub(crate) fn targets_use_local_roster(dir: &std::path::Path) -> bool {
    matches!(
        session_lang_opt(dir).as_deref(),
        Some("java") | Some("browser")
    )
}

/// Main-only roster from a session dir (explicit dir so unit tests exercise
/// it without touching the real sessions dir).
pub(crate) fn cmd_targets_in(dir: &std::path::Path) -> Value {
    let parsed: Value = std::fs::read_to_string(dir.join("session.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(json!({}));
    let stopped = parsed
        .get("stopped")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    json!({
        "ok": true,
        "targets": [{
            "id": "main",
            "kind": "main",
            "pid": Value::Null,
            "state": if stopped { "stopped" } else { "running" },
            "lastStop": parsed.get("lastStop").cloned().unwrap_or(Value::Null),
            "scope": "global",
        }],
        "selected": "main",
        "ignored": 0,
        "droppedExited": 0,
        "target": "main",
        "targetIdentity": layered_identity_or_null(parsed.get("targetIdentity").cloned()),
    })
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

    #[test]
    fn target_normalize_keeps_main_and_rejects_on_single_target_langs() {
        // Omitted target is auto-select on every lang.
        assert_eq!(
            normalize_target_for_lang_opt(Some("java"), None).unwrap(),
            None
        );
        assert_eq!(
            normalize_target_for_lang_opt(Some("py"), None).unwrap(),
            None
        );
        assert_eq!(normalize_target_for_lang_opt(None, None).unwrap(), None);
        // Explicit main is accepted everywhere but only pinned on
        // multi-target/unknown langs; main-only adapters keep the legacy
        // no-op (they can only ever serve main).
        assert_eq!(
            normalize_target_for_lang_opt(Some("browser"), Some("main")).unwrap(),
            None
        );
        assert_eq!(
            normalize_target_for_lang_opt(Some("java"), Some("main")).unwrap(),
            None
        );
        assert_eq!(
            normalize_target_for_lang_opt(Some("py"), Some("main")).unwrap(),
            Some("main".to_string())
        );
        assert_eq!(
            normalize_target_for_lang_opt(Some("node"), Some("main")).unwrap(),
            Some("main".to_string())
        );
        assert_eq!(
            normalize_target_for_lang_opt(None, Some("main")).unwrap(),
            Some("main".to_string())
        );
        assert_eq!(
            normalize_target_for_lang_opt(Some("go"), Some("main")).unwrap(),
            Some("main".to_string())
        );
        // Multi-target langs pass selectors through for bridge validation.
        assert_eq!(
            normalize_target_for_lang_opt(Some("py"), Some("child:7")).unwrap(),
            Some("child:7".to_string())
        );
        assert_eq!(
            normalize_target_for_lang_opt(Some("node"), Some("worker:abc")).unwrap(),
            Some("worker:abc".to_string())
        );
        // Main-only adapters fail fast instead of silently serving main.
        assert!(normalize_target_for_lang_opt(Some("java"), Some("child:7")).is_err());
        assert!(normalize_target_for_lang_opt(Some("browser"), Some("worker:abc")).is_err());
        // Missing/unknown languages forward for bridge-side validation.
        assert_eq!(
            normalize_target_for_lang_opt(None, Some("child:7")).unwrap(),
            Some("child:7".to_string())
        );
        assert_eq!(
            normalize_target_for_lang_opt(Some("go"), Some("child:7")).unwrap(),
            Some("child:7".to_string())
        );
    }

    #[test]
    fn target_normalize_explicit_main_differs_from_omitted() {
        // The A1 contract: explicit `--target main` must not collapse into
        // auto-select on multi-target langs (a recently stopped child would
        // otherwise serve vars/context/continue instead of main).
        for lang in [Some("py"), Some("node"), None, Some("mystery")] {
            assert_eq!(
                normalize_target_for_lang_opt(lang, Some("main")).unwrap(),
                Some("main".to_string()),
                "explicit main pins main on {lang:?}"
            );
            assert_eq!(
                normalize_target_for_lang_opt(lang, None).unwrap(),
                None,
                "omitted still auto-selects on {lang:?}"
            );
        }
        // Main-only langs: explicit main stays accepted and cheap.
        for lang in [Some("java"), Some("browser")] {
            assert_eq!(
                normalize_target_for_lang_opt(lang, Some("main")).unwrap(),
                None,
                "explicit main is a no-op on {lang:?}"
            );
        }
    }

    #[test]
    fn reload_gate_is_browser_only() {
        assert!(reload_gate(Some("browser")).is_ok());
        for lang in [
            Some("py"),
            Some("node"),
            Some("java"),
            None,
            Some("mystery"),
        ] {
            let err = reload_gate(lang).expect_err("non-browser reload must fail");
            assert!(
                format!("{err:#}").contains("browser-only"),
                "gate names the browser-only contract on {lang:?}: {err:#}"
            );
        }
    }

    #[test]
    fn targets_in_reports_uniform_main_roster() {
        let dir = tmpdir("targets-main");
        std::fs::write(
            dir.join("session.json"),
            r#"{"name":"x","kind":"attach","port":0,"stopped":true,
                    "lastStop":{"file":"a.java","line":3,"method":"m"},
                    "schemaVersion":2,
                    "targetIdentity":{"debuggee":null,"endpoint":null,"adapter":null}}"#,
        )
        .unwrap();
        let v = cmd_targets_in(&dir);
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["targets"].as_array().unwrap().len(), 1);
        let t = &v["targets"][0];
        assert_eq!(t["id"], json!("main"));
        assert_eq!(t["kind"], json!("main"));
        assert_eq!(t["state"], json!("stopped"));
        assert_eq!(t["lastStop"]["line"], json!(3));
        assert_eq!(t["scope"], json!("global"));
        // No flat identity key on the roster entry; layered roles ride top-level.
        assert!(t.get("observed").is_none());
        assert!(v["targetIdentity"].is_object());
        assert_eq!(v["selected"], json!("main"));
        assert_eq!(v["ignored"], json!(0));
        assert_eq!(v["droppedExited"], json!(0));
        assert_eq!(v["target"], json!("main"));
        // Missing session file degrades to an unknown-shape main row.
        let dir = tmpdir("targets-empty");
        let v = cmd_targets_in(&dir);
        assert_eq!(v["targets"][0]["state"], json!("running"));
        assert!(v["targets"][0]["lastStop"].is_null());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_lang_forwards_instead_of_assuming_java() {
        // Missing/corrupt lang.json: routing forwards.
        let dir = tmpdir("lang-missing");
        assert!(session_lang_opt(&dir).is_none());
        assert!(!targets_use_local_roster(&dir));
        assert_eq!(
            normalize_target_for_lang_opt(session_lang_opt(&dir).as_deref(), Some("child:7"))
                .unwrap(),
            Some("child:7".to_string())
        );
        std::fs::write(dir.join("lang.json"), r#"{"lang":"py"}"#).unwrap();
        assert!(!targets_use_local_roster(&dir));
        std::fs::write(dir.join("lang.json"), r#"{"lang":"mystery"}"#).unwrap();
        assert!(!targets_use_local_roster(&dir));
        assert_eq!(
            normalize_target_for_lang_opt(session_lang_opt(&dir).as_deref(), Some("child:7"))
                .unwrap(),
            Some("child:7".to_string())
        );
        // Known main-only langs still gate.
        std::fs::write(dir.join("lang.json"), r#"{"lang":"java"}"#).unwrap();
        assert!(targets_use_local_roster(&dir));
        assert!(
            normalize_target_for_lang_opt(session_lang_opt(&dir).as_deref(), Some("child:7"))
                .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bridge_failure_carries_wait_context_with_prefix_intact() {
        // wait/capture timeout: the message keeps the exact frozen prefix,
        // the additive waitContext rides along structurally (unknown cause,
        // never a fabricated trigger verdict).
        let resp = json!({
            "ok": false,
            "error": "timeout: no stop within 2s; target identity: tab ?",
            "target": "main",
            "waitContext": {
                "waitStartedAt": 1735689600,
                "waitedMs": 2000,
                "triggerStatus": "unknown",
                "targetIdentity": {"debuggee": Value::Null},
                "note": "external trigger execution is not observed",
            },
        });
        let err = bridge_failure(&resp);
        assert_eq!(
            format!("{err:#}"),
            "timeout: no stop within 2s; target identity: tab ?"
        );
        let bf = err.downcast_ref::<BridgeFailure>().expect("typed failure");
        let ctx = bf.wait_context.as_ref().expect("waitContext carried");
        assert_eq!(ctx["triggerStatus"], json!("unknown"));
        assert_eq!(ctx["waitedMs"], json!(2000));
        // Non-object waitContext reads as absent, never fabricated.
        let plain = bridge_failure(&json!({"ok": false, "error": "busy: x"}));
        assert_eq!(format!("{plain:#}"), "busy: x");
        assert!(plain
            .downcast_ref::<BridgeFailure>()
            .unwrap()
            .wait_context
            .is_none());
        // Missing error still maps (legacy shape), without context.
        let missing = bridge_failure(&json!({"ok": false}));
        assert_eq!(format!("{missing:#}"), "unknown bridge error");
        assert!(missing
            .downcast_ref::<BridgeFailure>()
            .unwrap()
            .wait_context
            .is_none());
    }
}
