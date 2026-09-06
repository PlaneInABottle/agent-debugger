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
        append_confirmed_breaks(name, &confirmed).map_err(|e| applied_live_err("add", e))?;
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
    if target.is_some() {
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
/// not clear scope and never inflate the bound). A `target` drops only that
/// target's ephemeral target-scoped records (no stops.json change); bare
/// clear is the full line-break reset.
pub fn cmd_breaks_clear(name: &str, target: Option<&str>) -> anyhow::Result<Value> {
    check_name(name)?;
    let dir = checked_session_dir(name)?;
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
    if target.is_some() {
        return Ok(stamp_main(resp));
    }
    let removed = confirmed_removed(&resp);
    if !removed.is_empty() {
        remove_confirmed_breaks(name, &removed).map_err(|e| applied_live_err("clear", e))?;
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

/// A live break mutation applied on the bridge, then intent persistence
/// failed: say so explicitly (bare `breaks` shows live truth; resume intent
/// in stops.json is now stale until resynced) instead of a bare I/O error
/// that reads as "nothing happened".
fn applied_live_err(op: &str, e: anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "breaks {op} applied live but intent persistence failed ({e:#}) — \
         run bare `breaks` to check live stops; resume intent is stale until stops.json is fixed"
    )
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
/// A bridge-reported failure. `message` is the exact bridge error string
/// (timeout prefixes like `timeout: no stop within Ns` are preserved
/// verbatim for compatibility); `wait_context` carries the bridge's
/// additive `waitContext` (wait/capture timeouts only) through the CLI to
/// the JSON/human error envelope. Attach setup failures additionally carry
/// `diagnosis` (`{code, confidence, evidence, recommendation}`), the
/// redacted attempted `target_identity`, and the redacted
/// `requested_target` endpoint — all additive, the message never changes
/// shape for them. Display is the message alone so every existing string
/// match keeps working.
#[derive(Debug)]
pub struct BridgeFailure {
    pub message: String,
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
fn bridge_failure(resp: &Value) -> anyhow::Error {
    let message = resp
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown bridge error")
        .to_string();
    let wait_context = resp.get("waitContext").filter(|v| v.is_object()).cloned();
    BridgeFailure {
        message,
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

/// Normalize a target selector for one session: omitted and explicit `main`
/// both serve main (no wire difference — legacy bridges never see the
/// field); anything else on a main-only adapter (java/browser) fails fast
/// instead of silently serving main. Unknown languages pass through for
/// bridge-side validation — never assume java (a legacy live py/node dir
/// without lang.json must forward, not fail fast as main-only).
/// Legacy narrow helper kept for unit tests; routing uses the `_opt`
/// variant so missing/unknown languages forward instead of assuming java.
#[allow(dead_code)]
fn normalize_target_for_lang(lang: &str, target: Option<&str>) -> anyhow::Result<Option<String>> {
    match target {
        None | Some("main") => Ok(None),
        Some(other) if lang == "java" || lang == "browser" => {
            anyhow::bail!("unsupported target '{other}' (session is {lang}, main-only)")
        }
        Some(other) => Ok(Some(other.to_string())),
    }
}

/// Routing variant over an optional language: `None` (missing/corrupt
/// lang.json) and unrecognized values forward like multi-target langs.
/// Display callers keep `session_lang_in` (java default); every routing
/// call site uses this so unknown never misroutes as main-only.
fn normalize_target_for_lang_opt(
    lang: Option<&str>,
    target: Option<&str>,
) -> anyhow::Result<Option<String>> {
    match (lang, target) {
        (_, None) | (_, Some("main")) => Ok(None),
        (Some("java") | Some("browser"), Some(other)) => {
            anyhow::bail!(
                "unsupported target '{other}' (session is {}, main-only)",
                lang.unwrap()
            )
        }
        (_, Some(other)) => Ok(Some(other.to_string())),
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

/// List debug targets in this session. Multi-target bridges (py/node, plus
/// unknown/missing langs which forward rather than assume main-only) own
/// their roster; main-only adapters (java/browser) get a uniform main-only
/// roster built from the bridge-maintained session.json (no new bridge
/// protocol needed before Batch3). A dead bridge still errors via forward —
/// no fabricated roster — preserving dead-session behavior.
pub fn cmd_targets(name: &str) -> anyhow::Result<Value> {
    check_name(name)?;
    let dir = checked_session_dir(name)?;
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
fn targets_use_local_roster(dir: &std::path::Path) -> bool {
    matches!(
        session_lang_opt(dir).as_deref(),
        Some("java") | Some("browser")
    )
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
        "targetIdentity": parsed.get("targetIdentity").filter(|v| v.is_object()).cloned().unwrap_or(Value::Null),
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
/// Element cap per array inside an observedTarget (argv/cmdline). High-count
/// argv shapes (~80+ small strings) converge via this count cap plus the
/// total loop's guaranteed shrink-or-collapse, never by re-marking forever.
pub const OBSERVED_ARRAY_CAP: usize = 32;

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
fn trunc_chars(s: &str, limit: usize) -> String {
    let n = s.chars().count();
    if n <= limit {
        return s.to_string();
    }
    let kept: String = s.chars().take(limit).collect();
    format!("{kept}… (+{} more chars)", n - limit)
}

/// Cap one observedTarget object: every string field to 512 chars, every
/// array to a bounded element count (tail marker), then the whole object to
/// 2KB serialized. Only the known identity shapes are capped (process/tab);
/// anything else passes through untouched. The total loop always terminates:
/// each pass either shrinks the longest string or, when the trunc marker
/// would not shrink it, collapses it to a 1-char marker (strictly smaller
/// than any picked string), so ~80+ small argv entries converge by dropping
/// rather than re-marking forever.
fn cap_observed(mut v: Value) -> Value {
    fn cap_str(s: &mut String) {
        if s.chars().count() > OBSERVED_FIELD_CAP {
            *s = trunc_chars(s, OBSERVED_FIELD_CAP);
        }
    }
    fn walk(v: &mut Value) {
        match v {
            Value::String(s) => cap_str(s),
            Value::Array(a) => {
                // Deterministic count cap: keep the head, note the dropped
                // tail. Bounds high-count argv/cmdline shapes before the
                // total loop runs.
                if a.len() > OBSERVED_ARRAY_CAP {
                    let dropped = a.len() - OBSERVED_ARRAY_CAP;
                    a.truncate(OBSERVED_ARRAY_CAP);
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
    while v.to_string().chars().count() > OBSERVED_TOTAL_CAP && passes < 4096 {
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
    let local = normalize_attach_host(host) == "loopback";
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

/// Bounded subprocess capture for the macOS lookup (lsof/ps). Same
/// spawn-and-wait-with-timeout idiom as the bridge provisioning path: ~5s
/// hard bound, None on timeout/failure so attach degrades to structured
/// `unavailable` instead of hanging the CLI. No new deps (std mpsc only).
fn run_bounded(cmd: &str, args: &[&str]) -> Option<String> {
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

/// Language owning a session dir (sidecar file; missing = "java" for
/// display/status only — routing must use `session_lang_opt` so unknown
/// forwards to the bridge instead of assuming main-only java).
fn session_lang_in(dir: &std::path::Path) -> String {
    session_lang_opt(dir).unwrap_or_else(|| "java".to_string())
}

/// Routing language: `None` when lang.json is missing, corrupt, or has no
/// `lang` string. Unknown values pass through as `Some` and still forward
/// (only exact "java"/"browser" take the main-only path).
fn session_lang_opt(dir: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(dir.join("lang.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| {
            v.get("lang")
                .and_then(|l| l.as_str())
                .map(|s| s.to_string())
        })
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

/// Sibling lockfile guarding one name's startup window (pre-session.json).
/// Created atomically before the stale-dir clear; held until the wait loop
/// resolves (success or failure) via RAII release. Only our own nonce is
/// ever removed, and only a provably stale lock is stolen — never a live
/// starter's — so stale dirs stay reusable and retries never wedge.
fn startup_lock_path(sessions_root: &std::path::Path, name: &str) -> PathBuf {
    sessions_root.join(format!("{name}.lock"))
}

/// A lock is stale only when its mtime is provably older than the bound.
/// Unreadable clocks fail closed (treat as live) — a retry costs one wait,
/// a wrongful steal costs a live startup its dir.
const STARTUP_LOCK_STALE: Duration = Duration::from_secs(120);

struct StartupGuard {
    path: PathBuf,
    nonce: String,
}

impl Drop for StartupGuard {
    fn drop(&mut self) {
        release_startup_lock(&self.path, &self.nonce);
    }
}

fn startup_nonce() -> String {
    // Padded with a process-wide atomic counter: `SystemTime` nanos can
    // repeat across threads racing in the same instant, and two lock
    // records must never share a nonce (release deletes by nonce match).
    static CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = CTR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}-{n}", std::process::id())
}

fn startup_lock_is_mine(path: &std::path::Path, nonce: &str) -> bool {
    std::fs::read_to_string(path)
        .map(|c| c == nonce)
        .unwrap_or(false)
}

fn release_startup_lock(path: &std::path::Path, nonce: &str) {
    if startup_lock_is_mine(path, nonce) {
        let _ = std::fs::remove_file(path);
    }
}

/// Atomically claim the startup lock (single steal retry for a stale lock).
/// Live locks (fresh or undatable) bail with a retryable "starting" error.
fn acquire_startup_lock(path: &std::path::Path) -> anyhow::Result<StartupGuard> {
    let nonce = startup_nonce();
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut f) => {
            use std::io::Write as _;
            f.write_all(nonce.as_bytes())
                .map_err(|e| anyhow::anyhow!("cannot write startup lock: {e}"))?;
            return Ok(StartupGuard {
                path: path.to_path_buf(),
                nonce,
            });
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => anyhow::bail!("cannot create startup lock: {e}"),
    }
    // Lock held by someone: steal only if provably stale, once.
    let stale = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
        .map(|age| age >= STARTUP_LOCK_STALE)
        .unwrap_or(false);
    if !stale {
        let holder = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        anyhow::bail!(
            "session '{holder}' is starting (concurrent start in progress; retry shortly)"
        );
    }
    std::fs::remove_file(path)
        .map_err(|e| anyhow::anyhow!("cannot clear stale startup lock: {e}"))?;
    let nonce2 = startup_nonce();
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| {
            anyhow::anyhow!("session is starting (concurrent start in progress; retry shortly)")
        })
        .and_then(|mut f| {
            use std::io::Write as _;
            f.write_all(nonce2.as_bytes())
                .map_err(|e| anyhow::anyhow!("cannot write startup lock: {e}"))?;
            Ok(StartupGuard {
                path: path.to_path_buf(),
                nonce: nonce2,
            })
        })
}

/// Spawn the bridge daemon and wait for the first stop.
pub fn spawn(name: &str, spec: &SpawnSpec) -> anyhow::Result<Value> {
    spawn_in(&sessions_dir(), name, spec)
}

// ---- attach endpoint collision prevention ----

/// Loopback aliases share one endpoint identity: `localhost` (case- and
/// trailing-dot-insensitive: `LOCALHOST.` counts), the whole 127/8 range,
/// and `::1` in any textual form (compressed, expanded, bracketed, or
/// IPv4-mapped like `::ffff:127.0.0.1`). Anything else compares
/// exact/lowercase (IPv6 brackets stripped). The unspecified addresses
/// `0.0.0.0`/`::` are NOT loopback — they are rejected as attach
/// destinations (see `is_unspecified_host`), never silently merged.
pub fn normalize_attach_host(host: &str) -> String {
    let h = host.trim();
    let inner = if h.starts_with('[') && h.ends_with(']') && h.len() > 2 {
        &h[1..h.len() - 1]
    } else {
        h
    };
    let lower = inner.to_ascii_lowercase();
    let fqdn = lower.strip_suffix('.').unwrap_or(&lower);
    if fqdn == "localhost" || ip_is_loopback(fqdn) {
        return "loopback".to_string();
    }
    fqdn.to_string()
}

/// True for IPv4 loopback (all of 127/8), IPv6 `::1`, and IPv4-mapped or
/// IPv4-compatible forms wrapping a loopback v4 (`::ffff:127.0.0.1`).
/// Anything unparseable is not loopback — never a guess.
fn ip_is_loopback(s: &str) -> bool {
    let ip: std::net::IpAddr = match s.parse() {
        Ok(ip) => ip,
        Err(_) => return false,
    };
    if ip.is_loopback() {
        return true;
    }
    match ip {
        std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
        std::net::IpAddr::V4(_) => false, // non-loopback v4, already checked
    }
}

/// Unspecified/wildcard destinations can never be attach targets: dialing
/// `0.0.0.0` is platform-dependent (Linux loops back, elsewhere it fails),
/// so it fails fast with a clear error pointing at an explicit address.
fn is_unspecified_host(host: &str) -> bool {
    let h = host.trim();
    let inner = if h.starts_with('[') && h.ends_with(']') && h.len() > 2 {
        &h[1..h.len() - 1]
    } else {
        h
    };
    matches!(
        inner.to_ascii_lowercase().as_str(),
        "0.0.0.0" | "::" | "0:0:0:0:0:0:0:0"
    )
}

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
fn attach_endpoint(spec: &SpawnSpec) -> anyhow::Result<Option<(String, u16)>> {
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
fn listener_present(host: &str, port: u16) -> Option<bool> {
    if normalize_attach_host(host) != "loopback" {
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
fn daemon_alive_in(dir: &std::path::Path) -> bool {
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
    let updated_ago = parsed
        .as_ref()
        .and_then(|v| v.get("updatedAt").and_then(|u| u.as_u64()))
        .and_then(|s| {
            std::time::UNIX_EPOCH
                .checked_add(Duration::from_secs(s))
                .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
        });
    matches!(updated_ago, Some(age) if age < RECENT_PUBLISH)
}

/// Attach endpoint from a persisted intent: `requestedTarget` first (exact
/// CLI request), `target` summary as fallback (string ports there).
/// Anything unparseable reads as absent — never a block.
fn intent_endpoint(stops: &Value) -> Option<(String, u16)> {
    for key in ["requestedTarget", "target"] {
        let v = stops.get(key)?;
        let host = v.get("host")?.as_str()?;
        let port = v
            .get("port")
            .and_then(|p| p.as_u64().or_else(|| p.as_str()?.parse::<u64>().ok()))?;
        if let Ok(port) = u16::try_from(port) {
            return Some((host.to_string(), port));
        }
    }
    None
}

/// A confirmed live owner of an attach endpoint: a session dir (not a
/// symlink, valid name) whose intent names the same normalized endpoint,
/// whose lang is exclusive, whose kind is attach/launch, and whose daemon
/// answers. Stale/dead dirs never block; nothing is deleted as a side
/// effect — this is a read-only scan.
fn find_live_endpoint_owner_in(
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
        let stops: Value = std::fs::read_to_string(dir.join("stops.json"))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())?;
        let (eh, ep) = intent_endpoint(&stops)?;
        if ep != port || normalize_attach_host(&eh) != norm {
            continue;
        }
        // Capability-aware: only exclusive owners collide. A browser
        // session on the same CDP port (different tab) never blocks.
        if !attach_exclusive(session_lang_opt(&dir).as_deref().unwrap_or("")) {
            continue;
        }
        let sess: Value = std::fs::read_to_string(dir.join("session.json"))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())?;
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

// ---- endpoint-scoped atomic reservation ----

/// Locks live under `~/.agent-debugger/endpoint-locks/`, one file per
/// normalized language/host/port, so two different endpoints never block
/// each other. The lock is held from preflight through bridge handshake
/// until session ownership is published; afterwards the published session
/// (found by the scan above) owns the endpoint.
fn endpoint_locks_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".agent-debugger")
        .join("endpoint-locks")
}

/// Locks dir scoped to a sessions root (unit tests pass a tmpdir so the
/// reservation is exercised without touching the real namespace).
/// Production roots (`~/.agent-debugger/sessions`) map to the canonical
/// `~/.agent-debugger/endpoint-locks`.
fn endpoint_locks_dir_for(sessions_root: &std::path::Path) -> PathBuf {
    sessions_root
        .parent()
        .map(|p| p.join("endpoint-locks"))
        .unwrap_or_else(endpoint_locks_dir)
}

fn endpoint_lock_path(
    locks_dir: &std::path::Path,
    lang: &str,
    norm_host: &str,
    port: u16,
) -> PathBuf {
    let safe: String = norm_host
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    locks_dir.join(format!("{lang}-{safe}-{port}.lock"))
}

/// A lock is provably stale only by owner liveness/age: a dead holder pid
/// (past a short grace for just-created locks) or an age past the bound
/// (backstop for pid reuse / wedged holders). Unreadable clocks fail
/// closed — a retry costs one wait, a wrongful steal costs a live attach.
const ENDPOINT_LOCK_STALE: Duration = Duration::from_secs(7200);
const ENDPOINT_LOCK_GRACE: Duration = Duration::from_secs(10);

#[derive(Debug)]
struct EndpointGuard {
    path: PathBuf,
    nonce: String,
}

impl Drop for EndpointGuard {
    fn drop(&mut self) {
        release_endpoint_lock(&self.path, &self.nonce);
    }
}

/// Only our own nonce is ever removed (parsed out of the JSON record —
/// never a blind delete), so a live starter's lock is never stolen on
/// release.
fn release_endpoint_lock(path: &std::path::Path, nonce: &str) {
    let mine = std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v.get("nonce").and_then(|n| n.as_str()).map(|n| n == nonce))
        .unwrap_or(false);
    if mine {
        let _ = std::fs::remove_file(path);
    }
}

/// Another starter holds (or just held) this endpoint.
#[derive(Debug)]
struct EndpointBusy {
    session: String,
    published: bool,
}

fn endpoint_lock_session_name(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| {
            v.get("session")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
        })
}

/// Three-state holder liveness: only a provably-dead holder makes a lock
/// stealable. `Unknown` (probe itself failed) fails closed — treated as
/// live — so a wedged `ps` can never authorize deleting a live starter's
/// lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    Alive,
    Dead,
    Unknown,
}

/// Best-effort holder liveness. Linux reads /proc directly (no
/// subprocess); elsewhere a bounded `ps` probe answers.
#[cfg(target_os = "linux")]
fn holder_alive(pid: u32) -> Liveness {
    let p = std::path::PathBuf::from(format!("/proc/{pid}"));
    match std::fs::symlink_metadata(&p) {
        Ok(m) if m.file_type().is_dir() => Liveness::Alive,
        Ok(_) => Liveness::Unknown, // exists but unreadable shape: no verdict
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Liveness::Dead,
        Err(_) => Liveness::Unknown,
    }
}

#[cfg(not(target_os = "linux"))]
fn holder_alive(pid: u32) -> Liveness {
    ps_has_pid(pid)
}

/// Dedicated bounded `ps` status probe (non-Linux only): the exit status —
/// not mere output presence — is the signal. `ps -p <dead>` exits nonzero
/// (dead); spawn failure or timeout means the probe itself failed
/// (unknown), never "dead". No shell, fixed argv, 5s hard bound.
#[cfg(not(target_os = "linux"))]
fn ps_has_pid(pid: u32) -> Liveness {
    const PS_TIMEOUT: Duration = Duration::from_secs(5);
    let want = pid.to_string();
    let child = match std::process::Command::new("ps")
        .args(["-p", &want, "-o", "pid="])
        .stdin(Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Liveness::Unknown, // ps missing/unforkable: no verdict
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(PS_TIMEOUT) {
        Ok(Ok(out)) if out.status.success() => {
            // Success lists the pid when alive (headerless `-o pid=` row);
            // a bare success without the row still reads as dead — ps
            // exited 0 only when the selection matched... defensively, an
            // empty match is dead, never alive.
            if String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|l| l.trim() == want)
            {
                Liveness::Alive
            } else {
                Liveness::Dead
            }
        }
        Ok(Ok(_)) => Liveness::Dead,     // nonzero exit: no such process
        Ok(Err(_)) => Liveness::Unknown, // wait/reap failure: no verdict
        Err(_) => Liveness::Unknown,     // timeout: probe failed, not the holder
    }
}

/// Snapshot decision: is this exact lock record stale as of `mtime`?
/// Pure over bytes + timestamp (no IO) so tests exercise the policy
/// without clocks or sleeps. A future mtime fails closed (not stale).
fn lock_snapshot_is_stale(raw: &str, mtime: std::time::SystemTime) -> bool {
    let age = match std::time::SystemTime::now().duration_since(mtime) {
        Ok(a) => a,
        Err(_) => return false,
    };
    if age >= ENDPOINT_LOCK_STALE {
        return true;
    }
    if age < ENDPOINT_LOCK_GRACE {
        return false;
    }
    let pid = serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| v.get("pid").and_then(|p| p.as_u64()))
        .and_then(|p| u32::try_from(p).ok());
    matches!(pid.map(holder_alive), Some(Liveness::Dead))
}

/// Remove a stale endpoint lock without ever deleting a fresh claimant's.
/// Protocol: snapshot the record, re-verify the exact bytes, then detach
/// via atomic `rename` into a unique quarantine file (never `remove_file`
/// on the live path — a verify→unlink race could otherwise delete a fresh
/// lock a rival just claimed). Only a quarantined record that still equals
/// the verified-stale bytes is dropped; anything else is restored or left
/// for its owner. Concurrent reclaimers of the same stale bytes all
/// succeed at detaching (exactly one wins the rename; the rest see
/// NotFound and proceed); the subsequent atomic `create_new` claim still
/// admits exactly one holder. Returns true when no stale record remains.
fn reclaim_stale_lock(path: &std::path::Path) -> bool {
    // Snapshot record + metadata together.
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
        Err(_) => return false,
    };
    let mtime = match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return false,
    };
    if !lock_snapshot_is_stale(&raw, mtime) {
        return false;
    }
    quarantine_verified(path, &raw)
}

/// Detach-and-verify (test seam for interleavings): re-read the exact
/// bytes, atomically quarantine, and drop only a still-stale match.
/// Returns false (hands off, fresh record intact) on any deviation.
fn quarantine_verified(path: &std::path::Path, expected: &str) -> bool {
    let raw2 = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
        Err(_) => return false,
    };
    if raw2 != expected {
        return false; // replaced under us: fresh claim or fellow reclaim
    }
    let q = match detach_to_quarantine(path) {
        Ok(q) => q,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
        Err(_) => return false,
    };
    reconcile_quarantine(path, &q, expected)
}

/// Atomically detach the lock file into a unique quarantine sibling (same
/// dir = same filesystem). Exactly one concurrent detacher wins; the rest
/// see NotFound.
fn detach_to_quarantine(path: &std::path::Path) -> std::io::Result<PathBuf> {
    // Hidden `.q-` prefix marks it as temp (never scanned as a lock).
    let q = path.with_file_name(format!(".q-{}-{}.tmp", std::process::id(), startup_nonce()));
    std::fs::rename(path, &q).map(|()| q)
}

/// Reconcile a detached quarantine file against the verified-stale bytes.
/// Match → drop it (stale detached, exactly as judged; its age only grew
/// since the verdict, so no re-probe). Mismatch → the path changed under
/// us (a fresh claim F): if the path is still free, restore F's bytes via
/// a non-overwriting atomic claim; if a newer claim (F2) landed meanwhile,
/// drop our copy — F's owner backs off at its pre-spawn nonce re-verify
/// while F2's owner proceeds. Either way exactly one starter proceeds, no
/// fresh record is ever deleted or overwritten, and the quarantine file
/// never lingers (deleted or consumed on every path short of a crash).
/// Returns true only for a dropped stale match.
fn reconcile_quarantine(path: &std::path::Path, q: &std::path::Path, expected: &str) -> bool {
    let qb = std::fs::read_to_string(q).unwrap_or_default();
    if qb == expected {
        let _ = std::fs::remove_file(q);
        return true;
    }
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Path looks free: restore by copy, never by move — the
            // create_new claim below refuses if an F2 landed in the
            // check→restore window, so overwrite is impossible.
            let _ = restore_quarantine_bytes(path, q);
            let _ = std::fs::remove_file(q); // consumed either way
        }
        _ => {
            let _ = std::fs::remove_file(q);
        }
    }
    false
}

/// Restore quarantined bytes to an absent path without ever overwriting:
/// exclusive `create_new` wins the slot atomically — a rival F2 that lands
/// first (or a planted symlink, which create_new refuses to follow into)
/// makes us fail closed with the rival's record untouched. If creation
/// succeeds but the write fails, only the file we just created is removed
/// (nothing else's — create_new proves no other record was there) and we
/// fail closed. Best-effort `sync_all` so a restored record is durable.
fn restore_quarantine_bytes(path: &std::path::Path, q: &std::path::Path) -> bool {
    let bytes = match std::fs::read(q) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let mut f = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(f) => f,
        Err(_) => return false,
    };
    use std::io::Write as _;
    if f.write_all(&bytes).is_err() {
        let _ = std::fs::remove_file(path); // ours alone: just created it
        return false;
    }
    let _ = f.sync_all();
    true
}

/// Pre-spawn ownership check: our endpoint reservation must still hold our
/// nonce immediately before the bridge spawns, or we back off. Combined
/// with quarantine-restore this admits exactly one bridge per endpoint
/// even when a rival detached around us: the rival either restores our
/// record (we proceed, it backs off) or owns the path (we back off).
/// A momentarily-absent path (mid-restore flap) retries briefly; a
/// different record fails at once.
fn still_holds_endpoint(guard: &EndpointGuard) -> bool {
    for _ in 0..10 {
        match std::fs::read_to_string(&guard.path) {
            Ok(raw) => {
                return serde_json::from_str::<Value>(&raw)
                    .ok()
                    .and_then(|v| {
                        v.get("nonce")
                            .and_then(|n| n.as_str())
                            .map(|n| n == guard.nonce)
                    })
                    .unwrap_or(false);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return false,
        }
    }
    false
}

/// How an endpoint-claim attempt resolves. IO failures surface as
/// `anyhow::Error` with their actual message — never disguised as a
/// collision with our own session.
#[derive(Debug)]
enum EndpointClaim {
    Held(EndpointGuard),
    Busy(EndpointBusy),
}

/// Claim the endpoint lock (single reclaim retry for a stale lock). A live
/// lock reports the holder — the caller turns it into an
/// endpoint-already-attached error, never a wait.
fn acquire_endpoint_lock(
    locks_dir: &std::path::Path,
    sessions_root: &std::path::Path,
    name: &str,
    lang: &str,
    norm_host: &str,
    port: u16,
) -> anyhow::Result<EndpointClaim> {
    std::fs::create_dir_all(locks_dir).map_err(|e| {
        anyhow::anyhow!(
            "cannot create endpoint locks dir {}: {e}",
            locks_dir.display()
        )
    })?;
    let path = endpoint_lock_path(locks_dir, lang, norm_host, port);
    let claim = || -> anyhow::Result<EndpointGuard> {
        let nonce = startup_nonce();
        let content = serde_json::json!({
            "nonce": nonce,
            "pid": std::process::id(),
            "session": name,
            "createdAt": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        })
        .to_string();
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        use std::io::Write as _;
        f.write_all(content.as_bytes())
            .map_err(|e| anyhow::anyhow!("cannot write endpoint lock: {e}"))?;
        Ok(EndpointGuard {
            path: path.clone(),
            nonce,
        })
    };
    match claim() {
        Ok(g) => return Ok(EndpointClaim::Held(g)),
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .map(|io| io.kind() == std::io::ErrorKind::AlreadyExists)
                .unwrap_or(false) => {}
        Err(e) => return Err(e),
    }
    if reclaim_stale_lock(&path) {
        match claim() {
            Ok(g) => return Ok(EndpointClaim::Held(g)),
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .map(|io| io.kind() == std::io::ErrorKind::AlreadyExists)
                    .unwrap_or(false) => {}
            Err(e) => return Err(e),
        }
    }
    let session =
        endpoint_lock_session_name(&path).unwrap_or_else(|| "another session".to_string());
    // A published session.json under the holder's name means the handshake
    // finished (the scan will confirm ownership); otherwise another attach
    // is still in flight.
    let published = sessions_root.join(&session).join("session.json").exists();
    Ok(EndpointClaim::Busy(EndpointBusy { session, published }))
}

// ---- attach failure classification ----

/// Classify an attach setup failure from OS-observed listener state taken
/// immediately pre-attach (`pre`) and immediately after the failure
/// (`post`). `None` is unknown (remote hosts have no local source).
/// Confidence is calibrated, never certain about foreign clients: a
/// present-but-rejecting listener *may* already have another debugger
/// client — it is not claimed as fact.
fn attach_diagnosis(pre: Option<bool>, post: Option<bool>, endpoint: &str) -> Value {
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
fn display_endpoint(host: &str, port: u16) -> String {
    const HOST_CAP: usize = 128;
    let h = if host.chars().count() > HOST_CAP {
        trunc_chars(host, HOST_CAP)
    } else {
        host.to_string()
    };
    format!("{h}:{port}")
}

/// One classification site for every attach setup failure (fast error.json,
/// late-settling error.json, bridge exit, first-forward transport): probe
/// the OS listener now, classify pre-vs-post, keep the bridge/transport
/// message verbatim, and attach the redacted identities. The debuggee is
/// never fabricated — on a failed attach the attempted identity keeps its
/// `unavailable` entries as the bridge/lookup reported them.
fn attach_setup_failure(
    host: &str,
    port: u16,
    pre: Option<bool>,
    message: String,
    spec: &SpawnSpec,
) -> anyhow::Error {
    let post = listener_present(host, port);
    let diagnosis = attach_diagnosis(pre, post, &display_endpoint(host, port));
    attach_failure(message, diagnosis, &spec.observed, &spec.requested)
}

/// Build the typed attach-setup failure: the bridge message stays verbatim
/// (existing `attach failed` matches keep working); the diagnosis,
/// redacted attempted identity, and redacted requested endpoint ride
/// alongside into the outer envelope.
fn attach_failure(
    message: String,
    diagnosis: Value,
    observed: &Value,
    requested: &Value,
) -> anyhow::Error {
    BridgeFailure {
        message,
        wait_context: None,
        diagnosis: Some(diagnosis),
        target_identity: Some(observed.clone()),
        requested_target: Some(requested.clone()),
    }
    .into()
}

/// Preflight rejection when a live session already owns the endpoint.
/// Names the confirmed owner and the redacted endpoint; tells the agent to
/// reuse or close it. No bridge is ever spawned, so the first session's
/// target is untouched.
fn endpoint_owned_failure(
    owner: &str,
    in_progress: bool,
    lang: &str,
    host: &str,
    port: u16,
    observed: &Value,
    requested: &Value,
) -> anyhow::Error {
    let endpoint = display_endpoint(host, port);
    let message = if in_progress {
        format!(
            "endpoint-already-attached: session '{owner}' is attaching to {lang} \
             {endpoint} — use the existing session or wait and retry"
        )
    } else {
        format!(
            "endpoint-already-attached: session '{owner}' already owns {lang} \
             {endpoint} — use the existing session or close it first"
        )
    };
    let diagnosis = serde_json::json!({
        "code": "endpoint-already-attached",
        "confidence": "high",
        "evidence": {
            "endpoint": endpoint,
            "ownerSession": owner,
            "inProgress": in_progress,
        },
        "recommendation": "use the existing session for this endpoint, or close it first and retry",
    });
    attach_failure(message, diagnosis, observed, requested)
}

/// `spawn` with an explicit sessions root (unit tests pass a tmpdir so the
/// interlock is exercised without touching the real sessions dir).
fn spawn_in(
    sessions_root: &std::path::Path,
    name: &str,
    spec: &SpawnSpec,
) -> anyhow::Result<Value> {
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(spec.wait_secs))
        .ok_or_else(|| anyhow::anyhow!("timeout is too large"))?;
    check_name(name)?;
    let dir = sessions_root.join(name);
    check_dir_real(&dir)?;
    // Exclusive startup interlock BEFORE any stale cleanup: two concurrent
    // starts on one name used to race pre-session.json, the loser deleting
    // the winner's dir via the stale clear below. The lock is a sibling
    // file created atomically (create_new); only a provably stale lock
    // (mtime older than the bound) is stolen, so crashed starters never
    // block the name forever and retries keep working.
    std::fs::create_dir_all(sessions_root)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", sessions_root.display()))?;
    let _guard = acquire_startup_lock(&startup_lock_path(sessions_root, name))?;
    if dir.join("session.json").exists() {
        anyhow::bail!("session '{name}' already exists (close it first)");
    }
    // Attach collision preflight (exclusive endpoints only: launch has no
    // pre-known endpoint and browser multiplexes tabs, so neither takes
    // this path). The endpoint lock is claimed BEFORE the session scan so
    // two concurrent second attaches cannot both pass it (preflight alone
    // would be TOCTOU); it stays held through the handshake below until
    // session ownership publishes. The scan under the lock then catches
    // already-published owners. Nothing here deletes or touches other
    // sessions — preflight is read-only plus our own lock file.
    let endpoint = attach_endpoint(spec)?;
    let pre_listener = endpoint.as_ref().map(|(h, p)| listener_present(h, *p));
    let _endpoint_guard = match &endpoint {
        Some((host, port)) => {
            let norm = normalize_attach_host(host);
            let locks = endpoint_locks_dir_for(sessions_root);
            match acquire_endpoint_lock(&locks, sessions_root, name, spec.lang, &norm, *port)? {
                EndpointClaim::Held(g) => {
                    if let Some(owner) =
                        find_live_endpoint_owner_in(sessions_root, name, host, *port)
                    {
                        drop(g);
                        return Err(endpoint_owned_failure(
                            &owner,
                            false,
                            spec.lang,
                            host,
                            *port,
                            &spec.observed,
                            &spec.requested,
                        ));
                    }
                    // Final ownership gate before any bridge spawns: a
                    // rival that verified a stale record earlier may have
                    // detached around us (it restores what isn't stale and
                    // backs off, but only our nonce match proves we won).
                    if !still_holds_endpoint(&g) {
                        let cur = endpoint_lock_session_name(&g.path)
                            .unwrap_or_else(|| "another session".to_string());
                        let published = sessions_root.join(&cur).join("session.json").exists();
                        drop(g);
                        return Err(endpoint_owned_failure(
                            &cur,
                            !published,
                            spec.lang,
                            host,
                            *port,
                            &spec.observed,
                            &spec.requested,
                        ));
                    }
                    Some(g)
                }
                EndpointClaim::Busy(busy) => {
                    return Err(endpoint_owned_failure(
                        &busy.session,
                        !busy.published,
                        spec.lang,
                        host,
                        *port,
                        &spec.observed,
                        &spec.requested,
                    ));
                }
            }
        }
        None => None,
    };
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
            // Attach setup failure: classify from OS-observed listener
            // state (pre-attach vs now). The bridge message stays verbatim;
            // the diagnosis + redacted identities ride alongside.
            if let Some((host, port)) = &endpoint {
                let pre = pre_listener.flatten();
                return Err(attach_setup_failure(host, *port, pre, msg, spec));
            }
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
                    // as null. The layered targetIdentity (debuggee /
                    // endpoint / adapter roles) rides along the same way —
                    // no spawn-time fallback, the roles need bridge facts.
                    data["requestedTarget"] = spec.requested.clone();
                    data["observedTarget"] = cached_observed(&dir, spec);
                    data["targetIdentity"] = cached_target_identity(&dir);
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
                        // Late-settling attach failure: same envelope as the
                        // fast path (message verbatim + fresh post probe).
                        if let Some((host, port)) = &endpoint {
                            let pre = pre_listener.flatten();
                            return Err(attach_setup_failure(host, *port, pre, msg, spec));
                        }
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
                            out["targetIdentity"] = cached_target_identity(&dir);
                            return Ok(out);
                        }
                    }
                    // No session was established and nothing is collectible:
                    // take the daemon down and remove the dir, so the name
                    // is reusable and no orphan lingers. For attach, the
                    // transport message stays verbatim inside the same
                    // classified envelope (fresh post probe + identities).
                    reap(&mut child);
                    let _ = std::fs::remove_dir_all(&dir);
                    if let Some((host, port)) = &endpoint {
                        let pre = pre_listener.flatten();
                        return Err(attach_setup_failure(
                            host,
                            *port,
                            pre,
                            format!("{e:#}"),
                            spec,
                        ));
                    }
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
            // An attach bridge that dies without an error file still gets
            // listener evidence (same classification, same verbatim
            // message policy).
            if let Some((host, port)) = &endpoint {
                let pre = pre_listener.flatten();
                return Err(attach_setup_failure(
                    host,
                    *port,
                    pre,
                    format!("bridge exited during setup (code {status}). {log_tail}"),
                    spec,
                ));
            }
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

/// Layered target identity for a live session dir (session.json
/// `targetIdentity`: `{debuggee, endpoint, adapter}` roles built by the
/// bridge from protocol-confirmed + OS-corroborated sources). No spawn-time
/// fallback: the roles need bridge-observed protocol facts the CLI never
/// has, so a missing copy reads as null (honest "unknown"), never a
/// fabricated role. `observedTarget` stays the compatibility view.
fn cached_target_identity(dir: &std::path::Path) -> Value {
    std::fs::read_to_string(dir.join("session.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v.get("targetIdentity").cloned())
        .filter(|v| v.is_object())
        .unwrap_or(Value::Null)
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
    let identity = std::fs::read_to_string(dir.join("session.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v.get("targetIdentity").cloned())
        .filter(|v| v.is_object())
        .unwrap_or(Value::Null);
    if let Value::Object(ref mut m) = resp {
        m.insert("requestedTarget".to_string(), requested);
        m.insert("observedTarget".to_string(), observed);
        m.insert("targetIdentity".to_string(), identity);
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
    let identity = parsed
        .get("targetIdentity")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or(Value::Null);
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
        "targetIdentity": identity,
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
    fn observed_high_count_argv_terminates_bounded() {
        // ~80+ small strings: the old longest-shrink loop re-marked without
        // progress (attach hang); count cap + collapse must converge <=2KB.
        let argv: Vec<Value> = (0..120)
            .map(|i| Value::String(format!("--arg{i:03}-{}", "v".repeat(20))))
            .collect();
        let v = cap_observed(json!({
            "kind": "process", "pid": 1, "executable": "/bin/x",
            "argv": argv, "cwd": "/t",
            "source": "s", "observedAt": 1,
            "unavailable": [], "warnings": [],
        }));
        assert!(v.to_string().chars().count() <= OBSERVED_TOTAL_CAP);
        let arr = v["argv"].as_array().unwrap();
        assert!(arr.len() <= OBSERVED_ARRAY_CAP + 1); // head + tail marker
        assert!(
            arr.iter()
                .any(|e| e.as_str().is_some_and(|s| s.contains("more"))),
            "dropped tail must be marked"
        );
        // Every surviving field still fits the per-field cap (+marker slack).
        for entry in arr {
            assert!(
                entry.as_str().unwrap().chars().count() <= OBSERVED_FIELD_CAP + 30,
                "{}",
                entry.as_str().unwrap().chars().count()
            );
        }
    }

    #[test]
    fn unknown_lang_forwards_instead_of_assuming_java() {
        // Missing/corrupt lang.json: display stays java, routing forwards.
        let dir = tmpdir("lang-missing");
        assert_eq!(session_lang_in(&dir), "java");
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
    fn startup_lock_exclusive_live_and_stale() {
        let base = tmpdir("startup-lock");
        let lock = startup_lock_path(&base, "demo");
        // First claim wins; second sees a live lock and bails retryably.
        let g = acquire_startup_lock(&lock).unwrap();
        assert!(acquire_startup_lock(&lock).is_err());
        // Foreign nonce is never removed by release.
        release_startup_lock(&lock, "not-mine");
        assert!(lock.exists());
        // Drop releases ours; the name is reusable (retries keep working).
        drop(g);
        assert!(!lock.exists());
        let g2 = acquire_startup_lock(&lock).unwrap();
        drop(g2);
        // Provably stale lock (old mtime) is stolen exactly once; a fresh
        // lock after the steal blocks again.
        std::fs::write(&lock, "crashed-starter").unwrap();
        let old = std::time::SystemTime::now() - STARTUP_LOCK_STALE - Duration::from_secs(5);
        let f = std::fs::File::options().write(true).open(&lock).unwrap();
        f.set_modified(old).unwrap();
        drop(f);
        let g3 = acquire_startup_lock(&lock).unwrap();
        assert!(startup_lock_is_mine(&lock, &g3.nonce));
        assert!(acquire_startup_lock(&lock).is_err());
        drop(g3);
        assert!(!lock.exists());
        let _ = std::fs::remove_dir_all(&base);
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

    #[test]
    fn cached_target_identity_needs_bridge_object() {
        // Present object passes through; missing/non-object reads as null
        // (no spawn-time fabrication — the roles need bridge facts).
        let dir = tmpdir("identity-cache");
        assert_eq!(cached_target_identity(&dir), Value::Null);
        std::fs::write(
            dir.join("session.json"),
            r#"{"name":"x","observedTarget":{"kind":"process"},
                "targetIdentity":{"debuggee":{"confidence":"protocol-confirmed"}}}"#,
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

    #[test]
    fn session_entry_and_roster_surface_identity_additively() {
        // observedTarget shape untouched; targetIdentity rides alongside in
        // both status rows and the main-only targets roster.
        let dir = tmpdir("identity-surface");
        std::fs::write(
            dir.join("session.json"),
            r#"{"name":"x","kind":"attach","port":0,"stopped":false,
                "observedTarget":{"kind":"process","pid":7},
                "targetIdentity":{"debuggee":{"pid":7},"endpoint":null,"adapter":null}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("lang.json"), r#"{"lang":"java"}"#).unwrap();
        let row = session_entry(&dir);
        assert_eq!(row["observedTarget"]["pid"], json!(7));
        assert_eq!(row["targetIdentity"]["debuggee"]["pid"], json!(7));
        let roster = cmd_targets_in(&dir);
        assert_eq!(roster["targetIdentity"]["debuggee"]["pid"], json!(7));
        assert_eq!(roster["targets"][0]["observed"]["pid"], json!(7));
        // Legacy file without the key: honest nulls, same as before.
        std::fs::write(
            dir.join("session.json"),
            r#"{"name":"x","kind":"attach","port":0,"stopped":false}"#,
        )
        .unwrap();
        assert_eq!(session_entry(&dir)["targetIdentity"], Value::Null);
        assert_eq!(cmd_targets_in(&dir)["targetIdentity"], Value::Null);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- attach endpoint collision prevention ----

    #[test]
    fn attach_host_normalization_collapses_loopback() {
        for alias in [
            "localhost",
            "LOCALHOST",
            "localhost.", // FQDN root form
            "LOCALHOST. ",
            "127.0.0.1",
            "127.0.0.5", // whole 127/8
            "127.255.200.1",
            "::1",
            "[::1]",
            "0:0:0:0:0:0:0:1",  // expanded
            "::ffff:127.0.0.1", // v4-mapped
            "[::ffff:127.0.0.1]",
            " localhost ",
        ] {
            assert_eq!(normalize_attach_host(alias), "loopback", "{alias}");
        }
        // Non-loopback compares exact/lowercase, brackets stripped.
        assert_eq!(normalize_attach_host("Example.COM"), "example.com");
        assert_eq!(normalize_attach_host("[2001:db8::1]"), "2001:db8::1");
        assert_eq!(normalize_attach_host("2001:db8::1"), "2001:db8::1");
        assert_ne!(normalize_attach_host("example.com"), "loopback");
        // Unspecified is not loopback (rejected as a destination, never
        // silently merged into the loopback identity).
        assert_ne!(normalize_attach_host("0.0.0.0"), "loopback");
        assert_ne!(normalize_attach_host("::"), "loopback");
        assert!(is_unspecified_host("0.0.0.0"));
        assert!(is_unspecified_host("::"));
        assert!(is_unspecified_host("[::]"));
        assert!(!is_unspecified_host("localhost"));
        assert!(!is_unspecified_host("127.0.0.1"));
        assert!(!is_unspecified_host("example.com"));
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
            observed: Value::Null,
            observed_hint: String::new(),
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
    fn endpoint_lock_is_exclusive_per_endpoint_with_stale_recovery() {
        let base = tmpdir("endpoint-lock");
        let locks = base.join("locks");
        let sessions = base.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let held = |r: anyhow::Result<EndpointClaim>| match r.unwrap() {
            EndpointClaim::Held(g) => g,
            EndpointClaim::Busy(b) => panic!("expected hold, got busy: {b:?}"),
        };
        let busy = |r: anyhow::Result<EndpointClaim>| match r.unwrap() {
            EndpointClaim::Busy(b) => b,
            EndpointClaim::Held(_) => panic!("expected busy, got hold"),
        };
        // First claim wins; second sees the live holder.
        let g = held(acquire_endpoint_lock(
            &locks, &sessions, "first", "py", "loopback", 5678,
        ));
        let b = busy(acquire_endpoint_lock(
            &locks, &sessions, "second", "py", "loopback", 5678,
        ));
        assert_eq!(b.session, "first");
        assert!(!b.published, "no session.json yet: in flight");
        // A different endpoint never blocks.
        let g2 = held(acquire_endpoint_lock(
            &locks, &sessions, "second", "py", "loopback", 5679,
        ));
        // A different lang on the same port is a different reservation key
        // (scan-level capability decides collisions, not the lock).
        let g3 = held(acquire_endpoint_lock(
            &locks, &sessions, "b", "browser", "loopback", 5678,
        ));
        // Release frees the endpoint (guard drops only our own nonce).
        drop(g);
        let g4 = held(acquire_endpoint_lock(
            &locks, &sessions, "second", "py", "loopback", 5678,
        ));
        drop(g2);
        drop(g3);
        drop(g4);
        // Stale lock (dead pid, old mtime) is reclaimed with bounded wait.
        let stale_holder = held(acquire_endpoint_lock(
            &locks, &sessions, "crashed", "py", "loopback", 1,
        ));
        let live_pid = std::process::id();
        drop(stale_holder);
        // Rewrite the record with a provably-dead pid and old mtime.
        let path = endpoint_lock_path(&locks, "py", "loopback", 1);
        std::fs::write(
            &path,
            json!({"nonce": "dead", "pid": 4199999u32, "session": "crashed", "createdAt": 1})
                .to_string(),
        )
        .unwrap();
        backdate(&path, ENDPOINT_LOCK_STALE + Duration::from_secs(5));
        assert!(reclaim_stale_lock(&path), "dead pid + old age must reclaim");
        assert!(!path.exists(), "reclaimed record must be gone");
        let g5 = held(acquire_endpoint_lock(
            &locks, &sessions, "retry", "py", "loopback", 1,
        ));
        drop(g5);
        // Fresh lock with our own live pid is never stale (even the test's
        // own pid, which is alive by definition).
        let g6 = held(acquire_endpoint_lock(
            &locks, &sessions, "live", "py", "loopback", 2,
        ));
        let path2 = endpoint_lock_path(&locks, "py", "loopback", 2);
        // Rewrite mtime old but keep the live pid: liveness wins over age
        // until the backstop bound.
        let nonce_live = std::fs::read_to_string(&path2).unwrap();
        let mut v: Value = serde_json::from_str(&nonce_live).unwrap();
        v["pid"] = json!(live_pid);
        std::fs::write(&path2, v.to_string()).unwrap();
        backdate(&path2, Duration::from_secs(60));
        assert!(
            !reclaim_stale_lock(&path2),
            "live pid must not be reclaimed"
        );
        assert!(path2.exists(), "live record must survive");
        // Foreign release never removes our lock.
        release_endpoint_lock(&path2, "not-mine");
        assert!(path2.exists());
        drop(g6);
        assert!(!path2.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Set a file's mtime into the past (stale-policy tests control age
    /// directly — nothing here waits out a real bound).
    fn backdate(path: &std::path::Path, age: Duration) {
        let old = std::time::SystemTime::now()
            .checked_sub(age)
            .unwrap_or(std::time::UNIX_EPOCH);
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(old).unwrap();
        drop(f);
    }

    /// A provably-dead pid on any platform: spawn `true`, wait for it, and
    /// reuse its (now free) pid. No sleep, no guess.
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("true must spawn");
        let pid = child.id();
        child.wait().expect("true must exit");
        // The pid is free now (reuse in the wild is vanishingly unlikely
        // inside this assertion window; the stale tests pair it with old
        // mtimes, and holder_alive runs immediately).
        pid
    }

    #[test]
    fn holder_liveness_distinguishes_dead_from_live() {
        // Self is alive by definition; a reaped child is dead. Unknown
        // (ps missing/timed out) has no deterministic trigger and is
        // covered by the fail-closed match arm in lock_snapshot_is_stale.
        assert_eq!(holder_alive(std::process::id()), Liveness::Alive);
        assert_eq!(holder_alive(dead_pid()), Liveness::Dead);
    }

    #[test]
    fn stale_reclaim_never_deletes_a_fresh_claimant() {
        let base = tmpdir("endpoint-reclaim-swap");
        let locks = base.join("locks");
        std::fs::create_dir_all(&locks).unwrap();
        let path = endpoint_lock_path(&locks, "py", "loopback", 7001);
        // Seed a stale record and snapshot its exact bytes (the reclaimer's
        // view before the interleaving).
        let stale = json!({"nonce": "old", "pid": dead_pid(), "session": "gone", "createdAt": 1})
            .to_string();
        std::fs::write(&path, &stale).unwrap();
        backdate(&path, Duration::from_secs(60));
        // Interleaving: a fresh claimant replaces the record before our
        // re-verify runs (same live-test pid, current mtime).
        let fresh = json!({
            "nonce": "new",
            "pid": std::process::id(),
            "session": "fresh",
            "createdAt": 2,
        })
        .to_string();
        std::fs::write(&path, &fresh).unwrap();
        // The stale snapshot must not authorize deleting the fresh record.
        assert!(!quarantine_verified(&path, &stale));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            fresh,
            "fresh claimant must survive"
        );
        // And the full reclaim path agrees on the swapped file.
        assert!(!reclaim_stale_lock(&path));
        assert!(path.exists());
        // Backstop bound: age past 2h reclaims even with an unparseable
        // record (mtime set directly — no real 2h wait).
        std::fs::write(&path, "not-json{{{").unwrap();
        backdate(&path, ENDPOINT_LOCK_STALE + Duration::from_secs(5));
        assert!(reclaim_stale_lock(&path));
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn quarantine_restore_keeps_fresh_records() {
        // Deterministic coverage of the reconcile branches: detach F
        // (as if a rename won after the bytes changed), then reconcile
        // against different expected bytes.
        let base = tmpdir("endpoint-quarantine");
        let locks = base.join("locks");
        std::fs::create_dir_all(&locks).unwrap();
        let no_quarantine_orphans = || {
            let leftovers: Vec<_> = std::fs::read_dir(&locks)
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(".q-"))
                .collect();
            assert!(leftovers.is_empty(), "quarantine files must not linger");
        };
        // (a) Path free after detach: F moves back intact, hands off.
        let path = locks.join("ep.lock");
        let fresh = json!({"nonce": "n1", "pid": std::process::id(), "session": "f"}).to_string();
        std::fs::write(&path, &fresh).unwrap();
        let q = detach_to_quarantine(&path).unwrap();
        assert!(!path.exists());
        assert!(!reconcile_quarantine(&path, &q, "stale-bytes"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), fresh);
        no_quarantine_orphans();
        // (b) Path taken by F2 after detach: our copy drops, F2 intact,
        // hands off (F's owner backs off at its pre-spawn nonce check).
        std::fs::write(&path, &fresh).unwrap();
        let q2 = detach_to_quarantine(&path).unwrap();
        let fresh2 = json!({"nonce": "n2", "pid": std::process::id(), "session": "f2"}).to_string();
        std::fs::write(&path, &fresh2).unwrap();
        assert!(!reconcile_quarantine(&path, &q2, "stale-bytes"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), fresh2);
        no_quarantine_orphans();
        // (d) Non-overwriting restore unit behavior: free path restores
        // bytes verbatim; an F2 that lands between the absence check and
        // the restore (the old rename-overwrite window) is preserved
        // byte-for-byte while we fail closed; an unreadable quarantine
        // source leaves the path untouched.
        let qp = locks.join(".q-restore-src.tmp");
        std::fs::write(&qp, &fresh).unwrap();
        let rp = locks.join("restore-free.lock");
        assert!(restore_quarantine_bytes(&rp, &qp));
        assert_eq!(std::fs::read_to_string(&rp).unwrap(), fresh);
        let rp2 = locks.join("restore-raced.lock");
        std::fs::write(&rp2, &fresh2).unwrap();
        assert!(!restore_quarantine_bytes(&rp2, &qp));
        assert_eq!(
            std::fs::read_to_string(&rp2).unwrap(),
            fresh2,
            "an F2 in the check→restore window must survive intact"
        );
        let missing_q = locks.join(".q-never-written.tmp");
        let rp3 = locks.join("restore-no-src.lock");
        assert!(!restore_quarantine_bytes(&rp3, &missing_q));
        assert!(!rp3.exists(), "failed restore must not plant a file");
        let _ = std::fs::remove_file(&qp);
        no_quarantine_orphans();
        // (c) Match: stale detached exactly as judged → dropped, true.
        std::fs::write(&path, &fresh).unwrap();
        let q3 = detach_to_quarantine(&path).unwrap();
        assert!(reconcile_quarantine(&path, &q3, &fresh));
        assert!(!path.exists());
        no_quarantine_orphans();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn stale_reclaim_racers_admit_exactly_one() {
        // N starters race one stale-seeded lock: reclaimers may all detach
        // the same stale bytes to quarantine, but the atomic claim admits
        // exactly one. Outcome assertion is deterministic (count == 1);
        // only scheduling varies. Guards stay held through the count (see
        // race test note).
        let base = tmpdir("endpoint-reclaim-race");
        let locks = base.join("locks");
        let sessions = base.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&locks).unwrap();
        let path = endpoint_lock_path(&locks, "py", "loopback", 7002);
        std::fs::write(
            &path,
            json!({"nonce": "old", "pid": dead_pid(), "session": "gone", "createdAt": 1})
                .to_string(),
        )
        .unwrap();
        backdate(&path, Duration::from_secs(60));
        std::thread::scope(|s| {
            let mut handles = vec![];
            let locks_r = &locks;
            let sess_r = &sessions;
            for i in 0..8 {
                handles.push(s.spawn(move || {
                    acquire_endpoint_lock(
                        locks_r,
                        sess_r,
                        &format!("reclaimer-{i}"),
                        "py",
                        "loopback",
                        7002,
                    )
                    .ok()
                    .and_then(|c| match c {
                        EndpointClaim::Held(g) => Some(g),
                        EndpointClaim::Busy(_) => None,
                    })
                }));
            }
            let results: Vec<Option<EndpointGuard>> =
                handles.into_iter().map(|h| h.join().unwrap()).collect();
            assert_eq!(
                results.iter().filter(|g| g.is_some()).count(),
                1,
                "exactly one stale-reclaim racer must win"
            );
        });
        // Every quarantine file is consumed (deleted or restored) on all
        // reconcile paths short of a crash: none may linger after the race.
        let orphans: Vec<_> = std::fs::read_dir(&locks)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".q-"))
            .collect();
        assert!(orphans.is_empty(), "quarantine files must not linger");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn endpoint_lock_concurrent_second_attach_loses() {
        // Two starters race one endpoint: exactly one wins (atomic
        // create_new), the other gets the holder — the TOCTOU preflight
        // alone could never guarantee.
        let base = tmpdir("endpoint-lock-race");
        let locks = base.join("locks");
        let sessions = base.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::thread::scope(|s| {
            let mut handles = vec![];
            let locks_r = &locks;
            let sess_r = &sessions;
            for i in 0..8 {
                handles.push(s.spawn(move || {
                    // Hold the guard: dropping it releases the endpoint,
                    // so counting requires keeping winners alive. Only
                    // Held counts — Busy is a (correct) loss, not a win.
                    let r = acquire_endpoint_lock(
                        locks_r,
                        sess_r,
                        &format!("racer-{i}"),
                        "py",
                        "loopback",
                        6000,
                    )
                    .ok()
                    .and_then(|c| match c {
                        EndpointClaim::Held(g) => Some(g),
                        EndpointClaim::Busy(_) => None,
                    });
                    (i, r)
                }));
            }
            let results: Vec<(i32, Option<EndpointGuard>)> =
                handles.into_iter().map(|h| h.join().unwrap()).collect();
            // Guards stay alive in `results` through the count: joining is
            // sequential, and dropping an early winner before late starters
            // even claim would re-open the endpoint (that flake, not a lock
            // bug, is why the count must hold every guard).
            let winners: Vec<i32> = results
                .iter()
                .filter_map(|(i, g)| g.as_ref().map(|_| *i))
                .collect();
            assert_eq!(winners.len(), 1, "exactly one racer must win");
        });
        let _ = std::fs::remove_dir_all(&base);
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
    fn attach_failure_envelope_keeps_message_adds_context() {
        // The bridge message stays verbatim (existing `attach failed`
        // matches keep working); diagnosis + redacted identities ride
        // alongside. No raw argv anywhere in the envelope.
        let msg = "attach failed (127.0.0.1:9): Connection refused — is the target started \
             with debugpy --listen 9 ?";
        let observed = attach_observed("127.0.0.1", 9);
        let requested = json!({"host": "127.0.0.1", "port": 9, "pid": Value::Null});
        let d = attach_diagnosis(Some(false), Some(false), "127.0.0.1:9");
        let err = attach_failure(msg.to_string(), d, &observed, &requested);
        assert_eq!(format!("{err:#}"), msg);
        let bf = err.downcast_ref::<BridgeFailure>().expect("typed failure");
        assert_eq!(
            bf.diagnosis.as_ref().unwrap()["code"],
            json!("endpoint-not-listening")
        );
        assert!(bf.wait_context.is_none());
        assert_eq!(bf.requested_target.as_ref().unwrap()["port"], json!(9));
        // Redacted by construction: observed argv (if any) carries no
        // secrets and the envelope serializes within caps.
        let ser = serde_json::to_string(bf.target_identity.as_ref().unwrap()).unwrap();
        assert!(ser.len() <= OBSERVED_TOTAL_CAP);
        // Preflight owner error names the session + endpoint, actionably.
        let err = endpoint_owned_failure(
            "first",
            false,
            "py",
            "127.0.0.1",
            5678,
            &observed,
            &requested,
        );
        let text = format!("{err:#}");
        assert!(text.contains("endpoint-already-attached"), "{text}");
        assert!(text.contains("'first'"), "{text}");
        assert!(text.contains("5678"), "{text}");
        assert!(text.contains("close it first"), "{text}");
        let bf = err.downcast_ref::<BridgeFailure>().expect("typed failure");
        assert_eq!(
            bf.diagnosis.as_ref().unwrap()["evidence"]["ownerSession"],
            json!("first")
        );
        let in_flight = endpoint_owned_failure("w", true, "py", "h", 1, &observed, &requested);
        assert!(
            format!("{in_flight:#}").contains("is attaching"),
            "in-flight wording"
        );
    }

    #[test]
    fn attach_setup_failure_classifies_with_verbatim_message() {
        // Shared helper behind all four setup-failure branches (fast
        // error.json, late settle, bridge exit, first-forward transport):
        // the message stays byte-identical, the diagnosis reflects a
        // fresh post probe, and the redacted identities ride along.
        let mk_spec = || SpawnSpec {
            lang: "py",
            kind: "attach",
            bridge_args: vec![],
            wait_secs: 1,
            stops: json!({}),
            requested: json!({"host": "127.0.0.1", "port": 9, "pid": Value::Null}),
            observed: attach_observed("127.0.0.1", 9),
            observed_hint: String::new(),
        };
        let spec = mk_spec();
        let msg = "cannot reach debug session on port 61234: connection refused (stale?)";
        let err = attach_setup_failure("127.0.0.1", 9, Some(false), msg.to_string(), &spec);
        assert_eq!(format!("{err:#}"), msg, "transport message verbatim");
        let bf = err.downcast_ref::<BridgeFailure>().expect("typed failure");
        // Port 9 is dead pre and post: not-listening, with identities.
        assert_eq!(
            bf.diagnosis.as_ref().unwrap()["code"],
            json!("endpoint-not-listening")
        );
        assert!(bf.target_identity.is_some());
        assert_eq!(bf.requested_target.as_ref().unwrap()["port"], json!(9));
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
}
