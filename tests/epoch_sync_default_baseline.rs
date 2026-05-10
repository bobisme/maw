//! Regression test for bn-3r8s.
//!
//! When the user committed directly to the default workspace's branch and
//! then ran `maw epoch sync`, only `refs/manifold/epoch/current` advanced —
//! `refs/manifold/epoch/ws/default` stayed at the OLD epoch. The next
//! `maw ws merge` then anchored HEAD at the stale per-workspace baseline
//! during snapshot/replay, treated the direct commit's content as
//! "uncommitted local edits", and double-applied it onto the merge result —
//! producing diff3 markers wrapping a duplicated copy of the file in the
//! worktree (the COMMIT itself was clean; only the worktree was corrupted).
//!
//! Fix: `maw epoch sync` now advances the default workspace's per-workspace
//! epoch ref alongside `refs/manifold/epoch/current`. The merge precondition
//! path (FF-absorb) was patched in parallel to advance the target's ref too,
//! so the same shape can't sneak through that path either.

mod manifold_common;

use manifold_common::{TestRepo, git_ok};

/// End-to-end recipe matching the original bn-4c6g report: direct commit
/// on default, manual `maw epoch sync`, sibling workspace merge — assert
/// the worktree is NOT inflated with diff3 markers afterwards.
#[test]
fn epoch_sync_advances_default_baseline_so_merge_does_not_duplicate() {
    let repo = TestRepo::new();
    repo.seed_files(&[("payload.txt", "BASE LINE 1\nBASE LINE 2\nBASE LINE 3\n")]);

    // Sibling workspace edits the file (commits a divergent change).
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.modify_file(
        "alice",
        "payload.txt",
        "BASE LINE 1\nALICE EDIT\nBASE LINE 2\nBASE LINE 3\n",
    );
    repo.git_in_workspace("alice", &["add", "payload.txt"]);
    repo.git_in_workspace("alice", &["commit", "-m", "alice edit"]);

    // Direct commit on default's branch (advances main, leaves
    // refs/manifold/epoch/ws/default at the old epoch in the buggy version).
    let default_path = repo.default_workspace();
    std::fs::write(
        default_path.join("payload.txt"),
        "BASE LINE 1\nBASE LINE 2\nBASE LINE 3\nDEFAULT EDIT\n",
    )
    .expect("write payload");
    git_ok(&default_path, &["add", "payload.txt"]);
    git_ok(&default_path, &["commit", "-m", "direct edit on default"]);

    // Run `maw epoch sync` to absorb the direct commit into the global
    // epoch. The fix makes this also advance the default's per-workspace
    // epoch ref.
    repo.maw_ok(&["epoch", "sync"]);

    // Sync alice onto the new epoch (clean rebase, both edits land on
    // disjoint lines).
    repo.maw_ok(&["ws", "sync", "alice"]);

    // Merge alice into default. With the fix in place, default's HEAD ==
    // its per-workspace baseline, so the snapshot/replay anchor logic
    // doesn't mistakenly snapshot the direct-commit content as a local
    // delta. Without the fix, this produces a worktree file with diff3
    // markers wrapping ~2x the merged tree size.
    repo.maw_ok(&[
        "ws",
        "merge",
        "alice",
        "--destroy",
        "--message",
        "merge alice",
    ]);

    let merged = repo
        .read_file("default", "payload.txt")
        .expect("payload.txt should exist after merge");

    assert!(
        !merged.contains("<<<<<<<"),
        "default worktree must NOT contain diff3 conflict markers after a clean rebase + merge.\n\
         Buggy versions inflated the file ~2x with markers wrapping duplicated content.\n\
         Got:\n{merged}",
    );
    assert!(
        !merged.contains("|||||||"),
        "default worktree must NOT contain diff3 base markers.\nGot:\n{merged}",
    );
    assert!(
        merged.contains("ALICE EDIT"),
        "alice's edit should be present in the merged worktree.\nGot:\n{merged}",
    );
    assert!(
        merged.contains("DEFAULT EDIT"),
        "default's direct edit should be preserved in the merged worktree.\nGot:\n{merged}",
    );
}

