//! Per-arm dominance table renderer (T2.4 / `bn-oko4`).
//!
//! The renderer turns a slice of [`crate::MetricRecord`] into a
//! human-readable table where:
//!
//! - Each row is **one metric**, labeled with its [`crate::Axis`].
//! - Columns are **arms** (`maw`, `git-worktrees-bare`,
//!   `claude-native-worktrees`, `jj-workspaces`).
//! - The **correctness axis is printed first** and visually
//!   separated from the efficiency axis by a divider, per pre-reg
//!   §4.1 frozen shape.
//! - **No composite column.** No "overall" row. No weighted score.
//!   The `no_composite.rs` integration test enforces this against
//!   the rendered output by string-matching forbidden labels.
//!
//! # Per-run vs aggregated
//!
//! The bone (`bn-oko4`) says "output is a per-run table". This
//! renderer is **per-run**: one column-set per arm shows ALL its
//! runs side-by-side (one sub-column per run id), so a reader sees
//! the raw distribution and cannot mistake a single number for the
//! arm's behavior. When `ReportOptions::aggregate_median` is set,
//! the renderer ADDITIONALLY prints a median row PER ARM (still
//! never combined across arms) below the per-run block — this is
//! the pre-reg §4.1 "median + IQR" surface for the publication.
//! The per-run block remains the primary output.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::record::{Axis, MetricRecord, MetricValue};

/// Renderer knobs. Defaults to the per-run, no-aggregate output the
/// bone requires.
#[derive(Clone, Debug, Default)]
pub struct ReportOptions {
    /// If true, append a per-arm median row below the per-run table.
    /// Medians are computed within an arm only — never across arms,
    /// never as a composite.
    pub aggregate_median: bool,
    /// Optional fixed arm ordering. If `None`, arms are sorted
    /// lexicographically so the printout is deterministic across runs.
    pub arm_order: Option<Vec<String>>,
}

/// Render `records` as a per-arm dominance table to a `String`.
///
/// Output shape (one block per arm; correctness first):
///
/// ```text
/// ARM: maw   (N=2)
///   --- correctness (higher-is-worse; 0 is the bar) ---
///   work_lost_events            r-001=0     r-002=0
///   human_intervention_events   r-001=n/a   r-002=n/a
///   --- efficiency (lower-is-better; not safety) ---
///   tool_calls_total            r-001=12    r-002=17
///   turns_to_done               r-001=4     r-002=INF
///   wall_duration_ms            r-001=1234ms r-002=2200ms
///   cost_usd                    r-001=$0.0342 r-002=$0.0510
///   work_redone_turns           r-001=0     r-002=1
/// ```
///
/// The reader applies the pre-reg §4.3 verdict rule themselves; the
/// renderer NEVER prints a winner.
pub fn render_dominance_table(records: &[MetricRecord], opts: &ReportOptions) -> String {
    let mut out = String::new();
    if records.is_empty() {
        out.push_str("(no records)\n");
        return out;
    }

    // Group by arm. BTreeMap for deterministic iteration when no
    // explicit ordering is provided.
    let mut by_arm: BTreeMap<String, Vec<&MetricRecord>> = BTreeMap::new();
    for r in records {
        by_arm.entry(r.arm.clone()).or_default().push(r);
    }

    // Apply caller-supplied arm ordering (filter to arms actually
    // present; preserves the caller's order; appends any leftover
    // arms in lexicographic order for stability).
    let arm_order: Vec<String> = match &opts.arm_order {
        Some(order) => {
            let mut seen: Vec<String> = Vec::new();
            for name in order {
                if by_arm.contains_key(name) {
                    seen.push(name.clone());
                }
            }
            for name in by_arm.keys() {
                if !seen.contains(name) {
                    seen.push(name.clone());
                }
            }
            seen
        }
        None => by_arm.keys().cloned().collect(),
    };

    // Header: explicit reminder of the binding contract. The reader
    // sees this every time so a screenshot cannot strip the rule.
    // The header is load-bearing: the reader sees this on every render
    // so a screenshot of the table cannot strip the rule. Wording is
    // chosen to make the no-combination rule unmissable without using
    // tokens that the no_composite invariant test scans for as
    // forbidden (those tokens are reserved for indications that the
    // renderer accidentally ADDED an aggregation).
    let _ = writeln!(
        out,
        "SG2 per-run dominance table  (axes printed SEPARATELY; no cross-axis aggregation)"
    );
    let _ = writeln!(
        out,
        "schema_version=v{}   pre-reg=§1.1+§4.1   bone=bn-oko4",
        MetricRecord::SCHEMA_VERSION
    );
    out.push('\n');

    for arm in &arm_order {
        let runs = &by_arm[arm];
        render_arm_block(&mut out, arm, runs, opts);
        out.push('\n');
    }

    out
}

fn render_arm_block(out: &mut String, arm: &str, runs: &[&MetricRecord], opts: &ReportOptions) {
    let _ = writeln!(out, "ARM: {arm}   (N={})", runs.len());

    // Compute column widths. Metric name column = max(name_len).
    // Per-run value column = max(value_format_len) + run_id prefix.
    let metric_col_w = "human_intervention_events".len(); // longest metric name.

    // Cell renderer: `<run_id>=<value>` so a single column carries
    // both the id and the value (per-run table requirement).
    let cell = |rec: &MetricRecord, name: &str| -> String {
        let v = lookup_value(rec, name);
        format!("{}={}", rec.run_id, v.format())
    };

    let mut current_axis: Option<Axis> = None;
    // Field metadata mirrors MetricRecord::axed exactly. We re-derive
    // it from the first record so any future metric additions to
    // axed() flow through here automatically.
    let layout = runs[0].axed();
    for (name, _, axis) in layout {
        // Print axis divider when the axis switches.
        if current_axis != Some(axis) {
            let _ = writeln!(out, "  --- {} ---", axis_caption(axis));
            current_axis = Some(axis);
        }
        let mut row = format!("  {name:<metric_col_w$}");
        for rec in runs {
            row.push_str("  ");
            row.push_str(&cell(rec, name));
        }
        let _ = writeln!(out, "{row}");
    }

    if opts.aggregate_median {
        let _ = writeln!(
            out,
            "  --- per-arm median (within-arm only; axes stay separate) ---"
        );
        let layout = runs[0].axed();
        for (name, _, _) in layout {
            let med = median_value(runs, name);
            let _ = writeln!(
                out,
                "  {:<w$}  median={}",
                name,
                med.format(),
                w = metric_col_w
            );
        }
    }

    // T2.5 diagnostic block: per-verb attribution. ONLY for the maw
    // arm; other arms get an explicit n/a line so a reader cannot
    // misread the absence as "no data captured".
    render_diagnostic_block(out, arm, runs, metric_col_w);
}

/// Render the T2.5 diagnostic block beneath an arm's metric block.
///
/// **Maw arm only.** Other arms render a single
/// `n/a (substrate has no maw verbs)` line — explicit so the reader
/// distinguishes "no friction" from "no concept of friction".
///
/// **Never folded into a composite.** This is a DIAGNOSTIC AXIS;
/// `no_composite.rs` invariant test continues to scan for forbidden
/// tokens here too (the diagnostic block must NOT contain "winner",
/// "score", "ranking", etc.).
fn render_diagnostic_block(out: &mut String, arm: &str, runs: &[&MetricRecord], col_w: usize) {
    let is_maw = arm == "maw" || arm.starts_with("maw-");
    // Wording is deliberate: this caption is a load-bearing reminder
    // that the diagnostic axis is per-verb attribution only, never an
    // aggregate score. We avoid the literal "composite" token because
    // the no_composite invariant test scans for it — the rule it
    // enforces is that the renderer cannot emit a cross-axis number,
    // not that the renderer cannot mention the rule. Same intent,
    // tokens chosen to satisfy the invariant scanner cleanly.
    let _ = writeln!(
        out,
        "  --- diagnostic: per-verb attribution (T2.5; maw-arm only; per-verb counts ONLY) ---"
    );
    if !is_maw {
        let _ = writeln!(out, "  n/a (substrate has no maw verbs)");
        return;
    }
    // Sum per-attribution across all runs for this arm.
    let mut totals: BTreeMap<crate::MawVerbAttribution, u32> = BTreeMap::new();
    for rec in runs {
        for (att, n) in &rec.per_verb_wasted_turns {
            *totals.entry(*att).or_insert(0) += *n;
        }
    }
    if totals.values().all(|n| *n == 0) {
        let _ = writeln!(out, "  (no attributed wasted turns)");
        return;
    }
    // Render in stable variant order so screenshots match across runs.
    for att in crate::MawVerbAttribution::ALL {
        let n = totals.get(att).copied().unwrap_or(0);
        if n == 0 {
            continue;
        }
        let _ = writeln!(out, "  {:<w$}  count={}", att.slug(), n, w = col_w);
    }
}

fn axis_caption(a: Axis) -> &'static str {
    match a {
        Axis::Correctness => "correctness (higher-is-worse; 0 is the bar)",
        Axis::Efficiency => "efficiency (lower-is-better; NOT safety)",
    }
}

fn lookup_value(rec: &MetricRecord, name: &str) -> MetricValue {
    rec.axed()
        .iter()
        .find(|(n, _, _)| *n == name)
        .map_or(MetricValue::Unavailable, |(_, v, _)| *v)
}

/// Within-arm median over `MetricValue`s. The median of a mixed
/// `Unavailable`/`Infinite`/finite set is computed per the
/// `notes/sg2-metric-definitions.md` "median across runs" rule:
///
/// - `Unavailable` values are dropped from the set (the metric was
///   not measured; it does not contribute).
/// - `Infinite` is treated as the maximum element (an unfinished
///   run dominates the upper half of the distribution); the median
///   is then computed on the resulting ordered set.
/// - If all values are `Unavailable`, median is `Unavailable`.
/// - Median definition: lower-median for even N (preserves type;
///   no implicit float promotion from integer counts).
fn median_value(runs: &[&MetricRecord], name: &str) -> MetricValue {
    let mut finite: Vec<u64> = Vec::with_capacity(runs.len());
    let mut infinite_count: usize = 0;
    let mut total_measured: usize = 0;
    let mut kind: Option<&'static str> = None;
    for r in runs {
        let v = lookup_value(r, name);
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
        return MetricValue::Unavailable;
    }
    finite.sort_unstable();
    // Append infinite-as-max sentinels to the tail.
    let n_total = finite.len() + infinite_count;
    // Lower-median index.
    let lower = (n_total - 1) / 2;
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
    val_at(lower)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::MetricValue;

    fn rec(run_id: &str, arm: &str, lost: u64, turns: MetricValue, calls: u64) -> MetricRecord {
        MetricRecord {
            schema_version: MetricRecord::SCHEMA_VERSION,
            run_id: run_id.into(),
            arm: arm.into(),
            condition_id: "C0".into(),
            t_class: "T2".into(),
            work_lost_events: MetricValue::count(lost),
            human_intervention_events: MetricValue::Unavailable,
            tool_calls_total: MetricValue::count(calls),
            turns_to_done: turns,
            wall_duration_ms: MetricValue::duration_ms(1000),
            cost_usd: MetricValue::usd_cents(100),
            work_redone_turns: MetricValue::count(0),
            per_verb_wasted_turns: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn renders_correctness_before_efficiency() {
        let recs = vec![rec("r1", "maw", 0, MetricValue::count(3), 12)];
        let out = render_dominance_table(&recs, &ReportOptions::default());
        let corr_idx = out.find("correctness").expect("correctness header");
        let eff_idx = out.find("efficiency").expect("efficiency header");
        assert!(
            corr_idx < eff_idx,
            "correctness must come first; got out:\n{out}"
        );
    }

    #[test]
    fn empty_records_renders_placeholder() {
        let out = render_dominance_table(&[], &ReportOptions::default());
        assert!(out.contains("(no records)"));
    }

    #[test]
    fn no_composite_label_in_output() {
        let recs = vec![
            rec("r1", "maw", 0, MetricValue::count(3), 12),
            rec("r2", "jj-workspaces", 1, MetricValue::count(7), 25),
        ];
        let out = render_dominance_table(&recs, &ReportOptions::default());
        // Forbidden words/phrases.
        for forbidden in [
            "composite",
            "weighted",
            "score:",
            "overall",
            "total =",
            "winner",
            "ranking",
            "rank:",
        ] {
            assert!(
                !out.to_ascii_lowercase().contains(forbidden),
                "rendered output contains forbidden phrase {forbidden:?}:\n{out}"
            );
        }
    }

    #[test]
    fn per_run_columns_show_each_run_id() {
        let recs = vec![
            rec("alpha", "maw", 0, MetricValue::count(3), 12),
            rec("beta", "maw", 0, MetricValue::count(4), 14),
        ];
        let out = render_dominance_table(&recs, &ReportOptions::default());
        assert!(out.contains("alpha=12"), "missing alpha=12:\n{out}");
        assert!(out.contains("beta=14"), "missing beta=14:\n{out}");
    }

    #[test]
    fn aggregate_median_optional_and_per_arm_only() {
        let recs = vec![
            rec("a1", "maw", 0, MetricValue::count(3), 10),
            rec("a2", "maw", 0, MetricValue::count(5), 12),
            rec("a3", "maw", 0, MetricValue::count(7), 14),
            rec("b1", "jj-workspaces", 0, MetricValue::count(20), 50),
        ];
        let opts = ReportOptions {
            aggregate_median: true,
            arm_order: None,
        };
        let out = render_dominance_table(&recs, &opts);
        // Both arms have a median row. Default BTreeMap ordering is
        // alphabetic so jj-workspaces appears before maw.
        let jj_idx = out.find("ARM: jj-workspaces").unwrap();
        let maw_idx = out.find("ARM: maw").unwrap();
        assert!(jj_idx < maw_idx, "expected alphabetic order:\n{out}");
        assert!(out[jj_idx..maw_idx].contains("median="));
        assert!(out[maw_idx..].contains("median="));
    }

    #[test]
    fn diagnostic_block_renders_for_maw_arm_with_attribution() {
        use crate::MawVerbAttribution;
        let mut per_verb = std::collections::BTreeMap::new();
        per_verb.insert(MawVerbAttribution::WsMergeStructuredConflict, 2);
        per_verb.insert(MawVerbAttribution::WsRecoverInvoked, 1);
        let r = MetricRecord {
            schema_version: MetricRecord::SCHEMA_VERSION,
            run_id: "m1".into(),
            arm: "maw".into(),
            condition_id: "C0".into(),
            t_class: "T2".into(),
            work_lost_events: MetricValue::count(0),
            human_intervention_events: MetricValue::Unavailable,
            tool_calls_total: MetricValue::count(20),
            turns_to_done: MetricValue::count(8),
            wall_duration_ms: MetricValue::duration_ms(1000),
            cost_usd: MetricValue::usd_cents(100),
            work_redone_turns: MetricValue::count(3),
            per_verb_wasted_turns: per_verb,
        };
        let out = render_dominance_table(&[r], &ReportOptions::default());
        // Diagnostic header present.
        assert!(out.contains("diagnostic: per-verb attribution"));
        // Cluster rows present in stable order.
        let merge_idx = out.find("ws_merge_structured_conflict").expect("merge row");
        let recover_idx = out.find("ws_recover_invoked").expect("recover row");
        assert!(merge_idx < recover_idx, "stable variant ordering");
        // Counts present.
        assert!(out.contains("count=2"));
        assert!(out.contains("count=1"));
        // No "n/a" line for the maw arm with populated attribution.
        assert!(!out.contains("n/a (substrate has no maw verbs)"));
    }

    #[test]
    fn diagnostic_block_renders_na_for_non_maw_arm() {
        let r = rec("r1", "jj-workspaces", 0, MetricValue::count(3), 12);
        let out = render_dominance_table(&[r], &ReportOptions::default());
        assert!(out.contains("diagnostic: per-verb attribution"));
        assert!(out.contains("n/a (substrate has no maw verbs)"));
    }

    #[test]
    fn diagnostic_block_renders_no_attributed_for_clean_maw_run() {
        // Maw arm but zero attributed friction. The block should say
        // "(no attributed wasted turns)" — distinct from the non-maw
        // "n/a" line.
        let r = rec("clean", "maw", 0, MetricValue::count(3), 10);
        let out = render_dominance_table(&[r], &ReportOptions::default());
        assert!(out.contains("(no attributed wasted turns)"));
        assert!(!out.contains("n/a (substrate has no maw verbs)"));
    }

    #[test]
    fn arm_order_respected() {
        let recs = vec![
            rec("r1", "zeta-arm", 0, MetricValue::count(3), 12),
            rec("r2", "maw", 0, MetricValue::count(4), 14),
        ];
        let opts = ReportOptions {
            aggregate_median: false,
            arm_order: Some(vec!["maw".into(), "zeta-arm".into()]),
        };
        let out = render_dominance_table(&recs, &opts);
        let maw_idx = out.find("ARM: maw").unwrap();
        let zeta_idx = out.find("ARM: zeta-arm").unwrap();
        assert!(maw_idx < zeta_idx, "arm_order not respected:\n{out}");
    }

    #[test]
    fn median_handles_infinite_as_max() {
        let recs_ref: Vec<&MetricRecord> = vec![];
        // Three runs: 3, INF, 5. Sorted: 3, 5, INF. Lower median = 5 (index 1).
        let r1 = rec("r1", "maw", 0, MetricValue::count(3), 0);
        let r2 = rec("r2", "maw", 0, MetricValue::Infinite, 0);
        let r3 = rec("r3", "maw", 0, MetricValue::count(5), 0);
        let refs: Vec<&MetricRecord> = vec![&r1, &r2, &r3];
        let _ = recs_ref;
        let med = median_value(&refs, "turns_to_done");
        assert_eq!(med, MetricValue::count(5));
    }

    #[test]
    fn median_all_unavailable_is_unavailable() {
        let r1 = MetricRecord {
            schema_version: MetricRecord::SCHEMA_VERSION,
            run_id: "r1".into(),
            arm: "x".into(),
            condition_id: String::new(),
            t_class: String::new(),
            work_lost_events: MetricValue::count(0),
            human_intervention_events: MetricValue::Unavailable,
            tool_calls_total: MetricValue::count(0),
            turns_to_done: MetricValue::count(0),
            wall_duration_ms: MetricValue::duration_ms(0),
            cost_usd: MetricValue::Unavailable,
            work_redone_turns: MetricValue::count(0),
            per_verb_wasted_turns: std::collections::BTreeMap::new(),
        };
        let refs = vec![&r1];
        let med = median_value(&refs, "cost_usd");
        assert_eq!(med, MetricValue::Unavailable);
    }
}
