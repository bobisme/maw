mod manifold_common;

use std::process::Command;

use manifold_common::TestRepo;

#[test]
fn ws_diff_summary_against_default_reports_modified_paths() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "pub fn answer() -> i32 { 1 }\n")]);
    repo.create_workspace("alice");
    repo.modify_file("alice", "src/lib.rs", "pub fn answer() -> i32 { 2 }\n");

    let out = repo.maw_ok(&["ws", "diff", "alice"]);
    assert!(out.contains("Diff: default -> alice"), "output: {out}");
    assert!(out.contains("M src/lib.rs"), "output: {out}");
}

#[test]
fn ws_diff_json_contract_includes_metadata_and_files() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "pub fn answer() -> i32 { 1 }\n")]);
    repo.create_workspace("alice");
    repo.modify_file("alice", "src/lib.rs", "pub fn answer() -> i32 { 2 }\n");

    let out = repo.maw_ok(&["ws", "diff", "alice", "--format", "json"]);
    let json: serde_json::Value = serde_json::from_str(&out).expect("valid JSON output");

    assert_eq!(json["workspace"].as_str(), Some("alice"));
    assert_eq!(json["against"]["label"].as_str(), Some("default"));
    assert_eq!(json["head"]["label"].as_str(), Some("alice"));
    assert_eq!(json["stats"]["files_changed"].as_u64(), Some(1));
    assert_eq!(json["files"][0]["path"].as_str(), Some("src/lib.rs"));
    assert_eq!(json["files"][0]["status"].as_str(), Some("M"));
}

#[test]
fn ws_diff_patch_output_is_unified_diff() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "pub fn answer() -> i32 { 1 }\n")]);
    repo.create_workspace("alice");
    repo.modify_file("alice", "src/lib.rs", "pub fn answer() -> i32 { 2 }\n");

    let out = repo.maw_ok(&["ws", "diff", "alice", "--format", "patch"]);
    assert!(out.contains("diff --git a/src/lib.rs b/src/lib.rs"));
    assert!(out.contains("-pub fn answer() -> i32 { 1 }"));
    assert!(out.contains("+pub fn answer() -> i32 { 2 }"));
}

#[test]
fn ws_diff_name_only_outputs_one_path_per_line() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "pub fn answer() -> i32 { 1 }\n")]);
    repo.create_workspace("alice");
    repo.modify_file("alice", "src/lib.rs", "pub fn answer() -> i32 { 2 }\n");

    let out = repo.maw_ok(&["ws", "diff", "alice", "--name-only"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines, vec!["src/lib.rs"]);
}

#[test]
fn ws_diff_supports_epoch_target() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "pub fn answer() -> i32 { 1 }\n")]);
    repo.create_workspace("alice");
    repo.modify_file("alice", "src/lib.rs", "pub fn answer() -> i32 { 2 }\n");

    let out = repo.maw_ok(&[
        "ws",
        "diff",
        "alice",
        "--against",
        "epoch",
        "--format",
        "json",
    ]);
    let json: serde_json::Value = serde_json::from_str(&out).expect("valid JSON output");
    assert_eq!(json["against"]["label"].as_str(), Some("epoch"));
    assert_eq!(json["stats"]["files_changed"].as_u64(), Some(1));
}

#[test]
fn ws_diff_runs_from_default_workspace_cwd() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "pub fn answer() -> i32 { 1 }\n")]);
    repo.create_workspace("alice");
    repo.modify_file("alice", "src/lib.rs", "pub fn answer() -> i32 { 2 }\n");

    let output = Command::new(env!("CARGO_BIN_EXE_maw"))
        .args(["ws", "diff", "alice", "--format", "json"])
        .current_dir(repo.default_workspace())
        .output()
        .expect("failed to execute maw");

    assert!(
        output.status.success(),
        "command failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn ws_diff_missing_workspace_includes_recovery_guidance() {
    let repo = TestRepo::new();
    let err = repo.maw_fails(&["ws", "diff", "does-not-exist"]);
    assert!(err.contains("Workspace 'does-not-exist' does not exist"));
    assert!(err.contains("maw ws list"));
}
