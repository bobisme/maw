// Bin lint waivers — match the lib's pragmatism (the bin is a thin
// CLI shim; missing_const_for_fn / doc nits don't help readability).
#![allow(clippy::doc_markdown)]
#![allow(clippy::missing_const_for_fn)]

//! `sg2-report <artifact-dir>` — read `*.json` `BenchRun` files from
//! a directory and print the per-arm dominance table to stdout.
//!
//! Wired up by the `just sg2-report <dir>` recipe. The binary is
//! intentionally minimal — no clap, no fancy CLI — because the human
//! contract is "point it at a directory of BenchRun JSONs and read
//! the table". The Justfile recipe is the documented entry point.
//!
//! Exit codes:
//! - `0` — printed a table (zero or more records).
//! - `2` — invalid arguments.
//! - `3` — directory read or BenchRun parse error (with stderr detail).
//!
//! Per the bone: NEVER prints a composite. The renderer is shared
//! with the library tests so the no-composite invariant is enforced
//! against the same code path.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use maw_bench::run::BenchRun;
use maw_bench_metrics::{extract_metrics, render_dominance_table, ReportOptions};

fn usage() -> &'static str {
    "usage: sg2-report <artifact-dir> [--median]\n\
     prints the per-arm SG2 dominance table over every *.json BenchRun under <artifact-dir>.\n\
     --median  ADDITIONALLY print a per-arm median row (never combined across arms)."
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(dir) = args.next() else {
        eprintln!("{}", usage());
        return ExitCode::from(2);
    };
    let mut want_median = false;
    for a in args {
        match a.as_str() {
            "--median" => want_median = true,
            other => {
                eprintln!("unknown arg: {other}\n{}", usage());
                return ExitCode::from(2);
            }
        }
    }
    let dir = PathBuf::from(dir);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("read_dir {}: {e}", dir.display());
            return ExitCode::from(3);
        }
    };
    let mut records = Vec::new();
    for ent in entries {
        let ent = match ent {
            Ok(e) => e,
            Err(e) => {
                eprintln!("dir entry error: {e}");
                return ExitCode::from(3);
            }
        };
        let path = ent.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let bytes = match fs::read_to_string(&path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("read {}: {e}", path.display());
                return ExitCode::from(3);
            }
        };
        let run: BenchRun = match serde_json::from_str(&bytes) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("parse {}: {e}", path.display());
                return ExitCode::from(3);
            }
        };
        records.push(extract_metrics(&run));
    }
    // Stable per-arm ordering — same as the publication's frozen arm
    // order in pre-reg §1.3.
    let arm_order = vec![
        "maw".to_string(),
        "git-worktrees-bare".to_string(),
        "claude-native-worktrees".to_string(),
        "jj-workspaces".to_string(),
    ];
    let opts = ReportOptions {
        aggregate_median: want_median,
        arm_order: Some(arm_order),
    };
    let out = render_dominance_table(&records, &opts);
    print!("{out}");
    ExitCode::SUCCESS
}
