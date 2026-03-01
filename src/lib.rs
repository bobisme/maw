//! maw library crate — re-exports for the CLI and integration tests.
//!
//! The primary interface is the `maw` CLI binary (in the maw-cli crate).
//! This lib.rs exposes all modules so that maw-cli and integration tests
//! can access them.

#[cfg(feature = "assurance")]
pub mod assurance;

pub mod backend;
pub mod failpoints;
pub mod config;
pub mod epoch_gc;
pub mod eval;
pub mod merge;
pub mod merge_state;
pub mod model;
pub mod oplog;
pub mod refs;

// CLI modules — used by the maw-cli binary crate.
pub mod agents;
pub mod audit;
pub mod doctor;
pub mod epoch;
#[allow(dead_code)]
pub mod error;
pub mod exec;
pub mod format;
pub mod merge_cmd;
pub mod push;
pub mod release;
pub mod status;
pub mod telemetry;
pub mod transport;
pub mod tui;
pub mod upgrade;
pub mod v2_init;
pub mod workspace;
