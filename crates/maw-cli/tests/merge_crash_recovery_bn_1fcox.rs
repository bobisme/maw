//! bn-1fcox: a `maw ws merge` killed after its COMMIT-phase ref CAS left a
//! merge journal that nothing in production could clear — `--abort` refused
//! (committed work), and FF-absorb, `doctor --repair`, `epoch sync` and `undo`
//! all refuse while it exists. The only way out was deleting the file by
//! hand, which also skipped the merge's CLEANUP: the default workspace stayed
//! checked out at the OLD epoch under a HEAD at the new one (a later commit
//! there would silently revert the merge) and `--destroy` sources survived.
//!
//! These tests crash a real merge with `MAW_FP=<site>=abort` (a real process
//! abort) and check that `maw ws merge --recover`, `--abort`, and the next
//! `maw ws merge` converge: forward when the CAS landed (CLEANUP finished,
//! refs untouched), backward when it provably did not.
//!
//! Needs `--features failpoints`:
//! `cargo test -p maw-cli --features failpoints --test merge_crash_recovery_bn_1fcox`
//! (`just sg1-faithful-test` runs it). The no-failpoint test at the bottom
//! runs in the default gate.

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
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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

fn journal(root: &Path) -> PathBuf {
    root.join(".maw/manifold/merge-state.json")
}

fn epoch(root: &Path) -> String {
    git(root, &["rev-parse", "refs/manifold/epoch/current"])
}

fn main_head(root: &Path) -> String {
    git(root, &["rev-parse", "refs/heads/main"])
}

/// Fresh repo with two workspaces `a` and `b`, each with one committed file.
fn setup(root: &Path) {
    git_quiet(root, &["init", "-b", "main"]);
    git_quiet(root, &["config", "user.email", "test@example.com"]);
    git_quiet(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("f1.txt"), "base\n").expect("write f1");
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-m", "seed"]);
    maw(root, &["init"]);
    git_quiet(root, &["add", "-A"]);
    if !git(root, &["status", "--porcelain"]).is_empty() {
        git_quiet(root, &["commit", "-m", "maw init artifacts"]);
        maw(root, &["epoch", "sync"]);
    }
    for name in ["a", "b"] {
        maw(root, &["ws", "create", name, "--from", "main"]);
        std::fs::write(ws_path(root, name).join(format!("{name}.txt")), "work\n")
            .expect("write ws file");
        maw(root, &["exec", name, "--", "git", "add", "-A"]);
        maw(
            root,
            &[
                "exec",
                name,
                "--",
                "git",
                "commit",
                "-m",
                &format!("{name} work"),
            ],
        );
    }
}

#[cfg_attr(not(feature = "failpoints"), allow(dead_code))]
fn merge(root: &Path, ws: &str, fp: Option<&str>) -> Output {
    maw_raw(
        root,
        &[
            "ws",
            "merge",
            ws,
            "--into",
            "default",
            "--destroy",
            "--message",
            &format!("merge {ws}"),
        ],
        fp,
    )
}

/// Crash `merge a --destroy` at `site`; return the pre-merge epoch.
#[cfg_attr(not(feature = "failpoints"), allow(dead_code))]
fn crash_merge_a(root: &Path, site: &str) -> String {
    let epoch0 = epoch(root);
    let out = merge(root, "a", Some(&format!("{site}=abort")));
    assert!(
        !out.status.success(),
        "the injected abort must kill the merge:\n{}",
        combined(&out)
    );
    assert!(journal(root).exists(), "the crash must leave the journal");
    epoch0
}

/// The merge's CLEANUP happened: the default workspace is checked out at the
/// merged commit (clean status, file present) and `a` was destroyed.
#[cfg_attr(not(feature = "failpoints"), allow(dead_code))]
fn assert_cleanup_finished(root: &Path, merged: &str) {
    assert!(!journal(root).exists(), "journal must be cleared");
    assert!(
        !root.join(".maw/manifold/commit-state.json").exists(),
        "commit-state sidecar must be cleared"
    );
    assert_eq!(epoch(root), merged, "recovery must not move the epoch");
    assert_eq!(git(root, &["rev-parse", "HEAD"]), merged);
    assert_eq!(
        git(root, &["status", "--porcelain", "--untracked-files=no"]),
        "",
        "default workspace must be checked out at the merged commit"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).expect("a.txt in default"),
        "work\n"
    );
    assert!(
        !ws_path(root, "a").exists(),
        "--destroy must be finished for the merged source"
    );
}

/// After a crash AFTER the CAS, `maw ws merge --recover` finishes the merge
/// forward (target checkout + --destroy), leaves the refs alone, and the repo
/// is fully usable: epoch sync, the next merge, undo.
#[cfg(feature = "failpoints")]
#[test]
fn crash_after_cas_then_recover_finishes_cleanup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup(root);
    let epoch0 = crash_merge_a(root, "FP_COMMIT_AFTER_EPOCH_CAS");
    let merged = epoch(root);
    assert_ne!(merged, epoch0, "the CAS landed before the crash");
    assert_eq!(main_head(root), merged);
    assert_eq!(git(root, &["show", "main:a.txt"]), "work");

    // `fsck --repair` used to delete an orphaned journal in ANY phase —
    // skipping this merge's CLEANUP. It must decline a post-CAS journal and
    // point at `--recover`.
    let out = maw_raw(root, &["fsck", "--repair"], None);
    let fsck = combined(&out);
    assert!(
        journal(root).exists(),
        "fsck --repair deleted a post-CAS journal:\n{fsck}"
    );
    assert!(fsck.contains("maw ws merge --recover"), "{fsck}");

    let text = maw(root, &["ws", "merge", "--recover"]);
    assert!(
        text.contains("already landed"),
        "recover must say the commit landed:\n{text}"
    );
    assert_cleanup_finished(root, &merged);

    // Idempotent.
    let again = maw(root, &["ws", "merge", "--recover"]);
    assert!(again.contains("nothing to recover"), "{again}");

    // The repo works again: epoch sync after a direct trunk commit, the next
    // merge, and undo of that merge.
    std::fs::write(root.join("trunk.txt"), "trunk\n").expect("write trunk");
    git_quiet(root, &["add", "trunk.txt"]);
    git_quiet(root, &["commit", "-m", "trunk"]);
    maw(root, &["epoch", "sync"]);
    maw(root, &["ws", "sync", "b"]);
    let out = merge(root, "b", None);
    assert!(out.status.success(), "merge b failed:\n{}", combined(&out));
    assert_eq!(git(root, &["show", "main:a.txt"]), "work");
    assert_eq!(git(root, &["show", "main:b.txt"]), "work");
    assert_eq!(git(root, &["show", "main:trunk.txt"]), "trunk");
    maw(root, &["undo"]);
    assert!(ws_path(root, "b").exists(), "undo restores merged b");
}

/// The same crash, recovered by `--abort`: the commit landed, so it cannot be
/// aborted without hiding committed work — it is finished forward instead
/// (pre-bn-1fcox this refused and told the user to delete the file by hand).
#[cfg(feature = "failpoints")]
#[test]
fn crash_after_cas_then_abort_finishes_instead_of_refusing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup(root);
    crash_merge_a(root, "FP_COMMIT_AFTER_EPOCH_CAS");
    let merged = epoch(root);
    let text = maw(root, &["ws", "merge", "--abort"]);
    assert!(text.contains("cannot be aborted"), "{text}");
    assert!(text.contains("maw undo"), "{text}");
    assert_cleanup_finished(root, &merged);
}

/// Crash in CLEANUP before the default checkout; the NEXT merge recovers the
/// journal automatically (finishing the first merge's cleanup) and then
/// merges normally.
#[cfg(feature = "failpoints")]
#[test]
fn crash_in_cleanup_then_next_merge_recovers_automatically() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup(root);
    crash_merge_a(root, "FP_CLEANUP_BEFORE_DEFAULT_CHECKOUT");
    let merged_a = epoch(root);
    // The crashed merge's auto-rebase already moved `b` onto the new epoch.
    let out = merge(root, "b", None);
    let text = combined(&out);
    assert!(out.status.success(), "merge b failed:\n{text}");
    assert!(!journal(root).exists());
    assert!(
        !ws_path(root, "a").exists(),
        "a's --destroy was never finished:\n{text}"
    );
    assert!(!ws_path(root, "b").exists());
    assert_eq!(
        git(root, &["rev-parse", "main^1"]),
        merged_a,
        "merge b builds on merge a"
    );
    assert_eq!(git(root, &["show", "main:a.txt"]), "work");
    assert_eq!(git(root, &["show", "main:b.txt"]), "work");
    assert_eq!(
        git(root, &["status", "--porcelain", "--untracked-files=no"]),
        "",
        "default workspace checked out at the final merge"
    );
    assert!(
        text.contains("Recovered an interrupted merge"),
        "the next merge must report the recovery:\n{text}"
    );
}

/// A direct trunk commit landed on top of the merged commit before recovery
/// (the classic "user kept working" case): `maw epoch sync` refuses under the
/// journal and points at `--recover`; recovery still converges forward (the
/// merged commit is an ancestor of the branch) without touching the refs, and
/// then `epoch sync` absorbs the trunk commit.
#[cfg(feature = "failpoints")]
#[test]
fn crash_after_cas_trunk_commit_then_epoch_sync_points_at_recover() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup(root);
    crash_merge_a(root, "FP_COMMIT_AFTER_EPOCH_CAS");
    let merged = epoch(root);
    // A trunk commit made from a *clean* checkout of the merged commit (the
    // crash left the default worktree stale; restore it first so the commit
    // does not itself revert the merge).
    git_quiet(root, &["checkout", "-f", "main"]);
    std::fs::write(root.join("trunk.txt"), "trunk\n").expect("write trunk");
    git_quiet(root, &["add", "trunk.txt"]);
    git_quiet(root, &["commit", "-m", "trunk"]);
    let trunk = main_head(root);

    let out = maw_raw(root, &["epoch", "sync"], None);
    let text = combined(&out);
    assert!(!out.status.success(), "epoch sync must refuse:\n{text}");
    assert!(
        text.contains("maw ws merge --recover"),
        "refusal must point at --recover:\n{text}"
    );

    let text = maw(root, &["ws", "merge", "--recover"]);
    assert!(text.contains("already landed"), "{text}");
    assert!(!journal(root).exists());
    assert_eq!(epoch(root), merged, "recovery never moves the epoch");
    assert_eq!(main_head(root), trunk, "recovery never moves the branch");
    assert!(!ws_path(root, "a").exists());

    maw(root, &["epoch", "sync"]);
    assert_eq!(epoch(root), trunk);
    assert_eq!(git(root, &["show", "main:a.txt"]), "work");
    assert_eq!(git(root, &["show", "main:trunk.txt"]), "trunk");
}

/// Crash AFTER entering COMMIT but BEFORE the CAS: the commit provably never
/// landed, so `--recover` aborts it — refs unchanged, source NOT destroyed —
/// and the merge can simply be re-run.
#[cfg(feature = "failpoints")]
#[test]
fn crash_before_cas_then_recover_aborts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup(root);
    let epoch0 = crash_merge_a(root, "FP_COMMIT_BEFORE_BRANCH_CAS");
    assert_eq!(epoch(root), epoch0);
    let text = maw(root, &["ws", "merge", "--recover"]);
    assert!(text.contains("never landed"), "{text}");
    assert!(!journal(root).exists());
    assert_eq!(epoch(root), epoch0);
    assert!(
        ws_path(root, "a").exists(),
        "an aborted merge destroys nothing"
    );
    let out = merge(root, "a", None);
    assert!(out.status.success(), "re-run failed:\n{}", combined(&out));
    assert_eq!(git(root, &["show", "main:a.txt"]), "work");
}

/// A journal whose refs prove neither outcome (the epoch was moved somewhere
/// else behind maw's back) is refused with the journal kept, and the
/// refusal names the refs. No failpoint needed: the journal is written the
/// way a crashed COMMIT leaves it.
#[test]
fn unprovable_ref_state_is_refused_and_journal_kept() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    setup(root);
    let epoch0 = epoch(root);
    // Some unrelated commit object to act as the "candidate" and another the
    // epoch was moved to.
    std::fs::write(root.join("x.txt"), "x\n").expect("write x");
    git_quiet(root, &["add", "x.txt"]);
    git_quiet(root, &["commit", "-m", "x"]);
    let other = main_head(root);
    git_quiet(root, &["reset", "--hard", &epoch0]);
    git_quiet(
        root,
        &["update-ref", "refs/manifold/epoch/current", &other, &epoch0],
    );
    let candidate = git(
        root,
        &[
            "commit-tree",
            "-p",
            &epoch0,
            "-m",
            "cand",
            &format!("{epoch0}^{{tree}}"),
        ],
    );
    let state = serde_json::json!({
        "phase": "commit",
        "sources": ["a"],
        "epoch_before": epoch0,
        "epoch_candidate": candidate,
        "epoch_after": candidate,
        "started_at": 1,
        "updated_at": 1,
        "target_branch": "main",
        "target_workspace": "default",
        "updates_epoch": true,
        "destroy_after": true,
    });
    std::fs::write(journal(root), serde_json::to_string(&state).expect("json"))
        .expect("write journal");

    let out = maw_raw(root, &["ws", "merge", "--recover"], None);
    let text = combined(&out);
    assert!(!out.status.success(), "must refuse:\n{text}");
    assert!(text.contains("Cannot recover"), "{text}");
    assert!(
        text.contains(&candidate),
        "names the merged commit:\n{text}"
    );
    assert!(journal(root).exists(), "journal kept on refusal");
    assert!(ws_path(root, "a").exists(), "nothing destroyed on refusal");
    assert_eq!(epoch(root), other, "refs untouched on refusal");
}
