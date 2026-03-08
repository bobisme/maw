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

    repo.maw_ok(&[
        "ws",
        "merge",
        "alice",
        "bob",
        "--destroy",
        "--message",
        "test merge",
    ]);

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

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "alice",
        "bob",
        "--destroy",
        "--message",
        "test merge",
    ]);
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

    repo.maw_ok(&[
        "ws",
        "merge",
        "agent",
        "--destroy",
        "--message",
        "test merge",
    ]);

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

    repo.maw_ok(&[
        "ws",
        "merge",
        "worker",
        "--destroy",
        "--message",
        "test merge",
    ]);

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

    repo.maw_ok(&["ws", "merge", "worker", "--message", "test merge"]);

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
fn annotate_payload_is_visible_in_history_json() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "worker"]);
    repo.maw_ok(&[
        "ws",
        "annotate",
        "worker",
        "test-results",
        r#"{"passed":42,"failed":0}"#,
    ]);

    let history = repo.maw_ok(&["ws", "history", "worker", "--format", "json"]);
    let payload: serde_json::Value =
        serde_json::from_str(&history).expect("history output should be valid JSON");
    let operations = payload["operations"]
        .as_array()
        .expect("history operations should be an array");

    let annotate = operations
        .iter()
        .find(|op| op["op_type"].as_str() == Some("annotate"))
        .expect("history should include annotate operation");
    assert_eq!(annotate["annotation_key"].as_str(), Some("test-results"));
    assert_eq!(annotate["annotation_data"]["passed"].as_u64(), Some(42));
    assert_eq!(annotate["annotation_data"]["failed"].as_u64(), Some(0));
}

#[test]
fn reject_merge_default_workspace() {
    let repo = TestRepo::new();

    let stderr = repo.maw_fails(&["ws", "merge", "default", "--message", "test merge"]);
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

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "json-a",
        "json-b",
        "--format",
        "json",
        "--message",
        "test merge",
    ]);
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
    let advice = payload["advice"]
        .as_array()
        .expect("merge success JSON should include advice array");
    assert!(
        advice.is_empty(),
        "expected no advice when --message is provided, got: {advice:?}"
    );
}

#[test]
fn merge_without_message_fails_in_non_tty() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "guidance-a"]);
    repo.add_file("guidance-a", "note.txt", "content\n");

    let stderr = repo.maw_fails(&["ws", "merge", "guidance-a"]);
    assert!(
        stderr.contains("No --message provided"),
        "expected message-required error, got:\n{stderr}"
    );
    assert!(
        stderr.contains("--message"),
        "expected --message usage hint in error, got:\n{stderr}"
    );
}

#[test]
fn merge_json_conflict_stdout_is_pure_json() {
    let repo = TestRepo::new();

    repo.seed_files(&[("shared.txt", "base\n")]);
    repo.maw_ok(&["ws", "create", "json-a"]);
    repo.maw_ok(&["ws", "create", "json-b"]);
    repo.modify_file("json-a", "shared.txt", "alpha\n");
    repo.modify_file("json-b", "shared.txt", "beta\n");

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "json-a",
        "json-b",
        "--format",
        "json",
        "--message",
        "test merge",
    ]);
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

#[test]
fn merge_dry_run_json_stdout_is_pure_json() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "json-dry-run"]);
    repo.add_file("json-dry-run", "dry.txt", "preview\n");

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "json-dry-run",
        "--into",
        "default",
        "--dry-run",
        "--format",
        "json",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();

    assert!(
        out.status.success(),
        "dry-run should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.starts_with('{'),
        "stdout should be pure JSON, got: {stdout}"
    );

    let payload: serde_json::Value =
        serde_json::from_str(&stdout).expect("dry-run --format json output should be valid JSON");
    assert_eq!(payload["status"].as_str(), Some("dry-run"));
    assert_eq!(payload["dry_run"].as_bool(), Some(true));
    assert_eq!(payload["into"].as_str(), Some("default"));
    assert!(
        payload["workspace_changes"].is_array(),
        "expected workspace_changes in dry-run JSON: {payload}"
    );
}

/// Regression: stale workspaces must be refreshed before merge so they cannot
/// accidentally rewrite newer epoch content.
#[test]
fn stale_workspace_merge_is_blocked_when_epoch_has_advanced() {
    let repo = TestRepo::new();

    // Create both workspaces before advancing epoch.
    repo.maw_ok(&["ws", "create", "epoch-advancer"]);
    repo.maw_ok(&["ws", "create", "worker"]);

    // Advance epoch with files worker was not based on.
    repo.add_file("epoch-advancer", "vendor/pkg/.cargo-ok", "ok\n");
    repo.add_file("epoch-advancer", "src/lib.rs", "fn lib() {}\n");
    repo.maw_ok(&[
        "ws",
        "merge",
        "epoch-advancer",
        "--destroy",
        "--message",
        "test merge",
    ]);

    // Worker has committed work on the stale base.
    repo.add_file("worker", "worker.txt", "worker output\n");
    repo.git_in_workspace("worker", &["add", "worker.txt"]);
    repo.git_in_workspace("worker", &["commit", "-m", "feat: worker output"]);

    // Direct merge from stale workspace is rejected with actionable guidance.
    let stale_merge = repo.maw_raw(&["ws", "merge", "worker", "--message", "test merge"]);
    assert!(
        !stale_merge.status.success(),
        "stale merge should be blocked\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&stale_merge.stdout),
        String::from_utf8_lossy(&stale_merge.stderr)
    );
    let stale_err = String::from_utf8_lossy(&stale_merge.stderr);
    assert!(
        stale_err.contains("is stale") && stale_err.contains("maw ws sync worker"),
        "expected stale remediation guidance, got: {stale_err}"
    );

    // The epoch-advancer's files stay intact after refused stale merge.
    assert_eq!(
        repo.read_file("default", "vendor/pkg/.cargo-ok").as_deref(),
        Some("ok\n"),
        "vendor file added by epoch-advancer should survive stale-merge refusal"
    );
    assert_eq!(
        repo.read_file("default", "src/lib.rs").as_deref(),
        Some("fn lib() {}\n"),
        "src/lib.rs added by epoch-advancer should survive stale-merge refusal"
    );
    assert_eq!(
        repo.read_file("default", "worker.txt"),
        None,
        "stale workspace changes should not be merged when merge is blocked"
    );
}

#[test]
fn stale_workspace_is_blocked_for_check_plan_dry_run_and_merge() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "base-advancer"]);
    repo.maw_ok(&["ws", "create", "stale"]);

    repo.add_file("base-advancer", "new.txt", "new epoch\n");
    repo.maw_ok(&[
        "ws",
        "merge",
        "base-advancer",
        "--destroy",
        "--message",
        "advance epoch",
    ]);

    repo.add_file("stale", "work.txt", "stale work\n");
    repo.git_in_workspace("stale", &["add", "work.txt"]);
    repo.git_in_workspace("stale", &["commit", "-m", "feat: stale work"]);

    let check = repo.maw_raw(&[
        "ws", "merge", "stale", "--into", "default", "--check", "--format", "json",
    ]);
    assert!(
        !check.status.success(),
        "stale check should fail\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
    let check_json: serde_json::Value =
        serde_json::from_slice(&check.stdout).expect("check output should be JSON");
    assert_eq!(check_json["stale"].as_bool(), Some(true));

    let plan = repo.maw_raw(&[
        "ws", "merge", "stale", "--into", "default", "--plan", "--format", "json",
    ]);
    assert!(
        !plan.status.success(),
        "stale plan should fail\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&plan.stdout),
        String::from_utf8_lossy(&plan.stderr)
    );
    let plan_json: serde_json::Value =
        serde_json::from_slice(&plan.stdout).expect("plan output should be JSON");
    assert_eq!(plan_json["ready"].as_bool(), Some(false));
    assert_eq!(plan_json["stale"].as_bool(), Some(true));
    assert!(
        plan_json["conflicts"]
            .as_array()
            .is_some_and(|conflicts| conflicts.is_empty()),
        "stale plan JSON should not synthesize conflicts, got: {plan_json}"
    );

    let dry_run = repo.maw_raw(&["ws", "merge", "stale", "--into", "default", "--dry-run"]);
    assert!(
        !dry_run.status.success(),
        "stale dry-run should fail\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&dry_run.stdout),
        String::from_utf8_lossy(&dry_run.stderr)
    );
    assert!(
        String::from_utf8_lossy(&dry_run.stderr).contains("is stale"),
        "expected stale guidance in dry-run stderr"
    );

    let merge = repo.maw_raw(&[
        "ws",
        "merge",
        "stale",
        "--into",
        "default",
        "--message",
        "attempt stale merge",
    ]);
    assert!(
        !merge.status.success(),
        "stale merge should fail\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&merge.stdout),
        String::from_utf8_lossy(&merge.stderr)
    );
    let merge_err = String::from_utf8_lossy(&merge.stderr);
    assert!(
        merge_err.contains("is stale") && merge_err.contains("maw ws sync stale"),
        "expected stale remediation in merge stderr, got: {merge_err}"
    );
}

#[test]
fn merge_into_default_after_change_target_keeps_trunk_isolated() {
    let repo = TestRepo::new();

    repo.seed_files(&[
        ("README.md", "# App\n"),
        ("src/lib.rs", "pub fn hello() {}\n"),
    ]);

    repo.maw_ok(&[
        "changes",
        "create",
        "Flow",
        "--from",
        "main",
        "--id",
        "ch-clean",
        "--workspace",
        "ch-clean",
    ]);

    repo.maw_ok(&["ws", "create", "--change", "ch-clean", "worker"]);
    repo.add_file(
        "worker",
        "src/feature_alpha.rs",
        "pub fn alpha() -> i32 { 1 }\n",
    );
    repo.git_in_workspace("worker", &["add", "-A"]);
    repo.git_in_workspace("worker", &["commit", "-m", "feat: add alpha module"]);

    let epoch_before = repo.current_epoch();
    let main_before = repo
        .git(&["rev-parse", "refs/heads/main"])
        .trim()
        .to_owned();

    // Merge into change target. This should advance the change branch only;
    // trunk refs stay put.
    repo.maw_ok(&[
        "ws",
        "merge",
        "worker",
        "--into",
        "ch-clean",
        "--destroy",
        "--message",
        "merge worker into change",
    ]);

    assert_eq!(
        repo.current_epoch(),
        epoch_before,
        "global epoch should remain trunk-oriented after merge --into change"
    );
    assert_eq!(
        repo.git(&["rev-parse", "refs/heads/main"]).trim(),
        main_before,
        "main should remain unchanged after merge --into change"
    );

    // Merge a separate workspace into default/main.
    repo.maw_ok(&["ws", "create", "--from", "main", "hotfix"]);
    repo.add_file("hotfix", "HOTFIX.txt", "hotfix\n");
    repo.git_in_workspace("hotfix", &["add", "HOTFIX.txt"]);
    repo.git_in_workspace("hotfix", &["commit", "-m", "fix: add hotfix"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "hotfix",
        "--into",
        "default",
        "--destroy",
        "--message",
        "merge hotfix",
    ]);

    assert!(
        repo.read_file("default", "src/feature_alpha.rs").is_none(),
        "default workspace should not pull change-branch-only files into trunk merges"
    );
    assert_eq!(
        repo.read_file("default", "HOTFIX.txt").as_deref(),
        Some("hotfix\n"),
        "default workspace should include trunk-targeted hotfix"
    );

    let default_status = repo.git_in_workspace("default", &["status", "--porcelain"]);
    assert!(
        default_status.trim().is_empty(),
        "default workspace should be clean after merge cleanup, got: {default_status:?}"
    );
}

#[test]
fn merge_into_change_accumulates_on_change_branch_with_trunk_epoch_static() {
    let repo = TestRepo::new();

    repo.seed_files(&[
        ("README.md", "# App\n"),
        ("src/lib.rs", "pub fn hello() {}\n"),
    ]);

    repo.maw_ok(&[
        "changes",
        "create",
        "Flow",
        "--from",
        "main",
        "--id",
        "ch-acc",
        "--workspace",
        "ch-acc",
    ]);

    let epoch_before = repo.current_epoch();
    let main_before = repo
        .git(&["rev-parse", "refs/heads/main"])
        .trim()
        .to_owned();

    repo.maw_ok(&["ws", "create", "--change", "ch-acc", "w1"]);
    repo.add_file("w1", "src/a.rs", "pub fn a() {}\n");
    repo.git_in_workspace("w1", &["add", "-A"]);
    repo.git_in_workspace("w1", &["commit", "-m", "feat: a"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "w1",
        "--into",
        "ch-acc",
        "--destroy",
        "--message",
        "merge a",
    ]);

    repo.maw_ok(&["ws", "create", "--change", "ch-acc", "w2"]);
    repo.add_file("w2", "src/b.rs", "pub fn b() {}\n");
    repo.git_in_workspace("w2", &["add", "-A"]);
    repo.git_in_workspace("w2", &["commit", "-m", "feat: b"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "w2",
        "--into",
        "ch-acc",
        "--destroy",
        "--message",
        "merge b",
    ]);

    assert_eq!(
        repo.current_epoch(),
        epoch_before,
        "global epoch must remain unchanged for change-target merges"
    );
    assert_eq!(
        repo.git(&["rev-parse", "refs/heads/main"]).trim(),
        main_before,
        "main must remain unchanged for change-target merges"
    );

    let change_head = repo.git(&["rev-parse", "refs/heads/feat/ch-acc-flow"]);
    let change_tree = repo.git(&["ls-tree", "-r", "--name-only", change_head.trim()]);
    assert!(
        change_tree.lines().any(|line| line == "src/a.rs"),
        "change branch should retain first merged file"
    );
    assert!(
        change_tree.lines().any(|line| line == "src/b.rs"),
        "change branch should include second merged file"
    );
}

#[test]
fn merge_into_default_blocks_unbound_workspace_with_active_change_ancestry() {
    let repo = TestRepo::new();

    repo.seed_files(&[
        ("README.md", "# App\n"),
        ("src/lib.rs", "pub fn hello() {}\n"),
    ]);

    repo.maw_ok(&[
        "changes",
        "create",
        "Flow",
        "--from",
        "main",
        "--id",
        "ch-guard",
        "--workspace",
        "ch-guard",
    ]);

    repo.maw_ok(&["ws", "create", "--change", "ch-guard", "worker"]);
    repo.add_file("worker", "src/change_only.rs", "pub fn from_change() {}\n");
    repo.git_in_workspace("worker", &["add", "-A"]);
    repo.git_in_workspace("worker", &["commit", "-m", "feat: change work"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "worker",
        "--into",
        "ch-guard",
        "--destroy",
        "--message",
        "merge worker into change",
    ]);

    // Create an unbound main workspace, then intentionally contaminate it by
    // moving it to the active change epoch.
    repo.maw_ok(&["ws", "create", "--from", "main", "hotfix"]);
    let change_epoch = repo
        .git(&["rev-parse", "refs/heads/feat/ch-guard-flow"])
        .trim()
        .to_owned();
    repo.git_in_workspace("hotfix", &["checkout", "--detach", &change_epoch]);

    repo.add_file("hotfix", "HOTFIX.txt", "hotfix\n");
    repo.git_in_workspace("hotfix", &["add", "HOTFIX.txt"]);
    repo.git_in_workspace("hotfix", &["commit", "-m", "fix: add hotfix"]);

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "hotfix",
        "--into",
        "default",
        "--destroy",
        "--message",
        "merge hotfix",
    ]);

    assert!(
        !out.status.success(),
        "merge should be blocked\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not bound to a change") && stderr.contains("Refusing merge into 'main'"),
        "expected active-change ancestry guard message, got stderr: {stderr}"
    );
}

#[test]
fn merge_check_is_target_aware_for_change_target_conflicts() {
    let repo = TestRepo::new();

    repo.seed_files(&[
        ("README.md", "# App\n"),
        ("src/lib.rs", "pub fn hello() {}\n"),
    ]);

    repo.maw_ok(&[
        "changes",
        "create",
        "Flow",
        "--from",
        "main",
        "--id",
        "ch-check",
        "--workspace",
        "ch-check",
    ]);

    repo.maw_ok(&["ws", "create", "--change", "ch-check", "change-worker"]);
    repo.modify_file(
        "change-worker",
        "src/lib.rs",
        "pub fn hello() { println!(\"from-change\"); }\n",
    );
    repo.git_in_workspace("change-worker", &["add", "src/lib.rs"]);
    repo.git_in_workspace("change-worker", &["commit", "-m", "feat: change-side edit"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "change-worker",
        "--into",
        "ch-check",
        "--destroy",
        "--message",
        "merge change-side edit",
    ]);

    repo.maw_ok(&["ws", "create", "--from", "main", "main-worker"]);
    repo.modify_file(
        "main-worker",
        "src/lib.rs",
        "pub fn hello() { println!(\"from-main\"); }\n",
    );
    repo.git_in_workspace("main-worker", &["add", "src/lib.rs"]);
    repo.git_in_workspace("main-worker", &["commit", "-m", "feat: main-side edit"]);

    let check_out = repo.maw_raw(&[
        "ws",
        "merge",
        "main-worker",
        "--into",
        "ch-check",
        "--check",
        "--format",
        "json",
    ]);
    assert!(
        !check_out.status.success(),
        "target-aware check should block conflict\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&check_out.stdout),
        String::from_utf8_lossy(&check_out.stderr)
    );
    let check_stdout = String::from_utf8_lossy(&check_out.stdout);
    assert!(
        check_stdout.contains("\"ready\": false") && check_stdout.contains("src/lib.rs"),
        "check output should report conflict against target branch base, got: {check_stdout}"
    );
}

#[test]
fn merge_check_blocks_contaminated_unbound_workspace_for_default_target() {
    let repo = TestRepo::new();

    repo.seed_files(&[
        ("README.md", "# App\n"),
        ("src/lib.rs", "pub fn hello() {}\n"),
    ]);

    repo.maw_ok(&[
        "changes",
        "create",
        "Flow",
        "--from",
        "main",
        "--id",
        "ch-check-guard",
        "--workspace",
        "ch-check-guard",
    ]);
    repo.maw_ok(&["ws", "create", "--change", "ch-check-guard", "worker"]);
    repo.add_file("worker", "src/change_only.rs", "pub fn from_change() {}\n");
    repo.git_in_workspace("worker", &["add", "-A"]);
    repo.git_in_workspace("worker", &["commit", "-m", "feat: change work"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "worker",
        "--into",
        "ch-check-guard",
        "--destroy",
        "--message",
        "merge worker into change",
    ]);

    repo.maw_ok(&["ws", "create", "--from", "main", "hotfix"]);
    let change_head = repo
        .git(&["rev-parse", "refs/heads/feat/ch-check-guard-flow"])
        .trim()
        .to_owned();
    repo.git_in_workspace("hotfix", &["checkout", "--detach", &change_head]);
    repo.add_file("hotfix", "HOTFIX.txt", "hotfix\n");
    repo.git_in_workspace("hotfix", &["add", "HOTFIX.txt"]);
    repo.git_in_workspace("hotfix", &["commit", "-m", "fix: add hotfix"]);

    let check_out = repo.maw_raw(&["ws", "merge", "hotfix", "--into", "default", "--check"]);
    assert!(
        !check_out.status.success(),
        "check should fail for contaminated unbound workspace\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&check_out.stdout),
        String::from_utf8_lossy(&check_out.stderr)
    );
    let stderr = String::from_utf8_lossy(&check_out.stderr);
    assert!(
        stderr.contains("not bound to a change") && stderr.contains("Refusing merge into 'main'"),
        "expected guardrail message in check failure, got: {stderr}"
    );
}

#[test]
fn merge_check_guardrail_failures_emit_json_payload_when_requested() {
    let repo = TestRepo::new();

    repo.seed_files(&[
        ("README.md", "# App\n"),
        ("src/lib.rs", "pub fn hello() {}\n"),
    ]);

    repo.maw_ok(&[
        "changes",
        "create",
        "Flow",
        "--from",
        "main",
        "--id",
        "ch-json-guard",
        "--workspace",
        "ch-json-guard",
    ]);
    repo.maw_ok(&["ws", "create", "--change", "ch-json-guard", "worker"]);
    repo.add_file("worker", "src/change_only.rs", "pub fn from_change() {}\n");
    repo.git_in_workspace("worker", &["add", "-A"]);
    repo.git_in_workspace("worker", &["commit", "-m", "feat: change work"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "worker",
        "--into",
        "ch-json-guard",
        "--destroy",
        "--message",
        "merge worker into change",
    ]);

    repo.maw_ok(&["ws", "create", "--from", "main", "hotfix"]);
    let change_head = repo
        .git(&["rev-parse", "refs/heads/feat/ch-json-guard-flow"])
        .trim()
        .to_owned();
    repo.git_in_workspace("hotfix", &["checkout", "--detach", &change_head]);
    repo.add_file("hotfix", "HOTFIX.txt", "hotfix\n");
    repo.git_in_workspace("hotfix", &["add", "HOTFIX.txt"]);
    repo.git_in_workspace("hotfix", &["commit", "-m", "fix: add hotfix"]);

    let check_out = repo.maw_raw(&[
        "ws", "merge", "hotfix", "--into", "default", "--check", "--format", "json",
    ]);
    assert!(
        !check_out.status.success(),
        "check should fail for contaminated unbound workspace\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&check_out.stdout),
        String::from_utf8_lossy(&check_out.stderr)
    );

    let check_stdout = String::from_utf8_lossy(&check_out.stdout);
    let payload: serde_json::Value =
        serde_json::from_str(&check_stdout).expect("check guardrail output should be valid JSON");
    assert_eq!(
        payload["ready"].as_bool(),
        Some(false),
        "guardrail JSON should report not-ready: {payload}"
    );
    assert!(
        check_stdout.contains("Refusing merge into 'main'"),
        "guardrail JSON should include remediation context, got: {check_stdout}"
    );
}

#[test]
fn merge_check_missing_workspace_has_actionable_error_text() {
    let repo = TestRepo::new();

    let out = repo.maw_raw(&["ws", "merge", "missing", "--into", "default", "--check"]);
    assert!(
        !out.status.success(),
        "check should fail for missing workspace\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("does not exist at")
            && stderr.contains("Check available workspaces: maw ws list"),
        "expected actionable missing-workspace error, got: {stderr}"
    );
}

#[test]
fn merge_dry_run_missing_workspace_fails_with_actionable_error_text() {
    let repo = TestRepo::new();

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "missing",
        "--into",
        "default",
        "--dry-run",
        "--format",
        "json",
    ]);
    assert!(
        !out.status.success(),
        "dry-run should fail for missing workspace\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("does not exist at")
            && stderr.contains("Check available workspaces: maw ws list"),
        "expected actionable missing-workspace error, got: {stderr}"
    );
}

#[test]
fn merge_check_missing_workspace_with_active_change_stays_actionable() {
    let repo = TestRepo::new();

    repo.maw_ok(&[
        "changes",
        "create",
        "Flow",
        "--from",
        "main",
        "--id",
        "ch-missing-check",
        "--workspace",
        "ch-missing-check",
    ]);
    repo.maw_ok(&["ws", "create", "--change", "ch-missing-check", "worker"]);
    repo.add_file("worker", "change.txt", "change\n");
    repo.git_in_workspace("worker", &["add", "change.txt"]);
    repo.git_in_workspace("worker", &["commit", "-m", "feat: change"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "worker",
        "--into",
        "ch-missing-check",
        "--destroy",
        "--message",
        "merge worker",
    ]);

    let out = repo.maw_raw(&["ws", "merge", "missing", "--into", "default", "--check"]);
    assert!(
        !out.status.success(),
        "check should fail for missing workspace\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("does not exist at")
            && stderr.contains("Check available workspaces: maw ws list")
            && !stderr.contains("failed to run git rev-parse HEAD"),
        "expected actionable missing-workspace error even with active changes, got: {stderr}"
    );
}

#[test]
fn merge_check_invalid_target_emits_json_payload_when_requested() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "agent"]);
    repo.add_file("agent", "agent.txt", "agent\n");
    repo.git_in_workspace("agent", &["add", "agent.txt"]);
    repo.git_in_workspace("agent", &["commit", "-m", "feat: agent"]);

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "agent",
        "--into",
        "does-not-exist",
        "--check",
        "--format",
        "json",
    ]);
    assert!(
        !out.status.success(),
        "invalid target check should fail\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let payload: serde_json::Value =
        serde_json::from_str(&stdout).expect("invalid target check should emit JSON payload");
    assert_eq!(payload["ready"].as_bool(), Some(false));
    assert!(
        stdout.contains("Unknown merge target 'does-not-exist'"),
        "JSON payload should include target resolution error, got: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("merge check:") && stderr.contains("Unknown merge target 'does-not-exist'"),
        "stderr should include actionable reason, got: {stderr}"
    );
}

#[test]
fn merge_plan_is_target_aware_for_change_target_conflicts() {
    let repo = TestRepo::new();

    repo.seed_files(&[
        ("README.md", "# App\n"),
        ("src/lib.rs", "pub fn hello() {}\n"),
    ]);

    repo.maw_ok(&[
        "changes",
        "create",
        "Flow",
        "--from",
        "main",
        "--id",
        "ch-plan",
        "--workspace",
        "ch-plan",
    ]);

    repo.maw_ok(&["ws", "create", "--change", "ch-plan", "change-worker"]);
    repo.modify_file(
        "change-worker",
        "src/lib.rs",
        "pub fn hello() { println!(\"from-change\"); }\n",
    );
    repo.git_in_workspace("change-worker", &["add", "src/lib.rs"]);
    repo.git_in_workspace("change-worker", &["commit", "-m", "feat: change-side edit"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "change-worker",
        "--into",
        "ch-plan",
        "--destroy",
        "--message",
        "merge change-side edit",
    ]);

    repo.maw_ok(&["ws", "create", "--from", "main", "main-worker"]);
    repo.modify_file(
        "main-worker",
        "src/lib.rs",
        "pub fn hello() { println!(\"from-main\"); }\n",
    );
    repo.git_in_workspace("main-worker", &["add", "src/lib.rs"]);
    repo.git_in_workspace("main-worker", &["commit", "-m", "feat: main-side edit"]);

    let plan_out = repo.maw_raw(&[
        "ws",
        "merge",
        "main-worker",
        "--into",
        "ch-plan",
        "--plan",
        "--format",
        "json",
    ]);
    assert!(
        plan_out.status.success(),
        "plan should complete and report predicted conflicts\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&plan_out.stdout),
        String::from_utf8_lossy(&plan_out.stderr)
    );

    let payload: serde_json::Value =
        serde_json::from_slice(&plan_out.stdout).expect("plan output should be valid JSON");
    let predicted = payload["predicted_conflicts"]
        .as_array()
        .expect("predicted_conflicts should be an array");
    assert!(
        predicted
            .iter()
            .any(|entry| entry["path"].as_str() == Some("src/lib.rs")),
        "plan should predict change-target conflict on src/lib.rs, got: {payload}"
    );
    assert!(
        predicted
            .iter()
            .find(|entry| entry["path"].as_str() == Some("src/lib.rs"))
            .and_then(|entry| entry["sides"].as_array())
            .is_some_and(|sides| sides.iter().any(|s| s.as_str() == Some("main-worker"))),
        "plan conflict sides should include source workspace attribution, got: {payload}"
    );
}

#[test]
fn merge_plan_rejects_invalid_target_value() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "agent"]);
    repo.add_file("agent", "agent.txt", "agent\n");
    repo.git_in_workspace("agent", &["add", "agent.txt"]);
    repo.git_in_workspace("agent", &["commit", "-m", "feat: agent"]);

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "agent",
        "--into",
        "does-not-exist",
        "--plan",
        "--format",
        "json",
    ]);
    assert!(
        !out.status.success(),
        "plan should fail for invalid target\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let payload: serde_json::Value =
        serde_json::from_str(&stdout).expect("plan json mode should emit JSON error payload");
    assert_eq!(payload["ready"].as_bool(), Some(false));
    assert!(
        payload["conflicts"]
            .as_array()
            .and_then(|conflicts| conflicts.first())
            .and_then(|entry| entry["reason"].as_str())
            .is_some_and(|msg| msg.contains("Unknown merge target 'does-not-exist'")),
        "expected actionable invalid-target JSON error, got: {payload}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("merge plan:") && stderr.contains("Unknown merge target 'does-not-exist'"),
        "stderr should include actionable reason, got: {stderr}"
    );
}

#[test]
fn merge_plan_guardrail_failures_emit_json_payload_when_requested() {
    let repo = TestRepo::new();

    repo.seed_files(&[
        ("README.md", "# App\n"),
        ("src/lib.rs", "pub fn hello() {}\n"),
    ]);

    repo.maw_ok(&[
        "changes",
        "create",
        "Flow",
        "--from",
        "main",
        "--id",
        "ch-plan-guard",
        "--workspace",
        "ch-plan-guard",
    ]);
    repo.maw_ok(&["ws", "create", "--change", "ch-plan-guard", "worker"]);
    repo.add_file("worker", "src/change_only.rs", "pub fn from_change() {}\n");
    repo.git_in_workspace("worker", &["add", "-A"]);
    repo.git_in_workspace("worker", &["commit", "-m", "feat: change work"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "worker",
        "--into",
        "ch-plan-guard",
        "--destroy",
        "--message",
        "merge worker into change",
    ]);

    repo.maw_ok(&["ws", "create", "--from", "main", "hotfix"]);
    let change_head = repo
        .git(&["rev-parse", "refs/heads/feat/ch-plan-guard-flow"])
        .trim()
        .to_owned();
    repo.git_in_workspace("hotfix", &["checkout", "--detach", &change_head]);
    repo.add_file("hotfix", "HOTFIX.txt", "hotfix\n");
    repo.git_in_workspace("hotfix", &["add", "HOTFIX.txt"]);
    repo.git_in_workspace("hotfix", &["commit", "-m", "fix: add hotfix"]);

    let out = repo.maw_raw(&[
        "ws", "merge", "hotfix", "--into", "default", "--plan", "--format", "json",
    ]);
    assert!(
        !out.status.success(),
        "plan should fail for contaminated unbound workspace\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let payload: serde_json::Value =
        serde_json::from_str(&stdout).expect("plan guardrail output should be valid JSON");
    assert_eq!(payload["ready"].as_bool(), Some(false));
    assert!(
        payload["conflicts"]
            .as_array()
            .and_then(|conflicts| conflicts.first())
            .and_then(|entry| entry["reason"].as_str())
            .is_some_and(|msg| msg.contains("Refusing merge into 'main'")),
        "plan guardrail JSON should include remediation context, got: {payload}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("merge plan:") && stderr.contains("Refusing merge into 'main'"),
        "stderr should include actionable reason, got: {stderr}"
    );
}

#[test]
fn merge_plan_missing_workspace_with_active_change_stays_actionable() {
    let repo = TestRepo::new();

    repo.maw_ok(&[
        "changes",
        "create",
        "Flow",
        "--from",
        "main",
        "--id",
        "ch-plan-missing",
        "--workspace",
        "ch-plan-missing",
    ]);
    repo.maw_ok(&["ws", "create", "--change", "ch-plan-missing", "worker"]);
    repo.add_file("worker", "change.txt", "change\n");
    repo.git_in_workspace("worker", &["add", "change.txt"]);
    repo.git_in_workspace("worker", &["commit", "-m", "feat: change"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "worker",
        "--into",
        "ch-plan-missing",
        "--destroy",
        "--message",
        "merge worker",
    ]);

    let out = repo.maw_raw(&[
        "ws", "merge", "missing", "--into", "default", "--plan", "--format", "json",
    ]);
    assert!(
        !out.status.success(),
        "plan should fail for missing workspace\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let payload: serde_json::Value =
        serde_json::from_str(&stdout).expect("plan json mode should emit JSON error payload");
    assert_eq!(payload["ready"].as_bool(), Some(false));
    assert!(
        payload["conflicts"]
            .as_array()
            .and_then(|conflicts| conflicts.first())
            .and_then(|entry| entry["reason"].as_str())
            .is_some_and(|msg| {
                msg.contains("Workspace 'missing' does not exist at")
                    && msg.contains("Check available workspaces: maw ws list")
                    && !msg.contains("failed to run git rev-parse HEAD")
            }),
        "expected actionable missing-workspace JSON error even with active changes, got: {payload}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("merge plan:") && stderr.contains("Workspace 'missing' does not exist at"),
        "stderr should include actionable reason, got: {stderr}"
    );
}

#[test]
fn merge_into_default_blocks_unbound_workspace_with_stale_change_tip_ancestry() {
    let repo = TestRepo::new();

    repo.seed_files(&[("README.md", "# App\n")]);

    repo.maw_ok(&[
        "changes",
        "create",
        "Flow",
        "--from",
        "main",
        "--id",
        "ch-stale",
        "--workspace",
        "ch-stale",
    ]);

    repo.maw_ok(&["ws", "create", "--change", "ch-stale", "w1"]);
    repo.modify_file("w1", "README.md", "# App\nfrom-change\n");
    repo.git_in_workspace("w1", &["add", "README.md"]);
    repo.git_in_workspace("w1", &["commit", "-m", "feat: first change tip"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "w1",
        "--into",
        "ch-stale",
        "--destroy",
        "--message",
        "merge w1",
    ]);

    let stale_change_tip = repo
        .git(&["rev-parse", "refs/heads/feat/ch-stale-flow"])
        .trim()
        .to_owned();

    repo.maw_ok(&["ws", "create", "--change", "ch-stale", "w2"]);
    repo.add_file("w2", "later.txt", "later change\n");
    repo.git_in_workspace("w2", &["add", "later.txt"]);
    repo.git_in_workspace("w2", &["commit", "-m", "feat: advance change tip"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "w2",
        "--into",
        "ch-stale",
        "--destroy",
        "--message",
        "merge w2",
    ]);

    repo.maw_ok(&["ws", "create", "--from", "main", "hotfix"]);
    repo.git_in_workspace("hotfix", &["checkout", "--detach", &stale_change_tip]);
    repo.add_file("hotfix", "HOTFIX.txt", "hotfix\n");
    repo.git_in_workspace("hotfix", &["add", "HOTFIX.txt"]);
    repo.git_in_workspace("hotfix", &["commit", "-m", "fix: add hotfix"]);

    let check_out = repo.maw_raw(&[
        "ws", "merge", "hotfix", "--into", "default", "--check", "--format", "json",
    ]);
    assert!(
        !check_out.status.success(),
        "stale-tip contamination should fail check\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&check_out.stdout),
        String::from_utf8_lossy(&check_out.stderr)
    );
    let check_stdout = String::from_utf8_lossy(&check_out.stdout);
    assert!(
        check_stdout.contains("\"ready\": false")
            && check_stdout.contains("shares unmerged ancestry"),
        "check should report stale-tip lineage guardrail, got: {check_stdout}"
    );

    let merge_out = repo.maw_raw(&[
        "ws",
        "merge",
        "hotfix",
        "--into",
        "default",
        "--destroy",
        "--message",
        "merge hotfix",
    ]);
    assert!(
        !merge_out.status.success(),
        "merge should also be blocked\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&merge_out.stdout),
        String::from_utf8_lossy(&merge_out.stderr)
    );
    let merge_stderr = String::from_utf8_lossy(&merge_out.stderr);
    assert!(
        merge_stderr.contains("shares unmerged ancestry")
            && merge_stderr.contains("Refusing merge into 'main'"),
        "expected stale-tip guard message, got: {merge_stderr}"
    );
}

#[test]
fn merge_into_change_outputs_change_specific_next_steps() {
    let repo = TestRepo::new();

    repo.seed_files(&[("README.md", "# App\n")]);
    repo.maw_ok(&[
        "changes",
        "create",
        "Flow",
        "--from",
        "main",
        "--id",
        "ch-msg",
        "--workspace",
        "ch-msg",
    ]);
    repo.maw_ok(&["ws", "create", "--change", "ch-msg", "worker"]);
    repo.add_file("worker", "src/msg.rs", "pub fn msg() {}\n");
    repo.git_in_workspace("worker", &["add", "-A"]);
    repo.git_in_workspace("worker", &["commit", "-m", "feat: msg"]);

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "worker",
        "--into",
        "ch-msg",
        "--destroy",
        "--message",
        "merge msg",
    ]);
    assert!(
        out.status.success(),
        "merge into change should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("COMMIT: Updating target branch...")
            && stdout.contains("maw changes pr ch-msg --draft"),
        "expected change-specific commit + next-step messaging, got: {stdout}"
    );
    assert!(
        !stdout.contains("Next: push to remote:")
            && !stdout.contains("Default workspace updated to new epoch."),
        "should not print trunk/default-epoch guidance for change-target merge, got: {stdout}"
    );
}

#[test]
fn changes_create_guidance_avoids_invalid_self_merge_instructions() {
    let repo = TestRepo::new();

    let out = repo.maw_raw(&[
        "changes",
        "create",
        "Flow",
        "--from",
        "main",
        "--id",
        "ch-guide",
        "--workspace",
        "ch-guide",
    ]);
    assert!(
        out.status.success(),
        "changes create should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("maw ws merge ch-guide --into ch-guide --destroy"),
        "output should not suggest invalid self-merge command, got: {stdout}"
    );
    assert!(
        stdout.contains("maw ws create --change ch-guide <agent-workspace>")
            && stdout.contains("maw ws merge <agent-workspace> --into ch-guide --destroy"),
        "output should suggest worker-workspace merge flow, got: {stdout}"
    );
}

#[test]
fn changes_create_json_advice_avoids_invalid_self_merge_instructions() {
    let repo = TestRepo::new();

    let out = repo.maw_raw(&[
        "changes",
        "create",
        "Flow",
        "--from",
        "main",
        "--id",
        "ch-guide-json",
        "--workspace",
        "ch-guide-json",
        "--format",
        "json",
    ]);
    assert!(
        out.status.success(),
        "changes create json should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let payload: serde_json::Value =
        serde_json::from_str(&stdout).expect("changes create JSON output should be pure JSON");
    assert!(
        !stdout.contains("Creating workspace"),
        "JSON mode must not include human prose before payload, got: {stdout}"
    );
    let advice = payload["advice"]
        .as_array()
        .expect("advice should be an array")
        .iter()
        .filter_map(|entry| entry.as_str())
        .collect::<Vec<_>>();

    assert!(
        !advice
            .iter()
            .any(|line| *line == "maw ws merge ch-guide-json --into ch-guide-json --destroy"),
        "JSON advice should not include invalid self-merge command: {payload}"
    );
    assert!(
        advice
            .iter()
            .any(|line| *line == "maw ws create --change ch-guide-json <agent-workspace>")
            && advice.iter().any(
                |line| *line == "maw ws merge <agent-workspace> --into ch-guide-json --destroy"
            ),
        "JSON advice should include worker workspace merge guidance: {payload}"
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
    repo.maw_ok(&[
        "ws",
        "merge",
        "agent",
        "--destroy",
        "--message",
        "test merge",
    ]);

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
    repo.maw_ok(&[
        "ws",
        "merge",
        "agent",
        "--destroy",
        "--message",
        "test merge",
    ]);

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

/// Merging a workspace with zero changes should NOT create an epoch commit.
/// This prevents agents from accidentally advancing the epoch with empty merges.
#[test]
fn merge_empty_workspace_rejects_without_epoch_advance() {
    let repo = TestRepo::new();

    // Capture HEAD before merge attempt.
    let head_before = repo.git(&["rev-parse", "HEAD"]);

    repo.maw_ok(&["ws", "create", "empty-agent"]);
    // Don't add any files — the workspace has zero changes.

    // Without --destroy: merge should fail (non-zero exit).
    let out = repo.maw_raw(&["ws", "merge", "empty-agent", "--message", "test merge"]);
    assert!(
        !out.status.success(),
        "merging an empty workspace without --destroy should fail"
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}\n{stderr}").to_lowercase();
    assert!(
        combined.contains("no changes"),
        "expected 'no changes' message, got:\n{combined}"
    );

    // HEAD should NOT have advanced.
    let head_after = repo.git(&["rev-parse", "HEAD"]);
    assert_eq!(
        head_before, head_after,
        "epoch should not advance for an empty merge"
    );

    // The workspace should still exist (not destroyed).
    let names = workspace_names(&repo);
    assert!(
        names.contains(&"empty-agent".to_owned()),
        "empty workspace should be preserved (not destroyed) after empty merge"
    );
}

/// Same as above, but with --format json: output should be valid JSON with
/// status "empty".
#[test]
fn merge_empty_workspace_json_output() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "empty-json"]);

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "empty-json",
        "--format",
        "json",
        "--message",
        "test merge",
    ]);
    assert!(
        !out.status.success(),
        "merging an empty workspace should fail even with --format json"
    );

    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        stdout.starts_with('{'),
        "stdout should be pure JSON, got: {stdout}"
    );

    let payload: serde_json::Value =
        serde_json::from_str(&stdout).expect("empty merge JSON output should be valid JSON");
    assert_eq!(
        payload["status"].as_str(),
        Some("empty"),
        "JSON status should be 'empty', got: {payload}"
    );
}
