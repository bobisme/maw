//! Rebase a workspace's committed commits onto a newer epoch, routed
//! through the structured-merge engine (`maw-core::merge`).
//!
//! # Pipeline
//!
//! 1. Seed a [`ConflictTree`] with the new epoch's tree contents (clean map)
//!    but tag the tree's `base_epoch` as the **old** epoch, so patches
//!    extracted from `old_epoch..HEAD` can be applied.
//! 2. Walk workspace commits `old_epoch..HEAD` (oldest first).
//! 3. For each commit, compute the parent→commit delta via
//!    [`diff_patchset`] and fold it into the `ConflictTree` via
//!    [`apply_unilateral_patchset`]. Merge commits (multi-parent) are
//!    handled by applying the first-parent delta AND injecting an
//!    explicit `Conflict::Content` entry for every path touched by the
//!    non-first parents (V1 simplification documented inline).
//! 4. After each fold, [`materialize`] the tree to obtain a final
//!    `(mode, oid_or_content)` per path. Rendered marker blobs are
//!    written via `GitRepo::write_blob`; the resulting tree is built by
//!    [`GitRepo::edit_tree`] against the new-epoch tree.
//! 5. A new commit is created per step (`create_commit`) preserving the
//!    original commit message — this keeps commit-count parity so
//!    `find_conflicted_files` (which diffs against the workspace base)
//!    still sees the `+<<<<<<<` lines added by this rebase, tripping
//!    the merge-time marker gate when conflicts exist. For merge commits
//!    (≥2 parents in the original), the replayed commit also has ≥2
//!    parents: first parent = the rebased chain head, subsequent parents
//!    = the ORIGINAL pre-rebase OIDs of the side(s). This preserves the
//!    DAG shape so downstream tooling sees a real merge commit; a future
//!    follow-up can rebase the side branches and substitute the rebased
//!    OIDs (bn-7mbe).
//! 6. HEAD is moved via `GitRepo::set_head` and the worktree is
//!    synchronized via `GitRepo::checkout_tree`.
//! 7. Both sidecars (`rebase-conflicts.json`, `conflict-tree.json`) are
//!    written by `materialize::write_legacy_sidecar` and
//!    `materialize::write_structured_sidecar`.
//!
//! This module does **no** shelling out to `git` — all git operations
//! flow through the [`GitRepo`] trait.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use maw_core::config::ManifoldConfig;
use maw_core::merge::apply::apply_unilateral_patchset;
use maw_core::merge::diff_extract::diff_patchset;
use maw_core::merge::materialize::{
    looks_text, materialize, write_legacy_sidecar, write_structured_sidecar,
};
use maw_core::merge::types::{ConflictTree, EntryMode, MaterializedEntry};
use maw_core::model::conflict::{Conflict, ConflictSide};
use maw_core::model::ordering::OrderingKey;
use maw_core::model::patch::FileId;
use maw_core::model::types::{EpochId, GitOid, WorkspaceId};
use maw_core::oplog::read::read_head;
use maw_core::oplog::types::{OpPayload, Operation};
use maw_core::refs as manifold_refs;
use maw_git::merge::{MergeResult, merge_text};
use maw_git::{self as git, GitRepo, TreeEdit};

use super::checks::{
    sync_worktree_to_epoch, sync_worktree_to_epoch_quiet, workspace_has_uncommitted_changes,
};
use super::lock::WorkspaceRebaseLock;

// ---------------------------------------------------------------------------
// Legacy sidecar types (kept public for `maw ws resolve` / callers in
// `super::*`)
// ---------------------------------------------------------------------------

/// A single rebase conflict recorded as data.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RebaseConflict {
    /// File path relative to workspace root.
    pub path: String,
    /// The original commit SHA being replayed when conflict occurred.
    pub original_commit: String,
    /// Base content (merge base), if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// "Ours" content (new epoch version), if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ours: Option<String>,
    /// "Theirs" content (workspace commit version), if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theirs: Option<String>,
}

/// Rebase conflict metadata stored in `.manifold/artifacts/ws/<name>/rebase-conflicts.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RebaseConflicts {
    /// All conflicts from the rebase.
    pub conflicts: Vec<RebaseConflict>,
    /// The epoch OID before the rebase.
    pub rebase_from: String,
    /// The epoch OID after the rebase (target).
    pub rebase_to: String,
}

/// Path to the rebase conflicts JSON file for a workspace.
fn rebase_conflicts_path(root: &Path, ws_name: &str) -> std::path::PathBuf {
    let flavor = maw_core::model::layout::LayoutFlavor::detect_with_env(root);
    flavor
        .manifold_dir(root)
        .join("artifacts")
        .join("ws")
        .join(ws_name)
        .join("rebase-conflicts.json")
}

/// Read rebase conflicts for a workspace, if any.
#[must_use]
pub fn read_rebase_conflicts(root: &Path, ws_name: &str) -> Option<RebaseConflicts> {
    let path = rebase_conflicts_path(root, ws_name);
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Delete rebase conflicts file for a workspace (called on resolution).
///
/// # Errors
///
/// Returns an error if the conflict sidecar cannot be removed.
pub fn delete_rebase_conflicts(root: &Path, ws_name: &str) -> Result<()> {
    let path = rebase_conflicts_path(root, ws_name);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rebase implementation — routed through maw-core::merge
// ---------------------------------------------------------------------------

/// Structured outcome from a rebase invocation.
///
/// Returned by the rebase core so callers (CLI flow, sibling auto-rebase) can
/// surface results in their own format without parsing stdout. Stable enough
/// to be exposed at crate root for downstream summarizers (bn-3vf5).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RebaseOutcome {
    /// Number of commits that were replayed onto the new epoch.
    pub replayed: usize,
    /// Number of unresolved conflict entries in the resulting state.
    pub conflicts: usize,
    /// Number of replay steps that introduced at least one conflict.
    pub conflicted_steps: usize,
    /// Number of replay steps where the post-rebase sanity check (bn-2upt)
    /// flagged at least one path. Distinct from `conflicted_steps`: a
    /// step might fall into both buckets if it had a textual conflict
    /// AND a sanity flag on a different path. With
    /// `merge.strict_post_rebase_check = true` (default) sanity-flagged
    /// paths are converted into conflicts and contribute to
    /// `conflicted_steps` too.
    pub sanity_flagged_steps: usize,
    /// True when there were no commits to replay (workspace was already up
    /// to date with the old epoch and just needed a fast-forward sync).
    pub fast_forwarded: bool,
    /// True iff the worktree files were synchronized to the new HEAD. When
    /// `mutate_worktree` was false this is always false. When `mutate_worktree`
    /// was true but `continue_past_worktree_failure` allowed the call to skip
    /// the checkout (newly-dirty re-check or transient I/O), this is also
    /// false and `worktree_skip_reason` carries the diagnostic.
    pub worktree_updated: bool,
    /// Diagnostic for callers that want to surface "refs synced, worktree
    /// not". Empty when `worktree_updated` is true or worktree mutation was
    /// not requested at all.
    pub worktree_skip_reason: String,
}

/// Tunables for [`rebase_workspace_run`]. The two existing CLI entry points
/// (`maw ws sync --rebase` and the sibling auto-rebase orchestrator) differ
/// in whether they print progress, whether they sync the worktree, whether
/// they own the per-workspace lock, and whether worktree-update failure is
/// fatal. The bools are independent caller-specific guarantees — a state-
/// machine refactor would just rename the same product type.
#[allow(
    clippy::struct_excessive_bools,
    reason = "four orthogonal caller-specific guarantees, not a state machine"
)]
#[derive(Clone, Copy, Debug)]
pub(super) struct RebaseRunOptions {
    /// When true, emit progress / conflict-resolution help to stdout.
    pub print: bool,
    /// When true, advance HEAD via `set_head` AND `checkout_tree` so the
    /// worktree files match. When false, only `set_head` runs — the worktree
    /// is left at its old contents and will be reconciled the next time the
    /// owning agent runs a workspace command.
    pub mutate_worktree: bool,
    /// When true, the function may itself acquire the per-workspace lock.
    /// Set to `false` by callers that have already acquired the lock so the
    /// re-checks (dirty / merge-state) can be done under that same lock —
    /// see `auto_rebase_siblings` in `super::auto_rebase`.
    pub acquire_lock: bool,
    /// When true (and `mutate_worktree` is also true), failures during the
    /// final worktree-update phase do NOT cause the function to return Err —
    /// instead `RebaseOutcome::worktree_updated` is set to false and
    /// `worktree_skip_reason` carries a short diagnostic. This is only used
    /// by sibling auto-rebase (bn-103k): refs must advance even if a
    /// transient I/O error or a freshly-dirty worktree blocks the checkout.
    /// Also gates a final dirty re-check immediately before checkout, so a
    /// worktree that becomes dirty between the lock acquisition and the
    /// checkout is skipped (logged) rather than clobbered.
    pub continue_past_worktree_failure: bool,
}

impl Default for RebaseRunOptions {
    fn default() -> Self {
        Self {
            print: true,
            mutate_worktree: true,
            acquire_lock: true,
            continue_past_worktree_failure: false,
        }
    }
}

/// Replay workspace commits onto the current epoch via the structured-merge
/// engine. Zero shell-outs — everything goes through [`GitRepo`].
///
/// Thin wrapper preserving the `pub ws sync --rebase` user-visible output;
/// real work happens in [`rebase_workspace_run`].
pub(super) fn rebase_workspace(
    root: &Path,
    ws_name: &str,
    old_epoch: &str,
    new_epoch: &str,
    ws_path: &Path,
    ahead_count: u32,
    trigger: &str,
) -> Result<()> {
    rebase_workspace_run(
        root,
        ws_name,
        old_epoch,
        new_epoch,
        ws_path,
        ahead_count,
        RebaseRunOptions::default(),
        trigger,
    )
    .map(|_| ())
}

/// Core rebase routine. Returns a structured [`RebaseOutcome`] so callers
/// that don't want the full stdout flow (sibling auto-rebase) can summarize
/// the result themselves.
#[expect(
    clippy::too_many_lines,
    reason = "rebase command follows the structured merge pipeline in order"
)]
// `trigger`: short context string for the oplog Rebase entry. Use `"sync"` for
// a direct `maw ws sync --rebase` invocation, `"sync-all"` for a
// `maw ws sync --all` batch, or `"auto-rebase:merge(<sources>)"` for a
// sibling auto-rebase triggered by another workspace's merge.
#[allow(clippy::too_many_arguments)]
pub(super) fn rebase_workspace_run(
    root: &Path,
    ws_name: &str,
    old_epoch: &str,
    new_epoch: &str,
    ws_path: &Path,
    ahead_count: u32,
    opts: RebaseRunOptions,
    trigger: &str,
) -> Result<RebaseOutcome> {
    macro_rules! say {
        ($($arg:tt)*) => {
            if opts.print {
                println!($($arg)*);
            }
        };
    }

    // Serialize concurrent rebases on the same workspace (bn-1d1g). Without
    // this, two racing `maw ws sync --rebase <ws>` processes both rewrite
    // HEAD / the worktree and the loser aborts mid-pipeline with an internal
    // error (e.g. `set_head failed: ... No such file or directory`), leaving
    // the workspace in a half-rebased state.
    //
    // Lock is scoped to this function — it drops (and releases the kernel
    // flock) when the function returns or panics. Callers that have already
    // acquired the lock pass `acquire_lock: false`.
    let _lock = if opts.acquire_lock {
        match WorkspaceRebaseLock::try_acquire(root, ws_name) {
            Ok(Some(guard)) => Some(guard),
            Ok(None) => {
                bail!(
                    "Another rebase is in progress for workspace '{ws_name}'. \
                     Wait for it to finish and retry. \
                     (Lock file: {})",
                    maw_core::model::layout::LayoutFlavor::detect_with_env(root)
                        .manifold_dir(root)
                        .join("locks")
                        .join("rebase")
                        .join(format!("{ws_name}.lock"))
                        .display()
                );
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Failed to acquire rebase lock for workspace '{ws_name}': {e}"
                ));
            }
        }
    } else {
        None
    };

    // Safety: refuse to rebase if the workspace has uncommitted changes.
    let is_dirty = workspace_has_uncommitted_changes(ws_path).map_err(|e| {
        anyhow::anyhow!("Failed to check dirty state for workspace '{ws_name}': {e}")
    })?;

    if is_dirty {
        bail!(
            "Workspace '{ws_name}' has uncommitted changes that would be lost by rebase. \
             Commit or stash first.\n  \
             Check: git -C {} status",
            ws_path.display()
        );
    }

    say!(
        "Rebasing workspace '{ws_name}' ({ahead_count} commit(s)) onto epoch {}...",
        &new_epoch[..std::cmp::min(12, new_epoch.len())]
    );
    say!();

    // Open the repo **at the workspace path**. For a linked worktree this
    // makes `set_head` update the workspace's own HEAD (not the common-dir
    // HEAD), matching the old `git checkout --detach` behavior.
    let repo = git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("Failed to open git repo at {}: {e}", ws_path.display()))?;
    let repo_dyn: &dyn GitRepo = &repo;

    // Parse epoch OIDs (both `maw-git` and `maw-core` flavors).
    let old_core = GitOid::new(old_epoch)
        .map_err(|e| anyhow::anyhow!("invalid old epoch {old_epoch}: {e}"))?;
    let new_core = GitOid::new(new_epoch)
        .map_err(|e| anyhow::anyhow!("invalid new epoch {new_epoch}: {e}"))?;
    let old_git: git::GitOid = old_epoch
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid old epoch OID: {e}"))?;
    let new_git: git::GitOid = new_epoch
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid new epoch OID: {e}"))?;

    let ws_id = WorkspaceId::new(ws_name).map_err(|e| anyhow::anyhow!("{e}"))?;
    let base_epoch_id =
        EpochId::new(old_epoch).map_err(|e| anyhow::anyhow!("invalid old epoch id: {e}"))?;

    // Enumerate commits old_epoch..HEAD (oldest first).
    let head_git = repo_dyn
        .rev_parse("HEAD")
        .map_err(|e| anyhow::anyhow!("Failed to rev-parse HEAD: {e}"))?;
    let commits = repo_dyn
        .walk_commits(old_git, head_git, true)
        .map_err(|e| anyhow::anyhow!("Failed to walk commits {old_epoch}..HEAD: {e}"))?;

    if commits.is_empty() {
        say!("No commits to replay. Performing normal sync.");
        let mut worktree_updated = false;
        let mut worktree_skip_reason = String::new();
        if opts.mutate_worktree {
            // Re-check dirty immediately before the worktree-touching phase
            // (bn-103k race window: lock excludes other maw processes, but an
            // editor save could have just landed). If we're allowed to skip,
            // do so; otherwise let the original guard inside
            // `sync_worktree_to_epoch` surface the error.
            if opts.continue_past_worktree_failure {
                match workspace_has_uncommitted_changes(ws_path) {
                    Ok(true) => {
                        worktree_skip_reason = "dirty re-check before checkout".to_string();
                        let epoch_ref = manifold_refs::workspace_epoch_ref(ws_name);
                        if let Err(e) = manifold_refs::write_ref(root, &epoch_ref, &new_core) {
                            tracing::warn!(
                                workspace = %ws_name,
                                epoch_ref = %epoch_ref,
                                oid = %new_core,
                                error = %e,
                                "failed to update workspace epoch ref during sibling auto-rebase"
                            );
                        }
                    }
                    Ok(false) => {
                        // Sibling auto-rebase calls with print=false so the
                        // chatty `  ✓ <ws> - synced ...` line doesn't leak
                        // into the merge summary (regression from bn-103k
                        // flipping mutate_worktree to true).
                        // No CAS guard (None): we already hold the workspace
                        // lock and re-checked dirty state above (bn-103k). The
                        // lock prevents concurrent maw processes; the dirty
                        // re-check is the guard we need here.
                        let sync_res = if opts.print {
                            sync_worktree_to_epoch(root, ws_name, new_epoch, None)
                        } else {
                            sync_worktree_to_epoch_quiet(root, ws_name, new_epoch)
                        };
                        match sync_res {
                            Ok(_) => worktree_updated = true,
                            Err(e) => {
                                tracing::warn!(
                                    workspace = %ws_name,
                                    error = %e,
                                    "sibling auto-rebase: worktree fast-forward failed; refs still advanced"
                                );
                                worktree_skip_reason = format!("worktree update: {e}");
                                // Refs still need to advance even though checkout failed.
                                let epoch_ref = manifold_refs::workspace_epoch_ref(ws_name);
                                if let Err(e) =
                                    manifold_refs::write_ref(root, &epoch_ref, &new_core)
                                {
                                    tracing::warn!(
                                        workspace = %ws_name,
                                        epoch_ref = %epoch_ref,
                                        oid = %new_core,
                                        error = %e,
                                        "failed to update workspace epoch ref during sibling auto-rebase"
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        worktree_skip_reason = format!("dirty re-check failed: {e}");
                        let epoch_ref = manifold_refs::workspace_epoch_ref(ws_name);
                        if let Err(e) = manifold_refs::write_ref(root, &epoch_ref, &new_core) {
                            tracing::warn!(
                                workspace = %ws_name,
                                epoch_ref = %epoch_ref,
                                oid = %new_core,
                                error = %e,
                                "failed to update workspace epoch ref during sibling auto-rebase"
                            );
                        }
                    }
                }
            } else {
                // No CAS guard (None): caller holds the lock; this is a
                // direct `maw ws sync --rebase` invocation, not an exec
                // pre-hook racing against concurrent commits.
                if opts.print {
                    sync_worktree_to_epoch(root, ws_name, new_epoch, None)?;
                } else {
                    sync_worktree_to_epoch_quiet(root, ws_name, new_epoch)?;
                }
                worktree_updated = true;
            }
        } else {
            // Refs-only path: advance only the per-workspace epoch ref so
            // `WorkspaceStatus::is_stale` clears. The worktree gets updated
            // the next time the owning agent runs a workspace command.
            let epoch_ref = manifold_refs::workspace_epoch_ref(ws_name);
            if let Err(e) = manifold_refs::write_ref(root, &epoch_ref, &new_core) {
                tracing::warn!(
                    workspace = %ws_name,
                    epoch_ref = %epoch_ref,
                    oid = %new_core,
                    error = %e,
                    "failed to update workspace epoch ref during sibling auto-rebase"
                );
            }
        }
        say!();
        // bn-6xpz: the fast-forward advanced HEAD but may land on a target
        // epoch that carries committed conflict content from an earlier
        // auto-rebase. Check before claiming success.
        let effective_conflicts = match crate::workspace::conflict_state::effective_conflict_state(
            root, ws_name, ws_path,
        ) {
            Ok(residual) if residual.is_conflicted() => {
                let n = residual.conflict_count();
                say!(
                    "Workspace synced, but has {n} unresolved conflict(s) from an earlier \
                         rebase still committed in this workspace:"
                );
                if opts.print {
                    for path in residual.unresolved_paths() {
                        println!("  - {}", path.display());
                    }
                    print_conflict_guidance(ws_name);
                }
                n
            }
            Ok(_) => {
                say!("Workspace synced successfully.");
                0
            }
            Err(e) => {
                // Could not verify — do NOT claim success; warn and let
                // the caller inspect manually. Keep conservative: 0 so
                // the caller can still proceed if it ignores warnings.
                tracing::warn!(
                    workspace = %ws_name,
                    error = %e,
                    "fast-forward: could not verify conflict state after sync"
                );
                say!(
                    "Workspace synced, but could not verify conflict state ({e}); \
                         run `maw ws resolve {ws_name} --list` to confirm."
                );
                0
            }
        };
        // bn-20sa (Part 4): record a Rebase oplog entry for the fast-forward
        // path too so `maw ws history <ws>` shows ff operations.
        record_rebase_op(
            root,
            ws_name,
            &ws_id,
            old_epoch,
            new_epoch,
            &head_git.to_string(),
            new_epoch, // new_head == new_epoch for a fast-forward
            0,         // replayed = 0
            0,         // conflicts = 0 (effective_conflicts may be > 0 from a prior rebase)
            trigger,
        );

        return Ok(RebaseOutcome {
            replayed: 0,
            conflicts: effective_conflicts,
            conflicted_steps: 0,
            sanity_flagged_steps: 0,
            fast_forwarded: true,
            worktree_updated,
            worktree_skip_reason,
        });
    }

    // Read the new epoch's tree OID — we'll use it as the base for `edit_tree`.
    let new_epoch_commit = repo_dyn
        .read_commit(new_git)
        .map_err(|e| anyhow::anyhow!("Failed to read new epoch commit {new_epoch}: {e}"))?;
    let new_epoch_tree = new_epoch_commit.tree_oid;

    // Seed the ConflictTree: clean map populated from the new-epoch tree;
    // `base_epoch` is set to the **old** epoch so `diff_patchset` produces
    // patches that `apply_unilateral_patchset` will accept.
    let mut state = seed_conflict_tree_from_epoch(repo_dyn, new_git, base_epoch_id.clone())?;

    // Pre-compute the epoch delta (old → new) so we can detect three-way
    // overlap: if a workspace commit modifies a path that the epoch also
    // changed, we must synthesize a `Conflict::Content` rather than silently
    // overwriting the epoch version. See the doc for
    // `promote_overlaps_to_conflicts` for the full rationale.
    let epoch_delta = build_epoch_delta_map(repo_dyn, old_git, new_git)?;

    // bn-2upt — load merge sanity config once and thread through the
    // overlap path. If the config file fails to load (or just isn't
    // there) we use defaults — i.e. strict ON, ratio 1.5x. Failing
    // closed: a config we can't parse is not a license to skip the
    // check.
    let manifold_config = ManifoldConfig::load(
        &maw_core::model::layout::LayoutFlavor::detect_with_env(root).bootstrap_config_path(root),
    )
    .unwrap_or_default();
    let sanity_cfg = PostRebaseSanityConfig::from_merge(&manifold_config.merge);
    let mut sanity_flagged_steps = 0usize;
    let mut sanity_flagged_paths_total: Vec<PathBuf> = Vec::new();

    let total = commits.len();
    let mut parent_git = new_git;
    let mut replayed = 0usize;
    let mut conflicted_steps = 0usize;

    for (i, commit_git) in commits.iter().copied().enumerate() {
        let commit_hex = commit_git.to_string();
        let short_sha = &commit_hex[..std::cmp::min(12, commit_hex.len())];
        let commit_core = GitOid::new(&commit_hex)
            .map_err(|e| anyhow::anyhow!("malformed commit OID {commit_hex}: {e}"))?;
        let commit_info = repo_dyn
            .read_commit(commit_git)
            .map_err(|e| anyhow::anyhow!("Failed to read commit {short_sha}: {e}"))?;

        let parent_oids = &commit_info.parents;
        if parent_oids.is_empty() {
            // Root commit appearing in an old_epoch..HEAD range would mean
            // old_epoch is unrelated. Skip defensively.
            say!(
                "  [{}/{}] skipping root commit {short_sha} (no parents)",
                i + 1,
                total
            );
            continue;
        }

        let first_parent_core = GitOid::new(&parent_oids[0].to_string())
            .map_err(|e| anyhow::anyhow!("malformed parent OID: {e}"))?;
        let mut first_parent_patch = diff_patchset(
            repo_dyn,
            &first_parent_core,
            &commit_core,
            &ws_id,
            &base_epoch_id,
            50,
        )
        .map_err(|e| anyhow::anyhow!("Failed to extract patchset for {short_sha}: {e}"))?;

        let conflicts_before = state.conflicts.len();

        // Pre-pass: for every Add/Modify in the workspace patch that also hits
        // a path changed by the epoch (alice's side), install a
        // `Conflict::Content` on the clean entry so `apply_unilateral_patchset`
        // enters its conflict-propagation branch. The V1 propagation
        // replaces-and-collapses, but we `materialize` BEFORE the next fold,
        // and we inspect the conflicts pre-apply to surface them here.
        //
        // This pass may also mutate the patch (e.g. drop `Modified(to)` for
        // rename pairs that were resolved into a pre-installed clean entry
        // at `to` — see `promote_overlaps_to_conflicts` for the rationale).
        let mut sanity_flagged_this_step: Vec<PathBuf> = Vec::new();
        promote_overlaps_to_conflicts(
            repo_dyn,
            &mut state,
            &mut first_parent_patch,
            &epoch_delta,
            ws_name,
            &base_epoch_id,
            sanity_cfg,
            &mut sanity_flagged_this_step,
        )
        .map_err(|e| anyhow::anyhow!("{e} (while replaying {short_sha})"))?;
        if !sanity_flagged_this_step.is_empty() {
            sanity_flagged_steps += 1;
            sanity_flagged_paths_total.extend(sanity_flagged_this_step);
        }

        // Snapshot for sidecar before apply_unilateral_patchset's V1 "modifed
        // replaces/collapses" semantics collapse the newly-injected conflicts
        // back into clean. The commit's rendered-marker tree is what lands on
        // disk, but the sidecar gets the pre-collapse state so
        // `find_conflicted_files` + `ws resolve` can see the per-side detail.
        let snapshot_with_conflicts = if state.has_conflicts() {
            Some(state.clone())
        } else {
            None
        };

        // Apply first-parent delta.
        state = apply_unilateral_patchset(state, first_parent_patch.clone()).map_err(|e| {
            anyhow::anyhow!("apply_unilateral_patchset failed for {short_sha}: {e}")
        })?;

        // V1 multi-parent handling: for merge commits, synthesize an explicit
        // `Conflict::Content` at every path touched by the non-first parents.
        // This ensures merge-commit content isn't silently dropped (bn-372v).
        if parent_oids.len() > 1 {
            for (idx, side_parent) in parent_oids.iter().enumerate().skip(1) {
                let side_parent_core = GitOid::new(&side_parent.to_string())
                    .map_err(|e| anyhow::anyhow!("malformed merge parent OID: {e}"))?;
                let side_patch = diff_patchset(
                    repo_dyn,
                    &side_parent_core,
                    &commit_core,
                    &ws_id,
                    &base_epoch_id,
                    50,
                )
                .map_err(|e| {
                    anyhow::anyhow!(
                        "Failed to extract merge-side patchset for {short_sha} (parent #{idx}): {e}"
                    )
                })?;

                inject_merge_side_conflicts(&mut state, ws_name, &commit_core, idx, &side_patch);
            }
        }

        // If we captured a conflict-bearing snapshot before the V1 collapse,
        // restore its conflicts now so they survive to the sidecar. The clean
        // map from the post-apply state is still authoritative for everything
        // not in that snapshot's conflicts.
        if let Some(snap) = snapshot_with_conflicts {
            for (path, conflict) in snap.conflicts {
                if !state.conflicts.contains_key(&path) {
                    state.conflicts.insert(path.clone(), conflict);
                    // Evict any collapsed-to-clean entry so the conflict wins.
                    state.clean.remove(&path);
                }
            }
        }

        let step_introduced_conflicts = state.conflicts.len() > conflicts_before;
        if step_introduced_conflicts {
            conflicted_steps += 1;
        }

        // Materialize, write blobs, build tree, commit.
        //
        // `materialize` reads each conflict side's blob via `repo.read_blob`
        // (bn-324m), so we thread the `&dyn GitRepo` through here — same
        // handle that `write_blobs_and_build_tree` uses below to write the
        // rendered marker blobs back.
        let output = materialize(&state, repo_dyn)
            .map_err(|e| anyhow::anyhow!("materialize failed after replaying {short_sha}: {e}"))?;
        let tree_oid = write_blobs_and_build_tree(repo_dyn, new_epoch_tree, output)
            .map_err(|e| anyhow::anyhow!("failed to build tree for {short_sha}: {e}"))?;

        let commit_msg = if commit_info.message.is_empty() {
            format!("rebase: replay {short_sha}")
        } else {
            commit_info.message.clone()
        };

        // Preserve merge-commit DAG shape (bn-7mbe). If the original had
        // ≥2 parents, the replayed commit must too — otherwise downstream
        // tooling that inspects `git log --format=%P` or walks parents sees
        // a silently-flattened linear chain.
        //
        // V1 limitation: only the first parent is rebased (it's the chain
        // head we've been building). The second (and subsequent) parents
        // are carried over as the ORIGINAL pre-rebase OIDs — semantically
        // "this references the side content that was merged in" — so
        // `git log --graph` will show the extra parent(s) pointing back into
        // the pre-rebase branch. A future follow-up can rebase the side
        // branches too and substitute the rebased OIDs here.
        let parents_for_commit: Vec<git::GitOid> = if parent_oids.len() > 1 {
            let mut ps = Vec::with_capacity(parent_oids.len());
            ps.push(parent_git);
            ps.extend(parent_oids.iter().skip(1).copied());
            ps
        } else {
            vec![parent_git]
        };

        parent_git = repo_dyn
            .create_commit(tree_oid, &parents_for_commit, &commit_msg, None)
            .map_err(|e| anyhow::anyhow!("create_commit failed for {short_sha}: {e}"))?;
        replayed += 1;

        let summary = commit_msg.lines().next().unwrap_or("(no message)");
        if parent_oids.len() > 1 {
            say!(
                "  [{}/{}] Replayed (merge commit) {short_sha}: {summary}",
                i + 1,
                total
            );
        } else {
            say!("  [{}/{}] Replayed {short_sha}: {summary}", i + 1, total);
        }
    }

    // Final dirty re-check BEFORE we move HEAD. Once `set_head` runs, the
    // worktree (still at the old contents) will look "dirty" relative to the
    // new HEAD even though no user write happened — so this check has to
    // come first to be meaningful. The auto-rebase orchestrator already did
    // one dirty check at lock acquisition; this closes the window between
    // that and the destructive write. With `continue_past_worktree_failure`
    // set we honour the result by skipping the checkout (refs still advance
    // below). Without it, behave as the CLI sync path does — the user
    // explicitly asked for a rebase, the dirty check at function entry has
    // already passed, and `checkout_tree` is allowed to overwrite tracked
    // edits.
    let mut worktree_updated = false;
    let mut worktree_skip_reason = String::new();
    let mut skip_checkout = false;
    if opts.mutate_worktree && opts.continue_past_worktree_failure {
        match workspace_has_uncommitted_changes(ws_path) {
            Ok(true) => {
                worktree_skip_reason = "dirty re-check before checkout".to_string();
                skip_checkout = true;
                tracing::warn!(
                    workspace = %ws_name,
                    "sibling auto-rebase: worktree became dirty after lock-time check; \
                     skipping worktree checkout"
                );
            }
            Ok(false) => {}
            Err(e) => {
                worktree_skip_reason = format!("dirty re-check failed: {e}");
                skip_checkout = true;
                tracing::warn!(
                    workspace = %ws_name,
                    error = %e,
                    "sibling auto-rebase: dirty re-check failed; skipping worktree checkout"
                );
            }
        }
    }

    // bn-20sa: NEVER-ABANDON GUARD — verify that set_head will not silently
    // orphan commits. Two conditions trigger a refusal:
    //
    // (a) CAS check: HEAD must not have moved since we did the walk. If it
    //     did, a concurrent commit landed between the walk and now and we
    //     would orphan it. The caller must re-run `maw ws sync <ws>`.
    //
    // (b) Non-consumed-work check: if HEAD carried commits exclusive to it
    //     (i.e. it is ahead of old_epoch) but this run replayed ZERO of them,
    //     something is wrong — the walk silently returned empty against a
    //     non-empty range. This is the exact failure mode of the bn-1qtj /
    //     bn-3d4a incidents: base=d8542518, head=4be34a20 (1 commit), yet
    //     the rebase replayed 0 and called set_head(new_epoch), orphaning the
    //     commit. We refuse unless old HEAD == old epoch (i.e. no exclusive
    //     work existed — pure fast-forward).
    //
    // Legitimate fast-forward case: head_git == old_git (workspace was at
    // its epoch base; commits is empty and replayed==0 correctly). This is
    // already handled by the `commits.is_empty()` early return above, so we
    // should never reach this point with replayed==0 unless the commit walk
    // failed silently. We still check explicitly as belt-and-suspenders.
    {
        let current_head = repo_dyn
            .rev_parse("HEAD")
            .map_err(|e| anyhow::anyhow!("never-abandon guard: failed to re-read HEAD: {e}"))?;

        // (a) CAS: HEAD must not have moved since the walk.
        if current_head != head_git {
            bail!(
                "SAFETY ABORT: workspace '{ws_name}' HEAD moved between rebase walk and \
                 set_head — a concurrent commit landed and would be orphaned.\n  \
                 HEAD at walk-start: {head_git}\n  \
                 HEAD now:          {current_head}\n  \
                 Remediation: maw ws sync {ws_name}",
            );
        }

        // (b) Non-consumed-work: replayed==0 yet workspace had exclusive commits.
        // head_git != old_git means there were commits in old_epoch..HEAD;
        // we already returned early if commits.is_empty(), so this branch is
        // only reachable if the walk returned a non-empty list but ALL commits
        // were skipped (e.g., every commit was a root-commit and we `continue`d
        // past it). Refuse loudly rather than silently orphaning them.
        if replayed == 0 && head_git != old_git {
            // Check that head_git is truly not an ancestor of parent_git
            // (i.e. not already contained in the new chain). If head_git IS
            // an ancestor of the new tip, the commits were already incorporated
            // and we can proceed safely. This handles the pathological case
            // where the workspace's HEAD happens to be the new epoch (all
            // commits absorbed via epoch advancement).
            let already_contained = repo_dyn.is_ancestor(head_git, parent_git).unwrap_or(false);
            if !already_contained {
                bail!(
                    "SAFETY ABORT: workspace '{ws_name}' walk-start HEAD ({head_git_short}) is \
                     ahead of old epoch ({old_epoch_short}) but this rebase replayed 0 commits — \
                     the commit walk returned no workable entries even though exclusive work exists. \
                     Moving HEAD would silently orphan that work.\n  \
                     Walk-start HEAD: {head_git}\n  \
                     Old epoch:       {old_epoch}\n  \
                     New epoch tip:   {parent_git}\n  \
                     Remediation: maw ws sync {ws_name}",
                    head_git_short = &head_git.to_string()[..12.min(head_git.to_string().len())],
                    old_epoch_short = &old_epoch[..12.min(old_epoch.len())],
                    head_git = head_git,
                    old_epoch = old_epoch,
                    parent_git = parent_git,
                );
            }
        }
    }

    // Advance HEAD to the new chain tip. This is a refs-only step, always
    // performed even if we're going to skip the worktree checkout below.
    repo_dyn
        .set_head(parent_git)
        .map_err(|e| anyhow::anyhow!("set_head failed: {e}"))?;

    if opts.mutate_worktree && !skip_checkout {
        match repo_dyn.checkout_tree(parent_git, ws_path) {
            Ok(()) => worktree_updated = true,
            Err(e) => {
                if opts.continue_past_worktree_failure {
                    tracing::warn!(
                        workspace = %ws_name,
                        error = %e,
                        "sibling auto-rebase: checkout_tree failed; refs still advanced"
                    );
                    worktree_skip_reason = format!("checkout_tree: {e}");
                } else {
                    return Err(anyhow::anyhow!("checkout_tree failed: {e}"));
                }
            }
        }
    }

    // Step 3: Update the workspace's epoch ref to the new epoch. Silent
    // failure would leave a stale ref (bn-3pkx) — surface as a warn.
    {
        let epoch_ref = manifold_refs::workspace_epoch_ref(ws_name);
        if let Err(e) = manifold_refs::write_ref(root, &epoch_ref, &new_core) {
            tracing::warn!(
                workspace = %ws_name,
                epoch_ref = %epoch_ref,
                oid = %new_core,
                error = %e,
                "failed to update workspace epoch ref after rebase — \
                 downstream commands may see a stale epoch"
            );
        }
    }

    // bn-20sa (Part 4): Record a Rebase oplog entry so `maw ws history`
    // shows that the workspace was rebased. Before this, sibling auto-rebases
    // and sync rebases were invisible — `maw ws history <ws>` showed only
    // [create] even after a workspace had been rebased multiple times by other
    // agents' merges.
    //
    // Best-effort: oplog failures must not abort a successful rebase.
    record_rebase_op(
        root,
        ws_name,
        &ws_id,
        old_epoch,
        new_epoch,
        &head_git.to_string(),
        &parent_git.to_string(),
        replayed,
        // effective_conflicts not yet computed; use conflict_count (raw).
        // We'll update this below once effective_conflicts is known, but that
        // requires the sidecar pass. Use state.conflicts.len() here, which is
        // the same value used for `effective_conflicts` in the conflicts branch.
        state.conflicts.len(),
        trigger,
    );

    // Write both sidecars. The legacy one is what `maw ws resolve` still
    // consumes; the structured one is for future tooling (bn-3rah).
    let conflict_count = state.conflicts.len();
    let has_conflicts = state.has_conflicts();
    let mut effective_conflicts = if has_conflicts { conflict_count } else { 0 };
    if has_conflicts {
        write_legacy_sidecar(ws_path, &state, &old_core, &new_core)
            .map_err(|e| anyhow::anyhow!("failed to write legacy sidecar: {e}"))?;
        write_structured_sidecar(ws_path, &state)
            .map_err(|e| anyhow::anyhow!("failed to write structured sidecar: {e}"))?;

        say!();
        if sanity_flagged_steps > 0 {
            say!(
                "Rebase complete: {replayed} commit(s) replayed, \
                 {conflicted_steps} with conflicts ({sanity_flagged_steps} sanity-flagged).",
            );
        } else {
            say!(
                "Rebase complete: {replayed} commit(s) replayed, \
                 {conflicted_steps} with conflicts.",
            );
        }
        say!("Workspace '{ws_name}' has {conflict_count} unresolved conflict(s).");
        if opts.print {
            print_conflict_guidance(ws_name);
        }
    } else {
        // bn-21cj: this replay run introduced no NEW conflicts, but the
        // replayed commits may carry committed conflict content from an
        // earlier rebase (e.g. a quiet sibling auto-rebase committed marker
        // blobs + sidecars during another workspace's merge, and this sync
        // replayed those marker-laden commits onto a newer epoch as ordinary
        // content). "Replayed cleanly" must never print — and the sidecar
        // must never be deleted — while unresolved conflict evidence still
        // sits in HEAD. Inspect the FINAL workspace state with the same
        // helper `maw ws resolve --list` and the merge gate use, so this
        // summary always matches what they report (bn-8zqz).
        match crate::workspace::conflict_state::effective_conflict_state(root, ws_name, ws_path) {
            Ok(residual) if residual.is_conflicted() => {
                effective_conflicts = residual.conflict_count();
                say!();
                say!(
                    "Rebase complete: {replayed} commit(s) replayed, but \
                     {effective_conflicts} unresolved conflict(s) from an earlier \
                     rebase are still committed in this workspace:"
                );
                if opts.print {
                    for path in residual.unresolved_paths() {
                        println!("  - {}", path.display());
                    }
                    print_conflict_guidance(ws_name);
                }
            }
            Ok(residual) => {
                // Truly clean — clear any stale sidecars from a previous
                // attempt (both flavors; this used to delete only the legacy
                // one). `effective_conflict_state` may already have cleared
                // them when it proved the metadata stale.
                if !residual.cleared_stale_sidecar {
                    let _ =
                        super::super::resolve_structured::clear_conflict_sidecars(root, ws_name);
                }
                say!();
                if sanity_flagged_steps > 0 {
                    // Reachable only with strict_post_rebase_check = false:
                    // the check tripped but we accepted the merge anyway.
                    // Surface the flag count alongside the clean count so
                    // it's visible.
                    say!(
                        "Rebase complete: {replayed} commit(s) replayed cleanly \
                         ({sanity_flagged_steps} sanity-flagged but accepted; \
                         set merge.strict_post_rebase_check = true to refuse)."
                    );
                } else {
                    say!("Rebase complete: {replayed} commit(s) replayed cleanly.");
                }
                say!("Workspace '{ws_name}' is now up to date.");
            }
            Err(e) => {
                // Could not verify the final state: do NOT claim "cleanly"
                // and do NOT delete any sidecar.
                tracing::warn!(
                    workspace = %ws_name,
                    error = %e,
                    "post-rebase conflict-state verification failed"
                );
                say!();
                say!("Rebase complete: {replayed} commit(s) replayed.");
                say!(
                    "WARNING: could not verify conflict state ({e}); \
                     run `maw ws resolve {ws_name} --list` to confirm."
                );
            }
        }
    }
    if !sanity_flagged_paths_total.is_empty() {
        tracing::warn!(
            workspace = %ws_name,
            count = sanity_flagged_paths_total.len(),
            paths = ?sanity_flagged_paths_total,
            "post-rebase sanity check flagged paths"
        );
    }

    Ok(RebaseOutcome {
        replayed,
        conflicts: effective_conflicts,
        conflicted_steps,
        sanity_flagged_steps,
        fast_forwarded: false,
        worktree_updated,
        worktree_skip_reason,
    })
}

/// Print the conflict-resolution guidance block shared by the "new conflicts
/// this run" and the bn-21cj "residual committed conflicts" summaries, so
/// agents get identical, copy-pastable next steps either way.
fn print_conflict_guidance(ws_name: &str) {
    println!();
    println!("Conflict markers use labeled sides:");
    println!("  <<<<<<< epoch   — current epoch version");
    println!("  ||||||| base");
    println!("  =======");
    println!("  >>>>>>> {ws_name}   — workspace changes");
    println!();
    println!("To resolve:");
    println!("  maw ws resolve {ws_name} --list                  # list conflicts");
    println!("  maw ws resolve {ws_name} --keep epoch            # keep epoch version");
    println!("  maw ws resolve {ws_name} --keep {ws_name}    # keep workspace version");
    println!("  maw ws resolve {ws_name} --keep both             # keep both sides");
    println!();
    // bn-6xpz: since bn-8zqz, any reader (merge --check, resolve --list,
    // ws conflicts) auto-clears stale conflict metadata after a manual
    // resolution commit. The trailing `maw ws sync` step that the old
    // guidance required is no longer necessary — conflict state clears
    // automatically on the next maw command.
    println!("After resolving, commit your changes:");
    println!(
        "  maw exec {ws_name} -- git add -A && maw exec {ws_name} -- git commit -m \"fix: resolve rebase conflicts\""
    );
    println!("  (Conflict state clears automatically on the next maw command.)");
}

// ---------------------------------------------------------------------------
// Seeding
// ---------------------------------------------------------------------------

/// Recursively walk `epoch_tree_commit`'s tree and return a `ConflictTree`
/// whose `clean` map has one entry per blob in the tree. Non-blob entries
/// (submodules, symlinks, etc.) are preserved with their appropriate
/// `EntryMode` so the rebase round-trips type information.
///
/// The returned tree's `base_epoch` is set to `base_epoch` (the caller
/// provides this — typically the **old** epoch id so subsequent
/// `diff_patchset` outputs match the tree's epoch).
fn seed_conflict_tree_from_epoch(
    repo: &dyn GitRepo,
    epoch_commit_oid: git::GitOid,
    base_epoch: EpochId,
) -> Result<ConflictTree> {
    // Resolve the commit's tree.
    let commit = repo
        .read_commit(epoch_commit_oid)
        .map_err(|e| anyhow::anyhow!("Failed to read epoch commit: {e}"))?;

    let mut tree = ConflictTree::new(base_epoch);
    walk_tree_into_clean(repo, commit.tree_oid, std::path::Path::new(""), &mut tree)?;
    Ok(tree)
}

fn walk_tree_into_clean(
    repo: &dyn GitRepo,
    tree_oid: git::GitOid,
    prefix: &std::path::Path,
    tree: &mut ConflictTree,
) -> Result<()> {
    let entries = repo
        .read_tree(tree_oid)
        .map_err(|e| anyhow::anyhow!("Failed to read tree {tree_oid}: {e}"))?;

    for entry in entries {
        let path = prefix.join(&entry.name);
        match entry.mode {
            git::EntryMode::Tree => {
                walk_tree_into_clean(repo, entry.oid, &path, tree)?;
            }
            git::EntryMode::Blob
            | git::EntryMode::BlobExecutable
            | git::EntryMode::Link
            | git::EntryMode::Commit => {
                let mode_core: EntryMode = entry.mode.into();
                let oid_core = GitOid::new(&entry.oid.to_string())
                    .map_err(|e| anyhow::anyhow!("malformed blob oid in tree: {e}"))?;
                tree.clean
                    .insert(path, MaterializedEntry::new(mode_core, oid_core));
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Epoch-delta overlap detection
// ---------------------------------------------------------------------------

/// `path → (old_epoch_blob_oid, new_epoch_blob_oid)` for every file the
/// epoch transition (old → new) changed (Added/Modified/Renamed/Deleted).
///
/// Used by [`promote_overlaps_to_conflicts`] to spot three-way overlaps:
/// if a workspace commit modifies a path that's in this map, the workspace
/// and the epoch are both racing to change the same file, and the rebase
/// must surface a structured conflict rather than silently keep the epoch
/// version.
type EpochDelta = std::collections::HashMap<std::path::PathBuf, (Option<GitOid>, Option<GitOid>)>;

fn build_epoch_delta_map(
    repo: &dyn GitRepo,
    old_epoch: git::GitOid,
    new_epoch: git::GitOid,
) -> Result<EpochDelta> {
    use maw_git::ChangeType;

    let old_commit = repo
        .read_commit(old_epoch)
        .map_err(|e| anyhow::anyhow!("Failed to read old epoch commit: {e}"))?;
    let new_commit = repo
        .read_commit(new_epoch)
        .map_err(|e| anyhow::anyhow!("Failed to read new epoch commit: {e}"))?;

    let entries = repo
        .diff_trees_with_renames(Some(old_commit.tree_oid), new_commit.tree_oid, 50)
        .map_err(|e| anyhow::anyhow!("Failed to diff epoch trees: {e}"))?;

    let mut map: EpochDelta = EpochDelta::new();
    for entry in entries {
        let path = std::path::PathBuf::from(&entry.path);
        let old_oid = if entry.old_oid.is_zero() {
            None
        } else {
            Some(
                GitOid::new(&entry.old_oid.to_string())
                    .map_err(|e| anyhow::anyhow!("malformed old epoch diff oid: {e}"))?,
            )
        };
        let new_oid = if entry.new_oid.is_zero() {
            None
        } else {
            Some(
                GitOid::new(&entry.new_oid.to_string())
                    .map_err(|e| anyhow::anyhow!("malformed new epoch diff oid: {e}"))?,
            )
        };
        map.insert(path.clone(), (old_oid.clone(), new_oid.clone()));

        // For renames, also record the OLD path so a workspace Delete or
        // Modify against the pre-rename name also registers as overlap.
        if let ChangeType::Renamed { from } = &entry.change_type {
            map.insert(
                std::path::PathBuf::from(from),
                (old_oid, None), // renamed-away paths look "deleted" to workspace
            );
        }
    }
    Ok(map)
}

/// Walk the workspace patchset and, for every Add/Modify on a path that the
/// epoch also changed, replace the `clean` entry with a `Conflict::Content`
/// describing the three-way overlap (epoch-side = ours, workspace-side =
/// theirs, base = old-epoch blob).
///
/// This is the pipeline-level step that turns what would be a silent
/// overwrite into a structured conflict the merge-time marker gate (bn-372v)
/// can surface.
///
/// ## Rename handling (bn-3525)
///
/// `diff_patchset` emits renames as a `Deleted(from) + Modified(to)` pair
/// with a shared `FileId`. When the epoch *independently* modified the
/// renamed-from path, we must **follow the rename**: the epoch's content
/// change applies to the workspace's new path, not the old one. Two
/// sub-cases:
///
/// * **Pure rename** (workspace did not edit content) — the workspace's
///   content at `to` equals the epoch's old content at `from`. We install
///   a clean entry at `to` carrying the epoch's *new* blob, and record the
///   delete side so `apply` still clears `from` from the tree.
///
/// * **Rename + edit** (workspace changed content too) — we have a true
///   three-way overlap at `to`: base = epoch-old, ours = epoch-new,
///   theirs = workspace-content. We install a `Conflict::Content` at `to`
///   and the snapshot-restore step downstream preserves it through the V1
///   apply-collapse.
///
/// In both sub-cases the `Deleted(from)` side is left alone — the default
/// `apply` handling will remove `from` from the clean tree without
/// manufacturing a spurious `ModifyDelete` at the stale path.
#[expect(
    clippy::too_many_lines,
    reason = "rename overlap promotion keeps planning and mutation together"
)]
#[expect(
    clippy::too_many_arguments,
    reason = "promotion plumbs explicit sanity-check state through to the three-way helper"
)]
fn promote_overlaps_to_conflicts(
    repo: &dyn GitRepo,
    tree: &mut ConflictTree,
    patch: &mut maw_core::merge::types::PatchSet,
    epoch_delta: &EpochDelta,
    ws_name: &str,
    base_epoch_id: &EpochId,
    sanity_cfg: PostRebaseSanityConfig,
    sanity_flagged: &mut Vec<PathBuf>,
) -> Result<()> {
    use maw_core::merge::types::ChangeKind;

    // Pre-scan: identify rename pairs within this patch. A rename shows up
    // as a `Deleted(from, F)` and a `Modified(to, F)` with the same FileId
    // (both derived from the same old blob by `diff_patchset`).
    let rename_pairs = collect_rename_pairs(patch);

    // Collect rename resolutions in a first pass (read-only over `patch`) so
    // we can mutate both the tree and the patch's changes afterwards without
    // borrow-checker conflicts.
    let mut rename_resolutions: Vec<RenameResolution> = Vec::new();
    for change in &patch.changes {
        if change.kind == ChangeKind::Modified
            && let Some(ws_blob) = change.blob.clone()
            && let Some(from_path) = rename_pairs.modified_to_source.get(&change.path)
            && let Some((ref_old_from, ref_new_from)) = epoch_delta.get(from_path)
            && let Some(res) = plan_rename_overlap(
                ws_name,
                base_epoch_id,
                patch,
                change,
                ws_blob,
                ref_old_from.clone(),
                ref_new_from.clone(),
            )
        {
            rename_resolutions.push(res);
        }
    }

    // Apply rename resolutions to the tree and patch in a second pass.
    for res in rename_resolutions {
        apply_rename_resolution(tree, &mut patch.changes, res);
    }

    // bn-2dy1: D/F prefix-clash detection.
    //
    // The epoch_delta map is keyed by exact paths (files that changed). But a
    // D/F clash is a structural mismatch where:
    //
    //   Direction 1: ws commits FILE `P`; epoch gained paths under `P/` (epoch
    //   turned P into a directory). The exact-path lookup
    //   `epoch_delta.get(&change.path)` for path `P` returns None because only
    //   `P/a`, `P/b`, etc. appear in the map — the clash is silently missed.
    //
    //   Direction 2: ws commits FILE `P/sub`; epoch gained FILE `P` (epoch
    //   turned P/ into a file). Again, the lookup for `P/sub` returns None,
    //   and `P`'s entry in the epoch tree now collides.
    //
    // Detection is COMPONENT-WISE (`deep` clashes with `deep/leaf` but NOT
    // with `deeper` or `deep.txt`).
    //
    // Representation: `Conflict::ModifyDelete` with `df_hint = Some(P)` where
    // P is the collision root. UNIFORMLY: modifier = the WORKSPACE side (its
    // blob is renderable + restorable), deleter = "epoch".
    //
    // CRITICAL TREE-VALIDITY INVARIANT: a git tree cannot hold a path that is
    // both a file and a directory, so exactly ONE side's paths may exist in
    // the materialized output. We keep the WORKSPACE side's path(s) (rendered
    // as marker stubs at the conflict key) and remove the EPOCH side's
    // clashing entries from `tree.clean`. The epoch side is always restorable
    // from the (immutable) epoch commit — `maw ws resolve --keep epoch` does
    // exactly that. Without this, the rendered stub plus the other side's
    // clean entries produce an invalid tree and `write_blobs_and_build_tree`'s
    // backstop aborts the whole rebase (the wedged-workspace failure this
    // block exists to prevent).
    //
    //   Direction 1: conflict keyed at P (stub = ws file content + markers);
    //   epoch's clean entries under P/ removed.
    //   Direction 2: conflict keyed at the ws child path P/sub (stub = ws
    //   child content + markers); epoch's clean FILE entry at P removed.
    //
    // The ws changes involved are REMOVED from the patch so
    // `apply_unilateral_patchset` cannot collapse the conflict back to a
    // clean entry (V1 replace-and-collapse semantics).
    let mut df_clash_paths: Vec<std::path::PathBuf> = Vec::new();
    {
        for change in &patch.changes {
            if !matches!(change.kind, ChangeKind::Added | ChangeKind::Modified) {
                continue;
            }
            // Skip paths already handled by rename resolution.
            if rename_pairs.modified_to_source.contains_key(&change.path) {
                continue;
            }
            let Some(ws_blob) = change.blob.clone() else {
                continue;
            };

            // Direction 1: ws path P is a FILE; epoch ADDED paths under P/
            // (entries whose new side is Some — a deleted child is not a
            // structural clash).
            let dir_prefix = format!("{}/", change.path.to_string_lossy());
            let epoch_dir_child = epoch_delta.iter().find(|(ep, (_, new))| {
                new.is_some() && ep.to_string_lossy().starts_with(&dir_prefix)
            });
            // Only treat P as D/F when the epoch did NOT also keep a FILE at
            // P (exact-path entry with a live new side is the normal overlap
            // case, handled by the main loop below). An epoch entry at P
            // whose new side is None (epoch deleted file P while adding
            // P/children) IS part of the D/F restructure and is claimed here.
            let epoch_has_live_file_at_p =
                matches!(epoch_delta.get(&change.path), Some((_, Some(_))));
            if epoch_dir_child.is_some() && !epoch_has_live_file_at_p {
                let ord = OrderingKey::new(base_epoch_id.clone(), patch.workspace_id.clone(), 0, 0);
                let file_id = FileId::new(merge_file_id_seed(
                    &GitOid::new(&"d".repeat(40)).expect("operation should succeed"),
                    &change.path,
                ));
                // modifier = workspace (has FILE content at P); deleter =
                // epoch (turned P into a directory). The deleter's blob is
                // the representative child's blob so the conflict record
                // points at real epoch content.
                let modifier = ConflictSide::new(ws_name.to_owned(), ws_blob.clone(), ord.clone());
                let deleter_blob = epoch_dir_child
                    .and_then(|(_, (_, new))| new.clone())
                    .unwrap_or_else(|| ws_blob.clone());
                let deleter = ConflictSide::new("epoch".to_owned(), deleter_blob, ord);

                // Tree-validity: the stub will live at P, so the epoch's
                // children under P/ must leave the clean map (restorable
                // from the epoch; `--keep epoch` restores them).
                let children: Vec<std::path::PathBuf> = tree
                    .clean
                    .keys()
                    .filter(|p| p.to_string_lossy().starts_with(&dir_prefix))
                    .cloned()
                    .collect();
                for child in children {
                    tree.clean.remove(&child);
                }
                tree.clean.remove(&change.path);

                tree.conflicts.insert(
                    change.path.clone(),
                    Conflict::ModifyDelete {
                        path: change.path.clone(),
                        file_id,
                        modifier,
                        deleter,
                        modified_content: ws_blob.clone(),
                        // rename_hint deliberately None: it triggers the
                        // bn-heb8 "deleted by rename" note, which would be
                        // misleading here. The D/F note keys off df_hint.
                        rename_hint: None,
                        df_hint: Some(change.path.clone()),
                    },
                );
                df_clash_paths.push(change.path.clone());
                continue;
            }

            // Direction 2: ws path P/sub is a FILE; epoch has FILE `P` (epoch
            // turned the directory prefix into a file). Check every
            // component-wise ancestor of change.path against epoch_delta.
            let mut dir_ancestor = change.path.parent();
            while let Some(ancestor) = dir_ancestor {
                if ancestor == std::path::Path::new("") {
                    break;
                }
                if let Some((_, Some(ep_file_blob))) = epoch_delta.get(ancestor)
                    && !epoch_delta.contains_key(&change.path)
                {
                    let ep_file_blob = ep_file_blob.clone();
                    let ord =
                        OrderingKey::new(base_epoch_id.clone(), patch.workspace_id.clone(), 0, 0);
                    let file_id = FileId::new(merge_file_id_seed(
                        &GitOid::new(&"d".repeat(40)).expect("operation should succeed"),
                        &change.path,
                    ));
                    // modifier = workspace (the child FILE under P/);
                    // deleter = epoch (whose FILE at P clashes with the
                    // ws's directory P/). The conflict is keyed at the WS
                    // child path so the stub renders inside the directory —
                    // a valid tree shape.
                    let modifier =
                        ConflictSide::new(ws_name.to_owned(), ws_blob.clone(), ord.clone());
                    let deleter = ConflictSide::new("epoch".to_owned(), ep_file_blob, ord);

                    // Tree-validity: the epoch's FILE at the ancestor must
                    // leave the clean map (it clashes with the ws's
                    // directory). Restorable from the epoch via
                    // `--keep epoch`.
                    let ancestor_path = ancestor.to_path_buf();
                    tree.clean.remove(&ancestor_path);

                    tree.conflicts.insert(
                        change.path.clone(),
                        Conflict::ModifyDelete {
                            path: change.path.clone(),
                            file_id,
                            modifier,
                            deleter,
                            modified_content: ws_blob.clone(),
                            rename_hint: None,
                            df_hint: Some(ancestor_path),
                        },
                    );
                    df_clash_paths.push(change.path.clone());
                    break;
                }
                dir_ancestor = ancestor.parent();
            }
        }
    }

    // Remove D/F-claimed changes from the patch entirely: the conflict stub
    // is rendered from the conflict record, and leaving the change in the
    // patch would let `apply_unilateral_patchset`'s V1 collapse semantics
    // replace the conflict with a clean entry — re-creating the invalid
    // file+directory tree shape.
    if !df_clash_paths.is_empty() {
        patch
            .changes
            .retain(|change| !df_clash_paths.contains(&change.path));
    }

    let mut auto_resolved_paths: Vec<std::path::PathBuf> = Vec::new();

    for change in &patch.changes {
        // Skip paths that were already handled by D/F clash detection.
        if df_clash_paths.contains(&change.path) {
            continue;
        }
        match change.kind {
            ChangeKind::Added | ChangeKind::Modified => {
                let Some(ws_blob) = change.blob.clone() else {
                    continue;
                };

                // Skip Modified changes that are the destination of a
                // rename pair — they were handled above.
                if rename_pairs.modified_to_source.contains_key(&change.path) {
                    continue;
                }

                let Some((ref_old, ref_new)) = epoch_delta.get(&change.path) else {
                    // Path not touched by the epoch — no overlap.
                    continue;
                };

                // If the epoch's new-side blob equals what the workspace
                // produced, there's no real divergence.
                if let Some(epoch_new) = ref_new
                    && *epoch_new == ws_blob
                {
                    continue;
                }

                // If the workspace's blob is identical to the old base
                // (workspace effectively reverts to base while epoch went
                // forward), the epoch version wins and there's no conflict.
                if let Some(epoch_old) = ref_old
                    && *epoch_old == ws_blob
                {
                    continue;
                }

                // bn-3hqg: submodule (gitlink) conflicts are not yet
                // supported. A workspace bumped the submodule to one SHA
                // while the epoch bumped it to a different SHA — the merge
                // engine has no way to run a textual 3-way merge across two
                // gitlink OIDs (they aren't blobs), and rendering diff3
                // markers would be meaningless. Bail with a clear error so
                // the user can resolve the submodule manually (rather than
                // producing a cryptic "not found: blob" later in
                // materialize).
                if change.mode == Some(EntryMode::Commit) {
                    bail!(
                        "submodule conflict at {} (workspace bumped to {}, epoch bumped to {:?}) is not yet supported; resolve the submodule manually",
                        change.path.display(),
                        ws_blob,
                        ref_new,
                    );
                }

                let Some(epoch_side_blob) = ref_new.clone() else {
                    // bn-566k: epoch DELETED this path while the workspace
                    // ADDED or MODIFIED it.  Both change.kind == Added (the
                    // workspace re-introduced the file at a path the epoch
                    // removed) and change.kind == Modified (the workspace
                    // edited a file that the epoch subsequently deleted) fall
                    // here.  We must surface a ModifyDelete conflict (modifier
                    // = ws, deleter = epoch) rather than letting the workspace
                    // content sail through clean — that silent pass-through
                    // causes the merged deletion to be silently resurrected on
                    // main (the "silent overwrite" class bn-7phd/epoch-delta
                    // injection exists to prevent, for the delete direction).
                    //
                    // Resolution flows (mirror the existing ws-deletes
                    // direction, sides swapped):
                    //   --keep epoch   → deleter.workspace == "epoch" → None
                    //                    → file stays deleted   ✓
                    //   --keep <ws>    → modifier.workspace == ws_name
                    //                    → Ok(Some(ws_blob))  ✓
                    //   --keep both    → bn-2pry alias → keeps modifier (ws)  ✓
                    //
                    // Submodule guard: the existing Commit-mode check above
                    // (bn-3hqg) already bails before we reach here, so no
                    // special case needed.
                    let ord =
                        OrderingKey::new(base_epoch_id.clone(), patch.workspace_id.clone(), 0, 0);
                    let ws_mode: Option<maw_core::model::conflict::ConflictSideMode> =
                        change.mode.and_then(std::convert::Into::into);
                    // modifier = workspace (has content), deleter = epoch
                    let modifier = ConflictSide::with_mode_and_base(
                        ws_name.to_owned(),
                        ws_blob.clone(),
                        ord.clone(),
                        ws_mode,
                        ref_old.clone(),
                    );
                    // deleter = epoch; `content` holds the last known blob OID
                    // (the old-epoch blob, or the ws blob as a fallback when
                    // there was no pre-existing file — Added case).
                    let deleter = ConflictSide::new(
                        "epoch".to_owned(),
                        ref_old.clone().unwrap_or_else(|| ws_blob.clone()),
                        ord,
                    );
                    let file_id = change.file_id.unwrap_or_else(|| {
                        FileId::new(merge_file_id_seed(
                            &GitOid::new(&"d".repeat(40)).expect("operation should succeed"),
                            &change.path,
                        ))
                    });
                    // bn-heb8: detect whether the epoch's "deletion" was
                    // actually a rename. A rename in the epoch appears as:
                    //   old path → (Some(old_blob), None)   [deleted here]
                    //   new path → (Some(old_blob), Some(new_blob))  [added there]
                    // We perform exact-blob-match only (v1 scope): the new
                    // path's OLD side must equal this path's ref_old (same
                    // content before the rename). Content changed DURING the
                    // rename is NOT matched.
                    let rename_hint =
                        detect_epoch_rename_target(epoch_delta, &change.path, ref_old.as_ref());
                    tree.clean.remove(&change.path);
                    tree.conflicts.insert(
                        change.path.clone(),
                        Conflict::ModifyDelete {
                            path: change.path.clone(),
                            file_id,
                            modifier,
                            deleter,
                            modified_content: ws_blob,
                            rename_hint,
                            df_hint: None,
                        },
                    );
                    continue;
                };

                if let Some(resolved) = try_clean_three_way_overlap(
                    repo,
                    &change.path,
                    ref_old.as_ref(),
                    &epoch_side_blob,
                    &ws_blob,
                    tree.clean.get(&change.path).map(|e| e.mode),
                    change.mode,
                    ws_name,
                    sanity_cfg,
                    sanity_flagged,
                )? {
                    tree.conflicts.remove(&change.path);
                    tree.clean.insert(change.path.clone(), resolved);
                    auto_resolved_paths.push(change.path.clone());
                    continue;
                }

                let ord = OrderingKey::new(base_epoch_id.clone(), patch.workspace_id.clone(), 0, 0);
                // bn-mg0j: propagate the workspace-side file mode into the
                // conflict so resolvers can re-apply symlink/executable
                // modes after `--keep`. We don't have mode info for the
                // epoch side here (the epoch-delta map carries OIDs only),
                // so leave that side's mode as `None`; the workspace-side
                // hint is what matters for symlink-aware resolution in V1.
                let ws_mode: Option<maw_core::model::conflict::ConflictSideMode> =
                    change.mode.and_then(std::convert::Into::into);
                // bn-3mbj: thread the merge-base blob into both sides so the
                // resolver can run a 3-way merge during `--keep <ws>`. The
                // base is whatever `try_clean_three_way_overlap` had above
                // — `ref_old`, the old epoch's blob at this path. When it's
                // `None` (rename / no-common-ancestor path), the resolver
                // falls back to legacy blob-replace with a stderr warning.
                let ours = ConflictSide::with_base(
                    "epoch".to_owned(),
                    epoch_side_blob.clone(),
                    ord.clone(),
                    ref_old.clone(),
                );
                let theirs = ConflictSide::with_mode_and_base(
                    ws_name.to_owned(),
                    ws_blob,
                    ord,
                    ws_mode,
                    ref_old.clone(),
                );

                let file_id = change.file_id.unwrap_or_else(|| {
                    FileId::new(merge_file_id_seed(
                        &GitOid::new(&"f".repeat(40)).expect("operation should succeed"),
                        &change.path,
                    ))
                });

                // Install the conflict, evicting the clean entry.
                tree.clean.remove(&change.path);
                tree.conflicts.insert(
                    change.path.clone(),
                    Conflict::Content {
                        path: change.path.clone(),
                        file_id,
                        base: ref_old.clone(),
                        sides: vec![ours, theirs],
                        atoms: vec![],
                    },
                );
            }
            ChangeKind::Deleted => {
                // If this delete is the source half of a rename pair within
                // the same patch, skip the ModifyDelete promotion — the
                // rename-aware branch above will have installed the right
                // shape at the destination, and `apply` will take care of
                // clearing `from` from `tree.clean` (bn-3525).
                if rename_pairs.deleted_from_paths.contains(&change.path) {
                    continue;
                }

                let Some((ref_old, ref_new)) = epoch_delta.get(&change.path) else {
                    continue;
                };

                // Workspace wants to delete a file the epoch modified.
                // That's a modify/delete conflict from the workspace's
                // perspective. Only meaningful if the epoch kept the file.
                if ref_new.is_none() {
                    continue;
                }
                let Some(epoch_new) = ref_new.clone() else {
                    continue;
                };

                // bn-3hqg follow-up: delete-vs-bump on a submodule is still a
                // gitlink conflict, not a text/blob conflict. The modifier SHA
                // points at a commit in another repository, so materializing a
                // generic ModifyDelete would fail later when it tried to read
                // that SHA as a blob.
                if change.mode == Some(EntryMode::Commit) {
                    bail!(
                        "submodule conflict at {} (workspace deleted the submodule, epoch bumped it to {}) is not yet supported; resolve the submodule manually",
                        change.path.display(),
                        epoch_new,
                    );
                }

                let ord = OrderingKey::new(base_epoch_id.clone(), patch.workspace_id.clone(), 0, 0);
                let modifier =
                    ConflictSide::new("epoch".to_owned(), epoch_new.clone(), ord.clone());
                let deleter = ConflictSide::new(
                    ws_name.to_owned(),
                    ref_old.clone().unwrap_or_else(|| epoch_new.clone()),
                    ord,
                );
                let file_id = change.file_id.unwrap_or_else(|| {
                    FileId::new(merge_file_id_seed(
                        &GitOid::new(&"e".repeat(40)).expect("operation should succeed"),
                        &change.path,
                    ))
                });
                tree.clean.remove(&change.path);
                tree.conflicts.insert(
                    change.path.clone(),
                    Conflict::ModifyDelete {
                        path: change.path.clone(),
                        file_id,
                        modifier,
                        deleter,
                        modified_content: epoch_new,
                        rename_hint: None,
                        df_hint: None,
                    },
                );
            }
        }
    }

    if !auto_resolved_paths.is_empty() {
        patch.changes.retain(|change| {
            !(matches!(change.kind, ChangeKind::Added | ChangeKind::Modified)
                && auto_resolved_paths.contains(&change.path))
        });
    }

    Ok(())
}

/// Configuration for the post-rebase sanity check (bn-2upt).
///
/// Built from `MergeConfig` and passed through the rebase machinery so the
/// per-three-way-merge code can decide whether a "clean" output looks
/// implausible — and if so, route through the conflict-tree path instead
/// of silently accepting it.
#[derive(Clone, Copy, Debug)]
pub struct PostRebaseSanityConfig {
    /// When true (default), a tripped sanity check makes the three-way
    /// overlap merge fall through to the conflict-tree path. When false,
    /// trips are emitted as stderr warnings and the merge is accepted as
    /// clean anyway.
    pub strict: bool,
    /// Maximum allowed `merged_size / max(ours, theirs, base)` ratio
    /// before the size-delta check flags the merge.
    pub size_ratio_max: f64,
}

impl PostRebaseSanityConfig {
    pub(crate) const fn from_merge(cfg: &maw_core::config::MergeConfig) -> Self {
        Self {
            strict: cfg.strict_post_rebase_check,
            size_ratio_max: cfg.post_rebase_size_ratio_max,
        }
    }

    /// Disabled config: never trips. Used by callers that explicitly opt
    /// out (config-load failure path treats this like "load defaults
    /// instead of bypass" — see `rebase_workspace_run`).
    #[allow(
        dead_code,
        reason = "used by integration tests that construct rebase machinery directly"
    )]
    pub(super) const fn disabled() -> Self {
        Self {
            strict: false,
            size_ratio_max: f64::INFINITY,
        }
    }
}

// bn-c5ui: canonical implementations live in `sync::sanity`; re-exported here
// so that callers in `working_copy.rs` and existing tests are unaffected.
pub use super::sanity::{SanityFailure, check_size_delta};
// `check_ast_parse` is only exercised from tests compiled under the
// `ast-merge` feature; re-export it under the same gate to avoid a dead-import
// warning in the default (no-ast-merge) build.
#[cfg(feature = "ast-merge")]
#[allow(unused_imports)]
pub use super::sanity::check_ast_parse;

/// Compose the size-delta and AST-parse checks. Order: cheapest first.
///
/// Delegates to [`super::sanity::run_post_merge_sanity`]; the rebase-specific
/// wrapper forwards `cfg.size_ratio_max` so the shared logic is consistent.
fn run_post_merge_sanity(
    path: &std::path::Path,
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    merged: &[u8],
    cfg: PostRebaseSanityConfig,
) -> Result<(), SanityFailure> {
    super::sanity::run_post_merge_sanity(
        path,
        base,
        ours,
        theirs,
        merged,
        super::sanity::PostMergeSanityConfig {
            size_ratio_max: cfg.size_ratio_max,
        },
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "three-way overlap helper takes explicit blob identities"
)]
fn try_clean_three_way_overlap(
    repo: &dyn GitRepo,
    path: &std::path::Path,
    base_blob: Option<&GitOid>,
    epoch_blob: &GitOid,
    workspace_blob: &GitOid,
    epoch_mode: Option<EntryMode>,
    workspace_mode: Option<EntryMode>,
    ws_name: &str,
    sanity_cfg: PostRebaseSanityConfig,
    sanity_flagged: &mut Vec<PathBuf>,
) -> Result<Option<MaterializedEntry>> {
    let Some(base_blob) = base_blob else {
        return Ok(None);
    };

    let base = read_blob_by_core_oid(repo, base_blob)
        .map_err(|e| anyhow::anyhow!("failed to read base blob for {}: {e}", path.display()))?;
    let epoch = read_blob_by_core_oid(repo, epoch_blob)
        .map_err(|e| anyhow::anyhow!("failed to read epoch blob for {}: {e}", path.display()))?;
    let workspace = read_blob_by_core_oid(repo, workspace_blob).map_err(|e| {
        anyhow::anyhow!(
            "failed to read workspace blob for {} during three-way overlap merge: {e}",
            path.display()
        )
    })?;

    // bn-1hmz: binary guard — if any blob is binary (NUL byte or invalid
    // UTF-8), the text merge driver must not run. A binary file edited by
    // both epoch and workspace is ALWAYS a conflict; the safe rendering in
    // materialize (bn-ad5z) will emit a binary-conflict stub when the path
    // reaches the conflict-tree path. Without this guard, `merge_text`
    // produces a "clean" frankenstein result on binary files that happen to
    // contain 0x0A bytes (common: executables, images, archives), silently
    // committing corrupted bytes as a "rebased clean" merge — exactly what
    // git's own merge driver refuses to do. The bn-2upt sanity check does
    // NOT catch this (size-plausible, .bin not AST-parsed).
    if !looks_text(&base) || !looks_text(&epoch) || !looks_text(&workspace) {
        return Ok(None);
    }

    let merged =
        match merge_text(&base, &epoch, &workspace, "epoch", "base", ws_name).map_err(|e| {
            anyhow::anyhow!("three-way overlap merge failed for {}: {e}", path.display())
        })? {
            MergeResult::Clean(bytes) => bytes,
            MergeResult::Conflict(_) => return Ok(None),
        };

    // bn-2upt — defense-in-depth: even when `merge_text` reports clean,
    // sanity-check the output before accepting it. If it looks
    // implausible (size or AST parse), route through the conflict-tree
    // path. Fail closed: under `strict=true` any sanity failure → defer
    // to conflict path (return Ok(None)). Under `strict=false` we still
    // log a warning but accept the merge.
    if let Err(failure) =
        run_post_merge_sanity(path, &base, &epoch, &workspace, &merged, sanity_cfg)
    {
        if sanity_cfg.strict {
            eprintln!(
                "warning: post-rebase sanity check tripped for {}: {failure}; \
                 routing through conflict-tree path (set merge.strict_post_rebase_check = false to override)",
                path.display()
            );
            tracing::warn!(
                workspace = %ws_name,
                path = %path.display(),
                failure = %failure,
                "post-rebase sanity check tripped — converting clean merge to conflict"
            );
            sanity_flagged.push(path.to_path_buf());
            return Ok(None);
        }
        eprintln!(
            "warning: post-rebase sanity check tripped for {}: {failure}; \
             accepting merge anyway (merge.strict_post_rebase_check = false)",
            path.display()
        );
        tracing::warn!(
            workspace = %ws_name,
            path = %path.display(),
            failure = %failure,
            "post-rebase sanity check tripped but strict mode is off — accepting clean merge"
        );
    }

    let rel_path = path.to_string_lossy().replace('\\', "/");
    let merged_git_oid = repo
        .write_blob_with_path(&merged, &rel_path)
        .map_err(|e| anyhow::anyhow!("failed to write merged blob for {}: {e}", path.display()))?;
    let merged_oid = GitOid::new(&merged_git_oid.to_string())
        .map_err(|e| anyhow::anyhow!("invalid merged blob oid for {}: {e}", path.display()))?;
    let mode = epoch_mode.or(workspace_mode).unwrap_or(EntryMode::Blob);

    Ok(Some(MaterializedEntry::new(mode, merged_oid)))
}

fn read_blob_by_core_oid(repo: &dyn GitRepo, oid: &GitOid) -> Result<Vec<u8>, git::GitError> {
    let git_oid: git::GitOid =
        oid.as_str()
            .parse()
            .map_err(|e: git::OidParseError| git::GitError::InvalidOid {
                value: oid.as_str().to_owned(),
                reason: e.to_string(),
            })?;
    repo.read_blob(git_oid)
}

/// Rename-pair indices derived from a single [`PatchSet`].
///
/// A rename is encoded by `diff_patchset` as `Deleted(from, FileId=F) +
/// Modified(to, FileId=F)`. These maps let `promote_overlaps_to_conflicts`
/// recognize the pair by path and by `FileId`.
#[derive(Default)]
struct RenamePairs {
    /// Every `to` path for a rename pair → its matching `from` path.
    modified_to_source: std::collections::HashMap<std::path::PathBuf, std::path::PathBuf>,
    /// Every `from` path for a rename pair.
    deleted_from_paths: std::collections::HashSet<std::path::PathBuf>,
}

/// Walk the patchset and identify `Deleted(from) + Modified(to)` pairs
/// sharing a `FileId`. Both sides of the pair are added to the returned
/// index so the caller can look up either direction.
fn collect_rename_pairs(patch: &maw_core::merge::types::PatchSet) -> RenamePairs {
    use maw_core::merge::types::ChangeKind;

    let mut pairs = RenamePairs::default();

    // Group by FileId: collect (file_id) → (Vec<delete_paths>, Vec<modify_paths>).
    let mut by_fid: std::collections::HashMap<
        FileId,
        (Vec<std::path::PathBuf>, Vec<std::path::PathBuf>),
    > = std::collections::HashMap::new();

    for change in &patch.changes {
        let Some(fid) = change.file_id else { continue };
        let entry = by_fid.entry(fid).or_default();
        match change.kind {
            ChangeKind::Deleted => entry.0.push(change.path.clone()),
            ChangeKind::Modified => entry.1.push(change.path.clone()),
            ChangeKind::Added => { /* not part of a rename pair */ }
        }
    }

    // A rename pair is a FileId with exactly one Delete and exactly one
    // Modified. Ambiguous groupings (e.g. two deletes, or a split-rename)
    // are left alone — they fall back to the default per-change handling.
    for (_fid, (deletes, modifies)) in by_fid {
        if deletes.len() == 1 && modifies.len() == 1 {
            let from = deletes
                .into_iter()
                .next()
                .expect("operation should succeed");
            let to = modifies
                .into_iter()
                .next()
                .expect("operation should succeed");
            // Sanity check: a rename pair must have distinct paths.
            if from != to {
                pairs.deleted_from_paths.insert(from.clone());
                pairs.modified_to_source.insert(to, from);
            }
        }
    }

    pairs
}

/// A planned resolution for one rename-vs-epoch-modify overlap.
///
/// Produced by [`plan_rename_overlap`] during the read-only pass over the
/// patch; consumed by [`apply_rename_resolution`] which mutates both the
/// conflict tree and the patch itself.
#[expect(
    clippy::large_enum_variant,
    reason = "rename resolution variants carry path/conflict context for diagnostics"
)]
enum RenameResolution {
    /// Pure rename (workspace carried content unchanged across the move) —
    /// the epoch's new blob lands at `to`.
    Follow {
        /// Destination path (`to`).
        to_path: std::path::PathBuf,
        /// Epoch's new-side blob at `from` — the content that follows the
        /// rename.
        epoch_new_blob: GitOid,
        /// Tree-entry mode to use for the clean entry at `to`. Taken from
        /// the workspace's `Modified(to)` change when available, else
        /// defaults to `EntryMode::Blob`.
        mode: EntryMode,
    },
    /// Rename + edit vs epoch modify — surface a three-way content conflict
    /// at `to`.
    Conflict {
        /// Destination path (`to`).
        to_path: std::path::PathBuf,
        /// The fully-built `Conflict::Content` to install at `to`.
        conflict: Conflict,
    },
}

/// Plan a rename-vs-epoch-modify resolution without mutating anything.
///
/// Called during the read-only pass over the patch. Returns `None` when the
/// overlap does not need special handling (e.g. the epoch deleted `from`;
/// the workspace's change-at-`to` has no blob), leaving the default
/// per-change handling to take over.
fn plan_rename_overlap(
    ws_name: &str,
    base_epoch_id: &EpochId,
    patch: &maw_core::merge::types::PatchSet,
    change: &maw_core::merge::types::FileChange,
    ws_blob: GitOid,
    epoch_old: Option<GitOid>,
    epoch_new: Option<GitOid>,
) -> Option<RenameResolution> {
    // If the epoch's new-side at `from` is None (epoch deleted `from`), both
    // sides agree the old path is gone. The workspace's rename stands
    // unchallenged; default handling upserts `to` with its own content.
    let epoch_new_blob = epoch_new?;

    // Pure rename detection: workspace's content at `to` equals epoch's old
    // content at `from`. When true, epoch's modification can follow cleanly.
    // Defensive: if epoch_old is missing, we can't prove pure-rename;
    // fall through to a conflict so nothing is silently overwritten.
    let is_pure_rename = epoch_old.as_ref().is_some_and(|old| *old == ws_blob);

    if is_pure_rename {
        let mode = change.mode.unwrap_or(EntryMode::Blob);
        Some(RenameResolution::Follow {
            to_path: change.path.clone(),
            epoch_new_blob,
            mode,
        })
    } else {
        let ord = OrderingKey::new(base_epoch_id.clone(), patch.workspace_id.clone(), 0, 0);
        // bn-mg0j: propagate the workspace-side mode into the conflict.
        let ws_mode: Option<maw_core::model::conflict::ConflictSideMode> =
            change.mode.and_then(std::convert::Into::into);
        // bn-3mbj: thread the merge-base blob into both sides so the
        // resolver can run a 3-way merge during `--keep <ws>`. For
        // rename-vs-modify, `epoch_old` is the original blob at the rename
        // source — the workspace's pre-rebase content at `to` was derived
        // from it.
        let ours = ConflictSide::with_base(
            "epoch".to_owned(),
            epoch_new_blob,
            ord.clone(),
            epoch_old.clone(),
        );
        let theirs = ConflictSide::with_mode_and_base(
            ws_name.to_owned(),
            ws_blob,
            ord,
            ws_mode,
            epoch_old.clone(),
        );

        let file_id = change.file_id.unwrap_or_else(|| {
            FileId::new(merge_file_id_seed(
                &GitOid::new(&"f".repeat(40)).expect("operation should succeed"),
                &change.path,
            ))
        });

        Some(RenameResolution::Conflict {
            to_path: change.path.clone(),
            conflict: Conflict::Content {
                path: change.path.clone(),
                file_id,
                base: epoch_old,
                sides: vec![ours, theirs],
                atoms: vec![],
            },
        })
    }
}

/// Apply a planned rename resolution to both the conflict tree and the
/// workspace's patch.
///
/// * Evicts any stale entry for `to_path` from `tree.clean` and
///   `tree.conflicts`.
/// * Installs the follow-the-rename clean entry or the rename+modify
///   `Conflict::Content`.
/// * **Mutates the patch**: removes the `Modified(to_path)` change so the
///   subsequent `apply_unilateral_patchset` doesn't clobber our rename-aware
///   resolution with the workspace's stale blob. The paired `Deleted(from)`
///   is intentionally left in place — it still needs to run during apply to
///   clear `from` from `tree.clean`.
fn apply_rename_resolution(
    tree: &mut ConflictTree,
    patch_changes: &mut Vec<maw_core::merge::types::FileChange>,
    res: RenameResolution,
) {
    use maw_core::merge::types::ChangeKind;

    let to_path = match &res {
        RenameResolution::Follow { to_path, .. } | RenameResolution::Conflict { to_path, .. } => {
            to_path.clone()
        }
    };

    tree.clean.remove(&to_path);
    tree.conflicts.remove(&to_path);

    match res {
        RenameResolution::Follow {
            to_path,
            epoch_new_blob,
            mode,
        } => {
            tree.clean
                .insert(to_path, MaterializedEntry::new(mode, epoch_new_blob));
        }
        RenameResolution::Conflict { to_path, conflict } => {
            tree.conflicts.insert(to_path, conflict);
        }
    }

    // Drop the workspace's Modified(to) from the patch so apply doesn't
    // overwrite our resolution. The paired Deleted(from) stays so apply
    // still clears `from` from the tree.
    patch_changes.retain(|c| !(c.kind == ChangeKind::Modified && c.path == to_path));
}

// ---------------------------------------------------------------------------
// Merge-commit handling
// ---------------------------------------------------------------------------

/// For a merge commit with N parents, we've already applied the first-parent
/// delta via `apply_unilateral_patchset`. For every non-first parent, we
/// synthesize an explicit `Conflict::Content` at each path that parent
/// touched — preserving the "non-first side" content as a second side of
/// the conflict. The effect: `materialize` renders marker blobs into these
/// files so `find_conflicted_files` (which diffs `base..HEAD` for
/// `+<<<<<<<`) trips the merge-time marker gate (bn-372v).
///
/// V1 SIMPLIFICATION: we don't attempt true multi-side three-way merging of
/// each parent's delta. Each non-first parent contributes **one** side per
/// touched path (its post-merge blob). A future bone can extend this to
/// produce fully atomized `ConflictAtom`s.
///
/// ## Convergence collapse (bn-2ras)
///
/// Before installing (or extending) a `Conflict::Content`, we check whether
/// every side would carry the same blob OID. If they would, there is no real
/// conflict — all parents agree on the final content — so we collapse to a
/// clean tree entry instead of manufacturing a phantom conflict that
/// `--keep` has nothing to pick. This fires for merge commits whose parents
/// are byte-identical on a given path (e.g. a cross-branch rename+modify
/// that happens to produce the same bytes on both sides).
fn inject_merge_side_conflicts(
    tree: &mut ConflictTree,
    ws_name: &str,
    commit_core: &GitOid,
    parent_index: usize,
    side_patch: &maw_core::merge::types::PatchSet,
) {
    use maw_core::merge::types::ChangeKind;

    // Synthetic ordering key pinned to this rebase step. Only used for
    // display ordering — the concrete timestamp is irrelevant to conflict
    // semantics.
    let ord = OrderingKey::new(
        tree.base_epoch.clone(),
        side_patch.workspace_id.clone(),
        parent_index as u64,
        0,
    );
    let side_ws_label = format!("{ws_name}#merge-parent-{parent_index}");

    for change in &side_patch.changes {
        let path = change.path.clone();

        // We only care about content-bearing sides for the marker rendering.
        let Some(blob) = change.blob.clone() else {
            continue;
        };
        // Skip deletions — they don't contribute a `<<<<<<<` marker.
        if matches!(change.kind, ChangeKind::Deleted) {
            continue;
        }

        let new_side = ConflictSide::new(side_ws_label.clone(), blob.clone(), ord.clone());

        if let Some(existing) = tree.conflicts.remove(&path) {
            // Promote / extend an existing conflict.
            match existing {
                Conflict::Content {
                    path: p,
                    file_id,
                    base,
                    mut sides,
                    atoms,
                } => {
                    sides.push(new_side);
                    // bn-2ras: if every side now shares the same blob OID,
                    // there is no conflict — all parents agree. Collapse to
                    // a clean entry and move on. The workspace-side mode hint
                    // (the first one we find) is the best approximation we
                    // have; default to `Blob` if no side carries one.
                    if sides_all_same(&sides) {
                        let mode = change.mode.unwrap_or(EntryMode::Blob);
                        let oid = sides
                            .first()
                            .map_or_else(|| blob.clone(), |s| s.content.clone());
                        tree.clean.insert(p, MaterializedEntry::new(mode, oid));
                    } else {
                        tree.conflicts.insert(
                            p.clone(),
                            Conflict::Content {
                                path: p,
                                file_id,
                                base,
                                sides,
                                atoms,
                            },
                        );
                    }
                }
                other => {
                    // Reinsert unchanged — we don't know how to extend other
                    // shapes from a merge-side delta in V1.
                    tree.conflicts.insert(path.clone(), other);
                }
            }
            continue;
        }

        // New conflict: seed `ours` from whatever the first-parent apply
        // left in `tree.clean` (if anything). This gives the marker block a
        // meaningful "ours" side even though both OIDs came out of the merge
        // commit's own content.
        let ours_oid = tree
            .clean
            .get(&path)
            .map_or_else(|| blob.clone(), |e| e.oid.clone());

        // bn-2ras: if the "ours" OID (first-parent's effective content) and
        // the new merge-parent side are byte-identical, both parents agree —
        // there is no conflict. Keep the existing clean entry intact (if
        // present) or install a fresh clean entry from the agreed blob. We
        // must NOT install a Conflict::Content with identical sides because
        // that produces a marker-file that `--keep` can't resolve.
        if ours_oid == blob {
            let mode = change.mode.unwrap_or(EntryMode::Blob);
            // Preserve the existing clean entry's mode if there is one —
            // otherwise seed a fresh entry from the change's mode hint.
            tree.clean
                .entry(path)
                .or_insert_with(|| MaterializedEntry::new(mode, ours_oid));
            continue;
        }

        let ours_side = ConflictSide::new(
            format!("{ws_name}#merge-parent-0"),
            ours_oid.clone(),
            ord.clone(),
        );

        let file_id = FileId::new(merge_file_id_seed(commit_core, &path));
        tree.clean.remove(&path);
        tree.conflicts.insert(
            path.clone(),
            Conflict::Content {
                path,
                file_id,
                base: Some(ours_oid),
                sides: vec![ours_side, new_side],
                atoms: vec![],
            },
        );
    }
}

/// Returns `true` when `sides` is non-empty and every entry shares the same
/// blob OID. Used by [`inject_merge_side_conflicts`] to collapse phantom
/// conflicts where every parent contributed identical content (bn-2ras).
fn sides_all_same(sides: &[ConflictSide]) -> bool {
    sides
        .first()
        .is_some_and(|first| sides.iter().all(|s| s.content == first.content))
}

/// Scan the epoch delta for a rename whose source had the same blob OID as
/// `deleted_path`. If the epoch deleted `deleted_path` (`ref_old_blob`) and
/// added another path with an OLD side equal to `ref_old_blob`, the deletion
/// was actually a rename and this function returns the destination path.
///
/// Exact blob-match only (bn-heb8 v1 scope). A rename where the content also
/// changed (`old_blob` ≠ `new_blob`) will not match; in that case `None` is
/// returned so we do not annotate with a false rename hint.
fn detect_epoch_rename_target(
    epoch_delta: &EpochDelta,
    deleted_path: &std::path::Path,
    ref_old_blob: Option<&GitOid>,
) -> Option<std::path::PathBuf> {
    let old_blob = ref_old_blob?;
    // A rename in the epoch delta produces two entries:
    //   deleted_path → (Some(old_blob), None)    — the source was removed
    //   new_path     → (Some(old_blob), Some(…)) — the content appeared here
    // We're looking for a NEW path (ref_new is Some) whose OLD side matches
    // the deleted path's old blob.
    epoch_delta
        .iter()
        .find_map(|(candidate, (cand_old, cand_new))| {
            if candidate == deleted_path {
                return None; // same path, not a rename target
            }
            if cand_new.is_none() {
                return None; // target was also deleted — not a rename destination
            }
            if cand_old.as_ref() == Some(old_blob) {
                Some(candidate.clone())
            } else {
                None
            }
        })
}

/// Deterministic `FileId` seed used for merge-commit-induced conflicts.
///
/// Not based on `file_id_from_blob` because we want the same path across
/// a repeated rebase to get the same id. Truncated SHA-256 of commit OID
/// + path is deterministic enough for display-only purposes.
fn merge_file_id_seed(commit: &GitOid, path: &std::path::Path) -> u128 {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(commit.as_str().as_bytes());
    h.update(b"\0");
    h.update(path.to_string_lossy().as_bytes());
    let digest = h.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    u128::from_be_bytes(bytes)
}

// ---------------------------------------------------------------------------
// Tree build
// ---------------------------------------------------------------------------

/// Take a [`MaterializedOutput`], write any `Rendered` blobs to the object
/// store, and produce the new root tree by editing the new-epoch base tree.
///
/// Rendered conflict blobs replace the base-tree entry at the same path;
/// clean entries are either already present in the base tree (pass-through)
/// or come from workspace-side adds (upserted).
///
/// Paths in the base tree that are absent from `output.entries` are removed
/// from the final tree — this matches the "all entries are present in the
/// materialized output" invariant: `seed_conflict_tree_from_epoch` loaded
/// every base-tree blob into `clean` before any patches were applied, so a
/// missing output entry really means the rebase wanted to delete it.
fn write_blobs_and_build_tree(
    repo: &dyn GitRepo,
    base_tree: git::GitOid,
    output: maw_core::merge::materialize::MaterializedOutput,
) -> Result<git::GitOid> {
    use maw_core::merge::materialize::FinalEntry;

    // Collect the set of paths currently in the base tree so we can compute
    // which ones need to be explicitly removed.
    let mut base_paths = std::collections::BTreeSet::<String>::new();
    collect_blob_paths(repo, base_tree, "", &mut base_paths)?;

    let mut edits: Vec<TreeEdit> = Vec::new();
    let mut final_paths = std::collections::BTreeSet::<String>::new();

    for (path, entry) in output.entries {
        // Trees use forward-slash paths regardless of host OS.
        let path_str = path.to_string_lossy().replace('\\', "/");
        final_paths.insert(path_str.clone());
        match entry {
            FinalEntry::Clean { mode, oid } => {
                let git_mode: git::EntryMode = mode.into();
                let git_oid: git::GitOid = oid
                    .as_str()
                    .parse()
                    .map_err(|e| anyhow::anyhow!("bad blob oid in clean entry: {e}"))?;
                edits.push(TreeEdit::Upsert {
                    path: path_str,
                    mode: git_mode,
                    oid: git_oid,
                });
            }
            FinalEntry::Rendered { mode, content } => {
                let git_oid = repo
                    .write_blob_with_path(&content, &path_str)
                    .map_err(|e| anyhow::anyhow!("write_blob failed for {path_str}: {e}"))?;
                let git_mode: git::EntryMode = mode.into();
                edits.push(TreeEdit::Upsert {
                    path: path_str,
                    mode: git_mode,
                    oid: git_oid,
                });
            }
        }
    }

    // Any base-tree path not in final_paths must be removed.
    //
    // bn-2dy1 EXCEPTION: when a base-tree FILE became a DIRECTORY in the
    // output (some final path lives under "<base_path>/"), the upsert of the
    // deeper path already replaced the blob with a subtree. Emitting a
    // `Remove(base_path)` here would delete that whole subtree — silently
    // dropping the just-written entries (this is how the D/F direction-2
    // conflict stub vanished from the rebased tree). Skip the removal; the
    // file→directory replacement is complete without it.
    for base_path in &base_paths {
        if !final_paths.contains(base_path) {
            let dir_prefix = format!("{base_path}/");
            let became_directory = final_paths
                .range(dir_prefix.clone()..)
                .next()
                .is_some_and(|p| p.starts_with(&dir_prefix));
            if became_directory {
                continue;
            }
            edits.push(TreeEdit::Remove {
                path: base_path.clone(),
            });
        }
    }

    // bn-2dy1 defense-in-depth: detect D/F clashes in the output tree before
    // handing them to `edit_tree`. If path P is in `final_paths` AND any path
    // under P/ is also in `final_paths`, the tree construction is structurally
    // ambiguous — git cannot represent both a file at P and files under P/ in
    // the same tree. This should have been caught by `promote_df_clashes`
    // earlier; if we still reach here it means a D/F clash slipped through the
    // conflict machinery (e.g. a test fixture bypassing the pipeline). Error
    // loudly rather than silently drop one side.
    //
    // The check is O(N * log N) via sorted-set prefix scan.
    for path_str in &final_paths {
        let dir_prefix = format!("{path_str}/");
        // BTreeSet::range gives us paths that start with the prefix in O(log N).
        if let Some(child) = final_paths.range(dir_prefix.clone()..).next()
            && child.starts_with(&dir_prefix)
        {
            anyhow::bail!(
                "D/F clash in rebase output tree: path '{path_str}' is both a file and a \
                 directory (first child: '{child}') — this is a bug; D/F clashes should have \
                 been surfaced as structured conflicts by promote_overlaps_to_conflicts"
            );
        }
    }

    repo.edit_tree(base_tree, &edits)
        .map_err(|e| anyhow::anyhow!("edit_tree failed: {e}"))
}

/// Recursively collect all blob-entry paths from a tree, slash-joined.
fn collect_blob_paths(
    repo: &dyn GitRepo,
    tree: git::GitOid,
    prefix: &str,
    out: &mut std::collections::BTreeSet<String>,
) -> Result<()> {
    let entries = repo
        .read_tree(tree)
        .map_err(|e| anyhow::anyhow!("read_tree failed: {e}"))?;
    for entry in entries {
        let path = if prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{prefix}/{}", entry.name)
        };
        match entry.mode {
            git::EntryMode::Tree => {
                collect_blob_paths(repo, entry.oid, &path, out)?;
            }
            _ => {
                out.insert(path);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Oplog helpers (bn-20sa Part 4)
// ---------------------------------------------------------------------------

/// Append a `OpPayload::Rebase` entry to the workspace oplog so that
/// `maw ws history <ws>` shows rebase / fast-forward events.
///
/// Best-effort: any failure is logged as a warning and does NOT abort the
/// rebase that already succeeded.
#[expect(
    clippy::too_many_arguments,
    reason = "oplog Rebase records require all these fields; a struct would be more ceremony than the single call-site justifies"
)]
fn record_rebase_op(
    root: &Path,
    ws_name: &str,
    ws_id: &WorkspaceId,
    old_epoch: &str,
    new_epoch: &str,
    old_head: &str,
    new_head: &str,
    replayed: usize,
    conflicts: usize,
    trigger: &str,
) {
    use super::super::oplog_runtime::append_operation_with_runtime_checkpoint;
    use maw_core::model::types::EpochId;

    let old_epoch_id = match EpochId::new(old_epoch) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(workspace = %ws_name, error = %e, "record_rebase_op: invalid old_epoch");
            return;
        }
    };
    let new_epoch_id = match EpochId::new(new_epoch) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(workspace = %ws_name, error = %e, "record_rebase_op: invalid new_epoch");
            return;
        }
    };
    let old_head_oid = match GitOid::new(old_head) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(workspace = %ws_name, error = %e, "record_rebase_op: invalid old_head");
            return;
        }
    };
    let new_head_oid = match GitOid::new(new_head) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(workspace = %ws_name, error = %e, "record_rebase_op: invalid new_head");
            return;
        }
    };

    // Read the current oplog head so we can CAS-append.
    let previous_head = match read_head(root, ws_id) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(workspace = %ws_name, error = %e, "record_rebase_op: failed to read oplog head");
            return;
        }
    };
    let parent_ids: Vec<GitOid> = previous_head.iter().cloned().collect();

    let op = Operation {
        parent_ids,
        workspace_id: ws_id.clone(),
        timestamp: crate::workspace::now_timestamp_iso8601(),
        payload: OpPayload::Rebase {
            old_epoch: old_epoch_id,
            new_epoch: new_epoch_id,
            old_head: old_head_oid,
            new_head: new_head_oid,
            replayed,
            conflicts,
            trigger: trigger.to_owned(),
        },
    };

    if let Err(e) =
        append_operation_with_runtime_checkpoint(root, ws_id, &op, previous_head.as_ref())
    {
        tracing::warn!(
            workspace = %ws_name,
            error = %e,
            "record_rebase_op: failed to append rebase oplog entry (non-fatal)"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebase_conflict_serialization_roundtrip() {
        let conflicts = RebaseConflicts {
            conflicts: vec![
                RebaseConflict {
                    path: "src/main.rs".to_string(),
                    original_commit: "a".repeat(40),
                    base: Some("base content".to_string()),
                    ours: Some("ours content".to_string()),
                    theirs: Some("theirs content".to_string()),
                },
                RebaseConflict {
                    path: "Cargo.toml".to_string(),
                    original_commit: "b".repeat(40),
                    base: None,
                    ours: Some("ours only".to_string()),
                    theirs: None,
                },
            ],
            rebase_from: "c".repeat(40),
            rebase_to: "d".repeat(40),
        };
        let json = serde_json::to_string_pretty(&conflicts).expect("operation should succeed");
        let parsed: RebaseConflicts =
            serde_json::from_str(&json).expect("operation should succeed");
        assert_eq!(parsed.conflicts.len(), 2);
        assert_eq!(parsed.conflicts[0].path, "src/main.rs");
        assert_eq!(parsed.conflicts[1].path, "Cargo.toml");
        assert_eq!(parsed.rebase_from, "c".repeat(40));
        assert_eq!(parsed.rebase_to, "d".repeat(40));
    }

    // -----------------------------------------------------------------------
    // bn-2ras — merge-side convergence collapse
    //
    // When every merge parent contributes byte-identical content to a path,
    // `inject_merge_side_conflicts` must not install a phantom
    // `Conflict::Content` with three convergent sides — it should collapse
    // to a clean entry carrying the agreed blob.
    // -----------------------------------------------------------------------

    use maw_core::merge::types::{ChangeKind, FileChange, PatchSet};
    use maw_core::model::patch::FileId as CoreFileId;
    use maw_core::model::types::{EpochId, WorkspaceId};

    fn test_epoch() -> EpochId {
        EpochId::new(&"e".repeat(40)).expect("operation should succeed")
    }
    fn test_oid(c: char) -> GitOid {
        GitOid::new(&c.to_string().repeat(40)).expect("operation should succeed")
    }
    fn test_ws_id(name: &str) -> WorkspaceId {
        WorkspaceId::new(name).expect("operation should succeed")
    }

    #[test]
    fn inject_merge_side_conflicts_collapses_identical_sides_to_clean() {
        // Seed the tree with a clean entry for `side1.txt` = `A1` — this
        // mimics the state after `apply_unilateral_patchset` folded in the
        // first-parent delta.
        let mut tree = ConflictTree::new(test_epoch());
        let path = std::path::PathBuf::from("side1.txt");
        let shared = test_oid('a');
        tree.clean.insert(
            path.clone(),
            MaterializedEntry::new(EntryMode::Blob, shared.clone()),
        );

        // Second parent's delta also contributes `A1` (same blob) to
        // side1.txt — merge parents converge.
        let side_patch = PatchSet::new(
            test_ws_id("feat"),
            test_epoch(),
            vec![FileChange::with_mode(
                path.clone(),
                ChangeKind::Added,
                None,
                Some(CoreFileId::new(1)),
                Some(shared.clone()),
                Some(EntryMode::Blob),
            )],
        );
        let commit_oid = test_oid('c');

        inject_merge_side_conflicts(&mut tree, "feat", &commit_oid, 1, &side_patch);

        // No phantom conflict should exist.
        assert!(
            tree.conflicts.is_empty(),
            "convergent merge sides must NOT install a conflict, got {:?}",
            tree.conflicts
        );
        // The clean entry survives with the shared blob.
        let entry = tree
            .clean
            .get(&path)
            .expect("clean entry should remain in place");
        assert_eq!(entry.oid, shared);
    }

    #[test]
    fn inject_merge_side_conflicts_installs_conflict_when_sides_differ() {
        // Baseline: when parents genuinely disagree, a conflict is installed.
        let mut tree = ConflictTree::new(test_epoch());
        let path = std::path::PathBuf::from("side1.txt");
        let ours = test_oid('a');
        let theirs = test_oid('b');
        tree.clean
            .insert(path.clone(), MaterializedEntry::new(EntryMode::Blob, ours));

        let side_patch = PatchSet::new(
            test_ws_id("feat"),
            test_epoch(),
            vec![FileChange::with_mode(
                path.clone(),
                ChangeKind::Modified,
                None,
                Some(CoreFileId::new(1)),
                Some(theirs),
                Some(EntryMode::Blob),
            )],
        );
        let commit_oid = test_oid('c');

        inject_merge_side_conflicts(&mut tree, "feat", &commit_oid, 1, &side_patch);

        // A real conflict is installed. `tree.clean` for this path is evicted.
        assert!(!tree.clean.contains_key(&path));
        let conflict = tree
            .conflicts
            .get(&path)
            .expect("divergent parents must produce a conflict");
        match conflict {
            Conflict::Content { sides, .. } => {
                assert_eq!(sides.len(), 2, "expected two sides");
                assert_ne!(
                    sides[0].content, sides[1].content,
                    "sides must differ for a genuine conflict"
                );
            }
            other => panic!("expected Content conflict, got {other:?}"),
        }
    }

    #[test]
    fn sides_all_same_identifies_equal_sides() {
        let o = test_oid('a');
        let ord = OrderingKey::new(test_epoch(), test_ws_id("w"), 0, 0);
        let sides = vec![
            ConflictSide::new("x".to_owned(), o.clone(), ord.clone()),
            ConflictSide::new("y".to_owned(), o.clone(), ord.clone()),
            ConflictSide::new("z".to_owned(), o, ord),
        ];
        assert!(sides_all_same(&sides));
    }

    #[test]
    fn sides_all_same_rejects_divergent_sides() {
        let ord = OrderingKey::new(test_epoch(), test_ws_id("w"), 0, 0);
        let sides = vec![
            ConflictSide::new("x".to_owned(), test_oid('a'), ord.clone()),
            ConflictSide::new("y".to_owned(), test_oid('b'), ord),
        ];
        assert!(!sides_all_same(&sides));
    }

    #[test]
    fn sides_all_same_empty_is_false() {
        assert!(!sides_all_same(&[]));
    }

    // -----------------------------------------------------------------------
    // bn-2upt — post-rebase output sanity check
    // -----------------------------------------------------------------------

    #[test]
    fn check_size_delta_passes_when_merge_is_reasonable() {
        // base ~ 5 bytes, ours/theirs each add a couple of lines, merged is
        // a sensible accumulation of both. Expected = max(o,t) + (o-base) +
        // (t-base) = 17 + 10 + 12 = 39. Merged = 27 → ratio 0.69, well
        // under 1.5x.
        let base = b"BASE\n";
        let ours = b"BASE\nOURS-LINE\n";
        let theirs = b"BASE\nTHEIRS-LINE\n";
        let merged = b"BASE\nOURS-LINE\nTHEIRS-LINE\n";
        check_size_delta(base, ours, theirs, merged, 1.5)
            .expect("legitimate disjoint-add merge must not trip default 1.5x ratio");
    }

    #[test]
    fn check_size_delta_flags_implausible_inflation() {
        let base = b"x\n";
        let ours = b"x\n";
        let theirs = b"x\n";
        // Merged ~ 60 bytes vs max input 2 bytes. ratio = 30. Way over 1.5.
        let merged = b"this is a much larger body that did not come from any input\n";
        let err = check_size_delta(base, ours, theirs, merged, 1.5)
            .expect_err("3x merge result must trip");
        match err {
            SanityFailure::SizeDelta {
                merged_len,
                max_input,
                expected_size,
                ratio,
            } => {
                assert_eq!(merged_len, merged.len());
                assert_eq!(max_input, 2);
                // base=2, ours=2, theirs=2 → ours_added=0, theirs_added=0;
                // expected = max(2,2) + 0 + 0 = 2.
                assert_eq!(expected_size, 2);
                assert!(ratio > 1.5);
            }
            SanityFailure::AstParse { .. } => panic!("size-delta check unexpectedly returned AST"),
        }
    }

    #[test]
    fn check_size_delta_borderline_ratio_does_not_trip() {
        // base=3, ours=4 (+1), theirs=2 (-0 saturating). Expected =
        // max(4,2) + 1 + 0 = 5. Merged = 4 → ratio 0.8. Must pass.
        let base = b"abc";
        let ours = b"abcd";
        let theirs = b"ab";
        let merged = b"wxyz";
        check_size_delta(base, ours, theirs, merged, 1.5)
            .expect("merged within expected envelope must not trip 1.5 threshold");
    }

    #[test]
    fn check_size_delta_zero_input_zero_merged_passes() {
        // All-empty inputs → empty merged output. Trivially fine.
        check_size_delta(&[], &[], &[], &[], 1.5).expect("empty in / empty out is not suspicious");
    }

    #[test]
    fn check_size_delta_zero_input_nonempty_merged_flags() {
        // All-empty inputs but non-empty merged is suspicious — there's
        // nothing the merge could have legitimately drawn from.
        let merged = b"surprise content";
        let err = check_size_delta(&[], &[], &[], merged, 1.5)
            .expect_err("conjuring content from empty inputs must trip");
        match err {
            SanityFailure::SizeDelta {
                merged_len,
                max_input,
                expected_size,
                ratio,
            } => {
                assert_eq!(merged_len, merged.len());
                assert_eq!(max_input, 0);
                assert_eq!(expected_size, 0);
                assert!(ratio.is_infinite());
            }
            SanityFailure::AstParse { .. } => panic!("expected SizeDelta"),
        }
    }

    #[cfg(feature = "ast-merge")]
    #[test]
    fn check_ast_parse_flags_unbalanced_braces_when_inputs_parse_cleanly() {
        // Both sides are valid Rust; the "merged" output has unbalanced
        // braces — this is exactly the bn-4c6g triplication shape.
        let ours = b"fn main() { println!(\"a\"); }\n";
        let theirs = b"fn main() { println!(\"b\"); }\n";
        // Two opening fns, one closing brace. tree-sitter Rust will flag.
        let merged = b"fn main() { println!(\"a\"); fn main() { println!(\"b\"); }\n";
        let path = std::path::Path::new("src/main.rs");
        let err = check_ast_parse(path, ours, theirs, merged)
            .expect_err("merged output with unbalanced braces must trip the AST check");
        match err {
            SanityFailure::AstParse { reason } => {
                assert!(
                    !reason.is_empty(),
                    "AstParse reason should describe the failure"
                );
            }
            SanityFailure::SizeDelta { .. } => panic!("expected AstParse failure"),
        }
    }

    #[cfg(feature = "ast-merge")]
    #[test]
    fn check_ast_parse_passes_when_merged_parses_cleanly() {
        let ours = b"fn a() {}\n";
        let theirs = b"fn b() {}\n";
        let merged = b"fn a() {}\nfn b() {}\n";
        let path = std::path::Path::new("src/lib.rs");
        check_ast_parse(path, ours, theirs, merged).expect("clean concat must not trip");
    }

    #[cfg(feature = "ast-merge")]
    #[test]
    fn check_ast_parse_skips_unsupported_extensions() {
        // No tree-sitter grammar for .txt — the check must early-return Ok.
        let merged = b"this {{{ would not parse as anything";
        check_ast_parse(
            std::path::Path::new("notes.txt"),
            b"any\n",
            b"any\n",
            merged,
        )
        .expect("unsupported extension must skip the check");
    }

    #[cfg(feature = "ast-merge")]
    #[test]
    fn check_ast_parse_skips_when_inputs_already_broken() {
        // Inputs themselves don't parse — the check must NOT blame the
        // merge; return Ok regardless of the merged output's parse state.
        let broken = b"fn main() {{{ this is broken\n";
        let merged_also_broken = b"this is more broken }}}\n";
        check_ast_parse(
            std::path::Path::new("src/main.rs"),
            broken,
            broken,
            merged_also_broken,
        )
        .expect("can't blame a merge when both inputs were already broken");
    }

    #[test]
    fn post_rebase_sanity_config_disabled_never_trips_via_run() {
        // Compose: even with a wildly oversized merge, a disabled config
        // must let it pass the size check. (AST check is independent.)
        let cfg = PostRebaseSanityConfig::disabled();
        let res = check_size_delta(
            b"a",
            b"a",
            b"a",
            b"a".repeat(100).as_slice(),
            cfg.size_ratio_max,
        );
        assert!(res.is_ok(), "infinity ratio must accept any size");
    }

    #[test]
    fn post_rebase_sanity_config_from_merge_uses_defaults() {
        let cfg = PostRebaseSanityConfig::from_merge(&maw_core::config::MergeConfig::default());
        assert!(cfg.strict, "strict_post_rebase_check defaults to true");
        assert!(
            (cfg.size_ratio_max - 1.5).abs() < 1e-9,
            "size ratio defaults to 1.5"
        );
    }
}
