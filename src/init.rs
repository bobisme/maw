use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// Files/dirs at root that should NOT be cleaned after forgetting the default workspace.
const KEEP_ROOT: &[&str] = &[
    ".git",
    ".jj",
    "ws",
    ".gitignore",
    ".maw.toml",
    ".beads",
    ".crit",
    ".agents",
    ".botbox.json",
    "notes",
];

/// Initialize maw in the current repository (bare repo model)
///
/// Ensures jj is initialized, ws/ is gitignored, and the repo is set up
/// in bare mode with a coord workspace.
pub fn run() -> Result<()> {
    println!("Initializing maw...");
    println!();

    ensure_jj_repo()?;
    ensure_workspaces_gitignored()?;
    ensure_maw_config()?;
    forget_default_workspace()?;
    set_git_bare_mode()?;
    create_coord_workspace()?;
    clean_root_source_files()?;

    let cwd = std::env::current_dir()
        .context("Could not determine current directory")?;
    let coord_path = cwd.join("ws").join("coord");

    println!();
    println!("maw is ready! (bare repo model)");
    println!("  Coord workspace: {}/", coord_path.display());
    println!("  Next: maw ws create <agent-name>");

    Ok(())
}

fn ensure_jj_repo() -> Result<()> {
    let check = Command::new("jj").args(["status"]).output().context(
        "jj not found — install from https://martinvonz.github.io/jj/latest/install-and-setup/",
    )?;

    if check.status.success() {
        println!("[OK] jj repository already initialized");
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&check.stderr);
    if stderr.contains("no jj repo") || stderr.contains("There is no jj repo") {
        println!("[..] Initializing jj repository...");

        // Check if this is a git repo — if so, colocate
        let is_git = Path::new(".git").exists();
        let args = if is_git {
            vec!["git", "init", "--colocate"]
        } else {
            vec!["git", "init"]
        };

        let init_output = Command::new("jj")
            .args(&args)
            .output()
            .context("Failed to run jj git init")?;

        if init_output.status.success() {
            if is_git {
                println!("[OK] jj initialized (colocated with existing git repo)");
            } else {
                println!("[OK] jj initialized (new git-backed repo)");
            }
        } else {
            let init_stderr = String::from_utf8_lossy(&init_output.stderr);
            anyhow::bail!(
                "jj git init failed: {}\n  Check jj is installed: jj --version",
                init_stderr.trim()
            );
        }
    } else {
        println!("[WARN] jj status returned error: {}", stderr.trim());
    }

    Ok(())
}

/// Check if ws/ is in .gitignore, add it if not
pub fn ensure_workspaces_gitignored() -> Result<()> {
    let gitignore_path = Path::new(".gitignore");

    if gitignore_path.exists() {
        let content = fs::read_to_string(gitignore_path).context("Failed to read .gitignore")?;

        // Check for common patterns that would cover ws/
        let dominated = content.lines().any(|line| {
            let line = line.trim();
            line == "ws"
                || line == "ws/"
                || line == "/ws"
                || line == "/ws/"
        });

        if dominated {
            println!("[OK] ws/ is in .gitignore");
            return Ok(());
        }

        // Append it
        let separator = if content.ends_with('\n') { "" } else { "\n" };
        let new_content = format!("{content}{separator}\n# maw agent workspaces\nws/\n");
        fs::write(gitignore_path, new_content).context("Failed to update .gitignore")?;
        println!("[OK] Added ws/ to .gitignore");
    } else {
        // Create .gitignore
        fs::write(gitignore_path, "# maw agent workspaces\nws/\n")
            .context("Failed to create .gitignore")?;
        println!("[OK] Created .gitignore with ws/");
    }

    Ok(())
}

/// Create default .maw.toml if it doesn't exist
fn ensure_maw_config() -> Result<()> {
    let config_path = Path::new(".maw.toml");

    if config_path.exists() {
        println!("[OK] .maw.toml already exists");
        return Ok(());
    }

    let default_config = r#"[repo]
# Branch name used for merge target, push, and sync status
# branch = "main"

[merge]
# Auto-resolve conflicts in these paths by taking main's version
# Useful for tracking files that change frequently on main
auto_resolve_from_main = [
  ".beads/**",
]
"#;

    fs::write(config_path, default_config).context("Failed to create .maw.toml")?;
    println!("[OK] Created .maw.toml with default config");

    Ok(())
}

/// Forget the default workspace if it exists.
/// In bare repo model, we don't want a default workspace — only coord + agent workspaces.
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
        .context("Failed to run jj workspace forget default")?;

    if forget.status.success() {
        println!("[OK] Forgot default workspace");
    } else {
        let stderr = String::from_utf8_lossy(&forget.stderr);
        anyhow::bail!(
            "Failed to forget default workspace: {}\n  Try: jj workspace forget default",
            stderr.trim()
        );
    }

    Ok(())
}

/// Set git core.bare = true so git treats this as a bare repo.
fn set_git_bare_mode() -> Result<()> {
    // Check current value first
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
        .context("Failed to run git config core.bare true")?;

    if output.status.success() {
        println!("[OK] Set git core.bare = true");
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Failed to set git core.bare: {}\n  Try: git config core.bare true",
            stderr.trim()
        );
    }

    Ok(())
}

/// Create the coord workspace in ws/coord if it doesn't already exist.
fn create_coord_workspace() -> Result<()> {
    let ws_dir = Path::new("ws");
    let coord_path = ws_dir.join("coord");

    // Check if coord workspace already exists in jj
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

    // Ensure ws/ directory exists
    fs::create_dir_all(ws_dir)
        .with_context(|| format!("Failed to create {}", ws_dir.display()))?;

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
            .context("Failed to run jj workspace add for coord")?;

        if !add.status.success() {
            let stderr = String::from_utf8_lossy(&add.stderr);
            anyhow::bail!(
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

/// Clean root source files after forgetting the default workspace.
/// Only removes files/dirs that are tracked by git at HEAD and not in the keep list.
#[allow(clippy::unnecessary_wraps)]
pub fn clean_root_source_files() -> Result<()> {
    let output = Command::new("git")
        .args(["ls-tree", "--name-only", "HEAD"])
        .output();

    let Ok(out) = output else {
        // git ls-tree may fail if no commits yet — that's fine
        println!("[OK] No tracked files to clean (no HEAD)");
        return Ok(());
    };

    if !out.status.success() {
        // No HEAD yet (fresh repo) — nothing to clean
        println!("[OK] No tracked files to clean (no HEAD)");
        return Ok(());
    }

    let tracked = String::from_utf8_lossy(&out.stdout);
    let mut cleaned = 0;

    for entry in tracked.lines() {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }

        // Skip entries in the keep list
        if KEEP_ROOT.contains(&entry) {
            continue;
        }

        let path = Path::new(entry);
        if !path.exists() {
            continue;
        }

        if path.is_dir() {
            if fs::remove_dir_all(path).is_ok() {
                cleaned += 1;
            }
        } else if fs::remove_file(path).is_ok() {
            cleaned += 1;
        }
    }

    if cleaned > 0 {
        println!("[OK] Cleaned {cleaned} root file(s)/dir(s)");
    } else {
        println!("[OK] Root already clean");
    }

    Ok(())
}
