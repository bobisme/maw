use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::backend::WorkspaceBackend;
use crate::model::types::WorkspaceId;
use crate::refs as manifold_refs;

use super::{DEFAULT_WORKSPACE, MawConfig, get_backend, repo_root};

fn is_default_workspace(name: &str) -> bool {
    name == DEFAULT_WORKSPACE
}

fn workspace_name_from_cwd(root: &Path, cwd: &Path) -> String {
    let ws_root = root.join("ws");
    let Ok(relative) = cwd.strip_prefix(&ws_root) else {
        return DEFAULT_WORKSPACE.to_string();
    };

    let Some(component) = relative.components().next() else {
        return DEFAULT_WORKSPACE.to_string();
    };

    let std::path::Component::Normal(name) = component else {
        return DEFAULT_WORKSPACE.to_string();
    };

    let Some(name) = name.to_str() else {
        return DEFAULT_WORKSPACE.to_string();
    };

    if WorkspaceId::new(name).is_ok() {
        name.to_owned()
    } else {
        DEFAULT_WORKSPACE.to_string()
    }
}

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

    let cwd = std::env::current_dir().unwrap_or_else(|_| root.clone());
    let workspace_name = workspace_name_from_cwd(&root, &cwd);
    let ws_id = WorkspaceId::new(&workspace_name).map_err(|e| anyhow::anyhow!("{e}"))?;

    if is_default_workspace(&workspace_name) {
        let branch = MawConfig::load(&root)
            .map(|cfg| cfg.branch().to_string())
            .unwrap_or_else(|_| "main".to_string());
        println!(
            "Workspace '{workspace_name}' is the default branch workspace (tracks '{branch}')."
        );
        println!("Skipping detached-epoch sync for default workspace.");
        return Ok(());
    }

    if !backend.exists(&ws_id) {
        println!("Workspace '{workspace_name}' not found.");
        return Ok(());
    }

    let ws_status = backend.status(&ws_id).map_err(|e| anyhow::anyhow!("{e}"))?;

    if !ws_status.is_stale {
        println!("Workspace '{workspace_name}' is up to date.");
        return Ok(());
    }

    // Safety: don't sync over committed work. If the workspace has commits not
    // yet in epoch (diverged after a concurrent merge), syncing would wipe them.
    // The lead agent must merge the workspace first.
    let ws_path = root.join("ws").join(&workspace_name);
    let ahead = committed_ahead_of_epoch(&ws_path, current_epoch.as_str());
    if ahead > 0 {
        println!(
            "Workspace '{workspace_name}' is stale but has {ahead} committed commit(s) not yet \
             merged into epoch."
        );
        println!(
            "  Merge the workspace first: maw ws merge {workspace_name}"
        );
        println!(
            "  Then sync: maw ws sync {workspace_name}"
        );
        return Ok(());
    }

    println!("Workspace '{workspace_name}' is stale (behind current epoch), syncing...");
    println!();

    // In the git worktree model, "syncing" means updating the worktree's
    // HEAD to point to the current epoch via detached checkout.
    sync_worktree_to_epoch(&root, &workspace_name, current_epoch.as_str())?;

    println!();
    println!("Workspace synced successfully.");

    Ok(())
}

/// Count commits reachable from HEAD but not from `epoch_oid` inside a workspace.
///
/// Returns the number of committed-but-not-yet-merged commits in the workspace.
/// A result > 0 means the workspace has committed work that should be merged
/// before syncing; syncing over it would wipe those commits.
fn committed_ahead_of_epoch(ws_path: &Path, epoch_oid: &str) -> u32 {
    let range = format!("{epoch_oid}..HEAD");
    Command::new("git")
        .args(["rev-list", "--count", &range])
        .current_dir(ws_path)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Sync a single worktree to the given epoch commit.
///
/// Uses `git checkout --detach <epoch>` inside the worktree to update it.
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
             Manual fix: git -C {} checkout --detach {epoch_oid}",
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

    let stale_count = workspaces
        .iter()
        .filter(|ws| ws.state.is_stale() && !is_default_workspace(ws.id.as_str()))
        .count();

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
    let mut skipped_with_work: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for ws in &workspaces {
        if !ws.state.is_stale() || is_default_workspace(ws.id.as_str()) {
            continue;
        }

        let name = ws.id.as_str();

        // Safety: skip workspaces with committed work not yet in epoch.
        // Syncing over them would wipe those commits.
        let ws_path = root.join("ws").join(name);
        let ahead = committed_ahead_of_epoch(&ws_path, current_epoch.as_str());
        if ahead > 0 {
            skipped_with_work.push(format!("{name} ({ahead} commit(s) ahead)"));
            continue;
        }

        match sync_worktree_to_epoch(&root, name, current_epoch.as_str()) {
            Ok(()) => synced += 1,
            Err(e) => errors.push(format!("{name}: {e}")),
        }
    }

    if !skipped_with_work.is_empty() {
        println!();
        println!("Skipped (committed work not yet merged — merge first):");
        for s in &skipped_with_work {
            println!("  - {s}");
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
    if is_default_workspace(name) {
        return Ok(());
    }

    let root = repo_root()?;
    let backend = get_backend()?;

    let Ok(ws_id) = WorkspaceId::new(name) else {
        return Ok(()); // Invalid name, skip
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

    // Safety: never auto-sync over committed work. When epoch advances laterally
    // (another workspace merged while this one has commits), the workspace is
    // stale AND has diverged commits. Syncing would wipe those commits.
    // The lead agent must merge this workspace first.
    let ws_path = root.join("ws").join(name);
    let ahead = committed_ahead_of_epoch(&ws_path, current_epoch.as_str());
    if ahead > 0 {
        eprintln!(
            "WARNING: Workspace '{name}' is stale relative to current epoch, but has \
             {ahead} committed commit(s) not yet merged into epoch."
        );
        eprintln!("  Skipping auto-sync to preserve committed work.");
        eprintln!("  The lead agent should merge this workspace: maw ws merge {name}");
        return Ok(());
    }

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
#[allow(dead_code)]
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
        if is_default_workspace(ws_name) {
            continue;
        }

        let Ok(ws_id) = WorkspaceId::new(ws_name) else {
            continue;
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

#[cfg(test)]
mod tests {
    use super::workspace_name_from_cwd;
    use std::path::Path;

    #[test]
    fn detects_workspace_name_from_workspace_path() {
        let root = Path::new("/repo");
        let cwd = Path::new("/repo/ws/agent-1/src");
        assert_eq!(workspace_name_from_cwd(root, cwd), "agent-1");
    }

    #[test]
    fn falls_back_to_default_outside_workspace_tree() {
        let root = Path::new("/repo");
        let cwd = Path::new("/repo/docs");
        assert_eq!(workspace_name_from_cwd(root, cwd), "default");
    }

    #[test]
    fn falls_back_to_default_for_invalid_workspace_segment() {
        let root = Path::new("/repo");
        let cwd = Path::new("/repo/ws/not_valid");
        assert_eq!(workspace_name_from_cwd(root, cwd), "default");
    }

    #[test]
    fn detects_default_workspace_name() {
        assert!(super::is_default_workspace("default"));
        assert!(!super::is_default_workspace("agent-1"));
    }
}
