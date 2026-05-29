//! Shared `--real-llm` + `--substrate=` wiring for the sg2/sg3 eval
//! bins (`bn-1h4b`).
//!
//! Both `sg2-sweep-pilot` and `sg3-layout-eval` were originally pinned
//! to [`maw_bench::MockAgent`] + [`maw_bench::NoopSubstrate`]. `bn-3kxq`
//! shipped the real-LLM subprocess driver ([`maw_bench::claude::ClaudeBackend`])
//! and `bn-mit2` shipped real substrate adapters
//! ([`maw_bench_adapters::maw_adapter::MawAdapter`],
//! [`maw_bench_adapters::worktrees_adapter::WorktreesConventionAdapter`],
//! [`maw_bench_adapters::jj_adapter::JjAdapter`]) — but the two layers
//! were never connected. This module is the connection.
//!
//! # Design
//!
//! - [`BackendChoice`] picks the agent backend (`Mock` for the
//!   deterministic pilot, `Claude` for the real-LLM subprocess wire).
//!   `Mock` is the default for backward-compat — the existing
//!   `just sg{2,3}-*-pilot` recipes are byte-identical to before.
//! - [`SubstrateChoice`] picks the substrate arm (`Noop` for the
//!   pilot, `Maw` / `Worktrees` / `Jj` for real-substrate exercises).
//!   For maw, the layout flavor (v2 `ws/` vs proposed consolidated
//!   `.maw/workspaces/`) is selected via the same enum
//!   ([`SubstrateChoice::MawWsLayout`] vs
//!   [`SubstrateChoice::MawConsolidatedLayout`]) so the sg3-layout-eval
//!   bin's `--layout=old|new|both` maps onto two arms without an
//!   adapter fork.
//! - [`AnyAgent`] / [`RealSubstrate`] are concrete enum wrappers that
//!   implement [`maw_bench::agent::AgentBackend`] /
//!   [`maw_bench::substrate::Substrate`] so the sweep driver's generic
//!   factory signature stays unchanged.
//!
//! # Defence-in-depth gates (bn-3kxq, preserved)
//!
//! The `claude-backend` feature is a compile-time gate; the runtime
//! `MAW_BENCH_ALLOW_REAL_LLM=1` env var is checked inside
//! `ClaudeBackend::run` itself. **Both** must be active before a
//! real LLM call fires. The bin-side `--real-llm` flag is the third
//! layer: a misconfigured invocation (e.g. `--real-llm --substrate=noop`)
//! is rejected at arg-parse time with a loud error.

#![cfg(feature = "bench")]

use std::path::{Path, PathBuf};

use maw_bench::agent::{AgentBackend, AgentConfig, AgentError, AgentReply};
use maw_bench::substrate::{
    NoopSubstrate, Substrate, SubstrateConfig, SubstrateError, SubstrateHandle, SubstrateLabel,
};
use maw_bench::{MockAgent, MockScript};
// Aliased to distinguish from `maw_bench::substrate::Substrate` (the
// narrow harness trait we implement here). The adapter-side trait
// only contributes its `root()` accessor for our purposes.
use maw_bench_adapters::Substrate as AdapterSubstrate;

#[cfg(feature = "claude-backend")]
use maw_bench::claude::ClaudeBackend;

// ---------------------------------------------------------------------------
// Choice enums + parsing
// ---------------------------------------------------------------------------

/// Which agent backend to use for a run.
///
/// `Mock` keeps the deterministic pilot path (default). `Claude`
/// drives the real-LLM subprocess wire (`claude -p --output-format
/// stream-json`); requires the `claude-backend` feature compiled in
/// AND `MAW_BENCH_ALLOW_REAL_LLM=1` exported at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendChoice {
    /// The deterministic in-memory mock backend
    /// ([`maw_bench::MockAgent`]). No spend, no network.
    Mock,
    /// The real Claude Code subprocess backend
    /// ([`maw_bench::claude::ClaudeBackend`]). Bills the operator's
    /// configured CC auth (OAuth keychain / `ANTHROPIC_API_KEY`).
    Claude,
}

impl BackendChoice {
    /// Stable manifest label, used by tests + dispatch logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mock => "mock",
            Self::Claude => "claude",
        }
    }
}

/// Which substrate adapter to mount under the agent.
///
/// `Noop` is the default backward-compat path (tempdir, no git, no
/// substrate-native verbs to exercise). The other variants spin up
/// the real adapter for that arm.
///
/// `MawWsLayout` and `MawConsolidatedLayout` are the two flavors of
/// the maw arm; sg3-layout-eval's `--layout=old|new` maps onto these
/// two flavors (`old → MawWsLayout`, `new → MawConsolidatedLayout`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubstrateChoice {
    /// Tempdir + placeholder convention text. Used by MockAgent
    /// pilots; meaningless with `BackendChoice::Claude` (the real
    /// agent has no substrate-native verbs to exercise on a noop).
    Noop,
    /// Real maw arm using the current v2 `ws/` layout (the layout
    /// shipped in v0.61.0). For sg3-layout-eval this maps onto
    /// `--layout=old`.
    MawWsLayout,
    /// Real maw arm using the proposed consolidated `.maw/workspaces/`
    /// layout (bone bn-2kgu). For sg3-layout-eval this maps onto
    /// `--layout=new`.
    MawConsolidatedLayout,
    /// Hand-rolled `git worktree` + coordination convention arm
    /// ([`maw_bench_adapters::worktrees_adapter::WorktreesConventionAdapter`]).
    Worktrees,
    /// jj workspaces arm ([`maw_bench_adapters::jj_adapter::JjAdapter`]).
    Jj,
}

impl SubstrateChoice {
    /// Stable manifest label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Noop => "noop",
            Self::MawWsLayout => "maw",
            Self::MawConsolidatedLayout => "maw-consolidated",
            Self::Worktrees => "worktrees",
            Self::Jj => "jj",
        }
    }

    /// Parse a CLI value like `maw` / `worktrees` / `jj` / `noop`.
    /// `maw` is treated as `MawWsLayout` (the layout shipped in
    /// v0.61.0); callers wanting the consolidated layout pass
    /// `maw-consolidated` explicitly, OR (for sg3-layout-eval) use
    /// the `--layout=new` knob which selects it for them.
    ///
    /// # Errors
    ///
    /// Returns a human-readable string on unknown value.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "noop" => Ok(Self::Noop),
            "maw" => Ok(Self::MawWsLayout),
            "maw-consolidated" => Ok(Self::MawConsolidatedLayout),
            "worktrees" | "git-worktrees-bare" => Ok(Self::Worktrees),
            "jj" | "jj-workspaces" => Ok(Self::Jj),
            other => Err(format!(
                "unknown substrate: {other:?} (valid: noop|maw|maw-consolidated|worktrees|jj)"
            )),
        }
    }
}

/// Validate that the (backend, substrate) pair makes sense. The
/// invalid combinations are loud misconfigurations: e.g. `--real-llm
/// --substrate=noop` would burn real LLM spend driving an empty
/// tempdir with no substrate-native verbs to exercise; conversely
/// MockAgent on a real substrate would create a maw repo, then
/// emit a single canned reply and exit — a waste of substrate setup.
///
/// # Errors
///
/// Returns a human-readable string on misconfig.
pub fn validate_pairing(backend: BackendChoice, substrate: SubstrateChoice) -> Result<(), String> {
    match (backend, substrate) {
        (BackendChoice::Claude, SubstrateChoice::Noop) => {
            Err("misconfig: --real-llm with --substrate=noop. \
             ClaudeBackend on NoopSubstrate has nothing to exercise — the agent has no \
             substrate-native verbs and the run is wasted spend. \
             Pass --substrate=maw|maw-consolidated|worktrees|jj."
                .to_string())
        }
        (
            BackendChoice::Mock,
            SubstrateChoice::MawWsLayout
            | SubstrateChoice::MawConsolidatedLayout
            | SubstrateChoice::Worktrees
            | SubstrateChoice::Jj,
        ) => Err(
            "misconfig: MockAgent with a real substrate. MockAgent emits canned replies \
             and never invokes substrate-native verbs; spinning up a real maw/worktrees/jj \
             repo serves no purpose. Either pass --real-llm or drop --substrate=..."
                .to_string(),
        ),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// AnyAgent — enum wrapper implementing AgentBackend so the
// sweep driver's generic A: AgentBackend bound is satisfied.
// ---------------------------------------------------------------------------

/// Concrete agent backend chosen per run. Wraps [`MockAgent`] or
/// (gated) [`ClaudeBackend`] behind a single type so the sweep
/// driver's generic `A: AgentBackend` signature can be satisfied
/// without `Box<dyn>` ceremony.
pub enum AnyAgent {
    /// Deterministic scripted backend (zero spend, deterministic JSON).
    Mock(MockAgent),
    /// Real-LLM subprocess backend (billable). Only present when the
    /// `claude-backend` feature is compiled in.
    #[cfg(feature = "claude-backend")]
    Claude(ClaudeBackend),
}

impl AgentBackend for AnyAgent {
    fn run(
        &mut self,
        prompt: &str,
        config: &AgentConfig,
        handle: &SubstrateHandle,
    ) -> Result<AgentReply, AgentError> {
        match self {
            Self::Mock(m) => m.run(prompt, config, handle),
            #[cfg(feature = "claude-backend")]
            Self::Claude(c) => c.run(prompt, config, handle),
        }
    }
}

/// Build an [`AnyAgent`] from the per-run seed. For [`BackendChoice::Mock`]
/// the agent is a single-turn "done" script with a pinned clock so JSONs
/// stay byte-identical. For [`BackendChoice::Claude`] the agent is a fresh
/// [`ClaudeBackend`] (the seed is unused — real LLM dispatch isn't
/// reproducible from a seed).
///
/// # Errors
///
/// Returns an error string when `BackendChoice::Claude` is requested
/// but the `claude-backend` feature isn't compiled in.
pub fn make_any_agent(backend: BackendChoice, _seed: u64) -> Result<AnyAgent, String> {
    match backend {
        BackendChoice::Mock => Ok(AnyAgent::Mock(MockAgent::with_pinned_clock(
            MockScript::finished_in_one("done"),
            1_234,
        ))),
        BackendChoice::Claude => {
            #[cfg(feature = "claude-backend")]
            {
                Ok(AnyAgent::Claude(ClaudeBackend::new()))
            }
            #[cfg(not(feature = "claude-backend"))]
            {
                Err("--real-llm requested but the binary was not built with \
                     --features claude-backend. Rebuild with \
                     `cargo build -p maw-bench-sweep --features bench,claude-backend` \
                     (and remember to export MAW_BENCH_ALLOW_REAL_LLM=1 at runtime)."
                    .to_string())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// RealSubstrate — bridges the maw-bench-adapters Substrate (rich
// vocab: create_workspace/edit/commit/merge/destroy) onto the
// harness's maw_bench::substrate::Substrate (narrow setup/teardown
// surface). The harness only needs workspace_root + convention_text
// — the substrate's rich verbs are exercised by the agent at runtime
// via the per-arm crib.
// ---------------------------------------------------------------------------

/// Frozen per-arm convention cribs (§8.1 of `notes/sg2-benchmark-preregistration.md`).
///
/// Per the pre-registration these texts are **binding** and must be
/// equalized in length / detail across arms. Encoded as `&'static str`
/// constants so two runs with the same arm produce byte-identical
/// `convention_text` (a load-bearing input to `prompt_sha256`).
pub mod cribs {
    /// maw arm crib. Covers the `maw ws` verb surface listed in §8.1
    /// (`create / list / sync / diff / merge / recover` + the
    /// conflict-as-data resolution surface).
    pub const MAW_WS_LAYOUT_CRIB: &str = include_str!("cribs/maw_ws_layout.md");
    /// maw arm crib (proposed consolidated `.maw/` layout, bn-2kgu).
    /// Vocabulary identical to [`MAW_WS_LAYOUT_CRIB`]; the layout
    /// difference is what we're measuring, NOT a different verb set.
    pub const MAW_CONSOLIDATED_LAYOUT_CRIB: &str = include_str!("cribs/maw_consolidated_layout.md");
    /// Hand-rolled `git worktree` + coordination convention arm
    /// (§8.1). The convention text the agent reads to coordinate
    /// across worktrees lives in `notes/sg2-worktrees-convention.md`;
    /// this crib instructs the agent to read it.
    pub const WORKTREES_CONVENTION_CRIB: &str = include_str!("cribs/worktrees_convention.md");
    /// jj workspaces arm crib (§8.1). Covers `jj workspace add /
    /// list / update-stale`, divergence detection + resolution, op-log
    /// inspection + recovery, and when to AVOID op-integration.
    pub const JJ_WORKSPACES_CRIB: &str = include_str!("cribs/jj_workspaces.md");
}

/// Concrete real-substrate enum bridging the rich adapter trait
/// onto the harness's narrow `Substrate` trait.
///
/// Each variant lazily constructs its underlying adapter on
/// [`Substrate::setup`] (substrate setup can fail; we surface the
/// failure as [`SubstrateError::Setup`] so the harness's §8.7
/// classification routes it to `discard_harness_bug`).
pub enum RealSubstrate {
    /// Tempdir substrate — same as [`NoopSubstrate`]. Kept here so the
    /// sweep driver's substrate factory has a uniform return type.
    Noop(NoopSubstrate),
    /// Real maw substrate with the current v2 `ws/` layout.
    MawWsLayout(MawWsLayoutState),
    /// Real maw substrate with the proposed consolidated `.maw/`
    /// layout (simulation per `consolidated_layout_adapter.rs`).
    MawConsolidatedLayout(MawConsolidatedLayoutState),
    /// `git worktree` + coordination-convention substrate.
    Worktrees(WorktreesState),
    /// jj workspaces substrate.
    Jj(JjState),
}

/// Per-arm lazy state for the maw v2 `ws/` arm. The adapter is
/// constructed in `setup()` so each run gets a clean tempdir.
pub struct MawWsLayoutState {
    inner: Option<maw_bench_adapters::maw_adapter::MawAdapter>,
}

/// Per-arm lazy state for the maw consolidated-layout arm.
pub struct MawConsolidatedLayoutState {
    inner: Option<maw_bench_adapters::consolidated_layout_adapter::ConsolidatedLayoutAdapter>,
}

/// Per-arm lazy state for the worktrees+convention arm.
pub struct WorktreesState {
    inner: Option<maw_bench_adapters::worktrees_adapter::WorktreesConventionAdapter>,
}

/// Per-arm lazy state for the jj arm.
pub struct JjState {
    inner: Option<maw_bench_adapters::jj_adapter::JjAdapter>,
}

impl RealSubstrate {
    /// Construct an empty (un-setup) substrate of the chosen kind.
    /// Real init happens on [`Substrate::setup`].
    #[must_use]
    pub fn for_choice(choice: SubstrateChoice) -> Self {
        match choice {
            SubstrateChoice::Noop => Self::Noop(NoopSubstrate::new()),
            SubstrateChoice::MawWsLayout => Self::MawWsLayout(MawWsLayoutState { inner: None }),
            SubstrateChoice::MawConsolidatedLayout => {
                Self::MawConsolidatedLayout(MawConsolidatedLayoutState { inner: None })
            }
            SubstrateChoice::Worktrees => Self::Worktrees(WorktreesState { inner: None }),
            SubstrateChoice::Jj => Self::Jj(JjState { inner: None }),
        }
    }
}

/// Shape a `SubstrateError::Setup` from an adapter-side error.
fn setup_err<E: std::fmt::Display>(arm: &str, e: E) -> SubstrateError {
    SubstrateError::Setup(format!("{arm}: {e}"))
}

/// Build a `SubstrateHandle` from an adapter root + convention crib.
/// The adapter's `root()` is the natural cwd for the agent — for maw
/// it's the v2 root (so `maw ws create` works); for worktrees it's
/// the integration worktree; for jj it's the jj-init'd repo.
fn handle_at(label: SubstrateLabel, root: PathBuf, convention: &'static str) -> SubstrateHandle {
    SubstrateHandle {
        label,
        workspace_root: root.clone(),
        repo_root: root,
        convention_text: convention.to_string(),
        agent_extra_env: std::collections::BTreeMap::new(),
    }
}

/// bn-1q6z: as [`handle_at`] but with a substrate-supplied env
/// overlay merged into the spawned agent's `extra_env`. Load-bearing
/// use is the PATH-shim seam for worktrees/jj — the adapter
/// materialises a per-run shim dir and prepends it to PATH here so
/// the real-LLM agent's `git`/`jj` invocations are intercepted by
/// the shim. With chaos disabled the shim is inert (`exec`-thru).
fn handle_at_with_env(
    label: SubstrateLabel,
    root: PathBuf,
    convention: &'static str,
    extra_env: std::collections::BTreeMap<String, String>,
) -> SubstrateHandle {
    SubstrateHandle {
        label,
        workspace_root: root.clone(),
        repo_root: root,
        convention_text: convention.to_string(),
        agent_extra_env: extra_env,
    }
}

/// bn-1q6z: build the `agent_extra_env` overlay carrying the PATH
/// prepend for a per-run shim dir. Reads the caller's current `PATH`
/// at substrate-setup time (the agent inherits the harness's env
/// modulo `extra_env` overrides).
fn shim_path_overlay(shim_dir: &Path) -> std::collections::BTreeMap<String, String> {
    let orig_path = std::env::var("PATH").unwrap_or_default();
    let prepended = if orig_path.is_empty() {
        shim_dir.display().to_string()
    } else {
        format!("{}:{}", shim_dir.display(), orig_path)
    };
    let mut out = std::collections::BTreeMap::new();
    out.insert("PATH".to_string(), prepended);
    out.insert(
        maw_bench_adapters::shim::env_keys::SHIM_DIR.to_string(),
        shim_dir.display().to_string(),
    );
    out
}

impl Substrate for RealSubstrate {
    fn label(&self) -> SubstrateLabel {
        match self {
            Self::Noop(s) => s.label(),
            // The maw label is shared for both flavors. The grid-side
            // logical arm name (`maw@old-layout` / `maw@new-layout`)
            // is what the driver writes into `BenchRun.manifest.arm`
            // (see SweepDriver::drive at the `run.manifest.arm.clone_from`
            // line); the substrate label below is the SP3-frozen
            // §6.4 substrate kind, not the layout variant.
            Self::MawWsLayout(_) | Self::MawConsolidatedLayout(_) => SubstrateLabel::Maw,
            Self::Worktrees(_) => SubstrateLabel::GitWorktreesBare,
            Self::Jj(_) => SubstrateLabel::JjWorkspaces,
        }
    }

    fn setup(&mut self, config: &SubstrateConfig) -> Result<SubstrateHandle, SubstrateError> {
        match self {
            Self::Noop(s) => s.setup(config),
            Self::MawWsLayout(state) => {
                let adapter = maw_bench_adapters::maw_adapter::MawAdapter::new()
                    .map_err(|e| setup_err("MawAdapter::new", e))?;
                let root = adapter.root().clone();
                state.inner = Some(adapter);
                Ok(handle_at(
                    SubstrateLabel::Maw,
                    root,
                    cribs::MAW_WS_LAYOUT_CRIB,
                ))
            }
            Self::MawConsolidatedLayout(state) => {
                let adapter =
                    maw_bench_adapters::consolidated_layout_adapter::ConsolidatedLayoutAdapter::new()
                        .map_err(|e| setup_err("ConsolidatedLayoutAdapter::new", e))?;
                let root = adapter.root().clone();
                state.inner = Some(adapter);
                Ok(handle_at(
                    SubstrateLabel::Maw,
                    root,
                    cribs::MAW_CONSOLIDATED_LAYOUT_CRIB,
                ))
            }
            Self::Worktrees(state) => {
                let adapter =
                    maw_bench_adapters::worktrees_adapter::WorktreesConventionAdapter::new()
                        .map_err(|e| setup_err("WorktreesConventionAdapter::new", e))?;
                let root = adapter.root().clone();
                // bn-1q6z: prepend the adapter's per-substrate shim
                // dir to the spawned agent's PATH so its real-LLM
                // `git` invocations are intercepted by the shim
                // (inert when MAW_BENCH_CHAOS_KILL_PROB unset).
                let extra_env = shim_path_overlay(adapter.shim().dir());
                state.inner = Some(adapter);
                Ok(handle_at_with_env(
                    SubstrateLabel::GitWorktreesBare,
                    root,
                    cribs::WORKTREES_CONVENTION_CRIB,
                    extra_env,
                ))
            }
            Self::Jj(state) => {
                let adapter = maw_bench_adapters::jj_adapter::JjAdapter::new()
                    .map_err(|e| setup_err("JjAdapter::new", e))?;
                let root = adapter.root().clone();
                // bn-1q6z: same PATH-shim overlay as worktrees. The
                // shim intercepts both `git` and `jj` (the jj arm is
                // colocated, so the agent may run either).
                let extra_env = shim_path_overlay(adapter.shim().dir());
                state.inner = Some(adapter);
                Ok(handle_at_with_env(
                    SubstrateLabel::JjWorkspaces,
                    root,
                    cribs::JJ_WORKSPACES_CRIB,
                    extra_env,
                ))
            }
        }
    }

    fn teardown(&mut self, handle: SubstrateHandle) -> Result<(), SubstrateError> {
        match self {
            Self::Noop(s) => s.teardown(handle),
            // The adapter owns a TempDir guard; dropping it removes
            // the on-disk repo. Best-effort per the trait contract.
            Self::MawWsLayout(state) => {
                state.inner = None;
                Ok(())
            }
            Self::MawConsolidatedLayout(state) => {
                state.inner = None;
                Ok(())
            }
            Self::Worktrees(state) => {
                state.inner = None;
                Ok(())
            }
            Self::Jj(state) => {
                state.inner = None;
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substrate_parse_accepts_all_canonical_forms() {
        for (input, want) in [
            ("noop", SubstrateChoice::Noop),
            ("maw", SubstrateChoice::MawWsLayout),
            ("maw-consolidated", SubstrateChoice::MawConsolidatedLayout),
            ("worktrees", SubstrateChoice::Worktrees),
            ("git-worktrees-bare", SubstrateChoice::Worktrees),
            ("jj", SubstrateChoice::Jj),
            ("jj-workspaces", SubstrateChoice::Jj),
        ] {
            assert_eq!(
                SubstrateChoice::parse(input).unwrap(),
                want,
                "input={input}"
            );
        }
    }

    #[test]
    fn substrate_parse_rejects_unknown() {
        let e = SubstrateChoice::parse("hg").unwrap_err();
        assert!(e.contains("unknown substrate"), "msg: {e}");
    }

    #[test]
    fn validate_rejects_claude_with_noop() {
        let e = validate_pairing(BackendChoice::Claude, SubstrateChoice::Noop).unwrap_err();
        assert!(e.contains("misconfig"), "msg: {e}");
        assert!(e.contains("--real-llm"), "msg should name the flag: {e}");
    }

    #[test]
    fn validate_rejects_mock_with_real_substrate() {
        for s in [
            SubstrateChoice::MawWsLayout,
            SubstrateChoice::MawConsolidatedLayout,
            SubstrateChoice::Worktrees,
            SubstrateChoice::Jj,
        ] {
            let e = validate_pairing(BackendChoice::Mock, s).unwrap_err();
            assert!(e.contains("MockAgent"), "msg: {e}");
        }
    }

    #[test]
    fn validate_accepts_canonical_pairings() {
        validate_pairing(BackendChoice::Mock, SubstrateChoice::Noop).unwrap();
        validate_pairing(BackendChoice::Claude, SubstrateChoice::MawWsLayout).unwrap();
        validate_pairing(
            BackendChoice::Claude,
            SubstrateChoice::MawConsolidatedLayout,
        )
        .unwrap();
        validate_pairing(BackendChoice::Claude, SubstrateChoice::Worktrees).unwrap();
        validate_pairing(BackendChoice::Claude, SubstrateChoice::Jj).unwrap();
    }

    #[test]
    fn make_any_agent_mock_is_pinned() {
        // Two constructions should produce a deterministic backend
        // — assert via construction-doesn't-panic. (Byte-identity is
        // tested by the existing harness determinism test.)
        let a = make_any_agent(BackendChoice::Mock, 1).unwrap();
        let b = make_any_agent(BackendChoice::Mock, 2).unwrap();
        match (a, b) {
            (AnyAgent::Mock(_), AnyAgent::Mock(_)) => {}
            #[cfg(feature = "claude-backend")]
            _ => panic!("Mock should produce Mock variant"),
        }
    }

    #[test]
    fn cribs_are_nonempty() {
        // The §8.1 cribs are binding inputs to `prompt_sha256`;
        // accidentally shipping an empty file would silently change
        // every prompt hash. Smoke check that each is non-trivial.
        assert!(
            cribs::MAW_WS_LAYOUT_CRIB.len() > 100,
            "maw ws-layout crib too short"
        );
        assert!(
            cribs::MAW_CONSOLIDATED_LAYOUT_CRIB.len() > 100,
            "maw consolidated-layout crib too short"
        );
        assert!(
            cribs::WORKTREES_CONVENTION_CRIB.len() > 100,
            "worktrees-convention crib too short"
        );
        assert!(
            cribs::JJ_WORKSPACES_CRIB.len() > 100,
            "jj-workspaces crib too short"
        );
    }
}
