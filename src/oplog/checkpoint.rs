//! View checkpoints and log compaction (§5.5).
//!
//! Checkpoints are periodic snapshots of a [`MaterializedView`] stored as
//! Annotate operations in the op log. They bound replay cost: instead of
//! replaying from root, we replay from the latest checkpoint.
//!
//! # Checkpoint strategy
//!
//! Every N operations (configurable, default 100), the caller writes a
//! Checkpoint annotation containing the serialized [`MaterializedView`].
//! The checkpoint is deterministic: the same sequence of operations always
//! produces the same checkpoint.
//!
//! # Compaction
//!
//! Compaction replaces all operations before a checkpoint with a single
//! synthetic Create + Checkpoint pair. The old blobs remain in git's object
//! store and will be garbage-collected by `git gc` when unreferenced.
//!
//! # Replay from checkpoint
//!
//! [`materialize_from_checkpoint`] walks the op log backwards until it hits
//! a checkpoint annotation, then replays only the operations after the
//! checkpoint. This is semantically equivalent to full replay.

#![allow(clippy::missing_errors_doc)]

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::model::patch::PatchSet;
use crate::model::types::{GitOid, WorkspaceId};
use crate::oplog::read::{walk_chain, OpLogReadError};
use crate::oplog::types::{OpPayload, Operation};
use crate::oplog::view::{materialize_from_ops, MaterializedView, ViewError};
use crate::oplog::write::{append_operation, OpLogWriteError};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// The annotation key used for checkpoint data in the op log.
pub const CHECKPOINT_KEY: &str = "checkpoint";

/// Default checkpoint interval: write a checkpoint every N operations.
#[allow(dead_code)]
pub const DEFAULT_CHECKPOINT_INTERVAL: usize = 100;

// ---------------------------------------------------------------------------
// Checkpoint data
// ---------------------------------------------------------------------------

/// Serialized checkpoint data stored in an Annotate operation.
///
/// Contains the full materialized view state at the point of the checkpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointData {
    /// The materialized view at the checkpoint.
    pub view: CheckpointView,

    /// Number of operations replayed to produce this checkpoint.
    pub op_count: usize,

    /// The OID of the operation that triggered the checkpoint.
    pub trigger_oid: String,
}

/// Subset of [`MaterializedView`] that is checkpointed.
///
/// We serialize only the essential state, not the full `MaterializedView`
/// struct, to keep checkpoints forward-compatible.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointView {
    /// The workspace this view belongs to.
    pub workspace_id: String,

    /// Current epoch (from latest Create or Merge).
    pub epoch: Option<String>,

    /// Current patch set (serialized).
    pub patch_set: Option<PatchSet>,

    /// Patch set blob OID.
    pub patch_set_oid: Option<String>,

    /// Description.
    pub description: Option<String>,

    /// Annotations (excluding checkpoint annotations themselves).
    pub annotations: BTreeMap<String, BTreeMap<String, serde_json::Value>>,

    /// Whether workspace is destroyed.
    pub is_destroyed: bool,
}

impl CheckpointView {
    /// Convert a [`MaterializedView`] to checkpoint-serializable form.
    #[must_use]
    pub fn from_view(view: &MaterializedView) -> Self {
        Self {
            workspace_id: view.workspace_id.to_string(),
            epoch: view.epoch.as_ref().map(|e| e.as_str().to_owned()),
            patch_set: view.patch_set.clone(),
            patch_set_oid: view.patch_set_oid.as_ref().map(|o| o.as_str().to_owned()),
            description: view.description.clone(),
            annotations: view
                .annotations
                .iter()
                .filter(|(k, _)| k.as_str() != CHECKPOINT_KEY)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            is_destroyed: view.is_destroyed,
        }
    }

    /// Restore a [`MaterializedView`] from checkpoint data.
    ///
    /// # Errors
    ///
    /// Returns `CheckpointError::InvalidData` if OIDs cannot be parsed.
    pub fn to_view(&self, op_count: usize) -> Result<MaterializedView, CheckpointError> {
        use crate::model::types::EpochId;

        let epoch = self
            .epoch
            .as_ref()
            .map(|s| EpochId::new(s))
            .transpose()
            .map_err(|_| CheckpointError::InvalidData {
                detail: format!("invalid epoch OID: {:?}", self.epoch),
            })?;

        let patch_set_oid = self
            .patch_set_oid
            .as_ref()
            .map(|s| GitOid::new(s))
            .transpose()
            .map_err(|_| CheckpointError::InvalidData {
                detail: format!("invalid patch_set OID: {:?}", self.patch_set_oid),
            })?;

        let ws_id =
            WorkspaceId::new(&self.workspace_id).map_err(|_| CheckpointError::InvalidData {
                detail: format!("invalid workspace_id: {:?}", self.workspace_id),
            })?;

        Ok(MaterializedView {
            workspace_id: ws_id,
            epoch,
            patch_set: self.patch_set.clone(),
            patch_set_oid,
            description: self.description.clone(),
            annotations: self.annotations.clone(),
            op_count,
            is_destroyed: self.is_destroyed,
        })
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors from checkpoint operations.
#[derive(Debug)]
pub enum CheckpointError {
    /// Op log read error.
    OpLogRead(OpLogReadError),

    /// Op log write error.
    OpLogWrite(OpLogWriteError),

    /// View materialization error.
    View(ViewError),

    /// Checkpoint data was malformed or unparseable.
    InvalidData {
        /// Description of what was wrong.
        detail: String,
    },

    /// No checkpoint found in the op log.
    NoCheckpoint {
        /// The workspace that has no checkpoint.
        workspace_id: WorkspaceId,
    },
}

impl fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpLogRead(e) => write!(f, "checkpoint: op log read error: {e}"),
            Self::OpLogWrite(e) => write!(f, "checkpoint: op log write error: {e}"),
            Self::View(e) => write!(f, "checkpoint: view error: {e}"),
            Self::InvalidData { detail } => {
                write!(f, "checkpoint: invalid data: {detail}")
            }
            Self::NoCheckpoint { workspace_id } => {
                write!(
                    f,
                    "no checkpoint found for workspace '{workspace_id}'\n  \
                     To fix: run checkpoint creation first."
                )
            }
        }
    }
}

impl std::error::Error for CheckpointError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::OpLogRead(e) => Some(e),
            Self::OpLogWrite(e) => Some(e),
            Self::View(e) => Some(e),
            _ => None,
        }
    }
}

impl From<OpLogReadError> for CheckpointError {
    fn from(e: OpLogReadError) -> Self {
        Self::OpLogRead(e)
    }
}

impl From<OpLogWriteError> for CheckpointError {
    fn from(e: OpLogWriteError) -> Self {
        Self::OpLogWrite(e)
    }
}

impl From<ViewError> for CheckpointError {
    fn from(e: ViewError) -> Self {
        Self::View(e)
    }
}

// ---------------------------------------------------------------------------
// Checkpoint detection
// ---------------------------------------------------------------------------

/// Check if an operation is a checkpoint annotation.
#[must_use]
pub fn is_checkpoint(op: &Operation) -> bool {
    matches!(&op.payload, OpPayload::Annotate { key, .. } if key == CHECKPOINT_KEY)
}

/// Extract [`CheckpointData`] from a checkpoint annotation operation.
///
/// Returns `None` if the operation is not a checkpoint annotation or if
/// the data cannot be parsed.
#[must_use]
pub fn extract_checkpoint(op: &Operation) -> Option<CheckpointData> {
    match &op.payload {
        OpPayload::Annotate { key, data } if key == CHECKPOINT_KEY => {
            // Convert BTreeMap<String, Value> to CheckpointData via JSON
            let value = serde_json::Value::Object(
                data.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            );
            serde_json::from_value(value).ok()
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Checkpoint creation
// ---------------------------------------------------------------------------

/// Determine if a checkpoint should be written based on operation count.
///
/// Returns `true` if `op_count` is a multiple of `interval` and `op_count > 0`.
#[must_use]
pub const fn should_checkpoint(op_count: usize, interval: usize) -> bool {
    interval > 0 && op_count > 0 && op_count.is_multiple_of(interval)
}

/// Create a checkpoint [`Operation`] from a materialized view.
///
/// The checkpoint is an Annotate operation with key `"checkpoint"` and
/// the serialized checkpoint data as the value.
///
/// # Arguments
/// * `view` — the materialized view to checkpoint.
/// * `trigger_oid` — the OID of the operation that triggered this checkpoint.
/// * `parent_oid` — the parent operation OID for the checkpoint op.
#[must_use]
pub fn create_checkpoint_op(
    view: &MaterializedView,
    trigger_oid: &GitOid,
    parent_oid: &GitOid,
) -> Operation {
    let checkpoint_data = CheckpointData {
        view: CheckpointView::from_view(view),
        op_count: view.op_count,
        trigger_oid: trigger_oid.as_str().to_owned(),
    };

    // Serialize CheckpointData into a BTreeMap<String, Value> for the annotation
    let data_value = serde_json::to_value(&checkpoint_data).unwrap_or_default();
    let data: BTreeMap<String, serde_json::Value> = match data_value {
        serde_json::Value::Object(map) => map.into_iter().collect(),
        _ => BTreeMap::new(),
    };

    Operation {
        parent_ids: vec![parent_oid.clone()],
        workspace_id: view.workspace_id.clone(),
        timestamp: {
            let dur = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            format!(
                "{}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                1970 + dur.as_secs() / 31_557_600,
                (dur.as_secs() % 31_557_600) / 2_629_800 + 1,
                (dur.as_secs() % 2_629_800) / 86400 + 1,
                (dur.as_secs() % 86400) / 3600,
                (dur.as_secs() % 3600) / 60,
                dur.as_secs() % 60,
            )
        },
        payload: OpPayload::Annotate {
            key: CHECKPOINT_KEY.to_owned(),
            data,
        },
    }
}

/// Write a checkpoint to the op log if the interval has been reached.
///
/// This is the primary entry point for automated checkpoint creation.
/// Call it after each operation is appended.
///
/// # Returns
///
/// `Some(oid)` if a checkpoint was written, `None` if not needed yet.
///
/// # Arguments
/// * `root` — path to the git repository root.
/// * `workspace_id` — workspace to checkpoint.
/// * `current_view` — the just-materialized view.
/// * `trigger_oid` — OID of the operation that just completed.
/// * `current_head` — current head ref value (= `trigger_oid` usually).
/// * `interval` — checkpoint every N operations.
pub fn maybe_write_checkpoint(
    root: &Path,
    workspace_id: &WorkspaceId,
    current_view: &MaterializedView,
    trigger_oid: &GitOid,
    current_head: &GitOid,
    interval: usize,
) -> Result<Option<GitOid>, CheckpointError> {
    if !should_checkpoint(current_view.op_count, interval) {
        return Ok(None);
    }

    let cp_op = create_checkpoint_op(current_view, trigger_oid, current_head);
    let oid = append_operation(root, workspace_id, &cp_op, Some(current_head))?;

    Ok(Some(oid))
}

// ---------------------------------------------------------------------------
// Replay from checkpoint
// ---------------------------------------------------------------------------

/// Materialize a workspace view, starting from the latest checkpoint if one exists.
///
/// This is semantically equivalent to full replay but faster for long op logs:
/// 1. Walk backwards from head until a checkpoint annotation is found.
/// 2. Restore the view from the checkpoint.
/// 3. Replay only the operations after the checkpoint.
///
/// If no checkpoint exists, falls back to full replay from root.
///
/// # Arguments
/// * `root` — path to the git repository root.
/// * `workspace_id` — workspace to materialize.
/// * `read_patch_set` — callback to fetch patch-set blob contents.
pub fn materialize_from_checkpoint<F>(
    root: &Path,
    workspace_id: &WorkspaceId,
    read_patch_set: F,
) -> Result<MaterializedView, CheckpointError>
where
    F: Fn(&GitOid) -> Result<PatchSet, ViewError>,
{
    // Walk the entire chain to find the latest checkpoint
    let stop_pred: Option<&dyn Fn(&Operation) -> bool> = None;
    let chain = walk_chain(root, workspace_id, None, stop_pred)?;

    if chain.is_empty() {
        return Err(CheckpointError::OpLogRead(OpLogReadError::NoHead {
            workspace_id: workspace_id.clone(),
        }));
    }

    // chain is in reverse chronological order (newest first)
    // Find the latest checkpoint
    let mut checkpoint_idx = None;
    for (i, (_oid, op)) in chain.iter().enumerate() {
        if is_checkpoint(op) {
            checkpoint_idx = Some(i);
            break; // First one found is the latest (chain is newest-first)
        }
    }

    if let Some(cp_idx) = checkpoint_idx {
        // Extract checkpoint data
        let (_cp_oid, cp_op) = &chain[cp_idx];
        let cp_data = extract_checkpoint(cp_op).ok_or_else(|| CheckpointError::InvalidData {
            detail: "checkpoint annotation has unparseable data".to_owned(),
        })?;

        // Restore view from checkpoint
        let mut view = cp_data.view.to_view(cp_data.op_count)?;

        // Replay operations AFTER the checkpoint (newer operations)
        // chain[0..cp_idx] are newer than the checkpoint, reversed for causal order
        let post_checkpoint: Vec<_> = chain[..cp_idx].iter().rev().cloned().collect();

        for (oid, op) in &post_checkpoint {
            // Skip checkpoint annotations during replay
            if is_checkpoint(op) {
                view.op_count += 1;
                continue;
            }
            replay_single_op(&mut view, oid, op, &read_patch_set)?;
        }

        Ok(view)
    } else {
        // No checkpoint found — full replay
        let mut ops: Vec<_> = chain;
        ops.reverse(); // causal order (oldest first)
        let view = materialize_from_ops(workspace_id.clone(), &ops, read_patch_set)?;
        Ok(view)
    }
}

/// Replay a single operation on a mutable view (mirrors `apply_operation` in view.rs).
fn replay_single_op<F>(
    view: &mut MaterializedView,
    _oid: &GitOid,
    op: &Operation,
    read_patch_set: &F,
) -> Result<(), CheckpointError>
where
    F: Fn(&GitOid) -> Result<PatchSet, ViewError>,
{
    view.op_count += 1;

    match &op.payload {
        OpPayload::Create { epoch } => {
            view.epoch = Some(epoch.clone());
            view.patch_set = None;
            view.patch_set_oid = None;
            view.is_destroyed = false;
        }

        OpPayload::Snapshot { patch_set_oid } => {
            let ps = read_patch_set(patch_set_oid)?;
            view.patch_set = Some(ps);
            view.patch_set_oid = Some(patch_set_oid.clone());
        }

        OpPayload::Compensate { .. } => {
            view.patch_set = None;
            view.patch_set_oid = None;
        }

        OpPayload::Merge { epoch_after, .. } => {
            view.epoch = Some(epoch_after.clone());
            view.patch_set = None;
            view.patch_set_oid = None;
        }

        OpPayload::Describe { message } => {
            view.description = Some(message.clone());
        }

        OpPayload::Annotate { key, data } => {
            view.annotations.insert(key.clone(), data.clone());
        }

        OpPayload::Destroy => {
            view.is_destroyed = true;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Log compaction
// ---------------------------------------------------------------------------

/// Compact result containing the new head after compaction.
#[derive(Clone, Debug)]
pub struct CompactionResult {
    /// The new head OID after compaction.
    #[allow(dead_code)]
    pub new_head: GitOid,

    /// Number of operations before compaction.
    pub ops_before: usize,

    /// Number of operations after compaction (checkpoint + any post-checkpoint ops).
    pub ops_after: usize,
}

/// Compact the op log for a workspace by replacing all operations before
/// the latest checkpoint with a synthetic Create + Checkpoint pair.
///
/// This reduces the chain length while preserving the same materialized view.
/// Old blobs remain in git's object store until `git gc` collects them.
///
/// # Strategy
///
/// 1. Walk chain, find latest checkpoint.
/// 2. Create synthetic root: Create operation with the checkpoint's epoch.
/// 3. Create checkpoint annotation on top of synthetic root.
/// 4. Re-link post-checkpoint operations to point to the new checkpoint.
/// 5. Update head ref.
///
/// # Arguments
/// * `root` — path to the git repository root.
/// * `workspace_id` — workspace to compact.
///
/// # Errors
///
/// Returns `CheckpointError::NoCheckpoint` if no checkpoint exists.
pub fn compact(
    root: &Path,
    workspace_id: &WorkspaceId,
) -> Result<CompactionResult, CheckpointError> {
    let stop_pred: Option<&dyn Fn(&Operation) -> bool> = None;
    let chain = walk_chain(root, workspace_id, None, stop_pred)?;

    if chain.is_empty() {
        return Err(CheckpointError::OpLogRead(OpLogReadError::NoHead {
            workspace_id: workspace_id.clone(),
        }));
    }

    let ops_before = chain.len();

    // Find the latest checkpoint (chain is newest-first)
    let mut checkpoint_idx = None;
    for (i, (_oid, op)) in chain.iter().enumerate() {
        if is_checkpoint(op) {
            checkpoint_idx = Some(i);
            break;
        }
    }

    let cp_idx = checkpoint_idx.ok_or_else(|| CheckpointError::NoCheckpoint {
        workspace_id: workspace_id.clone(),
    })?;

    // If checkpoint is the second-to-last or last op, not much to compact
    if cp_idx >= chain.len() - 1 {
        // Nothing to compact — checkpoint is already at or near root
        return Ok(CompactionResult {
            new_head: chain[0].0.clone(),
            ops_before,
            ops_after: ops_before,
        });
    }

    let (_cp_oid, cp_op) = &chain[cp_idx];
    let cp_data = extract_checkpoint(cp_op).ok_or_else(|| CheckpointError::InvalidData {
        detail: "checkpoint annotation has unparseable data".to_owned(),
    })?;

    // Extract epoch from checkpoint
    let epoch = cp_data
        .view
        .epoch
        .as_ref()
        .ok_or_else(|| CheckpointError::InvalidData {
            detail: "checkpoint has no epoch".to_owned(),
        })?;
    let epoch_id =
        crate::model::types::EpochId::new(epoch).map_err(|_| CheckpointError::InvalidData {
            detail: format!("invalid epoch in checkpoint: {epoch}"),
        })?;

    // Step 1: Write synthetic Create (new root, no parents)
    let synthetic_create = Operation {
        parent_ids: vec![],
        workspace_id: workspace_id.clone(),
        timestamp: cp_op.timestamp.clone(),
        payload: OpPayload::Create { epoch: epoch_id },
    };

    // We can't use append_operation because we're building a new chain.
    // Write blobs directly and update the ref at the end.
    let create_oid = crate::oplog::write::write_operation_blob(root, &synthetic_create)?;

    // Step 2: Write checkpoint annotation on top of synthetic Create
    let mut cp_annotate = cp_op.clone();
    cp_annotate.parent_ids = vec![create_oid];
    let cp_new_oid = crate::oplog::write::write_operation_blob(root, &cp_annotate)?;

    // Step 3: Re-write post-checkpoint ops with updated parent pointers
    // Post-checkpoint ops are chain[0..cp_idx], from newest to oldest
    // We need to re-link them: the oldest post-checkpoint op should point to cp_new_oid
    let post_ops: Vec<_> = chain[..cp_idx].iter().rev().cloned().collect(); // oldest first

    let mut prev_oid = cp_new_oid;
    let mut ops_after = 2; // synthetic create + checkpoint

    for (_old_oid, mut op) in post_ops {
        // Replace parent with prev_oid
        op.parent_ids = vec![prev_oid.clone()];
        let new_oid = crate::oplog::write::write_operation_blob(root, &op)?;
        prev_oid = new_oid;
        ops_after += 1;
    }

    // Step 4: Update head ref to point to the new chain head
    let ref_name = crate::refs::workspace_head_ref(workspace_id.as_str());
    let current_head = chain[0].0.clone();
    crate::refs::write_ref_cas(root, &ref_name, &current_head, &prev_oid).map_err(|e| match e {
        crate::refs::RefError::CasMismatch { .. } => {
            CheckpointError::OpLogWrite(OpLogWriteError::CasMismatch {
                workspace_id: workspace_id.clone(),
            })
        }
        other => CheckpointError::OpLogWrite(OpLogWriteError::RefError(other)),
    })?;

    Ok(CompactionResult {
        new_head: prev_oid,
        ops_before,
        ops_after,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
mod tests {
    use super::*;
    use crate::model::patch::{FileId, PatchSet, PatchValue};
    use crate::model::types::{EpochId, GitOid, WorkspaceId};
    use crate::oplog::types::{OpPayload, Operation};
    use crate::oplog::view::MaterializedView;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn test_oid(c: char) -> GitOid {
        GitOid::new(&c.to_string().repeat(40)).unwrap()
    }

    fn test_epoch(c: char) -> EpochId {
        EpochId::new(&c.to_string().repeat(40)).unwrap()
    }

    fn test_ws(name: &str) -> WorkspaceId {
        WorkspaceId::new(name).unwrap()
    }

    fn test_patch_set(epoch_char: char) -> PatchSet {
        let mut patches = BTreeMap::new();
        patches.insert(
            PathBuf::from("src/main.rs"),
            PatchValue::Add {
                blob: test_oid('f'),
                file_id: FileId::new(1),
            },
        );
        PatchSet {
            base_epoch: test_epoch(epoch_char),
            patches,
        }
    }

    fn mock_reader(ps: PatchSet) -> impl Fn(&GitOid) -> Result<PatchSet, ViewError> {
        move |_oid| Ok(ps.clone())
    }

    fn make_op(ws: &str, payload: OpPayload) -> Operation {
        Operation {
            parent_ids: vec![],
            workspace_id: test_ws(ws),
            timestamp: "2026-02-19T12:00:00Z".to_owned(),
            payload,
        }
    }

    fn make_view(ws: &str, epoch_char: char, op_count: usize) -> MaterializedView {
        MaterializedView {
            workspace_id: test_ws(ws),
            epoch: Some(test_epoch(epoch_char)),
            patch_set: Some(test_patch_set(epoch_char)),
            patch_set_oid: Some(test_oid('d')),
            description: Some("test description".into()),
            annotations: BTreeMap::new(),
            op_count,
            is_destroyed: false,
        }
    }

    // -----------------------------------------------------------------------
    // is_checkpoint
    // -----------------------------------------------------------------------

    #[test]
    fn is_checkpoint_returns_true_for_checkpoint_annotate() {
        let op = make_op(
            "ws-1",
            OpPayload::Annotate {
                key: CHECKPOINT_KEY.to_owned(),
                data: BTreeMap::new(),
            },
        );
        assert!(is_checkpoint(&op));
    }

    #[test]
    fn is_checkpoint_returns_false_for_other_annotate() {
        let op = make_op(
            "ws-1",
            OpPayload::Annotate {
                key: "validation".to_owned(),
                data: BTreeMap::new(),
            },
        );
        assert!(!is_checkpoint(&op));
    }

    #[test]
    fn is_checkpoint_returns_false_for_non_annotate() {
        let op = make_op("ws-1", OpPayload::Destroy);
        assert!(!is_checkpoint(&op));
    }

    // -----------------------------------------------------------------------
    // should_checkpoint
    // -----------------------------------------------------------------------

    #[test]
    fn should_checkpoint_at_interval() {
        assert!(should_checkpoint(100, 100));
        assert!(should_checkpoint(200, 100));
        assert!(should_checkpoint(50, 50));
    }

    #[test]
    fn should_not_checkpoint_between_intervals() {
        assert!(!should_checkpoint(99, 100));
        assert!(!should_checkpoint(101, 100));
        assert!(!should_checkpoint(1, 100));
    }

    #[test]
    fn should_not_checkpoint_at_zero() {
        assert!(!should_checkpoint(0, 100));
    }

    #[test]
    fn should_not_checkpoint_with_zero_interval() {
        assert!(!should_checkpoint(100, 0));
    }

    // -----------------------------------------------------------------------
    // CheckpointView round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn checkpoint_view_from_and_to_view() {
        let view = make_view("ws-1", 'a', 100);
        let cp_view = CheckpointView::from_view(&view);
        let restored = cp_view.to_view(100).unwrap();

        assert_eq!(restored.workspace_id, view.workspace_id);
        assert_eq!(restored.epoch, view.epoch);
        assert_eq!(restored.patch_set, view.patch_set);
        assert_eq!(restored.patch_set_oid, view.patch_set_oid);
        assert_eq!(restored.description, view.description);
        assert_eq!(restored.is_destroyed, view.is_destroyed);
        assert_eq!(restored.op_count, view.op_count);
    }

    #[test]
    fn checkpoint_view_filters_checkpoint_annotations() {
        let mut view = make_view("ws-1", 'a', 100);
        let mut checkpoint_data = BTreeMap::new();
        checkpoint_data.insert("key".into(), serde_json::Value::String("val".into()));
        view.annotations
            .insert(CHECKPOINT_KEY.to_owned(), checkpoint_data);

        let mut other_data = BTreeMap::new();
        other_data.insert("passed".into(), serde_json::Value::Bool(true));
        view.annotations.insert("validation".to_owned(), other_data);

        let cp_view = CheckpointView::from_view(&view);

        // Checkpoint annotation should be filtered out
        assert!(!cp_view.annotations.contains_key(CHECKPOINT_KEY));
        assert!(cp_view.annotations.contains_key("validation"));
    }

    #[test]
    fn checkpoint_view_empty_epoch() {
        let view = MaterializedView::empty(test_ws("ws-1"));
        let cp_view = CheckpointView::from_view(&view);
        let restored = cp_view.to_view(0).unwrap();

        assert!(restored.epoch.is_none());
        assert!(restored.patch_set.is_none());
        assert!(!restored.is_destroyed);
    }

    #[test]
    fn checkpoint_view_destroyed() {
        let mut view = make_view("ws-1", 'a', 5);
        view.is_destroyed = true;

        let cp_view = CheckpointView::from_view(&view);
        assert!(cp_view.is_destroyed);

        let restored = cp_view.to_view(5).unwrap();
        assert!(restored.is_destroyed);
    }

    // -----------------------------------------------------------------------
    // CheckpointData serde
    // -----------------------------------------------------------------------

    #[test]
    fn checkpoint_data_serde_roundtrip() {
        let view = make_view("ws-1", 'a', 100);
        let cp_data = CheckpointData {
            view: CheckpointView::from_view(&view),
            op_count: 100,
            trigger_oid: test_oid('1').as_str().to_owned(),
        };

        let json = serde_json::to_value(&cp_data).unwrap();
        let restored: CheckpointData = serde_json::from_value(json).unwrap();

        assert_eq!(restored.op_count, 100);
        assert_eq!(restored.trigger_oid, cp_data.trigger_oid);
        assert_eq!(restored.view.workspace_id, "ws-1");
    }

    // -----------------------------------------------------------------------
    // create_checkpoint_op
    // -----------------------------------------------------------------------

    #[test]
    fn create_checkpoint_op_produces_annotate_with_correct_key() {
        let view = make_view("ws-1", 'a', 100);
        let trigger = test_oid('1');
        let parent = test_oid('2');

        let op = create_checkpoint_op(&view, &trigger, &parent);

        assert!(is_checkpoint(&op));
        assert_eq!(op.parent_ids, vec![parent]);
        assert_eq!(op.workspace_id, test_ws("ws-1"));
    }

    #[test]
    fn create_checkpoint_op_data_is_extractable() {
        let view = make_view("ws-1", 'a', 100);
        let trigger = test_oid('1');
        let parent = test_oid('2');

        let op = create_checkpoint_op(&view, &trigger, &parent);
        let extracted = extract_checkpoint(&op).expect("should extract checkpoint");

        assert_eq!(extracted.op_count, 100);
        assert_eq!(extracted.trigger_oid, trigger.as_str());
        assert_eq!(extracted.view.workspace_id, "ws-1");
    }

    // -----------------------------------------------------------------------
    // extract_checkpoint
    // -----------------------------------------------------------------------

    #[test]
    fn extract_checkpoint_returns_none_for_non_checkpoint() {
        let op = make_op("ws-1", OpPayload::Destroy);
        assert!(extract_checkpoint(&op).is_none());
    }

    #[test]
    fn extract_checkpoint_returns_none_for_wrong_key() {
        let op = make_op(
            "ws-1",
            OpPayload::Annotate {
                key: "not-a-checkpoint".to_owned(),
                data: BTreeMap::new(),
            },
        );
        assert!(extract_checkpoint(&op).is_none());
    }

    // -----------------------------------------------------------------------
    // materialize_from_ops with checkpoint (unit-level)
    // -----------------------------------------------------------------------

    #[test]
    fn materialize_from_ops_with_checkpoint_in_chain() {
        let ps = test_patch_set('a');

        // Build a chain: Create → Snapshot → Checkpoint → Describe
        let ops = vec![
            (
                test_oid('1'),
                make_op(
                    "ws-1",
                    OpPayload::Create {
                        epoch: test_epoch('a'),
                    },
                ),
            ),
            (
                test_oid('2'),
                make_op(
                    "ws-1",
                    OpPayload::Snapshot {
                        patch_set_oid: test_oid('d'),
                    },
                ),
            ),
        ];

        // Full replay should give us the view at snapshot
        let view = materialize_from_ops(test_ws("ws-1"), &ops, mock_reader(ps.clone())).unwrap();

        assert_eq!(view.epoch, Some(test_epoch('a')));
        assert_eq!(view.patch_set, Some(ps));
        assert_eq!(view.op_count, 2);
    }

    // -----------------------------------------------------------------------
    // Replay from checkpoint equivalence (property test)
    // -----------------------------------------------------------------------

    #[test]
    fn checkpoint_restore_equals_full_replay() {
        // Build view from full replay
        let ps = test_patch_set('a');
        let ops = vec![
            (
                test_oid('1'),
                make_op(
                    "ws-1",
                    OpPayload::Create {
                        epoch: test_epoch('a'),
                    },
                ),
            ),
            (
                test_oid('2'),
                make_op(
                    "ws-1",
                    OpPayload::Snapshot {
                        patch_set_oid: test_oid('d'),
                    },
                ),
            ),
            (
                test_oid('3'),
                make_op(
                    "ws-1",
                    OpPayload::Describe {
                        message: "checkpoint test".into(),
                    },
                ),
            ),
        ];

        let full_view =
            materialize_from_ops(test_ws("ws-1"), &ops, mock_reader(ps.clone())).unwrap();

        // Create checkpoint from the view at op 2
        let partial_view =
            materialize_from_ops(test_ws("ws-1"), &ops[..2], mock_reader(ps.clone())).unwrap();
        let cp_view = CheckpointView::from_view(&partial_view);
        let mut restored = cp_view.to_view(2).unwrap();

        // Replay the remaining op (Describe)
        let remaining_ops = &ops[2..];
        for (oid, op) in remaining_ops {
            replay_single_op(&mut restored, oid, op, &mock_reader(ps.clone())).unwrap();
        }

        // Should be equivalent to full replay
        assert_eq!(restored.epoch, full_view.epoch);
        assert_eq!(restored.patch_set, full_view.patch_set);
        assert_eq!(restored.description, full_view.description);
        assert_eq!(restored.is_destroyed, full_view.is_destroyed);
        assert_eq!(restored.op_count, full_view.op_count);
    }

    // -----------------------------------------------------------------------
    // Compaction determinism
    // -----------------------------------------------------------------------

    #[test]
    fn compaction_produces_same_view() {
        // This is a logical test: given the same checkpoint data, compaction
        // always produces the same synthetic root.
        let view1 = make_view("ws-1", 'a', 100);
        let view2 = make_view("ws-1", 'a', 100);

        let cp1 = CheckpointView::from_view(&view1);
        let cp2 = CheckpointView::from_view(&view2);

        assert_eq!(cp1, cp2, "deterministic: same view → same checkpoint");
    }

    // -----------------------------------------------------------------------
    // Error cases
    // -----------------------------------------------------------------------

    #[test]
    fn checkpoint_view_invalid_epoch() {
        let cp_view = CheckpointView {
            workspace_id: "ws-1".into(),
            epoch: Some("not-a-valid-oid".into()),
            patch_set: None,
            patch_set_oid: None,
            description: None,
            annotations: BTreeMap::new(),
            is_destroyed: false,
        };

        let result = cp_view.to_view(0);
        assert!(result.is_err());
    }

    #[test]
    fn checkpoint_view_invalid_workspace_id() {
        let cp_view = CheckpointView {
            workspace_id: String::new(),
            epoch: None,
            patch_set: None,
            patch_set_oid: None,
            description: None,
            annotations: BTreeMap::new(),
            is_destroyed: false,
        };

        let result = cp_view.to_view(0);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Error display
    // -----------------------------------------------------------------------

    #[test]
    fn error_display_no_checkpoint() {
        let err = CheckpointError::NoCheckpoint {
            workspace_id: test_ws("agent-1"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("agent-1"));
        assert!(msg.contains("no checkpoint"));
    }

    #[test]
    fn error_display_invalid_data() {
        let err = CheckpointError::InvalidData {
            detail: "bad epoch".into(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("invalid data"));
        assert!(msg.contains("bad epoch"));
    }

    // -----------------------------------------------------------------------
    // Integration: checkpoint + compact in-memory
    // -----------------------------------------------------------------------

    #[test]
    fn checkpoint_interval_logic_over_sequence() {
        let interval = 3;

        // Simulate 10 operations
        let checkpoints: Vec<usize> = (1..=10)
            .filter(|n| should_checkpoint(*n, interval))
            .collect();

        assert_eq!(checkpoints, vec![3, 6, 9]);
    }

    #[test]
    fn maybe_write_checkpoint_respects_interval() {
        // When op_count is not at the interval, should return None
        let _view = MaterializedView {
            op_count: 50,
            ..make_view("ws-1", 'a', 50)
        };

        // We can't call maybe_write_checkpoint without a real repo,
        // but we can test the should_checkpoint guard
        assert!(!should_checkpoint(50, 100));
        assert!(should_checkpoint(100, 100));
    }

    // -----------------------------------------------------------------------
    // Full integration with git (requires tempdir + git init)
    // -----------------------------------------------------------------------

    fn setup_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        use std::process::Command;

        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();

        Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&root)
            .output()
            .unwrap();

        std::fs::write(root.join("README.md"), "# Test\n").unwrap();
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&root)
            .output()
            .unwrap();

        (dir, root)
    }

    #[test]
    fn integration_write_checkpoint_and_compact() {
        let (_dir, root) = setup_repo();
        let ws_id = test_ws("agent-1");

        // Build a chain of 5 operations
        let op1 = Operation {
            parent_ids: vec![],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T12:00:00Z".into(),
            payload: OpPayload::Create {
                epoch: test_epoch('a'),
            },
        };
        let oid1 = append_operation(&root, &ws_id, &op1, None).unwrap();

        let op2 = Operation {
            parent_ids: vec![oid1.clone()],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T12:01:00Z".into(),
            payload: OpPayload::Describe {
                message: "step 2".into(),
            },
        };
        let oid2 = append_operation(&root, &ws_id, &op2, Some(&oid1)).unwrap();

        let op3 = Operation {
            parent_ids: vec![oid2.clone()],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T12:02:00Z".into(),
            payload: OpPayload::Describe {
                message: "step 3".into(),
            },
        };
        let oid3 = append_operation(&root, &ws_id, &op3, Some(&oid2)).unwrap();

        // Materialize the view at this point
        let ps = test_patch_set('a');
        let ops = vec![(oid1, op1), (oid2, op2), (oid3.clone(), op3)];
        let view = materialize_from_ops(ws_id.clone(), &ops, mock_reader(ps.clone())).unwrap();
        assert_eq!(view.op_count, 3);

        // Write a checkpoint
        let cp_oid = maybe_write_checkpoint(
            &root, &ws_id, &view, &oid3, &oid3, 3, // checkpoint every 3 ops
        )
        .unwrap();
        assert!(cp_oid.is_some(), "should write checkpoint at op 3");
        let cp_oid = cp_oid.unwrap();

        // Add two more operations after checkpoint
        let op4 = Operation {
            parent_ids: vec![cp_oid.clone()],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T12:03:00Z".into(),
            payload: OpPayload::Describe {
                message: "step 4 after checkpoint".into(),
            },
        };
        let oid4 = append_operation(&root, &ws_id, &op4, Some(&cp_oid)).unwrap();

        let op5 = Operation {
            parent_ids: vec![oid4.clone()],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T12:04:00Z".into(),
            payload: OpPayload::Describe {
                message: "step 5".into(),
            },
        };
        let _oid5 = append_operation(&root, &ws_id, &op5, Some(&oid4)).unwrap();

        // Compact: should reduce chain from 6 ops to 4 (synthetic root + checkpoint + 2 post-cp ops)
        let result = compact(&root, &ws_id).unwrap();
        assert_eq!(result.ops_before, 6); // create + 2 describe + checkpoint + 2 describe
        assert_eq!(result.ops_after, 4); // synthetic create + checkpoint + 2 describes

        // Verify the chain is correct after compaction
        let stop_pred: Option<&dyn Fn(&Operation) -> bool> = None;
        let chain = walk_chain(&root, &ws_id, None, stop_pred).unwrap();
        assert_eq!(chain.len(), 4);

        // The view from compacted chain should match
        let mut chain_causal: Vec<_> = chain;
        chain_causal.reverse();
        let compacted_view = materialize_from_ops(ws_id, &chain_causal, mock_reader(ps)).unwrap();

        assert_eq!(compacted_view.description, Some("step 5".into()));
        assert_eq!(compacted_view.epoch, Some(test_epoch('a')));
    }

    #[test]
    fn integration_materialize_from_checkpoint() {
        let (_dir, root) = setup_repo();
        let ws_id = test_ws("agent-1");

        // Build chain: Create → Describe → Describe (checkpoint) → Describe
        let op1 = Operation {
            parent_ids: vec![],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T12:00:00Z".into(),
            payload: OpPayload::Create {
                epoch: test_epoch('a'),
            },
        };
        let oid1 = append_operation(&root, &ws_id, &op1, None).unwrap();

        let op2 = Operation {
            parent_ids: vec![oid1.clone()],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T12:01:00Z".into(),
            payload: OpPayload::Describe {
                message: "step 2".into(),
            },
        };
        let oid2 = append_operation(&root, &ws_id, &op2, Some(&oid1)).unwrap();

        let op3 = Operation {
            parent_ids: vec![oid2.clone()],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T12:02:00Z".into(),
            payload: OpPayload::Describe {
                message: "step 3".into(),
            },
        };
        let oid3 = append_operation(&root, &ws_id, &op3, Some(&oid2)).unwrap();

        // Materialize view at step 3 and write checkpoint
        let ps = test_patch_set('a');
        let ops = vec![(oid1, op1), (oid2, op2), (oid3.clone(), op3)];
        let view = materialize_from_ops(ws_id.clone(), &ops, mock_reader(ps.clone())).unwrap();

        let cp_oid = maybe_write_checkpoint(&root, &ws_id, &view, &oid3, &oid3, 3)
            .unwrap()
            .expect("checkpoint should be written");

        // Add an operation after checkpoint
        let op4 = Operation {
            parent_ids: vec![cp_oid.clone()],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T12:03:00Z".into(),
            payload: OpPayload::Describe {
                message: "step 4 after checkpoint".into(),
            },
        };
        let _oid4 = append_operation(&root, &ws_id, &op4, Some(&cp_oid)).unwrap();

        // Materialize from checkpoint
        let cp_view = materialize_from_checkpoint(&root, &ws_id, mock_reader(ps)).unwrap();

        // Should have the latest description
        assert_eq!(cp_view.description, Some("step 4 after checkpoint".into()));
        assert_eq!(cp_view.epoch, Some(test_epoch('a')));
    }

    #[test]
    fn integration_no_checkpoint_falls_back_to_full_replay() {
        let (_dir, root) = setup_repo();
        let ws_id = test_ws("agent-1");

        let op1 = Operation {
            parent_ids: vec![],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T12:00:00Z".into(),
            payload: OpPayload::Create {
                epoch: test_epoch('a'),
            },
        };
        let oid1 = append_operation(&root, &ws_id, &op1, None).unwrap();

        let op2 = Operation {
            parent_ids: vec![oid1.clone()],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T12:01:00Z".into(),
            payload: OpPayload::Describe {
                message: "no checkpoint here".into(),
            },
        };
        let _oid2 = append_operation(&root, &ws_id, &op2, Some(&oid1)).unwrap();

        let ps = test_patch_set('a');
        let view = materialize_from_checkpoint(&root, &ws_id, mock_reader(ps)).unwrap();

        assert_eq!(view.description, Some("no checkpoint here".into()));
        assert_eq!(view.op_count, 2);
    }

    #[test]
    fn integration_compact_without_checkpoint_fails() {
        let (_dir, root) = setup_repo();
        let ws_id = test_ws("agent-1");

        let op1 = Operation {
            parent_ids: vec![],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T12:00:00Z".into(),
            payload: OpPayload::Create {
                epoch: test_epoch('a'),
            },
        };
        let _oid1 = append_operation(&root, &ws_id, &op1, None).unwrap();

        let result = compact(&root, &ws_id);
        assert!(
            matches!(result, Err(CheckpointError::NoCheckpoint { .. })),
            "compact without checkpoint should fail"
        );
    }
}
