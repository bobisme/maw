//! Real-subprocess coverage for `maw undo` / `maw ops log` (bn-117s).
//!
//! These drive the *built* `maw` binary (via `CARGO_BIN_EXE_maw`) through a
//! full `ws merge --destroy` → `undo` → `undo` (redo) cycle, so they exercise
//! the actual command wiring, guarded ref movement, source restore, and the
//! refusal rails end-to-end. Rebuild `maw-cli` before trusting a run.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MAW: &str = env!("CARGO_BIN_EXE_maw");

fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        status.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
}

fn git_rev(dir: &Path, spec: &str) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", spec])
        .output()
        .expect("git rev-parse");
    assert!(out.status.success(), "git rev-parse {spec} failed");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn maw(dir: &Path, args: &[&str]) -> Output {
    Command::new(MAW)
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run maw")
}

fn maw_ok(dir: &Path, args: &[&str]) -> String {
    let out = maw(dir, args);
    assert!(
        out.status.success(),
        "maw {args:?} failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn setup_repo(dir: &Path) -> PathBuf {
    run_git(dir, &["init", "-b", "main"]);
    run_git(dir, &["config", "user.email", "test@example.com"]);
    run_git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("file.txt"), "base\n").expect("write");
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-m", "init"]);
    let out = maw(dir, &["init"]);
    assert!(
        out.status.success(),
        "maw init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir.to_path_buf()
}

/// Create workspace `name`, commit an appended line, and merge it into default
/// with `--destroy`. Returns (`epoch_before`, `epoch_after`).
fn merge_destroy(root: &Path, name: &str) -> (String, String) {
    let before = git_rev(root, "refs/manifold/epoch/current");
    maw_ok(root, &["ws", "create", name, "--from", "main"]);
    let script =
        format!("printf '{name}\\n' >> file.txt && git add -A && git commit -m '{name} change'");
    let out = maw(root, &["exec", name, "--", "sh", "-c", &script]);
    assert!(
        out.status.success(),
        "workspace commit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = maw(
        root,
        &[
            "ws",
            "merge",
            name,
            "--into",
            "default",
            "--destroy",
            "--message",
            "feat: merge",
        ],
    );
    assert!(
        out.status.success(),
        "merge failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let after = git_rev(root, "refs/manifold/epoch/current");
    assert_ne!(before, after, "merge should advance the epoch");
    (before, after)
}

#[test]
fn merge_then_undo_restores_repo_and_pins_result() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = setup_repo(tmp.path());
    let (before, after) = merge_destroy(&root, "alice");

    // ops log shows the merge (test f, part 1).
    let log = maw_ok(&root, &["ops", "log"]);
    assert!(log.contains("merge"), "ops log shows the merge:\n{log}");

    // Undo.
    let out = maw_ok(&root, &["undo"]);
    assert!(out.contains("Undid merge"), "undo reports success:\n{out}");

    // Epoch + branch back at epoch_before (test a).
    assert_eq!(git_rev(&root, "refs/manifold/epoch/current"), before);
    assert_eq!(git_rev(&root, "main"), before);

    // Trunk file back to pre-merge content.
    let file = std::fs::read_to_string(root.join("file.txt")).expect("read file");
    assert_eq!(file, "base\n", "trunk rewound to before the merge");

    // Source workspace restored with its commit (test a).
    let list = maw_ok(&root, &["ws", "list"]);
    assert!(list.contains("alice"), "alice restored:\n{list}");
    let alice_file = maw_ok(&root, &["exec", "alice", "--", "cat", "file.txt"]);
    assert!(
        alice_file.contains("alice"),
        "restored alice keeps its committed change:\n{alice_file}"
    );

    // Merge result still reachable via the pinned undo recovery ref (test a).
    let refs = Command::new("git")
        .current_dir(&root)
        .args([
            "for-each-ref",
            "--format=%(objectname)",
            "refs/manifold/recovery/undo/",
        ])
        .output()
        .expect("for-each-ref");
    let pinned = String::from_utf8_lossy(&refs.stdout);
    assert!(
        pinned.contains(&after),
        "the merge result {after} stays pinned under recovery/undo:\n{pinned}"
    );

    // ops log now shows the compensation (test f, part 2).
    let log = maw_ok(&root, &["ops", "log"]);
    assert!(
        log.contains("maw-undo merge"),
        "ops log shows the compensation:\n{log}"
    );
}

#[test]
fn undo_then_undo_is_redo() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = setup_repo(tmp.path());
    let (before, after) = merge_destroy(&root, "bob");

    maw_ok(&root, &["undo"]);
    assert_eq!(git_rev(&root, "refs/manifold/epoch/current"), before);

    // Second undo = redo (test b).
    let out = maw_ok(&root, &["undo"]);
    assert!(out.contains("Redid merge"), "second undo redoes:\n{out}");
    assert_eq!(git_rev(&root, "refs/manifold/epoch/current"), after);
    assert_eq!(git_rev(&root, "main"), after);

    // Source re-destroyed by the redo.
    let list = maw_ok(&root, &["ws", "list"]);
    assert!(
        !list.contains("bob"),
        "redo re-destroys the source workspace:\n{list}"
    );
}

#[test]
fn undo_refuses_when_epoch_advanced_since_merge() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = setup_repo(tmp.path());
    merge_destroy(&root, "carol");

    // A direct out-of-maw commit + epoch sync advances the epoch on top of the
    // merge (test c).
    std::fs::write(root.join("extra.txt"), "extra\n").expect("write");
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["commit", "-m", "direct commit"]);
    maw_ok(&root, &["epoch", "sync"]);

    let out = maw(&root, &["undo"]);
    assert!(!out.status.success(), "undo must refuse");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("epoch has advanced"),
        "refusal explains the advanced epoch:\n{stderr}"
    );
    assert!(
        stderr.contains("To fix"),
        "refusal is self-contained:\n{stderr}"
    );
}

#[test]
fn dry_run_reports_plan_without_acting() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = setup_repo(tmp.path());
    let (_before, after) = merge_destroy(&root, "dave");

    let out = maw_ok(&root, &["undo", "--dry-run"]);
    assert!(out.contains("Undo plan"), "dry-run prints a plan:\n{out}");
    assert!(
        out.contains("Would proceed"),
        "dry-run says it would proceed"
    );

    // Nothing changed.
    assert_eq!(git_rev(&root, "refs/manifold/epoch/current"), after);
    assert!(maw_ok(&root, &["ws", "list"]).contains("default"));
}

#[test]
fn nothing_to_undo_on_fresh_repo() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = setup_repo(tmp.path());
    let out = maw(&root, &["undo"]);
    assert!(!out.status.success(), "no epoch mutation to undo");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Nothing to undo"),
        "clear message on empty history:\n{stderr}"
    );
}
