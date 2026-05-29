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
    SpectrumReportOptions, SweepDriver, aggregate_artifacts, pilot_grid, render_crossover_doc,
    render_spectrum_table, spectrum_grid,
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
    assert!(
        table.contains("CELL: C0"),
        "C0 cell missing from spectrum table:\n{table}"
    );
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

/// bn-205s: drive the spectrum_grid end-to-end (the same call path
/// `sg2-sweep-pilot --grid=spectrum` exercises). Asserts the full
/// §5.1 10-cell schedule is materialized (5 T0 cells across C0..C4
/// + 5 T1..T5 chaos overlays at C2) and every cell is represented in
/// the per-arm aggregate, so the spectrum table renders the full grid.
#[test]
fn spectrum_grid_drives_ten_cells_end_to_end() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let driver = SweepDriver::new(tmp.path())
        .expect("driver")
        .with_plan_steps(4)
        .with_pinned_clock(1_000, 2_000);

    // Match the `--grid=spectrum` defaults the binary uses: seed=42,
    // seeds_per_cell=10. We shrink to seeds_per_cell=1 here purely
    // to keep wall time well under the 60s MockAgent budget; the
    // *cell schedule* is what bn-205s adds and what we assert against.
    let mut grid = spectrum_grid(42, 10);
    grid.seeds_per_cell = 1;

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
        "spectrum drove in {elapsed:?} — must stay <60s under MockAgent"
    );

    // 10 cells × 4 arms × 1 seed = 40 runs (vs. pilot's 18).
    assert_eq!(runs.len(), 40);
    // 5 unique conditions: C0..C4.
    let conditions: std::collections::BTreeSet<_> = runs
        .iter()
        .map(|r| r.manifest.condition_id.clone())
        .collect();
    assert_eq!(conditions.len(), 5, "expected C0..C4, got {conditions:?}");
    // 6 unique T-classes: T0..T5.
    let t_classes: std::collections::BTreeSet<_> =
        runs.iter().map(|r| r.manifest.t_class.clone()).collect();
    assert_eq!(t_classes.len(), 6, "expected T0..T5, got {t_classes:?}");
    // C2 is the chaos pivot: T0 + T1..T5 = 6 distinct (cond, t)
    // combinations all sitting on C2.
    let c2_combos: std::collections::BTreeSet<_> = runs
        .iter()
        .filter(|r| r.manifest.condition_id == "C2")
        .map(|r| r.manifest.t_class.clone())
        .collect();
    assert_eq!(
        c2_combos.len(),
        6,
        "C2 must carry T0 + T1..T5, got {c2_combos:?}"
    );

    let summary = aggregate_artifacts(tmp.path()).expect("aggregate");
    // 10 grid cells × 4 arms = 40 aggregate cells.
    assert_eq!(summary.cells.len(), 40);
    assert_eq!(summary.arms.len(), 4);
    assert_eq!(summary.conditions.len(), 5);

    // Renders cleanly (no crash, contains the table header).
    let opts = SpectrumReportOptions::default();
    let table = render_spectrum_table(&summary, &opts);
    assert!(
        table.contains("CELL: C0"),
        "spectrum table missing C0 row:\n{table}"
    );
    assert!(
        table.contains("CELL: C4"),
        "spectrum table missing C4 row:\n{table}"
    );
}
