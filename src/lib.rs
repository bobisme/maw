//! maw library crate — re-exports core domain modules for integration tests.
//!
//! The primary interface is the `maw` CLI binary (in the maw-cli crate).
//! CLI-specific modules (workspace, status, push, etc.) now live in maw-cli.

#[cfg(feature = "assurance")]
pub mod assurance;

pub mod backend;
pub mod config;
pub mod eval;
pub mod failpoints;
pub mod merge;
pub mod merge_state;
pub mod model;
pub mod oplog;
pub mod refs;
