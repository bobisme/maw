//! Reflink (copy-on-write) workspace backend.
//!
//! Implements [`WorkspaceBackend`] using `cp --reflink=auto` to create
//! workspaces from immutable epoch snapshot directories. On Btrfs, XFS, and
//! APFS (with the appropriate `cp` from GNU coreutils or macOS), the copy is
//! nearly instant because disk blocks are shared until modified.
//!
//! # Directory layout
//!
//! ```text
//! repo-root/
//! ├── .manifold/
//! │   └── epochs/
//! │       └── e-{hash}/   ← immutable epoch snapshot (source for reflinks)
//! └── ws/
//!     └── <name>/         ← workspace (reflink copy of epoch snapshot)
//!         └── .maw-epoch  ← stores the base epoch OID (40 hex chars + newline)
//! ```
//!
//! # Fallback behaviour
//!
//! If `cp --reflink=always` fails (non-CoW filesystem), `create` retries with
//! `cp --reflink=auto` which silently falls back to a regular copy. This means
//! the backend works on any filesystem — it just isn't instant on non-CoW ones.

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

/// Errors from the reflink workspace backend.
#[derive(Debug)]
pub enum ReflinkBackendError {
    /// An I/O error occurred.
    Io(std::io::Error),
    /// A subprocess (e.g. `cp`) failed.
    Command {
        command: String,
        stderr: String,
        exit_code: Option<i32>,
    },
    /// Workspace not found.
    NotFound { name: String },
    /// The epoch snapshot directory does not exist.
    EpochSnapshotMissing { epoch: String },
    /// The workspace is missing the `.maw-epoch` metadata file.
    MissingEpochFile { workspace: String },
    /// The epoch ID stored in `.maw-epoch` is malformed.
    InvalidEpochFile { workspace: String, reason: String },
}

impl fmt::Display for ReflinkBackendError {
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
            Self::EpochSnapshotMissing { epoch } => {
                write!(
                    f,
                    "epoch snapshot .manifold/epochs/e-{epoch}/ not found; \
                     run `maw epoch snapshot` to create it"
                )
            }
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

impl std::error::Error for ReflinkBackendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ReflinkBackendError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ---------------------------------------------------------------------------
// RefLinkBackend
// ---------------------------------------------------------------------------

/// A workspace backend that uses reflink (CoW) copies of epoch snapshots.
///
/// Each workspace is a `cp --reflink=auto` copy of the immutable epoch
/// snapshot directory located at `.manifold/epochs/e-{epoch_hash}/`.
///
/// # Thread safety
///
/// `RefLinkBackend` is `Send + Sync`. All state is derived from the
/// filesystem; no interior mutability is used.
pub struct RefLinkBackend {
    /// Absolute path to the repository root (contains `.git/`, `ws/`, `.manifold/`).
    root: PathBuf,
}

impl RefLinkBackend {
    /// Create a new `RefLinkBackend` rooted at `root`.
    ///
    /// `root` must be the repository root — the directory that contains `.git/`
    /// and `.manifold/`.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// `ws/` directory under the repo root.
    fn workspaces_dir(&self) -> PathBuf {
        self.root.join("ws")
    }

    /// Path to the epoch snapshot directory for a given epoch.
    ///
    /// e.g. `/repo/.manifold/epochs/e-abc123.../`
    fn epoch_snapshot_path(&self, epoch: &EpochId) -> PathBuf {
        self.root
            .join(".manifold")
            .join("epochs")
            .join(format!("e-{}", epoch.as_str()))
    }

    /// Read the base epoch from a workspace's `.maw-epoch` file.
    fn read_epoch_file(&self, ws_path: &Path, name: &str) -> Result<EpochId, ReflinkBackendError> {
        let epoch_file = ws_path.join(EPOCH_FILE);
        if !epoch_file.exists() {
            return Err(ReflinkBackendError::MissingEpochFile {
                workspace: name.to_owned(),
            });
        }
        let raw = std::fs::read_to_string(&epoch_file)?;
        let oid_str = raw.trim();
        EpochId::new(oid_str).map_err(|e| ReflinkBackendError::InvalidEpochFile {
            workspace: name.to_owned(),
            reason: e.to_string(),
        })
    }

    /// Write the base epoch to a workspace's `.maw-epoch` file.
    fn write_epoch_file(&self, ws_path: &Path, epoch: &EpochId) -> Result<(), ReflinkBackendError> {
        let epoch_file = ws_path.join(EPOCH_FILE);
        let content = format!("{}\n", epoch.as_str());
        std::fs::write(&epoch_file, content)?;
        Ok(())
    }

    /// Read `refs/manifold/epoch/current` from git.
    ///
    /// Returns `None` if the ref does not exist (Manifold not yet initialized).
    fn current_epoch_opt(&self) -> Option<EpochId> {
        let output = Command::new("git")
            .args(["rev-parse", "refs/manifold/epoch/current"])
            .current_dir(&self.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if output.status.success() {
            let oid_str = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            EpochId::new(&oid_str).ok()
        } else {
            None
        }
    }

    /// Copy `src` into `dst` using `cp --reflink=auto -r`.
    ///
    /// On CoW filesystems (Btrfs, XFS, APFS) this is nearly instant.
    /// Falls back silently to a regular copy on non-CoW filesystems.
    ///
    /// `src` must be an existing directory. `dst` must not already exist.
    fn reflink_copy(&self, src: &Path, dst: &Path) -> Result<(), ReflinkBackendError> {
        // First try --reflink=auto (most portable: GNU coreutils / macOS `cp`)
        let output = Command::new("cp")
            .args(["-r", "--reflink=auto"])
            .arg(src)
            .arg(dst)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output();

        match output {
            Ok(o) if o.status.success() => return Ok(()),
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr).trim().to_owned();
                // If --reflink=auto fails (e.g., option not recognised on some
                // systems), fall through to the portable recursive copy below.
                if !stderr.contains("invalid option") && !stderr.contains("unrecognized option") {
                    return Err(ReflinkBackendError::Command {
                        command: format!("cp -r --reflink=auto {} {}", src.display(), dst.display()),
                        stderr,
                        exit_code: o.status.code(),
                    });
                }
            }
            Err(_) => {} // cp not found — fall through to Rust fs copy
        }

        // Fallback: portable recursive copy via Rust std::fs
        self.recursive_copy(src, dst)
    }

    /// Portable recursive directory copy using `std::fs`.
    fn recursive_copy(&self, src: &Path, dst: &Path) -> Result<(), ReflinkBackendError> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                self.recursive_copy(&src_path, &dst_path)?;
            } else if metadata.is_symlink() {
                let target = std::fs::read_link(&src_path)?;
                #[cfg(unix)]
                std::os::unix::fs::symlink(&target, &dst_path)?;
                #[cfg(not(unix))]
                {
                    // Best-effort on non-Unix: copy the file instead
                    std::fs::copy(&src_path, &dst_path)?;
                }
            } else {
                std::fs::copy(&src_path, &dst_path)?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// WorkspaceBackend impl
// ---------------------------------------------------------------------------

impl WorkspaceBackend for RefLinkBackend {
    type Error = ReflinkBackendError;

    /// Create a workspace by reflinking the epoch snapshot.
    ///
    /// Steps:
    /// 1. Verify the epoch snapshot exists at `.manifold/epochs/e-{hash}/`.
    /// 2. Copy it to `ws/<name>/` using `cp --reflink=auto -r`.
    /// 3. Write the epoch OID to `ws/<name>/.maw-epoch`.
    ///
    /// If a valid workspace already exists (idempotency), returns its info.
    fn create(&self, name: &WorkspaceId, epoch: &EpochId) -> Result<WorkspaceInfo, Self::Error> {
        let ws_path = self.workspace_path(name);

        // Idempotency: if workspace already exists with correct epoch, return it.
        if ws_path.exists() {
            if let Ok(existing_epoch) = self.read_epoch_file(&ws_path, name.as_str()) {
                if existing_epoch == *epoch {
                    return Ok(WorkspaceInfo {
                        id: name.clone(),
                        path: ws_path,
                        epoch: epoch.clone(),
                        state: WorkspaceState::Active,
                        mode: WorkspaceMode::default(),
                    });
                }
            }
            // Partial/mismatched workspace: remove and recreate.
            std::fs::remove_dir_all(&ws_path)?;
        }

        // Verify the epoch snapshot exists.
        let snapshot_path = self.epoch_snapshot_path(epoch);
        if !snapshot_path.exists() {
            return Err(ReflinkBackendError::EpochSnapshotMissing {
                epoch: epoch.as_str().to_owned(),
            });
        }

        // Ensure the ws/ parent directory exists.
        let ws_dir = self.workspaces_dir();
        std::fs::create_dir_all(&ws_dir)?;

        // Reflink-copy the snapshot into the workspace directory.
        self.reflink_copy(&snapshot_path, &ws_path)?;

        // Write the base epoch identifier into the workspace.
        self.write_epoch_file(&ws_path, epoch)?;

        Ok(WorkspaceInfo {
            id: name.clone(),
            path: ws_path,
            epoch: epoch.clone(),
            state: WorkspaceState::Active,
            mode: WorkspaceMode::default(),
        })
    }

    /// Destroy a workspace by removing its directory.
    ///
    /// Idempotent: destroying a non-existent workspace is a no-op.
    fn destroy(&self, name: &WorkspaceId) -> Result<(), Self::Error> {
        let ws_path = self.workspace_path(name);
        if ws_path.exists() {
            std::fs::remove_dir_all(&ws_path)?;
        }
        Ok(())
    }

    /// List all active workspaces by scanning `ws/`.
    ///
    /// A directory under `ws/` is considered a workspace if:
    /// - Its name is a valid `WorkspaceId`.
    /// - It contains a readable `.maw-epoch` file with a valid epoch OID.
    fn list(&self) -> Result<Vec<WorkspaceInfo>, Self::Error> {
        let ws_dir = self.workspaces_dir();
        if !ws_dir.exists() {
            return Ok(vec![]);
        }

        let current_epoch = self.current_epoch_opt();
        let mut infos = Vec::new();

        for entry in std::fs::read_dir(&ws_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name_str = match path.file_name().and_then(|n| n.to_str()) {
                Some(s) => s.to_owned(),
                None => continue,
            };
            let id = match WorkspaceId::new(&name_str) {
                Ok(id) => id,
                Err(_) => continue, // Skip directories with non-conforming names
            };

            let epoch = match self.read_epoch_file(&path, &name_str) {
                Ok(e) => e,
                Err(_) => continue, // Not a valid workspace (no metadata file)
            };

            let state = match &current_epoch {
                Some(current) if epoch == *current => WorkspaceState::Active,
                Some(_) => WorkspaceState::Stale { behind_epochs: 1 },
                None => WorkspaceState::Active,
            };

            infos.push(WorkspaceInfo {
                id,
                path,
                epoch,
                state,
                mode: WorkspaceMode::default(),
            });
        }

        Ok(infos)
    }

    /// Get the current status of a workspace.
    ///
    /// Reports the base epoch, dirty files (by comparing workspace against the
    /// epoch snapshot), and whether the workspace is stale.
    fn status(&self, name: &WorkspaceId) -> Result<WorkspaceStatus, Self::Error> {
        let ws_path = self.workspace_path(name);
        if !ws_path.exists() {
            return Err(ReflinkBackendError::NotFound {
                name: name.as_str().to_owned(),
            });
        }

        let base_epoch = self.read_epoch_file(&ws_path, name.as_str())?;
        let snapshot_path = self.epoch_snapshot_path(&base_epoch);

        let snap = diff_dirs(&snapshot_path, &ws_path);
        let mut dirty_files: Vec<PathBuf> = snap
            .added
            .iter()
            .chain(snap.modified.iter())
            .chain(snap.deleted.iter())
            .cloned()
            .collect();
        dirty_files.sort();
        dirty_files.dedup();

        let is_stale = self
            .current_epoch_opt()
            .map(|current| base_epoch != current)
            .unwrap_or(false);

        Ok(WorkspaceStatus::new(base_epoch, dirty_files, is_stale))
    }

    /// Snapshot (diff) the workspace against its base epoch snapshot.
    ///
    /// Compares the workspace directory tree against the immutable epoch
    /// snapshot at `.manifold/epochs/e-{hash}/`.
    ///
    /// Files excluded from comparison:
    /// - `.maw-epoch` (backend metadata)
    fn snapshot(&self, name: &WorkspaceId) -> Result<SnapshotResult, Self::Error> {
        let ws_path = self.workspace_path(name);
        if !ws_path.exists() {
            return Err(ReflinkBackendError::NotFound {
                name: name.as_str().to_owned(),
            });
        }

        let base_epoch = self.read_epoch_file(&ws_path, name.as_str())?;
        let snapshot_path = self.epoch_snapshot_path(&base_epoch);

        Ok(diff_dirs(&snapshot_path, &ws_path))
    }

    fn workspace_path(&self, name: &WorkspaceId) -> PathBuf {
        self.workspaces_dir().join(name.as_str())
    }

    fn exists(&self, name: &WorkspaceId) -> bool {
        let ws_path = self.workspace_path(name);
        ws_path.is_dir() && ws_path.join(EPOCH_FILE).exists()
    }
}

// ---------------------------------------------------------------------------
// Directory diff
// ---------------------------------------------------------------------------

/// Names excluded from workspace snapshots.
///
/// These are backend-internal metadata files that should never appear in the
/// diff output, even though they live inside the workspace directory.
const EXCLUDED_NAMES: &[&str] = &[EPOCH_FILE];

/// Diff two directory trees.
///
/// `base_dir` is the immutable epoch snapshot. `ws_dir` is the workspace.
/// Returns a `SnapshotResult` with paths relative to `ws_dir`.
///
/// If `base_dir` does not exist (epoch snapshot missing or not yet created),
/// all files in `ws_dir` are treated as additions.
fn diff_dirs(base_dir: &Path, ws_dir: &Path) -> SnapshotResult {
    // Collect all files in the base snapshot (relative paths).
    let base_files: HashSet<PathBuf> = if base_dir.exists() {
        collect_files(base_dir, &[])
            .into_iter()
            .collect()
    } else {
        HashSet::new()
    };

    // Collect all files in the workspace (relative paths), excluding metadata.
    let ws_files: HashSet<PathBuf> = collect_files(ws_dir, EXCLUDED_NAMES)
        .into_iter()
        .collect();

    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut deleted = Vec::new();

    // Files in workspace: added or modified
    for rel in &ws_files {
        if !base_files.contains(rel) {
            added.push(rel.clone());
        } else {
            // Both exist — compare content
            let base_file = base_dir.join(rel);
            let ws_file = ws_dir.join(rel);
            if !files_equal(&base_file, &ws_file) {
                modified.push(rel.clone());
            }
        }
    }

    // Files in base but not workspace: deleted
    for rel in &base_files {
        if !ws_files.contains(rel) {
            deleted.push(rel.clone());
        }
    }

    added.sort();
    modified.sort();
    deleted.sort();

    SnapshotResult::new(added, modified, deleted)
}

/// Recursively collect relative file paths under `root`.
///
/// Skips directories and any top-level entries whose names appear in
/// `exclude_names`.
fn collect_files(root: &Path, exclude_names: &[&str]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_files_inner(root, root, exclude_names, &mut files);
    files
}

fn collect_files_inner(
    root: &Path,
    dir: &Path,
    exclude_names: &[&str],
    files: &mut Vec<PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip excluded entries (checked against unqualified filename only)
        if exclude_names.iter().any(|e| *e == name_str.as_ref()) {
            continue;
        }

        if path.is_dir() {
            collect_files_inner(root, &path, exclude_names, files);
        } else if path.is_file() {
            // Relative path from root
            if let Ok(rel) = path.strip_prefix(root) {
                files.push(rel.to_path_buf());
            }
        }
        // Symlinks: count as files (strip_prefix will work because is_file()
        // follows symlinks). If the symlink target is a directory, is_dir()
        // returns true and we recurse. This matches common expectations.
    }
}

/// Return `true` if both files have identical byte content.
///
/// Returns `false` on any I/O error (treat as different).
fn files_equal(a: &Path, b: &Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(a_bytes), Ok(b_bytes)) => a_bytes == b_bytes,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Helper: set up a temporary directory with a fake epoch snapshot.
    ///
    /// Returns the temp dir, the repo root path, and the epoch ID.
    fn setup_repo_with_snapshot() -> (TempDir, PathBuf, EpochId) {
        let temp = TempDir::new().unwrap();
        let root = temp.path().to_path_buf();

        // Fake 40-char hex epoch OID
        let epoch_oid = "a".repeat(40);
        let epoch = EpochId::new(&epoch_oid).unwrap();

        // Create epoch snapshot directory with some files
        let snap_dir = root
            .join(".manifold")
            .join("epochs")
            .join(format!("e-{epoch_oid}"));
        fs::create_dir_all(&snap_dir).unwrap();
        fs::write(snap_dir.join("README.md"), "# Epoch snapshot").unwrap();
        fs::write(snap_dir.join("main.rs"), "fn main() {}").unwrap();
        fs::create_dir_all(snap_dir.join("src")).unwrap();
        fs::write(snap_dir.join("src").join("lib.rs"), "pub fn lib() {}").unwrap();

        (temp, root, epoch)
    }

    // -- create tests --

    #[test]
    fn test_create_workspace() {
        let (_temp, root, epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("test-ws").unwrap();

        let info = backend.create(&ws_name, &epoch).unwrap();
        assert_eq!(info.id, ws_name);
        assert_eq!(info.path, root.join("ws").join("test-ws"));
        assert!(info.path.exists());
        // Workspace contains snapshot files
        assert!(info.path.join("README.md").exists());
        assert!(info.path.join("main.rs").exists());
        assert!(info.path.join("src").join("lib.rs").exists());
        // Epoch file written
        assert!(info.path.join(EPOCH_FILE).exists());
    }

    #[test]
    fn test_create_idempotent() {
        let (_temp, root, epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("idem-ws").unwrap();

        let info1 = backend.create(&ws_name, &epoch).unwrap();
        let info2 = backend.create(&ws_name, &epoch).unwrap();
        assert_eq!(info1.path, info2.path);
        assert_eq!(info1.epoch, info2.epoch);
    }

    #[test]
    fn test_create_replaces_mismatched_workspace() {
        let (_temp, root, epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("replace-ws").unwrap();

        // Create workspace with wrong epoch file
        let ws_path = root.join("ws").join("replace-ws");
        fs::create_dir_all(&ws_path).unwrap();
        fs::write(ws_path.join(EPOCH_FILE), "b".repeat(40) + "\n").unwrap();
        fs::write(ws_path.join("stale.txt"), "stale content").unwrap();

        // Create should replace with correct epoch
        let info = backend.create(&ws_name, &epoch).unwrap();
        assert_eq!(info.epoch, epoch);
        // Old content removed
        assert!(!ws_path.join("stale.txt").exists());
        // Snapshot content present
        assert!(ws_path.join("README.md").exists());
    }

    #[test]
    fn test_create_missing_epoch_snapshot() {
        let (_temp, root, _epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("no-snap-ws").unwrap();

        // Use an epoch that has no snapshot
        let missing_epoch = EpochId::new(&"f".repeat(40)).unwrap();
        let err = backend.create(&ws_name, &missing_epoch).unwrap_err();
        assert!(
            matches!(err, ReflinkBackendError::EpochSnapshotMissing { .. }),
            "expected EpochSnapshotMissing: {err}"
        );
    }

    // -- exists tests --

    #[test]
    fn test_exists_false_for_nonexistent() {
        let (_temp, root, _epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        assert!(!backend.exists(&WorkspaceId::new("nope").unwrap()));
    }

    #[test]
    fn test_exists_true_after_create() {
        let (_temp, root, epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("exists-ws").unwrap();

        backend.create(&ws_name, &epoch).unwrap();
        assert!(backend.exists(&ws_name));
    }

    #[test]
    fn test_exists_false_for_dir_without_epoch_file() {
        let (_temp, root, _epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_path = root.join("ws").join("incomplete");
        fs::create_dir_all(&ws_path).unwrap();
        // No .maw-epoch file → not a valid workspace

        let ws_name = WorkspaceId::new("incomplete").unwrap();
        assert!(!backend.exists(&ws_name));
    }

    // -- destroy tests --

    #[test]
    fn test_destroy_workspace() {
        let (_temp, root, epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("destroy-ws").unwrap();

        let info = backend.create(&ws_name, &epoch).unwrap();
        assert!(info.path.exists());

        backend.destroy(&ws_name).unwrap();
        assert!(!info.path.exists());
        assert!(!backend.exists(&ws_name));
    }

    #[test]
    fn test_destroy_idempotent() {
        let (_temp, root, epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("idem-destroy").unwrap();

        backend.create(&ws_name, &epoch).unwrap();
        backend.destroy(&ws_name).unwrap();
        backend.destroy(&ws_name).unwrap(); // second call is a no-op
    }

    #[test]
    fn test_destroy_never_existed() {
        let (_temp, root, _epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("no-such-ws").unwrap();
        backend.destroy(&ws_name).unwrap(); // should not error
    }

    #[test]
    fn test_create_after_destroy() {
        let (_temp, root, epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("recreate-ws").unwrap();

        backend.create(&ws_name, &epoch).unwrap();
        backend.destroy(&ws_name).unwrap();
        let info = backend.create(&ws_name, &epoch).unwrap();
        assert!(info.path.exists());
        assert!(backend.exists(&ws_name));
    }

    // -- list tests --

    #[test]
    fn test_list_empty_no_workspaces() {
        let (_temp, root, _epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let infos = backend.list().unwrap();
        assert!(infos.is_empty());
    }

    #[test]
    fn test_list_single_workspace() {
        let (_temp, root, epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("list-ws").unwrap();

        backend.create(&ws_name, &epoch).unwrap();

        let infos = backend.list().unwrap();
        assert_eq!(infos.len(), 1, "expected 1: {infos:?}");
        assert_eq!(infos[0].id, ws_name);
        assert_eq!(infos[0].epoch, epoch);
        assert!(infos[0].state.is_active());
    }

    #[test]
    fn test_list_multiple_workspaces() {
        let (_temp, root, epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());

        let a = WorkspaceId::new("alpha").unwrap();
        let b = WorkspaceId::new("beta").unwrap();
        backend.create(&a, &epoch).unwrap();
        backend.create(&b, &epoch).unwrap();

        let mut infos = backend.list().unwrap();
        assert_eq!(infos.len(), 2, "expected 2: {infos:?}");
        infos.sort_by(|x, y| x.id.as_str().cmp(y.id.as_str()));
        assert_eq!(infos[0].id.as_str(), "alpha");
        assert_eq!(infos[1].id.as_str(), "beta");
    }

    #[test]
    fn test_list_excludes_destroyed_workspace() {
        let (_temp, root, epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("gone-ws").unwrap();

        backend.create(&ws_name, &epoch).unwrap();
        backend.destroy(&ws_name).unwrap();

        let infos = backend.list().unwrap();
        assert!(infos.is_empty(), "destroyed workspace must not appear: {infos:?}");
    }

    #[test]
    fn test_list_skips_non_workspace_dirs() {
        let (_temp, root, epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("real-ws").unwrap();

        backend.create(&ws_name, &epoch).unwrap();

        // Create a directory with no .maw-epoch (not a workspace)
        fs::create_dir_all(root.join("ws").join("not-a-ws")).unwrap();

        let infos = backend.list().unwrap();
        assert_eq!(infos.len(), 1, "should skip dirs without epoch file: {infos:?}");
        assert_eq!(infos[0].id, ws_name);
    }

    // -- snapshot tests --

    #[test]
    fn test_snapshot_empty_no_changes() {
        let (_temp, root, epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("snap-clean").unwrap();
        backend.create(&ws_name, &epoch).unwrap();

        let snap = backend.snapshot(&ws_name).unwrap();
        assert!(snap.is_empty(), "no changes expected: {snap:?}");
    }

    #[test]
    fn test_snapshot_added_file() {
        let (_temp, root, epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("snap-add").unwrap();
        let info = backend.create(&ws_name, &epoch).unwrap();

        fs::write(info.path.join("newfile.txt"), "hello").unwrap();

        let snap = backend.snapshot(&ws_name).unwrap();
        assert_eq!(snap.added.len(), 1, "expected 1 added: {snap:?}");
        assert_eq!(snap.added[0], PathBuf::from("newfile.txt"));
        assert!(snap.modified.is_empty());
        assert!(snap.deleted.is_empty());
    }

    #[test]
    fn test_snapshot_modified_file() {
        let (_temp, root, epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("snap-mod").unwrap();
        let info = backend.create(&ws_name, &epoch).unwrap();

        fs::write(info.path.join("README.md"), "# Modified").unwrap();

        let snap = backend.snapshot(&ws_name).unwrap();
        assert!(snap.added.is_empty(), "no adds: {snap:?}");
        assert_eq!(snap.modified.len(), 1, "expected 1 modified: {snap:?}");
        assert_eq!(snap.modified[0], PathBuf::from("README.md"));
        assert!(snap.deleted.is_empty());
    }

    #[test]
    fn test_snapshot_deleted_file() {
        let (_temp, root, epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("snap-del").unwrap();
        let info = backend.create(&ws_name, &epoch).unwrap();

        fs::remove_file(info.path.join("README.md")).unwrap();

        let snap = backend.snapshot(&ws_name).unwrap();
        assert!(snap.added.is_empty());
        assert!(snap.modified.is_empty());
        assert_eq!(snap.deleted.len(), 1, "expected 1 deleted: {snap:?}");
        assert_eq!(snap.deleted[0], PathBuf::from("README.md"));
    }

    #[test]
    fn test_snapshot_nested_file_modified() {
        let (_temp, root, epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("snap-nested").unwrap();
        let info = backend.create(&ws_name, &epoch).unwrap();

        fs::write(info.path.join("src").join("lib.rs"), "pub fn changed() {}").unwrap();

        let snap = backend.snapshot(&ws_name).unwrap();
        assert!(snap.added.is_empty());
        assert_eq!(snap.modified.len(), 1, "expected 1 modified: {snap:?}");
        assert_eq!(snap.modified[0], PathBuf::from("src/lib.rs"));
        assert!(snap.deleted.is_empty());
    }

    #[test]
    fn test_snapshot_epoch_file_excluded() {
        let (_temp, root, epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("snap-exclude").unwrap();
        backend.create(&ws_name, &epoch).unwrap();

        // The .maw-epoch file must not appear in the snapshot
        let snap = backend.snapshot(&ws_name).unwrap();
        let has_epoch_file = snap
            .added
            .iter()
            .chain(snap.modified.iter())
            .chain(snap.deleted.iter())
            .any(|p| p.file_name().map(|n| n == EPOCH_FILE).unwrap_or(false));
        assert!(!has_epoch_file, ".maw-epoch must be excluded: {snap:?}");
    }

    #[test]
    fn test_snapshot_nonexistent_workspace() {
        let (_temp, root, _epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("no-such").unwrap();

        let err = backend.snapshot(&ws_name).unwrap_err();
        assert!(
            matches!(err, ReflinkBackendError::NotFound { .. }),
            "expected NotFound: {err}"
        );
    }

    // -- status tests --

    #[test]
    fn test_status_clean_workspace() {
        let (_temp, root, epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("status-clean").unwrap();
        backend.create(&ws_name, &epoch).unwrap();

        let status = backend.status(&ws_name).unwrap();
        assert_eq!(status.base_epoch, epoch);
        assert!(status.is_clean(), "expected clean: {:?}", status.dirty_files);
        assert!(!status.is_stale);
    }

    #[test]
    fn test_status_modified_file() {
        let (_temp, root, epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("status-mod").unwrap();
        let info = backend.create(&ws_name, &epoch).unwrap();

        fs::write(info.path.join("README.md"), "# Modified").unwrap();

        let status = backend.status(&ws_name).unwrap();
        assert_eq!(status.dirty_count(), 1);
        assert!(
            status.dirty_files.iter().any(|p| p == &PathBuf::from("README.md")),
            "expected README.md dirty: {:?}",
            status.dirty_files
        );
    }

    #[test]
    fn test_status_nonexistent_workspace() {
        let (_temp, root, _epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("no-such").unwrap();

        let err = backend.status(&ws_name).unwrap_err();
        assert!(
            matches!(err, ReflinkBackendError::NotFound { .. }),
            "expected NotFound: {err}"
        );
    }

    // -- workspace_path tests --

    #[test]
    fn test_workspace_path() {
        let (_temp, root, _epoch) = setup_repo_with_snapshot();
        let backend = RefLinkBackend::new(root.clone());
        let ws_name = WorkspaceId::new("path-test").unwrap();
        assert_eq!(backend.workspace_path(&ws_name), root.join("ws/path-test"));
    }

    // -- diff_dirs tests --

    #[test]
    fn test_diff_dirs_identical() {
        let temp = TempDir::new().unwrap();
        let base = temp.path().join("base");
        let ws = temp.path().join("ws");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&ws).unwrap();
        fs::write(base.join("file.txt"), "hello").unwrap();
        fs::write(ws.join("file.txt"), "hello").unwrap();

        let snap = diff_dirs(&base, &ws);
        assert!(snap.is_empty());
    }

    #[test]
    fn test_diff_dirs_added() {
        let temp = TempDir::new().unwrap();
        let base = temp.path().join("base");
        let ws = temp.path().join("ws");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&ws).unwrap();
        fs::write(ws.join("new.txt"), "new").unwrap();

        let snap = diff_dirs(&base, &ws);
        assert_eq!(snap.added, vec![PathBuf::from("new.txt")]);
        assert!(snap.modified.is_empty());
        assert!(snap.deleted.is_empty());
    }

    #[test]
    fn test_diff_dirs_modified() {
        let temp = TempDir::new().unwrap();
        let base = temp.path().join("base");
        let ws = temp.path().join("ws");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&ws).unwrap();
        fs::write(base.join("file.txt"), "original").unwrap();
        fs::write(ws.join("file.txt"), "changed").unwrap();

        let snap = diff_dirs(&base, &ws);
        assert!(snap.added.is_empty());
        assert_eq!(snap.modified, vec![PathBuf::from("file.txt")]);
        assert!(snap.deleted.is_empty());
    }

    #[test]
    fn test_diff_dirs_deleted() {
        let temp = TempDir::new().unwrap();
        let base = temp.path().join("base");
        let ws = temp.path().join("ws");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&ws).unwrap();
        fs::write(base.join("old.txt"), "old").unwrap();

        let snap = diff_dirs(&base, &ws);
        assert!(snap.added.is_empty());
        assert!(snap.modified.is_empty());
        assert_eq!(snap.deleted, vec![PathBuf::from("old.txt")]);
    }

    #[test]
    fn test_diff_dirs_missing_base() {
        // If the epoch snapshot doesn't exist, all workspace files are "added"
        let temp = TempDir::new().unwrap();
        let base = temp.path().join("nonexistent-base");
        let ws = temp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        fs::write(ws.join("file.txt"), "hello").unwrap();

        let snap = diff_dirs(&base, &ws);
        assert_eq!(snap.added, vec![PathBuf::from("file.txt")]);
        assert!(snap.modified.is_empty());
        assert!(snap.deleted.is_empty());
    }

    #[test]
    fn test_diff_dirs_excludes_epoch_file() {
        let temp = TempDir::new().unwrap();
        let base = temp.path().join("base");
        let ws = temp.path().join("ws");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&ws).unwrap();
        // .maw-epoch only in ws (as it would be after create)
        fs::write(ws.join(EPOCH_FILE), "a".repeat(40) + "\n").unwrap();

        let snap = diff_dirs(&base, &ws);
        // .maw-epoch is excluded, so snap should be empty
        assert!(
            snap.is_empty(),
            ".maw-epoch must be excluded from diff: {snap:?}"
        );
    }

    #[test]
    fn test_recursive_copy_fallback() {
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("src");
        let dst = temp.path().join("dst");
        fs::create_dir_all(src.join("subdir")).unwrap();
        fs::write(src.join("file.txt"), "hello").unwrap();
        fs::write(src.join("subdir").join("nested.txt"), "nested").unwrap();

        let backend = RefLinkBackend::new(temp.path().to_path_buf());
        backend.recursive_copy(&src, &dst).unwrap();

        assert!(dst.join("file.txt").exists());
        assert!(dst.join("subdir").join("nested.txt").exists());
        assert_eq!(fs::read_to_string(dst.join("file.txt")).unwrap(), "hello");
        assert_eq!(
            fs::read_to_string(dst.join("subdir").join("nested.txt")).unwrap(),
            "nested"
        );
    }
}
