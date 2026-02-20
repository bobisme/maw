//! Tests for workspace name validation.

mod manifold_common;

use manifold_common::TestRepo;

#[test]
fn rejects_empty_workspace_name() {
    let repo = TestRepo::new();
    let stderr = repo.maw_fails(&["ws", "create", ""]);
    assert!(stderr.contains("cannot be empty"), "Got: {stderr}");
}

#[test]
fn rejects_path_traversal_dotdot() {
    let repo = TestRepo::new();
    let stderr = repo.maw_fails(&["ws", "create", ".."]);
    assert!(stderr.contains("cannot be '.' or '..'"), "Got: {stderr}");
}

#[test]
fn rejects_path_traversal_slash() {
    let repo = TestRepo::new();
    let stderr = repo.maw_fails(&["ws", "create", "../etc"]);
    assert!(stderr.contains("path separators"), "Got: {stderr}");
}

#[test]
fn rejects_path_traversal_backslash() {
    let repo = TestRepo::new();
    let stderr = repo.maw_fails(&["ws", "create", "..\\etc"]);
    assert!(stderr.contains("path separators"), "Got: {stderr}");
}

#[test]
fn rejects_leading_dash() {
    let repo = TestRepo::new();
    let stderr = repo.maw_fails(&["ws", "create", "--", "-rf"]);
    assert!(stderr.contains("cannot start with '-'"), "Got: {stderr}");
}

#[test]
fn rejects_spaces_in_name() {
    let repo = TestRepo::new();
    let stderr = repo.maw_fails(&["ws", "create", "my workspace"]);
    assert!(stderr.contains("must contain only"), "Got: {stderr}");
}

#[test]
fn rejects_special_characters() {
    let repo = TestRepo::new();
    let stderr = repo.maw_fails(&["ws", "create", "test@workspace"]);
    assert!(stderr.contains("must contain only"), "Got: {stderr}");
}

#[test]
fn allows_valid_names() {
    let repo = TestRepo::new();
    let valid_names = ["alice", "agent-1", "my-agent"];

    for name in valid_names {
        let output = repo.maw_raw(&["ws", "create", name]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "Failed to create workspace '{name}':\nstdout: {stdout}\nstderr: {stderr}",
        );
    }
}
