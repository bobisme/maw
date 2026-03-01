//! Stash create/apply built from gix commit/tree primitives.
//!
//! gix does not provide a high-level stash API.
//! We build stash from tree, index, and commit operations.

use std::io::Write;

use gix::bstr::ByteSlice;
use gix::objs::TreeRefIter;

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::*;

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
    let empty_tree_id = repo
        .repo
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
        let path = match entry.path(&index).to_str() {
            Ok(p) => p,
            Err(_) => continue,
        };

        let kind = match entry.mode {
            gix::index::entry::Mode::FILE => gix::objs::tree::EntryKind::Blob,
            gix::index::entry::Mode::FILE_EXECUTABLE => {
                gix::objs::tree::EntryKind::BlobExecutable
            }
            gix::index::entry::Mode::SYMLINK => gix::objs::tree::EntryKind::Link,
            gix::index::entry::Mode::COMMIT => gix::objs::tree::EntryKind::Commit,
            _ => continue,
        };

        editor.upsert(path, kind, entry.id).map_err(|e| GitError::BackendError {
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
    let dirty = repo
        .repo
        .is_dirty()
        .map_err(|e| GitError::BackendError {
            message: format!("failed to check dirty state: {e}"),
        })?;
    if !dirty {
        return Ok(None);
    }

    // 2. Read HEAD to get current commit OID.
    let head_id = repo
        .repo
        .rev_parse_single("HEAD")
        .map_err(|e| GitError::BackendError {
            message: format!("failed to resolve HEAD: {e}"),
        })?;
    let head_oid = from_gix_oid(head_id.detach());

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
            author: author_sig.clone().into(),
            committer: committer_sig.clone().into(),
            encoding: None,
            parents: vec![head_gix].into(),
            extra_headers: Default::default(),
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
            extra_headers: Default::default(),
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

pub fn stash_apply(repo: &GixRepo, oid: GitOid) -> Result<(), GitError> {
    let workdir = repo.workdir.as_ref().ok_or_else(|| GitError::BackendError {
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
        .to_vec();

    let stash_tree_data = repo
        .repo
        .find_object(stash_tree_oid)
        .map_err(|e| GitError::BackendError {
            message: format!("failed to find stash tree: {e}"),
        })?
        .data
        .to_vec();

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
    for change in &recorder.records {
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
                let path_str = match path.to_str() {
                    Ok(s) => s,
                    Err(_) => continue,
                };

                // Read blob from stash.
                let blob = repo
                    .repo
                    .find_blob(*oid)
                    .map_err(|e| GitError::BackendError {
                        message: format!("failed to read blob {oid} for '{path_str}': {e}"),
                    })?;

                // Write file to worktree.
                let file_path = workdir.join(path_str);
                if let Some(parent) = file_path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| GitError::BackendError {
                        message: format!("failed to create directory for '{path_str}': {e}"),
                    })?;
                }
                let mut file =
                    std::fs::File::create(&file_path).map_err(|e| GitError::BackendError {
                        message: format!("failed to create file '{path_str}': {e}"),
                    })?;
                file.write_all(blob.data.as_ref())
                    .map_err(|e| GitError::BackendError {
                        message: format!("failed to write file '{path_str}': {e}"),
                    })?;

                // Set executable bit on Unix if needed.
                #[cfg(unix)]
                if entry_mode.kind() == gix::objs::tree::EntryKind::BlobExecutable {
                    use std::os::unix::fs::PermissionsExt;
                    let perms = std::fs::Permissions::from_mode(0o755);
                    std::fs::set_permissions(&file_path, perms).ok();
                }
            }
            gix::diff::tree::recorder::Change::Deletion {
                entry_mode, path, ..
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                let path_str = match path.to_str() {
                    Ok(s) => s,
                    Err(_) => continue,
                };

                // Remove file from worktree.
                let file_path = workdir.join(path_str);
                if file_path.exists() {
                    std::fs::remove_file(&file_path).map_err(|e| GitError::BackendError {
                        message: format!("failed to remove file '{path_str}': {e}"),
                    })?;
                }
            }
        }
    }

    // 6. Update the index to match the stash tree.
    let stash_index = repo
        .repo
        .index_from_tree(&stash_tree_oid)
        .map_err(|e| GitError::BackendError {
            message: format!("failed to create index from stash tree: {e}"),
        })?;

    // Write the stash index state to disk.
    let index_path = repo.repo.index_path();
    let mut index_file = gix::index::File::from_state(stash_index.into(), index_path);
    index_file
        .write(Default::default())
        .map_err(|e| GitError::BackendError {
            message: format!("failed to write index: {e}"),
        })?;

    Ok(())
}
