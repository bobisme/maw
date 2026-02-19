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

pub mod types;
pub mod write;
