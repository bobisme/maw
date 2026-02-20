//! `maw ws advance <name>` — rebase a persistent workspace onto the latest epoch.
//!
//! Persistent workspaces can survive across epoch advances. When the mainline
//! epoch advances, a persistent workspace becomes stale. `maw ws advance` rebases
//! the workspace's uncommitted changes onto the new epoch:
//!
//! 1. Check that the workspace is persistent (mode = persistent).
//! 2. Get the workspace's current HEAD (its base epoch).
//! 3. Get the current epoch from `refs/manifold/epoch/current`.
//! 4. If already up-to-date, exit early.
//! 5. Stash any uncommitted changes in the workspace.
//! 6. Reset the workspace HEAD to the new epoch.
//! 7. Pop the stash to apply changes on top of the new base.
//! 8. Detect and report any conflicts.
//! 9. Update the workspace metadata base epoch.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::format::OutputFormat;
use crate::model::types::WorkspaceMode;
use crate::refs as manifold_refs;

use super::{metadata, repo_root, workspace_path, DEFAULT_WORKSPACE};

// ---------------------------------------------------------------------------
// Conflict info
// ---------------------------------------------------------------------------

/// A single file conflict detected during advance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AdvanceConflict {
    /// Path of the conflicted file, relative to the workspace root.
    pub path: String,
    /// Conflict type: `"content"`, `"deleted_by_us"`, `"deleted_by_them"`,
    /// `"added_by_us"`, `"added_by_them"`, `"both_added"`.
    pub conflict_type: String,
}

/// Result of a `maw ws advance` operation.
#[derive(Clone, Debug, Serialize)]
pub struct AdvanceResult {
    /// Name of the workspace that was advanced.
    pub workspace: String,
    /// Old base epoch (OID before advance).
    pub old_epoch: String,
    /// New base epoch (OID after advance).
    pub new_epoch: String,
    /// Whether the advance completed without conflicts.
    pub success: bool,
    /// Files with conflicts (empty on success).
    pub conflicts: Vec<AdvanceConflict>,
    /// Human-readable summary message.
    pub message: String,
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Run `maw ws advance <name>`.
///
/// Rebases the workspace's uncommitted changes onto the latest epoch.
/// Reports conflicts as structured data if they occur.
#[allow(clippy::too_many_lines)]
pub fn advance(name: &str, format: OutputFormat) -> Result<()> {
    if name == DEFAULT_WORKSPACE {
        bail!(
            "Cannot advance the default workspace — it is always up to date.\n  \
             The default workspace is updated automatically during merge."
        );
    }

    let root = repo_root()?;
    let ws_path = workspace_path(name)?;

    if !ws_path.exists() {
        bail!(
            "Workspace '{name}' not found at {}.\n  \
             Check existing workspaces: maw ws list",
            ws_path.display()
        );
    }

    // Read metadata — advance only works for persistent workspaces.
    let meta = metadata::read(&root, name)
        .with_context(|| format!("Failed to read metadata for workspace '{name}'"))?;

    if meta.mode != WorkspaceMode::Persistent {
        bail!(
            "Workspace '{name}' is ephemeral (the default mode).\n  \
             Only persistent workspaces can be advanced.\n  \
             \n  To create a persistent workspace: maw ws create <name> --persistent\n  \
             To advance a persistent workspace after epoch change: maw ws advance <name>"
        );
    }

    // Get the workspace's current base epoch (HEAD of the worktree).
    let old_epoch = get_worktree_head(&ws_path)
        .with_context(|| format!("Failed to get HEAD of workspace '{name}'"))?;

    // Get the current epoch from refs/manifold/epoch/current.
    let current_epoch =
        manifold_refs::read_epoch_current(&root).with_context(|| "Failed to read current epoch")?;

    let Some(current_epoch) = current_epoch else {
        bail!(
            "No epoch ref found. Run `maw init` to initialize the repository.\n  \
             Then retry: maw ws advance {name}"
        );
    };

    // Already up to date?
    if old_epoch == current_epoch.as_str() {
        if format == OutputFormat::Json {
            let result = AdvanceResult {
                workspace: name.to_owned(),
                old_epoch: old_epoch.clone(),
                new_epoch: old_epoch,
                success: true,
                conflicts: vec![],
                message: format!("Workspace '{name}' is already at the current epoch."),
            };
            println!("{}", format.serialize(&result)?);
        } else {
            println!("Workspace '{name}' is already at the current epoch.");
            println!("  Epoch: {}...", &current_epoch.as_str()[..12]);
            println!();
            println!("Nothing to do.");
        }
        return Ok(());
    }

    let new_epoch = current_epoch.as_str().to_owned();
    let old_short = &old_epoch[..12.min(old_epoch.len())];
    let new_short = &new_epoch[..12.min(new_epoch.len())];

    if !matches!(format, OutputFormat::Json) {
        println!("Advancing workspace '{name}'...");
        println!("  From epoch: {old_short}...");
        println!("  To epoch:   {new_short}...");
        println!();
    }

    // Step 1: Stash uncommitted changes.
    let had_stash = stash_changes(&ws_path)?;

    // Step 2: Reset HEAD to the new epoch.
    checkout_epoch(&ws_path, &new_epoch)
        .with_context(|| format!("Failed to checkout new epoch in workspace '{name}'"))?;

    // Step 3: Pop the stash if there was one.
    let conflicts = if had_stash {
        pop_stash_and_detect_conflicts(&ws_path)?
    } else {
        vec![]
    };

    let success = conflicts.is_empty();

    let message = if success {
        format!(
            "Workspace '{name}' advanced from epoch {old_short}... to {new_short}... successfully."
        )
    } else {
        format!(
            "Workspace '{name}' advanced from {old_short}... to {new_short}... with {} conflict(s).\n  \
             Resolve conflicts in {}, then continue working.",
            conflicts.len(),
            ws_path.display()
        )
    };

    let result = AdvanceResult {
        workspace: name.to_owned(),
        old_epoch: old_epoch.clone(),
        new_epoch: new_epoch.clone(),
        success,
        conflicts,
        message,
    };

    match format {
        OutputFormat::Json => {
            println!("{}", format.serialize(&result)?);
        }
        OutputFormat::Text => {
            print_advance_text(&result);
        }
        OutputFormat::Pretty => {
            print_advance_pretty(&result);
        }
    }

    if !success {
        // Propagate conflict as non-zero exit for script use.
        bail!("Advance completed with conflicts. Resolve them before continuing.");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Git helpers
// ---------------------------------------------------------------------------

/// Get the HEAD OID of a worktree (the workspace's current base epoch).
fn get_worktree_head(ws_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(ws_path)
        .output()
        .context("Failed to run git rev-parse HEAD")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git rev-parse HEAD failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Stash uncommitted changes. Returns `true` if there was something to stash.
fn stash_changes(ws_path: &Path) -> Result<bool> {
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
fn checkout_epoch(ws_path: &Path, epoch_oid: &str) -> Result<()> {
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
fn pop_stash_and_detect_conflicts(ws_path: &Path) -> Result<Vec<AdvanceConflict>> {
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
fn detect_conflicts_in_worktree(ws_path: &Path) -> Result<Vec<AdvanceConflict>> {
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

        conflicts.push(AdvanceConflict {
            path,
            conflict_type: conflict_type.to_owned(),
        });
    }

    Ok(conflicts)
}

// ---------------------------------------------------------------------------
// Output formatters
// ---------------------------------------------------------------------------

fn print_advance_text(result: &AdvanceResult) {
    println!("{}", result.message);
    println!();
    if result.conflicts.is_empty() {
        println!("Next: maw exec {} -- <command>", result.workspace);
    } else {
        println!("Conflicts:");
        for c in &result.conflicts {
            println!("  [{:>20}] {}", c.conflict_type, c.path);
        }
        println!();
        println!("Resolve conflicts manually, then continue working.");
    }
}

fn print_advance_pretty(result: &AdvanceResult) {
    let (green, yellow, bold, gray, reset) =
        ("\x1b[32m", "\x1b[33m", "\x1b[1m", "\x1b[90m", "\x1b[0m");

    if result.success {
        println!("{green}✓{reset} {bold}Advance complete{reset}");
        println!("{}", result.message);
        println!();
        println!(
            "{gray}Next: maw exec {} -- <command>{reset}",
            result.workspace
        );
    } else {
        println!("{yellow}⚠ Advance completed with conflicts{reset}");
        println!("{}", result.message);
        println!();
        println!("{bold}Conflicts:{reset}");
        for c in &result.conflicts {
            println!("  {yellow}[{:>20}]{reset} {}", c.conflict_type, c.path);
        }
        println!();
        println!(
            "Resolve conflicts manually in {bold}{}{reset}",
            result.workspace
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
    fn advance_result_success_serialize() {
        let r = AdvanceResult {
            workspace: "my-ws".to_owned(),
            old_epoch: "a".repeat(40),
            new_epoch: "b".repeat(40),
            success: true,
            conflicts: vec![],
            message: "Advanced successfully.".to_owned(),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"success\":true"));
        assert!(json.contains("\"conflicts\":[]"));
    }

    #[test]
    fn advance_result_conflict_serialize() {
        let r = AdvanceResult {
            workspace: "my-ws".to_owned(),
            old_epoch: "a".repeat(40),
            new_epoch: "b".repeat(40),
            success: false,
            conflicts: vec![AdvanceConflict {
                path: "src/main.rs".to_owned(),
                conflict_type: "content".to_owned(),
            }],
            message: "Conflicts detected.".to_owned(),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"success\":false"));
        assert!(json.contains("\"conflict_type\":\"content\""));
        assert!(json.contains("\"path\":\"src/main.rs\""));
    }
}
