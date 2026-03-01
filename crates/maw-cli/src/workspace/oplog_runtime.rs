use std::path::Path;

use maw_core::model::types::{GitOid, WorkspaceId};
use maw_core::oplog::checkpoint::{
    DEFAULT_CHECKPOINT_INTERVAL, materialize_from_checkpoint, maybe_write_checkpoint,
};
use maw_core::oplog::types::Operation;
use maw_core::oplog::view::read_patch_set_blob;
use maw_core::oplog::write::{OpLogWriteError, append_operation};

pub(crate) fn append_operation_with_runtime_checkpoint(
    root: &Path,
    workspace_id: &WorkspaceId,
    op: &Operation,
    old_head: Option<&GitOid>,
) -> Result<GitOid, OpLogWriteError> {
    let new_head = append_operation(root, workspace_id, op, old_head)?;

    if let Err(err) = maybe_checkpoint_after_append(root, workspace_id, &new_head) {
        eprintln!(
            "WARNING: checkpoint write skipped for workspace '{}': {err}",
            workspace_id.as_str()
        );
    }

    Ok(new_head)
}

fn maybe_checkpoint_after_append(
    root: &Path,
    workspace_id: &WorkspaceId,
    trigger_oid: &GitOid,
) -> Result<(), String> {
    let view =
        materialize_from_checkpoint(root, workspace_id, |oid| read_patch_set_blob(root, oid))
            .map_err(|e| e.to_string())?;

    let _ = maybe_write_checkpoint(
        root,
        workspace_id,
        &view,
        trigger_oid,
        trigger_oid,
        DEFAULT_CHECKPOINT_INTERVAL,
    )
    .map_err(|e| e.to_string())?;

    Ok(())
}
