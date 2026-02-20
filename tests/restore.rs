//! Integration tests for `maw ws restore` command
//!
//! Tests workspace restoration after destroy in bare repos (v2 model).
//! Each test creates an isolated temp repo.

mod common;

use common::{maw_fails, maw_ok, read_from_ws, run_jj, setup_bare_repo, write_in_ws};

#[test]
#[ignore = "requires jj - being replaced by git-native tests (bd-2hw9.4)"]
fn restore_destroyed_workspace_with_changes() {
    let repo = setup_bare_repo();

    // Create a workspace and make changes
    maw_ok(repo.path(), &["ws", "create", "alice"]);
    write_in_ws(
        repo.path(),
        "alice",
        "feature.txt",
        "Alice's important work",
    );
    let alice_ws = repo.path().join("ws").join("alice");
    run_jj(&alice_ws, &["describe", "-m", "feat: alice's feature"]);

    // Verify workspace exists and has the file
    let content = read_from_ws(repo.path(), "alice", "feature.txt");
    assert_eq!(content.as_deref(), Some("Alice's important work"));

    // Destroy the workspace
    let destroy_output = maw_ok(repo.path(), &["ws", "destroy", "alice"]);
    assert!(
        destroy_output.contains("restore"),
        "Destroy output should mention restore command, got: {destroy_output}"
    );

    // Verify workspace is gone
    assert!(
        !repo.path().join("ws").join("alice").exists(),
        "Workspace directory should be removed after destroy"
    );

    // Restore the workspace
    let restore_output = maw_ok(repo.path(), &["ws", "restore", "alice"]);
    assert!(
        restore_output.contains("restored") || restore_output.contains("Restored"),
        "Restore output should confirm restoration, got: {restore_output}"
    );

    // Verify the workspace directory exists again
    assert!(
        repo.path().join("ws").join("alice").exists(),
        "Workspace directory should exist after restore"
    );

    // Verify the file content is recovered
    let restored_content = read_from_ws(repo.path(), "alice", "feature.txt");
    assert_eq!(
        restored_content.as_deref(),
        Some("Alice's important work"),
        "Restored workspace should have the original file content"
    );

    // Verify workspace appears in list
    let list_output = maw_ok(repo.path(), &["ws", "list"]);
    assert!(
        list_output.contains("alice"),
        "Restored workspace should appear in list, got: {list_output}"
    );
}

#[test]
fn restore_already_existing_workspace_fails() {
    let repo = setup_bare_repo();

    // Create a workspace
    maw_ok(repo.path(), &["ws", "create", "bob"]);

    // Try to restore it (it still exists)
    let stderr = maw_fails(repo.path(), &["ws", "restore", "bob"]);
    assert!(
        stderr.contains("already exists"),
        "Should fail when workspace already exists, got: {stderr}"
    );
}

#[test]
fn restore_default_workspace_fails() {
    let repo = setup_bare_repo();

    let stderr = maw_fails(repo.path(), &["ws", "restore", "default"]);
    assert!(
        stderr.contains("default"),
        "Should refuse to restore default workspace, got: {stderr}"
    );
}

#[test]
#[ignore = "requires jj - being replaced by git-native tests (bd-2hw9.4)"]
fn restore_never_existed_workspace_fails() {
    let repo = setup_bare_repo();

    // Try to restore a workspace that was never created
    let stderr = maw_fails(repo.path(), &["ws", "restore", "phantom"]);
    assert!(
        stderr.contains("Could not find") || stderr.contains("op log"),
        "Should fail when no forget operation exists, got: {stderr}"
    );
}

#[test]
#[ignore = "requires jj - being replaced by git-native tests (bd-2hw9.4)"]
fn destroy_output_mentions_restore() {
    let repo = setup_bare_repo();

    maw_ok(repo.path(), &["ws", "create", "carol"]);

    let destroy_output = maw_ok(repo.path(), &["ws", "destroy", "carol"]);
    assert!(
        destroy_output.contains("maw ws restore carol"),
        "Destroy output should include exact restore command, got: {destroy_output}"
    );
}
