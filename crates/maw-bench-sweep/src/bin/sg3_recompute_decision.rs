//! `sg3-recompute-decision` — offline SG3 verdict recomputation.
//!
//! bn-27ai (2026-05-27): the 2026-05-26 SG3 rerun reported R6 NO-GO
//! at C2-T0 because of a 3-layer metric-pipeline defect (see
//! `notes/sg3-no-go-rootcause-v2.md`). This binary loads the committed
//! per-arm BenchRun artifacts, runs them through the **fixed** metric
//! pipeline (Fix A.1 = `is_maw_arm` recognises `maw@<flavor>`;
//! Fix A.2 = task-aware recover suppression; Fix A.3 = raw
//! per-replicate sum in `sum_proxy`), and writes a fresh decision.json
//! plus a Markdown delta report.
//!
//! No new LLM runs. No fresh BenchRuns. Pure offline math against
//! the artifacts already committed under
//! `notes/eval-real-2026-05-27/sg3-rerun/`.
//!
//! # Usage
//!
//! ```text
//! sg3-recompute-decision \
//!     --rerun-dir notes/eval-real-2026-05-27/sg3-rerun \
//!     --out-dir   notes/eval-real-2026-05-27/sg3-rerun-recomputed
//! ```
//!
//! The expected directory layout (under `--rerun-dir`):
//!
//! ```text
//! <rerun-dir>/maw-old-layout/{C0-T0,C2-T0}/*.json   (BenchRun records)
//! <rerun-dir>/maw-new-layout/{C0-T0,C2-T0}/*.json   (BenchRun records)
//! <rerun-dir>/decision.json                         (original verdict)
//! ```
//!
//! Outputs (created under `--out-dir`):
//!
//! - `decision.json` — the recomputed verdict (post-fix).
//! - `delta.md`      — side-by-side old-vs-new diff per rule.
//! - `summary.json`  — machine-readable summary of the delta.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use maw_bench_sweep::{
    ARM_NEW, ARM_OLD, Decision, EvaluatedRule, PrereggedBars, aggregate_artifacts, decide_go_no_go,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mut rerun_dir: Option<PathBuf> = None;
    let mut out_dir: Option<PathBuf> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--rerun-dir" => {
                rerun_dir = Some(PathBuf::from(args.get(i + 1).expect("--rerun-dir value")));
                i += 2;
            }
            "--out-dir" => {
                out_dir = Some(PathBuf::from(args.get(i + 1).expect("--out-dir value")));
                i += 2;
            }
            "-h" | "--help" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown arg: {other}");
                print_help();
                return ExitCode::from(2);
            }
        }
    }
    let Some(rerun_dir) = rerun_dir else {
        eprintln!("--rerun-dir required");
        return ExitCode::from(2);
    };
    let Some(out_dir) = out_dir else {
        eprintln!("--out-dir required");
        return ExitCode::from(2);
    };

    // The on-disk layout uses hyphen-separated arm directory names
    // (`maw-old-layout` / `maw-new-layout`) even though the BenchRun
    // `manifest.arm` field stores the canonical `maw@<flavor>` token.
    let old_dir = rerun_dir.join("maw-old-layout");
    let new_dir = rerun_dir.join("maw-new-layout");
    if !old_dir.is_dir() || !new_dir.is_dir() {
        eprintln!(
            "expected per-arm dirs at:\n  {}\n  {}",
            old_dir.display(),
            new_dir.display()
        );
        return ExitCode::from(2);
    }

    eprintln!("sg3-recompute-decision: loading old-layout artifacts");
    let old_summary = match aggregate_artifacts(&old_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("aggregate old: {e}");
            return ExitCode::from(3);
        }
    };
    eprintln!(
        "  loaded {} runs across {} cells",
        old_summary.total_runs,
        old_summary.cells.len()
    );
    eprintln!("sg3-recompute-decision: loading new-layout artifacts");
    let new_summary = match aggregate_artifacts(&new_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("aggregate new: {e}");
            return ExitCode::from(3);
        }
    };
    eprintln!(
        "  loaded {} runs across {} cells",
        new_summary.total_runs,
        new_summary.cells.len()
    );

    let decision = decide_go_no_go(&old_summary, &new_summary, None, PrereggedBars::default());

    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("mkdir {}: {e}", out_dir.display());
        return ExitCode::from(3);
    }

    // Pretty-print the recomputed decision.
    let decision_path = out_dir.join("decision.json");
    let decision_json = serde_json::to_string_pretty(&decision).expect("serialize decision");
    if let Err(e) = std::fs::write(&decision_path, &decision_json) {
        eprintln!("write {}: {e}", decision_path.display());
        return ExitCode::from(3);
    }
    eprintln!("wrote {}", decision_path.display());

    // Per-arm per-cell raw work_redone summary for R6 visibility.
    let mut per_cell_table: Vec<(String, String, u64, u64)> = Vec::new();
    for cond in &["C0-T0", "C2-T0"] {
        let (cell_old, cell_new) = (
            cond.split_once('-').and_then(|(c, t)| {
                old_summary.cell(ARM_OLD, c, t).map(|cell| {
                    let total = cell
                        .sum
                        .get("work_redone_turns")
                        .map(|v| match v {
                            maw_bench_metrics::MetricValue::Count { n } => *n,
                            maw_bench_metrics::MetricValue::DurationMs { ms } => *ms,
                            maw_bench_metrics::MetricValue::UsdCents { cents } => *cents,
                            maw_bench_metrics::MetricValue::Infinite => u64::MAX,
                            maw_bench_metrics::MetricValue::Unavailable => 0,
                        })
                        .unwrap_or(0);
                    (cell.n, total)
                })
            }),
            cond.split_once('-').and_then(|(c, t)| {
                new_summary.cell(ARM_NEW, c, t).map(|cell| {
                    let total = cell
                        .sum
                        .get("work_redone_turns")
                        .map(|v| match v {
                            maw_bench_metrics::MetricValue::Count { n } => *n,
                            maw_bench_metrics::MetricValue::DurationMs { ms } => *ms,
                            maw_bench_metrics::MetricValue::UsdCents { cents } => *cents,
                            maw_bench_metrics::MetricValue::Infinite => u64::MAX,
                            maw_bench_metrics::MetricValue::Unavailable => 0,
                        })
                        .unwrap_or(0);
                    (cell.n, total)
                })
            }),
        );
        if let (Some((n_old, sum_old)), Some((n_new, sum_new))) = (cell_old, cell_new) {
            per_cell_table.push((
                (*cond).to_string(),
                format!("N_old={n_old} N_new={n_new}"),
                sum_old,
                sum_new,
            ));
        }
    }

    // Write delta.md.
    let delta_path = out_dir.join("delta.md");
    let delta = render_delta(&decision, &per_cell_table, &rerun_dir);
    if let Err(e) = std::fs::write(&delta_path, &delta) {
        eprintln!("write {}: {e}", delta_path.display());
        return ExitCode::from(3);
    }
    eprintln!("wrote {}", delta_path.display());

    // summary.json — machine-readable rollup.
    let summary_path = out_dir.join("summary.json");
    let summary = serde_json::json!({
        "schema_version": 1,
        "verdict": decision.label(),
        "rerun_dir": rerun_dir.display().to_string(),
        "per_cell_raw_work_redone_sum": per_cell_table.iter().map(|(cell, _, o, n)| {
            serde_json::json!({"cell": cell, "old": o, "new": n})
        }).collect::<Vec<_>>(),
        "note": "bn-27ai offline recomputation (Fix A.1/A.2/A.3). \
                Raw per-replicate sums replace the pre-fix median×n proxy.",
    });
    if let Err(e) = std::fs::write(
        &summary_path,
        serde_json::to_string_pretty(&summary).expect("serialize summary"),
    ) {
        eprintln!("write {}: {e}", summary_path.display());
        return ExitCode::from(3);
    }
    eprintln!("wrote {}", summary_path.display());

    eprintln!(
        "sg3-recompute-decision: recomputed verdict = {}",
        decision.label()
    );
    ExitCode::SUCCESS
}

fn render_delta(
    decision: &Decision,
    per_cell: &[(String, String, u64, u64)],
    rerun_dir: &Path,
) -> String {
    let mut s = String::new();
    s.push_str("# SG3 Recomputed Decision — bn-27ai Fix A.1/A.2/A.3\n\n");
    s.push_str(&format!("Source artifacts: `{}`\n\n", rerun_dir.display()));
    s.push_str(
        "Recomputed offline against the committed 2026-05-27 BenchRun \
         set using the **fixed** metric pipeline:\n\n\
         - **Fix A.1**: `is_maw_arm` now recognises `maw@<flavor>` arms \
         (was: only `maw` and `maw-*`). The SG3 arms \
         `maw@old-layout` / `maw@new-layout` now route through the \
         principled T2.5 attribution path instead of the substring \
         fallback.\n\
         - **Fix A.2** (Approach α): `WsRecoverInvoked` cluster count is \
         decremented by the number of recover-tasks in the scenario \
         prompt's task battery — correctly-executed task-required \
         recovers are no longer mis-classified as friction.\n\
         - **Fix A.3**: `sum_proxy` reads the raw per-replicate sum \
         from `CellAggregate::sum` rather than `median × n` (which \
         integer-truncated 1-bit median deltas into N×-amplified \
         totals).\n\n",
    );
    s.push_str(&format!("## Verdict\n\n**{}**\n\n", decision.label()));
    match decision {
        Decision::Go { .. } => {
            s.push_str(
                "All R1-R6 rules pass under the corrected metric \
                 pipeline. Compare against the pre-fix `decision.json` \
                 in the parent directory.\n\n",
            );
        }
        Decision::NoGo {
            regression_rule,
            regression_metric,
            by_amount,
            ..
        } => {
            s.push_str(&format!(
                "Regression: **{regression_rule}** / **{regression_metric}**\n\n\
                 By amount: {by_amount}\n\n",
            ));
        }
    }

    s.push_str("## R6 raw per-replicate sums (Fix A.3 surface)\n\n");
    s.push_str("| cell | N | raw sum(old) | raw sum(new) | delta |\n");
    s.push_str("|------|---|-------------:|-------------:|------:|\n");
    for (cell, n_str, sum_old, sum_new) in per_cell {
        let delta = i64::try_from(*sum_new).unwrap_or(i64::MAX)
            - i64::try_from(*sum_old).unwrap_or(i64::MAX);
        s.push_str(&format!(
            "| {cell} | {n_str} | {sum_old} | {sum_new} | {delta:+} |\n"
        ));
    }
    s.push_str(
        "\nPre-fix the same data emitted R6 C2-T0 as `total(new) = 10, \
         total(old) = 0` via `median × n`. Post-fix the totals are the \
         raw per-replicate sums.\n",
    );

    s.push_str("\n## Per-rule evidence\n\n");
    let evidence = match decision {
        Decision::Go { evidence } => evidence,
        Decision::NoGo { evidence, .. } => evidence,
    };
    s.push_str("| rule | cell | metric | old | new | status |\n");
    s.push_str("|------|------|--------|----:|----:|--------|\n");
    for r in &evidence.rules {
        s.push_str(&format_rule_row(r));
    }

    s
}

fn format_rule_row(r: &EvaluatedRule) -> String {
    format!(
        "| {} | {} | {} | {} | {} | {:?} |\n",
        r.rule_id, r.cell_id, r.metric, r.old_value, r.new_value, r.status
    )
}

fn print_help() {
    eprintln!(
        "sg3-recompute-decision (bn-27ai)\n\n\
         Loads committed BenchRun artifacts and writes a recomputed \
         SG3 decision.json + delta.md against the post-Fix-A.1/A.2/A.3 \
         metric pipeline. No LLM runs.\n\n\
         Usage:\n\
         \x20 sg3-recompute-decision --rerun-dir <DIR> --out-dir <DIR>\n"
    );
}
