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
mod objects_impl;
mod push_impl;
mod refs_impl;
mod stash_impl;
mod status_impl;
mod worktree_impl;

pub use gix_repo::GixRepo;

// Re-export the main trait and commonly used types at the crate root for
// ergonomic imports: `use maw_git::{GitRepo, GitOid, GitError};`
pub use error::GitError;
pub use repo::GitRepo;
pub use types::{
    ChangeType, CommitInfo, DiffEntry, EntryMode, FileStatus, GitOid, IndexEntry, OidParseError,
    RefEdit, RefName, RefNameError, StatusEntry, TreeEdit, TreeEntry, WorktreeInfo,
};
