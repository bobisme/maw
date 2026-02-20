use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::backend::WorkspaceBackend;
use crate::model::types::WorkspaceId;
use crate::refs as manifold_refs;

use super::{get_backend, repo_root};

pub fn sync(all: bool) -> Result<()> {
    if all {
        return sync_all();
    }

    let root = repo_root()?;
    let backend = get_backend()?;

    // Get the current epoch
    let current_epoch = manifold_refs::read_epoch_current(&root)
        .map_err(|e| anyhow::anyhow!("Failed to read current epoch: {e}"))?;

    let Some(current_epoch) = current_epoch else {
        println!("No epoch ref set. Run `maw init` first.");
        return Ok(());
    };

    // Check the default workspace
    let default_id = WorkspaceId::new("default").map_err(|e| anyhow::anyhow!("{e}"))?;

    if !backend.exists(&default_id) {
        println!("No default workspace found.");
        return Ok(());
    }

    let ws_status = backend
        .status(&default_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if !ws_status.is_stale {
        println!("Workspace is up to date.");
        return Ok(());
    }

    println!("Default workspace is stale (behind current epoch), syncing...");
    println!();

    // In the git worktree model, "syncing" means updating the worktree's
    // HEAD to point to the current epoch. This is done via git reset.
    sync_worktree_to_epoch(&root, "default", current_epoch.as_str())?;

    println!();
    println!("Workspace synced successfully.");

    Ok(())
}

/// Sync a single worktree to the given epoch commit.
///
/// Uses `git reset --hard <epoch>` inside the worktree to update it.
/// This is safe because workspace changes are captured by the merge engine
/// via snapshot before any merge, so uncommitted changes are not lost
/// during the normal workflow. However, this function is only called
/// explicitly by the user/agent via `maw ws sync`.
fn sync_worktree_to_epoch(root: &Path, ws_name: &str, epoch_oid: &str) -> Result<()> {
    let ws_path = root.join("ws").join(ws_name);
    if !ws_path.exists() {
        bail!("Workspace directory does not exist: {}", ws_path.display());
    }

    // Use checkout --detach to move HEAD to the new epoch
    let output = Command::new("git")
        .args(["checkout", "--detach", epoch_oid])
        .current_dir(&ws_path)
        .output()
        .with_context(|| format!("Failed to sync workspace '{ws_name}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Failed to sync workspace '{ws_name}': {}\n  \
             Manual fix: cd {} && git checkout --detach {epoch_oid}",
            stderr.trim(),
            ws_path.display()
        );
    }

    println!(
        "  \u{2713} {ws_name} - synced to epoch {}",
        &epoch_oid[..12]
    );
    Ok(())
}

/// Sync all workspaces at once
fn sync_all() -> Result<()> {
    let root = repo_root()?;
    let backend = get_backend()?;

    let current_epoch = manifold_refs::read_epoch_current(&root)
        .map_err(|e| anyhow::anyhow!("Failed to read current epoch: {e}"))?;

    let Some(current_epoch) = current_epoch else {
        println!("No epoch ref set. Run `maw init` first.");
        return Ok(());
    };

    let workspaces = backend.list().map_err(|e| anyhow::anyhow!("{e}"))?;

    if workspaces.is_empty() {
        println!("No workspaces found.");
        return Ok(());
    }

    let stale_count = workspaces.iter().filter(|ws| ws.state.is_stale()).count();

    if stale_count == 0 {
        println!("All {} workspace(s) are up to date.", workspaces.len());
        return Ok(());
    }

    println!(
        "Syncing {} stale workspace(s) of {} total...",
        stale_count,
        workspaces.len()
    );
    println!();

    let mut synced = 0;
    let mut errors: Vec<String> = Vec::new();

    for ws in &workspaces {
        if !ws.state.is_stale() {
            continue;
        }

        let name = ws.id.as_str();
        match sync_worktree_to_epoch(&root, name, current_epoch.as_str()) {
            Ok(()) => synced += 1,
            Err(e) => errors.push(format!("{name}: {e}")),
        }
    }

    println!();
    println!(
        "Results: {} synced, {} already current, {} errors",
        synced,
        workspaces.len() - stale_count,
        errors.len()
    );

    if !errors.is_empty() {
        println!();
        println!("Errors:");
        for err in &errors {
            println!("  - {err}");
        }
    }

    Ok(())
}

/// Auto-sync a stale workspace before running a command.
/// In the git worktree model, this updates the worktree HEAD to the current epoch.
/// Returns Ok(()) whether or not it was stale (idempotent).
pub fn auto_sync_if_stale(name: &str, _path: &Path) -> Result<()> {
    let root = repo_root()?;
    let backend = get_backend()?;

    let ws_id = match WorkspaceId::new(name) {
        Ok(id) => id,
        Err(_) => return Ok(()), // Invalid name, skip
    };

    if !backend.exists(&ws_id) {
        return Ok(());
    }

    let ws_status = backend.status(&ws_id).map_err(|e| anyhow::anyhow!("{e}"))?;

    if !ws_status.is_stale {
        return Ok(());
    }

    let current_epoch = manifold_refs::read_epoch_current(&root)
        .map_err(|e| anyhow::anyhow!("Failed to read current epoch: {e}"))?;

    let Some(current_epoch) = current_epoch else {
        return Ok(());
    };

    eprintln!("Workspace '{name}' is stale — auto-syncing before running command...");

    sync_worktree_to_epoch(&root, name, current_epoch.as_str())?;

    eprintln!("Workspace '{name}' synced. Proceeding with command.");
    eprintln!();

    Ok(())
}

/// Sync stale workspaces before merge to avoid spurious conflicts.
///
/// In the git worktree model, each workspace's HEAD is at the epoch it
/// was created from. If the epoch has advanced, the workspace is stale.
/// Syncing updates the HEAD to the current epoch before merging.
pub fn sync_stale_workspaces_for_merge(workspaces: &[String], root: &Path) -> Result<()> {
    let backend = get_backend()?;

    let current_epoch = manifold_refs::read_epoch_current(root)
        .map_err(|e| anyhow::anyhow!("Failed to read current epoch: {e}"))?;

    let Some(current_epoch) = current_epoch else {
        // No epoch ref — nothing to sync
        return Ok(());
    };

    let mut synced_count = 0;

    for ws_name in workspaces {
        let ws_id = match WorkspaceId::new(ws_name) {
            Ok(id) => id,
            Err(_) => continue,
        };

        if !backend.exists(&ws_id) {
            continue;
        }

        let ws_status = backend.status(&ws_id).map_err(|e| anyhow::anyhow!("{e}"))?;

        if !ws_status.is_stale {
            continue;
        }

        println!("Syncing stale workspace '{ws_name}' before merge...");
        sync_worktree_to_epoch(root, ws_name, current_epoch.as_str())?;
        synced_count += 1;
    }

    if synced_count > 0 {
        println!("Synced {synced_count} stale workspace(s). Proceeding with merge.");
        println!();
    }

    Ok(())
}

// Legacy functions kept as no-ops for backward compatibility during transition

/// Resolve divergent working copy — not applicable in git worktree model.
/// Git worktrees don't have the divergent commit concept that jj has.
pub fn resolve_divergent_working_copy(_workspace_dir: &Path) -> Result<()> {
    Ok(())
}
