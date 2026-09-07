# agent-debugger

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2021%20edition-orange.svg)](https://www.rust-lang.org/)
[![Status](https://img.shields.io/badge/status-production--ready-green.svg)]()

> **Headless, snapshot-first multi-language debugger for AI agents and terminal workflows.**  
> Supports **Python**, **Node.js**, **Browser (Chrome/CDP)**, and **Java** over a single unified protocol.

---

## Overview

Traditional debuggers are built for human eyes and interactive GUI IDEs. They require complex multi-step handshakes (connect, set breakpoint, resume, query thread, query stack, query locals, evaluate variables), which is slow, noisy, and error-prone for AI agents and automated coding systems.

**`agent-debugger`** changes this paradigm:
- **Zero IDE Required**: Headless single Rust binary. No VS Code, IntelliJ, or browser window needed.
- **Snapshot-First**: Every breakpoint hit returns location, source code snippet, call stack, and top-frame locals in **one single round-trip**.
- **Unified Protocol**: Learn once. Python, Node.js, Chrome tabs, and Java share the exact same JSON response envelope and CLI ergonomics.
- **LLM Compaction Survival**: AI agents frequently lose conversational context during prompt compaction. `agent-debugger` allows an agent to reorient state in three deterministic calls (`status`, `breaks`, `context`).
- **Kernel-Locked Persistence**: Breakpoints and session state are protected by OS-level file locks (`flock`/`LockFileEx`), preventing race conditions and lost updates during concurrent CLI invocations.
- **Human or Agent Modes**: Stable JSON output by default for agents and tools; `--human` flag for pretty, colorized terminal reading.

---

## Supported Runtimes

| Runtime | Engine / Adapter | Launch (`start`) | Attach (`attach`) | Multi-Target Support | Advanced Stops |
|---|---|:---:|:---:|---|---|
| **Python** | `debugpy` (auto-provisioned venv) | Yes | Yes (port 5678) | Subprocesses (`--subprocess`) | Line breaks, Conditions (`\|cond`), Ephemeral |
| **Node.js** | Native CDP via `--inspect` | Yes | Yes (port 9229) | Worker threads (`--workers`) | Line breaks, Conditions (`\|cond`), Ephemeral |
| **Browser** | Chrome DevTools Protocol (CDP) | — | Yes (port 9222) | Page tabs (`--tab <match>`) | Line breaks, Conditions, `reload` trigger |
| **Java** | Embedded JDWP/JDI bridge | Yes | Yes (port 5005) | Multi-threaded JVM | Line breaks, Field watchpoints (`--watch`), Method exits (`--exit`), Logpoints |

---

## Installation

### Prerequisites
- [Rust toolchain](https://rustup.rs/) (1.89+ recommended for stable file locks; tested on 1.94+).
- Installed runtime engines for the languages you want to debug:
  - Python: `python3` (debugpy is automatically installed in an isolated cache).
  - Node.js: `node` (Node 18+).
  - Browser: Google Chrome or Chromium.
  - Java: JDK (Java 17+ recommended) with `javac` and `java` on `PATH`.

### Build & Install from Source

```bash
git clone https://github.com/PlaneInABottle/agent-debugger.git
cd agent-debugger
cargo install --path .
```

Verify your installation:

```bash
agent-debugger --version
agent-debugger doctor
```

`agent-debugger doctor` inspects available system runtimes, tools, ports, and reports environment readiness.

---

## Core Workflow

### 1. Launch or Attach a Target

```bash
# Python: Launch a script with a breakpoint at line 42
agent-debugger --session api py start app.py --break 'app.py:42'

# Node.js: Launch a server with a conditional breakpoint
agent-debugger --session web node start server.js --break 'server.js:87|port > 1024'

# Browser: Attach to an active Chrome instance (default port 9222)
agent-debugger --session frontend browser attach --tab "Dashboard" --break 'app.js:25'

# Java: Attach to a running JVM listening on JDWP port 5005
agent-debugger --session backend java attach --port 5005 --break 'com.acme.OrderService:120'
```

### 2. Snapshot-First Stop Response

When execution hits a stop, `agent-debugger` immediately returns a complete snapshot:

```json
{
  "ok": true,
  "status": "stopped",
  "location": {
    "file": "app.py",
    "line": 42,
    "function": "process_order"
  },
  "snippet": [
    "40: def process_order(order_id, user):",
    "41:     validate_user(user)",
    "42:->   total = calculate_total(order_id)",
    "43:     return total"
  ],
  "threads": [
    { "id": "1", "name": "MainThread", "state": "stopped" }
  ],
  "frames": [
    {
      "index": 0,
      "function": "process_order",
      "file": "app.py",
      "line": 42,
      "locals": {
        "order_id": 1042,
        "user": { "id": "u_99", "tier": "gold" }
      }
    }
  ]
}
```

*Note: You do not need follow-up calls to fetch the stack trace or local variables.*

### 3. Inspect & Step

```bash
# Step over, into, or out
agent-debugger --session api step over

# Evaluate an expression in top frame (or specified --frame)
agent-debugger --session api eval "user['tier'] == 'gold'"

# Re-inspect parked context
agent-debugger --session api context

# Inspect variables or full stack
agent-debugger --session api vars
agent-debugger --session api stack
```

### 4. Breakpoint Management on Live Sessions

Add or remove breakpoints dynamically while the session is running or parked:

```bash
# Add line breakpoints (persisted to stops.json)
agent-debugger --session api breaks add --break 'app.py:55' --break 'app.py:60|count > 5'

# List all armed breakpoints with plant state (verified, pending, slid, hits)
agent-debugger --session api breaks

# Remove a specific breakpoint
agent-debugger --session api breaks remove --break 'app.py:55'

# Clear all line breakpoints
agent-debugger --session api breaks clear
```

### 5. Resume or Close

```bash
# Resume until next breakpoint or exit
agent-debugger --session api continue

# Cleanly disconnect and tear down session
agent-debugger --session api close
```

*For launch sessions, `close` cleanly terminates the spawned process. For attach sessions, `close` detaches safely without killing your running application.*

---

## Agent / LLM Compaction Survival

When an AI agent runs a multi-step debugging mission, conversation context is frequently compressed or truncated. Agents do not need to maintain complex state in memory:

1. **`agent-debugger status`**  
   Shows all active sessions, target PID/state, and count of armed breakpoints.
2. **`agent-debugger --session <name> breaks`**  
   Shows the live plant state of all breakpoints (`verified`, `pending`, `slid`, `rejected`) and their actual hit counts (`hits`).
3. **`agent-debugger --session <name> context`**  
   Returns the current execution point, call stack, and locals if parked.

---

## Advanced Features

### 1. Ephemeral Capture (`capture`)
Need a one-shot variable inspection without manually adding a breakpoint, waiting, taking snapshot, and deleting the breakpoint?

```bash
agent-debugger --session api capture \
  --break 'app.py:75' \
  --pause-budget 1500 \
  --timeout 10
```
`capture` temporarily plants an ephemeral breakpoint, waits for execution to hit it, captures a bounded snapshot of local variables, removes the breakpoint, and immediately resumes the program within the specified pause budget.

### 2. Browser Reload Trigger (`reload`)
When debugging web frontends, loading code needs a page reload. Rather than switching to an external browser tool, run:

```bash
agent-debugger --session frontend reload --timeout 15
```
This commands the browser tab via CDP to reload and waits for the first breakpoint hit.

### 3. Pure Polling Wait (`wait`)
If an external client (e.g. HTTP curl or browser click) triggers the flow, the agent can wait without resuming:

```bash
agent-debugger --session api wait --timeout 30
```
Succeeds immediately if already stopped, or long-polls until the target stops at any breakpoint.

---

## Command Reference

### Global Flags
- `--session <name>`: Session identifier (default: `"default"`).
- `--human`: Pretty-print terminal-friendly output instead of raw JSON.

### Session & Target Setup
- `py start <script> [--break ...] [--module ...] [--subprocess]`: Start Python program under debugpy.
- `py attach [--port 5678] [--host localhost] [--break ...]`: Attach to running debugpy.
- `node start <script> [--break ...] [--workers]`: Start Node.js script with CDP.
- `node attach [--port 9229] [--host localhost] [--break ...]`: Attach to running Node `--inspect`.
- `browser attach [--port 9222] [--tab <pattern>] [--break ...]`: Attach to Chrome tab.
- `java start --main <MainClass> [--cp <classpath>] [--break ...]`: Start Java target.
- `java attach [--port 5005] [--host localhost] [--break ...]`: Attach to JDWP JVM.

### Execution Control
- `continue [--timeout 20] [--target <id>]`: Resume target until next stop.
- `step [over|into|out] [--timeout 20] [--target <id>]`: Single-step execution.
- `wait [--timeout 20] [--target <id>]`: Long-poll for next stop without resuming.
- `capture [--break spec] [--pause-budget ms] [--timeout 20]`: Bounded snapshot & auto-resume.
- `reload [--timeout 20]`: Reload browser tab and wait for stop.

### State & Inspection
- `context [--target <id>]`: Fetch current location, snippet, locals, and threads.
- `eval <expr> [--frame 0] [--target <id>]`: Evaluate expression in stack frame.
- `vars [--frame 0] [--target <id>]`: List variables in frame.
- `stack [--target <id>]`: Inspect call stack.
- `threads [--target <id>]`: Dump active threads.
- `logs [--tail 50]`: Read captured logpoints.
- `targets`: List all observed targets (main process, child processes, worker threads).

### Breakpoints
- `breaks`: List all armed breakpoints and hit counts.
- `breaks add --break <spec>`: Add live breakpoint (`file:line` or `file:line|condition`).
- `breaks remove --break <spec>`: Remove live breakpoint.
- `breaks clear`: Remove all live line breakpoints.

### System & Diagnostics
- `status`: Show CLI version and all active sessions.
- `close`: Terminate/detach session and reap temporary files.
- `doctor`: Verify system prerequisites and environment readiness.

---

## Architecture

```
                          ┌─────────────────────────┐
                          │   agent-debugger CLI    │
                          │      (Rust Binary)      │
                          └────────────┬────────────┘
                                       │
            ┌──────────────────────────┼──────────────────────────┐
            │                          │                          │
            ▼                          ▼                          ▼
   ┌─────────────────┐        ┌─────────────────┐        ┌─────────────────┐
   │  Python Bridge  │        │   Node Bridge   │        │   Java Bridge   │
   │    (debugpy)    │        │      (CDP)      │        │     (JDWP)      │
   └────────┬────────┘        └────────┬────────┘        └────────┬────────┘
            │                          │                          │
            ▼                          ▼                          ▼
     Python Runtime              Node.js / Chrome             JVM Target
```

1. **State Directory**: Sessions live under `~/.agent-debugger/sessions/<session-name>/`.
2. **Persistence Guarantee**:
   - `stops.json` stores user intent.
   - `breaks.lock` uses kernel-level file locks (`flock`) for atomic read-modify-write without stale file deletion races.
3. **Embedded Adapters**: Bridges for Python, Node, Browser, and Java are embedded in the compiled binary via `include_str!` and materialized to `~/.agent-debugger/adapters/` on demand.

---

## Testing & Quality Gates

The codebase enforces strict end-to-end verification across unit tests, bridge ownership invariants, and live multi-language executions:

```bash
# Run unit & contract tests
cargo test

# Run the complete test suite across all languages & live targets
./scripts/run_gates.sh
```

---

## License

This project is licensed under the [MIT License](LICENSE).  
Copyright (c) 2026 PlaneInABottle.
