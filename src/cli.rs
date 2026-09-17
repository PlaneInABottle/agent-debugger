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
        /// Exactly one of positional PROGRAM and --module is required.
        program: Option<String>,
        /// Dotted module to run as `python -m`, e.g. mypkg.mod or pytest.
        /// Mutually exclusive with positional PROGRAM.
        #[arg(long, conflicts_with = "program")]
        module: Option<String>,
        /// Python interpreter for adapter+target (must have debugpy).
        /// Defaults to the isolated venv, else system python3.
        #[arg(long)]
        python: Option<String>,
        /// Debug subprocess/multiprocessing children as extra targets
        /// (opt-in; default is main-only). Child breakpoints inherit the
        /// global intent; `targets` lists the roster.
        #[arg(long, default_value_t = false)]
        subprocess: bool,
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
        /// Follow worker_threads as extra targets (opt-in; default is
        /// main-only). Worker breakpoints inherit the global intent;
        /// `targets` lists the roster.
        #[arg(long, default_value_t = false)]
        workers: bool,
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
        /// Target id (e.g. child:123, worker:abc). Omit for auto-select
        /// (most recent stopped live target, else main).
        #[arg(long)]
        target: Option<String>,
    },
    /// Wait for the next breakpoint stop WITHOUT resuming (pure long-poll).
    /// Immediate success if the selected target is already parked; otherwise
    /// waits for the next fresh stop. Never resumes. Timeout preserves the
    /// session and intents (typed `timeout: no stop within Ns; <hint>`).
    Wait {
        /// Seconds to wait for the next stop.
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..=3600))]
        timeout: u64,
        /// Target id (e.g. child:123, worker:abc). Omit for auto-select
        /// (already-parked target first, else the first fresh stop on any
        /// target, stamped in the response).
        #[arg(long)]
        target: Option<String>,
    },
    /// One-shot bounded stop: optionally plant an ephemeral line break, wait
    /// for the park, collect a bounded snapshot, remove the ephemeral break
    /// BEFORE resuming, then auto-resume within the pause budget. If the
    /// target is already parked at entry the snapshot is collected WITHOUT
    /// resuming (targetWasPaused:true, resumed:false). Never leaves a fresh
    /// command-caused park suspended (collection/remove failure still
    /// resumes; timeout never resumes). Capture carries no eval expression
    /// and captured locals live only in the response.
    Capture {
        /// Seconds to wait for the park.
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..=3600))]
        timeout: u64,
        /// Max milliseconds the fresh park may stay suspended before the
        /// auto-resume (budget overrun still resumes, then reports).
        #[arg(long = "pause-budget", default_value_t = 2000, value_parser = clap::value_parser!(u64).range(1..=10000))]
        pause_budget: u64,
        /// Target id. Omit for auto-select.
        #[arg(long)]
        target: Option<String>,
        /// Optional one-shot line break, e.g. app.py:42|x > 1. Line-only
        /// (same parser as `breaks add`), target-scoped, ephemeral: never
        /// persisted to stops.json, never inherited.
        #[arg(long = "break")]
        break_spec: Option<String>,
        /// Frames to include in the snapshot (1..=10).
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..=10))]
        frames: u64,
        /// Locals to include for frame 0 (1..=20).
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..=20))]
        vars: u64,
    },
    /// Step one line: over | into | out.
    Step {
        /// over = next line, into = descend into call, out = run to caller.
        #[arg(value_parser = ["over", "into", "out"], default_value = "over")]
        mode: String,
        /// Seconds to wait for the step to land.
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..=3600))]
        timeout: u64,
        /// Target id. Omit for auto-select.
        #[arg(long)]
        target: Option<String>,
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
    Context {
        /// Target id. Omit for auto-select.
        #[arg(long)]
        target: Option<String>,
    },
    /// Evaluate a path/call expression in a frame (e.g. `orders.size()`).
    Eval {
        /// Path/call expression, e.g. orders.size(), a.b[2].c, refs(order,2).
        expression: String,
        /// Stack frame index (0 = top).
        #[arg(long, default_value_t = 0)]
        frame: usize,
        /// Target id. Omit for auto-select.
        #[arg(long)]
        target: Option<String>,
    },
    /// List locals in a frame.
    Vars {
        /// Stack frame index (0 = top).
        #[arg(long, default_value_t = 0)]
        frame: usize,
        /// Target id. Omit for auto-select.
        #[arg(long)]
        target: Option<String>,
    },
    /// Show the call stack (frames without locals).
    Stack {
        /// Target id. Omit for auto-select.
        #[arg(long)]
        target: Option<String>,
    },
    /// Instant thread dump (all threads + top frames, VM keeps running).
    Threads {
        /// Target id. Omit for auto-select (bare threads aggregates
        /// main + live targets on multi-target bridges).
        #[arg(long)]
        target: Option<String>,
    },
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
        /// How many trailing lines to return (clamped to 500 by every bridge).
        #[arg(long, default_value_t = 50)]
        tail: usize,
    },
    /// Show CLI version and known sessions.
    Status,
    /// List debug targets in this session (main + child/worker roster).
    /// Response carries targets[{id,kind,pid,state,lastStop,observed,scope}]
    /// plus selected/ignored/droppedExited counters.
    Targets,
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
        /// Target id for an ephemeral target-scoped break (no stops.json
        /// persistence, no inheritance). Omit for the global intent.
        #[arg(long)]
        target: Option<String>,
    },
    /// Remove live line breakpoints by spec (running or parked).
    /// Matches stored identity without requiring the source to still
    /// exist; only confirmed removals drop from stops.json.
    Remove {
        /// Line breakpoint specs to remove, e.g. app.py:42|x > 1.
        /// Line breaks only (no method:/exc:/logpoint/watch/exit).
        #[arg(long = "break", alias = "breakpoint", required = true)]
        breaks: Vec<String>,
        /// Target id: only that target's target-scoped records match.
        /// Omit to drop the global intent plus its inherited copies.
        #[arg(long)]
        target: Option<String>,
    },
    /// Drop all live line breakpoints (logpoints/watches/exits untouched).
    /// Takes no extra args. Only confirmed removals drop from stops.json.
    Clear {
        /// Target id: drop only that target's target-scoped records.
        /// Omit for the full line-break reset (global intent + all copies
        /// + ephemeral target records).
        #[arg(long)]
        target: Option<String>,
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
            vec!["wait"],
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
    fn wait_and_capture_bounds_hold() {
        // wait: timeout 1..=3600, optional target.
        assert!(Cli::try_parse_from(["agent-debugger", "wait"]).is_ok());
        assert!(Cli::try_parse_from(["agent-debugger", "wait", "--target", "child:1"]).is_ok());
        for timeout in ["0", "3601"] {
            assert!(Cli::try_parse_from(["agent-debugger", "wait", "--timeout", timeout]).is_err());
            assert!(
                Cli::try_parse_from(["agent-debugger", "capture", "--timeout", timeout]).is_err()
            );
        }
        // capture: timeout 1..=3600, pause-budget 1..=10000, frames 1..=10,
        // vars 1..=20, optional target + optional one-shot --break.
        assert!(Cli::try_parse_from(["agent-debugger", "capture"]).is_ok());
        assert!(Cli::try_parse_from([
            "agent-debugger",
            "capture",
            "--break",
            "a.py:1",
            "--frames",
            "3",
            "--vars",
            "5",
            "--pause-budget",
            "500"
        ])
        .is_ok());
        for args in [
            vec!["agent-debugger", "capture", "--pause-budget", "0"],
            vec!["agent-debugger", "capture", "--pause-budget", "10001"],
            vec!["agent-debugger", "capture", "--frames", "0"],
            vec!["agent-debugger", "capture", "--frames", "11"],
            vec!["agent-debugger", "capture", "--vars", "0"],
            vec!["agent-debugger", "capture", "--vars", "21"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
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

    #[test]
    fn breaks_remove_needs_breaks_and_clear_takes_no_args() {
        assert!(
            Cli::try_parse_from(["agent-debugger", "breaks", "remove", "--break", "a.py:1"])
                .is_ok()
        );
        assert!(Cli::try_parse_from(["agent-debugger", "breaks", "remove"]).is_err());
        assert!(Cli::try_parse_from(["agent-debugger", "breaks", "clear"]).is_ok());
        // Bare clear is line-breaks only: --target scopes it, anything else
        // is rejected.
        assert!(
            Cli::try_parse_from(["agent-debugger", "breaks", "clear", "--target", "child:1"])
                .is_ok()
        );
        for extra in ["--break", "--timeout", "--kind", "a.py:1"] {
            assert!(
                Cli::try_parse_from(["agent-debugger", "breaks", "clear", extra]).is_err(),
                "{extra} must be rejected on breaks clear"
            );
        }
    }

    #[test]
    fn multitarget_flags_and_selectors_parse() {
        assert!(
            Cli::try_parse_from(["agent-debugger", "py", "start", "--subprocess", "app.py"])
                .is_ok()
        );
        assert!(
            Cli::try_parse_from(["agent-debugger", "node", "start", "--workers", "app.js"]).is_ok()
        );
        assert!(Cli::try_parse_from(["agent-debugger", "targets"]).is_ok());
        assert!(Cli::try_parse_from(["agent-debugger", "continue", "--target", "child:1"]).is_ok());
        assert!(
            Cli::try_parse_from(["agent-debugger", "context", "--target", "worker:abc"]).is_ok()
        );
        assert!(Cli::try_parse_from([
            "agent-debugger",
            "breaks",
            "add",
            "--break",
            "a.py:1",
            "--target",
            "child:1"
        ])
        .is_ok());
    }

    #[test]
    fn py_start_takes_exactly_one_of_program_or_module() {
        // File form unchanged.
        assert!(Cli::try_parse_from(["agent-debugger", "py", "start", "app.py"]).is_ok());
        // Module form: --module with no positional.
        assert!(
            Cli::try_parse_from(["agent-debugger", "py", "start", "--module", "mypkg.mod"]).is_ok()
        );
        // Both forms together conflict at parse time.
        assert!(Cli::try_parse_from([
            "agent-debugger",
            "py",
            "start",
            "app.py",
            "--module",
            "mypkg.mod"
        ])
        .is_err());
    }
}
