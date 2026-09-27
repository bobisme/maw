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

    // bn-2dy1 / bn-2jml: D/F clash detection over EVERY non-deletion entry,
    // unique and shared alike. See `detect_df_clashes`.
    let df_clashes = detect_df_clashes(&unique, &shared);
    PartitionResult {
        unique,
        shared,
        df_clashes,
    }
}

/// Component-normalised byte key for a repo-relative path: components joined
/// with `/`. `./` components and trailing separators vanish, so `a/./b` and
/// `a/b/` both key as `a/b`. Bytes (not lossy UTF-8) so distinct non-UTF-8
/// names never collapse onto the same key.
fn path_key(path: &std::path::Path) -> Vec<u8> {
    let mut key = Vec::new();
    for comp in path.components() {
        if matches!(comp, std::path::Component::CurDir) {
            continue;
        }
        if !key.is_empty() {
            key.push(b'/');
        }
        key.extend_from_slice(comp.as_os_str().as_encoded_bytes());
    }
    key
}

/// Detect D/F (Directory/File) clashes (bn-2dy1, completed by bn-2jml).
///
/// A clash exists for every pair of non-deleted paths `(P, Q)` — drawn from
/// ANY entry, unique or shared — where `P` is a strict component-wise prefix
/// of `Q`: some workspace puts a FILE at `P` while some workspace needs `P`
/// to be a directory. Both cannot occupy one git tree.
///
/// bn-2jml: the original detector only iterated `unique` entries, so a
/// shared FILE `a` (ws1+ws2) against a shared `a/b` (ws3+ws4) produced no
/// clash and the build step wrote a tree with a duplicate `a` entry (blob +
/// tree) onto the target branch.
///
/// Deletions never participate: a `Deleted` entry does not occupy the result
/// tree. (A single workspace restructuring FILE↔DIR emits `Deleted deep/x` +
/// `Added deep` — internally consistent, not a clash.)
///
/// Output: one `DfClash` per `(file_path, dir_child)` pair, sorted by
/// `(file_path, dir_child_example)`, deduplicated. When several workspaces
/// sit on either side, the reported `(file_ws, dir_ws)` is the first pair in
/// workspace-id order whose workspaces differ (falling back to the first
/// pair). Same-workspace pairs are still reported: no valid single tree can
/// hold both `P` and `P/...`, so reporting them fails closed.
fn detect_df_clashes(
    unique: &[(PathBuf, PathEntry)],
    shared: &[(PathBuf, Vec<PathEntry>)],
) -> Vec<DfClash> {
    // (key, path, workspace) for every non-deletion entry, sorted by key then
    // workspace so iteration (and thus the chosen example pair) is
    // deterministic.
    let mut rows: Vec<(Vec<u8>, &PathBuf, &WorkspaceId)> = unique
        .iter()
        .map(|(p, e)| (p, e))
        .chain(
            shared
                .iter()
                .flat_map(|(p, entries)| entries.iter().map(move |e| (p, e))),
        )
        .filter(|(_, e)| !e.is_deletion())
        .map(|(p, e)| (path_key(p), p, &e.workspace_id))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.2.as_str().cmp(b.2.as_str())));

    // (file_path, dir_child) -> (file_ws, dir_ws)
    let mut found: BTreeMap<(PathBuf, PathBuf), (WorkspaceId, WorkspaceId)> = BTreeMap::new();

    for (file_key, file_path, file_ws) in &rows {
        let mut dir_prefix = file_key.clone();
        dir_prefix.push(b'/');
        // Every key starting with `P/` forms one contiguous run in the sorted
        // rows, beginning at the first key >= `P/`.
        let start = rows.partition_point(|(k, _, _)| k.as_slice() < dir_prefix.as_slice());
        for (child_key, child_path, child_ws) in &rows[start..] {
            if !child_key.starts_with(&dir_prefix) {
                break;
            }
            let slot = (PathBuf::clone(file_path), PathBuf::clone(child_path));
            let candidate = (WorkspaceId::clone(file_ws), WorkspaceId::clone(child_ws));
            match found.get_mut(&slot) {
                None => {
                    found.insert(slot, candidate);
                }
                Some(existing) => {
                    if existing.0 == existing.1 && candidate.0 != candidate.1 {
                        *existing = candidate;
                    }
                }
            }
        }
    }

    found
        .into_iter()
        .map(
            |((file_path, dir_child_example), (file_ws, dir_ws))| DfClash {
                file_path,
                file_ws,
                dir_child_example,
                dir_ws,
            },
        )
        .collect()
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

    /// bn-2jml regression: shared FILE `a` (ws1+ws2) vs shared `a/b`
    /// (ws3+ws4). Both paths are in `shared`, so the pre-fix detector — which
    /// only iterated `unique` — emitted no clash, and the build step wrote a
    /// tree with duplicate entry `a` (blob + tree) onto the target branch.
    #[test]
    fn partition_df_clash_shared_file_vs_shared_dir_child() {
        let file = |ws: &str| {
            PatchSet::new(
                make_ws(ws),
                make_epoch(),
                vec![make_change("a", ChangeKind::Added, Some(b"file"))],
            )
        };
        let child = |ws: &str| {
            PatchSet::new(
                make_ws(ws),
                make_epoch(),
                vec![make_change("a/b", ChangeKind::Added, Some(b"child"))],
            )
        };

        let result = partition_by_path(&[file("ws-1"), file("ws-2"), child("ws-3"), child("ws-4")]);

        assert_eq!(result.unique_count(), 0);
        assert_eq!(result.shared_count(), 2);
        let pairs: Vec<(PathBuf, PathBuf)> = result
            .df_clashes
            .iter()
            .map(|c| (c.file_path.clone(), c.dir_child_example.clone()))
            .collect();
        assert_eq!(
            pairs,
            vec![(PathBuf::from("a"), PathBuf::from("a/b"))],
            "shared-vs-shared D/F clash must be reported exactly once"
        );
        let paths = result.df_clash_paths();
        assert!(paths.contains(&PathBuf::from("a")));
        assert!(paths.contains(&PathBuf::from("a/b")));
        assert!(!result.is_conflict_free());
    }

    /// bn-3bjx (mutation gap): the reported `(file_ws, dir_ws)` is the FIRST
    /// pair in workspace-id order whose workspaces differ — a later
    /// differing pair must not overwrite it, and a same-workspace pair must be
    /// upgraded to a differing one when one exists.
    #[test]
    fn partition_df_clash_reports_first_differing_workspace_pair() {
        let ps = |ws: &str, path: &str| {
            PatchSet::new(
                make_ws(ws),
                make_epoch(),
                vec![make_change(path, ChangeKind::Added, Some(b"x"))],
            )
        };

        // File `a` shared by ws-1 + ws-2, child `a/b` only in ws-3: both
        // (ws-1, ws-3) and (ws-2, ws-3) differ; the first must be kept.
        let result = partition_by_path(&[ps("ws-1", "a"), ps("ws-2", "a"), ps("ws-3", "a/b")]);
        assert_eq!(result.df_clashes.len(), 1);
        let c = &result.df_clashes[0];
        assert_eq!(c.file_ws.as_str(), "ws-1");
        assert_eq!(c.dir_ws.as_str(), "ws-3");

        // File `a` shared by ws-1 + ws-2, child `a/b` only in ws-1: the first
        // pair (ws-1, ws-1) is same-workspace and must be replaced by (ws-2, ws-1).
        let ws1 = PatchSet::new(
            make_ws("ws-1"),
            make_epoch(),
            vec![
                make_change("a", ChangeKind::Added, Some(b"x")),
                make_change("a/b", ChangeKind::Added, Some(b"x")),
            ],
        );
        let result = partition_by_path(&[ws1, ps("ws-2", "a")]);
        assert_eq!(result.df_clashes.len(), 1);
        let c = &result.df_clashes[0];
        assert_eq!(c.file_ws.as_str(), "ws-2");
        assert_eq!(c.dir_ws.as_str(), "ws-1");
    }

    /// bn-2jml: every directory-side child of a clashing FILE must land in
    /// `df_clash_paths()`, including shared children, so the resolve step
    /// never applies any of them next to the file.
    #[test]
    fn partition_df_clash_covers_every_shared_dir_child() {
        let ps1 = PatchSet::new(
            make_ws("ws-1"),
            make_epoch(),
            vec![make_change("a", ChangeKind::Added, Some(b"file"))],
        );
        let ps2 = PatchSet::new(
            make_ws("ws-2"),
            make_epoch(),
            vec![
                make_change("a/b", ChangeKind::Added, Some(b"x")),
                make_change("a/c/d", ChangeKind::Added, Some(b"y")),
            ],
        );
        let ps3 = PatchSet::new(
            make_ws("ws-3"),
            make_epoch(),
            vec![
                make_change("a/b", ChangeKind::Added, Some(b"x")),
                make_change("a/c/d", ChangeKind::Added, Some(b"y")),
            ],
        );

        let result = partition_by_path(&[ps1, ps2, ps3]);
        let paths = result.df_clash_paths();
        for p in ["a", "a/b", "a/c/d"] {
            assert!(
                paths.contains(&PathBuf::from(p)),
                "{p} missing from {paths:?}"
            );
        }
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

    // -----------------------------------------------------------------------
    // bn-2jml: D/F clash specification property
    // -----------------------------------------------------------------------

    mod df_clash_props {
        use super::*;
        use proptest::prelude::*;
        use std::collections::BTreeSet;
        use std::path::Path;

        /// Paths over a tiny alphabet {a, b, a.rs} with 1..=3 components, so
        /// component-prefix pairs (`a` vs `a/b`, `a/b` vs `a/b/a.rs`) and
        /// near-miss string prefixes (`a` vs `a.rs`) are both common.
        fn arb_small_path() -> impl Strategy<Value = PathBuf> {
            prop::collection::vec(prop::sample::select(vec!["a", "b", "a.rs"]), 1..=3)
                .prop_map(|segs| PathBuf::from(segs.join("/")))
        }

        fn arb_kind() -> impl Strategy<Value = ChangeKind> {
            prop_oneof![
                3 => Just(ChangeKind::Added),
                2 => Just(ChangeKind::Modified),
                2 => Just(ChangeKind::Deleted),
            ]
        }

        /// 2..=4 workspaces, each with 0..=4 distinct paths.
        fn arb_patch_sets() -> impl Strategy<Value = Vec<PatchSet>> {
            prop::collection::vec(
                prop::collection::btree_map(arb_small_path(), arb_kind(), 0..=4),
                2..=4,
            )
            .prop_map(|per_ws| {
                per_ws
                    .into_iter()
                    .enumerate()
                    .map(|(i, changes)| {
                        let changes = changes
                            .into_iter()
                            .map(|(path, kind)| {
                                let content =
                                    (!matches!(kind, ChangeKind::Deleted)).then(|| b"x".to_vec());
                                FileChange::new(path, kind, content)
                            })
                            .collect();
                        PatchSet::new(make_ws(&format!("ws-{i}")), make_epoch(), changes)
                    })
                    .collect()
            })
        }

        /// Independent oracle: every (P, Q) of non-deleted paths (any
        /// workspace) with P a strict component-wise prefix of Q, via
        /// `Path::starts_with` rather than the production byte-key scan.
        fn spec_pairs(patch_sets: &[PatchSet]) -> BTreeSet<(PathBuf, PathBuf)> {
            let live: BTreeSet<PathBuf> = patch_sets
                .iter()
                .flat_map(|ps| ps.changes.iter())
                .filter(|c| !matches!(c.kind, ChangeKind::Deleted))
                .map(|c| c.path.clone())
                .collect();
            let mut out = BTreeSet::new();
            for p in &live {
                for q in &live {
                    if p != q && q.starts_with(p) {
                        out.insert((p.clone(), q.clone()));
                    }
                }
            }
            out
        }

        fn live_in(patch_sets: &[PatchSet], ws: &WorkspaceId, path: &Path) -> bool {
            patch_sets.iter().any(|ps| {
                &ps.workspace_id == ws
                    && ps
                        .changes
                        .iter()
                        .any(|c| c.path == path && !matches!(c.kind, ChangeKind::Deleted))
            })
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(2048))]

            /// Clash reported for (P, Q) iff P and Q are non-deleted in some
            /// workspaces and P is a strict component prefix of Q — whether
            /// either side is unique or shared (bn-2jml).
            #[test]
            fn prop_df_clash_iff_component_prefix_small_alphabet_le_3_comps(
                patch_sets in arb_patch_sets()
            ) {
                let result = partition_by_path(&patch_sets);
                let got: BTreeSet<(PathBuf, PathBuf)> = result
                    .df_clashes
                    .iter()
                    .map(|c| (c.file_path.clone(), c.dir_child_example.clone()))
                    .collect();
                prop_assert_eq!(got.len(), result.df_clashes.len(), "clashes must be deduplicated");
                prop_assert_eq!(&got, &spec_pairs(&patch_sets));

                let skip = result.df_clash_paths();
                for (p, q) in &got {
                    prop_assert!(skip.contains(p) && skip.contains(q));
                }

                for c in &result.df_clashes {
                    prop_assert!(live_in(&patch_sets, &c.file_ws, &c.file_path));
                    prop_assert!(live_in(&patch_sets, &c.dir_ws, &c.dir_child_example));
                    // Prefer a cross-workspace witness whenever one exists.
                    let cross_exists = patch_sets.iter().any(|a| patch_sets.iter().any(|b| {
                        a.workspace_id != b.workspace_id
                            && live_in(&patch_sets, &a.workspace_id, &c.file_path)
                            && live_in(&patch_sets, &b.workspace_id, &c.dir_child_example)
                    }));
                    if cross_exists {
                        prop_assert_ne!(&c.file_ws, &c.dir_ws);
                    }
                }

                // Order of workspaces must not change the answer.
                let mut reversed = patch_sets.clone();
                reversed.reverse();
                let again = partition_by_path(&reversed);
                let render = |r: &PartitionResult| -> Vec<(PathBuf, PathBuf, String, String)> {
                    r.df_clashes
                        .iter()
                        .map(|c| (
                            c.file_path.clone(),
                            c.dir_child_example.clone(),
                            c.file_ws.as_str().to_owned(),
                            c.dir_ws.as_str().to_owned(),
                        ))
                        .collect()
                };
                prop_assert_eq!(render(&result), render(&again));
            }
        }
    }
}
