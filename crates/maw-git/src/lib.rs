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
pub mod repo;
pub mod types;

// Re-export the main trait and commonly used types at the crate root for
// ergonomic imports: `use maw_git::{GitRepo, GitOid, GitError};`
pub use error::GitError;
pub use repo::GitRepo;
pub use types::{
    ChangeType, CommitInfo, DiffEntry, EntryMode, FileStatus, GitOid, IndexEntry, OidParseError,
    RefEdit, RefName, RefNameError, StatusEntry, TreeEdit, TreeEntry, WorktreeInfo,
};
