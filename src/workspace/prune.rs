use std::process::Command;

use anyhow::{bail, Context, Result};

use super::{jj_cwd, validate_workspace_name, workspaces_dir, DEFAULT_WORKSPACE};

/// Result of analyzing workspaces for pruning
#[derive(Debug)]
struct PruneAnalysis {
    /// Directories in ws/ that jj doesn't know about
    orphaned: Vec<String>,
    /// Workspaces jj tracks but directories are missing
    missing: Vec<String>,
    /// Workspaces with no changes (empty working copies)
    empty: Vec<String>,
}

pub(crate) fn prune(force: bool, include_empty: bool) -> Result<()> {
    let cwd = jj_cwd()?;
    let ws_dir = workspaces_dir()?;

    // Get workspaces jj knows about
    let output = Command::new("jj")
        .args(["workspace", "list"])
        .current_dir(&cwd)
        .output()
        .context("Failed to run jj workspace list")?;

    if !output.status.success() {
        bail!(
            "jj workspace list failed: {}\n  To fix: ensure you're in a jj repository",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let ws_list = String::from_utf8_lossy(&output.stdout);

    // Parse jj-tracked workspaces (format: "name@: change_id ..." or "name: change_id ...")
    let jj_workspaces: std::collections::HashSet<String> = ws_list
        .lines()
        .filter_map(|l| l.split(':').next())
        .map(|s| s.trim().trim_end_matches('@').to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // Get directories in ws/
    // Security: validate names and skip symlinks to prevent traversal attacks
    let dir_workspaces: std::collections::HashSet<String> = if ws_dir.exists() {
        std::fs::read_dir(&ws_dir)
            .context("Failed to read ws directory")?
            .filter_map(std::result::Result::ok)
            .filter(|entry| {
                let path = entry.path();
                // Skip symlinks - they could point anywhere
                if path.is_symlink() {
                    return false;
                }
                path.is_dir()
            })
            .filter_map(|entry| entry.file_name().into_string().ok())
            // Validate name to prevent path traversal
            .filter(|name| validate_workspace_name(name).is_ok())
            .collect()
    } else {
        std::collections::HashSet::new()
    };

    let mut analysis = PruneAnalysis {
        orphaned: Vec::new(),
        missing: Vec::new(),
        empty: Vec::new(),
    };

    // Find orphaned: directories that exist but jj doesn't track
    for dir_name in &dir_workspaces {
        if !jj_workspaces.contains(dir_name) {
            analysis.orphaned.push(dir_name.clone());
        }
    }

    // Find missing: jj tracks but directory doesn't exist
    for jj_ws in &jj_workspaces {
        if !dir_workspaces.contains(jj_ws) {
            analysis.missing.push(jj_ws.clone());
        }
    }

    // Find empty workspaces (if requested)
    if include_empty {
        for jj_ws in &jj_workspaces {
            if jj_ws == DEFAULT_WORKSPACE {
                continue; // don't suggest pruning the default workspace
            }
            // Skip workspaces that are already in orphaned or missing lists
            if analysis.orphaned.contains(jj_ws) || analysis.missing.contains(jj_ws) {
                continue;
            }
            // Check if workspace has changes using jj diff
            let diff_output = Command::new("jj")
                .args(["diff", "--stat", "-r", &format!("{jj_ws}@")])
                .current_dir(&cwd)
                .output();

            if let Ok(diff) = diff_output
                && diff.status.success() {
                    let diff_text = String::from_utf8_lossy(&diff.stdout);
                    if diff_text.trim().is_empty() {
                        analysis.empty.push(jj_ws.clone());
                    }
                }
        }
    }

    // Sort for consistent output
    analysis.orphaned.sort();
    analysis.missing.sort();
    analysis.empty.sort();

    // Report findings
    let total_issues = analysis.orphaned.len() + analysis.missing.len() + analysis.empty.len();

    if total_issues == 0 {
        println!("No workspaces need pruning.");
        if !include_empty {
            println!("  (Use --empty to also check for workspaces with no changes)");
        }
        return Ok(());
    }

    if force {
        println!("Pruning workspaces...");
    } else {
        println!("=== Prune Preview ===");
        println!("(Use --force to actually delete)");
    }
    println!();

    // Handle orphaned directories
    if !analysis.orphaned.is_empty() {
        println!(
            "Orphaned ({} directory exists but jj forgot the workspace):",
            analysis.orphaned.len()
        );
        for name in &analysis.orphaned {
            let path = ws_dir.join(name);
            if force {
                // Defense in depth: check symlink again before deletion
                if path.is_symlink() {
                    println!("  \u{2717} {name}: refused to delete symlink (security)");
                    continue;
                }
                if let Err(e) = std::fs::remove_dir_all(&path) {
                    println!("  \u{2717} {name}: failed to delete - {e}");
                } else {
                    println!("  \u{2713} {name}: deleted");
                }
            } else {
                println!("  - {name}");
                println!("      Path: {}/", path.display());
            }
        }
        println!();
    }

    // Handle missing workspaces (jj tracks but no directory)
    if !analysis.missing.is_empty() {
        println!(
            "Missing ({} jj tracks workspace but directory is gone):",
            analysis.missing.len()
        );
        for name in &analysis.missing {
            if force {
                let forget_result = Command::new("jj")
                    .args(["workspace", "forget", name])
                    .current_dir(&cwd)
                    .output();

                match forget_result {
                    Ok(out) if out.status.success() => {
                        println!("  \u{2713} {name}: forgotten from jj");
                    }
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        println!("  \u{2717} {name}: failed to forget - {}", stderr.trim());
                    }
                    Err(e) => {
                        println!("  \u{2717} {name}: failed to forget - {e}");
                    }
                }
            } else {
                println!("  - {name}");
                println!("      Would run: jj workspace forget {name}");
            }
        }
        println!();
    }

    // Handle empty workspaces
    if !analysis.empty.is_empty() {
        println!(
            "Empty ({} workspaces with no changes):",
            analysis.empty.len()
        );
        for name in &analysis.empty {
            let path = ws_dir.join(name);
            if force {
                // Defense in depth: check symlink before deletion
                if path.is_symlink() {
                    println!("  \u{2717} {name}: refused to delete symlink (security)");
                    continue;
                }
                // First forget from jj, then delete directory
                let _ = Command::new("jj")
                    .args(["workspace", "forget", name])
                    .current_dir(&cwd)
                    .status();

                if path.exists() {
                    if let Err(e) = std::fs::remove_dir_all(&path) {
                        println!("  \u{2717} {name}: failed to delete - {e}");
                    } else {
                        println!("  \u{2713} {name}: deleted");
                    }
                } else {
                    println!("  \u{2713} {name}: forgotten");
                }
            } else {
                println!("  - {name}");
                println!("      Path: {}/", path.display());
            }
        }
        println!();
    }

    // Summary
    if force {
        let deleted = analysis.orphaned.len() + analysis.empty.len();
        let forgotten = analysis.missing.len();
        println!(
            "Pruned: {deleted} deleted, {forgotten} forgotten from jj"
        );
    } else {
        println!("=== Summary ===");
        println!(
            "Would prune {} workspace(s): {} orphaned, {} missing, {} empty",
            total_issues,
            analysis.orphaned.len(),
            analysis.missing.len(),
            analysis.empty.len()
        );
        println!();
        println!("To prune:");
        if include_empty {
            println!("  maw ws prune --empty --force");
        } else {
            println!("  maw ws prune --force");
        }
    }

    Ok(())
}
