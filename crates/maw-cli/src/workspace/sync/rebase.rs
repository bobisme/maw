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

use std::path::Path;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use maw_core::merge::apply::apply_unilateral_patchset;
use maw_core::merge::diff_extract::diff_patchset;
use maw_core::merge::materialize::{
    materialize, write_legacy_sidecar, write_structured_sidecar,
};
use maw_core::merge::types::{ConflictTree, EntryMode, MaterializedEntry};
use maw_core::model::conflict::{Conflict, ConflictSide};
use maw_core::model::ordering::OrderingKey;
use maw_core::model::patch::FileId;
use maw_core::model::types::{EpochId, GitOid, WorkspaceId};
use maw_core::refs as manifold_refs;
use maw_git::{self as git, GitRepo, TreeEdit};

use super::checks::{sync_worktree_to_epoch, workspace_has_uncommitted_changes};

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
    root.join(".manifold")
        .join("artifacts")
        .join("ws")
        .join(ws_name)
        .join("rebase-conflicts.json")
}

/// Read rebase conflicts for a workspace, if any.
pub fn read_rebase_conflicts(root: &Path, ws_name: &str) -> Option<RebaseConflicts> {
    let path = rebase_conflicts_path(root, ws_name);
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Delete rebase conflicts file for a workspace (called on resolution).
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

/// Replay workspace commits onto the current epoch via the structured-merge
/// engine. Zero shell-outs — everything goes through [`GitRepo`].
pub(super) fn rebase_workspace(
    root: &Path,
    ws_name: &str,
    old_epoch: &str,
    new_epoch: &str,
    ws_path: &Path,
    ahead_count: u32,
) -> Result<()> {
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

    println!(
        "Rebasing workspace '{ws_name}' ({ahead_count} commit(s)) onto epoch {}...",
        &new_epoch[..std::cmp::min(12, new_epoch.len())]
    );
    println!();

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
    let base_epoch_id = EpochId::new(old_epoch)
        .map_err(|e| anyhow::anyhow!("invalid old epoch id: {e}"))?;

    // Enumerate commits old_epoch..HEAD (oldest first).
    let head_git = repo_dyn
        .rev_parse("HEAD")
        .map_err(|e| anyhow::anyhow!("Failed to rev-parse HEAD: {e}"))?;
    let commits = repo_dyn
        .walk_commits(old_git, head_git, true)
        .map_err(|e| anyhow::anyhow!("Failed to walk commits {old_epoch}..HEAD: {e}"))?;

    if commits.is_empty() {
        println!("No commits to replay. Performing normal sync.");
        sync_worktree_to_epoch(root, ws_name, new_epoch)?;
        println!();
        println!("Workspace synced successfully.");
        return Ok(());
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
            println!(
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
        promote_overlaps_to_conflicts(
            &mut state,
            &mut first_parent_patch,
            &epoch_delta,
            ws_name,
            &base_epoch_id,
        );

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
        state = apply_unilateral_patchset(state, first_parent_patch.clone())
            .map_err(|e| anyhow::anyhow!("apply_unilateral_patchset failed for {short_sha}: {e}"))?;

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

                inject_merge_side_conflicts(
                    &mut state,
                    ws_name,
                    &commit_core,
                    idx,
                    &side_patch,
                );
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
        let output = materialize(&state, repo_dyn).map_err(|e| {
            anyhow::anyhow!("materialize failed after replaying {short_sha}: {e}")
        })?;
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
            println!(
                "  [{}/{}] Replayed (merge commit) {short_sha}: {summary}",
                i + 1,
                total
            );
        } else {
            println!("  [{}/{}] Replayed {short_sha}: {summary}", i + 1, total);
        }
    }

    // Advance HEAD + worktree to the new chain tip.
    repo_dyn
        .set_head(parent_git)
        .map_err(|e| anyhow::anyhow!("set_head failed: {e}"))?;
    repo_dyn
        .checkout_tree(parent_git, ws_path)
        .map_err(|e| anyhow::anyhow!("checkout_tree failed: {e}"))?;

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

    // Write both sidecars. The legacy one is what `maw ws resolve` still
    // consumes; the structured one is for future tooling (bn-3rah).
    if state.has_conflicts() {
        let _ = conflicted_steps; // surfaced in stdout above

        write_legacy_sidecar(ws_path, &state, &old_core, &new_core)
            .map_err(|e| anyhow::anyhow!("failed to write legacy sidecar: {e}"))?;
        write_structured_sidecar(ws_path, &state)
            .map_err(|e| anyhow::anyhow!("failed to write structured sidecar: {e}"))?;

        let conflict_count = state.conflicts.len();

        println!();
        println!(
            "Rebase complete: {replayed} commit(s) replayed, {} with conflicts.",
            conflicted_steps,
        );
        println!("Workspace '{ws_name}' has {conflict_count} unresolved conflict(s).");
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
        println!("After resolving, commit and clear conflict state:");
        println!(
            "  maw exec {ws_name} -- git add -A && maw exec {ws_name} -- git commit -m \"fix: resolve rebase conflicts\""
        );
        println!("  maw ws sync {ws_name}");
    } else {
        // Clean run — clear any stale sidecar from a previous attempt.
        let _ = delete_rebase_conflicts(root, ws_name);
        println!();
        println!("Rebase complete: {replayed} commit(s) replayed cleanly.");
        println!("Workspace '{ws_name}' is now up to date.");
    }

    Ok(())
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
    walk_tree_into_clean(repo, commit.tree_oid, std::path::PathBuf::new(), &mut tree)?;
    Ok(tree)
}

fn walk_tree_into_clean(
    repo: &dyn GitRepo,
    tree_oid: git::GitOid,
    prefix: std::path::PathBuf,
    tree: &mut ConflictTree,
) -> Result<()> {
    let entries = repo
        .read_tree(tree_oid)
        .map_err(|e| anyhow::anyhow!("Failed to read tree {tree_oid}: {e}"))?;

    for entry in entries {
        let path = prefix.join(&entry.name);
        match entry.mode {
            git::EntryMode::Tree => {
                walk_tree_into_clean(repo, entry.oid, path, tree)?;
            }
            git::EntryMode::Blob
            | git::EntryMode::BlobExecutable
            | git::EntryMode::Link
            | git::EntryMode::Commit => {
                let mode_core: EntryMode = entry.mode.into();
                let oid_core = GitOid::new(&entry.oid.to_string())
                    .map_err(|e| anyhow::anyhow!("malformed blob oid in tree: {e}"))?;
                tree.clean.insert(path, MaterializedEntry::new(mode_core, oid_core));
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
fn promote_overlaps_to_conflicts(
    tree: &mut ConflictTree,
    patch: &mut maw_core::merge::types::PatchSet,
    epoch_delta: &EpochDelta,
    ws_name: &str,
    base_epoch_id: &EpochId,
) {
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
        {
            if let Some(res) = plan_rename_overlap(
                ws_name,
                base_epoch_id,
                patch,
                change,
                ws_blob,
                ref_old_from.clone(),
                ref_new_from.clone(),
            ) {
                rename_resolutions.push(res);
            }
        }
    }

    // Apply rename resolutions to the tree and patch in a second pass.
    for res in rename_resolutions {
        apply_rename_resolution(tree, &mut patch.changes, res);
    }

    for change in &patch.changes {
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

                let epoch_side_blob = match ref_new {
                    Some(new) => new.clone(),
                    None => continue, // epoch deleted; workspace-re-added → AddAdd-ish, skip for V1
                };

                let ord = OrderingKey::new(
                    base_epoch_id.clone(),
                    patch.workspace_id.clone(),
                    0,
                    0,
                );
                let ours = ConflictSide::new(
                    "epoch".to_owned(),
                    epoch_side_blob.clone(),
                    ord.clone(),
                );
                let theirs = ConflictSide::new(ws_name.to_owned(), ws_blob, ord);

                let file_id = change
                    .file_id
                    .unwrap_or_else(|| FileId::new(merge_file_id_seed(
                        &GitOid::new(&"f".repeat(40)).unwrap(),
                        &change.path,
                    )));

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
                let Some(epoch_new) = ref_new.clone() else { continue };
                let ord = OrderingKey::new(
                    base_epoch_id.clone(),
                    patch.workspace_id.clone(),
                    0,
                    0,
                );
                let modifier = ConflictSide::new("epoch".to_owned(), epoch_new.clone(), ord.clone());
                let deleter = ConflictSide::new(
                    ws_name.to_owned(),
                    ref_old.clone().unwrap_or_else(|| epoch_new.clone()),
                    ord,
                );
                let file_id = change
                    .file_id
                    .unwrap_or_else(|| FileId::new(merge_file_id_seed(
                        &GitOid::new(&"e".repeat(40)).unwrap(),
                        &change.path,
                    )));
                tree.clean.remove(&change.path);
                tree.conflicts.insert(
                    change.path.clone(),
                    Conflict::ModifyDelete {
                        path: change.path.clone(),
                        file_id,
                        modifier,
                        deleter,
                        modified_content: epoch_new,
                    },
                );
            }
        }
    }
}

/// Rename-pair indices derived from a single [`PatchSet`].
///
/// A rename is encoded by `diff_patchset` as `Deleted(from, FileId=F) +
/// Modified(to, FileId=F)`. These maps let `promote_overlaps_to_conflicts`
/// recognize the pair by path and by FileId.
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
            let from = deletes.into_iter().next().unwrap();
            let to = modifies.into_iter().next().unwrap();
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
    let is_pure_rename = match &epoch_old {
        Some(old) => *old == ws_blob,
        // Defensive: if epoch_old is missing, we can't prove pure-rename;
        // fall through to a conflict so nothing is silently overwritten.
        None => false,
    };

    if is_pure_rename {
        let mode = change.mode.unwrap_or(EntryMode::Blob);
        Some(RenameResolution::Follow {
            to_path: change.path.clone(),
            epoch_new_blob,
            mode,
        })
    } else {
        let ord = OrderingKey::new(
            base_epoch_id.clone(),
            patch.workspace_id.clone(),
            0,
            0,
        );
        let ours = ConflictSide::new("epoch".to_owned(), epoch_new_blob, ord.clone());
        let theirs = ConflictSide::new(ws_name.to_owned(), ws_blob, ord);

        let file_id = change.file_id.unwrap_or_else(|| {
            FileId::new(merge_file_id_seed(
                &GitOid::new(&"f".repeat(40)).unwrap(),
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
        RenameResolution::Follow { to_path, .. } => to_path.clone(),
        RenameResolution::Conflict { to_path, .. } => to_path.clone(),
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
            .map(|e| e.oid.clone())
            .unwrap_or_else(|| blob.clone());
        let ours_side = ConflictSide::new(
            format!("{ws_name}#merge-parent-0"),
            ours_oid.clone(),
            ord.clone(),
        );

        let file_id = FileId::new(merge_file_id_seed(commit_core, &path));
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
    for base_path in &base_paths {
        if !final_paths.contains(base_path) {
            edits.push(TreeEdit::Remove {
                path: base_path.clone(),
            });
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
        let json = serde_json::to_string_pretty(&conflicts).unwrap();
        let parsed: RebaseConflicts = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.conflicts.len(), 2);
        assert_eq!(parsed.conflicts[0].path, "src/main.rs");
        assert_eq!(parsed.conflicts[1].path, "Cargo.toml");
        assert_eq!(parsed.rebase_from, "c".repeat(40));
        assert_eq!(parsed.rebase_to, "d".repeat(40));
    }
}
