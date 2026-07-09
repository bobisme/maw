use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use maw_core::model::types::{EpochId, GitOid};

use super::capture::{CaptureMode, CaptureResult};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestroyReason {
    Destroy,
    MergeDestroy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordCaptureMode {
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
pub struct DestroyRecord {
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

impl DestroyRecord {
    /// The recovery ref that pins this record's snapshot, if any.
    ///
    /// `dirty_snapshot` captures store the pin in `snapshot_ref`;
    /// `head_only` captures store it in `final_head_ref`. `none`-mode
    /// records have no snapshot and thus no pinning ref.
    #[must_use]
    pub fn recovery_ref(&self) -> Option<&str> {
        self.snapshot_ref
            .as_deref()
            .or(self.final_head_ref.as_deref())
    }

    /// Parse `destroyed_at` (ISO-8601 UTC, e.g. `2026-03-07T00:05:47.278Z`)
    /// into Unix epoch seconds. Returns `None` if the timestamp is
    /// malformed. Used to age-gate destroy-record pruning.
    #[must_use]
    pub fn destroyed_at_epoch_secs(&self) -> Option<u64> {
        parse_iso8601_utc_secs(&self.destroyed_at)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LatestPointer {
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
    let destroyed_at = super::now_timestamp_iso8601_precise();
    let filename_ts = destroyed_at.replace(':', "-");

    let destroy_dir = maw_core::model::layout::LayoutFlavor::detect_with_env(root)
        .manifold_dir(root)
        .join("artifacts")
        .join("ws")
        .join(workspace_name)
        .join("destroy");

    fs::create_dir_all(&destroy_dir)
        .with_context(|| format!("create destroy artifact dir {}", destroy_dir.display()))?;

    let (capture_mode, final_head_ref, snapshot_oid, snapshot_ref, dirty_files) = capture
        .map_or_else(
            || (RecordCaptureMode::None, None, None, None, Vec::new()),
            |c| match c.mode {
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
        );

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

    let filename = path.file_name().map_or_else(
        || "artifact".to_owned(),
        |n| n.to_string_lossy().to_string(),
    );
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
pub fn destroy_dir(root: &Path, workspace_name: &str) -> PathBuf {
    maw_core::model::layout::LayoutFlavor::detect_with_env(root)
        .manifold_dir(root)
        .join("artifacts")
        .join("ws")
        .join(workspace_name)
        .join("destroy")
}

/// Read the latest pointer for a destroyed workspace, if any.
pub fn read_latest_pointer(root: &Path, workspace_name: &str) -> Result<Option<LatestPointer>> {
    let latest_path = destroy_dir(root, workspace_name).join("latest.json");
    if !latest_path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&latest_path)
        .with_context(|| format!("read {}", latest_path.display()))?;
    let pointer: LatestPointer = serde_json::from_str(&content)
        .with_context(|| format!("parse {}", latest_path.display()))?;
    Ok(Some(pointer))
}

/// Read a specific destroy record by filename.
pub fn read_record(root: &Path, workspace_name: &str, filename: &str) -> Result<DestroyRecord> {
    let record_path = destroy_dir(root, workspace_name).join(filename);
    let content = fs::read_to_string(&record_path)
        .with_context(|| format!("read {}", record_path.display()))?;
    let record: DestroyRecord = serde_json::from_str(&content)
        .with_context(|| format!("parse {}", record_path.display()))?;
    Ok(record)
}

/// List all destroy record filenames for a workspace (excluding latest.json).
pub fn list_record_files(root: &Path, workspace_name: &str) -> Result<Vec<String>> {
    let dir = destroy_dir(root, workspace_name);
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut records = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("read dir {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if is_json_record_name(&name) {
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
pub fn list_destroyed_workspaces(root: &Path) -> Result<Vec<String>> {
    let ws_dir = maw_core::model::layout::LayoutFlavor::detect_with_env(root)
        .manifold_dir(root)
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
        if destroy_path.join("latest.json").exists() || has_any_record_files(&destroy_path) {
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
        if is_json_record_name(&name) {
            return true;
        }
    }
    false
}

fn is_json_record_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        && name != "latest.json"
        && !name.starts_with('.')
}

/// Read the latest destroy record for a workspace, trying `latest.json` first,
/// then falling back to scanning timestamped record files.
///
/// Returns `None` only if no records exist at all.
pub fn read_latest_record(root: &Path, workspace_name: &str) -> Result<Option<DestroyRecord>> {
    // Fast path: latest.json exists and points to a valid record.
    if let Some(pointer) = read_latest_pointer(root, workspace_name)?
        && let Ok(record) = read_record(root, workspace_name, &pointer.record)
    {
        return Ok(Some(record));
    }
    // latest.json exists but points to a missing/corrupt record — fall through
    // to the directory scan.

    // Fallback: scan the directory for timestamped record files.
    let files = list_record_files(root, workspace_name)?;
    if let Some(last) = files.last() {
        let record = read_record(root, workspace_name, last)?;
        return Ok(Some(record));
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// Prune API — used by `maw gc --recovery-snapshots` to keep destroy records
// coherent with recovery refs (bn-3uou).
// ---------------------------------------------------------------------------

/// Remove a single timestamped destroy record and keep `latest.json`
/// coherent.
///
/// If `latest.json` pointed at (or has gone stale relative to) the removed
/// record, it is repointed at the newest surviving record. When the last
/// record for a workspace is removed, `latest.json` and the now-empty
/// `destroy/` (and parent `ws/<name>/`) directories are removed too so the
/// workspace stops being reported as an abandoned destroyed workspace.
///
/// Returns `true` if a record file was actually removed.
///
/// # Errors
///
/// Returns an error if filesystem operations fail.
pub fn remove_record(root: &Path, workspace_name: &str, filename: &str) -> Result<bool> {
    let dir = destroy_dir(root, workspace_name);
    let record_path = dir.join(filename);
    if !record_path.exists() {
        return Ok(false);
    }
    fs::remove_file(&record_path)
        .with_context(|| format!("remove destroy record {}", record_path.display()))?;

    // Recompute latest.json from the surviving timestamped records.
    let survivors = list_record_files(root, workspace_name)?;
    let latest_path = dir.join("latest.json");
    if let Some(newest) = survivors.last() {
        // Repoint latest.json at the newest surviving record if it was
        // pointing at the record we just removed (or is missing/stale).
        let repoint = match read_latest_pointer(root, workspace_name)? {
            Some(p) => p.record == filename || !survivors.contains(&p.record),
            None => true,
        };
        if repoint {
            let record = read_record(root, workspace_name, newest)?;
            let latest = LatestPointer {
                record: newest.clone(),
                destroyed_at: record.destroyed_at,
            };
            write_json_atomic(&latest_path, &latest)?;
        }
    } else {
        // No records remain — drop latest.json and the now-empty dirs.
        if latest_path.exists() {
            fs::remove_file(&latest_path)
                .with_context(|| format!("remove {}", latest_path.display()))?;
        }
        // `remove_dir` only succeeds when the directory is empty, so this is
        // a safe best-effort tidy-up: leftover sibling files keep the dir.
        let _ = fs::remove_dir(&dir);
        if let Some(ws_parent) = dir.parent() {
            let _ = fs::remove_dir(ws_parent);
        }
    }
    Ok(true)
}

/// Parse an ISO-8601 UTC timestamp (`YYYY-MM-DDThh:mm:ss[.fraction][Z]`) into
/// Unix epoch seconds. Returns `None` if the fixed-width layout is malformed.
///
/// Timestamps are always emitted by [`super::now_timestamp_iso8601_precise`]
/// in this exact zero-padded UTC form, so a bespoke parser avoids pulling in
/// a date-time dependency.
fn parse_iso8601_utc_secs(ts: &str) -> Option<u64> {
    let bytes = ts.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year: i64 = ts.get(0..4)?.parse().ok()?;
    let month: i64 = ts.get(5..7)?.parse().ok()?;
    let day: i64 = ts.get(8..10)?.parse().ok()?;
    let hour: u64 = ts.get(11..13)?.parse().ok()?;
    let min: u64 = ts.get(14..16)?.parse().ok()?;
    let sec: u64 = ts.get(17..19)?.parse().ok()?;
    if hour >= 24 || min >= 60 || sec >= 60 {
        return None;
    }
    let days = days_from_civil(year, month, day)?;
    days.checked_mul(86_400)?
        .checked_add(hour * 3_600 + min * 60 + sec)
}

/// Days since the Unix epoch for a proleptic-Gregorian date (the inverse of
/// `days_to_ymd` in the parent module; Howard Hinnant's `days_from_civil`).
/// Returns `None` for out-of-range months/days or pre-epoch dates (we only
/// ever handle post-2020 destroy timestamps).
fn days_from_civil(y: i64, m: i64, d: i64) -> Option<u64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use maw_core::model::types::WorkspaceId;

    #[test]
    fn write_destroy_record_creates_timestamped_and_latest_files() {
        let dir = tempfile::TempDir::new().expect("operation should succeed");
        let root = dir.path();
        let ws = WorkspaceId::new("agent-1").expect("operation should succeed");
        let base = EpochId::new(&"a".repeat(40)).expect("operation should succeed");
        let head = GitOid::new(&"b".repeat(40)).expect("operation should succeed");

        let record_path = write_destroy_record(
            root,
            ws.as_str(),
            &base,
            &head,
            None,
            DestroyReason::Destroy,
        )
        .expect("operation should succeed");

        assert!(record_path.exists());
        let latest = root
            .join(".manifold")
            .join("artifacts")
            .join("ws")
            .join(ws.as_str())
            .join("destroy")
            .join("latest.json");
        assert!(latest.exists());

        let latest_json = std::fs::read_to_string(latest).expect("operation should succeed");
        assert!(latest_json.contains(".json"));
        assert!(latest_json.contains("destroyed_at"));
    }

    #[test]
    fn list_destroyed_workspaces_finds_ws_without_latest_json() {
        let dir = tempfile::TempDir::new().expect("operation should succeed");
        let root = dir.path();
        let ws = WorkspaceId::new("orphan-1").expect("operation should succeed");
        let base = EpochId::new(&"a".repeat(40)).expect("operation should succeed");
        let head = GitOid::new(&"b".repeat(40)).expect("operation should succeed");

        // Write a normal destroy record (creates both timestamped + latest.json).
        write_destroy_record(
            root,
            ws.as_str(),
            &base,
            &head,
            None,
            DestroyReason::Destroy,
        )
        .expect("operation should succeed");

        // Verify the workspace is listed normally.
        let names = list_destroyed_workspaces(root).expect("operation should succeed");
        assert_eq!(names, vec!["orphan-1"]);

        // Simulate a partial write: delete latest.json, leaving the timestamped record.
        let latest = destroy_dir(root, ws.as_str()).join("latest.json");
        std::fs::remove_file(&latest).expect("operation should succeed");
        assert!(!latest.exists());

        // The workspace should still be discovered via directory scan.
        let names = list_destroyed_workspaces(root).expect("operation should succeed");
        assert_eq!(names, vec!["orphan-1"]);
    }

    #[test]
    fn read_latest_record_falls_back_when_latest_json_missing() {
        let dir = tempfile::TempDir::new().expect("operation should succeed");
        let root = dir.path();
        let ws = WorkspaceId::new("orphan-2").expect("operation should succeed");
        let base = EpochId::new(&"a".repeat(40)).expect("operation should succeed");
        let head = GitOid::new(&"b".repeat(40)).expect("operation should succeed");

        write_destroy_record(
            root,
            ws.as_str(),
            &base,
            &head,
            None,
            DestroyReason::Destroy,
        )
        .expect("operation should succeed");

        // Delete latest.json.
        let latest = destroy_dir(root, ws.as_str()).join("latest.json");
        std::fs::remove_file(&latest).expect("operation should succeed");

        // read_latest_record should still find the timestamped record.
        let record = read_latest_record(root, ws.as_str()).expect("operation should succeed");
        assert!(record.is_some());
        let record = record.expect("operation should succeed");
        assert_eq!(record.workspace_id, "orphan-2");
        assert_eq!(record.final_head, "b".repeat(40));
    }

    #[test]
    fn read_latest_record_returns_none_when_no_records() {
        let dir = tempfile::TempDir::new().expect("operation should succeed");
        let root = dir.path();

        let record = read_latest_record(root, "nonexistent").expect("operation should succeed");
        assert!(record.is_none());
    }

    #[test]
    fn read_latest_record_falls_back_when_latest_json_points_to_missing_file() {
        let dir = tempfile::TempDir::new().expect("operation should succeed");
        let root = dir.path();
        let ws = WorkspaceId::new("orphan-3").expect("operation should succeed");
        let base = EpochId::new(&"a".repeat(40)).expect("operation should succeed");
        let head = GitOid::new(&"b".repeat(40)).expect("operation should succeed");

        write_destroy_record(
            root,
            ws.as_str(),
            &base,
            &head,
            None,
            DestroyReason::Destroy,
        )
        .expect("operation should succeed");

        // Corrupt latest.json: point it to a nonexistent file.
        let latest_path = destroy_dir(root, ws.as_str()).join("latest.json");
        let bad_pointer = LatestPointer {
            record: "does-not-exist.json".to_string(),
            destroyed_at: "2025-01-01T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string_pretty(&bad_pointer).expect("operation should succeed");
        std::fs::write(&latest_path, json).expect("operation should succeed");

        // read_latest_record should fall back to the timestamped record.
        let record = read_latest_record(root, ws.as_str()).expect("operation should succeed");
        assert!(record.is_some());
        let record = record.expect("operation should succeed");
        assert_eq!(record.workspace_id, "orphan-3");
    }

    // -----------------------------------------------------------------------
    // Destroy record resilience tests (bn-qf0b)
    //
    // read_latest_pointer falls back to directory scan when latest.json is
    // missing or points to a nonexistent record.
    // -----------------------------------------------------------------------

    #[test]
    fn list_record_files_finds_records_without_latest_json() {
        let dir = tempfile::TempDir::new().expect("operation should succeed");
        let root = dir.path();
        let ws_name = "agent-orphan";

        // Write a destroy record.
        let base = EpochId::new(&"a".repeat(40)).expect("operation should succeed");
        let head = GitOid::new(&"b".repeat(40)).expect("operation should succeed");
        write_destroy_record(root, ws_name, &base, &head, None, DestroyReason::Destroy)
            .expect("operation should succeed");

        // Remove latest.json to simulate crash after record write but before
        // latest pointer was written.
        let latest_path = destroy_dir(root, ws_name).join("latest.json");
        assert!(latest_path.exists());
        std::fs::remove_file(&latest_path).expect("operation should succeed");
        assert!(!latest_path.exists());

        // list_record_files should still find the timestamped record.
        let records = list_record_files(root, ws_name).expect("operation should succeed");
        assert_eq!(
            records.len(),
            1,
            "expected exactly 1 record file via directory scan, got: {records:?}"
        );
        assert!(
            Path::new(&records[0])
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json")),
            "record filename should be a .json file"
        );

        // We can read the record directly from the discovered filename.
        let record = read_record(root, ws_name, &records[0]).expect("operation should succeed");
        assert_eq!(record.workspace_id, ws_name);
        assert_eq!(record.final_head, "b".repeat(40));
    }

    #[test]
    fn read_latest_pointer_returns_none_when_latest_json_missing() {
        let dir = tempfile::TempDir::new().expect("operation should succeed");
        let root = dir.path();
        let ws_name = "no-latest";

        // Write a destroy record, then delete latest.json.
        let base = EpochId::new(&"c".repeat(40)).expect("operation should succeed");
        let head = GitOid::new(&"d".repeat(40)).expect("operation should succeed");
        write_destroy_record(
            root,
            ws_name,
            &base,
            &head,
            None,
            DestroyReason::MergeDestroy,
        )
        .expect("operation should succeed");
        let latest_path = destroy_dir(root, ws_name).join("latest.json");
        std::fs::remove_file(&latest_path).expect("operation should succeed");

        // read_latest_pointer returns None (no latest.json).
        let pointer = read_latest_pointer(root, ws_name).expect("operation should succeed");
        assert!(
            pointer.is_none(),
            "expected None when latest.json is missing"
        );

        // But the fallback path (list_record_files -> read_record) still works.
        let records = list_record_files(root, ws_name).expect("operation should succeed");
        assert!(!records.is_empty(), "directory scan should find records");
        let record = read_record(
            root,
            ws_name,
            records.last().expect("operation should succeed"),
        )
        .expect("operation should succeed");
        assert_eq!(record.workspace_id, ws_name);
    }

    #[test]
    fn latest_json_pointing_to_nonexistent_file_is_detected() {
        let dir = tempfile::TempDir::new().expect("operation should succeed");
        let root = dir.path();
        let ws_name = "stale-latest";

        // Create the destroy directory with a real record.
        let base = EpochId::new(&"e".repeat(40)).expect("operation should succeed");
        let head = GitOid::new(&"f".repeat(40)).expect("operation should succeed");
        write_destroy_record(root, ws_name, &base, &head, None, DestroyReason::Destroy)
            .expect("operation should succeed");

        // Overwrite latest.json to point to a nonexistent record file.
        let bad_pointer = LatestPointer {
            record: "nonexistent-9999.json".to_owned(),
            destroyed_at: "2025-01-01T00:00:00Z".to_owned(),
        };
        let latest_path = destroy_dir(root, ws_name).join("latest.json");
        let json = serde_json::to_string_pretty(&bad_pointer).expect("operation should succeed");
        std::fs::write(&latest_path, json).expect("operation should succeed");

        // read_latest_pointer succeeds (it just reads the pointer).
        let pointer = read_latest_pointer(root, ws_name)
            .expect("operation should succeed")
            .expect("operation should succeed");
        assert_eq!(pointer.record, "nonexistent-9999.json");

        // But reading the record it points to fails.
        let read_result = read_record(root, ws_name, &pointer.record);
        assert!(
            read_result.is_err(),
            "reading a nonexistent record should fail"
        );

        // Fallback: list_record_files finds the real record.
        let records = list_record_files(root, ws_name).expect("operation should succeed");
        assert!(
            !records.is_empty(),
            "directory scan should find the real record"
        );
        // The real record is not the nonexistent one.
        assert!(
            !records.contains(&"nonexistent-9999.json".to_owned()),
            "directory scan should not return nonexistent files"
        );
        // We can read the real record.
        let record = read_record(
            root,
            ws_name,
            records.last().expect("operation should succeed"),
        )
        .expect("operation should succeed");
        assert_eq!(record.workspace_id, ws_name);
    }

    #[test]
    fn list_destroyed_workspaces_includes_ws_even_without_latest_json() {
        let dir = tempfile::TempDir::new().expect("operation should succeed");
        let root = dir.path();

        // Create two destroy records.
        let base = EpochId::new(&"a".repeat(40)).expect("operation should succeed");
        let head = GitOid::new(&"b".repeat(40)).expect("operation should succeed");
        write_destroy_record(
            root,
            "ws-complete",
            &base,
            &head,
            None,
            DestroyReason::Destroy,
        )
        .expect("operation should succeed");
        write_destroy_record(
            root,
            "ws-orphan",
            &base,
            &head,
            None,
            DestroyReason::Destroy,
        )
        .expect("operation should succeed");

        // Remove latest.json from ws-orphan (simulating partial write failure).
        let orphan_latest = destroy_dir(root, "ws-orphan").join("latest.json");
        std::fs::remove_file(&orphan_latest).expect("operation should succeed");

        // list_destroyed_workspaces SHOULD still return ws-orphan because it
        // scans for timestamped record files as a fallback.
        let destroyed = list_destroyed_workspaces(root).expect("operation should succeed");
        assert!(
            destroyed.contains(&"ws-complete".to_owned()),
            "ws-complete should be listed"
        );
        assert!(
            destroyed.contains(&"ws-orphan".to_owned()),
            "ws-orphan SHOULD be listed (fallback to directory scan)"
        );

        // And the orphaned workspace's record is discoverable via directory scan.
        let orphan_records =
            list_record_files(root, "ws-orphan").expect("operation should succeed");
        assert!(
            !orphan_records.is_empty(),
            "orphaned records should be findable via list_record_files"
        );
    }

    #[test]
    fn multiple_destroy_records_for_same_workspace() {
        let dir = tempfile::TempDir::new().expect("operation should succeed");
        let root = dir.path();
        let ws_name = "multi-destroy";

        let base = EpochId::new(&"a".repeat(40)).expect("operation should succeed");
        let head1 = GitOid::new(&"1".repeat(40)).expect("operation should succeed");
        let head2 = GitOid::new(&"2".repeat(40)).expect("operation should succeed");

        // Write first destroy record.
        write_destroy_record(root, ws_name, &base, &head1, None, DestroyReason::Destroy)
            .expect("operation should succeed");

        // The timestamp uses nanosecond-level granularity (bn-2dy4), so no
        // sleep is needed between writes to avoid filename collision.

        // Write second destroy record.
        write_destroy_record(
            root,
            ws_name,
            &base,
            &head2,
            None,
            DestroyReason::MergeDestroy,
        )
        .expect("operation should succeed");

        // Both records should be listed.
        let records = list_record_files(root, ws_name).expect("operation should succeed");
        assert_eq!(
            records.len(),
            2,
            "expected 2 destroy records, got: {records:?}"
        );

        // latest.json should point to the most recent one.
        let pointer = read_latest_pointer(root, ws_name)
            .expect("operation should succeed")
            .expect("operation should succeed");
        let latest_record =
            read_record(root, ws_name, &pointer.record).expect("operation should succeed");
        assert_eq!(
            latest_record.final_head,
            "2".repeat(40),
            "latest should point to the second record"
        );

        // First record is still readable.
        let first_record =
            read_record(root, ws_name, &records[0]).expect("operation should succeed");
        assert_eq!(first_record.final_head, "1".repeat(40));
    }

    /// Regression test for bn-2dy4: rapid back-to-back destroy records must
    /// use distinct timestamp-based filenames (no collision even within the
    /// same millisecond).
    #[test]
    fn back_to_back_destroys_produce_distinct_filenames() {
        let dir = tempfile::TempDir::new().expect("operation should succeed");
        let root = dir.path();
        let ws_name = "rapid-destroy";

        let base = EpochId::new(&"a".repeat(40)).expect("operation should succeed");
        let head = GitOid::new(&"b".repeat(40)).expect("operation should succeed");

        // Write 10 destroy records in a tight loop — no sleep.
        for _ in 0..10 {
            write_destroy_record(root, ws_name, &base, &head, None, DestroyReason::Destroy)
                .expect("operation should succeed");
        }

        let records = list_record_files(root, ws_name).expect("operation should succeed");
        assert_eq!(
            records.len(),
            10,
            "expected 10 distinct destroy records, got: {records:?}"
        );
    }

    // -----------------------------------------------------------------------
    // bn-3uou: recovery_ref accessor, timestamp parsing, and remove_record
    // (latest.json coherence on partial prune).
    // -----------------------------------------------------------------------

    fn record_with_refs(snapshot_ref: Option<&str>, final_head_ref: Option<&str>) -> DestroyRecord {
        DestroyRecord {
            workspace_id: "ws".to_owned(),
            destroyed_at: "2026-03-07T00:05:47.278Z".to_owned(),
            final_head: "b".repeat(40),
            final_head_ref: final_head_ref.map(str::to_owned),
            snapshot_oid: snapshot_ref.map(|_| "c".repeat(40)),
            snapshot_ref: snapshot_ref.map(str::to_owned),
            capture_mode: RecordCaptureMode::DirtySnapshot,
            dirty_files: vec![],
            base_epoch: "a".repeat(40),
            destroy_reason: DestroyReason::Destroy,
            tool_version: "test".to_owned(),
        }
    }

    #[test]
    fn recovery_ref_prefers_snapshot_then_final_head() {
        assert_eq!(
            record_with_refs(Some("refs/manifold/recovery/ws/snap"), None).recovery_ref(),
            Some("refs/manifold/recovery/ws/snap")
        );
        assert_eq!(
            record_with_refs(None, Some("refs/manifold/recovery/ws/head")).recovery_ref(),
            Some("refs/manifold/recovery/ws/head")
        );
        assert_eq!(record_with_refs(None, None).recovery_ref(), None);
    }

    #[test]
    fn destroyed_at_epoch_secs_parses_iso8601() {
        // 2026-03-07T00:05:47Z — verified against a reference epoch value.
        let rec = record_with_refs(None, None);
        assert_eq!(rec.destroyed_at_epoch_secs(), Some(1_772_841_947));

        // A malformed timestamp yields None (no panic).
        let mut bad = record_with_refs(None, None);
        bad.destroyed_at = "not-a-timestamp".to_owned();
        assert_eq!(bad.destroyed_at_epoch_secs(), None);

        // Epoch zero.
        let mut zero = record_with_refs(None, None);
        zero.destroyed_at = "1970-01-01T00:00:00Z".to_owned();
        assert_eq!(zero.destroyed_at_epoch_secs(), Some(0));
    }

    #[test]
    fn remove_record_repoints_latest_json_on_partial_prune() {
        let dir = tempfile::TempDir::new().expect("tmp");
        let root = dir.path();
        let ws = "multi";
        let base = EpochId::new(&"a".repeat(40)).expect("epoch");
        let h1 = GitOid::new(&"1".repeat(40)).expect("oid");
        let h2 = GitOid::new(&"2".repeat(40)).expect("oid");

        // Two records; latest.json points at the second (newest).
        write_destroy_record(root, ws, &base, &h1, None, DestroyReason::Destroy).expect("r1");
        write_destroy_record(root, ws, &base, &h2, None, DestroyReason::Destroy).expect("r2");
        let files = list_record_files(root, ws).expect("list");
        assert_eq!(files.len(), 2);
        let newest = files[1].clone();

        // Remove the NEWEST record (the one latest.json points at). latest.json
        // must repoint at the surviving (older) record, not dangle.
        let removed = remove_record(root, ws, &newest).expect("remove");
        assert!(removed);
        let survivors = list_record_files(root, ws).expect("list");
        assert_eq!(survivors.len(), 1);
        let pointer = read_latest_pointer(root, ws)
            .expect("read latest")
            .expect("latest present");
        assert_eq!(
            pointer.record, survivors[0],
            "latest.json must repoint at the surviving record"
        );
        // And it resolves to a real record.
        assert!(read_record(root, ws, &pointer.record).is_ok());
    }

    #[test]
    fn remove_record_drops_dir_when_last_record_removed() {
        let dir = tempfile::TempDir::new().expect("tmp");
        let root = dir.path();
        let ws = "solo";
        let base = EpochId::new(&"a".repeat(40)).expect("epoch");
        let head = GitOid::new(&"b".repeat(40)).expect("oid");
        write_destroy_record(root, ws, &base, &head, None, DestroyReason::Destroy).expect("r");

        let files = list_record_files(root, ws).expect("list");
        assert_eq!(files.len(), 1);
        assert!(remove_record(root, ws, &files[0]).expect("remove"));

        // No records, no latest.json, and the workspace is no longer counted
        // as a destroyed workspace.
        assert!(
            list_record_files(root, ws).expect("list").is_empty(),
            "no records remain"
        );
        assert!(
            !destroy_dir(root, ws).join("latest.json").exists(),
            "latest.json removed with the last record"
        );
        assert!(
            !list_destroyed_workspaces(root)
                .expect("list ws")
                .contains(&ws.to_owned()),
            "workspace no longer reported as destroyed once its last record is pruned"
        );
    }

    #[test]
    fn remove_record_is_noop_for_missing_file() {
        let dir = tempfile::TempDir::new().expect("tmp");
        let root = dir.path();
        assert!(!remove_record(root, "ghost", "nope.json").expect("noop"));
    }
}
