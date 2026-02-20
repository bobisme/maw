//! Collect step of the N-way merge pipeline.
//!
//! For each source workspace, calls `backend.snapshot()` to enumerate changed
//! files, then reads their content. Produces one [`PatchSet`] per workspace.
//!
//! # Invariants
//!
//! - **Determinism**: `PatchSet` changes are sorted by path on construction.
//! - **Completeness**: Every workspace in `workspace_ids` produces a `PatchSet`,
//!   including empty workspaces. The caller decides how to handle empties.
//! - **Isolation**: Each workspace is snapshotted independently. A failure in
//!   one workspace returns `Err` immediately (fail-fast).
//!
//! # FileId and blob OID enrichment (Phase 3+)
//!
//! When `repo_root` is provided, `collect_snapshots` enriches each
//! [`FileChange`] with:
//!
//! - `file_id`: looked up from `.manifold/fileids` for Modified/Deleted files
//!   (files that existed in the epoch). Added files receive a fresh random
//!   [`FileId`]. If the fileids file is absent, FileIds are omitted.
//! - `blob`: the git blob OID for the new content, computed via
//!   `git hash-object -w --stdin`. Enables O(1) hash-equality checks in the
//!   resolve step.

use std::fmt;
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::backend::WorkspaceBackend;
use crate::model::file_id::FileIdMap;
use crate::model::patch::FileId;
use crate::model::types::{GitOid, WorkspaceId};

use super::types::{ChangeKind, FileChange, PatchSet};

// ---------------------------------------------------------------------------
// CollectError
// ---------------------------------------------------------------------------

/// Errors that can occur during the collect step.
#[derive(Debug)]
pub enum CollectError {
    /// A workspace snapshot operation failed.
    SnapshotFailed {
        /// The workspace that failed.
        workspace_id: WorkspaceId,
        /// Underlying error message.
        reason: String,
    },
    /// Reading a changed file's content failed.
    ReadFailed {
        /// The workspace where the file lives.
        workspace_id: WorkspaceId,
        /// The file that could not be read (relative path).
        path: PathBuf,
        /// Underlying I/O error message.
        reason: String,
    },
    /// Querying the workspace's base epoch failed.
    EpochFailed {
        /// The workspace that failed.
        workspace_id: WorkspaceId,
        /// Underlying error message.
        reason: String,
    },
}

impl fmt::Display for CollectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SnapshotFailed {
                workspace_id,
                reason,
            } => {
                write!(
                    f,
                    "snapshot failed for workspace '{}': {}",
                    workspace_id, reason
                )
            }
            Self::ReadFailed {
                workspace_id,
                path,
                reason,
            } => {
                write!(
                    f,
                    "failed to read '{}' in workspace '{}': {}",
                    path.display(),
                    workspace_id,
                    reason
                )
            }
            Self::EpochFailed {
                workspace_id,
                reason,
            } => {
                write!(
                    f,
                    "epoch query failed for workspace '{}': {}",
                    workspace_id, reason
                )
            }
        }
    }
}

impl std::error::Error for CollectError {}

// ---------------------------------------------------------------------------
// collect_snapshots
// ---------------------------------------------------------------------------

/// Collect changed-file snapshots from a set of workspaces.
///
/// For each workspace in `workspace_ids`:
/// 1. Calls `backend.snapshot()` to enumerate added, modified, and deleted paths.
/// 2. Calls `backend.status()` to determine the workspace's base epoch.
/// 3. Reads file content for added/modified files from the workspace directory.
/// 4. Enriches each [`FileChange`] with a git blob OID (via `git hash-object`)
///    and a stable [`FileId`] (from `.manifold/fileids` or freshly generated).
///
/// Returns one `PatchSet` per workspace in the same order as `workspace_ids`.
/// Empty workspaces (no changes) produce an empty `PatchSet` — they are **not**
/// filtered out, so the caller receives a complete picture.
///
/// # Arguments
///
/// * `repo_root` — Path to the git repository root, used to:
///   - Write blobs via `git hash-object -w --stdin`.
///   - Load the epoch FileId map from `<repo_root>/.manifold/fileids`.
///
/// # Errors
///
/// Returns [`CollectError`] on the first workspace that fails. Failures include:
/// - Workspace not found (e.g., destroyed between listing and collect)
/// - I/O errors reading file content
/// - Backend errors querying status
pub fn collect_snapshots<B: WorkspaceBackend>(
    repo_root: &Path,
    backend: &B,
    workspace_ids: &[WorkspaceId],
) -> Result<Vec<PatchSet>, CollectError> {
    // Load the epoch FileId map once; shared across all workspaces.
    // If the file doesn't exist yet (new repo), use an empty map.
    let fileids_path = repo_root.join(".manifold").join("fileids");
    let file_id_map = FileIdMap::load(&fileids_path).unwrap_or_default();

    let mut patch_sets = Vec::with_capacity(workspace_ids.len());
    for ws_id in workspace_ids {
        let patch_set = collect_one(repo_root, &file_id_map, backend, ws_id)?;
        patch_sets.push(patch_set);
    }

    Ok(patch_sets)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Collect a single workspace's changes into a `PatchSet`.
///
/// Enriches each [`FileChange`] with:
/// - `file_id`: from `file_id_map` (Modified/Deleted) or freshly generated (Added).
/// - `blob`: computed via `git hash-object -w --stdin` for Added/Modified content.
fn collect_one<B: WorkspaceBackend>(
    repo_root: &Path,
    file_id_map: &FileIdMap,
    backend: &B,
    ws_id: &WorkspaceId,
) -> Result<PatchSet, CollectError> {
    // Step 1: Enumerate changed paths.
    let snapshot = backend
        .snapshot(ws_id)
        .map_err(|e| CollectError::SnapshotFailed {
            workspace_id: ws_id.clone(),
            reason: e.to_string(),
        })?;

    // Step 2: Determine the workspace's base epoch.
    let status = backend
        .status(ws_id)
        .map_err(|e| CollectError::EpochFailed {
            workspace_id: ws_id.clone(),
            reason: e.to_string(),
        })?;
    let epoch = status.base_epoch;

    // Step 3: Short-circuit for empty workspaces.
    if snapshot.is_empty() {
        return Ok(PatchSet::new(ws_id.clone(), epoch, vec![]));
    }

    // Step 4: Build FileChanges, reading content for non-deletions.
    let ws_path = backend.workspace_path(ws_id);
    let capacity = snapshot.change_count();
    let mut changes = Vec::with_capacity(capacity);

    // Added files: read content, generate fresh FileId, compute blob OID.
    for path in &snapshot.added {
        let content = read_workspace_file(&ws_path, path, ws_id)?;
        let blob = git_hash_object(repo_root, &content);
        // Assign a fresh FileId for new files. The FileIdMap for the epoch
        // won't have an entry yet; the FileId is minted here and would be
        // persisted by the workspace's oplog in a full implementation.
        let file_id = Some(FileId::random());
        changes.push(FileChange::with_identity(
            path.clone(),
            ChangeKind::Added,
            Some(content),
            file_id,
            blob,
        ));
    }

    // Modified files: read current content, look up existing FileId, compute blob OID.
    for path in &snapshot.modified {
        let content = read_workspace_file(&ws_path, path, ws_id)?;
        let blob = git_hash_object(repo_root, &content);
        // Modified files existed in the epoch, so their FileId is in the map.
        let file_id = file_id_map.id_for_path(path);
        changes.push(FileChange::with_identity(
            path.clone(),
            ChangeKind::Modified,
            Some(content),
            file_id,
            blob,
        ));
    }

    // Deleted files: no content; look up FileId from epoch map.
    for path in &snapshot.deleted {
        let file_id = file_id_map.id_for_path(path);
        changes.push(FileChange::with_identity(
            path.clone(),
            ChangeKind::Deleted,
            None,
            file_id,
            None, // no blob for deletions
        ));
    }

    Ok(PatchSet::new(ws_id.clone(), epoch, changes))
}

/// Read the current content of a file from a workspace's working tree.
fn read_workspace_file(
    ws_path: &PathBuf,
    rel_path: &PathBuf,
    ws_id: &WorkspaceId,
) -> Result<Vec<u8>, CollectError> {
    let full_path = ws_path.join(rel_path);
    std::fs::read(&full_path).map_err(|e| CollectError::ReadFailed {
        workspace_id: ws_id.clone(),
        path: rel_path.clone(),
        reason: e.to_string(),
    })
}

/// Write `content` to the git object store and return its blob OID.
///
/// Runs `git hash-object -w --stdin` in `repo_root`. Returns `None` on any
/// failure (git unavailable, I/O error, invalid OID output) — callers treat
/// a missing blob OID as a degraded-mode fallback, not a hard error.
fn git_hash_object(repo_root: &Path, content: &[u8]) -> Option<GitOid> {
    let mut child = Command::new("git")
        .args(["hash-object", "-w", "--stdin"])
        .current_dir(repo_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // Write content to stdin; ignore broken-pipe errors.
    if let Some(stdin) = child.stdin.take() {
        let mut stdin = stdin;
        let _ = stdin.write_all(content);
    }

    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }

    let hex = String::from_utf8(output.stdout).ok()?;
    GitOid::new(hex.trim()).ok()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::WorkspaceBackend;
    use crate::backend::git::GitWorktreeBackend;
    use crate::model::types::{EpochId, WorkspaceId};
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Set up a fresh git repo with one initial commit.
    ///
    /// Returns `(TempDir, EpochId)` where `EpochId` is the initial commit OID.
    /// The `TempDir` must outlive the `GitWorktreeBackend` that uses it.
    fn setup_git_repo() -> (TempDir, EpochId) {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();

        for (key, val) in [
            ("user.name", "Test User"),
            ("user.email", "test@example.com"),
            ("commit.gpgsign", "false"),
        ] {
            Command::new("git")
                .args(["config", key, val])
                .current_dir(root)
                .output()
                .unwrap();
        }

        // Write an initial file so the repo has at least one tracked file.
        fs::write(root.join("README.md"), "# Test Repo").unwrap();
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Initial commit"])
            .current_dir(root)
            .output()
            .unwrap();

        let oid_str = git_head_oid(root);
        let epoch = EpochId::new(&oid_str).unwrap();
        (temp_dir, epoch)
    }

    /// Return the current HEAD OID of a git repo.
    fn git_head_oid(root: &std::path::Path) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    // -----------------------------------------------------------------------
    // Error display
    // -----------------------------------------------------------------------

    #[test]
    fn collect_error_display_snapshot_failed() {
        let ws_id = WorkspaceId::new("alpha").unwrap();
        let err = CollectError::SnapshotFailed {
            workspace_id: ws_id,
            reason: "disk full".to_owned(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("alpha"), "missing workspace name: {msg}");
        assert!(msg.contains("disk full"), "missing reason: {msg}");
    }

    #[test]
    fn collect_error_display_read_failed() {
        let ws_id = WorkspaceId::new("beta").unwrap();
        let err = CollectError::ReadFailed {
            workspace_id: ws_id,
            path: PathBuf::from("src/lib.rs"),
            reason: "permission denied".to_owned(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("beta"), "missing workspace name: {msg}");
        assert!(msg.contains("src/lib.rs"), "missing path: {msg}");
        assert!(msg.contains("permission denied"), "missing reason: {msg}");
    }

    #[test]
    fn collect_error_display_epoch_failed() {
        let ws_id = WorkspaceId::new("gamma").unwrap();
        let err = CollectError::EpochFailed {
            workspace_id: ws_id,
            reason: "not a git repo".to_owned(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("gamma"), "missing workspace name: {msg}");
        assert!(msg.contains("not a git repo"), "missing reason: {msg}");
    }

    // -----------------------------------------------------------------------
    // Empty workspaces
    // -----------------------------------------------------------------------

    #[test]
    fn collect_empty_workspace_produces_empty_patch_set() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_id = WorkspaceId::new("empty-ws").unwrap();
        backend.create(&ws_id, &epoch).unwrap();

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id.clone()]).unwrap();

        assert_eq!(results.len(), 1, "should have one PatchSet");
        let ps = &results[0];
        assert_eq!(ps.workspace_id, ws_id);
        assert!(ps.is_empty(), "no changes expected: {:?}", ps.changes);
        assert_eq!(ps.epoch, epoch);
    }

    // -----------------------------------------------------------------------
    // Added files
    // -----------------------------------------------------------------------

    #[test]
    fn collect_added_file() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_id = WorkspaceId::new("add-ws").unwrap();
        let info = backend.create(&ws_id, &epoch).unwrap();

        fs::write(info.path.join("new.rs"), "fn main() {}").unwrap();

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id.clone()]).unwrap();
        let ps = &results[0];

        assert_eq!(ps.change_count(), 1);
        let change = &ps.changes[0];
        assert_eq!(change.path, PathBuf::from("new.rs"));
        assert!(matches!(change.kind, ChangeKind::Added));
        assert_eq!(change.content.as_deref(), Some(b"fn main() {}".as_ref()));
    }

    // -----------------------------------------------------------------------
    // Modified files
    // -----------------------------------------------------------------------

    #[test]
    fn collect_modified_file() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_id = WorkspaceId::new("mod-ws").unwrap();
        let info = backend.create(&ws_id, &epoch).unwrap();

        fs::write(info.path.join("README.md"), "# Modified").unwrap();

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id.clone()]).unwrap();
        let ps = &results[0];

        assert_eq!(ps.change_count(), 1);
        let change = &ps.changes[0];
        assert_eq!(change.path, PathBuf::from("README.md"));
        assert!(matches!(change.kind, ChangeKind::Modified));
        assert_eq!(change.content.as_deref(), Some(b"# Modified".as_ref()));
    }

    // -----------------------------------------------------------------------
    // Deleted files
    // -----------------------------------------------------------------------

    #[test]
    fn collect_deleted_file() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_id = WorkspaceId::new("del-ws").unwrap();
        let info = backend.create(&ws_id, &epoch).unwrap();

        fs::remove_file(info.path.join("README.md")).unwrap();

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id.clone()]).unwrap();
        let ps = &results[0];

        assert_eq!(ps.change_count(), 1);
        let change = &ps.changes[0];
        assert_eq!(change.path, PathBuf::from("README.md"));
        assert!(matches!(change.kind, ChangeKind::Deleted));
        assert!(change.content.is_none(), "deletions have no content");
    }

    /// Deletion-only workspace: PatchSet reports all deletions, none are filtered.
    #[test]
    fn collect_deletion_only_workspace() {
        let (temp_dir, epoch) = setup_git_repo();
        let root = temp_dir.path();
        let backend = GitWorktreeBackend::new(root.to_path_buf());

        // Add a second tracked file so we can delete both later.
        fs::write(root.join("lib.rs"), "pub fn lib() {}").unwrap();
        Command::new("git")
            .args(["add", "lib.rs"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Add lib.rs"])
            .current_dir(root)
            .output()
            .unwrap();
        let epoch2 = EpochId::new(&git_head_oid(root)).unwrap();

        let ws_id = WorkspaceId::new("del-only").unwrap();
        let info = backend.create(&ws_id, &epoch2).unwrap();

        // Delete both tracked files.
        fs::remove_file(info.path.join("README.md")).unwrap();
        fs::remove_file(info.path.join("lib.rs")).unwrap();

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id.clone()]).unwrap();
        let ps = &results[0];

        assert!(
            ps.is_deletion_only(),
            "expected deletion-only: {:?}",
            ps.changes
        );
        assert_eq!(ps.deleted_count(), 2);
        assert_eq!(ps.added_count(), 0);
        assert_eq!(ps.modified_count(), 0);
        for change in &ps.changes {
            assert!(change.content.is_none(), "deletions should have no content");
        }
    }

    // -----------------------------------------------------------------------
    // Multiple workspaces with various change patterns
    // -----------------------------------------------------------------------

    /// Collect from 3 workspaces with disjoint, mixed, and empty changes.
    #[test]
    fn collect_three_workspaces_various_patterns() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());

        // Workspace A: adds a new file.
        let ws_a = WorkspaceId::new("ws-a").unwrap();
        let info_a = backend.create(&ws_a, &epoch).unwrap();
        fs::write(info_a.path.join("feature_a.rs"), "pub fn a() {}").unwrap();

        // Workspace B: modifies README and adds a file.
        let ws_b = WorkspaceId::new("ws-b").unwrap();
        let info_b = backend.create(&ws_b, &epoch).unwrap();
        fs::write(info_b.path.join("README.md"), "# Updated by B").unwrap();
        fs::write(info_b.path.join("feature_b.rs"), "pub fn b() {}").unwrap();

        // Workspace C: no changes.
        let ws_c = WorkspaceId::new("ws-c").unwrap();
        backend.create(&ws_c, &epoch).unwrap();

        let ids = vec![ws_a.clone(), ws_b.clone(), ws_c.clone()];
        let results = collect_snapshots(temp_dir.path(), &backend, &ids).unwrap();

        assert_eq!(results.len(), 3, "should have one PatchSet per workspace");

        let ps_a = &results[0];
        let ps_b = &results[1];
        let ps_c = &results[2];

        // Workspace A: 1 added
        assert_eq!(ps_a.workspace_id, ws_a);
        assert_eq!(ps_a.change_count(), 1);
        assert!(matches!(ps_a.changes[0].kind, ChangeKind::Added));
        assert_eq!(ps_a.changes[0].path, PathBuf::from("feature_a.rs"));

        // Workspace B: 1 modified + 1 added = 2 total, sorted by path
        assert_eq!(ps_b.workspace_id, ws_b);
        assert_eq!(ps_b.change_count(), 2);
        // Sorted: README.md < feature_b.rs
        assert_eq!(ps_b.changes[0].path, PathBuf::from("README.md"));
        assert!(matches!(ps_b.changes[0].kind, ChangeKind::Modified));
        assert_eq!(ps_b.changes[1].path, PathBuf::from("feature_b.rs"));
        assert!(matches!(ps_b.changes[1].kind, ChangeKind::Added));

        // Workspace C: empty
        assert_eq!(ps_c.workspace_id, ws_c);
        assert!(ps_c.is_empty());
    }

    // -----------------------------------------------------------------------
    // Ordering: PatchSets match workspace_ids order
    // -----------------------------------------------------------------------

    #[test]
    fn collect_preserves_workspace_order() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());

        let names = ["zulu", "alpha", "mike"];
        let ids: Vec<WorkspaceId> = names.iter().map(|n| WorkspaceId::new(n).unwrap()).collect();

        for ws_id in &ids {
            backend.create(ws_id, &epoch).unwrap();
        }

        let results = collect_snapshots(temp_dir.path(), &backend, &ids).unwrap();

        assert_eq!(results.len(), 3);
        for (i, ws_id) in ids.iter().enumerate() {
            assert_eq!(
                &results[i].workspace_id, ws_id,
                "PatchSet[{i}] should match input order"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Content correctness
    // -----------------------------------------------------------------------

    #[test]
    fn collect_content_matches_file() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_id = WorkspaceId::new("content-ws").unwrap();
        let info = backend.create(&ws_id, &epoch).unwrap();

        let expected = b"hello world\n";
        fs::write(info.path.join("hello.txt"), expected).unwrap();

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id]).unwrap();
        let change = &results[0].changes[0];

        assert_eq!(
            change.content.as_deref(),
            Some(expected.as_ref()),
            "content should match what was written"
        );
    }

    // -----------------------------------------------------------------------
    // Error: nonexistent workspace
    // -----------------------------------------------------------------------

    #[test]
    fn collect_nonexistent_workspace_returns_error() {
        let (temp_dir, _epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_id = WorkspaceId::new("no-such").unwrap();

        let err = collect_snapshots(temp_dir.path(), &backend, &[ws_id]).unwrap_err();
        match err {
            CollectError::SnapshotFailed { workspace_id, .. } => {
                assert_eq!(workspace_id.as_str(), "no-such");
            }
            other => panic!("expected SnapshotFailed, got {other}"),
        }
    }

    // -----------------------------------------------------------------------
    // Phase 3: FileId + blob OID enrichment
    // -----------------------------------------------------------------------

    /// Added files should receive a fresh (non-None) FileId.
    #[test]
    fn collect_added_file_has_file_id() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_id = WorkspaceId::new("fileid-add").unwrap();
        let info = backend.create(&ws_id, &epoch).unwrap();

        fs::write(info.path.join("brand_new.rs"), "pub fn new() {}").unwrap();

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id]).unwrap();
        let change = &results[0].changes[0];

        assert!(
            change.file_id.is_some(),
            "added file should receive a fresh FileId"
        );
        assert!(
            matches!(change.kind, ChangeKind::Added),
            "kind should be Added"
        );
    }

    /// Added files should have a blob OID computed via git hash-object.
    #[test]
    fn collect_added_file_has_blob_oid() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_id = WorkspaceId::new("blob-add").unwrap();
        let info = backend.create(&ws_id, &epoch).unwrap();

        fs::write(info.path.join("blob_test.rs"), "pub fn blob() {}").unwrap();

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id]).unwrap();
        let change = &results[0].changes[0];

        assert!(
            change.blob.is_some(),
            "added file should have a blob OID from git hash-object"
        );
    }

    /// Modified files should have a blob OID that reflects the new content.
    #[test]
    fn collect_modified_file_has_blob_oid() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_id = WorkspaceId::new("blob-mod").unwrap();
        let info = backend.create(&ws_id, &epoch).unwrap();

        fs::write(info.path.join("README.md"), "# Modified content").unwrap();

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id]).unwrap();
        let change = &results[0].changes[0];

        assert!(
            matches!(change.kind, ChangeKind::Modified),
            "kind should be Modified"
        );
        assert!(
            change.blob.is_some(),
            "modified file should have a blob OID"
        );
    }

    /// Deleted files should NOT have a blob OID (no content was written).
    #[test]
    fn collect_deleted_file_has_no_blob_oid() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_id = WorkspaceId::new("blob-del").unwrap();
        let info = backend.create(&ws_id, &epoch).unwrap();

        fs::remove_file(info.path.join("README.md")).unwrap();

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id]).unwrap();
        let change = &results[0].changes[0];

        assert!(
            matches!(change.kind, ChangeKind::Deleted),
            "kind should be Deleted"
        );
        assert!(change.blob.is_none(), "deleted file should have no blob OID");
    }

    /// Two different workspaces adding a file with identical content should
    /// produce the same blob OID — demonstrating content-addressable identity.
    #[test]
    fn collect_same_content_produces_same_blob_oid() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());

        let content = b"pub fn shared() {}\n";

        let ws_a = WorkspaceId::new("same-blob-a").unwrap();
        let info_a = backend.create(&ws_a, &epoch).unwrap();
        fs::write(info_a.path.join("shared.rs"), content).unwrap();

        let ws_b = WorkspaceId::new("same-blob-b").unwrap();
        let info_b = backend.create(&ws_b, &epoch).unwrap();
        fs::write(info_b.path.join("shared.rs"), content).unwrap();

        let results_a =
            collect_snapshots(temp_dir.path(), &backend, &[ws_a]).unwrap();
        let results_b =
            collect_snapshots(temp_dir.path(), &backend, &[ws_b]).unwrap();

        let blob_a = results_a[0].changes[0].blob.as_ref();
        let blob_b = results_b[0].changes[0].blob.as_ref();

        assert!(blob_a.is_some(), "ws_a should have a blob OID");
        assert!(blob_b.is_some(), "ws_b should have a blob OID");
        assert_eq!(
            blob_a, blob_b,
            "same content should produce the same blob OID (content-addressable)"
        );
    }

    /// Modified files look up FileId from the epoch FileIdMap when available.
    #[test]
    fn collect_modified_file_uses_file_id_from_map() {
        use crate::model::file_id::FileIdMap;
        use crate::model::patch::FileId;
        use std::path::Path;

        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());

        // Pre-populate .manifold/fileids with a known FileId for README.md.
        let known_id = FileId::new(0xdead_beef_cafe_babe_1234_5678_9abc_def0);
        let fileids_path = temp_dir.path().join(".manifold").join("fileids");
        let mut map = FileIdMap::new();
        map.track_new("README.md".into()).unwrap();
        // Replace the random id with our known id by rebuilding.
        let mut map2 = FileIdMap::new();
        // Manually insert: we use a workaround since track_new is random.
        // Build the map via save+reload with a known value.
        let json = format!(
            r#"[{{"path":"README.md","file_id":"{}"}}]"#,
            known_id
        );
        fs::create_dir_all(fileids_path.parent().unwrap()).unwrap();
        fs::write(&fileids_path, &json).unwrap();
        let _ = map2; // unused

        let ws_id = WorkspaceId::new("fileid-mod").unwrap();
        let info = backend.create(&ws_id, &epoch).unwrap();
        fs::write(info.path.join("README.md"), "# Updated").unwrap();

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id]).unwrap();
        let change = &results[0].changes[0];

        assert_eq!(
            change.file_id,
            Some(known_id),
            "modified file should inherit FileId from epoch FileIdMap"
        );
    }
}
