#![cfg(feature = "bench")]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::float_cmp)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::doc_markdown)]

//! Synthetic-SweepSummary tests for [`find_crossover`].
//!
//! Mirrors the bone's "Crossover identification" acceptance test:
//! plant a known crossover into a SweepSummary, assert the finder
//! returns it.

use maw_bench_metrics::{MetricRecord, MetricValue};
use maw_bench_sweep::aggregate::aggregate_metric_records;
use maw_bench_sweep::crossover::{
    CrossoverRegime, MATERIALITY_RATE_GAP, MetricName, find_crossover,
};

fn rec(arm: &str, cond: &str, t: &str, lost: u64, turns: u64, calls: u64) -> MetricRecord {
    MetricRecord {
        schema_version: 1,
        run_id: format!("{arm}-{cond}-{t}-{turns}-{calls}-{lost}"),
        arm: arm.into(),
        condition_id: cond.into(),
        t_class: t.into(),
        work_lost_events: MetricValue::count(lost),
        human_intervention_events: MetricValue::Unavailable,
        tool_calls_total: MetricValue::count(calls),
        turns_to_done: MetricValue::count(turns),
        wall_duration_ms: MetricValue::duration_ms(1000),
        cost_usd: MetricValue::usd_cents(100),
        work_redone_turns: MetricValue::count(0),
        per_verb_wasted_turns: std::collections::BTreeMap::new(),
    }
}

#[test]
fn planted_overkill_at_c0_and_dominance_at_c4_is_found() {
    let mut records = Vec::new();
    // C0: maw 30 tool_calls vs git-worktrees-bare 10 -> Overkill.
    for i in 0..10 {
        records.push(rec("maw", "C0", "T0", 0, 5, 30 + (i % 3)));
        records.push(rec("git-worktrees-bare", "C0", "T0", 0, 4, 10 + (i % 3)));
    }
    // C4: maw clean; worktrees has 60% work_lost rate -> rate Dominant.
    for i in 0..10 {
        records.push(rec("maw", "C4", "T0", 0, 5, 30 + (i % 3)));
        records.push(rec(
            "git-worktrees-bare",
            "C4",
            "T0",
            u64::from(i >= 4),
            8,
            100,
        ));
    }
    let s = aggregate_metric_records(&records);
    let calls = find_crossover(&s, MetricName::ToolCallsTotal, "maw");
    let work = find_crossover(&s, MetricName::WorkLostRate, "maw");

    let c0_calls = calls.iter().find(|c| c.condition_id == "C0").unwrap();
    assert_eq!(c0_calls.regime, CrossoverRegime::Overkill);
    let c4_work = work.iter().find(|c| c.condition_id == "C4").unwrap();
    assert_eq!(c4_work.regime, CrossoverRegime::Dominant);
}

#[test]
fn band_of_mixed_verdicts_is_reported_per_cell_not_collapsed() {
    // §4.3 R7: a band of MIXED is reported as the crossover
    // band-with-width, never collapsed to a point. Here we plant
    // two adjacent cells with different verdicts and assert the
    // finder returns BOTH points (the caller renders the band).
    let mut records = Vec::new();
    for _ in 0..6 {
        records.push(rec("maw", "C1", "T0", 0, 6, 18)); // overkill ish (1.5x)
        records.push(rec("git-worktrees-bare", "C1", "T0", 0, 5, 12));
    }
    for _ in 0..6 {
        records.push(rec("maw", "C2", "T0", 0, 6, 12)); // tie (1x)
        records.push(rec("git-worktrees-bare", "C2", "T0", 0, 5, 12));
    }
    let s = aggregate_metric_records(&records);
    let cps = find_crossover(&s, MetricName::ToolCallsTotal, "maw");
    let conds: std::collections::BTreeSet<_> = cps.iter().map(|c| c.condition_id.clone()).collect();
    assert!(conds.contains("C1"));
    assert!(conds.contains("C2"));
    // Different regimes — verifies the band is "wide" not collapsed.
    let c1 = cps.iter().find(|c| c.condition_id == "C1").unwrap();
    let c2 = cps.iter().find(|c| c.condition_id == "C2").unwrap();
    assert_ne!(c1.regime, c2.regime);
}

#[test]
fn rate_dominance_threshold_matches_pre_reg_010_gap() {
    // Just over the materiality gap should fire Dominant.
    let mut records = Vec::new();
    for i in 0..10 {
        records.push(rec("maw", "C4", "T0", 0, 5, 30));
        records.push(rec("jj-workspaces", "C4", "T0", u64::from(i >= 8), 5, 30));
    }
    let s = aggregate_metric_records(&records);
    let cps = find_crossover(&s, MetricName::WorkLostRate, "maw");
    let cp = cps.iter().find(|c| c.other_arm == "jj-workspaces").unwrap();
    // gap = 0.2 - 0.0 = 0.2 > 0.10 -> Dominant.
    assert_eq!(cp.regime, CrossoverRegime::Dominant);
    assert!(cp.statistic.unwrap() > MATERIALITY_RATE_GAP);
}
