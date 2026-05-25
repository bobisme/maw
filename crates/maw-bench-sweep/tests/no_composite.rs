#![cfg(feature = "bench")]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::missing_errors_doc)]

//! Mirrors `maw-bench-metrics::tests::no_composite`: the
//! T2.6 sweep renderer must NEVER emit a composite, weighted
//! score, ranking, or "overall winner". Asserted against the
//! actual rendered output of both `render_spectrum_table` and
//! `render_crossover_doc`.

use maw_bench_metrics::{MetricRecord, MetricValue};
use maw_bench_sweep::aggregate::aggregate_metric_records;
use maw_bench_sweep::{SpectrumReportOptions, render_crossover_doc, render_spectrum_table};

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

fn synth_summary() -> maw_bench_sweep::SweepSummary {
    let mut recs = Vec::new();
    for i in 0..6 {
        recs.push(rec("maw", "C0", "T0", 0, 5, 30 + (i % 2)));
        recs.push(rec("git-worktrees-bare", "C0", "T0", 0, 4, 10 + (i % 2)));
    }
    for i in 0..6 {
        recs.push(rec("maw", "C4", "T0", 0, 5, 30 + (i % 2)));
        recs.push(rec(
            "git-worktrees-bare",
            "C4",
            "T0",
            u64::from(i < 3),
            10,
            100,
        ));
    }
    aggregate_metric_records(&recs)
}

#[test]
fn spectrum_renderer_has_no_composite_tokens() {
    let s = synth_summary();
    let out = render_spectrum_table(&s, &SpectrumReportOptions::default());
    let lowered = out.to_ascii_lowercase();
    for forbidden in [
        "composite",
        "weighted",
        "score:",
        "overall:",
        "winner:",
        "leaderboard",
        "ranking",
        "rank:",
        "total =",
    ] {
        assert!(
            !lowered.contains(forbidden),
            "spectrum renderer contains forbidden token {forbidden:?}:\n{out}"
        );
    }
}

#[test]
fn crossover_doc_has_no_composite_tokens() {
    let s = synth_summary();
    let out = render_crossover_doc(&s, &SpectrumReportOptions::default());
    let lowered = out.to_ascii_lowercase();
    for forbidden in [
        "composite",
        "weighted",
        "score:",
        "overall:",
        "winner:",
        "leaderboard",
    ] {
        assert!(
            !lowered.contains(forbidden),
            "crossover doc contains forbidden token {forbidden:?}:\n{out}"
        );
    }
}

#[test]
fn axes_remain_separated_in_spectrum_renderer() {
    let s = synth_summary();
    let out = render_spectrum_table(&s, &SpectrumReportOptions::default());
    // Same load-bearing header text as the T2.4 renderer.
    assert!(out.contains("axes printed SEPARATELY"));
    assert!(out.contains("no cross-axis aggregation"));
    // Correctness header appears before efficiency header in each
    // cell block.
    let mut search = out.as_str();
    while let Some(corr_idx) = search.find("correctness") {
        let after_corr = &search[corr_idx..];
        let eff_idx = after_corr
            .find("efficiency")
            .expect("efficiency after correctness");
        assert!(eff_idx > 0);
        search = &after_corr[eff_idx + 1..];
    }
}
