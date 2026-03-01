//! Deterministic N-way merge engine — core types and pure logic.
//!
//! This module contains the subset of the merge pipeline that depends only on
//! `model` types (no backend, config, refs, or merge-state dependencies).
//!
//! Modules that require integration with the workspace backend, config, or
//! ref-management remain in the main `maw` crate and will move here once
//! those dependencies are also in `maw-core`.

pub mod build;
pub mod partition;
pub mod plan;
pub mod rename;
pub mod types;
