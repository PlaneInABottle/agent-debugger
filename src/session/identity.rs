// See `mod.rs` for the one-way dependency DAG.
use super::paths::normalize_attach_host;
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;

// ---- target identity (M-I) ----

/// Per-field cap (chars) for layered identity strings.
pub const IDENTITY_FIELD_CAP: usize = 512;

/// Total cap (chars, serialized) for one layered identity object.
pub const IDENTITY_TOTAL_CAP: usize = 2048;

/// Element cap per array inside a layered identity (argv/cmdline). High-count
/// argv shapes (~80+ small strings) converge via this count cap plus the
/// total loop's guaranteed shrink-or-collapse, never by re-marking forever.
pub const IDENTITY_ARRAY_CAP: usize = 32;

/// Lowercase substrings marking a flag/value as secret. Matched against the
/// flag name (`--token`, `--password=`, `api-key:` …) case-insensitively;
/// separators `=`, `:`, or whitespace all count.
pub(crate) fn is_secret_flag(flag: &str) -> bool {
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
    // Short keys only on token boundaries (--auth, --token, --pwd, --pass,
    // --pw — but never --author or --passage). A token also matches as a
    // SUFFIX (-Dtoken=, mytoken=, --db-pass) but never as a mere prefix
    // (--author keeps its value: "auth" is a prefix of "author", not a
    // suffix; --passage likewise never matches "pass").
    const TOKEN: [&str; 5] = ["token", "auth", "pwd", "pass", "pw"];
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
        // A pending secret value that turns out to be another flag was never
        // a value: keep it on the flag path (its own `=`/`:` value still
        // redacts, its own bare form still arms skip_next) instead of
        // swallowing it as "[redacted]" and leaking the real value after it
        // (`--token --password hunter2` must hide hunter2).
        if skip_next {
            skip_next = false;
            if !(a.starts_with('-') && a.len() > 1) {
                out.push(Value::String("[redacted]".to_string()));
                continue;
            }
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
pub(crate) fn trunc_chars(s: &str, limit: usize) -> String {
    let n = s.chars().count();
    if n <= limit {
        return s.to_string();
    }
    let kept: String = s.chars().take(limit).collect();
    format!("{kept}… (+{} more chars)", n - limit)
}

/// Cap one layered identity object: every string field to 512 chars, every
/// array to a bounded element count (tail marker), then the whole object to
/// 2KB serialized. Only the known identity shapes are capped (process/tab);
/// anything else passes through untouched. The total loop always terminates:
/// each pass either shrinks the longest string or, when the trunc marker
/// would not shrink it, collapses it to a 1-char marker (strictly smaller
/// than any picked string), so ~80+ small argv entries converge by dropping
/// rather than re-marking forever.
pub(crate) fn cap_identity(mut v: Value) -> Value {
    fn cap_str(s: &mut String) {
        if s.chars().count() > IDENTITY_FIELD_CAP {
            *s = trunc_chars(s, IDENTITY_FIELD_CAP);
        }
    }
    fn walk(v: &mut Value) {
        match v {
            Value::String(s) => cap_str(s),
            Value::Array(a) => {
                // Deterministic count cap: keep the head, note the dropped
                // tail. Bounds high-count argv/cmdline shapes before the
                // total loop runs.
                if a.len() > IDENTITY_ARRAY_CAP {
                    let dropped = a.len() - IDENTITY_ARRAY_CAP;
                    a.truncate(IDENTITY_ARRAY_CAP);
                    a.push(Value::String(format!("… (+{dropped} more)")));
                }
                a.iter_mut().for_each(walk);
            }
            Value::Object(m) => m.values_mut().for_each(walk),
            _ => {}
        }
    }
    walk(&mut v);
    // Total cap: shrink the longest string until the object fits. The trunc
    // marker itself costs ~17 chars, so re-truncating an already-short
    // marked string can grow it — in that case collapse to "…" instead.
    // Either branch strictly reduces the serialized size, guaranteeing
    // progress; a bounded pass count is the backstop, not the mechanism.
    let mut passes = 0;
    while v.to_string().chars().count() > IDENTITY_TOTAL_CAP && passes < 4096 {
        passes += 1;
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
                let cand = trunc_chars(s, n.saturating_sub(64).max(1));
                if cand.chars().count() < n {
                    *s = cand;
                } else {
                    *s = "…".to_string();
                }
            }
            None => break,
        }
    }
    v
}

pub(crate) fn unavailable(field: &str, reason: &str) -> Value {
    json!({"field": field, "reason": reason})
}

/// Launch seed identity: layered roles built directly from our own spawn
/// (executable + argv + cwd are genuinely launcher-observed; nothing here
/// is OS-corroborated or protocol-confirmed). `debuggee.confidence` stays
/// `unavailable` with `source: "launcher-args"` — launcher truth is not
/// OS-corroborated identity; the bridge's protocol event upgrades it.
pub fn launch_seed(exe: &str, argv: Vec<String>) -> Value {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .ok();
    let now = identity_now();
    cap_identity(json!({
        "debuggee": {
            "kind": "process",
            "pid": Value::Null,
            "executable": exe,
            "argv": redact_argv(&argv),
            "cwd": cwd.map(Value::String).unwrap_or(Value::Null),
            "source": "launcher-args",
            "confidence": "unavailable",
            "observedAt": now,
            "unavailable": [unavailable(
                "pid",
                "not protocol-confirmed (launcher args only); protocol event upgrades debuggee",
            )],
        },
        "endpoint": {
            "confidence": "unavailable",
            "observedAt": now,
            "unavailable": [unavailable(
                "endpoint",
                "no listener yet (launcher args only)",
            )],
        },
        "adapter": {
            "confidence": "unavailable",
            "observedAt": now,
            "unavailable": [unavailable(
                "adapter",
                "adapter not started (launcher args only)",
            )],
        },
    }))
}

/// Attach seed identity via a bounded OS-native lookup of the localhost
/// listener. No target eval, no env, no shell reparse: pids come from the
/// kernel (lsof on macOS, /proc on Linux), details from /proc or lsof/ps.
/// Anything missing becomes a structured `unavailable` entry — never a
/// fabricated value. The `debuggee` role stays `unavailable` until the
/// bridge's protocol event confirms it; no adapter name is guessed.
pub fn attach_seed(host: &str, port: u16) -> Value {
    let now = identity_now();
    let debuggee = json!({
        "kind": "process",
        "pid": Value::Null,
        "confidence": "unavailable",
        "observedAt": now,
        "unavailable": [unavailable(
            "pid",
            "debuggee pid not confirmed by protocol (seed only)",
        )],
    });
    let adapter = json!({
        "confidence": "unavailable",
        "observedAt": now,
        "unavailable": [unavailable(
            "adapter",
            "adapter not confirmed by protocol (seed only)",
        )],
    });
    if normalize_attach_host(host) != "loopback" {
        let missing = ["ownerPid", "executable", "argv", "cwd"]
            .iter()
            .map(|f| unavailable(f, "remote host has no local source"))
            .collect::<Vec<_>>();
        return cap_identity(json!({
            "debuggee": debuggee,
            "endpoint": {
                "host": host,
                "port": port,
                "ownerPid": Value::Null,
                "confidence": "unavailable",
                "observedAt": now,
                "unavailable": missing,
            },
            "adapter": adapter,
        }));
    }
    match port_lookup(port) {
        Some(info) => {
            let mut missing: Vec<Value> = vec![];
            let mut warnings: Vec<Value> = vec![];
            let mut endpoint = json!({
                "host": host,
                "port": port,
                "ownerPid": info.pid,
                "source": info.source,
                "confidence": "os-corroborated",
                "observedAt": now,
            });
            match info.exe {
                Some(e) => endpoint["executable"] = Value::String(e),
                None => missing.push(unavailable("executable", info.exe_reason)),
            }
            if info.argv.is_empty() {
                missing.push(unavailable("argv", info.argv_reason));
            } else {
                endpoint["argv"] = Value::Array(redact_argv(&info.argv));
            }
            match info.cwd {
                Some(c) => endpoint["cwd"] = Value::String(c),
                None => missing.push(unavailable("cwd", info.cwd_reason)),
            }
            for w in info.warnings {
                warnings.push(Value::String(w));
            }
            endpoint["unavailable"] = Value::Array(missing);
            endpoint["warnings"] = Value::Array(warnings);
            cap_identity(json!({
                "debuggee": debuggee,
                "endpoint": endpoint,
                "adapter": adapter,
            }))
        }
        None => cap_identity(json!({
            "debuggee": debuggee,
            "endpoint": {
                "host": host,
                "port": port,
                "ownerPid": Value::Null,
                "confidence": "unavailable",
                "observedAt": now,
                "unavailable": [unavailable("ownerPid", "no independent pid source")],
            },
            "adapter": adapter,
        })),
    }
}

/// One-line redacted hint derived from a layered seed identity (no
/// root-cause claim). Prefers the debuggee's launcher-observed
/// executable/argv, then the endpoint's OS-corroborated copy; a null or
/// empty identity reads as empty (the caller omits the suffix).
pub fn identity_hint(seed: &Value) -> String {
    let hint = identity_hint_inner(seed);
    trunc_chars(&hint, 200)
}

pub(crate) fn identity_hint_inner(seed: &Value) -> String {
    for role in ["debuggee", "endpoint"] {
        if let Some(r) = seed.get(role) {
            let exe = r.get("executable").and_then(|e| e.as_str()).unwrap_or("?");
            let argv: Vec<&str> = r
                .get("argv")
                .and_then(|a| a.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).take(3).collect())
                .unwrap_or_default();
            let cwd = r.get("cwd").and_then(|c| c.as_str()).unwrap_or("?");
            if exe != "?" || !argv.is_empty() {
                return format!("target identity: {} {} (cwd {})", exe, argv.join(" "), cwd);
            }
        }
    }
    if seed.is_null() {
        return String::new();
    }
    "target identity unavailable (no independent source)".to_string()
}

pub(crate) struct ProcInfo {
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

/// Bounded subprocess capture for the macOS lookup (lsof/ps). Same
/// spawn-and-wait-with-timeout idiom as the bridge provisioning path: ~5s
/// hard bound, None on timeout/failure so attach degrades to structured
/// `unavailable` instead of hanging the CLI. No new deps (std mpsc only).
pub(crate) fn run_bounded(cmd: &str, args: &[&str]) -> Option<String> {
    const RUN_BOUNDED_TIMEOUT: Duration = Duration::from_secs(5);
    let child = std::process::Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let child_id = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(RUN_BOUNDED_TIMEOUT) {
        Ok(Ok(out)) if out.status.success() => String::from_utf8(out.stdout).ok(),
        // Nonzero exit, spawn/wait failure, or undecodable bytes: no data.
        Ok(_) => None,
        Err(_) => {
            // Timed out: best-effort kill (the waiter thread reaps the
            // child whenever it actually exits); the lookup reports
            // unavailable instead of hanging attach.
            let _ = std::process::Command::new("kill")
                .arg(child_id.to_string())
                .output();
            None
        }
    }
}

/// Kernel-observed listener pid for a localhost port. macOS: lsof;
/// Linux: /proc/net/tcp{,6} + fd scan (no subprocess at all).
#[cfg(target_os = "linux")]
pub(crate) fn port_lookup(port: u16) -> Option<ProcInfo> {
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
pub(crate) fn tcp_listen_inode(port: u16) -> Option<String> {
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
            // Loopback (`127.0.0.1`, `::1` incl. the v4-mapped form) plus
            // wildcard listeners (`0.0.0.0`, `::`): a server bound to all
            // interfaces still serves loopback attach destinations, so its
            // pid is the right owner/coroboration for our lookup.
            let loopback = ip == "0100007F"
                || ip == "00000000000000000000000001000000"
                || ip == "0000000000000000FFFF00000100007F"
                || ip == "00000000"
                || ip == "00000000000000000000000000000000"
                || ip == "0000000000000000FFFF000000000000";
            if loopback {
                return Some(f[9].to_string());
            }
        }
    }
    None
}

/// First numeric /proc pid whose fd table holds `socket:[inode]`.
#[cfg(target_os = "linux")]
pub(crate) fn pid_holding_socket(inode: &str) -> Option<u32> {
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
pub(crate) fn port_lookup(port: u16) -> Option<ProcInfo> {
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
pub(crate) fn stops_armed(stops: &Value) -> Value {
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

/// Layered target identity for a live session dir (session.json
/// `targetIdentity`: `{debuggee, endpoint, adapter}` roles built by the
/// bridge from protocol-confirmed + OS-corroborated sources). No spawn-time
/// fallback: the roles need bridge-observed protocol facts the CLI never
/// has, so a missing copy reads as null (honest "unknown"), never a
/// fabricated role. Only a valid layered object is ever echoed (re-redacted
/// + capped defensively); flat/malformed shapes read as null.
pub(crate) fn cached_target_identity(dir: &std::path::Path) -> Value {
    let stored = std::fs::read_to_string(dir.join("session.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v.get("targetIdentity").cloned());
    layered_identity_or_null(stored)
}

/// Canonical persisted-identity read for status/spawn/context/roster: a
/// valid layered `{debuggee, endpoint, adapter}` object echoes back
/// (sanitized: re-redacted + capped, idempotent over bridge output);
/// anything else — flat legacy, malformed, non-object, absent — reads as
/// null, never echoed.
pub(crate) fn layered_identity_or_null(stored: Option<Value>) -> Value {
    match stored {
        Some(ref v) if is_layered_identity(v) => sanitize_layered_identity(v),
        _ => Value::Null,
    }
}

/// Collision-safe owner identity for `endpoint-already-attached`: always a
/// layered `{debuggee, endpoint, adapter}` object. Sources: the owner's
/// cached session.json `targetIdentity` when it is a valid layered object
/// (re-redacted + capped defensively so no stale raw secret leaks);
/// otherwise all three roles unavailable — no pid is ever promoted from
/// legacy or malformed stored state (on a wrapped Python server the
/// listener-owner pid is the adapter, not the debuggee).
pub(crate) fn owner_target_identity(sessions_root: &std::path::Path, owner: &str) -> Value {
    let dir = sessions_root.join(owner);
    let stored: Option<Value> = std::fs::read_to_string(dir.join("session.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v.get("targetIdentity").cloned());
    match stored {
        // Owner already publishes layered roles: keep them (sanitized).
        Some(v) if is_layered_identity(&v) => sanitize_layered_identity(&v),
        // Anything else (legacy owner, malformed identity, unpublished
        // lock holder): all unavailable, nothing killable inferable.
        _ => unavailable_identity("owner has no layered identity"),
    }
}

/// Valid layered identity: an object carrying exactly the three role keys
/// (`debuggee`, `endpoint`, `adapter`, each an object or null; additional
/// top-level metadata is allowed). A flat legacy shape (top-level
/// `pid`/`argv`/`executable`) never qualifies, even when it happens to
/// carry one of the role names.
pub(crate) fn is_layered_identity(v: &Value) -> bool {
    let m = match v.as_object() {
        Some(m) => m,
        None => return false,
    };
    for role in ["debuggee", "endpoint", "adapter"] {
        match m.get(role) {
            Some(Value::Object(_)) | Some(Value::Null) => {}
            _ => return false,
        }
    }
    // Flat-shape guard: a top-level pid/argv/executable/cwd/kind marks a
    // legacy flat view, never the layered identity.
    for flat in ["pid", "argv", "executable", "cwd"] {
        if m.contains_key(flat) {
            return false;
        }
    }
    true
}

/// Re-redact + cap a valid layered identity before it leaves the CLI: every
/// `argv` array is passed through the secret redactor again (idempotent —
/// a stale raw secret cached before redaction never leaks), then each role
/// object is capped with the layered-identity caps. Non-object roles read
/// as null; extra top-level metadata passes through capped.
pub(crate) fn sanitize_layered_identity(v: &Value) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(m) = v.as_object() {
        for (k, role) in m {
            if ["debuggee", "endpoint", "adapter"].contains(&k.as_str()) {
                out.insert(
                    k.clone(),
                    match role {
                        Value::Object(_) => {
                            let mut r = role.clone();
                            reredact_argv_in(&mut r);
                            cap_identity(r)
                        }
                        Value::Null => Value::Null,
                        _ => Value::Null,
                    },
                );
            } else {
                let mut extra = role.clone();
                reredact_argv_in(&mut extra);
                out.insert(k.clone(), cap_identity(extra));
            }
        }
    }
    // Roles are mandatory on the way out: a stored object that passed the
    // shape check always has them, but fill defensively so a hand-built
    // value can never leave a role missing.
    let now = identity_now();
    for role in ["debuggee", "endpoint", "adapter"] {
        if !out.contains_key(role) {
            out.insert(
                role.to_string(),
                json!({"confidence": "unavailable", "observedAt": now,
                       "unavailable": [{"field": role, "reason": "role missing from stored identity"}]}),
            );
        }
    }
    Value::Object(out)
}

/// Re-run argv redaction in place over every `argv` array in a role value.
pub(crate) fn reredact_argv_in(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for (k, child) in m.iter_mut() {
                if k == "argv" {
                    if let Value::Array(a) = child {
                        let strs: Vec<String> = a
                            .iter()
                            .filter_map(|e| e.as_str().map(|s| s.to_string()))
                            .collect();
                        if strs.len() == a.len() {
                            *child = Value::Array(redact_argv(&strs));
                            continue;
                        }
                    }
                }
                reredact_argv_in(child);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(reredact_argv_in),
        _ => {}
    }
}

/// All-roles-unavailable identity: malformed or absent owner state carries
/// no pid anywhere, so nothing killable can be inferred from it.
pub(crate) fn unavailable_identity(reason: &str) -> Value {
    let now = identity_now();
    json!({
        "debuggee": {"kind": "process", "pid": Value::Null, "confidence": "unavailable",
            "observedAt": now, "unavailable": [{"field": "pid", "reason": reason}]},
        "endpoint": {"confidence": "unavailable", "observedAt": now,
            "unavailable": [{"field": "ownerPid", "reason": reason}]},
        "adapter": {"confidence": "unavailable", "observedAt": now,
            "unavailable": [{"field": "pid", "reason": reason}]},
    })
}

pub(crate) fn identity_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
    fn identity_fields_capped_and_total_bounded() {
        let long = "x".repeat(600);
        let v = cap_identity(json!({
            "debuggee": {"kind": "process", "pid": 1, "executable": long},
            "endpoint": {"argv": [long, long]},
            "adapter": {"cwd": "/t", "source": "s", "observedAt": 1},
        }));
        assert!(v["debuggee"]["executable"].as_str().unwrap().contains("(+"));
        for entry in v["endpoint"]["argv"].as_array().unwrap() {
            assert!(entry.as_str().unwrap().chars().count() <= IDENTITY_FIELD_CAP + 30);
        }
        assert!(v.to_string().chars().count() <= IDENTITY_TOTAL_CAP);
    }

    #[test]
    fn attach_seed_remote_host_is_structured_unavailable() {
        let v = attach_seed("example.com", 5678);
        assert!(v["debuggee"]["pid"].is_null());
        assert_eq!(v["debuggee"]["confidence"], json!("unavailable"));
        assert_eq!(v["endpoint"]["confidence"], json!("unavailable"));
        assert_eq!(v["endpoint"]["host"], json!("example.com"));
        assert_eq!(v["endpoint"]["port"], json!(5678));
        let unavailable = v["endpoint"]["unavailable"].as_array().unwrap();
        assert!(unavailable.iter().any(|u| u["field"] == json!("ownerPid")));
        assert_eq!(v["adapter"]["confidence"], json!("unavailable"));
        // No env, no raw secrets: the whole object serializes clean.
        assert!(v.to_string().len() <= IDENTITY_TOTAL_CAP);
    }

    #[test]
    fn attach_seed_closed_local_port_is_unavailable_not_fabricated() {
        // Nothing listens on port 1: lookup must yield structured
        // unavailable, never an invented pid.
        let v = attach_seed("localhost", 1);
        assert!(v["debuggee"]["pid"].is_null());
        assert_eq!(v["endpoint"]["confidence"], json!("unavailable"));
        assert!(!v["endpoint"]["unavailable"].as_array().unwrap().is_empty());
    }

    #[test]
    fn launch_seed_carries_launcher_args_without_confidence() {
        let v = launch_seed("python3", vec!["python3".to_string(), "a.py".to_string()]);
        assert_eq!(v["debuggee"]["source"], json!("launcher-args"));
        assert_eq!(v["debuggee"]["confidence"], json!("unavailable"));
        assert!(v["debuggee"]["pid"].is_null());
        assert_eq!(v["endpoint"]["confidence"], json!("unavailable"));
        assert_eq!(v["adapter"]["confidence"], json!("unavailable"));
    }

    #[test]
    fn identity_hint_names_target_without_root_cause() {
        let v = launch_seed("python3", vec!["python3".to_string(), "a.py".to_string()]);
        let hint = identity_hint(&v);
        assert!(hint.contains("python3"));
        assert!(!hint.contains("reason"));
        let missing = attach_seed("example.com", 9);
        assert!(identity_hint(&missing).contains("unavailable"));
        assert_eq!(identity_hint(&Value::Null), "");
    }

    #[test]
    fn redact_adjacent_secret_flags_do_not_leak() {
        // The value slot after --token holds another secret flag, not a
        // value: the flag must survive and its own value must redact.
        let argv = ["--token", "--password", "hunter2"];
        let red = redact_argv(&argv.map(|s| s.to_string()));
        let strs: Vec<&str> = red.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(strs, vec!["--token", "--password", "[redacted]"]);
        assert!(!strs.join(" ").contains("hunter2"));
        // Chained three-deep: every value still masks.
        let argv = ["--token", "--pass", "--pw", "zzz"];
        let red = redact_argv(&argv.map(|s| s.to_string()));
        let strs: Vec<&str> = red.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(strs, vec!["--token", "--pass", "--pw", "[redacted]"]);
        assert!(!strs.join(" ").contains("zzz"));
    }

    #[test]
    fn redact_short_pass_forms_without_false_positives() {
        for (flag, val) in [("--pass", "s1"), ("--pw", "s2"), ("--db-pass", "s3")] {
            let argv = [flag, val];
            let red = redact_argv(&argv.map(|s| s.to_string()));
            let strs: Vec<&str> = red.iter().filter_map(|v| v.as_str()).collect();
            assert_eq!(strs, vec![flag, "[redacted]"], "{flag}");
            assert!(!strs.join(" ").contains(val));
        }
        // =/: forms too.
        let red = redact_argv(&["--pass=s1".to_string(), "--pw:s2".to_string()]);
        let strs: Vec<&str> = red.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(strs, vec!["--pass=[redacted]", "--pw:[redacted]"]);
        // Near-misses keep their values: --passage, --author, --passed.
        for argv in [
            ["--passage", "Story"],
            ["--author", "Jane"],
            ["--passed", "yes"],
        ] {
            let red = redact_argv(&argv.map(|s| s.to_string()));
            let strs: Vec<&str> = red.iter().filter_map(|v| v.as_str()).collect();
            assert_eq!(strs, argv, "{argv:?}");
        }
    }

    #[test]
    fn identity_high_count_argv_terminates_bounded() {
        // ~80+ small strings: the old longest-shrink loop re-marked without
        // progress (attach hang); count cap + collapse must converge <=2KB.
        let argv: Vec<Value> = (0..120)
            .map(|i| Value::String(format!("--arg{i:03}-{}", "v".repeat(20))))
            .collect();
        let v = cap_identity(json!({
            "kind": "process", "pid": 1, "executable": "/bin/x",
            "argv": argv, "cwd": "/t",
            "source": "s", "observedAt": 1,
            "unavailable": [], "warnings": [],
        }));
        assert!(v.to_string().chars().count() <= IDENTITY_TOTAL_CAP);
        let arr = v["argv"].as_array().unwrap();
        assert!(arr.len() <= IDENTITY_ARRAY_CAP + 1); // head + tail marker
        assert!(
            arr.iter()
                .any(|e| e.as_str().is_some_and(|s| s.contains("more"))),
            "dropped tail must be marked"
        );
        // Every surviving field still fits the per-field cap (+marker slack).
        for entry in arr {
            assert!(
                entry.as_str().unwrap().chars().count() <= IDENTITY_FIELD_CAP + 30,
                "{}",
                entry.as_str().unwrap().chars().count()
            );
        }
    }

    #[test]
    fn cached_target_identity_needs_bridge_object() {
        // Only a valid layered object echoes; missing/non-object reads as
        // null (no spawn-time fabrication — the roles need bridge facts).
        let dir = tmpdir("identity-cache");
        assert_eq!(cached_target_identity(&dir), Value::Null);
        std::fs::write(
            dir.join("session.json"),
            r#"{"name":"x",
                    "targetIdentity":{"debuggee":{"confidence":"protocol-confirmed"},
                        "endpoint":null,"adapter":null}}"#,
        )
        .unwrap();
        let v = cached_target_identity(&dir);
        assert_eq!(v["debuggee"]["confidence"], json!("protocol-confirmed"));
        std::fs::write(dir.join("session.json"), r#"{"targetIdentity":null}"#).unwrap();
        assert_eq!(cached_target_identity(&dir), Value::Null);
        std::fs::write(dir.join("session.json"), r#"{"targetIdentity":[1]}"#).unwrap();
        assert_eq!(cached_target_identity(&dir), Value::Null);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
