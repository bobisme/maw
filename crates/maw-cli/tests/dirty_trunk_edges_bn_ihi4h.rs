//! bn-ihi4h / bn-7xuvm: dirty-trunk edge semantics.
//!
//! 1. An uncommitted file <-> directory (or directory -> symlink) swap on a
//!    path the merge did NOT touch must survive in place, unreported — the
//!    Prime Invariant's rule for any uncommitted change to an untouched path.
//!    Before bn-ihi4h every such swap was displaced (merged side on disk,
//!    user's side only in the recovery pin) and reported as a
//!    `directory_change` conflict, because the collision check compared the
//!    user's entry with the merged tree instead of asking whether the merge
//!    changed that region.
//! 2. A merge that only flips the executable bit of a file the user replaced
//!    with a symlink is a type conflict (bn-2ygs0 rule): reported, with a
//!    restore command that works as printed.
//! 3. bn-7xuvm: when the dirty-trunk replay fails, the printed recovery
//!    commands must work verbatim (it used to print `git stash apply <oid>`,
//!    which fails for the single-parent snapshot / pin commit).

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
    let meta = std::fs::symlink_metadata(path).expect("stat");
    assert!(meta.is_file(), "{} must be a regular file", path.display());
    std::fs::read_to_string(path).expect("read")
}

fn link_target(path: &Path) -> Option<PathBuf> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    meta.file_type()
        .is_symlink()
        .then(|| std::fs::read_link(path).expect("readlink"))
}

fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// Run a printed command verbatim (with `maw` pointing at the test binary).
fn run_printed(root: &Path, cmd: &str) {
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd.replacen("maw ", &format!("{MAW} "), 1))
        .current_dir(root)
        .output()
        .expect("run printed command");
    assert!(
        out.status.success(),
        "printed command `{cmd}` failed:\n{}",
        combined(&out)
    );
}

/// Run the `restore yours:` command maw printed for `path`, verbatim.
fn run_printed_restore(root: &Path, text: &str, path: &str) {
    let cmd = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("restore yours: "))
        .find(|c| c.ends_with(&format!(" {path}")))
        .unwrap_or_else(|| panic!("no restore command printed for {path}:\n{text}"));
    run_printed(root, cmd);
}

/// The merge left the swapped region alone: no conflict, no failed replay,
/// no repair, no displacement.
fn assert_untouched_swap_unreported(text: &str) {
    for bad in [
        "type conflict",
        "directory_change",
        "replay_snapshot failed",
        "replay did not reproduce",
        "refusing to",
        "stash apply",
    ] {
        assert!(!text.contains(bad), "unexpected `{bad}`:\n{text}");
    }
}

/// `git status --porcelain` of the trunk, sorted, without maw's own files.
fn trunk_status(root: &Path) -> Vec<String> {
    let mut lines: Vec<String> = git(root, &["status", "--porcelain", "--untracked-files=all"])
        .lines()
        .filter(|l| !l.contains(".maw") && !l.contains(".gitignore"))
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

// ---------------------------------------------------------------------------
// Item 2: untouched swaps survive in place
// ---------------------------------------------------------------------------

/// The user turned file `p` into directory `p/`; the merge only touched
/// `other.txt`.
fn setup_untouched_file_to_dir(root: &Path) {
    init_repo(root, &[("p", "one\n"), ("other.txt", "other\n")]);
    ws_commit(root, |ws| {
        std::fs::write(ws.join("other.txt"), "other\nmerged\n").expect("edit other");
    });
    std::fs::remove_file(root.join("p")).expect("rm p");
    std::fs::create_dir(root.join("p")).expect("mkdir p");
    std::fs::write(root.join("p/x"), "user x\n").expect("write p/x");
}

fn assert_untouched_file_to_dir_in_place(root: &Path, text: &str) {
    assert!(
        is_real_dir(&root.join("p")),
        "p must stay a directory:\n{text}"
    );
    assert_eq!(read_regular(&root.join("p/x")), "user x\n", "{text}");
    assert_eq!(read_regular(&root.join("other.txt")), "other\nmerged\n");
    assert_eq!(
        trunk_status(root),
        vec![" D p".to_owned(), "?? p/x".to_owned()],
        "{text}"
    );
}

#[test]
fn untouched_local_file_to_dir_swap_survives_in_place() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup_untouched_file_to_dir(root);
    let out = merge_a(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert_untouched_swap_unreported(&text);
    assert_untouched_file_to_dir_in_place(root, &text);
}

/// The user turned directory `d/` into file `d`; the merge only touched
/// `other.txt`.
fn setup_untouched_dir_to_file(root: &Path) {
    init_repo(
        root,
        &[("d/x", "x\n"), ("d/sub/y", "y\n"), ("other.txt", "other\n")],
    );
    ws_commit(root, |ws| {
        std::fs::write(ws.join("other.txt"), "other\nmerged\n").expect("edit other");
    });
    std::fs::remove_dir_all(root.join("d")).expect("rm d");
    std::fs::write(root.join("d"), "user file\n").expect("write d");
}

fn assert_untouched_dir_to_file_in_place(root: &Path, text: &str) {
    assert_eq!(read_regular(&root.join("d")), "user file\n", "{text}");
    assert_eq!(read_regular(&root.join("other.txt")), "other\nmerged\n");
    assert_eq!(
        trunk_status(root),
        vec![
            " D d/sub/y".to_owned(),
            " D d/x".to_owned(),
            "?? d".to_owned()
        ],
        "{text}"
    );
}

#[test]
fn untouched_local_dir_to_file_swap_survives_in_place() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup_untouched_dir_to_file(root);
    let out = merge_a(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert_untouched_swap_unreported(&text);
    assert_untouched_dir_to_file_in_place(root, &text);
}

/// The bn-1dlkd shape: tracked directory `d/` replaced by a symlink to
/// another directory; the merge only touched `other.txt`.
#[test]
fn untouched_local_dir_to_symlink_swap_survives_in_place() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(
        root,
        &[("d/x", "x\n"), ("t/z", "z\n"), ("other.txt", "other\n")],
    );
    ws_commit(root, |ws| {
        std::fs::write(ws.join("other.txt"), "other\nmerged\n").expect("edit other");
    });
    std::fs::remove_dir_all(root.join("d")).expect("rm d");
    std::os::unix::fs::symlink("t", root.join("d")).expect("ln -s t d");

    let out = merge_a(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert_untouched_swap_unreported(&text);
    assert_eq!(
        link_target(&root.join("d")),
        Some(PathBuf::from("t")),
        "{text}"
    );
    assert_eq!(read_regular(&root.join("t/z")), "z\n", "{text}");
    assert_eq!(read_regular(&root.join("other.txt")), "other\nmerged\n");
}

/// The merge changed the region (added a file inside `d/`), so the user's
/// dir -> file swap still collides: displaced and reported, restorable.
#[test]
fn touched_local_dir_to_file_swap_is_still_a_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root, &[("d/x", "x\n"), ("other.txt", "other\n")]);
    ws_commit(root, |ws| {
        std::fs::write(ws.join("d/new"), "merged new\n").expect("add d/new");
    });
    std::fs::remove_dir_all(root.join("d")).expect("rm d");
    std::fs::write(root.join("d"), "user file\n").expect("write d");

    let out = merge_a(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert!(text.contains("type conflict"), "{text}");
    assert_eq!(read_regular(&root.join("d/new")), "merged new\n", "{text}");
    run_printed_restore(root, &text, "d");
    assert_eq!(read_regular(&root.join("d")), "user file\n", "{text}");
}

// ---------------------------------------------------------------------------
// Item 1: +x-only merge over a user symlink
// ---------------------------------------------------------------------------

#[test]
fn exec_bit_only_merge_over_local_symlink_is_a_type_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(
        root,
        &[("f", "#!/bin/sh\n"), ("t.txt", "t\n"), ("other.txt", "o\n")],
    );
    ws_commit(root, |ws| {
        std::fs::set_permissions(ws.join("f"), std::fs::Permissions::from_mode(0o755))
            .expect("chmod +x");
    });
    std::fs::remove_file(root.join("f")).expect("rm f");
    std::os::unix::fs::symlink("t.txt", root.join("f")).expect("ln -s");
    std::fs::write(root.join("other.txt"), "o\nuser\n").expect("dirty other");

    let out = merge_a(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert!(
        text.contains("type conflict") && text.contains("symlink -> t.txt"),
        "the +x-only merge over the user's symlink must be reported:\n{text}"
    );
    // bn-2ygs0 rule: the merged entry is on disk, the user's in the pin.
    assert_eq!(read_regular(&root.join("f")), "#!/bin/sh\n", "{text}");
    let mode = std::fs::metadata(root.join("f"))
        .expect("stat f")
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o111,
        0o111,
        "the merged +x must be on disk:\n{text}"
    );
    assert_eq!(read_regular(&root.join("other.txt")), "o\nuser\n", "{text}");
    run_printed_restore(root, &text, "f");
    assert_eq!(
        link_target(&root.join("f")),
        Some(PathBuf::from("t.txt")),
        "{text}"
    );
}

// ---------------------------------------------------------------------------
// Failpoint paths: the snapshot-failed fallback, crash resume, failed replay
// ---------------------------------------------------------------------------

#[cfg(feature = "failpoints")]
const FAIL_SNAPSHOT: &str = "FP_UPDATE_DEFAULT_BEFORE_SNAPSHOT=error:injected";

#[cfg(feature = "failpoints")]
#[test]
fn fallback_untouched_file_to_dir_swap_survives_in_place() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup_untouched_file_to_dir(root);
    let out = merge_a(root, Some(FAIL_SNAPSHOT));
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert!(text.contains("snapshot_working_copy failed"), "{text}");
    assert_untouched_swap_unreported(&text);
    assert_untouched_file_to_dir_in_place(root, &text);
}

#[cfg(feature = "failpoints")]
#[test]
fn fallback_untouched_dir_to_file_swap_survives_in_place() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup_untouched_dir_to_file(root);
    let out = merge_a(root, Some(FAIL_SNAPSHOT));
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert!(text.contains("snapshot_working_copy failed"), "{text}");
    assert_untouched_swap_unreported(&text);
    assert_untouched_dir_to_file_in_place(root, &text);
}

#[cfg(feature = "failpoints")]
#[test]
fn resumed_untouched_swaps_survive_in_place() {
    for (setup, check) in [
        (
            setup_untouched_file_to_dir as fn(&Path),
            assert_untouched_file_to_dir_in_place as fn(&Path, &str),
        ),
        (
            setup_untouched_dir_to_file,
            assert_untouched_dir_to_file_in_place,
        ),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        setup(root);
        let crash = merge_a(root, Some("FP_CLEANUP_AFTER_DEFAULT_CHECKOUT=abort"));
        assert!(!crash.status.success(), "{}", combined(&crash));
        let text = maw(root, &["ws", "merge", "--recover"]);
        assert_untouched_swap_unreported(&text);
        check(root, &text);
    }
}

/// bn-7xuvm: dirty trunk where the merge and the user both edited
/// `shared.txt` (different hunks), plus untouched user edits.
#[cfg(feature = "failpoints")]
fn setup_replay_failure(root: &Path) {
    init_repo(
        root,
        &[
            ("f.txt", "base\n"),
            ("gone.txt", "tracked\n"),
            ("shared.txt", "1\n2\n3\n4\n5\n6\n7\n8\n9\n"),
        ],
    );
    ws_commit(root, |ws| {
        std::fs::write(ws.join("shared.txt"), "1-a\n2\n3\n4\n5\n6\n7\n8\n9\n").expect("edit");
    });
    std::fs::write(root.join("f.txt"), "user edit\n").expect("dirty f");
    std::fs::write(root.join("new.txt"), "untracked user file\n").expect("untracked");
    std::fs::remove_file(root.join("gone.txt")).expect("rm gone");
    std::fs::write(root.join("shared.txt"), "1\n2\n3\n4\n5\n6\n7\n8\n9-user\n")
        .expect("dirty shared");
}

#[cfg(feature = "failpoints")]
const FAIL_REPLAY: &str = "FP_CLEANUP_REPLAY_BEFORE_APPLY=error:injected";

/// Live merge whose replay fails: the printed commands restore the user's
/// side of the merge-changed path, verbatim.
#[cfg(feature = "failpoints")]
#[test]
fn live_replay_failure_prints_working_restore_commands() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup_replay_failure(root);
    let out = merge_a(root, Some(FAIL_REPLAY));
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert!(text.contains("replay_snapshot failed"), "{text}");
    assert!(
        !text.contains("stash apply"),
        "no broken stash hint:\n{text}"
    );
    assert_eq!(
        read_regular(&root.join("shared.txt")),
        "1-a\n2\n3\n4\n5\n6\n7\n8\n9\n",
        "the merged version is on disk until restored:\n{text}"
    );
    run_printed_restore(root, &text, "shared.txt");
    assert_eq!(
        read_regular(&root.join("shared.txt")),
        "1\n2\n3\n4\n5\n6\n7\n8\n9-user\n",
        "{text}"
    );
    // The untouched edits were repaired from memory.
    assert_eq!(read_regular(&root.join("f.txt")), "user edit\n", "{text}");
    assert_eq!(read_regular(&root.join("new.txt")), "untracked user file\n");
    assert!(!root.join("gone.txt").exists(), "{text}");
}

/// Resumed update whose replay fails: there is no in-memory capture, so
/// every path of the pin not on disk as pinned gets a command, and each one
/// works verbatim.
#[cfg(feature = "failpoints")]
#[test]
fn resumed_replay_failure_prints_working_restore_commands() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup_replay_failure(root);
    let crash = merge_a(root, Some("FP_CLEANUP_AFTER_DEFAULT_CHECKOUT=abort"));
    assert!(!crash.status.success(), "{}", combined(&crash));
    let out = maw_raw(root, &["ws", "merge", "--recover"], Some(FAIL_REPLAY));
    let text = combined(&out);
    assert!(out.status.success(), "recover failed:\n{text}");
    assert!(text.contains("replay_snapshot failed"), "{text}");
    assert!(
        !text.contains("stash apply"),
        "no broken stash hint:\n{text}"
    );
    for path in ["shared.txt", "f.txt", "new.txt", "gone.txt"] {
        run_printed_restore(root, &text, path);
    }
    assert_eq!(
        read_regular(&root.join("shared.txt")),
        "1\n2\n3\n4\n5\n6\n7\n8\n9-user\n",
        "{text}"
    );
    assert_eq!(read_regular(&root.join("f.txt")), "user edit\n", "{text}");
    assert_eq!(read_regular(&root.join("new.txt")), "untracked user file\n");
    assert!(!root.join("gone.txt").exists(), "{text}");
}

/// The replay itself fails: the untouched swap is still the user's alone,
/// so the post-replay fidelity repair puts it back from the in-memory
/// capture (it used to leave it to the collision report).
#[cfg(feature = "failpoints")]
#[test]
fn replay_failure_repairs_untouched_swaps_from_memory() {
    for (setup, check) in [
        (
            setup_untouched_file_to_dir as fn(&Path),
            assert_untouched_file_to_dir_in_place as fn(&Path, &str),
        ),
        (
            setup_untouched_dir_to_file,
            assert_untouched_dir_to_file_in_place,
        ),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        setup(root);
        let out = merge_a(root, Some(FAIL_REPLAY));
        let text = combined(&out);
        assert!(out.status.success(), "merge failed:\n{text}");
        assert!(text.contains("replay_snapshot failed"), "{text}");
        assert!(!text.contains("Automatic repair FAILED"), "{text}");
        check(root, &text);
    }
}
