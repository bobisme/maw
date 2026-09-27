//! bn-15fzo: a crash inside `update_default_workspace` AFTER the target
//! checkout but BEFORE the replay and the per-workspace epoch ref write
//! (whether the live merge or crash recovery itself crashes there) left the
//! next recovery anchored at `epoch_before` against the already-merged tree:
//! it snapshotted the merged tree as "user edits" and never replayed the
//! user's real pre-merge edits, which survived only in a recovery ref.
//!
//! Needs `--features failpoints`.
#![cfg(feature = "failpoints")]

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

fn git_raw(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    git_raw(dir, args).trim().to_owned()
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

fn journal(root: &Path) -> PathBuf {
    root.join(".maw/manifold/merge-state.json")
}

/// Repo with workspace `a` committing `a.txt` and editing `shared.txt`, and
/// the target (repo root) carrying uncommitted edits: `f.txt` (untouched by
/// the merge), an untracked `new.txt`, and `shared.txt` in a hunk far from
/// the one `a` edits.
fn setup(root: &Path) {
    git_quiet(root, &["init", "-b", "main"]);
    git_quiet(root, &["config", "user.email", "test@example.com"]);
    git_quiet(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("f.txt"), "base\n").expect("seed");
    std::fs::write(root.join("shared.txt"), "1\n2\n3\n4\n5\n6\n7\n8\n9\n").expect("seed");
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
    let a = ws_path(root, "a");
    std::fs::write(a.join("a.txt"), "a\n").expect("write a.txt");
    std::fs::write(a.join("shared.txt"), "1-a\n2\n3\n4\n5\n6\n7\n8\n9\n").expect("edit shared");
    maw(root, &["exec", "a", "--", "git", "add", "-A"]);
    maw(root, &["exec", "a", "--", "git", "commit", "-m", "a work"]);
    std::fs::write(root.join("f.txt"), "user edit\n").expect("dirty target");
    std::fs::write(root.join("new.txt"), "untracked user file\n").expect("untracked");
    std::fs::write(root.join("shared.txt"), "1\n2\n3\n4\n5\n6\n7\n8\n9-user\n")
        .expect("dirty shared");
}

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

/// The target must hold the merge AND every pre-merge user edit, with HEAD
/// back on the branch at the merged commit.
fn assert_target_state(root: &Path, context: &str) {
    let read =
        |p: &str| std::fs::read_to_string(root.join(p)).unwrap_or_else(|_| "<missing>".into());
    assert_eq!(
        read("f.txt"),
        "user edit\n",
        "f.txt user edit lost\n{context}"
    );
    assert_eq!(
        read("new.txt"),
        "untracked user file\n",
        "untracked user file lost\n{context}"
    );
    assert_eq!(
        read("shared.txt"),
        "1-a\n2\n3\n4\n5\n6\n7\n8\n9-user\n",
        "shared.txt must carry both the merge hunk and the user hunk\n{context}"
    );
    assert_eq!(read("a.txt"), "a\n", "merged a.txt missing\n{context}");
    assert_eq!(
        git_out(root, &["symbolic-ref", "HEAD"]),
        "refs/heads/main",
        "HEAD must be attached to the branch\n{context}"
    );
    assert_eq!(
        git_out(root, &["rev-parse", "HEAD"]),
        git_out(root, &["rev-parse", "refs/manifold/epoch/current"]),
        "HEAD must be the merged epoch\n{context}"
    );
    // The user's edits are uncommitted edits relative to the merged commit —
    // nothing of the merge shows up as a local change (that would mean the
    // merge got recorded as "user edits" and would revert on commit).
    let status = git_raw(root, &["status", "--porcelain", "--untracked-files=all"]);
    let paths: Vec<&str> = status
        .lines()
        .map(|l| l[3..].trim())
        .filter(|p| !p.starts_with(".maw"))
        .collect();
    let mut sorted = paths.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        vec!["f.txt", "new.txt", "shared.txt"],
        "status vs merged commit\n{status}\n{context}"
    );
}

/// The live merge crashes after the target checkout, before the replay.
/// Recovery must replay the user's pre-merge edits, not snapshot the merged
/// tree as if it were user work.
#[test]
fn live_crash_after_target_checkout_recovers_user_edits() {
    let live_dir = tempfile::tempdir().expect("tempdir");
    let live = live_dir.path();
    setup(live);
    let out = merge_a(live, None);
    assert!(out.status.success(), "live merge:\n{}", combined(&out));
    assert_target_state(live, "live merge");
    let live_kinds = default_op_kinds(live);

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup(root);
    let crash = merge_a(root, Some("FP_CLEANUP_AFTER_DEFAULT_CHECKOUT=abort"));
    assert!(!crash.status.success(), "{}", combined(&crash));
    assert!(journal(root).exists(), "the crash must leave the journal");
    let text = maw(root, &["ws", "merge", "--recover"]);
    assert!(!journal(root).exists(), "{text}");
    assert_target_state(root, &text);
    assert_eq!(
        default_op_kinds(root),
        live_kinds,
        "recovered op trail must match the live merge\n{text}"
    );
}

/// The live merge crashes before the target checkout; the FIRST recovery
/// then crashes after its checkout (double crash). The second recovery must
/// still land the user's edits on top of the merge.
#[test]
fn recovery_crash_after_target_checkout_second_recovery_keeps_user_edits() {
    let live_dir = tempfile::tempdir().expect("tempdir");
    let live = live_dir.path();
    setup(live);
    let out = merge_a(live, None);
    assert!(out.status.success(), "live merge:\n{}", combined(&out));
    let live_kinds = default_op_kinds(live);

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup(root);
    let crash = merge_a(root, Some("FP_CLEANUP_BEFORE_DEFAULT_CHECKOUT=abort"));
    assert!(!crash.status.success(), "{}", combined(&crash));
    assert!(journal(root).exists(), "the crash must leave the journal");

    let crash2 = maw_raw(
        root,
        &["ws", "merge", "--recover"],
        Some("FP_CLEANUP_AFTER_DEFAULT_CHECKOUT=abort"),
    );
    assert!(!crash2.status.success(), "{}", combined(&crash2));
    assert!(
        journal(root).exists(),
        "the recovery crash must leave the journal"
    );

    let text = maw(root, &["ws", "merge", "--recover"]);
    assert!(!journal(root).exists(), "{text}");
    assert_target_state(root, &text);
    assert_eq!(
        default_op_kinds(root),
        live_kinds,
        "recovered op trail must match the live merge\n{text}"
    );

    // Idempotent afterwards.
    let again = maw_raw(root, &["ws", "merge", "--recover"], None);
    assert!(again.status.success(), "{}", combined(&again));
    assert_target_state(root, "after a no-op recovery");
}

/// Same double crash with a CLEAN target: recovery must not record the
/// merge's own changes as a target Snapshot op.
#[test]
fn clean_target_crash_after_checkout_records_no_bogus_snapshot() {
    let setup_clean = |root: &Path| {
        setup(root);
        git_quiet(root, &["checkout", "--", "f.txt", "shared.txt"]);
        std::fs::remove_file(root.join("new.txt")).expect("rm new.txt");
    };
    let live_dir = tempfile::tempdir().expect("tempdir");
    let live = live_dir.path();
    setup_clean(live);
    let out = merge_a(live, None);
    assert!(out.status.success(), "live merge:\n{}", combined(&out));
    let live_kinds = default_op_kinds(live);
    assert!(
        !live_kinds.iter().any(|k| k == "snapshot"),
        "sanity: a clean target records no Snapshot: {live_kinds:?}"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup_clean(root);
    let crash = merge_a(root, Some("FP_CLEANUP_AFTER_DEFAULT_CHECKOUT=abort"));
    assert!(!crash.status.success(), "{}", combined(&crash));
    let text = maw(root, &["ws", "merge", "--recover"]);
    assert_eq!(default_op_kinds(root), live_kinds, "{text}");
    let status = git_raw(root, &["status", "--porcelain"]);
    assert!(
        status.lines().all(|l| l[3..].starts_with(".maw")),
        "clean target must end clean:\n{status}\n{text}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).expect("a.txt"),
        "a\n"
    );
}
