//! Shared jj helpers used by multiple modules.

use std::path::Path;
use std::process::{Command, Output};

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

/// Run a jj command, returning the Output for the caller to inspect.
///
/// Thin wrapper around `Command::new("jj")` with consistent error context.
/// Does NOT auto-recover from errors — callers should check
/// `is_sibling_op_error()` on stderr and degrade gracefully.
pub fn run_jj(args: &[&str], cwd: &Path) -> Result<Output> {
    Command::new("jj")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("Failed to run jj {}", args.join(" ")))
}

/// Check if jj stderr indicates a "sibling operation" error caused by
/// concurrent workspace operations forking the operation graph.
pub fn is_sibling_op_error(stderr: &str) -> bool {
    stderr.contains("sibling of the working copy")
}

/// Build a human-readable fix command for a sibling-op error.
/// Returns `None` if the op ID can't be extracted from stderr.
pub fn sibling_op_fix_command(stderr: &str) -> Option<String> {
    extract_op_integrate_id(stderr).map(|id| format!("jj op integrate {id}"))
}

/// Extract operation ID from jj's hint about `jj op integrate <id>`.
///
/// jj emits a hint like:
///   Hint: Run `jj op integrate 4a8f...` to combine the operations.
fn extract_op_integrate_id(stderr: &str) -> Option<String> {
    for line in stderr.lines() {
        // Look for the pattern: jj op integrate <id>
        if let Some(pos) = line.find("jj op integrate ") {
            let after = &line[pos + "jj op integrate ".len()..];
            // The ID ends at the next backtick, quote, or whitespace
            let id: String = after
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '`' && *c != '\'' && *c != '"')
                .collect();
            if !id.is_empty() {
                return Some(id);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_op_id_from_backtick_hint() {
        let stderr = "\
Error: The working copy is a sibling of the working copy's operation
Hint: Run `jj op integrate 4a8f3bc2e1d0` to combine the operations.";
        assert_eq!(
            extract_op_integrate_id(stderr),
            Some("4a8f3bc2e1d0".to_string())
        );
    }

    #[test]
    fn extract_op_id_without_backticks() {
        let stderr = "Hint: Run jj op integrate abc123def456 to fix this.";
        assert_eq!(
            extract_op_integrate_id(stderr),
            Some("abc123def456".to_string())
        );
    }

    #[test]
    fn extract_op_id_with_long_hex() {
        let stderr = "\
Error: some error
Hint: Run `jj op integrate 4a8f3bc2e1d0abcdef1234567890abcdef1234567890abcdef1234567890abcd` to combine the operations.";
        assert_eq!(
            extract_op_integrate_id(stderr),
            Some("4a8f3bc2e1d0abcdef1234567890abcdef1234567890abcdef1234567890abcd".to_string())
        );
    }

    #[test]
    fn returns_none_when_no_hint() {
        let stderr = "Error: something else went wrong\nHint: try something different";
        assert_eq!(extract_op_integrate_id(stderr), None);
    }

    #[test]
    fn returns_none_for_empty_stderr() {
        assert_eq!(extract_op_integrate_id(""), None);
    }

    #[test]
    fn no_false_positive_on_unrelated_sibling_text() {
        // Should not match without the "jj op integrate" hint
        let stderr = "Error: something about sibling of the working copy\nHint: do something else";
        assert_eq!(extract_op_integrate_id(stderr), None);
    }
}
