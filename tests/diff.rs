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

// ---------------------------------------------------------------------------
// bn-1olb: stale workspace diffs against its own base epoch, not current epoch
// ---------------------------------------------------------------------------

/// A stale workspace (epoch advanced by a sibling merge with --no-auto-rebase)
/// must diff against its OWN creation epoch, not the current epoch.
///
/// Without the fix: `maw ws diff dave` shows "erin.txt deleted" even though
/// dave never touched erin.txt — the phantom deletion is the epoch's advance
/// inverted.  With the fix: only dave's real edit appears.
#[test]
fn ws_diff_stale_workspace_shows_only_own_changes_not_phantom_deletions() {
    let repo = TestRepo::new();
    repo.seed_files(&[("other.txt", "base content\n")]);

    // dave commits a real 1-line edit to other.txt
    repo.create_workspace("dave");
    repo.modify_file("dave", "other.txt", "dave's edit\n");
    repo.git_in_workspace("dave", &["add", "-A"]);
    repo.git_in_workspace("dave", &["commit", "-m", "dave: edit other.txt"]);

    // erin adds a new file and merges it.  We use --no-auto-rebase so dave
    // stays stale (its epoch ref is NOT advanced to the new epoch).
    repo.create_workspace("erin");
    repo.add_file("erin", "erin.txt", "erin was here\n");
    repo.git_in_workspace("erin", &["add", "-A"]);
    repo.git_in_workspace("erin", &["commit", "-m", "erin: add erin.txt"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "erin",
        "--no-auto-rebase",
        "--message",
        "merge erin",
    ]);

    // dave is now stale (epoch advanced past dave's base).
    // `maw ws diff dave` must show ONLY dave's real edit, NOT erin.txt as deleted.
    let out = Command::new(manifold_common::maw_bin())
        .args(["ws", "diff", "dave"])
        .current_dir(repo.root())
        .output()
        .expect("failed to run maw");
    assert!(
        out.status.success(),
        "maw ws diff dave failed:\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // dave's real edit must appear
    assert!(
        stdout.contains("-base content") || stdout.contains("+dave's edit"),
        "dave's real edit must appear in diff, got: {stdout}"
    );
    // erin.txt must NOT appear as deleted (phantom)
    assert!(
        !stdout.contains("erin.txt"),
        "erin.txt must not appear in dave's diff (dave never touched it), got: {stdout}"
    );
    // stderr NOTE must mention the stale situation
    assert!(
        stderr.contains("NOTE") && stderr.contains("dave"),
        "expected stale-workspace NOTE on stderr, got: {stderr}"
    );
}

/// Explicit `--against epoch` on a stale workspace always uses the CURRENT
/// epoch (not the workspace's own base), as documented.  This means the
/// sibling's merged file shows up in the diff — the caller opted in explicitly.
#[test]
fn ws_diff_stale_workspace_explicit_against_epoch_uses_current_epoch() {
    let repo = TestRepo::new();
    repo.seed_files(&[("other.txt", "base content\n")]);

    repo.create_workspace("dave");
    repo.modify_file("dave", "other.txt", "dave's edit\n");
    repo.git_in_workspace("dave", &["add", "-A"]);
    repo.git_in_workspace("dave", &["commit", "-m", "dave: edit other.txt"]);

    repo.create_workspace("erin");
    repo.add_file("erin", "erin.txt", "erin was here\n");
    repo.git_in_workspace("erin", &["add", "-A"]);
    repo.git_in_workspace("erin", &["commit", "-m", "erin: add erin.txt"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "erin",
        "--no-auto-rebase",
        "--message",
        "merge erin no-ar",
    ]);

    // With explicit --against epoch, current epoch is used → erin.txt appears as deleted
    let out = repo.maw_ok(&["ws", "diff", "dave", "--against", "epoch", "--name-only"]);
    assert!(
        out.contains("erin.txt"),
        "explicit --against epoch on a stale workspace must use current epoch (erin.txt deleted), got: {out}"
    );
}

/// JSON format of stale workspace diff uses the same resolved base (workspace
/// base epoch) as the patch format — the `against.label` is still "epoch".
#[test]
fn ws_diff_stale_workspace_json_uses_own_base_epoch() {
    let repo = TestRepo::new();
    repo.seed_files(&[("thing.txt", "original\n")]);

    repo.create_workspace("worker");
    repo.modify_file("worker", "thing.txt", "worker's change\n");
    repo.git_in_workspace("worker", &["add", "-A"]);
    repo.git_in_workspace("worker", &["commit", "-m", "worker: edit thing.txt"]);

    repo.create_workspace("sidekick");
    repo.add_file("sidekick", "new_file.txt", "hello\n");
    repo.git_in_workspace("sidekick", &["add", "-A"]);
    repo.git_in_workspace("sidekick", &["commit", "-m", "sidekick: add file"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "sidekick",
        "--no-auto-rebase",
        "--message",
        "merge sidekick no-ar",
    ]);

    let out = repo.maw_ok(&["ws", "diff", "worker", "--json"]);
    let json: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");

    // against.label is always "epoch"
    assert_eq!(json["against"]["label"].as_str(), Some("epoch"));
    // only worker's real edit shows — new_file.txt must not appear as deleted
    let files = json["files"].as_array().expect("files array");
    let paths: Vec<&str> = files.iter().filter_map(|f| f["path"].as_str()).collect();
    assert!(
        !paths.contains(&"new_file.txt"),
        "new_file.txt (sidekick's file) must not appear in worker's diff, got: {paths:?}"
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
