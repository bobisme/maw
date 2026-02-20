use anyhow::{Result, bail};

use crate::backend::WorkspaceBackend;
use crate::model::types::WorkspaceId;

use super::{DEFAULT_WORKSPACE, create::create, get_backend, repo_root, workspace_path};

/// Restore a previously destroyed workspace.
///
/// In the git worktree model, restoring means recreating the workspace
/// at the current epoch. Unlike jj, git worktrees don't have an operation
/// log to revert from, so we create a fresh workspace.
///
/// If a backup of the workspace content exists, it would need to be
/// restored separately (e.g., from git stash or reflog).
pub fn restore(name: &str) -> Result<()> {
    if name == DEFAULT_WORKSPACE {
        bail!("Cannot restore the default workspace");
    }

    let _root = repo_root()?;
    let path = workspace_path(name)?;

    if path.exists() {
        let backend = get_backend()?;
        let ws_id =
            WorkspaceId::new(name).map_err(|e| anyhow::anyhow!("Invalid workspace name: {e}"))?;

        if backend.exists(&ws_id) {
            bail!(
                "Workspace '{name}' already exists at {}\n  \
                 Nothing to restore. Use 'maw ws list' to see all workspaces.",
                path.display()
            );
        } else {
            // Directory exists but not tracked — try to attach
            println!("Directory exists but workspace not tracked. Re-creating...");
        }
    }

    println!("Restoring workspace '{name}'...");
    println!("  Creating fresh workspace at current epoch.");
    println!();

    // Create a fresh workspace at the current epoch (always ephemeral on restore).
    create(name, None, false)?;

    println!();
    println!("Note: Workspace '{name}' was recreated at the current epoch.");
    println!("Previous workspace contents are not automatically restored.");
    println!("If you had uncommitted changes, check git reflog for recovery options.");

    Ok(())
}
