//! bn-302v: FF-absorb sibling advance ordering and concurrency.
//!
//! When `maw ws merge` absorbs a direct trunk commit (FF-absorb), every clean
//! sibling whose HEAD is at its base epoch is fast-forwarded: the absorbed
//! paths are materialized, HEAD moves to the new epoch, and the sibling's
//! per-workspace epoch ref (the snapshot diff base) follows.
//!
//! * Defect A: the epoch ref used to be written FIRST. A crash, or an early
//!   warn-and-return in the materialize step, left the ref claiming the new
//!   epoch over an old HEAD/worktree. The next merge of that sibling diffed
//!   new-epoch -> old worktree, which is a clean-merging REVERT of the
//!   absorbed trunk commit.
//! * Defect B: the loop held no sibling lock and reused the classification
//!   time dirty set, so an agent edit to an absorbed path that landed after
//!   classification was overwritten, and a concurrent maw process on the
//!   sibling could race the HEAD move.
//!
//! The failpoint tests need `--features failpoints` (the `fp!()` sites compile
//! to nothing otherwise); run them with `just sg1-faithful-test` or
//! `cargo test -p maw-cli --features failpoints --test ff_absorb_sibling_order_bn_302v`.
//! The lock-contention test needs no failpoint and runs in the default gate.

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

fn epoch_ref(root: &Path, ws: &str) -> String {
    git(
        root,
        &["rev-parse", &format!("refs/manifold/epoch/ws/{ws}")],
    )
}

/// Repo with f1/f2 at base epoch E0, workspaces `sib` (clean, never touched
/// by its agent before the absorb) and `src` (one commit adding `src.txt`),
/// plus a direct trunk commit that changes `f2.txt` so the next merge of
/// `src` FF-absorbs it. Returns (`sib` HEAD at E0, trunk tip).
fn setup(root: &Path) -> (String, String) {
    git_quiet(root, &["init", "-b", "main"]);
    git_quiet(root, &["config", "user.email", "test@example.com"]);
    git_quiet(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("f1.txt"), "f1-base\n").expect("write f1");
    std::fs::write(root.join("f2.txt"), "f2-base\n").expect("write f2");
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-m", "seed"]);
    maw(root, &["init"]);
    git_quiet(root, &["add", "-A"]);
    if !git(root, &["status", "--porcelain"]).is_empty() {
        git_quiet(root, &["commit", "-m", "maw init artifacts"]);
        maw(root, &["epoch", "sync"]);
    }

    maw(root, &["ws", "create", "sib", "--from", "main"]);
    maw(root, &["ws", "create", "src", "--from", "main"]);
    let sib_head = git(&ws_path(root, "sib"), &["rev-parse", "HEAD"]);

    // Direct trunk commit, NOT epoch-synced: the FF-absorb range.
    std::fs::write(root.join("f2.txt"), "f2-trunk\n").expect("write trunk f2");
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-m", "trunk: f2"]);
    let trunk = git(root, &["rev-parse", "HEAD"]);

    let src = ws_path(root, "src");
    std::fs::write(src.join("src.txt"), "src work\n").expect("write src");
    maw(root, &["exec", "src", "--", "git", "add", "-A"]);
    maw(
        root,
        &["exec", "src", "--", "git", "commit", "-m", "src work"],
    );
    (sib_head, trunk)
}

fn merge_src(root: &Path, fp: Option<&str>) -> Output {
    maw_raw(
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
        fp,
    )
}

/// The agent in `sib` does ordinary work afterwards and the lead merges it.
/// The trunk commit's `f2.txt` must survive: a revert here is the bn-302v
/// silent-data-loss symptom.
fn agent_work_then_merge_sib_keeps_trunk(root: &Path) {
    // The agent commits whatever it has, brings the workspace up to date
    // (the documented recovery for a stale workspace), and adds more work.
    let sib = ws_path(root, "sib");
    if !git(&sib, &["status", "--porcelain"]).is_empty() {
        git_quiet(&sib, &["add", "-A"]);
        git_quiet(&sib, &["commit", "-m", "sib wip"]);
    }
    maw(root, &["ws", "sync", "sib"]);
    std::fs::write(sib.join("f4.txt"), "sib work\n").expect("write sib f4");
    maw(root, &["exec", "sib", "--", "git", "add", "-A"]);
    maw(
        root,
        &["exec", "sib", "--", "git", "commit", "-m", "sib work"],
    );
    let out = maw_raw(
        root,
        &[
            "ws",
            "merge",
            "sib",
            "--into",
            "default",
            "--message",
            "merge sib",
        ],
        None,
    );
    let text = combined(&out);
    assert!(out.status.success(), "merge sib failed:\n{text}");
    assert_eq!(
        git(root, &["show", "main:f2.txt"]),
        "f2-trunk",
        "merging the sibling silently reverted the absorbed trunk commit:\n{text}"
    );
    assert_eq!(git(root, &["show", "main:f4.txt"]), "sib work");
}

/// Invariant after ANY FF-absorb outcome for a sibling: its epoch ref is an
/// ancestor of (or equal to) its HEAD — never ahead of it.
fn assert_epoch_ref_not_ahead(root: &Path, ws: &str) {
    let base = epoch_ref(root, ws);
    let head = git(&ws_path(root, ws), &["rev-parse", "HEAD"]);
    let ok = Command::new("git")
        .current_dir(root)
        .args(["merge-base", "--is-ancestor", &base, &head])
        .status()
        .expect("git merge-base")
        .success();
    assert!(
        ok,
        "workspace '{ws}' epoch ref {base} is not an ancestor of HEAD {head} \
         (epoch ref ahead of HEAD: next merge would revert)"
    );
}

/// Defect A, warn path: the materialize step fails (e.g. the sibling repo
/// cannot be opened). The sibling must stay STALE (epoch ref at its old
/// base), never "ref advanced, files old". The sibling carries a disjoint
/// uncommitted edit so the post-merge auto-rebase skips it and cannot mask
/// the FF-absorb outcome.
#[cfg(feature = "failpoints")]
#[test]
fn materialize_failure_leaves_sibling_stale_and_next_merge_keeps_trunk() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = td.path();
    let (sib_head, trunk) = setup(root);
    std::fs::write(ws_path(root, "sib").join("f1.txt"), "f1-agent\n").expect("edit f1");

    let out = merge_src(
        root,
        Some("FP_FF_ABSORB_BEFORE_SIBLING_MATERIALIZE=error:injected"),
    );
    assert!(
        out.status.success(),
        "merge src failed:\n{}",
        combined(&out)
    );
    assert!(
        git(root, &["merge-base", "--is-ancestor", &trunk, "main"]).is_empty(),
        "trunk commit must be in main"
    );

    let sib = ws_path(root, "sib");
    assert_eq!(git(&sib, &["rev-parse", "HEAD"]), sib_head);
    assert_eq!(
        epoch_ref(root, "sib"),
        sib_head,
        "a failed materialize must leave the sibling epoch ref at its old base"
    );
    assert_epoch_ref_not_ahead(root, "sib");
    assert_eq!(
        std::fs::read_to_string(ws_path(root, "sib").join("f1.txt")).expect("read f1"),
        "f1-agent\n"
    );
    agent_work_then_merge_sib_keeps_trunk(root);
    assert_eq!(git(root, &["show", "main:f1.txt"]), "f1-agent");
}

/// Defect A, crash: the merge process dies inside the sibling loop before
/// the materialize step. Same requirement as the warn path.
#[cfg(feature = "failpoints")]
#[test]
fn crash_before_materialize_leaves_sibling_stale_and_next_merge_keeps_trunk() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = td.path();
    let (sib_head, _trunk) = setup(root);

    let out = merge_src(root, Some("FP_FF_ABSORB_BEFORE_SIBLING_MATERIALIZE=abort"));
    assert!(
        !out.status.success(),
        "the injected abort must kill the merge"
    );

    assert_eq!(git(&ws_path(root, "sib"), &["rev-parse", "HEAD"]), sib_head);
    assert_epoch_ref_not_ahead(root, "sib");
    agent_work_then_merge_sib_keeps_trunk(root);
}

/// Defect A, crash after the HEAD move but before the epoch-ref write (the
/// new order's last window). HEAD is ahead of the ref, which is the safe
/// direction: the sibling reads as stale and syncs cleanly.
#[cfg(feature = "failpoints")]
#[test]
fn crash_between_head_move_and_epoch_ref_is_recoverable() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = td.path();
    let (_sib_head, _trunk) = setup(root);

    let out = merge_src(root, Some("FP_FF_ABSORB_BEFORE_SIBLING_EPOCH_REF=abort"));
    assert!(
        !out.status.success(),
        "the injected abort must kill the merge"
    );
    assert_epoch_ref_not_ahead(root, "sib");
    let status = git(&ws_path(root, "sib"), &["status", "--porcelain"]);
    assert!(status.is_empty(), "sibling must be clean, got:\n{status}");
    agent_work_then_merge_sib_keeps_trunk(root);
}

/// Defect B: an agent edits an absorbed path AFTER classification (the
/// failpoint writes the file, standing in for the agent). The re-check under
/// the sibling lock must see it and leave the sibling stale instead of
/// overwriting the edit with the trunk blob.
#[cfg(feature = "failpoints")]
#[test]
fn agent_edit_after_classification_is_never_overwritten() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = td.path();
    let (sib_head, _trunk) = setup(root);
    let sib = ws_path(root, "sib");
    let victim = sib.join("f2.txt");

    let spec = format!(
        "FP_FF_ABSORB_BEFORE_SIBLING_LOCK=corrupt:{}",
        victim.display()
    );
    let out = merge_src(root, Some(&spec));
    let text = combined(&out);
    assert!(out.status.success(), "merge src failed:\n{text}");

    assert_eq!(
        std::fs::read(&victim).expect("read sib f2"),
        maw_core::failpoints::CORRUPT_BYTES,
        "the agent's post-classification edit was overwritten by FF-absorb:\n{text}"
    );
    assert_eq!(
        git(&sib, &["rev-parse", "HEAD"]),
        sib_head,
        "HEAD must not move"
    );
    assert_eq!(epoch_ref(root, "sib"), sib_head, "epoch ref must not move");
    assert!(
        text.contains("sib") && text.contains("left stale"),
        "the skip must be reported:\n{text}"
    );
}

/// Defect B: another maw process holds the sibling's rebase lock (e.g. a
/// `maw ws sync` or an auto-sync). FF-absorb must skip that sibling (leave
/// it stale) rather than move its HEAD underneath the other process.
#[test]
fn held_sibling_rebase_lock_skips_ff_advance() {
    use fs4::fs_std::FileExt;

    let td = tempfile::tempdir().expect("tempdir");
    let root = td.path();
    let (sib_head, _trunk) = setup(root);
    let sib = ws_path(root, "sib");

    let lock_dir = root.join(".maw/manifold/locks/rebase");
    std::fs::create_dir_all(&lock_dir).expect("mkdir lock dir");
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_dir.join("sib.lock"))
        .expect("open lock");
    lock.try_lock_exclusive().expect("take sib rebase lock");

    let out = merge_src(root, None);
    let text = combined(&out);
    assert!(out.status.success(), "merge src failed:\n{text}");
    assert_eq!(
        git(&sib, &["rev-parse", "HEAD"]),
        sib_head,
        "FF-absorb moved HEAD of a sibling whose rebase lock is held:\n{text}"
    );
    assert_eq!(epoch_ref(root, "sib"), sib_head);
    assert_epoch_ref_not_ahead(root, "sib");
    assert_eq!(
        std::fs::read_to_string(sib.join("f2.txt")).expect("read f2"),
        "f2-base\n",
        "FF-absorb wrote files into a locked sibling"
    );
    assert!(
        text.contains("sib") && text.contains("in use"),
        "the skip must be reported:\n{text}"
    );

    drop(lock);
    agent_work_then_merge_sib_keeps_trunk(root);
}

/// A merge crashed inside COMMIT (journal written, refs not moved), then a
/// direct trunk commit landed. The next `ws merge` must NOT FF-absorb the
/// trunk commit into the epoch before PREPARE sees the journal: that would
/// move the epoch away from the journal's `epoch_before` and make
/// `maw ws merge --abort` refuse forever. It refuses instead, the documented
/// recovery works, and the merge then succeeds.
#[cfg(feature = "failpoints")]
#[test]
fn ff_absorb_refuses_under_crashed_commit_journal_and_recovery_converges() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = td.path();
    git_quiet(root, &["init", "-b", "main"]);
    git_quiet(root, &["config", "user.email", "test@example.com"]);
    git_quiet(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("f1.txt"), "f1-base\n").expect("write f1");
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-m", "seed"]);
    maw(root, &["init"]);
    git_quiet(root, &["add", "-A"]);
    if !git(root, &["status", "--porcelain"]).is_empty() {
        git_quiet(root, &["commit", "-m", "maw init artifacts"]);
        maw(root, &["epoch", "sync"]);
    }
    for ws in ["a", "b"] {
        maw(root, &["ws", "create", ws, "--from", "main"]);
        std::fs::write(ws_path(root, ws).join(format!("{ws}.txt")), "work\n").expect("write");
        maw(root, &["exec", ws, "--", "git", "add", "-A"]);
        maw(root, &["exec", ws, "--", "git", "commit", "-m", "work"]);
    }
    let epoch0 = git(root, &["rev-parse", "refs/manifold/epoch/current"]);

    let out = maw_raw(
        root,
        &[
            "ws",
            "merge",
            "a",
            "--into",
            "default",
            "--message",
            "merge a",
        ],
        Some("FP_COMMIT_BEFORE_BRANCH_CAS=abort"),
    );
    assert!(
        !out.status.success(),
        "the injected abort must kill the merge"
    );

    // Direct trunk commit while the crashed journal is still there.
    std::fs::write(root.join("trunk.txt"), "trunk\n").expect("write trunk");
    git_quiet(root, &["add", "trunk.txt"]);
    git_quiet(root, &["commit", "-m", "trunk"]);

    let out = maw_raw(
        root,
        &[
            "ws",
            "merge",
            "b",
            "--into",
            "default",
            "--message",
            "merge b",
        ],
        None,
    );
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "merge must refuse under the journal:\n{text}"
    );
    assert!(
        text.contains("did not finish"),
        "unexpected refusal:\n{text}"
    );
    assert_eq!(
        git(root, &["rev-parse", "refs/manifold/epoch/current"]),
        epoch0,
        "the epoch must not be absorbed past the crashed merge's epoch_before"
    );

    maw(root, &["ws", "merge", "--abort"]);
    maw(
        root,
        &[
            "ws",
            "merge",
            "b",
            "--into",
            "default",
            "--message",
            "merge b",
        ],
    );
    assert_eq!(git(root, &["show", "main:trunk.txt"]), "trunk");
    assert_eq!(git(root, &["show", "main:b.txt"]), "work");
}
