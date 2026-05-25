// Bench-feature gated; default-feature `cargo test` compiles to nothing.
#![cfg(feature = "bench")]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::missing_errors_doc)]

//! End-to-end pilot test: drive the pilot grid, aggregate, render.
//! Mirrors the bone's "Pilot end-to-end" acceptance test.

use std::time::Instant;

use maw_bench::{MockAgent, MockScript, NoopSubstrate};
use maw_bench_sweep::{
    aggregate_artifacts, pilot_grid, render_crossover_doc, render_spectrum_table,
    SpectrumReportOptions, SweepDriver,
};

#[test]
fn pilot_end_to_end_loads_cleanly_and_renders_with_overkill_section() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let driver = SweepDriver::new(tmp.path())
        .expect("driver")
        .with_plan_steps(4)
        .with_pinned_clock(1_000, 2_000);

    let grid = pilot_grid(42);
    let start = Instant::now();

    let runs = driver
        .drive(
            &grid,
            |_arm| Ok::<NoopSubstrate, String>(NoopSubstrate::new()),
            |_seed| MockAgent::with_pinned_clock(MockScript::finished_in_one("done"), 1_234),
        )
        .expect("drive ok");

    let elapsed = start.elapsed();
    assert!(
        elapsed.as_secs() < 60,
        "pilot took {elapsed:?} — must complete <60s in MockAgent mode"
    );

    // 18 BenchRuns: 2 cells × 3 arms × 3 seeds.
    assert_eq!(runs.len(), 18);

    let summary = aggregate_artifacts(tmp.path()).expect("aggregate");
    assert_eq!(summary.total_runs, 18);
    // 2 cells × 3 arms = 6 aggregate cells.
    assert_eq!(summary.cells.len(), 6);

    let opts = SpectrumReportOptions::default();
    let table = render_spectrum_table(&summary, &opts);
    let doc = render_crossover_doc(&summary, &opts);

    // The bone's acceptance: the overkill-regime cell must appear.
    assert!(
        doc.contains("## OVERKILL_REGIME"),
        "doc missing OVERKILL_REGIME section:\n{doc}"
    );
    assert!(
        doc.contains("## HOSTILE_REGIME"),
        "doc missing HOSTILE_REGIME section:\n{doc}"
    );
    // The C0 cell — pre-reg's overkill-region anchor — is included
    // in the spectrum table, not clipped.
    assert!(table.contains("CELL: C0"), "C0 cell missing from spectrum table:\n{table}");
}

#[test]
fn pilot_is_deterministic_given_pinned_clock_and_base_seed() {
    let tmp1 = tempfile::tempdir().expect("tempdir");
    let tmp2 = tempfile::tempdir().expect("tempdir");

    let drive = |dir: &std::path::Path| {
        let driver = SweepDriver::new(dir)
            .expect("driver")
            .with_plan_steps(4)
            .with_pinned_clock(1_000, 2_000);
        driver
            .drive(
                &pilot_grid(42),
                |_arm| Ok::<NoopSubstrate, String>(NoopSubstrate::new()),
                |_seed| MockAgent::with_pinned_clock(MockScript::finished_in_one("done"), 1_234),
            )
            .expect("drive ok")
    };
    let r1 = drive(tmp1.path());
    let r2 = drive(tmp2.path());
    assert_eq!(r1.len(), r2.len());
    for (a, b) in r1.iter().zip(r2.iter()) {
        // run_id is stable; total counters are stable; oracle_b
        // depends on substrate path so we compare per-field instead
        // of the whole struct (workspace_root is tmp-dependent).
        assert_eq!(a.run_id, b.run_id, "run_id divergence");
        assert_eq!(a.total_turns, b.total_turns);
        assert_eq!(a.total_tool_calls, b.total_tool_calls);
        assert_eq!(a.manifest.condition_id, b.manifest.condition_id);
        assert_eq!(a.manifest.arm, b.manifest.arm);
    }
}
