//! Stash create/apply built from gix commit/tree primitives.
//!
//! gix does not provide a high-level stash API.
//! We build stash from tree, index, and commit operations.

use std::io::Write;

use gix::bstr::ByteSlice;
use gix::objs::TreeRefIter;

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::repo::GitRepo as _;
use crate::types::{EntryMode, FileStatus, GitOid, TreeEdit};

/// Convert a `GitOid` to a `gix::ObjectId`.
fn to_gix_oid(oid: GitOid) -> gix::ObjectId {
    gix::ObjectId::from(*oid.as_bytes())
}

/// Convert a `gix::ObjectId` to our `GitOid`.
fn from_gix_oid(oid: gix::ObjectId) -> GitOid {
    let bytes: [u8; 20] = oid.as_slice().try_into().expect("SHA-1 is 20 bytes");
    GitOid::from_bytes(bytes)
}

/// Write the current index state as a tree object, returning its OID.
fn write_index_tree(repo: &GixRepo) -> Result<GitOid, GitError> {
    let index = repo.repo.open_index().map_err(|e| GitError::BackendError {
        message: format!("failed to open index: {e}"),
    })?;

    // Use a tree editor starting from an empty tree to build up the tree
    // from index entries.
    let empty_tree = gix::objs::Tree::empty();
    let empty_tree_id =
        repo.repo
            .write_object(&empty_tree)
            .map_err(|e| GitError::BackendError {
                message: format!("failed to write empty tree: {e}"),
            })?;

    let tree = repo
        .repo
        .find_tree(empty_tree_id)
        .map_err(|e| GitError::BackendError {
            message: format!("failed to find empty tree: {e}"),
        })?;

    let mut editor = tree.edit().map_err(|e| GitError::BackendError {
        message: format!("failed to create tree editor: {e}"),
    })?;

    for entry in index.entries() {
        let Ok(path) = entry.path(&index).to_str() else {
            continue;
        };

        let kind = match entry.mode {
            gix::index::entry::Mode::FILE => gix::objs::tree::EntryKind::Blob,
            gix::index::entry::Mode::FILE_EXECUTABLE => gix::objs::tree::EntryKind::BlobExecutable,
            gix::index::entry::Mode::SYMLINK => gix::objs::tree::EntryKind::Link,
            gix::index::entry::Mode::COMMIT => gix::objs::tree::EntryKind::Commit,
            _ => continue,
        };

        editor
            .upsert(path, kind, entry.id)
            .map_err(|e| GitError::BackendError {
                message: format!("tree editor upsert '{path}': {e}"),
            })?;
    }

    let tree_id = editor.write().map_err(|e| GitError::BackendError {
        message: format!("failed to write index tree: {e}"),
    })?;

    Ok(from_gix_oid(tree_id.detach()))
}

pub fn stash_create(repo: &GixRepo) -> Result<Option<GitOid>, GitError> {
    // 1. Check if worktree is dirty. If clean, nothing to stash.
    // bn-1dlkd: through the status fallback, so a tree gix cannot walk
    // (a symlinked leading path component) still answers.
    let dirty = crate::status_impl::is_dirty(repo).map_err(|e| GitError::BackendError {
        message: format!("failed to check dirty state: {e}"),
    })?;
    if !dirty {
        return Ok(None);
    }

    // 2. Read HEAD to get current commit OID.
    let head_object = repo
        .repo
        .rev_parse_single("HEAD")
        .map_err(|e| GitError::BackendError {
            message: format!("failed to resolve HEAD: {e}"),
        })?;
    let head_oid = from_gix_oid(head_object.detach());

    // 3. Write the current index state as a tree.
    let index_tree_oid = write_index_tree(repo)?;

    // 4. Create index commit: parent=HEAD, tree=index_tree
    let index_commit = {
        let tree_gix = to_gix_oid(index_tree_oid);
        let head_gix = to_gix_oid(head_oid);

        let author_sig = repo
            .repo
            .author()
            .ok_or_else(|| GitError::BackendError {
                message: "no author identity configured".to_string(),
            })?
            .map_err(|e| GitError::BackendError {
                message: format!("failed to read author identity: {e}"),
            })?;

        let committer_sig = repo
            .repo
            .committer()
            .ok_or_else(|| GitError::BackendError {
                message: "no committer identity configured".to_string(),
            })?
            .map_err(|e| GitError::BackendError {
                message: format!("failed to read committer identity: {e}"),
            })?;

        let commit = gix::objs::Commit {
            message: "index on HEAD".into(),
            tree: tree_gix,
            author: author_sig.into(),
            committer: committer_sig.into(),
            encoding: None,
            parents: vec![head_gix].into(),
            extra_headers: Vec::default(),
        };
        let id = repo
            .repo
            .write_object(&commit)
            .map_err(|e| GitError::BackendError {
                message: format!("failed to write index commit: {e}"),
            })?;
        from_gix_oid(id.detach())
    };

    // 5. Create stash commit: merge commit with parents=[HEAD, index_commit], tree=index_tree
    let stash_commit = {
        let tree_gix = to_gix_oid(index_tree_oid);
        let head_gix = to_gix_oid(head_oid);
        let idx_gix = to_gix_oid(index_commit);

        let author_sig = repo
            .repo
            .author()
            .ok_or_else(|| GitError::BackendError {
                message: "no author identity configured".to_string(),
            })?
            .map_err(|e| GitError::BackendError {
                message: format!("failed to read author identity: {e}"),
            })?;

        let committer_sig = repo
            .repo
            .committer()
            .ok_or_else(|| GitError::BackendError {
                message: "no committer identity configured".to_string(),
            })?
            .map_err(|e| GitError::BackendError {
                message: format!("failed to read committer identity: {e}"),
            })?;

        let commit = gix::objs::Commit {
            message: "WIP on HEAD".into(),
            tree: tree_gix,
            author: author_sig.into(),
            committer: committer_sig.into(),
            encoding: None,
            parents: vec![head_gix, idx_gix].into(),
            extra_headers: Vec::default(),
        };
        let id = repo
            .repo
            .write_object(&commit)
            .map_err(|e| GitError::BackendError {
                message: format!("failed to write stash commit: {e}"),
            })?;
        from_gix_oid(id.detach())
    };

    Ok(Some(stash_commit))
}

#[expect(
    clippy::too_many_lines,
    reason = "stash replay handles all tree diff cases"
)]
pub fn stash_apply(repo: &GixRepo, oid: GitOid) -> Result<(), GitError> {
    let workdir = repo
        .workdir
        .as_ref()
        .ok_or_else(|| GitError::BackendError {
            message: "repository has no working directory".to_string(),
        })?;

    // 1. Read the stash commit and get its tree.
    let stash_gix = to_gix_oid(oid);
    let stash_commit = repo
        .repo
        .find_commit(stash_gix)
        .map_err(|e| GitError::NotFound {
            message: format!("stash commit {oid}: {e}"),
        })?;
    let stash_decoded = stash_commit.decode().map_err(|e| GitError::BackendError {
        message: format!("failed to decode stash commit {oid}: {e}"),
    })?;
    let stash_tree_oid = stash_decoded.tree();

    // 2. Read the stash commit's first parent (HEAD at time of stash).
    let parent_oid = stash_decoded
        .parents()
        .next()
        .ok_or_else(|| GitError::BackendError {
            message: "stash commit has no parent".to_string(),
        })?;

    // 3. Get the parent's tree.
    let parent_commit = repo
        .repo
        .find_commit(parent_oid)
        .map_err(|e| GitError::NotFound {
            message: format!("stash parent commit {parent_oid}: {e}"),
        })?;
    let parent_decoded = parent_commit.decode().map_err(|e| GitError::BackendError {
        message: format!("failed to decode parent commit: {e}"),
    })?;
    let parent_tree_oid = parent_decoded.tree();

    // 4. Diff the parent tree vs stash tree to find changes.
    let parent_tree_data = repo
        .repo
        .find_object(parent_tree_oid)
        .map_err(|e| GitError::BackendError {
            message: format!("failed to find parent tree: {e}"),
        })?
        .data
        .clone();

    let stash_tree_data = repo
        .repo
        .find_object(stash_tree_oid)
        .map_err(|e| GitError::BackendError {
            message: format!("failed to find stash tree: {e}"),
        })?
        .data
        .clone();

    let old_iter = TreeRefIter::from_bytes(&parent_tree_data);
    let new_iter = TreeRefIter::from_bytes(&stash_tree_data);

    let mut recorder = gix::diff::tree::Recorder::default();
    gix::diff::tree(
        old_iter,
        new_iter,
        gix::diff::tree::State::default(),
        &repo.repo,
        &mut recorder,
    )
    .map_err(|e| GitError::BackendError {
        message: format!("tree diff failed: {e}"),
    })?;

    // 5. For each changed file, read the blob from stash tree and write it to worktree.
    //
    // Deletions go first (bn-ihi4h): a file <-> directory swap is a deletion
    // plus additions at or under the same path, and the new entry can only
    // be written once the old one is gone (`p` must be removed before
    // `p/x` can be created; `d/x` before file `d`).
    let mut ordered: Vec<&gix::diff::tree::recorder::Change> = recorder.records.iter().collect();
    ordered.sort_by_key(|change| {
        !matches!(change, gix::diff::tree::recorder::Change::Deletion { .. })
    });
    for change in ordered {
        match change {
            gix::diff::tree::recorder::Change::Addition {
                entry_mode,
                oid,
                path,
                ..
            }
            | gix::diff::tree::recorder::Change::Modification {
                entry_mode,
                oid,
                path,
                ..
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                let Ok(path_str) = path.to_str() else {
                    continue;
                };

                // Reject paths with .. components (path traversal protection).
                if path_str.split('/').any(|c| c == "..") {
                    return Err(GitError::BackendError {
                        message: format!("refusing path with '..' component: '{path_str}'"),
                    });
                }

                // Read blob from stash.
                let blob = repo
                    .repo
                    .find_blob(*oid)
                    .map_err(|e| GitError::BackendError {
                        message: format!("failed to read blob {oid} for '{path_str}': {e}"),
                    })?;

                let file_path = checked_stash_path(workdir, path_str)?;
                if let Some(parent) = file_path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| GitError::BackendError {
                        message: format!("failed to create directory for '{path_str}': {e}"),
                    })?;
                }

                // SAFETY: Remove any existing symlink (or file) before writing.
                // If we skip this, File::create follows existing symlinks and
                // corrupts the symlink target instead of replacing the symlink.
                // This was the root cause of the .bones/events shard corruption:
                // writing symlink target text through a symlink into the real file.
                if let Ok(meta) = std::fs::symlink_metadata(&file_path)
                    && (meta.is_symlink() || meta.is_file())
                {
                    let _ = std::fs::remove_file(&file_path);
                }
                // A directory where the entry goes (bn-ihi4h: the user turned
                // directory `d/` into file or symlink `d`). The deletions of
                // its tracked files ran above; what is left may only be empty
                // directories, which git does not track. Anything else is
                // refused, never deleted.
                if std::fs::symlink_metadata(&file_path).is_ok_and(|m| m.is_dir()) {
                    remove_empty_dir_tree(&file_path).map_err(|e| GitError::BackendError {
                        message: format!(
                            "cannot write '{path_str}': a non-empty directory is in the way: {e}"
                        ),
                    })?;
                }

                if entry_mode.kind() == gix::objs::tree::EntryKind::Link {
                    // Symlink entry: blob content is the target path.
                    let target = blob.data.as_slice().as_bstr().to_str().map_err(|_| {
                        GitError::BackendError {
                            message: format!("symlink target for '{path_str}' is not valid UTF-8"),
                        }
                    })?;
                    #[cfg(unix)]
                    {
                        std::os::unix::fs::symlink(target, &file_path).map_err(|e| {
                            GitError::BackendError {
                                message: format!(
                                    "failed to create symlink '{path_str}' -> '{target}': {e}"
                                ),
                            }
                        })?;
                    }
                    #[cfg(not(unix))]
                    {
                        // On non-Unix, fall back to writing the target as a plain file
                        // (same behavior as git on Windows without symlink support).
                        let mut file = std::fs::File::create(&file_path).map_err(|e| {
                            GitError::BackendError {
                                message: format!("failed to create file '{path_str}': {e}"),
                            }
                        })?;
                        file.write_all(blob.data.as_ref())
                            .map_err(|e| GitError::BackendError {
                                message: format!("failed to write file '{path_str}': {e}"),
                            })?;
                    }
                } else {
                    // Regular file (blob or executable blob).
                    let mut file =
                        std::fs::File::create(&file_path).map_err(|e| GitError::BackendError {
                            message: format!("failed to create file '{path_str}': {e}"),
                        })?;
                    file.write_all(blob.data.as_ref())
                        .map_err(|e| GitError::BackendError {
                            message: format!("failed to write file '{path_str}': {e}"),
                        })?;

                    #[cfg(unix)]
                    if entry_mode.kind() == gix::objs::tree::EntryKind::BlobExecutable {
                        use std::os::unix::fs::PermissionsExt;
                        let perms = std::fs::Permissions::from_mode(0o755);
                        std::fs::set_permissions(&file_path, perms).ok();
                    }
                }
            }
            gix::diff::tree::recorder::Change::Deletion {
                entry_mode, path, ..
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                let Ok(path_str) = path.to_str() else {
                    continue;
                };

                // Reject paths with .. components (path traversal protection).
                if path_str.split('/').any(|c| c == "..") {
                    return Err(GitError::BackendError {
                        message: format!("refusing path with '..' component: '{path_str}'"),
                    });
                }

                // Remove file (or symlink) from worktree.
                // Use symlink_metadata instead of exists() so dangling symlinks
                // are also detected and removed.
                let file_path = checked_stash_path(workdir, path_str)?;
                if std::fs::symlink_metadata(&file_path).is_ok() {
                    std::fs::remove_file(&file_path).map_err(|e| GitError::BackendError {
                        message: format!("failed to remove file '{path_str}': {e}"),
                    })?;
                }
            }
        }
    }

    // 6. Update the index to match the stash tree.
    let stash_index =
        repo.repo
            .index_from_tree(&stash_tree_oid)
            .map_err(|e| GitError::BackendError {
                message: format!("failed to create index from stash tree: {e}"),
            })?;

    // Write the stash index state to disk.
    let index_path = repo.repo.index_path();
    let mut index_file = gix::index::File::from_state(stash_index.into(), index_path);
    index_file
        .write(gix::index::write::Options::default())
        .map_err(|e| GitError::BackendError {
            message: format!("failed to write index: {e}"),
        })?;

    Ok(())
}

/// A leaf symlink may be replaced, but no replay operation may traverse a
/// symlinked parent, including deletions. Check from the worktree outwards
/// before creating directories, unlinking entries, or writing bytes.
fn checked_stash_path(
    workdir: &std::path::Path,
    rel: &str,
) -> Result<std::path::PathBuf, GitError> {
    let mut full = workdir.to_path_buf();
    let mut components = std::path::Path::new(rel).components().peekable();
    while let Some(component) = components.next() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(GitError::BackendError {
                message: format!("refusing unsafe replay path '{rel}'"),
            });
        }
        full.push(component);
        if components.peek().is_none() {
            break;
        }
        match full.symlink_metadata() {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(GitError::BackendError {
                    message: format!(
                        "refusing replay path '{rel}': parent '{}' is a symlink",
                        full.display()
                    ),
                });
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(GitError::BackendError {
                    message: format!("cannot inspect replay parent '{}': {e}", full.display()),
                });
            }
        }
    }
    Ok(full)
}

/// Remove `dir` if it holds nothing but (nested) empty directories; never
/// follows a symlink, and fails without removing anything else otherwise.
fn remove_empty_dir_tree(dir: &std::path::Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            remove_empty_dir_tree(&entry.path())?;
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::DirectoryNotEmpty,
                format!("{} is not empty", dir.display()),
            ));
        }
    }
    std::fs::remove_dir(dir)
}

/// Materialize the current working tree (including untracked files) into a
/// commit, without modifying the index, the worktree, or any ref.
///
/// Equivalent to `git stash create`: snapshots HEAD with all working-tree
/// modifications and untracked files applied on top, then writes a commit
/// with `parents = [HEAD]` and the supplied message. Returns the new commit
/// OID, or `None` if there is nothing to capture (clean worktree).
///
/// # How
/// 1. Read HEAD tree.
/// 2. Run [`status`](crate::status_impl::status) to enumerate working-tree
///    changes (modified, deleted, added, untracked).
/// 3. Hash each modified/added/untracked file in the worktree and apply
///    [`TreeEdit::Upsert`] edits to the HEAD tree; apply [`TreeEdit::Remove`]
///    for each deleted entry.
/// 4. [`create_commit`](crate::objects_impl::create_commit) with the resulting
///    tree, `parents=[HEAD]`, and `message`.
///
/// # Limitations
/// - Symlinks: their target text is hashed as the blob (matches gix tree
///   semantics for mode `120000`).
/// - File modes: regular vs executable vs symlink is inferred from
///   `symlink_metadata` on Unix; on other platforms all non-symlink files
///   are treated as regular blobs.
/// - Like `git stash create`, the resulting commit is detached — it is not
///   reachable from any ref unless the caller writes one.
///
/// # Errors
/// Returns a `GitError` if HEAD cannot be resolved, the worktree cannot be
/// scanned, any blob cannot be hashed, or the commit cannot be written.
#[expect(
    clippy::too_many_lines,
    reason = "single pass capturing HEAD tree + status edits + commit"
)]
pub fn worktree_state_commit(repo: &GixRepo, message: &str) -> Result<Option<GitOid>, GitError> {
    // 1. Resolve HEAD; without a HEAD there is nothing meaningful to capture.
    let Some(head_oid) = crate::refs_impl::rev_parse_opt(repo, "HEAD")? else {
        return Ok(None);
    };

    // 2. Enumerate worktree changes vs HEAD (incl. staged + untracked).
    //    Must be HEAD→worktree, not index→worktree: a staged-but-not-
    //    re-edited fix would otherwise be dropped, promoting the unfixed
    //    tree (Prime Invariant: no staged work is ever lost).
    let status = crate::status_impl::status_head_to_worktree(repo)?;
    if status.is_empty() {
        return Ok(None);
    }

    // 3. Resolve HEAD's tree.
    let head_tree_oid = {
        let gix_head = gix::ObjectId::from_bytes_or_panic(head_oid.as_bytes());
        let obj = repo
            .repo
            .find_object(gix_head)
            .map_err(|e| GitError::NotFound {
                message: format!("HEAD object {head_oid}: {e}"),
            })?;
        match obj.kind {
            gix::object::Kind::Commit => {
                let commit = obj.into_commit();
                let tree_id = commit
                    .tree_id()
                    .map_err(|e| GitError::BackendError {
                        message: format!("failed to get HEAD tree: {e}"),
                    })?
                    .detach();
                from_gix_oid(tree_id)
            }
            gix::object::Kind::Tree => from_gix_oid(gix_head),
            other => {
                return Err(GitError::BackendError {
                    message: format!("HEAD points to unexpected kind: {other}"),
                });
            }
        }
    };

    let workdir = repo
        .workdir
        .as_ref()
        .ok_or_else(|| GitError::BackendError {
            message: "repository has no working directory".to_string(),
        })?
        .clone();

    // The snapshot's tree must be internally consistent when it changes
    // `.gitattributes`: paths in that same tree must be cleaned with the new
    // rules, and deleted rules must stop applying. The general writer reads
    // committed attributes from HEAD, so resolve the final worktree view once
    // for this special case and reuse it for every captured path.
    #[cfg(feature = "lfs")]
    let worktree_attrs = status
        .iter()
        .any(|entry| entry.path.rsplit('/').next() == Some(".gitattributes"))
        .then(|| crate::lfs_clean::attrs_from_workdir(repo));

    // 4. Walk status entries; hash content for upserts.
    let mut edits: Vec<TreeEdit> = Vec::new();
    for entry in status {
        // Reject paths with .. components (path traversal protection).
        if entry.path.split('/').any(|c| c == "..") {
            return Err(GitError::BackendError {
                message: format!("refusing path with '..' component: '{}'", entry.path),
            });
        }
        match entry.status {
            FileStatus::Deleted => {
                edits.push(TreeEdit::Remove {
                    path: entry.path.clone(),
                });
            }
            FileStatus::Added
            | FileStatus::Modified
            | FileStatus::Renamed
            | FileStatus::Untracked => {
                let full = workdir.join(&entry.path);
                let Ok(meta) = std::fs::symlink_metadata(&full) else {
                    // File enumerated by status but missing on disk — treat
                    // as a deletion so the snapshot stays internally
                    // consistent.
                    edits.push(TreeEdit::Remove {
                        path: entry.path.clone(),
                    });
                    continue;
                };

                #[cfg(unix)]
                let is_executable = {
                    use std::os::unix::fs::PermissionsExt;
                    !meta.is_symlink() && (meta.permissions().mode() & 0o111) != 0
                };
                #[cfg(not(unix))]
                let is_executable = false;

                let (data, mode) = if meta.is_symlink() {
                    let target = std::fs::read_link(&full).map_err(|e| GitError::BackendError {
                        message: format!("read symlink '{}': {e}", entry.path),
                    })?;
                    // Hash the raw bytes of the symlink target. On Unix the
                    // target's OsString may legitimately contain non-UTF-8
                    // bytes; `to_string_lossy()` would replace them with
                    // U+FFFD and corrupt the blob (mismatching `git stash
                    // create`).
                    #[cfg(unix)]
                    let bytes = {
                        use std::os::unix::ffi::OsStrExt;
                        target.as_os_str().as_bytes().to_vec()
                    };
                    #[cfg(not(unix))]
                    let bytes = target.to_string_lossy().into_owned().into_bytes();
                    (bytes, EntryMode::Link)
                } else if meta.is_file() {
                    let bytes = std::fs::read(&full).map_err(|e| GitError::BackendError {
                        message: format!("read file '{}': {e}", entry.path),
                    })?;
                    let mode = if is_executable {
                        EntryMode::BlobExecutable
                    } else {
                        EntryMode::Blob
                    };
                    (bytes, mode)
                } else {
                    // Directory or other non-blob type — skip; gix's status
                    // pipeline only surfaces file-like entries, but guard
                    // anyway.
                    continue;
                };

                // Use the clean-filter-aware writer so the snapshot matches
                // what `git stash create` (and maw's own merge/rebase
                // writers, see merge.rs / build.rs) would store: LFS-tracked
                // paths get pointer blobs, not full content. Falls back to a
                // plain `write_blob` when the "lfs" feature is disabled (via
                // the trait's default method) or when the path has no
                // matching filter rule.
                #[cfg(feature = "lfs")]
                let blob_oid = match worktree_attrs.as_ref() {
                    Some(attrs) => {
                        crate::lfs_clean::write_blob_with_attrs(repo, &data, &entry.path, attrs)?
                    }
                    None => repo.write_blob_with_path(&data, &entry.path)?,
                };
                #[cfg(not(feature = "lfs"))]
                let blob_oid = repo.write_blob_with_path(&data, &entry.path)?;
                edits.push(TreeEdit::Upsert {
                    path: entry.path.clone(),
                    mode,
                    oid: blob_oid,
                });
            }
        }
    }

    if edits.is_empty() {
        return Ok(None);
    }

    // 5. Apply edits to the HEAD tree and commit.
    let new_tree = crate::objects_impl::edit_tree(repo, head_tree_oid, &edits)?;
    let commit_oid =
        crate::objects_impl::create_commit(repo, new_tree, &[head_oid], message, None)?;
    Ok(Some(commit_oid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Helper: init a git repo with an initial commit, return (tempdir, `GixRepo`).
    fn setup_repo() -> (tempfile::TempDir, GixRepo) {
        // bn-5rdz: shared init + seed-commit helper. The helper writes
        // README.md as its seed file; the tests below don't depend on the
        // specific seed-file name (they create their own .bones/events,
        // links, etc.) so swapping `init.txt` for `README.md` is safe.
        let (dir, root, _oid) = crate::test_support::init_test_repo_with_commit();
        let repo = GixRepo::open(&root).expect("test setup should succeed");
        (dir, repo)
    }

    /// Regression test: `stash_apply` must create OS symlinks for mode 120000 entries,
    /// not write the target path as regular file content.
    ///
    /// This was the root cause of the .bones/events shard corruption where a 1.6MB
    /// event log was overwritten with a 14-byte symlink target string.
    #[cfg(unix)]
    #[test]
    fn stash_apply_creates_symlinks_not_regular_files() {
        let (dir, repo) = setup_repo();
        let root = dir.path();

        // Add a real file and a symlink as dirty (unstaged) changes.
        std::fs::write(root.join("real-data.txt"), "important data\n")
            .expect("test setup should succeed");
        std::os::unix::fs::symlink("real-data.txt", root.join("current.txt"))
            .expect("test setup should succeed");

        // Stage so the stash captures them (stash reads from index).
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(root)
            .output()
            .expect("test setup should succeed");

        // Create a stash from the dirty state.
        let stash_oid = stash_create(&repo)
            .expect("test setup should succeed")
            .expect("stash should not be empty");

        // Clean the worktree (remove the files we just added).
        std::fs::remove_file(root.join("current.txt")).expect("test setup should succeed");
        std::fs::remove_file(root.join("real-data.txt")).expect("test setup should succeed");

        // Reset index to HEAD.
        Command::new("git")
            .args(["reset", "HEAD", "--", "."])
            .current_dir(root)
            .output()
            .expect("test setup should succeed");

        assert!(!root.join("current.txt").exists());
        assert!(!root.join("real-data.txt").exists());

        // Apply the stash — this should recreate the symlink.
        stash_apply(&repo, stash_oid).expect("test setup should succeed");

        // Verify: current.txt should be a symlink, not a regular file.
        let meta =
            std::fs::symlink_metadata(root.join("current.txt")).expect("test setup should succeed");
        assert!(
            meta.is_symlink(),
            "current.txt should be a symlink, but is type {:?}",
            meta.file_type()
        );

        // Verify: symlink target is correct.
        let target =
            std::fs::read_link(root.join("current.txt")).expect("test setup should succeed");
        assert_eq!(
            target.to_str().expect("test setup should succeed"),
            "real-data.txt"
        );

        // Verify: real-data.txt is a regular file with correct content.
        let content =
            std::fs::read_to_string(root.join("real-data.txt")).expect("test setup should succeed");
        assert_eq!(content, "important data\n");
    }

    /// Regression test: writing a regular file where a symlink exists on disk
    /// must NOT follow the symlink. The symlink must be removed first.
    ///
    /// Without the fix, <File::create> follows the symlink and overwrites the
    /// target file with the new content.
    #[cfg(unix)]
    #[test]
    fn stash_apply_does_not_follow_existing_symlinks() {
        let (dir, repo) = setup_repo();
        let root = dir.path();

        // Set up: a regular file "data.txt" with important content.
        std::fs::write(root.join("data.txt"), "precious data that must survive\n")
            .expect("test setup should succeed");
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(root)
            .output()
            .expect("test setup should succeed");
        Command::new("git")
            .args(["commit", "-m", "add data"])
            .current_dir(root)
            .output()
            .expect("test setup should succeed");

        // Now create a stash where "link.txt" is a regular file.
        std::fs::write(root.join("link.txt"), "regular content\n")
            .expect("test setup should succeed");
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(root)
            .output()
            .expect("test setup should succeed");
        let stash_oid = stash_create(&repo)
            .expect("test setup should succeed")
            .expect("stash should not be empty");

        // But on disk, replace link.txt with a symlink pointing to data.txt.
        std::fs::remove_file(root.join("link.txt")).expect("test setup should succeed");
        std::os::unix::fs::symlink("data.txt", root.join("link.txt"))
            .expect("test setup should succeed");

        // Apply stash: should replace the symlink with a regular file,
        // NOT write "regular content" through the symlink into data.txt.
        stash_apply(&repo, stash_oid).expect("test setup should succeed");

        // Verify: data.txt must NOT be corrupted.
        let data =
            std::fs::read_to_string(root.join("data.txt")).expect("test setup should succeed");
        assert_eq!(
            data, "precious data that must survive\n",
            "data.txt was corrupted by symlink following"
        );

        // Verify: link.txt should now be a regular file.
        let meta =
            std::fs::symlink_metadata(root.join("link.txt")).expect("test setup should succeed");
        assert!(
            meta.is_file() && !meta.is_symlink(),
            "link.txt should be a regular file, not a symlink"
        );
        let content =
            std::fs::read_to_string(root.join("link.txt")).expect("test setup should succeed");
        assert_eq!(content, "regular content\n");
    }

    fn commit_all(root: &std::path::Path, msg: &str) {
        for args in [&["add", "-A"][..], &["commit", "-q", "-m", msg][..]] {
            let out = Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .expect("run git");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        }
    }

    /// bn-ihi4h: a snapshot that turned tracked directory `d/` into file `d`
    /// replays onto the unchanged tree: the deletions under `d/` run first,
    /// the empty directories left behind make way, and file `d` is written.
    #[test]
    fn stash_apply_replays_dir_to_file_swap() {
        let (dir, repo) = setup_repo();
        let root = dir.path();
        std::fs::create_dir_all(root.join("d/sub")).expect("mkdir");
        std::fs::write(root.join("d/x"), "x\n").expect("write");
        std::fs::write(root.join("d/sub/y"), "y\n").expect("write");
        commit_all(root, "dir");

        std::fs::remove_dir_all(root.join("d")).expect("rm d");
        std::fs::write(root.join("d"), "user file\n").expect("write d");
        let snap = worktree_state_commit(&repo, "snap")
            .expect("snapshot")
            .expect("dirty");

        // Back to the committed tree, then replay.
        std::fs::remove_file(root.join("d")).expect("rm d");
        std::fs::create_dir_all(root.join("d/sub")).expect("mkdir");
        std::fs::write(root.join("d/x"), "x\n").expect("write");
        std::fs::write(root.join("d/sub/y"), "y\n").expect("write");
        stash_apply(&repo, snap).expect("replay the swap");

        let meta = std::fs::symlink_metadata(root.join("d")).expect("stat d");
        assert!(meta.is_file(), "d must be the user's file");
        assert_eq!(
            std::fs::read_to_string(root.join("d")).expect("read d"),
            "user file\n"
        );
    }

    /// bn-ihi4h: a snapshot that turned tracked file `p` into directory `p/`.
    #[test]
    fn stash_apply_replays_file_to_dir_swap() {
        let (dir, repo) = setup_repo();
        let root = dir.path();
        std::fs::write(root.join("p"), "one\n").expect("write");
        commit_all(root, "file");

        std::fs::remove_file(root.join("p")).expect("rm p");
        std::fs::create_dir(root.join("p")).expect("mkdir p");
        std::fs::write(root.join("p/x"), "user x\n").expect("write p/x");
        let snap = worktree_state_commit(&repo, "snap")
            .expect("snapshot")
            .expect("dirty");

        std::fs::remove_dir_all(root.join("p")).expect("rm p");
        std::fs::write(root.join("p"), "one\n").expect("write");
        stash_apply(&repo, snap).expect("replay the swap");

        assert_eq!(
            std::fs::read_to_string(root.join("p/x")).expect("read p/x"),
            "user x\n"
        );
    }

    /// bn-ihi4h: a directory in the way that still holds a file is never
    /// deleted to make room; the replay fails and the file survives.
    #[test]
    fn stash_apply_refuses_to_remove_non_empty_directory() {
        let (dir, repo) = setup_repo();
        let root = dir.path();
        std::fs::create_dir(root.join("d")).expect("mkdir");
        std::fs::write(root.join("d/x"), "x\n").expect("write");
        commit_all(root, "dir");

        std::fs::remove_dir_all(root.join("d")).expect("rm d");
        std::fs::write(root.join("d"), "user file\n").expect("write d");
        let snap = worktree_state_commit(&repo, "snap")
            .expect("snapshot")
            .expect("dirty");

        std::fs::remove_file(root.join("d")).expect("rm d");
        std::fs::create_dir(root.join("d")).expect("mkdir");
        std::fs::write(root.join("d/x"), "x\n").expect("write");
        std::fs::write(root.join("d/keep"), "not tracked, not captured\n").expect("write");
        assert!(stash_apply(&repo, snap).is_err(), "must refuse");
        assert_eq!(
            std::fs::read_to_string(root.join("d/keep")).expect("d/keep survives"),
            "not tracked, not captured\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stash_apply_refuses_writes_through_parent_symlink() {
        let (dir, repo) = setup_repo();
        let root = dir.path();
        std::fs::create_dir(root.join("d")).expect("mkdir");
        std::fs::write(root.join("d/x"), "base\n").expect("write base");
        commit_all(root, "directory");
        std::fs::write(root.join("d/x"), "local edit\n").expect("edit");
        let snap = worktree_state_commit(&repo, "snap")
            .expect("snapshot")
            .expect("dirty");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("x"), "outside work\n").expect("outside x");
        std::fs::remove_dir_all(root.join("d")).expect("remove d");
        std::os::unix::fs::symlink(outside.path(), root.join("d")).expect("symlink");

        let result = stash_apply(&repo, snap);
        assert_eq!(
            std::fs::read_to_string(outside.path().join("x")).expect("outside survives"),
            "outside work\n"
        );
        assert!(result.is_err(), "replay must refuse the symlinked parent");
    }

    /// Regression test (bn-17o1): `worktree_state_commit` must go through the
    /// clean-filter-aware writer, so an LFS-tracked file in the snapshot ends
    /// up as a pointer blob — matching what `git stash create` / `git add`
    /// would store — instead of the raw file content. Before the fix, this
    /// used a raw `write_blob` and the materialized commit diverged from real
    /// git for any repo using LFS (or other clean filters).
    #[cfg(feature = "lfs")]
    #[test]
    fn worktree_state_commit_writes_lfs_pointer_for_tracked_path() {
        use std::io::Read as _;

        let (dir, repo) = setup_repo();
        let root = dir.path();

        // Track *.bin as LFS and commit the .gitattributes so it's visible
        // from HEAD (worktree_state_commit loads attrs from HEAD's tree).
        std::fs::write(
            root.join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .expect("test setup should succeed");
        let _ = crate::test_support::commit_all(root, "add gitattributes");

        // Dirty the worktree with an untracked LFS-tracked file, holding raw
        // (non-pointer) content — the same shape a build artifact or
        // freshly-downloaded asset would have before `git add` runs it
        // through the LFS clean filter.
        let real_content = b"this is real binary content, not a pointer\n";
        std::fs::write(root.join("data.bin"), real_content).expect("test setup should succeed");

        let commit_oid = worktree_state_commit(&repo, "snapshot")
            .expect("worktree_state_commit should succeed")
            .expect("snapshot should not be empty");

        // Read data.bin back out of the resulting commit's tree.
        let (_mode, _oid, stored) = repo
            .read_blob_at_path(commit_oid, "data.bin")
            .expect("read_blob_at_path should succeed")
            .expect("data.bin should be present in the snapshot commit");

        // It must be a pointer, not the real content.
        assert!(
            maw_lfs::looks_like_pointer(&stored),
            "expected data.bin to be stored as an LFS pointer, got: {:?}",
            String::from_utf8_lossy(&stored)
        );
        assert_ne!(
            stored, real_content,
            "data.bin should not store the raw content directly"
        );
        let pointer =
            maw_lfs::Pointer::parse(&stored).expect("stored blob should be a valid LFS pointer");
        assert_eq!(pointer.size, real_content.len() as u64);

        // And the real content must have been pushed into the LFS object
        // store, addressable by the pointer's sha256 oid.
        let git_dir = repo.repo.git_dir();
        let store = maw_lfs::Store::open(git_dir).expect("lfs store should open");
        let mut reader = store
            .open_object(&pointer.oid)
            .expect("lfs store read should succeed")
            .expect("lfs object should be present in the store");
        let mut stored_bytes = Vec::new();
        reader
            .read_to_end(&mut stored_bytes)
            .expect("reading lfs object should succeed");
        assert_eq!(stored_bytes, real_content);
    }

    /// A snapshot must apply the `.gitattributes` content that it captures,
    /// not the older rules from `HEAD`. Otherwise a newly tracked LFS path is
    /// stored as a raw git blob while the same commit says it is LFS-managed.
    #[cfg(feature = "lfs")]
    #[test]
    fn worktree_state_commit_uses_modified_worktree_gitattributes() {
        let (dir, repo) = setup_repo();
        let root = dir.path();

        std::fs::write(root.join(".gitattributes"), "*.old filter=lfs -text\n")
            .expect("test setup should succeed");
        let _ = crate::test_support::commit_all(root, "add initial gitattributes");

        std::fs::write(root.join(".gitattributes"), "*.bin filter=lfs -text\n")
            .expect("test setup should succeed");
        let real_content = b"newly tracked binary content\n";
        std::fs::write(root.join("data.bin"), real_content).expect("test setup should succeed");

        let commit_oid = worktree_state_commit(&repo, "snapshot")
            .expect("worktree_state_commit should succeed")
            .expect("snapshot should not be empty");
        let (_mode, _oid, stored) = repo
            .read_blob_at_path(commit_oid, "data.bin")
            .expect("read_blob_at_path should succeed")
            .expect("data.bin should be present in the snapshot commit");

        assert!(
            maw_lfs::looks_like_pointer(&stored),
            "modified worktree attributes must clean data.bin; got: {:?}",
            String::from_utf8_lossy(&stored)
        );
    }

    /// Removing an LFS rule in the captured worktree must take effect in the
    /// snapshot. Reading the deleted rule from `HEAD` would incorrectly wrap
    /// the new file in an LFS pointer.
    #[cfg(feature = "lfs")]
    #[test]
    fn worktree_state_commit_honors_deleted_worktree_gitattributes() {
        let (dir, repo) = setup_repo();
        let root = dir.path();

        std::fs::write(root.join(".gitattributes"), "*.bin filter=lfs -text\n")
            .expect("test setup should succeed");
        let _ = crate::test_support::commit_all(root, "add initial gitattributes");

        std::fs::remove_file(root.join(".gitattributes")).expect("test setup should succeed");
        let real_content = b"ordinary binary content after attributes removal\n";
        std::fs::write(root.join("data.bin"), real_content).expect("test setup should succeed");

        let commit_oid = worktree_state_commit(&repo, "snapshot")
            .expect("worktree_state_commit should succeed")
            .expect("snapshot should not be empty");
        let (_mode, _oid, stored) = repo
            .read_blob_at_path(commit_oid, "data.bin")
            .expect("read_blob_at_path should succeed")
            .expect("data.bin should be present in the snapshot commit");

        assert_eq!(
            stored, real_content,
            "deleted worktree attributes must stop cleaning data.bin"
        );
    }
}
