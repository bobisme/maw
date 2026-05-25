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

/// One tool call the agent made. The pre-registration §1.1 counts
/// `tool_calls` per run as `len(every turn's tool_calls)`.
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
    /// Current schema version. Bumped only when an incompatible field
    /// change is made; downstream parsers gate on this.
    pub const SCHEMA_VERSION: u32 = 1;

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
        assert_eq!(BenchRun::SCHEMA_VERSION, 1);
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
