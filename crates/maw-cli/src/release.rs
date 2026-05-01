use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Args;
use maw_git::GitRepo as _;

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
/// # Errors
///
/// Returns an error if release validation or git operations fail.
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
    let repo = maw_git::GixRepo::open(&root)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;

    let epoch_oid = match repo.rev_parse_opt("refs/manifold/epoch/current") {
        Ok(Some(oid)) => oid.to_string(),
        _ => bail!(
            "No current epoch found.\n  \
             Run `maw init` and `maw ws merge` first."
        ),
    };

    // Read current branch position
    let branch_ref = format!("refs/heads/{branch}");
    let branch_oid = repo
        .rev_parse_opt(&branch_ref)
        .ok()
        .flatten()
        .map(|o| o.to_string())
        .unwrap_or_default();

    let release_oid = if branch_oid == epoch_oid {
        println!("  {branch} already at current epoch.");
        epoch_oid
    } else if branch_oid.is_empty() {
        println!(
            "  Creating {branch} at epoch ({})...",
            short_oid(&epoch_oid)
        );
        let ref_name = maw_git::RefName::new(&branch_ref)
            .map_err(|e| anyhow::anyhow!("invalid ref name: {e}"))?;
        let oid: maw_git::GitOid = epoch_oid
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid epoch OID: {e}"))?;
        repo.write_ref(&ref_name, oid, &format!("release: create {branch}"))
            .map_err(|e| anyhow::anyhow!("Failed to set {branch}: {e}"))?;
        epoch_oid
    } else if git_is_ancestor(&root, &branch_oid, &epoch_oid)? {
        println!(
            "  Advancing {branch} to epoch ({})...",
            short_oid(&epoch_oid)
        );
        let ref_name = maw_git::RefName::new(&branch_ref)
            .map_err(|e| anyhow::anyhow!("invalid ref name: {e}"))?;
        let oid: maw_git::GitOid = epoch_oid
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid epoch OID: {e}"))?;
        repo.write_ref(&ref_name, oid, &format!("release: advance {branch}"))
            .map_err(|e| anyhow::anyhow!("Failed to advance {branch}: {e}"))?;
        epoch_oid
    } else if git_is_ancestor(&root, &epoch_oid, &branch_oid)? {
        println!(
            "  {branch} is ahead of current epoch ({} > {}). Not rewinding.",
            &branch_oid[..12.min(branch_oid.len())],
            short_oid(&epoch_oid)
        );
        println!(
            "  WARNING: refs/manifold/epoch/current is stale for this branch tip; releasing from {branch}."
        );
        branch_oid
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
    create_or_verify_tag(&root, tag, &release_oid)?;

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
    let repo = maw_git::GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;
    let ancestor_oid: maw_git::GitOid = ancestor
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid ancestor OID: {e}"))?;
    let descendant_oid: maw_git::GitOid = descendant
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid descendant OID: {e}"))?;
    repo.is_ancestor(ancestor_oid, descendant_oid)
        .map_err(|e| anyhow::anyhow!("is_ancestor failed: {e}"))
}

/// Get a short commit info line for a commit hash.
fn get_commit_info(root: &std::path::Path, oid: &str) -> Result<String> {
    let repo = maw_git::GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;
    let git_oid: maw_git::GitOid = oid
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid OID: {e}"))?;
    match repo.read_commit(git_oid) {
        Ok(info) => {
            let short_oid = &oid[..12.min(oid.len())];
            let subject = info.message.lines().next().unwrap_or("").to_string();
            Ok(format!("{short_oid} {subject}"))
        }
        Err(_) => Ok(oid[..12.min(oid.len())].to_string()),
    }
}

fn short_oid(oid: &str) -> &str {
    &oid[..12.min(oid.len())]
}

fn create_or_verify_tag(root: &std::path::Path, tag: &str, release_oid: &str) -> Result<()> {
    if let Some(existing_oid) = resolve_tag_target_oid(root, tag)? {
        if existing_oid == release_oid {
            println!(
                "  Git tag {tag} already exists at {}.",
                short_oid(release_oid)
            );
            return Ok(());
        }
        bail!(
            "Git tag {tag} already exists at {}, but this release targets {}.\n  \
             Refusing to push a tag that points at the wrong commit.\n  \
             Inspect locally:\n    \
             git -C {} show --no-patch --decorate {tag}\n    \
             git -C {} show --no-patch --decorate {}",
            short_oid(&existing_oid),
            short_oid(release_oid),
            root.display(),
            root.display(),
            short_oid(release_oid)
        );
    }

    let git_tag = Command::new("git")
        .args(["tag", tag, release_oid])
        .current_dir(root)
        .output()
        .context("Failed to create git tag")?;

    if git_tag.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&git_tag.stderr);
    if stderr.contains("already exists") {
        let existing_oid = resolve_tag_target_oid(root, tag)?.ok_or_else(|| {
            anyhow::anyhow!("Git reported that tag {tag} exists, but it could not be resolved")
        })?;
        if existing_oid == release_oid {
            println!(
                "  Git tag {tag} already exists at {}.",
                short_oid(release_oid)
            );
            return Ok(());
        }
        bail!(
            "Git tag {tag} was created concurrently at {}, but this release targets {}.\n  \
             Refusing to push a tag that points at the wrong commit.",
            short_oid(&existing_oid),
            short_oid(release_oid)
        );
    }

    bail!("Failed to create git tag: {}", stderr.trim());
}

fn resolve_tag_target_oid(root: &std::path::Path, tag: &str) -> Result<Option<String>> {
    let tag_ref = format!("refs/tags/{tag}^{{}}");
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", &tag_ref])
        .current_dir(root)
        .output()
        .context("Failed to inspect git tag")?;

    if output.status.success() {
        let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if oid.is_empty() {
            Ok(None)
        } else {
            Ok(Some(oid))
        }
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use tempfile::TempDir;

    use super::{create_or_verify_tag, resolve_tag_target_oid};

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap_or_else(|e| panic!("failed to run git {}: {e}", args.join(" ")));
        assert!(
            output.status.success(),
            "git {} failed:\nstdout: {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn repo_with_two_commits() -> (TempDir, String, String) {
        let dir = TempDir::new().expect("operation should succeed");
        let root = dir.path();
        git(root, &["init"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "Test User"]);

        std::fs::write(root.join("file.txt"), "one\n").expect("operation should succeed");
        git(root, &["add", "file.txt"]);
        git(root, &["commit", "-m", "first"]);
        let first = git(root, &["rev-parse", "HEAD"]);

        std::fs::write(root.join("file.txt"), "two\n").expect("operation should succeed");
        git(root, &["commit", "-am", "second"]);
        let second = git(root, &["rev-parse", "HEAD"]);

        (dir, first, second)
    }

    #[test]
    fn create_or_verify_tag_accepts_existing_tag_at_release_commit() {
        let (dir, first, _) = repo_with_two_commits();
        let root = dir.path();
        git(root, &["tag", "v1.0.0", &first]);

        create_or_verify_tag(root, "v1.0.0", &first).expect("operation should succeed");

        assert_eq!(
            resolve_tag_target_oid(root, "v1.0.0")
                .expect("operation should succeed")
                .as_deref(),
            Some(first.as_str())
        );
    }

    #[test]
    fn create_or_verify_tag_rejects_existing_tag_at_different_commit() {
        let (dir, first, second) = repo_with_two_commits();
        let root = dir.path();
        git(root, &["tag", "v1.0.0", &first]);

        let err = create_or_verify_tag(root, "v1.0.0", &second)
            .expect_err("tag mismatch must be rejected")
            .to_string();

        assert!(err.contains("already exists at"), "{err}");
        assert!(err.contains("release targets"), "{err}");
        assert_eq!(
            resolve_tag_target_oid(root, "v1.0.0")
                .expect("operation should succeed")
                .as_deref(),
            Some(first.as_str())
        );
    }
}
