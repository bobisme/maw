use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::workspace::{MawConfig, git_cwd, repo_root};

#[derive(Args)]
pub struct ReleaseArgs {
    /// Version tag to create (e.g., v0.30.1)
    ///
    /// Must start with 'v' followed by a semver-like version.
    /// Creates a git tag and pushes to origin.
    pub tag: String,
}

/// Tag and push a release in one step.
///
/// Does everything after the version bump commit:
///   1. Advance branch to current epoch (if needed)
///   2. Push branch to origin
///   3. Create git tag at the branch tip
///   4. Push git tag to origin
///
/// Assumes the version bump is already merged (via `maw ws merge`).
#[allow(clippy::too_many_lines)]
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
    let _cwd = git_cwd()?;
    let config = MawConfig::load(&root)?;
    let branch = config.branch();

    // Step 1: Ensure branch is aligned safely with the current epoch
    println!("Ensuring {branch} is at current epoch...");
    let epoch_output = Command::new("git")
        .args(["rev-parse", "refs/manifold/epoch/current"])
        .current_dir(&root)
        .output()
        .context("Failed to read current epoch")?;

    if !epoch_output.status.success() {
        bail!(
            "No current epoch found.\n  \
             Run `maw init` and `maw ws merge` first."
        );
    }

    let epoch_oid = String::from_utf8_lossy(&epoch_output.stdout)
        .trim()
        .to_string();

    // Read current branch position
    let branch_ref = format!("refs/heads/{branch}");
    let branch_output = Command::new("git")
        .args(["rev-parse", &branch_ref])
        .current_dir(&root)
        .output();

    let branch_oid = branch_output
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let release_oid = if branch_oid == epoch_oid {
        println!("  {branch} already at current epoch.");
        epoch_oid.clone()
    } else if branch_oid.is_empty() {
        println!("  Creating {branch} at epoch ({})...", &epoch_oid[..12]);
        let update = Command::new("git")
            .args(["update-ref", &branch_ref, &epoch_oid])
            .current_dir(&root)
            .output()
            .context("Failed to update branch ref")?;

        if !update.status.success() {
            let stderr = String::from_utf8_lossy(&update.stderr);
            bail!("Failed to set {branch}: {}", stderr.trim());
        }
        epoch_oid.clone()
    } else if git_is_ancestor(&root, &branch_oid, &epoch_oid)? {
        println!("  Advancing {branch} to epoch ({})...", &epoch_oid[..12]);
        let update = Command::new("git")
            .args(["update-ref", &branch_ref, &epoch_oid])
            .current_dir(&root)
            .output()
            .context("Failed to update branch ref")?;

        if !update.status.success() {
            let stderr = String::from_utf8_lossy(&update.stderr);
            bail!("Failed to advance {branch}: {}", stderr.trim());
        }
        epoch_oid.clone()
    } else if git_is_ancestor(&root, &epoch_oid, &branch_oid)? {
        println!(
            "  {branch} is ahead of current epoch ({} > {}). Not rewinding.",
            &branch_oid[..12.min(branch_oid.len())],
            &epoch_oid[..12]
        );
        println!(
            "  WARNING: refs/manifold/epoch/current is stale for this branch tip; releasing from {branch}."
        );
        branch_oid.clone()
    } else {
        bail!(
            "Ref divergence detected: {branch} and refs/manifold/epoch/current do not have an ancestor relationship.\n  \
             Refusing to release to avoid tagging an ambiguous history.\n  \
             To inspect:\n    \
             git -C {} log --oneline --graph --decorate --max-count=30 {branch} refs/manifold/epoch/current",
            root.display()
        );
    };

    // Get commit info for reporting
    let commit_info = get_commit_info(&root, &release_oid)?;
    println!("  {branch} -> {commit_info}");

    // Step 2: Push branch to origin
    println!("Pushing {branch} to origin...");
    let push = Command::new("git")
        .args(["push", "origin", branch])
        .current_dir(&root)
        .output()
        .context("Failed to push branch")?;

    if !push.status.success() {
        let stderr = String::from_utf8_lossy(&push.stderr);
        bail!(
            "Push failed: {}\n  \
             Check: git -C {} log --oneline -5",
            stderr.trim(),
            root.display()
        );
    }

    let push_stderr = String::from_utf8_lossy(&push.stderr);
    if push_stderr.contains("Everything up-to-date") {
        println!("  {branch} was already up to date.");
    } else {
        println!("  Pushed.");
    }

    // Step 3: Create git tag at the branch tip
    println!("Creating tag {tag}...");
    let git_tag = Command::new("git")
        .args(["tag", tag, &release_oid])
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

    // Step 4: Push git tag to origin
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

fn git_is_ancestor(root: &std::path::Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let output = Command::new("git")
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .current_dir(root)
        .output()
        .context("Failed to run git merge-base --is-ancestor")?;

    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git merge-base --is-ancestor failed: {}", stderr.trim());
        }
    }
}

/// Get a short commit info line for a commit hash.
fn get_commit_info(root: &std::path::Path, oid: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["log", "--format=%h %s", "-1", oid])
        .current_dir(root)
        .output()
        .context("Failed to get commit info")?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Ok(oid[..12.min(oid.len())].to_string())
    }
}
