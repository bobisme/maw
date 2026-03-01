//! gix-backed checkout and index operations.

use std::path::Path;

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::*;

pub fn checkout_tree(repo: &GixRepo, oid: GitOid, workdir: &Path) -> Result<(), GitError> {
    todo!("Implement with gix: State::from_tree() + gix_worktree_state::checkout()")
}

pub fn read_index(repo: &GixRepo) -> Result<Vec<IndexEntry>, GitError> {
    todo!("Implement with gix: repo.open_index()")
}

pub fn write_index(repo: &GixRepo, entries: &[IndexEntry]) -> Result<(), GitError> {
    todo!("Implement with gix: index.write()")
}
