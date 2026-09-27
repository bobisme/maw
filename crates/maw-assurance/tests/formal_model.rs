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
//!   documented races so a fix shows up as a test that needs updating, never
//!   silently. (The bn-3ppf FF-absorb residuals were fixed by bn-302v and are
//!   now `fast_*` tests with the old shapes kept as `mutation_*`.)
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

/// bn-32g8 fix: `maw doctor --repair` takes the epoch lock and CAS-advances
/// the epoch from the classified value, so racing `ws merge` (FF-absorb +
/// commit) and a direct trunk commit never regresses the epoch, and the
/// doctor still gets to advance (non-vacuity).
#[test]
fn fast_doctor_repair_vs_merge() {
    check_green(configs::doctor_vs_merge());
}

/// bn-3w2b fix: COMMIT enters `phase=commit` and records `epoch_after` in
/// ONE journal write, so Oracle B's strict journal shape holds at every crash
/// point (was `residual_oracle_b_commit_phase_without_epoch_after`).
#[test]
fn fast_oracle_b_strict() {
    check_green(configs::fast_oracle_b_strict());
}

/// bn-3w2b fix: `maw merge promote` under the epoch lock with one atomic
/// epoch+branch CAS never splits the refs, never regresses the epoch, and
/// loses nothing, racing `ws merge` + FF-absorb + a crash; promote and merge
/// are both still reachable (non-vacuity).
#[test]
fn fast_quarantine_promote_vs_merge() {
    check_green(configs::fast_quarantine_promote_vs_merge());
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

/// Pre-bn-32g8 `doctor --repair` (no epoch lock, branch re-read, plain epoch
/// write): a merge that commits between the doctor's read and its write is
/// un-done from the epoch — the epoch regresses behind the branch.
#[test]
fn mutation_pre_bn_32g8_doctor_repair_unlocked_regresses_epoch() {
    expect_counterexample(
        mutated(configs::doctor_vs_merge(), Mutation::DoctorRepairUnlocked),
        P_EPOCH_MONOTONE,
    );
}

/// Pre-bn-3w2b COMMIT journal: `advance_merge_state(Commit)` and
/// `record_epoch_after` as two writes; a crash between them leaves
/// phase=commit with no `epoch_after` — the shape Oracle B flags.
#[test]
fn mutation_pre_bn_3w2b_split_commit_journal_breaks_oracle_b() {
    expect_counterexample(
        mutated(
            configs::fast_oracle_b_strict(),
            Mutation::SplitCommitJournal,
        ),
        P_ORACLE_B_JOURNAL,
    );
}

/// Pre-bn-3w2b `maw merge promote` (no epoch lock, epoch CAS then branch
/// CAS): the refs are observed split around the quarantine candidate.
#[test]
fn mutation_pre_bn_3w2b_promote_split_cas_breaks_atomicity() {
    expect_counterexample(
        mutated(
            configs::fast_quarantine_promote_vs_merge(),
            Mutation::QuarantinePromoteUnlockedSplitCas,
        ),
        P_COMMIT_ATOMIC,
    );
}

/// ... and racing FF-absorb's plain `write_epoch_current`, the promoted epoch
/// is overwritten by the absorbed branch tip (the epoch regresses).
#[test]
fn mutation_pre_bn_3w2b_promote_unlocked_regresses_epoch() {
    expect_counterexample(
        mutated(
            configs::fast_quarantine_promote_vs_merge(),
            Mutation::QuarantinePromoteUnlockedSplitCas,
        ),
        P_EPOCH_MONOTONE,
    );
}

// ---------------------------------------------------------------------------
// bn-302v: former residuals, now must-hold, with the old shapes as mutations
// ---------------------------------------------------------------------------

/// With agents editing and committing at ANY time, the FF-absorb sibling loop
/// (sibling try-lock + re-check on fresh facts + HEAD CAS) loses nothing.
/// Was `residual_ff_absorb_races_concurrent_agent`.
#[test]
fn fast_ff_absorb_agents_anytime() {
    check_green(configs::fast_ff_absorb_agents_anytime());
}

/// Pre-bn-302v sibling write (no lock, classification-time dirty set,
/// unconditional `set_head`): a concurrent agent write is dropped.
#[test]
fn mutation_pre_bn_302v_ff_no_sibling_lock_loses_agent_work() {
    expect_counterexample(
        mutated(
            configs::fast_ff_absorb_agents_anytime(),
            Mutation::FfNoSiblingLockRecheck,
        ),
        P_NO_LOST_WORK,
    );
}

/// A crash anywhere inside the FF-absorb sibling loop leaves every sibling
/// coherent: its epoch ref is written last, so it is never ahead of HEAD.
/// Was `residual_ff_absorb_crash_leaves_leading_epoch_ref`.
#[test]
fn fast_ff_absorb_crash_in_loop() {
    check_green(configs::fast_ff_absorb_crash_in_loop());
}

/// Pre-bn-302v order (epoch ref first): a crash leaves it AHEAD of HEAD.
#[test]
fn mutation_pre_bn_302v_ff_ref_before_head_breaks_coherence() {
    expect_counterexample(
        mutated(
            configs::fast_ff_absorb_crash_in_loop(),
            Mutation::FfRefBeforeHead,
        ),
        P_WS_COHERENT,
    );
}

/// ... and the next merge of that sibling cannot silently revert the
/// absorbed hunks. Was `residual_ff_absorb_crash_then_merge_reverts`.
#[test]
fn fast_ff_absorb_crash_then_merge_sibling() {
    check_green(configs::fast_ff_absorb_crash_then_merge_sibling());
}

/// Pre-bn-302v order: the next merge of the sibling silently reverts.
#[test]
fn mutation_pre_bn_302v_ff_ref_before_head_then_merge_reverts() {
    expect_counterexample(
        mutated(
            configs::fast_ff_absorb_crash_then_merge_sibling(),
            Mutation::FfRefBeforeHead,
        ),
        P_NO_SILENT_REVERT,
    );
}

/// Pre-bn-302v `ws merge`: the FF-absorb reconcile (which runs BEFORE
/// PREPARE's journal check) absorbs a trunk commit under a crashed merge's
/// COMMIT journal, so its recovery can never clear it.
#[test]
fn mutation_pre_bn_302v_ff_absorb_ignores_merge_journal_strands_recovery() {
    expect_counterexample(
        mutated(
            configs::fast_ff_absorb_crash_then_merge_sibling(),
            Mutation::FfAbsorbIgnoresMergeJournal,
        ),
        P_RECOVERY_CONVERGES,
    );
}

/// `maw doctor --repair` refuses while a crashed merge's journal exists, so
/// crash recovery always converges; the doctor still advances otherwise.
#[test]
fn fast_doctor_repair_vs_crashed_merge() {
    check_green(configs::doctor_vs_crashed_merge());
}

/// Pre-bn-302v doctor: advancing the epoch under a crashed COMMIT-phase
/// journal strands the merge (recovery can no longer clear it).
#[test]
fn mutation_pre_bn_302v_doctor_ignores_merge_journal_strands_recovery() {
    expect_counterexample(
        mutated(
            configs::doctor_vs_crashed_merge(),
            Mutation::DoctorIgnoresMergeJournal,
        ),
        P_RECOVERY_CONVERGES,
    );
}

// ---------------------------------------------------------------------------
// Deep (manual / nightly)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "deep Stateright check; run via `just formal-check`"]
fn deep_everything() {
    check_green(configs::deep_everything());
}
