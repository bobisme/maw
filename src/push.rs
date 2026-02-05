use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::workspace::{repo_root, MawConfig};

#[derive(Args)]
pub struct PushArgs {
    /// Move the branch bookmark to @- (parent of working copy) before pushing.
    ///
    /// Use this after committing work to advance the branch to your latest
    /// commit. Without this flag, maw push only pushes if the bookmark is
    /// already ahead of origin.
    #[arg(long)]
    advance: bool,
}

/// Push the configured branch to its remote.
///
/// Wraps `jj git push` with better UX: checks sync status, provides
/// clear error messages, and shows what was pushed.
pub fn run(args: &PushArgs) -> Result<()> {
    let root = repo_root()?;
    let config = MawConfig::load(&root)?;
    let branch = config.branch();

    // Check if branch bookmark exists and get commit info
    let _commit_info = resolve_branch(&root, branch)?;

    // If --advance, move bookmark to @- before checking status
    if args.advance {
        advance_bookmark(&root, branch)?;
    }

    // Check ahead/behind status — bail if behind, return early if up-to-date
    if !should_push(&root, branch)? {
        println!("{branch} is up to date with origin.");
        // Check if there's unpushed work at @- that could be pushed with --advance
        if !args.advance {
            suggest_advance(&root, branch);
        }
        return Ok(());
    }

    // Re-resolve after potential advance to get updated commit info
    let commit_info = resolve_branch(&root, branch)?;

    // Push
    println!("Pushing {branch} to origin...");
    let push_output = Command::new("jj")
        .args(["git", "push"])
        .current_dir(&root)
        .output()
        .context("Failed to run jj git push")?;

    if !push_output.status.success() {
        let stderr = String::from_utf8_lossy(&push_output.stderr);
        bail!(
            "Push failed: {}\n  \
             Check: jj log -r '{branch}' and jj git fetch",
            stderr.trim()
        );
    }

    let push_stdout = String::from_utf8_lossy(&push_output.stdout);
    if push_stdout.contains("Nothing changed") {
        println!("{branch} is up to date with origin.");
    } else {
        println!("  Pushed: {commit_info}");
    }

    Ok(())
}

/// Move the branch bookmark to @- (parent of working copy).
fn advance_bookmark(root: &Path, branch: &str) -> Result<()> {
    let output = Command::new("jj")
        .args(["bookmark", "set", branch, "-r", "@-"])
        .current_dir(root)
        .output()
        .context("Failed to run jj bookmark set")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Failed to advance {branch} to @-: {}\n  \
             Check: jj log --limit 5",
            stderr.trim()
        );
    }

    println!("Advanced {branch} bookmark to @- (parent of working copy).");
    Ok(())
}

/// Check if @- is ahead of the branch and print a suggestion if so.
fn suggest_advance(root: &Path, branch: &str) {
    // Count commits between branch and @- (exclusive of branch, inclusive of @-)
    if let Ok(ahead) = count_revset(root, &format!("{branch}..@-"))
        && ahead > 0
    {
        println!(
            "\n  Your working copy parent (@-) is {ahead} commit(s) ahead of {branch}.\n  \
             To advance {branch} and push: maw push --advance"
        );
    }
}

/// Verify the branch bookmark exists and return its commit info string.
fn resolve_branch(root: &Path, branch: &str) -> Result<String> {
    let output = Command::new("jj")
        .args([
            "log",
            "-r",
            branch,
            "--no-graph",
            "--color=never",
            "--no-pager",
            "-T",
            r#"commit_id.short() ++ " " ++ description.first_line()"#,
        ])
        .current_dir(root)
        .output()
        .context("Failed to run jj log")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("doesn't exist") || stderr.contains("not found") {
            bail!(
                "Bookmark '{branch}' does not exist.\n  \
                 Create it with: jj bookmark create {branch} -r @-\n  \
                 Or set the branch name in .maw.toml under [repo]"
            );
        }
        bail!("Failed to check {branch} bookmark: {}", stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_string())
}

/// Check sync status. Returns true if there's something to push, false if up-to-date.
/// Bails if the branch is behind origin (must fetch first).
fn should_push(root: &Path, branch: &str) -> Result<bool> {
    let origin_ref = format!("{branch}@origin");

    // Check if origin tracking ref exists
    let origin_check = Command::new("jj")
        .args([
            "log",
            "-r",
            &origin_ref,
            "--no-graph",
            "--color=never",
            "--no-pager",
            "-T",
            "commit_id.short()",
        ])
        .current_dir(root)
        .output();

    let Ok(output) = origin_check else {
        return Ok(true); // Can't check, proceed with push
    };
    if !output.status.success() {
        return Ok(true); // No remote tracking yet, proceed with push
    }

    // Check if behind
    if let Ok(behind) = count_revset(root, &format!("{branch}..{origin_ref}"))
        && behind > 0
    {
        bail!(
            "{branch} is behind origin by {behind} commit(s).\n  \
             Run: jj git fetch && maw ws sync --all"
        );
    }

    // Check if ahead
    if let Ok(ahead) = count_revset(root, &format!("{origin_ref}..{branch}"))
        && ahead == 0
    {
        return Ok(false);
    }

    Ok(true)
}

fn count_revset(root: &Path, revset: &str) -> Result<usize> {
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
        .current_dir(root)
        .output()
        .context("Failed to run jj log")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("jj log failed for {revset}: {}", stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count())
}
