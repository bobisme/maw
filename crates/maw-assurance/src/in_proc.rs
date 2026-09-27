//! In-process model driver for SG1 DST (bn-32k3, T1.6).
//!
//! This module implements the **in-process tier** the SG1 architecture
//! describes (`notes/sg1-dst-architecture.md` §1, the "workhorse" tier).
//! It is the substrate the T1.6 determinism guarantee tests and the T1.6
//! shrinker run against — fast enough (no `maw` subprocess, no `setsid`,
//! no `SIGKILL`) that thousands of shrink iterations are cheap.
//!
//! ## What this driver IS
//!
//! A deterministic, bit-exact applier of [`crate::scenario::ScenarioPlan`]
//! steps against a real git repo (`tempfile::TempDir`-rooted). Per op it
//! replicates the **ref-shape effect** maw would have produced — workspace
//! state/epoch/head refs, recovery refs on destroy, merge advances `main`
//! and `refs/manifold/epoch/current` — without invoking the merge FSM.
//! This is the same modelling level the existing
//! [`crate::oracle_a::tests`] and [`crate::oracle_b::tests`] use to plant
//! violations; we hoist it into the driver so a `ScenarioPlan` end-to-end
//! produces oracle verdicts deterministically.
//!
//! Crucially, every git write runs with `GIT_AUTHOR_DATE` /
//! `GIT_COMMITTER_DATE` pinned to `PlannedStep::git_time`, per the §5
//! determinism contract. Without this pin, commit OIDs embed wall-clock
//! time and re-running the same seed produces different OIDs — the bug SP1
//! caught and the precondition the T1.6 determinism tests verify.
//!
//! ## What this driver is NOT
//!
//! Not a faithful subprocess driver — that is [`crate::fault::SubprocFault`]
//! (T1.5). Not a full merge-FSM driver — that requires linking the
//! production `src/merge/*` pipeline and is the deeper integration T1.7
//! wires into CI. The minimum required by **T1.6** is a substrate that:
//!
//! 1. Reproduces a planted oracle violation deterministically per seed;
//! 2. Replays a `ScenarioPlan` end-to-end fast enough for shrinking;
//! 3. Provides the `(Oracle A | Oracle B)` verdict per step so the
//!    shrinker can check equivalence by violation class.
//!
//! ## Fault semantics
//!
//! A [`crate::scenario::FaultSpec::Failpoint`] attached to a merge step
//! semantically means "the merge crashed at this FSM phase". The driver
//! interprets that by skipping the merge's **cleanup**:
//!
//! - For an `Op::Merge { srcs, destroy: true, .. }` carrying a fault, the
//!   merge result still lands in `main`/`epoch/current` (the commit phase
//!   completed in many real bn-cm63 reproductions — the leak was the
//!   *cleanup* failing), but the source workspaces' `refs/manifold/head/*`
//!   ref is **not** torn down, **and** the `ws/<src>/` directory is also
//!   left in place (no destroy happened — that's a separate op). This is
//!   the bn-cm63 setup the canonical seed reaches: after the next planned
//!   `Op::Destroy { ws }` (which removes the directory), the head ref is
//!   left dangling → **Oracle B B1 RED**.
//!
//! - For a destroy-without-recovery scenario (the Oracle A canonical
//!   class), the harness manually plants the loss by issuing a destroy
//!   whose recovery ref intentionally is not pinned. This is the
//!   `inject_plant_work_loss` knob below — the T1.6 test that proves
//!   `Oracle A` reproduces a bit-exact violation across 10 replays
//!   uses it to plant the same lost-blob class the
//!   `tests::planted_work_loss_trips_oracle_a` Oracle A unit test plants.
//!
//! These are exactly the two classes the SG1 architecture says shrinkers
//! must reduce, and the two whose 10/10 reproduction T1.6 must guarantee.

#![cfg(feature = "oracles")]
// This module is harness/test-support code (the in-proc DST driver),
// not production-shipped public API. Relax the strictest pedantic /
// nursery clippy lints — they hurt readability of the substantial
// git-CLI plumbing here without buying real defect prevention. The
// workspace lints stay strict for the production crates.
#![allow(clippy::doc_markdown)]
#![allow(clippy::too_long_first_doc_paragraph)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::format_push_string)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::single_match_else)]
#![allow(clippy::manual_let_else)]
#![allow(clippy::redundant_closure_for_method_calls)]
#![allow(clippy::if_not_else)]
#![allow(clippy::similar_names)]
#![allow(clippy::missing_const_for_fn)]
#![allow(clippy::trivially_copy_pass_by_ref)]
#![allow(clippy::unused_self)]
#![allow(clippy::map_unwrap_or)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::case_sensitive_file_extension_comparisons)]
#![allow(clippy::uninlined_format_args)]
#![allow(clippy::manual_assert)]
#![allow(clippy::needless_pass_by_ref_mut)]
#![allow(clippy::doc_overindented_list_items)]
#![allow(clippy::format_collect)]
#![allow(clippy::if_then_some_else_none)]

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

use crate::infra::{self, InfraFailure};
use crate::oracle::{AssuranceState, AssuranceViolation, WorkspaceStatus, capture_state};
use crate::oracle_a::{OracleA, StepReport};
use crate::oracle_b::{self, OracleBViolation};
use crate::scenario::{
    BaseRef, FaultSpec, FileEdit, Op, PlannedStep, ScenarioPlan, Seeded, Target, WsId,
};

// ---------------------------------------------------------------------------
// Violation verdict — what the driver reports per step
// ---------------------------------------------------------------------------

/// A unified verdict the driver hands back per step — exactly the shape the
/// shrinker uses to decide "did the SAME oracle trip with the SAME violation
/// class on this replay?".
#[derive(Clone, Debug)]
pub enum StepVerdict {
    /// Both oracles are green.
    Clean,
    /// Oracle A tripped. Carries the violation enum variant (lossy
    /// representation as the offending OID/ref string pair so the shrinker
    /// can do class equivalence without holding live `AssuranceViolation`
    /// values across replays).
    OracleA(OracleAClass),
    /// Oracle B tripped. Carries the first violation (deterministic
    /// B1→B2→B3→B4 order) reduced to its class signature.
    OracleB(OracleBClass),
    /// The harness itself malfunctioned (bn-25pac): a plan step could not
    /// be applied, the post-step state could not be read, an oracle
    /// returned a tooling error, or the seed ended with vacuous oracle
    /// evidence. The oracles did NOT judge this seed, so it must never be
    /// counted as clean — it fails the seed exactly like an oracle
    /// violation. Positively classified host-resource failures
    /// (`crate::infra`) never reach this variant: they abort the seed as
    /// INFRA before a verdict is formed.
    HarnessError(HarnessErrorClass),
}

/// Class signature for a harness malfunction (bn-25pac).
#[derive(Clone, Debug)]
pub struct HarnessErrorClass {
    /// Stable identifier of the failing harness site (e.g.
    /// `"capture_state"`, `"oracle_a_check"`, `"apply_op:Commit"`,
    /// `"vacuous_witnesses"`). This is the equivalence key.
    pub site: &'static str,
    /// Human-readable detail (error text; may embed temp paths, so it is
    /// NOT part of the equivalence key).
    pub detail: String,
}

impl PartialEq for HarnessErrorClass {
    fn eq(&self, other: &Self) -> bool {
        self.site == other.site
    }
}
impl Eq for HarnessErrorClass {}

impl HarnessErrorClass {
    fn new(site: &'static str, detail: impl Into<String>) -> Self {
        Self {
            site,
            detail: detail.into(),
        }
    }
}

/// Class signature for an Oracle A violation — enum-variant + offending
/// entity. The shrinker keeps a reduction iff this matches the original.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OracleAClass {
    /// `"ReachabilityLost"` for the only Oracle A violation class.
    pub kind: &'static str,
    /// The lost blob OID.
    pub oid: String,
}

/// Class signature for an Oracle B violation — enum-variant + offending
/// entity (workspace name, or ref name for malformed-recovery).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OracleBClass {
    /// `"DanglingHeadRef" | "DanglingOwnedRef" | "MergeStateOrphanSource"
    /// | "MergeStateBadEpoch" | "RecoveryRefMalformed"`.
    pub kind: &'static str,
    /// The offending workspace/source name (or the OID for bad-epoch /
    /// the ref name for malformed-recovery).
    pub entity: String,
}

impl StepVerdict {
    /// `true` iff this verdict is a violation (any oracle).
    #[must_use]
    pub const fn is_violation(&self) -> bool {
        !matches!(self, Self::Clean)
    }

    /// True iff `self` and `other` are the same violation class+entity.
    ///
    /// This is the **equivalence relation** the shrinker uses. A reduced
    /// plan is kept iff it produces the SAME class and SAME offending
    /// entity (not just any violation — that would let the shrinker drift
    /// onto an unrelated bug).
    #[must_use]
    pub fn same_class(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Clean, Self::Clean) => true,
            (Self::OracleA(a), Self::OracleA(b)) => a == b,
            (Self::OracleB(a), Self::OracleB(b)) => a == b,
            (Self::HarnessError(a), Self::HarnessError(b)) => a == b,
            _ => false,
        }
    }

    /// `(kind, entity)` signature for bundles / corpus entries.
    #[must_use]
    pub fn signature(&self) -> (&'static str, String) {
        match self {
            Self::Clean => ("Clean", String::new()),
            Self::OracleA(a) => (a.kind, a.oid.clone()),
            Self::OracleB(b) => (b.kind, b.entity.clone()),
            Self::HarnessError(h) => ("HarnessError", format!("{}: {}", h.site, h.detail)),
        }
    }

    /// `true` iff this is a harness malfunction (not an oracle finding).
    #[must_use]
    pub const fn is_harness_error(&self) -> bool {
        matches!(self, Self::HarnessError(_))
    }
}

impl OracleAClass {
    fn from_violation(v: &AssuranceViolation) -> Self {
        match v {
            AssuranceViolation::ReachabilityLost { oid, .. } => Self {
                kind: "ReachabilityLost",
                oid: oid.clone(),
            },
            // Other AssuranceViolation variants are not Oracle A scope
            // (capture_state errors etc.) — funnel them into a sentinel
            // so the shrinker can still observe equivalence.
            other => Self {
                kind: "Other",
                oid: format!("{other}"),
            },
        }
    }
}

impl OracleBClass {
    fn from_violation(v: &OracleBViolation) -> Self {
        match v {
            OracleBViolation::DanglingHeadRef { workspace, .. } => Self {
                kind: "DanglingHeadRef",
                entity: workspace.clone(),
            },
            OracleBViolation::DanglingOwnedRef { workspace, .. } => Self {
                kind: "DanglingOwnedRef",
                entity: workspace.clone(),
            },
            OracleBViolation::MergeStateOrphanSource { source, .. } => Self {
                kind: "MergeStateOrphanSource",
                entity: source.clone(),
            },
            OracleBViolation::MergeStateBadEpoch { which, oid, .. } => Self {
                kind: "MergeStateBadEpoch",
                entity: format!("{which}:{oid}"),
            },
            OracleBViolation::RecoveryRefMalformed { ref_name, .. } => Self {
                kind: "RecoveryRefMalformed",
                entity: ref_name.clone(),
            },
            OracleBViolation::GitError { check, .. } => Self {
                kind: "GitError",
                entity: (*check).to_string(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// PlantedFault — knobs the harness can layer on a plan to inject defects
// ---------------------------------------------------------------------------

/// A defect the harness deliberately plants on top of a plan, used by the
/// T1.6 determinism tests to seed a guaranteed violation.
///
/// These are **not** generated by `DefaultScenarioGenerator` — they are
/// explicit harness annotations so the test can plant a known-good
/// violation and then prove (a) it reproduces 10/10 times and (b) the
/// shrinker reduces the plan around it.
///
/// ## Plant timing
///
/// Plants always fire **after the FINAL plan step**, regardless of how
/// long the plan is. This is deliberate: the shrinker reduces by removing
/// steps, and if a plant were keyed to a specific 0-based index, every
/// removal that crossed the index would orphan the plant and let the
/// shrinker mistakenly declare the reduction successful. With "always
/// after last", the plant rides with the plan's tail and the reduction
/// can keep removing prefix/middle steps until only the load-bearing
/// minimum remain (typically just the create+commit that authored the
/// witness blob).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlantedDefect {
    /// After the plan's last step fires, *also* delete every
    /// workspace-owned ref for `<ws>` **without** pinning a recovery ref,
    /// then prune the dangling commit so the authored content is genuinely
    /// unreachable. Oracle A must fire with `ReachabilityLost` on the next
    /// `check_step`. (Mirrors `oracle_a::tests::planted_work_loss_trips_oracle_a`.)
    WorkLoss {
        /// Workspace whose authored content is lost.
        ws: String,
    },
    /// After the plan's last step fires, leave the workspace's
    /// `refs/manifold/head/<ws>` in place but remove `ws/<ws>/` and all
    /// other owned refs. Oracle B B1 must fire with `DanglingHeadRef` on
    /// the next `check`. (Mirrors `oracle_b::tests::b1_fires_on_bn_cm63_reproduction`.)
    DanglingHeadRef {
        /// Workspace whose head ref is left dangling.
        ws: String,
    },
}

// ---------------------------------------------------------------------------
// InProcDriver — the workhorse
// ---------------------------------------------------------------------------

/// The in-process driver. Owns a temp git repo, applies plan steps,
/// invokes both oracles per step, returns the first violation (if any).
pub struct InProcDriver {
    repo: TempDir,
    /// Stable HEAD OID at repo init — every `WsCreate` builds on top of
    /// this. Recorded once so the driver is independent of the global
    /// `git init` HEAD shifting between platforms.
    root_oid: String,
    /// Planted defects to apply after specific plan steps.
    planted: Vec<PlantedDefect>,
    /// Reusable Oracle A across steps (incremental design).
    oracle_a: OracleA,
    /// Evidence counters for the vacuity guard (bn-25pac).
    stats: DriveStats,
    /// Workspaces with evidence (a create and/or new commits) that no
    /// observation has handed to Oracle A yet: `ws -> (creates, commits)`.
    pending_evidence: std::collections::BTreeMap<String, (usize, usize)>,
    /// Test-only fault knob: pretend the per-step workspace observation
    /// saw no workspaces (the historical `unwrap_or_default` fail-open
    /// class), so the vacuity guard can be proven to fire.
    #[cfg(test)]
    test_blind_ws_observation: bool,
}

/// Per-drive evidence counters (bn-25pac). A seed only counts as clean
/// when these prove the oracles actually judged it; see
/// [`DriveStats::vacuity`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DriveStats {
    /// Oracle A `check_step` calls that returned `Ok` (harvest + judge).
    pub oracle_a_checks: usize,
    /// Oracle B `check` calls that completed.
    pub oracle_b_checks: usize,
    /// Final `|W|` (Oracle A witness blobs harvested over the whole drive).
    pub witnesses: usize,
    /// `WsCreate`/`Recover` ops that materialised a workspace.
    pub workspaces_created: usize,
    /// Created workspaces that a later observation handed to Oracle A.
    pub workspaces_observed: usize,
    /// `Commit` ops that produced a new workspace commit (any).
    pub commits_made: usize,
    /// New workspace commits that a later Oracle A harvest observed while
    /// the workspace was still extant (a plant that removes the workspace
    /// before any observation legitimately leaves a commit unobserved).
    pub commits_observed: usize,
}

impl DriveStats {
    /// The end-of-seed vacuity guard (bn-25pac). `Some` iff the counters
    /// prove the oracles did not actually judge the seed.
    #[must_use]
    pub fn vacuity(&self) -> Option<HarnessErrorClass> {
        if self.oracle_a_checks == 0 || self.oracle_b_checks == 0 {
            return Some(HarnessErrorClass::new(
                "vacuous_checks",
                format!(
                    "seed ended with oracle_a_checks={} oracle_b_checks={}",
                    self.oracle_a_checks, self.oracle_b_checks
                ),
            ));
        }
        if self.workspaces_created > 0 && self.workspaces_observed == 0 {
            return Some(HarnessErrorClass::new(
                "vacuous_workspaces",
                format!(
                    "plan created {} workspace(s) but Oracle A never observed one",
                    self.workspaces_created
                ),
            ));
        }
        if self.commits_observed > 0 && self.witnesses == 0 {
            return Some(HarnessErrorClass::new(
                "vacuous_witnesses",
                format!(
                    "Oracle A observed {} workspace commit(s) but harvested 0 witnesses",
                    self.commits_observed
                ),
            ));
        }
        None
    }

    /// Fold another drive's counters into this one (soak totals).
    pub fn accumulate(&mut self, other: &Self) {
        self.oracle_a_checks += other.oracle_a_checks;
        self.oracle_b_checks += other.oracle_b_checks;
        self.witnesses += other.witnesses;
        self.workspaces_created += other.workspaces_created;
        self.workspaces_observed += other.workspaces_observed;
        self.commits_made += other.commits_made;
        self.commits_observed += other.commits_observed;
    }
}

impl InProcDriver {
    /// Construct a fresh driver rooted at a new tempdir-backed git repo.
    pub fn new() -> std::io::Result<Self> {
        let repo = TempDir::new()?;
        let root = repo.path().to_path_buf();
        Self::init_repo(&root)?;
        let root_oid = git_capture(&root, &["rev-parse", "HEAD"])?;
        std::fs::create_dir_all(root.join("ws"))?;
        let oracle_a = OracleA::new(&root);
        Ok(Self {
            repo,
            root_oid,
            planted: Vec::new(),
            oracle_a,
            stats: DriveStats::default(),
            pending_evidence: std::collections::BTreeMap::new(),
            #[cfg(test)]
            test_blind_ws_observation: false,
        })
    }

    /// Plant a defect that the driver will apply after the named step.
    #[must_use]
    pub fn with_planted(mut self, defects: Vec<PlantedDefect>) -> Self {
        self.planted = defects;
        self
    }

    /// Repo root (so tests/callers can inspect post-state).
    #[must_use]
    pub fn repo_root(&self) -> &Path {
        self.repo.path()
    }

    /// Drive `plan` end-to-end, invoking Oracle A + Oracle B after every
    /// step. Returns the first step's verdict that is a violation, or the
    /// final clean verdict if the whole plan passes.
    ///
    /// Planted defects fire **after the last step**, then a final oracle
    /// check runs so the planted violation is observed (otherwise a
    /// shrinker that removes the trailing step would silently lose the
    /// plant).
    #[must_use]
    pub fn drive(&mut self, plan: &ScenarioPlan) -> DriveOutcome {
        self.drive_inner(plan, /*check_each_step=*/ true)
    }

    /// Faster variant: apply every step, plant defects after the last,
    /// run oracles **only at the end**. Used by the shrinker to keep
    /// replays cheap (~ms per iter on top of the per-step apply cost).
    /// Soundness is preserved because the shrinker only ever asks
    /// "does the violation still reproduce", which a final-only check
    /// answers identically for a planted-at-tail defect.
    #[must_use]
    pub fn drive_fast(&mut self, plan: &ScenarioPlan) -> DriveOutcome {
        self.drive_inner(plan, /*check_each_step=*/ false)
    }

    fn drive_inner(&mut self, plan: &ScenarioPlan, check_each_step: bool) -> DriveOutcome {
        let mut steps_replayed = 0usize;
        let last_idx = plan.steps.len().saturating_sub(1);
        for (i, step) in plan.steps.iter().enumerate() {
            steps_replayed = i + 1;
            // Plan-step application FAILS CLOSED (bn-25pac). The in-proc
            // model expresses every *expected* outcome of an op under the
            // plan — a merge whose source has no tip, an edit/commit to a
            // workspace destroyed in flight, a recover with no recovery ref,
            // a faulted merge skipping cleanup — as an `Ok(())` no-op or a
            // modelled ref shape inside `apply_op`, never as an `Err`. So
            // an `Err` here always means the harness could not make the repo
            // reflect the plan. A positively classified host resource
            // failure (EDQUOT/ENOSPC/EMFILE/ENFILE) aborts the seed as INFRA
            // (bn-30v6e); anything else is a HarnessError that fails the
            // seed like an oracle violation — the oracles must never judge
            // a half-applied step and call it clean.
            if let Err(err) = self.apply_op(step) {
                infra::raise_if_infra_io(&err, &format!("in-proc apply step {i}"));
                return DriveOutcome {
                    verdict: StepVerdict::HarnessError(HarnessErrorClass::new(
                        op_site(&step.op),
                        format!("step {i}: {err}"),
                    )),
                    steps_replayed,
                    stats: self.final_stats(),
                };
            }

            // Per-step Oracle A harvest is REQUIRED even in fast mode:
            // Oracle A is incremental (`W` accretes across steps from
            // per-workspace deltas), so without a per-step harvest a
            // ws that gets destroyed before the final check would never
            // have its blobs witnessed → planted work-loss would silently
            // "vanish". This is the price of Oracle A's incremental
            // design (`oracle_a` SP2 §2.1); we accept it.
            //
            // We do NOT run Oracle B per-step in fast mode (Oracle B is
            // a stateless predicate — final check is sufficient).
            // Plants fire after the FINAL step (see PlantedDefect doc).
            if i == last_idx && !self.planted.is_empty() {
                let defects = self.planted.clone();
                for d in &defects {
                    self.apply_planted_defect(d, step);
                }
            }
            if check_each_step {
                let verdict = self.check_oracles(i);
                if verdict.is_violation() {
                    return DriveOutcome {
                        verdict,
                        steps_replayed,
                        stats: self.final_stats(),
                    };
                }
            } else if i < last_idx {
                // Fast mode: per-step Oracle A harvest only (no Oracle B).
                if let Some(err) = self.harvest_only(i) {
                    return DriveOutcome {
                        verdict: StepVerdict::HarnessError(err),
                        steps_replayed,
                        stats: self.final_stats(),
                    };
                }
            }
        }
        // Final oracle check (always — catches plants at tail and is the
        // only check done in fast mode).
        let mut final_verdict = self.check_oracles(last_idx);
        let stats = self.final_stats();
        // Vacuity guard (bn-25pac): a seed only counts as clean when the
        // evidence counters prove the oracles actually judged it.
        if !final_verdict.is_violation()
            && let Some(err) = stats.vacuity()
        {
            final_verdict = StepVerdict::HarnessError(err);
        }
        DriveOutcome {
            verdict: final_verdict,
            steps_replayed,
            stats,
        }
    }

    fn final_stats(&self) -> DriveStats {
        DriveStats {
            witnesses: self.oracle_a.witness_count(),
            ..self.stats
        }
    }

    /// Fast-mode-only helper: run **just** Oracle A's incremental harvest
    /// on the current state and discard a finding (the final check judges).
    /// Keeps `W` accreting across steps without paying the full
    /// `check_oracles` cost. Returns `Some` iff the harness malfunctioned
    /// (fail closed, bn-25pac).
    fn harvest_only(&mut self, step_index: usize) -> Option<HarnessErrorClass> {
        let state = match self.observe_state() {
            Ok(s) => s,
            Err(e) => return Some(e),
        };
        match self.oracle_a.check_step(&state, step_index) {
            Err(err) => {
                infra::raise_if_infra_text(&err.to_string(), "oracle A harvest");
                Some(HarnessErrorClass::new("oracle_a_check", err.to_string()))
            }
            Ok(StepReport {
                violation: Some(v), ..
            }) => {
                if let Some(f) = oracle_a_infra(&v) {
                    infra::raise(f);
                }
                self.note_oracle_a_checked(&state);
                None
            }
            Ok(_) => {
                self.note_oracle_a_checked(&state);
                None
            }
        }
    }

    /// Capture the post-step state in the maw-ref-shape view Oracle A
    /// needs. FAILS CLOSED (bn-25pac): an unreadable state, an unreadable
    /// `ws/` directory, or an extant workspace directory whose state ref
    /// cannot be resolved is a HarnessError — never an empty/skipped
    /// workspace that would let Oracle A silently harvest nothing.
    ///
    /// We override `state.workspaces` (head_oid taken from
    /// `refs/manifold/ws/<ws>`) because the in-proc driver doesn't create
    /// real per-ws git worktrees — `capture_state`'s default
    /// `git rev-parse HEAD` inside `ws/<x>/` would return empty. This
    /// matches the modelling level the `oracle_a::tests` use.
    fn observe_state(&mut self) -> Result<AssuranceState, HarnessErrorClass> {
        let root = self.repo.path().to_path_buf();
        let mut state = match capture_state(&root) {
            Ok(s) => s,
            Err(err) => {
                // A host resource failure must not read as "clean".
                infra::raise_if_infra_text(&err.to_string(), "capture_state");
                return Err(HarnessErrorClass::new("capture_state", err.to_string()));
            }
        };
        state.workspaces.clear();
        let entries = std::fs::read_dir(root.join("ws")).map_err(|err| {
            infra::raise_if_infra_io(&err, "read ws/");
            HarnessErrorClass::new("read_ws_dir", err.to_string())
        })?;
        for entry in entries {
            let entry = entry.map_err(|err| {
                infra::raise_if_infra_io(&err, "read ws/ entry");
                HarnessErrorClass::new("read_ws_dir", err.to_string())
            })?;
            let name = entry.file_name().to_string_lossy().to_string();
            if !entry.path().is_dir() {
                continue;
            }
            let head_oid = match resolve_ref(&root, &refs_workspace_state(&name)) {
                Ok(Some(oid)) => oid,
                Ok(None) => {
                    return Err(HarnessErrorClass::new(
                        "ws_state_ref_missing",
                        format!(
                            "workspace dir ws/{name} exists but refs/manifold/ws/{name} does not"
                        ),
                    ));
                }
                Err(err) => {
                    return Err(HarnessErrorClass::new(
                        "ws_state_ref_unreadable",
                        err.to_string(),
                    ));
                }
            };
            state.workspaces.insert(
                name,
                WorkspaceStatus {
                    head_oid,
                    is_dirty: false,
                    exists: true,
                },
            );
        }
        #[cfg(test)]
        if self.test_blind_ws_observation {
            state.workspaces.clear();
        }
        // Cross-check: every workspace that gained evidence since the last
        // observation and whose directory still exists MUST be in the view
        // handed to Oracle A (independent of the enumeration above, so a
        // future regression there cannot silently blind Oracle A).
        for ws in self.pending_evidence.keys() {
            if root.join("ws").join(ws).is_dir() && !state.workspaces.contains_key(ws) {
                return Err(HarnessErrorClass::new(
                    "ws_unobserved",
                    format!("workspace ws/{ws} exists on disk but was not handed to Oracle A"),
                ));
            }
        }
        Ok(state)
    }

    /// Record that Oracle A judged `state`: fold pending evidence for every
    /// workspace it saw into the counters. Evidence for a workspace that
    /// vanished before any observation (a tail plant) is dropped uncounted.
    fn note_oracle_a_checked(&mut self, state: &AssuranceState) {
        self.stats.oracle_a_checks += 1;
        let pending = std::mem::take(&mut self.pending_evidence);
        for (ws, (creates, commits)) in pending {
            if state.workspaces.contains_key(&ws) {
                self.stats.workspaces_observed += creates;
                self.stats.commits_observed += commits;
            }
        }
    }

    fn note_evidence(&mut self, ws: &str, creates: usize, commits: usize) {
        self.stats.workspaces_created += creates;
        self.stats.commits_made += commits;
        let e = self.pending_evidence.entry(ws.to_string()).or_default();
        e.0 += creates;
        e.1 += commits;
    }

    // -- impl details below --

    fn init_repo(root: &Path) -> std::io::Result<()> {
        run_git(root, &["init", "-q", "-b", "main"])?;
        run_git(root, &["config", "user.name", "DST"])?;
        run_git(root, &["config", "user.email", "dst@maw"])?;
        run_git(root, &["config", "commit.gpgsign", "false"])?;
        // Pin the initial commit's clock to a fixed second so even the
        // repo-init step is deterministic across runs.
        let env = pinned_env(crate::scenario::GIT_TIME_BASE_FOR_DRIVER);
        std::fs::write(root.join("README.md"), "dst\n")?;
        run_git(root, &["add", "README.md"])?;
        run_git_env(root, &["commit", "-q", "--no-gpg-sign", "-m", "init"], &env)?;
        let head = git_capture(root, &["rev-parse", "HEAD"])?;
        run_git(root, &["update-ref", "refs/manifold/epoch/current", &head])?;
        Ok(())
    }

    /// Apply a single plan step to the repo.
    ///
    /// ## Error classification (bn-25pac)
    ///
    /// The in-proc model has no op that is *expected* to fail: fault
    /// injection only changes a merge's cleanup shape, and every plan-level
    /// "refusal" is modelled as an `Ok(())` no-op below —
    ///
    /// | op | expected no-op outcome (data, `Ok`) |
    /// |----|--------------------------------------|
    /// | `EditFiles` / `Commit` | workspace dir gone (destroyed in flight) or nothing to commit |
    /// | `Merge` | no sources, or the last source has no state ref (no tip) |
    /// | `Recover` | no recovery ref for the source workspace |
    /// | `Destroy` | workspace already gone (dir/refs absent) |
    /// | `Sync`/`Advance`/`DirtyTrunkWrite`/`CorruptWorktreeStatMasked` | not modelled at this tier |
    /// | `Gc` | age-gated sweep (no modellable effect) |
    ///
    /// Every `Err` is therefore a harness malfunction: the caller turns it
    /// into [`StepVerdict::HarnessError`] (after infra triage).
    fn apply_op(&mut self, step: &PlannedStep) -> std::io::Result<()> {
        let root = self.repo.path().to_path_buf();
        let env = pinned_env(step.git_time);
        match &step.op {
            Op::WsCreate { ws, from } => {
                self.do_ws_create(&root, ws, from, &env)?;
                self.note_evidence(&ws.0, 1, 0);
                Ok(())
            }
            Op::EditFiles { ws, files } => self.do_edit_files(&root, ws, files),
            Op::Commit { ws, msg } => {
                if self.do_commit(&root, ws, msg, &env)? {
                    self.note_evidence(&ws.0, 0, 1);
                }
                Ok(())
            }
            Op::Merge {
                srcs,
                into,
                destroy,
            } => self.do_merge(&root, srcs, into, *destroy, &step.fault, &env),
            // Advance is modelled like Sync at the in-proc level (no per-ws
            // epoch-staleness representation). Only generated when a profile
            // sets advance_weight > 0; the default soak profile never emits it.
            Op::Sync { ws } | Op::Advance { ws } => self.do_sync(&root, ws),
            Op::Destroy { ws, force: _ } => self.do_destroy(&root, ws, &env),
            Op::Recover { ws, to } => {
                if self.do_recover(&root, ws, to, &env)? {
                    self.note_evidence(&to.0, 1, 0);
                }
                Ok(())
            }
            // bn-2bcx escape ops. Only generated when a profile sets
            // escape_weight > 0; the default in-proc soak profile keeps it 0, so
            // these arms are inert for the bn-2yzz campaign. The load-bearing
            // coverage for these ops is the production-code DST tier
            // (`tests/dst_production_tier.rs`), which drives the REAL maw binary
            // where the FF-absorb / dirty-trunk / gc-recover code actually lives.
            Op::OutOfMawCommit { files, msg } => self.do_out_of_maw_commit(&root, files, msg, &env),
            // The in-proc model has no real default worktree, so an uncommitted
            // trunk edit has no ref-shape effect to model — the driver that can
            // exercise it is the production tier.
            //
            // bn-22jy: the corruption primitive lands in the same bucket — the
            // in-proc model's "workspaces" are plain directories, not real git
            // worktrees with an index, so there is no stat cache to mask and no
            // `maw ws sync` checkout to arm. Its load-bearing coverage is the
            // production-code DST tier (`tests/dst_production_tier.rs`), which
            // drives the REAL maw binary where bn-154g's
            // preserve-before-overwrite guard actually lives. Only generated
            // when a profile sets corrupt_weight > 0; the in-proc soak profile
            // keeps it 0, so this arm is inert for the bn-2yzz campaign.
            Op::DirtyTrunkWrite { .. } | Op::CorruptWorktreeStatMasked { .. } => Ok(()),
            Op::Gc {
                recovery_snapshots,
                older_than_days,
            } => self.do_gc(&root, *recovery_snapshots, *older_than_days),
        }
    }

    fn do_ws_create(
        &self,
        root: &Path,
        ws: &WsId,
        _from: &BaseRef,
        env: &[(String, String)],
    ) -> std::io::Result<()> {
        let ws_dir = root.join("ws").join(&ws.0);
        std::fs::create_dir_all(&ws_dir)?;
        // Plant a minimal sentinel so the worktree isn't empty; not
        // committed yet.
        std::fs::write(ws_dir.join(".maw-ws"), &ws.0)?;
        // Wire the workspace's owned refs to the root commit (`main` head).
        // This mirrors `maw ws create` which establishes head/state/epoch
        // refs pointing at the base epoch.
        let head = git_capture(root, &["rev-parse", "refs/manifold/epoch/current"])?;
        run_git(root, &["update-ref", &refs_workspace_state(&ws.0), &head])?;
        run_git(root, &["update-ref", &refs_workspace_epoch(&ws.0), &head])?;
        // Create an oplog-head **blob** at the same shape `ensure_workspace_oplog_head`
        // would write so Oracle B has something to evaluate.
        let oplog_blob = git_hash_object_stdin(
            root,
            format!(r#"{{"workspace_id":"{}","epoch":"{}"}}"#, ws.0, head).as_bytes(),
        )?;
        run_git(
            root,
            &["update-ref", &refs_workspace_head(&ws.0), &oplog_blob],
        )?;
        let _ = env; // env not needed; no commit produced here
        Ok(())
    }

    fn do_edit_files(&self, root: &Path, ws: &WsId, files: &[FileEdit]) -> std::io::Result<()> {
        let ws_dir = root.join("ws").join(&ws.0);
        if !ws_dir.is_dir() {
            return Ok(()); // a planted destroy may have removed it
        }
        for f in files {
            let path = ws_dir.join(&f.path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, &f.content)?;
        }
        Ok(())
    }

    fn do_commit(
        &self,
        root: &Path,
        ws: &WsId,
        msg: &Seeded,
        env: &[(String, String)],
    ) -> std::io::Result<bool> {
        // Manually build a commit at refs/manifold/ws/<ws> with the
        // workspace's edited file content (a single file per commit is
        // sufficient — we just need a real, hash-stable blob for Oracle
        // A's witness harvest).
        let ws_dir = root.join("ws").join(&ws.0);
        if !ws_dir.is_dir() {
            return Ok(false); // destroyed in flight (modelled no-op)
        }
        let mut tree_entries: Vec<(String, String)> = Vec::new();
        for entry in walk_files(&ws_dir)? {
            let rel = entry
                .strip_prefix(&ws_dir)
                .unwrap_or(&entry)
                .to_string_lossy()
                .into_owned();
            // Skip git-internal & sentinel files.
            if rel.starts_with(".git") || rel == ".maw-ws" {
                continue;
            }
            let content = std::fs::read(&entry)?;
            let blob = git_hash_object_stdin(root, &content)?;
            tree_entries.push((rel, blob));
        }
        if tree_entries.is_empty() {
            return Ok(false); // nothing edited yet (modelled no-op)
        }
        // Build a flat tree (paths with `/` get nested via mktree's flat
        // input format — we use one entry per file with `/` allowed only
        // at depth 1; canonicalise by sorting).
        tree_entries.sort();
        let mut mktree_input = String::new();
        for (path, blob) in &tree_entries {
            // Skip nested paths to keep the test tree flat; if the path
            // contains `/`, replace with a slot name. The plan's edit
            // paths are short ("ws-0/file-1.txt", "shared/file-0.txt").
            // Use the basename so mktree is happy.
            let basename = std::path::Path::new(path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone());
            mktree_input.push_str(&format!("100644 blob {blob}\t{basename}\n"));
        }
        // Deduplicate by basename (mktree refuses duplicates) — keep last.
        let mut dedup: Vec<(String, String)> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for line in mktree_input.lines().rev() {
            let parts: Vec<&str> = line.splitn(3, [' ', '\t']).collect();
            if parts.len() < 3 {
                continue;
            }
            let basename = parts[2].to_string();
            if seen.insert(basename.clone()) {
                dedup.push((basename, line.to_string()));
            }
        }
        dedup.sort_by(|a, b| a.0.cmp(&b.0));
        let mktree_input: String = dedup.iter().map(|(_, l)| format!("{l}\n")).collect();
        let tree = git_pipe(root, &["mktree"], mktree_input.as_bytes())?;
        // Parent: current ws tip if any, else main.
        let ws_ref = refs_workspace_state(&ws.0);
        let parent = resolve_ref(root, &format!("{ws_ref}^{{commit}}"))?
            .unwrap_or_else(|| self.root_oid.clone());
        let commit = git_pipe_env(
            root,
            &["commit-tree", &tree, "-p", &parent, "-m", &msg.0],
            &[],
            env,
        )?;
        run_git(root, &["update-ref", &ws_ref, &commit])?;
        // Roll the workspace head ref to the new commit too (oplog
        // semantics aside, this gives Oracle A a fresh tip to harvest).
        run_git(root, &["update-ref", &refs_workspace_head(&ws.0), &commit])?;
        Ok(true)
    }

    fn do_merge(
        &self,
        root: &Path,
        srcs: &[WsId],
        into: &Target,
        destroy: bool,
        fault: &FaultSpec,
        env: &[(String, String)],
    ) -> std::io::Result<()> {
        let _ = into; // we only model `Target::Default`
        // The merge's effect: advance main + refs/manifold/epoch/current
        // to the last source's tip; bump the per-ws epoch refs of
        // non-sources to mark them stale; optionally destroy sources.
        let Some(last) = srcs.last() else {
            return Ok(());
        };
        let ws_ref = refs_workspace_state(&last.0);
        let Some(new_tip) = resolve_ref(root, &ws_ref)? else {
            return Ok(()); // no tip to merge — model says the chooser still emitted it; no-op
        };
        // Synthesize a merge commit so main has a fresh OID (closer to maw's
        // semantics, which always emits a fresh epoch commit).
        let prev_main = git_capture(root, &["rev-parse", "refs/heads/main"])?;
        let merge_tree = git_capture(root, &["rev-parse", &format!("{new_tip}^{{tree}}")])?;
        let merge_msg = format!(
            "merge {srcs:?} -> default (in-proc-driver)",
            srcs = srcs.iter().map(|w| &w.0).collect::<Vec<_>>()
        );
        let merge_commit = git_pipe_env(
            root,
            &[
                "commit-tree",
                &merge_tree,
                "-p",
                &prev_main,
                "-m",
                &merge_msg,
            ],
            &[],
            env,
        )?;
        run_git(root, &["update-ref", "refs/heads/main", &merge_commit])?;
        run_git(
            root,
            &["update-ref", "refs/manifold/epoch/current", &merge_commit],
        )?;
        if destroy && !fault.is_some() {
            // Clean destroy of sources with recovery refs pinned.
            for src in srcs {
                self.do_destroy(root, src, env)?;
            }
        }
        // If `fault.is_some()`, the cleanup phase did NOT run — sources
        // are left in place; their head refs remain. This is the bn-cm63
        // setup that a follow-up `Op::Destroy { ws }` (or a planted
        // `DanglingHeadRef` defect) will then turn into a B1 violation.
        Ok(())
    }

    fn do_sync(&self, _root: &Path, _ws: &WsId) -> std::io::Result<()> {
        // Sync is a no-op at this modelling level (no per-ws epoch
        // staleness representation).
        Ok(())
    }

    /// Model an out-of-maw trunk commit: build a commit from `files` on top of
    /// `refs/heads/main` and advance `main` to it, WITHOUT touching
    /// `refs/manifold/epoch/current`. This is the FF-absorb arming condition
    /// (branch ahead of epoch) at the ref-shape level the in-proc model uses.
    fn do_out_of_maw_commit(
        &self,
        root: &Path,
        files: &[FileEdit],
        msg: &Seeded,
        env: &[(String, String)],
    ) -> std::io::Result<()> {
        let prev_main = git_capture(root, &["rev-parse", "refs/heads/main"])?;
        let base_tree = git_capture(root, &["rev-parse", &format!("{prev_main}^{{tree}}")])?;
        // Layer the seed-derived blobs onto the base tree via a fresh flat
        // tree (basename-only, matching do_commit's flattening).
        let mut mktree_input = String::new();
        // Preserve the base tree's existing entries by reading it back.
        let ls = git_capture(root, &["ls-tree", &base_tree])?;
        for line in ls.lines() {
            mktree_input.push_str(line);
            mktree_input.push('\n');
        }
        for f in files {
            let blob = git_hash_object_stdin(root, f.content.as_bytes())?;
            let basename = std::path::Path::new(&f.path)
                .file_name()
                .map_or_else(|| f.path.clone(), |s| s.to_string_lossy().into_owned());
            mktree_input.push_str(&format!("100644 blob {blob}\t{basename}\n"));
        }
        // Dedup by basename (keep last), sort — mktree refuses dups / unsorted.
        let mut seen = std::collections::BTreeSet::new();
        let mut dedup: Vec<(String, String)> = Vec::new();
        for line in mktree_input.lines().rev() {
            let name = line.rsplit('\t').next().unwrap_or_default().to_string();
            if !name.is_empty() && seen.insert(name.clone()) {
                dedup.push((name, line.to_string()));
            }
        }
        dedup.sort_by(|a, b| a.0.cmp(&b.0));
        let tree_input: String = dedup.iter().map(|(_, l)| format!("{l}\n")).collect();
        let tree = git_pipe(root, &["mktree"], tree_input.as_bytes())?;
        let commit = git_pipe_env(
            root,
            &["commit-tree", &tree, "-p", &prev_main, "-m", &msg.0],
            &[],
            env,
        )?;
        run_git(root, &["update-ref", "refs/heads/main", &commit])?;
        // Deliberately DO NOT advance refs/manifold/epoch/current — that is the
        // whole point: main is now ahead of the epoch (drift to be absorbed).
        Ok(())
    }

    /// Model `maw gc`'s recovery-snapshot sweep at the ref-shape level: when
    /// `recovery_snapshots` and `older_than_days == 0`, drain the recovery-ref
    /// queue (the most hostile bn-3uou setting). Plain `maw gc` (recovery
    /// snapshots off) is modelled as a no-op — it only self-heals dangling head
    /// refs, which the in-proc model never leaks in isolation.
    fn do_gc(
        &self,
        root: &Path,
        recovery_snapshots: bool,
        older_than_days: u64,
    ) -> std::io::Result<()> {
        if !recovery_snapshots || older_than_days != 0 {
            // Age-gated sweeps keep recent snapshots; the in-proc model pins
            // all recovery refs at the same synthetic clock, so only the
            // drain-everything (older_than 0) case has a modellable effect.
            return Ok(());
        }
        let listing = git_capture(
            root,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/manifold/recovery/",
            ],
        )?;
        for ref_name in listing.lines() {
            run_git(root, &["update-ref", "-d", ref_name])?;
        }
        Ok(())
    }

    fn do_destroy(&self, root: &Path, ws: &WsId, env: &[(String, String)]) -> std::io::Result<()> {
        let _ = env;
        let ws_dir = root.join("ws").join(&ws.0);
        // Pin a recovery ref BEFORE tearing refs down (well-behaved destroy).
        if let Some(tip) = resolve_ref(root, &refs_workspace_state(&ws.0))? {
            run_git(
                root,
                &[
                    "update-ref",
                    // bn-25pac: the pinned date is "<secs> +0000"; only the
                    // seconds go in the ref name. The full value contains a
                    // space, which git rejects as a bad ref name — and since
                    // that error was swallowed, EVERY in-proc destroy was a
                    // silent no-op (dir + owned refs kept, no recovery pin)
                    // until plan-step errors started failing closed.
                    &format!(
                        "refs/manifold/recovery/{}/dst-{}",
                        ws.0,
                        env.iter()
                            .find(|(k, _)| k == "GIT_AUTHOR_DATE")
                            .and_then(|(_, v)| v.split_whitespace().next())
                            .unwrap_or("0")
                    ),
                    &tip,
                ],
            )?;
        }
        // An already-absent directory is the modelled "destroy of a gone
        // workspace" no-op; any other removal failure is a harness error.
        match std::fs::remove_dir_all(&ws_dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        for owned in [
            refs_workspace_state(&ws.0),
            refs_workspace_epoch(&ws.0),
            refs_workspace_head(&ws.0),
        ] {
            // `update-ref -d` of an absent ref exits 0, so an error here is
            // a real failure.
            run_git(root, &["update-ref", "-d", &owned])?;
        }
        Ok(())
    }

    fn do_recover(
        &self,
        root: &Path,
        ws: &WsId,
        to: &WsId,
        env: &[(String, String)],
    ) -> std::io::Result<bool> {
        let _ = env;
        // Pick the first recovery ref for `ws` (deterministic ordering
        // via for-each-ref's lexicographic output) and materialize a new
        // workspace at `to` from it.
        let listing = git_capture(
            root,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                &format!("refs/manifold/recovery/{}/", ws.0),
            ],
        )?;
        let Some(first) = listing.lines().next() else {
            return Ok(false); // no recovery ref — modelled no-op
        };
        let Some((_, oid)) = first.split_once(' ') else {
            return Err(std::io::Error::other(format!(
                "malformed for-each-ref line: {first:?}"
            )));
        };
        let ws_dir = root.join("ws").join(&to.0);
        std::fs::create_dir_all(&ws_dir)?;
        std::fs::write(ws_dir.join(".maw-ws"), &to.0)?;
        run_git(root, &["update-ref", &refs_workspace_state(&to.0), oid])?;
        run_git(root, &["update-ref", &refs_workspace_epoch(&to.0), oid])?;
        run_git(root, &["update-ref", &refs_workspace_head(&to.0), oid])?;
        Ok(true)
    }

    fn apply_planted_defect(&self, defect: &PlantedDefect, _step: &PlannedStep) {
        let root = self.repo.path().to_path_buf();
        match defect {
            PlantedDefect::WorkLoss { ws } => {
                // Snapshot the tip's blob OIDs so we know what we're losing.
                let ws_ref = refs_workspace_state(ws);
                if let Ok(Some(_tip)) = resolve_ref(&root, &ws_ref) {
                    // Drop ALL owned refs + the ws dir + DO NOT pin recovery.
                    let _ = std::fs::remove_dir_all(root.join("ws").join(ws));
                    for owned in [
                        refs_workspace_state(ws),
                        refs_workspace_epoch(ws),
                        refs_workspace_head(ws),
                    ] {
                        let _ = run_git(&root, &["update-ref", "-d", &owned]);
                    }
                    // Aggressive prune so the blob is genuinely unreachable.
                    let _ = run_git(&root, &["reflog", "expire", "--expire=now", "--all"]);
                    let _ = run_git(&root, &["gc", "--prune=now", "--quiet"]);
                }
            }
            PlantedDefect::DanglingHeadRef { ws } => {
                // Rip the ws dir + state/epoch refs, but LEAVE head ref dangling.
                let _ = std::fs::remove_dir_all(root.join("ws").join(ws));
                for owned in [refs_workspace_state(ws), refs_workspace_epoch(ws)] {
                    let _ = run_git(&root, &["update-ref", "-d", &owned]);
                }
                // If there is no head ref yet (workspace was never
                // created), synthesize one pointing at the root commit so
                // B1 has something to flag.
                if matches!(resolve_ref(&root, &refs_workspace_head(ws)), Ok(None)) {
                    let _ = run_git(
                        &root,
                        &["update-ref", &refs_workspace_head(ws), &self.root_oid],
                    );
                }
            }
        }
    }

    /// Run Oracle A + Oracle B on the post-step state.
    ///
    /// FAILS CLOSED (bn-25pac): an unreadable state or an Oracle A tooling
    /// error that is not positively classified as INFRA is a
    /// [`StepVerdict::HarnessError`], never `Clean`. (Oracle A/B findings
    /// — including their non-infra `GitError` findings — are violations as
    /// before.)
    fn check_oracles(&mut self, step_index: usize) -> StepVerdict {
        let state = match self.observe_state() {
            Ok(s) => s,
            Err(e) => return StepVerdict::HarnessError(e),
        };
        if std::env::var("MAW_INPROC_DEBUG").is_ok() {
            eprintln!(
                "[in_proc] step {step_index}: refs={} workspaces={} W={} U={}",
                state.durable_refs.len(),
                state.workspaces.len(),
                self.oracle_a.witness_count(),
                self.oracle_a.reachable_count(),
            );
            for (n, s) in &state.workspaces {
                eprintln!("  ws {n}: head={} exists={}", s.head_oid, s.exists);
            }
        }
        match self.oracle_a.check_step(&state, step_index) {
            Ok(StepReport {
                violation: Some(v), ..
            }) => {
                if let Some(f) = oracle_a_infra(&v) {
                    infra::raise(f);
                }
                self.note_oracle_a_checked(&state);
                return StepVerdict::OracleA(OracleAClass::from_violation(&v));
            }
            Ok(rep) => {
                self.note_oracle_a_checked(&state);
                if std::env::var("MAW_INPROC_DEBUG").is_ok() {
                    eprintln!(
                        "[in_proc] step {step_index} oracle A: clean (full_rescan={}, W={}, U={})",
                        rep.did_full_rescan, rep.witness_count, rep.reachable_count
                    );
                }
            }
            Err(err) => {
                infra::raise_if_infra_text(&err.to_string(), "oracle A check");
                return StepVerdict::HarnessError(HarnessErrorClass::new(
                    "oracle_a_check",
                    err.to_string(),
                ));
            }
        }
        // Oracle B.
        let verdict = match first_oracle_b_finding(oracle_b::check(self.repo.path())) {
            Ok(Some(v)) => StepVerdict::OracleB(OracleBClass::from_violation(&v)),
            Ok(None) => StepVerdict::Clean,
            Err(f) => infra::raise(f),
        };
        self.stats.oracle_b_checks += 1;
        verdict
    }
}

/// Stable HarnessError site for a failed plan step.
fn op_site(op: &Op) -> &'static str {
    match op {
        Op::WsCreate { .. } => "apply_op:WsCreate",
        Op::EditFiles { .. } => "apply_op:EditFiles",
        Op::Commit { .. } => "apply_op:Commit",
        Op::Merge { .. } => "apply_op:Merge",
        Op::Sync { .. } => "apply_op:Sync",
        Op::Advance { .. } => "apply_op:Advance",
        Op::Destroy { .. } => "apply_op:Destroy",
        Op::Recover { .. } => "apply_op:Recover",
        Op::OutOfMawCommit { .. } => "apply_op:OutOfMawCommit",
        Op::DirtyTrunkWrite { .. } => "apply_op:DirtyTrunkWrite",
        Op::CorruptWorktreeStatMasked { .. } => "apply_op:CorruptWorktreeStatMasked",
        Op::Gc { .. } => "apply_op:Gc",
    }
}

/// Outcome of [`InProcDriver::drive`].
#[derive(Clone, Debug)]
pub struct DriveOutcome {
    /// The first violating verdict, or `Clean`.
    pub verdict: StepVerdict,
    /// Number of plan steps actually replayed (the last one is the one
    /// that tripped, if any).
    pub steps_replayed: usize,
    /// Evidence counters (bn-25pac): how much the oracles actually judged.
    pub stats: DriveStats,
}

// ---------------------------------------------------------------------------
// Infra triage of oracle tooling failures (bn-30v6e)
// ---------------------------------------------------------------------------

/// `Some` iff `v` is Oracle A's own tooling failure (`GitError`) AND its
/// stderr positively classifies as a host resource failure. Every oracle
/// finding, and every other `GitError`, is `None` (fail closed).
fn oracle_a_infra(v: &AssuranceViolation) -> Option<InfraFailure> {
    match v {
        AssuranceViolation::GitError {
            check,
            command,
            stderr,
        } => infra::classify_text(stderr).map(|kind| InfraFailure {
            kind,
            detail: format!("oracle A {check}: `{command}`: {}", stderr.trim()),
        }),
        _ => None,
    }
}

/// Pick the Oracle B verdict to report.
///
/// Returns the first violation that is NOT an infra-classified `GitError`
/// (a real finding always wins, and a non-infra `GitError` stays a
/// violation). Only when every violation is an infra `GitError` does it
/// return `Err`, so the harness aborts the seed as INFRA.
fn first_oracle_b_finding(
    bvs: Vec<OracleBViolation>,
) -> Result<Option<OracleBViolation>, InfraFailure> {
    let mut first_infra = None;
    for v in bvs {
        let infra = match &v {
            OracleBViolation::GitError {
                check,
                command,
                stderr,
            } => infra::classify_text(stderr).map(|kind| InfraFailure {
                kind,
                detail: format!("oracle B {check}: `{command}`: {}", stderr.trim()),
            }),
            _ => None,
        };
        match infra {
            Some(f) => {
                first_infra.get_or_insert(f);
            }
            None => return Ok(Some(v)),
        }
    }
    first_infra.map_or(Ok(None), Err)
}

// ---------------------------------------------------------------------------
// Small git helpers (driver-private; not the oracle's verifier carveout)
// ---------------------------------------------------------------------------

/// Run `cmd` and return its output. A spawn error, or a non-zero exit whose
/// stderr names a host resource failure, aborts the seed as INFRA
/// (bn-30v6e); any other spawn error is returned (the caller fails closed).
fn git_output(cmd: &mut Command, args: &[&str]) -> std::io::Result<std::process::Output> {
    match cmd.output() {
        Ok(out) => {
            if !out.status.success() {
                infra::raise_if_infra_text(
                    &String::from_utf8_lossy(&out.stderr),
                    &format!("git {}", args.join(" ")),
                );
            }
            Ok(out)
        }
        Err(err) => {
            infra::raise_if_infra_io(&err, &format!("spawn git {}", args.join(" ")));
            Err(err)
        }
    }
}

fn pinned_env(git_time: i64) -> Vec<(String, String)> {
    // The pinned-clock contract (`notes/sg1-dst-architecture.md` §5.2):
    // export `GIT_AUTHOR_DATE` and `GIT_COMMITTER_DATE` derived from the
    // plan-step's `git_time`, so commit OIDs are a pure function of seed.
    let v = format!("{git_time} +0000");
    vec![
        ("GIT_AUTHOR_DATE".to_string(), v.clone()),
        ("GIT_COMMITTER_DATE".to_string(), v),
    ]
}

fn refs_workspace_state(ws: &str) -> String {
    format!("refs/manifold/ws/{ws}")
}
fn refs_workspace_epoch(ws: &str) -> String {
    format!("refs/manifold/epoch/ws/{ws}")
}
fn refs_workspace_head(ws: &str) -> String {
    format!("refs/manifold/head/{ws}")
}

fn run_git(root: &Path, args: &[&str]) -> std::io::Result<()> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

fn run_git_env(root: &Path, args: &[&str], env: &[(String, String)]) -> std::io::Result<()> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(root);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

/// Run git and return trimmed stdout. A non-zero exit is an error
/// (bn-25pac: previously it silently returned empty stdout).
fn git_capture(root: &Path, args: &[&str]) -> std::io::Result<String> {
    let out = git_output(Command::new("git").args(args).current_dir(root), args)?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "git {} failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Resolve `rev` to an OID. `Ok(None)` ONLY when git positively reports
/// "no such revision" (`rev-parse --verify --quiet` exits 1 with empty
/// stderr); every other failure is an error (bn-25pac: previously any
/// failure — including a spawn failure — read as "absent").
fn resolve_ref(root: &Path, rev: &str) -> std::io::Result<Option<String>> {
    let args = ["rev-parse", "--verify", "--quiet", rev];
    let out = git_output(Command::new("git").args(args).current_dir(root), &args)?;
    if out.status.success() {
        let oid = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if oid.is_empty() {
            return Err(std::io::Error::other(format!(
                "git rev-parse --verify {rev}: empty output"
            )));
        }
        return Ok(Some(oid));
    }
    if out.status.code() == Some(1) && out.stderr.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    Err(std::io::Error::other(format!(
        "git rev-parse --verify {rev} failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    )))
}

fn git_pipe(root: &Path, args: &[&str], stdin: &[u8]) -> std::io::Result<String> {
    git_pipe_env(root, args, stdin, &[])
}

fn git_pipe_env(
    root: &Path,
    args: &[&str],
    stdin: &[u8],
    env: &[(String, String)],
) -> std::io::Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let ctx = format!("git {}", args.join(" "));
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            infra::raise_if_infra_io(&err, &format!("spawn {ctx}"));
            return Err(err);
        }
    };
    // A git that dies early (e.g. on EDQUOT) closes stdin → EPIPE here;
    // the real cause is in its stderr, so collect that before failing.
    let write_err = if stdin.is_empty() {
        None
    } else {
        child
            .stdin
            .as_mut()
            .ok_or_else(|| std::io::Error::other(format!("{ctx}: no stdin pipe")))?
            .write_all(stdin)
            .err()
    };
    let out = match child.wait_with_output() {
        Ok(out) => out,
        Err(err) => {
            infra::raise_if_infra_io(&err, &format!("wait {ctx}"));
            return Err(err);
        }
    };
    if let Some(err) = write_err {
        infra::raise_if_infra_text(&String::from_utf8_lossy(&out.stderr), &ctx);
        infra::raise_if_infra_io(&err, &format!("{ctx} stdin"));
        return Err(std::io::Error::other(format!(
            "{ctx}: stdin write failed: {err}"
        )));
    }
    if !out.status.success() {
        infra::raise_if_infra_text(&String::from_utf8_lossy(&out.stderr), &ctx);
        return Err(std::io::Error::other(format!(
            "{ctx} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_hash_object_stdin(root: &Path, content: &[u8]) -> std::io::Result<String> {
    git_pipe(root, &["hash-object", "-w", "--stdin"], content)
}

fn walk_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk_files_inner(dir, &mut out)?;
    out.sort();
    Ok(out)
}
fn walk_files_inner(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            walk_files_inner(&p, out)?;
        } else {
            out.push(p);
        }
    }
    Ok(())
}

// Unused but reserved for tests that want to inspect frontier evolution.
#[allow(dead_code)]
fn list_refs(root: &Path) -> BTreeSet<String> {
    git_capture(root, &["for-each-ref", "--format=%(refname)"])
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod infra_triage_tests {
    //! bn-30v6e: infra classification of oracle tooling failures must never
    //! swallow an oracle finding.
    use super::*;
    use crate::infra::InfraKind;

    fn b_git_error(stderr: &str) -> OracleBViolation {
        OracleBViolation::GitError {
            check: "B4",
            command: "git cat-file --batch-check".into(),
            stderr: stderr.into(),
        }
    }
    fn b1() -> OracleBViolation {
        OracleBViolation::DanglingHeadRef {
            workspace: "ws-0".into(),
            ref_name: "refs/manifold/head/ws-0".into(),
            oid: "abc".into(),
        }
    }

    #[test]
    fn oracle_b_empty_is_clean() {
        assert!(matches!(first_oracle_b_finding(vec![]), Ok(None)));
    }

    #[test]
    fn oracle_b_real_finding_is_kept() {
        let got = first_oracle_b_finding(vec![b1()]).expect("not infra");
        assert!(matches!(
            got,
            Some(OracleBViolation::DanglingHeadRef { .. })
        ));
    }

    #[test]
    fn oracle_b_real_finding_wins_over_infra_git_error() {
        let got = first_oracle_b_finding(vec![b_git_error("Disk quota exceeded"), b1()])
            .expect("a real finding must never be reclassified as infra");
        assert!(matches!(
            got,
            Some(OracleBViolation::DanglingHeadRef { .. })
        ));
    }

    #[test]
    fn oracle_b_non_infra_git_error_stays_a_violation() {
        let got = first_oracle_b_finding(vec![b_git_error("fatal: bad object abc")])
            .expect("non-infra GitError is not infra");
        assert!(matches!(got, Some(OracleBViolation::GitError { .. })));
    }

    #[test]
    fn oracle_b_only_infra_git_errors_is_infra() {
        let err = first_oracle_b_finding(vec![
            b_git_error("fatal: unable to write: No space left on device"),
            b_git_error("Disk quota exceeded"),
        ])
        .expect_err("all-infra GitErrors classify as infra");
        assert_eq!(err.kind, InfraKind::StorageFull);
    }

    #[test]
    fn oracle_a_only_git_error_with_infra_stderr_classifies() {
        let infra_err = AssuranceViolation::GitError {
            check: "oracle_a::rev_list_objects".into(),
            command: "git rev-list".into(),
            stderr: "fatal: Too many open files".into(),
        };
        assert_eq!(
            oracle_a_infra(&infra_err).map(|f| f.kind),
            Some(InfraKind::TooManyOpenFiles)
        );
        let plain = AssuranceViolation::GitError {
            check: "oracle_a::rev_list_objects".into(),
            command: "git rev-list".into(),
            stderr: "fatal: bad object".into(),
        };
        assert_eq!(oracle_a_infra(&plain), None);
        // A finding whose text happens to mention quota is still a finding.
        let finding = AssuranceViolation::ReachabilityLost {
            oid: "Disk quota exceeded".into(),
            previous_ref: "No space left on device".into(),
        };
        assert_eq!(oracle_a_infra(&finding), None);
    }

    /// A real planted violation still surfaces as a violation through the
    /// infra-aware driver (the classifier did not swallow it).
    #[test]
    fn planted_work_loss_still_trips_through_infra_aware_driver() {
        let (plan, planted) = crate::shrinker_tests::planted_oracle_a_fixture();
        let mut driver = InProcDriver::new().expect("driver").with_planted(planted);
        let out = driver.drive(&plan);
        assert!(out.verdict.is_violation(), "{:?}", out.verdict);
    }
}

#[cfg(test)]
mod fail_closed_tests {
    //! bn-25pac: oracle/harness errors and unreadable state must never be
    //! counted as a clean seed, and a clean seed must carry non-vacuous
    //! evidence.
    use super::*;
    use crate::scenario::{ConditionProfile, GIT_TIME_BASE_FOR_DRIVER, generate_plan};

    fn step(index: usize, op: Op) -> PlannedStep {
        PlannedStep {
            index,
            op,
            fault: FaultSpec::None,
            git_time: GIT_TIME_BASE_FOR_DRIVER + 100 * (i64::try_from(index).unwrap() + 1),
        }
    }

    /// create ws-7, edit, commit (no plants).
    fn commit_plan() -> ScenarioPlan {
        let ws = WsId::slot(7);
        ScenarioPlan {
            seed: 0x25AC,
            profile: ConditionProfile::default(),
            steps: vec![
                step(
                    0,
                    Op::WsCreate {
                        ws: ws.clone(),
                        from: BaseRef::Main,
                    },
                ),
                step(
                    1,
                    Op::EditFiles {
                        ws: ws.clone(),
                        files: vec![FileEdit {
                            path: "doc.txt".into(),
                            content: "bn-25pac witness\n".into(),
                        }],
                    },
                ),
                step(
                    2,
                    Op::Commit {
                        ws,
                        msg: Seeded("bn-25pac".into()),
                    },
                ),
            ],
        }
    }

    fn harness_site(v: &StepVerdict) -> Option<&'static str> {
        match v {
            StepVerdict::HarnessError(h) => Some(h.site),
            _ => None,
        }
    }

    fn git_ok(root: &Path, args: &[&str]) {
        run_git(root, args).expect("git");
    }

    #[test]
    fn normal_drive_is_clean_with_real_evidence() {
        let mut d = InProcDriver::new().expect("driver");
        let out = d.drive(&commit_plan());
        assert!(
            matches!(out.verdict, StepVerdict::Clean),
            "{:?}",
            out.verdict
        );
        // one per step + the final check
        assert_eq!(out.stats.oracle_a_checks, 4, "{:?}", out.stats);
        assert_eq!(out.stats.oracle_b_checks, 4, "{:?}", out.stats);
        assert_eq!(out.stats.workspaces_created, 1);
        assert_eq!(out.stats.workspaces_observed, 1);
        assert_eq!(out.stats.commits_made, 1);
        assert_eq!(out.stats.commits_observed, 1);
        assert!(out.stats.witnesses > 0, "{:?}", out.stats);
        assert_eq!(out.stats.vacuity(), None);
    }

    #[test]
    fn generated_plans_are_clean_and_non_vacuous() {
        for seed in 0..6u64 {
            let plan = generate_plan(seed, &ConditionProfile::default(), 32);
            let mut d = InProcDriver::new().expect("driver");
            let out = d.drive(&plan);
            assert!(
                matches!(out.verdict, StepVerdict::Clean),
                "seed {seed}: {:?}",
                out.verdict
            );
            assert_eq!(
                out.stats.oracle_a_checks,
                plan.steps.len() + 1,
                "seed {seed}"
            );
            assert_eq!(
                out.stats.oracle_b_checks,
                plan.steps.len() + 1,
                "seed {seed}"
            );
            assert!(
                out.stats.workspaces_observed > 0,
                "seed {seed}: {:?}",
                out.stats
            );
        }
    }

    /// bn-25pac finding: the recovery ref name used to embed the pinned
    /// date's " +0000", git rejected it, the error was swallowed, and every
    /// in-proc destroy was a silent no-op.
    #[test]
    fn destroy_really_destroys_and_pins_recovery() {
        let mut plan = commit_plan();
        plan.steps.push(step(
            3,
            Op::Destroy {
                ws: WsId::slot(7),
                force: false,
            },
        ));
        let mut d = InProcDriver::new().expect("driver");
        let out = d.drive(&plan);
        assert!(
            matches!(out.verdict, StepVerdict::Clean),
            "{:?}",
            out.verdict
        );
        let root = d.repo_root();
        assert!(!root.join("ws/ws-7").exists(), "ws dir must be gone");
        assert_eq!(resolve_ref(root, "refs/manifold/ws/ws-7").unwrap(), None);
        let pins = git_capture(
            root,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/manifold/recovery/ws-7/",
            ],
        )
        .unwrap();
        assert_eq!(
            pins.lines().count(),
            1,
            "exactly one recovery pin: {pins:?}"
        );
    }

    #[test]
    fn capture_state_error_is_harness_error() {
        let mut d = InProcDriver::new().expect("driver");
        let _ = d.drive(&commit_plan());
        // Make the repo unreadable to `git for-each-ref`.
        std::fs::remove_file(d.repo_root().join(".git/HEAD")).unwrap();
        let v = d.check_oracles(3);
        assert_eq!(harness_site(&v), Some("capture_state"), "{v:?}");
    }

    #[test]
    fn oracle_a_tooling_error_is_harness_error() {
        let mut d = InProcDriver::new().expect("driver");
        let _ = d.drive(&commit_plan());
        let root = d.repo_root().to_path_buf();
        // Point ws-7's base epoch at a BLOB: capture_state still reads the
        // refs fine, but Oracle A's `git diff --raw <base> <tip>` fails.
        let blob = git_hash_object_stdin(&root, b"not a commit\n").unwrap();
        git_ok(&root, &["update-ref", "refs/manifold/epoch/ws/ws-7", &blob]);
        let v = d.check_oracles(3);
        assert_eq!(harness_site(&v), Some("oracle_a_check"), "{v:?}");
    }

    #[test]
    fn extant_ws_without_state_ref_is_harness_error() {
        let mut d = InProcDriver::new().expect("driver");
        let _ = d.drive(&commit_plan());
        std::fs::create_dir_all(d.repo_root().join("ws/ghost")).unwrap();
        let v = d.check_oracles(3);
        assert_eq!(harness_site(&v), Some("ws_state_ref_missing"), "{v:?}");
    }

    #[test]
    fn plan_step_error_is_harness_error() {
        let mut d = InProcDriver::new().expect("driver");
        // A FILE where ws-7's directory must go: WsCreate cannot apply.
        std::fs::write(d.repo_root().join("ws/ws-7"), "obstruction").unwrap();
        let out = d.drive(&commit_plan());
        assert_eq!(
            harness_site(&out.verdict),
            Some("apply_op:WsCreate"),
            "{:?}",
            out.verdict
        );
        assert_eq!(out.steps_replayed, 1);
    }

    #[test]
    fn plan_step_error_is_harness_error_in_fast_mode_too() {
        let mut d = InProcDriver::new().expect("driver");
        std::fs::write(d.repo_root().join("ws/ws-7"), "obstruction").unwrap();
        let out = d.drive_fast(&commit_plan());
        assert_eq!(harness_site(&out.verdict), Some("apply_op:WsCreate"));
    }

    #[test]
    fn blind_workspace_observation_is_caught() {
        let mut d = InProcDriver::new().expect("driver");
        d.test_blind_ws_observation = true;
        let out = d.drive(&commit_plan());
        assert_eq!(
            harness_site(&out.verdict),
            Some("ws_unobserved"),
            "{:?}",
            out.verdict
        );
    }

    #[test]
    fn vacuity_guard_fires_on_each_vacuous_shape() {
        let ok = DriveStats {
            oracle_a_checks: 4,
            oracle_b_checks: 4,
            witnesses: 2,
            workspaces_created: 1,
            workspaces_observed: 1,
            commits_made: 1,
            commits_observed: 1,
        };
        assert_eq!(ok.vacuity(), None);
        let site = |s: DriveStats| s.vacuity().map(|h| h.site);
        assert_eq!(
            site(DriveStats {
                oracle_a_checks: 0,
                ..ok
            }),
            Some("vacuous_checks")
        );
        assert_eq!(
            site(DriveStats {
                oracle_b_checks: 0,
                ..ok
            }),
            Some("vacuous_checks")
        );
        assert_eq!(
            site(DriveStats {
                workspaces_observed: 0,
                ..ok
            }),
            Some("vacuous_workspaces")
        );
        assert_eq!(
            site(DriveStats { witnesses: 0, ..ok }),
            Some("vacuous_witnesses")
        );
        // A plan that never created/committed anything is not vacuous.
        assert_eq!(
            site(DriveStats {
                witnesses: 0,
                workspaces_created: 0,
                workspaces_observed: 0,
                commits_made: 0,
                commits_observed: 0,
                ..ok
            }),
            None
        );
    }

    /// The guard is wired into the drive: a seed whose final verdict is
    /// clean but whose evidence is vacuous is reported as HarnessError.
    #[test]
    fn vacuity_guard_is_wired_into_drive() {
        let mut d = InProcDriver::new().expect("driver");
        // Empty plan → drive still runs the final check; zero evidence of
        // anything is fine (nothing created) — so force a vacuous shape:
        // pretend a commit was observed but Oracle A harvested nothing.
        d.stats.commits_observed = 1;
        let plan = ScenarioPlan {
            seed: 0,
            profile: ConditionProfile::default(),
            steps: vec![step(0, Op::Sync { ws: WsId::slot(0) })],
        };
        let out = d.drive(&plan);
        assert_eq!(
            harness_site(&out.verdict),
            Some("vacuous_witnesses"),
            "{:?}",
            out.verdict
        );
    }

    /// bn-25pac finding (Oracle A): when the step that loses work (refs
    /// dropped + pruned) ALSO advances another frontier root, Oracle A's
    /// incremental `git rev-list <new> ^<prev roots>` hit `bad object` on
    /// the pruned previous root and errored — which the driver used to
    /// swallow as clean, so the loss went unreported. It must be judged.
    #[test]
    fn work_loss_with_concurrent_root_advance_is_judged() {
        let mut plan = commit_plan();
        let ws6 = WsId::slot(6);
        plan.steps.push(step(
            3,
            Op::WsCreate {
                ws: ws6.clone(),
                from: BaseRef::Main,
            },
        ));
        plan.steps.push(step(
            4,
            Op::EditFiles {
                ws: ws6.clone(),
                files: vec![FileEdit {
                    path: "other.txt".into(),
                    content: "ws-6 content\n".into(),
                }],
            },
        ));
        plan.steps.push(step(
            5,
            Op::Commit {
                ws: ws6,
                msg: Seeded("advance ws-6".into()),
            },
        ));
        let planted = vec![PlantedDefect::WorkLoss { ws: "ws-7".into() }];
        for fast in [false, true] {
            let mut d = InProcDriver::new()
                .expect("driver")
                .with_planted(planted.clone());
            let out = if fast {
                d.drive_fast(&plan)
            } else {
                d.drive(&plan)
            };
            assert!(
                matches!(&out.verdict, StepVerdict::OracleA(a) if a.kind == "ReachabilityLost"),
                "fast={fast}: {:?}",
                out.verdict
            );
        }
    }

    #[test]
    fn harness_error_same_class_is_by_site() {
        let a = StepVerdict::HarnessError(HarnessErrorClass::new("capture_state", "x /tmp/a"));
        let b = StepVerdict::HarnessError(HarnessErrorClass::new("capture_state", "y /tmp/b"));
        let c = StepVerdict::HarnessError(HarnessErrorClass::new("oracle_a_check", "x"));
        assert!(a.same_class(&b));
        assert!(!a.same_class(&c));
        assert!(a.is_violation());
        assert!(!a.same_class(&StepVerdict::Clean));
    }
}
