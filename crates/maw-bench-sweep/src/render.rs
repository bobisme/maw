//! Spectrum-mode renderer + crossover-doc generator.
//!
//! # Two outputs
//!
//! - [`render_spectrum_table`] — ASCII summary suitable for stdout
//!   in the pilot. One block per (condition, t_class) cell with
//!   the per-arm median + Wilson CI on the zero-event rate (the
//!   §4.1 cell shape, no composite, axes-separated header).
//! - [`render_crossover_doc`] — a Markdown doc with the
//!   publishable narrative scaffolding. Explicit
//!   `OVERKILL_REGIME` and `HOSTILE_REGIME` headers; the
//!   "overkill" cells are listed verbatim (no clipping) per the
//!   §2 binding.
//!
//! # No composite, ever
//!
//! Both renderers re-implement the no-composite invariants that
//! `notes/sg2-metric-definitions.md` §Renderer invariants
//! defines. The local `no_composite.rs` test scans the output of
//! both renderers for the forbidden tokens (composite, weighted,
//! winner, ranking, overall, ...).

use std::fmt::Write as _;

use crate::aggregate::{CellAggregate, SweepSummary};
use crate::crossover::{CrossoverPoint, CrossoverRegime, MetricName};

/// Spectrum-mode renderer options.
#[derive(Clone, Debug)]
pub struct SpectrumReportOptions {
    /// Fixed arm ordering; falls back to summary's observed order
    /// when `None`. Pilot + publication callers set this to
    /// [`crate::ARMS_PUBLICATION`].
    pub arm_order: Option<Vec<String>>,
    /// Reference arm for the crossover lines (typically `"maw"`).
    pub ref_arm: String,
    /// Metrics included in the crossover lines, in display order.
    pub metrics: Vec<MetricName>,
}

impl Default for SpectrumReportOptions {
    fn default() -> Self {
        Self {
            arm_order: None,
            ref_arm: "maw".to_string(),
            metrics: vec![
                MetricName::WorkLostRate,
                MetricName::ToolCallsTotal,
                MetricName::TurnsToDone,
                MetricName::WorkRedoneTurns,
            ],
        }
    }
}

/// Render a spectrum-mode ASCII summary of `summary`. The output
/// is plain text suitable for stdout / a CI log.
#[must_use]
pub fn render_spectrum_table(summary: &SweepSummary, opts: &SpectrumReportOptions) -> String {
    let mut out = String::new();
    if summary.cells.is_empty() {
        out.push_str("(no cells)\n");
        return out;
    }

    // Load-bearing header (axes-separated reminder; mirrors T2.4
    // renderer so a screenshot of the table cannot strip the rule).
    let _ = writeln!(
        out,
        "SG2 spectrum-mode summary  (axes printed SEPARATELY; no cross-axis aggregation)"
    );
    let _ = writeln!(
        out,
        "bone=bn-3l1f   pre-reg=§5+§4.1   ref_arm={}",
        opts.ref_arm
    );
    out.push('\n');

    let arms = arm_order(summary, opts);

    // One block per (condition_id, t_class) cell, in spectrum
    // order (C0..C4, T0..T5 by ascii sort which gives the right
    // ordering).
    for cond in &summary.conditions {
        for t in &summary.t_classes {
            let any_data = arms.iter().any(|a| summary.cell(a, cond, t).is_some());
            if !any_data {
                continue;
            }
            render_cell_block(&mut out, cond, t, &arms, summary);
        }
    }

    // Crossover summary lines per metric (the publishable headline
    // pattern, table form). Each line shows the ref_arm vs each
    // other arm classification at this condition × t_class for
    // that metric.
    out.push('\n');
    let _ = writeln!(
        out,
        "--- per-(metric × condition) crossover (ref_arm = {}) ---",
        opts.ref_arm
    );
    for metric in &opts.metrics {
        let cps = crate::find_crossover(summary, *metric, &opts.ref_arm);
        if cps.is_empty() {
            continue;
        }
        let _ = writeln!(out, "  metric: {}", metric.as_str());
        for cp in &cps {
            let stat = fmt_stat(cp.statistic);
            let _ = writeln!(
                out,
                "    {cond} {t}  vs {oth:<26}  regime={reg:<9}  stat={stat:<10}  N(ref/oth)={nr}/{no}",
                cond = cp.condition_id,
                t = cp.t_class,
                oth = cp.other_arm,
                reg = cp.regime.as_str(),
                nr = cp.n_ref,
                no = cp.n_other,
            );
        }
    }

    out
}

fn render_cell_block(
    out: &mut String,
    cond_id: &str,
    t_class: &str,
    arms: &[String],
    summary: &SweepSummary,
) {
    let _ = writeln!(out, "CELL: {cond_id} × {t_class}");
    // Axis 1 — correctness first (load-bearing per §4.1).
    let _ = writeln!(out, "  --- correctness (higher-is-worse; 0 is the bar) ---");
    for arm in arms {
        let Some(c) = summary.cell(arm, cond_id, t_class) else {
            continue;
        };
        let _ = writeln!(
            out,
            "    {arm:<26}  N={n:<3}  work_lost_rate {ci}",
            n = c.n,
            ci = c.work_lost_rate_ci.format(),
        );
    }
    // Axis 2 — efficiency.
    let _ = writeln!(out, "  --- efficiency (lower-is-better; NOT safety) ---");
    for metric in [
        "tool_calls_total",
        "turns_to_done",
        "work_redone_turns",
        "cost_usd",
    ] {
        let _ = writeln!(out, "    {metric}");
        for arm in arms {
            let Some(c) = summary.cell(arm, cond_id, t_class) else {
                continue;
            };
            let lo = fmt_metric(c, metric, "min");
            let med = fmt_metric(c, metric, "median");
            let hi = fmt_metric(c, metric, "max");
            let _ = writeln!(out, "      {arm:<24}  med={med:<10}  range=({lo}..{hi})",);
        }
    }
    out.push('\n');
}

fn fmt_metric(c: &CellAggregate, name: &str, which: &str) -> String {
    let m = match which {
        "min" => c.min.get(name),
        "max" => c.max.get(name),
        _ => c.median.get(name),
    };
    m.map_or_else(|| "n/a".to_string(), |v| v.format())
}

/// Pretty-print a crossover statistic. Treats values near the
/// `f64::MAX/2` `Infinite` sentinel from [`crate::crossover::find_crossover`]
/// as `INF` so the rendered output stays readable when an arm's
/// median was `Infinite` (e.g. agent never finished). Treats the
/// reciprocal — `0.0` — as `0` so a Dominant verdict with a
/// vanishing ratio reads cleanly. Pure formatting; no semantic
/// change.
fn fmt_stat(stat: Option<f64>) -> String {
    match stat {
        None => "n/a".to_string(),
        Some(s) if !s.is_finite() => "INF".to_string(),
        Some(s) if s.abs() > 1e30 => "INF".to_string(),
        Some(s) => format!("{s:.3}"),
    }
}

fn arm_order(summary: &SweepSummary, opts: &SpectrumReportOptions) -> Vec<String> {
    match &opts.arm_order {
        Some(order) => {
            let mut seen: Vec<String> = Vec::new();
            for name in order {
                if summary.arms.contains(name) {
                    seen.push(name.clone());
                }
            }
            for name in &summary.arms {
                if !seen.contains(name) {
                    seen.push(name.clone());
                }
            }
            seen
        }
        None => summary.arms.clone(),
    }
}

/// Render the publishable `crossover-summary.md` doc. Has two
/// explicit, load-bearing sections:
///
/// - `## OVERKILL_REGIME` — every cell × metric where the
///   reference arm is materially worse, listed verbatim. The §2
///   binding requires this section to ship unclipped.
/// - `## HOSTILE_REGIME` — every cell × metric where the
///   reference arm dominates (typically the high-coordination
///   conditions where the alternatives lose/wedge).
///
/// `## TIE_REGIME` and `## NO_DATA` sections are emitted too so a
/// reader can see the full classification surface.
///
/// The doc's narrative cells are scaffolding only — the real-run
/// artifact dir will fill them in with measured numbers. The
/// shape is fixed here so the publication shape is review-able
/// before the campaign starts.
#[must_use]
pub fn render_crossover_doc(summary: &SweepSummary, opts: &SpectrumReportOptions) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# SG2 Crossover Summary (scaffold; reference arm = {})",
        opts.ref_arm
    );
    out.push('\n');
    let _ = writeln!(
        out,
        "_Pre-reg: `notes/sg2-benchmark-preregistration.md` §2 (publish-the-loss-regime), §4.2 (crossover figure), §5 (frozen spectrum). Bone: bn-3l1f / T2.6._"
    );
    out.push('\n');
    let _ = writeln!(
        out,
        "**Binding format reminders (this scaffolding preserves them verbatim):**"
    );
    let _ = writeln!(
        out,
        "- Correctness axis is reported SEPARATELY from efficiency. Axes are not combined; there is no cross-axis aggregation."
    );
    let _ = writeln!(
        out,
        "- Zero-event proportion cells publish their Wilson 95% upper bound, NOT a bare zero (§6.1)."
    );
    let _ = writeln!(
        out,
        "- The overkill regime (where {} loses on an efficiency metric) is shipped, never clipped (§2).",
        opts.ref_arm
    );
    out.push('\n');

    // Bucket crossover points by regime per metric, keeping the
    // correctness axis separate from the efficiency axis. The pre-
    // reg §4.3 OVERKILL definition is "tie-on-correctness +
    // materially-worse-on-efficiency"; conflating the two would
    // misread a safety regression as an "overkill cost" in the
    // doc.
    let mut overkill_eff: Vec<CrossoverPoint> = Vec::new();
    let mut hostile_eff: Vec<CrossoverPoint> = Vec::new();
    let mut safety_inferior: Vec<CrossoverPoint> = Vec::new();
    let mut safety_superior: Vec<CrossoverPoint> = Vec::new();
    let mut tie: Vec<CrossoverPoint> = Vec::new();
    let mut no_data: Vec<CrossoverPoint> = Vec::new();

    for metric in &opts.metrics {
        let is_rate = metric.is_rate();
        for cp in crate::find_crossover(summary, *metric, &opts.ref_arm) {
            match cp.regime {
                CrossoverRegime::Overkill if is_rate => safety_inferior.push(cp),
                CrossoverRegime::Overkill => overkill_eff.push(cp),
                CrossoverRegime::Dominant if is_rate => safety_superior.push(cp),
                CrossoverRegime::Dominant => hostile_eff.push(cp),
                CrossoverRegime::Tie => tie.push(cp),
                CrossoverRegime::NoData => no_data.push(cp),
            }
        }
    }

    let _ = writeln!(out, "## OVERKILL_REGIME");
    out.push('\n');
    let _ = writeln!(
        out,
        "_Efficiency-axis cells where `{}` is materially worse than the comparison arm. This is the publishable benign-end regime; do NOT clip it._",
        opts.ref_arm
    );
    out.push('\n');
    write_crossover_table(&mut out, &overkill_eff);
    out.push('\n');
    let _ = writeln!(
        out,
        "**Narrative scaffold (fill from real-run artifacts):**"
    );
    let _ = writeln!(
        out,
        "- _On low-coordination conditions ({{C0, C1}}), {} uses more tool calls / turns than the comparison arm. Margin: {{ratio range from the table above}}. This is expected and pre-registered._",
        opts.ref_arm
    );
    let _ = writeln!(
        out,
        "- _Recommendation (for the publication): \"Do not use {} below the {{C?}} condition class.\"_",
        opts.ref_arm
    );
    out.push('\n');

    let _ = writeln!(out, "## HOSTILE_REGIME");
    out.push('\n');
    let _ = writeln!(
        out,
        "_Efficiency-axis cells where `{}` materially dominates the comparison arm (typically high coordination + concurrency). This is where alternatives lose / wedge on efficiency too._",
        opts.ref_arm
    );
    out.push('\n');
    write_crossover_table(&mut out, &hostile_eff);
    out.push('\n');

    let _ = writeln!(out, "## SAFETY (correctness-axis classification)");
    out.push('\n');
    let _ = writeln!(
        out,
        "_The correctness axis is reported **separately** per the pre-reg §4.1 binding format. Two sub-tables: cells where `{}` has a materially HIGHER work-lost rate (a safety regression — must be investigated), and cells where it has a materially LOWER rate (the substrate-coordination dividend)._",
        opts.ref_arm
    );
    out.push('\n');
    let _ = writeln!(
        out,
        "### Cells where `{}` rate is materially HIGHER (ref WORSE on safety)",
        opts.ref_arm
    );
    out.push('\n');
    write_crossover_table(&mut out, &safety_inferior);
    out.push('\n');
    let _ = writeln!(
        out,
        "### Cells where `{}` rate is materially LOWER (ref BETTER on safety)",
        opts.ref_arm
    );
    out.push('\n');
    write_crossover_table(&mut out, &safety_superior);
    out.push('\n');
    let _ = writeln!(out, "**Narrative scaffold:**");
    let _ = writeln!(
        out,
        "- _On C3/C4, the comparison arm experiences {{wedge / lost-work / agent-abandon}} events at rate {{Wilson CI}}; {} stays at 0 with Wilson 95% UB ≤ {{table}}._",
        opts.ref_arm
    );
    let _ = writeln!(
        out,
        "- _The crossover from OVERKILL to HOSTILE occurs in {{C1..C3}} band; the band's width is reported per pre-reg §4.3 (never collapsed to a point if MIXED)._"
    );
    out.push('\n');

    let _ = writeln!(out, "## TIE_REGIME");
    out.push('\n');
    let _ = writeln!(
        out,
        "_Cells within the pre-registered materiality margin (×{:.2} ratio for efficiency; ±{:.2} gap for rates)._",
        crate::crossover::MATERIALITY_RATIO,
        crate::crossover::MATERIALITY_RATE_GAP
    );
    out.push('\n');
    write_crossover_table(&mut out, &tie);
    out.push('\n');

    let _ = writeln!(out, "## NO_DATA");
    out.push('\n');
    let _ = writeln!(
        out,
        "_Cells where the metric was not measurable (e.g. `MockAgent` cost is `Unavailable`)._"
    );
    out.push('\n');
    write_crossover_table(&mut out, &no_data);
    out.push('\n');

    let _ = writeln!(out, "## Confidence statement");
    out.push('\n');
    let _ = writeln!(
        out,
        "_Per-cell Wilson 95% CIs on the `work_lost_rate` proportion are carried in the spectrum table above. At pre-reg N=10 the conservative upper bound on a zero-event cell is ~0.278; at N=20 it is ~0.161 (§6.1)._"
    );

    out
}

fn write_crossover_table(out: &mut String, cps: &[CrossoverPoint]) {
    if cps.is_empty() {
        let _ = writeln!(out, "_(no cells in this regime)_");
        return;
    }
    let _ = writeln!(
        out,
        "| metric | condition | t_class | other arm | regime | statistic | N(ref / other) |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|");
    for cp in cps {
        let stat = fmt_stat(cp.statistic);
        let _ = writeln!(
            out,
            "| {m} | {c} | {t} | {o} | {r} | {s} | {nr} / {no} |",
            m = cp.metric.as_str(),
            c = cp.condition_id,
            t = cp.t_class,
            o = cp.other_arm,
            r = cp.regime.as_str(),
            s = stat,
            nr = cp.n_ref,
            no = cp.n_other,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregate::aggregate_metric_records;
    use maw_bench_metrics::{MetricRecord, MetricValue};

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

    fn planted_summary() -> SweepSummary {
        let mut records = Vec::new();
        // C0 — maw overkill on tool_calls.
        for i in 0..6 {
            records.push(rec("maw", "C0", "T0", 0, 5, 30 + (i % 2)));
            records.push(rec("git-worktrees-bare", "C0", "T0", 0, 4, 10 + (i % 2)));
        }
        // C4 — worktrees has wedge events; maw clean.
        for i in 0..6 {
            records.push(rec("maw", "C4", "T0", 0, 5, 30 + (i % 2)));
            records.push(rec(
                "git-worktrees-bare",
                "C4",
                "T0",
                u64::from(i < 3),
                10,
                100,
            ));
        }
        aggregate_metric_records(&records)
    }

    #[test]
    fn spectrum_table_includes_overkill_cell_label() {
        let s = planted_summary();
        let out = render_spectrum_table(&s, &SpectrumReportOptions::default());
        // C0 cell is rendered (the overkill cell — not clipped).
        assert!(out.contains("CELL: C0"), "C0 cell missing:\n{out}");
        // Axis-order: correctness before efficiency.
        let corr = out.find("correctness").expect("correctness");
        let eff = out.find("efficiency").expect("efficiency");
        assert!(corr < eff);
    }

    #[test]
    fn crossover_doc_has_overkill_and_hostile_sections() {
        let s = planted_summary();
        let doc = render_crossover_doc(&s, &SpectrumReportOptions::default());
        // The two load-bearing section headers, exactly.
        assert!(
            doc.contains("## OVERKILL_REGIME"),
            "missing OVERKILL_REGIME:\n{doc}"
        );
        assert!(
            doc.contains("## HOSTILE_REGIME"),
            "missing HOSTILE_REGIME:\n{doc}"
        );
        // The overkill regime narrative scaffold mentions the
        // "do not use" guidance.
        assert!(doc.to_ascii_lowercase().contains("do not use"));
    }

    #[test]
    fn spectrum_table_has_no_composite_tokens() {
        let s = planted_summary();
        let out = render_spectrum_table(&s, &SpectrumReportOptions::default());
        let lowered = out.to_ascii_lowercase();
        for forbidden in [
            "composite",
            "weighted",
            "winner:",
            "overall:",
            "rank:",
            "ranking",
            "leaderboard",
            "score:",
            "total =",
        ] {
            assert!(
                !lowered.contains(forbidden),
                "spectrum table contains forbidden token {forbidden:?}:\n{out}"
            );
        }
    }

    #[test]
    fn crossover_doc_has_no_composite_tokens() {
        let s = planted_summary();
        let out = render_crossover_doc(&s, &SpectrumReportOptions::default());
        let lowered = out.to_ascii_lowercase();
        for forbidden in [
            "composite",
            "weighted",
            "winner:",
            "overall:",
            "leaderboard",
            "score:",
        ] {
            assert!(
                !lowered.contains(forbidden),
                "crossover doc contains forbidden token {forbidden:?}:\n{out}"
            );
        }
    }

    #[test]
    fn spectrum_table_publishes_wilson_ci_for_zero_event_cells() {
        let s = planted_summary();
        let out = render_spectrum_table(&s, &SpectrumReportOptions::default());
        // C0 / maw should have a Wilson CI like "0.000 [0.000, 0.???]".
        assert!(
            out.contains("[0.000,"),
            "wilson lower bound missing:\n{out}"
        );
    }
}
