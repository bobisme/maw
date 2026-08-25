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

/// bn-2fto: the sigil field report's EXACT shape, one epoch deep.
///
/// A workspace is created at epoch E0 and edits one hunk of `f1.txt` WITHOUT
/// committing. A sibling commits a DIFFERENT hunk of the same file and the lead
/// merges it, advancing the epoch to E1. The dirty workspace must not be
/// advanced behind the agent's back: its HEAD stays at E0, its uncommitted edit
/// survives byte-for-byte, and — the symptom that made this a data-loss bug —
/// its next commit must NOT revert the sibling's hunk.
///
/// This shape never reaches FF-absorb (epoch == branch at merge time); it is
/// handled by the post-merge sibling auto-rebase, which skips dirty siblings.
/// The test pins that end-to-end behaviour so a future change to either path
/// cannot reintroduce the field symptom.
#[test]
fn dirty_sibling_editing_the_merged_file_is_not_advanced_no_silent_revert() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = td.path();
    setup(root);
    // A two-hunk file so the agent and the sibling can edit different regions.
    std::fs::write(root.join("f1.txt"), "head: base\nmiddle\ntail: base\n").expect("write f1");
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-m", "f1: two hunks"]);
    maw(root, &["epoch", "sync"]);

    maw(root, &["ws", "create", "victim", "--from", "main"]);
    maw(root, &["ws", "create", "src", "--from", "main"]);

    let victim = root.join(".maw/workspaces/victim");
    let victim_head_e0 = git(&victim, &["rev-parse", "HEAD"]);

    // The agent edits the head hunk — uncommitted.
    std::fs::write(
        victim.join("f1.txt"),
        "head: EDITED-BY-AGENT\nmiddle\ntail: base\n",
    )
    .expect("edit victim f1");

    // The sibling commits the tail hunk of the SAME file and is merged.
    let src_path = root.join(".maw/workspaces/src");
    std::fs::write(
        src_path.join("f1.txt"),
        "head: base\nmiddle\ntail: FROM-SIBLING\n",
    )
    .expect("write src f1");
    maw(root, &["exec", "src", "--", "git", "add", "-A"]);
    maw(
        root,
        &["exec", "src", "--", "git", "commit", "-m", "src: tail hunk"],
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
    assert!(
        out.contains("victim") && out.contains("skipped: dirty"),
        "merge must report the dirty sibling as skipped; got:\n{out}"
    );

    // HEAD must not have moved: an advance here is what strands the stale
    // worker bytes behind a HEAD claiming the new epoch.
    assert_eq!(
        git(&victim, &["rev-parse", "HEAD"]),
        victim_head_e0,
        "a dirty sibling's HEAD must stay at its base epoch across a sibling merge"
    );
    assert_eq!(
        std::fs::read_to_string(victim.join("f1.txt")).expect("read victim f1"),
        "head: EDITED-BY-AGENT\nmiddle\ntail: base\n",
        "the agent's uncommitted edit must survive verbatim"
    );

    // End-to-end symptom check: the agent commits and the resulting commit must
    // carry ONLY its own hunk. Pre-fix, HEAD had advanced, so this diff deleted
    // the sibling's `tail: FROM-SIBLING` line — a silent revert of merged work.
    maw(root, &["exec", "victim", "--", "git", "add", "-A"]);
    let _ = Command::new(MAW)
        .current_dir(root)
        .args([
            "exec",
            "victim",
            "--",
            "git",
            "commit",
            "-m",
            "agent commit",
        ])
        .output()
        .expect("run maw exec git commit");
    assert_eq!(
        git(&victim, &["rev-parse", "HEAD^"]),
        victim_head_e0,
        "agent commit must parent on the sibling's base epoch"
    );
    let diff = git(&victim, &["show", "HEAD", "--", "f1.txt"]);
    assert!(
        !diff.contains("-tail: FROM-SIBLING"),
        "agent commit must not revert the merged sibling's hunk:\n{diff}"
    );
    assert!(
        diff.contains("+head: EDITED-BY-AGENT"),
        "agent commit must carry the agent's own hunk:\n{diff}"
    );
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

    // bn-2fto (field-report Expected item 3): the FF-absorb moved this
    // sibling's HEAD and epoch ref behind the agent's back. That advance must
    // be visible in `maw ws history` — it used to leave no trace at all.
    let history = maw(root, &["ws", "history", "victim"]);
    assert!(
        history.contains("[rebase]") && history.contains("absorb:ff("),
        "FF-absorb advance must be recorded in ws history; got:\n{history}"
    );
}

/// A directory-to-file transition must be applied child-first during an FF
/// absorb. If the new file is written before the old tracked child is removed,
/// the directory still occupies the destination and the write fails. The old
/// code then moved HEAD and the index anyway, leaving the sibling dirty at a
/// path that should have been cleanly materialized.
#[test]
fn ff_absorb_materializes_directory_to_file_transition_cleanly() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = td.path();
    setup(root);

    std::fs::create_dir_all(root.join("shape")).expect("create shape directory");
    std::fs::write(root.join("shape/old.txt"), "old child\n").expect("write old child");
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-m", "A: shape is a directory"]);
    maw(root, &["epoch", "sync"]);

    maw(root, &["ws", "create", "victim", "--from", "main"]);
    maw(root, &["ws", "create", "src", "--from", "main"]);

    // Advance trunk outside maw: the tracked directory becomes one file.
    std::fs::remove_dir_all(root.join("shape")).expect("remove old shape directory");
    std::fs::write(root.join("shape"), "shape is now a file\n").expect("write new shape file");
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-m", "B: shape becomes a file"]);

    // Give the source real work. Its merge absorbs A..B and refreshes the
    // clean victim workspace as a sibling.
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
    assert!(
        victim.join("shape").is_file(),
        "FF absorb must replace the old directory with the target file"
    );
    assert_eq!(
        std::fs::read_to_string(victim.join("shape")).expect("read materialized shape"),
        "shape is now a file\n"
    );
    let status = git(&victim, &["status", "--porcelain"]);
    assert!(
        status.is_empty(),
        "directory-to-file FF absorb must leave the sibling clean, got:\n{status}"
    );
}

/// bn-mq3b / bn-2fto: a dirty sibling whose uncommitted edit lands on a path
/// that is STALE against the absorbed epoch must be left fully stale — HEAD is
/// NOT advanced. The pre-fix code advanced the sibling's HEAD to the absorbed
/// tip while skipping the dirty path's materialization, so the worktree held
/// the old, agent-edited blob behind a HEAD that claimed the new epoch. The
/// sibling's next commit then silently REVERTED the epoch's hunks in that path
/// (data loss that only surfaced at review time).
///
/// The safe behavior: leave the sibling exactly where it is (like auto-rebase's
/// `SkippedDirty`), warn, and let a later `maw ws sync` do a real 3-way rebase.
#[test]
fn ff_absorb_leaves_stale_dirty_sibling_untouched_no_silent_revert() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = td.path();
    setup(root);

    maw(root, &["ws", "create", "victim", "--from", "main"]);
    maw(root, &["ws", "create", "src", "--from", "main"]);

    // Epoch A is the sibling's base. Capture it to prove HEAD never moves.
    let victim = root.join(".maw/workspaces/victim");
    let victim_head_a = git(&victim, &["rev-parse", "HEAD"]);

    std::fs::write(root.join("f1.txt"), "f1-at-B\n").expect("write f1");
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-m", "B: trunk direct, touches f1"]);
    maw(root, &["epoch", "sync"]);

    std::fs::write(root.join("f2.txt"), "f2-at-C\n").expect("write f2");
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-m", "C: trunk direct, touches f2"]);

    // The sibling has an uncommitted edit on f1.txt — the very path that is
    // stale against the absorbed epoch (it changed A..B, outside the B..C
    // `ff_paths` range).
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

    // The uncommitted edit is preserved verbatim.
    let f1 = std::fs::read_to_string(victim.join("f1.txt")).expect("read victim f1");
    assert_eq!(
        f1, "f1-EDITED-BY-AGENT\n",
        "FF-absorb must never clobber an uncommitted local edit"
    );
    // The merge names the stale sibling it declined to fast-forward.
    assert!(
        out.contains("left stale (NOT fast-forwarded)"),
        "merge must warn that the stale-but-edited sibling was left stale; got:\n{out}"
    );
    // The core invariant: HEAD did NOT move. Advancing it is what makes the
    // next commit silently revert the epoch's hunks.
    let victim_head_after = git(&victim, &["rev-parse", "HEAD"]);
    assert_eq!(
        victim_head_after, victim_head_a,
        "a stale dirty sibling's HEAD must stay at its base epoch — advancing it \
         strands the old blob and the next commit reverts the epoch's hunks"
    );

    // Prove the absence of silent data loss end-to-end: the agent commits, and
    // the resulting commit must contain ONLY the agent's own change (f1: A ->
    // EDITED). If HEAD had been advanced to the tip, this diff would instead
    // DELETE the absorbed hunks (f1-at-B, f2-at-C) — the bn-2fto symptom.
    maw(root, &["exec", "victim", "--", "git", "add", "-A"]);
    // `maw exec` auto-sync refuses to advance a dirty stale worktree, so the
    // commit lands on the sibling's own base epoch.
    let _ = Command::new(MAW)
        .current_dir(root)
        .args([
            "exec",
            "victim",
            "--",
            "git",
            "commit",
            "-m",
            "agent commit",
        ])
        .output()
        .expect("run maw exec git commit");
    let parent = git(&victim, &["rev-parse", "HEAD^"]);
    assert_eq!(
        parent, victim_head_a,
        "agent commit must parent on the sibling's base epoch, not the absorbed tip"
    );
    let show = git(&victim, &["show", "--stat", "HEAD"]);
    assert!(
        !show.contains("f2.txt"),
        "agent commit must not touch f2.txt (an absorbed-range path it never \
         edited) — a reference to it means the epoch's hunks were reverted:\n{show}"
    );
    let committed_f1 = git(&victim, &["show", "HEAD:f1.txt"]);
    assert_eq!(
        committed_f1, "f1-EDITED-BY-AGENT",
        "agent commit must carry the agent's own f1 edit"
    );

    // bn-2fto (field-report Expected item 3): a skipped sibling is as important
    // to see as an advanced one — the agent needs to learn from its own history
    // why its workspace stayed behind the epoch.
    let history = maw(root, &["ws", "history", "victim"]);
    assert!(
        history.contains("ff-absorb-skipped") && history.contains("f1.txt"),
        "FF-absorb skip must be recorded in ws history (naming the stale path); got:\n{history}"
    );
}
