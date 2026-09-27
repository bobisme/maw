//! bn-3fcbu: the merge's dirty-trunk preserve/replay must not reset a
//! merged file-mode change.
//!
//! The target (repo root) has an uncommitted edit to `script.sh`; the merged
//! workspace committed `chmod +x script.sh`. The replay restored the user's
//! edit from the snapshot, whose tree records the mode the file had BEFORE
//! the merge (100644), so the file came back as 0644: the worktree silently
//! reverted the merged mode change, and the next trunk `git commit -a`
//! committed the revert. The replay now 3-way merges the executable bit
//! (base = the snapshot's parent, ours = the merged commit, theirs = the
//! snapshot): the user's own mode change wins, otherwise the merged mode.
//!
//! The crash-recovery test needs `--features failpoints`
//! (`just sg1-faithful-test`).

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

/// Seed files: `(path, content, executable)`.
fn init_repo(root: &Path, files: &[(&str, &str, bool)], links: &[(&str, &str)]) {
    git_quiet(root, &["init", "-b", "main"]);
    git_quiet(root, &["config", "user.email", "test@example.com"]);
    git_quiet(root, &["config", "user.name", "Test"]);
    for (path, content, exec) in files {
        let p = root.join(path);
        std::fs::write(&p, content).expect("write seed file");
        set_exec(&p, *exec);
    }
    for (link, target) in links {
        std::os::unix::fs::symlink(target, root.join(link)).expect("seed symlink");
    }
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-m", "seed"]);
    maw(root, &["init"]);
    git_quiet(root, &["add", "-A"]);
    if !git(root, &["status", "--porcelain"]).is_empty() {
        git_quiet(root, &["commit", "-m", "maw init"]);
        maw(root, &["epoch", "sync"]);
    }
}

/// Create workspace `a`, apply `edit` inside it, commit.
fn ws_commit(root: &Path, edit: impl FnOnce(&Path)) {
    maw(root, &["ws", "create", "a", "--from", "main"]);
    edit(&ws_path(root, "a"));
    maw(root, &["exec", "a", "--", "git", "add", "-A"]);
    maw(root, &["exec", "a", "--", "git", "commit", "-m", "a work"]);
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

fn committed_mode(root: &Path, rev: &str, path: &str) -> String {
    let line = git(root, &["ls-tree", rev, "--", path]);
    line.split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// Commit the trunk's dirty state the way a user would and return the
/// committed mode of `path`.
fn commit_trunk_and_mode(root: &Path, path: &str) -> String {
    git_quiet(root, &["commit", "-a", "-m", "user commits local edits"]);
    committed_mode(root, "HEAD", path)
}

const SEED: &str = "#!/bin/sh\necho one\necho two\necho three\n";

// ---------------------------------------------------------------------------
// Scenario 1: workspace commits ONLY `chmod +x`; the trunk has an uncommitted
// content edit. The merge leaves the committed content unchanged, so the
// replay takes the plain `stash_apply` path.
// ---------------------------------------------------------------------------

fn setup_mode_only(root: &Path) {
    init_repo(root, &[("script.sh", SEED, false)], &[]);
    ws_commit(root, |ws| set_exec(&ws.join("script.sh"), true));
    std::fs::write(root.join("script.sh"), format!("{SEED}echo user\n")).expect("dirty edit");
}

fn assert_mode_only_outcome(root: &Path, text: &str) {
    assert_eq!(
        committed_mode(root, "HEAD", "script.sh"),
        "100755",
        "{text}"
    );
    let script = root.join("script.sh");
    assert!(
        is_exec(&script),
        "the merged chmod +x must survive the dirty-trunk replay:\n{text}"
    );
    assert_eq!(
        std::fs::read_to_string(&script).expect("read"),
        format!("{SEED}echo user\n"),
        "the user's uncommitted edit must survive:\n{text}"
    );
    let diff = git(root, &["diff"]);
    assert!(
        !diff.contains("old mode"),
        "the worktree must not carry a mode revert:\n{diff}"
    );
    assert_eq!(commit_trunk_and_mode(root, "script.sh"), "100755");
}

#[test]
fn merged_chmod_survives_dirty_trunk_content_edit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup_mode_only(root);
    let out = merge_a(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert_mode_only_outcome(root, &text);
}

// ---------------------------------------------------------------------------
// Scenario 2: workspace commits `chmod +x` AND a content edit; the trunk has
// an uncommitted edit elsewhere in the same file. The committed content
// changed, so the replay takes the merge-protection (3-way) path.
// ---------------------------------------------------------------------------

#[test]
fn merged_chmod_and_content_survive_overlapping_dirty_edit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root, &[("script.sh", SEED, false)], &[]);
    ws_commit(root, |ws| {
        let p = ws.join("script.sh");
        std::fs::write(&p, SEED.replace("echo one", "echo ONE")).expect("ws edit");
        set_exec(&p, true);
    });
    std::fs::write(root.join("script.sh"), format!("{SEED}echo user\n")).expect("dirty edit");

    let out = merge_a(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    let script = root.join("script.sh");
    assert!(is_exec(&script), "merged +x must survive:\n{text}");
    assert_eq!(
        std::fs::read_to_string(&script).expect("read"),
        format!("{}echo user\n", SEED.replace("echo one", "echo ONE")),
        "{text}"
    );
    assert!(!text.contains("local-vs-merge conflict"), "{text}");
    assert_eq!(commit_trunk_and_mode(root, "script.sh"), "100755");
}

// ---------------------------------------------------------------------------
// Scenario 3: the user's OWN uncommitted mode change wins over the merged one.
// ---------------------------------------------------------------------------

#[test]
fn users_uncommitted_mode_change_wins() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(
        root,
        &[("up.sh", SEED, false), ("down.sh", SEED, true)],
        &[],
    );
    // The workspace edits both files' content, and flips both modes the
    // opposite way from the user.
    ws_commit(root, |ws| {
        for f in ["up.sh", "down.sh"] {
            let p = ws.join(f);
            std::fs::write(&p, SEED.replace("echo one", "echo ONE")).expect("ws edit");
        }
    });
    // User: chmod +x up.sh (mode only), chmod -x down.sh + content edit.
    set_exec(&root.join("up.sh"), true);
    std::fs::write(root.join("down.sh"), format!("{SEED}echo user\n")).expect("dirty edit");
    set_exec(&root.join("down.sh"), false);

    let out = merge_a(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert!(is_exec(&root.join("up.sh")), "user's +x must win:\n{text}");
    assert!(
        !is_exec(&root.join("down.sh")),
        "user's -x must win:\n{text}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("up.sh")).expect("read"),
        SEED.replace("echo one", "echo ONE"),
        "{text}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("down.sh")).expect("read"),
        format!("{}echo user\n", SEED.replace("echo one", "echo ONE")),
        "{text}"
    );
    git_quiet(root, &["commit", "-a", "-m", "user commits"]);
    assert_eq!(committed_mode(root, "HEAD", "up.sh"), "100755");
    assert_eq!(committed_mode(root, "HEAD", "down.sh"), "100644");
}

// ---------------------------------------------------------------------------
// Scenario 4: symlinks. The mode repair must never chmod through a symlink,
// and a dirty symlink must come back as a symlink.
// ---------------------------------------------------------------------------

#[test]
fn symlinks_are_not_followed_and_survive_replay() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(
        root,
        &[("script.sh", SEED, false), ("data.txt", "data\n", false)],
        &[("link", "script.sh")],
    );
    ws_commit(root, |ws| set_exec(&ws.join("script.sh"), true));
    // User: content edit to script.sh, and retargets `link` at data.txt.
    std::fs::write(root.join("script.sh"), format!("{SEED}echo user\n")).expect("dirty edit");
    std::fs::remove_file(root.join("link")).expect("rm link");
    std::os::unix::fs::symlink("data.txt", root.join("link")).expect("retarget");

    let out = merge_a(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    let link = root.join("link");
    let meta = std::fs::symlink_metadata(&link).expect("stat link");
    assert!(meta.file_type().is_symlink(), "link must stay a symlink");
    assert_eq!(
        std::fs::read_link(&link).expect("readlink"),
        PathBuf::from("data.txt")
    );
    assert!(
        !is_exec(&root.join("data.txt")),
        "the mode repair must not chmod through a symlink:\n{text}"
    );
    assert!(
        is_exec(&root.join("script.sh")),
        "merged +x must survive:\n{text}"
    );
    git_quiet(root, &["commit", "-a", "-m", "user commits"]);
    assert_eq!(committed_mode(root, "HEAD", "script.sh"), "100755");
    assert_eq!(committed_mode(root, "HEAD", "link"), "120000");
    assert_eq!(committed_mode(root, "HEAD", "data.txt"), "100644");
}

// ---------------------------------------------------------------------------
// Scenario 5: crash-recovered merge replays the same way.
// ---------------------------------------------------------------------------

#[cfg(feature = "failpoints")]
#[test]
fn recovered_merge_keeps_merged_chmod() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup_mode_only(root);
    let crash = merge_a(root, Some("FP_CLEANUP_BEFORE_DEFAULT_CHECKOUT=abort"));
    assert!(
        !crash.status.success(),
        "the injected abort must kill the merge:\n{}",
        combined(&crash)
    );
    assert!(root.join(".maw/manifold/merge-state.json").exists());
    let text = maw(root, &["ws", "merge", "--recover"]);
    assert!(text.contains("already landed"), "{text}");
    assert_mode_only_outcome(root, &text);
}
