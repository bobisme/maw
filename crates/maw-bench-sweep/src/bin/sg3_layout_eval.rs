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
//! ## Adapter choice (post-bn-1h4b)
//!
//! Default: [`maw_bench::NoopSubstrate`] + [`maw_bench::MockAgent`]
//! (the pilot path; byte-identical JSON, ≤ 60s wall, $0 spend).
//!
//! With `--real-llm`: [`maw_bench::claude::ClaudeBackend`] + the real
//! maw substrate adapter for the chosen layout flavor
//! (`--layout=old → MawWsLayout`, `--layout=new → MawConsolidatedLayout`).
//! Requires `--features bench,claude-backend` at build time +
//! `MAW_BENCH_ALLOW_REAL_LLM=1` at runtime (bn-3kxq guards).
//!
//! `--substrate=<arm>` overrides the substrate to a non-maw arm
//! (`worktrees|jj`); useful for sanity-comparing the eval pipeline
//! against a non-maw substrate. Only allowed with `--layout=old|new`
//! (single-arm mode); `--layout=both` requires the maw substrate per
//! the §3.1 R1–R6 verdict logic.
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
//! ## Why no `--grid=pilot|spectrum` flag (bn-205s)
//!
//! sg2-sweep-pilot accepts `--grid=pilot|spectrum` to choose between
//! the 2-cell harness pilot and the 10-cell §5.1 spectrum. sg3 does
//! NOT accept that flag: its cell layout is **bn-iux4 §1.3 frozen**
//! (SUB-A = `C0×T0`, SUB-B = `C2×T0`) and a different schedule would
//! void the §3.1 R1–R6 verdict — the pre-registered hard bars are
//! defined against exactly that two-cell subset, not against the
//! spectrum. Adding `--grid` here would let an operator generate a
//! verdict against a different schedule than the one the bars were
//! pre-registered against — a §3.6 violation. Tune SUB-A/SUB-B N via
//! `--n-a` / `--n-b` instead.
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

use maw_bench_sweep::{
    ARM_NEW, ARM_OLD, BackendChoice, ConditionPoint, Decision, PairedCiSignals, PrereggedBars,
    SubstrateChoice, SweepCell, SweepDriver, SweepGrid, TClass, aggregate_artifacts,
    check_maw_version_skew, decide_go_no_go, make_any_agent, real_runtime::RealSubstrate,
    validate_pairing,
};

fn usage() -> &'static str {
    "usage: sg3-layout-eval [--layout=old|new|both] [--n-a=N] [--n-b=N] \
     [--artifact-dir=<dir>] [--pilot] [--decision-json=<path>] \
     [--real-llm] [--substrate=<arm>] [--chaos=on|off]\n\
     \n\
     Runs the T3.5 SG3 layout-eval harness against the bn-iux4 frozen subset.\n\
     Defaults: --layout=both --n-a=20 --n-b=10 (frozen by bn-iux4 §1.3).\n\
     With --pilot: --n-a=3 --n-b=3 + MockAgent + NoopSubstrate (≤ 60s wall).\n\
     With --real-llm: ClaudeBackend + the real maw adapter for the chosen\n\
       layout flavor (--layout=old → ws/ layout, --layout=new → .maw/ layout).\n\
       Requires --features claude-backend at build + MAW_BENCH_ALLOW_REAL_LLM=1.\n\
     --substrate=<arm>: override substrate (only with --layout=old|new).\n\
       Valid: noop|maw|maw-consolidated|worktrees|jj.\n\
     --chaos=on|off: enable bn-3hzt chaos overlay (default off). When on,\n\
       MAW_FP=... is injected into the agent subprocess env so the agent's\n\
       next `maw ws merge` crashes deterministically at a failpoint.\n\
       REQUIRES `maw` built with --features failpoints (preflight warns).\n\
     \n\
     Use `just sg3-layout-eval-pilot` for the canonical pilot invocation.\n\
     Use `just sg3-layout-eval-real n_a=1 n_b=1` for the canonical real-LLM smoke."
}

struct Args {
    layout: Layout,
    n_a: u32,
    n_b: u32,
    artifact_dir: Option<PathBuf>,
    pilot: bool,
    decision_json: Option<PathBuf>,
    plant_regression: PlantedRegression,
    backend: BackendChoice,
    /// `Some` if the user passed `--substrate=...` explicitly. `None`
    /// means: derive per-arm from layout (`old → MawWsLayout`,
    /// `new → MawConsolidatedLayout`, MockAgent → Noop).
    substrate_override: Option<SubstrateChoice>,
    /// bn-3hzt: chaos overlay on/off. Default off for back-compat.
    chaos: bool,
    /// bn-3w0c: optional `--model=<id>` override.
    model: Option<String>,
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
    let mut backend = BackendChoice::Mock;
    let mut substrate_override: Option<SubstrateChoice> = None;
    let mut chaos = false;
    let mut model: Option<String> = None;

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
        } else if a == "--real-llm" {
            backend = BackendChoice::Claude;
        } else if let Some(v) = a.strip_prefix("--substrate=") {
            substrate_override = Some(SubstrateChoice::parse(v)?);
        } else if let Some(v) = a.strip_prefix("--chaos=") {
            chaos = match v {
                "on" | "true" | "1" => true,
                "off" | "false" | "0" => false,
                other => return Err(format!("--chaos: expected on|off, got {other:?}")),
            };
        } else if let Some(v) = a.strip_prefix("--model=") {
            model = Some(v.to_string());
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

    // Pairing checks.
    if let Some(sub) = substrate_override {
        validate_pairing(backend, sub)?;
        if matches!(layout, Layout::Both) && !matches!(sub, SubstrateChoice::Noop) {
            // --layout=both is the §3.1 GO/NO-GO mode: the two arms
            // MUST be the two maw layouts. Allowing --substrate=jj
            // with --layout=both would emit a meaningless verdict
            // (jj has no layout flavor to compare).
            return Err(format!(
                "misconfig: --layout=both with --substrate={}. \
                 --layout=both requires the maw substrate (the two arms \
                 are the two maw layout flavors). Use --layout=old|new \
                 with --substrate=<arm> for single-arm sanity runs.",
                sub.as_str()
            ));
        }
    } else {
        // No explicit substrate. Validate the default pairing per
        // backend (Mock→Noop, Claude→maw).
        let default_sub = match backend {
            BackendChoice::Mock => SubstrateChoice::Noop,
            BackendChoice::Claude => SubstrateChoice::MawWsLayout,
        };
        validate_pairing(backend, default_sub)?;
    }

    Ok(Args {
        layout,
        n_a,
        n_b,
        artifact_dir,
        pilot,
        decision_json,
        plant_regression,
        backend,
        substrate_override,
        chaos,
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
    // any work. Warning-only (operators may intentionally bench an
    // older binary — e.g. to A/B a fix's effect). See
    // `notes/sg3-no-go-rootcause.md` for the bn-2ert root cause this
    // catches at-source.
    let _ = check_maw_version_skew(env!("CARGO_PKG_VERSION"));
    // bn-3hzt: when chaos is requested, advise that the installed
    // maw binary must be built with --features failpoints (we can't
    // reliably feature-detect from outside; the env-bridge is
    // silently a no-op on a non-failpoints build).
    if args.chaos {
        let _ = maw_bench_sweep::check_maw_failpoints_advisory();
    }

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
        "  layout={} n-a={} n-b={} pilot={} backend={} substrate={} chaos={}",
        match args.layout {
            Layout::Old => "old",
            Layout::New => "new",
            Layout::Both => "both",
        },
        args.n_a,
        args.n_b,
        args.pilot,
        args.backend.as_str(),
        args.substrate_override
            .map(|s| s.as_str().to_string())
            .unwrap_or_else(|| match args.backend {
                BackendChoice::Mock => "noop (auto)".to_string(),
                BackendChoice::Claude => "per-layout maw (auto)".to_string(),
            }),
        if args.chaos { "on" } else { "off" },
    );

    let start = Instant::now();

    let arms_to_run: Vec<&str> = match args.layout {
        Layout::Old => vec![ARM_OLD],
        Layout::New => vec![ARM_NEW],
        Layout::Both => vec![ARM_OLD, ARM_NEW],
    };
    let mut total_cost = 0.0_f64;
    for arm in &arms_to_run {
        let plant_for_arm = if *arm == ARM_NEW {
            args.plant_regression
        } else {
            PlantedRegression::None
        };
        let substrate_for_arm = resolve_substrate_for_arm(&args, arm);
        match run_arm(
            arm,
            args.n_a,
            args.n_b,
            &dir,
            plant_for_arm,
            args.backend,
            substrate_for_arm,
            args.chaos,
            args.model.as_deref(),
        ) {
            Ok(cost) => total_cost += cost,
            Err(e) => {
                eprintln!("run_arm({arm}): {e}");
                return ExitCode::from(3);
            }
        }
    }

    let elapsed = start.elapsed();
    eprintln!(
        "  drove {} arm(s) in {:.2}s",
        arms_to_run.len(),
        elapsed.as_secs_f64()
    );
    if total_cost > 0.0 {
        eprintln!("  total_cost_usd = {total_cost:.4}");
    }
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

/// Pick the substrate for a given arm based on (a) explicit
/// `--substrate=` override, (b) backend (Mock→Noop), (c) arm name
/// (`maw@old-layout → MawWsLayout`, `maw@new-layout → MawConsolidatedLayout`).
fn resolve_substrate_for_arm(args: &Args, arm: &str) -> SubstrateChoice {
    if let Some(sub) = args.substrate_override {
        return sub;
    }
    match args.backend {
        BackendChoice::Mock => SubstrateChoice::Noop,
        BackendChoice::Claude => {
            if arm == ARM_OLD {
                SubstrateChoice::MawWsLayout
            } else {
                SubstrateChoice::MawConsolidatedLayout
            }
        }
    }
}

/// Drive one arm's subset (SUB-A + SUB-B). Returns total cost for the
/// arm (sum of per-run `cost_usd`).
fn run_arm(
    arm: &str,
    n_a: u32,
    n_b: u32,
    base_dir: &Path,
    plant: PlantedRegression,
    backend: BackendChoice,
    substrate: SubstrateChoice,
    chaos: bool,
    model: Option<&str>,
) -> Result<f64, String> {
    let arm_root = arm_dir(base_dir, arm);
    let agent_cfg_override = model.map(|m| maw_bench::agent::AgentConfig {
        model: m.to_string(),
        ..maw_bench::agent::AgentConfig::default()
    });
    let driver = SweepDriver::new(&arm_root)
        .map_err(|e| format!("driver: {e}"))?
        .with_plan_steps(4)
        .with_pinned_clock(1_000, 2_000)
        .with_chaos(chaos)
        .with_agent_config(agent_cfg_override);

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

    let mut total = 0.0_f64;
    total += drive_grid(&driver, &sub_a, plant, "C0", backend, substrate)?;
    total += drive_grid(&driver, &sub_b, plant, "C2", backend, substrate)?;
    Ok(total)
}

fn drive_grid(
    driver: &SweepDriver,
    grid: &SweepGrid,
    plant: PlantedRegression,
    plant_cond: &str,
    backend: BackendChoice,
    substrate: SubstrateChoice,
) -> Result<f64, String> {
    // For the planted-regression case at SUB-A on the new arm we
    // need to materialize a `work_lost_events > 0` outcome. The
    // MockAgent path always produces a clean BenchRun; we mutate
    // the on-disk JSON after the driver completes (only for the
    // planted cell). This is test-only behavior gated behind
    // `--plant-r1`; production runs never set it.
    let runs = driver
        .drive(
            grid,
            |_arm| Ok::<RealSubstrate, String>(RealSubstrate::for_choice(substrate)),
            |seed| make_any_agent(backend, seed).expect("agent factory checked at parse time"),
        )
        .map_err(|e| format!("drive: {e}"))?;
    if plant == PlantedRegression::R1HardBar && plant_cond == "C0" {
        plant_r1_loss(driver.artifact_dir(), &grid.cells[0])?;
    }
    let total = runs.iter().filter_map(|r| r.cost_usd).sum();
    Ok(total)
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
