//! Assurance module — invariant oracle for DST and formal verification.
//!
//! This crate provides:
//!
//! - [`oracle::AssuranceState`] — snapshot of repository state at a point in time
//! - [`oracle::capture_state`] — capture current repo state for checking
//! - [`oracle::check_g1_reachability`] — G1: committed no-loss
//! - [`oracle::check_g2_rewrite_preservation`] — G2: rewrite preservation
//! - [`oracle::check_g3_commit_monotonicity`] — G3: post-COMMIT monotonicity
//! - [`oracle::check_g4_destructive_gate`] — G4: destructive gate
//! - [`oracle::check_g5_discoverability`] — G5: discoverable recovery
//! - [`oracle::check_g6_searchability`] — G6: searchable recovery
//! - [`oracle::check_all`] — run all six checks
//! - [`model`] — Stateright model-checking definitions
//! - [`fault`] — DST fault-injection / real-SIGKILL / recovery driver
//!   (bn-263u; behind the `fault-injection` feature)
//! - [`scenario`] — deterministic scenario + condition generator
//!   (bn-1f53; behind the `scenario` feature). The driver-agnostic plan
//!   stream "build once, drive two ways" (sg1-dst-architecture.md §2).
//! - [`oracle_b`] — SG1 Oracle B: state-coherence predicate (B1-B4) that
//!   catches the bn-cm63 class (bn-3ji6, behind the `oracles` feature).
//!
//! # Usage
//!
//! ```rust,ignore
//! use maw_assurance::oracle::{capture_state, check_all};
//!
//! let pre = capture_state(repo_root)?;
//! // ... run operation ...
//! let post = capture_state(repo_root)?;
//! check_all(&pre, &post)?;
//! ```

#[cfg(feature = "fault-injection")]
pub mod fault;
#[cfg(feature = "stateright")]
pub mod model;
pub mod oracle;
/// **Oracle B** — state coherence (B1–B4) for SG1 (bn-3ji6 / T1.4).
///
/// Pure predicate over `(refs, ws-dirs, merge-state.json)`. Catches the
/// **bn-cm63 class** (dangling `refs/manifold/head/<ws>` for a non-existent
/// workspace with no live merge protecting it) that Oracle A
/// (content-reachability) cannot see by construction. See
/// `notes/oracle-ab-spec.md` §3 for the predicate definitions and
/// `notes/sg1-dst-architecture.md` §4.2 for the harness integration point.
#[cfg(feature = "oracles")]
pub mod oracle_b;
#[cfg(feature = "scenario")]
pub mod scenario;
pub mod trace;

// Re-export key types for convenience.
pub use oracle::{
    AssuranceState, AssuranceViolation, WorkspaceStatus, capture_state, check_all,
    check_g1_reachability, check_g2_rewrite_preservation, check_g3_commit_monotonicity,
    check_g4_destructive_gate, check_g5_discoverability, check_g6_searchability,
};
