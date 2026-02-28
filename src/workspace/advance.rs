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

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::format::OutputFormat;
use crate::model::types::WorkspaceMode;
use crate::refs as manifold_refs;

use super::working_copy::{
    WorkingCopyConflict, checkout_epoch, pop_stash_and_detect_conflicts, stash_changes,
};
use super::{DEFAULT_WORKSPACE, metadata, repo_root, workspace_path};

// ---------------------------------------------------------------------------
// Conflict info
// ---------------------------------------------------------------------------

/// Type alias preserving the original name for backward compatibility.
pub type AdvanceConflict = WorkingCopyConflict;

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
    // IMPORTANT: if checkout fails, restore the stash first so changes are not
    // orphaned. Without this, the user's work would be stranded in the stash
    // stack with no recovery path.
    if let Err(e) = checkout_epoch(&ws_path, &new_epoch) {
        if had_stash {
            // Best-effort restore — ignore errors so the original error surfaces.
            let _ = pop_stash_and_detect_conflicts(&ws_path);
        }
        return Err(e.context(format!("Failed to checkout new epoch in workspace '{name}'")));
    }

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
