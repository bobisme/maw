//! Tests for workspace name validation

use std::process::Command;

/// Get the repo root by finding the .jj directory that is NOT inside .workspaces.
/// Jj workspaces have their own .jj directory pointing back to the main repo,
/// so we need to find the actual repo root (the one with the backing store).
fn repo_root() -> std::path::PathBuf {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    // Walk up from manifest dir looking for .jj that's not in .workspaces
    for ancestor in manifest_dir.ancestors() {
        let jj_dir = ancestor.join(".jj");
        if jj_dir.exists() {
            // Check if we're inside .workspaces by looking at the path
            let path_str = ancestor.to_string_lossy();
            if !path_str.contains(".workspaces") {
                return ancestor.to_path_buf();
            }
        }
    }

    // Fallback: just use manifest dir (tests may fail with different error)
    manifest_dir
}

fn maw(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_maw"))
        .args(args)
        .current_dir(repo_root())
        .output()
        .expect("failed to execute maw")
}

fn maw_fails_with(args: &[&str], expected_error: &str) {
    let output = maw(args);
    assert!(
        !output.status.success(),
        "Expected command to fail: maw {}",
        args.join(" ")
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(expected_error),
        "Expected error containing '{}', got: {}",
        expected_error,
        stderr
    );
}

#[test]
fn rejects_empty_workspace_name() {
    maw_fails_with(&["ws", "create", ""], "cannot be empty");
}

#[test]
fn rejects_path_traversal_dotdot() {
    maw_fails_with(&["ws", "create", ".."], "cannot be '.' or '..'");
}

#[test]
fn rejects_path_traversal_slash() {
    maw_fails_with(&["ws", "create", "../etc"], "path separators");
}

#[test]
fn rejects_path_traversal_backslash() {
    maw_fails_with(&["ws", "create", "..\\etc"], "path separators");
}

#[test]
fn rejects_leading_dash() {
    maw_fails_with(&["ws", "create", "--", "-rf"], "cannot start with '-'");
}

#[test]
fn rejects_spaces_in_name() {
    maw_fails_with(&["ws", "create", "my workspace"], "must contain only");
}

#[test]
fn rejects_special_characters() {
    maw_fails_with(&["ws", "create", "test@workspace"], "must contain only");
}

#[test]
fn allows_valid_names() {
    // These should pass validation (but may fail for other reasons like "not in jj repo")
    // We just check they don't fail with validation errors
    let valid_names = ["alice", "agent_1", "my-agent", "Agent123"];

    for name in valid_names {
        let output = maw(&["ws", "create", name]);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Should NOT contain validation error messages
        assert!(
            !stderr.contains("cannot be empty")
                && !stderr.contains("path separators")
                && !stderr.contains("cannot start with")
                && !stderr.contains("must contain only"),
            "Valid name '{}' was incorrectly rejected: {}",
            name,
            stderr
        );
    }
}
