//! Integration tests for `maw push` in git-native Manifold repos.

mod manifold_common;

use std::process::Command;

use manifold_common::TestRepo;
use tempfile::TempDir;

fn clone_remote(remote: &std::path::Path) -> TempDir {
    let verify_dir = TempDir::new().expect("failed to create verify temp dir");
    let out = Command::new("git")
        .args(["clone", remote.to_str().unwrap(), "."])
        .current_dir(verify_dir.path())
        .output()
        .expect("failed to run git clone");
    assert!(
        out.status.success(),
        "Failed to clone remote for verification: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    verify_dir
}

#[test]
fn push_after_merge() {
    let (repo, remote) = TestRepo::with_remote();

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("alice", "feature.txt", "new feature\n");
    repo.maw_ok(&["ws", "merge", "alice", "--destroy"]);

    let output = repo.maw_ok(&["push"]);
    assert!(output.contains("push") || output.contains("origin"));

    let verify_dir = clone_remote(remote.path());
    let feature_file = verify_dir.path().join("feature.txt");
    assert!(feature_file.exists());
    let content = std::fs::read_to_string(&feature_file).unwrap();
    assert_eq!(content, "new feature\n");
}

#[test]
fn push_advance_moves_branch_to_current_epoch() {
    let (repo, remote) = TestRepo::with_remote();

    repo.add_file("default", "hotfix.txt", "urgent fix\n");
    repo.git_in_workspace("default", &["add", "hotfix.txt"]);
    repo.git_in_workspace("default", &["commit", "-m", "fix: hotfix"]);

    let detached_head = repo.workspace_head("default");
    repo.git(&[
        "update-ref",
        "refs/manifold/epoch/current",
        detached_head.as_str(),
    ]);
    repo.git(&["update-ref", "refs/heads/main", repo.epoch0()]);

    let output = repo.maw_ok(&["push", "--advance"]);
    assert!(output.contains("push") || output.contains("Advancing"));

    let verify_dir = clone_remote(remote.path());
    let hotfix = verify_dir.path().join("hotfix.txt");
    assert!(hotfix.exists());
    let content = std::fs::read_to_string(&hotfix).unwrap();
    assert_eq!(content, "urgent fix\n");
}
