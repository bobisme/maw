//! T2.8 (`bn-u9iy`) — Prioritized maw friction list (SG4's input).
//!
//! # What this module does
//!
//! Reduces a set of [`crate::attribution::DiagnosticBundle`] records
//! (one per maw-arm BenchRun) into a single [`FrictionList`]: a
//! ranked, evidence-bearing catalog of which maw verbs/states are
//! costing agents the most turns. This is the artifact SG4/T4.1
//! reads to pick hardening targets.
//!
//! # Why this is NOT a composite score
//!
//! The friction list ranks **within a single axis** (the diagnostic
//! per-verb attribution axis defined by T2.5). "Total cost in turns"
//! is a same-unit sort key over comparable cluster counts — it does
//! NOT cross the correctness/efficiency axis boundary. The
//! `no_composite.rs` invariant (lifted from T2.4) continues to apply
//! and is asserted in this module's tests against the renderer
//! output.
//!
//! # Inputs
//!
//! - `bundles: &[DiagnosticBundle]` — the T2.5 contract format. Each
//!   bundle is per-(arm, run). Non-maw bundles are accepted (they
//!   contribute 0 to every cluster); see the `accepts_mixed_arms`
//!   test. The expected production path passes only `arm == "maw"`
//!   bundles, but the reducer is tolerant.
//! - `harness_commit: &str` — the git SHA of the bench harness that
//!   produced the bundles. Recorded verbatim in the output so the
//!   SG4 consumer can pin its hardening campaign to a known producer
//!   version.
//!
//! # First-pass classifier vs publication-grade coding
//!
//! Per `notes/sg2-benchmark-preregistration.md` §6.3, publication-
//! grade numbers require blind double-coding by two analysts on a
//! 20% transcript sample. **This module is the first-pass classifier
//! path** — sufficient for SG4 hardening-input selection (SG4 picks
//! the top-K targets and validates reduction post-hardening). The
//! `FrictionList::source` field carries that classification so the
//! consumer knows what they are reading.
//!
//! # The unattributed bucket
//!
//! `total_unattributed_wasted_turns` is the sum of every input
//! bundle's `total_unattributed_wasted_turns`. It is surfaced as a
//! distinct top-level field (NOT a cluster) so SG4 can see what the
//! heuristic missed. The bone hard-rule:
//! `total_unattributed_wasted_turns` MUST be flagged for human
//! coding follow-up (§6.3).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::attribution::{DiagnosticBundle, MawVerbAttribution};

/// Schema version for the [`FrictionList`] artifact. Bumped only
/// when SG4's consumed shape changes; additive fields do NOT bump.
pub const FRICTION_LIST_SCHEMA_VERSION: u32 = 1;

/// Cap on `evidence_run_ids` per cluster in the rendered output.
/// Excess go into `evidence_overflow_count`. Picked so a reader can
/// eyeball the cluster without paging.
pub const EVIDENCE_RUN_ID_CAP: usize = 5;

/// Cap on `example_transcript_excerpts` per cluster. The full
/// transcripts live in the BenchRun artifacts; the excerpt list is
/// a paste-friendly affordance, not the data of record.
pub const EXAMPLE_EXCERPT_CAP: usize = 3;

/// Provenance of the friction-list numbers — first-pass classifier
/// (good enough for SG4 input) versus publication-grade
/// blind-double-coded (§6.3).
///
/// Recorded in the output so the SG4 consumer cannot accidentally
/// publish first-pass numbers as headline results.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrictionSource {
    /// Counts come from [`crate::attribute_tool_call`] + the
    /// two-call stale-read detector. Sufficient for SG4 to pick
    /// hardening targets; NOT publication-grade.
    FirstPassClassifier,
    /// Counts come from blind double-coding per pre-reg §6.3. Used
    /// only when the friction list is regenerated from a coded run
    /// set. T2.8 itself never produces this; documented here so the
    /// SG4 → T2 re-run loop can stamp it.
    BlindDoubleCoded,
}

/// Pointer to the sweep run / artifact dir the friction list
/// summarizes. Keeps the SG4 input self-describing — given a
/// `FrictionList`, you can walk back to the source BenchRuns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepRunRef {
    /// Filesystem path (or URI) to the artifact directory the
    /// bundles were extracted from. Empty when the reducer was
    /// called on in-memory bundles (tests, pilot synthetic data).
    pub artifact_dir: String,
    /// `SweepSummary` source identifier when one exists (e.g. the
    /// `crossover-summary.md` companion path). Empty when not
    /// applicable.
    pub sweep_summary_ref: String,
    /// Number of input [`DiagnosticBundle`]s the list was reduced
    /// from. Sanity field for the SG4 consumer.
    pub bundle_count: u32,
}

/// One short paste-in from a transcript that motivated a cluster.
///
/// Excerpts are deliberately structured (not raw text) so the
/// rendered doc can be regenerated from JSON without re-reading the
/// source BenchRuns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptExcerpt {
    /// The BenchRun the excerpt comes from.
    pub run_id: String,
    /// 1-based turn index within that run.
    pub turn_index: u32,
    /// `ToolCall::name`.
    pub tool_call_name: String,
    /// Truncated args summary (≤ 200 chars). The full args live in
    /// the source BenchRun JSON; the summary is a paste-in.
    pub tool_call_args_summary: String,
    /// The substrate's outcome flags for this call, if recorded.
    /// `None` for first-pass extraction where the bundle did not
    /// carry attributed outcomes; the doc still renders the row.
    pub subsequent_outcome: Option<ExcerptOutcome>,
}

/// Per-excerpt outcome flags. Mirrors [`maw_bench::run::StepOutcome`]
/// but lives in the friction-list crate so the SG4 consumer doesn't
/// need to pull `maw-bench` to read its input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcerptOutcome {
    /// Substrate completed the op without an adapter-visible error.
    pub ok: bool,
    /// Op produced a structured conflict the agent must resolve.
    pub conflicted: bool,
}

/// One cluster row in the ranked friction list. Sorted DESCENDING
/// by [`FrictionCluster::total_cost_turns`] within
/// [`FrictionList::ranked_clusters`].
///
/// **Empty clusters are filtered.** Only attributions that fired at
/// least once across the input set appear in the list (the bone
/// asks for "verbs costing agents turns", not a fixed shape).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrictionCluster {
    /// 1-indexed rank among the ranked clusters. `1` is the worst
    /// cluster (highest total cost). Ties broken by stable
    /// [`MawVerbAttribution::ALL`] order so the rendered list is
    /// deterministic.
    pub rank: u32,
    /// Which maw verb / state this cluster attributes friction to.
    pub attribution: MawVerbAttribution,
    /// Summed wasted-turn count across every input bundle's row
    /// for this cluster. The ranking key.
    pub total_cost_turns: u32,
    /// Distinct BenchRun-id count where this cluster fired at
    /// least once. (Density indicator complementary to total cost.)
    pub occurrence_count: u32,
    /// Up to [`EVIDENCE_RUN_ID_CAP`] BenchRun ids whose transcripts
    /// motivated this cluster. Stable-sorted for determinism.
    pub evidence_run_ids: Vec<String>,
    /// Count of run-ids that hit the cap and were not included in
    /// `evidence_run_ids`. SG4 can still query them out of the
    /// source artifact dir.
    pub evidence_overflow_count: u32,
    /// 1..[`EXAMPLE_EXCERPT_CAP`] paste-in transcript excerpts.
    /// Empty when no excerpt data was available (the per-call
    /// detail is optional in the [`DiagnosticBundle`] schema).
    pub example_transcript_excerpts: Vec<TranscriptExcerpt>,
}

/// The SG4 input format. The machine-readable peer of
/// `notes/sg2-friction-list.md`.
///
/// # Stability
///
/// Pinned by [`FRICTION_LIST_SCHEMA_VERSION`] and the
/// `friction_list_schema_is_pinned` test. The SG4 consumer
/// (`bn-2j45` / T4.1) reads this struct's JSON form; any field
/// rename or removal MUST bump the version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrictionList {
    /// = [`FRICTION_LIST_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Provenance pointer to the source sweep / artifact dir.
    pub sweep_run: SweepRunRef,
    /// Whether numbers come from the first-pass classifier or
    /// publication-grade blind double-coding.
    pub source: FrictionSource,
    /// Ranked clusters, DESCENDING by `total_cost_turns`. Empty
    /// clusters (count == 0 across every input bundle) are NOT
    /// included.
    pub ranked_clusters: Vec<FrictionCluster>,
    /// Sum of `total_unattributed_wasted_turns` across every input
    /// bundle. Surfaced as a top-level field (NOT a cluster) so
    /// SG4 sees the heuristic's blind spot directly. Flagged for
    /// human coding follow-up (§6.3).
    pub total_unattributed_wasted_turns: u32,
    /// ISO-8601 UTC timestamp the list was generated at. Set by
    /// the binary; tests pass a pinned value.
    pub generated_at_utc: String,
    /// Git SHA of the bench harness that produced the source
    /// bundles. Recorded so SG4 can pin its hardening campaign to
    /// a known producer version.
    pub harness_commit_sha: String,
}

impl FrictionList {
    /// Pretty-JSON serialize. Used by the
    /// `sg2-friction-list` bin and by the pinning test.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Decode from JSON. Used by SG4's consumer side.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

/// Reduce a slice of [`DiagnosticBundle`] into a [`FrictionList`].
///
/// **Pure.** Same inputs → same `FrictionList` (modulo the caller-
/// supplied `generated_at_utc`).
///
/// Steps:
///
/// 1. Sum per-cluster counts across every bundle.
/// 2. Collect per-cluster evidence run-ids (dedupe, stable sort).
/// 3. Drop clusters with `total == 0`.
/// 4. Sort DESCENDING by total; ties broken by stable
///    `MawVerbAttribution::ALL` order.
/// 5. Rank 1-indexed; cap evidence-run-ids; record overflow count.
/// 6. Sum `total_unattributed_wasted_turns` across bundles.
///
/// `generated_at_utc` and `harness_commit` are stamped verbatim onto
/// the output so the reducer stays I/O-free; the binary supplies
/// real wall-clock + git-sha values, tests pass pinned strings.
#[must_use]
pub fn friction_list_from_bundles(
    bundles: &[DiagnosticBundle],
    sweep_run: SweepRunRef,
    source: FrictionSource,
    generated_at_utc: &str,
    harness_commit: &str,
) -> FrictionList {
    // (1) Aggregate per-cluster totals + per-cluster evidence sets.
    let mut totals: BTreeMap<MawVerbAttribution, u32> = BTreeMap::new();
    let mut evidence: BTreeMap<MawVerbAttribution, Vec<String>> = BTreeMap::new();
    let mut occurrences: BTreeMap<MawVerbAttribution, u32> = BTreeMap::new();
    let mut total_unattributed: u32 = 0;

    for bundle in bundles {
        total_unattributed = total_unattributed.saturating_add(bundle.total_unattributed_wasted_turns);
        for row in &bundle.per_verb_clusters {
            if row.count == 0 {
                continue;
            }
            *totals.entry(row.attribution).or_insert(0) =
                totals.get(&row.attribution).copied().unwrap_or(0).saturating_add(row.count);
            *occurrences.entry(row.attribution).or_insert(0) =
                occurrences.get(&row.attribution).copied().unwrap_or(0).saturating_add(1);
            // Evidence: this bundle's own run_id, plus whatever
            // run-ids the row already carries (from a multi-run
            // bundle).
            let ev = evidence.entry(row.attribution).or_default();
            ev.push(bundle.run_id.clone());
            for rid in &row.evidence_run_ids {
                ev.push(rid.clone());
            }
        }
    }

    // (2) Stable per-cluster evidence: dedupe + sort.
    for ev in evidence.values_mut() {
        ev.sort();
        ev.dedup();
    }

    // (3 + 4) Build cluster list, drop empties, sort DESC by total
    // with stable tiebreak on MawVerbAttribution::ALL order.
    let ordering: BTreeMap<MawVerbAttribution, usize> = MawVerbAttribution::ALL
        .iter()
        .copied()
        .enumerate()
        .map(|(i, a)| (a, i))
        .collect();
    let mut clusters: Vec<(MawVerbAttribution, u32, u32, Vec<String>)> = totals
        .into_iter()
        .map(|(att, total)| {
            let occ = occurrences.get(&att).copied().unwrap_or(0);
            let ev = evidence.remove(&att).unwrap_or_default();
            (att, total, occ, ev)
        })
        .collect();
    clusters.sort_by(|a, b| {
        // DESC on total_cost_turns; ASC on stable variant order.
        b.1.cmp(&a.1).then_with(|| {
            let ai = ordering.get(&a.0).copied().unwrap_or(usize::MAX);
            let bi = ordering.get(&b.0).copied().unwrap_or(usize::MAX);
            ai.cmp(&bi)
        })
    });

    // (5) Rank, cap evidence, record overflow.
    let ranked_clusters: Vec<FrictionCluster> = clusters
        .into_iter()
        .enumerate()
        .map(|(i, (att, total, occ, ev))| {
            let overflow = ev.len().saturating_sub(EVIDENCE_RUN_ID_CAP);
            let capped: Vec<String> = ev.into_iter().take(EVIDENCE_RUN_ID_CAP).collect();
            FrictionCluster {
                rank: u32::try_from(i + 1).unwrap_or(u32::MAX),
                attribution: att,
                total_cost_turns: total,
                occurrence_count: occ,
                evidence_run_ids: capped,
                evidence_overflow_count: u32::try_from(overflow).unwrap_or(u32::MAX),
                // First-pass extractor has no per-excerpt detail in
                // the DiagnosticBundle schema; the bin can enrich
                // post-hoc by re-reading transcripts. Leave empty
                // for the pure reducer.
                example_transcript_excerpts: Vec::new(),
            }
        })
        .collect();

    FrictionList {
        schema_version: FRICTION_LIST_SCHEMA_VERSION,
        sweep_run,
        source,
        ranked_clusters,
        total_unattributed_wasted_turns: total_unattributed,
        generated_at_utc: generated_at_utc.to_string(),
        harness_commit_sha: harness_commit.to_string(),
    }
}

/// Render `list` as the human-readable Markdown doc that ships as
/// `notes/sg2-friction-list.md`.
///
/// Output shape (one `##` section per cluster, ranked):
///
/// ```text
/// # SG2 prioritized friction list  (T2.8 / bn-u9iy)
///
/// > TEMPLATE: real-run cells populated post-campaign. ...
///
/// **Source:** first_pass_classifier   **Schema:** v1
/// **Bundles consumed:** N             **Harness SHA:** <sha>
///
/// ## #1 — ws_merge_structured_conflict   (cost=12 turns, runs=3)
///
/// - Evidence runs: r-001, r-002, r-003
/// - Excerpts: (none — first-pass extractor)
/// - Recommended-fix-class: ...
///
/// ## #2 — ws_sync_stale_workspace   (cost=8 turns, runs=2)
/// ...
///
/// ## Unattributed bucket
/// total_unattributed_wasted_turns = N
/// (flagged for human coding per §6.3)
/// ```
///
/// The rendered doc is a TEMPLATE today — the real-run campaign
/// hasn't run, so numbers come from synthetic pilot data. The
/// renderer stamps an explicit TEMPLATE banner at the top so a
/// reader cannot mistake pilot numbers for publication numbers.
pub fn render_friction_list_md(list: &FrictionList) -> String {
    let mut out = String::new();
    render_md_header(&mut out, list);
    render_md_clusters(&mut out, list);
    render_md_unattributed(&mut out, list);
    render_md_sg4_handoff(&mut out);
    out
}

fn render_md_header(out: &mut String, list: &FrictionList) {
    let _ = writeln!(out, "# SG2 prioritized friction list  (T2.8 / bn-u9iy)");
    out.push('\n');
    let _ = writeln!(
        out,
        "> **TEMPLATE — real-run cells populated post-campaign.**  Numbers below are extracted"
    );
    let _ = writeln!(
        out,
        "> by the first-pass classifier ({}). They are sufficient for SG4 hardening-target",
        list.source_label()
    );
    let _ = writeln!(
        out,
        "> selection but NOT publication-grade. Pre-reg §6.3 blind double-coding required for"
    );
    let _ = writeln!(out, "> headline numbers.");
    out.push('\n');
    let _ = writeln!(
        out,
        "**Source:** `{}`   **Schema:** v{}   **Bundles consumed:** {}",
        list.source_label(),
        list.schema_version,
        list.sweep_run.bundle_count,
    );
    let _ = writeln!(
        out,
        "**Artifact dir:** `{}`   **Harness SHA:** `{}`   **Generated:** `{}`",
        if list.sweep_run.artifact_dir.is_empty() {
            "(in-memory)"
        } else {
            list.sweep_run.artifact_dir.as_str()
        },
        list.harness_commit_sha,
        list.generated_at_utc,
    );
    out.push('\n');
    // No-composite reminder in the human-readable surface (same
    // discipline as the T2.4 renderer's header).
    let _ = writeln!(
        out,
        "_Ranking is within the diagnostic axis only — turn counts are the same unit. NO cross-axis aggregation._"
    );
    out.push('\n');
}

fn render_md_clusters(out: &mut String, list: &FrictionList) {
    if list.ranked_clusters.is_empty() {
        let _ = writeln!(out, "## No attributed friction observed");
        let _ = writeln!(
            out,
            "(The classifier found zero attributable wasted turns across the input set.)"
        );
        out.push('\n');
        return;
    }
    for cluster in &list.ranked_clusters {
        render_md_one_cluster(out, cluster);
    }
}

fn render_md_one_cluster(out: &mut String, cluster: &FrictionCluster) {
    let _ = writeln!(
        out,
        "## #{} — `{}`   (cost={} turns, runs={})",
        cluster.rank,
        cluster.attribution.slug(),
        cluster.total_cost_turns,
        cluster.occurrence_count,
    );
    out.push('\n');

    // Special call-out for the agent-fluency principle's headline
    // cluster (per the bone hard-rule).
    if cluster.attribution == MawVerbAttribution::VocabularyScarcity {
        let _ = writeln!(
            out,
            "> **agent-fluency principle measurement**: this cluster is the open thread from"
        );
        let _ = writeln!(
            out,
            "> `maw-design-rationale-agent-fluency` — whether maw's self-describing output is"
        );
        let _ = writeln!(
            out,
            "> a sufficient mitigation for the training-data-scarce verb problem. Cost > 0 here"
        );
        let _ = writeln!(out, "> means the mitigation is incomplete.");
        out.push('\n');
    }

    // Evidence runs (capped).
    if cluster.evidence_run_ids.is_empty() {
        let _ = writeln!(out, "- Evidence runs: (none recorded)");
    } else {
        let ev = cluster.evidence_run_ids.join(", ");
        if cluster.evidence_overflow_count == 0 {
            let _ = writeln!(out, "- Evidence runs: `{ev}`");
        } else {
            let _ = writeln!(
                out,
                "- Evidence runs: `{ev}`  (+{} more)",
                cluster.evidence_overflow_count
            );
        }
    }
    // Excerpts.
    if cluster.example_transcript_excerpts.is_empty() {
        let _ = writeln!(
            out,
            "- Excerpts: _(none — first-pass extractor does not enrich per-call detail)_"
        );
    } else {
        let _ = writeln!(out, "- Excerpts:");
        for ex in &cluster.example_transcript_excerpts {
            let outcome = ex
                .subsequent_outcome
                .map_or_else(String::new, |o| {
                    format!(" (ok={}, conflicted={})", o.ok, o.conflicted)
                });
            let _ = writeln!(
                out,
                "  - `{}` turn {}: `{}` — `{}`{outcome}",
                ex.run_id, ex.turn_index, ex.tool_call_name, ex.tool_call_args_summary,
            );
        }
    }
    // Recommended-fix-class label — a hint to SG4 for what KIND of
    // hardening reduces this cluster. NOT a binding prescription;
    // SG4 owns the actual hardening design.
    let _ = writeln!(
        out,
        "- Recommended-fix-class: `{}`",
        recommended_fix_class(cluster.attribution)
    );
    out.push('\n');
}

fn render_md_unattributed(out: &mut String, list: &FrictionList) {
    // Unattributed bucket — always emitted, even when zero, so the
    // reader sees it explicitly (bone hard rule).
    let _ = writeln!(out, "## Unattributed bucket");
    out.push('\n');
    let _ = writeln!(
        out,
        "`total_unattributed_wasted_turns` = **{}**",
        list.total_unattributed_wasted_turns
    );
    out.push('\n');
    let _ = writeln!(
        out,
        "Friction the first-pass classifier detected but could not attribute to a named cluster."
    );
    let _ = writeln!(
        out,
        "_Flagged for human coding follow-up per `notes/sg2-benchmark-preregistration.md` §6.3._"
    );
    out.push('\n');
}

fn render_md_sg4_handoff(out: &mut String) {
    let _ = writeln!(out, "## SG4 handoff");
    out.push('\n');
    let _ = writeln!(
        out,
        "- **Consumer:** SG4 (`bn-2j45`) / T4.1 reads `ranked_clusters` and picks hardening targets."
    );
    let _ = writeln!(
        out,
        "- **Validation loop:** post-hardening, T2 re-runs and confirms `total_cost_turns` reduction"
    );
    let _ = writeln!(out, "  in the targeted clusters.");
    let _ = writeln!(
        out,
        "- **Closing this task** (`bn-u9iy`) auto-closes SG2 (`bn-2jwi`) which unblocks SG4 (`bn-2j45`)."
    );
    out.push('\n');
}

impl FrictionList {
    fn source_label(&self) -> &'static str {
        match self.source {
            FrictionSource::FirstPassClassifier => "first_pass_classifier",
            FrictionSource::BlindDoubleCoded => "blind_double_coded",
        }
    }
}

/// Per-cluster recommended-fix-class hint for SG4. Captures the
/// rough KIND of hardening that historically reduces each cluster.
/// NOT a binding prescription; T4.x owns the actual design.
#[must_use]
pub fn recommended_fix_class(att: MawVerbAttribution) -> &'static str {
    match att {
        MawVerbAttribution::WsCreateNameClash => "preflight-validation",
        MawVerbAttribution::WsMergeStructuredConflict => "merge-engine-resilience",
        MawVerbAttribution::WsSyncStaleWorkspace => "stale-state-self-healing",
        MawVerbAttribution::WsResolveRetry => "resolve-vocabulary-clarification",
        MawVerbAttribution::WsDestroyRefused => "destroy-guidance-output",
        MawVerbAttribution::WsRecoverInvoked => "destroy-prevention",
        MawVerbAttribution::WsAbortInvoked => "abort-then-resume-affordance",
        MawVerbAttribution::EpochSyncRequired => "epoch-auto-advance",
        MawVerbAttribution::ReadFromStaleWorkspace => "status-output-discoverability",
        MawVerbAttribution::ReadFromConflictedWorkspace => "conflicted-state-output-clarity",
        MawVerbAttribution::ReadFromDetachedHead => "head-state-output-clarity",
        MawVerbAttribution::VocabularyScarcity => "verb-discoverability",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attribution::{DiagnosticBundle, MawVerbAttribution};

    fn sweep_run(n: u32) -> SweepRunRef {
        SweepRunRef {
            artifact_dir: "test://in-memory".to_string(),
            sweep_summary_ref: String::new(),
            bundle_count: n,
        }
    }

    fn bundle(run_id: &str, attrs: &[(MawVerbAttribution, u32)], unattributed: u32) -> DiagnosticBundle {
        let mut counts: BTreeMap<MawVerbAttribution, u32> = BTreeMap::new();
        for (a, n) in attrs {
            counts.insert(*a, *n);
        }
        let evidence: BTreeMap<MawVerbAttribution, Vec<String>> = BTreeMap::new();
        DiagnosticBundle::from_counts("maw", run_id, &counts, &evidence, unattributed)
    }

    // ---------- Ranking: planted-highest-cost cluster ranks #1 ----------

    #[test]
    fn planted_highest_cost_cluster_ranks_first() {
        // Two bundles: planted total for WsMergeStructuredConflict = 5;
        // total for WsSyncStaleWorkspace = 2. Merge MUST rank #1.
        let bundles = vec![
            bundle(
                "r-001",
                &[
                    (MawVerbAttribution::WsMergeStructuredConflict, 3),
                    (MawVerbAttribution::WsSyncStaleWorkspace, 1),
                ],
                0,
            ),
            bundle(
                "r-002",
                &[
                    (MawVerbAttribution::WsMergeStructuredConflict, 2),
                    (MawVerbAttribution::WsSyncStaleWorkspace, 1),
                ],
                0,
            ),
        ];
        let list = friction_list_from_bundles(
            &bundles,
            sweep_run(2),
            FrictionSource::FirstPassClassifier,
            "2026-05-25T00:00:00Z",
            "deadbeef",
        );
        assert_eq!(list.ranked_clusters.len(), 2);
        assert_eq!(list.ranked_clusters[0].rank, 1);
        assert_eq!(
            list.ranked_clusters[0].attribution,
            MawVerbAttribution::WsMergeStructuredConflict
        );
        assert_eq!(list.ranked_clusters[0].total_cost_turns, 5);
        assert_eq!(list.ranked_clusters[0].occurrence_count, 2);
        assert_eq!(list.ranked_clusters[1].rank, 2);
        assert_eq!(
            list.ranked_clusters[1].attribution,
            MawVerbAttribution::WsSyncStaleWorkspace
        );
    }

    #[test]
    fn tiebreak_is_stable_variant_order() {
        // Equal totals → ties broken by MawVerbAttribution::ALL order.
        // WsCreateNameClash precedes WsMergeStructuredConflict in ALL.
        let bundles = vec![bundle(
            "r-x",
            &[
                (MawVerbAttribution::WsMergeStructuredConflict, 3),
                (MawVerbAttribution::WsCreateNameClash, 3),
            ],
            0,
        )];
        let list = friction_list_from_bundles(
            &bundles,
            sweep_run(1),
            FrictionSource::FirstPassClassifier,
            "2026-05-25T00:00:00Z",
            "",
        );
        assert_eq!(
            list.ranked_clusters[0].attribution,
            MawVerbAttribution::WsCreateNameClash,
            "stable tiebreak uses ALL order"
        );
        assert_eq!(
            list.ranked_clusters[1].attribution,
            MawVerbAttribution::WsMergeStructuredConflict
        );
    }

    #[test]
    fn empty_clusters_are_dropped() {
        // Bundle has every variant present with count=0 (per
        // `DiagnosticBundle::from_counts`'s fixed shape), but the
        // friction list drops zero-cost clusters.
        let bundles = vec![bundle("r-empty", &[], 0)];
        let list = friction_list_from_bundles(
            &bundles,
            sweep_run(1),
            FrictionSource::FirstPassClassifier,
            "2026-05-25T00:00:00Z",
            "",
        );
        assert!(
            list.ranked_clusters.is_empty(),
            "zero-cost clusters must be filtered out"
        );
    }

    // ---------- Unattributed bucket sums correctly ----------

    #[test]
    fn unattributed_bucket_is_sum_across_bundles() {
        let bundles = vec![
            bundle("a", &[(MawVerbAttribution::WsRecoverInvoked, 1)], 7),
            bundle("b", &[], 5),
            bundle("c", &[(MawVerbAttribution::WsAbortInvoked, 1)], 3),
        ];
        let list = friction_list_from_bundles(
            &bundles,
            sweep_run(3),
            FrictionSource::FirstPassClassifier,
            "2026-05-25T00:00:00Z",
            "",
        );
        assert_eq!(list.total_unattributed_wasted_turns, 15);
    }

    // ---------- JSON round-trip is stable ----------

    #[test]
    fn json_round_trip_is_stable() {
        let bundles = vec![
            bundle("r-001", &[(MawVerbAttribution::WsMergeStructuredConflict, 4)], 1),
            bundle("r-002", &[(MawVerbAttribution::WsRecoverInvoked, 2)], 0),
        ];
        let list = friction_list_from_bundles(
            &bundles,
            sweep_run(2),
            FrictionSource::FirstPassClassifier,
            "2026-05-25T12:34:56Z",
            "abc123",
        );
        let s = list.to_json().expect("serialize");
        let back = FrictionList::from_json(&s).expect("deserialize");
        assert_eq!(list, back);
    }

    // ---------- Schema pin ----------

    #[test]
    fn friction_list_schema_is_pinned() {
        let bundles = vec![bundle("r-pin", &[(MawVerbAttribution::WsMergeStructuredConflict, 1)], 0)];
        let list = friction_list_from_bundles(
            &bundles,
            sweep_run(1),
            FrictionSource::FirstPassClassifier,
            "1970-01-01T00:00:00Z",
            "pinned-sha",
        );
        let s = list.to_json().expect("ser");
        // Field-presence assertions — SG4 reads these.
        for field in [
            "schema_version",
            "sweep_run",
            "source",
            "ranked_clusters",
            "rank",
            "attribution",
            "total_cost_turns",
            "occurrence_count",
            "evidence_run_ids",
            "evidence_overflow_count",
            "example_transcript_excerpts",
            "total_unattributed_wasted_turns",
            "generated_at_utc",
            "harness_commit_sha",
        ] {
            assert!(
                s.contains(field),
                "SG4 consumer would break: schema missing field {field:?}\n{s}"
            );
        }
        // Source enum slug stable.
        assert!(s.contains("first_pass_classifier"));
    }

    #[test]
    fn schema_version_constant_is_one() {
        assert_eq!(FRICTION_LIST_SCHEMA_VERSION, 1);
    }

    // ---------- Renderer produces one section per cluster ----------

    #[test]
    fn renderer_emits_one_section_per_cluster() {
        let bundles = vec![bundle(
            "r-doc",
            &[
                (MawVerbAttribution::WsMergeStructuredConflict, 5),
                (MawVerbAttribution::WsRecoverInvoked, 2),
                (MawVerbAttribution::VocabularyScarcity, 1),
            ],
            3,
        )];
        let list = friction_list_from_bundles(
            &bundles,
            sweep_run(1),
            FrictionSource::FirstPassClassifier,
            "2026-05-25T00:00:00Z",
            "doc-sha",
        );
        let md = render_friction_list_md(&list);
        // Header.
        assert!(md.starts_with("# SG2 prioritized friction list"));
        // TEMPLATE banner.
        assert!(md.contains("TEMPLATE"));
        // One `## #N — ` heading per cluster.
        let section_count = md.matches("## #").count();
        assert_eq!(section_count, 3, "expected one section per cluster:\n{md}");
        // VocabularyScarcity call-out is present.
        assert!(md.contains("agent-fluency principle measurement"));
        // Unattributed bucket section, surfaced explicitly.
        assert!(md.contains("## Unattributed bucket"));
        assert!(md.contains("total_unattributed_wasted_turns` = **3**"));
        // SG4 handoff section.
        assert!(md.contains("## SG4 handoff"));
        assert!(md.contains("bn-2j45"));
        // Recommended-fix-class hints rendered.
        assert!(md.contains("merge-engine-resilience"));
    }

    #[test]
    fn renderer_handles_empty_friction_list() {
        let bundles: Vec<DiagnosticBundle> = vec![bundle("r-clean", &[], 0)];
        let list = friction_list_from_bundles(
            &bundles,
            sweep_run(1),
            FrictionSource::FirstPassClassifier,
            "2026-05-25T00:00:00Z",
            "",
        );
        let md = render_friction_list_md(&list);
        assert!(md.contains("No attributed friction observed"));
        // Still emits the unattributed bucket section with 0.
        assert!(md.contains("## Unattributed bucket"));
        assert!(md.contains("`total_unattributed_wasted_turns` = **0**"));
        // SG4 handoff still emitted (consumer contract is unconditional).
        assert!(md.contains("## SG4 handoff"));
    }

    // ---------- no_composite invariant lifted from T2.4 ----------

    #[test]
    fn renderer_emits_no_composite_score() {
        // Mixed-cost set: would tempt a "winner / overall score"
        // summary. Renderer must not add one.
        let bundles = vec![
            bundle(
                "r-1",
                &[
                    (MawVerbAttribution::WsMergeStructuredConflict, 5),
                    (MawVerbAttribution::WsSyncStaleWorkspace, 2),
                ],
                0,
            ),
            bundle(
                "r-2",
                &[
                    (MawVerbAttribution::WsRecoverInvoked, 4),
                    (MawVerbAttribution::EpochSyncRequired, 1),
                ],
                0,
            ),
        ];
        let list = friction_list_from_bundles(
            &bundles,
            sweep_run(2),
            FrictionSource::FirstPassClassifier,
            "2026-05-25T00:00:00Z",
            "",
        );
        let md = render_friction_list_md(&list).to_ascii_lowercase();
        for forbidden in [
            "composite",
            "weighted",
            "winner:",
            "overall score",
            "severity score",
            "leaderboard",
            "score =",
            "total score",
        ] {
            assert!(
                !md.contains(forbidden),
                "renderer emitted forbidden composite token {forbidden:?}:\n{md}"
            );
        }
        // Required: explicit reminder that ranking is within-axis only.
        assert!(md.contains("no cross-axis aggregation"));
    }

    // ---------- Accepts mixed arms (non-maw bundles contribute 0) ----------

    #[test]
    fn accepts_mixed_arms() {
        // Non-maw bundle is empty by construction.
        let jj = DiagnosticBundle::empty_for("jj-workspaces", "r-jj");
        let maw = bundle("r-maw", &[(MawVerbAttribution::WsMergeStructuredConflict, 4)], 2);
        let list = friction_list_from_bundles(
            &[jj, maw],
            sweep_run(2),
            FrictionSource::FirstPassClassifier,
            "2026-05-25T00:00:00Z",
            "",
        );
        assert_eq!(list.ranked_clusters.len(), 1);
        assert_eq!(
            list.ranked_clusters[0].attribution,
            MawVerbAttribution::WsMergeStructuredConflict
        );
        assert_eq!(list.ranked_clusters[0].total_cost_turns, 4);
        assert_eq!(list.total_unattributed_wasted_turns, 2);
    }

    // ---------- Evidence cap + overflow count ----------

    #[test]
    fn evidence_run_ids_cap_and_overflow() {
        // 7 bundles all firing the same cluster -> 7 evidence ids
        // collapsed to EVIDENCE_RUN_ID_CAP + overflow = 7 - cap.
        let mut bundles = Vec::new();
        for i in 0..7 {
            bundles.push(bundle(
                &format!("r-{i:02}"),
                &[(MawVerbAttribution::WsRecoverInvoked, 1)],
                0,
            ));
        }
        let list = friction_list_from_bundles(
            &bundles,
            sweep_run(7),
            FrictionSource::FirstPassClassifier,
            "2026-05-25T00:00:00Z",
            "",
        );
        let cluster = &list.ranked_clusters[0];
        assert_eq!(cluster.evidence_run_ids.len(), EVIDENCE_RUN_ID_CAP);
        assert_eq!(
            cluster.evidence_overflow_count as usize,
            7 - EVIDENCE_RUN_ID_CAP
        );
        assert_eq!(cluster.occurrence_count, 7);
    }
}
