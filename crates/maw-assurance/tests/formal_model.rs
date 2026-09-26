//! Exhaustive Stateright check of maw's multi-process protocol model
//! (`maw_assurance::model`, bn-3ppf).
//!
//! Three groups:
//!
//! * **Green** (`fast_*`, run in the gate via `just formal-fast`): every
//!   safety property holds and every `sometimes` event is reachable
//!   (non-vacuity).
//! * **Mutations** (`mutation_*`, also in the gate): each property is shown to
//!   catch a deliberate breakage mirroring a real historical bug. If one of
//!   these starts passing *without* a discovery the property went vacuous.
//! * **Residuals** (`residual_*`): the faithful model under a weaker
//!   environment assumption finds a real counterexample. These pin known,
//!   documented races (see the bn-3ppf bone comments) so a fix shows up as a
//!   test that needs updating, never silently.
//! * **Deep** (`deep_*`, `#[ignore]`): larger configurations for
//!   `just formal-check` / nightly.
//!
//! Requires the `stateright` feature:
//! `cargo test -p maw-assurance --features stateright --test formal_model`

#![cfg(feature = "stateright")]

use std::collections::BTreeSet;

use maw_assurance::model::configs::{self, mutated};
use maw_assurance::model::*;
use stateright::{Checker, HasDiscoveries, Model};

fn threads() -> usize {
    std::thread::available_parallelism().map_or(2, std::num::NonZero::get)
}

fn check_green(model: ProtocolModel) {
    let checker = model.checker().threads(threads()).spawn_bfs().join();
    eprintln!(
        "states: {} unique, max depth {}",
        checker.unique_state_count(),
        checker.max_depth()
    );
    checker.assert_properties();
}

/// Check `model` until `property` has a counterexample; return its length.
fn expect_counterexample(model: ProtocolModel, property: &'static str) -> usize {
    let checker = model
        .checker()
        .threads(threads())
        .finish_when(HasDiscoveries::AnyOf(BTreeSet::from([property])))
        .spawn_bfs()
        .join();
    let path = checker.assert_any_discovery(property);
    let actions = path.into_actions();
    eprintln!("counterexample for {property:?} ({} steps):", actions.len());
    for a in &actions {
        eprintln!("  {a:?}");
    }
    actions.len()
}

// ---------------------------------------------------------------------------
// Green: all properties hold (fast subset, in the gate)
// ---------------------------------------------------------------------------

#[test]
fn fast_merge_crash_destroy() {
    check_green(configs::fast_merge_crash_destroy());
}

#[test]
fn fast_merge_into_branch() {
    check_green(configs::fast_merge_into_branch());
}

#[test]
fn fast_concurrent_actors() {
    check_green(configs::fast_concurrent_actors());
}

/// Control for the bn-29z8 mutation: with agents editing and committing at
/// ANY time, the faithful auto-sync (HEAD CAS + ancestor refusal + dirty
/// re-check) and sync still lose nothing.
#[test]
fn fast_concurrent_actors_agents_anytime() {
    check_green(configs::fast_concurrent_actors_agents_anytime());
}

#[test]
fn fast_destroy_vs_sync() {
    check_green(configs::fast_destroy_vs_sync());
}

#[test]
fn fast_ff_absorb() {
    check_green(configs::fast_ff_absorb());
}

// ---------------------------------------------------------------------------
// Mutations: every property catches a real bug class
// ---------------------------------------------------------------------------

#[test]
fn mutation_destroy_without_capture_loses_work() {
    expect_counterexample(
        mutated(
            configs::fast_merge_crash_destroy(),
            Mutation::DestroyWithoutCapture,
        ),
        P_NO_LOST_WORK,
    );
}

#[test]
fn mutation_bn_mq3b_no_stale_dirty_guard_breaks_coherence() {
    expect_counterexample(
        mutated(configs::fast_ff_absorb(), Mutation::NoStaleDirtyGuard),
        P_WS_COHERENT,
    );
}

#[test]
fn mutation_bn_mq3b_no_stale_dirty_guard_silently_reverts() {
    expect_counterexample(
        mutated(configs::fast_ff_absorb(), Mutation::NoStaleDirtyGuard),
        P_NO_SILENT_REVERT,
    );
}

#[test]
fn mutation_bn_rah2_raw_move_loses_committed_work() {
    expect_counterexample(
        mutated(configs::fast_ff_absorb(), Mutation::RawMoveInsteadOfReplay),
        P_NO_LOST_WORK,
    );
}

#[test]
fn mutation_bn_p3m9_global_ff_paths_only_breaks_coherence() {
    expect_counterexample(
        mutated(configs::fast_ff_absorb(), Mutation::GlobalFfPathsOnly),
        P_WS_COHERENT,
    );
}

#[test]
fn mutation_split_commit_cas_breaks_atomicity() {
    expect_counterexample(
        mutated(
            configs::fast_merge_crash_destroy(),
            Mutation::SplitCommitCas,
        ),
        P_COMMIT_ATOMIC,
    );
}

#[test]
fn mutation_pre_bn_38vw_epoch_after_after_cas_breaks_journal() {
    expect_counterexample(
        mutated(
            configs::fast_merge_crash_destroy(),
            Mutation::EpochAfterAfterCas,
        ),
        P_JOURNAL_COHERENT,
    );
}

#[test]
fn mutation_reversed_lock_order_deadlocks() {
    expect_counterexample(
        mutated(
            configs::fast_concurrent_actors(),
            Mutation::ReversedLockOrder,
        ),
        P_NO_DEADLOCK,
    );
}

#[test]
fn mutation_pre_bn_29z8_autosync_without_head_cas_loses_commit() {
    expect_counterexample(
        mutated(
            configs::fast_concurrent_actors_agents_anytime(),
            Mutation::AutoSyncNoHeadCas,
        ),
        P_NO_LOST_WORK,
    );
}

#[test]
fn mutation_sync_ignores_dirty_loses_work() {
    expect_counterexample(
        mutated(
            configs::fast_concurrent_actors(),
            Mutation::SyncIgnoresDirty,
        ),
        P_NO_LOST_WORK,
    );
}

// ---------------------------------------------------------------------------
// Residuals: real races in the faithful model under weaker assumptions
// ---------------------------------------------------------------------------

/// With agents acting at any time, FF-absorb's classification-time dirty
/// snapshot and unconditional `set_head` let a concurrent agent write be
/// dropped (see bone comment).
#[test]
fn residual_ff_absorb_races_concurrent_agent() {
    expect_counterexample(configs::residual_ff_absorb_agents_anytime(), P_NO_LOST_WORK);
}

/// A crash between the sibling epoch-ref write and `set_head` in the FF-absorb
/// loop leaves the epoch ref AHEAD of HEAD (see bone comment).
#[test]
fn residual_ff_absorb_crash_leaves_leading_epoch_ref() {
    expect_counterexample(configs::residual_ff_absorb_crash(), P_WS_COHERENT);
}

/// ... and the next merge of that sibling silently reverts the absorbed hunks.
#[test]
fn residual_ff_absorb_crash_then_merge_reverts() {
    expect_counterexample(
        configs::residual_ff_absorb_crash_then_merge_sibling(),
        P_NO_SILENT_REVERT,
    );
}

/// A crash between `advance_merge_state(Commit)` and `record_epoch_after`
/// leaves phase=commit with no `epoch_after` — the shape Oracle B flags.
#[test]
fn residual_oracle_b_commit_phase_without_epoch_after() {
    expect_counterexample(configs::residual_oracle_b_strict(), P_ORACLE_B_JOURNAL);
}

/// `maw doctor --repair` writes `refs/manifold/epoch/current` with a plain
/// write and no epoch lock, after re-reading the branch: a merge that commits
/// in between is un-done from the epoch (the epoch regresses).
#[test]
fn residual_doctor_repair_unlocked_regresses_epoch() {
    expect_counterexample(configs::doctor_vs_merge(false), P_EPOCH_MONOTONE);
}

/// Control / proposed fix: the same race with the doctor holding the epoch
/// lock is clean.
#[test]
fn fast_doctor_repair_locked_is_safe() {
    check_green(configs::doctor_vs_merge(true));
}

// ---------------------------------------------------------------------------
// Deep (manual / nightly)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "deep Stateright check; run via `just formal-check`"]
fn deep_everything() {
    check_green(configs::deep_everything());
}
