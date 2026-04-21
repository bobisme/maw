//! Property-based tests for the `ws sync` decision gate (bn-3o7w).
//!
//! These tests fuzz random sequences of (workspace_create, local_commit*,
//! concurrent_merge*, sync) and assert the core safety invariants that were
//! violated by bn-18dj:
//!
//! 1. `sync` (without `--rebase`) on a workspace with local commits must NEVER
//!    change HEAD and must NEVER drop committed content.
//! 2. `sync --rebase` must preserve every commit from `base..HEAD_before` —
//!    the count ahead of the new epoch after rebase must equal the count ahead
//!    of the old base before rebase.
//! 3. Auto-sync triggered by `maw exec` must never drop local commits.
//! 4. `sync --all` must never drop local commits in any workspace.
//!
//! If any of these properties fail, file a bone — do NOT fix here.

#![cfg(not(miri))]

mod manifold_common;

use manifold_common::TestRepo;
use proptest::prelude::*;

/// Advance the default-branch epoch by creating a throwaway workspace, committing
/// a unique file, and merging it back.
fn advance_epoch(repo: &TestRepo, tag: &str, idx: usize) {
    let name = format!("adv-{tag}-{idx}");
    repo.maw_ok(&["ws", "create", &name]);
    repo.add_file(
        &name,
        &format!("epoch_{tag}_{idx}.txt"),
        &format!("epoch {tag} {idx}\n"),
    );
    repo.git_in_workspace(&name, &["add", "-A"]);
    repo.git_in_workspace(&name, &["commit", "-m", &format!("advance {tag} {idx}")]);
    repo.maw_ok(&[
        "ws",
        "merge",
        &name,
        "--destroy",
        "--message",
        &format!("merge advance {tag} {idx}"),
    ]);
}

/// Create `n` local commits in `ws_name`, each touching a distinct file.
fn make_local_commits(repo: &TestRepo, ws_name: &str, n: usize, tag: &str) {
    for i in 0..n {
        let file = format!("local_{tag}_{i}.txt");
        repo.add_file(ws_name, &file, &format!("local {tag} {i}\n"));
        repo.git_in_workspace(ws_name, &["add", "-A"]);
        repo.git_in_workspace(ws_name, &["commit", "-m", &format!("local {tag} {i}")]);
    }
}

/// Count commits reachable from HEAD but not from `base`.
fn commits_ahead(repo: &TestRepo, ws_name: &str, base: &str) -> usize {
    let out = repo.git_in_workspace(ws_name, &["rev-list", "--count", &format!("{base}..HEAD")]);
    out.trim().parse().unwrap_or(0)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 32,
        // Each case spins up a TestRepo and runs several maw commands, so keep
        // cases bounded. 32 cases finishes well under 60s on this machine.
        .. ProptestConfig::default()
    })]

    /// Property 1: `sync` (without `--rebase`) must not change HEAD when the
    /// workspace has local commits, no matter how many epoch advances happen.
    #[test]
    fn sync_without_rebase_never_changes_head(
        local_commits in 1usize..=4,
        epoch_advances in 1usize..=3,
    ) {
        let repo = TestRepo::new();
        repo.maw_ok(&["ws", "create", "feature"]);
        make_local_commits(&repo, "feature", local_commits, "a");

        let head_before = repo.workspace_head("feature");

        for i in 0..epoch_advances {
            advance_epoch(&repo, "a", i);
        }

        // Run sync — must not drop commits.
        let _ = repo.maw_raw(&["ws", "sync", "feature"]);

        let head_after = repo.workspace_head("feature");
        prop_assert_eq!(
            head_before.clone(),
            head_after,
            "sync without --rebase must not change HEAD"
        );

        // All local files must still be present.
        for i in 0..local_commits {
            let file = format!("local_a_{i}.txt");
            prop_assert!(
                repo.read_file("feature", &file).is_some(),
                "local file {} missing after sync (refused path)",
                file
            );
        }
    }

    /// Property 2: `sync --rebase` must preserve every commit from base..HEAD.
    /// After rebase, the workspace should have the same number of commits ahead
    /// of the new epoch as it had before ahead of the old base.
    #[test]
    fn sync_rebase_preserves_commit_count_and_content(
        local_commits in 1usize..=4,
        epoch_advances in 1usize..=3,
    ) {
        let repo = TestRepo::new();
        repo.maw_ok(&["ws", "create", "feature"]);

        // Capture the base epoch (what the workspace was created from).
        let base_epoch = repo.current_epoch();

        make_local_commits(&repo, "feature", local_commits, "b");

        let ahead_before = commits_ahead(&repo, "feature", &base_epoch);
        prop_assert_eq!(ahead_before, local_commits);

        for i in 0..epoch_advances {
            advance_epoch(&repo, "b", i);
        }

        let out = repo.maw_raw(&["ws", "sync", "feature", "--rebase"]);
        prop_assert!(
            out.status.success(),
            "sync --rebase should succeed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );

        // All local files must still be present.
        for i in 0..local_commits {
            let file = format!("local_b_{i}.txt");
            prop_assert!(
                repo.read_file("feature", &file).is_some(),
                "local file {} missing after sync --rebase",
                file
            );
        }

        // Every epoch-advance file must be present (merge fast-forwarded onto HEAD).
        for i in 0..epoch_advances {
            let file = format!("epoch_b_{i}.txt");
            prop_assert!(
                repo.read_file("feature", &file).is_some(),
                "epoch file {} missing after sync --rebase",
                file
            );
        }

        // Count ahead of new epoch must equal local commit count.
        let new_epoch = repo.current_epoch();
        let ahead_after = commits_ahead(&repo, "feature", &new_epoch);
        prop_assert_eq!(
            ahead_after,
            local_commits,
            "commit count ahead of new epoch after rebase must equal local_commits"
        );
    }

    /// Property 3: `maw exec` triggers auto-sync. Auto-sync must never drop
    /// commits from a stale workspace that has local work.
    #[test]
    fn auto_sync_via_exec_never_drops_commits(
        local_commits in 1usize..=3,
        epoch_advances in 1usize..=2,
    ) {
        let repo = TestRepo::new();
        repo.maw_ok(&["ws", "create", "feature"]);
        make_local_commits(&repo, "feature", local_commits, "c");

        let head_before = repo.workspace_head("feature");

        for i in 0..epoch_advances {
            advance_epoch(&repo, "c", i);
        }

        // maw exec should trigger internal auto-sync. It must not clobber HEAD.
        let _ = repo.maw_raw(&["exec", "feature", "--", "git", "status"]);

        let head_after = repo.workspace_head("feature");
        prop_assert_eq!(
            head_before.clone(),
            head_after,
            "auto-sync via exec must not change HEAD when workspace has local commits"
        );

        for i in 0..local_commits {
            let file = format!("local_c_{i}.txt");
            prop_assert!(
                repo.read_file("feature", &file).is_some(),
                "local file {} missing after auto-sync",
                file
            );
        }
    }

    /// Property 4: `ws sync --all` must never drop commits in any workspace,
    /// even across multiple workspaces with varying states.
    #[test]
    fn batch_sync_all_never_drops_commits(
        ws_a_commits in 1usize..=3,
        ws_b_commits in 0usize..=2,
        epoch_advances in 1usize..=2,
    ) {
        let repo = TestRepo::new();
        repo.maw_ok(&["ws", "create", "alpha"]);
        repo.maw_ok(&["ws", "create", "beta"]);

        make_local_commits(&repo, "alpha", ws_a_commits, "d");
        if ws_b_commits > 0 {
            make_local_commits(&repo, "beta", ws_b_commits, "e");
        }

        let alpha_head_before = repo.workspace_head("alpha");
        let beta_head_before = repo.workspace_head("beta");

        for i in 0..epoch_advances {
            advance_epoch(&repo, "d", i);
        }

        let _ = repo.maw_raw(&["ws", "sync", "--all"]);

        // alpha has local commits — HEAD must not change.
        prop_assert_eq!(
            alpha_head_before.clone(),
            repo.workspace_head("alpha"),
            "sync --all must not change HEAD of workspace with local commits (alpha)"
        );
        for i in 0..ws_a_commits {
            let file = format!("local_d_{i}.txt");
            prop_assert!(
                repo.read_file("alpha", &file).is_some(),
                "alpha local file {} missing after sync --all",
                file
            );
        }

        // beta with local commits: must also be preserved.
        if ws_b_commits > 0 {
            prop_assert_eq!(
                beta_head_before.clone(),
                repo.workspace_head("beta"),
                "sync --all must not change HEAD of workspace with local commits (beta)"
            );
            for i in 0..ws_b_commits {
                let file = format!("local_e_{i}.txt");
                prop_assert!(
                    repo.read_file("beta", &file).is_some(),
                    "beta local file {} missing after sync --all",
                    file
                );
            }
        }
    }
}
