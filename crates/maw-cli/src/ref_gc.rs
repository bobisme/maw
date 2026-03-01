//! Garbage-collect stale `refs/manifold/head/*` and `refs/manifold/recovery/*` refs.
//!
//! Over the lifetime of a project, agent workspaces are created and destroyed.
//! The head refs (`refs/manifold/head/<name>`) and recovery refs
//! (`refs/manifold/recovery/<name>/*`) for destroyed workspaces accumulate
//! indefinitely. This module provides a GC mechanism to clean them up.
//!
//! # Head refs
//!
//! A head ref is considered stale if the corresponding workspace directory
//! (`ws/<name>/`) no longer exists.
//!
//! # Recovery refs
//!
//! Recovery refs are deleted if they are older than a configurable threshold
//! (default: 30 days), based on the commit timestamp of the referenced commit.

use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use maw_git::GitRepo as _;

use maw_core::refs;

/// Result of a ref GC pass.
#[derive(Debug, Default)]
pub struct RefGcReport {
    /// Number of stale head refs deleted.
    pub head_refs_deleted: usize,
    /// Number of old recovery refs deleted.
    pub recovery_refs_deleted: usize,
    /// Names of stale head refs that were deleted (workspace names).
    pub stale_head_names: Vec<String>,
    /// Recovery ref names that were deleted.
    pub deleted_recovery_refs: Vec<String>,
}

/// Count stale head refs (refs for workspaces that no longer exist).
///
/// Used by `maw doctor` to report stale refs without deleting them.
pub fn count_stale_head_refs(root: &Path) -> Result<usize> {
    let repo = maw_git::GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo: {e}"))?;
    let head_refs = repo
        .list_refs(refs::HEAD_PREFIX)
        .map_err(|e| anyhow::anyhow!("list_refs failed: {e}"))?;

    let mut count = 0;
    for (ref_name, _oid) in &head_refs {
        let ws_name = ref_name
            .as_str()
            .strip_prefix(refs::HEAD_PREFIX)
            .unwrap_or("");
        if ws_name.is_empty() {
            continue;
        }
        let ws_dir = root.join("ws").join(ws_name);
        if !ws_dir.exists() {
            count += 1;
        }
    }
    Ok(count)
}

/// Run ref GC: delete stale head refs and old recovery refs.
///
/// - Head refs are deleted if `ws/<name>/` does not exist.
/// - Recovery refs are deleted if the commit they reference is older than
///   `older_than_days` days (default: 30).
///
/// If `dry_run` is true, nothing is deleted but the report shows what would be.
#[allow(clippy::missing_errors_doc)]
pub fn run(root: &Path, older_than_days: u64, dry_run: bool) -> Result<RefGcReport> {
    let repo = maw_git::GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo: {e}"))?;

    let mut report = RefGcReport::default();

    // --- Head refs ---
    let head_refs = repo
        .list_refs(refs::HEAD_PREFIX)
        .map_err(|e| anyhow::anyhow!("list_refs failed for head refs: {e}"))?;

    for (ref_name, _oid) in &head_refs {
        let ws_name = ref_name
            .as_str()
            .strip_prefix(refs::HEAD_PREFIX)
            .unwrap_or("");
        if ws_name.is_empty() {
            continue;
        }
        let ws_dir = root.join("ws").join(ws_name);
        if !ws_dir.exists() {
            report.stale_head_names.push(ws_name.to_string());
            if !dry_run {
                refs::delete_ref(root, ref_name.as_str())
                    .map_err(|e| anyhow::anyhow!("failed to delete ref {}: {e}", ref_name.as_str()))?;

                // Also clean up associated workspace state and epoch refs.
                let state_ref = refs::workspace_state_ref(ws_name);
                let _ = refs::delete_ref(root, &state_ref);
                let epoch_ref = refs::workspace_epoch_ref(ws_name);
                let _ = refs::delete_ref(root, &epoch_ref);
            }
            report.head_refs_deleted += 1;
        }
    }

    // --- Recovery refs ---
    let recovery_prefix = "refs/manifold/recovery/";
    let recovery_refs = repo
        .list_refs(recovery_prefix)
        .map_err(|e| anyhow::anyhow!("list_refs failed for recovery refs: {e}"))?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time before UNIX epoch")?
        .as_secs();
    let cutoff = now.saturating_sub(older_than_days * 86400);

    for (ref_name, oid) in &recovery_refs {
        let commit_ts = get_commit_timestamp(root, oid.to_string().as_str());
        match commit_ts {
            Some(ts) if ts <= cutoff => {
                report
                    .deleted_recovery_refs
                    .push(ref_name.as_str().to_string());
                if !dry_run {
                    refs::delete_ref(root, ref_name.as_str()).map_err(|e| {
                        anyhow::anyhow!(
                            "failed to delete recovery ref {}: {e}",
                            ref_name.as_str()
                        )
                    })?;
                }
                report.recovery_refs_deleted += 1;
            }
            Some(_) => {
                // Recent enough — keep.
            }
            None => {
                // Could not determine commit time — skip (conservative).
            }
        }
    }

    Ok(report)
}

/// Get the commit timestamp (committer date as unix epoch seconds) for a given OID.
///
/// Uses `git log -1 --format=%ct <oid>` because `CommitInfo` does not expose
/// a timestamp field. Marked TODO(gix) for future migration.
// TODO(gix): Add committer_time to CommitInfo and use read_commit instead.
fn get_commit_timestamp(root: &Path, oid: &str) -> Option<u64> {
    let output = Command::new("git")
        .args(["log", "-1", "--format=%ct", oid])
        .current_dir(root)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.trim().parse::<u64>().ok()
}

/// CLI entry point for `maw gc --refs`.
#[allow(clippy::missing_errors_doc)]
pub fn run_cli(root: &Path, older_than_days: u64, dry_run: bool) -> Result<()> {
    let report = run(root, older_than_days, dry_run)?;

    if report.head_refs_deleted == 0 && report.recovery_refs_deleted == 0 {
        println!("No stale refs found. Nothing to clean up.");
        return Ok(());
    }

    if dry_run {
        println!("Ref GC preview (dry run):");
        if !report.stale_head_names.is_empty() {
            println!(
                "  Would delete {} stale head ref(s):",
                report.head_refs_deleted
            );
            for name in &report.stale_head_names {
                println!("    refs/manifold/head/{name}");
            }
        }
        if !report.deleted_recovery_refs.is_empty() {
            println!(
                "  Would delete {} recovery ref(s) older than {older_than_days} day(s):",
                report.recovery_refs_deleted
            );
            for r in &report.deleted_recovery_refs {
                println!("    {r}");
            }
        }
        println!("To apply: maw gc --refs");
    } else {
        println!(
            "Cleaned {} head ref(s), {} recovery ref(s)",
            report.head_refs_deleted, report.recovery_refs_deleted
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
mod tests {
    use std::fs;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    fn setup_repo() -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(root)
            .output()
            .unwrap();

        fs::write(root.join("README.md"), "# test\n").unwrap();
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        let oid = String::from_utf8(out.stdout).unwrap().trim().to_string();

        // Create ws/ directory structure
        fs::create_dir_all(root.join("ws")).unwrap();

        (dir, oid)
    }

    #[test]
    fn no_stale_refs_is_noop() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();

        let report = run(root, 30, false).unwrap();
        assert_eq!(report.head_refs_deleted, 0);
        assert_eq!(report.recovery_refs_deleted, 0);
    }

    #[test]
    fn stale_head_ref_deleted_when_workspace_gone() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        // Create a head ref for a workspace that does not exist
        refs::write_ref(
            root,
            &refs::workspace_head_ref("gone-agent"),
            &maw_core::model::types::GitOid::new(&oid).unwrap(),
        )
        .unwrap();

        // Verify the ref exists
        assert!(refs::read_ref(root, &refs::workspace_head_ref("gone-agent"))
            .unwrap()
            .is_some());

        let report = run(root, 30, false).unwrap();
        assert_eq!(report.head_refs_deleted, 1);
        assert_eq!(report.stale_head_names, vec!["gone-agent"]);

        // Ref should be gone
        assert!(refs::read_ref(root, &refs::workspace_head_ref("gone-agent"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn head_ref_kept_when_workspace_exists() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        // Create workspace directory
        fs::create_dir_all(root.join("ws/active-agent")).unwrap();

        // Create a head ref for the workspace
        refs::write_ref(
            root,
            &refs::workspace_head_ref("active-agent"),
            &maw_core::model::types::GitOid::new(&oid).unwrap(),
        )
        .unwrap();

        let report = run(root, 30, false).unwrap();
        assert_eq!(report.head_refs_deleted, 0);

        // Ref should still exist
        assert!(refs::read_ref(root, &refs::workspace_head_ref("active-agent"))
            .unwrap()
            .is_some());
    }

    #[test]
    fn dry_run_does_not_delete() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        refs::write_ref(
            root,
            &refs::workspace_head_ref("gone-agent"),
            &maw_core::model::types::GitOid::new(&oid).unwrap(),
        )
        .unwrap();

        let report = run(root, 30, true).unwrap();
        assert_eq!(report.head_refs_deleted, 1);

        // Ref should still exist because it was a dry run
        assert!(refs::read_ref(root, &refs::workspace_head_ref("gone-agent"))
            .unwrap()
            .is_some());
    }

    #[test]
    fn count_stale_head_refs_returns_correct_count() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        let git_oid = maw_core::model::types::GitOid::new(&oid).unwrap();

        // Two stale refs
        refs::write_ref(root, &refs::workspace_head_ref("stale-1"), &git_oid).unwrap();
        refs::write_ref(root, &refs::workspace_head_ref("stale-2"), &git_oid).unwrap();

        // One active ref (workspace exists)
        fs::create_dir_all(root.join("ws/active")).unwrap();
        refs::write_ref(root, &refs::workspace_head_ref("active"), &git_oid).unwrap();

        let count = count_stale_head_refs(root).unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn old_recovery_ref_deleted() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        let git_oid = maw_core::model::types::GitOid::new(&oid).unwrap();

        // Create a recovery ref. The commit is from "just now", so with
        // older_than_days=0 it should be deleted.
        let recovery_ref = "refs/manifold/recovery/gone-ws/20250101-000000";
        refs::write_ref(root, recovery_ref, &git_oid).unwrap();

        let report = run(root, 0, false).unwrap();
        assert_eq!(report.recovery_refs_deleted, 1);

        // Ref should be gone
        assert!(refs::read_ref(root, recovery_ref).unwrap().is_none());
    }

    #[test]
    fn recent_recovery_ref_kept() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        let git_oid = maw_core::model::types::GitOid::new(&oid).unwrap();

        // Create a recovery ref. The commit is from "just now", so with
        // older_than_days=30 it should be kept.
        let recovery_ref = "refs/manifold/recovery/some-ws/20260301-000000";
        refs::write_ref(root, recovery_ref, &git_oid).unwrap();

        let report = run(root, 30, false).unwrap();
        assert_eq!(report.recovery_refs_deleted, 0);

        // Ref should still exist
        assert!(refs::read_ref(root, recovery_ref).unwrap().is_some());
    }
}
