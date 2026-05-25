//! `MetricRecord` — the per-run, schema-versioned, never-composited
//! metric envelope for T2.4 (`bn-oko4`).
//!
//! The contract:
//!
//! - One record per [`maw_bench::BenchRun`].
//! - Every metric value carries its own [`Axis`] tag (`Efficiency` or
//!   `Correctness`). The renderer reads this tag and prints axes in
//!   separated blocks — no composite, ever.
//! - Schema is additive: optional new fields don't bump
//!   [`MetricRecord::SCHEMA_VERSION`]. Field removals or type changes
//!   do.
//! - Serializable — the same JSON-on-disk pattern as `BenchRun` so
//!   metric records can be cached, diffed in code review, and
//!   re-rendered without re-extracting.
//!
//! # Metric naming + axis assignment
//!
//! Names match the pre-registration (§1.1) verbatim where they exist
//! there. Two T2.4-specific metrics (`work_redone_turns` and
//! `human_intervention_events`) are pre-reg-amendments documented in
//! `notes/sg2-metric-definitions.md`:
//!
//! - `work_redone_turns` is the pre-reg `wasted_turns` count under
//!   T2.4's conservative heuristic (the human-coded version per
//!   §6.3 supersedes this when available).
//! - `human_intervention_events` is a forward-compatibility hook for
//!   future BenchRun fields; placeholder = `Unavailable` today.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

use crate::attribution::MawVerbAttribution;

/// Which axis a metric belongs to. The reporter prints axes in
/// separated blocks; never combined. The pre-registration §4
/// mandates this presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    /// Lower-is-better; **never** safety-relevant on its own.
    /// E.g. `tool_calls_total`, `turns_to_done`.
    Efficiency,
    /// Higher-is-worse; **0 is the bar**. Pre-reg §1.1.
    /// E.g. `work_lost_events`, `human_intervention_events`.
    Correctness,
}

impl fmt::Display for Axis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Efficiency => f.write_str("efficiency"),
            Self::Correctness => f.write_str("correctness"),
        }
    }
}

/// A single metric value. `Unavailable` is the explicit "we did not
/// measure this for this run" state — distinct from `Count(0)` so a
/// reader can't conflate "absent" with "zero".
///
/// `Cost` carries a separate variant because a missing `cost_usd`
/// (MockAgent, provider envelope missing the field) is a structurally
/// distinct condition from "the metric does not apply".
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MetricValue {
    /// Integer count (turns, tool calls, events). Struct variant so
    /// serde's internally-tagged representation works on `n` —
    /// newtype variants with a primitive inner type are not
    /// representable that way.
    Count {
        /// The value.
        n: u64,
    },
    /// Milliseconds (wall-clock duration).
    DurationMs {
        /// The value.
        ms: u64,
    },
    /// USD cost (provider envelope `total_cost_usd`), expressed as
    /// cents × 100 so sub-cent precision is preserved.
    UsdCents {
        /// The value.
        cents: u64,
    },
    /// Sentinel for "agent did not finish" (`turns_to_done` semantics).
    /// Rendered as `INF` by the report; never converted to a finite
    /// number for downstream math.
    Infinite,
    /// Metric not available for this run (e.g. cost missing for
    /// MockAgent, or the `human_intervention_events` future hook).
    Unavailable,
}

impl MetricValue {
    /// Construct a `Count`. Ergonomic shortcut for the struct variant.
    #[must_use]
    pub const fn count(n: u64) -> Self {
        Self::Count { n }
    }

    /// Construct a `DurationMs`.
    #[must_use]
    pub const fn duration_ms(ms: u64) -> Self {
        Self::DurationMs { ms }
    }

    /// Construct a `UsdCents` (cents × 100).
    #[must_use]
    pub const fn usd_cents(cents: u64) -> Self {
        Self::UsdCents { cents }
    }

    /// Render for the dominance table. Stable formatting — used by
    /// tests + the bin renderer.
    #[allow(clippy::cast_precision_loss)]
    pub fn format(self) -> String {
        match self {
            Self::Count { n } => n.to_string(),
            Self::DurationMs { ms } => format!("{ms}ms"),
            // Cents -> dollars with 4-decimal precision so a $0.0001
            // distinction (real for cheap agents) is visible.
            Self::UsdCents { cents } => {
                let dollars = (cents as f64) / 10_000.0;
                format!("${dollars:.4}")
            }
            Self::Infinite => "INF".to_string(),
            Self::Unavailable => "n/a".to_string(),
        }
    }
}

/// Schema-version mismatch — the on-disk record is from a different
/// `MetricRecord` schema generation than this binary expects.
#[derive(Debug, thiserror::Error)]
pub enum MetricsSchemaError {
    /// On-disk schema version doesn't match this build's
    /// [`MetricRecord::SCHEMA_VERSION`].
    #[error("metric record schema version mismatch: got {got}, expected {expected}")]
    VersionMismatch {
        /// The version we read.
        got: u32,
        /// The version this build expects.
        expected: u32,
    },
}

/// The per-run record. **One per BenchRun.** Never averaged into a
/// single score.
///
/// # Field order is the rendered order (correctness first)
///
/// Per pre-reg §4.1 the correctness/safety axis is printed first and
/// visually separated. We preserve that ordering in the struct so a
/// `serde_json` dump reads top-to-bottom in the reporter's order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricRecord {
    /// Bumped when an incompatible schema change is made.
    pub schema_version: u32,
    /// Stable run id (echoes [`maw_bench::BenchRun::run_id`]).
    pub run_id: String,
    /// Arm under test (`"maw"`, `"git-worktrees-bare"`,
    /// `"jj-workspaces"`, `"claude-native-worktrees"`). Echoes
    /// [`maw_bench::run::RunManifest::arm`].
    pub arm: String,
    /// `C0..C4` condition spectrum point (§5). Empty if the run did
    /// not carry a condition_id.
    pub condition_id: String,
    /// `T0..T5` task class. Empty if the run did not carry a t_class.
    pub t_class: String,

    // ----- correctness axis (printed first) -----
    /// Count of run-level work-loss events. Currently sourced from
    /// `OracleBSummary::Red` + `RunVerdict::SubstrateIncoherent`.
    /// See `notes/sg2-metric-definitions.md` §work_lost_events.
    pub work_lost_events: MetricValue,
    /// Forward-compatibility hook for transcript-level human
    /// intervention markers. Today: always `Unavailable`. See
    /// `notes/sg2-metric-definitions.md` §human_intervention_events.
    pub human_intervention_events: MetricValue,

    // ----- efficiency axis -----
    /// Tool-call count across every turn. Echoes
    /// [`maw_bench::BenchRun::total_tool_calls`].
    pub tool_calls_total: MetricValue,
    /// Turns the agent took to finish. `Infinite` when the verdict
    /// is not `Success`. Pre-reg §1.1.
    pub turns_to_done: MetricValue,
    /// Wall-clock duration. Recorded for completeness (pre-reg notes
    /// CV is 28.4%; explicitly NOT a headline metric).
    pub wall_duration_ms: MetricValue,
    /// Provider-reported cost. `Unavailable` for MockAgent.
    pub cost_usd: MetricValue,
    /// Turns the agent spent re-doing or recovering already-done
    /// work. As of T2.5 (schema v2) this is **attribution-driven**:
    /// counts turns whose call is attributed to a
    /// [`crate::MawVerbAttribution`] cluster (i.e. the agent retried
    /// after a `StepOutcome { conflicted: true }` and re-issued an op
    /// of the same class on the same target). Pre-T2.5 records (schema
    /// v1) used a substring heuristic.
    pub work_redone_turns: MetricValue,

    // ----- T2.5 diagnostic axis (schema v2) -----
    /// Per-verb attribution histogram. **Diagnostic axis ONLY** — never
    /// folded into a single score and never compared cross-axis.
    ///
    /// `BTreeMap` for stable serialization order (every read gets the
    /// same JSON bytes regardless of insertion order). Empty for
    /// non-maw arms (substrate has no maw verbs).
    ///
    /// New in schema v2 (T2.5 / `bn-1rgk`). Default empty so v1
    /// records still load.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub per_verb_wasted_turns: BTreeMap<MawVerbAttribution, u32>,
}

impl MetricRecord {
    /// Current schema version.
    ///
    /// # Migration history
    ///
    /// - **v1 → v2 (T2.5 / `bn-1rgk`)**: added optional
    ///   `per_verb_wasted_turns` (BTreeMap, default empty). v1 records
    ///   load cleanly into v2 with the new field empty. The schema
    ///   bumped (even though additive) so downstream tools can assert
    ///   "this record carries attribution data" rather than guess from
    ///   field presence — same migration discipline as
    ///   `BenchRun::SCHEMA_VERSION`.
    pub const SCHEMA_VERSION: u32 = 2;

    /// Return all metric (name, value, axis) triples in **rendered
    /// order** (correctness first, then efficiency). Used by the
    /// reporter and by the no-composite invariant test.
    pub fn axed(&self) -> [(&'static str, MetricValue, Axis); 7] {
        [
            ("work_lost_events", self.work_lost_events, Axis::Correctness),
            (
                "human_intervention_events",
                self.human_intervention_events,
                Axis::Correctness,
            ),
            ("tool_calls_total", self.tool_calls_total, Axis::Efficiency),
            ("turns_to_done", self.turns_to_done, Axis::Efficiency),
            (
                "wall_duration_ms",
                self.wall_duration_ms,
                Axis::Efficiency,
            ),
            ("cost_usd", self.cost_usd, Axis::Efficiency),
            (
                "work_redone_turns",
                self.work_redone_turns,
                Axis::Efficiency,
            ),
        ]
    }

    /// Decode from JSON, validating the schema version.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        let v: Self = serde_json::from_str(s)?;
        // Schema validation is a separate error path — we still parse
        // successfully and let the caller decide what to do with
        // version mismatches.
        Ok(v)
    }

    /// Verify the on-disk schema matches this build's. Distinct from
    /// `from_json` so the caller can choose to be tolerant.
    pub fn verify_schema(&self) -> Result<(), MetricsSchemaError> {
        if self.schema_version == Self::SCHEMA_VERSION {
            Ok(())
        } else {
            Err(MetricsSchemaError::VersionMismatch {
                got: self.schema_version,
                expected: Self::SCHEMA_VERSION,
            })
        }
    }

    /// Pretty-JSON serialize. Used by `sg2-report` when `--emit json`.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_version_is_two() {
        // v2 in T2.5 — adds per_verb_wasted_turns.
        assert_eq!(MetricRecord::SCHEMA_VERSION, 2);
    }

    #[test]
    fn axed_correctness_first_then_efficiency() {
        let r = MetricRecord {
            schema_version: MetricRecord::SCHEMA_VERSION,
            run_id: "r".into(),
            arm: "maw".into(),
            condition_id: String::new(),
            t_class: String::new(),
            work_lost_events: MetricValue::count(0),
            human_intervention_events: MetricValue::Unavailable,
            tool_calls_total: MetricValue::count(10),
            turns_to_done: MetricValue::count(3),
            wall_duration_ms: MetricValue::duration_ms(1000),
            cost_usd: MetricValue::usd_cents(100),
            work_redone_turns: MetricValue::count(0),
            per_verb_wasted_turns: BTreeMap::new(),
        };
        let axes: Vec<Axis> = r.axed().iter().map(|t| t.2).collect();
        // Correctness block must come first.
        assert_eq!(axes[0], Axis::Correctness);
        assert_eq!(axes[1], Axis::Correctness);
        // Efficiency block follows.
        assert!(axes[2..].iter().all(|a| *a == Axis::Efficiency));
    }

    #[test]
    fn value_format_is_stable() {
        assert_eq!(MetricValue::count(7).format(), "7");
        assert_eq!(MetricValue::duration_ms(42).format(), "42ms");
        assert_eq!(MetricValue::usd_cents(12345).format(), "$1.2345");
        assert_eq!(MetricValue::Infinite.format(), "INF");
        assert_eq!(MetricValue::Unavailable.format(), "n/a");
    }

    #[test]
    fn roundtrip_json() {
        let mut per_verb = BTreeMap::new();
        per_verb.insert(MawVerbAttribution::WsMergeStructuredConflict, 2);
        let r = MetricRecord {
            schema_version: MetricRecord::SCHEMA_VERSION,
            run_id: "abc".into(),
            arm: "maw".into(),
            condition_id: "C0".into(),
            t_class: "T2".into(),
            work_lost_events: MetricValue::count(0),
            human_intervention_events: MetricValue::Unavailable,
            tool_calls_total: MetricValue::count(42),
            turns_to_done: MetricValue::count(5),
            wall_duration_ms: MetricValue::duration_ms(1500),
            cost_usd: MetricValue::usd_cents(2500),
            work_redone_turns: MetricValue::count(1),
            per_verb_wasted_turns: per_verb,
        };
        let s = r.to_json().expect("ser");
        let back = MetricRecord::from_json(&s).expect("de");
        assert_eq!(r, back);
        back.verify_schema().expect("schema");
    }

    /// v1 → v2 migration for MetricRecord: a v1 JSON (no
    /// `per_verb_wasted_turns` field) must deserialize cleanly into
    /// v2 with the new field empty.
    #[test]
    fn v1_metric_record_loads_into_v2_with_empty_per_verb() {
        let v1_json = serde_json::json!({
            "schema_version": 1,
            "run_id": "legacy-1",
            "arm": "maw",
            "condition_id": "C0",
            "t_class": "T2",
            "work_lost_events": {"kind": "count", "n": 0},
            "human_intervention_events": {"kind": "unavailable"},
            "tool_calls_total": {"kind": "count", "n": 10},
            "turns_to_done": {"kind": "count", "n": 3},
            "wall_duration_ms": {"kind": "duration_ms", "ms": 1000},
            "cost_usd": {"kind": "usd_cents", "cents": 100},
            "work_redone_turns": {"kind": "count", "n": 0}
        });
        let r: MetricRecord = serde_json::from_value(v1_json).expect("v1 -> v2 deserialize");
        assert_eq!(r.schema_version, 1, "field carries v1 verbatim");
        assert!(
            r.per_verb_wasted_turns.is_empty(),
            "missing v1 field defaults to empty"
        );
    }

    #[test]
    fn version_mismatch_detected() {
        let mut r = MetricRecord {
            schema_version: 999,
            run_id: "abc".into(),
            arm: "maw".into(),
            condition_id: String::new(),
            t_class: String::new(),
            work_lost_events: MetricValue::count(0),
            human_intervention_events: MetricValue::Unavailable,
            tool_calls_total: MetricValue::count(0),
            turns_to_done: MetricValue::Infinite,
            wall_duration_ms: MetricValue::duration_ms(0),
            cost_usd: MetricValue::Unavailable,
            work_redone_turns: MetricValue::count(0),
            per_verb_wasted_turns: BTreeMap::new(),
        };
        assert!(r.verify_schema().is_err());
        r.schema_version = MetricRecord::SCHEMA_VERSION;
        assert!(r.verify_schema().is_ok());
    }
}
