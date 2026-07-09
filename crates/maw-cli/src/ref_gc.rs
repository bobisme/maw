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

use std::collections::HashSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use maw_git::GitRepo as _;

use maw_core::refs;

use crate::workspace::destroy_record;

/// Result of a ref GC pass.
#[derive(Debug, Default)]
pub struct RefGcReport {
    /// Number of stale head refs deleted.
    pub head_refs_deleted: usize,
    /// Number of old recovery snapshots (recovery refs) deleted.
    pub recovery_refs_deleted: usize,
    /// Number of recovery snapshots kept (newer than the age threshold).
    pub recovery_refs_kept: usize,
    /// Names of stale head refs that were deleted (workspace names).
    pub stale_head_names: Vec<String>,
    /// Recovery ref names that were deleted.
    pub deleted_recovery_refs: Vec<String>,
    /// Number of destroy records pruned in lockstep with their recovery refs
    /// (or because their recovery ref was already gone). See [`run`].
    pub destroy_records_deleted: usize,
    /// `(workspace, record filename)` of every destroy record pruned.
    pub deleted_destroy_records: Vec<(String, String)>,
}

/// Count stale head refs (refs for workspaces that no longer exist).
///
/// Used by `maw doctor` to report stale refs without deleting them.
/// # Errors
///
/// Returns an error if stale refs cannot be inspected.
pub fn count_stale_head_refs(root: &Path) -> Result<usize> {
    let repo =
        maw_git::GixRepo::open(root).map_err(|e| anyhow::anyhow!("failed to open repo: {e}"))?;
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
        let ws_dir = maw_core::model::layout::LayoutFlavor::detect_with_env(root)
            .workspace_path(root, ws_name);
        if !ws_dir.exists() {
            count += 1;
        }
    }
    Ok(count)
}

/// Workspace names that an in-flight, non-terminal, *live* merge has frozen
/// as sources. A head ref for such a workspace must NOT be pruned: the
/// running merge legitimately owns the oplog head and will append to it
/// post-COMMIT. Deleting it here would re-introduce the bn-cm63 race from
/// the GC side. Orphaned/indeterminate merge-state does NOT protect a head
/// ref (the merge will never complete), so its dangling refs are still
/// reclaimed — that is the whole point of self-healing GC.
fn live_merge_source_names(root: &Path) -> std::collections::HashSet<String> {
    use maw_core::merge_state::{DEFAULT_STALE_AFTER_SECS, MergeStateFile, Staleness};

    let mut names = std::collections::HashSet::new();
    let state_path = MergeStateFile::default_path(
        &maw_core::model::layout::LayoutFlavor::detect_with_env(root).manifold_dir(root),
    );
    let Ok(state) = MergeStateFile::read(&state_path) else {
        return names;
    };
    if state.phase.is_terminal() {
        return names;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if matches!(
        state.staleness(now, DEFAULT_STALE_AFTER_SECS),
        Staleness::Live
    ) {
        for s in &state.sources {
            names.insert(s.as_str().to_string());
        }
    }
    names
}

/// Prune dangling oplog head refs: `refs/manifold/head/<name>` (and the
/// other refs owned by that workspace) when `ws/<name>/` no longer exists
/// and the workspace is not a source of a *live* in-flight merge.
///
/// Extracted so plain `maw gc` can self-heal leaked head refs (bn-cm63)
/// without also running the recovery-ref age sweep that only `maw gc --recovery-snapshots`
/// should perform.
fn prune_dangling_head_refs(
    repo: &maw_git::GixRepo,
    root: &Path,
    dry_run: bool,
    report: &mut RefGcReport,
) -> Result<()> {
    let head_refs = repo
        .list_refs(refs::HEAD_PREFIX)
        .map_err(|e| anyhow::anyhow!("list_refs failed for head refs: {e}"))?;

    let protected = live_merge_source_names(root);

    for (ref_name, _oid) in &head_refs {
        let ws_name = ref_name
            .as_str()
            .strip_prefix(refs::HEAD_PREFIX)
            .unwrap_or("");
        if ws_name.is_empty() {
            continue;
        }
        let ws_dir = maw_core::model::layout::LayoutFlavor::detect_with_env(root)
            .workspace_path(root, ws_name);
        if ws_dir.exists() {
            continue;
        }
        if protected.contains(ws_name) {
            // A live merge owns this oplog head right now. Skip it; it is
            // not dangling — it will be reclaimed on a later GC once the
            // merge (and any subsequent destroy) settles.
            continue;
        }
        report.stale_head_names.push(ws_name.to_string());
        if !dry_run {
            // Delete every ref owned by this (gone) workspace. Iterates
            // the single source of truth in `workspace_owned_refs` so a
            // new ref kind is a one-line change there (bn-3kcp). The
            // head ref we discovered via list_refs is one of the entries
            // in that set — delete_ref is idempotent so re-deleting it
            // is harmless.
            for owned in refs::workspace_owned_refs(ws_name) {
                let _ = refs::delete_ref(root, &owned);
            }
        }
        report.head_refs_deleted += 1;
    }
    Ok(())
}

/// Prune only dangling oplog head refs (no recovery-ref sweep).
///
/// This is what plain `maw gc` runs so the documented cleanup path actually
/// clears the `maw doctor` "stale head refs" warning, and so already-leaked
/// or legacy dangling head refs self-heal (bn-cm63). `maw gc --recovery-snapshots` still
/// additionally sweeps old recovery refs via [`run`].
///
/// # Errors
///
/// Returns an error if the repository cannot be opened or refs cannot be
/// listed.
pub fn run_head_refs_only(root: &Path, dry_run: bool) -> Result<RefGcReport> {
    let repo =
        maw_git::GixRepo::open(root).map_err(|e| anyhow::anyhow!("failed to open repo: {e}"))?;
    let mut report = RefGcReport::default();
    prune_dangling_head_refs(&repo, root, dry_run, &mut report)?;
    Ok(report)
}

/// CLI entry point for plain `maw gc`'s head-ref self-heal pass (bn-cm63).
///
/// Prints a concise summary only when something was (or would be) cleaned,
/// so the common no-op case stays quiet and does not clutter `maw gc`
/// output.
#[allow(clippy::missing_errors_doc)]
pub fn run_head_refs_cli(root: &Path, dry_run: bool) -> Result<()> {
    let report = run_head_refs_only(root, dry_run)?;
    if report.head_refs_deleted == 0 {
        return Ok(());
    }
    if dry_run {
        println!(
            "Would prune {} dangling head ref(s) for non-existent workspaces:",
            report.head_refs_deleted
        );
        for name in &report.stale_head_names {
            println!("  refs/manifold/head/{name}");
        }
        println!("To apply: maw gc");
    } else {
        println!(
            "Pruned {} dangling head ref(s) for non-existent workspaces.",
            report.head_refs_deleted
        );
    }
    Ok(())
}

/// Run ref GC: delete stale head refs and old recovery refs, and keep destroy
/// records coherent with the recovery refs they claim (bn-3uou).
///
/// - Head refs are deleted if `ws/<name>/` does not exist.
/// - Recovery refs are deleted if the commit they reference is older than
///   `older_than_days` days (default: 30).
/// - Destroy records (the `maw ws recover` audit trail under
///   `.maw/manifold/artifacts/ws/<name>/destroy/`) are pruned in lockstep so
///   the system never lands in the incoherent "record claims a snapshot whose
///   recovery ref was swept" state that a later `git gc --prune` would turn
///   into a dangling pointer. A record is pruned when, for a workspace that no
///   longer exists, either its recovery ref is being swept in this same pass,
///   or its recovery ref is already gone (a prior sweep / manual delete) and
///   the record itself is older than `older_than_days`. Records for
///   still-existing workspaces, and `none`-mode records that never pinned a
///   snapshot, are never touched.
///
/// If `dry_run` is true, nothing is deleted but the report shows what would be.
#[allow(clippy::missing_errors_doc)]
pub fn run(root: &Path, older_than_days: u64, dry_run: bool) -> Result<RefGcReport> {
    let repo =
        maw_git::GixRepo::open(root).map_err(|e| anyhow::anyhow!("failed to open repo: {e}"))?;

    let mut report = RefGcReport::default();

    // --- Head refs ---
    prune_dangling_head_refs(&repo, root, dry_run, &mut report)?;

    // --- Recovery refs ---
    let recovery_prefix = "refs/manifold/recovery/";
    let recovery_refs = repo
        .list_refs(recovery_prefix)
        .map_err(|e| anyhow::anyhow!("list_refs failed for recovery refs: {e}"))?;

    // Every recovery ref that currently exists, captured before deletion so the
    // record-pruning pass can tell "kept (recent pin)" from "already gone".
    let existing_recovery_refs: HashSet<String> = recovery_refs
        .iter()
        .map(|(name, _)| name.as_str().to_string())
        .collect();

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time before UNIX epoch")?
        .as_secs();
    let cutoff = now.saturating_sub(older_than_days.saturating_mul(86_400));

    for (ref_name, oid) in &recovery_refs {
        let commit_ts = get_commit_timestamp(&repo, *oid);
        match commit_ts {
            Some(ts) if ts <= cutoff => {
                report
                    .deleted_recovery_refs
                    .push(ref_name.as_str().to_string());
                if !dry_run {
                    refs::delete_ref(root, ref_name.as_str()).map_err(|e| {
                        anyhow::anyhow!("failed to delete recovery ref {}: {e}", ref_name.as_str())
                    })?;
                }
                report.recovery_refs_deleted += 1;
            }
            Some(_) | None => {
                // Recent enough or unknown commit time — keep conservatively.
                report.recovery_refs_kept += 1;
            }
        }
    }

    // --- Destroy records (coherence with recovery refs) ---
    let swept_recovery_refs: HashSet<String> =
        report.deleted_recovery_refs.iter().cloned().collect();
    prune_desynced_destroy_records(
        root,
        &existing_recovery_refs,
        &swept_recovery_refs,
        cutoff,
        dry_run,
        &mut report,
    )?;

    Ok(report)
}

/// Prune destroy records so they stay coherent with recovery refs.
///
/// Driven by two ref-name sets from the recovery-ref pass:
/// - `existing_recovery_refs`: every recovery ref that existed at the start of
///   this GC (before any deletion).
/// - `swept_recovery_refs`: the subset being deleted in this pass.
///
/// For each destroyed workspace (directory gone) and each of its records that
/// claims a recovery ref:
/// - claimed ref is being swept now → prune the record in lockstep;
/// - claimed ref still exists and is not being swept → keep (recent pin);
/// - claimed ref is already gone → the record is desynced; prune it when it is
///   older than the cutoff.
///
/// `none`-mode records (no snapshot pinned) and records for still-existing
/// workspaces are never touched.
fn prune_desynced_destroy_records(
    root: &Path,
    existing_recovery_refs: &HashSet<String>,
    swept_recovery_refs: &HashSet<String>,
    cutoff: u64,
    dry_run: bool,
    report: &mut RefGcReport,
) -> Result<()> {
    let flavor = maw_core::model::layout::LayoutFlavor::detect_with_env(root);

    for ws in destroy_record::list_destroyed_workspaces(root)? {
        // Never touch records for a workspace that currently exists.
        if flavor.workspace_path(root, &ws).exists() {
            continue;
        }
        for filename in destroy_record::list_record_files(root, &ws)? {
            let Ok(record) = destroy_record::read_record(root, &ws, &filename) else {
                continue;
            };
            let Some(claimed) = record.recovery_ref() else {
                // `none`-mode record: no snapshot, pure audit trail. Leave it.
                continue;
            };
            let prune = if swept_recovery_refs.contains(claimed) {
                // Ref is being swept in this pass — prune the record too so no
                // unpinned-but-claimed state is ever created.
                true
            } else if existing_recovery_refs.contains(claimed) {
                // Ref still pinned and newer than the cutoff — keep both.
                false
            } else {
                // Ref already gone (prior sweep / manual delete): the record is
                // desynced. Age-gate its removal by the record's own timestamp.
                record
                    .destroyed_at_epoch_secs()
                    .is_some_and(|ts| ts <= cutoff)
            };
            if prune {
                if !dry_run {
                    destroy_record::remove_record(root, &ws, &filename)?;
                }
                report.destroy_records_deleted += 1;
                report.deleted_destroy_records.push((ws.clone(), filename));
            }
        }
    }
    Ok(())
}

/// Get the commit timestamp (committer date as unix epoch seconds) for a given OID.
///
/// Returns `None` if the commit cannot be read or the timestamp is negative
/// (which we treat as "missing"). Replaces `git log -1 --format=%ct <oid>`.
fn get_commit_timestamp(repo: &maw_git::GixRepo, oid: maw_git::GitOid) -> Option<u64> {
    let info = repo.read_commit(oid).ok()?;
    u64::try_from(info.committer_time).ok()
}

/// CLI entry point for `maw gc --recovery-snapshots`.
#[allow(clippy::missing_errors_doc)]
pub fn run_cli(root: &Path, older_than_days: u64, dry_run: bool) -> Result<()> {
    let report = run(root, older_than_days, dry_run)?;

    if report.head_refs_deleted == 0
        && report.recovery_refs_deleted == 0
        && report.destroy_records_deleted == 0
    {
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
                "  Would delete {} recovery snapshot(s) older than {older_than_days} day(s) \
                 ({} newer kept):",
                report.recovery_refs_deleted, report.recovery_refs_kept
            );
            for r in &report.deleted_recovery_refs {
                println!("    {r}");
            }
        }
        if !report.deleted_destroy_records.is_empty() {
            println!(
                "  Would prune {} destroy record(s) whose recovery snapshot is (or is being) \
                 removed:",
                report.destroy_records_deleted
            );
            for (ws, file) in &report.deleted_destroy_records {
                println!("    {ws}/{file}");
            }
        }
        println!("To apply: maw gc --recovery-snapshots");
    } else {
        // Recovery refs (the snapshot pins) and destroy records (the
        // `maw ws recover` audit trail) are pruned together so they never
        // disagree — this is what lets `maw doctor`'s abandoned-with-snapshot
        // count actually drop after a GC.
        println!(
            "Pruned {} stale head ref(s); removed {} recovery snapshot(s) and {} destroy \
             record(s) older than {older_than_days} day(s) ({} newer snapshot(s) kept).",
            report.head_refs_deleted,
            report.recovery_refs_deleted,
            report.destroy_records_deleted,
            report.recovery_refs_kept
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
        let dir = TempDir::new().expect("operation should succeed");
        let root = dir.path();

        Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .expect("operation should succeed");
        Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(root)
            .output()
            .expect("operation should succeed");
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(root)
            .output()
            .expect("operation should succeed");
        Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(root)
            .output()
            .expect("operation should succeed");

        fs::write(root.join("README.md"), "# test\n").expect("operation should succeed");
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(root)
            .output()
            .expect("operation should succeed");
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .expect("operation should succeed");

        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .expect("operation should succeed");
        let oid = String::from_utf8(out.stdout)
            .expect("operation should succeed")
            .trim()
            .to_string();

        // Create ws/ directory structure
        fs::create_dir_all(root.join("ws")).expect("operation should succeed");

        (dir, oid)
    }

    #[test]
    fn ref_gc_handles_extreme_age_threshold_without_overflow() {
        let (dir, _) = setup_repo();
        let root = dir.path();

        let report = run(root, u64::MAX, true).expect("ref gc should not overflow");
        assert_eq!(report.head_refs_deleted, 0);
        assert_eq!(report.recovery_refs_deleted, 0);
    }

    #[test]
    fn no_stale_refs_is_noop() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();

        let report = run(root, 30, false).expect("operation should succeed");
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
            &maw_core::model::types::GitOid::new(&oid).expect("operation should succeed"),
        )
        .expect("operation should succeed");

        // Verify the ref exists
        assert!(
            refs::read_ref(root, &refs::workspace_head_ref("gone-agent"))
                .expect("operation should succeed")
                .is_some()
        );

        let report = run(root, 30, false).expect("operation should succeed");
        assert_eq!(report.head_refs_deleted, 1);
        assert_eq!(report.stale_head_names, vec!["gone-agent"]);

        // Ref should be gone
        assert!(
            refs::read_ref(root, &refs::workspace_head_ref("gone-agent"))
                .expect("operation should succeed")
                .is_none()
        );
    }

    #[test]
    fn head_ref_kept_when_workspace_exists() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        // Create workspace directory
        fs::create_dir_all(root.join("ws/active-agent")).expect("operation should succeed");

        // Create a head ref for the workspace
        refs::write_ref(
            root,
            &refs::workspace_head_ref("active-agent"),
            &maw_core::model::types::GitOid::new(&oid).expect("operation should succeed"),
        )
        .expect("operation should succeed");

        let report = run(root, 30, false).expect("operation should succeed");
        assert_eq!(report.head_refs_deleted, 0);

        // Ref should still exist
        assert!(
            refs::read_ref(root, &refs::workspace_head_ref("active-agent"))
                .expect("operation should succeed")
                .is_some()
        );
    }

    #[test]
    fn dry_run_does_not_delete() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        refs::write_ref(
            root,
            &refs::workspace_head_ref("gone-agent"),
            &maw_core::model::types::GitOid::new(&oid).expect("operation should succeed"),
        )
        .expect("operation should succeed");

        let report = run(root, 30, true).expect("operation should succeed");
        assert_eq!(report.head_refs_deleted, 1);

        // Ref should still exist because it was a dry run
        assert!(
            refs::read_ref(root, &refs::workspace_head_ref("gone-agent"))
                .expect("operation should succeed")
                .is_some()
        );
    }

    #[test]
    fn count_stale_head_refs_returns_correct_count() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        let git_oid = maw_core::model::types::GitOid::new(&oid).expect("operation should succeed");

        // Two stale refs
        refs::write_ref(root, &refs::workspace_head_ref("stale-1"), &git_oid)
            .expect("operation should succeed");
        refs::write_ref(root, &refs::workspace_head_ref("stale-2"), &git_oid)
            .expect("operation should succeed");

        // One active ref (workspace exists)
        fs::create_dir_all(root.join("ws/active")).expect("operation should succeed");
        refs::write_ref(root, &refs::workspace_head_ref("active"), &git_oid)
            .expect("operation should succeed");

        let count = count_stale_head_refs(root).expect("operation should succeed");
        assert_eq!(count, 2);
    }

    #[test]
    fn old_recovery_ref_deleted() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        let git_oid = maw_core::model::types::GitOid::new(&oid).expect("operation should succeed");

        // Create a recovery ref. The commit is from "just now", so with
        // older_than_days=0 it should be deleted.
        let recovery_ref = "refs/manifold/recovery/gone-ws/20250101-000000";
        refs::write_ref(root, recovery_ref, &git_oid).expect("operation should succeed");

        let report = run(root, 0, false).expect("operation should succeed");
        assert_eq!(report.recovery_refs_deleted, 1);

        // Ref should be gone
        assert!(
            refs::read_ref(root, recovery_ref)
                .expect("operation should succeed")
                .is_none()
        );
    }

    #[test]
    fn recent_recovery_ref_kept() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        let git_oid = maw_core::model::types::GitOid::new(&oid).expect("operation should succeed");

        // Create a recovery ref. The commit is from "just now", so with
        // older_than_days=30 it should be kept.
        let recovery_ref = "refs/manifold/recovery/some-ws/20260301-000000";
        refs::write_ref(root, recovery_ref, &git_oid).expect("operation should succeed");

        let report = run(root, 30, false).expect("operation should succeed");
        assert_eq!(report.recovery_refs_deleted, 0);

        // Ref should still exist
        assert!(
            refs::read_ref(root, recovery_ref)
                .expect("operation should succeed")
                .is_some()
        );
    }

    // --- bn-cm63: plain `maw gc` head-ref self-heal + live-merge guard ---

    /// Write a `.manifold/merge-state.json` owned by *this* process (so
    /// `staleness` classifies it `Live`) listing `source` as a frozen
    /// source at the `validate` phase.
    fn write_live_merge_state(root: &Path, source: &str) {
        use maw_core::merge_state::{MergePhase, MergeStateFile};
        use maw_core::model::types::{EpochId, WorkspaceId};

        let manifold = root.join(".manifold");
        fs::create_dir_all(&manifold).expect("create .manifold");
        let epoch = EpochId::new(&"a".repeat(40)).expect("epoch");
        let mut state =
            MergeStateFile::new(vec![WorkspaceId::new(source).expect("ws id")], epoch, 0);
        state.stamp_owner(); // pid == our pid -> Liveness::Alive -> Live
        state
            .advance(MergePhase::Build, 1)
            .and_then(|()| state.advance(MergePhase::Validate, 2))
            .expect("advance to validate");
        state
            .write_atomic(&MergeStateFile::default_path(&manifold))
            .expect("write merge-state");
    }

    #[test]
    fn plain_gc_prunes_dangling_head_ref() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        let git_oid = maw_core::model::types::GitOid::new(&oid).expect("oid");

        refs::write_ref(root, &refs::workspace_head_ref("ghost"), &git_oid).expect("write ref");

        // Plain gc path: head refs only, no recovery sweep.
        let report = run_head_refs_only(root, false).expect("run head refs");
        assert_eq!(report.head_refs_deleted, 1);
        assert_eq!(report.stale_head_names, vec!["ghost"]);
        assert!(
            refs::read_ref(root, &refs::workspace_head_ref("ghost"))
                .expect("read")
                .is_none(),
            "plain gc must prune the dangling head ref"
        );
    }

    #[test]
    fn live_merge_source_head_ref_is_protected_from_gc() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        let git_oid = maw_core::model::types::GitOid::new(&oid).expect("oid");

        // A head ref whose workspace dir is gone, but a LIVE merge (owned by
        // this process) has it frozen as a source. It must NOT be pruned —
        // pruning it would re-introduce the bn-cm63 race from the GC side.
        refs::write_ref(root, &refs::workspace_head_ref("inflight"), &git_oid).expect("write ref");
        write_live_merge_state(root, "inflight");

        let report = run_head_refs_only(root, false).expect("run head refs");
        assert_eq!(
            report.head_refs_deleted, 0,
            "a live merge's source head ref must be protected from GC"
        );
        assert!(
            refs::read_ref(root, &refs::workspace_head_ref("inflight"))
                .expect("read")
                .is_some(),
            "live-merge source head ref must survive gc"
        );
    }

    #[test]
    fn non_source_dangling_head_ref_pruned_even_with_live_merge() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        let git_oid = maw_core::model::types::GitOid::new(&oid).expect("oid");

        // Live merge for "inflight"; a *different* workspace "ghost" is gone
        // and is NOT a source — it must still be pruned.
        refs::write_ref(root, &refs::workspace_head_ref("ghost"), &git_oid).expect("write ref");
        write_live_merge_state(root, "inflight");

        let report = run_head_refs_only(root, false).expect("run head refs");
        assert_eq!(report.head_refs_deleted, 1);
        assert_eq!(report.stale_head_names, vec!["ghost"]);
    }

    // --- bn-3uou: destroy-record coherence with recovery refs ---

    use crate::workspace::capture::{CaptureMode, CaptureResult};
    use crate::workspace::destroy_record::{self, DestroyReason, DestroyRecord, RecordCaptureMode};

    /// Create a destroyed-workspace pair (recovery ref + matching destroy
    /// record) pinned at `oid`. The record is written "now" via the real
    /// writer so its `snapshot_ref` is exactly the ref we created.
    fn seed_destroyed_with_ref(root: &Path, ws: &str, oid: &str, ref_ts: &str) -> String {
        let git_oid = maw_core::model::types::GitOid::new(oid).expect("oid");
        let ref_name = format!("refs/manifold/recovery/{ws}/{ref_ts}");
        refs::write_ref(root, &ref_name, &git_oid).expect("write recovery ref");
        let capture = CaptureResult {
            commit_oid: git_oid.clone(),
            pinned_ref: ref_name.clone(),
            dirty_paths: vec!["draft.txt".to_string()],
            mode: CaptureMode::WorktreeCapture,
        };
        let base = maw_core::model::types::EpochId::new(&"a".repeat(40)).expect("epoch");
        destroy_record::write_destroy_record(
            root,
            ws,
            &base,
            &git_oid,
            Some(&capture),
            DestroyReason::Destroy,
        )
        .expect("write destroy record");
        ref_name
    }

    /// Write a destroy record whose claimed recovery ref does NOT exist (the
    /// desynced / already-swept state), with a caller-chosen `destroyed_at`.
    fn seed_orphaned_record(root: &Path, ws: &str, destroyed_at: &str) {
        let rec = DestroyRecord {
            workspace_id: ws.to_string(),
            destroyed_at: destroyed_at.to_string(),
            final_head: "b".repeat(40),
            final_head_ref: None,
            snapshot_oid: Some("c".repeat(40)),
            snapshot_ref: Some(format!("refs/manifold/recovery/{ws}/gone-forever")),
            capture_mode: RecordCaptureMode::DirtySnapshot,
            dirty_files: vec![],
            base_epoch: "a".repeat(40),
            destroy_reason: DestroyReason::Destroy,
            tool_version: "test".to_string(),
        };
        let dir = destroy_record::destroy_dir(root, ws);
        fs::create_dir_all(&dir).expect("create destroy dir");
        let fname = format!("{}.json", destroyed_at.replace(':', "-"));
        fs::write(
            dir.join(&fname),
            serde_json::to_string_pretty(&rec).expect("serialize record"),
        )
        .expect("write record file");
    }

    #[test]
    fn gc_prunes_record_when_recovery_ref_is_swept() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        let ref_name = seed_destroyed_with_ref(root, "alice", &oid, "20260101-000000");

        // older_than 0 → the (now-dated) commit is at/older than the cutoff,
        // so the ref is swept AND its record pruned in lockstep.
        let report = run(root, 0, false).expect("run gc");
        assert_eq!(report.recovery_refs_deleted, 1);
        assert_eq!(report.destroy_records_deleted, 1);

        assert!(
            refs::read_ref(root, &ref_name).expect("read ref").is_none(),
            "recovery ref must be swept"
        );
        assert!(
            destroy_record::list_record_files(root, "alice")
                .expect("list")
                .is_empty(),
            "destroy record must be pruned in lockstep with its ref"
        );
    }

    #[test]
    fn gc_keeps_record_when_recovery_ref_is_recent() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        seed_destroyed_with_ref(root, "bob", &oid, "20260101-000000");

        // older_than 30 → the just-created commit is newer than the cutoff,
        // so both the ref and its record are kept.
        let report = run(root, 30, false).expect("run gc");
        assert_eq!(report.recovery_refs_deleted, 0);
        assert_eq!(report.destroy_records_deleted, 0);
        assert_eq!(
            destroy_record::list_record_files(root, "bob")
                .expect("list")
                .len(),
            1,
            "recent record must be kept"
        );
    }

    #[test]
    fn gc_does_not_touch_records_for_live_workspace() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();

        // A LIVE workspace directory exists AND has an orphaned old record.
        fs::create_dir_all(root.join("ws/carol")).expect("mk ws dir");
        seed_orphaned_record(root, "carol", "2020-01-01T00:00:00.000000000Z");

        let report = run(root, 0, false).expect("run gc");
        assert_eq!(
            report.destroy_records_deleted, 0,
            "records for a still-existing workspace must never be pruned"
        );
        assert_eq!(
            destroy_record::list_record_files(root, "carol")
                .expect("list")
                .len(),
            1
        );
    }

    #[test]
    fn gc_prunes_old_orphaned_record_but_keeps_recent_one() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();

        // Two destroyed (dir-gone) workspaces, each with a record whose
        // recovery ref is already gone (the Defect-B residue). One is old,
        // one is fresh.
        seed_orphaned_record(root, "old-ws", "2020-01-01T00:00:00.000000000Z");
        seed_orphaned_record(
            root,
            "fresh-ws",
            &crate::workspace::now_timestamp_iso8601_precise(),
        );

        let report = run(root, 30, false).expect("run gc");
        assert_eq!(
            report.destroy_records_deleted, 1,
            "only the old orphaned record should be age-gated for pruning"
        );
        assert!(
            destroy_record::list_record_files(root, "old-ws")
                .expect("list")
                .is_empty(),
            "old orphaned record pruned"
        );
        assert_eq!(
            destroy_record::list_record_files(root, "fresh-ws")
                .expect("list")
                .len(),
            1,
            "fresh orphaned record kept (age gate protects it)"
        );
    }

    #[test]
    fn gc_dry_run_reports_but_does_not_prune_records() {
        let (dir, oid) = setup_repo();
        let root = dir.path();
        seed_destroyed_with_ref(root, "dave", &oid, "20260101-000000");

        let report = run(root, 0, true).expect("dry run");
        assert_eq!(report.recovery_refs_deleted, 1);
        assert_eq!(report.destroy_records_deleted, 1);
        // Nothing actually removed.
        assert_eq!(
            destroy_record::list_record_files(root, "dave")
                .expect("list")
                .len(),
            1,
            "dry run must not delete records"
        );
    }
}
