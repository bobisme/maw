//! Regression coverage for bn-p3m9 (continuum field report 3).
//!
//! FF-absorb used to materialise a sibling workspace's worktree from
//! `ff_paths` — the delta of the GLOBAL pre-absorb epoch against the branch
//! tip — while moving that sibling's HEAD and index all the way to the tip.
//! For a sibling more than one epoch behind (the epoch had been advanced by
//! `maw epoch sync` after a direct trunk commit, which does not touch live
//! workspaces), the paths in `ws_epoch..pre_absorb_epoch` were never written.
//! The worktree kept the OLD blobs behind a HEAD that claimed otherwise, so
//! `git status` reported them as a local revert of the skipped commit.
//!
//! These tests drive the built `maw` binary so the whole merge/absorb wiring
//! is exercised. Rebuild `maw-cli` before trusting a run.

use std::path::Path;
use std::process::{Command, Stdio};

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

fn maw(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(MAW)
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run maw");
    assert!(
        out.status.success(),
        "maw {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    combined
}

/// git init + maw init + one commit that already contains everything `maw
/// init` writes, so the base commit is a clean starting tree.
fn setup(dir: &Path) {
    git_quiet(dir, &["init", "-b", "main"]);
    git_quiet(dir, &["config", "user.email", "test@example.com"]);
    git_quiet(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("f1.txt"), "f1-at-A\n").expect("write f1");
    std::fs::write(dir.join("f2.txt"), "f2-at-A\n").expect("write f2");
    git_quiet(dir, &["add", "-A"]);
    git_quiet(dir, &["commit", "-m", "seed"]);
    maw(dir, &["init"]);
    // Fold anything `maw init` added (e.g. .gitignore) into the base commit
    // and re-sync so the epoch is exactly the base commit.
    git_quiet(dir, &["add", "-A"]);
    let dirty = git(dir, &["status", "--porcelain"]);
    if !dirty.is_empty() {
        git_quiet(dir, &["commit", "-m", "maw init artifacts"]);
        maw(dir, &["epoch", "sync"]);
    }
}

/// Drive the exact continuum shape:
///   `ws_epoch` (A)  <  `pre_absorb_epoch` (B)  <  branch tip (C)
/// and assert the FF-absorbed sibling's worktree matches its new HEAD.
#[test]
fn ff_absorb_refreshes_paths_a_doubly_stale_sibling_missed() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = td.path();
    setup(root);

    // Sibling created while the epoch is still A. It is never touched again.
    maw(root, &["ws", "create", "victim", "--from", "main"]);
    // Merge source, also at A.
    maw(root, &["ws", "create", "src", "--from", "main"]);

    // B: direct trunk commit touching f1, then `maw epoch sync`. This
    // advances refs/manifold/epoch/current to B but leaves `victim` at A.
    std::fs::write(root.join("f1.txt"), "f1-at-B\n").expect("write f1");
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-m", "B: trunk direct, touches f1"]);
    maw(root, &["epoch", "sync"]);

    // C: another direct trunk commit, deliberately NOT epoch-synced, so the
    // next merge has a B..C range to absorb.
    std::fs::write(root.join("f2.txt"), "f2-at-C\n").expect("write f2");
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-m", "C: trunk direct, touches f2"]);

    // Give the merge source a commit so the merge is real.
    let src_path = root.join(".maw/workspaces/src");
    std::fs::write(src_path.join("src.txt"), "src work\n").expect("write src file");
    maw(root, &["exec", "src", "--", "git", "add", "-A"]);
    maw(
        root,
        &["exec", "src", "--", "git", "commit", "-m", "src work"],
    );

    maw(
        root,
        &[
            "ws",
            "merge",
            "src",
            "--into",
            "default",
            "--message",
            "merge src",
        ],
    );

    let victim = root.join(".maw/workspaces/victim");

    // The bug: f1.txt kept its A content because `ff_paths` (diff B..C) only
    // named f2.txt, while HEAD and the index jumped to the tip.
    let f1 = std::fs::read_to_string(victim.join("f1.txt")).expect("read victim f1");
    assert_eq!(
        f1, "f1-at-B\n",
        "FF-absorb left a doubly-stale sibling holding the pre-B blob of f1.txt"
    );
    let f2 = std::fs::read_to_string(victim.join("f2.txt")).expect("read victim f2");
    assert_eq!(f2, "f2-at-C\n", "absorbed-range path was not materialized");

    // The invariant that actually matters: a clean workspace's worktree must
    // equal its HEAD tree after any materialization step.
    let status = git(&victim, &["status", "--porcelain"]);
    assert!(
        status.is_empty(),
        "FF-absorbed sibling must be clean, got:\n{status}"
    );
}

/// A local edit on a path that is stale against the absorbed epoch must be
/// preserved, not clobbered, and the merge must say so.
#[test]
fn ff_absorb_preserves_local_edits_on_doubly_stale_paths_and_warns() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = td.path();
    setup(root);

    maw(root, &["ws", "create", "victim", "--from", "main"]);
    maw(root, &["ws", "create", "src", "--from", "main"]);

    std::fs::write(root.join("f1.txt"), "f1-at-B\n").expect("write f1");
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-m", "B: trunk direct, touches f1"]);
    maw(root, &["epoch", "sync"]);

    std::fs::write(root.join("f2.txt"), "f2-at-C\n").expect("write f2");
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-m", "C: trunk direct, touches f2"]);

    // The sibling has an uncommitted edit on f1.txt — the very path that is
    // stale against the absorbed epoch.
    let victim = root.join(".maw/workspaces/victim");
    std::fs::write(victim.join("f1.txt"), "f1-EDITED-BY-AGENT\n").expect("edit victim f1");

    let src_path = root.join(".maw/workspaces/src");
    std::fs::write(src_path.join("src.txt"), "src work\n").expect("write src file");
    maw(root, &["exec", "src", "--", "git", "add", "-A"]);
    maw(
        root,
        &["exec", "src", "--", "git", "commit", "-m", "src work"],
    );

    let out = maw(
        root,
        &[
            "ws",
            "merge",
            "src",
            "--into",
            "default",
            "--message",
            "merge src",
        ],
    );

    let f1 = std::fs::read_to_string(victim.join("f1.txt")).expect("read victim f1");
    assert_eq!(
        f1, "f1-EDITED-BY-AGENT\n",
        "FF-absorb must never clobber an uncommitted local edit"
    );
    assert!(
        out.contains("were NOT refreshed"),
        "merge must warn about the stale-but-edited path; got:\n{out}"
    );
}
