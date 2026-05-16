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
//! # `FileId` and blob OID enrichment (Phase 3+)
//!
//! When `repo_root` is provided, `collect_snapshots` enriches each
//! [`FileChange`] with:
//!
//! - `file_id`: looked up from `.manifold/fileids` for Modified/Deleted files
//!   (files that existed in the epoch). Added files receive a fresh random
//!   [`FileId`]. If the fileids file is absent, `FileIds` are omitted.
//! - `blob`: the git blob OID for the new content, computed via
//!   `git hash-object -w --stdin`. Enables O(1) hash-equality checks in the
//!   resolve step.

use std::fmt;
use std::path::{Path, PathBuf};

use maw_git::{EntryMode as GitEntryMode, GitRepo, GixRepo};

use crate::backend::WorkspaceBackend;
use crate::model::file_id::FileIdMap;
use crate::model::patch::FileId;
use crate::model::types::{EpochId, GitOid, WorkspaceId};

use super::types::{ChangeKind, EntryMode, FileChange, PatchSet};

/// Collected file bytes plus an optional git tree-entry mode.
///
/// `None` mode means the producer could not determine the mode (rare; e.g.
/// `symlink_metadata` failed while a plain read succeeded). Downstream then
/// degrades to its prior default rather than failing the merge.
type ContentAndMode = (Vec<u8>, Option<EntryMode>);

// ---------------------------------------------------------------------------
// CollectError
// ---------------------------------------------------------------------------

/// Errors that can occur during the collect step.
#[derive(Debug)]
#[allow(clippy::enum_variant_names)]
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
                    "snapshot failed for workspace '{workspace_id}': {reason}"
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
                    "epoch query failed for workspace '{workspace_id}': {reason}"
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
///   - Load the epoch `FileId` map from `<repo_root>/.manifold/fileids`.
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
    // status.base_epoch is a BaseEpoch; the downstream merge APIs (PatchSet,
    // phantom-deletion check) still operate on generic EpochId, so convert.
    let epoch = status.base_epoch.to_epoch_id();

    // Step 3: Short-circuit for empty workspaces.
    if snapshot.is_empty() {
        return Ok(PatchSet::new(ws_id.clone(), epoch, vec![]));
    }

    // Step 4: Build FileChanges, reading content for non-deletions.
    let ws_path = backend.workspace_path(ws_id);
    let capacity = snapshot.change_count();
    let mut changes = Vec::with_capacity(capacity);

    // Open gix-backed repos once for all hot-path operations. Both can fail
    // gracefully — the helpers fall back to safe defaults when a repo is
    // unavailable, preserving prior CLI-era resilience.
    let root_repo = GixRepo::open(repo_root).ok();
    let ws_repo = GixRepo::open(&ws_path).ok();

    // Check if workspace HEAD has advanced beyond the epoch. If so, the
    // workspace has committed changes and we should read blobs from HEAD
    // (which holds pointer bytes for LFS files) rather than the working tree
    // (which holds smudged binary content).
    let ws_has_commits = ws_repo
        .as_ref()
        .is_some_and(|r| ws_head_differs_from_epoch(r, &epoch));

    // Added files: read content + mode, generate fresh FileId, compute blob OID.
    for path in &snapshot.added {
        let Some((content, mode)) =
            read_content_and_mode(ws_repo.as_ref(), &ws_path, path, ws_id, ws_has_commits)?
        else {
            // Ignore directory entries (for example untracked nested git dirs)
            // that can appear in porcelain outputs as "path/".
            continue;
        };
        let blob = root_repo
            .as_ref()
            .and_then(|r| git_hash_object(r, &content));
        // Assign a fresh FileId for new files. The FileIdMap for the epoch
        // won't have an entry yet; the FileId is minted here and would be
        // persisted by the workspace's oplog in a full implementation.
        let file_id = Some(FileId::random());
        changes.push(FileChange::with_mode(
            path.clone(),
            ChangeKind::Added,
            Some(content),
            file_id,
            blob,
            mode,
        ));
    }

    // Modified files: read current content + mode, look up existing FileId,
    // compute blob OID.
    for path in &snapshot.modified {
        let Some((content, mode)) =
            read_content_and_mode(ws_repo.as_ref(), &ws_path, path, ws_id, ws_has_commits)?
        else {
            // Ignore non-file paths to keep collect robust against directory-only
            // workspace entries.
            continue;
        };
        let blob = root_repo
            .as_ref()
            .and_then(|r| git_hash_object(r, &content));
        // Modified files existed in the epoch, so their FileId is in the map.
        let file_id = file_id_map.id_for_path(path);
        changes.push(FileChange::with_mode(
            path.clone(),
            ChangeKind::Modified,
            Some(content),
            file_id,
            blob,
            mode,
        ));
    }

    // Deleted files: no content; look up FileId from epoch map.
    //
    // Phantom-deletion filter: the snapshot diffs the working tree against the
    // *current* (global) epoch, but this workspace may be based on a different
    // commit. Files added by another path (e.g., merged into a branch-attached
    // workspace target) that the worker never had show up as "Deleted" here.
    // These are phantom deletions — skip them so the merge engine doesn't
    // remove files the worker never touched.
    //
    // The right tree to check existence against is `epoch` — i.e.
    // `status.base_epoch`, the workspace's per-workspace baseline ref. That
    // ref tracks the commit the workspace was created from (and advanced to
    // by sync / auto-rebase), and is unaffected by agent commits inside the
    // workspace — so a committed `git rm foo` still correctly resolves "did
    // foo exist at this workspace's creation point?" without confusing the
    // filter.
    //
    // bn-3bl2: previously this used `merge_base(epoch, global_current_epoch)`
    // to compute a "creation epoch". When the global epoch lagged behind the
    // workspace's per-workspace baseline (the typical shape for cleanup
    // workspaces created from a branch-attached merge target), the merge-base
    // walked past the workspace's true creation point all the way back to
    // the global epoch, where the file never existed — so real deletions
    // were silently dropped. The merge-base step solved a problem that
    // no longer exists once `epoch` is the per-workspace baseline.
    for path in &snapshot.deleted {
        if !root_repo
            .as_ref()
            .is_some_and(|r| path_exists_at_commit(r, &epoch, path))
        {
            // File doesn't exist at the workspace's creation epoch — it was
            // added after this workspace was created. Not a real deletion.
            continue;
        }
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

/// Check whether a file path exists in a given git commit's tree.
///
/// Uses gix rev-parse on `<commit>:<path>` — resolves to the blob OID if the
/// path exists at that commit, or `None` otherwise. Returns false on any
/// failure (repo error, non-UTF-8 path) so the phantom-deletion filter
/// degrades safely.
fn path_exists_at_commit(repo: &GixRepo, commit: &EpochId, path: &Path) -> bool {
    let Some(path_str) = path.to_str() else {
        return false;
    };
    matches!(
        repo.rev_parse_opt(&format!("{}:{path_str}", commit.as_str())),
        Ok(Some(_))
    )
}

/// Read file content **and git tree-entry mode** for the merge, preferring
/// committed blobs when the workspace has commits ahead of the epoch.
///
/// When `ws_has_commits` is true, the workspace HEAD has advanced beyond the
/// epoch — meaning the user/agent committed changes. In this case we read
/// from the committed tree (`HEAD:<path>`) to get the clean (non-smudged)
/// content **and the committed mode**. This is critical for LFS correctness:
/// the working tree contains smudged real binary content, but the committed
/// blob holds the LFS pointer bytes that the merge must store.
///
/// When `ws_has_commits` is false, all changes are uncommitted working-tree
/// edits, so we read directly from disk and derive the mode from
/// `symlink_metadata` (symlink / executable bit), mirroring the canonical
/// `worktree_state_commit` logic in `maw-git`.
///
/// bn-1tl6: previously this returned content only and never recorded a mode,
/// so every `FileChange` from the production collect path had `mode == None`.
/// Downstream (`apply.rs`) then defaulted new/added paths to `Blob` (100644)
/// and reused the base-epoch mode for modified paths — silently corrupting the
/// executable bit and symlink/file mode flips in the *committed* merge tree
/// (a Prime-Invariant violation).
///
/// Returns `Ok(None)` for directory entries (which the caller skips). The
/// returned mode is `None` only when it genuinely cannot be determined (e.g.
/// a path read from disk whose metadata is unavailable but content read
/// succeeded — rare); downstream then falls back to its prior behavior.
fn read_content_and_mode(
    ws_repo: Option<&GixRepo>,
    ws_path: &Path,
    rel_path: &Path,
    ws_id: &WorkspaceId,
    ws_has_commits: bool,
) -> Result<Option<ContentAndMode>, CollectError> {
    if ws_has_commits
        && let Some(repo) = ws_repo
        && let Some((blob, mode)) = read_committed_blob_and_mode(repo, rel_path)
    {
        return Ok(Some((blob, Some(EntryMode::from(mode)))));
    }
    // Fallback: workspace has no commits, or file not in HEAD (uncommitted).
    read_workspace_file_and_mode(ws_path, rel_path, ws_id)
}

/// Check whether the workspace HEAD has advanced beyond the epoch.
///
/// Returns `true` if the workspace has committed changes (HEAD != epoch),
/// meaning we should read from the committed tree rather than the working
/// tree to avoid LFS smudge contamination.
fn ws_head_differs_from_epoch(ws_repo: &GixRepo, epoch: &EpochId) -> bool {
    match ws_repo.rev_parse_opt("HEAD") {
        Ok(Some(head)) => head.to_string() != epoch.as_str(),
        _ => false,
    }
}

/// Read a blob **and its tree-entry mode** from `HEAD:<rel_path>` in the
/// workspace worktree.
///
/// Returns `None` if the file is not in the HEAD tree (e.g., untracked), if
/// the path is not valid UTF-8, if it names a non-blob entry (subtree /
/// submodule), or if any gix lookup fails.
///
/// The committed blob is already mode-correct for symlinks: `HEAD:<symlink>`
/// stores the link *target text* as the blob, which is exactly what a
/// `Link`-mode tree entry must contain. No special-casing is needed here
/// (contrast the working-tree path, which must `read_link`).
fn read_committed_blob_and_mode(
    ws_repo: &GixRepo,
    rel_path: &Path,
) -> Option<(Vec<u8>, GitEntryMode)> {
    let path_str = rel_path.to_str()?;
    let head = ws_repo.rev_parse_opt("HEAD").ok()??;
    let (mode, _oid, data) = ws_repo.read_blob_at_path(head, path_str).ok()??;
    Some((data, mode))
}

/// Read the current content of a file from a workspace's working tree, along
/// with its git tree-entry mode.
///
/// Mode derivation mirrors the canonical `worktree_state_commit` logic in
/// `maw-git/src/stash_impl.rs`:
///
/// - **Symlink** → [`EntryMode::Link`]. The git blob for a symlink is the
///   *link target text*, not the target file's content. `std::fs::read`
///   follows the link and would read the target file's bytes — corrupting
///   the blob (a regular file's content masquerading as a symlink target).
///   So for symlinks we read the link via `std::fs::read_link` and use the
///   target path's raw bytes (on unix, the `OsStr` bytes verbatim — no lossy
///   UTF-8 conversion, matching `git stash create`).
/// - **Regular file with any execute bit** (`mode & 0o111 != 0` on unix) →
///   [`EntryMode::BlobExecutable`].
/// - **Regular file otherwise** → [`EntryMode::Blob`].
///
/// Returns `Ok(None)` for directories. If `symlink_metadata` itself fails but
/// a plain `read` succeeds, the mode is reported as `None` (rare; downstream
/// degrades to its prior default) rather than failing the whole merge.
fn read_workspace_file_and_mode(
    ws_path: &Path,
    rel_path: &Path,
    ws_id: &WorkspaceId,
) -> Result<Option<ContentAndMode>, CollectError> {
    let full_path = ws_path.join(rel_path);

    // Path enumerated by snapshot but missing on disk, or metadata
    // unavailable → `None`; fall back to the plain-read path below, which
    // will surface a precise error or skip a directory.
    let meta = std::fs::symlink_metadata(&full_path).ok();

    if meta.as_ref().is_some_and(std::fs::Metadata::is_dir) || full_path.is_dir() {
        return Ok(None);
    }

    // Symlink: the blob must be the link target text, NOT the (followed)
    // target file's content. Mirror `worktree_state_commit`.
    if meta.as_ref().is_some_and(std::fs::Metadata::is_symlink) {
        let target = std::fs::read_link(&full_path).map_err(|e| CollectError::ReadFailed {
            workspace_id: ws_id.clone(),
            path: rel_path.to_path_buf(),
            reason: format!("read symlink: {e}"),
        })?;
        #[cfg(unix)]
        let bytes = {
            use std::os::unix::ffi::OsStrExt;
            target.as_os_str().as_bytes().to_vec()
        };
        #[cfg(not(unix))]
        let bytes = target.to_string_lossy().into_owned().into_bytes();
        return Ok(Some((bytes, Some(EntryMode::Link))));
    }

    #[cfg(unix)]
    let exec_mode = meta.as_ref().and_then(|m| {
        use std::os::unix::fs::PermissionsExt;
        if m.is_file() {
            Some(if m.permissions().mode() & 0o111 != 0 {
                EntryMode::BlobExecutable
            } else {
                EntryMode::Blob
            })
        } else {
            None
        }
    });
    #[cfg(not(unix))]
    let exec_mode = meta
        .as_ref()
        .and_then(|m| m.is_file().then_some(EntryMode::Blob));

    match std::fs::read(&full_path) {
        Ok(content) => Ok(Some((content, exec_mode))),
        Err(e) if e.kind() == std::io::ErrorKind::IsADirectory => Ok(None),
        Err(e) => Err(CollectError::ReadFailed {
            workspace_id: ws_id.clone(),
            path: rel_path.to_path_buf(),
            reason: e.to_string(),
        }),
    }
}

/// Write `content` to the git object store and return its blob OID.
///
/// Returns `None` on any failure (`write_blob` error, OID round-trip failure)
/// — callers treat a missing blob OID as a degraded-mode fallback, not a
/// hard error.
fn git_hash_object(repo: &GixRepo, content: &[u8]) -> Option<GitOid> {
    let oid = repo.write_blob(content).ok()?;
    GitOid::new(&oid.to_string()).ok()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
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
        // bn-5rdz: use shared init + seed-commit helper from maw-git.
        let (temp_dir, _root, oid_str) = maw_git::test_support::init_test_repo_with_commit();
        let epoch = EpochId::new(&oid_str).expect("operation should succeed");
        (temp_dir, epoch)
    }

    /// Return the current HEAD OID of a git repo.
    fn git_head_oid(root: &std::path::Path) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .expect("operation should succeed");
        String::from_utf8(out.stdout)
            .expect("operation should succeed")
            .trim()
            .to_owned()
    }

    // -----------------------------------------------------------------------
    // Error display
    // -----------------------------------------------------------------------

    #[test]
    fn collect_error_display_snapshot_failed() {
        let ws_id = WorkspaceId::new("alpha").expect("operation should succeed");
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
        let ws_id = WorkspaceId::new("beta").expect("operation should succeed");
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
        let ws_id = WorkspaceId::new("gamma").expect("operation should succeed");
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
        let ws_id = WorkspaceId::new("empty-ws").expect("operation should succeed");
        backend
            .create(&ws_id, &epoch)
            .expect("operation should succeed");

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id.clone()])
            .expect("operation should succeed");

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
        let ws_id = WorkspaceId::new("add-ws").expect("operation should succeed");
        let info = backend
            .create(&ws_id, &epoch)
            .expect("operation should succeed");

        fs::write(info.path.join("new.rs"), "fn main() {}").expect("operation should succeed");

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id])
            .expect("operation should succeed");
        let ps = &results[0];

        assert_eq!(ps.change_count(), 1);
        let change = &ps.changes[0];
        assert_eq!(change.path, PathBuf::from("new.rs"));
        assert!(matches!(change.kind, ChangeKind::Added));
        assert_eq!(change.content.as_deref(), Some(b"fn main() {}".as_ref()));
    }

    #[test]
    fn collect_ignores_untracked_directory_entries() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_id = WorkspaceId::new("dir-entry-ws").expect("operation should succeed");
        let info = backend
            .create(&ws_id, &epoch)
            .expect("operation should succeed");

        // Create a nested git directory that porcelain can report as a
        // directory entry (path with trailing slash).
        let nested = info.path.join(".tmp").join("sub");
        fs::create_dir_all(&nested).expect("operation should succeed");
        let out = Command::new("git")
            .args(["init"])
            .current_dir(&nested)
            .output()
            .expect("operation should succeed");
        assert!(
            out.status.success(),
            "git init nested repo failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        // Also add a normal file so the patch set is non-empty.
        fs::write(info.path.join("normal.txt"), "ok\n").expect("operation should succeed");

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id])
            .expect("operation should succeed");
        let ps = &results[0];

        assert!(
            ps.changes
                .iter()
                .all(|c| !c.path.to_string_lossy().ends_with('/')),
            "directory entries must be skipped: {:?}",
            ps.changes
        );
        assert!(
            ps.changes
                .iter()
                .any(|c| c.path == PathBuf::from("normal.txt")),
            "expected regular file to still be collected"
        );
    }

    // -----------------------------------------------------------------------
    // Modified files
    // -----------------------------------------------------------------------

    #[test]
    fn collect_modified_file() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_id = WorkspaceId::new("mod-ws").expect("operation should succeed");
        let info = backend
            .create(&ws_id, &epoch)
            .expect("operation should succeed");

        fs::write(info.path.join("README.md"), "# Modified").expect("operation should succeed");

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id])
            .expect("operation should succeed");
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
        let ws_id = WorkspaceId::new("del-ws").expect("operation should succeed");
        let info = backend
            .create(&ws_id, &epoch)
            .expect("operation should succeed");

        fs::remove_file(info.path.join("README.md")).expect("operation should succeed");

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id])
            .expect("operation should succeed");
        let ps = &results[0];

        assert_eq!(ps.change_count(), 1);
        let change = &ps.changes[0];
        assert_eq!(change.path, PathBuf::from("README.md"));
        assert!(matches!(change.kind, ChangeKind::Deleted));
        assert!(change.content.is_none(), "deletions have no content");
    }

    /// Committed deletion: agent does `git rm` + `git commit` inside the workspace.
    ///
    /// This is a regression test for bn-129d: the merge engine silently dropped
    /// file deletions that were committed (not just staged or working-tree
    /// changes). The phantom-deletion filter was using HEAD (which had the
    /// deletion committed) instead of the creation epoch (where the file still
    /// existed), causing it to incorrectly classify real deletions as phantom.
    #[test]
    fn collect_committed_deletion() {
        let (temp_dir, epoch) = setup_git_repo();
        let root = temp_dir.path();
        let backend = GitWorktreeBackend::new(root.to_path_buf());

        // Set refs/manifold/epoch/current so snapshot() diffs against the epoch.
        Command::new("git")
            .args(["update-ref", "refs/manifold/epoch/current", epoch.as_str()])
            .current_dir(root)
            .output()
            .expect("operation should succeed");

        let ws_id = WorkspaceId::new("committed-del").expect("operation should succeed");
        let info = backend
            .create(&ws_id, &epoch)
            .expect("operation should succeed");

        // Agent commits a deletion inside the workspace (git rm + git commit).
        Command::new("git")
            .args(["rm", "README.md"])
            .current_dir(&info.path)
            .output()
            .expect("operation should succeed");
        Command::new("git")
            .args(["commit", "-m", "delete README.md"])
            .current_dir(&info.path)
            .output()
            .expect("operation should succeed");

        // Verify HEAD has advanced beyond the epoch.
        let ws_head = git_head_oid(&info.path);
        assert_ne!(
            ws_head,
            epoch.as_str(),
            "workspace HEAD should have advanced after commit"
        );

        let results =
            collect_snapshots(root, &backend, &[ws_id]).expect("operation should succeed");
        let ps = &results[0];

        assert_eq!(
            ps.change_count(),
            1,
            "committed deletion should be captured, not silently dropped: {:?}",
            ps.changes
        );
        let change = &ps.changes[0];
        assert_eq!(change.path, PathBuf::from("README.md"));
        assert!(
            matches!(change.kind, ChangeKind::Deleted),
            "change should be Deleted, got {:?}",
            change.kind
        );
        assert!(change.content.is_none(), "deletions have no content");
    }

    /// bn-3bl2 regression: when a workspace's per-workspace baseline is at a
    /// commit *ahead* of the global epoch ref (the typical shape for a
    /// cleanup workspace created from a branch-attached merge target), an
    /// agent's committed deletion of a file that exists at the baseline but
    /// not at the global epoch must still be captured. Previously the
    /// phantom-deletion filter walked back to the global epoch via merge-base
    /// and classified the deletion as phantom because the file didn't exist
    /// at that earlier tree, silently dropping it.
    #[test]
    fn collect_committed_deletion_when_baseline_ahead_of_global_epoch() {
        let (temp_dir, global_epoch) = setup_git_repo();
        let root = temp_dir.path();
        let backend = GitWorktreeBackend::new(root.to_path_buf());

        // Add a second commit that introduces `a.go`. This is the workspace's
        // per-workspace baseline — ahead of `global_epoch`, which is still the
        // initial commit.
        fs::write(root.join("a.go"), "a content").expect("operation should succeed");
        Command::new("git")
            .args(["add", "a.go"])
            .current_dir(root)
            .output()
            .expect("operation should succeed");
        Command::new("git")
            .args(["commit", "-m", "add a.go"])
            .current_dir(root)
            .output()
            .expect("operation should succeed");
        let ws_baseline_oid = git_head_oid(root);
        let ws_baseline = EpochId::new(&ws_baseline_oid).expect("operation should succeed");

        // Wire refs: global epoch lags at the initial commit; the per-workspace
        // baseline points at the commit that has `a.go`.
        Command::new("git")
            .args([
                "update-ref",
                "refs/manifold/epoch/current",
                global_epoch.as_str(),
            ])
            .current_dir(root)
            .output()
            .expect("operation should succeed");

        let ws_id = WorkspaceId::new("ahead-baseline").expect("operation should succeed");
        let info = backend
            .create(&ws_id, &ws_baseline)
            .expect("operation should succeed");
        Command::new("git")
            .args([
                "update-ref",
                "refs/manifold/epoch/ws/ahead-baseline",
                ws_baseline.as_str(),
            ])
            .current_dir(root)
            .output()
            .expect("operation should succeed");

        // Sanity: a.go exists in the workspace worktree (it was at HEAD when
        // the workspace was created).
        assert!(
            info.path.join("a.go").exists(),
            "workspace should start with a.go materialized"
        );

        // Agent commits a deletion inside the workspace.
        Command::new("git")
            .args(["rm", "a.go"])
            .current_dir(&info.path)
            .output()
            .expect("operation should succeed");
        Command::new("git")
            .args(["commit", "-m", "delete a.go"])
            .current_dir(&info.path)
            .output()
            .expect("operation should succeed");

        let results =
            collect_snapshots(root, &backend, &[ws_id]).expect("operation should succeed");
        let ps = &results[0];

        let deletions: Vec<_> = ps
            .changes
            .iter()
            .filter(|c| matches!(c.kind, ChangeKind::Deleted))
            .collect();
        assert_eq!(
            deletions.len(),
            1,
            "deletion of a.go must be captured even though it doesn't exist at the global epoch: {:?}",
            ps.changes
        );
        assert_eq!(deletions[0].path, PathBuf::from("a.go"));
    }

    /// Deletion-only workspace: `PatchSet` reports all deletions, none are filtered.
    #[test]
    fn collect_deletion_only_workspace() {
        let (temp_dir, _epoch) = setup_git_repo();
        let root = temp_dir.path();
        let backend = GitWorktreeBackend::new(root.to_path_buf());

        // Add a second tracked file so we can delete both later.
        fs::write(root.join("lib.rs"), "pub fn lib() {}").expect("operation should succeed");
        Command::new("git")
            .args(["add", "lib.rs"])
            .current_dir(root)
            .output()
            .expect("operation should succeed");
        Command::new("git")
            .args(["commit", "-m", "Add lib.rs"])
            .current_dir(root)
            .output()
            .expect("operation should succeed");
        let epoch2 = EpochId::new(&git_head_oid(root)).expect("operation should succeed");

        let ws_id = WorkspaceId::new("del-only").expect("operation should succeed");
        let info = backend
            .create(&ws_id, &epoch2)
            .expect("operation should succeed");

        // Delete both tracked files.
        fs::remove_file(info.path.join("README.md")).expect("operation should succeed");
        fs::remove_file(info.path.join("lib.rs")).expect("operation should succeed");

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id])
            .expect("operation should succeed");
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
        let ws_a = WorkspaceId::new("ws-a").expect("operation should succeed");
        let info_a = backend
            .create(&ws_a, &epoch)
            .expect("operation should succeed");
        fs::write(info_a.path.join("feature_a.rs"), "pub fn a() {}")
            .expect("operation should succeed");

        // Workspace B: modifies README and adds a file.
        let ws_b = WorkspaceId::new("ws-b").expect("operation should succeed");
        let info_b = backend
            .create(&ws_b, &epoch)
            .expect("operation should succeed");
        fs::write(info_b.path.join("README.md"), "# Updated by B")
            .expect("operation should succeed");
        fs::write(info_b.path.join("feature_b.rs"), "pub fn b() {}")
            .expect("operation should succeed");

        // Workspace C: no changes.
        let ws_c = WorkspaceId::new("ws-c").expect("operation should succeed");
        backend
            .create(&ws_c, &epoch)
            .expect("operation should succeed");

        let ids = vec![ws_a.clone(), ws_b.clone(), ws_c.clone()];
        let results =
            collect_snapshots(temp_dir.path(), &backend, &ids).expect("operation should succeed");

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
        let ids: Vec<WorkspaceId> = names
            .iter()
            .map(|n| WorkspaceId::new(n).expect("operation should succeed"))
            .collect();

        for ws_id in &ids {
            backend
                .create(ws_id, &epoch)
                .expect("operation should succeed");
        }

        let results =
            collect_snapshots(temp_dir.path(), &backend, &ids).expect("operation should succeed");

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
        let ws_id = WorkspaceId::new("content-ws").expect("operation should succeed");
        let info = backend
            .create(&ws_id, &epoch)
            .expect("operation should succeed");

        let expected = b"hello world\n";
        fs::write(info.path.join("hello.txt"), expected).expect("operation should succeed");

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id])
            .expect("operation should succeed");
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
        let ws_id = WorkspaceId::new("no-such").expect("operation should succeed");

        let err = collect_snapshots(temp_dir.path(), &backend, &[ws_id])
            .expect_err("operation should fail");
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

    /// Added files should receive a fresh (non-None) `FileId`.
    #[test]
    fn collect_added_file_has_file_id() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_id = WorkspaceId::new("fileid-add").expect("operation should succeed");
        let info = backend
            .create(&ws_id, &epoch)
            .expect("operation should succeed");

        fs::write(info.path.join("brand_new.rs"), "pub fn new() {}")
            .expect("operation should succeed");

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id])
            .expect("operation should succeed");
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
        let ws_id = WorkspaceId::new("blob-add").expect("operation should succeed");
        let info = backend
            .create(&ws_id, &epoch)
            .expect("operation should succeed");

        fs::write(info.path.join("blob_test.rs"), "pub fn blob() {}")
            .expect("operation should succeed");

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id])
            .expect("operation should succeed");
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
        let ws_id = WorkspaceId::new("blob-mod").expect("operation should succeed");
        let info = backend
            .create(&ws_id, &epoch)
            .expect("operation should succeed");

        fs::write(info.path.join("README.md"), "# Modified content")
            .expect("operation should succeed");

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id])
            .expect("operation should succeed");
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
        let ws_id = WorkspaceId::new("blob-del").expect("operation should succeed");
        let info = backend
            .create(&ws_id, &epoch)
            .expect("operation should succeed");

        fs::remove_file(info.path.join("README.md")).expect("operation should succeed");

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id])
            .expect("operation should succeed");
        let change = &results[0].changes[0];

        assert!(
            matches!(change.kind, ChangeKind::Deleted),
            "kind should be Deleted"
        );
        assert!(
            change.blob.is_none(),
            "deleted file should have no blob OID"
        );
    }

    /// Two different workspaces adding a file with identical content should
    /// produce the same blob OID — demonstrating content-addressable identity.
    #[test]
    fn collect_same_content_produces_same_blob_oid() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());

        let content = b"pub fn shared() {}\n";

        let ws_a = WorkspaceId::new("same-blob-a").expect("operation should succeed");
        let info_a = backend
            .create(&ws_a, &epoch)
            .expect("operation should succeed");
        fs::write(info_a.path.join("shared.rs"), content).expect("operation should succeed");

        let ws_b = WorkspaceId::new("same-blob-b").expect("operation should succeed");
        let info_b = backend
            .create(&ws_b, &epoch)
            .expect("operation should succeed");
        fs::write(info_b.path.join("shared.rs"), content).expect("operation should succeed");

        let results_a = collect_snapshots(temp_dir.path(), &backend, &[ws_a])
            .expect("operation should succeed");
        let results_b = collect_snapshots(temp_dir.path(), &backend, &[ws_b])
            .expect("operation should succeed");

        let blob_a = results_a[0].changes[0].blob.as_ref();
        let blob_b = results_b[0].changes[0].blob.as_ref();

        assert!(blob_a.is_some(), "ws_a should have a blob OID");
        assert!(blob_b.is_some(), "ws_b should have a blob OID");
        assert_eq!(
            blob_a, blob_b,
            "same content should produce the same blob OID (content-addressable)"
        );
    }

    /// Modified files look up `FileId` from the epoch `FileIdMap` when available.
    #[test]
    fn collect_modified_file_uses_file_id_from_map() {
        use crate::model::patch::FileId;

        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());

        // Pre-populate .manifold/fileids with a known FileId for README.md.
        let known_id = FileId::new(0xdead_beef_cafe_babe_1234_5678_9abc_def0);
        let fileids_path = temp_dir.path().join(".manifold").join("fileids");
        // Replace the random id with our known id by rebuilding.
        // Manually insert: we use a workaround since track_new is random.
        // Build the map via save+reload with a known value.
        let json = format!(r#"[{{"path":"README.md","file_id":"{known_id}"}}]"#);
        fs::create_dir_all(fileids_path.parent().expect("operation should succeed"))
            .expect("operation should succeed");
        fs::write(&fileids_path, &json).expect("operation should succeed");

        let ws_id = WorkspaceId::new("fileid-mod").expect("operation should succeed");
        let info = backend
            .create(&ws_id, &epoch)
            .expect("operation should succeed");
        fs::write(info.path.join("README.md"), "# Updated").expect("operation should succeed");

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id])
            .expect("operation should succeed");
        let change = &results[0].changes[0];

        assert_eq!(
            change.file_id,
            Some(known_id),
            "modified file should inherit FileId from epoch FileIdMap"
        );
    }

    // -----------------------------------------------------------------------
    // bn-1tl6: file modes (exec bit, symlink) must survive collect
    //
    // Before bn-1tl6, collect_one always built FileChanges with mode == None,
    // so the merge engine dropped the executable bit on new scripts/binaries
    // and corrupted symlink<->file mode flips in the committed merge tree
    // (a Prime-Invariant violation). These tests pin the four producing
    // paths: worktree exec, worktree symlink, committed-HEAD exec,
    // committed-HEAD symlink — content AND mode.
    // -----------------------------------------------------------------------

    /// An uncommitted (working-tree) executable file collected as Added must
    /// carry `mode == Some(BlobExecutable)`.
    #[cfg(unix)]
    #[test]
    fn collect_worktree_added_executable_has_exec_mode() {
        use crate::merge::types::EntryMode;
        use std::os::unix::fs::PermissionsExt;

        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_id = WorkspaceId::new("wt-exec-add").expect("operation should succeed");
        let info = backend
            .create(&ws_id, &epoch)
            .expect("operation should succeed");

        let script = info.path.join("tool.sh");
        fs::write(&script, "#!/bin/sh\necho hi\n").expect("operation should succeed");
        let mut perms = fs::metadata(&script)
            .expect("operation should succeed")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).expect("operation should succeed");

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id])
            .expect("operation should succeed");
        let change = results[0]
            .changes
            .iter()
            .find(|c| c.path == PathBuf::from("tool.sh"))
            .expect("tool.sh should be collected");

        assert!(matches!(change.kind, ChangeKind::Added));
        assert_eq!(
            change.mode,
            Some(EntryMode::BlobExecutable),
            "new worktree executable must keep the exec bit (bn-1tl6)"
        );
        assert_eq!(
            change.content.as_deref(),
            Some(b"#!/bin/sh\necho hi\n".as_ref())
        );
    }

    /// An uncommitted (working-tree) symlink collected as Added must carry
    /// `mode == Some(Link)` AND its content must be the link *target text*,
    /// not the (followed) target file's bytes.
    #[cfg(unix)]
    #[test]
    fn collect_worktree_added_symlink_has_link_mode_and_target_content() {
        use crate::merge::types::EntryMode;

        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_id = WorkspaceId::new("wt-link-add").expect("operation should succeed");
        let info = backend
            .create(&ws_id, &epoch)
            .expect("operation should succeed");

        // Target file with DISTINCT content so a follow-the-link bug is
        // detectable: a corrupted symlink would carry "TARGET BYTES\n"
        // instead of the link path "tool.sh".
        fs::write(info.path.join("tool.sh"), "TARGET BYTES\n").expect("operation should succeed");
        std::os::unix::fs::symlink("tool.sh", info.path.join("alias.sh"))
            .expect("operation should succeed");

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id])
            .expect("operation should succeed");
        let link = results[0]
            .changes
            .iter()
            .find(|c| c.path == PathBuf::from("alias.sh"))
            .expect("alias.sh should be collected");

        assert!(matches!(link.kind, ChangeKind::Added));
        assert_eq!(
            link.mode,
            Some(EntryMode::Link),
            "new worktree symlink must be Link mode (bn-1tl6)"
        );
        assert_eq!(
            link.content.as_deref(),
            Some(b"tool.sh".as_ref()),
            "symlink blob must be the link target text, NOT the target file's content (bn-1tl6)"
        );
    }

    /// A committed (HEAD-tree) executable collected as Added must carry
    /// `mode == Some(BlobExecutable)` (read from the HEAD tree entry, not the
    /// working tree).
    #[cfg(unix)]
    #[test]
    fn collect_committed_added_executable_has_exec_mode() {
        use crate::merge::types::EntryMode;
        use std::os::unix::fs::PermissionsExt;

        let (temp_dir, epoch) = setup_git_repo();
        let root = temp_dir.path();
        let backend = GitWorktreeBackend::new(root.to_path_buf());

        Command::new("git")
            .args(["update-ref", "refs/manifold/epoch/current", epoch.as_str()])
            .current_dir(root)
            .output()
            .expect("operation should succeed");

        let ws_id = WorkspaceId::new("hd-exec-add").expect("operation should succeed");
        let info = backend
            .create(&ws_id, &epoch)
            .expect("operation should succeed");

        let script = info.path.join("tool.sh");
        fs::write(&script, "#!/bin/sh\necho hi\n").expect("operation should succeed");
        let mut perms = fs::metadata(&script)
            .expect("operation should succeed")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).expect("operation should succeed");

        Command::new("git")
            .args(["add", "-A"])
            .current_dir(&info.path)
            .output()
            .expect("operation should succeed");
        Command::new("git")
            .args(["commit", "-m", "add tool.sh"])
            .current_dir(&info.path)
            .output()
            .expect("operation should succeed");

        let ws_head = git_head_oid(&info.path);
        assert_ne!(ws_head, epoch.as_str(), "HEAD should have advanced");

        let results =
            collect_snapshots(root, &backend, &[ws_id]).expect("operation should succeed");
        let change = results[0]
            .changes
            .iter()
            .find(|c| c.path == PathBuf::from("tool.sh"))
            .expect("tool.sh should be collected");

        assert!(matches!(change.kind, ChangeKind::Added));
        assert_eq!(
            change.mode,
            Some(EntryMode::BlobExecutable),
            "committed executable must keep the exec bit from the HEAD tree (bn-1tl6)"
        );
    }

    /// A committed (HEAD-tree) symlink collected as Added must carry
    /// `mode == Some(Link)` and content == link target text. (The committed
    /// blob is already the target text; this pins that the mode is read.)
    #[cfg(unix)]
    #[test]
    fn collect_committed_added_symlink_has_link_mode_and_target_content() {
        use crate::merge::types::EntryMode;

        let (temp_dir, epoch) = setup_git_repo();
        let root = temp_dir.path();
        let backend = GitWorktreeBackend::new(root.to_path_buf());

        Command::new("git")
            .args(["update-ref", "refs/manifold/epoch/current", epoch.as_str()])
            .current_dir(root)
            .output()
            .expect("operation should succeed");

        let ws_id = WorkspaceId::new("hd-link-add").expect("operation should succeed");
        let info = backend
            .create(&ws_id, &epoch)
            .expect("operation should succeed");

        fs::write(info.path.join("tool.sh"), "TARGET BYTES\n").expect("operation should succeed");
        std::os::unix::fs::symlink("tool.sh", info.path.join("alias.sh"))
            .expect("operation should succeed");

        Command::new("git")
            .args(["add", "-A"])
            .current_dir(&info.path)
            .output()
            .expect("operation should succeed");
        Command::new("git")
            .args(["commit", "-m", "add tool.sh + alias.sh"])
            .current_dir(&info.path)
            .output()
            .expect("operation should succeed");

        let ws_head = git_head_oid(&info.path);
        assert_ne!(ws_head, epoch.as_str(), "HEAD should have advanced");

        let results =
            collect_snapshots(root, &backend, &[ws_id]).expect("operation should succeed");
        let link = results[0]
            .changes
            .iter()
            .find(|c| c.path == PathBuf::from("alias.sh"))
            .expect("alias.sh should be collected");

        assert!(matches!(link.kind, ChangeKind::Added));
        assert_eq!(
            link.mode,
            Some(EntryMode::Link),
            "committed symlink must be Link mode read from the HEAD tree (bn-1tl6)"
        );
        assert_eq!(
            link.content.as_deref(),
            Some(b"tool.sh".as_ref()),
            "committed symlink blob is the link target text"
        );
    }

    /// A plain (non-exec) regular file added in the worktree must stay
    /// `mode == Some(Blob)` — the safe case must NOT regress.
    #[cfg(unix)]
    #[test]
    fn collect_worktree_added_regular_file_stays_blob_mode() {
        use crate::merge::types::EntryMode;

        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_id = WorkspaceId::new("wt-reg-add").expect("operation should succeed");
        let info = backend
            .create(&ws_id, &epoch)
            .expect("operation should succeed");

        fs::write(info.path.join("regular.txt"), "realfile\n").expect("operation should succeed");

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id])
            .expect("operation should succeed");
        let change = results[0]
            .changes
            .iter()
            .find(|c| c.path == PathBuf::from("regular.txt"))
            .expect("regular.txt should be collected");

        assert_eq!(
            change.mode,
            Some(EntryMode::Blob),
            "plain new file must remain Blob (100644) — safe case must not regress"
        );
    }

    /// Editing an existing tracked non-exec file (Modified) keeps `Blob`
    /// mode — confirms the modified path also threads the (correct) mode and
    /// the common safe case is unaffected.
    #[cfg(unix)]
    #[test]
    fn collect_worktree_modified_regular_file_stays_blob_mode() {
        use crate::merge::types::EntryMode;

        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_id = WorkspaceId::new("wt-reg-mod").expect("operation should succeed");
        let info = backend
            .create(&ws_id, &epoch)
            .expect("operation should succeed");

        // README.md exists in the seed commit; edit it in place.
        fs::write(info.path.join("README.md"), "# edited\n").expect("operation should succeed");

        let results = collect_snapshots(temp_dir.path(), &backend, &[ws_id])
            .expect("operation should succeed");
        let change = results[0]
            .changes
            .iter()
            .find(|c| c.path == PathBuf::from("README.md"))
            .expect("README.md should be collected");

        assert!(matches!(change.kind, ChangeKind::Modified));
        assert_eq!(
            change.mode,
            Some(EntryMode::Blob),
            "edited plain file must stay Blob (100644)"
        );
    }
}
