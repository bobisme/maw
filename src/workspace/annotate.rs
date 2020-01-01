use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::backend::WorkspaceBackend;

use crate::model::types::{EpochId, GitOid, WorkspaceId};
use crate::oplog::read::read_head;
use crate::oplog::types::{OpPayload, Operation};

use super::{get_backend, oplog_runtime::append_operation_with_runtime_checkpoint, repo_root};

/// Annotate a workspace with structured metadata.
///
/// Records an `Annotate` operation in the workspace's operation log. The metadata
/// is a key-value pair where the value is an arbitrary JSON object. Annotations
/// are visible in `maw ws history --format json` output.
///
/// # Arguments
/// - `name`: The workspace name to annotate
/// - `key`: The annotation key (e.g., "test-results", "review-status")
/// - `json_value`: The annotation value as a JSON string (must be an object)
pub fn annotate(name: &str, key: &str, json_value: &str) -> Result<()> {
    let ws_id = WorkspaceId::new(name)
        .map_err(|e| anyhow::anyhow!("invalid workspace name '{name}': {e}"))?;

    let backend = get_backend()?;
    if !backend.exists(&ws_id) {
        bail!(
            "Workspace '{name}' does not exist\n  Check: maw ws list\n  Next: maw ws annotate <workspace> <key> <json-value>"
        );
    }

    if key.is_empty() {
        bail!("Annotation key cannot be empty");
    }

    // Parse the JSON value into a BTreeMap<String, serde_json::Value>
    let data: BTreeMap<String, serde_json::Value> =
        serde_json::from_str(json_value).with_context(|| {
            format!("Failed to parse annotation value as JSON object: {json_value}")
        })?;

    let root = repo_root()?;
    let status = backend.status(&ws_id).map_err(|e| anyhow::anyhow!("{e}"))?;

    // Ensure workspace has an oplog head.
    let head = ensure_workspace_oplog_head(&root, &ws_id, &status.base_epoch)
        .context("Failed to initialize workspace oplog")?;

    // Create the Annotate operation
    let annotate_op = Operation {
        parent_ids: vec![head.clone()],
        workspace_id: ws_id.clone(),
        timestamp: super::now_timestamp_iso8601(),
        payload: OpPayload::Annotate {
            key: key.to_string(),
            data,
        },
    };

    let op_oid = append_operation_with_runtime_checkpoint(&root, &ws_id, &annotate_op, Some(&head))
        .context("Failed to append annotate operation")?;

    println!("Annotated workspace '{name}' with key '{key}':");
    println!("  Op: {}", &op_oid.as_str()[..12]);
    println!("Next: maw ws history {name} --format json");

    Ok(())
}

/// Ensure the workspace oplog has at least one head entry.
///
/// If no head exists, creates a bootstrap `Create` operation so that subsequent
/// operations have a valid parent.
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
        .context("Failed to bootstrap workspace op log for annotate")
}
