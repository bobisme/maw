//! bn-2byw (step 3): a REAL two-`maw`-process flock mutual-exclusion test.
//!
//! maw serializes concurrent `maw ws sync --rebase <ws>` runs on the same
//! workspace with an advisory OS file lock (see
//! `crates/maw-cli/src/workspace/sync/lock.rs`: `WorkspaceRebaseLock::try_acquire`,
//! fs4 `try_lock_exclusive`, lockfile `<root>/.manifold/locks/rebase/<ws>.lock`).
//! On contention `rebase_workspace_run` bails with
//! "Another rebase is in progress for workspace '<ws>'".
//!
//! Neither the bn-2byw production-code DST tier nor the SG1 in-proc soak is
//! multi-process, so NEITHER exercises this OS-level *cross-process* mutual
//! exclusion. The race-feasibility spike
//! (`notes/sg1-race-feasibility-spike-bn-3ny7.md`) explicitly calls out a
//! "tiny real two-maw-process flock mutual-exclusion test" as the one guarantee
//! a deterministic single-process model can't cover. This is that test.
//!
//! # How it is made deterministic (no real race window to lose)
//!
//! We do NOT depend on two processes happening to collide in a microsecond
//! window. Instead we *widen the lock-hold window* with a failpoint:
//!
//!  - Process 1 runs with `MAW_FP="FP_REBASE_BEFORE_SETHEAD=sleep:<HOLD_MS>"`.
//!    `FP_REBASE_BEFORE_SETHEAD` fires inside `rebase_workspace_run` AFTER the
//!    rebase lock is acquired (and after the worktree/walk work) but BEFORE
//!    `set_head`, so process 1 holds the flock for ~`HOLD_MS` ms.
//!  - We give process 1 a head start, then launch process 2 with no failpoint.
//!    While process 1 sleeps holding the lock, process 2's `try_acquire`
//!    returns `WouldBlock` → maw bails with "Another rebase is in progress".
//!
//! Because the hold window is ~seconds (not microseconds), the outcome is
//! deterministic and robust against scheduler jitter.
//!
//! # Why a custom (failpoints-enabled) binary
//!
//! `MAW_FP` and every `fp!()` site are gated behind `--features failpoints`;
//! the default test binary (`target/<profile>/maw`, what `manifold_common`
//! invokes) has none of them, so it can't be made to hold the lock. This test
//! therefore builds and locates a failpoints-enabled `maw` explicitly (see
//! `failpoints_maw_bin`), keeping the test self-contained: it works whether or
//! not the caller pre-built that binary.

mod manifold_common;

use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use manifold_common::TestRepo;

/// How long process 1 holds the rebase lock (ms). Generous so process 2 is
/// guaranteed to attempt acquisition while the lock is still held.
const HOLD_MS: u64 = 2500;

/// Head start for process 1 before launching process 2 (ms). Long enough for
/// process 1 to spawn, parse args, do the pre-set_head work, and enter the
/// `FP_REBASE_BEFORE_SETHEAD` sleep (i.e. to be *holding* the lock).
const GAP_MS: u64 = 600;

/// Build (once per test process) and locate a `--features failpoints` `maw`
/// binary, returning its absolute path.
///
/// The default `manifold_common::maw_bin()` returns the plain binary, which has
/// no failpoint machinery. We build the failpoints variant into a *separate*
/// target dir so we never clobber the plain `target/<profile>/maw` other tests
/// (and `just check`) rely on. Memoized so repeated calls in one test process
/// build at most once.
fn failpoints_maw_bin() -> &'static std::path::Path {
    static BIN: OnceLock<std::path::PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        // Dedicated target dir so the failpoints build does not overwrite the
        // plain binary used by the rest of the suite.
        let target_dir = manifest_dir.join("target").join("flock-fp-bn-2byw");

        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "maw-cli",
                "--features",
                "failpoints",
                "--target-dir",
            ])
            .arg(&target_dir)
            .current_dir(&manifest_dir)
            .status()
            .expect("failed to spawn `cargo build` for the failpoints binary");
        assert!(
            status.success(),
            "`cargo build -p maw-cli --features failpoints` failed; cannot run \
             the flock mutual-exclusion test"
        );

        let bin = target_dir.join("debug").join("maw");
        assert!(
            bin.exists(),
            "failpoints maw binary not found at {} after build",
            bin.display()
        );
        bin
    })
    .as_path()
}

/// Set up a workspace `feat` that is exactly 1 commit AHEAD of the epoch, so a
/// subsequent `maw ws sync --rebase feat` takes the real rebase path (acquiring
/// the lock + passing through `FP_REBASE_BEFORE_SETHEAD`), not a trivial
/// fast-forward. Returns the committed-ahead HEAD OID.
fn setup_committed_ahead_feat(repo: &TestRepo) -> String {
    repo.seed_files(&[("base.txt", "base content\n")]);

    repo.maw_ok(&["ws", "create", "feat"]);

    // Commit work in `feat` (ahead of its base epoch).
    repo.add_file("feat", "precious.txt", "precious work\n");
    repo.git_in_workspace("feat", &["add", "precious.txt"]);
    repo.git_in_workspace("feat", &["commit", "-m", "feat: precious work"]);
    let committed_head = repo.workspace_head("feat");

    // Advance the global epoch (simulate another agent's merge landing), so
    // `feat` is now BOTH committed-ahead AND stale → real rebase path.
    repo.add_file("default", "epoch-advance.txt", "epoch content\n");
    let new_epoch = repo.advance_epoch("chore: advance epoch (another agent)");
    assert_ne!(
        committed_head, new_epoch,
        "committed_head must differ from the advanced epoch"
    );

    committed_head
}

/// THE TEST: two real `maw ws sync --rebase feat` processes contend on the
/// per-workspace rebase flock. Process 1 holds the lock (sleep failpoint);
/// process 2 must be REFUSED with "Another rebase is in progress" — proving the
/// OS-level cross-process mutual exclusion holds — and the committed-ahead work
/// must survive (Prime Invariant).
#[test]
#[ignore = "heavyweight: builds a --features failpoints maw binary; run via just sg1-flock-test"]
fn two_maw_processes_contend_on_rebase_flock() {
    let repo = TestRepo::new();
    let committed_head = setup_committed_ahead_feat(&repo);

    let bin = failpoints_maw_bin();
    let args = ["ws", "sync", "--rebase", "feat"];

    // --- Process 1: holds the rebase lock for ~HOLD_MS via the failpoint. ---
    // spawn() is non-blocking: it returns a Child while proc1 sleeps holding
    // the lock.
    let proc1 = Command::new(bin)
        .args(args)
        .current_dir(repo.root())
        .env(
            "MAW_FP",
            format!("FP_REBASE_BEFORE_SETHEAD=sleep:{HOLD_MS}"),
        )
        .spawn()
        .expect("failed to spawn process 1 (lock holder)");

    // Give process 1 a head start so it has acquired the lock and entered the
    // failpoint sleep before process 2 attempts acquisition.
    std::thread::sleep(Duration::from_millis(GAP_MS));

    // --- Process 2: same args, NO failpoint. Should be refused immediately. ---
    let proc2 = Command::new(bin)
        .args(args)
        .current_dir(repo.root())
        // Explicitly clear MAW_FP so proc2 never sleeps / never holds.
        .env_remove("MAW_FP")
        .output()
        .expect("failed to run process 2 (contender)");

    let p2_stdout = String::from_utf8_lossy(&proc2.stdout);
    let p2_stderr = String::from_utf8_lossy(&proc2.stderr);
    let p2_combined = format!("{p2_stdout}\n{p2_stderr}");

    // Evidence for `--nocapture` runs: show that the contender was refused.
    eprintln!(
        "[flock-bn-2byw] process 2 exit success={}, output:\n{}",
        proc2.status.success(),
        p2_combined.trim()
    );

    // --- Now wait for process 1 to finish and assert it succeeded. ---
    let proc1_out = proc1
        .wait_with_output()
        .expect("failed to wait on process 1");
    let p1_stdout = String::from_utf8_lossy(&proc1_out.stdout);
    let p1_stderr = String::from_utf8_lossy(&proc1_out.stderr);

    // ===== ASSERTIONS =====

    // (1) MUTUAL EXCLUSION: process 2 was refused while process 1 held the lock.
    assert!(
        p2_combined.contains("Another rebase is in progress"),
        "MUTUAL-EXCLUSION FAILURE: process 2 was NOT refused while process 1 \
         held the rebase lock.\n\
         If proc2 ran the rebase concurrently, the flock did not serialize them.\n\
         --- proc2 stdout ---\n{p2_stdout}\n--- proc2 stderr ---\n{p2_stderr}\n\
         --- proc1 stdout ---\n{p1_stdout}\n--- proc1 stderr ---\n{p1_stderr}"
    );
    assert!(
        !proc2.status.success(),
        "MUTUAL-EXCLUSION FAILURE: process 2 exited 0 (success) but should have \
         been refused (non-zero) while the lock was held.\n\
         --- proc2 stdout ---\n{p2_stdout}\n--- proc2 stderr ---\n{p2_stderr}"
    );

    // (2) Process 1 (the lock holder) completed its rebase successfully.
    assert!(
        proc1_out.status.success(),
        "process 1 (lock holder) should have completed its rebase successfully.\n\
         --- proc1 stdout ---\n{p1_stdout}\n--- proc1 stderr ---\n{p1_stderr}"
    );

    // (3) PRIME INVARIANT: the committed-ahead work is preserved regardless of
    //     which process did what. The file must still be on disk with its
    //     content, and HEAD must still carry exactly one commit ahead of the
    //     epoch (the rebased `precious work`).
    let content = repo.read_file("feat", "precious.txt");
    assert_eq!(
        content.as_deref(),
        Some("precious work\n"),
        "PRIME INVARIANT VIOLATION: precious.txt must survive cross-process \
         rebase contention — committed work must never be orphaned"
    );

    // After a successful rebase the original OID becomes a fresh cherry-picked
    // commit, so HEAD differs from the pre-rebase committed OID and from the
    // epoch, but still carries exactly one commit of exclusive work.
    let new_epoch = repo.current_epoch();
    let head_after = repo.workspace_head("feat");
    assert_ne!(
        head_after, new_epoch,
        "HEAD should be a rebased commit on top of the epoch (not the epoch \
         itself); if equal the committed work was orphaned"
    );

    let commits_ahead_str = repo.git_in_workspace(
        "feat",
        &["rev-list", "--count", &format!("{new_epoch}..HEAD")],
    );
    let commits_ahead: u32 = commits_ahead_str.trim().parse().unwrap_or(0);
    assert_eq!(
        commits_ahead, 1,
        "feat should carry exactly 1 rebased commit on top of the epoch after \
         contention, got {commits_ahead}"
    );

    // The original committed OID was rebased (new OID), confirming proc1 really
    // ran the guarded rebase path that acquired the lock.
    assert_ne!(
        head_after, committed_head,
        "HEAD should be a new cherry-picked OID after the rebase (not the \
         original committed OID)"
    );
}
