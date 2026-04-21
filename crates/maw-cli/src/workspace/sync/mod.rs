mod checks;
mod cross_target;
mod lock;
mod rebase;

use std::path::Path;

use anyhow::Result;
use maw_git::GitRepo;
use tracing::instrument;

use maw_core::backend::WorkspaceBackend;
use maw_core::model::types::WorkspaceId;
use maw_core::refs as manifold_refs;

use super::{MawConfig, get_backend, repo_root};

use checks::{
    committed_ahead_of_epoch, is_default_workspace, sync_worktree_to_epoch,
    workspace_has_uncommitted_changes, workspace_name_from_cwd,
};
use cross_target::cross_target_sync_risk;
use rebase::rebase_workspace;

pub use rebase::{RebaseConflict, RebaseConflicts, delete_rebase_conflicts, read_rebase_conflicts};

fn maybe_clear_stale_conflict_sidecars(root: &Path, ws_name: &str, ws_path: &Path) -> Result<bool> {
    let mut tracked_paths = std::collections::BTreeSet::new();

    if let Some(tree) = super::resolve_structured::read_conflict_tree_sidecar(root, ws_name) {
        tracked_paths.extend(tree.conflicts.into_keys());
    }

    if tracked_paths.is_empty()
        && let Some(legacy) = read_rebase_conflicts(root, ws_name)
    {
        tracked_paths.extend(
            legacy
                .conflicts
                .into_iter()
                .map(|conflict| std::path::PathBuf::from(conflict.path)),
        );
    }

    if tracked_paths.is_empty() {
        return Ok(false);
    }

    let marker_paths =
        super::resolve::find_conflicted_files_filtered(ws_path, Some(&tracked_paths))?;
    if !marker_paths.is_empty() {
        return Ok(false);
    }

    let head_oid_str = match super::merge::resolve_workspace_head_oid(ws_path) {
        Ok(oid) => oid,
        Err(_) => return Ok(false),
    };
    let head_oid: maw_git::GitOid = head_oid_str
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid workspace HEAD OID '{head_oid_str}': {e}"))?;

    let repo = maw_git::GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;
    let commit = repo
        .read_commit(head_oid)
        .map_err(|e| anyhow::anyhow!("read_commit({head_oid}) failed: {e}"))?;
    let tainted = super::merge::find_tool_placeholder_blobs(&repo, commit.tree_oid)?;
    if !tainted.is_empty() {
        return Ok(false);
    }

    super::resolve_structured::clear_conflict_sidecars(root, ws_name)?;
    Ok(true)
}

#[instrument]
pub fn sync(name: Option<&str>, all: bool, rebase: bool) -> Result<()> {
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
    let ws_path = root.join("ws").join(&workspace_name);

    if !ws_status.is_stale {
        if maybe_clear_stale_conflict_sidecars(&root, &workspace_name, &ws_path)? {
            println!("Workspace '{workspace_name}' is up to date.");
            println!("Cleared stale conflict metadata after a manual resolution commit.");
        } else {
            println!("Workspace '{workspace_name}' is up to date.");
        }
        return Ok(());
    }

    // Safety: don't sync over committed work. If the workspace has commits not
    // yet in epoch (diverged after a concurrent merge), syncing would wipe them.
    // The lead agent must merge the workspace first — unless --rebase is used.
    // NOTE: We compare against the workspace's *original* base epoch, not the
    // current epoch. The workspace HEAD is based on the old epoch, so comparing
    // against the new epoch would report 0 commits ahead (HEAD is behind it),
    // causing us to skip the rebase and fast-forward — silently dropping commits.
    match committed_ahead_of_epoch(&ws_path, &ws_status.base_epoch) {
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
            if rebase {
                // --rebase: replay workspace commits onto the new epoch
                return rebase_workspace(
                    &root,
                    &workspace_name,
                    ws_status.base_epoch.as_str(),
                    current_epoch.as_str(),
                    &ws_path,
                    ahead,
                );
            }
            println!(
                "Workspace '{workspace_name}' is stale but has {ahead} committed commit(s) not yet \
                 merged into epoch."
            );
            println!("  Merge the workspace first: maw ws merge {workspace_name} --into default");
            println!("  Or rebase onto current epoch: maw ws sync {workspace_name} --rebase");
            println!("  Then sync: maw ws sync {workspace_name}");
            return Ok(());
        }
        Some(_) => {}
    }

    if let Some(active_change) = cross_target_sync_risk(
        &root,
        &workspace_name,
        ws_status.base_epoch.as_str(),
        current_epoch.as_str(),
    )? {
        println!(
            "Workspace '{workspace_name}' is behind current epoch, but that epoch tracks active change '{}' ({}) not yet on trunk.",
            active_change.change_id, active_change.change_branch
        );
        println!(
            "  Refusing to sync this unbound workspace to avoid pulling change-only commits into a trunk-targeted flow."
        );
        println!(
            "  To continue change work, create/use a change-bound workspace: maw ws create --change {} <name>",
            active_change.change_id
        );
        println!(
            "  To continue trunk-only work, keep this workspace on its current base and merge with --into default."
        );
        return Ok(());
    }

    if rebase {
        println!("Workspace '{workspace_name}' has no commits ahead of epoch; nothing to rebase.");
        println!("Performing normal sync instead.");
        println!();
    }

    println!("Workspace '{workspace_name}' is stale (behind current epoch), syncing...");
    println!();

    // In the git worktree model, "syncing" means updating the worktree's
    // HEAD to point to the current epoch via detached checkout.
    sync_worktree_to_epoch(&root, &workspace_name, current_epoch.as_str())?;

    println!();
    println!("Workspace synced successfully.");
    if !rebase {
        println!(
            "  If you expected committed workspace changes to be replayed onto the new epoch, run: maw ws sync {workspace_name} --rebase"
        );
    }

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
    let mut skipped_cross_target: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for ws in &workspaces {
        if !ws.state.is_stale() || is_default_workspace(ws.id.as_str()) {
            continue;
        }

        let name = ws.id.as_str();

        // Safety: skip workspaces with committed work not yet in epoch.
        // Syncing over them would wipe those commits.
        // If git fails (None), treat as "has work" to prevent data loss.
        // NOTE: Compare against the workspace's base epoch, not current epoch.
        // The workspace HEAD is based on the old epoch — comparing against the
        // new epoch would report 0 ahead (HEAD is behind it), missing local commits.
        let ws_path = root.join("ws").join(name);
        let ws_status = backend.status(&ws.id).map_err(|e| anyhow::anyhow!("{e}"))?;
        match committed_ahead_of_epoch(&ws_path, &ws_status.base_epoch) {
            None => {
                skipped_with_work.push(format!(
                    "{name} (could not determine commit count \u{2014} skipped for safety)"
                ));
                continue;
            }
            Some(ahead) if ahead > 0 => {
                skipped_with_work.push(format!("{name} ({ahead} commit(s) ahead)"));
                continue;
            }
            Some(_) => {}
        }

        if let Some(active_change) = cross_target_sync_risk(
            &root,
            name,
            ws_status.base_epoch.as_str(),
            current_epoch.as_str(),
        )? {
            skipped_cross_target.push(format!(
                "{name} (epoch tracks active change '{}' / {})",
                active_change.change_id, active_change.change_branch
            ));
            continue;
        }

        match sync_worktree_to_epoch(&root, name, current_epoch.as_str()) {
            Ok(()) => synced += 1,
            Err(e) => errors.push(format!("{name}: {e}")),
        }
    }

    if !skipped_with_work.is_empty() {
        println!();
        println!("Skipped (committed work not yet merged \u{2014} merge first):");
        for s in &skipped_with_work {
            println!("  - {s}");
        }
    }

    if !skipped_cross_target.is_empty() {
        println!();
        println!("Skipped (cross-target safety; active change epoch not yet on trunk):");
        for s in &skipped_cross_target {
            println!("  - {s}");
        }
    }

    let skipped_total = skipped_with_work.len() + skipped_cross_target.len();

    println!();
    println!(
        "Results: {} synced, {} already current, {} skipped, {} errors",
        synced,
        workspaces.len() - stale_count,
        skipped_total,
        errors.len()
    );

    if skipped_total > 0 {
        println!("Result: INCOMPLETE (safety skips detected; see skipped sections above).");
    }

    if !errors.is_empty() {
        println!();
        println!("Errors:");
        for err in &errors {
            println!("  - {err}");
        }
        anyhow::bail!(
            "sync --all failed for {} workspace(s); resolve listed errors and retry",
            errors.len()
        );
    }

    if skipped_total > 0 {
        anyhow::bail!(
            "sync --all incomplete: {skipped_total} workspace(s) were skipped by safety checks; merge or resolve them, then rerun maw ws sync --all"
        );
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
    // NOTE: Compare against base epoch, not current — see bn-18dj.
    let ws_path = root.join("ws").join(name);
    match committed_ahead_of_epoch(&ws_path, &ws_status.base_epoch) {
        None => {
            eprintln!(
                "WARNING: Workspace '{name}' is behind the current epoch (another merge advanced repository state), \
                 but git could not determine commit count. Skipping auto-sync to preserve committed work."
            );
            eprintln!(
                "  The lead agent should merge this workspace: maw ws merge {name} --into default"
            );
            return Ok(());
        }
        Some(ahead) if ahead > 0 => {
            eprintln!(
                "WARNING: Workspace '{name}' is behind the current epoch (another merge advanced repository state since \
                 this one was created), and has {ahead} committed commit(s) not yet merged."
            );
            eprintln!("  Skipping auto-sync to preserve committed work.");
            eprintln!(
                "  The lead agent should merge or rebase this workspace: maw ws merge {name} --into default  or  maw ws sync {name} --rebase"
            );
            return Ok(());
        }
        Some(_) => {}
    }

    if let Some(active_change) = cross_target_sync_risk(
        &root,
        name,
        ws_status.base_epoch.as_str(),
        current_epoch.as_str(),
    )? {
        eprintln!(
            "WARNING: Workspace '{name}' is behind current epoch, but epoch tracks active change '{}' ({}) not yet on trunk.",
            active_change.change_id, active_change.change_branch
        );
        eprintln!(
            "  Skipping auto-sync for this unbound workspace to avoid pulling change-only commits into trunk-targeted work."
        );
        eprintln!(
            "  Use a change-bound workspace instead: maw ws create --change {} <name>",
            active_change.change_id
        );
        eprintln!(
            "  If this workspace should stay trunk-only, continue without syncing and merge with --into default."
        );
        return Ok(());
    }

    // Safety: don't auto-sync over uncommitted changes — warn and let the
    // command run against the stale workspace instead of blocking it entirely.
    let ws_path = root.join("ws").join(name);
    let is_dirty = workspace_has_uncommitted_changes(&ws_path).unwrap_or(false);
    if is_dirty {
        eprintln!(
            "WARNING: Workspace '{name}' is behind the current epoch, but has uncommitted changes. \
             Skipping auto-sync to preserve uncommitted work."
        );
        eprintln!("  Commit or stash changes, then run: maw ws sync {name}");
        return Ok(());
    }

    eprintln!(
        "Workspace '{name}' is behind the current epoch \u{2014} auto-syncing before running command..."
    );

    sync_worktree_to_epoch(&root, name, current_epoch.as_str())?;

    eprintln!("Workspace '{name}' synced. Proceeding with command.");
    eprintln!();

    Ok(())
}
