#![cfg(feature = "bench")]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::float_cmp)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::missing_errors_doc)]

//! Wilson-CI invariants. Mirrors T1.9 §3.1 zero-event reporting:
//! every "0/N" cell publishes `[0.0, U]`, never a bare zero.

use maw_bench_metrics::{MetricRecord, MetricValue};
use maw_bench_sweep::aggregate::{aggregate_metric_records, wilson_ci, wilson_score_upper};
use maw_bench_sweep::{SpectrumReportOptions, render_spectrum_table};

fn rec(arm: &str, cond: &str, t: &str, lost: u64) -> MetricRecord {
    MetricRecord {
        schema_version: 1,
        run_id: format!("{arm}-{cond}-{t}-{lost}"),
        arm: arm.into(),
        condition_id: cond.into(),
        t_class: t.into(),
        work_lost_events: MetricValue::count(lost),
        human_intervention_events: MetricValue::Unavailable,
        tool_calls_total: MetricValue::count(10),
        turns_to_done: MetricValue::count(5),
        wall_duration_ms: MetricValue::duration_ms(1000),
        cost_usd: MetricValue::usd_cents(100),
        work_redone_turns: MetricValue::count(0),
        per_verb_wasted_turns: std::collections::BTreeMap::new(),
    }
}

#[test]
fn zero_event_cell_renders_lower_zero_and_nontrivial_upper() {
    // 20 maw runs, 0 work_lost events.
    let recs: Vec<_> = (0..20).map(|_| rec("maw", "C0", "T0", 0)).collect();
    let s = aggregate_metric_records(&recs);
    let table = render_spectrum_table(&s, &SpectrumReportOptions::default());
    // Mirrors T1.9 §3.1: the publication-facing string is "[0.000, U]".
    assert!(table.contains("[0.000,"));
    // The standard N=20 upper bound is ~0.161.
    let cell = s.cell("maw", "C0", "T0").unwrap();
    assert_eq!(cell.work_lost_rate_ci.k, 0);
    assert_eq!(cell.work_lost_rate_ci.n, 20);
    assert!(cell.work_lost_rate_ci.lower == 0.0);
    assert!(cell.work_lost_rate_ci.upper > 0.10);
    assert!(cell.work_lost_rate_ci.upper < 0.25);
}

#[test]
fn wilson_upper_table_matches_pre_reg_at_canonical_sample_sizes() {
    // Spot-check the pre-reg §6.1 MDE table.
    let cases = [(10_u64, 0.278), (20, 0.161), (50, 0.071), (100, 0.037)];
    for (n, expected) in cases {
        let u = wilson_score_upper(n);
        assert!(
            (u - expected).abs() < 0.01,
            "Wilson UB at N={n}: got {u}, expected ~{expected}"
        );
    }
}

#[test]
fn wilson_at_n_zero_is_no_information() {
    let w = wilson_ci(0, 0);
    assert_eq!(w.lower, 0.0);
    assert_eq!(w.upper, 1.0);
}
