//! gix-backed tree-to-tree diff.

use gix::objs::TreeRefIter;

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::*;

/// Convert a `GitOid` to a gix `ObjectId`.
fn to_gix_oid(oid: GitOid) -> gix::ObjectId {
    gix::ObjectId::from(*oid.as_bytes())
}

/// Convert a gix `ObjectId` to our `GitOid`.
fn from_gix_oid(oid: gix::ObjectId) -> GitOid {
    let bytes: [u8; 20] = oid.as_slice().try_into().expect("SHA-1 is 20 bytes");
    GitOid::from_bytes(bytes)
}

/// Convert a gix `EntryMode` to our `EntryMode`.
fn convert_entry_mode(mode: gix::objs::tree::EntryMode) -> EntryMode {
    match mode.kind() {
        gix::objs::tree::EntryKind::Blob => EntryMode::Blob,
        gix::objs::tree::EntryKind::BlobExecutable => EntryMode::BlobExecutable,
        gix::objs::tree::EntryKind::Tree => EntryMode::Tree,
        gix::objs::tree::EntryKind::Link => EntryMode::Link,
        gix::objs::tree::EntryKind::Commit => EntryMode::Commit,
    }
}

pub fn diff_trees(
    repo: &GixRepo,
    old: Option<GitOid>,
    new: GitOid,
) -> Result<Vec<DiffEntry>, GitError> {
    let gix_repo = &repo.repo;

    // Load old tree data (empty bytes for None → empty tree).
    let old_tree_data = match old {
        Some(oid) => {
            let obj = gix_repo
                .find_object(to_gix_oid(oid))
                .map_err(|e| GitError::BackendError {
                    message: format!("failed to find old tree {oid}: {e}"),
                })?;
            obj.data.to_vec()
        }
        None => Vec::new(),
    };

    // Load new tree data.
    let new_tree_data = gix_repo
        .find_object(to_gix_oid(new))
        .map_err(|e| GitError::BackendError {
            message: format!("failed to find new tree {new}: {e}"),
        })?
        .data
        .to_vec();

    let old_iter = TreeRefIter::from_bytes(&old_tree_data);
    let new_iter = TreeRefIter::from_bytes(&new_tree_data);

    let mut recorder = gix::diff::tree::Recorder::default();
    gix::diff::tree(
        old_iter,
        new_iter,
        gix::diff::tree::State::default(),
        gix_repo,
        &mut recorder,
    )
    .map_err(|e| GitError::BackendError {
        message: format!("tree diff failed: {e}"),
    })?;

    let entries = recorder
        .records
        .into_iter()
        .filter_map(|change| {
            match change {
                gix::diff::tree::recorder::Change::Addition {
                    entry_mode,
                    oid,
                    path,
                    ..
                } => {
                    // Skip tree entries — we only want file-level changes.
                    if entry_mode.is_tree() {
                        return None;
                    }
                    Some(DiffEntry {
                        path: path.to_string(),
                        change_type: ChangeType::Added,
                        old_oid: GitOid::ZERO,
                        new_oid: from_gix_oid(oid),
                        old_mode: None,
                        new_mode: Some(convert_entry_mode(entry_mode)),
                    })
                }
                gix::diff::tree::recorder::Change::Deletion {
                    entry_mode,
                    oid,
                    path,
                    ..
                } => {
                    if entry_mode.is_tree() {
                        return None;
                    }
                    Some(DiffEntry {
                        path: path.to_string(),
                        change_type: ChangeType::Deleted,
                        old_oid: from_gix_oid(oid),
                        new_oid: GitOid::ZERO,
                        old_mode: Some(convert_entry_mode(entry_mode)),
                        new_mode: None,
                    })
                }
                gix::diff::tree::recorder::Change::Modification {
                    previous_entry_mode,
                    previous_oid,
                    entry_mode,
                    oid,
                    path,
                } => {
                    if entry_mode.is_tree() {
                        return None;
                    }
                    Some(DiffEntry {
                        path: path.to_string(),
                        change_type: ChangeType::Modified,
                        old_oid: from_gix_oid(previous_oid),
                        new_oid: from_gix_oid(oid),
                        old_mode: Some(convert_entry_mode(previous_entry_mode)),
                        new_mode: Some(convert_entry_mode(entry_mode)),
                    })
                }
            }
        })
        .collect();

    Ok(entries)
}
