use std::path::Path;
use std::process::Command;

use anyhow::{Result, bail};
use maw_git::GitRepo as _;
use tracing::instrument;

use maw_core::backend::WorkspaceBackend;
use maw_core::model::types::WorkspaceId;
use maw_core::refs as manifold_refs;

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

#[instrument]
pub fn sync(name: Option<&str>, all: bool) -> Result<()> {
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

    let workspace_name = if let Some(n) = name {
        n.to_string()
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| root.clone());
        workspace_name_from_cwd(&root, &cwd)
    };
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
    match committed_ahead_of_epoch(&ws_path, current_epoch.as_str()) {
        None => {
            // Could not determine commit count — refuse to sync to prevent data loss.
            println!(
                "WARNING: Could not determine committed work for '{workspace_name}' \
                 (git failed). Refusing to sync to avoid data loss."
            );
            println!("  Check workspace state manually, then retry.");
            return Ok(());
        }
        Some(ahead) if ahead > 0 => {
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
        Some(_) => {}
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
///
/// Returns `None` if git fails for any reason (invalid repo, unknown OID, etc.).
/// Callers MUST treat `None` as "has committed work" (i.e. refuse to sync) to
/// prevent data loss when the workspace state cannot be determined.
// TODO(gix): GitRepo doesn't have a rev-list --count equivalent. Keep CLI.
fn committed_ahead_of_epoch(ws_path: &Path, epoch_oid: &str) -> Option<u32> {
    let range = format!("{epoch_oid}..HEAD");
    let output = Command::new("git")
        .args(["rev-list", "--count", &range])
        .current_dir(ws_path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .and_then(|s| s.trim().parse().ok())
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

    // Safety: refuse to sync if the workspace has uncommitted changes.
    // `git checkout --detach` would overwrite tracked files, losing staged,
    // unstaged, and untracked work.
    let repo = maw_git::GixRepo::open(&ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    let is_dirty = repo.is_dirty()
        .map_err(|e| anyhow::anyhow!("Failed to check dirty state for workspace '{ws_name}': {e}"))?;

    if is_dirty {
        bail!(
            "Workspace '{ws_name}' has uncommitted changes that would be lost by sync.\n\
             \n  \
             Sync rebases the workspace onto the latest epoch, which requires a clean\n  \
             working tree. (This is different from `maw ws merge` and `maw ws diff`,\n  \
             which operate on working-tree state including uncommitted changes.)\n\
             \n  \
             Commit first:\n    \
             maw exec {ws_name} -- git add -A && maw exec {ws_name} -- git commit -m 'wip'\n\
             \n  \
             Check: git -C {} status",
            ws_path.display()
        );
    }

    // Detach HEAD at the new epoch to sync the workspace.
    // TODO(gix): checkout_tree() does not update HEAD, and write_ref("HEAD")
    // doesn't reliably create a detached HEAD in linked worktrees. Keep
    // `git checkout --detach` until gix gains proper worktree HEAD support.
    let output = Command::new("git")
        .args(["checkout", "--detach", epoch_oid])
        .current_dir(&ws_path)
        .output()
        .map_err(|e| anyhow::anyhow!(
            "Failed to run git checkout in workspace '{ws_name}': {e}"
        ))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Failed to sync workspace '{ws_name}': {}\n  \
             Manual fix: git -C {} checkout --detach {epoch_oid}",
            stderr.trim(),
            ws_path.display()
        );
    }

    // Update the per-workspace creation epoch ref to the new epoch.
    // After sync, the workspace is rebased onto the new epoch, so
    // the epoch ref should reflect the new base.
    if let Ok(oid) = maw_core::model::types::GitOid::new(epoch_oid) {
        let epoch_ref = manifold_refs::workspace_epoch_ref(ws_name);
        let _ = manifold_refs::write_ref(root, &epoch_ref, &oid);
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
        // If git fails (None), treat as "has work" to prevent data loss.
        let ws_path = root.join("ws").join(name);
        match committed_ahead_of_epoch(&ws_path, current_epoch.as_str()) {
            None => {
                skipped_with_work.push(format!(
                    "{name} (could not determine commit count — skipped for safety)"
                ));
                continue;
            }
            Some(ahead) if ahead > 0 => {
                skipped_with_work.push(format!("{name} ({ahead} commit(s) ahead)"));
                continue;
            }
            Some(_) => {}
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
    match committed_ahead_of_epoch(&ws_path, current_epoch.as_str()) {
        None => {
            eprintln!(
                "WARNING: Workspace '{name}' is behind main (another workspace was merged), \
                 but git could not determine commit count. Skipping auto-sync to preserve committed work."
            );
            eprintln!("  The lead agent should merge this workspace: maw ws merge {name}");
            return Ok(());
        }
        Some(ahead) if ahead > 0 => {
            eprintln!(
                "WARNING: Workspace '{name}' is behind main (another workspace was merged since \
                 this one was created), and has {ahead} committed commit(s) not yet merged."
            );
            eprintln!("  Skipping auto-sync to preserve committed work.");
            eprintln!("  The lead agent should merge this workspace: maw ws merge {name}");
            return Ok(());
        }
        Some(_) => {}
    }

    eprintln!("Workspace '{name}' is behind main — auto-syncing before running command...");

    sync_worktree_to_epoch(&root, name, current_epoch.as_str())?;

    eprintln!("Workspace '{name}' synced. Proceeding with command.");
    eprintln!();

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
