//! Substrate adapters for the SG2 agent-ergonomics benchmark (T2.3 / bn-mit2).
//!
//! Three substrate adapters present **equivalent task semantics** so the SG2
//! benchmark measures *substrate ergonomics*, not task wording or arm-specific
//! command flailing.
//!
//! - [`maw_adapter::MawAdapter`]: shells out to `maw ws create/sync/merge/destroy`.
//! - [`worktrees_adapter::WorktreesConventionAdapter`]: uses `git worktree
//!   add/remove` plus the thin written convention captured in
//!   `notes/sg2-worktrees-convention.md` — the honest baseline maw is compared
//!   against.
//! - [`jj_adapter::JjAdapter`]: uses `jj workspace add/forget` and preserves
//!   the SP3 opfork-wedge observability.
//!
//! All adapters implement the [`Substrate`] trait below. The trait is
//! intentionally minimal: it is the operation vocabulary T2.2's real-agent
//! driver invokes once the agent chooses an action; it is NOT the agent's
//! command surface (each arm receives its own command crib per the
//! pre-registration §8.1).
//!
//! # Equivalence is the load-bearing property
//!
//! Every per-adapter step is documented in `notes/sg2-adapter-parity.md`
//! (the parity-audit table). Any asymmetric step is justified there as
//! essential to that substrate's contract — not added for convenience.
//!
//! # Crate gating
//!
//! This crate is gated behind the `bench` feature so the default workspace
//! build pays zero compile cost. With the gate off, the crate exports only
//! the trait and the shared types so downstream crates can `use` them
//! without pulling in adapter code.

#![cfg_attr(not(feature = "bench"), allow(dead_code))]
// The whole crate is benchmarks-adjacent test infrastructure. Pedantic +
// nursery clippy is appropriate for production code; here we keep prose
// and panics deliberate (test helpers panic with context). The
// per-lint waivers below are explicit, not blanket — every one has a
// rationale.
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::if_not_else)]
#![allow(clippy::needless_pass_by_ref_mut)]
#![allow(clippy::map_unwrap_or)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::too_long_first_doc_paragraph)]
#![allow(clippy::needless_collect)]
#![allow(clippy::ignored_unit_patterns)]
#![allow(clippy::missing_const_for_fn)]
#![allow(clippy::redundant_clone)]
#![allow(clippy::used_underscore_binding)]
#![allow(clippy::single_match_else)]
#![allow(clippy::match_same_arms)]
#![allow(clippy::format_push_string)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::uninlined_format_args)]
#![allow(clippy::cloned_ref_to_slice_refs)]
#![allow(clippy::unnecessary_map_or)]
#![cfg_attr(test, allow(clippy::expect_used))]

use std::collections::BTreeMap;
use std::path::PathBuf;

use maw_scenario::WsId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[cfg(feature = "bench")]
pub mod jj_adapter;
#[cfg(feature = "bench")]
pub mod maw_adapter;
#[cfg(feature = "bench")]
pub mod worktrees_adapter;

// Re-export the file-walker helper so adapters share one collector and the
// equivalence test compares against the same byte set.
#[cfg(feature = "bench")]
pub(crate) use worktrees_adapter::collect_files as worktrees_adapter_collect_files;

// ---------------------------------------------------------------------------
// Shared types — the Substrate trait + result/error/state vocabulary.
// ---------------------------------------------------------------------------

/// Driver-agnostic side-effect outcome produced by a single
/// [`Substrate`] operation. Captures only what the *equivalence audit*
/// needs: did the op succeed, did it leave a substrate-visible conflict,
/// did it produce a recoverable orphan, did it advance the epoch.
///
/// Per-adapter native artifacts (`refs/manifold/recovery/*` for maw,
/// `.git/worktrees/<name>` for worktrees, divergent change-ids for jj) are
/// NOT in this struct on purpose: they are exactly the asymmetries the
/// parity table documents. Equivalence tests compare the
/// [`StepOutcome`] sequence; the parity table is what justifies why the
/// per-adapter `extra_artifacts` differ.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepOutcome {
    /// True iff the substrate reports the op completed without an
    /// adapter-visible error. (Substrate-recoverable conflicts are NOT
    /// errors — they are reported via [`Self::conflicted`].)
    pub ok: bool,
    /// True iff the op succeeded but left a substrate-visible conflict
    /// state the agent must resolve before continuing (jj-style: conflict
    /// is data, not error; the maw arm follows this convention too).
    pub conflicted: bool,
    /// True iff the op advanced the substrate's notion of an integration
    /// point — for maw the epoch ref, for worktrees+convention a merge
    /// commit on the target branch, for jj a commit landing on the
    /// integration branch.
    pub advanced_integration: bool,
    /// Free-form per-adapter notes for the parity audit. Equivalence
    /// tests ignore this; the publication (T5.3) cites them when
    /// presenting per-arm behavior. Bounded to keep traces small.
    pub notes: String,
}

/// Substrate state snapshot used by the equivalence test to assert all three
/// adapters reach equivalent end-states. Records only the *substrate-neutral*
/// surface; per-adapter artifacts are intentionally absent. The asymmetries
/// the parity audit names live in adapter-specific helpers, not here.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSnapshot {
    /// Live workspaces (id → terminal commit message; empty string if the
    /// workspace exists but has no commits yet). `BTreeMap` for stable
    /// equivalence comparison across runs and adapters.
    pub live_workspaces: BTreeMap<String, String>,
    /// Workspace ids the substrate has recorded as destroyed and
    /// recoverable (maw recovery refs; jj `forget`-able op-log entries;
    /// for worktrees+convention, the convention's claim-file archive).
    /// Names only — the contents are per-substrate.
    pub destroyed_workspaces: Vec<String>,
    /// Integration head label as the substrate presents it to the agent
    /// (e.g. `main`, `default`). Drives the `deliverable_integrated`
    /// oracle in T2.2.
    pub integration_head: Option<String>,
    /// Files visible on the integration head, with their content. Used by
    /// the equivalence test: all three adapters must materialize the same
    /// integrated bytes given the same `Op` sequence and a `NoopAgent`.
    pub integrated_files: BTreeMap<String, String>,
}

/// Adapter operation errors. Distinguishes "substrate said no" from "the
/// adapter or its host environment is broken" so the discard taxonomy
/// (pre-reg §8.7) can classify cleanly.
#[derive(Debug, Error)]
pub enum SubstrateError {
    /// The substrate refused the op (e.g. `maw ws destroy` without `--force`
    /// on an unmerged workspace). This is **counted** by the benchmark — the
    /// substrate's surface IS the measured thing.
    #[error("substrate refused: {0}")]
    Refused(String),
    /// The substrate ran but reported a logical conflict that the agent
    /// must resolve. NOT a discard event — conflicts are data (cf.
    /// `ws/default/AGENTS.md` "Conflicts Are Data, Not Errors").
    #[error("conflict: {0}")]
    Conflict(String),
    /// The substrate binary is missing on this host (e.g. `jj` not on
    /// PATH). The driver should treat this as a `discard_harness_bug`
    /// per pre-reg §8.7.
    #[error("substrate binary not found: {0}")]
    BinaryNotFound(String),
    /// I/O failure in the adapter (tempfile / spawn / fs). Discardable.
    #[error("adapter I/O failure: {0}")]
    Io(String),
    /// Subprocess exited non-zero with output captured. Distinct from
    /// `Refused` because some substrates blur the line; the adapter
    /// classifies based on the exit + stderr shape (each adapter
    /// documents its mapping in-module).
    #[error("substrate subprocess failed: exit={code:?} stderr={stderr}")]
    SubprocessFailed {
        /// Exit code if the subprocess reported one.
        code: Option<i32>,
        /// Captured stderr (truncated to a sane bound).
        stderr: String,
    },
}

/// Convenience result alias.
pub type Result<T> = std::result::Result<T, SubstrateError>;

/// The minimal substrate operation vocabulary. T2.2's real-agent driver
/// invokes these once an agent chooses an action; T2.4's metrics layer
/// derives oracle facts from the returned [`StepOutcome`]s plus the
/// per-adapter [`StateSnapshot`] taken at run end.
///
/// **Intent of the trait shape (for T2.2 to consume verbatim):**
///
/// 1. Methods are `&mut self`. Each substrate is single-writer for a given
///    benchmark run; the bench harness owns one adapter per (arm, run).
/// 2. No method takes a "transcript" or "agent" handle. The adapter is
///    pure substrate; agent/turn/cost accounting is T2.2's domain.
/// 3. The base-ref parameter for `create_workspace` is a [`maw_scenario::BaseRef`]
///    so the SG1 generator's plan is consumed verbatim — no per-adapter
///    re-resolution that would let adapters quietly diverge on what
///    "from main" means.
/// 4. `state_snapshot` is the *single* equivalence-test contract: any
///    adapter producing a different snapshot for the same scenario+agent
///    is a parity bug (`notes/sg2-adapter-parity.md` justifies every
///    permitted asymmetry; nothing here is permitted to disagree on the
///    substrate-neutral surface).
///
/// **Why this lives in the adapters crate, not `maw-bench`:** T2.2 is
/// in-flight in parallel; we ship the trait here so T2.2 can pull it in
/// (or copy it verbatim into `maw-bench`) with zero coordination friction.
/// Once T2.2 lands, the canonical home is `maw_bench::Substrate` and this
/// module re-exports it.
pub trait Substrate {
    /// Stable identifier for the substrate (`"maw"`, `"git-worktrees-bare"`,
    /// `"jj-workspaces"`). Used in T2.2's per-run manifest (§6.4 `arm`).
    fn arm_name(&self) -> &'static str;

    /// Root directory the substrate scribbles into. Lifetime-bound by the
    /// adapter (typically a [`tempfile::TempDir`]).
    fn root(&self) -> &PathBuf;

    /// Create a new workspace identified by `ws`, branching from `base`.
    fn create_workspace(
        &mut self,
        ws: &WsId,
        base: &maw_scenario::BaseRef,
    ) -> Result<StepOutcome>;

    /// Write/overwrite `path` inside `ws` with `content` (text). Adapters
    /// may collapse multiple edits to the same `(ws, path)` between commits.
    fn edit_file(&mut self, ws: &WsId, path: &str, content: &str) -> Result<StepOutcome>;

    /// Commit any pending edits inside `ws` with `msg`.
    fn commit(&mut self, ws: &WsId, msg: &str) -> Result<StepOutcome>;

    /// Merge `srcs` into `target` (a substrate-native integration label —
    /// `"default"` for maw, the merge-target branch for worktrees, the
    /// integration commit for jj). If `destroy_sources`, drop sources on
    /// success (sources NOT dropped if the merge conflicts — Prime Invariant).
    fn merge(
        &mut self,
        srcs: &[WsId],
        target: &str,
        destroy_sources: bool,
    ) -> Result<StepOutcome>;

    /// Sync `ws` to the current integration head (`maw ws sync`,
    /// `git rebase`, `jj workspace update-stale` + `jj rebase` depending
    /// on the substrate).
    fn sync(&mut self, ws: &WsId) -> Result<StepOutcome>;

    /// Destroy `ws`. `force` corresponds to maw's `--force` (capture
    /// recovery snapshot anyway); ignored by substrates that always
    /// preserve recovery state (jj op-log, git reflog).
    fn destroy(&mut self, ws: &WsId, force: bool) -> Result<StepOutcome>;

    /// Substrate-neutral state snapshot for equivalence checks.
    fn state_snapshot(&self) -> Result<StateSnapshot>;

    /// Tear down the substrate root. Idempotent. Called by the bench
    /// harness when a run ends, even if the run failed. Adapters
    /// generally drop their owned `TempDir` here.
    fn cleanup(&mut self) -> Result<()>;
}

// ---------------------------------------------------------------------------
// NoopAgent — a stand-in "agent" used by the equivalence test in tests/.
// It does NO interpretation; it replays a planned op stream verbatim. The
// equivalence test asserts that the resulting StateSnapshot is identical
// across all three substrates (modulo per-adapter artifacts excluded by
// construction from StateSnapshot).
// ---------------------------------------------------------------------------

/// A trivial scripted "agent" that the equivalence test uses. NOT used by
/// SG2 itself — SG2 spawns real Claude agents per T2.2.
///
/// The script is a `Vec<ScriptedOp>` and the agent replays it on each
/// substrate. Because the script is the same across substrates, any
/// substrate-attributed difference in the [`StateSnapshot`] is a parity bug.
#[derive(Clone, Debug)]
pub struct NoopAgent;

/// The scripted op set the [`NoopAgent`] understands. Deliberately a tiny
/// subset of [`maw_scenario::Op`] — equivalence is about the substrate
/// surface, not the full hostile-interleaving generator.
#[derive(Clone, Debug)]
pub enum ScriptedOp {
    /// Create `ws` from `base`.
    Create {
        /// Workspace id.
        ws: WsId,
        /// Base ref.
        base: maw_scenario::BaseRef,
    },
    /// Write `content` to `path` inside `ws`.
    Edit {
        /// Workspace id.
        ws: WsId,
        /// Relative path inside the workspace.
        path: String,
        /// Text content.
        content: String,
    },
    /// Commit `ws` with `msg`.
    Commit {
        /// Workspace id.
        ws: WsId,
        /// Commit message.
        msg: String,
    },
    /// Merge `srcs` into `target` (substrate-native label) optionally
    /// destroying sources.
    Merge {
        /// Source workspaces.
        srcs: Vec<WsId>,
        /// Integration target label.
        target: String,
        /// Drop sources on success.
        destroy: bool,
    },
    /// Destroy `ws`.
    Destroy {
        /// Workspace id.
        ws: WsId,
        /// Force flag.
        force: bool,
    },
}

impl NoopAgent {
    /// Replay `script` against `subs`, collecting each step's outcome.
    ///
    /// # Errors
    ///
    /// Propagates substrate errors verbatim. The equivalence test treats
    /// any error as a parity divergence to be investigated.
    pub fn drive<S: Substrate>(
        subs: &mut S,
        script: &[ScriptedOp],
    ) -> Result<Vec<StepOutcome>> {
        let mut out = Vec::with_capacity(script.len());
        for op in script {
            let step = match op {
                ScriptedOp::Create { ws, base } => subs.create_workspace(ws, base)?,
                ScriptedOp::Edit { ws, path, content } => subs.edit_file(ws, path, content)?,
                ScriptedOp::Commit { ws, msg } => subs.commit(ws, msg)?,
                ScriptedOp::Merge {
                    srcs,
                    target,
                    destroy,
                } => subs.merge(srcs, target, *destroy)?,
                ScriptedOp::Destroy { ws, force } => subs.destroy(ws, *force)?,
            };
            out.push(step);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Subprocess helper — tiny, audited, used by all three adapters. Centralized
// so the parity table can cite *one* spawning policy (no shell, no PATH
// inheritance surprises, captured stderr).
// ---------------------------------------------------------------------------

#[cfg(feature = "bench")]
pub(crate) mod proc_util {
    use super::SubstrateError;
    use std::ffi::OsStr;
    use std::path::Path;
    use std::process::Command;

    /// Run `bin` with `args` from `cwd`. Captures stdout+stderr; non-zero
    /// exit becomes `SubprocessFailed`. Never invokes a shell.
    pub fn run(bin: &str, args: &[&str], cwd: &Path) -> Result<String, SubstrateError> {
        let output = Command::new(bin)
            .args(args)
            .current_dir(cwd)
            // Quiet noisy git config dialogs and per-user aliases.
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            // Stable, isolated config so host gitconfig can't influence
            // determinism. The adapters explicitly set user.name/email
            // inside repos that need it — these env vars are belt-and-braces.
            .env("GIT_AUTHOR_NAME", "bench")
            .env("GIT_AUTHOR_EMAIL", "bench@localhost")
            .env("GIT_COMMITTER_NAME", "bench")
            .env("GIT_COMMITTER_EMAIL", "bench@localhost")
            .output()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    // NotFound from `Command::output` can mean either the
                    // binary is missing OR cwd doesn't exist. Disambiguate
                    // so the discard-classification (pre-reg §8.7) is right.
                    if !cwd.exists() {
                        SubstrateError::Io(format!(
                            "cwd does not exist: {} (running `{} ...`)",
                            cwd.display(),
                            bin
                        ))
                    } else {
                        SubstrateError::BinaryNotFound(bin.to_string())
                    }
                }
                _ => SubstrateError::Io(format!("spawn {bin}: {e}")),
            })?;
        if !output.status.success() {
            // Capture both streams in `stderr` (git merge prints CONFLICT
            // to stdout, error reasons to stderr; the caller pattern-matches
            // on substring so we concat with a separator).
            let combined = format!(
                "{}\n--stderr--\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return Err(SubstrateError::SubprocessFailed {
                code: output.status.code(),
                stderr: truncate(&combined, 8_192),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Run with a list of extra env pairs. Same semantics as [`run`].
    pub fn run_envs(
        bin: &str,
        args: &[&str],
        cwd: &Path,
        envs: &[(&str, &str)],
    ) -> Result<String, SubstrateError> {
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_AUTHOR_NAME", "bench")
            .env("GIT_AUTHOR_EMAIL", "bench@localhost")
            .env("GIT_COMMITTER_NAME", "bench")
            .env("GIT_COMMITTER_EMAIL", "bench@localhost");
        for (k, v) in envs {
            cmd.env(OsStr::new(k), OsStr::new(v));
        }
        let output = cmd.output().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                if !cwd.exists() {
                    SubstrateError::Io(format!(
                        "cwd does not exist: {} (running `{} ...`)",
                        cwd.display(),
                        bin
                    ))
                } else {
                    SubstrateError::BinaryNotFound(bin.to_string())
                }
            }
            _ => SubstrateError::Io(format!("spawn {bin}: {e}")),
        })?;
        if !output.status.success() {
            let combined = format!(
                "{}\n--stderr--\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return Err(SubstrateError::SubprocessFailed {
                code: output.status.code(),
                stderr: truncate(&combined, 8_192),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Same as [`run`] but treats a non-zero exit as `Ok` (returns
    /// combined stdout+stderr). For substrates that use exit codes as a
    /// status channel (jj, in some cases).
    pub fn run_lenient(bin: &str, args: &[&str], cwd: &Path) -> Result<String, SubstrateError> {
        let output = Command::new(bin)
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_AUTHOR_NAME", "bench")
            .env("GIT_AUTHOR_EMAIL", "bench@localhost")
            .env("GIT_COMMITTER_NAME", "bench")
            .env("GIT_COMMITTER_EMAIL", "bench@localhost")
            .output()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    // NotFound from `Command::output` can mean either the
                    // binary is missing OR cwd doesn't exist. Disambiguate
                    // so the discard-classification (pre-reg §8.7) is right.
                    if !cwd.exists() {
                        SubstrateError::Io(format!(
                            "cwd does not exist: {} (running `{} ...`)",
                            cwd.display(),
                            bin
                        ))
                    } else {
                        SubstrateError::BinaryNotFound(bin.to_string())
                    }
                }
                _ => SubstrateError::Io(format!("spawn {bin}: {e}")),
            })?;
        let mut s = String::from_utf8_lossy(&output.stdout).into_owned();
        s.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok(s)
    }

    fn truncate(s: &str, max: usize) -> String {
        if s.len() <= max {
            s.to_string()
        } else {
            let mut out = s[..max].to_string();
            out.push_str("\n...[truncated]");
            out
        }
    }
}
