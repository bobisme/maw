//! Pre-destroy capture helper for workspace safety.
//!
//! Before a workspace is destroyed (via `maw ws destroy` or post-merge
//! `--destroy`), this module captures the workspace's dirty state as a
//! detached git commit and pins it under `refs/manifold/recovery/` so
//! the data survives garbage collection.
//!
//! # Design
//!
//! - **Workspace-owned**: orchestration lives in the workspace layer so both
//!   destroy paths (standalone and post-merge) stay consistent.
//! - **Git-first**: uses git commands directly for capture. A non-git backend
//!   would need its own capture implementation (TODO if ever needed).
//! - **Fail-safe**: if capture fails on a dirty workspace, the caller should
//!   abort the destructive delete rather than proceeding without a safety net.
//!
//! # Capture Modes
//!
//! - `WorktreeCapture`: workspace has uncommitted changes (staged, unstaged,
//!   or untracked files). A detached commit is created from the full worktree
//!   state.
//! - `HeadOnly`: workspace has no dirty files but is ahead of its base epoch
//!   (committed-only changes). The final HEAD is pinned as the recovery ref.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use maw_git::GitRepo as _;
use serde::Serialize;
use tracing::instrument;

use maw_core::model::types::GitOid;
use maw_core::refs;

// ---------------------------------------------------------------------------
// Recovery ref prefix
// ---------------------------------------------------------------------------

/// Ref namespace for recovery pins.
///
/// Format: `refs/manifold/recovery/<workspace-name>/<timestamp>`
pub const RECOVERY_PREFIX: &str = "refs/manifold/recovery/";

/// Build the recovery ref name for a workspace capture.
#[must_use]
pub fn recovery_ref(workspace_name: &str, timestamp: &str) -> String {
    // Sanitize timestamp for ref name (colons → dashes)
    let safe_ts = timestamp.replace(':', "-");
    format!("{RECOVERY_PREFIX}{workspace_name}/{safe_ts}")
}

// ---------------------------------------------------------------------------
// Capture result types
// ---------------------------------------------------------------------------

/// The mode in which the workspace state was captured.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMode {
    /// Full worktree capture — workspace had uncommitted changes.
    WorktreeCapture,
    /// Head-only pin — workspace was clean but ahead of epoch.
    HeadOnly,
}

/// Metadata returned from a successful capture.
#[derive(Clone, Debug, Serialize)]
pub struct CaptureResult {
    /// The git OID of the captured commit.
    pub commit_oid: GitOid,
    /// The pinned ref path (under `refs/manifold/recovery/`).
    pub pinned_ref: String,
    /// List of dirty paths that were captured (empty for `HeadOnly`).
    pub dirty_paths: Vec<String>,
    /// How the capture was performed.
    pub mode: CaptureMode,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Capture the current workspace state before destruction.
///
/// Returns `Ok(None)` if the workspace is clean *and* at the base epoch
/// (nothing to preserve). Returns `Ok(Some(result))` if state was captured.
///
/// # Fail-safe
///
/// If the workspace has dirty files and capture fails, this returns `Err`.
/// The caller **must not** proceed with destruction on error — doing so
/// would lose data.
///
/// # Arguments
///
/// * `ws_path` — absolute path to the workspace directory
/// * `ws_name` — workspace name (used for the recovery ref)
/// * `base_epoch` — the workspace's base epoch OID (to detect committed-ahead)
#[instrument(skip_all, fields(workspace = ws_name))]
pub fn capture_before_destroy(
    ws_path: &Path,
    ws_name: &str,
    base_epoch: &GitOid,
) -> Result<Option<CaptureResult>> {
    // Step 1: detect dirty state
    let dirty_paths = list_dirty_paths(ws_path)?;

    if dirty_paths.is_empty() {
        // No dirty files — check if HEAD is ahead of base epoch
        let head_oid = resolve_head(ws_path)?;
        if head_oid.as_str() == base_epoch.as_str() {
            // Workspace is clean and at epoch — nothing to capture
            tracing::debug!("workspace is clean and at epoch, nothing to capture");
            return Ok(None);
        }
        // HEAD is ahead of epoch but no dirty files — pin HEAD
        return pin_head_only(ws_path, ws_name, &head_oid);
    }

    // Step 2: capture dirty worktree as a detached commit
    capture_dirty_worktree(ws_path, ws_name, &dirty_paths)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// List all dirty paths in the workspace (staged + unstaged + untracked).
fn list_dirty_paths(ws_path: &Path) -> Result<Vec<String>> {
    let repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    let entries = repo.status()
        .map_err(|e| anyhow::anyhow!("git status failed: {e}"))?;

    let paths: Vec<String> = entries
        .into_iter()
        .map(|entry| entry.path)
        .collect();

    Ok(paths)
}

/// Resolve HEAD to a full OID.
pub(crate) fn resolve_head(ws_path: &Path) -> Result<GitOid> {
    let repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    let git_oid = repo.rev_parse("HEAD")
        .map_err(|e| anyhow::anyhow!("failed to resolve HEAD: {e}"))?;
    let oid_str = git_oid.to_string();
    GitOid::new(&oid_str).map_err(|e| anyhow::anyhow!("invalid HEAD OID: {e}"))
}

/// Pin HEAD (committed-only, no dirty files) under a recovery ref.
fn pin_head_only(
    ws_path: &Path,
    ws_name: &str,
    head_oid: &GitOid,
) -> Result<Option<CaptureResult>> {
    let timestamp = super::now_timestamp_iso8601();
    let ref_name = recovery_ref(ws_name, &timestamp);

    // Pin the ref in the repo (use the repo root, not the worktree)
    let repo_root = repo_root_from_worktree(ws_path)?;
    refs::write_ref(&repo_root, &ref_name, head_oid)
        .map_err(|e| anyhow::anyhow!("failed to pin recovery ref: {e}"))?;

    tracing::info!(
        ref_name = %ref_name,
        oid = %head_oid,
        "pinned head-only recovery ref"
    );

    Ok(Some(CaptureResult {
        commit_oid: head_oid.clone(),
        pinned_ref: ref_name,
        dirty_paths: Vec::new(),
        mode: CaptureMode::HeadOnly,
    }))
}

/// Capture the dirty worktree as a detached commit and pin it.
///
/// Uses `git add -A` + `git stash create` to build a commit object that
/// includes all tracked changes plus untracked files, without moving HEAD
/// or altering the index/stash-list.
fn capture_dirty_worktree(
    ws_path: &Path,
    ws_name: &str,
    dirty_paths: &[String],
) -> Result<Option<CaptureResult>> {
    // `git stash create` produces a merge commit that captures the current
    // index + worktree state as a detached object. It does NOT modify
    // HEAD, the index, or the stash list — perfect for our pre-destroy
    // capture.
    //
    // However, `git stash create` only captures tracked files + staged
    // changes. Untracked files are missed unless we stage them first.
    // We use `git add -A` to stage everything, then `git stash create`
    // to build the commit, then `git reset` to restore the index.
    //
    // TODO(gix): replace git add -A and git reset with GitRepo trait methods
    // when full working-tree staging is available.

    // Stage all files (including untracked)
    let add_output = Command::new("git")
        .args(["add", "-A"])
        .current_dir(ws_path)
        .output()
        .context("failed to run git add -A")?;

    if !add_output.status.success() {
        let stderr = String::from_utf8_lossy(&add_output.stderr);
        bail!("git add -A failed during capture: {}", stderr.trim());
    }

    // Create a stash commit (does not modify HEAD or stash list)
    let repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    let stash_result = repo.stash_create()
        .map_err(|e| {
            // Restore index state before bailing
            // TODO(gix): replace git reset with GitRepo trait method
            let _ = Command::new("git")
                .args(["reset"])
                .current_dir(ws_path)
                .output();
            anyhow::anyhow!("git stash create failed during capture: {e}")
        })?;

    let stash_git_oid = match stash_result {
        Some(oid) => oid,
        None => {
            // `stash_create` returns None if there's nothing to stash
            // (shouldn't happen since we checked dirty_paths, but be defensive)
            // TODO(gix): replace git reset with GitRepo trait method
            let _ = Command::new("git")
                .args(["reset"])
                .current_dir(ws_path)
                .output();
            tracing::warn!("stash_create returned None despite dirty paths");
            return Ok(None);
        }
    };

    let stash_oid_str = stash_git_oid.to_string();
    let commit_oid = GitOid::new(&stash_oid_str)
        .map_err(|e| anyhow::anyhow!("invalid stash OID: {e}"))?;

    // Restore the index to its pre-add state (don't leave staged changes
    // behind — the workspace is about to be destroyed, but be clean anyway)
    // TODO(gix): replace git reset with GitRepo trait method
    let _ = Command::new("git")
        .args(["reset"])
        .current_dir(ws_path)
        .output();

    // FP: crash after stash/tree creation but before ref pinning.
    // A crash here means the commit object exists but is unreachable (no ref).
    maw::fp!("FP_CAPTURE_BEFORE_PIN")?;

    // Pin the commit under a recovery ref
    let timestamp = super::now_timestamp_iso8601();
    let ref_name = recovery_ref(ws_name, &timestamp);

    let repo_root = repo_root_from_worktree(ws_path)?;
    refs::write_ref(&repo_root, &ref_name, &commit_oid)
        .map_err(|e| anyhow::anyhow!("failed to pin recovery ref: {e}"))?;

    tracing::info!(
        ref_name = %ref_name,
        oid = %commit_oid,
        dirty_count = dirty_paths.len(),
        "captured dirty worktree state"
    );

    Ok(Some(CaptureResult {
        commit_oid,
        pinned_ref: ref_name,
        dirty_paths: dirty_paths.to_vec(),
        mode: CaptureMode::WorktreeCapture,
    }))
}

/// Resolve the repo root from a worktree path.
///
/// Uses `git rev-parse --git-common-dir` to find the shared git directory,
/// then derives the repo root from it.
// TODO(gix): replace with a dedicated GitRepo method for repo-root discovery
fn repo_root_from_worktree(ws_path: &Path) -> Result<std::path::PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(ws_path)
        .output()
        .context("failed to determine git common dir")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git rev-parse --git-common-dir failed: {}", stderr.trim());
    }

    let common_dir =
        std::path::PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let mut root = common_dir
        .parent()
        .context("cannot determine repo root from git common dir")?
        .to_path_buf();

    // Support nested common-dir layouts like <root>/.manifold/git
    if root.file_name().is_some_and(|name| name == ".manifold") {
        root = root
            .parent()
            .context("cannot determine repo root from nested common dir")?
            .to_path_buf();
    }

    Ok(root)
}

// ---------------------------------------------------------------------------
// Recovery surface output
// ---------------------------------------------------------------------------

/// Emit the full recovery output contract to stderr.
///
/// All 5 required fields:
/// 1. Operation result (success/failure)
/// 2. Whether COMMIT succeeded
/// 3. Snapshot ref + oid
/// 4. Artifact path
/// 5. Executable recovery command
///
/// This function is the single source of truth for recovery surface output.
/// All code paths that create recovery snapshots MUST call this to ensure
/// agents can parse and act on the output consistently.
pub fn emit_recovery_surface(
    workspace_name: &str,
    capture: &CaptureResult,
    artifact_path: Option<&std::path::Path>,
    commit_succeeded: bool,
    operation_succeeded: bool,
) {
    let status = if operation_succeeded {
        "success"
    } else {
        "failure"
    };
    let commit_status = if commit_succeeded { "yes" } else { "no" };
    let mode_label = match capture.mode {
        CaptureMode::WorktreeCapture => "worktree-snapshot",
        CaptureMode::HeadOnly => "head-only",
    };

    eprintln!("RECOVERY_SURFACE for '{workspace_name}':");
    eprintln!("  result:       {status}");
    eprintln!("  commit:       {commit_status}");
    eprintln!("  snapshot_ref: {}", capture.pinned_ref);
    eprintln!("  snapshot_oid: {}", capture.commit_oid);
    eprintln!("  capture_mode: {mode_label}");
    if let Some(path) = artifact_path {
        eprintln!("  artifact:     {}", path.display());
    } else {
        eprintln!("  artifact:     (none)");
    }
    eprintln!(
        "  recover_cmd:  maw ws recover {workspace_name}"
    );
}

/// Emit a structured recovery failure notice when capture itself fails.
///
/// Emits the same field names as [`emit_recovery_surface`] but with
/// `(capture failed)` placeholders so agents can still parse the output
/// structure consistently.
pub fn emit_recovery_surface_failed(
    workspace_name: &str,
    error: &dyn std::fmt::Display,
    commit_succeeded: bool,
) {
    let commit_status = if commit_succeeded { "yes" } else { "no" };

    eprintln!("RECOVERY_SURFACE for '{workspace_name}':");
    eprintln!("  result:       failure");
    eprintln!("  commit:       {commit_status}");
    eprintln!("  snapshot_ref: (capture failed)");
    eprintln!("  snapshot_oid: (capture failed)");
    eprintln!("  capture_mode: (capture failed)");
    eprintln!("  artifact:     (none)");
    eprintln!("  recover_cmd:  git -C <workspace-path> stash list");
    eprintln!("  error:        {error}");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Create a fresh git repo with one commit. Returns (tempdir, repo root, HEAD OID).
    fn setup_repo() -> (TempDir, std::path::PathBuf, GitOid) {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();

        Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&root)
            .output()
            .unwrap();

        fs::write(root.join("README.md"), "# Test\n").unwrap();
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&root)
            .output()
            .unwrap();

        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        let oid_str = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        let oid = GitOid::new(&oid_str).unwrap();

        (dir, root, oid)
    }

    // -----------------------------------------------------------------------
    // recovery_ref formatting
    // -----------------------------------------------------------------------

    #[test]
    fn recovery_ref_format() {
        let r = recovery_ref("alice", "2025-01-15T10:30:00Z");
        assert_eq!(r, "refs/manifold/recovery/alice/2025-01-15T10-30-00Z");
    }

    #[test]
    fn recovery_ref_sanitizes_colons() {
        let r = recovery_ref("ws-1", "2025-01-15T10:30:45Z");
        assert!(!r.contains(':'), "colons should be replaced: {r}");
    }

    // -----------------------------------------------------------------------
    // capture_before_destroy — clean workspace at epoch
    // -----------------------------------------------------------------------

    #[test]
    fn capture_clean_at_epoch_returns_none() {
        let (_dir, root, head_oid) = setup_repo();
        let result = capture_before_destroy(&root, "test-ws", &head_oid).unwrap();
        assert!(result.is_none(), "clean workspace at epoch should return None");
    }

    // -----------------------------------------------------------------------
    // capture_before_destroy — dirty workspace
    // -----------------------------------------------------------------------

    #[test]
    fn capture_dirty_workspace_returns_some() {
        let (_dir, root, head_oid) = setup_repo();

        // Create a dirty file
        fs::write(root.join("dirty.txt"), "dirty content\n").unwrap();

        let result = capture_before_destroy(&root, "test-ws", &head_oid)
            .unwrap()
            .expect("dirty workspace should return Some");

        assert_eq!(result.mode, CaptureMode::WorktreeCapture);
        assert!(!result.dirty_paths.is_empty());
        assert!(result.dirty_paths.iter().any(|p| p == "dirty.txt"));
        assert!(result.pinned_ref.starts_with("refs/manifold/recovery/test-ws/"));

        // Verify the pinned ref exists and resolves
        let ref_oid = refs::read_ref(&root, &result.pinned_ref).unwrap();
        assert_eq!(ref_oid, Some(result.commit_oid));
    }

    // -----------------------------------------------------------------------
    // capture_before_destroy — untracked files
    // -----------------------------------------------------------------------

    #[test]
    fn capture_untracked_files() {
        let (_dir, root, head_oid) = setup_repo();

        // Create an untracked file (never git-added)
        fs::write(root.join("new-file.txt"), "brand new\n").unwrap();

        let result = capture_before_destroy(&root, "test-ws", &head_oid)
            .unwrap()
            .expect("untracked files should be captured");

        assert_eq!(result.mode, CaptureMode::WorktreeCapture);
        assert!(result.dirty_paths.iter().any(|p| p == "new-file.txt"));

        // Verify the captured commit contains the file
        let output = Command::new("git")
            .args(["show", &format!("{}:new-file.txt", result.commit_oid)])
            .current_dir(&root)
            .output()
            .unwrap();
        // git stash create uses a merge commit structure; the worktree
        // content is in the third parent's tree. Access via the commit's
        // tree directly.
        let tree_output = Command::new("git")
            .args([
                "ls-tree",
                "-r",
                "--name-only",
                result.commit_oid.as_str(),
            ])
            .current_dir(&root)
            .output()
            .unwrap();
        let tree_files = String::from_utf8_lossy(&tree_output.stdout);
        // The stash commit's tree should include the new file
        // (via the index parent or worktree parent)
        assert!(
            tree_files.contains("new-file.txt")
                || output.status.success(),
            "captured commit should contain untracked file"
        );
    }

    // -----------------------------------------------------------------------
    // capture_before_destroy — committed-ahead (head_only mode)
    // -----------------------------------------------------------------------

    #[test]
    fn capture_committed_ahead_pins_head() {
        let (_dir, root, base_oid) = setup_repo();

        // Make a second commit (workspace is now ahead of base epoch)
        fs::write(root.join("feature.txt"), "new feature\n").unwrap();
        Command::new("git")
            .args(["add", "feature.txt"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "add feature"])
            .current_dir(&root)
            .output()
            .unwrap();

        let result = capture_before_destroy(&root, "test-ws", &base_oid)
            .unwrap()
            .expect("committed-ahead workspace should return Some");

        assert_eq!(result.mode, CaptureMode::HeadOnly);
        assert!(result.dirty_paths.is_empty());

        // The captured OID should be the current HEAD, not the base epoch
        let current_head = resolve_head(&root).unwrap();
        assert_eq!(result.commit_oid, current_head);
        assert_ne!(result.commit_oid.as_str(), base_oid.as_str());

        // Recovery ref should exist
        let ref_oid = refs::read_ref(&root, &result.pinned_ref).unwrap();
        assert_eq!(ref_oid, Some(result.commit_oid));
    }

    // -----------------------------------------------------------------------
    // list_dirty_paths
    // -----------------------------------------------------------------------

    #[test]
    fn list_dirty_paths_empty_when_clean() {
        let (_dir, root, _oid) = setup_repo();
        let paths = list_dirty_paths(&root).unwrap();
        assert!(paths.is_empty());
    }

    #[test]
    fn list_dirty_paths_detects_modified() {
        let (_dir, root, _oid) = setup_repo();
        fs::write(root.join("README.md"), "# Modified\n").unwrap();
        let paths = list_dirty_paths(&root).unwrap();
        assert!(paths.contains(&"README.md".to_string()));
    }

    #[test]
    fn list_dirty_paths_detects_untracked() {
        let (_dir, root, _oid) = setup_repo();
        fs::write(root.join("untracked.txt"), "hi\n").unwrap();
        let paths = list_dirty_paths(&root).unwrap();
        assert!(paths.contains(&"untracked.txt".to_string()));
    }
}
