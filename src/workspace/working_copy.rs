//! Shared working-copy helpers: stash, checkout, stash-pop, conflict detection.
//!
//! These primitives operate on a git worktree (identified by its path on disk)
//! and are used by `maw ws advance` and potentially by merge cleanup and other
//! code paths that need to manipulate the working copy safely.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;

// ---------------------------------------------------------------------------
// Conflict info
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
// Git helpers
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
