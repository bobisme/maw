use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use maw_git::GitRepo as _;

/// Upgrade a v1 repo (.workspaces/) to v2 bare model (ws/).
///
/// Migrates workspace layout, sets git bare mode, moves default workspace
/// to ws/default/, and cleans up the old structure. Idempotent — safe to
/// run multiple times.
pub fn run() -> Result<()> {
    println!("Checking repo for upgrade...");
    println!();

    // Step 1: Check if already v2
    if is_already_v2() {
        // Still apply config upgrades for existing v2 repos
        set_conflict_marker_style();
        println!();
        println!("Already v2 (bare repo model). Applied config updates.");
        return Ok(());
    }

    // Step 2: Check for uncommitted changes and auto-commit if needed
    auto_commit_wip()?;

    // Step 3: Destroy all agent workspaces in .workspaces/
    destroy_old_workspaces()?;

    // Step 4: Create ws/ directory
    fs::create_dir_all("ws").context("Failed to create ws/ directory")?;
    println!("[OK] Created ws/ directory");

    // Step 5: Move default workspace to ws/default/
    relocate_default_workspace()?;

    // Step 6: Set git core.bare = true
    set_git_bare()?;

    // Step 6b: Fix git HEAD (points to branch ref, not detached)
    fix_git_head()?;

    // Step 6c: Set conflict-marker-style = snapshot (agent-safe markers)
    set_conflict_marker_style();

    // Step 7: Clean root source files
    clean_root_source_files()?;

    // Step 8: Remove old .workspaces/ directory
    remove_old_workspaces_dir()?;

    // Print verification instructions
    let cwd = std::env::current_dir().context("Could not determine current directory")?;
    let default_path = cwd.join("ws").join("default");

    println!();
    println!("Upgrade complete! (v1 -> v2 bare repo model)");
    println!();
    println!("  Default workspace: {}/", default_path.display());
    println!();
    println!("Verify:");
    println!("  maw ws list                # should include default workspace");
    println!("  git config core.bare       # should be true");
    println!("  ls ws/                     # should have default/");
    println!("  ls .workspaces/ 2>/dev/null # should not exist");
    println!();
    println!("Next: maw ws create <agent-name>");

    Ok(())
}

/// Check if the repo is already in v2 layout.
/// v2 = ws/default/ directory exists.
fn is_already_v2() -> bool {
    Path::new("ws").join("default").exists()
}

/// Check for uncommitted changes and auto-commit them as WIP.
fn auto_commit_wip() -> Result<()> {
    let output = Command::new("jj")
        .args(["status"])
        .output()
        .context("Failed to run jj status")?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    // jj says "The working copy has no changes" when clean.
    // The diff --stat check below is the real test; this is just an early hint.
    let has_changes = !stdout.contains("no changes");

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

/// Move the default workspace from root to ws/default/.
///
/// In v1, "default" lives at the repo root. We forget it and recreate
/// at ws/default/ so the root becomes bare.
fn relocate_default_workspace() -> Result<()> {
    let default_path = Path::new("ws").join("default");

    if default_path.exists() {
        println!("[OK] Default workspace already at ws/default/");
        return Ok(());
    }

    // Check if default workspace exists (at root, in v1)
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

    // Forget the root-based default workspace
    if has_default {
        let forget = Command::new("jj")
            .args(["workspace", "forget", "default"])
            .output()
            .context("Failed to forget default workspace")?;

        if !forget.status.success() {
            let stderr = String::from_utf8_lossy(&forget.stderr);
            bail!(
                "Failed to forget default workspace: {}\n  Try: jj workspace forget default",
                stderr.trim()
            );
        }
    }

    // Recreate default at ws/default/, parented on main.
    let add = Command::new("jj")
        .args([
            "workspace",
            "add",
            default_path.to_str().unwrap_or("ws/default"),
            "--name",
            "default",
            "-r",
            "main",
        ])
        .output()
        .context("Failed to create default workspace at ws/default/")?;

    if !add.status.success() {
        // main might not exist — retry without -r
        let add_fallback = Command::new("jj")
            .args([
                "workspace",
                "add",
                default_path.to_str().unwrap_or("ws/default"),
                "--name",
                "default",
            ])
            .output()
            .context("Failed to create default workspace at ws/default/")?;

        if !add_fallback.status.success() {
            let stderr = String::from_utf8_lossy(&add_fallback.stderr);
            bail!(
                "Failed to create default workspace: {}\n  Try: jj workspace add ws/default --name default",
                stderr.trim()
            );
        }
    }

    println!("[OK] Moved default workspace to ws/default/");

    Ok(())
}

/// Set git core.bare = true.
fn set_git_bare() -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    let repo = maw_git::GixRepo::open(&cwd)
        .map_err(|e| anyhow::anyhow!("failed to open repo: {e}"))?;

    if let Ok(Some(val)) = repo.read_config("core.bare")
        && val.trim() == "true" {
            println!("[OK] git core.bare already true");
            return Ok(());
        }

    repo.write_config("core.bare", "true")
        .map_err(|e| anyhow::anyhow!("Failed to set git core.bare: {e}\n  Try: git config core.bare true"))?;
    println!("[OK] Set git core.bare = true");

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

// TODO(gix): symbolic-ref HEAD is not available via GitRepo trait.
// Keep CLI for now.
fn fix_git_head() -> Result<()> {
    let head = Command::new("git").args(["symbolic-ref", "HEAD"]).output();
    if let Ok(out) = &head
        && out.status.success()
    {
        println!("[OK] git HEAD already points to branch ref");
        return Ok(());
    }

    let branch = crate::workspace::MawConfig::load(Path::new("."))
        .map_or_else(|_| "main".to_string(), |cfg| cfg.branch().to_string());
    let target = format!("refs/heads/{branch}");

    let set = Command::new("git")
        .args(["symbolic-ref", "HEAD", &target])
        .output()
        .context("Failed to set git HEAD symbolic ref")?;

    if !set.status.success() {
        let stderr = String::from_utf8_lossy(&set.stderr);
        bail!(
            "Failed to set git HEAD to {target}: {}\n  Try: git symbolic-ref HEAD {target}",
            stderr.trim()
        );
    }

    println!("[OK] Set git HEAD -> {target}");
    Ok(())
}

fn set_conflict_marker_style() {
    println!("[OK] Skipping jj conflict-marker-style (not required in Manifold mode)");
}

// TODO(gix): `git ls-tree -r --name-only HEAD` requires recursive tree walk.
// Keep CLI.
fn clean_root_source_files() -> Result<()> {
    let list = Command::new("git")
        .args(["ls-tree", "-r", "--name-only", "HEAD"])
        .output()
        .context("Failed to list tracked files at HEAD")?;

    if !list.status.success() {
        let stderr = String::from_utf8_lossy(&list.stderr);
        bail!(
            "Failed to list tracked files: {}\n  Try: git ls-tree -r --name-only HEAD",
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&list.stdout);
    let mut removed = 0usize;

    for rel in stdout.lines().map(str::trim).filter(|s| !s.is_empty()) {
        if rel.starts_with("ws/") || rel.starts_with(".manifold/") || rel == ".gitignore" {
            continue;
        }

        let path = Path::new(rel);
        if path.is_file() && fs::remove_file(path).is_ok() {
            removed += 1;
        }
    }

    println!("[OK] Cleaned {removed} tracked file(s) from repo root");

    warn_remaining_untracked_root_files();

    Ok(())
}

/// Warn if untracked files are still present at repo root after cleanup.
///
/// In brownfield repos, partially-tracked directories can retain untracked
/// files (locks/state/cache) even after tracked files are moved to ws/default/.
/// We surface these explicitly so users don't miss manual cleanup.
fn warn_remaining_untracked_root_files() {
    let output = match Command::new("git")
        .args(["status", "--porcelain=1", "--untracked-files=all"])
        .output()
    {
        Ok(out) => out,
        Err(_) => return,
    };

    if !output.status.success() {
        return;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut leftover: Vec<String> = stdout
        .lines()
        .filter_map(|line| line.strip_prefix("?? "))
        .map(str::trim)
        .filter(|path| !is_ignored_untracked_root_path(path))
        .map(ToString::to_string)
        .collect();

    if leftover.is_empty() {
        return;
    }

    leftover.sort();
    leftover.dedup();

    let preview = leftover
        .iter()
        .take(5)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let more = if leftover.len() > 5 {
        format!(" (+{} more)", leftover.len() - 5)
    } else {
        String::new()
    };

    println!(
        "[WARN] {} untracked root file(s)/dir(s) remained after init: {}{}",
        leftover.len(),
        preview,
        more
    );
    println!(
        "  To fix: move these into ws/default/ (or remove them), then re-run: maw init"
    );
}

fn is_ignored_untracked_root_path(path: &str) -> bool {
    path == "ws"
        || path.starts_with("ws/")
        || path == ".git"
        || path.starts_with(".git/")
        || path == ".jj"
        || path.starts_with(".jj/")
        || path == ".manifold"
        || path.starts_with(".manifold/")
        || path == ".agents"
        || path.starts_with(".agents/")
        || path == ".claude"
        || path.starts_with(".claude/")
        || path == ".botbus"
        || path.starts_with(".botbus/")
        || path == ".crit"
        || path.starts_with(".crit/")
        || path == "AGENTS.md"
        || path == "CLAUDE.md"
}
