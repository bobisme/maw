//! Integration tests for `maw ws restore`.

mod manifold_common;

use manifold_common::TestRepo;

#[test]
fn restore_recreates_destroyed_workspace_at_current_epoch() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("alice", "feature.txt", "Alice's important work\n");

    let destroy_output = repo.maw_ok(&["ws", "destroy", "alice"]);
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
    assert!(output.contains("Workspace 'carol' destroyed."), "Got: {output}");
}
