//! Stash create/apply built from gix commit/tree primitives.
//!
//! gix does not provide a high-level stash API.
//! We build stash from tree, index, and commit operations.

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::*;

pub fn stash_create(repo: &GixRepo) -> Result<Option<GitOid>, GitError> {
    todo!("Build stash commit from index + worktree state — see bn-1qd5")
}

pub fn stash_apply(repo: &GixRepo, oid: GitOid) -> Result<(), GitError> {
    todo!("Apply stash by diffing stash tree against parent and replaying")
}
