use std::path::Path;
use std::process::Command;

use anyhow::{Result, bail};
use maw_git::GitRepo as _;

use crate::changes::store::ChangesStore;
use crate::workspace::{MawConfig, metadata};

#[derive(Debug, Clone)]
pub(super) struct ActiveChangeEpoch {
    pub(super) change_id: String,
    pub(super) change_branch: String,
    pub(super) head_oid: String,
}

pub(super) fn git_is_ancestor(repo_root: &Path, maybe_ancestor: &str, maybe_descendant: &str) -> Result<bool> {
    let output = Command::new("git")
        .args([
            "merge-base",
            "--is-ancestor",
            maybe_ancestor,
            maybe_descendant,
        ])
        .current_dir(repo_root)
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to run git merge-base --is-ancestor: {e}"))?;

    if output.status.success() {
        return Ok(true);
    }
    if output.status.code() == Some(1) {
        return Ok(false);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("git merge-base --is-ancestor failed: {}", stderr.trim());
}

fn active_change_tracking_epoch(
    root: &Path,
    epoch_oid: &str,
    trunk_branch: &str,
) -> Result<Option<ActiveChangeEpoch>> {
    let store = ChangesStore::open(root);
    let active = store
        .list_active_records()
        .map_err(|e| anyhow::anyhow!("Failed to read active changes: {e}"))?;

    let repo = maw_git::GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;

    let trunk_ref = format!("refs/heads/{trunk_branch}");
    let trunk_head = repo
        .rev_parse_opt(&trunk_ref)
        .map_err(|e| anyhow::anyhow!("failed to read {trunk_ref}: {e}"))?
        .map(|oid| oid.to_string());

    for record in active {
        let change_branch = record.git.change_branch.trim().to_string();
        if change_branch.is_empty() || change_branch == trunk_branch {
            continue;
        }
        let change_ref = format!("refs/heads/{change_branch}");
        let Some(change_head) = repo
            .rev_parse_opt(&change_ref)
            .map_err(|e| anyhow::anyhow!("failed to read {change_ref}: {e}"))?
        else {
            continue;
        };

        let change_head_oid = change_head.to_string();
        if change_head_oid != epoch_oid {
            continue;
        }

        if let Some(trunk_head_oid) = trunk_head.as_deref()
            && git_is_ancestor(root, &change_head_oid, trunk_head_oid)?
        {
            // Already landed on trunk; this is no longer cross-target drift.
            continue;
        }

        return Ok(Some(ActiveChangeEpoch {
            change_id: record.change_id,
            change_branch,
            head_oid: change_head_oid,
        }));
    }

    Ok(None)
}

pub(super) fn cross_target_sync_risk(
    root: &Path,
    ws_name: &str,
    ws_base_epoch: &str,
    epoch_oid: &str,
) -> Result<Option<ActiveChangeEpoch>> {
    let ws_meta = metadata::read(root, ws_name).unwrap_or_default();
    if ws_meta.change_id.is_some() {
        return Ok(None);
    }

    let trunk_branch = MawConfig::load(root)
        .map(|cfg| cfg.branch().to_string())
        .unwrap_or_else(|_| "main".to_string());

    let Some(active_change) = active_change_tracking_epoch(root, epoch_oid, &trunk_branch)? else {
        return Ok(None);
    };

    let trunk_ref = format!("refs/heads/{trunk_branch}");
    let repo = maw_git::GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;
    let Some(trunk_head) = repo
        .rev_parse_opt(&trunk_ref)
        .map_err(|e| anyhow::anyhow!("failed to read {trunk_ref}: {e}"))?
    else {
        return Ok(None);
    };

    let trunk_head_oid = trunk_head.to_string();

    // Only flag likely trunk-pinned workspaces that don't already include the
    // active change head.
    let base_is_on_trunk = git_is_ancestor(root, ws_base_epoch, &trunk_head_oid)?;
    let base_already_has_change = git_is_ancestor(root, &active_change.head_oid, ws_base_epoch)?;
    if base_is_on_trunk && !base_already_has_change {
        return Ok(Some(active_change));
    }

    Ok(None)
}
