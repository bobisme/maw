use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// Non-dotfile entries at root that should NOT be cleaned.
/// Dotfiles/dotdirs (.git, .jj, .claude, .pi, etc.) are always kept.
/// AGENTS.md and CLAUDE.md are redirect stubs pointing into ws/default/.
const KEEP_ROOT: &[&str] = &["ws", "AGENTS.md", "CLAUDE.md"];

/// Initialize maw in the current repository (bare repo model)
///
/// Ensures jj is initialized, ws/ is gitignored, and the repo is set up
/// in bare mode with a default workspace at ws/default/.
pub fn run() -> Result<()> {
    println!("Initializing maw...");
    println!();

    ensure_jj_repo()?;
    ensure_workspaces_gitignored()?;
    ensure_maw_config()?;
    setup_bare_default_workspace()?;
    set_git_bare_mode()?;
    fix_git_head()?;
    clean_root_source_files()?;
    ensure_gitignore_in_workspace()?;
    refresh_workspace_state()?;

    let cwd = std::env::current_dir()
        .context("Could not determine current directory")?;
    let default_path = cwd.join("ws").join("default");

    println!();
    println!("maw is ready! (bare repo model)");
    println!("  Default workspace: {}/", default_path.display());
    println!("  Next: maw ws create <agent-name>");

    Ok(())
}

fn ensure_jj_repo() -> Result<()> {
    // Run from ws/default/ if it exists (root may lack .jj/working_copy/)
    let jj_cwd = if Path::new("ws/default").exists() {
        Path::new("ws/default")
    } else {
        Path::new(".")
    };
    let check = Command::new("jj").args(["status"]).current_dir(jj_cwd).output().context(
        "jj not found — install from https://martinvonz.github.io/jj/latest/install-and-setup/",
    )?;

    if check.status.success() {
        println!("[OK] jj repository already initialized");
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&check.stderr);
    if stderr.contains("no jj repo") || stderr.contains("There is no jj repo") {
        println!("[..] Initializing jj repository...");

        // Always colocate — maw requires both .jj/ and .git/
        let had_git = Path::new(".git").exists();
        let args = vec!["git", "init", "--colocate"];

        let init_output = Command::new("jj")
            .args(&args)
            .output()
            .context("Failed to run jj git init")?;

        if init_output.status.success() {
            if had_git {
                println!("[OK] jj initialized (colocated with existing git repo)");
            } else {
                println!("[OK] jj initialized (new colocated repo)");
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

/// Move the default workspace from root to ws/default/.
///
/// In a fresh jj repo, "default" is at the repo root. We relocate it to
/// ws/default/ so the root becomes bare (no working copy). If "default"
/// already exists at ws/default/, this is a no-op.
fn setup_bare_default_workspace() -> Result<()> {
    let ws_dir = Path::new("ws");
    let default_path = ws_dir.join("default");

    // Check current workspace state. Run from ws/default/ if it exists
    // (root may lack .jj/working_copy/ after ghost cleanup).
    let jj_cwd = if default_path.exists() {
        default_path.as_path()
    } else {
        Path::new(".")
    };
    let output = Command::new("jj")
        .args(["workspace", "list"])
        .current_dir(jj_cwd)
        .output()
        .context("Failed to run jj workspace list")?;

    let ws_list = String::from_utf8_lossy(&output.stdout);

    let has_default = ws_list.lines().any(|line| {
        line.split(':')
            .next()
            .is_some_and(|n| n.trim().trim_end_matches('@') == "default")
    });

    // If default workspace exists at ws/default/, we're already set up
    if has_default && default_path.exists() {
        println!("[OK] Default workspace already at ws/default/");
        return Ok(());
    }

    // Ensure ws/ directory exists
    fs::create_dir_all(ws_dir)
        .with_context(|| format!("Failed to create {}", ws_dir.display()))?;

    // If default exists at root, forget it so we can recreate at ws/default/
    if has_default {
        let forget = Command::new("jj")
            .args(["workspace", "forget", "default"])
            .current_dir(jj_cwd)
            .output()
            .context("Failed to forget default workspace")?;

        if !forget.status.success() {
            let stderr = String::from_utf8_lossy(&forget.stderr);
            anyhow::bail!(
                "Failed to forget default workspace: {}\n  Try: jj workspace forget default",
                stderr.trim()
            );
        }

        // NOTE: Do NOT remove .jj/working_copy/ yet — jj needs it to run
        // workspace add. We'll clean it up after the new workspace is created.
    }

    // Create default workspace at ws/default/, parented on main.
    // Try with -r main first; fall back to no -r for fresh repos without main.
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
        // main might not exist yet — retry without -r
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
            anyhow::bail!(
                "Failed to create default workspace: {}\n  Try: jj workspace add ws/default --name default",
                stderr.trim()
            );
        }
    }

    // Now that the new workspace exists, clean up ghost working copy metadata
    // at root. `jj workspace forget` leaves .jj/working_copy/ on disk — if any
    // jj command later runs from root, jj sees the stale metadata and
    // materializes files into root, polluting the bare repo.
    let ghost_wc = Path::new(".jj").join("working_copy");
    if ghost_wc.exists() {
        fs::remove_dir_all(&ghost_wc).ok();
    }

    println!("[OK] Created default workspace at ws/default/");

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

/// Point git HEAD at the main branch ref.
///
/// After `jj workspace forget` + `core.bare=true`, HEAD gets stuck at whatever
/// commit was checked out before the upgrade. jj only exports to branch refs,
/// not HEAD, so it stays detached forever. This breaks `git log`, GitHub Desktop,
/// and any tool that relies on HEAD.
pub fn fix_git_head() -> Result<()> {
    // Read branch from .maw.toml if available, fall back to "main"
    let root = crate::workspace::repo_root().unwrap_or_else(|_| ".".into());
    let branch = crate::workspace::MawConfig::load(&root)
        .map(|c| c.branch().to_string())
        .unwrap_or_else(|_| "main".to_string());

    let ref_name = format!("refs/heads/{branch}");

    // Check current HEAD
    let current = Command::new("git")
        .args(["symbolic-ref", "HEAD"])
        .output();

    if let Ok(out) = &current {
        let val = String::from_utf8_lossy(&out.stdout);
        if val.trim() == ref_name {
            println!("[OK] git HEAD already points to {branch}");
            return Ok(());
        }
    }

    let output = Command::new("git")
        .args(["symbolic-ref", "HEAD", &ref_name])
        .output()
        .context("Failed to run git symbolic-ref HEAD")?;

    if output.status.success() {
        println!("[OK] Set git HEAD -> {ref_name}");
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Non-fatal — HEAD being wrong is annoying but not blocking
        println!("[WARN] Could not set git HEAD: {}", stderr.trim());
    }

    Ok(())
}

/// Clean root directory so only `KEEP_ROOT` items remain.
///
/// Scans the root directory and removes any files/dirs not in the keep list.
/// Previews what will be deleted before removing anything. Files tracked by
/// jj/git are recoverable and removed silently; untracked files get a warning.
#[allow(clippy::unnecessary_wraps)]
pub fn clean_root_source_files() -> Result<()> {
    let entries = if let Ok(e) = fs::read_dir(".") { e } else {
        println!("[OK] Could not read root directory");
        return Ok(());
    };

    // Collect files/dirs that would be removed.
    let mut to_remove: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str.starts_with('.') || KEEP_ROOT.contains(&name_str.as_ref()) {
            continue;
        }
        to_remove.push(name_str.into_owned());
    }

    // Nothing to clean (besides ghost working_copy, handled below).
    if to_remove.is_empty() {
        clean_ghost_working_copy();
        println!("[OK] Root already clean");
        return Ok(());
    }

    // Check which files are untracked (not recoverable from jj/git).
    let untracked: Vec<&str> = to_remove
        .iter()
        .filter(|name| !is_jj_tracked(name))
        .map(String::as_str)
        .collect();

    if !untracked.is_empty() {
        eprintln!("[WARN] Root cleanup would delete files NOT tracked by jj/git (unrecoverable):");
        for name in &untracked {
            eprintln!("  - {name}");
        }
        eprintln!("  These files are not in any jj/git commit and cannot be restored.");
        eprintln!("  To keep them, move them into ws/default/ before running init.");
        eprintln!("  To force cleanup, delete them manually and re-run maw init.");
        // Still remove the tracked files — those are recoverable.
        // Skip the untracked ones.
    }

    let mut cleaned = 0;
    for name in &to_remove {
        if !untracked.contains(&name.as_str()) {
            let path = Path::new(name);
            if path.is_dir() {
                if fs::remove_dir_all(path).is_ok() {
                    cleaned += 1;
                }
            } else if fs::remove_file(path).is_ok() {
                cleaned += 1;
            }
        }
    }

    clean_ghost_working_copy();

    if cleaned > 0 {
        println!("[OK] Cleaned {cleaned} tracked root file(s)/dir(s)");
    }
    if !untracked.is_empty() {
        println!("[WARN] Skipped {} untracked file(s) — see warnings above", untracked.len());
    }

    Ok(())
}

/// Remove ghost .jj/working_copy/ if present — left behind by `jj workspace forget`.
fn clean_ghost_working_copy() {
    let ghost_wc = Path::new(".jj").join("working_copy");
    if ghost_wc.exists() {
        if fs::remove_dir_all(&ghost_wc).is_ok() {
            println!("[OK] Removed ghost .jj/working_copy/ (prevents root pollution)");
        }
    }
}

/// Check whether a file at root is tracked by jj (present in the main branch).
///
/// Must run jj from ws/default/ because the bare root has no working copy.
fn is_jj_tracked(name: &str) -> bool {
    let ws_default = Path::new("ws").join("default");
    if !ws_default.exists() {
        return false;
    }
    Command::new("jj")
        .args(["file", "show", "-r", "main", name])
        .current_dir(&ws_default)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Ensure .gitignore with ws/ exists in the default workspace.
///
/// After `clean_root_source_files` removes .gitignore from root, it needs
/// to exist in ws/default/ so jj ignores workspace directories.
fn ensure_gitignore_in_workspace() -> Result<()> {
    let ws_gitignore = Path::new("ws").join("default").join(".gitignore");

    if ws_gitignore.exists() {
        let content = fs::read_to_string(&ws_gitignore).unwrap_or_default();
        let has_ws = content
            .lines()
            .any(|l| matches!(l.trim(), "ws" | "ws/" | "/ws" | "/ws/"));
        if has_ws {
            return Ok(());
        }
        // Append ws/ to existing .gitignore
        let separator = if content.ends_with('\n') { "" } else { "\n" };
        let new_content = format!("{content}{separator}\n# maw agent workspaces\nws/\n");
        fs::write(&ws_gitignore, new_content).context("Failed to update ws/default/.gitignore")?;
    } else {
        fs::write(&ws_gitignore, "# maw agent workspaces\nws/\n")
            .context("Failed to create ws/default/.gitignore")?;
    }
    println!("[OK] Ensured .gitignore in ws/default/");

    Ok(())
}

/// Refresh jj workspace state after init to prevent stale errors.
///
/// After moving the default workspace from root to ws/default/, jj may
/// have stale working copy state. Running update-stale from inside the
/// workspace fixes this.
fn refresh_workspace_state() -> Result<()> {
    let ws_path = Path::new("ws").join("default");
    if !ws_path.exists() {
        return Ok(());
    }

    let _ = Command::new("jj")
        .args(["workspace", "update-stale"])
        .current_dir(&ws_path)
        .output();

    Ok(())
}
