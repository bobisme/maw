//! maw CLI crate — all CLI-specific modules live here.
//!
//! The binary entry point is in `main.rs`. This lib.rs exposes CLI modules
//! so that `main.rs` can use them as `crate::module`.

pub mod agents;
pub mod audit;
pub mod doctor;
pub mod epoch;
pub mod epoch_gc;
#[allow(dead_code)]
pub mod error;
pub mod ref_gc;
pub mod exec;
pub mod format;
pub mod merge_cmd;
pub mod push;
pub mod release;
pub mod status;
pub mod telemetry;
pub mod transport;
#[cfg(feature = "tui")]
pub mod tui;
pub mod upgrade;
pub mod v2_init;
pub mod workspace;
