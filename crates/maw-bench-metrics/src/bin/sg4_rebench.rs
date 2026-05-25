// Bin lint waivers — mirror sg2-friction-list.
#![allow(clippy::doc_markdown)]
#![allow(clippy::missing_const_for_fn)]
#![allow(clippy::single_match_else)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::too_many_lines)]

//! `sg4-rebench --baseline <path-or-dir> --after <path-or-dir>
//! [--out-json <path>] [--out-md <path>] [--pilot]` — T4.3 (`bn-1qty`)
//! re-bench runner.
//!
//! Drives the T2.6 SG2 sweep harness over the full pre-reg cells (in
//! production), then diffs the resulting [`maw_bench_metrics::FrictionList`]s
//! to produce per-cluster `(before_cost, after_cost, delta_pct)` rows
//! and per-cluster iteration triggers. The deliverable is
//! `notes/sg4-fix-deltas.md` (Markdown) + a JSON peer for the SG5
//! consumer.
//!
//! ## Input modes
//!
//! Each of `--baseline` and `--after` accepts either:
//!
//! - a JSON file (a [`maw_bench_metrics::FrictionList`] produced by
//!   `sg2-friction-list`), OR
//! - a directory of `BenchRun` JSONs (the recursive layout produced by
//!   `sg2-sweep-pilot`). In this mode the binary reduces the directory
//!   into a [`maw_bench_metrics::FrictionList`] on the fly using the
//!   same reducer the standalone bin uses, then diffs.
//!
//! In pilot mode (`--pilot`), the binary builds the baseline + after
//! pair in-memory from planted demo data (MockAgent-shaped — no
//! BenchRuns are read, no LLM spend, no network). Used by
//! `just sg4-rebench-pilot`.
//!
//! ## Exit codes
//!
//! - `0` — wrote outputs.
//! - `2` — invalid arguments.
//! - `3` — read / parse / write error.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use maw_bench::run::BenchRun;
use maw_bench_metrics::{
    DiagnosticBundle, FrictionList, FrictionSource, MawVerbAttribution, SweepRunRef,
    diff_friction_lists, extract_metrics, friction_list_from_bundles, render_delta_report_md,
    sg4_backlog,
};

fn usage() -> &'static str {
    "usage:\n\
     sg4-rebench --baseline <path-or-dir> --after <path-or-dir> \\\n\
                 [--out-json <path>] [--out-md <path>]\n\
       Diff two FrictionList sources (JSON files or sweep artifact\n\
       directories) and emit the per-cluster delta report.\n\
     sg4-rebench --pilot [--out-json <path>] [--out-md <path>]\n\
       Build a planted baseline+after demo pair in-memory; produces\n\
       the sg4-fix-deltas.md scaffold without reading BenchRuns."
}

struct Args {
    baseline: Option<PathBuf>,
    after: Option<PathBuf>,
    out_json: Option<PathBuf>,
    out_md: Option<PathBuf>,
    pilot: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        baseline: None,
        after: None,
        out_json: None,
        out_md: None,
        pilot: false,
    };
    let mut it = env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            "--pilot" => a.pilot = true,
            "--baseline" => match it.next() {
                Some(v) => a.baseline = Some(PathBuf::from(v)),
                None => return Err("--baseline requires a path".into()),
            },
            "--after" => match it.next() {
                Some(v) => a.after = Some(PathBuf::from(v)),
                None => return Err("--after requires a path".into()),
            },
            "--out-json" => match it.next() {
                Some(v) => a.out_json = Some(PathBuf::from(v)),
                None => return Err("--out-json requires a path".into()),
            },
            "--out-md" => match it.next() {
                Some(v) => a.out_md = Some(PathBuf::from(v)),
                None => return Err("--out-md requires a path".into()),
            },
            other => return Err(format!("unknown arg: {other}")),
        }
    }
    if !a.pilot && (a.baseline.is_none() || a.after.is_none()) {
        return Err("either --pilot or both --baseline and --after required".into());
    }
    Ok(a)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n{}", usage());
            return ExitCode::from(2);
        }
    };

    let (baseline_list, baseline_label) = if args.pilot {
        (pilot_baseline(), "synthetic://pilot-baseline".to_string())
    } else {
        let Some(p) = args.baseline.as_ref() else {
            eprintln!(
                "--baseline is required when --pilot is not set\n{}",
                usage()
            );
            return ExitCode::from(2);
        };
        match load_friction_list(p) {
            Ok(list) => (list, p.display().to_string()),
            Err(e) => {
                eprintln!("read baseline {}: {e}", p.display());
                return ExitCode::from(3);
            }
        }
    };
    let (after_list, after_label) = if args.pilot {
        (pilot_after(), "synthetic://pilot-after".to_string())
    } else {
        let Some(p) = args.after.as_ref() else {
            eprintln!("--after is required when --pilot is not set\n{}", usage());
            return ExitCode::from(2);
        };
        match load_friction_list(p) {
            Ok(list) => (list, p.display().to_string()),
            Err(e) => {
                eprintln!("read after {}: {e}", p.display());
                return ExitCode::from(3);
            }
        }
    };

    let now = current_utc_string();
    let report = diff_friction_lists(
        &baseline_list,
        &after_list,
        &sg4_backlog(),
        &baseline_label,
        &after_label,
        args.pilot,
        &now,
    );

    let json = match report.to_json() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("serialize delta report: {e}");
            return ExitCode::from(3);
        }
    };
    let md = render_delta_report_md(&report);

    if let Some(p) = &args.out_json {
        if let Err(e) = fs::write(p, &json) {
            eprintln!("write {}: {e}", p.display());
            return ExitCode::from(3);
        }
    }
    if let Some(p) = &args.out_md {
        if let Err(e) = fs::write(p, &md) {
            eprintln!("write {}: {e}", p.display());
            return ExitCode::from(3);
        }
    }
    if args.out_json.is_none() && args.out_md.is_none() {
        eprintln!("----- sg4-fix-deltas.md (preview on stderr) -----");
        eprintln!("{md}");
        eprintln!("----- end preview -----");
        print!("{json}");
    }

    // Summary line on stderr for the operator.
    let met = report
        .rows
        .iter()
        .filter(|r| matches!(r.verdict, maw_bench_metrics::RebenchVerdict::TargetMet))
        .count();
    let missed = report
        .rows
        .iter()
        .filter(|r| matches!(r.verdict, maw_bench_metrics::RebenchVerdict::TargetMissed))
        .count();
    let regressed = report
        .rows
        .iter()
        .filter(|r| matches!(r.verdict, maw_bench_metrics::RebenchVerdict::Regressed))
        .count();
    eprintln!(
        "sg4-rebench: {} rows -> met={met}, missed={missed}, regressed={regressed}; triggers={}",
        report.rows.len(),
        report.iteration_triggers.len(),
    );
    if report.unattributed.blocks_sg4() {
        eprintln!(
            "sg4-rebench: WARNING — unattributed bucket grew by {:.2}% (T4.1 footnote blocker)",
            report.unattributed.growth_pct().unwrap_or_default()
        );
    }
    ExitCode::SUCCESS
}

/// Load a [`FrictionList`] from either a JSON file or a directory of
/// `BenchRun` JSONs (reducing on the fly).
fn load_friction_list(path: &Path) -> Result<FrictionList, std::io::Error> {
    let meta = fs::metadata(path)?;
    if meta.is_file() {
        let s = fs::read_to_string(path)?;
        return FrictionList::from_json(&s).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("parse FrictionList: {e}"),
            )
        });
    }
    if meta.is_dir() {
        // Walk recursively; reduce on the fly.
        let mut runs: Vec<BenchRun> = Vec::new();
        visit(path, &mut runs)?;
        let bundles: Vec<DiagnosticBundle> = runs
            .iter()
            .map(|run| {
                let rec = extract_metrics(run);
                let counts: BTreeMap<MawVerbAttribution, u32> = rec
                    .per_verb_wasted_turns
                    .iter()
                    .map(|(a, n)| (*a, *n))
                    .collect();
                let evidence: BTreeMap<MawVerbAttribution, Vec<String>> = BTreeMap::new();
                DiagnosticBundle::from_counts(&run.manifest.arm, &run.run_id, &counts, &evidence, 0)
            })
            .collect();
        let sweep_run = SweepRunRef {
            artifact_dir: path.display().to_string(),
            sweep_summary_ref: String::new(),
            bundle_count: u32::try_from(bundles.len()).unwrap_or(u32::MAX),
        };
        let now = current_utc_string();
        let list = friction_list_from_bundles(
            &bundles,
            sweep_run,
            FrictionSource::FirstPassClassifier,
            &now,
            "",
        );
        return Ok(list);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("not a file or directory: {}", path.display()),
    ))
}

fn visit(dir: &Path, out: &mut Vec<BenchRun>) -> Result<(), std::io::Error> {
    for ent in fs::read_dir(dir)? {
        let ent = ent?;
        let path = ent.path();
        let ftype = ent.file_type()?;
        if ftype.is_dir() {
            visit(&path, out)?;
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let bytes = fs::read_to_string(&path)?;
        let run: BenchRun = match serde_json::from_str(&bytes) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("WARN: skipping {} (parse: {e})", path.display());
                continue;
            }
        };
        out.push(run);
    }
    Ok(())
}

/// Pilot baseline: matches the T2.8 synthetic-demo distribution
/// rounded onto the T4.1 backlog. Returns a [`FrictionList`] with
/// the same per-cluster shape the real "before" snapshot would have.
fn pilot_baseline() -> FrictionList {
    use MawVerbAttribution as A;
    let bundles = vec![planted_bundle(
        "synthetic-baseline-r01",
        &[
            (A::WsMergeStructuredConflict, 9),
            (A::WsSyncStaleWorkspace, 3),
            (A::EpochSyncRequired, 3),
            (A::VocabularyScarcity, 3),
            (A::WsRecoverInvoked, 2),
            (A::WsDestroyRefused, 1),
            (A::ReadFromStaleWorkspace, 1),
        ],
        5,
    )];
    friction_list_from_bundles(
        &bundles,
        SweepRunRef {
            artifact_dir: "synthetic://pilot-baseline".to_string(),
            sweep_summary_ref: String::new(),
            bundle_count: 1,
        },
        FrictionSource::FirstPassClassifier,
        "2026-05-25T00:00:00Z",
        "pilot-sha",
    )
}

/// Pilot after: a "successful hardening pass" shape — every backlog
/// row meets target (≥ 50% reduction or reaches 0). Used by the
/// pilot recipe so the rendered scaffold shows a happy-path table;
/// tests cover the planted-missed-target / regression paths
/// independently in `friction_delta.rs`.
fn pilot_after() -> FrictionList {
    use MawVerbAttribution as A;
    let bundles = vec![planted_bundle(
        "synthetic-after-r01",
        &[
            (A::WsMergeStructuredConflict, 4),
            (A::WsSyncStaleWorkspace, 1),
            (A::EpochSyncRequired, 1),
            (A::VocabularyScarcity, 1),
            (A::WsRecoverInvoked, 1),
            // ws_destroy_refused: 1 → 0 (ReachZero).
            // read_from_stale_workspace: 1 → 0 (ReachZero).
        ],
        4,
    )];
    friction_list_from_bundles(
        &bundles,
        SweepRunRef {
            artifact_dir: "synthetic://pilot-after".to_string(),
            sweep_summary_ref: String::new(),
            bundle_count: 1,
        },
        FrictionSource::FirstPassClassifier,
        "2026-05-25T00:00:00Z",
        "pilot-sha",
    )
}

fn planted_bundle(
    run_id: &str,
    attrs: &[(MawVerbAttribution, u32)],
    unattributed: u32,
) -> DiagnosticBundle {
    let mut counts: BTreeMap<MawVerbAttribution, u32> = BTreeMap::new();
    for (a, n) in attrs {
        counts.insert(*a, *n);
    }
    let evidence: BTreeMap<MawVerbAttribution, Vec<String>> = BTreeMap::new();
    DiagnosticBundle::from_counts("maw", run_id, &counts, &evidence, unattributed)
}

fn current_utc_string() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let (year, month, day, hour, minute, second) = utc_breakdown(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn utc_breakdown(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let mut s = secs as i64;
    let second = (s % 60) as u32;
    s /= 60;
    let minute = (s % 60) as u32;
    s /= 60;
    let hour = (s % 24) as u32;
    s /= 24;
    let mut year: i64 = 1970;
    loop {
        let leap = is_leap(year);
        let dy: i64 = if leap { 366 } else { 365 };
        if s < dy {
            break;
        }
        s -= dy;
        year += 1;
    }
    let leap = is_leap(year);
    let dim: [i64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1i64;
    for d in dim {
        if s < d {
            break;
        }
        s -= d;
        month += 1;
    }
    let day = (s + 1) as u32;
    (year as u32, month as u32, day, hour, minute, second)
}

const fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}
