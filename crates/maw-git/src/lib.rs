//! Git abstraction layer for maw.
//!
//! This crate defines the [`GitRepo`] trait — the single interface through
//! which all other maw crates interact with git. No maw crate should import
//! gix (or any other git library) directly; instead, they depend on `maw-git`
//! and program against the trait.
//!
//! # Crate layout
//!
//! - [`repo`] — the [`GitRepo`] trait definition.
//! - [`types`] — value types used in trait signatures ([`GitOid`], [`RefName`],
//!   [`TreeEntry`], [`DiffEntry`], etc.).
//! - [`error`] — the [`GitError`] enum returned by all trait methods.

pub mod error;
pub mod merge;
pub mod repo;
pub mod types;

// gix-backed implementation modules
mod checkout_impl;
mod config_impl;
mod diff_impl;
mod gix_repo;
mod index_impl;
#[cfg(feature = "lfs")]
mod lfs_clean;
mod objects_impl;
mod push_impl;
mod refs_impl;
mod rev_walk_impl;
mod stash_impl;
mod status_impl;
mod worktree_impl;

pub use gix_repo::GixRepo;

/// Run the LFS smudge post-pass on a worktree at `ws_path`, using
/// `target_commit` as the tree to check for missing files (instead of HEAD,
/// which may be stale if checkout failed).
///
/// This is the public entry point for callers outside maw-git (e.g. maw-cli
/// merge code that needs to smudge after a `git checkout` CLI call).
#[cfg(feature = "lfs")]
pub fn lfs_smudge_worktree_at(
    ws_path: &std::path::Path,
    target_commit: &str,
) -> Result<(), GitError> {
    checkout_impl::lfs_smudge_worktree_at(ws_path, target_commit)
}

// Re-export the main trait and commonly used types at the crate root for
// ergonomic imports: `use maw_git::{GitRepo, GitOid, GitError};`
pub use error::GitError;
pub use repo::GitRepo;
pub use types::{
    ChangeType, CommitInfo, DiffEntry, EntryMode, FileStatus, GitOid, IndexEntry, OidParseError,
    RefEdit, RefName, RefNameError, StatusEntry, TreeEdit, TreeEntry, WorktreeInfo,
};
