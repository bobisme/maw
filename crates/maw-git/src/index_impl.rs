//! Index (staging area) operations for [`GixRepo`].

use crate::GixRepo;
use crate::error::GitError;

/// Reset the index to match HEAD, unstaging all staged changes.
///
/// This reads HEAD's tree into the index without touching the working tree,
/// equivalent to `git reset HEAD`.
pub fn unstage_all(repo: &GixRepo) -> Result<(), GitError> {
    let head_commit = repo
        .repo
        .head_commit()
        .map_err(|e| GitError::BackendError {
            message: format!("failed to resolve HEAD commit: {e}"),
        })?;

    let head_tree_id = head_commit
        .tree_id()
        .map_err(|e| GitError::BackendError {
            message: format!("failed to read HEAD tree id: {e}"),
        })?;

    // Build a new index state from the HEAD tree.
    let state = gix::index::State::from_tree(&head_tree_id, &repo.repo.objects, Default::default())
        .map_err(|e| GitError::BackendError {
            message: format!("failed to create index from tree: {e}"),
        })?;

    // Write the new index to disk.
    let mut new_index = gix::index::File::from_state(state, repo.repo.index_path());
    new_index
        .write(Default::default())
        .map_err(|e| GitError::BackendError {
            message: format!("failed to write index: {e}"),
        })?;

    Ok(())
}
