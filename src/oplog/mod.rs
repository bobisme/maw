//! Git-native per-workspace operation log (§5.3).
//!
//! Every workspace records a chain of [`Operation`]s as git blobs referenced
//! from `refs/manifold/oplog/<workspace>`. The chain forms a DAG (one parent
//! per workspace, multi-parent for merges) that can be replayed to derive
//! workspace state or global views.

pub mod types;
