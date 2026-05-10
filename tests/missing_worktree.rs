//! Integration test for MISSING-worktree detection (bn-3fhj).
//!
//! When a user manually `rm -rf`s a workspace's worktree directory,
//! the CLI registry / git worktree admin dir / .manifold metadata can
//! continue to advertise the workspace as "ready to merge". The real
//! merge then errors with a confusing "does not exist" message.
//!
//! This test asserts:
//!   1. `maw ws list` surfaces a distinct MISSING state (not "ready to merge").
//!   2. `maw ws merge --check` fails with a MISSING diagnostic (not a generic
//!      "does not exist" surprise at real-merge time).
//!   3. `maw ws destroy <name> --force` cleans up registry + metadata so the
//!      stale entry stops showing up in subsequent `ws list` output.

#![allow(clippy::all, clippy::pedantic, clippy::nursery)]

mod manifold_common;

use manifold_common::TestRepo;

#[test]
fn missing_worktree_is_reported_and_recoverable() {
    let repo = TestRepo::new();

    repo.seed_files(&[("README.md", "# Project\n")]);

    repo.maw_ok(&["ws", "create", "kill-test"]);

    // Make a commit so the workspace would otherwise be "ready to merge".
    repo.add_file("kill-test", "feature.txt", "feature\n");
    repo.git_in_workspace("kill-test", &["add", "-A"]);
    repo.git_in_workspace("kill-test", &["commit", "-m", "test commit"]);

    // Sanity: pre-removal, ws list should say "ready to merge".
    let pre = repo.maw_ok(&["ws", "list"]);
    assert!(
        pre.contains("ready to merge"),
        "Setup expectation: pre-removal `ws list` should advertise ready-to-merge: {pre}"
    );

    // Manually remove the worktree dir, leaving registry/metadata behind.
    let ws_path = repo.workspace_path("kill-test");
    std::fs::remove_dir_all(&ws_path)
        .expect("removing kill-test worktree dir for test should succeed");
    assert!(!ws_path.exists(), "worktree dir should be removed");

    // 1. `ws list` should now surface MISSING and NOT advertise ready-to-merge.
    let list_out = repo.maw_ok(&["ws", "list"]);
    assert!(
        list_out.contains("MISSING"),
        "`ws list` should surface MISSING state: {list_out}"
    );
    assert!(
        !list_out.contains("(ready to merge)"),
        "`ws list` must not advertise a missing workspace as ready to merge: {list_out}"
    );
    assert!(
        list_out.contains("maw ws destroy kill-test --force"),
        "`ws list` should point users at the recovery command: {list_out}"
    );

    // 2. `merge --check` should fail cleanly with a MISSING diagnostic.
    let check_err = repo.maw_fails(&["ws", "merge", "kill-test", "--into", "default", "--check"]);
    assert!(
        check_err.contains("MISSING"),
        "`merge --check` should fail with MISSING diagnostic: {check_err}"
    );
    assert!(
        check_err.contains("maw ws destroy kill-test --force"),
        "`merge --check` MISSING diagnostic should include recovery hint: {check_err}"
    );

    // 3. `destroy --force` should clean up the residual registry/metadata.
    let destroy_out = repo.maw_ok(&["ws", "destroy", "kill-test", "--force"]);
    assert!(
        destroy_out.contains("cleaned up") || destroy_out.contains("destroyed"),
        "`destroy --force` on missing-on-disk workspace should succeed and clean up: {destroy_out}"
    );

    // After destroy, `ws list` must no longer mention kill-test.
    let post = repo.maw_ok(&["ws", "list"]);
    assert!(
        !post.contains("kill-test"),
        "After `destroy --force`, `ws list` must not still advertise the missing workspace: {post}"
    );
}
