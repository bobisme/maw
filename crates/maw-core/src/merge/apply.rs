//! Apply a [`PatchSet`] to a [`ConflictTree`] (Phase 2 — unilateral over conflicts).
//!
//! This module is the fold primitive used by the structured-merge rebase
//! pipeline: given a running `ConflictTree` and a new `PatchSet`, produce an
//! updated tree.
//!
//! # Scope
//!
//! A "unilateral" patch is a single workspace's [`PatchSet`] — we are folding
//! it into a tree that may already carry conflicts from previous workspaces.
//! Phase 2 extends Phase 1 by propagating that patch into the per-path
//! [`Conflict`] records rather than passing conflicted paths through
//! unchanged.
//!
//! ## Clean paths (Phase 1, unchanged)
//!
//! * path in `tree.clean` + `Modified` → replace blob, preserve mode;
//! * path in `tree.clean` + `Deleted` → remove from `clean`;
//! * path absent from both maps + `Added` → insert into `clean`;
//! * path absent from both maps + `Modified` → insert into `clean` (upsert).
//!   This is necessary for the "follow-the-rename" semantics (bn-3525):
//!   `diff_patchset` emits renames as a `Deleted(from) + Modified(to)` pair,
//!   and the seeded tree may not contain `to` (because `to` did not yet exist
//!   at the epoch base). Without upsert semantics the rename's content would
//!   be silently dropped.
//! * path absent from both maps + `Deleted` → log a warning (unexpected) and
//!   skip — no base entry to remove.
//!
//! ## Conflicted paths (Phase 2)
//!
//! For a path already in `tree.conflicts`, the unilateral patch is applied to
//! each side of the conflict. V1 semantics treat the unilateral patch as the
//! workspace's **final word**: a unilateral `Modified` **replaces** every
//! side's content, a unilateral `Deleted` **collapses** the conflict to
//! clean absence. See [`apply_one_to_conflict`] for the per-variant table.
//!
//! `Added` on a conflicted path is treated as equivalent to `Modified`: the
//! path is already tracked (the pipeline's overlap detector pre-populates
//! `tree.conflicts` for add/add and content overlaps), so the unilateral
//! "first-time add" is really the workspace's final content for the path.
//! We dispatch to the same replace-each-side logic `Modified` uses.
//!
//! ### Convergence
//!
//! After applying a `Modified` patch to a `Conflict::Content`, every side now
//! carries the same blob OID (the new content). We detect this and promote
//! the path back into `tree.clean` with a fresh [`MaterializedEntry`]. Mode
//! is conservatively set to [`EntryMode::Blob`] — proper mode tracking across
//! conflict sides is a future-phase concern.
//!
//! ### V1 simplifications (documented TODOs)
//!
//! * **Per-side 3-way merge** — a unilateral `Modified` should ideally
//!   three-way-merge the new content into each side's version of the file,
//!   so that a workspace's typo fix can coexist with an ongoing conflict.
//!   V1 replaces each side wholesale; a follow-up bone will plumb real 3-way
//!   merges per side.
//! * **Mode tracking** — `MaterializedEntry` has a mode, but `Conflict`
//!   sides currently carry only a blob OID. When convergence collapses a
//!   conflict, we default to [`EntryMode::Blob`]; a follow-up bone will
//!   track per-side mode so executable-bit-only and symlink edits survive
//!   conflict resolution.
//! * **`DivergentRename`** — applying a unilateral patch to one of several
//!   divergent rename destinations is semantically fiddly (the other sides
//!   may still point at different paths with different content). V1 returns
//!   [`ApplyError::UnhandledConflictShape`] and defers the real work.
//! * **Non-content conflicts** — chmod conflicts, type-change conflicts, and
//!   submodule-boundary conflicts are not representable in the current
//!   [`Conflict`] enum. If the enum grows such variants, this module must
//!   explicitly refuse them via `UnhandledConflictShape`, never silently
//!   flatten them.

use tracing::warn;

use super::types::{ChangeKind, ConflictTree, EntryMode, FileChange, MaterializedEntry, PatchSet};
use crate::model::conflict::{Conflict, ConflictSide};
use crate::model::types::{EpochId, GitOid};

/// Errors returned by [`apply_unilateral_patchset`].
#[derive(Debug)]
pub enum ApplyError {
    /// The patch's epoch does not match the tree's `base_epoch`.
    EpochMismatch {
        /// The tree's base epoch.
        tree_base: EpochId,
        /// The patch's epoch.
        patch_epoch: EpochId,
    },
    /// A `FileChange` of kind `Added`/`Modified` was missing a blob OID.
    ///
    /// Phase 1 only accepts patches whose content changes carry a precomputed
    /// `blob` OID — byte-level hashing is the caller's responsibility.
    MissingBlob {
        /// The path of the offending change.
        path: std::path::PathBuf,
        /// The change kind that was missing a blob.
        kind: ChangeKind,
    },
    /// A unilateral `Added` change targeted a path that already appears in
    /// an existing conflict record.
    ///
    /// **V1 semantics (bn-3l5p)**: `Added` on a conflicted path is now
    /// treated as `Modified` — see [`apply_one_to_conflict`]. This variant
    /// is retained for future phases that might distinguish "first-time
    /// add" from "replace content" semantically, and for conflict shapes
    /// that genuinely cannot accept an `Added` (none in V1).
    UnexpectedAddOnConflict {
        /// The path that was re-added on top of a conflict.
        path: std::path::PathBuf,
        /// The shape of the existing conflict (for diagnostics).
        existing_shape: &'static str,
    },
    /// A unilateral `Modified` change targeted a path that is in an
    /// `AddAdd` conflict.
    ///
    /// An `AddAdd` conflict has no agreed-upon base — by definition the
    /// file did not exist before. A "modify" operation against such a path
    /// is a collect-layer error.
    UnexpectedModifyOnAddAdd {
        /// The path that was modified on top of an `AddAdd` conflict.
        path: std::path::PathBuf,
    },
    /// The tree carries a [`Conflict`] variant that [`apply_unilateral_patchset`]
    /// does not know how to propagate.
    ///
    /// Currently emitted for [`Conflict::DivergentRename`] (Phase 2 defers
    /// proper handling) and reserved for future non-content conflict shapes
    /// such as chmod/type-change/submodule-boundary conflicts. The key
    /// invariant this variant upholds: **we never silently flatten a
    /// conflict we can't reason about** — we error loudly so the rebase
    /// pipeline can surface the gap.
    ///
    /// TODO(follow-up bone): file DivergentRename + non-content conflict
    /// handling as its own phase.
    UnhandledConflictShape {
        /// The path of the unhandled conflict.
        path: std::path::PathBuf,
        /// The shape of the conflict (variant name, for diagnostics).
        shape: &'static str,
    },
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EpochMismatch {
                tree_base,
                patch_epoch,
            } => write!(
                f,
                "patch epoch {patch_epoch} does not match tree base epoch {tree_base}"
            ),
            Self::MissingBlob { path, kind } => write!(
                f,
                "file change at {} (kind={kind}) is missing its blob OID",
                path.display()
            ),
            Self::UnexpectedAddOnConflict {
                path,
                existing_shape,
            } => write!(
                f,
                "unexpected Added on conflicted path {} (existing shape: {existing_shape})",
                path.display()
            ),
            Self::UnexpectedModifyOnAddAdd { path } => write!(
                f,
                "unexpected Modified on path {} which is in an AddAdd conflict (no base to modify)",
                path.display()
            ),
            Self::UnhandledConflictShape { path, shape } => write!(
                f,
                "conflict shape {shape} at {} is not yet handled by apply_unilateral_patchset",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ApplyError {}

/// Apply a single workspace's [`PatchSet`] to a [`ConflictTree`].
///
/// See the module docs for the full behavior table. Returns an updated tree
/// on success; returns an error if the patch's epoch does not match the
/// tree's base epoch, if an add/modify change lacks a precomputed blob OID,
/// or if the patch targets a conflict shape that Phase 2 refuses to silently
/// flatten.
///
/// This function is pure: `tree` is consumed and a new tree is returned.
///
/// # Errors
///
/// - [`ApplyError::EpochMismatch`] — the patch is based on a different epoch.
/// - [`ApplyError::MissingBlob`] — an add/modify change had no `blob` OID.
/// - [`ApplyError::UnexpectedAddOnConflict`] — `Added` on an already-tracked path.
/// - [`ApplyError::UnexpectedModifyOnAddAdd`] — `Modified` on an `AddAdd` conflict.
/// - [`ApplyError::UnhandledConflictShape`] — conflict variant not yet handled.
pub fn apply_unilateral_patchset(
    mut tree: ConflictTree,
    patch: PatchSet,
) -> Result<ConflictTree, ApplyError> {
    if patch.epoch != tree.base_epoch {
        return Err(ApplyError::EpochMismatch {
            tree_base: tree.base_epoch.clone(),
            patch_epoch: patch.epoch,
        });
    }

    let workspace = patch.workspace_id.clone();
    for change in patch.changes {
        apply_one(&mut tree, change, workspace.as_str())?;
    }

    Ok(tree)
}

fn apply_one(
    tree: &mut ConflictTree,
    change: FileChange,
    workspace: &str,
) -> Result<(), ApplyError> {
    if tree.conflicts.contains_key(&change.path) {
        return apply_one_to_conflict(tree, change, workspace);
    }

    match change.kind {
        ChangeKind::Added => apply_added(tree, change),
        ChangeKind::Modified => apply_modified(tree, change),
        ChangeKind::Deleted => {
            apply_deleted(tree, &change);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Clean-path path (Phase 1)
// ---------------------------------------------------------------------------

fn apply_added(tree: &mut ConflictTree, change: FileChange) -> Result<(), ApplyError> {
    let blob = change.blob.clone().ok_or_else(|| ApplyError::MissingBlob {
        path: change.path.clone(),
        kind: change.kind.clone(),
    })?;

    if tree.clean.contains_key(&change.path) {
        // TODO(follow-up bone): add-on-top-of-clean is a structured conflict
        // candidate (AddAdd or Modify-vs-Add). Phase 2 still only handles
        // the unilateral case, so we log and overwrite.
        warn!(
            path = %change.path.display(),
            "apply_unilateral_patchset: Added on a path already in clean; overwriting"
        );
    }

    let mode = infer_mode_for_new_file(&change);
    tree.clean
        .insert(change.path, MaterializedEntry::new(mode, blob));
    Ok(())
}

fn apply_modified(tree: &mut ConflictTree, change: FileChange) -> Result<(), ApplyError> {
    let blob = change.blob.clone().ok_or_else(|| ApplyError::MissingBlob {
        path: change.path.clone(),
        kind: change.kind.clone(),
    })?;

    if let Some(entry) = tree.clean.get_mut(&change.path) {
        entry.oid = blob;
        // If the patch carries an explicit mode, honor it (covers chmod-only
        // commits and symlink/blob transitions). Otherwise preserve the
        // existing materialized mode (bn-nsz0).
        if let Some(mode) = change.mode {
            entry.mode = mode;
        }
    } else {
        // Upsert semantics: treat `Modified` on an absent path as an insert.
        // This path is reached legitimately when `diff_patchset` emits a
        // rename as `Deleted(from) + Modified(to)` and the seeded tree did
        // not contain `to` yet (bn-3525). Falling back to the `Added`
        // handler preserves mode/blob fidelity.
        let mode = infer_mode_for_new_file(&change);
        tree.clean
            .insert(change.path, MaterializedEntry::new(mode, blob));
    }
    Ok(())
}

fn apply_deleted(tree: &mut ConflictTree, change: &FileChange) {
    if tree.clean.remove(&change.path).is_none() {
        warn!(
            path = %change.path.display(),
            "apply_unilateral_patchset: Deleted on a path not present in tree; ignoring"
        );
    }
}

// ---------------------------------------------------------------------------
// Conflict-path path (Phase 2)
// ---------------------------------------------------------------------------

/// Apply a single [`FileChange`] to an already-conflicted path.
///
/// # Variant × change-kind table
///
/// | Existing conflict                | `Added`                    | `Modified`                 | `Deleted`          |
/// |----------------------------------|----------------------------|----------------------------|--------------------|
/// | [`Conflict::Content`]            | same as `Modified`         | replace every side's blob  | collapse to absent |
/// | [`Conflict::AddAdd`]             | replace every side's blob  | error (no base)            | collapse to absent |
/// | [`Conflict::ModifyDelete`]       | same as `Modified`         | update modifier, keep del  | collapse to absent |
/// | [`Conflict::DivergentRename`]    | `Unhandled`                | `Unhandled`                | `Unhandled`        |
///
/// `Added` on a conflicted path (bn-3l5p): the rebase pipeline pre-populates
/// `tree.conflicts` when the epoch and the workspace both introduce/modify
/// the same path. The subsequent unilateral `Added` from the workspace
/// patchset is therefore the workspace's final content for the path, just
/// like `Modified` would be. We dispatch to the same replace-each-side
/// handler and let convergence collapse the conflict if the workspace's
/// content happens to equal every side.
///
/// After mutation we check for **convergence**: if every side now carries the
/// same blob OID, we collapse the conflict back into `tree.clean`. For
/// `AddAdd` and `ModifyDelete + Deleted`, the conflict is simply removed
/// (no clean entry) because every side ends up as a deletion.
fn apply_one_to_conflict(
    tree: &mut ConflictTree,
    change: FileChange,
    workspace: &str,
) -> Result<(), ApplyError> {
    // Take the conflict out so we can move across its variants without
    // fighting the borrow checker. We reinsert (possibly in a different
    // shape) below unless we decided to collapse.
    let Some(existing) = tree.conflicts.remove(&change.path) else {
        // Caller already checked `contains_key`, so this is unreachable in
        // normal flow. Fall through to the clean path defensively.
        return apply_one_clean_defensive(tree, change);
    };

    match existing {
        Conflict::Content {
            path,
            file_id,
            base,
            sides,
            atoms,
        } => handle_content(tree, path, file_id, base, sides, atoms, change, workspace),
        Conflict::AddAdd { path, sides } => handle_add_add(tree, path, sides, change),
        Conflict::ModifyDelete {
            path,
            file_id,
            modifier,
            deleter,
            modified_content,
        } => handle_modify_delete(
            tree,
            path,
            file_id,
            modifier,
            deleter,
            modified_content,
            change,
            workspace,
        ),
        divergent @ Conflict::DivergentRename { .. } => {
            // Put it back unchanged so the tree remains consistent.
            let path = divergent.path().clone();
            tree.conflicts.insert(path.clone(), divergent);
            Err(ApplyError::UnhandledConflictShape {
                path,
                shape: "divergent_rename",
            })
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_content(
    tree: &mut ConflictTree,
    path: std::path::PathBuf,
    file_id: crate::model::patch::FileId,
    base: Option<GitOid>,
    mut sides: Vec<ConflictSide>,
    atoms: Vec<crate::model::conflict::ConflictAtom>,
    change: FileChange,
    _workspace: &str,
) -> Result<(), ApplyError> {
    match change.kind {
        // V1 SEMANTICS (bn-3l5p): `Added` on an already-tracked conflicted
        // path is treated as `Modified`. The rebase pipeline's overlap
        // detector installs the conflict record before the workspace's
        // unilateral patch is folded in, so a workspace `Added` on such a
        // path is really the workspace's final content for the path.
        ChangeKind::Added | ChangeKind::Modified => {
            let blob = change.blob.clone().ok_or_else(|| ApplyError::MissingBlob {
                path: change.path.clone(),
                kind: change.kind.clone(),
            })?;

            // V1 SIMPLIFICATION: the unilateral patch is the workspace's
            // final word on this file — replace every side's content with
            // the new blob. A future-phase bone will instead 3-way-merge
            // the unilateral change into each side's version.
            for side in &mut sides {
                side.content = blob.clone();
            }

            // Convergence: all sides now carry the same blob. Collapse to
            // clean.
            if sides_converged(&sides) {
                // TODO(follow-up bone): mode tracking across conflict sides.
                // We currently default to `Blob` on collapse; executable and
                // symlink bits conflicted through a content conflict will be
                // lost until per-side mode is plumbed through.
                tree.clean
                    .insert(path, MaterializedEntry::new(EntryMode::Blob, blob));
            } else {
                tree.conflicts.insert(
                    path.clone(),
                    Conflict::Content {
                        path,
                        file_id,
                        base,
                        sides,
                        atoms,
                    },
                );
            }
            Ok(())
        }
        ChangeKind::Deleted => {
            // V1 SEMANTICS: a unilateral deletion collapses the conflict.
            // Rationale: the workspace's final-word stance says "this file
            // is gone"; every side of the existing conflict is superseded.
            // Alternative semantics (morph to ModifyDelete keeping each
            // side's content as a would-be modifier) is possible but breaks
            // the "unilateral is authoritative" invariant V1 relies on.
            //
            // TODO(follow-up bone): revisit once per-side 3-way merge lands
            // — at that point a unilateral delete could legitimately
            // become a ModifyDelete against each surviving side.
            let _ = (file_id, base, sides, atoms); // intentionally dropped
            // Do not re-insert into `conflicts`; do not insert into `clean`.
            let _ = path;
            Ok(())
        }
    }
}

fn handle_add_add(
    tree: &mut ConflictTree,
    path: std::path::PathBuf,
    mut sides: Vec<ConflictSide>,
    change: FileChange,
) -> Result<(), ApplyError> {
    match change.kind {
        ChangeKind::Added => {
            // V1 SEMANTICS (bn-3l5p): `Added` on an already-tracked AddAdd
            // is treated as `Modified` — the pipeline pre-populated the
            // AddAdd conflict (both sides created the path independently),
            // so the workspace's unilateral `Added` is its final content.
            // Replace every side's blob and let convergence collapse if
            // the workspace matches every side.
            let blob = change.blob.clone().ok_or_else(|| ApplyError::MissingBlob {
                path: change.path.clone(),
                kind: change.kind.clone(),
            })?;

            for side in &mut sides {
                side.content = blob.clone();
            }

            if sides_converged(&sides) {
                tree.clean
                    .insert(path, MaterializedEntry::new(EntryMode::Blob, blob));
            } else {
                tree.conflicts
                    .insert(path.clone(), Conflict::AddAdd { path, sides });
            }
            Ok(())
        }
        ChangeKind::Modified => {
            // No base exists for an AddAdd. A Modified operation implies a
            // base, so the collect layer has produced inconsistent data.
            tree.conflicts.insert(
                path.clone(),
                Conflict::AddAdd {
                    path: path.clone(),
                    sides,
                },
            );
            Err(ApplyError::UnexpectedModifyOnAddAdd { path })
        }
        ChangeKind::Deleted => {
            // V1 SEMANTICS: both sides effectively agreed to not have this
            // file — collapse to clean absence.
            let _ = (path, sides);
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_modify_delete(
    tree: &mut ConflictTree,
    path: std::path::PathBuf,
    file_id: crate::model::patch::FileId,
    mut modifier: ConflictSide,
    deleter: ConflictSide,
    _modified_content: GitOid,
    change: FileChange,
    _workspace: &str,
) -> Result<(), ApplyError> {
    match change.kind {
        // V1 SEMANTICS (bn-3l5p): `Added` on an already-tracked
        // ModifyDelete is treated as `Modified` — replace the modifier
        // side's content with the unilateral blob. The deleter side stays
        // as-is, and the conflict remains a ModifyDelete.
        ChangeKind::Added | ChangeKind::Modified => {
            let blob = change.blob.clone().ok_or_else(|| ApplyError::MissingBlob {
                path: change.path.clone(),
                kind: change.kind.clone(),
            })?;

            // V1 SIMPLIFICATION: replace the modify side's content with the
            // unilateral blob. The deleter side is untouched — still a
            // ModifyDelete conflict.
            modifier.content = blob.clone();
            tree.conflicts.insert(
                path.clone(),
                Conflict::ModifyDelete {
                    path,
                    file_id,
                    modifier,
                    deleter,
                    modified_content: blob,
                },
            );
            Ok(())
        }
        ChangeKind::Deleted => {
            // V1 SEMANTICS: both sides now want the file gone — collapse
            // to clean absence.
            let _ = (path, file_id, modifier, deleter, _modified_content);
            Ok(())
        }
    }
}

/// Defensive fallback — only hit if `tree.conflicts` lost the entry between
/// the `contains_key` check and our `remove` (impossible under current
/// single-threaded `&mut ConflictTree`, but kept for robustness).
fn apply_one_clean_defensive(
    tree: &mut ConflictTree,
    change: FileChange,
) -> Result<(), ApplyError> {
    match change.kind {
        ChangeKind::Added => apply_added(tree, change),
        ChangeKind::Modified => apply_modified(tree, change),
        ChangeKind::Deleted => {
            apply_deleted(tree, &change);
            Ok(())
        }
    }
}

/// Returns `true` iff every side carries the same blob OID.
///
/// Used by [`handle_content`] to detect convergence after a unilateral
/// `Modified` propagated into every side.
fn sides_converged(sides: &[ConflictSide]) -> bool {
    sides
        .first()
        .is_some_and(|first| sides.iter().all(|s| s.content == first.content))
}

/// Phase 1 placeholder for mode inference on a new file.
///
/// Uses `FileChange.mode` when present (populated by `diff_patchset` from the
/// source tree's entry mode). Falls back to `EntryMode::Blob` for patches
/// built without an explicit mode (legacy/test fixtures). This preserves
/// executable-bit and symlink modes through replay of added files (bn-nsz0).
fn infer_mode_for_new_file(change: &FileChange) -> EntryMode {
    change.mode.unwrap_or(EntryMode::Blob)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::merge::types::{ChangeKind, FileChange, PatchSet};
    use crate::model::conflict::{Conflict, ConflictSide};
    use crate::model::ordering::OrderingKey;
    use crate::model::patch::FileId;
    use crate::model::types::{EpochId, GitOid, WorkspaceId};

    fn epoch() -> EpochId {
        EpochId::new(&"e".repeat(40)).unwrap()
    }

    fn other_epoch() -> EpochId {
        EpochId::new(&"f".repeat(40)).unwrap()
    }

    fn ws() -> WorkspaceId {
        WorkspaceId::new("ws-1").unwrap()
    }

    fn oid(c: char) -> GitOid {
        GitOid::new(&c.to_string().repeat(40)).unwrap()
    }

    fn ord() -> OrderingKey {
        OrderingKey::new(epoch(), ws(), 1, 1_700_000_000_000)
    }

    fn side(name: &str, content: GitOid) -> ConflictSide {
        ConflictSide::new(name.to_owned(), content, ord())
    }

    fn modify_change(path: &str, blob: GitOid) -> FileChange {
        FileChange::with_identity(
            PathBuf::from(path),
            ChangeKind::Modified,
            Some(b"content".to_vec()),
            None,
            Some(blob),
        )
    }

    fn add_change(path: &str, blob: GitOid) -> FileChange {
        FileChange::with_identity(
            PathBuf::from(path),
            ChangeKind::Added,
            Some(b"content".to_vec()),
            None,
            Some(blob),
        )
    }

    fn delete_change(path: &str) -> FileChange {
        FileChange::new(PathBuf::from(path), ChangeKind::Deleted, None)
    }

    fn patch(changes: Vec<FileChange>) -> PatchSet {
        PatchSet::new(ws(), epoch(), changes)
    }

    fn content_conflict(path: &str, base: GitOid, a: GitOid, b: GitOid) -> Conflict {
        Conflict::Content {
            path: PathBuf::from(path),
            file_id: FileId::new(1),
            base: Some(base),
            sides: vec![side("alice", a), side("bob", b)],
            atoms: vec![],
        }
    }

    fn add_add_conflict(path: &str, a: GitOid, b: GitOid) -> Conflict {
        Conflict::AddAdd {
            path: PathBuf::from(path),
            sides: vec![side("alice", a), side("bob", b)],
        }
    }

    fn modify_delete_conflict(path: &str, mod_content: GitOid, del_content: GitOid) -> Conflict {
        Conflict::ModifyDelete {
            path: PathBuf::from(path),
            file_id: FileId::new(2),
            modifier: side("alice", mod_content.clone()),
            deleter: side("bob", del_content),
            modified_content: mod_content,
        }
    }

    // -----------------------------------------------------------------------
    // Clean-path tests (Phase 1, preserved)
    // -----------------------------------------------------------------------

    #[test]
    fn clean_apply_replaces_blob_and_preserves_mode() {
        let mut tree = ConflictTree::new(epoch());
        tree.clean.insert(
            PathBuf::from("src/lib.rs"),
            MaterializedEntry::new(EntryMode::Blob, oid('a')),
        );

        let result =
            apply_unilateral_patchset(tree, patch(vec![modify_change("src/lib.rs", oid('b'))]))
                .unwrap();

        let entry = &result.clean[&PathBuf::from("src/lib.rs")];
        assert_eq!(entry.mode, EntryMode::Blob);
        assert_eq!(entry.oid, oid('b'));
    }

    #[test]
    fn clean_apply_adds_new_path() {
        let tree = ConflictTree::new(epoch());

        let result =
            apply_unilateral_patchset(tree, patch(vec![add_change("src/new.rs", oid('c'))]))
                .unwrap();

        let entry = &result.clean[&PathBuf::from("src/new.rs")];
        assert_eq!(entry.mode, EntryMode::Blob);
        assert_eq!(entry.oid, oid('c'));
    }

    #[test]
    fn clean_apply_deletes_path() {
        let mut tree = ConflictTree::new(epoch());
        tree.clean.insert(
            PathBuf::from("src/gone.rs"),
            MaterializedEntry::new(EntryMode::Blob, oid('a')),
        );

        let result =
            apply_unilateral_patchset(tree, patch(vec![delete_change("src/gone.rs")])).unwrap();

        assert!(!result.clean.contains_key(&PathBuf::from("src/gone.rs")));
    }

    #[test]
    fn clean_apply_preserves_executable_bit() {
        let mut tree = ConflictTree::new(epoch());
        tree.clean.insert(
            PathBuf::from("scripts/build.sh"),
            MaterializedEntry::new(EntryMode::BlobExecutable, oid('a')),
        );

        let result = apply_unilateral_patchset(
            tree,
            patch(vec![modify_change("scripts/build.sh", oid('b'))]),
        )
        .unwrap();

        let entry = &result.clean[&PathBuf::from("scripts/build.sh")];
        assert_eq!(entry.mode, EntryMode::BlobExecutable);
        assert_eq!(entry.oid, oid('b'));
    }

    #[test]
    fn clean_apply_preserves_symlink_mode() {
        let mut tree = ConflictTree::new(epoch());
        tree.clean.insert(
            PathBuf::from("link"),
            MaterializedEntry::new(EntryMode::Link, oid('a')),
        );

        let result =
            apply_unilateral_patchset(tree, patch(vec![modify_change("link", oid('b'))])).unwrap();

        let entry = &result.clean[&PathBuf::from("link")];
        assert_eq!(entry.mode, EntryMode::Link);
        assert_eq!(entry.oid, oid('b'));
    }

    #[test]
    fn epoch_mismatch_is_rejected() {
        let tree = ConflictTree::new(epoch());
        let mismatched = PatchSet::new(
            ws(),
            other_epoch(),
            vec![add_change("src/foo.rs", oid('b'))],
        );
        let err = apply_unilateral_patchset(tree, mismatched).unwrap_err();
        match err {
            ApplyError::EpochMismatch {
                tree_base,
                patch_epoch,
            } => {
                assert_eq!(tree_base, epoch());
                assert_eq!(patch_epoch, other_epoch());
            }
            other => panic!("expected EpochMismatch, got {other:?}"),
        }
    }

    #[test]
    fn modified_on_absent_path_is_upserted() {
        // bn-3525: `Modified` on an absent path now inserts the entry (upsert
        // semantics) so that the `Deleted(from) + Modified(to)` pair emitted
        // for a rename lands cleanly — the rebase seeds the tree from the
        // new epoch, which may not yet contain the rename's destination.
        let tree = ConflictTree::new(epoch());
        let result =
            apply_unilateral_patchset(tree, patch(vec![modify_change("src/ghost.rs", oid('b'))]))
                .unwrap();
        let entry = &result.clean[&PathBuf::from("src/ghost.rs")];
        assert_eq!(entry.oid, oid('b'));
        assert!(result.conflicts.is_empty());
    }

    #[test]
    fn deleted_on_absent_path_is_ignored() {
        let tree = ConflictTree::new(epoch());
        let result =
            apply_unilateral_patchset(tree, patch(vec![delete_change("src/ghost.rs")])).unwrap();
        assert!(result.clean.is_empty());
        assert!(result.conflicts.is_empty());
    }

    #[test]
    fn rename_delta_removes_from_and_upserts_to() {
        // bn-3525: a rename delta (a.txt → b.txt) is modeled as
        // `Deleted(a.txt) + Modified(b.txt)`. Applying it to a tree that
        // contains `a.txt` must remove `a.txt` and insert `b.txt` — without
        // this, the rename would be silently dropped when the seeded tree
        // does not yet contain the rename destination.
        let mut tree = ConflictTree::new(epoch());
        tree.clean.insert(
            PathBuf::from("a.txt"),
            MaterializedEntry::new(EntryMode::Blob, oid('a')),
        );

        let delete = FileChange::with_identity(
            PathBuf::from("a.txt"),
            ChangeKind::Deleted,
            None,
            None,
            None,
        );
        let modify = FileChange::with_identity(
            PathBuf::from("b.txt"),
            ChangeKind::Modified,
            Some(b"renamed content".to_vec()),
            None,
            Some(oid('a')),
        );

        let result = apply_unilateral_patchset(tree, patch(vec![delete, modify])).unwrap();

        assert!(
            !result.clean.contains_key(&PathBuf::from("a.txt")),
            "rename-from path must be removed"
        );
        let b_entry = &result.clean[&PathBuf::from("b.txt")];
        assert_eq!(b_entry.oid, oid('a'));
    }

    // -----------------------------------------------------------------------
    // Phase 2: Conflict::Content
    // -----------------------------------------------------------------------

    #[test]
    fn content_conflict_modified_replaces_all_sides() {
        // V1 SEMANTICS: a unilateral Modified replaces every side's content
        // with the new blob. Since both sides now carry the same blob, the
        // conflict converges and collapses to clean (see the
        // `convergence_collapses_conflict_to_clean` test for the explicit
        // collapse assertion).
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/battle.rs"),
            content_conflict("src/battle.rs", oid('0'), oid('a'), oid('b')),
        );

        let result =
            apply_unilateral_patchset(tree, patch(vec![modify_change("src/battle.rs", oid('c'))]))
                .unwrap();

        // Convergence collapses the conflict to clean with the new blob.
        assert!(
            !result
                .conflicts
                .contains_key(&PathBuf::from("src/battle.rs")),
            "Modified over a 2-way content conflict should converge and collapse"
        );
        let entry = &result.clean[&PathBuf::from("src/battle.rs")];
        assert_eq!(entry.oid, oid('c'));
        assert_eq!(entry.mode, EntryMode::Blob);
    }

    #[test]
    fn content_conflict_modified_three_way_all_replaced() {
        // Three sides, unilateral Modified — still converges (all replaced
        // with the same blob).
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/three.rs"),
            Conflict::Content {
                path: PathBuf::from("src/three.rs"),
                file_id: FileId::new(7),
                base: Some(oid('0')),
                sides: vec![
                    side("alice", oid('a')),
                    side("bob", oid('b')),
                    side("carol", oid('c')),
                ],
                atoms: vec![],
            },
        );

        let result =
            apply_unilateral_patchset(tree, patch(vec![modify_change("src/three.rs", oid('d'))]))
                .unwrap();

        // All three sides replaced with 'd' => convergence => clean.
        assert!(
            !result
                .conflicts
                .contains_key(&PathBuf::from("src/three.rs")),
            "convergence should collapse a 3-way conflict too"
        );
        assert_eq!(result.clean[&PathBuf::from("src/three.rs")].oid, oid('d'));
    }

    #[test]
    fn content_conflict_deleted_collapses_to_absent() {
        // V1 SEMANTICS: unilateral Deleted over an existing Content conflict
        // collapses to clean absence. The unilateral delete is the
        // workspace's "final word" — every side is superseded.
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/gone.rs"),
            content_conflict("src/gone.rs", oid('0'), oid('a'), oid('b')),
        );

        let result =
            apply_unilateral_patchset(tree, patch(vec![delete_change("src/gone.rs")])).unwrap();

        assert!(!result.conflicts.contains_key(&PathBuf::from("src/gone.rs")));
        assert!(!result.clean.contains_key(&PathBuf::from("src/gone.rs")));
    }

    #[test]
    fn add_on_content_conflict_behaves_as_modified() {
        // bn-3l5p: `Added` on a conflicted path is now treated as `Modified`.
        // The pipeline pre-populates a Conflict::Content when epoch and
        // workspace both touch the same path; the workspace's subsequent
        // `Added` is the workspace's final content for that path. Folding
        // it replaces every side's blob, and since both sides match the
        // new blob, the conflict converges to clean.
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/battle.rs"),
            content_conflict("src/battle.rs", oid('0'), oid('a'), oid('b')),
        );

        let result =
            apply_unilateral_patchset(tree, patch(vec![add_change("src/battle.rs", oid('c'))]))
                .expect("Added on content conflict must succeed (treated as Modified)");

        // Convergence: both sides now hold 'c', so the conflict collapses
        // to clean.
        assert!(
            !result
                .conflicts
                .contains_key(&PathBuf::from("src/battle.rs")),
            "converged sides should collapse to clean"
        );
        let entry = &result.clean[&PathBuf::from("src/battle.rs")];
        assert_eq!(entry.oid, oid('c'));
        assert_eq!(entry.mode, EntryMode::Blob);
    }

    // -----------------------------------------------------------------------
    // Phase 2: Conflict::AddAdd
    // -----------------------------------------------------------------------

    #[test]
    fn add_add_conflict_deleted_collapses() {
        // V1 SEMANTICS: both sides effectively agreed there's no file.
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/new.rs"),
            add_add_conflict("src/new.rs", oid('a'), oid('b')),
        );

        let result =
            apply_unilateral_patchset(tree, patch(vec![delete_change("src/new.rs")])).unwrap();

        assert!(!result.conflicts.contains_key(&PathBuf::from("src/new.rs")));
        assert!(!result.clean.contains_key(&PathBuf::from("src/new.rs")));
    }

    #[test]
    fn add_on_addadd_conflict_replaces_workspace_side() {
        // bn-3l5p: `Added` on a tracked AddAdd conflict is the workspace's
        // final content for the path. Replace every side's blob; under V1
        // "unilateral is authoritative" semantics, both sides converge and
        // the conflict collapses to clean.
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/new.rs"),
            add_add_conflict("src/new.rs", oid('a'), oid('b')),
        );

        let result =
            apply_unilateral_patchset(tree, patch(vec![add_change("src/new.rs", oid('c'))]))
                .expect("Added on AddAdd conflict must succeed (treated as Modified)");

        assert!(
            !result.conflicts.contains_key(&PathBuf::from("src/new.rs")),
            "AddAdd with converged sides should collapse to clean"
        );
        let entry = &result.clean[&PathBuf::from("src/new.rs")];
        assert_eq!(entry.oid, oid('c'));
    }

    #[test]
    fn add_add_conflict_modified_is_error() {
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/new.rs"),
            add_add_conflict("src/new.rs", oid('a'), oid('b')),
        );

        let err =
            apply_unilateral_patchset(tree, patch(vec![modify_change("src/new.rs", oid('c'))]))
                .unwrap_err();

        match err {
            ApplyError::UnexpectedModifyOnAddAdd { path } => {
                assert_eq!(path, PathBuf::from("src/new.rs"));
            }
            other => panic!("expected UnexpectedModifyOnAddAdd, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Phase 2: Conflict::ModifyDelete
    // -----------------------------------------------------------------------

    #[test]
    fn modify_delete_conflict_modified_updates_modify_side() {
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/ambivalent.rs"),
            modify_delete_conflict("src/ambivalent.rs", oid('a'), oid('b')),
        );

        let result = apply_unilateral_patchset(
            tree,
            patch(vec![modify_change("src/ambivalent.rs", oid('c'))]),
        )
        .unwrap();

        // Still a ModifyDelete; modifier's content is now 'c', deleter unchanged.
        let conflict = &result.conflicts[&PathBuf::from("src/ambivalent.rs")];
        match conflict {
            Conflict::ModifyDelete {
                modifier,
                deleter,
                modified_content,
                ..
            } => {
                assert_eq!(modifier.content, oid('c'));
                assert_eq!(deleter.content, oid('b'));
                assert_eq!(*modified_content, oid('c'));
            }
            other => panic!("expected ModifyDelete, got {other:?}"),
        }
        assert!(
            !result
                .clean
                .contains_key(&PathBuf::from("src/ambivalent.rs"))
        );
    }

    #[test]
    fn modify_delete_conflict_deleted_collapses() {
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/ambivalent.rs"),
            modify_delete_conflict("src/ambivalent.rs", oid('a'), oid('b')),
        );

        let result =
            apply_unilateral_patchset(tree, patch(vec![delete_change("src/ambivalent.rs")]))
                .unwrap();

        assert!(
            !result
                .conflicts
                .contains_key(&PathBuf::from("src/ambivalent.rs"))
        );
        assert!(
            !result
                .clean
                .contains_key(&PathBuf::from("src/ambivalent.rs"))
        );
    }

    #[test]
    fn add_on_modify_delete_conflict_updates_modifier() {
        // bn-3l5p: `Added` on a tracked ModifyDelete is the workspace's
        // final content for the path. Update the modifier side's blob and
        // keep the deleter side untouched — still a ModifyDelete.
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/ambivalent.rs"),
            modify_delete_conflict("src/ambivalent.rs", oid('a'), oid('b')),
        );

        let result =
            apply_unilateral_patchset(tree, patch(vec![add_change("src/ambivalent.rs", oid('c'))]))
                .expect("Added on ModifyDelete must succeed (treated as Modified)");

        let conflict = &result.conflicts[&PathBuf::from("src/ambivalent.rs")];
        match conflict {
            Conflict::ModifyDelete {
                modifier,
                deleter,
                modified_content,
                ..
            } => {
                assert_eq!(modifier.content, oid('c'));
                assert_eq!(deleter.content, oid('b'));
                assert_eq!(*modified_content, oid('c'));
            }
            other => panic!("expected ModifyDelete, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Phase 2: Convergence
    // -----------------------------------------------------------------------

    #[test]
    fn convergence_collapses_conflict_to_clean() {
        // A content conflict where both sides converge to the unilateral blob
        // should be promoted back into `clean`.
        let mut tree = ConflictTree::new(epoch());
        tree.conflicts.insert(
            PathBuf::from("src/converge.rs"),
            content_conflict("src/converge.rs", oid('0'), oid('a'), oid('b')),
        );

        let result = apply_unilateral_patchset(
            tree,
            patch(vec![modify_change("src/converge.rs", oid('d'))]),
        )
        .unwrap();

        assert!(
            !result
                .conflicts
                .contains_key(&PathBuf::from("src/converge.rs")),
            "convergence should remove the path from conflicts"
        );
        let entry = &result.clean[&PathBuf::from("src/converge.rs")];
        assert_eq!(entry.oid, oid('d'));
        // TODO(follow-up bone): mode should be the converged per-side mode
        // once we track it; for now we default to Blob.
        assert_eq!(entry.mode, EntryMode::Blob);
    }

    #[test]
    fn sides_converged_helper_identifies_all_equal() {
        let all_same = vec![side("a", oid('d')), side("b", oid('d'))];
        assert!(sides_converged(&all_same));

        let divergent = vec![side("a", oid('d')), side("b", oid('e'))];
        assert!(!sides_converged(&divergent));

        let empty: Vec<ConflictSide> = vec![];
        assert!(!sides_converged(&empty));
    }

    // -----------------------------------------------------------------------
    // Phase 2: Unhandled conflict shapes
    // -----------------------------------------------------------------------

    #[test]
    fn unhandled_conflict_shape_returns_error() {
        // DivergentRename is explicitly deferred — applying any unilateral
        // change to it should surface an UnhandledConflictShape error so the
        // pipeline cannot silently flatten.
        let mut tree = ConflictTree::new(epoch());
        let dr = Conflict::DivergentRename {
            file_id: FileId::new(99),
            original: PathBuf::from("src/util.rs"),
            destinations: vec![
                (PathBuf::from("src/helpers.rs"), side("alice", oid('a'))),
                (PathBuf::from("src/common.rs"), side("bob", oid('b'))),
            ],
        };
        // We key this by "src/util.rs" (the original) because that's what
        // `path()` returns and that's what the patch-collect layer would
        // emit targeting the same `FileId`. Here we just want to confirm
        // the error path.
        tree.conflicts.insert(PathBuf::from("src/util.rs"), dr);

        let err =
            apply_unilateral_patchset(tree, patch(vec![modify_change("src/util.rs", oid('d'))]))
                .unwrap_err();

        match err {
            ApplyError::UnhandledConflictShape { path, shape } => {
                assert_eq!(path, PathBuf::from("src/util.rs"));
                assert_eq!(shape, "divergent_rename");
            }
            other => panic!("expected UnhandledConflictShape, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Phase 2: Idempotence property test
    // -----------------------------------------------------------------------

    #[test]
    fn disjoint_patches_are_idempotent_on_conflicts_map() {
        // Property: for any PatchSet whose paths are unique AND disjoint
        // from tree.conflicts, applying it once and then applying it again
        // leaves the conflicts map identical to a single application.
        //
        // We iterate a handful of hand-constructed cases rather than pull in
        // a property-testing crate.

        // Build a conflict-rich baseline tree that all cases reuse.
        let make_baseline = || {
            let mut tree = ConflictTree::new(epoch());
            tree.conflicts.insert(
                PathBuf::from("conflict/a.rs"),
                content_conflict("conflict/a.rs", oid('0'), oid('a'), oid('b')),
            );
            tree.conflicts.insert(
                PathBuf::from("conflict/b.rs"),
                add_add_conflict("conflict/b.rs", oid('a'), oid('b')),
            );
            tree.conflicts.insert(
                PathBuf::from("conflict/c.rs"),
                modify_delete_conflict("conflict/c.rs", oid('a'), oid('b')),
            );
            tree.clean.insert(
                PathBuf::from("clean/existing.rs"),
                MaterializedEntry::new(EntryMode::Blob, oid('e')),
            );
            tree
        };

        let cases: Vec<Vec<FileChange>> = vec![
            vec![add_change("clean/new-1.rs", oid('1'))],
            vec![modify_change("clean/existing.rs", oid('2'))],
            vec![delete_change("clean/existing.rs")],
            vec![
                add_change("clean/fresh-1.rs", oid('3')),
                add_change("clean/fresh-2.rs", oid('4')),
            ],
            vec![
                modify_change("clean/existing.rs", oid('5')),
                add_change("clean/fresh-3.rs", oid('6')),
            ],
            vec![
                delete_change("clean/existing.rs"),
                add_change("clean/fresh-4.rs", oid('7')),
            ],
            // Empty patch — trivial fixpoint.
            vec![],
            // Modify an absent path (ignored with warning — still must be
            // idempotent on conflicts).
            vec![modify_change("clean/ghost.rs", oid('8'))],
            // Delete an absent path.
            vec![delete_change("clean/ghost2.rs")],
            // Multiple unique disjoint adds.
            vec![
                add_change("clean/a.rs", oid('9')),
                add_change("clean/b.rs", oid('a')),
                add_change("clean/c.rs", oid('b')),
            ],
            // Add followed by modify of the same newly-added path. (Added
            // wins, then Modified replaces it — both paths are disjoint
            // from conflicts.) Idempotent because applying the same
            // sequence twice leaves the same final blob.
            vec![
                add_change("clean/seq.rs", oid('c')),
                modify_change("clean/seq.rs", oid('d')),
            ],
            // Modify followed by delete of the same path.
            vec![
                modify_change("clean/existing.rs", oid('e')),
                delete_change("clean/existing.rs"),
            ],
        ];

        for (idx, changes) in cases.into_iter().enumerate() {
            let tree = make_baseline();

            // Apply once.
            let once = apply_unilateral_patchset(tree, patch(changes.clone()))
                .expect("first apply should succeed");
            let after_first_conflicts = once.conflicts.clone();

            // Apply the same patch again (same epoch, same workspace).
            let twice = apply_unilateral_patchset(once, patch(changes.clone()))
                .expect("second apply should succeed");
            let after_second_conflicts = twice.conflicts;

            assert_eq!(
                after_first_conflicts, after_second_conflicts,
                "case {idx}: conflicts map should be idempotent under disjoint re-application (changes={changes:?})"
            );
        }
    }

    #[test]
    fn disjoint_patches_dont_touch_conflicts_map() {
        // Sanity check of the above property — single apply of a disjoint
        // patch should leave `conflicts` bitwise-equal to the input.
        let mut tree = ConflictTree::new(epoch());
        let c1 = content_conflict("c/a.rs", oid('0'), oid('a'), oid('b'));
        let c2 = add_add_conflict("c/b.rs", oid('a'), oid('b'));
        tree.conflicts.insert(PathBuf::from("c/a.rs"), c1.clone());
        tree.conflicts.insert(PathBuf::from("c/b.rs"), c2.clone());

        let before = tree.conflicts.clone();
        let result = apply_unilateral_patchset(
            tree,
            patch(vec![
                add_change("clean/x.rs", oid('c')),
                add_change("clean/y.rs", oid('d')),
            ]),
        )
        .unwrap();

        assert_eq!(result.conflicts, before);
        assert_eq!(&result.conflicts[&PathBuf::from("c/a.rs")], &c1);
        assert_eq!(&result.conflicts[&PathBuf::from("c/b.rs")], &c2);
    }
}
