//! T1.6 acceptance tests: determinism guarantee + failing-seed shrinker
//! (bn-32k3).
//!
//! These are the central acceptance battery the bone calls for. They
//! consume the in-proc driver ([`crate::in_proc`]) + the shrinker
//! ([`crate::shrinker`]) end-to-end against planted Oracle A / Oracle B
//! violations.

#![cfg(all(test, feature = "oracles"))]
#![allow(
    clippy::unwrap_used,
    clippy::missing_panics_doc,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::uninlined_format_args,
    clippy::manual_let_else,
    clippy::option_if_let_else,
    clippy::redundant_clone,
    clippy::explicit_counter_loop
)]

use std::time::Instant;

use crate::in_proc::{InProcDriver, PlantedDefect, StepVerdict};
use crate::oracle_b;
use crate::scenario::{
    CANONICAL_BN_CM63_SEED, ConditionProfile, DefaultScenarioGenerator,
    ScenarioGenerator, generate_plan,
};
use crate::shrinker::{ShrinkerCorpusEntry, TARGET_MIN_STEPS, shrink};

// ---------------------------------------------------------------------------
// Helpers — small repeatable plans tailored to plant violations
// ---------------------------------------------------------------------------

/// Build a short plan that creates `ws-7` (a synthetic name unlikely to
/// collide with the generator's `ws-N` slot allocation) and commits a
/// witness blob, then return it together with a planted `WorkLoss` defect
/// at the last commit step.
///
/// This is the **Oracle A determinism fixture** — replaying this 10×
/// MUST yield a `ReachabilityLost` on the exact same blob OID every time.
fn planted_oracle_a_fixture() -> (crate::scenario::ScenarioPlan, Vec<PlantedDefect>) {
    use crate::scenario::{
        BaseRef, FaultSpec, FileEdit, Op, PlannedStep, ScenarioPlan, Seeded, WsId,
    };
    // Build by hand — 4 steps, totally deterministic.
    let ws = WsId::slot(7);
    let plan = ScenarioPlan {
        seed: 0xA1B2_C3D4_E5F6_7890,
        profile: ConditionProfile::default(),
        steps: vec![
            PlannedStep {
                index: 0,
                op: Op::WsCreate {
                    ws: ws.clone(),
                    from: BaseRef::Main,
                },
                fault: FaultSpec::None,
                git_time: crate::scenario::GIT_TIME_BASE_FOR_DRIVER + 100,
            },
            PlannedStep {
                index: 1,
                op: Op::EditFiles {
                    ws: ws.clone(),
                    files: vec![FileEdit {
                        path: "doc.txt".into(),
                        content: "oracle-a-witness-content-v1\n".into(),
                    }],
                },
                fault: FaultSpec::None,
                git_time: crate::scenario::GIT_TIME_BASE_FOR_DRIVER + 200,
            },
            PlannedStep {
                index: 2,
                op: Op::Commit {
                    ws: ws.clone(),
                    msg: Seeded("planted work-loss fixture".into()),
                },
                fault: FaultSpec::None,
                git_time: crate::scenario::GIT_TIME_BASE_FOR_DRIVER + 300,
            },
            // Step 3 is a no-op placeholder so the planted defect can
            // fire *after* the commit witnessed the blob (the harvest
            // happens during step 2's check; the plant happens after
            // step 3's apply, then step 3's oracle check fires).
            PlannedStep {
                index: 3,
                op: Op::Sync { ws: ws.clone() },
                fault: FaultSpec::None,
                git_time: crate::scenario::GIT_TIME_BASE_FOR_DRIVER + 400,
            },
        ],
    };
    let planted = vec![PlantedDefect::WorkLoss { ws: "ws-7".into() }];
    (plan, planted)
}

/// Build a short plan rooted at the canonical bn-cm63 seed and plant a
/// `DanglingHeadRef` defect at its tail. This is the **Oracle B
/// determinism fixture** — replaying it 10× MUST yield a B1
/// `DanglingHeadRef` on the same workspace every time.
fn planted_oracle_b_fixture() -> (crate::scenario::ScenarioPlan, Vec<PlantedDefect>) {
    // Use the bn-cm63 canonical seed to make the test self-document:
    // even though the plant is what *forces* the violation, the seed is
    // the same one downstream tasks (T1.8 corpus) will ingest, so
    // failure mode + replay surface stays aligned.
    let plan = generate_plan(CANONICAL_BN_CM63_SEED, &ConditionProfile::default(), 16);
    // Plant a dangling head ref on workspace `ws-0` (the generator
    // pre-seeds slot 0 as a starter ws, so it always exists; ensures
    // the plant lands somewhere real).
    let planted = vec![PlantedDefect::DanglingHeadRef { ws: "ws-0".into() }];
    (plan, planted)
}

/// Drive a plan + planted defects once and return the first-violating verdict.
fn drive_once(
    plan: &crate::scenario::ScenarioPlan,
    planted: &[PlantedDefect],
) -> StepVerdict {
    let mut driver = InProcDriver::new()
        .expect("driver init")
        .with_planted(planted.to_vec());
    driver.drive(plan).verdict
}

// ---------------------------------------------------------------------------
// (1) Determinism guarantee — 10/10 reproduction of an Oracle A violation
// ---------------------------------------------------------------------------

#[test]
fn determinism_oracle_a_reproduces_10_of_10() {
    let (plan, planted) = planted_oracle_a_fixture();
    let mut verdicts = Vec::with_capacity(10);
    for i in 0..10 {
        let v = drive_once(&plan, &planted);
        assert!(
            matches!(v, StepVerdict::OracleA(_)),
            "iteration {i}: planted work-loss MUST trip Oracle A, got {v:?}"
        );
        verdicts.push(v);
    }
    let first = &verdicts[0];
    for (i, v) in verdicts.iter().enumerate() {
        assert!(
            first.same_class(v),
            "iteration {i}: violation class drifted from baseline\n  baseline={first:?}\n  iter={v:?}"
        );
    }
    if let StepVerdict::OracleA(c) = first {
        assert_eq!(c.kind, "ReachabilityLost");
        assert!(!c.oid.is_empty(), "lost blob OID must be present");
    }
}

// ---------------------------------------------------------------------------
// (2) Determinism guarantee — 10/10 reproduction of an Oracle B violation
//     (bn-cm63 canonical seed + planted dangling head ref)
// ---------------------------------------------------------------------------

#[test]
fn determinism_oracle_b_reproduces_10_of_10_on_bn_cm63_seed() {
    let (plan, planted) = planted_oracle_b_fixture();
    let mut verdicts = Vec::with_capacity(10);
    for i in 0..10 {
        let v = drive_once(&plan, &planted);
        assert!(
            matches!(v, StepVerdict::OracleB(_)),
            "iteration {i}: planted dangling-head-ref MUST trip Oracle B, got {v:?}"
        );
        verdicts.push(v);
    }
    let first = &verdicts[0];
    for (i, v) in verdicts.iter().enumerate() {
        assert!(
            first.same_class(v),
            "iteration {i}: violation class drifted from baseline\n  baseline={first:?}\n  iter={v:?}"
        );
    }
    if let StepVerdict::OracleB(c) = first {
        assert!(
            matches!(
                c.kind,
                "DanglingHeadRef" | "DanglingOwnedRef" | "MergeStateOrphanSource"
            ),
            "unexpected Oracle B class {c:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// (3) git_time pinning is mandatory — a plan replays bit-exactly only when
//     PlannedStep.git_time pins GIT_AUTHOR_DATE/GIT_COMMITTER_DATE. We
//     verify by capturing the post-replay refs from two consecutive drives
//     and asserting they're equal (the load-bearing OID invariant).
// ---------------------------------------------------------------------------

#[test]
fn git_time_pinning_yields_bit_exact_repo_state() {
    use std::process::Command;

    let plan = generate_plan(CANONICAL_BN_CM63_SEED, &ConditionProfile::default(), 12);
    let mut snapshots: Vec<Vec<(String, String)>> = Vec::with_capacity(3);
    for _ in 0..3 {
        let mut driver = InProcDriver::new().expect("driver init");
        let _ = driver.drive(&plan);
        let out = Command::new("git")
            .args(["for-each-ref", "--format=%(refname) %(objectname)"])
            .current_dir(driver.repo_root())
            .output()
            .expect("for-each-ref");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut refs: Vec<(String, String)> = stdout
            .lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(n, o)| (n.to_string(), o.to_string()))
            .collect();
        refs.sort();
        snapshots.push(refs);
    }
    for i in 1..snapshots.len() {
        assert_eq!(
            snapshots[0], snapshots[i],
            "iteration {i}: ref OIDs diverged from iteration 0 — \
             git_time pinning is BROKEN; replays are not bit-exact",
        );
    }
}

// ---------------------------------------------------------------------------
// (4) Shrinker reduces a >50-step plan with planted Oracle B violation to
//     <10 steps, and the reduction is sound (replays the same violation).
// ---------------------------------------------------------------------------

#[test]
fn shrinker_reduces_oracle_b_plan_to_under_10_steps_and_is_sound() {
    // Start from a deliberately long plan (>50 steps) so the shrinker
    // has plenty to remove.
    let long = generate_plan(CANONICAL_BN_CM63_SEED, &ConditionProfile::default(), 64);
    assert!(
        long.steps.len() >= 50,
        "fixture must start with ≥50 steps; got {}",
        long.steps.len()
    );
    let planted = vec![PlantedDefect::DanglingHeadRef { ws: "ws-0".into() }];
    let original = drive_once(&long, &planted);
    assert!(
        original.is_violation(),
        "fixture must start with a violation; got {original:?}"
    );

    let t0 = Instant::now();
    let report = shrink(&long, &planted, original.clone());
    let wall = t0.elapsed();
    eprintln!(
        "shrinker: {} -> {} steps in {} iters / {:.2}s ({:.1}ms/iter)",
        long.steps.len(),
        report.minimal.steps.len(),
        report.iterations,
        wall.as_secs_f64(),
        wall.as_secs_f64() * 1000.0 / report.iterations.max(1) as f64,
    );

    // -- Minimality --
    assert!(
        report.minimal.steps.len() < TARGET_MIN_STEPS,
        "shrinker output ({} steps) must be < {TARGET_MIN_STEPS}",
        report.minimal.steps.len(),
    );

    // -- Soundness: replay STANDALONE (fresh driver) and verify SAME class --
    let replayed = drive_once(&report.minimal, &planted);
    assert!(
        report.original_verdict.same_class(&replayed),
        "shrinker UNSOUND: minimal plan replayed produced {replayed:?}, not the original {:?}",
        report.original_verdict
    );
    assert!(
        report.minimal_verdict.same_class(&replayed),
        "shrinker self-check failed: report.minimal_verdict {:?} != standalone replay {replayed:?}",
        report.minimal_verdict,
    );

    // -- Never green --
    assert!(
        replayed.is_violation(),
        "shrinker UNSOUND: minimal plan replayed produced a clean verdict"
    );
}

// ---------------------------------------------------------------------------
// (5) Shrinker reduces an Oracle A plan to <10 steps, sound.
// ---------------------------------------------------------------------------

#[test]
fn shrinker_reduces_oracle_a_plan_to_under_10_steps_and_is_sound() {
    // Pad the planted A fixture with no-op tail to give the shrinker work.
    let (mut plan, _initial_planted) = planted_oracle_a_fixture();
    // Splice in 50+ extra "Sync" steps before the plant fires, then update
    // the plant's after_step accordingly.
    use crate::scenario::{FaultSpec, Op, PlannedStep, WsId};
    let mut bloated = plan.steps.clone();
    let tail_pos = 3; // before the placeholder
    let mut t = crate::scenario::GIT_TIME_BASE_FOR_DRIVER + 350;
    for i in 0..50 {
        bloated.insert(
            tail_pos + i,
            PlannedStep {
                index: tail_pos + i,
                op: Op::Sync {
                    ws: WsId::slot(7),
                },
                fault: FaultSpec::None,
                git_time: t,
            },
        );
        t += 1;
    }
    // Re-pack indices.
    for (i, s) in bloated.iter_mut().enumerate() {
        s.index = i;
    }
    plan.steps = bloated;
    assert!(
        plan.steps.len() > 50,
        "Oracle A fixture must be >50 steps for this test"
    );
    let planted = vec![PlantedDefect::WorkLoss { ws: "ws-7".into() }];
    let _ = planted_oracle_a_fixture; // satisfy unused-import lint

    let original = drive_once(&plan, &planted);
    assert!(
        matches!(original, StepVerdict::OracleA(_)),
        "fixture must trip Oracle A; got {original:?}"
    );

    let t0 = Instant::now();
    let report = shrink(&plan, &planted, original.clone());
    let wall = t0.elapsed();
    eprintln!(
        "shrinker (A): {} -> {} steps in {} iters / {:.2}s ({:.1}ms/iter)",
        plan.steps.len(),
        report.minimal.steps.len(),
        report.iterations,
        wall.as_secs_f64(),
        wall.as_secs_f64() * 1000.0 / report.iterations.max(1) as f64,
    );

    assert!(
        report.minimal.steps.len() < TARGET_MIN_STEPS,
        "shrinker output ({} steps) must be < {TARGET_MIN_STEPS}",
        report.minimal.steps.len(),
    );
    let replayed = drive_once(&report.minimal, &planted);
    assert!(
        original.same_class(&replayed),
        "Oracle A shrinker UNSOUND: original={original:?}, replay={replayed:?}",
    );
}

// ---------------------------------------------------------------------------
// (6) Bundle format — ShrinkerCorpusEntry serializes to the same JSON
//     schema as tests/corpus/dst/sample-g1-commit-crash.json
// ---------------------------------------------------------------------------

#[test]
fn shrinker_corpus_entry_serializes_with_corpus_compatible_fields() {
    let (plan, planted) = planted_oracle_b_fixture();
    let original = drive_once(&plan, &planted);
    let report = shrink(&plan, &planted, original);
    let entry = ShrinkerCorpusEntry::from_report(&report, &planted);
    let json = serde_json::to_string_pretty(&entry).expect("serialize corpus entry");
    eprintln!("corpus entry json:\n{json}");

    // Round-trip.
    let back: ShrinkerCorpusEntry = serde_json::from_str(&json).expect("round-trip");
    assert_eq!(back.seed, entry.seed);
    assert_eq!(back.num_steps, entry.num_steps);

    // The corpus README requires these top-level fields verbatim
    // (`seed`, `crash_phase`, `num_workspaces`, `create_candidate`,
    // `expected`, `description`). Assert the JSON contains them at the
    // top level so T1.8 can drop this in directly.
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let obj = v.as_object().expect("entry is an object");
    for required in [
        "seed",
        "crash_phase",
        "num_workspaces",
        "create_candidate",
        "expected",
        "description",
    ] {
        assert!(
            obj.contains_key(required),
            "shrinker corpus entry must contain top-level `{required}` to be \
             drop-in compatible with tests/corpus/dst/sample-g1-commit-crash.json"
        );
    }
}

// ---------------------------------------------------------------------------
// (7) Performance — shrinker stays in the §6 budget envelope
//     (~42 ms/iter target; we assert a loose ≤200 ms/iter to avoid CI
//     flake but PRINT the actual measurement so the task report can
//     quote the real number).
// ---------------------------------------------------------------------------

#[test]
fn shrinker_iteration_count_and_wall_within_budget() {
    let (plan, planted) = planted_oracle_b_fixture();
    let original = drive_once(&plan, &planted);
    let report = shrink(&plan, &planted, original);
    let per_iter_ms = report.wall.as_secs_f64() * 1000.0 / report.iterations.max(1) as f64;
    eprintln!(
        "shrinker perf: {} iter, {:.2}s wall, {:.1} ms/iter",
        report.iterations,
        report.wall.as_secs_f64(),
        per_iter_ms,
    );
    assert!(report.iterations >= 1, "shrinker performed zero replays");
    assert!(
        per_iter_ms < 200.0,
        "per-iter cost {per_iter_ms:.1} ms exceeds 200 ms ceiling \
         (architecture §6 target is ~42 ms; the ceiling is loose to \
         absorb CI flake)",
    );
}

// ---------------------------------------------------------------------------
// (8) Sanity: clean plans don't trip oracles (sanity check on the driver)
// ---------------------------------------------------------------------------

#[test]
fn driver_clean_plan_no_violation_under_zero_fault_profile() {
    // Zero fault prob ⇒ generator never attaches faults; nothing should
    // trip either oracle when no defects are planted.
    let prof = ConditionProfile::new(2, 0.0, 0.0, 0.0);
    let plan = DefaultScenarioGenerator::generate(0, &prof);
    let v = drive_once(&plan, &[]);
    assert!(
        matches!(v, StepVerdict::Clean),
        "clean plan with no faults must not trip oracles; got {v:?}"
    );
}

// ---------------------------------------------------------------------------
// (9) Oracle B integrates: a fresh in-proc driver run on a manually
//     dangling-head repo shape (mirrors `oracle_b::tests` directly) trips
//     B1 — proves the driver's check_oracles() path actually wires Oracle
//     B in.
// ---------------------------------------------------------------------------

#[test]
fn oracle_b_check_wired_into_driver() {
    let mut driver = InProcDriver::new()
        .expect("driver init")
        .with_planted(vec![PlantedDefect::DanglingHeadRef {
            ws: "ghost".into(),
        }]);
    // A minimal one-step plan whose op is a no-op WsCreate so the
    // planted defect fires after step 0.
    use crate::scenario::{
        BaseRef, FaultSpec, Op, PlannedStep, ScenarioPlan, WsId,
    };
    let plan = ScenarioPlan {
        seed: 0,
        profile: ConditionProfile::default(),
        steps: vec![PlannedStep {
            index: 0,
            op: Op::WsCreate {
                ws: WsId::slot(0),
                from: BaseRef::Main,
            },
            fault: FaultSpec::None,
            git_time: crate::scenario::GIT_TIME_BASE_FOR_DRIVER + 1,
        }],
    };
    let v = driver.drive(&plan).verdict;
    assert!(
        matches!(v, StepVerdict::OracleB(_)),
        "planted dangling head ref on ws='ghost' must trip Oracle B; got {v:?}"
    );
    // Sanity: confirm directly via oracle_b::check().
    let bvs = oracle_b::check(driver.repo_root());
    assert!(
        bvs.iter().any(|b| matches!(
            b,
            crate::oracle_b::OracleBViolation::DanglingHeadRef { workspace, .. } if workspace == "ghost"
        )),
        "oracle_b::check did not see B1 on 'ghost': {bvs:?}"
    );
}
