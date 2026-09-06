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
fn bridge_failure(resp: &Value) -> anyhow::Error {
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
            "scope": "global",
        }],
        "selected": "main",
        "ignored": 0,
        "droppedExited": 0,
        "target": "main",
        "targetIdentity": layered_identity_or_null(parsed.get("targetIdentity").cloned()),
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
    /// Layered seed identity (redacted, capped). Process bridges receive
    /// it as `--target-identity` and upgrade it from protocol facts; the
    /// browser accepts the flag and builds its own tab identity instead.
    pub target_identity: Value,
    /// One-line redacted hint derived from the seed (no root-cause claim).
    /// Bridges derive their own hint from the upgraded identity; the CLI
    /// appends this seed hint to spawn timeouts.
    pub identity_hint: String,
}

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

/// Cap one layered identity object: every string field to 512 chars, every
/// array to a bounded element count (tail marker), then the whole object to
/// 2KB serialized. Only the known identity shapes are capped (process/tab);
/// anything else passes through untouched. The total loop always terminates:
/// each pass either shrinks the longest string or, when the trunc marker
/// would not shrink it, collapses it to a 1-char marker (strictly smaller
/// than any picked string), so ~80+ small argv entries converge by dropping
/// rather than re-marking forever.
fn cap_identity(mut v: Value) -> Value {
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

fn unavailable(field: &str, reason: &str) -> Value {
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

fn identity_hint_inner(seed: &Value) -> String {
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

/// Schema v2 marker: every sidecar object carries `schemaVersion: 2`.
pub const SCHEMA_VERSION: u64 = 2;

/// True when one CLI-owned sidecar file is a v2 object: present, parseable,
/// an object, with `schemaVersion == 2`.
fn sidecar_is_v2(dir: &std::path::Path, file: &str) -> bool {
    std::fs::read_to_string(dir.join(file))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v.as_object().cloned())
        .and_then(|m| m.get("schemaVersion").and_then(|s| s.as_u64()))
        == Some(SCHEMA_VERSION)
}

/// Old iff a CLI-owned marker (`lang.json` OR `stops.json`) is
/// missing/corrupt/lacks `schemaVersion == 2`. Bridge-owned `session.json`
/// never gates by itself (absence is a startup row, not a version verdict).
fn cli_markers_v2(dir: &std::path::Path) -> bool {
    sidecar_is_v2(dir, "lang.json") && sidecar_is_v2(dir, "stops.json")
}

/// Gate for every command that speaks the bridge protocol: old dirs are
/// `status`-visible then `close`-only. Runs after `check_name` /
/// `check_dir_real`, before lang/port routing.
fn require_schema_v2(dir: &std::path::Path, name: &str) -> anyhow::Result<()> {
    if cli_markers_v2(dir) {
        return Ok(());
    }
    anyhow::bail!("unsupported session '{name}' (schema v1; close it and recreate)")
}

/// Atomic sidecar write (tmp+rename in the same dir): a concurrent `status`
/// never reads a torn file.
fn write_sidecar_atomic(dir: &std::path::Path, file: &str, content: &str) -> anyhow::Result<()> {
    let tmp = dir.join(format!("{file}.tmp"));
    std::fs::write(&tmp, content)
        .map_err(|e| anyhow::anyhow!("cannot write session {file}: {e}"))?;
    std::fs::rename(&tmp, dir.join(file))
        .map_err(|e| anyhow::anyhow!("cannot write session {file}: {e}"))?;
    Ok(())
}

/// Write sidecars and spawn the bridge process. Called only from `spawn`,
/// which removes the session dir if this fails.
fn setup_bridge(dir: &std::path::Path, spec: &SpawnSpec) -> anyhow::Result<std::process::Child> {
    // Spawn-time intent first: while the session lives, stops.json is
    // always present for resume. Unlike before, these writes propagate
    // errors — a session whose intent cannot persist must not start.
    // Both markers carry schemaVersion: 2 (atomic tmp+rename, same dir).
    write_sidecar_atomic(
        dir,
        "lang.json",
        &format!(
            "{{\"lang\":\"{}\",\"schemaVersion\":{SCHEMA_VERSION}}}",
            spec.lang
        ),
    )?;
    write_sidecar_atomic(
        dir,
        "stops.json",
        &serde_json::to_string_pretty(&spec.stops).unwrap_or_else(|_| "{}".to_string()),
    )?;

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
    // The layered seed identity rides as `--target-identity` (redacted,
    // capped CLI-side; the bridge upgrades it from protocol facts).
    // Inserted BEFORE the `--` program-args separator: anything after it
    // belongs to the target. The browser accepts the flag and builds its
    // own tab identity instead. Malformed seeds never fail the bridge —
    // every bridge degrades to an all-unavailable identity.
    let extra = vec![
        "--target-identity".to_string(),
        spec.target_identity.to_string(),
    ];
    match args.iter().position(|a| a == "--") {
        Some(pos) => {
            let tail = args.split_off(pos);
            args.extend(extra);
            args.extend(tail);
        }
        None => args.extend(extra),
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

/// Attach endpoint from a persisted intent: `requestedTarget` only (exact
/// CLI request, numeric port). The old `target`-summary fallback (string
/// ports) is gone: v2 intents always carry a numeric `requestedTarget`.
/// Anything unparseable reads as absent — never a block.
fn intent_endpoint(stops: &Value) -> Option<(String, u16)> {
    let v = stops.get("requestedTarget")?;
    let host = v.get("host")?.as_str()?;
    let port = v.get("port")?.as_u64()?;
    let port = u16::try_from(port).ok()?;
    Some((host.to_string(), port))
}

/// Proven-dead legacy dir: parseable `session.json` with a numeric nonzero
/// port AND a dead daemon (double TCP fail + no recent publish). Only this
/// proof authorizes spawn-time reclaim; everything else bails for `close`.
fn legacy_proven_dead(dir: &std::path::Path) -> bool {
    let port: Option<u16> = std::fs::read_to_string(dir.join("session.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v.get("port").and_then(|p| p.as_u64()))
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p != 0);
    port.is_some() && !daemon_alive_in(dir)
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

/// Global live-legacy scan (fail-safe upgrade rule): every remaining live
/// OLD session dir (CLI markers not v2) whose lang is exclusive or
/// unknown/missing (fail-closed — an unknowable owner may hold our
/// endpoint), excluding browser (CDP multiplexes, never collides) and the
/// session being started. Sorted by name. Read-only: nothing is deleted or
/// touched — a live old daemon is only ever cleaned by `close`.
fn find_live_legacy_sessions_in(sessions_root: &std::path::Path, exclude: &str) -> Vec<String> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(sessions_root) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == exclude || check_name(&name).is_err() {
            continue;
        }
        let dir = sessions_root.join(&name);
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.file_type().is_dir() => {}
            _ => continue,
        }
        if cli_markers_v2(&dir) {
            continue;
        }
        match session_lang_opt(&dir).as_deref() {
            Some("browser") => continue,
            Some(l) if attach_exclusive(l) => {}
            // Unknown/missing lang fails closed: it may be an exclusive
            // owner whose markers predate the scan.
            _ => {}
        }
        let kind: Option<String> = std::fs::read_to_string(dir.join("session.json"))
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|v| {
                v.get("kind")
                    .and_then(|k| k.as_str())
                    .map(|s| s.to_string())
            });
        match kind.as_deref() {
            Some("attach") | Some("launch") => {}
            _ => continue,
        }
        if !daemon_alive_in(&dir) {
            continue;
        }
        out.push(name);
    }
    out.sort();
    out
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

/// Cap (chars) for the sanitized attach-failure `cause` (raw adapter
/// message preserved separately from the concise top-level `error`).
pub const CAUSE_CAP: usize = 2048;

/// Sanitize a raw bridge/adapter message for the additive `cause` field:
/// bounded secret redaction (URL/query `token=`-style values, `Bearer`
/// tokens, `--token` argv forms) then a hard char cap. Never fabricates:
/// an empty input reads as empty, and useful line/class context survives.
fn sanitize_cause(raw: &str) -> String {
    let redacted = redact_cause_secrets(raw);
    if redacted.chars().count() <= CAUSE_CAP {
        return redacted;
    }
    trunc_chars(&redacted, CAUSE_CAP)
}

/// Bounded redaction over free-text adapter output (no regex dep): any
/// `name=value` / `name:value` pair whose name is a secret flag keeps the
/// name and masks the value; a `Bearer <token>` word masks the token; a
/// bare secret `--flag <value>` masks the following word. Delimiters are
/// whitespace, `&`, `;`, `,`, quotes — values never span them.
fn redact_cause_secrets(raw: &str) -> String {
    let masked_eq = mask_keyed_values(raw);
    mask_bare_flag_values(&mask_bearer(&masked_eq))
}

/// Mask `name=value` / `name:value` (incl. URL `?token=abc&x=1`) whose name
/// is a secret flag. Walks `=`/`:` sites, extracts the trailing name token,
/// checks it with the argv redactor's predicate, and masks the value span.
fn mask_keyed_values(s: &str) -> String {
    let bytes: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == '=' || c == ':' {
            // Extract the name token immediately before the separator:
            // [A-Za-z0-9_.-]+ plus URL prefixes (? & ;). A leading run of
            // `?`/`&`/`;`/`#` is skipped, then the name must be non-empty.
            let mut j = out.len();
            while j > 0 && matches!(out.as_bytes()[j - 1], b'?' | b'&' | b';' | b'#') {
                j -= 1;
            }
            let mut k = j;
            while k > 0 {
                let b = out.as_bytes()[k - 1];
                if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
                    k -= 1;
                } else {
                    break;
                }
            }
            let head = out[k..j].to_string();
            // Mask the value span: up to the next delimiter (whitespace,
            // `&`, `;`, `,`, quote). `://` after a bare scheme (`http:`)
            // is not a secret pair — its "value" starts with `//`, skip it.
            let mut v = i + 1;
            while v < bytes.len() && bytes[v].is_whitespace() {
                v += 1;
            }
            let mut vend = v;
            while vend < bytes.len()
                && !bytes[vend].is_whitespace()
                && !matches!(bytes[vend], '&' | ';' | ',' | '"' | '\'' | ')')
            {
                vend += 1;
            }
            let val: String = bytes[v..vend].iter().collect();
            // A `Bearer <token>` value belongs to the bearer pass (which
            // keeps the scheme and masks the token): masking the scheme word
            // here would orphan the token into the clear.
            if val.eq_ignore_ascii_case("bearer") {
                out.push(c);
                i += 1;
                continue;
            }
            if !head.is_empty()
                && is_secret_flag(&head)
                && !val.is_empty()
                && !val.starts_with("//")
            {
                out.push(c);
                // Preserve one skipped whitespace exactly when the input
                // had `key: value` spacing.
                if v > i + 1 {
                    out.push(' ');
                }
                out.push_str("[redacted]");
                i = vend;
                continue;
            }
            out.push(c);
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Mask `Bearer <token>` (case-insensitive scheme): the token word becomes
/// `[redacted]`; the scheme itself is kept.
fn mask_bearer(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.to_ascii_lowercase().find("bearer ") {
        let token_start = pos + "bearer ".len();
        let mut vend = token_start;
        while vend < rest.len() {
            let b = rest.as_bytes()[vend];
            if b.is_ascii_whitespace() || matches!(b, b'"' | b'\'' | b',' | b';' | b')') {
                break;
            }
            vend += 1;
        }
        // Keep the original scheme casing, mask only a non-empty token.
        if vend > token_start {
            out.push_str(&rest[..pos]);
            out.push_str(&rest[pos..token_start]);
            out.push_str("[redacted]");
            rest = &rest[vend..];
        } else {
            out.push_str(&rest[..token_start]);
            rest = &rest[token_start..];
        }
    }
    out.push_str(rest);
    out
}

/// Mask the value word after a bare secret flag (`--token abc123` with no
/// `=`/`:`). Words split on ASCII whitespace; a following word starting
/// with `-` is another flag, never a value.
fn mask_bare_flag_values(s: &str) -> String {
    let mut out = String::new();
    let mut pending_secret = false;
    // Tokenize into alternating word/whitespace spans to preserve spacing.
    let bytes = s.as_bytes();
    let mut spans: Vec<(bool, &str)> = vec![]; // (is_word, text)
    let mut start = 0;
    while start < s.len() {
        let word = !bytes[start].is_ascii_whitespace();
        let mut end = start + 1;
        while end < s.len() && (!bytes[end].is_ascii_whitespace()) == word {
            end += 1;
        }
        spans.push((word, &s[start..end]));
        start = end;
    }
    for (is_word, span) in spans {
        if !is_word {
            out.push_str(span);
            continue;
        }
        if pending_secret {
            pending_secret = false;
            // A flag-looking word was never a value: keep it, re-arm when it
            // is itself a bare secret flag.
            if span.starts_with('-') && span.len() > 1 {
                out.push_str(span);
                if is_secret_flag(span) && !span.contains('=') && !span.contains(':') {
                    pending_secret = true;
                }
                continue;
            }
            out.push_str("[redacted]");
            continue;
        }
        out.push_str(span);
        if is_secret_flag(span) && !span.contains('=') && !span.contains(':') {
            pending_secret = true;
        }
    }
    out
}

/// Concise diagnosis-aligned top-level error for a diagnosed attach
/// failure. Same truth/confidence as the diagnosis code, always prefixed
/// with `attach failed:` so existing prefix matches keep working. The raw
/// adapter text never appears here — it rides in sanitized `cause`.
fn diagnosed_attach_error(code: &str, endpoint: &str) -> String {
    match code {
        "endpoint-not-listening" => {
            format!("attach failed: no debug listener found at {endpoint}")
        }
        "endpoint-closed-during-attach" => {
            format!("attach failed: debug endpoint closed while attaching ({endpoint})")
        }
        "endpoint-rejected" => {
            format!(
                "attach failed: debug endpoint rejected the connection at {endpoint}; \
                 it may already have another debugger client"
            )
        }
        "endpoint-unreachable" => {
            format!(
                "attach failed: could not reach debug endpoint at {endpoint} \
                 (unverified; check host/port)"
            )
        }
        _ => format!("attach failed: could not attach to {endpoint}"),
    }
}

/// One classification site for every attach setup failure (fast error.json,
/// late-settling error.json, bridge exit, first-forward transport): probe
/// the OS listener now, classify pre-vs-post, derive the concise
/// diagnosis-aligned top-level `error` from the code, keep the raw
/// bridge/transport message as sanitized additive `cause`, and attach the
/// redacted identities. The debuggee is never fabricated — on a failed
/// attach the attempted identity keeps its `unavailable` entries as the
/// bridge/lookup reported them.
fn attach_setup_failure(
    host: &str,
    port: u16,
    pre: Option<bool>,
    message: String,
    spec: &SpawnSpec,
) -> anyhow::Error {
    let post = listener_present(host, port);
    let endpoint = display_endpoint(host, port);
    let diagnosis = attach_diagnosis(pre, post, &endpoint);
    let code = diagnosis
        .get("code")
        .and_then(|c| c.as_str())
        .unwrap_or("endpoint-unreachable");
    let error = diagnosed_attach_error(code, &endpoint);
    let cause = sanitize_cause(&message);
    attach_failure(
        error,
        Some(cause),
        diagnosis,
        &spec.target_identity,
        &spec.requested,
    )
}

/// Build the typed attach-setup failure: the top-level `error` is the
/// concise diagnosis-aligned statement (always `attach failed:`-prefixed);
/// the raw adapter text rides separately as sanitized `cause`; the
/// diagnosis, redacted attempted identity, and redacted requested endpoint
/// ride alongside into the outer envelope.
fn attach_failure(
    error: String,
    cause: Option<String>,
    diagnosis: Value,
    identity: &Value,
    requested: &Value,
) -> anyhow::Error {
    // A cause identical to the concise error carries no information —
    // drop it so JSON/human never duplicate the same string twice.
    let cause = cause.filter(|c| !c.is_empty() && *c != error);
    BridgeFailure {
        message: error,
        cause,
        wait_context: None,
        diagnosis: Some(diagnosis),
        target_identity: Some(identity.clone()),
        requested_target: Some(requested.clone()),
    }
    .into()
}

/// Semantic attach-setup failure (bridge-reported `phase: "config"`): the
/// debug connection was established and the failure is a configuration
/// error (invalid breakpoint/method/line/condition/source). The semantic
/// message stays the top-level error verbatim — no endpoint diagnosis, no
/// cause duplication (there is no separate raw text). The redacted
/// attempted identity and requested endpoint still ride along; they are
/// spawn-time OS observations, never a diagnosis claim.
fn attach_config_failure(message: String, identity: &Value, requested: &Value) -> anyhow::Error {
    BridgeFailure {
        message,
        cause: None,
        wait_context: None,
        diagnosis: None,
        target_identity: Some(identity.clone()),
        requested_target: Some(requested.clone()),
    }
    .into()
}

/// True only for the bridge-reported config phase: the connection was
/// established, so the message is semantic, not a transport symptom.
/// Anything else takes the conservative transport path with endpoint
/// diagnosis (corrupt files never reach here — they bail earlier).
fn is_config_phase(phase: Option<&str>) -> bool {
    matches!(phase, Some("config"))
}

/// Preflight rejection when a live session already owns the endpoint.
/// Names the confirmed owner and the redacted endpoint; tells the agent to
/// reuse or close it. No bridge is ever spawned, so the first session's
/// target is untouched. No adapter ran, so there is no raw `cause` — the
/// concise `attach failed:` error plus the diagnosis carry everything.
/// `owner_identity` is the owner's layered `{debuggee, endpoint, adapter}`
/// identity (see `owner_target_identity`) — never a raw pid claim, which
/// would misdirect a kill at the adapter on wrapped servers.
fn endpoint_owned_failure(
    owner: &str,
    in_progress: bool,
    lang: &str,
    host: &str,
    port: u16,
    owner_identity: &Value,
    requested: &Value,
) -> anyhow::Error {
    let endpoint = display_endpoint(host, port);
    let message = if in_progress {
        format!(
            "attach failed: endpoint is already attached by session '{owner}' \
             ({lang} {endpoint}) — use the existing session or wait and retry"
        )
    } else {
        format!(
            "attach failed: endpoint is already attached by session '{owner}' \
             ({lang} {endpoint}) — use the existing session or close it first"
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
    attach_failure(message, None, diagnosis, owner_identity, requested)
}

/// Fail-closed rejection when live legacy (v1) sessions exist: their
/// endpoints may be unknowable, so no v2 attach of an exclusive lang may
/// proceed while any is live. Layered all-unavailable identity (no endpoint
/// claim, zero pids); the sorted names name the blockers to `close`.
fn legacy_live_failure(names: &str, requested: &Value) -> anyhow::Error {
    let message = format!(
        "attach failed: unsupported live legacy session(s) '{names}'; \
         close them first and retry"
    );
    let diagnosis = serde_json::json!({
        "code": "unsupported-legacy-live",
        "confidence": "high",
        "evidence": { "legacySessions": names },
        "recommendation": "close the listed legacy session(s) first and retry",
    });
    attach_failure(
        message,
        None,
        diagnosis,
        &unavailable_identity("live legacy session without layered identity"),
        requested,
    )
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
        if cli_markers_v2(&dir) {
            anyhow::bail!("session '{name}' already exists (close it first)");
        }
        // Intentional one-time legacy exception: an OLD dir is reclaimed
        // only when proven dead — parseable session.json with a numeric
        // nonzero port AND a dead daemon (double TCP fail + no recent
        // publish). Anything less (corrupt/missing/unparseable/zero port,
        // or a possibly-live daemon) bails: a live old daemon is never
        // killed by spawn/reclaim, only `close` may clean it.
        if !legacy_proven_dead(&dir) {
            anyhow::bail!("unsupported session '{name}' (schema v1; close it first)");
        }
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
                        let owner_identity = owner_target_identity(sessions_root, &owner);
                        return Err(endpoint_owned_failure(
                            &owner,
                            false,
                            spec.lang,
                            host,
                            *port,
                            &owner_identity,
                            &spec.requested,
                        ));
                    }
                    // Fail-safe upgrade rule: a pre-existing live v1 owner
                    // may be invisible to the endpoint match (old intents
                    // can lack a usable endpoint). Any remaining live legacy
                    // session blocks the attach outright — never risk a
                    // second client onto a possibly-shared target.
                    let legacy_live = find_live_legacy_sessions_in(sessions_root, name);
                    if !legacy_live.is_empty() {
                        drop(g);
                        return Err(legacy_live_failure(
                            &legacy_live.join("', '"),
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
                        let owner_identity = owner_target_identity(sessions_root, &cur);
                        return Err(endpoint_owned_failure(
                            &cur,
                            !published,
                            spec.lang,
                            host,
                            *port,
                            &owner_identity,
                            &spec.requested,
                        ));
                    }
                    Some(g)
                }
                EndpointClaim::Busy(busy) => {
                    let owner_identity = owner_target_identity(sessions_root, &busy.session);
                    return Err(endpoint_owned_failure(
                        &busy.session,
                        !busy.published,
                        spec.lang,
                        host,
                        *port,
                        &owner_identity,
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
            let failure = read_bridge_error(&dir);
            reap(&mut child);
            let _ = std::fs::remove_dir_all(&dir);
            // A present-but-corrupt setup error file is an internal error:
            // exact message, never transport-diagnosed.
            if failure.corrupt {
                anyhow::bail!("{}", failure.message);
            }
            // Attach setup failure: config phase (connection was
            // established) keeps the semantic message top-level with no
            // endpoint diagnosis; transport takes the OS-observed listener
            // state (pre-attach vs now) with the raw text as sanitized
            // cause. Every v2 bridge writes schemaVersion + phase.
            if let Some((host, port)) = &endpoint {
                if is_config_phase(failure.phase.as_deref()) {
                    return Err(attach_config_failure(
                        failure.message,
                        &spec.target_identity,
                        &spec.requested,
                    ));
                }
                let pre = pre_listener.flatten();
                return Err(attach_setup_failure(
                    host,
                    *port,
                    pre,
                    failure.message,
                    spec,
                ));
            }
            anyhow::bail!("{}", failure.message);
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
                    // requested from the CLI flags, layered targetIdentity
                    // (debuggee / endpoint / adapter roles) from the
                    // bridge-persisted session.json. No spawn-time fallback:
                    // the roles need bridge-observed protocol facts, so a
                    // missing copy reads as null (honest "unknown").
                    data["requestedTarget"] = spec.requested.clone();
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
                    if let Some(failure) = settle_for_bridge_error(&dir) {
                        reap(&mut child);
                        let _ = std::fs::remove_dir_all(&dir);
                        if failure.corrupt {
                            anyhow::bail!("{}", failure.message);
                        }
                        // Late-settling attach failure: same phase routing
                        // as the fast path (config keeps the semantic
                        // message; transport takes a fresh probe).
                        if let Some((host, port)) = &endpoint {
                            if is_config_phase(failure.phase.as_deref()) {
                                return Err(attach_config_failure(
                                    failure.message,
                                    &spec.target_identity,
                                    &spec.requested,
                                ));
                            }
                            let pre = pre_listener.flatten();
                            return Err(attach_setup_failure(
                                host,
                                *port,
                                pre,
                                failure.message,
                                spec,
                            ));
                        }
                        anyhow::bail!("{}", failure.message);
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
                spec.identity_hint
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Layered target identity for a live session dir (session.json
/// `targetIdentity`: `{debuggee, endpoint, adapter}` roles built by the
/// bridge from protocol-confirmed + OS-corroborated sources). No spawn-time
/// fallback: the roles need bridge-observed protocol facts the CLI never
/// has, so a missing copy reads as null (honest "unknown"), never a
/// fabricated role. Only a valid layered object is ever echoed (re-redacted
/// + capped defensively); flat/malformed shapes read as null.
fn cached_target_identity(dir: &std::path::Path) -> Value {
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
fn layered_identity_or_null(stored: Option<Value>) -> Value {
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
fn owner_target_identity(sessions_root: &std::path::Path, owner: &str) -> Value {
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
fn is_layered_identity(v: &Value) -> bool {
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
fn sanitize_layered_identity(v: &Value) -> Value {
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
fn reredact_argv_in(v: &mut Value) {
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
fn unavailable_identity(reason: &str) -> Value {
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

fn identity_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

/// Bridge setup-failure truth with a bounded settle: error.json can trail
/// the dead session.json by bridge-cleanup lag (the full-suite test11
/// race). Immediate check first — no added latency when the files land in
/// order — then a bounded re-poll at the wait-loop cadence. Only entered
/// after a failed first forward, so the success path never sleeps.
/// Returns the bridge failure when the file lands in budget.
fn settle_for_bridge_error(dir: &std::path::Path) -> Option<BridgeSetupError> {
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

/// Bridge setup-failure file (v2): `{"schemaVersion":2,"error":"…",
/// "phase":"transport"|"config"}`. Every v2 bridge writes all three keys on
/// every early path. Strict action: a present parseable `error.json` that
/// carries an `error` string but the wrong `schemaVersion` or a
/// missing/invalid `phase` reads as corrupt (`corrupt: true` with the exact
/// actionable message — internal, never transport-diagnosed). Absent,
/// unparseable, or error-less files keep the transport fallback
/// (`corrupt: false`, default message, `phase: None`).
#[derive(Debug)]
struct BridgeSetupError {
    message: String,
    phase: Option<String>,
    corrupt: bool,
}

fn read_bridge_error(dir: &std::path::Path) -> BridgeSetupError {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let parsed: Option<Value> = std::fs::read_to_string(dir.join("error.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok());
    let message = match parsed
        .as_ref()
        .and_then(|v| v.get("error"))
        .and_then(|e| e.as_str())
    {
        Some(s) => s.to_string(),
        None => {
            return BridgeSetupError {
                message: "bridge failed during setup (see bridge.log)".to_string(),
                phase: None,
                corrupt: false,
            };
        }
    };
    let version_ok = parsed
        .as_ref()
        .and_then(|v| v.get("schemaVersion").and_then(|s| s.as_u64()))
        == Some(SCHEMA_VERSION);
    let phase = parsed
        .as_ref()
        .and_then(|v| v.get("phase"))
        .and_then(|p| p.as_str())
        .map(|s| s.to_string());
    let phase_ok = matches!(phase.as_deref(), Some("transport") | Some("config"));
    if !version_ok || !phase_ok {
        return BridgeSetupError {
            message: format!(
                "corrupt setup error file in '{name}' (schemaVersion/phase); close and retry"
            ),
            phase: None,
            corrupt: true,
        };
    }
    BridgeSetupError {
        message,
        phase,
        corrupt: false,
    }
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
/// Never errors a row: unparseable files read as nulls, never fabricated.
/// Exact frozen contract — current rows keep every existing key, drop
/// `observedTarget`, and add `stale`/`unsupported`/`hint`:
/// - current: v2 CLI markers plus (when session.json is present) a v2
///   session marker; `stale:false, unsupported:false, hint:null`.
/// - old/unsupported: anything else (incl. a present session.json lacking
///   `schemaVersion==2`, even when the CLI markers are v2); `stale:true,
///   unsupported:true, hint:"close '<name>' and recreate (unsupported
///   schema v1)"`, with `lang` from lang.json else `"unknown"`, `port`
///   numeric-u16 else `0`, `alive` by TCP probe iff port nonzero,
///   `kind`/`stopped`/`lastStop`/`updatedAt` from parseable session.json
///   else `null`, `armed`/`target`/`requestedTarget` from parseable
///   stops.json else nulls, `targetIdentity` when a valid layered object
///   else `null` (never flat, never promoted).
fn session_entry(dir: &std::path::Path) -> Value {
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
    let alive = port != 0
        && std::net::TcpStream::connect_timeout(
            &format!("127.0.0.1:{port}").parse().unwrap(),
            Duration::from_millis(300),
        )
        .is_ok();
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
            "hint": Value::Null,
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
        assert_eq!(
            normalize_target_for_lang_opt(Some("java"), None).unwrap(),
            None
        );
        assert_eq!(
            normalize_target_for_lang_opt(Some("browser"), Some("main")).unwrap(),
            None
        );
        assert_eq!(
            normalize_target_for_lang_opt(Some("py"), Some("main")).unwrap(),
            None
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
            target_identity: Value::Null,
            identity_hint: String::new(),
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
    fn attach_failure_envelope_aligns_error_preserves_cause() {
        // Diagnosed attach failures: the top-level error is the concise
        // diagnosis-aligned statement (always `attach failed:`-prefixed,
        // never the raw adapter text); the raw message rides separately as
        // sanitized `cause`; diagnosis + redacted identities ride alongside.
        let raw = "attach failed (127.0.0.1:9): Connection refused — is the target started \
             with debugpy --listen 9 ?";
        let seed = attach_seed("127.0.0.1", 9);
        let requested = json!({"host": "127.0.0.1", "port": 9, "pid": Value::Null});
        let d = attach_diagnosis(Some(false), Some(false), "127.0.0.1:9");
        let code = d["code"].as_str().unwrap().to_string();
        let error = diagnosed_attach_error(&code, "127.0.0.1:9");
        let cause = sanitize_cause(raw);
        let err = attach_failure(error.clone(), Some(cause.clone()), d, &seed, &requested);
        let text = format!("{err:#}");
        assert!(text.starts_with("attach failed:"), "{text}");
        assert!(text.contains("no debug listener found"), "{text}");
        assert!(!text.contains("is the target started"), "{text}");
        let bf = err.downcast_ref::<BridgeFailure>().expect("typed failure");
        assert_eq!(
            bf.diagnosis.as_ref().unwrap()["code"],
            json!("endpoint-not-listening")
        );
        let kept = bf.cause.as_ref().expect("cause preserved");
        assert!(kept.contains("Connection refused"), "{kept}");
        assert_ne!(kept, &text, "cause must not duplicate error");
        assert!(bf.wait_context.is_none());
        assert_eq!(bf.requested_target.as_ref().unwrap()["port"], json!(9));
        // Redacted by construction: observed argv (if any) carries no
        // secrets and the envelope serializes within caps.
        let ser = serde_json::to_string(bf.target_identity.as_ref().unwrap()).unwrap();
        assert!(ser.len() <= IDENTITY_TOTAL_CAP);
        // Preflight owner errors are also `attach failed:`-prefixed, name
        // the session + endpoint actionably, and carry no adapter cause.
        // The identity is the owner's layered roles, never flat observed.
        let owner_identity = json!({
            "debuggee": {"kind": "process", "pid": 4242, "confidence": "protocol-confirmed"},
            "endpoint": {"ownerPid": 4343, "confidence": "os-corroborated"},
            "adapter": {"pid": 4343, "confidence": "os-corroborated"},
        });
        let err = endpoint_owned_failure(
            "first",
            false,
            "py",
            "127.0.0.1",
            5678,
            &owner_identity,
            &requested,
        );
        let text = format!("{err:#}");
        assert!(text.starts_with("attach failed:"), "{text}");
        assert!(
            text.contains("already attached by session 'first'"),
            "{text}"
        );
        assert!(text.contains("5678"), "{text}");
        assert!(text.contains("close it first"), "{text}");
        let bf = err.downcast_ref::<BridgeFailure>().expect("typed failure");
        assert_eq!(
            bf.diagnosis.as_ref().unwrap()["evidence"]["ownerSession"],
            json!("first")
        );
        assert!(bf.cause.is_none(), "preflight ran no adapter: no cause");
        let layered = bf.target_identity.as_ref().expect("layered owner identity");
        assert!(is_layered_identity(layered), "{layered}");
        assert!(
            layered.get("pid").is_none(),
            "no flat top-level pid: {layered}"
        );
        let in_flight =
            endpoint_owned_failure("w", true, "py", "h", 1, &owner_identity, &requested);
        let in_text = format!("{in_flight:#}");
        assert!(in_text.starts_with("attach failed:"), "{in_text}");
        assert!(
            in_text.contains("already attached by session 'w'"),
            "in-flight wording keeps the same aligned phrase: {in_text}"
        );
        assert!(in_text.contains("wait and retry"), "{in_text}");
    }

    #[test]
    fn owner_collision_identity_is_layered_never_flat() {
        // Current owner with a valid layered identity: preserved verbatim
        // (roles intact), defensively re-redacted + capped, never flattened.
        let root = tmpdir("owner-layered");
        let owner = root.join("first");
        std::fs::create_dir_all(&owner).unwrap();
        std::fs::write(
            owner.join("session.json"),
            r#"{"name":"first","kind":"attach",
                "observedTarget":{"kind":"process","pid":4343,
                    "executable":"python","argv":["python","-m","debugpy","--listen","127.0.0.1:5678"],
                    "cwd":"/work","source":"os-proc","observedAt":1},
                "targetIdentity":{
                    "debuggee":{"kind":"process","pid":4242,"name":"srv.py",
                        "confidence":"protocol-confirmed","observedAt":2,"unavailable":[]},
                    "endpoint":{"host":"127.0.0.1","port":5678,"ownerPid":4343,
                        "role":"listener-owner (not necessarily the debuggee)",
                        "source":"os-proc","confidence":"os-corroborated",
                        "observedAt":2,"unavailable":[]},
                    "adapter":{"name":"debugpy-adapter","pid":4343,
                        "source":"os-proc","confidence":"os-corroborated",
                        "observedAt":2,"unavailable":[]}}}"#,
        )
        .unwrap();
        let ident = owner_target_identity(&root, "first");
        assert!(is_layered_identity(&ident), "{ident}");
        assert!(ident.get("pid").is_none(), "no flat top-level pid: {ident}");
        assert_eq!(ident["debuggee"]["pid"], json!(4242));
        assert_eq!(ident["endpoint"]["ownerPid"], json!(4343));
        assert_eq!(ident["adapter"]["pid"], json!(4343));
        // The requester's flat observed pid (the adapter) is not what the
        // collision reports: the error carries the owner's layered roles.
        let err = endpoint_owned_failure(
            "first",
            false,
            "py",
            "127.0.0.1",
            5678,
            &ident,
            &json!({"host": "127.0.0.1", "port": 5678, "pid": Value::Null}),
        );
        let bf = err.downcast_ref::<BridgeFailure>().expect("typed failure");
        let carried = bf.target_identity.as_ref().expect("owner identity carried");
        assert!(is_layered_identity(carried), "{carried}");
        assert_eq!(carried["debuggee"]["pid"], json!(4242));
        // JSON envelope keeps the layered shape end to end.
        let env = crate::output::build_error_envelope(
            "attach",
            &format!("{err:#}"),
            None,
            None,
            bf.diagnosis.clone(),
            bf.target_identity.clone(),
            bf.requested_target.clone(),
        );
        assert!(is_layered_identity(&env["targetIdentity"]), "{env}");
        assert!(env["targetIdentity"].get("pid").is_none(), "{env}");
        assert_eq!(env["diagnosis"]["code"], json!("endpoint-already-attached"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn owner_collision_reredacts_stale_raw_secrets() {
        // A stale raw secret cached in the owner's stored argv never leaks
        // through the collision response: sanitize re-redacts defensively.
        let root = tmpdir("owner-reredact");
        let owner = root.join("first");
        std::fs::create_dir_all(&owner).unwrap();
        std::fs::write(
            owner.join("session.json"),
            r#"{"name":"first","kind":"attach",
                "targetIdentity":{
                    "debuggee":{"kind":"process","pid":11,"confidence":"protocol-confirmed",
                        "observedAt":1,"unavailable":[]},
                    "endpoint":{"host":"h","port":9,"ownerPid":22,
                        "argv":["python","--token","hunter2"],
                        "confidence":"os-corroborated","observedAt":1,"unavailable":[]},
                    "adapter":{"confidence":"unavailable","observedAt":1,
                        "unavailable":[{"field":"pid","reason":"x"}]}}}"#,
        )
        .unwrap();
        let ident = owner_target_identity(&root, "first");
        let ser = ident.to_string();
        assert!(!ser.contains("hunter2"), "{ser}");
        assert!(ser.contains("[redacted]"), "{ser}");
        assert_eq!(ident["debuggee"]["pid"], json!(11));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn owner_collision_legacy_flat_is_all_unavailable() {
        // Legacy owner (flat keys, no layered identity): all-unavailable —
        // no pid is ever promoted from legacy state (on a wrapped server
        // the listener-owner pid is the adapter, not the debuggee).
        let root = tmpdir("owner-legacy");
        let owner = root.join("first");
        std::fs::create_dir_all(&owner).unwrap();
        std::fs::write(
            owner.join("session.json"),
            r#"{"name":"first","kind":"attach",
                "observedTarget":{"kind":"process","pid":4343,
                    "executable":"python",
                    "argv":["python","-m","debugpy","--listen","127.0.0.1:5678"],
                    "cwd":"/work","source":"os-proc","observedAt":7,
                    "unavailable":[],"warnings":[]}}"#,
        )
        .unwrap();
        let ident = owner_target_identity(&root, "first");
        assert!(is_layered_identity(&ident), "{ident}");
        assert!(ident.get("pid").is_none(), "no flat top-level pid: {ident}");
        let ser = ident.to_string();
        assert!(!ser.contains("4343"), "legacy pid never promoted: {ser}");
        for role in ["debuggee", "endpoint", "adapter"] {
            assert_eq!(ident[role]["confidence"], json!("unavailable"), "{ident}");
        }
        assert_eq!(ident["debuggee"]["pid"], Value::Null);
        // A layered null reads the same way.
        std::fs::write(
            owner.join("session.json"),
            r#"{"name":"first","kind":"attach","targetIdentity":null}"#,
        )
        .unwrap();
        let ident2 = owner_target_identity(&root, "first");
        assert!(is_layered_identity(&ident2), "{ident2}");
        assert_eq!(ident2["debuggee"]["pid"], Value::Null);
        assert_eq!(ident2["debuggee"]["confidence"], json!("unavailable"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn owner_collision_malformed_identity_carries_no_pid() {
        // Malformed stored identities (flat shape under `targetIdentity`,
        // wrong role types, non-object) normalize to all-unavailable: no
        // pid is promoted anywhere, and the flat value is never forwarded.
        let root = tmpdir("owner-malformed");
        let owner = root.join("first");
        std::fs::create_dir_all(&owner).unwrap();
        for (tag, body) in [
            (
                "flat",
                r#"{"targetIdentity":{"kind":"process","pid":4343,"argv":["x"]},
                    "observedTarget":{"kind":"process","pid":4343,"argv":["python"]}}"#,
            ),
            (
                "role-type",
                r#"{"targetIdentity":{"debuggee":7,"endpoint":null,"adapter":null}}"#,
            ),
            (
                "missing-role",
                r#"{"targetIdentity":{"debuggee":null,"endpoint":null}}"#,
            ),
            ("array", r#"{"targetIdentity":[1,2]}"#),
        ] {
            std::fs::write(owner.join("session.json"), body).unwrap();
            let ident = owner_target_identity(&root, "first");
            assert!(is_layered_identity(&ident), "{tag}: {ident}");
            let ser = ident.to_string();
            assert!(!ser.contains("4343"), "{tag}: pid leaked: {ser}");
            for role in ["debuggee", "endpoint", "adapter"] {
                assert_eq!(
                    ident[role]["confidence"],
                    json!("unavailable"),
                    "{tag}: {ident}"
                );
                assert!(
                    ident[role]["unavailable"]
                        .as_array()
                        .is_some_and(|a| !a.is_empty()),
                    "{tag}: {ident}"
                );
            }
            assert!(ident.get("pid").is_none(), "{tag}: {ident}");
        }
        // Unknown owner (e.g. unpublished lock holder): same shape, no pid.
        let ident = owner_target_identity(&root, "ghost");
        assert!(is_layered_identity(&ident), "{ident}");
        assert!(ident["debuggee"]["pid"].is_null(), "{ident}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn attach_setup_failure_aligns_error_preserves_cause() {
        // Shared helper behind all four setup-failure branches (fast
        // error.json, late settle, bridge exit, first-forward transport):
        // the top-level error is the concise aligned statement, the raw
        // transport message rides as sanitized `cause`, the diagnosis
        // reflects a fresh post probe, and the redacted identities ride
        // along.
        let mk_spec = || SpawnSpec {
            lang: "py",
            kind: "attach",
            bridge_args: vec![],
            wait_secs: 1,
            stops: json!({}),
            requested: json!({"host": "127.0.0.1", "port": 9, "pid": Value::Null}),
            target_identity: attach_seed("127.0.0.1", 9),
            identity_hint: String::new(),
        };
        let spec = mk_spec();
        let msg = "cannot reach debug session on port 61234: connection refused (stale?)";
        let err = attach_setup_failure("127.0.0.1", 9, Some(false), msg.to_string(), &spec);
        let text = format!("{err:#}");
        assert!(text.starts_with("attach failed:"), "{text}");
        assert!(text.contains("no debug listener found"), "{text}");
        assert!(
            !text.contains("stale?"),
            "raw transport text leaves error: {text}"
        );
        let bf = err.downcast_ref::<BridgeFailure>().expect("typed failure");
        // Port 9 is dead pre and post: not-listening, with identities.
        assert_eq!(
            bf.diagnosis.as_ref().unwrap()["code"],
            json!("endpoint-not-listening")
        );
        assert_eq!(bf.cause.as_deref(), Some(msg));
        assert!(bf.target_identity.is_some());
        assert_eq!(bf.requested_target.as_ref().unwrap()["port"], json!(9));
    }

    #[test]
    fn bridge_setup_error_is_strict_v2() {
        let dir = tmpdir("phase-parse");
        // Config phase with schemaVersion 2: semantic error, no diagnosis.
        std::fs::write(
            dir.join("error.json"),
            r#"{"schemaVersion":2,"error":"no method noSuchMethod() in IdleAttach","phase":"config"}"#,
        )
        .unwrap();
        let e = read_bridge_error(&dir);
        assert!(!e.corrupt);
        assert_eq!(e.message, "no method noSuchMethod() in IdleAttach");
        assert!(is_config_phase(e.phase.as_deref()));
        // Transport phase with schemaVersion 2: listener evidence decides.
        std::fs::write(
            dir.join("error.json"),
            r#"{"schemaVersion":2,"error":"attach failed: refused","phase":"transport"}"#,
        )
        .unwrap();
        let e = read_bridge_error(&dir);
        assert!(!e.corrupt);
        assert!(!is_config_phase(e.phase.as_deref()));
        // Present + parseable but missing/bad version/phase: exact corrupt
        // message (internal, never transport-diagnosed).
        for (tag, body) in [
            (
                "missing-phase",
                r#"{"schemaVersion":2,"error":"attach failed: refused"}"#,
            ),
            (
                "missing-version",
                r#"{"error":"attach failed: refused","phase":"transport"}"#,
            ),
            (
                "bad-version",
                r#"{"schemaVersion":1,"error":"x","phase":"transport"}"#,
            ),
            (
                "bad-phase",
                r#"{"schemaVersion":2,"error":"x","phase":"runtime"}"#,
            ),
            (
                "nonstring-phase",
                r#"{"schemaVersion":2,"error":"x","phase":7}"#,
            ),
        ] {
            std::fs::write(dir.join("error.json"), body).unwrap();
            let e = read_bridge_error(&dir);
            assert!(e.corrupt, "{tag}");
            assert!(
                e.message.contains("corrupt setup error file")
                    && e.message.contains("(schemaVersion/phase)")
                    && e.message.contains("close and retry"),
                "{tag}: {e:?}"
            );
            assert!(e.phase.is_none(), "{tag}");
        }
        // Absent/unparseable/error-less file: transport fallback, not corrupt.
        std::fs::write(dir.join("error.json"), "not json").unwrap();
        let e = read_bridge_error(&dir);
        assert!(!e.corrupt);
        assert!(e.message.contains("see bridge.log"), "{e:?}");
        assert!(!is_config_phase(e.phase.as_deref()));
        std::fs::write(
            dir.join("error.json"),
            r#"{"schemaVersion":2,"phase":"config"}"#,
        )
        .unwrap();
        let e = read_bridge_error(&dir);
        assert!(!e.corrupt, "no error string: fallback, not corrupt");
        std::fs::remove_file(dir.join("error.json")).unwrap();
        assert!(!is_config_phase(read_bridge_error(&dir).phase.as_deref()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn attach_config_phase_preserves_semantic_error() {
        // Semantic config error: the exact message stays top-level (never
        // rewritten to `attach failed:`), with no diagnosis and no cause
        // duplication — but the redacted identities still ride along.
        let seed = attach_seed("127.0.0.1", 9);
        let requested = json!({"host": "127.0.0.1", "port": 9, "pid": Value::Null});
        let msg = "no method noSuchMethod() in IdleAttach";
        let err = attach_config_failure(msg.to_string(), &seed, &requested);
        assert_eq!(format!("{err:#}"), msg);
        assert!(
            !format!("{err:#}").contains("attach failed"),
            "semantic errors keep no transport prefix"
        );
        let bf = err.downcast_ref::<BridgeFailure>().expect("typed failure");
        assert!(bf.diagnosis.is_none(), "no misleading endpoint diagnosis");
        assert!(bf.cause.is_none(), "no cause duplication");
        assert!(bf.target_identity.is_some());
        assert_eq!(bf.requested_target.as_ref().unwrap()["port"], json!(9));
    }

    #[test]
    fn settle_returns_structured_phase() {
        let dir = tmpdir("phase-settle");
        std::fs::write(
            dir.join("error.json"),
            r#"{"schemaVersion":2,"error":"no method x() in Y","phase":"config"}"#,
        )
        .unwrap();
        let e = settle_for_bridge_error(&dir).expect("immediate hit, no wait");
        assert_eq!(e.message, "no method x() in Y");
        assert!(is_config_phase(e.phase.as_deref()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diagnosed_attach_error_covers_every_code() {
        // Every diagnosis code maps to a concise `attach failed:` statement
        // carrying the required human phrase and the endpoint — and never
        // the raw adapter text.
        let cases = [
            (
                "endpoint-already-attached",
                "h:1",
                "already attached by session",
            ),
            ("endpoint-not-listening", "h:2", "no debug listener found"),
            (
                "endpoint-closed-during-attach",
                "h:3",
                "closed while attaching",
            ),
            ("endpoint-rejected", "h:4", "rejected the connection"),
            ("endpoint-unreachable", "h:5", "could not reach"),
        ];
        for (code, endpoint, phrase) in cases {
            // Preflight owner errors name the session; the mapping helper
            // covers the other four codes.
            if code == "endpoint-already-attached" {
                let owner_identity = unavailable_identity("test owner without state");
                let requested = json!({"host": "h", "port": 1, "pid": Value::Null});
                let err = endpoint_owned_failure(
                    "sess",
                    false,
                    "py",
                    "h",
                    1,
                    &owner_identity,
                    &requested,
                );
                let text = format!("{err:#}");
                assert!(text.starts_with("attach failed:"), "{code}: {text}");
                assert!(text.contains(phrase), "{code}: {text}");
                assert!(
                    text.contains('\'') && text.contains("sess"),
                    "{code}: {text}"
                );
                continue;
            }
            let text = diagnosed_attach_error(code, endpoint);
            assert!(text.starts_with("attach failed:"), "{code}: {text}");
            assert!(text.contains(phrase), "{code}: {text}");
            assert!(text.contains(endpoint), "{code}: {text}");
        }
        // Rejected keeps the calibrated hedge (same truth as diagnosis).
        let rejected = diagnosed_attach_error("endpoint-rejected", "h:4");
        assert!(
            rejected.contains("may already have another debugger client"),
            "{rejected}"
        );
        // Unknown codes degrade to a generic prefixed error, never bare.
        let generic = diagnosed_attach_error("mystery", "h:9");
        assert!(generic.starts_with("attach failed:"), "{generic}");
    }

    #[test]
    fn attach_cause_sanitizes_caps_and_dedupes() {
        // Secrets in the raw adapter text never reach the envelope: URL
        // query tokens, bearer tokens, and --flag values all mask.
        let raw = "attach failed (h:1): ws://h:1/debug?token=hunter2&x=1 \
             Authorization: Bearer ABCDEF --password hunter2";
        let cause = sanitize_cause(raw);
        assert!(!cause.contains("hunter2"), "{cause}");
        assert!(!cause.contains("ABCDEF"), "{cause}");
        assert!(cause.contains("[redacted]"), "{cause}");
        assert!(cause.contains("x=1"), "non-secret values survive: {cause}");
        // Non-secret `key: value` spacing and schemes survive untouched.
        let plain = sanitize_cause("Connection refused (host h port 1)");
        assert_eq!(plain, "Connection refused (host h port 1)");
        // 2KB cap with the shared truncation marker.
        let long = "y".repeat(CAUSE_CAP + 500);
        let capped = sanitize_cause(&long);
        assert!(
            capped.chars().count() <= CAUSE_CAP + 30,
            "{}",
            capped.chars().count()
        );
        assert!(capped.contains("more chars"), "{capped}");
        // A cause identical to the concise error is dropped, never
        // duplicated into both fields.
        let seed = attach_seed("127.0.0.1", 9);
        let requested = json!({"host": "h", "port": 1, "pid": Value::Null});
        let d = attach_diagnosis(Some(false), Some(false), "h:1");
        let code = d["code"].as_str().unwrap().to_string();
        let error = diagnosed_attach_error(&code, "h:1");
        let err = attach_failure(error.clone(), Some(error.clone()), d, &seed, &requested);
        assert!(err.downcast_ref::<BridgeFailure>().unwrap().cause.is_none());
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

    // ---- schema v2 breaking cutover ----

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

    #[test]
    fn schema_gate_rejects_old_markers() {
        // v2 markers pass.
        let dir = tmpdir("gate-v2");
        write_v2_markers(&dir, "py");
        assert!(require_schema_v2(&dir, "gate-v2").is_ok());
        // Missing markers, corrupt files, wrong version: actionable reject.
        for (tag, lang, stops) in [
            ("missing", None, None),
            (
                "corrupt-lang",
                Some("not json"),
                Some(r#"{"schemaVersion":2}"#),
            ),
            (
                "v1-lang",
                Some(r#"{"lang":"py"}"#),
                Some(r#"{"schemaVersion":2}"#),
            ),
            (
                "v1-stops",
                Some(r#"{"lang":"py","schemaVersion":2}"#),
                Some(r#"{}"#),
            ),
            (
                "bad-version",
                Some(r#"{"lang":"py","schemaVersion":1}"#),
                Some(r#"{"schemaVersion":2}"#),
            ),
        ] {
            let d = tmpdir(&format!("gate-{tag}"));
            if let Some(l) = lang {
                std::fs::write(d.join("lang.json"), l).unwrap();
            }
            if let Some(s) = stops {
                std::fs::write(d.join("stops.json"), s).unwrap();
            }
            let err = require_schema_v2(&d, "old").expect_err(tag);
            let text = format!("{err:#}");
            assert!(text.contains("unsupported session 'old'"), "{tag}: {text}");
            assert!(text.contains("close it and recreate"), "{tag}: {text}");
            let _ = std::fs::remove_dir_all(&d);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

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

    #[test]
    fn legacy_reclaim_needs_proven_death() {
        // Proven dead: parseable session.json, numeric nonzero port, daemon
        // silent (port 1 refuses, no recent publish).
        let dir = tmpdir("reclaim-dead");
        std::fs::write(
            dir.join("session.json"),
            r#"{"name":"x","kind":"attach","port":1,"updatedAt":1}"#,
        )
        .unwrap();
        assert!(legacy_proven_dead(&dir), "dead daemon + recorded port");
        // Zero port, corrupt file, missing file: unprovable, never reclaim.
        std::fs::write(
            dir.join("session.json"),
            r#"{"name":"x","kind":"attach","port":0}"#,
        )
        .unwrap();
        assert!(!legacy_proven_dead(&dir));
        std::fs::write(dir.join("session.json"), "not json").unwrap();
        assert!(!legacy_proven_dead(&dir));
        std::fs::remove_file(dir.join("session.json")).unwrap();
        assert!(!legacy_proven_dead(&dir));
        // Live daemon (bound socket): never reclaim.
        let sock = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live = sock.local_addr().unwrap().port();
        std::fs::write(
            dir.join("session.json"),
            format!(r#"{{"name":"x","kind":"attach","port":{live}}}"#),
        )
        .unwrap();
        assert!(!legacy_proven_dead(&dir), "live daemon must survive");
        drop(sock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn intent_endpoint_reads_requested_target_only() {
        // Numeric requestedTarget parses; the old `target` summary fallback
        // (string ports) is gone.
        assert_eq!(
            intent_endpoint(&json!({
                "requestedTarget": {"host": "h", "port": 5678},
                "target": {"host": "other", "port": "1234"},
            })),
            Some(("h".to_string(), 5678))
        );
        assert_eq!(
            intent_endpoint(&json!({"target": {"host": "h", "port": "1234"}})),
            None,
            "target-summary fallback removed"
        );
        assert_eq!(
            intent_endpoint(&json!({"requestedTarget": {"host": "h", "port": "12"}})),
            None,
            "string ports removed"
        );
        assert_eq!(intent_endpoint(&json!({})), None);
    }

    #[test]
    fn legacy_scan_blocks_live_legacy_only() {
        let root = tmpdir("legacy-scan");
        let sock = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live = sock.local_addr().unwrap().port();
        // Live legacy py owner (no schemaVersion anywhere): blocked.
        // fake_owner writes v1 markers, which is exactly the legacy shape.
        fake_owner(&root, "old", "py", "attach", "127.0.0.1", json!(5678), live);
        // Live legacy browser: exempt (CDP multiplexes).
        fake_owner(
            &root,
            "oldtab",
            "browser",
            "attach",
            "127.0.0.1",
            json!(9222),
            live,
        );
        // Live v2 session: not legacy, never listed.
        let v2 = root.join("new");
        std::fs::create_dir_all(&v2).unwrap();
        write_v2_markers(&v2, "py");
        std::fs::write(
            v2.join("session.json"),
            format!(r#"{{"name":"new","kind":"attach","port":{live},"schemaVersion":2}}"#),
        )
        .unwrap();
        // Dead legacy dir: never listed.
        fake_owner(&root, "dead", "py", "attach", "127.0.0.1", json!(9999), 1);
        // Unknown-lang live legacy: fail-closed, listed.
        fake_owner(
            &root,
            "mystery",
            "mystery",
            "attach",
            "h",
            json!(1111),
            live,
        );
        std::fs::write(root.join("mystery").join("lang.json"), "not json").unwrap();
        let found = find_live_legacy_sessions_in(&root, "new");
        assert_eq!(found, vec!["mystery".to_string(), "old".to_string()]);
        assert!(find_live_legacy_sessions_in(&root, "old").contains(&"mystery".to_string()));
        // The block error names the sessions with the legacy-live code and
        // an all-unavailable layered identity (zero pids, no endpoint claim).
        let err = legacy_live_failure("mystery', 'old", &json!({"host": "h", "port": 1}));
        let text = format!("{err:#}");
        assert!(
            text.contains("unsupported live legacy session(s)"),
            "{text}"
        );
        assert!(text.contains("close them first and retry"), "{text}");
        let bf = err.downcast_ref::<BridgeFailure>().expect("typed failure");
        assert_eq!(
            bf.diagnosis.as_ref().unwrap()["code"],
            json!("unsupported-legacy-live")
        );
        let ident = bf.target_identity.as_ref().expect("identity carried");
        assert!(is_layered_identity(ident), "{ident}");
        assert!(
            !ident.to_string().contains("5678"),
            "no endpoint claim: {ident}"
        );
        drop(sock);
        let _ = std::fs::remove_dir_all(&root);
    }
}
