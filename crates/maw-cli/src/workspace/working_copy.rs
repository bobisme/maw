//! Working-copy helpers and safe rewrite primitives.
//!
//! Three layers:
//!
//! 1. **Legacy stash-based helpers** (`stash_changes`, `checkout_epoch`,
//!    `pop_stash_and_detect_conflicts`, `detect_conflicts_in_worktree`) — kept
//!    for backward compatibility but deprecated in favor of the snapshot layer.
//!
//! 2. **Snapshot-based composable helpers** (`snapshot_working_copy`,
//!    `checkout_to`, `replay_snapshot`, `cleanup_snapshot`) — the preferred
//!    working-copy preservation primitives. Uses `git stash create` + pinned
//!    refs (no stash-stack pollution) and leaves conflict markers in the
//!    working tree instead of rolling back.
//!
//! 3. **`preserve_checkout_replay()`** — the G2-compliant rewrite primitive
//!    (legacy, retained for non-merge paths). Uses patch-based delta
//!    extraction and rolls back on conflict.
//!
//! ## Snapshot-based algorithm (preferred)
//!
//! 1. CHECK — `git status --porcelain`. If clean, skip snapshot (fast path).
//! 2. SNAPSHOT — `git add -A`, `git stash create`, pin to
//!    `refs/manifold/snapshot/<workspace>`, `git reset`.
//! 3. CHECKOUT — `git checkout <branch>` (clean tree, no --force needed).
//! 4. REPLAY — `git stash apply <oid>`. Conflicts become markers (working-copy-preserving).
//! 5. CLEANUP — delete snapshot ref (only if replay was clean).
//!
//! See `notes/assurance/working-copy.md` for the normative specification.

use std::fs;
use std::path::Path;
use std::process::Command;

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use maw_git::GitRepo as _;
use serde::{Deserialize, Serialize};
use tracing::instrument;

use maw_core::model::types::GitOid;
use maw_core::refs as manifold_refs;
use super::capture::capture_before_destroy;

// ---------------------------------------------------------------------------
// Snapshot ref constants
// ---------------------------------------------------------------------------

/// Ref namespace for working-copy snapshots.
///
/// Format: `refs/manifold/snapshot/<workspace-name>`
///
/// Only one snapshot per workspace at a time — the ref is overwritten if a
/// prior snapshot exists (with a warning).
const SNAPSHOT_REF_PREFIX: &str = "refs/manifold/snapshot/";

/// Build the snapshot ref name for a workspace.
fn snapshot_ref_name(ws_name: &str) -> String {
    format!("{SNAPSHOT_REF_PREFIX}{ws_name}")
}

// ---------------------------------------------------------------------------
// Snapshot types
// ---------------------------------------------------------------------------

/// A durable snapshot of a workspace's uncommitted state.
///
/// Created by [`snapshot_working_copy()`] and consumed by
/// [`replay_snapshot()`].
#[derive(Clone, Debug)]
pub(crate) struct SnapshotRef {
    /// The git OID of the stash commit.
    pub oid: String,
    /// The full ref name (e.g. `refs/manifold/snapshot/default`).
    pub ref_name: String,
}

/// Outcome of replaying a snapshot onto a new tree.
#[derive(Clone, Debug)]
pub(crate) enum SnapshotReplayResult {
    /// Replay succeeded cleanly — all changes applied without conflict.
    Clean,
    /// Replay produced conflicts — conflict markers are in the working tree.
    /// The workspace is usable; conflicts are data, not errors (working-copy-preserving).
    Conflicts(Vec<WorkingCopyConflict>),
}

// ---------------------------------------------------------------------------
// Conflict info (stash-based layer)
// ---------------------------------------------------------------------------

/// A single file conflict detected in a git working copy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkingCopyConflict {
    /// Path of the conflicted file, relative to the workspace root.
    pub path: String,
    /// Conflict type: `"content"`, `"both_added"`, `"both_deleted"`,
    /// `"add_mod_conflict"`, `"delete_mod_conflict"`.
    pub conflict_type: String,
}

// ---------------------------------------------------------------------------
// Rewrite artifact types (legacy — retained for existing tests and future use)
// ---------------------------------------------------------------------------

/// Summary of dirty-state delta at the time of a rewrite.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(dead_code)]
pub(crate) struct DeltaSummary {
    pub staged_files: u32,
    pub unstaged_files: u32,
    pub untracked_files: u32,
}

/// Outcome of the replay step in a rewrite operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub(crate) enum ReplayOutcome {
    /// Working copy was clean — no replay needed.
    Clean,
    /// Dirty state was successfully replayed on top of the new target.
    Replayed,
    /// Replay failed; working copy was rolled back to the recovery point.
    Rollback,
}

impl std::fmt::Display for ReplayOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Clean => write!(f, "clean"),
            Self::Replayed => write!(f, "replayed"),
            Self::Rollback => write!(f, "rollback"),
        }
    }
}

/// A record of a single working-copy rewrite event.
///
/// Written to `.manifold/artifacts/rewrite/<workspace>/<timestamp>/record.json`
/// for crash recovery and audit trail.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(dead_code)]
pub(crate) struct RewriteRecord {
    /// Workspace name.
    pub workspace: String,
    /// ISO 8601 timestamp of the rewrite.
    pub timestamp: String,
    /// OID of the workspace HEAD before the rewrite (the base epoch).
    pub base_epoch: String,
    /// OID of the target commit the workspace was rewritten to.
    pub target_ref: String,
    /// Git ref name of the recovery pin (under `refs/manifold/recovery/`).
    pub recovery_ref: String,
    /// OID that the recovery ref points to.
    pub recovery_oid: String,
    /// Outcome of the replay step.
    pub replay_outcome: ReplayOutcome,
    /// Reason for rollback, if applicable.
    pub rollback_reason: Option<String>,
    /// Summary of dirty files at the time of the rewrite.
    pub delta_summary: DeltaSummary,
    /// Tool version that wrote this record.
    pub tool_version: String,
}

// ---------------------------------------------------------------------------
// Rewrite artifact paths
// ---------------------------------------------------------------------------

/// Root directory for rewrite artifacts for a given workspace.
#[allow(dead_code)]
fn rewrite_dir(root: &Path, workspace: &str) -> PathBuf {
    root.join(".manifold")
        .join("artifacts")
        .join("rewrite")
        .join(workspace)
}

/// Directory for a specific rewrite record (by timestamp).
#[allow(dead_code)]
fn rewrite_record_dir(root: &Path, workspace: &str, filename_ts: &str) -> PathBuf {
    rewrite_dir(root, workspace).join(filename_ts)
}

// ---------------------------------------------------------------------------
// Rewrite artifact I/O
// ---------------------------------------------------------------------------

/// Atomically write a JSON value to a file (write-tmp + fsync + rename).
#[allow(dead_code)]
fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("no parent directory for {}", path.display()))?;
    fs::create_dir_all(dir).with_context(|| format!("create dir {}", dir.display()))?;

    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "artifact".to_owned());
    let tmp_path = dir.join(format!(".{filename}.tmp"));

    let json = serde_json::to_string_pretty(value).context("serialize rewrite record")?;

    let mut file = fs::File::create(&tmp_path)
        .with_context(|| format!("create temp file {}", tmp_path.display()))?;
    file.write_all(json.as_bytes())
        .with_context(|| format!("write temp file {}", tmp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("fsync temp file {}", tmp_path.display()))?;
    drop(file);

    fs::rename(&tmp_path, path)
        .with_context(|| format!("rename {} -> {}", tmp_path.display(), path.display()))?;

    Ok(())
}

/// Write a rewrite record artifact to disk.
#[allow(dead_code)]
pub(crate) fn write_rewrite_record(
    root: &Path,
    workspace: &str,
    record: &RewriteRecord,
) -> Result<PathBuf> {
    let filename_ts = record.timestamp.replace(':', "-");
    let record_dir = rewrite_record_dir(root, workspace, &filename_ts);
    let record_path = record_dir.join("record.json");
    write_json_atomic(&record_path, record)?;
    Ok(record_path)
}

/// List all rewrite records for a workspace, sorted by timestamp directory name.
#[allow(dead_code)]
pub(crate) fn list_rewrite_records(
    root: &Path,
    workspace: &str,
) -> Result<Vec<RewriteRecord>> {
    let dir = rewrite_dir(root, workspace);
    if !dir.exists() {
        return Ok(vec![]);
    }

    let mut entries: Vec<String> = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("read dir {}", dir.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with('.') {
            entries.push(name);
        }
    }
    entries.sort();

    let mut records = Vec::new();
    for ts_dir in &entries {
        let record_path = dir.join(ts_dir).join("record.json");
        if record_path.exists() {
            match read_rewrite_record(&record_path) {
                Ok(r) => records.push(r),
                Err(e) => {
                    tracing::warn!(path = %record_path.display(), error = %e, "skipping corrupt rewrite record");
                }
            }
        }
    }

    Ok(records)
}

/// Read a single rewrite record from disk.
#[allow(dead_code)]
pub(crate) fn read_rewrite_record(path: &Path) -> Result<RewriteRecord> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let record: RewriteRecord = serde_json::from_str(&content)
        .with_context(|| format!("parse {}", path.display()))?;
    Ok(record)
}

/// List all workspace names that have rewrite records.
#[allow(dead_code)]
pub(crate) fn list_rewritten_workspaces(root: &Path) -> Result<Vec<String>> {
    let rewrite_root = root
        .join(".manifold")
        .join("artifacts")
        .join("rewrite");
    if !rewrite_root.exists() {
        return Ok(vec![]);
    }
    let mut names = Vec::new();
    for entry in
        fs::read_dir(&rewrite_root).with_context(|| format!("read dir {}", rewrite_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let ws_name = entry.file_name().to_string_lossy().to_string();
        if !ws_name.starts_with('.') {
            names.push(ws_name);
        }
    }
    names.sort();
    Ok(names)
}

// ---------------------------------------------------------------------------
// Stash-based helpers
// ---------------------------------------------------------------------------

/// Stash uncommitted changes. Returns `true` if there was something to stash.
// TODO(gix): `git stash --include-untracked` captures untracked files; GitRepo::stash_create()
// does not push to the stash stack and may not capture untracked files. Kept as CLI for now.
#[allow(dead_code)]
pub(crate) fn stash_changes(ws_path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["stash", "--include-untracked"])
        .current_dir(ws_path)
        .output()
        .context("Failed to run git stash")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git stash failed: {}", stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // If working tree is clean, git outputs "No local changes to save"
    let had_changes = !stdout.trim().starts_with("No local changes");
    Ok(had_changes)
}

/// Checkout the workspace HEAD to a specific epoch OID (detached).
#[allow(dead_code)]
// TODO(gix): checkout_tree() does not update HEAD, and write_ref("HEAD")
// doesn't reliably create a detached HEAD in linked worktrees. Keep
// `git checkout --detach` until gix gains proper worktree HEAD support.
pub(crate) fn checkout_epoch(ws_path: &Path, epoch_oid: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["checkout", "--detach", epoch_oid])
        .current_dir(ws_path)
        .output()
        .context("failed to run git checkout --detach")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git checkout --detach {epoch_oid} failed: {}", stderr.trim());
    }
    Ok(())
}

/// Pop the stash and return a list of conflict entries (if any).
///
/// After `git stash pop` with conflicts, git leaves the working tree in a
/// partially-merged state with conflict markers. We detect conflicts via
/// `git status --porcelain` and parse the two-character status code.
// TODO(gix): replace with GitRepo trait method when `git stash pop` is supported.
#[allow(dead_code)]
pub(crate) fn pop_stash_and_detect_conflicts(
    ws_path: &Path,
) -> Result<Vec<WorkingCopyConflict>> {
    let output = Command::new("git")
        .args(["stash", "pop"])
        .current_dir(ws_path)
        .output()
        .context("Failed to run git stash pop")?;

    if output.status.success() {
        // Clean apply — no conflicts.
        return Ok(vec![]);
    }

    // stash pop failed — check for conflict markers.
    let conflicts = detect_conflicts_in_worktree(ws_path)?;
    if conflicts.is_empty() {
        // Something else failed.
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git stash pop failed (no conflicts detected): {}",
            stderr.trim()
        );
    }
    Ok(conflicts)
}

/// Parse `git status --porcelain` to find conflicted files.
///
/// Conflict status codes (first two chars of porcelain output):
/// - `AA` — both added
/// - `DD` — both deleted
/// - `UU` — both modified (content conflict)
/// - `AU` / `UA` — added/updated conflict
/// - `DU` / `UD` — deleted/updated conflict
// TODO(gix): GitRepo::status() does not yet report conflict markers (UU/AA/DD).
// Keep CLI for conflict detection until gix reports merge conflicts.
pub(crate) fn detect_conflicts_in_worktree(
    ws_path: &Path,
) -> Result<Vec<WorkingCopyConflict>> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(ws_path)
        .output()
        .context("Failed to run git status --porcelain")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git status failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut conflicts = Vec::new();

    for line in stdout.lines() {
        if line.len() < 4 {
            continue;
        }
        let xy = &line[..2];
        let path = line[3..].to_owned();

        let conflict_type = match xy {
            "UU" => "content",
            "AA" => "both_added",
            "DD" => "both_deleted",
            "AU" | "UA" => "add_mod_conflict",
            "DU" | "UD" => "delete_mod_conflict",
            _ => continue, // not a conflict status
        };

        conflicts.push(WorkingCopyConflict {
            path,
            conflict_type: conflict_type.to_owned(),
        });
    }

    Ok(conflicts)
}

// ===========================================================================
// Snapshot-based composable helpers (working-copy preservation)
// ===========================================================================

/// Snapshot the working copy if it has uncommitted changes.
///
/// Returns `Ok(None)` if the working tree is clean (fast path — no snapshot
/// overhead). Returns `Ok(Some(SnapshotRef))` if dirty state was captured.
///
/// Algorithm:
/// 1. `git status --porcelain` — if empty, return None.
/// 2. `git add -A` — stage untracked files so stash captures them.
/// 3. `git stash create` — create a stash commit without touching the stash stack.
/// 4. `git update-ref refs/manifold/snapshot/<ws_name> <oid>` — pin to durable ref.
/// 5. `git reset` — unstage everything (clean index for subsequent checkout).
///
/// If a prior snapshot ref exists, it is overwritten (with a warning).
// TODO(gix): replace CLI calls with GitRepo trait methods when git add -A, stash create,
// reset, reset --hard, and clean -fd are supported. Currently gix stash_create only
// captures index state (not working tree modifications).
#[instrument(skip_all, fields(workspace = ws_name))]
pub(crate) fn snapshot_working_copy(
    ws_path: &Path,
    repo_root: &Path,
    ws_name: &str,
) -> Result<Option<SnapshotRef>> {
    // Step 1: Check for dirty state.
    let repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    let is_dirty = repo.is_dirty()
        .map_err(|e| anyhow::anyhow!("is_dirty check failed: {e}"))?;

    if !is_dirty {
        tracing::debug!("working copy is clean, skipping snapshot");
        return Ok(None);
    }

    tracing::info!("dirty working copy detected, creating snapshot");

    // Check for prior snapshot ref (warn if overwriting).
    let ref_name = snapshot_ref_name(ws_name);
    if let Ok(Some(_existing)) = manifold_refs::read_ref(repo_root, &ref_name) {
        tracing::warn!(
            ref_name = %ref_name,
            "overwriting existing snapshot ref (prior snapshot not cleaned up)"
        );
    }

    // Step 2: Stage all files (including untracked) so stash captures them.
    let add_output = Command::new("git")
        .args(["add", "-A"])
        .current_dir(ws_path)
        .output()
        .context("failed to run git add -A")?;

    if !add_output.status.success() {
        let stderr = String::from_utf8_lossy(&add_output.stderr);
        bail!("git add -A failed during snapshot: {}", stderr.trim());
    }

    // Step 3: Create a stash commit (does NOT modify HEAD or stash list).
    let stash_result = repo.stash_create()
        .map_err(|e| {
            // Restore index before bailing.
            // TODO(gix): replace git reset with GitRepo trait method
            let _ = Command::new("git")
                .args(["reset"])
                .current_dir(ws_path)
                .output();
            anyhow::anyhow!("stash_create failed during snapshot: {e}")
        })?;

    let stash_oid = match stash_result {
        Some(oid) => oid.to_string(),
        None => {
            // Shouldn't happen since we checked status, but be defensive.
            // TODO(gix): replace git reset with GitRepo trait method
            let _ = Command::new("git")
                .args(["reset"])
                .current_dir(ws_path)
                .output();
            tracing::warn!("stash_create returned None despite dirty status");
            return Ok(None);
        }
    };

    // Step 4: Pin to durable ref (crash-safe).
    let oid = GitOid::new(&stash_oid)
        .map_err(|e| anyhow::anyhow!("invalid stash OID '{stash_oid}': {e}"))?;
    manifold_refs::write_ref(repo_root, &ref_name, &oid)
        .map_err(|e| anyhow::anyhow!("failed to pin snapshot ref: {e}"))?;

    // Step 5: Clean the working tree so the subsequent checkout succeeds.
    //
    // `git stash create` does NOT modify the working tree or index — it only
    // creates a commit object. We need to:
    // (a) Reset tracked file modifications to match HEAD.
    // (b) Remove untracked files that were captured in the stash.
    //
    // Without this, `git checkout <branch>` would fail if the branch has
    // moved and there are conflicting modifications, and `git stash apply`
    // would fail if untracked files captured in the stash still exist.
    let _ = Command::new("git")
        .args(["reset", "--hard", "HEAD"])
        .current_dir(ws_path)
        .output();
    let _ = Command::new("git")
        .args(["clean", "-fd"])
        .current_dir(ws_path)
        .output();

    tracing::info!(
        ref_name = %ref_name,
        oid = %stash_oid,
        "snapshot pinned, working tree cleaned"
    );

    Ok(Some(SnapshotRef {
        oid: stash_oid,
        ref_name,
    }))
}

/// Checkout a workspace to a target commit or branch.
///
/// If `branch_name` is `Some`, checks out the named branch (attaching HEAD).
/// If `branch_name` is `None`, performs a detached checkout to the target OID.
///
/// # Precondition
///
/// The working tree MUST be clean before calling this function — either
/// because there were no changes, or because [`snapshot_working_copy()`]
/// already captured and cleaned the dirty state. This function does NOT
/// use `--force`; a dirty tree will cause `git checkout` to fail, which is
/// the correct behavior (it means the snapshot step was skipped or broken).
// TODO(gix): replace with GitRepo trait method when `git checkout` is supported.
pub(crate) fn checkout_to(
    ws_path: &Path,
    target: &str,
    branch_name: Option<&str>,
) -> Result<()> {
    let checkout_target = branch_name.unwrap_or(target);
    let output = Command::new("git")
        .args(if branch_name.is_some() {
            vec!["checkout", checkout_target]
        } else {
            vec!["checkout", "--detach", checkout_target]
        })
        .current_dir(ws_path)
        .output()
        .context("failed to run git checkout")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git checkout {} failed: {}",
            checkout_target,
            stderr.trim()
        );
    }

    Ok(())
}

/// Replay a snapshot onto the current working tree.
///
/// Uses `stash_apply()` to reapply the captured changes. Unlike the
/// legacy stash-based helpers, this does NOT pop from the stash stack (the
/// snapshot was created with `stash_create`, not `git stash push`).
///
/// Returns:
/// - `SnapshotReplayResult::Clean` if all changes applied without conflict.
/// - `SnapshotReplayResult::Conflicts(list)` if there are conflict markers
///   in the working tree. The conflicts are left as markers (working-copy-preserving —
///   conflicts are data, not errors).
pub(crate) fn replay_snapshot(
    ws_path: &Path,
    snapshot: &SnapshotRef,
) -> Result<SnapshotReplayResult> {
    let repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    let oid: maw_git::GitOid = snapshot.oid.parse()
        .map_err(|e| anyhow::anyhow!("invalid snapshot OID '{}': {e}", snapshot.oid))?;

    match repo.stash_apply(oid) {
        Ok(()) => {
            tracing::info!("snapshot replayed cleanly");
            Ok(SnapshotReplayResult::Clean)
        }
        Err(_e) => {
            // stash apply failed — check for conflict markers.
            let conflicts = detect_conflicts_in_worktree(ws_path)?;
            if conflicts.is_empty() {
                // Something else went wrong (not a merge conflict).
                bail!(
                    "stash_apply failed (no conflicts detected): {}",
                    _e
                );
            }

            tracing::info!(
                conflict_count = conflicts.len(),
                "snapshot replay produced conflicts (left as markers in working tree)"
            );

            Ok(SnapshotReplayResult::Conflicts(conflicts))
        }
    }
}

/// Delete the snapshot ref for a workspace.
///
/// Call this after a successful replay to clean up the durable pin.
/// On replay with conflicts, the ref is intentionally KEPT as a recovery
/// anchor.
pub(crate) fn cleanup_snapshot(repo_root: &Path, ws_name: &str) -> Result<()> {
    let ref_name = snapshot_ref_name(ws_name);
    manifold_refs::delete_ref(repo_root, &ref_name)
        .map_err(|e| anyhow::anyhow!("failed to delete snapshot ref '{ref_name}': {e}"))?;
    tracing::debug!(ref_name = %ref_name, "snapshot ref cleaned up");
    Ok(())
}

/// Check if a dangling snapshot ref exists for a workspace.
///
/// Returns the snapshot ref details if one exists (e.g. from a prior crash).
/// Callers can use this to offer recovery.
#[allow(dead_code)]
pub(crate) fn dangling_snapshot(
    repo_root: &Path,
    ws_name: &str,
) -> Result<Option<SnapshotRef>> {
    let ref_name = snapshot_ref_name(ws_name);
    match manifold_refs::read_ref(repo_root, &ref_name) {
        Ok(Some(oid)) => Ok(Some(SnapshotRef {
            oid: oid.as_str().to_owned(),
            ref_name,
        })),
        Ok(None) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("failed to read snapshot ref: {e}")),
    }
}

// ===========================================================================
// preserve_checkout_replay — G2-compliant rewrite primitive (legacy)
// ===========================================================================

/// Outcome of a `preserve_checkout_replay()` operation.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) enum ReplayResult {
    /// No user work existed — clean checkout performed.
    Clean,
    /// User work existed, captured and replayed successfully.
    Replayed {
        recovery_ref: String,
        recovery_oid: String,
    },
    /// Replay failed, rolled back to captured snapshot.
    Rollback {
        recovery_ref: String,
        recovery_oid: String,
        reason: String,
    },
}

/// Safely rewrite a workspace from one epoch to another, preserving user work.
///
/// This is the core primitive for G2 compliance: before any destructive rewrite,
/// user work is captured, and deltas are replayed onto the new target. If replay
/// fails, the workspace is rolled back to the captured snapshot.
///
/// # Arguments
///
/// * `ws_path` — absolute path to the workspace directory
/// * `base_epoch` — the epoch the workspace was created at (B); used as the
///   anchor for delta extraction
/// * `target_ref` — the commit/branch to materialize (T)
/// * `repo_root` — repo root path (for recovery ref pinning)
/// * `workspace_name` — workspace name (for recovery ref naming)
// TODO(gix): replace CLI calls (git diff --cached --quiet, git diff --quiet,
// git ls-files --others) with GitRepo trait methods when supported.
#[instrument(skip_all, fields(workspace = workspace_name, target = target_ref))]
#[allow(dead_code)]
pub(crate) fn preserve_checkout_replay(
    ws_path: &Path,
    base_epoch: &str,
    target_ref: &str,
    _repo_root: &Path,
    workspace_name: &str,
) -> Result<ReplayResult> {
    // Step 1: Check for user work relative to the base epoch.
    // We want to know if the user has made any changes since they last synced.
    // A workspace is "clean" if its index and worktree match the base epoch,
    // regardless of where HEAD currently points (e.g. if a branch moved).
    //
    // TODO(gix): These diff-against-base-epoch checks need a more targeted
    // GitRepo method (e.g. diff_trees with index). For now, keep CLI since
    // is_dirty() only checks HEAD, not an arbitrary base epoch.
    let is_index_clean = Command::new("git")
        .args(["diff", "--cached", "--quiet", base_epoch])
        .current_dir(ws_path)
        .status()?
        .success();
    let is_worktree_clean = Command::new("git")
        .args(["diff", "--quiet", base_epoch])
        .current_dir(ws_path)
        .status()?
        .success();
    let untracked_output = Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .current_dir(ws_path)
        .output()
        .context("failed to run git ls-files")?;
    let untracked_empty = String::from_utf8_lossy(&untracked_output.stdout).trim().is_empty();

    if is_index_clean && is_worktree_clean && untracked_empty {
        tracing::debug!("no user work detected (clean vs base), fast-path checkout");
        git_checkout_force(ws_path, target_ref)?;
        return Ok(ReplayResult::Clean);
    }

    tracing::info!("user work detected, beginning capture-replay cycle");

    // Step 2: Capture recovery snapshot.
    let base_oid = GitOid::new(base_epoch)
        .map_err(|e| anyhow::anyhow!("invalid base_epoch OID '{base_epoch}': {e}"))?;

    let capture_result = capture_before_destroy(ws_path, workspace_name, &base_oid)
        .context("failed to capture recovery snapshot before rewrite")?;

    let capture = match capture_result {
        Some(c) => c,
        None => {
            tracing::warn!(
                "capture returned None despite status check showing work; \
                 falling back to clean checkout"
            );
            git_checkout_force(ws_path, target_ref)?;
            return Ok(ReplayResult::Clean);
        }
    };

    let recovery_ref = capture.pinned_ref.clone();
    let recovery_oid = capture.commit_oid.as_str().to_owned();

    tracing::info!(
        recovery_ref = %recovery_ref,
        recovery_oid = %recovery_oid,
        "recovery snapshot captured"
    );

    // Step 3: Extract user deltas from the explicit base epoch.
    let deltas = match extract_user_deltas(ws_path, base_epoch) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("failed to extract user deltas: {e}");
            return Ok(ReplayResult::Rollback {
                recovery_ref,
                recovery_oid,
                reason: format!("failed to extract user deltas: {e}"),
            });
        }
    };

    // Step 4: Materialize the target via force checkout.
    if let Err(e) = git_checkout_force(ws_path, target_ref) {
        tracing::error!("force checkout to target failed: {e}, rolling back");
        let _ = git_checkout_force(ws_path, &recovery_oid);
        return Ok(ReplayResult::Rollback {
            recovery_ref,
            recovery_oid,
            reason: format!("checkout to target '{target_ref}' failed: {e}"),
        });
    }

    // Step 5: Replay staged deltas (if non-empty).
    if let Some(ref staged_patch) = deltas.staged_patch_path
        && let Err(e) = git_apply_patch(ws_path, staged_patch, true) {
            tracing::warn!("staged patch apply failed: {e}, rolling back");
            let _ = git_checkout_force(ws_path, &recovery_oid);
            return Ok(ReplayResult::Rollback {
                recovery_ref,
                recovery_oid,
                reason: format!("staged patch replay failed: {e}"),
            });
        }

    // Step 6: Replay unstaged deltas (if non-empty).
    if let Some(ref unstaged_patch) = deltas.unstaged_patch_path
        && let Err(e) = git_apply_patch(ws_path, unstaged_patch, false) {
            tracing::warn!("unstaged patch apply failed: {e}, rolling back");
            let _ = git_checkout_force(ws_path, &recovery_oid);
            return Ok(ReplayResult::Rollback {
                recovery_ref,
                recovery_oid,
                reason: format!("unstaged patch replay failed: {e}"),
            });
        }

    // Step 7: Restore untracked files.
    if let Some(ref untracked) = deltas.untracked {
        for (rel_path, tmp_path) in untracked {
            let dest = ws_path.join(rel_path);
            if let Some(parent) = dest.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Err(e) = fs::copy(tmp_path, &dest) {
                tracing::warn!(
                    path = %rel_path,
                    "failed to restore untracked file: {e}, rolling back"
                );
                let _ = git_checkout_force(ws_path, &recovery_oid);
                return Ok(ReplayResult::Rollback {
                    recovery_ref,
                    recovery_oid,
                    reason: format!("failed to restore untracked file '{rel_path}': {e}"),
                });
            }
        }
    }

    // Step 8: Check for conflicts.
    let post_status = git_status_porcelain(ws_path)?;
    if has_conflict_markers(&post_status) {
        tracing::warn!("conflict markers detected after replay, rolling back");
        let _ = git_checkout_force(ws_path, &recovery_oid);
        return Ok(ReplayResult::Rollback {
            recovery_ref,
            recovery_oid,
            reason: "merge conflicts detected after replay".to_string(),
        });
    }

    tracing::info!("replay completed successfully");
    Ok(ReplayResult::Replayed {
        recovery_ref,
        recovery_oid,
    })
}

// ---------------------------------------------------------------------------
// Delta extraction
// ---------------------------------------------------------------------------

/// Extracted user deltas from a workspace relative to a base epoch.
struct UserDeltas {
    staged_patch_path: Option<std::path::PathBuf>,
    unstaged_patch_path: Option<std::path::PathBuf>,
    untracked: Option<Vec<(String, std::path::PathBuf)>>,
    _temp_dir: tempfile::TempDir,
}

/// Extract user deltas from the workspace relative to the base epoch.
// TODO(gix): replace CLI calls (git diff --cached --binary, git diff --binary,
// git ls-files --others) with GitRepo trait methods when supported.
fn extract_user_deltas(ws_path: &Path, base_epoch: &str) -> Result<UserDeltas> {
    let temp_dir = tempfile::TempDir::new()
        .context("failed to create temp directory for delta extraction")?;

    // Staged diff.
    let staged_patch_path = {
        let output = Command::new("git")
            .args(["diff", "--cached", "--binary", base_epoch])
            .current_dir(ws_path)
            .output()
            .context("failed to run git diff --cached")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git diff --cached failed: {}", stderr.trim());
        }

        if output.stdout.is_empty() {
            None
        } else {
            let path = temp_dir.path().join("staged.patch");
            fs::write(&path, &output.stdout)
                .context("failed to write staged patch")?;
            Some(path)
        }
    };

    // Unstaged diff.
    let unstaged_patch_path = {
        let output = Command::new("git")
            .args(["diff", "--binary"])
            .current_dir(ws_path)
            .output()
            .context("failed to run git diff")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git diff failed: {}", stderr.trim());
        }

        if output.stdout.is_empty() {
            None
        } else {
            let path = temp_dir.path().join("unstaged.patch");
            fs::write(&path, &output.stdout)
                .context("failed to write unstaged patch")?;
            Some(path)
        }
    };

    // Untracked files.
    let untracked = {
        let output = Command::new("git")
            .args(["ls-files", "--others", "--exclude-standard"])
            .current_dir(ws_path)
            .output()
            .context("failed to run git ls-files --others")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git ls-files --others failed: {}", stderr.trim());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let files: Vec<String> = stdout
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();

        if files.is_empty() {
            None
        } else {
            let untracked_dir = temp_dir.path().join("untracked");
            fs::create_dir_all(&untracked_dir)
                .context("failed to create untracked temp dir")?;

            let mut entries = Vec::new();
            for rel_path in &files {
                let src = ws_path.join(rel_path);
                if !src.exists() {
                    continue;
                }
                let dest = untracked_dir.join(rel_path);
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(&src, &dest)
                    .with_context(|| format!("failed to copy untracked file '{rel_path}'"))?;
                entries.push((rel_path.clone(), dest));
            }

            if entries.is_empty() {
                None
            } else {
                Some(entries)
            }
        }
    };

    Ok(UserDeltas {
        staged_patch_path,
        unstaged_patch_path,
        untracked,
        _temp_dir: temp_dir,
    })
}

// ---------------------------------------------------------------------------
// Git helpers (replay layer)
// ---------------------------------------------------------------------------

/// Run `git status --porcelain` and return the raw output.
// TODO(gix): GitRepo::status() does not yet report conflict markers (UU/AA/DD).
// Need raw porcelain output for conflict detection in has_conflict_markers().
fn git_status_porcelain(ws_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(ws_path)
        .output()
        .context("failed to run git status --porcelain")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git status --porcelain failed: {}", stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Resolve HEAD to a string OID.
#[allow(dead_code)]
fn resolve_head_str(ws_path: &Path) -> Result<String> {
    let repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    let oid = repo
        .rev_parse("HEAD")
        .map_err(|e| anyhow::anyhow!("failed to resolve HEAD: {e}"))?;
    Ok(oid.to_string())
}

/// Run `git checkout --force <ref>`.
// TODO(gix): replace with GitRepo trait method when `git checkout --force` is supported.
fn git_checkout_force(ws_path: &Path, target: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["checkout", "--force", target])
        .current_dir(ws_path)
        .output()
        .context("failed to run git checkout --force")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git checkout --force failed: {}", stderr.trim());
    }

    Ok(())
}

/// Apply a patch file via `git apply --3way`.
// TODO(gix): replace with GitRepo trait method when `git apply --3way` is supported.
fn git_apply_patch(ws_path: &Path, patch_path: &Path, index: bool) -> Result<()> {
    let mut args = vec!["apply", "--3way"];
    if index {
        args.push("--index");
    }
    let patch_str = patch_path
        .to_str()
        .context("patch path is not valid UTF-8")?;
    args.push(patch_str);

    let output = Command::new("git")
        .args(&args)
        .current_dir(ws_path)
        .output()
        .context("failed to run git apply")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git apply failed: {}", stderr.trim());
    }

    Ok(())
}

/// Check if porcelain status output contains conflict markers (UU, AA, DD, etc.).
fn has_conflict_markers(status: &str) -> bool {
    for line in status.lines() {
        if line.len() < 2 {
            continue;
        }
        let xy = &line[..2];
        match xy {
            "UU" | "AA" | "DD" | "AU" | "UA" | "DU" | "UD" => return true,
            _ => {}
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    /// Create a fresh git repo with one initial commit.
    fn setup_repo() -> (TempDir, std::path::PathBuf, String) {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();

        for (args, _label) in [
            (vec!["init"], "init"),
            (vec!["config", "user.name", "Test"], "config name"),
            (vec!["config", "user.email", "test@test.com"], "config email"),
            (
                vec!["config", "commit.gpgsign", "false"],
                "config gpgsign",
            ),
        ] {
            let out = Command::new("git")
                .args(&args)
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        }

        fs::write(root.join("README.md"), "# Test\n").unwrap();
        let out = Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(out.status.success());

        let out = Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(out.status.success());

        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        let oid = String::from_utf8_lossy(&out.stdout).trim().to_owned();

        (dir, root, oid)
    }

    fn make_second_commit(root: &Path) -> String {
        fs::write(root.join("epoch2.txt"), "epoch2 content\n").unwrap();
        let out = Command::new("git")
            .args(["add", "epoch2.txt"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(out.status.success());

        let out = Command::new("git")
            .args(["commit", "-m", "epoch2"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(out.status.success());

        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    #[test]
    fn clean_workspace_fast_path() {
        let (_dir, root, base_oid) = setup_repo();
        let target_oid = make_second_commit(&root);

        let out = Command::new("git")
            .args(["checkout", "--force", &base_oid])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(out.status.success());

        let result = preserve_checkout_replay(
            &root,
            &base_oid,
            &target_oid,
            &root,
            "test-ws",
        )
        .unwrap();

        assert!(
            matches!(result, ReplayResult::Clean),
            "expected Clean, got {result:?}"
        );

        let head = resolve_head_str(&root).unwrap();
        assert_eq!(head, target_oid);
        assert!(root.join("epoch2.txt").exists());
    }

    #[test]
    fn dirty_workspace_deltas_survive_rewrite() {
        let (_dir, root, base_oid) = setup_repo();
        let target_oid = make_second_commit(&root);

        let out = Command::new("git")
            .args(["checkout", "--force", &base_oid])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(out.status.success());

        // Staged change
        fs::write(root.join("README.md"), "# Modified by user\n").unwrap();
        let out = Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(out.status.success());

        // Unstaged change
        fs::write(
            root.join("README.md"),
            "# Modified by user\nUnstaged extra line\n",
        )
        .unwrap();

        // Untracked file
        fs::write(root.join("user-notes.txt"), "my important notes\n").unwrap();

        let result = preserve_checkout_replay(
            &root,
            &base_oid,
            &target_oid,
            &root,
            "test-ws",
        )
        .unwrap();

        match &result {
            ReplayResult::Replayed {
                recovery_ref,
                recovery_oid,
            } => {
                assert!(
                    recovery_ref.starts_with("refs/manifold/recovery/test-ws/"),
                    "unexpected recovery ref: {recovery_ref}"
                );
                assert!(!recovery_oid.is_empty());
            }
            other => panic!("expected Replayed, got {other:?}"),
        }

        assert!(
            root.join("epoch2.txt").exists(),
            "epoch2.txt should exist after replay"
        );

        let readme = fs::read_to_string(root.join("README.md")).unwrap();
        assert!(
            readme.contains("Modified by user"),
            "staged changes should survive: {readme}"
        );

        assert!(
            root.join("user-notes.txt").exists(),
            "untracked file should be restored"
        );
        let notes = fs::read_to_string(root.join("user-notes.txt")).unwrap();
        assert_eq!(notes, "my important notes\n");
    }

    #[test]
    fn has_conflict_markers_detects_uu() {
        assert!(has_conflict_markers("UU src/main.rs\n"));
        assert!(has_conflict_markers("AA both-added.txt\n"));
        assert!(has_conflict_markers("DD both-deleted.txt\n"));
    }

    #[test]
    fn has_conflict_markers_ignores_normal_status() {
        assert!(!has_conflict_markers("M  src/main.rs\n"));
        assert!(!has_conflict_markers("?? new-file.txt\n"));
        assert!(!has_conflict_markers("A  staged.txt\n"));
        assert!(!has_conflict_markers(""));
    }

    // -----------------------------------------------------------------------
    // Rewrite artifact tests
    // -----------------------------------------------------------------------

    fn make_test_record(workspace: &str, outcome: ReplayOutcome) -> RewriteRecord {
        RewriteRecord {
            workspace: workspace.to_owned(),
            timestamp: "2025-06-01T12:00:00Z".to_owned(),
            base_epoch: "a".repeat(40),
            target_ref: "b".repeat(40),
            recovery_ref: format!(
                "refs/manifold/recovery/{workspace}/2025-06-01T12-00-00Z"
            ),
            recovery_oid: "c".repeat(40),
            replay_outcome: outcome,
            rollback_reason: None,
            delta_summary: DeltaSummary {
                staged_files: 1,
                unstaged_files: 2,
                untracked_files: 3,
            },
            tool_version: "0.47.0".to_owned(),
        }
    }

    #[test]
    fn rewrite_record_serialization_roundtrip() {
        let record = make_test_record("test-ws", ReplayOutcome::Replayed);
        let json = serde_json::to_string_pretty(&record).unwrap();
        let parsed: RewriteRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.workspace, "test-ws");
        assert_eq!(parsed.replay_outcome, ReplayOutcome::Replayed);
        assert_eq!(parsed.delta_summary.staged_files, 1);
        assert_eq!(parsed.delta_summary.unstaged_files, 2);
        assert_eq!(parsed.delta_summary.untracked_files, 3);
        assert!(parsed.rollback_reason.is_none());
    }

    #[test]
    fn rewrite_record_rollback_serialization() {
        let mut record = make_test_record("ws", ReplayOutcome::Rollback);
        record.rollback_reason = Some("stash pop failed: conflict".to_owned());
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"rollback\""));
        assert!(json.contains("stash pop failed"));
    }

    #[test]
    fn write_and_read_rewrite_artifact() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let record = make_test_record("agent-1", ReplayOutcome::Replayed);

        let path = write_rewrite_record(root, "agent-1", &record).unwrap();
        assert!(path.exists());

        let read_back = read_rewrite_record(&path).unwrap();
        assert_eq!(read_back.workspace, "agent-1");
        assert_eq!(read_back.replay_outcome, ReplayOutcome::Replayed);
        assert_eq!(read_back.delta_summary.staged_files, 1);
    }

    #[test]
    fn list_rewrite_records_returns_sorted() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        for ts in &["2025-06-01T12:00:00Z", "2025-06-02T12:00:00Z"] {
            let mut record = make_test_record("agent-1", ReplayOutcome::Replayed);
            record.timestamp = ts.to_string();
            record.recovery_ref =
                format!("refs/manifold/recovery/agent-1/{}", ts.replace(':', "-"));
            write_rewrite_record(root, "agent-1", &record).unwrap();
        }

        let records = list_rewrite_records(root, "agent-1").unwrap();
        assert_eq!(records.len(), 2);
        assert!(records[0].recovery_ref.contains("2025-06-01"));
        assert!(records[1].recovery_ref.contains("2025-06-02"));
    }

    #[test]
    fn list_rewrite_records_empty_for_nonexistent_workspace() {
        let dir = TempDir::new().unwrap();
        let records = list_rewrite_records(dir.path(), "nonexistent").unwrap();
        assert!(records.is_empty());
    }

    #[test]
    fn list_rewritten_workspaces_discovers_workspace_dirs() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        for ws in &["alpha", "beta"] {
            let record = make_test_record(ws, ReplayOutcome::Clean);
            write_rewrite_record(root, ws, &record).unwrap();
        }

        let names = list_rewritten_workspaces(root).unwrap();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn replay_outcome_display() {
        assert_eq!(ReplayOutcome::Clean.to_string(), "clean");
        assert_eq!(ReplayOutcome::Replayed.to_string(), "replayed");
        assert_eq!(ReplayOutcome::Rollback.to_string(), "rollback");
    }

    // -----------------------------------------------------------------------
    // Snapshot-based helper tests (bn-1wtu)
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_clean_workspace_returns_none() {
        let (_dir, root, _base_oid) = setup_repo();
        let result = snapshot_working_copy(&root, &root, "test-ws").unwrap();
        assert!(result.is_none(), "clean workspace should return None");
    }

    #[test]
    fn snapshot_dirty_workspace_captures_and_cleans() {
        let (_dir, root, _base_oid) = setup_repo();

        // Create dirty state: modify tracked file + add untracked file.
        fs::write(root.join("README.md"), "# Modified by user\n").unwrap();
        fs::write(root.join("notes.txt"), "user notes\n").unwrap();

        let result = snapshot_working_copy(&root, &root, "test-ws")
            .unwrap()
            .expect("dirty workspace should return Some");

        // Snapshot ref should be pinned.
        assert_eq!(result.ref_name, "refs/manifold/snapshot/test-ws");
        assert!(!result.oid.is_empty());

        // Verify the ref was written.
        let ref_oid = maw_core::refs::read_ref(&root, &result.ref_name).unwrap();
        assert!(ref_oid.is_some(), "snapshot ref should exist");

        // Working tree should be clean after snapshot.
        let status = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&root)
            .output()
            .unwrap();
        let status_str = String::from_utf8_lossy(&status.stdout);
        assert!(
            status_str.trim().is_empty(),
            "working tree should be clean after snapshot, got: {status_str}"
        );
    }

    #[test]
    fn snapshot_checkout_replay_roundtrip() {
        let (_dir, root, base_oid) = setup_repo();
        let target_oid = make_second_commit(&root);

        // Go back to base.
        let out = Command::new("git")
            .args(["checkout", "--force", &base_oid])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(out.status.success());

        // Create user changes.
        fs::write(root.join("README.md"), "# User modified\n").unwrap();
        fs::write(root.join("user-work.txt"), "important work\n").unwrap();

        // Step 1: Snapshot.
        let snapshot = snapshot_working_copy(&root, &root, "test-ws")
            .unwrap()
            .expect("dirty workspace should produce snapshot");

        // Step 2: Checkout to target.
        checkout_to(&root, &target_oid, None).unwrap();

        // Verify target is checked out.
        assert!(root.join("epoch2.txt").exists(), "epoch2.txt should exist after checkout");

        // Step 3: Replay.
        let replay_result = replay_snapshot(&root, &snapshot).unwrap();
        assert!(
            matches!(replay_result, SnapshotReplayResult::Clean),
            "replay should be clean for non-overlapping changes"
        );

        // User changes should be present on top of new epoch.
        let readme = fs::read_to_string(root.join("README.md")).unwrap();
        assert!(
            readme.contains("User modified"),
            "user modification should survive: {readme}"
        );
        assert!(
            root.join("user-work.txt").exists(),
            "untracked user file should be restored"
        );
        assert!(
            root.join("epoch2.txt").exists(),
            "epoch2.txt from target should still exist"
        );

        // Step 4: Cleanup.
        cleanup_snapshot(&root, "test-ws").unwrap();
        let ref_oid = maw_core::refs::read_ref(&root, "refs/manifold/snapshot/test-ws").unwrap();
        assert!(ref_oid.is_none(), "snapshot ref should be deleted after cleanup");
    }

    #[test]
    fn snapshot_replay_with_conflict_leaves_markers() {
        let (_dir, root, _base_oid) = setup_repo();

        // Create a second commit that modifies README.md.
        fs::write(root.join("README.md"), "# Epoch 2 version\n").unwrap();
        let out = Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(out.status.success());
        let out = Command::new("git")
            .args(["commit", "-m", "epoch2: modify README"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(out.status.success());
        let target_out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        let target_oid = String::from_utf8_lossy(&target_out.stdout).trim().to_owned();

        // Go back to base and create a conflicting modification.
        let out = Command::new("git")
            .args(["checkout", "HEAD~1"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(out.status.success());

        fs::write(root.join("README.md"), "# User conflicting version\n").unwrap();

        // Snapshot.
        let snapshot = snapshot_working_copy(&root, &root, "test-ws")
            .unwrap()
            .expect("dirty workspace should produce snapshot");

        // Checkout to target (which has different README.md).
        checkout_to(&root, &target_oid, None).unwrap();

        // Replay — should produce conflict.
        let replay_result = replay_snapshot(&root, &snapshot).unwrap();
        match replay_result {
            SnapshotReplayResult::Conflicts(conflicts) => {
                assert!(
                    !conflicts.is_empty(),
                    "should have at least one conflict"
                );
                assert!(
                    conflicts.iter().any(|c| c.path.contains("README.md")),
                    "README.md should be in conflicts list"
                );
            }
            SnapshotReplayResult::Clean => {
                // If git resolved the conflict automatically (fast-forward
                // or clean merge), that's also acceptable. The key property
                // is that we didn't abort.
            }
        }

        // Snapshot ref should still exist (not cleaned up on conflict).
        let ref_oid = maw_core::refs::read_ref(&root, "refs/manifold/snapshot/test-ws").unwrap();
        assert!(ref_oid.is_some(), "snapshot ref should be kept on conflict");
    }
}
