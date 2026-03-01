//! gix-backed object read/write and tree editing operations.

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::*;

pub fn read_blob(repo: &GixRepo, oid: GitOid) -> Result<Vec<u8>, GitError> {
    todo!("Implement with gix: repo.find_object()")
}

pub fn read_tree(repo: &GixRepo, oid: GitOid) -> Result<Vec<TreeEntry>, GitError> {
    todo!("Implement with gix: repo.find_object().into_tree()")
}

pub fn read_commit(repo: &GixRepo, oid: GitOid) -> Result<CommitInfo, GitError> {
    todo!("Implement with gix: repo.find_object().into_commit()")
}

pub fn write_blob(repo: &GixRepo, data: &[u8]) -> Result<GitOid, GitError> {
    todo!("Implement with gix: repo.write_blob()")
}

pub fn write_tree(repo: &GixRepo, entries: &[TreeEntry]) -> Result<GitOid, GitError> {
    todo!("Implement with gix: repo.write_object(tree)")
}

pub fn create_commit(
    repo: &GixRepo,
    tree: GitOid,
    parents: &[GitOid],
    message: &str,
    update_ref: Option<&RefName>,
) -> Result<GitOid, GitError> {
    todo!("Implement with gix: repo.commit_as()")
}

pub fn edit_tree(repo: &GixRepo, base: GitOid, edits: &[TreeEdit]) -> Result<GitOid, GitError> {
    todo!("Implement with gix: repo.edit_tree() -> Editor")
}
