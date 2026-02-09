//! Tests for workspace name validation
//!
//! Each test creates an isolated jj repo in a temp directory,
//! so no workspaces are created in the real repo.

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

/// Create a fresh jj repo in a temp directory.
fn setup_test_repo() -> TempDir {
    let dir = TempDir::new().expect("failed to create temp dir");

    let out = Command::new("jj")
        .args(["git", "init"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run jj git init");
    assert!(
        out.status.success(),
        "jj git init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    dir
}

fn maw_in(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_maw"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to execute maw")
}

fn maw_fails_with(dir: &Path, args: &[&str], expected_error: &str) {
    let output = maw_in(dir, args);
    assert!(
        !output.status.success(),
        "Expected command to fail: maw {}",
        args.join(" ")
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(expected_error),
        "Expected error containing '{expected_error}', got: {stderr}",
    );
}

#[test]
fn rejects_empty_workspace_name() {
    let repo = setup_test_repo();
    maw_fails_with(repo.path(), &["ws", "create", ""], "cannot be empty");
}

#[test]
fn rejects_path_traversal_dotdot() {
    let repo = setup_test_repo();
    maw_fails_with(repo.path(), &["ws", "create", ".."], "cannot be '.' or '..'");
}

#[test]
fn rejects_path_traversal_slash() {
    let repo = setup_test_repo();
    maw_fails_with(repo.path(), &["ws", "create", "../etc"], "path separators");
}

#[test]
fn rejects_path_traversal_backslash() {
    let repo = setup_test_repo();
    maw_fails_with(
        repo.path(),
        &["ws", "create", "..\\etc"],
        "path separators",
    );
}

#[test]
fn rejects_leading_dash() {
    let repo = setup_test_repo();
    maw_fails_with(
        repo.path(),
        &["ws", "create", "--", "-rf"],
        "cannot start with '-'",
    );
}

#[test]
fn rejects_spaces_in_name() {
    let repo = setup_test_repo();
    maw_fails_with(
        repo.path(),
        &["ws", "create", "my workspace"],
        "must contain only",
    );
}

#[test]
fn rejects_special_characters() {
    let repo = setup_test_repo();
    maw_fails_with(
        repo.path(),
        &["ws", "create", "test@workspace"],
        "must contain only",
    );
}

#[test]
fn allows_valid_names() {
    let repo = setup_test_repo();
    let valid_names = ["alice", "agent_1", "my-agent", "Agent123"];

    for name in valid_names {
        let output = maw_in(repo.path(), &["ws", "create", name]);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Should NOT contain validation error messages
        assert!(
            !stderr.contains("cannot be empty")
                && !stderr.contains("path separators")
                && !stderr.contains("cannot start with")
                && !stderr.contains("must contain only"),
            "Valid name '{name}' was incorrectly rejected: {stderr}",
        );

        // Should succeed — workspace actually created in temp dir
        assert!(
            output.status.success(),
            "Failed to create workspace '{name}': {stderr}",
        );
    }
}
