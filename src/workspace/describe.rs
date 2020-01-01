use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::backend::WorkspaceBackend;
use crate::model::types::{GitOid, WorkspaceId};
use crate::oplog::read::read_head;
use crate::oplog::types::{OpPayload, Operation};

use super::{get_backend, oplog_runtime::append_operation_with_runtime_checkpoint, repo_root};

/// Describe (label) the current state of a workspace with a human-readable message.
///
/// Records a `Describe` operation in the workspace's operation log. The message
/// is visible in `maw ws history` output and serves as a checkpoint marker
/// for tracking workspace progress and intent.
///
/// # Arguments
/// - `name`: The workspace name to describe
/// - `message`: The description text (e.g., "wip: implementing auth", "ready for review")
///
/// # Example
/// ```bash
/// maw ws describe alice "wip: implementing user authentication module"
/// maw ws history alice
/// ```
pub fn describe(name: &str, message: &str) -> Result<()> {
    let ws_id = WorkspaceId::new(name)
        .map_err(|e| anyhow::anyhow!("invalid workspace name '{name}': {e}"))?;

    let backend = get_backend()?;
    if !backend.exists(&ws_id) {
        bail!(
            "Workspace '{name}' does not exist\n  Check: maw ws list\n  Next: maw ws describe {name} \"<message>\""
        );
    }

    if message.is_empty() {
        bail!("Description message cannot be empty");
    }

    let root = repo_root()?;
    let status = backend.status(&ws_id).map_err(|e| anyhow::anyhow!("{e}"))?;

    // Ensure workspace has an oplog head. If not, bootstrap with a Create op.
    let head = ensure_workspace_oplog_head(&root, &ws_id, &status.base_epoch)
        .context("Failed to initialize workspace oplog")?;

    // Create the Describe operation
    let describe_op = Operation {
        parent_ids: vec![head.clone()],
        workspace_id: ws_id.clone(),
        timestamp: super::now_timestamp_iso8601(),
        payload: OpPayload::Describe {
            message: message.to_string(),
        },
    };

    let op_oid = append_operation_with_runtime_checkpoint(&root, &ws_id, &describe_op, Some(&head))
        .context("Failed to append describe operation")?;

    println!("Described workspace '{name}':");
    println!("  {}", message);
    println!();
    println!("  Op: {}", &op_oid.as_str()[..12]);
    println!("Next: maw ws history {name}");

    Ok(())
}

fn ensure_workspace_oplog_head(
    root: &Path,
    ws_id: &WorkspaceId,
    base_epoch: &crate::model::types::EpochId,
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
        .context("Failed to bootstrap workspace op log for describe")
}
