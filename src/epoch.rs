//! Epoch management commands.
//!
//! Provides `maw epoch sync` to resync `refs/manifold/epoch/current` to the
//! configured branch HEAD without the side effects of `maw init`.

use std::process::Command;

use anyhow::{Result, bail};

use crate::model::types::EpochId;
use crate::refs as manifold_refs;
use crate::workspace::{MawConfig, repo_root};

/// Resync the epoch ref to the configured branch HEAD.
///
/// This is the targeted fix when `refs/manifold/epoch/current` falls behind
/// the branch (e.g. after direct git commits outside maw). Unlike `maw init`,
/// this only touches the epoch ref — no worktree pruning, migration, or
/// branch re-attachment.
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
    let epoch_id =
        EpochId::new(epoch_oid.as_str()).map_err(|e| anyhow::anyhow!("Invalid epoch OID: {e}"))?;
    let branch_id = EpochId::new(branch_oid.as_str())
        .map_err(|e| anyhow::anyhow!("Invalid branch OID: {e}"))?;

    if epoch_id == branch_id {
        println!(
            "Epoch is already in sync with '{branch}' at {}.",
            &epoch_oid.as_str()[..12]
        );
        return Ok(());
    }

    // Verify epoch is ancestor of branch HEAD (fast-forward)
    if !is_ancestor(&root, &epoch_id, &branch_id)? {
        bail!(
            "Cannot sync: epoch ({}) is not an ancestor of '{branch}' ({}).\n  \
             The branch and epoch have diverged. Run `maw init` to repair.",
            &epoch_oid.as_str()[..12],
            &branch_oid.as_str()[..12],
        );
    }

    // Advance epoch ref
    manifold_refs::write_epoch_current(&root, &branch_oid)
        .map_err(|e| anyhow::anyhow!("Failed to update epoch ref: {e}"))?;

    println!(
        "Epoch synced: {} → {} (branch '{branch}')",
        &epoch_oid.as_str()[..12],
        &branch_oid.as_str()[..12],
    );

    Ok(())
}

/// Check if `ancestor` is a git ancestor of `descendant`.
fn is_ancestor(root: &std::path::Path, ancestor: &EpochId, descendant: &EpochId) -> Result<bool> {
    let output = Command::new("git")
        .args([
            "merge-base",
            "--is-ancestor",
            ancestor.as_str(),
            descendant.as_str(),
        ])
        .current_dir(root)
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to run git merge-base: {e}"))?;

    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "git merge-base failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}
