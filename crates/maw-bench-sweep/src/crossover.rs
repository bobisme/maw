//! Crossover analysis — per-(metric, ref_arm) curves over the
//! condition spectrum.
//!
//! # What the crossover function returns
//!
//! Given a [`crate::SweepSummary`], a target metric, and a
//! reference arm (typically `"maw"`), [`find_crossover`] returns a
//! [`CrossoverPoint`] per **other** arm × condition that classifies
//! the relationship at that condition:
//!
//! - [`CrossoverRegime::Overkill`] — the reference arm is
//!   materially worse on this metric (e.g. maw uses more
//!   tool-calls than worktrees on a benign cell).
//! - [`CrossoverRegime::Tie`] — within the pre-registered
//!   materiality margin.
//! - [`CrossoverRegime::Dominant`] — the reference arm is
//!   materially better on this metric (the other arm
//!   loses/wedges, e.g. jj on the hostile cell).
//!
//! The renderer collapses this point sequence into the publishable
//! `OVERKILL_REGIME` / `HOSTILE_REGIME` headers; the raw
//! [`CrossoverPoint`] vector is **never** averaged into a single
//! "maw wins at condition X" number — per-metric, per-arm
//! reporting is the rule.
//!
//! # Materiality margin (§4.3 / R1 frozen)
//!
//! Efficiency comparisons use **median-ratio × 1.15** as the
//! pre-registered materiality threshold. A ratio in `[1/1.15,
//! 1.15]` is a tie; outside is material. This is the same number
//! the SG3 layout gate uses, anchored to ~1.5 × the SP3 9.5%
//! turns-CV. We deliberately use the median ratio rather than a
//! paired bootstrap CI here because the bootstrap requires
//! per-replicate pairing data the summary does not carry (the
//! summary already collapsed to per-cell median). T2.8 (the
//! diagnostic bundle) gets the per-replicate data and can compute
//! the bootstrap CI; the crossover view here is the median-ratio
//! approximation suitable for the headline visualization.
//!
//! Rate comparisons use the §4.3 rate material-gap rule: either
//! the Wilson 95% intervals separate in maw's favor OR the point
//! estimate gap exceeds **+0.10**. We compute the rate gap here
//! and label it the same way as the efficiency materiality.

use serde::{Deserialize, Serialize};

use maw_bench_metrics::MetricValue;

use crate::aggregate::{CellAggregate, SweepSummary};

/// Materiality factor for median-ratio comparisons.
/// pre-reg §4.3 R1 — `×1.15`.
pub const MATERIALITY_RATIO: f64 = 1.15;

/// Materiality absolute gap for binary-rate comparisons.
/// pre-reg §4.3 — `+0.10`.
pub const MATERIALITY_RATE_GAP: f64 = 0.10;

/// Named metrics the crossover analysis supports. We use a small
/// enum (not a string) so callers cannot typo a metric name into
/// silent "no crossover here". Adding a metric requires touching
/// this enum + [`metric_kind`] + the renderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricName {
    /// `tool_calls_total` — efficiency, lower-is-better.
    ToolCallsTotal,
    /// `turns_to_done` — efficiency, lower-is-better.
    TurnsToDone,
    /// `cost_usd` — efficiency, lower-is-better. Crossover only
    /// makes sense when both arms have a non-zero, non-Unavailable
    /// cost; mocked arms collapse to "Tie (no data)".
    CostUsd,
    /// `work_lost_events` — correctness, **higher-is-worse** rate.
    /// Crossover uses the rate, not the count.
    WorkLostRate,
    /// `work_redone_turns` — efficiency-adjacent. Same direction
    /// rules as `turns_to_done` (lower-is-better median ratio).
    WorkRedoneTurns,
}

impl MetricName {
    /// Stable string form for renderers / serialized output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToolCallsTotal => "tool_calls_total",
            Self::TurnsToDone => "turns_to_done",
            Self::CostUsd => "cost_usd",
            Self::WorkLostRate => "work_lost_rate",
            Self::WorkRedoneTurns => "work_redone_turns",
        }
    }

    /// True for rate-style metrics (correctness proportion). False
    /// for efficiency medians.
    #[must_use]
    pub const fn is_rate(self) -> bool {
        matches!(self, Self::WorkLostRate)
    }
}

/// Where, on a per-condition comparison, the reference arm sits
/// relative to another arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrossoverRegime {
    /// Reference arm (e.g. maw) is materially WORSE on this metric
    /// at this condition — the publishable overkill regime when
    /// the metric is efficiency.
    Overkill,
    /// Within the pre-registered materiality margin.
    Tie,
    /// Reference arm is materially BETTER on this metric — the
    /// publishable headline when the metric is correctness-axis
    /// or the efficiency lead is real.
    Dominant,
    /// Insufficient or missing data (e.g. cost is `Unavailable` on
    /// MockAgent runs). The point is still emitted (no clipping)
    /// so the renderer can render a "(no data)" cell rather than
    /// a hole the reader has to chase.
    NoData,
}

impl CrossoverRegime {
    /// Stable string form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Overkill => "overkill",
            Self::Tie => "tie",
            Self::Dominant => "dominant",
            Self::NoData => "no_data",
        }
    }
}

/// One row in the crossover output. Conceptually a per-(arm,
/// condition, t_class) classification for a single metric.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CrossoverPoint {
    /// The metric this point classifies.
    pub metric: MetricName,
    /// The reference arm (typically `"maw"`).
    pub ref_arm: String,
    /// The arm the reference is being compared against.
    pub other_arm: String,
    /// `C0..C4` of the cell.
    pub condition_id: String,
    /// `T0..T5` of the cell.
    pub t_class: String,
    /// The verdict.
    pub regime: CrossoverRegime,
    /// `median(ref) / median(other)` for efficiency metrics;
    /// `rate(other) − rate(ref)` for rate metrics. `None` when
    /// data is missing (regime = NoData). Carrying the raw
    /// statistic lets the renderer present a sortable scale next
    /// to the regime label.
    pub statistic: Option<f64>,
    /// Per-cell N for context. The renderer uses this for the
    /// Wilson-CI-sized confidence statement.
    pub n_ref: u64,
    /// N for the other arm at this cell.
    pub n_other: u64,
}

/// Compute per-arm × per-condition × per-t_class crossover points
/// for the given metric.
///
/// The function iterates every cell in `summary`, identifies pairs
/// `(ref_arm, other_arm)` at the same (condition, t_class) cell,
/// classifies the relationship by [`CrossoverRegime`], and returns
/// one [`CrossoverPoint`] per pair.
///
/// **The overkill regime is included** — never clipped. That is
/// the §2 publish-the-loss-regime commitment.
///
/// Cells where the other arm has no data at the cell are skipped
/// silently (no spurious "NoData" record for a non-existent pair);
/// cells where the metric is `Unavailable` for either arm at this
/// cell produce a `NoData` record so the rendered table makes the
/// gap visible.
#[must_use]
pub fn find_crossover(
    summary: &SweepSummary,
    metric: MetricName,
    ref_arm: &str,
) -> Vec<CrossoverPoint> {
    let mut out = Vec::new();
    // For each (condition, t_class), look up the ref cell, then
    // iterate every other arm.
    for cond_id in &summary.conditions {
        for t in &summary.t_classes {
            let Some(ref_cell) = summary.cell(ref_arm, cond_id, t) else {
                continue;
            };
            for other in &summary.arms {
                if other == ref_arm {
                    continue;
                }
                let Some(other_cell) = summary.cell(other, cond_id, t) else {
                    continue;
                };
                let (regime, stat) =
                    classify(metric, ref_cell, other_cell);
                out.push(CrossoverPoint {
                    metric,
                    ref_arm: ref_arm.to_string(),
                    other_arm: other.clone(),
                    condition_id: cond_id.clone(),
                    t_class: t.clone(),
                    regime,
                    statistic: stat,
                    n_ref: ref_cell.n,
                    n_other: other_cell.n,
                });
            }
        }
    }
    out
}

/// Classification primitive. Returns `(regime, statistic)`.
fn classify(
    metric: MetricName,
    ref_cell: &CellAggregate,
    other_cell: &CellAggregate,
) -> (CrossoverRegime, Option<f64>) {
    if metric.is_rate() {
        // Rate metric — use Wilson CI separation OR point-estimate
        // gap of +0.10.
        let r_ref = ref_cell.work_lost_rate_ci;
        let r_oth = other_cell.work_lost_rate_ci;
        if r_ref.n == 0 || r_oth.n == 0 {
            return (CrossoverRegime::NoData, None);
        }
        let gap = r_oth.point_estimate - r_ref.point_estimate;
        // Dominant: ref's wilson interval is strictly below other's;
        // OR the gap exceeds the material margin.
        let wilson_separated_dominant = r_ref.upper < r_oth.lower;
        let wilson_separated_overkill = r_oth.upper < r_ref.lower;
        let regime = if wilson_separated_dominant || gap > MATERIALITY_RATE_GAP {
            CrossoverRegime::Dominant
        } else if wilson_separated_overkill || -gap > MATERIALITY_RATE_GAP {
            CrossoverRegime::Overkill
        } else {
            CrossoverRegime::Tie
        };
        return (regime, Some(gap));
    }

    // Efficiency metric — median ratio with materiality factor.
    let metric_name = metric.as_str();
    let ref_med = numeric(ref_cell.median.get(metric_name).copied());
    let other_med = numeric(other_cell.median.get(metric_name).copied());
    match (ref_med, other_med) {
        (Some(r), Some(o)) if o > 0.0 => {
            let ratio = r / o;
            let regime = if ratio > MATERIALITY_RATIO {
                // ref is materially LARGER (worse for lower-is-better
                // efficiency metrics) → overkill.
                CrossoverRegime::Overkill
            } else if ratio < 1.0 / MATERIALITY_RATIO {
                CrossoverRegime::Dominant
            } else {
                CrossoverRegime::Tie
            };
            (regime, Some(ratio))
        }
        // Both finite, other median 0 — treat as Tie if ref is 0,
        // Overkill if ref > 0.
        (Some(r), Some(_)) => {
            let regime = if r > 0.0 {
                CrossoverRegime::Overkill
            } else {
                CrossoverRegime::Tie
            };
            (regime, None)
        }
        _ => (CrossoverRegime::NoData, None),
    }
}

/// Turn a `MetricValue` into a finite f64 for ratio math.
/// `Infinite` becomes a sentinel large number so a never-finished
/// arm sorts as the worst; `Unavailable` is `None`.
#[allow(clippy::cast_precision_loss)]
fn numeric(v: Option<MetricValue>) -> Option<f64> {
    match v? {
        MetricValue::Count { n } => Some(n as f64),
        MetricValue::DurationMs { ms } => Some(ms as f64),
        MetricValue::UsdCents { cents } => Some(cents as f64),
        MetricValue::Infinite => Some(f64::MAX / 2.0),
        MetricValue::Unavailable => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregate::aggregate_metric_records;
    use maw_bench_metrics::MetricRecord;

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

    /// A planted crossover: maw uses ~30 tool_calls vs worktrees
    /// uses ~10 at C0 (benign — maw OVERKILL), but at C4 maw uses
    /// ~30 and worktrees uses ~100 (hostile — worktrees worse).
    fn planted_summary() -> crate::SweepSummary {
        let mut records = Vec::new();
        // C0 — maw is overkill (30 vs 10).
        for i in 0..10 {
            records.push(rec("maw", "C0", "T0", 0, 5, 30 + (i % 3)));
            records.push(rec("git-worktrees-bare", "C0", "T0", 0, 4, 10 + (i % 3)));
        }
        // C4 — worktrees worse (100 vs 30).
        for i in 0..10 {
            records.push(rec("maw", "C4", "T0", 0, 5, 30 + (i % 3)));
            records.push(rec("git-worktrees-bare", "C4", "T0", 0, 4, 100 + (i % 3)));
        }
        aggregate_metric_records(&records)
    }

    #[test]
    fn crossover_identifies_overkill_and_dominant_regimes_on_planted_data() {
        let s = planted_summary();
        let cps = find_crossover(&s, MetricName::ToolCallsTotal, "maw");
        // Two arms × 2 conditions × 1 t_class = 2 points (only
        // git-worktrees-bare is the other arm).
        assert_eq!(cps.len(), 2);
        let c0 = cps.iter().find(|c| c.condition_id == "C0").unwrap();
        assert_eq!(c0.regime, CrossoverRegime::Overkill);
        let c4 = cps.iter().find(|c| c.condition_id == "C4").unwrap();
        assert_eq!(c4.regime, CrossoverRegime::Dominant);
    }

    #[test]
    fn tie_when_ratio_within_materiality_margin() {
        let mut records = Vec::new();
        // maw=10, other=10 → ratio 1.0 (tie).
        for _ in 0..5 {
            records.push(rec("maw", "C2", "T0", 0, 5, 10));
            records.push(rec("git-worktrees-bare", "C2", "T0", 0, 5, 10));
        }
        let s = aggregate_metric_records(&records);
        let cps = find_crossover(&s, MetricName::ToolCallsTotal, "maw");
        assert_eq!(cps.len(), 1);
        assert_eq!(cps[0].regime, CrossoverRegime::Tie);
        // ratio statistic should be exactly 1.0
        assert!((cps[0].statistic.unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ratio_at_boundary_materiality_is_classified_consistently() {
        // ref / other = 24/20 = 1.20 → just over materiality (×1.15) → Overkill.
        let mut records = Vec::new();
        for _ in 0..5 {
            records.push(rec("maw", "C0", "T0", 0, 5, 24));
            records.push(rec("git-worktrees-bare", "C0", "T0", 0, 5, 20));
        }
        let s = aggregate_metric_records(&records);
        let cps = find_crossover(&s, MetricName::ToolCallsTotal, "maw");
        assert_eq!(cps[0].regime, CrossoverRegime::Overkill);
        // ref / other = 16/20 = 0.80 < 1/1.15 ≈ 0.87 → Dominant.
        let mut records = Vec::new();
        for _ in 0..5 {
            records.push(rec("maw", "C0", "T0", 0, 5, 16));
            records.push(rec("git-worktrees-bare", "C0", "T0", 0, 5, 20));
        }
        let s = aggregate_metric_records(&records);
        let cps = find_crossover(&s, MetricName::ToolCallsTotal, "maw");
        assert_eq!(cps[0].regime, CrossoverRegime::Dominant);
    }

    #[test]
    fn rate_metric_uses_point_estimate_gap_for_dominance() {
        // maw: 0/10 work_lost; other: 5/10 work_lost. gap = 0.5 > 0.1 → Dominant.
        let mut records = Vec::new();
        for _ in 0..10 {
            records.push(rec("maw", "C4", "T0", 0, 5, 30));
        }
        for i in 0..10 {
            records.push(rec("jj-workspaces", "C4", "T0", u64::from(i >= 5), 5, 30));
        }
        let s = aggregate_metric_records(&records);
        let cps = find_crossover(&s, MetricName::WorkLostRate, "maw");
        assert_eq!(cps.len(), 1);
        assert_eq!(cps[0].regime, CrossoverRegime::Dominant);
        assert!(cps[0].statistic.unwrap() > 0.4);
    }

    #[test]
    fn no_data_regime_emitted_when_other_arm_has_unavailable_metric() {
        // maw cells have cost_usd; "mock-arm" cells have Unavailable cost.
        let mut records = Vec::new();
        for _ in 0..3 {
            records.push(rec("maw", "C0", "T0", 0, 5, 10));
        }
        // mock-arm record with Unavailable cost.
        let mut mock = rec("mock-arm", "C0", "T0", 0, 5, 10);
        mock.cost_usd = MetricValue::Unavailable;
        records.push(mock);
        let s = aggregate_metric_records(&records);
        let cps = find_crossover(&s, MetricName::CostUsd, "maw");
        assert_eq!(cps.len(), 1);
        assert_eq!(cps[0].regime, CrossoverRegime::NoData);
    }

    #[test]
    fn overkill_regime_is_visible_in_output_not_clipped() {
        // The §2 binding: the regime where maw is overkill must
        // appear in the output verbatim.
        let s = planted_summary();
        let cps = find_crossover(&s, MetricName::ToolCallsTotal, "maw");
        let has_overkill = cps.iter().any(|c| c.regime == CrossoverRegime::Overkill);
        assert!(has_overkill, "overkill regime missing from output: {cps:?}");
    }

    #[test]
    fn wilson_ci_is_propagated_into_crossover_n_field() {
        // (we don't use WilsonCi directly here — the assertion is on
        // n_ref/n_other being non-zero across all emitted points.)
        // The crossover record carries n_ref / n_other so the
        // renderer can stamp the Wilson row on the spectrum table.
        let s = planted_summary();
        let cps = find_crossover(&s, MetricName::WorkLostRate, "maw");
        for cp in &cps {
            assert!(cp.n_ref >= 1);
            assert!(cp.n_other >= 1);
        }
    }
}
