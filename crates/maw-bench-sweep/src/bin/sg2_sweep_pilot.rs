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
    check_maw_version_skew, make_any_agent, pilot_grid, real_runtime::RealSubstrate,
    render_crossover_doc, render_spectrum_table, spectrum_grid, validate_pairing,
};

fn usage() -> &'static str {
    "usage: sg2-sweep-pilot [<artifact-dir>] [--grid=pilot|spectrum] \
     [--real-llm] [--substrate=<arm>] [--n=<seeds-per-cell>]\n\
     \n\
     Default: runs the 18-cell sweep pilot under MockAgent + NoopSubstrate.\n\
       <artifact-dir> defaults to a tempdir under /tmp/.\n\
     \n\
     --grid=<g>           which grid to drive (default: pilot — back-compat).\n\
                          pilot:    2 cells (C0 + C4 endpoints), 3 arms, 3 seeds.\n\
                          spectrum: 10 cells (5 T0 across C0..C4 + 5 T1..T5 at C2),\n\
                                    4 arms (publication order). Seeds_per_cell\n\
                                    defaults to 10 unless --n=<N> is set.\n\
     --real-llm           use ClaudeBackend (real LLM subprocess).\n\
                          requires --features claude-backend at build time +\n\
                          MAW_BENCH_ALLOW_REAL_LLM=1 at runtime.\n\
     --substrate=<arm>    one of: noop|maw|maw-consolidated|worktrees|jj.\n\
                          default: noop (Mock); maw (real-LLM).\n\
     --n=<N>              seeds per cell. Pilot default: 3. Spectrum default: 10."
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GridChoice {
    Pilot,
    Spectrum,
}

impl GridChoice {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "pilot" => Ok(Self::Pilot),
            "spectrum" => Ok(Self::Spectrum),
            other => Err(format!("--grid: unknown value `{other}` (want pilot|spectrum)")),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Pilot => "pilot",
            Self::Spectrum => "spectrum",
        }
    }
}

struct Args {
    dir: Option<PathBuf>,
    backend: BackendChoice,
    substrate: SubstrateChoice,
    seeds_per_cell: Option<u32>,
    grid: GridChoice,
    /// bn-3w0c: optional `--model=<id>` override (defaults to `AgentConfig::default()`).
    model: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut dir: Option<PathBuf> = None;
    let mut backend = BackendChoice::Mock;
    let mut substrate: Option<SubstrateChoice> = None;
    let mut seeds_per_cell: Option<u32> = None;
    let mut grid = GridChoice::Pilot;
    let mut model: Option<String> = None;

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
        } else if let Some(v) = a.strip_prefix("--grid=") {
            grid = GridChoice::parse(v)?;
        } else if let Some(v) = a.strip_prefix("--model=") {
            model = Some(v.to_string());
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
        grid,
        model,
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
    // bn-f5zu: preflight binary-vs-source version skew before doing
    // any work. Warning-only — see `notes/sg3-no-go-rootcause.md` for
    // the version-skew root cause that motivated this guard.
    let _ = check_maw_version_skew(env!("CARGO_PKG_VERSION"));

    let dir: PathBuf = args
        .dir
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join(format!("sg2-sweep-pilot-{}", std::process::id())));

    let start = Instant::now();
    eprintln!("sg2-sweep-pilot: artifact_dir = {}", dir.display());
    // Preserve pre-bn-205s stderr shape exactly for the default
    // (--grid=pilot) path; only print the grid label when the user
    // opts into a non-default grid. Keeps `just sg2-sweep-pilot`
    // byte-identical with prior releases.
    if matches!(args.grid, GridChoice::Spectrum) {
        eprintln!("  grid={}", args.grid.as_str());
    }
    eprintln!(
        "  backend={} substrate={}{}{}",
        args.backend.as_str(),
        args.substrate.as_str(),
        args.seeds_per_cell
            .map(|n| format!(" seeds_per_cell={n}"))
            .unwrap_or_default(),
        args.model
            .as_deref()
            .map(|m| format!(" model={m}"))
            .unwrap_or_default(),
    );

    // 1. Drive the selected grid. The default `--grid=pilot` path is
    //    identical to pre-bn-205s behavior (byte-identical JSONs).
    //    `--grid=spectrum` exposes the full 10-cell §5.1 schedule
    //    (5 T0 across C0..C4 + 5 T1..T5 at C2) defined in
    //    `grid::spectrum_grid`. Default seeds_per_cell for spectrum is
    //    10 per pre-reg §6.1 headline N; pilot keeps 3 per `pilot_grid`.
    //    For the real-LLM path, the arm list is collapsed to ONE arm
    //    matching the chosen substrate (otherwise we'd burn 3-4× the
    //    spend running every baseline arm).
    let agent_cfg_override = args.model.as_ref().map(|m| {
        maw_bench::agent::AgentConfig {
            model: m.clone(),
            ..maw_bench::agent::AgentConfig::default()
        }
    });
    let driver = match SweepDriver::new(&dir) {
        Ok(d) => d
            .with_plan_steps(4)
            .with_pinned_clock(1_000, 2_000)
            .with_agent_config(agent_cfg_override),
        Err(e) => {
            eprintln!("driver setup: {e}");
            return ExitCode::from(3);
        }
    };
    let mut grid = match args.grid {
        GridChoice::Pilot => pilot_grid(42),
        // Default spectrum N=10 per pre-reg §6.1 headline; overridable
        // by `--n=<N>` below (applied uniformly to both grids).
        GridChoice::Spectrum => spectrum_grid(42, 10),
    };
    if matches!(args.grid, GridChoice::Spectrum) {
        // Useful at-a-glance count for the spectrum sweep; suppressed
        // for the pilot path to preserve byte-identical stderr.
        eprintln!("  grid cells: {}", grid.cells.len());
    }
    if matches!(args.backend, BackendChoice::Claude) {
        // Real-LLM: collapse to ONE arm matching the chosen substrate
        // (otherwise the same grid would run all baseline arms and
        // multiply the spend). The arm string the driver writes into
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_choice_parse_accepts_pilot_and_spectrum() {
        assert_eq!(GridChoice::parse("pilot").unwrap(), GridChoice::Pilot);
        assert_eq!(
            GridChoice::parse("spectrum").unwrap(),
            GridChoice::Spectrum
        );
    }

    #[test]
    fn grid_choice_parse_rejects_unknown_value() {
        let err = GridChoice::parse("full").unwrap_err();
        assert!(
            err.contains("pilot|spectrum"),
            "error should name valid values: {err}"
        );
    }

    #[test]
    fn grid_choice_pilot_drives_two_cells() {
        // The default --grid=pilot path must keep driving the 2-cell
        // pilot grid (back-compat with pre-bn-205s callers).
        let g = pilot_grid(42);
        assert_eq!(g.cells.len(), 2);
    }

    #[test]
    fn grid_choice_spectrum_drives_ten_cells() {
        // --grid=spectrum must drive the full §5.1 10-cell schedule.
        let g = spectrum_grid(42, 10);
        assert_eq!(g.cells.len(), 10);
    }
}
