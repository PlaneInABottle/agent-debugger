//! CLI surface: clap structs for all four languages plus shared
//! stops/commands. Moved verbatim from main.rs; dispatch stays there.

use clap::{Args, Parser, Subcommand};

/// Agent-first CLI debugger. Java + Python + Node + Browser over one session protocol.
///
/// Default output is a stable JSON envelope for agents.
/// Pass `--human` for pretty human-readable output.
/// Sessions persist across invocations; pick one with `--session`.
#[derive(Parser, Debug)]
#[command(name = "agent-debugger", version, about)]
pub(crate) struct Cli {
    /// Human-readable output instead of the JSON envelope.
    #[arg(long, global = true, default_value_t = false)]
    pub(crate) human: bool,

    /// Session name (all session commands operate on this).
    #[arg(long, global = true, default_value = "default")]
    pub(crate) session: String,

    #[command(subcommand)]
    pub(crate) command: Commands,
}

/// Stop specifications shared by every language: breakpoints, logpoints,
/// watchpoints, exits, sources and the first-stop wait.
#[derive(Args, Debug)]
pub(crate) struct Stops {
    /// Source roots for snippet mapping (repeatable for multi-module builds).
    #[arg(long, alias = "src")]
    pub(crate) source_paths: Vec<String>,
    /// Breakpoints: Class:line, method:Class.method, exc:ExcClass
    /// (py: file:line; node: file:line; browser: url-frag:line).
    /// Append `|cond` for a condition, e.g. "com.Foo:54|order == null".
    #[arg(long, alias = "break")]
    pub(crate) breakpoints: Vec<String>,
    /// Logpoints (never stop): "Class:line:template with {expr} holes".
    #[arg(long, alias = "logpoint")]
    pub(crate) logpoints: Vec<String>,
    /// Field watchpoints: "Class.field" (write) or "Class.field:read".
    /// Stops when the field is read/written — answers "who changed this?".
    /// Java only for now (Python fails fast with guidance).
    #[arg(long, alias = "watch")]
    pub(crate) watches: Vec<String>,
    /// Method exits: "Class.method". Stops at return, capturing the value.
    /// Java only for now (Python fails fast with guidance).
    #[arg(long, alias = "exit")]
    pub(crate) exits: Vec<String>,
    /// Seconds to wait for the first breakpoint hit.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub(crate) timeout: u64,
}

#[derive(Subcommand, Debug)]
pub(crate) enum JavaCmd {
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
pub(crate) enum PyCmd {
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
pub(crate) struct JavaGroup {
    #[command(subcommand)]
    pub(crate) cmd: JavaCmd,
}

#[derive(Args, Debug)]
pub(crate) struct PyGroup {
    #[command(subcommand)]
    pub(crate) cmd: PyCmd,
}

#[derive(Subcommand, Debug)]
pub(crate) enum NodeCmd {
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
pub(crate) struct NodeGroup {
    #[command(subcommand)]
    pub(crate) cmd: NodeCmd,
}

#[derive(Subcommand, Debug)]
pub(crate) enum BrowserCmd {
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
pub(crate) struct BrowserGroup {
    #[command(subcommand)]
    pub(crate) cmd: BrowserCmd,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Commands {
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
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..=3600))]
        timeout: u64,
    },
    /// Step one line: over | into | out.
    Step {
        /// over = next line, into = descend into call, out = run to caller.
        #[arg(value_parser = ["over", "into", "out"], default_value = "over")]
        mode: String,
        /// Seconds to wait for the step to land.
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..=3600))]
        timeout: u64,
    },
    /// Reload the page and wait for the next stop (browser tabs only).
    /// The agent-side trigger for load-path code; interaction-path code
    /// still needs a human click or agent-browser.
    Reload {
        /// Seconds to wait for the next stop after reload.
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..=3600))]
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
    /// `breaks add --break SPEC` adds line breakpoints to the live session.
    Breaks {
        #[command(subcommand)]
        cmd: Option<BreaksCmd>,
    },
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

#[derive(Subcommand, Debug)]
pub(crate) enum BreaksCmd {
    /// Add line breakpoints to the live session (running or parked).
    /// Only confirmed additions persist to stops.json; duplicates are
    /// idempotent, invalid batches change nothing.
    Add {
        /// Line breakpoint specs, e.g. com.Foo:54, app.py:42|x > 1.
        /// Line breaks only (no method:/exc:/logpoint/watch/exit).
        #[arg(long = "break", alias = "breakpoint", required = true)]
        breaks: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeouts_are_bounded_for_all_command_shapes() {
        for command in [
            vec!["continue"],
            vec!["step"],
            vec!["reload"],
            vec!["java", "attach"],
            vec!["py", "attach"],
            vec!["node", "attach"],
            vec!["browser", "attach"],
        ] {
            for timeout in ["0", "3601", "18446744073709551615"] {
                let mut args = vec!["agent-debugger"];
                args.extend(command.clone());
                args.extend(["--timeout", timeout]);
                assert!(Cli::try_parse_from(args).is_err());
            }
        }
        assert!(Cli::try_parse_from(["agent-debugger", "continue", "--timeout", "3600"]).is_ok());
    }

    #[test]
    fn breaks_bare_lists_and_add_needs_breaks_only() {
        // Bare `breaks` lists (backward compatible, no subcommand).
        assert!(Cli::try_parse_from(["agent-debugger", "breaks"]).is_ok());
        // Add takes one or more --break specs.
        assert!(
            Cli::try_parse_from(["agent-debugger", "breaks", "add", "--break", "a.py:1"]).is_ok()
        );
        assert!(Cli::try_parse_from([
            "agent-debugger",
            "breaks",
            "add",
            "--break",
            "a.py:1",
            "--break",
            "b.py:2"
        ])
        .is_ok());
        // At least one --break is required.
        assert!(Cli::try_parse_from(["agent-debugger", "breaks", "add"]).is_err());
        // Add takes no timeout/logpoint/watch/exit/src.
        for extra in ["--timeout", "--logpoint", "--watch", "--exit", "--src"] {
            assert!(
                Cli::try_parse_from([
                    "agent-debugger",
                    "breaks",
                    "add",
                    "--break",
                    "a.py:1",
                    extra,
                    "x"
                ])
                .is_err(),
                "{extra} must be rejected on breaks add"
            );
        }
    }
}
