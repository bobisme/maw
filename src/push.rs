use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::workspace::{jj_cwd, repo_root, MawConfig};

#[derive(Args)]
pub struct PushArgs {
    /// Move the branch ref to the current epoch before pushing.
    ///
    /// Use this after merging work directly (without maw ws merge) to
    /// ensure the branch points to the latest epoch. Without this flag,
    /// maw push only pushes if the branch is already ahead of origin.
    #[arg(long)]
    advance: bool,

    /// Skip pushing git tags.
    ///
    /// By default, maw push also pushes any unpushed git tags to origin.
    /// Use this flag to push only the branch.
    #[arg(long)]
    no_tags: bool,
}

/// Push the configured branch to its remote using git directly.
///
/// 1. Resolves the branch name from `.maw.toml` (default: `main`).
/// 2. If `--advance`, moves the local branch ref to the current epoch
///    (`refs/manifold/epoch/current`) before pushing.
/// 3. Compares local vs `origin/<branch>` to determine if there's work to push.
/// 4. Runs `git push origin <branch>`.
/// 5. Optionally pushes all tags (unless `--no-tags`).
pub fn run(args: &PushArgs) -> Result<()> {
    let root = repo_root()?;
    let config = MawConfig::load(&root)?;
    let branch = config.branch();

    // Ensure we're operating from within the repo
    let cwd = jj_cwd()?;

    // Step 0: Fetch to ensure we have latest remote state
    // (silently — we just need refs, not a full pull)
    let _ = Command::new("git")
        .args(["fetch", "origin", "--no-tags", "--quiet"])
        .current_dir(&root)
        .output();

    // Step 1: If --advance, move the branch ref to the current epoch
    if args.advance {
        advance_branch(&root, branch)?;
    }

    // Step 2: Check if there's something to push
    let sync = main_sync_status_inner(&root, branch);
    match &sync {
        SyncStatus::UpToDate => {
            println!("{branch} is up to date with origin.");
            suggest_advance(&root, branch);
            if !args.no_tags {
                push_tags(&root)?;
            }
            return Ok(());
        }
        SyncStatus::Behind(n) => {
            bail!(
                "{branch} is behind origin/{branch} by {n} commit(s).\n  \
                 Fetch and rebase first:\n    \
                 git -C {} fetch origin && git -C {} rebase origin/{branch} {branch}",
                root.display(),
                root.display()
            );
        }
        SyncStatus::Diverged { ahead, behind } => {
            bail!(
                "{branch} has diverged from origin/{branch} (ahead {ahead}, behind {behind}).\n  \
                 Manual recovery needed:\n    \
                 git -C {} fetch origin\n    \
                 git -C {} rebase origin/{branch} {branch}",
                root.display(),
                root.display()
            );
        }
        SyncStatus::Ahead(n) => {
            println!("Pushing {branch} to origin ({n} commit(s))...");
        }
        SyncStatus::NoRemote => {
            println!("Pushing {branch} to origin (new branch)...");
        }
        SyncStatus::NoLocal => {
            bail!(
                "Branch '{branch}' does not exist locally.\n  \
                 After merging work, the branch should be set automatically.\n  \
                 If starting fresh: git -C {} branch {branch} HEAD",
                root.display()
            );
        }
        SyncStatus::Unknown(reason) => {
            println!("Push status unknown ({reason}), attempting push...");
        }
    }

    // Step 3: Push the branch
    let push = Command::new("git")
        .args(["push", "origin", branch])
        .current_dir(&root)
        .output()
        .context("Failed to run git push")?;

    if !push.status.success() {
        let stderr = String::from_utf8_lossy(&push.stderr);
        let stderr_trimmed = stderr.trim();

        if stderr_trimmed.contains("rejected") || stderr_trimmed.contains("non-fast-forward") {
            bail!(
                "Push rejected (non-fast-forward).\n  \
                 Someone else pushed. Fetch and rebase first:\n    \
                 git -C {} fetch origin && git -C {} rebase origin/{branch} {branch}",
                root.display(),
                root.display()
            );
        }

        bail!("git push failed: {stderr_trimmed}");
    }

    // Print what was pushed
    let push_stderr = String::from_utf8_lossy(&push.stderr);
    if push_stderr.contains("Everything up-to-date") {
        println!("{branch} was already up to date.");
    } else {
        // Show the push summary from git
        println!("Changes pushed to origin:");
        for line in push_stderr.lines() {
            if !line.trim().is_empty() {
                println!("  {line}");
            }
        }
    }

    // Step 4: Push tags (unless --no-tags)
    if !args.no_tags {
        push_tags(&root)?;
    }

    let _ = cwd; // used for validation above
    Ok(())
}

/// Move the local branch ref to the current epoch.
///
/// In Manifold v2, `maw ws merge` updates both the epoch ref and the
/// branch ref. But if work was committed directly (e.g., manual edits
/// in the default workspace), the branch may lag behind the epoch.
/// `--advance` moves the branch to match the current epoch.
fn advance_branch(root: &std::path::Path, branch: &str) -> Result<()> {
    // Read the current epoch
    let epoch_output = Command::new("git")
        .args(["rev-parse", "refs/manifold/epoch/current"])
        .current_dir(root)
        .output()
        .context("Failed to read current epoch")?;

    if !epoch_output.status.success() {
        bail!(
            "No current epoch found (refs/manifold/epoch/current missing).\n  \
             Run `maw init` first, or ensure maw ws merge has been run."
        );
    }

    let epoch_oid = String::from_utf8_lossy(&epoch_output.stdout).trim().to_string();

    // Read the current branch position
    let branch_ref = format!("refs/heads/{branch}");
    let branch_output = Command::new("git")
        .args(["rev-parse", &branch_ref])
        .current_dir(root)
        .output()
        .context("Failed to read branch ref")?;

    let branch_oid = if branch_output.status.success() {
        String::from_utf8_lossy(&branch_output.stdout).trim().to_string()
    } else {
        String::new()
    };

    if branch_oid == epoch_oid {
        println!("{branch} already at current epoch ({}).", &epoch_oid[..12]);
        return Ok(());
    }

    // Move the branch to the epoch commit
    println!(
        "Advancing {branch} to current epoch ({})...",
        &epoch_oid[..12]
    );

    let update = Command::new("git")
        .args(["update-ref", &branch_ref, &epoch_oid])
        .current_dir(root)
        .output()
        .context("Failed to update branch ref")?;

    if !update.status.success() {
        let stderr = String::from_utf8_lossy(&update.stderr);
        bail!("Failed to advance {branch}: {}", stderr.trim());
    }

    if branch_oid.is_empty() {
        println!("  Created {branch} at {}", &epoch_oid[..12]);
    } else {
        println!(
            "  {branch}: {} → {}",
            &branch_oid[..12.min(branch_oid.len())],
            &epoch_oid[..12]
        );
    }

    Ok(())
}

/// Suggest --advance if the epoch is ahead of the branch.
fn suggest_advance(root: &std::path::Path, branch: &str) {
    let epoch = Command::new("git")
        .args(["rev-parse", "refs/manifold/epoch/current"])
        .current_dir(root)
        .output();

    let branch_ref = format!("refs/heads/{branch}");
    let branch_pos = Command::new("git")
        .args(["rev-parse", &branch_ref])
        .current_dir(root)
        .output();

    if let (Ok(e), Ok(b)) = (epoch, branch_pos) {
        if e.status.success() && b.status.success() {
            let epoch_oid = String::from_utf8_lossy(&e.stdout).trim().to_string();
            let branch_oid = String::from_utf8_lossy(&b.stdout).trim().to_string();

            if epoch_oid != branch_oid {
                // Check if epoch is ahead of branch
                let count = Command::new("git")
                    .args(["rev-list", "--count", &format!("{branch_oid}..{epoch_oid}")])
                    .current_dir(root)
                    .output();
                if let Ok(c) = count {
                    let n: usize = String::from_utf8_lossy(&c.stdout)
                        .trim()
                        .parse()
                        .unwrap_or(0);
                    if n > 0 {
                        println!();
                        println!(
                            "Hint: epoch is {n} commit(s) ahead of {branch}.\n  \
                             To push latest work: maw push --advance"
                        );
                    }
                }
            }
        }
    }
}

/// Push unpushed git tags to origin.
fn push_tags(root: &std::path::Path) -> Result<()> {
    // Find tags that exist locally but not on the remote
    let local_tags = Command::new("git")
        .args(["tag", "--list"])
        .current_dir(root)
        .output()
        .context("Failed to list local tags")?;

    let remote_tags = Command::new("git")
        .args(["ls-remote", "--tags", "origin"])
        .current_dir(root)
        .output()
        .context("Failed to list remote tags")?;

    if !local_tags.status.success() || !remote_tags.status.success() {
        return Ok(()); // Silently skip if we can't determine tag state
    }

    let local: Vec<String> = String::from_utf8_lossy(&local_tags.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    let remote_str = String::from_utf8_lossy(&remote_tags.stdout);
    let remote: Vec<String> = remote_str
        .lines()
        .filter_map(|line| {
            // Format: "<oid>\trefs/tags/<name>"
            line.split('\t')
                .nth(1)
                .and_then(|r| r.strip_prefix("refs/tags/"))
                .map(|t| t.strip_suffix("^{}").unwrap_or(t).to_string())
        })
        .collect();

    let unpushed: Vec<&String> = local.iter().filter(|t| !remote.contains(t)).collect();

    if unpushed.is_empty() {
        return Ok(());
    }

    println!("Pushing {} tag(s)...", unpushed.len());
    for tag in &unpushed {
        let push = Command::new("git")
            .args(["push", "origin", tag])
            .current_dir(root)
            .output()
            .context("Failed to push tag")?;

        if push.status.success() {
            println!("  Pushed tag: {tag}");
        } else {
            let stderr = String::from_utf8_lossy(&push.stderr);
            eprintln!("  WARNING: Failed to push tag {tag}: {}", stderr.trim());
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Sync status helpers (reused by status.rs)
// ---------------------------------------------------------------------------

/// Sync status between local branch and remote.
#[derive(Debug)]
pub enum SyncStatus {
    UpToDate,
    Ahead(usize),
    Behind(usize),
    Diverged { ahead: usize, behind: usize },
    NoRemote,
    NoLocal,
    Unknown(String),
}

impl SyncStatus {
    /// One-line summary for status bar.
    pub fn oneline(&self) -> String {
        match self {
            Self::UpToDate => "sync".to_string(),
            Self::Ahead(n) => format!("ahead({n})"),
            Self::Behind(n) => format!("behind({n})"),
            Self::Diverged { ahead, behind } => format!("diverged({ahead}/{behind})"),
            Self::NoRemote => "no-remote".to_string(),
            Self::NoLocal => "no-local".to_string(),
            Self::Unknown(_) => "unknown".to_string(),
        }
    }

    /// Human-readable description.
    pub fn describe(&self) -> String {
        match self {
            Self::UpToDate => "up to date".to_string(),
            Self::Ahead(n) => format!("ahead by {n} (not pushed)"),
            Self::Behind(n) => format!("behind by {n}"),
            Self::Diverged { ahead, behind } => {
                format!("diverged (ahead {ahead}, behind {behind})")
            }
            Self::NoRemote => "no origin remote".to_string(),
            Self::NoLocal => "no local branch".to_string(),
            Self::Unknown(reason) => format!("unknown ({reason})"),
        }
    }

    /// Whether this status indicates a warning condition.
    pub const fn is_warning(&self) -> bool {
        !matches!(self, Self::UpToDate)
    }
}

/// Determine sync status between local branch and origin/<branch>.
pub fn main_sync_status_inner(root: &std::path::Path, branch: &str) -> SyncStatus {
    let branch_ref = format!("refs/heads/{branch}");
    let remote_ref = format!("refs/remotes/origin/{branch}");

    // Check if local branch exists
    let local = Command::new("git")
        .args(["rev-parse", "--verify", &branch_ref])
        .current_dir(root)
        .output();

    let local_oid = match local {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        }
        _ => return SyncStatus::NoLocal,
    };

    // Check if remote branch exists
    let remote = Command::new("git")
        .args(["rev-parse", "--verify", &remote_ref])
        .current_dir(root)
        .output();

    let remote_oid = match remote {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        }
        _ => return SyncStatus::NoRemote,
    };

    if local_oid == remote_oid {
        return SyncStatus::UpToDate;
    }

    // Count commits ahead and behind using rev-list
    let ahead = Command::new("git")
        .args(["rev-list", "--count", &format!("{remote_oid}..{local_oid}")])
        .current_dir(root)
        .output();

    let behind = Command::new("git")
        .args(["rev-list", "--count", &format!("{local_oid}..{remote_oid}")])
        .current_dir(root)
        .output();

    let ahead_n: usize = ahead
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0);

    let behind_n: usize = behind
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0);

    match (ahead_n, behind_n) {
        (0, 0) => SyncStatus::UpToDate,
        (a, 0) => SyncStatus::Ahead(a),
        (0, b) => SyncStatus::Behind(b),
        (a, b) => SyncStatus::Diverged {
            ahead: a,
            behind: b,
        },
    }
}
