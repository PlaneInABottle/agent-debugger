// agent-debugger: agent-first CLI debugger. Java + Python + Node.

mod adapter;
mod bridge;
mod client;
mod dap;
mod fmt;
mod output;
mod session;

use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};
use std::time::Duration;

/// Agent-first CLI debugger. Java + Python + Node over one session protocol.
///
/// Default output is a stable JSON envelope for agents.
/// Pass `--human` for pretty human-readable output.
/// Sessions persist across invocations; pick one with `--session`.
#[derive(Parser, Debug)]
#[command(name = "agent-debugger", version, about)]
struct Cli {
    /// Human-readable output instead of the JSON envelope.
    #[arg(long, global = true, default_value_t = false)]
    human: bool,

    /// Session name (all session commands operate on this).
    #[arg(long, global = true, default_value = "default")]
    session: String,

    #[command(subcommand)]
    command: Commands,
}

/// Stop specifications shared by every language: breakpoints, logpoints,
/// watchpoints, exits, sources and the first-stop wait.
#[derive(Args, Debug)]
struct Stops {
    /// Source roots for snippet mapping (repeatable for multi-module builds).
    #[arg(long, alias = "src")]
    source_paths: Vec<String>,
    /// Breakpoints: Class:line, method:Class.method, exc:ExcClass (py: file:line).
    /// Append `|cond` for a condition, e.g. "com.Foo:54|order == null".
    #[arg(long, alias = "break")]
    breakpoints: Vec<String>,
    /// Logpoints (never stop): "Class:line:template with {expr} holes".
    #[arg(long, alias = "logpoint")]
    logpoints: Vec<String>,
    /// Field watchpoints: "Class.field" (write) or "Class.field:read".
    /// Stops when the field is read/written — answers "who changed this?".
    /// Java only for now (Python fails fast with guidance).
    #[arg(long, alias = "watch")]
    watches: Vec<String>,
    /// Method exits: "Class.method". Stops at return, capturing the value.
    /// Java only for now (Python fails fast with guidance).
    #[arg(long, alias = "exit")]
    exits: Vec<String>,
    /// Seconds to wait for the first breakpoint hit.
    #[arg(long, default_value_t = 20)]
    timeout: u64,
}

#[derive(Subcommand, Debug)]
enum JavaCmd {
    /// Launch a JVM under the debugger and keep a persistent session.
    Start {
        /// Target main class, e.g. com.acme.BillingApp.
        #[arg(long)]
        main: String,
        /// Classpath for the target (e.g. target/classes). Defaults to ".".
        #[arg(long, alias = "cp")]
        classpath: Option<String>,
        #[command(flatten)]
        stops: Stops,
        /// Args forwarded to the target program (after `--`).
        #[arg(last = true)]
        program_args: Vec<String>,
    },
    /// Attach to a running JVM and keep a persistent session (no stop needed).
    Attach {
        /// JDWP port the target JVM listens on
        /// (start it with -agentlib:jdwp=transport=dt_socket,server=y,address=*:PORT).
        #[arg(long, default_value_t = 5005)]
        port: u16,
        #[arg(long, default_value = "localhost")]
        host: String,
        #[command(flatten)]
        stops: Stops,
    },
}

#[derive(Subcommand, Debug)]
enum PyCmd {
    /// Launch a Python program under debugpy (isolated venv, auto-provisioned).
    Start {
        /// Script file to debug, e.g. app.py.
        program: String,
        /// Python interpreter for adapter+target (must have debugpy).
        /// Defaults to the isolated venv, else system python3.
        #[arg(long)]
        python: Option<String>,
        #[command(flatten)]
        stops: Stops,
        /// Args forwarded to the target program (after `--`).
        #[arg(last = true)]
        program_args: Vec<String>,
    },
    /// Attach to a Python process running `debugpy --listen`.
    Attach {
        /// debugpy listen port
        /// (start the target with `python -m debugpy --listen PORT app.py`).
        #[arg(long, default_value_t = 5678)]
        port: u16,
        #[arg(long, default_value = "localhost")]
        host: String,
        #[command(flatten)]
        stops: Stops,
    },
}

#[derive(Args, Debug)]
struct JavaGroup {
    #[command(subcommand)]
    cmd: JavaCmd,
}

#[derive(Args, Debug)]
struct PyGroup {
    #[command(subcommand)]
    cmd: PyCmd,
}

#[derive(Subcommand, Debug)]
enum NodeCmd {
    /// Launch a Node program under the debugger (CDP via --inspect).
    Start {
        /// Script file to debug, e.g. app.js.
        program: String,
        /// Node binary for the target (bridge itself runs on PATH node).
        #[arg(long)]
        node: Option<String>,
        #[command(flatten)]
        stops: Stops,
        /// Args forwarded to the target program (after `--`).
        #[arg(last = true)]
        program_args: Vec<String>,
    },
    /// Attach to a Node process running with `--inspect`.
    Attach {
        /// Inspector port
        /// (start the target with `node --inspect=PORT app.js`).
        #[arg(long, default_value_t = 9229)]
        port: u16,
        #[arg(long, default_value = "localhost")]
        host: String,
        #[command(flatten)]
        stops: Stops,
    },
}

#[derive(Args, Debug)]
struct NodeGroup {
    #[command(subcommand)]
    cmd: NodeCmd,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Java targets (embedded JDI bridge, zero setup).
    Java(JavaGroup),
    /// Python targets (debugpy, venv auto-provisioned).
    Py(PyGroup),
    /// Node targets (nodebridge + CDP).
    Node(NodeGroup),
    /// Resume until the next breakpoint (or timeout / exit).
    #[command(name = "continue")]
    Continue {
        /// Seconds to wait for the next stop.
        #[arg(long, default_value_t = 20)]
        timeout: u64,
    },
    /// Step one line: over | into | out.
    Step {
        /// over = next line, into = descend into call, out = run to caller.
        #[arg(value_parser = ["over", "into", "out"], default_value = "over")]
        mode: String,
        /// Seconds to wait for the step to land.
        #[arg(long, default_value_t = 20)]
        timeout: u64,
    },
    /// Re-fetch location + threads + frames without resuming.
    Context,
    /// Evaluate a path/call expression in a frame (e.g. `orders.size()`).
    Eval {
        /// Path/call expression, e.g. orders.size(), a.b[2].c, refs(order,2).
        expression: String,
        /// Stack frame index (0 = top).
        #[arg(long, default_value_t = 0)]
        frame: usize,
    },
    /// List locals in a frame.
    Vars {
        /// Stack frame index (0 = top).
        #[arg(long, default_value_t = 0)]
        frame: usize,
    },
    /// Show the call stack (frames without locals).
    Stack,
    /// Instant thread dump (all threads + top frames, VM keeps running).
    Threads,
    /// Show collected logpoint lines.
    Logs {
        /// How many trailing lines to return (max 500).
        #[arg(long, default_value_t = 50)]
        tail: usize,
    },
    /// Show CLI version and known sessions.
    Status,
    /// Disconnect and delete the session.
    Close,
    /// Check toolchains for current and future adapters.
    Doctor,
}

fn main() {
    let cli = Cli::parse();
    let session = cli.session.clone();
    let (name, result) = dispatch(&session, cli.command);
    std::process::exit(output::emit(name, result, cli.human));
}

fn dispatch(session: &str, cmd: Commands) -> (&'static str, anyhow::Result<Value>) {
    match cmd {
        Commands::Status => ("status", Ok(session::status())),
        Commands::Doctor => ("doctor", doctor()),
        Commands::Close => ("close", session::close(session)),
        Commands::Java(group) => match group.cmd {
            JavaCmd::Start {
                main,
                classpath,
                stops,
                program_args,
            } => (
                "start",
                cmd_spawn(
                    session,
                    "java",
                    "launch",
                    Target::JavaLaunch {
                        main: &main,
                        classpath: classpath.as_deref(),
                    },
                    &stops,
                    &program_args,
                ),
            ),
            JavaCmd::Attach { port, host, stops } => (
                "attach",
                cmd_spawn(
                    session,
                    "java",
                    "attach",
                    Target::JavaAttach { host: &host, port },
                    &stops,
                    &[],
                ),
            ),
        },
        Commands::Py(group) => match group.cmd {
            PyCmd::Start {
                program,
                python,
                stops,
                program_args,
            } => (
                "start",
                cmd_spawn(
                    session,
                    "py",
                    "launch",
                    Target::PyLaunch {
                        program: &program,
                        python: python.as_deref(),
                    },
                    &stops,
                    &program_args,
                ),
            ),
            PyCmd::Attach { port, host, stops } => (
                "attach",
                cmd_spawn(
                    session,
                    "py",
                    "attach",
                    Target::PyAttach { host: &host, port },
                    &stops,
                    &[],
                ),
            ),
        },
        Commands::Node(group) => match group.cmd {
            NodeCmd::Start {
                program,
                node,
                stops,
                program_args,
            } => (
                "start",
                cmd_spawn(
                    session,
                    "node",
                    "launch",
                    Target::NodeLaunch {
                        program: &program,
                        node: node.as_deref(),
                    },
                    &stops,
                    &program_args,
                ),
            ),
            NodeCmd::Attach { port, host, stops } => (
                "attach",
                cmd_spawn(
                    session,
                    "node",
                    "attach",
                    Target::NodeAttach { host: &host, port },
                    &stops,
                    &[],
                ),
            ),
        },
        Commands::Continue { timeout } => (
            "continue",
            session::forward(
                session,
                &json!({"cmd": "continue", "timeout": timeout}),
                Duration::from_secs(timeout + 5),
            ),
        ),
        Commands::Step { mode, timeout } => (
            "step",
            session::forward(
                session,
                &json!({"cmd": "step", "mode": mode, "timeout": timeout}),
                Duration::from_secs(timeout + 5),
            ),
        ),
        Commands::Context => (
            "context",
            session::forward(session, &json!({"cmd": "context"}), Duration::from_secs(10)),
        ),
        Commands::Stack => (
            "stack",
            session::forward(session, &json!({"cmd": "stack"}), Duration::from_secs(10)),
        ),
        Commands::Threads => (
            "threads",
            session::forward(session, &json!({"cmd": "threads"}), Duration::from_secs(15)),
        ),
        Commands::Vars { frame } => (
            "vars",
            session::forward(
                session,
                &json!({"cmd": "vars", "frame": frame}),
                Duration::from_secs(10),
            ),
        ),
        Commands::Eval { expression, frame } => (
            "eval",
            session::forward(
                session,
                &json!({"cmd": "eval", "expr": expression, "frame": frame}),
                Duration::from_secs(15),
            ),
        ),
        Commands::Logs { tail } => (
            "logs",
            session::forward(
                session,
                &json!({"cmd": "logs", "tail": tail}),
                Duration::from_secs(10),
            ),
        ),
    }
}

/// Language-specific half of a spawn: how to tell the adapter WHAT to run.
/// Everything else (breaks, sources, timeout) is shared via [`Stops`].
enum Target<'a> {
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
}

fn cmd_spawn(
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

    session::spawn(
        session,
        &session::SpawnSpec {
            lang,
            kind,
            bridge_args: args,
            wait_secs: stops.timeout + 10,
        },
    )
}

fn doctor() -> anyhow::Result<Value> {
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
        "adapters": {
            "java": {"via": "embedded JDI bridge (persistent session)", "ready": true},
            "python": {"via": "embedded pybridge + debugpy (isolated venv)", "ready": debugpy["found"]},
            "node": {"via": "embedded nodebridge + CDP", "ready": node["found"]},
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
