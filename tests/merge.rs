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

    assert_eq!(
        repo.read_file("default", "alice.txt").as_deref(),
        Some("Alice's work\n")
    );
    assert_eq!(
        repo.read_file("default", "bob.txt").as_deref(),
        Some("Bob's work\n")
    );

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
    assert!(
        combined.contains("conflict"),
        "expected conflict output, got:\n{combined}"
    );

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

    assert_eq!(
        repo.read_file("default", "agent.txt").as_deref(),
        Some("agent work\n")
    );
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
fn merge_records_snapshot_and_merge_ops_in_workspace_history() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "worker"]);
    repo.add_file("worker", "result.txt", "worker output\n");

    repo.maw_ok(&["ws", "merge", "worker"]);

    let history = repo.maw_ok(&["ws", "history", "worker", "--format", "json"]);
    let payload: serde_json::Value =
        serde_json::from_str(&history).expect("history output should be valid JSON");
    let operations = payload["operations"]
        .as_array()
        .expect("history operations should be an array");

    assert!(
        operations
            .iter()
            .any(|op| op["op_type"].as_str() == Some("snapshot")),
        "expected at least one snapshot operation in history: {payload}"
    );
    assert!(
        operations
            .iter()
            .any(|op| op["op_type"].as_str() == Some("merge")),
        "expected at least one merge operation in history: {payload}"
    );
}

#[test]
fn reject_merge_default_workspace() {
    let repo = TestRepo::new();

    let stderr = repo.maw_fails(&["ws", "merge", "default"]);
    assert!(
        stderr.contains("default") || stderr.contains("reserved"),
        "Got: {stderr}"
    );
}

#[test]
fn merge_json_success_stdout_is_pure_json() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "json-a"]);
    repo.maw_ok(&["ws", "create", "json-b"]);
    repo.add_file("json-a", "a.txt", "a\n");
    repo.add_file("json-b", "b.txt", "b\n");

    let out = repo.maw_raw(&["ws", "merge", "json-a", "json-b", "--format", "json"]);
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "merge should succeed\nstderr: {stderr}"
    );
    assert!(
        stdout.starts_with('{'),
        "stdout should be pure JSON, got: {stdout}"
    );

    let payload: serde_json::Value =
        serde_json::from_str(&stdout).expect("merge --format json output should be valid JSON");
    assert_eq!(payload["status"].as_str(), Some("success"));
}

#[test]
fn merge_json_conflict_stdout_is_pure_json() {
    let repo = TestRepo::new();

    repo.seed_files(&[("shared.txt", "base\n")]);
    repo.maw_ok(&["ws", "create", "json-a"]);
    repo.maw_ok(&["ws", "create", "json-b"]);
    repo.modify_file("json-a", "shared.txt", "alpha\n");
    repo.modify_file("json-b", "shared.txt", "beta\n");

    let out = repo.maw_raw(&["ws", "merge", "json-a", "json-b", "--format", "json"]);
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();

    assert!(!out.status.success(), "conflicting merge should fail");
    assert!(
        stdout.starts_with('{'),
        "stdout should be pure JSON, got: {stdout}"
    );

    let payload: serde_json::Value =
        serde_json::from_str(&stdout).expect("merge conflict output should be valid JSON");
    assert_eq!(payload["status"].as_str(), Some("conflict"));
}

/// Regression test: merging a workspace should not fail when the current epoch
/// contains files that are absent from the workspace's working tree but also
/// absent from the workspace's *base* epoch.
///
/// Scenario (mirrors the real `cargo vendor` bug):
///
/// 1. Epoch advances (via another workspace merge) to include `vendor/pkg/.cargo-ok`.
/// 2. A stale worker workspace (base epoch = old epoch) never had this file in its
///    working tree.
/// 3. `git diff <new_epoch>` in the worker shows `D vendor/pkg/.cargo-ok` because
///    the file is in the new epoch tree but absent from the working tree.
/// 4. The patch-set builder previously called `git rev-parse <old_epoch>:vendor/pkg/.cargo-ok`,
///    which failed with "path does not exist" — crashing the merge.
/// 5. The fix: skip deletions where the file is absent at the workspace's base epoch
///    (add-then-delete net no-op from the base epoch's perspective).
#[test]
fn merge_skips_phantom_deletion_when_epoch_advanced() {
    let repo = TestRepo::new();

    // Create both workspaces before advancing the epoch.
    repo.maw_ok(&["ws", "create", "epoch-advancer"]);
    repo.maw_ok(&["ws", "create", "worker"]);

    // epoch-advancer brings in vendor/pkg/.cargo-ok and an ordinary file.
    repo.add_file("epoch-advancer", "vendor/pkg/.cargo-ok", "ok\n");
    repo.add_file("epoch-advancer", "src/lib.rs", "fn lib() {}\n");

    // Merging epoch-advancer advances the current epoch to E2, which now has
    // vendor/pkg/.cargo-ok. The worker workspace's base epoch is still E1.
    repo.maw_ok(&["ws", "merge", "epoch-advancer", "--destroy"]);

    // Worker does some unrelated work. After the epoch advanced, git diff
    // <new_epoch> in the worker shows vendor/pkg/.cargo-ok as D (present in
    // new epoch, absent from worker working tree). But the worker's base epoch
    // (E1) never had that file, so the old code crashed with "path does not exist".
    repo.add_file("worker", "worker.txt", "worker output\n");

    // This must not fail — the phantom deletion is silently skipped.
    repo.maw_ok(&["ws", "merge", "worker", "--destroy"]);

    // Worker's real changes are applied.
    assert_eq!(
        repo.read_file("default", "worker.txt").as_deref(),
        Some("worker output\n"),
        "worker.txt should be present after merge"
    );
    // The epoch-advancer's files are intact (not deleted by the worker merge).
    assert_eq!(
        repo.read_file("default", "vendor/pkg/.cargo-ok").as_deref(),
        Some("ok\n"),
        "vendor file added by epoch-advancer should survive the worker merge"
    );
    assert_eq!(
        repo.read_file("default", "src/lib.rs").as_deref(),
        Some("fn lib() {}\n"),
        "src/lib.rs added by epoch-advancer should survive the worker merge"
    );
}

/// When the default workspace has dirty (uncommitted) files at merge time,
/// the merge should record a Snapshot operation in the default workspace's
/// oplog capturing those dirty files before the checkout.
#[test]
fn merge_with_dirty_default_records_snapshot_op_in_default_oplog() {
    let repo = TestRepo::new();

    // Create a workspace and make changes.
    repo.maw_ok(&["ws", "create", "agent"]);
    repo.add_file("agent", "agent.txt", "agent work\n");

    // Add a dirty (untracked/uncommitted) file in the default workspace.
    repo.add_file("default", "local-notes.txt", "my local notes\n");

    // Merge the agent workspace.
    repo.maw_ok(&["ws", "merge", "agent", "--destroy"]);

    // The default workspace's oplog should contain a Snapshot operation.
    let history = repo.maw_ok(&["ws", "history", "default", "--format", "json"]);
    let payload: serde_json::Value =
        serde_json::from_str(&history).expect("history output should be valid JSON");
    let operations = payload["operations"]
        .as_array()
        .expect("history operations should be an array");

    assert!(
        operations
            .iter()
            .any(|op| op["op_type"].as_str() == Some("snapshot")),
        "expected a snapshot operation in default workspace history when dirty: {payload}"
    );

    // The dirty file should still be present after merge (replayed).
    assert_eq!(
        repo.read_file("default", "local-notes.txt").as_deref(),
        Some("my local notes\n"),
        "dirty file should be preserved after merge"
    );
}

/// When the default workspace is clean (no uncommitted changes) at merge time,
/// no Snapshot operation should be recorded in the default workspace's oplog.
#[test]
fn merge_with_clean_default_does_not_record_snapshot_op() {
    let repo = TestRepo::new();

    // Seed a file so the workspace has content but is clean.
    repo.seed_files(&[("README.md", "# Test\n")]);

    // Create a workspace and make changes.
    repo.maw_ok(&["ws", "create", "agent"]);
    repo.add_file("agent", "agent.txt", "agent work\n");

    // Do NOT add any dirty files to the default workspace.

    // Merge the agent workspace.
    repo.maw_ok(&["ws", "merge", "agent", "--destroy"]);

    // The default workspace's oplog should NOT contain a Snapshot operation.
    let history_result = repo.maw_raw(&["ws", "history", "default", "--format", "json"]);
    let stdout = String::from_utf8_lossy(&history_result.stdout);

    if history_result.status.success() {
        let payload: serde_json::Value =
            serde_json::from_str(&stdout).expect("history output should be valid JSON");

        if let Some(operations) = payload["operations"].as_array() {
            assert!(
                !operations
                    .iter()
                    .any(|op| op["op_type"].as_str() == Some("snapshot")),
                "expected NO snapshot operation in default workspace history when clean: {payload}"
            );
        }
    }
    // If `ws history default` fails (no oplog exists), that's also correct —
    // no snapshot was recorded.
}
