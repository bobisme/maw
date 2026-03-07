use std::path::Path;
use std::process::Command;

use anyhow::{bail, Result};
use maw_git::GitRepo as _;
use serde::{Deserialize, Serialize};
use tracing::instrument;

use maw_core::backend::WorkspaceBackend;
use maw_core::model::types::WorkspaceId;
use maw_core::refs as manifold_refs;

use super::{get_backend, metadata, repo_root, MawConfig, DEFAULT_WORKSPACE};

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

    if !ws_status.is_stale {
        println!("Workspace '{workspace_name}' is up to date.");
        return Ok(());
    }

    // Safety: don't sync over committed work. If the workspace has commits not
    // yet in epoch (diverged after a concurrent merge), syncing would wipe them.
    // The lead agent must merge the workspace first — unless --rebase is used.
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
            if rebase {
                // --rebase: replay workspace commits onto the new epoch
                return rebase_workspace(
                    &root,
                    &workspace_name,
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

    Ok(())
}

// ---------------------------------------------------------------------------
// Rebase conflict metadata
// ---------------------------------------------------------------------------

/// A single rebase conflict recorded as data.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RebaseConflict {
    /// File path relative to workspace root.
    pub path: String,
    /// The original commit SHA being replayed when conflict occurred.
    pub original_commit: String,
    /// Base content (merge base), if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// "Ours" content (new epoch version), if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ours: Option<String>,
    /// "Theirs" content (workspace commit version), if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theirs: Option<String>,
}

/// Rebase conflict metadata stored in `.manifold/artifacts/ws/<name>/rebase-conflicts.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RebaseConflicts {
    /// All conflicts from the rebase.
    pub conflicts: Vec<RebaseConflict>,
    /// The epoch OID before the rebase.
    pub rebase_from: String,
    /// The epoch OID after the rebase (target).
    pub rebase_to: String,
}

/// Path to the rebase conflicts JSON file for a workspace.
fn rebase_conflicts_path(root: &Path, ws_name: &str) -> std::path::PathBuf {
    root.join(".manifold")
        .join("artifacts")
        .join("ws")
        .join(ws_name)
        .join("rebase-conflicts.json")
}

/// Read rebase conflicts for a workspace, if any.
pub fn read_rebase_conflicts(root: &Path, ws_name: &str) -> Option<RebaseConflicts> {
    let path = rebase_conflicts_path(root, ws_name);
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Write rebase conflicts for a workspace.
fn write_rebase_conflicts(root: &Path, ws_name: &str, conflicts: &RebaseConflicts) -> Result<()> {
    let path = rebase_conflicts_path(root, ws_name);
    let dir = path.parent().expect("path always has parent");
    std::fs::create_dir_all(dir)?;
    let content = serde_json::to_string_pretty(conflicts)?;
    std::fs::write(&path, content)?;
    Ok(())
}

/// Delete rebase conflicts file for a workspace (called on resolution).
pub fn delete_rebase_conflicts(root: &Path, ws_name: &str) -> Result<()> {
    let path = rebase_conflicts_path(root, ws_name);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rebase implementation
// ---------------------------------------------------------------------------

/// Replay workspace commits onto the current epoch via cherry-pick.
///
/// This is the core of `maw ws sync --rebase`. For each workspace commit
/// ahead of the old epoch:
/// 1. First, checkout the new epoch in the worktree (detached HEAD)
/// 2. For each commit in order, run `git cherry-pick --no-commit <sha>`
/// 3. If cherry-pick succeeds: stage changes, create new commit
/// 4. If cherry-pick conflicts: write conflict markers, record metadata, continue
/// 5. After all commits replayed: update workspace epoch ref
fn rebase_workspace(
    root: &Path,
    ws_name: &str,
    new_epoch: &str,
    ws_path: &Path,
    ahead_count: u32,
) -> Result<()> {
    // Safety: refuse to rebase if the workspace has uncommitted changes.
    let repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    let is_dirty = repo.is_dirty().map_err(|e| {
        anyhow::anyhow!("Failed to check dirty state for workspace '{ws_name}': {e}")
    })?;

    if is_dirty {
        bail!(
            "Workspace '{ws_name}' has uncommitted changes that would be lost by rebase. \
             Commit or stash first.\n  \
             Check: git -C {} status",
            ws_path.display()
        );
    }

    println!(
        "Rebasing workspace '{ws_name}' ({ahead_count} commit(s)) onto epoch {}...",
        &new_epoch[..std::cmp::min(12, new_epoch.len())]
    );
    println!();

    // Get the old epoch (workspace's current base).
    let old_epoch = get_workspace_head(ws_path)?;

    // Collect commit SHAs to replay (oldest first).
    let commits = list_commits_ahead(ws_path, &old_epoch)?;
    if commits.is_empty() {
        println!("No commits to replay. Performing normal sync.");
        sync_worktree_to_epoch(root, ws_name, new_epoch)?;
        println!();
        println!("Workspace synced successfully.");
        return Ok(());
    }

    // Step 1: Checkout the new epoch (detached HEAD).
    let output = Command::new("git")
        .args(["checkout", "--detach", new_epoch])
        .current_dir(ws_path)
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to run git checkout: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Failed to checkout new epoch in workspace '{ws_name}': {}",
            stderr.trim()
        );
    }

    // Step 2: Replay each commit via cherry-pick.
    let mut conflicts: Vec<RebaseConflict> = Vec::new();
    let mut replayed = 0;
    let mut conflicted = 0;

    for (i, commit_sha) in commits.iter().enumerate() {
        let short_sha = &commit_sha[..std::cmp::min(12, commit_sha.len())];
        let commit_msg = get_commit_message(ws_path, commit_sha);

        // Try cherry-pick --no-commit to apply changes without auto-committing.
        let cp_output = Command::new("git")
            .args(["cherry-pick", "--no-commit", commit_sha])
            .current_dir(ws_path)
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to run git cherry-pick: {e}"))?;

        if cp_output.status.success() {
            // Cherry-pick succeeded — create a new commit preserving the original message.
            let msg = commit_msg.unwrap_or_else(|| format!("rebase: replay {short_sha}"));
            let commit_output = Command::new("git")
                .args(["commit", "--no-verify", "-m", &msg])
                .current_dir(ws_path)
                .output()
                .map_err(|e| anyhow::anyhow!("Failed to commit replayed changes: {e}"))?;

            if commit_output.status.success() {
                replayed += 1;
                println!(
                    "  [{}/{}] Replayed {short_sha}: {}",
                    i + 1,
                    commits.len(),
                    msg.lines().next().unwrap_or("(no message)")
                );
            } else {
                // Commit may fail if cherry-pick resulted in no changes (already applied).
                let stderr = String::from_utf8_lossy(&commit_output.stderr);
                if stderr.contains("nothing to commit") {
                    println!(
                        "  [{}/{}] Skipped {short_sha} (already applied)",
                        i + 1,
                        commits.len()
                    );
                    // Reset for next cherry-pick.
                    let _ = Command::new("git")
                        .args(["reset", "HEAD"])
                        .current_dir(ws_path)
                        .output();
                } else {
                    println!(
                        "  [{}/{}] Warning: commit after cherry-pick failed for {short_sha}: {}",
                        i + 1,
                        commits.len(),
                        stderr.trim()
                    );
                }
            }
        } else {
            // Cherry-pick failed (conflict). Record conflicts and continue.
            conflicted += 1;

            // Find which files have conflict markers.
            let conflict_files = list_conflicted_files(ws_path);

            println!(
                "  [{}/{}] CONFLICT replaying {short_sha}: {} file(s)",
                i + 1,
                commits.len(),
                conflict_files.len()
            );

            for cf in &conflict_files {
                println!("    - {cf}");

                // Read the conflict content from the working tree (has markers).
                // For the metadata, try to capture base/ours/theirs from the index stages.
                let (base, ours, theirs) = read_conflict_stages(ws_path, cf);

                conflicts.push(RebaseConflict {
                    path: cf.clone(),
                    original_commit: commit_sha.clone(),
                    base,
                    ours,
                    theirs,
                });
            }

            // Add all conflicted files (with markers) to the index and commit.
            // This preserves the conflict markers in the history so the agent
            // can see and resolve them.
            let _ = Command::new("git")
                .args(["add", "--all"])
                .current_dir(ws_path)
                .output();
            let msg = format!(
                "rebase: conflict replaying {short_sha} ({} file(s))",
                conflict_files.len()
            );
            let _ = Command::new("git")
                .args(["commit", "--no-verify", "--allow-empty", "-m", &msg])
                .current_dir(ws_path)
                .output();
        }
    }

    // Step 3: Update workspace epoch ref.
    if let Ok(oid) = maw_core::model::types::GitOid::new(new_epoch) {
        let epoch_ref = manifold_refs::workspace_epoch_ref(ws_name);
        let _ = manifold_refs::write_ref(root, &epoch_ref, &oid);
    }

    // Step 4: Record conflict metadata and update workspace metadata.
    if !conflicts.is_empty() {
        let conflict_count = conflicts.len() as u32;

        // Write conflict metadata file.
        let rebase_meta = RebaseConflicts {
            conflicts,
            rebase_from: old_epoch,
            rebase_to: new_epoch.to_string(),
        };
        write_rebase_conflicts(root, ws_name, &rebase_meta)?;

        // Update workspace metadata with conflict count.
        let mut ws_meta = metadata::read(root, ws_name).unwrap_or_default();
        ws_meta.rebase_conflict_count = conflict_count;
        metadata::write(root, ws_name, &ws_meta)?;

        println!();
        println!("Rebase complete: {replayed} commit(s) replayed, {conflicted} with conflicts.");
        println!("Workspace '{ws_name}' has {conflict_count} unresolved conflict(s).");
        println!();
        println!("Files with conflict markers are in the working tree.");
        println!("To resolve:");
        println!("  1. Edit conflicted files in ws/{ws_name}/ to remove conflict markers");
        println!(
            "  2. Commit the resolution: maw exec {ws_name} -- git add -A && maw exec {ws_name} -- git commit -m \"fix: resolve rebase conflicts\""
        );
        println!("  3. Clear conflict state: maw ws sync {ws_name}");
    } else {
        // No conflicts — clean up any stale conflict metadata.
        let _ = delete_rebase_conflicts(root, ws_name);
        let mut ws_meta = metadata::read(root, ws_name).unwrap_or_default();
        if ws_meta.rebase_conflict_count > 0 {
            ws_meta.rebase_conflict_count = 0;
            metadata::write(root, ws_name, &ws_meta)?;
        }

        println!();
        println!("Rebase complete: {replayed} commit(s) replayed cleanly.");
        println!("Workspace '{ws_name}' is now up to date.");
    }

    Ok(())
}

/// Get the current HEAD OID for a workspace.
fn get_workspace_head(ws_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(ws_path)
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to run git rev-parse HEAD: {e}"))?;
    if !output.status.success() {
        bail!("Failed to get HEAD for workspace at {}", ws_path.display());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// List commits ahead of the epoch, oldest first (for replay order).
fn list_commits_ahead(ws_path: &Path, epoch_oid: &str) -> Result<Vec<String>> {
    let range = format!("{epoch_oid}..HEAD");
    let output = Command::new("git")
        .args(["rev-list", "--reverse", &range])
        .current_dir(ws_path)
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to run git rev-list: {e}"))?;
    if !output.status.success() {
        bail!("Failed to list commits ahead of epoch");
    }
    let commits: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    Ok(commits)
}

/// Get the commit message for a given SHA.
fn get_commit_message(ws_path: &Path, sha: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["log", "-1", "--format=%B", sha])
        .current_dir(ws_path)
        .output()
        .ok()?;
    if output.status.success() {
        let msg = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if msg.is_empty() {
            None
        } else {
            Some(msg)
        }
    } else {
        None
    }
}

/// List files with unmerged (conflicted) entries in the index.
fn list_conflicted_files(ws_path: &Path) -> Vec<String> {
    let output = Command::new("git")
        .args(["diff", "--name-only", "--diff-filter=U"])
        .current_dir(ws_path)
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        _ => {
            // Fallback: try ls-files --unmerged
            let output2 = Command::new("git")
                .args(["ls-files", "--unmerged"])
                .current_dir(ws_path)
                .output();
            match output2 {
                Ok(o) if o.status.success() => {
                    let mut files: Vec<String> = String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .filter_map(|l| l.split('\t').nth(1).map(|s| s.to_string()))
                        .collect();
                    files.sort();
                    files.dedup();
                    files
                }
                _ => vec![],
            }
        }
    }
}

/// Read the three conflict stages (base/ours/theirs) from the git index for a file.
/// Returns (base, ours, theirs) as Option<String> for each stage.
fn read_conflict_stages(
    ws_path: &Path,
    file_path: &str,
) -> (Option<String>, Option<String>, Option<String>) {
    let read_stage = |stage: &str| -> Option<String> {
        let spec = format!(":{stage}:{file_path}");
        let output = Command::new("git")
            .args(["show", &spec])
            .current_dir(ws_path)
            .output()
            .ok()?;
        if output.status.success() {
            String::from_utf8(output.stdout).ok()
        } else {
            None
        }
    };

    let base = read_stage("1");
    let ours = read_stage("2");
    let theirs = read_stage("3");
    (base, ours, theirs)
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
    let is_dirty = repo.is_dirty().map_err(|e| {
        anyhow::anyhow!("Failed to check dirty state for workspace '{ws_name}': {e}")
    })?;

    if is_dirty {
        bail!(
            "Workspace '{ws_name}' has uncommitted changes that would be lost by sync. \
             Commit or stash first.\n  \
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
        .map_err(|e| anyhow::anyhow!("Failed to run git checkout in workspace '{ws_name}': {e}"))?;
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

    eprintln!(
        "Workspace '{name}' is behind the current epoch \u{2014} auto-syncing before running command..."
    );

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

    #[test]
    fn rebase_conflict_serialization_roundtrip() {
        let conflicts = super::RebaseConflicts {
            conflicts: vec![
                super::RebaseConflict {
                    path: "src/main.rs".to_string(),
                    original_commit: "a".repeat(40),
                    base: Some("base content".to_string()),
                    ours: Some("ours content".to_string()),
                    theirs: Some("theirs content".to_string()),
                },
                super::RebaseConflict {
                    path: "Cargo.toml".to_string(),
                    original_commit: "b".repeat(40),
                    base: None,
                    ours: Some("ours only".to_string()),
                    theirs: None,
                },
            ],
            rebase_from: "c".repeat(40),
            rebase_to: "d".repeat(40),
        };
        let json = serde_json::to_string_pretty(&conflicts).unwrap();
        let parsed: super::RebaseConflicts = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.conflicts.len(), 2);
        assert_eq!(parsed.conflicts[0].path, "src/main.rs");
        assert_eq!(parsed.conflicts[1].path, "Cargo.toml");
        assert_eq!(parsed.rebase_from, "c".repeat(40));
        assert_eq!(parsed.rebase_to, "d".repeat(40));
    }
}
