//! Spawn argument building: CLI stops + target -> bridge args.
//! Moved verbatim from main.rs; behavior unchanged.

use crate::cli::Stops;
use crate::session;
use serde_json::{json, Value};

/// Language-specific half of a spawn: how to tell the adapter WHAT to run.
/// Everything else (breaks, sources, timeout) is shared via [`Stops`].
pub(super) enum Target<'a> {
    JavaLaunch {
        main: &'a str,
        classpath: Option<&'a str>,
    },
    JavaAttach {
        host: &'a str,
        port: u16,
    },
    PyLaunch {
        program: Option<&'a str>,
        module: Option<&'a str>,
        python: Option<&'a str>,
        subprocess: bool,
    },
    PyAttach {
        host: &'a str,
        port: u16,
    },
    NodeLaunch {
        program: &'a str,
        node: Option<&'a str>,
        workers: bool,
    },
    NodeAttach {
        host: &'a str,
        port: u16,
    },
    BrowserAttach {
        host: &'a str,
        port: u16,
        tab: Option<&'a str>,
    },
}

pub(super) fn cmd_spawn(
    session: &str,
    lang: &'static str,
    kind: &'static str,
    target: Target<'_>,
    stops: &Stops,
    program_args: &[String],
) -> anyhow::Result<Value> {
    // Empty breaks allowed: thread dumps and log collection need no stop.
    // Commands needing a stop fail gracefully until one arrives.
    // Identity first: the seed builder only borrows the target; the args
    // match below moves it.
    let requested = requested_target(&target, kind);
    let seed = seed_target_identity(&target);
    let seed_hint = session::identity_hint(&seed);
    let mut args: Vec<String> = Vec::new();
    match target {
        Target::JavaLaunch { main, classpath } => {
            args.push("--main".to_string());
            args.push(main.to_string());
            args.push("--cp".to_string());
            args.push(classpath.unwrap_or(".").to_string());
        }
        Target::JavaAttach { host, port } => {
            args.push("--host".to_string());
            args.push(host.to_string());
            args.push("--port".to_string());
            args.push(port.to_string());
        }
        Target::PyLaunch {
            program,
            module,
            python,
            subprocess,
        } => {
            // Exactly one of file/program and dotted module, checked before
            // any target runs (clap already rejects both; neither is a
            // dispatch-level fail-fast here).
            match (program, module) {
                (Some(p), None) => {
                    args.push("--program".to_string());
                    args.push(p.to_string());
                }
                (None, Some(m)) => {
                    check_module(m)?;
                    args.push("--module".to_string());
                    args.push(m.to_string());
                }
                _ => anyhow::bail!(
                    "py start needs exactly one of PROGRAM and --module \
                     (got neither or both)"
                ),
            }
            if let Some(py) = python {
                args.push("--python".to_string());
                args.push(py.to_string());
            }
            if subprocess {
                args.push("--subprocess".to_string());
            }
        }
        Target::PyAttach { host, port } => {
            args.push("--host".to_string());
            args.push(host.to_string());
            args.push("--port".to_string());
            args.push(port.to_string());
        }
        Target::NodeLaunch {
            program,
            node,
            workers,
        } => {
            args.push("--program".to_string());
            args.push(program.to_string());
            if let Some(n) = node {
                args.push("--node".to_string());
                args.push(n.to_string());
            }
            if workers {
                args.push("--workers".to_string());
            }
        }
        Target::NodeAttach { host, port } => {
            args.push("--host".to_string());
            args.push(host.to_string());
            args.push("--port".to_string());
            args.push(port.to_string());
        }
        Target::BrowserAttach { host, port, tab } => {
            args.push("--host".to_string());
            args.push(host.to_string());
            args.push("--port".to_string());
            args.push(port.to_string());
            if let Some(t) = tab {
                args.push("--tab".to_string());
                args.push(t.to_string());
            }
        }
    }
    for b in &stops.breakpoints {
        args.push("--break".to_string());
        args.push(b.clone());
    }
    for s in &stops.source_paths {
        args.push("--src".to_string());
        args.push(s.clone());
    }
    for l in &stops.logpoints {
        args.push("--logpoint".to_string());
        args.push(l.clone());
    }
    for w in &stops.watches {
        args.push("--watch".to_string());
        args.push(w.clone());
    }
    for e in &stops.exits {
        args.push("--exit".to_string());
        args.push(e.clone());
    }
    args.push("--timeout".to_string());
    args.push(stops.timeout.to_string());
    // `--` goes LAST: bridges treat everything after it as program args, so
    // an earlier `--` would swallow --break/--timeout (caught live in N3:
    // program args + stops silently dropped the stops on all adapters).
    if !program_args.is_empty() {
        args.push("--".to_string());
        args.extend(program_args.iter().cloned());
    }

    // Spawn-time intent for resume-without-memory: armed stops + target,
    // derived automatically (the agent writes nothing by hand). v2 markers:
    // every sidecar object carries schemaVersion 2.
    let intent = json!({
        "schemaVersion": session::SCHEMA_VERSION,
        "breaks": stops.breakpoints,
        "logpoints": stops.logpoints,
        "watches": stops.watches,
        "exits": stops.exits,
        "sources": stops.source_paths,
        "timeout": stops.timeout,
        "target": session::target_summary(&args),
        "requestedTarget": requested.clone(),
    });

    session::spawn(
        session,
        &session::SpawnSpec {
            lang,
            kind,
            bridge_args: args,
            wait_secs: stops.timeout.saturating_add(10),
            stops: intent,
            requested,
            target_identity: seed,
            identity_hint: seed_hint,
        },
    )
}

/// Module names run as `python -m`: dotted ASCII identifiers validated
/// before the target runs (each segment `[A-Za-z_][A-Za-z0-9_]*`).
fn check_module(module: &str) -> anyhow::Result<()> {
    let ok = !module.is_empty()
        && module.split('.').all(|seg| {
            let mut chars = seg.chars();
            matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
    if ok {
        Ok(())
    } else {
        anyhow::bail!("bad --module '{module}' (want dotted.name like mypkg.mod)")
    }
}

/// WHAT the user asked to attach/launch (endpoint + CLI flags). Never an
/// observation: `pid` stays null (v1 has no pid input; reserved), and the
/// requested value is never presented as observed.
fn requested_target(target: &Target<'_>, kind: &str) -> Value {
    let _ = kind;
    match target {
        Target::JavaLaunch { main, classpath } => json!({
            "main": main,
            "classpath": classpath.unwrap_or("."),
            "pid": Value::Null,
        }),
        Target::JavaAttach { host, port } => json!({
            "host": host, "port": port, "pid": Value::Null,
        }),
        Target::PyLaunch {
            program,
            module,
            python,
            subprocess,
        } => {
            let mut m = serde_json::Map::new();
            if let Some(p) = program {
                m.insert("program".to_string(), Value::String(p.to_string()));
            }
            if let Some(mo) = module {
                m.insert("module".to_string(), Value::String(mo.to_string()));
            }
            if let Some(py) = python {
                m.insert("python".to_string(), Value::String(py.to_string()));
            }
            if *subprocess {
                m.insert("subprocess".to_string(), Value::Bool(true));
            }
            m.insert("pid".to_string(), Value::Null);
            Value::Object(m)
        }
        Target::PyAttach { host, port } => json!({
            "host": host, "port": port, "pid": Value::Null,
        }),
        Target::NodeLaunch {
            program,
            node,
            workers,
        } => {
            let mut m = serde_json::Map::new();
            m.insert("program".to_string(), Value::String(program.to_string()));
            if let Some(n) = node {
                m.insert("node".to_string(), Value::String(n.to_string()));
            }
            if *workers {
                m.insert("workers".to_string(), Value::Bool(true));
            }
            m.insert("pid".to_string(), Value::Null);
            Value::Object(m)
        }
        Target::NodeAttach { host, port } => json!({
            "host": host, "port": port, "pid": Value::Null,
        }),
        Target::BrowserAttach { host, port, tab } => json!({
            "host": host, "port": port,
            "tab": tab.map(|t| Value::String(t.to_string())).unwrap_or(Value::Null),
            "pid": Value::Null,
        }),
    }
}

/// Layered seed identity for a spawn, computed CLI-side from bounded
/// OS-native sources (never target eval, never env): launch argv/cwd from
/// our own spawn, attach endpoint via the localhost port lookup. Browser
/// builds its own tab identity from `/json/list` (seed null here).
fn seed_target_identity(target: &Target<'_>) -> Value {
    match target {
        Target::PyLaunch {
            program,
            module,
            python,
            subprocess: _,
        } => {
            let exe = python.unwrap_or("python3");
            let mut argv = vec![exe.to_string()];
            match (program, module) {
                (Some(p), _) => argv.push(p.to_string()),
                (_, Some(m)) => {
                    argv.push("-m".to_string());
                    argv.push(m.to_string());
                }
                _ => {}
            }
            session::launch_seed(exe, argv)
        }
        Target::NodeLaunch {
            program,
            node,
            workers: _,
        } => session::launch_seed(
            node.unwrap_or("node"),
            vec![node.unwrap_or("node").to_string(), program.to_string()],
        ),
        Target::JavaLaunch { main, classpath } => session::launch_seed(
            "java",
            vec![
                "java".to_string(),
                "-cp".to_string(),
                classpath.unwrap_or(".").to_string(),
                main.to_string(),
            ],
        ),
        Target::PyAttach { host, port }
        | Target::NodeAttach { host, port }
        | Target::JavaAttach { host, port } => session::attach_seed(host, *port),
        Target::BrowserAttach { .. } => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_names_validate_before_target_runs() {
        for ok in ["mod", "mypkg.mod", "_a.b2.C3", "pytest"] {
            assert!(check_module(ok).is_ok(), "{ok}");
        }
        for bad in [
            "", ".mod", "mod.", "a..b", "a-b", "a b", "9lives", "mod/sub", "mödule",
        ] {
            assert!(check_module(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn requested_target_never_carries_a_pid() {
        let v = requested_target(
            &Target::PyAttach {
                host: "localhost",
                port: 5678,
            },
            "attach",
        );
        assert_eq!(v["port"], json!(5678));
        assert!(v["pid"].is_null());
        let v = requested_target(
            &Target::PyLaunch {
                program: None,
                module: Some("m"),
                python: None,
                subprocess: false,
            },
            "launch",
        );
        assert_eq!(v["module"], json!("m"));
        assert!(v.get("program").is_none());
        assert!(v["pid"].is_null());
        let v = requested_target(
            &Target::BrowserAttach {
                host: "h",
                port: 9222,
                tab: Some("shop"),
            },
            "attach",
        );
        assert_eq!(v["tab"], json!("shop"));
    }

    #[test]
    fn requested_target_omits_unset_optionals() {
        // Omitted selectors stay absent, never fabricated defaults
        // (architecture-map §5.3): no --node means no "node" key.
        let v = requested_target(
            &Target::NodeLaunch {
                program: "app.js",
                node: None,
                workers: false,
            },
            "launch",
        );
        assert_eq!(v["program"], json!("app.js"));
        assert!(
            v.get("node").is_none(),
            "unset --node must stay absent: {v}"
        );
        assert!(v.get("workers").is_none());
        assert!(v["pid"].is_null());
        let v = requested_target(
            &Target::NodeLaunch {
                program: "app.js",
                node: Some("/usr/local/bin/node"),
                workers: true,
            },
            "launch",
        );
        assert_eq!(v["node"], json!("/usr/local/bin/node"));
        assert_eq!(v["workers"], json!(true));
    }
}
