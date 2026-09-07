// See `mod.rs` for the one-way dependency DAG.
use super::attach::{
    attach_diagnosis, attach_endpoint, diagnosed_attach_error, display_endpoint,
    find_live_endpoint_owner_in, find_live_legacy_sessions_in, is_config_phase, is_runtime_phase,
    legacy_proven_dead, listener_present, sanitize_cause,
};
use super::forward::{forward, BridgeFailure};
use super::identity::{cached_target_identity, owner_target_identity, unavailable_identity};
use super::locks::{
    acquire_endpoint_lock, acquire_startup_lock, endpoint_lock_session_name,
    endpoint_locks_dir_for, startup_lock_path, still_holds_endpoint, EndpointClaim,
};
use super::paths::{check_dir_real, check_name, normalize_attach_host, sessions_dir};
use super::sidecar::{cli_markers_v2, write_sidecar_atomic, SpawnSpec, SCHEMA_VERSION};
use crate::bridge;
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

/// Write sidecars and spawn the bridge process. Called only from `spawn`,
/// which removes the session dir if this fails.
pub(crate) fn setup_bridge(
    dir: &std::path::Path,
    spec: &SpawnSpec,
) -> anyhow::Result<std::process::Child> {
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

/// Spawn the bridge daemon and wait for the first stop.
pub fn spawn(name: &str, spec: &SpawnSpec) -> anyhow::Result<Value> {
    spawn_in(&sessions_dir(), name, spec)
}

/// One classification site for every attach setup failure (fast error.json,
/// late-settling error.json, bridge exit, first-forward transport): probe
/// the OS listener now, classify pre-vs-post, derive the concise
/// diagnosis-aligned top-level `error` from the code, keep the raw
/// bridge/transport message as sanitized additive `cause`, and attach the
/// redacted identities. The debuggee is never fabricated — on a failed
/// attach the attempted identity keeps its `unavailable` entries as the
/// bridge/lookup reported them.
pub(crate) fn attach_setup_failure(
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
pub(crate) fn attach_failure(
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
pub(crate) fn attach_config_failure(
    message: String,
    identity: &Value,
    requested: &Value,
) -> anyhow::Error {
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

/// Unexpected internal attach-setup failure (bridge-reported `phase:
/// "runtime"`): the bridge operated successfully and then failed
/// internally (post-handshake crash, degraded state). The sanitized
/// message stays the top-level error verbatim — no endpoint diagnosis,
/// no cause duplication, never endpoint-rejected. The redacted attempted
/// identity and requested endpoint still ride along; they are spawn-time
/// OS observations, never a diagnosis claim.
pub(crate) fn attach_runtime_failure(
    message: String,
    identity: &Value,
    requested: &Value,
) -> anyhow::Error {
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

/// Preflight rejection when a live session already owns the endpoint.
/// Names the confirmed owner and the redacted endpoint; tells the agent to
/// reuse or close it. No bridge is ever spawned, so the first session's
/// target is untouched. No adapter ran, so there is no raw `cause` — the
/// concise `attach failed:` error plus the diagnosis carry everything.
/// `owner_identity` is the owner's layered `{debuggee, endpoint, adapter}`
/// identity (see `owner_target_identity`) — never a raw pid claim, which
/// would misdirect a kill at the adapter on wrapped servers.
pub(crate) fn endpoint_owned_failure(
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
pub(crate) fn legacy_live_failure(names: &str, requested: &Value) -> anyhow::Error {
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
pub(crate) fn spawn_in(
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
            // endpoint diagnosis; runtime phase (unexpected internal
            // failure after a successful operation) keeps the truthful
            // internal message top-level with no endpoint diagnosis and
            // is never called endpoint-rejected; transport takes the
            // OS-observed listener state (pre-attach vs now) with the raw
            // text as sanitized cause. Every v2 bridge writes
            // schemaVersion + phase.
            if let Some((host, port)) = &endpoint {
                if is_config_phase(failure.phase.as_deref()) {
                    return Err(attach_config_failure(
                        failure.message,
                        &spec.target_identity,
                        &spec.requested,
                    ));
                }
                if is_runtime_phase(failure.phase.as_deref()) {
                    return Err(attach_runtime_failure(
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
                        // message; runtime keeps the truthful internal
                        // message, never endpoint-rejected; transport
                        // takes a fresh probe).
                        if let Some((host, port)) = &endpoint {
                            if is_config_phase(failure.phase.as_deref()) {
                                return Err(attach_config_failure(
                                    failure.message,
                                    &spec.target_identity,
                                    &spec.requested,
                                ));
                            }
                            if is_runtime_phase(failure.phase.as_deref()) {
                                return Err(attach_runtime_failure(
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

/// Bridge setup-failure truth with a bounded settle: error.json can trail
/// the dead session.json by bridge-cleanup lag (the full-suite test11
/// race). Immediate check first — no added latency when the files land in
/// order — then a bounded re-poll at the wait-loop cadence. Only entered
/// after a failed first forward, so the success path never sleeps.
/// Returns the bridge failure when the file lands in budget.
pub(crate) fn settle_for_bridge_error(dir: &std::path::Path) -> Option<BridgeSetupError> {
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
pub(crate) fn logpoints_only_intent(stops: &Value) -> bool {
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
pub(crate) fn read_logs_file(dir: &std::path::Path, tail: usize) -> Option<Value> {
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
/// "phase":"transport"|"config"|"runtime"}`. Every v2 bridge writes all
/// three keys on every early path. Strict action: a present parseable
/// `error.json` that carries an `error` string but the wrong
/// `schemaVersion` or a missing/invalid `phase` reads as corrupt
/// (`corrupt: true` with the exact actionable message — internal, never
/// transport-diagnosed). Absent, unparseable, or error-less files keep
/// the transport fallback (`corrupt: false`, default message, `phase:
/// None`).
#[derive(Debug)]
pub(crate) struct BridgeSetupError {
    message: String,
    phase: Option<String>,
    corrupt: bool,
}

pub(crate) fn read_bridge_error(dir: &std::path::Path) -> BridgeSetupError {
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
    let phase_ok = matches!(
        phase.as_deref(),
        Some("transport") | Some("config") | Some("runtime")
    );
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

pub(crate) fn read_log_tail(dir: &std::path::Path) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::attach::CAUSE_CAP;
    use crate::session::identity::{attach_seed, is_layered_identity, IDENTITY_TOTAL_CAP};
    use std::path::PathBuf;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-debugger-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
        assert!(!is_runtime_phase(e.phase.as_deref()));
        // Runtime phase with schemaVersion 2: truthful internal error,
        // never endpoint-diagnosed, never endpoint-rejected.
        std::fs::write(
            dir.join("error.json"),
            r#"{"schemaVersion":2,"error":"internal: KeyError('value')","phase":"runtime"}"#,
        )
        .unwrap();
        let e = read_bridge_error(&dir);
        assert!(!e.corrupt);
        assert!(is_runtime_phase(e.phase.as_deref()));
        assert!(!is_config_phase(e.phase.as_deref()));
        // Present + parseable but missing/bad version/phase: exact corrupt
        // message (internal, never transport-diagnosed). Unknown phases
        // stay corrupt — only transport/config/runtime are valid.
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
                r#"{"schemaVersion":2,"error":"x","phase":"internal"}"#,
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
    fn attach_runtime_phase_stays_truthful_internal_error() {
        // Runtime phase: the bridge operated successfully then failed
        // internally. The message stays top-level verbatim (never
        // rewritten to `attach failed:`, never endpoint-rejected), with
        // no diagnosis and no cause duplication — but the redacted
        // identities still ride along for the envelope.
        let seed = attach_seed("127.0.0.1", 9);
        let requested = json!({"host": "127.0.0.1", "port": 9, "pid": Value::Null});
        let msg = "internal: KeyError('value')";
        let err = attach_runtime_failure(msg.to_string(), &seed, &requested);
        let text = format!("{err:#}");
        assert_eq!(text, msg);
        assert!(
            !text.contains("attach failed") && !text.contains("rejected"),
            "runtime errors are never endpoint-diagnosed: {text}"
        );
        let bf = err.downcast_ref::<BridgeFailure>().expect("typed failure");
        assert!(bf.diagnosis.is_none(), "no misleading endpoint diagnosis");
        assert!(bf.cause.is_none(), "no cause duplication");
        assert!(bf.target_identity.is_some());
        assert_eq!(bf.requested_target.as_ref().unwrap()["port"], json!(9));
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
