use std::process::Command;

use anyhow::{bail, Context, Result};

use super::{jj_cwd, repo_root, workspace_path, DEFAULT_WORKSPACE};

/// Restore a previously destroyed workspace by reverting the `jj workspace forget`
/// operation from jj's operation log.
///
/// Recovery strategy:
/// 1. Find the most recent `forget workspace <name>` operation in `jj op log`
/// 2. Run `jj op revert <op-id>` to undo the forget
/// 3. If the directory doesn't exist after revert, run `jj workspace update-stale`
/// 4. If that still doesn't work, fall back to re-creating and restoring content
pub fn restore(name: &str) -> Result<()> {
    if name == DEFAULT_WORKSPACE {
        bail!("Cannot restore the default workspace");
    }

    let root = repo_root()?;
    let cwd = jj_cwd()?;
    let path = workspace_path(name)?;

    if path.exists() {
        bail!(
            "Workspace '{name}' already exists at {}\n  \
             Nothing to restore. Use 'maw ws list' to see all workspaces.",
            path.display()
        );
    }

    // Find the forget operation in jj op log
    let op_id = find_forget_operation(name, &cwd)?;

    println!("Restoring workspace '{name}'...");
    println!("  Found forget operation: {op_id}");

    // Revert the forget operation
    let revert_output = Command::new("jj")
        .args(["op", "revert", &op_id])
        .current_dir(&cwd)
        .output()
        .context("Failed to run jj op revert")?;

    if !revert_output.status.success() {
        let stderr = String::from_utf8_lossy(&revert_output.stderr);
        bail!(
            "jj op revert failed: {}\n  \
             To fix: try creating a fresh workspace with 'maw ws create {name}'",
            stderr.trim()
        );
    }

    // Check if the directory was rematerialized
    if !path.exists() {
        println!("  Directory not yet materialized, running workspace update-stale...");

        // Try update-stale to rematerialize the workspace
        let update_output = Command::new("jj")
            .args(["workspace", "update-stale"])
            .current_dir(&cwd)
            .output()
            .context("Failed to run jj workspace update-stale")?;

        if !update_output.status.success() {
            let stderr = String::from_utf8_lossy(&update_output.stderr);
            // Log but don't bail — we have a fallback
            eprintln!("  WARNING: update-stale failed: {}", stderr.trim());
        }
    }

    // If directory still doesn't exist, use the fallback strategy:
    // forget the stale tracking, re-create workspace, restore content from the old commit
    if !path.exists() {
        println!("  Falling back to re-create + restore strategy...");
        fallback_restore(name, &root, &cwd, &path)?;
    }

    // Get workspace info for the success message
    let change_id = Command::new("jj")
        .args([
            "log",
            "-r",
            "@",
            "--no-graph",
            "-T",
            "change_id.short()",
            "--no-pager",
        ])
        .current_dir(&path)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    println!();
    println!("Workspace '{name}' restored!");
    println!();
    println!("  Commit: {change_id}");
    println!("  Path:   {}/", path.display());
    println!();
    println!("To continue working:");
    println!();
    println!("  maw exec {name} -- jj status");
    println!("  maw exec {name} -- jj log");
    println!("  maw exec {name} -- jj diff");

    Ok(())
}

/// Search `jj op log` for the most recent `forget workspace <name>` operation.
/// Returns the short operation ID.
fn find_forget_operation(name: &str, cwd: &std::path::Path) -> Result<String> {
    let output = Command::new("jj")
        .args([
            "op",
            "log",
            "--no-graph",
            "-T",
            r#"self.id().short() ++ " " ++ description ++ "\n""#,
            "--no-pager",
        ])
        .current_dir(cwd)
        .output()
        .context("Failed to run jj op log")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("jj op log failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let needle = format!("forget workspace {name}");

    for line in stdout.lines() {
        if line.contains(&needle) {
            // Format: "<op-id> <description>"
            let op_id = line
                .split_whitespace()
                .next()
                .context("Failed to parse operation ID from jj op log")?;
            return Ok(op_id.to_string());
        }
    }

    bail!(
        "Could not find a 'forget workspace {name}' operation in jj op log.\n  \
         The workspace may not have been destroyed via 'maw ws destroy',\n  \
         or the operation log may have been truncated.\n  \
         To create a fresh workspace: maw ws create {name}"
    );
}

/// Fallback restore strategy when op revert didn't rematerialize the directory.
///
/// 1. Find the change ID of the old commit that was being tracked
/// 2. Forget the stale workspace tracking (since directory doesn't exist)
/// 3. Create a fresh workspace with `jj workspace add`
/// 4. Restore content from the old commit
/// 5. Abandon the orphaned old commit
fn fallback_restore(
    name: &str,
    _root: &std::path::Path,
    cwd: &std::path::Path,
    path: &std::path::Path,
) -> Result<()> {
    // Find the change ID of the workspace's current commit
    // After op revert, the workspace is tracked again but may not have a directory.
    // We can get the change ID from `jj workspace list`.
    let ws_list_output = Command::new("jj")
        .args(["workspace", "list", "--no-pager"])
        .current_dir(cwd)
        .output()
        .context("Failed to run jj workspace list")?;

    let ws_list = String::from_utf8_lossy(&ws_list_output.stdout);

    // Parse workspace list to find the change ID for our workspace.
    // Format is like: "workspace-name: change-id@user timestamp description"
    let old_change_id = ws_list
        .lines()
        .find(|line| {
            line.split(':')
                .next()
                .is_some_and(|n| n.trim().trim_end_matches('@') == name)
        })
        .and_then(|line| {
            // After the "name: " prefix, the next token is the change ID
            let after_colon = line.split(':').nth(1)?.trim();
            after_colon.split_whitespace().next()
        })
        .map(ToString::to_string);

    // Forget the stale workspace tracking (no directory = can't function)
    let _ = Command::new("jj")
        .args(["workspace", "forget", name])
        .current_dir(cwd)
        .output();

    // Ensure ws/ directory exists
    let ws_dir = cwd
        .parent()
        .unwrap_or(cwd)
        .parent()
        .unwrap_or(cwd);
    let ws_parent = ws_dir.join("ws");
    std::fs::create_dir_all(&ws_parent).ok();

    // Create fresh workspace
    let add_output = Command::new("jj")
        .args([
            "workspace",
            "add",
            path.to_str().unwrap(),
            "--name",
            name,
        ])
        .current_dir(cwd)
        .output()
        .context("Failed to run jj workspace add")?;

    if !add_output.status.success() {
        let stderr = String::from_utf8_lossy(&add_output.stderr);
        bail!(
            "Failed to re-create workspace during restore: {}\n  \
             Try: maw ws create {name}",
            stderr.trim()
        );
    }

    // Create a dedicated commit (matching create behavior)
    let _ = Command::new("jj")
        .args(["new", "-m", &format!("wip: {name} workspace (restored)")])
        .current_dir(path)
        .output();

    // If we found the old change ID, restore content from it
    if let Some(ref old_id) = old_change_id {
        let restore_output = Command::new("jj")
            .args(["restore", "--from", old_id])
            .current_dir(path)
            .output()
            .context("Failed to run jj restore")?;

        if restore_output.status.success() {
            // Abandon the orphaned old commit
            let _ = Command::new("jj")
                .args(["abandon", old_id])
                .current_dir(cwd)
                .output();

            println!("  Restored content from previous commit ({old_id})");
        } else {
            let stderr = String::from_utf8_lossy(&restore_output.stderr);
            eprintln!(
                "  WARNING: Could not restore content from old commit: {}",
                stderr.trim()
            );
            eprintln!("  The workspace was re-created but may be empty.");
            eprintln!("  Old commit {old_id} is still available if you need it.");
        }
    } else {
        eprintln!("  WARNING: Could not identify old commit. Workspace re-created but may be empty.");
    }

    Ok(())
}
