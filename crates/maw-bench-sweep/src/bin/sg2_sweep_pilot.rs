// Bin lint waivers — match the lib's pragmatism (the bin is a thin
// CLI shim).
#![allow(clippy::doc_markdown)]
#![allow(clippy::missing_const_for_fn)]

//! `sg2-sweep-pilot` — harness-only validation pilot for the SG2
//! sweep driver + aggregator + crossover analysis.
//!
//! Runs a tiny grid (2 cells × 3 substrates × 3 seeds = 18
//! BenchRuns) under [`maw_bench::NoopSubstrate`] +
//! [`maw_bench::MockAgent`], aggregates the results, classifies the
//! crossover, and prints the spectrum table + crossover doc to
//! stdout. Writes nothing outside the supplied artifact dir.
//!
//! Per pre-reg §3.1 Pilot rule: this run is HARNESS-ONLY data.
//! Numbers from this binary MUST NOT be used to set bars or
//! support publication claims; the binary exists to confirm the
//! pipeline writes/aggregates/renders end-to-end. The
//! `just sg2-sweep-pilot` recipe is the developer-facing entry
//! point; CI exercises the same code path via
//! `tests/sweep_pilot.rs`.
//!
//! Exit codes:
//! - `0` — pilot completed; output printed.
//! - `2` — invalid arguments.
//! - `3` — pipeline error (driver, aggregate, or render).

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use maw_bench::{MockAgent, MockScript, NoopSubstrate};
use maw_bench_sweep::{
    SpectrumReportOptions, SweepDriver, aggregate_artifacts, pilot_grid, render_crossover_doc,
    render_spectrum_table,
};

fn usage() -> &'static str {
    "usage: sg2-sweep-pilot [<artifact-dir>]\n\
     runs the 18-cell sweep pilot under MockAgent + NoopSubstrate.\n\
     <artifact-dir> defaults to a tempdir under /tmp/."
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let dir: PathBuf = match args.next() {
        Some(a) if a == "--help" || a == "-h" => {
            println!("{}", usage());
            return ExitCode::SUCCESS;
        }
        Some(a) => PathBuf::from(a),
        None => std::env::temp_dir().join(format!("sg2-sweep-pilot-{}", std::process::id())),
    };

    let start = Instant::now();
    eprintln!("sg2-sweep-pilot: artifact_dir = {}", dir.display());

    // 1. Drive the pilot grid under deterministic MockAgent +
    //    NoopSubstrate. The pinned clock keeps BenchRun JSON byte-
    //    identical across invocations (the smoke check tests rely
    //    on this).
    let driver = match SweepDriver::new(&dir) {
        Ok(d) => d.with_plan_steps(4).with_pinned_clock(1_000, 2_000),
        Err(e) => {
            eprintln!("driver setup: {e}");
            return ExitCode::from(3);
        }
    };
    let grid = pilot_grid(42);
    let runs = match driver.drive(
        &grid,
        |_arm| Ok::<NoopSubstrate, String>(NoopSubstrate::new()),
        |_seed| MockAgent::with_pinned_clock(MockScript::finished_in_one("done"), 1_234),
    ) {
        Ok(rs) => rs,
        Err(e) => {
            eprintln!("driver: {e}");
            return ExitCode::from(3);
        }
    };
    eprintln!("  drove {} runs", runs.len());

    // 2. Aggregate.
    let summary = match aggregate_artifacts(&dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("aggregate: {e}");
            return ExitCode::from(3);
        }
    };
    eprintln!(
        "  aggregated {} cells across {} arms × {} conditions",
        summary.cells.len(),
        summary.arms.len(),
        summary.conditions.len()
    );

    // 3. Render. We deliberately stamp the publication arm order
    //    even though the pilot only runs 3 of the 4 (so the renderer
    //    code path is exercised the way the real-run will be).
    let opts = SpectrumReportOptions {
        arm_order: Some(
            maw_bench_sweep::ARMS_PUBLICATION
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        ),
        ..SpectrumReportOptions::default()
    };

    println!("{}", render_spectrum_table(&summary, &opts));
    println!("\n----- crossover-summary.md (scaffold) -----\n");
    println!("{}", render_crossover_doc(&summary, &opts));

    let elapsed = start.elapsed();
    eprintln!("sg2-sweep-pilot: done in {:.2}s", elapsed.as_secs_f64());

    // Hard cap (pre-reg §3.1 pilot rule + bone HARD RULE): the
    // pilot must complete well under 60s wall in MockAgent mode.
    if elapsed.as_secs() > 60 {
        eprintln!(
            "WARN: pilot took >{}s, exceeds expected MockAgent budget",
            elapsed.as_secs()
        );
    }

    ExitCode::SUCCESS
}
