use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::types::{EpochId, GitOid};

use super::capture::{CaptureMode, CaptureResult};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestroyReason {
    Destroy,
    MergeDestroy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecordCaptureMode {
    DirtySnapshot,
    HeadOnly,
    None,
}

impl std::fmt::Display for RecordCaptureMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DirtySnapshot => write!(f, "dirty_snapshot"),
            Self::HeadOnly => write!(f, "head_only"),
            Self::None => write!(f, "none"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DestroyRecord {
    pub workspace_id: String,
    pub destroyed_at: String,
    pub final_head: String,
    pub final_head_ref: Option<String>,
    pub snapshot_oid: Option<String>,
    pub snapshot_ref: Option<String>,
    pub capture_mode: RecordCaptureMode,
    pub dirty_files: Vec<String>,
    pub base_epoch: String,
    pub destroy_reason: DestroyReason,
    pub tool_version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct LatestPointer {
    pub record: String,
    pub destroyed_at: String,
}

pub fn write_destroy_record(
    root: &Path,
    workspace_name: &str,
    base_epoch: &EpochId,
    final_head: &GitOid,
    capture: Option<&CaptureResult>,
    destroy_reason: DestroyReason,
) -> Result<PathBuf> {
    let destroyed_at = super::now_timestamp_iso8601();
    let filename_ts = destroyed_at.replace(':', "-");

    let destroy_dir = root
        .join(".manifold")
        .join("artifacts")
        .join("ws")
        .join(workspace_name)
        .join("destroy");

    fs::create_dir_all(&destroy_dir)
        .with_context(|| format!("create destroy artifact dir {}", destroy_dir.display()))?;

    let (capture_mode, final_head_ref, snapshot_oid, snapshot_ref, dirty_files) = match capture {
        Some(c) => match c.mode {
            CaptureMode::WorktreeCapture => (
                RecordCaptureMode::DirtySnapshot,
                None,
                Some(c.commit_oid.as_str().to_owned()),
                Some(c.pinned_ref.clone()),
                c.dirty_paths.clone(),
            ),
            CaptureMode::HeadOnly => (
                RecordCaptureMode::HeadOnly,
                Some(c.pinned_ref.clone()),
                None,
                None,
                Vec::new(),
            ),
        },
        None => (RecordCaptureMode::None, None, None, None, Vec::new()),
    };

    let record = DestroyRecord {
        workspace_id: workspace_name.to_owned(),
        destroyed_at: destroyed_at.clone(),
        final_head: final_head.as_str().to_owned(),
        final_head_ref,
        snapshot_oid,
        snapshot_ref,
        capture_mode,
        dirty_files,
        base_epoch: base_epoch.as_str().to_owned(),
        destroy_reason,
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
    };

    let record_filename = format!("{filename_ts}.json");
    let record_path = destroy_dir.join(&record_filename);
    write_json_atomic(&record_path, &record)?;

    let latest = LatestPointer {
        record: record_filename,
        destroyed_at,
    };
    let latest_path = destroy_dir.join("latest.json");
    write_json_atomic(&latest_path, &latest)?;

    Ok(record_path)
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("no parent directory for {}", path.display()))?;
    fs::create_dir_all(dir).with_context(|| format!("create dir {}", dir.display()))?;

    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "artifact".to_owned());
    let tmp_path = dir.join(format!(".{filename}.tmp"));

    let json = serde_json::to_string_pretty(value).context("serialize destroy record")?;

    let mut file = File::create(&tmp_path)
        .with_context(|| format!("create temp file {}", tmp_path.display()))?;
    file.write_all(json.as_bytes())
        .with_context(|| format!("write temp file {}", tmp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("fsync temp file {}", tmp_path.display()))?;
    drop(file);

    fs::rename(&tmp_path, path)
        .with_context(|| format!("rename {} -> {}", tmp_path.display(), path.display()))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Read API — used by `maw ws recover`
// ---------------------------------------------------------------------------

/// Path to the destroy artifacts directory for a workspace.
pub(crate) fn destroy_dir(root: &Path, workspace_name: &str) -> PathBuf {
    root.join(".manifold")
        .join("artifacts")
        .join("ws")
        .join(workspace_name)
        .join("destroy")
}

/// Read the latest pointer for a destroyed workspace, if any.
pub(crate) fn read_latest_pointer(root: &Path, workspace_name: &str) -> Result<Option<LatestPointer>> {
    let latest_path = destroy_dir(root, workspace_name).join("latest.json");
    if !latest_path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&latest_path)
        .with_context(|| format!("read {}", latest_path.display()))?;
    let pointer: LatestPointer =
        serde_json::from_str(&content).with_context(|| format!("parse {}", latest_path.display()))?;
    Ok(Some(pointer))
}

/// Read a specific destroy record by filename.
pub(crate) fn read_record(root: &Path, workspace_name: &str, filename: &str) -> Result<DestroyRecord> {
    let record_path = destroy_dir(root, workspace_name).join(filename);
    let content = fs::read_to_string(&record_path)
        .with_context(|| format!("read {}", record_path.display()))?;
    let record: DestroyRecord =
        serde_json::from_str(&content).with_context(|| format!("parse {}", record_path.display()))?;
    Ok(record)
}

/// List all destroy record filenames for a workspace (excluding latest.json).
pub(crate) fn list_record_files(root: &Path, workspace_name: &str) -> Result<Vec<String>> {
    let dir = destroy_dir(root, workspace_name);
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut records = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("read dir {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".json") && name != "latest.json" && !name.starts_with('.') {
            records.push(name);
        }
    }
    records.sort();
    Ok(records)
}

/// List all workspace names that have destroy records.
///
/// A workspace is considered "destroyed" if its `destroy/` directory contains
/// either a `latest.json` pointer or any timestamped record files.  This makes
/// discovery resilient to partial writes where the record was persisted but
/// `latest.json` was not (e.g. crash between the two writes).
pub(crate) fn list_destroyed_workspaces(root: &Path) -> Result<Vec<String>> {
    let ws_dir = root
        .join(".manifold")
        .join("artifacts")
        .join("ws");
    if !ws_dir.exists() {
        return Ok(vec![]);
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(&ws_dir).with_context(|| format!("read dir {}", ws_dir.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let ws_name = entry.file_name().to_string_lossy().to_string();
        let destroy_path = entry.path().join("destroy");
        if !destroy_path.exists() {
            continue;
        }
        // Check for latest.json first (fast path), then fall back to scanning
        // for any timestamped record files.
        if destroy_path.join("latest.json").exists() {
            names.push(ws_name);
        } else if has_any_record_files(&destroy_path) {
            names.push(ws_name);
        }
    }
    names.sort();
    Ok(names)
}

/// Check whether a destroy directory contains any timestamped record files.
fn has_any_record_files(destroy_dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(destroy_dir) else {
        return false;
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".json") && name != "latest.json" && !name.starts_with('.') {
            return true;
        }
    }
    false
}

/// Read the latest destroy record for a workspace, trying `latest.json` first,
/// then falling back to scanning timestamped record files.
///
/// Returns `None` only if no records exist at all.
pub(crate) fn read_latest_record(root: &Path, workspace_name: &str) -> Result<Option<DestroyRecord>> {
    // Fast path: latest.json exists and points to a valid record.
    if let Some(pointer) = read_latest_pointer(root, workspace_name)? {
        if let Ok(record) = read_record(root, workspace_name, &pointer.record) {
            return Ok(Some(record));
        }
        // latest.json exists but points to a missing/corrupt record — fall through
        // to the directory scan.
    }

    // Fallback: scan the directory for timestamped record files.
    let files = list_record_files(root, workspace_name)?;
    if let Some(last) = files.last() {
        let record = read_record(root, workspace_name, last)?;
        return Ok(Some(record));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::WorkspaceId;

    #[test]
    fn write_destroy_record_creates_timestamped_and_latest_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let ws = WorkspaceId::new("agent-1").unwrap();
        let base = EpochId::new(&"a".repeat(40)).unwrap();
        let head = GitOid::new(&"b".repeat(40)).unwrap();

        let record_path = write_destroy_record(
            root,
            ws.as_str(),
            &base,
            &head,
            None,
            DestroyReason::Destroy,
        )
        .unwrap();

        assert!(record_path.exists());
        let latest = root
            .join(".manifold")
            .join("artifacts")
            .join("ws")
            .join(ws.as_str())
            .join("destroy")
            .join("latest.json");
        assert!(latest.exists());

        let latest_json = std::fs::read_to_string(latest).unwrap();
        assert!(latest_json.contains(".json"));
        assert!(latest_json.contains("destroyed_at"));
    }

    #[test]
    fn list_destroyed_workspaces_finds_ws_without_latest_json() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let ws = WorkspaceId::new("orphan-1").unwrap();
        let base = EpochId::new(&"a".repeat(40)).unwrap();
        let head = GitOid::new(&"b".repeat(40)).unwrap();

        // Write a normal destroy record (creates both timestamped + latest.json).
        write_destroy_record(root, ws.as_str(), &base, &head, None, DestroyReason::Destroy)
            .unwrap();

        // Verify the workspace is listed normally.
        let names = list_destroyed_workspaces(root).unwrap();
        assert_eq!(names, vec!["orphan-1"]);

        // Simulate a partial write: delete latest.json, leaving the timestamped record.
        let latest = destroy_dir(root, ws.as_str()).join("latest.json");
        std::fs::remove_file(&latest).unwrap();
        assert!(!latest.exists());

        // The workspace should still be discovered via directory scan.
        let names = list_destroyed_workspaces(root).unwrap();
        assert_eq!(names, vec!["orphan-1"]);
    }

    #[test]
    fn read_latest_record_falls_back_when_latest_json_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let ws = WorkspaceId::new("orphan-2").unwrap();
        let base = EpochId::new(&"a".repeat(40)).unwrap();
        let head = GitOid::new(&"b".repeat(40)).unwrap();

        write_destroy_record(root, ws.as_str(), &base, &head, None, DestroyReason::Destroy)
            .unwrap();

        // Delete latest.json.
        let latest = destroy_dir(root, ws.as_str()).join("latest.json");
        std::fs::remove_file(&latest).unwrap();

        // read_latest_record should still find the timestamped record.
        let record = read_latest_record(root, ws.as_str()).unwrap();
        assert!(record.is_some());
        let record = record.unwrap();
        assert_eq!(record.workspace_id, "orphan-2");
        assert_eq!(record.final_head, "b".repeat(40));
    }

    #[test]
    fn read_latest_record_returns_none_when_no_records() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        let record = read_latest_record(root, "nonexistent").unwrap();
        assert!(record.is_none());
    }

    #[test]
    fn read_latest_record_falls_back_when_latest_json_points_to_missing_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let ws = WorkspaceId::new("orphan-3").unwrap();
        let base = EpochId::new(&"a".repeat(40)).unwrap();
        let head = GitOid::new(&"b".repeat(40)).unwrap();

        write_destroy_record(root, ws.as_str(), &base, &head, None, DestroyReason::Destroy)
            .unwrap();

        // Corrupt latest.json: point it to a nonexistent file.
        let latest_path = destroy_dir(root, ws.as_str()).join("latest.json");
        let bad_pointer = LatestPointer {
            record: "does-not-exist.json".to_string(),
            destroyed_at: "2025-01-01T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string_pretty(&bad_pointer).unwrap();
        std::fs::write(&latest_path, json).unwrap();

        // read_latest_record should fall back to the timestamped record.
        let record = read_latest_record(root, ws.as_str()).unwrap();
        assert!(record.is_some());
        let record = record.unwrap();
        assert_eq!(record.workspace_id, "orphan-3");
    }
}
