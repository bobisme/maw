//! [`BenchHarness`] — the orchestrator.
//!
//! Owns the substrate, drives the agent backend, captures the trace,
//! optionally calls Oracle B, and emits a [`crate::BenchRun`] (and a
//! JSON file under the configured artifact dir).
//!
//! # End-to-end shape
//!
//! ```text
//!  ScenarioPlan ─┐
//!                ├─► render_prompt ─► AgentBackend.run ─► AgentReply
//!  Substrate ─►  │                                                 │
//!  AgentConfig ──┘                                                 ▼
//!                                                            (turns,
//!                                                             tool calls,
//!                                                             transcript)
//!                                                                 │
//!                                                                 ▼
//!                                            oracle_b::check(substrate.repo_root)
//!                                                                 │
//!                                                                 ▼
//!                                                            BenchRun JSON
//! ```
//!
//! # What this module guarantees
//!
//! - The per-run JSON is written **atomically** (write-then-rename) so a
//!   crashed harness never leaves a half-written record.
//! - Two runs with the same seed + same MockAgent script + pinned clock
//!   produce byte-identical JSON. The harness's determinism test relies
//!   on this.
//! - Oracle B is invoked only when [`BenchConfig::run_oracle_b`] is
//!   true. For arms whose ref shape doesn't match maw, the caller sets
//!   this to false and the record carries
//!   [`crate::OracleBSummary::NotApplicable`] with a reason.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use maw_scenario::ScenarioPlan;

use crate::agent::{AgentBackend, AgentConfig, AgentError};
use crate::prompt::{PromptInputs, prompt_sha256_hex, render_prompt};
use crate::run::{BenchRun, OracleBSummary, RunManifest, RunVerdict, Transcript};
use crate::substrate::{Substrate, SubstrateConfig, SubstrateError, SubstrateLabel};
use crate::version_capture::capture_versions;

/// Per-run configuration controlling the harness's behaviour.
///
/// `Default` is derived (and equivalent to "no artifact dir, no run id
/// override, empty condition/t-class, Oracle B off, real wall-clock") —
/// callers usually override one or two fields with struct-update syntax.
#[derive(Clone, Debug, Default)]
pub struct BenchConfig {
    /// Where to write per-run JSON files. The harness creates the
    /// directory if it does not exist. `None` ⇒ JSON is built in memory
    /// and returned but never written (useful for tests).
    pub artifact_dir: Option<PathBuf>,
    /// Stable run id; if `None` the harness derives one from
    /// `(seed, arm, condition_id, t_class, start_ts)`.
    pub run_id: Option<String>,
    /// `C0..C4` from §5 of the pre-reg. Empty when the caller isn't
    /// part of a §6.2 block-randomized sweep.
    pub condition_id: String,
    /// `T0..T5` from §5.1. Same as `condition_id`.
    pub t_class: String,
    /// Whether to invoke Oracle B end-of-run. False for non-maw arms
    /// (Oracle B's predicates are scoped to maw refs); the resulting
    /// record carries [`OracleBSummary::NotApplicable`].
    pub run_oracle_b: bool,
    /// Optional explanation embedded into [`OracleBSummary::NotApplicable`]
    /// when `run_oracle_b == false`. Empty ⇒ a default string is used.
    pub oracle_b_skip_reason: String,
    /// Pinned wall-clock for deterministic timestamps in the manifest.
    /// `None` ⇒ real wall clock. Tests that byte-compare JSON pin this.
    pub pinned_start_ms: Option<u64>,
    /// Same as `pinned_start_ms` but for end-of-run.
    pub pinned_end_ms: Option<u64>,
    /// bn-3hzt: chaos overlay env vars merged into
    /// [`AgentConfig::extra_env`] for this run only (the harness
    /// adds them transiently around `agent.run` and never persists
    /// them into the field). The load-bearing use is
    /// `MAW_FP=<failpoint>=error:bn-3hzt-sg2-chaos`, which arms
    /// the next `maw` invocation the agent spawns to crash at a
    /// deterministic failpoint (on a `--features failpoints`
    /// shipped binary). Empty by default — `--chaos=off` is the
    /// SG2 pre-bn-3hzt default and the existing pilot recipes
    /// pass an empty map, so this is back-compat for them.
    pub chaos_env: std::collections::BTreeMap<String, String>,
}

/// Harness-level errors. Substrate / agent errors are propagated through
/// the §8.7 discard taxonomy in the resulting [`BenchRun`] (the run is
/// still produced — the analyst sees the failure class), so these only
/// fire on truly catastrophic harness bugs (cannot write JSON, cannot
/// hash a prompt, etc.).
#[derive(Debug)]
pub enum HarnessError {
    /// Substrate setup failed (classified `discard_harness_bug` in §8.7).
    SubstrateSetup(SubstrateError),
    /// Could not write the per-run JSON. The run still completed; the
    /// in-memory [`BenchRun`] is included so the caller can decide what
    /// to do.
    PersistFailed {
        /// Inner I/O error.
        io: std::io::Error,
        /// The run we were trying to persist (intact).
        run: Box<BenchRun>,
    },
    /// Encoding the run to JSON failed (shouldn't happen — all our
    /// structs are `Serialize`-clean).
    Encode(serde_json::Error),
}

impl std::fmt::Display for HarnessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SubstrateSetup(e) => write!(f, "harness: substrate setup failed: {e}"),
            Self::PersistFailed { io, .. } => write!(f, "harness: persist failed: {io}"),
            Self::Encode(e) => write!(f, "harness: encode failed: {e}"),
        }
    }
}

impl std::error::Error for HarnessError {}

/// The orchestrator. Holds a substrate and an agent backend; one
/// instance can drive many [`run`](Self::run) calls.
pub struct BenchHarness<S: Substrate, A: AgentBackend> {
    substrate: S,
    agent: A,
    agent_config: AgentConfig,
}

impl<S: Substrate, A: AgentBackend> BenchHarness<S, A> {
    /// Construct a harness around a substrate and agent backend.
    pub const fn new(substrate: S, agent: A, agent_config: AgentConfig) -> Self {
        Self {
            substrate,
            agent,
            agent_config,
        }
    }

    /// Drive one fresh-context agent run end-to-end.
    ///
    /// Returns the in-memory [`BenchRun`] in addition to writing it to
    /// the configured artifact dir (if any). Always returns a record
    /// even when the agent fails the task — agent failure is data, not
    /// an error (per §8.7's `counted_agent_failure` class).
    ///
    /// # Errors
    ///
    /// Returns [`HarnessError`] only for true harness-level bugs (cannot
    /// set up the substrate; cannot encode/persist JSON). Agent errors
    /// and substrate-incoherence errors land in the returned [`BenchRun`]
    /// via the verdict / OracleB summary.
    pub fn run(
        &mut self,
        plan: &ScenarioPlan,
        config: &BenchConfig,
    ) -> Result<BenchRun, HarnessError> {
        // -- 1. Substrate setup. --
        let scfg = SubstrateConfig {
            seed: plan.seed,
            base_git_time: maw_scenario::GIT_TIME_BASE_FOR_DRIVER,
            debug_dir: None,
        };
        let handle = self
            .substrate
            .setup(&scfg)
            .map_err(HarnessError::SubstrateSetup)?;

        // -- 2. Prompt rendering (deterministic given seed + crib). --
        let prompt = render_prompt(&PromptInputs {
            plan,
            convention_text: &handle.convention_text,
            workspace_root_absolute: handle.workspace_root.to_string_lossy().as_ref(),
        });
        let prompt_sha256 = prompt_sha256_hex(&prompt);

        // -- 3. Timestamps (pinned for determinism tests). --
        let start_ms = config.pinned_start_ms.unwrap_or_else(unix_ms_now);

        // -- 4. Agent run. --
        // bn-3hzt: if the caller supplied chaos_env, fold it into a
        // PER-RUN AgentConfig so the spawned agent subprocess
        // inherits MAW_FP=... (or any other operator-controlled env
        // vars). We clone the harness's AgentConfig and never mutate
        // the harness state itself — this keeps `BenchHarness` safe
        // to reuse across runs without per-call chaos leakage.
        let agent_result = if config.chaos_env.is_empty() {
            self.agent.run(&prompt, &self.agent_config, &handle)
        } else {
            let mut per_run = self.agent_config.clone();
            for (k, v) in &config.chaos_env {
                per_run.extra_env.insert(k.clone(), v.clone());
            }
            self.agent.run(&prompt, &per_run, &handle)
        };

        let end_ms = config
            .pinned_end_ms
            .unwrap_or_else(|| unix_ms_now().max(start_ms));

        // -- 5. Build transcript / per-run counters. --
        let (turns_field, total_turns, total_tool_calls, agent_done, agent_cost, agent_stop) =
            match agent_result {
                Ok(reply) => {
                    let total_turns = u32::try_from(reply.turns.len()).unwrap_or(u32::MAX);
                    let total_tool_calls: u32 = reply
                        .turns
                        .iter()
                        .map(|t| u32::try_from(t.tool_calls.len()).unwrap_or(u32::MAX))
                        .sum();
                    (
                        reply.clone().into_turns(),
                        total_turns,
                        total_tool_calls,
                        reply.done,
                        reply.cost_usd,
                        reply.stop_reason,
                    )
                }
                Err(e) => (
                    Vec::new(),
                    0,
                    0,
                    false,
                    None,
                    match e {
                        AgentError::Provider(m) => format!("provider_error: {m}"),
                        AgentError::Config(m) => format!("config_error: {m}"),
                        AgentError::Io(io) => format!("io_error: {io}"),
                    },
                ),
            };

        let transcript = Transcript {
            prompt: prompt.clone(),
            prompt_sha256: prompt_sha256.clone(),
            convention_text: handle.convention_text.clone(),
            turns: turns_field,
        };

        // -- 6. Oracle B (only for maw substrates / when caller opts in). --
        let oracle_b = run_oracle_b_if_enabled(&handle, config);

        // -- 7. Substrate ground-truth file enumeration. --
        let substrate_final_files = enumerate_workspace_files(&handle.workspace_root);

        // -- 8. Verdict classification. --
        let verdict = classify_verdict(agent_done, &oracle_b, &agent_stop);

        // -- 9. Manifest. --
        //
        // `build_manifest` captures maw/git/jj versions once per run
        // (bn-f5zu). Previously `maw_version` was left empty; the
        // 2026-05-26 SG3 NO-GO root cause
        // (`notes/sg3-no-go-rootcause.md`) traced to v0.61.0-vs-post-
        // T3.2 binary skew that would have been one grep away if the
        // field had been populated. Captures are ms-fast (three
        // `Command::output` calls) at the end of the agent subprocess.
        let manifest = build_manifest(
            plan,
            &self.agent_config,
            &prompt_sha256,
            handle.label,
            start_ms,
            end_ms,
            config,
        );

        // -- 10. Compose record. --
        let run_id = config
            .run_id
            .clone()
            .unwrap_or_else(|| derive_run_id(plan, handle.label, config, start_ms));

        let run = BenchRun {
            schema_version: BenchRun::SCHEMA_VERSION,
            run_id,
            manifest,
            verdict,
            oracle_b,
            transcript,
            total_tool_calls,
            total_turns,
            cost_usd: agent_cost,
            duration_ms: end_ms.saturating_sub(start_ms),
            substrate_final_files,
        };

        // -- 11. Substrate teardown (best-effort). --
        if let Err(e) = self.substrate.teardown(handle) {
            // Teardown failures do NOT invalidate the run. Log via stderr.
            eprintln!("harness: substrate teardown error (ignored): {e}");
        }

        // -- 12. Persist. --
        if let Some(dir) = &config.artifact_dir {
            persist_run(&run, dir)?;
        }

        Ok(run)
    }
}

/// Run Oracle B if the caller enabled it. Pure free function so we don't
/// pull `oracle_b` in when the user only ever runs against a non-maw
/// arm — and it makes the verdict classifier testable in isolation.
fn run_oracle_b_if_enabled(
    handle: &crate::substrate::SubstrateHandle,
    config: &BenchConfig,
) -> OracleBSummary {
    if !config.run_oracle_b {
        let reason = if config.oracle_b_skip_reason.is_empty() {
            format!(
                "arm = {}; Oracle B scoped to maw refs",
                handle.label.as_str()
            )
        } else {
            config.oracle_b_skip_reason.clone()
        };
        return OracleBSummary::NotApplicable { reason };
    }
    let violations = maw_assurance::oracle_b::check(&handle.repo_root);
    if violations.is_empty() {
        OracleBSummary::Green
    } else {
        OracleBSummary::Red {
            violations: violations.iter().map(|v| format!("{v}")).collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Wall-clock in Unix ms. Wrapped so tests can stub it via the
/// `pinned_*_ms` knobs.
fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Decide the harness-side verdict. Pure function — easier to unit-test
/// than the orchestrator.
fn classify_verdict(agent_done: bool, oracle_b: &OracleBSummary, agent_stop: &str) -> RunVerdict {
    if !agent_done {
        return RunVerdict::AgentFailed {
            reason: if agent_stop.is_empty() {
                "agent_stopped_without_done".to_string()
            } else {
                agent_stop.to_string()
            },
        };
    }
    match oracle_b {
        OracleBSummary::Red { .. } => RunVerdict::SubstrateIncoherent,
        OracleBSummary::Green | OracleBSummary::NotApplicable { .. } => RunVerdict::Success,
    }
}

fn build_manifest(
    plan: &ScenarioPlan,
    cfg: &AgentConfig,
    prompt_hash: &str,
    arm: SubstrateLabel,
    start_ms: u64,
    end_ms: u64,
    bcfg: &BenchConfig,
) -> RunManifest {
    // §6.4 fields for external binaries. Each carries either the
    // captured `--version` first line OR `"error: <msg>"` (bn-f5zu).
    // Empty-string remains reserved for "we deliberately didn't look"
    // — currently never produced by the harness, but tests / future
    // callers can construct that shape via [`RunManifest::default`].
    let versions = capture_versions();
    RunManifest {
        claude_code_version: String::new(),
        claude_model_id: cfg.model.clone(),
        claude_effective_model: String::new(),
        git_version: versions.git.manifest_string(),
        jj_version: versions.jj.manifest_string(),
        maw_version: versions.maw.manifest_string(),
        benchmark_harness_commit: env!("CARGO_PKG_VERSION").to_string(),
        scenario_generator_commit: env!("CARGO_PKG_VERSION").to_string(),
        prompt_hash: prompt_hash.to_string(),
        seed: plan.seed,
        condition_id: bcfg.condition_id.clone(),
        t_class: bcfg.t_class.clone(),
        arm: arm.as_str().to_string(),
        os_kernel: detect_os_kernel(),
        start_ts_unix_ms: start_ms,
        end_ts_unix_ms: end_ms,
    }
}

/// Best-effort `uname -srm` probe. Empty on non-unix.
fn detect_os_kernel() -> String {
    std::process::Command::new("uname")
        .args(["-srm"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn derive_run_id(
    plan: &ScenarioPlan,
    arm: SubstrateLabel,
    cfg: &BenchConfig,
    start_ms: u64,
) -> String {
    let cond = if cfg.condition_id.is_empty() {
        "C-"
    } else {
        cfg.condition_id.as_str()
    };
    let tc = if cfg.t_class.is_empty() {
        "T-"
    } else {
        cfg.t_class.as_str()
    };
    format!(
        "{}-{}-seed{}-{}-{}",
        arm.as_str(),
        cond,
        plan.seed,
        tc,
        start_ms
    )
}

/// Enumerate every file under the workspace root, recursively. Returns
/// paths relative to the root, sorted. The pre-registration §1.1 raw
/// event stream includes the substrate-final state; this is the
/// minimum-viable view of it.
fn enumerate_workspace_files(root: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    enumerate_inner(root, root, &mut out);
    out.sort();
    out
}

fn enumerate_inner(base: &std::path::Path, cur: &std::path::Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(cur) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(base) else {
            continue;
        };
        let s = rel.to_string_lossy().to_string();
        // Skip the .git internals (huge churn, not interesting per-run).
        if s.starts_with(".git") {
            continue;
        }
        if path.is_dir() {
            enumerate_inner(base, &path, out);
        } else {
            out.push(s);
        }
    }
}

/// Write the run JSON atomically (write to `tmp` + rename).
fn persist_run(run: &BenchRun, dir: &std::path::Path) -> Result<(), HarnessError> {
    fs::create_dir_all(dir).map_err(|io| HarnessError::PersistFailed {
        io,
        run: Box::new(run.clone()),
    })?;
    let json = run.to_json().map_err(HarnessError::Encode)?;
    let final_path = dir.join(format!("{}.json", run.run_id));
    let tmp_path = dir.join(format!("{}.json.tmp", run.run_id));
    fs::write(&tmp_path, json).map_err(|io| HarnessError::PersistFailed {
        io,
        run: Box::new(run.clone()),
    })?;
    fs::rename(&tmp_path, &final_path).map_err(|io| HarnessError::PersistFailed {
        io,
        run: Box::new(run.clone()),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{MockAgent, MockScript, MockTurnScript};
    use crate::run::{OracleBSummary, RunVerdict, ToolCall};
    use crate::substrate::NoopSubstrate;
    use maw_scenario::ConditionProfile;

    fn small_plan() -> maw_scenario::ScenarioPlan {
        let profile = ConditionProfile::new(1, 0.0, 0.0, 0.0);
        // Generate 3 steps for our smallest end-to-end test.
        maw_scenario::generate_plan(123, &profile, 3)
    }

    fn three_turn_script_done() -> MockScript {
        MockScript {
            turns: vec![
                MockTurnScript {
                    reply_text: "Reading the convention crib.".to_string(),
                    tool_calls: vec![ToolCall {
                        name: "Read".to_string(),
                        args_json: r#"{"path":"AGENTS.md"}"#.to_string(),
                        ts_unix_ms: 0,
                        result_truncated: Some("(crib text)".to_string()),
                        attributed_op: None,
                        attributed_outcome: None,
                    }],
                },
                MockTurnScript {
                    reply_text: "Creating workspaces.".to_string(),
                    tool_calls: vec![ToolCall {
                        name: "Bash".to_string(),
                        args_json: r#"{"cmd":"maw ws create alice"}"#.to_string(),
                        ts_unix_ms: 0,
                        result_truncated: Some("OK".to_string()),
                        attributed_op: None,
                        attributed_outcome: None,
                    }],
                },
                MockTurnScript {
                    reply_text: "done".to_string(),
                    tool_calls: vec![],
                },
            ],
            done: true,
            stop_reason: String::new(),
            cost_usd: None,
        }
    }

    fn deterministic_bench_config() -> BenchConfig {
        BenchConfig {
            artifact_dir: None,
            run_id: Some("test-run-1".to_string()),
            condition_id: "C0".to_string(),
            t_class: "T0".to_string(),
            run_oracle_b: false,
            oracle_b_skip_reason: "test: noop substrate".to_string(),
            pinned_start_ms: Some(1000),
            pinned_end_ms: Some(2000),
            chaos_env: std::collections::BTreeMap::new(),
        }
    }

    /// AC: a small end-to-end fake run (NoopSubstrate + MockAgent +
    /// 3-step plan) produces a well-formed BenchRun JSON.
    #[test]
    fn end_to_end_mock_run_produces_well_formed_json() {
        let plan = small_plan();
        let agent = MockAgent::with_pinned_clock(three_turn_script_done(), 1234);
        let substrate = NoopSubstrate::new();
        let mut h = BenchHarness::new(substrate, agent, AgentConfig::default());
        let bcfg = deterministic_bench_config();
        let run = h.run(&plan, &bcfg).expect("harness run");

        assert_eq!(run.schema_version, BenchRun::SCHEMA_VERSION);
        assert_eq!(run.run_id, "test-run-1");
        assert_eq!(run.total_turns, 3);
        assert_eq!(run.total_tool_calls, 2);
        assert!(matches!(run.verdict, RunVerdict::Success));
        assert!(matches!(run.oracle_b, OracleBSummary::NotApplicable { .. }));
        assert_eq!(run.manifest.seed, 123);
        assert_eq!(run.manifest.arm, "noop");
        assert_eq!(run.manifest.condition_id, "C0");
        assert_eq!(run.manifest.t_class, "T0");
        assert_eq!(run.manifest.start_ts_unix_ms, 1000);
        assert_eq!(run.manifest.end_ts_unix_ms, 2000);
        assert_eq!(run.duration_ms, 1000);
        // Transcript carries the deterministic prompt + hash.
        assert!(!run.transcript.prompt.is_empty());
        assert_eq!(run.transcript.prompt_sha256.len(), 64);
        assert_eq!(run.transcript.turns.len(), 3);
        // Each Turn carries the agent's tool calls.
        assert_eq!(run.transcript.turns[0].tool_calls.len(), 1);
        assert_eq!(run.transcript.turns[1].tool_calls.len(), 1);
        assert_eq!(run.transcript.turns[2].tool_calls.len(), 0);
        // JSON round-trips cleanly.
        let json = run.to_json().expect("to_json");
        let decoded: BenchRun = serde_json::from_str(&json).expect("decode");
        assert_eq!(decoded.run_id, run.run_id);
        assert_eq!(decoded.total_turns, run.total_turns);
    }

    /// AC (bn-f5zu): the manifest's external-binary version fields are
    /// populated per-run — never empty unless capture errored. Asserts
    /// the bn-2ert "field exists but is empty" regression cannot
    /// silently recur.
    #[test]
    fn manifest_populates_maw_git_jj_versions() {
        let plan = small_plan();
        let agent = MockAgent::with_pinned_clock(MockScript::finished_in_one("done"), 0);
        let substrate = NoopSubstrate::new();
        let mut h = BenchHarness::new(substrate, agent, AgentConfig::default());
        let bcfg = deterministic_bench_config();
        let run = h.run(&plan, &bcfg).expect("harness run");

        // Each of the three external-binary fields is either populated
        // with the captured version string OR carries the literal
        // `error: ...` prefix when the binary is missing on $PATH.
        // Empty-string is the explicit regression bn-f5zu prevents.
        for (name, val) in [
            ("maw_version", &run.manifest.maw_version),
            ("git_version", &run.manifest.git_version),
            ("jj_version", &run.manifest.jj_version),
        ] {
            assert!(
                !val.is_empty(),
                "{name} must be non-empty (populated or `error: ...`); got empty"
            );
        }
        // Round-trip through JSON preserves the populated fields.
        let json = run.to_json().expect("to_json");
        let decoded: BenchRun = serde_json::from_str(&json).expect("decode");
        assert_eq!(decoded.manifest.maw_version, run.manifest.maw_version);
        assert_eq!(decoded.manifest.git_version, run.manifest.git_version);
        assert_eq!(decoded.manifest.jj_version, run.manifest.jj_version);
    }

    /// AC: a synthetic BenchRun JSON with a populated maw_version
    /// round-trips cleanly. This is the §6.4 manifest-schema test
    /// called out in the bn-f5zu acceptance criteria (4).
    #[test]
    fn populated_maw_version_round_trips() {
        let json = serde_json::json!({
            "schema_version": 2,
            "run_id": "round-trip-1",
            "manifest": {
                "claude_code_version": "claude-code/2.1.150",
                "claude_model_id": "sonnet",
                "claude_effective_model": "claude-sonnet-4-7",
                "git_version": "git version 2.45.0",
                "jj_version": "jj 0.21.0",
                "maw_version": "maw 0.61.0",
                "benchmark_harness_commit": "0.61.0",
                "scenario_generator_commit": "0.61.0",
                "prompt_hash": "deadbeef",
                "seed": 7,
                "condition_id": "C2",
                "t_class": "T0",
                "arm": "maw@new-layout",
                "os_kernel": "Linux 7.0 x86_64",
                "start_ts_unix_ms": 100,
                "end_ts_unix_ms": 200,
            },
            "verdict": {"outcome": "success"},
            "oracle_b": {"verdict": "green"},
            "transcript": {
                "prompt": "p",
                "prompt_sha256": "h",
                "convention_text": "c",
                "turns": [],
            },
            "total_tool_calls": 0,
            "total_turns": 0,
            "cost_usd": null,
            "duration_ms": 100,
            "substrate_final_files": [],
        });
        let run: BenchRun = serde_json::from_value(json).expect("decode");
        assert_eq!(run.manifest.maw_version, "maw 0.61.0");
        assert_eq!(run.manifest.git_version, "git version 2.45.0");
        assert_eq!(run.manifest.jj_version, "jj 0.21.0");
        // Re-serialize and re-decode — the populated maw_version must
        // round-trip identically (no field-name typo, no skip_serializing).
        let s = run.to_json().expect("to_json");
        let decoded: BenchRun = serde_json::from_str(&s).expect("decode2");
        assert_eq!(decoded.manifest.maw_version, "maw 0.61.0");
    }

    /// AC: same seed + same MockScript + pinned clock + pinned timestamps
    /// ⇒ byte-identical BenchRun JSON. The harness's own determinism
    /// contract — the SP3-variance framing applies only at the LLM
    /// edge, not at our edge.
    #[test]
    fn same_seed_same_script_yields_byte_identical_json() {
        let plan = small_plan();
        let script = three_turn_script_done();
        let bcfg = deterministic_bench_config();

        let agent1 = MockAgent::with_pinned_clock(script.clone(), 5000);
        let substrate1 = NoopSubstrate::new();
        let mut h1 = BenchHarness::new(substrate1, agent1, AgentConfig::default());
        let run1 = h1.run(&plan, &bcfg).expect("run1");

        let agent2 = MockAgent::with_pinned_clock(script, 5000);
        let substrate2 = NoopSubstrate::new();
        let mut h2 = BenchHarness::new(substrate2, agent2, AgentConfig::default());
        let run2 = h2.run(&plan, &bcfg).expect("run2");

        // Tempdir paths inside `substrate_final_files` differ across
        // runs, but for NoopSubstrate the dir is empty so the slice is
        // trivially identical. The manifest's start/end_ts and prompt
        // depend on pinned values + the workspace root path — the
        // workspace root differs per run (tempdir), so the prompt does
        // too. We assert byte-identity AFTER scrubbing the workspace
        // root from both prompt and substrate_final_files (those are
        // the only environment-dependent fields by design).
        let mut a = run1;
        let mut b = run2;
        scrub_env_dependent_fields(&mut a);
        scrub_env_dependent_fields(&mut b);
        let ja = a.to_canonical_json().expect("ja");
        let jb = b.to_canonical_json().expect("jb");
        assert_eq!(ja, jb, "harness JSON not deterministic after env-scrub");
    }

    fn scrub_env_dependent_fields(run: &mut BenchRun) {
        // Two fields depend on the substrate's per-run tempdir:
        // - transcript.prompt (mentions the workspace path)
        // - prompt hash
        // - manifest.prompt_hash
        // We do NOT zero those — instead we set them to a fixed sentinel
        // so a hash mismatch in OTHER fields is still detectable.
        run.transcript.prompt = "<scrubbed>".to_string();
        run.transcript.prompt_sha256 = "<scrubbed>".to_string();
        run.manifest.prompt_hash = "<scrubbed>".to_string();
        // substrate_final_files contains nothing for NoopSubstrate but
        // be safe.
        run.substrate_final_files.clear();
        // git/jj/maw/uname output varies by host — scrub. (maw_version
        // added bn-f5zu; previously empty.)
        run.manifest.git_version.clear();
        run.manifest.jj_version.clear();
        run.manifest.maw_version.clear();
        run.manifest.os_kernel.clear();
    }

    /// AC: classify_verdict ranks AgentFailed > SubstrateIncoherent > Success.
    #[test]
    fn verdict_classifier_priority() {
        // agent_done=false ⇒ AgentFailed regardless of oracle.
        let v = classify_verdict(false, &OracleBSummary::Green, "max_turns");
        assert!(matches!(v, RunVerdict::AgentFailed { .. }));
        // agent_done=true + Oracle B red ⇒ SubstrateIncoherent.
        let v = classify_verdict(
            true,
            &OracleBSummary::Red {
                violations: vec!["x".into()],
            },
            "",
        );
        assert!(matches!(v, RunVerdict::SubstrateIncoherent));
        // agent_done=true + Oracle B green ⇒ Success.
        let v = classify_verdict(true, &OracleBSummary::Green, "");
        assert!(matches!(v, RunVerdict::Success));
        // agent_done=true + Oracle B N/A ⇒ Success.
        let v = classify_verdict(
            true,
            &OracleBSummary::NotApplicable {
                reason: "noop".into(),
            },
            "",
        );
        assert!(matches!(v, RunVerdict::Success));
    }

    /// AC: a planted Oracle-B defect trips OracleBSummary::Red.
    /// We construct a substrate whose `setup` produces a maw-shaped repo
    /// with a deliberately dangling `refs/manifold/head/<ws>` and assert
    /// the harness records `oracle_b: Red`.
    #[test]
    fn planted_oracle_b_defect_trips_red_verdict() {
        let plan = small_plan();
        let agent = MockAgent::with_pinned_clock(MockScript::finished_in_one("done"), 0);
        let substrate = oracle_b_planted::PlantedDanglingHeadSubstrate::new();
        let mut h = BenchHarness::new(substrate, agent, AgentConfig::default());
        let bcfg = BenchConfig {
            run_oracle_b: true, // <- run it
            ..deterministic_bench_config()
        };
        let run = h.run(&plan, &bcfg).expect("harness run");
        match &run.oracle_b {
            OracleBSummary::Red { violations } => {
                assert!(!violations.is_empty(), "expected at least one violation");
                let joined = violations.join("\n");
                assert!(
                    joined.contains("DanglingHeadRef") || joined.contains("head"),
                    "expected a dangling-head violation; got: {joined}"
                );
            }
            other => panic!("expected Red, got {other:?}"),
        }
        assert!(matches!(run.verdict, RunVerdict::SubstrateIncoherent));
    }

    /// AC: persistence is atomic (write-then-rename) and per-run JSON is
    /// loadable from disk.
    #[test]
    fn run_is_persisted_atomically_to_artifact_dir() {
        let tmp = tempfile::TempDir::new().expect("td");
        let plan = small_plan();
        let agent = MockAgent::with_pinned_clock(MockScript::finished_in_one("done"), 0);
        let substrate = NoopSubstrate::new();
        let mut h = BenchHarness::new(substrate, agent, AgentConfig::default());
        let bcfg = BenchConfig {
            artifact_dir: Some(tmp.path().to_path_buf()),
            run_id: Some("persist-test-1".to_string()),
            pinned_start_ms: Some(100),
            pinned_end_ms: Some(101),
            ..deterministic_bench_config()
        };
        let run = h.run(&plan, &bcfg).expect("harness run");
        let path = tmp.path().join("persist-test-1.json");
        assert!(path.exists(), "expected per-run JSON at {path:?}");
        // No leftover .tmp file.
        assert!(!tmp.path().join("persist-test-1.json.tmp").exists());
        let bytes = std::fs::read_to_string(&path).expect("read");
        let decoded: BenchRun = serde_json::from_str(&bytes).expect("decode");
        assert_eq!(decoded.run_id, run.run_id);
    }

    // ---------------------------------------------------------------
    // Helper substrate that plants an Oracle-B-violating defect.
    // ---------------------------------------------------------------
    mod oracle_b_planted {
        use std::process::Command;

        use crate::substrate::{
            Substrate, SubstrateConfig, SubstrateError, SubstrateHandle, SubstrateLabel,
        };

        /// A substrate that sets up a maw-shaped repo and plants a
        /// dangling `refs/manifold/head/<ws>` so Oracle B B1 must fire.
        pub struct PlantedDanglingHeadSubstrate {
            keep: Vec<tempfile::TempDir>,
        }

        impl PlantedDanglingHeadSubstrate {
            pub const fn new() -> Self {
                Self { keep: Vec::new() }
            }
        }

        impl Substrate for PlantedDanglingHeadSubstrate {
            fn label(&self) -> SubstrateLabel {
                // Pretend to be the maw arm so the harness opts to run
                // Oracle B at the caller's request.
                SubstrateLabel::Maw
            }

            fn setup(
                &mut self,
                _config: &SubstrateConfig,
            ) -> Result<SubstrateHandle, SubstrateError> {
                let td = tempfile::TempDir::new()?;
                let root = td.path().to_path_buf();
                init_repo(&root)?;
                // Plant: write `refs/manifold/head/orphan` pointing at HEAD
                // WITHOUT creating `ws/orphan/` or a merge-state file.
                let head = git_capture(&root, &["rev-parse", "HEAD"])
                    .map_err(|e| SubstrateError::Setup(format!("rev-parse: {e}")))?;
                run_git(&root, &["update-ref", "refs/manifold/head/orphan", &head])
                    .map_err(|e| SubstrateError::Setup(format!("plant: {e}")))?;
                self.keep.push(td);
                Ok(SubstrateHandle {
                    label: SubstrateLabel::Maw,
                    workspace_root: root.clone(),
                    repo_root: root,
                    convention_text: "# planted maw substrate (test)\n".to_string(),
                })
            }

            fn teardown(&mut self, handle: SubstrateHandle) -> Result<(), SubstrateError> {
                if let Some(pos) = self.keep.iter().position(|d| d.path() == handle.repo_root) {
                    let _ = self.keep.remove(pos);
                }
                Ok(())
            }
        }

        fn init_repo(root: &std::path::Path) -> std::io::Result<()> {
            run_git(root, &["init", "-q", "-b", "main"])?;
            run_git(root, &["config", "user.name", "test"])?;
            run_git(root, &["config", "user.email", "test@test"])?;
            run_git(root, &["config", "commit.gpgsign", "false"])?;
            std::fs::write(root.join("README.md"), "test\n")?;
            run_git(root, &["add", "README.md"])?;
            run_git(root, &["commit", "-q", "--no-gpg-sign", "-m", "init"])?;
            Ok(())
        }

        fn run_git(root: &std::path::Path, args: &[&str]) -> std::io::Result<()> {
            let s = Command::new("git").current_dir(root).args(args).status()?;
            if !s.success() {
                return Err(std::io::Error::other(format!("git {args:?} -> {s}")));
            }
            Ok(())
        }

        fn git_capture(root: &std::path::Path, args: &[&str]) -> std::io::Result<String> {
            let o = Command::new("git").current_dir(root).args(args).output()?;
            if !o.status.success() {
                return Err(std::io::Error::other(format!(
                    "git {args:?} -> {} stderr={}",
                    o.status,
                    String::from_utf8_lossy(&o.stderr)
                )));
            }
            Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
        }
    }
}
