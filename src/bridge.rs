//! Embedded debug bridges: first-use provisioning.
//!
//! Java ships as source inside this binary via `include_str!` and is compiled
//! once with the host `javac`. Python ships the same way as a script (no
//! compile); its interpreter is an isolated venv with debugpy, created on
//! first use. Node ships the same way as a script speaking raw CDP; its only
//! dependency is the tiny `ws` package, installed once into an isolated dir.
//! All bridges run as per-session daemons (see `session`).

use std::path::PathBuf;
use std::time::Duration;

use crate::session::agent_home;

/// Java bridge sources, embedded at compile time. The bridge is split into
/// one file per concern (same default package); all are written to the
/// adapter dir and compiled together. `JdiBridge` stays the entry point.
const JAVA_SOURCES: &[(&str, &str)] = &[
    (
        "JdiBridge.java",
        include_str!("../bridge/java/src/JdiBridge.java"),
    ),
    (
        "BridgeModel.java",
        include_str!("../bridge/java/src/BridgeModel.java"),
    ),
    (
        "BridgeCli.java",
        include_str!("../bridge/java/src/BridgeCli.java"),
    ),
    (
        "BridgeConn.java",
        include_str!("../bridge/java/src/BridgeConn.java"),
    ),
    (
        "BridgeSnapshot.java",
        include_str!("../bridge/java/src/BridgeSnapshot.java"),
    ),
    (
        "BridgeSession.java",
        include_str!("../bridge/java/src/BridgeSession.java"),
    ),
    (
        "BridgeProto.java",
        include_str!("../bridge/java/src/BridgeProto.java"),
    ),
    (
        "BridgeEval.java",
        include_str!("../bridge/java/src/BridgeEval.java"),
    ),
];
pub const BRIDGE_MAIN_CLASS: &str = "JdiBridge";

/// One compiled class per source file, so a damaged classes dir (marker
/// present, some outputs deleted) still triggers a recompile. Update when
/// adding new top-level classes to the Java bridge.
const JAVA_CLASSES: &[&str] = &[
    "JdiBridge",
    "Config",
    "BridgeCli",
    "BridgeConn",
    "BridgeSnapshot",
    "BridgeSession",
    "BridgeProto",
    "BridgeEval",
    "StreamGobbler",
];

const PYBRIDGE_SOURCE: &str = include_str!("../bridge/py/src/pybridge.py");

const NODEBRIDGE_SOURCE: &str = include_str!("../bridge/node/src/nodebridge.js");

const BROWSERBRIDGE_SOURCE: &str = include_str!("../bridge/browser/src/browserbridge.js");

/// Shared JS wire core, required relatively by both CDP bridges.
/// Single source in the repo; provisioned next to each bridge on disk.
const JS_SHARED: &[(&str, &str)] = &[
    ("cdp_conn.js", include_str!("../bridge/js/cdp_conn.js")),
    ("framing.js", include_str!("../bridge/js/framing.js")),
];

/// True when `path` already holds exactly `source` (stale-write-if-changed
/// seam: every `ensure_*` below rewrites only on mismatch, so a fresh
/// adapter dir is never touched and a stale/damaged one always converges).
/// Callers lstat `path` (refusing planted links) BEFORE this read — the
/// read itself would follow a swapped-in link.
fn file_matches(path: &std::path::Path, source: &str) -> bool {
    std::fs::read_to_string(path)
        .map(|existing| existing == source)
        .unwrap_or(false)
}

// ---- provisioning guards (M3) ----

/// How long a contended provision waits for a live holder before bailing
/// retryably (`adapter provisioning in progress; retry shortly`): covers
/// the 180s pip/npm timeout below plus javac margin. Never spins forever,
/// never steals.
pub(crate) const PROVISION_LOCK_WAIT: Duration = Duration::from_secs(200);

/// Poll interval while a live holder provisions: prompt entry without hot
/// spinning. Pacing only — exclusion never depends on it.
pub(crate) const PROVISION_LOCK_POLL: Duration = Duration::from_millis(20);

/// Owns the open lock-file handle: the kernel exclusive lock is held as
/// long as this guard lives and releases on handle close — guard drop or
/// whole-process death alike (crash-safe, no mtime/steal protocol). The
/// lock file is never deleted, replaced, or truncated, so every holder
/// rendezvous on the same inode.
#[derive(Debug)]
pub(crate) struct ProvisionGuard {
    _file: std::fs::File,
}

pub(crate) fn provision_lock_path(adapters: &std::path::Path, adapter: &str) -> PathBuf {
    adapters.join(format!("{adapter}.lock"))
}

/// Claim one adapter's provisioning lock, waiting briefly for a live
/// holder. `WouldBlock` polls to the bound, then bails retryably; any
/// other lock error, a symlink, or a non-regular file bails as a
/// provisioning error. The file is opened read+write+create (never
/// truncate/exclusive/create_new) and is never deleted — first creator
/// and concurrent openers land on one inode. `wait`/`poll` are injectable
/// so tests use short bounds while production uses the 200s/20ms pair.
pub(crate) fn acquire_provision_lock(
    path: &std::path::Path,
    wait: Duration,
    poll: Duration,
) -> anyhow::Result<ProvisionGuard> {
    refuse_provision_link(path, "provisioning lock")?;
    if let Ok(m) = std::fs::symlink_metadata(path) {
        if !m.file_type().is_file() {
            anyhow::bail!(
                "provisioning lock must be a regular file: {}",
                path.display()
            );
        }
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
        .map_err(|e| anyhow::anyhow!("cannot open provisioning lock {}: {e}", path.display()))?;
    // Re-verify the opened handle (closes the check-then-open swap window
    // short of a swap-back race, accepted as best-effort — same stance as
    // the session-dir and breaks-lock checks).
    if !file
        .metadata()
        .map(|m| m.file_type().is_file())
        .unwrap_or(false)
    {
        anyhow::bail!(
            "provisioning lock must be a regular file: {}",
            path.display()
        );
    }
    let deadline = std::time::Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(ProvisionGuard { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => {
                if std::time::Instant::now() >= deadline {
                    anyhow::bail!("adapter provisioning in progress; retry shortly");
                }
                std::thread::sleep(poll);
            }
            Err(std::fs::TryLockError::Error(e)) => {
                anyhow::bail!("cannot lock {}: {e}", path.display())
            }
        }
    }
}

/// Refuse a planted symlink at `path` (absent is fine — the caller
/// creates). Every provisioning write/lock path lstates before use so a
/// swapped-in link is never followed by create/write/rename.
fn refuse_provision_link(path: &std::path::Path, what: &str) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            anyhow::bail!("{what} must not be a symlink: {}", path.display())
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => anyhow::bail!("cannot stat {what} {}: {e}", path.display()),
    }
}

/// Leaf check for a provisioning path: symlinks refuse; an existing
/// adapter dir must be a real dir, an existing bridge dest a regular file
/// (absent is fine — the caller creates). Runs BEFORE any
/// `file_matches`/`read_to_string`, which would follow a link.
fn check_provision_path(path: &std::path::Path, want_dir: bool, what: &str) -> anyhow::Result<()> {
    refuse_provision_link(path, what)?;
    match std::fs::symlink_metadata(path) {
        Ok(m) if want_dir && !m.file_type().is_dir() => {
            anyhow::bail!("{what} must be a real directory: {}", path.display())
        }
        Ok(m) if !want_dir && !m.file_type().is_file() => {
            anyhow::bail!("{what} must be a regular file: {}", path.display())
        }
        _ => Ok(()),
    }
}

/// Create `dir` (plus missing ancestors) after refusing a planted symlink
/// on the managed path. Trust boundary: `anchor` itself is trusted (the
/// user's own HOME root or a test-owned dir); every EXISTING component
/// strictly below it down to `dir` must not be a link. Components at/above
/// the anchor (system prefixes like /tmp or /var, legitimately links on
/// macOS) are never judged. Absent components are created; a swap between
/// check and create is accepted as best-effort (same stance as the
/// session-dir checks). Callers pass `(dir, dir)` for a leaf-only check.
fn ensure_real_dir_all(anchor: &std::path::Path, dir: &std::path::Path) -> anyhow::Result<()> {
    let mut chain: Vec<&std::path::Path> = vec![dir];
    let mut cur = dir;
    loop {
        if cur == anchor {
            break;
        }
        match cur.parent() {
            Some(p) if p != cur => {
                cur = p;
                chain.push(cur);
            }
            _ => break, // filesystem root without meeting anchor
        }
    }
    if chain.last() == Some(&anchor) {
        // Managed pair: verify top-down from below the anchor, stopping
        // at the first absent component (create covers the rest).
        for p in chain[..chain.len() - 1].iter().rev() {
            refuse_provision_link(p, "provisioning dir")?;
            if std::fs::symlink_metadata(p).is_err() {
                break;
            }
        }
    } else {
        // Test seam (dir outside the anchor): leaf only.
        refuse_provision_link(dir, "provisioning dir")?;
    }
    std::fs::create_dir_all(dir)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", dir.to_string_lossy()))?;
    Ok(())
}

/// Provisioning mkdir for canonical flows (anchor = the fail-closed HOME)
/// and test seams (unresolvable HOME degrades to a leaf-only check — the
/// test owns its dir outright, so there is no ancestor to protect).
fn ensure_provision_dir(dir: &std::path::Path) -> anyhow::Result<()> {
    let anchor = agent_home().unwrap_or_else(|_| PathBuf::from("/nonexistent-agent-home"));
    ensure_real_dir_all(&anchor, dir)
}

/// Unique tmp name in `dir` for one atomic bridge write (never a fixed
/// name, so concurrent holders never share it).
fn provision_tmp_name(dest: &std::path::Path) -> PathBuf {
    static CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = CTR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    dest.with_extension(format!("tmp.{}.{n}", std::process::id()))
}

/// Write `source` to `dest` atomically: exclusive create-new tmp in the
/// same dir + rename. The caller lstates `dest` first and holds the
/// adapter lock; only this call's own tmp is ever removed (never the dest,
/// never a lock inode).
fn atomic_write_provisioned(dest: &std::path::Path, source: &str) -> anyhow::Result<()> {
    let tmp = provision_tmp_name(dest);
    let written = (|| -> anyhow::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", tmp.to_string_lossy()))?;
        use std::io::Write as _;
        f.write_all(source.as_bytes())
            .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", tmp.to_string_lossy()))?;
        f.sync_all()
            .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", tmp.to_string_lossy()))?;
        drop(f);
        std::fs::rename(&tmp, dest)
            .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", dest.to_string_lossy()))?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Write the shared JS core into a bridge dir when changed. Runs under the
/// caller's adapter lock (node.lock for the node dir, browser.lock for the
/// browser dir — see the wrappers below); this helper takes no lock
/// itself. Symlink-guarded + atomic like every bridge write.
fn ensure_js_shared(dir: &std::path::Path) -> anyhow::Result<bool> {
    check_provision_path(dir, true, "bridge dir")?;
    let mut changed = false;
    for (name, source) in JS_SHARED {
        let dest = dir.join(name);
        check_provision_path(&dest, false, "bridge file")?;
        if !file_matches(&dest, source) {
            ensure_provision_dir(dir)?;
            atomic_write_provisioned(&dest, source)?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Prepend the isolated `ws` dir to `NODE_PATH` so any user value keeps
/// working (pure seam of the browser arm in `session::spawn_lifecycle`).
pub(crate) fn prepend_node_path(ws_dir: &str, existing: Option<&str>) -> String {
    match existing {
        Some(v) if !v.is_empty() => format!("{ws_dir}:{v}"),
        _ => ws_dir.to_string(),
    }
}

/// Adapter roots live under the fail-closed HOME (no `/tmp` fallback):
/// `<agent_home>/.agent-debugger/adapters`.
fn adapters_dir() -> anyhow::Result<PathBuf> {
    Ok(agent_home()?.join(".agent-debugger").join("adapters"))
}

pub fn adapter_dir() -> anyhow::Result<PathBuf> {
    Ok(adapters_dir()?.join("java"))
}

/// Ensure the bridge is compiled; return the classes dir for `java -cp`.
/// Single-flights on java.lock; the locked inner assumes it is held.
pub fn ensure_compiled() -> anyhow::Result<PathBuf> {
    let adapters = adapters_dir()?;
    ensure_provision_dir(&adapters)?;
    let _guard = acquire_provision_lock(
        &provision_lock_path(&adapters, "java"),
        PROVISION_LOCK_WAIT,
        PROVISION_LOCK_POLL,
    )?;
    ensure_compiled_locked()
}

fn ensure_compiled_locked() -> anyhow::Result<PathBuf> {
    let dir = adapter_dir()?;
    check_provision_path(&dir, true, "adapter dir")?;
    let classes = dir.join("classes");
    let marker = classes.join("JdiBridge.class");

    // Any changed source (or a missing marker) recompiles the whole set:
    // same package, so one javac invocation covers all files. Class
    // outputs are checked individually: the marker alone cannot prove a
    // damaged classes dir complete.
    let mut stale = !marker.exists();
    if !stale {
        stale = JAVA_CLASSES
            .iter()
            .any(|c| !classes.join(format!("{c}.class")).exists());
    }
    for (name, source) in JAVA_SOURCES {
        let dest = dir.join(name);
        // Lstat before the stale read (file_matches would follow a link).
        check_provision_path(&dest, false, "bridge file")?;
        if !file_matches(&dest, source) {
            stale = true;
        }
    }
    if stale {
        // `classes/` is compiler-managed output: guard the dir itself
        // (symlink refusal + ancestors), not each generated file.
        ensure_provision_dir(&classes)?;
        let mut sources = Vec::with_capacity(JAVA_SOURCES.len());
        for (name, source) in JAVA_SOURCES {
            let dest = dir.join(name);
            atomic_write_provisioned(&dest, source)?;
            sources.push(dest);
        }
        let out = std::process::Command::new("javac")
            .arg("-d")
            .arg(&classes)
            .args(&sources)
            .output()
            .map_err(|e| {
                anyhow::anyhow!(
                    "javac not found or failed to start: {e} (JDK required for the Java adapter)"
                )
            })?;
        if !out.status.success() {
            let log = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!("javac failed:\n{log}");
        }
    }
    Ok(classes)
}

pub fn python_dir() -> anyhow::Result<PathBuf> {
    Ok(adapters_dir()?.join("python"))
}

fn venv_python() -> anyhow::Result<PathBuf> {
    Ok(python_dir()?.join("venv").join("bin").join("python"))
}

/// Program + args used to install debugpy into the isolated venv.
/// Always the venv interpreter itself (`<venv>/bin/python -m pip ...`);
/// a `<venv>/bin/pip` path would resolve through the interpreter file
/// (`<venv>/bin/python/bin/pip`, ENOTDIR on fresh HOME).
fn debugpy_install_command() -> anyhow::Result<(PathBuf, Vec<String>)> {
    let venv = venv_python()?;
    Ok((
        venv,
        vec![
            "-m".to_string(),
            "pip".to_string(),
            "install".to_string(),
            // Pinned: debugpy 1.8.22 loses a breakpoint stop racing
            // another thread's stop on Python 3.12 (thread B parks, thread
            // A's hit never arrives — live test_16 fails 6/6 on CI while
            // 1.8.21 delivers both). Floating `debugpy` silently adopts
            // such regressions on every fresh runner provision.
            "debugpy==1.8.21".to_string(),
        ],
    ))
}

fn has_debugpy(interp: &str) -> bool {
    std::process::Command::new(interp)
        .args(["-c", "import debugpy"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Resolve a Python interpreter with debugpy: isolated venv first (created on
/// first use), then system python3. Returns the interpreter path.
/// Single-flights venv creation + debugpy install on python.lock; pure
/// probes stay outside the lock, and the locked section rechecks them so a
/// waiter never repeats a finished provision.
pub fn ensure_py() -> anyhow::Result<String> {
    if let Some(hit) = ensure_py_fast()? {
        return Ok(hit);
    }
    let adapters = adapters_dir()?;
    ensure_provision_dir(&adapters)?;
    let _guard = acquire_provision_lock(
        &provision_lock_path(&adapters, "python"),
        PROVISION_LOCK_WAIT,
        PROVISION_LOCK_POLL,
    )?;
    ensure_py_locked()
}

/// Probe-only fast path (no writes, no lock): venv interpreter, then
/// system python3 — both with debugpy importable.
fn ensure_py_fast() -> anyhow::Result<Option<String>> {
    let venv_str = venv_python()?.to_string_lossy().to_string();
    if has_debugpy(&venv_str) {
        return Ok(Some(venv_str));
    }
    if has_debugpy("python3") {
        return Ok(Some("python3".to_string()));
    }
    Ok(None)
}

fn ensure_py_locked() -> anyhow::Result<String> {
    // Another holder may have finished while we waited: recheck before
    // creating anything (and guard the adapter dir itself).
    if let Some(hit) = ensure_py_fast()? {
        return Ok(hit);
    }
    let dir = python_dir()?;
    check_provision_path(&dir, true, "adapter dir")?;
    let venv_str = venv_python()?.to_string_lossy().to_string();
    // First use: isolated venv so we never touch the system Python (PEP 668).
    // `venv/` is tool-managed output: guard the dir, not each generated file.
    let venv_dir = python_dir()?.join("venv");
    ensure_provision_dir(&venv_dir)?;
    let out = std::process::Command::new("python3")
        .args(["-m", "venv"])
        .arg(&venv_dir)
        .output()
        .map_err(|e| {
            anyhow::anyhow!("python3 not found ({e}); install Python 3.8+ for the Python adapter")
        })?;
    if !out.status.success() {
        anyhow::bail!(
            "could not create isolated Python env: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let (pip_program, pip_args) = debugpy_install_command()?;
    let pip = run_with_timeout(pip_program, &pip_args, std::time::Duration::from_secs(180))
        .map_err(|e| anyhow::anyhow!("pip failed to start: {e}"))?;
    if !pip.status.success() || !has_debugpy(&venv_str) {
        anyhow::bail!(
            "could not install debugpy (network needed once). Try manually:\n  {} -m pip install debugpy",
            venv_str
        );
    }
    Ok(venv_str)
}

/// Run a command with a hard timeout (agents must never hang forever on
/// network operations like pip). Returns the Output on time, else bails.
fn run_with_timeout(
    program: std::path::PathBuf,
    args: &[String],
    timeout: std::time::Duration,
) -> anyhow::Result<std::process::Output> {
    let (tx, rx) = std::sync::mpsc::channel();
    let program_dbg = program.to_string_lossy().to_string();
    let child = std::process::Command::new(&program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("{program_dbg} failed to start: {e}"))?;
    let child_id = child.id();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(e)) => Err(anyhow::anyhow!("{program_dbg} failed: {e}")),
        Err(_) => {
            // Best-effort kill so a timed-out pip/npm never orphans: the
            // waiter thread still reaps the child whenever it exits.
            kill_child(child_id);
            Err(anyhow::anyhow!(
                "{program_dbg} timed out after {}s",
                timeout.as_secs()
            ))
        }
    }
}

/// Best-effort kill of a timed-out provision child by pid.
/// Unix: `kill` (SIGTERM). Windows: `taskkill /PID /F` (`kill` does not
/// exist there — the old code silently leaked the child).
fn kill_child(pid: u32) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .output();
    }
}

/// Write the embedded pybridge when it changed; return its path.
/// Single-flights on python.lock (same scope as `ensure_py`).
pub fn ensure_pybridge() -> anyhow::Result<PathBuf> {
    let adapters = adapters_dir()?;
    ensure_provision_dir(&adapters)?;
    let _guard = acquire_provision_lock(
        &provision_lock_path(&adapters, "python"),
        PROVISION_LOCK_WAIT,
        PROVISION_LOCK_POLL,
    )?;
    ensure_pybridge_in(&python_dir()?)
}

/// `ensure_pybridge` against an explicit dir (temp-HOME test seam; the
/// public entry pins the canonical adapter dir). Lock-free: the caller
/// holds the adapter lock (or the test owns the dir outright).
fn ensure_pybridge_in(dir: &std::path::Path) -> anyhow::Result<PathBuf> {
    check_provision_path(dir, true, "adapter dir")?;
    let dest = dir.join("pybridge.py");
    check_provision_path(&dest, false, "bridge file")?;
    if !file_matches(&dest, PYBRIDGE_SOURCE) {
        ensure_provision_dir(dir)?;
        atomic_write_provisioned(&dest, PYBRIDGE_SOURCE)?;
    }
    Ok(dest)
}

pub fn node_dir() -> anyhow::Result<PathBuf> {
    Ok(adapters_dir()?.join("node"))
}

/// Shared node_modules holding the `ws` client (provisioned once by the
/// node adapter, reused by the browser bridge via NODE_PATH).
pub fn node_modules_dir() -> anyhow::Result<PathBuf> {
    Ok(node_dir()?.join("node_modules"))
}

/// Resolve the node binary running the bridge (explicit --node reaches the
/// bridge for the *target*; the bridge itself always runs on PATH node).
/// Returns "node" when `node --version` succeeds, else a helpful error.
pub fn ensure_node() -> anyhow::Result<String> {
    let ok = std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        Ok("node".to_string())
    } else {
        anyhow::bail!("node not found on PATH (install Node 18+ for the Node adapter)")
    }
}

/// Ensure the tiny `ws` client lives in the isolated adapter dir
/// (first use runs `npm install ws` once; reused afterwards).
/// Single-flights on node.lock; the locked inner assumes it is held.
pub fn ensure_ws() -> anyhow::Result<()> {
    let dir = node_dir()?;
    if ensure_ws_marker_ok(&dir) {
        return Ok(());
    }
    let adapters = adapters_dir()?;
    ensure_provision_dir(&adapters)?;
    let _guard = acquire_provision_lock(
        &provision_lock_path(&adapters, "node"),
        PROVISION_LOCK_WAIT,
        PROVISION_LOCK_POLL,
    )?;
    ensure_ws_locked(&node_dir()?)
}

/// Marker probe (no writes, no lock): the installed `ws` package manifest.
fn ensure_ws_marker_ok(dir: &std::path::Path) -> bool {
    dir.join("node_modules")
        .join("ws")
        .join("package.json")
        .exists()
}

/// Non-acquiring inner: caller holds node.lock. Rechecks the marker (a
/// waiter never repeats a finished install), then installs.
fn ensure_ws_locked(dir: &std::path::Path) -> anyhow::Result<()> {
    if ensure_ws_marker_ok(dir) {
        return Ok(());
    }
    check_provision_path(&dir, true, "adapter dir")?;
    // `node_modules/` is npm-managed output: guard the dir, not each
    // generated file.
    ensure_provision_dir(&dir)?;
    let marker = dir.join("node_modules").join("ws").join("package.json");
    let out = run_with_timeout(
        PathBuf::from("npm"),
        &[
            "install".to_string(),
            "ws".to_string(),
            "--prefix".to_string(),
            dir.to_string_lossy().to_string(),
            "--no-audit".to_string(),
            "--no-fund".to_string(),
        ],
        std::time::Duration::from_secs(180),
    )
    .map_err(|e| anyhow::anyhow!("npm failed to start: {e} (Node 18+ with npm required)"))?;
    if !out.status.success() || !marker.exists() {
        anyhow::bail!(
            "could not install `ws` (network needed once). Try manually:\n  npm install ws --prefix {} --no-audit --no-fund",
            dir.to_string_lossy()
        );
    }
    Ok(())
}

/// Write the embedded nodebridge (+ shared core) when changed; return path.
/// Single-flights on node.lock.
pub fn ensure_nodebridge() -> anyhow::Result<PathBuf> {
    let adapters = adapters_dir()?;
    ensure_provision_dir(&adapters)?;
    let _guard = acquire_provision_lock(
        &provision_lock_path(&adapters, "node"),
        PROVISION_LOCK_WAIT,
        PROVISION_LOCK_POLL,
    )?;
    ensure_nodebridge_in(&node_dir()?)
}

/// `ensure_nodebridge` against an explicit dir (temp-HOME test seam).
/// Lock-free: the caller holds the adapter lock (or the test owns the dir).
fn ensure_nodebridge_in(dir: &std::path::Path) -> anyhow::Result<PathBuf> {
    check_provision_path(dir, true, "adapter dir")?;
    let dest = dir.join("nodebridge.js");
    check_provision_path(&dest, false, "bridge file")?;
    // Independent checks: `||` would short-circuit and skip the shared
    // write exactly when the bridge itself is stale (observed live: new
    // bridge with requires, shared files missing, startup crash).
    let bridge_stale = !file_matches(&dest, NODEBRIDGE_SOURCE);
    let shared_stale = ensure_js_shared(dir)?;
    let stale = bridge_stale || shared_stale;
    if stale {
        ensure_provision_dir(dir)?;
        atomic_write_provisioned(&dest, NODEBRIDGE_SOURCE)?;
    }
    Ok(dest)
}

pub fn browser_dir() -> anyhow::Result<PathBuf> {
    Ok(adapters_dir()?.join("browser"))
}

/// Write the embedded browserbridge when it changed; return its path.
/// The bridge itself runs on Node (shared provisioning); Chrome is the
/// *target* and is never installed by us — see find_chrome().
///
/// Locking (fixed global order `node → browser`, never reverse): acquires
/// node.lock then browser.lock ONCE, then runs the non-acquiring inners —
/// it never calls the locking `ensure_ws()` wrapper while holding
/// node.lock (that would self-deadlock on the re-acquire).
pub fn ensure_browserbridge() -> anyhow::Result<PathBuf> {
    let adapters = adapters_dir()?;
    ensure_provision_dir(&adapters)?;
    let _node = acquire_provision_lock(
        &provision_lock_path(&adapters, "node"),
        PROVISION_LOCK_WAIT,
        PROVISION_LOCK_POLL,
    )?;
    let _browser = acquire_provision_lock(
        &provision_lock_path(&adapters, "browser"),
        PROVISION_LOCK_WAIT,
        PROVISION_LOCK_POLL,
    )?;
    ensure_browserbridge_under(&node_dir()?, &browser_dir()?)
}

/// Non-acquiring inner: caller holds node.lock + browser.lock (in that
/// order). Shared `ws` first (via NODE_PATH, no second install), then the
/// browser bridge + shared core.
fn ensure_browserbridge_under(
    ws_dir: &std::path::Path,
    dir: &std::path::Path,
) -> anyhow::Result<PathBuf> {
    ensure_ws_locked(ws_dir)?;
    ensure_browserbridge_in(dir)
}

/// `ensure_browserbridge` against an explicit dir (temp-HOME test seam).
/// Lock-free: the caller holds the adapter locks (or the test owns the dir).
fn ensure_browserbridge_in(dir: &std::path::Path) -> anyhow::Result<PathBuf> {
    check_provision_path(dir, true, "adapter dir")?;
    let dest = dir.join("browserbridge.js");
    check_provision_path(&dest, false, "bridge file")?;
    // Same no-short-circuit rule as ensure_nodebridge (see above).
    let bridge_stale = !file_matches(&dest, BROWSERBRIDGE_SOURCE);
    let shared_stale = ensure_js_shared(dir)?;
    let stale = bridge_stale || shared_stale;
    if stale {
        ensure_provision_dir(dir)?;
        atomic_write_provisioned(&dest, BROWSERBRIDGE_SOURCE)?;
    }
    Ok(dest)
}

/// Locate a Chrome/Chromium binary for the browser adapter (target only —
/// we attach to it, never provision it). Returns (binary, version line).
pub fn find_chrome() -> Option<(String, String)> {
    let candidates = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
    ];
    for bin in candidates {
        if let Ok(out) = std::process::Command::new(bin).arg("--version").output() {
            if out.status.success() {
                let mut text = String::from_utf8_lossy(&out.stdout).to_string();
                text.push_str(&String::from_utf8_lossy(&out.stderr));
                let first = text.lines().next().unwrap_or("").trim().to_string();
                return Some((bin.to_string(), first));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::with_home;

    fn tmpdir(name: &str) -> PathBuf {
        // Unique per test (Rust tests run in parallel): pid + name.
        let dir = std::env::temp_dir().join(format!(
            "agent-debugger-bridge-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn ino_of(p: &std::path::Path) -> (u64, u64) {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(p).unwrap();
        (m.dev(), m.ino())
    }

    #[test]
    fn provision_roots_fail_closed_without_home() {
        // No /tmp fallback anywhere: every root and derived helper
        // propagates the actionable HOME error.
        with_home(None, || {
            for r in [
                adapters_dir(),
                adapter_dir(),
                python_dir(),
                node_dir(),
                browser_dir(),
                venv_python(),
                node_modules_dir(),
            ] {
                let err = format!("{:#}", r.unwrap_err());
                assert!(err.contains("HOME is unset or empty"), "{err}");
            }
        });
        with_home(Some(std::path::Path::new("")), || {
            assert!(python_dir().is_err());
            assert!(node_dir().is_err());
        });
    }

    #[test]
    fn provision_lock_contention_serializes_and_retains_inode() {
        let dir = tmpdir("provision-lock");
        let lock = provision_lock_path(&dir, "python");
        let holder =
            acquire_provision_lock(&lock, Duration::from_secs(5), Duration::from_millis(5))
                .unwrap();
        // A rival with a short (injectable) bound bails retryably instead
        // of waiting forever.
        let t0 = std::time::Instant::now();
        let err = format!(
            "{:#}",
            acquire_provision_lock(&lock, Duration::from_millis(100), Duration::from_millis(5))
                .unwrap_err()
        );
        assert!(err.contains("in progress; retry shortly"), "{err}");
        assert!(t0.elapsed() < Duration::from_secs(5), "short bound honored");
        assert!(lock.exists(), "lock file retained on contention");
        #[cfg(unix)]
        let before = ino_of(&lock);
        drop(holder);
        // Release admits the next holder on the same inode.
        let _next = acquire_provision_lock(&lock, Duration::from_secs(5), Duration::from_millis(5))
            .unwrap();
        assert!(lock.exists(), "lock file never deleted");
        #[cfg(unix)]
        assert_eq!(ino_of(&lock), before, "inode rendezvous retained");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn provision_locks_are_not_reentrant() {
        // Same-holder re-acquire with a short bound fails retryably. This
        // pins the _locked split load-bearing: a wrapper calling a locking
        // wrapper would surface "in progress", never silently double-hold.
        let dir = tmpdir("provision-reentrant");
        let lock = provision_lock_path(&dir, "node");
        let _held = acquire_provision_lock(&lock, Duration::from_secs(5), Duration::from_millis(5))
            .unwrap();
        let err = format!(
            "{:#}",
            acquire_provision_lock(&lock, Duration::from_millis(100), Duration::from_millis(5))
                .unwrap_err()
        );
        assert!(err.contains("in progress; retry shortly"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// True cross-process evidence on unix: python3 holds a BSD flock on
    /// the lock file (the same mechanism as File::try_lock); killing it
    /// must release, and the next Rust holder proceeds on the same inode.
    /// Unix-only: the holder needs fcntl.flock, which does not exist on
    /// Windows. Death-release on Windows is OS-guaranteed like everywhere
    /// (LockFileEx releases on process death) and the Windows kill path
    /// itself (taskkill) is covered by run_with_timeout_kills_slow_child,
    /// which runs on every platform including Windows CI.
    #[test]
    #[cfg(unix)]
    fn provision_lock_releases_on_holder_death() {
        let dir = tmpdir("provision-crash");
        let lock = provision_lock_path(&dir, "python");
        std::fs::write(&lock, "").unwrap();
        #[cfg(unix)]
        let before = ino_of(&lock);
        let prog = lock.to_string_lossy().to_string();
        let mut child = std::process::Command::new("python3")
            .args([
                "-c",
                "import fcntl,sys,time; f=open(sys.argv[1],'r+'); \
                 fcntl.flock(f.fileno(), fcntl.LOCK_EX); time.sleep(30)",
                &prog,
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("python3 is required (canonical gate dependency)");
        std::thread::sleep(Duration::from_secs(1)); // let the child lock
        let err = format!(
            "{:#}",
            acquire_provision_lock(&lock, Duration::from_millis(200), Duration::from_millis(5))
                .unwrap_err()
        );
        assert!(
            err.contains("in progress; retry shortly"),
            "live foreign holder blocks: {err}"
        );
        child.kill().expect("kill foreign holder");
        child.wait().expect("reap foreign holder");
        let _next = acquire_provision_lock(&lock, Duration::from_secs(5), Duration::from_millis(5))
            .unwrap();
        assert!(lock.exists(), "lock file retained across holder death");
        #[cfg(unix)]
        assert_eq!(ino_of(&lock), before, "inode retained across death");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn provision_symlink_plants_refused() {
        // Canonical-scope plants under an isolated HOME: adapter-dir link,
        // bridge-dest link, and lock link all bail with the link intact
        // and the outside target untouched (never followed, never deleted).
        let home = tmpdir("provision-home-links");
        let outside = tmpdir("provision-outside");
        std::fs::write(outside.join("sentinel.txt"), "do-not-touch").unwrap();
        with_home(Some(home.as_path()), || {
            let adapters = adapters_dir().unwrap();
            std::fs::create_dir_all(&adapters).unwrap();
            // Lock link planted BEFORE any ensure call (the wrapper
            // creates the real file on first acquire, so plant first).
            let lock = provision_lock_path(&adapters, "python");
            std::os::unix::fs::symlink(outside.join("sentinel.txt"), &lock).unwrap();
            let err = format!(
                "{:#}",
                acquire_provision_lock(&lock, Duration::from_millis(50), Duration::from_millis(5))
                    .unwrap_err()
            );
            assert!(err.contains("must not be a symlink"), "{err}");
            std::fs::remove_file(&lock).unwrap();
            // Adapter dir symlink: the wrapper acquires the lock, then the
            // inner refuses the dir (lock retained, dir never followed).
            let adir = python_dir().unwrap();
            std::os::unix::fs::symlink(&outside, &adir).unwrap();
            let err = format!("{:#}", ensure_pybridge().unwrap_err());
            assert!(err.contains("must not be a symlink"), "{err}");
        });
        let home2 = tmpdir("provision-home-links2");
        with_home(Some(home2.as_path()), || {
            let adir = python_dir().unwrap();
            std::fs::create_dir_all(&adir).unwrap();
            std::os::unix::fs::symlink(outside.join("sentinel.txt"), adir.join("pybridge.py"))
                .unwrap();
            let err = format!("{:#}", ensure_pybridge().unwrap_err());
            assert!(err.contains("must not be a symlink"), "{err}");
        });
        assert_eq!(
            std::fs::read_to_string(outside.join("sentinel.txt")).unwrap(),
            "do-not-touch",
            "outside target intact"
        );
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&home2);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    #[cfg(unix)]
    fn provision_nested_generated_dir_symlink_refused() {
        // Tool-managed nested dirs (venv/, node_modules/, classes/) get
        // the same symlink refusal as adapter roots — realistically scoped:
        // the generated files inside stay tool-managed, only the dirs are
        // judged, and system prefixes above the anchor never are.
        let base = tmpdir("provision-nested");
        let outside = tmpdir("provision-nested-out");
        std::fs::write(outside.join("sentinel.txt"), "x").unwrap();
        let venv = base.join("venv");
        std::os::unix::fs::symlink(&outside, &venv).unwrap();
        let err = format!("{:#}", ensure_provision_dir(&venv).unwrap_err());
        assert!(err.contains("must not be a symlink"), "{err}");
        assert!(
            std::fs::symlink_metadata(&venv)
                .unwrap()
                .file_type()
                .is_symlink(),
            "plant intact, never followed"
        );
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn browser_provisioning_single_flights_without_self_deadlock() {
        // Pre-seeded ws marker so no npm/network runs; the composition
        // (node.lock then browser.lock, locked inners only) must complete
        // and converge on repeat. Locks release immediately after.
        let home = tmpdir("provision-home-browser");
        with_home(Some(home.as_path()), || {
            let ws_marker = node_dir().unwrap().join("node_modules").join("ws");
            std::fs::create_dir_all(&ws_marker).unwrap();
            std::fs::write(
                ws_marker.join("package.json"),
                r#"{"name":"ws","version":"9.0.0"}"#,
            )
            .unwrap();
            let dest = ensure_browserbridge().unwrap();
            assert_eq!(
                std::fs::read_to_string(&dest).unwrap(),
                BROWSERBRIDGE_SOURCE
            );
            for (name, source) in JS_SHARED {
                assert_eq!(
                    std::fs::read_to_string(browser_dir().unwrap().join(name)).unwrap(),
                    *source,
                    "shared {name} provisioned"
                );
            }
            let dest2 = ensure_browserbridge().unwrap();
            assert_eq!(dest, dest2, "second run converges");
            let adapters = adapters_dir().unwrap();
            let _n = acquire_provision_lock(
                &provision_lock_path(&adapters, "node"),
                Duration::from_secs(5),
                Duration::from_millis(5),
            )
            .unwrap();
            let _b = acquire_provision_lock(
                &provision_lock_path(&adapters, "browser"),
                Duration::from_secs(5),
                Duration::from_millis(5),
            )
            .unwrap();
        });
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn concurrent_pybridge_provision_serializes() {
        // Eight racers on one adapter: all succeed with identical content,
        // and the lock file survives (never deleted/replaced).
        let home = tmpdir("provision-home-concurrent");
        with_home(Some(home.as_path()), || {
            std::thread::scope(|s| {
                for _ in 0..8 {
                    s.spawn(|| {
                        let dest = ensure_pybridge().unwrap();
                        assert_eq!(std::fs::read_to_string(&dest).unwrap(), PYBRIDGE_SOURCE);
                    });
                }
            });
            let adapters = adapters_dir().unwrap();
            let lock = provision_lock_path(&adapters, "python");
            assert!(lock.exists(), "lock file retained");
            #[cfg(unix)]
            {
                let a = ino_of(&lock);
                let _g =
                    acquire_provision_lock(&lock, Duration::from_secs(5), Duration::from_millis(5))
                        .unwrap();
                assert_eq!(ino_of(&lock), a, "inode stable across contention");
            }
        });
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn provision_atomic_write_leaves_no_tmp() {
        // Exclusive unique tmp + atomic rename: success leaves no tmp
        // droppings beside the dest, and a second run converges.
        let dir = tmpdir("provision-atomic");
        let dest = ensure_pybridge_in(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), PYBRIDGE_SOURCE);
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "no tmp droppings: {leftovers:?}");
        ensure_pybridge_in(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every contract fixture must parse (a malformed edit breaks all
    /// three language consumers) and the CLI-side caps must equal the
    /// frozen values (bridge-side totals live in py/node sources and are
    /// asserted by their own fixture tests).
    #[test]
    fn contract_fixtures_match_cli_constants() {
        for name in [
            "timeout_prefix.json",
            "breaks_echo.json",
            "identity_caps.json",
            "error_phases.json",
            "close_status.json",
        ] {
            let text = std::fs::read_to_string(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("tests")
                    .join("contract")
                    .join(name),
            )
            .unwrap();
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert!(v.is_object(), "{name} must stay a flat JSON object");
        }
        let caps: serde_json::Value =
            serde_json::from_str(include_str!("../tests/contract/identity_caps.json")).unwrap();
        assert_eq!(
            crate::session::IDENTITY_FIELD_CAP,
            caps["fieldCap"].as_u64().unwrap() as usize
        );
        assert_eq!(
            crate::session::IDENTITY_TOTAL_CAP,
            caps["cliTotalCap"].as_u64().unwrap() as usize
        );
        assert_eq!(
            crate::session::CAUSE_CAP,
            caps["causeCap"].as_u64().unwrap() as usize
        );
        assert_eq!(caps["redacted"].as_str().unwrap(), "[redacted]");
    }

    /// Marker completeness: every embedded source contributes at least one
    /// expected class output, so a damaged classes dir (marker present,
    /// some outputs deleted) still triggers a recompile. `BridgeModel.java`
    /// provides `Config` (plus `ConfigBridgeException`); `BridgeProto.java`
    /// provides `BridgeProto` (plus `StreamGobbler`). Update this test with
    /// the source when adding classes.
    #[test]
    fn java_classes_cover_embedded_sources() {
        // (source file, marker classes it must provide at least one of).
        let provided: &[(&str, &[&str])] = &[
            ("JdiBridge.java", &["JdiBridge"]),
            ("BridgeModel.java", &["Config"]),
            ("BridgeCli.java", &["BridgeCli"]),
            ("BridgeConn.java", &["BridgeConn"]),
            ("BridgeSnapshot.java", &["BridgeSnapshot"]),
            ("BridgeSession.java", &["BridgeSession"]),
            ("BridgeProto.java", &["BridgeProto", "StreamGobbler"]),
            ("BridgeEval.java", &["BridgeEval"]),
        ];
        assert_eq!(
            JAVA_SOURCES.len(),
            provided.len(),
            "embedded source set changed: update the provider table"
        );
        for (i, (name, _source)) in JAVA_SOURCES.iter().enumerate() {
            assert_eq!(*name, provided[i].0, "embedded source order changed");
            assert!(
                provided[i].1.iter().any(|c| JAVA_CLASSES.contains(c)),
                "embedded {name} provides no marker class"
            );
        }
        for class in JAVA_CLASSES {
            let owner = provided.iter().find(|(_, markers)| markers.contains(class));
            assert!(owner.is_some(), "marker {class} maps to no embedded source");
        }
    }

    #[test]
    fn debugpy_install_uses_venv_interpreter_with_m_pip() {
        // Pinned HOME: other tests swap HOME process-wide; path derivation
        // must observe one stable root.
        with_home(
            Some(std::path::Path::new("/tmp/agent-debugger-home-stable")),
            || {
                let (program, args) = debugpy_install_command().unwrap();
                // Program is exactly the venv interpreter, never `<...>/bin/python/bin/pip`.
                assert_eq!(program, venv_python().unwrap());
                assert_eq!(program.file_name().and_then(|s| s.to_str()), Some("python"));
                assert_eq!(
                    program
                        .parent()
                        .and_then(|p| p.file_name())
                        .and_then(|s| s.to_str()),
                    Some("bin")
                );
                assert_eq!(args, vec!["-m", "pip", "install", "debugpy==1.8.21"]);
            },
        );
    }

    #[test]
    fn venv_paths_derive_from_adapter_dir_and_absent_interp_is_no_debugpy() {
        // Path seam of `ensure_py` (which itself needs processes/network
        // and stays untested): the venv interpreter is derived from the
        // adapter dir, never the system PATH. A missing interpreter reads
        // as "no debugpy" (the check the order logic branches on), never
        // an error — this test pins the seam, not the order itself.
        // Pinned HOME: derivation must observe one stable root.
        with_home(
            Some(std::path::Path::new("/tmp/agent-debugger-home-stable")),
            || {
                assert_eq!(
                    venv_python().unwrap(),
                    python_dir()
                        .unwrap()
                        .join("venv")
                        .join("bin")
                        .join("python")
                );
            },
        );
        // A missing interpreter is "no debugpy" (fallback proceeds),
        // never an error: proves the order check degrades, not fails.
        assert!(!has_debugpy("/nonexistent/agent-debugger-interp"));
    }

    #[test]
    fn node_path_prepend_preserves_user_value() {
        assert_eq!(prepend_node_path("/ws", None), "/ws");
        assert_eq!(prepend_node_path("/ws", Some("")), "/ws");
        assert_eq!(prepend_node_path("/ws", Some("/u/lib")), "/ws:/u/lib");
    }

    #[test]
    fn run_with_timeout_kills_slow_child() {
        // A 30s sleeper with a 1s bound must fail fast with the timeout
        // error (not hang 30s), on every OS — the kill path above must
        // fire portably (taskkill on Windows, kill elsewhere).
        let start = std::time::Instant::now();
        let err = run_with_timeout(
            PathBuf::from("python3"),
            &["-c".to_string(), "import time; time.sleep(30)".to_string()],
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("timed out after 1s"),
            "timeout error names the bound: {err:#}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "must return near the bound, not after the sleeper"
        );
    }

    #[test]
    fn stale_embedded_source_rewrites_once_then_stable() {
        for (ensure, file, source) in [
            (
                ensure_pybridge_in as fn(&std::path::Path) -> anyhow::Result<PathBuf>,
                "pybridge.py",
                PYBRIDGE_SOURCE,
            ),
            (
                ensure_nodebridge_in as fn(&std::path::Path) -> anyhow::Result<PathBuf>,
                "nodebridge.js",
                NODEBRIDGE_SOURCE,
            ),
            (
                ensure_browserbridge_in as fn(&std::path::Path) -> anyhow::Result<PathBuf>,
                "browserbridge.js",
                BROWSERBRIDGE_SOURCE,
            ),
        ] {
            let dir = tmpdir(file);
            std::fs::write(dir.join(file), "// stale embedded copy").unwrap();
            let dest = ensure(&dir).unwrap();
            assert_eq!(std::fs::read_to_string(&dest).unwrap(), source);
            // Second run converges: content identical, no error.
            let dest2 = ensure(&dir).unwrap();
            assert_eq!(dest, dest2);
            assert_eq!(std::fs::read_to_string(&dest2).unwrap(), source);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn shared_js_never_short_circuits() {
        // Fresh bridge + missing shared files: shared core is still
        // written (the observed live crash was exactly this shape).
        for (bridge_name, ensure, source) in [
            (
                "nodebridge.js",
                ensure_nodebridge_in as fn(&std::path::Path) -> anyhow::Result<PathBuf>,
                NODEBRIDGE_SOURCE,
            ),
            (
                "browserbridge.js",
                ensure_browserbridge_in as fn(&std::path::Path) -> anyhow::Result<PathBuf>,
                BROWSERBRIDGE_SOURCE,
            ),
        ] {
            let dir = tmpdir("shared-fresh");
            std::fs::write(dir.join(bridge_name), source).unwrap();
            ensure(&dir).unwrap();
            for (name, source) in JS_SHARED {
                assert_eq!(
                    std::fs::read_to_string(dir.join(name)).unwrap(),
                    *source,
                    "shared {name} must be provisioned even when the bridge is fresh"
                );
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
        // Stale bridge + missing shared: both converge in one call.
        let dir = tmpdir("shared-stale");
        std::fs::write(dir.join("nodebridge.js"), "// stale").unwrap();
        ensure_nodebridge_in(&dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("nodebridge.js")).unwrap(),
            NODEBRIDGE_SOURCE
        );
        for (name, source) in JS_SHARED {
            assert_eq!(std::fs::read_to_string(dir.join(name)).unwrap(), *source);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
