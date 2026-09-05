//! Toolchain probes for `doctor`. Moved verbatim from main.rs.

use crate::bridge;
use serde_json::{json, Value};

pub(super) fn doctor() -> anyhow::Result<Value> {
    let java = probe("java", &["-version"]);
    let javac = probe("javac", &["-version"]);
    let python = probe("python3", &["--version"]);
    // Prefer the isolated venv interpreter when provisioned, else system one.
    let venv_py = bridge::python_dir().join("venv").join("bin").join("python");
    let debugpy = if venv_py.exists() {
        probe(
            venv_py.to_str().unwrap_or("python3"),
            &["-c", "import debugpy"],
        )
    } else {
        probe("python3", &["-c", "import debugpy"])
    };
    let node = probe("node", &["--version"]);
    let chrome = match bridge::find_chrome() {
        Some((bin, version)) => json!({"found": true, "version": version, "bin": bin}),
        None => {
            json!({"found": false, "version": "", "hint": "install Chrome/Chromium or start it with --remote-debugging-port=9222"})
        }
    };
    let ws_pkg = bridge::node_dir()
        .join("node_modules")
        .join("ws")
        .join("package.json");
    let ws = if ws_pkg.exists() {
        match std::fs::read_to_string(&ws_pkg)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|v| {
                v.get("version")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string())
            }) {
            Some(ver) => json!({"found": true, "version": ver}),
            None => json!({"found": true, "version": ""}),
        }
    } else {
        json!({"found": false, "version": ""})
    };
    Ok(json!({
        "java": java,
        "javac": javac,
        "python": python,
        "debugpy": debugpy,
        "node": node,
        "ws": ws,
        "chrome": chrome,
        "adapters": {
            "java": {"via": "embedded JDI bridge (persistent session)", "ready": true},
            "python": {"via": "embedded pybridge + debugpy (isolated venv)", "ready": debugpy["found"]},
            "node": {"via": "embedded nodebridge + CDP", "ready": node["found"]},
            "browser": {"via": "embedded browserbridge + CDP", "ready": node["found"]},
        },
    }))
}

fn probe(program: &str, args: &[&str]) -> Value {
    match std::process::Command::new(program).args(args).output() {
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            let first = text.lines().next().unwrap_or("").trim().to_string();
            json!({"found": out.status.success(), "version": first})
        }
        Err(e) => json!({"found": false, "version": "", "error": e.to_string()}),
    }
}
