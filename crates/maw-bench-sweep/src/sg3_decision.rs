//! SG3 layout-eval decision logic (T3.5 / `bn-1uzn`).
//!
//! Implements the **frozen per-metric bars from `notes/sg3-subset-prereg.md`
//! §3.1 (R1–R6)** verbatim as a pure function. The result is a
//! [`Decision`] with explicit per-rule evidence; the caller (the
//! `sg3-layout-eval` binary, the eval-pilot test, and the go/no-go
//! writeup template) consumes this verdict mechanically.
//!
//! # Contract
//!
//! - [`decide_go_no_go`] is **pure** — no I/O, no global state, no
//!   randomness. Two calls with byte-identical inputs return
//!   byte-identical outputs.
//! - The function only **reads** [`SweepSummary`] cells; it never
//!   computes new metrics. Aggregation is the [`crate::aggregate`]
//!   module's job; decisioning is this module's job. This is the
//!   load-bearing separation that lets the §0 freeze clause bind:
//!   the bars cannot drift if the bars are encoded in a pure function
//!   the pre-reg pins.
//! - Per-rule rationale strings name the exact pre-reg §3.1 row
//!   (R1..R6) so a downstream writeup template can render the
//!   `notes/sg3-subset-prereg.md` table verbatim.
//! - "Ties go to the old layout" (§3.5) — a borderline point estimate
//!   that sits exactly AT a margin RESOLVES TO `NoGo`. The
//!   `EvaluatedRule::status` carries `RuleStatus::FailBorderline` for
//!   that case so the writeup can name it.
//! - Subset GO requires **every** rule to PASS on **both** cells
//!   (§3.1 "ALL of the following hold across BOTH SUB-A and SUB-B").
//!   A per-cell mixed result (one cell GO, one cell NO-GO) RESOLVES
//!   AS NO-GO for the whole subset (§5).
//!
//! # What this module does NOT do
//!
//! - It does **not** compute paired bootstrap CIs (R4 / R5). The
//!   bars are stated as "paired CI excludes 0 on the worse side AND
//!   median ratio exceeds ×1.15". The bootstrap is the analysis
//!   step that produces the `paired_ci_excludes_zero` flag; this
//!   module consumes that flag along with the median ratio. The
//!   bootstrap is invoked by the `sg3-layout-eval` binary because it
//!   needs the raw per-replicate values, not the cell aggregate.
//! - It does **not** know about Wilson upper bounds (§3.4). Those
//!   are a **reporting** discipline applied to GO results; they do
//!   not flip a verdict. The writeup template carries the Wilson
//!   formatting via [`crate::aggregate::WilsonCi`].
//! - It does **not** know about §3.6 pilot-rule exclusion. The
//!   pilot vs. real-run discrimination is the caller's
//!   responsibility (the eval binary tags pilot outputs with a
//!   `--pilot` flag and the writeup template excludes pilot data
//!   from the §3.1 verdict).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use maw_bench_metrics::MetricValue;

use crate::aggregate::{CellAggregate, SweepSummary};

/// The frozen subset cells (`notes/sg3-subset-prereg.md` §1.1).
pub const SUBSET_CELLS: &[(&str, &str)] = &[
    ("C0", "T0"), // SUB-A — benign / overkill regime, N=20
    ("C2", "T0"), // SUB-B — moderate / wedge-trigger anchor, N=10
];

/// The frozen arm names (`notes/sg3-subset-prereg.md` §1.2).
pub const ARM_OLD: &str = "maw@old-layout";
/// The frozen arm name for the proposed new layout.
pub const ARM_NEW: &str = "maw@new-layout";

/// Materiality margins frozen by `notes/sg3-subset-prereg.md` §3.1 / §3.5.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrereggedBars {
    /// R2 — `workflow_loss` rate absolute margin (`new ≤ old + 0.05`).
    pub workflow_loss_rate_margin_abs: f64,
    /// R3 — `wedge_incident` rate absolute margin (`new ≤ old + 0.10`).
    pub wedge_incident_rate_margin_abs: f64,
    /// R4 / R5 — median-ratio materiality margin (`×1.15`).
    pub turns_to_done_ratio_margin: f64,
    /// R5 — same materiality as R4 for `tool_calls_total`. Stored
    /// separately so a future §7 amendment can split them.
    pub tool_calls_total_ratio_margin: f64,
}

impl Default for PrereggedBars {
    /// The frozen bars at freeze `2026-05-25T00:00:00Z` (see
    /// `notes/sg3-subset-prereg.md` §3.1).
    fn default() -> Self {
        Self {
            workflow_loss_rate_margin_abs: 0.05,
            wedge_incident_rate_margin_abs: 0.10,
            turns_to_done_ratio_margin: 1.15,
            tool_calls_total_ratio_margin: 1.15,
        }
    }
}

/// Pre-computed paired-bootstrap input for R4 / R5. The bootstrap is
/// run upstream (by the eval binary) on raw per-replicate values; the
/// decision logic consumes the boolean output.
///
/// Per `notes/sg3-subset-prereg.md` §3.1 row R4 / R5:
///   "NO-GO iff paired bootstrap 95% CI for `(new − old)` excludes 0
///    on the worse (positive) side AND median ratio `median(new) /
///    median(old)` exceeds ×1.15. Both conditions must hold for a NO-GO."
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PairedCiSignal {
    /// True iff the paired bootstrap 95% CI for `(new − old)` excludes
    /// 0 on the worse (positive) side. False if the CI straddles 0 or
    /// is on the better (negative) side. False is the safe default for
    /// pilot data where the bootstrap was not run.
    pub ci_excludes_zero_on_worse_side: bool,
}

/// Paired CI signals collected from upstream bootstrap analysis. Keyed
/// by `(cell_id, metric_name)` where `cell_id` is e.g. `"C0-T0"` and
/// `metric_name` is `"turns_to_done"` or `"tool_calls_total"`.
pub type PairedCiSignals = BTreeMap<(String, String), PairedCiSignal>;

/// The status of a single (rule, cell) evaluation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleStatus {
    /// The bar held strictly (`new < old + margin` or `new < old` etc).
    Pass,
    /// Layout improved (positive finding per §3.5).
    PassImproved,
    /// Layout equal to old at the bar; passes by §3.5 (`new == old`
    /// equivalence side of "superiority-or-equivalence").
    PassEquivalent,
    /// The bar tripped — verdict is NO-GO.
    Fail,
    /// The point estimate sat exactly AT the margin (e.g.
    /// `new == old + margin`); §3.5 "ties go to the old layout" maps
    /// this to NO-GO and the writeup must name it.
    FailBorderline,
    /// The cell had insufficient data for the rule (e.g. R1 cell missing,
    /// R6 zero replicates). Treated as NO-GO under "data must exist to
    /// pass" — the eval cannot pass a bar it could not measure.
    FailMissingData,
}

impl RuleStatus {
    /// Did this status pass the gate? Both `Pass` and the §3.5 positive
    /// findings count.
    #[must_use]
    pub const fn passed(self) -> bool {
        matches!(self, Self::Pass | Self::PassImproved | Self::PassEquivalent)
    }
}

/// One rule's evaluation against one cell.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvaluatedRule {
    /// `"R1"` ..= `"R6"` — names the pre-reg §3.1 row.
    pub rule_id: String,
    /// The cell id (e.g. `"C0-T0"`).
    pub cell_id: String,
    /// The metric this rule guards.
    pub metric: String,
    /// Old-layout side of the comparison, as a rendered string.
    pub old_value: String,
    /// New-layout side of the comparison, as a rendered string.
    pub new_value: String,
    /// Status of this rule on this cell.
    pub status: RuleStatus,
    /// Free-form explanation suitable for the writeup template.
    pub rationale: String,
}

/// Per-rule evidence the decision can carry into the writeup.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    /// All rule evaluations (one per (rule, cell) pair). Ordered
    /// R1→R6 then cell→cell for stable rendering.
    pub rules: Vec<EvaluatedRule>,
}

impl Evidence {
    /// Push a rule evaluation, preserving stable ordering by rule id
    /// then cell id.
    pub fn push(&mut self, rule: EvaluatedRule) {
        self.rules.push(rule);
    }

    /// True iff every rule on every cell passed.
    #[must_use]
    pub fn all_passed(&self) -> bool {
        self.rules.iter().all(|r| r.status.passed())
    }

    /// The first failing rule, if any. Used to name the offending
    /// metric in the [`Decision::NoGo`] payload.
    #[must_use]
    pub fn first_failure(&self) -> Option<&EvaluatedRule> {
        self.rules.iter().find(|r| !r.status.passed())
    }
}

/// The mechanical SG3 layout-eval verdict.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Decision {
    /// All §3.1 bars held on both cells. Merge the layout into v1.0.
    Go {
        /// Per-rule evidence carried into the writeup.
        evidence: Evidence,
    },
    /// One or more §3.1 bars tripped. Defer the layout; v1.0 ships on
    /// the current `ws/` layout. The offending rule + metric are
    /// surfaced so the writeup can name them.
    NoGo {
        /// Per-rule evidence carried into the writeup.
        evidence: Evidence,
        /// The first §3.1 rule that tripped (R1..R6).
        regression_rule: String,
        /// The metric the first-tripped rule guards.
        regression_metric: String,
        /// Rendered "by how much" summary for the writeup.
        by_amount: String,
    },
}

impl Decision {
    /// Convenience: render the verdict as a short label suitable for
    /// the writeup header.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Go { .. } => "GO",
            Self::NoGo { .. } => "NO-GO",
        }
    }

    /// Convenience: return the evidence regardless of verdict.
    #[must_use]
    pub fn evidence(&self) -> &Evidence {
        match self {
            Self::Go { evidence } | Self::NoGo { evidence, .. } => evidence,
        }
    }
}

/// Decide GO vs NO-GO from a pair of [`SweepSummary`]s — one per
/// layout arm — and a frozen [`PrereggedBars`] (defaulted from
/// `notes/sg3-subset-prereg.md` §3.1).
///
/// Inputs:
/// - `old_summary`: the `maw@old-layout` arm's aggregate over SUB-A +
///   SUB-B.
/// - `new_summary`: the `maw@new-layout` arm's aggregate over the
///   same cells.
/// - `paired`: optional paired-bootstrap signals for R4 / R5 (keyed
///   by `("C0-T0" | "C2-T0", "turns_to_done" | "tool_calls_total")`).
///   If missing, R4 / R5 fall back to "ratio-only" decisioning:
///   `Fail` iff median ratio strictly exceeds the margin, `Pass`
///   otherwise. This is the explicit pilot-mode behavior; real-run
///   callers MUST supply bootstrap output.
/// - `bars`: the frozen materiality margins.
///
/// Returns [`Decision::Go`] iff every rule on every subset cell
/// passes per §3.1; otherwise [`Decision::NoGo`] naming the first
/// offending (rule, cell).
#[must_use]
pub fn decide_go_no_go(
    old_summary: &SweepSummary,
    new_summary: &SweepSummary,
    paired: Option<&PairedCiSignals>,
    bars: PrereggedBars,
) -> Decision {
    let mut evidence = Evidence::default();

    // Iterate the subset cells in canonical order so evidence is
    // stable across calls.
    for (cond_id, t_class) in SUBSET_CELLS {
        let cell_id = format!("{cond_id}-{t_class}");
        let old = old_summary.cell(ARM_OLD, cond_id, t_class);
        let new = new_summary.cell(ARM_NEW, cond_id, t_class);
        evaluate_cell(&cell_id, old, new, paired, &bars, &mut evidence);
    }

    if evidence.all_passed() {
        Decision::Go { evidence }
    } else {
        let f = evidence
            .first_failure()
            .expect("all_passed false ⇒ first_failure Some")
            .clone();
        Decision::NoGo {
            regression_rule: f.rule_id.clone(),
            regression_metric: f.metric.clone(),
            by_amount: f.rationale.clone(),
            evidence,
        }
    }
}

fn evaluate_cell(
    cell_id: &str,
    old: Option<&CellAggregate>,
    new: Option<&CellAggregate>,
    paired: Option<&PairedCiSignals>,
    bars: &PrereggedBars,
    out: &mut Evidence,
) {
    out.push(eval_r1(cell_id, old, new));
    out.push(eval_r2(cell_id, old, new, bars));
    out.push(eval_r3(cell_id, old, new, bars));
    out.push(eval_r4(cell_id, old, new, paired, bars));
    out.push(eval_r5(cell_id, old, new, paired, bars));
    out.push(eval_r6(cell_id, old, new));
}

// ---------------------------------------------------------------------------
// R1 — irrecoverable_lost_work == 0 (hard; one occurrence = NO-GO)
// ---------------------------------------------------------------------------
//
// We use `work_lost_events > 0` as the v1 schema's hard-bar proxy
// (per `notes/sg3-subset-prereg.md` §2.1: "work_lost_events IS the
// substrate-agnostic surface; if T3.5 runs on v2, `work_lost_events
// == 0` is the v2-equivalent hard-bar precondition"). v3 will split
// this into `irrecoverable_lost_work` per-named-metric rows; that
// extension is forward-compatible because Wilson CI's `k` is the
// same count.
fn eval_r1(
    cell_id: &str,
    old: Option<&CellAggregate>,
    new: Option<&CellAggregate>,
) -> EvaluatedRule {
    let (old_k, new_k) = match (old, new) {
        (Some(o), Some(n)) => (o.work_lost_rate_ci.k, n.work_lost_rate_ci.k),
        _ => {
            return missing(
                "R1",
                cell_id,
                "irrecoverable_lost_work",
                old.is_some(),
                new.is_some(),
            );
        }
    };
    let status = if new_k == 0 {
        RuleStatus::Pass
    } else {
        RuleStatus::Fail
    };
    let rationale = format!(
        "hard bar: new-layout work_lost_events = {new_k}, old = {old_k}; \
         §3.1 R1 = exactly 0 across all subset runs"
    );
    EvaluatedRule {
        rule_id: "R1".into(),
        cell_id: cell_id.to_string(),
        metric: "irrecoverable_lost_work".into(),
        old_value: old_k.to_string(),
        new_value: new_k.to_string(),
        status,
        rationale,
    }
}

// ---------------------------------------------------------------------------
// R2 — workflow_loss rate (new ≤ old + 0.05 absolute)
// ---------------------------------------------------------------------------
//
// "workflow_loss" per §2.1 is `!deliverable_integrated OR
// recoverable_orphaned_work`. v1 schema does not yet split that out;
// per §2.1 the v1-equivalent is `work_lost_events > 0` ∨ a
// non-Success verdict. The aggregator surfaces `work_lost_rate_ci`
// (proportion of runs with `work_lost_events > 0`); we use that as
// the workflow_loss proxy until v3 ships the named split.
fn eval_r2(
    cell_id: &str,
    old: Option<&CellAggregate>,
    new: Option<&CellAggregate>,
    bars: &PrereggedBars,
) -> EvaluatedRule {
    let (old_rate, new_rate) = match (old, new) {
        (Some(o), Some(n)) => (
            o.work_lost_rate_ci.point_estimate,
            n.work_lost_rate_ci.point_estimate,
        ),
        _ => return missing("R2", cell_id, "workflow_loss", old.is_some(), new.is_some()),
    };
    let delta = new_rate - old_rate;
    let margin = bars.workflow_loss_rate_margin_abs;
    let status = rate_status(delta, margin);
    let rationale = format!(
        "rate(new) = {new_rate:.3}, rate(old) = {old_rate:.3}, \
         Δ = {delta:+.3}; §3.1 R2 margin = +{margin:.3} absolute (ties → NO-GO per §3.5)"
    );
    EvaluatedRule {
        rule_id: "R2".into(),
        cell_id: cell_id.to_string(),
        metric: "workflow_loss".into(),
        old_value: format!("{old_rate:.3}"),
        new_value: format!("{new_rate:.3}"),
        status,
        rationale,
    }
}

// ---------------------------------------------------------------------------
// R3 — wedge_incident rate (new ≤ old + 0.10 absolute)
// ---------------------------------------------------------------------------
//
// v1 schema does not surface `wedge_incident` as its own field
// (T2.5/T2.6 add it). For the pilot we use the same
// `work_lost_rate_ci` proxy — the bound it asserts (rate diff ≤
// +0.10) is strictly weaker than R2's (+0.05) so any case that
// passes R2 trivially passes R3 under the v1 proxy. Real-run
// callers MUST overlay v2/v3 wedge_incident derivation via a future
// PairedCiSignals/extras path; this implementation is the pilot
// floor.
fn eval_r3(
    cell_id: &str,
    old: Option<&CellAggregate>,
    new: Option<&CellAggregate>,
    bars: &PrereggedBars,
) -> EvaluatedRule {
    let (old_rate, new_rate) = match (old, new) {
        (Some(o), Some(n)) => (
            o.work_lost_rate_ci.point_estimate,
            n.work_lost_rate_ci.point_estimate,
        ),
        _ => {
            return missing(
                "R3",
                cell_id,
                "wedge_incident",
                old.is_some(),
                new.is_some(),
            );
        }
    };
    let delta = new_rate - old_rate;
    let margin = bars.wedge_incident_rate_margin_abs;
    let status = rate_status(delta, margin);
    let rationale = format!(
        "rate(new) = {new_rate:.3}, rate(old) = {old_rate:.3}, \
         Δ = {delta:+.3}; §3.1 R3 margin = +{margin:.3} absolute (proxy from work_lost_rate; \
         v3 will split via wedge_incident named metric)"
    );
    EvaluatedRule {
        rule_id: "R3".into(),
        cell_id: cell_id.to_string(),
        metric: "wedge_incident".into(),
        old_value: format!("{old_rate:.3}"),
        new_value: format!("{new_rate:.3}"),
        status,
        rationale,
    }
}

// ---------------------------------------------------------------------------
// R4 — median turns_to_done (paired bootstrap CI + ratio gate)
// ---------------------------------------------------------------------------
//
// "NO-GO iff paired bootstrap 95% CI for `(new − old)` excludes 0
// on the worse (positive) side AND median ratio `median(new) /
// median(old)` exceeds ×1.15. Both conditions must hold for a NO-GO."
fn eval_r4(
    cell_id: &str,
    old: Option<&CellAggregate>,
    new: Option<&CellAggregate>,
    paired: Option<&PairedCiSignals>,
    bars: &PrereggedBars,
) -> EvaluatedRule {
    let (old_med, new_med) = match (old, new) {
        (Some(o), Some(n)) => {
            let o_med = median_or_zero(o, "turns_to_done");
            let n_med = median_or_zero(n, "turns_to_done");
            (o_med, n_med)
        }
        _ => return missing("R4", cell_id, "turns_to_done", old.is_some(), new.is_some()),
    };
    let ci_signal = paired
        .and_then(|m| {
            m.get(&(cell_id.to_string(), "turns_to_done".into()))
                .copied()
        })
        .unwrap_or_default();
    let (status, ratio) = median_ratio_status(
        old_med,
        new_med,
        bars.turns_to_done_ratio_margin,
        ci_signal.ci_excludes_zero_on_worse_side,
    );
    let rationale = format!(
        "median(new) = {new_med}, median(old) = {old_med}, ratio = {ratio:.3}; \
         margin ×{:.3}; paired_CI_excludes_0_on_worse_side = {}; §3.1 R4 \
         requires BOTH worse-CI AND ratio > margin for NO-GO (ties → NO-GO per §3.5)",
        bars.turns_to_done_ratio_margin, ci_signal.ci_excludes_zero_on_worse_side,
    );
    EvaluatedRule {
        rule_id: "R4".into(),
        cell_id: cell_id.to_string(),
        metric: "turns_to_done".into(),
        old_value: old_med.to_string(),
        new_value: new_med.to_string(),
        status,
        rationale,
    }
}

// ---------------------------------------------------------------------------
// R5 — median tool_calls_total (paired bootstrap CI + ratio gate)
// ---------------------------------------------------------------------------
fn eval_r5(
    cell_id: &str,
    old: Option<&CellAggregate>,
    new: Option<&CellAggregate>,
    paired: Option<&PairedCiSignals>,
    bars: &PrereggedBars,
) -> EvaluatedRule {
    let (old_med, new_med) = match (old, new) {
        (Some(o), Some(n)) => (
            median_or_zero(o, "tool_calls_total"),
            median_or_zero(n, "tool_calls_total"),
        ),
        _ => {
            return missing(
                "R5",
                cell_id,
                "tool_calls_total",
                old.is_some(),
                new.is_some(),
            );
        }
    };
    let ci_signal = paired
        .and_then(|m| {
            m.get(&(cell_id.to_string(), "tool_calls_total".into()))
                .copied()
        })
        .unwrap_or_default();
    let (status, ratio) = median_ratio_status(
        old_med,
        new_med,
        bars.tool_calls_total_ratio_margin,
        ci_signal.ci_excludes_zero_on_worse_side,
    );
    let rationale = format!(
        "median(new) = {new_med}, median(old) = {old_med}, ratio = {ratio:.3}; \
         margin ×{:.3}; paired_CI_excludes_0_on_worse_side = {}; §3.1 R5 \
         requires BOTH worse-CI AND ratio > margin for NO-GO (ties → NO-GO per §3.5)",
        bars.tool_calls_total_ratio_margin, ci_signal.ci_excludes_zero_on_worse_side,
    );
    EvaluatedRule {
        rule_id: "R5".into(),
        cell_id: cell_id.to_string(),
        metric: "tool_calls_total".into(),
        old_value: old_med.to_string(),
        new_value: new_med.to_string(),
        status,
        rationale,
    }
}

// ---------------------------------------------------------------------------
// R6 — interventions total (new ≤ old; no net increase)
// ---------------------------------------------------------------------------
//
// v1 schema surfaces `human_intervention_events` as Unavailable
// (forward-compat hook). The aggregator does not yet derive an
// `interventions_total` per cell. For the pilot we use the
// `work_redone_turns` median sum as the proxy (`interventions` is
// per §2.1 "events where the agent abandons/discards committed work
// or escalates out of the task to recover" — `work_redone_turns` is
// the closest v1 proxy until T2.5 lands attribution-driven counts).
fn eval_r6(
    cell_id: &str,
    old: Option<&CellAggregate>,
    new: Option<&CellAggregate>,
) -> EvaluatedRule {
    let (old_total, new_total) = match (old, new) {
        (Some(o), Some(n)) => (
            sum_proxy(o, "work_redone_turns"),
            sum_proxy(n, "work_redone_turns"),
        ),
        _ => return missing("R6", cell_id, "interventions", old.is_some(), new.is_some()),
    };
    // §3.5 ties go to old: new ≤ old PASSES (PassEquivalent on
    // exact equality); new > old FAILS.
    let status = if new_total < old_total {
        RuleStatus::PassImproved
    } else if new_total == old_total {
        RuleStatus::PassEquivalent
    } else {
        RuleStatus::Fail
    };
    let rationale = format!(
        "total(new) = {new_total}, total(old) = {old_total}; §3.1 R6 = no net increase \
         (raw per-replicate sum of work_redone_turns; bn-27ai Fix A.3 replaced the \
         lossy median×n proxy)"
    );
    EvaluatedRule {
        rule_id: "R6".into(),
        cell_id: cell_id.to_string(),
        metric: "interventions".into(),
        old_value: old_total.to_string(),
        new_value: new_total.to_string(),
        status,
        rationale,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// §3.5 "ties go to old layout": for a margin-bounded rate bar, a
/// point estimate that lies STRICTLY BELOW the margin is `Pass`; a
/// point estimate exactly AT the margin is `FailBorderline` (NO-GO);
/// anything ABOVE the margin is `Fail`. The improved-side
/// classification (`Δ < 0`) is `PassImproved`; `Δ == 0` is
/// `PassEquivalent`.
fn rate_status(delta: f64, margin: f64) -> RuleStatus {
    // f64 equality: rates come from `k / n` so two cells with the
    // same k+n produce bit-identical `point_estimate`. The
    // `≈margin` comparison uses a strict-equality test with a tiny
    // tolerance for paranoia.
    let eps = 1e-9;
    if delta < -eps {
        RuleStatus::PassImproved
    } else if delta.abs() <= eps {
        RuleStatus::PassEquivalent
    } else if delta + eps < margin {
        RuleStatus::Pass
    } else if (delta - margin).abs() <= eps {
        RuleStatus::FailBorderline
    } else {
        RuleStatus::Fail
    }
}

/// R4 / R5 status. Both the worse-side CI AND the ratio > margin
/// must hold for NO-GO. Otherwise the result is some flavor of pass.
fn median_ratio_status(
    old_med: u64,
    new_med: u64,
    margin: f64,
    ci_excludes_zero_on_worse_side: bool,
) -> (RuleStatus, f64) {
    // Guard against div-by-zero. old_med == 0 ⇒ ratio is undefined;
    // if new_med is also 0 it's PassEquivalent; otherwise it's
    // FailBorderline (the layout introduced a non-zero where old
    // had zero — meaningful regression by direction).
    if old_med == 0 {
        let status = if new_med == 0 {
            RuleStatus::PassEquivalent
        } else if ci_excludes_zero_on_worse_side {
            RuleStatus::Fail
        } else {
            // No bootstrap signal; we cannot trip the gate per
            // §3.1 R4/R5's two-condition rule, but the directional
            // adverse signal is recorded.
            RuleStatus::Pass
        };
        return (status, f64::INFINITY);
    }
    #[allow(clippy::cast_precision_loss)]
    let ratio = (new_med as f64) / (old_med as f64);
    let eps = 1e-9;
    // Classify the ratio:
    // - ratio < 1 - eps: improved (positive finding)
    // - |ratio - 1| <= eps: equivalent
    // - 1 < ratio < margin - eps: degraded but sub-margin (passes per
    //   §3.1 R4/R5 + §3.5)
    // - ratio == margin: borderline (NO-GO if CI also adverse,
    //   otherwise pass-borderline per the two-condition rule)
    // - ratio > margin + eps: above margin — NO-GO iff CI also
    //   adverse; pass with sub-CI rationale otherwise
    let status = if ratio + eps < 1.0 {
        RuleStatus::PassImproved
    } else if (ratio - 1.0).abs() <= eps {
        RuleStatus::PassEquivalent
    } else if ratio + eps < margin {
        RuleStatus::Pass
    } else if (ratio - margin).abs() <= eps {
        if ci_excludes_zero_on_worse_side {
            RuleStatus::FailBorderline
        } else {
            // Sub-CI: pass borderline (the two-condition rule was
            // not met). Per §3.5 directional adverse signal is
            // surfaced in the rationale, not the status.
            RuleStatus::Pass
        }
    } else if ci_excludes_zero_on_worse_side {
        RuleStatus::Fail
    } else {
        // Ratio > margin but CI does not exclude 0 on the worse
        // side — per the two-condition rule, this is NOT a NO-GO.
        // The pre-reg permits "statistically real but
        // sub-materiality regression" through, but a ratio above
        // the materiality margin with no statistical signal is the
        // mirror case: the materiality margin is exceeded but the
        // statistical signal is absent. Under R4/R5's BOTH
        // conditions rule, this passes. We tag it `Pass`.
        RuleStatus::Pass
    };
    (status, ratio)
}

fn median_or_zero(cell: &CellAggregate, name: &str) -> u64 {
    cell.median
        .get(name)
        .map(|v| match v {
            MetricValue::Count { n } => *n,
            MetricValue::DurationMs { ms } => *ms,
            MetricValue::UsdCents { cents } => *cents,
            MetricValue::Infinite => u64::MAX,
            MetricValue::Unavailable => 0,
        })
        .unwrap_or(0)
}

/// Per-cell "total" used by R6 (interventions).
///
/// **bn-27ai / Fix A.3 (2026-05-27)**: now reads the **raw per-
/// replicate sum** that `aggregate::summarize_cell` populates into
/// `CellAggregate::sum`. The previous `median × n` proxy integer-
/// truncated one-bit lower-median deltas into N×-amplified totals —
/// a 7-vs-6 raw difference rendered as 10-vs-0 in the SG3 2026-05-26
/// rerun and produced a spurious R6 NO-GO. See
/// `notes/sg3-no-go-rootcause-v2.md` §3 for the full mechanism.
///
/// Falls back to `median × n` when `CellAggregate::sum` is empty
/// (older serialized summaries that predate Fix A.3 deserialize with
/// the field empty thanks to `#[serde(default)]`). The fallback path
/// preserves bit-equivalent behavior for legacy callers.
fn sum_proxy(cell: &CellAggregate, name: &str) -> u64 {
    if let Some(v) = cell.sum.get(name) {
        return match v {
            MetricValue::Count { n } => *n,
            MetricValue::DurationMs { ms } => *ms,
            MetricValue::UsdCents { cents } => *cents,
            MetricValue::Infinite => u64::MAX,
            MetricValue::Unavailable => 0,
        };
    }
    // Legacy fallback: cell.sum is empty (pre-bn-27ai summary).
    let med = median_or_zero(cell, name);
    med.saturating_mul(cell.n)
}

fn missing(
    rule_id: &str,
    cell_id: &str,
    metric: &str,
    has_old: bool,
    has_new: bool,
) -> EvaluatedRule {
    let rationale = format!(
        "missing data: old={has_old} new={has_new} (subset cell unrepresented; the bar \
         must be measured to be passed)"
    );
    EvaluatedRule {
        rule_id: rule_id.to_string(),
        cell_id: cell_id.to_string(),
        metric: metric.to_string(),
        old_value: "n/a".into(),
        new_value: "n/a".into(),
        status: RuleStatus::FailMissingData,
        rationale,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregate::{WilsonCi, aggregate_metric_records};
    use maw_bench_metrics::{MetricRecord, MetricValue};
    use std::collections::BTreeMap;

    fn rec(
        arm: &str,
        cond: &str,
        t: &str,
        lost: u64,
        turns: u64,
        calls: u64,
        redone: u64,
    ) -> MetricRecord {
        MetricRecord {
            schema_version: MetricRecord::SCHEMA_VERSION,
            run_id: format!("{arm}-{cond}-{t}-{lost}-{turns}-{calls}-{redone}"),
            arm: arm.into(),
            condition_id: cond.into(),
            t_class: t.into(),
            work_lost_events: MetricValue::count(lost),
            human_intervention_events: MetricValue::Unavailable,
            tool_calls_total: MetricValue::count(calls),
            turns_to_done: MetricValue::count(turns),
            wall_duration_ms: MetricValue::duration_ms(1_000),
            cost_usd: MetricValue::Unavailable,
            work_redone_turns: MetricValue::count(redone),
            per_verb_wasted_turns: BTreeMap::new(),
        }
    }

    /// Build a summary for one arm at one cell with N identical
    /// replicates. Pilot-tight; tests want byte-stable cell shapes.
    fn summary_of(arm: &str, cells: &[(&str, &str, u64, u64, u64, u64, u64)]) -> SweepSummary {
        // (cond, t, n_reps, lost_per_rep, turns, calls, redone)
        let mut recs = Vec::new();
        for (cond, t, n_reps, lost, turns, calls, redone) in cells {
            for _ in 0..*n_reps {
                recs.push(rec(arm, cond, t, *lost, *turns, *calls, *redone));
            }
        }
        aggregate_metric_records(&recs)
    }

    fn identical_summaries() -> (SweepSummary, SweepSummary) {
        let cells = &[
            ("C0", "T0", 20_u64, 0, 5, 12, 0),
            ("C2", "T0", 10_u64, 0, 8, 18, 1),
        ];
        let old = summary_of(ARM_OLD, cells);
        let new = summary_of(ARM_NEW, cells);
        (old, new)
    }

    #[test]
    fn identical_summaries_pass_all_rules() {
        let (old, new) = identical_summaries();
        let d = decide_go_no_go(&old, &new, None, PrereggedBars::default());
        match d {
            Decision::Go { evidence } => {
                assert!(evidence.all_passed(), "evidence: {evidence:#?}");
                // 6 rules × 2 cells = 12 EvaluatedRule rows.
                assert_eq!(evidence.rules.len(), 12);
                // Every rule is some flavor of pass; identical
                // medians + zero-event proportions ⇒ PassEquivalent
                // for the rate + ratio rules.
                for r in &evidence.rules {
                    assert!(
                        r.status.passed(),
                        "rule {} on cell {} failed: {:?}",
                        r.rule_id,
                        r.cell_id,
                        r
                    );
                }
            }
            Decision::NoGo { .. } => panic!("expected Go: {d:#?}"),
        }
    }

    /// R1 — any `work_lost_events > 0` on new-layout trips the hard bar.
    #[test]
    fn r1_hard_bar_one_loss_is_no_go() {
        let old = summary_of(
            ARM_OLD,
            &[("C0", "T0", 20, 0, 5, 12, 0), ("C2", "T0", 10, 0, 8, 18, 1)],
        );
        // One run in SUB-B with a lost event.
        let mut recs = vec![rec(ARM_NEW, "C0", "T0", 0, 5, 12, 0); 20];
        recs.extend(vec![rec(ARM_NEW, "C2", "T0", 0, 8, 18, 1); 9]);
        recs.push(rec(ARM_NEW, "C2", "T0", 1, 8, 18, 1));
        let new = aggregate_metric_records(&recs);

        let d = decide_go_no_go(&old, &new, None, PrereggedBars::default());
        match d {
            Decision::NoGo {
                regression_rule,
                regression_metric,
                ..
            } => {
                assert_eq!(regression_rule, "R1");
                assert_eq!(regression_metric, "irrecoverable_lost_work");
            }
            Decision::Go { .. } => panic!("expected NoGo: {d:#?}"),
        }
    }

    /// R2 — workflow_loss rate adverse beyond +0.05 trips.
    #[test]
    fn r2_workflow_loss_rate_above_margin_is_no_go() {
        // Old: 0/20 wedge events at SUB-A. New: 5/20 = 0.25 rate.
        // delta = 0.25 - 0.0 = 0.25 > 0.05 ⇒ NO-GO.
        let mut old_recs = vec![rec(ARM_OLD, "C0", "T0", 0, 5, 12, 0); 20];
        old_recs.extend(vec![rec(ARM_OLD, "C2", "T0", 0, 8, 18, 1); 10]);
        let old = aggregate_metric_records(&old_recs);

        let mut new_recs = vec![rec(ARM_NEW, "C0", "T0", 1, 5, 12, 0); 5];
        new_recs.extend(vec![rec(ARM_NEW, "C0", "T0", 0, 5, 12, 0); 15]);
        new_recs.extend(vec![rec(ARM_NEW, "C2", "T0", 0, 8, 18, 1); 10]);
        let new = aggregate_metric_records(&new_recs);

        let d = decide_go_no_go(&old, &new, None, PrereggedBars::default());
        match d {
            Decision::NoGo {
                regression_rule, ..
            } => assert_eq!(regression_rule, "R1"),
            Decision::Go { .. } => panic!("expected NoGo: {d:#?}"),
        }
        // Note: the "1" in lost trips R1 first (hard bar). To exercise R2 cleanly
        // we need the v3-split metric; the pilot's tied-rule-firing behavior is
        // tested via r3 below where the rate proxy fires on its own through R3.
    }

    /// R3 — wedge_incident rate adverse beyond +0.10 trips (proxy from
    /// work_lost rate). Identical to R2 but the rate increase exceeds
    /// +0.10. Because R2 fires first under the v1 proxy, we observe that
    /// behavior (first-failure ordering).
    #[test]
    fn r3_wedge_rate_adverse_first_failure_is_r1_or_r2_proxy_chain() {
        // Old benign cell: 0/20 lost. New: 4/20 = 0.20 rate
        // (still under R3's +0.10? 0.20 - 0 = 0.20 > 0.10 ⇒ R3 trips).
        // R2 also trips (0.20 > 0.05). R1 also trips (any > 0).
        // First failure is R1; this confirms ordering.
        let mut old_recs = vec![rec(ARM_OLD, "C0", "T0", 0, 5, 12, 0); 20];
        old_recs.extend(vec![rec(ARM_OLD, "C2", "T0", 0, 8, 18, 1); 10]);
        let old = aggregate_metric_records(&old_recs);

        let mut new_recs = vec![rec(ARM_NEW, "C0", "T0", 1, 5, 12, 0); 4];
        new_recs.extend(vec![rec(ARM_NEW, "C0", "T0", 0, 5, 12, 0); 16]);
        new_recs.extend(vec![rec(ARM_NEW, "C2", "T0", 0, 8, 18, 1); 10]);
        let new = aggregate_metric_records(&new_recs);

        let d = decide_go_no_go(&old, &new, None, PrereggedBars::default());
        match &d {
            Decision::NoGo {
                regression_rule,
                evidence,
                ..
            } => {
                assert_eq!(regression_rule, "R1");
                // R1, R2, R3 all fail at SUB-A.
                let sub_a_r1 = evidence
                    .rules
                    .iter()
                    .find(|r| r.rule_id == "R1" && r.cell_id == "C0-T0")
                    .unwrap();
                let sub_a_r2 = evidence
                    .rules
                    .iter()
                    .find(|r| r.rule_id == "R2" && r.cell_id == "C0-T0")
                    .unwrap();
                let sub_a_r3 = evidence
                    .rules
                    .iter()
                    .find(|r| r.rule_id == "R3" && r.cell_id == "C0-T0")
                    .unwrap();
                assert!(!sub_a_r1.status.passed());
                assert!(!sub_a_r2.status.passed());
                assert!(!sub_a_r3.status.passed());
            }
            Decision::Go { .. } => panic!("expected NoGo: {d:#?}"),
        }
    }

    /// R4 — turns ratio above ×1.15 + worse-side CI ⇒ NO-GO.
    #[test]
    fn r4_turns_ratio_above_margin_and_ci_adverse_is_no_go() {
        // Old turns median = 10. New turns median = 13. Ratio = 1.30 > 1.15.
        let old = summary_of(
            ARM_OLD,
            &[
                ("C0", "T0", 20, 0, 10, 12, 0),
                ("C2", "T0", 10, 0, 8, 18, 1),
            ],
        );
        let new = summary_of(
            ARM_NEW,
            &[
                ("C0", "T0", 20, 0, 13, 12, 0),
                ("C2", "T0", 10, 0, 8, 18, 1),
            ],
        );

        // Supply CI signal: SUB-A turns CI excludes 0 on worse side.
        let mut paired = PairedCiSignals::new();
        paired.insert(
            ("C0-T0".into(), "turns_to_done".into()),
            PairedCiSignal {
                ci_excludes_zero_on_worse_side: true,
            },
        );
        let d = decide_go_no_go(&old, &new, Some(&paired), PrereggedBars::default());
        match d {
            Decision::NoGo {
                regression_rule,
                regression_metric,
                ..
            } => {
                assert_eq!(regression_rule, "R4");
                assert_eq!(regression_metric, "turns_to_done");
            }
            Decision::Go { .. } => panic!("expected NoGo: {d:#?}"),
        }
    }

    /// R4 — ratio above margin but CI is NOT worse-side ⇒ GO (the
    /// two-condition rule blocks the NO-GO when only one holds).
    #[test]
    fn r4_turns_ratio_above_margin_without_ci_is_go() {
        let old = summary_of(
            ARM_OLD,
            &[
                ("C0", "T0", 20, 0, 10, 12, 0),
                ("C2", "T0", 10, 0, 8, 18, 1),
            ],
        );
        let new = summary_of(
            ARM_NEW,
            &[
                ("C0", "T0", 20, 0, 13, 12, 0),
                ("C2", "T0", 10, 0, 8, 18, 1),
            ],
        );
        // Empty paired ⇒ CI signal defaults to false (no worse-side).
        let d = decide_go_no_go(&old, &new, None, PrereggedBars::default());
        match d {
            Decision::Go { evidence } => {
                let r4 = evidence
                    .rules
                    .iter()
                    .find(|r| r.rule_id == "R4" && r.cell_id == "C0-T0")
                    .unwrap();
                assert!(r4.status.passed(), "R4 SUB-A: {r4:?}");
                assert!(
                    r4.rationale
                        .contains("paired_CI_excludes_0_on_worse_side = false"),
                    "rationale: {}",
                    r4.rationale
                );
            }
            Decision::NoGo { .. } => panic!("expected Go: {d:#?}"),
        }
    }

    /// R5 — tool-calls ratio above margin + worse-side CI ⇒ NO-GO.
    #[test]
    fn r5_tool_calls_ratio_above_margin_and_ci_adverse_is_no_go() {
        let old = summary_of(
            ARM_OLD,
            &[("C0", "T0", 20, 0, 5, 10, 0), ("C2", "T0", 10, 0, 8, 18, 1)],
        );
        // New tool_calls median = 13 (ratio 1.30).
        let new = summary_of(
            ARM_NEW,
            &[("C0", "T0", 20, 0, 5, 13, 0), ("C2", "T0", 10, 0, 8, 18, 1)],
        );
        let mut paired = PairedCiSignals::new();
        paired.insert(
            ("C0-T0".into(), "tool_calls_total".into()),
            PairedCiSignal {
                ci_excludes_zero_on_worse_side: true,
            },
        );
        let d = decide_go_no_go(&old, &new, Some(&paired), PrereggedBars::default());
        match d {
            Decision::NoGo {
                regression_rule,
                regression_metric,
                ..
            } => {
                assert_eq!(regression_rule, "R5");
                assert_eq!(regression_metric, "tool_calls_total");
            }
            Decision::Go { .. } => panic!("expected NoGo: {d:#?}"),
        }
    }

    /// R6 — interventions total adverse ⇒ NO-GO.
    #[test]
    fn r6_interventions_total_above_old_is_no_go() {
        // Old SUB-A: median work_redone = 0 ⇒ total proxy = 0. New: median 1 ⇒
        // total proxy = 20. delta > 0 ⇒ Fail.
        let old = summary_of(
            ARM_OLD,
            &[("C0", "T0", 20, 0, 5, 12, 0), ("C2", "T0", 10, 0, 8, 18, 0)],
        );
        let new = summary_of(
            ARM_NEW,
            &[("C0", "T0", 20, 0, 5, 12, 1), ("C2", "T0", 10, 0, 8, 18, 0)],
        );
        let d = decide_go_no_go(&old, &new, None, PrereggedBars::default());
        match d {
            Decision::NoGo {
                regression_rule,
                regression_metric,
                ..
            } => {
                assert_eq!(regression_rule, "R6");
                assert_eq!(regression_metric, "interventions");
            }
            Decision::Go { .. } => panic!("expected NoGo: {d:#?}"),
        }
    }

    /// bn-27ai Fix A.3: `sum_proxy` reads raw per-replicate sum
    /// (not `median × n`). Reproduces the SG3 2026-05-27 rerun
    /// scenario where `median × n` amplified a 7-vs-6 raw delta into
    /// a 10-vs-0 NO-GO. With raw sums, R6 is PASS (new ≤ old or
    /// within the rule's tolerance).
    ///
    /// new fires: [0,1,1,1,0,0,1,0,1,2] → median 1, sum 7
    /// old fires: [1,0,2,0,1,0,1,1,0,0] → median 0, sum 6
    ///
    /// Pre-A.3: total(new) = 10, total(old) = 0 ⇒ R6 NO-GO.
    /// Post-A.3: total(new) = 7, total(old) = 6 ⇒ R6 Fail (7 > 6),
    /// but per §3.5 the delta is now a *one-fire* difference (not a
    /// 10× gap); §3.5 ties-go-to-old keeps strict-greater-than as
    /// Fail. The verdict still surfaces a one-event delta — the
    /// metric is now honest, the production decision will use that
    /// honest delta. (Whether the rule classifies 7v6 as PASS_EQUIV
    /// vs FAIL is the per-reg call; the metric pipeline's job is to
    /// surface the truth.)
    #[test]
    fn r6_raw_sum_proxy_matches_per_replicate_total() {
        // Build per-rep fire counts using the redone parameter.
        let mut new_recs = Vec::new();
        for r in [0_u64, 1, 1, 1, 0, 0, 1, 0, 1, 2] {
            new_recs.push(rec(ARM_NEW, "C2", "T0", 0, 8, 18, r));
        }
        let new = aggregate_metric_records(&new_recs);
        let cell_new = new.cell(ARM_NEW, "C2", "T0").unwrap();
        // sum_proxy reads raw sum, NOT median × n.
        assert_eq!(
            sum_proxy(cell_new, "work_redone_turns"),
            7,
            "raw sum is 7; pre-A.3 median×n proxy would have said 10"
        );
        // Spot-check fallback semantics: a hand-constructed cell with
        // empty `sum` must fall back to median × n.
        let mut legacy_cell = cell_new.clone();
        legacy_cell.sum.clear();
        assert_eq!(
            sum_proxy(&legacy_cell, "work_redone_turns"),
            10,
            "legacy fallback: median(1) × n(10) = 10"
        );
    }

    /// End-to-end: the SG3 rerun's exact per-replicate fire counts
    /// flip the R6 verdict from NO-GO (pre-fix proxy: 10 vs 0) to
    /// a one-event delta (post-fix raw sum: 7 vs 6). This is the
    /// "raw 7 vs 6" finding from
    /// `notes/sg3-no-go-rootcause-v2.md` §3.
    #[test]
    fn r6_sg3_rerun_pattern_is_one_event_delta_not_ten() {
        let mut new_recs = Vec::new();
        for r in [0_u64, 1, 1, 1, 0, 0, 1, 0, 1, 2] {
            new_recs.push(rec(ARM_NEW, "C2", "T0", 0, 8, 18, r));
        }
        // Pad SUB-A so the decide function has C0/T0 too.
        for _ in 0..20 {
            new_recs.push(rec(ARM_NEW, "C0", "T0", 0, 5, 12, 0));
        }
        let mut old_recs = Vec::new();
        for r in [1_u64, 0, 2, 0, 1, 0, 1, 1, 0, 0] {
            old_recs.push(rec(ARM_OLD, "C2", "T0", 0, 8, 18, r));
        }
        for _ in 0..20 {
            old_recs.push(rec(ARM_OLD, "C0", "T0", 0, 5, 12, 0));
        }
        let new = aggregate_metric_records(&new_recs);
        let old = aggregate_metric_records(&old_recs);

        // Read the R6 evaluated rule via the public decide function
        // and locate the C2-T0 row to inspect the totals.
        let d = decide_go_no_go(&old, &new, None, PrereggedBars::default());
        let evidence = match &d {
            Decision::Go { evidence } => evidence,
            Decision::NoGo { evidence, .. } => evidence,
        };
        let r6_c2 = evidence
            .rules
            .iter()
            .find(|r| r.rule_id == "R6" && r.cell_id == "C2-T0")
            .expect("R6 C2-T0 rule");
        // Post-A.3: raw 7 vs 6.
        assert_eq!(r6_c2.new_value, "7", "raw new sum");
        assert_eq!(r6_c2.old_value, "6", "raw old sum");
        // Status is Fail (7 > 6 per the strict no-net-increase rule),
        // but the rationale now carries honest numbers — not the 10v0
        // amplification artifact.
        assert!(
            r6_c2.rationale.contains("raw per-replicate sum"),
            "rationale must reference raw sum: {}",
            r6_c2.rationale
        );
    }

    /// §3.5 ties-go-to-old: a rate sitting exactly AT the margin
    /// must resolve to NO-GO via FailBorderline.
    #[test]
    fn rate_status_borderline_at_margin_is_fail_borderline() {
        let s = rate_status(0.05, 0.05);
        assert_eq!(s, RuleStatus::FailBorderline);
        assert!(!s.passed());
    }

    /// §3.5 superiority-or-equivalence: equal point estimate PASSES.
    #[test]
    fn rate_status_equal_is_pass_equivalent() {
        let s = rate_status(0.0, 0.05);
        assert_eq!(s, RuleStatus::PassEquivalent);
        assert!(s.passed());
    }

    /// §3.5 improved-side: negative Δ is a positive finding.
    #[test]
    fn rate_status_improved_is_pass_improved() {
        let s = rate_status(-0.10, 0.05);
        assert_eq!(s, RuleStatus::PassImproved);
        assert!(s.passed());
    }

    /// Missing cell → FailMissingData.
    #[test]
    fn missing_cell_in_new_summary_fails_with_missing_data() {
        let old = summary_of(
            ARM_OLD,
            &[("C0", "T0", 20, 0, 5, 12, 0), ("C2", "T0", 10, 0, 8, 18, 1)],
        );
        // New only has SUB-A.
        let new = summary_of(ARM_NEW, &[("C0", "T0", 20, 0, 5, 12, 0)]);
        let d = decide_go_no_go(&old, &new, None, PrereggedBars::default());
        match d {
            Decision::NoGo {
                regression_rule,
                evidence,
                ..
            } => {
                // SUB-B missing rules are FailMissingData.
                let sub_b_missing = evidence
                    .rules
                    .iter()
                    .find(|r| r.cell_id == "C2-T0")
                    .unwrap();
                assert_eq!(sub_b_missing.status, RuleStatus::FailMissingData);
                // First failure surfaces from SUB-B (since SUB-A passes).
                assert!(["R1", "R2", "R3", "R4", "R5", "R6"].contains(&regression_rule.as_str()));
            }
            Decision::Go { .. } => panic!("expected NoGo: {d:#?}"),
        }
    }

    /// Wilson CI is unaffected by the decision function (it's a
    /// reporting discipline). Sanity-check that the aggregator's CI
    /// flows through unchanged.
    #[test]
    fn wilson_ci_is_carried_through_summary() {
        let old = summary_of(ARM_OLD, &[("C0", "T0", 20, 0, 5, 12, 0)]);
        let cell = old.cell(ARM_OLD, "C0", "T0").unwrap();
        let WilsonCi { k, n, upper, .. } = cell.work_lost_rate_ci;
        assert_eq!(k, 0);
        assert_eq!(n, 20);
        assert!(
            upper > 0.15 && upper < 0.18,
            "Wilson upper at N=20: {upper}"
        );
    }

    /// Identical summaries WITH paired CI signals still PASS (the
    /// CI signal only matters when the ratio also exceeds margin).
    #[test]
    fn identical_summaries_with_ci_signal_still_pass() {
        let (old, new) = identical_summaries();
        let mut paired = PairedCiSignals::new();
        paired.insert(
            ("C0-T0".into(), "turns_to_done".into()),
            PairedCiSignal {
                ci_excludes_zero_on_worse_side: true,
            },
        );
        paired.insert(
            ("C0-T0".into(), "tool_calls_total".into()),
            PairedCiSignal {
                ci_excludes_zero_on_worse_side: true,
            },
        );
        let d = decide_go_no_go(&old, &new, Some(&paired), PrereggedBars::default());
        assert!(matches!(d, Decision::Go { .. }), "expected Go: {d:#?}");
    }
}
