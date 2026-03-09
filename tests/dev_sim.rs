mod manifold_common;

use std::fs;
use std::process::Command;

use manifold_common::maw_bin;
use tempfile::tempdir;

#[test]
fn dev_sim_replay_prints_workflow_seed_command() {
    let dir = tempdir().unwrap();
    let out = Command::new(maw_bin())
        .args([
            "dev",
            "sim",
            "replay",
            "--harness",
            "workflow",
            "--seed",
            "7",
            "--print-only",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "print-only replay should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("WORKFLOW_DST_SEED=7 cargo test -p maw-workspaces --test workflow_dst"),
        "expected workflow replay command, got: {stdout}"
    );
}

#[test]
fn dev_sim_replay_bundle_prefers_minimized_command() {
    let dir = tempdir().unwrap();
    let bundle = dir.path().join("bundle.json");
    fs::write(
        &bundle,
        r#"{
          "harness": "action-workflow-dst",
          "seed": 99,
          "replay_command": "FULL_CMD",
          "minimized_replay_command": "MIN_CMD"
        }"#,
    )
    .unwrap();

    let out = Command::new(maw_bin())
        .args([
            "dev",
            "sim",
            "replay",
            "--bundle",
            bundle.to_str().unwrap(),
            "--print-only",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(out.status.success(), "bundle replay should succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("MIN_CMD"),
        "expected minimized replay command: {stdout}"
    );
    assert!(
        !stdout.contains("FULL_CMD\n"),
        "unexpected full command in default print: {stdout}"
    );
}

#[test]
fn dev_sim_replay_rejects_success_summary_bundle() {
    let dir = tempdir().unwrap();
    let bundle = dir.path().join("summary.json");
    fs::write(
        &bundle,
        r#"{
          "harness": "workflow-dst",
          "settings": {"trace_count": 4},
          "seeds": [{"seed": 1, "steps_executed": 4, "warnings": []}]
        }"#,
    )
    .unwrap();

    let out = Command::new(maw_bin())
        .args([
            "dev",
            "sim",
            "replay",
            "--bundle",
            bundle.to_str().unwrap(),
            "--print-only",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "success summary should not be replayable"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("is a DST success summary")
            && stderr.contains("maw dev sim replay --harness"),
        "expected actionable summary error, got: {stderr}"
    );
}

#[test]
fn dev_sim_run_prints_campaign_commands() {
    let dir = tempdir().unwrap();
    let out = Command::new(maw_bin())
        .args([
            "dev",
            "sim",
            "run",
            "--harness",
            "all",
            "--seeds",
            "5",
            "--steps",
            "9",
            "--print-only",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(out.status.success(), "run print-only should succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("WORKFLOW_DST_TRACES=5"),
        "missing workflow campaign: {stdout}"
    );
    assert!(
        stdout.contains("ACTION_DST_TRACES=5"),
        "missing action campaign: {stdout}"
    );
    assert!(
        stdout.contains("ACTION_DST_STEPS=9"),
        "missing action step limit: {stdout}"
    );
}

#[test]
fn dev_sim_run_json_output_includes_results_array() {
    let dir = tempdir().unwrap();
    let out = Command::new(maw_bin())
        .args([
            "dev",
            "sim",
            "run",
            "--harness",
            "all",
            "--seeds",
            "5",
            "--steps",
            "9",
            "--print-only",
            "--format",
            "json",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(out.status.success(), "json run should succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON output");
    assert_eq!(json["print_only"].as_bool(), Some(true));
    assert!(
        json["commands"].is_array(),
        "expected commands array: {stdout}"
    );
    assert!(
        json["results"].is_array(),
        "expected results array: {stdout}"
    );
}

#[test]
fn dev_sim_shrink_prints_minimized_command_from_bundle() {
    let dir = tempdir().unwrap();
    let bundle = dir.path().join("bundle.json");
    fs::write(
        &bundle,
        r#"{
          "harness": "action-workflow-dst",
          "seed": 99,
          "replay_command": "ACTION_DST_SEED=99 ACTION_DST_STEPS=12 cargo test foo",
          "minimized_replay_command": "ACTION_DST_SEED=99 ACTION_DST_STEPS=4 cargo test foo"
        }"#,
    )
    .unwrap();

    let out = Command::new(maw_bin())
        .args([
            "dev",
            "sim",
            "shrink",
            "--bundle",
            bundle.to_str().unwrap(),
            "--print-only",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(out.status.success(), "shrink print-only should succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Min prefix:   4"),
        "expected minimized prefix: {stdout}"
    );
    assert!(
        stdout.contains("ACTION_DST_SEED=99 ACTION_DST_STEPS=4"),
        "expected minimized command: {stdout}"
    );
}

#[test]
fn dev_sim_inspect_prints_failure_bundle_summary() {
    let dir = tempdir().unwrap();
    let bundle = dir.path().join("bundle.json");
    fs::write(
        &bundle,
        r#"{
          "harness": "action-workflow-dst",
          "seed": 42,
          "replay_command": "FULL",
          "minimized_replay_command": "MIN",
          "trace": ["step1", "step2"],
          "violations": ["bad"],
          "warnings": ["warning"],
          "snapshots": {"repo_root": "/tmp/repo"}
        }"#,
    )
    .unwrap();

    let out = Command::new(maw_bin())
        .args(["dev", "sim", "inspect", bundle.to_str().unwrap()])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(out.status.success(), "inspect should succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("action-workflow-dst"),
        "expected harness in inspect output: {stdout}"
    );
    assert!(
        stdout.contains("Seed:        42"),
        "expected seed in inspect output: {stdout}"
    );
    assert!(
        stdout.contains("Min replay:  MIN"),
        "expected minimized replay in inspect output: {stdout}"
    );
}

#[test]
fn dev_sim_replay_json_output_is_machine_readable() {
    let dir = tempdir().unwrap();
    let out = Command::new(maw_bin())
        .args([
            "dev",
            "sim",
            "replay",
            "--harness",
            "workflow",
            "--seed",
            "11",
            "--print-only",
            "--format",
            "json",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(out.status.success(), "json replay should succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON output");
    assert_eq!(json["print_only"].as_bool(), Some(true));
    assert!(
        json["command"]
            .as_str()
            .is_some_and(|cmd| cmd.contains("WORKFLOW_DST_SEED=11"))
    );
}

#[test]
fn dev_sim_inspect_json_output_is_machine_readable() {
    let dir = tempdir().unwrap();
    let bundle = dir.path().join("summary.json");
    fs::write(
        &bundle,
        r#"{
          "harness": "workflow-dst",
          "settings": {"trace_count": 4},
          "seeds": [
            {"seed": 1, "steps_executed": 4, "warnings": []},
            {"seed": 2, "steps_executed": 5, "warnings": ["w"]}
          ]
        }"#,
    )
    .unwrap();

    let out = Command::new(maw_bin())
        .args([
            "dev",
            "sim",
            "inspect",
            bundle.to_str().unwrap(),
            "--format",
            "json",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(out.status.success(), "json inspect should succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON output");
    assert_eq!(json["bundle_type"].as_str(), Some("success"));
    assert_eq!(json["seed_count"].as_u64(), Some(2));
    assert_eq!(json["total_warning_count"].as_u64(), Some(1));
}

#[test]
fn dev_sim_inspect_latest_uses_newest_artifact() {
    let dir = tempdir().unwrap();
    let artifact_root = dir.path().join("artifacts");
    let older = artifact_root.join("workflow-dst").join("success-1");
    let newer = artifact_root.join("action-workflow-dst").join("seed-2-999");
    std::fs::create_dir_all(&older).unwrap();
    std::fs::create_dir_all(&newer).unwrap();
    fs::write(
        older.join("summary.json"),
        r#"{"harness":"workflow-dst","settings":{"trace_count":1},"seeds":[]}"#,
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));
    fs::write(
        newer.join("bundle.json"),
        r#"{
          "harness":"action-workflow-dst",
          "seed":77,
          "replay_command":"FULL",
          "minimized_replay_command":"MIN",
          "trace":[],
          "violations":["bad"],
          "warnings":[],
          "snapshots":{"repo_root":"/tmp/repo"}
        }"#,
    )
    .unwrap();

    let out = Command::new(maw_bin())
        .env("DST_ARTIFACT_DIR", &artifact_root)
        .args(["dev", "sim", "inspect", "--latest", "--format", "json"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(out.status.success(), "latest inspect should succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON output");
    assert_eq!(json["bundle_type"].as_str(), Some("failure"));
    assert_eq!(json["seed"].as_u64(), Some(77));
    assert!(
        json["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("bundle.json"))
    );
}

#[test]
fn dev_sim_run_json_reports_execution_results_and_artifact() {
    let dir = tempdir().unwrap();
    let artifact_root = dir.path().join("artifacts");
    let out = Command::new(maw_bin())
        .env("DST_ARTIFACT_DIR", &artifact_root)
        .args([
            "dev",
            "sim",
            "run",
            "--harness",
            "workflow",
            "--seeds",
            "1",
            "--cwd",
            env!("CARGO_MANIFEST_DIR"),
            "--format",
            "json",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(out.status.success(), "json run should succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON output");
    let results = json["results"].as_array().expect("results array");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["harness"].as_str(), Some("workflow"));
    assert_eq!(results[0]["success"].as_bool(), Some(true));
    assert!(
        results[0]["artifact_path"]
            .as_str()
            .is_some_and(|p| p.ends_with("summary.json"))
    );
}
