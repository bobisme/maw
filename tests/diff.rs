mod manifold_common;

use std::process::Command;

use manifold_common::TestRepo;

#[test]
fn ws_diff_defaults_to_patch_output() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "pub fn answer() -> i32 { 1 }\n")]);
    repo.create_workspace("alice");
    repo.modify_file("alice", "src/lib.rs", "pub fn answer() -> i32 { 2 }\n");

    let out = repo.maw_ok(&["ws", "diff", "alice"]);
    assert!(
        out.contains("diff --git a/src/lib.rs b/src/lib.rs"),
        "expected patch output, got: {out}"
    );
    assert!(
        out.contains("-pub fn answer() -> i32 { 1 }"),
        "output: {out}"
    );
    assert!(
        out.contains("+pub fn answer() -> i32 { 2 }"),
        "output: {out}"
    );
}

#[test]
fn ws_diff_json_contract_includes_metadata_and_files() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "pub fn answer() -> i32 { 1 }\n")]);
    repo.create_workspace("alice");
    repo.modify_file("alice", "src/lib.rs", "pub fn answer() -> i32 { 2 }\n");

    let out = repo.maw_ok(&["ws", "diff", "alice", "--json"]);
    let json: serde_json::Value = serde_json::from_str(&out).expect("valid JSON output");

    assert_eq!(json["workspace"].as_str(), Some("alice"));
    // bn-1abp: no --against means epoch (the documented semantics).
    assert_eq!(json["against"]["label"].as_str(), Some("epoch"));
    assert_eq!(json["head"]["label"].as_str(), Some("alice"));
    assert_eq!(json["stats"]["files_changed"].as_u64(), Some(1));
    assert_eq!(json["files"][0]["path"].as_str(), Some("src/lib.rs"));
    assert_eq!(json["files"][0]["status"].as_str(), Some("M"));
}

#[test]
fn ws_diff_stat_shows_diffstat() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "pub fn answer() -> i32 { 1 }\n")]);
    repo.create_workspace("alice");
    repo.modify_file("alice", "src/lib.rs", "pub fn answer() -> i32 { 2 }\n");

    let out = repo.maw_ok(&["ws", "diff", "alice", "--stat"]);
    assert!(out.contains("src/lib.rs"), "output: {out}");
    assert!(out.contains("1 file changed"), "output: {out}");
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
fn ws_diff_name_status_shows_status_and_path() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "pub fn answer() -> i32 { 1 }\n")]);
    repo.create_workspace("alice");
    repo.modify_file("alice", "src/lib.rs", "pub fn answer() -> i32 { 2 }\n");

    let out = repo.maw_ok(&["ws", "diff", "alice", "--name-status"]);
    assert!(out.contains("M\tsrc/lib.rs"), "output: {out}");
}

#[test]
fn ws_diff_supports_epoch_target() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "pub fn answer() -> i32 { 1 }\n")]);
    repo.create_workspace("alice");
    repo.modify_file("alice", "src/lib.rs", "pub fn answer() -> i32 { 2 }\n");

    let out = repo.maw_ok(&["ws", "diff", "alice", "--against", "epoch", "--json"]);
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

    let output = Command::new(manifold_common::maw_bin())
        .args(["ws", "diff", "alice", "--json"])
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
fn ws_diff_positional_path_filters() {
    let repo = TestRepo::new();
    repo.seed_files(&[
        ("src/lib.rs", "pub fn answer() -> i32 { 1 }\n"),
        ("README.md", "# hello\n"),
    ]);
    repo.create_workspace("alice");
    repo.modify_file("alice", "src/lib.rs", "pub fn answer() -> i32 { 2 }\n");
    repo.modify_file("alice", "README.md", "# goodbye\n");

    let out = repo.maw_ok(&["ws", "diff", "alice", "--name-only", "src/*"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines, vec!["src/lib.rs"]);
}

#[test]
fn ws_diff_missing_workspace_includes_recovery_guidance() {
    let repo = TestRepo::new();
    let err = repo.maw_fails(&["ws", "diff", "does-not-exist"]);
    assert!(err.contains("Workspace 'does-not-exist' does not exist"));
    assert!(err.contains("maw ws list"));
}

// ---------------------------------------------------------------------------
// bn-1abp: default baseline is the epoch
// ---------------------------------------------------------------------------

/// Committed-but-unmerged work must show in `maw ws diff <ws>` with no
/// --against flag — the documented semantics is "changes vs the epoch".
#[test]
fn ws_diff_default_shows_committed_unmerged_work_vs_epoch() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "pub fn answer() -> i32 { 1 }\n")]);
    repo.create_workspace("alice");
    repo.modify_file("alice", "src/lib.rs", "pub fn answer() -> i32 { 2 }\n");
    repo.git_in_workspace("alice", &["add", "-A"]);
    repo.git_in_workspace("alice", &["commit", "-m", "alice: change answer"]);

    let out = repo.maw_ok(&["ws", "diff", "alice"]);
    assert!(
        out.contains("+pub fn answer() -> i32 { 2 }"),
        "committed-but-unmerged work must appear in the default diff, got: {out}"
    );
}

/// Another workspace's MERGED work must not be attributed to the diff
/// target. (The old `--against default` baseline used the default
/// workspace's recorded state ref, which lags the epoch and showed sibling
/// merges as if the diffed workspace had authored them.)
#[test]
fn ws_diff_default_does_not_attribute_sibling_merged_work() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "pub fn answer() -> i32 { 1 }\n")]);

    repo.create_workspace("alice");
    repo.modify_file("alice", "src/lib.rs", "pub fn answer() -> i32 { 2 }\n");
    repo.git_in_workspace("alice", &["add", "-A"]);
    repo.git_in_workspace("alice", &["commit", "-m", "alice: change answer"]);

    repo.create_workspace("bob");
    repo.add_file("bob", "bob_file.txt", "bob was here\n");
    repo.git_in_workspace("bob", &["add", "-A"]);
    repo.git_in_workspace("bob", &["commit", "-m", "bob: add file"]);
    repo.maw_ok(&["ws", "merge", "bob", "--message", "merge bob"]);

    // alice was auto-rebased onto the new epoch; her diff must contain only
    // her own change, not bob's merged file.
    let out = repo.maw_ok(&["ws", "diff", "alice"]);
    assert!(
        out.contains("+pub fn answer() -> i32 { 2 }"),
        "alice's own work missing: {out}"
    );
    assert!(
        !out.contains("bob_file.txt"),
        "bob's merged work must not show in alice's diff: {out}"
    );
}

/// An empty diff prints an explicit one-liner (stderr) saying what was
/// compared, so "no output" is never ambiguous.
#[test]
fn ws_diff_empty_prints_explicit_comparison_note() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "pub fn answer() -> i32 { 1 }\n")]);
    repo.create_workspace("alice");

    let out = Command::new(manifold_common::maw_bin())
        .args(["ws", "diff", "alice"])
        .current_dir(repo.root())
        .output()
        .expect("failed to run maw");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.trim().is_empty(),
        "patch stream should stay empty, got: {stdout}"
    );
    assert!(
        stderr.contains("No differences") && stderr.contains("epoch"),
        "expected explicit comparison note on stderr, got: {stderr}"
    );
}
