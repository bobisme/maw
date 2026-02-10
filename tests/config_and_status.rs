mod common;
use common::*;

/// Test that config loading works with custom branch name
#[test]
fn config_fallback_to_default_workspace() {
    let repo = setup_test_repo();

    // Create a custom config in the repo root with a non-default branch name
    let config_content = r#"
[repo]
branch = "develop"
"#;
    std::fs::write(repo.path().join(".maw.toml"), config_content)
        .expect("failed to write .maw.toml");

    // Create a develop bookmark so the status check has something to compare
    std::process::Command::new("jj")
        .args(["bookmark", "create", "develop", "-r", "@"])
        .current_dir(repo.path())
        .output()
        .expect("failed to create develop bookmark");

    // Run maw status with JSON format and verify it uses the configured branch
    let stdout = maw_ok(repo.path(), &["status", "--format=json"]);

    // The JSON output should reference "develop" as the branch name
    // It might be in main_sync or mentioned in the JSON structure
    // Since we configured branch=develop, the status should check develop (not main)
    // With no remote, main_sync should be "no-remote" rather than "no-main"
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .expect("status --format=json should produce valid JSON");

    let main_sync = parsed.get("main_sync")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // If the config is loaded correctly, status checks "develop" not "main"
    // So we shouldn't see "no-main" - we should see "no-remote" since develop exists but develop@origin doesn't
    assert_eq!(
        main_sync, "no-remote",
        "Expected main_sync to be 'no-remote' when develop bookmark exists but has no remote, got: {main_sync}"
    );
}

/// Test that status JSON format produces valid JSON
#[test]
fn status_json_format() {
    let repo = setup_test_repo();

    let stdout = maw_ok(repo.path(), &["status", "--format=json"]);

    // Parse the JSON to verify it's valid
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .expect("status --format=json should produce valid JSON");

    // Verify expected top-level keys exist
    assert!(parsed.get("workspaces").is_some(), "JSON should have 'workspaces' field");
    assert!(parsed.get("changed_files").is_some(), "JSON should have 'changed_files' field");
    assert!(parsed.get("untracked_files").is_some(), "JSON should have 'untracked_files' field");
    assert!(parsed.get("is_stale").is_some(), "JSON should have 'is_stale' field");
    assert!(parsed.get("main_sync").is_some(), "JSON should have 'main_sync' field");
}

/// Test that status text format uses [OK]/[WARN] markers
#[test]
fn status_text_format() {
    let repo = setup_test_repo();

    let stdout = maw_ok(repo.path(), &["status", "--format=text"]);

    // Text format should contain structured [OK] or [WARN] markers
    assert!(
        stdout.contains("[OK]") || stdout.contains("[WARN]"),
        "Text format should contain [OK] or [WARN] markers, got: {stdout}"
    );

    // Text format should NOT be JSON (shouldn't start with '{')
    assert!(
        !stdout.trim_start().starts_with('{'),
        "Text format should not be JSON, got: {stdout}"
    );
}
