//! Core types for the N-way merge engine.
//!
//! Defines the data structures that flow through the collect → partition →
//! resolve → build pipeline.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::conflict::Conflict;
use crate::model::patch::FileId;
use crate::model::types::{EpochId, GitOid, WorkspaceId};

// ---------------------------------------------------------------------------
// ChangeKind
// ---------------------------------------------------------------------------

/// The kind of change made to a file in a workspace.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// File was newly added (did not exist at the epoch base).
    Added,
    /// File was modified (existed at the epoch base, content changed).
    Modified,
    /// File was deleted (existed at the epoch base, removed in workspace).
    Deleted,
}

impl std::fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Added => write!(f, "added"),
            Self::Modified => write!(f, "modified"),
            Self::Deleted => write!(f, "deleted"),
        }
    }
}

// ---------------------------------------------------------------------------
// FileChange
// ---------------------------------------------------------------------------

/// A single file change captured from a workspace.
///
/// For `Added` and `Modified` changes, `content` holds the new file bytes.
/// For `Deleted` changes, `content` is `None`.
///
/// `file_id` is the stable [`FileId`] assigned when the file was created. It
/// survives renames and modifications, enabling rename-aware merge (§5.8).
/// `None` if `FileId` tracking was not available at collect time.
///
/// `blob` is the git blob OID for the new content (computed via
/// `git hash-object`). Present for `Added` and `Modified` changes when the
/// collect step had access to the git repo. Enables O(1) hash-equality checks
/// in the resolve step without comparing raw bytes.
///
/// `mode` is the file's tree-entry mode (regular, executable, symlink, etc.).
/// Present when the change was extracted from a git tree diff with mode info
/// available (typically the new-side mode for Add/Modify; the old-side mode
/// for Delete). `None` when unknown (legacy / test fixture paths).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    /// Path relative to the workspace root (and to the repo root).
    pub path: PathBuf,
    /// Type of change.
    pub kind: ChangeKind,
    /// New file content (`None` for deletions).
    pub content: Option<Vec<u8>>,
    /// Stable file identity that persists across renames (§5.8).
    ///
    /// `None` only for legacy artifacts and explicit test fixtures.
    pub file_id: Option<FileId>,
    /// Git blob OID for the new content (present for Add/Modify; `None` for
    /// Delete and for changes collected without git access).
    ///
    /// When populated, the resolve step uses OID equality instead of byte
    /// comparison for hash-equality checks, which is both faster and avoids
    /// loading content into memory.
    pub blob: Option<GitOid>,
    /// File mode from the git tree entry (regular / exec / symlink /
    /// submodule / subdirectory).
    ///
    /// For Add/Modified, holds the new-side mode. For Deleted, holds the
    /// old-side mode (so downstream code can tell e.g. whether a deleted
    /// path was a symlink). `None` when the producer did not have mode
    /// information (typical for hand-constructed test fixtures).
    pub mode: Option<EntryMode>,
}

impl FileChange {
    /// Create a new `FileChange` without `FileId`, blob OID, or mode metadata.
    ///
    /// Suitable for explicit legacy/test fixtures. Production collect paths
    /// should prefer [`FileChange::with_identity`] or
    /// [`FileChange::with_mode`].
    #[must_use]
    pub const fn new(path: PathBuf, kind: ChangeKind, content: Option<Vec<u8>>) -> Self {
        Self {
            path,
            kind,
            content,
            file_id: None,
            blob: None,
            mode: None,
        }
    }

    /// Create a new `FileChange` with full identity metadata.
    ///
    /// Preferred constructor for Phase 3+ code paths where `file_id` and
    /// `blob` OID are available from the workspace's `FileId` map and git
    /// object store.
    #[must_use]
    pub const fn with_identity(
        path: PathBuf,
        kind: ChangeKind,
        content: Option<Vec<u8>>,
        file_id: Option<FileId>,
        blob: Option<GitOid>,
    ) -> Self {
        Self {
            path,
            kind,
            content,
            file_id,
            blob,
            mode: None,
        }
    }

    /// Create a new `FileChange` with full identity metadata *and* a tree
    /// entry mode.
    ///
    /// Used by the historical-diff extractor ([`crate::merge::diff_extract`])
    /// so mode information (executable bit, symlink, submodule) can flow
    /// through the merge pipeline.
    #[must_use]
    pub const fn with_mode(
        path: PathBuf,
        kind: ChangeKind,
        content: Option<Vec<u8>>,
        file_id: Option<FileId>,
        blob: Option<GitOid>,
        mode: Option<EntryMode>,
    ) -> Self {
        Self {
            path,
            kind,
            content,
            file_id,
            blob,
            mode,
        }
    }

    /// Returns `true` if this change is a deletion.
    #[must_use]
    pub const fn is_deletion(&self) -> bool {
        matches!(self.kind, ChangeKind::Deleted)
    }

    /// Returns `true` if this change adds or modifies a file (has content).
    #[must_use]
    pub const fn has_content(&self) -> bool {
        self.content.is_some()
    }
}

// ---------------------------------------------------------------------------
// PatchSet
// ---------------------------------------------------------------------------

/// All changes from a single workspace relative to the epoch base.
///
/// Changes are sorted by path on construction for determinism.
/// An empty `PatchSet` represents a workspace with no changes — these
/// are included in collect output (not skipped) so the caller can
/// handle them explicitly.
#[derive(Clone, Debug)]
pub struct PatchSet {
    /// The workspace these changes came from.
    pub workspace_id: WorkspaceId,
    /// The epoch commit this workspace is based on.
    pub epoch: EpochId,
    /// File changes sorted by path for determinism.
    pub changes: Vec<FileChange>,
}

impl PatchSet {
    /// Create a new `PatchSet`, sorting changes by path for determinism.
    #[must_use]
    pub fn new(workspace_id: WorkspaceId, epoch: EpochId, mut changes: Vec<FileChange>) -> Self {
        // Lexicographic sort by path ensures determinism regardless of insertion order.
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        Self {
            workspace_id,
            epoch,
            changes,
        }
    }

    /// Returns `true` if there are no changes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Total count of all changes.
    #[must_use]
    pub const fn change_count(&self) -> usize {
        self.changes.len()
    }

    /// Count of added files.
    #[must_use]
    pub fn added_count(&self) -> usize {
        self.changes
            .iter()
            .filter(|c| matches!(c.kind, ChangeKind::Added))
            .count()
    }

    /// Count of modified files.
    #[must_use]
    pub fn modified_count(&self) -> usize {
        self.changes
            .iter()
            .filter(|c| matches!(c.kind, ChangeKind::Modified))
            .count()
    }

    /// Count of deleted files.
    #[must_use]
    pub fn deleted_count(&self) -> usize {
        self.changes
            .iter()
            .filter(|c| matches!(c.kind, ChangeKind::Deleted))
            .count()
    }

    /// Returns `true` if this workspace only has deletions (no additions or modifications).
    ///
    /// Useful for the caller to detect deletion-only workspaces, which are
    /// valid but may require special treatment in merge resolution.
    #[must_use]
    pub fn is_deletion_only(&self) -> bool {
        !self.is_empty()
            && self
                .changes
                .iter()
                .all(|c| matches!(c.kind, ChangeKind::Deleted))
    }

    /// Iterate over changed paths.
    pub fn paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.changes.iter().map(|c| &c.path)
    }
}

// ---------------------------------------------------------------------------
// EntryMode
// ---------------------------------------------------------------------------

/// Domain-neutral mirror of [`maw_git::EntryMode`].
///
/// Lives here rather than referencing `maw_git` directly because `maw-core` is
/// the domain layer that owns structured-merge types, and this type needs
/// `Serialize`/`Deserialize` so it can round-trip through JSON as part of
/// [`MaterializedEntry`] and [`ConflictTree`]. Adding `serde` to `maw-git`
/// purely for this would pull serde into the git abstraction layer; mirroring
/// the (tiny) enum here is the lighter-touch choice and follows the same
/// pattern as the two `GitOid` types (one in `maw-git`, one in `maw-core`).
///
/// Conversions are provided via `From` in both directions, so callers at the
/// `maw-core` ↔ `maw-git` boundary can freely convert.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryMode {
    /// Regular file (`100644`).
    Blob,
    /// Executable file (`100755`).
    BlobExecutable,
    /// Subdirectory (`040000`).
    Tree,
    /// Symbolic link (`120000`).
    Link,
    /// Gitlink / submodule (`160000`).
    Commit,
}

impl From<maw_git::EntryMode> for EntryMode {
    fn from(m: maw_git::EntryMode) -> Self {
        match m {
            maw_git::EntryMode::Blob => Self::Blob,
            maw_git::EntryMode::BlobExecutable => Self::BlobExecutable,
            maw_git::EntryMode::Tree => Self::Tree,
            maw_git::EntryMode::Link => Self::Link,
            maw_git::EntryMode::Commit => Self::Commit,
        }
    }
}

impl From<EntryMode> for maw_git::EntryMode {
    fn from(m: EntryMode) -> Self {
        match m {
            EntryMode::Blob => Self::Blob,
            EntryMode::BlobExecutable => Self::BlobExecutable,
            EntryMode::Tree => Self::Tree,
            EntryMode::Link => Self::Link,
            EntryMode::Commit => Self::Commit,
        }
    }
}

// bn-mg0j: lossy projection from the full `EntryMode` to the trimmed
// [`crate::model::conflict::ConflictSideMode`] hint carried by conflict
// sides. `Tree` and `Commit` have no meaningful conflict-side shape in V1
// (they don't appear as leaves that go through the marker-render path), so
// those project to `None`.
impl From<EntryMode> for Option<crate::model::conflict::ConflictSideMode> {
    fn from(m: EntryMode) -> Self {
        use crate::model::conflict::ConflictSideMode;
        match m {
            EntryMode::Blob => Some(ConflictSideMode::Blob),
            EntryMode::BlobExecutable => Some(ConflictSideMode::BlobExecutable),
            EntryMode::Link => Some(ConflictSideMode::Link),
            EntryMode::Tree | EntryMode::Commit => None,
        }
    }
}

// ---------------------------------------------------------------------------
// MaterializedEntry
// ---------------------------------------------------------------------------

/// A concrete, cleanly-resolved entry in a tree — a `(mode, oid)` pair.
///
/// `MaterializedEntry` is what appears at a path in the `clean` map of a
/// [`ConflictTree`]: the merge engine has decided exactly what mode and blob
/// OID that path should have. Conflicted paths are represented by a
/// [`Conflict`] in a separate map and are not materialized until resolved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializedEntry {
    /// The file mode of this entry.
    pub mode: EntryMode,
    /// The git blob (or tree/commit) OID for the entry's content.
    pub oid: GitOid,
}

impl MaterializedEntry {
    /// Create a new materialized entry.
    #[must_use]
    pub const fn new(mode: EntryMode, oid: GitOid) -> Self {
        Self { mode, oid }
    }
}

// ---------------------------------------------------------------------------
// ConflictTree
// ---------------------------------------------------------------------------

/// A partially-resolved tree produced by the N-way merge engine.
///
/// Rebase (and future multi-way merges) operate by folding `PatchSet`s into a
/// `ConflictTree`: cleanly-resolved paths live in `clean`, unresolved paths
/// live in `conflicts`. The `base_epoch` pins the ancestor commit all patches
/// are expressed against — a `PatchSet` whose `epoch` does not match this
/// base is rejected by [`crate::merge::apply::apply_unilateral_patchset`].
///
/// `clean` and `conflicts` are partitioned: a given path appears in exactly
/// one of them (or neither, if untouched).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictTree {
    /// Paths the merge engine has resolved to a single `(mode, oid)` entry.
    pub clean: BTreeMap<PathBuf, MaterializedEntry>,
    /// Paths with unresolved structured conflicts.
    pub conflicts: BTreeMap<PathBuf, Conflict>,
    /// The epoch every patch applied to this tree must be based on.
    pub base_epoch: EpochId,
}

impl ConflictTree {
    /// Create an empty `ConflictTree` pinned to `base_epoch`.
    #[must_use]
    pub const fn new(base_epoch: EpochId) -> Self {
        Self {
            clean: BTreeMap::new(),
            conflicts: BTreeMap::new(),
            base_epoch,
        }
    }

    /// Returns `true` if there are neither clean entries nor conflicts.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.clean.is_empty() && self.conflicts.is_empty()
    }

    /// Returns `true` if the tree has any unresolved conflicts.
    #[must_use]
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::{EpochId, WorkspaceId};

    fn make_epoch() -> EpochId {
        EpochId::new(&"a".repeat(40)).unwrap()
    }

    fn make_ws() -> WorkspaceId {
        WorkspaceId::new("test-ws").unwrap()
    }

    #[test]
    fn change_kind_display() {
        assert_eq!(format!("{}", ChangeKind::Added), "added");
        assert_eq!(format!("{}", ChangeKind::Modified), "modified");
        assert_eq!(format!("{}", ChangeKind::Deleted), "deleted");
    }

    #[test]
    fn file_change_deletion_has_no_content() {
        let fc = FileChange::new(PathBuf::from("gone.rs"), ChangeKind::Deleted, None);
        assert!(fc.is_deletion());
        assert!(!fc.has_content());
    }

    #[test]
    fn file_change_add_has_content() {
        let fc = FileChange::new(
            PathBuf::from("new.rs"),
            ChangeKind::Added,
            Some(b"fn main() {}".to_vec()),
        );
        assert!(!fc.is_deletion());
        assert!(fc.has_content());
    }

    #[test]
    fn patch_set_empty() {
        let ps = PatchSet::new(make_ws(), make_epoch(), vec![]);
        assert!(ps.is_empty());
        assert_eq!(ps.change_count(), 0);
        assert!(!ps.is_deletion_only());
    }

    #[test]
    fn patch_set_sorts_by_path() {
        let changes = vec![
            FileChange::new(PathBuf::from("z.rs"), ChangeKind::Added, Some(vec![])),
            FileChange::new(PathBuf::from("a.rs"), ChangeKind::Added, Some(vec![])),
            FileChange::new(PathBuf::from("m.rs"), ChangeKind::Modified, Some(vec![])),
        ];
        let ps = PatchSet::new(make_ws(), make_epoch(), changes);
        let paths: Vec<_> = ps.paths().collect();
        assert_eq!(paths[0], &PathBuf::from("a.rs"));
        assert_eq!(paths[1], &PathBuf::from("m.rs"));
        assert_eq!(paths[2], &PathBuf::from("z.rs"));
    }

    #[test]
    fn patch_set_deletion_only() {
        let changes = vec![
            FileChange::new(PathBuf::from("old.rs"), ChangeKind::Deleted, None),
            FileChange::new(PathBuf::from("other.rs"), ChangeKind::Deleted, None),
        ];
        let ps = PatchSet::new(make_ws(), make_epoch(), changes);
        assert!(ps.is_deletion_only());
        assert!(!ps.is_empty());
        assert_eq!(ps.deleted_count(), 2);
    }

    #[test]
    fn patch_set_mixed_not_deletion_only() {
        let changes = vec![
            FileChange::new(PathBuf::from("old.rs"), ChangeKind::Deleted, None),
            FileChange::new(PathBuf::from("new.rs"), ChangeKind::Added, Some(vec![])),
        ];
        let ps = PatchSet::new(make_ws(), make_epoch(), changes);
        assert!(!ps.is_deletion_only());
    }

    #[test]
    fn patch_set_counts() {
        let changes = vec![
            FileChange::new(PathBuf::from("add.rs"), ChangeKind::Added, Some(vec![])),
            FileChange::new(PathBuf::from("add2.rs"), ChangeKind::Added, Some(vec![])),
            FileChange::new(PathBuf::from("mod.rs"), ChangeKind::Modified, Some(vec![])),
            FileChange::new(PathBuf::from("del.rs"), ChangeKind::Deleted, None),
        ];
        let ps = PatchSet::new(make_ws(), make_epoch(), changes);
        assert_eq!(ps.added_count(), 2);
        assert_eq!(ps.modified_count(), 1);
        assert_eq!(ps.deleted_count(), 1);
        assert_eq!(ps.change_count(), 4);
    }

    // -----------------------------------------------------------------------
    // EntryMode ↔ maw_git::EntryMode conversion
    // -----------------------------------------------------------------------

    #[test]
    fn entry_mode_roundtrips_through_maw_git() {
        let variants = [
            EntryMode::Blob,
            EntryMode::BlobExecutable,
            EntryMode::Tree,
            EntryMode::Link,
            EntryMode::Commit,
        ];
        for mode in variants {
            let git_mode: maw_git::EntryMode = mode.into();
            let back: EntryMode = git_mode.into();
            assert_eq!(back, mode);
        }
    }

    // -----------------------------------------------------------------------
    // ConflictTree
    // -----------------------------------------------------------------------

    #[test]
    fn conflict_tree_new_is_empty() {
        let tree = ConflictTree::new(make_epoch());
        assert!(tree.is_empty());
        assert!(!tree.has_conflicts());
        assert_eq!(tree.base_epoch, make_epoch());
    }

    #[test]
    fn conflict_tree_serde_roundtrip() {
        use crate::model::conflict::{Conflict, ConflictSide};
        use crate::model::ordering::OrderingKey;

        let base_epoch = make_epoch();
        // Build the OrderingKey first (consumes a clone of base_epoch) so the
        // final `ConflictTree::new(base_epoch)` below moves instead of clones.
        let ord_key = OrderingKey::new(
            base_epoch.clone(),
            "ws-1".parse().unwrap(),
            1,
            1_700_000_000_000,
        );
        let mut tree = ConflictTree::new(base_epoch);

        // A couple of clean entries.
        tree.clean.insert(
            PathBuf::from("src/lib.rs"),
            MaterializedEntry::new(EntryMode::Blob, GitOid::new(&"a".repeat(40)).unwrap()),
        );
        tree.clean.insert(
            PathBuf::from("scripts/build.sh"),
            MaterializedEntry::new(
                EntryMode::BlobExecutable,
                GitOid::new(&"b".repeat(40)).unwrap(),
            ),
        );
        tree.clean.insert(
            PathBuf::from("link"),
            MaterializedEntry::new(EntryMode::Link, GitOid::new(&"c".repeat(40)).unwrap()),
        );

        // One conflict.
        let side_a = ConflictSide::new(
            "ws-1".into(),
            GitOid::new(&"1".repeat(40)).unwrap(),
            ord_key.clone(),
        );
        let side_b = ConflictSide::new(
            "ws-2".into(),
            GitOid::new(&"2".repeat(40)).unwrap(),
            ord_key,
        );
        tree.conflicts.insert(
            PathBuf::from("src/new.rs"),
            Conflict::AddAdd {
                path: PathBuf::from("src/new.rs"),
                sides: vec![side_a, side_b],
            },
        );

        let json = serde_json::to_string(&tree).unwrap();
        let decoded: ConflictTree = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, tree);
    }

    #[test]
    fn materialized_entry_serde_roundtrip() {
        let entry = MaterializedEntry::new(EntryMode::Blob, GitOid::new(&"f".repeat(40)).unwrap());
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: MaterializedEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, entry);
    }
}
