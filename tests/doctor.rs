mod manifold_common;

use manifold_common::TestRepo;

fn default_workspace_check(parsed: &serde_json::Value) -> &serde_json::Value {
    parsed["checks"]
        .as_array()
        .expect("doctor json should contain checks array")
        .iter()
        .find(|check| check["name"].as_str() == Some("default workspace"))
        .expect("doctor output should include default workspace check")
}

#[test]
fn doctor_reports_default_worktree_ok_when_registered() {
    let repo = TestRepo::new();
    repo.add_file("default", "README.md", "hello\n");

    let out = repo.maw_ok(&["doctor", "--format", "json"]);
    let parsed: serde_json::Value =
        serde_json::from_str(&out).expect("doctor --format json should be valid JSON");

    let check = default_workspace_check(&parsed);
    assert_eq!(check["status"].as_str(), Some("ok"));
}

#[test]
fn doctor_fails_when_default_exists_but_not_a_worktree() {
    let repo = TestRepo::new();
    repo.add_file("default", "README.md", "hello\n");

    let git_link = repo.workspace_path("default").join(".git");
    std::fs::remove_file(&git_link).expect("should remove default worktree .git link");

    let out = repo.maw_ok(&["doctor", "--format", "json"]);
    let parsed: serde_json::Value =
        serde_json::from_str(&out).expect("doctor --format json should be valid JSON");

    let check = default_workspace_check(&parsed);
    assert_eq!(check["status"].as_str(), Some("fail"));
    assert!(
        check["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not a registered git worktree"),
        "expected explicit worktree registration failure message: {check}"
    );
}
