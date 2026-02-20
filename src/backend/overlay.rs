//! `OverlayFS` workspace backend (Linux only).
//!
//! Provides zero-copy workspace isolation using Linux overlayfs (via
//! `fuse-overlayfs` or kernel overlayfs in user namespaces). Each workspace is
//! an overlay mount with three layers:
//!
//! - **lowerdir**: `.manifold/epochs/e-{hash}/` — immutable epoch snapshot
//! - **upperdir**: `.manifold/cow/<name>/upper/` — per-workspace changes
//! - **workdir**:  `.manifold/cow/<name>/work/`  — overlayfs bookkeeping
//! - **merged**:   `ws/<name>/` — the workspace working copy (mount point)
//!
//! The lowerdir is **always** an immutable epoch snapshot — never the mutable
//! default workspace. This satisfies the §4.4 invariant: overlay mounts never
//! become semantically stale as the epoch ref advances.
//!
//! # Platform requirements
//! - Linux only (`OverlayBackendError::NotLinux` on other platforms).
//! - Either `fuse-overlayfs` (preferred: user-space, persistent) **or** kernel
//!   overlayfs >= 5.11 via `unshare -Ur` (persists within the calling process).
//! - No root required.
//!
//! # Mount persistence
//! `fuse-overlayfs` spawns a background FUSE daemon that persists across tool
//! calls, making it the preferred implementation. Kernel overlay via `unshare`
//! is tied to the calling process namespace and is therefore only suitable for
//! single-process sessions (or use with a persistent daemon).
//!
//! The backend auto-remounts any workspace whose mount point is not active
//! at the start of `status()` and `snapshot()` calls, ensuring that `maw exec`
//! invocations always see a live overlay.
//!
//! # Epoch snapshot lifecycle
//! Snapshots are created via `git archive | tar -x` on first use and stored in
//! `.manifold/epochs/e-{hash}/`. They are retained as long as any workspace's
//! upper-dir references that epoch (tracked via a ref-count file at
//! `.manifold/epochs/e-{hash}/.refcount`). Snapshots are removed during
//! workspace `destroy()` when the ref-count drops to zero.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{SnapshotResult, WorkspaceBackend, WorkspaceStatus};
use crate::model::types::{EpochId, WorkspaceId, WorkspaceInfo, WorkspaceMode, WorkspaceState};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors produced by the `OverlayFS` workspace backend.
#[derive(Debug)]
pub enum OverlayBackendError {
    /// `OverlayFS` backend is Linux-only.
    NotLinux,
    /// Neither fuse-overlayfs nor kernel overlayfs (user namespaces) is available.
    NotSupported { reason: String },
    /// An I/O error occurred.
    Io(std::io::Error),
    /// An external command failed.
    Command {
        command: String,
        stderr: String,
        exit_code: Option<i32>,
    },
    /// The workspace does not exist.
    NotFound { name: String },
}

impl fmt::Display for OverlayBackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLinux => write!(
                f,
                "OverlayFS backend is Linux-only. \
                 Use the git-worktree or reflink backend on this platform."
            ),
            Self::NotSupported { reason } => write!(
                f,
                "OverlayFS not available on this system: {reason}\n\
                 Install fuse-overlayfs (>= 0.7) or use a kernel >= 5.11 with \
                 user namespace overlayfs support."
            ),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Command {
                command,
                stderr,
                exit_code,
            } => {
                write!(f, "`{command}` failed")?;
                if let Some(code) = exit_code {
                    write!(f, " (exit {code})")?;
                }
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            Self::NotFound { name } => write!(f, "workspace '{name}' not found"),
        }
    }
}

impl std::error::Error for OverlayBackendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for OverlayBackendError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Mount strategy
// ---------------------------------------------------------------------------

/// Which `OverlayFS` mount mechanism to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountStrategy {
    /// `fuse-overlayfs` user-space FUSE daemon (preferred — persistent).
    FuseOverlayfs,
    /// Kernel overlayfs in a user namespace (`unshare -Ur mount -t overlay …`).
    KernelUserNamespace,
}

impl MountStrategy {
    /// Auto-detect the best available strategy on this system.
    ///
    /// Returns `None` if neither strategy is available.
    #[must_use]
    pub fn detect() -> Option<Self> {
        if !is_linux() {
            return None;
        }
        if command_available("fuse-overlayfs") {
            return Some(Self::FuseOverlayfs);
        }
        if kernel_userns_overlay_available() {
            return Some(Self::KernelUserNamespace);
        }
        None
    }
}

// ---------------------------------------------------------------------------
// OverlayBackend
// ---------------------------------------------------------------------------

/// `OverlayFS` workspace backend.
///
/// Creates isolated workspaces via overlay mounts using immutable epoch
/// snapshots as the read-only lower layer.
pub struct OverlayBackend {
    /// Repository root (where `.git` and `.manifold/` live).
    root: PathBuf,
    /// Mount strategy in use.
    strategy: MountStrategy,
}

impl OverlayBackend {
    /// Create a new `OverlayBackend` for the given repository root.
    ///
    /// Auto-selects the best available mount strategy.
    ///
    /// # Errors
    /// - `OverlayBackendError::NotLinux` on non-Linux platforms.
    /// - `OverlayBackendError::NotSupported` if no mount strategy is available.
    pub fn new(root: PathBuf) -> Result<Self, OverlayBackendError> {
        if !is_linux() {
            return Err(OverlayBackendError::NotLinux);
        }
        let strategy =
            MountStrategy::detect().ok_or_else(|| OverlayBackendError::NotSupported {
                reason:
                    "no fuse-overlayfs binary found and kernel user-namespace overlay unavailable"
                        .to_owned(),
            })?;
        Ok(Self { root, strategy })
    }

    // --- directory helpers --------------------------------------------------

    /// `ws/` directory (workspace mount points live here).
    fn workspaces_dir(&self) -> PathBuf {
        self.root.join("ws")
    }

    /// `ws/<name>/` — overlay mount point (the workspace working copy).
    fn mount_point(&self, name: &WorkspaceId) -> PathBuf {
        self.workspaces_dir().join(name.as_str())
    }

    /// `.manifold/epochs/e-{hash}/` — immutable epoch snapshot (lowerdir).
    fn epoch_snapshot_dir(&self, epoch: &EpochId) -> PathBuf {
        self.root
            .join(".manifold")
            .join("epochs")
            .join(format!("e-{}", epoch.as_str()))
    }

    /// `.manifold/cow/<name>/upper/` — per-workspace writable layer.
    fn upper_dir(&self, name: &WorkspaceId) -> PathBuf {
        self.root
            .join(".manifold")
            .join("cow")
            .join(name.as_str())
            .join("upper")
    }

    /// `.manifold/cow/<name>/work/` — overlayfs bookkeeping dir.
    fn work_dir(&self, name: &WorkspaceId) -> PathBuf {
        self.root
            .join(".manifold")
            .join("cow")
            .join(name.as_str())
            .join("work")
    }

    /// `.manifold/cow/<name>/epoch` — records which epoch this workspace uses.
    fn workspace_epoch_file(&self, name: &WorkspaceId) -> PathBuf {
        self.root
            .join(".manifold")
            .join("cow")
            .join(name.as_str())
            .join("epoch")
    }

    /// `.manifold/epochs/e-{hash}/.refcount` — snapshot reference count file.
    fn epoch_refcount_path(&self, epoch: &EpochId) -> PathBuf {
        self.epoch_snapshot_dir(epoch).join(".refcount")
    }

    // --- epoch snapshot management -----------------------------------------

    /// Ensure that `.manifold/epochs/e-{hash}/` exists and is populated.
    ///
    /// If the snapshot already exists (directory non-empty), this is a no-op.
    /// Otherwise, uses `git archive | tar -x` to materialize the epoch contents.
    fn ensure_epoch_snapshot(&self, epoch: &EpochId) -> Result<PathBuf, OverlayBackendError> {
        let snapshot_dir = self.epoch_snapshot_dir(epoch);

        // Already populated: snapshot dir exists and has content (not just .refcount).
        if snapshot_dir.exists() {
            let has_content = fs::read_dir(&snapshot_dir)
                .map(|mut rd| {
                    rd.any(|e| {
                        e.ok()
                            .is_some_and(|e| e.file_name() != ".refcount")
                    })
                })
                .unwrap_or(false);
            if has_content {
                return Ok(snapshot_dir);
            }
        }

        // Create snapshot directory.
        fs::create_dir_all(&snapshot_dir)?;

        // Extract epoch via `git archive <epoch> | tar -x -C <snapshot_dir>`.
        let archive_cmd = format!(
            "git -C '{}' archive '{}' | tar -x -C '{}'",
            self.root.display(),
            epoch.as_str(),
            snapshot_dir.display()
        );

        let output = Command::new("sh")
            .args(["-c", &archive_cmd])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()?;

        if !output.status.success() {
            let _ = fs::remove_dir_all(&snapshot_dir);
            return Err(OverlayBackendError::Command {
                command: format!("git archive {} | tar -x", epoch.as_str()),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                exit_code: output.status.code(),
            });
        }

        Ok(snapshot_dir)
    }

    /// Increment the reference count for an epoch snapshot.
    fn epoch_refcount_inc(&self, epoch: &EpochId) -> Result<(), OverlayBackendError> {
        let path = self.epoch_refcount_path(epoch);
        let count = self.read_refcount(epoch);
        fs::write(&path, (count + 1).to_string())?;
        Ok(())
    }

    /// Decrement the reference count and return the new count.
    ///
    /// Returns `0` if the file doesn't exist or contains an invalid value.
    fn epoch_refcount_dec(&self, epoch: &EpochId) -> Result<u32, OverlayBackendError> {
        let count = self.read_refcount(epoch);
        let new_count = count.saturating_sub(1);
        let path = self.epoch_refcount_path(epoch);
        fs::write(&path, new_count.to_string())?;
        Ok(new_count)
    }

    /// Read the current reference count for an epoch snapshot (0 if missing).
    fn read_refcount(&self, epoch: &EpochId) -> u32 {
        let path = self.epoch_refcount_path(epoch);
        fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0)
    }

    /// Remove an epoch snapshot if its reference count has reached zero.
    fn maybe_remove_epoch_snapshot(&self, epoch: &EpochId) -> Result<(), OverlayBackendError> {
        let count = self.read_refcount(epoch);
        if count == 0 {
            let snapshot_dir = self.epoch_snapshot_dir(epoch);
            if snapshot_dir.exists() {
                fs::remove_dir_all(&snapshot_dir)?;
            }
        }
        Ok(())
    }

    /// Best-effort cleanup for a partially-created workspace.
    fn cleanup_partial_workspace(&self, name: &WorkspaceId) {
        let _ = self.unmount_overlay(name);

        let mount_point = self.mount_point(name);
        if mount_point.exists() {
            let _ = fs::remove_dir_all(&mount_point);
        }

        let cow_dir = self.root.join(".manifold").join("cow").join(name.as_str());
        if cow_dir.exists() {
            let _ = fs::remove_dir_all(&cow_dir);
        }
    }

    // --- overlay mount operations ------------------------------------------

    /// Mount the overlay for a workspace.
    ///
    /// If the overlay is already mounted, this is a no-op (idempotent).
    fn mount_overlay(
        &self,
        name: &WorkspaceId,
        epoch: &EpochId,
    ) -> Result<(), OverlayBackendError> {
        let mount_point = self.mount_point(name);

        // Idempotent: already mounted.
        if is_overlay_mounted(&mount_point) {
            return Ok(());
        }

        let snapshot_dir = self.ensure_epoch_snapshot(epoch)?;
        let upper_dir = self.upper_dir(name);
        let work_dir = self.work_dir(name);

        fs::create_dir_all(&mount_point)?;
        fs::create_dir_all(&upper_dir)?;
        fs::create_dir_all(&work_dir)?;

        match self.strategy {
            MountStrategy::FuseOverlayfs => {
                Self::mount_fuse_overlayfs(&snapshot_dir, &upper_dir, &work_dir, &mount_point)?;
            }
            MountStrategy::KernelUserNamespace => {
                Self::mount_kernel_overlay(&snapshot_dir, &upper_dir, &work_dir, &mount_point)?;
            }
        }

        Ok(())
    }

    /// Mount using `fuse-overlayfs` (persistent FUSE daemon).
    fn mount_fuse_overlayfs(
        lower: &Path,
        upper: &Path,
        work: &Path,
        merged: &Path,
    ) -> Result<(), OverlayBackendError> {
        let options = format!(
            "lowerdir={},upperdir={},workdir={}",
            lower.display(),
            upper.display(),
            work.display()
        );
        let output = Command::new("fuse-overlayfs")
            .args(["-o", &options, merged.to_str().unwrap()])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()?;

        if !output.status.success() {
            return Err(OverlayBackendError::Command {
                command: "fuse-overlayfs".to_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                exit_code: output.status.code(),
            });
        }

        Ok(())
    }

    /// Mount using kernel overlayfs in a user namespace (`unshare -Ur`).
    ///
    /// **Note**: This mount is tied to the calling process's namespace and
    /// does not persist after the process exits. Prefer `fuse-overlayfs` for
    /// persistent workspaces.
    fn mount_kernel_overlay(
        lower: &Path,
        upper: &Path,
        work: &Path,
        merged: &Path,
    ) -> Result<(), OverlayBackendError> {
        let shell_cmd = format!(
            "mount -t overlay overlay -o lowerdir='{}',upperdir='{}',workdir='{}' '{}'",
            lower.display(),
            upper.display(),
            work.display(),
            merged.display()
        );
        let output = Command::new("unshare")
            .args(["-Ur", "sh", "-c", &shell_cmd])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()?;

        if !output.status.success() {
            return Err(OverlayBackendError::Command {
                command: "unshare -Ur mount -t overlay".to_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                exit_code: output.status.code(),
            });
        }

        Ok(())
    }

    /// Unmount an overlay workspace.
    ///
    /// Tries `fusermount3 -u`, then `fusermount -u`, then `umount -l` as
    /// fallbacks. Returns `Ok(())` if the mount point is not mounted at all
    /// (idempotent).
    fn unmount_overlay(&self, name: &WorkspaceId) -> Result<(), OverlayBackendError> {
        let mount_point = self.mount_point(name);

        if !mount_point.exists() || !is_overlay_mounted(&mount_point) {
            return Ok(());
        }

        let mp_str = mount_point.to_str().unwrap();

        // Try FUSE unmount first (works for both fuse-overlayfs and regular).
        for cmd in &[
            vec!["fusermount3", "-u", mp_str],
            vec!["fusermount", "-u", mp_str],
            vec!["umount", "-l", mp_str],
        ] {
            let status = Command::new(cmd[0])
                .args(&cmd[1..])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();

            if let Ok(s) = status
                && s.success() {
                    return Ok(());
                }
        }

        // Last resort: unshare umount
        let shell_cmd = format!("umount -l '{mp_str}'");
        let output = Command::new("unshare")
            .args(["-Ur", "sh", "-c", &shell_cmd])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()?;

        if !output.status.success() {
            // If unmount failed but the mount point is inaccessible, that's ok.
            // We'll proceed with best-effort cleanup.
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            if !stderr.is_empty() {
                // Log the error but don't fail destroy — we still clean up the dirs.
                eprintln!("warning: unmount failed: {stderr}");
            }
        }

        Ok(())
    }

    // --- workspace epoch file ----------------------------------------------

    /// Write the epoch OID to the workspace's epoch file.
    fn write_workspace_epoch(
        &self,
        name: &WorkspaceId,
        epoch: &EpochId,
    ) -> Result<(), OverlayBackendError> {
        let epoch_file = self.workspace_epoch_file(name);
        if let Some(parent) = epoch_file.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&epoch_file, epoch.as_str())?;
        Ok(())
    }

    /// Read the epoch OID from the workspace's epoch file.
    fn read_workspace_epoch(&self, name: &WorkspaceId) -> Result<EpochId, OverlayBackendError> {
        let epoch_file = self.workspace_epoch_file(name);
        let content = fs::read_to_string(&epoch_file)?;
        let oid = content.trim();
        EpochId::new(oid).map_err(|e| OverlayBackendError::Command {
            command: format!("read epoch file for workspace '{}'", name.as_str()),
            stderr: format!("invalid OID in epoch file: {e}"),
            exit_code: None,
        })
    }
}

// ---------------------------------------------------------------------------
// WorkspaceBackend impl
// ---------------------------------------------------------------------------

impl WorkspaceBackend for OverlayBackend {
    type Error = OverlayBackendError;

    fn create(&self, name: &WorkspaceId, epoch: &EpochId) -> Result<WorkspaceInfo, Self::Error> {
        let mount_point = self.mount_point(name);

        // Idempotent: if already mounted, return info.
        if is_overlay_mounted(&mount_point) {
            let stored_epoch = self
                .read_workspace_epoch(name)
                .unwrap_or_else(|_| epoch.clone());
            return Ok(WorkspaceInfo {
                id: name.clone(),
                path: mount_point,
                epoch: stored_epoch,
                state: WorkspaceState::Active,
                mode: WorkspaceMode::default(),
            });
        }

        // Ensure CoW directories exist.
        fs::create_dir_all(self.upper_dir(name))?;
        fs::create_dir_all(self.work_dir(name))?;

        // Record which epoch this workspace is anchored to.
        self.write_workspace_epoch(name, epoch)?;

        // Ensure the immutable epoch snapshot exists before mount.
        self.ensure_epoch_snapshot(epoch)?;

        // Mount the overlay. If this fails, remove partial workspace state.
        if let Err(err) = self.mount_overlay(name, epoch) {
            self.cleanup_partial_workspace(name);
            return Err(err);
        }

        // Count the mounted workspace as an epoch snapshot reference.
        if let Err(err) = self.epoch_refcount_inc(epoch) {
            self.cleanup_partial_workspace(name);
            return Err(err);
        }

        Ok(WorkspaceInfo {
            id: name.clone(),
            path: mount_point,
            epoch: epoch.clone(),
            state: WorkspaceState::Active,
            mode: WorkspaceMode::default(),
        })
    }

    fn destroy(&self, name: &WorkspaceId) -> Result<(), Self::Error> {
        // Unmount before removing directories.
        self.unmount_overlay(name)?;

        // Read the epoch so we can decrement its ref-count.
        let epoch_opt = self.read_workspace_epoch(name).ok();

        // Remove mount point directory.
        let mount_point = self.mount_point(name);
        if mount_point.exists() {
            fs::remove_dir_all(&mount_point)?;
        }

        // Remove CoW directories (upper + work).
        let cow_dir = self.root.join(".manifold").join("cow").join(name.as_str());
        if cow_dir.exists() {
            fs::remove_dir_all(&cow_dir)?;
        }

        // Decrement epoch ref-count and prune snapshot if no longer referenced.
        if let Some(epoch) = epoch_opt {
            let remaining = self.epoch_refcount_dec(&epoch)?;
            if remaining == 0 {
                self.maybe_remove_epoch_snapshot(&epoch)?;
            }
        }

        Ok(())
    }

    fn list(&self) -> Result<Vec<WorkspaceInfo>, Self::Error> {
        let cow_dir = self.root.join(".manifold").join("cow");
        if !cow_dir.exists() {
            return Ok(vec![]);
        }

        let mut infos = Vec::new();

        for entry in fs::read_dir(&cow_dir)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let Some(name_str) = file_name.to_str() else {
                continue;
            };

            let Ok(name) = WorkspaceId::new(name_str) else {
                continue;
            };

            // Read the epoch recorded for this workspace.
            let Ok(epoch) = self.read_workspace_epoch(&name) else {
                continue;
            };

            let mount_point = self.mount_point(&name);
            let is_mounted = is_overlay_mounted(&mount_point);

            // If the mount point doesn't exist at all, the workspace was
            // partially destroyed; skip it.
            if !mount_point.exists() && !self.upper_dir(&name).exists() {
                continue;
            }

            let state = if is_mounted {
                WorkspaceState::Active
            } else {
                // Not mounted but CoW dirs exist: treatable as Stale (needs remount).
                WorkspaceState::Stale { behind_epochs: 0 }
            };

            infos.push(WorkspaceInfo {
                id: name.clone(),
                path: mount_point,
                epoch,
                state,
                mode: WorkspaceMode::default(),
            });
        }

        Ok(infos)
    }

    fn status(&self, name: &WorkspaceId) -> Result<WorkspaceStatus, Self::Error> {
        // Ensure workspace CoW directories exist.
        if !self.upper_dir(name).exists() {
            return Err(OverlayBackendError::NotFound {
                name: name.as_str().to_owned(),
            });
        }

        let epoch = self.read_workspace_epoch(name)?;
        let mount_point = self.mount_point(name);

        // Auto-remount if the overlay is not active.
        if !is_overlay_mounted(&mount_point) {
            self.mount_overlay(name, &epoch)?;
        }

        // Collect dirty files by scanning the upper directory.
        let dirty_files = scan_upper_dir_for_dirty(&self.upper_dir(name))?;

        Ok(WorkspaceStatus::new(epoch, dirty_files, false))
    }

    fn snapshot(&self, name: &WorkspaceId) -> Result<SnapshotResult, Self::Error> {
        if !self.upper_dir(name).exists() {
            return Err(OverlayBackendError::NotFound {
                name: name.as_str().to_owned(),
            });
        }

        let epoch = self.read_workspace_epoch(name)?;
        let mount_point = self.mount_point(name);

        // Auto-remount if the overlay is not active.
        if !is_overlay_mounted(&mount_point) {
            self.mount_overlay(name, &epoch)?;
        }

        let snapshot_dir = self.epoch_snapshot_dir(&epoch);
        let upper_dir = self.upper_dir(name);

        diff_upper_vs_lower(&upper_dir, &snapshot_dir)
    }

    fn workspace_path(&self, name: &WorkspaceId) -> PathBuf {
        self.mount_point(name)
    }

    fn exists(&self, name: &WorkspaceId) -> bool {
        // A workspace exists if its CoW upper directory is present.
        self.upper_dir(name).exists()
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Returns `true` if we're running on Linux.
#[inline]
fn is_linux() -> bool {
    std::env::consts::OS == "linux"
}

/// Check whether a command is available in `$PATH`.
fn command_available(cmd: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {cmd} >/dev/null 2>&1")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Check whether kernel overlayfs via user namespaces is available.
///
/// Parses `uname -r` and probes `unshare -Ur mount -t overlay` in a tempdir.
fn kernel_userns_overlay_available() -> bool {
    if !is_linux() {
        return false;
    }
    if !command_available("unshare") {
        return false;
    }

    let Ok(dir) = tempfile::tempdir() else {
        return false;
    };

    let lower = dir.path().join("lower");
    let upper = dir.path().join("upper");
    let work = dir.path().join("work");
    let merged = dir.path().join("merged");

    if fs::create_dir_all(&lower).is_err()
        || fs::create_dir_all(&upper).is_err()
        || fs::create_dir_all(&work).is_err()
        || fs::create_dir_all(&merged).is_err()
        || fs::write(lower.join("probe"), b"ok").is_err()
    {
        return false;
    }

    let shell_cmd = format!(
        "mount -t overlay overlay \
         -o lowerdir='{}',upperdir='{}',workdir='{}' '{}' && umount '{}'",
        lower.display(),
        upper.display(),
        work.display(),
        merged.display(),
        merged.display()
    );

    Command::new("unshare")
        .args(["-Ur", "sh", "-c", &shell_cmd])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Check whether `path` is currently an overlay filesystem mount.
///
/// Reads `/proc/mounts` and looks for an entry whose mount point matches
/// `path`. Only available on Linux; returns `false` on other platforms.
#[must_use]
pub fn is_overlay_mounted(path: &Path) -> bool {
    if !is_linux() {
        return false;
    }

    let Some(path_str) = path.to_str() else {
        return false;
    };

    let Ok(mounts) = fs::read_to_string("/proc/mounts") else {
        return false;
    };

    for line in mounts.lines() {
        // /proc/mounts format: <device> <mountpoint> <fstype> <options> <dump> <pass>
        let mut fields = line.split_whitespace();
        let _device = fields.next();
        let Some(mountpoint) = fields.next() else {
            continue;
        };
        let Some(fstype) = fields.next() else {
            continue;
        };

        if (fstype == "overlay" || fstype == "fuse.fuse-overlayfs") && mountpoint == path_str {
            return true;
        }
    }

    false
}

/// Scan the upper directory of an overlay workspace and collect dirty files.
///
/// Returns all files present in the upper directory (excluding overlayfs
/// whiteout files and the `work/` directory). Paths are relative to the
/// upper directory root.
#[allow(clippy::items_after_statements)]
fn scan_upper_dir_for_dirty(upper: &Path) -> Result<Vec<PathBuf>, OverlayBackendError> {
    let mut dirty = Vec::new();

    if !upper.exists() {
        return Ok(dirty);
    }

    fn walk(dir: &Path, base: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;

            if ft.is_dir() {
                // Recurse, but skip the overlay work directory marker.
                let name = entry.file_name();
                if name == "work" {
                    continue;
                }
                walk(&path, base, out)?;
            } else {
                // Whiteout files are char devices with 0/0 major:minor; skip them.
                if is_whiteout_file(&path) {
                    continue;
                }
                let rel = path.strip_prefix(base).unwrap_or(&path);
                out.push(rel.to_path_buf());
            }
        }
        Ok(())
    }

    walk(upper, upper, &mut dirty)?;
    dirty.sort();
    Ok(dirty)
}

/// Returns `true` if `path` is an overlayfs whiteout file (char device 0:0).
///
/// Whiteout files represent deletions in the overlay upper layer. They have
/// device type `c` with major and minor numbers both equal to 0.
#[must_use]
fn is_whiteout_file(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(meta) = fs::metadata(path) {
            // Whiteout: char device (S_IFCHR = 0o20000) with rdev == 0.
            let is_char_dev = (meta.mode() & 0o170_000) == 0o020_000;
            return is_char_dev && meta.rdev() == 0;
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = path;
    false
}

/// Compute added/modified/deleted by comparing an overlay upper layer to the
/// immutable epoch snapshot (lowerdir).
///
/// - **Added**: path exists in `upper` but NOT in `lower`.
/// - **Modified**: path exists in both `upper` and `lower` (and is not a whiteout).
/// - **Deleted**: path is a whiteout file in `upper` (deletion marker).
///
/// All returned paths are relative to `upper` (== relative to the workspace root).
#[allow(clippy::items_after_statements)]
fn diff_upper_vs_lower(upper: &Path, lower: &Path) -> Result<SnapshotResult, OverlayBackendError> {
    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut deleted = Vec::new();

    if !upper.exists() {
        return Ok(SnapshotResult::new(added, modified, deleted));
    }

    fn walk(
        upper_dir: &Path,
        lower_dir: &Path,
        upper_base: &Path,
        added: &mut Vec<PathBuf>,
        modified: &mut Vec<PathBuf>,
        deleted: &mut Vec<PathBuf>,
    ) -> std::io::Result<()> {
        for entry in fs::read_dir(upper_dir)? {
            let entry = entry?;
            let upper_path = entry.path();
            let ft = entry.file_type()?;

            // Compute relative path from upper base.
            let rel = upper_path
                .strip_prefix(upper_base)
                .unwrap_or(&upper_path)
                .to_path_buf();

            if ft.is_dir() {
                // Recurse into subdirectories.
                let lower_subdir = lower_dir.join(rel.file_name().unwrap_or_default());
                walk(
                    &upper_path,
                    &lower_subdir,
                    upper_base,
                    added,
                    modified,
                    deleted,
                )?;
            } else if is_whiteout_file(&upper_path) {
                // Whiteout: this file was deleted.
                deleted.push(rel);
            } else {
                // Regular file: added if not in lower, modified if in lower.
                let lower_path = lower_dir.join(rel.file_name().unwrap_or_default());
                if lower_path.exists() {
                    modified.push(rel);
                } else {
                    added.push(rel);
                }
            }
        }
        Ok(())
    }

    walk(upper, lower, upper, &mut added, &mut modified, &mut deleted)?;

    added.sort();
    added.dedup();
    modified.sort();
    modified.dedup();
    deleted.sort();
    deleted.dedup();

    Ok(SnapshotResult::new(added, modified, deleted))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
mod tests {
    use super::*;

    // ---- is_whiteout_file ------------------------------------------------

    #[test]
    fn whiteout_file_regular_is_not_whiteout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("regular.txt");
        fs::write(&path, b"hello").unwrap();
        assert!(!is_whiteout_file(&path));
    }

    #[test]
    fn whiteout_file_directory_is_not_whiteout() {
        let dir = tempfile::tempdir().unwrap();
        let subdir = dir.path().join("subdir");
        fs::create_dir(&subdir).unwrap();
        assert!(!is_whiteout_file(&subdir));
    }

    // ---- scan_upper_dir_for_dirty ----------------------------------------

    #[test]
    fn scan_empty_upper_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().join("upper");
        fs::create_dir_all(&upper).unwrap();

        let dirty = scan_upper_dir_for_dirty(&upper).unwrap();
        assert!(dirty.is_empty(), "empty upper → no dirty files: {dirty:?}");
    }

    #[test]
    fn scan_upper_reports_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().join("upper");
        fs::create_dir_all(&upper).unwrap();

        fs::write(upper.join("modified.rs"), b"changed").unwrap();
        fs::create_dir_all(upper.join("src")).unwrap();
        fs::write(upper.join("src").join("new.rs"), b"added").unwrap();

        let mut dirty = scan_upper_dir_for_dirty(&upper).unwrap();
        dirty.sort();
        assert!(
            dirty.iter().any(|p| p == &PathBuf::from("modified.rs")),
            "should contain modified.rs: {dirty:?}"
        );
        assert!(
            dirty.iter().any(|p| p == &PathBuf::from("src/new.rs")),
            "should contain src/new.rs: {dirty:?}"
        );
    }

    // ---- diff_upper_vs_lower --------------------------------------------

    #[test]
    fn diff_empty_upper_empty_lower() {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().join("upper");
        let lower = dir.path().join("lower");
        fs::create_dir_all(&upper).unwrap();
        fs::create_dir_all(&lower).unwrap();

        let result = diff_upper_vs_lower(&upper, &lower).unwrap();
        assert!(result.is_empty(), "nothing changed: {result:?}");
    }

    #[test]
    fn diff_added_file_not_in_lower() {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().join("upper");
        let lower = dir.path().join("lower");
        fs::create_dir_all(&upper).unwrap();
        fs::create_dir_all(&lower).unwrap();
        // Only in upper → added
        fs::write(upper.join("new.rs"), b"fn main() {}").unwrap();

        let result = diff_upper_vs_lower(&upper, &lower).unwrap();
        assert_eq!(result.added.len(), 1, "one added file: {result:?}");
        assert_eq!(result.added[0], PathBuf::from("new.rs"));
        assert!(result.modified.is_empty());
        assert!(result.deleted.is_empty());
    }

    #[test]
    fn diff_modified_file_in_both() {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().join("upper");
        let lower = dir.path().join("lower");
        fs::create_dir_all(&upper).unwrap();
        fs::create_dir_all(&lower).unwrap();
        // Same name in both → modified
        fs::write(lower.join("README.md"), b"original").unwrap();
        fs::write(upper.join("README.md"), b"modified").unwrap();

        let result = diff_upper_vs_lower(&upper, &lower).unwrap();
        assert!(result.added.is_empty());
        assert_eq!(result.modified.len(), 1, "one modified file: {result:?}");
        assert_eq!(result.modified[0], PathBuf::from("README.md"));
        assert!(result.deleted.is_empty());
    }

    #[test]
    fn diff_empty_upper_no_changes() {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().join("upper");
        let lower = dir.path().join("lower");
        fs::create_dir_all(&upper).unwrap();
        fs::create_dir_all(&lower).unwrap();
        // File only in lower (not modified) → nothing reported
        fs::write(lower.join("base.rs"), b"base").unwrap();

        let result = diff_upper_vs_lower(&upper, &lower).unwrap();
        assert!(result.is_empty(), "no upper changes → empty: {result:?}");
    }

    // ---- is_overlay_mounted (smoke test on Linux) ------------------------

    #[test]
    fn is_overlay_mounted_returns_false_for_regular_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !is_overlay_mounted(dir.path()),
            "regular tempdir should not be an overlay mount"
        );
    }

    // ---- MountStrategy::detect -------------------------------------------

    #[test]
    fn mount_strategy_detect_smoke() {
        // Just verify it doesn't panic; the result depends on the host OS.
        let _strategy = MountStrategy::detect();
    }

    // ---- OverlayBackendError Display -------------------------------------

    #[test]
    fn error_display_not_linux() {
        let msg = format!("{}", OverlayBackendError::NotLinux);
        assert!(msg.contains("Linux-only"));
    }

    #[test]
    fn error_display_not_supported() {
        let msg = format!(
            "{}",
            OverlayBackendError::NotSupported {
                reason: "no binary".to_owned()
            }
        );
        assert!(msg.contains("no binary"));
    }

    #[test]
    fn error_display_not_found() {
        let msg = format!(
            "{}",
            OverlayBackendError::NotFound {
                name: "my-ws".to_owned()
            }
        );
        assert!(msg.contains("my-ws"));
    }

    #[test]
    fn error_display_command() {
        let msg = format!(
            "{}",
            OverlayBackendError::Command {
                command: "fuse-overlayfs".to_owned(),
                stderr: "permission denied".to_owned(),
                exit_code: Some(1),
            }
        );
        assert!(msg.contains("fuse-overlayfs"));
        assert!(msg.contains("permission denied"));
    }

    // ---- epoch refcount helpers (in-process, no mount needed) -----------

    #[test]
    fn epoch_refcount_inc_dec_remove() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        // Build a minimal backend (strategy doesn't matter for refcount ops).
        let backend = OverlayBackend {
            root,
            strategy: MountStrategy::FuseOverlayfs,
        };

        // We need a valid EpochId (40 lowercase hex chars).
        let oid = "a".repeat(40);
        let epoch = EpochId::new(&oid).unwrap();

        // Initial count is 0.
        assert_eq!(backend.read_refcount(&epoch), 0);

        // Create snapshot dir so the refcount file has a parent.
        let snap_dir = backend.epoch_snapshot_dir(&epoch);
        fs::create_dir_all(&snap_dir).unwrap();

        backend.epoch_refcount_inc(&epoch).unwrap();
        assert_eq!(backend.read_refcount(&epoch), 1);

        backend.epoch_refcount_inc(&epoch).unwrap();
        assert_eq!(backend.read_refcount(&epoch), 2);

        let remaining = backend.epoch_refcount_dec(&epoch).unwrap();
        assert_eq!(remaining, 1);

        let remaining = backend.epoch_refcount_dec(&epoch).unwrap();
        assert_eq!(remaining, 0);

        // Snapshot dir should be removed when refcount hits 0.
        backend.maybe_remove_epoch_snapshot(&epoch).unwrap();
        assert!(!snap_dir.exists(), "snapshot dir should be pruned");
    }

    // ---- ensure_epoch_snapshot (integration, requires git) ---------------

    #[test]
    fn ensure_epoch_snapshot_creates_files() {
        use std::process::Command as Cmd;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        // Init a small git repo with one commit.
        Cmd::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .unwrap();
        Cmd::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&root)
            .output()
            .unwrap();
        Cmd::new("git")
            .args(["config", "user.email", "t@t.com"])
            .current_dir(&root)
            .output()
            .unwrap();
        Cmd::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&root)
            .output()
            .unwrap();
        fs::write(root.join("hello.txt"), b"hello world").unwrap();
        Cmd::new("git")
            .args(["add", "hello.txt"])
            .current_dir(&root)
            .output()
            .unwrap();
        Cmd::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&root)
            .output()
            .unwrap();

        let head = Cmd::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        let oid_str = String::from_utf8(head.stdout).unwrap().trim().to_owned();
        let epoch = EpochId::new(&oid_str).unwrap();

        let backend = OverlayBackend {
            root,
            strategy: MountStrategy::FuseOverlayfs,
        };

        let snap = backend.ensure_epoch_snapshot(&epoch).unwrap();
        assert!(snap.exists(), "snapshot dir should exist");
        assert!(
            snap.join("hello.txt").exists(),
            "snapshot should contain hello.txt"
        );

        // Idempotent: calling again should not fail.
        let snap2 = backend.ensure_epoch_snapshot(&epoch).unwrap();
        assert_eq!(snap, snap2);
    }
}
