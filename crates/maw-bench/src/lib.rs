//! SG2 real-agent benchmark driver harness for maw (T2.2 / bn-1sqo).
//!
//! # What this crate is
//!
//! The **expensive driver** of the SG1/SG2 "build once, drive two ways"
//! architecture (`notes/sg1-dst-architecture.md` §2). Cheap driver:
//! [`maw_assurance::in_proc::InProcDriver`] (T1.6). Expensive driver: this
//! crate. Both consume the same [`maw_scenario::ScenarioPlan`] so the
//! seed-determinism guarantee is shared.
//!
//! # Hard contracts (frozen by `notes/sg2-benchmark-preregistration.md`)
//!
//! - **Reproducibility seam.** `seed → ScenarioPlan → prompts/transcripts`
//!   is bit-exact (T2.1 gives us that for the plan; this crate maps a
//!   plan deterministically to a prompt). The only nondeterminism allowed
//!   to leak in is real-LLM provider variance — bounded by SP3 (cost CV
//!   4.8%, turns CV 9.5%; bimodal wedge tail).
//! - **Pluggable substrate.** [`Substrate`] is a tiny trait T2.3
//!   (`bn-mit2`) implements three times: `maw`, `git-worktrees+convention`,
//!   `jj-workspaces` (plus the load-bearing arm 4
//!   `claude-native-worktrees`). The harness MUST NOT hard-code maw
//!   assumptions.
//! - **Pluggable agent backend.** [`AgentBackend`] gives us a [`MockAgent`]
//!   for in-repo tests (no network, no spend) and a [`ClaudeBackend`] for
//!   real sweeps (gated behind `claude-backend`).
//! - **Per-run output is the T2.4 raw event stream.** [`BenchRun`] carries
//!   turn count, tool-call list with timestamps, full transcript,
//!   substrate state at end, Oracle B verdict, and the planned-task-battery
//!   pass/fail. Serialized as JSON, one file per run, into a configurable
//!   artifact dir (mirrors the `FailureBundle` / `tests/corpus/dst` pattern).
//! - **No real-LLM dispatch from `cargo test`.** Every test in this crate
//!   uses [`MockAgent`]; real-agent sweeps are an explicit invocation
//!   (developer or CI choice), never a `cargo test` side-effect.
//!
//! # Why a separate crate
//!
//! `maw-bench` is a sibling of `maw-scenario` (not nested inside
//! `maw-assurance`) because the harness's cost model is different from
//! SG1's: SG1 wants `cargo test` cheap; SG2 wants opt-in real-LLM runs.
//! Co-locating would either force `claude-backend` into the assurance
//! feature graph or leave a dead feature in this crate. A sibling crate
//! gated behind `bench` keeps the default workspace build zero-overhead
//! and gives T2.3 a clean import path (`use maw_bench::Substrate;`).
//!
//! # Module layout
//!
//! - [`substrate`] — the [`Substrate`] trait + [`NoopSubstrate`] for tests.
//! - [`agent`] — the [`AgentBackend`] trait + [`MockAgent`]
//!   (and [`claude::ClaudeBackend`] under `claude-backend`).
//! - [`harness`] — the orchestrator that owns the substrate, drives the
//!   agent, captures the trace, and emits a [`BenchRun`].
//! - [`run`] — the [`BenchRun`] type, [`Transcript`], [`ToolCall`],
//!   [`Turn`] — the on-disk JSON schema T2.4 consumes.
//! - [`prompt`] — deterministic mapping of `ScenarioPlan` → the prompt
//!   the agent receives, so the same seed yields the same prompt bytes.

#![cfg(feature = "bench")]
#![deny(rust_2018_idioms)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::too_long_first_doc_paragraph)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::must_use_candidate)]

pub mod agent;
pub mod harness;
pub mod prompt;
pub mod run;
pub mod substrate;
pub mod version_capture;

#[cfg(feature = "claude-backend")]
pub mod claude;

pub use agent::{
    AgentBackend, AgentConfig, AgentReply, AgentTurn, MockAgent, MockScript, MockTurnScript,
};
pub use harness::{BenchConfig, BenchHarness, HarnessError};
pub use prompt::{PromptInputs, render_prompt};
pub use run::{
    BenchRun, OpClass, OracleBSummary, RunVerdict, StepOutcome, Substrate as SubstrateLabel,
    ToolCall, Transcript, Turn,
};
pub use substrate::{NoopSubstrate, Substrate, SubstrateError, SubstrateHandle};
pub use version_capture::{CapturedVersions, ToolVersion, capture_tool_version, capture_versions};
