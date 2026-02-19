//! Tests for workspace name validation
//!
//! Each test creates an isolated git repo in a temp directory,
//! so no workspaces are created in the real repo.

mod common;

use common::{maw_fails, maw_in, setup_git_test_repo, setup_test_repo};

#[test]
fn rejects_empty_workspace_name() {
    let repo = setup_test_repo();
    let stderr = maw_fails(repo.path(), &["ws", "create", ""]);
    assert!(stderr.contains("cannot be empty"), "Got: {stderr}");
}

#[test]
fn rejects_path_traversal_dotdot() {
    let repo = setup_test_repo();
    let stderr = maw_fails(repo.path(), &["ws", "create", ".."]);
    assert!(stderr.contains("cannot be '.' or '..'"), "Got: {stderr}");
}

#[test]
fn rejects_path_traversal_slash() {
    let repo = setup_test_repo();
    let stderr = maw_fails(repo.path(), &["ws", "create", "../etc"]);
    assert!(stderr.contains("path separators"), "Got: {stderr}");
}

#[test]
fn rejects_path_traversal_backslash() {
    let repo = setup_test_repo();
    let stderr = maw_fails(repo.path(), &["ws", "create", "..\\etc"]);
    assert!(stderr.contains("path separators"), "Got: {stderr}");
}

#[test]
fn rejects_leading_dash() {
    let repo = setup_test_repo();
    let stderr = maw_fails(repo.path(), &["ws", "create", "--", "-rf"]);
    assert!(stderr.contains("cannot start with '-'"), "Got: {stderr}");
}

#[test]
fn rejects_spaces_in_name() {
    let repo = setup_test_repo();
    let stderr = maw_fails(repo.path(), &["ws", "create", "my workspace"]);
    assert!(stderr.contains("must contain only"), "Got: {stderr}");
}

#[test]
fn rejects_special_characters() {
    let repo = setup_test_repo();
    let stderr = maw_fails(repo.path(), &["ws", "create", "test@workspace"]);
    assert!(stderr.contains("must contain only"), "Got: {stderr}");
}

#[test]
fn allows_valid_names() {
    // Use git-native test repo (no jj) since workspace creation
    // now uses git worktree backend
    let repo = setup_git_test_repo();

    // Note: WorkspaceId only allows lowercase + digits + hyphens.
    // "agent_1" (underscore) and "Agent123" (uppercase) are rejected by WorkspaceId.
    let valid_names = ["alice", "agent-1", "my-agent"];

    for name in valid_names {
        let output = maw_in(repo.path(), &["ws", "create", name]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);

        assert!(
            output.status.success(),
            "Failed to create workspace '{name}':\nstdout: {stdout}\nstderr: {stderr}",
        );
    }
}
