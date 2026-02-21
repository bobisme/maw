//! Integration tests for `maw ws restore`.

mod manifold_common;

use manifold_common::TestRepo;

#[test]
fn restore_recreates_destroyed_workspace_at_current_epoch() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("alice", "feature.txt", "Alice's important work\n");

    let destroy_output = repo.maw_ok(&["ws", "destroy", "alice", "--force"]);
    assert!(destroy_output.contains("destroyed"));
    assert!(!repo.workspace_exists("alice"));

    let restore_output = repo.maw_ok(&["ws", "restore", "alice"]);
    assert!(restore_output.contains("Restoring") || restore_output.contains("restored"));

    assert!(repo.workspace_exists("alice"));
    assert!(repo.read_file("alice", "feature.txt").is_none());
}

#[test]
fn restore_already_existing_workspace_fails() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "bob"]);

    let stderr = repo.maw_fails(&["ws", "restore", "bob"]);
    assert!(stderr.contains("already exists"), "Got: {stderr}");
}

#[test]
fn restore_default_workspace_fails() {
    let repo = TestRepo::new();

    let stderr = repo.maw_fails(&["ws", "restore", "default"]);
    assert!(stderr.contains("default"), "Got: {stderr}");
}

#[test]
fn restore_never_existed_workspace_creates_fresh_workspace() {
    let repo = TestRepo::new();

    let output = repo.maw_ok(&["ws", "restore", "phantom"]);
    assert!(output.contains("Restoring") || output.contains("recreated"));
    assert!(repo.workspace_exists("phantom"));
}

#[test]
fn destroy_output_confirms_workspace_removed() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "carol"]);

    let output = repo.maw_ok(&["ws", "destroy", "carol"]);
    assert!(
        output.contains("Workspace 'carol' destroyed."),
        "Got: {output}"
    );
}

#[test]
fn history_includes_workspace_lifecycle_events_after_restore() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "hist-a"]);
    repo.maw_ok(&["ws", "destroy", "hist-a"]);
    repo.maw_ok(&["ws", "restore", "hist-a"]);

    let raw = repo.maw_ok(&[
        "ws", "history", "hist-a", "--format", "json", "--limit", "20",
    ]);
    let history_json: serde_json::Value =
        serde_json::from_str(&raw).expect("ws history --format json should be valid JSON");

    let operations = history_json["operations"]
        .as_array()
        .expect("operations should be present in history output");
    let op_types: Vec<&str> = operations
        .iter()
        .filter_map(|op| op["op_type"].as_str())
        .collect();

    assert!(
        op_types.contains(&"create"),
        "history should include at least one create operation"
    );
    assert!(
        op_types.contains(&"destroy"),
        "history should include destroy operation"
    );
}
