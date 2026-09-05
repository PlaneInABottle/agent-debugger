// agent-debugger: agent-first CLI debugger. Java + Python + Node + Browser.

mod bridge;
mod cli;
mod client;
mod dap;
mod doctor;
mod fmt;
mod output;
mod session;
mod spawn;

use clap::Parser;
use serde_json::{json, Value};
use std::time::Duration;

fn main() {
    let cli = cli::Cli::parse();
    let session = cli.session.clone();
    let (name, result) = dispatch(&session, cli.command);
    std::process::exit(output::emit(name, result, cli.human));
}

fn dispatch(session: &str, cmd: cli::Commands) -> (&'static str, anyhow::Result<Value>) {
    match cmd {
        cli::Commands::Status => ("status", Ok(session::status())),
        cli::Commands::Doctor => ("doctor", doctor::doctor()),
        cli::Commands::Close => ("close", session::close(session)),
        cli::Commands::Java(group) => match group.cmd {
            cli::JavaCmd::Start {
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
            cli::JavaCmd::Attach { port, host, stops } => (
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
        cli::Commands::Py(group) => match group.cmd {
            cli::PyCmd::Start {
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
            cli::PyCmd::Attach { port, host, stops } => (
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
        cli::Commands::Node(group) => match group.cmd {
            cli::NodeCmd::Start {
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
            cli::NodeCmd::Attach { port, host, stops } => (
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
        cli::Commands::Browser(group) => match group.cmd {
            cli::BrowserCmd::Attach {
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
        cli::Commands::Continue { timeout } => (
            "continue",
            session::forward(
                session,
                &json!({"cmd": "continue", "timeout": timeout}),
                Duration::from_secs(timeout + 5),
            ),
        ),
        cli::Commands::Step { mode, timeout } => (
            "step",
            session::forward(
                session,
                &json!({"cmd": "step", "mode": mode, "timeout": timeout}),
                Duration::from_secs(timeout + 5),
            ),
        ),
        cli::Commands::Reload { timeout } => (
            "reload",
            session::forward(
                session,
                &json!({"cmd": "reload", "timeout": timeout}),
                Duration::from_secs(timeout + 5),
            ),
        ),
        cli::Commands::Context => (
            "context",
            session::forward(session, &json!({"cmd": "context"}), Duration::from_secs(10)),
        ),
        cli::Commands::Stack => (
            "stack",
            session::forward(session, &json!({"cmd": "stack"}), Duration::from_secs(10)),
        ),
        cli::Commands::Threads => (
            "threads",
            session::forward(session, &json!({"cmd": "threads"}), Duration::from_secs(15)),
        ),
        cli::Commands::Breaks => (
            "breaks",
            session::forward(session, &json!({"cmd": "breaks"}), Duration::from_secs(10)),
        ),
        cli::Commands::Vars { frame } => (
            "vars",
            session::forward(
                session,
                &json!({"cmd": "vars", "frame": frame}),
                Duration::from_secs(10),
            ),
        ),
        cli::Commands::Eval { expression, frame } => (
            "eval",
            session::forward(
                session,
                &json!({"cmd": "eval", "expr": expression, "frame": frame}),
                Duration::from_secs(15),
            ),
        ),
        cli::Commands::Logs { tail } => (
            "logs",
            session::forward(
                session,
                &json!({"cmd": "logs", "tail": tail}),
                Duration::from_secs(10),
            ),
        ),
    }
}
