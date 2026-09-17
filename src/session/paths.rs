// See `mod.rs` for the one-way dependency DAG.
use serde_json::Value;
use std::path::PathBuf;

pub fn sessions_dir() -> anyhow::Result<PathBuf> {
    Ok(agent_home()?.join(".agent-debugger").join("sessions"))
}

pub fn session_dir(name: &str) -> anyhow::Result<PathBuf> {
    Ok(sessions_dir()?.join(name))
}

/// Fail-closed home root (M3): `HOME` unset or blank is an actionable
/// error, never a silent shared `/tmp` fallback (which aimed every user
/// without `HOME` at one shared root). All session/adapter/lock roots
/// route through here.
pub(crate) fn agent_home() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.trim().is_empty() {
        anyhow::bail!(
            "HOME is unset or empty; set HOME to a writable private directory \
             (e.g. export HOME=$PWD/.agent-home)"
        );
    }
    Ok(PathBuf::from(home))
}

/// Session names are single filesystem segments. Without this, an absolute
/// name would escape the sessions root on join (Rust replaces the base),
/// and `close` would recursively delete an arbitrary directory.
pub(crate) fn check_name(name: &str) -> anyhow::Result<()> {
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

pub(crate) fn checked_session_dir(name: &str) -> anyhow::Result<PathBuf> {
    check_name(name)?;
    let dir = session_dir(name)?;
    check_dir_real(&dir)?;
    Ok(dir)
}

/// A session dir must be a real directory, never a symlink: otherwise an
/// attacker-planted entry could redirect sidecar writes (or `close`'s
/// recursive delete) outside the sessions root. TOCTOU between check and
/// use is accepted as best-effort; creation paths below use exclusive
/// create_dir so a swapped-in link fails instead of being followed.
pub(crate) fn check_dir_real(dir: &std::path::Path) -> anyhow::Result<()> {
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

/// Immediate lstat-before-delete classification shared by every recursive
/// session-dir delete (spawn's stale clear, close's unpublished/foreign
/// deletes): a symlink or non-dir is refused with the canonical
/// `check_dir_real` text instead of being followed by `remove_dir_all`.
/// Missing reads as `Ok(false)` (nothing to delete — spawn proceeds to
/// exclusive create, close reports the vanished dir itself); a real
/// directory reads as `Ok(true)` (delete immediately). Any other metadata
/// failure keeps the caller's `context` (each call site preserves its
/// established error behavior). Best-effort only — not a TOCTOU proof —
/// so callers invoke this immediately before the delete with nothing
/// between.
pub(crate) fn real_dir_for_delete(dir: &std::path::Path, context: &str) -> anyhow::Result<bool> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => {
            anyhow::bail!("session path must not be a symlink: {}", dir.display())
        }
        Ok(meta) if !meta.file_type().is_dir() => {
            anyhow::bail!("session path must be a real directory: {}", dir.display())
        }
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => anyhow::bail!("{context} {}: {e}", dir.display()),
    }
}

/// Refuse a planted symlink at a file path that the caller is about to
/// create/truncate (absent is fine). `File::create` follows links, so
/// without this a swapped-in `bridge.log` link redirects bridge output
/// (and truncates the link target). Best-effort like every other planting
/// check — callers invoke it immediately before the create.
pub(crate) fn refuse_symlink(path: &std::path::Path, what: &str) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            anyhow::bail!("{what} must not be a symlink: {}", path.display())
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => anyhow::bail!("cannot stat {what} {}: {e}", path.display()),
    }
}

pub(crate) fn read_session(name: &str) -> anyhow::Result<Value> {
    let file = checked_session_dir(name)?.join("session.json");
    let raw = std::fs::read_to_string(&file)
        .map_err(|_| anyhow::anyhow!("no session '{name}' (start or attach first)"))?;
    serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("corrupt session file: {e}"))
}

/// Kind-preserving read of a session dir's `session.json` for lifecycle
/// decisions (`close`): unlike `read_session` (which erases every read
/// failure into `no session`), this distinguishes missing (NotFound), other
/// IO, and JSON corruption so callers preserve state they cannot understand
/// instead of deleting it. Takes the dir (not a name) so tests exercise it
/// on isolated tmp roots.
#[derive(Debug)]
pub(crate) enum SessionReadError {
    /// No `session.json` at all.
    NotFound { path: String },
    /// Any other read failure, with its kind for the message.
    Io {
        kind: std::io::ErrorKind,
        path: String,
    },
    /// Present but unparseable.
    Corrupt { path: String, detail: String },
}

impl std::fmt::Display for SessionReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionReadError::NotFound { path } => write!(f, "no session file at {path}"),
            SessionReadError::Io { kind, path } => {
                write!(f, "unreadable session file at {path} ({kind:?})")
            }
            SessionReadError::Corrupt { path, detail } => {
                write!(f, "corrupt session file at {path}: {detail}")
            }
        }
    }
}

pub(crate) fn read_session_file(dir: &std::path::Path) -> Result<Value, SessionReadError> {
    let file = dir.join("session.json");
    let raw = match std::fs::read_to_string(&file) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(SessionReadError::NotFound {
                path: file.display().to_string(),
            });
        }
        Err(e) => {
            return Err(SessionReadError::Io {
                kind: e.kind(),
                path: file.display().to_string(),
            });
        }
    };
    match serde_json::from_str(&raw) {
        Ok(v) => Ok(v),
        Err(e) => Err(SessionReadError::Corrupt {
            path: file.display().to_string(),
            detail: e.to_string(),
        }),
    }
}

pub(crate) fn session_port(name: &str) -> anyhow::Result<u16> {
    let session = read_session(name)?;
    session
        .get("port")
        .and_then(|p| p.as_u64())
        .and_then(|p| u16::try_from(p).ok())
        .ok_or_else(|| anyhow::anyhow!("corrupt session file for '{name}'"))
}

/// Test-only serialization for HOME-mutating tests (Rust runs tests in
/// threads of one process and env vars are process-global): every test
/// that sets/removes HOME goes through `with_home`, which holds this
/// guard and restores the previous value. Never nested (std Mutex is not
/// reentrant). Other tests only DERIVE paths from HOME (never write
/// through them), so a concurrent read observes a self-consistent root.
#[cfg(test)]
pub(crate) static HOME_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
struct HomeRestore<'a> {
    _held: std::sync::MutexGuard<'a, ()>,
    previous: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl Drop for HomeRestore<'_> {
    fn drop(&mut self) {
        self.restore_env();
    }
}

#[cfg(test)]
impl HomeRestore<'_> {
    fn restore_env(&self) {
        match &self.previous {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}

/// Run `f` with HOME set/removed, restoring the previous value after.
/// Holds HOME_TEST_GUARD for the duration.
#[cfg(test)]
pub(crate) fn with_home<T>(home: Option<&std::path::Path>, f: impl FnOnce() -> T) -> T {
    let held = HOME_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let restore = HomeRestore {
        _held: held,
        previous: std::env::var_os("HOME"),
    };
    match home {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }
    let out = f();
    drop(restore);
    out
}

pub(crate) fn startup_nonce() -> String {
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
/// Also covers `inet_aton`-style aliases the strict parser rejects but the
/// OS resolves to loopback (`127.1`, `127.0.1`, `0x7f000001`,
/// `017700000001`): without this, the same debug endpoint attached via two
/// spellings escapes the exclusivity scan. Anything unparseable is not
/// loopback — never a guess.
pub(crate) fn ip_is_loopback(s: &str) -> bool {
    let ip: std::net::IpAddr = match s.parse() {
        Ok(ip) => ip,
        Err(_) => return inet_aton_loopback(s),
    };
    if ip.is_loopback() {
        return true;
    }
    match ip {
        std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
        std::net::IpAddr::V4(_) => false, // non-loopback v4, already checked
    }
}

/// `inet_aton` numeric IPv4 forms (`127.1`, `0x7f000001`, `0177.0.0.1`):
/// 1–4 dot-separated parts, each decimal, `0x`-hex, or leading-`0` octal.
/// True only when the resulting 32-bit value is in 127/8.
fn inet_aton_loopback(s: &str) -> bool {
    if s.contains(':') || s.is_empty() {
        return false; // IPv6 (or empty) is the strict parser's job
    }
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() > 4 || parts.iter().any(|p| p.is_empty() || p.len() > 18) {
        return false;
    }
    let mut nums: Vec<u32> = Vec::with_capacity(parts.len());
    for p in &parts {
        let (digits, radix) = if let Some(h) = p.strip_prefix("0x").or_else(|| p.strip_prefix("0X"))
        {
            (h, 16)
        } else if p.len() > 1 && p.starts_with('0') {
            (*p, 8)
        } else {
            (*p, 10)
        };
        if digits.is_empty() || digits.len() > 16 {
            return false;
        }
        let valid = match radix {
            16 => digits.bytes().all(|b| b.is_ascii_hexdigit()),
            8 => digits.bytes().all(|b| matches!(b, b'0'..=b'7')),
            _ => digits.bytes().all(|b| b.is_ascii_digit()),
        };
        if !valid {
            return false;
        }
        let n = match u32::from_str_radix(digits, radix) {
            Ok(n) => n,
            Err(_) => return false,
        };
        nums.push(n);
    }
    let addr: u64 = match nums.len() {
        1 => nums[0] as u64,
        2 => {
            if nums[0] > 0xff || nums[1] > 0xffffff {
                return false;
            }
            ((nums[0] << 24) | nums[1]) as u64
        }
        3 => {
            if nums[0] > 0xff || nums[1] > 0xff || nums[2] > 0xffff {
                return false;
            }
            ((nums[0] << 24) | (nums[1] << 16) | nums[2]) as u64
        }
        4 => {
            if nums.iter().any(|&n| n > 0xff) {
                return false;
            }
            ((nums[0] << 24) | (nums[1] << 16) | (nums[2] << 8) | nums[3]) as u64
        }
        _ => return false,
    };
    if addr > 0xffffffff {
        return false;
    }
    (addr >> 24) == 127
}

/// Unspecified/wildcard destinations can never be attach targets: dialing
/// `0.0.0.0` is platform-dependent (Linux loops back, elsewhere it fails),
/// so it fails fast with a clear error pointing at an explicit address.
pub(crate) fn is_unspecified_host(host: &str) -> bool {
    let h = host.trim();
    let inner = if h.starts_with('[') && h.ends_with(']') && h.len() > 2 {
        &h[1..h.len() - 1]
    } else {
        h
    };
    matches!(
        inner.to_ascii_lowercase().as_str(),
        "0.0.0.0" | "::" | "::0" | "0:0:0:0:0:0:0:0"
    )
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
    fn read_session_file_preserves_error_kind() {
        // The close matrix routes through this seam: missing, IO failure,
        // and corruption must stay distinguishable (read_session erases
        // them all into `no session`).
        let base = tmpdir("read-kind");
        let dir = base.join("demo");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(matches!(
            read_session_file(&dir),
            Err(SessionReadError::NotFound { .. })
        ));
        std::fs::write(dir.join("session.json"), "not json").unwrap();
        assert!(matches!(
            read_session_file(&dir),
            Err(SessionReadError::Corrupt { .. })
        ));
        std::fs::remove_file(dir.join("session.json")).unwrap();
        std::fs::create_dir_all(dir.join("session.json")).unwrap();
        match read_session_file(&dir) {
            Err(SessionReadError::Io { kind, .. }) => {
                assert_ne!(kind, std::io::ErrorKind::NotFound)
            }
            other => panic!("expected Io, got {other:?}"),
        }
        std::fs::remove_dir_all(dir.join("session.json")).unwrap();
        std::fs::write(
            dir.join("session.json"),
            r#"{"name":"demo","kind":"launch","port":1}"#,
        )
        .unwrap();
        assert_eq!(read_session_file(&dir).unwrap()["port"], Value::from(1));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn refuse_symlink_guards_planted_log_link() {
        // Absent and regular files pass; a symlink refuses with the
        // canonical text so File::create never follows it. Symlink
        // creation needs privileges Windows CI may not grant: if the
        // plant fails, the symlink leg is skipped (the other legs still
        // assert), never failed.
        let base = tmpdir("refuse-symlink");
        let target = base.join("victim.txt");
        std::fs::write(&target, "do-not-touch").unwrap();
        let absent = base.join("bridge.log");
        refuse_symlink(&absent, "bridge log").expect("absent passes");
        std::fs::write(&absent, "log").unwrap();
        refuse_symlink(&absent, "bridge log").expect("regular file passes");
        std::fs::remove_file(&absent).unwrap();
        #[cfg(unix)]
        let planted = std::os::unix::fs::symlink(&target, &absent).is_ok();
        #[cfg(windows)]
        let planted = std::os::windows::fs::symlink_file(&target, &absent).is_ok();
        #[cfg(not(any(unix, windows)))]
        let planted = false;
        if !planted {
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        let err = refuse_symlink(&absent, "bridge log").unwrap_err();
        assert!(
            format!("{err:#}").contains("must not be a symlink"),
            "{err:#}"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "do-not-touch");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn agent_home_fails_closed_without_home() {
        // Unset, empty, and blank HOME all fail with the actionable error
        // (never a silent shared /tmp root); every root propagates it.
        for home in [
            None,
            Some(std::path::Path::new("")),
            Some(std::path::Path::new("   ")),
        ] {
            with_home(home, || {
                let err = format!("{:#}", agent_home().unwrap_err());
                assert!(err.contains("HOME is unset or empty"), "{err}");
                assert!(err.contains("agent-home"), "{err}");
                assert!(sessions_dir().is_err());
                assert!(session_dir("demo").is_err());
                assert!(checked_session_dir("demo").is_err());
            });
        }
        // A set HOME still resolves the canonical roots.
        with_home(
            Some(std::path::Path::new("/tmp/agent-debugger-home-probe")),
            || {
                assert_eq!(
                    sessions_dir().unwrap(),
                    std::path::Path::new("/tmp/agent-debugger-home-probe/.agent-debugger/sessions")
                );
            },
        );
    }

    #[test]
    fn with_home_restores_after_panic() {
        // Hold the existing non-reentrant guard manually so both the
        // restoration and the assertion happen under HOME_TEST_GUARD.
        // Calling with_home here would deadlock by design.
        let held = HOME_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let before = std::env::var_os("HOME");
        let restore = HomeRestore {
            _held: held,
            previous: before.clone(),
        };
        std::env::remove_var("HOME");
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("intentional HOME guard panic");
        }));
        assert!(panic.is_err());
        restore.restore_env();
        assert_eq!(std::env::var_os("HOME"), before);
        drop(restore);
        // The mutex and environment remain usable after unwinding.
        with_home(
            Some(std::path::Path::new("/tmp/home-restore-check")),
            || {
                assert_eq!(
                    std::env::var_os("HOME"),
                    Some(std::ffi::OsString::from("/tmp/home-restore-check"))
                );
            },
        );
        assert_eq!(std::env::var_os("HOME"), before);
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
            "127.1", // inet_aton shorthand
            "127.0.1",
            "0x7f000001", // hex form
            "0X7F000001",
            "017700000001", // octal form
            "0177.0.0.1",
            "2130706433", // single decimal form
        ] {
            assert_eq!(normalize_attach_host(alias), "loopback", "{alias}");
        }
        // Near-misses stay non-loopback: overflow, out-of-range part,
        // bad octal digit, empty part, and non-127 first byte.
        for not in [
            "127.0.0.1.5",
            "128.0.0.1",
            "127.0.0.256",
            "0192.0.0.1",
            "127..0.1",
            "0x7f0000010",
        ] {
            assert_ne!(normalize_attach_host(not), "loopback", "{not}");
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
        assert!(is_unspecified_host("::0"));
        assert!(!is_unspecified_host("localhost"));
        assert!(!is_unspecified_host("127.0.0.1"));
        assert!(!is_unspecified_host("example.com"));
    }
}
