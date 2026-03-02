//! The gix-backed implementation of [`GitRepo`].

use std::path::{Path, PathBuf};

use crate::error::GitError;
use crate::repo::GitRepo;
use crate::types::*;

/// A [`GitRepo`] implementation backed by [gix](https://github.com/GitoxideLabs/gitoxide).
///
/// Construct via [`GixRepo::open`] or [`GixRepo::open_at`].
pub struct GixRepo {
    pub(crate) repo: gix::Repository,
    pub(crate) workdir: Option<PathBuf>,
}

impl GixRepo {
    /// Open the git repository at or above `path`.
    pub fn open(path: &Path) -> Result<Self, GitError> {
        let repo = gix::open(path).map_err(|e| GitError::BackendError { message: e.to_string() })?;
        let workdir = repo.workdir().map(|p| p.to_path_buf());
        Ok(Self { repo, workdir })
    }

    /// Open a git repository at exactly `path` (no parent discovery).
    pub fn open_at(path: &Path) -> Result<Self, GitError> {
        let repo = gix::open_opts(path, gix::open::Options::isolated())
            .map_err(|e| GitError::BackendError { message: e.to_string() })?;
        let workdir = repo.workdir().map(|p| p.to_path_buf());
        Ok(Self { repo, workdir })
    }
}

impl GitRepo for GixRepo {
    // === Refs ===
    fn read_ref(&self, name: &RefName) -> Result<Option<GitOid>, GitError> {
        crate::refs_impl::read_ref(self, name)
    }

    fn write_ref(&self, name: &RefName, oid: GitOid, log_message: &str) -> Result<(), GitError> {
        crate::refs_impl::write_ref(self, name, oid, log_message)
    }

    fn delete_ref(&self, name: &RefName) -> Result<(), GitError> {
        crate::refs_impl::delete_ref(self, name)
    }

    fn atomic_ref_update(&self, edits: &[RefEdit]) -> Result<(), GitError> {
        crate::refs_impl::atomic_ref_update(self, edits)
    }

    fn list_refs(&self, prefix: &str) -> Result<Vec<(RefName, GitOid)>, GitError> {
        crate::refs_impl::list_refs(self, prefix)
    }

    // === Rev-parse ===
    fn rev_parse(&self, spec: &str) -> Result<GitOid, GitError> {
        crate::refs_impl::rev_parse(self, spec)
    }

    fn rev_parse_opt(&self, spec: &str) -> Result<Option<GitOid>, GitError> {
        crate::refs_impl::rev_parse_opt(self, spec)
    }

    // === Object read ===
    fn read_blob(&self, oid: GitOid) -> Result<Vec<u8>, GitError> {
        crate::objects_impl::read_blob(self, oid)
    }

    fn read_tree(&self, oid: GitOid) -> Result<Vec<TreeEntry>, GitError> {
        crate::objects_impl::read_tree(self, oid)
    }

    fn read_commit(&self, oid: GitOid) -> Result<CommitInfo, GitError> {
        crate::objects_impl::read_commit(self, oid)
    }

    // === Object write ===
    fn write_blob(&self, data: &[u8]) -> Result<GitOid, GitError> {
        crate::objects_impl::write_blob(self, data)
    }

    fn write_tree(&self, entries: &[TreeEntry]) -> Result<GitOid, GitError> {
        crate::objects_impl::write_tree(self, entries)
    }

    fn create_commit(
        &self,
        tree: GitOid,
        parents: &[GitOid],
        message: &str,
        update_ref: Option<&RefName>,
    ) -> Result<GitOid, GitError> {
        crate::objects_impl::create_commit(self, tree, parents, message, update_ref)
    }

    // === Tree editing ===
    fn edit_tree(&self, base: GitOid, edits: &[TreeEdit]) -> Result<GitOid, GitError> {
        crate::objects_impl::edit_tree(self, base, edits)
    }

    // === Index ===
    fn read_index(&self) -> Result<Vec<IndexEntry>, GitError> {
        crate::checkout_impl::read_index(self)
    }

    fn write_index(&self, entries: &[IndexEntry]) -> Result<(), GitError> {
        crate::checkout_impl::write_index(self, entries)
    }

    // === Checkout ===
    fn checkout_tree(&self, oid: GitOid, workdir: &Path) -> Result<(), GitError> {
        crate::checkout_impl::checkout_tree(self, oid, workdir)
    }

    // === Status ===
    fn is_dirty(&self) -> Result<bool, GitError> {
        crate::status_impl::is_dirty(self)
    }

    fn status(&self) -> Result<Vec<StatusEntry>, GitError> {
        crate::status_impl::status(self)
    }

    // === Diff ===
    fn diff_trees(&self, old: Option<GitOid>, new: GitOid) -> Result<Vec<DiffEntry>, GitError> {
        crate::diff_impl::diff_trees(self, old, new)
    }

    // === Worktrees ===
    fn worktree_add(&self, name: &str, target: GitOid, path: &Path) -> Result<(), GitError> {
        crate::worktree_impl::worktree_add(self, name, target, path)
    }

    fn worktree_remove(&self, name: &str) -> Result<(), GitError> {
        crate::worktree_impl::worktree_remove(self, name)
    }

    fn worktree_list(&self) -> Result<Vec<WorktreeInfo>, GitError> {
        crate::worktree_impl::worktree_list(self)
    }

    // === Stash ===
    fn stash_create(&self) -> Result<Option<GitOid>, GitError> {
        crate::stash_impl::stash_create(self)
    }

    fn stash_apply(&self, oid: GitOid) -> Result<(), GitError> {
        crate::stash_impl::stash_apply(self, oid)
    }

    fn unstage_all(&self) -> Result<(), GitError> {
        crate::index_impl::unstage_all(self)
    }

    // === Push ===
    fn push_branch(
        &self,
        remote: &str,
        local_ref: &str,
        remote_ref: &str,
        force: bool,
    ) -> Result<(), GitError> {
        crate::push_impl::push_branch(self, remote, local_ref, remote_ref, force)
    }

    fn push_tag(&self, remote: &str, tag: &str) -> Result<(), GitError> {
        crate::push_impl::push_tag(self, remote, tag)
    }

    // === Config ===
    fn read_config(&self, key: &str) -> Result<Option<String>, GitError> {
        crate::config_impl::read_config(self, key)
    }

    fn write_config(&self, key: &str, value: &str) -> Result<(), GitError> {
        crate::config_impl::write_config(self, key, value)
    }

    // === Ancestry ===
    fn is_ancestor(&self, ancestor: GitOid, descendant: GitOid) -> Result<bool, GitError> {
        crate::refs_impl::is_ancestor(self, ancestor, descendant)
    }

    fn merge_base(&self, a: GitOid, b: GitOid) -> Result<Option<GitOid>, GitError> {
        crate::refs_impl::merge_base(self, a, b)
    }
}
