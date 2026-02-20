//! Integration tests for `maw ws merge` on the git-native Manifold model.

mod manifold_common;

use manifold_common::TestRepo;

fn workspace_names(repo: &TestRepo) -> Vec<String> {
    let listed = repo.maw_ok(&["ws", "list", "--format", "json"]);
    let listed_json: serde_json::Value =
        serde_json::from_str(&listed).expect("ws list --format json should be valid JSON");
    listed_json["workspaces"]
        .as_array()
        .expect("workspaces should be an array")
        .iter()
        .filter_map(|w| w["name"].as_str().map(ToOwned::to_owned))
        .collect()
}

#[test]
fn basic_merge_destroy_two_workspaces() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.maw_ok(&["ws", "create", "bob"]);

    repo.add_file("alice", "alice.txt", "Alice's work\n");
    repo.add_file("bob", "bob.txt", "Bob's work\n");

    repo.maw_ok(&["ws", "merge", "alice", "bob", "--destroy"]);

    assert_eq!(repo.read_file("default", "alice.txt").as_deref(), Some("Alice's work\n"));
    assert_eq!(repo.read_file("default", "bob.txt").as_deref(), Some("Bob's work\n"));

    let names = workspace_names(&repo);
    assert!(names.contains(&"default".to_owned()));
    assert!(!names.contains(&"alice".to_owned()));
    assert!(!names.contains(&"bob".to_owned()));
}

#[test]
fn merge_conflict_preserves_source_workspaces() {
    let repo = TestRepo::new();

    repo.seed_files(&[("shared.txt", "base\n")]);
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.maw_ok(&["ws", "create", "bob"]);

    repo.modify_file("alice", "shared.txt", "alice\n");
    repo.modify_file("bob", "shared.txt", "bob\n");

    let out = repo.maw_raw(&["ws", "merge", "alice", "bob", "--destroy"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}\n{stderr}").to_lowercase();

    assert!(!out.status.success(), "conflicting merge should fail");
    assert!(combined.contains("conflict"), "expected conflict output, got:\n{combined}");

    let names = workspace_names(&repo);
    assert!(names.contains(&"alice".to_owned()));
    assert!(names.contains(&"bob".to_owned()));
}

#[test]
fn merge_preserves_dirty_default_changes() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "agent"]);
    repo.add_file("agent", "agent.txt", "agent work\n");

    repo.add_file("default", "local.txt", "local default edits\n");

    repo.maw_ok(&["ws", "merge", "agent", "--destroy"]);

    assert_eq!(repo.read_file("default", "agent.txt").as_deref(), Some("agent work\n"));
    assert_eq!(
        repo.read_file("default", "local.txt").as_deref(),
        Some("local default edits\n")
    );
}

#[test]
fn merge_captures_source_workspace_edits_without_extra_vcs_commands() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "worker"]);
    repo.add_file("worker", "result.txt", "worker output\n");

    repo.maw_ok(&["ws", "merge", "worker", "--destroy"]);

    assert_eq!(
        repo.read_file("default", "result.txt").as_deref(),
        Some("worker output\n")
    );
}

#[test]
fn reject_merge_default_workspace() {
    let repo = TestRepo::new();

    let stderr = repo.maw_fails(&["ws", "merge", "default"]);
    assert!(stderr.contains("default") || stderr.contains("reserved"), "Got: {stderr}");
}
