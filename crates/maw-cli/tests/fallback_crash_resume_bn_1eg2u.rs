//! bn-1eg2u: crash safety of the snapshot-failed fallback, and the report
//! of what a resumed target update found beyond the interrupted one.
//!
//! Item 4: the fallback (the dirty-trunk snapshot failed; force checkout +
//! repair from the in-memory capture) wrote no checkout intent, so a crash
//! inside it left recovery anchored at `epoch_before` against the merged
//! tree — the merge's own changes were snapshotted as "user edits" (and
//! reverted on disk) and the user's edits survived only in a recovery ref.
//!
//! Item 5: a resumed update pins residual changes it cannot place; the
//! warning must name each lost path with a command that restores it.
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

const FAIL_SNAPSHOT: &str = "FP_UPDATE_DEFAULT_BEFORE_SNAPSHOT=error:injected";

/// Crash inside the fallback, after its force checkout: recovery must end
/// with the merge AND every pre-merge edit, exactly as a live merge does.
#[test]
fn crash_inside_fallback_recovers_user_edits() {
    let live_dir = tempfile::tempdir().expect("tempdir");
    let live = live_dir.path();
    setup(live);
    let out = merge_a(live, Some(FAIL_SNAPSHOT));
    assert!(out.status.success(), "live merge:\n{}", combined(&out));
    // (The live fallback cannot replay shared.txt — the merge changed it —
    // so only its op trail is compared; the resumed update replays the pin
    // with the driver-aware 3-way merge and ends better off.)
    let live_kinds = default_op_kinds(live);

    for recovery_snapshot_fails in [false, true] {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        setup(root);
        let crash = merge_a(
            root,
            Some(&format!(
                "{FAIL_SNAPSHOT};FP_CLEANUP_FALLBACK_AFTER_CHECKOUT=abort"
            )),
        );
        let crash_text = combined(&crash);
        assert!(!crash.status.success(), "{crash_text}");
        assert!(
            crash_text.contains("snapshot_working_copy failed"),
            "the fallback must run:\n{crash_text}"
        );
        assert!(journal(root).exists(), "the crash must leave the journal");
        assert!(
            root.join(".maw/manifold/target-checkout-default.json")
                .exists(),
            "the fallback must record a checkout intent before its checkout:\n{crash_text}"
        );
        let out = maw_raw(
            root,
            &["ws", "merge", "--recover"],
            recovery_snapshot_fails.then_some(FAIL_SNAPSHOT),
        );
        let text = combined(&out);
        assert!(out.status.success(), "recover failed:\n{text}");
        assert!(!journal(root).exists(), "{text}");
        assert!(
            !root
                .join(".maw/manifold/target-checkout-default.json")
                .exists(),
            "{text}"
        );
        assert_target_state(root, &format!("{crash_text}\n---\n{text}"));
        assert_eq!(
            default_op_kinds(root),
            live_kinds,
            "recovered op trail must match the live fallback merge\n{text}"
        );
    }
}

/// Run the `restore yours:` command maw printed for `path`, verbatim.
fn run_printed_restore(root: &Path, text: &str, path: &str) {
    let quoted = format!(" {path}");
    let cmd = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("restore yours: "))
        .find(|c| c.ends_with(&quoted))
        .unwrap_or_else(|| panic!("no restore command printed for {path}:\n{text}"));
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd.replacen("maw ", &format!("{MAW} "), 1))
        .current_dir(root)
        .output()
        .expect("run restore");
    assert!(
        out.status.success(),
        "printed restore command `{cmd}` failed:\n{}\n---\n{text}",
        combined(&out)
    );
}

/// Edits made after a crash that followed the checkout: the resumed update
/// pins them and names each one with a command that restores it; merged and
/// replayed paths are not listed.
#[test]
fn resume_names_edits_made_after_the_crash_with_restore_commands() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup(root);
    let crash = merge_a(root, Some("FP_CLEANUP_AFTER_DEFAULT_CHECKOUT=abort"));
    assert!(!crash.status.success(), "{}", combined(&crash));
    // The tree is the merged tree now. The user keeps working.
    std::fs::write(root.join("after-crash.txt"), "work after the crash\n").expect("write");
    std::fs::write(root.join("a.txt"), "a\nedited after the crash\n").expect("edit a.txt");
    // A second version of a file the replay also restores: the command must
    // overwrite the replayed version (`--force`).
    std::fs::write(root.join("new.txt"), "second version\n").expect("rewrite new.txt");
    // Re-done pre-merge edits equal what the replay writes: nothing is lost.
    std::fs::write(root.join("f.txt"), "user edit\n").expect("redo f.txt");
    std::fs::write(
        root.join("shared.txt"),
        "1-a\n2\n3\n4\n5\n6\n7\n8\n9-user\n",
    )
    .expect("redo shared.txt");

    let text = maw(root, &["ws", "merge", "--recover"]);
    assert!(
        text.contains("are not in 'default' as pinned"),
        "the resumed update must list what it pinned:\n{text}"
    );
    for merged_or_replayed in ["    shared.txt", "    f.txt"] {
        assert!(
            !text.lines().any(|l| l == merged_or_replayed),
            "{merged_or_replayed} is not lost and must not be listed:\n{text}"
        );
    }
    // The pre-merge edits were replayed on top of the merge.
    assert_eq!(
        std::fs::read_to_string(root.join("f.txt")).expect("f.txt"),
        "user edit\n",
        "{text}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).expect("a.txt"),
        "a\n",
        "the merged a.txt is on disk until restored:\n{text}"
    );

    run_printed_restore(root, &text, "after-crash.txt");
    run_printed_restore(root, &text, "a.txt");
    run_printed_restore(root, &text, "new.txt");
    assert_eq!(
        std::fs::read_to_string(root.join("new.txt")).expect("new.txt"),
        "second version\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("after-crash.txt")).expect("after-crash"),
        "work after the crash\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).expect("a.txt"),
        "a\nedited after the crash\n"
    );
}

/// A crash after the snapshot cleaned the tree (before the checkout) leaves
/// the ANCHOR tree: against the merged commit every merged path then looks
/// "reverted". Only the genuinely new edit may be listed.
#[test]
fn resume_after_pre_checkout_crash_lists_only_new_edits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup(root);
    let crash = merge_a(root, Some("FP_SNAPSHOT_AFTER_CLEAN=abort"));
    assert!(!crash.status.success(), "{}", combined(&crash));
    std::fs::write(root.join("after-crash.txt"), "work after the crash\n").expect("write");

    let text = maw(root, &["ws", "merge", "--recover"]);
    assert!(
        text.contains("1 path(s) pinned at"),
        "exactly the new edit must be listed:\n{text}"
    );
    run_printed_restore(root, &text, "after-crash.txt");
    assert_eq!(
        std::fs::read_to_string(root.join("after-crash.txt")).expect("after-crash"),
        "work after the crash\n"
    );
    std::fs::remove_file(root.join("after-crash.txt")).expect("rm after-crash");
    assert_target_state(root, &text);
}
