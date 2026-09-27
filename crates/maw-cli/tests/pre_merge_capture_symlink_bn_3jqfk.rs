//! bn-3jqfk: the in-memory pre-merge capture of the dirty trunk
//! (`capture_pre_merge_dirty`) must record a symlink as a symlink, and the
//! dirty-trunk replay must treat a file <-> directory change as
//! conflict-as-data.
//!
//! Before the fix the capture used `is_file()`, which follows symlinks: a
//! dirty symlink was recorded as the *contents of its target* (and a dangling
//! one as a deletion). That capture feeds the fallback recovery pin
//! (`pin_pre_merge_recovery_ref`, used when the dirty-trunk snapshot fails)
//! and the post-replay fidelity repair, so the fallback path replaced the
//! user's symlink with a regular file holding the target's bytes, pinned a
//! regular file (or a deletion) in its place, and never told the user how to
//! get a merge-overwritten entry back.
//!
//! The fallback tests force the snapshot to fail with the
//! `FP_UPDATE_DEFAULT_BEFORE_SNAPSHOT` failpoint and need
//! `--features failpoints` (`just sg1-faithful-test`). They run the restore
//! command maw prints, verbatim.

#![cfg(unix)]

#[cfg(feature = "failpoints")]
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

/// Seed regular files `(path, content)` and symlinks `(link, target)`.
fn init_repo(root: &Path, files: &[(&str, &str)], links: &[(&str, &str)]) {
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

#[cfg(feature = "failpoints")]
fn symlink(target: &str, link: &Path) {
    if link.symlink_metadata().is_ok() {
        std::fs::remove_file(link).expect("rm existing");
    }
    std::os::unix::fs::symlink(target, link).expect("symlink");
}

#[cfg(feature = "failpoints")]
fn link_target(path: &Path) -> Option<PathBuf> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    meta.file_type()
        .is_symlink()
        .then(|| std::fs::read_link(path).expect("readlink"))
}

fn read_regular(path: &Path) -> String {
    let meta = std::fs::symlink_metadata(path).expect("stat");
    assert!(meta.is_file(), "{} must be a regular file", path.display());
    std::fs::read_to_string(path).expect("read")
}

/// Run the restore command maw printed for `path`, verbatim, from `root`.
#[cfg(feature = "failpoints")]
fn run_printed_restore(root: &Path, text: &str, path: &str) {
    let cmd = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("restore yours: "))
        .find(|c| c.ends_with(&format!(" {path}")))
        .unwrap_or_else(|| panic!("no restore command printed for {path}:\n{text}"));
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd.replacen("maw ", &format!("{MAW} "), 1))
        .current_dir(root)
        .output()
        .expect("run restore");
    assert!(
        out.status.success(),
        "printed restore command `{cmd}` failed:\n{}",
        combined(&out)
    );
}

/// The pinned `refs/manifold/recovery/default/*` refs, oldest first.
#[cfg(feature = "failpoints")]
fn recovery_refs(root: &Path) -> Vec<String> {
    let out = git(
        root,
        &[
            "for-each-ref",
            "--sort=refname",
            "--format=%(refname)",
            "refs/manifold/recovery/default/",
        ],
    );
    out.lines().map(str::to_owned).collect()
}

/// `(mode, content)` of `path` in the tree of `rev`, or `None` if absent.
#[cfg(feature = "failpoints")]
fn tree_entry(root: &Path, rev: &str, path: &str) -> Option<(String, String)> {
    let line = git(root, &["ls-tree", rev, "--", path]);
    if line.is_empty() {
        return None;
    }
    let mode = line.split_whitespace().next().expect("mode").to_owned();
    let content = git(root, &["cat-file", "-p", &format!("{rev}:{path}")]);
    Some((mode, content))
}

// ---------------------------------------------------------------------------
// Fallback path: the dirty-trunk snapshot fails, the merge force-checks-out
// the merged tree, pins the in-memory capture, and repairs from memory.
// ---------------------------------------------------------------------------

#[cfg(feature = "failpoints")]
const FAIL_SNAPSHOT: &str = "FP_UPDATE_DEFAULT_BEFORE_SNAPSHOT=error:injected";

/// Merge `a` with the snapshot forced to fail; returns the output text and
/// the single recovery ref the fallback pinned.
#[cfg(feature = "failpoints")]
fn merge_with_failed_snapshot(root: &Path) -> (String, String) {
    let before = recovery_refs(root);
    let out = merge_a(root, Some(FAIL_SNAPSHOT));
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert!(
        text.contains("snapshot_working_copy failed"),
        "the failpoint must force the fallback path:\n{text}"
    );
    let after = recovery_refs(root);
    let new: Vec<_> = after.iter().filter(|r| !before.contains(r)).collect();
    assert_eq!(new.len(), 1, "exactly one recovery pin: {after:?}\n{text}");
    (text, new[0].clone())
}

/// The user's retarget of a symlink the merge did not touch: the pin records
/// a symlink (mode 120000, blob = link text) and the link is back on disk.
/// Also: a new executable script is pinned with mode 100755.
#[cfg(feature = "failpoints")]
#[test]
fn fallback_keeps_local_symlink_retarget() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(
        root,
        &[("a.txt", "a\n"), ("c.txt", "c\n"), ("other.txt", "other\n")],
        &[("link", "a.txt")],
    );
    ws_commit(root, |ws| {
        std::fs::write(ws.join("other.txt"), "other\nmerged\n").expect("edit");
    });
    symlink("c.txt", &root.join("link"));
    let script = root.join("run.sh");
    std::fs::write(&script, "#!/bin/sh\necho hi\n").expect("write script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let (text, pin) = merge_with_failed_snapshot(root);

    assert_eq!(
        tree_entry(root, &pin, "link"),
        Some(("120000".to_owned(), "c.txt".to_owned())),
        "the pin must record the symlink, not its target's contents:\n{text}"
    );
    assert_eq!(
        tree_entry(root, &pin, "run.sh").map(|(m, _)| m),
        Some("100755".to_owned()),
        "the pin must keep the executable bit:\n{text}"
    );
    assert_eq!(
        link_target(&root.join("link")),
        Some(PathBuf::from("c.txt")),
        "the user's symlink must be back on disk:\n{text}"
    );
    assert_eq!(read_regular(&root.join("c.txt")), "c\n");
    assert_eq!(read_regular(&root.join("a.txt")), "a\n");
    assert_eq!(read_regular(&root.join("other.txt")), "other\nmerged\n");
}

/// A new dangling symlink: `is_file()` is false for it, so it used to be
/// pinned as a deletion (i.e. not at all).
#[cfg(feature = "failpoints")]
#[test]
fn fallback_pins_new_dangling_symlink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root, &[("other.txt", "other\n")], &[]);
    ws_commit(root, |ws| {
        std::fs::write(ws.join("other.txt"), "other\nmerged\n").expect("edit");
    });
    symlink("missing.txt", &root.join("dangling"));

    let (text, pin) = merge_with_failed_snapshot(root);

    assert_eq!(
        tree_entry(root, &pin, "dangling"),
        Some(("120000".to_owned(), "missing.txt".to_owned())),
        "the pin must record the dangling symlink:\n{text}"
    );
    assert_eq!(
        link_target(&root.join("dangling")),
        Some(PathBuf::from("missing.txt")),
        "{text}"
    );
}

/// A tracked regular file the user replaced with a symlink: the fallback
/// repair used to write the link target's bytes into a regular file.
#[cfg(feature = "failpoints")]
#[test]
fn fallback_keeps_local_file_to_symlink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(
        root,
        &[
            ("f.txt", "original\n"),
            ("target.txt", "target\n"),
            ("other.txt", "other\n"),
        ],
        &[],
    );
    ws_commit(root, |ws| {
        std::fs::write(ws.join("other.txt"), "other\nmerged\n").expect("edit");
    });
    symlink("target.txt", &root.join("f.txt"));

    let (text, pin) = merge_with_failed_snapshot(root);

    assert_eq!(
        tree_entry(root, &pin, "f.txt"),
        Some(("120000".to_owned(), "target.txt".to_owned())),
        "{text}"
    );
    assert_eq!(
        link_target(&root.join("f.txt")),
        Some(PathBuf::from("target.txt")),
        "the user's symlink must be back on disk, not a copy of its target:\n{text}"
    );
    assert_eq!(read_regular(&root.join("target.txt")), "target\n");
}

/// The merge and the user both retargeted the link. The fallback cannot
/// replay, so the merged link stays on disk and maw prints the command that
/// restores the user's link from the pin — which must restore a symlink.
#[cfg(feature = "failpoints")]
#[test]
fn fallback_prints_restore_for_merge_changed_symlink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(
        root,
        &[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")],
        &[("link", "a.txt")],
    );
    ws_commit(root, |ws| symlink("b.txt", &ws.join("link")));
    symlink("c.txt", &root.join("link"));

    let (text, pin) = merge_with_failed_snapshot(root);

    assert_eq!(
        tree_entry(root, &pin, "link"),
        Some(("120000".to_owned(), "c.txt".to_owned())),
        "{text}"
    );
    assert_eq!(
        link_target(&root.join("link")),
        Some(PathBuf::from("b.txt")),
        "the merged link is on disk:\n{text}"
    );
    assert!(text.contains("symlink -> c.txt"), "{text}");
    run_printed_restore(root, &text, "link");
    assert_eq!(
        link_target(&root.join("link")),
        Some(PathBuf::from("c.txt")),
        "the printed command must restore the user's symlink:\n{text}"
    );
    for (f, c) in [("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")] {
        assert_eq!(read_regular(&root.join(f)), c, "{f} untouched:\n{text}");
    }
}

// ---------------------------------------------------------------------------
// Directory type changes in the dirty-trunk replay (no failpoint needed).
// ---------------------------------------------------------------------------

/// The merge must succeed, must not report a failed replay, and must report
/// `path` as a type conflict naming the user's side.
fn assert_type_conflict_reported(text: &str, path: &str, local_desc: &str) {
    for bad in ["replay_snapshot failed", "refusing to", "replay failed"] {
        assert!(!text.contains(bad), "replay must not fail ({bad}):\n{text}");
    }
    assert!(
        text.contains("type conflict") && text.contains(path),
        "the path must be reported as a type conflict:\n{text}"
    );
    assert!(
        text.contains(local_desc),
        "the user's side ({local_desc}) must be named:\n{text}"
    );
}

/// Run the `inspect yours:` command maw printed for `path`, verbatim, from
/// `root`; returns its stdout.
fn run_printed_inspect(root: &Path, text: &str, path: &str) -> String {
    let cmd = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("inspect yours: "))
        .find(|c| c.ends_with(&format!(" {path}")))
        .unwrap_or_else(|| panic!("no inspect command printed for {path}:\n{text}"));
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd.replacen("maw ", &format!("{MAW} "), 1))
        .current_dir(root)
        .output()
        .expect("run inspect");
    assert!(
        out.status.success(),
        "printed inspect command `{cmd}` failed:\n{}",
        combined(&out)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The merge turned file `p` into directory `p/`; the user edited `p`.
/// Before bn-3jqfk `stash_apply` failed ("Is a directory") and the whole
/// replay aborted, so even the unrelated `other.txt` edit was not replayed.
#[test]
fn merged_file_to_dir_vs_local_edit_is_a_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root, &[("p", "one\ntwo\n"), ("other.txt", "other\n")], &[]);
    ws_commit(root, |ws| {
        std::fs::remove_file(ws.join("p")).expect("rm p");
        std::fs::create_dir(ws.join("p")).expect("mkdir p");
        std::fs::write(ws.join("p/x"), "merged x\n").expect("write p/x");
    });
    std::fs::write(root.join("p"), "one\ntwo\nuser\n").expect("dirty edit");
    std::fs::write(root.join("other.txt"), "other\nuser\n").expect("dirty other");

    let out = merge_a(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert_type_conflict_reported(&text, "p", "yours (uncommitted): regular file");
    assert!(text.contains("merged (a): directory"), "{text}");
    assert_eq!(read_regular(&root.join("p/x")), "merged x\n", "{text}");
    assert_eq!(
        read_regular(&root.join("other.txt")),
        "other\nuser\n",
        "non-overlapping edits are replayed:\n{text}"
    );
    assert_eq!(run_printed_inspect(root, &text, "p"), "one\ntwo\nuser\n");
}

/// The merge turned directory `d/` into file `d`; the user edited `d/x`.
#[test]
fn merged_dir_to_file_vs_local_edit_inside_is_a_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root, &[("d/x", "x\n"), ("other.txt", "other\n")], &[]);
    ws_commit(root, |ws| {
        std::fs::remove_dir_all(ws.join("d")).expect("rm d");
        std::fs::write(ws.join("d"), "merged file\n").expect("write d");
    });
    std::fs::write(root.join("d/x"), "x\nuser\n").expect("dirty edit");
    std::fs::write(root.join("other.txt"), "other\nuser\n").expect("dirty other");

    let out = merge_a(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert_type_conflict_reported(&text, "d/x", "yours (uncommitted): regular file");
    assert_eq!(read_regular(&root.join("d")), "merged file\n", "{text}");
    assert_eq!(
        read_regular(&root.join("other.txt")),
        "other\nuser\n",
        "{text}"
    );
    assert_eq!(run_printed_inspect(root, &text, "d/x"), "x\nuser\n");
}

/// The user turned file `p` into directory `p/`; the merge edited `p`.
/// Before bn-3jqfk the replay tried to write delete/modify markers into the
/// user's directory and aborted.
#[test]
fn local_file_to_dir_vs_merged_edit_is_a_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root, &[("p", "one\n"), ("other.txt", "other\n")], &[]);
    ws_commit(root, |ws| {
        std::fs::write(ws.join("p"), "one\nmerged\n").expect("edit p");
    });
    std::fs::remove_file(root.join("p")).expect("rm p");
    std::fs::create_dir(root.join("p")).expect("mkdir p");
    std::fs::write(root.join("p/x"), "user x\n").expect("write p/x");
    std::fs::write(root.join("other.txt"), "other\nuser\n").expect("dirty other");

    let out = merge_a(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert_type_conflict_reported(&text, "p/x", "yours (uncommitted): regular file");
    assert!(text.contains("yours (uncommitted): directory"), "{text}");
    assert_eq!(read_regular(&root.join("p")), "one\nmerged\n", "{text}");
    assert_eq!(
        read_regular(&root.join("other.txt")),
        "other\nuser\n",
        "{text}"
    );
    assert_eq!(run_printed_inspect(root, &text, "p/x"), "user x\n");
    assert!(
        !text.contains("Automatic repair FAILED"),
        "the fidelity repair must leave a reported conflict alone:\n{text}"
    );
}

/// The user turned directory `d/` into file `d`; the merge edited `d/x`.
#[test]
fn local_dir_to_file_vs_merged_edit_inside_is_a_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root, &[("d/x", "x\n"), ("other.txt", "other\n")], &[]);
    ws_commit(root, |ws| {
        std::fs::write(ws.join("d/x"), "x\nmerged\n").expect("edit d/x");
    });
    std::fs::remove_dir_all(root.join("d")).expect("rm d");
    std::fs::write(root.join("d"), "user file\n").expect("write d");
    std::fs::write(root.join("other.txt"), "other\nuser\n").expect("dirty other");

    let out = merge_a(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert_type_conflict_reported(&text, "d", "yours (uncommitted): regular file");
    assert!(text.contains("merged (a): directory"), "{text}");
    assert_eq!(read_regular(&root.join("d/x")), "x\nmerged\n", "{text}");
    assert_eq!(
        read_regular(&root.join("other.txt")),
        "other\nuser\n",
        "{text}"
    );
    assert_eq!(run_printed_inspect(root, &text, "d"), "user file\n");
    assert!(
        !text.contains("Automatic repair FAILED"),
        "the fidelity repair must leave a reported conflict alone:\n{text}"
    );
}

/// The user created an untracked file `n` where the merge added directory
/// `n/`: no merged path overlaps, but the plain replay could not write it.
#[test]
fn local_new_file_vs_merged_new_dir_is_a_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root, &[("other.txt", "other\n")], &[]);
    ws_commit(root, |ws| {
        std::fs::create_dir(ws.join("n")).expect("mkdir n");
        std::fs::write(ws.join("n/y"), "merged y\n").expect("write n/y");
    });
    std::fs::write(root.join("n"), "user n\n").expect("write n");
    std::fs::write(root.join("other.txt"), "other\nuser\n").expect("dirty other");

    let out = merge_a(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert_type_conflict_reported(&text, "n", "yours (uncommitted): regular file");
    assert_eq!(read_regular(&root.join("n/y")), "merged y\n", "{text}");
    assert_eq!(
        read_regular(&root.join("other.txt")),
        "other\nuser\n",
        "{text}"
    );
    assert_eq!(run_printed_inspect(root, &text, "n"), "user n\n");
}
