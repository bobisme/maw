//! bn-2nnuz: a merged commit that CLEARS an executable bit (100755 -> 100644)
//! must clear it in every worktree the merge materializes into.
//!
//! The native checkout (gix) writes over an existing file in place and only
//! ever ADDS the executable bit, so the trunk kept `+x`: `git status` showed a
//! mode change and the next `git commit -a` silently reverted the merged
//! mode. The snapshot-failed fallback's fidelity repair had the mirror bug: it
//! restored the user's bytes but kept the checkout's mode, dropping the
//! user's own uncommitted `chmod`.
//!
//! Covered here: clean trunk, dirty-trunk replay (unrelated and same-file
//! dirty edits), the user's own mode change still winning, sibling workspaces
//! the merge fast-forwards or auto-rebases, `maw ws sync`, and — with
//! `--features failpoints` — the snapshot-failed fallback.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const MAW: &str = env!("CARGO_BIN_EXE_maw");

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

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

fn set_exec(path: &Path, exec: bool) {
    let mode = if exec { 0o755 } else { 0o644 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

fn is_exec(path: &Path) -> bool {
    let meta = std::fs::symlink_metadata(path).expect("stat");
    assert!(meta.is_file(), "{} must be a regular file", path.display());
    meta.permissions().mode() & 0o111 != 0
}

fn committed_mode(root: &Path, rev: &str, path: &str) -> String {
    let line = git(root, &["ls-tree", rev, "--", path]);
    line.split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned()
}

const SEED: &str = "#!/bin/sh\necho one\necho two\necho three\n";

/// Seed `mode_only.sh`, `mode_content.sh`, `user.sh` (all 100755) and
/// `notes.txt` (100644), then `maw init`.
fn init_repo(root: &Path) {
    git_quiet(root, &["init", "-b", "main"]);
    git_quiet(root, &["config", "user.email", "test@example.com"]);
    git_quiet(root, &["config", "user.name", "Test"]);
    for f in ["mode_only.sh", "mode_content.sh", "user.sh"] {
        std::fs::write(root.join(f), SEED).expect("seed");
        set_exec(&root.join(f), true);
    }
    std::fs::write(root.join("notes.txt"), "notes\n").expect("seed");
    set_exec(&root.join("notes.txt"), false);
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-m", "seed"]);
    maw(root, &["init"]);
    git_quiet(root, &["add", "-A"]);
    if !git(root, &["status", "--porcelain"]).is_empty() {
        git_quiet(root, &["commit", "-m", "maw init"]);
        maw(root, &["epoch", "sync"]);
    }
}

/// Workspace `a`: clear the exec bit of `mode_only.sh` (mode only) and of
/// `mode_content.sh` (mode + content), commit.
fn ws_a_clears_exec(root: &Path) {
    maw(root, &["ws", "create", "a", "--from", "main"]);
    let ws = ws_path(root, "a");
    set_exec(&ws.join("mode_only.sh"), false);
    std::fs::write(ws.join("mode_content.sh"), format!("{SEED}echo ws\n")).expect("ws edit");
    set_exec(&ws.join("mode_content.sh"), false);
    maw(root, &["exec", "a", "--", "git", "add", "-A"]);
    maw(
        root,
        &["exec", "a", "--", "git", "commit", "-m", "a: chmod -x"],
    );
}

fn merge_a(root: &Path, fp: Option<&str>) -> String {
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
        fp,
    );
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    text
}

/// The merged `chmod -x` is committed AND on disk in `dir`.
fn assert_exec_cleared(root: &Path, dir: &Path, text: &str) {
    for f in ["mode_only.sh", "mode_content.sh"] {
        assert_eq!(committed_mode(root, "main", f), "100644", "{f}:\n{text}");
        assert!(
            !is_exec(&dir.join(f)),
            "{}: the merged chmod -x must clear the worktree's exec bit:\n{text}",
            dir.join(f).display()
        );
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("mode_content.sh")).expect("read"),
        format!("{SEED}echo ws\n"),
        "{text}"
    );
    assert!(
        !text.contains("did not materialize cleanly"),
        "the checkout itself must clear the exec bit, not the bn-3gba repair backstop:\n{text}"
    );
    let diff = git(dir, &["diff", "--", "mode_only.sh", "mode_content.sh"]);
    assert!(
        !diff.contains("old mode"),
        "the worktree must not carry a mode revert:\n{diff}\n{text}"
    );
}

/// A user commit of the trunk's dirty state keeps the merged modes.
fn assert_next_commit_keeps_mode(root: &Path) {
    let dirty = git(root, &["status", "--porcelain", "--untracked-files=no"]);
    if !dirty.is_empty() {
        git_quiet(root, &["commit", "-a", "-m", "user commits local edits"]);
    }
    assert_eq!(committed_mode(root, "HEAD", "mode_only.sh"), "100644");
    assert_eq!(committed_mode(root, "HEAD", "mode_content.sh"), "100644");
}

#[test]
fn clean_trunk_merge_clears_exec_bit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root);
    ws_a_clears_exec(root);

    let text = merge_a(root, None);
    assert_exec_cleared(root, root, &text);
    assert_eq!(
        git(root, &["status", "--porcelain", "--untracked-files=no"]),
        "",
        "a clean trunk must stay clean after the merge:\n{text}"
    );
    assert_next_commit_keeps_mode(root);
}

#[test]
fn dirty_trunk_replay_clears_merged_exec_bit_and_keeps_user_edits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root);
    ws_a_clears_exec(root);
    // Dirty trunk: a content edit to the mode-only file (same path as the
    // merged mode change) and an unrelated edit elsewhere.
    std::fs::write(root.join("mode_only.sh"), format!("{SEED}echo user\n")).expect("dirty");
    std::fs::write(root.join("notes.txt"), "notes\nuser\n").expect("dirty");

    let text = merge_a(root, None);
    assert!(
        !is_exec(&root.join("mode_only.sh")),
        "merged -x must survive the replay of a same-file content edit:\n{text}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("mode_only.sh")).expect("read"),
        format!("{SEED}echo user\n"),
        "{text}"
    );
    assert!(!is_exec(&root.join("mode_content.sh")), "{text}");
    assert_eq!(
        std::fs::read_to_string(root.join("notes.txt")).expect("read"),
        "notes\nuser\n",
        "{text}"
    );
    assert!(!git(root, &["diff"]).contains("old mode"), "{text}");
    assert_next_commit_keeps_mode(root);
}

#[test]
fn users_own_uncommitted_mode_change_still_wins() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root);
    ws_a_clears_exec(root);
    // The user's own mode changes: chmod +x notes.txt (mode only) and
    // chmod -x + edit user.sh; neither is touched by the merge.
    set_exec(&root.join("notes.txt"), true);
    std::fs::write(root.join("user.sh"), format!("{SEED}echo user\n")).expect("dirty");
    set_exec(&root.join("user.sh"), false);

    let text = merge_a(root, None);
    assert_exec_cleared(root, root, &text);
    assert!(
        is_exec(&root.join("notes.txt")),
        "user's +x must win:\n{text}"
    );
    assert!(
        !is_exec(&root.join("user.sh")),
        "user's -x must win:\n{text}"
    );
    git_quiet(root, &["commit", "-a", "-m", "user commits"]);
    assert_eq!(committed_mode(root, "HEAD", "notes.txt"), "100755");
    assert_eq!(committed_mode(root, "HEAD", "user.sh"), "100644");
    assert_eq!(committed_mode(root, "HEAD", "mode_only.sh"), "100644");
}

#[test]
fn sibling_fast_forwarded_by_merge_clears_exec_bit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root);
    // Sibling `b` sits clean at the pre-merge epoch.
    maw(root, &["ws", "create", "b", "--from", "main"]);
    ws_a_clears_exec(root);
    let b = ws_path(root, "b");
    assert!(is_exec(&b.join("mode_only.sh")));

    let text = merge_a(root, None);
    let main = git(root, &["rev-parse", "main"]);
    assert_eq!(
        git(&b, &["rev-parse", "HEAD"]),
        main,
        "the clean sibling must be fast-forwarded by the merge:\n{text}"
    );
    assert_exec_cleared(root, &b, &text);
    assert_eq!(
        git(&b, &["status", "--porcelain", "--untracked-files=no"]),
        "",
        "{text}"
    );
}

#[test]
fn ws_sync_fast_forward_clears_exec_bit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root);
    maw(root, &["ws", "create", "b", "--from", "main"]);
    let b = ws_path(root, "b");
    // The epoch advances outside a merge: a direct trunk commit clearing the
    // exec bits, absorbed by `maw epoch sync`. `b` is left stale.
    set_exec(&root.join("mode_only.sh"), false);
    std::fs::write(root.join("mode_content.sh"), format!("{SEED}echo ws\n")).expect("edit");
    set_exec(&root.join("mode_content.sh"), false);
    git_quiet(root, &["commit", "-a", "-m", "trunk: chmod -x"]);
    let mut text = maw(root, &["epoch", "sync"]);
    assert!(
        is_exec(&b.join("mode_only.sh")),
        "b must still be stale:\n{text}"
    );

    text.push_str(&maw(root, &["ws", "sync", "b"]));
    assert_eq!(
        git(&b, &["rev-parse", "HEAD"]),
        git(root, &["rev-parse", "main"]),
        "{text}"
    );
    assert_exec_cleared(root, &b, &text);
    assert_eq!(
        git(&b, &["status", "--porcelain", "--untracked-files=no"]),
        "",
        "{text}"
    );
}

#[test]
fn sibling_with_own_commits_rebased_by_merge_clears_exec_bit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root);
    // Sibling `b` has its own commit: the merge auto-rebases it.
    maw(root, &["ws", "create", "b", "--from", "main"]);
    let b = ws_path(root, "b");
    ws_a_clears_exec(root);
    std::fs::write(b.join("b.txt"), "b\n").expect("b edit");
    maw(root, &["exec", "b", "--", "git", "add", "-A"]);
    maw(root, &["exec", "b", "--", "git", "commit", "-m", "b work"]);

    let text = merge_a(root, None);
    let text = format!("{text}\n{}", maw(root, &["ws", "sync", "b"]));
    assert_eq!(
        committed_mode(&b, "HEAD", "mode_only.sh"),
        "100644",
        "{text}"
    );
    assert_exec_cleared(root, &b, &text);
    assert_eq!(
        git(&b, &["status", "--porcelain", "--untracked-files=no"]),
        "",
        "{text}"
    );
}

/// The snapshot-failed fallback (force checkout + repair from memory) keeps
/// the user's own mode changes and clears the merged exec bit.
#[cfg(feature = "failpoints")]
#[test]
fn snapshot_failed_fallback_settles_exec_bits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root);
    ws_a_clears_exec(root);
    set_exec(&root.join("notes.txt"), true);
    std::fs::write(root.join("notes.txt"), "notes\nuser\n").expect("dirty");
    std::fs::write(root.join("user.sh"), format!("{SEED}echo user\n")).expect("dirty");
    set_exec(&root.join("user.sh"), false);

    let text = merge_a(
        root,
        Some("FP_UPDATE_DEFAULT_BEFORE_SNAPSHOT=error:injected"),
    );
    assert!(
        text.contains("force checkout"),
        "fallback must run:\n{text}"
    );
    assert_exec_cleared(root, root, &text);
    assert!(
        is_exec(&root.join("notes.txt")),
        "user's +x must win:\n{text}"
    );
    assert!(
        !is_exec(&root.join("user.sh")),
        "user's -x must win:\n{text}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("user.sh")).expect("read"),
        format!("{SEED}echo user\n"),
        "{text}"
    );
    git_quiet(root, &["commit", "-a", "-m", "user commits"]);
    assert_eq!(committed_mode(root, "HEAD", "notes.txt"), "100755");
    assert_eq!(committed_mode(root, "HEAD", "user.sh"), "100644");
    assert_eq!(committed_mode(root, "HEAD", "mode_only.sh"), "100644");
}
