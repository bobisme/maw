//! Workspace error types for Manifold.
//!
//! Defines [`ManifoldError`], the unified error type for all workspace operations.
//! Error messages are designed to be agent-friendly: each variant includes
//! a clear description of what went wrong and actionable guidance on how to fix it.
//!
//! No legacy VCS-specific concepts leak into this module — all errors are expressed in
//! terms of Manifold's own abstractions (workspaces, epochs, merge).

use std::fmt;
use std::path::PathBuf;

use crate::model::types::WorkspaceId;

// ---------------------------------------------------------------------------
// ManifoldError
// ---------------------------------------------------------------------------

/// Unified error type for Manifold workspace operations.
///
/// Each variant is designed to be self-contained: an agent receiving this error
/// should be able to understand what happened and what to do next without
/// additional context.
#[derive(Debug)]
pub enum ManifoldError {
    /// A workspace with this name already exists.
    WorkspaceExists {
        /// The workspace name that already exists.
        name: WorkspaceId,
    },

    /// The requested workspace does not exist.
    WorkspaceNotFound {
        /// The workspace name that was not found.
        name: WorkspaceId,
    },

    /// A workspace's on-disk state is corrupted or inconsistent.
    WorkspaceCorrupted {
        /// The workspace name.
        name: WorkspaceId,
        /// Human-readable description of the corruption.
        detail: String,
    },

    /// A workspace name failed validation.
    InvalidWorkspaceName {
        /// The invalid name that was provided.
        name: String,
        /// Why the name is invalid.
        reason: String,
    },

    /// The requested epoch does not exist.
    EpochNotFound {
        /// The epoch identifier (git OID hex string) that was not found.
        epoch: String,
    },

    /// A merge operation encountered conflicts.
    MergeConflict {
        /// Summary of each conflicted file.
        conflicts: Vec<ConflictInfo>,
    },

    /// A merge is already in progress and must be completed or aborted first.
    MergeInProgress {
        /// Description of the in-progress merge state.
        state: String,
    },

    /// A post-merge validation command failed.
    ValidationFailed {
        /// The command that was run.
        command: String,
        /// The process exit code.
        exit_code: i32,
        /// Captured stderr output (may be truncated).
        stderr: String,
    },

    /// A git command failed.
    GitError {
        /// The git command that was run (e.g. `"git worktree add"`).
        command: String,
        /// Captured stderr from git.
        stderr: String,
    },

    /// A configuration file could not be loaded or parsed.
    ConfigError {
        /// Path to the configuration file.
        path: PathBuf,
        /// Human-readable description of the problem.
        detail: String,
    },

    /// An I/O error occurred during a workspace operation.
    Io(std::io::Error),
}

// ---------------------------------------------------------------------------
// ConflictInfo
// ---------------------------------------------------------------------------

/// Summary information about a single conflicted file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictInfo {
    /// Path to the conflicted file, relative to the workspace root.
    pub path: PathBuf,
    /// Human-readable description of the conflict (e.g. "both modified",
    /// "deleted vs modified").
    pub description: String,
}

impl ConflictInfo {
    /// Create a new conflict summary.
    pub const fn new(path: PathBuf, description: String) -> Self {
        Self { path, description }
    }
}

impl fmt::Display for ConflictInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.description)
    }
}

// ---------------------------------------------------------------------------
// Display — agent-friendly error messages
// ---------------------------------------------------------------------------

impl fmt::Display for ManifoldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkspaceExists { name } => {
                write!(
                    f,
                    "workspace '{name}' already exists.\n  To fix: use a different name, or destroy the existing workspace first:\n    maw ws destroy {name}"
                )
            }
            Self::WorkspaceNotFound { name } => {
                write!(
                    f,
                    "workspace '{name}' not found.\n  To fix: check available workspaces:\n    maw ws list"
                )
            }
            Self::WorkspaceCorrupted { name, detail } => {
                write!(
                    f,
                    "workspace '{name}' is corrupted: {detail}\n  To fix: destroy and re-create the workspace:\n    maw ws destroy {name}\n    maw ws create {name}"
                )
            }
            Self::InvalidWorkspaceName { name, reason } => {
                write!(
                    f,
                    "invalid workspace name '{name}': {reason}\n  Workspace names must be lowercase alphanumeric with hyphens, 1-64 characters.\n  Examples: agent-1, feature-auth, bugfix-123"
                )
            }
            Self::EpochNotFound { epoch } => {
                write!(
                    f,
                    "epoch '{epoch}' not found.\n  To fix: check the current epoch:\n    git show-ref refs/manifold/epoch/current"
                )
            }
            Self::MergeConflict { conflicts } => {
                write!(f, "merge conflict in {} file(s):", conflicts.len())?;
                for c in conflicts {
                    write!(f, "\n  - {c}")?;
                }
                write!(
                    f,
                    "\n  To fix: resolve conflicts in each file, then retry the merge."
                )
            }
            Self::MergeInProgress { state } => {
                write!(
                    f,
                    "a merge is already in progress: {state}\n  To fix: complete or abort the current merge before starting a new one."
                )
            }
            Self::ValidationFailed {
                command,
                exit_code,
                stderr,
            } => {
                write!(
                    f,
                    "validation command failed (exit code {exit_code}): {command}"
                )?;
                if !stderr.is_empty() {
                    write!(f, "\n  stderr: {stderr}")?;
                }
                write!(
                    f,
                    "\n  To fix: check the validation output, fix the issue, and retry."
                )
            }
            Self::GitError { command, stderr } => {
                write!(f, "git command failed: {command}")?;
                if !stderr.is_empty() {
                    write!(f, "\n  stderr: {stderr}")?;
                }
                write!(
                    f,
                    "\n  To fix: check git state and retry. Run `git status` for details."
                )
            }
            Self::ConfigError { path, detail } => {
                write!(
                    f,
                    "configuration error in '{}': {}\n  To fix: edit the config file and correct the issue.",
                    path.display(),
                    detail
                )
            }
            Self::Io(err) => {
                write!(
                    f,
                    "I/O error: {err}\n  To fix: check file permissions and disk space."
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// std::error::Error
// ---------------------------------------------------------------------------

impl std::error::Error for ManifoldError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// From impls
// ---------------------------------------------------------------------------

impl From<std::io::Error> for ManifoldError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<crate::config::ConfigError> for ManifoldError {
    fn from(err: crate::config::ConfigError) -> Self {
        Self::ConfigError {
            path: err.path.unwrap_or_default(),
            detail: err.message,
        }
    }
}

impl From<crate::model::types::ValidationError> for ManifoldError {
    fn from(err: crate::model::types::ValidationError) -> Self {
        Self::InvalidWorkspaceName {
            name: err.value,
            reason: err.reason,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ws_id(name: &str) -> WorkspaceId {
        WorkspaceId::new(name).unwrap()
    }

    // -- Display tests: every variant produces actionable output --

    #[test]
    fn display_workspace_exists() {
        let err = ManifoldError::WorkspaceExists {
            name: sample_ws_id("agent-1"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("agent-1"));
        assert!(msg.contains("already exists"));
        assert!(msg.contains("maw ws destroy"));
    }

    #[test]
    fn display_workspace_not_found() {
        let err = ManifoldError::WorkspaceNotFound {
            name: sample_ws_id("ghost"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("ghost"));
        assert!(msg.contains("not found"));
        assert!(msg.contains("maw ws list"));
    }

    #[test]
    fn display_workspace_corrupted() {
        let err = ManifoldError::WorkspaceCorrupted {
            name: sample_ws_id("broken"),
            detail: "missing HEAD ref".to_owned(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("broken"));
        assert!(msg.contains("corrupted"));
        assert!(msg.contains("missing HEAD ref"));
        assert!(msg.contains("maw ws destroy"));
        assert!(msg.contains("maw ws create"));
    }

    #[test]
    fn display_invalid_workspace_name() {
        let err = ManifoldError::InvalidWorkspaceName {
            name: "BAD NAME".to_owned(),
            reason: "contains uppercase".to_owned(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("BAD NAME"));
        assert!(msg.contains("contains uppercase"));
        assert!(msg.contains("lowercase alphanumeric"));
    }

    #[test]
    fn display_epoch_not_found() {
        let err = ManifoldError::EpochNotFound {
            epoch: "abc123".to_owned(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("abc123"));
        assert!(msg.contains("not found"));
        assert!(msg.contains("refs/manifold/epoch/current"));
    }

    #[test]
    fn display_merge_conflict_single() {
        let err = ManifoldError::MergeConflict {
            conflicts: vec![ConflictInfo::new(
                PathBuf::from("src/main.rs"),
                "both modified".to_owned(),
            )],
        };
        let msg = format!("{err}");
        assert!(msg.contains("1 file(s)"));
        assert!(msg.contains("src/main.rs"));
        assert!(msg.contains("both modified"));
        assert!(msg.contains("resolve conflicts"));
    }

    #[test]
    fn display_merge_conflict_multiple() {
        let err = ManifoldError::MergeConflict {
            conflicts: vec![
                ConflictInfo::new(PathBuf::from("a.rs"), "both modified".to_owned()),
                ConflictInfo::new(PathBuf::from("b.rs"), "deleted vs modified".to_owned()),
            ],
        };
        let msg = format!("{err}");
        assert!(msg.contains("2 file(s)"));
        assert!(msg.contains("a.rs"));
        assert!(msg.contains("b.rs"));
    }

    #[test]
    fn display_merge_in_progress() {
        let err = ManifoldError::MergeInProgress {
            state: "merging agent-1 into default".to_owned(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("already in progress"));
        assert!(msg.contains("merging agent-1"));
        assert!(msg.contains("complete or abort"));
    }

    #[test]
    fn display_validation_failed() {
        let err = ManifoldError::ValidationFailed {
            command: "cargo test".to_owned(),
            exit_code: 101,
            stderr: "thread panicked".to_owned(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("cargo test"));
        assert!(msg.contains("101"));
        assert!(msg.contains("thread panicked"));
        assert!(msg.contains("fix the issue"));
    }

    #[test]
    fn display_validation_failed_empty_stderr() {
        let err = ManifoldError::ValidationFailed {
            command: "make check".to_owned(),
            exit_code: 1,
            stderr: String::new(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("make check"));
        assert!(!msg.contains("stderr:"));
    }

    #[test]
    fn display_git_error() {
        let err = ManifoldError::GitError {
            command: "git worktree add".to_owned(),
            stderr: "fatal: already exists".to_owned(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("git worktree add"));
        assert!(msg.contains("fatal: already exists"));
        assert!(msg.contains("git status"));
    }

    #[test]
    fn display_git_error_empty_stderr() {
        let err = ManifoldError::GitError {
            command: "git init".to_owned(),
            stderr: String::new(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("git init"));
        assert!(!msg.contains("stderr:"));
    }

    #[test]
    fn display_config_error() {
        let err = ManifoldError::ConfigError {
            path: PathBuf::from(".manifold/config.toml"),
            detail: "unknown field 'foo'".to_owned(),
        };
        let msg = format!("{err}");
        assert!(msg.contains(".manifold/config.toml"));
        assert!(msg.contains("unknown field 'foo'"));
        assert!(msg.contains("edit the config file"));
    }

    #[test]
    fn display_io_error() {
        let err = ManifoldError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "permission denied",
        ));
        let msg = format!("{err}");
        assert!(msg.contains("permission denied"));
        assert!(msg.contains("file permissions"));
    }

    // -- std::error::Error trait --

    #[test]
    fn error_source_io() {
        let inner = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let err = ManifoldError::Io(inner);
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn error_source_non_io_is_none() {
        let err = ManifoldError::WorkspaceExists {
            name: sample_ws_id("test"),
        };
        assert!(std::error::Error::source(&err).is_none());
    }

    // -- From impls --

    #[test]
    fn from_io_error() {
        let io_err = std::io::Error::other("disk full");
        let err: ManifoldError = io_err.into();
        assert!(matches!(err, ManifoldError::Io(_)));
    }

    #[test]
    fn from_config_error() {
        let cfg_err = crate::config::ConfigError {
            path: Some(PathBuf::from("/repo/.manifold/config.toml")),
            message: "bad syntax".to_owned(),
        };
        let err: ManifoldError = cfg_err.into();
        match err {
            ManifoldError::ConfigError { path, detail } => {
                assert_eq!(path, PathBuf::from("/repo/.manifold/config.toml"));
                assert_eq!(detail, "bad syntax");
            }
            other => panic!("expected ConfigError, got {other:?}"),
        }
    }

    #[test]
    fn from_validation_error() {
        let val_err = crate::model::types::ValidationError {
            kind: crate::model::types::ErrorKind::WorkspaceId,
            value: "BAD".to_owned(),
            reason: "uppercase".to_owned(),
        };
        let err: ManifoldError = val_err.into();
        match err {
            ManifoldError::InvalidWorkspaceName { name, reason } => {
                assert_eq!(name, "BAD");
                assert_eq!(reason, "uppercase");
            }
            other => panic!("expected InvalidWorkspaceName, got {other:?}"),
        }
    }

    // -- ConflictInfo --

    #[test]
    fn conflict_info_display() {
        let c = ConflictInfo::new(PathBuf::from("src/lib.rs"), "both modified".to_owned());
        assert_eq!(format!("{c}"), "src/lib.rs: both modified");
    }

    #[test]
    fn conflict_info_equality() {
        let a = ConflictInfo::new(PathBuf::from("a.rs"), "x".to_owned());
        let b = ConflictInfo::new(PathBuf::from("a.rs"), "x".to_owned());
        assert_eq!(a, b);
    }
}
