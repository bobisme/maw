//! Deterministic N-way merge engine.
//!
//! Implements the collect → partition → resolve → build pipeline described in
//! design doc §6.1. Each phase is a separate module.
//!
//! # Phase 1 (this implementation)
//!
//! - **collect**: Scan each source workspace and capture changed files as [`PatchSet`]s.
//! - **partition**: Group changes by path into unique vs shared paths.
//! - **resolve**: Resolve shared paths via hash equality + diff3 with structured conflicts.
//! - **build**: Take epoch + resolved changes, produce a new git tree + commit ([`build`] module).
//!
//! # Determinism guarantee
//!
//! The same set of epoch + workspace patch-sets always produces the same merge
//! result, regardless of workspace creation order or iteration order:
//!
//! - Paths are processed in lexicographic order.
//! - File content (blob identity) drives resolution, not timestamps.
//! - diff3 is itself deterministic given the same inputs.

// --- Modules moved to maw-core (re-exported) ---
pub use maw_core::merge::build;
pub use maw_core::merge::partition;
pub use maw_core::merge::plan;
#[allow(unused_imports)]
pub use maw_core::merge::rename;
pub use maw_core::merge::types;

// --- Modules that remain here (have cross-deps on backend/config/refs/merge_state/ast-merge) ---
#[cfg(feature = "ast-merge")]
pub mod ast_merge;
pub mod build_phase;
pub mod collect;
pub mod commit;
pub mod prepare;
pub mod quarantine;
pub mod resolve;
pub mod validate;

#[allow(unused_imports)]
pub use build_phase::run_build_phase_with_inputs;

#[cfg(all(test, feature = "proptests"))]
mod determinism_tests;

#[cfg(all(test, feature = "proptests"))]
mod pushout_tests;

#[cfg(kani)]
mod kani_proofs;
