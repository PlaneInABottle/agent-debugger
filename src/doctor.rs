//! Toolchain probes for `doctor`. Moved verbatim from main.rs.

use crate::bridge;
use serde_json::{json, Value};

pub(super) fn doctor() -> anyhow::Result<Value> {
    // Fail-closed HOME first: without a usable root no adapter path below
    // means anything, so the actionable HOME error is the whole payload —
    // never a partial report over a silent fallback root.
    crate::session::agent_home()?;
    let java = probe("java", &["-version"]);
    let javac = probe("javac", &["-version"]);
    let java_ready = javac["found"].as_bool().unwrap_or(false);
    let python = probe("python3", &["--version"]);
    // Prefer the isolated venv interpreter when provisioned, else system one.
    // (Paths resolve through the fail-closed roots above; a non-UTF8 HOME
    // degrades to the system probe here, never a panic.)
    let venv_py = bridge::python_dir()?
        .join("venv")
        .join("bin")
        .join("python");
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
    // Browser needs BOTH: node runs the bridge, Chrome is the target.
    // Reporting ready on node alone lied on Chrome-less machines.
    let node_found = node["found"].as_bool().unwrap_or(false);
    let chrome_found = chrome["found"].as_bool().unwrap_or(false);
    let ws_pkg = bridge::node_dir()?
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
            // Java readiness is the compiler probe: without javac the
            // embedded sources can never provision (previously hardcoded
            // true, which lied on JDK-less machines).
            "java": {"via": "embedded JDI bridge (persistent session)", "ready": java_ready},
            "python": {"via": "embedded pybridge + debugpy (isolated venv)", "ready": debugpy["found"]},
            "node": {"via": "embedded nodebridge + CDP", "ready": node["found"]},
            "browser": {"via": "embedded browserbridge + CDP", "ready": node_found && chrome_found},
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::with_home;

    #[test]
    fn doctor_fails_closed_without_home() {
        // No partial payload over a fallback root: the actionable HOME
        // error is the whole result.
        with_home(None, || {
            let err = format!("{:#}", doctor().unwrap_err());
            assert!(err.contains("HOME is unset or empty"), "{err}");
            assert!(err.contains("agent-home"), "{err}");
        });
        with_home(Some(std::path::Path::new("")), || {
            assert!(doctor().is_err());
        });
    }

    #[test]
    fn doctor_readiness_matrix_is_honest() {
        // Real probes (fast, no network): every dependency section keeps
        // its shape, and Java readiness EQUALS the javac probe (never the
        // old hardcoded true). Non-UTF8-hostile paths degrade, never panic.
        let v = with_home(
            Some(std::path::Path::new("/tmp/doctor-readiness-home")),
            || doctor().expect("doctor with a usable HOME"),
        );
        for key in ["java", "javac", "python", "debugpy", "node", "ws", "chrome"] {
            assert!(v.get(key).is_some(), "deps section keeps {key}");
        }
        let javac_found = v["javac"]["found"].as_bool().unwrap();
        assert_eq!(
            v["adapters"]["java"]["ready"].as_bool().unwrap(),
            javac_found,
            "java ready reflects the javac probe"
        );
        assert_eq!(
            v["adapters"]["python"]["ready"].as_bool().unwrap(),
            v["debugpy"]["found"].as_bool().unwrap(),
            "python ready reflects debugpy"
        );
        assert_eq!(
            v["adapters"]["node"]["ready"].as_bool().unwrap(),
            v["node"]["found"].as_bool().unwrap(),
            "node ready reflects the node probe"
        );
        assert_eq!(
            v["adapters"]["browser"]["ready"].as_bool().unwrap(),
            v["node"]["found"].as_bool().unwrap() && v["chrome"]["found"].as_bool().unwrap(),
            "browser ready needs node AND chrome"
        );
    }
}
