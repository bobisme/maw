//! bn-1eg2u: file <-> directory conflicts of the dirty-trunk replay (and of
//! the snapshot-failed fallback) get a ONE-STEP restore of the user's side.
//!
//! The merged side stays on disk and the user's side is pinned. Before
//! bn-1eg2u maw only printed `maw ws recover --ref <pin> --show <file>` for
//! them, which cannot put a directory (with its untracked files) back. Every
//! test here runs the printed `restore yours:` command verbatim and checks
//! the bytes. The merged side of a path under a merged file is also named
//! "replaced by file <p>", not "deleted".
//!
//! The fallback tests force the snapshot to fail with the
//! `FP_UPDATE_DEFAULT_BEFORE_SNAPSHOT` failpoint and need
//! `--features failpoints` (`just sg1-faithful-test`).

#![cfg(unix)]

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

fn init_repo(root: &Path, files: &[(&str, &str)]) {
    git_quiet(root, &["init", "-b", "main"]);
    git_quiet(root, &["config", "user.email", "test@example.com"]);
    git_quiet(root, &["config", "user.name", "Test"]);
    for (path, content) in files {
        let full = root.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("mkdir seed parent");
        }
        std::fs::write(full, content).expect("write seed file");
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

fn read_regular(path: &Path) -> String {
    let meta =
        std::fs::symlink_metadata(path).unwrap_or_else(|e| panic!("stat {}: {e}", path.display()));
    assert!(meta.is_file(), "{} must be a regular file", path.display());
    std::fs::read_to_string(path).expect("read")
}

fn assert_dir(path: &Path, context: &str) {
    let meta = std::fs::symlink_metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}\n{context}", path.display()));
    assert!(
        meta.is_dir(),
        "{} must be a directory\n{context}",
        path.display()
    );
}

/// The `restore yours:` command maw printed for `path`.
fn printed_restore(text: &str, path: &str) -> String {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("restore yours: "))
        .find(|c| c.ends_with(&format!(" {path}")))
        .unwrap_or_else(|| panic!("no restore command printed for {path}:\n{text}"))
        .to_owned()
}

/// Run the `restore yours:` command maw printed for `path`, verbatim, from
/// `root`; returns its output.
fn run_printed_restore(root: &Path, text: &str, path: &str) -> Output {
    let cmd = printed_restore(text, path);
    Command::new("sh")
        .arg("-c")
        .arg(cmd.replacen("maw ", &format!("{MAW} "), 1))
        .current_dir(root)
        .output()
        .expect("run restore")
}

fn run_printed_restore_ok(root: &Path, text: &str, path: &str) {
    let out = run_printed_restore(root, text, path);
    assert!(
        out.status.success(),
        "printed restore command `{}` failed:\n{}\n--- merge output ---\n{text}",
        printed_restore(text, path),
        combined(&out)
    );
}

/// `git status --porcelain --untracked-files=all`, minus maw's own files.
fn status(root: &Path) -> Vec<String> {
    let mut lines: Vec<String> = git(root, &["status", "--porcelain", "--untracked-files=all"])
        .lines()
        .filter(|l| !l[3..].starts_with(".maw"))
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

fn assert_merge_ok(out: &Output) -> String {
    let text = combined(out);
    assert!(out.status.success(), "merge failed:\n{text}");
    for bad in ["replay_snapshot failed", "replay failed"] {
        assert!(!text.contains(bad), "replay must not fail ({bad}):\n{text}");
    }
    text
}

// ---------------------------------------------------------------------------
// Live replay path (no failpoint).
// ---------------------------------------------------------------------------

/// The user replaced tracked file `p` with a directory holding UNTRACKED
/// files (one nested); the merge edited `p`. The printed command must bring
/// back the whole directory, replacing the merged file `p`.
#[test]
fn local_dir_over_merged_file_restores_in_one_step() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root, &[("p", "one\n"), ("other.txt", "other\n")]);
    ws_commit(root, |ws| {
        std::fs::write(ws.join("p"), "one\nmerged\n").expect("edit p");
    });
    std::fs::remove_file(root.join("p")).expect("rm p");
    std::fs::create_dir_all(root.join("p/sub")).expect("mkdir p/sub");
    std::fs::write(root.join("p/x"), "user x\n").expect("write p/x");
    std::fs::write(root.join("p/sub/y"), "user y\n").expect("write p/sub/y");
    std::fs::write(root.join("other.txt"), "other\nuser\n").expect("dirty other");

    let text = assert_merge_ok(&merge_a(root, None));
    assert_eq!(read_regular(&root.join("p")), "one\nmerged\n", "{text}");
    // bn-1eg2u item 2: the merged side of p/x is not "deleted".
    assert!(
        text.contains("merged (a): replaced by file p"),
        "the merged side of p/x must read 'replaced by file p':\n{text}"
    );
    assert!(!text.contains("merged (a): deleted"), "{text}");

    run_printed_restore_ok(root, &text, "p");
    assert_dir(&root.join("p"), &text);
    assert_eq!(read_regular(&root.join("p/x")), "user x\n", "{text}");
    assert_eq!(read_regular(&root.join("p/sub/y")), "user y\n", "{text}");
    assert_eq!(
        read_regular(&root.join("other.txt")),
        "other\nuser\n",
        "{text}"
    );
    assert_eq!(
        status(root),
        vec![" D p", " M other.txt", "?? p/sub/y", "?? p/x"],
        "the worktree must be exactly the user's pre-merge state on the merged commit\n{text}"
    );
}

/// The merge turned directory `d/` into file `d`; the user edited `d/x`
/// (and left `d/y` alone). Restoring must bring back the user's whole `d/`,
/// not only the edited file.
#[test]
fn merged_file_over_local_dir_restores_whole_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(
        root,
        &[("d/x", "x\n"), ("d/y", "y\n"), ("other.txt", "other\n")],
    );
    ws_commit(root, |ws| {
        std::fs::remove_dir_all(ws.join("d")).expect("rm d");
        std::fs::write(ws.join("d"), "merged file\n").expect("write d");
    });
    std::fs::write(root.join("d/x"), "x\nuser\n").expect("dirty edit");
    std::fs::write(root.join("d/new"), "untracked\n").expect("untracked in d");

    let text = assert_merge_ok(&merge_a(root, None));
    assert_eq!(read_regular(&root.join("d")), "merged file\n", "{text}");
    assert!(
        text.contains("merged (a): replaced by file d"),
        "the merged side of d/x must read 'replaced by file d':\n{text}"
    );
    assert!(!text.contains("merged (a): deleted"), "{text}");

    run_printed_restore_ok(root, &text, "d");
    assert_eq!(read_regular(&root.join("d/x")), "x\nuser\n", "{text}");
    assert_eq!(read_regular(&root.join("d/y")), "y\n", "{text}");
    assert_eq!(read_regular(&root.join("d/new")), "untracked\n", "{text}");
}

/// The merge turned file `p` into directory `p/`; the user edited `p`.
#[test]
fn local_file_over_merged_dir_restores_in_one_step() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root, &[("p", "one\ntwo\n"), ("other.txt", "other\n")]);
    ws_commit(root, |ws| {
        std::fs::remove_file(ws.join("p")).expect("rm p");
        std::fs::create_dir(ws.join("p")).expect("mkdir p");
        std::fs::write(ws.join("p/x"), "merged x\n").expect("write p/x");
    });
    std::fs::write(root.join("p"), "one\ntwo\nuser\n").expect("dirty edit");

    let text = assert_merge_ok(&merge_a(root, None));
    assert_eq!(read_regular(&root.join("p/x")), "merged x\n", "{text}");
    run_printed_restore_ok(root, &text, "p");
    assert_eq!(read_regular(&root.join("p")), "one\ntwo\nuser\n", "{text}");
}

/// The user replaced directory `d/` with file `d`; the merge edited `d/x`.
#[test]
fn local_file_over_merged_edit_inside_dir_restores_in_one_step() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root, &[("d/x", "x\n"), ("other.txt", "other\n")]);
    ws_commit(root, |ws| {
        std::fs::write(ws.join("d/x"), "x\nmerged\n").expect("edit d/x");
    });
    std::fs::remove_dir_all(root.join("d")).expect("rm d");
    std::fs::write(root.join("d"), "user file\n").expect("write d");

    let text = assert_merge_ok(&merge_a(root, None));
    assert_eq!(read_regular(&root.join("d/x")), "x\nmerged\n", "{text}");
    run_printed_restore_ok(root, &text, "d");
    assert_eq!(read_regular(&root.join("d")), "user file\n", "{text}");
}

/// Restoring never removes content that is not committed: an untracked
/// (e.g. build output) file inside the merged directory in the way, or an
/// edit to the merged file in the way, makes the command refuse and change
/// nothing.
#[test]
fn restore_refuses_to_remove_uncommitted_blockers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root, &[("p", "one\n"), (".gitignore", "*.o\n")]);
    ws_commit(root, |ws| {
        std::fs::remove_file(ws.join("p")).expect("rm p");
        std::fs::create_dir(ws.join("p")).expect("mkdir p");
        std::fs::write(ws.join("p/x"), "merged x\n").expect("write p/x");
    });
    std::fs::write(root.join("p"), "one\nuser\n").expect("dirty edit");

    let text = assert_merge_ok(&merge_a(root, None));
    // An ignored file appears in the merged directory after the merge.
    std::fs::write(root.join("p/build.o"), "object\n").expect("ignored file");
    let out = run_printed_restore(root, &text, "p");
    assert!(
        !out.status.success(),
        "must refuse to delete an ignored file:\n{}",
        combined(&out)
    );
    assert_eq!(read_regular(&root.join("p/build.o")), "object\n");
    assert_eq!(read_regular(&root.join("p/x")), "merged x\n");

    // An uncommitted edit to the merged file in the way.
    std::fs::remove_file(root.join("p/build.o")).expect("rm ignored");
    std::fs::write(root.join("p/x"), "merged x\nedited after\n").expect("edit merged");
    let out = run_printed_restore(root, &text, "p");
    assert!(
        !out.status.success(),
        "must refuse to delete an uncommitted edit:\n{}",
        combined(&out)
    );
    assert_eq!(read_regular(&root.join("p/x")), "merged x\nedited after\n");

    // Committed content only: the command works.
    git_quiet(root, &["checkout", "--", "p/x"]);
    run_printed_restore_ok(root, &text, "p");
    assert_eq!(read_regular(&root.join("p")), "one\nuser\n", "{text}");
}

/// A manual restore of a path under a file that has uncommitted edits must
/// refuse: removing that file would drop the edits.
#[test]
fn restore_refuses_to_remove_a_dirty_parent_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root, &[("d/x", "x\n"), ("other.txt", "other\n")]);
    ws_commit(root, |ws| {
        std::fs::remove_dir_all(ws.join("d")).expect("rm d");
        std::fs::write(ws.join("d"), "merged file\n").expect("write d");
    });
    std::fs::write(root.join("d/x"), "x\nuser\n").expect("dirty edit");

    let text = assert_merge_ok(&merge_a(root, None));
    let cmd = printed_restore(&text, "d");
    std::fs::write(root.join("d"), "merged file\nedited after\n").expect("edit d");
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("{}/x", cmd.replacen("maw ", &format!("{MAW} "), 1)))
        .current_dir(root)
        .output()
        .expect("run restore");
    assert!(
        !out.status.success(),
        "must refuse to replace an edited file:\n{}",
        combined(&out)
    );
    assert_eq!(read_regular(&root.join("d")), "merged file\nedited after\n");
}

/// Git status omits ignored leaf destinations inside a restored directory.
#[test]
fn restore_directory_refuses_ignored_destination() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root, &[("d/x", "snapshot x\n"), ("d/y", "snapshot y\n")]);
    let pin = "refs/manifold/recovery/default/ignored-destination";
    git_quiet(root, &["update-ref", pin, "HEAD"]);
    std::fs::remove_file(root.join("d/y")).expect("remove y");
    std::fs::write(root.join(".gitignore"), "d/y\n").expect("ignore y");
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-m", "stop tracking y"]);
    std::fs::write(root.join("d/y"), "only local copy\n").expect("ignored y");

    let out = maw_raw(
        root,
        &["ws", "recover", "--ref", pin, "--restore-file", "d"],
        None,
    );
    assert!(
        !out.status.success(),
        "must refuse ignored destination:\n{}",
        combined(&out)
    );
    assert_eq!(read_regular(&root.join("d/y")), "only local copy\n");
    assert_eq!(read_regular(&root.join("d/x")), "snapshot x\n");
    maw(
        root,
        &[
            "ws",
            "recover",
            "--ref",
            pin,
            "--restore-file",
            "d",
            "--force",
        ],
    );
    assert_eq!(read_regular(&root.join("d/y")), "snapshot y\n");
}

/// --force must still refuse an uncommitted symlink inside a blocker.
#[test]
fn restore_force_refuses_nested_uncommitted_symlink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root, &[("p", "snapshot file\n")]);
    let pin = "refs/manifold/recovery/default/nested-link";
    git_quiet(root, &["update-ref", pin, "HEAD"]);
    std::fs::remove_file(root.join("p")).expect("remove p");
    std::fs::create_dir_all(root.join("p/sub")).expect("mkdir");
    std::fs::write(root.join("p/sub/x"), "committed x\n").expect("write x");
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-m", "directory"]);
    std::os::unix::fs::symlink("unique target", root.join("p/sub/link")).expect("symlink");

    let out = maw_raw(
        root,
        &[
            "ws",
            "recover",
            "--ref",
            pin,
            "--restore-file",
            "p",
            "--force",
        ],
        None,
    );
    assert!(
        !out.status.success(),
        "must refuse nested symlink:\n{}",
        combined(&out)
    );
    assert_eq!(
        std::fs::read_link(root.join("p/sub/link")).expect("link survives"),
        Path::new("unique target")
    );
    assert_eq!(read_regular(&root.join("p/sub/x")), "committed x\n");
    // Committed symlinks can safely make way, including inside directories.
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-m", "commit link"]);
    maw(
        root,
        &[
            "ws",
            "recover",
            "--ref",
            pin,
            "--restore-file",
            "p",
            "--force",
        ],
    );
    assert_eq!(read_regular(&root.join("p")), "snapshot file\n");
}

/// A local deletion below a directory replaced by a merged symlink must
/// never unlink a file in the symlink's target outside the repository.
#[test]
fn replay_deletion_does_not_follow_merged_parent_symlink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("repo");
    let outside = dir.path().join("outside");
    std::fs::create_dir(&root).expect("mkdir repo");
    std::fs::create_dir(&outside).expect("mkdir outside");
    std::fs::write(outside.join("x"), "irreplaceable outside work\n").expect("outside x");
    init_repo(&root, &[("d/x", "base\n")]);
    ws_commit(&root, |ws| {
        std::fs::remove_dir_all(ws.join("d")).expect("remove directory");
        std::os::unix::fs::symlink(&outside, ws.join("d")).expect("merged symlink");
    });
    std::fs::remove_file(root.join("d/x")).expect("local deletion");

    let out = merge_a(&root, None);
    let text = combined(&out);
    assert!(out.status.success(), "{text}");
    assert_eq!(
        std::fs::read_to_string(outside.join("x")).ok().as_deref(),
        Some("irreplaceable outside work\n"),
        "replaying a deletion must not remove outside work:\n{text}"
    );
    assert_eq!(
        std::fs::read_link(root.join("d")).expect("merged link"),
        outside
    );
}

// ---------------------------------------------------------------------------
// Snapshot-failed fallback path.
// ---------------------------------------------------------------------------

#[cfg(feature = "failpoints")]
const FAIL_SNAPSHOT: &str = "FP_UPDATE_DEFAULT_BEFORE_SNAPSHOT=error:injected";

#[cfg(feature = "failpoints")]
fn merge_with_failed_snapshot(root: &Path) -> String {
    let out = merge_a(root, Some(FAIL_SNAPSHOT));
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert!(
        text.contains("snapshot_working_copy failed"),
        "the failpoint must force the fallback path:\n{text}"
    );
    text
}

/// The fallback's force checkout replaces the user's directory (with its
/// untracked files) by the merged file; the printed command brings it back.
#[cfg(feature = "failpoints")]
#[test]
fn fallback_local_dir_over_merged_file_restores_in_one_step() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root, &[("p", "one\n"), ("other.txt", "other\n")]);
    ws_commit(root, |ws| {
        std::fs::write(ws.join("p"), "one\nmerged\n").expect("edit p");
    });
    std::fs::remove_file(root.join("p")).expect("rm p");
    std::fs::create_dir_all(root.join("p/sub")).expect("mkdir p/sub");
    std::fs::write(root.join("p/x"), "user x\n").expect("write p/x");
    std::fs::write(root.join("p/sub/y"), "user y\n").expect("write p/sub/y");
    std::fs::write(root.join("other.txt"), "other\nuser\n").expect("dirty other");

    let text = merge_with_failed_snapshot(root);
    assert_eq!(read_regular(&root.join("p")), "one\nmerged\n", "{text}");
    assert!(
        text.contains(
            "    p\n      merged (a): regular file\n      yours (uncommitted): directory"
        ),
        "the user's side of p is a directory, not a deletion:\n{text}"
    );
    run_printed_restore_ok(root, &text, "p");
    assert_eq!(read_regular(&root.join("p/x")), "user x\n", "{text}");
    assert_eq!(read_regular(&root.join("p/sub/y")), "user y\n", "{text}");
    assert_eq!(
        read_regular(&root.join("other.txt")),
        "other\nuser\n",
        "{text}"
    );
}

/// Fallback, merge turned directory `d/` into file `d`, user edited `d/x`.
#[cfg(feature = "failpoints")]
#[test]
fn fallback_merged_file_over_local_dir_restores_whole_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(
        root,
        &[("d/x", "x\n"), ("d/y", "y\n"), ("other.txt", "other\n")],
    );
    ws_commit(root, |ws| {
        std::fs::remove_dir_all(ws.join("d")).expect("rm d");
        std::fs::write(ws.join("d"), "merged file\n").expect("write d");
    });
    std::fs::write(root.join("d/x"), "x\nuser\n").expect("dirty edit");
    std::fs::write(root.join("d/new"), "untracked\n").expect("untracked in d");

    let text = merge_with_failed_snapshot(root);
    assert_eq!(read_regular(&root.join("d")), "merged file\n", "{text}");
    run_printed_restore_ok(root, &text, "d");
    assert_eq!(read_regular(&root.join("d/x")), "x\nuser\n", "{text}");
    assert_eq!(read_regular(&root.join("d/y")), "y\n", "{text}");
    assert_eq!(read_regular(&root.join("d/new")), "untracked\n", "{text}");
}

/// A failed replay after recovery must report a directory swap as one
/// restore, not a deletion followed by commands invalidated by that deletion.
#[cfg(feature = "failpoints")]
#[test]
fn resumed_failed_replay_directory_commands_work_verbatim() {
    for local_is_dir in [true, false] {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        if local_is_dir {
            init_repo(root, &[("p", "base\n")]);
            ws_commit(root, |ws| {
                std::fs::write(ws.join("p"), "merged\n").expect("edit");
            });
            std::fs::remove_file(root.join("p")).expect("remove p");
            std::fs::create_dir(root.join("p")).expect("mkdir");
            std::fs::write(root.join("p/x"), "user x\n").expect("write x");
            std::fs::write(root.join("p/y"), "user y\n").expect("write y");
        } else {
            init_repo(root, &[("p/x", "base\n")]);
            ws_commit(root, |ws| {
                std::fs::write(ws.join("p/x"), "merged\n").expect("edit");
            });
            std::fs::remove_dir_all(root.join("p")).expect("remove p");
            std::fs::write(root.join("p"), "user file\n").expect("write p");
        }
        let crash = merge_a(root, Some("FP_CLEANUP_AFTER_DEFAULT_CHECKOUT=abort"));
        assert!(!crash.status.success(), "{}", combined(&crash));
        let out = maw_raw(
            root,
            &["ws", "merge", "--recover"],
            Some("FP_CLEANUP_REPLAY_BEFORE_APPLY=error:injected"),
        );
        let text = combined(&out);
        assert!(out.status.success(), "{text}");
        assert!(text.contains("replay_snapshot failed"), "{text}");
        let commands: Vec<_> = text
            .lines()
            .filter_map(|l| l.trim().strip_prefix("restore yours: "))
            .collect();
        assert!(!commands.is_empty(), "{text}");
        for cmd in commands {
            let out = Command::new("sh")
                .args(["-c", &cmd.replacen("maw ", &format!("{MAW} "), 1)])
                .current_dir(root)
                .output()
                .expect("run printed command");
            assert!(
                out.status.success(),
                "command {cmd} failed:\n{}\n{text}",
                combined(&out)
            );
        }
        if local_is_dir {
            assert_eq!(read_regular(&root.join("p/x")), "user x\n");
            assert_eq!(read_regular(&root.join("p/y")), "user y\n");
        } else {
            assert_eq!(read_regular(&root.join("p")), "user file\n");
        }
    }
}
