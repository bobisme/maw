use std::io::{self, Write};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::backend::WorkspaceBackend;
use crate::model::types::{EpochId, WorkspaceId};
use crate::refs as manifold_refs;

use super::{
    ensure_repo_root, get_backend, repo_root, workspace_path, workspaces_dir,
    MawConfig, DEFAULT_WORKSPACE,
};

pub fn create(name: &str, revision: Option<&str>) -> Result<()> {
    let root = ensure_repo_root()?;
    let backend = get_backend()?;
    let path = workspace_path(name)?;

    if path.exists() {
        bail!("Workspace already exists at {}", path.display());
    }

    // Ensure ws directory exists
    let ws_dir = workspaces_dir()?;
    std::fs::create_dir_all(&ws_dir)
        .with_context(|| format!("Failed to create {}", ws_dir.display()))?;

    println!("Creating workspace '{name}' at ws/{name} ...");

    // Determine base epoch.
    // Use the provided revision, or fall back to refs/manifold/epoch/current,
    // or HEAD of the configured branch.
    let epoch = resolve_epoch(&root, revision)?;

    // Create workspace ID
    let ws_id = WorkspaceId::new(name)
        .map_err(|e| anyhow::anyhow!("Invalid workspace name: {e}"))?;

    // Create the workspace via backend
    let info = backend.create(&ws_id, &epoch)
        .map_err(|e| anyhow::anyhow!(
            "Failed to create workspace: {e}\n  Check: maw doctor\n  Verify name is not already used: maw ws list"
        ))?;

    // Get short commit ID for display
    let short_oid = &epoch.as_str()[..12];

    println!();
    println!("Workspace '{name}' ready!");
    println!();
    println!("  Epoch:  {short_oid} (base commit for this workspace)");
    println!("  Path:   {}/", info.path.display());
    println!();
    println!("  IMPORTANT: All file reads, writes, and edits must use this path.");
    println!("  This is your working directory for ALL operations, not just bash.");
    println!();
    println!("To start working:");
    println!();
    println!("  # Edit files under {}/", info.path.display());
    println!("  # Changes are detected automatically by the merge engine");
    println!();
    println!("  # Run commands in the workspace:");
    println!("  maw exec {name} -- cargo test");
    println!();
    println!("Note: All edits in the workspace are tracked automatically.");
    println!("The merge engine captures changes when merging.");

    Ok(())
}

/// Resolve the epoch (base commit) for a new workspace.
///
/// Priority:
/// 1. Explicit revision (from --revision flag)
/// 2. refs/manifold/epoch/current (if set by `maw init`)
/// 3. HEAD of the configured branch
fn resolve_epoch(root: &std::path::Path, revision: Option<&str>) -> Result<EpochId> {
    if let Some(rev) = revision {
        // Resolve the user-specified revision to a full OID
        let output = Command::new("git")
            .args(["rev-parse", rev])
            .current_dir(root)
            .output()
            .context("Failed to resolve revision")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Cannot resolve revision '{rev}': {}", stderr.trim());
        }
        let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return EpochId::new(&oid)
            .map_err(|e| anyhow::anyhow!("Invalid commit OID: {e}"));
    }

    // Try refs/manifold/epoch/current first
    if let Ok(Some(oid)) = manifold_refs::read_epoch_current(root) {
        return EpochId::new(oid.as_str())
            .map_err(|e| anyhow::anyhow!("Invalid epoch OID: {e}"));
    }

    // Fall back to configured branch HEAD
    let config = MawConfig::load(root).unwrap_or_default();
    let branch = config.branch();
    let output = Command::new("git")
        .args(["rev-parse", branch])
        .current_dir(root)
        .output()
        .with_context(|| format!("Failed to resolve branch '{branch}'"))?;

    if !output.status.success() {
        // Last resort: try HEAD
        let head_output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .context("Failed to resolve HEAD")?;
        if !head_output.status.success() {
            bail!("No commits found. Run `maw init` first, or specify --revision.");
        }
        let oid = String::from_utf8_lossy(&head_output.stdout).trim().to_string();
        return EpochId::new(&oid)
            .map_err(|e| anyhow::anyhow!("Invalid HEAD OID: {e}"));
    }

    let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    EpochId::new(&oid)
        .map_err(|e| anyhow::anyhow!("Invalid branch OID: {e}"))
}

pub fn destroy(name: &str, confirm: bool) -> Result<()> {
    if name == DEFAULT_WORKSPACE {
        bail!("Cannot destroy the default workspace");
    }
    // Also check config in case default_workspace is customized
    if let Ok(root) = repo_root()
        && let Ok(config) = MawConfig::load(&root)
            && name == config.default_workspace() {
                bail!("Cannot destroy the default workspace");
            }

    ensure_repo_root()?;
    let path = workspace_path(name)?;

    if !path.exists() {
        println!("Workspace '{name}' is already absent at {}.", path.display());
        println!("No action needed.");
        return Ok(());
    }

    if confirm {
        println!("About to destroy workspace '{name}' at {}", path.display());
        println!("This will remove the workspace and delete the directory.");
        println!();
        print!("Continue? [y/N] ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    println!("Destroying workspace '{name}'...");

    let backend = get_backend()?;
    let ws_id = WorkspaceId::new(name)
        .map_err(|e| anyhow::anyhow!("Invalid workspace name: {e}"))?;

    backend.destroy(&ws_id)
        .map_err(|e| anyhow::anyhow!("Failed to destroy workspace: {e}"))?;

    println!("Workspace '{name}' destroyed.");
    Ok(())
}

/// Attach (reconnect) an orphaned workspace directory.
/// In the git worktree model, this means creating a worktree entry
/// for an existing directory.
#[allow(clippy::too_many_lines)]
pub fn attach(name: &str, revision: Option<&str>) -> Result<()> {
    if name == DEFAULT_WORKSPACE {
        bail!("Cannot attach the default workspace (it's always tracked)");
    }

    ensure_repo_root()?;
    let root = repo_root()?;
    let path = workspace_path(name)?;

    // Check if directory exists
    if !path.exists() {
        bail!(
            "Workspace directory does not exist at {}\n  \
             The directory must exist to attach it.\n  \
             To create a new workspace: maw ws create {name}",
            path.display()
        );
    }

    // Check if workspace is already tracked by git worktree
    let backend = get_backend()?;
    let ws_id = WorkspaceId::new(name)
        .map_err(|e| anyhow::anyhow!("Invalid workspace name: {e}"))?;

    if backend.exists(&ws_id) {
        bail!(
            "Workspace '{name}' is already tracked.\n  \
             Use 'maw ws list' to see all workspaces."
        );
    }

    // Resolve epoch
    let epoch = resolve_epoch(&root, revision)?;

    println!("Attaching workspace '{name}' at epoch {}...", &epoch.as_str()[..12]);

    // Move existing contents to a temp location
    let temp_backup = root.join("ws").join(format!(".{name}-attach-backup"));
    backup_workspace_contents(&path, &temp_backup)?;

    // Create the worktree via backend
    match backend.create(&ws_id, &epoch) {
        Ok(_) => {
            // Move contents back from backup, overwriting git-populated files
            restore_backup_overwrite(&temp_backup, &path)?;
            std::fs::remove_dir_all(&temp_backup).ok();
        }
        Err(e) => {
            // Restore backup on failure
            restore_backup_best_effort(&temp_backup, &path);
            let _ = std::fs::remove_dir_all(&temp_backup);
            bail!(
                "Failed to attach workspace: {e}\n  \
                 Your files have been restored.\n  \
                 Try: maw ws destroy {name} && maw ws create {name}"
            );
        }
    }

    println!();
    println!("Workspace '{name}' attached!");
    println!();
    println!("  Path: {}/", path.display());
    println!();
    println!("  NOTE: Your local files were preserved. They may differ from the");
    println!("  epoch's files. Run 'maw exec {name} -- git status' to see differences.");
    println!();
    println!("To continue working:");
    println!("  maw exec {name} -- git status");

    Ok(())
}

/// Move all workspace contents (except `.git`) into a backup directory,
/// then remove any stale `.git` file/directory so the workspace dir is empty.
fn backup_workspace_contents(
    workspace: &std::path::Path,
    backup: &std::path::Path,
) -> Result<()> {
    std::fs::create_dir_all(backup)
        .with_context(|| format!("Failed to create backup directory: {}", backup.display()))?;

    let entries: Vec<_> = std::fs::read_dir(workspace)
        .with_context(|| format!("Failed to read directory: {}", workspace.display()))?
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_name() != ".git" && e.file_name() != ".jj")
        .collect();

    for entry in &entries {
        let src = entry.path();
        let dst = backup.join(entry.file_name());
        std::fs::rename(&src, &dst).with_context(|| {
            format!(
                "Failed to move {} to backup",
                entry.file_name().to_string_lossy()
            )
        })?;
    }

    // Remove the .git file/directory (stale workspace metadata)
    let git_entry = workspace.join(".git");
    if git_entry.exists() {
        if git_entry.is_dir() {
            std::fs::remove_dir_all(&git_entry).with_context(|| "Failed to remove stale .git directory")?;
        } else {
            std::fs::remove_file(&git_entry).with_context(|| "Failed to remove stale .git file")?;
        }
    }

    // Also clean up .jj if present (legacy)
    let jj_dir = workspace.join(".jj");
    if jj_dir.exists() {
        std::fs::remove_dir_all(&jj_dir).ok();
    }

    Ok(())
}

/// Best-effort restore of backup contents (used on failure paths).
fn restore_backup_best_effort(backup: &std::path::Path, workspace: &std::path::Path) {
    for entry in std::fs::read_dir(backup)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(std::result::Result::ok)
    {
        let src = entry.path();
        let dst = workspace.join(entry.file_name());
        let _ = std::fs::rename(&src, &dst);
    }
}

/// Restore backup contents into workspace, overwriting git-populated files.
fn restore_backup_overwrite(
    backup: &std::path::Path,
    workspace: &std::path::Path,
) -> Result<()> {
    for entry in std::fs::read_dir(backup)
        .with_context(|| "Failed to read backup directory")?
        .filter_map(std::result::Result::ok)
    {
        let src = entry.path();
        let dst = workspace.join(entry.file_name());
        // If git created the file, remove it first
        if dst.exists() {
            if dst.is_dir() {
                std::fs::remove_dir_all(&dst).ok();
            } else {
                std::fs::remove_file(&dst).ok();
            }
        }
        std::fs::rename(&src, &dst).with_context(|| {
            format!(
                "Failed to restore {} from backup",
                entry.file_name().to_string_lossy()
            )
        })?;
    }
    Ok(())
}
