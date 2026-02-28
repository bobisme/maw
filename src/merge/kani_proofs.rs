//! Kani proof harnesses for merge algebra verification.
//!
//! These harnesses use bounded symbolic inputs (`kani::any()`) to achieve
//! exhaustive verification of the same merge algebra properties that the
//! proptest-based tests in `determinism_tests.rs` and `pushout_tests.rs`
//! verify statistically.
//!
//! # Properties verified
//!
//! 1. **Permutation determinism**: merge result is independent of workspace
//!    ordering (commutativity of the pushout).
//! 2. **Idempotence**: merging identical inputs produces the same result as
//!    a single input (hash-equality resolution).
//! 3. **Conflict monotonicity**: every input path appears in either resolved
//!    or conflicts (no silent drops), and no path appears in both.
//!
//! # Bounds
//!
//! Kani explores all values within bounds exhaustively. We keep workspace
//! counts small (2-3) and file counts small (1-3) so the state space remains
//! tractable for the SAT/SMT solver.
//!
//! # Running
//!
//! ```bash
//! cargo kani --harness <harness_name>
//! # or all harnesses:
//! cargo kani
//! ```
//!
//! These harnesses are gated behind `#[cfg(kani)]` so they are invisible to
//! normal `cargo build` and `cargo test`.

#![allow(clippy::all, clippy::pedantic, clippy::nursery)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::merge::build::ResolvedChange;
use crate::merge::partition::partition_by_path;
use crate::merge::resolve::{resolve_partition, ResolveResult};
use crate::merge::types::{ChangeKind, FileChange, PatchSet};
use crate::model::types::{EpochId, WorkspaceId};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Fixed epoch OID for all proofs (content irrelevant at unit level).
fn epoch() -> EpochId {
    EpochId::new(&"a".repeat(40)).unwrap()
}

fn ws(name: &str) -> WorkspaceId {
    WorkspaceId::new(name).unwrap()
}

/// Build a `ChangeKind` from a bounded symbolic index (0..3).
fn change_kind_from_index(idx: u8) -> ChangeKind {
    match idx % 3 {
        0 => ChangeKind::Added,
        1 => ChangeKind::Modified,
        _ => ChangeKind::Deleted,
    }
}

/// Build a deterministic file path from a bounded index.
fn path_from_index(idx: u8) -> PathBuf {
    match idx {
        0 => PathBuf::from("alpha.rs"),
        1 => PathBuf::from("beta.rs"),
        2 => PathBuf::from("gamma.rs"),
        _ => PathBuf::from("delta.rs"),
    }
}

/// Build deterministic file content from a bounded index.
fn content_from_index(idx: u8) -> Vec<u8> {
    match idx % 4 {
        0 => b"fn a() {}\n".to_vec(),
        1 => b"fn b() {}\n".to_vec(),
        2 => b"fn c() {}\n".to_vec(),
        _ => b"fn d() {}\n".to_vec(),
    }
}

/// Build a `FileChange` from symbolic indices.
fn file_change(path_idx: u8, kind_idx: u8, content_idx: u8) -> FileChange {
    let path = path_from_index(path_idx);
    let kind = change_kind_from_index(kind_idx);
    let content = if matches!(kind, ChangeKind::Deleted) {
        None
    } else {
        Some(content_from_index(content_idx))
    };
    FileChange::new(path, kind, content)
}

/// Build base contents for paths that appear as Modified or Deleted.
fn make_base_for_changes(changes: &[&[FileChange]]) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut base = BTreeMap::new();
    for ws_changes in changes {
        for change in *ws_changes {
            if matches!(change.kind, ChangeKind::Modified | ChangeKind::Deleted) {
                base.entry(change.path.clone())
                    .or_insert_with(|| b"base content\nline 2\nline 3\n".to_vec());
            }
        }
    }
    base
}

/// Run the merge pipeline and return the result.
fn run_merge(patch_sets: &[PatchSet], base_contents: &BTreeMap<PathBuf, Vec<u8>>) -> ResolveResult {
    let partition = partition_by_path(patch_sets);
    resolve_partition(&partition, base_contents).expect("resolve should not error")
}

/// Compare two `ResolveResult`s for structural equality.
fn results_equal(a: &ResolveResult, b: &ResolveResult) -> bool {
    if a.resolved.len() != b.resolved.len() || a.conflicts.len() != b.conflicts.len() {
        return false;
    }

    for (ra, rb) in a.resolved.iter().zip(b.resolved.iter()) {
        match (ra, rb) {
            (
                ResolvedChange::Upsert {
                    path: pa,
                    content: ca,
                },
                ResolvedChange::Upsert {
                    path: pb,
                    content: cb,
                },
            ) => {
                if pa != pb || ca != cb {
                    return false;
                }
            }
            (ResolvedChange::Delete { path: pa }, ResolvedChange::Delete { path: pb }) => {
                if pa != pb {
                    return false;
                }
            }
            _ => return false,
        }
    }

    for (ca, cb) in a.conflicts.iter().zip(b.conflicts.iter()) {
        if ca.path != cb.path
            || format!("{}", ca.reason) != format!("{}", cb.reason)
            || ca.sides.len() != cb.sides.len()
        {
            return false;
        }
    }

    true
}

// =========================================================================
// PROPERTY 1: Permutation Determinism (Commutativity)
//
// merge(ws_a, ws_b) == merge(ws_b, ws_a)
// For all valid workspace inputs, the result is order-independent.
// =========================================================================

/// Two workspaces, each with one change, all permutations verified.
#[kani::proof]
#[kani::unwind(10)]
fn permutation_determinism_2ws_1change() {
    // Symbolic inputs for workspace A's change.
    let path_a: u8 = kani::any();
    kani::assume(path_a < 4);
    let kind_a: u8 = kani::any();
    kani::assume(kind_a < 3);
    let content_a: u8 = kani::any();
    kani::assume(content_a < 4);

    // Symbolic inputs for workspace B's change.
    let path_b: u8 = kani::any();
    kani::assume(path_b < 4);
    let kind_b: u8 = kani::any();
    kani::assume(kind_b < 3);
    let content_b: u8 = kani::any();
    kani::assume(content_b < 4);

    let change_a = file_change(path_a, kind_a, content_a);
    let change_b = file_change(path_b, kind_b, content_b);

    let base = make_base_for_changes(&[&[change_a.clone()], &[change_b.clone()]]);

    // Forward order: [A, B]
    let ps_forward = vec![
        PatchSet::new(ws("ws-00"), epoch(), vec![change_a.clone()]),
        PatchSet::new(ws("ws-01"), epoch(), vec![change_b.clone()]),
    ];

    // Reverse order: [B, A]
    let ps_reverse = vec![
        PatchSet::new(ws("ws-01"), epoch(), vec![change_b]),
        PatchSet::new(ws("ws-00"), epoch(), vec![change_a]),
    ];

    let result_forward = run_merge(&ps_forward, &base);
    let result_reverse = run_merge(&ps_reverse, &base);

    assert!(
        results_equal(&result_forward, &result_reverse),
        "Permutation determinism violated: merge(A,B) != merge(B,A)"
    );
}

/// Three workspaces with one change each, verify all 6 permutations produce
/// identical results.
#[kani::proof]
#[kani::unwind(10)]
fn permutation_determinism_3ws_1change() {
    // Use fixed kinds to keep state space tractable for 3 workspaces.
    let path_a: u8 = kani::any();
    kani::assume(path_a < 3);
    let path_b: u8 = kani::any();
    kani::assume(path_b < 3);
    let path_c: u8 = kani::any();
    kani::assume(path_c < 3);

    let kind: u8 = kani::any();
    kani::assume(kind < 3);

    let change_a = file_change(path_a, kind, 0);
    let change_b = file_change(path_b, kind, 1);
    let change_c = file_change(path_c, kind, 2);

    let base = make_base_for_changes(&[
        &[change_a.clone()],
        &[change_b.clone()],
        &[change_c.clone()],
    ]);

    let changes = [
        (ws("ws-00"), change_a),
        (ws("ws-01"), change_b),
        (ws("ws-02"), change_c),
    ];

    // All 6 permutations of 3 elements.
    let perms: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];

    // Compute reference result with identity permutation.
    let ps_ref: Vec<PatchSet> = perms[0]
        .iter()
        .map(|&i| PatchSet::new(changes[i].0.clone(), epoch(), vec![changes[i].1.clone()]))
        .collect();
    let result_ref = run_merge(&ps_ref, &base);

    // Verify all other permutations match.
    for perm in &perms[1..] {
        let ps: Vec<PatchSet> = perm
            .iter()
            .map(|&i| PatchSet::new(changes[i].0.clone(), epoch(), vec![changes[i].1.clone()]))
            .collect();
        let result = run_merge(&ps, &base);
        assert!(
            results_equal(&result_ref, &result),
            "Permutation determinism violated for 3 workspaces"
        );
    }
}

// =========================================================================
// PROPERTY 2: Idempotence
//
// If all workspaces submit identical changes for a path, the merge result
// should be the same as a single workspace submitting that change.
// No spurious conflicts from identical inputs.
// =========================================================================

/// Two workspaces with identical changes must resolve cleanly to the same
/// content as a single workspace.
#[kani::proof]
#[kani::unwind(10)]
fn idempotence_2ws_identical_changes() {
    let path_idx: u8 = kani::any();
    kani::assume(path_idx < 4);
    let content_idx: u8 = kani::any();
    kani::assume(content_idx < 4);

    // Both workspaces make the same modification to the same file.
    let change = file_change(path_idx, 1 /* Modified */, content_idx);

    let mut base = BTreeMap::new();
    base.insert(
        change.path.clone(),
        b"base content\nline 2\nline 3\n".to_vec(),
    );

    // Two-workspace merge (identical changes).
    let ps_two = vec![
        PatchSet::new(ws("ws-00"), epoch(), vec![change.clone()]),
        PatchSet::new(ws("ws-01"), epoch(), vec![change.clone()]),
    ];
    let result_two = run_merge(&ps_two, &base);

    // Single-workspace merge.
    let ps_one = vec![PatchSet::new(ws("ws-00"), epoch(), vec![change.clone()])];
    let result_one = run_merge(&ps_one, &base);

    // Two-workspace identical merge must resolve cleanly.
    assert!(
        result_two.is_clean(),
        "Identical changes from 2 workspaces should resolve cleanly"
    );

    // Both should produce the same resolved content.
    assert_eq!(
        result_two.resolved.len(),
        result_one.resolved.len(),
        "Identical 2-ws merge should produce same number of resolved changes as 1-ws"
    );

    // Verify content matches.
    for (r_two, r_one) in result_two.resolved.iter().zip(result_one.resolved.iter()) {
        match (r_two, r_one) {
            (
                ResolvedChange::Upsert {
                    path: p2,
                    content: c2,
                },
                ResolvedChange::Upsert {
                    path: p1,
                    content: c1,
                },
            ) => {
                assert_eq!(p2, p1, "Paths should match");
                assert_eq!(c2, c1, "Content should match for identical inputs");
            }
            (ResolvedChange::Delete { path: p2 }, ResolvedChange::Delete { path: p1 }) => {
                assert_eq!(p2, p1, "Delete paths should match");
            }
            _ => {
                assert!(false, "Resolved change types should match");
            }
        }
    }
}

/// Three workspaces with identical Add operations should resolve to a
/// single clean upsert.
#[kani::proof]
#[kani::unwind(10)]
fn idempotence_3ws_identical_adds() {
    let path_idx: u8 = kani::any();
    kani::assume(path_idx < 4);
    let content_idx: u8 = kani::any();
    kani::assume(content_idx < 4);

    let change = file_change(path_idx, 0 /* Added */, content_idx);

    let base = BTreeMap::new(); // No base for adds.

    let ps = vec![
        PatchSet::new(ws("ws-00"), epoch(), vec![change.clone()]),
        PatchSet::new(ws("ws-01"), epoch(), vec![change.clone()]),
        PatchSet::new(ws("ws-02"), epoch(), vec![change.clone()]),
    ];

    let result = run_merge(&ps, &base);

    assert!(
        result.is_clean(),
        "Identical adds from 3 workspaces should resolve cleanly"
    );
    assert_eq!(
        result.resolved.len(),
        1,
        "Should produce exactly 1 resolved change"
    );

    // Verify the resolved content matches the input.
    match &result.resolved[0] {
        ResolvedChange::Upsert { content, .. } => {
            assert_eq!(
                content,
                change.content.as_ref().unwrap(),
                "Resolved content should match input"
            );
        }
        _ => assert!(false, "Expected upsert for identical adds"),
    }
}

/// N workspaces all deleting the same file should resolve to a single
/// clean delete.
#[kani::proof]
#[kani::unwind(10)]
fn idempotence_delete_delete_resolves() {
    let path_idx: u8 = kani::any();
    kani::assume(path_idx < 4);
    let n: u8 = kani::any();
    kani::assume(n >= 2 && n <= 3);

    let path = path_from_index(path_idx);
    let change = FileChange::new(path.clone(), ChangeKind::Deleted, None);

    let mut base = BTreeMap::new();
    base.insert(path.clone(), b"old content\n".to_vec());

    let mut ps = Vec::new();
    for i in 0..n {
        ps.push(PatchSet::new(
            ws(&format!("ws-{i:02}")),
            epoch(),
            vec![change.clone()],
        ));
    }

    let result = run_merge(&ps, &base);

    assert!(
        result.is_clean(),
        "Delete/delete from N workspaces should resolve cleanly"
    );
    assert_eq!(result.resolved.len(), 1);
    match &result.resolved[0] {
        ResolvedChange::Delete { path: p } => {
            assert_eq!(p, &path, "Deleted path should match");
        }
        _ => assert!(false, "Expected delete resolution"),
    }
}

// =========================================================================
// PROPERTY 3: Conflict Monotonicity
//
// a) No silent drops: every input path appears in either resolved or
//    conflicts.
// b) No path duplication: no path appears in both resolved and conflicts.
// c) Modify/delete always produces a conflict (never silently dropped).
// =========================================================================

/// Every path from every workspace must appear in either resolved or
/// conflicts. No path is silently dropped.
#[kani::proof]
#[kani::unwind(10)]
fn conflict_monotonicity_no_silent_drops_2ws() {
    let path_a: u8 = kani::any();
    kani::assume(path_a < 4);
    let kind_a: u8 = kani::any();
    kani::assume(kind_a < 3);
    let content_a: u8 = kani::any();
    kani::assume(content_a < 4);

    let path_b: u8 = kani::any();
    kani::assume(path_b < 4);
    let kind_b: u8 = kani::any();
    kani::assume(kind_b < 3);
    let content_b: u8 = kani::any();
    kani::assume(content_b < 4);

    let change_a = file_change(path_a, kind_a, content_a);
    let change_b = file_change(path_b, kind_b, content_b);

    let base = make_base_for_changes(&[&[change_a.clone()], &[change_b.clone()]]);

    let ps = vec![
        PatchSet::new(ws("ws-00"), epoch(), vec![change_a.clone()]),
        PatchSet::new(ws("ws-01"), epoch(), vec![change_b.clone()]),
    ];

    let result = run_merge(&ps, &base);

    // Collect all input paths.
    let mut input_paths: BTreeSet<PathBuf> = BTreeSet::new();
    input_paths.insert(change_a.path.clone());
    input_paths.insert(change_b.path.clone());

    // Collect all output paths.
    let mut output_paths: BTreeSet<PathBuf> = BTreeSet::new();
    for r in &result.resolved {
        output_paths.insert(r.path().clone());
    }
    for c in &result.conflicts {
        output_paths.insert(c.path.clone());
    }

    // Every input path must appear in output.
    for path in &input_paths {
        assert!(
            output_paths.contains(path),
            "Silent drop: input path missing from output"
        );
    }
}

/// No path appears in both resolved and conflicts.
#[kani::proof]
#[kani::unwind(10)]
fn conflict_monotonicity_no_path_duplication_2ws() {
    let path_a: u8 = kani::any();
    kani::assume(path_a < 4);
    let kind_a: u8 = kani::any();
    kani::assume(kind_a < 3);
    let content_a: u8 = kani::any();
    kani::assume(content_a < 4);

    let path_b: u8 = kani::any();
    kani::assume(path_b < 4);
    let kind_b: u8 = kani::any();
    kani::assume(kind_b < 3);
    let content_b: u8 = kani::any();
    kani::assume(content_b < 4);

    let change_a = file_change(path_a, kind_a, content_a);
    let change_b = file_change(path_b, kind_b, content_b);

    let base = make_base_for_changes(&[&[change_a.clone()], &[change_b.clone()]]);

    let ps = vec![
        PatchSet::new(ws("ws-00"), epoch(), vec![change_a]),
        PatchSet::new(ws("ws-01"), epoch(), vec![change_b]),
    ];

    let result = run_merge(&ps, &base);

    let resolved_paths: BTreeSet<PathBuf> =
        result.resolved.iter().map(|r| r.path().clone()).collect();
    let conflict_paths: BTreeSet<PathBuf> =
        result.conflicts.iter().map(|c| c.path.clone()).collect();

    let overlap: Vec<_> = resolved_paths.intersection(&conflict_paths).collect();
    assert!(
        overlap.is_empty(),
        "Path appears in both resolved and conflicts"
    );
}

/// Modify/delete on the same path always produces a conflict.
#[kani::proof]
#[kani::unwind(10)]
fn conflict_monotonicity_modify_delete_always_conflicts() {
    let path_idx: u8 = kani::any();
    kani::assume(path_idx < 4);
    let content_idx: u8 = kani::any();
    kani::assume(content_idx < 4);

    let path = path_from_index(path_idx);
    let modify = FileChange::new(
        path.clone(),
        ChangeKind::Modified,
        Some(content_from_index(content_idx)),
    );
    let delete = FileChange::new(path.clone(), ChangeKind::Deleted, None);

    let mut base = BTreeMap::new();
    base.insert(path.clone(), b"base content\nline 2\nline 3\n".to_vec());

    let ps = vec![
        PatchSet::new(ws("ws-00"), epoch(), vec![modify]),
        PatchSet::new(ws("ws-01"), epoch(), vec![delete]),
    ];

    let result = run_merge(&ps, &base);

    // Must not be clean.
    assert!(
        !result.is_clean(),
        "Modify/delete must always produce a conflict"
    );

    // The conflict must be on our path.
    let has_conflict = result.conflicts.iter().any(|c| c.path == path);
    assert!(
        has_conflict,
        "Conflict must exist for the modify/delete path"
    );

    // The conflict must have exactly 2 sides.
    let conflict = result.conflicts.iter().find(|c| c.path == path).unwrap();
    assert_eq!(conflict.sides.len(), 2, "Modify/delete should have 2 sides");
}

/// Add/add with different content always produces a conflict.
#[kani::proof]
#[kani::unwind(10)]
fn conflict_monotonicity_add_add_different_conflicts() {
    let path_idx: u8 = kani::any();
    kani::assume(path_idx < 4);
    let content_a: u8 = kani::any();
    kani::assume(content_a < 4);
    let content_b: u8 = kani::any();
    kani::assume(content_b < 4);

    // Ensure contents actually differ.
    kani::assume(content_a != content_b);

    let path = path_from_index(path_idx);
    let add_a = FileChange::new(
        path.clone(),
        ChangeKind::Added,
        Some(content_from_index(content_a)),
    );
    let add_b = FileChange::new(
        path.clone(),
        ChangeKind::Added,
        Some(content_from_index(content_b)),
    );

    let base = BTreeMap::new(); // No base for adds.

    let ps = vec![
        PatchSet::new(ws("ws-00"), epoch(), vec![add_a]),
        PatchSet::new(ws("ws-01"), epoch(), vec![add_b]),
    ];

    let result = run_merge(&ps, &base);

    assert!(
        !result.is_clean(),
        "Add/add with different content must conflict"
    );

    let has_conflict = result.conflicts.iter().any(|c| c.path == path);
    assert!(
        has_conflict,
        "Conflict must exist for add/add different path"
    );
}

// =========================================================================
// PROPERTY: Disjoint changes never conflict
//
// If each workspace touches a unique set of files, the merge must resolve
// cleanly with no conflicts.
// =========================================================================

/// Two workspaces with disjoint file paths never conflict.
#[kani::proof]
#[kani::unwind(10)]
fn disjoint_changes_never_conflict() {
    let kind_a: u8 = kani::any();
    kani::assume(kind_a < 2); // Added or Modified only (not Delete for simplicity)
    let content_a: u8 = kani::any();
    kani::assume(content_a < 4);

    let kind_b: u8 = kani::any();
    kani::assume(kind_b < 2);
    let content_b: u8 = kani::any();
    kani::assume(content_b < 4);

    // Force disjoint paths: ws-00 uses path 0, ws-01 uses path 1.
    let change_a = file_change(0, kind_a, content_a);
    let change_b = file_change(1, kind_b, content_b);

    let base = make_base_for_changes(&[&[change_a.clone()], &[change_b.clone()]]);

    let ps = vec![
        PatchSet::new(ws("ws-00"), epoch(), vec![change_a]),
        PatchSet::new(ws("ws-01"), epoch(), vec![change_b]),
    ];

    let result = run_merge(&ps, &base);

    assert!(
        result.is_clean(),
        "Disjoint changes should never produce conflicts"
    );
    assert_eq!(
        result.resolved.len(),
        2,
        "Should have exactly 2 resolved changes"
    );
}

// =========================================================================
// PROPERTY: Partition invariants
//
// partition_by_path must maintain sorting and correct unique/shared counts.
// =========================================================================

/// Partition output paths are always sorted lexicographically.
#[kani::proof]
#[kani::unwind(10)]
fn partition_paths_sorted() {
    let path_a: u8 = kani::any();
    kani::assume(path_a < 4);
    let path_b: u8 = kani::any();
    kani::assume(path_b < 4);
    let kind: u8 = kani::any();
    kani::assume(kind < 3);

    let change_a = file_change(path_a, kind, 0);
    let change_b = file_change(path_b, kind, 1);

    let ps = vec![
        PatchSet::new(ws("ws-00"), epoch(), vec![change_a]),
        PatchSet::new(ws("ws-01"), epoch(), vec![change_b]),
    ];

    let partition = partition_by_path(&ps);

    // Unique paths must be sorted.
    let unique_paths: Vec<&PathBuf> = partition.unique.iter().map(|(p, _)| p).collect();
    for w in unique_paths.windows(2) {
        assert!(w[0] <= w[1], "Unique paths must be sorted");
    }

    // Shared paths must be sorted.
    let shared_paths: Vec<&PathBuf> = partition.shared.iter().map(|(p, _)| p).collect();
    for w in shared_paths.windows(2) {
        assert!(w[0] <= w[1], "Shared paths must be sorted");
    }
}

/// Partition unique + shared path counts must equal the total distinct
/// input paths.
#[kani::proof]
#[kani::unwind(10)]
fn partition_path_accounting() {
    let path_a: u8 = kani::any();
    kani::assume(path_a < 4);
    let path_b: u8 = kani::any();
    kani::assume(path_b < 4);
    let kind_a: u8 = kani::any();
    kani::assume(kind_a < 3);
    let kind_b: u8 = kani::any();
    kani::assume(kind_b < 3);

    let change_a = file_change(path_a, kind_a, 0);
    let change_b = file_change(path_b, kind_b, 1);

    let ps = vec![
        PatchSet::new(ws("ws-00"), epoch(), vec![change_a.clone()]),
        PatchSet::new(ws("ws-01"), epoch(), vec![change_b.clone()]),
    ];

    let partition = partition_by_path(&ps);

    // Count distinct input paths.
    let mut input_paths = BTreeSet::new();
    input_paths.insert(change_a.path);
    input_paths.insert(change_b.path);

    assert_eq!(
        partition.unique_count() + partition.shared_count(),
        input_paths.len(),
        "unique + shared must equal distinct input paths"
    );
}

// =========================================================================
// PROPERTY: Resolve output paths are sorted
// =========================================================================

/// Resolved and conflict paths in the output are lexicographically sorted.
#[kani::proof]
#[kani::unwind(10)]
fn resolve_output_paths_sorted() {
    let path_a: u8 = kani::any();
    kani::assume(path_a < 4);
    let path_b: u8 = kani::any();
    kani::assume(path_b < 4);
    let kind_a: u8 = kani::any();
    kani::assume(kind_a < 3);
    let kind_b: u8 = kani::any();
    kani::assume(kind_b < 3);
    let content_a: u8 = kani::any();
    kani::assume(content_a < 4);
    let content_b: u8 = kani::any();
    kani::assume(content_b < 4);

    let change_a = file_change(path_a, kind_a, content_a);
    let change_b = file_change(path_b, kind_b, content_b);

    let base = make_base_for_changes(&[&[change_a.clone()], &[change_b.clone()]]);

    let ps = vec![
        PatchSet::new(ws("ws-00"), epoch(), vec![change_a]),
        PatchSet::new(ws("ws-01"), epoch(), vec![change_b]),
    ];

    let result = run_merge(&ps, &base);

    // Resolved paths sorted.
    let res_paths: Vec<&PathBuf> = result.resolved.iter().map(|r| r.path()).collect();
    for w in res_paths.windows(2) {
        assert!(w[0] <= w[1], "Resolved paths must be sorted");
    }

    // Conflict paths sorted.
    let con_paths: Vec<&PathBuf> = result.conflicts.iter().map(|c| &c.path).collect();
    for w in con_paths.windows(2) {
        assert!(w[0] <= w[1], "Conflict paths must be sorted");
    }
}

// =========================================================================
// PROPERTY: PatchSet sorts changes by path on construction
// =========================================================================

/// PatchSet::new always sorts changes by path, regardless of input order.
#[kani::proof]
#[kani::unwind(10)]
fn patch_set_sorts_by_path() {
    let path_a: u8 = kani::any();
    kani::assume(path_a < 4);
    let path_b: u8 = kani::any();
    kani::assume(path_b < 4);

    let change_a = FileChange::new(
        path_from_index(path_a),
        ChangeKind::Added,
        Some(b"a\n".to_vec()),
    );
    let change_b = FileChange::new(
        path_from_index(path_b),
        ChangeKind::Added,
        Some(b"b\n".to_vec()),
    );

    let ps = PatchSet::new(ws("ws-00"), epoch(), vec![change_a, change_b]);

    // Paths must be sorted.
    let paths: Vec<&PathBuf> = ps.paths().collect();
    for w in paths.windows(2) {
        assert!(w[0] <= w[1], "PatchSet paths must be sorted");
    }
}

// =========================================================================
// PROPERTY: Empty inputs produce empty outputs
// =========================================================================

/// Empty patch sets produce an empty, clean result.
#[kani::proof]
#[kani::unwind(5)]
fn empty_patch_sets_produce_empty_result() {
    let n: u8 = kani::any();
    kani::assume(n >= 1 && n <= 3);

    let mut ps = Vec::new();
    for i in 0..n {
        ps.push(PatchSet::new(
            ws(&format!("ws-{i:02}")),
            epoch(),
            vec![],
        ));
    }

    let base = BTreeMap::new();
    let result = run_merge(&ps, &base);

    assert!(result.is_clean(), "Empty inputs should produce clean result");
    assert_eq!(
        result.resolved.len(),
        0,
        "Empty inputs should produce no resolved changes"
    );
    assert_eq!(
        result.conflicts.len(),
        0,
        "Empty inputs should produce no conflicts"
    );
}
