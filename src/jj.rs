//! Shared jj helpers used by multiple modules.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Count commits matching a revset expression.
pub fn count_revset(cwd: &Path, revset: &str) -> Result<usize> {
    let output = Command::new("jj")
        .args([
            "log",
            "-r",
            revset,
            "--no-graph",
            "--color=never",
            "--no-pager",
            "-T",
            "commit_id.short()",
        ])
        .current_dir(cwd)
        .output()
        .context("Failed to run jj log")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = format!("{stderr}{stdout}");
        bail!("jj log failed for {revset}: {}", message.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count())
}

/// Check whether a revset resolves to at least one commit.
///
/// Returns `false` (not an error) when jj reports the revset "doesn't exist"
/// or is "not found", which happens for missing bookmarks/refs.
pub fn revset_exists(cwd: &Path, revset: &str) -> Result<bool> {
    let output = Command::new("jj")
        .args([
            "log",
            "-r",
            revset,
            "--no-graph",
            "--color=never",
            "--no-pager",
            "-T",
            "change_id.short()",
        ])
        .current_dir(cwd)
        .output()
        .context("Failed to run jj log")?;

    if output.status.success() {
        return Ok(true);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let message = format!("{stderr}{stdout}");
    if message.contains("doesn't exist") || message.contains("not found") {
        return Ok(false);
    }

    bail!("jj log failed: {}", message.trim())
}
