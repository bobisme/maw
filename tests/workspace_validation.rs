//! Tests for workspace name validation
//!
//! Each test creates an isolated jj repo in a temp directory,
//! so no workspaces are created in the real repo.

mod common;

use common::{maw_fails, maw_in, setup_test_repo};

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
    let repo = setup_test_repo();
    let valid_names = ["alice", "agent_1", "my-agent", "Agent123"];

    for name in valid_names {
        let output = maw_in(repo.path(), &["ws", "create", name]);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(
            !stderr.contains("cannot be empty")
                && !stderr.contains("path separators")
                && !stderr.contains("cannot start with")
                && !stderr.contains("must contain only"),
            "Valid name '{name}' was incorrectly rejected: {stderr}",
        );

        assert!(
            output.status.success(),
            "Failed to create workspace '{name}': {stderr}",
        );
    }
}
