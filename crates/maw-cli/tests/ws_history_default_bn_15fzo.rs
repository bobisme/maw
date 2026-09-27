//! bn-15fzo: `maw ws history default` must resolve the default workspace in
//! the consolidated layout, where it is the repo root (there is no
//! `.maw/workspaces/default`). It used to fail "Workspace 'default' not
//! found" while `maw ops log` showed its op log.

use std::path::Path;
use std::process::{Command, Output, Stdio};

const MAW: &str = env!("CARGO_BIN_EXE_maw");

fn git_quiet(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

fn maw_raw(dir: &Path, args: &[&str]) -> Output {
    Command::new(MAW)
        .current_dir(dir)
        .args(args)
        .env_remove("MAW_FP")
        .output()
        .expect("run maw")
}

fn combined(out: &Output) -> String {
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    s
}

fn maw(dir: &Path, args: &[&str]) -> String {
    let out = maw_raw(dir, args);
    assert!(
        out.status.success(),
        "maw {args:?} failed:\n{}",
        combined(&out)
    );
    combined(&out)
}

fn setup(root: &Path) {
    git_quiet(root, &["init", "-b", "main"]);
    git_quiet(root, &["config", "user.email", "test@example.com"]);
    git_quiet(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("f.txt"), "base\n").expect("seed");
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-m", "seed"]);
    maw(root, &["init"]);
    git_quiet(root, &["add", "-A"]);
    let porcelain = Command::new("git")
        .current_dir(root)
        .args(["status", "--porcelain"])
        .output()
        .expect("git status");
    if !porcelain.stdout.is_empty() {
        git_quiet(root, &["commit", "-m", "maw config"]);
        maw(root, &["epoch", "sync"]);
    }
    maw(root, &["ws", "create", "a", "--from", "main"]);
    let a = root.join(".maw/workspaces/a");
    std::fs::write(a.join("a.txt"), "a\n").expect("write a.txt");
    maw(root, &["exec", "a", "--", "git", "add", "-A"]);
    maw(root, &["exec", "a", "--", "git", "commit", "-m", "a work"]);
}

#[test]
fn ws_history_default_resolves_in_consolidated_layout() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup(root);
    let out = maw_raw(
        root,
        &[
            "ws",
            "merge",
            "a",
            "--into",
            "default",
            "--destroy",
            "--message",
            "merge a",
        ],
    );
    assert!(out.status.success(), "{}", combined(&out));
    assert!(
        !root.join(".maw/workspaces/default").exists(),
        "sanity: consolidated layout has no .maw/workspaces/default"
    );

    let text = maw(root, &["ws", "history", "default"]);
    assert!(
        text.contains("merge"),
        "history must list the merge op:\n{text}"
    );

    let json = maw_raw(root, &["ws", "history", "default", "--format", "json"]);
    assert!(json.status.success(), "{}", combined(&json));
    let v: serde_json::Value = serde_json::from_slice(&json.stdout).expect("json");
    let ops = v["operations"].as_array().expect("operations");
    assert!(
        ops.iter().any(|op| op["op_type"] == "merge"),
        "json history must list the merge op: {v}"
    );

    // Unknown names still fail with the same guidance.
    let missing = maw_raw(root, &["ws", "history", "nope"]);
    assert!(!missing.status.success());
    assert!(combined(&missing).contains("not found"));
}
