//! Git-native per-workspace operation log (§5.3).
//!
//! Every workspace records a chain of [`Operation`]s as git blobs referenced
//! from `refs/manifold/oplog/<workspace>`. The chain forms a DAG (one parent
//! per workspace, multi-parent for merges) that can be replayed to derive
//! workspace state or global views.
//!
//! # Modules
//!
//! - [`types`] — [`Operation`] struct and [`OpPayload`] enum with canonical JSON
//! - [`write`] — write operations as git blobs and advance head refs
//! - [`read`] — walk the causal chain from head backwards
//! - [`view`] — per-workspace view materialization from op log replay

pub mod global_view;
pub mod read;
pub mod types;
pub mod view;
pub mod write;
