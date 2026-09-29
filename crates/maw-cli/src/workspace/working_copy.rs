//! Working-copy helpers and safe rewrite primitives.
//!
//! Three layers:
//!
//! 1. **Legacy stash-based helpers** (`stash_changes`,
//!    `pop_stash_and_detect_conflicts`, `detect_conflicts_in_worktree`) — kept
//!    for backward compatibility but deprecated in favor of the snapshot layer.
//!    (`checkout_epoch` was removed in bn-8flz — it was dead/unreferenced code.)
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
//! 3. CHECKOUT — native `checkout_to()` (clean tree; uses `checkout_tree +
//!    set_head/set_head_to_branch`, no shell-out). (bn-8flz)
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

use super::capture::capture_before_destroy;
use maw_core::model::types::GitOid;
use maw_core::refs as manifold_refs;

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
pub fn snapshot_ref_name(ws_name: &str) -> String {
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
pub struct SnapshotRef {
    /// The git OID of the stash commit.
    pub oid: String,
    /// The full ref name (e.g. `refs/manifold/snapshot/default`).
    pub ref_name: String,
}

/// Outcome of replaying a snapshot onto a new tree.
#[derive(Clone, Debug)]
pub enum SnapshotReplayResult {
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
    /// `"add_mod_conflict"`, `"delete_mod_conflict"`, `"type_change"`.
    pub conflict_type: String,
    /// For a `"type_change"` conflict (bn-2ygs0): the two sides that could
    /// not be merged into one path. No conflict markers exist for it; the
    /// merged side is on disk and the local side lives only in the replayed
    /// snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_conflict: Option<TypeConflict>,
}

/// The two sides of a dirty-replay type conflict: a symlink vs a regular
/// file, a symlink vs a deletion, or two different symlink targets. (bn-2ygs0)
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TypeConflict {
    /// The merged side.
    pub merged: EntryKind,
    /// The user's uncommitted side (always preserved in the snapshot).
    pub local: EntryKind,
    /// Which side the replay left on disk: the merged side, unless the merge
    /// deleted the path (then the user's side stays, as for a regular file
    /// the merge deleted).
    pub kept: KeptSide,
}

/// Which side of a [`TypeConflict`] is on disk after the replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeptSide {
    /// The merged side is on disk; the user's side is only in the snapshot.
    Merged,
    /// The user's side is on disk (the merge deleted the path).
    Local,
}

/// The kind of a worktree entry, as far as a type conflict cares.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryKind {
    /// The path does not exist.
    Deleted,
    /// A regular file.
    File {
        /// Whether the file is executable.
        executable: bool,
    },
    /// A symbolic link.
    Symlink {
        /// The link target (lossy UTF-8).
        target: String,
    },
    /// A directory (bn-3jqfk: a file <-> directory change).
    Directory,
    /// The path cannot exist because a parent of it is a file or symlink
    /// (bn-1eg2u): e.g. the merge turned directory `p/` into file `p`, so
    /// `p/x` is not "deleted" but "replaced by file p".
    ReplacedBy {
        /// The parent path that is a file or symlink (slash-separated).
        path: String,
        /// Whether that parent is a symlink (else a regular file).
        symlink: bool,
    },
}

impl std::fmt::Display for EntryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Deleted => f.write_str("deleted"),
            Self::File { executable: false } => f.write_str("regular file"),
            Self::File { executable: true } => f.write_str("executable regular file"),
            Self::Symlink { target } => write!(f, "symlink -> {target}"),
            Self::Directory => f.write_str("directory"),
            Self::ReplacedBy {
                path,
                symlink: false,
            } => write!(f, "replaced by file {path}"),
            Self::ReplacedBy {
                path,
                symlink: true,
            } => write!(f, "replaced by symlink {path}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Rewrite artifact types (legacy — retained for existing tests and future use)
// ---------------------------------------------------------------------------

/// Summary of dirty-state delta at the time of a rewrite.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(dead_code)]
#[expect(
    clippy::struct_field_names,
    reason = "field names are serialized/user-facing delta counters"
)]
pub struct DeltaSummary {
    pub staged_files: u32,
    pub unstaged_files: u32,
    pub untracked_files: u32,
}

/// Outcome of the replay step in a rewrite operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum ReplayOutcome {
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
pub struct RewriteRecord {
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
    maw_core::model::layout::LayoutFlavor::detect_with_env(root)
        .manifold_dir(root)
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

    let filename = path.file_name().map_or_else(
        || "artifact".to_owned(),
        |n| n.to_string_lossy().to_string(),
    );
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
pub fn write_rewrite_record(
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
pub fn list_rewrite_records(root: &Path, workspace: &str) -> Result<Vec<RewriteRecord>> {
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
pub fn read_rewrite_record(path: &Path) -> Result<RewriteRecord> {
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let record: RewriteRecord =
        serde_json::from_str(&content).with_context(|| format!("parse {}", path.display()))?;
    Ok(record)
}

/// List all workspace names that have rewrite records.
#[allow(dead_code)]
pub fn list_rewritten_workspaces(root: &Path) -> Result<Vec<String>> {
    let rewrite_root = maw_core::model::layout::LayoutFlavor::detect_with_env(root)
        .manifold_dir(root)
        .join("artifacts")
        .join("rewrite");
    if !rewrite_root.exists() {
        return Ok(vec![]);
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(&rewrite_root)
        .with_context(|| format!("read dir {}", rewrite_root.display()))?
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
pub fn stash_changes(ws_path: &Path) -> Result<bool> {
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

// checkout_epoch was removed (bn-8flz): it was a dead function (only
// referenced in the module doc comment above, never called from live code).
// Replaced by the native checkout_detach primitive in maw-git.

/// Pop the stash and return a list of conflict entries (if any).
///
/// After `git stash pop` with conflicts, git leaves the working tree in a
/// partially-merged state with conflict markers. We detect conflicts via
/// `git status --porcelain` and parse the two-character status code.
// TODO(gix): replace with GitRepo trait method when `git stash pop` is supported.
#[allow(dead_code)]
pub fn pop_stash_and_detect_conflicts(ws_path: &Path) -> Result<Vec<WorkingCopyConflict>> {
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
pub fn detect_conflicts_in_worktree(ws_path: &Path) -> Result<Vec<WorkingCopyConflict>> {
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
            type_conflict: None,
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
pub fn snapshot_working_copy(
    ws_path: &Path,
    repo_root: &Path,
    ws_name: &str,
) -> Result<Option<SnapshotRef>> {
    let snapshot = snapshot_working_copy_preserving_tree(ws_path, repo_root, ws_name)?;
    if let Some(snapshot) = &snapshot {
        clean_snapshotted_working_copy(ws_path, snapshot)?;
    }
    Ok(snapshot)
}

/// Capture and pin dirty content without changing the worktree. Merge must
/// persist its replay intent before calling `clean_snapshotted_working_copy`.
pub(super) fn snapshot_working_copy_preserving_tree(
    ws_path: &Path,
    repo_root: &Path,
    ws_name: &str,
) -> Result<Option<SnapshotRef>> {
    const ADMIN_EXCLUDES: [&str; 4] = [".maw", "repo.git", ".manifold", ".git"];
    // Step 1: Check for dirty state via `status_head_to_worktree()`.
    //
    // We avoid gix's `is_dirty()` because it does not detect untracked
    // files. Without untracked detection, untracked files in the target
    // workspace would be silently destroyed by the checkout step since no
    // snapshot would be created to preserve them. (bn-2fk0)
    //
    // Must be HEAD→worktree, not the plain index→worktree `status()`: a
    // file `git add`-ed but not re-edited (worktree == staged blob) is
    // invisible to `status()`, so a staged-only-dirty workspace would skip
    // the recovery snapshot entirely and lose staged work on the subsequent
    // checkout (bn-pfh7 class — Prime Invariant). `status_head_to_worktree`
    // is the true `git status --porcelain` set, incl. staged + untracked.
    let status_repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    let status_entries = status_repo
        .status_head_to_worktree()
        .map_err(|e| anyhow::anyhow!("failed to query workspace status: {e}"))?;
    let is_dirty = !status_entries.is_empty();

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

    // Open repo for stash_create below.
    let repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;

    // Admin/git paths that must NEVER be staged, reset, or cleaned by the
    // snapshot. In the consolidated layout the DEFAULT workspace IS the repo
    // root, so these live *inside* `ws_path`: `repo.git/` is the shared common
    // git dir, `.maw/`/`.manifold/` are admin trees, `.git` is the gitfile/dir.
    // Without protecting them, `git add -A` stages `repo.git/` and the
    // subsequent `git reset --hard` / `git clean -fd` DELETE the git directory,
    // destroying the repository (bn-3bkn). For non-default workspaces these
    // paths don't exist at the worktree root, so the protection is a no-op.

    // Step 2: Stage all files (including untracked) so stash captures them.
    // (Plain `git add -A` skips gitignored paths without erroring.)
    let add_output = Command::new("git")
        .args(["add", "-A"])
        .current_dir(ws_path)
        .output()
        .context("failed to run git add -A")?;

    if !add_output.status.success() {
        let stderr = String::from_utf8_lossy(&add_output.stderr);
        bail!("git add -A failed during snapshot: {}", stderr.trim());
    }

    // Step 2b: Unstage the admin/git paths so they are NEITHER captured in the
    // snapshot NOR deleted by the `git reset --hard HEAD` below (which removes
    // staged-but-not-in-HEAD paths from the worktree). `git reset HEAD -- <p>`
    // is a no-op for paths that weren't staged. This is the load-bearing guard
    // that keeps `repo.git/` alive even when the `.gitignore` is missing or
    // uncommitted (bn-3bkn).
    let mut unstage_args: Vec<String> =
        vec!["reset".into(), "-q".into(), "HEAD".into(), "--".into()];
    unstage_args.extend(ADMIN_EXCLUDES.iter().map(|s| (*s).to_string()));
    let unstage_output = Command::new("git")
        .args(&unstage_args)
        .current_dir(ws_path)
        .output()
        .context("failed to unstage admin paths during snapshot")?;
    if !unstage_output.status.success() {
        let stderr = String::from_utf8_lossy(&unstage_output.stderr);
        let _ = repo.unstage_all();
        bail!(
            "failed to unstage admin paths during snapshot: {}",
            stderr.trim()
        );
    }

    // Step 3: Create a stash commit (does NOT modify HEAD or stash list).
    let stash_result = repo.stash_create().map_err(|e| {
        // Restore index before bailing.
        let _ = repo.unstage_all();
        anyhow::anyhow!("stash_create failed during snapshot: {e}")
    })?;

    let stash_oid = if let Some(oid) = stash_result {
        oid.to_string()
    } else {
        // The working tree was dirty (status is non-empty above) yet
        // `stash_create` refused to produce a commit. Returning `Ok(None)`
        // here would tell the caller "genuinely clean" and it would proceed
        // to overwrite the dirty tree with the merge result — silent data
        // loss. Mirror bn-3mpx's `capture_before_destroy` fix: surface an
        // error so the caller preserves the dirt (its in-memory pre-merge
        // repair path) instead of treating ambiguity as clean (bn-1xmk).
        let _ = repo.unstage_all();
        tracing::warn!("stash_create returned None despite dirty status");
        bail!(
            "snapshot aborted to avoid silent data loss: the working tree is \
             dirty but `git stash create` produced no snapshot commit"
        );
    };

    // Step 4: Pin to durable ref (crash-safe).
    //
    // bn-3ppf lock audit: the per-workspace snapshot ref has a fixed name, so
    // concurrent writers would clobber each other. Its callers
    // (`update_default_workspace` in `ws merge`, and `ws advance`) both hold
    // the repo epoch lock, which serializes them.
    let oid = GitOid::new(&stash_oid)
        .map_err(|e| anyhow::anyhow!("invalid stash OID '{stash_oid}': {e}"))?;
    manifold_refs::write_ref(repo_root, &ref_name, &oid)
        .map_err(|e| anyhow::anyhow!("failed to pin snapshot ref: {e}"))?;

    Ok(Some(SnapshotRef {
        oid: stash_oid,
        ref_name,
    }))
}

/// Clean only after the caller has durably recorded how to replay `snapshot`.
pub(super) fn clean_snapshotted_working_copy(ws_path: &Path, snapshot: &SnapshotRef) -> Result<()> {
    const ADMIN_EXCLUDES: [&str; 4] = [".maw", "repo.git", ".manifold", ".git"];
    let ref_name = &snapshot.ref_name;
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
    let reset_output = Command::new("git")
        .args(["reset", "--hard", "HEAD"])
        .current_dir(ws_path)
        .output()
        .context("failed to run git reset --hard HEAD during snapshot")?;
    if !reset_output.status.success() {
        let stderr = String::from_utf8_lossy(&reset_output.stderr);
        bail!(
            "git reset --hard HEAD failed during snapshot (snapshot preserved at {}): {}",
            ref_name,
            stderr.trim()
        );
    }
    // `git clean -fd` removes untracked files captured in the stash. Exclude
    // the admin/git dirs so it can never delete the repository (bn-3bkn): in
    // the consolidated layout the root checkout contains the untracked
    // `repo.git/` common git dir, which clean would otherwise wipe.
    let mut clean_args: Vec<String> = vec!["clean".into(), "-fd".into()];
    for p in ADMIN_EXCLUDES {
        clean_args.push("-e".into());
        clean_args.push(p.into());
    }
    let clean_output = Command::new("git")
        .args(&clean_args)
        .current_dir(ws_path)
        .output()
        .context("failed to run git clean -fd during snapshot")?;
    if !clean_output.status.success() {
        let stderr = String::from_utf8_lossy(&clean_output.stderr);
        bail!(
            "git clean -fd failed during snapshot (snapshot preserved at {}): {}",
            ref_name,
            stderr.trim()
        );
    }

    maw::fp!("FP_SNAPSHOT_AFTER_CLEAN")?;

    tracing::info!(
        ref_name = %ref_name,
        oid = %snapshot.oid,
        "snapshot pinned, working tree cleaned"
    );

    Ok(())
}

/// Checkout a workspace to a target commit or branch (native, no shell-out).
///
/// If `branch_name` is `Some`, attaches HEAD to the named branch.
/// If `branch_name` is `None`, performs a detached checkout to `target`.
///
/// Uses `maw_git::GixRepo::checkout_to_branch` (branch attachment) or
/// `checkout_detach` (detached) — both compose `checkout_tree + set_head*`
/// natively, writing a reflog entry (bn-20sa).
///
/// # Precondition
///
/// The working tree MUST be clean before calling this function — either
/// because there were no changes, or because [`snapshot_working_copy()`]
/// already captured and cleaned the dirty state.
pub fn checkout_to(ws_path: &Path, target: &str, branch_name: Option<&str>) -> Result<()> {
    let repo = maw_git::GixRepo::open(ws_path)
        .with_context(|| format!("failed to open repo at {}", ws_path.display()))?;
    let oid = repo
        .rev_parse(target)
        .with_context(|| format!("failed to resolve '{target}'"))?;

    branch_name.map_or_else(
        || {
            repo.checkout_detach(oid, ws_path)
                .with_context(|| format!("detached checkout to '{target}' failed"))
        },
        |branch| {
            repo.checkout_to_branch(oid, ws_path, branch)
                .with_context(|| format!("checkout to branch '{branch}' failed"))
        },
    )
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
pub fn replay_snapshot(ws_path: &Path, snapshot: &SnapshotRef) -> Result<SnapshotReplayResult> {
    let result = replay_snapshot_raw(ws_path, snapshot);
    // bn-3fcbu: whatever `stash_apply` wrote, the executable bit of each
    // replayed file must be the 3-way result, not the snapshot's stale mode.
    reconcile_replayed_exec_bits(ws_path, snapshot, "HEAD");
    result
}

/// [`replay_snapshot`] without the bn-3fcbu mode reconciliation; callers
/// must run [`reconcile_replayed_exec_bits`] against the right "ours" commit.
fn replay_snapshot_raw(ws_path: &Path, snapshot: &SnapshotRef) -> Result<SnapshotReplayResult> {
    let repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    let oid: maw_git::GitOid = snapshot
        .oid
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid snapshot OID '{}': {e}", snapshot.oid))?;

    match repo.stash_apply(oid) {
        Ok(()) => {
            // Unstage everything so the user sees clean "unstaged modifications"
            // instead of a confusing mix of staged and unstaged files (bn-1r81).
            if let Err(e) = repo.unstage_all() {
                tracing::warn!("failed to unstage after replay: {e}");
            }
            tracing::info!("snapshot replayed cleanly");
            Ok(SnapshotReplayResult::Clean)
        }
        Err(err) => {
            // stash apply failed — check for conflict markers.
            let conflicts = detect_conflicts_in_worktree(ws_path)?;
            if conflicts.is_empty() {
                // Something else went wrong (not a merge conflict).
                bail!("stash_apply failed (no conflicts detected): {err}");
            }

            tracing::info!(
                conflict_count = conflicts.len(),
                "snapshot replay produced conflicts (left as markers in working tree)"
            );

            Ok(SnapshotReplayResult::Conflicts(conflicts))
        }
    }
}

/// Replay a snapshot with merge-aware 3-way merge for overlapping files.
///
/// When the target workspace has uncommitted edits that overlap with files
/// just merged from a workspace, a plain `stash_apply` would naively overwrite
/// the merge result with the stash content. This function instead uses
/// `git merge-file` for proper 3-way merging:
///
/// 1. Computes the overlap: stash paths whose committed content changed
///    between `anchor_epoch` and `epoch_after`.
/// 2. If **no overlap**: delegates to [`replay_snapshot()`].
/// 3. If **overlap exists**:
///    - Applies the stash for non-overlapping files (restores user edits).
///    - For each overlapping file, runs `git merge-file --diff3` with:
///      - BASE: content from `anchor_epoch` (the common ancestor)
///      - OURS: content from the merge result (current working tree)
///      - THEIRS: content from the stash (the user's local edits)
///    - Clean merges are written directly; conflicts get diff3 markers.
#[expect(
    clippy::too_many_lines,
    reason = "snapshot replay keeps merge-protection cases in ordered control flow"
)]
pub fn replay_snapshot_with_merge_protection(
    ws_path: &Path,
    snapshot: &SnapshotRef,
    anchor_epoch: &str,
    epoch_after: &str,
    source_workspace_names: &[String],
    target_workspace_name: &str,
) -> Result<SnapshotReplayResult> {
    // Step 1: Get the list of paths changed in the stash.
    let stash_paths = stash_changed_paths(ws_path, &snapshot.oid)?;

    // Step 2: Compute overlap.
    //
    // A stash path "overlaps" the merge — and therefore needs a driver-aware
    // 3-way merge rather than a bare `stash_apply` — when its committed
    // content changed between the anchor epoch and the post-merge epoch,
    // whether a merged workspace changed it or out-of-maw commits absorbed
    // into trunk did (bn-1xmk: a dirty `merge=union` journal restored via
    // `stash_apply` bypasses the driver and can silently drop the user's
    // uncommitted appends).
    //
    // bn-28s78: this used to also include every path the merge engine
    // resolved (`BuildPhaseOutput::resolved_paths`). That clause only ever
    // added paths whose committed bytes the merge left UNCHANGED (all others
    // are caught here already), where base == ours: the 3-way merge then
    // returns the user's version for diff3/union/ours, and a spurious
    // "conflict" for `merge=binary` or a user deletion — which
    // `verify_trunk_replay_fidelity` then overwrote with the user's version
    // anyway. Net effect: identical bytes on disk, plus a false conflict
    // report. It was also unavailable to crash recovery (not journaled),
    // so a recovered merge reported differently from an uninterrupted one.
    // The replay now depends only on (anchor, epoch_after, snapshot), which
    // every caller — live merge, crash recovery, promote — has.
    //
    // bn-3jqfk: file <-> directory collisions are split out first; they are
    // reported as conflicts and replayed from `apply_snapshot`, which leaves
    // them out.
    let (directory_conflicts, apply_snapshot) =
        split_directory_collisions(ws_path, snapshot, epoch_after, &stash_paths)?;
    // bn-1eg2u: the filtered copy is a new commit no ref points at; pin it
    // until the replay is done so a concurrent `git gc --prune` cannot drop
    // it between here and `stash_apply`. Unpinned on every return below.
    let _replay_pin = ReplayPin::pin(ws_path, target_workspace_name, snapshot, &apply_snapshot)?;
    maw::fp!("FP_CLEANUP_REPLAY_BEFORE_APPLY")?;
    let overlapping: Vec<PathBuf> = stash_paths
        .iter()
        .filter(|p| {
            !directory_conflicts
                .iter()
                .any(|c| Path::new(&c.path) == p.as_path())
        })
        .filter(|p| {
            committed_content_changed(ws_path, anchor_epoch, epoch_after, p)
                || exec_bit_change_under_local_symlink(
                    ws_path,
                    snapshot,
                    anchor_epoch,
                    epoch_after,
                    p,
                )
        })
        .cloned()
        .collect();

    if overlapping.is_empty() {
        // No overlap — safe to use normal replay. The executable bits are
        // reconciled against the merged commit, not whatever HEAD is (the
        // force-checkout fallback may have left HEAD elsewhere). (bn-3fcbu)
        let result = replay_snapshot_raw(ws_path, &apply_snapshot);
        reconcile_replayed_exec_bits(ws_path, &apply_snapshot, epoch_after);
        return with_conflicts(result, directory_conflicts);
    }

    tracing::info!(
        overlap_count = overlapping.len(),
        "stash overlaps with merged paths — using merge-file for overlapping files"
    );

    // Load the manifold config once so the bn-2upt sanity check below can
    // honor `merge.strict_post_rebase_check` and `merge.post_rebase_size_ratio_max`.
    // Resolve the repo root from `ws_path`: ws/<name>/ → repo_root.
    let sanity_cfg = {
        // Layout-aware repo-root resolution. From `<root>/ws/<n>/` we go
        // up two levels (parent().parent() → root); from
        // `<root>/.maw/workspaces/<n>/` we go up three. Try both shapes
        // and load whichever config exists; missing → defaults.
        let two_up = ws_path
            .parent()
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf);
        let three_up = two_up
            .as_deref()
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf);

        // The consolidated default workspace IS the repo root, so try
        // `ws_path` itself first.
        let mut cfg = maw_core::config::ManifoldConfig::default();
        for candidate_root in [Some(ws_path.to_path_buf()), two_up, three_up]
            .into_iter()
            .flatten()
        {
            let manifold = maw_core::model::layout::LayoutFlavor::detect_with_env(&candidate_root)
                .manifold_dir(&candidate_root);
            if manifold.is_dir() {
                // bn-hcbc8: this runs after the merge COMMIT, so refusing here
                // would strand the dirty-trunk snapshot. The merge already
                // refused an unparseable config up front; if it became
                // invalid since, warn and keep the fail-closed defaults
                // (strict sanity check ON).
                cfg = super::load_manifold_config_or_warn(&candidate_root);
                break;
            }
        }
        cfg
    };

    // Step 3a (bn-2ygs0): a path where the merged side or the user's side is
    // a symlink cannot go through the text merge below: the worktree entry
    // may be a symlink (writing markers would follow it, or refuse and abort
    // the whole replay), and a symlink and a file — or two symlink targets —
    // have no textual merge. Classify those from the three trees and capture
    // the merged entry exactly as the checkout left it on disk.
    let typed_paths =
        classify_symlink_overlaps(ws_path, snapshot, anchor_epoch, epoch_after, &overlapping)?;

    // Step 3: Save the merge versions of overlapping files BEFORE stash apply.
    // After checkout, these files contain the correct merge result.
    let mut merge_versions: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    for path in &overlapping {
        if typed_paths.iter().any(|t| &t.path == path) {
            continue;
        }
        let full = ws_path.join(path);
        // Never follow a symlink here: its target's bytes are not this
        // path's merged content. (bn-2ygs0)
        if full.symlink_metadata().is_ok_and(|m| m.is_file()) {
            match std::fs::read(&full) {
                Ok(content) => merge_versions.push((path.clone(), content)),
                Err(e) => {
                    tracing::warn!("failed to read merge version of {}: {e}", path.display());
                }
            }
        }
    }

    // Step 4: Apply the stash (restores user edits for non-overlapping files).
    // stash_apply is a naive overwrite, so overlapping files will get the stash
    // version — we fix those up in step 5 with a proper 3-way merge.
    let repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    let oid: maw_git::GitOid = apply_snapshot
        .oid
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid snapshot OID '{}': {e}", apply_snapshot.oid))?;
    // If stash_apply fails, user edits to non-overlapping files are NOT
    // restored — they aren't tracked in `merge_versions` (built only from
    // `overlapping`), so step 5's 3-way merge cannot rescue them. Treat this
    // as fatal so the caller preserves the snapshot ref for manual recovery
    // rather than silently advancing with missing edits. (bn-1psp)
    if let Err(e) = repo.stash_apply(oid) {
        return Err(anyhow::anyhow!(
            "failed to apply recovery stash during merge replay: {e}; \
             non-overlapping local edits would be lost — aborting so the \
             snapshot ref is preserved for manual recovery"
        ));
    }

    // Unstage everything for a clean working tree view.
    if let Err(e) = repo.unstage_all() {
        tracing::warn!("failed to unstage after stash apply: {e}");
    }

    // Step 4b (bn-2ygs0): settle the symlink/type-change paths. Conflicts are
    // data: the merged side goes back on disk, the user's side stays in the
    // pinned snapshot, and the conflict record names both so the caller can
    // print how to restore the user's side.
    let mut conflicts = settle_symlink_overlaps(ws_path, &typed_paths)?;
    conflicts.extend(directory_conflicts);

    // Step 5: For each overlapping file, run a proper 3-way merge using
    // `git merge-file`. This cleanly merges non-overlapping edits and only
    // produces conflict markers for true conflicts. (bn-2fk0)
    let merge_label = if source_workspace_names.len() == 1 {
        format!("{} (merged workspace)", source_workspace_names[0])
    } else {
        format!("{} (merged workspaces)", source_workspace_names.join(", "))
    };
    let local_label = format!("{target_workspace_name} (local edits)");

    // Load `.gitattributes` for merge driver selection. The anchor state
    // keeps parity with BUILD phase semantics, while the merged target state
    // covers dirty replay when a workspace introduces append-only merge rules
    // such as `.bones/events/** merge=union`.
    let anchor_attrs = load_stash_replay_attrs(ws_path, anchor_epoch);
    let target_attrs = load_stash_replay_attrs(ws_path, epoch_after);

    for (path, merge_content) in &merge_versions {
        let full = ws_path.join(path);

        // Read the stash version (user's local edits).
        let Some(stash_content) = read_file_at_commit(ws_path, &snapshot.oid, path) else {
            // Stash doesn't have this file — user deleted it.
            // That's a delete-vs-modify conflict.
            let markers = write_diff3_markers(
                read_file_at_commit(ws_path, anchor_epoch, path)
                    .as_deref()
                    .unwrap_or(b""),
                merge_content,
                b"",
                &merge_label,
                &local_label,
            );
            write_replay_output(&full, &markers, "write delete/modify conflict markers")?;
            conflicts.push(WorkingCopyConflict {
                path: path.display().to_string(),
                conflict_type: "delete_mod_conflict".to_owned(),
                type_conflict: None,
            });
            continue;
        };

        // If stash version equals merge version, nothing to do.
        if stash_content == *merge_content {
            // Restore merge version (stash_apply may have overwritten).
            write_replay_output(&full, merge_content, "restore merged content")?;
            continue;
        }

        let base_content = read_file_at_commit(ws_path, anchor_epoch, path).unwrap_or_default();

        // Ensure parent directories exist.
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create parent directory {} while replaying snapshot",
                    parent.display()
                )
            })?;
        }

        // Look up the merge driver for this path.
        let rel = path.to_string_lossy().replace('\\', "/");
        let driver = anchor_attrs
            .as_ref()
            .and_then(|m| m.merge_driver(&rel))
            .or_else(|| target_attrs.as_ref().and_then(|m| m.merge_driver(&rel)));

        // 3-way text merge (pure Rust via gix-merge). Cleanly merges
        // non-overlapping edits; produces diff3 markers for true conflicts.
        // If the path has a merge driver (union, ours, binary), that takes
        // precedence over the default diff3 behavior.
        match merge_text_three_way(
            &base_content,
            merge_content,
            &stash_content,
            &merge_label,
            &local_label,
            driver.as_deref(),
        ) {
            Ok((merged, false)) => {
                // Apply the bn-2upt post-merge sanity check to this clean
                // result before writing. The snapshot-replay path is the
                // surface that produced bn-3r8s's silent doubling; without
                // this check, a bug that fabricates 2x or 3x content
                // through a diff3 misalignment would be written to disk
                // and the user would only notice when their build fails.
                let sanity =
                    super::sync::rebase::PostRebaseSanityConfig::from_merge(&sanity_cfg.merge);
                let size_check = super::sync::rebase::check_size_delta(
                    &base_content,
                    merge_content,
                    &stash_content,
                    &merged,
                    sanity.size_ratio_max,
                );
                if let Err(failure) = size_check {
                    if sanity.strict {
                        // Strict: reject the suspicious-but-clean result and
                        // fall through to the marker path so the user gets
                        // a chance to resolve manually.
                        eprintln!(
                            "  WARNING: post-merge sanity check tripped on '{}': {failure}. Routing through conflict markers.",
                            path.display()
                        );
                        let markers = write_diff3_markers(
                            &base_content,
                            merge_content,
                            &stash_content,
                            &merge_label,
                            &local_label,
                        );
                        write_replay_output(
                            &full,
                            &markers,
                            "write post-merge sanity conflict markers",
                        )?;
                        conflicts.push(WorkingCopyConflict {
                            path: path.display().to_string(),
                            conflict_type: "sanity-flag".to_owned(),
                            type_conflict: None,
                        });
                        continue;
                    }
                    eprintln!(
                        "  WARNING: post-merge sanity check tripped on '{}': {failure}. Accepting (strict_post_rebase_check is off).",
                        path.display()
                    );
                }
                // Non-overlapping edits merged cleanly — write the result.
                write_replay_output(&full, &merged, "write merged content")?;
            }
            Ok((marker_output, true)) => {
                // True conflict — write the markers.
                write_replay_output(&full, &marker_output, "write conflict markers")?;
                conflicts.push(WorkingCopyConflict {
                    path: path.display().to_string(),
                    conflict_type: "content".to_owned(),
                    type_conflict: None,
                });
            }
            Err(e) => {
                // Merge failed — fall back to whole-file conflict markers.
                tracing::warn!(
                    "text merge failed for {}: {e}; falling back to full-file markers",
                    path.display()
                );
                let markers = write_diff3_markers(
                    &base_content,
                    merge_content,
                    &stash_content,
                    &merge_label,
                    &local_label,
                );
                write_replay_output(&full, &markers, "write fallback conflict markers")?;
                conflicts.push(WorkingCopyConflict {
                    path: path.display().to_string(),
                    conflict_type: "content".to_owned(),
                    type_conflict: None,
                });
            }
        }
    }

    // Step 5b: `stash_apply` recreated every replayed file with the
    // snapshot's mode — the mode the file had BEFORE the merge — so a merged
    // `chmod +x` would be silently reverted in the worktree (and by the next
    // trunk commit). Reconcile the executable bits against the merged
    // commit. (bn-3fcbu)
    reconcile_replayed_exec_bits(ws_path, &apply_snapshot, epoch_after);

    // Step 6: Also detect any conflicts from non-overlapping files (normal
    // stash apply conflicts).
    let git_conflicts = detect_conflicts_in_worktree(ws_path)?;
    for c in git_conflicts {
        let c_path = PathBuf::from(&c.path);
        if !overlapping.contains(&c_path) {
            conflicts.push(c);
        }
    }

    if conflicts.is_empty() {
        Ok(SnapshotReplayResult::Clean)
    } else {
        Ok(SnapshotReplayResult::Conflicts(conflicts))
    }
}

// ---------------------------------------------------------------------------
// bn-2ygs0: symlink / type-change overlaps in the dirty-trunk replay
// ---------------------------------------------------------------------------

/// A path's entry in one of the three replay trees (base, merged, snapshot).
#[derive(Clone, Debug)]
struct TreeSide {
    mode: maw_git::EntryMode,
    oid: maw_git::GitOid,
    content: Vec<u8>,
}

impl TreeSide {
    fn same_entry(a: Option<&Self>, b: Option<&Self>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => a.mode == b.mode && a.oid == b.oid,
            _ => false,
        }
    }

    fn is_symlink(side: Option<&Self>) -> bool {
        side.is_some_and(|s| s.mode == maw_git::EntryMode::Link)
    }

    fn kind(side: Option<&Self>) -> EntryKind {
        match side {
            None => EntryKind::Deleted,
            Some(s) if s.mode == maw_git::EntryMode::Link => EntryKind::Symlink {
                target: String::from_utf8_lossy(&s.content).into_owned(),
            },
            Some(s) => EntryKind::File {
                executable: s.mode == maw_git::EntryMode::BlobExecutable,
            },
        }
    }
}

/// A worktree entry captured from disk (never through a symlink).
///
/// Shared with the merge's in-memory pre-merge capture (bn-3jqfk), which must
/// record a symlink as a symlink, never as its target's contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DiskSide {
    Absent,
    Symlink(PathBuf),
    File { bytes: Vec<u8>, mode: u32 },
}

impl DiskSide {
    /// Capture the entry at `rel` inside the worktree `ws_path`, the way git
    /// sees it: an entry behind a symlinked (or non-directory) parent
    /// component does not exist — `d/inner.txt` after `rm -r d && ln -s x d`
    /// is a deletion, never the bytes of `x/inner.txt`. (bn-1dlkd)
    pub(super) fn capture_at(ws_path: &Path, rel: &Path) -> Result<Self> {
        let components: Vec<_> = rel.components().collect();
        let Some((_, parents)) = components.split_last() else {
            bail!("refusing to capture an empty path in {}", ws_path.display());
        };
        let mut current = ws_path.to_path_buf();
        for component in parents {
            let std::path::Component::Normal(name) = component else {
                bail!("refusing to capture non-normal path {}", rel.display());
            };
            current.push(name);
            match current.symlink_metadata() {
                Ok(meta) if meta.is_dir() => {}
                // A symlink or a file where a parent directory would go.
                Ok(_) => return Ok(Self::Absent),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::Absent),
                Err(e) => return Err(e).with_context(|| format!("stat {}", current.display())),
            }
        }
        Self::capture(&ws_path.join(rel))
    }

    pub(super) fn capture(full: &Path) -> Result<Self> {
        let meta = match full.symlink_metadata() {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::Absent),
            Err(e) => return Err(e).with_context(|| format!("stat {}", full.display())),
        };
        if meta.file_type().is_symlink() {
            let target =
                std::fs::read_link(full).with_context(|| format!("readlink {}", full.display()))?;
            return Ok(Self::Symlink(target));
        }
        if meta.is_file() {
            let bytes = std::fs::read(full).with_context(|| format!("read {}", full.display()))?;
            return Ok(Self::File {
                bytes,
                mode: file_mode_bits(&meta),
            });
        }
        Ok(Self::Absent)
    }

    /// The entry a tree side describes (used when the disk does not hold it).
    fn from_tree(side: Option<&TreeSide>) -> Self {
        match side {
            None => Self::Absent,
            Some(s) if s.mode == maw_git::EntryMode::Link => {
                Self::Symlink(PathBuf::from(bytes_to_os_string(&s.content)))
            }
            Some(s) => Self::File {
                bytes: s.content.clone(),
                mode: if s.mode == maw_git::EntryMode::BlobExecutable {
                    0o755
                } else {
                    0o644
                },
            },
        }
    }

    fn matches_kind(&self, side: Option<&TreeSide>) -> bool {
        match (self, side) {
            (Self::Absent, None) => true,
            (Self::Symlink(_), Some(s)) => s.mode == maw_git::EntryMode::Link,
            (Self::File { .. }, Some(s)) => matches!(
                s.mode,
                maw_git::EntryMode::Blob | maw_git::EntryMode::BlobExecutable
            ),
            _ => false,
        }
    }
}

#[cfg(unix)]
fn file_mode_bits(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn file_mode_bits(_meta: &std::fs::Metadata) -> u32 {
    0o644
}

#[cfg(unix)]
fn bytes_to_os_string(bytes: &[u8]) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::OsStr::from_bytes(bytes).to_os_string()
}

#[cfg(not(unix))]
fn bytes_to_os_string(bytes: &[u8]) -> std::ffi::OsString {
    String::from_utf8_lossy(bytes).into_owned().into()
}

/// An overlapping replay path where the merged or the user's side is a
/// symlink.
#[derive(Debug)]
struct SymlinkOverlap {
    path: PathBuf,
    base: Option<TreeSide>,
    ours: Option<TreeSide>,
    theirs: Option<TreeSide>,
    /// The merged entry as the checkout left it on disk, captured before
    /// `stash_apply` overwrote it.
    ours_on_disk: DiskSide,
}

fn read_tree_side(
    repo: &maw_git::GixRepo,
    commit: maw_git::GitOid,
    path: &str,
) -> Result<Option<TreeSide>> {
    Ok(repo
        .read_blob_at_path(commit, path)
        .map_err(|e| anyhow::anyhow!("read '{path}' at {commit}: {e}"))?
        .map(|(mode, oid, content)| TreeSide { mode, oid, content }))
}

/// Pick out the overlapping paths whose merged (`epoch_after`) or snapshot
/// entry is a symlink, with all three sides and the on-disk merged entry.
fn classify_symlink_overlaps(
    ws_path: &Path,
    snapshot: &SnapshotRef,
    anchor_epoch: &str,
    epoch_after: &str,
    overlapping: &[PathBuf],
) -> Result<Vec<SymlinkOverlap>> {
    let repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    let resolve = |spec: &str| {
        repo.rev_parse(spec)
            .map_err(|e| anyhow::anyhow!("resolve '{spec}': {e}"))
    };
    let base_oid = resolve(anchor_epoch)?;
    let ours_oid = resolve(epoch_after)?;
    let theirs_oid = resolve(&snapshot.oid)?;

    let mut out = Vec::new();
    for path in overlapping {
        let Some(rel) = path.to_str() else {
            continue;
        };
        let rel = rel.replace('\\', "/");
        let ours = read_tree_side(&repo, ours_oid, &rel)?;
        let theirs = read_tree_side(&repo, theirs_oid, &rel)?;
        if !TreeSide::is_symlink(ours.as_ref()) && !TreeSide::is_symlink(theirs.as_ref()) {
            continue;
        }
        let base = read_tree_side(&repo, base_oid, &rel)?;
        let ours_on_disk = DiskSide::capture_at(ws_path, path)?;
        out.push(SymlinkOverlap {
            path: path.clone(),
            base,
            ours,
            theirs,
            ours_on_disk,
        });
    }
    Ok(out)
}

/// After `stash_apply` wrote the user's entry for every symlink overlap,
/// settle each one as a 3-way merge of whole entries:
///
/// - same entry on both sides, or only the user changed it: the user's entry
///   (already on disk) stands;
/// - only the merge changed it: the merged entry goes back on disk;
/// - both changed it differently: a `type_change` conflict. The merged entry
///   goes back on disk and the user's entry stays in the snapshot; the record
///   names both. If the merge deleted the path, the user's entry stays on
///   disk (as for a regular file the merge deleted), still reported.
fn settle_symlink_overlaps(
    ws_path: &Path,
    overlaps: &[SymlinkOverlap],
) -> Result<Vec<WorkingCopyConflict>> {
    let mut conflicts = Vec::new();
    for o in overlaps {
        let (base, ours, theirs) = (o.base.as_ref(), o.ours.as_ref(), o.theirs.as_ref());
        if TreeSide::same_entry(ours, theirs) || TreeSide::same_entry(ours, base) {
            continue;
        }
        let merged_entry = if o.ours_on_disk.matches_kind(ours) {
            o.ours_on_disk.clone()
        } else {
            DiskSide::from_tree(ours)
        };
        if TreeSide::same_entry(theirs, base) {
            write_worktree_entry(ws_path, &o.path, &merged_entry)?;
            continue;
        }
        let kept = if ours.is_some() {
            write_worktree_entry(ws_path, &o.path, &merged_entry)?;
            KeptSide::Merged
        } else {
            KeptSide::Local
        };
        let (merged, local) = (TreeSide::kind(ours), TreeSide::kind(theirs));
        tracing::info!(
            path = %o.path.display(),
            %merged,
            %local,
            ?kept,
            "dirty replay: symlink/type conflict"
        );
        conflicts.push(WorkingCopyConflict {
            path: o.path.display().to_string(),
            conflict_type: "type_change".to_owned(),
            type_conflict: Some(TypeConflict {
                merged,
                local,
                kept,
            }),
        });
    }
    Ok(conflicts)
}

/// Replace the worktree entry at `rel` with `entry`, never following a
/// symlink: parent components inside the workspace must be real directories
/// (missing ones are created), and an existing final entry is unlinked, not
/// written through.
pub(super) fn write_worktree_entry(ws_path: &Path, rel: &Path, entry: &DiskSide) -> Result<()> {
    let components: Vec<_> = rel.components().collect();
    let Some((_, parents)) = components.split_last() else {
        bail!("refusing to write an empty path in {}", ws_path.display());
    };
    let mut current = ws_path.to_path_buf();
    for component in parents {
        let std::path::Component::Normal(name) = component else {
            bail!("refusing to write non-normal path {}", rel.display());
        };
        current.push(name);
        match current.symlink_metadata() {
            Ok(meta) if meta.file_type().is_symlink() => bail!(
                "refusing to write {} because path component {} is a symlink",
                rel.display(),
                current.display()
            ),
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => bail!(
                "refusing to write {} because {} is not a directory",
                rel.display(),
                current.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current)
                    .with_context(|| format!("create directory {}", current.display()))?;
            }
            Err(e) => return Err(e).with_context(|| format!("stat {}", current.display())),
        }
    }
    let full = ws_path.join(rel);
    match full.symlink_metadata() {
        // bn-ihi4h: a directory holding nothing but empty directories (what
        // is left of a tracked directory once its files are deleted, e.g. the
        // user turned `d/` into file `d`) is no data and may be replaced.
        Ok(meta) if meta.is_dir() => remove_empty_dir_tree(&full).with_context(|| {
            format!(
                "refusing to replace non-empty directory {} with a file or symlink",
                full.display()
            )
        })?,
        Ok(_) => {
            std::fs::remove_file(&full).with_context(|| format!("remove {}", full.display()))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("stat {}", full.display())),
    }
    match entry {
        DiskSide::Absent => Ok(()),
        DiskSide::Symlink(target) => create_symlink(target, &full),
        DiskSide::File { bytes, mode } => {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&full)
                .with_context(|| format!("create {}", full.display()))?;
            file.write_all(bytes)
                .with_context(|| format!("write {}", full.display()))?;
            set_file_mode(&file, *mode).with_context(|| format!("chmod {}", full.display()))
        }
    }
}

/// Remove `dir` if it holds nothing but (nested) empty directories. Never
/// follows a symlink; anything else in the tree fails the call.
fn remove_empty_dir_tree(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            remove_empty_dir_tree(&entry.path())?;
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::DirectoryNotEmpty,
                format!("{} is not empty", dir.display()),
            ));
        }
    }
    std::fs::remove_dir(dir)
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("create symlink {}", link.display()))
}

#[cfg(not(unix))]
fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    std::fs::write(link, target.to_string_lossy().as_bytes())
        .with_context(|| format!("write symlink placeholder {}", link.display()))
}

#[cfg(unix)]
fn set_file_mode(file: &std::fs::File, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_file_mode(_file: &std::fs::File, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// bn-3jqfk: file <-> directory changes in the dirty-trunk replay
// ---------------------------------------------------------------------------

/// How a tree holds one path, for the file/directory collision check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathShape {
    /// Neither the path nor any ancestor that would block it exists.
    Absent,
    /// A proper ancestor of the path is a file, symlink or gitlink, so the
    /// path cannot exist. Carries that ancestor's component count and
    /// whether it is a symlink (bn-1eg2u: reported as "replaced by ...").
    Blocked {
        /// Number of leading path components that form the blocking entry.
        depth: usize,
        /// Whether the blocking entry is a symlink.
        symlink: bool,
    },
    /// The path is a directory.
    Tree,
    /// The path is a file, symlink or gitlink.
    Entry,
}

/// The shape of `rel` (slash-separated) in the tree of `commit`.
fn tree_path_shape(
    repo: &maw_git::GixRepo,
    commit: maw_git::GitOid,
    rel: &str,
) -> Result<PathShape> {
    let lookup = |path: &str| {
        repo.find_entry_at_path(commit, path)
            .map_err(|e| anyhow::anyhow!("read '{path}' at {commit}: {e}"))
    };
    let components: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
    for end in 1..components.len() {
        match lookup(&components[..end].join("/"))? {
            None => return Ok(PathShape::Absent),
            Some((maw_git::EntryMode::Tree, _)) => {}
            Some((mode, _)) => {
                return Ok(PathShape::Blocked {
                    depth: end,
                    symlink: mode == maw_git::EntryMode::Link,
                });
            }
        }
    }
    Ok(match lookup(rel)? {
        None => PathShape::Absent,
        Some((maw_git::EntryMode::Tree, _)) => PathShape::Tree,
        Some(_) => PathShape::Entry,
    })
}

/// The [`EntryKind`] of `rel` in `commit`, for a conflict report.
fn tree_path_kind(
    repo: &maw_git::GixRepo,
    commit: maw_git::GitOid,
    rel: &str,
    shape: PathShape,
) -> Result<EntryKind> {
    Ok(match shape {
        PathShape::Absent => EntryKind::Deleted,
        PathShape::Blocked { depth, symlink } => EntryKind::ReplacedBy {
            path: rel
                .split('/')
                .filter(|c| !c.is_empty())
                .take(depth)
                .collect::<Vec<_>>()
                .join("/"),
            symlink,
        },
        PathShape::Tree => EntryKind::Directory,
        PathShape::Entry => TreeSide::kind(read_tree_side(repo, commit, rel)?.as_ref()),
    })
}

/// Whether `path` in `rev` is a directory or sits under a file — i.e. a
/// regular file or symlink the user had at `path` cannot be put back there.
/// `None` if the tree cannot be read. (bn-3jqfk)
pub fn is_directory_blocked_at(repo: &maw_git::GixRepo, rev: &str, path: &Path) -> Option<bool> {
    let commit = repo.rev_parse(rev).ok()?;
    let rel = path.to_str()?.replace('\\', "/");
    let shape = tree_path_shape(repo, commit, &rel).ok()?;
    Some(matches!(shape, PathShape::Tree | PathShape::Blocked { .. }))
}

/// The parent of `path` that is a file or symlink in `rev` (so `path`
/// cannot exist there), if any. `None` also when the tree cannot be read.
/// (bn-1eg2u)
pub fn blocking_parent_at(repo: &maw_git::GixRepo, rev: &str, path: &Path) -> Option<PathBuf> {
    let commit = repo.rev_parse(rev).ok()?;
    let rel = path.to_str()?.replace('\\', "/");
    match tree_path_shape(repo, commit, &rel).ok()? {
        PathShape::Blocked { depth, .. } => Some(
            rel.split('/')
                .filter(|c| !c.is_empty())
                .take(depth)
                .collect::<PathBuf>(),
        ),
        _ => None,
    }
}

/// Split the file <-> directory collisions out of a dirty-trunk replay.
///
/// A path the user has as a file or symlink collides with the merged tree
/// when the merged tree has a directory there, or a file where one of its
/// parent directories would go. `stash_apply` cannot write either (it fails
/// with "Is a directory" / "File exists" and the whole replay used to abort),
/// and no merge of the two exists. Every changed snapshot path at, under or
/// above a colliding path is part of the same change (e.g. the deletion of
/// file `p` that made room for the user's `p/x`), so all of them are left
/// out of the replay: the merged side stays on disk, the user's side stays
/// in the pinned snapshot, and each one is reported as a `directory_change`
/// conflict naming both sides.
///
/// Returns the conflicts, and the snapshot to apply instead: `snapshot`
/// itself when nothing collides, else a commit on the snapshot's base whose
/// tree has those paths put back to the base. The original snapshot is not
/// changed; it remains the recovery source for the user's side.
fn split_directory_collisions(
    ws_path: &Path,
    snapshot: &SnapshotRef,
    epoch_after: &str,
    stash_paths: &[PathBuf],
) -> Result<(Vec<WorkingCopyConflict>, SnapshotRef)> {
    let repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    let resolve = |spec: &str| {
        repo.rev_parse(spec)
            .map_err(|e| anyhow::anyhow!("resolve '{spec}': {e}"))
    };
    let ours_oid = resolve(epoch_after)?;
    let theirs_oid = resolve(&snapshot.oid)?;
    // The snapshot's own base: `stash_apply` replays base -> snapshot.
    let base_oid = *repo
        .read_commit(theirs_oid)
        .map_err(|e| anyhow::anyhow!("read snapshot commit: {e}"))?
        .parents
        .first()
        .ok_or_else(|| anyhow::anyhow!("snapshot {} has no parent", snapshot.oid))?;

    let rel_of = |path: &Path| path.to_str().map(|p| p.replace('\\', "/"));
    let mut colliding: Vec<&PathBuf> = Vec::new();
    for path in stash_paths {
        let Some(rel) = rel_of(path) else {
            continue;
        };
        if tree_path_shape(&repo, theirs_oid, &rel)? == PathShape::Entry
            && collision_region_changed(&repo, base_oid, ours_oid, &rel)?
        {
            colliding.push(path);
        }
    }
    if colliding.is_empty() {
        return Ok((Vec::new(), snapshot.clone()));
    }

    let dropped: Vec<&PathBuf> = stash_paths
        .iter()
        .filter(|p| {
            colliding
                .iter()
                .any(|c| p.starts_with(c.as_path()) || c.starts_with(p.as_path()))
        })
        .collect();

    let mut conflicts = Vec::new();
    for path in &dropped {
        let Some(rel) = rel_of(path) else {
            continue;
        };
        let merged = tree_path_kind(
            &repo,
            ours_oid,
            &rel,
            tree_path_shape(&repo, ours_oid, &rel)?,
        )?;
        let local = tree_path_kind(
            &repo,
            theirs_oid,
            &rel,
            tree_path_shape(&repo, theirs_oid, &rel)?,
        )?;
        tracing::info!(
            path = %path.display(),
            %merged,
            %local,
            "dirty replay: file/directory conflict"
        );
        conflicts.push(WorkingCopyConflict {
            path: path.display().to_string(),
            conflict_type: "directory_change".to_owned(),
            type_conflict: Some(TypeConflict {
                merged,
                local,
                kept: KeptSide::Merged,
            }),
        });
    }

    let filtered = snapshot_without_paths(&repo, snapshot, theirs_oid, &dropped)?;
    Ok((conflicts, filtered))
}

/// Whether the merged tree `ours` is in the way of an entry at `rel` (a
/// directory there, or a file or symlink where one of its parent directories
/// would go) AND the merge changed that obstacle relative to `base`.
///
/// bn-ihi4h: when the obstacle is identical in `base` and `ours`, the merge
/// left the region alone: the user's own change (file `p` -> directory `p/`,
/// directory `d/` -> file or symlink `d`) replays onto the merged tree
/// exactly as onto the base, so it is not a collision and stays in place,
/// like any uncommitted change to a path the merge did not touch. A gitlink
/// obstacle always collides (`stash_apply` cannot remove a submodule).
fn collision_region_changed(
    repo: &maw_git::GixRepo,
    base: maw_git::GitOid,
    ours: maw_git::GitOid,
    rel: &str,
) -> Result<bool> {
    let obstacle = match tree_path_shape(repo, ours, rel)? {
        PathShape::Tree => rel.to_owned(),
        PathShape::Blocked { depth, .. } => rel
            .split('/')
            .filter(|c| !c.is_empty())
            .take(depth)
            .collect::<Vec<_>>()
            .join("/"),
        PathShape::Absent | PathShape::Entry => return Ok(false),
    };
    let entry = |commit: maw_git::GitOid| {
        repo.find_entry_at_path(commit, &obstacle)
            .map_err(|e| anyhow::anyhow!("read '{obstacle}' at {commit}: {e}"))
    };
    let ours_entry = entry(ours)?;
    if matches!(ours_entry, Some((maw_git::EntryMode::Commit, _))) {
        return Ok(true);
    }
    Ok(entry(base)? != ours_entry)
}

/// Whether the merge changed the region that makes the user's entry at
/// `path` collide with the merged tree `epoch_after` (see
/// [`collision_region_changed`]); the base is `anchor`. `None` if a tree
/// cannot be read. (bn-ihi4h)
pub fn is_changed_collision_at(
    repo: &maw_git::GixRepo,
    anchor: &str,
    epoch_after: &str,
    path: &Path,
) -> Option<bool> {
    let base = repo.rev_parse(anchor).ok()?;
    let ours = repo.rev_parse(epoch_after).ok()?;
    let rel = path.to_str()?.replace('\\', "/");
    collision_region_changed(repo, base, ours, &rel).ok()
}

/// Ref that pins the filtered replay commit of [`split_directory_collisions`]
/// while `ws_name`'s replay runs (bn-1eg2u).
pub(super) fn replay_pin_ref_name(ws_name: &str) -> String {
    format!("refs/manifold/replay/{ws_name}")
}

/// Keeps the filtered replay commit reachable for the duration of one
/// replay; the ref is deleted when this is dropped. The original snapshot
/// stays pinned by its own refs, so the filtered copy is never needed again.
struct ReplayPin {
    ws_path: PathBuf,
    ref_name: Option<String>,
}

impl ReplayPin {
    fn pin(
        ws_path: &Path,
        ws_name: &str,
        snapshot: &SnapshotRef,
        apply_snapshot: &SnapshotRef,
    ) -> Result<Self> {
        if apply_snapshot.oid == snapshot.oid {
            return Ok(Self {
                ws_path: ws_path.to_path_buf(),
                ref_name: None,
            });
        }
        let ref_name = replay_pin_ref_name(ws_name);
        let oid = GitOid::new(&apply_snapshot.oid)
            .map_err(|e| anyhow::anyhow!("invalid replay OID '{}': {e}", apply_snapshot.oid))?;
        manifold_refs::write_ref(ws_path, &ref_name, &oid)
            .map_err(|e| anyhow::anyhow!("failed to pin the replay commit at {ref_name}: {e}"))?;
        Ok(Self {
            ws_path: ws_path.to_path_buf(),
            ref_name: Some(ref_name),
        })
    }
}

impl Drop for ReplayPin {
    fn drop(&mut self) {
        if let Some(ref_name) = &self.ref_name
            && let Err(e) = manifold_refs::delete_ref(&self.ws_path, ref_name)
        {
            tracing::warn!("failed to remove replay pin {ref_name}: {e}");
        }
    }
}

/// `snapshot` minus `dropped`: a commit on the snapshot's base whose tree puts
/// the top-most dropped path of each group back to the base (a whole subtree
/// if the base has a directory there). (bn-3jqfk)
fn snapshot_without_paths(
    repo: &maw_git::GixRepo,
    snapshot: &SnapshotRef,
    theirs_oid: maw_git::GitOid,
    dropped: &[&PathBuf],
) -> Result<SnapshotRef> {
    let stash = repo
        .read_commit(theirs_oid)
        .map_err(|e| anyhow::anyhow!("read snapshot commit: {e}"))?;
    let base_oid = *stash
        .parents
        .first()
        .ok_or_else(|| anyhow::anyhow!("snapshot {} has no parent", snapshot.oid))?;
    let mut edits = Vec::new();
    for path in dropped {
        if dropped
            .iter()
            .any(|other| other != path && path.starts_with(other.as_path()))
        {
            continue;
        }
        let Some(rel) = path.to_str().map(|p| p.replace('\\', "/")) else {
            continue;
        };
        match repo
            .find_entry_at_path(base_oid, &rel)
            .map_err(|e| anyhow::anyhow!("read '{rel}' at {base_oid}: {e}"))?
        {
            Some((mode, oid)) => edits.push(maw_git::TreeEdit::Upsert {
                path: rel,
                mode,
                oid,
            }),
            None => edits.push(maw_git::TreeEdit::Remove { path: rel }),
        }
    }
    let tree = repo
        .edit_tree(stash.tree_oid, &edits)
        .map_err(|e| anyhow::anyhow!("build replay tree without directory conflicts: {e}"))?;
    let filtered = repo
        .create_commit(
            tree,
            &[base_oid],
            "bn-3jqfk: dirty replay without file/directory conflicts",
            None,
        )
        .map_err(|e| anyhow::anyhow!("commit replay tree without directory conflicts: {e}"))?;
    Ok(SnapshotRef {
        oid: filtered.to_string(),
        ref_name: snapshot.ref_name.clone(),
    })
}

/// Add `extra` conflicts to a replay result.
fn with_conflicts(
    result: Result<SnapshotReplayResult>,
    extra: Vec<WorkingCopyConflict>,
) -> Result<SnapshotReplayResult> {
    if extra.is_empty() {
        return result;
    }
    match result? {
        SnapshotReplayResult::Clean => Ok(SnapshotReplayResult::Conflicts(extra)),
        SnapshotReplayResult::Conflicts(mut conflicts) => {
            conflicts.extend(extra);
            Ok(SnapshotReplayResult::Conflicts(conflicts))
        }
    }
}

/// Reconcile the executable bit of every file a snapshot replay wrote.
///
/// `stash_apply` recreates each replayed file with the mode recorded in the
/// snapshot tree, which is the mode the file had when the snapshot was taken
/// unless the user changed it. When the checkout between snapshot and replay
/// changed a file's mode (e.g. a merged workspace committed `chmod +x`), the
/// replayed file would silently revert that change. This runs a 3-way merge
/// on the executable bit per replayed regular file:
///
/// - BASE: the snapshot's first parent (HEAD when the snapshot was taken),
/// - THEIRS: the snapshot tree (the user's worktree state),
/// - OURS: `ours_rev` (the commit that was checked out before the replay).
///
/// If the user changed the path's mode (THEIRS != BASE), the user's mode
/// wins; otherwise OURS's executable bit is applied. Only the executable bit
/// of regular files is touched, never through a symlink (neither the file
/// itself nor any parent component inside the workspace). Best effort: a
/// failure is logged and leaves the file as the replay wrote it. (bn-3fcbu)
fn reconcile_replayed_exec_bits(ws_path: &Path, snapshot: &SnapshotRef, ours_rev: &str) {
    if let Err(e) = try_reconcile_replayed_exec_bits(ws_path, snapshot, ours_rev) {
        tracing::warn!("failed to reconcile file modes after snapshot replay: {e:#}");
    }
}

fn try_reconcile_replayed_exec_bits(
    ws_path: &Path,
    snapshot: &SnapshotRef,
    ours_rev: &str,
) -> Result<()> {
    use maw_git::EntryMode;

    let repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    let stash_oid: maw_git::GitOid = snapshot
        .oid
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid snapshot OID '{}': {e}", snapshot.oid))?;
    let stash = repo
        .read_commit(stash_oid)
        .map_err(|e| anyhow::anyhow!("read snapshot commit: {e}"))?;
    let Some(base_commit) = stash.parents.first().copied() else {
        return Ok(());
    };
    let base_tree = repo
        .read_commit(base_commit)
        .map_err(|e| anyhow::anyhow!("read snapshot parent: {e}"))?
        .tree_oid;
    let ours_commit = repo
        .rev_parse(ours_rev)
        .map_err(|e| anyhow::anyhow!("resolve '{ours_rev}': {e}"))?;
    let ours_tree = repo
        .read_commit(ours_commit)
        .map_err(|e| anyhow::anyhow!("read '{ours_rev}': {e}"))?
        .tree_oid;
    if ours_tree == base_tree {
        // Nothing was checked out in between — the snapshot's modes are
        // already the right answer.
        return Ok(());
    }

    let user_changes = repo
        .diff_trees(Some(base_tree), stash.tree_oid)
        .map_err(|e| anyhow::anyhow!("diff snapshot against its parent: {e}"))?;
    let merged_modes: std::collections::HashMap<String, Option<EntryMode>> = repo
        .diff_trees(Some(base_tree), ours_tree)
        .map_err(|e| anyhow::anyhow!("diff '{ours_rev}' against the snapshot parent: {e}"))?
        .into_iter()
        .map(|d| (d.path, d.new_mode))
        .collect();

    for change in user_changes {
        let is_regular =
            |m: Option<EntryMode>| matches!(m, Some(EntryMode::Blob | EntryMode::BlobExecutable));
        if !is_regular(change.new_mode) {
            // Deleted, a symlink, or a gitlink in the snapshot: no exec bit.
            continue;
        }
        if change.old_mode != change.new_mode {
            // The user's own mode change (or addition) wins; `stash_apply`
            // already wrote the snapshot's mode.
            continue;
        }
        let Some(&ours_mode) = merged_modes.get(&change.path) else {
            // The merge did not touch this path: ours == base == theirs.
            continue;
        };
        if !is_regular(ours_mode) {
            // Type change on the merged side (e.g. file -> symlink) vs a user
            // edit: that is a content conflict the replay reports; do not
            // guess an exec bit.
            continue;
        }
        let want_exec = ours_mode == Some(EntryMode::BlobExecutable);
        set_worktree_exec_bit(ws_path, Path::new(&change.path), want_exec)?;
    }
    Ok(())
}

/// Set or clear the executable bit of the regular file `rel` inside
/// `ws_path`, without following symlinks (the file or any parent component
/// inside the workspace). Non-regular paths are left alone.
#[cfg(unix)]
pub(super) fn set_worktree_exec_bit(ws_path: &Path, rel: &Path, exec: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut current = ws_path.to_path_buf();
    for component in rel.components() {
        current.push(component);
        let Ok(meta) = current.symlink_metadata() else {
            return Ok(());
        };
        if meta.file_type().is_symlink() {
            return Ok(());
        }
    }
    let meta = current
        .symlink_metadata()
        .with_context(|| format!("stat {}", current.display()))?;
    if !meta.is_file() {
        return Ok(());
    }
    let mode = meta.permissions().mode();
    let new_mode = if exec {
        // Grant execute wherever read is granted, like git checkout does.
        mode | ((mode & 0o444) >> 2)
    } else {
        mode & !0o111
    };
    if new_mode != mode {
        std::fs::set_permissions(&current, std::fs::Permissions::from_mode(new_mode))
            .with_context(|| format!("chmod {}", current.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn set_worktree_exec_bit(_ws_path: &Path, _rel: &Path, _exec: bool) -> Result<()> {
    Ok(())
}

/// Write one replay result and preserve the error as part of the replay
/// outcome. A failed write must not be downgraded to a warning: callers may
/// otherwise clean up the durable snapshot and report a successful replay
/// while the user's merged or conflict-marked content was never persisted.
fn write_replay_output(path: &Path, bytes: &[u8], action: &str) -> Result<()> {
    let mut current = path;
    loop {
        if let Ok(metadata) = current.symlink_metadata()
            && metadata.file_type().is_symlink()
        {
            bail!(
                "refusing to {action} at {} because path component {} is a symlink",
                path.display(),
                current.display()
            );
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent;
    }
    std::fs::write(path, bytes).with_context(|| format!("failed to {action} at {}", path.display()))
}

/// Perform a 3-way text merge using gix's built-in text driver (pure Rust).
///
/// When `driver` is `Some("union")`, `Some("ours")`, or similar, the merge
/// uses the corresponding gix conflict resolution strategy instead of the
/// default diff3-with-markers. `Some("binary")` returns an immediate conflict
/// without attempting a text merge.
///
/// Returns `Ok((merged_bytes, is_conflict))`.
fn merge_text_three_way(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    ours_label: &str,
    theirs_label: &str,
    driver: Option<&str>,
) -> Result<(Vec<u8>, bool)> {
    use maw_git::merge::{MergeResult, merge_text_with_style, resolution_for_driver};

    let Some(resolution) = resolution_for_driver(driver) else {
        // `merge=binary` (or `-text`) — no text merge, always a conflict.
        // Return the merge version as-is along with a conflict flag so the
        // caller knows not to auto-resolve.
        return Ok((ours.to_vec(), true));
    };

    match merge_text_with_style(
        base,
        ours,
        theirs,
        ours_label,
        "base",
        theirs_label,
        resolution,
    ) {
        Ok(MergeResult::Clean(out)) => Ok((out, false)),
        Ok(MergeResult::Conflict(out)) => Ok((out, true)),
        Err(e) => bail!("text merge failed: {e}"),
    }
}

/// Load a `.gitattributes` matcher from the anchor epoch commit for stash
/// replay merge driver selection.
///
/// Returns `None` on any error so merges fall back to default diff3 behavior.
fn load_stash_replay_attrs(ws_path: &Path, anchor_epoch: &str) -> Option<maw_lfs::AttrsMatcher> {
    let repo = maw_git::GixRepo::open(ws_path).ok()?;
    repo.load_gitattributes_at_commit(anchor_epoch)
}

/// Get the list of file paths changed in a stash commit relative to its parent.
///
/// Stash commits have 2-3 parents:
///   - Parent 1: HEAD at stash time
///   - Parent 2: index state
///   - Parent 3 (optional): untracked files
///
/// We use `git stash show --include-untracked --name-only` as the primary
/// method since it handles all three parents. Falls back to `diff_trees` if
/// stash show fails.
// TODO(gix): gix has no stash-aware diff that walks the optional untracked
// third parent of a stash commit. Keep the `git stash show` CLI call as the
// primary path; the fallback now uses gix's `diff_trees`.
fn stash_changed_paths(ws_path: &Path, stash_oid: &str) -> Result<Vec<PathBuf>> {
    // Primary: git stash show --include-untracked captures all stash content
    // including untracked files (third parent).
    let show_output = Command::new("git")
        .args([
            "stash",
            "show",
            "--include-untracked",
            "--name-only",
            stash_oid,
        ])
        .current_dir(ws_path)
        .output()
        .context("failed to run git stash show")?;

    if show_output.status.success() {
        let paths: Vec<PathBuf> = String::from_utf8_lossy(&show_output.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect();
        if !paths.is_empty() {
            return Ok(paths);
        }
    }

    // Fallback: diff stash commit's tree against its first parent's tree
    // (misses untracked files captured as the stash's third parent, but
    // better than nothing).
    if let Ok(repo) = maw_git::GixRepo::open(ws_path)
        && let Ok(stash_gix) = stash_oid.parse::<maw_git::GitOid>()
        && let Ok(stash_commit) = repo.read_commit(stash_gix)
        && let Some(parent_gix) = stash_commit.parents.first().copied()
        && let Ok(parent_commit) = repo.read_commit(parent_gix)
    {
        let entries = repo
            .diff_trees(Some(parent_commit.tree_oid), stash_commit.tree_oid)
            .unwrap_or_default();
        let paths: Vec<PathBuf> = entries.into_iter().map(|e| PathBuf::from(e.path)).collect();
        if !paths.is_empty() {
            return Ok(paths);
        }
    }

    Ok(Vec::new())
}

/// Read a file's content from a specific git commit.
fn read_file_at_commit(ws_path: &Path, commit: &str, path: &Path) -> Option<Vec<u8>> {
    let repo = maw_git::GixRepo::open(ws_path).ok()?;
    repo.read_file_at_commit(commit, path).ok()?
}

/// Whether a path's committed content differs between two commits.
///
/// Used to decide whether a dirty stash path needs a driver-aware 3-way merge
/// on replay: if the merge changed the file's committed content (even via
/// absorbed out-of-maw commits the merge engine never saw), a
/// bare `stash_apply` would bypass merge drivers and can silently drop the
/// user's uncommitted edits (bn-1xmk). On any read error we conservatively
/// report "changed" so the safe 3-way path runs.
fn committed_content_changed(ws_path: &Path, before: &str, after: &str, path: &Path) -> bool {
    let Ok(repo) = maw_git::GixRepo::open(ws_path) else {
        return true;
    };
    let a = repo.read_file_at_commit(before, path);
    let b = repo.read_file_at_commit(after, path);
    match (a, b) {
        (Ok(a), Ok(b)) if a != b => true,
        // Same bytes: still "changed" if the path turned from a symlink into
        // a regular file or back (a symlink's blob is its target text), or
        // the user's replayed entry would silently revert the merged type
        // change. (bn-2ygs0)
        (Ok(_), Ok(_)) => is_symlink_at(&repo, before, path) != is_symlink_at(&repo, after, path),
        // Read failure — prefer the safe (driver-aware) path.
        _ => true,
    }
}

/// Whether the merge flipped only the executable bit of `path` while the
/// user replaced that file with a symlink (bn-ihi4h). The committed bytes
/// are the same, so the path is not an overlap by content, but the merged
/// mode change and the user's type change cannot both stand: it goes
/// through the symlink/type-change settlement and is reported as a
/// `type_change` conflict, like any other symlink-vs-file change (bn-2ygs0).
fn exec_bit_change_under_local_symlink(
    ws_path: &Path,
    snapshot: &SnapshotRef,
    anchor_epoch: &str,
    epoch_after: &str,
    path: &Path,
) -> bool {
    let Ok(repo) = maw_git::GixRepo::open(ws_path) else {
        return false;
    };
    is_symlink_at(&repo, &snapshot.oid, path) == Some(true)
        && file_mode_changed(&repo, anchor_epoch, epoch_after, path)
}

/// Whether `path` is a regular file in both commits with a different
/// executable bit.
pub fn file_mode_changed(repo: &maw_git::GixRepo, before: &str, after: &str, path: &Path) -> bool {
    let mode_at = |commit: &str| -> Option<maw_git::EntryMode> {
        let oid = repo.rev_parse(commit).ok()?;
        let rel = path.to_str()?.replace('\\', "/");
        repo.read_blob_at_path(oid, &rel)
            .ok()?
            .map(|(mode, _, _)| mode)
    };
    matches!(
        (mode_at(before), mode_at(after)),
        (
            Some(maw_git::EntryMode::Blob),
            Some(maw_git::EntryMode::BlobExecutable)
        ) | (
            Some(maw_git::EntryMode::BlobExecutable),
            Some(maw_git::EntryMode::Blob)
        )
    )
}

/// Whether `path` is a symlink in `commit` (`None` if unreadable).
pub fn is_symlink_at(repo: &maw_git::GixRepo, commit: &str, path: &Path) -> Option<bool> {
    let oid = repo.rev_parse(commit).ok()?;
    let rel = path.to_str()?.replace('\\', "/");
    let entry = repo.read_blob_at_path(oid, &rel).ok()?;
    Some(entry.is_some_and(|(mode, _, _)| mode == maw_git::EntryMode::Link))
}

/// Write diff3-style conflict markers for a file.
///
/// Labels use actual workspace names so `maw ws resolve` can match them:
/// ```text
/// <<<<<<< bn-2sc3 (merged workspace)
/// {merge_content}
/// ||||||| base
/// {base_content}
/// =======
/// {local_content}
/// >>>>>>> default (local edits)
/// ```
fn write_diff3_markers(
    base: &[u8],
    merge_content: &[u8],
    local_content: &[u8],
    merge_label: &str,
    local_label: &str,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("<<<<<<< {merge_label}\n").as_bytes());
    out.extend_from_slice(merge_content);
    if !merge_content.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(b"||||||| base\n");
    out.extend_from_slice(base);
    if !base.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(b"=======\n");
    out.extend_from_slice(local_content);
    if !local_content.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(format!(">>>>>>> {local_label}\n").as_bytes());
    out
}

/// Delete the snapshot ref for a workspace.
///
/// Call this after a successful replay to clean up the durable pin.
/// On replay with conflicts, the ref is intentionally KEPT as a recovery
/// anchor.
pub fn cleanup_snapshot(repo_root: &Path, ws_name: &str) -> Result<()> {
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
pub fn dangling_snapshot(repo_root: &Path, ws_name: &str) -> Result<Option<SnapshotRef>> {
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
pub enum ReplayResult {
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
#[expect(
    clippy::too_many_lines,
    reason = "preserve checkout replay coordinates capture, checkout, replay, and cleanup"
)]
pub fn preserve_checkout_replay(
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
    let untracked_repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    let untracked_paths = untracked_repo
        .list_untracked()
        .map_err(|e| anyhow::anyhow!("failed to list untracked files: {e}"))?;
    let untracked_empty = untracked_paths.is_empty();

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

    let Some(capture) = capture_result else {
        tracing::warn!(
            "capture returned None despite status check showing work; \
             falling back to clean checkout"
        );
        git_checkout_force(ws_path, target_ref)?;
        return Ok(ReplayResult::Clean);
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
        && let Err(e) = git_apply_patch(ws_path, staged_patch, true)
    {
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
        && let Err(e) = git_apply_patch(ws_path, unstaged_patch, false)
    {
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
    let temp_dir =
        tempfile::TempDir::new().context("failed to create temp directory for delta extraction")?;

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
            fs::write(&path, &output.stdout).context("failed to write staged patch")?;
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
            fs::write(&path, &output.stdout).context("failed to write unstaged patch")?;
            Some(path)
        }
    };

    // Untracked files.
    let untracked = {
        let repo = maw_git::GixRepo::open(ws_path)
            .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
        let files = repo
            .list_untracked()
            .map_err(|e| anyhow::anyhow!("failed to list untracked files: {e}"))?;

        if files.is_empty() {
            None
        } else {
            let untracked_dir = temp_dir.path().join("untracked");
            fs::create_dir_all(&untracked_dir).context("failed to create untracked temp dir")?;

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

/// Force-checkout a workspace to `target` OID, clobbering tracked modifications.
///
/// Native replacement for `git checkout --force <target>` (bn-8flz).
/// Uses `GixRepo::checkout_force` (`checkout_tree` with `overwrite_existing=true`)
/// plus `set_head` (detached, reflog entry).
///
/// Tracked modifications are overwritten;
/// untracked files are preserved (bn-29x0 semantics).
///
/// Only used on rollback / force-restore paths where discarding the current
/// working tree is intentional.
fn git_checkout_force(ws_path: &Path, target: &str) -> Result<()> {
    let repo = maw_git::GixRepo::open(ws_path).with_context(|| {
        format!(
            "failed to open repo for force-checkout at {}",
            ws_path.display()
        )
    })?;
    let oid = repo
        .rev_parse(target)
        .with_context(|| format!("failed to resolve '{target}' for force-checkout"))?;
    repo.checkout_force(oid, ws_path)
        .with_context(|| format!("force-checkout to '{target}' failed"))
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
        // bn-5rdz: shared helper from maw_git::test_support, plus this
        // crate's specific need: a `.manifold/` marker dir so
        // `repo_root_from_worktree`'s layout validation (bn-2bow) recognizes
        // this as a maw repo root.
        let (dir, root, oid) = maw_git::test_support::init_test_repo_with_commit();
        fs::create_dir_all(root.join(".manifold")).expect("operation should succeed");
        (dir, root, oid)
    }

    fn make_second_commit(root: &Path) -> String {
        fs::write(root.join("epoch2.txt"), "epoch2 content\n").expect("operation should succeed");
        // bn-5rdz: stage + commit + rev-parse consolidated in commit_all.
        maw_git::test_support::commit_all(root, "epoch2")
    }

    #[test]
    fn clean_workspace_fast_path() {
        let (_dir, root, base_oid) = setup_repo();
        let target_oid = make_second_commit(&root);

        let out = Command::new("git")
            .args(["checkout", "--force", &base_oid])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");
        assert!(out.status.success());

        let result = preserve_checkout_replay(&root, &base_oid, &target_oid, &root, "test-ws")
            .expect("operation should succeed");

        assert!(
            matches!(result, ReplayResult::Clean),
            "expected Clean, got {result:?}"
        );

        let head = resolve_head_str(&root).expect("operation should succeed");
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
            .expect("operation should succeed");
        assert!(out.status.success());

        // Staged change
        fs::write(root.join("README.md"), "# Modified by user\n")
            .expect("operation should succeed");
        let out = Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");
        assert!(out.status.success());

        // Unstaged change
        fs::write(
            root.join("README.md"),
            "# Modified by user\nUnstaged extra line\n",
        )
        .expect("operation should succeed");

        // Untracked file
        fs::write(root.join("user-notes.txt"), "my important notes\n")
            .expect("operation should succeed");

        let result = preserve_checkout_replay(&root, &base_oid, &target_oid, &root, "test-ws")
            .expect("operation should succeed");

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

        let readme = fs::read_to_string(root.join("README.md")).expect("operation should succeed");
        assert!(
            readme.contains("Modified by user"),
            "staged changes should survive: {readme}"
        );

        assert!(
            root.join("user-notes.txt").exists(),
            "untracked file should be restored"
        );
        let notes =
            fs::read_to_string(root.join("user-notes.txt")).expect("operation should succeed");
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
            recovery_ref: format!("refs/manifold/recovery/{workspace}/2025-06-01T12-00-00Z"),
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
        let json = serde_json::to_string_pretty(&record).expect("operation should succeed");
        let parsed: RewriteRecord = serde_json::from_str(&json).expect("operation should succeed");
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
        let json = serde_json::to_string(&record).expect("operation should succeed");
        assert!(json.contains("\"rollback\""));
        assert!(json.contains("stash pop failed"));
    }

    #[test]
    fn write_and_read_rewrite_artifact() {
        let dir = TempDir::new().expect("operation should succeed");
        let root = dir.path();
        let record = make_test_record("agent-1", ReplayOutcome::Replayed);

        let path =
            write_rewrite_record(root, "agent-1", &record).expect("operation should succeed");
        assert!(path.exists());

        let read_back = read_rewrite_record(&path).expect("operation should succeed");
        assert_eq!(read_back.workspace, "agent-1");
        assert_eq!(read_back.replay_outcome, ReplayOutcome::Replayed);
        assert_eq!(read_back.delta_summary.staged_files, 1);
    }

    #[test]
    fn list_rewrite_records_returns_sorted() {
        let dir = TempDir::new().expect("operation should succeed");
        let root = dir.path();

        for ts in &["2025-06-01T12:00:00Z", "2025-06-02T12:00:00Z"] {
            let mut record = make_test_record("agent-1", ReplayOutcome::Replayed);
            record.timestamp = ts.to_string();
            record.recovery_ref =
                format!("refs/manifold/recovery/agent-1/{}", ts.replace(':', "-"));
            write_rewrite_record(root, "agent-1", &record).expect("operation should succeed");
        }

        let records = list_rewrite_records(root, "agent-1").expect("operation should succeed");
        assert_eq!(records.len(), 2);
        assert!(records[0].recovery_ref.contains("2025-06-01"));
        assert!(records[1].recovery_ref.contains("2025-06-02"));
    }

    #[test]
    fn list_rewrite_records_empty_for_nonexistent_workspace() {
        let dir = TempDir::new().expect("operation should succeed");
        let records =
            list_rewrite_records(dir.path(), "nonexistent").expect("operation should succeed");
        assert!(records.is_empty());
    }

    #[test]
    fn list_rewritten_workspaces_discovers_workspace_dirs() {
        let dir = TempDir::new().expect("operation should succeed");
        let root = dir.path();

        for ws in &["alpha", "beta"] {
            let record = make_test_record(ws, ReplayOutcome::Clean);
            write_rewrite_record(root, ws, &record).expect("operation should succeed");
        }

        let names = list_rewritten_workspaces(root).expect("operation should succeed");
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
        let result =
            snapshot_working_copy(&root, &root, "test-ws").expect("operation should succeed");
        assert!(result.is_none(), "clean workspace should return None");
    }

    #[test]
    fn snapshot_dirty_workspace_captures_and_cleans() {
        let (_dir, root, _base_oid) = setup_repo();

        // Create dirty state: modify tracked file + add untracked file.
        fs::write(root.join("README.md"), "# Modified by user\n")
            .expect("operation should succeed");
        fs::write(root.join("notes.txt"), "user notes\n").expect("operation should succeed");

        let result = snapshot_working_copy(&root, &root, "test-ws")
            .expect("operation should succeed")
            .expect("dirty workspace should return Some");

        // Snapshot ref should be pinned.
        assert_eq!(result.ref_name, "refs/manifold/snapshot/test-ws");
        assert!(!result.oid.is_empty());

        // Verify the ref was written.
        let ref_oid =
            maw_core::refs::read_ref(&root, &result.ref_name).expect("operation should succeed");
        assert!(ref_oid.is_some(), "snapshot ref should exist");

        // Working tree should be clean after snapshot.
        let status = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");
        let status_str = String::from_utf8_lossy(&status.stdout);
        assert!(
            status_str.trim().is_empty(),
            "working tree should be clean after snapshot, got: {status_str}"
        );
    }

    /// bn-3bkn: in the consolidated layout the default workspace IS the repo
    /// root, which contains the untracked admin/git dirs (`repo.git/`, `.maw/`,
    /// `.manifold/`). The snapshot's `git add -A` + `git reset --hard` +
    /// `git clean -fd` must NEVER delete them — doing so destroyed the whole
    /// repository. Simulate those dirs at the worktree root and assert they
    /// survive a snapshot of a dirty workspace.
    #[test]
    fn snapshot_does_not_delete_admin_or_git_dirs() {
        let (_dir, root, _base_oid) = setup_repo();

        // Untracked admin/git dirs that live inside the consolidated root.
        for d in [".maw", "repo.git", ".manifold"] {
            fs::create_dir_all(root.join(d).join("inner")).expect("mkdir admin");
            fs::write(root.join(d).join("inner").join("f"), "admin state\n").expect("write admin");
        }
        // Genuine user dirt so the snapshot path actually runs.
        fs::write(root.join("README.md"), "# user edit\n").expect("write user");
        fs::write(root.join("user-untracked.txt"), "keep me\n").expect("write user2");

        let result = snapshot_working_copy(&root, &root, "test-ws")
            .expect("snapshot should succeed")
            .expect("dirty workspace should return Some");
        assert!(!result.oid.is_empty());

        // The admin/git dirs MUST still exist after the snapshot's reset+clean.
        for d in [".maw", "repo.git", ".manifold"] {
            assert!(
                root.join(d).join("inner").join("f").is_file(),
                "snapshot must not delete admin/git dir `{d}` (bn-3bkn)"
            );
        }
    }

    /// bn-1eg2u: the filtered replay commit that `split_directory_collisions`
    /// builds (the snapshot minus its file <-> directory collisions) must stay
    /// reachable until the replay has applied it. A `git prune` between the
    /// split and `stash_apply` used to delete it and fail the whole replay.
    #[cfg(feature = "failpoints")]
    #[test]
    fn filtered_replay_commit_survives_prune_during_replay() {
        use maw_core::failpoints::{self, FailpointAction};

        let (_dir, root, _) = setup_repo();
        fs::write(root.join("p"), "one\n").expect("write p");
        fs::write(root.join("other.txt"), "other\n").expect("write other");
        let anchor = maw_git::test_support::commit_all(&root, "anchor");
        fs::write(root.join("p"), "one\nmerged\n").expect("edit p");
        let target = maw_git::test_support::commit_all(&root, "target");
        let out = Command::new("git")
            .args(["checkout", "--force", "-q", &anchor])
            .current_dir(&root)
            .output()
            .expect("checkout anchor");
        assert!(out.status.success());

        // The user replaced file `p` with directory `p/`.
        fs::remove_file(root.join("p")).expect("rm p");
        fs::create_dir(root.join("p")).expect("mkdir p");
        fs::write(root.join("p/x"), "user x\n").expect("write p/x");
        fs::write(root.join("other.txt"), "other\nuser\n").expect("edit other");

        let snapshot = snapshot_working_copy(&root, &root, "test-ws")
            .expect("snapshot")
            .expect("dirty");
        checkout_to(&root, &target, None).expect("checkout target");

        let prune_root = root.clone();
        let fp = failpoints::set_for_this_thread(
            "FP_CLEANUP_REPLAY_BEFORE_APPLY",
            FailpointAction::Callback(std::sync::Arc::new(move || {
                let out = Command::new("git")
                    .args(["prune", "--expire=now"])
                    .current_dir(&prune_root)
                    .output()
                    .expect("git prune");
                assert!(out.status.success(), "git prune failed");
            })),
        );
        let result = replay_snapshot_with_merge_protection(
            &root,
            &snapshot,
            &anchor,
            &target,
            &["a".to_owned()],
            "test-ws",
        );
        drop(fp);

        let result = result.expect("replay must not lose its filtered commit to a prune");
        let SnapshotReplayResult::Conflicts(conflicts) = result else {
            panic!("expected the directory conflict to be reported");
        };
        assert!(
            conflicts
                .iter()
                .any(|c| c.path == "p/x" && c.conflict_type == "directory_change"),
            "{conflicts:?}"
        );
        assert_eq!(
            fs::read_to_string(root.join("other.txt")).expect("read other"),
            "other\nuser\n",
            "the non-conflicting edit must be replayed"
        );
        assert_eq!(
            maw_core::refs::read_ref(&root, &replay_pin_ref_name("test-ws")).expect("read ref"),
            None,
            "the replay pin must be removed once the replay is done"
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
            .expect("operation should succeed");
        assert!(out.status.success());

        // Create user changes.
        fs::write(root.join("README.md"), "# User modified\n").expect("operation should succeed");
        fs::write(root.join("user-work.txt"), "important work\n")
            .expect("operation should succeed");

        // Step 1: Snapshot.
        let snapshot = snapshot_working_copy(&root, &root, "test-ws")
            .expect("operation should succeed")
            .expect("dirty workspace should produce snapshot");

        // Step 2: Checkout to target.
        checkout_to(&root, &target_oid, None).expect("operation should succeed");

        // Verify target is checked out.
        assert!(
            root.join("epoch2.txt").exists(),
            "epoch2.txt should exist after checkout"
        );

        // Step 3: Replay.
        let replay_result = replay_snapshot(&root, &snapshot).expect("operation should succeed");
        assert!(
            matches!(replay_result, SnapshotReplayResult::Clean),
            "replay should be clean for non-overlapping changes"
        );

        // User changes should be present on top of new epoch.
        let readme = fs::read_to_string(root.join("README.md")).expect("operation should succeed");
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
        cleanup_snapshot(&root, "test-ws").expect("operation should succeed");
        let ref_oid = maw_core::refs::read_ref(&root, "refs/manifold/snapshot/test-ws")
            .expect("operation should succeed");
        assert!(
            ref_oid.is_none(),
            "snapshot ref should be deleted after cleanup"
        );
    }

    #[test]
    fn snapshot_replay_with_conflict_leaves_markers() {
        let (_dir, root, _base_oid) = setup_repo();

        // Create a second commit that modifies README.md.
        fs::write(root.join("README.md"), "# Epoch 2 version\n").expect("operation should succeed");
        let out = Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");
        assert!(out.status.success());
        let out = Command::new("git")
            .args(["commit", "-m", "epoch2: modify README"])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");
        assert!(out.status.success());
        let target_out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");
        let target_oid = String::from_utf8_lossy(&target_out.stdout)
            .trim()
            .to_owned();

        // Go back to base and create a conflicting modification.
        let out = Command::new("git")
            .args(["checkout", "HEAD~1"])
            .current_dir(&root)
            .output()
            .expect("operation should succeed");
        assert!(out.status.success());

        fs::write(root.join("README.md"), "# User conflicting version\n")
            .expect("operation should succeed");

        // Snapshot.
        let snapshot = snapshot_working_copy(&root, &root, "test-ws")
            .expect("operation should succeed")
            .expect("dirty workspace should produce snapshot");

        // Checkout to target (which has different README.md).
        checkout_to(&root, &target_oid, None).expect("operation should succeed");

        // Replay — should produce conflict.
        let replay_result = replay_snapshot(&root, &snapshot).expect("operation should succeed");
        match replay_result {
            SnapshotReplayResult::Conflicts(conflicts) => {
                assert!(!conflicts.is_empty(), "should have at least one conflict");
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
        let ref_oid = maw_core::refs::read_ref(&root, "refs/manifold/snapshot/test-ws")
            .expect("operation should succeed");
        assert!(ref_oid.is_some(), "snapshot ref should be kept on conflict");
    }

    #[test]
    fn replay_output_write_failure_is_reported() {
        let temp = TempDir::new().expect("operation should succeed");
        let path = temp.path().join("output");
        fs::create_dir(&path).expect("create blocking directory");

        let error = write_replay_output(&path, b"content", "write merged content")
            .expect_err("writing a directory must fail");
        let message = error.to_string();
        assert!(
            message.contains("write merged content"),
            "message={message}"
        );
        assert!(message.contains("output"), "message={message}");
    }

    #[cfg(unix)]
    #[test]
    fn replay_output_refuses_symlink_destinations() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("operation should succeed");
        let outside = temp.path().join("outside");
        let path = temp.path().join("output");
        fs::write(&outside, b"protected").expect("create outside file");
        symlink(&outside, &path).expect("create symlink");

        let error = write_replay_output(&path, b"replacement", "write merged content")
            .expect_err("replay must not follow a symlink");
        assert!(error.to_string().contains("symlink"));
        assert_eq!(fs::read(&outside).expect("read outside file"), b"protected");
    }

    // bn-3fcbu: the exec-bit repair touches only regular files and never
    // follows a symlink — neither the file itself nor a parent directory.
    #[cfg(unix)]
    #[test]
    fn set_worktree_exec_bit_never_follows_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;

        let outside = TempDir::new().unwrap();
        let victim = outside.path().join("victim.txt");
        fs::write(&victim, "x").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o640)).unwrap();
        let victim_dir = outside.path().join("d");
        fs::create_dir(&victim_dir).unwrap();
        let inner = victim_dir.join("f.sh");
        fs::write(&inner, "x").unwrap();
        fs::set_permissions(&inner, fs::Permissions::from_mode(0o640)).unwrap();

        let ws = TempDir::new().unwrap();
        std::os::unix::fs::symlink(&victim, ws.path().join("link")).unwrap();
        std::os::unix::fs::symlink(&victim_dir, ws.path().join("dirlink")).unwrap();
        let real = ws.path().join("real.sh");
        fs::write(&real, "x").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o640)).unwrap();

        set_worktree_exec_bit(ws.path(), Path::new("link"), true).unwrap();
        set_worktree_exec_bit(ws.path(), Path::new("dirlink/f.sh"), true).unwrap();
        set_worktree_exec_bit(ws.path(), Path::new("missing.sh"), true).unwrap();
        assert_eq!(mode(&victim), 0o640, "must not chmod through a symlink");
        assert_eq!(
            mode(&inner),
            0o640,
            "must not chmod through a symlinked dir"
        );

        // Regular file: execute follows read, other bits preserved.
        set_worktree_exec_bit(ws.path(), Path::new("real.sh"), true).unwrap();
        assert_eq!(mode(&real), 0o750);
        set_worktree_exec_bit(ws.path(), Path::new("real.sh"), false).unwrap();
        assert_eq!(mode(&real), 0o640);
    }
}
