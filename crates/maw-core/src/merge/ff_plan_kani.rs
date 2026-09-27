//! Kani harnesses for the FF-absorb sibling decision logic (bn-27n7).
//!
//! Every harness calls the production functions in [`super`]. Harness names
//! state their bounds; the classifier and verdict harnesses are exhaustive
//! over their finite fact space; the path-set harnesses verify the generic
//! `*_by` cores (which the PathBuf wrappers call) for every conflict
//! relation over a small id domain, because std Path parsing over symbolic
//! bytes does not terminate in CBMC in reasonable time.
//!
//! Run: `cargo kani -p maw-core --harness <name>` (all of them run in `just kani-fast`).

use super::*;
use std::cell::Cell;

/// Bounded symbolic sequence: length 0..=`N`, elements in 0..`alpha`.
/// Returns the backing array and the length.
fn any_seq<const N: usize>(alpha: u8) -> ([u8; N], usize) {
    let arr: [u8; N] = kani::any();
    for x in &arr {
        kani::assume(*x < alpha);
    }
    let len: usize = kani::any();
    kani::assume(len <= N);
    (arr, len)
}

/// component_prefix_either (the core of paths_conflict) is exactly "one
/// component sequence is a prefix of the other", over every pair of
/// sequences of <= 3 components from a 3-symbol alphabet. Symmetry and
/// reflexivity follow because the spec has both. Components are opaque
/// here; std's Path::components splitting is covered by the exhaustive
/// unit test paths_conflict_exhaustive_le_3_comps_matches_legacy_and_spec.
#[kani::proof]
#[kani::unwind(5)]
fn component_prefix_either_matches_prefix_spec_le_3_comps() {
    let (a, la) = any_seq::<3>(3);
    let (b, lb) = any_seq::<3>(3);
    let (a, b) = (&a[..la], &b[..lb]);
    let got = component_prefix_either(a.iter(), b.iter());
    assert_eq!(got, a.starts_with(b) || b.starts_with(a));
}

/// Symbolic conflict relation over ids 0..3 (every one of the 2^9
/// relations).
fn any_relation() -> [[bool; 3]; 3] {
    kani::any()
}

/// stale_dirty_filter_by with a known delta, for EVERY conflict relation:
/// the result is exactly the delta elements (in delta order) that conflict
/// with some dirty element. Hence result is a subset of the delta and is
/// empty iff no (delta, dirty) pair conflicts.
#[kani::proof]
#[kani::unwind(5)]
fn stale_dirty_filter_known_delta_any_relation_le_3_delta_le_2_dirty() {
    let rel = any_relation();
    let (delta, ld) = any_seq::<3>(3);
    let (dirty, lr) = any_seq::<2>(3);
    let (delta, dirty) = (&delta[..ld], &dirty[..lr]);
    let conflict = |x: &u8, y: &u8| rel[*x as usize][*y as usize];
    let got = stale_dirty_filter_by(Some(delta.to_vec()), dirty, conflict);

    let mut expected = Vec::new();
    for x in delta {
        if dirty.iter().any(|y| conflict(x, y)) {
            expected.push(*x);
        }
    }
    assert_eq!(got, expected);
    let any_pair = delta.iter().any(|x| dirty.iter().any(|y| conflict(x, y)));
    assert_eq!(got.is_empty(), !any_pair);
}

/// stale_dirty_filter_by with an unknown delta fails closed: the result is
/// every dirty element, so it is non-empty whenever the sibling is dirty.
#[kani::proof]
#[kani::unwind(5)]
fn stale_dirty_filter_unknown_delta_fails_closed_le_2_dirty() {
    let rel = any_relation();
    let (dirty, lr) = any_seq::<2>(3);
    let dirty = &dirty[..lr];
    let got = stale_dirty_filter_by(None, dirty, |x: &u8, y: &u8| rel[*x as usize][*y as usize]);
    assert_eq!(got.as_slice(), dirty);
    assert_eq!(got.is_empty(), dirty.is_empty());
}

/// split_own_delta_by, for EVERY FF-membership set and conflict relation:
/// every non-FF delta element lands in exactly one list, in order; no
/// refreshed element conflicts with a dirty element; every skipped one
/// does.
#[kani::proof]
#[kani::unwind(5)]
fn split_own_delta_partitions_any_relation_le_3_delta_le_2_dirty() {
    let rel = any_relation();
    let ff: [bool; 3] = kani::any();
    let (delta, ld) = any_seq::<3>(3);
    let (dirty, lr) = any_seq::<2>(3);
    let (delta, dirty) = (&delta[..ld], &dirty[..lr]);
    let conflict = |x: &u8, y: &u8| rel[*x as usize][*y as usize];
    let (refresh, skipped) =
        split_own_delta_by(delta.to_vec(), |x: &u8| ff[*x as usize], dirty, conflict);

    let mut exp_refresh = Vec::new();
    let mut exp_skipped = Vec::new();
    for x in delta {
        if ff[*x as usize] {
            continue;
        }
        if dirty.iter().any(|y| conflict(x, y)) {
            exp_skipped.push(*x);
        } else {
            exp_refresh.push(*x);
        }
    }
    assert_eq!(refresh, exp_refresh);
    assert_eq!(skipped, exp_skipped);
    for x in &refresh {
        assert!(!dirty.iter().any(|y| conflict(x, y)));
    }
}

fn head_of(n: u8) -> SiblingHead {
    match n {
        0 => SiblingHead::Unreadable,
        1 => SiblingHead::AtBase,
        _ => SiblingHead::Ahead,
    }
}

/// Exhaustive over (base_is_branch, head, dirty, stale overlap, probe error):
/// the classifier is total and upholds every FF-absorb safety rule,
/// including which lazy I/O closures run.
#[kani::proof]
fn classify_sibling_safety_exhaustive() {
    let base_is_branch: bool = kani::any();
    let head_n: u8 = kani::any();
    kani::assume(head_n < 3);
    let head = head_of(head_n);
    let dirty: bool = kani::any();
    let stale: bool = kani::any();
    let probe_err: bool = kani::any();

    let probe_calls = Cell::new(0u8);
    let stale_calls = Cell::new(0u8);
    let res = classify_sibling(
        base_is_branch,
        || {
            probe_calls.set(probe_calls.get() + 1);
            if probe_err {
                Err(())
            } else {
                Ok(SiblingProbe { head, dirty })
            }
        },
        || {
            stale_calls.set(stale_calls.get() + 1);
            stale
        },
    );

    // Laziness: no I/O for an already-synced sibling; the stale check only
    // for a dirty, not-ahead sibling whose probe succeeded.
    assert_eq!(probe_calls.get(), u8::from(!base_is_branch));
    let expect_stale_call =
        !base_is_branch && !probe_err && dirty && matches!(head, SiblingHead::AtBase);
    assert_eq!(stale_calls.get(), u8::from(expect_stale_call));

    // Errors propagate (fail closed: the absorb is refused).
    assert_eq!(res.is_err(), !base_is_branch && probe_err);
    let Ok(d) = res else { return };

    assert_eq!(d == SiblingDecision::AlreadySynced, base_is_branch);
    if d == SiblingDecision::FastForward {
        // Only a HEAD proven to be at its base epoch is fast-forwarded:
        // never committed work ahead of base (bn-rah2), never a HEAD that
        // could not be read (bn-302v).
        assert!(matches!(head, SiblingHead::AtBase));
        // Never fast-forward over a stale dirty overlap (bn-mq3b).
        assert!(!(dirty && stale));
    }
    // Block exactly when committed-ahead AND dirty.
    assert_eq!(
        d == SiblingDecision::BlockAbsorb,
        !base_is_branch && matches!(head, SiblingHead::Ahead) && dirty
    );
    // Replay exactly when committed-ahead AND clean.
    assert_eq!(
        d == SiblingDecision::Replay,
        !base_is_branch && matches!(head, SiblingHead::Ahead) && !dirty
    );
    // SkipStaleDirty exactly when dirty, at base, and the stale check hit.
    assert_eq!(
        d == SiblingDecision::SkipStaleDirty,
        !base_is_branch && matches!(head, SiblingHead::AtBase) && dirty && stale
    );
    // bn-302v: SkipUnreadableHead exactly when HEAD could not be read.
    assert_eq!(
        d == SiblingDecision::SkipUnreadableHead,
        !base_is_branch && matches!(head, SiblingHead::Unreadable)
    );
}

/// Unreadable HEAD or tree fails closed, wired through the real
/// stale_dirty_filter_by with an unknown delta (None), for every conflict
/// relation and every dirty set of <= 2 paths (including clean):
/// * HEAD unreadable => SkipUnreadableHead, clean or dirty (bn-302v; a clean
///   sibling used to be fast-forwarded);
/// * HEAD at base, tree unreadable, dirty => SkipStaleDirty (bn-27n7).
#[kani::proof]
#[kani::unwind(5)]
fn classify_unreadable_head_or_tree_never_fast_forwards_le_2_dirty() {
    let rel = any_relation();
    let (dirty, lr) = any_seq::<2>(3);
    let dirty = &dirty[..lr];
    let head_n: u8 = kani::any();
    kani::assume(head_n < 2); // Unreadable HEAD, or AtBase with unreadable tree
    let head = head_of(head_n);
    let is_dirty = !dirty.is_empty();
    let d = classify_sibling::<()>(
        false,
        || {
            Ok(SiblingProbe {
                head,
                dirty: is_dirty,
            })
        },
        || {
            !stale_dirty_filter_by(None, dirty, |x: &u8, y: &u8| rel[*x as usize][*y as usize])
                .is_empty()
        },
    );
    match head {
        SiblingHead::Unreadable => assert_eq!(d, Ok(SiblingDecision::SkipUnreadableHead)),
        _ if is_dirty => assert_eq!(d, Ok(SiblingDecision::SkipStaleDirty)),
        _ => assert_eq!(d, Ok(SiblingDecision::FastForward)),
    }
}

fn decision_of(n: u8) -> SiblingDecision {
    match n {
        0 => SiblingDecision::AlreadySynced,
        1 => SiblingDecision::BlockAbsorb,
        2 => SiblingDecision::Replay,
        3 => SiblingDecision::FastForward,
        4 => SiblingDecision::SkipStaleDirty,
        _ => SiblingDecision::SkipUnreadableHead,
    }
}

/// Any BlockAbsorb => Block verdict carrying exactly the blocking indices
/// (and no mutation plan); no BlockAbsorb => Proceed.
#[kani::proof]
#[kani::unwind(5)]
fn absorb_verdict_blocks_all_or_nothing_le_3_siblings() {
    let (codes, len) = any_seq::<3>(6);
    let all = [
        decision_of(codes[0]),
        decision_of(codes[1]),
        decision_of(codes[2]),
    ];
    let decisions = &all[..len];
    let blockers = decisions
        .iter()
        .filter(|d| **d == SiblingDecision::BlockAbsorb)
        .count();
    match absorb_verdict(decisions) {
        AbsorbVerdict::Proceed => assert_eq!(blockers, 0),
        AbsorbVerdict::Block { blocked } => {
            assert_eq!(blocked.len(), blockers);
            assert!(blockers > 0);
            let mut prev: Option<usize> = None;
            for &i in &blocked {
                assert!(i < len);
                assert_eq!(decisions[i], SiblingDecision::BlockAbsorb);
                if let Some(p) = prev {
                    assert!(p < i);
                }
                prev = Some(i);
            }
        }
    }
}
