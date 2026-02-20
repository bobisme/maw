//! Integration tests for git compatibility on Manifold repos.
//!
//! Verifies that standard git tools (log, blame, bisect, grep, diff) continue
//! to work with Manifold metadata refs present.

mod manifold_common;

use std::process::Command;

use manifold_common::TestRepo;

fn git_ok_in(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run git");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "git {} failed\nstdout: {}\nstderr: {}",
        args.join(" "),
        stdout,
        stderr
    );
    stdout
}

#[test]
fn git_log_grep_and_diff_work_across_epochs() {
    let repo = TestRepo::new();

    // Epoch 1
    repo.seed_files(&[("src/lib.rs", "pub fn value() -> i32 { 1 }\n")]);

    // Epoch 2
    repo.modify_file("default", "src/lib.rs", "pub fn value() -> i32 { 2 }\n");
    let epoch2 = repo.advance_epoch("feat: bump value to 2");

    // Epoch 3
    repo.add_file("default", "README.md", "# Manifold test\n");
    let epoch3 = repo.advance_epoch("docs: add readme");

    let ws_default = repo.default_workspace();

    // git log should be clean and linear enough for normal tooling.
    let log = git_ok_in(&ws_default, &["log", "--oneline", "-3"]);
    assert!(log.contains("docs: add readme"));
    assert!(log.contains("feat: bump value to 2"));

    // git grep should find current content.
    let grep = git_ok_in(
        &ws_default,
        &["grep", "-F", "value() -> i32 { 2 }", "--", "src/lib.rs"],
    );
    assert!(grep.contains("src/lib.rs"));

    // git diff across epochs should show expected file changes.
    let diff = git_ok_in(&ws_default, &["diff", "--name-only", &epoch2, &epoch3]);
    assert!(diff.contains("README.md"));

    // No conflict markers in the resulting mainline snapshot.
    let lib_content = repo.read_file("default", "src/lib.rs").unwrap();
    assert!(!lib_content.contains("<<<<<<<"));
}

#[test]
fn git_blame_and_bisect_work_with_manifold_refs_present() {
    let repo = TestRepo::new();

    // Seed file (epoch 1)
    let epoch1 = repo.seed_files(&[("src/main.rs", "fn score() -> i32 {\n    1\n}\n")]);

    // Introduce a bad value in epoch 2.
    repo.modify_file(
        "default",
        "src/main.rs",
        "fn score() -> i32 {\n    999\n}\n",
    );
    repo.advance_epoch("feat: introduce regression");

    // Fix in epoch 3.
    repo.modify_file("default", "src/main.rs", "fn score() -> i32 {\n    2\n}\n");
    let epoch3 = repo.advance_epoch("fix: restore sane score");

    // Add an extra manifold metadata ref to prove refs/manifold/* doesn't interfere.
    repo.git(&["update-ref", "refs/manifold/epoch/snapshot", &epoch3]);

    let ws_default = repo.default_workspace();

    // git blame should still attribute line origins.
    let blame = git_ok_in(&ws_default, &["blame", "-L", "2,2", "src/main.rs"]);
    assert!(
        blame.contains("2"),
        "blame should include current line content: {blame}"
    );

    // git bisect should run through commit graph with manifold refs present.
    git_ok_in(&ws_default, &["bisect", "start"]);
    git_ok_in(&ws_default, &["bisect", "bad", &epoch3]);
    let bisect_out = git_ok_in(&ws_default, &["bisect", "good", &epoch1]);
    assert!(
        bisect_out.contains("Bisecting") || bisect_out.contains("is the first bad commit"),
        "unexpected bisect output: {bisect_out}"
    );
    git_ok_in(&ws_default, &["bisect", "reset"]);
}
