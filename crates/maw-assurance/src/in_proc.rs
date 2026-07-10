//! In-process model driver for SG1 DST (bn-32k3, T1.6).
//!
//! This module implements the **in-process tier** the SG1 architecture
//! describes (`notes/sg1-dst-architecture.md` §1, the "workhorse" tier).
//! It is the substrate the T1.6 determinism guarantee tests and the T1.6
//! shrinker run against — fast enough (no `maw` subprocess, no `setsid`,
//! no `SIGKILL`) that thousands of shrink iterations are cheap.
//!
//! ## What this driver IS
//!
//! A deterministic, bit-exact applier of [`crate::scenario::ScenarioPlan`]
//! steps against a real git repo (`tempfile::TempDir`-rooted). Per op it
//! replicates the **ref-shape effect** maw would have produced — workspace
//! state/epoch/head refs, recovery refs on destroy, merge advances `main`
//! and `refs/manifold/epoch/current` — without invoking the merge FSM.
//! This is the same modelling level the existing
//! [`crate::oracle_a::tests`] and [`crate::oracle_b::tests`] use to plant
//! violations; we hoist it into the driver so a `ScenarioPlan` end-to-end
//! produces oracle verdicts deterministically.
//!
//! Crucially, every git write runs with `GIT_AUTHOR_DATE` /
//! `GIT_COMMITTER_DATE` pinned to `PlannedStep::git_time`, per the §5
//! determinism contract. Without this pin, commit OIDs embed wall-clock
//! time and re-running the same seed produces different OIDs — the bug SP1
//! caught and the precondition the T1.6 determinism tests verify.
//!
//! ## What this driver is NOT
//!
//! Not a faithful subprocess driver — that is [`crate::fault::SubprocFault`]
//! (T1.5). Not a full merge-FSM driver — that requires linking the
//! production `src/merge/*` pipeline and is the deeper integration T1.7
//! wires into CI. The minimum required by **T1.6** is a substrate that:
//!
//! 1. Reproduces a planted oracle violation deterministically per seed;
//! 2. Replays a `ScenarioPlan` end-to-end fast enough for shrinking;
//! 3. Provides the `(Oracle A | Oracle B)` verdict per step so the
//!    shrinker can check equivalence by violation class.
//!
//! ## Fault semantics
//!
//! A [`crate::scenario::FaultSpec::Failpoint`] attached to a merge step
//! semantically means "the merge crashed at this FSM phase". The driver
//! interprets that by skipping the merge's **cleanup**:
//!
//! - For an `Op::Merge { srcs, destroy: true, .. }` carrying a fault, the
//!   merge result still lands in `main`/`epoch/current` (the commit phase
//!   completed in many real bn-cm63 reproductions — the leak was the
//!   *cleanup* failing), but the source workspaces' `refs/manifold/head/*`
//!   ref is **not** torn down, **and** the `ws/<src>/` directory is also
//!   left in place (no destroy happened — that's a separate op). This is
//!   the bn-cm63 setup the canonical seed reaches: after the next planned
//!   `Op::Destroy { ws }` (which removes the directory), the head ref is
//!   left dangling → **Oracle B B1 RED**.
//!
//! - For a destroy-without-recovery scenario (the Oracle A canonical
//!   class), the harness manually plants the loss by issuing a destroy
//!   whose recovery ref intentionally is not pinned. This is the
//!   `inject_plant_work_loss` knob below — the T1.6 test that proves
//!   `Oracle A` reproduces a bit-exact violation across 10 replays
//!   uses it to plant the same lost-blob class the
//!   `tests::planted_work_loss_trips_oracle_a` Oracle A unit test plants.
//!
//! These are exactly the two classes the SG1 architecture says shrinkers
//! must reduce, and the two whose 10/10 reproduction T1.6 must guarantee.

#![cfg(feature = "oracles")]
// This module is harness/test-support code (the in-proc DST driver),
// not production-shipped public API. Relax the strictest pedantic /
// nursery clippy lints — they hurt readability of the substantial
// git-CLI plumbing here without buying real defect prevention. The
// workspace lints stay strict for the production crates.
#![allow(clippy::doc_markdown)]
#![allow(clippy::too_long_first_doc_paragraph)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::format_push_string)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::single_match_else)]
#![allow(clippy::manual_let_else)]
#![allow(clippy::redundant_closure_for_method_calls)]
#![allow(clippy::if_not_else)]
#![allow(clippy::similar_names)]
#![allow(clippy::missing_const_for_fn)]
#![allow(clippy::trivially_copy_pass_by_ref)]
#![allow(clippy::unused_self)]
#![allow(clippy::map_unwrap_or)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::case_sensitive_file_extension_comparisons)]
#![allow(clippy::uninlined_format_args)]
#![allow(clippy::manual_assert)]
#![allow(clippy::needless_pass_by_ref_mut)]
#![allow(clippy::doc_overindented_list_items)]
#![allow(clippy::format_collect)]
#![allow(clippy::if_then_some_else_none)]

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

use crate::oracle::{AssuranceViolation, WorkspaceStatus, capture_state};
use crate::oracle_a::{OracleA, StepReport};
use crate::oracle_b::{self, OracleBViolation};
use crate::scenario::{
    BaseRef, FaultSpec, FileEdit, Op, PlannedStep, ScenarioPlan, Seeded, Target, WsId,
};

// ---------------------------------------------------------------------------
// Violation verdict — what the driver reports per step
// ---------------------------------------------------------------------------

/// A unified verdict the driver hands back per step — exactly the shape the
/// shrinker uses to decide "did the SAME oracle trip with the SAME violation
/// class on this replay?".
#[derive(Clone, Debug)]
pub enum StepVerdict {
    /// Both oracles are green.
    Clean,
    /// Oracle A tripped. Carries the violation enum variant (lossy
    /// representation as the offending OID/ref string pair so the shrinker
    /// can do class equivalence without holding live `AssuranceViolation`
    /// values across replays).
    OracleA(OracleAClass),
    /// Oracle B tripped. Carries the first violation (deterministic
    /// B1→B2→B3→B4 order) reduced to its class signature.
    OracleB(OracleBClass),
}

/// Class signature for an Oracle A violation — enum-variant + offending
/// entity. The shrinker keeps a reduction iff this matches the original.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OracleAClass {
    /// `"ReachabilityLost"` for the only Oracle A violation class.
    pub kind: &'static str,
    /// The lost blob OID.
    pub oid: String,
}

/// Class signature for an Oracle B violation — enum-variant + offending
/// entity (workspace name, or ref name for malformed-recovery).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OracleBClass {
    /// `"DanglingHeadRef" | "DanglingOwnedRef" | "MergeStateOrphanSource"
    /// | "MergeStateBadEpoch" | "RecoveryRefMalformed"`.
    pub kind: &'static str,
    /// The offending workspace/source name (or the OID for bad-epoch /
    /// the ref name for malformed-recovery).
    pub entity: String,
}

impl StepVerdict {
    /// `true` iff this verdict is a violation (any oracle).
    #[must_use]
    pub const fn is_violation(&self) -> bool {
        !matches!(self, Self::Clean)
    }

    /// True iff `self` and `other` are the same violation class+entity.
    ///
    /// This is the **equivalence relation** the shrinker uses. A reduced
    /// plan is kept iff it produces the SAME class and SAME offending
    /// entity (not just any violation — that would let the shrinker drift
    /// onto an unrelated bug).
    #[must_use]
    pub fn same_class(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Clean, Self::Clean) => true,
            (Self::OracleA(a), Self::OracleA(b)) => a == b,
            (Self::OracleB(a), Self::OracleB(b)) => a == b,
            _ => false,
        }
    }
}

impl OracleAClass {
    fn from_violation(v: &AssuranceViolation) -> Self {
        match v {
            AssuranceViolation::ReachabilityLost { oid, .. } => Self {
                kind: "ReachabilityLost",
                oid: oid.clone(),
            },
            // Other AssuranceViolation variants are not Oracle A scope
            // (capture_state errors etc.) — funnel them into a sentinel
            // so the shrinker can still observe equivalence.
            other => Self {
                kind: "Other",
                oid: format!("{other}"),
            },
        }
    }
}

impl OracleBClass {
    fn from_violation(v: &OracleBViolation) -> Self {
        match v {
            OracleBViolation::DanglingHeadRef { workspace, .. } => Self {
                kind: "DanglingHeadRef",
                entity: workspace.clone(),
            },
            OracleBViolation::DanglingOwnedRef { workspace, .. } => Self {
                kind: "DanglingOwnedRef",
                entity: workspace.clone(),
            },
            OracleBViolation::MergeStateOrphanSource { source, .. } => Self {
                kind: "MergeStateOrphanSource",
                entity: source.clone(),
            },
            OracleBViolation::MergeStateBadEpoch { which, oid, .. } => Self {
                kind: "MergeStateBadEpoch",
                entity: format!("{which}:{oid}"),
            },
            OracleBViolation::RecoveryRefMalformed { ref_name, .. } => Self {
                kind: "RecoveryRefMalformed",
                entity: ref_name.clone(),
            },
            OracleBViolation::GitError { check, .. } => Self {
                kind: "GitError",
                entity: (*check).to_string(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// PlantedFault — knobs the harness can layer on a plan to inject defects
// ---------------------------------------------------------------------------

/// A defect the harness deliberately plants on top of a plan, used by the
/// T1.6 determinism tests to seed a guaranteed violation.
///
/// These are **not** generated by `DefaultScenarioGenerator` — they are
/// explicit harness annotations so the test can plant a known-good
/// violation and then prove (a) it reproduces 10/10 times and (b) the
/// shrinker reduces the plan around it.
///
/// ## Plant timing
///
/// Plants always fire **after the FINAL plan step**, regardless of how
/// long the plan is. This is deliberate: the shrinker reduces by removing
/// steps, and if a plant were keyed to a specific 0-based index, every
/// removal that crossed the index would orphan the plant and let the
/// shrinker mistakenly declare the reduction successful. With "always
/// after last", the plant rides with the plan's tail and the reduction
/// can keep removing prefix/middle steps until only the load-bearing
/// minimum remain (typically just the create+commit that authored the
/// witness blob).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlantedDefect {
    /// After the plan's last step fires, *also* delete every
    /// workspace-owned ref for `<ws>` **without** pinning a recovery ref,
    /// then prune the dangling commit so the authored content is genuinely
    /// unreachable. Oracle A must fire with `ReachabilityLost` on the next
    /// `check_step`. (Mirrors `oracle_a::tests::planted_work_loss_trips_oracle_a`.)
    WorkLoss {
        /// Workspace whose authored content is lost.
        ws: String,
    },
    /// After the plan's last step fires, leave the workspace's
    /// `refs/manifold/head/<ws>` in place but remove `ws/<ws>/` and all
    /// other owned refs. Oracle B B1 must fire with `DanglingHeadRef` on
    /// the next `check`. (Mirrors `oracle_b::tests::b1_fires_on_bn_cm63_reproduction`.)
    DanglingHeadRef {
        /// Workspace whose head ref is left dangling.
        ws: String,
    },
}

// ---------------------------------------------------------------------------
// InProcDriver — the workhorse
// ---------------------------------------------------------------------------

/// The in-process driver. Owns a temp git repo, applies plan steps,
/// invokes both oracles per step, returns the first violation (if any).
pub struct InProcDriver {
    repo: TempDir,
    /// Stable HEAD OID at repo init — every `WsCreate` builds on top of
    /// this. Recorded once so the driver is independent of the global
    /// `git init` HEAD shifting between platforms.
    root_oid: String,
    /// Planted defects to apply after specific plan steps.
    planted: Vec<PlantedDefect>,
    /// Reusable Oracle A across steps (incremental design).
    oracle_a: OracleA,
}

impl InProcDriver {
    /// Construct a fresh driver rooted at a new tempdir-backed git repo.
    pub fn new() -> std::io::Result<Self> {
        let repo = TempDir::new()?;
        let root = repo.path().to_path_buf();
        Self::init_repo(&root)?;
        let root_oid = git_capture(&root, &["rev-parse", "HEAD"]);
        std::fs::create_dir_all(root.join("ws"))?;
        let oracle_a = OracleA::new(&root);
        Ok(Self {
            repo,
            root_oid,
            planted: Vec::new(),
            oracle_a,
        })
    }

    /// Plant a defect that the driver will apply after the named step.
    #[must_use]
    pub fn with_planted(mut self, defects: Vec<PlantedDefect>) -> Self {
        self.planted = defects;
        self
    }

    /// Repo root (so tests/callers can inspect post-state).
    #[must_use]
    pub fn repo_root(&self) -> &Path {
        self.repo.path()
    }

    /// Drive `plan` end-to-end, invoking Oracle A + Oracle B after every
    /// step. Returns the first step's verdict that is a violation, or the
    /// final clean verdict if the whole plan passes.
    ///
    /// Planted defects fire **after the last step**, then a final oracle
    /// check runs so the planted violation is observed (otherwise a
    /// shrinker that removes the trailing step would silently lose the
    /// plant).
    #[must_use]
    pub fn drive(&mut self, plan: &ScenarioPlan) -> DriveOutcome {
        self.drive_inner(plan, /*check_each_step=*/ true)
    }

    /// Faster variant: apply every step, plant defects after the last,
    /// run oracles **only at the end**. Used by the shrinker to keep
    /// replays cheap (~ms per iter on top of the per-step apply cost).
    /// Soundness is preserved because the shrinker only ever asks
    /// "does the violation still reproduce", which a final-only check
    /// answers identically for a planted-at-tail defect.
    #[must_use]
    pub fn drive_fast(&mut self, plan: &ScenarioPlan) -> DriveOutcome {
        self.drive_inner(plan, /*check_each_step=*/ false)
    }

    fn drive_inner(&mut self, plan: &ScenarioPlan, check_each_step: bool) -> DriveOutcome {
        let mut steps_replayed = 0usize;
        let last_idx = plan.steps.len().saturating_sub(1);
        for (i, step) in plan.steps.iter().enumerate() {
            steps_replayed = i + 1;
            let _ = self.apply_op(step);

            // Per-step Oracle A harvest is REQUIRED even in fast mode:
            // Oracle A is incremental (`W` accretes across steps from
            // per-workspace deltas), so without a per-step harvest a
            // ws that gets destroyed before the final check would never
            // have its blobs witnessed → planted work-loss would silently
            // "vanish". This is the price of Oracle A's incremental
            // design (`oracle_a` SP2 §2.1); we accept it.
            //
            // We do NOT run Oracle B per-step in fast mode (Oracle B is
            // a stateless predicate — final check is sufficient).
            // Plants fire after the FINAL step (see PlantedDefect doc).
            if i == last_idx && !self.planted.is_empty() {
                let defects = self.planted.clone();
                for d in &defects {
                    self.apply_planted_defect(d, step);
                }
            }
            if check_each_step {
                let verdict = self.check_oracles(i);
                if verdict.is_violation() {
                    return DriveOutcome {
                        verdict,
                        steps_replayed,
                    };
                }
            } else if i < last_idx {
                // Fast mode: per-step Oracle A harvest only (no Oracle B).
                self.harvest_only(i);
            }
        }
        // Final oracle check (always — catches plants at tail and is the
        // only check done in fast mode).
        let final_verdict = self.check_oracles(last_idx);
        DriveOutcome {
            verdict: final_verdict,
            steps_replayed,
        }
    }

    /// Fast-mode-only helper: run **just** Oracle A's incremental harvest
    /// on the current state and discard the result. Keeps `W` accreting
    /// across steps without paying the full `check_oracles` cost.
    fn harvest_only(&mut self, step_index: usize) {
        let root = self.repo.path();
        let mut state = match capture_state(root) {
            Ok(s) => s,
            Err(_) => return,
        };
        state.workspaces.clear();
        if let Ok(entries) = std::fs::read_dir(root.join("ws")) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !entry.path().is_dir() {
                    continue;
                }
                let head_oid = git_capture_opt(
                    root,
                    &["rev-parse", "--verify", &refs_workspace_state(&name)],
                )
                .unwrap_or_default();
                state.workspaces.insert(
                    name,
                    WorkspaceStatus {
                        head_oid,
                        is_dirty: false,
                        exists: true,
                    },
                );
            }
        }
        let _ = self.oracle_a.check_step(&state, step_index);
    }

    // -- impl details below --

    fn init_repo(root: &Path) -> std::io::Result<()> {
        run_git(root, &["init", "-q", "-b", "main"])?;
        run_git(root, &["config", "user.name", "DST"])?;
        run_git(root, &["config", "user.email", "dst@maw"])?;
        run_git(root, &["config", "commit.gpgsign", "false"])?;
        // Pin the initial commit's clock to a fixed second so even the
        // repo-init step is deterministic across runs.
        let env = pinned_env(crate::scenario::GIT_TIME_BASE_FOR_DRIVER);
        std::fs::write(root.join("README.md"), "dst\n")?;
        run_git(root, &["add", "README.md"])?;
        run_git_env(root, &["commit", "-q", "--no-gpg-sign", "-m", "init"], &env)?;
        let head = git_capture(root, &["rev-parse", "HEAD"]);
        run_git(root, &["update-ref", "refs/manifold/epoch/current", &head])?;
        Ok(())
    }

    /// Apply a single plan step to the repo. Best-effort: a model-valid
    /// plan never produces errors here in normal operation.
    fn apply_op(&mut self, step: &PlannedStep) -> std::io::Result<()> {
        let root = self.repo.path().to_path_buf();
        let env = pinned_env(step.git_time);
        match &step.op {
            Op::WsCreate { ws, from } => self.do_ws_create(&root, ws, from, &env),
            Op::EditFiles { ws, files } => self.do_edit_files(&root, ws, files),
            Op::Commit { ws, msg } => self.do_commit(&root, ws, msg, &env),
            Op::Merge {
                srcs,
                into,
                destroy,
            } => self.do_merge(&root, srcs, into, *destroy, &step.fault, &env),
            // Advance is modelled like Sync at the in-proc level (no per-ws
            // epoch-staleness representation). Only generated when a profile
            // sets advance_weight > 0; the default soak profile never emits it.
            Op::Sync { ws } | Op::Advance { ws } => self.do_sync(&root, ws),
            Op::Destroy { ws, force: _ } => self.do_destroy(&root, ws, &env),
            Op::Recover { ws, to } => self.do_recover(&root, ws, to, &env),
            // bn-2bcx escape ops. Only generated when a profile sets
            // escape_weight > 0; the default in-proc soak profile keeps it 0, so
            // these arms are inert for the bn-2yzz campaign. The load-bearing
            // coverage for these ops is the production-code DST tier
            // (`tests/dst_production_tier.rs`), which drives the REAL maw binary
            // where the FF-absorb / dirty-trunk / gc-recover code actually lives.
            Op::OutOfMawCommit { files, msg } => self.do_out_of_maw_commit(&root, files, msg, &env),
            // The in-proc model has no real default worktree, so an uncommitted
            // trunk edit has no ref-shape effect to model — the driver that can
            // exercise it is the production tier.
            Op::DirtyTrunkWrite { .. } => Ok(()),
            Op::Gc {
                recovery_snapshots,
                older_than_days,
            } => self.do_gc(&root, *recovery_snapshots, *older_than_days),
        }
    }

    fn do_ws_create(
        &self,
        root: &Path,
        ws: &WsId,
        _from: &BaseRef,
        env: &[(String, String)],
    ) -> std::io::Result<()> {
        let ws_dir = root.join("ws").join(&ws.0);
        std::fs::create_dir_all(&ws_dir)?;
        // Plant a minimal sentinel so the worktree isn't empty; not
        // committed yet.
        std::fs::write(ws_dir.join(".maw-ws"), &ws.0)?;
        // Wire the workspace's owned refs to the root commit (`main` head).
        // This mirrors `maw ws create` which establishes head/state/epoch
        // refs pointing at the base epoch.
        let head = git_capture(root, &["rev-parse", "refs/manifold/epoch/current"]);
        run_git(root, &["update-ref", &refs_workspace_state(&ws.0), &head])?;
        run_git(root, &["update-ref", &refs_workspace_epoch(&ws.0), &head])?;
        // Create an oplog-head **blob** at the same shape `ensure_workspace_oplog_head`
        // would write so Oracle B has something to evaluate.
        let oplog_blob = git_hash_object_stdin(
            root,
            format!(r#"{{"workspace_id":"{}","epoch":"{}"}}"#, ws.0, head).as_bytes(),
        );
        run_git(
            root,
            &["update-ref", &refs_workspace_head(&ws.0), &oplog_blob],
        )?;
        let _ = env; // env not needed; no commit produced here
        Ok(())
    }

    fn do_edit_files(&self, root: &Path, ws: &WsId, files: &[FileEdit]) -> std::io::Result<()> {
        let ws_dir = root.join("ws").join(&ws.0);
        if !ws_dir.is_dir() {
            return Ok(()); // a planted destroy may have removed it
        }
        for f in files {
            let path = ws_dir.join(&f.path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, &f.content)?;
        }
        Ok(())
    }

    fn do_commit(
        &self,
        root: &Path,
        ws: &WsId,
        msg: &Seeded,
        env: &[(String, String)],
    ) -> std::io::Result<()> {
        // Manually build a commit at refs/manifold/ws/<ws> with the
        // workspace's edited file content (a single file per commit is
        // sufficient — we just need a real, hash-stable blob for Oracle
        // A's witness harvest).
        let ws_dir = root.join("ws").join(&ws.0);
        if !ws_dir.is_dir() {
            return Ok(()); // destroyed in flight
        }
        let mut tree_entries: Vec<(String, String)> = Vec::new();
        for entry in walk_files(&ws_dir) {
            let rel = entry
                .strip_prefix(&ws_dir)
                .unwrap_or(&entry)
                .to_string_lossy()
                .into_owned();
            // Skip git-internal & sentinel files.
            if rel.starts_with(".git") || rel == ".maw-ws" {
                continue;
            }
            let content = std::fs::read(&entry)?;
            let blob = git_hash_object_stdin(root, &content);
            tree_entries.push((rel, blob));
        }
        if tree_entries.is_empty() {
            return Ok(());
        }
        // Build a flat tree (paths with `/` get nested via mktree's flat
        // input format — we use one entry per file with `/` allowed only
        // at depth 1; canonicalise by sorting).
        tree_entries.sort();
        let mut mktree_input = String::new();
        for (path, blob) in &tree_entries {
            // Skip nested paths to keep the test tree flat; if the path
            // contains `/`, replace with a slot name. The plan's edit
            // paths are short ("ws-0/file-1.txt", "shared/file-0.txt").
            // Use the basename so mktree is happy.
            let basename = std::path::Path::new(path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone());
            mktree_input.push_str(&format!("100644 blob {blob}\t{basename}\n"));
        }
        // Deduplicate by basename (mktree refuses duplicates) — keep last.
        let mut dedup: Vec<(String, String)> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for line in mktree_input.lines().rev() {
            let parts: Vec<&str> = line.splitn(3, [' ', '\t']).collect();
            if parts.len() < 3 {
                continue;
            }
            let basename = parts[2].to_string();
            if seen.insert(basename.clone()) {
                dedup.push((basename, line.to_string()));
            }
        }
        dedup.sort_by(|a, b| a.0.cmp(&b.0));
        let mktree_input: String = dedup.iter().map(|(_, l)| format!("{l}\n")).collect();
        let tree = git_pipe(root, &["mktree"], mktree_input.as_bytes());
        // Parent: current ws tip if any, else main.
        let ws_ref = refs_workspace_state(&ws.0);
        let parent = git_capture_or(
            root,
            &["rev-parse", "--verify", &format!("{ws_ref}^{{commit}}")],
            &self.root_oid,
        );
        let commit = git_pipe_env(
            root,
            &["commit-tree", &tree, "-p", &parent, "-m", &msg.0],
            &[],
            env,
        );
        run_git(root, &["update-ref", &ws_ref, &commit])?;
        // Roll the workspace head ref to the new commit too (oplog
        // semantics aside, this gives Oracle A a fresh tip to harvest).
        run_git(root, &["update-ref", &refs_workspace_head(&ws.0), &commit])?;
        Ok(())
    }

    fn do_merge(
        &self,
        root: &Path,
        srcs: &[WsId],
        into: &Target,
        destroy: bool,
        fault: &FaultSpec,
        env: &[(String, String)],
    ) -> std::io::Result<()> {
        let _ = into; // we only model `Target::Default`
        // The merge's effect: advance main + refs/manifold/epoch/current
        // to the last source's tip; bump the per-ws epoch refs of
        // non-sources to mark them stale; optionally destroy sources.
        let Some(last) = srcs.last() else {
            return Ok(());
        };
        let ws_ref = refs_workspace_state(&last.0);
        let Some(new_tip) = git_capture_opt(root, &["rev-parse", "--verify", &ws_ref]) else {
            return Ok(()); // no tip to merge — model says the chooser still emitted it; no-op
        };
        // Synthesize a merge commit so main has a fresh OID (closer to maw's
        // semantics, which always emits a fresh epoch commit).
        let prev_main = git_capture(root, &["rev-parse", "refs/heads/main"]);
        let merge_tree = git_capture(root, &["rev-parse", &format!("{new_tip}^{{tree}}")]);
        let merge_msg = format!(
            "merge {srcs:?} -> default (in-proc-driver)",
            srcs = srcs.iter().map(|w| &w.0).collect::<Vec<_>>()
        );
        let merge_commit = git_pipe_env(
            root,
            &[
                "commit-tree",
                &merge_tree,
                "-p",
                &prev_main,
                "-m",
                &merge_msg,
            ],
            &[],
            env,
        );
        run_git(root, &["update-ref", "refs/heads/main", &merge_commit])?;
        run_git(
            root,
            &["update-ref", "refs/manifold/epoch/current", &merge_commit],
        )?;
        if destroy && !fault.is_some() {
            // Clean destroy of sources with recovery refs pinned.
            for src in srcs {
                self.do_destroy(root, src, env)?;
            }
        }
        // If `fault.is_some()`, the cleanup phase did NOT run — sources
        // are left in place; their head refs remain. This is the bn-cm63
        // setup that a follow-up `Op::Destroy { ws }` (or a planted
        // `DanglingHeadRef` defect) will then turn into a B1 violation.
        Ok(())
    }

    fn do_sync(&self, _root: &Path, _ws: &WsId) -> std::io::Result<()> {
        // Sync is a no-op at this modelling level (no per-ws epoch
        // staleness representation).
        Ok(())
    }

    /// Model an out-of-maw trunk commit: build a commit from `files` on top of
    /// `refs/heads/main` and advance `main` to it, WITHOUT touching
    /// `refs/manifold/epoch/current`. This is the FF-absorb arming condition
    /// (branch ahead of epoch) at the ref-shape level the in-proc model uses.
    fn do_out_of_maw_commit(
        &self,
        root: &Path,
        files: &[FileEdit],
        msg: &Seeded,
        env: &[(String, String)],
    ) -> std::io::Result<()> {
        let prev_main = git_capture(root, &["rev-parse", "refs/heads/main"]);
        let base_tree = git_capture(root, &["rev-parse", &format!("{prev_main}^{{tree}}")]);
        // Layer the seed-derived blobs onto the base tree via a fresh flat
        // tree (basename-only, matching do_commit's flattening).
        let mut mktree_input = String::new();
        // Preserve the base tree's existing entries by reading it back.
        let ls = git_capture(root, &["ls-tree", &base_tree]);
        for line in ls.lines() {
            mktree_input.push_str(line);
            mktree_input.push('\n');
        }
        for f in files {
            let blob = git_hash_object_stdin(root, f.content.as_bytes());
            let basename = std::path::Path::new(&f.path)
                .file_name()
                .map_or_else(|| f.path.clone(), |s| s.to_string_lossy().into_owned());
            mktree_input.push_str(&format!("100644 blob {blob}\t{basename}\n"));
        }
        // Dedup by basename (keep last), sort — mktree refuses dups / unsorted.
        let mut seen = std::collections::BTreeSet::new();
        let mut dedup: Vec<(String, String)> = Vec::new();
        for line in mktree_input.lines().rev() {
            let name = line.rsplit('\t').next().unwrap_or_default().to_string();
            if !name.is_empty() && seen.insert(name.clone()) {
                dedup.push((name, line.to_string()));
            }
        }
        dedup.sort_by(|a, b| a.0.cmp(&b.0));
        let tree_input: String = dedup.iter().map(|(_, l)| format!("{l}\n")).collect();
        let tree = git_pipe(root, &["mktree"], tree_input.as_bytes());
        let commit = git_pipe_env(
            root,
            &["commit-tree", &tree, "-p", &prev_main, "-m", &msg.0],
            &[],
            env,
        );
        run_git(root, &["update-ref", "refs/heads/main", &commit])?;
        // Deliberately DO NOT advance refs/manifold/epoch/current — that is the
        // whole point: main is now ahead of the epoch (drift to be absorbed).
        Ok(())
    }

    /// Model `maw gc`'s recovery-snapshot sweep at the ref-shape level: when
    /// `recovery_snapshots` and `older_than_days == 0`, drain the recovery-ref
    /// queue (the most hostile bn-3uou setting). Plain `maw gc` (recovery
    /// snapshots off) is modelled as a no-op — it only self-heals dangling head
    /// refs, which the in-proc model never leaks in isolation.
    fn do_gc(
        &self,
        root: &Path,
        recovery_snapshots: bool,
        older_than_days: u64,
    ) -> std::io::Result<()> {
        if !recovery_snapshots || older_than_days != 0 {
            // Age-gated sweeps keep recent snapshots; the in-proc model pins
            // all recovery refs at the same synthetic clock, so only the
            // drain-everything (older_than 0) case has a modellable effect.
            return Ok(());
        }
        let listing = git_capture(
            root,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/manifold/recovery/",
            ],
        );
        for ref_name in listing.lines() {
            let _ = run_git(root, &["update-ref", "-d", ref_name]);
        }
        Ok(())
    }

    fn do_destroy(&self, root: &Path, ws: &WsId, env: &[(String, String)]) -> std::io::Result<()> {
        let _ = env;
        let ws_dir = root.join("ws").join(&ws.0);
        // Pin a recovery ref BEFORE tearing refs down (well-behaved destroy).
        if let Some(tip) = git_capture_opt(
            root,
            &["rev-parse", "--verify", &refs_workspace_state(&ws.0)],
        ) {
            run_git(
                root,
                &[
                    "update-ref",
                    &format!(
                        "refs/manifold/recovery/{}/dst-{}",
                        ws.0,
                        env.iter()
                            .find(|(k, _)| k == "GIT_AUTHOR_DATE")
                            .map_or("0", |(_, v)| v.as_str())
                    ),
                    &tip,
                ],
            )?;
        }
        let _ = std::fs::remove_dir_all(&ws_dir);
        for owned in [
            refs_workspace_state(&ws.0),
            refs_workspace_epoch(&ws.0),
            refs_workspace_head(&ws.0),
        ] {
            let _ = run_git(root, &["update-ref", "-d", &owned]);
        }
        Ok(())
    }

    fn do_recover(
        &self,
        root: &Path,
        ws: &WsId,
        to: &WsId,
        env: &[(String, String)],
    ) -> std::io::Result<()> {
        let _ = env;
        // Pick the first recovery ref for `ws` (deterministic ordering
        // via for-each-ref's lexicographic output) and materialize a new
        // workspace at `to` from it.
        let listing = git_capture(
            root,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                &format!("refs/manifold/recovery/{}/", ws.0),
            ],
        );
        let Some(first) = listing.lines().next() else {
            return Ok(()); // no recovery ref — skip
        };
        let Some((_, oid)) = first.split_once(' ') else {
            return Ok(());
        };
        let ws_dir = root.join("ws").join(&to.0);
        std::fs::create_dir_all(&ws_dir)?;
        std::fs::write(ws_dir.join(".maw-ws"), &to.0)?;
        run_git(root, &["update-ref", &refs_workspace_state(&to.0), oid])?;
        run_git(root, &["update-ref", &refs_workspace_epoch(&to.0), oid])?;
        run_git(root, &["update-ref", &refs_workspace_head(&to.0), oid])?;
        Ok(())
    }

    fn apply_planted_defect(&self, defect: &PlantedDefect, _step: &PlannedStep) {
        let root = self.repo.path().to_path_buf();
        match defect {
            PlantedDefect::WorkLoss { ws } => {
                // Snapshot the tip's blob OIDs so we know what we're losing.
                let ws_ref = refs_workspace_state(ws);
                if let Some(_tip) = git_capture_opt(&root, &["rev-parse", "--verify", &ws_ref]) {
                    // Drop ALL owned refs + the ws dir + DO NOT pin recovery.
                    let _ = std::fs::remove_dir_all(root.join("ws").join(ws));
                    for owned in [
                        refs_workspace_state(ws),
                        refs_workspace_epoch(ws),
                        refs_workspace_head(ws),
                    ] {
                        let _ = run_git(&root, &["update-ref", "-d", &owned]);
                    }
                    // Aggressive prune so the blob is genuinely unreachable.
                    let _ = run_git(&root, &["reflog", "expire", "--expire=now", "--all"]);
                    let _ = run_git(&root, &["gc", "--prune=now", "--quiet"]);
                }
            }
            PlantedDefect::DanglingHeadRef { ws } => {
                // Rip the ws dir + state/epoch refs, but LEAVE head ref dangling.
                let _ = std::fs::remove_dir_all(root.join("ws").join(ws));
                for owned in [refs_workspace_state(ws), refs_workspace_epoch(ws)] {
                    let _ = run_git(&root, &["update-ref", "-d", &owned]);
                }
                // If there is no head ref yet (workspace was never
                // created), synthesize one pointing at the root commit so
                // B1 has something to flag.
                if git_capture_opt(&root, &["rev-parse", "--verify", &refs_workspace_head(ws)])
                    .is_none()
                {
                    let _ = run_git(
                        &root,
                        &["update-ref", &refs_workspace_head(ws), &self.root_oid],
                    );
                }
            }
        }
    }

    fn check_oracles(&mut self, step_index: usize) -> StepVerdict {
        let root = self.repo.path();
        // Oracle A first (incremental). We override `state.workspaces`
        // to reflect the **maw-ref-shape** view (head_oid taken from
        // `refs/manifold/ws/<ws>`) because the in-proc driver doesn't
        // create real per-ws git worktrees — so `capture_state`'s default
        // `git rev-parse HEAD` inside `ws/<x>/` would return empty and
        // Oracle A's witness harvest would skip every workspace. This
        // matches the modelling level the `oracle_a::tests` use
        // (`make_state` sets head_oid manually from the ws state ref).
        let mut state = match capture_state(root) {
            Ok(s) => s,
            Err(_) => {
                return StepVerdict::Clean; // best-effort
            }
        };
        state.workspaces.clear();
        if let Ok(entries) = std::fs::read_dir(root.join("ws")) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !entry.path().is_dir() {
                    continue;
                }
                let head_oid = git_capture_opt(
                    root,
                    &["rev-parse", "--verify", &refs_workspace_state(&name)],
                )
                .unwrap_or_default();
                state.workspaces.insert(
                    name,
                    WorkspaceStatus {
                        head_oid,
                        is_dirty: false,
                        exists: true,
                    },
                );
            }
        }
        if std::env::var("MAW_INPROC_DEBUG").is_ok() {
            eprintln!(
                "[in_proc] step {step_index}: refs={} workspaces={} W={} U={}",
                state.durable_refs.len(),
                state.workspaces.len(),
                self.oracle_a.witness_count(),
                self.oracle_a.reachable_count(),
            );
            for (n, s) in &state.workspaces {
                eprintln!("  ws {n}: head={} exists={}", s.head_oid, s.exists);
            }
        }
        match self.oracle_a.check_step(&state, step_index) {
            Ok(StepReport {
                violation: Some(v), ..
            }) => {
                return StepVerdict::OracleA(OracleAClass::from_violation(&v));
            }
            Ok(rep) => {
                if std::env::var("MAW_INPROC_DEBUG").is_ok() {
                    eprintln!(
                        "[in_proc] step {step_index} oracle A: clean (full_rescan={}, W={}, U={})",
                        rep.did_full_rescan, rep.witness_count, rep.reachable_count
                    );
                }
            }
            Err(_) => {}
        }
        // Oracle B.
        let bvs = oracle_b::check(root);
        if let Some(v) = bvs.into_iter().next() {
            return StepVerdict::OracleB(OracleBClass::from_violation(&v));
        }
        StepVerdict::Clean
    }
}

/// Outcome of [`InProcDriver::drive`].
#[derive(Clone, Debug)]
pub struct DriveOutcome {
    /// The first violating verdict, or `Clean`.
    pub verdict: StepVerdict,
    /// Number of plan steps actually replayed (the last one is the one
    /// that tripped, if any).
    pub steps_replayed: usize,
}

// ---------------------------------------------------------------------------
// Small git helpers (driver-private; not the oracle's verifier carveout)
// ---------------------------------------------------------------------------

fn pinned_env(git_time: i64) -> Vec<(String, String)> {
    // The pinned-clock contract (`notes/sg1-dst-architecture.md` §5.2):
    // export `GIT_AUTHOR_DATE` and `GIT_COMMITTER_DATE` derived from the
    // plan-step's `git_time`, so commit OIDs are a pure function of seed.
    let v = format!("{git_time} +0000");
    vec![
        ("GIT_AUTHOR_DATE".to_string(), v.clone()),
        ("GIT_COMMITTER_DATE".to_string(), v),
    ]
}

fn refs_workspace_state(ws: &str) -> String {
    format!("refs/manifold/ws/{ws}")
}
fn refs_workspace_epoch(ws: &str) -> String {
    format!("refs/manifold/epoch/ws/{ws}")
}
fn refs_workspace_head(ws: &str) -> String {
    format!("refs/manifold/head/{ws}")
}

fn run_git(root: &Path, args: &[&str]) -> std::io::Result<()> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

fn run_git_env(root: &Path, args: &[&str], env: &[(String, String)]) -> std::io::Result<()> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(root);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

fn git_capture(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("git spawn");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_capture_or(root: &Path, args: &[&str], default: &str) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("git spawn");
    if out.status.success() {
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    } else {
        default.to_string()
    }
}

fn git_capture_opt(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

fn git_pipe(root: &Path, args: &[&str], stdin: &[u8]) -> String {
    git_pipe_env(root, args, stdin, &[])
}

fn git_pipe_env(root: &Path, args: &[&str], stdin: &[u8], env: &[(String, String)]) -> String {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("git spawn");
    if !stdin.is_empty() {
        child.stdin.as_mut().unwrap().write_all(stdin).unwrap();
    }
    let out = child.wait_with_output().expect("git wait");
    if !out.status.success() {
        panic!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_hash_object_stdin(root: &Path, content: &[u8]) -> String {
    git_pipe(root, &["hash-object", "-w", "--stdin"], content)
}

fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let _ = walk_files_inner(dir, &mut out);
    out.sort();
    out
}
fn walk_files_inner(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            walk_files_inner(&p, out)?;
        } else {
            out.push(p);
        }
    }
    Ok(())
}

// Unused but reserved for tests that want to inspect frontier evolution.
#[allow(dead_code)]
fn list_refs(root: &Path) -> BTreeSet<String> {
    git_capture(root, &["for-each-ref", "--format=%(refname)"])
        .lines()
        .map(str::to_string)
        .collect()
}
