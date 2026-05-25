// Bench-feature gated; default-feature `cargo test` compiles to nothing.
#![cfg(feature = "bench")]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::missing_errors_doc)]

//! T3.5 / `bn-1uzn` pilot end-to-end test.
//!
//! Drives the same `SweepDriver` pass the production-mode
//! `sg3-layout-eval` binary will drive, but with MockAgent +
//! NoopSubstrate and a pilot-sized N. Asserts:
//!
//! 1. Identical substrates produce identical aggregates per arm ⇒
//!    [`decide_go_no_go`] returns [`Decision::Go`] (the "GO when
//!    layouts are equivalent" pilot acceptance).
//! 2. A planted R1 regression (one `work_lost_events > 0` on the
//!    new arm at SUB-A) produces [`Decision::NoGo`] with
//!    `regression_rule = "R1"` and `regression_metric =
//!    "irrecoverable_lost_work"` (the "NO-GO when planted
//!    regression" pilot acceptance).
//! 3. Both passes complete in < 60s on MockAgent / NoopSubstrate
//!    fidelity (bone HARD RULE).
//!
//! These tests are the harness-validation pilot per
//! `notes/sg3-subset-prereg.md` §3.6: they confirm the gate's
//! decision logic wires up correctly. They DO NOT set any bar and
//! DO NOT contribute to the real-run go/no-go writeup.

use std::time::Instant;

use maw_bench::{MockAgent, MockScript, NoopSubstrate};
use maw_bench_sweep::{
    aggregate_artifacts, decide_go_no_go, ConditionPoint, Decision, PrereggedBars, SweepCell,
    SweepDriver, SweepGrid, TClass, ARM_NEW, ARM_OLD,
};

/// One-arm subset driver shared between both pilot tests.
fn drive_arm(arm: &str, base: &std::path::Path, n_a: u32, n_b: u32) {
    let arm_safe: String = arm
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let arm_dir = base.join(arm_safe);
    let driver = SweepDriver::new(&arm_dir)
        .expect("driver")
        .with_plan_steps(4)
        .with_pinned_clock(1_000, 2_000);
    let sub_a = SweepGrid {
        cells: vec![SweepCell {
            condition: ConditionPoint::c0_benign(),
            t_class: TClass::T0,
        }],
        arms: vec![arm.to_string()],
        seeds_per_cell: n_a,
        base_seed: 0x5e3e_4e4e_5e3e_4e4e,
    };
    let sub_b = SweepGrid {
        cells: vec![SweepCell {
            condition: ConditionPoint {
                id: "C2".to_string(),
                name: "moderate".to_string(),
                k_overlap_numerator: 4,
                k_concurrency: 3,
                k_rounds: 5,
                burst: false,
            },
            t_class: TClass::T0,
        }],
        arms: vec![arm.to_string()],
        seeds_per_cell: n_b,
        base_seed: 0x5e3e_4e4e_5e3e_4e4e,
    };
    for g in [&sub_a, &sub_b] {
        driver
            .drive(
                g,
                |_arm| Ok::<NoopSubstrate, String>(NoopSubstrate::new()),
                |_seed| MockAgent::with_pinned_clock(MockScript::finished_in_one("done"), 1_234),
            )
            .expect("drive ok");
    }
}

/// Plant an R1 hard-bar regression: mutate the first BenchRun JSON
/// on the new arm's SUB-A cell to report Oracle-B Red.
fn plant_r1_loss_in(base: &std::path::Path) {
    let arm_safe = ARM_NEW.replace('@', "-");
    let cell_dir = base.join(arm_safe).join("C0-T0");
    let mut entries: Vec<_> = std::fs::read_dir(&cell_dir)
        .expect("read cell dir")
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    entries.sort_by_key(std::fs::DirEntry::path);
    let path = entries.first().expect("at least one BenchRun").path();
    let bytes = std::fs::read_to_string(&path).expect("read");
    let mut v: serde_json::Value = serde_json::from_str(&bytes).expect("parse");
    v["oracle_b"] = serde_json::json!({
        "verdict": "red",
        "violations": ["planted R1 regression (pilot test)"],
    });
    let out = serde_json::to_string_pretty(&v).expect("re-encode");
    std::fs::write(&path, out).expect("write");
}

#[test]
fn pilot_identical_substrates_returns_go() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let start = Instant::now();

    drive_arm(ARM_OLD, tmp.path(), 3, 3);
    drive_arm(ARM_NEW, tmp.path(), 3, 3);

    let elapsed = start.elapsed();
    assert!(
        elapsed.as_secs() < 60,
        "pilot took {elapsed:?} — must complete <60s in MockAgent mode"
    );

    let arm_safe_old = ARM_OLD.replace('@', "-");
    let arm_safe_new = ARM_NEW.replace('@', "-");
    let old_summary = aggregate_artifacts(&tmp.path().join(arm_safe_old)).expect("aggregate old");
    let new_summary = aggregate_artifacts(&tmp.path().join(arm_safe_new)).expect("aggregate new");

    // Sanity: 2 cells × 3 reps = 6 runs per arm.
    assert_eq!(old_summary.total_runs, 6);
    assert_eq!(new_summary.total_runs, 6);

    let d = decide_go_no_go(&old_summary, &new_summary, None, PrereggedBars::default());
    match d {
        Decision::Go { evidence } => {
            // 6 rules × 2 cells = 12 EvaluatedRule rows.
            assert_eq!(evidence.rules.len(), 12);
            assert!(evidence.all_passed(), "evidence: {evidence:#?}");
        }
        Decision::NoGo {
            regression_rule,
            regression_metric,
            by_amount,
            ..
        } => panic!(
            "expected GO on identical pilot substrates; got NO-GO at {regression_rule} \
             ({regression_metric}): {by_amount}"
        ),
    }
}

#[test]
fn pilot_planted_r1_regression_returns_no_go_naming_r1() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let start = Instant::now();

    drive_arm(ARM_OLD, tmp.path(), 3, 3);
    drive_arm(ARM_NEW, tmp.path(), 3, 3);
    plant_r1_loss_in(tmp.path());

    let elapsed = start.elapsed();
    assert!(
        elapsed.as_secs() < 60,
        "pilot took {elapsed:?} — must complete <60s in MockAgent mode"
    );

    let arm_safe_old = ARM_OLD.replace('@', "-");
    let arm_safe_new = ARM_NEW.replace('@', "-");
    let old_summary = aggregate_artifacts(&tmp.path().join(arm_safe_old)).expect("aggregate old");
    let new_summary = aggregate_artifacts(&tmp.path().join(arm_safe_new)).expect("aggregate new");

    let d = decide_go_no_go(&old_summary, &new_summary, None, PrereggedBars::default());
    match d {
        Decision::Go { .. } => {
            panic!("expected NO-GO; planted R1 regression should trip the hard bar: {d:#?}")
        }
        Decision::NoGo {
            regression_rule,
            regression_metric,
            by_amount,
            ..
        } => {
            assert_eq!(regression_rule, "R1");
            assert_eq!(regression_metric, "irrecoverable_lost_work");
            assert!(
                by_amount.contains("work_lost_events = 1"),
                "by_amount: {by_amount}"
            );
        }
    }
}
