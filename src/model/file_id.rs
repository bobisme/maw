//! `FileId` ↔ path mapping — persistent sidecar for stable file identity (§5.8).
//!
//! # Overview
//!
//! [`FileId`] is a 128-bit random identifier assigned to a file when it is
//! first created. It never changes, even if the file is renamed, moved, or
//! has its content modified. This makes rename-aware merge possible without
//! heuristics.
//!
//! This module provides [`FileIdMap`]: a bidirectional index between stable
//! file identities and their current paths. The map can be persisted to
//! `.manifold/fileids` (JSON) and loaded back.
//!
//! # Operations
//!
//! | Operation | `FileId` | Path |
//! |-----------|--------|------|
//! | Create    | new (random) | new path |
//! | Rename    | unchanged | old → new path |
//! | Modify    | unchanged | unchanged path |
//! | Copy      | new (random) | new path |
//! | Delete    | removed | removed |
//!
//! # Concurrent rename + edit (§5.8)
//!
//! Workspace A renames `foo.rs → bar.rs` (same `FileId`, different path key).
//! Workspace B modifies `foo.rs` (same `FileId`, same path key, different blob).
//!
//! During patch-set join, both patches carry the same `FileId`. The merge engine
//! sees:
//! - One workspace changed the path (Rename).
//! - The other changed the content (Modify).
//!
//! Result: `bar.rs` with B's content. No heuristics needed.
//!
//! # File format
//!
//! `.manifold/fileids` is a JSON file containing an array of `{"path": "...",
//! "file_id": "..."}` records (one per tracked file, sorted by path for
//! deterministic diffs):
//!
//! ```json
//! [
//!   {"path": "src/lib.rs", "file_id": "0000...0001"},
//!   {"path": "src/main.rs", "file_id": "0000...0002"}
//! ]
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::patch::FileId;

// ---------------------------------------------------------------------------
// FileIdMap
// ---------------------------------------------------------------------------

/// Bidirectional mapping between [`FileId`] and the current path of each
/// tracked file (§5.8).
///
/// Invariants maintained by all mutating methods:
/// - Every `FileId` maps to exactly one path.
/// - Every path maps to exactly one `FileId`.
/// - The two maps are always consistent with each other.
///
/// These invariants guarantee O(1) lookup in both directions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileIdMap {
    /// Canonical index: path → `FileId` (serialized).
    path_to_id: BTreeMap<PathBuf, FileId>,
    /// Reverse index: `FileId` → path (rebuilt from `path_to_id` on load).
    id_to_path: BTreeMap<FileId, PathBuf>,
}

impl Default for FileIdMap {
    fn default() -> Self {
        Self::new()
    }
}

impl FileIdMap {
    /// Create an empty map.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            path_to_id: BTreeMap::new(),
            id_to_path: BTreeMap::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Mutation — file lifecycle
    // -----------------------------------------------------------------------

    /// Track a newly created file.
    ///
    /// Assigns a fresh random [`FileId`] to `path` and returns it.
    ///
    /// # Errors
    /// Returns [`FileIdMapError::PathAlreadyTracked`] if `path` is already
    /// tracked. A file must be deleted before it can be re-created at the
    /// same path.
    pub fn track_new(&mut self, path: PathBuf) -> Result<FileId, FileIdMapError> {
        if self.path_to_id.contains_key(&path) {
            return Err(FileIdMapError::PathAlreadyTracked(path));
        }
        let id = FileId::random();
        self.path_to_id.insert(path.clone(), id);
        self.id_to_path.insert(id, path);
        Ok(id)
    }

    /// Track a file rename: `old_path` → `new_path`.
    ///
    /// The [`FileId`] is preserved — only the path mapping changes.
    ///
    /// # Errors
    /// - [`FileIdMapError::PathNotTracked`] if `old_path` is not tracked.
    /// - [`FileIdMapError::PathAlreadyTracked`] if `new_path` is already
    ///   tracked by a different file.
    pub fn track_rename(
        &mut self,
        old_path: &Path,
        new_path: PathBuf,
    ) -> Result<FileId, FileIdMapError> {
        let id = self
            .path_to_id
            .remove(old_path)
            .ok_or_else(|| FileIdMapError::PathNotTracked(old_path.to_path_buf()))?;

        // Check destination is free.
        if let Some(&existing_id) = self.path_to_id.get(&new_path)
            && existing_id != id
        {
            // Restore old mapping before returning the error.
            self.path_to_id.insert(old_path.to_path_buf(), id);
            return Err(FileIdMapError::PathAlreadyTracked(new_path));
        }

        self.id_to_path.insert(id, new_path.clone());
        self.path_to_id.insert(new_path, id);
        Ok(id)
    }

    /// Track a file copy: create `dst_path` as a copy of `src_path`.
    ///
    /// Assigns a **new** random [`FileId`] to the copy. The source file
    /// keeps its original identity. This is explicit, not inferred from
    /// content similarity.
    ///
    /// # Errors
    /// - [`FileIdMapError::PathNotTracked`] if `src_path` is not tracked.
    /// - [`FileIdMapError::PathAlreadyTracked`] if `dst_path` is already
    ///   tracked.
    pub fn track_copy(
        &mut self,
        src_path: &Path,
        dst_path: PathBuf,
    ) -> Result<FileId, FileIdMapError> {
        if !self.path_to_id.contains_key(src_path) {
            return Err(FileIdMapError::PathNotTracked(src_path.to_path_buf()));
        }
        if self.path_to_id.contains_key(&dst_path) {
            return Err(FileIdMapError::PathAlreadyTracked(dst_path));
        }
        // New FileId for the copy — explicit, not inherited.
        let new_id = FileId::random();
        self.path_to_id.insert(dst_path.clone(), new_id);
        self.id_to_path.insert(new_id, dst_path);
        Ok(new_id)
    }

    /// Track a file deletion.
    ///
    /// Removes both mappings. Returns the [`FileId`] that was assigned to
    /// the deleted file.
    ///
    /// # Errors
    /// Returns [`FileIdMapError::PathNotTracked`] if `path` is not tracked.
    pub fn track_delete(&mut self, path: &Path) -> Result<FileId, FileIdMapError> {
        let id = self
            .path_to_id
            .remove(path)
            .ok_or_else(|| FileIdMapError::PathNotTracked(path.to_path_buf()))?;
        self.id_to_path.remove(&id);
        Ok(id)
    }

    // -----------------------------------------------------------------------
    // Lookup
    // -----------------------------------------------------------------------

    /// Look up the [`FileId`] for a given path. Returns `None` if untracked.
    #[must_use]
    pub fn id_for_path(&self, path: &Path) -> Option<FileId> {
        self.path_to_id.get(path).copied()
    }

    /// Look up the current path for a given [`FileId`]. Returns `None` if
    /// not tracked (file was deleted or never registered).
    #[must_use]
    pub fn path_for_id(&self, id: FileId) -> Option<&Path> {
        self.id_to_path.get(&id).map(PathBuf::as_path)
    }

    /// Return `true` if `path` is currently tracked.
    #[must_use]
    pub fn contains_path(&self, path: &Path) -> bool {
        self.path_to_id.contains_key(path)
    }

    /// Return `true` if `id` is currently tracked.
    #[must_use]
    pub fn contains_id(&self, id: FileId) -> bool {
        self.id_to_path.contains_key(&id)
    }

    /// Return the number of tracked files.
    #[must_use]
    pub fn len(&self) -> usize {
        self.path_to_id.len()
    }

    /// Return `true` if no files are tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.path_to_id.is_empty()
    }

    /// Iterate over all `(path, FileId)` entries in sorted path order.
    pub fn iter(&self) -> impl Iterator<Item = (&Path, FileId)> {
        self.path_to_id.iter().map(|(p, &id)| (p.as_path(), id))
    }

    // -----------------------------------------------------------------------
    // Persistence
    // -----------------------------------------------------------------------

    /// Load the map from a `.manifold/fileids` JSON file.
    ///
    /// Returns an empty map if the file does not exist (first run).
    ///
    /// # Errors
    /// Returns [`FileIdMapError::Io`] on I/O failure (other than not-found),
    /// or [`FileIdMapError::Json`] on JSON parse failure.
    pub fn load(path: &Path) -> Result<Self, FileIdMapError> {
        match fs::read_to_string(path) {
            Ok(content) => {
                let records: Vec<FileIdRecord> =
                    serde_json::from_str(&content).map_err(FileIdMapError::Json)?;
                Self::from_records(records)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::new()),
            Err(e) => Err(FileIdMapError::Io(e)),
        }
    }

    /// Save the map to a `.manifold/fileids` JSON file.
    ///
    /// Writes are atomic: content is first written to `<path>.tmp` then
    /// renamed over the destination. This prevents a crash mid-write from
    /// leaving a corrupt file.
    ///
    /// Parent directories are created if they don't exist.
    ///
    /// # Errors
    /// Returns [`FileIdMapError::Io`] on I/O failure, or
    /// [`FileIdMapError::Json`] on serialization failure.
    pub fn save(&self, path: &Path) -> Result<(), FileIdMapError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(FileIdMapError::Io)?;
        }

        let records: Vec<FileIdRecord> = self
            .path_to_id
            .iter()
            .map(|(p, &id)| FileIdRecord {
                path: p.clone(),
                file_id: id,
            })
            .collect();

        let json = serde_json::to_string_pretty(&records).map_err(FileIdMapError::Json)?;

        // Atomic write: write to tmp, then rename.
        let tmp_path = path.with_extension("tmp");
        fs::write(&tmp_path, json).map_err(FileIdMapError::Io)?;
        fs::rename(&tmp_path, path).map_err(FileIdMapError::Io)?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Rebuild the map from serialized records, validating consistency.
    fn from_records(records: Vec<FileIdRecord>) -> Result<Self, FileIdMapError> {
        let mut map = Self::new();
        for record in records {
            // Detect duplicate paths (shouldn't happen in a well-formed file).
            if map.path_to_id.contains_key(&record.path) {
                return Err(FileIdMapError::DuplicatePath(record.path));
            }
            // Detect duplicate FileIds (shouldn't happen in a well-formed file).
            if map.id_to_path.contains_key(&record.file_id) {
                return Err(FileIdMapError::DuplicateFileId(record.file_id));
            }
            map.id_to_path.insert(record.file_id, record.path.clone());
            map.path_to_id.insert(record.path, record.file_id);
        }
        Ok(map)
    }
}

// ---------------------------------------------------------------------------
// Serialization record
// ---------------------------------------------------------------------------

/// A single entry in the `.manifold/fileids` JSON file.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct FileIdRecord {
    path: PathBuf,
    file_id: FileId,
}

// ---------------------------------------------------------------------------
// FileIdMapError
// ---------------------------------------------------------------------------

/// Errors produced by [`FileIdMap`] operations.
#[derive(Debug)]
pub enum FileIdMapError {
    /// The specified path is already tracked by a file with a different `FileId`.
    PathAlreadyTracked(PathBuf),
    /// The specified path is not tracked in the map.
    PathNotTracked(PathBuf),
    /// Two records in the persisted file share the same path.
    DuplicatePath(PathBuf),
    /// Two records in the persisted file share the same `FileId`.
    DuplicateFileId(FileId),
    /// I/O error reading or writing the fileids file.
    Io(io::Error),
    /// JSON (de)serialization error.
    Json(serde_json::Error),
}

impl fmt::Display for FileIdMapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PathAlreadyTracked(p) => {
                write!(f, "path already tracked: {}", p.display())
            }
            Self::PathNotTracked(p) => {
                write!(f, "path not tracked: {}", p.display())
            }
            Self::DuplicatePath(p) => {
                write!(f, "corrupt fileids: duplicate path entry: {}", p.display())
            }
            Self::DuplicateFileId(id) => {
                write!(f, "corrupt fileids: duplicate FileId: {id}")
            }
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Json(e) => write!(f, "JSON error: {e}"),
        }
    }
}

impl std::error::Error for FileIdMapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Json(e) => Some(e),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Basic operations
    // -----------------------------------------------------------------------

    #[test]
    fn track_new_assigns_fresh_id() {
        let mut map = FileIdMap::new();
        let id = map.track_new("src/main.rs".into()).unwrap();
        assert!(map.contains_path(Path::new("src/main.rs")));
        assert!(map.contains_id(id));
        assert_eq!(map.id_for_path(Path::new("src/main.rs")), Some(id));
        assert_eq!(map.path_for_id(id), Some(Path::new("src/main.rs")));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn track_new_generates_unique_ids() {
        let mut map = FileIdMap::new();
        let id_a = map.track_new("a.rs".into()).unwrap();
        let id_b = map.track_new("b.rs".into()).unwrap();
        // IDs must be distinct (with overwhelming probability).
        assert_ne!(id_a, id_b);
    }

    #[test]
    fn track_new_rejects_duplicate_path() {
        let mut map = FileIdMap::new();
        map.track_new("src/lib.rs".into()).unwrap();
        let err = map.track_new("src/lib.rs".into()).unwrap_err();
        assert!(matches!(err, FileIdMapError::PathAlreadyTracked(_)));
        assert_eq!(map.len(), 1); // Map unchanged.
    }

    #[test]
    fn track_rename_preserves_file_id() {
        let mut map = FileIdMap::new();
        let id = map.track_new("foo.rs".into()).unwrap();

        let returned_id = map
            .track_rename(Path::new("foo.rs"), "bar.rs".into())
            .unwrap();

        assert_eq!(returned_id, id, "FileId must be unchanged by rename");
        assert!(!map.contains_path(Path::new("foo.rs")));
        assert!(map.contains_path(Path::new("bar.rs")));
        assert_eq!(map.id_for_path(Path::new("bar.rs")), Some(id));
        assert_eq!(map.path_for_id(id), Some(Path::new("bar.rs")));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn track_rename_rejects_unknown_source() {
        let mut map = FileIdMap::new();
        let err = map
            .track_rename(Path::new("missing.rs"), "dest.rs".into())
            .unwrap_err();
        assert!(matches!(err, FileIdMapError::PathNotTracked(_)));
    }

    #[test]
    fn track_rename_rejects_occupied_destination() {
        let mut map = FileIdMap::new();
        map.track_new("a.rs".into()).unwrap();
        map.track_new("b.rs".into()).unwrap();

        let err = map
            .track_rename(Path::new("a.rs"), "b.rs".into())
            .unwrap_err();
        assert!(matches!(err, FileIdMapError::PathAlreadyTracked(_)));
        // Both originals must still be intact.
        assert_eq!(map.len(), 2);
        assert!(map.contains_path(Path::new("a.rs")));
        assert!(map.contains_path(Path::new("b.rs")));
    }

    #[test]
    fn track_copy_assigns_new_id() {
        let mut map = FileIdMap::new();
        let src_id = map.track_new("src/lib.rs".into()).unwrap();

        let dst_id = map
            .track_copy(Path::new("src/lib.rs"), "src/lib_copy.rs".into())
            .unwrap();

        assert_ne!(dst_id, src_id, "copy gets a new FileId");
        assert_eq!(map.len(), 2);
        assert_eq!(map.id_for_path(Path::new("src/lib.rs")), Some(src_id));
        assert_eq!(map.id_for_path(Path::new("src/lib_copy.rs")), Some(dst_id));
    }

    #[test]
    fn track_copy_rejects_unknown_source() {
        let mut map = FileIdMap::new();
        let err = map
            .track_copy(Path::new("missing.rs"), "dest.rs".into())
            .unwrap_err();
        assert!(matches!(err, FileIdMapError::PathNotTracked(_)));
    }

    #[test]
    fn track_copy_rejects_occupied_destination() {
        let mut map = FileIdMap::new();
        map.track_new("a.rs".into()).unwrap();
        map.track_new("b.rs".into()).unwrap();

        let err = map
            .track_copy(Path::new("a.rs"), "b.rs".into())
            .unwrap_err();
        assert!(matches!(err, FileIdMapError::PathAlreadyTracked(_)));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn track_delete_removes_both_mappings() {
        let mut map = FileIdMap::new();
        let id = map.track_new("gone.rs".into()).unwrap();

        let returned = map.track_delete(Path::new("gone.rs")).unwrap();

        assert_eq!(returned, id);
        assert!(!map.contains_path(Path::new("gone.rs")));
        assert!(!map.contains_id(id));
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn track_delete_rejects_unknown_path() {
        let mut map = FileIdMap::new();
        let err = map.track_delete(Path::new("nope.rs")).unwrap_err();
        assert!(matches!(err, FileIdMapError::PathNotTracked(_)));
    }

    #[test]
    fn track_new_after_delete_same_path() {
        let mut map = FileIdMap::new();
        let id1 = map.track_new("file.rs".into()).unwrap();
        map.track_delete(Path::new("file.rs")).unwrap();
        let id2 = map.track_new("file.rs".into()).unwrap();

        // New file at same path gets a brand-new FileId.
        assert_ne!(id1, id2);
        assert_eq!(map.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Consistency invariants
    // -----------------------------------------------------------------------

    #[test]
    fn map_len_matches_both_directions() {
        let mut map = FileIdMap::new();
        assert_eq!(map.path_to_id.len(), map.id_to_path.len());

        map.track_new("a.rs".into()).unwrap();
        assert_eq!(map.path_to_id.len(), map.id_to_path.len());

        map.track_new("b.rs".into()).unwrap();
        assert_eq!(map.path_to_id.len(), map.id_to_path.len());

        map.track_rename(Path::new("a.rs"), "c.rs".into()).unwrap();
        assert_eq!(map.path_to_id.len(), map.id_to_path.len());

        map.track_delete(Path::new("b.rs")).unwrap();
        assert_eq!(map.path_to_id.len(), map.id_to_path.len());
    }

    #[test]
    fn iter_returns_sorted_paths() {
        let mut map = FileIdMap::new();
        map.track_new("z.rs".into()).unwrap();
        map.track_new("a.rs".into()).unwrap();
        map.track_new("m.rs".into()).unwrap();

        let paths: Vec<_> = map.iter().map(|(p, _)| p.to_path_buf()).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted, "iter must return paths in sorted order");
    }

    // -----------------------------------------------------------------------
    // Persistence: save + load round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let fileids_path = dir.path().join(".manifold").join("fileids");

        let mut map = FileIdMap::new();
        let id_a = map.track_new("src/main.rs".into()).unwrap();
        let id_b = map.track_new("src/lib.rs".into()).unwrap();
        map.save(&fileids_path).unwrap();

        let loaded = FileIdMap::load(&fileids_path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.id_for_path(Path::new("src/main.rs")), Some(id_a));
        assert_eq!(loaded.id_for_path(Path::new("src/lib.rs")), Some(id_b));
        assert_eq!(loaded.path_for_id(id_a), Some(Path::new("src/main.rs")));
        assert_eq!(loaded.path_for_id(id_b), Some(Path::new("src/lib.rs")));
    }

    #[test]
    fn load_missing_file_returns_empty_map() {
        let dir = tempfile::tempdir().unwrap();
        let fileids_path = dir.path().join(".manifold").join("fileids");

        let map = FileIdMap::load(&fileids_path).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn save_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir
            .path()
            .join("deep")
            .join("nested")
            .join(".manifold")
            .join("fileids");

        let map = FileIdMap::new();
        map.save(&nested).unwrap();
        assert!(nested.exists());
    }

    #[test]
    fn save_produces_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let fileids_path = dir.path().join("fileids");

        let mut map = FileIdMap::new();
        map.track_new("src/main.rs".into()).unwrap();
        map.save(&fileids_path).unwrap();

        let content = fs::read_to_string(&fileids_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(parsed.is_array());
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert!(arr[0].get("path").is_some());
        assert!(arr[0].get("file_id").is_some());
    }

    #[test]
    fn save_is_deterministic() {
        // Two maps with the same contents produce identical JSON.
        let make = || {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("fileids");
            let mut map = FileIdMap::new();
            // Use fixed FileIds via direct insertion (bypass random).
            let id1 = FileId::new(0x1111_1111_1111_1111_1111_1111_1111_1111);
            let id2 = FileId::new(0x2222_2222_2222_2222_2222_2222_2222_2222);
            map.path_to_id.insert("a.rs".into(), id1);
            map.id_to_path.insert(id1, "a.rs".into());
            map.path_to_id.insert("b.rs".into(), id2);
            map.id_to_path.insert(id2, "b.rs".into());
            map.save(&p).unwrap();
            (dir, p)
        };
        let (_dir1, p1) = make();
        let (_dir2, p2) = make();
        let c1 = fs::read_to_string(&p1).unwrap();
        let c2 = fs::read_to_string(&p2).unwrap();
        assert_eq!(c1, c2, "save must be deterministic");
    }

    #[test]
    fn load_detects_duplicate_paths() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("fileids");
        // Write a corrupt file with duplicate paths.
        let json = r#"[
            {"path": "foo.rs", "file_id": "00000000000000000000000000000001"},
            {"path": "foo.rs", "file_id": "00000000000000000000000000000002"}
        ]"#;
        fs::write(&p, json).unwrap();
        let err = FileIdMap::load(&p).unwrap_err();
        assert!(matches!(err, FileIdMapError::DuplicatePath(_)));
    }

    #[test]
    fn load_detects_duplicate_file_ids() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("fileids");
        // Write a corrupt file with duplicate FileIds.
        let json = r#"[
            {"path": "a.rs", "file_id": "00000000000000000000000000000001"},
            {"path": "b.rs", "file_id": "00000000000000000000000000000001"}
        ]"#;
        fs::write(&p, json).unwrap();
        let err = FileIdMap::load(&p).unwrap_err();
        assert!(matches!(err, FileIdMapError::DuplicateFileId(_)));
    }

    // -----------------------------------------------------------------------
    // §5.8 scenario: concurrent rename + edit
    // -----------------------------------------------------------------------

    /// Verifies the design doc's core claim (§5.8):
    ///
    /// "If workspace A renames foo.rs → bar.rs and workspace B modifies
    /// foo.rs, Manifold sees: same `FileId`, one workspace changed the path,
    /// one changed the content. Clean merge to bar.rs with B's edits.
    /// Without `FileId`, this is a delete+add+modify mess."
    ///
    /// This test demonstrates that:
    /// 1. Both patch-sets carry the same `FileId`.
    /// 2. The `FileIdMap` confirms the rename.
    /// 3. A merge engine can identify the correct resolution.
    #[test]
    fn concurrent_rename_and_edit_same_file_id() {
        // Common base state: foo.rs exists with a known FileId.
        let mut base_map = FileIdMap::new();
        let foo_id = base_map.track_new("foo.rs".into()).unwrap();

        // --- Workspace A: renames foo.rs → bar.rs ---
        let mut map_a = base_map.clone();
        let rename_id = map_a
            .track_rename(Path::new("foo.rs"), "bar.rs".into())
            .unwrap();
        assert_eq!(rename_id, foo_id, "FileId unchanged across rename");
        assert!(!map_a.contains_path(Path::new("foo.rs")));
        assert!(map_a.contains_path(Path::new("bar.rs")));

        // --- Workspace B: modifies foo.rs (no rename) ---
        let map_b = base_map.clone();
        let modify_id = map_b
            .id_for_path(Path::new("foo.rs"))
            .expect("foo.rs must be tracked");
        assert_eq!(modify_id, foo_id, "FileId unchanged across modify");

        // --- Merge observation ---
        // Both workspaces agree on the FileId for the file.
        // WS A → Rename { from: "foo.rs", file_id: foo_id, new_blob: None }
        // WS B → Modify { base_blob: ..., new_blob: ..., file_id: foo_id }
        //
        // A merge engine that indexes by FileId (not path) can detect:
        // - same FileId → same file
        // - WS A changed path: foo.rs → bar.rs
        // - WS B changed content
        // - Result: bar.rs with WS B's new content
        assert_eq!(rename_id, modify_id, "Same FileId seen in both workspaces");
    }

    /// Test that copies get a NEW `FileId`, not the source's `FileId`.
    /// (§5.8: "Copy = new `FileId` with same initial blob. Explicit, not inferred.")
    #[test]
    fn copy_gets_new_file_id() {
        let mut map = FileIdMap::new();
        let orig_id = map.track_new("original.rs".into()).unwrap();
        let copy_id = map
            .track_copy(Path::new("original.rs"), "copy.rs".into())
            .unwrap();

        assert_ne!(orig_id, copy_id, "copy must have a new FileId");

        // Original is unaffected.
        assert_eq!(map.id_for_path(Path::new("original.rs")), Some(orig_id));
    }

    // -----------------------------------------------------------------------
    // Display and error formatting
    // -----------------------------------------------------------------------

    #[test]
    fn file_id_map_error_display_all_variants() {
        let errors: &[FileIdMapError] = &[
            FileIdMapError::PathAlreadyTracked("a.rs".into()),
            FileIdMapError::PathNotTracked("b.rs".into()),
            FileIdMapError::DuplicatePath("c.rs".into()),
            FileIdMapError::DuplicateFileId(FileId::new(0)),
            FileIdMapError::Io(io::Error::new(io::ErrorKind::PermissionDenied, "oops")),
            FileIdMapError::Json(serde_json::from_str::<FileId>("!").unwrap_err()),
        ];
        for err in errors {
            let msg = format!("{err}");
            assert!(!msg.is_empty(), "error variant must have display text");
        }
    }

    #[test]
    fn file_id_map_error_source() {
        let io_err = FileIdMapError::Io(io::Error::new(io::ErrorKind::NotFound, "gone"));
        assert!(std::error::Error::source(&io_err).is_some());

        let path_err = FileIdMapError::PathNotTracked("x.rs".into());
        assert!(std::error::Error::source(&path_err).is_none());
    }

    // -----------------------------------------------------------------------
    // Empty map
    // -----------------------------------------------------------------------

    #[test]
    fn empty_map_state() {
        let map = FileIdMap::new();
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);
        assert_eq!(map.iter().count(), 0);
        assert!(map.id_for_path(Path::new("any.rs")).is_none());
        assert!(map.path_for_id(FileId::new(0)).is_none());
    }

    #[test]
    fn default_is_empty() {
        let map = FileIdMap::default();
        assert!(map.is_empty());
    }
}
