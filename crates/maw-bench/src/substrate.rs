//! [`Substrate`] — the pluggable arm interface T2.3 (`bn-mit2`) implements.
//!
//! T2.3 ships three production substrate adapters: `maw`,
//! `git-worktrees-bare` (the hand-rolled coordination convention), and
//! `jj-workspaces`. The §1.3 arm 4 (`claude-native-worktrees`) is a fourth.
//! The harness consumes `&mut dyn Substrate` so the SG2 driver does NOT
//! hard-code maw assumptions — the same harness runs all four arms.
//!
//! # Why a tiny trait
//!
//! The pre-registration (`notes/sg2-benchmark-preregistration.md` §1.3)
//! demands every arm receive an equivalent command crib and a clean
//! per-run setup/teardown. Anything richer (e.g. "create workspace named
//! X" / "merge workspaces a,b,c") would push substrate-specific verbs
//! into the trait and bias the benchmark. The agent is the one who must
//! discover the substrate's vocabulary from its crib — the trait only
//! gives the harness "set up clean state at this base ref", "tell me
//! where the agent should work and what convention text to read", "tear
//! down".
//!
//! # Determinism
//!
//! Implementors MUST pin every git write (commit dates) to
//! `maw_scenario::GIT_TIME_BASE_FOR_DRIVER` plus the per-step `git_time`
//! field (sg1-dst-architecture.md §5). The harness passes the plan's
//! base time through [`SubstrateConfig::base_git_time`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Stable label identifying which arm a substrate represents. Recorded
/// in the per-run JSON manifest (§6.4 of the pre-registration) so a
/// dataset is partitionable by arm without re-reading per-run config.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubstrateLabel {
    /// The maw arm — `maw ws` workflow.
    Maw,
    /// The plain `git worktree` + hand-rolled coordination convention arm.
    GitWorktreesBare,
    /// The `jj workspace` arm.
    JjWorkspaces,
    /// The Claude Code native worktree arm (`claude --worktree`).
    ClaudeNativeWorktrees,
    /// Used by the harness's own self-test ([`NoopSubstrate`]). Never
    /// appears in a published dataset.
    Noop,
}

impl SubstrateLabel {
    /// Stable string form used in §6.4 manifest entries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Maw => "maw",
            Self::GitWorktreesBare => "git-worktrees-bare",
            Self::JjWorkspaces => "jj-workspaces",
            Self::ClaudeNativeWorktrees => "claude-native-worktrees",
            Self::Noop => "noop",
        }
    }
}

/// Per-run configuration the harness hands the substrate at [`Substrate::setup`].
///
/// Kept narrow on purpose: a richer struct invites substrate-specific
/// knob proliferation that would bias the benchmark.
#[derive(Clone, Debug)]
pub struct SubstrateConfig {
    /// Stable seed (same value as `ScenarioPlan.seed`). Substrates that
    /// need to derive any internal randomness MUST use this seed so a
    /// re-run is reproducible.
    pub seed: u64,
    /// The base git_time the harness pins for the substrate's own init
    /// commits (workspace creation, README seed, etc.). Per-step times
    /// come from the plan steps; this is just for setup writes.
    pub base_git_time: i64,
    /// Optional artifact dir for substrate-private debug output (logs,
    /// process traces). Distinct from the harness's per-run JSON dir.
    pub debug_dir: Option<PathBuf>,
}

/// Handle the substrate returns from [`Substrate::setup`]. Carries
/// everything the agent and the harness need *during* a run:
///
/// - `workspace_root` — absolute path the agent is told to work in;
/// - `convention_text` — the substrate's AGENTS.md / equivalent (the
///   per-arm crib from `notes/sg2-benchmark-preregistration.md` §8.1);
/// - `repo_root` — root of the underlying repo (where the harness reads
///   ground-truth state at end-of-run);
/// - `label` — which arm this is (echoed into the [`crate::BenchRun`] for
///   manifest hygiene).
#[derive(Clone, Debug)]
pub struct SubstrateHandle {
    /// Arm identification. Same value as the substrate's [`Substrate::label`].
    pub label: SubstrateLabel,
    /// Where the agent works. Always absolute (the agent is told absolute
    /// paths per `ws/default/AGENTS.md` "Output Guidelines").
    pub workspace_root: PathBuf,
    /// Repo root (under which `workspace_root` lives, typically). The
    /// harness reads end-of-run ground truth here (Oracle B for arms
    /// where it applies, file enumeration otherwise).
    pub repo_root: PathBuf,
    /// The exact per-arm convention crib the agent receives in its prompt.
    /// Frozen per arm by §8.1 of the pre-registration — implementors
    /// must NOT vary it between runs.
    pub convention_text: String,
}

/// Errors a substrate can surface to the harness.
///
/// The harness handles these per §8.7 of the pre-registration: a setup
/// failure is a `discard_harness_bug` (not a substrate outcome); a
/// run-time substrate failure during an oracle check is propagated up.
#[derive(Debug)]
pub enum SubstrateError {
    /// Substrate could not initialize a clean state (the discard-class is
    /// `discard_harness_bug` per §8.7 — count it against the harness,
    /// not the substrate under test).
    Setup(String),
    /// Substrate could not be torn down. Logged but does NOT invalidate
    /// the run's measurements (the agent finished before this fired).
    Teardown(String),
    /// I/O or env error wrapping a system call.
    Io(std::io::Error),
}

impl std::fmt::Display for SubstrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Setup(m) => write!(f, "substrate setup failed: {m}"),
            Self::Teardown(m) => write!(f, "substrate teardown failed: {m}"),
            Self::Io(e) => write!(f, "substrate I/O error: {e}"),
        }
    }
}

impl std::error::Error for SubstrateError {}

impl From<std::io::Error> for SubstrateError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Pluggable arm interface. **T2.3 (`bn-mit2`) implements this 3+ times.**
///
/// # Contract for implementors
///
/// 1. [`Substrate::label`] is stable across `setup`/`teardown` calls.
/// 2. [`Substrate::setup`] MUST leave the substrate in a known-clean
///    state: a deterministic seed repo (mirroring SP3 §2's `/tmp` seed
///    repo with shared-file hotspot `src/lib.rs`), zero extant workspaces
///    beyond the substrate's own default, and pinned committer dates per
///    [`SubstrateConfig::base_git_time`]. Two `setup` calls with the same
///    seed MUST produce bit-identical seed-repo content.
/// 3. [`Substrate::setup`] MUST return the per-arm convention text from
///    `notes/sg2-benchmark-preregistration.md` §8.1, **verbatim from a
///    frozen string constant**. No per-run interpolation, no env-derived
///    variability — anything that varies the crib biases the comparison.
/// 4. [`Substrate::teardown`] is best-effort. The harness has already
///    captured measurements; teardown failures are logged and the run is
///    still counted.
///
/// # Why the trait does NOT have "run the agent"
///
/// Driving the agent is the harness's job; the substrate is *passive*
/// state. This split is what lets [`crate::BenchHarness`] plug a
/// [`crate::MockAgent`] in tests without each substrate adapter caring.
pub trait Substrate {
    /// Stable label identifying this arm.
    fn label(&self) -> SubstrateLabel;

    /// Initialize a clean per-run state and return the handle the agent
    /// will work against.
    ///
    /// # Errors
    ///
    /// Returns [`SubstrateError::Setup`] / [`SubstrateError::Io`] if the
    /// substrate cannot be brought to a known-clean state. The harness
    /// classifies this as `discard_harness_bug` per §8.7.
    fn setup(&mut self, config: &SubstrateConfig) -> Result<SubstrateHandle, SubstrateError>;

    /// Best-effort cleanup. Called after the harness has captured
    /// measurements. Implementations MUST NOT panic — return
    /// [`SubstrateError::Teardown`] for the harness to log.
    ///
    /// # Errors
    ///
    /// Returns [`SubstrateError::Teardown`] if cleanup partially fails.
    /// The harness logs but does not invalidate the run.
    fn teardown(&mut self, handle: SubstrateHandle) -> Result<(), SubstrateError>;
}

// ---------------------------------------------------------------------------
// NoopSubstrate — used by the harness's own self-test
// ---------------------------------------------------------------------------

/// A trivial in-memory substrate used by the harness's own tests.
///
/// Creates a tempdir on `setup` and removes it on `teardown`. Its
/// "convention text" is a stable placeholder string so the harness's
/// determinism test can byte-compare two runs without depending on a
/// real arm's crib. Never used in a published benchmark dataset.
///
/// The tempdir is leaked into [`SubstrateHandle::workspace_root`]; the
/// `_keep_alive` field on the substrate owns the [`tempfile::TempDir`]
/// guard so the dir survives until [`Substrate::teardown`] runs.
pub struct NoopSubstrate {
    /// Held tempdirs by created handles, indexed by `repo_root` so
    /// `teardown` can drop the matching guard.
    keep_alive: Vec<tempfile::TempDir>,
}

impl NoopSubstrate {
    /// Construct a fresh `NoopSubstrate`. Each `setup` allocates a new
    /// tempdir; `teardown` drops it.
    #[must_use]
    pub const fn new() -> Self {
        Self { keep_alive: Vec::new() }
    }

    /// Stable placeholder convention text. The harness's determinism
    /// test hashes the per-run JSON; embedding a fixed string here lets
    /// the test assert byte-identity across runs without depending on a
    /// production arm's crib churn.
    pub const NOOP_CRIB: &'static str = "# Noop substrate (test only)\n\
        - There are no commands to run.\n\
        - The agent's transcript IS the measurement.\n";
}

impl Default for NoopSubstrate {
    fn default() -> Self {
        Self::new()
    }
}

impl Substrate for NoopSubstrate {
    fn label(&self) -> SubstrateLabel {
        SubstrateLabel::Noop
    }

    fn setup(&mut self, _config: &SubstrateConfig) -> Result<SubstrateHandle, SubstrateError> {
        let dir = tempfile::TempDir::new()?;
        let root = dir.path().to_path_buf();
        self.keep_alive.push(dir);
        Ok(SubstrateHandle {
            label: SubstrateLabel::Noop,
            workspace_root: root.clone(),
            repo_root: root,
            convention_text: Self::NOOP_CRIB.to_string(),
        })
    }

    fn teardown(&mut self, handle: SubstrateHandle) -> Result<(), SubstrateError> {
        // Drop the tempdir whose path matches; ignore if missing (defensive).
        if let Some(pos) = self
            .keep_alive
            .iter()
            .position(|d| d.path() == handle.repo_root)
        {
            let _ = self.keep_alive.remove(pos);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_substrate_setup_and_teardown_clean() {
        let mut s = NoopSubstrate::new();
        let cfg = SubstrateConfig {
            seed: 42,
            base_git_time: 1_767_225_600,
            debug_dir: None,
        };
        let h = s.setup(&cfg).expect("noop setup");
        assert_eq!(h.label, SubstrateLabel::Noop);
        assert!(h.workspace_root.is_dir(), "tempdir exists during run");
        assert_eq!(h.convention_text, NoopSubstrate::NOOP_CRIB);
        s.teardown(h).expect("noop teardown");
    }

    #[test]
    fn noop_substrate_label_is_stable() {
        let s = NoopSubstrate::new();
        assert_eq!(s.label(), SubstrateLabel::Noop);
        assert_eq!(SubstrateLabel::Noop.as_str(), "noop");
    }

    #[test]
    fn substrate_label_string_form_is_kebab_case() {
        // The §6.4 manifest expects kebab-case (matches the §1.3 arm IDs).
        assert_eq!(SubstrateLabel::Maw.as_str(), "maw");
        assert_eq!(
            SubstrateLabel::GitWorktreesBare.as_str(),
            "git-worktrees-bare"
        );
        assert_eq!(SubstrateLabel::JjWorkspaces.as_str(), "jj-workspaces");
        assert_eq!(
            SubstrateLabel::ClaudeNativeWorktrees.as_str(),
            "claude-native-worktrees"
        );
    }
}
