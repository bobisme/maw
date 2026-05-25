//! Anthropic API backend — real LLM calls.
//!
//! **GATED BEHIND `claude-backend`.** A vanilla `cargo check --features
//! bench` does NOT pull `reqwest`'s TLS deps and does NOT require an
//! API key. Real benchmark sweeps opt in by enabling this feature.
//!
//! # Important: no automatic test runs
//!
//! Tests in this module are `#[ignore]`-gated and require
//! `ANTHROPIC_API_KEY` in env. The pre-registration freeze (T2.7) is
//! explicit: "DO NOT actually run the harness against real LLMs in this
//! bone — that costs money and is the EXECUTION phase."
//!
//! # Wire shape (sketch)
//!
//! We do not implement Claude Code's full multi-turn tool-use loop in
//! this skeleton. T2.3 + T2.6 layer on top of this with the canonical
//! `claude -p --output-format json` invocation (so we inherit the
//! envelope shape SP3 measured) and a stable subprocess wrapper. This
//! module provides the *contract*: an [`AgentBackend`] impl that:
//!
//! - takes [`crate::AgentConfig`] (model, max_turns, max_budget_usd,
//!   temperature, permission_mode) verbatim;
//! - drives the Anthropic `messages` API with the deterministic prompt;
//! - returns an [`crate::AgentReply`] with the per-turn shape the
//!   harness records.
//!
//! The skeleton compiles and gives T2.6 a place to drop the real
//! invocation. The contract is what T2.6 wires to.

use crate::agent::{AgentBackend, AgentConfig, AgentError, AgentReply, AgentTurn};
use crate::run::ToolCall;
use crate::substrate::SubstrateHandle;

/// Real-LLM backend. Holds a blocking `reqwest::Client` and an API key.
///
/// **Skeleton only at T2.2.** T2.6 wires this to the canonical
/// `claude -p --output-format json` subprocess invocation (SP3 §2). The
/// current `run` impl is a placeholder that returns a stub reply so
/// `cargo check --features bench,claude-backend` succeeds; it deliber-
/// ately fires an [`AgentError::Config`] if anyone tries to actually
/// run it without env opt-in (`MAW_BENCH_ALLOW_REAL_LLM=1`).
pub struct ClaudeBackend {
    /// Anthropic API key. Loaded from `ANTHROPIC_API_KEY` in env at
    /// construction.
    api_key: String,
    /// HTTP client, reused across runs.
    #[allow(dead_code)] // hooked up by T2.6
    client: reqwest::blocking::Client,
}

impl ClaudeBackend {
    /// Construct a `ClaudeBackend` reading `ANTHROPIC_API_KEY` from env.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Config`] if `ANTHROPIC_API_KEY` is unset.
    pub fn from_env() -> Result<Self, AgentError> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| AgentError::Config("ANTHROPIC_API_KEY not set".to_string()))?;
        let client = reqwest::blocking::Client::builder()
            .build()
            .map_err(|e| AgentError::Config(format!("reqwest: {e}")))?;
        Ok(Self { api_key, client })
    }
}

impl AgentBackend for ClaudeBackend {
    fn run(
        &mut self,
        _prompt: &str,
        _config: &AgentConfig,
        _handle: &SubstrateHandle,
    ) -> Result<AgentReply, AgentError> {
        // Defence-in-depth: even with the feature enabled, refuse to
        // make a real LLM call unless the operator has explicitly opted
        // in by setting an env var. This protects against the case where
        // a developer cargo-tests with `--features claude-backend` set
        // accidentally and burns through tokens.
        if std::env::var("MAW_BENCH_ALLOW_REAL_LLM").as_deref() != Ok("1") {
            return Err(AgentError::Config(
                "real LLM dispatch is opt-in: set MAW_BENCH_ALLOW_REAL_LLM=1 \
                 (this is a deliberate guard — T2.2 skeleton; T2.6 wires the \
                 canonical `claude -p` invocation)"
                    .to_string(),
            ));
        }
        // Skeleton: T2.6 replaces this with the real subprocess /
        // messages-API call. Returning a stub reply that signals the
        // skeleton state explicitly.
        let _ = &self.api_key; // suppress dead-code warning
        Ok(AgentReply {
            turns: vec![AgentTurn {
                index: 1,
                ts_unix_ms: 0,
                reply_text: "<ClaudeBackend skeleton: T2.6 wires the real call>".to_string(),
                tool_calls: vec![ToolCall {
                    name: "(skeleton)".to_string(),
                    args_json: String::new(),
                    ts_unix_ms: 0,
                    result_truncated: None,
                }],
            }],
            done: false,
            cost_usd: None,
            stop_reason: "skeleton".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construction without API key fails predictably.
    #[test]
    fn from_env_without_key_returns_config_error() {
        // SAFETY: only this test touches the env; we restore after.
        let prev = std::env::var("ANTHROPIC_API_KEY").ok();
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
        let r = ClaudeBackend::from_env();
        match r {
            Err(AgentError::Config(m)) => assert!(m.contains("ANTHROPIC_API_KEY")),
            other => panic!("expected Config error; got {other:?}"),
        }
        if let Some(p) = prev {
            unsafe {
                std::env::set_var("ANTHROPIC_API_KEY", p);
            }
        }
    }

    /// Even with the feature enabled, refuses to make a real call
    /// unless `MAW_BENCH_ALLOW_REAL_LLM=1` (cost guard).
    ///
    /// `#[ignore]`-gated because it requires an `ANTHROPIC_API_KEY`
    /// to construct the backend. Run manually:
    /// `MAW_BENCH_ALLOW_REAL_LLM=0 ANTHROPIC_API_KEY=... cargo test \
    ///  -p maw-bench --features bench,claude-backend -- --ignored`.
    #[test]
    #[ignore]
    fn run_refuses_without_explicit_opt_in() {
        let mut b = ClaudeBackend::from_env().expect("api key present");
        let agent_cfg = AgentConfig::default();
        let mut s = crate::substrate::NoopSubstrate::new();
        let h = s
            .setup(&crate::substrate::SubstrateConfig {
                seed: 1,
                base_git_time: 0,
                debug_dir: None,
            })
            .expect("noop");
        // SAFETY: only this test touches the env.
        unsafe {
            std::env::remove_var("MAW_BENCH_ALLOW_REAL_LLM");
        }
        let r = b.run("p", &agent_cfg, &h);
        match r {
            Err(AgentError::Config(m)) => assert!(m.contains("MAW_BENCH_ALLOW_REAL_LLM")),
            other => panic!("expected Config error; got {other:?}"),
        }
    }
}
