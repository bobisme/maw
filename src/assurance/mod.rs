//! Assurance module — invariant oracle for DST and formal verification.
//!
//! This module is feature-gated behind `assurance` and has zero overhead in
//! normal builds. It provides:
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
//!
//! # Usage
//!
//! ```rust,ignore
//! use maw::assurance::oracle::{capture_state, check_all};
//!
//! let pre = capture_state(repo_root)?;
//! // ... run operation ...
//! let post = capture_state(repo_root)?;
//! check_all(&pre, &post)?;
//! ```

pub mod oracle;

// Re-export key types for convenience.
pub use oracle::{
    AssuranceState, AssuranceViolation, WorkspaceStatus,
    capture_state, check_all,
    check_g1_reachability, check_g2_rewrite_preservation,
    check_g3_commit_monotonicity, check_g4_destructive_gate,
    check_g5_discoverability, check_g6_searchability,
};
