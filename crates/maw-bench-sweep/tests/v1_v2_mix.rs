#![cfg(feature = "bench")]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::doc_markdown)]

//! Forward-compat test: aggregator must load a mix of v1 and v2
//! BenchRun JSONs cleanly. T2.5 (bn-1rgk) will bump the BenchRun
//! schema to v2; this test pre-validates the parser handles both.

use std::path::Path;

use maw_bench_sweep::aggregate_artifacts;

const V1_RUN_JSON: &str = r#"{
    "schema_version": 1,
    "run_id": "v1-synth",
    "manifest": {
        "claude_code_version": "",
        "claude_model_id": "",
        "claude_effective_model": "",
        "git_version": "",
        "jj_version": "",
        "maw_version": "",
        "benchmark_harness_commit": "",
        "scenario_generator_commit": "",
        "prompt_hash": "",
        "seed": 1,
        "condition_id": "C0",
        "t_class": "T0",
        "arm": "maw",
        "os_kernel": "",
        "start_ts_unix_ms": 0,
        "end_ts_unix_ms": 1000
    },
    "verdict": {"outcome": "success"},
    "oracle_b": {"verdict": "green"},
    "transcript": {
        "prompt": "",
        "prompt_sha256": "",
        "convention_text": "",
        "turns": []
    },
    "total_tool_calls": 7,
    "total_turns": 3,
    "cost_usd": null,
    "duration_ms": 1000,
    "substrate_final_files": []
}"#;

/// v2 BenchRun — v1 shape PLUS two T2.5-style extras
/// (`v2_per_call_attribution`, `attributed_work_redone_turns`).
/// The aggregator's load path uses serde_json::Value sniffing
/// then a tolerant BenchRun deserialize that silently drops
/// unknown fields, so v1+v2 mix.
const V2_RUN_JSON: &str = r#"{
    "schema_version": 2,
    "run_id": "v2-synth",
    "manifest": {
        "claude_code_version": "",
        "claude_model_id": "",
        "claude_effective_model": "",
        "git_version": "",
        "jj_version": "",
        "maw_version": "",
        "benchmark_harness_commit": "",
        "scenario_generator_commit": "",
        "prompt_hash": "",
        "seed": 1,
        "condition_id": "C2",
        "t_class": "T0",
        "arm": "git-worktrees-bare",
        "os_kernel": "",
        "start_ts_unix_ms": 0,
        "end_ts_unix_ms": 1000
    },
    "verdict": {"outcome": "success"},
    "oracle_b": {"verdict": "not_applicable", "reason": "arm = git-worktrees-bare"},
    "transcript": {
        "prompt": "",
        "prompt_sha256": "",
        "convention_text": "",
        "turns": []
    },
    "total_tool_calls": 5,
    "total_turns": 2,
    "cost_usd": null,
    "duration_ms": 800,
    "substrate_final_files": [],
    "v2_per_call_attribution": [],
    "attributed_work_redone_turns": 0
}"#;

const BAD_SCHEMA_RUN_JSON: &str = r#"{
    "schema_version": 99,
    "run_id": "bad",
    "manifest": {
        "claude_code_version": "", "claude_model_id": "", "claude_effective_model": "",
        "git_version": "", "jj_version": "", "maw_version": "",
        "benchmark_harness_commit": "", "scenario_generator_commit": "",
        "prompt_hash": "", "seed": 0, "condition_id": "C0", "t_class": "T0",
        "arm": "maw", "os_kernel": "", "start_ts_unix_ms": 0, "end_ts_unix_ms": 0
    },
    "verdict": {"outcome": "success"},
    "oracle_b": {"verdict": "green"},
    "transcript": {"prompt": "", "prompt_sha256": "", "convention_text": "", "turns": []},
    "total_tool_calls": 0, "total_turns": 0, "cost_usd": null, "duration_ms": 0,
    "substrate_final_files": []
}"#;

#[test]
fn v1_and_v2_records_aggregate_in_the_same_directory() {
    let tmp = tempfile::tempdir().unwrap();
    write_json(tmp.path(), "v1.json", V1_RUN_JSON);
    write_json(tmp.path(), "v2.json", V2_RUN_JSON);
    let s = aggregate_artifacts(tmp.path()).expect("aggregate");
    assert_eq!(s.total_runs, 2);
    // Both cells appear.
    assert!(s.cell("maw", "C0", "T0").is_some());
    assert!(s.cell("git-worktrees-bare", "C2", "T0").is_some());
}

#[test]
fn unknown_schema_version_surfaces_as_error() {
    let tmp = tempfile::tempdir().unwrap();
    write_json(tmp.path(), "bad.json", BAD_SCHEMA_RUN_JSON);
    let err = aggregate_artifacts(tmp.path()).expect_err("must error");
    let msg = format!("{err}");
    assert!(msg.contains("99"), "expected schema 99 in error: {msg}");
    assert!(
        msg.contains("unsupported"),
        "expected 'unsupported' in error: {msg}"
    );
}

fn write_json(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).expect("write json");
}
