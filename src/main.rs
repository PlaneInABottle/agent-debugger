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
                module,
                python,
                subprocess,
                stops,
                program_args,
            } => (
                "start",
                spawn::cmd_spawn(
                    session,
                    "py",
                    "launch",
                    spawn::Target::PyLaunch {
                        program: program.as_deref(),
                        module: module.as_deref(),
                        python: python.as_deref(),
                        subprocess,
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
                workers,
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
                        workers,
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
        cli::Commands::Continue { timeout, target } => (
            "continue",
            session::forward_target(
                session,
                &json!({"cmd": "continue", "timeout": timeout}),
                Duration::from_secs(timeout.saturating_add(5)),
                target.as_deref(),
            ),
        ),
        cli::Commands::Wait { timeout, target } => (
            "wait",
            session::forward_target(
                session,
                &json!({"cmd": "wait", "timeout": timeout}),
                Duration::from_secs(timeout.saturating_add(5)),
                target.as_deref(),
            ),
        ),
        cli::Commands::Capture {
            timeout,
            pause_budget,
            target,
            break_spec,
            frames,
            vars,
        } => {
            let mut body = json!({
                "cmd": "capture",
                "timeout": timeout,
                "pauseBudgetMs": pause_budget,
                "frames": frames,
                "vars": vars,
            });
            if let Some(spec) = break_spec.as_deref() {
                body["break"] = Value::String(spec.to_string());
            }
            (
                "capture",
                session::forward_target(
                    session,
                    &body,
                    Duration::from_secs(timeout.saturating_add(5)),
                    target.as_deref(),
                ),
            )
        }
        cli::Commands::Step {
            mode,
            timeout,
            target,
        } => (
            "step",
            session::forward_target(
                session,
                &json!({"cmd": "step", "mode": mode, "timeout": timeout}),
                Duration::from_secs(timeout.saturating_add(5)),
                target.as_deref(),
            ),
        ),
        cli::Commands::Reload { timeout } => ("reload", session::cmd_reload(session, timeout)),
        cli::Commands::Context { target } => (
            "context",
            session::cmd_context_target(session, target.as_deref()),
        ),
        cli::Commands::Stack { target } => (
            "stack",
            session::forward_target(
                session,
                &json!({"cmd": "stack"}),
                Duration::from_secs(10),
                target.as_deref(),
            ),
        ),
        cli::Commands::Threads { target } => (
            "threads",
            session::forward_target(
                session,
                &json!({"cmd": "threads"}),
                Duration::from_secs(15),
                target.as_deref(),
            ),
        ),
        cli::Commands::Targets => ("targets", session::cmd_targets(session)),
        cli::Commands::Breaks { cmd } => match cmd {
            None => (
                "breaks",
                session::forward(session, &json!({"cmd": "breaks"}), Duration::from_secs(10)),
            ),
            Some(cli::BreaksCmd::Add { breaks, target }) => (
                "breaks",
                session::cmd_breaks_add(session, &breaks, target.as_deref()),
            ),
            Some(cli::BreaksCmd::Remove { breaks, target }) => (
                "breaks",
                session::cmd_breaks_remove(session, &breaks, target.as_deref()),
            ),
            Some(cli::BreaksCmd::Clear { target }) => (
                "breaks",
                session::cmd_breaks_clear(session, target.as_deref()),
            ),
        },
        cli::Commands::Vars { frame, target } => (
            "vars",
            session::forward_target(
                session,
                &json!({"cmd": "vars", "frame": frame}),
                Duration::from_secs(10),
                target.as_deref(),
            ),
        ),
        cli::Commands::Eval {
            expression,
            frame,
            target,
        } => (
            "eval",
            session::forward_target(
                session,
                &json!({"cmd": "eval", "expr": expression, "frame": frame}),
                Duration::from_secs(15),
                target.as_deref(),
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
