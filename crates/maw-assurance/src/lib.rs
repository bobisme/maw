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
//! - [`oracle_a`] — SG1 Oracle A: blob/content reachability with
//!   incremental witness `W` + reachable-set `U` design
//!   (bn-1z8q; behind the `oracles` feature).
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
/// **In-process model driver** for SG1 DST (bn-32k3 / T1.6).
///
/// The workhorse tier of the SG1 architecture (§1) — applies a
/// [`scenario::ScenarioPlan`] step-by-step to a real git temp repo,
/// runs Oracle A + Oracle B after every step, and returns the first
/// violating verdict. Bit-exact across replays because every git write
/// is pinned to `PlannedStep::git_time`. The substrate the T1.6
/// determinism guarantee tests and the T1.6 shrinker run against.
#[cfg(feature = "oracles")]
pub mod in_proc;
/// **Infrastructure-failure classifier** for the SG1 harness (bn-30v6e).
///
/// Tells host resource exhaustion (EDQUOT/ENOSPC/EMFILE/ENFILE) apart
/// from Oracle A/B violations. Fail closed: unmatched errors stay errors.
pub mod infra;
#[cfg(feature = "stateright")]
pub mod model;
pub mod oracle;
/// **Oracle A** — content (blob) reachability for SG1 (bn-1z8q / T1.3).
///
/// Predicate `W ⊆ U(F)` with an incremental `W,U` design (SP2 §2.1, the
/// mandatory amortised-`O(1)`/step design). Catches **work-loss** —
/// committed blob content that has left the durable frontier. See
/// `notes/oracle-ab-spec.md` §0/§2 for why this is **blob**, not
/// commit-ancestry, reachability and `notes/sg1-dst-architecture.md` §4.1
/// for the harness integration point.
#[cfg(feature = "oracles")]
pub mod oracle_a;
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
/// **Escape-path oracles** for the 2026-07 field-report bug classes (bn-2bcx).
///
/// Three targeted oracles closing the DST gaps the 2026-07 escapes slipped
/// through: [`oracle_escape::SiblingRefFaithfulness`] (FF-absorb orphaned
/// committed-ahead siblings — bn-rah2), [`oracle_escape::TrunkDirtyPreservation`]
/// (trunk preserve-and-replay clobbered dirty tracked files — bn-1xmk), and
/// [`oracle_escape::check_record_ref_coherence`] (gc desynced recovery refs from
/// destroy records — bn-3uou).
///
/// bn-286g: `SiblingRefFaithfulness` shares Oracle A's bn-3g6o
/// conflict-as-data carveout, so a sibling replay that CONFLICTS (original blob
/// rewritten into a diff3-marker blob, original OID pinned in the sibling's
/// conflict sidecar) reads as preserved rather than orphaned.
#[cfg(feature = "oracles")]
pub mod oracle_escape;
/// **Clean-materialization oracle** for the bn-p3m9 class (bn-3gba).
///
/// After any op whose contract is "the workspace ends clean at commit X", the
/// working tree must equal the HEAD tree.
/// [`oracle_worktree::CleanMaterialization`] models which workspaces the plan
/// expects to be dirty and asserts every other live, non-default workspace is
/// byte-clean at its own HEAD. Oracle A (blob reachability), Oracle B (refs +
/// merge-state) and the bn-2bcx escape oracles are all blind to this class by
/// construction: in bn-p3m9 every ref was correct and the stale blobs were
/// perfectly reachable — only the *working tree* was wrong.
///
/// bn-22jy adds [`oracle_worktree::MaskedStalePreservation`] alongside it: the
/// preserve-before-overwrite gate (bn-154g). When maw overwrites a
/// **stat-cache-masked** stale file — a divergence every status-shaped query
/// calls clean — the pre-overwrite bytes must first be pinned to
/// `refs/manifold/recovery/<ws>/materialize-*`. The generator op that arms it
/// is `Op::CorruptWorktreeStatMasked`, gated behind
/// `ConditionProfile::corrupt_weight` (0 by default).
#[cfg(feature = "oracles")]
pub mod oracle_worktree;
#[cfg(feature = "scenario")]
pub mod scenario;
/// **Failing-seed shrinker** for SG1 DST (bn-32k3 / T1.6).
///
/// Reduces a failing [`scenario::ScenarioPlan`] to a minimal repro via
/// delta-debugging over `plan.steps`, replaying through
/// [`in_proc::InProcDriver`]. A reduction is kept iff the SAME oracle
/// trips with the SAME violation class on replay
/// ([`in_proc::StepVerdict::same_class`]); never drifts onto an
/// unrelated bug.
#[cfg(feature = "oracles")]
pub mod shrinker;
#[cfg(all(test, feature = "oracles"))]
mod shrinker_tests;
pub mod trace;

// Re-export key types for convenience.
pub use oracle::{
    AssuranceState, AssuranceViolation, WorkspaceStatus, capture_state, check_all,
    check_g1_reachability, check_g2_rewrite_preservation, check_g3_commit_monotonicity,
    check_g4_destructive_gate, check_g5_discoverability, check_g6_searchability,
};
