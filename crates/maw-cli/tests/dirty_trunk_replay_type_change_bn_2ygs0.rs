//! bn-2ygs0: the merge's dirty-trunk replay must treat a symlink/file type
//! change (or a symlink retarget) that collides with an uncommitted trunk
//! change as conflict-as-data, not abort.
//!
//! Before the fix, a merged workspace that turned `link` into a regular file
//! while the trunk had an uncommitted retarget of `link` made the replay
//! abort with "refusing to write conflict markers ... is a symlink"; the
//! other directions were worse — the replay 3-way merged the bytes of the
//! symlink's *target* file, or silently let the user's retarget replace the
//! merged one with no conflict reported. A symlink and a file (or two
//! symlink targets) cannot be merged into one path, so the replay now keeps
//! the merged side on disk, reports the path as a type conflict naming both
//! sides, and prints a `maw ws recover ... --restore-file` command that puts
//! the user's uncommitted side back from the pinned recovery snapshot. The
//! tests run that printed command verbatim.
//!
//! The crash-recovery test needs `--features failpoints`
//! (`just sg1-faithful-test`).

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

/// Seed regular files `(path, content)` and symlinks `(link, target)`.
fn init_repo(root: &Path, files: &[(&str, &str)], links: &[(&str, &str)]) {
    git_quiet(root, &["init", "-b", "main"]);
    git_quiet(root, &["config", "user.email", "test@example.com"]);
    git_quiet(root, &["config", "user.name", "Test"]);
    for (path, content) in files {
        std::fs::write(root.join(path), content).expect("write seed file");
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

fn symlink(target: &str, link: &Path) {
    if link.symlink_metadata().is_ok() {
        std::fs::remove_file(link).expect("rm existing");
    }
    std::os::unix::fs::symlink(target, link).expect("symlink");
}

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

/// The merge must succeed, must not report a failed replay, and must report
/// `path` as a type conflict.
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

/// Run the restore command maw printed for `path`, verbatim, from `root`.
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

/// The worktree path matches HEAD (the merged side), i.e. nothing half-way.
fn assert_path_clean(root: &Path, path: &str, text: &str) {
    let st = git(root, &["status", "--porcelain", "--", path]);
    assert!(
        st.is_empty(),
        "{path} must match the merged commit, got {st:?}:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// Direction 1 (the reported bug): merged symlink -> regular file vs the
// trunk's uncommitted retarget of the symlink.
// ---------------------------------------------------------------------------

fn setup_symlink_to_file(root: &Path) {
    init_repo(
        root,
        &[("target.txt", "target\n"), ("other.txt", "other\n")],
        &[("link", "target.txt")],
    );
    ws_commit(root, |ws| {
        std::fs::remove_file(ws.join("link")).expect("rm link");
        std::fs::write(ws.join("link"), "merged file\n").expect("write file");
    });
    symlink("other.txt", &root.join("link"));
}

fn assert_symlink_to_file_outcome(root: &Path, text: &str) {
    assert_type_conflict_reported(text, "link", "symlink -> other.txt");
    assert_eq!(read_regular(&root.join("link")), "merged file\n", "{text}");
    assert_path_clean(root, "link", text);
    assert_eq!(read_regular(&root.join("target.txt")), "target\n");
    assert_eq!(read_regular(&root.join("other.txt")), "other\n");
    run_printed_restore(root, text, "link");
    assert_eq!(
        link_target(&root.join("link")),
        Some(PathBuf::from("other.txt")),
        "restore must bring back the user's symlink:\n{text}"
    );
}

#[test]
fn merged_symlink_to_file_vs_local_retarget_is_a_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup_symlink_to_file(root);
    let out = merge_a(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert_symlink_to_file_outcome(root, &text);
}

// ---------------------------------------------------------------------------
// Direction 2: merged regular file -> symlink vs the trunk's uncommitted
// content edit. Must never write through the merged symlink into its target.
// ---------------------------------------------------------------------------

#[test]
fn merged_file_to_symlink_vs_local_edit_is_a_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(
        root,
        &[
            ("f.txt", "one\ntwo\nthree\n"),
            ("target.txt", "one\ntwo\nthree\n"),
        ],
        &[],
    );
    ws_commit(root, |ws| symlink("target.txt", &ws.join("f.txt")));
    std::fs::write(root.join("f.txt"), "one\ntwo\nthree\nuser\n").expect("dirty edit");

    let out = merge_a(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert_type_conflict_reported(&text, "f.txt", "regular file");
    assert_eq!(
        link_target(&root.join("f.txt")),
        Some(PathBuf::from("target.txt")),
        "the merged symlink must be on disk:\n{text}"
    );
    assert_eq!(
        read_regular(&root.join("target.txt")),
        "one\ntwo\nthree\n",
        "the replay must not write through the merged symlink:\n{text}"
    );
    assert_path_clean(root, "f.txt", &text);
    run_printed_restore(root, &text, "f.txt");
    assert_eq!(
        read_regular(&root.join("f.txt")),
        "one\ntwo\nthree\nuser\n",
        "restore must bring back the user's edit as a regular file:\n{text}"
    );
    assert_eq!(read_regular(&root.join("target.txt")), "one\ntwo\nthree\n");
}

// ---------------------------------------------------------------------------
// Direction 3: merged retarget vs the trunk's uncommitted retarget — both an
// existing merged target and a dangling one (which used to be silently
// overwritten by the user's retarget with no conflict reported).
// ---------------------------------------------------------------------------

fn retarget_vs_retarget(merged_target: &str) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(
        root,
        &[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")],
        &[("link", "a.txt")],
    );
    ws_commit(root, |ws| symlink(merged_target, &ws.join("link")));
    symlink("c.txt", &root.join("link"));

    let out = merge_a(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert_type_conflict_reported(&text, "link", "symlink -> c.txt");
    assert_eq!(
        link_target(&root.join("link")),
        Some(PathBuf::from(merged_target)),
        "the merged retarget must be on disk:\n{text}"
    );
    assert_path_clean(root, "link", &text);
    for (f, c) in [("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")] {
        assert_eq!(
            read_regular(&root.join(f)),
            c,
            "{f} must be untouched:\n{text}"
        );
    }
    run_printed_restore(root, &text, "link");
    assert_eq!(
        link_target(&root.join("link")),
        Some(PathBuf::from("c.txt")),
        "restore must bring back the user's retarget:\n{text}"
    );
}

#[test]
fn merged_retarget_vs_local_retarget_is_a_conflict() {
    retarget_vs_retarget("b.txt");
}

#[test]
fn merged_dangling_retarget_vs_local_retarget_is_a_conflict() {
    retarget_vs_retarget("missing.txt");
}

/// The merged regular file holds exactly the bytes of the old symlink target,
/// so a bytes-only "did the merge change this path?" check misses the type
/// change and the user's retarget used to silently revert it on disk.
#[test]
fn merged_symlink_to_same_bytes_file_vs_local_retarget_is_a_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(
        root,
        &[("target.txt", "target\n"), ("other.txt", "other\n")],
        &[("link", "target.txt")],
    );
    ws_commit(root, |ws| {
        std::fs::remove_file(ws.join("link")).expect("rm link");
        std::fs::write(ws.join("link"), "target.txt").expect("write file");
    });
    symlink("other.txt", &root.join("link"));

    let out = merge_a(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert_type_conflict_reported(&text, "link", "symlink -> other.txt");
    assert_eq!(read_regular(&root.join("link")), "target.txt", "{text}");
    assert_path_clean(root, "link", &text);
    run_printed_restore(root, &text, "link");
    assert_eq!(
        link_target(&root.join("link")),
        Some(PathBuf::from("other.txt")),
        "{text}"
    );
}

/// The merge deleted the symlink, the trunk retargeted it: the user's
/// symlink stays on disk (as a user-edited regular file would), and the
/// conflict is still reported with the command that takes the deletion.
#[test]
fn merged_symlink_delete_vs_local_retarget_keeps_local_and_reports() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(
        root,
        &[("target.txt", "target\n"), ("other.txt", "other\n")],
        &[("link", "target.txt")],
    );
    ws_commit(root, |ws| {
        std::fs::remove_file(ws.join("link")).expect("rm link");
    });
    symlink("other.txt", &root.join("link"));

    let out = merge_a(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert_type_conflict_reported(&text, "link", "symlink -> other.txt");
    assert!(text.contains("take the merge's deletion"), "{text}");
    assert_eq!(
        link_target(&root.join("link")),
        Some(PathBuf::from("other.txt")),
        "{text}"
    );
    assert_eq!(read_regular(&root.join("other.txt")), "other\n");
}

/// Both sides made the same retarget: nothing to report.
#[test]
fn identical_retarget_is_clean() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(
        root,
        &[("a.txt", "a\n"), ("b.txt", "b\n")],
        &[("link", "a.txt")],
    );
    ws_commit(root, |ws| symlink("b.txt", &ws.join("link")));
    symlink("b.txt", &root.join("link"));
    std::fs::write(root.join("a.txt"), "a\nuser\n").expect("dirty edit");

    let out = merge_a(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert!(!text.contains("type conflict"), "{text}");
    assert!(!text.contains("local-vs-merge"), "{text}");
    assert_eq!(
        link_target(&root.join("link")),
        Some(PathBuf::from("b.txt"))
    );
    assert_eq!(read_regular(&root.join("a.txt")), "a\nuser\n");
}

// ---------------------------------------------------------------------------
// Crash-recovered merge replays the same way.
// ---------------------------------------------------------------------------

#[cfg(feature = "failpoints")]
#[test]
fn recovered_merge_reports_type_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup_symlink_to_file(root);
    let crash = merge_a(root, Some("FP_CLEANUP_BEFORE_DEFAULT_CHECKOUT=abort"));
    assert!(
        !crash.status.success(),
        "the injected abort must kill the merge:\n{}",
        combined(&crash)
    );
    assert!(root.join(".maw/manifold/merge-state.json").exists());
    let text = maw(root, &["ws", "merge", "--recover"]);
    assert!(text.contains("already landed"), "{text}");
    assert_symlink_to_file_outcome(root, &text);
}
