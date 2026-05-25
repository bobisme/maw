//! DST fault-injection / kill / recovery driver (bn-263u).
//!
//! This is the **net-new** code SP1 (bn-imw8 ADR) identified as missing from
//! `maw-assurance`: a layer that can, *from a seed*, deterministically crash a
//! merge at any named `FP_*` phase, issue a **real** `SIGKILL` at the
//! dangerous boundaries, and then drive a bounded, self-reaping recovery so
//! Oracles A/B ([`crate::oracle`]) can run on the recovered state.
//!
//! It is the shared crash core for **both** SG1 execution tiers (SP1 chose
//! HYBRID, "build once, drive two ways"):
//!
//! - **In-process tier** ([`InProcFault`]): the workhorse. The DST harness
//!   links `maw` + `maw-core`, drives the real merge FSM, and asks this layer
//!   to arm/clear a seed-selected fault via the existing
//!   `maw_core::failpoints` registry. Bit-exact, ~24 seq/s.
//! - **Faithful subprocess tier** ([`SubprocFault`]): spawns the *shipped*
//!   `maw` binary detached, exports `MAW_FP` so it crashes deterministically
//!   at the chosen boundary (SP1 Finding A: the old `sleep`-window could only
//!   crash the widened phase), sends a real `kill -9 -<pgid>`, then runs the
//!   recovery retry. Outcome-deterministic; the only tier that proves real
//!   crash recovery (zombies, pid-reuse, owner-liveness, partial fsync).
//!
//! ## SP1 Finding B — recovery MUST reap its own children
//!
//! A real `SIGKILL` leaves the victim a **zombie** until its parent reaps it.
//! `maw`'s owner-liveness probe reads `/proc/<pid>`, where a zombie still
//! reads as *alive*, so the first naive retry refuses with *"merge already in
//! progress … owned by a running process"*. This is **not a maw bug** — it is
//! a hard requirement on the harness. [`recover_with_retry`] therefore
//! **reaps the harness's own child** (`Child::wait()`) *before* the first
//! retry and models recovery as a **bounded retry loop with backoff**, never a
//! single call. The in-process tier never surfaces this class (no separate
//! owner process), which is exactly why the faithful tier is non-optional.
//!
//! Everything here is behind the `fault-injection` cargo feature so the
//! default `maw-assurance` build (and the default `maw` release) stay clean
//! and zero-overhead. The in-proc registry control additionally requires the
//! `failpoints` feature on `maw-core` (transitively pulled by this feature).
//!
//! `unsafe` is forbidden workspace-wide, so the real kill is delivered via the
//! `kill(1)` binary (`kill -9 -<pgid>`), exactly the bn-cm63 chaos pattern,
//! rather than a raw `libc::kill` FFI call.

#![cfg(feature = "fault-injection")]
// This module's docs deliberately use bare domain acronyms (SP1, SIGKILL,
// MAW_FP, FSM, RAII, pgid, …) and long rationale paragraphs that cite the SP1
// ADR verbatim. It is harness/test-support code, not a shipped public API, so
// relax the two cosmetic pedantic doc lints here rather than backtick-noise
// every acronym.
#![allow(clippy::doc_markdown)]
#![allow(clippy::too_long_first_doc_paragraph)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use maw_core::failpoints::{self, FailpointAction};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

/// The dangerous boundaries where a *real* `SIGKILL` (not a clean unwind) is
/// required for crash fidelity. SP1 calls these out explicitly: the two-ref
/// CAS straddle, plus the post-commit cleanup and pre-commit prepare writes.
///
/// A fault landing on any of these in the faithful tier is delivered as an OS
/// kill; faults elsewhere may use the cheaper in-binary `error`/`panic`.
pub const DANGEROUS_FAILPOINTS: &[&str] = &[
    "FP_COMMIT_BETWEEN_CAS_OPS",
    "FP_COMMIT_AFTER_EPOCH_CAS",
    "FP_COMMIT_BEFORE_BRANCH_CAS",
    "FP_CLEANUP_AFTER_CAPTURE",
    "FP_CLEANUP_BEFORE_DEFAULT_CHECKOUT",
    "FP_PREPARE_BEFORE_STATE_WRITE",
    "FP_PREPARE_AFTER_STATE_WRITE",
];

/// The full pool of crashable `FP_*` sites, grouped by FSM phase, that the
/// seed selects from. Mirrors the canonical
/// `maw_core::failpoints::KNOWN_FAILPOINTS` table but ordered by phase so a
/// seed maps stably to a `(phase, failpoint)` pair across releases (new
/// failpoints appended per-phase do not reshuffle existing seeds for earlier
/// phases).
///
/// **bn-4qwp (T2.1):** the canonical definition now lives in `maw-scenario`
/// because the scenario generator's [`maw_scenario::FaultSpec::Failpoint`]
/// selection is the load-bearing use site (every driver — in-proc, faithful
/// subprocess, real-agent — picks failpoint sites by indexing into this
/// table). This re-export preserves the pre-factor public path
/// `maw_assurance::fault::CRASHABLE_BY_PHASE` so existing consumers
/// (in-proc tier, `InProcFault::arm`, fault tests below) compile unchanged.
pub use maw_scenario::CRASHABLE_BY_PHASE;

/// Which crash mechanism a [`FaultPlan`] uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashKind {
    /// Clean in-process unwind (`FailpointAction::Error`). Used by the
    /// in-proc tier where there is no separate process to kill; lets recovery
    /// be observed in the same process.
    Unwind,
    /// `FailpointAction::Panic` — unwinds/aborts the thread; used when a seed
    /// wants a harsher in-proc failure than a clean `Err`.
    Panic,
    /// A real OS `SIGKILL` of the whole process group, delivered by the
    /// faithful tier at a dangerous boundary. The most faithful crash.
    SigKill,
}

/// A deterministic crash plan derived purely from a seed.
///
/// The same seed always yields the same `(phase, failpoint, kind)` triple, so
/// a failing run is replayable by its seed (bit-exact in the in-proc tier;
/// outcome-deterministic in the faithful tier — the *instruction* killed by a
/// real SIGKILL is inherently non-deterministic, by SP1's accepted trade).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FaultPlan {
    /// The seed this plan was derived from (for replay messaging).
    pub seed: u64,
    /// FSM phase the fault lands in (`prepare`/`build`/…/`cleanup`).
    pub phase: String,
    /// The exact `FP_*` site to crash at.
    pub failpoint: String,
    /// How the crash is delivered.
    pub kind: CrashKind,
}

impl FaultPlan {
    /// Derive a deterministic fault plan from `seed`.
    ///
    /// Selection is stable: a phase is chosen, then a failpoint within it,
    /// then the kind is forced to [`CrashKind::SigKill`] whenever the chosen
    /// site is in [`DANGEROUS_FAILPOINTS`] (SP1: those boundaries must be
    /// proven under a *real* kill), otherwise the seed picks unwind vs panic.
    #[must_use]
    pub fn from_seed(seed: u64) -> Self {
        let mut rng = StdRng::seed_from_u64(seed);
        let (phase, sites) = CRASHABLE_BY_PHASE[rng.random_range(0..CRASHABLE_BY_PHASE.len())];
        let failpoint = sites[rng.random_range(0..sites.len())];
        let kind = if DANGEROUS_FAILPOINTS.contains(&failpoint) {
            CrashKind::SigKill
        } else if rng.random_bool(0.5) {
            CrashKind::Unwind
        } else {
            CrashKind::Panic
        };
        Self {
            seed,
            phase: phase.to_string(),
            failpoint: failpoint.to_string(),
            kind,
        }
    }

    /// Render this plan's fault as a `MAW_FP` spec value for the faithful
    /// subprocess tier (consumed by `maw_core::failpoints::parse_env_spec`).
    ///
    /// `SigKill` is exported as `error` — the env bridge's job is only to make
    /// the child *reach* the boundary deterministically; the real kill is then
    /// delivered out-of-band by [`SubprocFault::run`] once the state file
    /// shows the target phase (SP1: "the env bridge replaces the sleep-window
    /// race, not the kill").
    #[must_use]
    pub fn maw_fp_spec(&self) -> String {
        let action = match self.kind {
            CrashKind::Panic => "panic:dst-injected",
            // Unwind and SigKill both export `error`; SigKill is delivered
            // separately by the harness at the observed phase.
            CrashKind::Unwind | CrashKind::SigKill => "error:dst-injected",
        };
        format!("{}={action}", self.failpoint)
    }
}

// ---------------------------------------------------------------------------
// In-process tier
// ---------------------------------------------------------------------------

/// In-process fault arming/clearing via the `maw_core::failpoints` registry.
///
/// The DST harness links the real merge FSM and calls [`Self::arm`] before
/// driving a merge, then [`Self::disarm`] before the recovery/retry. RAII:
/// dropping also disarms, so a panicking test never leaks a live failpoint
/// into a sibling test in the same process.
#[derive(Debug)]
pub struct InProcFault {
    failpoint: &'static str,
}

impl InProcFault {
    /// Arm the fault described by `plan` in the global failpoint registry.
    ///
    /// `failpoint` must be a `'static` name from [`CRASHABLE_BY_PHASE`] (the
    /// registry keys are `&'static str`); pass the matching constant.
    ///
    /// `SigKill` is meaningless in-process (no separate process), so it is
    /// armed as a clean `Error` unwind — the faithful tier is what proves the
    /// real-kill behaviour for those sites.
    #[must_use]
    pub fn arm(failpoint: &'static str, plan: &FaultPlan) -> Self {
        let action = match plan.kind {
            CrashKind::Panic => FailpointAction::Panic("dst-injected".to_string()),
            CrashKind::Unwind | CrashKind::SigKill => {
                FailpointAction::Error("dst-injected".to_string())
            }
        };
        failpoints::set(failpoint, action);
        Self { failpoint }
    }

    /// Disarm just this fault (idempotent).
    pub fn disarm(&self) {
        failpoints::clear(self.failpoint);
    }
}

impl Drop for InProcFault {
    fn drop(&mut self) {
        failpoints::clear(self.failpoint);
    }
}

// ---------------------------------------------------------------------------
// Faithful subprocess tier
// ---------------------------------------------------------------------------

/// Outcome of a faithful subprocess crash+recovery run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubprocOutcome {
    /// The target phase was observed, a real SIGKILL delivered, and the
    /// bounded recovery retry eventually succeeded. `recovery_attempts` is how
    /// many `maw ws merge` retries it took (SP1: the first is racy; >1 is
    /// expected and not a failure).
    Recovered { killed_phase: String, recovery_attempts: u32 },
    /// The target phase was never observed within the deadline (a timing miss,
    /// e.g. SP1 Finding A's sub-20 ms BUILD without the env bridge). This is
    /// **not** a maw fault — the caller should replay or widen.
    PhaseNotObserved { target: String },
    /// The phase was killed but every recovery retry was exhausted. This *is*
    /// a candidate Prime-Invariant failure for the oracle to adjudicate.
    RecoveryExhausted { killed_phase: String, attempts: u32 },
}

/// Faithful subprocess crash driver: spawn the *shipped* `maw` detached,
/// crash it for real at the seed-selected phase, then recover.
///
/// The caller owns repo setup (a real v2 bare repo + a committed source
/// workspace). This type only owns the crash + reap + recovery dance and is
/// agnostic to how the oracle then inspects `repo_root`.
pub struct SubprocFault {
    plan: FaultPlan,
    maw_bin: PathBuf,
    repo_root: PathBuf,
    /// Source workspace + merge args, e.g. `["ws","merge","rz","--into","default","--message","rz"]`.
    merge_args: Vec<String>,
    /// Max wall time to wait for the target phase to appear in the state file.
    observe_timeout: Duration,
    /// Max recovery retries (SP1 measured ~6 s total / a few attempts).
    max_recovery_attempts: u32,
    /// Backoff between recovery retries (SP1: 1 s too short; seconds needed
    /// for pid-reap + maw's conservative liveness recheck).
    recovery_backoff: Duration,
}

impl SubprocFault {
    /// Construct a faithful driver.
    ///
    /// `merge_args` is the argv (after the binary) for the merge that will be
    /// crashed and then retried, e.g.
    /// `["ws","merge","rz","--into","default","--message","rz"]`.
    #[must_use]
    pub fn new(
        plan: FaultPlan,
        maw_bin: impl Into<PathBuf>,
        repo_root: impl Into<PathBuf>,
        merge_args: Vec<String>,
    ) -> Self {
        Self {
            plan,
            maw_bin: maw_bin.into(),
            repo_root: repo_root.into(),
            merge_args,
            observe_timeout: Duration::from_secs(20),
            max_recovery_attempts: 8,
            recovery_backoff: Duration::from_secs(1),
        }
    }

    /// Override the phase-observation deadline (default 20 s).
    #[must_use]
    pub const fn with_observe_timeout(mut self, d: Duration) -> Self {
        self.observe_timeout = d;
        self
    }

    /// Override the recovery retry budget / backoff (defaults: 8 × 1 s).
    #[must_use]
    pub const fn with_recovery(mut self, attempts: u32, backoff: Duration) -> Self {
        self.max_recovery_attempts = attempts;
        self.recovery_backoff = backoff;
        self
    }

    /// Read the current merge phase from `<root>/.manifold/merge-state.json`.
    fn read_phase(&self) -> Option<String> {
        read_merge_phase(&self.repo_root)
    }

    /// Spawn the merge detached in its own process group, with `MAW_FP` set so
    /// the shipped binary deterministically reaches the target boundary.
    ///
    /// Returns the child (its pid is the process-group id, since `setsid`
    /// makes it the group leader). `stdout`/`stderr` are discarded.
    fn spawn_detached_merge(&self) -> std::io::Result<Child> {
        let mut cmd = Command::new("setsid");
        cmd.arg(&self.maw_bin)
            .args(&self.merge_args)
            .current_dir(&self.repo_root)
            .env("MAW_FP", self.plan.maw_fp_spec())
            // SP1 determinism contract: pin git dates so any commit OID the
            // crashed merge produced is a pure function of the seed.
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00 +0000")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00 +0000")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd.spawn()
    }

    /// Run the full faithful cycle: spawn detached → poll for the target
    /// phase → real `kill -9 -<pgid>` → **reap our own child** → bounded
    /// recovery retry. Returns the [`SubprocOutcome`]; the caller then runs
    /// the oracle against `repo_root`.
    ///
    /// # Errors
    ///
    /// Returns an error only if the child cannot be spawned at all (e.g. the
    /// `maw` binary path is wrong) — every *expected* crash path is encoded in
    /// [`SubprocOutcome`], not in `Err`.
    pub fn run(&self) -> std::io::Result<SubprocOutcome> {
        let mut child = self.spawn_detached_merge()?;
        let pgid = child.id();

        // ---- poll the state-file logical clock, then a REAL group kill ----
        let deadline = Instant::now() + self.observe_timeout;
        let mut killed_phase: Option<String> = None;
        while Instant::now() < deadline {
            if let Some(phase) = self.read_phase()
                && phase == self.plan.phase
            {
                kill_process_group(pgid);
                killed_phase = Some(phase);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        // SP1 Finding B: reap our own child *unconditionally* and *before*
        // any recovery attempt. A SIGKILLed child is a zombie until waited
        // on; maw's `/proc/<pid>` liveness probe reads a zombie as ALIVE and
        // refuses recovery. `wait()` collects the setsid group leader; the
        // kernel orphan-reaper collects the rest of the group.
        let _ = child.wait();

        let Some(killed_phase) = killed_phase else {
            // SP1 Finding A: e.g. a sub-20 ms BUILD never showed its window.
            // With the env bridge this should be rare; surface it as a timing
            // miss, never as a maw fault.
            return Ok(SubprocOutcome::PhaseNotObserved {
                target: self.plan.phase.clone(),
            });
        };

        // ---- bounded, self-healing recovery retry -------------------------
        match self.recover_with_retry() {
            Some(attempts) => Ok(SubprocOutcome::Recovered {
                killed_phase,
                recovery_attempts: attempts,
            }),
            None => Ok(SubprocOutcome::RecoveryExhausted {
                killed_phase,
                attempts: self.max_recovery_attempts,
            }),
        }
    }

    /// Bounded recovery retry: re-run `maw ws merge …` until it succeeds or
    /// the budget is exhausted, backing off between attempts.
    ///
    /// This is the recovery surface the oracle needs: post-crash, the same
    /// `maw ws merge` invocation IS the recovery path (it detects the
    /// orphaned `merge-state.json`, self-heals once the dead owner is
    /// observed, and either finishes the partial commit or aborts pre-commit
    /// — exactly `recovery_outcome_for_phase` in `maw-core`). SP1 Finding B:
    /// the first attempt right after a real kill is racy (owner pid not yet
    /// observed dead), so this MUST be a loop with seconds of backoff, not a
    /// single call. Returns the attempt count on success, `None` if exhausted.
    #[must_use]
    pub fn recover_with_retry(&self) -> Option<u32> {
        for attempt in 1..=self.max_recovery_attempts {
            let ok = Command::new(&self.maw_bin)
                .args(&self.merge_args)
                .current_dir(&self.repo_root)
                .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00 +0000")
                .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00 +0000")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if ok {
                return Some(attempt);
            }
            if attempt < self.max_recovery_attempts {
                std::thread::sleep(self.recovery_backoff);
            }
        }
        None
    }
}

/// Read the merge phase from a v2 repo's `merge-state.json`, if present.
///
/// Shared by both tiers' polling/oracle glue. Returns `None` if the file is
/// absent or malformed (a malformed/absent state file is itself a valid
/// observation, not an error).
#[must_use]
pub fn read_merge_phase(repo_root: &Path) -> Option<String> {
    let bytes = std::fs::read(repo_root.join(".manifold/merge-state.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("phase")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// Send a real `SIGKILL` to an entire process group (`kill -9 -<pgid>`).
///
/// `unsafe` is forbidden workspace-wide, so this shells out to `kill(1)`
/// rather than calling `libc::kill` via FFI. Negative pid ⇒ the whole group
/// (the detached merge plus any `git`/validation children it spawned),
/// matching the bn-cm63 chaos pattern. Best-effort: a failure to spawn `kill`
/// (or an already-dead group) is ignored — the subsequent `child.wait()` and
/// recovery loop are the real synchronisation points.
fn kill_process_group(pgid: u32) {
    let _ = Command::new("kill")
        .arg("-9")
        .arg(format!("-{pgid}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `from_seed` is deterministic: same seed ⇒ identical plan.
    #[test]
    fn plan_is_deterministic() {
        for seed in [0_u64, 1, 7, 42, 99, 12345] {
            let a = FaultPlan::from_seed(seed);
            let b = FaultPlan::from_seed(seed);
            assert_eq!(a, b, "seed {seed} must be replayable");
        }
    }

    /// Every selected failpoint is a real, known site in the canonical table.
    #[test]
    fn plan_targets_are_known_failpoints() {
        let known: std::collections::HashSet<&str> = CRASHABLE_BY_PHASE
            .iter()
            .flat_map(|(_, sites)| sites.iter().copied())
            .collect();
        for seed in 0..500_u64 {
            let p = FaultPlan::from_seed(seed);
            assert!(
                known.contains(p.failpoint.as_str()),
                "seed {seed} produced unknown failpoint {}",
                p.failpoint
            );
        }
    }

    /// A dangerous boundary is ALWAYS delivered as a real SIGKILL (SP1: those
    /// boundaries must be proven under a real kill, never a clean unwind).
    #[test]
    fn dangerous_sites_force_sigkill() {
        for seed in 0..1000_u64 {
            let p = FaultPlan::from_seed(seed);
            if DANGEROUS_FAILPOINTS.contains(&p.failpoint.as_str()) {
                assert_eq!(
                    p.kind,
                    CrashKind::SigKill,
                    "seed {seed} ({}) must SIGKILL",
                    p.failpoint
                );
            }
        }
    }

    /// The MAW_FP spec a plan emits round-trips through the production parser
    /// to exactly the (failpoint, action) the plan intends.
    #[test]
    fn spec_round_trips_through_parser() {
        for seed in 0..200_u64 {
            let p = FaultPlan::from_seed(seed);
            let parsed = failpoints::parse_env_spec(&p.maw_fp_spec());
            assert_eq!(parsed.len(), 1, "seed {seed} spec must be one pair");
            assert_eq!(parsed[0].0, p.failpoint);
            match (p.kind, &parsed[0].1) {
                (CrashKind::Panic, FailpointAction::Panic(_))
                | (
                    CrashKind::Unwind | CrashKind::SigKill,
                    FailpointAction::Error(_),
                ) => {}
                (k, a) => panic!("seed {seed}: kind {k:?} -> wrong action {a:?}"),
            }
        }
    }

    /// All phases are reachable across the seed space (the generator is not
    /// degenerate / stuck on one phase).
    #[test]
    fn all_phases_reachable() {
        let mut seen = std::collections::HashSet::new();
        for seed in 0..2000_u64 {
            seen.insert(FaultPlan::from_seed(seed).phase);
        }
        for (phase, _) in CRASHABLE_BY_PHASE {
            assert!(seen.contains(*phase), "phase {phase} never selected");
        }
    }

    /// `read_merge_phase` returns `None` for a missing file, never panics.
    #[test]
    fn read_phase_missing_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_merge_phase(dir.path()), None);
    }
}
