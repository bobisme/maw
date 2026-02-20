//! Workspace backend trait and common types.
//!
//! Defines the interface that all workspace backends must implement.
//! This is the API contract between maw's CLI layer and the underlying
//! isolation mechanism (git worktrees, copy-on-write snapshots, or other backends).

pub mod copy;
pub mod git;
pub mod reflink;

use std::path::PathBuf;

use crate::model::types::{EpochId, WorkspaceId, WorkspaceInfo};

/// A workspace backend implementation.
///
/// The `WorkspaceBackend` trait defines the interface for creating, managing,
/// and querying workspaces. Implementations of this trait are responsible for
/// the actual isolation mechanism (e.g., git worktrees, reflinks, overlays).
///
/// # Key Invariants
///
/// - **Workspace isolation**: Each workspace's working copy is independent.
///   Changes in one workspace don't affect others until explicitly merged.
/// - **Workspace uniqueness**: No two active workspaces can have the same name
///   within a given repository.
/// - **Epoch tracking**: Each workspace is anchored to an epoch (a specific
///   repository state). Workspaces can become stale if the repository advances.
#[allow(clippy::missing_errors_doc)]
pub trait WorkspaceBackend {
    /// The error type returned by backend operations.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Create a new workspace.
    ///
    /// Creates a new workspace with the given name, anchored to the provided epoch.
    /// The workspace is initialized with a clean working copy at that epoch.
    ///
    /// # Arguments
    /// * `name` - Unique workspace identifier (must be a valid [`WorkspaceId`])
    /// * `epoch` - The repository state this workspace is based on
    ///
    /// # Returns
    /// Complete information about the newly created workspace, including its
    /// path and initial state.
    ///
    /// # Invariants
    /// - The returned `WorkspaceInfo` has state [`WorkspaceState::Active`]
    /// - The workspace directory exists and is ready for use
    /// - No workspace with the same name exists before the call
    /// - The workspace is isolated from all other workspaces
    fn create(&self, name: &WorkspaceId, epoch: &EpochId) -> Result<WorkspaceInfo, Self::Error>;

    /// Destroy a workspace.
    ///
    /// Removes the workspace from the system. The workspace directory and all
    /// its contents are deleted. The workspace becomes unavailable for future
    /// operations.
    ///
    /// # Arguments
    /// * `name` - Identifier of the workspace to destroy
    ///
    /// # Invariants
    /// - The workspace directory is fully removed
    /// - The workspace can no longer be accessed via any backend method
    /// - Destroying a non-existent workspace is a no-op (idempotent)
    fn destroy(&self, name: &WorkspaceId) -> Result<(), Self::Error>;

    /// List all workspaces.
    ///
    /// Returns information about all active workspaces in the repository.
    /// Does not include destroyed workspaces.
    ///
    /// # Returns
    /// A vector of [`WorkspaceInfo`] for all active workspaces,
    /// or empty vector if no workspaces exist.
    ///
    /// # Invariants
    /// - Only active workspaces are included
    /// - Order is consistent but unspecified
    fn list(&self) -> Result<Vec<WorkspaceInfo>, Self::Error>;

    /// Get the current status of a workspace.
    ///
    /// Returns detailed information about the workspace's current state,
    /// including its epoch, dirty files, and staleness.
    ///
    /// # Arguments
    /// * `name` - Identifier of the workspace to query
    ///
    /// # Invariants
    /// - The returned status reflects the workspace's current state
    /// - For a stale workspace, `is_stale` is `true` and `behind_epochs`
    ///   indicates how many epochs the workspace is behind
    /// - For a destroyed workspace, returns an error (not a status)
    fn status(&self, name: &WorkspaceId) -> Result<WorkspaceStatus, Self::Error>;

    /// Capture all changes in the workspace.
    ///
    /// Scans the workspace for all modified, added, and deleted files.
    /// Returns a snapshot of changes that can be committed or discarded.
    ///
    /// # Arguments
    /// * `name` - Identifier of the workspace to snapshot
    ///
    /// # Returns
    /// A [`SnapshotResult`] containing the list of changed paths and their
    /// change kinds (add, modify, delete).
    ///
    /// # Invariants
    /// - Only working copy changes are included; committed changes are not
    /// - All reported paths are relative to the workspace root
    /// - The snapshot is point-in-time; changes made after the snapshot are not included
    fn snapshot(&self, name: &WorkspaceId) -> Result<SnapshotResult, Self::Error>;

    /// Get the absolute path to a workspace.
    ///
    /// Returns the absolute filesystem path where the workspace's files are stored.
    /// Does not verify that the workspace exists.
    ///
    /// # Arguments
    /// * `name` - Identifier of the workspace
    ///
    /// # Returns
    /// An absolute [`PathBuf`] to the workspace root directory.
    ///
    /// # Invariants
    /// - The path is absolute (not relative)
    /// - The path is consistent: repeated calls return equal paths
    /// - The path may not exist if the workspace has been destroyed
    fn workspace_path(&self, name: &WorkspaceId) -> PathBuf;

    /// Check if a workspace exists.
    ///
    /// Returns `true` if a workspace with the given name exists and is active,
    /// `false` otherwise.
    ///
    /// # Arguments
    /// * `name` - Identifier of the workspace
    ///
    /// # Invariants
    /// - Returns `true` only if the workspace is active and accessible
    /// - Destroyed or non-existent workspaces return `false`
    /// - This is a lightweight check; no I/O is guaranteed
    fn exists(&self, name: &WorkspaceId) -> bool;
}

/// Detailed status information about a workspace.
///
/// Captures the current state of a workspace, including its epoch,
/// whether it is stale, and which files have been modified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceStatus {
    /// The epoch this workspace is based on.
    pub base_epoch: EpochId,
    /// Paths to all dirty (modified) files in the working copy,
    /// relative to the workspace root.
    pub dirty_files: Vec<PathBuf>,
    /// Whether this workspace is stale (behind the current repository epoch).
    pub is_stale: bool,
}

impl WorkspaceStatus {
    /// Create a new workspace status.
    ///
    /// # Arguments
    /// * `base_epoch` - The epoch this workspace is based on
    /// * `dirty_files` - List of modified file paths (relative to workspace root)
    /// * `is_stale` - Whether the workspace is behind the current epoch
    #[must_use] 
    pub const fn new(base_epoch: EpochId, dirty_files: Vec<PathBuf>, is_stale: bool) -> Self {
        Self {
            base_epoch,
            dirty_files,
            is_stale,
        }
    }

    /// Returns `true` if there are no dirty files.
    #[must_use]
    #[allow(dead_code)]
    pub const fn is_clean(&self) -> bool {
        self.dirty_files.is_empty()
    }

    /// Returns the number of dirty files.
    #[must_use]
    #[allow(dead_code)]
    pub const fn dirty_count(&self) -> usize {
        self.dirty_files.len()
    }
}

/// The result of a workspace snapshot operation.
///
/// Contains all changes detected in a workspace's working copy,
/// categorized by type (added, modified, deleted).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotResult {
    /// Added files (relative to workspace root).
    pub added: Vec<PathBuf>,
    /// Modified files (relative to workspace root).
    pub modified: Vec<PathBuf>,
    /// Deleted files (relative to workspace root).
    pub deleted: Vec<PathBuf>,
}

impl SnapshotResult {
    /// Create a new snapshot result with the given changes.
    ///
    /// # Arguments
    /// * `added` - Paths to files that were added
    /// * `modified` - Paths to files that were modified
    /// * `deleted` - Paths to files that were deleted
    #[must_use] 
    pub const fn new(added: Vec<PathBuf>, modified: Vec<PathBuf>, deleted: Vec<PathBuf>) -> Self {
        Self {
            added,
            modified,
            deleted,
        }
    }

    /// All changed files (added + modified + deleted).
    #[must_use]
    pub fn all_changed(&self) -> Vec<&PathBuf> {
        self.added
            .iter()
            .chain(self.modified.iter())
            .chain(self.deleted.iter())
            .collect()
    }

    /// Total count of all changes.
    #[must_use]
    pub const fn change_count(&self) -> usize {
        self.added.len() + self.modified.len() + self.deleted.len()
    }

    /// Returns `true` if there are no changes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.change_count() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_status_is_clean() {
        let status = WorkspaceStatus::new(EpochId::new(&"a".repeat(40)).unwrap(), vec![], false);
        assert!(status.is_clean());
        assert_eq!(status.dirty_count(), 0);
    }

    #[test]
    fn workspace_status_dirty() {
        let dirty_files = vec![PathBuf::from("file1.rs"), PathBuf::from("file2.rs")];
        let status = WorkspaceStatus::new(
            EpochId::new(&"b".repeat(40)).unwrap(),
            dirty_files.clone(),
            false,
        );
        assert!(!status.is_clean());
        assert_eq!(status.dirty_count(), 2);
        assert_eq!(status.dirty_files, dirty_files);
    }

    #[test]
    fn workspace_status_stale() {
        let status = WorkspaceStatus::new(EpochId::new(&"c".repeat(40)).unwrap(), vec![], true);
        assert!(status.is_stale);
        assert!(status.is_clean());
    }

    #[test]
    fn snapshot_result_empty() {
        let snapshot = SnapshotResult::new(vec![], vec![], vec![]);
        assert!(snapshot.is_empty());
        assert_eq!(snapshot.change_count(), 0);
        assert!(snapshot.all_changed().is_empty());
    }

    #[test]
    fn snapshot_result_added() {
        let added = vec![PathBuf::from("src/main.rs"), PathBuf::from("Cargo.toml")];
        let snapshot = SnapshotResult::new(added.clone(), vec![], vec![]);
        assert!(!snapshot.is_empty());
        assert_eq!(snapshot.change_count(), 2);
        assert_eq!(snapshot.added, added);
        assert!(snapshot.modified.is_empty());
        assert!(snapshot.deleted.is_empty());
    }

    #[test]
    fn snapshot_result_modified() {
        let modified = vec![PathBuf::from("src/lib.rs")];
        let snapshot = SnapshotResult::new(vec![], modified.clone(), vec![]);
        assert!(!snapshot.is_empty());
        assert_eq!(snapshot.change_count(), 1);
        assert_eq!(snapshot.modified, modified);
    }

    #[test]
    fn snapshot_result_deleted() {
        let deleted = vec![PathBuf::from("old_file.rs")];
        let snapshot = SnapshotResult::new(vec![], vec![], deleted.clone());
        assert!(!snapshot.is_empty());
        assert_eq!(snapshot.change_count(), 1);
        assert_eq!(snapshot.deleted, deleted);
    }

    #[test]
    fn snapshot_result_mixed() {
        let added = vec![PathBuf::from("new.rs")];
        let modified = vec![PathBuf::from("src/main.rs")];
        let deleted = vec![PathBuf::from("deprecated.rs")];
        let snapshot = SnapshotResult::new(added, modified, deleted);
        assert!(!snapshot.is_empty());
        assert_eq!(snapshot.change_count(), 3);

        let all = snapshot.all_changed();
        assert_eq!(all.len(), 3);
        assert!(all.contains(&&PathBuf::from("new.rs")));
        assert!(all.contains(&&PathBuf::from("src/main.rs")));
        assert!(all.contains(&&PathBuf::from("deprecated.rs")));
    }
}
pub mod overlay;
pub mod platform;

// ---------------------------------------------------------------------------
// AnyBackend — polymorphic backend enum
// ---------------------------------------------------------------------------

use copy::CopyBackend;
use git::GitWorktreeBackend;
use overlay::OverlayBackend;
use reflink::RefLinkBackend;

use crate::config::BackendKind;

// ---------------------------------------------------------------------------
// AnyBackendError
// ---------------------------------------------------------------------------

/// Error type for [`AnyBackend`] — boxes the underlying backend error.
#[derive(Debug)]
pub struct AnyBackendError(pub Box<dyn std::error::Error + Send + Sync + 'static>);

impl std::fmt::Display for AnyBackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for AnyBackendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

// ---------------------------------------------------------------------------
// AnyBackend
// ---------------------------------------------------------------------------

/// A concrete backend selected at runtime based on platform capabilities and
/// configuration. Dispatches to the appropriate implementation.
///
/// Using an enum (rather than `Box<dyn WorkspaceBackend>`) avoids dynamic
/// dispatch and keeps the `Error` associated type monomorphic.
pub enum AnyBackend {
    /// Git worktree backend — always available.
    GitWorktree(GitWorktreeBackend),
    /// Reflink (`CoW`) backend — requires a CoW-capable filesystem.
    Reflink(RefLinkBackend),
    /// `OverlayFS` backend — Linux only.
    Overlay(OverlayBackend),
    /// Plain recursive-copy backend — universal fallback.
    Copy(CopyBackend),
}

impl AnyBackend {
    /// Construct the appropriate backend for the resolved (non-Auto) kind and repo root.
    ///
    /// If `kind` is `BackendKind::Auto` (which should be resolved before calling
    /// this function), falls back to `GitWorktree`.
    ///
    /// # Errors
    /// Returns an error if the overlay backend is selected but is not supported
    /// on this platform (not Linux, or no mount strategy available).
    pub fn from_kind(kind: BackendKind, root: PathBuf) -> anyhow::Result<Self> {
        match kind {
            BackendKind::GitWorktree | BackendKind::Auto => {
                Ok(Self::GitWorktree(GitWorktreeBackend::new(root)))
            }
            BackendKind::Reflink => Ok(Self::Reflink(RefLinkBackend::new(root))),
            BackendKind::Overlay => {
                let backend = OverlayBackend::new(root).map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(Self::Overlay(backend))
            }
            BackendKind::Copy => Ok(Self::Copy(CopyBackend::new(root))),
        }
    }

}

/// Helper: convert a backend-specific error into [`AnyBackendError`].
fn wrap_err<E>(e: E) -> AnyBackendError
where
    E: std::error::Error + Send + Sync + 'static,
{
    AnyBackendError(Box::new(e))
}

impl WorkspaceBackend for AnyBackend {
    type Error = AnyBackendError;

    fn create(&self, name: &WorkspaceId, epoch: &EpochId) -> Result<WorkspaceInfo, Self::Error> {
        match self {
            Self::GitWorktree(b) => b.create(name, epoch).map_err(wrap_err),
            Self::Reflink(b) => b.create(name, epoch).map_err(wrap_err),
            Self::Overlay(b) => b.create(name, epoch).map_err(wrap_err),
            Self::Copy(b) => b.create(name, epoch).map_err(wrap_err),
        }
    }

    fn destroy(&self, name: &WorkspaceId) -> Result<(), Self::Error> {
        match self {
            Self::GitWorktree(b) => b.destroy(name).map_err(wrap_err),
            Self::Reflink(b) => b.destroy(name).map_err(wrap_err),
            Self::Overlay(b) => b.destroy(name).map_err(wrap_err),
            Self::Copy(b) => b.destroy(name).map_err(wrap_err),
        }
    }

    fn list(&self) -> Result<Vec<WorkspaceInfo>, Self::Error> {
        match self {
            Self::GitWorktree(b) => b.list().map_err(wrap_err),
            Self::Reflink(b) => b.list().map_err(wrap_err),
            Self::Overlay(b) => b.list().map_err(wrap_err),
            Self::Copy(b) => b.list().map_err(wrap_err),
        }
    }

    fn status(&self, name: &WorkspaceId) -> Result<WorkspaceStatus, Self::Error> {
        match self {
            Self::GitWorktree(b) => b.status(name).map_err(wrap_err),
            Self::Reflink(b) => b.status(name).map_err(wrap_err),
            Self::Overlay(b) => b.status(name).map_err(wrap_err),
            Self::Copy(b) => b.status(name).map_err(wrap_err),
        }
    }

    fn snapshot(&self, name: &WorkspaceId) -> Result<SnapshotResult, Self::Error> {
        match self {
            Self::GitWorktree(b) => b.snapshot(name).map_err(wrap_err),
            Self::Reflink(b) => b.snapshot(name).map_err(wrap_err),
            Self::Overlay(b) => b.snapshot(name).map_err(wrap_err),
            Self::Copy(b) => b.snapshot(name).map_err(wrap_err),
        }
    }

    fn workspace_path(&self, name: &WorkspaceId) -> PathBuf {
        match self {
            Self::GitWorktree(b) => b.workspace_path(name),
            Self::Reflink(b) => b.workspace_path(name),
            Self::Overlay(b) => b.workspace_path(name),
            Self::Copy(b) => b.workspace_path(name),
        }
    }

    fn exists(&self, name: &WorkspaceId) -> bool {
        match self {
            Self::GitWorktree(b) => b.exists(name),
            Self::Reflink(b) => b.exists(name),
            Self::Overlay(b) => b.exists(name),
            Self::Copy(b) => b.exists(name),
        }
    }
}
