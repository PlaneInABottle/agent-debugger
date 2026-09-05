//! Session lifecycle: spawn the bridge daemon, forward commands, clean up.
//!
//! Layout: `~/.agent-debugger/sessions/<name>/` holds `session.json`
//! (written by the bridge when stopped at the first breakpoint),
//! `error.json` (fatal setup failure), `lang.json` (adapter language),
//! `stops.json` (spawn-time intent: armed stops + target summary, written by
//! the CLI so a compacted agent can resume with zero prior memory),
//! `owner.json` (abandonment guard nonce) and `bridge.log`.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::{bridge, client};

pub fn sessions_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".agent-debugger").join("sessions")
}

pub fn session_dir(name: &str) -> PathBuf {
    sessions_dir().join(name)
}

/// Session names are single filesystem segments. Without this, an absolute
/// name would escape the sessions root on join (Rust replaces the base),
/// and `close` would recursively delete an arbitrary directory.
fn check_name(name: &str) -> anyhow::Result<()> {
    // Backslash is rejected outright too: a legal filename char on Unix
    // but a separator on Windows — session names never need it.
    if name.contains(['\\', '/', '\0']) {
        anyhow::bail!("invalid session name '{name}' (single path segment only)");
    }
    let mut comps = std::path::Path::new(name).components();
    match comps.next() {
        Some(std::path::Component::Normal(_)) if comps.next().is_none() => Ok(()),
        _ => anyhow::bail!("invalid session name '{name}' (single path segment only)"),
    }
}

fn checked_session_dir(name: &str) -> anyhow::Result<PathBuf> {
    check_name(name)?;
    let dir = session_dir(name);
    check_dir_real(&dir)?;
    Ok(dir)
}

/// A session dir must be a real directory, never a symlink: otherwise an
/// attacker-planted entry could redirect sidecar writes (or `close`'s
/// recursive delete) outside the sessions root. TOCTOU between check and
/// use is accepted as best-effort; creation paths below use exclusive
/// create_dir so a swapped-in link fails instead of being followed.
fn check_dir_real(dir: &std::path::Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => {
            anyhow::bail!("session path must not be a symlink: {}", dir.display())
        }
        Ok(meta) if !meta.file_type().is_dir() => {
            anyhow::bail!("session path must be a real directory: {}", dir.display())
        }
        _ => Ok(()),
    }
}

fn read_session(name: &str) -> anyhow::Result<Value> {
    let file = checked_session_dir(name)?.join("session.json");
    let raw = std::fs::read_to_string(&file)
        .map_err(|_| anyhow::anyhow!("no session '{name}' (start or attach first)"))?;
    serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("corrupt session file: {e}"))
}

fn session_port(name: &str) -> anyhow::Result<u16> {
    let session = read_session(name)?;
    session
        .get("port")
        .and_then(|p| p.as_u64())
        .and_then(|p| u16::try_from(p).ok())
        .ok_or_else(|| anyhow::anyhow!("corrupt session file for '{name}'"))
}

/// Forward one command to the session; map {ok:false} to Err.
/// Both adapters speak the same session protocol, so forwarding is
/// language-agnostic (the session's lang only selects the adapter process).
pub fn forward(name: &str, body: &Value, timeout: Duration) -> anyhow::Result<Value> {
    check_name(name)?;
    let port = session_port(name)?;
    let resp = client::request(port, body, timeout)?;
    if resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        Ok(resp)
    } else {
        let msg = resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown bridge error");
        anyhow::bail!("{msg}")
    }
}

pub struct SpawnSpec {
    pub lang: &'static str, // "java" | "py" | "node" | "browser"
    pub kind: &'static str, // "attach" | "launch"
    pub bridge_args: Vec<String>,
    pub wait_secs: u64,
    /// Spawn-time intent (armed stops + target summary), persisted to
    /// `stops.json` so resume needs no prior memory. Built by `cmd_spawn`.
    pub stops: Value,
}

/// Summarize WHAT the session targets by scanning bridge args for known
/// flags. Derived automatically at spawn — the agent never writes this.
/// Unknown flags are ignored (forward-compat with new target options).
pub fn target_summary(args: &[String]) -> Value {
    let mut map = serde_json::Map::new();
    // Flag -> display key. Values are single tokens (paths, ports, hosts).
    let keys = [
        ("--program", "program"),
        ("--main", "main"),
        ("--port", "port"),
        ("--host", "host"),
        ("--tab", "tab"),
        ("--cp", "classpath"),
        ("--python", "python"),
        ("--node", "node"),
    ];
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            break; // everything after is program args, not target identity
        }
        let mut matched = false;
        for (flag, key) in keys {
            if a == flag && i + 1 < args.len() {
                map.insert(key.to_string(), Value::String(args[i + 1].clone()));
                matched = true;
                break;
            }
        }
        i += if matched { 2 } else { 1 };
    }
    Value::Object(map)
}

/// Armed-stop counts from a parsed `stops.json` (intent side of resume).
fn stops_armed(stops: &Value) -> Value {
    let count = |key: &str| {
        stops
            .get(key)
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0)
    };
    json!({
        "breaks": count("breaks"),
        "logpoints": count("logpoints"),
        "watches": count("watches"),
        "exits": count("exits"),
    })
}

/// Language owning a session dir (sidecar file; missing = "java").
fn session_lang_in(dir: &std::path::Path) -> String {
    std::fs::read_to_string(dir.join("lang.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| {
            v.get("lang")
                .and_then(|l| l.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "java".to_string())
}

/// Write sidecars and spawn the bridge process. Called only from `spawn`,
/// which removes the session dir if this fails.
fn setup_bridge(dir: &std::path::Path, spec: &SpawnSpec) -> anyhow::Result<std::process::Child> {
    // Spawn-time intent first: while the session lives, stops.json is
    // always present for resume. Unlike before, these writes propagate
    // errors — a session whose intent cannot persist must not start.
    std::fs::write(
        dir.join("lang.json"),
        format!("{{\"lang\":\"{}\"}}", spec.lang),
    )
    .map_err(|e| anyhow::anyhow!("cannot write session lang: {e}"))?;
    std::fs::write(
        dir.join("stops.json"),
        serde_json::to_string_pretty(&spec.stops).unwrap_or_else(|_| "{}".to_string()),
    )
    .map_err(|e| anyhow::anyhow!("cannot write session intent: {e}"))?;

    let log = std::fs::File::create(dir.join("bridge.log"))
        .map_err(|e| anyhow::anyhow!("cannot create bridge log: {e}"))?;

    // Adapter process per language; the session protocol on top is identical.
    let (program, mut args) = match spec.lang {
        "py" => {
            let interp = bridge::ensure_py()?;
            let script = bridge::ensure_pybridge()?;
            (
                interp,
                vec![
                    script.to_string_lossy().to_string(),
                    "session".to_string(),
                    "--kind".to_string(),
                    spec.kind.to_string(),
                    "--dir".to_string(),
                    dir.to_string_lossy().to_string(),
                ],
            )
        }
        "node" => {
            let bin = bridge::ensure_node()?;
            let script = bridge::ensure_nodebridge()?;
            bridge::ensure_ws()?;
            (
                bin,
                vec![
                    script.to_string_lossy().to_string(),
                    "session".to_string(),
                    "--kind".to_string(),
                    spec.kind.to_string(),
                    "--dir".to_string(),
                    dir.to_string_lossy().to_string(),
                ],
            )
        }
        "browser" => {
            // The bridge runs on Node (shared provisioning); Chrome is the
            // target. `ws` is provisioned once by the node adapter and shared
            // via NODE_PATH (no second install), prepended so any user value
            // keeps working.
            let bin = bridge::ensure_node()?;
            let script = bridge::ensure_browserbridge()?;
            bridge::ensure_ws()?;
            let ws_dir = bridge::node_modules_dir().to_string_lossy().to_string();
            let node_path = match std::env::var("NODE_PATH") {
                Ok(existing) if !existing.is_empty() => format!("{ws_dir}:{existing}"),
                _ => ws_dir,
            };
            std::env::set_var("NODE_PATH", node_path);
            (
                bin,
                vec![
                    script.to_string_lossy().to_string(),
                    "session".to_string(),
                    "--kind".to_string(),
                    spec.kind.to_string(),
                    "--dir".to_string(),
                    dir.to_string_lossy().to_string(),
                ],
            )
        }
        _ => {
            let classes = bridge::ensure_compiled()?;
            (
                "java".to_string(),
                vec![
                    "-cp".to_string(),
                    classes.to_string_lossy().to_string(),
                    bridge::BRIDGE_MAIN_CLASS.to_string(),
                    "session".to_string(),
                    "--kind".to_string(),
                    spec.kind.to_string(),
                    "--dir".to_string(),
                    dir.to_string_lossy().to_string(),
                ],
            )
        }
    };
    args.extend(spec.bridge_args.clone());

    let child = std::process::Command::new(&program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|e| anyhow::anyhow!("{e}"))?)
        .stderr(log)
        .spawn()
        .map_err(|e| anyhow::anyhow!("{program} not found or failed to start: {e}"))?;
    // Detached by design: the short-lived CLI exits, the bridge keeps the
    // debug session alive (reparented). Ownership transfers to the wait
    // loop below (handle dropped on success, kill on failure paths).
    Ok(child)
}

/// Spawn the bridge daemon and wait for the first stop.
pub fn spawn(name: &str, spec: &SpawnSpec) -> anyhow::Result<Value> {
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(spec.wait_secs))
        .ok_or_else(|| anyhow::anyhow!("timeout is too large"))?;
    let dir = checked_session_dir(name)?;
    if dir.join("session.json").exists() {
        anyhow::bail!("session '{name}' already exists (close it first)");
    }
    std::fs::create_dir_all(sessions_dir())
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", sessions_dir().display()))?;
    // A leftover dir without session.json is a failed attempt, not a live
    // session: clear it so the name is reusable. A live session is caught
    // by the session.json check above. Re-validate immediately before the
    // recursive delete: the earlier check_dir_real covered a TOCTOU window
    // in which a planted symlink (or file) could redirect the clear outside
    // the sessions root. Only a real directory is removed.
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if meta.file_type().is_symlink() => {
            anyhow::bail!("session path must not be a symlink: {}", dir.display())
        }
        Ok(meta) if !meta.file_type().is_dir() => {
            anyhow::bail!("session path must be a real directory: {}", dir.display())
        }
        Ok(_) => std::fs::remove_dir_all(&dir)
            .map_err(|e| anyhow::anyhow!("cannot clear stale {}: {e}", dir.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => anyhow::bail!("cannot clear stale {}: {e}", dir.display()),
    }
    // Exclusive creation prevents two concurrent starts racing on one name.
    std::fs::create_dir(&dir)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", dir.to_string_lossy()))?;
    // Clear leftovers from a previous failed attempt.
    let _ = std::fs::remove_file(dir.join("session.json"));
    let _ = std::fs::remove_file(dir.join("error.json"));
    let _ = std::fs::remove_file(dir.join("stops.json"));
    // All setup below runs guarded: any failure before the wait loop owns
    // a live bridge removes the dir wholesale, so a failed start never
    // blocks a retry and never leaves misleading intent behind.
    let mut child = match setup_bridge(&dir, spec) {
        Ok(child) => Some(child),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
    };
    // Reap helper: the loop below either forgets the child (session owns
    // it now) or kills it on failure paths. Option-taking keeps every
    // path sound across loop iterations (no use-after-forget).
    let reap = |child: &mut Option<std::process::Child>| {
        if let Some(mut c) = child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    };

    // Detached by design: the short-lived CLI exits, the bridge keeps the
    // debug session alive (reparented). Never kill on success.
    loop {
        // error.json first: a fast-exiting target can leave BOTH files
        // behind (session.json from the death publish, error.json from
        // the throw) — the error is the truth, not the stale readiness.
        if dir.join("error.json").exists() {
            let msg = read_bridge_error(&dir);
            reap(&mut child);
            let _ = std::fs::remove_dir_all(&dir);
            anyhow::bail!("{msg}");
        }
        if dir.join("session.json").exists() {
            // Retain the handle until the first request is validated.
            // Initial data: live context when stopped, else a thread dump.
            let stopped = std::fs::read_to_string(dir.join("session.json"))
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .and_then(|v| v.get("stopped").and_then(|s| s.as_bool()))
                .unwrap_or(true);
            let first = if stopped {
                json!({"cmd": "context"})
            } else {
                json!({"cmd": "threads"})
            };
            match forward(name, &first, Duration::from_secs(15)) {
                Ok(data) => return Ok(data),
                Err(e) => {
                    // Fast program + logpoints-only: the target may exit before
                    // the first read, but its logs are already on disk. The
                    // daemon reaps itself on exit; the session stays for them.
                    if let Some(logs) = read_logs_file(&dir, 50) {
                        return Ok(json!({
                            "warning": format!("{e:#} (target already exited)"),
                            "logs": logs,
                        }));
                    }
                    // No session was established and nothing is collectible:
                    // take the daemon down and remove the dir, so the name
                    // is reusable and no orphan lingers.
                    reap(&mut child);
                    let _ = std::fs::remove_dir_all(&dir);
                    return Err(e);
                }
            }
        }
        if let Some(status) = child
            .as_mut()
            .and_then(|c: &mut std::process::Child| c.try_wait().ok())
            .flatten()
        {
            let log_tail = read_log_tail(&dir);
            let _ = std::fs::remove_dir_all(&dir);
            anyhow::bail!("bridge exited during setup (code {status}). {log_tail}");
        }
        if Instant::now() > deadline {
            reap(&mut child);
            let log_tail = read_log_tail(&dir);
            let _ = std::fs::remove_dir_all(&dir);
            anyhow::bail!("timed out waiting for first breakpoint. {log_tail}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Tail of the session's collected logpoint lines, if any exist on disk.
fn read_logs_file(dir: &std::path::Path, tail: usize) -> Option<Value> {
    let raw = std::fs::read_to_string(dir.join("logs.jsonl")).ok()?;
    let lines: Vec<&str> = raw.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let start = lines.len().saturating_sub(tail);
    Some(json!({
        "total": lines.len(),
        "truncated": start > 0,
        "lines": lines[start..],
    }))
}

fn read_bridge_error(dir: &std::path::Path) -> String {
    std::fs::read_to_string(dir.join("error.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "bridge failed during setup (see bridge.log)".to_string())
}

fn read_log_tail(dir: &std::path::Path) -> String {
    let raw = std::fs::read_to_string(dir.join("bridge.log")).unwrap_or_default();
    let tail: String = raw
        .chars()
        .rev()
        .take(500)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if tail.trim().is_empty() {
        "no bridge log".to_string()
    } else {
        format!("bridge log tail: {}", tail.trim())
    }
}

/// Close a session; retain management state if the daemon stays alive.
pub fn close(name: &str) -> anyhow::Result<Value> {
    let dir = checked_session_dir(name)?;
    // Whether the bridge ACKed the close. A false here (with the port
    // already dead) means the daemon died on its own; a false with the port
    // alive means it is wedged — the dir is still removed, but the caller
    // sees the difference instead of a fabricated success.
    let mut confirmed = false;
    if let Ok(port) = session_port(name) {
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
    std::fs::remove_dir_all(&dir).map_err(|e| anyhow::anyhow!("cannot remove session: {e}"))?;
    Ok(json!({"closed": name, "confirmed": confirmed}))
}

/// CLI status: binary version plus known sessions with liveness probes.
/// `stopped`/`lastStop`/`updatedAt` come straight from the bridge-maintained
/// session.json (rewritten on every stop/resume/exit), so a compacted agent
/// sees where each session is parked without any prior memory.
pub fn status() -> Value {
    let mut sessions = Vec::new();
    if let Ok(entries) = std::fs::read_dir(sessions_dir()) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && checked_session_dir(&entry.file_name().to_string_lossy()).is_ok()
            {
                sessions.push(session_entry(&entry.path()));
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
fn session_entry(dir: &std::path::Path) -> Value {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let raw = std::fs::read_to_string(dir.join("session.json")).unwrap_or_default();
    let parsed: Value = serde_json::from_str(&raw).unwrap_or(json!({}));
    // Checked conversion (like session_port): a corrupt huge port must not
    // truncate into a live-looking probe of an unrelated service.
    let port = parsed
        .get("port")
        .and_then(|p| p.as_u64())
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or(0);
    let alive = port != 0
        && std::net::TcpStream::connect_timeout(
            &format!("127.0.0.1:{port}").parse().unwrap(),
            Duration::from_millis(300),
        )
        .is_ok();
    // Resume intent: armed counts + target, derived at spawn. Old
    // sessions predate stops.json — null there is honest ("unknown"),
    // never fabricated zeros.
    let stops_raw = std::fs::read_to_string(dir.join("stops.json")).ok();
    let stops_parsed: Option<Value> = stops_raw.and_then(|raw| serde_json::from_str(&raw).ok());
    let (armed, target) = match &stops_parsed {
        Some(v) => (
            Some(stops_armed(v)),
            v.get("target").cloned().unwrap_or(Value::Null),
        ),
        None => (None, Value::Null),
    };
    json!({
        "name": name,
        "lang": session_lang_in(dir),
        "kind": parsed.get("kind"),
        "port": port,
        "alive": alive,
        "stopped": parsed.get("stopped"),
        "lastStop": parsed.get("lastStop").cloned().unwrap_or(Value::Null),
        "updatedAt": parsed.get("updatedAt").cloned().unwrap_or(Value::Null),
        "armed": armed,
        "target": target,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-debugger-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn logs_tail_returns_last_lines_with_counts() {
        let dir = tmpdir("logs");
        std::fs::write(dir.join("logs.jsonl"), "a\nb\nc\nd\ne\n").unwrap();
        let v = read_logs_file(&dir, 3).expect("must read");
        assert_eq!(v["total"], 5);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["lines"], json!(["c", "d", "e"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn logs_missing_or_empty_is_none() {
        let dir = tmpdir("logs-empty");
        assert!(read_logs_file(&dir, 50).is_none());
        std::fs::write(dir.join("logs.jsonl"), "").unwrap();
        assert!(read_logs_file(&dir, 50).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn target_summary_picks_known_flags() {
        let v = target_summary(&args(&[
            "--program",
            "app.py",
            "--break",
            "app.py:3",
            "--timeout",
            "20",
        ]));
        assert_eq!(v["program"], json!("app.py"));
        assert!(
            v.get("break").is_none(),
            "stop flags are not target identity"
        );
        assert!(v.get("timeout").is_none());
    }

    #[test]
    fn target_summary_stops_at_dashdash() {
        // Program args after `--` must never leak into target identity,
        // even when they look like flags.
        let v = target_summary(&args(&["--port", "9222", "--", "--port", "1234"]));
        assert_eq!(v["port"], json!("9222"));
    }

    #[test]
    fn target_summary_unknown_flags_ignored() {
        let v = target_summary(&args(&["--tab", "shop", "--frobnicate", "x"]));
        assert_eq!(v["tab"], json!("shop"));
        assert!(v.get("frobnicate").is_none());
    }

    #[test]
    fn stops_armed_counts_lists() {
        let v = json!({"breaks": ["a:1", "b:2"], "logpoints": [], "watches": ["C.f"]});
        let armed = stops_armed(&v);
        assert_eq!(
            armed,
            json!({"breaks": 2, "logpoints": 0, "watches": 1, "exits": 0})
        );
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
        assert_eq!(v["lang"], json!("java")); // missing sidecar default
        assert_eq!(v["lastStop"], Value::Null);
        assert_eq!(v["armed"], Value::Null);
        assert_eq!(v["alive"], json!(false)); // port 0: no probe
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
    fn check_dir_real_rejects_symlink_and_non_dir() {
        // Stale-dir clear re-validates with symlink_metadata right before
        // remove_dir_all: a planted link or file must never be deleted.
        // No races here — the test only classifies paths it just created.
        let base = tmpdir("check-real");
        let real = base.join("real");
        std::fs::create_dir(&real).unwrap();
        assert!(check_dir_real(&real).is_ok());
        assert!(check_dir_real(&base.join("missing")).is_ok());
        let file = base.join("f");
        std::fs::write(&file, "x").unwrap();
        assert!(check_dir_real(&file).is_err());
        #[cfg(unix)]
        {
            let link = base.join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert!(check_dir_real(&link).is_err());
            let dangling = base.join("dangling");
            std::os::unix::fs::symlink(base.join("nope"), &dangling).unwrap();
            assert!(check_dir_real(&dangling).is_err());
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn check_name_allows_single_segments() {
        for ok in ["cart", "cart-npe", "a", "a_b.c-9", "UPPER09"] {
            assert!(check_name(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn check_name_rejects_escape() {
        // Absolute paths, traversal, separators, and empties must never
        // reach session_dir (close() deletes whatever it resolves to).
        for bad in ["", ".", "..", "../x", "a/b", "/tmp", "/etc/passwd", "a\\b"] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }
}
