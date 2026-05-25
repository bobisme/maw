//! T4.3 (`bn-1qty`) — SG4 re-bench friction-delta math + iteration policy.
//!
//! # What this module does
//!
//! Diffs two [`FrictionList`]s — a `baseline` (pre-hardening) snapshot
//! against an `after` (post-hardening) snapshot — and produces a
//! per-cluster delta report. Each row carries
//! `(cluster, baseline_cost, after_cost, delta_pct, target_pct,
//! target_met)`, plus a list of cluster-keyed fix-task bone IDs the
//! delta is attributed to (the T4.1 backlog wiring from
//! `notes/sg4-fix-backlog.md`).
//!
//! The module is **pure math**: same inputs → same `DeltaReport`. The
//! I/O wrapping (load FrictionList JSONs, emit JSON + Markdown) lives
//! in the `sg4-rebench` binary.
//!
//! # Why this exists (not a composite score)
//!
//! Per the T2.8 handoff hard rule and the T4.1 backlog (`hard rules in
//! effect`), each cluster carries its OWN target delta in its OWN
//! axis. The delta report computes one ratio per cluster and reports
//! per-cluster pass/fail; it does NOT aggregate the cells into a
//! "fraction of clusters that met target" headline. The
//! `RebenchVerdict` enum reports per-cluster status; the iteration
//! policy fires per-cluster.
//!
//! # Iteration policy
//!
//! For each cluster whose `after_cost / baseline_cost` is not a
//! reduction of at least `target_pct`, the policy emits a
//! [`IterationTrigger`] naming the bone ID of the fix-task child and
//! the measured gap. The lead consumes this list and re-opens those
//! specific bones (T4.2 re-run) — not a blanket re-open of T4.2.
//!
//! # Cluster cardinality (zero-baseline and new-cluster cases)
//!
//! Two boundary cases need explicit handling:
//!
//! 1. **Cluster present in `after` but absent from `baseline`**
//!    (`baseline_cost == 0`, `after_cost > 0`). The "delta percent"
//!    formula `(baseline - after) / baseline` is undefined; we record
//!    [`DeltaReportRow::delta_pct`] as `None` and the row carries
//!    [`RegressionFlag::NewClusterInAfter`]. This is a NEW friction
//!    introduced by the hardening pass — the T2.8 stop condition
//!    treats this as a blocker independent of the per-cluster
//!    reductions. The iteration policy fires
//!    [`IterationTrigger::NewClusterRegression`].
//!
//! 2. **Cluster present in `baseline` but absent from `after`**
//!    (`baseline_cost > 0`, `after_cost == 0`). The reduction is 100%.
//!    `delta_pct = Some(100.0)`; target is trivially met.
//!
//! Per the T4.1 backlog (`bn-1t17` / `vocabulary_scarcity`), the
//! "cost = 0 at baseline, must stay 0" rule is encoded as a special
//! ClusterTarget — see [`ClusterTarget::StaysZero`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::attribution::MawVerbAttribution;
use crate::friction_list::FrictionList;

/// Schema version for the [`DeltaReport`] artifact. Bumped only when
/// the consumed shape changes; additive fields do NOT bump.
pub const DELTA_REPORT_SCHEMA_VERSION: u32 = 1;

/// Per-cluster pass/fail policy. Each T4.1 backlog row carries one of
/// these; the iteration policy uses it to decide whether the cluster
/// "met target".
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClusterTarget {
    /// Reduce `total_cost_turns` by at least this fraction of the
    /// baseline value. Default per T4.1 ("≥ 50%" for every row).
    /// `min_reduction_pct` is a number in `[0.0, 100.0]`.
    ReducePct { min_reduction_pct: f64 },
    /// "Reach 0" target — practical version of "≥ 50%" for clusters
    /// whose baseline is small enough that 50% rounds to a tie. Used
    /// by `bn-c6l3` (`ws_destroy_refused`, baseline=1) and `bn-242l`
    /// (`read_from_stale_workspace`, baseline=1). The cluster passes
    /// iff `after_cost == 0`.
    ReachZero,
    /// "Was 0 at baseline; must stay 0 across the re-run". Used by
    /// `vocabulary_scarcity` (T4.1's `bn-1t17` carries this rule
    /// explicitly: if cost=0 at baseline, target is "remains 0 across
    /// the next two benches"). Passes iff `after_cost == 0`.
    StaysZero,
}

impl ClusterTarget {
    /// Default target for a fresh fix-task per T4.1.
    pub const fn default_reduce_50_pct() -> Self {
        Self::ReducePct {
            min_reduction_pct: 50.0,
        }
    }
}

/// Outcome of a per-cluster delta check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RebenchVerdict {
    /// Cluster met its target.
    TargetMet,
    /// Cluster did NOT meet its target. Iteration policy fires
    /// [`IterationTrigger::TargetMissed`].
    TargetMissed,
    /// Cluster regressed: cost went up, or the cluster was absent at
    /// baseline and appeared in the after-run.
    Regressed,
}

/// Why a regression was flagged. Surfaced alongside the verdict so the
/// lead can decide between "iterate the named child bone" vs "open a
/// new bone for the regression".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegressionFlag {
    /// Cluster was absent in baseline (`baseline_cost == 0`) and is
    /// present in after (`after_cost > 0`). This is a NEW cluster
    /// induced by hardening; the T2.8 stop-condition blocks SG4
    /// completion on this.
    NewClusterInAfter,
    /// Cluster was present in both; cost rose.
    CostIncreased,
}

/// One per-cluster row in the [`DeltaReport`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeltaReportRow {
    /// The cluster this row diffs.
    pub attribution: MawVerbAttribution,
    /// Fix-task bone ID (the T4.1 backlog wiring). Empty string when
    /// the cluster has no fix-task child — usually for a NEW cluster
    /// introduced by the after-run that wasn't in the backlog.
    pub fix_task_bone: String,
    /// Per-cluster pass/fail policy.
    pub target: ClusterTarget,
    /// `total_cost_turns` from the baseline FrictionList.
    pub baseline_cost: u32,
    /// `total_cost_turns` from the after FrictionList.
    pub after_cost: u32,
    /// `(baseline - after) / baseline * 100`, rounded to two decimals.
    /// `None` when undefined: `baseline_cost == 0`.
    pub delta_pct: Option<f64>,
    /// Verdict from comparing `delta_pct` (or the boundary cases)
    /// against `target`.
    pub verdict: RebenchVerdict,
    /// Set when the row counts as a regression (cost went up, or a
    /// new cluster appeared in after).
    pub regression_flag: Option<RegressionFlag>,
}

/// What the iteration policy decided for one cluster after seeing the
/// delta row. Consumed by the lead to drive T4.2 re-runs.
///
/// Not `Eq` because `TargetMissed::measured_delta_pct` and
/// `target_pct` carry `f64`s. Tests compare with `PartialEq` only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IterationTrigger {
    /// Re-open this specific fix-task child bone; its target was not
    /// met. Carries the measured `delta_pct` for the bone comment.
    TargetMissed {
        fix_task_bone: String,
        attribution: MawVerbAttribution,
        measured_delta_pct: Option<f64>,
        target_pct: f64,
    },
    /// Cluster regressed (cost rose or cluster is brand new in
    /// after-run); needs a fresh bone or a re-open of the relevant
    /// child. Carries enough to compose the bone comment.
    NewClusterRegression {
        attribution: MawVerbAttribution,
        baseline_cost: u32,
        after_cost: u32,
    },
}

/// The T4.3 deliverable artifact. JSON peer of
/// `notes/sg4-fix-deltas.md`.
///
/// # Stability
///
/// Pinned by [`DELTA_REPORT_SCHEMA_VERSION`] and the
/// `delta_report_schema_is_pinned` test. The SG5 (release-readiness)
/// consumer reads this struct's JSON form to decide if SG4 has cleared
/// its stop condition; any field rename or removal MUST bump the
/// version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeltaReport {
    /// = [`DELTA_REPORT_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Per-cluster delta rows, ordered by the T4.1 backlog (rank
    /// DESC by baseline cost; ties by stable `MawVerbAttribution::ALL`
    /// order). NEW clusters in the after-run appear at the end in
    /// stable variant order.
    pub rows: Vec<DeltaReportRow>,
    /// Iteration triggers (per-cluster). Empty when every cluster met
    /// target and no regressions surfaced.
    pub iteration_triggers: Vec<IterationTrigger>,
    /// `total_unattributed_wasted_turns` delta: `(baseline, after)`.
    /// Surfaced as a top-level pair (NOT a cluster row) per the T2.8
    /// "unattributed bucket always surfaced" rule. The doc renders the
    /// growth-rate per the T4.1 footnote ("growth >20% blocks the SG4
    /// stop-condition independent of cluster reductions").
    pub unattributed: UnattributedDelta,
    /// Provenance: where the inputs came from.
    pub baseline_artifact: String,
    pub after_artifact: String,
    /// Pilot vs production-data flag. `true` when either input is a
    /// synthetic-demo / pilot FrictionList; `false` when both are
    /// real-LLM campaign outputs. Surfaced explicitly so a reader
    /// cannot mistake pilot numbers for publication numbers.
    pub is_pilot: bool,
    /// ISO-8601 UTC timestamp the report was generated at. Set by
    /// the binary; tests pass a pinned value.
    pub generated_at_utc: String,
}

/// Unattributed-bucket delta pair surfaced at top level (NOT as a
/// cluster row).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnattributedDelta {
    pub baseline_count: u32,
    pub after_count: u32,
}

impl UnattributedDelta {
    /// `(after - baseline) / baseline * 100`, rounded to two decimals.
    /// `None` when `baseline_count == 0` (growth is undefined).
    #[must_use]
    pub fn growth_pct(&self) -> Option<f64> {
        if self.baseline_count == 0 {
            None
        } else {
            let b = f64::from(self.baseline_count);
            let a = f64::from(self.after_count);
            Some(round2((a - b) / b * 100.0))
        }
    }

    /// True iff the T4.1 footnote's "growth > 20% blocks SG4
    /// stop-condition" rule fires.
    #[must_use]
    pub fn blocks_sg4(&self) -> bool {
        self.growth_pct().is_some_and(|g| g > 20.0)
    }
}

impl DeltaReport {
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

/// One entry in the T4.1 backlog wiring: which fix-task child bone
/// owns which cluster, and which target policy it carries.
///
/// Encoded literally from `notes/sg4-fix-backlog.md` table. The
/// `sg4-rebench` binary builds this from the static
/// [`sg4_backlog`] function; tests can pass their own to plant
/// missed-target / regression scenarios.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BacklogEntry {
    pub attribution: MawVerbAttribution,
    pub fix_task_bone: String,
    pub target: ClusterTarget,
}

/// Static T4.1 backlog: the 7 fix-task children plus the
/// vocabulary-scarcity inclusion rule. Order matches
/// `notes/sg4-fix-backlog.md` ranking.
#[must_use]
pub fn sg4_backlog() -> Vec<BacklogEntry> {
    use MawVerbAttribution as A;
    vec![
        BacklogEntry {
            attribution: A::WsMergeStructuredConflict,
            fix_task_bone: "bn-yyx".to_string(),
            target: ClusterTarget::default_reduce_50_pct(),
        },
        BacklogEntry {
            attribution: A::WsSyncStaleWorkspace,
            fix_task_bone: "bn-221b".to_string(),
            target: ClusterTarget::default_reduce_50_pct(),
        },
        BacklogEntry {
            attribution: A::EpochSyncRequired,
            fix_task_bone: "bn-1ieb".to_string(),
            target: ClusterTarget::default_reduce_50_pct(),
        },
        BacklogEntry {
            attribution: A::VocabularyScarcity,
            fix_task_bone: "bn-1t17".to_string(),
            // T4.1 rule: if baseline cost > 0, treat as 50%; if cost=0,
            // treat as "stays 0". The diff() resolver below promotes
            // a ReducePct → StaysZero when the baseline is 0.
            target: ClusterTarget::default_reduce_50_pct(),
        },
        BacklogEntry {
            attribution: A::WsRecoverInvoked,
            fix_task_bone: "bn-29fi".to_string(),
            target: ClusterTarget::default_reduce_50_pct(),
        },
        BacklogEntry {
            attribution: A::WsDestroyRefused,
            fix_task_bone: "bn-c6l3".to_string(),
            // T4.1 row: "practical: reaches 0" (baseline=1).
            target: ClusterTarget::ReachZero,
        },
        BacklogEntry {
            attribution: A::ReadFromStaleWorkspace,
            fix_task_bone: "bn-242l".to_string(),
            target: ClusterTarget::ReachZero,
        },
    ]
}

/// Decide per-cluster pass/fail given `(baseline_cost, after_cost,
/// target)`. Returns `(verdict, regression_flag, delta_pct)`.
///
/// Boundary cases:
///
/// - `baseline == 0` AND `after == 0` → `TargetMet` (StaysZero rule
///   trivially holds; ReducePct/ReachZero also trivially holds);
///   `delta_pct = None`.
/// - `baseline == 0` AND `after > 0` → `Regressed`,
///   `NewClusterInAfter`, `delta_pct = None`.
/// - `baseline > 0` AND `after > baseline` → `Regressed`,
///   `CostIncreased`, `delta_pct < 0`.
/// - Otherwise: compute `delta_pct = (baseline - after) / baseline *
///   100`; compare against the target.
#[must_use]
pub fn evaluate_row(
    baseline_cost: u32,
    after_cost: u32,
    target: ClusterTarget,
) -> (RebenchVerdict, Option<RegressionFlag>, Option<f64>) {
    if baseline_cost == 0 && after_cost == 0 {
        return (RebenchVerdict::TargetMet, None, None);
    }
    if baseline_cost == 0 && after_cost > 0 {
        return (
            RebenchVerdict::Regressed,
            Some(RegressionFlag::NewClusterInAfter),
            None,
        );
    }
    let b = f64::from(baseline_cost);
    let a = f64::from(after_cost);
    let delta = round2((b - a) / b * 100.0);
    if after_cost > baseline_cost {
        return (
            RebenchVerdict::Regressed,
            Some(RegressionFlag::CostIncreased),
            Some(delta),
        );
    }
    let met = match target {
        ClusterTarget::ReducePct { min_reduction_pct } => delta >= min_reduction_pct,
        ClusterTarget::ReachZero | ClusterTarget::StaysZero => after_cost == 0,
    };
    let verdict = if met {
        RebenchVerdict::TargetMet
    } else {
        RebenchVerdict::TargetMissed
    };
    (verdict, None, Some(delta))
}

/// Build per-cluster cost maps from a FrictionList. Helper for
/// [`diff_friction_lists`]; exposed for tests.
#[must_use]
pub fn cluster_cost_map(list: &FrictionList) -> BTreeMap<MawVerbAttribution, u32> {
    list.ranked_clusters
        .iter()
        .map(|c| (c.attribution, c.total_cost_turns))
        .collect()
}

/// Compute the per-cluster delta report from a `(baseline, after)`
/// FrictionList pair, using the supplied T4.1 backlog wiring.
///
/// **Pure.** Same inputs → same `DeltaReport` (modulo the caller-
/// supplied `generated_at_utc` and `is_pilot` flag).
///
/// Steps:
///
/// 1. Build per-cluster cost maps from each FrictionList.
/// 2. For each backlog entry, look up baseline and after costs
///    (defaulting to 0 when the cluster is absent). Promote
///    `ReducePct → StaysZero` when the baseline is 0 AND the target
///    is `ReducePct` (T4.1 vocabulary-scarcity rule).
/// 3. Run [`evaluate_row`] to get verdict + delta.
/// 4. Emit an iteration trigger for each TargetMissed / Regressed row.
/// 5. Append rows for ANY clusters in `after` that aren't in the
///    backlog (the "new cluster appeared post-hardening" case);
///    these always fire a `NewClusterRegression` trigger.
/// 6. Stamp the unattributed-bucket delta at top level.
#[must_use]
pub fn diff_friction_lists(
    baseline: &FrictionList,
    after: &FrictionList,
    backlog: &[BacklogEntry],
    baseline_artifact: &str,
    after_artifact: &str,
    is_pilot: bool,
    generated_at_utc: &str,
) -> DeltaReport {
    let baseline_costs = cluster_cost_map(baseline);
    let after_costs = cluster_cost_map(after);
    let mut rows = Vec::new();
    let mut triggers = Vec::new();
    let mut covered: BTreeSet<MawVerbAttribution> = BTreeSet::new();

    // (1+2+3+4) Walk the backlog in order.
    for entry in backlog {
        covered.insert(entry.attribution);
        let baseline_cost = baseline_costs.get(&entry.attribution).copied().unwrap_or(0);
        let after_cost = after_costs.get(&entry.attribution).copied().unwrap_or(0);
        // T4.1 vocabulary-scarcity promotion: if the entry's target is
        // ReducePct but the baseline is 0, fall back to StaysZero.
        let resolved_target = match entry.target {
            ClusterTarget::ReducePct { .. } if baseline_cost == 0 => ClusterTarget::StaysZero,
            other => other,
        };
        let (verdict, regression_flag, delta_pct) =
            evaluate_row(baseline_cost, after_cost, resolved_target);
        // Trigger emission.
        match (verdict, regression_flag) {
            (RebenchVerdict::TargetMissed, _) => {
                triggers.push(IterationTrigger::TargetMissed {
                    fix_task_bone: entry.fix_task_bone.clone(),
                    attribution: entry.attribution,
                    measured_delta_pct: delta_pct,
                    target_pct: match resolved_target {
                        ClusterTarget::ReducePct { min_reduction_pct } => min_reduction_pct,
                        // "reaches 0" / "stays 0" surface as 100.0 for the
                        // comment-rendering side; the verdict is what
                        // matters operationally.
                        ClusterTarget::ReachZero | ClusterTarget::StaysZero => 100.0,
                    },
                });
            }
            (RebenchVerdict::Regressed, _) => {
                triggers.push(IterationTrigger::NewClusterRegression {
                    attribution: entry.attribution,
                    baseline_cost,
                    after_cost,
                });
            }
            (RebenchVerdict::TargetMet, _) => {}
        }
        rows.push(DeltaReportRow {
            attribution: entry.attribution,
            fix_task_bone: entry.fix_task_bone.clone(),
            target: resolved_target,
            baseline_cost,
            after_cost,
            delta_pct,
            verdict,
            regression_flag,
        });
    }

    // (5) NEW clusters in `after` not covered by the backlog. Walk
    // MawVerbAttribution::ALL for stable order.
    for &att in MawVerbAttribution::ALL {
        if covered.contains(&att) {
            continue;
        }
        let after_cost = after_costs.get(&att).copied().unwrap_or(0);
        let baseline_cost = baseline_costs.get(&att).copied().unwrap_or(0);
        if after_cost == 0 && baseline_cost == 0 {
            // Genuinely absent from both — don't add a row.
            continue;
        }
        // Cluster appeared (or rose) without a backlog wiring.
        let target = ClusterTarget::StaysZero;
        let (verdict, regression_flag, delta_pct) = evaluate_row(baseline_cost, after_cost, target);
        triggers.push(IterationTrigger::NewClusterRegression {
            attribution: att,
            baseline_cost,
            after_cost,
        });
        rows.push(DeltaReportRow {
            attribution: att,
            fix_task_bone: String::new(),
            target,
            baseline_cost,
            after_cost,
            delta_pct,
            verdict,
            regression_flag,
        });
    }

    let unattributed = UnattributedDelta {
        baseline_count: baseline.total_unattributed_wasted_turns,
        after_count: after.total_unattributed_wasted_turns,
    };

    DeltaReport {
        schema_version: DELTA_REPORT_SCHEMA_VERSION,
        rows,
        iteration_triggers: triggers,
        unattributed,
        baseline_artifact: baseline_artifact.to_string(),
        after_artifact: after_artifact.to_string(),
        is_pilot,
        generated_at_utc: generated_at_utc.to_string(),
    }
}

/// Render the [`DeltaReport`] as the human-readable Markdown
/// `notes/sg4-fix-deltas.md` deliverable.
#[must_use]
pub fn render_delta_report_md(report: &DeltaReport) -> String {
    let mut out = String::new();
    render_md_header(&mut out, report);
    render_md_rows(&mut out, report);
    render_md_unattributed(&mut out, report);
    render_md_triggers(&mut out, report);
    render_md_renegotiation_template(&mut out, report);
    out
}

fn render_md_header(out: &mut String, report: &DeltaReport) {
    let _ = writeln!(out, "# SG4 fix-deltas report (T4.3 / bn-1qty)");
    out.push('\n');
    if report.is_pilot {
        let _ = writeln!(
            out,
            "> **PILOT — synthetic data, harness validation only.**  Numbers below come"
        );
        let _ = writeln!(
            out,
            "> from `just sg4-rebench-pilot` (MockAgent + planted FrictionList pair). They"
        );
        let _ = writeln!(
            out,
            "> are HARNESS-ONLY per pre-reg §3.1; the real production-grade re-bench replaces"
        );
        let _ = writeln!(out, "> the rows when the real-LLM after-run lands.");
    } else {
        let _ = writeln!(
            out,
            "> **PRODUCTION DATA.**  Numbers below come from a real-LLM campaign artifact pair."
        );
    }
    out.push('\n');
    let _ = writeln!(
        out,
        "**Schema:** v{}   **Baseline:** `{}`   **After:** `{}`   **Generated:** `{}`",
        report.schema_version,
        report.baseline_artifact,
        report.after_artifact,
        report.generated_at_utc,
    );
    out.push('\n');
    let _ = writeln!(
        out,
        "_Per-cluster verdicts only. Each row carries its own target axis (T4.1 hard rule); no cross-axis aggregation._"
    );
    out.push('\n');
}

fn render_md_rows(out: &mut String, report: &DeltaReport) {
    let _ = writeln!(out, "## Per-cluster delta table");
    out.push('\n');
    let _ = writeln!(
        out,
        "| Bone | Cluster | Baseline cost | After cost | Δ % | Target | Verdict |"
    );
    let _ = writeln!(out, "|---|---|---:|---:|---:|---|---|");
    for row in &report.rows {
        let bone = if row.fix_task_bone.is_empty() {
            "_(no backlog wiring)_".to_string()
        } else {
            format!("`{}`", row.fix_task_bone)
        };
        let delta = match row.delta_pct {
            None => "n/a".to_string(),
            Some(v) => format!("{v:.2}"),
        };
        let target = match row.target {
            ClusterTarget::ReducePct { min_reduction_pct } => {
                format!("≥ {min_reduction_pct:.0}% reduction")
            }
            ClusterTarget::ReachZero => "reaches 0".to_string(),
            ClusterTarget::StaysZero => "stays 0".to_string(),
        };
        let verdict = match row.verdict {
            RebenchVerdict::TargetMet => "MET".to_string(),
            RebenchVerdict::TargetMissed => "MISSED".to_string(),
            RebenchVerdict::Regressed => {
                let why = match row.regression_flag {
                    Some(RegressionFlag::NewClusterInAfter) => "new cluster",
                    Some(RegressionFlag::CostIncreased) => "cost rose",
                    None => "regressed",
                };
                format!("REGRESSED ({why})")
            }
        };
        let _ = writeln!(
            out,
            "| {bone} | `{}` | {} | {} | {delta} | {target} | {verdict} |",
            row.attribution.slug(),
            row.baseline_cost,
            row.after_cost,
        );
    }
    out.push('\n');
}

fn render_md_unattributed(out: &mut String, report: &DeltaReport) {
    let _ = writeln!(out, "## Unattributed bucket delta");
    out.push('\n');
    let _ = writeln!(
        out,
        "- Baseline `total_unattributed_wasted_turns` = **{}**",
        report.unattributed.baseline_count,
    );
    let _ = writeln!(
        out,
        "- After `total_unattributed_wasted_turns` = **{}**",
        report.unattributed.after_count,
    );
    match report.unattributed.growth_pct() {
        None => {
            let _ = writeln!(out, "- Growth: _undefined (baseline = 0)_",);
        }
        Some(g) => {
            let blocker = if report.unattributed.blocks_sg4() {
                "  **(T4.1 footnote: growth > 20% BLOCKS SG4 stop-condition.)**"
            } else {
                ""
            };
            let _ = writeln!(out, "- Growth: **{g:.2}%**{blocker}");
        }
    }
    let _ = writeln!(
        out,
        "_Flagged for human coding follow-up per `notes/sg2-benchmark-preregistration.md` §6.3._"
    );
    out.push('\n');
}

fn render_md_triggers(out: &mut String, report: &DeltaReport) {
    let _ = writeln!(out, "## Iteration triggers (re-open T4.2 children)");
    out.push('\n');
    if report.iteration_triggers.is_empty() {
        let _ = writeln!(
            out,
            "_No triggers — every cluster met target and no regressions surfaced._"
        );
        out.push('\n');
        return;
    }
    let _ = writeln!(
        out,
        "Each row below names a specific fix-task child bone the lead should re-open."
    );
    let _ = writeln!(
        out,
        "Per the T4.1 hard rule, decisions are PER-CLUSTER — no overall composite re-open."
    );
    out.push('\n');
    for trig in &report.iteration_triggers {
        match trig {
            IterationTrigger::TargetMissed {
                fix_task_bone,
                attribution,
                measured_delta_pct,
                target_pct,
            } => {
                let measured = match measured_delta_pct {
                    None => "n/a".to_string(),
                    Some(v) => format!("{v:.2}%"),
                };
                let _ = writeln!(
                    out,
                    "- `{fix_task_bone}` (`{}`): measured Δ = {measured}, target ≥ {target_pct:.0}%. **Re-open.**",
                    attribution.slug(),
                );
            }
            IterationTrigger::NewClusterRegression {
                attribution,
                baseline_cost,
                after_cost,
            } => {
                let _ = writeln!(
                    out,
                    "- `{}`: REGRESSION (baseline={baseline_cost}, after={after_cost}). **Open new fix-task bone or re-open the relevant child.**",
                    attribution.slug(),
                );
            }
        }
    }
    out.push('\n');
}

fn render_md_renegotiation_template(out: &mut String, report: &DeltaReport) {
    let _ = writeln!(
        out,
        "## Renegotiated targets (template; populate only when a target is sound but missed)"
    );
    out.push('\n');
    if report.is_pilot {
        let _ = writeln!(
            out,
            "_PILOT: no renegotiation in pilot (synthetic data). Section is a TEMPLATE only._"
        );
        out.push('\n');
    }
    let _ = writeln!(
        out,
        "When a target is missed and the measurement is sound, document the renegotiation here."
    );
    let _ = writeln!(out, "Required fields per renegotiation entry:");
    out.push('\n');
    let _ = writeln!(
        out,
        "- **Cluster + fix-task bone:** e.g. `ws_destroy_refused` / `bn-c6l3`."
    );
    let _ = writeln!(out, "- **Original target:** e.g. `reaches 0`.");
    let _ = writeln!(out, "- **Measured delta:** e.g. `40% reduction (1 → 0.6)`.");
    let _ = writeln!(
        out,
        "- **Renegotiated target:** e.g. `≥ 40% reduction at the next bench`."
    );
    let _ = writeln!(
        out,
        "- **Rationale:** what structural blocker prevents closing the remaining gap."
    );
    let _ = writeln!(
        out,
        "- **Tracking bone:** ID of the new bone capturing the structural follow-up."
    );
    out.push('\n');
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attribution::DiagnosticBundle;
    use crate::friction_list::{FrictionSource, SweepRunRef, friction_list_from_bundles};

    fn sweep_run(n: u32) -> SweepRunRef {
        SweepRunRef {
            artifact_dir: "test://in-memory".to_string(),
            sweep_summary_ref: String::new(),
            bundle_count: n,
        }
    }

    fn bundle(
        run_id: &str,
        attrs: &[(MawVerbAttribution, u32)],
        unattributed: u32,
    ) -> DiagnosticBundle {
        let mut counts: BTreeMap<MawVerbAttribution, u32> = BTreeMap::new();
        for (a, n) in attrs {
            counts.insert(*a, *n);
        }
        let evidence: BTreeMap<MawVerbAttribution, Vec<String>> = BTreeMap::new();
        DiagnosticBundle::from_counts("maw", run_id, &counts, &evidence, unattributed)
    }

    fn make_list(bundles: &[DiagnosticBundle]) -> FrictionList {
        let n = bundles.len() as u32;
        friction_list_from_bundles(
            bundles,
            sweep_run(n),
            FrictionSource::FirstPassClassifier,
            "2026-05-25T00:00:00Z",
            "test-sha",
        )
    }

    // ---------- evaluate_row boundary cases ----------

    #[test]
    fn evaluate_target_met_on_50_pct_reduction() {
        let target = ClusterTarget::ReducePct {
            min_reduction_pct: 50.0,
        };
        let (v, flag, delta) = evaluate_row(10, 5, target);
        assert_eq!(v, RebenchVerdict::TargetMet);
        assert!(flag.is_none());
        assert_eq!(delta, Some(50.0));
    }

    #[test]
    fn evaluate_target_missed_just_below_threshold() {
        let target = ClusterTarget::ReducePct {
            min_reduction_pct: 50.0,
        };
        // baseline=10, after=6 → 40% reduction → MISSED.
        let (v, flag, delta) = evaluate_row(10, 6, target);
        assert_eq!(v, RebenchVerdict::TargetMissed);
        assert!(flag.is_none());
        assert_eq!(delta, Some(40.0));
    }

    #[test]
    fn evaluate_regressed_when_cost_rose() {
        let target = ClusterTarget::ReducePct {
            min_reduction_pct: 50.0,
        };
        // baseline=2, after=3 → cost rose.
        let (v, flag, delta) = evaluate_row(2, 3, target);
        assert_eq!(v, RebenchVerdict::Regressed);
        assert_eq!(flag, Some(RegressionFlag::CostIncreased));
        assert_eq!(delta, Some(-50.0));
    }

    #[test]
    fn evaluate_new_cluster_when_baseline_zero_after_nonzero() {
        let target = ClusterTarget::StaysZero;
        let (v, flag, delta) = evaluate_row(0, 4, target);
        assert_eq!(v, RebenchVerdict::Regressed);
        assert_eq!(flag, Some(RegressionFlag::NewClusterInAfter));
        assert_eq!(delta, None);
    }

    #[test]
    fn evaluate_stays_zero_passes_when_both_zero() {
        let target = ClusterTarget::StaysZero;
        let (v, flag, delta) = evaluate_row(0, 0, target);
        assert_eq!(v, RebenchVerdict::TargetMet);
        assert!(flag.is_none());
        assert_eq!(delta, None);
    }

    #[test]
    fn evaluate_reach_zero_passes_only_at_zero_after() {
        let target = ClusterTarget::ReachZero;
        let (v_zero, _, _) = evaluate_row(1, 0, target);
        assert_eq!(v_zero, RebenchVerdict::TargetMet);
        // Same baseline, after=1 (no reduction at all → still misses
        // because target is ReachZero, not ReducePct).
        let (v_one, _, _) = evaluate_row(2, 1, target);
        assert_eq!(v_one, RebenchVerdict::TargetMissed);
    }

    // ---------- diff_friction_lists end-to-end on planted data ----------

    #[test]
    fn diff_pilot_targets_all_met() {
        // Baseline matches T2.8 pilot synthetic ranking.
        let baseline = make_list(&[bundle(
            "b-r01",
            &[
                (MawVerbAttribution::WsMergeStructuredConflict, 9),
                (MawVerbAttribution::WsSyncStaleWorkspace, 3),
                (MawVerbAttribution::EpochSyncRequired, 3),
                (MawVerbAttribution::VocabularyScarcity, 3),
                (MawVerbAttribution::WsRecoverInvoked, 2),
                (MawVerbAttribution::WsDestroyRefused, 1),
                (MawVerbAttribution::ReadFromStaleWorkspace, 1),
            ],
            5,
        )]);
        // After: every cluster reduced by 50% or to 0.
        let after = make_list(&[bundle(
            "a-r01",
            &[
                (MawVerbAttribution::WsMergeStructuredConflict, 4),
                (MawVerbAttribution::WsSyncStaleWorkspace, 1),
                (MawVerbAttribution::EpochSyncRequired, 1),
                (MawVerbAttribution::VocabularyScarcity, 1),
                (MawVerbAttribution::WsRecoverInvoked, 1),
                // ws_destroy_refused: 1 → 0 (ReachZero).
                // read_from_stale_workspace: 1 → 0 (ReachZero).
            ],
            4,
        )]);
        let report = diff_friction_lists(
            &baseline,
            &after,
            &sg4_backlog(),
            "baseline://pilot",
            "after://pilot",
            true,
            "2026-05-25T00:00:00Z",
        );
        // Every backlog row present, in order.
        assert_eq!(report.rows.len(), 7);
        for row in &report.rows {
            assert_eq!(
                row.verdict,
                RebenchVerdict::TargetMet,
                "row {:?} unexpectedly failed; delta={:?}",
                row.attribution,
                row.delta_pct
            );
        }
        // No triggers fired.
        assert!(
            report.iteration_triggers.is_empty(),
            "{:#?}",
            report.iteration_triggers
        );
        // Unattributed went down 5 → 4: growth = -20%. Does NOT block SG4.
        assert!(!report.unattributed.blocks_sg4());
    }

    #[test]
    fn diff_planted_missed_target_fires_iteration_trigger() {
        // ws_merge_structured_conflict misses 50% reduction (9 → 7
        // ~= 22.2% reduction).
        let baseline = make_list(&[bundle(
            "b",
            &[(MawVerbAttribution::WsMergeStructuredConflict, 9)],
            0,
        )]);
        let after = make_list(&[bundle(
            "a",
            &[(MawVerbAttribution::WsMergeStructuredConflict, 7)],
            0,
        )]);
        let report = diff_friction_lists(
            &baseline,
            &after,
            &sg4_backlog(),
            "b",
            "a",
            true,
            "2026-05-25T00:00:00Z",
        );
        // First row corresponds to ws_merge_structured_conflict.
        let merge_row = &report.rows[0];
        assert_eq!(
            merge_row.attribution,
            MawVerbAttribution::WsMergeStructuredConflict
        );
        assert_eq!(merge_row.verdict, RebenchVerdict::TargetMissed);
        // Triggers must include this bone.
        let mut found = false;
        for t in &report.iteration_triggers {
            if let IterationTrigger::TargetMissed { fix_task_bone, .. } = t
                && fix_task_bone == "bn-yyx"
            {
                found = true;
            }
        }
        assert!(
            found,
            "expected a TargetMissed trigger for bn-yyx; got {:#?}",
            report.iteration_triggers
        );
    }

    #[test]
    fn diff_new_cluster_in_after_fires_regression_trigger() {
        // Baseline: only WsMergeStructuredConflict at 9. After: same
        // merge cluster down to 4 (passes), PLUS a brand-new
        // ReadFromConflictedWorkspace cluster at 2 (no backlog entry).
        let baseline = make_list(&[bundle(
            "b",
            &[(MawVerbAttribution::WsMergeStructuredConflict, 9)],
            0,
        )]);
        let after = make_list(&[bundle(
            "a",
            &[
                (MawVerbAttribution::WsMergeStructuredConflict, 4),
                (MawVerbAttribution::ReadFromConflictedWorkspace, 2),
            ],
            0,
        )]);
        let report = diff_friction_lists(
            &baseline,
            &after,
            &sg4_backlog(),
            "b",
            "a",
            true,
            "2026-05-25T00:00:00Z",
        );
        // 7 backlog rows + 1 NEW cluster row.
        assert_eq!(report.rows.len(), 8);
        // Last row is the new cluster.
        let new_row = report.rows.last().unwrap();
        assert_eq!(
            new_row.attribution,
            MawVerbAttribution::ReadFromConflictedWorkspace
        );
        assert_eq!(new_row.verdict, RebenchVerdict::Regressed);
        assert_eq!(
            new_row.regression_flag,
            Some(RegressionFlag::NewClusterInAfter)
        );
        // Trigger fired.
        let mut found = false;
        for t in &report.iteration_triggers {
            if let IterationTrigger::NewClusterRegression { attribution, .. } = t
                && *attribution == MawVerbAttribution::ReadFromConflictedWorkspace
            {
                found = true;
            }
        }
        assert!(found);
    }

    #[test]
    fn diff_vocabulary_scarcity_zero_baseline_promotes_to_stays_zero() {
        // T4.1 special case: VocabularyScarcity baseline=0 → target
        // resolves to StaysZero. After=0 → MET; after=1 → REGRESSED
        // with NewClusterInAfter flag.
        let baseline = make_list(&[bundle("b", &[], 0)]);
        let after_zero = make_list(&[bundle("a0", &[], 0)]);
        let after_one = make_list(&[bundle(
            "a1",
            &[(MawVerbAttribution::VocabularyScarcity, 1)],
            0,
        )]);
        let r_zero = diff_friction_lists(
            &baseline,
            &after_zero,
            &sg4_backlog(),
            "b",
            "a0",
            true,
            "2026-05-25T00:00:00Z",
        );
        let voc_row = r_zero
            .rows
            .iter()
            .find(|r| r.attribution == MawVerbAttribution::VocabularyScarcity)
            .unwrap();
        assert_eq!(voc_row.target, ClusterTarget::StaysZero);
        assert_eq!(voc_row.verdict, RebenchVerdict::TargetMet);

        let r_one = diff_friction_lists(
            &baseline,
            &after_one,
            &sg4_backlog(),
            "b",
            "a1",
            true,
            "2026-05-25T00:00:00Z",
        );
        let voc_row = r_one
            .rows
            .iter()
            .find(|r| r.attribution == MawVerbAttribution::VocabularyScarcity)
            .unwrap();
        assert_eq!(voc_row.verdict, RebenchVerdict::Regressed);
        assert_eq!(
            voc_row.regression_flag,
            Some(RegressionFlag::NewClusterInAfter)
        );
    }

    #[test]
    fn diff_math_is_exact_to_two_decimals() {
        // Triple-check the rounding: baseline=7, after=2 → 71.43%.
        let baseline = make_list(&[bundle(
            "b",
            &[(MawVerbAttribution::WsMergeStructuredConflict, 7)],
            0,
        )]);
        let after = make_list(&[bundle(
            "a",
            &[(MawVerbAttribution::WsMergeStructuredConflict, 2)],
            0,
        )]);
        let report = diff_friction_lists(
            &baseline,
            &after,
            &sg4_backlog(),
            "b",
            "a",
            true,
            "2026-05-25T00:00:00Z",
        );
        assert_eq!(report.rows[0].delta_pct, Some(71.43));
    }

    #[test]
    fn unattributed_growth_blocker_fires_above_20_pct() {
        let u = UnattributedDelta {
            baseline_count: 10,
            after_count: 13,
        };
        assert_eq!(u.growth_pct(), Some(30.0));
        assert!(u.blocks_sg4());

        let safe = UnattributedDelta {
            baseline_count: 10,
            after_count: 11,
        };
        assert_eq!(safe.growth_pct(), Some(10.0));
        assert!(!safe.blocks_sg4());

        let drop = UnattributedDelta {
            baseline_count: 10,
            after_count: 7,
        };
        assert_eq!(drop.growth_pct(), Some(-30.0));
        assert!(!drop.blocks_sg4());
    }

    #[test]
    fn unattributed_growth_undefined_when_baseline_zero() {
        let u = UnattributedDelta {
            baseline_count: 0,
            after_count: 4,
        };
        assert_eq!(u.growth_pct(), None);
        // Cannot block; growth is undefined.
        assert!(!u.blocks_sg4());
    }

    // ---------- JSON pin + render smoke ----------

    #[test]
    fn delta_report_schema_is_pinned() {
        let baseline = make_list(&[bundle(
            "b",
            &[(MawVerbAttribution::WsMergeStructuredConflict, 4)],
            1,
        )]);
        let after = make_list(&[bundle(
            "a",
            &[(MawVerbAttribution::WsMergeStructuredConflict, 2)],
            0,
        )]);
        let report = diff_friction_lists(
            &baseline,
            &after,
            &sg4_backlog(),
            "b",
            "a",
            true,
            "2026-05-25T00:00:00Z",
        );
        let s = report.to_json().expect("ser");
        for field in [
            "schema_version",
            "rows",
            "attribution",
            "fix_task_bone",
            "target",
            "baseline_cost",
            "after_cost",
            "delta_pct",
            "verdict",
            "iteration_triggers",
            "unattributed",
            "baseline_count",
            "after_count",
            "baseline_artifact",
            "after_artifact",
            "is_pilot",
            "generated_at_utc",
        ] {
            assert!(
                s.contains(field),
                "SG5 consumer would break: missing field {field:?}\n{s}"
            );
        }
        assert_eq!(DELTA_REPORT_SCHEMA_VERSION, 1);
    }

    #[test]
    fn render_md_contains_required_sections() {
        let baseline = make_list(&[bundle(
            "b",
            &[(MawVerbAttribution::WsMergeStructuredConflict, 4)],
            1,
        )]);
        let after = make_list(&[bundle(
            "a",
            &[(MawVerbAttribution::WsMergeStructuredConflict, 1)],
            2,
        )]);
        let report = diff_friction_lists(
            &baseline,
            &after,
            &sg4_backlog(),
            "b",
            "a",
            true,
            "2026-05-25T00:00:00Z",
        );
        let md = render_delta_report_md(&report);
        assert!(md.starts_with("# SG4 fix-deltas report"));
        assert!(md.contains("PILOT"));
        assert!(md.contains("## Per-cluster delta table"));
        assert!(md.contains("## Unattributed bucket delta"));
        assert!(md.contains("## Iteration triggers"));
        assert!(md.contains("## Renegotiated targets"));
        // Composite-headline negative invariant.
        let lower = md.to_ascii_lowercase();
        for forbidden in [
            "composite",
            "overall score",
            "severity score",
            "weighted",
            "total score",
        ] {
            assert!(
                !lower.contains(forbidden),
                "renderer emitted forbidden composite token {forbidden:?}:\n{md}"
            );
        }
    }

    #[test]
    fn json_round_trip_is_stable() {
        let baseline = make_list(&[bundle(
            "b",
            &[(MawVerbAttribution::WsMergeStructuredConflict, 4)],
            1,
        )]);
        let after = make_list(&[bundle(
            "a",
            &[(MawVerbAttribution::WsMergeStructuredConflict, 1)],
            2,
        )]);
        let report = diff_friction_lists(
            &baseline,
            &after,
            &sg4_backlog(),
            "b",
            "a",
            true,
            "2026-05-25T00:00:00Z",
        );
        let s = report.to_json().unwrap();
        let back = DeltaReport::from_json(&s).unwrap();
        assert_eq!(report, back);
    }
}
