use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::init::{clean_root_source_files, ensure_workspaces_gitignored};

/// Upgrade a v1 repo (.workspaces/) to v2 bare model (ws/).
///
/// Migrates workspace layout, sets git bare mode, creates coord workspace,
/// and cleans up the old structure. Idempotent — safe to run multiple times.
pub fn run() -> Result<()> {
    println!("Checking repo for upgrade...");
    println!();

    // Step 1: Check if already v2
    if is_already_v2()? {
        println!("Already v2 (bare repo model). Nothing to do.");
        return Ok(());
    }

    // Step 2: Check for uncommitted changes and auto-commit if needed
    auto_commit_wip()?;

    // Step 3: Destroy all agent workspaces in .workspaces/
    destroy_old_workspaces()?;

    // Step 4: Create ws/ directory
    fs::create_dir_all("ws").context("Failed to create ws/ directory")?;
    println!("[OK] Created ws/ directory");

    // Step 5: Create coord workspace
    create_coord_workspace()?;

    // Step 6: Forget default workspace
    forget_default_workspace()?;

    // Step 7: Set git core.bare = true
    set_git_bare()?;

    // Step 8: Clean root source files
    clean_root_source_files()?;

    // Step 9: Remove old .workspaces/ directory
    remove_old_workspaces_dir()?;

    // Step 10: Update .gitignore
    update_gitignore()?;

    // Print verification instructions
    let cwd = std::env::current_dir().context("Could not determine current directory")?;
    let coord_path = cwd.join("ws").join("coord");

    println!();
    println!("Upgrade complete! (v1 -> v2 bare repo model)");
    println!();
    println!("  Coord workspace: {}/", coord_path.display());
    println!();
    println!("Verify:");
    println!("  jj workspace list          # should show coord workspace");
    println!("  git config core.bare       # should be true");
    println!("  ls ws/                     # should have coord/");
    println!("  ls .workspaces/ 2>/dev/null # should not exist");
    println!();
    println!("Next: maw ws create <agent-name>");

    Ok(())
}

/// Check if the repo is already in v2 layout.
/// v2 = ws/ dir exists AND no default workspace in jj.
fn is_already_v2() -> Result<bool> {
    let ws_exists = Path::new("ws").exists();
    if !ws_exists {
        return Ok(false);
    }

    let output = Command::new("jj")
        .args(["workspace", "list"])
        .output()
        .context("Failed to run jj workspace list")?;

    let ws_list = String::from_utf8_lossy(&output.stdout);
    let has_default = ws_list.lines().any(|line| {
        line.split(':')
            .next()
            .is_some_and(|n| n.trim().trim_end_matches('@') == "default")
    });

    Ok(!has_default)
}

/// Check for uncommitted changes and auto-commit them as WIP.
fn auto_commit_wip() -> Result<()> {
    let output = Command::new("jj")
        .args(["status"])
        .output()
        .context("Failed to run jj status")?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    // If there are working-copy changes (not just "The working copy has no changes")
    let has_changes = !stdout.contains("nothing to") && !stdout.contains("no changes");

    // Check if the current working copy has modifications
    let diff_output = Command::new("jj")
        .args(["diff", "--stat"])
        .output()
        .context("Failed to run jj diff")?;

    let diff_stdout = String::from_utf8_lossy(&diff_output.stdout);
    let has_diff = !diff_stdout.trim().is_empty();

    if has_changes || has_diff {
        println!("[WARN] Uncommitted changes detected — auto-committing as WIP");
        let commit = Command::new("jj")
            .args(["commit", "-m", "wip: auto-commit before upgrade"])
            .output()
            .context("Failed to auto-commit WIP")?;

        if commit.status.success() {
            println!("[OK] Auto-committed WIP changes");
        } else {
            let stderr = String::from_utf8_lossy(&commit.stderr);
            println!("[WARN] Auto-commit returned: {}", stderr.trim());
            // Don't bail — the commit might have been empty, which is fine
        }
    } else {
        println!("[OK] No uncommitted changes");
    }

    Ok(())
}

/// Destroy all agent workspaces under .workspaces/.
fn destroy_old_workspaces() -> Result<()> {
    let old_dir = Path::new(".workspaces");

    if !old_dir.exists() {
        println!("[OK] No .workspaces/ directory (already removed or never existed)");
        return Ok(());
    }

    // Get list of jj workspaces
    let output = Command::new("jj")
        .args(["workspace", "list"])
        .output()
        .context("Failed to run jj workspace list")?;

    let ws_list = String::from_utf8_lossy(&output.stdout);

    // Parse workspace names (exclude "default" — we handle that separately)
    let workspace_names: Vec<String> = ws_list
        .lines()
        .filter_map(|l| l.split(':').next())
        .map(|s| s.trim().trim_end_matches('@').to_string())
        .filter(|s| !s.is_empty() && s != "default")
        .collect();

    // Forget each agent workspace that has a directory under .workspaces/
    let mut forgotten = 0;
    for ws in &workspace_names {
        let ws_path = old_dir.join(ws);
        if ws_path.exists() {
            let forget = Command::new("jj")
                .args(["workspace", "forget", ws])
                .output();

            if let Ok(out) = forget
                && out.status.success()
            {
                forgotten += 1;
            }
        }
    }

    // Also scan the directory for any workspace dirs not tracked by jj
    if let Ok(entries) = fs::read_dir(old_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if entry.path().is_dir() && !workspace_names.contains(&name_str.to_string()) {
                // Try to forget it from jj just in case
                let _ = Command::new("jj")
                    .args(["workspace", "forget", &name_str])
                    .output();
            }
        }
    }

    if forgotten > 0 {
        println!("[OK] Forgot {forgotten} old workspace(s) from .workspaces/");
    } else {
        println!("[OK] No agent workspaces to forget in .workspaces/");
    }

    Ok(())
}

/// Create the coord workspace in ws/coord.
fn create_coord_workspace() -> Result<()> {
    let coord_path = Path::new("ws").join("coord");

    // Check if coord already exists
    let output = Command::new("jj")
        .args(["workspace", "list"])
        .output()
        .context("Failed to run jj workspace list")?;

    let ws_list = String::from_utf8_lossy(&output.stdout);
    let has_coord = ws_list.lines().any(|line| {
        line.split(':')
            .next()
            .is_some_and(|n| n.trim().trim_end_matches('@') == "coord")
    });

    if has_coord && coord_path.exists() {
        println!("[OK] Coord workspace already exists");
        return Ok(());
    }

    if !has_coord {
        let add = Command::new("jj")
            .args([
                "workspace",
                "add",
                coord_path.to_str().unwrap_or("ws/coord"),
                "--name",
                "coord",
            ])
            .output()
            .context("Failed to create coord workspace")?;

        if !add.status.success() {
            let stderr = String::from_utf8_lossy(&add.stderr);
            bail!(
                "Failed to create coord workspace: {}\n  Try: jj workspace add ws/coord --name coord",
                stderr.trim()
            );
        }
    }

    // Rebase coord onto main (ignore errors if main doesn't exist yet)
    let _ = Command::new("jj")
        .args(["rebase", "-r", "coord@", "-d", "main"])
        .output();

    println!("[OK] Created coord workspace at ws/coord/");

    Ok(())
}

/// Forget the default workspace if it exists.
fn forget_default_workspace() -> Result<()> {
    let output = Command::new("jj")
        .args(["workspace", "list"])
        .output()
        .context("Failed to run jj workspace list")?;

    let ws_list = String::from_utf8_lossy(&output.stdout);
    let has_default = ws_list.lines().any(|line| {
        line.split(':')
            .next()
            .is_some_and(|n| n.trim().trim_end_matches('@') == "default")
    });

    if !has_default {
        println!("[OK] Default workspace already forgotten");
        return Ok(());
    }

    let forget = Command::new("jj")
        .args(["workspace", "forget", "default"])
        .output()
        .context("Failed to forget default workspace")?;

    if forget.status.success() {
        println!("[OK] Forgot default workspace");
    } else {
        let stderr = String::from_utf8_lossy(&forget.stderr);
        bail!(
            "Failed to forget default workspace: {}\n  Try: jj workspace forget default",
            stderr.trim()
        );
    }

    Ok(())
}

/// Set git core.bare = true.
fn set_git_bare() -> Result<()> {
    let check = Command::new("git")
        .args(["config", "core.bare"])
        .output();

    if let Ok(out) = &check {
        let val = String::from_utf8_lossy(&out.stdout);
        if val.trim() == "true" {
            println!("[OK] git core.bare already true");
            return Ok(());
        }
    }

    let output = Command::new("git")
        .args(["config", "core.bare", "true"])
        .output()
        .context("Failed to set git core.bare")?;

    if output.status.success() {
        println!("[OK] Set git core.bare = true");
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Failed to set git core.bare: {}\n  Try: git config core.bare true",
            stderr.trim()
        );
    }

    Ok(())
}

/// Remove the old .workspaces/ directory.
fn remove_old_workspaces_dir() -> Result<()> {
    let old_dir = Path::new(".workspaces");

    if !old_dir.exists() {
        println!("[OK] .workspaces/ already removed");
        return Ok(());
    }

    fs::remove_dir_all(old_dir).context("Failed to remove .workspaces/ directory")?;
    println!("[OK] Removed .workspaces/ directory");

    Ok(())
}

/// Update .gitignore: replace .workspaces/ with ws/ if needed.
fn update_gitignore() -> Result<()> {
    let gitignore_path = Path::new(".gitignore");

    if !gitignore_path.exists() {
        // Just ensure ws/ is gitignored using the shared function
        return ensure_workspaces_gitignored();
    }

    let content = fs::read_to_string(gitignore_path).context("Failed to read .gitignore")?;

    // Replace .workspaces/ references with ws/
    let has_old = content.lines().any(|line| {
        let line = line.trim();
        line == ".workspaces"
            || line == ".workspaces/"
            || line == "/.workspaces"
            || line == "/.workspaces/"
    });

    if has_old {
        let new_content: String = content
            .lines()
            .map(|line| {
                let trimmed = line.trim();
                if trimmed == ".workspaces"
                    || trimmed == ".workspaces/"
                    || trimmed == "/.workspaces"
                    || trimmed == "/.workspaces/"
                {
                    "ws/"
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        // Ensure trailing newline
        let new_content = if new_content.ends_with('\n') {
            new_content
        } else {
            format!("{new_content}\n")
        };

        fs::write(gitignore_path, new_content).context("Failed to update .gitignore")?;
        println!("[OK] Updated .gitignore: .workspaces/ -> ws/");
    } else {
        // Just ensure ws/ is in there
        ensure_workspaces_gitignored()?;
    }

    Ok(())
}
