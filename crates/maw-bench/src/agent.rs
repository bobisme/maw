//! [`AgentBackend`] — pluggable LLM driver.
//!
//! Two implementations live in this crate:
//!
//! - [`MockAgent`] — deterministic, scripted-response agent for in-repo
//!   tests. No network, no spend, byte-identical across runs.
//! - [`crate::claude::ClaudeBackend`] — real Anthropic API call, gated
//!   behind the `claude-backend` feature so `cargo check --features bench`
//!   does NOT require `reqwest`'s TLS deps and so a stray `cargo test`
//!   cannot accidentally spend money.
//!
//! # The contract
//!
//! An [`AgentBackend::run`] call:
//!
//! 1. Receives the prompt text and an [`AgentConfig`] (model id, max
//!    turns, temperature) and the substrate handle the agent is told to
//!    work in.
//! 2. Performs zero-or-more "turns" — each turn is one model reply, which
//!    may contain zero-or-more tool calls. The harness does NOT mediate
//!    tool calls (that is the substrate's job at observation time); the
//!    agent backend just reports what the model said it did.
//! 3. Returns an [`AgentReply`] with the full transcript and a final
//!    `done` signal — the harness then checks ground truth and renders
//!    the [`crate::BenchRun`].
//!
//! # Why mock-then-real
//!
//! The pre-registration freeze requires the harness be reproducibly
//! invokable independently of provider availability and budget. Coding
//! the harness against `AgentBackend` (not `claude` calls directly) is
//! what lets the in-repo tests prove the harness's own determinism
//! without LLM nondeterminism contaminating the test signal.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::run::{ToolCall, Turn};
use crate::substrate::SubstrateHandle;

/// Configuration the harness passes the agent backend per run. Mirrors
/// `claude -p --output-format json` flags from
/// `notes/agent-benchmark-feasibility.md` §2 so the §6.4 manifest can
/// echo it back verbatim.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Model identifier (e.g. `"sonnet"`, `"claude-sonnet-4-7"`). Pinned
    /// per §8.6 of the pre-registration; identical across every run and
    /// arm in a sweep.
    pub model: String,
    /// Hard cap on agent turns. §8.6: deterministic termination of a
    /// wedged run. SP3 used 40.
    pub max_turns: u32,
    /// Hard $ ceiling. §8.6: runaway-loop guard. SP3 used 2.00.
    pub max_budget_usd: f64,
    /// Sampling temperature. Recorded in the §6.4 manifest so a re-run
    /// is reproducible up to provider drift.
    pub temperature: f64,
    /// Permission mode (echoed for §6.4 manifest provenance). SP3 used
    /// `bypassPermissions` (non-interactive, no prompts).
    pub permission_mode: String,
    /// bn-3hzt: extra env vars to pass to the spawned agent
    /// subprocess. The load-bearing use is `MAW_FP=...` for the
    /// chaos overlay (translates to a deterministic failpoint
    /// crash inside the agent's next `maw` invocation, on a
    /// `--features failpoints` binary). The agent subprocess
    /// inherits the env and forwards it to anything it shells out
    /// to (Bash → maw), so a single env var is the seam.
    ///
    /// `BTreeMap` for stable JSON ordering. Empty by default —
    /// existing recipes that don't set anything serialize
    /// byte-identically to pre-bn-3hzt (the field is
    /// `skip_serializing_if = is_empty`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub extra_env: std::collections::BTreeMap<String, String>,
}

impl Default for AgentConfig {
    /// Defaults to SP3 §2's measured pins so re-deriving the SP3 numbers
    /// requires no overrides.
    fn default() -> Self {
        Self {
            model: "sonnet".to_string(),
            max_turns: 40,
            max_budget_usd: 2.00,
            temperature: 1.0,
            permission_mode: "bypassPermissions".to_string(),
            extra_env: std::collections::BTreeMap::new(),
        }
    }
}

/// One agent turn — what the model said and what tool calls it made.
///
/// `ts_unix_ms` is the harness's local timestamp when the turn was
/// received; it is NOT part of the determinism contract. It is recorded
/// because §1.1 of the pre-registration counts `tool_calls` with
/// timestamps as part of the raw event stream T2.4 consumes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentTurn {
    /// 1-based turn index. The §1.1 `num_turns` metric is `len(turns)`.
    pub index: u32,
    /// Local wall-clock when the harness received this turn. Diagnostic
    /// only; not load-bearing for any frozen metric.
    pub ts_unix_ms: u64,
    /// Model's natural-language reply for this turn.
    pub reply_text: String,
    /// Tool calls the model issued in this turn (zero or more).
    pub tool_calls: Vec<ToolCall>,
}

/// The aggregate per-run result the agent backend hands the harness.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentReply {
    /// Every turn the model produced (in order).
    pub turns: Vec<AgentTurn>,
    /// `true` iff the model emitted a "done" / "task complete" signal
    /// before hitting [`AgentConfig::max_turns`]. The harness uses this
    /// to disambiguate the `wedge_incident` derived flag from
    /// `agent_finished_cleanly` (§1.1).
    pub done: bool,
    /// Total cost reported by the provider envelope. Mirrors
    /// `total_cost_usd` from the `claude -p` JSON envelope (§1.1).
    /// `None` for [`MockAgent`].
    pub cost_usd: Option<f64>,
    /// If `done` is false and the agent stopped, this carries the reason
    /// (`max_turns`, `max_budget`, `provider_error`, ...). Surfaces in
    /// the §8.7 discard taxonomy when the harness classifies the run.
    pub stop_reason: String,
}

impl AgentReply {
    /// Convert the agent's per-turn list into the harness's [`Turn`]
    /// records. Kept on this type so backends can map their native
    /// turn shape into the JSON schema without a separate adapter.
    #[must_use]
    pub fn into_turns(self) -> Vec<Turn> {
        self.turns
            .into_iter()
            .map(|t| Turn {
                index: t.index,
                ts_unix_ms: t.ts_unix_ms,
                reply_text: t.reply_text,
                tool_calls: t.tool_calls,
            })
            .collect()
    }
}

/// Errors an agent backend can surface to the harness.
#[derive(Debug)]
pub enum AgentError {
    /// Backend hit a provider-side error (rate-limit, auth, 5xx). The
    /// harness classifies the run via §8.7's `discard_auth` /
    /// `discard_external_service_outage` taxonomy.
    Provider(String),
    /// Backend was misconfigured (missing model id, missing API key, ...).
    Config(String),
    /// Underlying I/O failure.
    Io(std::io::Error),
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Provider(m) => write!(f, "agent provider error: {m}"),
            Self::Config(m) => write!(f, "agent config error: {m}"),
            Self::Io(e) => write!(f, "agent I/O error: {e}"),
        }
    }
}

impl std::error::Error for AgentError {}

impl From<std::io::Error> for AgentError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// The trait every agent backend implements.
///
/// **Pure-function contract:** `run` MUST NOT mutate any state the
/// harness reads later (the substrate handle is `&` not `&mut`; ground
/// truth at end-of-run is what the harness reads).
pub trait AgentBackend {
    /// Run one fresh-context agent session.
    ///
    /// # Arguments
    ///
    /// - `prompt` — the deterministic prompt rendered from the
    ///   [`maw_scenario::ScenarioPlan`] by [`crate::render_prompt`].
    /// - `config` — pinned model/turn/budget knobs.
    /// - `handle` — substrate handle (workspace path, convention text).
    ///   The agent reads its workspace path from here; it does NOT mutate
    ///   the substrate through this borrow (substrate state is mutated by
    ///   the agent through tool calls, which the harness observes via the
    ///   on-disk ground truth read at end-of-run).
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] only if the backend itself failed (provider
    /// outage, config). An agent who fails the *task* still returns
    /// `Ok(AgentReply)` with `done: false`; the harness's oracle decides
    /// the verdict.
    fn run(
        &mut self,
        prompt: &str,
        config: &AgentConfig,
        handle: &SubstrateHandle,
    ) -> Result<AgentReply, AgentError>;
}

// ---------------------------------------------------------------------------
// MockAgent — deterministic scripted backend for in-repo tests
// ---------------------------------------------------------------------------

/// A scripted reply for one turn of a [`MockAgent`] run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MockTurnScript {
    /// Text the agent "says" this turn.
    pub reply_text: String,
    /// Tool calls the agent "makes" this turn (zero or more).
    pub tool_calls: Vec<ToolCall>,
}

/// The complete script for a [`MockAgent`] run.
///
/// The script is *byte-identical* across runs — that is what makes the
/// harness's determinism test deterministic. To simulate provider
/// nondeterminism in a test (rare; usually we want the deterministic
/// path), construct two different scripts and assert the harness's
/// JSON diverges only in the agent-controlled fields.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MockScript {
    /// Per-turn scripted replies, in order.
    pub turns: Vec<MockTurnScript>,
    /// Did the agent finish the task? Drives [`AgentReply::done`].
    pub done: bool,
    /// Optional stop reason if `done == false`.
    pub stop_reason: String,
    /// Optional cost (recorded in [`AgentReply::cost_usd`]). Most tests
    /// leave this `None` and assert on the harness's bookkeeping
    /// independently of the (non-existent) cost envelope.
    pub cost_usd: Option<f64>,
}

impl MockScript {
    /// Build a one-turn script that finishes cleanly with the given
    /// reply text. Useful for the harness's smallest end-to-end test.
    #[must_use]
    pub fn finished_in_one(reply_text: impl Into<String>) -> Self {
        Self {
            turns: vec![MockTurnScript {
                reply_text: reply_text.into(),
                tool_calls: Vec::new(),
            }],
            done: true,
            stop_reason: String::new(),
            cost_usd: None,
        }
    }
}

/// Deterministic scripted [`AgentBackend`] for in-repo tests. Replays a
/// [`MockScript`] verbatim, with stable timestamps so the harness's
/// determinism test can byte-compare two runs.
///
/// # Determinism
///
/// `ts_unix_ms` is normally clock-derived, which would break
/// byte-identity. [`MockAgent::with_pinned_clock`] pins the per-turn
/// timestamp to a function of the turn index — every test that wants
/// byte-identical JSON uses this constructor.
pub struct MockAgent {
    script: MockScript,
    /// If `Some(t0)`, turn `i` (1-based) gets `ts_unix_ms = t0 + i - 1`.
    /// Otherwise uses the real wall clock (still acceptable when the
    /// test does not byte-compare JSON across runs).
    pinned_clock: Option<u64>,
}

impl MockAgent {
    /// New `MockAgent` using the real wall clock for turn timestamps.
    #[must_use]
    pub const fn new(script: MockScript) -> Self {
        Self {
            script,
            pinned_clock: None,
        }
    }

    /// New `MockAgent` with deterministic timestamps. Required for the
    /// harness's byte-identity determinism test.
    #[must_use]
    pub const fn with_pinned_clock(script: MockScript, t0_unix_ms: u64) -> Self {
        Self {
            script,
            pinned_clock: Some(t0_unix_ms),
        }
    }
}

impl AgentBackend for MockAgent {
    fn run(
        &mut self,
        _prompt: &str,
        config: &AgentConfig,
        _handle: &SubstrateHandle,
    ) -> Result<AgentReply, AgentError> {
        // Respect the configured max_turns even with a scripted backend
        // (so a test that pins max_turns = 2 with a 5-turn script gets
        // the realistic truncation behaviour).
        let take = self.script.turns.len().min(config.max_turns as usize);
        let mut turns = Vec::with_capacity(take);
        for (i, scripted) in self.script.turns.iter().take(take).enumerate() {
            let idx = u32::try_from(i + 1).unwrap_or(u32::MAX);
            let ts = self.pinned_clock.map_or_else(
                || {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                },
                |t0| t0 + u64::from(idx) - 1,
            );
            turns.push(AgentTurn {
                index: idx,
                ts_unix_ms: ts,
                reply_text: scripted.reply_text.clone(),
                tool_calls: scripted.tool_calls.clone(),
            });
        }
        // If we ran out of script before max_turns AND the script said
        // done, we report done. If we ran out of turns due to max_turns,
        // we report not-done with stop_reason = max_turns.
        let truncated = take < self.script.turns.len();
        let done = self.script.done && !truncated;
        let stop_reason = if truncated {
            "max_turns".to_string()
        } else {
            self.script.stop_reason.clone()
        };
        Ok(AgentReply {
            turns,
            done,
            cost_usd: self.script.cost_usd,
            stop_reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::substrate::{NoopSubstrate, Substrate, SubstrateConfig, SubstrateLabel};

    fn handle() -> SubstrateHandle {
        let mut s = NoopSubstrate::new();
        s.setup(&SubstrateConfig {
            seed: 1,
            base_git_time: 0,
            debug_dir: None,
        })
        .expect("noop")
    }

    #[test]
    fn mock_agent_pinned_clock_is_deterministic() {
        let script = MockScript {
            turns: vec![
                MockTurnScript {
                    reply_text: "thinking".to_string(),
                    tool_calls: Vec::new(),
                },
                MockTurnScript {
                    reply_text: "done".to_string(),
                    tool_calls: Vec::new(),
                },
            ],
            done: true,
            stop_reason: String::new(),
            cost_usd: None,
        };
        let cfg = AgentConfig::default();
        let h = handle();
        let mut a = MockAgent::with_pinned_clock(script.clone(), 1000);
        let r1 = a.run("p", &cfg, &h).expect("ok");
        let mut a2 = MockAgent::with_pinned_clock(script, 1000);
        let r2 = a2.run("p", &cfg, &h).expect("ok");
        assert_eq!(r1.turns.len(), 2);
        assert_eq!(r1.turns[0].ts_unix_ms, 1000);
        assert_eq!(r1.turns[1].ts_unix_ms, 1001);
        assert_eq!(
            serde_json::to_string(&r1).expect("ser r1"),
            serde_json::to_string(&r2).expect("ser r2")
        );
    }

    #[test]
    fn mock_agent_truncates_at_max_turns() {
        let script = MockScript {
            turns: vec![
                MockTurnScript {
                    reply_text: "t1".to_string(),
                    tool_calls: vec![],
                },
                MockTurnScript {
                    reply_text: "t2".to_string(),
                    tool_calls: vec![],
                },
                MockTurnScript {
                    reply_text: "t3".to_string(),
                    tool_calls: vec![],
                },
            ],
            done: true,
            stop_reason: String::new(),
            cost_usd: None,
        };
        let cfg = AgentConfig {
            max_turns: 2,
            ..AgentConfig::default()
        };
        let mut a = MockAgent::with_pinned_clock(script, 0);
        let r = a.run("p", &cfg, &handle()).expect("ok");
        assert_eq!(r.turns.len(), 2);
        assert!(!r.done, "truncation overrides scripted done");
        assert_eq!(r.stop_reason, "max_turns");
    }

    #[test]
    fn mock_agent_finished_in_one_helper() {
        let h = handle();
        let cfg = AgentConfig::default();
        let mut a = MockAgent::with_pinned_clock(MockScript::finished_in_one("hi"), 42);
        let r = a.run("p", &cfg, &h).expect("ok");
        assert_eq!(r.turns.len(), 1);
        assert!(r.done);
        assert_eq!(r.turns[0].reply_text, "hi");
        // sanity: handle is the noop one
        assert_eq!(h.label, SubstrateLabel::Noop);
    }
}
