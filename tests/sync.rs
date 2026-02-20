//! Tests for workspace sync and divergent resolution
//!
//! Each test creates an isolated bare maw repo in a temp directory.

mod common;

use common::*;

#[test]
#[ignore = "requires jj - being replaced by git-native tests (bd-2hw9.4)"]
fn stale_workspace_detected() {
    // Create a bare maw repo
    let repo = setup_bare_repo();

    // Create workspace "alice"
    maw_ok(repo.path(), &["ws", "create", "alice"]);

    // Make a change in default workspace that modifies shared history
    // This will cause alice's working copy to become stale
    write_in_ws(repo.path(), "default", "newfile.txt", "trigger stale");
    run_jj(&default_ws(repo.path()), &["commit", "-m", "trigger stale"]);

    // Check workspace status - should report alice as stale
    let status_output = maw_ok(repo.path(), &["ws", "status"]);

    // The status output should mention stale or indicate alice needs sync
    assert!(
        status_output.contains("stale")
            || status_output.contains("Stale")
            || status_output.contains("sync"),
        "Expected status to report stale workspace, got: {status_output}"
    );

    // Run sync from repo root - should succeed
    let sync_output = maw_ok(repo.path(), &["ws", "sync"]);

    // Verify sync completed successfully (may report "up to date" if default already synced)
    assert!(
        sync_output.contains("sync")
            || sync_output.contains("Sync")
            || sync_output.contains("updated")
            || sync_output.contains("Updated")
            || sync_output.contains("up to date"),
        "Expected sync success message, got: {sync_output}"
    );
}

#[test]
#[ignore = "requires jj - being replaced by git-native tests (bd-2hw9.4)"]
fn auto_sync_on_exec() {
    // Create a bare maw repo
    let repo = setup_bare_repo();

    // Create workspace "alice"
    maw_ok(repo.path(), &["ws", "create", "alice"]);

    // Make alice stale by committing in default workspace
    write_in_ws(repo.path(), "default", "trigger.txt", "make alice stale");
    run_jj(&default_ws(repo.path()), &["commit", "-m", "trigger stale"]);

    // Now run a jj command via maw exec in alice workspace
    // This should auto-sync the stale workspace before running the command
    let exec_output = maw_ok(repo.path(), &["exec", "alice", "--", "jj", "status"]);

    // The command should succeed (not fail with stale error)
    // The output should show jj status executed successfully
    assert!(
        exec_output.contains("working copy")
            || exec_output.contains("Working copy")
            || exec_output.contains("parent")
            || exec_output.contains("Parent"),
        "Expected jj status output, got: {exec_output}"
    );
}

#[test]
#[ignore = "requires jj - being replaced by git-native tests (bd-2hw9.4)"]
fn sync_resolves_divergent_identical() {
    // Create a bare maw repo
    let repo = setup_bare_repo();

    // Create workspace "bob"
    maw_ok(repo.path(), &["ws", "create", "bob"]);

    // Make a change in bob's workspace
    write_in_ws(repo.path(), "bob", "test.txt", "same content");
    run_jj(
        &repo.path().join("ws").join("bob"),
        &["describe", "-m", "test change"],
    );

    // Trigger stale by committing in default
    write_in_ws(repo.path(), "default", "other.txt", "other file");
    run_jj(&default_ws(repo.path()), &["commit", "-m", "trigger stale"]);

    // Running sync should handle any divergence automatically
    maw_ok(repo.path(), &["ws", "sync"]);

    // After sync, workspace status should be clean (no divergent warnings)
    let status_output = maw_ok(repo.path(), &["ws", "status"]);

    // Should not report divergent commits after sync
    assert!(
        !status_output.contains("divergent") && !status_output.contains("Divergent"),
        "Expected no divergent commits after sync, got: {status_output}"
    );
}
