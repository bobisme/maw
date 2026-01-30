use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// Initialize MAW in the current repository
///
/// Ensures jj is initialized and .workspaces/ is gitignored.
pub fn run() -> Result<()> {
    println!("Initializing MAW...");
    println!();

    ensure_jj_repo()?;
    ensure_workspaces_gitignored()?;

    println!();
    println!("MAW is ready! Next steps:");
    println!("  maw ws create <agent-name>   # Create a workspace");
    println!("  maw agents init              # Add agent instructions to AGENTS.md");
    println!("  maw doctor                   # Verify full setup");

    Ok(())
}

fn ensure_jj_repo() -> Result<()> {
    let check = Command::new("jj")
        .args(["status"])
        .output()
        .context("jj not found — install from https://martinvonz.github.io/jj/latest/install-and-setup/")?;

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

/// Check if .workspaces/ is in .gitignore, add it if not
pub fn ensure_workspaces_gitignored() -> Result<()> {
    let gitignore_path = Path::new(".gitignore");

    if gitignore_path.exists() {
        let content = fs::read_to_string(gitignore_path).context("Failed to read .gitignore")?;

        // Check for common patterns that would cover .workspaces/
        let dominated = content.lines().any(|line| {
            let line = line.trim();
            line == ".workspaces"
                || line == ".workspaces/"
                || line == "/.workspaces"
                || line == "/.workspaces/"
        });

        if dominated {
            println!("[OK] .workspaces/ is in .gitignore");
            return Ok(());
        }

        // Append it
        let separator = if content.ends_with('\n') { "" } else { "\n" };
        let new_content = format!("{content}{separator}\n# MAW agent workspaces\n.workspaces/\n");
        fs::write(gitignore_path, new_content).context("Failed to update .gitignore")?;
        println!("[OK] Added .workspaces/ to .gitignore");
    } else {
        // Create .gitignore
        fs::write(gitignore_path, "# MAW agent workspaces\n.workspaces/\n")
            .context("Failed to create .gitignore")?;
        println!("[OK] Created .gitignore with .workspaces/");
    }

    Ok(())
}
