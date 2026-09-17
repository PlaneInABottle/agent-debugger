// See `mod.rs` for the one-way dependency DAG.
use super::attach::{probe_session_bridge, published_recently, Probe};
use super::forward::forward_target;
use super::identity::{layered_identity_or_null, stops_armed};
use super::locks::{quarantine_verified, stale_startup_mtime, startup_lock_path};
use super::paths::{
    check_dir_real, check_name, checked_session_dir, read_session_file, real_dir_for_delete,
    sessions_dir, SessionReadError,
};
use super::sidecar::{cli_markers_v2, session_lang_opt, SCHEMA_VERSION};
use crate::client;
use serde_json::{json, Value};
use std::time::Duration;
use std::time::Instant;

/// Pre-delete guard for `close` over an unpublished session dir (no
/// session.json yet): the dir may belong to an actively starting bridge
/// whose handshake has not published. Fresh, live, or undatable startup
/// locks bail with a stable retryable "starting" error and the dir is
/// untouched. A provably stale lock is detached-and-verified (a freshly
/// replaced lock never matches the stale bytes, so it is never deleted)
/// and close proceeds. A session.json that appears at any point takes the
/// normal bridge path — close re-reads it fresh after this gate, so a
/// starter that just published is closed, never deleted mid-handshake.
/// No sleeps: every branch decides on current filesystem state.
pub(crate) fn startup_close_gate(
    sessions_root: &std::path::Path,
    name: &str,
) -> anyhow::Result<()> {
    // Published sessions take the normal bridge path regardless of any
    // lock file (a lock may legitimately still be held at publish time).
    if sessions_root.join(name).join("session.json").exists() {
        return Ok(());
    }
    let lock = startup_lock_path(sessions_root, name);
    let raw = match super::locks::read_lock_bytes(&lock) {
        Ok(Some(r)) => r,
        // Vanished under us: nothing live left to protect — proceed.
        Ok(None) => return Ok(()),
        Err(_) => anyhow::bail!("session '{name}' is starting; retry shortly"),
    };
    // Snapshot the mtime with the bytes: a lock reclaimed under us reads
    // as gone (nothing live left to protect — proceed), an undatable one
    // as live (fail closed).
    let mtime = match super::locks::lock_mtime(&lock) {
        Ok(Some(t)) => t,
        Ok(None) => return Ok(()),
        Err(_) => anyhow::bail!("session '{name}' is starting; retry shortly"),
    };
    let stale = stale_startup_mtime(mtime);
    if !stale {
        anyhow::bail!("session '{name}' is starting; retry shortly");
    }
    // Provably stale: drop only the exact verified-stale bytes. A lock
    // replaced under us (fresh starter) mismatches and is left intact —
    // close still bails so the new starter keeps its dir.
    if quarantine_verified(&lock, &raw) {
        return Ok(());
    }
    anyhow::bail!("session '{name}' is starting; retry shortly");
}

/// Re-fetch location + threads + frames without resuming, with the cached
/// redacted identity attached (same object as the start/attach response).
/// An optional target selector routes to one parked target; identity stays
/// session-level (requested/targetIdentity are main-focused, like status).
pub fn cmd_context_target(name: &str, target: Option<&str>) -> anyhow::Result<Value> {
    check_name(name)?;
    let mut resp = forward_target(
        name,
        &json!({"cmd": "context"}),
        Duration::from_secs(10),
        target,
    )?;
    let dir = checked_session_dir(name)?;
    let stops_raw = std::fs::read_to_string(dir.join("stops.json")).ok();
    let requested = stops_raw
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v.get("requestedTarget").cloned())
        .unwrap_or(Value::Null);
    let identity = std::fs::read_to_string(dir.join("session.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v.get("targetIdentity").cloned());
    let identity = layered_identity_or_null(identity);
    if let Value::Object(ref mut m) = resp {
        m.insert("requestedTarget".to_string(), requested);
        m.insert("targetIdentity".to_string(), identity);
    }
    Ok(resp)
}

/// Delete a session dir at close time, unless it is unpublished and a
/// starter claimed the name since the gate: a fresh lock reads as an
/// active start (retryable, dir intact), never a leftover to reap. This
/// narrows the gate-to-delete window (which spans the bridge forward) to
/// the instant before deletion. Published dirs proceed only when the caller
/// took the published path (bridge close attempted); a caller on the
/// unpublished path (`expect_unpublished`) bails if a `session.json`
/// appeared since — a starter published mid-close and owns the session
/// now, so deleting would reap a live handshake (retry takes the bridge
/// path instead).
pub(crate) fn remove_unpublished_dir(
    sessions_root: &std::path::Path,
    name: &str,
    dir: &std::path::Path,
    expect_unpublished: bool,
) -> anyhow::Result<()> {
    if !dir.join("session.json").exists() && startup_lock_path(sessions_root, name).exists() {
        anyhow::bail!("session '{name}' is starting; retry shortly");
    }
    if expect_unpublished && dir.join("session.json").exists() {
        anyhow::bail!("session '{name}' is starting; retry shortly");
    }
    // Immediate lstat-before-delete (best-effort, not a TOCTOU proof): the
    // gate above ran before the bridge forward, so a path swapped to a
    // symlink in between must never be followed by the recursive delete.
    if !real_dir_for_delete(dir, "cannot remove session")? {
        anyhow::bail!("cannot remove session: {} does not exist", dir.display());
    }
    std::fs::remove_dir_all(dir).map_err(|e| anyhow::anyhow!("cannot remove session: {e}"))?;
    Ok(())
}

/// Foreign-probe close seam: the listener is proven not ours, so the dir
/// is dropped unconfirmed — but only after the same immediate
/// lstat-before-delete as above (the 65 s close round-trip plus the probe
/// window passed since entry validation, so a swapped-in symlink must
/// refuse here, never delete through). Separated from
/// `remove_unpublished_dir` (no startup-lock recheck: a port was read, so
/// the session published) so tests exercise this exact deletion without
/// network waits.
pub(crate) fn remove_dir_after_foreign_probe(dir: &std::path::Path) -> anyhow::Result<()> {
    if !real_dir_for_delete(dir, "cannot remove session")? {
        anyhow::bail!("cannot remove session: {} does not exist", dir.display());
    }
    std::fs::remove_dir_all(dir).map_err(|e| anyhow::anyhow!("cannot remove session: {e}"))?;
    Ok(())
}

/// Close a session; retain management state if the daemon stays alive.
pub fn close(name: &str) -> anyhow::Result<Value> {
    checked_session_dir(name)?;
    close_in(&sessions_dir()?, name)
}

/// Testable close over an explicit sessions root (same logic as `close`,
/// without resolving the real sessions dir): enables the deterministic
/// close matrix on isolated tmp roots without touching the real
/// `~/.agent-debugger`.
///
/// Decided matrix (breaking, authorized): only a missing `session.json`
/// with no live starter cleans the stale unpublished dir. IO failures,
/// JSON corruption, and missing/invalid ports preserve the dir with an
/// actionable error (session path + next step) — never an automatic
/// delete, PID kill, or force mode.
pub(crate) fn close_in(sessions_root: &std::path::Path, name: &str) -> anyhow::Result<Value> {
    check_name(name)?;
    let dir = sessions_root.join(name);
    check_dir_real(&dir)?;
    // Startup-window guard: an unpublished dir may belong to an actively
    // starting bridge (no session.json yet). A live starter keeps its dir
    // with a retryable error; only a lock-free or provably-stale-lock dir
    // proceeds to the normal path below (which re-reads session.json
    // fresh, so a starter that just published is closed via its bridge).
    startup_close_gate(sessions_root, name)?;
    // Whether the bridge ACKed the close. A false here (with the port
    // already dead) means the daemon died on its own; a false with the port
    // alive means it is wedged — the dir is still removed, but the caller
    // sees the difference instead of a fabricated success.
    let mut confirmed = false;
    let session_file = dir.join("session.json");
    let port: Option<u16> = match read_session_file(&dir) {
        Ok(v) => match v
            .get("port")
            .and_then(|p| p.as_u64())
            .and_then(|p| u16::try_from(p).ok())
        {
            Some(p) => Some(p),
            None => anyhow::bail!(
                "cannot close session '{name}': session file {} has no valid port; \
                 check {}, fix or remove it explicitly, then retry",
                session_file.display(),
                session_file.display(),
            ),
        },
        // Missing session.json with no live starter (the gate above already
        // bailed on a live lock): stale unpublished dir, cleaned below.
        Err(SessionReadError::NotFound { .. }) => None,
        // IO failures and corruption preserve the dir with an actionable
        // error instead of falling through to the delete path.
        Err(e) => anyhow::bail!(
            "cannot close session '{name}': {e}; \
             check {}, fix or remove it explicitly, then retry",
            session_file.display(),
        ),
    };
    if let Some(port) = port {
        // Generous timeout: bridges serve one command at a time, so a close
        // behind a blocking step/continue queues until that command finishes.
        // A short timeout here used to orphan the daemon (client gave up,
        // queued frame never read, session dir already gone). Normal closes
        // still answer in milliseconds; the bound only covers the worst case.
        if let Ok(resp) = client::request(port, &json!({"cmd": "close"}), Duration::from_secs(65)) {
            // An actual ACK, not just any framed reply: {ok:false} means
            // the bridge refused (still alive by definition).
            confirmed = resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        }
        if !confirmed {
            // No valid close ACK: ask the port who it is before wedging.
            // A stranger (or dead listener) is never our bridge — drop the
            // dir unconfirmed instead of preserving it for a retry that can
            // never succeed. Never signal/kill by port; a live bridge that
            // refused, or an ambiguous (timed-out) probe, keeps the old
            // death-poll path below so a valid session is never deleted on
            // uncertainty.
            match probe_session_bridge(port, Duration::from_secs(2)) {
                Probe::NotOurs(_) => {
                    remove_dir_after_foreign_probe(&dir)?;
                    return Ok(json!({"closed": name, "confirmed": false, "target": "main"}));
                }
                Probe::Ours | Probe::Unclear => {}
            }
        }
        // Then make sure the daemon actually exited before dropping the dir.
        let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut observed_dead = false;
        while Instant::now() < deadline {
            if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err() {
                observed_dead = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        // A refused close, or a daemon still listening after 15s, is not
        // a confirmed close — even though the dir is removed below.
        confirmed = confirmed && observed_dead;
        if !observed_dead {
            anyhow::bail!(
                "session '{name}' still listens on port {port}; state preserved for retry"
            );
        }
    }
    if !dir.exists() {
        anyhow::bail!("no session '{name}'");
    }
    remove_unpublished_dir(sessions_root, name, &dir, port.is_none())?;
    Ok(json!({"closed": name, "confirmed": confirmed, "target": "main"}))
}

/// CLI status: binary version plus known sessions with liveness probes.
/// `stopped`/`lastStop`/`updatedAt` come straight from the bridge-maintained
/// session.json (rewritten on every stop/resume/exit), so a compacted agent
/// sees where each session is parked without any prior memory.
pub fn status() -> Value {
    let mut sessions = Vec::new();
    // status() stays infallible (frozen row contract): without a usable
    // HOME there is no namespace to list, so the row list stays empty.
    if let Ok(root) = sessions_dir() {
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    && checked_session_dir(&entry.file_name().to_string_lossy()).is_ok()
                {
                    sessions.push(session_entry(&entry.path()));
                }
            }
        }
    }
    sessions.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "sessions": sessions,
    })
}

/// One session's status row from its dir. Takes an explicit dir (not a name)
/// so unit tests exercise it without touching the real sessions dir.
/// Never errors a row: unparseable files read as nulls, never fabricated.
/// Exact frozen contract — current rows keep every existing key, drop
/// `observedTarget`, and add `stale`/`unsupported`/`hint`:
/// - current: v2 CLI markers plus (when session.json is present) a v2
///   session marker; `stale:false, unsupported:false`, `hint:null` when the
///   recorded port serves our debugger, otherwise a bounded non-secret
///   stranger note (`hint` names the port + reason, never secrets).
///   `alive` is protocol-aware (valid debugger envelope), never bare TCP.
/// - old/unsupported: anything else (incl. a present session.json lacking
///   `schemaVersion==2`, even when the CLI markers are v2); `stale:true,
///   unsupported:true, hint:"close '<name>' and recreate (unsupported
///   schema v1)"`, with `lang` from lang.json else `"unknown"`, `port`
///   numeric-u16 else `0`, `alive` by the same protocol-aware probe iff
///   port nonzero, `kind`/`stopped`/`lastStop`/`updatedAt` from parseable
///   `kind`/`stopped`/`lastStop`/`updatedAt` from parseable session.json
///   else `null`, `armed`/`target`/`requestedTarget` from parseable
///   stops.json else nulls, `targetIdentity` when a valid layered object
///   else `null` (never flat, never promoted).
pub(crate) fn session_entry(dir: &std::path::Path) -> Value {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let session_raw = std::fs::read_to_string(dir.join("session.json")).ok();
    let parsed: Option<Value> = session_raw
        .as_ref()
        .and_then(|raw| serde_json::from_str(raw).ok());
    let session_v2 = match (&session_raw, &parsed) {
        (None, _) => true, // absent session.json: startup row, not a verdict
        (Some(_), Some(v)) => {
            v.as_object()
                .and_then(|m| m.get("schemaVersion").and_then(|s| s.as_u64()))
                == Some(SCHEMA_VERSION)
        }
        (Some(_), None) => false,
    };
    let current = cli_markers_v2(dir) && session_v2;
    let live = parsed.clone().unwrap_or(json!({}));
    // Checked conversion (like session_port): a corrupt huge port must not
    // truncate into a live-looking probe of an unrelated service.
    let port = live
        .get("port")
        .and_then(|p| p.as_u64())
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or(0);
    // Protocol-aware liveness (not bare TCP): a stranger reusing the port
    // after our bridge died must never read as live. Bound matches the old
    // TCP probe (≤300ms per row) so status latency is unchanged. Ambiguous
    // (timeout) fails closed toward live only with a recent bridge publish
    // — the same corroboration daemon_alive_in uses.
    let (alive, alive_hint) = if port == 0 {
        (false, None)
    } else {
        match probe_session_bridge(port, Duration::from_millis(300)) {
            Probe::Ours => (true, None),
            Probe::NotOurs(reason) => (
                false,
                Some(format!(
                    "port {port} is not this session's debugger ({reason}); close to clear"
                )),
            ),
            Probe::Unclear => {
                if published_recently(&live, Duration::from_secs(120)) {
                    (true, None)
                } else {
                    (false, None)
                }
            }
        }
    };
    // Resume intent: armed counts + target, derived at spawn. Unparseable
    // intent reads as nulls — honest ("unknown"), never fabricated zeros.
    let stops_parsed: Option<Value> = std::fs::read_to_string(dir.join("stops.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok());
    let (armed, target) = match &stops_parsed {
        Some(v) => (
            Some(stops_armed(v)),
            v.get("target").cloned().unwrap_or(Value::Null),
        ),
        None => (None, Value::Null),
    };
    // Identity: requested from the spawn-time intent, layered roles from
    // the bridge-persisted session.json cache (valid layered object or
    // null — flat/malformed never echoes).
    let requested = stops_parsed
        .as_ref()
        .and_then(|v| v.get("requestedTarget").cloned())
        .unwrap_or(Value::Null);
    let identity = layered_identity_or_null(live.get("targetIdentity").cloned());
    let lang = session_lang_opt(dir).unwrap_or_else(|| "unknown".to_string());
    if current {
        json!({
            "name": name,
            "lang": lang,
            "kind": live.get("kind").cloned().unwrap_or(Value::Null),
            "port": port,
            "alive": alive,
            "stopped": live.get("stopped").cloned().unwrap_or(Value::Null),
            "lastStop": live.get("lastStop").cloned().unwrap_or(Value::Null),
            "updatedAt": live.get("updatedAt").cloned().unwrap_or(Value::Null),
            "armed": armed,
            "target": target,
            "requestedTarget": requested,
            "targetIdentity": identity,
            "stale": false,
            "unsupported": false,
            "hint": alive_hint.map(Value::String).unwrap_or(Value::Null),
        })
    } else {
        json!({
            "name": name,
            "lang": lang,
            "kind": live.get("kind").cloned().unwrap_or(Value::Null),
            "port": port,
            "alive": alive,
            "stopped": live.get("stopped").cloned().unwrap_or(Value::Null),
            "lastStop": live.get("lastStop").cloned().unwrap_or(Value::Null),
            "updatedAt": live.get("updatedAt").cloned().unwrap_or(Value::Null),
            "armed": armed,
            "target": target,
            "requestedTarget": requested,
            "targetIdentity": identity,
            "stale": true,
            "unsupported": true,
            "hint": format!("close '{name}' and recreate (unsupported schema v1)"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dap;
    use crate::session::forward::cmd_targets_in;
    use crate::session::identity::{
        cached_target_identity, IDENTITY_FIELD_CAP, IDENTITY_TOTAL_CAP,
    };
    use crate::session::locks::STARTUP_LOCK_STALE;
    use std::path::PathBuf;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-debugger-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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

    fn framed(body: &Value) -> Vec<u8> {
        dap::encode_message(body)
    }

    /// Write v2 CLI-owned markers into a fake session dir.
    fn write_v2_markers(dir: &std::path::Path, lang: &str) {
        std::fs::write(
            dir.join("lang.json"),
            format!("{{\"lang\":\"{lang}\",\"schemaVersion\":2}}"),
        )
        .unwrap();
        std::fs::write(
            dir.join("stops.json"),
            r#"{"breaks":[],"logpoints":[],"watches":[],"exits":[],
                    "sources":[],"timeout":7,"schemaVersion":2,
                    "target":{},"requestedTarget":{}}"#,
        )
        .unwrap();
    }

    /// A current-schema dir whose port serves garbage: alive:false with a
    /// bounded non-secret hint (never true on TCP-only).
    #[test]
    fn session_entry_stranger_port_is_not_alive() {
        let dir = tmpdir("entry-stranger");
        let port = spawn_stranger(b"nope".to_vec());
        std::fs::write(
            dir.join("session.json"),
            format!(
                r#"{{"name":"x","kind":"launch","port":{port},"stopped":false,"schemaVersion":2}}"#
            ),
        )
        .unwrap();
        std::fs::write(dir.join("lang.json"), r#"{"lang":"py","schemaVersion":2}"#).unwrap();
        std::fs::write(
            dir.join("stops.json"),
            r#"{"breaks":[],"logpoints":[],"watches":[],"exits":[],
                    "sources":[],"timeout":7,"schemaVersion":2}"#,
        )
        .unwrap();
        let v = session_entry(&dir);
        assert_eq!(v["unsupported"], json!(false));
        assert_eq!(v["alive"], json!(false));
        let hint = v["hint"].as_str().expect("stranger rows carry a hint");
        assert!(hint.contains(&port.to_string()), "{hint}");
        assert!(hint.contains("not this session's debugger"), "{hint}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Same dir shape against a valid-envelope mimic: alive:true, hint null.
    #[test]
    fn session_entry_valid_bridge_is_alive() {
        let dir = tmpdir("entry-valid");
        let port = spawn_stranger(framed(&json!({"ok": true, "stops": []})));
        std::fs::write(
            dir.join("session.json"),
            format!(
                r#"{{"name":"x","kind":"launch","port":{port},"stopped":false,"schemaVersion":2}}"#
            ),
        )
        .unwrap();
        std::fs::write(dir.join("lang.json"), r#"{"lang":"py","schemaVersion":2}"#).unwrap();
        std::fs::write(
            dir.join("stops.json"),
            r#"{"breaks":[],"logpoints":[],"watches":[],"exits":[],
                    "sources":[],"timeout":7,"schemaVersion":2}"#,
        )
        .unwrap();
        let v = session_entry(&dir);
        assert_eq!(v["alive"], json!(true));
        assert!(v["hint"].is_null());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_entry_surfaces_resume_fields() {
        let dir = tmpdir("entry");
        std::fs::write(
            dir.join("session.json"),
            r#"{"name":"x","kind":"launch","port":1,"stopped":true,
                    "lastStop":{"file":"a.py","line":7,"method":"f"},
                    "updatedAt":123}"#,
        )
        .unwrap();
        std::fs::write(dir.join("lang.json"), r#"{"lang":"py"}"#).unwrap();
        std::fs::write(
            dir.join("stops.json"),
            r#"{"breaks":["a.py:7"],"logpoints":[],"watches":[],"exits":[],
                    "target":{"program":"a.py"}}"#,
        )
        .unwrap();
        let v = session_entry(&dir);
        assert_eq!(v["lang"], json!("py"));
        assert_eq!(v["stopped"], json!(true));
        assert_eq!(v["lastStop"]["line"], json!(7));
        assert_eq!(v["updatedAt"], json!(123));
        assert_eq!(v["armed"]["breaks"], json!(1));
        assert_eq!(v["target"]["program"], json!("a.py"));
        assert_eq!(v["alive"], json!(false)); // port 1: nothing listens
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_entry_legacy_is_honest_nulls() {
        let dir = tmpdir("entry-legacy");
        std::fs::write(
            dir.join("session.json"),
            r#"{"name":"old","kind":"attach","port":0,"stopped":false}"#,
        )
        .unwrap();
        let v = session_entry(&dir);
        assert_eq!(v["lang"], json!("unknown")); // missing sidecar: unknown
        assert_eq!(v["lastStop"], Value::Null);
        assert_eq!(v["armed"], Value::Null);
        assert_eq!(v["alive"], json!(false)); // port 0: no probe
        assert_eq!(v["unsupported"], json!(true));
        assert_eq!(v["stale"], json!(true));
        assert!(v.get("observedTarget").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_entry_rejects_oversize_port() {
        // A corrupt huge port must not truncate into a live-looking probe
        // (u16 wrap would turn 65537 into a probe of port 1).
        let dir = tmpdir("entry-bigport");
        std::fs::write(
            dir.join("session.json"),
            r#"{"name":"x","kind":"attach","port":65537,"stopped":false}"#,
        )
        .unwrap();
        let v = session_entry(&dir);
        assert_eq!(v["port"], json!(0));
        assert_eq!(v["alive"], json!(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_unpublished_dir_guards_fresh_claims() {
        // Unpublished dir + fresh lock: retryable bail, dir and lock
        // intact. Unpublished dir, no lock: proceeds and removes.
        // Published dir + lock: proceeds (normal bridge path owns it).
        let root = tmpdir("close-rm-unpublished");
        let name = "demo";
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = startup_lock_path(&root, name);
        std::fs::write(&lock, "fresh-starter").unwrap();
        let err = remove_unpublished_dir(&root, name, &dir, true).unwrap_err();
        assert!(
            format!("{err:#}").contains("session 'demo' is starting; retry shortly"),
            "{err:#}"
        );
        assert!(dir.is_dir(), "claimed dir preserved");
        assert!(lock.exists(), "fresh lock preserved");
        std::fs::remove_file(&lock).unwrap();
        remove_unpublished_dir(&root, name, &dir, true).expect("lock-free proceeds");
        assert!(!dir.exists(), "unclaimed dir removed");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session.json"), r#"{"port":1}"#).unwrap();
        std::fs::write(&lock, "fresh-starter").unwrap();
        remove_unpublished_dir(&root, name, &dir, false).expect("published proceeds");
        assert!(!dir.exists(), "published dir removed");
        // Just-published dir on the unpublished path: a starter won the
        // race mid-close — bail retryably, dir and session file intact.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session.json"), r#"{"port":1}"#).unwrap();
        let err = remove_unpublished_dir(&root, name, &dir, true).unwrap_err();
        assert!(
            format!("{err:#}").contains("session 'demo' is starting; retry shortly"),
            "{err:#}"
        );
        assert!(
            dir.join("session.json").exists(),
            "just-published file preserved"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    #[cfg(unix)]
    fn close_deletion_seams_refuse_symlink_swap() {
        // Both close deletions, exercised exactly (no close() network
        // waits: the 65 s close round-trip + probe + 15 s death poll never
        // run): swap the session dir for a symlink to an outside dir just
        // before each seam, assert the seam bails and the outside target
        // stays intact (sentinel present, link itself never followed).
        let seams: [(
            &str,
            fn(&std::path::Path, &str, &std::path::Path) -> anyhow::Result<()>,
        ); 2] = [
            ("remove_unpublished_dir", |root, name, dir| {
                remove_unpublished_dir(root, name, dir, false)
            }),
            ("remove_dir_after_foreign_probe", |_root, _name, dir| {
                remove_dir_after_foreign_probe(dir)
            }),
        ];
        for (label, delete) in seams {
            let root = tmpdir(&format!("close-symlink-{label}"));
            let name = "demo";
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("marker.txt"), "session-data").unwrap();
            let outside = root.join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("sentinel.txt"), "do-not-touch").unwrap();
            // Swap the session dir for a symlink just before the deletion.
            std::fs::remove_dir_all(&dir).unwrap();
            std::os::unix::fs::symlink(&outside, &dir).unwrap();
            let err = delete(&root, name, &dir).unwrap_err();
            assert!(
                format!("{err:#}").contains("must not be a symlink"),
                "{label}: {err:#}"
            );
            assert_eq!(
                std::fs::read_to_string(outside.join("sentinel.txt")).unwrap(),
                "do-not-touch",
                "{label}: outside target intact"
            );
            assert!(
                std::fs::symlink_metadata(&dir)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false),
                "{label}: link itself preserved, never followed"
            );
            // Drop the planted link itself (never through it), then the root.
            std::fs::remove_file(&dir).unwrap();
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn close_preserves_unreadable_corrupt_and_bad_port() {
        // Decided matrix: IO failure, JSON corruption, and missing/invalid
        // port all preserve the dir with an actionable error (session path
        // + next step) — never an automatic delete. Pre-fix, all four fell
        // through to the delete path.
        let cases: Vec<(&str, Box<dyn Fn(&std::path::Path)>)> = vec![
            // Unreadable session.json (a directory: read_to_string fails
            // with a non-NotFound IO kind for any user).
            (
                "io",
                Box::new(|dir: &std::path::Path| {
                    std::fs::create_dir_all(dir.join("session.json")).unwrap();
                }),
            ),
            (
                "corrupt",
                Box::new(|dir: &std::path::Path| {
                    std::fs::write(dir.join("session.json"), "not json").unwrap();
                }),
            ),
            (
                "no-port",
                Box::new(|dir: &std::path::Path| {
                    std::fs::write(
                        dir.join("session.json"),
                        r#"{"name":"demo","kind":"launch"}"#,
                    )
                    .unwrap();
                }),
            ),
            (
                "bad-port",
                Box::new(|dir: &std::path::Path| {
                    std::fs::write(
                        dir.join("session.json"),
                        r#"{"name":"demo","kind":"launch","port":99999}"#,
                    )
                    .unwrap();
                }),
            ),
        ];
        for (tag, setup) in cases {
            let root = tmpdir(&format!("close-preserve-{tag}"));
            let name = "demo";
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            setup(&dir);
            let err = close_in(&root, name).unwrap_err();
            let msg = format!("{err:#}");
            let sess = dir.join("session.json").display().to_string();
            assert!(msg.contains(&sess), "{tag}: error names the path: {msg}");
            assert!(
                msg.contains("fix or remove it explicitly"),
                "{tag}: error gives the next step: {msg}"
            );
            assert!(dir.exists(), "{tag}: session dir preserved");
            assert!(
                root.join(name).join("session.json").exists(),
                "{tag}: session file preserved"
            );
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn close_cleans_stale_unpublished_and_retries_live_starter() {
        // Missing session.json with no lock: stale unpublished dir is
        // cleaned (existing remove_unpublished_dir semantics). With a live
        // startup lock: retryable, dir and lock intact.
        let root = tmpdir("close-stale-unpub");
        let name = "demo";
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let v = close_in(&root, name).expect("stale unpublished cleans");
        assert_eq!(v["closed"], json!("demo"));
        assert!(!dir.exists(), "stale unpublished dir removed");
        // Live starter claims the name: hands off.
        std::fs::create_dir_all(&dir).unwrap();
        let lock = startup_lock_path(&root, name);
        std::fs::write(&lock, "live-starter-nonce").unwrap();
        let err = close_in(&root, name).unwrap_err();
        assert!(
            format!("{err:#}").contains("is starting; retry shortly"),
            "{err:#}"
        );
        assert!(dir.is_dir(), "starting dir preserved");
        assert!(lock.exists(), "live lock preserved");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn close_startup_gate_fresh_lock_blocks_and_preserves() {
        // Actively starting session: unpublished dir + live lock. Close
        // bails retryably and touches neither the dir nor the lock.
        let root = tmpdir("close-gate-fresh");
        let name = "demo";
        std::fs::create_dir_all(root.join(name)).unwrap();
        let lock = startup_lock_path(&root, name);
        std::fs::write(&lock, "live-starter-nonce").unwrap();
        let err = startup_close_gate(&root, name).unwrap_err();
        assert!(
            format!("{err:#}").contains("session 'demo' is starting; retry shortly"),
            "{err:#}"
        );
        assert!(root.join(name).is_dir(), "starting dir untouched");
        assert!(lock.exists(), "live lock untouched");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn close_startup_gate_stale_lock_proceeds_and_drops_lock() {
        // Crashed starter: provably stale lock is detached-and-verified,
        // the gate proceeds (close then removes the dead dir itself).
        // The gate never deletes dirs — only the stale lock.
        let root = tmpdir("close-gate-stale");
        let name = "demo";
        std::fs::create_dir_all(root.join(name)).unwrap();
        let lock = startup_lock_path(&root, name);
        std::fs::write(&lock, "crashed-starter").unwrap();
        let old = std::time::SystemTime::now() - STARTUP_LOCK_STALE - Duration::from_secs(5);
        let f = std::fs::File::options().write(true).open(&lock).unwrap();
        f.set_modified(old).unwrap();
        drop(f);
        startup_close_gate(&root, name).expect("stale lock proceeds");
        assert!(!lock.exists(), "stale lock dropped");
        assert!(root.join(name).is_dir(), "gate never deletes dirs");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn close_startup_gate_replaced_lock_is_preserved() {
        // A lock replaced under a stale verdict (fresh starter claimed the
        // name) mismatches the verified bytes: hands off, new lock intact.
        let root = tmpdir("close-gate-replaced");
        let lock = startup_lock_path(&root, "demo");
        std::fs::write(&lock, "stale-bytes").unwrap();
        assert!(
            !quarantine_verified(&lock, "different-bytes"),
            "mismatch must not detach"
        );
        assert_eq!(
            std::fs::read_to_string(&lock).unwrap(),
            "stale-bytes",
            "fresh record intact"
        );
        // A replacement with a fresh mtime reads as live at the gate.
        std::fs::write(&lock, "new-starter-nonce").unwrap();
        let err = startup_close_gate(&root, "demo").unwrap_err();
        assert!(
            format!("{err:#}").contains("is starting; retry shortly"),
            "{err:#}"
        );
        assert_eq!(std::fs::read_to_string(&lock).unwrap(), "new-starter-nonce");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn close_startup_gate_concurrent_stale_reclaim() {
        // Eight racers on one stale lock: all proceed, the lock is dropped
        // exactly once (rename admits one detacher; the rest see NotFound),
        // and no quarantine file lingers.
        let root = tmpdir("close-gate-race");
        let name = "demo";
        std::fs::create_dir_all(root.join(name)).unwrap();
        let lock = startup_lock_path(&root, name);
        std::fs::write(&lock, "crashed-starter").unwrap();
        let old = std::time::SystemTime::now() - STARTUP_LOCK_STALE - Duration::from_secs(5);
        let f = std::fs::File::options().write(true).open(&lock).unwrap();
        f.set_modified(old).unwrap();
        drop(f);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    startup_close_gate(&root, name).expect("stale reclaim proceeds");
                });
            }
        });
        assert!(!lock.exists(), "stale lock dropped once");
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(".q-"))
            .collect();
        assert!(leftovers.is_empty(), "no quarantine lingers: {leftovers:?}");
        assert!(root.join(name).is_dir(), "gate never deletes dirs");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn close_startup_gate_published_session_ignores_lock() {
        // A session.json that appeared (starter just published) takes the
        // normal bridge path regardless of any lock file.
        let root = tmpdir("close-gate-published");
        let name = "demo";
        std::fs::create_dir_all(root.join(name)).unwrap();
        std::fs::write(root.join(name).join("session.json"), r#"{"port":1}"#).unwrap();
        let lock = startup_lock_path(&root, name);
        std::fs::write(&lock, "live-starter-nonce").unwrap();
        startup_close_gate(&root, name).expect("published proceeds");
        assert!(lock.exists(), "live lock untouched on the bridge path");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn layered_reads_reject_flat_and_malformed_across_surfaces() {
        // Canonical v2 read: flat legacy and malformed stored identities
        // become null on every surface (status row, local roster, cached
        // spawn/context read); valid layered echoes (sanitized, capped).
        for (tag, body) in [
            (
                "flat",
                r#"{"name":"x","kind":"attach","port":0,"stopped":true,
                        "targetIdentity":{"kind":"process","pid":4343,"argv":["python"]}}"#,
            ),
            (
                "role-type",
                r#"{"name":"x","kind":"attach","port":0,"stopped":true,
                        "targetIdentity":{"debuggee":7,"endpoint":null,"adapter":null}}"#,
            ),
            (
                "partial",
                r#"{"name":"x","kind":"attach","port":0,"stopped":true,
                        "targetIdentity":{"debuggee":{"confidence":"protocol-confirmed"}}}"#,
            ),
        ] {
            let dir = tmpdir(&format!("layered-reject-{tag}"));
            std::fs::write(dir.join("session.json"), body).unwrap();
            assert_eq!(cached_target_identity(&dir), Value::Null, "{tag}");
            assert_eq!(cmd_targets_in(&dir)["targetIdentity"], Value::Null, "{tag}");
            assert_eq!(session_entry(&dir)["targetIdentity"], Value::Null, "{tag}");
            assert_eq!(
                layered_identity_or_null(
                    serde_json::from_str::<Value>(body)
                        .unwrap()
                        .get("targetIdentity")
                        .cloned()
                ),
                Value::Null,
                "{tag}: context read path"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
        // Valid layered echoes verbatim when in budget (bridge output
        // untouched); over-budget strings cap defensively, roles intact.
        let dir = tmpdir("layered-accept");
        let valid = json!({
            "debuggee": {"kind": "process", "pid": 11, "confidence": "protocol-confirmed"},
            "endpoint": {"host": "h", "port": 9, "confidence": "os-corroborated"},
            "adapter": null,
        });
        assert_eq!(layered_identity_or_null(Some(valid.clone())), valid);
        let long = "y".repeat(IDENTITY_FIELD_CAP + 100);
        let big = json!({
            "debuggee": {"kind": "process", "pid": 11, "name": long},
            "endpoint": null,
            "adapter": null,
        });
        let out = layered_identity_or_null(Some(big));
        assert_eq!(out["debuggee"]["pid"], json!(11));
        assert!(
            out["debuggee"]["name"].as_str().unwrap().chars().count() <= IDENTITY_FIELD_CAP + 30,
            "{out}"
        );
        assert!(out.to_string().chars().count() <= IDENTITY_TOTAL_CAP);
        assert_eq!(layered_identity_or_null(None), Value::Null);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_entry_and_roster_surface_identity_layered_only() {
        // Layered-only: no observedTarget key anywhere; targetIdentity rides
        // in both status rows and the main-only targets roster.
        let dir = tmpdir("identity-surface");
        std::fs::write(
            dir.join("lang.json"),
            r#"{"lang":"java","schemaVersion":2}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("stops.json"),
            r#"{"breaks":[],"logpoints":[],"watches":[],"exits":[],
                    "sources":[],"timeout":7,"schemaVersion":2,
                    "target":{"main":"x"},"requestedTarget":{"main":"x"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("session.json"),
            r#"{"name":"x","kind":"attach","port":0,"stopped":false,
                    "schemaVersion":2,
                    "targetIdentity":{"debuggee":{"pid":7},"endpoint":null,"adapter":null}}"#,
        )
        .unwrap();
        let row = session_entry(&dir);
        assert!(row.get("observedTarget").is_none());
        assert_eq!(row["targetIdentity"]["debuggee"]["pid"], json!(7));
        assert_eq!(row["unsupported"], json!(false));
        assert_eq!(row["stale"], json!(false));
        assert!(row["hint"].is_null());
        assert_eq!(row["lang"], json!("java"));
        let roster = cmd_targets_in(&dir);
        assert_eq!(roster["targetIdentity"]["debuggee"]["pid"], json!(7));
        assert!(roster["targets"][0].get("observed").is_none());
        // Marker-less session.json with v2 CLI markers: old/unsupported,
        // never current.
        std::fs::write(
            dir.join("session.json"),
            r#"{"name":"x","kind":"attach","port":0,"stopped":false}"#,
        )
        .unwrap();
        let row = session_entry(&dir);
        assert_eq!(row["unsupported"], json!(true));
        assert_eq!(row["stale"], json!(true));
        let hint = row["hint"].as_str().unwrap();
        assert!(hint.starts_with("close '"), "{hint}");
        assert!(hint.contains("unsupported schema v1"), "{hint}");
        assert_eq!(cmd_targets_in(&dir)["targetIdentity"], Value::Null);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- attach endpoint collision prevention ----

    #[test]
    fn status_row_marks_old_sessions_unsupported() {
        // Legacy dir (v1 markers, old session file): frozen old row.
        let dir = tmpdir("status-old");
        std::fs::write(dir.join("lang.json"), r#"{"lang":"py"}"#).unwrap();
        std::fs::write(
            dir.join("stops.json"),
            r#"{"breaks":["a.py:1"],"logpoints":[],"watches":[],"exits":[],
                    "target":{"program":"a.py"},
                    "requestedTarget":{"program":"a.py","pid":null}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("session.json"),
            r#"{"name":"s","kind":"launch","port":0,"stopped":true,
                    "lastStop":null,"updatedAt":7,
                    "observedTarget":{"kind":"process","pid":1}}"#,
        )
        .unwrap();
        let row = session_entry(&dir);
        assert_eq!(row["unsupported"], json!(true));
        assert_eq!(row["stale"], json!(true));
        assert_eq!(
            row["hint"],
            json!("close 'agent-debugger-test-status-old' and recreate (unsupported schema v1)")
        );
        assert_eq!(row["lang"], json!("py"));
        assert_eq!(row["port"], json!(0));
        assert_eq!(row["alive"], json!(false));
        assert!(row.get("observedTarget").is_none(), "{row}");
        assert!(
            row["targetIdentity"].is_null(),
            "legacy flat never promoted: {row}"
        );
        assert_eq!(row["armed"]["breaks"], json!(1));
        assert_eq!(row["requestedTarget"]["program"], json!("a.py"));
        // Marker-less dir (no sidecars at all): same unsupported shape,
        // lang unknown, everything null — never a crash, never current.
        let bare = tmpdir("status-bare");
        let row = session_entry(&bare);
        assert_eq!(row["unsupported"], json!(true));
        assert_eq!(row["lang"], json!("unknown"));
        assert!(row["stopped"].is_null());
        assert!(row["armed"].is_null());
        assert!(row["requestedTarget"].is_null());
        // Startup row (v2 markers, no session.json yet): current, not stale.
        let starting = tmpdir("status-starting");
        write_v2_markers(&starting, "node");
        let row = session_entry(&starting);
        assert_eq!(row["unsupported"], json!(false));
        assert_eq!(row["stale"], json!(false));
        assert!(row["hint"].is_null());
        assert_eq!(row["alive"], json!(false));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&bare);
        let _ = std::fs::remove_dir_all(&starting);
    }
}
