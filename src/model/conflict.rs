//! Structured conflict model — variant types and serialization (§6.4).
//!
//! Conflicts in Manifold are structured and localizable — per file, per region,
//! per edit atom — not giant marker soup. Each variant captures the minimal data
//! needed to present the conflict to an agent or human for resolution.
//!
//! # Conflict Variants
//!
//! | Variant | Description |
//! |---------|-------------|
//! | [`Conflict::Content`] | Two or more workspaces modified the same file region |
//! | [`Conflict::AddAdd`] | Same path added independently with different content |
//! | [`Conflict::ModifyDelete`] | One workspace modified, another deleted the file |
//! | [`Conflict::DivergentRename`] | Same file renamed to different destinations |
//!
//! # Serialization
//!
//! All types use `#[serde(tag = "type")]` for clean, tagged JSON:
//!
//! ```json
//! {
//!   "type": "content",
//!   "path": "src/lib.rs",
//!   "file_id": "00000000000000000000000000000001",
//!   "base": "aaaa...",
//!   "sides": [...],
//!   "atoms": [...]
//! }
//! ```

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::ordering::OrderingKey;
use super::patch::FileId;
use super::types::GitOid;

// ---------------------------------------------------------------------------
// ConflictSide
// ---------------------------------------------------------------------------

/// One side of a conflict — identifies which workspace contributed what content.
///
/// # Example
///
/// In a two-way content conflict between workspaces `alice` and `bob`:
/// - Side 0: `{ workspace: "alice", content: <oid-of-alice-version>, timestamp: ... }`
/// - Side 1: `{ workspace: "bob", content: <oid-of-bob-version>, timestamp: ... }`
///
/// Sides are sorted by `(workspace_id, timestamp)` for deterministic output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictSide {
    /// The workspace that produced this side of the conflict.
    pub workspace: String,

    /// Git blob OID of the file content from this workspace.
    pub content: GitOid,

    /// The ordering key of the operation that produced this side.
    ///
    /// Used for display and tie-breaking, not for conflict resolution logic.
    pub timestamp: OrderingKey,
}

impl ConflictSide {
    /// Create a new conflict side.
    #[must_use]
    pub fn new(workspace: String, content: GitOid, timestamp: OrderingKey) -> Self {
        Self {
            workspace,
            content,
            timestamp,
        }
    }
}

// ---------------------------------------------------------------------------
// ConflictAtom (forward declaration — full definition in bd-15yn.2)
// ---------------------------------------------------------------------------

/// A minimal conflict region within a file (placeholder for bd-15yn.2).
///
/// The full `ConflictAtom` type (with `Region`, line ranges, AST spans) will
/// be defined in subtask bd-15yn.2. For now, this placeholder carries a
/// human-readable description of the conflicting region.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictAtom {
    /// Human-readable description of the conflicting region.
    pub description: String,
}

impl ConflictAtom {
    /// Create a new conflict atom with a description.
    #[must_use]
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Conflict
// ---------------------------------------------------------------------------

/// A structured conflict produced by the merge engine.
///
/// Each variant captures a specific kind of merge conflict with enough data
/// to present it to an agent for resolution. Conflicts never appear in git
/// commits on main — they are resolved before epoch advancement.
///
/// # Serialization
///
/// Uses `#[serde(tag = "type")]` for tagged JSON output with `snake_case` names.
///
/// ```
/// use maw::model::conflict::{Conflict, ConflictSide, ConflictAtom};
/// use maw::model::types::GitOid;
/// use maw::model::ordering::OrderingKey;
/// use maw::model::patch::FileId;
/// use maw::model::types::EpochId;
///
/// let oid = GitOid::new(&"a".repeat(40)).unwrap();
/// let epoch = EpochId::new(&"e".repeat(40)).unwrap();
/// let ts = OrderingKey::new(epoch.clone(), "ws-1".parse().unwrap(), 1, 1000);
/// let side = ConflictSide::new("ws-1".into(), oid.clone(), ts);
///
/// let conflict = Conflict::AddAdd {
///     path: "src/new.rs".into(),
///     sides: vec![side],
/// };
///
/// let json = serde_json::to_string(&conflict).unwrap();
/// assert!(json.contains("\"type\":\"add_add\""));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Conflict {
    /// Two or more workspaces modified the same region of the same file.
    ///
    /// This is the most common conflict type. The `base` field holds the
    /// common ancestor blob OID, and each `side` holds a workspace's version.
    /// The `atoms` list localizes the conflict to specific regions.
    ///
    /// # Example
    ///
    /// Workspace `alice` edits lines 10-15 of `src/lib.rs`, workspace `bob`
    /// edits lines 12-18. The base is the original blob, sides contain each
    /// workspace's full-file blob, and atoms pinpoint lines 12-15 as the
    /// overlapping conflict region.
    Content {
        /// Path to the conflicted file (relative to repo root).
        path: PathBuf,

        /// Stable file identity (survives renames).
        file_id: FileId,

        /// Git blob OID of the common ancestor (base) version.
        ///
        /// `None` if no common ancestor exists (e.g., both sides added
        /// different content to a previously nonexistent path with the
        /// same FileId — unusual but possible after merge).
        base: Option<GitOid>,

        /// The conflicting sides (one per workspace that modified this region).
        ///
        /// Always has ≥ 2 entries. Sorted by workspace ID for determinism.
        sides: Vec<ConflictSide>,

        /// Localized conflict regions within the file.
        ///
        /// May be empty if region-level granularity is not yet computed
        /// (e.g., binary files or pre-atom analysis).
        atoms: Vec<ConflictAtom>,
    },

    /// Two or more workspaces independently added a file at the same path
    /// with different content.
    ///
    /// Unlike `Content` conflicts, there is no common base — the file did
    /// not exist before. Each side contains the independently added content.
    ///
    /// # Example
    ///
    /// Workspace `alice` creates `src/util.rs` with helper functions.
    /// Workspace `bob` also creates `src/util.rs` with different helpers.
    /// Both are add operations with no shared ancestor.
    AddAdd {
        /// Path where the file was independently added.
        path: PathBuf,

        /// The conflicting sides (one per workspace that added this path).
        ///
        /// Always has ≥ 2 entries. Sorted by workspace ID for determinism.
        sides: Vec<ConflictSide>,
    },

    /// One workspace modified a file while another deleted it.
    ///
    /// This conflict requires a human/agent decision: keep the modified
    /// version, accept the deletion, or do something else entirely.
    ///
    /// # Example
    ///
    /// Workspace `alice` refactors `src/old.rs` (modifies it).
    /// Workspace `bob` deletes `src/old.rs` as part of a cleanup.
    /// The merge engine cannot decide which intent should win.
    ModifyDelete {
        /// Path to the file that was both modified and deleted.
        path: PathBuf,

        /// Stable file identity.
        file_id: FileId,

        /// The workspace that modified the file (the "modify" side).
        modifier: ConflictSide,

        /// The workspace that deleted the file (the "delete" side).
        ///
        /// The `content` field of this side holds the last known blob OID
        /// before deletion.
        deleter: ConflictSide,

        /// Git blob OID of the modified file content.
        ///
        /// This is the content from the `modifier` side, provided separately
        /// for convenience so resolvers can inspect it without dereferencing
        /// the side's content OID.
        modified_content: GitOid,
    },

    /// The same file was renamed to different destinations by different
    /// workspaces.
    ///
    /// The FileId is the same across all sides — only the destination paths
    /// differ. Content may or may not have changed.
    ///
    /// # Example
    ///
    /// File `src/util.rs` (FileId=X) is renamed:
    /// - Workspace `alice` renames to `src/helpers.rs`
    /// - Workspace `bob` renames to `src/common.rs`
    ///
    /// Both operations share the same FileId, but the destinations diverge.
    DivergentRename {
        /// Stable file identity (same across all sides).
        file_id: FileId,

        /// Original path before any rename.
        original: PathBuf,

        /// The divergent rename destinations (one per workspace).
        ///
        /// Each entry is `(destination_path, conflict_side)`.
        /// Sorted by destination path for determinism.
        destinations: Vec<(PathBuf, ConflictSide)>,
    },
}

impl Conflict {
    /// Return the primary path associated with this conflict.
    ///
    /// For `DivergentRename`, returns the original path.
    /// For all other variants, returns the conflict path.
    #[must_use]
    pub fn path(&self) -> &PathBuf {
        match self {
            Self::Content { path, .. }
            | Self::AddAdd { path, .. }
            | Self::ModifyDelete { path, .. } => path,
            Self::DivergentRename { original, .. } => original,
        }
    }

    /// Return the conflict variant name as a static string.
    #[must_use]
    pub fn variant_name(&self) -> &'static str {
        match self {
            Self::Content { .. } => "content",
            Self::AddAdd { .. } => "add_add",
            Self::ModifyDelete { .. } => "modify_delete",
            Self::DivergentRename { .. } => "divergent_rename",
        }
    }

    /// Return the number of sides involved in this conflict.
    #[must_use]
    pub fn side_count(&self) -> usize {
        match self {
            Self::Content { sides, .. } | Self::AddAdd { sides, .. } => sides.len(),
            Self::ModifyDelete { .. } => 2,
            Self::DivergentRename { destinations, .. } => destinations.len(),
        }
    }

    /// Return all workspace names involved in this conflict.
    #[must_use]
    pub fn workspaces(&self) -> Vec<&str> {
        match self {
            Self::Content { sides, .. } | Self::AddAdd { sides, .. } => {
                sides.iter().map(|s| s.workspace.as_str()).collect()
            }
            Self::ModifyDelete {
                modifier, deleter, ..
            } => vec![modifier.workspace.as_str(), deleter.workspace.as_str()],
            Self::DivergentRename { destinations, .. } => {
                destinations.iter().map(|(_, s)| s.workspace.as_str()).collect()
            }
        }
    }
}

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Content {
                path, sides, atoms, ..
            } => {
                let ws: Vec<_> = sides.iter().map(|s| s.workspace.as_str()).collect();
                write!(
                    f,
                    "content conflict in {} between [{}] ({} atom(s))",
                    path.display(),
                    ws.join(", "),
                    atoms.len()
                )
            }
            Self::AddAdd { path, sides } => {
                let ws: Vec<_> = sides.iter().map(|s| s.workspace.as_str()).collect();
                write!(
                    f,
                    "add/add conflict at {} between [{}]",
                    path.display(),
                    ws.join(", ")
                )
            }
            Self::ModifyDelete {
                path,
                modifier,
                deleter,
                ..
            } => {
                write!(
                    f,
                    "modify/delete conflict on {}: {} modified, {} deleted",
                    path.display(),
                    modifier.workspace,
                    deleter.workspace
                )
            }
            Self::DivergentRename {
                original,
                destinations,
                ..
            } => {
                let dests: Vec<_> = destinations
                    .iter()
                    .map(|(p, s)| format!("{} → {}", s.workspace, p.display()))
                    .collect();
                write!(
                    f,
                    "divergent rename of {}: [{}]",
                    original.display(),
                    dests.join(", ")
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::EpochId;

    // Helper to create a test GitOid
    fn test_oid(c: char) -> GitOid {
        GitOid::new(&c.to_string().repeat(40)).unwrap()
    }

    // Helper to create a test FileId
    fn test_file_id(val: u128) -> FileId {
        FileId::new(val)
    }

    // Helper to create a test OrderingKey
    fn test_ordering_key(ws: &str, seq: u64) -> OrderingKey {
        let epoch = EpochId::new(&"e".repeat(40)).unwrap();
        OrderingKey::new(epoch, ws.parse().unwrap(), seq, 1_700_000_000_000)
    }

    // Helper to create a test ConflictSide
    fn test_side(ws: &str, oid_char: char, seq: u64) -> ConflictSide {
        ConflictSide::new(ws.into(), test_oid(oid_char), test_ordering_key(ws, seq))
    }

    // -----------------------------------------------------------------------
    // ConflictSide
    // -----------------------------------------------------------------------

    #[test]
    fn conflict_side_construction() {
        let side = test_side("alice", 'a', 1);
        assert_eq!(side.workspace, "alice");
        assert_eq!(side.content, test_oid('a'));
        assert_eq!(side.timestamp.seq, 1);
    }

    #[test]
    fn conflict_side_serde_roundtrip() {
        let side = test_side("bob", 'b', 42);
        let json = serde_json::to_string(&side).unwrap();
        let decoded: ConflictSide = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.workspace, side.workspace);
        assert_eq!(decoded.content, side.content);
        assert_eq!(decoded.timestamp.seq, side.timestamp.seq);
    }

    // -----------------------------------------------------------------------
    // ConflictAtom
    // -----------------------------------------------------------------------

    #[test]
    fn conflict_atom_construction() {
        let atom = ConflictAtom::new("lines 10-15 overlap");
        assert_eq!(atom.description, "lines 10-15 overlap");
    }

    #[test]
    fn conflict_atom_serde_roundtrip() {
        let atom = ConflictAtom::new("function signature diverged");
        let json = serde_json::to_string(&atom).unwrap();
        let decoded: ConflictAtom = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, atom);
    }

    // -----------------------------------------------------------------------
    // Conflict::Content
    // -----------------------------------------------------------------------

    #[test]
    fn content_conflict_with_base() {
        let conflict = Conflict::Content {
            path: "src/lib.rs".into(),
            file_id: test_file_id(1),
            base: Some(test_oid('0')),
            sides: vec![test_side("alice", 'a', 1), test_side("bob", 'b', 2)],
            atoms: vec![ConflictAtom::new("lines 10-15")],
        };

        assert_eq!(conflict.path(), &PathBuf::from("src/lib.rs"));
        assert_eq!(conflict.variant_name(), "content");
        assert_eq!(conflict.side_count(), 2);
        assert_eq!(conflict.workspaces(), vec!["alice", "bob"]);
    }

    #[test]
    fn content_conflict_without_base() {
        let conflict = Conflict::Content {
            path: "src/new.rs".into(),
            file_id: test_file_id(2),
            base: None,
            sides: vec![test_side("ws-1", 'a', 1), test_side("ws-2", 'b', 1)],
            atoms: vec![],
        };

        if let Conflict::Content { base, atoms, .. } = &conflict {
            assert!(base.is_none());
            assert!(atoms.is_empty());
        } else {
            panic!("expected Content variant");
        }
    }

    #[test]
    fn content_conflict_three_way() {
        let conflict = Conflict::Content {
            path: "README.md".into(),
            file_id: test_file_id(3),
            base: Some(test_oid('0')),
            sides: vec![
                test_side("alice", 'a', 1),
                test_side("bob", 'b', 2),
                test_side("carol", 'c', 3),
            ],
            atoms: vec![
                ConflictAtom::new("header section"),
                ConflictAtom::new("footer section"),
            ],
        };

        assert_eq!(conflict.side_count(), 3);
        assert_eq!(conflict.workspaces(), vec!["alice", "bob", "carol"]);
    }

    #[test]
    fn content_conflict_serde_roundtrip() {
        let conflict = Conflict::Content {
            path: "src/main.rs".into(),
            file_id: test_file_id(10),
            base: Some(test_oid('0')),
            sides: vec![test_side("alice", 'a', 1), test_side("bob", 'b', 2)],
            atoms: vec![ConflictAtom::new("imports block")],
        };

        let json = serde_json::to_string_pretty(&conflict).unwrap();
        assert!(json.contains("\"type\": \"content\""));
        assert!(json.contains("\"path\": \"src/main.rs\""));

        let decoded: Conflict = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.variant_name(), "content");
        assert_eq!(decoded.path(), &PathBuf::from("src/main.rs"));
    }

    #[test]
    fn content_conflict_json_tag() {
        let conflict = Conflict::Content {
            path: "a.txt".into(),
            file_id: test_file_id(99),
            base: None,
            sides: vec![test_side("ws-1", 'a', 1), test_side("ws-2", 'b', 1)],
            atoms: vec![],
        };
        let json = serde_json::to_string(&conflict).unwrap();
        assert!(json.contains("\"type\":\"content\""));
    }

    // -----------------------------------------------------------------------
    // Conflict::AddAdd
    // -----------------------------------------------------------------------

    #[test]
    fn add_add_conflict() {
        let conflict = Conflict::AddAdd {
            path: "src/util.rs".into(),
            sides: vec![test_side("alice", 'a', 1), test_side("bob", 'b', 1)],
        };

        assert_eq!(conflict.path(), &PathBuf::from("src/util.rs"));
        assert_eq!(conflict.variant_name(), "add_add");
        assert_eq!(conflict.side_count(), 2);
        assert_eq!(conflict.workspaces(), vec!["alice", "bob"]);
    }

    #[test]
    fn add_add_conflict_serde_roundtrip() {
        let conflict = Conflict::AddAdd {
            path: "new-file.txt".into(),
            sides: vec![test_side("ws-a", 'a', 5), test_side("ws-b", 'b', 3)],
        };

        let json = serde_json::to_string(&conflict).unwrap();
        assert!(json.contains("\"type\":\"add_add\""));

        let decoded: Conflict = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.variant_name(), "add_add");
    }

    // -----------------------------------------------------------------------
    // Conflict::ModifyDelete
    // -----------------------------------------------------------------------

    #[test]
    fn modify_delete_conflict() {
        let conflict = Conflict::ModifyDelete {
            path: "src/old.rs".into(),
            file_id: test_file_id(42),
            modifier: test_side("alice", 'a', 5),
            deleter: test_side("bob", 'b', 6),
            modified_content: test_oid('a'),
        };

        assert_eq!(conflict.path(), &PathBuf::from("src/old.rs"));
        assert_eq!(conflict.variant_name(), "modify_delete");
        assert_eq!(conflict.side_count(), 2);
        assert_eq!(conflict.workspaces(), vec!["alice", "bob"]);
    }

    #[test]
    fn modify_delete_conflict_serde_roundtrip() {
        let conflict = Conflict::ModifyDelete {
            path: "docs/api.md".into(),
            file_id: test_file_id(100),
            modifier: test_side("dev-1", 'a', 10),
            deleter: test_side("dev-2", 'b', 11),
            modified_content: test_oid('a'),
        };

        let json = serde_json::to_string_pretty(&conflict).unwrap();
        assert!(json.contains("\"type\": \"modify_delete\""));

        let decoded: Conflict = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.variant_name(), "modify_delete");
        if let Conflict::ModifyDelete {
            modifier, deleter, ..
        } = &decoded
        {
            assert_eq!(modifier.workspace, "dev-1");
            assert_eq!(deleter.workspace, "dev-2");
        }
    }

    // -----------------------------------------------------------------------
    // Conflict::DivergentRename
    // -----------------------------------------------------------------------

    #[test]
    fn divergent_rename_conflict() {
        let conflict = Conflict::DivergentRename {
            file_id: test_file_id(77),
            original: "src/util.rs".into(),
            destinations: vec![
                ("src/helpers.rs".into(), test_side("alice", 'a', 1)),
                ("src/common.rs".into(), test_side("bob", 'b', 1)),
            ],
        };

        assert_eq!(conflict.path(), &PathBuf::from("src/util.rs"));
        assert_eq!(conflict.variant_name(), "divergent_rename");
        assert_eq!(conflict.side_count(), 2);
        assert_eq!(conflict.workspaces(), vec!["alice", "bob"]);
    }

    #[test]
    fn divergent_rename_three_way() {
        let conflict = Conflict::DivergentRename {
            file_id: test_file_id(88),
            original: "old.rs".into(),
            destinations: vec![
                ("new-a.rs".into(), test_side("ws-1", 'a', 1)),
                ("new-b.rs".into(), test_side("ws-2", 'b', 1)),
                ("new-c.rs".into(), test_side("ws-3", 'c', 1)),
            ],
        };

        assert_eq!(conflict.side_count(), 3);
        assert_eq!(conflict.workspaces(), vec!["ws-1", "ws-2", "ws-3"]);
    }

    #[test]
    fn divergent_rename_serde_roundtrip() {
        let conflict = Conflict::DivergentRename {
            file_id: test_file_id(55),
            original: "src/old.rs".into(),
            destinations: vec![
                ("src/new-a.rs".into(), test_side("alice", 'a', 3)),
                ("src/new-b.rs".into(), test_side("bob", 'b', 4)),
            ],
        };

        let json = serde_json::to_string(&conflict).unwrap();
        assert!(json.contains("\"type\":\"divergent_rename\""));

        let decoded: Conflict = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.variant_name(), "divergent_rename");
    }

    // -----------------------------------------------------------------------
    // Display
    // -----------------------------------------------------------------------

    #[test]
    fn display_content_conflict() {
        let conflict = Conflict::Content {
            path: "src/lib.rs".into(),
            file_id: test_file_id(1),
            base: Some(test_oid('0')),
            sides: vec![test_side("alice", 'a', 1), test_side("bob", 'b', 2)],
            atoms: vec![ConflictAtom::new("line 10")],
        };
        let display = format!("{conflict}");
        assert!(display.contains("content conflict in src/lib.rs"));
        assert!(display.contains("alice"));
        assert!(display.contains("bob"));
        assert!(display.contains("1 atom(s)"));
    }

    #[test]
    fn display_add_add_conflict() {
        let conflict = Conflict::AddAdd {
            path: "new.rs".into(),
            sides: vec![test_side("ws-1", 'a', 1), test_side("ws-2", 'b', 1)],
        };
        let display = format!("{conflict}");
        assert!(display.contains("add/add conflict at new.rs"));
    }

    #[test]
    fn display_modify_delete_conflict() {
        let conflict = Conflict::ModifyDelete {
            path: "gone.rs".into(),
            file_id: test_file_id(9),
            modifier: test_side("alice", 'a', 1),
            deleter: test_side("bob", 'b', 2),
            modified_content: test_oid('a'),
        };
        let display = format!("{conflict}");
        assert!(display.contains("modify/delete"));
        assert!(display.contains("alice modified"));
        assert!(display.contains("bob deleted"));
    }

    #[test]
    fn display_divergent_rename_conflict() {
        let conflict = Conflict::DivergentRename {
            file_id: test_file_id(7),
            original: "old.rs".into(),
            destinations: vec![
                ("new-a.rs".into(), test_side("alice", 'a', 1)),
                ("new-b.rs".into(), test_side("bob", 'b', 1)),
            ],
        };
        let display = format!("{conflict}");
        assert!(display.contains("divergent rename of old.rs"));
        assert!(display.contains("alice → new-a.rs"));
        assert!(display.contains("bob → new-b.rs"));
    }

    // -----------------------------------------------------------------------
    // Cross-variant serialization
    // -----------------------------------------------------------------------

    #[test]
    fn all_variants_deserialize_from_json() {
        let variants = vec![
            Conflict::Content {
                path: "a.rs".into(),
                file_id: test_file_id(1),
                base: Some(test_oid('0')),
                sides: vec![test_side("ws-1", 'a', 1), test_side("ws-2", 'b', 1)],
                atoms: vec![],
            },
            Conflict::AddAdd {
                path: "b.rs".into(),
                sides: vec![test_side("ws-1", 'a', 1), test_side("ws-2", 'b', 1)],
            },
            Conflict::ModifyDelete {
                path: "c.rs".into(),
                file_id: test_file_id(2),
                modifier: test_side("ws-1", 'a', 1),
                deleter: test_side("ws-2", 'b', 1),
                modified_content: test_oid('a'),
            },
            Conflict::DivergentRename {
                file_id: test_file_id(3),
                original: "d.rs".into(),
                destinations: vec![
                    ("e.rs".into(), test_side("ws-1", 'a', 1)),
                    ("f.rs".into(), test_side("ws-2", 'b', 1)),
                ],
            },
        ];

        for variant in &variants {
            let json = serde_json::to_string(variant).unwrap();
            let decoded: Conflict = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded.variant_name(), variant.variant_name());
        }
    }

    #[test]
    fn conflict_json_keys_are_snake_case() {
        let conflict = Conflict::ModifyDelete {
            path: "test.rs".into(),
            file_id: test_file_id(1),
            modifier: test_side("ws-1", 'a', 1),
            deleter: test_side("ws-2", 'b', 1),
            modified_content: test_oid('a'),
        };
        let json = serde_json::to_string(&conflict).unwrap();
        assert!(json.contains("\"modified_content\""));
        assert!(json.contains("\"file_id\""));
        assert!(!json.contains("\"modifiedContent\""));
    }

    #[test]
    fn variant_name_matches_serde_tag() {
        let cases: Vec<(Conflict, &str)> = vec![
            (
                Conflict::Content {
                    path: "a.rs".into(),
                    file_id: test_file_id(1),
                    base: None,
                    sides: vec![test_side("ws-1", 'a', 1), test_side("ws-2", 'b', 1)],
                    atoms: vec![],
                },
                "content",
            ),
            (
                Conflict::AddAdd {
                    path: "b.rs".into(),
                    sides: vec![test_side("ws-1", 'a', 1), test_side("ws-2", 'b', 1)],
                },
                "add_add",
            ),
            (
                Conflict::ModifyDelete {
                    path: "c.rs".into(),
                    file_id: test_file_id(2),
                    modifier: test_side("ws-1", 'a', 1),
                    deleter: test_side("ws-2", 'b', 1),
                    modified_content: test_oid('a'),
                },
                "modify_delete",
            ),
            (
                Conflict::DivergentRename {
                    file_id: test_file_id(3),
                    original: "d.rs".into(),
                    destinations: vec![("e.rs".into(), test_side("ws-1", 'a', 1))],
                },
                "divergent_rename",
            ),
        ];

        for (conflict, expected_name) in cases {
            assert_eq!(conflict.variant_name(), expected_name);
            let json = serde_json::to_string(&conflict).unwrap();
            let expected_tag = format!("\"type\":\"{expected_name}\"");
            assert!(
                json.contains(&expected_tag),
                "JSON should contain {expected_tag}, got: {json}"
            );
        }
    }
}
