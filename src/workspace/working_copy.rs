//! Working-copy helpers and safe rewrite primitives.
//!
//! Two layers:
//!
//! 1. **Stash-based helpers** (`stash_changes`, `checkout_epoch`,
//!    `pop_stash_and_detect_conflicts`, `detect_conflicts_in_worktree`) — used
//!    by `maw ws advance` and other paths that manipulate the working copy.
//!
//! 2. **`preserve_checkout_replay()`** — the G2-compliant rewrite primitive
//!    that safely transitions a workspace from one epoch to another while
//!    preserving user work (staged, unstaged, and untracked files). This
//!    replaces raw `git checkout --force` which silently destroys dirty state.
//!
//! ## preserve_checkout_replay algorithm
//!
//! 1. Check for user work — if none, fast-path with force checkout.
//! 2. Capture a recovery snapshot (pinned ref via `capture_before_destroy`).
//! 3. Extract user deltas from the explicit base epoch (the anchor invariant).
//! 4. Materialize the target commit via force checkout.
//! 5. Replay staged deltas, then unstaged deltas, then restore untracked files.
//! 6. If replay produces conflicts, rollback to the recovery snapshot.
//!
//! See `notes/assurance/working-copy.md` for the normative specification.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tracing::instrument;

use crate::model::types::GitOid;
use super::capture::capture_before_destroy;

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
// Stash-based helpers
// ---------------------------------------------------------------------------

/// Stash uncommitted changes. Returns `true` if there was something to stash.
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
pub(crate) fn checkout_epoch(ws_path: &Path, epoch_oid: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["checkout", "--detach", epoch_oid])
        .current_dir(ws_path)
        .output()
        .context("Failed to run git checkout --detach")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git checkout --detach failed: {}", stderr.trim());
    }
    Ok(())
}

/// Pop the stash and return a list of conflict entries (if any).
///
/// After `git stash pop` with conflicts, git leaves the working tree in a
/// partially-merged state with conflict markers. We detect conflicts via
/// `git status --porcelain` and parse the two-character status code.
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
// preserve_checkout_replay — G2-compliant rewrite primitive
// ===========================================================================

/// Outcome of a `preserve_checkout_replay()` operation.
#[derive(Clone, Debug)]
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
#[instrument(skip_all, fields(workspace = workspace_name, target = target_ref))]
pub(crate) fn preserve_checkout_replay(
    ws_path: &Path,
    base_epoch: &str,
    target_ref: &str,
    _repo_root: &Path,
    workspace_name: &str,
) -> Result<ReplayResult> {
    // Step 1: Check for user work.
    let status_output = git_status_porcelain(ws_path)?;
    let head_oid = resolve_head_str(ws_path)?;

    if status_output.is_empty() && head_oid == base_epoch {
        tracing::debug!("no user work detected, fast-path checkout");
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
    if let Some(ref staged_patch) = deltas.staged_patch_path {
        if let Err(e) = git_apply_patch(ws_path, staged_patch, true) {
            tracing::warn!("staged patch apply failed: {e}, rolling back");
            let _ = git_checkout_force(ws_path, &recovery_oid);
            return Ok(ReplayResult::Rollback {
                recovery_ref,
                recovery_oid,
                reason: format!("staged patch replay failed: {e}"),
            });
        }
    }

    // Step 6: Replay unstaged deltas (if non-empty).
    if let Some(ref unstaged_patch) = deltas.unstaged_patch_path {
        if let Err(e) = git_apply_patch(ws_path, unstaged_patch, false) {
            tracing::warn!("unstaged patch apply failed: {e}, rolling back");
            let _ = git_checkout_force(ws_path, &recovery_oid);
            return Ok(ReplayResult::Rollback {
                recovery_ref,
                recovery_oid,
                reason: format!("unstaged patch replay failed: {e}"),
            });
        }
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
fn resolve_head_str(ws_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(ws_path)
        .output()
        .context("failed to run git rev-parse HEAD")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git rev-parse HEAD failed: {}", stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Run `git checkout --force <ref>`.
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
}
