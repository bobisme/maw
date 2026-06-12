//! Integration tests for bn-29z8: silent commit abandonment fixes.
//!
//! Three defects fixed:
//!
//! A. `sync_worktree_to_epoch_inner` now refuses (with a named-SHA error) when
//!    HEAD has commits not in the target epoch's history, and surfaces any
//!    "leaving commits behind" git warning on stderr.
//!
//! B. `auto_sync_if_stale` (the `maw exec` pre-hook) now acquires the per-
//!    workspace rebase lock before reading HEAD, then passes the captured OID
//!    as an `expected_head_hex` CAS guard into `sync_worktree_to_epoch`. If
//!    HEAD moved (concurrent git commit) between the decision and the checkout,
//!    the sync is skipped and the commit survives.
//!
//! C. exec auto-sync continues to mutate HEAD (smallest behavior change),
//!    but every mutation prints what it did ("auto-syncing..."). This file
//!    verifies that the mutation is guarded and loud.

mod manifold_common;

use fs4::fs_std::FileExt as _;
use manifold_common::TestRepo;

// ---------------------------------------------------------------------------
// Defect A: ancestor-refusal
// ---------------------------------------------------------------------------

/// bn-29z8-A1: when auto-sync is triggered on a workspace whose HEAD has a
/// diverged commit (not in the epoch's ancestry), the sync must be SKIPPED
/// rather than silently orphaning the commit.
///
/// This is the exact scenario from the sigil incident (bn-3d4a):
///   1. Worker commits in workspace (HEAD → C1).
///   2. Another workspace merges, advancing epoch past C1's parent.
///   3. `maw exec ws -- git status` fires auto-sync.
///   4. OLD behavior: git checkout --detach epoch → C1 orphaned silently.
///   5. NEW behavior: auto-sync detects C1 is not an ancestor of epoch →
///      skips sync, warns on stderr, C1 preserved.
#[test]
fn auto_sync_skips_and_warns_when_head_has_unmerged_commits() {
    let repo = TestRepo::new();

    // Worker commits in the workspace.
    repo.maw_ok(&["ws", "create", "worker"]);
    repo.add_file("worker", "work.txt", "precious work\n");
    repo.git_in_workspace("worker", &["add", "work.txt"]);
    repo.git_in_workspace("worker", &["commit", "-m", "feat: precious work"]);
    let commit_sha = repo.workspace_head("worker");

    // A peer merges, advancing the epoch PAST worker's base without including
    // worker's commit. Use --no-auto-rebase to produce the exact TOCTOU shape:
    // worker is stale AND has a diverged commit.
    repo.maw_ok(&["ws", "create", "peer"]);
    repo.add_file("peer", "peer.txt", "peer work\n");
    repo.git_in_workspace("peer", &["add", "peer.txt"]);
    repo.git_in_workspace("peer", &["commit", "-m", "peer: advance epoch"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "peer",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge peer",
    ]);

    // At this point: worker is stale AND has a diverged commit (not in epoch).
    // Trigger auto-sync via `maw exec worker -- git status`.
    let out = repo.maw_raw(&["exec", "worker", "--", "git", "status", "--short"]);
    assert!(
        out.status.success(),
        "exec should still succeed (auto-sync skip is non-fatal)\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // The precious commit must still be HEAD — not orphaned.
    let head_after = repo.workspace_head("worker");
    assert_eq!(
        head_after, commit_sha,
        "auto-sync must not orphan a committed SHA that isn't in the epoch's ancestry"
    );

    // The work file must still exist.
    assert_eq!(
        repo.read_file("worker", "work.txt").as_deref(),
        Some("precious work\n"),
        "precious work must survive auto-sync skip"
    );

    // The skip warning should appear on stderr.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        // Either the existing "Skipping auto-sync to preserve committed work"
        // message fires (already handled by committed_ahead_of_epoch), OR the
        // new ancestor-refusal message fires — both are acceptable. The key
        // property is that HEAD didn't change.
        stderr.contains("Skipping auto-sync")
            || stderr.contains("Refusing to sync")
            || stderr.contains("skipped"),
        "expected a skip/refusal notice on stderr, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Defect A (belt+braces): regression — normal stale+clean auto-sync still
// works after the ancestor check is added.
// ---------------------------------------------------------------------------

/// bn-29z8 regression guard: a stale+clean workspace (no diverged commits)
/// should still auto-sync successfully.
#[test]
fn auto_sync_still_works_for_clean_stale_workspace() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "worker"]);

    // Advance epoch via default workspace (worker has no commits of its own).
    repo.add_file("default", "advance.txt", "epoch advance\n");
    repo.advance_epoch("chore: advance epoch");

    let old_head = repo.workspace_head("worker");
    let new_epoch = repo.current_epoch();
    assert_ne!(old_head, new_epoch, "worker should be stale");

    // Trigger auto-sync.
    let out = repo.maw_raw(&["exec", "worker", "--", "git", "rev-parse", "HEAD"]);
    assert!(
        out.status.success(),
        "exec should succeed on clean stale workspace\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // HEAD should now equal the new epoch.
    let new_head = repo.workspace_head("worker");
    assert_eq!(
        new_head, new_epoch,
        "clean stale workspace should auto-sync to new epoch"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("auto-syncing"),
        "expected auto-sync announcement on stderr, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Defect A (error message): ws sync refuses + names orphaned SHA
// ---------------------------------------------------------------------------

/// bn-29z8-A2: `maw ws sync` (explicit, not auto) on a workspace with
/// diverged commits and `--no-rebase` should refuse clearly.
/// (Note: default `maw ws sync` now rebases; `--no-rebase` is the refusal
/// path. But the INNER ancestor-refusal in `sync_worktree_to_epoch_inner`
/// is only reached by the no-rebase path AFTER the `committed_ahead` check
/// passes — which means this test verifies the rebase path still handles
/// the case correctly and doesn't regress.)
///
/// More importantly: the `sync_worktree_to_epoch` function-level ancestor
/// check fires when the `committed_ahead` check is bypassed (e.g. when HEAD
/// has moved after the check). The unit tests in `checks.rs` cover this
/// directly; this integration test covers the end-to-end `ws sync` flow.
#[test]
fn ws_sync_no_rebase_refuses_diverged_workspace_with_clear_message() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "feature"]);
    repo.add_file("feature", "work.txt", "precious\n");
    repo.git_in_workspace("feature", &["add", "work.txt"]);
    repo.git_in_workspace("feature", &["commit", "-m", "feat: precious"]);
    let commit_sha = repo.workspace_head("feature");

    // Advance epoch via another workspace.
    repo.maw_ok(&["ws", "create", "advancer"]);
    repo.add_file("advancer", "epoch.txt", "advance\n");
    repo.git_in_workspace("advancer", &["add", "epoch.txt"]);
    repo.git_in_workspace("advancer", &["commit", "-m", "chore: advance"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge advancer",
    ]);

    // maw ws sync --no-rebase must refuse and not change HEAD.
    let out = repo.maw_raw(&["ws", "sync", "feature", "--no-rebase"]);
    assert!(
        !out.status.success(),
        "sync --no-rebase must fail when commits are ahead\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let head_after = repo.workspace_head("feature");
    assert_eq!(
        head_after, commit_sha,
        "HEAD must not change after refused sync"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    // Either the --no-rebase refusal fires (committed_ahead > 0) or the
    // ancestor-refusal fires (HEAD has diverged commits).
    assert!(
        stderr.contains("--no-rebase would discard committed work")
            || stderr.contains("Refusing to sync")
            || stderr.contains("would orphan"),
        "expected a clear refusal message, got stderr: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Defect B: auto-sync lock acquisition — lock already held → skip
// ---------------------------------------------------------------------------

/// bn-29z8-B: if the workspace rebase lock is already held when auto-sync
/// fires, the sync should be skipped (not blocking or failing the user's
/// command). The command runs against the current HEAD.
///
/// We simulate "lock held" by acquiring the lock file directly in the test
/// process before the maw invocation.
#[test]
fn auto_sync_skips_when_workspace_lock_is_held() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "worker"]);

    // Advance epoch so worker is stale.
    repo.add_file("default", "advance.txt", "epoch advance\n");
    repo.advance_epoch("chore: advance epoch");

    let old_head = repo.workspace_head("worker");

    // Acquire the rebase lock before running maw exec.
    let lock_dir = repo.root().join(".manifold").join("locks").join("rebase");
    std::fs::create_dir_all(&lock_dir).expect("create lock dir");
    let lock_file = lock_dir.join("worker.lock");
    let lock_fd = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_file)
        .expect("open lock file");
    lock_fd.try_lock_exclusive().expect("acquire lock in test");

    // Now run maw exec — auto-sync should detect the held lock and skip.
    let out = repo.maw_raw(&["exec", "worker", "--", "git", "rev-parse", "HEAD"]);
    drop(lock_fd); // Release the lock after the test command.

    assert!(
        out.status.success(),
        "exec should succeed even when lock is held (skip not fail)\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // HEAD must not have changed — auto-sync was skipped.
    let head_after = repo.workspace_head("worker");
    assert_eq!(
        head_after, old_head,
        "HEAD must not change when auto-sync is skipped due to held lock"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("skipped") || stderr.contains("lock held"),
        "expected a skip notice on stderr, got: {stderr}"
    );
}
