//! gix-backed object read/write and tree editing operations.

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::*;

/// Convert our `GitOid` to a `gix::ObjectId`.
fn to_gix_oid(oid: GitOid) -> gix::ObjectId {
    gix::ObjectId::from_bytes_or_panic(oid.as_bytes())
}

/// Convert a `gix::ObjectId` to our `GitOid`.
fn from_gix_oid(oid: gix::ObjectId) -> GitOid {
    let bytes: [u8; 20] = oid
        .as_bytes()
        .try_into()
        .expect("SHA1 is 20 bytes");
    GitOid::from_bytes(bytes)
}

/// Convert a gix `EntryMode` to our `EntryMode`.
fn from_gix_entry_mode(mode: gix::objs::tree::EntryMode) -> EntryMode {
    match mode.kind() {
        gix::objs::tree::EntryKind::Tree => EntryMode::Tree,
        gix::objs::tree::EntryKind::Blob => EntryMode::Blob,
        gix::objs::tree::EntryKind::BlobExecutable => EntryMode::BlobExecutable,
        gix::objs::tree::EntryKind::Link => EntryMode::Link,
        gix::objs::tree::EntryKind::Commit => EntryMode::Commit,
    }
}

/// Convert our `EntryMode` to a gix `EntryKind`.
fn to_gix_entry_kind(mode: EntryMode) -> gix::objs::tree::EntryKind {
    match mode {
        EntryMode::Blob => gix::objs::tree::EntryKind::Blob,
        EntryMode::BlobExecutable => gix::objs::tree::EntryKind::BlobExecutable,
        EntryMode::Tree => gix::objs::tree::EntryKind::Tree,
        EntryMode::Link => gix::objs::tree::EntryKind::Link,
        EntryMode::Commit => gix::objs::tree::EntryKind::Commit,
    }
}

pub fn read_blob(repo: &GixRepo, oid: GitOid) -> Result<Vec<u8>, GitError> {
    let gix_oid = to_gix_oid(oid);
    let mut blob = repo
        .repo
        .find_blob(gix_oid)
        .map_err(|e| GitError::NotFound {
            message: format!("blob {oid}: {e}"),
        })?;
    Ok(blob.take_data())
}

pub fn read_tree(repo: &GixRepo, oid: GitOid) -> Result<Vec<TreeEntry>, GitError> {
    let gix_oid = to_gix_oid(oid);
    let tree = repo
        .repo
        .find_tree(gix_oid)
        .map_err(|e| GitError::NotFound {
            message: format!("tree {oid}: {e}"),
        })?;

    let mut entries = Vec::new();
    for result in tree.iter() {
        let entry = result.map_err(|e| GitError::BackendError {
            message: format!("failed to decode tree entry: {e}"),
        })?;
        let oid_bytes: [u8; 20] = entry
            .inner
            .oid
            .as_bytes()
            .try_into()
            .expect("SHA1 is 20 bytes");
        entries.push(TreeEntry {
            name: entry.inner.filename.to_string(),
            mode: from_gix_entry_mode(entry.inner.mode),
            oid: GitOid::from_bytes(oid_bytes),
        });
    }
    Ok(entries)
}

pub fn read_commit(repo: &GixRepo, oid: GitOid) -> Result<CommitInfo, GitError> {
    let gix_oid = to_gix_oid(oid);
    let commit = repo
        .repo
        .find_commit(gix_oid)
        .map_err(|e| GitError::NotFound {
            message: format!("commit {oid}: {e}"),
        })?;

    let decoded = commit.decode().map_err(|e| GitError::BackendError {
        message: format!("failed to decode commit {oid}: {e}"),
    })?;

    let tree_oid = from_gix_oid(decoded.tree());
    let parents = decoded.parents().map(from_gix_oid).collect();
    let message = decoded.message.to_string();

    let author_sig = decoded.author();
    let committer_sig = decoded.committer();

    let author = format!("{} <{}>", author_sig.name, author_sig.email);
    let committer = format!("{} <{}>", committer_sig.name, committer_sig.email);

    Ok(CommitInfo {
        tree_oid,
        parents,
        message,
        author,
        committer,
    })
}

pub fn write_blob(repo: &GixRepo, data: &[u8]) -> Result<GitOid, GitError> {
    let id = repo
        .repo
        .write_blob(data)
        .map_err(|e| GitError::BackendError {
            message: format!("failed to write blob: {e}"),
        })?;
    Ok(from_gix_oid(id.detach()))
}

pub fn write_tree(repo: &GixRepo, entries: &[TreeEntry]) -> Result<GitOid, GitError> {
    let tree = gix::objs::Tree {
        entries: entries
            .iter()
            .map(|e| gix::objs::tree::Entry {
                mode: to_gix_entry_kind(e.mode).into(),
                filename: e.name.as_str().into(),
                oid: to_gix_oid(e.oid),
            })
            .collect(),
    };
    let id = repo
        .repo
        .write_object(&tree)
        .map_err(|e| GitError::BackendError {
            message: format!("failed to write tree: {e}"),
        })?;
    Ok(from_gix_oid(id.detach()))
}

pub fn create_commit(
    repo: &GixRepo,
    tree: GitOid,
    parents: &[GitOid],
    message: &str,
    update_ref: Option<&RefName>,
) -> Result<GitOid, GitError> {
    let tree_oid = to_gix_oid(tree);
    let parent_oids: Vec<gix::ObjectId> = parents.iter().map(|p| to_gix_oid(*p)).collect();

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

    match update_ref {
        Some(ref_name) => {
            let id = repo
                .repo
                .commit_as(
                    committer_sig,
                    author_sig,
                    ref_name.as_str(),
                    message,
                    tree_oid,
                    parent_oids,
                )
                .map_err(|e| GitError::BackendError {
                    message: format!("failed to create commit: {e}"),
                })?;
            Ok(from_gix_oid(id.detach()))
        }
        None => {
            let commit = gix::objs::Commit {
                message: message.into(),
                tree: tree_oid,
                author: author_sig.into(),
                committer: committer_sig.into(),
                encoding: None,
                parents: parent_oids.into_iter().collect(),
                extra_headers: Default::default(),
            };
            let id = repo
                .repo
                .write_object(&commit)
                .map_err(|e| GitError::BackendError {
                    message: format!("failed to write commit object: {e}"),
                })?;
            Ok(from_gix_oid(id.detach()))
        }
    }
}

pub fn edit_tree(repo: &GixRepo, base: GitOid, edits: &[TreeEdit]) -> Result<GitOid, GitError> {
    let gix_oid = to_gix_oid(base);
    let tree = repo
        .repo
        .find_tree(gix_oid)
        .map_err(|e| GitError::NotFound {
            message: format!("base tree {base}: {e}"),
        })?;

    let mut editor = tree.edit().map_err(|e| GitError::BackendError {
        message: format!("failed to create tree editor: {e}"),
    })?;

    for edit in edits {
        match edit {
            TreeEdit::Upsert { path, mode, oid } => {
                let kind = to_gix_entry_kind(*mode);
                let gix_oid = to_gix_oid(*oid);
                editor
                    .upsert(path.as_str(), kind, gix_oid)
                    .map_err(|e| GitError::BackendError {
                        message: format!("tree edit upsert '{path}': {e}"),
                    })?;
            }
            TreeEdit::Remove { path } => {
                editor
                    .remove(path.as_str())
                    .map_err(|e| GitError::BackendError {
                        message: format!("tree edit remove '{path}': {e}"),
                    })?;
            }
        }
    }

    let new_id = editor.write().map_err(|e| GitError::BackendError {
        message: format!("failed to write edited tree: {e}"),
    })?;
    Ok(from_gix_oid(new_id.detach()))
}
