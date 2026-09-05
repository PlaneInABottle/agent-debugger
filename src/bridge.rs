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

const PYBRIDGE_SOURCE: &str = include_str!("../bridge/py/src/pybridge.py");

const NODEBRIDGE_SOURCE: &str = include_str!("../bridge/node/src/nodebridge.js");

const BROWSERBRIDGE_SOURCE: &str = include_str!("../bridge/browser/src/browserbridge.js");

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
    // same package, so one javac invocation covers all files.
    let mut stale = !marker.exists();
    for (name, source) in JAVA_SOURCES {
        let dest = dir.join(name);
        let same = std::fs::read_to_string(&dest)
            .map(|existing| existing == *source)
            .unwrap_or(false);
        if !same {
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
    let pip = run_with_timeout(
        venv.join("bin").join("pip"),
        &["install".to_string(), "debugpy".to_string()],
        std::time::Duration::from_secs(180),
    )
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
    let dir = python_dir();
    let dest = dir.join("pybridge.py");
    let stale = match std::fs::read_to_string(&dest) {
        Ok(existing) => existing != PYBRIDGE_SOURCE,
        Err(_) => true,
    };
    if stale {
        std::fs::create_dir_all(&dir)
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

/// Write the embedded nodebridge when it changed; return its path.
pub fn ensure_nodebridge() -> anyhow::Result<PathBuf> {
    let dir = node_dir();
    let dest = dir.join("nodebridge.js");
    let stale = match std::fs::read_to_string(&dest) {
        Ok(existing) => existing != NODEBRIDGE_SOURCE,
        Err(_) => true,
    };
    if stale {
        std::fs::create_dir_all(&dir)
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
    let dir = browser_dir();
    let dest = dir.join("browserbridge.js");
    let stale = match std::fs::read_to_string(&dest) {
        Ok(existing) => existing != BROWSERBRIDGE_SOURCE,
        Err(_) => true,
    };
    if stale {
        std::fs::create_dir_all(&dir)
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
