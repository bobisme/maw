//! Apply a [`PatchSet`] to a [`ConflictTree`] (Phase 1 — unilateral, clean paths only).
//!
//! This module is the fold primitive used by the structured-merge rebase
//! pipeline: given a running `ConflictTree` and a new `PatchSet`, produce an
//! updated tree.
//!
//! Phase 1 scope is deliberately small. We handle **only** the non-conflicted
//! cases a single workspace's patch can touch when its changes don't overlap
//! anything already conflicted:
//!
//! * path in `tree.clean` + patch has a `Modified` → replace blob, preserve mode;
//! * path in `tree.clean` + patch has a `Deleted` → remove from `clean`;
//! * path absent from both maps + patch has `Added` → insert into `clean`;
//! * path absent from both maps + patch has `Modified`/`Deleted` → log a
//!   warning (unexpected) and skip — we have no base entry to modify/remove;
//! * path already in `tree.conflicts` → pass through unchanged (Phase 2 will
//!   detect new overlapping edits and turn them into structured conflicts).
//!
//! Phase 2 will extend this to detect when a patch's change conflicts with
//! an existing clean entry (both workspaces modified the same path) and
//! promote the path from `clean` to `conflicts`.

use tracing::warn;

use super::types::{
    ChangeKind, ConflictTree, EntryMode, FileChange, MaterializedEntry, PatchSet,
};
use crate::model::types::EpochId;

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
        }
    }
}

impl std::error::Error for ApplyError {}

/// Apply a single workspace's [`PatchSet`] to a [`ConflictTree`].
///
/// See the module docs for the Phase 1 scope. Returns an updated tree on
/// success; returns an error if the patch's epoch does not match the tree's
/// base epoch, or if an add/modify change lacks a precomputed blob OID.
///
/// This function is pure: `tree` is consumed and a new tree is returned.
///
/// # Errors
///
/// - [`ApplyError::EpochMismatch`] — the patch is based on a different epoch.
/// - [`ApplyError::MissingBlob`] — an add/modify change had no `blob` OID.
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

    for change in patch.changes {
        // TODO(Phase 2): detect when `change.path` collides with another
        // workspace's change via an existing clean entry whose `oid` does
        // not match `change.blob`, and promote the path from `clean` to
        // `conflicts` with a proper `Conflict::Content` record. Phase 1
        // treats every incoming change as unilateral.
        apply_one(&mut tree, change)?;
    }

    Ok(tree)
}

fn apply_one(tree: &mut ConflictTree, change: FileChange) -> Result<(), ApplyError> {
    // TODO(Phase 2): when `change.path` is already in `tree.conflicts`, we
    // need to add this side to the existing conflict record. For Phase 1 we
    // pass through unchanged — the path stays conflicted, the change is
    // dropped on the floor (with a debug log for traceability).
    if tree.conflicts.contains_key(&change.path) {
        tracing::debug!(
            path = %change.path.display(),
            kind = %change.kind,
            "apply_unilateral_patchset: path already conflicted, skipping (TODO Phase 2)"
        );
        return Ok(());
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

fn apply_added(tree: &mut ConflictTree, change: FileChange) -> Result<(), ApplyError> {
    let blob = change.blob.clone().ok_or_else(|| ApplyError::MissingBlob {
        path: change.path.clone(),
        kind: change.kind.clone(),
    })?;

    if tree.clean.contains_key(&change.path) {
        // TODO(Phase 2): add-on-top-of-clean is a structured conflict
        // candidate (AddAdd or Modify-vs-Add). For Phase 1 we log and
        // overwrite — test coverage does not exercise this branch.
        warn!(
            path = %change.path.display(),
            "apply_unilateral_patchset: Added on a path already in clean; overwriting (TODO Phase 2)"
        );
    }

    // Newly-added files default to a regular blob. Executable bits and
    // symlinks are introduced by the patch collector attaching an explicit
    // mode in a later phase; for Phase 1 we assume Blob.
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
        // Preserve the existing mode (executable bit, symlink, etc.).
        // Phase 2 will thread an explicit mode through the patch so
        // executable-bit-only changes can be represented.
        entry.oid = blob;
    } else {
        warn!(
            path = %change.path.display(),
            "apply_unilateral_patchset: Modified on a path not present in tree; ignoring"
        );
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

/// Phase 1 placeholder for mode inference on a new file.
///
/// Until the patch collector carries an explicit mode field, we assume every
/// newly-added file is a regular blob. Executable-bit and symlink support is
/// preserved through the `Modified` path because we read the existing mode
/// off the materialized entry.
///
/// TODO(Phase 2): read the mode from the patch itself once `FileChange`
/// grows an explicit `mode` field (or we derive it from the worktree).
const fn infer_mode_for_new_file(_change: &FileChange) -> EntryMode {
    EntryMode::Blob
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
    fn conflict_path_untouched_by_phase_1() {
        // TODO(Phase 2): this test locks in the Phase 1 behavior where a
        // path already marked conflicted is left untouched by further
        // unilateral patches. Phase 2 should instead fold the new side into
        // the existing conflict record and this test will need to change.
        let mut tree = ConflictTree::new(epoch());
        let ord = OrderingKey::new(epoch(), ws(), 1, 1_700_000_000_000);
        let side = ConflictSide::new("ws-prev".into(), oid('a'), ord);
        let conflict = Conflict::AddAdd {
            path: PathBuf::from("src/battle.rs"),
            sides: vec![side.clone(), side],
        };
        tree.conflicts
            .insert(PathBuf::from("src/battle.rs"), conflict.clone());

        let result =
            apply_unilateral_patchset(tree, patch(vec![add_change("src/battle.rs", oid('b'))]))
                .unwrap();

        // Path stays in conflicts, unchanged. Nothing shows up in clean.
        assert!(!result.clean.contains_key(&PathBuf::from("src/battle.rs")));
        assert_eq!(&result.conflicts[&PathBuf::from("src/battle.rs")], &conflict);
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
    fn modified_on_absent_path_is_ignored() {
        // Pre-existing ConflictTree with no entry for the modified path.
        let tree = ConflictTree::new(epoch());
        let result =
            apply_unilateral_patchset(tree, patch(vec![modify_change("src/ghost.rs", oid('b'))]))
                .unwrap();
        // The modification is dropped — no entry added, no conflict.
        assert!(result.clean.is_empty());
        assert!(result.conflicts.is_empty());
    }

    #[test]
    fn deleted_on_absent_path_is_ignored() {
        let tree = ConflictTree::new(epoch());
        let result = apply_unilateral_patchset(tree, patch(vec![delete_change("src/ghost.rs")]))
            .unwrap();
        assert!(result.clean.is_empty());
        assert!(result.conflicts.is_empty());
    }
}
