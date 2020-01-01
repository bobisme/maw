//! Integration tests for recovery reference capture durability.

mod manifold_common;

use manifold_common::TestRepo;
use std::thread::sleep;
use std::time::Duration;

/// Collect all recovery refs for a workspace.
fn recovery_refs(repo: &TestRepo, workspace: &str) -> Vec<String> {
    let output = repo.git(&["for-each-ref", "--format=%(refname)", "refs/manifold/recovery/"]);
    output
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with(&format!("refs/manifold/recovery/{workspace}/")))
        .map(ToOwned::to_owned)
        .collect()
}

/// Resolve a ref to a full commit SHA.
fn resolve_ref_oid(repo: &TestRepo, git_ref: &str) -> String {
    repo
        .git(&["rev-parse", "--verify", git_ref])
        .trim()
        .to_owned()
}

#[test]
fn capture_ref_is_created_on_destroy_for_dirty_workspace() {
    let repo = TestRepo::new();

    repo.create_workspace("capture-dirty");
    repo.add_file("capture-dirty", "dirty.txt", "important change\n");

    repo.maw_ok(&["ws", "destroy", "capture-dirty", "--force"]);

    let refs = recovery_refs(&repo, "capture-dirty");
    assert_eq!(
        refs.len(),
        1,
        "Expected exactly one recovery ref for a single dirty capture, got: {refs:?}"
    );

    let commit = resolve_ref_oid(&repo, &refs[0]);
    let files = repo.git(&["ls-tree", "-r", "--name-only", &commit]);
    assert!(
        files.lines().any(|line| line == "dirty.txt"),
        "Captured commit {commit} should include dirty.txt"
    );
}

#[test]
fn capture_commit_is_durable_after_aggressive_gc() {
    let repo = TestRepo::new();

    repo.create_workspace("capture-gc");
    repo.add_file("capture-gc", "snapshot.txt", "preserve me\n");

    repo.maw_ok(&["ws", "destroy", "capture-gc", "--force"]);

    let refs_before = recovery_refs(&repo, "capture-gc");
    assert_eq!(
        refs_before.len(),
        1,
        "Expected one recovery ref before GC, got: {refs_before:?}"
    );

    repo.git(&["gc", "--aggressive", "--prune=now"]);

    let refs_after = recovery_refs(&repo, "capture-gc");
    assert_eq!(
        refs_after.len(),
        1,
        "Expected recovery ref to survive GC, got: {refs_after:?}"
    );

    let commit = resolve_ref_oid(&repo, &refs_after[0]);
    let contents = repo.git(&["show", &format!("{commit}:snapshot.txt")]);
    assert_eq!(
        contents.trim(),
        "preserve me",
        "Captured file content should survive aggressive GC"
    );
}

#[test]
fn repeated_destroy_cycles_preserve_capture_history() {
    let repo = TestRepo::new();

    repo.create_workspace("repeat-cycle");
    repo.add_file("repeat-cycle", "first-cycle.txt", "first snapshot\n");
    repo.maw_ok(&["ws", "destroy", "repeat-cycle", "--force"]);

    let refs_after_first = recovery_refs(&repo, "repeat-cycle");
    assert_eq!(
        refs_after_first.len(),
        1,
        "Expected a recovery ref from first destroy, got: {refs_after_first:?}"
    );
    let first_ref = refs_after_first[0].clone();
    let first_commit = resolve_ref_oid(&repo, &first_ref);

    // Ensure a different recovery ref name across cycles (timestamp-based).
    sleep(Duration::from_secs(1));

    // Re-create the same workspace name and destroy again with new dirty changes.
    repo.create_workspace("repeat-cycle");
    repo.add_file("repeat-cycle", "second-cycle.txt", "second snapshot\n");
    repo.maw_ok(&["ws", "destroy", "repeat-cycle", "--force"]);

    let refs_after_second = recovery_refs(&repo, "repeat-cycle");
    assert_eq!(
        refs_after_second.len(),
        2,
        "Expected two recovery refs after repeated destroy cycles, got: {refs_after_second:?}"
    );
    assert!(
        refs_after_second.iter().any(|r| r == &first_ref),
        "First capture ref should still exist: {refs_after_second:?}"
    );

    let second_ref = refs_after_second
        .iter()
        .find(|r| *r != &first_ref)
        .cloned()
        .expect("should have a second unique recovery ref");
    let second_commit = resolve_ref_oid(&repo, &second_ref);

    let first_files = repo.git(&["ls-tree", "-r", "--name-only", &first_commit]);
    let second_files = repo.git(&["ls-tree", "-r", "--name-only", &second_commit]);

    assert!(
        first_files.lines().any(|line| line == "first-cycle.txt"),
        "First captured commit should include first-cycle.txt"
    );
    assert!(
        second_files.lines().any(|line| line == "second-cycle.txt"),
        "Second captured commit should include second-cycle.txt"
    );

    // Recreate the workspace so we can read history after captures.
    repo.create_workspace("repeat-cycle");
    let history = repo.maw_ok(&["ws", "history", "repeat-cycle", "--format", "json"]);
    let history_json: serde_json::Value =
        serde_json::from_str(&history).expect("ws history --format json should parse");
    let destroy_count = history_json["operations"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter(|op| op["op_type"] == "destroy")
        .count();

    assert!(
        destroy_count >= 2,
        "Expected at least two destroy history entries, got: {destroy_count}"
    );
}

#[test]
fn clean_workspace_destroy_does_not_create_recovery_ref() {
    let repo = TestRepo::new();

    repo.create_workspace("clean-no-capture");
    repo.maw_ok(&["ws", "destroy", "clean-no-capture"]);

    let refs = recovery_refs(&repo, "clean-no-capture");
    assert!(
        refs.is_empty(),
        "Clean workspace at epoch should not pin a recovery ref, got: {refs:?}"
    );
}
