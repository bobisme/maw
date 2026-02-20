//! Integration tests for `maw ws undo`.

mod manifold_common;

use std::process::Command;

use manifold_common::TestRepo;

#[test]
fn undo_reverts_added_modified_deleted_and_renamed_paths() {
    let repo = TestRepo::new();

    repo.seed_files(&[
        ("src/lib.rs", "pub fn value() -> i32 {\n    1\n}\n"),
        ("to-delete.txt", "keep me\n"),
        ("rename-me.txt", "original\n"),
    ]);

    repo.create_workspace("agent");
    repo.modify_file("agent", "src/lib.rs", "pub fn value() -> i32 {\n    2\n}\n");
    repo.add_file("agent", "new.txt", "new file\n");
    repo.delete_file("agent", "to-delete.txt");

    std::fs::rename(
        repo.workspace_path("agent").join("rename-me.txt"),
        repo.workspace_path("agent").join("renamed.txt"),
    )
    .unwrap();

    repo.maw_ok(&["ws", "undo", "agent"]);

    assert_eq!(
        repo.read_file("agent", "src/lib.rs").as_deref(),
        Some("pub fn value() -> i32 {\n    1\n}\n")
    );
    assert!(!repo.file_exists("agent", "new.txt"));
    assert_eq!(
        repo.read_file("agent", "to-delete.txt").as_deref(),
        Some("keep me\n")
    );
    assert!(repo.file_exists("agent", "rename-me.txt"));
    assert!(!repo.file_exists("agent", "renamed.txt"));

    let touched = repo.maw_ok(&["ws", "touched", "agent", "--format", "json"]);
    let touched_json: serde_json::Value =
        serde_json::from_str(&touched).expect("ws touched --format json should be valid JSON");
    assert_eq!(touched_json["touched_count"], 0);
}

#[test]
fn undo_records_compensate_operation_in_workspace_oplog() {
    let repo = TestRepo::new();
    repo.seed_files(&[("file.txt", "base\n")]);

    repo.create_workspace("agent");
    repo.modify_file("agent", "file.txt", "changed\n");

    repo.maw_ok(&["ws", "undo", "agent"]);

    let head = repo.git(&["rev-parse", "refs/manifold/head/agent"]);
    let head = head.trim();

    let op_json = repo.git(&["cat-file", "-p", head]);
    let op: serde_json::Value =
        serde_json::from_str(&op_json).expect("head operation should be valid JSON");

    assert_eq!(op["payload"]["type"].as_str(), Some("compensate"));
    let reason = op["payload"]["reason"]
        .as_str()
        .expect("compensate reason should be present");
    assert!(reason.contains("undo"));

    let parent = op["parent_ids"]
        .as_array()
        .and_then(|parents| parents.first())
        .and_then(|p| p.as_str())
        .expect("compensate op should have one parent");

    let parent_json = repo.git(&["cat-file", "-p", parent]);
    let parent_op: serde_json::Value =
        serde_json::from_str(&parent_json).expect("parent operation should be valid JSON");
    assert_eq!(parent_op["payload"]["type"].as_str(), Some("create"));
}

#[test]
fn undo_no_changes_is_noop_and_does_not_create_oplog_head() {
    let repo = TestRepo::new();
    repo.create_workspace("agent");

    let stdout = repo.maw_ok(&["ws", "undo", "agent"]);
    assert!(stdout.contains("No local changes to undo"));

    let output = Command::new("git")
        .args(["rev-parse", "refs/manifold/head/agent"])
        .current_dir(repo.root())
        .output()
        .expect("failed to run git rev-parse for op log head");
    assert!(
        !output.status.success(),
        "undo noop should not create op log head"
    );
}
