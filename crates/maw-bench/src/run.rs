//! [`BenchRun`] — the per-run JSON record T2.4 consumes.
//!
//! The schema below is the *raw event stream* the pre-registration's §1.1
//! metric table is derived from. T2.4 (`bn-1uzn` analysis bone) reads
//! these files; T2.6 (`bn-3l1f` sweep) writes one file per run; the
//! publication (`bn-2xfn`) cites the dataset.
//!
//! # Schema design choices
//!
//! - **One file per run.** Mirrors the FailureBundle / corpus pattern in
//!   `tests/corpus/dst/*.json` (T1.6) — one record per failing seed, one
//!   record per measured run.
//! - **`serde_json` with stable field order.** All structs use derived
//!   `Serialize`; for fields that need stable insertion order (none
//!   currently — no `HashMap`s), we'd use `BTreeMap`. The schema is
//!   stable enough that downstream parsers can `serde_json::from_value`
//!   into a partial subset.
//! - **Manifest is embedded in every record.** §6.4 of the pre-reg lists
//!   the version-capture fields; we embed them so a run file is
//!   self-describing — no out-of-band lookups needed to interpret it.
//! - **Oracle B verdict carried verbatim.** The substrate-final state is
//!   the ground truth; Oracle B is the substrate-agnostic coherence
//!   check (the bn-cm63 class). For arms where Oracle B does not apply
//!   (jj, plain git, claude-native), the harness marks it
//!   [`OracleBSummary::NotApplicable`] so the dataset clearly
//!   distinguishes "green" from "not checked".

use serde::{Deserialize, Serialize};

pub use crate::substrate::SubstrateLabel as Substrate;

/// Substrate-op classification a tool call maps to (schema v2 addition).
///
/// Mirrors the per-op vocabulary of [`maw_bench_adapters::Substrate`]
/// (T2.3 / `bn-mit2`). Carried as an opt-in attribution rather than
/// re-derived at read time so the on-disk record IS the audit trail —
/// downstream analysts see what the harness (or post-hoc coder)
/// attributed without re-running the heuristic.
///
/// `Other` is the explicit "the tool call was issued but does not map
/// to a substrate-op verb" bucket (Read / Glob / web search / etc.).
/// Distinguished from "no attribution" (`ToolCall.attributed_op =
/// None`) so a future re-coding pass can tell "we looked and saw
/// nothing maw-shaped" from "we never looked".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpClass {
    /// `maw ws create <ws> [--from ...]`. Adapter verb:
    /// `Substrate::create_workspace`.
    CreateWorkspace,
    /// File edit inside a workspace. Adapter verb: `Substrate::edit_file`.
    EditFile,
    /// `git commit` / equivalent inside a workspace. Adapter verb:
    /// `Substrate::commit`.
    Commit,
    /// `maw ws merge ... [--check] [--destroy]` / equivalent merge into
    /// the integration label. Adapter verb: `Substrate::merge`.
    Merge,
    /// `maw ws sync` (refresh stale workspace to current epoch).
    /// Adapter verb: `Substrate::sync`.
    Sync,
    /// `maw ws destroy <ws> [--force]`. Adapter verb:
    /// `Substrate::destroy`.
    Destroy,
    /// `maw ws resolve <ws> [--list] [--keep ...]` — applies a
    /// conflict-resolution decision. No 1:1 adapter verb; conflict
    /// resolution lives in the agent loop, not the substrate trait,
    /// but we name the op class so attribution can talk about it.
    ResolveConflict,
    /// `maw ws recover [<ws>] [--to <new>] [--show ...] [--search ...]`.
    /// The Prime-Invariant rescue path; named so attribution can
    /// distinguish recovery from progress.
    Recover,
    /// `maw ws abort` / cancellation of an in-flight rebase or merge.
    /// Named so a "started rebase, panicked, aborted" cluster is
    /// distinguishable from a successful rebase + cleanup.
    Abort,
    /// `maw epoch sync` — advances the workspace's epoch baseline
    /// after a direct commit to the integration branch.
    EpochSync,
    /// `maw ws status` / `maw ws list` / `maw ws diff` / `maw status` —
    /// read-only inspection. Named because state-misread clusters
    /// often start with a poorly-interpreted status call.
    Inspect,
    /// Tool call was issued but does not map to a substrate-op verb
    /// (e.g. `Read` of a source file, generic `Glob`, `WebSearch`).
    /// Explicit so "we looked and it's non-op" is distinguishable
    /// from "we never looked" (the latter is `attributed_op = None`).
    Other,
}

/// Substrate-visible side-effect outcome for one op. Schema v2
/// addition. **Inlined** (not imported from `maw-bench-adapters`) so
/// the `BenchRun` record stays self-describing and `maw-bench` keeps
/// its current dep set — adapters depend on `maw-scenario` but
/// `maw-bench` deliberately does not pull `maw-bench-adapters` (would
/// introduce a coupling cycle). The field set matches
/// [`maw_bench_adapters::StepOutcome`] exactly so the two structs are
/// interconvertible by the harness's per-arm attribution code.
///
/// The "outcome" attached to a tool call is the *substrate result the
/// tool call produced*, NOT the precondition. So when an attribution
/// extractor looks at `turn[N].tool_calls[i].attributed_outcome`, it
/// reads the substrate's verdict on that specific op; whether the
/// agent retried because of a *prior* conflict is determined by
/// inspecting `turn[N-k].tool_calls[*].attributed_outcome`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepOutcome {
    /// True iff the substrate completed the op without an
    /// adapter-visible error.
    pub ok: bool,
    /// True iff the op succeeded but left a substrate-visible conflict
    /// the agent must resolve (jj-style: conflict is data, not error).
    pub conflicted: bool,
    /// True iff the op advanced the integration point (epoch for maw,
    /// merge commit on target branch for worktrees, etc.).
    pub advanced_integration: bool,
    /// Free-form per-adapter notes (bounded length; treat as audit
    /// surface, not load-bearing for metric computation).
    pub notes: String,
}

/// One tool call the agent made. The pre-registration §1.1 counts
/// `tool_calls` per run as `len(every turn's tool_calls)`.
///
/// # Schema v2 additions (T2.5 / `bn-1rgk`)
///
/// Two **optional** fields were added in schema v2:
///
/// - `attributed_op`: the substrate-op verb this call maps to (if any).
/// - `attributed_outcome`: the substrate's result for this op (if any).
///
/// Both default to `None` for v1 records, preserving backwards
/// compatibility. See the schema-version doc-comment on [`BenchRun`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    /// Tool name (e.g. `Bash`, `Read`, `Edit`).
    pub name: String,
    /// JSON-serialised arguments (verbatim — the analyst classifies, the
    /// harness doesn't interpret).
    pub args_json: String,
    /// Local wall-clock at issue time. Diagnostic; not load-bearing.
    pub ts_unix_ms: u64,
    /// Optional tool result text (truncated to bound JSON size; the
    /// harness's responsibility is the event stream, not the data
    /// stream — analyst can re-derive full results from substrate logs
    /// when needed).
    pub result_truncated: Option<String>,
    /// Schema v2: substrate-op classification, if attributed. `None`
    /// means "not classified" (either v1 record or post-hoc coding
    /// not yet run); `Some(OpClass::Other)` means "classified as
    /// non-op". The two are deliberately distinguishable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributed_op: Option<OpClass>,
    /// Schema v2: substrate result for the attributed op, if known.
    /// `None` is again distinct from "no outcome" — a Read call has
    /// no substrate outcome to report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributed_outcome: Option<StepOutcome>,
}

/// One agent turn — what the model produced this round.
///
/// Mirrors [`crate::agent::AgentTurn`]; carried separately so the
/// on-disk schema is decoupled from the in-memory backend type.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Turn {
    /// 1-based turn index.
    pub index: u32,
    /// Local wall-clock when the harness received this turn.
    pub ts_unix_ms: u64,
    /// Model's natural-language reply.
    pub reply_text: String,
    /// Tool calls issued in this turn (zero or more).
    pub tool_calls: Vec<ToolCall>,
}

/// The full prompt-to-final-reply transcript. Stored once per run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transcript {
    /// The exact bytes sent to the agent (deterministic given the seed —
    /// see [`crate::render_prompt`]).
    pub prompt: String,
    /// SHA-256 of [`Transcript::prompt`]. The §6.4 manifest's
    /// `prompt_hash` field. Computed lazily by the harness so re-running
    /// a saved record validates the prompt was not edited.
    pub prompt_sha256: String,
    /// The per-arm convention crib the agent received (frozen per §8.1).
    pub convention_text: String,
    /// Every turn in order.
    pub turns: Vec<Turn>,
}

/// Oracle B summary embedded in the [`BenchRun`].
///
/// The harness invokes [`maw_assurance::oracle_b::check`] against the
/// substrate's `repo_root` at end-of-run. The result is one of:
///
/// - [`OracleBSummary::Green`] — zero violations, full Oracle B pass.
/// - [`OracleBSummary::Red`] — at least one violation; the list is
///   serialized verbatim (display form) so the analyst can root-cause.
/// - [`OracleBSummary::NotApplicable`] — the arm under test is not a
///   maw substrate; running Oracle B would compare against the wrong
///   ref shape. The harness still records this explicitly so a dataset
///   row is never silently missing the field.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum OracleBSummary {
    /// Oracle B reported zero violations.
    Green,
    /// Oracle B reported one or more violations.
    Red {
        /// Display-form violation strings (one per violation, B1→B2→B3→B4
        /// order from [`maw_assurance::oracle_b::check`]).
        violations: Vec<String>,
    },
    /// Oracle B was not run (substrate is not a maw substrate).
    NotApplicable {
        /// Why it was skipped, e.g. `"arm = jj-workspaces; B-predicate
        /// scoped to maw refs"`.
        reason: String,
    },
}

/// Top-level verdict the harness assigns to a run.
///
/// This is a **harness-side** classification (whether the agent finished
/// the planned task battery and whether Oracle B stayed green). It is
/// NOT the publication's dominance verdict — that one is computed by T2.6
/// over many runs per the §4.3 dominance rule.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RunVerdict {
    /// Agent finished the planned task battery AND Oracle B stayed green.
    Success,
    /// Agent finished but Oracle B reported violations. Substrate
    /// coherence failure — counts as `counted_substrate_failure` in §8.7.
    SubstrateIncoherent,
    /// Agent did not finish the planned task battery (hit `max_turns`,
    /// max budget, or signalled give-up). Counts as
    /// `counted_agent_failure` in §8.7 unless the agent's stop reason is
    /// `provider_error` (then it's `discard_external_service_outage`).
    AgentFailed {
        /// Mirrors [`crate::agent::AgentReply::stop_reason`] verbatim.
        reason: String,
    },
}

/// §6.4 version-capture manifest, embedded in every [`BenchRun`].
///
/// Fields match the frozen list in §6.4 of `notes/sg2-benchmark-preregistration.md`
/// modulo the ones T2.6 fills in at sweep time (`arm_order_index`,
/// `replicate_id`, `retry_count`, `discard_class`, `discard_reason`,
/// `host_id`). T2.2 emits the per-run manifest; T2.6 attaches the
/// sweep-position fields when it composes the dataset.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct RunManifest {
    /// E.g. `"claude-code/2.1.143"`. Empty for [`crate::MockAgent`].
    pub claude_code_version: String,
    /// E.g. `"sonnet"`. Echoes [`crate::AgentConfig::model`].
    pub claude_model_id: String,
    /// The model id the response envelope reports (in case routing
    /// redirects). Empty for [`crate::MockAgent`].
    pub claude_effective_model: String,
    /// Captured by the harness via `git --version`. Empty if the harness
    /// could not query git (e.g. NoopSubstrate self-test).
    pub git_version: String,
    /// Captured by the harness via `jj --version`. Empty unless the
    /// substrate needs jj.
    pub jj_version: String,
    /// `commit SHA + tag` of the installed maw under test (the substrate
    /// adapter is the source of truth — Noop/Mock self-tests leave empty).
    pub maw_version: String,
    /// Compile-time commit of this `maw-bench` crate. The harness reads
    /// `CARGO_PKG_VERSION` and (when present) the env var
    /// `MAW_BENCH_GIT_SHA` set at build time.
    pub benchmark_harness_commit: String,
    /// Compile-time commit of the `maw-scenario` generator the plan came
    /// from.
    pub scenario_generator_commit: String,
    /// SHA-256 of the prompt (also stored on [`Transcript::prompt_sha256`]).
    pub prompt_hash: String,
    /// Seed that produced the [`maw_scenario::ScenarioPlan`].
    pub seed: u64,
    /// `C0..C4` (frozen condition spectrum from §5). Set by T2.6 at sweep
    /// time; T2.2 leaves this empty unless the caller supplies it via
    /// [`crate::BenchConfig::condition_id`].
    pub condition_id: String,
    /// `T0..T5` task class (frozen taxonomy from §5.1). Same handoff
    /// shape as `condition_id`.
    pub t_class: String,
    /// Arm under test. Echoes [`crate::Substrate::label`].
    pub arm: String,
    /// OS / kernel string. Captured via `uname -srm`.
    pub os_kernel: String,
    /// Run start in Unix milliseconds (UTC).
    pub start_ts_unix_ms: u64,
    /// Run end in Unix milliseconds (UTC).
    pub end_ts_unix_ms: u64,
}

/// The complete per-run record. One JSON file per run; the schema T2.4
/// consumes.
///
/// # Stable field order
///
/// The struct's field order matches the schema's JSON field order
/// (serde respects struct field order by default). T2.4 parsers should
/// NOT depend on field order, but we keep it intentional so a `git diff`
/// of two run files is readable.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchRun {
    /// Schema version. Bump if a field is removed or its type changes.
    /// Adding optional fields does NOT bump.
    pub schema_version: u32,
    /// Stable, opaque run id. Derived from `(seed, arm, condition_id,
    /// t_class, replicate_id)` if the caller supplies them; otherwise a
    /// timestamp+seed string. The harness ensures uniqueness within an
    /// artifact dir.
    pub run_id: String,
    /// §6.4 manifest, embedded.
    pub manifest: RunManifest,
    /// Final harness verdict.
    pub verdict: RunVerdict,
    /// Oracle B end-of-run check.
    pub oracle_b: OracleBSummary,
    /// Full transcript.
    pub transcript: Transcript,
    /// Total tool calls counted across every turn (denormalized for
    /// quick parsing; equals `sum(len(turn.tool_calls))`).
    pub total_tool_calls: u32,
    /// Total turns produced. Equals `len(transcript.turns)`.
    pub total_turns: u32,
    /// `total_cost_usd` from the provider envelope. `None` for
    /// [`crate::MockAgent`] runs.
    pub cost_usd: Option<f64>,
    /// `duration_ms` = `manifest.end_ts_unix_ms - manifest.start_ts_unix_ms`.
    /// Recorded redundantly per §1.1 (`duration_ms` is for completeness
    /// only; not a headline metric — SP3 §3 measured its CV at 28.4%).
    pub duration_ms: u64,
    /// Files the agent left under the workspace root, sorted (the
    /// substrate-state-at-end raw event T2.4 needs). Path strings are
    /// relative to [`crate::SubstrateHandle::workspace_root`].
    pub substrate_final_files: Vec<String>,
}

impl BenchRun {
    /// Current schema version.
    ///
    /// # Migration history
    ///
    /// - **v1 → v2 (T2.5 / `bn-1rgk`)**: added two OPTIONAL fields on
    ///   [`ToolCall`] — `attributed_op: Option<OpClass>` and
    ///   `attributed_outcome: Option<StepOutcome>`. Both default to
    ///   `None` for legacy records; v1 JSON files load cleanly into
    ///   v2 with the new fields filled with `None`. No field was
    ///   removed or had its type changed; only the schema-version
    ///   integer bumped because downstream tools (T2.6, T2.8) want
    ///   to assert "this run has v2 attribution data" before reading
    ///   the new fields.
    pub const SCHEMA_VERSION: u32 = 2;

    /// Serialize to JSON. Uses `to_string_pretty` so on-disk records are
    /// readable in a code review.
    ///
    /// # Errors
    ///
    /// Propagates `serde_json::Error` from the encoder.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Compact JSON form. Used by the determinism test: byte-identity
    /// of two runs (same seed, same MockScript, same pinned clock)
    /// implies the schema has no nondeterministic fields.
    ///
    /// # Errors
    ///
    /// Propagates `serde_json::Error` from the encoder.
    pub fn to_canonical_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_version_is_constant() {
        // v2 in T2.5: added optional attribution fields on ToolCall.
        assert_eq!(BenchRun::SCHEMA_VERSION, 2);
    }

    /// v1 → v2 migration: a v1 JSON (no attribution fields on
    /// ToolCall, schema_version=1) must deserialize cleanly into the
    /// v2 struct; the new fields default to `None`.
    #[test]
    fn v1_bench_run_loads_into_v2_with_none_attribution() {
        // A minimal v1 BenchRun JSON. Mirrors the v1 schema fields
        // exactly — no `attributed_op` or `attributed_outcome` keys.
        let v1_json = serde_json::json!({
            "schema_version": 1,
            "run_id": "legacy-1",
            "manifest": {
                "claude_code_version": "",
                "claude_model_id": "",
                "claude_effective_model": "",
                "git_version": "",
                "jj_version": "",
                "maw_version": "",
                "benchmark_harness_commit": "",
                "scenario_generator_commit": "",
                "prompt_hash": "",
                "seed": 0,
                "condition_id": "",
                "t_class": "",
                "arm": "maw",
                "os_kernel": "",
                "start_ts_unix_ms": 0,
                "end_ts_unix_ms": 0
            },
            "verdict": {"outcome": "success"},
            "oracle_b": {"verdict": "green"},
            "transcript": {
                "prompt": "",
                "prompt_sha256": "",
                "convention_text": "",
                "turns": [{
                    "index": 1,
                    "ts_unix_ms": 0,
                    "reply_text": "",
                    "tool_calls": [{
                        "name": "Bash",
                        "args_json": "{}",
                        "ts_unix_ms": 0,
                        "result_truncated": null
                    }]
                }]
            },
            "total_tool_calls": 1,
            "total_turns": 1,
            "cost_usd": null,
            "duration_ms": 0,
            "substrate_final_files": []
        });
        let run: BenchRun =
            serde_json::from_value(v1_json).expect("v1 JSON deserializes into v2 struct");
        assert_eq!(run.schema_version, 1, "schema field carries v1 verbatim");
        let tc = &run.transcript.turns[0].tool_calls[0];
        assert!(tc.attributed_op.is_none(), "missing v1 field defaults to None");
        assert!(
            tc.attributed_outcome.is_none(),
            "missing v1 field defaults to None"
        );
    }

    #[test]
    fn op_class_serializes_to_snake_case() {
        let s = serde_json::to_string(&OpClass::CreateWorkspace).expect("ser");
        assert_eq!(s, "\"create_workspace\"");
        let s = serde_json::to_string(&OpClass::ResolveConflict).expect("ser");
        assert_eq!(s, "\"resolve_conflict\"");
        let s = serde_json::to_string(&OpClass::EpochSync).expect("ser");
        assert_eq!(s, "\"epoch_sync\"");
    }

    #[test]
    fn step_outcome_default_is_all_false() {
        let s = StepOutcome::default();
        assert!(!s.ok);
        assert!(!s.conflicted);
        assert!(!s.advanced_integration);
        assert!(s.notes.is_empty());
    }

    #[test]
    fn tool_call_v2_serializes_only_set_attribution_fields() {
        // With attribution unset, the optional fields skip serialization
        // so v2 ToolCall JSON byte-matches v1 ToolCall JSON.
        let tc = ToolCall {
            name: "Bash".into(),
            args_json: "{}".into(),
            ts_unix_ms: 0,
            result_truncated: None,
            attributed_op: None,
            attributed_outcome: None,
        };
        let s = serde_json::to_string(&tc).expect("ser");
        assert!(!s.contains("attributed_op"));
        assert!(!s.contains("attributed_outcome"));

        // With attribution set, the fields appear.
        let tc = ToolCall {
            name: "Bash".into(),
            args_json: "{\"cmd\":\"maw ws merge a\"}".into(),
            ts_unix_ms: 0,
            result_truncated: None,
            attributed_op: Some(OpClass::Merge),
            attributed_outcome: Some(StepOutcome {
                ok: true,
                conflicted: true,
                advanced_integration: false,
                notes: String::new(),
            }),
        };
        let s = serde_json::to_string(&tc).expect("ser");
        assert!(s.contains("\"attributed_op\":\"merge\""));
        assert!(s.contains("\"attributed_outcome\""));
        assert!(s.contains("\"conflicted\":true"));
    }

    #[test]
    fn oracle_b_summary_serializes_to_tagged_json() {
        let g = OracleBSummary::Green;
        let s = serde_json::to_string(&g).expect("ser");
        assert!(s.contains("\"verdict\":\"green\""), "got {s}");

        let r = OracleBSummary::Red {
            violations: vec!["B1 DanglingHeadRef ws-1".to_string()],
        };
        let s = serde_json::to_string(&r).expect("ser");
        assert!(s.contains("\"verdict\":\"red\""), "got {s}");
        assert!(s.contains("DanglingHeadRef"), "got {s}");

        let na = OracleBSummary::NotApplicable {
            reason: "arm = jj".to_string(),
        };
        let s = serde_json::to_string(&na).expect("ser");
        assert!(s.contains("\"verdict\":\"not_applicable\""), "got {s}");
    }

    #[test]
    fn run_verdict_serializes_to_tagged_json() {
        let v = RunVerdict::Success;
        assert!(serde_json::to_string(&v)
            .expect("ser ok")
            .contains("\"outcome\":\"success\""));
        let v = RunVerdict::AgentFailed {
            reason: "max_turns".to_string(),
        };
        let s = serde_json::to_string(&v).expect("ser ok");
        assert!(s.contains("\"outcome\":\"agent_failed\""));
        assert!(s.contains("max_turns"));
    }
}
