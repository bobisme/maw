//! Operation struct and OpPayload enum — canonical JSON for deterministic hashing (§5.3).
//!
//! Operations are the fundamental unit of the op log. Each operation records
//! a single workspace mutation (create, destroy, snapshot, merge, compensate,
//! describe, annotate) with enough metadata to replay or derive state.
//!
//! Canonical JSON rules:
//! - Sorted keys (guaranteed by `serde_json` with `BTreeMap` / `#[serde(sort_keys)]`)
//! - No trailing whitespace
//! - Deterministic: serialize twice → identical bytes

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::types::{EpochId, GitOid, WorkspaceId};

// ---------------------------------------------------------------------------
// Operation
// ---------------------------------------------------------------------------

/// A single operation in a workspace's op log (§5.3).
///
/// Operations form a chain: each operation points to its parent(s) by git OID.
/// For single-workspace operations there is one parent; merge operations may
/// have multiple parents (one per source workspace).
///
/// The entire struct serializes to canonical JSON, which is then stored as a
/// git blob. The blob's OID becomes the operation's identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    /// OIDs of parent operations (empty for the first op in a workspace).
    pub parent_ids: Vec<GitOid>,

    /// The workspace that performed this operation.
    pub workspace_id: WorkspaceId,

    /// ISO 8601 timestamp (UTC) of when the operation was created.
    ///
    /// Stored as a string for canonical JSON (avoids platform-specific
    /// floating-point or integer timestamp representations).
    pub timestamp: String,

    /// The mutation that this operation represents.
    pub payload: OpPayload,
}

// ---------------------------------------------------------------------------
// OpPayload
// ---------------------------------------------------------------------------

/// The kind of mutation recorded by an [`Operation`] (§5.3).
///
/// Each variant captures the minimal data needed to replay or undo the
/// operation. Serialized with a `"type"` tag for canonical JSON:
/// `{"type":"create","epoch":"…"}` etc.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OpPayload {
    /// Workspace was created, anchored to a base epoch.
    Create {
        /// The epoch this workspace is based on.
        epoch: EpochId,
    },

    /// Workspace was destroyed.
    Destroy,

    /// Working directory was snapshotted — records the files that changed
    /// relative to the base epoch as a patch-set blob.
    Snapshot {
        /// Git blob OID of the serialized [`PatchSet`] (stored separately).
        patch_set_oid: GitOid,
    },

    /// One or more workspaces were merged into a new epoch.
    Merge {
        /// Source workspace IDs that were merged.
        sources: Vec<WorkspaceId>,
        /// The epoch before the merge.
        epoch_before: EpochId,
        /// The new epoch produced by the merge.
        epoch_after: EpochId,
    },

    /// A compensation (undo) operation that reverses a prior operation.
    Compensate {
        /// The OID of the operation being undone.
        target_op: GitOid,
        /// Human-readable reason for the compensation.
        reason: String,
    },

    /// The workspace description was updated (human-readable label).
    Describe {
        /// The new description text.
        message: String,
    },

    /// An arbitrary annotation attached to the op log (e.g., validation
    /// result, review status, CI outcome).
    Annotate {
        /// Annotation key (e.g., "validation", "review").
        key: String,
        /// Annotation value — arbitrary JSON-safe data.
        ///
        /// Uses `BTreeMap` for deterministic key ordering in canonical JSON.
        data: BTreeMap<String, serde_json::Value>,
    },
}

// ---------------------------------------------------------------------------
// Canonical JSON helpers
// ---------------------------------------------------------------------------

impl Operation {
    /// Serialize this operation to canonical JSON bytes.
    ///
    /// Canonical JSON: sorted keys, no trailing whitespace, deterministic.
    /// Two calls with the same `Operation` always produce identical bytes.
    ///
    /// # Errors
    /// Returns an error if serialization fails (shouldn't happen for valid ops).
    pub fn to_canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        // serde_json serializes struct fields in declaration order.
        // For BTreeMap keys inside Annotate, serde_json sorts them.
        // This gives us canonical output without a custom serializer.
        serde_json::to_vec(self)
    }

    /// Deserialize an operation from JSON bytes.
    ///
    /// # Errors
    /// Returns an error if the bytes are not valid JSON or don't match the schema.
    pub fn from_json(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: build a valid 40-char hex OID string.
    fn oid(c: char) -> String {
        c.to_string().repeat(40)
    }

    fn git_oid(c: char) -> GitOid {
        GitOid::new(&oid(c)).unwrap()
    }

    fn epoch(c: char) -> EpochId {
        EpochId::new(&oid(c)).unwrap()
    }

    fn ws(name: &str) -> WorkspaceId {
        WorkspaceId::new(name).unwrap()
    }

    fn timestamp() -> String {
        "2026-02-19T12:00:00Z".to_owned()
    }

    // -----------------------------------------------------------------------
    // OpPayload variant serialization round-trips
    // -----------------------------------------------------------------------

    #[test]
    fn create_round_trip() {
        let op = Operation {
            parent_ids: vec![],
            workspace_id: ws("agent-1"),
            timestamp: timestamp(),
            payload: OpPayload::Create { epoch: epoch('a') },
        };
        let json = op.to_canonical_json().unwrap();
        let parsed = Operation::from_json(&json).unwrap();
        assert_eq!(op, parsed);
    }

    #[test]
    fn destroy_round_trip() {
        let op = Operation {
            parent_ids: vec![git_oid('b')],
            workspace_id: ws("agent-2"),
            timestamp: timestamp(),
            payload: OpPayload::Destroy,
        };
        let json = op.to_canonical_json().unwrap();
        let parsed = Operation::from_json(&json).unwrap();
        assert_eq!(op, parsed);
    }

    #[test]
    fn snapshot_round_trip() {
        let op = Operation {
            parent_ids: vec![git_oid('c')],
            workspace_id: ws("feature-x"),
            timestamp: timestamp(),
            payload: OpPayload::Snapshot {
                patch_set_oid: git_oid('d'),
            },
        };
        let json = op.to_canonical_json().unwrap();
        let parsed = Operation::from_json(&json).unwrap();
        assert_eq!(op, parsed);
    }

    #[test]
    fn merge_round_trip() {
        let op = Operation {
            parent_ids: vec![git_oid('e'), git_oid('f')],
            workspace_id: ws("default"),
            timestamp: timestamp(),
            payload: OpPayload::Merge {
                sources: vec![ws("agent-1"), ws("agent-2")],
                epoch_before: epoch('a'),
                epoch_after: epoch('b'),
            },
        };
        let json = op.to_canonical_json().unwrap();
        let parsed = Operation::from_json(&json).unwrap();
        assert_eq!(op, parsed);
    }

    #[test]
    fn compensate_round_trip() {
        let op = Operation {
            parent_ids: vec![git_oid('c')],
            workspace_id: ws("agent-1"),
            timestamp: timestamp(),
            payload: OpPayload::Compensate {
                target_op: git_oid('a'),
                reason: "reverted broken snapshot".to_owned(),
            },
        };
        let json = op.to_canonical_json().unwrap();
        let parsed = Operation::from_json(&json).unwrap();
        assert_eq!(op, parsed);
    }

    #[test]
    fn describe_round_trip() {
        let op = Operation {
            parent_ids: vec![git_oid('d')],
            workspace_id: ws("agent-1"),
            timestamp: timestamp(),
            payload: OpPayload::Describe {
                message: "implementing auth module".to_owned(),
            },
        };
        let json = op.to_canonical_json().unwrap();
        let parsed = Operation::from_json(&json).unwrap();
        assert_eq!(op, parsed);
    }

    #[test]
    fn annotate_round_trip() {
        let mut data = BTreeMap::new();
        data.insert("passed".to_owned(), serde_json::Value::Bool(true));
        data.insert(
            "duration_ms".to_owned(),
            serde_json::Value::Number(1234.into()),
        );
        data.insert(
            "command".to_owned(),
            serde_json::Value::String("cargo test".to_owned()),
        );

        let op = Operation {
            parent_ids: vec![git_oid('e')],
            workspace_id: ws("default"),
            timestamp: timestamp(),
            payload: OpPayload::Annotate {
                key: "validation".to_owned(),
                data,
            },
        };
        let json = op.to_canonical_json().unwrap();
        let parsed = Operation::from_json(&json).unwrap();
        assert_eq!(op, parsed);
    }

    // -----------------------------------------------------------------------
    // Canonical JSON determinism
    // -----------------------------------------------------------------------

    #[test]
    fn canonical_json_is_deterministic() {
        let mut data = BTreeMap::new();
        data.insert("z_key".to_owned(), serde_json::Value::Bool(false));
        data.insert("a_key".to_owned(), serde_json::Value::Bool(true));
        data.insert(
            "m_key".to_owned(),
            serde_json::Value::String("hello".to_owned()),
        );

        let op = Operation {
            parent_ids: vec![git_oid('a'), git_oid('b')],
            workspace_id: ws("agent-1"),
            timestamp: timestamp(),
            payload: OpPayload::Annotate {
                key: "test".to_owned(),
                data,
            },
        };

        let json1 = op.to_canonical_json().unwrap();
        let json2 = op.to_canonical_json().unwrap();
        assert_eq!(json1, json2, "canonical JSON must be deterministic");

        // Verify BTreeMap keys are sorted in output
        let json_str = String::from_utf8(json1).unwrap();
        let a_pos = json_str.find("\"a_key\"").unwrap();
        let m_pos = json_str.find("\"m_key\"").unwrap();
        let z_pos = json_str.find("\"z_key\"").unwrap();
        assert!(a_pos < m_pos, "a_key should come before m_key");
        assert!(m_pos < z_pos, "m_key should come before z_key");
    }

    #[test]
    fn canonical_json_sorted_keys_in_annotate() {
        let mut data = BTreeMap::new();
        data.insert("zebra".to_owned(), serde_json::Value::Null);
        data.insert("apple".to_owned(), serde_json::Value::Null);

        let op = Operation {
            parent_ids: vec![],
            workspace_id: ws("w"),
            timestamp: timestamp(),
            payload: OpPayload::Annotate {
                key: "test".to_owned(),
                data,
            },
        };

        let json = String::from_utf8(op.to_canonical_json().unwrap()).unwrap();
        let apple_pos = json.find("\"apple\"").unwrap();
        let zebra_pos = json.find("\"zebra\"").unwrap();
        assert!(
            apple_pos < zebra_pos,
            "BTreeMap keys must be sorted: apple < zebra"
        );
    }

    // -----------------------------------------------------------------------
    // Payload type tag verification
    // -----------------------------------------------------------------------

    #[test]
    fn payload_type_tag_create() {
        let op = Operation {
            parent_ids: vec![],
            workspace_id: ws("w"),
            timestamp: timestamp(),
            payload: OpPayload::Create { epoch: epoch('a') },
        };
        let json = String::from_utf8(op.to_canonical_json().unwrap()).unwrap();
        assert!(
            json.contains("\"type\":\"create\""),
            "Create variant should have type:create tag"
        );
    }

    #[test]
    fn payload_type_tag_destroy() {
        let op = Operation {
            parent_ids: vec![],
            workspace_id: ws("w"),
            timestamp: timestamp(),
            payload: OpPayload::Destroy,
        };
        let json = String::from_utf8(op.to_canonical_json().unwrap()).unwrap();
        assert!(
            json.contains("\"type\":\"destroy\""),
            "Destroy variant should have type:destroy tag"
        );
    }

    #[test]
    fn payload_type_tag_snapshot() {
        let op = Operation {
            parent_ids: vec![],
            workspace_id: ws("w"),
            timestamp: timestamp(),
            payload: OpPayload::Snapshot {
                patch_set_oid: git_oid('a'),
            },
        };
        let json = String::from_utf8(op.to_canonical_json().unwrap()).unwrap();
        assert!(
            json.contains("\"type\":\"snapshot\""),
            "Snapshot variant should have type:snapshot tag"
        );
    }

    #[test]
    fn payload_type_tag_merge() {
        let op = Operation {
            parent_ids: vec![],
            workspace_id: ws("w"),
            timestamp: timestamp(),
            payload: OpPayload::Merge {
                sources: vec![],
                epoch_before: epoch('a'),
                epoch_after: epoch('b'),
            },
        };
        let json = String::from_utf8(op.to_canonical_json().unwrap()).unwrap();
        assert!(
            json.contains("\"type\":\"merge\""),
            "Merge variant should have type:merge tag"
        );
    }

    #[test]
    fn payload_type_tag_compensate() {
        let op = Operation {
            parent_ids: vec![],
            workspace_id: ws("w"),
            timestamp: timestamp(),
            payload: OpPayload::Compensate {
                target_op: git_oid('a'),
                reason: "test".to_owned(),
            },
        };
        let json = String::from_utf8(op.to_canonical_json().unwrap()).unwrap();
        assert!(
            json.contains("\"type\":\"compensate\""),
            "Compensate variant should have type:compensate tag"
        );
    }

    #[test]
    fn payload_type_tag_describe() {
        let op = Operation {
            parent_ids: vec![],
            workspace_id: ws("w"),
            timestamp: timestamp(),
            payload: OpPayload::Describe {
                message: "hello".to_owned(),
            },
        };
        let json = String::from_utf8(op.to_canonical_json().unwrap()).unwrap();
        assert!(
            json.contains("\"type\":\"describe\""),
            "Describe variant should have type:describe tag"
        );
    }

    #[test]
    fn payload_type_tag_annotate() {
        let op = Operation {
            parent_ids: vec![],
            workspace_id: ws("w"),
            timestamp: timestamp(),
            payload: OpPayload::Annotate {
                key: "k".to_owned(),
                data: BTreeMap::new(),
            },
        };
        let json = String::from_utf8(op.to_canonical_json().unwrap()).unwrap();
        assert!(
            json.contains("\"type\":\"annotate\""),
            "Annotate variant should have type:annotate tag"
        );
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn empty_parent_ids() {
        let op = Operation {
            parent_ids: vec![],
            workspace_id: ws("first"),
            timestamp: timestamp(),
            payload: OpPayload::Create { epoch: epoch('a') },
        };
        let json = String::from_utf8(op.to_canonical_json().unwrap()).unwrap();
        assert!(json.contains("\"parent_ids\":[]"));
    }

    #[test]
    fn multiple_parent_ids() {
        let op = Operation {
            parent_ids: vec![git_oid('a'), git_oid('b'), git_oid('c')],
            workspace_id: ws("merged"),
            timestamp: timestamp(),
            payload: OpPayload::Merge {
                sources: vec![ws("w1"), ws("w2"), ws("w3")],
                epoch_before: epoch('d'),
                epoch_after: epoch('e'),
            },
        };
        let json = op.to_canonical_json().unwrap();
        let parsed = Operation::from_json(&json).unwrap();
        assert_eq!(parsed.parent_ids.len(), 3);
        assert_eq!(parsed.payload, op.payload);
    }

    #[test]
    fn describe_with_newlines_and_unicode() {
        let op = Operation {
            parent_ids: vec![],
            workspace_id: ws("w"),
            timestamp: timestamp(),
            payload: OpPayload::Describe {
                message: "line 1\nline 2\n日本語".to_owned(),
            },
        };
        let json = op.to_canonical_json().unwrap();
        let parsed = Operation::from_json(&json).unwrap();
        assert_eq!(op, parsed);
    }
}
