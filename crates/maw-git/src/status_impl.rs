//! gix-backed status and dirty detection.

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::*;

pub fn is_dirty(repo: &GixRepo) -> Result<bool, GitError> {
    todo!("Implement with gix: repo.is_dirty()")
}

pub fn status(repo: &GixRepo) -> Result<Vec<StatusEntry>, GitError> {
    todo!("Implement with gix: repo.status()")
}
