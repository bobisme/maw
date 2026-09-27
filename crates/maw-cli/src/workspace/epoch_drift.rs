//! Epoch-drift detection for `maw doctor` and `maw status` (bn-1ieb, SG4).
//!
//! `refs/manifold/epoch/current` is the integration-state pointer the merge
//! engine treats as the diff3 base for every in-flight workspace. When the
//! configured branch (`refs/heads/<branch>`) moves forward without the epoch
//! ref following — typically because someone ran `git commit` directly on
//! the default workspace and forgot to advance — every subsequent
//! `maw ws merge` errors out with "Target branch '...' has diverged from the
//! current epoch" and tells the agent to run `maw epoch sync`.
//!
//! The friction this causes is named `epoch_sync_required` in
//! `MawVerbAttribution` (see
//! `crates/maw-bench-metrics/src/attribution.rs`); the cluster's wasted-turn
//! cost is the SG4 hardening target for this bone.
//!
//! # What this module exposes
//!
//! - [`EpochDriftKind`] — a four-state classifier:
//!   - [`EpochDriftKind::InSync`]: nothing to do.
//!   - [`EpochDriftKind::FfAbsorbable`]: branch is strictly ahead of epoch
//!     via fast-forward AND no in-flight workspace's touched paths overlap
//!     the FF range — the next `maw ws merge` will auto-absorb it (see
//!     [`super::ff_absorb`]), and even outside of merge the standalone
//!     `maw epoch sync` (or [`auto_advance_if_safe`]) is provably correct.
//!   - [`EpochDriftKind::FfBlocked`]: branch is strictly ahead via fast-
//!     forward but at least one in-flight workspace has touched a path in
//!     the FF range — auto-advance would change the diff3 base under their
//!     feet, so the agent must coordinate (resolve / merge those
//!     workspaces) before advancing.
//!   - [`EpochDriftKind::Diverged`]: epoch and branch have forked (neither
//!     is an ancestor of the other) OR epoch is strictly ahead of branch
//!     (e.g. a merge commit was reset). Manual recovery required.
//!
//! - [`classify_drift`] — pure I/O wrapper that loads the two refs, calls
//!   [`super::ff_absorb`] for the safety predicate, and returns the kind +
//!   12-char-prefixed OIDs for display.
//!
//! - [`auto_advance_if_safe`] — convenience entry-point: classify, and if
//!   [`EpochDriftKind::FfAbsorbable`], advance the epoch + per-workspace
//!   baselines in one shot. Used by `maw doctor` to repair epoch drift
//!   without requiring the agent to remember the `maw epoch sync` verb.
//!
//! # Layout-agnostic note (T3.2 / bn-2sw3 coordination)
//!
//! This module reads refs via `maw_core::refs` (the abstract ref API) and
//! delegates workspace-touched-path queries to existing collectors via the
//! `WorkspaceBackend` trait. No hard-coded `ws/` or `.manifold/` path
//! strings are used here. When T3.2 introduces the layout-flavor enum the
//! callers it touches (currently `repo_root()` + `MawConfig::load`) absorb
//! the change; this module remains layout-agnostic.

use std::path::Path;

use anyhow::{Result, anyhow};
use serde::Serialize;

use maw_core::refs as manifold_refs;
use maw_git::{GitOid as MawGitOid, GitRepo as _};

/// Four-state classification of `epoch` vs `branch` HEAD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EpochDriftKind {
    /// Epoch OID equals branch HEAD OID. Nothing to do.
    InSync,
    /// Epoch is a strict ancestor of branch HEAD AND no in-flight workspace
    /// touches any path in the FF range. The next `maw ws merge` would
    /// auto-absorb this; a standalone `maw epoch sync` is also safe.
    FfAbsorbable,
    /// Epoch is a strict ancestor of branch HEAD BUT at least one in-flight
    /// workspace's touched paths intersect the FF range. Auto-advance is
    /// unsafe — running `maw epoch sync` here would silently change the
    /// diff3 base for those workspaces.
    FfBlocked,
    /// Epoch and branch HEAD have forked (neither is an ancestor of the
    /// other) OR epoch is strictly ahead of branch (e.g. branch was reset
    /// after a merge). Manual recovery required.
    Diverged,
}

impl EpochDriftKind {
    /// Short slug for serialization and tests. Pinned so JSON consumers
    /// can rely on stable strings. Also used by `ws merge --check`'s NOTE
    /// wording (bn-3eew), which kebab-cases it for the `CheckResult`
    /// `drift_classification` field.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::InSync => "in_sync",
            Self::FfAbsorbable => "ff_absorbable",
            Self::FfBlocked => "ff_blocked",
            Self::Diverged => "diverged",
        }
    }

    /// True iff drift is present (i.e. not `InSync`).
    #[must_use]
    pub const fn has_drift(self) -> bool {
        !matches!(self, Self::InSync)
    }

    /// True iff `auto_advance_if_safe` would advance the epoch without
    /// further coordination.
    #[must_use]
    pub const fn is_auto_advanceable(self) -> bool {
        matches!(self, Self::FfAbsorbable)
    }
}

/// Structured drift report for machine-readable output (`maw status --json`)
/// and `maw doctor` checks.
#[derive(Debug, Clone, Serialize)]
pub struct EpochDriftReport {
    pub kind: EpochDriftKind,
    /// Short OID (12-char prefix) of `refs/manifold/epoch/current`.
    pub epoch_short: String,
    /// Short OID (12-char prefix) of `refs/heads/<branch>`.
    pub branch_short: String,
    /// Configured branch name, e.g. `"main"`.
    pub branch: String,
    /// Number of commits in the FF range `epoch..branch`. `0` when not a
    /// pure FF (i.e. `Diverged` or `InSync`).
    pub ff_commit_count: usize,
    /// Workspaces blocking auto-advance (empty unless `kind == FfBlocked`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blocking_workspaces: Vec<String>,
}

impl EpochDriftReport {
    /// Build the canonical "next command" suggestion the report should
    /// surface to a confused agent. Returns `None` when there's nothing to
    /// recommend (in sync). Kept as a method so JSON consumers and future
    /// renderers (TUI, event log) all share the same routing logic.
    #[must_use]
    #[allow(dead_code)]
    pub const fn next_command(&self) -> Option<&'static str> {
        match self.kind {
            EpochDriftKind::InSync => None,
            EpochDriftKind::FfAbsorbable => Some("maw epoch sync"),
            EpochDriftKind::FfBlocked => {
                Some("maw ws merge <blocking-workspace> --into default --check")
            }
            EpochDriftKind::Diverged => Some("maw doctor"),
        }
    }
}

/// Look up both refs, classify the relationship, and (for FF cases) run
/// the [`super::ff_absorb`] safety predicate.
///
/// # Errors
/// Returns an error if either ref lookup or the ancestry walk fails. Returns
/// `Ok(None)` (not an error) when the epoch ref is unset — that's a
/// pre-`maw init` state and a separate doctor check covers it.
pub fn classify_drift<B>(root: &Path, branch: &str, backend: &B) -> Result<Option<EpochDriftReport>>
where
    B: maw_core::backend::WorkspaceBackend,
    B::Error: std::fmt::Display,
{
    Ok(classify_drift_with_oids(root, branch, backend)?.map(|c| c.report))
}

/// A drift classification together with the exact full OIDs it judged.
///
/// `auto_advance_if_safe` needs these so the epoch write is a CAS from the
/// epoch the safety predicate saw to the branch tip it saw (bn-32g8), never a
/// re-read that could describe a different state than the one classified.
struct ClassifiedDrift {
    report: EpochDriftReport,
    epoch_oid: maw_core::model::types::GitOid,
    branch_oid: maw_core::model::types::GitOid,
}

fn classify_drift_with_oids<B>(
    root: &Path,
    branch: &str,
    backend: &B,
) -> Result<Option<ClassifiedDrift>>
where
    B: maw_core::backend::WorkspaceBackend,
    B::Error: std::fmt::Display,
{
    let Some(epoch_oid) = manifold_refs::read_epoch_current(root)
        .map_err(|e| anyhow!("failed to read epoch ref: {e}"))?
    else {
        return Ok(None);
    };
    let branch_ref = format!("refs/heads/{branch}");
    let Some(branch_oid) = manifold_refs::read_ref(root, &branch_ref)
        .map_err(|e| anyhow!("failed to read branch ref '{branch_ref}': {e}"))?
    else {
        // Branch missing entirely: `maw doctor`'s `check_default_workspace`
        // / `check_git_head` will cover it. We don't have enough to
        // classify drift.
        return Ok(None);
    };

    let epoch_short = short_oid(epoch_oid.as_str());
    let branch_short = short_oid(branch_oid.as_str());

    if epoch_oid.as_str() == branch_oid.as_str() {
        return Ok(Some(ClassifiedDrift::new(
            epoch_oid,
            branch_oid,
            EpochDriftReport {
                kind: EpochDriftKind::InSync,
                epoch_short,
                branch_short,
                branch: branch.to_owned(),
                ff_commit_count: 0,
                blocking_workspaces: Vec::new(),
            },
        )));
    }

    // Open the gix repo via the shared FF-absorb helper so doctor/status
    // share the same repo-open path as merge.
    let repo = super::ff_absorb::open_repo(root)?;
    let epoch_git: MawGitOid = epoch_oid
        .as_str()
        .parse()
        .map_err(|e| anyhow!("invalid epoch OID '{}': {e}", epoch_oid.as_str()))?;
    let branch_git: MawGitOid = branch_oid
        .as_str()
        .parse()
        .map_err(|e| anyhow!("invalid branch OID '{}': {e}", branch_oid.as_str()))?;

    let epoch_is_ancestor = super::ff_absorb::is_strict_ancestor(&repo, &epoch_git, &branch_git)?;
    if !epoch_is_ancestor {
        return Ok(Some(ClassifiedDrift::new(
            epoch_oid,
            branch_oid,
            EpochDriftReport {
                kind: EpochDriftKind::Diverged,
                epoch_short,
                branch_short,
                branch: branch.to_owned(),
                ff_commit_count: 0,
                blocking_workspaces: Vec::new(),
            },
        )));
    }

    // Pure FF: count commits + run the safety predicate over the current
    // in-flight workspaces.
    let ff_count = repo
        .walk_commits(epoch_git, branch_git, false)
        .map_or(0, |walk| walk.len());
    let ff_paths = super::ff_absorb::compute_ff_changed_paths(&repo, &epoch_git, &branch_git)?;

    let workspaces_info = backend
        .list()
        .map_err(|e| anyhow!("failed to list workspaces: {e}"))?;

    let mut ws_touched: Vec<super::ff_absorb::WorkspaceTouchedPaths> = Vec::new();
    for info in workspaces_info {
        // Skip the default workspace: by construction it tracks the
        // configured branch directly, so any "touched" paths it shows up
        // with are themselves the FF range. Including it would
        // tautologically self-block (mirrors the target-workspace carve-
        // out in `merge.rs::reconcile_epoch_with_branch`).
        if info.id.as_str() == super::DEFAULT_WORKSPACE {
            continue;
        }
        // collect_touched_workspace fails closed if a workspace's state
        // is unreadable; treat that as "no opinion" and move on rather
        // than blowing up doctor over a single bad sibling.
        let Ok(touched) = super::touched::collect_touched_workspace(backend, &info.id) else {
            continue;
        };
        ws_touched.push(super::ff_absorb::WorkspaceTouchedPaths {
            name: touched.workspace,
            paths: touched.touched_paths.into_iter().collect(),
        });
    }

    let decision = super::ff_absorb::evaluate_ff_safety(&ff_paths, &ws_touched);
    let report = match decision {
        super::ff_absorb::FfAbsorbDecision::Safe => EpochDriftReport {
            kind: EpochDriftKind::FfAbsorbable,
            epoch_short,
            branch_short,
            branch: branch.to_owned(),
            ff_commit_count: ff_count,
            blocking_workspaces: Vec::new(),
        },
        super::ff_absorb::FfAbsorbDecision::Blocked {
            affected_workspaces,
        } => EpochDriftReport {
            kind: EpochDriftKind::FfBlocked,
            epoch_short,
            branch_short,
            branch: branch.to_owned(),
            ff_commit_count: ff_count,
            blocking_workspaces: affected_workspaces,
        },
    };
    Ok(Some(ClassifiedDrift::new(epoch_oid, branch_oid, report)))
}

impl ClassifiedDrift {
    const fn new(
        epoch_oid: maw_core::model::types::GitOid,
        branch_oid: maw_core::model::types::GitOid,
        report: EpochDriftReport,
    ) -> Self {
        Self {
            report,
            epoch_oid,
            branch_oid,
        }
    }
}

/// Auto-advance the epoch when `classify_drift` returns
/// [`EpochDriftKind::FfAbsorbable`]. No-op for any other kind.
///
/// On a successful advance, writes the new OID to
/// `refs/manifold/epoch/current` AND to the default workspace's per-
/// workspace baseline (`refs/manifold/epoch/ws/<default>`). Sibling
/// workspace baselines are NOT touched here — they're updated by the
/// merge engine when it absorbs the FF (see
/// `merge.rs::reconcile_epoch_with_branch`). Outside of merge, leaving
/// sibling baselines at the old epoch is safe: subsequent merges will
/// observe `is_stale = true` for those siblings and run the normal
/// rebase path, which is the correct behavior (they need to know the
/// epoch moved).
///
/// # Concurrency (bn-32g8)
///
/// The whole classify → write sequence runs under the repo-level epoch lock
/// (the same lock `maw epoch sync` and `maw ws merge` hold), and the epoch
/// write is a CAS from the exact epoch OID the safety predicate judged to the
/// exact branch OID it judged. Previously this re-read the branch and did a
/// plain write with no lock, so a `ws merge` that committed in between had its
/// epoch advance overwritten (epoch regressed behind the branch). Modelled in
/// `maw-assurance`'s Stateright model (`DoctorRepairUnlocked` mutation).
///
/// # Errors
/// Returns an error if the epoch lock is busy
/// ([`crate::epoch_lock::EpochLockBusy`]), if classification or ref writes
/// fail, or if the epoch moved after classification (CAS mismatch — nothing
/// is written; re-run the repair).
pub fn auto_advance_if_safe<B>(
    root: &Path,
    branch: &str,
    default_workspace: &str,
    backend: &B,
) -> Result<AutoAdvanceOutcome>
where
    B: maw_core::backend::WorkspaceBackend,
    B::Error: std::fmt::Display,
{
    auto_advance_if_safe_with(
        root,
        branch,
        default_workspace,
        backend,
        crate::epoch_lock::WaitPolicy::resolve(root),
    )
}

/// [`auto_advance_if_safe`] with an explicit epoch-lock [`WaitPolicy`]
/// (tests use a no-wait policy to observe contention deterministically).
///
/// [`WaitPolicy`]: crate::epoch_lock::WaitPolicy
///
/// # Errors
/// See [`auto_advance_if_safe`].
pub fn auto_advance_if_safe_with<B>(
    root: &Path,
    branch: &str,
    default_workspace: &str,
    backend: &B,
    policy: crate::epoch_lock::WaitPolicy,
) -> Result<AutoAdvanceOutcome>
where
    B: maw_core::backend::WorkspaceBackend,
    B::Error: std::fmt::Display,
{
    // bn-32g8: serialize against every other epoch mutator (ws merge's atomic
    // epoch+branch commit, epoch sync, ws advance, …). Classification MUST run
    // under the lock too, so the FF-absorb safety predicate judges the state we
    // are about to write over.
    let _epoch_lock = crate::epoch_lock::EpochLock::acquire_with(root, "doctor --repair", policy)?;

    // bn-302v: refuse while an unfinished `ws merge` journal exists. Under the
    // epoch lock no live merge can be running, so this is a crashed merge
    // awaiting recovery; its recovery (`maw ws merge --recover`, bn-1fcox) needs
    // the refs where the merge left them. Same rule as bn-3w2b's
    // `maw merge promote` refusal (Stateright
    // `fast_doctor_repair_vs_crashed_merge`).
    if let Some(phase) = unfinished_merge_phase(root)? {
        return Ok(AutoAdvanceOutcome::NoOp {
            reason: AutoAdvanceSkip::MergeInProgress { phase },
        });
    }

    let Some(classified) = classify_drift_with_oids(root, branch, backend)? else {
        return Ok(AutoAdvanceOutcome::NoOp {
            reason: AutoAdvanceSkip::EpochUnset,
        });
    };
    let ClassifiedDrift {
        report,
        epoch_oid: old_epoch,
        branch_oid: new_epoch,
    } = classified;

    if !report.kind.is_auto_advanceable() {
        return Ok(AutoAdvanceOutcome::NoOp {
            reason: match report.kind {
                EpochDriftKind::InSync => AutoAdvanceSkip::InSync,
                EpochDriftKind::FfBlocked => AutoAdvanceSkip::FfBlocked(report),
                EpochDriftKind::Diverged => AutoAdvanceSkip::Diverged(report),
                EpochDriftKind::FfAbsorbable => unreachable!(),
            },
        });
    }

    // Interleaving hook for the bn-32g8 regression test: a writer that
    // bypasses the lock moves the epoch between classification and the write.
    maw::fp!("FP_DOCTOR_ADVANCE_BEFORE_WRITE")?;

    // CAS from the classified epoch to the classified branch tip. Under the
    // epoch lock no maw mutator can move the epoch here; the CAS is the
    // belt-and-braces guard against a lock-bypassing writer. On mismatch
    // nothing is written — the epoch is never regressed.
    match manifold_refs::advance_epoch(root, &old_epoch, &new_epoch) {
        Ok(()) => {}
        Err(maw_core::refs::RefError::CasMismatch { .. }) => {
            return Err(anyhow!(
                "epoch moved concurrently since it was classified (expected {}); \
                 nothing written — re-run `maw doctor --repair`",
                short_oid(old_epoch.as_str())
            ));
        }
        Err(e) => return Err(anyhow!("failed to advance epoch ref: {e}")),
    }

    // Default workspace baseline must follow the epoch (see epoch.rs
    // sync()'s bn-3r8s comment for why). Still under the epoch lock.
    let default_ws_ref = manifold_refs::workspace_epoch_ref(default_workspace);
    if let Err(e) = manifold_refs::write_ref(root, &default_ws_ref, &new_epoch) {
        // Surface as a warning, not a failure: the epoch advanced
        // successfully and the default-ws ref drift is recoverable by
        // `maw init` or the next merge.
        tracing::warn!(
            workspace = %default_workspace,
            error = %e,
            "advanced epoch but failed to update default workspace baseline ref"
        );
    }

    Ok(AutoAdvanceOutcome::Advanced {
        report,
        new_epoch_short: short_oid(new_epoch.as_str()),
    })
}

/// Result of an [`auto_advance_if_safe`] call.
#[derive(Debug)]
pub enum AutoAdvanceOutcome {
    /// Epoch was advanced from `report.epoch_short` → `new_epoch_short`.
    Advanced {
        report: EpochDriftReport,
        new_epoch_short: String,
    },
    /// No advance was performed; `reason` carries the structured why.
    NoOp { reason: AutoAdvanceSkip },
}

/// Why [`auto_advance_if_safe`] declined to advance.
#[derive(Debug)]
pub enum AutoAdvanceSkip {
    /// The epoch ref isn't set (pre-`maw init`).
    EpochUnset,
    /// Already in sync — nothing to do.
    InSync,
    /// Pure FF but at least one workspace would have its diff3 base
    /// silently changed.
    FfBlocked(EpochDriftReport),
    /// Epoch and branch have forked.
    Diverged(EpochDriftReport),
    /// bn-302v: an unfinished (crashed) `ws merge` journal exists; advancing
    /// the epoch under it would strand its recovery.
    MergeInProgress {
        /// The journal's phase.
        phase: maw_core::merge_state::MergePhase,
    },
}

/// bn-302v: the phase of an unfinished `ws merge` journal
/// (`merge-state.json` in a non-terminal phase), if one exists.
///
/// Callers that are about to move the epoch use this to refuse while a
/// crashed merge still needs the epoch at its `epoch_before` to recover.
///
/// # Errors
/// Returns an error when the journal exists but cannot be read (fails
/// closed: the caller must not move the epoch past a journal it cannot see).
pub fn unfinished_merge_phase(root: &Path) -> Result<Option<maw_core::merge_state::MergePhase>> {
    use maw_core::merge_state::{MergeStateError, MergeStateFile};
    let manifold_dir =
        maw_core::model::layout::LayoutFlavor::detect_with_env(root).manifold_dir(root);
    let path = MergeStateFile::default_path(&manifold_dir);
    match MergeStateFile::read(&path) {
        Ok(state) if !state.phase.is_terminal() => Ok(Some(state.phase)),
        Ok(_) | Err(MergeStateError::NotFound(_)) => Ok(None),
        Err(e) => Err(anyhow!(
            "cannot read merge journal {}: {e}; refusing to move the epoch past it",
            path.display()
        )),
    }
}

fn short_oid(s: &str) -> String {
    if s.len() >= 12 {
        s[..12].to_owned()
    } else {
        s.to_owned()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn drift_kind_slugs_round_trip() {
        // Pin the slugs — they're load-bearing for JSON consumers
        // (`maw status --json` and `maw doctor --format json`).
        assert_eq!(EpochDriftKind::InSync.slug(), "in_sync");
        assert_eq!(EpochDriftKind::FfAbsorbable.slug(), "ff_absorbable");
        assert_eq!(EpochDriftKind::FfBlocked.slug(), "ff_blocked");
        assert_eq!(EpochDriftKind::Diverged.slug(), "diverged");
    }

    #[test]
    fn has_drift_is_true_for_all_non_insync_states() {
        assert!(!EpochDriftKind::InSync.has_drift());
        assert!(EpochDriftKind::FfAbsorbable.has_drift());
        assert!(EpochDriftKind::FfBlocked.has_drift());
        assert!(EpochDriftKind::Diverged.has_drift());
    }

    #[test]
    fn only_ff_absorbable_is_auto_advanceable() {
        assert!(!EpochDriftKind::InSync.is_auto_advanceable());
        assert!(EpochDriftKind::FfAbsorbable.is_auto_advanceable());
        assert!(!EpochDriftKind::FfBlocked.is_auto_advanceable());
        assert!(!EpochDriftKind::Diverged.is_auto_advanceable());
    }

    #[test]
    fn next_command_routes_each_kind() {
        let mk = |kind: EpochDriftKind| EpochDriftReport {
            kind,
            epoch_short: "aaaaaaaaaaaa".into(),
            branch_short: "bbbbbbbbbbbb".into(),
            branch: "main".into(),
            ff_commit_count: 0,
            blocking_workspaces: Vec::new(),
        };
        assert_eq!(mk(EpochDriftKind::InSync).next_command(), None);
        assert_eq!(
            mk(EpochDriftKind::FfAbsorbable).next_command(),
            Some("maw epoch sync")
        );
        assert!(
            mk(EpochDriftKind::FfBlocked)
                .next_command()
                .unwrap()
                .starts_with("maw ws merge")
        );
        assert_eq!(
            mk(EpochDriftKind::Diverged).next_command(),
            Some("maw doctor")
        );
    }

    #[test]
    fn short_oid_truncates_to_twelve() {
        let full = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(short_oid(full), "0123456789ab");
    }

    #[test]
    fn short_oid_handles_short_input_gracefully() {
        assert_eq!(short_oid("deadbeef"), "deadbeef");
    }

    #[test]
    fn report_serializes_skips_empty_blocking_workspaces() {
        // `serde(skip_serializing_if = "Vec::is_empty")` means the JSON
        // shouldn't carry an empty `blocking_workspaces` for the common
        // in-sync / ff-absorbable cases.
        let report = EpochDriftReport {
            kind: EpochDriftKind::InSync,
            epoch_short: "aaaa".into(),
            branch_short: "aaaa".into(),
            branch: "main".into(),
            ff_commit_count: 0,
            blocking_workspaces: Vec::new(),
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(
            !json.contains("blocking_workspaces"),
            "should omit empty blocking_workspaces: {json}"
        );
        assert!(json.contains("\"kind\":\"in_sync\""));
    }

    #[test]
    fn report_serializes_includes_blocking_workspaces_when_populated() {
        let report = EpochDriftReport {
            kind: EpochDriftKind::FfBlocked,
            epoch_short: "aaaa".into(),
            branch_short: "bbbb".into(),
            branch: "main".into(),
            ff_commit_count: 2,
            blocking_workspaces: vec!["alice".into(), "carol".into()],
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"kind\":\"ff_blocked\""));
        assert!(json.contains("\"blocking_workspaces\":[\"alice\",\"carol\"]"));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod integration_tests {
    //! End-to-end drift classification + auto-advance tests against a
    //! real on-disk repo. These pin the actual behavior change targeted by
    //! bn-1ieb (SG4 `epoch_sync_required` reduction):
    //!
    //! 1. `classify_drift` returns the four expected kinds for the four
    //!    real states (in-sync, ff-absorbable, ff-blocked, diverged).
    //! 2. `auto_advance_if_safe` advances epoch + default-workspace
    //!    baseline iff `ff_absorbable`; refuses otherwise.
    //! 3. The advanced state is observable via a follow-up classify
    //!    call (now `in_sync`).

    use super::*;
    use maw_core::backend::git::GitWorktreeBackend;
    use std::fs;
    use std::path::Path;

    /// Set up a bare-repo-shaped scratch dir with an initial commit on
    /// `main`, the manifold metadata directory, the epoch ref pointing at
    /// the initial commit, and the `workspace_state` ref for "default".
    fn setup() -> (tempfile::TempDir, std::path::PathBuf, String) {
        let (dir, root, oid) = maw_git::test_support::init_test_repo_with_commit();
        fs::create_dir_all(root.join(".manifold/epochs")).expect("mkdir manifold");
        manifold_refs::write_epoch_current(
            &root,
            &maw_core::model::types::GitOid::new(&oid).expect("oid parse"),
        )
        .expect("write epoch_current");
        manifold_refs::write_ref(
            &root,
            &manifold_refs::workspace_state_ref("default"),
            &maw_core::model::types::GitOid::new(&oid).expect("oid parse"),
        )
        .expect("write default workspace_state");
        manifold_refs::write_ref(
            &root,
            &manifold_refs::workspace_epoch_ref("default"),
            &maw_core::model::types::GitOid::new(&oid).expect("oid parse"),
        )
        .expect("write default workspace_epoch");
        (dir, root, oid)
    }

    fn commit(root: &Path, file: &str, content: &str) -> String {
        fs::write(root.join(file), content).expect("write file");
        maw_git::test_support::commit_all(root, &format!("commit {file}"))
    }

    /// Advance the branch HEAD by `n` commits without touching the epoch
    /// ref. Returns the new branch tip OID.
    fn advance_branch(root: &Path, n: usize, prefix: &str) -> String {
        let mut tip = String::new();
        for i in 0..n {
            tip = commit(root, &format!("{prefix}-{i}.txt"), &format!("{prefix}-{i}"));
        }
        tip
    }

    #[test]
    fn in_sync_when_epoch_equals_branch() {
        let (dir, root, oid) = setup();
        let _ = dir;
        let backend = GitWorktreeBackend::new(root.clone());

        let report = classify_drift(&root, "main", &backend)
            .expect("classify")
            .expect("some report (epoch ref is set)");
        assert_eq!(report.kind, EpochDriftKind::InSync);
        assert_eq!(report.epoch_short, &oid[..12]);
        assert_eq!(report.branch_short, &oid[..12]);
        assert_eq!(report.ff_commit_count, 0);
        assert!(report.blocking_workspaces.is_empty());
    }

    #[test]
    fn ff_absorbable_when_branch_ahead_and_no_workspaces_touch_ff_paths() {
        let (dir, root, _epoch0) = setup();
        let _ = dir;
        let _new_tip = advance_branch(&root, 2, "ff");

        let backend = GitWorktreeBackend::new(root.clone());
        let report = classify_drift(&root, "main", &backend)
            .expect("classify")
            .expect("some report");
        assert_eq!(report.kind, EpochDriftKind::FfAbsorbable);
        assert_eq!(report.ff_commit_count, 2);
        assert!(report.blocking_workspaces.is_empty());
    }

    #[test]
    fn diverged_when_branch_was_reset_behind_epoch() {
        let (dir, root, epoch0) = setup();
        let _ = dir;
        // Advance branch, then advance epoch past it, then reset branch
        // BACK so branch is strictly an ancestor of epoch (epoch ahead).
        // This is the "merge dropped" case.
        let mid = commit(&root, "mid.txt", "mid");
        let later = commit(&root, "later.txt", "later");
        manifold_refs::write_epoch_current(
            &root,
            &maw_core::model::types::GitOid::new(&later).expect("oid"),
        )
        .expect("write epoch");
        // Reset branch back to mid (branch is now an ancestor of epoch).
        let _ = maw_git::test_support::git_capture(&root, &["update-ref", "refs/heads/main", &mid]);

        let backend = GitWorktreeBackend::new(root.clone());
        let report = classify_drift(&root, "main", &backend)
            .expect("classify")
            .expect("some report");
        assert_eq!(
            report.kind,
            EpochDriftKind::Diverged,
            "epoch ahead of branch must classify as Diverged, got {:?}",
            report.kind
        );
        // ensure we didn't accidentally trip the InSync short-circuit
        assert_ne!(report.epoch_short, &epoch0[..12]);
    }

    #[test]
    fn auto_advance_advances_epoch_when_safe_and_is_observable() {
        let (dir, root, epoch0) = setup();
        let _ = dir;
        let new_tip = advance_branch(&root, 3, "advance");

        let backend = GitWorktreeBackend::new(root.clone());
        let outcome = auto_advance_if_safe(&root, "main", "default", &backend)
            .expect("auto-advance call should succeed");
        match outcome {
            AutoAdvanceOutcome::Advanced {
                report,
                new_epoch_short,
            } => {
                assert_eq!(report.kind, EpochDriftKind::FfAbsorbable);
                assert_eq!(report.ff_commit_count, 3);
                assert_eq!(new_epoch_short, &new_tip[..12]);
            }
            other @ AutoAdvanceOutcome::NoOp { .. } => {
                panic!("expected Advanced, got {other:?}")
            }
        }

        // Post-advance: epoch ref must equal the new branch tip.
        let after = manifold_refs::read_epoch_current(&root)
            .expect("read")
            .expect("set");
        assert_eq!(after.as_str(), new_tip, "epoch ref should equal branch tip");
        // Default workspace baseline must follow (avoids the bn-3r8s
        // double-application class).
        let default_ws_ref =
            manifold_refs::read_ref(&root, &manifold_refs::workspace_epoch_ref("default"))
                .expect("read")
                .expect("set");
        assert_eq!(
            default_ws_ref.as_str(),
            new_tip,
            "default workspace baseline must follow epoch (bn-3r8s)"
        );
        // A subsequent classify must now return InSync.
        let post_report = classify_drift(&root, "main", &backend)
            .expect("classify post-advance")
            .expect("report");
        assert_eq!(post_report.kind, EpochDriftKind::InSync);
        // Sanity: we genuinely moved.
        assert_ne!(after.as_str(), epoch0);
    }

    /// bn-302v: a crashed `ws merge` left its COMMIT journal behind. The
    /// doctor must not advance the epoch under it (its `--abort` recovery
    /// needs the epoch at `epoch_before`), and must advance once it is gone.
    #[test]
    fn auto_advance_refuses_while_unfinished_merge_journal_exists() {
        let (dir, root, epoch0) = setup();
        let _ = dir;
        let new_tip = advance_branch(&root, 1, "trunk");
        let manifold_dir =
            maw_core::model::layout::LayoutFlavor::detect_with_env(&root).manifold_dir(&root);
        fs::create_dir_all(&manifold_dir).expect("mkdir manifold");
        let path = maw_core::merge_state::MergeStateFile::default_path(&manifold_dir);
        let epoch_id = maw_core::model::types::EpochId::new(&epoch0).expect("epoch id");
        let mut journal = maw_core::merge_state::MergeStateFile::new(
            vec![maw_core::model::types::WorkspaceId::new("alice").expect("ws id")],
            epoch_id.clone(),
            1,
        );
        for phase in [
            maw_core::merge_state::MergePhase::Build,
            maw_core::merge_state::MergePhase::Validate,
        ] {
            journal.advance(phase, 2).expect("advance");
        }
        journal
            .advance_to_commit(epoch_id, 3)
            .expect("commit phase");
        journal.write_atomic(&path).expect("write journal");

        let backend = GitWorktreeBackend::new(root.clone());
        let outcome = auto_advance_if_safe(&root, "main", "default", &backend).expect("call");
        assert!(
            matches!(
                outcome,
                AutoAdvanceOutcome::NoOp {
                    reason: AutoAdvanceSkip::MergeInProgress { .. }
                }
            ),
            "doctor --repair must refuse under an unfinished merge journal, got {outcome:?}"
        );
        let after = manifold_refs::read_epoch_current(&root)
            .expect("read")
            .expect("set");
        assert_eq!(after.as_str(), epoch0, "the epoch must not move");

        fs::remove_file(&path).expect("remove journal");
        let outcome = auto_advance_if_safe(&root, "main", "default", &backend).expect("call");
        assert!(
            matches!(outcome, AutoAdvanceOutcome::Advanced { .. }),
            "{outcome:?}"
        );
        let after = manifold_refs::read_epoch_current(&root)
            .expect("read")
            .expect("set");
        assert_eq!(after.as_str(), new_tip);
    }

    #[test]
    fn auto_advance_is_no_op_when_in_sync() {
        let (dir, root, oid) = setup();
        let _ = dir;
        let backend = GitWorktreeBackend::new(root.clone());
        let outcome = auto_advance_if_safe(&root, "main", "default", &backend)
            .expect("auto-advance call should succeed");
        assert!(
            matches!(
                outcome,
                AutoAdvanceOutcome::NoOp {
                    reason: AutoAdvanceSkip::InSync
                }
            ),
            "expected NoOp::InSync, got {outcome:?}"
        );
        // Refs unchanged.
        let after = manifold_refs::read_epoch_current(&root)
            .expect("read")
            .expect("set");
        assert_eq!(after.as_str(), oid);
    }

    #[test]
    fn auto_advance_refuses_when_diverged() {
        let (dir, root, _epoch0) = setup();
        let _ = dir;
        // Set up the same "epoch ahead of branch" state used in the
        // diverged classify test.
        let mid = commit(&root, "mid.txt", "mid");
        let later = commit(&root, "later.txt", "later");
        manifold_refs::write_epoch_current(
            &root,
            &maw_core::model::types::GitOid::new(&later).expect("oid"),
        )
        .expect("write epoch");
        let _ = maw_git::test_support::git_capture(&root, &["update-ref", "refs/heads/main", &mid]);

        let backend = GitWorktreeBackend::new(root.clone());
        let outcome = auto_advance_if_safe(&root, "main", "default", &backend)
            .expect("auto-advance call should succeed");
        assert!(
            matches!(
                outcome,
                AutoAdvanceOutcome::NoOp {
                    reason: AutoAdvanceSkip::Diverged(_)
                }
            ),
            "expected NoOp::Diverged, got {outcome:?}"
        );
        // Epoch ref must NOT have moved.
        let after = manifold_refs::read_epoch_current(&root)
            .expect("read")
            .expect("set");
        assert_eq!(after.as_str(), later);
    }

    /// bn-32g8: the repair must hold the repo epoch lock. With another holder
    /// present (a `ws merge` mid-commit, say) and a no-wait policy, the repair
    /// must report busy and write NOTHING. Before the fix it never took the
    /// lock and wrote the (possibly stale) branch tip regardless.
    #[test]
    fn auto_advance_takes_epoch_lock_and_writes_nothing_when_busy() {
        let (dir, root, epoch0) = setup();
        let _ = dir;
        let _tip = advance_branch(&root, 2, "busy");
        let backend = GitWorktreeBackend::new(root.clone());

        let no_wait = crate::epoch_lock::WaitPolicy {
            wait: false,
            timeout: std::time::Duration::ZERO,
        };
        let holder = crate::epoch_lock::EpochLock::acquire_with(&root, "test: ws merge", no_wait)
            .expect("test takes the epoch lock first");

        let res = auto_advance_if_safe_with(&root, "main", "default", &backend, no_wait);
        let err = res.expect_err("repair must not proceed while the epoch lock is held");
        assert!(
            err.downcast_ref::<crate::epoch_lock::EpochLockBusy>()
                .is_some(),
            "expected EpochLockBusy, got: {err:#}"
        );
        let after = manifold_refs::read_epoch_current(&root)
            .expect("read")
            .expect("set");
        assert_eq!(after.as_str(), epoch0, "epoch must be untouched while busy");

        // Once the holder releases, the repair proceeds normally. Wait a
        // little: a sibling test's fork() can transiently inherit the flock'd
        // fd until its exec (the bn-1d22 fd-inheritance window).
        drop(holder);
        let short_wait = crate::epoch_lock::WaitPolicy {
            wait: true,
            timeout: std::time::Duration::from_secs(10),
        };
        let outcome = auto_advance_if_safe_with(&root, "main", "default", &backend, short_wait)
            .expect("repair after release");
        assert!(matches!(outcome, AutoAdvanceOutcome::Advanced { .. }));
    }

    /// bn-32g8 interleaving test (needs `--features failpoints`; run by
    /// `just sg1-faithful-test`): a writer that bypasses the epoch lock lands a
    /// merge commit (epoch + branch both advance to M) between the repair's
    /// classification and its write. The repair must CAS-fail and leave the
    /// epoch at M — never regress it to the classified branch tip.
    #[cfg(feature = "failpoints")]
    #[test]
    fn auto_advance_never_regresses_epoch_past_concurrent_merge() {
        let (dir, root, _epoch0) = setup();
        let _ = dir;
        let classified_tip = advance_branch(&root, 2, "drift");
        let backend = GitWorktreeBackend::new(root.clone());

        let root_cb = root.clone();
        let merged = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let merged_cb = std::sync::Arc::clone(&merged);
        // The failpoint registry is process-global and sibling tests run in
        // parallel through the same site: only perturb THIS test's thread.
        let me = std::thread::current().id();
        maw_core::failpoints::set_callback("FP_DOCTOR_ADVANCE_BEFORE_WRITE", move || {
            if std::thread::current().id() != me {
                return;
            }
            // The concurrent merge's atomic epoch+branch commit.
            let m = commit(&root_cb, "merge.txt", "merged work");
            let oid = maw_core::model::types::GitOid::new(&m).expect("oid");
            manifold_refs::write_epoch_current(&root_cb, &oid).expect("merge epoch write");
            *merged_cb.lock().expect("lock") = m;
        });
        let no_wait = crate::epoch_lock::WaitPolicy {
            wait: false,
            timeout: std::time::Duration::ZERO,
        };
        let res = auto_advance_if_safe_with(&root, "main", "default", &backend, no_wait);
        maw_core::failpoints::clear("FP_DOCTOR_ADVANCE_BEFORE_WRITE");

        let merged = merged.lock().expect("lock").clone();
        assert!(!merged.is_empty(), "failpoint callback must have fired");
        let err = res.expect_err("repair must detect the concurrent epoch move");
        assert!(
            err.to_string().contains("epoch moved concurrently"),
            "unexpected error: {err:#}"
        );
        let after = manifold_refs::read_epoch_current(&root)
            .expect("read")
            .expect("set");
        assert_ne!(
            after.as_str(),
            classified_tip,
            "epoch regressed to the pre-merge branch tip (bn-32g8)"
        );
        assert_eq!(
            after.as_str(),
            merged,
            "epoch must stay at the merge commit"
        );
    }
}
