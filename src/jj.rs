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

/// Run a jj command with automatic recovery from "sibling operation" errors.
///
/// When concurrent workspaces run jj commands simultaneously, they can create
/// forked operation graphs. jj reports this as "sibling of the working copy's
/// operation" and suggests `jj op integrate <id>`. This helper:
///   1. Runs the command
///   2. If it fails with a sibling-op error, extracts the op ID from the hint
///   3. Runs `jj op integrate <id>` to heal the fork
///   4. Retries the original command once
///   5. Returns a clear error if recovery fails
pub fn run_jj_with_op_recovery(args: &[&str], cwd: &Path) -> Result<Output> {
    let output = Command::new("jj")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("Failed to run jj {}", args.join(" ")))?;

    if output.status.success() {
        return Ok(output);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.contains("sibling of the working copy") {
        // Not a sibling-op error — return as-is for caller to handle
        return Ok(output);
    }

    // Extract operation ID from hint line: "Run `jj op integrate <id>` ..."
    let op_id = extract_op_integrate_id(&stderr);

    let Some(op_id) = op_id else {
        bail!(
            "Concurrent workspace operations caused an operation graph fork.\n\
             jj could not determine the operation ID to integrate.\n\
             stderr: {stderr}"
        );
    };

    // Attempt auto-fix
    eprintln!(
        "maw: concurrent operation fork detected, running: jj op integrate {op_id}"
    );

    let integrate = Command::new("jj")
        .args(["op", "integrate", &op_id])
        .current_dir(cwd)
        .output()
        .context("Failed to run jj op integrate")?;

    if !integrate.status.success() {
        let integrate_err = String::from_utf8_lossy(&integrate.stderr);
        bail!(
            "Concurrent workspace operations caused an operation graph fork.\n\
             Auto-recovery failed: {integrate_err}\n\
             Try again shortly, or run manually: jj op integrate {op_id}"
        );
    }

    // Retry the original command
    let retry = Command::new("jj")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("Failed to run jj {} (retry)", args.join(" ")))?;

    if !retry.status.success() {
        let retry_err = String::from_utf8_lossy(&retry.stderr);
        bail!(
            "Concurrent workspace operations caused an operation graph fork.\n\
             jj op integrate succeeded but the retry failed: {retry_err}\n\
             Try again shortly, or run manually: jj op integrate {op_id}"
        );
    }

    Ok(retry)
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
