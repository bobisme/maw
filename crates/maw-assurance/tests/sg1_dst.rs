//! SG1 DST harness — bounded per-commit + nightly soak (T1.7, bn-1gp4).
//!
//! This is the CI-facing harness for the SG1 hard release gate
//! (`notes/sg1-dst-architecture.md` §7). It consumes the
//! `maw_assurance::{scenario, in_proc, shrinker}` substrate (T1.2–T1.6)
//! end-to-end and produces oracle verdicts deterministically per seed.
//!
//! It deliberately lives **alongside** the legacy `tests/workflow_dst.rs`
//! and `tests/action_workflow_dst.rs` harnesses (which predate the
//! `ScenarioPlan` substrate and use ad-hoc prefix-minimisation). They
//! continue to run via `just sim-run`; this harness runs via
//! `just sg1-per-commit` (PR + push) and `just sg1-nightly` (cron).
//!
//! ## Tests in this file
//!
//! - [`sg1_per_commit_corpus`] — replay every entry in
//!   `tests/corpus/dst/` that fits the `ScenarioPlan` schema. Hard-fails
//!   CI on any oracle violation. Always runs.
//! - [`sg1_per_commit_random_budget`] — generate a small fixed-seed-budget
//!   of `ScenarioPlan`s and drive each through the in-proc tier. Budget
//!   tuned for the §7 per-commit wall-clock cap (≤ 8 min). Always runs.
//! - [`sg1_nightly_soak`] — large seed budget (`SG1_NIGHTLY_SEEDS`,
//!   default `100_000`). Marked `#[ignore]` so it only runs when CI
//!   passes `-- --ignored`. Failing seeds auto-shrink and a minimal
//!   bundle uploads via `DST_ARTIFACT_DIR`.
//!
//! ## Determinism
//!
//! In-proc tier ⇒ bit-exact replay per seed (`PlannedStep::git_time` pins
//! `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE`; see
//! `crates/maw-assurance/src/in_proc.rs`).
//!
//! ## Gate semantics
//!
//! ANY oracle violation = red CI = release-blocking for v1.0
//! (`notes/sg1-dst-architecture.md` §7 acceptance gate).
//!
//! ## Planted-violation smoke test
//!
//! Set `SG1_PLANT_VIOLATION=1` to force a guaranteed Oracle A planted
//! defect into `sg1_per_commit_random_budget`. The harness then asserts
//! the plant TRIPS (i.e. that a "clean" run with the plant set turns
//! red). This is the CI sanity check used in `just sg1-per-commit-smoke`.
//!
//! Set `SG1_PLANT_AND_FAIL=1` to plant the same defect WITHOUT inverting
//! the assertion — i.e. behave like a real CI run with a regression
//! present. The test then fails normally (exit 101 from cargo test),
//! which is how T1.7 acceptance criterion §5 verifies "a planted-
//! violation seed turns the run red".
//!
//! ## Infrastructure failures (bn-30v6e)
//!
//! A seed that dies because the HOST ran out of disk quota, disk space or
//! file descriptors (EDQUOT/ENOSPC/EMFILE/ENFILE, see
//! `maw_assurance::infra`) is not an oracle verdict. The random-budget and
//! nightly-soak tests catch such a failure, print ONE marker line
//! `[sg1] INFRA-FAILURE: <reason>` and exit the process with code 75
//! (`EX_TEMPFAIL`). `scripts/sg1-soak/slot.sh` records such a slot as
//! `infra` (no op-steps accrued, no violation) instead of halting.
//!
//! Fail closed:
//! - only errors the classifier positively matches are infra; any other
//!   panic still fails the test (exit 101) as before;
//! - an oracle violation seen earlier in the same run always wins: the
//!   run then fails with the violations, never exits 75;
//! - infra never counts a seed as clean.
//!
//! ## Dirty-trunk tier (bn-1h9ue)
//!
//! Every driver in this binary runs the in-proc dirty-trunk tier: a real
//! default worktree whose target update after each merge is the PRODUCTION
//! `maw_cli::workspace::update_default_workspace`, run in a self-exec'd
//! helper ([`sg1_trunk_update_helper`]) so crash windows really `abort()` and
//! the update's output reaches the displacement oracle. Random seeds use
//! [`ConditionProfile::sg1_soak`] (rich `DirtyTrunkWrite`s, merges that change
//! modes and entry types, crashes inside the target update). Per step the
//! `TrunkDirtyPreservation`, `TrunkDirtyDisplacement` and
//! `TrunkReplayFaithfulness` oracles judge the trunk.
//!
//! Test hooks (used by `sg1_infra_exit_contract`): `SG1_SIMULATE_INFRA_AT=<i>`
//! raises a simulated EDQUOT at seed index `i`; `SG1_SIMULATE_PANIC_AT=<i>`
//! raises an ordinary (non-infra) panic there. Neither can produce a clean
//! seed, so they cannot inflate soak accrual.

#![cfg(feature = "oracles")]
#![allow(
    clippy::unwrap_used,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::needless_pass_by_value,
    clippy::case_sensitive_file_extension_comparisons,
    clippy::manual_let_else,
    clippy::option_if_let_else,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::too_long_first_doc_paragraph,
    clippy::items_after_statements,
    clippy::single_match_else,
    clippy::if_then_some_else_none,
    clippy::manual_is_multiple_of
)]

use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use maw_assurance::in_proc::{
    DriveStats, HarnessErrorClass, InProcDriver, OracleAClass, OracleBClass, PlantedDefect,
    StepVerdict,
};
use maw_assurance::infra::{self, INFRA_EXIT_CODE, INFRA_MARKER, InfraFailure};
use maw_assurance::scenario::{
    CANONICAL_BN_CM63_SEED, ConditionProfile, DefaultScenarioGenerator, ScenarioGenerator,
    generate_plan,
};
use maw_assurance::shrinker::{ShrinkReport, ShrinkerCorpusEntry, shrink};
use maw_assurance::trunk::{self, SelfExecUpdater};

/// The in-proc soak's profile (bn-1h9ue): default knobs + the dirty-trunk
/// soak. Every random seed of this binary is generated with it.
fn soak_profile() -> ConditionProfile {
    ConditionProfile::sg1_soak()
}

/// Name of the self-exec'd helper test (see [`sg1_trunk_update_helper`]).
const TRUNK_HELPER_TEST: &str = "sg1_trunk_update_helper";

/// Install the process-wide production target updater (idempotent).
fn install_trunk_tier() {
    let exe = std::env::current_exe().expect("current_exe");
    let _ = trunk::install_trunk_updater(Arc::new(SelfExecUpdater {
        exe,
        test_name: TRUNK_HELPER_TEST.to_owned(),
    }));
}

/// bn-1h9ue: helper process of the dirty-trunk tier. The in-proc driver
/// re-executes THIS binary with `--exact sg1_trunk_update_helper --ignored`
/// and a JSON request in `SG1_TRUNK_UPDATE_REQUEST`; the helper arms `MAW_FP`
/// and runs the production target update exactly as `maw ws merge`'s CLEANUP
/// phase does. Without the env var (a plain `-- --ignored` run) it is a no-op.
#[test]
#[ignore = "helper process for the dirty-trunk tier (self-exec'd by the in-proc driver)"]
fn sg1_trunk_update_helper() {
    trunk::run_trunk_update_helper(|req| {
        maw_cli::workspace::update_default_workspace(
            &req.default_ws_path,
            "default",
            &req.branch,
            &req.epoch_before,
            &req.epoch_after,
            None,
            &req.repo_root,
            true,
            true,
            &req.sources,
        )
        .map_err(|e| format!("{e:#}"))
    });
}

// ---------------------------------------------------------------------------
// Tunables (overridable via env vars in CI)
// ---------------------------------------------------------------------------

/// Default per-commit seed budget. Sized for the §7 wall-clock cap:
/// in-proc tier clocks ~42 ms / 32-step plan, so 64 seeds ≈ 2.7 s of
/// pure driver time; ~1× overhead for shrinker re-runs on the (rare)
/// failing seed; ample headroom under the 8-minute cap.
const PER_COMMIT_SEEDS_DEFAULT: u64 = 64;

/// Default per-commit plan length. Generator's `DEFAULT_PLAN_STEPS` is
/// 32; we let CI override via `SG1_PER_COMMIT_STEPS` to grow coverage
/// without growing the seed axis.
const PER_COMMIT_STEPS_DEFAULT: usize = 32;

/// Default nightly seed budget. Sized for a 100k-seed soak in
/// ~70 minutes wall-clock at ~42 ms/seed (single-threaded; the nightly
/// runner pays this once). CI override: `SG1_NIGHTLY_SEEDS`.
const NIGHTLY_SEEDS_DEFAULT: u64 = 100_000;

/// Default nightly plan length. Longer than per-commit so soak reaches
/// deeper interleavings; overrideable via `SG1_NIGHTLY_STEPS`.
const NIGHTLY_STEPS_DEFAULT: usize = 64;

fn per_commit_seeds() -> u64 {
    env_u64("SG1_PER_COMMIT_SEEDS", PER_COMMIT_SEEDS_DEFAULT)
}
fn per_commit_steps() -> usize {
    env_usize("SG1_PER_COMMIT_STEPS", PER_COMMIT_STEPS_DEFAULT)
}
fn per_commit_wall_cap() -> Duration {
    Duration::from_secs(env_u64("SG1_PER_COMMIT_WALL_CAP_SECS", 8 * 60))
}
fn nightly_seeds() -> u64 {
    env_u64("SG1_NIGHTLY_SEEDS", NIGHTLY_SEEDS_DEFAULT)
}
fn nightly_steps() -> usize {
    env_usize("SG1_NIGHTLY_STEPS", NIGHTLY_STEPS_DEFAULT)
}
fn base_seed() -> u64 {
    env_u64("SG1_BASE_SEED", DEFAULT_BASE_SEED)
}

/// Compile-time constant: the per-commit base seed. Picked so the
/// per-commit budget seeds (`base..base+N`) form an unrelated slice
/// from `CANONICAL_BN_CM63_SEED` (which is `1`) and from any seeds the
/// legacy `workflow_dst.rs` harness uses
/// (`BASE_SEED = 0x5EED_CAFE_7000_0001`). Tagged with the readable
/// nibble pattern `5_DST_` so a reader can recognise it in logs.
const DEFAULT_BASE_SEED: u64 = 0x5D57_BA5E_0000_0001;
fn single_seed() -> Option<u64> {
    std::env::var("SG1_SEED").ok().and_then(|v| v.parse().ok())
}
fn plant_violation() -> bool {
    env_bool("SG1_PLANT_VIOLATION") || env_bool("SG1_PLANT_AND_FAIL")
}

/// `SG1_PLANT_AND_FAIL=1` runs in "demonstration of red CI" mode: it
/// plants the same Oracle A `WorkLoss` defect as
/// `SG1_PLANT_VIOLATION=1`, BUT it does NOT invert the assertion. The
/// harness fails normally on the planted violation, exactly as a real
/// regression would. This is the workflow T1.7 acceptance criterion §5
/// requires ("verify a planted-violation seed turns the run red").
///
/// In contrast, `SG1_PLANT_VIOLATION=1` (alone) inverts the assertion —
/// it's the CI self-test that proves the gate IS wired correctly. Both
/// modes plant the same defect; they differ only in the final assert.
/// Generator workspace slots the planted-violation smoke plants on.
const PLANT_SLOTS: usize = 8;

fn plant_and_fail() -> bool {
    env_bool("SG1_PLANT_AND_FAIL")
}

fn env_bool(key: &str) -> bool {
    std::env::var(key).is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Corpus loader
// ---------------------------------------------------------------------------

/// Path to `tests/corpus/dst/` resolved relative to the workspace root.
///
/// `CARGO_MANIFEST_DIR` points at `crates/maw-assurance/`; the corpus
/// lives at `<workspace_root>/tests/corpus/dst/`. We walk up one
/// directory level from `crates/maw-assurance/` to reach the workspace
/// root, then descend into `tests/corpus/dst/`.
fn corpus_dir() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // crates/maw-assurance/ → crates/ → workspace_root
    let workspace_root = manifest
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR has at least two ancestors");
    workspace_root.join("tests").join("corpus").join("dst")
}

/// A corpus entry the SG1 harness can replay.
///
/// We accept TWO schemas under `tests/corpus/dst/*.json`:
///
/// 1. **ScenarioPlan corpus** (the format `ShrinkerCorpusEntry` writes,
///    populated by T1.8 bn-3ryq). Replayed by re-running its `plan` +
///    `planted` defects through `InProcDriver` and asserting the
///    `expected` verdict ("pass" ⇒ Clean; "known_violation" ⇒ matches
///    `description`).
/// 2. **Legacy schema** (`sample-g1-commit-crash.json`): a `seed` +
///    `crash_phase` + bookkeeping fields, no `plan` field. The legacy
///    schema is not driveable by the in-proc tier (it parameterises the
///    pre-`ScenarioPlan` harness in `tests/dst_harness.rs`). The SG1
///    harness logs it as `Skipped` so the per-commit job stays green
///    until T1.8 migrates the entry; the legacy `dst_harness.rs` still
///    exercises it via `just dst-fast`.
enum CorpusEntry {
    ScenarioPlan {
        path: PathBuf,
        entry: Box<ShrinkerCorpusEntry>,
    },
    Legacy {
        path: PathBuf,
        seed: u64,
        description: String,
    },
}

fn load_corpus() -> Vec<CorpusEntry> {
    let dir = corpus_dir();
    let Ok(read) = fs::read_dir(&dir) else {
        eprintln!("[sg1] corpus dir not found: {}", dir.display());
        return Vec::new();
    };
    let mut entries = Vec::new();
    for ent in read.flatten() {
        let p = ent.path();
        if !p.is_file() {
            continue;
        }
        if p.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let body = match fs::read_to_string(&p) {
            Ok(b) => b,
            Err(err) => {
                eprintln!("[sg1] corpus skip {}: read error: {err}", p.display());
                continue;
            }
        };
        // Try ScenarioPlan-shape first.
        if let Ok(entry) = serde_json::from_str::<ShrinkerCorpusEntry>(&body) {
            entries.push(CorpusEntry::ScenarioPlan {
                path: p,
                entry: Box::new(entry),
            });
            continue;
        }
        // Fall back to legacy schema (seed + crash_phase).
        #[derive(serde::Deserialize)]
        struct Legacy {
            seed: u64,
            #[serde(default)]
            description: String,
        }
        if let Ok(legacy) = serde_json::from_str::<Legacy>(&body) {
            entries.push(CorpusEntry::Legacy {
                path: p,
                seed: legacy.seed,
                description: legacy.description,
            });
            continue;
        }
        eprintln!("[sg1] corpus skip {}: unknown schema", p.display());
    }
    entries.sort_by(|a, b| corpus_path(a).cmp(corpus_path(b)));
    entries
}

fn corpus_path(e: &CorpusEntry) -> &Path {
    match e {
        CorpusEntry::ScenarioPlan { path, .. } | CorpusEntry::Legacy { path, .. } => path,
    }
}

// ---------------------------------------------------------------------------
// Per-seed driver wrapper
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct SeedOutcome {
    steps: usize,
    verdict: StepVerdict,
    elapsed: Duration,
    /// bn-25pac evidence counters (what the oracles actually judged).
    stats: DriveStats,
}

/// One summary fragment with the bn-25pac evidence totals, appended to the
/// `[sg1] ... end:` lines so soak ledgers can record them.
fn evidence_summary(totals: &DriveStats, harness_errors: usize) -> String {
    format!(
        "oracle_a_checks={} oracle_b_checks={} witnesses={} workspaces_observed={} \
         commits_observed={} trunk_writes={} trunk_updates={} trunk_crashes={} \
         dirty_trunk_merges={} displacement_checks={} replay_judgements={} replay_checks={} \
         trunk_drains={} harness_errors={harness_errors}",
        totals.oracle_a_checks,
        totals.oracle_b_checks,
        totals.witnesses,
        totals.workspaces_observed,
        totals.commits_observed,
        totals.trunk_writes,
        totals.trunk_updates,
        totals.trunk_crashes,
        totals.dirty_trunk_merges,
        totals.displacement_checks,
        totals.replay_judgements,
        totals.replay_checks,
        totals.trunk_drains,
    )
}

fn new_driver() -> InProcDriver {
    install_trunk_tier();
    match InProcDriver::new() {
        Ok(d) => d,
        Err(err) => {
            infra::raise_if_infra_io(&err, "in-proc driver init");
            panic!("in-proc driver init: {err:?}");
        }
    }
}

fn drive_one(seed: u64, n_steps: usize, planted: &[PlantedDefect]) -> SeedOutcome {
    let plan = generate_plan(seed, &soak_profile(), n_steps);
    let mut driver = new_driver().with_planted(planted.to_vec());
    let started = Instant::now();
    let out = driver.drive(&plan);
    SeedOutcome {
        steps: out.steps_replayed,
        verdict: out.verdict,
        elapsed: started.elapsed(),
        stats: out.stats,
    }
}

fn drive_corpus_scenario_plan(entry: &ShrinkerCorpusEntry) -> SeedOutcome {
    let mut driver = new_driver().with_planted(entry.planted.clone());
    let started = Instant::now();
    let out = driver.drive(&entry.plan);
    SeedOutcome {
        steps: out.steps_replayed,
        verdict: out.verdict,
        elapsed: started.elapsed(),
        stats: out.stats,
    }
}

fn env_index(key: &str) -> Option<usize> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}

/// Drive one seed, separating infrastructure failures from everything else.
///
/// `Err` ONLY for a panic whose payload `maw_assurance::infra` positively
/// classifies (EDQUOT/ENOSPC/EMFILE/ENFILE). Every other panic resumes
/// unwinding unchanged (fail closed), and oracle verdicts are returned in
/// `Ok` untouched. Only the drive is wrapped: shrinking/bundling a real
/// violation is never reclassified.
fn drive_one_checked(
    index: usize,
    seed: u64,
    n_steps: usize,
    planted: &[PlantedDefect],
) -> Result<SeedOutcome, InfraFailure> {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        if env_index("SG1_SIMULATE_INFRA_AT") == Some(index) {
            infra::raise_if_infra_io(
                &std::io::Error::from(std::io::ErrorKind::QuotaExceeded),
                "SG1_SIMULATE_INFRA_AT (simulated)",
            );
        }
        assert!(
            env_index("SG1_SIMULATE_PANIC_AT") != Some(index),
            "SG1_SIMULATE_PANIC_AT: simulated non-infra harness failure"
        );
        drive_one(seed, n_steps, planted)
    }));
    match result {
        Ok(outcome) => Ok(outcome),
        Err(payload) => match infra::classify_panic_payload(payload.as_ref()) {
            Some(failure) => Err(failure),
            None => panic::resume_unwind(payload),
        },
    }
}

/// What a run does when a seed hits an infrastructure failure.
#[derive(Debug, PartialEq, Eq)]
enum InfraDisposition {
    /// No violation so far: print the marker and exit 75.
    ExitInfra,
    /// A violation was already observed: stop and fail with the
    /// violations (a real finding always outranks a full disk).
    FailWithViolations,
}

const fn infra_disposition(violations_so_far: usize) -> InfraDisposition {
    if violations_so_far == 0 {
        InfraDisposition::ExitInfra
    } else {
        InfraDisposition::FailWithViolations
    }
}

/// Handle an infra failure at `seed`. Exits the process with
/// [`INFRA_EXIT_CODE`] unless violations were already seen, in which case
/// it returns so the caller's violation assert fires.
fn on_infra_failure(failure: &InfraFailure, seed: u64, clean: u64, violations_so_far: usize) {
    match infra_disposition(violations_so_far) {
        InfraDisposition::ExitInfra => {
            eprintln!(
                "{}",
                failure.marker_line(&format!(
                    "seed={seed}, after {clean} clean seeds; run is NOT counted"
                ))
            );
            std::process::exit(INFRA_EXIT_CODE);
        }
        InfraDisposition::FailWithViolations => {
            eprintln!(
                "[sg1] infrastructure failure at seed={seed} ({failure}) AFTER                  {violations_so_far} oracle violation(s); reporting the violations                  (fail closed)"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Failure-bundle writer (sibling of `tests/dst_support::write_failure_bundle`
// but with no `TestRepo` dependency — the in-proc tier has none).
// ---------------------------------------------------------------------------

fn artifact_root() -> PathBuf {
    std::env::var_os("DST_ARTIFACT_DIR").map_or_else(
        || std::env::temp_dir().join("maw-dst-artifacts"),
        PathBuf::from,
    )
}
fn timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_millis()
}

#[derive(serde::Serialize)]
struct Sg1Bundle {
    harness: &'static str,
    seed: u64,
    replay_command: String,
    minimized_replay_command: Option<String>,
    violation_kind: String,
    violation_entity: String,
    steps_replayed: usize,
    elapsed_ms: u128,
    shrink_iterations: Option<usize>,
    shrink_wall_ms: Option<u128>,
    corpus_entry: Option<ShrinkerCorpusEntry>,
}

fn write_sg1_failure_bundle(
    harness: &'static str,
    seed: u64,
    replay_command: String,
    verdict: &StepVerdict,
    outcome: &SeedOutcome,
    shrink: Option<&ShrinkReport>,
    corpus_entry: Option<ShrinkerCorpusEntry>,
) -> PathBuf {
    let dir = artifact_root()
        .join(harness)
        .join(format!("seed-{seed}-{}", timestamp_millis()));
    fs::create_dir_all(&dir).expect("create SG1 DST artifact directory");
    let (kind, entity) = verdict.signature();
    let bundle = Sg1Bundle {
        harness,
        seed,
        replay_command,
        minimized_replay_command: shrink.map(|s| s.minimized_replay_command.clone()),
        violation_kind: kind.to_string(),
        violation_entity: entity,
        steps_replayed: outcome.steps,
        elapsed_ms: outcome.elapsed.as_millis(),
        shrink_iterations: shrink.map(|s| s.iterations),
        shrink_wall_ms: shrink.map(|s| s.wall.as_millis()),
        corpus_entry,
    };
    let body = serde_json::to_string_pretty(&bundle).expect("serialize SG1 failure bundle");
    let path = dir.join("bundle.json");
    fs::write(&path, body).expect("write SG1 failure bundle");
    path
}

fn replay_command_for_seed(seed: u64, steps: usize) -> String {
    format!(
        "SG1_SEED={seed} SG1_PER_COMMIT_STEPS={steps} \
         cargo test -p maw-assurance --features oracles --test sg1_dst \
         sg1_per_commit_random_budget -- --exact --nocapture"
    )
}

// ---------------------------------------------------------------------------
// Test: per-commit corpus replay
// ---------------------------------------------------------------------------

/// Replay every fitting entry in `tests/corpus/dst/`. Hard-fails CI on
/// any oracle violation (the §7 acceptance gate). Always runs.
///
/// Today (pre-T1.8) the corpus only contains the legacy
/// `sample-g1-commit-crash.json` which the SG1 harness skips; the
/// legacy `dst_harness.rs` still exercises it via `just dst-fast`. As
/// soon as T1.8 (bn-3ryq) lands ScenarioPlan-shaped seeds, this test
/// picks them up automatically with zero CI re-wire.
#[test]
fn sg1_per_commit_corpus() {
    let started = Instant::now();
    let corpus = load_corpus();
    let mut violations = Vec::new();
    let mut replayed = 0usize;
    let mut skipped = 0usize;
    for entry in &corpus {
        match entry {
            CorpusEntry::ScenarioPlan { path, entry } => {
                let outcome = drive_corpus_scenario_plan(entry);
                replayed += 1;
                if entry.expected == "known_violation" {
                    if let Some(why) =
                        known_violation_mismatch(&entry.description, &outcome.verdict)
                    {
                        violations.push(format!("corpus {}: {why}", path.display()));
                    }
                } else if outcome.verdict.is_violation() {
                    let replay = format!(
                        "cargo test -p maw-assurance --features oracles --test sg1_dst \
                         sg1_per_commit_corpus -- --exact --nocapture # corpus seed {}",
                        entry.seed
                    );
                    let bundle = write_sg1_failure_bundle(
                        "sg1-dst-corpus",
                        entry.seed,
                        replay,
                        &outcome.verdict,
                        &outcome,
                        None,
                        Some((**entry).clone()),
                    );
                    violations.push(format!(
                        "corpus {}: oracle tripped on a 'pass' seed → CI red. \
                         Bundle: {}",
                        path.display(),
                        bundle.display()
                    ));
                }
            }
            CorpusEntry::Legacy {
                path,
                seed,
                description,
            } => {
                eprintln!(
                    "[sg1] legacy-schema corpus entry skipped (handled by dst_harness): \
                     {} (seed={seed}, {description})",
                    path.display()
                );
                skipped += 1;
            }
        }
    }
    eprintln!(
        "[sg1] corpus: replayed={replayed} skipped={skipped} elapsed={:?}",
        started.elapsed()
    );
    assert!(
        violations.is_empty(),
        "SG1 per-commit corpus FAILED (release-blocking; §7 acceptance gate):\n  - {}",
        violations.join("\n  - ")
    );
}

/// Why a `known_violation` corpus replay does NOT reproduce its recorded
/// violation, or `None` when it does.
///
/// bn-2qamr: only an ORACLE finding of the recorded class satisfies the pin.
/// A `HarnessError` means the oracles never judged the plan, and an oracle's
/// own tooling failure (`OracleA` "Other" / `OracleB` "GitError") is not a
/// finding either — accepting "any violation" would let a broken harness keep
/// these permanent regression pins green. The recorded class is the verdict
/// kind named in the entry's `description` (e.g. "ReachabilityLost",
/// "DanglingHeadRef").
fn known_violation_mismatch(description: &str, verdict: &StepVerdict) -> Option<String> {
    match verdict {
        StepVerdict::Clean => Some(
            "expected known_violation but oracles were CLEAN (the recorded violation no \
             longer reproduces — the underlying bug may be re-introduced silently)"
                .to_owned(),
        ),
        StepVerdict::HarnessError(_) => Some(format!(
            "expected known_violation but the harness malfunctioned — the oracles did \
             NOT judge the plan: {verdict:?}"
        )),
        StepVerdict::OracleA(_) | StepVerdict::OracleB(_) | StepVerdict::Trunk(_) => {
            let (kind, _) = verdict.signature();
            if matches!(kind, "Other" | "GitError") {
                Some(format!(
                    "expected known_violation but the oracle hit a tooling error, not a \
                     finding: {verdict:?}"
                ))
            } else if !description.contains(kind) {
                Some(format!(
                    "expected the known_violation named in the description but got a \
                     different class ({kind}): {verdict:?}"
                ))
            } else {
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test: per-commit random budget
// ---------------------------------------------------------------------------

/// Fixed-budget random seed sweep through the in-proc tier. Always
/// runs. Override `SG1_PER_COMMIT_SEEDS` / `SG1_PER_COMMIT_STEPS` to
/// retune. Set `SG1_SEED=<n>` to replay one seed.
///
/// `SG1_SKIP_SHRINK=1` (triage, bn-1h9ue) lists every red seed without
/// shrinking or writing bundles — for sweeping a mutation over many seeds.
///
/// **Planted-violation smoke** (`SG1_PLANT_VIOLATION=1`): turns the
/// run red on purpose by planting an Oracle A `WorkLoss` defect.
/// Used by `just dst-per-commit-smoke` to prove "the gate goes red
/// when something is wrong".
#[test]
fn sg1_per_commit_random_budget() {
    let started = Instant::now();
    let wall_cap = per_commit_wall_cap();
    let steps = per_commit_steps();
    let planted: Vec<PlantedDefect> = if plant_violation() {
        // Plant a WorkLoss on EVERY generator slot. A plant on a workspace
        // that no longer exists at plan end is a no-op, so this lands on
        // whichever workspaces survive. (bn-25pac: planting only on the
        // pre-seeded "ws-0" relied on in-proc destroys being silent
        // no-ops; once destroys really destroy, ws-0 is often gone by the
        // tail and the single plant never fires.)
        (0..PLANT_SLOTS)
            .map(|n| PlantedDefect::WorkLoss {
                ws: format!("ws-{n}"),
            })
            .collect()
    } else {
        Vec::new()
    };
    let seeds: Vec<u64> = match single_seed() {
        Some(s) => vec![s],
        None => {
            let n = per_commit_seeds();
            let base = base_seed();
            (0..n).map(|i| base.wrapping_add(i)).collect()
        }
    };

    let mut violations = Vec::new();
    let mut clean = 0usize;
    let mut elapsed_total = Duration::ZERO;
    let mut totals = DriveStats::default();
    let mut harness_errors = 0usize;

    for (index, seed) in seeds.iter().enumerate() {
        assert!(
            started.elapsed() <= wall_cap,
            "SG1 per-commit budget exceeded wall-clock cap of {:?} \
             after {} seeds (clean={}, violations={}); \
             lower SG1_PER_COMMIT_SEEDS or raise SG1_PER_COMMIT_WALL_CAP_SECS",
            wall_cap,
            clean + violations.len(),
            clean,
            violations.len()
        );
        let outcome = match drive_one_checked(index, *seed, steps, &planted) {
            Ok(outcome) => outcome,
            Err(failure) => {
                on_infra_failure(&failure, *seed, clean as u64, violations.len());
                break;
            }
        };
        elapsed_total += outcome.elapsed;
        totals.accumulate(&outcome.stats);
        if outcome.verdict.is_harness_error() {
            harness_errors += 1;
            eprintln!(
                "[sg1] HARNESS-ERROR seed={seed} (oracles did not judge this seed; \
                 counted as a violation): {:?}",
                outcome.verdict
            );
        }
        if outcome.verdict.is_violation() && env_bool("SG1_SKIP_SHRINK") {
            // Triage knob (bn-1h9ue): list every red seed fast, no shrink.
            eprintln!("[sg1] RED seed={seed} verdict={:?}", outcome.verdict);
            violations.push((*seed, outcome.verdict.clone(), PathBuf::new()));
        } else if outcome.verdict.is_violation() {
            // Shrink and emit a minimal bundle.
            let original_plan = generate_plan(*seed, &soak_profile(), steps);
            let report = shrink(&original_plan, &planted, outcome.verdict.clone());
            let corpus_entry = ShrinkerCorpusEntry::from_report(&report, &planted);
            let replay = replay_command_for_seed(*seed, steps);
            let bundle = write_sg1_failure_bundle(
                "sg1-dst-per-commit",
                *seed,
                replay,
                &outcome.verdict,
                &outcome,
                Some(&report),
                Some(corpus_entry),
            );
            violations.push((*seed, outcome.verdict.clone(), bundle));
        } else {
            clean += 1;
        }
    }

    eprintln!(
        "[sg1] per-commit budget: seeds={} clean={} violations={} \
         driver_total={:?} wall={:?} {}",
        seeds.len(),
        clean,
        violations.len(),
        elapsed_total,
        started.elapsed(),
        evidence_summary(&totals, harness_errors)
    );

    if plant_violation() && !plant_and_fail() {
        // CI self-test mode: invert the assertion. The plant MUST trip
        // the gate; if it didn't, the smoke is broken. bn-25pac: only a
        // real ORACLE verdict proves the gate works — a HarnessError means
        // the oracles never judged the seed, so it fails the smoke outright
        // instead of satisfying it.
        assert_eq!(
            harness_errors, 0,
            "SG1 planted-violation smoke FAILED: {harness_errors} seed(s) hit a harness error \
             (the oracles did not judge them): {violations:?}"
        );
        assert!(
            violations
                .iter()
                .any(|(_, v, _)| matches!(v, StepVerdict::OracleA(_))),
            "SG1 planted-violation smoke FAILED: planted WorkLoss did not trip any oracle \
             across {} seeds; the gate is BROKEN — investigate before relying on green CI",
            seeds.len()
        );
        eprintln!(
            "[sg1] planted-violation smoke OK: {} of {} seeds tripped the planted defect",
            violations.len(),
            seeds.len()
        );
        return;
    }
    // `SG1_PLANT_AND_FAIL=1` mode falls through to the normal
    // violation-asserts-red path below: the plant + the normal CI
    // assert prove "a regression turns the run red", which is the §5
    // acceptance criterion T1.7 requires.

    assert!(
        violations.is_empty(),
        "SG1 per-commit random budget FAILED (release-blocking; §7 acceptance gate):\n  - {}",
        violations
            .iter()
            .map(|(seed, v, bundle)| format!(
                "seed={seed} verdict={v:?} bundle={}",
                bundle.display()
            ))
            .collect::<Vec<_>>()
            .join("\n  - ")
    );
}

// ---------------------------------------------------------------------------
// Test: nightly soak
// ---------------------------------------------------------------------------

/// Nightly soak — large seed budget, in-proc tier only. `#[ignore]` so
/// it only runs when invoked with `-- --ignored` (or via
/// `just dst-nightly`). Failing seeds auto-shrink and a minimal
/// `bundle.json` lands under `DST_ARTIFACT_DIR/sg1-dst-nightly/` for
/// the existing `maw-dst-artifacts` upload to pick up.
///
/// Also explicitly includes the **`CANONICAL_BN_CM63_SEED`** so the
/// nightly run always exercises the bn-cm63 hostile interleaving even
/// if the random seed slice happens to skip it.
#[test]
#[ignore = "Nightly soak — run via `just sg1-nightly` or `cargo test -- --ignored`"]
fn sg1_nightly_soak() {
    let started = Instant::now();
    let n = nightly_seeds();
    let steps = nightly_steps();
    let base = base_seed();
    eprintln!("[sg1] nightly soak begin: seeds={n} steps={steps} base_seed=0x{base:016x}");
    let mut violations = Vec::new();
    let mut clean = 0u64;
    let mut elapsed_total = Duration::ZERO;
    let mut totals = DriveStats::default();
    let mut harness_errors = 0usize;
    let progress_every = (n / 20).max(1);

    // Always include the canonical bn-cm63 seed first.
    let mut seeds: Vec<u64> = Vec::with_capacity((n as usize) + 1);
    seeds.push(CANONICAL_BN_CM63_SEED);
    for i in 0..n {
        seeds.push(base.wrapping_add(i));
    }

    for (i, seed) in seeds.iter().enumerate() {
        if i > 0 && (i as u64) % progress_every == 0 {
            eprintln!(
                "[sg1] nightly soak progress: {}/{}  clean={}  violations={}  elapsed={:?}",
                i,
                seeds.len(),
                clean,
                violations.len(),
                started.elapsed()
            );
        }
        let outcome = match drive_one_checked(i, *seed, steps, &[]) {
            Ok(outcome) => outcome,
            Err(failure) => {
                on_infra_failure(&failure, *seed, clean, violations.len());
                break;
            }
        };
        elapsed_total += outcome.elapsed;
        totals.accumulate(&outcome.stats);
        if outcome.verdict.is_harness_error() {
            harness_errors += 1;
            eprintln!(
                "[sg1] HARNESS-ERROR seed={seed} (oracles did not judge this seed; \
                 counted as a violation): {:?}",
                outcome.verdict
            );
        }
        if outcome.verdict.is_violation() {
            let original_plan = generate_plan(*seed, &soak_profile(), steps);
            let report = shrink(&original_plan, &[], outcome.verdict.clone());
            let corpus_entry = ShrinkerCorpusEntry::from_report(&report, &[]);
            let replay = replay_command_for_seed(*seed, steps);
            let bundle = write_sg1_failure_bundle(
                "sg1-dst-nightly",
                *seed,
                replay,
                &outcome.verdict,
                &outcome,
                Some(&report),
                Some(corpus_entry),
            );
            violations.push((*seed, outcome.verdict.clone(), bundle));
        } else {
            clean += 1;
        }
    }

    eprintln!(
        "[sg1] nightly soak end: seeds={} clean={} violations={} \
         driver_total={:?} wall={:?} {}",
        seeds.len(),
        clean,
        violations.len(),
        elapsed_total,
        started.elapsed(),
        evidence_summary(&totals, harness_errors)
    );

    assert!(
        violations.is_empty(),
        "SG1 nightly soak FAILED (release-blocking; §7 acceptance gate):\n  - {}",
        violations
            .iter()
            .map(|(seed, v, bundle)| format!(
                "seed={seed} verdict={v:?} bundle={}",
                bundle.display()
            ))
            .collect::<Vec<_>>()
            .join("\n  - ")
    );
}

// ---------------------------------------------------------------------------
// Dirty-trunk tier: every target-update window, production code (bn-1h9ue)
// ---------------------------------------------------------------------------

/// A hand-built plan that drives the PRODUCTION target update through every
/// window the soak profile crashes in — the snapshot-failed fallback, a crash
/// after the checkout (bn-15fzo resume) and a crash before the update — over
/// a dirty trunk (tracked edit, new untracked file, new symlink), and proves
/// the seed is judged clean AND non-vacuously: crashes happened, the
/// displacement oracle judged entries, the replay model judged updates, and
/// the user's uncommitted entries are on disk at the end.
#[test]
fn sg1_trunk_tier_production_windows_are_judged() {
    use maw_assurance::scenario::{
        BaseRef, EditKind, FaultSpec, FileEdit, GIT_TIME_BASE_FOR_DRIVER, Op, PlannedStep,
        ScenarioPlan, Seeded, Target, WsId,
    };
    let fp = |name: &str| FaultSpec::Failpoint {
        name: name.to_owned(),
        phase: "cleanup".to_owned(),
    };
    let mut ops: Vec<(Op, FaultSpec)> = Vec::new();
    let mut round = |n: usize, path: &str, content: &str, fault: FaultSpec| {
        let ws = WsId::slot(n);
        ops.push((
            Op::WsCreate {
                ws: ws.clone(),
                from: BaseRef::Main,
            },
            FaultSpec::None,
        ));
        ops.push((
            Op::EditFiles {
                ws: ws.clone(),
                files: vec![FileEdit::write(path, content)],
            },
            FaultSpec::None,
        ));
        ops.push((
            Op::Commit {
                ws: ws.clone(),
                msg: Seeded(format!("round {n}")),
            },
            FaultSpec::None,
        ));
        ops.push((
            Op::Merge {
                srcs: vec![ws],
                into: Target::Default,
                destroy: false,
            },
            fault,
        ));
    };
    round(0, "shared/file-0.txt", "committed\n", FaultSpec::None);
    let dirty = Op::DirtyTrunkWrite {
        files: vec![
            FileEdit::write("shared/file-0.txt", "uncommitted\n"),
            FileEdit {
                path: "shared/file-1.txt".into(),
                content: "file-0.txt".into(),
                kind: EditKind::Symlink,
            },
            FileEdit::write("trunk/new-0.txt", "untracked\n"),
        ],
    };
    round(1, "shared/file-2.txt", "one\n", FaultSpec::None);
    let mut all = ops.clone();
    all.push((dirty, FaultSpec::None));
    ops.clear();
    let round2 = |n: usize, path: &str, content: &str, fault: FaultSpec| {
        let ws = WsId::slot(n);
        vec![
            (
                Op::WsCreate {
                    ws: ws.clone(),
                    from: BaseRef::Main,
                },
                FaultSpec::None,
            ),
            (
                Op::EditFiles {
                    ws: ws.clone(),
                    files: vec![FileEdit::write(path, content)],
                },
                FaultSpec::None,
            ),
            (
                Op::Commit {
                    ws: ws.clone(),
                    msg: Seeded(format!("round {n}")),
                },
                FaultSpec::None,
            ),
            (
                Op::Merge {
                    srcs: vec![ws],
                    into: Target::Default,
                    destroy: false,
                },
                fault,
            ),
        ]
    };
    all.extend(round2(
        2,
        "shared/file-2.txt",
        "two\n",
        fp("FP_UPDATE_DEFAULT_BEFORE_SNAPSHOT"),
    ));
    all.extend(round2(
        3,
        "shared/file-3.txt",
        "three\n",
        fp("FP_CLEANUP_AFTER_DEFAULT_CHECKOUT"),
    ));
    all.extend(round2(
        4,
        "shared/file-2.txt",
        "four\n",
        fp("FP_CLEANUP_BEFORE_DEFAULT_CHECKOUT"),
    ));
    all.extend(round2(5, "shared/file-3.txt", "five\n", FaultSpec::None));
    let plan = ScenarioPlan {
        seed: 0x1_19E0,
        profile: soak_profile(),
        steps: all
            .into_iter()
            .enumerate()
            .map(|(index, (op, fault))| PlannedStep {
                index,
                op,
                fault,
                git_time: GIT_TIME_BASE_FOR_DRIVER + 60 * (i64::try_from(index).unwrap() + 1),
            })
            .collect(),
    };
    let mut driver = new_driver();
    assert!(
        driver.has_trunk(),
        "the sg1_dst binary installs the production updater"
    );
    let out = driver.drive(&plan);
    assert!(
        matches!(out.verdict, StepVerdict::Clean),
        "verdict={:?} stats={:?}",
        out.verdict,
        out.stats
    );
    let s = out.stats;
    assert_eq!(s.trunk_crashes, 2, "{s:?}");
    assert!(s.trunk_updates >= 6, "{s:?}");
    assert!(s.dirty_trunk_merges >= 2, "{s:?}");
    assert!(s.displacement_checks >= 4, "{s:?}");
    assert!(s.replay_judgements >= 5, "{s:?}");
    let w = driver.default_ws_path().expect("trunk tier").to_path_buf();
    assert_eq!(
        fs::read_to_string(w.join("shared/file-0.txt")).unwrap(),
        "uncommitted\n"
    );
    assert_eq!(
        fs::read_to_string(w.join("trunk/new-0.txt")).unwrap(),
        "untracked\n"
    );
    assert_eq!(
        fs::read_link(w.join("shared/file-1.txt")).unwrap(),
        PathBuf::from("file-0.txt")
    );
    assert_eq!(
        fs::read_to_string(w.join("shared/file-3.txt")).unwrap(),
        "five\n"
    );
}

// ---------------------------------------------------------------------------
// Triage: replay a failure bundle's minimal plan (bn-1h9ue)
// ---------------------------------------------------------------------------

/// Replay the minimal plan of a failure bundle (`SG1_REPLAY_BUNDLE=<path to
/// bundle.json>`, or a corpus entry JSON) and print its verdict. Combine with
/// `MAW_INPROC_DEBUG=1` to see every target update's output. A no-op without
/// the env var.
#[test]
#[ignore = "triage helper: SG1_REPLAY_BUNDLE=<bundle.json>"]
fn sg1_replay_bundle() {
    let Ok(path) = std::env::var("SG1_REPLAY_BUNDLE") else {
        return;
    };
    let body = fs::read_to_string(&path).expect("read bundle");
    let json: serde_json::Value = serde_json::from_str(&body).expect("bundle json");
    let entry_json = json.get("corpus_entry").cloned().unwrap_or(json);
    let entry: ShrinkerCorpusEntry = serde_json::from_value(entry_json).expect("corpus entry");
    let mut driver = new_driver().with_planted(entry.planted.clone());
    let out = driver.drive(&entry.plan);
    eprintln!(
        "[sg1] replay {path}: steps={} verdict={:?} repo={}",
        out.steps_replayed,
        out.verdict,
        driver.repo_root().display()
    );
    if std::env::var("SG1_REPLAY_KEEP").is_ok() {
        let keep = driver.repo_root().to_path_buf();
        std::mem::forget(driver);
        eprintln!("[sg1] kept repo at {}", keep.display());
    }
}

// ---------------------------------------------------------------------------
// Sanity test: the corpus dir is reachable.
// ---------------------------------------------------------------------------

#[test]
fn sg1_corpus_dir_is_reachable() {
    let dir = corpus_dir();
    assert!(
        dir.is_dir(),
        "expected corpus dir at {} (resolved relative to CARGO_MANIFEST_DIR={}). \
         If you moved the corpus, update `corpus_dir()` in this file.",
        dir.display(),
        env!("CARGO_MANIFEST_DIR")
    );
}

// ---------------------------------------------------------------------------
// bn-2qamr: a `known_violation` corpus pin is satisfied ONLY by the oracle
// verdict it records — never by a harness error (the oracles did not judge
// the seed) or by a different violation class.
// ---------------------------------------------------------------------------

fn corpus_scenario_entry(file: &str) -> ShrinkerCorpusEntry {
    load_corpus()
        .into_iter()
        .find_map(|e| match e {
            CorpusEntry::ScenarioPlan { path, entry } if path.ends_with(file) => Some(*entry),
            _ => None,
        })
        .unwrap_or_else(|| panic!("corpus entry {file} not found"))
}

#[test]
fn sg1_known_violation_rejects_harness_error_and_other_classes() {
    let lost =
        "2026-02-05 lost-commits incident class (seed=6, Oracle A ReachabilityLost on ws-12)";
    let reach = StepVerdict::OracleA(OracleAClass {
        kind: "ReachabilityLost",
        oid: "abc".into(),
    });
    assert_eq!(known_violation_mismatch(lost, &reach), None);
    for (why, verdict) in [
        ("clean", StepVerdict::Clean),
        (
            "harness error",
            StepVerdict::HarnessError(HarnessErrorClass {
                site: "oracle_a_check",
                detail: "fatal: bad object".into(),
            }),
        ),
        (
            "different oracle class",
            StepVerdict::OracleB(OracleBClass {
                kind: "DanglingHeadRef",
                entity: "ws-12".into(),
            }),
        ),
        (
            "oracle A tooling failure",
            StepVerdict::OracleA(OracleAClass {
                kind: "Other",
                oid: "git error".into(),
            }),
        ),
        (
            "oracle B tooling failure",
            StepVerdict::OracleB(OracleBClass {
                kind: "GitError",
                entity: "B1".into(),
            }),
        ),
    ] {
        assert!(
            known_violation_mismatch(lost, &verdict).is_some(),
            "{why} must NOT satisfy a known_violation pin: {verdict:?}"
        );
    }
    // A tooling failure is never a finding, even when the free-text
    // description happens to contain its kind word.
    let wordy = "Oracle A ReachabilityLost on ws-12. Other workspaces keep a GitError-free view.";
    for verdict in [
        StepVerdict::OracleA(OracleAClass {
            kind: "Other",
            oid: "git error".into(),
        }),
        StepVerdict::OracleB(OracleBClass {
            kind: "GitError",
            entity: "B1".into(),
        }),
    ] {
        assert!(
            known_violation_mismatch(wordy, &verdict).is_some(),
            "tooling failure must NOT satisfy a known_violation pin: {verdict:?}"
        );
    }
}

/// End to end: the lost-commits pin replayed on a harness that cannot apply
/// its plan must fail the corpus check, not pass as "reproduced".
#[test]
fn sg1_known_violation_pin_is_not_satisfied_by_a_broken_harness() {
    let entry = corpus_scenario_entry("lost-commits-2026-02-05.json");
    assert_eq!(entry.expected, "known_violation");
    let first_ws = entry
        .plan
        .steps
        .iter()
        .find_map(|s| match &s.op {
            maw_assurance::scenario::Op::WsCreate { ws, .. } => Some(ws.0.clone()),
            _ => None,
        })
        .expect("plan creates a workspace");
    let mut driver = new_driver().with_planted(entry.planted.clone());
    // A FILE where the first workspace's directory must go: WsCreate cannot
    // apply, so the oracles never judge the plan.
    fs::write(driver.repo_root().join("ws").join(&first_ws), "obstruction").unwrap();
    let out = driver.drive(&entry.plan);
    assert!(out.verdict.is_harness_error(), "{:?}", out.verdict);
    assert!(
        known_violation_mismatch(&entry.description, &out.verdict).is_some(),
        "a harness error must not count as the pin reproducing: {:?}",
        out.verdict
    );
}

// ---------------------------------------------------------------------------
// Generator-determinism micro-check (sanity, not the T1.6 acceptance test;
// that one lives in `crates/maw-assurance/src/shrinker_tests.rs`).
// ---------------------------------------------------------------------------

#[test]
fn sg1_generator_is_byte_identical_per_seed() {
    let profile = soak_profile();
    let a = DefaultScenarioGenerator::generate(123, &profile);
    let b = DefaultScenarioGenerator::generate(123, &profile);
    let a_json = a.canonical_json().expect("serialize a");
    let b_json = b.canonical_json().expect("serialize b");
    assert_eq!(
        a_json, b_json,
        "DefaultScenarioGenerator is NOT byte-identical for seed 123 — \
         the §5 determinism contract is broken; SG1 cannot trust replay"
    );
}

// ---------------------------------------------------------------------------
// bn-30v6e: infra-vs-violation exit contract (what scripts/sg1-soak/slot.sh
// relies on). Runs THIS test binary as a child on `sg1_nightly_soak`.
// ---------------------------------------------------------------------------

#[test]
fn sg1_infra_disposition_prefers_violations() {
    assert_eq!(infra_disposition(0), InfraDisposition::ExitInfra);
    assert_eq!(infra_disposition(1), InfraDisposition::FailWithViolations);
    assert_eq!(infra_disposition(7), InfraDisposition::FailWithViolations);
}

fn run_nightly_child(extra_env: &[(&str, &str)]) -> (Option<i32>, String) {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new(exe);
    cmd.args(["sg1_nightly_soak", "--ignored", "--exact", "--nocapture"]);
    for key in [
        "SG1_SEED",
        "SG1_BASE_SEED",
        "SG1_SIMULATE_INFRA_AT",
        "SG1_SIMULATE_PANIC_AT",
        "SG1_PLANT_VIOLATION",
        "SG1_PLANT_AND_FAIL",
    ] {
        cmd.env_remove(key);
    }
    cmd.env("SG1_NIGHTLY_SEEDS", "1")
        .env("SG1_NIGHTLY_STEPS", "4");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn sg1_dst child");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code(), text)
}

fn has_marker_line(text: &str) -> bool {
    text.lines().any(|l| l.starts_with(INFRA_MARKER))
}

#[test]
fn sg1_infra_exit_contract() {
    // Infra at seed index 1 (index 0 is the canonical bn-cm63 seed):
    // exit 75 + exactly the marker, and no "soak end" accrual line.
    let (code, text) = run_nightly_child(&[("SG1_SIMULATE_INFRA_AT", "1")]);
    assert_eq!(
        code,
        Some(INFRA_EXIT_CODE),
        "infra run must exit 75:\n{text}"
    );
    assert!(has_marker_line(&text), "missing marker line:\n{text}");
    assert!(text.contains("EDQUOT"), "{text}");
    assert!(
        !text.contains("nightly soak end:"),
        "an infra run must not print the accrual line:\n{text}"
    );

    // Fail closed: an ordinary panic is NOT infra.
    let (code, text) = run_nightly_child(&[("SG1_SIMULATE_PANIC_AT", "1")]);
    assert_ne!(code, Some(0), "{text}");
    assert_ne!(
        code,
        Some(INFRA_EXIT_CODE),
        "non-infra panic exited 75:\n{text}"
    );
    assert!(
        !has_marker_line(&text),
        "non-infra panic printed marker:\n{text}"
    );

    // Baseline: a clean run exits 0 and prints the accrual line.
    let (code, text) = run_nightly_child(&[]);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.contains("nightly soak end: seeds=2 clean=2"), "{text}");
    assert!(!has_marker_line(&text), "{text}");
}

/// Run the nightly soak child with a `git` shim first on PATH. The shim
/// fails any git invocation whose argv starts with `when` by printing
/// `stderr` and exiting 128; everything else runs the real git. This drives
/// the REAL driver code paths (driver init, apply_op, git helpers) with the
/// exact stderr shapes that halted the pre.6 campaign.
fn run_nightly_child_with_git_shim(when: &str, stderr: &str, steps: &str) -> (Option<i32>, String) {
    let real_git = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .expect("locate git");
    assert!(!real_git.is_empty(), "git not on PATH");
    let shim_dir = tempfile::TempDir::new().expect("shim dir");
    let shim = shim_dir.path().join("git");
    fs::write(
        &shim,
        format!(
            "#!/usr/bin/env bash\n\
             if [[ \"$*\" == \"$SHIM_GIT_WHEN\"* ]]; then printf '%s\\n' \"$SHIM_GIT_STDERR\" >&2; exit 128; fi\n\
             exec {real_git} \"$@\"\n"
        ),
    )
    .expect("write shim");
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).expect("chmod shim");
    }
    let path = format!(
        "{}:{}",
        shim_dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    run_nightly_child(&[
        ("PATH", path.as_str()),
        ("SHIM_GIT_WHEN", when),
        ("SHIM_GIT_STDERR", stderr),
        ("SG1_NIGHTLY_STEPS", steps),
    ])
}

#[cfg(unix)]
#[test]
fn sg1_infra_real_git_failures_classify_both_directions() {
    // 1. `git init` hits EDQUOT (the first pre.6 halt) => infra, exit 75.
    let (code, text) = run_nightly_child_with_git_shim(
        "init",
        "fatal: cannot mkdir .git: Disk quota exceeded",
        "4",
    );
    assert_eq!(code, Some(INFRA_EXIT_CODE), "{text}");
    assert!(has_marker_line(&text), "{text}");
    assert!(
        text.contains("EDQUOT") && text.contains("driver init"),
        "{text}"
    );

    // 2. A workspace-ref write inside a plan step hits ENOSPC => the driver's
    //    apply_op path aborts the seed as infra instead of letting the
    //    oracles judge a half-applied step.
    let (code, text) = run_nightly_child_with_git_shim(
        "update-ref refs/manifold/ws/",
        "fatal: update_ref failed: unable to write: No space left on device",
        "8",
    );
    assert_eq!(code, Some(INFRA_EXIT_CODE), "{text}");
    assert!(has_marker_line(&text), "{text}");
    assert!(
        text.contains("ENOSPC") && text.contains("apply step"),
        "{text}"
    );

    // 3. Fail closed: a NON-infra `git init` failure still panics (exit 101,
    //    no marker) exactly as before.
    let (code, text) =
        run_nightly_child_with_git_shim("init", "fatal: bad config line 1 in file", "4");
    assert_ne!(code, Some(0), "{text}");
    assert_ne!(code, Some(INFRA_EXIT_CODE), "{text}");
    assert!(!has_marker_line(&text), "{text}");

    // 4. A non-infra step failure is never classified as infra — and
    //    (bn-25pac) it is no longer swallowed as "best effort" either: the
    //    seed fails as a HarnessError, exactly like an oracle violation.
    let (code, text) = run_nightly_child_with_git_shim(
        "update-ref refs/manifold/ws/",
        "fatal: cannot lock ref 'refs/manifold/ws/ws-0'",
        "8",
    );
    assert_harness_failure(code, &text, "apply_op:");
}

/// bn-25pac: the run failed (non-zero, not infra, no marker, no accrual
/// line claiming clean seeds) and names a HarnessError at `site_prefix`.
fn assert_harness_failure(code: Option<i32>, text: &str, site_prefix: &str) {
    assert_ne!(code, Some(0), "harness error must fail the run:\n{text}");
    assert_ne!(code, Some(INFRA_EXIT_CODE), "{text}");
    assert!(!has_marker_line(text), "{text}");
    assert!(
        text.contains(&format!("site: \"{site_prefix}")),
        "expected a HarnessError at {site_prefix}:\n{text}"
    );
    assert!(
        !text.contains("nightly soak end: seeds=2 clean=2"),
        "a harness error must not count as clean:\n{text}"
    );
}

/// bn-25pac: an unreadable post-step state (capture_state's
/// `git for-each-ref` failing for a non-infra reason) used to read as a
/// CLEAN step; now the seed fails as a HarnessError.
#[cfg(unix)]
#[test]
fn sg1_capture_state_failure_fails_closed() {
    let (code, text) = run_nightly_child_with_git_shim(
        "for-each-ref --format=%(refname) %(objectname)",
        "fatal: bad object refs/manifold/whatever",
        "8",
    );
    assert_harness_failure(code, &text, "capture_state");
}

/// bn-25pac: an Oracle A tooling error (`git diff --raw` failing for a
/// non-infra reason) used to be ignored (`Err(_) => {}`) and the step read
/// as clean; now the seed fails as a HarnessError.
#[cfg(unix)]
#[test]
fn sg1_oracle_a_tooling_failure_fails_closed() {
    let (code, text) =
        run_nightly_child_with_git_shim("diff --raw", "fatal: unable to read tree deadbeef", "24");
    assert_harness_failure(code, &text, "oracle_a_check");
}
