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
