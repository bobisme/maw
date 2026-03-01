//! Worktree add/remove/list built from gix primitives.
//!
//! gix does not provide high-level worktree lifecycle APIs.
//! We build them from the documented git worktree format.

use std::path::Path;

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::*;

pub fn worktree_add(
    repo: &GixRepo,
    name: &str,
    target: GitOid,
    path: &Path,
) -> Result<(), GitError> {
    todo!("Build worktree from primitives — see bn-1cc1 for 9-step algorithm")
}

pub fn worktree_remove(repo: &GixRepo, name: &str) -> Result<(), GitError> {
    todo!("Remove worktree admin dir and working directory")
}

pub fn worktree_list(repo: &GixRepo) -> Result<Vec<WorktreeInfo>, GitError> {
    todo!("Read .git/worktrees/ directory entries")
}
