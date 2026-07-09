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

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use maw_core::backend::WorkspaceBackend;
use maw_core::merge_state::MergeStateFile;
use maw_core::model::types::WorkspaceId;

use super::checks::{
    committed_ahead_of_epoch, is_default_workspace, workspace_has_uncommitted_changes,
};
use super::lock::WorkspaceRebaseLock;
use super::rebase::{RebaseOutcome, RebaseRunOptions, rebase_workspace_run};

/// bn-2cvx: how many overlapping-path samples ride in the merge-output line
/// and the auto-rebase notice JSON. Capped so neither blows up for a large
/// rebase — the count is always exact, only the sample list is truncated.
const OVERLAP_SAMPLE_CAP: usize = 5;

/// Semantic-risk hint (bn-2cvx): the sibling was replayed over epoch-range
/// commits that touch at least one path the sibling itself also touches.
/// Textually clean (no merge conflict) does not imply semantically safe —
/// this is the machine-readable flag for "re-run tests before merging".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlapHint {
    /// Total number of overlapping paths (exact, not capped).
    pub count: usize,
    /// Sorted sample of overlapping paths, capped at
    /// [`OVERLAP_SAMPLE_CAP`].
    pub sample_paths: Vec<String>,
}

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
    /// of commits. `overlap` is the bn-2cvx semantic-risk hint: `Some` when
    /// the epoch range this sibling was rebased over touches at least one
    /// path the sibling itself also touches — textually clean is not the
    /// same as semantically safe.
    RebasedClean {
        replayed: usize,
        overlap: Option<OverlapHint>,
    },
    /// All workspace commits replayed cleanly, but the worktree update
    /// step was skipped or failed (refs still advanced). `reason` carries
    /// a short diagnostic for the user.
    RebasedCleanRefsOnly {
        replayed: usize,
        reason: String,
        overlap: Option<OverlapHint>,
    },
    /// Rebase produced conflict-as-data state and the worktree was
    /// synchronized — `maw ws resolve` will see the markers in the working
    /// tree. `conflicts` is the number of unresolved entries; `replayed`
    /// is the number of commits replayed.
    RebasedWithConflicts {
        replayed: usize,
        conflicts: usize,
        overlap: Option<OverlapHint>,
    },
    /// Rebase produced conflict-as-data state but the worktree update was
    /// skipped or failed — markers exist in the rebased HEAD's tree but
    /// have not been written to disk yet.
    RebasedWithConflictsRefsOnly {
        replayed: usize,
        conflicts: usize,
        reason: String,
        overlap: Option<OverlapHint>,
    },
    /// Rebase machinery returned an error. The merge was NOT aborted.
    Failed { reason: String },
}

/// bn-2cvx: render the trailing overlap-risk hint appended to merge-output
/// lines and used verbatim (minus the leading `; `) in the auto-rebase
/// notice. Empty string when there is no overlap to report.
#[must_use]
pub fn overlap_hint_suffix(overlap: Option<&OverlapHint>) -> String {
    match overlap {
        Some(hint) if hint.count > 0 => format!(
            "; replayed over commits touching {} file(s) this workspace also touches \u{2014} re-run its tests before merging",
            hint.count
        ),
        _ => String::new(),
    }
}

impl SiblingResult {
    /// One-line summary suitable for the merge output. `name` is the
    /// sibling workspace's name — used only to build the `maw ws resolve
    /// <name> --list` hint on conflicted outcomes (bn-mq6j).
    #[must_use]
    pub fn describe(&self, name: &str) -> String {
        match self {
            Self::UpToDate => "skipped: up to date".to_string(),
            Self::SkippedInUse => "skipped: in use".to_string(),
            Self::SkippedDirty => "skipped: dirty".to_string(),
            Self::SkippedInProgress => "skipped: in progress".to_string(),
            Self::RebasedClean { replayed, overlap } => {
                format!(
                    "rebased clean ({replayed} commit(s), worktree synced){}",
                    overlap_hint_suffix(overlap.as_ref())
                )
            }
            Self::RebasedCleanRefsOnly {
                replayed,
                reason,
                overlap,
            } => {
                format!(
                    "rebased clean ({replayed} commit(s), worktree update skipped: {reason}){}",
                    overlap_hint_suffix(overlap.as_ref())
                )
            }
            // bn-mq6j: distinct "CONFLICT:" tag so this doesn't blend into
            // the "rebased clean" lines above, plus the exact resolve
            // command so the reader doesn't have to go look it up.
            Self::RebasedWithConflicts {
                replayed,
                conflicts,
                overlap,
            } => format!(
                "CONFLICT: rebased with {conflicts} conflict(s) ({replayed} commit(s), worktree synced) \u{2014} resolve: maw ws resolve {name} --list{}",
                overlap_hint_suffix(overlap.as_ref())
            ),
            Self::RebasedWithConflictsRefsOnly {
                replayed,
                conflicts,
                reason,
                overlap,
            } => format!(
                "CONFLICT: rebased with {conflicts} conflict(s) ({replayed} commit(s), worktree update skipped: {reason}) \u{2014} resolve: maw ws resolve {name} --list{}",
                overlap_hint_suffix(overlap.as_ref())
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

        let result = rebase_one_sibling(root, backend, name, new_epoch, merge_sources);
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
    merge_sources: &[String],
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

    // bn-2cvx: compute the semantic-risk overlap hint BEFORE the rebase
    // mutates this sibling's worktree/HEAD — the sibling's own touched-path
    // set is meaningful only while it's still sitting on its old epoch.
    let overlap = compute_overlap_hint(root, backend, &ws_id, &status, new_epoch);

    // Build the trigger string for the oplog entry. It names the merge
    // sources that caused the epoch to advance, making `maw ws history <ws>`
    // useful for tracing which merge triggered a sibling rebase.
    let trigger_str = format!("auto-rebase:merge({})", merge_sources.join(","));

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
        &trigger_str,
    );

    drop(lock);

    let result = classify_outcome(outcome_res, overlap.clone());
    record_rebase_notice(
        root,
        name,
        status.base_epoch.as_str(),
        new_epoch,
        merge_sources,
        &result,
        overlap.as_ref(),
    );
    result
}

/// bn-2cvx: intersect (paths touched by the epoch range `old_epoch..new_epoch`
/// that this sibling is about to be rebased over) with (paths the sibling
/// itself has touched since its own `old_epoch`). A non-empty intersection
/// means the rebase can be textually clean yet still semantically risky —
/// the sibling's own changes sit next to code the merge just reworked.
///
/// Best-effort: any failure (bad OIDs, git errors) yields `None` rather than
/// failing the rebase — this is an advisory hint, not a safety gate.
fn compute_overlap_hint<B: WorkspaceBackend>(
    root: &Path,
    backend: &B,
    ws_id: &WorkspaceId,
    status: &maw_core::backend::WorkspaceStatus,
    new_epoch: &str,
) -> Option<OverlapHint>
where
    B::Error: std::fmt::Display,
{
    let old_epoch = status.base_epoch.as_str();
    if old_epoch == new_epoch {
        return None;
    }
    let old_oid: maw_git::GitOid = old_epoch.parse().ok()?;
    let new_oid: maw_git::GitOid = new_epoch.parse().ok()?;

    let repo = super::super::ff_absorb::open_repo(root).ok()?;
    let ff_paths = super::super::ff_absorb::compute_ff_changed_paths(&repo, &old_oid, &new_oid)
        .ok()
        .filter(|p| !p.is_empty())?;

    let touched = super::super::touched::collect_touched_workspace(backend, ws_id).ok()?;
    let ws_paths: BTreeSet<PathBuf> = touched.touched_paths.into_iter().collect();

    let mut overlap: Vec<String> = ff_paths
        .intersection(&ws_paths)
        .map(|p| p.display().to_string())
        .collect();
    if overlap.is_empty() {
        return None;
    }
    overlap.sort();
    let count = overlap.len();
    overlap.truncate(OVERLAP_SAMPLE_CAP);
    Some(OverlapHint {
        count,
        sample_paths: overlap,
    })
}

/// bn-1abp: an agent may be actively working in a rebased sibling and has
/// no idea its refs/worktree just moved. Record a one-time notice that the
/// next `maw exec <name> -- ...` prints and consumes. Advisory only —
/// write failures are logged inside `write_notice` and never abort.
fn record_rebase_notice(
    root: &Path,
    name: &str,
    old_epoch: &str,
    new_epoch: &str,
    merge_sources: &[String],
    result: &SiblingResult,
    overlap: Option<&OverlapHint>,
) {
    let Some((replayed, conflicts, worktree_updated)) = (match result {
        SiblingResult::RebasedClean { replayed, .. } => Some((*replayed, 0, true)),
        SiblingResult::RebasedCleanRefsOnly { replayed, .. } => Some((*replayed, 0, false)),
        SiblingResult::RebasedWithConflicts {
            replayed,
            conflicts,
            ..
        } => Some((*replayed, *conflicts, true)),
        SiblingResult::RebasedWithConflictsRefsOnly {
            replayed,
            conflicts,
            ..
        } => Some((*replayed, *conflicts, false)),
        SiblingResult::UpToDate
        | SiblingResult::SkippedInUse
        | SiblingResult::SkippedDirty
        | SiblingResult::SkippedInProgress
        | SiblingResult::Failed { .. } => None,
    }) else {
        return;
    };
    super::notice::write_notice(
        root,
        name,
        &super::notice::AutoRebaseNotice {
            old_epoch: old_epoch.to_string(),
            new_epoch: new_epoch.to_string(),
            merge_sources: merge_sources.to_vec(),
            replayed,
            conflicts,
            worktree_updated,
            overlap: overlap.cloned(),
        },
    );
}

/// Map a [`RebaseOutcome`] (or rebase error) into the [`SiblingResult`]
/// variant the orchestrator surfaces in the merge summary. `overlap` is
/// attached to every "replayed" variant (bn-2cvx) — it applies equally to
/// the clean and conflicted cases, since it's about paths the merge
/// touched, not about how the replay itself went.
fn classify_outcome(
    outcome_res: anyhow::Result<RebaseOutcome>,
    overlap: Option<OverlapHint>,
) -> SiblingResult {
    match outcome_res {
        Ok(RebaseOutcome {
            replayed,
            conflicts: 0,
            worktree_updated: true,
            ..
        }) => SiblingResult::RebasedClean { replayed, overlap },
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
            overlap,
        },
        Ok(RebaseOutcome {
            replayed,
            conflicts,
            worktree_updated: true,
            ..
        }) => SiblingResult::RebasedWithConflicts {
            replayed,
            conflicts,
            overlap,
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
            overlap,
        },
        Err(e) => {
            // Logged with full workspace context by the caller (merge.rs),
            // which already emits a `tracing::warn!` for `Failed` reports.
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
            SiblingResult::RebasedClean {
                replayed: 3,
                overlap: None
            }
            .describe("alice"),
            "rebased clean (3 commit(s), worktree synced)"
        );
    }

    #[test]
    fn describe_clean_refs_only() {
        let r = SiblingResult::RebasedCleanRefsOnly {
            replayed: 3,
            reason: "dirty re-check before checkout".to_string(),
            overlap: None,
        };
        assert_eq!(
            r.describe("alice"),
            "rebased clean (3 commit(s), worktree update skipped: dirty re-check before checkout)"
        );
    }

    #[test]
    fn describe_conflicts() {
        let r = SiblingResult::RebasedWithConflicts {
            replayed: 5,
            conflicts: 2,
            overlap: None,
        };
        assert_eq!(
            r.describe("bob"),
            "CONFLICT: rebased with 2 conflict(s) (5 commit(s), worktree synced) \u{2014} resolve: maw ws resolve bob --list"
        );
    }

    #[test]
    fn describe_conflicts_refs_only() {
        let r = SiblingResult::RebasedWithConflictsRefsOnly {
            replayed: 5,
            conflicts: 2,
            reason: "checkout_tree: io".to_string(),
            overlap: None,
        };
        assert_eq!(
            r.describe("bob"),
            "CONFLICT: rebased with 2 conflict(s) (5 commit(s), worktree update skipped: checkout_tree: io) \u{2014} resolve: maw ws resolve bob --list"
        );
    }

    #[test]
    fn describe_skips() {
        assert_eq!(
            SiblingResult::UpToDate.describe("alice"),
            "skipped: up to date"
        );
        assert_eq!(
            SiblingResult::SkippedInUse.describe("alice"),
            "skipped: in use"
        );
        assert_eq!(
            SiblingResult::SkippedDirty.describe("alice"),
            "skipped: dirty"
        );
        assert_eq!(
            SiblingResult::SkippedInProgress.describe("alice"),
            "skipped: in progress"
        );
    }

    #[test]
    fn describe_failed() {
        let r = SiblingResult::Failed {
            reason: "boom".to_string(),
        };
        assert_eq!(r.describe("alice"), "failed: boom");
    }

    // -------------------------------------------------------------------
    // bn-2cvx: overlap hint rendering
    // -------------------------------------------------------------------

    fn hint(count: usize, paths: &[&str]) -> OverlapHint {
        OverlapHint {
            count,
            sample_paths: paths.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn describe_clean_with_overlap_appends_hint() {
        let r = SiblingResult::RebasedClean {
            replayed: 2,
            overlap: Some(hint(1, &["src/lib.rs"])),
        };
        let d = r.describe("alice");
        assert!(
            d.contains("replayed over commits touching 1 file(s) this workspace also touches"),
            "missing overlap hint: {d}"
        );
        assert!(d.contains("re-run its tests before merging"));
    }

    #[test]
    fn describe_conflicts_with_overlap_appends_hint() {
        let r = SiblingResult::RebasedWithConflicts {
            replayed: 2,
            conflicts: 1,
            overlap: Some(hint(3, &["a.rs", "b.rs", "c.rs"])),
        };
        let d = r.describe("bob");
        assert!(d.starts_with("CONFLICT:"));
        assert!(d.contains("resolve: maw ws resolve bob --list"));
        assert!(d.contains("replayed over commits touching 3 file(s)"));
    }

    #[test]
    fn overlap_hint_suffix_empty_when_none() {
        assert_eq!(overlap_hint_suffix(None), "");
    }

    #[test]
    fn overlap_hint_suffix_empty_when_zero_count() {
        assert_eq!(overlap_hint_suffix(Some(&hint(0, &[]))), "");
    }
}
