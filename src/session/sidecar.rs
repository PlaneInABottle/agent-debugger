// See `mod.rs` for the one-way dependency DAG.
use serde_json::Value;

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

/// Routing language: `None` when lang.json is missing, corrupt, or has no
/// `lang` string. Unknown values pass through as `Some` and still forward
/// (only exact "java"/"browser" take the main-only path).
pub(crate) fn session_lang_opt(dir: &std::path::Path) -> Option<String> {
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
pub(crate) fn sidecar_is_v2(dir: &std::path::Path, file: &str) -> bool {
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
pub(crate) fn cli_markers_v2(dir: &std::path::Path) -> bool {
    sidecar_is_v2(dir, "lang.json") && sidecar_is_v2(dir, "stops.json")
}

/// Gate for every command that speaks the bridge protocol: old dirs are
/// `status`-visible then `close`-only. Runs after `check_name` /
/// `check_dir_real`, before lang/port routing.
pub(crate) fn require_schema_v2(dir: &std::path::Path, name: &str) -> anyhow::Result<()> {
    if cli_markers_v2(dir) {
        return Ok(());
    }
    anyhow::bail!("unsupported session '{name}' (schema v1; close it and recreate)")
}

/// Atomic sidecar write (tmp+rename in the same dir): a concurrent `status`
/// never reads a torn file.
pub(crate) fn write_sidecar_atomic(
    dir: &std::path::Path,
    file: &str,
    content: &str,
) -> anyhow::Result<()> {
    let tmp = dir.join(format!("{file}.tmp"));
    std::fs::write(&tmp, content)
        .map_err(|e| anyhow::anyhow!("cannot write session {file}: {e}"))?;
    std::fs::rename(&tmp, dir.join(file))
        .map_err(|e| anyhow::anyhow!("cannot write session {file}: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-debugger-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
}
