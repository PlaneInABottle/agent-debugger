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

fn read_session(name: &str) -> anyhow::Result<Value> {
    let file = session_dir(name).join("session.json");
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

/// Language owning a session (sidecar file; missing = "java" for old sessions).
fn session_lang(name: &str) -> String {
    std::fs::read_to_string(session_dir(name).join("lang.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| {
            v.get("lang")
                .and_then(|l| l.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "java".to_string())
}

/// Spawn the bridge daemon and wait for the first stop.
pub fn spawn(name: &str, spec: &SpawnSpec) -> anyhow::Result<Value> {
    let dir = session_dir(name);
    if dir.join("session.json").exists() {
        anyhow::bail!("session '{name}' already exists (close it first)");
    }
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", dir.to_string_lossy()))?;
    // Clear leftovers from a previous failed attempt.
    let _ = std::fs::remove_file(dir.join("session.json"));
    let _ = std::fs::remove_file(dir.join("error.json"));
    let _ = std::fs::remove_file(dir.join("stops.json"));
    let _ = std::fs::write(
        dir.join("lang.json"),
        format!("{{\"lang\":\"{}\"}}", spec.lang),
    );
    // Spawn-time intent first: even if the bridge dies during setup, the
    // failed attempt's dir is removed wholesale — but while the session
    // lives, stops.json is always present for resume.
    let _ = std::fs::write(
        dir.join("stops.json"),
        serde_json::to_string_pretty(&spec.stops).unwrap_or_else(|_| "{}".to_string()),
    );

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

    let mut child = std::process::Command::new(&program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|e| anyhow::anyhow!("{e}"))?)
        .stderr(log)
        .spawn()
        .map_err(|e| anyhow::anyhow!("{program} not found or failed to start: {e}"))?;
    // Detached by design: the short-lived CLI exits, the bridge keeps the
    // debug session alive (reparented). Never `child.kill()` on success.
    let deadline = Instant::now() + Duration::from_secs(spec.wait_secs);
    loop {
        if dir.join("session.json").exists() {
            // Forget the child (no kill on drop); session owns it now.
            std::mem::forget(child);
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
                    // the first read, but its logs are already on disk.
                    if let Some(logs) = read_logs_file(&dir, 50) {
                        return Ok(json!({
                            "warning": format!("{e:#} (target already exited)"),
                            "logs": logs,
                        }));
                    }
                    return Err(e);
                }
            }
        }
        if dir.join("error.json").exists() {
            let msg = read_bridge_error(&dir);
            let _ = child.kill();
            let _ = std::fs::remove_dir_all(&dir);
            anyhow::bail!("{msg}");
        }
        if let Ok(Some(status)) = child.try_wait() {
            let log_tail = read_log_tail(&dir);
            let _ = std::fs::remove_dir_all(&dir);
            anyhow::bail!("bridge exited during setup (code {status}). {log_tail}");
        }
        if Instant::now() > deadline {
            let _ = child.kill();
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

/// Close a session: ask the bridge to disconnect, always remove the dir.
pub fn close(name: &str) -> anyhow::Result<Value> {
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
        if client::request(port, &json!({"cmd": "close"}), Duration::from_secs(65)).is_ok() {
            confirmed = true;
        }
        // Then make sure the daemon actually exited before dropping the dir.
        let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    let dir = session_dir(name);
    if !dir.exists() {
        anyhow::bail!("no session '{name}'");
    }
    std::fs::remove_dir_all(&dir).map_err(|e| anyhow::anyhow!("cannot remove session: {e}"))?;
    Ok(json!({"closed": name, "confirmed": confirmed}))
}

/// CLI status: binary version plus known sessions with liveness probes.
pub fn status() -> Value {
    let mut sessions = Vec::new();
    if let Ok(entries) = std::fs::read_dir(sessions_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let session_file = entry.path().join("session.json");
            let raw = std::fs::read_to_string(&session_file).unwrap_or_default();
            let parsed: Value = serde_json::from_str(&raw).unwrap_or(json!({}));
            let port = parsed.get("port").and_then(|p| p.as_u64()).unwrap_or(0) as u16;
            let alive = port != 0
                && std::net::TcpStream::connect_timeout(
                    &format!("127.0.0.1:{port}").parse().unwrap(),
                    Duration::from_millis(300),
                )
                .is_ok();
            // Resume intent: armed counts + target, derived at spawn. Old
            // sessions predate stops.json — null there is honest ("unknown"),
            // never fabricated zeros.
            let stops_raw = std::fs::read_to_string(entry.path().join("stops.json")).ok();
            let stops_parsed: Option<Value> =
                stops_raw.and_then(|raw| serde_json::from_str(&raw).ok());
            let (armed, target) = match &stops_parsed {
                Some(v) => (
                    Some(stops_armed(v)),
                    v.get("target").cloned().unwrap_or(Value::Null),
                ),
                None => (None, Value::Null),
            };
            sessions.push(json!({
                "name": name,
                "lang": session_lang(&name),
                "kind": parsed.get("kind"),
                "port": port,
                "alive": alive,
                "stopped": parsed.get("stopped"),
                "armed": armed,
                "target": target,
            }));
        }
    }
    sessions.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "sessions": sessions,
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
}
