// See `mod.rs` for the one-way dependency DAG.
use serde_json::Value;
use std::path::PathBuf;

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
    let dir = session_dir(name);
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

pub(crate) fn read_session(name: &str) -> anyhow::Result<Value> {
    let file = checked_session_dir(name)?.join("session.json");
    let raw = std::fs::read_to_string(&file)
        .map_err(|_| anyhow::anyhow!("no session '{name}' (start or attach first)"))?;
    serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("corrupt session file: {e}"))
}

pub(crate) fn session_port(name: &str) -> anyhow::Result<u16> {
    let session = read_session(name)?;
    session
        .get("port")
        .and_then(|p| p.as_u64())
        .and_then(|p| u16::try_from(p).ok())
        .ok_or_else(|| anyhow::anyhow!("corrupt session file for '{name}'"))
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
/// Anything unparseable is not loopback — never a guess.
pub(crate) fn ip_is_loopback(s: &str) -> bool {
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
pub(crate) fn is_unspecified_host(host: &str) -> bool {
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
}
