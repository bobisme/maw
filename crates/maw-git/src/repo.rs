//! The [`GitRepo`] trait — the single abstraction boundary between maw and git.
//!
//! All maw crates interact with git exclusively through this trait. The trait
//! is object-safe so callers can use `dyn GitRepo` or `Box<dyn GitRepo>`.
//!
//! Method groups correspond to the git operation categories identified in the
//! maw capability assessment (~398 call sites):
//!
//! | Group        | Approx. calls | Methods                                     |
//! |-------------|---------------|---------------------------------------------|
//! | Refs         | ~60           | `read_ref`, `write_ref`, `delete_ref`, ...  |
//! | Rev-parse    | ~80           | `rev_parse`, `rev_parse_opt`                |
//! | Object read  | ~80           | `read_blob`, `read_tree`, `read_commit`     |
//! | Object write | ~40           | `write_blob`, `write_tree`, `create_commit` |
//! | Tree editing | ~20           | `edit_tree`                                 |
//! | Index        | ~15           | `read_index`, `write_index`                 |
//! | Checkout     | ~15           | `checkout_tree`                             |
//! | Status       | ~15           | `is_dirty`, `status`                        |
//! | Diff         | ~20           | `diff_trees`                                |
//! | Worktrees    | ~20           | `worktree_add/remove/list`                  |
//! | Stash        | ~15           | `stash_create`, `stash_apply`               |
//! | Push         | ~17           | `push_branch`, `push_tag`                   |
//! | Config       | ~15           | `read_config`, `write_config`               |
//! | Ancestry     | ~10           | `is_ancestor`, `merge_base`                 |

use std::path::Path;

use crate::error::GitError;
use crate::types::{
    CommitInfo, DiffEntry, GitOid, IndexEntry, RefEdit, RefName, StatusEntry, TreeEdit, TreeEntry,
    WorktreeInfo,
};

/// The git abstraction trait used by all maw crates.
///
/// Implementations may be backed by gix (the preferred backend), a git CLI
/// shim (for gradual migration), or a test double.
///
/// # Object safety
///
/// This trait is object-safe: no generic methods, no `Self` in return position
/// outside of `Result`. Callers may use `&dyn GitRepo` or `Box<dyn GitRepo>`.
pub trait GitRepo {
    // -----------------------------------------------------------------------
    // Refs (~60 call sites)
    //
    // Replaces: git rev-parse, git update-ref, git update-ref -d,
    //           git update-ref --stdin, git for-each-ref
    // -----------------------------------------------------------------------

    /// Resolve a ref to its OID, returning `None` if the ref does not exist.
    ///
    /// Replaces: `git rev-parse <ref>` (when used to resolve a known ref name).
    fn read_ref(&self, name: &RefName) -> Result<Option<GitOid>, GitError>;

    /// Create or overwrite a ref unconditionally.
    ///
    /// Replaces: `git update-ref <name> <oid>`.
    ///
    /// `log_message` is written to the reflog entry. Pass an empty string if
    /// no reflog message is needed.
    fn write_ref(&self, name: &RefName, oid: GitOid, log_message: &str) -> Result<(), GitError>;

    /// Delete a ref. No-op if the ref does not exist.
    ///
    /// Replaces: `git update-ref -d <name>`.
    fn delete_ref(&self, name: &RefName) -> Result<(), GitError>;

    /// Atomically apply a batch of ref updates with compare-and-swap semantics.
    ///
    /// All updates succeed or all fail. Each [`RefEdit`] carries an expected
    /// old OID; if any ref's current value differs, the entire transaction is
    /// aborted and [`GitError::RefConflict`] is returned.
    ///
    /// Replaces: `git update-ref --stdin` with `start`/`prepare`/`commit`.
    fn atomic_ref_update(&self, edits: &[RefEdit]) -> Result<(), GitError>;

    /// List refs matching a prefix (e.g., `"refs/manifold/"`, `"refs/heads/"`).
    ///
    /// Returns `(ref_name, oid)` pairs sorted by ref name. The prefix is
    /// matched literally.
    ///
    /// Replaces: `git for-each-ref --format=... refs/some/prefix/`.
    fn list_refs(&self, prefix: &str) -> Result<Vec<(RefName, GitOid)>, GitError>;

    // -----------------------------------------------------------------------
    // Rev-parse (~80 call sites)
    //
    // Replaces: git rev-parse <spec>
    // -----------------------------------------------------------------------

    /// Resolve a revision specification to an OID.
    ///
    /// Supports the same syntax as `git rev-parse`: commit-ish references,
    /// `HEAD~3`, `@{u}`, etc.
    ///
    /// Returns [`GitError::NotFound`] if the spec cannot be resolved.
    ///
    /// Replaces: `git rev-parse <spec>` (general revspec resolution).
    fn rev_parse(&self, spec: &str) -> Result<GitOid, GitError>;

    /// Like [`rev_parse`](Self::rev_parse) but returns `None` instead of an
    /// error when the spec cannot be resolved.
    fn rev_parse_opt(&self, spec: &str) -> Result<Option<GitOid>, GitError>;

    // -----------------------------------------------------------------------
    // Object read (~80 call sites)
    //
    // Replaces: git cat-file blob, git ls-tree, git cat-file commit,
    //           git cat-file -t, git cat-file -p
    // -----------------------------------------------------------------------

    /// Read the contents of a blob object.
    ///
    /// Returns the raw byte content.
    ///
    /// Replaces: `git cat-file blob <oid>`.
    fn read_blob(&self, oid: GitOid) -> Result<Vec<u8>, GitError>;

    /// Read the entries of a tree object.
    ///
    /// Returns the flat list of entries (one level deep, not recursive).
    ///
    /// Replaces: `git ls-tree <oid>`.
    fn read_tree(&self, oid: GitOid) -> Result<Vec<TreeEntry>, GitError>;

    /// Read a commit object's metadata.
    ///
    /// Replaces: `git cat-file commit <oid>` / `git log -1 --format=...`.
    fn read_commit(&self, oid: GitOid) -> Result<CommitInfo, GitError>;

    // -----------------------------------------------------------------------
    // Object write (~40 call sites)
    //
    // Replaces: git hash-object -w, git mktree, git commit-tree
    // -----------------------------------------------------------------------

    /// Write a blob to the object store and return its OID.
    ///
    /// Replaces: `git hash-object -w --stdin` / writing a blob via the
    /// object database.
    fn write_blob(&self, data: &[u8]) -> Result<GitOid, GitError>;

    /// Write a tree object from a list of entries and return its OID.
    ///
    /// Replaces: `git mktree`.
    fn write_tree(&self, entries: &[TreeEntry]) -> Result<GitOid, GitError>;

    /// Create a commit object and optionally update a ref to point to it.
    ///
    /// If `update_ref` is `Some`, the given ref is updated to the new commit
    /// OID after the commit is written.
    ///
    /// Replaces: `git commit-tree` + optional `git update-ref`.
    fn create_commit(
        &self,
        tree: GitOid,
        parents: &[GitOid],
        message: &str,
        update_ref: Option<&RefName>,
    ) -> Result<GitOid, GitError>;

    // -----------------------------------------------------------------------
    // Tree editing (~20 call sites)
    //
    // Replaces: sequences of git ls-tree + git mktree for path-based edits
    // -----------------------------------------------------------------------

    /// Apply a set of edits to an existing tree and return the OID of the new tree.
    ///
    /// Edits may insert, update, or remove entries at arbitrary paths
    /// (including nested paths like `"src/lib.rs"`). Intermediate trees are
    /// created or updated as needed.
    ///
    /// Replaces: manual tree traversal + `git mktree` pipelines.
    fn edit_tree(&self, base: GitOid, edits: &[TreeEdit]) -> Result<GitOid, GitError>;

    // -----------------------------------------------------------------------
    // Index (~15 call sites)
    //
    // Replaces: git ls-files, git read-tree, git update-index
    // -----------------------------------------------------------------------

    /// Read the current index (staging area) entries.
    ///
    /// Replaces: `git ls-files --stage`.
    fn read_index(&self) -> Result<Vec<IndexEntry>, GitError>;

    /// Replace the index with the given entries.
    ///
    /// Replaces: `git read-tree` + `git update-index`.
    fn write_index(&self, entries: &[IndexEntry]) -> Result<(), GitError>;

    // -----------------------------------------------------------------------
    // Checkout (~15 call sites)
    //
    // Replaces: git checkout <branch>, git checkout <oid> -- .,
    //           git read-tree + checkout-index
    // -----------------------------------------------------------------------

    /// Check out a tree into the working directory.
    ///
    /// Materializes the tree at `oid` into `workdir`, updating the index
    /// to match. Existing working-tree files not in the tree are removed.
    ///
    /// Replaces: `git checkout <oid> -- .` / `git read-tree -u <oid>`.
    fn checkout_tree(&self, oid: GitOid, workdir: &Path) -> Result<(), GitError>;

    // -----------------------------------------------------------------------
    // Status (~15 call sites)
    //
    // Replaces: git status --porcelain, git diff --quiet,
    //           git diff --cached --quiet
    // -----------------------------------------------------------------------

    /// Returns `true` if the working tree or index has uncommitted changes.
    ///
    /// Replaces: `git diff --quiet && git diff --cached --quiet` (exit code check).
    fn is_dirty(&self) -> Result<bool, GitError>;

    /// Return the list of changed files with their statuses.
    ///
    /// Replaces: `git status --porcelain`.
    fn status(&self) -> Result<Vec<StatusEntry>, GitError>;

    // -----------------------------------------------------------------------
    // Diff (~20 call sites)
    //
    // Replaces: git diff-tree --no-commit-id -r, git diff --name-status
    // -----------------------------------------------------------------------

    /// Diff two trees and return the list of changed files.
    ///
    /// If `old` is `None`, the diff is against an empty tree (i.e., all files
    /// in `new` appear as additions).
    ///
    /// Replaces: `git diff-tree -r <old> <new>`.
    fn diff_trees(&self, old: Option<GitOid>, new: GitOid) -> Result<Vec<DiffEntry>, GitError>;

    // -----------------------------------------------------------------------
    // Worktrees (~20 call sites)
    //
    // Replaces: git worktree add, git worktree remove, git worktree list
    // -----------------------------------------------------------------------

    /// Create a new linked worktree.
    ///
    /// Creates a worktree at `path` with HEAD detached at `target`.
    ///
    /// Replaces: `git worktree add --detach <path> <target>`.
    fn worktree_add(&self, name: &str, target: GitOid, path: &Path) -> Result<(), GitError>;

    /// Remove a linked worktree by name.
    ///
    /// Replaces: `git worktree remove <name>`.
    fn worktree_remove(&self, name: &str) -> Result<(), GitError>;

    /// List all worktrees (main + linked).
    ///
    /// Replaces: `git worktree list --porcelain`.
    fn worktree_list(&self) -> Result<Vec<WorktreeInfo>, GitError>;

    // -----------------------------------------------------------------------
    // Stash (~15 call sites)
    //
    // Replaces: git stash create, git stash apply
    // -----------------------------------------------------------------------

    /// Create a stash commit from the current working tree and index state
    /// without modifying them.
    ///
    /// Returns `None` if there is nothing to stash (clean working tree).
    ///
    /// Replaces: `git stash create` (no stack push, just creates the object).
    fn stash_create(&self) -> Result<Option<GitOid>, GitError>;

    /// Apply a stash commit to the working tree.
    ///
    /// Does not remove the stash object. Conflicts are left as merge markers
    /// in the working tree.
    ///
    /// Replaces: `git stash apply <oid>`.
    fn stash_apply(&self, oid: GitOid) -> Result<(), GitError>;

    // -----------------------------------------------------------------------
    // Push (~17 call sites)
    //
    // Replaces: git push origin <branch>, git push origin <tag>
    // -----------------------------------------------------------------------

    /// Push a local ref to a remote.
    ///
    /// If `force` is true, the push is a force-push (`git push --force`).
    ///
    /// Replaces: `git push <remote> <local_ref>:<remote_ref>` (or `--force`).
    fn push_branch(
        &self,
        remote: &str,
        local_ref: &str,
        remote_ref: &str,
        force: bool,
    ) -> Result<(), GitError>;

    /// Push a single tag to a remote.
    ///
    /// Replaces: `git push <remote> <tag>`.
    fn push_tag(&self, remote: &str, tag: &str) -> Result<(), GitError>;

    // -----------------------------------------------------------------------
    // Config (~15 call sites)
    //
    // Replaces: git config <key>, git config <key> <value>
    // -----------------------------------------------------------------------

    /// Read a git config value. Returns `None` if the key is not set.
    ///
    /// Replaces: `git config --get <key>`.
    fn read_config(&self, key: &str) -> Result<Option<String>, GitError>;

    /// Set a git config value.
    ///
    /// Replaces: `git config <key> <value>`.
    fn write_config(&self, key: &str, value: &str) -> Result<(), GitError>;

    // -----------------------------------------------------------------------
    // Ancestry (~10 call sites)
    //
    // Replaces: git merge-base --is-ancestor, git merge-base
    // -----------------------------------------------------------------------

    /// Check if `ancestor` is an ancestor of `descendant`.
    ///
    /// Returns `true` if `ancestor` is reachable from `descendant` following
    /// parent links.
    ///
    /// Replaces: `git merge-base --is-ancestor <ancestor> <descendant>`.
    fn is_ancestor(&self, ancestor: GitOid, descendant: GitOid) -> Result<bool, GitError>;

    /// Find the best common ancestor (merge base) of two commits.
    ///
    /// Returns `None` if the commits have no common ancestor.
    ///
    /// Replaces: `git merge-base <a> <b>`.
    fn merge_base(&self, a: GitOid, b: GitOid) -> Result<Option<GitOid>, GitError>;
}
