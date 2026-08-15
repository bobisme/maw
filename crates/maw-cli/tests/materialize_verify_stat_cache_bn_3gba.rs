//! The index-stat-cache regression for post-materialization verify (bn-3gba).
//!
//! # What this pins
//!
//! `git status --porcelain`, `git diff HEAD` and gix's
//! `status_head_to_worktree` all short-circuit on the index **stat cache**: an
//! index entry whose recorded `(size, mtime, ctime, ino, …)` matches the file on
//! disk is declared unmodified *without the file ever being read*.
//!
//! The bn-p3m9 corrupter writes stale bytes into the working tree and **then**
//! rewrites HEAD and the index (`set_head_detached` + index realignment). That
//! ordering can leave an index entry carrying the CORRECT blob OID stamped with
//! the STALE file's stat data — at which point every status-shaped check reports
//! a clean workspace while the working tree is genuinely wrong.
//!
//! This test reproduces that exact fingerprint with the very primitives the
//! corrupting call site uses (`maw_git`'s `set_head_detached` + `unstage_all`)
//! and asserts that
//! `workspace::materialize_verify::verify_clean_materialization` **still**
//! detects, preserves and repairs it. It is the executable form of the rule
//! "compare TREES, never stat data".
//!
//! It deliberately calls the verifier as a library function rather than through
//! a failpoint, so it needs no `--features failpoints` binary and therefore runs
//! in the DEFAULT `just check` lane, where a regression would be caught the same
//! day it lands.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use maw_cli::workspace::materialize_verify::{MaterializeOp, verify_clean_materialization};
use maw_git::GitRepo as _;

const MAW: &str = env!("CARGO_BIN_EXE_maw");

const VICTIM: &str = "src/victim.rs";
const GOOD: &str = "pub fn answer() -> u32 {\n    42\n}\n";
/// Same byte length as `GOOD` so even a size-only stat comparison cannot
/// separate them — the strongest form of the mask.
const STALE: &str = "pub fn answer() -> u32 {\n    17\n}\n";

fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn maw(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(MAW)
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run maw")
}

/// `git init` + seed + `maw init` + one workspace. Returns `(root, ws_path)`.
fn setup(dir: &Path, ws_name: &str) -> (PathBuf, PathBuf) {
    run_git(dir, &["init", "-b", "main"]);
    run_git(dir, &["config", "user.email", "test@example.com"]);
    run_git(dir, &["config", "user.name", "Test"]);
    std::fs::create_dir_all(dir.join("src")).expect("mkdir src");
    std::fs::write(dir.join(VICTIM), GOOD).expect("write victim");
    std::fs::write(dir.join("README.md"), "hi\n").expect("write readme");
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-m", "init"]);

    let out = maw(dir, &["init"]);
    assert!(
        out.status.success(),
        "maw init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = maw(dir, &["ws", "create", "--from", "main", ws_name]);
    assert!(
        out.status.success(),
        "maw ws create failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let ws = dir.join(".maw").join("workspaces").join(ws_name);
    assert!(
        ws.is_dir(),
        "workspace not materialized at {}",
        ws.display()
    );
    (dir.to_path_buf(), ws)
}

/// Build the WORST CASE for a stat-based checker: stale bytes on disk that
/// every status-shaped query calls clean.
///
/// Recipe, in the order the real corrupter produces it:
///
/// 1. back-date the victim so the index will record an unambiguously old mtime
///    (an entry whose mtime equals the index's own is "racily clean" and gets
///    re-hashed, which would accidentally rescue a stat-based checker);
/// 2. run the same primitives `sync_ff_paths_in_worktree` runs after
///    materializing its (partial) path set — `set_head_detached` then
///    `unstage_all` — so the index holds HEAD's blob OID plus that stat data;
/// 3. overwrite the file with `STALE` (identical byte length) and restore the
///    back-dated mtime, so `(size, mtime)` still match the index entry.
///
/// `core.checkStat = minimal` narrows git's comparison to size+mtime. It is a
/// real, documented setting (recommended on filesystems with unstable
/// ctime/ino, e.g. some network and container mounts), and it is the only part
/// of the fingerprint this test cannot reproduce portably — `ctime` cannot be
/// set from userspace. Under it the mask is deterministic, which is what makes
/// this a regression test rather than a coin flip.
///
/// Returns whether the mask actually took effect.
fn poison_with_stat_cache_mask(ws: &Path) -> bool {
    use std::fs::FileTimes;
    use std::time::{Duration, SystemTime};

    run_git(ws, &["config", "core.checkStat", "minimal"]);

    let victim = ws.join(VICTIM);
    let backdated = SystemTime::now() - Duration::from_mins(10);
    let times = FileTimes::new()
        .set_accessed(backdated)
        .set_modified(backdated);
    std::fs::File::options()
        .write(true)
        .open(&victim)
        .expect("open victim")
        .set_times(times)
        .expect("back-date victim");

    let repo = maw_git::GixRepo::open(ws).expect("open workspace repo");
    let head = repo
        .rev_parse("HEAD")
        .expect("rev-parse HEAD in the workspace");
    repo.set_head_detached(head).expect("set_head_detached");
    repo.unstage_all().expect("unstage_all");
    // Force the index to record the back-dated stat for the (still correct)
    // blob, which is the state the corrupter leaves behind.
    run_git(ws, &["update-index", "--refresh"]);

    std::fs::write(&victim, STALE).expect("write stale bytes");
    std::fs::File::options()
        .write(true)
        .open(&victim)
        .expect("reopen victim")
        .set_times(times)
        .expect("restore back-dated mtime");

    git_stdout(ws, &["status", "--porcelain"]).trim().is_empty()
}

/// The regression: stale bytes plus an index rewrite must still be caught,
/// preserved and repaired.
#[test]
fn materialize_verify_survives_index_stat_cache_refresh() {
    let td = tempfile::tempdir().expect("tempdir");
    let ws_name = "bn-3gba-mask";
    let (root, ws) = setup(td.path(), ws_name);

    let masked = poison_with_stat_cache_mask(&ws);
    assert!(
        masked,
        "the fixture failed to produce the mask: `git status --porcelain` already \
         reports the divergence, so this run would NOT be testing what it claims. \
         Fix the fixture (see poison_with_stat_cache_mask) rather than weakening \
         the assertion — a status-based detector passing here would be a false \
         green on the exact class bn-3gba exists to catch."
    );

    // Sanity: the working tree really is wrong, whatever git's status says.
    assert_eq!(
        std::fs::read_to_string(ws.join(VICTIM)).expect("read victim"),
        STALE,
        "test setup must leave the stale bytes on disk"
    );

    let record = verify_clean_materialization(&root, ws_name, &ws, MaterializeOp::Create).expect(
        "the verifier MUST detect tracked divergence even when the index stat cache masks it",
    );

    assert_eq!(record.paths.len(), 1, "{:?}", record.paths);
    assert_eq!(record.paths[0].path, VICTIM);
    assert_eq!(record.paths[0].status, "M");
    assert!(record.paths[0].repaired, "{:?}", record.paths[0]);
    assert_eq!(record.repaired_count, 1);
    assert!(
        record.residual_paths.is_empty(),
        "post-repair re-verification must ALSO be tree-based (the repair just \
         rewrote the index, so a stat-based re-check is the likeliest place to \
         be masked): {:?}",
        record.residual_paths
    );

    // Prime Invariant: the pre-repair bytes were pinned before being overwritten.
    let pinned = record
        .preserved_ref
        .as_deref()
        .expect("pre-repair bytes must be pinned to a recovery ref");
    assert!(
        pinned.starts_with("refs/manifold/recovery/"),
        "unexpected pin namespace: {pinned}"
    );
    assert_eq!(
        git_stdout(&root, &["show", &format!("{pinned}:{VICTIM}")]),
        STALE,
        "the pin must hold the PRE-repair (stale) bytes"
    );

    // Repaired byte-for-byte from HEAD.
    assert_eq!(
        std::fs::read_to_string(ws.join(VICTIM)).expect("read repaired victim"),
        GOOD,
    );
}

/// The negative control for the same code path: a genuinely clean workspace
/// whose index was ALSO just rewritten must produce no record at all. Without
/// this, "detects everything" would trivially satisfy the test above.
#[test]
fn clean_workspace_with_rewritten_index_is_not_divergence() {
    let td = tempfile::tempdir().expect("tempdir");
    let ws_name = "bn-3gba-mask-clean";
    let (root, ws) = setup(td.path(), ws_name);

    let repo = maw_git::GixRepo::open(&ws).expect("open workspace repo");
    let head = repo.rev_parse("HEAD").expect("rev-parse HEAD");
    repo.set_head_detached(head).expect("set_head_detached");
    repo.unstage_all().expect("unstage_all");

    assert!(
        verify_clean_materialization(&root, ws_name, &ws, MaterializeOp::Create).is_none(),
        "a clean workspace must produce no divergence record"
    );
}

/// Untracked scratch must survive the tree-based detector too: it appears as an
/// `A` entry in the tree diff (present on disk, absent from HEAD), and
/// "repairing" it would mean DELETING it.
#[test]
fn untracked_scratch_is_not_divergence_under_tree_comparison() {
    let td = tempfile::tempdir().expect("tempdir");
    let ws_name = "bn-3gba-mask-scratch";
    let (root, ws) = setup(td.path(), ws_name);

    let scratch = ws.join("agent-scratch.md");
    std::fs::write(&scratch, "notes\n").expect("write scratch");

    assert!(
        verify_clean_materialization(&root, ws_name, &ws, MaterializeOp::Create).is_none(),
        "an untracked file is not divergence"
    );
    assert!(
        scratch.exists(),
        "untracked scratch must never be deleted by the repair"
    );
}
