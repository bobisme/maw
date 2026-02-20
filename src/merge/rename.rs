//! Rename-aware merge using FileId (§5.8).
//!
//! After path-based partitioning, this module detects cases where the same
//! [`FileId`] appears under different paths in different workspaces — indicating
//! a rename. It rewrites the partition result to handle these scenarios:
//!
//! 1. **Rename + edit**: ws-A renames `foo→bar`, ws-B edits `foo`.
//!    Same FileId → merge ws-B's edits to `bar` (the renamed path).
//!
//! 2. **Divergent rename**: ws-A renames `foo→bar`, ws-B renames `foo→baz`.
//!    Same FileId, different destinations → [`RenameConflict::DivergentRename`].
//!
//! 3. **Rename + delete**: ws-A renames `foo→bar`, ws-B deletes `foo`.
//!    Same FileId → [`RenameConflict::RenameDelete`].
//!
//! 4. **Rename + edit (destination)**: ws-A renames `foo→bar`, ws-B adds a
//!    *new* file at `bar` with a different FileId → path conflict on `bar`
//!    (handled by normal path-based resolve, not here).
//!
//! # Algorithm
//!
//! 1. Build a `FileId → Vec<(WorkspaceId, Path, PathEntry)>` index from all
//!    partition entries.
//! 2. For each FileId that appears under multiple paths:
//!    - Classify the scenario (rename+edit, divergent rename, etc.)
//!    - Rewrite the partition result accordingly.
//! 3. Return the rewritten partition result + any rename-specific conflicts.
//!
//! # Determinism
//!
//! - FileId index is built from a BTreeMap (sorted by path).
//! - Rename conflicts include sorted workspace IDs for commutativity.
//! - All outputs are deterministic given the same inputs.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::model::patch::FileId;
use crate::model::types::WorkspaceId;

use super::partition::{PartitionResult, PathEntry};
use super::types::ChangeKind;

// ---------------------------------------------------------------------------
// RenameConflict
// ---------------------------------------------------------------------------

/// A conflict detected during rename-aware merge analysis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenameConflict {
    /// Two workspaces renamed the same file to different destinations.
    ///
    /// The file identity is the same (same [`FileId`]), but the destination
    /// paths differ.
    DivergentRename {
        /// The stable file identity.
        file_id: FileId,
        /// The original path (in the epoch base).
        original_path: PathBuf,
        /// All (workspace, destination_path) pairs, sorted by workspace ID.
        destinations: Vec<(WorkspaceId, PathBuf)>,
    },

    /// One workspace renamed a file while another deleted the original.
    RenameDelete {
        /// The stable file identity.
        file_id: FileId,
        /// The original path (before rename/delete).
        original_path: PathBuf,
        /// The workspace that renamed the file and its destination.
        renamer: (WorkspaceId, PathBuf),
        /// The workspace that deleted the file.
        deleter: WorkspaceId,
    },
}

impl std::fmt::Display for RenameConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DivergentRename {
                file_id,
                original_path,
                destinations,
            } => {
                write!(
                    f,
                    "divergent rename: file {} ({})",
                    original_path.display(),
                    file_id
                )?;
                for (ws, dest) in destinations {
                    write!(f, "\n  {} → {}", ws, dest.display())?;
                }
                Ok(())
            }
            Self::RenameDelete {
                file_id,
                original_path,
                renamer,
                deleter,
            } => {
                write!(
                    f,
                    "rename/delete: file {} ({})\n  {} renamed to {}\n  {} deleted",
                    original_path.display(),
                    file_id,
                    renamer.0,
                    renamer.1.display(),
                    deleter
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// RenameAwareResult
// ---------------------------------------------------------------------------

/// Output of rename-aware partition rewriting.
#[derive(Clone, Debug)]
pub struct RenameAwareResult {
    /// The rewritten partition result with rename-aware path grouping.
    pub partition: PartitionResult,
    /// Rename-specific conflicts that couldn't be handled by path rewriting.
    pub rename_conflicts: Vec<RenameConflict>,
}

impl RenameAwareResult {
    /// Returns `true` if no rename conflicts were detected.
    #[must_use]
    pub fn has_rename_conflicts(&self) -> bool {
        !self.rename_conflicts.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Internal: FileId tracking entry
// ---------------------------------------------------------------------------

/// A tracked occurrence of a FileId in the partition.
#[derive(Clone, Debug)]
struct FileIdOccurrence {
    workspace_id: WorkspaceId,
    path: PathBuf,
    entry: PathEntry,
}

// ---------------------------------------------------------------------------
// apply_rename_awareness
// ---------------------------------------------------------------------------

/// Analyze a partition result for rename scenarios and rewrite it.
///
/// This function:
/// 1. Scans all entries (unique + shared) for FileId metadata.
/// 2. Groups entries by FileId.
/// 3. For FileIds that appear under multiple *different* paths, classifies
///    the rename scenario and rewrites the partition.
/// 4. Returns the rewritten partition + any rename-specific conflicts.
///
/// Entries without FileId metadata are left unchanged (Phase 1 compatibility).
///
/// # Determinism
///
/// All internal data structures use BTreeMap/sorted Vecs. The output is
/// deterministic given the same input partition.
pub fn apply_rename_awareness(partition: PartitionResult) -> RenameAwareResult {
    // Step 1: Build FileId → occurrences index.
    let mut file_id_index: BTreeMap<FileId, Vec<FileIdOccurrence>> = BTreeMap::new();

    // Track which paths have been consumed by rename handling so we can
    // exclude them from the final partition.
    let mut consumed_paths: std::collections::BTreeSet<(PathBuf, WorkspaceId)> =
        std::collections::BTreeSet::new();

    // Index unique entries.
    for (path, entry) in &partition.unique {
        if let Some(fid) = entry.file_id {
            file_id_index
                .entry(fid)
                .or_default()
                .push(FileIdOccurrence {
                    workspace_id: entry.workspace_id.clone(),
                    path: path.clone(),
                    entry: entry.clone(),
                });
        }
    }

    // Index shared entries.
    for (path, entries) in &partition.shared {
        for entry in entries {
            if let Some(fid) = entry.file_id {
                file_id_index
                    .entry(fid)
                    .or_default()
                    .push(FileIdOccurrence {
                        workspace_id: entry.workspace_id.clone(),
                        path: path.clone(),
                        entry: entry.clone(),
                    });
            }
        }
    }

    // Step 2: Find FileIds with multiple distinct paths (rename candidates).
    let mut rename_conflicts = Vec::new();
    // New entries to add to the partition (path → entries to merge).
    let mut rerouted_entries: BTreeMap<PathBuf, Vec<PathEntry>> = BTreeMap::new();

    for (file_id, occurrences) in &file_id_index {
        // Collect distinct paths for this FileId.
        let mut paths_seen: BTreeMap<PathBuf, Vec<&FileIdOccurrence>> = BTreeMap::new();
        for occ in occurrences {
            paths_seen.entry(occ.path.clone()).or_default().push(occ);
        }

        // If all occurrences are on the same path, no rename — skip.
        if paths_seen.len() <= 1 {
            continue;
        }

        // Multiple paths for the same FileId → rename scenario.
        // Classify paths by whether they contain Add entries (rename destinations)
        // or only Modify/Delete entries (original/edit locations).
        //
        // Key insight:
        // - Add at a path → this is a rename destination (file appeared here)
        // - Modify at a path → this is an edit at the original location
        // - Delete at a path → part of a rename (removed from old location)
        //
        // Scenarios:
        // - 2+ Add-paths → divergent rename
        // - 1 Add-path + Modify-paths → rename + edit
        // - 1 Add-path + Delete-only-paths → rename + delete (or just rename)

        let mut add_paths: Vec<(PathBuf, Vec<&FileIdOccurrence>)> = Vec::new();
        let mut modify_paths: Vec<(PathBuf, Vec<&FileIdOccurrence>)> = Vec::new();
        let mut delete_occurrences: Vec<&FileIdOccurrence> = Vec::new();

        for (path, occs) in &paths_seen {
            let has_add = occs
                .iter()
                .any(|o| matches!(o.entry.kind, ChangeKind::Added));
            let non_deletes: Vec<&FileIdOccurrence> = occs
                .iter()
                .filter(|o| !o.entry.is_deletion())
                .copied()
                .collect();

            for occ in occs {
                if occ.entry.is_deletion() {
                    delete_occurrences.push(occ);
                }
            }

            if has_add {
                // This path is a rename destination.
                if !non_deletes.is_empty() {
                    add_paths.push((path.clone(), non_deletes));
                }
            } else if !non_deletes.is_empty() {
                // This path has only modifies (original location edits).
                modify_paths.push((path.clone(), non_deletes));
            }
        }

        // Case 1: Divergent rename — 2+ paths have Add entries.
        if add_paths.len() >= 2 {
            let original = modify_paths
                .first()
                .map(|(p, _)| p.clone())
                .or_else(|| add_paths.first().map(|(p, _)| p.clone()))
                .unwrap_or_default();

            let mut destinations: Vec<(WorkspaceId, PathBuf)> = add_paths
                .iter()
                .flat_map(|(path, occs)| {
                    occs.iter()
                        .map(|o| (o.workspace_id.clone(), path.clone()))
                })
                .collect();
            destinations.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));

            rename_conflicts.push(RenameConflict::DivergentRename {
                file_id: *file_id,
                original_path: original,
                destinations,
            });

            // Mark all occurrences as consumed.
            for occ in occurrences {
                consumed_paths.insert((occ.path.clone(), occ.workspace_id.clone()));
            }
            continue;
        }

        // Case 2: Exactly one Add-path (the rename destination).
        if add_paths.len() == 1 {
            let (dest_path, dest_occs) = &add_paths[0];

            // Check for rename + delete: another workspace deleted the file
            // at a different path (and didn't add it elsewhere).
            let dest_workspaces: std::collections::BTreeSet<&str> = dest_occs
                .iter()
                .map(|o| o.workspace_id.as_str())
                .collect();
            let pure_deleters: Vec<&FileIdOccurrence> = delete_occurrences
                .iter()
                .filter(|o| {
                    !dest_workspaces.contains(o.workspace_id.as_str())
                        && o.path != *dest_path
                })
                .copied()
                .collect();

            // A pure deleter is a workspace that only deleted the file (no
            // modify or add at any path for this FileId).
            let workspaces_with_non_delete: std::collections::BTreeSet<&str> = occurrences
                .iter()
                .filter(|o| !o.entry.is_deletion())
                .map(|o| o.workspace_id.as_str())
                .collect();
            let actual_pure_deleters: Vec<&FileIdOccurrence> = pure_deleters
                .iter()
                .filter(|o| !workspaces_with_non_delete.contains(o.workspace_id.as_str()))
                .copied()
                .collect();

            if !actual_pure_deleters.is_empty() {
                // Rename + delete conflict.
                let renamer_occ = dest_occs.first().unwrap();
                let deleter_occ = actual_pure_deleters.first().unwrap();

                rename_conflicts.push(RenameConflict::RenameDelete {
                    file_id: *file_id,
                    original_path: deleter_occ.path.clone(),
                    renamer: (
                        renamer_occ.workspace_id.clone(),
                        dest_path.clone(),
                    ),
                    deleter: deleter_occ.workspace_id.clone(),
                });

                // Mark all occurrences as consumed.
                for occ in occurrences {
                    consumed_paths.insert((occ.path.clone(), occ.workspace_id.clone()));
                }
                continue;
            }

            // Case 3: Rename + edit. Reroute edits at old paths to the
            // rename destination so they merge together.
            for (path, occs) in &paths_seen {
                if path == dest_path {
                    continue; // Already at destination.
                }
                for occ in occs {
                    if occ.entry.is_deletion() {
                        // Deletions at the old path are expected side effects
                        // of a rename — consume them (don't propagate).
                        consumed_paths
                            .insert((occ.path.clone(), occ.workspace_id.clone()));
                    } else {
                        // Non-deletion at old path → reroute to dest path.
                        consumed_paths
                            .insert((occ.path.clone(), occ.workspace_id.clone()));
                        rerouted_entries
                            .entry(dest_path.clone())
                            .or_default()
                            .push(occ.entry.clone());
                    }
                }
            }

            // Also consume + reroute the dest path entries.
            for occ in dest_occs {
                consumed_paths.insert((occ.path.clone(), occ.workspace_id.clone()));
                rerouted_entries
                    .entry(dest_path.clone())
                    .or_default()
                    .push(occ.entry.clone());
            }
        }
    }

    // Step 3: Rebuild partition, excluding consumed paths and adding rerouted ones.
    let mut new_unique: Vec<(PathBuf, PathEntry)> = Vec::new();
    let mut new_shared: BTreeMap<PathBuf, Vec<PathEntry>> = BTreeMap::new();

    // Re-add non-consumed unique entries.
    for (path, entry) in partition.unique {
        let key = (path.clone(), entry.workspace_id.clone());
        if consumed_paths.contains(&key) {
            continue;
        }
        new_unique.push((path, entry));
    }

    // Re-add non-consumed shared entries.
    for (path, entries) in partition.shared {
        let remaining: Vec<PathEntry> = entries
            .into_iter()
            .filter(|e| !consumed_paths.contains(&(path.clone(), e.workspace_id.clone())))
            .collect();
        if remaining.len() == 1 {
            // Demoted from shared to unique.
            new_unique.push((path, remaining.into_iter().next().unwrap()));
        } else if remaining.len() > 1 {
            new_shared.insert(path, remaining);
        }
        // If empty, the path is fully consumed — don't add.
    }

    // Add rerouted entries.
    for (path, entries) in rerouted_entries {
        // Merge with any existing entries at this path.
        let target = new_shared.entry(path.clone()).or_default();
        target.extend(entries);
    }

    // Finalize: convert shared BTreeMap to sorted Vec, and ensure internal
    // entry order is by workspace ID.
    let mut final_shared: Vec<(PathBuf, Vec<PathEntry>)> = new_shared
        .into_iter()
        .map(|(path, mut entries)| {
            entries.sort_by(|a, b| a.workspace_id.as_str().cmp(b.workspace_id.as_str()));
            (path, entries)
        })
        .collect();

    // Paths that ended up with a single entry in shared should be unique.
    let mut truly_shared: Vec<(PathBuf, Vec<PathEntry>)> = Vec::new();
    for (path, entries) in final_shared {
        if entries.len() == 1 {
            new_unique.push((path, entries.into_iter().next().unwrap()));
        } else {
            truly_shared.push((path, entries));
        }
    }

    // Sort unique by path.
    new_unique.sort_by(|a, b| a.0.cmp(&b.0));

    // Sort rename conflicts for determinism.
    rename_conflicts.sort_by(|a, b| {
        let a_fid = match a {
            RenameConflict::DivergentRename { file_id, .. } => *file_id,
            RenameConflict::RenameDelete { file_id, .. } => *file_id,
        };
        let b_fid = match b {
            RenameConflict::DivergentRename { file_id, .. } => *file_id,
            RenameConflict::RenameDelete { file_id, .. } => *file_id,
        };
        a_fid.cmp(&b_fid)
    });

    RenameAwareResult {
        partition: PartitionResult {
            unique: new_unique,
            shared: truly_shared,
        },
        rename_conflicts,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::partition::{PartitionResult, PathEntry};
    use crate::merge::types::ChangeKind;
    use crate::model::patch::FileId;
    use crate::model::types::WorkspaceId;

    fn ws(name: &str) -> WorkspaceId {
        WorkspaceId::new(name).unwrap()
    }

    fn fid(n: u128) -> FileId {
        FileId::new(n)
    }

    fn entry_with_fid(
        name: &str,
        kind: ChangeKind,
        content: Option<&[u8]>,
        file_id: FileId,
    ) -> PathEntry {
        PathEntry::with_identity(
            ws(name),
            kind,
            content.map(|c| c.to_vec()),
            Some(file_id),
            None,
        )
    }

    fn entry_no_fid(name: &str, kind: ChangeKind, content: Option<&[u8]>) -> PathEntry {
        PathEntry::new(ws(name), kind, content.map(|c| c.to_vec()))
    }

    // -----------------------------------------------------------------------
    // No rename — passthrough
    // -----------------------------------------------------------------------

    #[test]
    fn no_rename_passthrough() {
        // Same FileId on same path in both workspaces → no rename, partition unchanged.
        let partition = PartitionResult {
            unique: vec![],
            shared: vec![(
                PathBuf::from("file.rs"),
                vec![
                    entry_with_fid("ws-a", ChangeKind::Modified, Some(b"a"), fid(1)),
                    entry_with_fid("ws-b", ChangeKind::Modified, Some(b"b"), fid(1)),
                ],
            )],
        };

        let result = apply_rename_awareness(partition);
        assert!(!result.has_rename_conflicts());
        assert_eq!(result.partition.shared.len(), 1);
        assert_eq!(result.partition.shared[0].0, PathBuf::from("file.rs"));
        assert_eq!(result.partition.shared[0].1.len(), 2);
    }

    #[test]
    fn no_file_id_passthrough() {
        // Entries without FileId are left unchanged.
        let partition = PartitionResult {
            unique: vec![(
                PathBuf::from("a.rs"),
                entry_no_fid("ws-a", ChangeKind::Added, Some(b"a")),
            )],
            shared: vec![(
                PathBuf::from("b.rs"),
                vec![
                    entry_no_fid("ws-a", ChangeKind::Modified, Some(b"x")),
                    entry_no_fid("ws-b", ChangeKind::Modified, Some(b"y")),
                ],
            )],
        };

        let result = apply_rename_awareness(partition);
        assert!(!result.has_rename_conflicts());
        assert_eq!(result.partition.unique.len(), 1);
        assert_eq!(result.partition.shared.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Rename + edit: reroute edits to renamed path
    // -----------------------------------------------------------------------

    #[test]
    fn rename_plus_edit_reroutes_to_destination() {
        // ws-a renames foo.rs → bar.rs (edit at bar.rs with fid=1)
        // ws-b edits foo.rs (edit at foo.rs with fid=1)
        // → Should reroute ws-b's edit to bar.rs and merge there.
        let partition = PartitionResult {
            unique: vec![
                (
                    PathBuf::from("bar.rs"),
                    entry_with_fid("ws-a", ChangeKind::Added, Some(b"renamed content"), fid(1)),
                ),
                (
                    PathBuf::from("foo.rs"),
                    entry_with_fid("ws-b", ChangeKind::Modified, Some(b"edited content"), fid(1)),
                ),
            ],
            shared: vec![],
        };

        let result = apply_rename_awareness(partition);
        assert!(!result.has_rename_conflicts());
        // Both entries should now be at bar.rs (shared).
        assert_eq!(result.partition.unique.len(), 0);
        assert_eq!(result.partition.shared.len(), 1);
        assert_eq!(result.partition.shared[0].0, PathBuf::from("bar.rs"));
        assert_eq!(result.partition.shared[0].1.len(), 2);

        // Both workspaces should be represented.
        let ws_ids: Vec<&str> = result.partition.shared[0]
            .1
            .iter()
            .map(|e| e.workspace_id.as_str())
            .collect();
        assert!(ws_ids.contains(&"ws-a"));
        assert!(ws_ids.contains(&"ws-b"));
    }

    #[test]
    fn rename_plus_edit_with_delete_at_old_path() {
        // ws-a: deletes foo.rs (fid=1) + adds bar.rs (fid=1) → rename
        // ws-b: edits foo.rs (fid=1)
        // The delete at foo.rs by ws-a should be consumed (it's part of rename).
        // ws-b's edit should reroute to bar.rs.
        let partition = PartitionResult {
            unique: vec![
                (
                    PathBuf::from("bar.rs"),
                    entry_with_fid("ws-a", ChangeKind::Added, Some(b"new content"), fid(1)),
                ),
            ],
            shared: vec![(
                PathBuf::from("foo.rs"),
                vec![
                    entry_with_fid("ws-a", ChangeKind::Deleted, None, fid(1)),
                    entry_with_fid("ws-b", ChangeKind::Modified, Some(b"edited"), fid(1)),
                ],
            )],
        };

        let result = apply_rename_awareness(partition);
        assert!(!result.has_rename_conflicts());
        // bar.rs should now have ws-a's add and ws-b's edit.
        assert_eq!(result.partition.shared.len(), 1);
        assert_eq!(result.partition.shared[0].0, PathBuf::from("bar.rs"));
        let entries = &result.partition.shared[0].1;
        assert_eq!(entries.len(), 2);
        // foo.rs should be gone (consumed).
        assert!(
            result.partition.unique.iter().all(|(p, _)| p != &PathBuf::from("foo.rs")),
            "foo.rs should not remain in unique"
        );
    }

    // -----------------------------------------------------------------------
    // Divergent rename
    // -----------------------------------------------------------------------

    #[test]
    fn divergent_rename_detected() {
        // ws-a renames file → dest_a.rs (fid=1)
        // ws-b renames file → dest_b.rs (fid=1)
        let partition = PartitionResult {
            unique: vec![
                (
                    PathBuf::from("dest_a.rs"),
                    entry_with_fid("ws-a", ChangeKind::Added, Some(b"a content"), fid(1)),
                ),
                (
                    PathBuf::from("dest_b.rs"),
                    entry_with_fid("ws-b", ChangeKind::Added, Some(b"b content"), fid(1)),
                ),
            ],
            shared: vec![],
        };

        let result = apply_rename_awareness(partition);
        assert!(result.has_rename_conflicts());
        assert_eq!(result.rename_conflicts.len(), 1);

        match &result.rename_conflicts[0] {
            RenameConflict::DivergentRename {
                file_id,
                destinations,
                ..
            } => {
                assert_eq!(*file_id, fid(1));
                assert_eq!(destinations.len(), 2);
                // Sorted by workspace ID.
                assert_eq!(destinations[0].0.as_str(), "ws-a");
                assert_eq!(destinations[1].0.as_str(), "ws-b");
            }
            _ => panic!("expected DivergentRename"),
        }

        // Both entries should be consumed (not in unique or shared).
        assert!(result.partition.unique.is_empty());
        assert!(result.partition.shared.is_empty());
    }

    // -----------------------------------------------------------------------
    // Rename + delete
    // -----------------------------------------------------------------------

    #[test]
    fn rename_delete_detected() {
        // ws-a adds bar.rs with fid=1 (rename destination)
        // ws-b deletes foo.rs with fid=1
        let partition = PartitionResult {
            unique: vec![
                (
                    PathBuf::from("bar.rs"),
                    entry_with_fid("ws-a", ChangeKind::Added, Some(b"new"), fid(1)),
                ),
                (
                    PathBuf::from("foo.rs"),
                    entry_with_fid("ws-b", ChangeKind::Deleted, None, fid(1)),
                ),
            ],
            shared: vec![],
        };

        let result = apply_rename_awareness(partition);
        assert!(result.has_rename_conflicts());
        assert_eq!(result.rename_conflicts.len(), 1);

        match &result.rename_conflicts[0] {
            RenameConflict::RenameDelete {
                file_id,
                original_path,
                renamer,
                deleter,
            } => {
                assert_eq!(*file_id, fid(1));
                assert_eq!(original_path, &PathBuf::from("foo.rs"));
                assert_eq!(renamer.0.as_str(), "ws-a");
                assert_eq!(renamer.1, PathBuf::from("bar.rs"));
                assert_eq!(deleter.as_str(), "ws-b");
            }
            _ => panic!("expected RenameDelete"),
        }

        // Both entries consumed.
        assert!(result.partition.unique.is_empty());
        assert!(result.partition.shared.is_empty());
    }

    // -----------------------------------------------------------------------
    // Mixed scenarios: rename + non-rename entries
    // -----------------------------------------------------------------------

    #[test]
    fn rename_with_unrelated_entries_preserved() {
        // ws-a: renames foo.rs → bar.rs (fid=1)
        // ws-b: edits foo.rs (fid=1)
        // ws-c: adds unrelated.rs (fid=99, no rename)
        let partition = PartitionResult {
            unique: vec![
                (
                    PathBuf::from("bar.rs"),
                    entry_with_fid("ws-a", ChangeKind::Added, Some(b"renamed"), fid(1)),
                ),
                (
                    PathBuf::from("foo.rs"),
                    entry_with_fid("ws-b", ChangeKind::Modified, Some(b"edited"), fid(1)),
                ),
                (
                    PathBuf::from("unrelated.rs"),
                    entry_with_fid("ws-c", ChangeKind::Added, Some(b"new file"), fid(99)),
                ),
            ],
            shared: vec![],
        };

        let result = apply_rename_awareness(partition);
        assert!(!result.has_rename_conflicts());
        // unrelated.rs should still be unique.
        assert!(result
            .partition
            .unique
            .iter()
            .any(|(p, _)| p == &PathBuf::from("unrelated.rs")));
        // bar.rs should be shared (ws-a + ws-b rerouted).
        assert_eq!(result.partition.shared.len(), 1);
        assert_eq!(result.partition.shared[0].0, PathBuf::from("bar.rs"));
    }

    // -----------------------------------------------------------------------
    // Multiple renames in same partition
    // -----------------------------------------------------------------------

    #[test]
    fn multiple_independent_renames() {
        // fid=1: ws-a renames old1.rs → new1.rs, ws-b edits old1.rs
        // fid=2: ws-c renames old2.rs → new2.rs, ws-d edits old2.rs
        let partition = PartitionResult {
            unique: vec![
                (
                    PathBuf::from("new1.rs"),
                    entry_with_fid("ws-a", ChangeKind::Added, Some(b"renamed1"), fid(1)),
                ),
                (
                    PathBuf::from("new2.rs"),
                    entry_with_fid("ws-c", ChangeKind::Added, Some(b"renamed2"), fid(2)),
                ),
                (
                    PathBuf::from("old1.rs"),
                    entry_with_fid("ws-b", ChangeKind::Modified, Some(b"edit1"), fid(1)),
                ),
                (
                    PathBuf::from("old2.rs"),
                    entry_with_fid("ws-d", ChangeKind::Modified, Some(b"edit2"), fid(2)),
                ),
            ],
            shared: vec![],
        };

        let result = apply_rename_awareness(partition);
        assert!(!result.has_rename_conflicts());
        assert_eq!(result.partition.unique.len(), 0);
        // Two shared paths: new1.rs and new2.rs.
        assert_eq!(result.partition.shared.len(), 2);
        let shared_paths: Vec<&PathBuf> = result.partition.shared.iter().map(|(p, _)| p).collect();
        assert!(shared_paths.contains(&&PathBuf::from("new1.rs")));
        assert!(shared_paths.contains(&&PathBuf::from("new2.rs")));
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn empty_partition_passthrough() {
        let partition = PartitionResult {
            unique: vec![],
            shared: vec![],
        };

        let result = apply_rename_awareness(partition);
        assert!(!result.has_rename_conflicts());
        assert!(result.partition.unique.is_empty());
        assert!(result.partition.shared.is_empty());
    }

    #[test]
    fn same_workspace_rename_not_a_cross_ws_rename() {
        // ws-a has the same fid=1 at two paths — this is unusual but could
        // happen if the collect step emits both old and new path for a rename.
        // Since both are from the same workspace, this shouldn't be treated
        // as a cross-workspace rename conflict.
        let partition = PartitionResult {
            unique: vec![
                (
                    PathBuf::from("new.rs"),
                    entry_with_fid("ws-a", ChangeKind::Added, Some(b"new"), fid(1)),
                ),
                (
                    PathBuf::from("old.rs"),
                    entry_with_fid("ws-a", ChangeKind::Deleted, None, fid(1)),
                ),
            ],
            shared: vec![],
        };

        let result = apply_rename_awareness(partition);
        // The delete at old.rs is consumed as part of the rename.
        // Only new.rs should remain.
        assert!(!result.has_rename_conflicts());
        // new.rs should be in unique (only ws-a contributes).
        assert_eq!(result.partition.unique.len(), 1);
        assert_eq!(result.partition.unique[0].0, PathBuf::from("new.rs"));
    }

    #[test]
    fn three_way_divergent_rename() {
        // Three workspaces rename the same file to three different destinations.
        let partition = PartitionResult {
            unique: vec![
                (
                    PathBuf::from("dest_a.rs"),
                    entry_with_fid("ws-a", ChangeKind::Added, Some(b"a"), fid(1)),
                ),
                (
                    PathBuf::from("dest_b.rs"),
                    entry_with_fid("ws-b", ChangeKind::Added, Some(b"b"), fid(1)),
                ),
                (
                    PathBuf::from("dest_c.rs"),
                    entry_with_fid("ws-c", ChangeKind::Added, Some(b"c"), fid(1)),
                ),
            ],
            shared: vec![],
        };

        let result = apply_rename_awareness(partition);
        assert!(result.has_rename_conflicts());
        assert_eq!(result.rename_conflicts.len(), 1);

        match &result.rename_conflicts[0] {
            RenameConflict::DivergentRename { destinations, .. } => {
                assert_eq!(destinations.len(), 3);
            }
            _ => panic!("expected DivergentRename"),
        }
    }

    // -----------------------------------------------------------------------
    // Display formatting
    // -----------------------------------------------------------------------

    #[test]
    fn rename_conflict_display_divergent() {
        let conflict = RenameConflict::DivergentRename {
            file_id: fid(42),
            original_path: PathBuf::from("src/old.rs"),
            destinations: vec![
                (ws("ws-a"), PathBuf::from("src/new_a.rs")),
                (ws("ws-b"), PathBuf::from("src/new_b.rs")),
            ],
        };
        let s = format!("{conflict}");
        assert!(s.contains("divergent rename"));
        assert!(s.contains("src/old.rs"));
        assert!(s.contains("src/new_a.rs"));
        assert!(s.contains("src/new_b.rs"));
    }

    #[test]
    fn rename_conflict_display_rename_delete() {
        let conflict = RenameConflict::RenameDelete {
            file_id: fid(7),
            original_path: PathBuf::from("src/file.rs"),
            renamer: (ws("ws-a"), PathBuf::from("src/renamed.rs")),
            deleter: ws("ws-b"),
        };
        let s = format!("{conflict}");
        assert!(s.contains("rename/delete"));
        assert!(s.contains("renamed"));
        assert!(s.contains("deleted"));
    }

    // -----------------------------------------------------------------------
    // Commutativity: apply_rename_awareness(partition(a,b)) same regardless
    // of workspace insertion order in the partition
    // -----------------------------------------------------------------------

    #[test]
    fn rename_reroute_is_commutative() {
        // Order 1: ws-a first, ws-b second.
        let part1 = PartitionResult {
            unique: vec![
                (
                    PathBuf::from("bar.rs"),
                    entry_with_fid("ws-a", ChangeKind::Added, Some(b"renamed"), fid(1)),
                ),
                (
                    PathBuf::from("foo.rs"),
                    entry_with_fid("ws-b", ChangeKind::Modified, Some(b"edited"), fid(1)),
                ),
            ],
            shared: vec![],
        };

        // Order 2: ws-b first, ws-a second (different unique order).
        let part2 = PartitionResult {
            unique: vec![
                (
                    PathBuf::from("bar.rs"),
                    entry_with_fid("ws-a", ChangeKind::Added, Some(b"renamed"), fid(1)),
                ),
                (
                    PathBuf::from("foo.rs"),
                    entry_with_fid("ws-b", ChangeKind::Modified, Some(b"edited"), fid(1)),
                ),
            ],
            shared: vec![],
        };

        let r1 = apply_rename_awareness(part1);
        let r2 = apply_rename_awareness(part2);

        // Both should produce the same result.
        assert_eq!(r1.rename_conflicts.len(), r2.rename_conflicts.len());
        assert_eq!(r1.partition.unique.len(), r2.partition.unique.len());
        assert_eq!(r1.partition.shared.len(), r2.partition.shared.len());

        // Same shared paths.
        for (i, ((p1, e1), (p2, e2))) in r1
            .partition
            .shared
            .iter()
            .zip(r2.partition.shared.iter())
            .enumerate()
        {
            assert_eq!(p1, p2, "shared path {i} differs");
            assert_eq!(e1.len(), e2.len(), "shared path {i} entry count differs");
        }
    }
}
