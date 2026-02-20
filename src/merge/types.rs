//! Core types for the N-way merge engine.
//!
//! Defines the data structures that flow through the collect → partition →
//! resolve → build pipeline.

use std::path::PathBuf;

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
/// `None` if FileId tracking was not available at collect time.
///
/// `blob` is the git blob OID for the new content (computed via
/// `git hash-object`). Present for `Added` and `Modified` changes when the
/// collect step had access to the git repo. Enables O(1) hash-equality checks
/// in the resolve step without comparing raw bytes.
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
    /// `None` when FileId tracking is unavailable (e.g., legacy workspaces
    /// that predate FileId introduction, or tests that don't supply one).
    pub file_id: Option<FileId>,
    /// Git blob OID for the new content (present for Add/Modify; `None` for
    /// Delete and for changes collected without git access).
    ///
    /// When populated, the resolve step uses OID equality instead of byte
    /// comparison for hash-equality checks, which is both faster and avoids
    /// loading content into memory.
    pub blob: Option<GitOid>,
}

impl FileChange {
    /// Create a new `FileChange` without FileId or blob OID metadata.
    ///
    /// Suitable for Phase 1 tests and code paths that don't yet track
    /// stable file identity. Use [`FileChange::with_identity`] when those
    /// fields are available.
    pub fn new(path: PathBuf, kind: ChangeKind, content: Option<Vec<u8>>) -> Self {
        Self {
            path,
            kind,
            content,
            file_id: None,
            blob: None,
        }
    }

    /// Create a new `FileChange` with full identity metadata.
    ///
    /// Preferred constructor for Phase 3+ code paths where `file_id` and
    /// `blob` OID are available from the workspace's FileId map and git
    /// object store.
    pub fn with_identity(
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
        }
    }

    /// Returns `true` if this change is a deletion.
    #[must_use]
    pub fn is_deletion(&self) -> bool {
        matches!(self.kind, ChangeKind::Deleted)
    }

    /// Returns `true` if this change adds or modifies a file (has content).
    #[must_use]
    pub fn has_content(&self) -> bool {
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
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Total count of all changes.
    #[must_use]
    pub fn change_count(&self) -> usize {
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
}
