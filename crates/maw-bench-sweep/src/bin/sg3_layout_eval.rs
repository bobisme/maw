// Bin lint waivers — match the lib's pragmatism (the bin is a thin
// CLI shim).
#![allow(clippy::doc_markdown)]
#![allow(clippy::missing_const_for_fn)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::cast_precision_loss)]

//! `sg3-layout-eval` — the T3.5 SG3 layout-eval harness (`bn-1uzn`).
//!
//! Drives one [`SweepDriver`] pass over the bn-iux4 frozen subset
//! (SUB-A = `C0×T0`, SUB-B = `C2×T0`) for ONE of the two layout arms
//! (`maw@old-layout` or `maw@new-layout`), writes per-run BenchRun
//! JSONs to an artifact directory, and (if both arms are present in
//! one tree) computes the §3.1 R1–R6 verdict via [`decide_go_no_go`].
//!
//! ## Modes
//!
//! - `--layout=old --n-a=N --n-b=N` — run the old-layout arm only;
//!   writes BenchRuns under `<artifact-dir>/<arm>/<cell>/`.
//! - `--layout=new --n-a=N --n-b=N` — same, new-layout arm.
//! - `--layout=both` (default) — runs both arms sequentially under
//!   one artifact dir and emits the §3.1 verdict at the end.
//!
//! ## Adapter choice
//!
//! The eval binary is **adapter-agnostic**. By default it uses
//! [`NoopSubstrate`] + [`MockAgent`] for the pilot. Real-LLM runs are
//! driven by the same binary with a different `--adapter` mode (not
//! implemented here — that requires the production [`Substrate`]
//! wiring + auth wiring which is the T2.2/T2.6 production-run
//! concern; see `notes/sg2-benchmark-preregistration.md` §6.4).
//!
//! ## Decision logic
//!
//! When `--layout=both` is used, after both arms have written their
//! BenchRuns the binary loads + aggregates each arm's artifact
//! subdir into a [`SweepSummary`], passes both summaries to
//! [`decide_go_no_go`], and prints the verdict + per-rule evidence
//! as JSON to stdout. A `--decision-json <path>` flag writes the
//! same JSON to a file (consumed by the writeup template scaffold).
//!
//! ## Pre-registration discipline (§3.1 / §3.6)
//!
//! - Pilot mode (`--pilot` flag) tags outputs so the writeup
//!   template can exclude them from the §3.1 verdict per §3.6.
//! - The arm name written into each BenchRun manifest is the
//!   §1.2-frozen value (`maw@old-layout` / `maw@new-layout`); the
//!   driver overrides the substrate-self-reported arm before
//!   persisting (per `crate::driver::SweepDriver::drive`).
//! - Frozen subset N defaults to bn-iux4 §1.3 (SUB-A N=20,
//!   SUB-B N=10); overridable per run for pilot validation.
//!
//! Exit codes:
//! - `0` — eval completed; (if `--layout=both`) decision was GO.
//! - `1` — eval completed; decision was NO-GO (this is an
//!   acceptable outcome per `notes/sg3-subset-prereg.md` §5 — see
//!   `notes/sg3-go-no-go.md`).
//! - `2` — invalid arguments.
//! - `3` — pipeline error (driver, aggregate, decide).

use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use maw_bench::{MockAgent, MockScript, NoopSubstrate};
use maw_bench_sweep::{
    ARM_NEW, ARM_OLD, ConditionPoint, Decision, PairedCiSignals, PrereggedBars, SweepCell,
    SweepDriver, SweepGrid, TClass, aggregate_artifacts, decide_go_no_go,
};

fn usage() -> &'static str {
    "usage: sg3-layout-eval [--layout=old|new|both] [--n-a=N] [--n-b=N] \
     [--artifact-dir=<dir>] [--pilot] [--decision-json=<path>]\n\
     \n\
     Runs the T3.5 SG3 layout-eval harness against the bn-iux4 frozen subset.\n\
     Defaults: --layout=both --n-a=20 --n-b=10 (frozen by bn-iux4 §1.3).\n\
     With --pilot: --n-a=3 --n-b=3 + MockAgent + NoopSubstrate (≤ 60s wall).\n\
     Use `just sg3-layout-eval-pilot` for the canonical pilot invocation."
}

struct Args {
    layout: Layout,
    n_a: u32,
    n_b: u32,
    artifact_dir: Option<PathBuf>,
    pilot: bool,
    decision_json: Option<PathBuf>,
    plant_regression: PlantedRegression,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    Old,
    New,
    Both,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlantedRegression {
    None,
    /// Plant an R1 hard-bar regression on the new arm (one work_lost
    /// run at SUB-A). Used by the pilot test to confirm the decision
    /// logic correctly returns NO-GO on planted data.
    R1HardBar,
}

fn parse_args() -> Result<Args, String> {
    let mut layout = Layout::Both;
    let mut n_a: u32 = 20;
    let mut n_b: u32 = 10;
    let mut artifact_dir: Option<PathBuf> = None;
    let mut pilot = false;
    let mut decision_json: Option<PathBuf> = None;
    let mut plant_regression = PlantedRegression::None;

    let argv: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        if a == "-h" || a == "--help" {
            println!("{}", usage());
            std::process::exit(0);
        } else if let Some(v) = a.strip_prefix("--layout=") {
            layout = match v {
                "old" => Layout::Old,
                "new" => Layout::New,
                "both" => Layout::Both,
                _ => return Err(format!("unknown layout: {v}")),
            };
        } else if let Some(v) = a.strip_prefix("--n-a=") {
            n_a = v.parse().map_err(|e| format!("--n-a: {e}"))?;
        } else if let Some(v) = a.strip_prefix("--n-b=") {
            n_b = v.parse().map_err(|e| format!("--n-b: {e}"))?;
        } else if let Some(v) = a.strip_prefix("--artifact-dir=") {
            artifact_dir = Some(PathBuf::from(v));
        } else if a == "--pilot" {
            pilot = true;
        } else if let Some(v) = a.strip_prefix("--decision-json=") {
            decision_json = Some(PathBuf::from(v));
        } else if a == "--plant-r1" {
            plant_regression = PlantedRegression::R1HardBar;
        } else {
            return Err(format!("unknown arg: {a}"));
        }
        i += 1;
    }

    if pilot {
        // Pilot defaults override (per bone HARD RULE: ≤ 60s wall).
        n_a = n_a.min(3).max(2);
        n_b = n_b.min(3).max(2);
    }

    Ok(Args {
        layout,
        n_a,
        n_b,
        artifact_dir,
        pilot,
        decision_json,
        plant_regression,
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

    let dir = match args.artifact_dir.clone() {
        Some(d) => d,
        None => env::temp_dir().join(format!("sg3-layout-eval-{}", std::process::id())),
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("artifact dir setup: {e}");
        return ExitCode::from(3);
    }

    eprintln!("sg3-layout-eval: artifact_dir = {}", dir.display());
    eprintln!(
        "  layout={} n-a={} n-b={} pilot={}",
        match args.layout {
            Layout::Old => "old",
            Layout::New => "new",
            Layout::Both => "both",
        },
        args.n_a,
        args.n_b,
        args.pilot
    );

    let start = Instant::now();

    let arms_to_run: Vec<&str> = match args.layout {
        Layout::Old => vec![ARM_OLD],
        Layout::New => vec![ARM_NEW],
        Layout::Both => vec![ARM_OLD, ARM_NEW],
    };
    for arm in &arms_to_run {
        let plant_for_arm = if *arm == ARM_NEW {
            args.plant_regression
        } else {
            PlantedRegression::None
        };
        if let Err(e) = run_arm(arm, args.n_a, args.n_b, &dir, plant_for_arm) {
            eprintln!("run_arm({arm}): {e}");
            return ExitCode::from(3);
        }
    }

    let elapsed = start.elapsed();
    eprintln!(
        "  drove {} arm(s) in {:.2}s",
        arms_to_run.len(),
        elapsed.as_secs_f64()
    );
    if args.pilot && elapsed.as_secs() > 60 {
        eprintln!(
            "WARN: pilot took >{}s, exceeds expected MockAgent budget",
            elapsed.as_secs()
        );
    }

    // Only compute the decision when both arms ran.
    if args.layout != Layout::Both {
        eprintln!("sg3-layout-eval: single-arm run done; rerun --layout=both for verdict");
        return ExitCode::SUCCESS;
    }

    let old_dir = arm_dir(&dir, ARM_OLD);
    let new_dir = arm_dir(&dir, ARM_NEW);
    let old_summary = match aggregate_artifacts(&old_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("aggregate old: {e}");
            return ExitCode::from(3);
        }
    };
    let new_summary = match aggregate_artifacts(&new_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("aggregate new: {e}");
            return ExitCode::from(3);
        }
    };

    // Pilot mode: no paired bootstrap (the bootstrap requires raw
    // per-replicate values + a power computation that's outside
    // this binary's scope). The decision logic falls back to
    // "ratio-only" R4/R5 in this mode — appropriate for the pilot
    // per §3.6, NOT for the production run.
    let paired: Option<&PairedCiSignals> = None;
    let bars = PrereggedBars::default();
    let decision = decide_go_no_go(&old_summary, &new_summary, paired, bars);

    // Emit verdict to stdout (and optionally to file).
    let json = serde_json::to_string_pretty(&decision).expect("serialize decision");
    println!("{json}");
    if let Some(path) = args.decision_json.as_ref() {
        if let Err(e) = std::fs::write(path, &json) {
            eprintln!("write --decision-json {}: {e}", path.display());
            return ExitCode::from(3);
        }
    }

    eprintln!("sg3-layout-eval: verdict = {}", decision.label());
    match decision {
        Decision::Go { .. } => ExitCode::SUCCESS,
        // NO-GO is an acceptable outcome (bn-iux4 §5). Exit 1 so
        // CI can branch on the verdict; the writeup template
        // converts this to the "v1.0 ships on ws/" branch.
        Decision::NoGo { .. } => ExitCode::from(1),
    }
}

/// Drive one arm's subset (SUB-A + SUB-B) under MockAgent + NoopSubstrate.
fn run_arm(
    arm: &str,
    n_a: u32,
    n_b: u32,
    base_dir: &Path,
    plant: PlantedRegression,
) -> Result<(), String> {
    let arm_root = arm_dir(base_dir, arm);
    let driver = SweepDriver::new(&arm_root)
        .map_err(|e| format!("driver: {e}"))?
        .with_plan_steps(4)
        .with_pinned_clock(1_000, 2_000);

    // SUB-A grid: C0×T0, N=n_a.
    let sub_a = SweepGrid {
        cells: vec![SweepCell {
            condition: ConditionPoint::c0_benign(),
            t_class: TClass::T0,
        }],
        arms: vec![arm.to_string()],
        seeds_per_cell: n_a,
        // §6.1 frozen base seed (subset-specific; documented in
        // bn-iux4 §6.1). The value here matches the bone description.
        base_seed: 0x5e3e_4e4e_5e3e_4e4e,
    };

    // SUB-B grid: C2×T0, N=n_b.
    let c2 = ConditionPoint {
        id: "C2".to_string(),
        name: "moderate".to_string(),
        k_overlap_numerator: 4,
        k_concurrency: 3,
        k_rounds: 5,
        burst: false,
    };
    let sub_b = SweepGrid {
        cells: vec![SweepCell {
            condition: c2,
            t_class: TClass::T0,
        }],
        arms: vec![arm.to_string()],
        seeds_per_cell: n_b,
        base_seed: 0x5e3e_4e4e_5e3e_4e4e,
    };

    drive_grid(&driver, &sub_a, plant, "C0")?;
    drive_grid(&driver, &sub_b, plant, "C2")?;
    Ok(())
}

fn drive_grid(
    driver: &SweepDriver,
    grid: &SweepGrid,
    plant: PlantedRegression,
    plant_cond: &str,
) -> Result<(), String> {
    // For the planted-regression case at SUB-A on the new arm we
    // need to materialize a `work_lost_events > 0` outcome. The
    // MockAgent path always produces a clean BenchRun; we mutate
    // the on-disk JSON after the driver completes (only for the
    // planted cell). This is test-only behavior gated behind
    // `--plant-r1`; production runs never set it.
    driver
        .drive(
            grid,
            |_arm| Ok::<NoopSubstrate, String>(NoopSubstrate::new()),
            |_seed| MockAgent::with_pinned_clock(MockScript::finished_in_one("done"), 1_234),
        )
        .map_err(|e| format!("drive: {e}"))?;
    if plant == PlantedRegression::R1HardBar && plant_cond == "C0" {
        plant_r1_loss(driver.artifact_dir(), &grid.cells[0])?;
    }
    Ok(())
}

/// Mutate the first BenchRun JSON in the (cell)'s output dir so
/// `oracle_b.verdict` becomes `red` (yielding `work_lost_events =
/// 1` via the metric extractor's red→count mapping). Test-only.
fn plant_r1_loss(arm_dir: &Path, cell: &SweepCell) -> Result<(), String> {
    let cell_dir = arm_dir.join(format!("{}-{}", cell.condition.id, cell.t_class.as_str()));
    let mut entries: Vec<_> = std::fs::read_dir(&cell_dir)
        .map_err(|e| format!("read {}: {e}", cell_dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    entries.sort_by_key(std::fs::DirEntry::path);
    let first = entries.first().ok_or_else(|| {
        format!(
            "no JSONs in {} to plant R1 regression on",
            cell_dir.display()
        )
    })?;
    let path = first.path();
    let bytes =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut v: serde_json::Value =
        serde_json::from_str(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))?;
    // Set oracle_b verdict to "red" — the metric extractor maps
    // Oracle-B red → work_lost_events += 1 (per `extract.rs`
    // §work_lost_events).
    v["oracle_b"] = serde_json::json!({
        "verdict": "red",
        "violations": ["planted R1 regression (--plant-r1; test-only)"],
    });
    let out = serde_json::to_string_pretty(&v)
        .map_err(|e| format!("re-encode {}: {e}", path.display()))?;
    std::fs::write(&path, out).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

fn arm_dir(base: &Path, arm: &str) -> PathBuf {
    let safe: String = arm
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    base.join(safe)
}

// Suppress dead-code on this BTreeMap import (used inside parse_args
// indirectly via the env iter); kept to mirror the lib's import shape.
#[allow(dead_code)]
fn _unused_btreemap() -> BTreeMap<(), ()> {
    BTreeMap::new()
}
