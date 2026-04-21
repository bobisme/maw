//! gix-backed tree-to-tree diff.

use gix::objs::TreeRefIter;

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::*;

/// Clamp a user-supplied similarity percentage (0-100) to a `[0.0, 1.0]`
/// `f32` suitable for `gix_diff::Rewrites::percentage`.
fn similarity_fraction(pct: u32) -> f32 {
    (pct.min(100) as f32) / 100.0
}

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
            let obj =
                gix_repo
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

/// Tree-to-tree diff with rename detection.
///
/// Unlike [`diff_trees`], this function runs gix's rewrite tracker so that
/// matching delete+add pairs above `similarity_pct` similarity collapse into
/// a single [`ChangeType::Renamed`] entry at the destination path, with the
/// original path carried in `from`.
///
/// `similarity_pct` is clamped to `0..=100`; `100` requires an exact content
/// match (pure rename / mode change only), values below 100 enable similarity-
/// based matching via gix's edit-distance algorithm. A common default is 50,
/// which matches git's built-in rename-threshold.
pub fn diff_trees_with_renames(
    repo: &GixRepo,
    old: Option<GitOid>,
    new: GitOid,
    similarity_pct: u32,
) -> Result<Vec<DiffEntry>, GitError> {
    let gix_repo = &repo.repo;

    // Resolve trees. `None` ⇒ empty tree.
    let empty_tree = gix_repo.empty_tree();
    let old_tree_ref;
    let old_tree = match old {
        Some(oid) => {
            old_tree_ref = gix_repo
                .find_tree(to_gix_oid(oid))
                .map_err(|e| GitError::NotFound {
                    message: format!("old tree {oid}: {e}"),
                })?;
            &old_tree_ref
        }
        None => &empty_tree,
    };
    let new_tree_ref = gix_repo
        .find_tree(to_gix_oid(new))
        .map_err(|e| GitError::NotFound {
            message: format!("new tree {new}: {e}"),
        })?;

    // Configure rename-aware options. `gix::diff::Rewrites::percentage` is
    // an `Option<f32>` in [0.0, 1.0]; 1.0 means exact match only.
    let rewrites = gix::diff::Rewrites {
        copies: None,
        percentage: Some(similarity_fraction(similarity_pct)),
        limit: 1000,
        track_empty: false,
    };
    let opts = gix::diff::Options::default().with_rewrites(Some(rewrites));

    let changes = gix_repo
        .diff_tree_to_tree(old_tree, &new_tree_ref, opts)
        .map_err(|e| GitError::BackendError {
            message: format!("tree_to_tree diff failed: {e}"),
        })?;

    let mut entries: Vec<DiffEntry> = Vec::new();
    for change in changes {
        match change {
            gix::diff::tree_with_rewrites::Change::Addition {
                location,
                entry_mode,
                id,
                ..
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                entries.push(DiffEntry {
                    path: location.to_string(),
                    change_type: ChangeType::Added,
                    old_oid: GitOid::ZERO,
                    new_oid: from_gix_oid(id),
                    old_mode: None,
                    new_mode: Some(convert_entry_mode(entry_mode)),
                });
            }
            gix::diff::tree_with_rewrites::Change::Deletion {
                location,
                entry_mode,
                id,
                ..
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                entries.push(DiffEntry {
                    path: location.to_string(),
                    change_type: ChangeType::Deleted,
                    old_oid: from_gix_oid(id),
                    new_oid: GitOid::ZERO,
                    old_mode: Some(convert_entry_mode(entry_mode)),
                    new_mode: None,
                });
            }
            gix::diff::tree_with_rewrites::Change::Modification {
                location,
                previous_entry_mode,
                previous_id,
                entry_mode,
                id,
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                entries.push(DiffEntry {
                    path: location.to_string(),
                    change_type: ChangeType::Modified,
                    old_oid: from_gix_oid(previous_id),
                    new_oid: from_gix_oid(id),
                    old_mode: Some(convert_entry_mode(previous_entry_mode)),
                    new_mode: Some(convert_entry_mode(entry_mode)),
                });
            }
            gix::diff::tree_with_rewrites::Change::Rewrite {
                source_location,
                source_entry_mode,
                source_id,
                entry_mode,
                id,
                location,
                copy,
                ..
            } => {
                // Only emit renames (not copies) as the Renamed variant.
                // Copies have no pre-existing single source being consumed;
                // if we ever want them, extend ChangeType. For now, a copy
                // surfaces as a plain Addition to keep semantics identical
                // to the non-rename-aware path.
                if copy {
                    if entry_mode.is_tree() {
                        continue;
                    }
                    entries.push(DiffEntry {
                        path: location.to_string(),
                        change_type: ChangeType::Added,
                        old_oid: GitOid::ZERO,
                        new_oid: from_gix_oid(id),
                        old_mode: None,
                        new_mode: Some(convert_entry_mode(entry_mode)),
                    });
                    continue;
                }
                entries.push(DiffEntry {
                    path: location.to_string(),
                    change_type: ChangeType::Renamed {
                        from: source_location.to_string(),
                    },
                    old_oid: from_gix_oid(source_id),
                    new_oid: from_gix_oid(id),
                    old_mode: Some(convert_entry_mode(source_entry_mode)),
                    new_mode: Some(convert_entry_mode(entry_mode)),
                });
            }
        }
    }

    Ok(entries)
}
