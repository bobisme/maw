//! Integration tests for `maw release` in git-native Manifold repos.

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
fn release_does_not_rewind_branch_when_branch_ahead_of_epoch() {
    let (repo, remote) = TestRepo::with_remote();

    repo.add_file("default", "release-note.txt", "release from branch tip\n");
    repo.git_in_workspace("default", &["add", "release-note.txt"]);
    repo.git_in_workspace("default", &["commit", "-m", "chore: release prep"]);

    let branch_tip = repo.workspace_head("default");
    repo.git(&["update-ref", "refs/heads/main", branch_tip.as_str()]);
    // Leave refs/manifold/epoch/current at epoch0 to simulate stale epoch ref.

    let output = repo.maw_ok(&["release", "v9.9.9"]);
    assert!(output.contains("Not rewinding") || output.contains("stale"));

    let main_after = repo
        .git(&["rev-parse", "refs/heads/main"])
        .trim()
        .to_string();
    assert_eq!(main_after, branch_tip, "release must not rewind branch ref");

    let local_tag_target = repo
        .git(&["rev-parse", "refs/tags/v9.9.9"])
        .trim()
        .to_string();
    assert_eq!(
        local_tag_target, branch_tip,
        "release tag should point at branch tip"
    );

    let verify_dir = clone_remote(remote.path());
    let tag_target = Command::new("git")
        .args(["rev-parse", "refs/tags/v9.9.9"])
        .current_dir(verify_dir.path())
        .output()
        .expect("failed to resolve remote tag target");
    assert!(tag_target.status.success());
    let remote_tag_oid = String::from_utf8_lossy(&tag_target.stdout)
        .trim()
        .to_string();
    assert_eq!(
        remote_tag_oid, branch_tip,
        "remote tag should match branch tip"
    );

    let pushed = verify_dir.path().join("release-note.txt");
    assert!(pushed.exists(), "release commit should be pushed");
    let content = std::fs::read_to_string(&pushed).unwrap();
    assert_eq!(content, "release from branch tip\n");
}
