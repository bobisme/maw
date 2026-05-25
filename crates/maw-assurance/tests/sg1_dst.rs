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
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use maw_assurance::in_proc::{InProcDriver, PlantedDefect, StepVerdict};
use maw_assurance::scenario::{
    CANONICAL_BN_CM63_SEED, ConditionProfile, DefaultScenarioGenerator, ScenarioGenerator,
    generate_plan,
};
use maw_assurance::shrinker::{ShrinkReport, ShrinkerCorpusEntry, shrink};

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
fn plant_and_fail() -> bool {
    env_bool("SG1_PLANT_AND_FAIL")
}

fn env_bool(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
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
}

fn drive_one(seed: u64, n_steps: usize, planted: &[PlantedDefect]) -> SeedOutcome {
    let plan = generate_plan(seed, &ConditionProfile::default(), n_steps);
    let mut driver = InProcDriver::new()
        .expect("in-proc driver init")
        .with_planted(planted.to_vec());
    let started = Instant::now();
    let out = driver.drive(&plan);
    SeedOutcome {
        steps: out.steps_replayed,
        verdict: out.verdict,
        elapsed: started.elapsed(),
    }
}

fn drive_corpus_scenario_plan(entry: &ShrinkerCorpusEntry) -> SeedOutcome {
    let mut driver = InProcDriver::new()
        .expect("in-proc driver init")
        .with_planted(entry.planted.clone());
    let started = Instant::now();
    let out = driver.drive(&entry.plan);
    SeedOutcome {
        steps: out.steps_replayed,
        verdict: out.verdict,
        elapsed: started.elapsed(),
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
    let (kind, entity) = match verdict {
        StepVerdict::Clean => ("Clean", String::new()),
        StepVerdict::OracleA(a) => (a.kind, a.oid.clone()),
        StepVerdict::OracleB(b) => (b.kind, b.entity.clone()),
    };
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
                let want_violation = entry.expected == "known_violation";
                let actually_violated = outcome.verdict.is_violation();
                replayed += 1;
                if want_violation && !actually_violated {
                    let msg = format!(
                        "corpus {}: expected known_violation but oracles were CLEAN \
                         (the recorded violation no longer reproduces — \
                         the underlying bug may be re-introduced silently)",
                        path.display()
                    );
                    violations.push(msg);
                } else if !want_violation && actually_violated {
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

// ---------------------------------------------------------------------------
// Test: per-commit random budget
// ---------------------------------------------------------------------------

/// Fixed-budget random seed sweep through the in-proc tier. Always
/// runs. Override `SG1_PER_COMMIT_SEEDS` / `SG1_PER_COMMIT_STEPS` to
/// retune. Set `SG1_SEED=<n>` to replay one seed.
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
        // The default generator's pre-seeded workspace is "ws-0".
        vec![PlantedDefect::WorkLoss { ws: "ws-0".into() }]
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

    for seed in &seeds {
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
        let outcome = drive_one(*seed, steps, &planted);
        elapsed_total += outcome.elapsed;
        if outcome.verdict.is_violation() {
            // Shrink and emit a minimal bundle.
            let original_plan = generate_plan(*seed, &ConditionProfile::default(), steps);
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
         driver_total={:?} wall={:?}",
        seeds.len(),
        clean,
        violations.len(),
        elapsed_total,
        started.elapsed()
    );

    if plant_violation() && !plant_and_fail() {
        // CI self-test mode: invert the assertion. The plant MUST trip
        // the gate; if it didn't, the smoke is broken.
        assert!(
            !violations.is_empty(),
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
        let outcome = drive_one(*seed, steps, &[]);
        elapsed_total += outcome.elapsed;
        if outcome.verdict.is_violation() {
            let original_plan = generate_plan(*seed, &ConditionProfile::default(), steps);
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
         driver_total={:?} wall={:?}",
        seeds.len(),
        clean,
        violations.len(),
        elapsed_total,
        started.elapsed()
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
// Generator-determinism micro-check (sanity, not the T1.6 acceptance test;
// that one lives in `crates/maw-assurance/src/shrinker_tests.rs`).
// ---------------------------------------------------------------------------

#[test]
fn sg1_generator_is_byte_identical_per_seed() {
    let profile = ConditionProfile::default();
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
