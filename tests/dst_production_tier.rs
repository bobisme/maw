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
//!   soak (`maw::assurance::scenario`). **No change** is made to that crate.
//! - Increment 1 does NOT inject faults: each `PlannedStep.fault` / `.git_time`
//!   is ignored. We replay only the op stream.
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
//! default 24).

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
use maw::assurance::scenario::{BaseRef, ConditionProfile, Op, Target, generate_plan};

/// Read a `u64` count from `var`, defaulting to `default`.
#[cfg_attr(not(feature = "assurance"), allow(dead_code))]
fn env_count(var: &str, default: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
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
}

/// Short human-readable name for an op (for oracle-violation context strings).
#[cfg(feature = "assurance")]
fn op_name(op: &Op) -> &'static str {
    match op {
        Op::WsCreate { .. } => "ws_create",
        Op::EditFiles { .. } => "edit_files",
        Op::Commit { .. } => "commit",
        Op::Merge { .. } => "merge",
        Op::Sync { .. } => "sync",
        Op::Destroy { .. } => "destroy",
        Op::Recover { .. } => "recover",
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
            let out = repo.maw_raw_exact(&["ws", "create", &ws.0, "--from", base_ref_arg(from)]);
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
    }
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
#[cfg(feature = "assurance")]
fn run_seed(seed: u64, n_steps: usize, live: &mut Liveness) -> Vec<String> {
    let mut violations = Vec::new();

    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base content\n")]);

    let plan = generate_plan(seed, &ConditionProfile::default(), n_steps);

    // Oracle A is incremental: ONE instance per seed/repo, fed every step.
    let mut oracle_a = OracleA::new(repo.root());
    let mut last_epoch = repo.current_epoch();

    for (i, step) in plan.steps.iter().enumerate() {
        let op = &step.op;
        let name = op_name(op);

        live.ops_attempted += 1;
        let succeeded = execute_op(&repo, op);
        if succeeded {
            live.ops_succeeded += 1;
        }
        if matches!(op, Op::WsCreate { .. }) && succeeded {
            live.ws_created += 1;
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

    let mut live = Liveness::default();
    let mut all_violations: Vec<String> = Vec::new();
    let mut failing_seeds: Vec<u64> = Vec::new();

    for seed in 0..count {
        let v = run_seed(seed, n_steps, &mut live);
        if !v.is_empty() {
            failing_seeds.push(seed);
            for line in &v {
                all_violations.push(format!("[seed={seed}] {line}"));
            }
        }
    }

    eprintln!(
        "dst-production-tier: ran {} op-steps across {count} seeds ({} steps/seed); \
         {} ops succeeded, {} workspaces created, {} epoch advances, \
         {} Oracle-A witness blobs",
        live.ops_attempted,
        n_steps,
        live.ops_succeeded,
        live.ws_created,
        live.epoch_advances,
        live.oracle_a_witnesses,
    );

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

    assert!(
        all_violations.is_empty(),
        "Oracle violations across {} failing seed(s) {:?}:\n{}",
        failing_seeds.len(),
        failing_seeds,
        all_violations.join("\n"),
    );
}
