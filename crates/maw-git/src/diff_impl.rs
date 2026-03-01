//! gix-backed tree-to-tree diff.

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::*;

pub fn diff_trees(
    repo: &GixRepo,
    old: Option<GitOid>,
    new: GitOid,
) -> Result<Vec<DiffEntry>, GitError> {
    todo!("Implement with gix_diff::tree::Changes")
}
