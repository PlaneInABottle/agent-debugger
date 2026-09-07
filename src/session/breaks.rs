// See `mod.rs` for the one-way dependency DAG.
use super::forward::{bridge_failure, normalize_target_for_lang_opt, stamp_main};
use super::paths::{check_name, checked_session_dir, session_port, startup_nonce};
use super::sidecar::{require_schema_v2, session_lang_opt};
use crate::client;
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

/// Add line breakpoints to a live session, persisting only what the bridge
/// confirms. Forward bound: min(10 + 5*N, 65)s for N raw specs (each bridge
/// round-trip is bounded server-side). Transport failure persists nothing —
/// the bridge may still have applied, so the error points at bare `breaks`.
/// Bridge ok:false also persists nothing (zero confirmed by contract).
/// Only a non-main `target` selects an ephemeral target-scoped break:
/// forwarded verbatim, never persisted to stops.json (no inheritance).
/// Explicit `--target main` is the global path (same persistence as
/// omitted) — bridges treat `main` as global intent, never ephemeral.
pub fn cmd_breaks_add(
    name: &str,
    breaks: &[String],
    target: Option<&str>,
) -> anyhow::Result<Value> {
    check_name(name)?;
    let dir = checked_session_dir(name)?;
    require_schema_v2(&dir, name)?;
    let target = normalize_target_for_lang_opt(session_lang_opt(&dir).as_deref(), target)?;
    let port = session_port(name)?;
    let secs = std::cmp::min(10 + 5 * breaks.len() as u64, 65);
    let mut body = serde_json::json!({"cmd": "breaksAdd", "breaks": breaks});
    if let Some(t) = target.as_deref() {
        body["target"] = Value::String(t.to_string());
    }
    let resp = client::request(port, &body, Duration::from_secs(secs)).map_err(|e| {
        anyhow::anyhow!(
            "breaks add may or may not have applied ({e:#}) — persistence skipped; \
             run bare `breaks` to check live stops"
        )
    })?;
    if !resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Err(bridge_failure(&resp));
    }
    if is_ephemeral_target(target.as_deref()) {
        return Ok(stamp_main(resp));
    }
    let confirmed: Vec<String> = resp
        .get("added")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| e.get("raw").and_then(|r| r.as_str()).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if !confirmed.is_empty() {
        append_confirmed_breaks(name, &confirmed).map_err(|e| applied_live_err("add", e))?;
    }
    Ok(stamp_main(resp))
}

/// Remove live line breakpoints, dropping only what the bridge confirms.
/// Same transport contract as add: transport failure persists nothing (the
/// bridge may still have applied — bare `breaks` reconciles), ok:false
/// persists nothing. Partial success persists the confirmed subset and
/// keeps the bridge's warning. Only a non-main `target` scopes removal to
/// that target's target-scoped records (never touching stops.json);
/// explicit `--target main` is the global path like omitted.
pub fn cmd_breaks_remove(
    name: &str,
    breaks: &[String],
    target: Option<&str>,
) -> anyhow::Result<Value> {
    check_name(name)?;
    let dir = checked_session_dir(name)?;
    require_schema_v2(&dir, name)?;
    let target = normalize_target_for_lang_opt(session_lang_opt(&dir).as_deref(), target)?;
    let port = session_port(name)?;
    let secs = std::cmp::min(10 + 5 * breaks.len() as u64, 65);
    let mut body = serde_json::json!({"cmd": "breaksRemove", "breaks": breaks});
    if let Some(t) = target.as_deref() {
        body["target"] = Value::String(t.to_string());
    }
    let resp = client::request(port, &body, Duration::from_secs(secs)).map_err(|e| {
        anyhow::anyhow!(
            "breaks remove may or may not have applied ({e:#}) — persistence skipped; \
             run bare `breaks` to check live stops"
        )
    })?;
    if !resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Err(bridge_failure(&resp));
    }
    if is_ephemeral_target(target.as_deref()) {
        return Ok(stamp_main(resp));
    }
    let removed = confirmed_removed(&resp);
    if !removed.is_empty() {
        remove_confirmed_breaks(name, &removed).map_err(|e| applied_live_err("remove", e))?;
    }
    Ok(stamp_main(resp))
}

/// Drop all live line breakpoints (logpoints/watches/exits untouched).
/// Persistence mirrors remove: only the bridge-confirmed `removed` raws
/// leave stops.json. Forward bound matches remove policy (min(10 + 5*N,
/// 65)s over the persisted line-break count — logpoints/watches/exits are
/// not clear scope and never inflate the bound). Only a non-main `target`
/// drops that target's ephemeral target-scoped records (no stops.json
/// change); omitted and explicit `--target main` are the full line-break
/// reset.
pub fn cmd_breaks_clear(name: &str, target: Option<&str>) -> anyhow::Result<Value> {
    check_name(name)?;
    let dir = checked_session_dir(name)?;
    require_schema_v2(&dir, name)?;
    let target = normalize_target_for_lang_opt(session_lang_opt(&dir).as_deref(), target)?;
    let port = session_port(name)?;
    let secs = std::cmp::min(10 + 5 * persisted_line_breaks(name), 65);
    let mut body = serde_json::json!({"cmd": "breaksClear"});
    if let Some(t) = target.as_deref() {
        body["target"] = Value::String(t.to_string());
    }
    let resp = client::request(port, &body, Duration::from_secs(secs)).map_err(|e| {
        anyhow::anyhow!(
            "breaks clear may or may not have applied ({e:#}) — persistence skipped; \
             run bare `breaks` to check live stops"
        )
    })?;
    if !resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Err(bridge_failure(&resp));
    }
    if is_ephemeral_target(target.as_deref()) {
        return Ok(stamp_main(resp));
    }
    let removed = confirmed_removed(&resp);
    if !removed.is_empty() {
        remove_confirmed_breaks(name, &removed).map_err(|e| applied_live_err("clear", e))?;
    }
    Ok(stamp_main(resp))
}

/// True when a normalized target selects ephemeral target-scoped records
/// (persisted intent untouched). Only non-main targets are ephemeral:
/// omitted and explicit `main` are the global path on every bridge.
pub(crate) fn is_ephemeral_target(target: Option<&str>) -> bool {
    matches!(target, Some(t) if t != "main")
}

/// Count of persisted line breaks for the clear forward bound. Reads the
/// stops.json breaks list; method:/exc: startup forms are not line breaks
/// and logpoints/watches/exits live in other lists, so none of them count.
/// Unreadable intent bounds as zero (floor 10s still applies).
pub(crate) fn persisted_line_breaks(name: &str) -> u64 {
    match checked_session_dir(name) {
        Ok(d) => persisted_line_breaks_in(&d),
        Err(_) => 0,
    }
}

pub(crate) fn persisted_line_breaks_in(dir: &std::path::Path) -> u64 {
    let raw = match std::fs::read_to_string(dir.join("stops.json")) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let intent: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    intent
        .get("breaks")
        .and_then(|b| b.as_array())
        .map(|a| {
            a.iter()
                .filter(|v| {
                    let s = v.as_str().unwrap_or("");
                    let head = s.split('|').next().unwrap_or("");
                    !(head.starts_with("method:") || head == "exc" || head.starts_with("exc:"))
                })
                .count() as u64
        })
        .unwrap_or(0)
}

/// Persisted stored raws the bridge confirms removed. The bridge echoes the/// exact strings it stored at add/startup time, so Rust drops precisely the
/// confirmed persisted entries (never the request spelling).
pub(crate) fn confirmed_removed(resp: &Value) -> Vec<String> {
    resp.get("removed")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| e.get("raw").and_then(|r| r.as_str()).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// A live break mutation applied on the bridge, then intent persistence
/// failed: say so explicitly (bare `breaks` shows live truth; resume intent
/// in stops.json is now stale until resynced) instead of a bare I/O error
/// that reads as "nothing happened".
pub(crate) fn applied_live_err(op: &str, e: anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "breaks {op} applied live but intent persistence failed ({e:#}) — \
         run bare `breaks` to check live stops; resume intent is stale until stops.json is fixed"
    )
}

/// Short-held file guard serializing stops.json read-modify-write across
/// concurrent CLI invocations (same create_new + nonce + stale-steal idiom
/// as the startup lock, but scoped to the breaks file section only — never
/// across bridge forwards, which the bridges serialize themselves). Lives
/// inside the session dir so `close` reaps it; a crashed holder is
/// recoverable after a seconds-scale bound (the section itself is ms).
pub(crate) fn breaks_lock_path(dir: &std::path::Path) -> PathBuf {
    dir.join("breaks.lock")
}

/// A breaks lock is stale only when its mtime is provably older than the
/// bound. Unreadable clocks fail closed (treat as live) — a retry costs a
/// wait, a wrongful steal costs a sibling writer its update.
pub(crate) const BREAKS_LOCK_STALE: Duration = Duration::from_secs(30);

pub(crate) struct BreaksGuard {
    path: PathBuf,
    nonce: String,
}

impl Drop for BreaksGuard {
    fn drop(&mut self) {
        if std::fs::read_to_string(&self.path)
            .map(|c| c == self.nonce)
            .unwrap_or(false)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Claim the breaks lock, waiting briefly for a live holder. On failure
/// the caller reports applied-live-but-unpersisted (the existing honest
/// error) instead of risking a torn intent — never spins forever.
pub(crate) fn acquire_breaks_lock(dir: &std::path::Path) -> anyhow::Result<BreaksGuard> {
    let path = breaks_lock_path(dir);
    let nonce = startup_nonce();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                use std::io::Write as _;
                f.write_all(nonce.as_bytes())
                    .map_err(|e| anyhow::anyhow!("cannot write breaks lock: {e}"))?;
                return Ok(BreaksGuard { path, nonce });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => anyhow::bail!("cannot create breaks lock: {e}"),
        }
        let stale = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
            .map(|age| age >= BREAKS_LOCK_STALE)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("breaks lock held by a live writer");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Append confirmed raw specs to stops.json's breaks list. Atomic tmp+rename
/// in the same dir; every other field (timeout, target, unknown) is preserved
/// byte-for-byte in value (only the breaks array grows).
pub(crate) fn append_confirmed_breaks(name: &str, raws: &[String]) -> anyhow::Result<()> {
    append_confirmed_breaks_in(&checked_session_dir(name)?, raws)
}

pub(crate) fn append_confirmed_breaks_in(
    dir: &std::path::Path,
    raws: &[String],
) -> anyhow::Result<()> {
    let _guard = acquire_breaks_lock(dir)?;
    let path = dir.join("stops.json");
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("cannot read session intent: {e}"))?;
    let mut intent: Value =
        serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("corrupt session intent: {e}"))?;
    let list = intent
        .get_mut("breaks")
        .and_then(|b| b.as_array_mut())
        .ok_or_else(|| anyhow::anyhow!("corrupt session intent: breaks is not a list"))?;
    // Exact-raw dedup: concurrent identical adds (or a retried forward the
    // bridge already applied) must not grow the list — the bridge is the
    // source of truth for what is armed, this file only records it once.
    for r in raws {
        if !list.iter().any(|v| v.as_str() == Some(r)) {
            list.push(Value::String(r.clone()));
        }
    }
    let tmp = dir.join("stops.json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(&intent).unwrap_or_else(|_| "{}".to_string()),
    )
    .map_err(|e| anyhow::anyhow!("cannot persist session intent: {e}"))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| anyhow::anyhow!("cannot persist session intent: {e}"))?;
    Ok(())
}

/// Drop confirmed-removed raws from stops.json's breaks list. Exact-string
/// match against persisted entries, ALL occurrences per removed raw (a
/// legacy file may hold duplicates from before idempotent adds: leaving
/// one behind would ghost-resurrect the intent). Every other field is
/// preserved in value; the write is atomic tmp+rename like the append path.
pub(crate) fn remove_confirmed_breaks(name: &str, raws: &[String]) -> anyhow::Result<()> {
    remove_confirmed_breaks_in(&checked_session_dir(name)?, raws)
}

pub(crate) fn remove_confirmed_breaks_in(
    dir: &std::path::Path,
    raws: &[String],
) -> anyhow::Result<()> {
    let _guard = acquire_breaks_lock(dir)?;
    let path = dir.join("stops.json");
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("cannot read session intent: {e}"))?;
    let mut intent: Value =
        serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("corrupt session intent: {e}"))?;
    let list = intent
        .get_mut("breaks")
        .and_then(|b| b.as_array_mut())
        .ok_or_else(|| anyhow::anyhow!("corrupt session intent: breaks is not a list"))?;
    for r in raws {
        list.retain(|v| v.as_str() != Some(r.as_str()));
    }
    let tmp = dir.join("stops.json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(&intent).unwrap_or_else(|_| "{}".to_string()),
    )
    .map_err(|e| anyhow::anyhow!("cannot persist session intent: {e}"))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| anyhow::anyhow!("cannot persist session intent: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-debugger-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn append_confirmed_breaks_dedups_exact_raws() {
        let dir = tmpdir("append-dedup");
        std::fs::write(
            dir.join("stops.json"),
            r#"{"breaks":["a.py:1"],"logpoints":[],"watches":[],"exits":[],
                    "sources":[],"timeout":7}"#,
        )
        .unwrap();
        append_confirmed_breaks_in(&dir, &["a.py:1".to_string(), "b.py:2|x>1".to_string()])
            .unwrap();
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("stops.json")).unwrap())
                .unwrap();
        assert_eq!(v["breaks"], json!(["a.py:1", "b.py:2|x>1"]));
        assert!(!dir.join("breaks.lock").exists(), "lock never lingers");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_confirmed_breaks_drops_legacy_duplicates() {
        let dir = tmpdir("remove-dups");
        std::fs::write(
            dir.join("stops.json"),
            r#"{"breaks":["a.py:1","a.py:1","b.py:2"],"logpoints":[],"watches":[],
                    "exits":[],"sources":[],"timeout":7}"#,
        )
        .unwrap();
        remove_confirmed_breaks_in(&dir, &["a.py:1".to_string()]).unwrap();
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("stops.json")).unwrap())
                .unwrap();
        // All copies go (a leftover would ghost-resurrect the intent).
        assert_eq!(v["breaks"], json!(["b.py:2"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn breaks_lock_serializes_concurrent_appends() {
        // Eight threads racing the same identical add: the file holds
        // exactly one copy (lock serializes the read-modify-write so no
        // stale read can duplicate).
        let dir = tmpdir("append-race");
        std::fs::write(
            dir.join("stops.json"),
            r#"{"breaks":[],"logpoints":[],"watches":[],"exits":[],
                    "sources":[],"timeout":7}"#,
        )
        .unwrap();
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    append_confirmed_breaks_in(&dir, &["a.py:1".to_string()]).unwrap();
                });
            }
        });
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("stops.json")).unwrap())
                .unwrap();
        assert_eq!(v["breaks"], json!(["a.py:1"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ephemeral_target_is_non_main_only() {
        // Omitted and explicit main are the global path (persisted);
        // only child/worker targets skip stops.json persistence.
        assert!(!is_ephemeral_target(None));
        assert!(!is_ephemeral_target(Some("main")));
        assert!(is_ephemeral_target(Some("child:1")));
        assert!(is_ephemeral_target(Some("worker:abc")));
    }

    #[test]
    fn appended_breaks_preserve_intent_fields() {
        // Atomic append grows only the breaks list: timeout/target/unknown
        // fields survive, and no tmp file is left behind.
        let dir = tmpdir("append");
        std::fs::write(
            dir.join("stops.json"),
            r#"{"breaks":["a.py:1"],"logpoints":[],"watches":[],"exits":[],
                    "sources":[],"timeout":7,"target":{"program":"a.py"},
                    "futureField":{"nested":true}}"#,
        )
        .unwrap();
        append_confirmed_breaks_in(&dir, &["b.py:2".to_string(), "c.py:3|x > 1".to_string()])
            .unwrap();
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("stops.json")).unwrap())
                .unwrap();
        assert_eq!(v["breaks"], json!(["a.py:1", "b.py:2", "c.py:3|x > 1"]));
        assert_eq!(v["timeout"], json!(7));
        assert_eq!(v["target"]["program"], json!("a.py"));
        assert_eq!(v["futureField"]["nested"], json!(true));
        assert!(!dir.join("stops.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_rejects_missing_or_non_list_breaks() {
        let dir = tmpdir("append-bad");
        std::fs::write(dir.join("stops.json"), r#"{"timeout":7}"#).unwrap();
        assert!(append_confirmed_breaks_in(&dir, &["b.py:2".to_string()]).is_err());
        std::fs::write(dir.join("stops.json"), r#"{"breaks":"nope"}"#).unwrap();
        assert!(append_confirmed_breaks_in(&dir, &["b.py:2".to_string()]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn removed_breaks_drop_only_confirmed_entries() {
        let dir = tmpdir("remove");
        std::fs::write(
            dir.join("stops.json"),
            r#"{"breaks":["a.py:1","b.py:2|x>1","c.py:3"],"logpoints":[],"watches":[],"exits":[],
                    "sources":[],"timeout":7,"target":{"program":"a.py"},
                    "requestedTarget":{"host":"h","port":1,"pid":null}}"#,
        )
        .unwrap();
        remove_confirmed_breaks_in(
            &dir,
            &["b.py:2|x>1".to_string(), "missing.py:9".to_string()],
        )
        .unwrap();
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("stops.json")).unwrap())
                .unwrap();
        assert_eq!(v["breaks"], json!(["a.py:1", "c.py:3"]));
        assert_eq!(v["timeout"], json!(7));
        assert_eq!(v["target"]["program"], json!("a.py"));
        assert!(v.get("requestedTarget").is_some());
        assert!(!dir.join("stops.json.tmp").exists());
        // Unknown raws are a no-op, not an error.
        remove_confirmed_breaks_in(&dir, &["nope.py:1".to_string()]).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persisted_line_breaks_counts_only_line_scope() {
        // Line breaks count; method:/exc: startup forms and the sibling
        // logpoint/watch/exit lists never inflate the clear bound.
        let dir = tmpdir("clear-count");
        std::fs::write(
            dir.join("stops.json"),
            r#"{"breaks":["a.py:1","method:C.m","exc","exc:E","b.py:2|x>1"],
                    "logpoints":["a.py:3:t"],"watches":["C.f"],"exits":["C.m"],
                    "timeout":7}"#,
        )
        .unwrap();
        assert_eq!(persisted_line_breaks_in(&dir), 2);
        std::fs::write(dir.join("stops.json"), r#"{"timeout":7}"#).unwrap();
        assert_eq!(persisted_line_breaks_in(&dir), 0);
        assert_eq!(persisted_line_breaks("definitely-no-such-session-xyz"), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn confirmed_removed_reads_bridge_echo() {
        let resp = json!({"ok": true, "removed": [{"raw": "a.py:1"}, {"raw": "b.py:2|x>1"}]});
        assert_eq!(confirmed_removed(&resp), vec!["a.py:1", "b.py:2|x>1"]);
        assert!(confirmed_removed(&json!({"ok": true})).is_empty());
    }

    #[test]
    fn persistence_failure_says_applied_live() {
        for op in ["add", "remove", "clear"] {
            let e = applied_live_err(op, anyhow::anyhow!("cannot persist session intent: boom"));
            let msg = format!("{e:#}");
            assert!(
                msg.contains("applied live but intent persistence failed"),
                "{msg}"
            );
            assert!(msg.contains(op), "{msg}");
            assert!(msg.contains("boom"), "{msg}");
        }
    }
}
