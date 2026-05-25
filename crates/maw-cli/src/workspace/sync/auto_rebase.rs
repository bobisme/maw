//! Sibling auto-rebase orchestrator (bn-3vf5, refined in bn-103k).
//!
//! After `maw ws merge` advances the epoch, every other workspace becomes
//! stale. This module enumerates every non-target workspace and replays its
//! commits onto the new epoch via the existing rebase machinery, summarizing
//! the result in the merge output.
//!
//! Concurrency rules
//! -----------------
//! * Try-lock only — never block on a sibling lock. If a sibling is in use,
//!   we skip it ("in use") and let the user re-run `maw ws sync --rebase`.
//! * Re-check dirty state and merge-state membership UNDER the lock. The
//!   pre-lock check is purely an optimization; the post-lock check is
//!   authoritative.
//! * Per-sibling failure does NOT abort the parent merge — we record the
//!   error string and move on to the next sibling.
//! * Refs (`refs/manifold/epoch/ws/<name>` and the workspace's HEAD) are
//!   always advanced when the sibling passes all skip rules.
//! * Worktree mutation: when the sibling is provably clean (dirty re-check
//!   under lock passed), the worktree is ALSO synchronized via a checkout
//!   to the rebased HEAD. This keeps `git status` clean post-merge and
//!   avoids the dirty-workspace guard tripping on the next `maw ws sync`.
//!   The rebase routine performs ONE more dirty re-check immediately before
//!   the destructive checkout to close the small race window that follows
//!   the under-lock skip check.
//! * Worktree-update failure (transient I/O, freshly-dirty file) NEVER
//!   aborts the rebase — refs still advance and we report
//!   `RebasedCleanRefsOnly` (or `RebasedWithConflictsRefsOnly`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use maw_core::backend::WorkspaceBackend;
use maw_core::merge_state::MergeStateFile;
use maw_core::model::types::WorkspaceId;

use super::checks::{
    committed_ahead_of_epoch, is_default_workspace, workspace_has_uncommitted_changes,
};
use super::lock::WorkspaceRebaseLock;
use super::rebase::{RebaseOutcome, RebaseRunOptions, rebase_workspace_run};

/// Reason a sibling was skipped or how its rebase finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SiblingResult {
    /// The sibling was already pointing at the new epoch — nothing to do.
    UpToDate,
    /// The sibling's lock was held by another process, so we did not even
    /// look at its dirty state.
    SkippedInUse,
    /// The sibling has uncommitted changes; rebasing would silently lose them.
    SkippedDirty,
    /// The sibling is named as a source in the in-progress merge state.
    SkippedInProgress,
    /// All workspace commits replayed cleanly AND the worktree was
    /// synchronized to the rebased HEAD (bn-103k). `replayed` is the number
    /// of commits.
    RebasedClean { replayed: usize },
    /// All workspace commits replayed cleanly, but the worktree update
    /// step was skipped or failed (refs still advanced). `reason` carries
    /// a short diagnostic for the user.
    RebasedCleanRefsOnly { replayed: usize, reason: String },
    /// Rebase produced conflict-as-data state and the worktree was
    /// synchronized — `maw ws resolve` will see the markers in the working
    /// tree. `conflicts` is the number of unresolved entries; `replayed`
    /// is the number of commits replayed.
    RebasedWithConflicts { replayed: usize, conflicts: usize },
    /// Rebase produced conflict-as-data state but the worktree update was
    /// skipped or failed — markers exist in the rebased HEAD's tree but
    /// have not been written to disk yet.
    RebasedWithConflictsRefsOnly {
        replayed: usize,
        conflicts: usize,
        reason: String,
    },
    /// Rebase machinery returned an error. The merge was NOT aborted.
    Failed { reason: String },
}

impl SiblingResult {
    /// One-line summary suitable for the merge output.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::UpToDate => "skipped: up to date".to_string(),
            Self::SkippedInUse => "skipped: in use".to_string(),
            Self::SkippedDirty => "skipped: dirty".to_string(),
            Self::SkippedInProgress => "skipped: in progress".to_string(),
            Self::RebasedClean { replayed } => {
                format!("rebased clean ({replayed} commit(s), worktree synced)")
            }
            Self::RebasedCleanRefsOnly { replayed, reason } => {
                format!("rebased clean ({replayed} commit(s), worktree update skipped: {reason})")
            }
            Self::RebasedWithConflicts {
                replayed,
                conflicts,
            } => format!(
                "rebased with {conflicts} conflict(s) ({replayed} commit(s), worktree synced)"
            ),
            Self::RebasedWithConflictsRefsOnly {
                replayed,
                conflicts,
                reason,
            } => format!(
                "rebased with {conflicts} conflict(s) ({replayed} commit(s), worktree update skipped: {reason})"
            ),
            Self::Failed { reason } => format!("failed: {reason}"),
        }
    }
}

/// Per-sibling outcome row reported to the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiblingReport {
    pub name: String,
    pub result: SiblingResult,
}

/// Rebase every sibling workspace onto `new_epoch`.
///
/// Sources (`merge_sources`) and `target_workspace` are excluded — the parent
/// merge already finalized them. The default workspace is also excluded
/// (its worktree is reconciled by `update_default_workspace` in the merge
/// CLEANUP phase).
///
/// Errors from individual siblings are captured per-row; this function
/// returns `Ok(...)` whenever the orchestration itself succeeded. It only
/// returns `Err` if the overall enumeration setup fails.
pub fn auto_rebase_siblings<B: WorkspaceBackend>(
    root: &Path,
    backend: &B,
    target_workspace: &str,
    merge_sources: &[String],
    new_epoch: &str,
) -> Vec<SiblingReport> {
    // Snapshot the in-progress merge sources from the on-disk state file.
    // The skip rule says siblings named as sources are "in progress" — but
    // since the merge is past COMMIT, that set should be the same as the
    // explicit `merge_sources` list. We read the state file too as a belt-
    // and-suspenders defense against a renamed source.
    let mut in_progress: HashSet<String> = merge_sources.iter().cloned().collect();
    {
        let merge_state_path = MergeStateFile::default_path(
            &maw_core::model::layout::LayoutFlavor::detect_with_env(root).manifold_dir(root),
        );
        if let Ok(state) = MergeStateFile::read(&merge_state_path) {
            for ws_id in &state.sources {
                in_progress.insert(ws_id.as_str().to_string());
            }
        }
    }

    let workspaces = match backend.list() {
        Ok(ws) => ws,
        Err(e) => {
            tracing::warn!(error = %e, "auto_rebase_siblings: backend.list() failed");
            return Vec::new();
        }
    };

    let mut reports = Vec::new();
    for ws in &workspaces {
        let name = ws.id.as_str();
        if name == target_workspace || is_default_workspace(name) {
            continue;
        }
        if in_progress.contains(name) {
            // Sources of the just-completed merge — they may still be the
            // active subjects of CLEANUP (destroy). Defer to the user / their
            // own next sync.
            reports.push(SiblingReport {
                name: name.to_string(),
                result: SiblingResult::SkippedInProgress,
            });
            continue;
        }

        let result = rebase_one_sibling(root, backend, name, new_epoch);
        reports.push(SiblingReport {
            name: name.to_string(),
            result,
        });
    }

    reports
}

fn rebase_one_sibling<B: WorkspaceBackend>(
    root: &Path,
    backend: &B,
    name: &str,
    new_epoch: &str,
) -> SiblingResult {
    // Skip rule 4 (cheap): if the workspace's recorded base epoch already
    // equals the new epoch, there's nothing to do. Re-checked under the lock
    // below to be race-safe.
    let ws_id = match WorkspaceId::new(name) {
        Ok(id) => id,
        Err(e) => {
            return SiblingResult::Failed {
                reason: format!("invalid workspace id '{name}': {e}"),
            };
        }
    };

    let pre_status = match backend.status(&ws_id) {
        Ok(s) => s,
        Err(e) => {
            return SiblingResult::Failed {
                reason: format!("backend.status: {e}"),
            };
        }
    };
    if pre_status.base_epoch.as_str() == new_epoch {
        return SiblingResult::UpToDate;
    }

    // Skip rule 1: try-lock only. We never block on a sibling.
    let lock = match WorkspaceRebaseLock::try_acquire(root, name) {
        Ok(Some(guard)) => guard,
        Ok(None) => return SiblingResult::SkippedInUse,
        Err(e) => {
            return SiblingResult::Failed {
                reason: format!("lock acquisition failed: {e}"),
            };
        }
    };

    let ws_path: PathBuf =
        maw_core::model::layout::LayoutFlavor::detect_with_env(root).workspace_path(root, name);

    // Re-check skip rules 2 and 3 UNDER the lock — race-safe.
    match workspace_has_uncommitted_changes(&ws_path) {
        Ok(true) => return SiblingResult::SkippedDirty,
        Ok(false) => {}
        Err(e) => {
            return SiblingResult::Failed {
                reason: format!("dirty check failed: {e}"),
            };
        }
    }

    // Re-check merge-state under the lock. A new merge could (in principle)
    // have started since we read the snapshot above; the per-workspace lock
    // does not exclude the merge-state writer, so re-read here.
    {
        let merge_state_path = MergeStateFile::default_path(
            &maw_core::model::layout::LayoutFlavor::detect_with_env(root).manifold_dir(root),
        );
        if let Ok(state) = MergeStateFile::read(&merge_state_path)
            && state.sources.iter().any(|src| src.as_str() == name)
        {
            return SiblingResult::SkippedInProgress;
        }
    }

    // Re-read status under the lock (skip rule 4, race-safe).
    let status = match backend.status(&ws_id) {
        Ok(s) => s,
        Err(e) => {
            return SiblingResult::Failed {
                reason: format!("backend.status (post-lock): {e}"),
            };
        }
    };
    if status.base_epoch.as_str() == new_epoch {
        return SiblingResult::UpToDate;
    }

    // FP_AUTO_REBASE_BEFORE_REPLAY: crash here leaves the sibling stale.
    // Recovery is "do nothing" — the user can run `maw ws sync --rebase`
    // manually. The parent merge already committed, so this is safe.
    if let Err(e) = maw::fp!("FP_AUTO_REBASE_BEFORE_REPLAY") {
        return SiblingResult::Failed {
            reason: format!("failpoint FP_AUTO_REBASE_BEFORE_REPLAY: {e}"),
        };
    }

    // Determine ahead-count to feed the rebase header. None means the call
    // can't determine work — proceed with 0 and let rebase_workspace_run
    // decide.
    let ahead_count = committed_ahead_of_epoch(&ws_path, &status.base_epoch).unwrap_or(0);

    // bn-103k: pass `mutate_worktree: true` so the sibling's worktree files
    // also advance to the rebased HEAD. The under-lock dirty re-check above
    // proved the worktree is clean, and `continue_past_worktree_failure`
    // tells the rebase routine to do ONE more dirty re-check immediately
    // before checkout — closing the small race window with a hypothetical
    // editor save — and to log-and-continue rather than abort if the
    // checkout itself fails. Refs always advance on Ok(...).
    let outcome_res = rebase_workspace_run(
        root,
        name,
        status.base_epoch.as_str(),
        new_epoch,
        &ws_path,
        ahead_count,
        RebaseRunOptions {
            print: false,
            mutate_worktree: true,
            acquire_lock: false,
            continue_past_worktree_failure: true,
        },
    );

    drop(lock);

    classify_outcome(name, outcome_res)
}

/// Map a [`RebaseOutcome`] (or rebase error) into the [`SiblingResult`]
/// variant the orchestrator surfaces in the merge summary.
fn classify_outcome(name: &str, outcome_res: anyhow::Result<RebaseOutcome>) -> SiblingResult {
    match outcome_res {
        Ok(RebaseOutcome {
            replayed,
            conflicts: 0,
            worktree_updated: true,
            ..
        }) => SiblingResult::RebasedClean { replayed },
        Ok(RebaseOutcome {
            replayed,
            conflicts: 0,
            worktree_updated: false,
            worktree_skip_reason,
            ..
        }) => SiblingResult::RebasedCleanRefsOnly {
            replayed,
            reason: if worktree_skip_reason.is_empty() {
                "unknown".to_string()
            } else {
                worktree_skip_reason
            },
        },
        Ok(RebaseOutcome {
            replayed,
            conflicts,
            worktree_updated: true,
            ..
        }) => SiblingResult::RebasedWithConflicts {
            replayed,
            conflicts,
        },
        Ok(RebaseOutcome {
            replayed,
            conflicts,
            worktree_updated: false,
            worktree_skip_reason,
            ..
        }) => SiblingResult::RebasedWithConflictsRefsOnly {
            replayed,
            conflicts,
            reason: if worktree_skip_reason.is_empty() {
                "unknown".to_string()
            } else {
                worktree_skip_reason
            },
        },
        Err(e) => {
            tracing::warn!(workspace = %name, error = %e, "sibling auto-rebase failed");
            SiblingResult::Failed {
                reason: e.to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_clean() {
        assert_eq!(
            SiblingResult::RebasedClean { replayed: 3 }.describe(),
            "rebased clean (3 commit(s), worktree synced)"
        );
    }

    #[test]
    fn describe_clean_refs_only() {
        let r = SiblingResult::RebasedCleanRefsOnly {
            replayed: 3,
            reason: "dirty re-check before checkout".to_string(),
        };
        assert_eq!(
            r.describe(),
            "rebased clean (3 commit(s), worktree update skipped: dirty re-check before checkout)"
        );
    }

    #[test]
    fn describe_conflicts() {
        let r = SiblingResult::RebasedWithConflicts {
            replayed: 5,
            conflicts: 2,
        };
        assert_eq!(
            r.describe(),
            "rebased with 2 conflict(s) (5 commit(s), worktree synced)"
        );
    }

    #[test]
    fn describe_conflicts_refs_only() {
        let r = SiblingResult::RebasedWithConflictsRefsOnly {
            replayed: 5,
            conflicts: 2,
            reason: "checkout_tree: io".to_string(),
        };
        assert_eq!(
            r.describe(),
            "rebased with 2 conflict(s) (5 commit(s), worktree update skipped: checkout_tree: io)"
        );
    }

    #[test]
    fn describe_skips() {
        assert_eq!(SiblingResult::UpToDate.describe(), "skipped: up to date");
        assert_eq!(SiblingResult::SkippedInUse.describe(), "skipped: in use");
        assert_eq!(SiblingResult::SkippedDirty.describe(), "skipped: dirty");
        assert_eq!(
            SiblingResult::SkippedInProgress.describe(),
            "skipped: in progress"
        );
    }

    #[test]
    fn describe_failed() {
        let r = SiblingResult::Failed {
            reason: "boom".to_string(),
        };
        assert_eq!(r.describe(), "failed: boom");
    }
}
