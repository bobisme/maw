//! Integration tests for `maw push`
//!
//! Tests push workflow with a local bare git remote.
//! Each test creates an isolated jj repo with a remote in temp directories.

mod common;

use common::{default_ws, maw_ok, run_jj, setup_with_remote, write_in_ws};
use tempfile::TempDir;

#[test]
fn push_after_merge() {
    let (repo, remote) = setup_with_remote();

    // Create workspace and add a feature
    maw_ok(repo.path(), &["ws", "create", "alice"]);
    write_in_ws(repo.path(), "alice", "feature.txt", "new feature");

    // Describe the work
    let alice_ws = repo.path().join("ws").join("alice");
    run_jj(&alice_ws, &["describe", "-m", "feat: add feature"]);

    // Merge the workspace
    maw_ok(repo.path(), &["ws", "merge", "alice", "--destroy"]);

    // Push to remote
    let output = maw_ok(repo.path(), &["push"]);
    assert!(
        output.contains("main") || output.contains("push"),
        "Expected push confirmation, got: {output}"
    );

    // Verify the push landed by cloning the remote
    let verify_dir = TempDir::new().unwrap();
    let out = std::process::Command::new("git")
        .args(["clone", &remote.path().display().to_string(), "."])
        .current_dir(verify_dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "Failed to clone remote for verification: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Check the feature file exists in the clone
    let feature_file = verify_dir.path().join("feature.txt");
    assert!(
        feature_file.exists(),
        "feature.txt should exist in remote after push"
    );
    let content = std::fs::read_to_string(&feature_file).unwrap();
    assert_eq!(content, "new feature");
}

#[test]
fn push_advance() {
    let (repo, remote) = setup_with_remote();

    // Write directly in default workspace (simulating hotfix workflow)
    write_in_ws(repo.path(), "default", "hotfix.txt", "urgent fix");

    // Describe and commit from default workspace
    let ws = default_ws(repo.path());
    run_jj(&ws, &["describe", "-m", "fix: hotfix"]);
    run_jj(&ws, &["commit", "-m", "fix: hotfix"]);

    // Push with --advance flag
    let output = maw_ok(repo.path(), &["push", "--advance"]);
    assert!(
        output.contains("main") || output.contains("push"),
        "Expected push confirmation, got: {output}"
    );

    // Verify the push landed by cloning the remote
    let verify_dir = TempDir::new().unwrap();
    let out = std::process::Command::new("git")
        .args(["clone", &remote.path().display().to_string(), "."])
        .current_dir(verify_dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "Failed to clone remote for verification: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Check the hotfix file exists in the clone
    let hotfix_file = verify_dir.path().join("hotfix.txt");
    assert!(
        hotfix_file.exists(),
        "hotfix.txt should exist in remote after push --advance"
    );
    let content = std::fs::read_to_string(&hotfix_file).unwrap();
    assert_eq!(content, "urgent fix");
}
