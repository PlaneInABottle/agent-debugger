// agent-debugger: agent-first CLI debugger. Java + Python + Node + Browser.

mod bridge;
mod client;
mod dap;
mod doctor;
mod fmt;
mod output;
mod session;
mod spawn;

use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};
use std::time::Duration;

/// Agent-first CLI debugger. Java + Python + Node + Browser over one session protocol.
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
    /// Breakpoints: Class:line, method:Class.method, exc:ExcClass
    /// (py: file:line; node: file:line; browser: url-frag:line).
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
enum BrowserCmd {
    /// Attach to a browser tab via CDP and keep a persistent session.
    Attach {
        /// Tab selector: substring of tab url or title (first page if omitted).
        #[arg(long)]
        tab: Option<String>,
        /// CDP port (start Chrome with --remote-debugging-port=PORT).
        /// Default 9222 = the agent-browser convention, so interaction and
        /// debugging share one warm browser instead of two.
        #[arg(long, default_value_t = 9222)]
        port: u16,
        #[arg(long, default_value = "localhost")]
        host: String,
        #[command(flatten)]
        stops: Stops,
    },
}

#[derive(Args, Debug)]
struct BrowserGroup {
    #[command(subcommand)]
    cmd: BrowserCmd,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Java targets (embedded JDI bridge, zero setup).
    Java(JavaGroup),
    /// Python targets (debugpy, venv auto-provisioned).
    Py(PyGroup),
    /// Node targets (nodebridge + CDP).
    Node(NodeGroup),
    /// Browser tabs (browserbridge + CDP).
    Browser(BrowserGroup),
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
    /// Reload the page and wait for the next stop (browser tabs only).
    /// The agent-side trigger for load-path code; interaction-path code
    /// still needs a human click or agent-browser.
    Reload {
        /// Seconds to wait for the next stop after reload.
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
    /// List armed stops with plant state (verified/pending/slid/shadowed).
    /// No stop required — answers "which breakpoints do I have?" after
    /// compaction without reading bridge logs.
    Breaks,
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
        Commands::Doctor => ("doctor", doctor::doctor()),
        Commands::Close => ("close", session::close(session)),
        Commands::Java(group) => match group.cmd {
            JavaCmd::Start {
                main,
                classpath,
                stops,
                program_args,
            } => (
                "start",
                spawn::cmd_spawn(
                    session,
                    "java",
                    "launch",
                    spawn::Target::JavaLaunch {
                        main: &main,
                        classpath: classpath.as_deref(),
                    },
                    &stops,
                    &program_args,
                ),
            ),
            JavaCmd::Attach { port, host, stops } => (
                "attach",
                spawn::cmd_spawn(
                    session,
                    "java",
                    "attach",
                    spawn::Target::JavaAttach { host: &host, port },
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
                spawn::cmd_spawn(
                    session,
                    "py",
                    "launch",
                    spawn::Target::PyLaunch {
                        program: &program,
                        python: python.as_deref(),
                    },
                    &stops,
                    &program_args,
                ),
            ),
            PyCmd::Attach { port, host, stops } => (
                "attach",
                spawn::cmd_spawn(
                    session,
                    "py",
                    "attach",
                    spawn::Target::PyAttach { host: &host, port },
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
                spawn::cmd_spawn(
                    session,
                    "node",
                    "launch",
                    spawn::Target::NodeLaunch {
                        program: &program,
                        node: node.as_deref(),
                    },
                    &stops,
                    &program_args,
                ),
            ),
            NodeCmd::Attach { port, host, stops } => (
                "attach",
                spawn::cmd_spawn(
                    session,
                    "node",
                    "attach",
                    spawn::Target::NodeAttach { host: &host, port },
                    &stops,
                    &[],
                ),
            ),
        },
        Commands::Browser(group) => match group.cmd {
            BrowserCmd::Attach {
                tab,
                port,
                host,
                stops,
            } => (
                "attach",
                spawn::cmd_spawn(
                    session,
                    "browser",
                    "attach",
                    spawn::Target::BrowserAttach {
                        host: &host,
                        port,
                        tab: tab.as_deref(),
                    },
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
        Commands::Reload { timeout } => (
            "reload",
            session::forward(
                session,
                &json!({"cmd": "reload", "timeout": timeout}),
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
        Commands::Breaks => (
            "breaks",
            session::forward(session, &json!({"cmd": "breaks"}), Duration::from_secs(10)),
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
