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
//! # Real-LLM mode (bn-1h4b)
//!
//! Passing `--real-llm --substrate=<arm>` swaps the MockAgent +
//! NoopSubstrate combo for [`maw_bench::claude::ClaudeBackend`] +
//! a real adapter from `maw-bench-adapters`. Requires
//! `--features bench,claude-backend` at build time AND
//! `MAW_BENCH_ALLOW_REAL_LLM=1` at runtime (bn-3kxq's
//! defence-in-depth gates are preserved). Use the
//! `just sg2-sweep-real` recipe for the canonical invocation.
//!
//! Default (no flags) behavior is unchanged from before bn-1h4b:
//! MockAgent + NoopSubstrate, 18 runs, byte-identical JSON.
//!
//! Exit codes:
//! - `0` — pilot completed; output printed.
//! - `2` — invalid arguments.
//! - `3` — pipeline error (driver, aggregate, or render).

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use maw_bench_sweep::{
    BackendChoice, SpectrumReportOptions, SubstrateChoice, SweepDriver, aggregate_artifacts,
    make_any_agent, pilot_grid, real_runtime::RealSubstrate, render_crossover_doc,
    render_spectrum_table, validate_pairing,
};

fn usage() -> &'static str {
    "usage: sg2-sweep-pilot [<artifact-dir>] [--real-llm] [--substrate=<arm>] [--n=<seeds-per-cell>]\n\
     \n\
     Default: runs the 18-cell sweep pilot under MockAgent + NoopSubstrate.\n\
       <artifact-dir> defaults to a tempdir under /tmp/.\n\
     \n\
     --real-llm           use ClaudeBackend (real LLM subprocess).\n\
                          requires --features claude-backend at build time +\n\
                          MAW_BENCH_ALLOW_REAL_LLM=1 at runtime.\n\
     --substrate=<arm>    one of: noop|maw|maw-consolidated|worktrees|jj.\n\
                          default: noop (Mock); maw (real-LLM).\n\
     --n=<N>              seeds per cell (default 3). Smaller = cheaper smoke run."
}

struct Args {
    dir: Option<PathBuf>,
    backend: BackendChoice,
    substrate: SubstrateChoice,
    seeds_per_cell: Option<u32>,
}

fn parse_args() -> Result<Args, String> {
    let mut dir: Option<PathBuf> = None;
    let mut backend = BackendChoice::Mock;
    let mut substrate: Option<SubstrateChoice> = None;
    let mut seeds_per_cell: Option<u32> = None;

    let argv: Vec<String> = env::args().skip(1).collect();
    for a in argv {
        if a == "-h" || a == "--help" {
            println!("{}", usage());
            std::process::exit(0);
        } else if a == "--real-llm" {
            backend = BackendChoice::Claude;
        } else if let Some(v) = a.strip_prefix("--substrate=") {
            substrate = Some(SubstrateChoice::parse(v)?);
        } else if let Some(v) = a.strip_prefix("--n=") {
            seeds_per_cell = Some(v.parse().map_err(|e| format!("--n: {e}"))?);
        } else if a.starts_with("--") {
            return Err(format!("unknown arg: {a}"));
        } else if dir.is_none() {
            dir = Some(PathBuf::from(a));
        } else {
            return Err(format!("unexpected positional arg: {a}"));
        }
    }

    // Default substrate: Noop for Mock, Maw for Claude (per bone AC §3).
    let substrate = substrate.unwrap_or(match backend {
        BackendChoice::Mock => SubstrateChoice::Noop,
        BackendChoice::Claude => SubstrateChoice::MawWsLayout,
    });
    validate_pairing(backend, substrate)?;

    Ok(Args {
        dir,
        backend,
        substrate,
        seeds_per_cell,
    })
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n\n{}", usage());
            return ExitCode::from(2);
        }
    };
    let dir: PathBuf = args
        .dir
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join(format!("sg2-sweep-pilot-{}", std::process::id())));

    let start = Instant::now();
    eprintln!("sg2-sweep-pilot: artifact_dir = {}", dir.display());
    eprintln!(
        "  backend={} substrate={}{}",
        args.backend.as_str(),
        args.substrate.as_str(),
        args.seeds_per_cell
            .map(|n| format!(" seeds_per_cell={n}"))
            .unwrap_or_default(),
    );

    // 1. Drive the pilot grid. For the Mock path, this is identical
    //    to pre-bn-1h4b behavior (byte-identical JSONs). For the
    //    real-LLM path, we use the chosen substrate adapter; the
    //    pilot_grid arm list is overridden to only run the chosen
    //    arm (otherwise we'd burn 3x the spend running 3 substrates).
    let driver = match SweepDriver::new(&dir) {
        Ok(d) => d.with_plan_steps(4).with_pinned_clock(1_000, 2_000),
        Err(e) => {
            eprintln!("driver setup: {e}");
            return ExitCode::from(3);
        }
    };
    let mut grid = pilot_grid(42);
    if matches!(args.backend, BackendChoice::Claude) {
        // Real-LLM: collapse to ONE arm matching the chosen substrate
        // (otherwise the same `pilot_grid` would run all 3 baseline arms
        // and triple the spend). The arm string the driver writes into
        // BenchRun.manifest.arm is the substrate's stable label.
        grid.arms = vec![args.substrate.as_str().to_string()];
    }
    if let Some(n) = args.seeds_per_cell {
        grid.seeds_per_cell = n;
    }
    let backend = args.backend;
    let substrate = args.substrate;
    let runs = match driver.drive(
        &grid,
        |_arm| Ok::<RealSubstrate, String>(RealSubstrate::for_choice(substrate)),
        |seed| make_any_agent(backend, seed).expect("agent factory checked at parse time"),
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

    // Aggregate total cost (sum across runs). Mock runs report None
    // (skipped); Claude runs report a per-run `total_cost_usd` from
    // the §6.4 envelope, bubbled through BenchRun.cost_usd.
    let total_cost: f64 = runs
        .iter()
        .filter_map(|r| r.cost_usd)
        .sum();
    if total_cost > 0.0 {
        eprintln!("  total_cost_usd = {total_cost:.4}");
    }

    let elapsed = start.elapsed();
    eprintln!("sg2-sweep-pilot: done in {:.2}s", elapsed.as_secs_f64());

    // Hard cap (pre-reg §3.1 pilot rule + bone HARD RULE): the
    // Mock pilot must complete well under 60s wall. Real-LLM runs
    // are intentionally slower; no warn here.
    if matches!(backend, BackendChoice::Mock) && elapsed.as_secs() > 60 {
        eprintln!(
            "WARN: pilot took >{}s, exceeds expected MockAgent budget",
            elapsed.as_secs()
        );
    }

    ExitCode::SUCCESS
}
