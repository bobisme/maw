use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Args;
use maw_git::GitRepo as _;
use tracing::instrument;

use maw_core::merge_state::MergeStateFile;
use crate::transport::ManifoldPushArgs;
use crate::workspace::{MawConfig, git_cwd, repo_root};

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

    #[command(flatten)]
    manifold: ManifoldPushArgs,
}

/// Push the configured branch to its remote using git directly.
///
/// 1. Resolves the branch name from `.maw.toml` (default: `main`).
/// 2. If `--advance`, moves the local branch ref to the current epoch
///    (`refs/manifold/epoch/current`) before pushing.
/// 3. Compares local vs `origin/<branch>` to determine if there's work to push.
/// 4. Runs `git push origin <branch>`.
/// 5. Optionally pushes all tags (unless `--no-tags`).
#[instrument(skip(args), fields(advance = args.advance, no_tags = args.no_tags))]
pub fn run(args: &PushArgs) -> Result<()> {
    let root = repo_root()?;
    let config = MawConfig::load(&root)?;
    let branch = config.branch();

    // Ensure we're operating from within the repo
    let cwd = git_cwd()?;

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
    let branch_needs_push = match &sync {
        SyncStatus::UpToDate => {
            println!("{branch} is up to date with origin.");
            suggest_advance(&root, branch);
            false
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
            true
        }
        SyncStatus::NoRemote => {
            println!("Pushing {branch} to origin (new branch)...");
            true
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
            true
        }
    };

    // Step 3: Push the branch
    // Step 3: Push the branch (only if needed)
    if branch_needs_push {
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
    }

    // Step 4: Push tags (unless --no-tags)
    if !args.no_tags {
        push_tags(&root)?;
    }

    // Step 5: Push Manifold refs (--manifold)
    if args.manifold.manifold {
        crate::transport::push_manifold_refs(&root, "origin", /*dry_run=*/ false)?;
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
///
/// Uses compare-and-swap (CAS) semantics on the ref update to prevent
/// race conditions with concurrent merge operations.
fn advance_branch(root: &std::path::Path, branch: &str) -> Result<()> {
    // Guard: refuse to advance if a merge is in progress (non-terminal phase).
    // A concurrent merge COMMIT could be updating the same ref, so --advance
    // would race with the merge's own CAS ref update.
    let manifold_dir = root.join(".manifold");
    let merge_state_path = MergeStateFile::default_path(&manifold_dir);
    match MergeStateFile::read(&merge_state_path) {
        Ok(state) if !state.phase.is_terminal() => {
            bail!(
                "A merge is in progress (phase: {}).\n  \
                 --advance cannot safely update the branch ref while a merge is active.\n  \
                 Wait for the merge to complete, or abort it first:\n    \
                 maw ws merge --recover",
                state.phase
            );
        }
        // Terminal states (Complete/Aborted) or missing file are fine — no active merge.
        Ok(_) | Err(_) => {}
    }

    // Read the current epoch
    let repo = maw_git::GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;

    let epoch_git_oid = repo.rev_parse_opt("refs/manifold/epoch/current")
        .map_err(|e| anyhow::anyhow!("Failed to read current epoch: {e}"))?;

    let epoch_git_oid = match epoch_git_oid {
        Some(oid) => oid,
        None => bail!(
            "No current epoch found (refs/manifold/epoch/current missing).\n  \
             Run `maw init` first, or ensure maw ws merge has been run."
        ),
    };

    let epoch_oid = epoch_git_oid.to_string();
    let epoch_short = &epoch_oid[..12.min(epoch_oid.len())];

    // Read the current branch position (this is our CAS "expected" value)
    let branch_ref = format!("refs/heads/{branch}");
    let branch_git_oid = repo.rev_parse_opt(&branch_ref)
        .map_err(|e| anyhow::anyhow!("Failed to read branch ref: {e}"))?;

    let branch_oid = match branch_git_oid {
        Some(oid) => oid.to_string(),
        None => String::new(),
    };

    if branch_oid == epoch_oid {
        println!("{branch} already at current epoch ({}).", epoch_short);
        return Ok(());
    }

    if !branch_oid.is_empty() {
        if git_is_ancestor(root, &branch_oid, &epoch_oid)? {
            // branch is behind epoch, safe fast-forward
        } else if git_is_ancestor(root, &epoch_oid, &branch_oid)? {
            println!(
                "{branch} is ahead of current epoch ({} > {}). Leaving branch unchanged.",
                &branch_oid[..12.min(branch_oid.len())],
                epoch_short
            );
            println!(
                "  Hint: epoch is stale for this branch tip. Merge through maw ws merge to advance refs/manifold/epoch/current."
            );
            return Ok(());
        } else {
            bail!(
                "Ref divergence detected: {branch} and refs/manifold/epoch/current do not have an ancestor relationship.\n  \
                 Refusing to move {branch} to avoid data loss.\n  \
                 To inspect:\n    \
                 git -C {} log --oneline --graph --decorate --max-count=30 {branch} refs/manifold/epoch/current",
                root.display()
            );
        }
    }

    // Move the branch to the epoch commit using CAS (compare-and-swap).
    // `git update-ref <ref> <new> <old>` only succeeds if the ref still
    // points to <old>. If another process (e.g., a merge COMMIT) moved the
    // ref between our read and this write, the update fails atomically.
    println!(
        "Advancing {branch} to current epoch ({})...",
        epoch_short
    );

    let expected_old = if branch_oid.is_empty() {
        maw_git::GitOid::ZERO
    } else {
        branch_oid.parse::<maw_git::GitOid>()
            .map_err(|e| anyhow::anyhow!("invalid branch OID '{branch_oid}': {e}"))?
    };

    let ref_name = maw_git::RefName::new(&branch_ref)
        .map_err(|e| anyhow::anyhow!("invalid ref name '{branch_ref}': {e}"))?;

    let edit = maw_git::RefEdit {
        name: ref_name,
        new_oid: epoch_git_oid,
        expected_old_oid: expected_old,
    };

    if let Err(e) = repo.atomic_ref_update(&[edit]) {
        let msg = e.to_string();
        if msg.contains("conflict") || msg.contains("lock") || msg.contains("expected") {
            bail!(
                "Branch ref was modified concurrently (CAS failed).\n  \
                 Another process (likely a merge) updated {branch} between read and write.\n  \
                 Re-run `maw push --advance` to retry.\n  \
                 Detail: {}",
                msg
            );
        }
        bail!("Failed to advance {branch}: {}", msg);
    }

    if branch_oid.is_empty() {
        println!("  Created {branch} at {}", epoch_short);
    } else {
        println!(
            "  {branch}: {} → {}",
            &branch_oid[..12.min(branch_oid.len())],
            epoch_short
        );
    }

    Ok(())
}

fn git_is_ancestor(root: &std::path::Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let repo = maw_git::GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;
    let ancestor_oid: maw_git::GitOid = ancestor.parse()
        .map_err(|e| anyhow::anyhow!("invalid ancestor OID '{ancestor}': {e}"))?;
    let descendant_oid: maw_git::GitOid = descendant.parse()
        .map_err(|e| anyhow::anyhow!("invalid descendant OID '{descendant}': {e}"))?;
    repo.is_ancestor(ancestor_oid, descendant_oid)
        .map_err(|e| anyhow::anyhow!("is_ancestor check failed: {e}"))
}

/// Suggest --advance if the epoch is ahead of the branch.
fn suggest_advance(root: &std::path::Path, branch: &str) {
    let Ok(repo) = maw_git::GixRepo::open(root) else { return };

    let epoch_oid = match repo.rev_parse_opt("refs/manifold/epoch/current") {
        Ok(Some(oid)) => oid,
        _ => return,
    };

    let branch_ref = format!("refs/heads/{branch}");
    let branch_oid = match repo.rev_parse_opt(&branch_ref) {
        Ok(Some(oid)) => oid,
        _ => return,
    };

    if epoch_oid != branch_oid {
        // Check if epoch is ahead of branch
        // TODO(gix): rev-list --count has no GitRepo equivalent. Keep CLI for count.
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
            tracing::warn!(tag = %tag, "Failed to push tag: {}", stderr.trim());
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

    let repo = match maw_git::GixRepo::open(root) {
        Ok(r) => r,
        Err(e) => return SyncStatus::Unknown(format!("failed to open repo: {e}")),
    };

    // Check if local branch exists
    let local_oid = match repo.rev_parse_opt(&branch_ref) {
        Ok(Some(oid)) => oid.to_string(),
        Ok(None) => return SyncStatus::NoLocal,
        Err(e) => return SyncStatus::Unknown(format!("rev-parse {branch_ref} failed: {e}")),
    };

    // Check if remote branch exists
    let remote_oid = match repo.rev_parse_opt(&remote_ref) {
        Ok(Some(oid)) => oid.to_string(),
        Ok(None) => return SyncStatus::NoRemote,
        Err(e) => return SyncStatus::Unknown(format!("rev-parse {remote_ref} failed: {e}")),
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

    let ahead_n: usize = match ahead {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            match stdout.trim().parse::<usize>() {
                Ok(n) => n,
                Err(e) => {
                    return SyncStatus::Unknown(format!(
                        "failed to parse ahead count from git rev-list output {stdout:?}: {e}"
                    ));
                }
            }
        }
        Ok(o) => {
            return SyncStatus::Unknown(format!(
                "git rev-list ahead check failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ));
        }
        Err(e) => {
            return SyncStatus::Unknown(format!("failed to run git rev-list ahead check: {e}"));
        }
    };

    let behind_n: usize = match behind {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            match stdout.trim().parse::<usize>() {
                Ok(n) => n,
                Err(e) => {
                    return SyncStatus::Unknown(format!(
                        "failed to parse behind count from git rev-list output {stdout:?}: {e}"
                    ));
                }
            }
        }
        Ok(o) => {
            return SyncStatus::Unknown(format!(
                "git rev-list behind check failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ));
        }
        Err(e) => {
            return SyncStatus::Unknown(format!("failed to run git rev-list behind check: {e}"));
        }
    };

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
