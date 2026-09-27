//! bn-1losw: does a crash-recovered merge leave the same op-log trail as the
//! live merge?
//!
//! The live merge, after checking out the merged commit into a DIRTY target,
//! appends a `Snapshot` op (the target's pre-checkout patch set) to the
//! target's op log right after the `Merge` op. Crash recovery
//! (`merge/recover.rs`) did not, so `maw ops log` / `maw ws history` showed a
//! different trail for the same merge, and a later `maw ws undo` on the
//! target recorded its `Compensate` against a different op.
//!
//! The crash tests need `--features failpoints`.

use std::path::{Path, PathBuf};
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

fn maw_raw(dir: &Path, args: &[&str], fp: Option<&str>) -> Output {
    let mut cmd = Command::new(MAW);
    cmd.current_dir(dir).args(args).env_remove("MAW_FP");
    if let Some(spec) = fp {
        cmd.env("MAW_FP", spec);
    }
    cmd.output().expect("run maw")
}

fn combined(out: &Output) -> String {
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    s
}

fn maw(dir: &Path, args: &[&str]) -> String {
    let out = maw_raw(dir, args, None);
    assert!(
        out.status.success(),
        "maw {args:?} failed:\n{}",
        combined(&out)
    );
    combined(&out)
}

fn ws_path(root: &Path, name: &str) -> PathBuf {
    root.join(".maw/workspaces").join(name)
}

#[cfg_attr(not(feature = "failpoints"), allow(dead_code))]
fn journal(root: &Path) -> PathBuf {
    root.join(".maw/manifold/merge-state.json")
}

/// Repo with workspace `a` committing `a.txt`, and the target (repo root)
/// carrying an uncommitted edit to the unrelated `f.txt`.
#[cfg_attr(not(feature = "failpoints"), allow(dead_code))]
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
    std::fs::write(ws_path(root, "a").join("a.txt"), "a\n").expect("write a.txt");
    maw(root, &["exec", "a", "--", "git", "add", "-A"]);
    maw(root, &["exec", "a", "--", "git", "commit", "-m", "a work"]);
    std::fs::write(root.join("f.txt"), "user edit\n").expect("dirty target");
}

#[cfg_attr(not(feature = "failpoints"), allow(dead_code))]
fn merge_a(root: &Path, fp: Option<&str>) -> Output {
    maw_raw(
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
        fp,
    )
}

/// The `default` workspace's op kinds (sorted: `maw ops log` orders by
/// millisecond timestamp, so same-millisecond ops can swap places).
#[cfg_attr(not(feature = "failpoints"), allow(dead_code))]
fn default_op_kinds(root: &Path) -> Vec<String> {
    let out = maw_raw(root, &["ops", "log", "--format", "json"], None);
    assert!(out.status.success(), "{}", combined(&out));
    let ops: serde_json::Value = serde_json::from_slice(&out.stdout).expect("ops log json");
    let mut kinds: Vec<String> = ops
        .as_array()
        .expect("array")
        .iter()
        .filter(|op| op["workspace"] == "default")
        .map(|op| op["kind"].as_str().unwrap_or_default().to_owned())
        .collect();
    kinds.sort();
    kinds
}

/// Crash after the CAS, before the target checkout; recover. The target's op
/// trail, bytes, and a subsequent `maw undo` must match the live merge.
#[cfg(feature = "failpoints")]
#[test]
fn recovered_dirty_target_merge_records_same_ops_as_live_merge() {
    let live_dir = tempfile::tempdir().expect("tempdir");
    let live = live_dir.path();
    setup(live);
    let out = merge_a(live, None);
    assert!(out.status.success(), "live merge:\n{}", combined(&out));
    let live_kinds = default_op_kinds(live);
    assert!(
        live_kinds.iter().any(|k| k == "snapshot"),
        "sanity: the live merge into a dirty target records a Snapshot op \
         after the Merge op: {live_kinds:?}"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup(root);
    let crash = merge_a(root, Some("FP_CLEANUP_BEFORE_DEFAULT_CHECKOUT=abort"));
    assert!(!crash.status.success(), "{}", combined(&crash));
    assert!(journal(root).exists(), "the crash must leave the journal");
    let text = maw(root, &["ws", "merge", "--recover"]);
    assert!(text.contains("already landed"), "{text}");
    assert!(!journal(root).exists());

    assert_eq!(
        default_op_kinds(root),
        live_kinds,
        "crash-recovered merge must leave the same op trail on the target \
         as the live merge\nrecover output:\n{text}"
    );
    for r in [live, root] {
        assert_eq!(
            std::fs::read_to_string(r.join("f.txt")).expect("f.txt"),
            "user edit\n"
        );
        assert_eq!(
            std::fs::read_to_string(r.join("a.txt")).expect("a.txt"),
            "a\n"
        );
    }

    // Idempotent: a second recovery attempt has nothing to do and must not
    // append another Snapshot.
    let again = maw_raw(root, &["ws", "merge", "--recover"], None);
    assert!(again.status.success(), "{}", combined(&again));
    assert_eq!(default_op_kinds(root), live_kinds);

    // `maw undo` behaves the same after either path.
    let undo_live = maw_raw(live, &["undo"], None);
    let undo_rec = maw_raw(root, &["undo"], None);
    assert_eq!(
        undo_live.status.success(),
        undo_rec.status.success(),
        "live undo:\n{}\nrecovered undo:\n{}",
        combined(&undo_live),
        combined(&undo_rec)
    );
    assert_eq!(default_op_kinds(root), default_op_kinds(live));
}

/// Crash after the CAS and AFTER the target checkout + Snapshot op (during
/// the source destroy): the live merge already wrote the Snapshot, so
/// recovery must not write another — and must not take a fresh patch set of
/// the checked-out tree relative to the OLD epoch, which would record the
/// merge's own changes as user edits.
#[cfg(feature = "failpoints")]
#[test]
fn recovery_after_target_checkout_does_not_snapshot_again() {
    let live_dir = tempfile::tempdir().expect("tempdir");
    let live = live_dir.path();
    setup(live);
    let out = merge_a(live, None);
    assert!(out.status.success(), "live merge:\n{}", combined(&out));

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup(root);
    let crash = merge_a(root, Some("FP_CLEANUP_AFTER_CAPTURE=abort"));
    assert!(!crash.status.success(), "{}", combined(&crash));
    assert!(journal(root).exists(), "the crash must leave the journal");
    let text = maw(root, &["ws", "merge", "--recover"]);
    assert!(text.contains("already landed"), "{text}");
    assert_eq!(
        default_op_kinds(root),
        default_op_kinds(live),
        "recovery after the checkout already ran must leave the live trail\n{text}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("f.txt")).expect("f.txt"),
        "user edit\n"
    );
}
