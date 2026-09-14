// See `mod.rs` for the one-way dependency DAG.
use super::forward::{bridge_failure, normalize_target_for_lang_opt, stamp_main};
use super::paths::{check_name, checked_session_dir, session_port};
use super::sidecar::{atomic_tmp_name, require_schema_v2, session_lang_opt};
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

/// Short-held kernel file guard serializing stops.json read-modify-write
/// across concurrent CLI invocations (separate processes included — never
/// across bridge forwards, which the bridges serialize themselves).
/// Exclusion comes from the OS (`File::try_lock`, exclusive): the guard
/// owns the open handle, and the lock releases when the handle closes —
/// guard drop or whole-process death alike. A crashed holder therefore
/// never wedges the section: there is no stale protocol, no mtime bound,
/// and no steal. The lock file persists in the session dir (`close` reaps
/// it); nothing is ever written to it and it is never deleted, so every
/// cooperative writer rendezvous on the same inode and no path
/// replacement can admit two holders.
pub(crate) fn breaks_lock_path(dir: &std::path::Path) -> PathBuf {
    dir.join("breaks.lock")
}

/// How long a contended claim waits for a live holder before the caller
/// reports applied-live-but-unpersisted (the existing honest error)
/// instead of risking a torn intent — never spins forever. The section
/// itself is milliseconds.
pub(crate) const BREAKS_LOCK_WAIT: Duration = Duration::from_secs(2);

/// Fixed backoff between `try_lock` polls while a live holder is in the
/// section: short enough to enter promptly, long enough to avoid hot
/// spinning. Pacing only — exclusion never depends on it.
pub(crate) const BREAKS_LOCK_POLL: Duration = Duration::from_millis(5);

/// Owns the open lock-file handle: the kernel exclusive lock is held as
/// long as this guard lives and releases when the handle closes. The path
/// is never deleted — persistence is what keeps every writer on the same
/// inode.
#[derive(Debug)]
pub(crate) struct BreaksGuard {
    _file: std::fs::File,
}

/// Claim the breaks lock, waiting briefly for a live holder. `WouldBlock`
/// polls until the bound; any other lock error, a symlink or non-regular
/// file, or an unopenable path bails with a persistence error (the caller
/// reports applied-live-but-unpersisted). Never spins forever, never
/// deletes or replaces another writer's record.
pub(crate) fn acquire_breaks_lock(dir: &std::path::Path) -> anyhow::Result<BreaksGuard> {
    let path = breaks_lock_path(dir);
    // Refuse a planted link or non-file before opening: otherwise the open
    // below (or `close`'s reaping delete) could be redirected outside the
    // session dir. Best-effort against a swap between check and open — the
    // opened handle is re-verified below, same stance as the dir checks.
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_symlink() => {
            anyhow::bail!("breaks lock must not be a symlink: {}", path.display())
        }
        Ok(m) if !m.file_type().is_file() => {
            anyhow::bail!("breaks lock must be a regular file: {}", path.display())
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => anyhow::bail!("cannot stat breaks lock {}: {e}", path.display()),
    }
    // Read+write+create, never truncate or exclusive: the first creator
    // and every concurrent opener land on one inode; nothing is ever
    // written to it, so a first-create/open race needs no serialization.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)
        .map_err(|e| anyhow::anyhow!("cannot open breaks lock {}: {e}", path.display()))?;
    // The opened handle must be a regular file too (closes the swap-a-link
    // window above short of a swap-back race, accepted as best-effort).
    if !file
        .metadata()
        .map(|m| m.file_type().is_file())
        .unwrap_or(false)
    {
        anyhow::bail!("breaks lock must be a regular file: {}", path.display());
    }
    let deadline = Instant::now() + BREAKS_LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(BreaksGuard { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    anyhow::bail!("breaks lock held by a live writer");
                }
                std::thread::sleep(BREAKS_LOCK_POLL);
            }
            Err(std::fs::TryLockError::Error(e)) => {
                anyhow::bail!("cannot lock breaks lock {}: {e}", path.display())
            }
        }
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
    let tmp = atomic_tmp_name(dir, "stops.json");
    let written = (|| -> anyhow::Result<()> {
        std::fs::write(
            &tmp,
            serde_json::to_string_pretty(&intent).unwrap_or_else(|_| "{}".to_string()),
        )
        .map_err(|e| anyhow::anyhow!("cannot persist session intent: {e}"))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| anyhow::anyhow!("cannot persist session intent: {e}"))?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
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
    let tmp = atomic_tmp_name(dir, "stops.json");
    let written = (|| -> anyhow::Result<()> {
        std::fs::write(
            &tmp,
            serde_json::to_string_pretty(&intent).unwrap_or_else(|_| "{}".to_string()),
        )
        .map_err(|e| anyhow::anyhow!("cannot persist session intent: {e}"))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| anyhow::anyhow!("cannot persist session intent: {e}"))?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
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
        // The kernel lock persists (same inode for every writer) and is
        // unlocked when the guard drops: the file stays, nobody holds it.
        assert!(dir.join("breaks.lock").exists(), "lock file persists");
        assert_no_temp_orphans(&dir);
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
        // Eight threads racing the same identical add behind a barrier
        // (deterministic start, only scheduling varies): the file holds
        // exactly one copy (the kernel lock serializes the
        // read-modify-write so no stale read can duplicate). The lock
        // file persists (same inode for every writer) and no temp file
        // lingers on the success path.
        use std::sync::Barrier;
        let dir = tmpdir("append-race");
        std::fs::write(
            dir.join("stops.json"),
            r#"{"breaks":[],"logpoints":[],"watches":[],"exits":[],
                    "sources":[],"timeout":7}"#,
        )
        .unwrap();
        let barrier = Barrier::new(8);
        let barrier_r = &barrier;
        let dir_r = &dir;
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(move || {
                    barrier_r.wait();
                    append_confirmed_breaks_in(dir_r, &["a.py:1".to_string()]).unwrap();
                });
            }
        });
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("stops.json")).unwrap())
                .unwrap();
        assert_eq!(v["breaks"], json!(["a.py:1"]));
        assert!(dir.join("breaks.lock").exists(), "lock file persists");
        assert_no_temp_orphans(&dir);
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
        assert_no_temp_orphans(&dir);
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
        assert_no_temp_orphans(&dir);
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

    /// Success-path temp hygiene: no quarantine or tmp sibling may linger
    /// (the kernel lock needs no quarantine files; the unique stops.json
    /// tmp is consumed by its rename on every success).
    fn assert_no_temp_orphans(dir: &std::path::Path) {
        let leftovers: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.starts_with(".q-") || n.starts_with("stops.json.tmp.")
                // Legacy fixed-name tmp from pre-unique-tmp versions: a
                // crashed old binary could have left exactly this file.
                || n == "stops.json.tmp"
            })
            .collect();
        assert!(leftovers.is_empty(), "temp files must not linger");
    }

    #[test]
    fn atomic_tmp_names_are_unique_per_call() {
        // Two names for the same file must differ (pid + counter): racing
        // writers never share a tmp, unlike the old fixed stops.json.tmp.
        let dir = tmpdir("breaks-tmp-unique");
        let a = atomic_tmp_name(&dir, "stops.json");
        let b = atomic_tmp_name(&dir, "stops.json");
        assert_ne!(a, b, "tmp names must be unique");
        assert!(a.parent() == Some(dir.as_path()));
        assert!(b.parent() == Some(dir.as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kernel_lock_critical_section_never_overlaps() {
        // Eight threads race behind a barrier, each holding the guard
        // across a widened window. The kernel exclusive lock admits
        // exactly one holder at any instant (max active == 1) —
        // algorithmically, not by timing.
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Barrier;
        let dir = tmpdir("breaks-overlap");
        let barrier = Barrier::new(8);
        let active = AtomicUsize::new(0);
        let max = AtomicUsize::new(0);
        let barrier_r = &barrier;
        let active_r = &active;
        let max_r = &max;
        let dir_r = &dir;
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(move || {
                    barrier_r.wait();
                    let _guard = acquire_breaks_lock(dir_r).unwrap();
                    let cur = active_r.fetch_add(1, Ordering::SeqCst) + 1;
                    max_r.fetch_max(cur, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    active_r.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(
            max.load(Ordering::SeqCst),
            1,
            "stops.json transactions must never overlap"
        );
        assert!(dir.join("breaks.lock").exists(), "lock file persists");
        assert_no_temp_orphans(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kernel_lock_unique_appends_lose_none() {
        // Eight racers behind a barrier, each appending a unique raw:
        // every transaction serializes, so none of the eight is lost.
        use std::sync::Barrier;
        let dir = tmpdir("breaks-unique");
        std::fs::write(
            dir.join("stops.json"),
            r#"{"breaks":[],"logpoints":[],"watches":[],"exits":[],
                    "sources":[],"timeout":7}"#,
        )
        .unwrap();
        let barrier = Barrier::new(8);
        let barrier_r = &barrier;
        let dir_r = &dir;
        std::thread::scope(|s| {
            for i in 0..8 {
                s.spawn(move || {
                    barrier_r.wait();
                    append_confirmed_breaks_in(dir_r, &[format!("f{i}.py:1")]).unwrap();
                });
            }
        });
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("stops.json")).unwrap())
                .unwrap();
        let mut got: Vec<String> = v["breaks"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x.as_str().map(|s| s.to_string()))
            .collect();
        got.sort();
        let mut want: Vec<String> = (0..8).map(|i| format!("f{i}.py:1")).collect();
        want.sort();
        assert_eq!(got, want, "concurrent unique adds must lose none");
        assert!(dir.join("breaks.lock").exists(), "lock file persists");
        assert_no_temp_orphans(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_looking_lock_file_immediately_acquirable() {
        // No stale protocol exists: a lock file with junk content and an
        // hour-old mtime acquires immediately, and the content is never
        // written (no nonce, no metadata).
        let dir = tmpdir("breaks-stale-looking");
        let path = breaks_lock_path(&dir);
        std::fs::write(&path, "crashed-holder-junk").unwrap();
        let old = std::time::SystemTime::now()
            .checked_sub(Duration::from_secs(3600))
            .unwrap_or(std::time::UNIX_EPOCH);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        let _guard = acquire_breaks_lock(&dir).expect("must acquire without stale wait");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "lock content is never written"
        );
        drop(_guard);
        assert!(path.exists(), "lock file persists across release");
        // Still acquirable after release (nothing wedged, nothing deleted).
        let _guard2 = acquire_breaks_lock(&dir).unwrap();
        assert_no_temp_orphans(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn held_lock_second_acquisition_busy_and_inode_preserved() {
        // While one guard holds the file lock, a second open + try_lock
        // reports busy in milliseconds — and the file is neither deleted
        // nor replaced (same bytes; same inode on unix).
        let dir = tmpdir("breaks-busy");
        let path = breaks_lock_path(&dir);
        std::fs::write(&path, "sentinel").unwrap();
        let _holder = acquire_breaks_lock(&dir).unwrap();
        #[cfg(unix)]
        let ino_before = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&path).unwrap().ino()
        };
        let probe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(
            matches!(probe.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
            "second acquisition must report busy while held"
        );
        drop(probe);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "sentinel",
            "held record must survive"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().ino(),
                ino_before,
                "inode must survive contention"
            );
        }
        drop(_holder);
        // Released: immediately acquirable again.
        let _guard2 = acquire_breaks_lock(&dir).unwrap();
        assert_no_temp_orphans(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn second_acquire_times_out_on_live_holder() {
        // The full bounded wait honors the live-holder bound instead of
        // stealing: a held lock makes a second claim fail (same thread,
        // separate handle — exclusion is per-handle, not per-thread).
        let dir = tmpdir("breaks-timeout");
        let _holder = acquire_breaks_lock(&dir).unwrap();
        let start = Instant::now();
        let err = acquire_breaks_lock(&dir).unwrap_err();
        assert!(
            format!("{err:#}").contains("live writer"),
            "timeout must report a live holder: {err:#}"
        );
        assert!(
            start.elapsed() >= Duration::from_millis(1900),
            "the 2s bound must be honored"
        );
        drop(_holder);
        let _guard2 = acquire_breaks_lock(&dir).unwrap();
        assert_no_temp_orphans(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lock_releases_on_drop() {
        // Dropping the guard closes the handle (automatic unlock) while
        // the file persists for the next writer.
        let dir = tmpdir("breaks-drop");
        let path = breaks_lock_path(&dir);
        {
            let _guard = acquire_breaks_lock(&dir).unwrap();
            assert!(path.exists());
        }
        assert!(path.exists(), "drop must not delete the lock file");
        let _guard2 = acquire_breaks_lock(&dir).expect("must reacquire right after drop");
        assert_no_temp_orphans(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crash_style_child_exit_releases_lock() {
        // Crash-style proof across processes: a child holds the lock and
        // dies without unlocking (killed); the OS releases the kernel
        // lock, so the parent acquires afterwards. The child is this same
        // test binary re-executed with an env flag (exact-test match).
        const ENV: &str = "AGENT_DEBUGGER_BREAKS_LOCK_CHILD";
        if let Ok(child_dir) = std::env::var(ENV) {
            let child_dir = PathBuf::from(child_dir);
            let _guard = acquire_breaks_lock(&child_dir).expect("child must acquire");
            std::fs::write(child_dir.join("ready"), "held").unwrap();
            std::thread::sleep(Duration::from_secs(30));
            return; // killed first in practice; a normal return drops alike
        }
        let dir = tmpdir("breaks-crash-child");
        let path = breaks_lock_path(&dir);
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
            .unwrap();
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(&exe)
            .arg("--exact")
            .arg("session::breaks::tests::crash_style_child_exit_releases_lock")
            .env(ENV, &dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn child holder");
        let ready = dir.join("ready");
        let start = Instant::now();
        while !ready.exists() {
            if start.elapsed() > Duration::from_secs(10) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("child never signaled the held lock");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // Cross-process exclusion: the child holds, so the parent sees busy.
        let probe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let busy = matches!(probe.try_lock(), Err(std::fs::TryLockError::WouldBlock));
        drop(probe);
        // Kill without unlocking, reap, then acquire (killed before every
        // assertion that can fail, so no orphan holder lingers).
        let _ = child.kill();
        let _ = child.wait();
        assert!(busy, "parent must see WouldBlock while the child holds");
        let _guard =
            acquire_breaks_lock(&dir).expect("kernel must release the lock on child death");
        assert!(path.exists(), "lock file persists across the crash");
        assert_no_temp_orphans(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_breaks_lock_refused_target_untouched() {
        // A planted symlink at the lock path is refused before opening;
        // the link stays a link and the target bytes are untouched (no
        // lock is ever taken through it, nothing is written or deleted).
        let dir = tmpdir("breaks-symlink");
        let target = dir.join("target");
        std::fs::write(&target, "sentinel").unwrap();
        std::os::unix::fs::symlink(&target, breaks_lock_path(&dir)).unwrap();
        let err = acquire_breaks_lock(&dir).unwrap_err();
        assert!(
            format!("{err:#}").contains("symlink"),
            "symlink must be refused: {err:#}"
        );
        assert!(
            std::fs::symlink_metadata(breaks_lock_path(&dir))
                .unwrap()
                .file_type()
                .is_symlink(),
            "the planted link must survive"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "sentinel",
            "the link target must be untouched"
        );
        assert_no_temp_orphans(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nonregular_breaks_lock_refused() {
        // A directory (or any non-regular file) at the lock path is
        // refused, never opened or locked, and left in place.
        let dir = tmpdir("breaks-nonregular");
        std::fs::create_dir(breaks_lock_path(&dir)).unwrap();
        let err = acquire_breaks_lock(&dir).unwrap_err();
        assert!(
            format!("{err:#}").contains("regular file"),
            "non-regular must be refused: {err:#}"
        );
        assert!(
            std::fs::symlink_metadata(breaks_lock_path(&dir))
                .unwrap()
                .file_type()
                .is_dir(),
            "the non-regular entry must survive"
        );
        assert_no_temp_orphans(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
