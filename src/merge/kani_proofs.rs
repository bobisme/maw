//! Kani proof harnesses for merge algebra verification.
//!
//! These harnesses verify algebraic properties of two functions:
//!
//! 1. [`classify_shared_path`] — the pure decision tree (boolean inputs only).
//! 2. [`resolve_entries`] — the full resolve pipeline including classification
//!    AND the k-way diff3 fold, parameterized by `C = u8` so Kani can explore
//!    the content space without OOMing on `Vec<u8>`.
//!
//! # Properties verified
//!
//! ## classify_shared_path (decision tree)
//!
//! 1. **Totality**: every valid input combination produces an output (no panics).
//! 2. **No silent drops**: every classification either resolves or conflicts.
//! 3. **Commutativity**: classification is independent of entry order.
//! 4. **Idempotence**: identical inputs always resolve cleanly.
//! 5. **Conflict rules**: modify/delete always conflicts, add/add different
//!    always conflicts, all-delete always resolves.
//! 6. **Exhaustive decision table**: structural consistency of all 72 2-entry
//!    input combinations.
//!
//! ## resolve_entries (full pipeline with k-way diff3 fold)
//!
//! 7. **Totality**: no panics for any valid 2- or 3-entry input.
//! 8. **Outcome consistency**: results always in {Delete, Upsert, Conflict}.
//! 9. **K-way fold commutativity**: swapping entry order produces the same
//!    outcome (because the diff3 stub is commutative and the fold only matters
//!    when contents differ).
//! 10. **Idempotence**: identical content always resolves to Upsert with that
//!     content.
//! 11. **Conflict monotonicity**: if classify says conflict, resolve_entries
//!     agrees (conflict-before-diff3 never becomes clean-after-diff3).
//! 12. **Diff3 fold correctness**: when diff3 is needed, the fold applies it
//!     to all variants and threads the result correctly.
//!
//! # Running
//!
//! ```bash
//! cargo kani --no-default-features --harness <harness_name>
//! # or all harnesses:
//! cargo kani --no-default-features
//! ```

#![allow(clippy::all, clippy::pedantic, clippy::nursery)]

use crate::merge::resolve::{
    ConflictReason, Diff3Result, MergeOutcome, SharedClassification,
    classify_shared_path, resolve_entries,
};
use crate::merge::types::ChangeKind;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a `ChangeKind` from a bounded symbolic index (0..3).
fn kind_from_index(idx: u8) -> ChangeKind {
    match idx % 3 {
        0 => ChangeKind::Added,
        1 => ChangeKind::Modified,
        _ => ChangeKind::Deleted,
    }
}

/// Whether a kind is a deletion.
fn is_delete(k: &ChangeKind) -> bool {
    matches!(k, ChangeKind::Deleted)
}

/// Deterministic stub diff3 for `u8` content.
///
/// This models the essential contract of any correct diff3 implementation:
/// - If both variants agree: clean merge with that value.
/// - If only one side changed from base: clean merge with the changed side.
/// - If both sides changed from base differently: conflict.
///
/// This is richer than `ours == theirs → clean, else conflict` because it
/// captures the "one-side-changed" rule that real diff3 implements.
fn stub_diff3(base: &u8, ours: &u8, theirs: &u8) -> Result<Diff3Result<u8>, ()> {
    if ours == theirs {
        Ok(Diff3Result::Clean(*ours))
    } else if ours == base {
        // Only theirs changed.
        Ok(Diff3Result::Clean(*theirs))
    } else if theirs == base {
        // Only ours changed.
        Ok(Diff3Result::Clean(*ours))
    } else {
        // Both changed differently.
        Ok(Diff3Result::Conflict)
    }
}

/// Build a content slice from kinds and symbolic values.
///
/// Deleted entries get `None`, non-deleted get `Some(value)`.
fn make_contents(kinds: &[ChangeKind], values: &[u8]) -> Vec<Option<u8>> {
    kinds
        .iter()
        .zip(values.iter())
        .map(|(k, v)| {
            if matches!(k, ChangeKind::Deleted) {
                None
            } else {
                Some(*v)
            }
        })
        .collect()
}


// =========================================================================
// PROPERTY: Totality — no panics for any valid input
// =========================================================================

/// The classifier handles all 2-entry combinations without panicking.
#[kani::proof]
#[kani::unwind(3)]
fn totality_2_entries() {
    let k0: u8 = kani::any();
    kani::assume(k0 < 3);
    let k1: u8 = kani::any();
    kani::assume(k1 < 3);

    let kinds = [kind_from_index(k0), kind_from_index(k1)];
    let all_have_content: bool = kani::any();
    let all_content_equal: bool = kani::any();
    let has_base: bool = kani::any();

    // Must not panic.
    let _result = classify_shared_path(&kinds, all_have_content, all_content_equal, has_base);
}

/// The classifier handles all 3-entry combinations without panicking.
#[kani::proof]
#[kani::unwind(4)]
fn totality_3_entries() {
    let k0: u8 = kani::any();
    kani::assume(k0 < 3);
    let k1: u8 = kani::any();
    kani::assume(k1 < 3);
    let k2: u8 = kani::any();
    kani::assume(k2 < 3);

    let kinds = [kind_from_index(k0), kind_from_index(k1), kind_from_index(k2)];
    let all_have_content: bool = kani::any();
    let all_content_equal: bool = kani::any();
    let has_base: bool = kani::any();

    let _result = classify_shared_path(&kinds, all_have_content, all_content_equal, has_base);
}

// =========================================================================
// PROPERTY: No silent drops — every result is resolved or conflict
// =========================================================================

/// Every classification is either a resolution, a conflict, or NeedsDiff3.
/// There is no "unknown" or "dropped" state.
#[kani::proof]
#[kani::unwind(3)]
fn no_silent_drops_2_entries() {
    let k0: u8 = kani::any();
    kani::assume(k0 < 3);
    let k1: u8 = kani::any();
    kani::assume(k1 < 3);

    let kinds = [kind_from_index(k0), kind_from_index(k1)];
    let all_have_content: bool = kani::any();
    let all_content_equal: bool = kani::any();
    let has_base: bool = kani::any();

    let result = classify_shared_path(&kinds, all_have_content, all_content_equal, has_base);

    // Must be one of the known outcomes.
    assert!(matches!(
        result,
        SharedClassification::ResolvedDelete
            | SharedClassification::ResolvedIdentical
            | SharedClassification::ConflictModifyDelete
            | SharedClassification::ConflictMissingContent
            | SharedClassification::ConflictAddAddDifferent
            | SharedClassification::ConflictMissingBase
            | SharedClassification::NeedsDiff3
    ));
}

// =========================================================================
// PROPERTY: Commutativity — order of entries doesn't matter
// =========================================================================

/// Swapping two entries produces the same classification.
///
/// The classifier only inspects aggregate properties (all-delete, any-delete,
/// content equality, base presence), so it's inherently order-independent.
/// This proof verifies that invariant exhaustively.
#[kani::proof]
#[kani::unwind(3)]
fn commutativity_2_entries() {
    let k0: u8 = kani::any();
    kani::assume(k0 < 3);
    let k1: u8 = kani::any();
    kani::assume(k1 < 3);

    let all_have_content: bool = kani::any();
    let all_content_equal: bool = kani::any();
    let has_base: bool = kani::any();

    let forward = [kind_from_index(k0), kind_from_index(k1)];
    let reverse = [kind_from_index(k1), kind_from_index(k0)];

    let result_fwd = classify_shared_path(&forward, all_have_content, all_content_equal, has_base);
    let result_rev = classify_shared_path(&reverse, all_have_content, all_content_equal, has_base);

    assert_eq!(
        result_fwd, result_rev,
        "Classification must be independent of entry order"
    );
}

/// All 6 permutations of 3 entries produce the same classification.
#[kani::proof]
#[kani::unwind(6)]
fn commutativity_3_entries() {
    let k0: u8 = kani::any();
    kani::assume(k0 < 3);
    let k1: u8 = kani::any();
    kani::assume(k1 < 3);
    let k2: u8 = kani::any();
    kani::assume(k2 < 3);

    let all_have_content: bool = kani::any();
    let all_content_equal: bool = kani::any();
    let has_base: bool = kani::any();

    let a = kind_from_index(k0);
    let b = kind_from_index(k1);
    let c = kind_from_index(k2);

    let ref_result = classify_shared_path(
        &[a.clone(), b.clone(), c.clone()],
        all_have_content,
        all_content_equal,
        has_base,
    );

    // All 5 other permutations.
    let perms: [[ChangeKind; 3]; 5] = [
        [a.clone(), c.clone(), b.clone()],
        [b.clone(), a.clone(), c.clone()],
        [b.clone(), c.clone(), a.clone()],
        [c.clone(), a.clone(), b.clone()],
        [c.clone(), b.clone(), a.clone()],
    ];

    for perm in &perms {
        let r = classify_shared_path(perm, all_have_content, all_content_equal, has_base);
        assert_eq!(ref_result, r, "3-entry permutation produced different classification");
    }
}

// =========================================================================
// PROPERTY: Idempotence — identical inputs resolve cleanly
// =========================================================================

/// When all entries are identical non-deletes with content, content equality
/// holds, and the classifier must return ResolvedIdentical.
#[kani::proof]
#[kani::unwind(4)]
fn idempotence_identical_non_deletes() {
    let kind: u8 = kani::any();
    kani::assume(kind < 2); // Added or Modified only.

    let n: u8 = kani::any();
    kani::assume(n >= 2 && n <= 3);

    let k = kind_from_index(kind);
    let mut kinds = Vec::new();
    for _ in 0..n {
        kinds.push(k.clone());
    }

    // Identical inputs: all have content, all content equal.
    let result = classify_shared_path(&kinds, true, true, kani::any());

    assert_eq!(
        result,
        SharedClassification::ResolvedIdentical,
        "Identical non-delete inputs must resolve cleanly"
    );
}

/// When all entries are deletions, the classifier must return ResolvedDelete.
#[kani::proof]
#[kani::unwind(4)]
fn idempotence_all_deletes() {
    let n: u8 = kani::any();
    kani::assume(n >= 2 && n <= 3);

    let mut kinds = Vec::new();
    for _ in 0..n {
        kinds.push(ChangeKind::Deleted);
    }

    let result = classify_shared_path(&kinds, kani::any(), kani::any(), kani::any());

    assert_eq!(
        result,
        SharedClassification::ResolvedDelete,
        "All-delete must resolve to delete"
    );
}

// =========================================================================
// PROPERTY: Conflict rules — specific combinations always conflict
// =========================================================================

/// Modify/delete always produces ConflictModifyDelete.
#[kani::proof]
#[kani::unwind(3)]
fn modify_delete_always_conflicts() {
    let non_delete_kind: u8 = kani::any();
    kani::assume(non_delete_kind < 2); // Added or Modified.

    let kinds = [kind_from_index(non_delete_kind), ChangeKind::Deleted];

    let result = classify_shared_path(&kinds, kani::any(), kani::any(), kani::any());

    assert_eq!(
        result,
        SharedClassification::ConflictModifyDelete,
        "Any mix of delete and non-delete must be ModifyDelete conflict"
    );
}

/// Add/add with different content and no base always conflicts.
#[kani::proof]
#[kani::unwind(3)]
fn add_add_different_always_conflicts() {
    let kinds = [ChangeKind::Added, ChangeKind::Added];

    // Different content, no base.
    let result = classify_shared_path(&kinds, true, false, false);

    assert_eq!(
        result,
        SharedClassification::ConflictAddAddDifferent,
        "Add/add with different content and no base must conflict"
    );
}

/// Missing content on non-delete entries always produces MissingContent conflict.
#[kani::proof]
#[kani::unwind(3)]
fn missing_content_always_conflicts() {
    let k0: u8 = kani::any();
    kani::assume(k0 < 2); // Non-delete.
    let k1: u8 = kani::any();
    kani::assume(k1 < 2); // Non-delete.

    let kinds = [kind_from_index(k0), kind_from_index(k1)];

    // all_have_content = false (some entry lacks content).
    let result = classify_shared_path(&kinds, false, kani::any(), kani::any());

    assert_eq!(
        result,
        SharedClassification::ConflictMissingContent,
        "Missing content must always conflict"
    );
}

// =========================================================================
// PROPERTY: NeedsDiff3 conditions
// =========================================================================

/// NeedsDiff3 requires: no deletes, all have content, content differs, base present.
#[kani::proof]
#[kani::unwind(3)]
fn needs_diff3_conditions() {
    let k0: u8 = kani::any();
    kani::assume(k0 < 2); // Non-delete.
    let k1: u8 = kani::any();
    kani::assume(k1 < 2); // Non-delete.

    let kinds = [kind_from_index(k0), kind_from_index(k1)];

    // All have content, content differs, base present.
    let result = classify_shared_path(&kinds, true, false, true);

    assert_eq!(
        result,
        SharedClassification::NeedsDiff3,
        "Different content with base must produce NeedsDiff3"
    );
}

/// Without base and not all adds, produces MissingBase.
#[kani::proof]
#[kani::unwind(3)]
fn no_base_mixed_kinds_is_missing_base() {
    // Modified + Modified, different content, no base.
    let kinds = [ChangeKind::Modified, ChangeKind::Modified];

    let result = classify_shared_path(&kinds, true, false, false);

    assert_eq!(
        result,
        SharedClassification::ConflictMissingBase,
        "Different content, no base, not all adds must be MissingBase"
    );
}

// =========================================================================
// PROPERTY: Exhaustive 2-entry decision table
//
// For 2 entries, there are 3×3 = 9 kind combinations × 2×2×2 = 8 boolean
// combinations = 72 total inputs. Verify the classifier produces a
// consistent result for each.
// =========================================================================

/// Full exhaustive sweep of the 2-entry state space.
/// Verifies that the classifier is a total function with no contradictions.
#[kani::proof]
#[kani::unwind(3)]
fn exhaustive_2_entry_decision_table() {
    let k0: u8 = kani::any();
    kani::assume(k0 < 3);
    let k1: u8 = kani::any();
    kani::assume(k1 < 3);

    let kinds = [kind_from_index(k0), kind_from_index(k1)];
    let all_have_content: bool = kani::any();
    let all_content_equal: bool = kani::any();
    let has_base: bool = kani::any();

    let result = classify_shared_path(&kinds, all_have_content, all_content_equal, has_base);

    // Verify structural consistency: if we got a resolution, it can't also
    // be a conflict. If we got NeedsDiff3, the prerequisites must hold.
    match result {
        SharedClassification::ResolvedDelete => {
            assert!(kinds.iter().all(|k| is_delete(k)));
        }
        SharedClassification::ResolvedIdentical => {
            assert!(all_have_content && all_content_equal);
            assert!(!kinds.iter().any(|k| is_delete(k)));
        }
        SharedClassification::ConflictModifyDelete => {
            assert!(kinds.iter().any(|k| is_delete(k)));
            assert!(kinds.iter().any(|k| !is_delete(k)));
        }
        SharedClassification::ConflictMissingContent => {
            assert!(!all_have_content);
            assert!(!kinds.iter().all(|k| is_delete(k)));
            assert!(!kinds.iter().any(|k| is_delete(k)));
        }
        SharedClassification::ConflictAddAddDifferent => {
            assert!(!all_content_equal);
            assert!(!has_base);
            assert!(kinds.iter().all(|k| matches!(k, ChangeKind::Added)));
        }
        SharedClassification::ConflictMissingBase => {
            assert!(!all_content_equal);
            assert!(!has_base);
            assert!(!kinds.iter().all(|k| matches!(k, ChangeKind::Added)));
        }
        SharedClassification::NeedsDiff3 => {
            assert!(all_have_content);
            assert!(!all_content_equal);
            assert!(has_base);
            assert!(!kinds.iter().any(|k| is_delete(k)));
        }
    }
}

// =========================================================================
// resolve_entries<u8> PROOFS — full pipeline including k-way diff3 fold
//
// These exercise the production resolve_entries function with C=u8 and
// stub_diff3, verifying properties that classify_shared_path alone cannot:
// the k-way fold, content threading, and outcome consistency.
// =========================================================================

// =========================================================================
// PROPERTY: Totality — resolve_entries never panics
// =========================================================================

/// resolve_entries handles all valid 2-entry inputs without panicking.
///
/// Content values bounded to 0..4 (4 values cover all relationship cases:
/// base==ours, base==theirs, ours==theirs, all-different, all-same).
#[kani::proof]
#[kani::unwind(3)]
fn re_totality_2_entries() {
    let k0: u8 = kani::any();
    kani::assume(k0 < 3);
    let k1: u8 = kani::any();
    kani::assume(k1 < 3);

    let kinds = [kind_from_index(k0), kind_from_index(k1)];

    let v0: u8 = kani::any();
    kani::assume(v0 < 4);
    let v1: u8 = kani::any();
    kani::assume(v1 < 4);
    let contents = make_contents(&kinds, &[v0, v1]);

    let base_val: u8 = kani::any();
    kani::assume(base_val < 4);
    let has_base: bool = kani::any();
    let base = if has_base { Some(&base_val) } else { None };

    let _result = resolve_entries(&kinds, &contents, base, stub_diff3);
}

/// resolve_entries handles all valid 3-entry inputs without panicking.
#[kani::proof]
#[kani::unwind(4)]
fn re_totality_3_entries() {
    let k0: u8 = kani::any();
    kani::assume(k0 < 3);
    let k1: u8 = kani::any();
    kani::assume(k1 < 3);
    let k2: u8 = kani::any();
    kani::assume(k2 < 3);

    let kinds = [kind_from_index(k0), kind_from_index(k1), kind_from_index(k2)];

    let v0: u8 = kani::any();
    kani::assume(v0 < 4);
    let v1: u8 = kani::any();
    kani::assume(v1 < 4);
    let v2: u8 = kani::any();
    kani::assume(v2 < 4);
    let contents = make_contents(&kinds, &[v0, v1, v2]);

    let base_val: u8 = kani::any();
    kani::assume(base_val < 4);
    let has_base: bool = kani::any();
    let base = if has_base { Some(&base_val) } else { None };

    let _result = resolve_entries(&kinds, &contents, base, stub_diff3);
}

// =========================================================================
// PROPERTY: Outcome consistency — every result is Delete, Upsert, or Conflict
// =========================================================================

/// Every resolve_entries result falls into exactly one outcome category.
/// Upsert always carries content. Delete and Conflict carry no content.
#[kani::proof]
#[kani::unwind(3)]
fn re_outcome_consistency_2_entries() {
    let k0: u8 = kani::any();
    kani::assume(k0 < 3);
    let k1: u8 = kani::any();
    kani::assume(k1 < 3);

    let kinds = [kind_from_index(k0), kind_from_index(k1)];

    let v0: u8 = kani::any();
    kani::assume(v0 < 4);
    let v1: u8 = kani::any();
    kani::assume(v1 < 4);
    let contents = make_contents(&kinds, &[v0, v1]);

    let base_val: u8 = kani::any();
    kani::assume(base_val < 4);
    let has_base: bool = kani::any();
    let base = if has_base { Some(&base_val) } else { None };

    let result = resolve_entries(&kinds, &contents, base, stub_diff3);
    assert!(result.is_ok(), "stub_diff3 never returns Err");

    match result.unwrap() {
        MergeOutcome::Delete => {
            // Delete requires all entries to be deletions.
            assert!(kinds.iter().all(|k| is_delete(k)));
        }
        MergeOutcome::Upsert(_content) => {
            // Upsert requires at least one non-delete entry.
            assert!(kinds.iter().any(|k| !is_delete(k)));
        }
        MergeOutcome::Conflict(reason) => {
            // Conflict reason must be one of the known variants.
            assert!(matches!(
                reason,
                ConflictReason::ModifyDelete
                    | ConflictReason::AddAddDifferent
                    | ConflictReason::MissingBase
                    | ConflictReason::MissingContent
                    | ConflictReason::Diff3Conflict
            ));
        }
    }
}

// =========================================================================
// PROPERTY: K-way fold commutativity — entry order doesn't change outcome
// =========================================================================

/// Swapping 2 entries produces the same resolve_entries outcome.
///
/// This is stronger than classify_shared_path commutativity because it also
/// verifies that the diff3 fold produces the same merged content regardless
/// of entry order (given our commutative stub_diff3).
#[kani::proof]
#[kani::unwind(3)]
fn re_commutativity_2_entries() {
    let k0: u8 = kani::any();
    kani::assume(k0 < 3);
    let k1: u8 = kani::any();
    kani::assume(k1 < 3);

    let v0: u8 = kani::any();
    kani::assume(v0 < 4);
    let v1: u8 = kani::any();
    kani::assume(v1 < 4);

    let base_val: u8 = kani::any();
    kani::assume(base_val < 4);
    let has_base: bool = kani::any();
    let base = if has_base { Some(&base_val) } else { None };

    let kinds_fwd = [kind_from_index(k0), kind_from_index(k1)];
    let contents_fwd = make_contents(&kinds_fwd, &[v0, v1]);

    let kinds_rev = [kind_from_index(k1), kind_from_index(k0)];
    let contents_rev = make_contents(&kinds_rev, &[v1, v0]);

    let r_fwd = resolve_entries(&kinds_fwd, &contents_fwd, base, stub_diff3).unwrap();
    let r_rev = resolve_entries(&kinds_rev, &contents_rev, base, stub_diff3).unwrap();

    assert_eq!(r_fwd, r_rev, "resolve_entries must be commutative");
}

/// All 6 permutations of 3 entries produce the same resolve_entries outcome.
///
/// Content values bounded to 0..4 to keep the SAT solver tractable.
/// 4 values suffice: base, plus up to 3 distinct workspace values.
#[kani::proof]
#[kani::unwind(8)]
fn re_commutativity_3_entries() {
    let k0: u8 = kani::any();
    kani::assume(k0 < 3);
    let k1: u8 = kani::any();
    kani::assume(k1 < 3);
    let k2: u8 = kani::any();
    kani::assume(k2 < 3);

    let v0: u8 = kani::any();
    kani::assume(v0 < 4);
    let v1: u8 = kani::any();
    kani::assume(v1 < 4);
    let v2: u8 = kani::any();
    kani::assume(v2 < 4);

    let base_val: u8 = kani::any();
    kani::assume(base_val < 4);
    let has_base: bool = kani::any();
    let base = if has_base { Some(&base_val) } else { None };

    // Reference ordering: [0, 1, 2].
    let kinds_ref = [kind_from_index(k0), kind_from_index(k1), kind_from_index(k2)];
    let contents_ref = make_contents(&kinds_ref, &[v0, v1, v2]);
    let ref_result = resolve_entries(&kinds_ref, &contents_ref, base, stub_diff3).unwrap();

    // Permutation [1, 0, 2].
    let kinds_p = [kind_from_index(k1), kind_from_index(k0), kind_from_index(k2)];
    let contents_p = make_contents(&kinds_p, &[v1, v0, v2]);
    let r = resolve_entries(&kinds_p, &contents_p, base, stub_diff3).unwrap();
    assert_eq!(ref_result, r, "3-entry perm [1,0,2] differs");

    // Permutation [0, 2, 1].
    let kinds_p = [kind_from_index(k0), kind_from_index(k2), kind_from_index(k1)];
    let contents_p = make_contents(&kinds_p, &[v0, v2, v1]);
    let r = resolve_entries(&kinds_p, &contents_p, base, stub_diff3).unwrap();
    assert_eq!(ref_result, r, "3-entry perm [0,2,1] differs");

    // Permutation [2, 1, 0].
    let kinds_p = [kind_from_index(k2), kind_from_index(k1), kind_from_index(k0)];
    let contents_p = make_contents(&kinds_p, &[v2, v1, v0]);
    let r = resolve_entries(&kinds_p, &contents_p, base, stub_diff3).unwrap();
    assert_eq!(ref_result, r, "3-entry perm [2,1,0] differs");

    // Permutation [1, 2, 0].
    let kinds_p = [kind_from_index(k1), kind_from_index(k2), kind_from_index(k0)];
    let contents_p = make_contents(&kinds_p, &[v1, v2, v0]);
    let r = resolve_entries(&kinds_p, &contents_p, base, stub_diff3).unwrap();
    assert_eq!(ref_result, r, "3-entry perm [1,2,0] differs");

    // Permutation [2, 0, 1].
    let kinds_p = [kind_from_index(k2), kind_from_index(k0), kind_from_index(k1)];
    let contents_p = make_contents(&kinds_p, &[v2, v0, v1]);
    let r = resolve_entries(&kinds_p, &contents_p, base, stub_diff3).unwrap();
    assert_eq!(ref_result, r, "3-entry perm [2,0,1] differs");
}

// =========================================================================
// PROPERTY: Idempotence — identical content always resolves to Upsert
// =========================================================================

/// When all non-delete entries have identical content, resolve_entries
/// returns Upsert with that content (not Delete, not Conflict).
#[kani::proof]
#[kani::unwind(4)]
fn re_idempotence_identical_content() {
    let kind: u8 = kani::any();
    kani::assume(kind < 2); // Added or Modified only.

    let n: u8 = kani::any();
    kani::assume(n >= 2 && n <= 3);

    let k = kind_from_index(kind);
    let val: u8 = kani::any();
    kani::assume(val < 4);

    let mut kinds = Vec::new();
    let mut contents = Vec::new();
    for _ in 0..n {
        kinds.push(k.clone());
        contents.push(Some(val));
    }

    // Base can be anything — identical content short-circuits before diff3.
    let base_val: u8 = kani::any();
    kani::assume(base_val < 4);
    let has_base: bool = kani::any();
    let base = if has_base { Some(&base_val) } else { None };

    let result = resolve_entries(&kinds, &contents, base, stub_diff3).unwrap();

    assert_eq!(
        result,
        MergeOutcome::Upsert(val),
        "Identical non-delete content must resolve to Upsert"
    );
}

/// When all entries are deletes, resolve_entries returns Delete.
#[kani::proof]
#[kani::unwind(4)]
fn re_idempotence_all_deletes() {
    let n: u8 = kani::any();
    kani::assume(n >= 2 && n <= 3);

    let mut kinds = Vec::new();
    let mut contents = Vec::new();
    for _ in 0..n {
        kinds.push(ChangeKind::Deleted);
        contents.push(None);
    }

    let base_val: u8 = kani::any();
    kani::assume(base_val < 4);
    let has_base: bool = kani::any();
    let base = if has_base { Some(&base_val) } else { None };

    let result = resolve_entries(&kinds, &contents, base, stub_diff3).unwrap();

    assert_eq!(result, MergeOutcome::Delete, "All deletes must produce Delete");
}

// =========================================================================
// PROPERTY: Conflict monotonicity — pre-diff3 conflicts stay conflicts
// =========================================================================

/// If classify_shared_path says it's a conflict (not NeedsDiff3), then
/// resolve_entries also produces a Conflict with the corresponding reason.
#[kani::proof]
#[kani::unwind(3)]
fn re_conflict_monotonicity() {
    let k0: u8 = kani::any();
    kani::assume(k0 < 3);
    let k1: u8 = kani::any();
    kani::assume(k1 < 3);

    let kinds = [kind_from_index(k0), kind_from_index(k1)];

    let v0: u8 = kani::any();
    kani::assume(v0 < 4);
    let v1: u8 = kani::any();
    kani::assume(v1 < 4);
    let contents = make_contents(&kinds, &[v0, v1]);

    let base_val: u8 = kani::any();
    kani::assume(base_val < 4);
    let has_base: bool = kani::any();
    let base = if has_base { Some(&base_val) } else { None };

    // Compute the classification.
    let all_have_content = kinds.iter().zip(contents.iter()).all(|(k, c)| {
        matches!(k, ChangeKind::Deleted) || c.is_some()
    });
    let non_delete_contents: Vec<&u8> = contents.iter().filter_map(|c| c.as_ref()).collect();
    let all_content_equal = if non_delete_contents.len() >= 2 {
        non_delete_contents.windows(2).all(|w| w[0] == w[1])
    } else {
        true
    };
    let cls = classify_shared_path(&kinds, all_have_content, all_content_equal, base.is_some());

    if cls.is_conflict() {
        let result = resolve_entries(&kinds, &contents, base, stub_diff3).unwrap();
        assert!(
            matches!(result, MergeOutcome::Conflict(_)),
            "Pre-diff3 conflict must remain conflict through resolve_entries"
        );
    }
}

// =========================================================================
// PROPERTY: Diff3 fold correctness — one-side-changed resolves cleanly
// =========================================================================

/// When 2 workspaces modify a file, one matching the base and one changed,
/// resolve_entries produces Upsert with the changed content (not conflict).
/// This verifies the diff3 fold correctly picks the non-trivial side.
#[kani::proof]
fn re_diff3_one_side_changed() {
    let base_val: u8 = kani::any();
    let changed_val: u8 = kani::any();
    kani::assume(base_val != changed_val);

    let kinds = [ChangeKind::Modified, ChangeKind::Modified];
    let contents = [Some(base_val), Some(changed_val)];
    let base = Some(&base_val);

    let result = resolve_entries(&kinds, &contents, base, stub_diff3).unwrap();

    assert_eq!(
        result,
        MergeOutcome::Upsert(changed_val),
        "One-side-changed must resolve to the changed side"
    );
}

/// When 3 workspaces modify a file, two matching base and one changed,
/// resolve_entries picks the changed content.
#[kani::proof]
fn re_diff3_one_of_three_changed() {
    let base_val: u8 = kani::any();
    let changed_val: u8 = kani::any();
    kani::assume(base_val != changed_val);

    let kinds = [ChangeKind::Modified, ChangeKind::Modified, ChangeKind::Modified];
    let contents = [Some(base_val), Some(changed_val), Some(base_val)];
    let base = Some(&base_val);

    let result = resolve_entries(&kinds, &contents, base, stub_diff3).unwrap();

    assert_eq!(
        result,
        MergeOutcome::Upsert(changed_val),
        "One-of-three changed must resolve to the changed value"
    );
}

/// When both sides changed from base differently, diff3 reports conflict.
#[kani::proof]
fn re_diff3_both_sides_changed_conflicts() {
    let base_val: u8 = kani::any();
    let val_a: u8 = kani::any();
    let val_b: u8 = kani::any();
    kani::assume(base_val != val_a);
    kani::assume(base_val != val_b);
    kani::assume(val_a != val_b);

    let kinds = [ChangeKind::Modified, ChangeKind::Modified];
    let contents = [Some(val_a), Some(val_b)];
    let base = Some(&base_val);

    let result = resolve_entries(&kinds, &contents, base, stub_diff3).unwrap();

    assert_eq!(
        result,
        MergeOutcome::Conflict(ConflictReason::Diff3Conflict),
        "Both-sides-changed-differently must conflict"
    );
}
