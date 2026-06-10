//! PARTITION step of the N-way merge pipeline.
//!
//! Given a list of [`PatchSet`]s (one per workspace), builds an inverted index
//! from path → list of workspace changes. Then partitions paths into:
//!
//! - **Unique paths**: touched by exactly 1 workspace → can be applied directly.
//! - **Shared paths**: touched by 2+ workspaces → need conflict resolution.
//!
//! Paths are always sorted lexicographically for determinism.
//!
//! # Example
//!
//! ```text
//! Workspace A: adds foo.rs, modifies bar.rs
//! Workspace B: modifies bar.rs, deletes baz.rs
//!
//! Inverted index:
//!   foo.rs → [(A, Added)]
//!   bar.rs → [(A, Modified), (B, Modified)]
//!   baz.rs → [(B, Deleted)]
//!
//! Partition:
//!   unique: [baz.rs → (B, Deleted), foo.rs → (A, Added)]
//!   shared: [bar.rs → [(A, Modified), (B, Modified)]]
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::model::patch::FileId;
use crate::model::types::{GitOid, WorkspaceId};

use super::types::{ChangeKind, EntryMode, PatchSet};

// ---------------------------------------------------------------------------
// PathEntry
// ---------------------------------------------------------------------------

/// A single workspace's change to a particular file path.
///
/// Stored as entries in the inverted index. For non-deletions, `content`
/// holds the new file content. For deletions, `content` is `None`.
///
/// `file_id` carries the stable [`FileId`] from the collect step (§5.8).
/// When populated, the resolve step can group renames correctly — two entries
/// with the same `FileId` but different paths represent a rename + content
/// change rather than an independent add/delete pair.
///
/// `blob` is the git blob OID for the new content. The resolve step prefers
/// OID equality (`blob == blob`) over byte-level content comparison.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathEntry {
    /// The workspace that made this change.
    pub workspace_id: WorkspaceId,
    /// What kind of change was made.
    pub kind: ChangeKind,
    /// New file content (`None` for deletions).
    pub content: Option<Vec<u8>>,
    /// Stable file identity (§5.8). `None` for legacy/test paths without tracking.
    pub file_id: Option<FileId>,
    /// Git blob OID for the new content (Add/Modify only; `None` for Delete
    /// and paths collected without git access).
    pub blob: Option<GitOid>,
    /// Git tree-entry mode captured from the source workspace (executable
    /// bit / symlink / regular). `None` for legacy/test paths without mode
    /// info. bn-1tl6: threaded through so `build_merge_commit` can write the
    /// correct mode into the committed merge tree.
    pub mode: Option<EntryMode>,
}

impl PathEntry {
    /// Create a `PathEntry` without identity metadata (Phase 1 compat).
    #[must_use]
    pub const fn new(
        workspace_id: WorkspaceId,
        kind: ChangeKind,
        content: Option<Vec<u8>>,
    ) -> Self {
        Self {
            workspace_id,
            kind,
            content,
            file_id: None,
            blob: None,
            mode: None,
        }
    }

    /// Create a `PathEntry` with full identity metadata (Phase 3+) but no
    /// mode. Prefer [`PathEntry::with_mode`] on the production collect path.
    #[must_use]
    pub const fn with_identity(
        workspace_id: WorkspaceId,
        kind: ChangeKind,
        content: Option<Vec<u8>>,
        file_id: Option<FileId>,
        blob: Option<GitOid>,
    ) -> Self {
        Self {
            workspace_id,
            kind,
            content,
            file_id,
            blob,
            mode: None,
        }
    }

    /// Create a `PathEntry` with full identity metadata *and* a git
    /// tree-entry mode (bn-1tl6). Used by `partition_by_path` so the mode
    /// captured at collect time survives into `build_merge_commit`.
    #[must_use]
    pub const fn with_mode(
        workspace_id: WorkspaceId,
        kind: ChangeKind,
        content: Option<Vec<u8>>,
        file_id: Option<FileId>,
        blob: Option<GitOid>,
        mode: Option<EntryMode>,
    ) -> Self {
        Self {
            workspace_id,
            kind,
            content,
            file_id,
            blob,
            mode,
        }
    }

    /// Returns `true` if this entry is a deletion.
    #[must_use]
    pub const fn is_deletion(&self) -> bool {
        matches!(self.kind, ChangeKind::Deleted)
    }
}

// ---------------------------------------------------------------------------
// PartitionResult
// ---------------------------------------------------------------------------

/// A D/F (Directory/File) path clash detected during partition (bn-2dy1).
///
/// Records the two incompatible sides so the resolve step can emit a
/// structured `ConflictReason::FileDirectory` conflict without silently
/// dropping either side.
#[derive(Clone, Debug)]
pub struct DfClash {
    /// The path that is a FILE in `file_ws`.
    pub file_path: PathBuf,
    /// Workspace that contributed the FILE at `file_path`.
    pub file_ws: WorkspaceId,
    /// An example path under the directory side (P/...) that makes
    /// `file_path` structurally incompatible as a file name.
    pub dir_child_example: PathBuf,
    /// Workspace that contributed a file under `file_path/`.
    pub dir_ws: WorkspaceId,
}

/// The result of partitioning patch-sets by path.
///
/// Paths are sorted lexicographically in both `unique` and `shared` for
/// determinism.
#[derive(Clone, Debug)]
pub struct PartitionResult {
    /// Paths touched by exactly 1 workspace. These can be applied directly
    /// without conflict resolution.
    ///
    /// Each entry maps a path to the single workspace change.
    pub unique: Vec<(PathBuf, PathEntry)>,

    /// Paths touched by 2+ workspaces. These need conflict resolution
    /// (hash equality check, diff3 merge, or conflict reporting).
    ///
    /// Each entry maps a path to all workspace changes for that path.
    /// The inner `Vec` is sorted by workspace ID for determinism.
    pub shared: Vec<(PathBuf, Vec<PathEntry>)>,

    /// D/F path clashes detected during partition (bn-2dy1).
    ///
    /// When path P is a FILE in one workspace while another workspace has
    /// files under P/ (treating P as a directory), both are structurally
    /// incompatible. The resolve step must emit `ConflictReason::FileDirectory`
    /// conflicts for every path that participates in a clash (both the FILE
    /// path and the directory-side paths) rather than applying them silently.
    ///
    /// Paths that participate in a D/F clash remain in `unique` or `shared`
    /// but should be intercepted by the resolve step via this field.
    pub df_clashes: Vec<DfClash>,
}

impl PartitionResult {
    /// Total count of unique paths.
    #[must_use]
    pub const fn unique_count(&self) -> usize {
        self.unique.len()
    }

    /// Total count of shared (potentially conflicting) paths.
    #[must_use]
    pub const fn shared_count(&self) -> usize {
        self.shared.len()
    }

    /// Total count of all paths across unique and shared.
    #[must_use]
    pub const fn total_path_count(&self) -> usize {
        self.unique.len() + self.shared.len()
    }

    /// Returns `true` if there are no shared paths and no D/F clashes
    /// (no conflicts possible).
    #[must_use]
    pub const fn is_conflict_free(&self) -> bool {
        self.shared.is_empty() && self.df_clashes.is_empty()
    }

    /// Returns the set of paths that participate in a D/F clash (both
    /// the FILE-side path and every directory-child path under it).
    ///
    /// The resolve step uses this set to skip normal resolution for these
    /// paths and emit [`ConflictReason::FileDirectory`] conflicts instead.
    #[must_use]
    pub fn df_clash_paths(&self) -> std::collections::HashSet<PathBuf> {
        let mut set = std::collections::HashSet::new();
        for clash in &self.df_clashes {
            set.insert(clash.file_path.clone());
            set.insert(clash.dir_child_example.clone());
        }
        set
    }
}

// ---------------------------------------------------------------------------
// partition_by_path
// ---------------------------------------------------------------------------

/// Partition a set of workspace patch-sets into unique and shared paths.
///
/// Builds an inverted index from path → workspace changes, then splits
/// paths into those touched by exactly 1 workspace (unique) and those
/// touched by 2+ workspaces (shared).
///
/// **D/F (Directory/File) clash detection** (bn-2dy1): after the initial
/// partition, any path P in the index that is a component-wise prefix of
/// another path Q in the index represents a structural conflict: P is a FILE
/// in one workspace while P/... is a directory in another (or the epoch).
/// These clashes are promoted to the `shared` bucket so the resolve step can
/// surface them as conflicts rather than silently applying both.
///
/// # Determinism
///
/// - Paths are processed in lexicographic order (via [`BTreeMap`]).
/// - Within shared paths, entries are sorted by workspace ID.
/// - Empty patch-sets are silently ignored (they contribute no paths).
///
/// # Arguments
///
/// * `patch_sets` — One `PatchSet` per workspace (from the collect step).
///
/// # Returns
///
/// A [`PartitionResult`] with unique and shared paths.
#[must_use]
pub fn partition_by_path(patch_sets: &[PatchSet]) -> PartitionResult {
    // Build inverted index using BTreeMap for lexicographic ordering.
    let mut index: BTreeMap<PathBuf, Vec<PathEntry>> = BTreeMap::new();

    for ps in patch_sets {
        for change in &ps.changes {
            // Propagate FileId and blob OID from FileChange so that the
            // resolve step can use OID equality and FileId-based rename
            // tracking (§5.8).
            let entry = PathEntry::with_mode(
                ps.workspace_id.clone(),
                change.kind.clone(),
                change.content.clone(),
                change.file_id,
                change.blob.clone(),
                change.mode,
            );
            index.entry(change.path.clone()).or_default().push(entry);
        }
    }

    // Partition into unique and shared.
    let mut unique = Vec::new();
    let mut shared = Vec::new();

    for (path, mut entries) in index {
        if entries.len() == 1 {
            // Unique: exactly 1 workspace touched this path.
            unique.push((path, entries.remove(0)));
        } else {
            // Shared: 2+ workspaces touched this path.
            // Sort by workspace ID for determinism.
            entries.sort_by(|a, b| a.workspace_id.as_str().cmp(b.workspace_id.as_str()));
            shared.push((path, entries));
        }
    }

    // Paths are already sorted (BTreeMap iterates in order).

    // bn-2dy1: D/F clash detection.
    //
    // After the initial partition, a workspace might have added FILE `clash`
    // (in unique) while another workspace added `clash/sub.txt` (also in
    // unique). These never collide by exact-path, so they both land in `unique`
    // — but they are structurally incompatible in a git tree.
    //
    // We detect these by building a lookup of path → workspace for all entries
    // and then checking every path P for a component-wise prefix relationship
    // with every other path Q: if Q = P/..., then P is a FILE in its workspace
    // while Q's workspace treats P as a directory.
    //
    // We emit a `DfClash` record for each incompatible pair and leave the
    // paths themselves untouched in `unique`/`shared`. The resolve step will
    // intercept them via `PartitionResult::df_clash_paths()` and emit
    // `ConflictReason::FileDirectory` conflicts.
    //
    // Build a lookup: path_string → (path, workspace_id) for all NON-DELETION
    // entries. Sorted for deterministic binary-search.
    //
    // Deletions never participate in a D/F clash: a `Deleted` entry means the
    // path will NOT occupy the result tree, so it cannot structurally collide
    // with anything. (E.g. a single workspace restructuring FILE↔DIR emits
    // `Deleted deep/a/leaf.txt` + `Added deep` in one patch — internally
    // consistent, NOT a clash.)
    let all_path_ws: Vec<(String, PathBuf, WorkspaceId)> = {
        let mut v: Vec<(String, PathBuf, WorkspaceId)> = unique
            .iter()
            .filter(|(_, e)| !e.is_deletion())
            .map(|(p, e)| {
                (
                    p.to_string_lossy().replace('\\', "/"),
                    p.clone(),
                    e.workspace_id.clone(),
                )
            })
            .chain(shared.iter().flat_map(|(p, entries)| {
                entries.iter().filter(|e| !e.is_deletion()).map(|e| {
                    (
                        p.to_string_lossy().replace('\\', "/"),
                        p.clone(),
                        e.workspace_id.clone(),
                    )
                })
            }))
            .collect();
        v.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        v
    };

    let all_path_strs: Vec<&str> = all_path_ws.iter().map(|(s, _, _)| s.as_str()).collect();

    let mut df_clashes: Vec<DfClash> = Vec::new();

    // For each unique non-deletion path, check directions 1 and 2.
    for (path, entry) in &unique {
        if entry.is_deletion() {
            continue;
        }
        let path_str = path.to_string_lossy().replace('\\', "/");
        let dir_prefix = format!("{path_str}/");

        // Direction 1: is there any other path under path_str/?
        // (path is FILE, Q = path_str/... exists → Q's ws treats path as dir)
        if let Some(child_row) = all_path_ws
            .iter()
            .find(|(s, _, _)| s != &path_str && s.starts_with(&dir_prefix))
        {
            df_clashes.push(DfClash {
                file_path: path.clone(),
                file_ws: entry.workspace_id.clone(),
                dir_child_example: child_row.1.clone(),
                dir_ws: child_row.2.clone(),
            });
        }

        // Direction 2: is some ancestor of path also a FILE in another ws?
        let mut ancestor = path.parent();
        while let Some(anc) = ancestor {
            if anc == std::path::Path::new("") {
                break;
            }
            let anc_str = anc.to_string_lossy().replace('\\', "/");
            // Binary search for the ancestor string.
            if let Ok(pos) = all_path_strs.binary_search(&anc_str.as_str()) {
                let (_, anc_path, anc_ws) = &all_path_ws[pos];
                // `path` is under `anc` — so `anc_ws` has a FILE at `anc`
                // while the current workspace has files under it.
                df_clashes.push(DfClash {
                    file_path: anc_path.clone(),
                    file_ws: anc_ws.clone(),
                    dir_child_example: path.clone(),
                    dir_ws: entry.workspace_id.clone(),
                });
                break;
            }
            ancestor = anc.parent();
        }
    }

    // De-duplicate clashes (same file_path may be found multiple times).
    df_clashes.sort_unstable_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.dir_child_example.cmp(&b.dir_child_example))
    });
    df_clashes
        .dedup_by(|a, b| a.file_path == b.file_path && a.dir_child_example == b.dir_child_example);

    PartitionResult {
        unique,
        shared,
        df_clashes,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
mod tests {
    use super::*;
    use crate::merge::types::{ChangeKind, FileChange, PatchSet};
    use crate::model::types::{EpochId, WorkspaceId};

    fn make_epoch() -> EpochId {
        EpochId::new(&"a".repeat(40)).expect("operation should succeed")
    }

    fn make_ws(name: &str) -> WorkspaceId {
        WorkspaceId::new(name).expect("operation should succeed")
    }

    fn make_change(path: &str, kind: ChangeKind, content: Option<&[u8]>) -> FileChange {
        FileChange::new(PathBuf::from(path), kind, content.map(<[u8]>::to_vec))
    }

    // -- Empty inputs --

    #[test]
    fn partition_empty_patch_sets() {
        let result = partition_by_path(&[]);
        assert_eq!(result.unique_count(), 0);
        assert_eq!(result.shared_count(), 0);
        assert_eq!(result.total_path_count(), 0);
        assert!(result.is_conflict_free());
    }

    #[test]
    fn partition_single_empty_workspace() {
        let ps = PatchSet::new(make_ws("ws-a"), make_epoch(), vec![]);
        let result = partition_by_path(&[ps]);
        assert_eq!(result.total_path_count(), 0);
        assert!(result.is_conflict_free());
    }

    // -- All unique (disjoint changes) --

    #[test]
    fn partition_disjoint_changes_all_unique() {
        let ps_a = PatchSet::new(
            make_ws("ws-a"),
            make_epoch(),
            vec![make_change("a.rs", ChangeKind::Added, Some(b"fn a() {}"))],
        );
        let ps_b = PatchSet::new(
            make_ws("ws-b"),
            make_epoch(),
            vec![make_change("b.rs", ChangeKind::Added, Some(b"fn b() {}"))],
        );

        let result = partition_by_path(&[ps_a, ps_b]);

        assert_eq!(result.unique_count(), 2);
        assert_eq!(result.shared_count(), 0);
        assert!(result.is_conflict_free());

        // Check paths are sorted lexicographically.
        let unique_paths: Vec<_> = result.unique.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(
            unique_paths,
            vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")]
        );

        // Check workspace IDs.
        assert_eq!(result.unique[0].1.workspace_id.as_str(), "ws-a");
        assert_eq!(result.unique[1].1.workspace_id.as_str(), "ws-b");
    }

    // -- All shared (same file modified by both) --

    #[test]
    fn partition_shared_path() {
        let ps_a = PatchSet::new(
            make_ws("ws-a"),
            make_epoch(),
            vec![make_change("shared.rs", ChangeKind::Modified, Some(b"a"))],
        );
        let ps_b = PatchSet::new(
            make_ws("ws-b"),
            make_epoch(),
            vec![make_change("shared.rs", ChangeKind::Modified, Some(b"b"))],
        );

        let result = partition_by_path(&[ps_a, ps_b]);

        assert_eq!(result.unique_count(), 0);
        assert_eq!(result.shared_count(), 1);
        assert!(!result.is_conflict_free());

        let (path, entries) = &result.shared[0];
        assert_eq!(path, &PathBuf::from("shared.rs"));
        assert_eq!(entries.len(), 2);
        // Entries sorted by workspace ID.
        assert_eq!(entries[0].workspace_id.as_str(), "ws-a");
        assert_eq!(entries[1].workspace_id.as_str(), "ws-b");
    }

    // -- Mix of unique and shared --

    #[test]
    fn partition_mixed_unique_and_shared() {
        let ps_a = PatchSet::new(
            make_ws("ws-a"),
            make_epoch(),
            vec![
                make_change("only-a.rs", ChangeKind::Added, Some(b"a")),
                make_change("shared.rs", ChangeKind::Modified, Some(b"ver-a")),
            ],
        );
        let ps_b = PatchSet::new(
            make_ws("ws-b"),
            make_epoch(),
            vec![
                make_change("only-b.rs", ChangeKind::Deleted, None),
                make_change("shared.rs", ChangeKind::Modified, Some(b"ver-b")),
            ],
        );

        let result = partition_by_path(&[ps_a, ps_b]);

        assert_eq!(result.unique_count(), 2);
        assert_eq!(result.shared_count(), 1);
        assert_eq!(result.total_path_count(), 3);

        // Unique paths sorted.
        let unique_paths: Vec<_> = result.unique.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(
            unique_paths,
            vec![PathBuf::from("only-a.rs"), PathBuf::from("only-b.rs")]
        );

        // Shared path.
        let (shared_path, entries) = &result.shared[0];
        assert_eq!(shared_path, &PathBuf::from("shared.rs"));
        assert_eq!(entries.len(), 2);
    }

    // -- 3-way shared path --

    #[test]
    fn partition_three_way_shared() {
        let ps_a = PatchSet::new(
            make_ws("ws-a"),
            make_epoch(),
            vec![make_change("config.toml", ChangeKind::Modified, Some(b"a"))],
        );
        let ps_b = PatchSet::new(
            make_ws("ws-b"),
            make_epoch(),
            vec![make_change("config.toml", ChangeKind::Modified, Some(b"b"))],
        );
        let ps_c = PatchSet::new(
            make_ws("ws-c"),
            make_epoch(),
            vec![make_change("config.toml", ChangeKind::Modified, Some(b"c"))],
        );

        let result = partition_by_path(&[ps_a, ps_b, ps_c]);

        assert_eq!(result.shared_count(), 1);
        let (_, entries) = &result.shared[0];
        assert_eq!(entries.len(), 3);
        // Sorted by workspace ID.
        assert_eq!(entries[0].workspace_id.as_str(), "ws-a");
        assert_eq!(entries[1].workspace_id.as_str(), "ws-b");
        assert_eq!(entries[2].workspace_id.as_str(), "ws-c");
    }

    // -- 5-way with disjoint and shared --

    #[test]
    fn partition_five_way_mixed() {
        let workspaces: Vec<PatchSet> = (0..5)
            .map(|i| {
                let ws = make_ws(&format!("ws-{i}"));
                let mut changes = vec![
                    // Each workspace has a unique file.
                    make_change(
                        &format!("unique-{i}.rs"),
                        ChangeKind::Added,
                        Some(format!("fn ws_{i}() {{}}").as_bytes()),
                    ),
                ];
                // All workspaces modify the shared file.
                changes.push(make_change(
                    "shared.rs",
                    ChangeKind::Modified,
                    Some(format!("version {i}").as_bytes()),
                ));
                PatchSet::new(ws, make_epoch(), changes)
            })
            .collect();

        let result = partition_by_path(&workspaces);

        assert_eq!(result.unique_count(), 5, "5 unique files");
        assert_eq!(result.shared_count(), 1, "1 shared file");
        assert_eq!(result.total_path_count(), 6);

        let (_, entries) = &result.shared[0];
        assert_eq!(entries.len(), 5, "5 workspaces modified shared.rs");
    }

    // -- Deletion entries --

    #[test]
    fn partition_preserves_deletion_info() {
        let ps = PatchSet::new(
            make_ws("ws-a"),
            make_epoch(),
            vec![make_change("gone.rs", ChangeKind::Deleted, None)],
        );

        let result = partition_by_path(&[ps]);

        assert_eq!(result.unique_count(), 1);
        let (path, entry) = &result.unique[0];
        assert_eq!(path, &PathBuf::from("gone.rs"));
        assert!(entry.is_deletion());
        assert!(entry.content.is_none());
    }

    // -- Content preserved --

    #[test]
    fn partition_preserves_file_content() {
        let content = b"hello world\nline 2\n";
        let ps = PatchSet::new(
            make_ws("ws-a"),
            make_epoch(),
            vec![make_change("hello.txt", ChangeKind::Added, Some(content))],
        );

        let result = partition_by_path(&[ps]);

        let (_, entry) = &result.unique[0];
        assert_eq!(entry.content.as_deref(), Some(content.as_ref()));
    }

    // -- Path ordering --

    #[test]
    fn partition_paths_are_lexicographic() {
        let ps = PatchSet::new(
            make_ws("ws-a"),
            make_epoch(),
            vec![
                make_change("z.rs", ChangeKind::Added, Some(b"")),
                make_change("a.rs", ChangeKind::Added, Some(b"")),
                make_change("m/deep.rs", ChangeKind::Added, Some(b"")),
                make_change("b.rs", ChangeKind::Added, Some(b"")),
            ],
        );

        let result = partition_by_path(&[ps]);

        let paths: Vec<_> = result.unique.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("a.rs"),
                PathBuf::from("b.rs"),
                PathBuf::from("m/deep.rs"),
                PathBuf::from("z.rs"),
            ]
        );
    }

    // -- Modify/delete conflict --

    #[test]
    fn partition_modify_delete_is_shared() {
        let ps_a = PatchSet::new(
            make_ws("ws-a"),
            make_epoch(),
            vec![make_change("file.rs", ChangeKind::Modified, Some(b"new"))],
        );
        let ps_b = PatchSet::new(
            make_ws("ws-b"),
            make_epoch(),
            vec![make_change("file.rs", ChangeKind::Deleted, None)],
        );

        let result = partition_by_path(&[ps_a, ps_b]);

        assert_eq!(result.shared_count(), 1);
        let (_, entries) = &result.shared[0];
        assert_eq!(entries.len(), 2);
        assert!(matches!(entries[0].kind, ChangeKind::Modified));
        assert!(matches!(entries[1].kind, ChangeKind::Deleted));
    }

    // -- Add/add conflict --

    #[test]
    fn partition_add_add_is_shared() {
        let ps_a = PatchSet::new(
            make_ws("ws-a"),
            make_epoch(),
            vec![make_change("new.rs", ChangeKind::Added, Some(b"version a"))],
        );
        let ps_b = PatchSet::new(
            make_ws("ws-b"),
            make_epoch(),
            vec![make_change("new.rs", ChangeKind::Added, Some(b"version b"))],
        );

        let result = partition_by_path(&[ps_a, ps_b]);

        assert_eq!(result.unique_count(), 0);
        assert_eq!(result.shared_count(), 1);
        let (_, entries) = &result.shared[0];
        assert_eq!(entries.len(), 2);
        assert!(matches!(entries[0].kind, ChangeKind::Added));
        assert!(matches!(entries[1].kind, ChangeKind::Added));
    }

    // -- Delete/delete is shared (but trivially resolvable) --

    #[test]
    fn partition_delete_delete_is_shared() {
        let ps_a = PatchSet::new(
            make_ws("ws-a"),
            make_epoch(),
            vec![make_change("old.rs", ChangeKind::Deleted, None)],
        );
        let ps_b = PatchSet::new(
            make_ws("ws-b"),
            make_epoch(),
            vec![make_change("old.rs", ChangeKind::Deleted, None)],
        );

        let result = partition_by_path(&[ps_a, ps_b]);

        assert_eq!(result.shared_count(), 1);
        let (_, entries) = &result.shared[0];
        assert_eq!(entries.len(), 2);
        // Both deletions.
        assert!(entries.iter().all(super::PathEntry::is_deletion));
    }

    // -- PathEntry --

    #[test]
    fn path_entry_is_deletion() {
        let del = PathEntry::new(make_ws("ws"), ChangeKind::Deleted, None);
        assert!(del.is_deletion());

        let add = PathEntry::new(make_ws("ws"), ChangeKind::Added, Some(vec![]));
        assert!(!add.is_deletion());
    }

    // -----------------------------------------------------------------------
    // Phase 3: FileId + blob OID propagation through partition
    // -----------------------------------------------------------------------

    /// Helper: build a `FileChange` with identity metadata (`FileId` + blob OID).
    fn make_change_with_identity(
        path: &str,
        kind: ChangeKind,
        content: Option<&[u8]>,
        file_id: crate::model::patch::FileId,
        blob_hex: Option<&str>,
    ) -> FileChange {
        let blob = blob_hex.and_then(|h| crate::model::types::GitOid::new(h).ok());
        FileChange::with_identity(
            PathBuf::from(path),
            kind,
            content.map(<[u8]>::to_vec),
            Some(file_id),
            blob,
        )
    }

    /// `FileId` and blob OID on a `FileChange` should be propagated into the
    /// `PathEntry` that appears in the partition result.
    #[test]
    fn partition_propagates_file_id_and_blob_to_path_entry() {
        use crate::model::patch::FileId;

        let fid = FileId::new(0xdead_beef_cafe_babe_1234_5678_9abc_def0);
        let blob_hex = "a".repeat(40);

        let change = make_change_with_identity(
            "src/lib.rs",
            ChangeKind::Modified,
            Some(b"fn lib() {}"),
            fid,
            Some(&blob_hex),
        );
        let ps = PatchSet::new(make_ws("ws-a"), make_epoch(), vec![change]);

        let result = partition_by_path(&[ps]);

        // The file was only modified by one workspace → it's a unique path.
        assert_eq!(result.unique_count(), 1);
        let (path, entry) = &result.unique[0];
        assert_eq!(path, &PathBuf::from("src/lib.rs"));
        assert_eq!(
            entry.file_id,
            Some(fid),
            "FileId should propagate from FileChange to PathEntry"
        );
        assert!(
            entry.blob.is_some(),
            "blob OID should propagate from FileChange to PathEntry"
        );
    }

    /// `FileId` and blob OID propagate correctly into shared (multi-workspace) entries.
    #[test]
    fn partition_propagates_identity_into_shared_entries() {
        use crate::model::patch::FileId;

        let fid_a = FileId::new(1);
        let fid_b = FileId::new(2);
        let blob_a = "a".repeat(40);
        let blob_b = "b".repeat(40);

        let change_a = make_change_with_identity(
            "shared.rs",
            ChangeKind::Modified,
            Some(b"version A"),
            fid_a,
            Some(&blob_a),
        );
        let change_b = make_change_with_identity(
            "shared.rs",
            ChangeKind::Modified,
            Some(b"version B"),
            fid_b,
            Some(&blob_b),
        );

        let ps_a = PatchSet::new(make_ws("ws-a"), make_epoch(), vec![change_a]);
        let ps_b = PatchSet::new(make_ws("ws-b"), make_epoch(), vec![change_b]);

        let result = partition_by_path(&[ps_a, ps_b]);
        assert_eq!(result.shared_count(), 1);

        let (_, entries) = &result.shared[0];
        assert_eq!(entries.len(), 2);

        // Find ws-a and ws-b entries.
        let entry_a = entries
            .iter()
            .find(|e| e.workspace_id.as_str() == "ws-a")
            .expect("operation should succeed");
        let entry_b = entries
            .iter()
            .find(|e| e.workspace_id.as_str() == "ws-b")
            .expect("operation should succeed");

        assert_eq!(entry_a.file_id, Some(fid_a));
        assert_eq!(entry_b.file_id, Some(fid_b));
        assert!(entry_a.blob.is_some());
        assert!(entry_b.blob.is_some());
        // Blobs should differ (different content).
        assert_ne!(entry_a.blob, entry_b.blob);
    }

    /// `FileChange` without identity (Phase 1 compat) results in None fields in `PathEntry`.
    #[test]
    fn partition_phase1_change_has_no_identity_in_path_entry() {
        let change = make_change("old_style.rs", ChangeKind::Added, Some(b"fn old() {}"));
        let ps = PatchSet::new(make_ws("ws-legacy"), make_epoch(), vec![change]);
        let result = partition_by_path(&[ps]);

        let (_, entry) = &result.unique[0];
        assert!(
            entry.file_id.is_none(),
            "Phase 1 FileChange should produce PathEntry with no FileId"
        );
        assert!(
            entry.blob.is_none(),
            "Phase 1 FileChange should produce PathEntry with no blob OID"
        );
    }

    // -----------------------------------------------------------------------
    // bn-2dy1: D/F (Directory/File) clash detection
    // -----------------------------------------------------------------------

    /// Direction 1: ws-a adds FILE `clash`, ws-b adds `clash/sub.txt`.
    /// The partition must record a D/F clash between them.
    #[test]
    fn partition_df_clash_direction1_file_vs_dir() {
        let ps_a = PatchSet::new(
            make_ws("ws-a"),
            make_epoch(),
            vec![make_change(
                "clash",
                ChangeKind::Added,
                Some(b"file content"),
            )],
        );
        let ps_b = PatchSet::new(
            make_ws("ws-b"),
            make_epoch(),
            vec![make_change(
                "clash/sub.txt",
                ChangeKind::Added,
                Some(b"dir content"),
            )],
        );

        let result = partition_by_path(&[ps_a, ps_b]);

        // D/F clash must be detected.
        assert_eq!(
            result.df_clashes.len(),
            1,
            "exactly one D/F clash expected; got: {:?}",
            result
                .df_clashes
                .iter()
                .map(|c| (&c.file_path, &c.dir_child_example))
                .collect::<Vec<_>>()
        );
        let clash = &result.df_clashes[0];
        assert_eq!(clash.file_path, PathBuf::from("clash"));
        assert_eq!(clash.dir_child_example, PathBuf::from("clash/sub.txt"));
        assert_eq!(clash.file_ws.as_str(), "ws-a");
        assert_eq!(clash.dir_ws.as_str(), "ws-b");

        // is_conflict_free must be false.
        assert!(!result.is_conflict_free());
    }

    /// Direction 2: ws-a adds `deep/nested/leaf.txt`, ws-b adds FILE `deep`.
    /// The partition must record a D/F clash.
    #[test]
    fn partition_df_clash_direction2_nested_dir_vs_file() {
        let ps_a = PatchSet::new(
            make_ws("ws-a"),
            make_epoch(),
            vec![make_change(
                "deep/nested/leaf.txt",
                ChangeKind::Added,
                Some(b"leaf"),
            )],
        );
        let ps_b = PatchSet::new(
            make_ws("ws-b"),
            make_epoch(),
            vec![make_change("deep", ChangeKind::Added, Some(b"file"))],
        );

        let result = partition_by_path(&[ps_a, ps_b]);

        assert!(
            !result.df_clashes.is_empty(),
            "D/F clash should be detected for 'deep' vs 'deep/nested/leaf.txt'; \
             got no clashes"
        );
        let clash = &result.df_clashes[0];
        assert_eq!(clash.file_path, PathBuf::from("deep"));
        // dir_child_example should be the deeply nested child.
        assert_eq!(
            clash.dir_child_example,
            PathBuf::from("deep/nested/leaf.txt")
        );
    }

    /// Clean case: `deep.txt` vs `deep/sub.txt` — NOT a D/F clash because
    /// `deep.txt` is not a component-wise prefix of `deep/sub.txt`.
    #[test]
    fn partition_no_false_positive_extension_is_not_prefix() {
        let ps_a = PatchSet::new(
            make_ws("ws-a"),
            make_epoch(),
            vec![make_change("deep.txt", ChangeKind::Added, Some(b"file"))],
        );
        let ps_b = PatchSet::new(
            make_ws("ws-b"),
            make_epoch(),
            vec![make_change("deep/sub.txt", ChangeKind::Added, Some(b"dir"))],
        );

        let result = partition_by_path(&[ps_a, ps_b]);

        assert!(
            result.df_clashes.is_empty(),
            "deep.txt is NOT a prefix of deep/sub.txt — no D/F clash expected; \
             got: {:?}",
            result
                .df_clashes
                .iter()
                .map(|c| (&c.file_path, &c.dir_child_example))
                .collect::<Vec<_>>()
        );
        // Both paths should be in unique, no clash.
        assert_eq!(result.unique_count(), 2);
        assert_eq!(result.shared_count(), 0);
        assert!(result.is_conflict_free());
    }

    /// Clean case: `deep` vs `deeper` — NOT a D/F clash.
    /// `deep` is not a component-wise prefix of `deeper` (would need `deep/`).
    #[test]
    fn partition_no_false_positive_similar_names() {
        let ps_a = PatchSet::new(
            make_ws("ws-a"),
            make_epoch(),
            vec![make_change("deep", ChangeKind::Added, Some(b"file"))],
        );
        let ps_b = PatchSet::new(
            make_ws("ws-b"),
            make_epoch(),
            vec![make_change(
                "deeper",
                ChangeKind::Added,
                Some(b"another file"),
            )],
        );

        let result = partition_by_path(&[ps_a, ps_b]);

        assert!(
            result.df_clashes.is_empty(),
            "'deep' is NOT a prefix of 'deeper' — no D/F clash expected; \
             got: {:?}",
            result
                .df_clashes
                .iter()
                .map(|c| (&c.file_path, &c.dir_child_example))
                .collect::<Vec<_>>()
        );
        assert!(result.is_conflict_free());
    }

    /// Clean case: a single workspace restructuring FILE↔DIR (delete child +
    /// add file at the prefix in ONE patch) is internally consistent — NOT a
    /// D/F clash. Deletions never occupy paths.
    #[test]
    fn partition_no_df_clash_for_internal_restructure_with_deletions() {
        let ps = PatchSet::new(
            make_ws("ws-a"),
            make_epoch(),
            vec![
                make_change("deep/a/leaf.txt", ChangeKind::Deleted, None),
                make_change("deep", ChangeKind::Added, Some(b"now a file")),
            ],
        );

        let result = partition_by_path(&[ps]);

        assert!(
            result.df_clashes.is_empty(),
            "Deleted deep/a/leaf.txt + Added deep in one patch is a consistent \
             restructure, not a D/F clash; got: {:?}",
            result
                .df_clashes
                .iter()
                .map(|c| (&c.file_path, &c.dir_child_example))
                .collect::<Vec<_>>()
        );
        assert!(result.is_conflict_free());
    }

    /// Clean case: completely disjoint paths produce no D/F clashes.
    #[test]
    fn partition_no_df_clash_for_disjoint_paths() {
        let ps_a = PatchSet::new(
            make_ws("ws-a"),
            make_epoch(),
            vec![make_change(
                "foo.rs",
                ChangeKind::Added,
                Some(b"fn foo() {}"),
            )],
        );
        let ps_b = PatchSet::new(
            make_ws("ws-b"),
            make_epoch(),
            vec![make_change(
                "bar.rs",
                ChangeKind::Added,
                Some(b"fn bar() {}"),
            )],
        );

        let result = partition_by_path(&[ps_a, ps_b]);

        assert!(result.df_clashes.is_empty());
        assert!(result.is_conflict_free());
    }
}
