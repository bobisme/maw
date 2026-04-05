//! Native git-lfs support for maw.
//!
//! Provides pointer codec, content-addressed local object store, attributes
//! matching, HTTPS batch API client, and credential resolution — all in pure
//! Rust, with no subprocess invocations of `git` or `git-lfs`.
//!
//! # Scope
//!
//! maw-lfs replaces git-lfs's filter-driver role for the checkout and
//! commit paths that maw owns. It is **interoperable** with git-lfs on disk:
//! pointer blobs and the `.git/lfs/objects/` layout are bit-identical, so
//! running `git lfs` commands against the same repo works unchanged.
//!
//! # Non-goals
//!
//! - LFS file locking, custom transfer adapters (SSH, tus, multipart).
//! - Replacing git-lfs for end users — this crate serves maw's internal
//!   checkout/commit paths only.

pub mod attrs;
pub mod batch;
pub mod creds;
pub mod error;
pub mod pointer;
pub mod store;

pub use attrs::AttrsMatcher;
pub use batch::BatchClient;
pub use creds::CredentialProvider;
pub use error::LfsError;
pub use pointer::{looks_like_pointer, Pointer};
pub use store::Store;
