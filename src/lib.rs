//! maw library crate — re-exports for integration tests.
//!
//! The primary interface is the `maw` binary. This lib.rs exposes internal
//! modules so that integration tests can exercise the merge engine, backend,
//! and model types directly without going through the CLI.

pub mod backend;
pub mod config;
pub mod epoch_gc;
pub mod eval;
pub mod merge;
pub mod merge_state;
pub mod model;
pub mod oplog;
pub mod refs;

// Private modules only used by the binary — not re-exported.
// agents, doctor, error, exec, format, init, push, release,
// status, transport, tui, upgrade, v2_init, workspace
