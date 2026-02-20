//! Per-workspace metadata persistence.
//!
//! Stores workspace-level configuration (e.g., `mode: ephemeral | persistent`)
//! in `.manifold/workspaces/<name>.toml` within the repository root.
//!
//! The metadata file is separate from the git backend so it can be read and
//! written without touching the git index or working tree.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::types::WorkspaceMode;

// ---------------------------------------------------------------------------
// WorkspaceMetadata
// ---------------------------------------------------------------------------

/// Persistent metadata for a single workspace.
///
/// Stored at `.manifold/workspaces/<name>.toml` in the repository root.
/// Fields that are missing from an existing file default to their `Default`
/// implementation, so new fields can be added without breaking old files.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceMetadata {
    /// Workspace lifetime mode.
    #[serde(default)]
    pub mode: WorkspaceMode,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Read metadata for a workspace.
///
/// Returns `Ok(WorkspaceMetadata::default())` if the metadata file does not
/// exist (treats missing metadata as ephemeral — the default mode).
///
/// # Errors
/// Returns an error if the file exists but cannot be read or parsed.
pub fn read(repo_root: &Path, name: &str) -> Result<WorkspaceMetadata> {
    let path = metadata_path(repo_root, name);
    if !path.exists() {
        return Ok(WorkspaceMetadata::default());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read workspace metadata: {}", path.display()))?;
    toml::from_str(&content)
        .with_context(|| format!("Failed to parse workspace metadata: {}", path.display()))
}

/// Write metadata for a workspace.
///
/// Creates the `.manifold/workspaces/` directory if it does not exist.
///
/// # Errors
/// Returns an error if the directory cannot be created or the file cannot be written.
pub fn write(repo_root: &Path, name: &str, meta: &WorkspaceMetadata) -> Result<()> {
    let path = metadata_path(repo_root, name);
    let dir = path.parent().expect("metadata path always has a parent");
    std::fs::create_dir_all(dir)
        .with_context(|| format!("Failed to create metadata directory: {}", dir.display()))?;
    let content =
        toml::to_string_pretty(meta).with_context(|| "Failed to serialize workspace metadata")?;
    std::fs::write(&path, content)
        .with_context(|| format!("Failed to write workspace metadata: {}", path.display()))
}

/// Delete metadata for a workspace (called on destroy).
///
/// A no-op if the file does not exist.
///
/// # Errors
/// Returns an error only if the file exists but cannot be deleted.
pub fn delete(repo_root: &Path, name: &str) -> Result<()> {
    let path = metadata_path(repo_root, name);
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("Failed to delete workspace metadata: {}", path.display()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Canonical path for the metadata file of a workspace.
pub fn metadata_path(repo_root: &Path, name: &str) -> PathBuf {
    repo_root
        .join(".manifold")
        .join("workspaces")
        .join(format!("{name}.toml"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_and_read(meta: &WorkspaceMetadata) -> WorkspaceMetadata {
        let dir = tempdir().unwrap();
        write(dir.path(), "test-ws", meta).unwrap();
        read(dir.path(), "test-ws").unwrap()
    }

    #[test]
    fn missing_file_returns_default() {
        let dir = tempdir().unwrap();
        let meta = read(dir.path(), "nonexistent").unwrap();
        assert_eq!(meta, WorkspaceMetadata::default());
        assert!(meta.mode.is_ephemeral());
    }

    #[test]
    fn roundtrip_ephemeral() {
        let meta = WorkspaceMetadata {
            mode: WorkspaceMode::Ephemeral,
        };
        let decoded = write_and_read(&meta);
        assert_eq!(decoded.mode, WorkspaceMode::Ephemeral);
    }

    #[test]
    fn roundtrip_persistent() {
        let meta = WorkspaceMetadata {
            mode: WorkspaceMode::Persistent,
        };
        let decoded = write_and_read(&meta);
        assert_eq!(decoded.mode, WorkspaceMode::Persistent);
    }

    #[test]
    fn creates_directory() {
        let dir = tempdir().unwrap();
        let meta = WorkspaceMetadata {
            mode: WorkspaceMode::Persistent,
        };
        write(dir.path(), "my-ws", &meta).unwrap();
        let expected_path = dir
            .path()
            .join(".manifold")
            .join("workspaces")
            .join("my-ws.toml");
        assert!(expected_path.exists());
    }

    #[test]
    fn delete_existing() {
        let dir = tempdir().unwrap();
        let meta = WorkspaceMetadata {
            mode: WorkspaceMode::Persistent,
        };
        write(dir.path(), "ws", &meta).unwrap();
        delete(dir.path(), "ws").unwrap();
        // After delete, reading returns default (file gone).
        let after = read(dir.path(), "ws").unwrap();
        assert!(after.mode.is_ephemeral());
    }

    #[test]
    fn delete_nonexistent_is_noop() {
        let dir = tempdir().unwrap();
        // Should not error.
        delete(dir.path(), "ghost").unwrap();
    }

    #[test]
    fn metadata_path_format() {
        let path = metadata_path(Path::new("/repo"), "my-workspace");
        assert_eq!(
            path,
            PathBuf::from("/repo/.manifold/workspaces/my-workspace.toml")
        );
    }
}
