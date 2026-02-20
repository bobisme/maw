//! Structured conflict model — variant types, localization, and serialization (§5.7, §6.4).
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
//! # Localization Types
//!
//! | Type | Description |
//! |------|-------------|
//! | [`Region`] | Where in a file: line ranges, AST nodes, or whole file |
//! | [`ConflictReason`] | Why the conflict occurred: overlapping edits, same AST node, etc. |
//! | [`AtomEdit`] | One workspace's contribution to a conflict region |
//! | [`ConflictAtom`] | A localized conflict with base region, edits, and reason |
//!
//! # Serialization
//!
//! All types use tagged JSON for clean, agent-parseable output:
//!
//! ```json
//! {
//!   "type": "content",
//!   "path": "src/lib.rs",
//!   "file_id": "00000000000000000000000000000001",
//!   "base": "aaaa...",
//!   "sides": [...],
//!   "atoms": [{
//!     "base_region": { "kind": "lines", "start": 42, "end": 67 },
//!     "edits": [
//!       { "workspace": "alice", "region": { "kind": "lines", "start": 42, "end": 55 }, "content": "..." },
//!       { "workspace": "bob", "region": { "kind": "lines", "start": 50, "end": 67 }, "content": "..." }
//!     ],
//!     "reason": { "reason": "overlapping_line_edits", "description": "Both sides edited lines 42-67" }
//!   }]
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
// Region — localization of a conflict within a file
// ---------------------------------------------------------------------------

/// A region within a file that participates in a conflict.
///
/// Regions localize conflicts to specific parts of a file — either line ranges
/// or AST node spans. This is the difference between "file has conflict" and
/// "lines 42-67 of function process_order have a conflict."
///
/// # Serialization
///
/// Uses `#[serde(tag = "kind")]` with snake_case variant names:
///
/// ```json
/// { "kind": "lines", "start": 42, "end": 67 }
/// { "kind": "ast_node", "node_kind": "function", "name": "process_order", "start_byte": 1024, "end_byte": 2048 }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Region {
    /// A contiguous range of lines (1-indexed, inclusive start, exclusive end).
    ///
    /// # Example
    ///
    /// `Region::Lines { start: 10, end: 15 }` means lines 10..15.
    Lines {
        /// First line of the region (1-indexed, inclusive).
        start: u32,
        /// One past the last line of the region (exclusive).
        end: u32,
    },

    /// An AST node identified by tree-sitter node kind and optional name.
    ///
    /// Used when the merge engine has parsed the file and can identify
    /// conflicts at the syntax-tree level rather than raw line ranges.
    ///
    /// # Example
    ///
    /// ```text
    /// AstNode { node_kind: "function_item", name: Some("process_order"), start_byte: 1024, end_byte: 2048 }
    /// ```
    AstNode {
        /// The tree-sitter node kind (e.g., "function_item", "struct_item").
        node_kind: String,
        /// The name of the node if available (e.g., function name, struct name).
        name: Option<String>,
        /// Start byte offset in the file (0-indexed).
        start_byte: u32,
        /// End byte offset in the file (exclusive).
        end_byte: u32,
    },

    /// The entire file (used when region-level granularity is not available).
    WholeFile,
}

impl Region {
    /// Create a line-based region.
    #[must_use]
    pub fn lines(start: u32, end: u32) -> Self {
        Self::Lines { start, end }
    }

    /// Create an AST-node region.
    #[must_use]
    pub fn ast_node(
        node_kind: impl Into<String>,
        name: Option<String>,
        start_byte: u32,
        end_byte: u32,
    ) -> Self {
        Self::AstNode {
            node_kind: node_kind.into(),
            name,
            start_byte,
            end_byte,
        }
    }

    /// Create a whole-file region.
    #[must_use]
    pub fn whole_file() -> Self {
        Self::WholeFile
    }

    /// Return a human-readable summary of this region.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::Lines { start, end } => format!("lines {start}..{end}"),
            Self::AstNode {
                node_kind, name, ..
            } => match name {
                Some(n) => format!("{node_kind} `{n}`"),
                None => format!("{node_kind}"),
            },
            Self::WholeFile => "whole file".to_string(),
        }
    }
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.summary())
    }
}

// ---------------------------------------------------------------------------
// ConflictReason — why a conflict occurred
// ---------------------------------------------------------------------------

/// Explains why a specific conflict region could not be auto-merged.
///
/// Agents use this to decide resolution strategy. For example,
/// `OverlappingLineEdits` suggests a line-level diff3 resolution might work,
/// while `SameAstNodeModified` suggests looking at the AST structure.
///
/// # Serialization
///
/// Uses `#[serde(tag = "reason")]` with snake_case variant names:
///
/// ```json
/// { "reason": "overlapping_line_edits", "description": "Both sides edited lines 42-67" }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum ConflictReason {
    /// Two or more workspaces edited overlapping line ranges.
    ///
    /// This is the most common conflict reason. The line ranges from each
    /// side overlap, making a clean merge impossible without human/agent input.
    OverlappingLineEdits {
        /// Human-readable description of the overlap.
        description: String,
    },

    /// Two or more workspaces modified the same AST node.
    ///
    /// Even if the exact line ranges don't overlap, modifying the same
    /// function or struct from multiple workspaces requires review.
    SameAstNodeModified {
        /// Human-readable description of which AST node is affected.
        description: String,
    },

    /// The edits are non-commutative — applying them in different orders
    /// produces different results.
    ///
    /// This is the formal CRDT-theory reason for a conflict. It subsumes
    /// the other reasons but is used when no more specific reason applies.
    NonCommutativeEdits {
        /// Human-readable description.
        description: String,
    },

    /// A custom reason not covered by the predefined variants.
    ///
    /// Used by custom merge drivers or specialized analysis tools.
    Custom {
        /// The custom reason string.
        description: String,
    },
}

impl ConflictReason {
    /// Create an overlapping line edits reason.
    #[must_use]
    pub fn overlapping(description: impl Into<String>) -> Self {
        Self::OverlappingLineEdits {
            description: description.into(),
        }
    }

    /// Create a same-AST-node-modified reason.
    #[must_use]
    pub fn same_ast_node(description: impl Into<String>) -> Self {
        Self::SameAstNodeModified {
            description: description.into(),
        }
    }

    /// Create a non-commutative edits reason.
    #[must_use]
    pub fn non_commutative(description: impl Into<String>) -> Self {
        Self::NonCommutativeEdits {
            description: description.into(),
        }
    }

    /// Create a custom reason.
    #[must_use]
    pub fn custom(description: impl Into<String>) -> Self {
        Self::Custom {
            description: description.into(),
        }
    }

    /// Return the human-readable description.
    #[must_use]
    pub fn description(&self) -> &str {
        match self {
            Self::OverlappingLineEdits { description }
            | Self::SameAstNodeModified { description }
            | Self::NonCommutativeEdits { description }
            | Self::Custom { description } => description,
        }
    }

    /// Return the reason variant name as a static string.
    #[must_use]
    pub fn variant_name(&self) -> &'static str {
        match self {
            Self::OverlappingLineEdits { .. } => "overlapping_line_edits",
            Self::SameAstNodeModified { .. } => "same_ast_node_modified",
            Self::NonCommutativeEdits { .. } => "non_commutative_edits",
            Self::Custom { .. } => "custom",
        }
    }
}

impl fmt::Display for ConflictReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.description())
    }
}

// ---------------------------------------------------------------------------
// AtomEdit — one workspace's contribution to a conflict atom
// ---------------------------------------------------------------------------

/// A single workspace's edit within a conflict atom.
///
/// Each `AtomEdit` represents what one workspace did to a conflicted region.
/// The collection of `AtomEdit`s within a `ConflictAtom` shows all the
/// divergent changes that produced the conflict.
///
/// # Example
///
/// Workspace `alice` replaced lines 10-15 with new code. Workspace `bob`
/// replaced lines 12-18 with different code. Both edits would appear as
/// `AtomEdit` entries within the same `ConflictAtom`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AtomEdit {
    /// The workspace that made this edit.
    pub workspace: String,

    /// The region in the workspace's version of the file where the edit lands.
    pub region: Region,

    /// The text content of the edit (the new content from this workspace).
    ///
    /// May be empty for deletions.
    pub content: String,
}

impl AtomEdit {
    /// Create a new atom edit.
    #[must_use]
    pub fn new(workspace: impl Into<String>, region: Region, content: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            region,
            content: content.into(),
        }
    }
}

impl fmt::Display for AtomEdit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let content_preview = if self.content.len() > 40 {
            format!("{}...", &self.content[..40])
        } else {
            self.content.clone()
        };
        write!(
            f,
            "{} @ {}: {:?}",
            self.workspace, self.region, content_preview
        )
    }
}

// ---------------------------------------------------------------------------
// ConflictAtom — localized conflict region with edits and reason
// ---------------------------------------------------------------------------

/// A localized conflict region within a file.
///
/// A `ConflictAtom` pinpoints exactly WHERE a conflict occurs and WHY it
/// cannot be auto-merged. It carries the base region (in the common ancestor),
/// each workspace's edit to that region, and the reason for the conflict.
///
/// # Design philosophy
///
/// From §5.7: "An agent receiving 'two edits are non-commutative because both
/// modify AST node process_order at lines 42-67' can resolve the conflict
/// surgically. An agent receiving 'file has conflict' with marker soup cannot."
///
/// # Example JSON
///
/// ```json
/// {
///   "base_region": { "kind": "lines", "start": 42, "end": 67 },
///   "edits": [
///     { "workspace": "alice", "region": { "kind": "lines", "start": 42, "end": 55 }, "content": "fn process_order(..." },
///     { "workspace": "bob", "region": { "kind": "lines", "start": 50, "end": 67 }, "content": "fn process_order(..." }
///   ],
///   "reason": { "reason": "overlapping_line_edits", "description": "Both sides edited lines 42-67" }
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictAtom {
    /// The region in the base (common ancestor) version where the conflict occurs.
    pub base_region: Region,

    /// Each workspace's edit to the conflicted region.
    ///
    /// Always has ≥ 2 entries (otherwise it wouldn't be a conflict).
    /// Sorted by workspace name for deterministic output.
    pub edits: Vec<AtomEdit>,

    /// Why this region could not be auto-merged.
    pub reason: ConflictReason,
}

impl ConflictAtom {
    /// Create a new conflict atom.
    #[must_use]
    pub fn new(base_region: Region, edits: Vec<AtomEdit>, reason: ConflictReason) -> Self {
        Self {
            base_region,
            edits,
            reason,
        }
    }

    /// Create a simple line-overlap conflict atom (convenience constructor).
    #[must_use]
    pub fn line_overlap(
        start: u32,
        end: u32,
        edits: Vec<AtomEdit>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            base_region: Region::lines(start, end),
            edits,
            reason: ConflictReason::overlapping(description),
        }
    }

    /// Return a human-readable summary of this atom.
    #[must_use]
    pub fn summary(&self) -> String {
        let ws: Vec<_> = self.edits.iter().map(|e| e.workspace.as_str()).collect();
        format!(
            "{} — {} [{}]",
            self.base_region.summary(),
            self.reason,
            ws.join(", ")
        )
    }
}

impl fmt::Display for ConflictAtom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.summary())
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

    // Helper to create a simple test ConflictAtom with a description
    fn test_atom(desc: &str) -> ConflictAtom {
        ConflictAtom::line_overlap(
            1, 10,
            vec![
                AtomEdit::new("ws-1", Region::lines(1, 5), "side-1"),
                AtomEdit::new("ws-2", Region::lines(5, 10), "side-2"),
            ],
            desc,
        )
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
    // Region
    // -----------------------------------------------------------------------

    #[test]
    fn region_lines_construction() {
        let r = Region::lines(10, 15);
        assert_eq!(r.summary(), "lines 10..15");
        assert_eq!(format!("{r}"), "lines 10..15");
    }

    #[test]
    fn region_ast_node_with_name() {
        let r = Region::ast_node("function_item", Some("process_order".into()), 1024, 2048);
        assert_eq!(r.summary(), "function_item `process_order`");
    }

    #[test]
    fn region_ast_node_without_name() {
        let r = Region::ast_node("struct_item", None, 0, 100);
        assert_eq!(r.summary(), "struct_item");
    }

    #[test]
    fn region_whole_file() {
        let r = Region::whole_file();
        assert_eq!(r.summary(), "whole file");
    }

    #[test]
    fn region_lines_serde_roundtrip() {
        let r = Region::lines(42, 67);
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"kind\":\"lines\""));
        assert!(json.contains("\"start\":42"));
        assert!(json.contains("\"end\":67"));
        let decoded: Region = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, r);
    }

    #[test]
    fn region_ast_node_serde_roundtrip() {
        let r = Region::ast_node("function_item", Some("foo".into()), 100, 200);
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"kind\":\"ast_node\""));
        assert!(json.contains("\"node_kind\":\"function_item\""));
        let decoded: Region = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, r);
    }

    #[test]
    fn region_whole_file_serde_roundtrip() {
        let r = Region::whole_file();
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"kind\":\"whole_file\""));
        let decoded: Region = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, r);
    }

    // -----------------------------------------------------------------------
    // ConflictReason
    // -----------------------------------------------------------------------

    #[test]
    fn conflict_reason_overlapping() {
        let r = ConflictReason::overlapping("lines 10-15 overlap in both sides");
        assert_eq!(r.variant_name(), "overlapping_line_edits");
        assert_eq!(r.description(), "lines 10-15 overlap in both sides");
    }

    #[test]
    fn conflict_reason_same_ast_node() {
        let r = ConflictReason::same_ast_node("function `foo` modified by both");
        assert_eq!(r.variant_name(), "same_ast_node_modified");
    }

    #[test]
    fn conflict_reason_non_commutative() {
        let r = ConflictReason::non_commutative("edits produce different results in different order");
        assert_eq!(r.variant_name(), "non_commutative_edits");
    }

    #[test]
    fn conflict_reason_custom() {
        let r = ConflictReason::custom("custom driver reported conflict");
        assert_eq!(r.variant_name(), "custom");
        assert_eq!(r.description(), "custom driver reported conflict");
    }

    #[test]
    fn conflict_reason_serde_roundtrip() {
        let reasons = vec![
            ConflictReason::overlapping("overlap"),
            ConflictReason::same_ast_node("ast"),
            ConflictReason::non_commutative("non-comm"),
            ConflictReason::custom("custom"),
        ];
        for reason in &reasons {
            let json = serde_json::to_string(reason).unwrap();
            let decoded: ConflictReason = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded.variant_name(), reason.variant_name());
            assert_eq!(decoded.description(), reason.description());
        }
    }

    #[test]
    fn conflict_reason_display() {
        let r = ConflictReason::overlapping("test display");
        assert_eq!(format!("{r}"), "test display");
    }

    // -----------------------------------------------------------------------
    // AtomEdit
    // -----------------------------------------------------------------------

    #[test]
    fn atom_edit_construction() {
        let edit = AtomEdit::new("alice", Region::lines(10, 15), "fn foo() {}");
        assert_eq!(edit.workspace, "alice");
        assert_eq!(edit.region, Region::lines(10, 15));
        assert_eq!(edit.content, "fn foo() {}");
    }

    #[test]
    fn atom_edit_serde_roundtrip() {
        let edit = AtomEdit::new("bob", Region::lines(20, 30), "new code here");
        let json = serde_json::to_string(&edit).unwrap();
        let decoded: AtomEdit = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, edit);
    }

    #[test]
    fn atom_edit_display_short_content() {
        let edit = AtomEdit::new("ws-1", Region::lines(1, 5), "short");
        let display = format!("{edit}");
        assert!(display.contains("ws-1"));
        assert!(display.contains("lines 1..5"));
    }

    #[test]
    fn atom_edit_display_long_content_truncated() {
        let long = "a".repeat(100);
        let edit = AtomEdit::new("ws-1", Region::lines(1, 5), long);
        let display = format!("{edit}");
        assert!(display.contains("..."));
    }

    // -----------------------------------------------------------------------
    // ConflictAtom
    // -----------------------------------------------------------------------

    #[test]
    fn conflict_atom_construction() {
        let atom = ConflictAtom::new(
            Region::lines(10, 15),
            vec![
                AtomEdit::new("alice", Region::lines(10, 13), "alice's code"),
                AtomEdit::new("bob", Region::lines(12, 15), "bob's code"),
            ],
            ConflictReason::overlapping("lines 10-15 overlap"),
        );
        assert_eq!(atom.base_region, Region::lines(10, 15));
        assert_eq!(atom.edits.len(), 2);
        assert_eq!(atom.reason.variant_name(), "overlapping_line_edits");
    }

    #[test]
    fn conflict_atom_line_overlap_convenience() {
        let atom = ConflictAtom::line_overlap(
            42,
            67,
            vec![
                AtomEdit::new("ws-1", Region::lines(42, 55), "code-1"),
                AtomEdit::new("ws-2", Region::lines(50, 67), "code-2"),
            ],
            "Both sides edited lines 42-67",
        );
        assert_eq!(atom.base_region, Region::lines(42, 67));
        assert_eq!(atom.reason.variant_name(), "overlapping_line_edits");
    }

    #[test]
    fn conflict_atom_serde_roundtrip() {
        let atom = ConflictAtom::new(
            Region::lines(1, 10),
            vec![
                AtomEdit::new("ws-a", Region::lines(1, 5), "alpha"),
                AtomEdit::new("ws-b", Region::lines(3, 10), "beta"),
            ],
            ConflictReason::overlapping("overlap at lines 3-5"),
        );
        let json = serde_json::to_string_pretty(&atom).unwrap();
        assert!(json.contains("\"base_region\""));
        assert!(json.contains("\"edits\""));
        assert!(json.contains("\"reason\""));

        let decoded: ConflictAtom = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, atom);
    }

    #[test]
    fn conflict_atom_with_ast_region() {
        let atom = ConflictAtom::new(
            Region::ast_node("function_item", Some("process_order".into()), 1024, 2048),
            vec![
                AtomEdit::new("alice", Region::ast_node("function_item", Some("process_order".into()), 1024, 1800), "alice version"),
                AtomEdit::new("bob", Region::ast_node("function_item", Some("process_order".into()), 1024, 1900), "bob version"),
            ],
            ConflictReason::same_ast_node("function `process_order` modified by both"),
        );
        assert_eq!(atom.summary(), "function_item `process_order` — function `process_order` modified by both [alice, bob]");
    }

    #[test]
    fn conflict_atom_summary() {
        let atom = ConflictAtom::line_overlap(
            10,
            20,
            vec![
                AtomEdit::new("ws-1", Region::lines(10, 15), ""),
                AtomEdit::new("ws-2", Region::lines(12, 20), ""),
            ],
            "overlap",
        );
        let summary = atom.summary();
        assert!(summary.contains("lines 10..20"));
        assert!(summary.contains("overlap"));
        assert!(summary.contains("ws-1"));
        assert!(summary.contains("ws-2"));
    }

    #[test]
    fn conflict_atom_display() {
        let atom = ConflictAtom::line_overlap(
            1, 5,
            vec![AtomEdit::new("a", Region::lines(1, 3), "x"), AtomEdit::new("b", Region::lines(2, 5), "y")],
            "test",
        );
        let display = format!("{atom}");
        assert!(display.contains("lines 1..5"));
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
            atoms: vec![test_atom("lines 10-15")],
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
                test_atom("header section"),
                test_atom("footer section"),
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
            atoms: vec![test_atom("imports block")],
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
            atoms: vec![test_atom("line 10")],
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
