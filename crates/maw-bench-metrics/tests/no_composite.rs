// Integration test guarded behind the `bench` feature — the
// crate's entire surface lives under `#![cfg(feature = "bench")]`,
// so a `cargo test --workspace` with default features compiles
// this file to nothing.
#![cfg(feature = "bench")]

//! Binding invariant: the dominance-table renderer NEVER emits a
//! composite score, weighted sum, or aggregate ranking across axes.
//!
//! The pre-reg (§1.2, §4) and the bone (bn-oko4) both make this a
//! hard rule. This test fabricates a multi-arm scenario that would
//! be tempting to summarize (one arm clearly wins efficiency, the
//! other clearly wins correctness) and asserts the renderer does
//! not collapse the comparison.

use std::collections::BTreeMap;

use maw_bench_metrics::{render_dominance_table, MetricRecord, MetricValue, ReportOptions};

fn rec(arm: &str, run_id: &str, lost: u64, turns: u64, calls: u64) -> MetricRecord {
    MetricRecord {
        schema_version: MetricRecord::SCHEMA_VERSION,
        run_id: run_id.into(),
        arm: arm.into(),
        condition_id: "C2".into(),
        t_class: "T3".into(),
        work_lost_events: MetricValue::count(lost),
        human_intervention_events: MetricValue::Unavailable,
        tool_calls_total: MetricValue::count(calls),
        turns_to_done: MetricValue::count(turns),
        wall_duration_ms: MetricValue::duration_ms(1000),
        cost_usd: MetricValue::usd_cents(100),
        work_redone_turns: MetricValue::count(0),
        per_verb_wasted_turns: BTreeMap::new(),
    }
}

#[test]
fn renderer_emits_no_composite_under_mixed_dominance() {
    // maw wins correctness (0 loss vs 2); loses efficiency (10 vs 3 turns).
    let recs = vec![
        rec("maw", "m1", 0, 10, 50),
        rec("maw", "m2", 0, 12, 55),
        rec("jj-workspaces", "j1", 2, 3, 15),
        rec("jj-workspaces", "j2", 2, 4, 18),
    ];
    let out = render_dominance_table(
        &recs,
        &ReportOptions {
            aggregate_median: true,
            arm_order: Some(vec!["maw".into(), "jj-workspaces".into()]),
        },
    );

    // Forbidden tokens for any cross-axis aggregation.
    let lowered = out.to_ascii_lowercase();
    for forbidden in [
        "composite",
        "weighted",
        "winner:",
        "overall:",
        "rank:",
        "ranking",
        "score:",
        "leaderboard",
        " wins ",
        " loses ",
        "total =",
    ] {
        assert!(
            !lowered.contains(forbidden),
            "renderer emitted forbidden cross-axis token {forbidden:?}:\n{out}"
        );
    }

    // Required: BOTH axis captions present, in correct order, in
    // both arm blocks.
    let m_idx = out.find("ARM: maw").expect("maw block");
    let j_idx = out.find("ARM: jj-workspaces").expect("jj block");
    let maw_block = &out[m_idx..j_idx];
    let jj_block = &out[j_idx..];
    for block in [maw_block, jj_block] {
        let corr = block.find("correctness").expect("correctness header");
        let eff = block.find("efficiency").expect("efficiency header");
        assert!(corr < eff, "axes out of order in block:\n{block}");
    }
}

#[test]
fn renderer_makes_axis_labels_unmissable() {
    let recs = vec![rec("maw", "r1", 0, 3, 10)];
    let out = render_dominance_table(&recs, &ReportOptions::default());
    // Both phrases the reader needs to see at every render.
    assert!(out.contains("axes printed SEPARATELY"));
    assert!(out.contains("no cross-axis aggregation"));
}
