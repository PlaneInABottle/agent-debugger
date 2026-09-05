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
        program: &'a str,
        python: Option<&'a str>,
    },
    PyAttach {
        host: &'a str,
        port: u16,
    },
    NodeLaunch {
        program: &'a str,
        node: Option<&'a str>,
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
        Target::PyLaunch { program, python } => {
            args.push("--program".to_string());
            args.push(program.to_string());
            if let Some(py) = python {
                args.push("--python".to_string());
                args.push(py.to_string());
            }
        }
        Target::PyAttach { host, port } => {
            args.push("--host".to_string());
            args.push(host.to_string());
            args.push("--port".to_string());
            args.push(port.to_string());
        }
        Target::NodeLaunch { program, node } => {
            args.push("--program".to_string());
            args.push(program.to_string());
            if let Some(n) = node {
                args.push("--node".to_string());
                args.push(n.to_string());
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
    // derived automatically (the agent writes nothing by hand).
    let intent = json!({
        "breaks": stops.breakpoints,
        "logpoints": stops.logpoints,
        "watches": stops.watches,
        "exits": stops.exits,
        "sources": stops.source_paths,
        "timeout": stops.timeout,
        "target": session::target_summary(&args),
    });

    session::spawn(
        session,
        &session::SpawnSpec {
            lang,
            kind,
            bridge_args: args,
            wait_secs: stops.timeout.saturating_add(10),
            stops: intent,
        },
    )
}
