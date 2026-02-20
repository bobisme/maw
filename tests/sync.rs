//! Integration tests for workspace staleness and sync behavior.

mod manifold_common;

use manifold_common::TestRepo;

fn workspace_state(repo: &TestRepo, name: &str) -> String {
    let status = repo.maw_ok(&["ws", "status", "--format", "json"]);
    let status_json: serde_json::Value =
        serde_json::from_str(&status).expect("ws status --format json should be valid JSON");
    status_json["workspaces"]
        .as_array()
        .expect("workspaces should be an array")
        .iter()
        .find(|w| w["name"].as_str() == Some(name))
        .and_then(|w| w["state"].as_str())
        .unwrap_or_default()
        .to_string()
}

#[test]
fn stale_workspace_detected_and_sync_clears_it() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("default", "advance.txt", "epoch advance\n");
    repo.advance_epoch("chore: advance epoch");

    assert!(workspace_state(&repo, "alice").contains("stale"));

    repo.maw_ok(&["ws", "sync", "--all"]);
    assert!(!workspace_state(&repo, "alice").contains("stale"));
}

#[test]
fn exec_auto_syncs_stale_workspace_before_running_command() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("default", "advance.txt", "epoch advance\n");
    repo.advance_epoch("chore: advance epoch");

    let old_head = repo.workspace_head("alice");
    assert_ne!(old_head, repo.current_epoch());

    repo.maw_ok(&["exec", "alice", "--", "git", "rev-parse", "HEAD"]);

    let new_head = repo.workspace_head("alice");
    assert_eq!(new_head, repo.current_epoch());
}

#[test]
fn sync_all_updates_multiple_stale_workspaces() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.maw_ok(&["ws", "create", "bob"]);

    repo.add_file("default", "advance.txt", "epoch advance\n");
    repo.advance_epoch("chore: advance epoch");

    repo.maw_ok(&["ws", "sync", "--all"]);

    assert!(!workspace_state(&repo, "alice").contains("stale"));
    assert!(!workspace_state(&repo, "bob").contains("stale"));
}
