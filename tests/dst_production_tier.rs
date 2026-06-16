//! Production-code DST tier (bn-2byw step 2, increment 1).
//!
//! Drives maw's **real** workspace operations — via the actual `maw` binary
//! through [`TestRepo`] — over a deterministic, seed-generated op-sequence
//! produced by the shared `maw-scenario` generator, and runs the **authoritative
//! SG1 oracles** after every op:
//!
//! - **Oracle A** (`maw::assurance::oracle_a::OracleA`) — content-reachability
//!   no-work-lost. It accumulates witness blobs across steps (incremental) and
//!   fires `ReachabilityLost` if any previously-committed blob becomes
//!   unreachable. This is the load-bearing SG1 work-loss gate (SP2), NOT the
//!   demoted commit-ancestry proxy in `oracle::check_all`.
//! - **Oracle B** (`maw::assurance::oracle_b::check`) — state-coherence: dangling
//!   workspace head/owned refs and merge-state orphans (the bn-cm63 class). It
//!   reuses maw's PRODUCTION live-merge classification, so it understands maw's
//!   real ref shapes.
//!
//! These are the same oracles the in-proc soak (`maw-assurance::in_proc`) gates
//! on. Because they reason about CONTENT (blob reachability) and maw's real
//! refs, none of the demoted-proxy false-positives apply — so this tier needs
//! **no** snapshot relaxation: a violation here is a candidate REAL maw bug.
//!
//! # Why this exists
//!
//! The existing in-process soak (`maw-assurance::in_proc` + `tests/dst_harness.rs`'s
//! crash-simulation traces) drives a *plumbing model*: it writes merge-state
//! JSON directly and reasons about an abstract model. This tier instead spawns
//! the production `maw` binary for every op, so the oracles get statistical
//! coverage of the **real** CLI / merge-engine / workspace code path.
//!
//! Unlike the in-proc driver, this tier has REAL per-workspace git worktrees
//! (`TestRepo`), so `capture_state` reads real HEADs directly — we do NOT
//! override `state.workspaces` the way the in-proc driver must.
//!
//! # Determinism / scope
//!
//! - Uses the SAME `maw-scenario` generator and `ScenarioPlan` as the in-proc
//!   soak (`maw::assurance::scenario`). The generator carries a **gated**
//!   `Advance` op (`maw ws advance`): `ConditionProfile::advance_weight`
//!   defaults to 0, so the default-profile seed→plan byte stream — the bn-2yzz
//!   in-proc campaign — is UNCHANGED (verified by the maw-scenario determinism
//!   and corpus tests). This tier opts in via `with_advance_weight`, so it
//!   exercises the production `ws advance` HEAD-movement path (bn-8flz) that
//!   the in-proc model cannot reach.
//! - The default test (`dst_production_tier_no_work_lost`) replays only the op
//!   stream — each `PlannedStep.fault` / `.git_time` is ignored — and stays
//!   fast for the default gate. A SEPARATE `#[ignore]` variant
//!   (`dst_production_tier_survives_faults`, run via
//!   `just sg1-production-tier-faults`) DOES honor `PlannedStep.fault`: every
//!   step the generator marks with a `FaultSpec::Failpoint` is executed via a
//!   `--features failpoints` `maw` binary with `MAW_FP=<name>=abort`, crashing
//!   the op mid-flight, and the oracles must still hold on the post-crash repo
//!   (maw's merge-state recovery is what makes this true). A violation under
//!   faults is a candidate REAL maw recovery/work-loss bug.
//! - The oracle is the judge of correctness, NOT the maw exit code: we use
//!   `maw_raw_exact` (which does not panic on non-zero) and let the oracles
//!   decide whether an op broke an invariant. Many ops legitimately exit
//!   non-zero (e.g. a `git commit` with nothing staged, a merge of a workspace
//!   with no committed work) — that is expected and is not, by itself, a
//!   violation.
//!
//! # Running
//!
//! ```sh
//! cargo test --features assurance --test dst_production_tier -- --nocapture
//! # or: just sg1-production-tier
//! ```
//!
//! Knobs: `DST_TRACES` (seed count, default 16), `DST_STEPS` (steps per seed,
//! default 24). Deep runs are clean: e.g. `DST_TRACES=64 DST_STEPS=80` →
//! 5120 op-steps, 0 violations (the depth ceiling that needed bn-3g6o — Oracle
//! A recognizing content preserved inside conflict-marker rewrites — is fixed).

mod manifold_common;

#[cfg(feature = "assurance")]
use manifold_common::TestRepo;
#[cfg(feature = "assurance")]
use maw::assurance::oracle::capture_state as capture_oracle_state;
#[cfg(feature = "assurance")]
use maw::assurance::oracle_a::OracleA;
#[cfg(feature = "assurance")]
use maw::assurance::oracle_b;
#[cfg(feature = "assurance")]
use maw::assurance::scenario::{BaseRef, ConditionProfile, FaultSpec, Op, Target, generate_plan};

/// Read a `u64` count from `var`, defaulting to `default`.
#[cfg_attr(not(feature = "assurance"), allow(dead_code))]
fn env_count(var: &str, default: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Build (once per test process) and locate a `--features failpoints` `maw`
/// binary, returning its absolute path.
///
/// The default `manifold_common::maw_bin()` returns the plain binary, which has
/// no failpoint machinery (`MAW_FP` and every `fp!()` site are gated behind
/// `--features failpoints`). The faulted DST variant needs a binary that
/// actually honors `MAW_FP=<name>=abort` to crash mid-op, so we build the
/// failpoints variant into a *separate* target dir — we never clobber the plain
/// `target/<profile>/maw` the rest of the suite (and `just check`) rely on.
/// Memoized so repeated calls in one test process build at most once.
///
/// Mirrors `tests/flock_mutual_exclusion_bn_2byw.rs::failpoints_maw_bin`.
#[cfg(feature = "assurance")]
fn failpoints_maw_bin() -> &'static std::path::Path {
    use std::process::Command;
    use std::sync::OnceLock;

    static BIN: OnceLock<std::path::PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        // Dedicated target dir so the failpoints build does not overwrite the
        // plain binary used by the rest of the suite.
        let target_dir = manifest_dir.join("target").join("dst-prod-fp-bn-2byw");

        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "maw-cli",
                "--features",
                "failpoints",
                "--target-dir",
            ])
            .arg(&target_dir)
            .current_dir(&manifest_dir)
            .status()
            .expect("failed to spawn `cargo build` for the failpoints binary");
        assert!(
            status.success(),
            "`cargo build -p maw-cli --features failpoints` failed; cannot run \
             the faulted production-tier DST"
        );

        let bin = target_dir.join("debug").join("maw");
        assert!(
            bin.exists(),
            "failpoints maw binary not found at {} after build",
            bin.display()
        );
        bin
    })
    .as_path()
}

// ---------------------------------------------------------------------------
// Op → real maw command mapping
// ---------------------------------------------------------------------------

/// Counters tracking how much real work actually happened during a run, used
/// for the liveness guard (so a passing test is never vacuously green).
#[cfg(feature = "assurance")]
#[derive(Default)]
struct Liveness {
    /// Total ops attempted (== plan steps executed).
    ops_attempted: u64,
    /// Ops whose maw invocation exited 0.
    ops_succeeded: u64,
    /// `ws create` invocations that exited 0.
    ws_created: u64,
    /// Number of times the epoch ref advanced across the run.
    epoch_advances: u64,
    /// Total witness blobs Oracle A accumulated across all seeds. This is the
    /// definitive non-vacuity signal: if it is 0 the oracle never saw any
    /// committed content (e.g. `capture_state` failed to enumerate the real
    /// worktrees), so a green result would be meaningless.
    oracle_a_witnesses: u64,
    /// `ws advance` invocations that exited 0 — proves the production
    /// HEAD-movement advance path (bn-8flz) was actually exercised.
    advances_run: u64,
    /// Ops where a `FaultSpec::Failpoint` was armed and executed via the
    /// failpoints binary with `MAW_FP=<name>=abort`. The decisive non-vacuity
    /// signal for the faulted variant: if 0, no mid-op crash was ever injected,
    /// so the oracle never judged a post-crash repo and a green run is
    /// meaningless.
    faults_injected: u64,
}

/// Short human-readable name for an op (for oracle-violation context strings).
#[cfg(feature = "assurance")]
const fn op_name(op: &Op) -> &'static str {
    match op {
        Op::WsCreate { .. } => "ws_create",
        Op::EditFiles { .. } => "edit_files",
        Op::Commit { .. } => "commit",
        Op::Merge { .. } => "merge",
        Op::Sync { .. } => "sync",
        Op::Destroy { .. } => "destroy",
        Op::Recover { .. } => "recover",
        Op::Advance { .. } => "advance",
    }
}

/// Map `BaseRef` to a `--from` value.
///
/// `maw ws create --from` accepts a workspace/branch/revision but has NO
/// dedicated "epoch" keyword (verified via `maw ws create --help`). The repo's
/// epoch ref `refs/manifold/epoch/current` is at-or-ahead of `main`, but the
/// public CLI surface does not expose a stable name for it. So for increment 1
/// we map BOTH `Main` and `Epoch` to `"main"` — a real, always-resolvable base.
/// This is conservative: it never widens divergence, and the oracle still sees
/// real workspace creation either way.
#[cfg(feature = "assurance")]
const fn base_ref_arg(base: &BaseRef) -> &'static str {
    match base {
        BaseRef::Main | BaseRef::Epoch => "main",
    }
}

/// Execute one planned op against the real `maw` binary. Returns whether the
/// primary maw invocation exited 0 (for liveness accounting). The oracle — not
/// this return value — is the judge of correctness.
#[cfg(feature = "assurance")]
fn execute_op(repo: &TestRepo, op: &Op) -> bool {
    match op {
        Op::WsCreate { ws, from } => {
            // Create as --persistent so the `Advance` op (maw ws advance) has a
            // valid target: advance refuses non-persistent workspaces
            // (advance.rs "Only persistent workspaces can be advanced"). All
            // other ops (edit/commit/merge/sync/destroy/recover) work
            // identically on a persistent workspace.
            let out = repo.maw_raw_exact(&[
                "ws",
                "create",
                &ws.0,
                "--from",
                base_ref_arg(from),
                "--persistent",
            ]);
            out.status.success()
        }
        Op::EditFiles { ws, files } => {
            // Edits go straight to the workspace working tree (NOT through maw).
            // Only meaningful if the workspace actually exists on disk; the
            // generator can plan edits for a ws whose create failed (e.g. a
            // duplicate name), so guard the helper which would otherwise panic.
            if repo.workspace_exists(&ws.0) {
                for fe in files {
                    repo.add_file(&ws.0, &fe.path, &fe.content);
                }
            }
            // Editing is not a maw op; count it as a no-op for liveness.
            false
        }
        Op::Commit { ws, msg } => {
            // `git add -A` then `git commit -m <msg>` inside the workspace, via
            // `maw exec`. A commit with nothing staged exits non-zero — fine.
            let _ = repo.maw_raw_exact(&["exec", &ws.0, "--", "git", "add", "-A"]);
            let out = repo.maw_raw_exact(&["exec", &ws.0, "--", "git", "commit", "-m", &msg.0]);
            out.status.success()
        }
        Op::Merge {
            srcs,
            into,
            destroy,
        } => {
            // Always merge into default for increment 1.
            let _ = into; // Target is recorded for completeness; we pin `default`.
            let mut args: Vec<&str> = vec!["ws", "merge"];
            for src in srcs {
                args.push(&src.0);
            }
            args.push("--into");
            args.push(merge_target(into));
            // `maw ws merge` requires an explicit --message (it refuses to
            // read from a non-tty stdin), so supply a deterministic one. The
            // generator's Op::Merge carries no message, so this is purely a
            // mapping detail; the content does not affect what the oracle sees.
            args.push("--message");
            args.push("dst: production-tier merge");
            if *destroy {
                args.push("--destroy");
            }
            let out = repo.maw_raw_exact(&args);
            out.status.success()
        }
        Op::Sync { ws } => {
            let out = repo.maw_raw_exact(&["ws", "sync", &ws.0]);
            out.status.success()
        }
        Op::Destroy { ws, force } => {
            let mut args: Vec<&str> = vec!["ws", "destroy", &ws.0];
            if *force {
                args.push("--force");
            }
            let out = repo.maw_raw_exact(&args);
            out.status.success()
        }
        Op::Recover { ws, to } => {
            let out = repo.maw_raw_exact(&["ws", "recover", &ws.0, "--to", &to.0]);
            out.status.success()
        }
        Op::Advance { ws } => {
            // `maw ws advance <ws>` — routes committed-ahead work through the
            // guarded rebase path (bn-8flz). This is the production
            // HEAD-movement code the in-proc model can't reach.
            let out = repo.maw_raw_exact(&["ws", "advance", &ws.0]);
            out.status.success()
        }
    }
}

/// Execute one planned op while ARMING a `FaultSpec::Failpoint` as
/// `MAW_FP=<name>=abort` on the **failpoints** binary, so the op can crash
/// mid-flight. The generator only attaches faults to `Op::Merge` (any phase)
/// and `Op::Commit` (commit-phase sites), so only those two arms route through
/// the failpoints binary here; any other op (defensively) falls back to the
/// plain unfaulted path.
///
/// Returns `(succeeded, crashed)`:
/// - `succeeded` is `true` iff the maw invocation exited 0 (for liveness),
/// - `crashed` is `true` iff the process did not exit cleanly (non-zero or
///   killed by a signal — the realistic "mid-op kill"). A crash is EXPECTED,
///   not a failure: the post-crash repo state is what the oracle must judge.
///
/// The `FP_COMMIT_*` / `FP_BUILD_*_MERGE_COMPUTE` sites fire inside maw's
/// real merge engine (`maw::merge::commit` / `maw::merge::build_phase`, called
/// from `maw ws merge`), so a Merge op armed with any of them aborts mid-merge.
/// A commit-phase fault attached to an `Op::Commit` arms the same env on the
/// `git commit` shell-out; the `FP_COMMIT_*` sites are not on the `git commit`
/// path, so it typically will not crash there — that is fine (no crash, oracle
/// still runs), and matches the bn-18mv model where the armed env trips a later
/// merge.
#[cfg(feature = "assurance")]
fn execute_op_faulted(repo: &TestRepo, op: &Op, fp_name: &str) -> (bool, bool) {
    use std::process::Command;

    let bin = failpoints_maw_bin();
    let maw_fp = format!("{fp_name}=abort");

    // Build the argv for the op, matching `execute_op`'s mapping exactly.
    let run = |args: &[&str]| -> std::process::Output {
        Command::new(bin)
            .args(args)
            .current_dir(repo.root())
            .env("MAW_FP", &maw_fp)
            .output()
            .expect("failed to execute failpoints maw binary")
    };

    let out = match op {
        Op::Merge {
            srcs,
            into,
            destroy,
        } => {
            let mut args: Vec<&str> = vec!["ws", "merge"];
            for src in srcs {
                args.push(&src.0);
            }
            args.push("--into");
            args.push(merge_target(into));
            args.push("--message");
            args.push("dst: production-tier merge");
            if *destroy {
                args.push("--destroy");
            }
            run(&args)
        }
        Op::Commit { ws, msg } => {
            // `git add -A` is benign; only the commit carries the armed env.
            let _ = run(&["exec", &ws.0, "--", "git", "add", "-A"]);
            run(&["exec", &ws.0, "--", "git", "commit", "-m", &msg.0])
        }
        // The generator never attaches a fault to any other op; if that ever
        // changes, fall back to the unfaulted plain-binary path rather than
        // silently dropping the op.
        _ => return (execute_op(repo, op), false),
    };

    let succeeded = out.status.success();
    let crashed = !out.status.success();
    (succeeded, crashed)
}

/// Merge target name. Increment 1 always merges into `default`.
#[cfg(feature = "assurance")]
const fn merge_target(into: &Target) -> &'static str {
    // The generator only emits `Target::Default` (see maw-scenario try_emit),
    // but `Target::Change` would also route to `default` for increment 1 since
    // we deliberately pin a single, always-valid merge target here.
    match into {
        Target::Default | Target::Change(_) => "default",
    }
}

// ---------------------------------------------------------------------------
// Seeded run
// ---------------------------------------------------------------------------

/// Run a single seed's plan against a fresh real repo, returning oracle
/// violations and (via `live`) liveness accounting.
///
/// After each op we capture the real repo state (`capture_state` reads the
/// real per-ws worktree HEADs) and run BOTH authoritative oracles:
/// - Oracle A is INCREMENTAL — `check_step(state, step_index)` accumulates
///   witness blobs across steps, so it must be called once per step, in order,
///   with the running `step_index`. A `StepReport.violation` is a real
///   work-loss signal; an `Err(_)` is a plumbing failure (reported as
///   inconclusive, never silently swallowed).
/// - Oracle B is STATELESS — `oracle_b::check(root)` returns a `Vec` of
///   state-coherence violations (dangling head/owned refs, merge-state orphans
///   — the bn-cm63 class). Non-empty == violation.
///
/// `inject_faults`: when `true`, any step carrying a `FaultSpec::Failpoint` is
/// executed via the **failpoints** binary with `MAW_FP=<name>=abort`, crashing
/// the op mid-flight (the realistic "mid-op kill"). The oracle then judges the
/// post-crash repo: maw's merge-state recovery is what must keep it coherent
/// and lose no committed work. Unfaulted ops always take the plain-binary path,
/// so the failpoints binary is never paid for ops that carry no fault.
#[cfg(feature = "assurance")]
fn run_seed(seed: u64, n_steps: usize, inject_faults: bool, live: &mut Liveness) -> Vec<String> {
    let mut violations = Vec::new();

    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base content\n")]);

    // Enable the Advance op (weight 8, comparable to the other op weights) so
    // this tier exercises the production `ws advance` HEAD-movement path. The
    // DEFAULT profile keeps advance_weight=0, so the bn-2yzz in-proc campaign's
    // seed→plan stream is unaffected (proven by the maw-scenario determinism +
    // corpus tests). This tier regenerates plans each run, so a different
    // byte stream here is fine.
    let plan = generate_plan(
        seed,
        &ConditionProfile::default().with_advance_weight(8),
        n_steps,
    );

    // Oracle A is incremental: ONE instance per seed/repo, fed every step.
    let mut oracle_a = OracleA::new(repo.root());
    let mut last_epoch = repo.current_epoch();

    for (i, step) in plan.steps.iter().enumerate() {
        let op = &step.op;
        let name = op_name(op);

        live.ops_attempted += 1;
        // Decide whether this step is faulted. Faults only attach to Merge/Commit
        // ops (generator invariant), and only when fault injection is enabled.
        let fault_name = if inject_faults {
            match &step.fault {
                FaultSpec::Failpoint { name, .. } => Some(name.as_str()),
                FaultSpec::None => None,
            }
        } else {
            None
        };

        let succeeded = if let Some(fp_name) = fault_name {
            // Arm MAW_FP=<name>=abort on the failpoints binary; the op will
            // likely crash mid-flight. That is EXPECTED — the oracle judges the
            // post-crash state below.
            live.faults_injected += 1;
            let (ok, _crashed) = execute_op_faulted(&repo, op, fp_name);
            ok
        } else {
            execute_op(&repo, op)
        };
        if succeeded {
            live.ops_succeeded += 1;
        }
        if matches!(op, Op::WsCreate { .. }) && succeeded {
            live.ws_created += 1;
        }
        if matches!(op, Op::Advance { .. }) && succeeded {
            live.advances_run += 1;
        }

        // Detect epoch advance (a merge committing into default).
        let now_epoch = repo.current_epoch();
        if now_epoch != last_epoch {
            live.epoch_advances += 1;
            last_epoch = now_epoch;
        }

        // Capture the REAL post-op state (real worktree HEADs).
        let state = match capture_oracle_state(repo.root()) {
            Ok(s) => s,
            Err(err) => {
                violations.push(format!(
                    "seed={seed} step={i} op={name}: capture_state failed (plumbing): {err}"
                ));
                continue;
            }
        };

        // Oracle A (incremental content-reachability no-work-lost).
        match oracle_a.check_step(&state, i) {
            Ok(report) => {
                if let Some(v) = report.violation {
                    violations.push(format!("seed={seed} step={i} op={name} OracleA: {v}"));
                }
            }
            Err(err) => {
                // A plumbing error (e.g. git rev-list failed), NOT a clean
                // pass. Report it so the run is never vacuously green.
                violations.push(format!(
                    "seed={seed} step={i} op={name} OracleA plumbing error: {err}"
                ));
            }
        }

        // Oracle B (stateless state-coherence).
        for v in oracle_b::check(repo.root()) {
            violations.push(format!("seed={seed} step={i} op={name} OracleB: {v:?}"));
        }
    }

    // Record how much content Oracle A actually witnessed this seed (the
    // non-vacuity signal — accumulated across seeds by the caller).
    live.oracle_a_witnesses = live
        .oracle_a_witnesses
        .saturating_add(oracle_a.witness_count() as u64);

    violations
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

/// Drive the production-code DST tier over `count` seeds × `n_steps` steps,
/// running both authoritative oracles after every op. Shared by the fast
/// unfaulted default test and the heavyweight faulted variant; `inject_faults`
/// selects whether `FaultSpec::Failpoint` steps are armed with
/// `MAW_FP=<name>=abort` on the failpoints binary.
///
/// Returns the accumulated `(Liveness, all_violations, failing_seeds)` so the
/// caller can apply variant-specific guards (e.g. `faults_injected > 0`) and
/// the final violation assertion.
#[cfg(feature = "assurance")]
fn drive_tier(
    label: &str,
    count: u64,
    n_steps: usize,
    inject_faults: bool,
) -> (Liveness, Vec<String>, Vec<u64>) {
    let mut live = Liveness::default();
    let mut all_violations: Vec<String> = Vec::new();
    let mut failing_seeds: Vec<u64> = Vec::new();

    for seed in 0..count {
        let v = run_seed(seed, n_steps, inject_faults, &mut live);
        if !v.is_empty() {
            failing_seeds.push(seed);
            for line in &v {
                all_violations.push(format!("[seed={seed}] {line}"));
            }
        }
    }

    // Accrual + statistical reporting (mirrors the bn-2yzz in-proc floor's
    // rule). Each op-step is one oracle TRIAL (capture_state + Oracle A
    // check_step + Oracle B check). With X=0 observed violations over N
    // trials, the one-sided Wilson 95% upper bound on the per-op-step
    // violation rate is ≈ z²/N = 3.8416/N (z=1.96; the X=0 closed form). This
    // is the SAME discipline the SG1 soak campaign publishes (every "0/N" cell
    // reports its Wilson UB). Raise DST_TRACES / DST_STEPS to accrue toward a
    // production-code op-step floor; this tier is the production-code analog of
    // the in-proc volume soak, so its evidence is reported the same way.
    let n_trials = live.ops_attempted;
    // n_trials is a trial COUNT; the f64 widening for the Wilson 95% bound is
    // exact for any realistic soak volume and harmless for a confidence bound
    // (matches the in-proc soak's own cast_precision_loss allow).
    #[allow(clippy::cast_precision_loss)]
    let wilson_ub = if n_trials > 0 {
        3.8416_f64 / n_trials as f64
    } else {
        1.0
    };
    eprintln!(
        "{label}: ran {} op-steps across {count} seeds ({} steps/seed); \
         {} ops succeeded, {} workspaces created, {} epoch advances, \
         {} Oracle-A witness blobs, {} ws-advances, {} faults injected; \
         {} violations over N={} trials \
         (Wilson 95% UB on per-op-step violation rate = {:.3e})",
        live.ops_attempted,
        n_steps,
        live.ops_succeeded,
        live.ws_created,
        live.epoch_advances,
        live.oracle_a_witnesses,
        live.advances_run,
        live.faults_injected,
        all_violations.len(),
        n_trials,
        wilson_ub,
    );

    (live, all_violations, failing_seeds)
}

/// Apply the liveness guards shared by both variants (workspaces created, epoch
/// advanced, Oracle A witnessed content, advance path exercised).
#[cfg(feature = "assurance")]
fn assert_shared_liveness(live: &Liveness, count: u64, n_steps: usize) {
    // ----- Liveness guard: the test must not be vacuously green. -----
    // If essentially nothing happened, the Op→CLI mapping is broken and this
    // would be a false pass — fail loudly so it gets fixed, not papered over.
    assert!(
        live.ws_created > 0,
        "LIVENESS FAILURE: zero workspaces were created across {count} seeds \
         ({} op-steps). The Op->CLI mapping is wrong (maw ws create never \
         succeeded), so the oracle never saw real production state. Fix the \
         mapping rather than trusting this as a pass.",
        live.ops_attempted,
    );
    assert!(
        live.epoch_advances > 0,
        "LIVENESS FAILURE: the epoch never advanced across {count} seeds \
         ({} op-steps, {} ws created). No merge committed into default, so the \
         no-work-lost oracle was never exercised against a real epoch bump. \
         Fix the merge mapping (or raise DST_STEPS) rather than trusting this \
         as a pass.",
        live.ops_attempted,
        live.ws_created,
    );
    // The decisive non-vacuity guard: Oracle A must have actually witnessed
    // committed content. If it saw zero blobs, `capture_state` did not observe
    // the real worktrees and the oracle was never exercised — a green result
    // would be meaningless.
    assert!(
        live.oracle_a_witnesses > 0,
        "LIVENESS FAILURE: Oracle A accumulated ZERO witness blobs across \
         {count} seeds ({} ops succeeded, {} ws created, {} epoch advances). \
         The oracle never saw committed content — capture_state likely did not \
         enumerate the real worktrees — so 'no violations' is vacuous. \
         Investigate capture_state / layout before trusting this as a pass.",
        live.ops_succeeded,
        live.ws_created,
        live.epoch_advances,
    );

    // Non-vacuity for the Advance path: with advance_weight>0 enabled, the
    // production `ws advance` HEAD-movement code (bn-8flz) must actually have
    // run at least once — otherwise this tier silently stops covering it.
    assert!(
        live.advances_run > 0,
        "LIVENESS FAILURE: zero successful `ws advance` ops across {count} seeds \
         ({n_steps} steps/seed). The Advance op was enabled (advance_weight>0) but never \
         executed against a persistent committed-ahead workspace — the production \
         advance/rebase path was not exercised. Raise DST_STEPS or check the \
         WsCreate --persistent mapping.",
    );
}

/// Production-code DST tier: drive real `maw` over seed-generated op streams
/// and assert the SG1 oracles hold after every op.
#[cfg(feature = "assurance")]
#[test]
fn dst_production_tier_no_work_lost() {
    let count = env_count("DST_TRACES", 16);
    // Default 24 steps/seed: long enough that a healthy fraction of seeds reach
    // Edit -> Commit -> Merge and actually advance the epoch (so the
    // epoch-advance liveness guard is comfortably satisfied, not marginal),
    // while keeping the default run well under a minute. Soak campaigns raise
    // both knobs via DST_TRACES / DST_STEPS.
    let n_steps = usize::try_from(env_count("DST_STEPS", 24)).expect("DST_STEPS fits usize");

    let (live, all_violations, failing_seeds) =
        drive_tier("dst-production-tier", count, n_steps, false);

    assert_shared_liveness(&live, count, n_steps);

    assert!(
        all_violations.is_empty(),
        "Oracle violations across {} failing seed(s) {:?}:\n{}",
        failing_seeds.len(),
        failing_seeds,
        all_violations.join("\n"),
    );
}

/// FAULTED production-code DST tier: same op streams, but every step the
/// generator marks with a `FaultSpec::Failpoint` is executed via the
/// **failpoints** binary with `MAW_FP=<name>=abort`, crashing the op mid-flight
/// (the realistic "mid-op kill"). After EVERY op — crashed or not — both
/// authoritative oracles judge the on-disk repo. The load-bearing assertion:
/// even after an abort mid-merge/mid-commit, maw's merge-state recovery keeps
/// the repo coherent and loses no committed work (the oracle is the judge).
///
/// `#[ignore]` because it builds a `--features failpoints` `maw` binary
/// (~minutes cold) and runs every faulted op as a separate crashing process —
/// far heavier than the default tier. The fast unfaulted
/// `dst_production_tier_no_work_lost` stays the default-gate test; this variant
/// runs on demand via `just sg1-production-tier-faults` (`--ignored`).
///
/// A real oracle violation here is a candidate REAL maw recovery/work-loss bug:
/// the test is left RED with the seed + op + fault + violation, never suppressed.
///
/// bn-38vw RESOLVED: previously this reproduced a real finding — under
/// `FP_COMMIT_BETWEEN_CAS_OPS=abort` mid-merge, Oracle A (no-work-lost) stayed
/// GREEN but Oracle B fired `MergeStateBadEpoch` (epoch advanced past the
/// point-of-no-return before `epoch_after` was journaled). The fix records
/// `epoch_after` into the merge-state journal BEFORE the ref-advancing CAS, so
/// the journal is coherent at every post-build crash point. This test now
/// PASSES; it remains `#[ignore]` solely for its weight (see above).
#[cfg(feature = "assurance")]
#[test]
#[ignore = "heavyweight: builds a --features failpoints maw binary and runs every faulted op as a separate crashing process. Run via just sg1-production-tier-faults"]
fn dst_production_tier_survives_faults() {
    let count = env_count("DST_TRACES", 16);
    // Same 24-step window as the unfaulted `dst_production_tier_no_work_lost`
    // test, which is GREEN at this budget. Pinning the same window means any
    // violation this variant surfaces is attributable to the INJECTED FAULTS,
    // not to a fault-independent issue that only appears at deeper step counts.
    // (At 16 seeds × 24 steps the default profile arms ~10 Merge/Commit faults —
    // comfortably above the `faults_injected > 0` non-vacuity guard.) Soak
    // campaigns raise both knobs via DST_TRACES / DST_STEPS.
    let n_steps = usize::try_from(env_count("DST_STEPS", 24)).expect("DST_STEPS fits usize");

    let (live, all_violations, failing_seeds) =
        drive_tier("dst-production-tier-faults", count, n_steps, true);

    assert_shared_liveness(&live, count, n_steps);

    // Non-vacuity for the WHOLE POINT of this variant: faults must actually have
    // been injected. With the default profile's mid_op_kill_prob=0.15, faults
    // attach to a healthy fraction of Merge/Commit ops over enough steps; if
    // none fired, this variant degenerates into the unfaulted tier and "no
    // violations" says nothing about recovery under crashes.
    assert!(
        live.faults_injected > 0,
        "LIVENESS FAILURE: ZERO faults were injected across {count} seeds \
         ({n_steps} steps/seed). The default profile arms faults on Merge/Commit \
         ops at mid_op_kill_prob=0.15, so over enough steps at least one should \
         fire. Raise DST_STEPS / DST_TRACES (more Merge/Commit ops) — a faulted \
         tier that injects no faults is vacuously green.",
    );

    // A violation here under faults = a candidate REAL maw recovery/work-loss
    // bug. Leave it RED with full detail; do NOT suppress.
    assert!(
        all_violations.is_empty(),
        "ORACLE VIOLATION UNDER FAULT INJECTION across {} failing seed(s) {:?} \
         — candidate REAL maw recovery/work-loss bug (post-crash state failed \
         the oracle). DO NOT suppress; investigate the seed + op + fault:\n{}",
        failing_seeds.len(),
        failing_seeds,
        all_violations.join("\n"),
    );
}
