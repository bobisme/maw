//! T2.5 (`bn-1rgk`) — maw-per-verb wasted-turn attribution.
//!
//! # What this module does
//!
//! Attributes a [`maw_bench::ToolCall`] (with optional `prior_outcome`
//! context) to a named [`MawVerbAttribution`] cluster. The output is
//! the input format T2.8 (`bn-u9iy`, diagnostic report) consumes to
//! produce the prioritized friction list.
//!
//! # Why this is maw's interesting metric
//!
//! From the bone: "Failure asymmetry: worktrees/jj LOSE/WEDGE work;
//! maw PRESERVES it but may cost recovery turns — so this, NOT 'work
//! lost' (~0 for maw by design), is maw's interesting metric and the
//! instrument for confidence gap #2."
//!
//! The attribution is **maw-specific by definition** — non-maw arms
//! (worktrees, jj-workspaces, claude-native) have no maw verbs to
//! attribute to. For those arms, the diagnostic block renders
//! `n/a (substrate has no maw verbs)` and the per-arm
//! [`DiagnosticBundle`] is empty.
//!
//! # Conservative attribution
//!
//! When uncertain, [`attribute_tool_call`] returns `None`. The
//! downstream cost of mis-attribution is much higher than the cost of
//! a missed attribution (which surfaces as the
//! [`DiagnosticBundle::total_unattributed_wasted_turns`] count so the
//! analyst sees what the heuristic missed).

use std::collections::BTreeMap;
use std::fmt;

use maw_bench::run::{OpClass, StepOutcome, ToolCall};
use serde::{Deserialize, Serialize};

/// Per-verb wasted-turn attribution clusters.
///
/// Each variant names a *specific maw friction point* the transcript
/// can attribute a wasted turn to. The variants are deliberately
/// scoped to the maw CLI surface; non-maw substrates have no concept
/// here.
///
/// # Variant taxonomy
///
/// Variants split into three families:
///
/// - **Verb-failures** (`WsCreate*`, `WsMerge*`, `WsSync*`, `WsResolve*`,
///   `WsDestroy*`, `WsRecover`, `WsAbort`, `EpochSync*`): the agent
///   tried a maw verb and either the verb refused, surfaced a
///   conflict, or behaved differently than the agent expected.
/// - **State-misreads** (`ReadFromStaleWorkspace`,
///   `ReadFromConflictedWorkspace`, `ReadFromDetachedHead`): the agent
///   inspected workspace state and acted on a misinterpretation
///   (e.g. proceeded as if the workspace were clean when it was
///   conflicted, or treated stale-output as authoritative).
/// - **VocabularyScarcity**: the agent issued a verb that doesn't
///   exist or used a flag the wrong way — the bone's "scarce maw
///   vocabulary" cluster. Captures the agent-fluency confidence gap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MawVerbAttribution {
    /// `maw ws create` failed because a workspace of that name already
    /// existed, or `--from` resolved to an unexpected ref. Often
    /// signals "agent forgot it already created this workspace earlier"
    /// — a memory/state-tracking friction.
    WsCreateNameClash,
    /// `maw ws merge ... --check` (or implicit merge) reported a
    /// structured conflict (`StepOutcome { conflicted: true }`). The
    /// agent then issued a recovery op of the same class on the same
    /// target — a wasted turn attributable to the merge surface.
    WsMergeStructuredConflict,
    /// `maw ws sync` was needed because the workspace went stale
    /// (epoch advanced underneath it). Counted when the agent retried
    /// a verb that previously failed with a stale-workspace signal.
    WsSyncStaleWorkspace,
    /// `maw ws resolve` was issued more than once on the same
    /// workspace, indicating the agent's first resolution attempt did
    /// not actually clear the conflict state.
    WsResolveRetry,
    /// `maw ws destroy` refused because the workspace had unmerged
    /// changes (Prime Invariant guard); agent then either re-issued
    /// with `--force` or worked around. Counted because the refusal
    /// burns a turn even though it's correct behavior.
    WsDestroyRefused,
    /// `maw ws recover` was issued — by definition a wasted-recovery
    /// turn (the agent's earlier destroy or sync left something that
    /// needed rescuing).
    WsRecoverInvoked,
    /// `maw ws abort` was issued to cancel an in-flight operation.
    /// The work itself isn't lost (Prime Invariant) but the turn
    /// spent aborting is wasted relative to forward progress.
    WsAbortInvoked,
    /// `maw epoch sync` was needed (workspace's epoch baseline
    /// drifted from the integration branch). Agents often miss this
    /// step; the cluster names that miss.
    EpochSyncRequired,
    /// Agent read `maw ws status` / `list` / `diff` output but its
    /// next op was inconsistent with a stale workspace (e.g. tried
    /// to commit on top of a stale base then merge). The state was
    /// in the output; the agent misread it.
    ReadFromStaleWorkspace,
    /// Agent read workspace state but its next op ignored a
    /// `conflicted` flag in the output — i.e. it tried to merge or
    /// destroy a workspace that the status said was conflicted.
    ReadFromConflictedWorkspace,
    /// Agent inspected git state and acted as if HEAD pointed
    /// somewhere it didn't (detached HEAD post-rebase or after a
    /// failed merge). Less common in maw than in plain worktrees;
    /// named here because the maw-CLI surface can still expose it.
    ReadFromDetachedHead,
    /// Agent issued a verb / flag combination that doesn't exist in
    /// the maw CLI. Captures the "scarce maw vocabulary" friction
    /// from the agent-fluency mitigation: the agent had to discover
    /// the correct surface by trial and error.
    VocabularyScarcity,
}

impl MawVerbAttribution {
    /// All variants in stable ordering. Used by reporters and tests
    /// to iterate without depending on macro magic.
    pub const ALL: &'static [Self] = &[
        Self::WsCreateNameClash,
        Self::WsMergeStructuredConflict,
        Self::WsSyncStaleWorkspace,
        Self::WsResolveRetry,
        Self::WsDestroyRefused,
        Self::WsRecoverInvoked,
        Self::WsAbortInvoked,
        Self::EpochSyncRequired,
        Self::ReadFromStaleWorkspace,
        Self::ReadFromConflictedWorkspace,
        Self::ReadFromDetachedHead,
        Self::VocabularyScarcity,
    ];

    /// Short slug used in the diagnostic-table row label. Matches the
    /// serde rename so JSON and printout agree.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::WsCreateNameClash => "ws_create_name_clash",
            Self::WsMergeStructuredConflict => "ws_merge_structured_conflict",
            Self::WsSyncStaleWorkspace => "ws_sync_stale_workspace",
            Self::WsResolveRetry => "ws_resolve_retry",
            Self::WsDestroyRefused => "ws_destroy_refused",
            Self::WsRecoverInvoked => "ws_recover_invoked",
            Self::WsAbortInvoked => "ws_abort_invoked",
            Self::EpochSyncRequired => "epoch_sync_required",
            Self::ReadFromStaleWorkspace => "read_from_stale_workspace",
            Self::ReadFromConflictedWorkspace => "read_from_conflicted_workspace",
            Self::ReadFromDetachedHead => "read_from_detached_head",
            Self::VocabularyScarcity => "vocabulary_scarcity",
        }
    }
}

impl fmt::Display for MawVerbAttribution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}

/// One per-verb cluster row in the [`DiagnosticBundle`].
///
/// `evidence_run_ids` carries up to N run-ids where this attribution
/// fired, so the T2.8 report can link from a cluster row back to the
/// transcripts that motivated it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerVerbCluster {
    /// The attribution this row counts.
    pub attribution: MawVerbAttribution,
    /// Number of wasted turns attributed to this cluster across the
    /// bundle's run set.
    pub count: u32,
    /// Run-ids whose transcripts contributed at least one attributed
    /// turn to this cluster. Bounded (T2.8 caps the per-cluster
    /// evidence list size at render time).
    pub evidence_run_ids: Vec<String>,
}

/// The stable JSON schema T2.8 (`bn-u9iy`, diagnostic report) consumes
/// to produce the prioritized friction list.
///
/// # One bundle per `(arm, run-set)`
///
/// A bundle is **per-arm by definition** — only the maw arm has maw
/// verbs to attribute to. For non-maw arms, T2.8 may still construct
/// an empty bundle (so its input shape is consistent) but every cluster
/// count is 0 and `total_attributed_wasted_turns + total_unattributed_wasted_turns = 0`.
///
/// # Schema stability
///
/// This is the T2.8 input contract. Pinned by the
/// `diagnostic_bundle_schema_is_pinned` test (fixture-backed) so any
/// downstream schema break is caught at the producer side, not when
/// T2.8 fails to parse a real run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticBundle {
    /// Bundle schema version. Bumped when this schema's *consumed*
    /// shape changes; additive fields do NOT bump.
    pub schema_version: u32,
    /// The run id this bundle was extracted from. When a bundle
    /// aggregates multiple runs (T2.8), this is the first run id in
    /// the set — the per-cluster `evidence_run_ids` carries the full
    /// list.
    pub run_id: String,
    /// Arm under test. Echoes `MetricRecord.arm`.
    pub arm: String,
    /// One row per attribution cluster, in stable
    /// `MawVerbAttribution::ALL` order. Clusters with `count == 0`
    /// are included so the schema is fixed-shape (T2.8 can rely on
    /// `len(per_verb_clusters)` being a constant).
    pub per_verb_clusters: Vec<PerVerbCluster>,
    /// Sum of `per_verb_clusters[*].count`. Sanity-check field; T2.8
    /// asserts this matches the per-cluster sum to catch encoding
    /// drift.
    pub total_attributed_wasted_turns: u32,
    /// Wasted turns the heuristic detected but could not attribute to
    /// a named cluster. T2.8 surfaces this as "friction the report
    /// missed — coder follow-up needed".
    pub total_unattributed_wasted_turns: u32,
}

impl DiagnosticBundle {
    /// Current schema version.
    pub const SCHEMA_VERSION: u32 = 1;

    /// Construct an empty bundle for `arm` / `run_id`. All clusters
    /// present with `count = 0`. Used as the default for non-maw arms.
    #[must_use]
    pub fn empty_for(arm: &str, run_id: &str) -> Self {
        let per_verb_clusters = MawVerbAttribution::ALL
            .iter()
            .copied()
            .map(|a| PerVerbCluster {
                attribution: a,
                count: 0,
                evidence_run_ids: Vec::new(),
            })
            .collect();
        Self {
            schema_version: Self::SCHEMA_VERSION,
            run_id: run_id.to_string(),
            arm: arm.to_string(),
            per_verb_clusters,
            total_attributed_wasted_turns: 0,
            total_unattributed_wasted_turns: 0,
        }
    }

    /// Build a bundle from an attribution count map. The map's keys
    /// can be a subset of [`MawVerbAttribution::ALL`]; missing keys
    /// land as `count = 0` rows.
    #[must_use]
    pub fn from_counts(
        arm: &str,
        run_id: &str,
        per_verb: &BTreeMap<MawVerbAttribution, u32>,
        evidence: &BTreeMap<MawVerbAttribution, Vec<String>>,
        unattributed: u32,
    ) -> Self {
        let per_verb_clusters: Vec<PerVerbCluster> = MawVerbAttribution::ALL
            .iter()
            .copied()
            .map(|a| PerVerbCluster {
                attribution: a,
                count: per_verb.get(&a).copied().unwrap_or(0),
                evidence_run_ids: evidence.get(&a).cloned().unwrap_or_default(),
            })
            .collect();
        let total_attributed_wasted_turns: u32 = per_verb_clusters.iter().map(|c| c.count).sum();
        Self {
            schema_version: Self::SCHEMA_VERSION,
            run_id: run_id.to_string(),
            arm: arm.to_string(),
            per_verb_clusters,
            total_attributed_wasted_turns,
            total_unattributed_wasted_turns: unattributed,
        }
    }

    /// Pretty JSON. Used by T2.8's input loader and by the
    /// fixture-pinning test.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// Attribute a single tool call to a [`MawVerbAttribution`].
///
/// **Conservative.** When the call cannot be confidently attributed,
/// returns `None`. The downstream cost of mis-attribution
/// (false-positive verb friction reports) is much higher than the cost
/// of an unattributed wasted-turn (visible in
/// `DiagnosticBundle::total_unattributed_wasted_turns`).
///
/// # Inputs
///
/// - `call` — the tool call to attribute.
/// - `prior_outcome` — the substrate outcome of the *previous*
///   relevant tool call, if known. The attribution often hinges on
///   "did the agent issue this AFTER a conflict?".
///
/// # Heuristic
///
/// 1. If the call has an explicit `attributed_op`, prefer that as the
///    source of truth (the harness or post-hoc coder already classified).
/// 2. Otherwise, sniff the args_json for the maw verb tokens.
/// 3. Combine with `prior_outcome.conflicted` / `prior_outcome.ok` to
///    distinguish "retry after conflict" from "first attempt".
///
/// # Edge cases
///
/// - **Call name is not `Bash`** (e.g. Read, Edit, Glob): returns
///   `None` unless the call's `attributed_op` is set — non-Bash calls
///   are almost never maw verbs.
/// - **`args_json` mentions `maw ws` but in a comment or echo**:
///   under-attributes (returns `None` to be conservative). T2.8's
///   `total_unattributed_wasted_turns` is the visibility hook.
/// - **`prior_outcome = None`** (first turn): heuristic cannot
///   distinguish first-attempt from retry; only attributes when the
///   verb itself is intrinsically a recovery verb (recover, abort).
#[must_use]
pub fn attribute_tool_call(
    call: &ToolCall,
    prior_outcome: Option<&StepOutcome>,
) -> Option<MawVerbAttribution> {
    // 1. Honor explicit attribution if present + classifiable.
    if let Some(op) = call.attributed_op
        && let Some(att) = attribute_from_explicit_op(op, prior_outcome)
    {
        return Some(att);
    }

    // 2. Non-Bash calls (Read, Edit, Glob, etc.) are essentially
    //    never maw verbs once the explicit attribution is exhausted.
    if call.name != "Bash" {
        return None;
    }

    // 3. Sniff the args for maw verbs. Conservative: require both the
    //    `maw` token AND the specific verb to appear.
    let hay = call.args_json.to_ascii_lowercase();
    if !hay.contains("maw") {
        // Could still be a vocabulary-scarcity miss — agent typed a
        // nonexistent verb without the `maw` prefix. Without more
        // context we cannot tell; return None (the conservative path).
        return None;
    }

    // Intrinsic-recovery verbs first — these are always recovery
    // attempts whether or not we have prior context.
    if hay.contains("maw ws recover") {
        return Some(MawVerbAttribution::WsRecoverInvoked);
    }
    if hay.contains("maw ws abort") {
        return Some(MawVerbAttribution::WsAbortInvoked);
    }
    if hay.contains("maw ws resolve") {
        // Retry classification: WsResolveRetry if prior_outcome was a
        // conflict; otherwise this is initial resolution, which we
        // don't attribute (initial resolution is the correct path,
        // not friction).
        if matches!(prior_outcome, Some(o) if o.conflicted) {
            return Some(MawVerbAttribution::WsResolveRetry);
        }
        return None;
    }
    if hay.contains("maw epoch sync") {
        return Some(MawVerbAttribution::EpochSyncRequired);
    }
    if hay.contains("maw ws sync") {
        // Sync is friction iff the workspace was stale (the prior
        // outcome flagged it, or the agent is reacting to one).
        if matches!(prior_outcome, Some(o) if !o.ok || o.conflicted) {
            return Some(MawVerbAttribution::WsSyncStaleWorkspace);
        }
        return None;
    }
    if hay.contains("maw ws destroy") {
        // Friction iff destroy refused (prior_outcome.ok == false on
        // a destroy attempt). Without prior context we don't attribute.
        if matches!(prior_outcome, Some(o) if !o.ok) {
            return Some(MawVerbAttribution::WsDestroyRefused);
        }
        return None;
    }
    if hay.contains("maw ws merge") {
        // Friction iff the merge produced a structured conflict.
        if matches!(prior_outcome, Some(o) if o.conflicted) {
            return Some(MawVerbAttribution::WsMergeStructuredConflict);
        }
        return None;
    }
    if hay.contains("maw ws create") {
        // Friction iff the create attempt failed (name clash, base
        // ref invalid).
        if matches!(prior_outcome, Some(o) if !o.ok) {
            return Some(MawVerbAttribution::WsCreateNameClash);
        }
        return None;
    }
    if hay.contains("maw ws status") || hay.contains("maw ws list") || hay.contains("maw ws diff") {
        // State-misread clusters are detected by the downstream
        // attribution-extractor (which sees both this status call AND
        // the next op the agent took). attribute_tool_call alone
        // cannot decide — return None and let the extractor combine
        // calls.
        return None;
    }
    // Unknown / unrecognized verb under the `maw` prefix could be
    // vocabulary scarcity (e.g. `maw workspace create`, `maw ws new`,
    // `maw create-workspace`). Only attribute if prior context
    // confirms "this is a retry of something that didn't work".
    if matches!(prior_outcome, Some(o) if !o.ok) {
        return Some(MawVerbAttribution::VocabularyScarcity);
    }
    None
}

/// Explicit-attribution branch: when the harness or coder already
/// tagged the call with an [`OpClass`], translate that (plus prior
/// outcome) into a friction cluster.
fn attribute_from_explicit_op(
    op: OpClass,
    prior: Option<&StepOutcome>,
) -> Option<MawVerbAttribution> {
    match op {
        OpClass::Recover => Some(MawVerbAttribution::WsRecoverInvoked),
        OpClass::Abort => Some(MawVerbAttribution::WsAbortInvoked),
        OpClass::EpochSync => Some(MawVerbAttribution::EpochSyncRequired),
        OpClass::ResolveConflict => {
            if matches!(prior, Some(o) if o.conflicted) {
                Some(MawVerbAttribution::WsResolveRetry)
            } else {
                None
            }
        }
        OpClass::Sync => {
            if matches!(prior, Some(o) if !o.ok || o.conflicted) {
                Some(MawVerbAttribution::WsSyncStaleWorkspace)
            } else {
                None
            }
        }
        OpClass::Destroy => {
            if matches!(prior, Some(o) if !o.ok) {
                Some(MawVerbAttribution::WsDestroyRefused)
            } else {
                None
            }
        }
        OpClass::Merge => {
            if matches!(prior, Some(o) if o.conflicted) {
                Some(MawVerbAttribution::WsMergeStructuredConflict)
            } else {
                None
            }
        }
        OpClass::CreateWorkspace => {
            if matches!(prior, Some(o) if !o.ok) {
                Some(MawVerbAttribution::WsCreateNameClash)
            } else {
                None
            }
        }
        // Non-friction-attributable ops.
        OpClass::EditFile | OpClass::Commit | OpClass::Inspect | OpClass::Other => None,
    }
}

/// Detect a "read-from-stale" cluster: a status/list/diff call
/// followed by an op whose outcome flagged the workspace as stale.
/// Two-call window — exposed for the extractor which has the linear
/// turn sequence available.
#[must_use]
pub fn detect_stale_read(
    prior_call: &ToolCall,
    next_outcome: Option<&StepOutcome>,
) -> Option<MawVerbAttribution> {
    let hay = prior_call.args_json.to_ascii_lowercase();
    let is_status_call = hay.contains("maw ws status")
        || hay.contains("maw ws list")
        || hay.contains("maw ws diff")
        || hay.contains("maw status");
    if !is_status_call {
        return None;
    }
    let next = next_outcome?;
    if !next.ok && next.notes.to_ascii_lowercase().contains("stale") {
        return Some(MawVerbAttribution::ReadFromStaleWorkspace);
    }
    if next.conflicted && next.notes.to_ascii_lowercase().contains("conflict") {
        return Some(MawVerbAttribution::ReadFromConflictedWorkspace);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash(args: &str) -> ToolCall {
        ToolCall {
            name: "Bash".into(),
            args_json: args.into(),
            ts_unix_ms: 0,
            result_truncated: None,
            attributed_op: None,
            attributed_outcome: None,
        }
    }

    fn conflicted_outcome() -> StepOutcome {
        StepOutcome {
            ok: true,
            conflicted: true,
            advanced_integration: false,
            notes: "merge produced structured conflict".into(),
        }
    }

    fn refused_outcome() -> StepOutcome {
        StepOutcome {
            ok: false,
            conflicted: false,
            advanced_integration: false,
            notes: "substrate refused".into(),
        }
    }

    fn stale_outcome() -> StepOutcome {
        StepOutcome {
            ok: false,
            conflicted: false,
            advanced_integration: false,
            notes: "workspace is stale: epoch advanced".into(),
        }
    }

    // ---------- positive cases (one per variant) ----------

    #[test]
    fn positive_ws_create_name_clash() {
        let prior = refused_outcome();
        let call = bash(r#"{"cmd":"maw ws create alice --from main"}"#);
        assert_eq!(
            attribute_tool_call(&call, Some(&prior)),
            Some(MawVerbAttribution::WsCreateNameClash)
        );
    }

    #[test]
    fn positive_ws_merge_structured_conflict() {
        let prior = conflicted_outcome();
        let call = bash(r#"{"cmd":"maw ws merge a --into default"}"#);
        assert_eq!(
            attribute_tool_call(&call, Some(&prior)),
            Some(MawVerbAttribution::WsMergeStructuredConflict)
        );
    }

    #[test]
    fn positive_ws_sync_stale_workspace() {
        let prior = stale_outcome();
        let call = bash(r#"{"cmd":"maw ws sync"}"#);
        assert_eq!(
            attribute_tool_call(&call, Some(&prior)),
            Some(MawVerbAttribution::WsSyncStaleWorkspace)
        );
    }

    #[test]
    fn positive_ws_resolve_retry() {
        let prior = conflicted_outcome();
        let call = bash(r#"{"cmd":"maw ws resolve alice --keep both"}"#);
        assert_eq!(
            attribute_tool_call(&call, Some(&prior)),
            Some(MawVerbAttribution::WsResolveRetry)
        );
    }

    #[test]
    fn positive_ws_destroy_refused() {
        let prior = refused_outcome();
        let call = bash(r#"{"cmd":"maw ws destroy alice --force"}"#);
        assert_eq!(
            attribute_tool_call(&call, Some(&prior)),
            Some(MawVerbAttribution::WsDestroyRefused)
        );
    }

    #[test]
    fn positive_ws_recover_invoked_unconditional() {
        // Recover is intrinsically a recovery op; no prior context needed.
        let call = bash(r#"{"cmd":"maw ws recover alice --to alice2"}"#);
        assert_eq!(
            attribute_tool_call(&call, None),
            Some(MawVerbAttribution::WsRecoverInvoked)
        );
    }

    #[test]
    fn positive_ws_abort_invoked_unconditional() {
        let call = bash(r#"{"cmd":"maw ws abort"}"#);
        assert_eq!(
            attribute_tool_call(&call, None),
            Some(MawVerbAttribution::WsAbortInvoked)
        );
    }

    #[test]
    fn positive_epoch_sync_required() {
        let call = bash(r#"{"cmd":"maw epoch sync"}"#);
        assert_eq!(
            attribute_tool_call(&call, None),
            Some(MawVerbAttribution::EpochSyncRequired)
        );
    }

    #[test]
    fn positive_read_from_stale_workspace_via_detect_stale_read() {
        let prior = bash(r#"{"cmd":"maw ws status"}"#);
        let next = stale_outcome();
        assert_eq!(
            detect_stale_read(&prior, Some(&next)),
            Some(MawVerbAttribution::ReadFromStaleWorkspace)
        );
    }

    #[test]
    fn positive_read_from_conflicted_workspace_via_detect_stale_read() {
        let prior = bash(r#"{"cmd":"maw ws list"}"#);
        let next = conflicted_outcome();
        assert_eq!(
            detect_stale_read(&prior, Some(&next)),
            Some(MawVerbAttribution::ReadFromConflictedWorkspace)
        );
    }

    #[test]
    fn positive_vocabulary_scarcity_via_unknown_maw_verb_with_failure() {
        // Agent typed a verb that doesn't exist; prior call failed.
        let prior = refused_outcome();
        let call = bash(r#"{"cmd":"maw workspace new alice"}"#);
        assert_eq!(
            attribute_tool_call(&call, Some(&prior)),
            Some(MawVerbAttribution::VocabularyScarcity)
        );
    }

    #[test]
    fn positive_read_from_detached_head_is_explicit_attribution_only() {
        // ReadFromDetachedHead is reserved for post-hoc coding — the
        // automated heuristic doesn't have enough signal to attribute
        // it from a single Bash call. Verify the variant exists in ALL
        // so reporters render its row; the count remains 0 by default.
        assert!(MawVerbAttribution::ALL.contains(&MawVerbAttribution::ReadFromDetachedHead));
    }

    // ---------- negative / uncertain cases (must return None) ----------

    #[test]
    fn negative_non_bash_call_returns_none() {
        let call = ToolCall {
            name: "Read".into(),
            args_json: r#"{"path":"src/lib.rs"}"#.into(),
            ts_unix_ms: 0,
            result_truncated: None,
            attributed_op: None,
            attributed_outcome: None,
        };
        // Even with a "scary" prior outcome.
        let prior = conflicted_outcome();
        assert_eq!(attribute_tool_call(&call, Some(&prior)), None);
    }

    #[test]
    fn negative_ws_merge_without_prior_conflict_returns_none() {
        // First merge attempt, no prior conflict — this is normal
        // forward progress, not friction.
        let call = bash(r#"{"cmd":"maw ws merge a --into default"}"#);
        assert_eq!(attribute_tool_call(&call, None), None);
    }

    #[test]
    fn negative_args_without_maw_token_returns_none() {
        // Agent ran a plain `git rebase` — could be friction in
        // worktrees arm but not attributable to a maw verb.
        let call = bash(r#"{"cmd":"git rebase main"}"#);
        let prior = conflicted_outcome();
        assert_eq!(attribute_tool_call(&call, Some(&prior)), None);
    }

    #[test]
    fn negative_status_call_alone_returns_none() {
        // A `maw ws status` by itself isn't friction; it's friction
        // when the next op shows a misread. attribute_tool_call alone
        // cannot tell, so it returns None.
        let call = bash(r#"{"cmd":"maw ws status"}"#);
        assert_eq!(attribute_tool_call(&call, None), None);
    }

    #[test]
    fn negative_initial_resolve_without_conflict_returns_none() {
        // First resolve attempt without prior conflict signal: not friction.
        let call = bash(r#"{"cmd":"maw ws resolve alice --list"}"#);
        assert_eq!(attribute_tool_call(&call, None), None);
    }

    // ---------- explicit-attribution branch ----------

    #[test]
    fn explicit_op_class_is_authoritative() {
        // When attributed_op is set, the heuristic prefers it over
        // arg-sniffing — the harness already did the work.
        let call = ToolCall {
            name: "Bash".into(),
            args_json: r#"{"cmd":"some opaque wrapper command"}"#.into(),
            ts_unix_ms: 0,
            result_truncated: None,
            attributed_op: Some(OpClass::Recover),
            attributed_outcome: None,
        };
        assert_eq!(
            attribute_tool_call(&call, None),
            Some(MawVerbAttribution::WsRecoverInvoked)
        );
    }

    #[test]
    fn explicit_op_class_inspect_does_not_attribute() {
        // Inspect ops are deliberately not attributed; they only
        // create friction in combination with the next op's outcome.
        let call = ToolCall {
            name: "Bash".into(),
            args_json: r#"{"cmd":"maw ws status"}"#.into(),
            ts_unix_ms: 0,
            result_truncated: None,
            attributed_op: Some(OpClass::Inspect),
            attributed_outcome: None,
        };
        assert_eq!(attribute_tool_call(&call, None), None);
    }

    // ---------- DiagnosticBundle ----------

    #[test]
    fn empty_diagnostic_bundle_has_all_variants_zero() {
        let b = DiagnosticBundle::empty_for("git-worktrees-bare", "r-001");
        assert_eq!(b.schema_version, DiagnosticBundle::SCHEMA_VERSION);
        assert_eq!(b.per_verb_clusters.len(), MawVerbAttribution::ALL.len());
        assert_eq!(b.total_attributed_wasted_turns, 0);
        assert!(b.per_verb_clusters.iter().all(|c| c.count == 0));
    }

    #[test]
    fn diagnostic_bundle_from_counts_aggregates_correctly() {
        let mut counts: BTreeMap<MawVerbAttribution, u32> = BTreeMap::new();
        counts.insert(MawVerbAttribution::WsMergeStructuredConflict, 3);
        counts.insert(MawVerbAttribution::WsRecoverInvoked, 1);
        let mut evidence: BTreeMap<MawVerbAttribution, Vec<String>> = BTreeMap::new();
        evidence.insert(
            MawVerbAttribution::WsMergeStructuredConflict,
            vec!["r-001".into(), "r-002".into()],
        );
        let b = DiagnosticBundle::from_counts("maw", "r-001", &counts, &evidence, 2);
        assert_eq!(b.total_attributed_wasted_turns, 4);
        assert_eq!(b.total_unattributed_wasted_turns, 2);
        let merge_row = b
            .per_verb_clusters
            .iter()
            .find(|c| c.attribution == MawVerbAttribution::WsMergeStructuredConflict)
            .expect("row present");
        assert_eq!(merge_row.count, 3);
        assert_eq!(merge_row.evidence_run_ids.len(), 2);
    }

    // ---------- Pinned schema fixture ----------

    #[test]
    fn diagnostic_bundle_schema_is_pinned() {
        // Pin the T2.8-input schema as a fixture so any field rename
        // or removal is caught here, not when T2.8 fails on real input.
        let mut counts: BTreeMap<MawVerbAttribution, u32> = BTreeMap::new();
        counts.insert(MawVerbAttribution::WsMergeStructuredConflict, 1);
        let evidence: BTreeMap<MawVerbAttribution, Vec<String>> = BTreeMap::new();
        let b = DiagnosticBundle::from_counts("maw", "pinned-fixture", &counts, &evidence, 0);
        let s = b.to_json().expect("serialize");
        // Field presence assertions — the surface T2.8 reads.
        for field in [
            "schema_version",
            "run_id",
            "arm",
            "per_verb_clusters",
            "attribution",
            "count",
            "evidence_run_ids",
            "total_attributed_wasted_turns",
            "total_unattributed_wasted_turns",
        ] {
            assert!(
                s.contains(field),
                "fixture missing field {field:?}; T2.8 consumer would break:\n{s}"
            );
        }
        // Stable variant slug.
        assert!(s.contains("ws_merge_structured_conflict"));
    }
}
