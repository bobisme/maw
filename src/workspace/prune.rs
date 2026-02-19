use anyhow::Result;

use crate::backend::WorkspaceBackend;
use crate::model::types::WorkspaceId;

use super::{get_backend, validate_workspace_name, workspaces_dir, DEFAULT_WORKSPACE};

/// Result of analyzing workspaces for pruning
#[derive(Debug)]
struct PruneAnalysis {
    /// Directories in ws/ that git worktree doesn't know about
    orphaned: Vec<String>,
    /// Workspaces git tracks but directories are missing
    missing: Vec<String>,
    /// Workspaces with no changes (empty working copies)
    empty: Vec<String>,
}

pub fn prune(force: bool, include_empty: bool) -> Result<()> {
    let backend = get_backend()?;
    let ws_dir = workspaces_dir()?;

    let tracked_workspaces = backend.list()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let tracked_names: std::collections::HashSet<String> = tracked_workspaces
        .iter()
        .map(|ws| ws.id.as_str().to_string())
        .collect();

    let dir_workspaces = get_directory_workspaces(&ws_dir)?;

    let mut analysis = analyze_workspaces(
        &tracked_names,
        &dir_workspaces,
        include_empty,
        &backend,
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
    prune_missing(&analysis.missing, &backend, force);
    prune_empty(&analysis.empty, &backend, force);
    print_prune_summary(&analysis, total_issues, include_empty, force);

    Ok(())
}

/// Get the set of workspace directory names present in ws/.
fn get_directory_workspaces(
    ws_dir: &std::path::Path,
) -> Result<std::collections::HashSet<String>> {
    if !ws_dir.exists() {
        return Ok(std::collections::HashSet::new());
    }
    Ok(std::fs::read_dir(ws_dir)
        .map_err(|e| anyhow::anyhow!("Failed to read ws directory: {e}"))?
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            let path = entry.path();
            !path.is_symlink() && path.is_dir()
        })
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| validate_workspace_name(name).is_ok())
        .collect())
}

/// Compare tracked workspaces against directory workspaces.
fn analyze_workspaces(
    tracked_names: &std::collections::HashSet<String>,
    dir_workspaces: &std::collections::HashSet<String>,
    include_empty: bool,
    backend: &impl WorkspaceBackend,
) -> PruneAnalysis {
    let mut analysis = PruneAnalysis {
        orphaned: Vec::new(),
        missing: Vec::new(),
        empty: Vec::new(),
    };

    for dir_name in dir_workspaces {
        if !tracked_names.contains(dir_name) {
            analysis.orphaned.push(dir_name.clone());
        }
    }

    // In git worktree model, "missing" means directory is gone but
    // git still tracks the worktree. This is handled by git worktree prune.
    // We don't need to detect this separately since destroy() calls prune.

    if include_empty {
        for name in tracked_names {
            if name == DEFAULT_WORKSPACE {
                continue;
            }
            if analysis.orphaned.contains(name) {
                continue;
            }
            if let Ok(ws_id) = WorkspaceId::new(name) {
                if let Ok(snapshot) = backend.snapshot(&ws_id) {
                    if snapshot.is_empty() {
                        analysis.empty.push(name.clone());
                    }
                }
            }
        }
    }

    analysis
}

/// Handle orphaned directories (exist on disk but not tracked by git worktree).
fn prune_orphaned(orphaned: &[String], ws_dir: &std::path::Path, force: bool) {
    if orphaned.is_empty() {
        return;
    }
    println!(
        "Orphaned ({} — directory exists but not tracked as worktree):",
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

/// Handle missing workspaces (git tracks but directory is gone).
fn prune_missing(missing: &[String], backend: &impl WorkspaceBackend, force: bool) {
    if missing.is_empty() {
        return;
    }
    println!(
        "Missing ({} — tracked as worktree but directory is gone):",
        missing.len()
    );
    for name in missing {
        if force {
            if let Ok(ws_id) = WorkspaceId::new(name) {
                match backend.destroy(&ws_id) {
                    Ok(()) => println!("  \u{2713} {name}: removed from git"),
                    Err(e) => println!("  \u{2717} {name}: failed to remove - {e}"),
                }
            }
        } else {
            println!("  - {name}");
            println!("      Would remove from git worktree tracking");
        }
    }
    println!();
}

/// Handle empty workspaces (tracked, directory exists, but no file changes).
fn prune_empty(empty: &[String], backend: &impl WorkspaceBackend, force: bool) {
    if empty.is_empty() {
        return;
    }
    println!(
        "Empty ({} workspaces with no changes):",
        empty.len()
    );
    for name in empty {
        if force {
            if let Ok(ws_id) = WorkspaceId::new(name) {
                match backend.destroy(&ws_id) {
                    Ok(()) => println!("  \u{2713} {name}: deleted"),
                    Err(e) => println!("  \u{2717} {name}: failed to delete - {e}"),
                }
            }
        } else {
            println!("  - {name}");
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
            "Pruned: {deleted} deleted, {forgotten} removed from tracking"
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
