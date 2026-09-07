//! Session lifecycle: spawn the bridge daemon, forward commands, clean up.
//!
//! Layout: `~/.agent-debugger/sessions/<name>/` holds `session.json`
//! (written by the bridge when stopped at the first breakpoint),
//! `error.json` (fatal setup failure), `lang.json` (adapter language),
//! `stops.json` (spawn-time intent: armed stops + target summary, written by
//! the CLI so a compacted agent can resume with zero prior memory),
//! `owner.json` (abandonment guard nonce) and `bridge.log`.
//!
//! Module DAG (one-way; edges are `use` dependencies, anything else is
//! forbidden):
//!
//! ```text
//! spawn_lifecycle.rs ── setup_bridge/spawn/spawn_in + failure/settle set
//!   deps: paths, sidecar, identity, forward, attach, locks (no peer edge)
//! close_status.rs ── startup_close_gate/remove_unpublished_dir/close/
//!   status/session_entry/context (terminal/read path)
//!   deps: paths, sidecar, identity, forward, attach, locks (no peer edge)
//! breaks.rs ── cmd_breaks_* + stops.json mutation lock + persistence
//!   deps: paths, sidecar, forward (via normalize_target_for_lang_opt,
//!   bridge_failure, stamp_main only — never forward_target)
//! attach.rs ── exclusivity/endpoint/probe/legacy/diagnosis/taxonomy
//!   deps: paths, sidecar, identity (port_lookup, trunc_chars,
//!   is_secret_flag at the long-standing call sites); NO locks edge
//! forward.rs ── BridgeFailure/forward/roster/reload
//!   deps: paths, sidecar, identity (via cmd_targets_in)
//! identity.rs ── redact/caps/seeds/OS-probe/layered-reads
//!   deps: paths (normalize_attach_host)
//! locks.rs ── startup+endpoint guards/quarantine/stale/liveness
//!   deps: paths (startup_nonce)
//! sidecar.rs ── SCHEMA_VERSION/gate/atomic-write/session_lang_opt/SpawnSpec
//!   deps: none (paths reserved)
//! paths.rs ── sessions_dir/check_name/check_dir_real/real_dir_for_delete/
//!   ports + startup_nonce
//!   + host-normalize helpers; deps: none
//! ```
//!
//! Allowed-edge matrix (normative; enforced by forbidden-edge greps):
//!
//! | Module | May `use` | Must NOT reference |
//! |--------|-----------|-------------------|
//! | `paths` | nothing above std/serde_json/anyhow | `sidecar::\|locks::\|identity::\|forward::\|breaks::\|attach::\|spawn_lifecycle\|close_status` |
//! | `sidecar` | `paths` | `locks::\|identity::\|forward::\|breaks::\|attach::\|spawn_lifecycle\|close_status` |
//! | `locks` | `paths` | `sidecar::\|identity::\|forward::\|breaks::\|attach::\|spawn_lifecycle\|close_status` |
//! | `identity` | `paths` | `sidecar::\|locks::\|forward::\|breaks::\|attach::\|spawn_lifecycle\|close_status` |
//! | `forward` | `paths`, `sidecar`, `identity` | `locks::\|breaks::\|attach::\|spawn_lifecycle\|close_status` |
//! | `breaks` | `paths`, `sidecar`, `forward` | `locks::\|identity::\|attach::\|spawn_lifecycle\|close_status` |
//! | `attach` | `paths`, `sidecar`, `identity` | `locks::\|forward::\|breaks::\|spawn_lifecycle\|close_status` |
//! | `spawn_lifecycle` | `paths`, `sidecar`, `identity`, `forward`, `attach`, `locks` | `breaks::\|close_status` (peer — no cross-edge) |
//! | `close_status` | `paths`, `sidecar`, `identity`, `forward`, `attach`, `locks` | `breaks::\|spawn_lifecycle` (peer — no cross-edge) |
//!
//! This facade re-exports the full previous `crate::session::*` public
//! surface so external callers (`src/main.rs`, `src/spawn.rs`,
//! `src/output.rs`) compile unchanged.

mod attach;
mod breaks;
mod close_status;
mod forward;
mod identity;
mod locks;
mod paths;
mod sidecar;
mod spawn_lifecycle;

// Re-exports without a current in-crate caller (e.g. `CAUSE_CAP`,
// `attach_exclusive`, the `IDENTITY_*_CAP` caps) are kept intentionally:
// they are the stable `crate::session::*` API, not dead code.
#[allow(unused_imports)]
pub use attach::{attach_exclusive, CAUSE_CAP};
#[allow(unused_imports)]
pub use breaks::{cmd_breaks_add, cmd_breaks_clear, cmd_breaks_remove};
#[allow(unused_imports)]
pub use close_status::{close, cmd_context_target, status};
#[allow(unused_imports)]
pub use forward::{cmd_reload, cmd_targets, forward, forward_target, BridgeFailure};
#[allow(unused_imports)]
pub use identity::{
    attach_seed, identity_hint, launch_seed, redact_argv, target_summary, IDENTITY_ARRAY_CAP,
    IDENTITY_FIELD_CAP, IDENTITY_TOTAL_CAP,
};
#[allow(unused_imports)]
pub use paths::{normalize_attach_host, session_dir, sessions_dir};
#[allow(unused_imports)]
pub use sidecar::{SpawnSpec, SCHEMA_VERSION};
#[allow(unused_imports)]
pub use spawn_lifecycle::spawn;
