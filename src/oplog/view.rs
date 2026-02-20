//! Per-workspace view materialization from op log replay (§5.5).
//!
//! A [`MaterializedView`] is the read-side interpretation of a workspace's
//! operation log. It is produced by replaying operations from the earliest
//! (or from a checkpoint) to the head, accumulating state changes.
//!
//! # Replay semantics
//!
//! Operations are replayed in causal order (as returned by [`walk_chain`]):
//!
//! | Payload | Effect on view |
//! |---------|----------------|
//! | `Create` | Initialize epoch, clear patch set |
//! | `Snapshot` | Replace current patch set with the snapshot's data |
//! | `Compensate` | Clear current patch set (undo) |
//! | `Merge` | Update epoch to `epoch_after`, clear patch set |
//! | `Describe` | Update description metadata |
//! | `Annotate` | Upsert annotation key into metadata |
//! | `Destroy` | Mark view as destroyed |
//!
//! # Example
//!
//! ```text
//! materialize(root, "agent-1")
//!   → walk op log from head to root
//!   → sort in causal order (oldest first)
//!   → replay each operation
//!   → return MaterializedView { epoch, patch_set, metadata, op_count }
//! ```

#![allow(clippy::missing_errors_doc)]

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::model::patch::PatchSet;
use crate::model::types::{EpochId, GitOid, WorkspaceId};
use crate::oplog::read::{walk_chain, OpLogReadError};
use crate::oplog::types::{OpPayload, Operation};

// ---------------------------------------------------------------------------
// MaterializedView
// ---------------------------------------------------------------------------

/// The materialized state of a workspace at its current head.
///
/// Produced by replaying the workspace's op log from root to head.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializedView {
    /// The workspace this view belongs to.
    pub workspace_id: WorkspaceId,

    /// The current epoch of this workspace (from the latest Create or Merge).
    pub epoch: Option<EpochId>,

    /// The current patch set (accumulated from Snapshot operations).
    ///
    /// `None` if no snapshot has been taken yet (the workspace is clean).
    pub patch_set: Option<PatchSet>,

    /// The git blob OID of the latest patch set (from the most recent Snapshot).
    ///
    /// Used by callers who need the raw blob without deserializing the patch set.
    pub patch_set_oid: Option<GitOid>,

    /// Human-readable description (from the latest Describe operation).
    pub description: Option<String>,

    /// Annotations accumulated from Annotate operations (latest wins per key).
    pub annotations: BTreeMap<String, BTreeMap<String, serde_json::Value>>,

    /// Total number of operations replayed to produce this view.
    pub op_count: usize,

    /// Whether this workspace has been destroyed.
    pub is_destroyed: bool,
}

impl MaterializedView {
    /// Create a new empty view for a workspace.
    #[must_use]
    pub const fn empty(workspace_id: WorkspaceId) -> Self {
        Self {
            workspace_id,
            epoch: None,
            patch_set: None,
            patch_set_oid: None,
            description: None,
            annotations: BTreeMap::new(),
            op_count: 0,
            is_destroyed: false,
        }
    }

    /// Return `true` if the workspace has been destroyed.
    #[must_use]
    pub const fn destroyed(&self) -> bool {
        self.is_destroyed
    }

    /// Return `true` if the workspace has a non-empty patch set.
    #[must_use]
    pub fn has_changes(&self) -> bool {
        self.patch_set.as_ref().is_some_and(|ps| !ps.is_empty())
    }
}

impl fmt::Display for MaterializedView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "view({}", self.workspace_id)?;
        if let Some(epoch) = &self.epoch {
            write!(f, ", epoch={}", &epoch.as_str()[..12])?;
        }
        if let Some(ps) = &self.patch_set {
            write!(f, ", {} patches", ps.len())?;
        }
        write!(f, ", {} ops", self.op_count)?;
        if self.is_destroyed {
            write!(f, ", DESTROYED")?;
        }
        write!(f, ")")
    }
}

// ---------------------------------------------------------------------------
// View materialization error
// ---------------------------------------------------------------------------

/// Errors that can occur during view materialization.
#[derive(Debug)]
pub enum ViewError {
    /// Op log read error.
    OpLog(OpLogReadError),

    /// Failed to read a patch set blob.
    PatchSetRead {
        /// The patch set blob OID.
        oid: String,
        /// Error detail.
        detail: String,
    },
}

impl fmt::Display for ViewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpLog(e) => write!(f, "op log error: {e}"),
            Self::PatchSetRead { oid, detail } => {
                write!(f, "failed to read patch set blob {oid}: {detail}")
            }
        }
    }
}

impl std::error::Error for ViewError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::OpLog(e) => Some(e),
            Self::PatchSetRead { .. } => None,
        }
    }
}

impl From<OpLogReadError> for ViewError {
    fn from(e: OpLogReadError) -> Self {
        Self::OpLog(e)
    }
}

// ---------------------------------------------------------------------------
// Replay engine
// ---------------------------------------------------------------------------

/// Apply a single operation to a mutable view, updating state.
///
/// `read_patch_set` is a callback that reads a patch-set git blob OID and
/// returns the deserialized `PatchSet`. This allows the caller to control
/// how blobs are fetched (from disk, cache, or mock).
fn apply_operation<F>(
    view: &mut MaterializedView,
    _oid: &GitOid,
    op: &Operation,
    read_patch_set: &F,
) -> Result<(), ViewError>
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
            // Compensation clears the current patch set (reverts to base epoch).
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

/// Materialize a workspace view by replaying its operation log.
///
/// Walks the entire op log chain from root to head and replays each operation
/// in causal order. The `read_patch_set` callback is used to fetch patch-set
/// blob contents.
///
/// # Errors
///
/// Returns `ViewError::OpLog` if the op log cannot be read, or
/// `ViewError::PatchSetRead` if a patch set blob cannot be fetched.
#[allow(dead_code)]
pub fn materialize<F>(
    root: &Path,
    workspace: &WorkspaceId,
    read_patch_set: F,
) -> Result<MaterializedView, ViewError>
where
    F: Fn(&GitOid) -> Result<PatchSet, ViewError>,
{
    let no_stop: Option<&dyn Fn(&Operation) -> bool> = None;
    let chain = walk_chain(root, workspace, None, no_stop)?;

    // walk_chain returns (oid, operation) in BFS order (head first).
    // Reverse to get causal order (oldest first).
    let mut ops: Vec<_> = chain;
    ops.reverse();

    let mut view = MaterializedView::empty(workspace.clone());

    for (oid, op) in &ops {
        apply_operation(&mut view, oid, op, &read_patch_set)?;
    }

    Ok(view)
}

/// Materialize a view from a pre-built list of operations (for testing or
/// when the op log is already loaded).
///
/// Operations must be in causal order (oldest first).
pub fn materialize_from_ops<F>(
    workspace: WorkspaceId,
    ops: &[(GitOid, Operation)],
    read_patch_set: F,
) -> Result<MaterializedView, ViewError>
where
    F: Fn(&GitOid) -> Result<PatchSet, ViewError>,
{
    let mut view = MaterializedView::empty(workspace);

    for (oid, op) in ops {
        apply_operation(&mut view, oid, op, &read_patch_set)?;
    }

    Ok(view)
}

// ---------------------------------------------------------------------------
// Git-based patch set reader
// ---------------------------------------------------------------------------

/// Read a patch set blob from the git object store.
///
/// Uses `git cat-file -p <oid>` to fetch the blob, then deserializes
/// the JSON content as a [`PatchSet`].
///
/// # Errors
///
/// Returns `ViewError::PatchSetRead` if the blob cannot be read or
/// deserialized.
#[allow(dead_code)]
pub fn read_patch_set_blob(root: &Path, oid: &GitOid) -> Result<PatchSet, ViewError> {
    let output = std::process::Command::new("git")
        .args(["cat-file", "-p", oid.as_str()])
        .current_dir(root)
        .output()
        .map_err(|e| ViewError::PatchSetRead {
            oid: oid.as_str().to_owned(),
            detail: format!("spawn git: {e}"),
        })?;

    if !output.status.success() {
        return Err(ViewError::PatchSetRead {
            oid: oid.as_str().to_owned(),
            detail: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }

    serde_json::from_slice(&output.stdout).map_err(|e| ViewError::PatchSetRead {
        oid: oid.as_str().to_owned(),
        detail: format!("JSON parse: {e}"),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    // Helper to create a test GitOid
    fn test_oid(c: char) -> GitOid {
        GitOid::new(&c.to_string().repeat(40)).unwrap()
    }

    fn test_epoch(c: char) -> EpochId {
        EpochId::new(&c.to_string().repeat(40)).unwrap()
    }

    fn test_ws(name: &str) -> WorkspaceId {
        WorkspaceId::new(name).unwrap()
    }

    fn timestamp() -> String {
        "2026-02-19T12:00:00Z".to_owned()
    }

    // Create a simple PatchSet for testing
    fn test_patch_set(epoch_char: char) -> PatchSet {
        use crate::model::patch::{FileId, PatchValue};
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

    // A mock patch-set reader that returns a fixed PatchSet
    fn mock_reader(ps: PatchSet) -> impl Fn(&GitOid) -> Result<PatchSet, ViewError> {
        move |_oid| Ok(ps.clone())
    }

    // A patch-set reader that always fails
    fn failing_reader() -> impl Fn(&GitOid) -> Result<PatchSet, ViewError> {
        |oid| {
            Err(ViewError::PatchSetRead {
                oid: oid.as_str().to_owned(),
                detail: "mock failure".to_owned(),
            })
        }
    }

    // Build an operation with a given payload
    fn make_op(ws: &str, payload: OpPayload) -> Operation {
        Operation {
            parent_ids: vec![],
            workspace_id: test_ws(ws),
            timestamp: timestamp(),
            payload,
        }
    }

    // -----------------------------------------------------------------------
    // MaterializedView basics
    // -----------------------------------------------------------------------

    #[test]
    fn empty_view() {
        let view = MaterializedView::empty(test_ws("test"));
        assert_eq!(view.workspace_id, test_ws("test"));
        assert!(view.epoch.is_none());
        assert!(view.patch_set.is_none());
        assert!(view.description.is_none());
        assert!(view.annotations.is_empty());
        assert_eq!(view.op_count, 0);
        assert!(!view.is_destroyed);
        assert!(!view.has_changes());
    }

    #[test]
    fn view_display() {
        let mut view = MaterializedView::empty(test_ws("agent-1"));
        view.epoch = Some(test_epoch('a'));
        view.op_count = 5;
        let display = format!("{view}");
        assert!(display.contains("agent-1"));
        assert!(display.contains("5 ops"));
    }

    #[test]
    fn view_display_destroyed() {
        let mut view = MaterializedView::empty(test_ws("ws-1"));
        view.is_destroyed = true;
        view.op_count = 3;
        let display = format!("{view}");
        assert!(display.contains("DESTROYED"));
    }

    #[test]
    fn view_serde_roundtrip() {
        let mut view = MaterializedView::empty(test_ws("ws-1"));
        view.epoch = Some(test_epoch('a'));
        view.description = Some("test workspace".into());
        view.op_count = 2;

        let json = serde_json::to_string(&view).unwrap();
        let decoded: MaterializedView = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, view);
    }

    // -----------------------------------------------------------------------
    // Replay: Create
    // -----------------------------------------------------------------------

    #[test]
    fn replay_create() {
        let ops = vec![(
            test_oid('1'),
            make_op(
                "ws-1",
                OpPayload::Create {
                    epoch: test_epoch('a'),
                },
            ),
        )];
        let view =
            materialize_from_ops(test_ws("ws-1"), &ops, mock_reader(test_patch_set('a'))).unwrap();

        assert_eq!(view.epoch, Some(test_epoch('a')));
        assert!(view.patch_set.is_none());
        assert_eq!(view.op_count, 1);
        assert!(!view.is_destroyed);
    }

    // -----------------------------------------------------------------------
    // Replay: Snapshot
    // -----------------------------------------------------------------------

    #[test]
    fn replay_snapshot() {
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
        ];
        let view = materialize_from_ops(test_ws("ws-1"), &ops, mock_reader(ps.clone())).unwrap();

        assert_eq!(view.epoch, Some(test_epoch('a')));
        assert_eq!(view.patch_set, Some(ps));
        assert_eq!(view.patch_set_oid, Some(test_oid('d')));
        assert_eq!(view.op_count, 2);
        assert!(view.has_changes());
    }

    #[test]
    fn snapshot_read_failure_propagates() {
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
        let result = materialize_from_ops(test_ws("ws-1"), &ops, failing_reader());
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Replay: Compensate
    // -----------------------------------------------------------------------

    #[test]
    fn replay_compensate_clears_patch_set() {
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
                    OpPayload::Compensate {
                        target_op: test_oid('2'),
                        reason: "undo snapshot".into(),
                    },
                ),
            ),
        ];
        let view = materialize_from_ops(test_ws("ws-1"), &ops, mock_reader(ps)).unwrap();

        assert!(view.patch_set.is_none());
        assert!(view.patch_set_oid.is_none());
        assert_eq!(view.op_count, 3);
        assert!(!view.has_changes());
    }

    // -----------------------------------------------------------------------
    // Replay: Merge
    // -----------------------------------------------------------------------

    #[test]
    fn replay_merge_updates_epoch() {
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
                    OpPayload::Merge {
                        sources: vec![test_ws("ws-1"), test_ws("ws-2")],
                        epoch_before: test_epoch('a'),
                        epoch_after: test_epoch('b'),
                    },
                ),
            ),
        ];
        let view =
            materialize_from_ops(test_ws("ws-1"), &ops, mock_reader(test_patch_set('a'))).unwrap();

        assert_eq!(view.epoch, Some(test_epoch('b')));
        assert!(view.patch_set.is_none(), "merge clears patch set");
        assert_eq!(view.op_count, 3);
    }

    // -----------------------------------------------------------------------
    // Replay: Describe
    // -----------------------------------------------------------------------

    #[test]
    fn replay_describe_updates_metadata() {
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
                    OpPayload::Describe {
                        message: "implementing auth".into(),
                    },
                ),
            ),
        ];
        let view =
            materialize_from_ops(test_ws("ws-1"), &ops, mock_reader(test_patch_set('a'))).unwrap();

        assert_eq!(view.description, Some("implementing auth".into()));
        assert_eq!(view.op_count, 2);
    }

    #[test]
    fn describe_latest_wins() {
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
                    OpPayload::Describe {
                        message: "first description".into(),
                    },
                ),
            ),
            (
                test_oid('3'),
                make_op(
                    "ws-1",
                    OpPayload::Describe {
                        message: "updated description".into(),
                    },
                ),
            ),
        ];
        let view =
            materialize_from_ops(test_ws("ws-1"), &ops, mock_reader(test_patch_set('a'))).unwrap();

        assert_eq!(view.description, Some("updated description".into()));
    }

    // -----------------------------------------------------------------------
    // Replay: Annotate
    // -----------------------------------------------------------------------

    #[test]
    fn replay_annotate_adds_annotation() {
        let mut data = BTreeMap::new();
        data.insert("passed".into(), serde_json::Value::Bool(true));

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
                    OpPayload::Annotate {
                        key: "validation".into(),
                        data: data.clone(),
                    },
                ),
            ),
        ];
        let view =
            materialize_from_ops(test_ws("ws-1"), &ops, mock_reader(test_patch_set('a'))).unwrap();

        assert!(view.annotations.contains_key("validation"));
        assert_eq!(
            view.annotations["validation"]["passed"],
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn annotate_latest_wins_per_key() {
        let mut data1 = BTreeMap::new();
        data1.insert("status".into(), serde_json::Value::String("pending".into()));

        let mut data2 = BTreeMap::new();
        data2.insert(
            "status".into(),
            serde_json::Value::String("approved".into()),
        );

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
                    OpPayload::Annotate {
                        key: "review".into(),
                        data: data1,
                    },
                ),
            ),
            (
                test_oid('3'),
                make_op(
                    "ws-1",
                    OpPayload::Annotate {
                        key: "review".into(),
                        data: data2,
                    },
                ),
            ),
        ];
        let view =
            materialize_from_ops(test_ws("ws-1"), &ops, mock_reader(test_patch_set('a'))).unwrap();

        assert_eq!(
            view.annotations["review"]["status"],
            serde_json::Value::String("approved".into())
        );
    }

    // -----------------------------------------------------------------------
    // Replay: Destroy
    // -----------------------------------------------------------------------

    #[test]
    fn replay_destroy() {
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
            (test_oid('2'), make_op("ws-1", OpPayload::Destroy)),
        ];
        let view =
            materialize_from_ops(test_ws("ws-1"), &ops, mock_reader(test_patch_set('a'))).unwrap();

        assert!(view.is_destroyed);
        assert!(view.destroyed());
        assert_eq!(view.op_count, 2);
    }

    // -----------------------------------------------------------------------
    // Full lifecycle
    // -----------------------------------------------------------------------

    #[test]
    fn full_lifecycle_create_snapshot_describe_merge() {
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
                    OpPayload::Describe {
                        message: "implementing feature X".into(),
                    },
                ),
            ),
            (
                test_oid('3'),
                make_op(
                    "ws-1",
                    OpPayload::Snapshot {
                        patch_set_oid: test_oid('d'),
                    },
                ),
            ),
            (
                test_oid('4'),
                make_op(
                    "ws-1",
                    OpPayload::Merge {
                        sources: vec![test_ws("ws-1")],
                        epoch_before: test_epoch('a'),
                        epoch_after: test_epoch('b'),
                    },
                ),
            ),
        ];

        let view = materialize_from_ops(test_ws("ws-1"), &ops, mock_reader(ps)).unwrap();

        assert_eq!(view.epoch, Some(test_epoch('b')));
        assert!(view.patch_set.is_none(), "merge clears patches");
        assert_eq!(view.description, Some("implementing feature X".into()));
        assert_eq!(view.op_count, 4);
        assert!(!view.is_destroyed);
    }

    #[test]
    fn empty_op_list_produces_empty_view() {
        let ops: Vec<(GitOid, Operation)> = vec![];
        let view =
            materialize_from_ops(test_ws("ws-1"), &ops, mock_reader(test_patch_set('a'))).unwrap();

        assert_eq!(view.op_count, 0);
        assert!(view.epoch.is_none());
        assert!(view.patch_set.is_none());
    }

    #[test]
    fn multiple_snapshots_last_wins() {
        use crate::model::patch::{FileId, PatchValue};

        let ps1 = test_patch_set('a');
        let mut ps2_patches = BTreeMap::new();
        ps2_patches.insert(
            PathBuf::from("src/lib.rs"),
            PatchValue::Add {
                blob: test_oid('9'),
                file_id: FileId::new(2),
            },
        );
        let ps2 = PatchSet {
            base_epoch: test_epoch('a'),
            patches: ps2_patches,
        };

        let patch_sets: BTreeMap<String, PatchSet> = [
            (test_oid('d').as_str().to_owned(), ps1),
            (test_oid('e').as_str().to_owned(), ps2.clone()),
        ]
        .into_iter()
        .collect();

        let reader = move |oid: &GitOid| {
            patch_sets
                .get(oid.as_str())
                .cloned()
                .ok_or_else(|| ViewError::PatchSetRead {
                    oid: oid.as_str().to_owned(),
                    detail: "not found".into(),
                })
        };

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
                    OpPayload::Snapshot {
                        patch_set_oid: test_oid('e'),
                    },
                ),
            ),
        ];
        let view = materialize_from_ops(test_ws("ws-1"), &ops, reader).unwrap();

        assert_eq!(view.patch_set, Some(ps2));
        assert_eq!(view.patch_set_oid, Some(test_oid('e')));
    }

    // -----------------------------------------------------------------------
    // Order matters
    // -----------------------------------------------------------------------

    #[test]
    fn causal_order_matters_create_then_destroy_vs_destroy_then_create() {
        // Create then destroy → destroyed
        let ops1 = vec![
            (
                test_oid('1'),
                make_op(
                    "ws-1",
                    OpPayload::Create {
                        epoch: test_epoch('a'),
                    },
                ),
            ),
            (test_oid('2'), make_op("ws-1", OpPayload::Destroy)),
        ];
        let view1 =
            materialize_from_ops(test_ws("ws-1"), &ops1, mock_reader(test_patch_set('a'))).unwrap();
        assert!(view1.is_destroyed);

        // Destroy then create → not destroyed (re-created)
        let ops2 = vec![
            (test_oid('1'), make_op("ws-1", OpPayload::Destroy)),
            (
                test_oid('2'),
                make_op(
                    "ws-1",
                    OpPayload::Create {
                        epoch: test_epoch('b'),
                    },
                ),
            ),
        ];
        let view2 =
            materialize_from_ops(test_ws("ws-1"), &ops2, mock_reader(test_patch_set('a'))).unwrap();
        assert!(!view2.is_destroyed);
        assert_eq!(view2.epoch, Some(test_epoch('b')));
    }
}
