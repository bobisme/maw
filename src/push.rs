use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::jj::count_revset;
use crate::workspace::{jj_cwd, repo_root, MawConfig};

#[derive(Args)]
pub struct PushArgs {
    /// Move the branch bookmark to @- (parent of working copy) before pushing.
    ///
    /// Use this after committing work to advance the branch to your latest
    /// commit. Without this flag, maw push only pushes if the bookmark is
    /// already ahead of origin.
    #[arg(long)]
    advance: bool,

    /// Skip pushing git tags.
    ///
    /// By default, maw push also pushes any unpushed git tags to origin.
    /// Use this flag to push only the branch bookmark.
    #[arg(long)]
    no_tags: bool,
}

/// Push the configured branch to its remote.
///
/// Wraps `jj git push --bookmark <branch>` with better UX: checks sync
/// status, provides clear error messages, and shows what was pushed.
///
/// We pass `--bookmark` explicitly because in the bare-repo model the
/// default workspace is at ws/default/, not root. Without `--bookmark`,
/// jj's default push revset (`remote_bookmarks(remote=origin)..@`) won't
/// find the main bookmark since it isn't an ancestor of default's `@`.
pub fn run(args: &PushArgs) -> Result<()> {
    let root = repo_root()?;
    let cwd = jj_cwd()?;
    let config = MawConfig::load(&root)?;
    let branch = config.branch();

    // Check if branch bookmark exists and get commit info
    let _commit_info = resolve_branch(&cwd, branch)?;

    // If --advance, move bookmark to @- before checking status
    if args.advance {
        advance_bookmark(&cwd, branch)?;
    }

    // Check ahead/behind status — bail if behind, skip bookmark push if up-to-date
    if should_push(&cwd, branch)? {
        // Re-resolve after potential advance to get updated commit info
        let commit_info = resolve_branch(&cwd, branch)?;

        // Export jj refs to git before pushing — prevents false "Nothing changed"
        // when the operation graph has diverged and git refs are stale.
        let export = Command::new("jj")
            .args(["git", "export"])
            .current_dir(&cwd)
            .output()
            .context("Failed to run jj git export")?;

        if !export.status.success() {
            let stderr = String::from_utf8_lossy(&export.stderr);
            eprintln!("Warning: jj git export failed: {}", stderr.trim());
        }

        // Push bookmark
        println!("Pushing {branch} to origin...");
        let push_output = Command::new("jj")
            .args(["git", "push", "--bookmark", branch])
            .current_dir(&cwd)
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
    } else {
        println!("{branch} is up to date with origin.");
        // Check if there's unpushed work at @- that could be pushed with --advance
        if !args.advance {
            suggest_advance(&cwd, branch);
        }
    }

    // Push git tags regardless of branch status (unless --no-tags)
    if !args.no_tags {
        push_tags(&cwd, &root)?;
    }

    Ok(())
}

/// Move the branch bookmark to @- (parent of working copy).
fn advance_bookmark(cwd: &Path, branch: &str) -> Result<()> {
    let output = Command::new("jj")
        .args(["bookmark", "set", branch, "-r", "@-"])
        .current_dir(cwd)
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
fn suggest_advance(cwd: &Path, branch: &str) {
    // Count commits between branch and @- (exclusive of branch, inclusive of @-)
    if let Ok(ahead) = count_revset(cwd, &format!("{branch}..@-"))
        && ahead > 0
    {
        println!(
            "\n  Your working copy parent (@-) is {ahead} commit(s) ahead of {branch}.\n  \
             To advance {branch} and push: maw push --advance"
        );
    }
}

/// Verify the branch bookmark exists and return its commit info string.
fn resolve_branch(cwd: &Path, branch: &str) -> Result<String> {
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
        .current_dir(cwd)
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
fn should_push(cwd: &Path, branch: &str) -> Result<bool> {
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
        .current_dir(cwd)
        .output();

    let Ok(output) = origin_check else {
        return Ok(true); // Can't check, proceed with push
    };
    if !output.status.success() {
        return Ok(true); // No remote tracking yet, proceed with push
    }

    // Check if behind
    if let Ok(behind) = count_revset(cwd, &format!("{branch}..{origin_ref}"))
        && behind > 0
    {
        bail!(
            "{branch} is behind origin by {behind} commit(s).\n  \
             Run: jj git fetch && maw ws sync --all"
        );
    }

    // Check if ahead
    if let Ok(ahead) = count_revset(cwd, &format!("{origin_ref}..{branch}"))
        && ahead == 0
    {
        return Ok(false);
    }

    Ok(true)
}

/// Export jj tags to git, then push all tags to origin.
/// Reports which tags were pushed; warns on failures but does not fail the overall push.
fn push_tags(cwd: &Path, root: &Path) -> Result<()> {
    // Export jj state (including tags) to the colocated git repo.
    let export = Command::new("jj")
        .args(["git", "export"])
        .current_dir(cwd)
        .output()
        .context("Failed to run jj git export")?;

    if !export.status.success() {
        let stderr = String::from_utf8_lossy(&export.stderr);
        eprintln!("Warning: jj git export failed: {}", stderr.trim());
        // Non-fatal — tags may already be exported
    }

    // Verify at least some tag refs exist in git before pushing.
    // If jj git export didn't create any tag refs, git push --tags
    // would succeed but push nothing — which can be confusing.
    let tag_check = Command::new("git")
        .args(["tag", "--list"])
        .current_dir(root)
        .output();

    let has_git_tags = tag_check
        .as_ref()
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false);

    if !has_git_tags {
        eprintln!("Warning: No git tags found after jj git export.");
        eprintln!("  Tags created with `jj tag set` may not have exported to git.");
        eprintln!("  For reliable tag+push, use: maw release <tag>");
        return Ok(());
    }

    let output = Command::new("git")
        .args(["push", "origin", "--tags", "--porcelain"])
        .current_dir(root)
        .output()
        .context("Failed to run git push --tags")?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!(
            "Warning: git push --tags failed (tags not pushed): {}",
            stderr.trim()
        );
        return Ok(());
    }

    // Parse porcelain output for newly pushed and rejected tags.
    // Format: "<flag>\t<from>:<to>\t<summary>"
    //   * = new ref pushed, = = up to date, ! = rejected
    let mut pushed = Vec::new();
    let mut rejected = Vec::new();

    for line in stdout.lines() {
        let tag_name = line
            .split("refs/tags/")
            .nth(1)
            .and_then(|s| s.split(['\t', ' ', ':']).next());

        if line.starts_with('*') {
            if let Some(name) = tag_name {
                pushed.push(name.to_string());
            }
        } else if line.starts_with('!') {
            if let Some(name) = tag_name {
                rejected.push(name.to_string());
            }
        }
    }

    if !pushed.is_empty() {
        println!("  Tags pushed: {}", pushed.join(", "));
    }
    if !rejected.is_empty() {
        eprintln!(
            "Warning: {} tag(s) rejected by remote: {}",
            rejected.len(),
            rejected.join(", ")
        );
    }

    Ok(())
}
