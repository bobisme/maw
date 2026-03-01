//! gix-backed ref, rev-parse, and ancestry operations.

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::*;

pub fn read_ref(repo: &GixRepo, name: &RefName) -> Result<Option<GitOid>, GitError> {
    todo!("Implement with gix: repo.try_find_reference()")
}

pub fn write_ref(repo: &GixRepo, name: &RefName, oid: GitOid, log_message: &str) -> Result<(), GitError> {
    todo!("Implement with gix: repo.reference()")
}

pub fn delete_ref(repo: &GixRepo, name: &RefName) -> Result<(), GitError> {
    todo!("Implement with gix: ref.delete()")
}

pub fn atomic_ref_update(repo: &GixRepo, edits: &[RefEdit]) -> Result<(), GitError> {
    todo!("Implement with gix: repo.edit_references() with CAS")
}

pub fn list_refs(repo: &GixRepo, prefix: &str) -> Result<Vec<(RefName, GitOid)>, GitError> {
    todo!("Implement with gix: repo.references().prefixed()")
}

pub fn rev_parse(repo: &GixRepo, spec: &str) -> Result<GitOid, GitError> {
    todo!("Implement with gix: repo.rev_parse_single()")
}

pub fn rev_parse_opt(repo: &GixRepo, spec: &str) -> Result<Option<GitOid>, GitError> {
    todo!("Implement with gix: repo.rev_parse_single() with None on not-found")
}

pub fn is_ancestor(repo: &GixRepo, ancestor: GitOid, descendant: GitOid) -> Result<bool, GitError> {
    todo!("Implement ancestry check via commit graph traversal")
}

pub fn merge_base(repo: &GixRepo, a: GitOid, b: GitOid) -> Result<Option<GitOid>, GitError> {
    todo!("Implement merge-base via gix commit graph")
}
