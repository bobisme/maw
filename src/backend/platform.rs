//! Platform capability detection for workspace backend selection.
//!
//! Detects runtime capabilities needed by `CoW` backends and caches the result
//! in `.manifold/platform-capabilities`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::config::BackendKind;

const CACHE_SCHEMA_VERSION: u32 = 1;
const REF_LINK_THRESHOLD_FILES: usize = 30_000;
const OVERLAY_THRESHOLD_FILES: usize = 100_000;

/// Detected host/platform capabilities for workspace backend selection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformCapabilities {
    /// Cache schema version for future migrations.
    pub schema_version: u32,
    /// Runtime reflink capability (best-effort test via `cp --reflink=always`).
    pub reflink_supported: bool,
    /// Runtime `OverlayFS` in user namespace capability.
    pub overlay_userns_supported: bool,
    /// `fuse-overlayfs` binary availability on compatible Linux kernels.
    pub fuse_overlayfs_available: bool,
    /// Parsed kernel major.minor (Linux only).
    pub kernel_major: Option<u32>,
    pub kernel_minor: Option<u32>,
}

impl Default for PlatformCapabilities {
    fn default() -> Self {
        Self {
            schema_version: CACHE_SCHEMA_VERSION,
            reflink_supported: false,
            overlay_userns_supported: false,
            fuse_overlayfs_available: false,
            kernel_major: None,
            kernel_minor: None,
        }
    }
}

/// Resolve path for platform capability cache.
#[must_use]
pub fn cache_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".manifold").join("platform-capabilities")
}

/// Load cached capabilities if present and valid.
#[must_use]
pub fn load_cached(repo_root: &Path) -> Option<PlatformCapabilities> {
    let path = cache_path(repo_root);
    let bytes = std::fs::read(path).ok()?;
    let caps = serde_json::from_slice::<PlatformCapabilities>(&bytes).ok()?;
    if caps.schema_version == CACHE_SCHEMA_VERSION {
        Some(caps)
    } else {
        None
    }
}

/// Detect capabilities (or read from cache), then persist cache.
#[must_use]
pub fn detect_or_load(repo_root: &Path) -> PlatformCapabilities {
    if let Some(cached) = load_cached(repo_root) {
        return cached;
    }

    let detected = detect_platform_capabilities();
    let _ = persist_cache(repo_root, &detected);
    detected
}

/// Persist capability cache to `.manifold/platform-capabilities`.
#[allow(clippy::missing_errors_doc)]
pub fn persist_cache(repo_root: &Path, caps: &PlatformCapabilities) -> std::io::Result<()> {
    let path = cache_path(repo_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let payload = serde_json::to_vec_pretty(caps)
        .map_err(|e| std::io::Error::other(format!("serialize capabilities: {e}")))?;
    std::fs::write(path, payload)
}

/// Run runtime platform capability detection.
#[must_use]
pub fn detect_platform_capabilities() -> PlatformCapabilities {
    let (kernel_major, kernel_minor) = linux_kernel_version();
    let reflink_supported = detect_reflink_support();
    let overlay_userns_supported = detect_overlay_userns_support(kernel_major, kernel_minor);
    let fuse_overlayfs_available = detect_fuse_overlayfs(kernel_major, kernel_minor);

    PlatformCapabilities {
        schema_version: CACHE_SCHEMA_VERSION,
        reflink_supported,
        overlay_userns_supported,
        fuse_overlayfs_available,
        kernel_major,
        kernel_minor,
    }
}

/// Resolve backend kind from config + platform capabilities.
///
/// Auto selection follows design doc §7.5 order:
/// 1. git-worktree
/// 2. reflink (when supported and repo > 30k files)
/// 3. overlay (when supported and repo > 100k files)
/// 4. copy fallback
#[must_use]
pub const fn resolve_backend_kind(
    configured: BackendKind,
    repo_file_count: usize,
    caps: &PlatformCapabilities,
) -> BackendKind {
    match configured {
        BackendKind::Auto => auto_select_backend(repo_file_count, caps),
        BackendKind::Reflink => {
            if caps.reflink_supported {
                BackendKind::Reflink
            } else {
                BackendKind::Copy
            }
        }
        BackendKind::Overlay => {
            if caps.overlay_userns_supported || caps.fuse_overlayfs_available {
                BackendKind::Overlay
            } else {
                BackendKind::Copy
            }
        }
        other => other,
    }
}

/// Auto-select backend using §7.5 priority.
///
/// Selection order (highest priority first):
/// 1. `git-worktree` — always available; default for repos < 30k files.
/// 2. `reflink`      — if CoW-capable filesystem and repo > 30k files.
/// 3. `overlay`      — if Linux overlayfs available and repo > 100k files.
/// 4. `copy`         — universal fallback (plain recursive copy).
#[must_use]
pub const fn auto_select_backend(
    repo_file_count: usize,
    caps: &PlatformCapabilities,
) -> BackendKind {
    // Overlay: Linux + overlayfs + large repo (highest CoW benefit)
    let overlay_candidate = (caps.overlay_userns_supported || caps.fuse_overlayfs_available)
        && repo_file_count > OVERLAY_THRESHOLD_FILES;
    if overlay_candidate {
        return BackendKind::Overlay;
    }

    // Reflink: CoW filesystem + medium/large repo
    let reflink_candidate = caps.reflink_supported && repo_file_count > REF_LINK_THRESHOLD_FILES;
    if reflink_candidate {
        return BackendKind::Reflink;
    }

    // Default: git-worktree (always works, fast for smaller repos)
    BackendKind::GitWorktree
}

/// Count tracked + untracked repo files (best-effort), excluding `.git` and `ws`.
#[must_use]
pub fn estimate_repo_file_count(repo_root: &Path) -> Option<usize> {
    fn walk(path: &Path, count: &mut usize) -> std::io::Result<()> {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let p = entry.path();
            let name = entry.file_name();
            if name == OsStr::new(".git") || name == OsStr::new("ws") {
                continue;
            }
            if p.is_dir() {
                walk(&p, count)?;
            } else {
                *count += 1;
            }
        }
        Ok(())
    }

    let mut count = 0;
    walk(repo_root, &mut count).ok()?;
    Some(count)
}

fn command_available(cmd: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {cmd} >/dev/null 2>&1")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn detect_reflink_support() -> bool {
    if !command_available("cp") {
        return false;
    }

    let Ok(dir) = tempfile::tempdir() else {
        return false;
    };

    let src = dir.path().join("src.tmp");
    let dst = dir.path().join("dst.tmp");
    if std::fs::write(&src, b"reflink-check").is_err() {
        return false;
    }

    Command::new("cp")
        .arg("--reflink=always")
        .arg(&src)
        .arg(&dst)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn detect_overlay_userns_support(kernel_major: Option<u32>, kernel_minor: Option<u32>) -> bool {
    if std::env::consts::OS != "linux" {
        return false;
    }
    if !kernel_at_least(kernel_major, kernel_minor, 5, 11) {
        return false;
    }
    if !command_available("unshare") || !command_available("mount") || !command_available("umount")
    {
        return false;
    }

    let Ok(dir) = tempfile::tempdir() else {
        return false;
    };
    let lower = dir.path().join("lower");
    let upper = dir.path().join("upper");
    let work = dir.path().join("work");
    let merged = dir.path().join("merged");

    if std::fs::create_dir_all(&lower).is_err()
        || std::fs::create_dir_all(&upper).is_err()
        || std::fs::create_dir_all(&work).is_err()
        || std::fs::create_dir_all(&merged).is_err()
    {
        return false;
    }
    if std::fs::write(lower.join("probe"), b"ok").is_err() {
        return false;
    }

    let shell_cmd = format!(
        "mount -t overlay overlay -o lowerdir='{}',upperdir='{}',workdir='{}' '{}' && umount '{}'",
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

fn detect_fuse_overlayfs(kernel_major: Option<u32>, kernel_minor: Option<u32>) -> bool {
    if std::env::consts::OS != "linux" {
        return false;
    }
    if !kernel_at_least(kernel_major, kernel_minor, 4, 18) {
        return false;
    }
    command_available("fuse-overlayfs")
}

const fn kernel_at_least(
    kernel_major: Option<u32>,
    kernel_minor: Option<u32>,
    min_major: u32,
    min_minor: u32,
) -> bool {
    match (kernel_major, kernel_minor) {
        (Some(major), Some(minor)) => {
            major > min_major || (major == min_major && minor >= min_minor)
        }
        _ => false,
    }
}

fn linux_kernel_version() -> (Option<u32>, Option<u32>) {
    if std::env::consts::OS != "linux" {
        return (None, None);
    }

    let output = match Command::new("uname").arg("-r").output() {
        Ok(output) if output.status.success() => output,
        _ => return (None, None),
    };

    let release = String::from_utf8_lossy(&output.stdout);
    parse_kernel_version(&release)
}

fn parse_kernel_version(release: &str) -> (Option<u32>, Option<u32>) {
    let release = release.trim();
    let mut parts = release.split('.');
    let major = parts.next().and_then(|p| p.parse::<u32>().ok());

    let minor_str = parts.next().map(|m| {
        m.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
    });
    let minor = minor_str.and_then(|s| s.parse::<u32>().ok());

    (major, minor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kernel_version_basic() {
        assert_eq!(parse_kernel_version("6.8.9"), (Some(6), Some(8)));
        assert_eq!(parse_kernel_version("5.15.153-1-lts"), (Some(5), Some(15)));
        assert_eq!(parse_kernel_version("not-a-version"), (None, None));
    }

    #[test]
    fn kernel_at_least_works() {
        assert!(kernel_at_least(Some(5), Some(11), 5, 11));
        assert!(kernel_at_least(Some(6), Some(1), 5, 11));
        assert!(!kernel_at_least(Some(5), Some(10), 5, 11));
        assert!(!kernel_at_least(None, Some(10), 5, 11));
    }

    #[test]
    fn cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let caps = PlatformCapabilities {
            schema_version: CACHE_SCHEMA_VERSION,
            reflink_supported: true,
            overlay_userns_supported: false,
            fuse_overlayfs_available: true,
            kernel_major: Some(6),
            kernel_minor: Some(8),
        };

        persist_cache(dir.path(), &caps).unwrap();
        let loaded = load_cached(dir.path()).unwrap();
        assert_eq!(loaded, caps);
    }

    #[test]
    fn resolve_backend_fallbacks() {
        let caps = PlatformCapabilities {
            schema_version: CACHE_SCHEMA_VERSION,
            reflink_supported: false,
            overlay_userns_supported: false,
            fuse_overlayfs_available: false,
            kernel_major: Some(6),
            kernel_minor: Some(8),
        };

        assert_eq!(
            resolve_backend_kind(BackendKind::Reflink, 50_000, &caps),
            BackendKind::Copy
        );
        assert_eq!(
            resolve_backend_kind(BackendKind::Overlay, 120_000, &caps),
            BackendKind::Copy
        );
    }

    #[test]
    fn auto_selection_git_worktree_for_small_repos() {
        let caps_all = PlatformCapabilities {
            schema_version: CACHE_SCHEMA_VERSION,
            reflink_supported: true,
            overlay_userns_supported: true,
            fuse_overlayfs_available: true,
            kernel_major: Some(6),
            kernel_minor: Some(8),
        };
        // Repos under 30k files → always git-worktree regardless of caps.
        assert_eq!(
            auto_select_backend(10_000, &caps_all),
            BackendKind::GitWorktree
        );
        assert_eq!(
            auto_select_backend(29_999, &caps_all),
            BackendKind::GitWorktree
        );
    }

    #[test]
    fn auto_selection_reflink_for_medium_repos() {
        let caps = PlatformCapabilities {
            schema_version: CACHE_SCHEMA_VERSION,
            reflink_supported: true,
            overlay_userns_supported: false,
            fuse_overlayfs_available: false,
            kernel_major: Some(6),
            kernel_minor: Some(8),
        };
        // 30k–100k files with reflink: pick reflink.
        assert_eq!(auto_select_backend(30_001, &caps), BackendKind::Reflink);
        assert_eq!(auto_select_backend(99_999, &caps), BackendKind::Reflink);
    }

    #[test]
    fn auto_selection_overlay_for_large_repos() {
        let caps = PlatformCapabilities {
            schema_version: CACHE_SCHEMA_VERSION,
            reflink_supported: true,
            overlay_userns_supported: true,
            fuse_overlayfs_available: true,
            kernel_major: Some(6),
            kernel_minor: Some(8),
        };
        // > 100k files with overlay support: pick overlay.
        assert_eq!(auto_select_backend(100_001, &caps), BackendKind::Overlay);
        assert_eq!(auto_select_backend(1_000_000, &caps), BackendKind::Overlay);
    }

    #[test]
    fn auto_selection_falls_back_to_reflink_when_no_overlay() {
        let caps = PlatformCapabilities {
            schema_version: CACHE_SCHEMA_VERSION,
            reflink_supported: true,
            overlay_userns_supported: false,
            fuse_overlayfs_available: false,
            kernel_major: Some(6),
            kernel_minor: Some(8),
        };
        // Large repo but no overlay → reflink.
        assert_eq!(auto_select_backend(200_000, &caps), BackendKind::Reflink);
    }

    #[test]
    fn auto_selection_falls_back_to_git_worktree_when_no_cow_caps() {
        let caps = PlatformCapabilities {
            schema_version: CACHE_SCHEMA_VERSION,
            reflink_supported: false,
            overlay_userns_supported: false,
            fuse_overlayfs_available: false,
            kernel_major: None,
            kernel_minor: None,
        };
        // No CoW caps at all → git-worktree for any repo size.
        assert_eq!(auto_select_backend(50_000, &caps), BackendKind::GitWorktree);
        assert_eq!(
            auto_select_backend(500_000, &caps),
            BackendKind::GitWorktree
        );
    }

    #[test]
    fn detect_capabilities_smoke_test() {
        let caps = detect_platform_capabilities();
        assert_eq!(caps.schema_version, CACHE_SCHEMA_VERSION);
    }

    /// Acceptance test: auto-selection produces a valid backend for the current platform.
    ///
    /// This test runs against the actual platform capabilities — it validates that
    /// the selection is consistent and produces a recognized backend kind.
    #[test]
    fn auto_selection_on_current_platform_returns_valid_backend() {
        let caps = detect_platform_capabilities();

        // Test a range of repo sizes.
        for &size in &[0_usize, 1_000, 30_000, 100_000, 500_000] {
            let kind = auto_select_backend(size, &caps);
            // Must be one of the valid concrete backend kinds.
            assert!(
                matches!(
                    kind,
                    BackendKind::GitWorktree
                        | BackendKind::Reflink
                        | BackendKind::Overlay
                        | BackendKind::Copy
                ),
                "auto_select_backend({size}, caps) returned {kind:?}, expected a concrete kind"
            );
        }
    }

    /// Acceptance test: `resolve_backend_kind(Auto, ...)` never returns `Auto`.
    #[test]
    fn resolve_backend_kind_never_returns_auto() {
        let caps = detect_platform_capabilities();
        let resolved = resolve_backend_kind(BackendKind::Auto, 50_000, &caps);
        assert_ne!(
            resolved,
            BackendKind::Auto,
            "resolved backend should never be Auto"
        );
    }

    /// Acceptance test: config override works for all backend types.
    ///
    /// When the config explicitly sets a backend, `resolve_backend_kind` must
    /// return that backend (or its fallback on unsupported platforms).
    #[test]
    fn config_override_for_all_backend_types() {
        let caps_none = PlatformCapabilities::default();

        // git-worktree: always passes through.
        assert_eq!(
            resolve_backend_kind(BackendKind::GitWorktree, 0, &caps_none),
            BackendKind::GitWorktree
        );

        // copy: always passes through.
        assert_eq!(
            resolve_backend_kind(BackendKind::Copy, 0, &caps_none),
            BackendKind::Copy
        );

        // reflink: falls back to copy when not supported.
        assert_eq!(
            resolve_backend_kind(BackendKind::Reflink, 0, &caps_none),
            BackendKind::Copy
        );

        // overlay: falls back to copy when not supported.
        assert_eq!(
            resolve_backend_kind(BackendKind::Overlay, 0, &caps_none),
            BackendKind::Copy
        );

        // reflink: passes through when supported.
        let caps_reflink = PlatformCapabilities {
            reflink_supported: true,
            ..PlatformCapabilities::default()
        };
        assert_eq!(
            resolve_backend_kind(BackendKind::Reflink, 0, &caps_reflink),
            BackendKind::Reflink
        );

        // overlay: passes through when supported.
        let caps_overlay = PlatformCapabilities {
            overlay_userns_supported: true,
            ..PlatformCapabilities::default()
        };
        assert_eq!(
            resolve_backend_kind(BackendKind::Overlay, 0, &caps_overlay),
            BackendKind::Overlay
        );
    }
}
