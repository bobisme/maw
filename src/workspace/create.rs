use std::io::{self, Write};
use std::process::Command;

use anyhow::{bail, Context, Result};

use super::{
    ensure_repo_root, jj_cwd, repo_root, workspace_path, workspaces_dir,
    MawConfig, DEFAULT_WORKSPACE,
};

pub(crate) fn create(name: &str, revision: Option<&str>) -> Result<()> {
    let root = ensure_repo_root()?;
    let cwd = jj_cwd()?;
    let path = workspace_path(name)?;

    if path.exists() {
        bail!("Workspace already exists at {}", path.display());
    }

    // Ensure ws directory exists
    let ws_dir = workspaces_dir()?;
    std::fs::create_dir_all(&ws_dir)
        .with_context(|| format!("Failed to create {}", ws_dir.display()))?;

    println!("Creating workspace '{name}' at ws/{name} ...");

    // Determine base revision.
    // In v2 bare model, the default workspace is at ws/default/, not root.
    // @ can't resolve from root (no workspace there), so fall back to the
    // configured branch name (e.g. "main").
    let base = if let Some(rev) = revision {
        rev.to_string()
    } else {
        let check = Command::new("jj")
            .args(["log", "-r", "@", "--no-graph", "-T", "change_id.short()", "--no-pager"])
            .current_dir(&cwd)
            .output();
        match check {
            Ok(o) if o.status.success() => "@".to_string(),
            _ => {
                let config = MawConfig::load(&root).unwrap_or_default();
                config.branch().to_string()
            }
        }
    };

    // Create the workspace
    let output = Command::new("jj")
        .args([
            "workspace",
            "add",
            path.to_str().unwrap(),
            "--name",
            name,
            "-r",
            &base,
        ])
        .current_dir(&cwd)
        .output()
        .context("Failed to run jj workspace add")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "jj workspace add failed: {}\n  Check: maw doctor\n  Verify name is not already used: maw ws list",
            stderr.trim()
        );
    }

    // Create a dedicated commit for this agent to own
    // This prevents divergent commits when multiple agents work concurrently
    let new_output = Command::new("jj")
        .args(["new", "-m", &format!("wip: {name} workspace")])
        .current_dir(&path)
        .output()
        .context("Failed to create agent commit")?;

    if !new_output.status.success() {
        let stderr = String::from_utf8_lossy(&new_output.stderr);
        bail!(
            "Failed to create dedicated commit for workspace: {}\n  The workspace was created but has no dedicated commit.\n  Try: maw exec {name} -- jj new -m \"wip: {name}\"",
            stderr.trim()
        );
    }

    // Get the new commit's change ID for display
    let change_id = Command::new("jj")
        .args(["log", "-r", "@", "--no-graph", "-T", "change_id.short()"])
        .current_dir(&path)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    println!();
    println!("Workspace '{name}' ready!");
    println!();
    println!("  Commit: {change_id} (your dedicated change — jj's stable ID for this commit)");
    println!("  Path:   {}/", path.display());
    println!();
    println!("  IMPORTANT: All file reads, writes, and edits must use this path.");
    println!("  This is your working directory for ALL operations, not just bash.");
    println!();
    println!("To start working:");
    println!();
    println!("  # Set your commit message (like git commit --amend -m):");
    println!("  maw exec {name} -- jj describe -m \"feat: what you're implementing\"");
    println!();
    println!("  # View changes (like git diff / git log):");
    println!("  maw exec {name} -- jj diff");
    println!("  maw exec {name} -- jj log");
    println!();
    println!("  # Other commands (run inside workspace):");
    println!("  maw exec {name} -- cargo test");
    println!();
    println!("Note: jj has no staging area — all edits are tracked automatically.");
    println!("Your changes are always in your commit. Use 'describe' to set the message.");

    Ok(())
}

pub(crate) fn destroy(name: &str, confirm: bool) -> Result<()> {
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
        bail!("Workspace does not exist at {}", path.display());
    }

    if confirm {
        println!("About to destroy workspace '{name}' at {}", path.display());
        println!("This will forget the workspace and delete the directory.");
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

    // Forget from jj (ignore errors if already forgotten)
    let cwd = jj_cwd()?;
    let _ = Command::new("jj")
        .args(["workspace", "forget", name])
        .current_dir(&cwd)
        .status();

    // Remove directory
    std::fs::remove_dir_all(&path)
        .with_context(|| format!("Failed to remove {}", path.display()))?;

    println!("Workspace '{name}' destroyed.");
    println!("  To undo: maw ws restore {name}");
    Ok(())
}

/// Attach (reconnect) an orphaned workspace directory to jj's tracking.
/// An orphaned workspace is one where 'jj workspace forget' was run but
/// the directory still exists in ws/.
pub(crate) fn attach(name: &str, revision: Option<&str>) -> Result<()> {
    if name == DEFAULT_WORKSPACE {
        bail!("Cannot attach the default workspace (it's always tracked)");
    }

    ensure_repo_root()?;
    let root = repo_root()?;
    let cwd = jj_cwd()?;
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

    // Check if workspace is already tracked by jj
    let ws_output = Command::new("jj")
        .args(["workspace", "list"])
        .current_dir(&cwd)
        .output()
        .context("Failed to run jj workspace list")?;

    let ws_list = String::from_utf8_lossy(&ws_output.stdout);
    let is_tracked = ws_list.lines().any(|line| {
        line.split(':')
            .next()
            .is_some_and(|n| n.trim().trim_end_matches('@') == name)
    });

    if is_tracked {
        bail!(
            "Workspace '{name}' is already tracked by jj.\n  \
             Use 'maw ws sync' if the workspace is stale.\n  \
             Use 'maw ws list' to see all workspaces."
        );
    }

    // Determine the revision to attach to (user-specified or default to configured branch)
    let config = MawConfig::load(&root)?;
    let attach_rev = revision.map_or_else(|| config.branch().to_string(), ToString::to_string);

    println!("Attaching workspace '{name}' at revision {attach_rev}...");

    // jj workspace add requires an empty directory, so we need to:
    // 1. Move existing contents to a temp location
    // 2. Run jj workspace add
    // 3. Move contents back (excluding newly-created .jj)
    let temp_backup = root.join("ws").join(format!(".{name}-attach-backup"));

    // Create backup directory
    std::fs::create_dir_all(&temp_backup)
        .with_context(|| format!("Failed to create backup directory: {}", temp_backup.display()))?;

    // Move all contents (except .jj) to backup
    let entries: Vec<_> = std::fs::read_dir(&path)
        .with_context(|| format!("Failed to read directory: {}", path.display()))?
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_name() != ".jj")
        .collect();

    for entry in &entries {
        let src = entry.path();
        let dst = temp_backup.join(entry.file_name());
        std::fs::rename(&src, &dst).with_context(|| {
            format!(
                "Failed to move {} to backup",
                entry.file_name().to_string_lossy()
            )
        })?;
    }

    // Remove the .jj directory (stale workspace metadata)
    let jj_dir = path.join(".jj");
    if jj_dir.exists() {
        std::fs::remove_dir_all(&jj_dir).with_context(|| "Failed to remove stale .jj directory")?;
    }

    // Now the directory should be empty, run jj workspace add
    let output = Command::new("jj")
        .args([
            "workspace",
            "add",
            path.to_str().unwrap(),
            "--name",
            name,
            "-r",
            &attach_rev,
        ])
        .current_dir(&cwd)
        .output()
        .context("Failed to run jj workspace add")?;

    if !output.status.success() {
        // Restore backup on failure
        for entry in std::fs::read_dir(&temp_backup)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(std::result::Result::ok)
        {
            let src = entry.path();
            let dst = path.join(entry.file_name());
            let _ = std::fs::rename(&src, &dst);
        }
        let _ = std::fs::remove_dir_all(&temp_backup);

        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Failed to attach workspace: {}\n  \
             Your files have been restored.\n  \
             Try: maw ws destroy {name} && maw ws create {name}",
            stderr.trim()
        );
    }

    // Move contents back from backup
    for entry in std::fs::read_dir(&temp_backup)
        .with_context(|| "Failed to read backup directory")?
        .filter_map(std::result::Result::ok)
    {
        let src = entry.path();
        let dst = path.join(entry.file_name());
        // If jj created the file, remove it first (jj workspace add populates working copy)
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

    // Clean up backup directory
    std::fs::remove_dir_all(&temp_backup).ok();

    println!();
    println!("Workspace '{name}' attached!");
    println!();
    println!("  Path: {}/", path.display());
    println!();
    println!("  NOTE: Your local files were preserved. They may differ from the");
    println!("  revision's files. Run 'maw exec {name} -- jj status' to see differences.");
    println!();

    // Check if workspace is stale after attaching
    let status_check = Command::new("jj")
        .args(["status"])
        .current_dir(&path)
        .output();

    if let Ok(status) = status_check {
        let stderr = String::from_utf8_lossy(&status.stderr);
        if stderr.contains("working copy is stale") {
            println!("NOTE: Workspace is stale (files may be outdated).");
            println!("  Fix: maw ws sync {name}");
            println!();
        }
    }

    println!("To continue working:");
    println!("  maw exec {name} -- jj status");
    println!("  maw exec {name} -- jj log");

    Ok(())
}
