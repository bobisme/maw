//! bn-1dlkd: a tracked trunk directory replaced by a symlink must never make
//! a merge silently wipe the dirty trunk.
//!
//! gix's status refuses to lstat through a symlinked leading path component
//! and aborts the whole status run ("IO error while writing blob or reading
//! file metadata or changing filetype"). Before the fix both the dirty-trunk
//! snapshot and the in-memory fallback capture depended on that status, so a
//! merge of an UNRELATED workspace captured nothing, force-checked-out the
//! merged tree over every uncommitted trunk edit, pinned nothing, and printed
//! `[OK]`.
//!
//! Now: status falls back to git (which handles the case), the capture never
//! reads through a symlinked parent, and when the dirty state still cannot be
//! completely captured the update fails CLOSED — the worktree is left alone
//! and the merge journal stays for `maw ws merge --recover`. The fail-closed
//! tests need `--features failpoints`.

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

/// Trunk with tracked `d/inner.txt` and `other.txt`; workspace `wa` commits
/// an unrelated `new.txt`.
fn setup(root: &Path) {
    git_quiet(root, &["init", "-b", "main"]);
    git_quiet(root, &["config", "user.email", "test@example.com"]);
    git_quiet(root, &["config", "user.name", "Test"]);
    std::fs::create_dir(root.join("d")).expect("mkdir d");
    std::fs::write(root.join("d/inner.txt"), "inner\n").expect("write");
    std::fs::write(root.join("other.txt"), "other\n").expect("write");
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-m", "seed"]);
    maw(root, &["init"]);
    git_quiet(root, &["add", "-A"]);
    if !git(root, &["status", "--porcelain"]).is_empty() {
        git_quiet(root, &["commit", "-m", "maw init"]);
        maw(root, &["epoch", "sync"]);
    }
    maw(root, &["ws", "create", "wa", "--from", "main"]);
    let ws = root.join(".maw/workspaces/wa");
    std::fs::write(ws.join("new.txt"), "new\n").expect("write new");
    maw(root, &["exec", "wa", "--", "git", "add", "-A"]);
    maw(
        root,
        &["exec", "wa", "--", "git", "commit", "-m", "wa work"],
    );
}

/// The uncommitted trunk edits of the repro: append to `other.txt`, replace
/// the tracked directory `d` with a symlink to `target`.
fn dirty_trunk(root: &Path, target: &str) {
    std::fs::write(root.join("other.txt"), "other\nedit\n").expect("edit other");
    std::fs::remove_file(root.join("d/inner.txt")).expect("rm inner");
    std::fs::remove_dir(root.join("d")).expect("rmdir d");
    std::os::unix::fs::symlink(target, root.join("d")).expect("symlink d");
}

fn merge_wa(root: &Path, fp: Option<&str>) -> Output {
    maw_raw(
        root,
        &[
            "ws",
            "merge",
            "wa",
            "--into",
            "default",
            "--message",
            "merge wa",
        ],
        fp,
    )
}

fn recovery_refs(root: &Path) -> Vec<String> {
    git(
        root,
        &[
            "for-each-ref",
            "--sort=refname",
            "--format=%(refname)",
            "refs/manifold/recovery/default/",
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect()
}

/// `(mode, content)` of `path` in the tree of `rev`, or `None` if absent.
fn tree_entry(root: &Path, rev: &str, path: &str) -> Option<(String, String)> {
    let line = git(root, &["ls-tree", rev, "--", path]);
    if line.is_empty() {
        return None;
    }
    let mode = line.split_whitespace().next().expect("mode").to_owned();
    let content = git(root, &["cat-file", "-p", &format!("{rev}:{path}")]);
    Some((mode, content))
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn link_target(path: &Path) -> Option<PathBuf> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    meta.file_type()
        .is_symlink()
        .then(|| std::fs::read_link(path).expect("readlink"))
}

/// Shared assertions: the edits survive (on disk or pinned) and the user's
/// symlink is recoverable with the command maw printed.
fn assert_preserved(root: &Path, text: &str) -> String {
    assert!(
        !text.contains("Falling back to force checkout"),
        "the merge must not force-checkout over an uncaptured trunk:\n{text}"
    );
    assert_eq!(
        read(&root.join("other.txt")),
        "other\nedit\n",
        "the unrelated uncommitted edit must survive on disk:\n{text}"
    );
    assert_eq!(read(&root.join("new.txt")), "new\n", "merged file:\n{text}");
    let refs = recovery_refs(root);
    assert_eq!(refs.len(), 1, "exactly one recovery pin: {refs:?}\n{text}");
    let pin = refs[0].clone();
    assert_eq!(
        tree_entry(root, &pin, "other.txt").map(|(_, c)| c),
        Some("other\nedit".to_owned()),
        "the pin must hold the other.txt edit:\n{text}"
    );
    assert_eq!(
        tree_entry(root, &pin, "d").map(|(m, _)| m),
        Some("120000".to_owned()),
        "the pin must record d as the user's symlink:\n{text}"
    );
    assert_eq!(
        tree_entry(root, &pin, "d/inner.txt"),
        None,
        "d/inner.txt is deleted on the user's side (never read through the link):\n{text}"
    );
    // The type change is reported, with the one-step command that puts the
    // user's side back (bn-1eg2u; it was `--show d` before).
    let restore = format!("maw ws recover --ref {pin} --restore-file d");
    assert!(
        text.contains(&format!("restore yours: {restore}")),
        "the type change must be reported with `{restore}`:\n{text}"
    );
    // The on-disk state is coherent: either side of d, never an empty dir.
    match link_target(&root.join("d")) {
        Some(_) => {}
        None => assert_eq!(
            read(&root.join("d/inner.txt")),
            "inner\n",
            "d on disk must be the merged directory, complete:\n{text}"
        ),
    }
    // Run the printed command verbatim: d is the user's symlink again.
    let out = Command::new("sh")
        .current_dir(root)
        .args(["-c", &restore.replacen("maw ", &format!("{MAW} "), 1)])
        .output()
        .expect("run printed restore");
    assert!(
        out.status.success(),
        "printed command failed: {restore}\n{}\n---\n{text}",
        combined(&out)
    );
    assert_eq!(
        link_target(&root.join("d")),
        Some(PathBuf::from("x")),
        "the printed command must put the user's symlink back:\n{text}"
    );
    pin
}

/// The field repro (dangling symlink): the merge succeeds, the unrelated edit
/// survives, the symlink is pinned and reported.
#[test]
fn dir_replaced_by_dangling_symlink_keeps_trunk_edits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup(root);
    dirty_trunk(root, "x");

    let out = merge_wa(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert_preserved(root, &text);
}

/// A status API with UTF-8 paths must refuse an unrepresentable dirty name,
/// rather than silently omit it from the snapshot and clean it away.
#[test]
fn non_utf8_untracked_file_refuses_merge_cleanup() {
    use std::os::unix::ffi::OsStrExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup(root);
    let path = root.join(std::ffi::OsStr::from_bytes(b"notes-\xff.txt"));
    std::fs::write(&path, "irreplaceable notes\n").expect("write notes");
    std::fs::write(root.join("other.txt"), "user edit\n").expect("dirty tracked file");

    let out = merge_wa(root, None);
    let text = combined(&out);
    assert_eq!(
        std::fs::read(&path).ok(),
        Some(b"irreplaceable notes\n".to_vec()),
        "non-UTF-8 user file was lost:\n{text}"
    );
    assert!(
        !out.status.success(),
        "incomplete capture must refuse:\n{text}"
    );
    assert!(
        text.contains("non-UTF-8"),
        "cause must be actionable:\n{text}"
    );
    assert!(root.join(".maw/manifold/merge-state.json").exists());

    // Once the user gives the file a supported name, the kept journal can
    // finish the update and preserve both edits.
    std::fs::rename(&path, root.join("notes.txt")).expect("rename notes");
    maw(root, &["ws", "merge", "--recover"]);
    assert_eq!(read(&root.join("notes.txt")), "irreplaceable notes\n");
    assert_eq!(read(&root.join("other.txt")), "user edit\n");
    assert_eq!(read(&root.join("new.txt")), "new\n");
}

/// Fidelity repair must print a command that accepts the path as one shell
/// argument, including spaces and shell metacharacters.
#[cfg(feature = "failpoints")]
#[test]
fn fallback_fidelity_restore_command_quotes_filename() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup(root);
    let filename = "other notes 'draft'.txt";
    // Make this tracked on both sides so the force checkout erases the edit
    // and the fidelity repair must put it back and print its recovery command.
    git_quiet(root, &["mv", "other.txt", filename]);
    git_quiet(root, &["commit", "-am", "rename notes"]);
    maw(root, &["epoch", "sync"]);
    maw(root, &["ws", "sync", "wa"]);
    std::fs::write(root.join(filename), "user notes\n").expect("edit notes");
    // bn-2ds48: the fallback now replays the pin; fail that replay so the
    // fidelity repair (and its restore command) is what puts the edit back.
    let out = merge_wa(
        root,
        Some(
            "FP_UPDATE_DEFAULT_BEFORE_SNAPSHOT=error:injected;\
             FP_CLEANUP_REPLAY_BEFORE_APPLY=error:injected",
        ),
    );
    let text = combined(&out);
    assert!(out.status.success(), "{text}");
    assert_eq!(read(&root.join(filename)), "user notes\n", "{text}");
    let command = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("Restore:  "))
        .expect("fidelity repair must print a restore command");
    // Put the merged version back so the destination is clean. This lets us
    // run the printed command verbatim without its optional --force hint.
    std::fs::write(root.join(filename), "other\n").expect("restore merged bytes");
    let restored = Command::new("sh")
        .current_dir(root)
        .args(["-c", &command.replacen("maw ", &format!("{MAW} "), 1)])
        .output()
        .expect("run printed command");
    assert!(
        restored.status.success(),
        "printed command failed: {command}\n{}",
        combined(&restored)
    );
    assert_eq!(read(&root.join(filename)), "user notes\n");
}

/// The symlink points at a real directory that holds an `inner.txt`: the
/// capture must not read `d/inner.txt` through the link (it is a deletion
/// on the user's side), and the link's target must be left alone.
#[test]
fn dir_replaced_by_symlink_to_real_dir_is_not_read_through() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup(root);
    std::fs::create_dir(root.join("x")).expect("mkdir x");
    std::fs::write(root.join("x/inner.txt"), "target\n").expect("write x");
    dirty_trunk(root, "x");

    let out = merge_wa(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    let pin = assert_preserved(root, &text);
    assert_eq!(read(&root.join("x/inner.txt")), "target\n");
    assert_eq!(
        tree_entry(root, &pin, "x/inner.txt").map(|(_, c)| c),
        Some("target".to_owned()),
        "the untracked target dir is the user's too:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// Fail closed: the dirty state cannot be completely captured.
// ---------------------------------------------------------------------------

/// Snapshot fails AND the in-memory capture is incomplete.
#[cfg(feature = "failpoints")]
const UNCAPTURABLE: &str =
    "FP_UPDATE_DEFAULT_BEFORE_SNAPSHOT=error:injected;FP_UPDATE_DEFAULT_CAPTURE=error:injected";

#[cfg(feature = "failpoints")]
fn assert_refused(root: &Path, out: &Output) {
    let text = combined(out);
    assert!(
        !out.status.success(),
        "an uncapturable dirty trunk must fail the merge:\n{text}"
    );
    assert!(!text.contains("[OK]"), "never [OK] on refusal:\n{text}");
    assert!(
        text.contains("maw ws merge --recover"),
        "the refusal must say how to finish:\n{text}"
    );
    assert!(
        !text.contains("Falling back to force checkout"),
        "no force checkout on refusal:\n{text}"
    );
    assert_eq!(read(&root.join("other.txt")), "other\nedit\n", "{text}");
}

/// The live update refuses, leaves the worktree untouched, and a later
/// `maw ws merge --recover` finishes it with every edit preserved.
#[cfg(feature = "failpoints")]
#[test]
fn uncapturable_dirty_trunk_fails_closed_then_recovers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup(root);
    std::fs::write(root.join("other.txt"), "other\nedit\n").expect("edit other");
    std::fs::write(root.join("scratch.txt"), "untracked\n").expect("write scratch");

    let out = merge_wa(root, Some(UNCAPTURABLE));
    assert_refused(root, &out);
    let text = combined(&out);
    assert!(
        !root.join("new.txt").exists(),
        "the worktree must not have been checked out:\n{text}"
    );
    assert_eq!(read(&root.join("scratch.txt")), "untracked\n");
    assert_eq!(read(&root.join("d/inner.txt")), "inner\n");

    let text = maw(root, &["ws", "merge", "--recover"]);
    assert_eq!(read(&root.join("new.txt")), "new\n", "{text}");
    assert_eq!(read(&root.join("other.txt")), "other\nedit\n", "{text}");
    assert_eq!(read(&root.join("scratch.txt")), "untracked\n", "{text}");
}

/// bn-15fzo resume path: an update interrupted after its checkout is resumed
/// by `--recover`; if the tree then holds edits that cannot be captured, the
/// resume refuses too (no force checkout), and a later `--recover` finishes.
#[cfg(feature = "failpoints")]
#[test]
fn uncapturable_residual_on_resume_fails_closed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup(root);
    std::fs::write(root.join("other.txt"), "other\nedit\n").expect("edit other");

    // Interrupt after the target checkout: the tree is the merged one and the
    // pre-merge edit lives only in the pinned snapshot + checkout intent.
    let out = merge_wa(
        root,
        Some("FP_CLEANUP_AFTER_DEFAULT_CHECKOUT=error:injected"),
    );
    assert!(!out.status.success(), "interrupted:\n{}", combined(&out));

    // Edits made after the interruption.
    std::fs::write(root.join("residual.txt"), "residual\n").expect("write residual");

    let out = maw_raw(root, &["ws", "merge", "--recover"], Some(UNCAPTURABLE));
    let text = combined(&out);
    assert!(!out.status.success(), "resume must refuse:\n{text}");
    assert!(text.contains("maw ws merge --recover"), "{text}");
    assert_eq!(
        read(&root.join("residual.txt")),
        "residual\n",
        "the residual edit must be left on disk:\n{text}"
    );

    let text = maw(root, &["ws", "merge", "--recover"]);
    assert_eq!(read(&root.join("new.txt")), "new\n", "{text}");
    assert_eq!(read(&root.join("other.txt")), "other\nedit\n", "{text}");
}

/// The snapshot fails but the in-memory capture is complete, so the merge
/// takes the pin-from-memory fallback. With `d` a symlink to a real directory
/// holding an `inner.txt`, the capture must record `d/inner.txt` as deleted
/// (git's view), never the target's bytes — else the pin carries a phantom
/// file and the fidelity repair writes it back where the user has none.
#[cfg(feature = "failpoints")]
#[test]
fn fallback_capture_never_reads_through_a_symlinked_parent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup(root);
    std::fs::create_dir(root.join("x")).expect("mkdir x");
    std::fs::write(root.join("x/inner.txt"), "target\n").expect("write x");
    dirty_trunk(root, "x");

    let out = merge_wa(
        root,
        Some("FP_UPDATE_DEFAULT_BEFORE_SNAPSHOT=error:injected"),
    );
    let text = combined(&out);
    assert!(out.status.success(), "merge failed:\n{text}");
    assert!(
        text.contains("Falling back to force checkout"),
        "the failpoint must force the in-memory fallback:\n{text}"
    );
    let refs = recovery_refs(root);
    assert_eq!(refs.len(), 1, "exactly one recovery pin: {refs:?}\n{text}");
    let pin = &refs[0];
    assert_eq!(
        tree_entry(root, pin, "d").map(|(m, _)| m),
        Some("120000".to_owned()),
        "the pin must record d as the user's symlink:\n{text}"
    );
    assert_eq!(
        tree_entry(root, pin, "d/inner.txt"),
        None,
        "the pin must not hold the symlink target's bytes as d/inner.txt:\n{text}"
    );
    assert_eq!(read(&root.join("other.txt")), "other\nedit\n", "{text}");
    assert_eq!(read(&root.join("x/inner.txt")), "target\n", "{text}");
    // The report must describe the user's side of d/inner.txt truthfully:
    // not the link target's file, and (bn-1eg2u) not a bare "deleted".
    assert!(
        text.contains("    d/inner.txt\n      merged (wa): regular file\n      yours (uncommitted): replaced by symlink d"),
        "d/inner.txt is replaced by the user's symlink d, not the link target's file:\n{text}"
    );
    match link_target(&root.join("d")) {
        Some(_) => {}
        None => assert_eq!(read(&root.join("d/inner.txt")), "inner\n", "{text}"),
    }
    // bn-1eg2u: one printed command puts the user's symlink back.
    let restore = format!("maw ws recover --ref {pin} --restore-file d");
    assert!(
        text.contains(&format!("restore yours: {restore}")),
        "{text}"
    );
    let out = Command::new("sh")
        .current_dir(root)
        .args(["-c", &restore.replacen("maw ", &format!("{MAW} "), 1)])
        .output()
        .expect("run printed restore");
    assert!(out.status.success(), "{}\n---\n{text}", combined(&out));
    assert_eq!(
        link_target(&root.join("d")),
        Some(PathBuf::from("x")),
        "{text}"
    );
    assert_eq!(read(&root.join("x/inner.txt")), "target\n", "{text}");
}
