use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::backend::WorkspaceBackend;
use crate::model::diff::compute_patchset;
use crate::model::patch::{PatchSet, PatchValue};
use crate::model::types::{EpochId, GitOid, WorkspaceId};
use crate::oplog::read::{read_head, walk_chain};
use crate::oplog::types::{OpPayload, Operation};

use super::{get_backend, oplog_runtime::append_operation_with_runtime_checkpoint, repo_root};

pub fn undo(name: &str) -> Result<()> {
    let ws_id = WorkspaceId::new(name)
        .map_err(|e| anyhow::anyhow!("invalid workspace name '{name}': {e}"))?;

    let backend = get_backend()?;
    if !backend.exists(&ws_id) {
        bail!(
            "Workspace '{name}' does not exist\n  Check: maw ws list\n  Next: maw ws undo <workspace>"
        );
    }

    let status = backend.status(&ws_id).map_err(|e| anyhow::anyhow!("{e}"))?;
    let ws_path = backend.workspace_path(&ws_id);

    let patch_set = compute_patchset(&ws_path, &status.base_epoch).map_err(|e| {
        anyhow::anyhow!(
            "Failed to compute workspace changes for undo in '{}': {e}",
            ws_id.as_str()
        )
    })?;

    if patch_set.is_empty() {
        println!("No local changes to undo in workspace '{name}'.");
        println!("Next: maw ws touched {name} --format json");
        return Ok(());
    }

    let added_paths = collect_added_paths(&patch_set);
    restore_workspace_to_epoch(&ws_path, &status.base_epoch)?;
    remove_added_paths(&ws_path, &added_paths)?;

    // Sanity check: undo should leave no local delta against the base epoch.
    let remaining = compute_patchset(&ws_path, &status.base_epoch).map_err(|e| {
        anyhow::anyhow!(
            "Failed to verify workspace state after undo in '{}': {e}",
            ws_id.as_str()
        )
    })?;
    if !remaining.is_empty() {
        bail!(
            "Undo was incomplete for workspace '{name}'.\n  \
             Remaining changes: {} path(s)\n  \
             Next: maw ws touched {name} --format json",
            remaining.len()
        );
    }

    let root = repo_root()?;
    let op_oid = record_compensation_op(&root, &ws_id, &status.base_epoch, patch_set.len())?;

    println!("Undid local changes in workspace '{name}'.");
    println!(
        "  Reverted {} path(s) to base epoch {}.",
        patch_set.len(),
        &status.base_epoch.as_str()[..12]
    );
    println!("  Logged compensate op: {}", &op_oid.as_str()[..12]);
    println!("Next: maw ws touched {name} --format json");

    Ok(())
}

fn collect_added_paths(patch_set: &PatchSet) -> Vec<PathBuf> {
    patch_set
        .patches
        .iter()
        .filter_map(|(path, value)| match value {
            PatchValue::Add { .. } => Some(path.clone()),
            _ => None,
        })
        .collect()
}

fn restore_workspace_to_epoch(ws_path: &Path, base_epoch: &EpochId) -> Result<()> {
    let output = Command::new("git")
        .args([
            "restore",
            "--source",
            base_epoch.as_str(),
            "--staged",
            "--worktree",
            "--",
            ".",
        ])
        .current_dir(ws_path)
        .output()
        .context("Failed to run git restore for undo")?;

    if !output.status.success() {
        bail!(
            "Undo restore failed:\n  {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

fn remove_added_paths(ws_path: &Path, added_paths: &[PathBuf]) -> Result<()> {
    for rel_path in added_paths {
        ensure_safe_relative_path(rel_path)?;
        let full_path = ws_path.join(rel_path);

        if !full_path.exists() {
            continue;
        }

        let metadata = std::fs::symlink_metadata(&full_path)
            .with_context(|| format!("Failed to stat {}", full_path.display()))?;

        if metadata.file_type().is_dir() {
            std::fs::remove_dir_all(&full_path)
                .with_context(|| format!("Failed to remove {}", full_path.display()))?;
        } else {
            std::fs::remove_file(&full_path)
                .with_context(|| format!("Failed to remove {}", full_path.display()))?;
        }
    }

    Ok(())
}

fn ensure_safe_relative_path(path: &Path) -> Result<()> {
    if path.is_absolute() {
        bail!(
            "Unsafe absolute path in workspace patch: {}",
            path.display()
        );
    }

    if path
        .components()
        .any(|comp| matches!(comp, Component::ParentDir | Component::RootDir))
    {
        bail!(
            "Unsafe relative path in workspace patch: {}",
            path.display()
        );
    }

    Ok(())
}

fn record_compensation_op(
    root: &Path,
    ws_id: &WorkspaceId,
    base_epoch: &EpochId,
    reverted_paths: usize,
) -> Result<GitOid> {
    let head = ensure_workspace_oplog_head(root, ws_id, base_epoch)?;
    let target_op = latest_snapshot_or_head(root, ws_id, &head)?;

    let compensate = Operation {
        parent_ids: vec![head.clone()],
        workspace_id: ws_id.clone(),
        timestamp: super::now_timestamp_iso8601(),
        payload: OpPayload::Compensate {
            target_op,
            reason: format!("undo: reverted {reverted_paths} path(s) to base epoch"),
        },
    };

    append_operation_with_runtime_checkpoint(root, ws_id, &compensate, Some(&head))
        .context("Failed to append compensation operation")
}

fn ensure_workspace_oplog_head(
    root: &Path,
    ws_id: &WorkspaceId,
    base_epoch: &EpochId,
) -> Result<GitOid> {
    if let Some(head) = read_head(root, ws_id).context("Failed to read workspace op log head")? {
        return Ok(head);
    }

    let create_op = Operation {
        parent_ids: vec![],
        workspace_id: ws_id.clone(),
        timestamp: super::now_timestamp_iso8601(),
        payload: OpPayload::Create {
            epoch: base_epoch.clone(),
        },
    };

    append_operation_with_runtime_checkpoint(root, ws_id, &create_op, None)
        .context("Failed to bootstrap workspace op log for undo")
}

fn latest_snapshot_or_head(root: &Path, ws_id: &WorkspaceId, head: &GitOid) -> Result<GitOid> {
    let chain = walk_chain(root, ws_id, None, None)
        .context("Failed to read workspace operation chain for undo")?;

    Ok(chain
        .into_iter()
        .find_map(|(oid, op)| {
            if matches!(op.payload, OpPayload::Snapshot { .. }) {
                Some(oid)
            } else {
                None
            }
        })
        .unwrap_or_else(|| head.clone()))
}


