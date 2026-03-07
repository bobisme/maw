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

#[test]
fn exec_does_not_auto_sync_unbound_workspace_to_active_change_epoch() {
    let repo = TestRepo::new();

    repo.seed_files(&[("README.md", "# app\n")]);

    repo.maw_ok(&[
        "changes",
        "create",
        "Flow",
        "--from",
        "main",
        "--id",
        "ch-sync",
        "--workspace",
        "ch-sync",
    ]);
    repo.maw_ok(&["ws", "create", "--change", "ch-sync", "worker"]);
    repo.add_file("worker", "src/feature.rs", "pub fn feature() {}\n");
    repo.git_in_workspace("worker", &["add", "-A"]);
    repo.git_in_workspace("worker", &["commit", "-m", "feat: worker change"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "worker",
        "--into",
        "ch-sync",
        "--destroy",
        "--message",
        "merge worker",
    ]);

    // Simulate legacy drift shape (epoch tracking active change branch) so the
    // cross-target guard path stays covered even after bn-3092.
    let change_head = repo
        .git(&["rev-parse", "refs/heads/feat/ch-sync-flow"])
        .trim()
        .to_owned();
    repo.git(&[
        "update-ref",
        "refs/manifold/epoch/current",
        &change_head,
    ]);

    repo.maw_ok(&["ws", "create", "--from", "main", "hotfix"]);
    let old_head = repo.workspace_head("hotfix");
    assert_ne!(old_head, repo.current_epoch());

    let out = repo.maw_raw(&["exec", "hotfix", "--", "git", "rev-parse", "HEAD"]);
    assert!(
        out.status.success(),
        "exec should still run command\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let new_head = repo.workspace_head("hotfix");
    assert_eq!(
        new_head, old_head,
        "auto-sync should be skipped for risky cross-target stale workspace"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Skipping auto-sync for this unbound workspace"),
        "expected explicit cross-target skip warning, got stderr: {stderr}"
    );
}

#[test]
fn sync_refuses_cross_target_update_for_unbound_workspace() {
    let repo = TestRepo::new();

    repo.seed_files(&[("README.md", "# app\n")]);

    repo.maw_ok(&[
        "changes",
        "create",
        "Flow",
        "--from",
        "main",
        "--id",
        "ch-sync2",
        "--workspace",
        "ch-sync2",
    ]);
    repo.maw_ok(&["ws", "create", "--change", "ch-sync2", "worker"]);
    repo.add_file("worker", "src/feature.rs", "pub fn feature() {}\n");
    repo.git_in_workspace("worker", &["add", "-A"]);
    repo.git_in_workspace("worker", &["commit", "-m", "feat: worker change"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "worker",
        "--into",
        "ch-sync2",
        "--destroy",
        "--message",
        "merge worker",
    ]);

    // Simulate legacy drift shape (epoch tracking active change branch) so the
    // cross-target guard path stays covered even after bn-3092.
    let change_head = repo
        .git(&["rev-parse", "refs/heads/feat/ch-sync2-flow"])
        .trim()
        .to_owned();
    repo.git(&[
        "update-ref",
        "refs/manifold/epoch/current",
        &change_head,
    ]);

    repo.maw_ok(&["ws", "create", "--from", "main", "hotfix"]);
    let old_head = repo.workspace_head("hotfix");

    let out = repo.maw_raw(&["ws", "sync", "hotfix"]);
    assert!(
        out.status.success(),
        "sync command should return success with safety refusal\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let new_head = repo.workspace_head("hotfix");
    assert_eq!(
        new_head, old_head,
        "cross-target safety should leave workspace base unchanged"
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Refusing to sync this unbound workspace"),
        "expected explicit sync refusal message, got stdout: {stdout}"
    );
}
