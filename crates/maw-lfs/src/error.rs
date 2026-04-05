//! Top-level error type for maw-lfs.
//!
//! Each submodule defines its own specific error type; `LfsError` wraps them
//! for callers that don't need to distinguish.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum LfsError {
    #[error("pointer: {0}")]
    Pointer(#[from] crate::pointer::ParseError),

    #[error("store: {0}")]
    Store(#[from] crate::store::StoreError),

    #[error("attributes: {0}")]
    Attrs(#[from] crate::attrs::AttrsError),

    #[error("batch: {0}")]
    Batch(#[from] crate::batch::BatchError),

    #[error("credentials: {0}")]
    Creds(#[from] crate::creds::CredsError),
}
