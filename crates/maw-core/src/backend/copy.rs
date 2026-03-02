//! Plain recursive-copy workspace backend (universal fallback).
//!
//! Creates workspaces by extracting the epoch's git tree into a fresh
//! directory using `git archive | tar -x`. No `CoW`, no overlayfs — works on
//! any filesystem and any platform.
//!
//! # Directory layout
//!
//! ```text
//! repo-root/
//! └── ws/
//!     └── <name>/         ← workspace (full copy of epoch tree)
//!         └── .maw-epoch  ← stores the base epoch OID (40 hex chars + newline)
//! ```
//!
//! # Performance note
//!
//! Every workspace create is an O(repo-size) operation. For repos with fewer
//! than 30k files this is acceptable; for larger repos prefer the `reflink`
//! or `overlay` backend.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{SnapshotResult, WorkspaceBackend, WorkspaceStatus};
use crate::model::types::{EpochId, WorkspaceId, WorkspaceInfo, WorkspaceMode, WorkspaceState};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Hidden metadata file written into each workspace root.
///
/// Contains the base epoch OID (exactly 40 lowercase hex characters) followed
/// by a newline. This file is excluded from snapshot comparisons.
const EPOCH_FILE: &str = ".maw-epoch";

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors from the plain-copy workspace backend.
#[derive(Debug)]
pub enum CopyBackendError {
    /// An I/O error occurred.
    Io(std::io::Error),
    /// An external command (`git archive`, `tar`) failed.
    Command {
        command: String,
        stderr: String,
        exit_code: Option<i32>,
    },
    /// Workspace not found.
    NotFound { name: String },
    /// The workspace is missing the `.maw-epoch` metadata file.
    MissingEpochFile { workspace: String },
    /// The epoch ID stored in `.maw-epoch` is malformed.
    InvalidEpochFile { workspace: String, reason: String },
}

impl fmt::Display for CopyBackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Command {
                command,
                stderr,
                exit_code,
            } => {
                write!(f, "`{command}` failed")?;
                if let Some(code) = exit_code {
                    write!(f, " (exit code {code})")?;
                }
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            Self::NotFound { name } => write!(f, "workspace '{name}' not found"),
            Self::MissingEpochFile { workspace } => {
                write!(
                    f,
                    "workspace '{workspace}' is missing {EPOCH_FILE}; \
                     the workspace may be corrupted"
                )
            }
            Self::InvalidEpochFile { workspace, reason } => {
                write!(
                    f,
                    "workspace '{workspace}' has an invalid {EPOCH_FILE}: {reason}"
                )
            }
        }
    }
}

impl std::error::Error for CopyBackendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for CopyBackendError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ---------------------------------------------------------------------------
// CopyBackend
// ---------------------------------------------------------------------------

/// A workspace backend that extracts epoch trees via `git archive`.
///
/// Each workspace is a plain copy of the repository tree at the base epoch.
/// Changes to the workspace are detected by walking the directory and
/// comparing against the base epoch via `git diff`.
///
/// # Thread safety
///
/// `CopyBackend` is `Send + Sync`. All state lives on the filesystem.
pub struct CopyBackend {
    /// Absolute path to the repository root (contains `.git/`, `ws/`).
    root: PathBuf,
}

impl CopyBackend {
    /// Create a new `CopyBackend` rooted at `root`.
    #[must_use]
    pub const fn new(root: PathBuf) -> Self {
        Self { root }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn workspaces_dir(&self) -> PathBuf {
        self.root.join("ws")
    }

    fn read_epoch_file(ws_path: &Path, name: &str) -> Result<EpochId, CopyBackendError> {
        let epoch_file = ws_path.join(EPOCH_FILE);
        if !epoch_file.exists() {
            return Err(CopyBackendError::MissingEpochFile {
                workspace: name.to_owned(),
            });
        }
        let raw = std::fs::read_to_string(&epoch_file)?;
        let oid_str = raw.trim();
        EpochId::new(oid_str).map_err(|e| CopyBackendError::InvalidEpochFile {
            workspace: name.to_owned(),
            reason: e.to_string(),
        })
    }

    fn write_epoch_file(ws_path: &Path, epoch: &EpochId) -> Result<(), CopyBackendError> {
        let epoch_file = ws_path.join(EPOCH_FILE);
        std::fs::write(&epoch_file, format!("{}\n", epoch.as_str()))?;
        Ok(())
    }

    /// Extract the epoch's tree into `dest` using `git archive | tar -x`.
    ///
    /// This creates a full copy of all tracked files at the epoch commit.
    fn extract_epoch(&self, epoch: &EpochId, dest: &Path) -> Result<(), CopyBackendError> {
        std::fs::create_dir_all(dest)?;

        // Run `git archive <oid> | tar -x -C <dest>`
        let mut archive = Command::new("git")
            .args(["archive", epoch.as_str()])
            .current_dir(&self.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(CopyBackendError::Io)?;

        let archive_stdout = archive.stdout.take().expect("piped stdout");

        let tar_status = Command::new("tar")
            .args(["-x", "-C"])
            .arg(dest)
            .stdin(archive_stdout)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .map_err(CopyBackendError::Io)?;

        let archive_output = archive.wait_with_output().map_err(CopyBackendError::Io)?;

        if !archive_output.status.success() {
            let stderr = String::from_utf8_lossy(&archive_output.stderr)
                .trim()
                .to_owned();
            return Err(CopyBackendError::Command {
                command: format!("git archive {}", epoch.as_str()),
                stderr,
                exit_code: archive_output.status.code(),
            });
        }

        if !tar_status.success() {
            return Err(CopyBackendError::Command {
                command: format!("tar -x -C {}", dest.display()),
                stderr: String::new(),
                exit_code: tar_status.code(),
            });
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// WorkspaceBackend impl
// ---------------------------------------------------------------------------

impl WorkspaceBackend for CopyBackend {
    type Error = CopyBackendError;

    fn create(&self, name: &WorkspaceId, epoch: &EpochId) -> Result<WorkspaceInfo, Self::Error> {
        let ws_path = self.workspace_path(name);

        // Idempotency: workspace with correct epoch already exists.
        if ws_path.exists() {
            if let Ok(existing_epoch) = Self::read_epoch_file(&ws_path, name.as_str())
                && existing_epoch == *epoch
            {
                return Ok(WorkspaceInfo {
                    id: name.clone(),
                    path: ws_path,
                    epoch: epoch.clone(),
                    state: WorkspaceState::Active,
                    mode: WorkspaceMode::default(),
                    commits_ahead: 0,
                });
            }
            // Partial or mismatched workspace — remove and recreate.
            std::fs::remove_dir_all(&ws_path)?;
        }

        std::fs::create_dir_all(self.workspaces_dir())?;

        // Extract the epoch tree into the workspace directory.
        self.extract_epoch(epoch, &ws_path)?;

        // Write the epoch marker.
        Self::write_epoch_file(&ws_path, epoch)?;

        Ok(WorkspaceInfo {
            id: name.clone(),
            path: ws_path,
            epoch: epoch.clone(),
            state: WorkspaceState::Active,
            mode: WorkspaceMode::default(),
            commits_ahead: 0,
        })
    }

    fn destroy(&self, name: &WorkspaceId) -> Result<(), Self::Error> {
        let ws_path = self.workspace_path(name);
        if !ws_path.exists() {
            return Ok(()); // idempotent
        }
        std::fs::remove_dir_all(&ws_path)?;
        Ok(())
    }

    fn list(&self) -> Result<Vec<WorkspaceInfo>, Self::Error> {
        let ws_dir = self.workspaces_dir();
        if !ws_dir.exists() {
            return Ok(vec![]);
        }

        let mut workspaces = Vec::new();
        for entry in std::fs::read_dir(&ws_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name_str = entry.file_name().to_string_lossy().into_owned();
            let Ok(ws_id) = WorkspaceId::new(&name_str) else {
                continue;
            };
            let Ok(epoch) = Self::read_epoch_file(&path, &name_str) else {
                continue; // skip corrupted entries
            };
            workspaces.push(WorkspaceInfo {
                id: ws_id,
                path,
                epoch,
                state: WorkspaceState::Active,
                mode: WorkspaceMode::default(),
                commits_ahead: 0,
            });
        }
        Ok(workspaces)
    }

    fn status(&self, name: &WorkspaceId) -> Result<WorkspaceStatus, Self::Error> {
        let ws_path = self.workspace_path(name);
        if !ws_path.exists() {
            return Err(CopyBackendError::NotFound {
                name: name.as_str().to_owned(),
            });
        }

        let base_epoch = Self::read_epoch_file(&ws_path, name.as_str())?;

        // Use `git diff --name-only` to find modified/added/deleted files.
        let output = Command::new("git")
            .args([
                "diff",
                "--name-only",
                base_epoch.as_str(),
                "--",
                ws_path.to_str().unwrap_or(""),
            ])
            .current_dir(&self.root)
            .output()
            .map_err(CopyBackendError::Io)?;

        let dirty_files: Vec<PathBuf> = if output.status.success() {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(PathBuf::from)
                .collect()
        } else {
            vec![]
        };

        // Determine staleness: check if current epoch ref is ahead of base epoch.
        let is_stale = self.check_stale(&base_epoch);

        Ok(WorkspaceStatus::new(base_epoch, dirty_files, is_stale))
    }

    fn snapshot(&self, name: &WorkspaceId) -> Result<SnapshotResult, Self::Error> {
        let ws_path = self.workspace_path(name);
        if !ws_path.exists() {
            return Err(CopyBackendError::NotFound {
                name: name.as_str().to_owned(),
            });
        }

        let base_epoch = Self::read_epoch_file(&ws_path, name.as_str())?;

        // Walk the workspace and compare against the base epoch tree.
        let tracked = self.tracked_files_at_epoch(&base_epoch);
        let workspace_files = Self::walk_workspace(&ws_path);

        let is_excluded = |name: &str| name == EPOCH_FILE;

        let mut added = Vec::new();
        let mut modified = Vec::new();
        let mut deleted = Vec::new();

        // Check for modified and deleted tracked files.
        for rel_path in &tracked {
            let rel = std::path::Path::new(rel_path);
            let name_str = rel_path.as_str();
            if is_excluded(name_str) {
                continue;
            }
            let abs = ws_path.join(rel);
            if !abs.exists() {
                deleted.push(rel.to_path_buf());
            } else if self.file_differs_from_epoch(rel, &base_epoch) {
                modified.push(rel.to_path_buf());
            }
        }

        // Check for untracked (added) files.
        let tracked_set: HashSet<&str> = tracked.iter().map(std::string::String::as_str).collect();
        for ws_rel in &workspace_files {
            let ws_rel_str = ws_rel.to_string_lossy();
            if is_excluded(ws_rel_str.as_ref()) || tracked_set.contains(ws_rel_str.as_ref()) {
                continue;
            }
            added.push(ws_rel.clone());
        }

        Ok(SnapshotResult::new(added, modified, deleted))
    }

    fn workspace_path(&self, name: &WorkspaceId) -> PathBuf {
        self.workspaces_dir().join(name.as_str())
    }

    fn exists(&self, name: &WorkspaceId) -> bool {
        self.workspace_path(name).is_dir()
    }
}

impl CopyBackend {
    /// Check if the workspace's base epoch is behind the current epoch ref.
    fn check_stale(&self, base_epoch: &EpochId) -> bool {
        let output = Command::new("git")
            .args(["rev-parse", "refs/manifold/epoch/current"])
            .current_dir(&self.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();

        if let Ok(out) = output
            && out.status.success()
        {
            let current = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            return current != base_epoch.as_str();
        }
        false
    }

    /// List all files tracked at the given epoch via `git ls-tree`.
    fn tracked_files_at_epoch(&self, epoch: &EpochId) -> Vec<String> {
        let output = Command::new("git")
            .args(["ls-tree", "-r", "--name-only", epoch.as_str()])
            .current_dir(&self.root)
            .output();

        match output {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect(),
            _ => vec![],
        }
    }

    /// Walk the workspace directory, returning paths relative to `ws_path`.
    fn walk_workspace(ws_path: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        Self::walk_dir(ws_path, ws_path, &mut files);
        files
    }

    fn walk_dir(base: &Path, current: &Path, files: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(current) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(meta) = entry.metadata() {
                if meta.is_dir() {
                    Self::walk_dir(base, &path, files);
                } else if let Ok(rel) = path.strip_prefix(base) {
                    files.push(rel.to_path_buf());
                }
            }
        }
    }

    /// Check if a tracked file in the workspace differs from the epoch version.
    fn file_differs_from_epoch(&self, rel: &Path, epoch: &EpochId) -> bool {
        let blob_path = format!("{}:{}", epoch.as_str(), rel.display());
        let output = Command::new("git")
            .args(["cat-file", "blob", &blob_path])
            .current_dir(&self.root)
            .output();

        let Ok(out) = output else { return false };
        if !out.status.success() {
            return false;
        }
        out.stdout != std::fs::read(rel).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_backend_error_display() {
        let err = CopyBackendError::NotFound {
            name: "alice".to_owned(),
        };
        assert_eq!(format!("{err}"), "workspace 'alice' not found");

        let err = CopyBackendError::MissingEpochFile {
            workspace: "bob".to_owned(),
        };
        assert!(format!("{err}").contains("bob"));
        assert!(format!("{err}").contains(EPOCH_FILE));

        let err = CopyBackendError::Io(std::io::Error::other("disk full"));
        assert!(format!("{err}").contains("disk full"));
    }

    #[test]
    fn copy_backend_new() {
        let backend = CopyBackend::new(PathBuf::from("/tmp/repo"));
        assert_eq!(backend.root, PathBuf::from("/tmp/repo"));
        assert_eq!(
            backend.workspaces_dir(),
            PathBuf::from("/tmp/repo").join("ws")
        );
    }
}
