// See `mod.rs` for the one-way dependency DAG.
use super::paths::startup_nonce;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

/// Sibling lockfile guarding one name's startup window (pre-session.json).
/// Created atomically before the stale-dir clear; held until the wait loop
/// resolves (success or failure) via RAII release. Only our own nonce is
/// ever removed, and only a provably stale lock is stolen — never a live
/// starter's — so stale dirs stay reusable and retries never wedge.
pub(crate) fn startup_lock_path(sessions_root: &std::path::Path, name: &str) -> PathBuf {
    sessions_root.join(format!("{name}.lock"))
}

/// A lock is stale only when its mtime is provably older than the bound.
/// Unreadable clocks fail closed (treat as live) — a retry costs one wait,
/// a wrongful steal costs a live startup its dir.
pub(crate) const STARTUP_LOCK_STALE: Duration = Duration::from_secs(120);

/// Stale verdict over an already-read mtime: true only when provably at
/// least `STARTUP_LOCK_STALE` old. Single source for the 120s policy —
/// the acquire path and the close gate both judge snapshotted timestamps
/// through here, so a vanished lock reads as reclaimable (not live) while
/// undatable/future ones fail closed.
pub(crate) fn stale_startup_mtime(mtime: std::time::SystemTime) -> bool {
    std::time::SystemTime::now()
        .duration_since(mtime)
        .ok()
        .map(|age| age >= STARTUP_LOCK_STALE)
        .unwrap_or(false)
}

pub(crate) struct StartupGuard {
    path: PathBuf,
    nonce: String,
}

impl std::fmt::Debug for StartupGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Nonce never prints: lock nonces are internal coordination values.
        write!(f, "StartupGuard({})", self.path.display())
    }
}

impl Drop for StartupGuard {
    fn drop(&mut self) {
        release_startup_lock(&self.path, &self.nonce);
    }
}

pub(crate) fn startup_lock_is_mine(path: &std::path::Path, nonce: &str) -> bool {
    std::fs::read_to_string(path)
        .map(|c| c == nonce)
        .unwrap_or(false)
}

pub(crate) fn release_startup_lock(path: &std::path::Path, nonce: &str) {
    if startup_lock_is_mine(path, nonce) {
        let _ = std::fs::remove_file(path);
    }
}

/// Atomically claim the startup lock (single steal retry for a stale lock).
/// Live locks (fresh or undatable) bail with a retryable "starting" error.
pub(crate) fn acquire_startup_lock(path: &std::path::Path) -> anyhow::Result<StartupGuard> {
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
    // Snapshot the exact bytes with the mtime: a lock reclaimed under us
    // reads as gone (path free — one atomic claim attempt decides), an
    // undatable one as live (fail closed).
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return claim_stale_startup_lock(path);
        }
        Err(_) => {
            let holder = lock_holder_name(path);
            anyhow::bail!(
                "session '{holder}' is starting (concurrent start in progress; retry shortly)"
            )
        }
    };
    let mtime = match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return claim_stale_startup_lock(path);
        }
        Err(_) => {
            let holder = lock_holder_name(path);
            anyhow::bail!(
                "session '{holder}' is starting (concurrent start in progress; retry shortly)"
            )
        }
    };
    // Steal only a provably stale record via the verified quarantine
    // transfer: a fresh or replaced rival lock never matches, so it can
    // never be deleted — a bare remove_file here could (two retriers, one
    // rival claim between verdict and unlink). Single attempt, fail
    // closed; the atomic create_new below still admits exactly one owner.
    if !stale_startup_mtime(mtime) {
        let holder = lock_holder_name(path);
        anyhow::bail!(
            "session '{holder}' is starting (concurrent start in progress; retry shortly)"
        );
    }
    if !quarantine_verified(path, &raw) {
        let holder = lock_holder_name(path);
        anyhow::bail!(
            "session '{holder}' is starting (concurrent start in progress; retry shortly)"
        );
    }
    claim_stale_startup_lock(path)
}

/// Session name from a startup lock path (the file stem); best-effort for
/// retryable "starting" errors only, never a claim decision.
pub(crate) fn lock_holder_name(path: &std::path::Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Single atomic claim attempt after a verified stale detach: success owns
/// the name; any failure (a rival won the slot meanwhile) reads as a live
/// start — exactly one owner ever emerges from concurrent steals.
pub(crate) fn claim_stale_startup_lock(path: &std::path::Path) -> anyhow::Result<StartupGuard> {
    let nonce = startup_nonce();
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| {
            anyhow::anyhow!("session is starting (concurrent start in progress; retry shortly)")
        })?;
    use std::io::Write as _;
    f.write_all(nonce.as_bytes())
        .map_err(|e| anyhow::anyhow!("cannot write startup lock: {e}"))?;
    Ok(StartupGuard {
        path: path.to_path_buf(),
        nonce,
    })
}

// ---- endpoint-scoped atomic reservation ----

/// Locks live under `~/.agent-debugger/endpoint-locks/`, one file per
/// normalized language/host/port, so two different endpoints never block
/// each other. The lock is held from preflight through bridge handshake
/// until session ownership is published; afterwards the published session
/// (found by the scan above) owns the endpoint.
pub(crate) fn endpoint_locks_dir() -> anyhow::Result<PathBuf> {
    Ok(super::paths::agent_home()?
        .join(".agent-debugger")
        .join("endpoint-locks"))
}

/// Locks dir scoped to a sessions root (unit tests pass a tmpdir so the
/// reservation is exercised without touching the real namespace).
/// Production roots (`~/.agent-debugger/sessions`) map to the canonical
/// `~/.agent-debugger/endpoint-locks`.
pub(crate) fn endpoint_locks_dir_for(sessions_root: &std::path::Path) -> anyhow::Result<PathBuf> {
    match sessions_root.parent() {
        Some(p) => Ok(p.join("endpoint-locks")),
        None => endpoint_locks_dir(),
    }
}

pub(crate) fn endpoint_lock_path(
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
pub(crate) const ENDPOINT_LOCK_STALE: Duration = Duration::from_secs(7200);
pub(crate) const ENDPOINT_LOCK_GRACE: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub(crate) struct EndpointGuard {
    pub(crate) path: PathBuf,
    pub(crate) nonce: String,
}

impl Drop for EndpointGuard {
    fn drop(&mut self) {
        release_endpoint_lock(&self.path, &self.nonce);
    }
}

/// Only our own nonce is ever removed (parsed out of the JSON record —
/// never a blind delete), so a live starter's lock is never stolen on
/// release.
pub(crate) fn release_endpoint_lock(path: &std::path::Path, nonce: &str) {
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
pub(crate) struct EndpointBusy {
    pub(crate) session: String,
    pub(crate) published: bool,
}

pub(crate) fn endpoint_lock_session_name(path: &std::path::Path) -> Option<String> {
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
pub(crate) enum Liveness {
    Alive,
    Dead,
    Unknown,
}

/// Best-effort holder liveness. Linux reads /proc directly (no
/// subprocess); elsewhere a bounded `ps` probe answers.
#[cfg(target_os = "linux")]
pub(crate) fn holder_alive(pid: u32) -> Liveness {
    let p = std::path::PathBuf::from(format!("/proc/{pid}"));
    match std::fs::symlink_metadata(&p) {
        Ok(m) if m.file_type().is_dir() => Liveness::Alive,
        Ok(_) => Liveness::Unknown, // exists but unreadable shape: no verdict
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Liveness::Dead,
        Err(_) => Liveness::Unknown,
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn holder_alive(pid: u32) -> Liveness {
    ps_has_pid(pid)
}

/// Dedicated bounded `ps` status probe (non-Linux only): the exit status —
/// not mere output presence — is the signal. `ps -p <dead>` exits nonzero
/// (dead); spawn failure or timeout means the probe itself failed
/// (unknown), never "dead". No shell, fixed argv, 5s hard bound.
#[cfg(not(target_os = "linux"))]
pub(crate) fn ps_has_pid(pid: u32) -> Liveness {
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
pub(crate) fn lock_snapshot_is_stale(raw: &str, mtime: std::time::SystemTime) -> bool {
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
pub(crate) fn reclaim_stale_lock(path: &std::path::Path) -> bool {
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
pub(crate) fn quarantine_verified(path: &std::path::Path, expected: &str) -> bool {
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
pub(crate) fn detach_to_quarantine(path: &std::path::Path) -> std::io::Result<PathBuf> {
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
pub(crate) fn reconcile_quarantine(
    path: &std::path::Path,
    q: &std::path::Path,
    expected: &str,
) -> bool {
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
pub(crate) fn restore_quarantine_bytes(path: &std::path::Path, q: &std::path::Path) -> bool {
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
pub(crate) fn still_holds_endpoint(guard: &EndpointGuard) -> bool {
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
pub(crate) enum EndpointClaim {
    Held(EndpointGuard),
    Busy(EndpointBusy),
}

/// Claim the endpoint lock (single reclaim retry for a stale lock). A live
/// lock reports the holder — the caller turns it into an
/// endpoint-already-attached error, never a wait.
pub(crate) fn acquire_endpoint_lock(
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
    fn endpoint_locks_dir_fails_closed_without_home() {
        // Same fail-closed contract as the session roots: no /tmp fallback.
        crate::session::with_home(None, || {
            let err = format!("{:#}", endpoint_locks_dir().unwrap_err());
            assert!(err.contains("HOME is unset or empty"), "{err}");
            // Scoped roots never need HOME (sibling mapping only).
            assert_eq!(
                endpoint_locks_dir_for(std::path::Path::new("/x/sessions")).unwrap(),
                std::path::Path::new("/x/endpoint-locks")
            );
        });
        crate::session::with_home(Some(std::path::Path::new("/tmp/probe-home")), || {
            assert_eq!(
                endpoint_locks_dir().unwrap(),
                std::path::Path::new("/tmp/probe-home/.agent-debugger/endpoint-locks")
            );
            // Scoped roots map to their sibling locks dir without HOME.
            assert_eq!(
                endpoint_locks_dir_for(std::path::Path::new("/tmp/r/sessions")).unwrap(),
                std::path::Path::new("/tmp/r/endpoint-locks")
            );
        });
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
    fn startup_lock_two_contenders_exactly_one_owner() {
        // Two starters race one stale-seeded lock: exactly one emerges
        // holding it (atomic create_new after the verified detach), the
        // other bails retryably. The winner's fresh nonce survives in the
        // file — a bare remove-then-claim could have deleted it.
        // Outcome assertion is deterministic (count == 1); only scheduling
        // varies. Guards stay held through the count.
        let base = tmpdir("startup-steal-race");
        let lock = startup_lock_path(&base, "demo");
        std::fs::write(&lock, "crashed-starter").unwrap();
        backdate(&lock, STARTUP_LOCK_STALE + Duration::from_secs(5));
        std::thread::scope(|s| {
            let h1 = s.spawn(|| acquire_startup_lock(&lock).ok());
            let h2 = s.spawn(|| acquire_startup_lock(&lock).ok());
            let results = [h1.join().unwrap(), h2.join().unwrap()];
            assert_eq!(
                results.iter().filter(|g| g.is_some()).count(),
                1,
                "exactly one steal racer must win"
            );
            // The survivor's nonce is the file content (fresh, intact).
            let winner = results.into_iter().flatten().next().unwrap();
            assert_eq!(
                std::fs::read_to_string(&lock).unwrap(),
                winner.nonce,
                "fresh winner nonce survives"
            );
        });
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn startup_lock_fresh_and_replaced_records_survive() {
        // A live lock is never touched; a stale verdict overtaken by a
        // fresh replacement bails with the new record intact.
        let base = tmpdir("startup-steal-fresh");
        let lock = startup_lock_path(&base, "demo");
        std::fs::write(&lock, "live-starter").unwrap();
        let err = acquire_startup_lock(&lock).unwrap_err();
        assert!(format!("{err:#}").contains("is starting"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&lock).unwrap(), "live-starter");
        // Stale bytes, then a fresh rival claims before our verdict runs:
        // the quarantine re-verify mismatches — hands off, rival intact.
        std::fs::write(&lock, "stale-bytes").unwrap();
        backdate(&lock, STARTUP_LOCK_STALE + Duration::from_secs(5));
        std::fs::write(&lock, "rival-fresh-nonce").unwrap();
        let err = acquire_startup_lock(&lock).unwrap_err();
        assert!(format!("{err:#}").contains("is starting"), "{err:#}");
        assert_eq!(
            std::fs::read_to_string(&lock).unwrap(),
            "rival-fresh-nonce",
            "replaced rival lock must survive the steal"
        );
        let _ = std::fs::remove_dir_all(&base);
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
            let held = results.iter().filter(|g| g.is_some()).count();
            assert!(held >= 1, "at least one stale-reclaim racer must claim");
            assert_eq!(
                results
                    .iter()
                    .flatten()
                    .filter(|g| still_holds_endpoint(g))
                    .count(),
                1,
                "exactly one stale-reclaim racer must remain the owner"
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
}
