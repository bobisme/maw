use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::workspace::{jj_cwd, repo_root, MawConfig};

#[derive(Args)]
pub struct ReleaseArgs {
    /// Version tag to create (e.g., v0.30.1)
    ///
    /// Must start with 'v' followed by a semver-like version.
    /// Creates both jj and git tags, then pushes to origin.
    pub tag: String,
}

/// Tag and push a release in one step.
///
/// Does everything after the version bump commit:
///   1. Advance branch bookmark to @- (parent of working copy)
///   2. Push branch to origin
///   3. Create jj tag + git tag pointing at the branch
///   4. Push git tag to origin
///
/// Assumes the version bump is already committed (via `jj commit` or
/// `jj describe`). The working copy (@) should be empty or contain
/// only post-release work.
pub fn run(args: &ReleaseArgs) -> Result<()> {
    let tag = &args.tag;

    // Validate tag format
    if !tag.starts_with('v') {
        bail!(
            "Tag must start with 'v' (e.g., v1.0.0)\n  \
             Got: {tag}"
        );
    }

    let root = repo_root()?;
    let cwd = jj_cwd()?;
    let config = MawConfig::load(&root)?;
    let branch = config.branch();

    // Step 1: Advance bookmark to @-
    println!("Advancing {branch} bookmark to @-...");
    let advance = Command::new("jj")
        .args(["bookmark", "set", branch, "-r", "@-"])
        .current_dir(&cwd)
        .output()
        .context("Failed to advance bookmark")?;

    if !advance.status.success() {
        let stderr = String::from_utf8_lossy(&advance.stderr);
        bail!(
            "Failed to advance {branch} to @-: {}\n  \
             Check: jj log --limit 5",
            stderr.trim()
        );
    }

    // Get the commit info for reporting
    let commit_info = get_commit_info(&cwd, branch)?;
    println!("  {branch} -> {commit_info}");

    // Step 2: Push branch to origin
    println!("Pushing {branch} to origin...");
    let push = Command::new("jj")
        .args(["git", "push", "--bookmark", branch])
        .current_dir(&cwd)
        .output()
        .context("Failed to push branch")?;

    if !push.status.success() {
        let stderr = String::from_utf8_lossy(&push.stderr);
        bail!(
            "Push failed: {}\n  \
             Check: jj log -r '{branch}' and jj git fetch",
            stderr.trim()
        );
    }

    let push_stdout = String::from_utf8_lossy(&push.stdout);
    if push_stdout.contains("Nothing changed") {
        println!("  {branch} was already up to date.");
    } else {
        println!("  Pushed.");
    }

    // Step 3: Create jj tag
    println!("Creating tag {tag}...");
    let jj_tag = Command::new("jj")
        .args(["tag", "set", tag, "-r", branch])
        .current_dir(&cwd)
        .output()
        .context("Failed to create jj tag")?;

    if !jj_tag.status.success() {
        let stderr = String::from_utf8_lossy(&jj_tag.stderr);
        bail!("Failed to create jj tag: {}", stderr.trim());
    }

    // Step 4: Create git tag (jj tags don't reliably export to git)
    let git_tag = Command::new("git")
        .args(["tag", tag, branch])
        .current_dir(&root)
        .output()
        .context("Failed to create git tag")?;

    if !git_tag.status.success() {
        let stderr = String::from_utf8_lossy(&git_tag.stderr);
        if stderr.contains("already exists") {
            println!("  Git tag {tag} already exists.");
        } else {
            bail!("Failed to create git tag: {}", stderr.trim());
        }
    }

    // Step 5: Push git tag to origin
    println!("Pushing tag {tag} to origin...");
    let push_tag = Command::new("git")
        .args(["push", "origin", tag])
        .current_dir(&root)
        .output()
        .context("Failed to push git tag")?;

    if !push_tag.status.success() {
        let stderr = String::from_utf8_lossy(&push_tag.stderr);
        bail!(
            "Failed to push tag: {}\n  \
             Try: git push origin {tag}",
            stderr.trim()
        );
    }

    println!();
    println!("Released {tag}!");
    println!("  Branch: {branch} -> {commit_info}");
    println!("  Tag:    {tag} pushed to origin");

    Ok(())
}

fn get_commit_info(cwd: &std::path::Path, branch: &str) -> Result<String> {
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
        .context("Failed to get commit info")?;

    Ok(String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_string())
}
