//! Integration tests for workspace lifecycle on Manifold v2 backend.
//!
//! Covers create/list/status/destroy behavior with git-native worktrees.

mod manifold_common;

use std::process::Command;

use manifold_common::TestRepo;

#[test]
fn workspace_lifecycle_create_list_duplicate_destroy() {
    let repo = TestRepo::new();

    // Create workspace succeeds and appears in list.
    repo.maw_ok(&["ws", "create", "agent-a"]);

    let listed = repo.maw_ok(&["ws", "list", "--format", "json"]);
    let listed_json: serde_json::Value =
        serde_json::from_str(&listed).expect("ws list --format json should be valid JSON");

    let names: Vec<String> = listed_json["workspaces"]
        .as_array()
        .expect("workspaces should be an array")
        .iter()
        .filter_map(|w| w["name"].as_str().map(ToOwned::to_owned))
        .collect();

    assert!(names.contains(&"default".to_owned()));
    assert!(names.contains(&"agent-a".to_owned()));

    // Duplicate create is rejected.
    let dup_err = repo.maw_fails(&["ws", "create", "agent-a"]);
    assert!(
        dup_err.contains("already exists"),
        "duplicate create should report already exists, got: {dup_err}"
    );

    // Destroy succeeds and workspace is removed from list.
    repo.maw_ok(&["ws", "destroy", "agent-a"]);

    let listed_after = repo.maw_ok(&["ws", "list", "--format", "json"]);
    let listed_after_json: serde_json::Value =
        serde_json::from_str(&listed_after).expect("ws list --format json should be valid JSON");
    let names_after: Vec<String> = listed_after_json["workspaces"]
        .as_array()
        .expect("workspaces should be an array")
        .iter()
        .filter_map(|w| w["name"].as_str().map(ToOwned::to_owned))
        .collect();

    assert!(names_after.contains(&"default".to_owned()));
    assert!(!names_after.contains(&"agent-a".to_owned()));
}

#[test]
fn destroy_repeated_is_idempotent() {
    let repo = TestRepo::new();
    repo.maw_ok(&["ws", "create", "agent-a"]);

    repo.maw_ok(&["ws", "destroy", "agent-a"]);

    // Repeat destroy should be safe/no-op.
    let second = repo.maw_ok(&["ws", "destroy", "agent-a"]);
    assert!(
        second.contains("already absent") || second.contains("No action needed"),
        "expected idempotent destroy message, got: {second}"
    );
}

#[test]
fn status_reports_clean_dirty_and_stale_states() {
    let repo = TestRepo::new();

    // Commit initial .gitignore so the default workspace starts clean.
    repo.advance_epoch("chore: baseline for status assertions");

    // Clean default workspace at startup.
    let clean = repo.maw_ok(&["ws", "status", "--format", "json"]);
    let clean_json: serde_json::Value =
        serde_json::from_str(&clean).expect("ws status --format json should be valid JSON");
    assert_eq!(clean_json["is_stale"], false);
    assert_eq!(clean_json["has_changes"], false);

    // Dirty default workspace after file edit.
    repo.add_file("default", "dirty.txt", "changed");
    let dirty = repo.maw_ok(&["ws", "status", "--format", "json"]);
    let dirty_json: serde_json::Value =
        serde_json::from_str(&dirty).expect("ws status --format json should be valid JSON");
    assert_eq!(dirty_json["has_changes"], true);
    assert!(
        dirty_json["changes"]["dirty_files"]
            .as_array()
            .expect("dirty_files should be an array")
            .iter()
            .any(|v| v.as_str() == Some("dirty.txt")),
        "status should include dirty.txt in dirty_files"
    );

    // Stale workspace after epoch advances.
    repo.create_workspace("agent-a");
    repo.add_file("default", "epoch-advance.txt", "advance");
    repo.advance_epoch("chore: advance epoch for stale check");

    let stale = repo.maw_ok(&["ws", "status", "--format", "json"]);
    let stale_json: serde_json::Value =
        serde_json::from_str(&stale).expect("ws status --format json should be valid JSON");

    let workspaces = stale_json["workspaces"]
        .as_array()
        .expect("workspaces should be an array");
    let agent = workspaces
        .iter()
        .find(|w| w["name"].as_str() == Some("agent-a"))
        .expect("agent-a should appear in workspaces list");

    let agent_state = agent["state"].as_str().unwrap_or_default();
    assert!(
        agent_state.contains("stale"),
        "agent-a state should be stale after epoch advance, got: {agent_state}"
    );
}

#[test]
fn workspace_create_template_emits_metadata_and_artifact() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "agent-template", "--template", "bugfix"]);

    let listed = repo.maw_ok(&["ws", "list", "--format", "json"]);
    let listed_json: serde_json::Value =
        serde_json::from_str(&listed).expect("ws list --format json should be valid JSON");

    let workspaces = listed_json["workspaces"]
        .as_array()
        .expect("workspaces should be an array");
    let templated = workspaces
        .iter()
        .find(|w| w["name"].as_str() == Some("agent-template"))
        .expect("templated workspace should exist");

    assert_eq!(templated["template"].as_str(), Some("bugfix"));
    assert_eq!(
        templated["template_defaults"]["merge_policy"].as_str(),
        Some("fast-track-if-clean")
    );

    let artifact_path = repo
        .workspace_path("agent-template")
        .join(".manifold")
        .join("workspace-template.json");
    let artifact_raw = std::fs::read_to_string(&artifact_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", artifact_path.display()));
    let artifact_json: serde_json::Value =
        serde_json::from_str(&artifact_raw).expect("workspace-template artifact should be JSON");

    assert_eq!(artifact_json["template"].as_str(), Some("bugfix"));
    assert_eq!(
        artifact_json["merge_policy"].as_str(),
        Some("fast-track-if-clean")
    );
}

#[test]
fn ws_commands_work_from_inside_workspace_directory() {
    let repo = TestRepo::new();
    repo.maw_ok(&["ws", "create", "agent-a"]);

    let output = Command::new(env!("CARGO_BIN_EXE_maw"))
        .args(["ws", "list", "--format", "json"])
        .current_dir(repo.workspace_path("agent-a"))
        .output()
        .expect("failed to execute maw ws list from workspace directory");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "ws list should work from workspace dir\nstdout: {stdout}\nstderr: {stderr}"
    );

    let listed_json: serde_json::Value =
        serde_json::from_str(&stdout).expect("ws list --format json should produce valid JSON");
    let names: Vec<String> = listed_json["workspaces"]
        .as_array()
        .expect("workspaces should be an array")
        .iter()
        .filter_map(|w| w["name"].as_str().map(ToOwned::to_owned))
        .collect();

    assert!(names.contains(&"default".to_owned()));
    assert!(names.contains(&"agent-a".to_owned()));
}

#[test]
fn destroy_dirty_workspace_requires_force() {
    let repo = TestRepo::new();
    repo.maw_ok(&["ws", "create", "agent-a"]);
    repo.add_file("agent-a", "dirty.txt", "keep me\n");

    let err = repo.maw_fails(&["ws", "destroy", "agent-a"]);
    assert!(
        err.contains("unmerged") || err.contains("--force"),
        "destroy without --force should be blocked for dirty workspace, got: {err}"
    );

    repo.maw_ok(&["ws", "destroy", "agent-a", "--force"]);
    assert!(!repo.workspace_exists("agent-a"));
}
