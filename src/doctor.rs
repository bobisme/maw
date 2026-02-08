use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use crate::workspace;

/// Check system requirements and configuration
#[allow(clippy::unnecessary_wraps)]
pub fn run() -> Result<()> {
    println!("maw doctor");
    println!("==========");
    println!();

    let mut all_ok = true;

    // Check jj (required)
    all_ok &= check_tool(
        "jj",
        &["--version"],
        "https://martinvonz.github.io/jj/latest/install-and-setup/",
    );

    // Find repo root (best-effort — may fail if not in a repo)
    let root = workspace::repo_root().ok();

    // Check jj repo — run from ws/default if it exists (bare root is always stale)
    all_ok &= check_jj_repo(root.as_deref());

    // Check ws/default/ exists and looks correct
    all_ok &= check_default_workspace(root.as_deref());

    println!();
    if all_ok {
        println!("All checks passed!");
    } else {
        println!("Some checks failed. See above for details.");
    }

    Ok(())
}

fn check_tool(name: &str, args: &[&str], install_url: &str) -> bool {
    match Command::new(name).args(args).output() {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout);
            let version = version.lines().next().unwrap_or("unknown").trim();
            println!("[OK] {name}: {version}");
            true
        }
        Ok(_) => {
            println!("[FAIL] {name}: found but returned error");
            println!("       Install: {install_url}");
            false
        }
        Err(_) => {
            println!("[FAIL] {name}: not found");
            println!("       Install: {install_url}");
            false
        }
    }
}

fn default_ws_path(root: Option<&Path>) -> Option<PathBuf> {
    let path = root?.join("ws").join("default");
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

/// Check if we're in a jj repo. Runs from ws/default/ if it exists to avoid
/// the stale working copy error that always occurs at the bare root.
fn check_jj_repo(root: Option<&Path>) -> bool {
    let cwd = default_ws_path(root).unwrap_or_else(|| PathBuf::from("."));

    let Ok(output) = Command::new("jj").args(["status"]).current_dir(&cwd).output() else {
        // jj not installed — already reported by check_tool
        return true;
    };

    if output.status.success() {
        println!("[OK] jj repository: found");
        true
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("no jj repo") || stderr.contains("There is no jj repo") {
            println!("[FAIL] jj repository: not in a jj repo");
            println!("       Run: maw init");
            false
        } else {
            println!(
                "[WARN] jj repository: {}",
                stderr.lines().next().unwrap_or("unknown error")
            );
            true
        }
    }
}

/// Check that ws/default/ exists and contains the expected repo structure.
fn check_default_workspace(root: Option<&Path>) -> bool {
    let Some(root) = root else {
        println!("[WARN] default workspace: could not determine repo root");
        return true;
    };

    let default_ws = root.join("ws").join("default");

    if !default_ws.exists() {
        println!("[FAIL] default workspace: ws/default/ does not exist");
        println!("       Run: maw init");
        return false;
    }

    // Check that it has a .gitignore with ws/ entry
    let gitignore = default_ws.join(".gitignore");
    if gitignore.exists() {
        if let Ok(content) = std::fs::read_to_string(&gitignore) {
            let has_ws = content
                .lines()
                .any(|l| matches!(l.trim(), "ws" | "ws/" | "/ws" | "/ws/"));
            if !has_ws {
                println!("[WARN] default workspace: .gitignore missing ws/ entry");
                println!("       Run: maw init");
            }
        }
    }

    // Check that source files exist (not an empty workspace)
    let has_files = std::fs::read_dir(&default_ws)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| !e.file_name().to_string_lossy().starts_with('.'))
        })
        .unwrap_or(false);

    if has_files {
        println!("[OK] default workspace: ws/default/ exists with source files");
    } else {
        println!("[WARN] default workspace: ws/default/ exists but appears empty");
        println!("       Run: maw init");
    }

    true
}
