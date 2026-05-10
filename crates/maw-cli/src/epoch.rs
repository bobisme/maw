//! Epoch management commands.
//!
//! Provides `maw epoch sync` to resync `refs/manifold/epoch/current` to the
//! configured branch HEAD without the side effects of `maw init`.

use anyhow::{Result, bail};

use crate::workspace::{MawConfig, repo_root};
use maw_core::refs as manifold_refs;

/// Resync the epoch ref to the configured branch HEAD.
///
/// This is the targeted fix when `refs/manifold/epoch/current` falls behind
/// the branch (e.g. after direct git commits outside maw). Unlike `maw init`,
/// this only touches the epoch ref — no worktree pruning, migration, or
/// branch re-attachment.
/// # Errors
///
/// Returns an error if the epoch cannot be synchronized with repository state.
pub fn sync() -> Result<()> {
    let root = repo_root()?;
    let config = MawConfig::load(&root).unwrap_or_default();
    let branch = config.branch();
    let branch_ref = format!("refs/heads/{branch}");

    // Read current epoch
    let epoch_oid = match manifold_refs::read_epoch_current(&root) {
        Ok(Some(oid)) => oid,
        Ok(None) => bail!(
            "No epoch ref found (refs/manifold/epoch/current is unset).\n  \
             Run `maw init` to initialize the repository."
        ),
        Err(e) => bail!("Failed to read epoch ref: {e}"),
    };

    // Read branch HEAD
    let branch_oid = match manifold_refs::read_ref(&root, &branch_ref) {
        Ok(Some(oid)) => oid,
        Ok(None) => bail!("Branch '{branch}' does not exist."),
        Err(e) => bail!("Failed to read branch '{branch}': {e}"),
    };

    // Already in sync
    if epoch_oid == branch_oid {
        println!(
            "Epoch is already in sync with '{branch}' at {}.",
            &epoch_oid.as_str()[..12]
        );
        return Ok(());
    }

    // Update epoch ref unconditionally. This handles both cases:
    // - epoch behind branch (direct commits advanced branch)
    // - epoch ahead of branch (merge commit was dropped/reset)
    manifold_refs::write_epoch_current(&root, &branch_oid)
        .map_err(|e| anyhow::anyhow!("Failed to update epoch ref: {e}"))?;

    // Also advance the default workspace's per-workspace baseline. The
    // default workspace IS the working copy of the configured branch — its
    // baseline must equal `refs/manifold/epoch/current`. Without this step,
    // direct commit + epoch sync leaves the default workspace's ref at the
    // OLD epoch; subsequent `maw ws merge` anchors HEAD at the stale
    // baseline during snapshot-replay, treats the direct commit's content
    // as "uncommitted local edits", and double-applies it onto the merge
    // result — producing diff3 markers wrapping duplicated file content in
    // the worktree (silent corruption — bn-3r8s).
    let default_ws = config.default_workspace();
    let default_ws_ref = manifold_refs::workspace_epoch_ref(default_ws);
    if let Err(e) = manifold_refs::write_ref(&root, &default_ws_ref, &branch_oid) {
        eprintln!(
            "  warning: failed to advance default workspace baseline ref '{default_ws_ref}': {e}"
        );
    }

    println!(
        "Epoch synced: {} → {} (branch '{branch}')",
        &epoch_oid.as_str()[..12],
        &branch_oid.as_str()[..12],
    );

    Ok(())
}
