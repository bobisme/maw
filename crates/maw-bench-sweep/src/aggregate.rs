//! Pure aggregator — directory of BenchRun JSONs → [`SweepSummary`].
//!
//! # The aggregation surface
//!
//! - One [`CellAggregate`] per `(arm, condition_id, t_class)`.
//! - Each cell carries: N, per-metric **median** across replicates,
//!   plus per-metric **min/max** (the §4.1 frozen shape — never
//!   means, never IQR collapsed into a point), and a **Wilson 95%
//!   CI** on zero-event proportion cells (mirrors the T1.9
//!   `notes/sg1-soak-campaign.md` §3 zero-event reporting rule).
//! - **No composite.** No cross-axis aggregation. No across-arm
//!   number. The aggregator's job is per-cell summarization; the
//!   crossover pass is per-(arm × metric).
//!
//! # Forward-compat to BenchRun schema v2 (T2.5 / bn-1rgk)
//!
//! T2.5 plans to extend [`maw_bench::BenchRun`] with per-tool-call
//! attribution fields. The on-disk schema's `schema_version` will
//! tick to `2`. This aggregator parses tolerantly:
//!
//! - It does NOT call `BenchRun::SCHEMA_VERSION` for a hard
//!   equality check. Instead it accepts `schema_version ∈ {1, 2}`
//!   and gracefully ignores unknown extras.
//! - When a v2-only field is present, it populates the optional
//!   [`AggregateExtras`] on the cell so downstream consumers can
//!   surface attribution-based work-redone counts without a
//!   parser-version round-trip.
//! - Records with `schema_version` outside `{1, 2}` are surfaced
//!   as [`AggregateError::UnsupportedSchema`] so the analyst sees
//!   the mismatch immediately.
//!
//! # Wilson CI on zero-event cells
//!
//! The pre-reg §6.1 binds: every "0/N" proportion cell publishes
//! its Wilson 95% upper bound. The T1.9 soak campaign §3.2
//! standardized the one-sided phrasing as `[0.0, U]`. We compute U
//! via the closed-form Wilson formula
//! [`wilson_score_upper`] (no normal-approximation; exact at
//! p=0). For non-zero observation counts we still emit a Wilson
//! lower bound (`[L, U]`) so the renderer's narrative stays
//! consistent.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use maw_bench::run::BenchRun;
use maw_bench_metrics::{MetricRecord, MetricValue, extract_metrics};

/// Schema versions this aggregator knows about. v1 is the T2.4
/// shipping schema; v2 is the T2.5 extension (forward-compat).
pub const SUPPORTED_SCHEMA_VERSIONS: &[u32] = &[1, 2];

/// Aggregator errors. Per-file parse errors are returned eagerly —
/// the aggregator is a load-bearing pipeline step, not a tolerant
/// reader; a malformed BenchRun should stop the run, not silently
/// drop the cell.
#[derive(Debug, thiserror::Error)]
pub enum AggregateError {
    /// I/O error reading the artifact directory.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Could not parse a BenchRun JSON into a usable form.
    #[error("parse {path}: {source}")]
    Parse {
        /// Path to the offending file.
        path: PathBuf,
        /// Underlying serde error.
        #[source]
        source: serde_json::Error,
    },
    /// A BenchRun's `schema_version` was outside the supported set.
    #[error("unsupported schema_version {got} at {path} (supported: {supported:?})")]
    UnsupportedSchema {
        /// Path to the offending file.
        path: PathBuf,
        /// What the file declared.
        got: u32,
        /// What we know how to parse.
        supported: &'static [u32],
    },
}

/// Optional extras filled when a v2 BenchRun is loaded. Today these
/// are all `None` because v2 has not shipped; the field set is
/// what we expect T2.5 to add (per
/// `notes/sg2-metric-definitions.md` §Downstream constraints).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AggregateExtras {
    /// Per-verb attributed work-redone count if the BenchRun
    /// included T2.5's `ToolCall::attributed_outcome` field.
    /// `None` when consuming v1 records or when the v2 record
    /// lacked the attribution (per-call optional even in v2).
    pub attributed_work_redone_turns: Option<u64>,
}

/// The composite key for a sweep cell. Stable iteration order
/// (BTreeMap-friendly).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CellKey {
    /// Arm under test.
    pub arm: String,
    /// `C0..C4` (or empty when a record omits it — invariant: real
    /// sweep runs carry it).
    pub condition_id: String,
    /// `T0..T5` (or empty).
    pub t_class: String,
}

/// One-sided Wilson 95% interval (point estimate + bounds).
///
/// All three numbers in [0, 1]. The point estimate is the
/// observed proportion (`k / n`); `lower`/`upper` are the two-sided
/// Wilson 95% interval bounds (use `upper` as the headline at
/// zero-event cells).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WilsonCi {
    /// Observed numerator (k events).
    pub k: u64,
    /// Observed denominator (N runs).
    pub n: u64,
    /// `k / n` (or 0 when n = 0).
    pub point_estimate: f64,
    /// Wilson 95% lower bound.
    pub lower: f64,
    /// Wilson 95% upper bound.
    pub upper: f64,
}

impl WilsonCi {
    /// Render the standard `[L, U]` form used in the publication.
    /// Zero-event cells produce `0.000 [0.0, U]`.
    #[must_use]
    pub fn format(&self) -> String {
        format!(
            "{:.3} [{:.3}, {:.3}]",
            self.point_estimate, self.lower, self.upper
        )
    }
}

/// One cell's aggregate. Held in [`SweepSummary::cells`] keyed by
/// [`CellKey`].
///
/// # What's here vs. what isn't
///
/// - **Here:** per-metric median, min/max, Wilson CI on the
///   zero-event correctness proportions (`work_lost_events` rate
///   and `human_intervention_events` rate when measured),
///   replicate count, optional v2 extras.
/// - **Not here:** any cross-arm comparison, any composite, any
///   "winner" verdict. Those are downstream concerns (crossover
///   pass + renderer).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CellAggregate {
    /// Number of completed runs in this cell.
    pub n: u64,
    /// Per-metric median value (the §4.1 frozen reporting unit).
    pub median: BTreeMap<String, MetricValue>,
    /// Per-metric min (the "(lo–hi)" footprint).
    pub min: BTreeMap<String, MetricValue>,
    /// Per-metric max (the "(lo–hi)" footprint).
    pub max: BTreeMap<String, MetricValue>,
    /// Per-metric **raw per-replicate sum** across the cell's
    /// replicates. Added for bn-27ai / Fix A.3 so SG3's R6
    /// "interventions total" rule can compute against the actual sum
    /// rather than the lossy `median × n` proxy that integer-
    /// truncates one-bit median deltas into N×-amplified totals.
    ///
    /// `Unavailable` / `Infinite` replicates are excluded (consistent
    /// with the median/min/max convention in [`lo_med_hi`]); the sum
    /// is over the finite measured values only.
    ///
    /// Serde-`default` so older serialized summaries (pre-bn-27ai)
    /// deserialize cleanly with an empty map — `sum_proxy` falls back
    /// to `median × n` in that case.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sum: BTreeMap<String, MetricValue>,
    /// Wilson 95% CI on the `work_lost_events > 0` rate.
    /// Mirrors pre-reg §4.1 (every proportion cell carries Wilson)
    /// and T1.9 §3.1 (zero-event cells published as `[0.0, U]`).
    pub work_lost_rate_ci: WilsonCi,
    /// Forward-compat extras (T2.5 v2 schema fields).
    pub extras: AggregateExtras,
}

/// A complete sweep summary — per-cell aggregates plus the set of
/// arms and conditions observed (so the crossover pass and renderer
/// don't have to re-derive them).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SweepSummary {
    /// All cells, keyed by [`CellKey`]. BTreeMap so iteration order
    /// is stable across runs (deterministic rendered output).
    pub cells: BTreeMap<CellKey, CellAggregate>,
    /// Distinct arm names seen, in observation order — preserved so
    /// the renderer can honor the pre-reg §1.3 frozen order when
    /// it's supplied as an explicit option.
    pub arms: Vec<String>,
    /// Distinct `condition_id`s seen, in spectrum order
    /// (`C0..C4`). Sorted by id-as-string is correct here because
    /// the ids are zero-padded to a single digit.
    pub conditions: Vec<String>,
    /// Distinct `t_class`es seen.
    pub t_classes: Vec<String>,
    /// Total number of BenchRun files loaded.
    pub total_runs: u64,
}

impl SweepSummary {
    /// Lookup a cell by composite key. Returns `None` if the cell
    /// was not populated (analyst-facing rather than panic).
    #[must_use]
    pub fn cell(&self, arm: &str, condition_id: &str, t_class: &str) -> Option<&CellAggregate> {
        self.cells.get(&CellKey {
            arm: arm.to_string(),
            condition_id: condition_id.to_string(),
            t_class: t_class.to_string(),
        })
    }
}

/// Walk `artifact_dir` recursively, parse every `*.json` as a
/// BenchRun, return the loaded records. Cells are not yet
/// aggregated — that's [`aggregate_records`].
pub fn load_runs(artifact_dir: &Path) -> Result<Vec<(PathBuf, BenchRun)>, AggregateError> {
    let mut out = Vec::new();
    visit_dir(artifact_dir, &mut out)?;
    Ok(out)
}

fn visit_dir(dir: &Path, out: &mut Vec<(PathBuf, BenchRun)>) -> Result<(), AggregateError> {
    for ent in std::fs::read_dir(dir)? {
        let ent = ent?;
        let path = ent.path();
        let ftype = ent.file_type()?;
        if ftype.is_dir() {
            visit_dir(&path, out)?;
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let bytes = std::fs::read_to_string(&path)?;
        // First parse as Value so we can sniff schema_version.
        let v: Value = serde_json::from_str(&bytes).map_err(|e| AggregateError::Parse {
            path: path.clone(),
            source: e,
        })?;
        let sv = v.get("schema_version").and_then(Value::as_u64).unwrap_or(0);
        let sv = u32::try_from(sv).unwrap_or(0);
        if !SUPPORTED_SCHEMA_VERSIONS.contains(&sv) {
            return Err(AggregateError::UnsupportedSchema {
                path,
                got: sv,
                supported: SUPPORTED_SCHEMA_VERSIONS,
            });
        }
        // BenchRun deserialize: serde tolerates additional v2 fields
        // (deny_unknown_fields is not set on BenchRun), so a v2 record
        // loads cleanly into a v1 BenchRun struct — the extras live
        // in the original Value if a future consumer wants them.
        let run: BenchRun = serde_json::from_value(v).map_err(|e| AggregateError::Parse {
            path: path.clone(),
            source: e,
        })?;
        out.push((path, run));
    }
    Ok(())
}

/// Aggregate a directory of BenchRun JSONs into a [`SweepSummary`].
///
/// Entry-point convenience that wraps `load_runs` + `aggregate_records`.
pub fn aggregate_artifacts(artifact_dir: &Path) -> Result<SweepSummary, AggregateError> {
    let runs: Vec<BenchRun> = load_runs(artifact_dir)?
        .into_iter()
        .map(|(_, r)| r)
        .collect();
    Ok(aggregate_records(&runs))
}

/// Pure aggregator over an in-memory slice. Used by tests; the
/// real entry-point [`aggregate_artifacts`] composes I/O on top.
#[must_use]
pub fn aggregate_records(runs: &[BenchRun]) -> SweepSummary {
    let records: Vec<MetricRecord> = runs.iter().map(extract_metrics).collect();
    aggregate_metric_records(&records)
}

/// Aggregate a slice of already-extracted [`MetricRecord`]s.
/// Separated so tests can feed synthetic records without
/// constructing BenchRuns.
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn aggregate_metric_records(records: &[MetricRecord]) -> SweepSummary {
    let mut summary = SweepSummary::default();
    let mut by_cell: BTreeMap<CellKey, Vec<&MetricRecord>> = BTreeMap::new();
    let mut arms_seen: BTreeSet<String> = BTreeSet::new();
    let mut conds_seen: BTreeSet<String> = BTreeSet::new();
    let mut tcls_seen: BTreeSet<String> = BTreeSet::new();

    for r in records {
        let key = CellKey {
            arm: r.arm.clone(),
            condition_id: r.condition_id.clone(),
            t_class: r.t_class.clone(),
        };
        by_cell.entry(key).or_default().push(r);
        arms_seen.insert(r.arm.clone());
        conds_seen.insert(r.condition_id.clone());
        tcls_seen.insert(r.t_class.clone());
    }

    for (key, recs) in by_cell {
        let agg = summarize_cell(&recs);
        summary.cells.insert(key, agg);
    }
    summary.total_runs = records.len() as u64;
    summary.arms = arms_seen.into_iter().collect();
    summary.conditions = conds_seen.into_iter().collect();
    summary.t_classes = tcls_seen.into_iter().collect();
    summary
}

#[allow(clippy::cast_possible_truncation)]
fn summarize_cell(recs: &[&MetricRecord]) -> CellAggregate {
    let n = recs.len() as u64;
    let metric_names: Vec<&'static str> = if recs.is_empty() {
        Vec::new()
    } else {
        recs[0].axed().iter().map(|t| t.0).collect()
    };

    let mut median = BTreeMap::new();
    let mut min = BTreeMap::new();
    let mut max = BTreeMap::new();
    let mut sum = BTreeMap::new();
    for name in &metric_names {
        let (lo, mid, hi) = lo_med_hi(recs, name);
        min.insert((*name).to_string(), lo);
        median.insert((*name).to_string(), mid);
        max.insert((*name).to_string(), hi);
        sum.insert((*name).to_string(), sum_of(recs, name));
    }

    // Wilson CI on the work_lost_events > 0 rate (the headline
    // correctness proportion; mirrors pre-reg §4.1).
    let k = recs
        .iter()
        .filter(|r| matches!(r.work_lost_events, MetricValue::Count { n } if n > 0))
        .count() as u64;
    let work_lost_rate_ci = wilson_ci(k, n);

    CellAggregate {
        n,
        median,
        min,
        max,
        sum,
        work_lost_rate_ci,
        extras: AggregateExtras::default(),
    }
}

/// Compute the raw per-replicate sum of a metric across a cell's
/// records. Mirrors [`lo_med_hi`]'s handling of value variants:
///
/// - `Count`, `DurationMs`, `UsdCents`: numeric values summed
///   (saturating to guard against pathological inputs).
/// - `Infinite`: returned as `Infinite` if any replicate is infinite
///   (a single non-finite replicate makes the sum non-finite).
/// - `Unavailable`: excluded from the sum (treated as no measurement,
///   consistent with the median convention).
/// - All-`Unavailable` cells return `Unavailable`.
///
/// Added for bn-27ai / Fix A.3.
#[allow(clippy::cast_possible_truncation)]
fn sum_of(recs: &[&MetricRecord], name: &str) -> MetricValue {
    let mut total: u64 = 0;
    let mut measured: usize = 0;
    let mut kind: Option<&'static str> = None;
    for r in recs {
        match lookup(r, name) {
            MetricValue::Count { n } => {
                kind.get_or_insert("count");
                total = total.saturating_add(n);
                measured += 1;
            }
            MetricValue::DurationMs { ms } => {
                kind.get_or_insert("duration_ms");
                total = total.saturating_add(ms);
                measured += 1;
            }
            MetricValue::UsdCents { cents } => {
                kind.get_or_insert("usd_cents");
                total = total.saturating_add(cents);
                measured += 1;
            }
            MetricValue::Infinite => return MetricValue::Infinite,
            MetricValue::Unavailable => {}
        }
    }
    if measured == 0 {
        return MetricValue::Unavailable;
    }
    match kind {
        Some("duration_ms") => MetricValue::duration_ms(total),
        Some("usd_cents") => MetricValue::usd_cents(total),
        _ => MetricValue::count(total),
    }
}

/// Lower-median + (min, max) computed per the
/// `notes/sg2-metric-definitions.md` "median across runs" rule:
/// `Unavailable` is dropped, `Infinite` is sorted as the maximum.
#[allow(clippy::cast_possible_truncation)]
fn lo_med_hi(recs: &[&MetricRecord], name: &str) -> (MetricValue, MetricValue, MetricValue) {
    let mut finite: Vec<u64> = Vec::with_capacity(recs.len());
    let mut infinite_count: usize = 0;
    let mut total_measured: usize = 0;
    let mut kind: Option<&'static str> = None;
    for r in recs {
        let v = lookup(r, name);
        match v {
            MetricValue::Count { n } => {
                kind.get_or_insert("count");
                finite.push(n);
                total_measured += 1;
            }
            MetricValue::DurationMs { ms } => {
                kind.get_or_insert("duration_ms");
                finite.push(ms);
                total_measured += 1;
            }
            MetricValue::UsdCents { cents } => {
                kind.get_or_insert("usd_cents");
                finite.push(cents);
                total_measured += 1;
            }
            MetricValue::Infinite => {
                infinite_count += 1;
                total_measured += 1;
            }
            MetricValue::Unavailable => {}
        }
    }
    if total_measured == 0 {
        return (
            MetricValue::Unavailable,
            MetricValue::Unavailable,
            MetricValue::Unavailable,
        );
    }
    finite.sort_unstable();
    let n_total = finite.len() + infinite_count;
    let lower_idx = (n_total - 1) / 2;

    let val_at = |idx: usize| -> MetricValue {
        if idx < finite.len() {
            match kind {
                Some("duration_ms") => MetricValue::duration_ms(finite[idx]),
                Some("usd_cents") => MetricValue::usd_cents(finite[idx]),
                _ => MetricValue::count(finite[idx]),
            }
        } else {
            MetricValue::Infinite
        }
    };
    let lo_v = if finite.is_empty() {
        MetricValue::Infinite
    } else {
        val_at(0)
    };
    let hi_v = if infinite_count > 0 {
        MetricValue::Infinite
    } else if let Some(&last) = finite.last() {
        match kind {
            Some("duration_ms") => MetricValue::duration_ms(last),
            Some("usd_cents") => MetricValue::usd_cents(last),
            _ => MetricValue::count(last),
        }
    } else {
        MetricValue::Unavailable
    };
    let med = val_at(lower_idx);
    (lo_v, med, hi_v)
}

fn lookup(r: &MetricRecord, name: &str) -> MetricValue {
    r.axed()
        .iter()
        .find(|t| t.0 == name)
        .map_or(MetricValue::Unavailable, |t| t.1)
}

// ---------------------------------------------------------------------------
// Wilson score interval
// ---------------------------------------------------------------------------

const Z95: f64 = 1.959_963_984_540_054; // two-sided 95%

/// Closed-form Wilson 95% interval for a binomial proportion.
///
/// At `n == 0` returns a `[0, 1]` "no information" interval with
/// `point_estimate = 0`. At `k == 0` the lower bound is exactly 0;
/// the upper bound is the conservative Wilson upper bound.
///
/// We use Z = 1.959963984540054 (two-sided 95%). The bounds match
/// the §6.1 MDE table to 3 decimals (e.g. N=10, k=0 → upper ≈
/// 0.278).
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn wilson_ci(k: u64, n: u64) -> WilsonCi {
    if n == 0 {
        return WilsonCi {
            k: 0,
            n: 0,
            point_estimate: 0.0,
            lower: 0.0,
            upper: 1.0,
        };
    }
    let n_f = n as f64;
    let p = (k as f64) / n_f;
    let z2 = Z95 * Z95;
    let center = (p + z2 / (2.0 * n_f)) / (1.0 + z2 / n_f);
    let half = (Z95 / (1.0 + z2 / n_f)) * f64::sqrt(p * (1.0 - p) / n_f + z2 / (4.0 * n_f * n_f));
    let lower = (center - half).max(0.0);
    let upper = (center + half).min(1.0);
    WilsonCi {
        k,
        n,
        point_estimate: p,
        lower,
        upper,
    }
}

/// Convenience for tests/renderer: the standard zero-event upper
/// bound at N (matches pre-reg §6.1 MDE table to ≤ 0.001 abs).
#[must_use]
pub fn wilson_score_upper(n: u64) -> f64 {
    wilson_ci(0, n).upper
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(arm: &str, cond: &str, t: &str, lost: u64, turns: u64, calls: u64) -> MetricRecord {
        MetricRecord {
            schema_version: 1,
            run_id: format!("{arm}-{cond}-{t}-{lost}-{turns}-{calls}"),
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
    fn wilson_zero_event_matches_prereg_table() {
        // Pre-reg §6.1: N=10, X=0 -> Wilson upper ~ 0.278.
        let u = wilson_score_upper(10);
        assert!((u - 0.278).abs() < 0.01, "got {u}");
        // N=20, X=0 -> ~ 0.161.
        let u = wilson_score_upper(20);
        assert!((u - 0.161).abs() < 0.01, "got {u}");
        // N=100, X=0 -> ~ 0.037.
        let u = wilson_score_upper(100);
        assert!((u - 0.037).abs() < 0.01, "got {u}");
    }

    #[test]
    fn wilson_zero_event_lower_is_exactly_zero() {
        let w = wilson_ci(0, 50);
        assert_eq!(w.lower, 0.0);
        assert!(w.upper > 0.0);
        assert_eq!(w.point_estimate, 0.0);
    }

    #[test]
    fn wilson_n_zero_is_no_information_interval() {
        let w = wilson_ci(0, 0);
        assert_eq!(w.lower, 0.0);
        assert_eq!(w.upper, 1.0);
    }

    #[test]
    fn aggregate_groups_by_cell_key() {
        let recs = vec![
            rec("maw", "C0", "T0", 0, 3, 10),
            rec("maw", "C0", "T0", 0, 4, 11),
            rec("maw", "C2", "T0", 0, 6, 25),
            rec("jj-workspaces", "C2", "T0", 1, 8, 30),
        ];
        let s = aggregate_metric_records(&recs);
        assert_eq!(s.total_runs, 4);
        assert_eq!(s.cells.len(), 3);
        let c0 = s.cell("maw", "C0", "T0").expect("maw C0 cell");
        assert_eq!(c0.n, 2);
        // turns median (lower-median of [3,4]) = 3.
        assert_eq!(
            c0.median.get("turns_to_done").unwrap(),
            &MetricValue::count(3)
        );
        // min/max for tool_calls_total on the C0 cell.
        assert_eq!(
            c0.min.get("tool_calls_total").unwrap(),
            &MetricValue::count(10)
        );
        assert_eq!(
            c0.max.get("tool_calls_total").unwrap(),
            &MetricValue::count(11)
        );
    }

    #[test]
    fn zero_event_cell_publishes_wilson_upper_not_a_bare_zero() {
        // 10 maw runs, 0 work_lost_events.
        let recs: Vec<_> = (0..10)
            .map(|i| rec("maw", "C0", "T0", 0, 3 + i, 10))
            .collect();
        let s = aggregate_metric_records(&recs);
        let c = s.cell("maw", "C0", "T0").expect("cell");
        assert_eq!(c.work_lost_rate_ci.k, 0);
        assert_eq!(c.work_lost_rate_ci.n, 10);
        assert_eq!(c.work_lost_rate_ci.point_estimate, 0.0);
        assert_eq!(c.work_lost_rate_ci.lower, 0.0);
        // The Wilson UB at N=10/k=0 is non-trivial (~0.278) — the
        // headline reporting rule (§6.1) is exactly this number,
        // NOT a bare 0.
        assert!(c.work_lost_rate_ci.upper > 0.2);
        assert!(c.work_lost_rate_ci.upper < 0.3);
    }

    #[test]
    fn infinite_turns_sorts_as_max() {
        let mut recs = vec![rec("maw", "C0", "T0", 0, 3, 10)];
        let mut inf = rec("maw", "C0", "T0", 0, 0, 10);
        inf.turns_to_done = MetricValue::Infinite;
        recs.push(inf);
        recs.push(rec("maw", "C0", "T0", 0, 5, 10));
        let s = aggregate_metric_records(&recs);
        let c = s.cell("maw", "C0", "T0").unwrap();
        // sorted: [3, 5, INF]; lower-median = 5.
        assert_eq!(
            c.median.get("turns_to_done").unwrap(),
            &MetricValue::count(5)
        );
        // max is INF.
        assert_eq!(c.max.get("turns_to_done").unwrap(), &MetricValue::Infinite);
    }

    /// bn-27ai Fix A.3: `CellAggregate::sum` carries the raw
    /// per-replicate sum, not `median × n`. Demonstrates the
    /// integer-truncation bug that the old proxy had:
    /// `[0,1,1,1,0,0,1,0,1,2]` has lower-median 1, sum 7;
    /// `median × n = 10` but the true sum is 7.
    #[test]
    fn cell_aggregate_carries_raw_per_replicate_sum() {
        // Match the SG3-rerun new-layout C2-T0 fire pattern
        // (per notes/sg3-no-go-rootcause-v2.md §3): redone values
        // [0,1,1,1,0,0,1,0,1,2] across 10 reps.
        let redone_pattern = [0_u64, 1, 1, 1, 0, 0, 1, 0, 1, 2];
        let recs: Vec<MetricRecord> = redone_pattern
            .iter()
            .enumerate()
            .map(|(i, &r)| {
                let mut m = rec("maw@new-layout", "C2", "T0", 0, 8, 18);
                m.work_redone_turns = MetricValue::count(r);
                m.run_id = format!("synth-r{i:03}");
                m
            })
            .collect();
        let s = aggregate_metric_records(&recs);
        let c = s.cell("maw@new-layout", "C2", "T0").expect("cell");
        assert_eq!(c.n, 10);
        // Raw sum is 7 (matches the rerun's per-replicate sum).
        assert_eq!(
            c.sum.get("work_redone_turns").unwrap(),
            &MetricValue::count(7),
            "raw sum = 7; the pre-A.3 median×n proxy would have said 10"
        );
        // Sanity-check the median is still 1 (so the pre-A.3 proxy
        // would have been 1×10 = 10 — the headline bug).
        assert_eq!(
            c.median.get("work_redone_turns").unwrap(),
            &MetricValue::count(1)
        );
    }

    /// Sum tolerates `Unavailable` (excluded) and is sentinel-aware
    /// for `Infinite` (a single non-finite replicate makes the sum
    /// non-finite).
    #[test]
    fn cell_aggregate_sum_handles_sentinel_values() {
        // Three records: 2 finite, 1 Infinite turns_to_done.
        let mut recs = vec![rec("maw", "C0", "T0", 0, 3, 10)];
        let mut inf = rec("maw", "C0", "T0", 0, 0, 10);
        inf.turns_to_done = MetricValue::Infinite;
        recs.push(inf);
        recs.push(rec("maw", "C0", "T0", 0, 5, 10));
        let s = aggregate_metric_records(&recs);
        let c = s.cell("maw", "C0", "T0").unwrap();
        // turns_to_done sum: Infinite present → Infinite.
        assert_eq!(c.sum.get("turns_to_done").unwrap(), &MetricValue::Infinite);
        // tool_calls_total: all finite (10 + 10 + 10) = 30.
        assert_eq!(
            c.sum.get("tool_calls_total").unwrap(),
            &MetricValue::count(30)
        );
    }

    #[test]
    fn aggregate_artifacts_v1_v2_mix_loads_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        // Write a synthetic v1 BenchRun.
        let v1 = synth_benchrun_json(1);
        std::fs::write(tmp.path().join("v1.json"), v1).unwrap();
        // Write a synthetic v2 BenchRun (v1 shape + an extra field).
        let v2 = synth_benchrun_json(2);
        std::fs::write(tmp.path().join("v2.json"), v2).unwrap();
        let s = aggregate_artifacts(tmp.path()).expect("aggregate");
        assert_eq!(s.total_runs, 2);
    }

    #[test]
    fn unsupported_schema_version_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = synth_benchrun_json(99);
        std::fs::write(tmp.path().join("bad.json"), bad).unwrap();
        let err = aggregate_artifacts(tmp.path()).expect_err("must error");
        assert!(matches!(
            err,
            AggregateError::UnsupportedSchema { got: 99, .. }
        ));
    }

    /// Build a minimal BenchRun JSON for tests. The v2 variant is
    /// identical except for `schema_version` + an extra ignored
    /// field — forward-compat is "extra fields don't break loading".
    fn synth_benchrun_json(schema_version: u32) -> String {
        let extra = if schema_version >= 2 {
            r#", "v2_only_field": "placeholder""#
        } else {
            ""
        };
        format!(
            r#"{{
                "schema_version": {schema_version},
                "run_id": "synth-{schema_version}",
                "manifest": {{
                    "claude_code_version": "",
                    "claude_model_id": "",
                    "claude_effective_model": "",
                    "git_version": "",
                    "jj_version": "",
                    "maw_version": "",
                    "benchmark_harness_commit": "",
                    "scenario_generator_commit": "",
                    "prompt_hash": "",
                    "seed": 1,
                    "condition_id": "C0",
                    "t_class": "T0",
                    "arm": "maw",
                    "os_kernel": "",
                    "start_ts_unix_ms": 0,
                    "end_ts_unix_ms": 1000
                }},
                "verdict": {{"outcome": "success"}},
                "oracle_b": {{"verdict": "green"}},
                "transcript": {{
                    "prompt": "",
                    "prompt_sha256": "",
                    "convention_text": "",
                    "turns": []
                }},
                "total_tool_calls": 0,
                "total_turns": 1,
                "cost_usd": null,
                "duration_ms": 1000,
                "substrate_final_files": []{extra}
            }}"#,
        )
    }
}
