use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::model::types::{EpochId, GitOid};

use super::capture::{CaptureMode, CaptureResult};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DestroyReason {
    Destroy,
    MergeDestroy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RecordCaptureMode {
    DirtySnapshot,
    HeadOnly,
    None,
}

#[derive(Clone, Debug, Serialize)]
struct DestroyRecord {
    workspace_id: String,
    destroyed_at: String,
    final_head: String,
    final_head_ref: Option<String>,
    snapshot_oid: Option<String>,
    snapshot_ref: Option<String>,
    capture_mode: RecordCaptureMode,
    dirty_files: Vec<String>,
    base_epoch: String,
    destroy_reason: DestroyReason,
    tool_version: String,
}

#[derive(Clone, Debug, Serialize)]
struct LatestPointer {
    record: String,
    destroyed_at: String,
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
}
