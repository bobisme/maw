//! Error types for git operations.
//!
//! [`GitError`] is the single error type returned by all [`GitRepo`](crate::GitRepo) trait
//! methods. It uses rich enum variants so callers can match on specific failure
//! modes (e.g., missing ref, CAS mismatch, dirty worktree) without parsing
//! error messages.

use std::path::PathBuf;

use thiserror::Error;

/// Errors returned by [`GitRepo`](crate::GitRepo) operations.
#[derive(Debug, Error)]
pub enum GitError {
    /// A requested object, ref, or path was not found.
    #[error("not found: {message}")]
    NotFound {
        /// Human-readable description of what was missing.
        message: String,
    },

    /// A ref update failed because the ref's current value did not match the
    /// expected old value (compare-and-swap / optimistic concurrency failure).
    #[error("ref conflict on `{ref_name}`: {message}")]
    RefConflict {
        /// The ref that could not be updated.
        ref_name: String,
        /// Details about the mismatch.
        message: String,
    },

    /// An operation was refused because the working tree has uncommitted changes.
    #[error("dirty worktree at {}: {message}", path.display())]
    DirtyWorktree {
        /// Path to the worktree root.
        path: PathBuf,
        /// What was dirty (untracked files, staged changes, etc.).
        message: String,
    },

    /// An OID string could not be parsed or was otherwise invalid.
    #[error("invalid OID `{value}`: {reason}")]
    InvalidOid {
        /// The raw value that failed validation.
        value: String,
        /// Why validation failed.
        reason: String,
    },

    /// An I/O error occurred (file system, process spawn, etc.).
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),

    /// A push to a remote was rejected or failed.
    #[error("push to `{remote}` failed: {message}")]
    PushFailed {
        /// The remote name (e.g., `"origin"`).
        remote: String,
        /// Details about the failure.
        message: String,
    },

    /// A merge or rebase operation produced conflicts.
    #[error("merge conflict: {message}")]
    MergeConflict {
        /// Description of the conflict.
        message: String,
    },

    /// The underlying git backend (gix, CLI, etc.) returned an unclassified error.
    ///
    /// This is the catch-all for errors that don't fit other variants. The
    /// `message` should include enough context to diagnose the failure.
    #[error("git backend error: {message}")]
    BackendError {
        /// Freeform error description from the backend.
        message: String,
    },
}
