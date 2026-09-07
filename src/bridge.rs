//! Embedded debug bridges: first-use provisioning.
//!
//! Java ships as source inside this binary via `include_str!` and is compiled
//! once with the host `javac`. Python ships the same way as a script (no
//! compile); its interpreter is an isolated venv with debugpy, created on
//! first use. Node ships the same way as a script speaking raw CDP; its only
//! dependency is the tiny `ws` package, installed once into an isolated dir.
//! All bridges run as per-session daemons (see `session`).

use std::path::PathBuf;

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
fn file_matches(path: &std::path::Path, source: &str) -> bool {
    std::fs::read_to_string(path)
        .map(|existing| existing == source)
        .unwrap_or(false)
}

/// Write the shared JS core into a bridge dir when changed.
fn ensure_js_shared(dir: &std::path::Path) -> anyhow::Result<bool> {
    let mut changed = false;
    for (name, source) in JS_SHARED {
        let dest = dir.join(name);
        if !file_matches(&dest, source) {
            std::fs::create_dir_all(dir)
                .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", dir.to_string_lossy()))?;
            std::fs::write(&dest, source)
                .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", dest.to_string_lossy()))?;
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

pub fn adapter_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".agent-debugger")
        .join("adapters")
        .join("java")
}

/// Ensure the bridge is compiled; return the classes dir for `java -cp`.
pub fn ensure_compiled() -> anyhow::Result<PathBuf> {
    let dir = adapter_dir();
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
        if !file_matches(&dest, source) {
            stale = true;
        }
    }
    if stale {
        std::fs::create_dir_all(&classes)
            .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", classes.to_string_lossy()))?;
        let mut sources = Vec::with_capacity(JAVA_SOURCES.len());
        for (name, source) in JAVA_SOURCES {
            let dest = dir.join(name);
            std::fs::write(&dest, source)
                .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", dest.to_string_lossy()))?;
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

pub fn python_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".agent-debugger")
        .join("adapters")
        .join("python")
}

fn venv_python() -> PathBuf {
    python_dir().join("venv").join("bin").join("python")
}

/// Program + args used to install debugpy into the isolated venv.
/// Always the venv interpreter itself (`<venv>/bin/python -m pip ...`);
/// a `<venv>/bin/pip` path would resolve through the interpreter file
/// (`<venv>/bin/python/bin/pip`, ENOTDIR on fresh HOME).
fn debugpy_install_command() -> (PathBuf, Vec<String>) {
    (
        venv_python(),
        vec![
            "-m".to_string(),
            "pip".to_string(),
            "install".to_string(),
            "debugpy".to_string(),
        ],
    )
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
pub fn ensure_py() -> anyhow::Result<String> {
    let venv = venv_python();
    let venv_str = venv.to_string_lossy().to_string();
    if has_debugpy(&venv_str) {
        return Ok(venv_str);
    }
    if has_debugpy("python3") {
        return Ok("python3".to_string());
    }
    // First use: isolated venv so we never touch the system Python (PEP 668).
    let venv_dir = python_dir().join("venv");
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
    let (pip_program, pip_args) = debugpy_install_command();
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
            // Best-effort kill (Unix `kill`; ignored elsewhere — the waiter
            // thread reaps the child whenever it actually exits).
            let _ = std::process::Command::new("kill")
                .arg(child_id.to_string())
                .output();
            Err(anyhow::anyhow!(
                "{program_dbg} timed out after {}s",
                timeout.as_secs()
            ))
        }
    }
}

/// Write the embedded pybridge when it changed; return its path.
pub fn ensure_pybridge() -> anyhow::Result<PathBuf> {
    ensure_pybridge_in(&python_dir())
}

/// `ensure_pybridge` against an explicit dir (temp-HOME test seam; the
/// public entry pins the canonical adapter dir).
fn ensure_pybridge_in(dir: &std::path::Path) -> anyhow::Result<PathBuf> {
    let dest = dir.join("pybridge.py");
    if !file_matches(&dest, PYBRIDGE_SOURCE) {
        std::fs::create_dir_all(dir)
            .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", dir.to_string_lossy()))?;
        std::fs::write(&dest, PYBRIDGE_SOURCE)
            .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", dest.to_string_lossy()))?;
    }
    Ok(dest)
}

pub fn node_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".agent-debugger")
        .join("adapters")
        .join("node")
}

/// Shared node_modules holding the `ws` client (provisioned once by the
/// node adapter, reused by the browser bridge via NODE_PATH).
pub fn node_modules_dir() -> PathBuf {
    node_dir().join("node_modules")
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
pub fn ensure_ws() -> anyhow::Result<()> {
    let dir = node_dir();
    let marker = dir.join("node_modules").join("ws").join("package.json");
    if marker.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", dir.to_string_lossy()))?;
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
pub fn ensure_nodebridge() -> anyhow::Result<PathBuf> {
    ensure_nodebridge_in(&node_dir())
}

/// `ensure_nodebridge` against an explicit dir (temp-HOME test seam).
fn ensure_nodebridge_in(dir: &std::path::Path) -> anyhow::Result<PathBuf> {
    let dest = dir.join("nodebridge.js");
    // Independent checks: `||` would short-circuit and skip the shared
    // write exactly when the bridge itself is stale (observed live: new
    // bridge with requires, shared files missing, startup crash).
    let bridge_stale = !file_matches(&dest, NODEBRIDGE_SOURCE);
    let shared_stale = ensure_js_shared(dir)?;
    let stale = bridge_stale || shared_stale;
    if stale {
        std::fs::create_dir_all(dir)
            .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", dir.to_string_lossy()))?;
        std::fs::write(&dest, NODEBRIDGE_SOURCE)
            .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", dest.to_string_lossy()))?;
    }
    Ok(dest)
}

pub fn browser_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".agent-debugger")
        .join("adapters")
        .join("browser")
}

/// Write the embedded browserbridge when it changed; return its path.
/// The bridge itself runs on Node (shared provisioning); Chrome is the
/// *target* and is never installed by us — see find_chrome().
pub fn ensure_browserbridge() -> anyhow::Result<PathBuf> {
    ensure_browserbridge_in(&browser_dir())
}

/// `ensure_browserbridge` against an explicit dir (temp-HOME test seam).
fn ensure_browserbridge_in(dir: &std::path::Path) -> anyhow::Result<PathBuf> {
    let dest = dir.join("browserbridge.js");
    // Same no-short-circuit rule as ensure_nodebridge (see above).
    let bridge_stale = !file_matches(&dest, BROWSERBRIDGE_SOURCE);
    let shared_stale = ensure_js_shared(dir)?;
    let stale = bridge_stale || shared_stale;
    if stale {
        std::fs::create_dir_all(dir)
            .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", dir.to_string_lossy()))?;
        std::fs::write(&dest, BROWSERBRIDGE_SOURCE)
            .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", dest.to_string_lossy()))?;
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
        let (program, args) = debugpy_install_command();
        // Program is exactly the venv interpreter, never `<...>/bin/python/bin/pip`.
        assert_eq!(program, venv_python());
        assert_eq!(program.file_name().and_then(|s| s.to_str()), Some("python"));
        assert_eq!(
            program
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str()),
            Some("bin")
        );
        assert_eq!(args, vec!["-m", "pip", "install", "debugpy"]);
    }

    #[test]
    fn venv_paths_derive_from_adapter_dir_and_absent_interp_is_no_debugpy() {
        // Path seam of `ensure_py` (which itself needs processes/network
        // and stays untested): the venv interpreter is derived from the
        // adapter dir, never the system PATH. A missing interpreter reads
        // as "no debugpy" (the check the order logic branches on), never
        // an error — this test pins the seam, not the order itself.
        assert_eq!(
            venv_python(),
            python_dir().join("venv").join("bin").join("python")
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
