//! gix-backed tree-to-tree diff.

use gix::objs::TreeRefIter;

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::{ChangeType, DiffEntry, EntryMode, FileStatus, GitOid};

/// Clamp a user-supplied similarity percentage (0-100) to a `[0.0, 1.0]`
/// `f32` suitable for `gix_diff::Rewrites::percentage`.
fn similarity_fraction(pct: u32) -> f32 {
    f32::from(u16::try_from(pct.min(100)).unwrap_or(100)) / 100.0
}

/// Convert a `GitOid` to a gix `ObjectId`.
fn to_gix_oid(oid: GitOid) -> gix::ObjectId {
    gix::ObjectId::from(*oid.as_bytes())
}

/// Convert a gix `ObjectId` to our `GitOid`.
fn from_gix_oid(oid: gix::ObjectId) -> GitOid {
    let bytes: [u8; 20] = oid.as_slice().try_into().expect("SHA-1 is 20 bytes");
    GitOid::from_bytes(bytes)
}

/// Convert a gix `EntryMode` to our `EntryMode`.
const fn convert_entry_mode(mode: gix::objs::tree::EntryMode) -> EntryMode {
    match mode.kind() {
        gix::objs::tree::EntryKind::Blob => EntryMode::Blob,
        gix::objs::tree::EntryKind::BlobExecutable => EntryMode::BlobExecutable,
        gix::objs::tree::EntryKind::Tree => EntryMode::Tree,
        gix::objs::tree::EntryKind::Link => EntryMode::Link,
        gix::objs::tree::EntryKind::Commit => EntryMode::Commit,
    }
}

pub fn diff_trees(
    repo: &GixRepo,
    old: Option<GitOid>,
    new: GitOid,
) -> Result<Vec<DiffEntry>, GitError> {
    let gix_repo = &repo.repo;

    // Load old tree data (empty bytes for None → empty tree).
    let old_tree_data = match old {
        Some(oid) => {
            let obj =
                gix_repo
                    .find_object(to_gix_oid(oid))
                    .map_err(|e| GitError::BackendError {
                        message: format!("failed to find old tree {oid}: {e}"),
                    })?;
            obj.data.clone()
        }
        None => Vec::new(),
    };

    // Load new tree data.
    let new_tree_data = gix_repo
        .find_object(to_gix_oid(new))
        .map_err(|e| GitError::BackendError {
            message: format!("failed to find new tree {new}: {e}"),
        })?
        .data
        .clone();

    let old_iter = TreeRefIter::from_bytes(&old_tree_data);
    let new_iter = TreeRefIter::from_bytes(&new_tree_data);

    let mut recorder = gix::diff::tree::Recorder::default();
    gix::diff::tree(
        old_iter,
        new_iter,
        gix::diff::tree::State::default(),
        gix_repo,
        &mut recorder,
    )
    .map_err(|e| GitError::BackendError {
        message: format!("tree diff failed: {e}"),
    })?;

    let entries = recorder
        .records
        .into_iter()
        .filter_map(|change| {
            match change {
                gix::diff::tree::recorder::Change::Addition {
                    entry_mode,
                    oid,
                    path,
                    ..
                } => {
                    // Skip tree entries — we only want file-level changes.
                    if entry_mode.is_tree() {
                        return None;
                    }
                    Some(DiffEntry {
                        path: path.to_string(),
                        change_type: ChangeType::Added,
                        old_oid: GitOid::ZERO,
                        new_oid: from_gix_oid(oid),
                        old_mode: None,
                        new_mode: Some(convert_entry_mode(entry_mode)),
                    })
                }
                gix::diff::tree::recorder::Change::Deletion {
                    entry_mode,
                    oid,
                    path,
                    ..
                } => {
                    if entry_mode.is_tree() {
                        return None;
                    }
                    Some(DiffEntry {
                        path: path.to_string(),
                        change_type: ChangeType::Deleted,
                        old_oid: from_gix_oid(oid),
                        new_oid: GitOid::ZERO,
                        old_mode: Some(convert_entry_mode(entry_mode)),
                        new_mode: None,
                    })
                }
                gix::diff::tree::recorder::Change::Modification {
                    previous_entry_mode,
                    previous_oid,
                    entry_mode,
                    oid,
                    path,
                } => {
                    if entry_mode.is_tree() {
                        return None;
                    }
                    Some(DiffEntry {
                        path: path.to_string(),
                        change_type: ChangeType::Modified,
                        old_oid: from_gix_oid(previous_oid),
                        new_oid: from_gix_oid(oid),
                        old_mode: Some(convert_entry_mode(previous_entry_mode)),
                        new_mode: Some(convert_entry_mode(entry_mode)),
                    })
                }
            }
        })
        .collect();

    Ok(entries)
}

/// Tree-to-tree diff with rename detection.
///
/// Unlike [`diff_trees`], this function runs gix's rewrite tracker so that
/// matching delete+add pairs above `similarity_pct` similarity collapse into
/// a single [`ChangeType::Renamed`] entry at the destination path, with the
/// original path carried in `from`.
///
/// `similarity_pct` is clamped to `0..=100`; `100` requires an exact content
/// match (pure rename / mode change only), values below 100 enable similarity-
/// based matching via gix's edit-distance algorithm. A common default is 50,
/// which matches git's built-in rename-threshold.
#[expect(
    clippy::too_many_lines,
    reason = "maps every gix diff variant to maw diff entries"
)]
pub fn diff_trees_with_renames(
    repo: &GixRepo,
    old: Option<GitOid>,
    new: GitOid,
    similarity_pct: u32,
) -> Result<Vec<DiffEntry>, GitError> {
    let gix_repo = &repo.repo;

    // Resolve trees. `None` ⇒ empty tree.
    let empty_tree = gix_repo.empty_tree();
    let old_tree_ref;
    let old_tree = match old {
        Some(oid) => {
            old_tree_ref = gix_repo
                .find_tree(to_gix_oid(oid))
                .map_err(|e| GitError::NotFound {
                    message: format!("old tree {oid}: {e}"),
                })?;
            &old_tree_ref
        }
        None => &empty_tree,
    };
    let new_tree_ref = gix_repo
        .find_tree(to_gix_oid(new))
        .map_err(|e| GitError::NotFound {
            message: format!("new tree {new}: {e}"),
        })?;

    // Configure rename-aware options. `gix::diff::Rewrites::percentage` is
    // an `Option<f32>` in [0.0, 1.0]; 1.0 means exact match only.
    let rewrites = gix::diff::Rewrites {
        copies: None,
        percentage: Some(similarity_fraction(similarity_pct)),
        limit: 1000,
        track_empty: false,
    };
    let opts = gix::diff::Options::default().with_rewrites(Some(rewrites));

    let changes = gix_repo
        .diff_tree_to_tree(old_tree, &new_tree_ref, opts)
        .map_err(|e| GitError::BackendError {
            message: format!("tree_to_tree diff failed: {e}"),
        })?;

    let mut entries: Vec<DiffEntry> = Vec::new();
    for change in changes {
        match change {
            gix::diff::tree_with_rewrites::Change::Addition {
                location,
                entry_mode,
                id,
                ..
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                entries.push(DiffEntry {
                    path: location.to_string(),
                    change_type: ChangeType::Added,
                    old_oid: GitOid::ZERO,
                    new_oid: from_gix_oid(id),
                    old_mode: None,
                    new_mode: Some(convert_entry_mode(entry_mode)),
                });
            }
            gix::diff::tree_with_rewrites::Change::Deletion {
                location,
                entry_mode,
                id,
                ..
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                entries.push(DiffEntry {
                    path: location.to_string(),
                    change_type: ChangeType::Deleted,
                    old_oid: from_gix_oid(id),
                    new_oid: GitOid::ZERO,
                    old_mode: Some(convert_entry_mode(entry_mode)),
                    new_mode: None,
                });
            }
            gix::diff::tree_with_rewrites::Change::Modification {
                location,
                previous_entry_mode,
                previous_id,
                entry_mode,
                id,
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                entries.push(DiffEntry {
                    path: location.to_string(),
                    change_type: ChangeType::Modified,
                    old_oid: from_gix_oid(previous_id),
                    new_oid: from_gix_oid(id),
                    old_mode: Some(convert_entry_mode(previous_entry_mode)),
                    new_mode: Some(convert_entry_mode(entry_mode)),
                });
            }
            gix::diff::tree_with_rewrites::Change::Rewrite {
                source_location,
                source_entry_mode,
                source_id,
                entry_mode,
                id,
                location,
                copy,
                ..
            } => {
                // Only emit renames (not copies) as the Renamed variant.
                // Copies have no pre-existing single source being consumed;
                // if we ever want them, extend ChangeType. For now, a copy
                // surfaces as a plain Addition to keep semantics identical
                // to the non-rename-aware path.
                if copy {
                    if entry_mode.is_tree() {
                        continue;
                    }
                    entries.push(DiffEntry {
                        path: location.to_string(),
                        change_type: ChangeType::Added,
                        old_oid: GitOid::ZERO,
                        new_oid: from_gix_oid(id),
                        old_mode: None,
                        new_mode: Some(convert_entry_mode(entry_mode)),
                    });
                    continue;
                }
                entries.push(DiffEntry {
                    path: location.to_string(),
                    change_type: ChangeType::Renamed {
                        from: source_location.to_string(),
                    },
                    old_oid: from_gix_oid(source_id),
                    new_oid: from_gix_oid(id),
                    old_mode: Some(convert_entry_mode(source_entry_mode)),
                    new_mode: Some(convert_entry_mode(entry_mode)),
                });
            }
        }
    }

    Ok(entries)
}

/// Resolve a commit or tree OID to a tree OID.
///
/// If `oid` is a commit, returns its tree OID. If `oid` is already a tree,
/// returns it unchanged. Other object kinds produce an error.
fn resolve_to_tree_oid(repo: &GixRepo, oid: GitOid) -> Result<GitOid, GitError> {
    let gix_oid = to_gix_oid(oid);
    let obj = repo
        .repo
        .find_object(gix_oid)
        .map_err(|e| GitError::NotFound {
            message: format!("object {oid}: {e}"),
        })?;
    match obj.kind {
        gix::object::Kind::Commit => {
            let commit = obj.into_commit();
            let tree_id = commit
                .tree_id()
                .map_err(|e| GitError::BackendError {
                    message: format!("failed to get tree from commit {oid}: {e}"),
                })?
                .detach();
            Ok(from_gix_oid(tree_id))
        }
        gix::object::Kind::Tree => Ok(oid),
        other => Err(GitError::BackendError {
            message: format!("expected commit or tree, got {other}"),
        }),
    }
}

/// Categorized name-status pairs between a commit (or tree) and the current
/// working tree, including untracked files.
///
/// Mirrors the union of:
/// - `git diff --name-status <base>` (committed + uncommitted changes vs base)
/// - `git ls-files --others --exclude-standard` (untracked files)
///
/// `base` may be either a commit OID or a tree OID. The current working tree
/// is sampled at the repository's workdir.
///
/// Path conflicts are resolved with the same precedence as the legacy
/// porcelain pipeline: a path that appears as Added wins over Modified.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NameStatusPairs {
    /// Paths that exist in the worktree but not in `base` (additions and
    /// untracked files).
    pub added: Vec<String>,
    /// Paths whose blob content or mode differs between `base` and the
    /// worktree.
    pub modified: Vec<String>,
    /// Paths present in `base` but missing from the worktree.
    pub deleted: Vec<String>,
}

/// Compute add/modify/delete pairs between a commit and the current working
/// tree, including untracked files.
///
/// Combines [`diff_trees`] (tree-to-tree, commit→HEAD) with
/// [`crate::status_impl::status_head_to_worktree`] (HEAD→worktree,
/// including staged and untracked) to produce the same categorized output
/// the legacy `git diff --name-status <base>` + `git ls-files --others
/// --exclude-standard` pipeline produced. Using the plain index→worktree
/// `status` here would silently drop staged-but-not-re-edited changes,
/// losing user work at merge time (bn-pfh7).
///
/// Path deduplication rules:
/// - A path Added in tree-diff and Modified in status → Added.
/// - A path Modified in tree-diff and Deleted in status → Deleted.
/// - A path Deleted in tree-diff and Added in status (re-added) → Modified.
///
/// Used by the git workspace backend's `snapshot()` to detect agent changes
/// relative to the workspace's base epoch.
///
/// # Errors
/// Returns a `GitError` if either the tree diff or the worktree status
/// pipeline fails.
pub fn diff_name_status_pairs(repo: &GixRepo, base: GitOid) -> Result<NameStatusPairs, GitError> {
    use std::collections::BTreeSet;

    let base_tree = resolve_to_tree_oid(repo, base)?;

    // HEAD tree for the committed-changes portion of the diff.
    // No HEAD (orphan or pre-init) → fall back to `base`, yielding an empty
    // committed-changes diff.
    let head_oid = crate::refs_impl::rev_parse_opt(repo, "HEAD")?.unwrap_or(base);
    let head_tree = resolve_to_tree_oid(repo, head_oid)?;

    let mut added: BTreeSet<String> = BTreeSet::new();
    let mut modified: BTreeSet<String> = BTreeSet::new();
    let mut deleted: BTreeSet<String> = BTreeSet::new();

    // 1. Committed changes: diff base_tree → head_tree.
    let tree_changes = diff_trees(repo, Some(base_tree), head_tree)?;
    for change in tree_changes {
        match change.change_type {
            ChangeType::Added => {
                added.insert(change.path);
            }
            ChangeType::Modified => {
                modified.insert(change.path);
            }
            ChangeType::Deleted => {
                deleted.insert(change.path);
            }
            ChangeType::Renamed { from } => {
                deleted.insert(from);
                added.insert(change.path);
            }
        }
    }

    // 2. Working-tree changes: HEAD → worktree (incl. staged + untracked).
    let status_entries = crate::status_impl::status_head_to_worktree(repo)?;
    for entry in status_entries {
        match entry.status {
            FileStatus::Added | FileStatus::Renamed | FileStatus::Untracked => {
                added.insert(entry.path);
            }
            FileStatus::Modified => {
                modified.insert(entry.path);
            }
            FileStatus::Deleted => {
                deleted.insert(entry.path);
            }
        }
    }

    // Reconcile cross-bucket overlaps so each path lives in exactly one
    // bucket. This function reconstructs the legacy *single* `git diff
    // --name-status <base>` (a direct base→worktree diff) by composing two
    // half-diffs: base→HEAD (tree) ∪ HEAD→worktree (status). Composition
    // cannot observe base↔worktree cancellation: a path Added base→HEAD and
    // then Deleted in the worktree nets to "absent in base, absent in
    // worktree" — a direct base→worktree diff emits *nothing*, but the
    // composed sets place it in BOTH `added` and `deleted`. Blindly
    // reclassifying every such path to Modified resurrects a user-deleted
    // file into the merge (collect.rs reads `HEAD:<path>`), losing the
    // deletion. Resolve each cross path by its TRUE net state vs `base`
    // (presence in `base_tree` vs the worktree), exactly as a direct
    // base→worktree diff would. A path appearing in both `modified` and
    // `deleted` ended up deleted in the worktree, so deleted wins. A path
    // in both `added` and `modified` was newly added overall, so added wins.
    let mut reclassified_modified: BTreeSet<String> = BTreeSet::new();
    let cross_added_deleted: Vec<String> = added.intersection(&deleted).cloned().collect();
    for p in &cross_added_deleted {
        added.remove(p);
        deleted.remove(p);
        let in_base =
            crate::objects_impl::read_blob_at_path(repo, base_tree, p.as_str())?.is_some();
        let on_disk = repo
            .workdir()
            .is_some_and(|wd| wd.join(p).symlink_metadata().is_ok());
        match (in_base, on_disk) {
            // Absent in base AND absent in worktree → no net change vs
            // base (e.g. a scratch file committed in the workspace then
            // `rm`-ed without committing the removal). Legacy `git diff
            // --name-status <base>` emits nothing; dropping it here stops
            // the merge from resurrecting the deleted path.
            (false, false) => {}
            // Present in base, gone from the worktree → net deletion.
            (true, false) => {
                deleted.insert(p.clone());
            }
            // Absent in base, present in the worktree → net addition.
            (false, true) => {
                added.insert(p.clone());
            }
            // Present in both (deleted on one half, re-added on the
            // other) → a modification, matching the documented rule.
            (true, true) => {
                reclassified_modified.insert(p.clone());
            }
        }
    }
    // Deleted wins over Modified.
    for p in &deleted {
        modified.remove(p);
    }
    // Added wins over Modified.
    for p in &added {
        modified.remove(p);
    }
    modified.extend(reclassified_modified);

    Ok(NameStatusPairs {
        added: added.into_iter().collect(),
        modified: modified.into_iter().collect(),
        deleted: deleted.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests_bn_uyk3 {
    //! Regression: `diff_name_status_pairs` reconstructs the legacy single
    //! `git diff --name-status <base>` (a direct base→worktree diff) by
    //! composing base→HEAD (tree) ∪ HEAD→worktree (status). The
    //! composition must not resurrect a path that was added base→HEAD and
    //! then deleted in the worktree (net: absent vs base) — a direct
    //! base→worktree diff emits nothing for it.

    use super::*;
    use crate::test_support::{commit_all, init_test_repo_with_commit};

    fn pairs_for(root: &std::path::Path, base: &str) -> NameStatusPairs {
        let repo = crate::GixRepo::open(root).expect("open repo");
        let base_oid: GitOid = base.parse().expect("parse base oid");
        diff_name_status_pairs(&repo, base_oid).expect("diff_name_status_pairs")
    }

    /// A file added in a commit *after* base, then `rm`-ed from the
    /// worktree without committing the removal, nets to "absent in base,
    /// absent in worktree". It must appear in NONE of the buckets — the
    /// pre-fix code reclassified it to `modified`, causing the merge to
    /// re-inject the user-deleted file from `HEAD:<path>`.
    #[test]
    fn added_then_worktree_deleted_is_dropped() {
        let (dir, root, base) = init_test_repo_with_commit();
        std::fs::write(root.join("scratch.txt"), "temp").expect("write scratch");
        let _ = commit_all(&root, "add scratch.txt");
        std::fs::remove_file(root.join("scratch.txt")).expect("rm scratch");

        let p = pairs_for(&root, &base);
        assert!(
            !p.added.iter().any(|x| x == "scratch.txt")
                && !p.modified.iter().any(|x| x == "scratch.txt")
                && !p.deleted.iter().any(|x| x == "scratch.txt"),
            "added-then-worktree-deleted must net to no change vs base, got: {p:#?}",
        );
        drop(dir);
    }

    /// Normal-case sanity: a genuinely new file (absent in base, present
    /// in the worktree) is still reported as `added`, and a file present
    /// in base but removed from the worktree is still `deleted`.
    #[test]
    fn normal_add_and_delete_still_classified() {
        let (dir, root, base) = init_test_repo_with_commit();
        // README.md is the committed seed from the shared helper.
        std::fs::write(root.join("brand_new.txt"), "new").expect("write new");
        std::fs::remove_file(root.join("README.md")).expect("rm seed");

        let p = pairs_for(&root, &base);
        assert!(
            p.added.iter().any(|x| x == "brand_new.txt"),
            "genuinely new file must be `added`, got: {p:#?}",
        );
        assert!(
            p.deleted.iter().any(|x| x == "README.md"),
            "base file removed from worktree must be `deleted`, got: {p:#?}",
        );
        drop(dir);
    }
}
