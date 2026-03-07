use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const SCHEMA_VERSION: u32 = 1;
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_STALE_AFTER: Duration = Duration::from_secs(30);
const LOCK_INITIAL_BACKOFF: Duration = Duration::from_millis(10);
const LOCK_MAX_BACKOFF: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeIndex {
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub by_branch: BTreeMap<String, String>,
    #[serde(default)]
    pub by_workspace: BTreeMap<String, String>,
}

impl Default for ChangeIndex {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            by_branch: BTreeMap::new(),
            by_workspace: BTreeMap::new(),
        }
    }
}

impl ChangeIndex {
    #[must_use]
    pub fn change_for_branch(&self, branch: &str) -> Option<&str> {
        self.by_branch.get(branch).map(String::as_str)
    }

    #[must_use]
    pub fn change_for_workspace(&self, workspace: &str) -> Option<&str> {
        self.by_workspace.get(workspace).map(String::as_str)
    }

    pub fn set_branch_mapping(&mut self, branch: &str, change_id: &str) {
        self.by_branch
            .insert(branch.to_owned(), change_id.to_owned());
    }

    pub fn set_workspace_mapping(&mut self, workspace: &str, change_id: &str) {
        self.by_workspace
            .insert(workspace.to_owned(), change_id.to_owned());
    }

    pub fn clear_mappings_for_change(&mut self, change_id: &str) {
        self.by_branch.retain(|_, mapped| mapped != change_id);
        self.by_workspace.retain(|_, mapped| mapped != change_id);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeState {
    Open,
    Review,
    Merged,
    Closed,
    Aborted,
}

impl Default for ChangeState {
    fn default() -> Self {
        Self::Open
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSource {
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub from_oid: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeGit {
    #[serde(default)]
    pub base_branch: String,
    #[serde(default)]
    pub change_branch: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeWorkspaces {
    #[serde(default)]
    pub primary: String,
    #[serde(default)]
    pub linked: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeTracker {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangePr {
    pub number: u64,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub draft: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRecord {
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    pub change_id: String,
    pub title: String,
    #[serde(default)]
    pub state: ChangeState,
    pub created_at: String,
    #[serde(default)]
    pub source: ChangeSource,
    #[serde(default)]
    pub git: ChangeGit,
    #[serde(default)]
    pub workspaces: ChangeWorkspaces,
    #[serde(default)]
    pub tracker: Option<ChangeTracker>,
    #[serde(default)]
    pub pr: Option<ChangePr>,
}

fn schema_version() -> u32 {
    SCHEMA_VERSION
}

#[derive(Clone, Debug)]
pub struct ChangesStore {
    repo_root: PathBuf,
}

impl ChangesStore {
    #[must_use]
    pub fn open(repo_root: &Path) -> Self {
        Self {
            repo_root: repo_root.to_path_buf(),
        }
    }

    #[must_use]
    pub fn changes_root(&self) -> PathBuf {
        self.repo_root.join(".manifold").join("changes")
    }

    #[must_use]
    pub fn index_path(&self) -> PathBuf {
        self.changes_root().join("index.toml")
    }

    #[must_use]
    pub fn active_dir(&self) -> PathBuf {
        self.changes_root().join("active")
    }

    #[must_use]
    pub fn archive_dir(&self) -> PathBuf {
        self.changes_root().join("archive")
    }

    pub fn active_record_path(&self, change_id: &str) -> Result<PathBuf> {
        validate_change_id(change_id)?;
        Ok(self.active_dir().join(format!("{change_id}.toml")))
    }

    pub fn read_index(&self) -> Result<ChangeIndex> {
        let path = self.index_path();
        if !path.exists() {
            return Ok(ChangeIndex::default());
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read changes index: {}", path.display()))?;
        toml::from_str(&content)
            .with_context(|| format!("Failed to parse changes index: {}", path.display()))
    }

    pub fn read_active_record(&self, change_id: &str) -> Result<Option<ChangeRecord>> {
        let path = self.active_record_path(change_id)?;
        if !path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read change record: {}", path.display()))?;
        let record: ChangeRecord = toml::from_str(&content)
            .with_context(|| format!("Failed to parse change record: {}", path.display()))?;
        Ok(Some(record))
    }

    pub fn list_active_records(&self) -> Result<Vec<ChangeRecord>> {
        let dir = self.active_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut files = Vec::new();
        for entry in
            fs::read_dir(&dir).with_context(|| format!("Failed to read dir: {}", dir.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || !name.ends_with(".toml") {
                continue;
            }
            files.push(entry.path());
        }
        files.sort();

        let mut records = Vec::with_capacity(files.len());
        for path in files {
            let content = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read change record: {}", path.display()))?;
            let record: ChangeRecord = toml::from_str(&content)
                .with_context(|| format!("Failed to parse change record: {}", path.display()))?;
            records.push(record);
        }
        Ok(records)
    }

    pub fn with_lock<T, F>(&self, operation: &str, f: F) -> Result<T>
    where
        F: FnOnce(&LockedChangesStore<'_>) -> Result<T>,
    {
        let lock_path = self.changes_root().join(".lock");
        let lock_guard = acquire_lock(&lock_path, operation)?;
        let locked = LockedChangesStore {
            store: self,
            _lock_guard: lock_guard,
        };
        f(&locked)
    }
}

pub struct LockedChangesStore<'a> {
    store: &'a ChangesStore,
    _lock_guard: ChangesLockGuard,
}

impl LockedChangesStore<'_> {
    pub fn write_index(&self, index: &ChangeIndex) -> Result<()> {
        write_toml_atomic(&self.store.index_path(), index, "changes index")
    }

    pub fn write_active_record(&self, record: &ChangeRecord) -> Result<()> {
        validate_change_id(&record.change_id)?;
        let path = self.store.active_record_path(&record.change_id)?;
        write_toml_atomic(&path, record, "change record")
    }

    pub fn delete_active_record(&self, change_id: &str) -> Result<()> {
        let path = self.store.active_record_path(change_id)?;
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("Failed to delete change record: {}", path.display()))?;
        }
        Ok(())
    }

    pub fn archive_active_record(
        &self,
        change_id: &str,
        archive_stamp: &str,
    ) -> Result<Option<PathBuf>> {
        validate_change_id(change_id)?;
        let active_path = self.store.active_record_path(change_id)?;
        if !active_path.exists() {
            return Ok(None);
        }

        let archive_filename = format!("{}-{change_id}.toml", sanitize_stamp(archive_stamp));
        let archive_path = self.store.archive_dir().join(archive_filename);
        let archive_parent = archive_path.parent().with_context(|| {
            format!(
                "Archive path has no parent directory: {}",
                archive_path.display()
            )
        })?;
        fs::create_dir_all(archive_parent).with_context(|| {
            format!("Failed to create archive dir: {}", archive_parent.display())
        })?;

        fs::rename(&active_path, &archive_path).with_context(|| {
            format!(
                "Failed to archive change record: {} -> {}",
                active_path.display(),
                archive_path.display()
            )
        })?;

        Ok(Some(archive_path))
    }
}

fn validate_change_id(change_id: &str) -> Result<()> {
    if change_id.is_empty() {
        bail!("Change id cannot be empty");
    }
    if change_id
        .chars()
        .any(|ch| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
    {
        bail!(
            "Invalid change id '{change_id}': only ASCII letters, digits, '-' and '_' are allowed"
        );
    }
    Ok(())
}

fn sanitize_stamp(stamp: &str) -> String {
    stamp.replace(':', "-")
}

fn write_toml_atomic<T: Serialize>(path: &Path, value: &T, context_name: &str) -> Result<()> {
    let parent = path.parent().with_context(|| {
        format!(
            "Path has no parent directory for {context_name}: {}",
            path.display()
        )
    })?;
    fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create directory: {}", parent.display()))?;

    let tmp_path = temp_path_for(path);
    let encoded = toml::to_string_pretty(value)
        .with_context(|| format!("Failed to serialize {context_name}"))?;

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
        .with_context(|| format!("Failed to create temp file: {}", tmp_path.display()))?;
    file.write_all(encoded.as_bytes())
        .with_context(|| format!("Failed to write temp file: {}", tmp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("Failed to fsync temp file: {}", tmp_path.display()))?;
    drop(file);

    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "Failed to rename temp file for {context_name}: {} -> {}",
            tmp_path.display(),
            path.display()
        )
    })?;

    Ok(())
}

fn temp_path_for(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "changes".to_owned());
    let nonce = now_unix_millis();
    path.with_file_name(format!(".{file_name}.{nonce}.tmp"))
}

fn now_unix_secs() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(dur) => dur.as_secs(),
        Err(_) => 0,
    }
}

fn now_unix_millis() -> u128 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(dur) => dur.as_millis(),
        Err(_) => 0,
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct LockRecord {
    pid: u32,
    created_unix_secs: u64,
}

struct ChangesLockGuard {
    lock_path: PathBuf,
}

impl Drop for ChangesLockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.lock_path);
    }
}

fn acquire_lock(lock_path: &Path, operation: &str) -> Result<ChangesLockGuard> {
    let parent = lock_path.parent().with_context(|| {
        format!(
            "Lock path has no parent directory for operation '{operation}': {}",
            lock_path.display()
        )
    })?;
    fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create lock dir: {}", parent.display()))?;

    let start = Instant::now();
    let mut backoff = LOCK_INITIAL_BACKOFF;

    loop {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
        {
            Ok(mut file) => {
                let lock_record = LockRecord {
                    pid: std::process::id(),
                    created_unix_secs: now_unix_secs(),
                };
                let encoded = toml::to_string(&lock_record)
                    .context("Failed to serialize changes lock record")?;
                file.write_all(encoded.as_bytes()).with_context(|| {
                    format!("Failed to write lock file: {}", lock_path.display())
                })?;
                file.sync_all().with_context(|| {
                    format!("Failed to fsync lock file: {}", lock_path.display())
                })?;
                return Ok(ChangesLockGuard {
                    lock_path: lock_path.to_path_buf(),
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if try_break_stale_lock(lock_path)? {
                    continue;
                }
                if start.elapsed() >= LOCK_TIMEOUT {
                    bail!(
                        "Timed out acquiring changes lock for '{operation}' after {}s: {}\n  To fix: remove stale lock if no maw process is running: rm {}",
                        LOCK_TIMEOUT.as_secs(),
                        lock_path.display(),
                        lock_path.display()
                    );
                }
                thread::sleep(backoff);
                backoff = std::cmp::min(backoff.saturating_mul(2), LOCK_MAX_BACKOFF);
            }
            Err(err) => {
                bail!(
                    "Failed to acquire changes lock for '{operation}': {} ({err})",
                    lock_path.display()
                );
            }
        }
    }
}

fn try_break_stale_lock(lock_path: &Path) -> Result<bool> {
    let content = match fs::read_to_string(lock_path) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            bail!(
                "Failed to read lock file while checking staleness: {} ({err})",
                lock_path.display()
            );
        }
    };

    let record: LockRecord = match toml::from_str(&content) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(false),
    };

    let age_secs = now_unix_secs().saturating_sub(record.created_unix_secs);
    if age_secs < LOCK_STALE_AFTER.as_secs() {
        return Ok(false);
    }

    if pid_is_alive(record.pid) {
        return Ok(false);
    }

    fs::remove_file(lock_path)
        .with_context(|| format!("Failed to remove stale lock file: {}", lock_path.display()))?;
    tracing::warn!(
        lock_path = %lock_path.display(),
        pid = record.pid,
        age_secs,
        "Removed stale changes lock"
    );
    Ok(true)
}

#[cfg(target_os = "linux")]
fn pid_is_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(target_os = "linux"))]
fn pid_is_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_change(id: &str) -> ChangeRecord {
        ChangeRecord {
            schema_version: SCHEMA_VERSION,
            change_id: id.to_owned(),
            title: format!("Title for {id}"),
            state: ChangeState::Open,
            created_at: "2026-03-05T19:20:11Z".to_owned(),
            source: ChangeSource {
                from: "origin/main".to_owned(),
                from_oid: "abc123".to_owned(),
            },
            git: ChangeGit {
                base_branch: "main".to_owned(),
                change_branch: format!("feat/{id}"),
            },
            workspaces: ChangeWorkspaces {
                primary: id.to_owned(),
                linked: vec![id.to_owned()],
            },
            tracker: None,
            pr: None,
        }
    }

    #[test]
    fn missing_index_returns_default() {
        let temp = tempdir().expect("tempdir");
        let store = ChangesStore::open(temp.path());
        let index = store.read_index().expect("read index");
        assert_eq!(index, ChangeIndex::default());
    }

    #[test]
    fn index_roundtrip_under_lock() {
        let temp = tempdir().expect("tempdir");
        let store = ChangesStore::open(temp.path());

        store
            .with_lock("index write", |locked| {
                let mut index = ChangeIndex::default();
                index.set_branch_mapping("feat/ch-1xr", "ch-1xr");
                index.set_workspace_mapping("ch-1xr", "ch-1xr");
                locked.write_index(&index)
            })
            .expect("write index");

        let loaded = store.read_index().expect("read index");
        assert_eq!(loaded.change_for_branch("feat/ch-1xr"), Some("ch-1xr"));
        assert_eq!(loaded.change_for_workspace("ch-1xr"), Some("ch-1xr"));
    }

    #[test]
    fn change_roundtrip_and_list_sorted() {
        let temp = tempdir().expect("tempdir");
        let store = ChangesStore::open(temp.path());

        store
            .with_lock("write changes", |locked| {
                locked.write_active_record(&sample_change("ch-2ab"))?;
                locked.write_active_record(&sample_change("ch-1xr"))?;
                Ok(())
            })
            .expect("write active records");

        let listed = store.list_active_records().expect("list active");
        let ids: Vec<&str> = listed
            .iter()
            .map(|record| record.change_id.as_str())
            .collect();
        assert_eq!(ids, vec!["ch-1xr", "ch-2ab"]);

        let one = store
            .read_active_record("ch-1xr")
            .expect("read active")
            .expect("expected record");
        assert_eq!(one.change_id, "ch-1xr");
    }

    #[test]
    fn archive_moves_active_record() {
        let temp = tempdir().expect("tempdir");
        let store = ChangesStore::open(temp.path());

        store
            .with_lock("write change", |locked| {
                locked.write_active_record(&sample_change("ch-1xr"))
            })
            .expect("write active record");

        let archived_path = store
            .with_lock("archive change", |locked| {
                locked.archive_active_record("ch-1xr", "2026-03-05T20:11:00Z")
            })
            .expect("archive op")
            .expect("archive path");

        assert!(archived_path.exists());
        assert!(
            store
                .read_active_record("ch-1xr")
                .expect("read active")
                .is_none()
        );
    }

    #[test]
    fn stale_dead_lock_is_broken() {
        let temp = tempdir().expect("tempdir");
        let store = ChangesStore::open(temp.path());
        let lock_path = store.changes_root().join(".lock");

        fs::create_dir_all(store.changes_root()).expect("create changes dir");
        let stale = LockRecord {
            pid: u32::MAX,
            created_unix_secs: now_unix_secs().saturating_sub(LOCK_STALE_AFTER.as_secs() + 5),
        };
        let encoded = toml::to_string(&stale).expect("serialize stale lock");
        fs::write(&lock_path, encoded).expect("write stale lock");

        store
            .with_lock("break stale", |_locked| Ok(()))
            .expect("acquire after stale lock");
    }

    #[test]
    fn rejects_invalid_change_id_for_paths() {
        let temp = tempdir().expect("tempdir");
        let store = ChangesStore::open(temp.path());
        let err = store
            .active_record_path("../oops")
            .expect_err("must reject path traversal");
        let msg = err.to_string();
        assert!(msg.contains("Invalid change id"));
    }
}
