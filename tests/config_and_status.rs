mod manifold_common;

use manifold_common::TestRepo;

/// Test that status respects a custom branch from `.maw.toml`.
#[test]
fn config_fallback_to_default_workspace_git_native() {
    let repo = TestRepo::new();

    std::fs::write(
        repo.root().join(".maw.toml"),
        "[repo]\nbranch = \"develop\"\n",
    )
    .expect("failed to write .maw.toml");

    let epoch = repo.current_epoch();
    repo.git(&["update-ref", "refs/heads/develop", &epoch]);

    let stdout = repo.maw_ok(&["status", "--format=json"]);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("status --format=json should produce valid JSON");

    let main_sync = parsed
        .get("main_sync")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    assert_eq!(
        main_sync, "no-remote",
        "Expected main_sync to be 'no-remote' for develop without origin/develop, got: {main_sync}"
    );
}

#[test]
fn status_json_format() {
    let repo = TestRepo::new();

    let stdout = repo.maw_ok(&["status", "--format=json"]);

    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("status --format=json should produce valid JSON");

    assert!(parsed.get("workspaces").is_some());
    assert!(parsed.get("changed_files").is_some());
    assert!(parsed.get("untracked_files").is_some());
    assert!(parsed.get("is_stale").is_some());
    assert!(parsed.get("main_sync").is_some());
}

#[test]
fn status_text_format() {
    let repo = TestRepo::new();

    let stdout = repo.maw_ok(&["status", "--format=text"]);

    assert!(
        stdout.contains("[OK]") || stdout.contains("[WARN]"),
        "Text format should contain [OK] or [WARN] markers, got: {stdout}"
    );
    assert!(!stdout.trim_start().starts_with('{'));
}

#[test]
fn status_does_not_flag_default_stale_when_branch_ahead_of_epoch() {
    let repo = TestRepo::new();

    repo.add_file("default", "hotfix.txt", "urgent fix\n");
    repo.git_in_workspace("default", &["add", "hotfix.txt"]);
    repo.git_in_workspace("default", &["commit", "-m", "fix: hotfix"]);

    let branch_tip = repo.workspace_head("default");
    repo.git(&["update-ref", "refs/heads/main", branch_tip.as_str()]);
    // Leave refs/manifold/epoch/current at epoch0 to simulate stale epoch.

    let stdout = repo.maw_ok(&["status", "--format=json"]);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("status --format=json should produce valid JSON");

    assert_eq!(
        parsed.get("is_stale").and_then(|v| v.as_bool()),
        Some(false),
        "default branch workspace should not be reported stale when epoch lags branch"
    );
}
