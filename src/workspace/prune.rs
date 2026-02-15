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

    let jj_workspaces = get_jj_tracked_workspaces(&cwd)?;
    let dir_workspaces = get_directory_workspaces(&ws_dir)?;

    let mut analysis = analyze_workspaces(
        &jj_workspaces,
        &dir_workspaces,
        include_empty,
        &cwd,
    );

    // Sort for consistent output
    analysis.orphaned.sort();
    analysis.missing.sort();
    analysis.empty.sort();

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

    prune_orphaned(&analysis.orphaned, &ws_dir, force);
    prune_missing(&analysis.missing, &cwd, force);
    prune_empty(&analysis.empty, &ws_dir, &cwd, force);
    print_prune_summary(&analysis, total_issues, include_empty, force);

    Ok(())
}

/// Get the set of workspace names that jj tracks.
fn get_jj_tracked_workspaces(
    cwd: &std::path::Path,
) -> Result<std::collections::HashSet<String>> {
    let output = Command::new("jj")
        .args(["workspace", "list"])
        .current_dir(cwd)
        .output()
        .context("Failed to run jj workspace list")?;

    if !output.status.success() {
        bail!(
            "jj workspace list failed: {}\n  To fix: ensure you're in a jj repository",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let ws_list = String::from_utf8_lossy(&output.stdout);
    Ok(ws_list
        .lines()
        .filter_map(|l| l.split(':').next())
        .map(|s| s.trim().trim_end_matches('@').to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// Get the set of workspace directory names present in ws/.
/// Skips symlinks and validates names to prevent path traversal.
fn get_directory_workspaces(
    ws_dir: &std::path::Path,
) -> Result<std::collections::HashSet<String>> {
    if !ws_dir.exists() {
        return Ok(std::collections::HashSet::new());
    }
    Ok(std::fs::read_dir(ws_dir)
        .context("Failed to read ws directory")?
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            let path = entry.path();
            !path.is_symlink() && path.is_dir()
        })
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| validate_workspace_name(name).is_ok())
        .collect())
}

/// Compare jj-tracked workspaces against directory workspaces to find
/// orphaned, missing, and (optionally) empty workspaces.
fn analyze_workspaces(
    jj_workspaces: &std::collections::HashSet<String>,
    dir_workspaces: &std::collections::HashSet<String>,
    include_empty: bool,
    cwd: &std::path::Path,
) -> PruneAnalysis {
    let mut analysis = PruneAnalysis {
        orphaned: Vec::new(),
        missing: Vec::new(),
        empty: Vec::new(),
    };

    for dir_name in dir_workspaces {
        if !jj_workspaces.contains(dir_name) {
            analysis.orphaned.push(dir_name.clone());
        }
    }

    for jj_ws in jj_workspaces {
        if !dir_workspaces.contains(jj_ws) {
            analysis.missing.push(jj_ws.clone());
        }
    }

    if include_empty {
        for jj_ws in jj_workspaces {
            if jj_ws == DEFAULT_WORKSPACE {
                continue;
            }
            if analysis.orphaned.contains(jj_ws) || analysis.missing.contains(jj_ws) {
                continue;
            }
            let diff_output = Command::new("jj")
                .args(["diff", "--stat", "-r", &format!("{jj_ws}@")])
                .current_dir(cwd)
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

    analysis
}

/// Handle orphaned directories (exist on disk but not tracked by jj).
fn prune_orphaned(orphaned: &[String], ws_dir: &std::path::Path, force: bool) {
    if orphaned.is_empty() {
        return;
    }
    println!(
        "Orphaned ({} directory exists but jj forgot the workspace):",
        orphaned.len()
    );
    for name in orphaned {
        let path = ws_dir.join(name);
        if force {
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

/// Handle missing workspaces (jj tracks but directory is gone).
fn prune_missing(missing: &[String], cwd: &std::path::Path, force: bool) {
    if missing.is_empty() {
        return;
    }
    println!(
        "Missing ({} jj tracks workspace but directory is gone):",
        missing.len()
    );
    for name in missing {
        if force {
            let forget_result = Command::new("jj")
                .args(["workspace", "forget", name])
                .current_dir(cwd)
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

/// Handle empty workspaces (tracked, directory exists, but no file changes).
fn prune_empty(
    empty: &[String],
    ws_dir: &std::path::Path,
    cwd: &std::path::Path,
    force: bool,
) {
    if empty.is_empty() {
        return;
    }
    println!(
        "Empty ({} workspaces with no changes):",
        empty.len()
    );
    for name in empty {
        let path = ws_dir.join(name);
        if force {
            if path.is_symlink() {
                println!("  \u{2717} {name}: refused to delete symlink (security)");
                continue;
            }
            let _ = Command::new("jj")
                .args(["workspace", "forget", name])
                .current_dir(cwd)
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

/// Print final prune summary.
fn print_prune_summary(
    analysis: &PruneAnalysis,
    total_issues: usize,
    include_empty: bool,
    force: bool,
) {
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
}
