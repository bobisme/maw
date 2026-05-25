// Bin lint waivers — mirror sg2-report + sg2-sweep-pilot.
#![allow(clippy::doc_markdown)]
#![allow(clippy::missing_const_for_fn)]
// CLI shim: arg-parsing match is the canonical pattern here even when
// the arm is single-pattern (matches the sister bins' shape); cast
// chains in the gregorian-breakdown helper are deliberately narrow
// (positive epoch seconds within u32 range for the audit field).
#![allow(clippy::single_match_else)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::too_many_lines)]

//! `sg2-friction-list <artifact-dir> [--out-json <path>] [--out-md <path>]` —
//! read maw-arm `*.json` `BenchRun` files from a directory, extract per-verb
//! attribution into `DiagnosticBundle`s, reduce to a [`FrictionList`], and
//! emit JSON + Markdown.
//!
//! This is the T2.8 (`bn-u9iy`) generator binary. The output JSON is
//! the SG4 input format; the Markdown is the human-readable peer that
//! ships as `notes/sg2-friction-list.md`.
//!
//! Per pre-reg §3.1 Pilot rule: when run against pilot artifacts
//! (e.g. `just sg2-friction-list-pilot`), the numbers are HARNESS-ONLY
//! and the Markdown header stamps a TEMPLATE banner so a reader cannot
//! mistake them for publication numbers.
//!
//! Exit codes:
//! - `0` — wrote outputs.
//! - `2` — invalid arguments.
//! - `3` — directory read, BenchRun parse, or write error.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use maw_bench::run::BenchRun;
use maw_bench_metrics::{
    DiagnosticBundle, FrictionSource, MawVerbAttribution, SweepRunRef, extract_metrics,
    friction_list_from_bundles, render_friction_list_md,
};

fn usage() -> &'static str {
    "usage:\n\
     sg2-friction-list <artifact-dir> [--out-json <path>] [--out-md <path>]\n\
       Reduce maw-arm DiagnosticBundles extracted from BenchRun JSONs.\n\
     sg2-friction-list --synthetic-demo [--out-json <path>] [--out-md <path>]\n\
       Build a planted-bundle demo set (TEMPLATE for the doc scaffold).\n\
       No BenchRuns are read; the bundles are constructed in-memory."
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(first) = args.next() else {
        eprintln!("{}", usage());
        return ExitCode::from(2);
    };
    if first == "--help" || first == "-h" {
        println!("{}", usage());
        return ExitCode::SUCCESS;
    }
    let synthetic_demo = first == "--synthetic-demo";
    let dir: PathBuf = if synthetic_demo {
        PathBuf::new()
    } else {
        PathBuf::from(&first)
    };
    let mut out_json: Option<PathBuf> = None;
    let mut out_md: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out-json" => match args.next() {
                Some(p) => out_json = Some(PathBuf::from(p)),
                None => {
                    eprintln!("--out-json requires a path\n{}", usage());
                    return ExitCode::from(2);
                }
            },
            "--out-md" => match args.next() {
                Some(p) => out_md = Some(PathBuf::from(p)),
                None => {
                    eprintln!("--out-md requires a path\n{}", usage());
                    return ExitCode::from(2);
                }
            },
            other => {
                eprintln!("unknown arg: {other}\n{}", usage());
                return ExitCode::from(2);
            }
        }
    }

    let bundles: Vec<DiagnosticBundle> = if synthetic_demo {
        // Planted-bundle demo: builds the doc scaffold a populated
        // friction list would render. Per the bone, this is
        // TEMPLATE data — the real numbers come from the post-
        // hardening real-LLM campaign that hasn't run yet.
        synthetic_demo_bundles()
    } else {
        let runs = match load_runs(&dir) {
            Ok(rs) => rs,
            Err(e) => {
                eprintln!("read artifact dir {}: {e}", dir.display());
                return ExitCode::from(3);
            }
        };
        // Extract a DiagnosticBundle per run. We construct the bundle
        // from the MetricRecord's per-verb map; this is the canonical
        // path (extract_metrics ↔ per_verb_attribution
        // ↔ attribute_tool_call).
        runs.iter()
            .map(|run| {
                let rec = extract_metrics(run);
                let counts: BTreeMap<MawVerbAttribution, u32> = rec
                    .per_verb_wasted_turns
                    .iter()
                    .map(|(a, n)| (*a, *n))
                    .collect();
                let evidence: BTreeMap<MawVerbAttribution, Vec<String>> = BTreeMap::new();
                // total_unattributed_wasted_turns is not currently
                // computed by extract_metrics — the first-pass
                // classifier returns Option<Cluster> and "None" is
                // silent. We approximate it as 0 here; the
                // unattributed bucket gets surfaced explicitly in
                // the doc with "(first-pass extractor does not
                // enrich…)" framing. Real values land when the
                // harness writes a v2 BenchRun with explicit
                // unattributed-marker counts.
                DiagnosticBundle::from_counts(&run.manifest.arm, &run.run_id, &counts, &evidence, 0)
            })
            .collect()
    };

    let sweep_run = SweepRunRef {
        artifact_dir: if synthetic_demo {
            "synthetic://demo".to_string()
        } else {
            dir.display().to_string()
        },
        sweep_summary_ref: String::new(),
        bundle_count: u32::try_from(bundles.len()).unwrap_or(u32::MAX),
    };
    let now = current_utc_string();
    let harness_sha = git_head_sha().unwrap_or_default();
    let list = friction_list_from_bundles(
        &bundles,
        sweep_run,
        FrictionSource::FirstPassClassifier,
        &now,
        &harness_sha,
    );

    let json = match list.to_json() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("serialize friction list: {e}");
            return ExitCode::from(3);
        }
    };
    let md = render_friction_list_md(&list);

    if let Some(p) = &out_json {
        if let Err(e) = fs::write(p, &json) {
            eprintln!("write {}: {e}", p.display());
            return ExitCode::from(3);
        }
    }
    if let Some(p) = &out_md {
        if let Err(e) = fs::write(p, &md) {
            eprintln!("write {}: {e}", p.display());
            return ExitCode::from(3);
        }
    }
    if out_json.is_none() && out_md.is_none() {
        // No outputs requested → write JSON to stdout. The Markdown
        // is the human-readable peer; print it to stderr behind a
        // banner so a `> out.json` redirect still yields valid JSON.
        eprintln!("----- friction-list.md (preview on stderr) -----");
        eprintln!("{md}");
        eprintln!("----- end preview -----");
        print!("{json}");
    }
    eprintln!(
        "sg2-friction-list: {} bundles -> {} ranked clusters, unattributed = {}",
        bundles.len(),
        list.ranked_clusters.len(),
        list.total_unattributed_wasted_turns,
    );
    ExitCode::SUCCESS
}

fn load_runs(dir: &Path) -> Result<Vec<BenchRun>, std::io::Error> {
    // Walk recursively so sweep-driver outputs (which nest by
    // C<n>-T<n>/ subdir) load cleanly. Mirrors the
    // `maw_bench_sweep::aggregate::visit_dir` pattern.
    let mut out = Vec::new();
    visit(dir, &mut out)?;
    Ok(out)
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

fn current_utc_string() -> String {
    // Minimalist ISO-8601 UTC. Avoids pulling chrono as a dep just
    // for this. Seconds-precision is enough for the audit field.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let (year, month, day, hour, minute, second) = utc_breakdown(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}-{second:02}Z")
}

// Toy gregorian breakdown (good through year 9999). Avoids a deps cliff.
fn utc_breakdown(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let mut s = secs as i64;
    let second = (s % 60) as u32;
    s /= 60;
    let minute = (s % 60) as u32;
    s /= 60;
    let hour = (s % 24) as u32;
    s /= 24; // days since 1970-01-01
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

/// Construct a planted-friction demo set: 6 bundles modeled on the
/// kinds of cluster distributions a real maw-arm campaign would
/// surface. Used by `--synthetic-demo` and the `just
/// sg2-friction-list-pilot` recipe so the rendered doc scaffold
/// shows non-trivial cluster rows even before the real campaign
/// produces friction-bearing transcripts. TEMPLATE-only by design.
fn synthetic_demo_bundles() -> Vec<DiagnosticBundle> {
    use MawVerbAttribution as A;
    let mut out = Vec::new();
    let push =
        |out: &mut Vec<DiagnosticBundle>, run_id: &str, attrs: &[(A, u32)], unattributed: u32| {
            let mut counts: BTreeMap<A, u32> = BTreeMap::new();
            for (a, n) in attrs {
                counts.insert(*a, *n);
            }
            let evidence: BTreeMap<A, Vec<String>> = BTreeMap::new();
            out.push(DiagnosticBundle::from_counts(
                "maw",
                run_id,
                &counts,
                &evidence,
                unattributed,
            ));
        };
    push(
        &mut out,
        "synthetic-maw-r01",
        &[
            (A::WsMergeStructuredConflict, 3),
            (A::WsSyncStaleWorkspace, 1),
            (A::VocabularyScarcity, 1),
        ],
        2,
    );
    push(
        &mut out,
        "synthetic-maw-r02",
        &[(A::WsMergeStructuredConflict, 2), (A::EpochSyncRequired, 1)],
        1,
    );
    push(
        &mut out,
        "synthetic-maw-r03",
        &[
            (A::WsMergeStructuredConflict, 4),
            (A::WsRecoverInvoked, 1),
            (A::VocabularyScarcity, 2),
        ],
        0,
    );
    push(
        &mut out,
        "synthetic-maw-r04",
        &[(A::WsSyncStaleWorkspace, 2), (A::ReadFromStaleWorkspace, 1)],
        1,
    );
    push(
        &mut out,
        "synthetic-maw-r05",
        &[(A::WsDestroyRefused, 1), (A::WsRecoverInvoked, 1)],
        0,
    );
    push(
        &mut out,
        "synthetic-maw-r06",
        &[(A::EpochSyncRequired, 2)],
        1,
    );
    out
}

/// Best-effort `git rev-parse HEAD` so the friction list pins the
/// harness commit. Falls back to env vars (`GIT_HEAD_SHA`) or empty
/// string. Returns Option so the bin can log absence without a panic
/// path.
fn git_head_sha() -> Option<String> {
    if let Ok(sha) = env::var("GIT_HEAD_SHA") {
        if !sha.is_empty() {
            return Some(sha);
        }
    }
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    Some(s.trim().to_string())
}
