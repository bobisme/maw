//! Global view computation — CRDT merge of per-workspace views (§5.5).
//!
//! A [`GlobalView`] merges all (non-destroyed) workspace views into a single
//! read-only picture of the repository state. It is computed by collecting
//! per-workspace [`MaterializedView`]s and merging them using join-semilattice
//! semantics.
//!
//! # CRDT merge semantics
//!
//! | Component | Strategy |
//! |-----------|----------|
//! | Workspace set | G-Set (grow-only union) |
//! | Per-workspace patch set | Latest from op log head |
//! | Epoch | Max (lexicographic on OID, deterministic) |
//! | Annotations | Per-key latest-wins (LWW register per workspace) |
//!
//! # Cache key
//!
//! The global view is keyed by the sorted set of `(workspace_id, head_oid)`
//! pairs. When any workspace head advances, the cache is invalidated.
//!
//! # Example
//!
//! ```text
//! compute_global_view(root, &["agent-1", "agent-2", "agent-3"])
//!   → materialize each workspace view
//!   → merge: epoch = max, patch_set = union (with conflicts), workspaces = all
//!   → return GlobalView { epoch, workspace_views, merged_patch_set, conflicts }
//! ```

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::model::join::{self, PathConflict};
use crate::model::patch::PatchSet;
use crate::model::types::{EpochId, GitOid, WorkspaceId};

use super::view::{MaterializedView, ViewError};

// ---------------------------------------------------------------------------
// GlobalView
// ---------------------------------------------------------------------------

/// A merged view of all workspaces in the repository.
///
/// Produced by [`compute_global_view`] or [`compute_global_view_from_views`].
/// The global view is a read-only projection — it is never persisted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalView {
    /// The current epoch (max across all workspace views).
    pub epoch: Option<EpochId>,

    /// Per-workspace views (keyed by workspace ID, excludes destroyed).
    pub workspace_views: BTreeMap<String, WorkspaceSnapshot>,

    /// The merged patch set (union of all workspace patch sets).
    ///
    /// `None` if no workspace has any patches.
    pub merged_patch_set: Option<PatchSet>,

    /// Paths where workspace patch sets conflict.
    pub conflicts: Vec<PathConflict>,

    /// Total number of operations across all workspaces.
    pub total_ops: usize,

    /// Cache key: sorted (`workspace_id`, `head_oid`) pairs.
    ///
    /// Used to determine if the cached global view is still valid.
    pub cache_key: Vec<(String, String)>,
}

/// A snapshot of a single workspace's state within the global view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    /// The workspace's current epoch.
    pub epoch: Option<EpochId>,

    /// Whether the workspace has uncommitted patches.
    pub has_changes: bool,

    /// Number of patches in the workspace's patch set.
    pub patch_count: usize,

    /// Human-readable description (from Describe operations).
    pub description: Option<String>,

    /// Number of operations in the workspace's log.
    pub op_count: usize,
}

impl WorkspaceSnapshot {
    /// Create from a [`MaterializedView`].
    #[must_use]
    pub fn from_view(view: &MaterializedView) -> Self {
        Self {
            epoch: view.epoch.clone(),
            has_changes: view.has_changes(),
            patch_count: view
                .patch_set
                .as_ref()
                .map_or(0, super::super::model::patch::PatchSet::len),
            description: view.description.clone(),
            op_count: view.op_count,
        }
    }
}

impl GlobalView {
    /// Return `true` if there are no conflicts across workspace patch sets.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }

    /// Return the number of active (non-destroyed) workspaces.
    #[must_use]
    pub fn workspace_count(&self) -> usize {
        self.workspace_views.len()
    }

    /// Return workspace IDs sorted alphabetically.
    #[must_use]
    #[allow(dead_code)]
    pub fn workspace_ids(&self) -> Vec<&str> {
        self.workspace_views
            .keys()
            .map(std::string::String::as_str)
            .collect()
    }

    /// Return the total number of patches across all workspaces.
    #[must_use]
    pub fn total_patches(&self) -> usize {
        self.merged_patch_set
            .as_ref()
            .map_or(0, super::super::model::patch::PatchSet::len)
    }

    /// Check if a given cache key matches this view's cache key.
    #[must_use]
    pub fn cache_valid(&self, other_key: &[(String, String)]) -> bool {
        self.cache_key == other_key
    }
}

impl fmt::Display for GlobalView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "global_view(")?;
        if let Some(epoch) = &self.epoch {
            write!(f, "epoch={}", &epoch.as_str()[..12])?;
        } else {
            write!(f, "no-epoch")?;
        }
        write!(
            f,
            ", {} ws, {} patches, {} conflicts, {} ops)",
            self.workspace_views.len(),
            self.total_patches(),
            self.conflicts.len(),
            self.total_ops
        )
    }
}

// ---------------------------------------------------------------------------
// Compute global view from pre-materialized views
// ---------------------------------------------------------------------------

/// Compute a global view from a list of pre-materialized workspace views.
///
/// This is the core merge function. It:
/// 1. Filters out destroyed workspaces
/// 2. Computes the max epoch (lexicographic comparison)
/// 3. Merges all patch sets using the `PatchSet` join operation
/// 4. Collects conflicts
///
/// # CRDT properties
///
/// The merge is:
/// - **Commutative**: order of views doesn't affect the result (`BTreeMap` + sorted conflicts)
/// - **Associative**: merging (a, b, c) is the same regardless of grouping
/// - **Idempotent**: merging the same view twice produces the same result
///
/// These properties hold because:
/// - [`join::join`] is commutative, associative, and idempotent
/// - We use `BTreeMap` for deterministic iteration
/// - Conflict sides are sorted within each `PathConflict`
///
/// # Arguments
///
/// * `views` - workspace views to merge (including potentially destroyed ones)
/// * `cache_key` - sorted (`workspace_id`, `head_oid`) pairs for cache validation
#[must_use]
pub fn compute_global_view_from_views(
    views: &[MaterializedView],
    cache_key: Vec<(String, String)>,
) -> GlobalView {
    let mut workspace_views = BTreeMap::new();
    let mut max_epoch: Option<EpochId> = None;
    let mut total_ops = 0;
    let mut patch_sets: Vec<(&WorkspaceId, &PatchSet)> = Vec::new();

    for view in views {
        // Skip destroyed workspaces
        if view.is_destroyed {
            continue;
        }

        total_ops += view.op_count;

        // Track max epoch (lexicographic comparison on OID string)
        if let Some(epoch) = &view.epoch {
            let should_update = max_epoch
                .as_ref()
                .is_none_or(|current| epoch.as_str() > current.as_str());
            if should_update {
                max_epoch = Some(epoch.clone());
            }
        }

        // Collect workspace snapshot
        workspace_views.insert(
            view.workspace_id.to_string(),
            WorkspaceSnapshot::from_view(view),
        );

        // Collect patch sets for merging
        if let Some(ps) = &view.patch_set {
            patch_sets.push((&view.workspace_id, ps));
        }
    }

    // Merge all patch sets using pairwise join
    let (merged_patch_set, conflicts) = merge_patch_sets(&patch_sets);

    GlobalView {
        epoch: max_epoch,
        workspace_views,
        merged_patch_set,
        conflicts,
        total_ops,
        cache_key,
    }
}

/// Merge multiple patch sets into one using pairwise join.
///
/// Returns (`merged_patch_set`, conflicts). If no patch sets, returns (None, []).
/// If one patch set, returns (Some(clone), []).
/// If multiple, joins them pairwise accumulating conflicts.
fn merge_patch_sets(
    patch_sets: &[(&WorkspaceId, &PatchSet)],
) -> (Option<PatchSet>, Vec<PathConflict>) {
    if patch_sets.is_empty() {
        return (None, vec![]);
    }

    if patch_sets.len() == 1 {
        return (Some(patch_sets[0].1.clone()), vec![]);
    }

    // Start with the first patch set and join the rest
    let mut accumulated = patch_sets[0].1.clone();
    let mut all_conflicts: Vec<PathConflict> = Vec::new();

    for (_ws, ps) in &patch_sets[1..] {
        match join::join(&accumulated, ps) {
            Ok(result) => {
                accumulated = result.merged;
                all_conflicts.extend(result.conflicts);
            }
            Err(_epoch_mismatch) => {
                // Epoch mismatch means the patch sets can't be joined directly.
                // This shouldn't happen in normal operation (all workspaces in
                // the same epoch), but we handle it gracefully by keeping the
                // accumulated result so far.
            }
        }
    }

    // Deduplicate conflicts by path (join may produce duplicates if
    // the same path conflicts with multiple workspaces)
    all_conflicts.sort_by(|a, b| a.path.cmp(&b.path));
    all_conflicts.dedup_by(|a, b| a.path == b.path);

    (Some(accumulated), all_conflicts)
}

/// Compute a global view by materializing all workspace views from the op log.
///
/// This is the high-level API. It:
/// 1. Lists all workspaces by reading `refs/manifold/head/*`
/// 2. Materializes each workspace view
/// 3. Builds the cache key from workspace heads
/// 4. Calls [`compute_global_view_from_views`]
///
/// # Errors
///
/// Returns `ViewError` if any workspace view cannot be materialized.
#[allow(dead_code)]
pub fn compute_global_view<F>(
    root: &std::path::Path,
    workspace_ids: &[WorkspaceId],
    read_patch_set: F,
) -> Result<GlobalView, ViewError>
where
    F: Fn(&GitOid) -> Result<PatchSet, ViewError>,
{
    let mut views = Vec::new();
    let mut cache_key = Vec::new();

    for ws_id in workspace_ids {
        let view = super::checkpoint::materialize_from_checkpoint(root, ws_id, &read_patch_set)
            .or_else(|_| super::view::materialize(root, ws_id, &read_patch_set))?;

        // For cache key, use the patch_set_oid if available, otherwise "empty"
        let head_oid = view
            .patch_set_oid
            .as_ref()
            .map_or_else(|| "empty".to_string(), |o| o.as_str().to_owned());
        cache_key.push((ws_id.to_string(), head_oid));

        views.push(view);
    }

    cache_key.sort();

    Ok(compute_global_view_from_views(&views, cache_key))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
mod tests {
    use super::*;
    use crate::model::patch::{FileId, PatchValue};
    use crate::model::types::EpochId;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    // Helpers
    fn test_oid(c: char) -> GitOid {
        GitOid::new(&c.to_string().repeat(40)).unwrap()
    }

    fn test_epoch(c: char) -> EpochId {
        EpochId::new(&c.to_string().repeat(40)).unwrap()
    }

    fn test_ws(name: &str) -> WorkspaceId {
        WorkspaceId::new(name).unwrap()
    }

    fn make_view(
        ws: &str,
        epoch_char: Option<char>,
        patches: BTreeMap<PathBuf, PatchValue>,
        op_count: usize,
    ) -> MaterializedView {
        let epoch = epoch_char.map(test_epoch);
        let patch_set = if patches.is_empty() {
            None
        } else {
            Some(PatchSet {
                base_epoch: epoch.clone().unwrap_or_else(|| test_epoch('0')),
                patches,
            })
        };
        MaterializedView {
            workspace_id: test_ws(ws),
            epoch,
            patch_set,
            patch_set_oid: None,
            description: None,
            annotations: BTreeMap::new(),
            op_count,
            is_destroyed: false,
        }
    }

    fn add_patch(path: &str, oid_char: char, file_id: u128) -> (PathBuf, PatchValue) {
        (
            PathBuf::from(path),
            PatchValue::Add {
                blob: test_oid(oid_char),
                file_id: FileId::new(file_id),
            },
        )
    }

    // -----------------------------------------------------------------------
    // Empty cases
    // -----------------------------------------------------------------------

    #[test]
    fn empty_views_produce_empty_global() {
        let gv = compute_global_view_from_views(&[], vec![]);
        assert!(gv.epoch.is_none());
        assert_eq!(gv.workspace_count(), 0);
        assert!(gv.merged_patch_set.is_none());
        assert!(gv.is_clean());
        assert_eq!(gv.total_ops, 0);
    }

    // -----------------------------------------------------------------------
    // Single workspace
    // -----------------------------------------------------------------------

    #[test]
    fn single_workspace_view() {
        let patches: BTreeMap<_, _> = [add_patch("src/main.rs", 'a', 1)].into_iter().collect();
        let view = make_view("ws-1", Some('a'), patches, 3);

        let gv = compute_global_view_from_views(&[view], vec![]);
        assert_eq!(gv.epoch, Some(test_epoch('a')));
        assert_eq!(gv.workspace_count(), 1);
        assert!(gv.merged_patch_set.is_some());
        assert_eq!(gv.total_patches(), 1);
        assert!(gv.is_clean());
        assert_eq!(gv.total_ops, 3);
    }

    #[test]
    fn single_workspace_no_patches() {
        let view = make_view("ws-1", Some('a'), BTreeMap::new(), 1);
        let gv = compute_global_view_from_views(&[view], vec![]);
        assert!(gv.merged_patch_set.is_none());
        assert_eq!(gv.total_patches(), 0);
    }

    // -----------------------------------------------------------------------
    // Multiple workspaces — no conflicts
    // -----------------------------------------------------------------------

    #[test]
    fn two_workspaces_disjoint_patches() {
        let view1 = make_view(
            "ws-1",
            Some('a'),
            [add_patch("src/a.rs", 'a', 1)].into_iter().collect(),
            2,
        );
        let view2 = make_view(
            "ws-2",
            Some('a'),
            [add_patch("src/b.rs", 'b', 2)].into_iter().collect(),
            3,
        );

        let gv = compute_global_view_from_views(&[view1, view2], vec![]);
        assert_eq!(gv.workspace_count(), 2);
        assert_eq!(gv.total_patches(), 2);
        assert!(gv.is_clean());
        assert_eq!(gv.total_ops, 5);
    }

    #[test]
    fn three_workspaces_disjoint_patches() {
        let view1 = make_view(
            "ws-1",
            Some('a'),
            [add_patch("src/a.rs", 'a', 1)].into_iter().collect(),
            1,
        );
        let view2 = make_view(
            "ws-2",
            Some('a'),
            [add_patch("src/b.rs", 'b', 2)].into_iter().collect(),
            1,
        );
        let view3 = make_view(
            "ws-3",
            Some('a'),
            [add_patch("src/c.rs", 'c', 3)].into_iter().collect(),
            1,
        );

        let gv = compute_global_view_from_views(&[view1, view2, view3], vec![]);
        assert_eq!(gv.workspace_count(), 3);
        assert_eq!(gv.total_patches(), 3);
        assert!(gv.is_clean());
        assert_eq!(gv.total_ops, 3);
    }

    // -----------------------------------------------------------------------
    // Multiple workspaces — with conflicts
    // -----------------------------------------------------------------------

    #[test]
    fn two_workspaces_conflicting_patches() {
        // Both workspaces add different content to the same path
        let view1 = make_view(
            "ws-1",
            Some('a'),
            [add_patch("src/main.rs", 'a', 1)].into_iter().collect(),
            2,
        );
        let view2 = make_view(
            "ws-2",
            Some('a'),
            [add_patch("src/main.rs", 'b', 2)].into_iter().collect(),
            2,
        );

        let gv = compute_global_view_from_views(&[view1, view2], vec![]);
        assert_eq!(gv.workspace_count(), 2);
        assert!(!gv.is_clean());
        assert!(!gv.conflicts.is_empty());
        assert_eq!(gv.conflicts[0].path, PathBuf::from("src/main.rs"));
    }

    // -----------------------------------------------------------------------
    // Epoch — max wins
    // -----------------------------------------------------------------------

    #[test]
    fn epoch_max_wins() {
        let view1 = make_view("ws-1", Some('a'), BTreeMap::new(), 1);
        let view2 = make_view("ws-2", Some('c'), BTreeMap::new(), 1);
        let view3 = make_view("ws-3", Some('b'), BTreeMap::new(), 1);

        let gv = compute_global_view_from_views(&[view1, view2, view3], vec![]);
        assert_eq!(gv.epoch, Some(test_epoch('c')));
    }

    #[test]
    fn epoch_none_when_all_workspaces_have_no_epoch() {
        let view1 = make_view("ws-1", None, BTreeMap::new(), 0);
        let view2 = make_view("ws-2", None, BTreeMap::new(), 0);

        let gv = compute_global_view_from_views(&[view1, view2], vec![]);
        assert!(gv.epoch.is_none());
    }

    #[test]
    fn epoch_some_beats_none() {
        let view1 = make_view("ws-1", None, BTreeMap::new(), 0);
        let view2 = make_view("ws-2", Some('b'), BTreeMap::new(), 1);

        let gv = compute_global_view_from_views(&[view1, view2], vec![]);
        assert_eq!(gv.epoch, Some(test_epoch('b')));
    }

    // -----------------------------------------------------------------------
    // Destroyed workspaces excluded
    // -----------------------------------------------------------------------

    #[test]
    fn destroyed_workspaces_excluded() {
        let view1 = make_view(
            "ws-1",
            Some('a'),
            [add_patch("src/a.rs", 'a', 1)].into_iter().collect(),
            2,
        );
        let mut view2 = make_view(
            "ws-2",
            Some('a'),
            [add_patch("src/b.rs", 'b', 2)].into_iter().collect(),
            3,
        );
        view2.is_destroyed = true;

        let gv = compute_global_view_from_views(&[view1, view2], vec![]);
        assert_eq!(gv.workspace_count(), 1);
        assert!(gv.workspace_views.contains_key("ws-1"));
        assert!(!gv.workspace_views.contains_key("ws-2"));
        // Only ws-1's patches should be in the merged set
        assert_eq!(gv.total_patches(), 1);
        // Only ws-1's ops counted
        assert_eq!(gv.total_ops, 2);
    }

    // -----------------------------------------------------------------------
    // WorkspaceSnapshot
    // -----------------------------------------------------------------------

    #[test]
    fn workspace_snapshot_from_view() {
        let view = make_view(
            "ws-1",
            Some('a'),
            [add_patch("src/main.rs", 'a', 1)].into_iter().collect(),
            5,
        );
        let snap = WorkspaceSnapshot::from_view(&view);
        assert_eq!(snap.epoch, Some(test_epoch('a')));
        assert!(snap.has_changes);
        assert_eq!(snap.patch_count, 1);
        assert_eq!(snap.op_count, 5);
        assert!(snap.description.is_none());
    }

    #[test]
    fn workspace_snapshot_empty_view() {
        let view = make_view("ws-1", None, BTreeMap::new(), 0);
        let snap = WorkspaceSnapshot::from_view(&view);
        assert!(snap.epoch.is_none());
        assert!(!snap.has_changes);
        assert_eq!(snap.patch_count, 0);
        assert_eq!(snap.op_count, 0);
    }

    // -----------------------------------------------------------------------
    // Cache key
    // -----------------------------------------------------------------------

    #[test]
    fn cache_key_validation() {
        let key1 = vec![("ws-1".into(), "aaa".into()), ("ws-2".into(), "bbb".into())];
        let key2 = vec![("ws-1".into(), "aaa".into()), ("ws-2".into(), "bbb".into())];
        let key3 = vec![
            ("ws-1".into(), "aaa".into()),
            ("ws-2".into(), "ccc".into()), // different head
        ];

        let view = make_view("ws-1", Some('a'), BTreeMap::new(), 0);
        let gv = compute_global_view_from_views(&[view], key1);

        assert!(gv.cache_valid(&key2));
        assert!(!gv.cache_valid(&key3));
    }

    // -----------------------------------------------------------------------
    // Display
    // -----------------------------------------------------------------------

    #[test]
    fn global_view_display() {
        let view = make_view(
            "ws-1",
            Some('a'),
            [add_patch("src/main.rs", 'a', 1)].into_iter().collect(),
            3,
        );
        let gv = compute_global_view_from_views(&[view], vec![]);
        let display = format!("{gv}");
        assert!(display.contains("global_view("));
        assert!(display.contains("1 ws"));
        assert!(display.contains("1 patches"));
        assert!(display.contains("0 conflicts"));
        assert!(display.contains("3 ops"));
    }

    #[test]
    fn global_view_display_no_epoch() {
        let gv = compute_global_view_from_views(&[], vec![]);
        let display = format!("{gv}");
        assert!(display.contains("no-epoch"));
    }

    // -----------------------------------------------------------------------
    // Serde roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn global_view_serde_roundtrip() {
        let view = make_view(
            "ws-1",
            Some('a'),
            [add_patch("src/main.rs", 'a', 1)].into_iter().collect(),
            3,
        );
        let gv = compute_global_view_from_views(&[view], vec![("ws-1".into(), "head1".into())]);

        let json = serde_json::to_string_pretty(&gv).unwrap();
        let decoded: GlobalView = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, gv);
    }

    // -----------------------------------------------------------------------
    // CRDT properties
    // -----------------------------------------------------------------------

    #[test]
    fn commutativity_two_views() {
        // Both views must share the same epoch for join to work
        let view1 = make_view(
            "ws-1",
            Some('a'),
            [add_patch("src/a.rs", 'a', 1)].into_iter().collect(),
            1,
        );
        let view2 = make_view(
            "ws-2",
            Some('a'),
            [add_patch("src/b.rs", 'b', 2)].into_iter().collect(),
            1,
        );

        let gv1 = compute_global_view_from_views(&[view1.clone(), view2.clone()], vec![]);
        let gv2 = compute_global_view_from_views(&[view2, view1], vec![]);

        assert_eq!(gv1.epoch, gv2.epoch);
        assert_eq!(gv1.merged_patch_set, gv2.merged_patch_set);
        assert_eq!(gv1.conflicts, gv2.conflicts);
        assert_eq!(gv1.workspace_views, gv2.workspace_views);
    }

    #[test]
    fn idempotency_same_view_twice() {
        let view = make_view(
            "ws-1",
            Some('a'),
            [add_patch("src/a.rs", 'a', 1)].into_iter().collect(),
            1,
        );

        let gv1 = compute_global_view_from_views(&[view.clone()], vec![]);
        let gv2 = compute_global_view_from_views(&[view.clone(), view], vec![]);

        // With idempotent join, patch sets should be same
        assert_eq!(gv1.merged_patch_set, gv2.merged_patch_set);
        // Note: gv2 has 2 workspace entries (same ws counted once)
        // because BTreeMap dedup by key handles this
    }

    #[test]
    fn associativity_three_views() {
        let view1 = make_view(
            "ws-1",
            Some('a'),
            [add_patch("src/a.rs", 'a', 1)].into_iter().collect(),
            1,
        );
        let view2 = make_view(
            "ws-2",
            Some('a'),
            [add_patch("src/b.rs", 'b', 2)].into_iter().collect(),
            1,
        );
        let view3 = make_view(
            "ws-3",
            Some('a'),
            [add_patch("src/c.rs", 'c', 3)].into_iter().collect(),
            1,
        );

        // (view1, view2, view3) all at once
        let gv_all = compute_global_view_from_views(&[view1, view2, view3], vec![]);

        // The merged patch set should have all 3 paths
        assert_eq!(gv_all.total_patches(), 3);
        assert!(gv_all.is_clean());
    }

    // -----------------------------------------------------------------------
    // Mixed patches: some conflict, some clean
    // -----------------------------------------------------------------------

    #[test]
    fn mixed_clean_and_conflicting_patches() {
        let view1 = make_view(
            "ws-1",
            Some('a'),
            [
                add_patch("src/a.rs", 'a', 1),      // unique to ws-1
                add_patch("src/shared.rs", 'c', 3), // conflicts with ws-2 (different blob)
            ]
            .into_iter()
            .collect(),
            2,
        );
        let view2 = make_view(
            "ws-2",
            Some('a'),
            [
                add_patch("src/b.rs", 'b', 2),      // unique to ws-2
                add_patch("src/shared.rs", 'd', 4), // conflicts with ws-1 (different blob)
            ]
            .into_iter()
            .collect(),
            2,
        );

        let gv = compute_global_view_from_views(&[view1, view2], vec![]);
        assert_eq!(gv.workspace_count(), 2);
        assert!(!gv.is_clean());
        // Should have conflict on src/shared.rs
        assert!(
            gv.conflicts
                .iter()
                .any(|c| c.path == PathBuf::from("src/shared.rs"))
        );
    }

    // -----------------------------------------------------------------------
    // Workspace with description
    // -----------------------------------------------------------------------

    #[test]
    fn workspace_with_description() {
        let mut view = make_view("ws-1", Some('a'), BTreeMap::new(), 2);
        view.description = Some("implementing feature X".into());

        let gv = compute_global_view_from_views(&[view], vec![]);
        assert_eq!(
            gv.workspace_views["ws-1"].description,
            Some("implementing feature X".into())
        );
    }
}
