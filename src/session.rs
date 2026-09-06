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

/// Add line breakpoints to a live session, persisting only what the bridge
/// confirms. Forward bound: min(10 + 5*N, 65)s for N raw specs (each bridge
/// round-trip is bounded server-side). Transport failure persists nothing —
/// the bridge may still have applied, so the error points at bare `breaks`.
/// Bridge ok:false also persists nothing (zero confirmed by contract).
/// A `target` selects an ephemeral target-scoped break: forwarded verbatim,
/// never persisted to stops.json (no inheritance).
pub fn cmd_breaks_add(
    name: &str,
    breaks: &[String],
    target: Option<&str>,
) -> anyhow::Result<Value> {
    check_name(name)?;
    let dir = checked_session_dir(name)?;
    let target = normalize_target_for_lang(&session_lang_in(&dir), target)?;
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
        let msg = resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown bridge error");
        anyhow::bail!("{msg}");
    }
    if target.is_some() {
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
        append_confirmed_breaks(name, &confirmed)?;
    }
    Ok(stamp_main(resp))
}

/// Remove live line breakpoints, dropping only what the bridge confirms.
/// Same transport contract as add: transport failure persists nothing (the
/// bridge may still have applied — bare `breaks` reconciles), ok:false
/// persists nothing. Partial success persists the confirmed subset and
/// keeps the bridge's warning. A `target` scopes removal to that target's
/// target-scoped records only and never touches stops.json.
pub fn cmd_breaks_remove(
    name: &str,
    breaks: &[String],
    target: Option<&str>,
) -> anyhow::Result<Value> {
    check_name(name)?;
    let dir = checked_session_dir(name)?;
    let target = normalize_target_for_lang(&session_lang_in(&dir), target)?;
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
        let msg = resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown bridge error");
        anyhow::bail!("{msg}");
    }
    if target.is_some() {
        return Ok(stamp_main(resp));
    }
    let removed = confirmed_removed(&resp);
    if !removed.is_empty() {
        remove_confirmed_breaks(name, &removed)?;
    }
    Ok(stamp_main(resp))
}

/// Drop all live line breakpoints (logpoints/watches/exits untouched).
/// Persistence mirrors remove: only the bridge-confirmed `removed` raws
/// leave stops.json. Forward bound matches remove policy (min(10 + 5*N,
/// 65)s over the persisted line-break count — logpoints/watches/exits are
/// not clear scope and never inflate the bound). A `target` drops only that
/// target's ephemeral target-scoped records (no stops.json change); bare
/// clear is the full line-break reset.
pub fn cmd_breaks_clear(name: &str, target: Option<&str>) -> anyhow::Result<Value> {
    check_name(name)?;
    let dir = checked_session_dir(name)?;
    let target = normalize_target_for_lang(&session_lang_in(&dir), target)?;
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
        let msg = resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown bridge error");
        anyhow::bail!("{msg}");
    }
    if target.is_some() {
        return Ok(stamp_main(resp));
    }
    let removed = confirmed_removed(&resp);
    if !removed.is_empty() {
        remove_confirmed_breaks(name, &removed)?;
    }
    Ok(stamp_main(resp))
}

/// Count of persisted line breaks for the clear forward bound. Reads the
/// stops.json breaks list; method:/exc: startup forms are not line breaks
/// and logpoints/watches/exits live in other lists, so none of them count.
/// Unreadable intent bounds as zero (floor 10s still applies).
fn persisted_line_breaks(name: &str) -> u64 {
    match checked_session_dir(name) {
        Ok(d) => persisted_line_breaks_in(&d),
        Err(_) => 0,
    }
}

fn persisted_line_breaks_in(dir: &std::path::Path) -> u64 {
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
fn confirmed_removed(resp: &Value) -> Vec<String> {
    resp.get("removed")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| e.get("raw").and_then(|r| r.as_str()).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Append confirmed raw specs to stops.json's breaks list. Atomic tmp+rename
/// in the same dir; every other field (timeout, target, unknown) is preserved
/// byte-for-byte in value (only the breaks array grows).
fn append_confirmed_breaks(name: &str, raws: &[String]) -> anyhow::Result<()> {
    append_confirmed_breaks_in(&checked_session_dir(name)?, raws)
}

fn append_confirmed_breaks_in(dir: &std::path::Path, raws: &[String]) -> anyhow::Result<()> {
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
        list.push(Value::String(r.clone()));
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
/// match against persisted entries, first occurrence per removed raw (the
/// list holds no duplicates: adds are idempotent). Every other field is
/// preserved in value; the write is atomic tmp+rename like the append path.
fn remove_confirmed_breaks(name: &str, raws: &[String]) -> anyhow::Result<()> {
    remove_confirmed_breaks_in(&checked_session_dir(name)?, raws)
}

fn remove_confirmed_breaks_in(dir: &std::path::Path, raws: &[String]) -> anyhow::Result<()> {
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
        if let Some(pos) = list.iter().position(|v| v.as_str() == Some(r)) {
            list.remove(pos);
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
/// Forward one command to the session; map {ok:false} to Err.
/// Both adapters speak the same session protocol, so forwarding is
/// language-agnostic (the session's lang only selects the adapter process).
/// Single-target bridges (java/browser) never named a target: stamp main
/// so every served response carries one uniformly.
pub fn forward(name: &str, body: &Value, timeout: Duration) -> anyhow::Result<Value> {
    forward_target(name, body, timeout, None)
}

/// Normalize a target selector for one session: omitted and explicit `main`
/// both serve main (no wire difference — legacy bridges never see the
/// field); anything else on a main-only adapter (java/browser) fails fast
/// instead of silently serving main.
fn normalize_target_for_lang(lang: &str, target: Option<&str>) -> anyhow::Result<Option<String>> {
    match target {
        None | Some("main") => Ok(None),
        Some(other) if lang == "java" || lang == "browser" => {
            anyhow::bail!("unsupported target '{other}' (session is {lang}, main-only)")
        }
        Some(other) => Ok(Some(other.to_string())),
    }
}

/// Every served response names its target. Multi-target bridges echo the
/// selected id themselves; main-only bridges predate the field, so stamp
/// `main` here (they can only ever serve main).
fn stamp_main(mut resp: Value) -> Value {
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
    let target = normalize_target_for_lang(&session_lang_in(&dir), target)?;
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
        let msg = resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown bridge error");
        anyhow::bail!("{msg}")
    }
}

/// List debug targets in this session. Multi-target bridges (py/node) own
/// their roster; main-only adapters (java/browser) get a uniform main-only
/// roster built from the bridge-maintained session.json (no new bridge
/// protocol needed before Batch3).
pub fn cmd_targets(name: &str) -> anyhow::Result<Value> {
    check_name(name)?;
    let dir = checked_session_dir(name)?;
    if matches!(session_lang_in(&dir).as_str(), "java" | "browser") {
        return Ok(cmd_targets_in(&dir));
    }
    Ok(stamp_main(forward(
        name,
        &json!({"cmd": "targets"}),
        Duration::from_secs(10),
    )?))
}

/// Main-only roster from a session dir (explicit dir so unit tests exercise
/// it without touching the real sessions dir).
fn cmd_targets_in(dir: &std::path::Path) -> Value {
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
            "observed": parsed.get("observedTarget").cloned().unwrap_or(Value::Null),
            "scope": "global",
        }],
        "selected": "main",
        "ignored": 0,
        "droppedExited": 0,
        "target": "main",
    })
}

pub struct SpawnSpec {
    pub lang: &'static str, // "java" | "py" | "node" | "browser"
    pub kind: &'static str, // "attach" | "launch"
    pub bridge_args: Vec<String>,
    pub wait_secs: u64,
    /// Spawn-time intent (armed stops + target summary), persisted to
    /// `stops.json` so resume needs no prior memory. Built by `cmd_spawn`.
    pub stops: Value,
    /// WHAT was requested (endpoint + CLI flags; never an observation).
    pub requested: Value,
    /// Bridge-observed identity (redacted, capped). Process bridges receive
    /// it as `--observed-target` and persist it verbatim into session.json;
    /// the browser bridge reports its own `/json/list` entry instead.
    pub observed: Value,
    /// One-line redacted hint for timeout/unhit diagnostics (no root-cause
    /// claim). Bridges append it; the CLI appends it to spawn errors.
    pub observed_hint: String,
}

// ---- target identity (M-I) ----

/// Per-field cap (chars) for observed identity strings.
pub const OBSERVED_FIELD_CAP: usize = 512;
/// Total cap (chars, serialized) for one observedTarget object.
pub const OBSERVED_TOTAL_CAP: usize = 2048;

/// Lowercase substrings marking a flag/value as secret. Matched against the
/// flag name (`--token`, `--password=`, `api-key:` …) case-insensitively;
/// separators `=`, `:`, or whitespace all count.
fn is_secret_flag(flag: &str) -> bool {
    let t = flag.trim_start_matches('-').to_ascii_lowercase();
    // Substring keys (long enough to avoid false positives like --author).
    const SUBSTR: [&str; 7] = [
        "password",
        "passwd",
        "secret",
        "apikey",
        "authorization",
        "authtoken",
        "accesstoken",
    ];
    let flat: String = t.chars().filter(|c| *c != '-' && *c != '_').collect();
    if SUBSTR.iter().any(|k| flat.contains(k)) {
        return true;
    }
    // Short keys only on token boundaries (--auth, --token, --pwd — but
    // never --author or --monkey). A token also matches as a SUFFIX
    // (-Dtoken=, mytoken=) but never as a mere prefix (--author keeps its
    // value: "auth" is a prefix of "author", not a suffix).
    const TOKEN: [&str; 3] = ["token", "auth", "pwd"];
    t.split(['-', '_', '.'])
        .any(|tok| TOKEN.iter().any(|k| tok == *k || tok.ends_with(k)))
}

/// Redact an argv for persistence/output: `--token <v>` drops the value,
/// `--password=<v>` / `key:<v>` mask the value, and the bare secret flag
/// itself is kept (only values are ever secret). Returns display strings.
pub fn redact_argv(argv: &[String]) -> Vec<Value> {
    let mut out = Vec::with_capacity(argv.len());
    let mut skip_next = false;
    for a in argv {
        if skip_next {
            out.push(Value::String("[redacted]".to_string()));
            skip_next = false;
            continue;
        }
        // Split head from value on the first `=` or `:` (not a bare flag).
        let split = a.find('=').or_else(|| a.find(':')).filter(|&i| i > 0);
        match split {
            Some(i) => {
                let (head, val) = a.split_at(i);
                if is_secret_flag(head) {
                    out.push(Value::String(format!(
                        "{head}{}{}",
                        &val[..1],
                        "[redacted]"
                    )));
                } else {
                    out.push(Value::String(a.clone()));
                }
            }
            None => {
                out.push(Value::String(a.clone()));
                if is_secret_flag(a) {
                    skip_next = true;
                }
            }
        }
    }
    out
}

/// Shared truncation idiom (mirrors every bridge's trunc helper):
/// `… (+N more chars)`.
fn trunc_chars(s: &str, limit: usize) -> String {
    let n = s.chars().count();
    if n <= limit {
        return s.to_string();
    }
    let kept: String = s.chars().take(limit).collect();
    format!("{kept}… (+{} more chars)", n - limit)
}

/// Cap one observedTarget object: every string field to 512 chars, then the
/// whole object to 2KB serialized (longest string shrinks first). Only the
/// known identity shapes are capped (process/tab); anything else passes
/// through untouched.
fn cap_observed(mut v: Value) -> Value {
    fn cap_str(s: &mut String) {
        if s.chars().count() > OBSERVED_FIELD_CAP {
            *s = trunc_chars(s, OBSERVED_FIELD_CAP);
        }
    }
    fn walk(v: &mut Value) {
        match v {
            Value::String(s) => cap_str(s),
            Value::Array(a) => a.iter_mut().for_each(walk),
            Value::Object(m) => m.values_mut().for_each(walk),
            _ => {}
        }
    }
    walk(&mut v);
    // Total cap: shrink the longest string until the object fits.
    while v.to_string().chars().count() > OBSERVED_TOTAL_CAP {
        fn longest(v: &mut Value) -> Option<&mut String> {
            match v {
                Value::String(s) if s.chars().count() > 1 => Some(s),
                Value::Array(a) => a.iter_mut().filter_map(longest).max_by_key(|s| s.len()),
                Value::Object(m) => m.values_mut().filter_map(longest).max_by_key(|s| s.len()),
                _ => None,
            }
        }
        match longest(&mut v) {
            Some(s) => {
                let n = s.chars().count();
                *s = trunc_chars(s, n.saturating_sub(64).max(1));
            }
            None => break,
        }
    }
    v
}

fn unavailable(field: &str, reason: &str) -> Value {
    json!({"field": field, "reason": reason})
}

/// Launch identity from our own spawn (executable + argv + cwd are
/// genuinely launcher-observed; the adapter-spawned pid is not reported).
pub fn launch_observed(exe: &str, argv: Vec<String>, source: &str) -> Value {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .ok();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    cap_observed(json!({
        "kind": "process",
        "pid": Value::Null,
        "executable": exe,
        "argv": redact_argv(&argv),
        "cwd": cwd.map(Value::String).unwrap_or(Value::Null),
        "source": source,
        "observedAt": now,
        "unavailable": [unavailable("pid", "target pid is not reported by the adapter")],
        "warnings": Value::Array(vec![]),
    }))
}

/// Attach identity via a bounded OS-native lookup of the localhost listener.
/// No target eval, no env, no shell reparse: pids come from the kernel
/// (lsof on macOS, /proc on Linux), details from /proc or lsof/ps.
/// Anything missing becomes a structured `unavailable` entry — never a
/// fabricated value.
pub fn attach_observed(host: &str, port: u16) -> Value {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut missing: Vec<Value> = vec![unavailable("pid", "no independent pid source")];
    let mut warnings: Vec<Value> = vec![];
    let mut base = json!({
        "kind": "process",
        "pid": Value::Null,
        "executable": Value::Null,
        "argv": Value::Null,
        "cwd": Value::Null,
        "source": Value::Null,
        "observedAt": now,
    });
    let local = matches!(host, "localhost" | "127.0.0.1" | "::1");
    if !local {
        missing = vec![
            unavailable("pid", "remote host has no local source"),
            unavailable("executable", "remote host has no local source"),
            unavailable("argv", "remote host has no local source"),
            unavailable("cwd", "remote host has no local source"),
        ];
    } else if let Some(info) = port_lookup(port) {
        missing.clear();
        base["pid"] = json!(info.pid);
        base["source"] = Value::String(info.source);
        match info.exe {
            Some(e) => base["executable"] = Value::String(e),
            None => missing.push(unavailable("executable", info.exe_reason)),
        }
        if info.argv.is_empty() {
            missing.push(unavailable("argv", info.argv_reason));
        } else {
            base["argv"] = Value::Array(redact_argv(&info.argv));
        }
        match info.cwd {
            Some(c) => base["cwd"] = Value::String(c),
            None => missing.push(unavailable("cwd", info.cwd_reason)),
        }
        for w in info.warnings {
            warnings.push(Value::String(w));
        }
    }
    if base["executable"].is_null() && base["pid"].is_null() {
        warnings.push(Value::String(
            "identity-unverified: showing attach endpoint only".to_string(),
        ));
    }
    base["unavailable"] = Value::Array(missing);
    base["warnings"] = Value::Array(warnings);
    cap_observed(base)
}

struct ProcInfo {
    pid: u32,
    source: String,
    exe: Option<String>,
    exe_reason: &'static str,
    argv: Vec<String>,
    argv_reason: &'static str,
    cwd: Option<String>,
    cwd_reason: &'static str,
    warnings: Vec<String>,
}

fn run_bounded(cmd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Kernel-observed listener pid for a localhost port. macOS: lsof;
/// Linux: /proc/net/tcp{,6} + fd scan (no subprocess at all).
#[cfg(target_os = "linux")]
fn port_lookup(port: u16) -> Option<ProcInfo> {
    let inode = tcp_listen_inode(port)?;
    let pid = pid_holding_socket(&inode)?;
    let exe = std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|p| p.to_string_lossy().to_string());
    let raw = std::fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .unwrap_or_default();
    let argv: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect();
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|p| p.to_string_lossy().to_string());
    Some(ProcInfo {
        pid,
        source: "os-proc".to_string(),
        exe,
        exe_reason: "unreadable /proc exe link",
        argv,
        argv_reason: "unreadable /proc cmdline",
        cwd,
        cwd_reason: "unreadable /proc cwd link",
        warnings: vec![],
    })
}

/// Parse /proc/net/tcp{,6} for a loopback LISTEN socket's inode.
#[cfg(target_os = "linux")]
fn tcp_listen_inode(port: u16) -> Option<String> {
    let want = format!(":{port:04X}");
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let raw = std::fs::read_to_string(file).ok()?;
        for line in raw.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 10 {
                continue;
            }
            // f[1] local_address (hex ip:hex port), f[3] state (0A=LISTEN).
            if f[3] != "0A" || !f[1].ends_with(&want) {
                continue;
            }
            let ip = f[1].split(':').next().unwrap_or("");
            // Loopback only: 127.0.0.1 or ::1 (zero-padded hex forms).
            let loopback = ip == "0100007F"
                || ip == "00000000000000000000000001000000"
                || ip == "0000000000000000FFFF00000100007F";
            if loopback {
                return Some(f[9].to_string());
            }
        }
    }
    None
}

/// First numeric /proc pid whose fd table holds `socket:[inode]`.
#[cfg(target_os = "linux")]
fn pid_holding_socket(inode: &str) -> Option<u32> {
    let want = format!("socket:[{inode}]");
    let procs = std::fs::read_dir("/proc").ok()?;
    let mut pids: Vec<u32> = procs
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse::<u32>().ok())
        .collect();
    pids.sort_unstable();
    for pid in pids {
        let dir = format!("/proc/{pid}/fd");
        let fds = match std::fs::read_dir(&dir) {
            Ok(d) => d,
            Err(_) => continue,
        };
        for fd in fds.flatten() {
            if std::fs::read_link(fd.path())
                .ok()
                .map(|p| p.to_string_lossy() == want)
                .unwrap_or(false)
            {
                return Some(pid);
            }
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn port_lookup(port: u16) -> Option<ProcInfo> {
    // macOS (/proc absent): lsof gives the kernel-observed pid plus the
    // executable/cwd vnodes; ps supplies the command string (single-string
    // source, so the argv split is marked approximate).
    let pids = run_bounded(
        "lsof",
        &[&format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t", "-P", "-n"],
    )?;
    let pid: u32 = pids.lines().filter_map(|l| l.trim().parse().ok()).next()?;
    let field = |args: &[&str]| -> Option<String> {
        run_bounded("lsof", args).and_then(|out| {
            out.lines()
                .filter_map(|l| l.strip_prefix('n'))
                .next()
                .map(|s| s.to_string())
        })
    };
    let exe = field(&["-p", &pid.to_string(), "-a", "-d", "txt", "-F", "n"]);
    let cwd = field(&["-p", &pid.to_string(), "-a", "-d", "cwd", "-F", "n"]);
    let cmdline = run_bounded("ps", &["-p", &pid.to_string(), "-o", "args="])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let mut warnings = vec![];
    let argv: Vec<String> = match cmdline {
        Some(s) => {
            warnings.push("argv-approximate: macOS ps reports a single command string".to_string());
            s.split_whitespace().map(|t| t.to_string()).collect()
        }
        None => vec![],
    };
    Some(ProcInfo {
        pid,
        source: "os-lsof-ps".to_string(),
        exe,
        exe_reason: "lsof reports no executable vnode",
        argv,
        argv_reason: "ps reports no command string",
        cwd,
        cwd_reason: "lsof reports no cwd vnode",
        warnings,
    })
}

/// One-line redacted hint for timeout/unhit diagnostics. Names the observed
/// target without claiming root cause (`verified` stays plant-only).
pub fn compact_hint(observed: &Value) -> String {
    let hint = compact_hint_inner(observed);
    trunc_chars(&hint, 200)
}

fn compact_hint_inner(observed: &Value) -> String {
    if observed.get("kind").and_then(|k| k.as_str()) == Some("tab") {
        let url = observed.get("url").and_then(|u| u.as_str()).unwrap_or("?");
        return format!("target identity: tab {url}");
    }
    let exe = observed
        .get("executable")
        .and_then(|e| e.as_str())
        .unwrap_or("?");
    let argv: Vec<&str> = observed
        .get("argv")
        .and_then(|a| a.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).take(3).collect())
        .unwrap_or_default();
    let cwd = observed.get("cwd").and_then(|c| c.as_str()).unwrap_or("?");
    if exe == "?" && argv.is_empty() {
        return "target identity unavailable (no independent source)".to_string();
    }
    format!("target identity: {} {} (cwd {})", exe, argv.join(" "), cwd)
}

/// Summarize WHAT the session targets by scanning bridge args for known
/// flags. Derived automatically at spawn — the agent never writes this.
/// Unknown flags are ignored (forward-compat with new target options).
pub fn target_summary(args: &[String]) -> Value {
    let mut map = serde_json::Map::new();
    // Flag -> display key. Values are single tokens (paths, ports, hosts).
    let keys = [
        ("--program", "program"),
        ("--module", "module"),
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
        // Opt-in multi-target flags carry no value.
        if a == "--subprocess" {
            map.insert("subprocess".to_string(), Value::Bool(true));
            i += 1;
            continue;
        }
        if a == "--workers" {
            map.insert("workers".to_string(), Value::Bool(true));
            i += 1;
            continue;
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
    // Process bridges persist our redacted observed identity verbatim into
    // session.json (atomic, bridge-owned write) and use the hint in
    // timeout/unhit diagnostics. The browser reports its own /json/list
    // entry instead — no process claim is passed. Inserted BEFORE the `--`
    // program-args separator: anything after it belongs to the target.
    if spec.lang != "browser" {
        let extra = vec![
            "--observed-target".to_string(),
            spec.observed.to_string(),
            "--observed-hint".to_string(),
            spec.observed_hint.clone(),
        ];
        match args.iter().position(|a| a == "--") {
            Some(pos) => {
                let tail = args.split_off(pos);
                args.extend(extra);
                args.extend(tail);
            }
            None => args.extend(extra),
        }
    }

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
                Ok(mut data) => {
                    // Immediate start/attach response carries identity:
                    // requested from the CLI flags, observed from the
                    // bridge-persisted cache (browser) or our own lookup
                    // (process targets). Never invent: missing files read
                    // as null.
                    data["requestedTarget"] = spec.requested.clone();
                    data["observedTarget"] = cached_observed(&dir, spec);
                    return Ok(data);
                }
                Err(e) => {
                    // Fast-exit race: a pending-breakpoint target publishes
                    // session.json (dead) before error.json lands, and the
                    // first forward already fails while logs exist on disk.
                    // The error file is the truth — re-check it first
                    // (immediate, then a bounded settle at the wait-loop
                    // cadence covering bridge-cleanup lag), never the logs.
                    if let Some(msg) = settle_for_bridge_error(&dir) {
                        reap(&mut child);
                        let _ = std::fs::remove_dir_all(&dir);
                        anyhow::bail!("{msg}");
                    }
                    // Fast program + logpoints-only: the target may exit before
                    // the first read, but its logs are already on disk. The
                    // daemon reaps itself on exit; the session stays for them.
                    // Gated to explicit logpoints-only intent: any stop that
                    // requires a first stop (breaks incl. method:/exc:,
                    // watches, exits) must surface the failure instead of a
                    // logs-shaped Ok with no snapshot.
                    if logpoints_only_intent(&spec.stops) {
                        if let Some(logs) = read_logs_file(&dir, 50) {
                            let mut out = json!({
                                "warning": format!("{e:#} (target already exited)"),
                                "logs": logs,
                            });
                            out["requestedTarget"] = spec.requested.clone();
                            out["observedTarget"] = cached_observed(&dir, spec);
                            return Ok(out);
                        }
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
            anyhow::bail!(
                "timed out waiting for first breakpoint. {log_tail}. {}",
                spec.observed_hint
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Bridge-persisted observed identity for a live session dir (session.json
/// `observedTarget`, written atomically by the bridge). Falls back to the
/// spawn-time value (identical redacted content) when the file has no copy
/// yet; unknown shapes read as null, never fabricated.
fn cached_observed(dir: &std::path::Path, spec: &SpawnSpec) -> Value {
    std::fs::read_to_string(dir.join("session.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v.get("observedTarget").cloned())
        .filter(|v| !v.is_null())
        .unwrap_or_else(|| spec.observed.clone())
}

/// Re-fetch location + threads + frames without resuming, with the cached
/// redacted identity attached (same object as the start/attach response).
/// An optional target selector routes to one parked target; identity stays
/// session-level (requested/observed are main-focused, like status).
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
    let observed = std::fs::read_to_string(dir.join("session.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v.get("observedTarget").cloned())
        .unwrap_or(Value::Null);
    if let Value::Object(ref mut m) = resp {
        m.insert("requestedTarget".to_string(), requested);
        m.insert("observedTarget".to_string(), observed);
    }
    Ok(resp)
}

/// Bridge setup-failure truth with a bounded settle: error.json can trail
/// the dead session.json by bridge-cleanup lag (the full-suite test11
/// race). Immediate check first — no added latency when the files land in
/// order — then a bounded re-poll at the wait-loop cadence. Only entered
/// after a failed first forward, so the success path never sleeps.
/// Returns the bridge message when the file lands in budget.
fn settle_for_bridge_error(dir: &std::path::Path) -> Option<String> {
    if dir.join("error.json").exists() {
        return Some(read_bridge_error(dir));
    }
    let deadline = Instant::now() + Duration::from_millis(2000);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        if dir.join("error.json").exists() {
            return Some(read_bridge_error(dir));
        }
    }
    None
}

/// Explicit logpoints-only intent: logpoints armed, and no stop that
/// requires a first stop (breaks incl. method:/exc:, watches, exits).
/// Only this intent may take the collect-logs Ok path on fast exit.
fn logpoints_only_intent(stops: &Value) -> bool {
    fn nonempty(stops: &Value, key: &str) -> bool {
        stops
            .get(key)
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false)
    }
    nonempty(stops, "logpoints")
        && !nonempty(stops, "breaks")
        && !nonempty(stops, "watches")
        && !nonempty(stops, "exits")
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
    Ok(json!({"closed": name, "confirmed": confirmed, "target": "main"}))
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
    // Identity: requested from the spawn-time intent, observed from the
    // bridge-persisted session.json cache. Legacy dirs predate both —
    // null there is honest ("unknown"), never fabricated.
    let requested = stops_parsed
        .as_ref()
        .and_then(|v| v.get("requestedTarget").cloned())
        .unwrap_or(Value::Null);
    let observed = parsed.get("observedTarget").cloned().unwrap_or(Value::Null);
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
        "requestedTarget": requested,
        "observedTarget": observed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logpoints_only_intent_gates_the_collect_logs_fallback() {
        // Logpoints alone: the fallback may apply.
        assert!(logpoints_only_intent(&json!({
            "breaks": [], "logpoints": ["a.py:1:hit"],
            "watches": [], "exits": [],
        })));
        // Any first-stop stop kills it: line break, method:/exc: (both
        // live in "breaks"), watch, exit.
        for stops in [
            json!({"breaks": ["a.py:1"], "logpoints": ["a.py:1:hit"],
                   "watches": [], "exits": []}),
            json!({"breaks": ["method:C.m"], "logpoints": ["a.py:1:hit"],
                   "watches": [], "exits": []}),
            json!({"breaks": ["exc:Boom"], "logpoints": ["a.py:1:hit"],
                   "watches": [], "exits": []}),
            json!({"breaks": [], "logpoints": ["a.py:1:hit"],
                   "watches": ["C.f"], "exits": []}),
            json!({"breaks": [], "logpoints": ["a.py:1:hit"],
                   "watches": [], "exits": ["C.m"]}),
            // No logpoints at all (breakpoint-only and empty intents).
            json!({"breaks": ["a.py:1"], "logpoints": [],
                   "watches": [], "exits": []}),
            json!({"breaks": [], "logpoints": [], "watches": [], "exits": []}),
            json!({}),
        ] {
            assert!(!logpoints_only_intent(&stops), "{stops}");
        }
    }

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
    fn target_normalize_keeps_main_and_rejects_on_single_target_langs() {
        // Omitted and explicit main are identical (legacy wire behavior).
        assert_eq!(normalize_target_for_lang("java", None).unwrap(), None);
        assert_eq!(
            normalize_target_for_lang("browser", Some("main")).unwrap(),
            None
        );
        assert_eq!(normalize_target_for_lang("py", Some("main")).unwrap(), None);
        // Multi-target langs pass selectors through for bridge validation.
        assert_eq!(
            normalize_target_for_lang("py", Some("child:7")).unwrap(),
            Some("child:7".to_string())
        );
        assert_eq!(
            normalize_target_for_lang("node", Some("worker:abc")).unwrap(),
            Some("worker:abc".to_string())
        );
        // Main-only adapters fail fast instead of silently serving main.
        assert!(normalize_target_for_lang("java", Some("child:7")).is_err());
        assert!(normalize_target_for_lang("browser", Some("worker:abc")).is_err());
    }

    #[test]
    fn targets_in_reports_uniform_main_roster() {
        let dir = tmpdir("targets-main");
        std::fs::write(
            dir.join("session.json"),
            r#"{"name":"x","kind":"attach","port":0,"stopped":true,
                "lastStop":{"file":"a.java","line":3,"method":"m"},
                "observedTarget":{"kind":"process"}}"#,
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

    #[test]
    fn target_summary_records_module() {
        let v = target_summary(&[
            "--module".to_string(),
            "mypkg.mod".to_string(),
            "--timeout".to_string(),
            "20".to_string(),
        ]);
        assert_eq!(v["module"], json!("mypkg.mod"));
        assert!(v.get("program").is_none());
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
    fn redact_argv_masks_secret_values_only() {
        let argv = [
            "python3",
            "app.py",
            "--token",
            "S3CR3T",
            "--password=hunter2",
            "--api-key:ABC",
            "authorization=Bearer XYZ",
            "--mode",
            "fast",
        ];
        let red = redact_argv(&argv.map(|s| s.to_string()));
        let strs: Vec<&str> = red.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(
            strs,
            vec![
                "python3",
                "app.py",
                "--token",
                "[redacted]",
                "--password=[redacted]",
                "--api-key:[redacted]",
                "authorization=[redacted]",
                "--mode",
                "fast",
            ]
        );
        let joined = strs.join(" ");
        assert!(!joined.contains("S3CR3T"));
        assert!(!joined.contains("hunter2"));
        assert!(!joined.contains("XYZ"));
    }

    #[test]
    fn redact_argv_case_insensitive_and_dash_forms() {
        let argv = ["--ApiKey", "K", "--AUTH=Q", "X-Authorization: Y"];
        let red = redact_argv(&argv.map(|s| s.to_string()));
        let strs: Vec<&str> = red.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(
            strs,
            vec![
                "--ApiKey",
                "[redacted]",
                "--AUTH=[redacted]",
                "X-Authorization:[redacted]"
            ]
        );
        assert!(!strs.join(" ").contains(" K"));
        assert!(!strs.join(" ").contains("=Q"));
        assert!(!strs.join(" ").contains(": Y"));
        // JVM -D properties: suffix match redacts, prefix never does.
        let argv = ["-Dtoken=S3CR3T", "--author", "Jane"];
        let red = redact_argv(&argv.map(|s| s.to_string()));
        let strs: Vec<&str> = red.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(strs, vec!["-Dtoken=[redacted]", "--author", "Jane"]);
    }

    #[test]
    fn observed_fields_capped_and_total_bounded() {
        let long = "x".repeat(600);
        let v = cap_observed(json!({
            "kind": "process", "pid": 1, "executable": long,
            "argv": [long, long], "cwd": "/t",
            "source": "s", "observedAt": 1,
            "unavailable": [], "warnings": [],
        }));
        assert!(v["executable"].as_str().unwrap().contains("(+"));
        for entry in v["argv"].as_array().unwrap() {
            assert!(entry.as_str().unwrap().chars().count() <= OBSERVED_FIELD_CAP + 30);
        }
        assert!(v.to_string().chars().count() <= OBSERVED_TOTAL_CAP);
    }

    #[test]
    fn attach_remote_host_is_structured_unavailable() {
        let v = attach_observed("example.com", 5678);
        assert_eq!(v["kind"], json!("process"));
        assert!(v["pid"].is_null());
        let unavailable = v["unavailable"].as_array().unwrap();
        assert!(unavailable.iter().any(|u| u["field"] == json!("pid")));
        let warnings: Vec<&str> = v["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|w| w.as_str())
            .collect();
        assert!(warnings.iter().any(|w| w.contains("identity-unverified")));
        // No env, no raw secrets: the whole object serializes clean.
        assert!(v.to_string().len() <= OBSERVED_TOTAL_CAP);
    }

    #[test]
    fn attach_closed_local_port_is_unavailable_not_fabricated() {
        // Nothing listens on port 1: lookup must yield structured
        // unavailable, never an invented pid.
        let v = attach_observed("localhost", 1);
        assert!(v["pid"].is_null());
        assert!(v["executable"].is_null());
        assert!(!v["unavailable"].as_array().unwrap().is_empty());
    }

    #[test]
    fn compact_hint_names_target_without_root_cause() {
        let v = launch_observed(
            "python3",
            vec!["python3".to_string(), "a.py".to_string()],
            "launcher-args",
        );
        let hint = compact_hint(&v);
        assert!(hint.contains("python3"));
        assert!(!hint.contains("reason"));
        let tab = json!({"kind": "tab", "url": "http://h/app.js"});
        assert!(compact_hint(&tab).contains("http://h/app.js"));
        let missing = attach_observed("example.com", 9);
        assert!(compact_hint(&missing).contains("unavailable"));
    }
}
