//! Property-based lifecycle invariant harness (bn-3fy4).
//!
//! Generates random sequences of workspace operations via proptest and checks
//! cross-cutting invariants after every applied operation.  These invariants
//! are the single highest-ROI testing investment for maw — every recent bug
//! (bn-18dj, bn-3h90 x3, v0.58.4 detector regression) would have been caught
//! by at least one of the invariants below.
//!
//! # Invariants
//!
//! - **I1**: No zombie refs after destroy.
//! - **I2**: Clean sync --rebase means no conflict markers in worktree.
//! - **I3**: Merge never silently drops content.
//! - **I4**: Create-after-destroy starts fresh (different oplog head OID).
//! - **I5**: Sync --rebase preserves commit count.
//! - **I6**: gitattributes merge=union produces no conflict markers.

#![cfg(not(miri))]

mod manifold_common;

use manifold_common::TestRepo;
use proptest::prelude::*;
use std::fmt::Write as _;
use std::process::Command;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Get the list of refs that a workspace owns (mirrors `maw_core::refs::workspace_owned_refs`).
/// Inlined here because maw-core is not a direct dependency of the root test crate
/// in a way that makes the function trivially importable from integration tests.
fn workspace_owned_refs(name: &str) -> Vec<String> {
    vec![
        format!("refs/manifold/ws/{name}"),
        format!("refs/manifold/epoch/ws/{name}"),
        format!("refs/manifold/head/{name}"),
    ]
}

/// Check whether a git ref exists in the repo.
fn ref_exists(repo: &TestRepo, ref_name: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", ref_name])
        .current_dir(repo.root())
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Read the OID that a ref points to, or None if the ref doesn't exist.
fn ref_oid(repo: &TestRepo, ref_name: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--verify", ref_name])
        .current_dir(repo.root())
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        None
    }
}

/// Advance the default-branch epoch by creating a throwaway workspace,
/// committing a unique file, and merging it back.
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

// ---------------------------------------------------------------------------
// I1: No zombie refs after destroy.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 32,
        .. ProptestConfig::default()
    })]

    /// After `ws destroy <name> --force`, every owned ref must be gone.
    /// Recovery refs should still exist if applicable.
    #[test]
    fn i1_no_zombie_refs_after_destroy(
        num_commits in 0usize..4,
    ) {
        let repo = TestRepo::new();
        repo.maw_ok(&["ws", "create", "victim"]);

        for i in 0..num_commits {
            repo.add_file("victim", &format!("f{i}.txt"), &format!("c{i}\n"));
            repo.git_in_workspace("victim", &["add", "-A"]);
            repo.git_in_workspace("victim", &["commit", "-m", &format!("commit {i}")]);
        }

        repo.maw_ok(&["ws", "destroy", "victim", "--force"]);

        // I1: every owned ref should be gone.
        for ref_name in workspace_owned_refs("victim") {
            prop_assert!(
                !ref_exists(&repo, &ref_name),
                "zombie ref '{}' survived destroy (num_commits={})",
                ref_name,
                num_commits
            );
        }
    }
}

// ---------------------------------------------------------------------------
// I2: Clean sync --rebase means no conflict markers in worktree.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 32,
        .. ProptestConfig::default()
    })]

    /// After a clean `sync --rebase` (exit 0, no conflict output), the
    /// workspace should have no conflict markers in the worktree.
    #[test]
    fn i2_clean_sync_rebase_means_no_markers(
        local_commits in 1usize..=3,
        epoch_advances in 1usize..=2,
    ) {
        let repo = TestRepo::new();
        repo.maw_ok(&["ws", "create", "feature"]);

        make_local_commits(&repo, "feature", local_commits, "i2");

        for i in 0..epoch_advances {
            advance_epoch(&repo, "i2", i);
        }

        let out = repo.maw_raw(&["ws", "sync", "feature", "--rebase"]);
        if !out.status.success() {
            // If sync --rebase itself fails, skip this case —
            // we only check the invariant when it reports clean.
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let combined = format!("{stdout}{stderr}");

        // If the output mentions conflicts, skip — we only check the "clean" case.
        if combined.contains("conflict") || combined.contains("CONFLICT") {
            return Ok(());
        }

        // I2: no conflict markers in any tracked file.
        let ws_path = repo.workspace_path("feature");
        let diff_out = Command::new("git")
            .args(["diff", "--name-only"])
            .current_dir(&ws_path)
            .output()
            .expect("operation should succeed");
        let diff_files = String::from_utf8_lossy(&diff_out.stdout);

        // Check tracked files for markers.
        let ls_out = Command::new("git")
            .args(["ls-files"])
            .current_dir(&ws_path)
            .output()
            .expect("operation should succeed");
        let tracked = String::from_utf8_lossy(&ls_out.stdout);

        for file in tracked.lines().chain(diff_files.lines()) {
            let file = file.trim();
            if file.is_empty() {
                continue;
            }
            let full_path = ws_path.join(file);
            if let Ok(content) = std::fs::read_to_string(&full_path) {
                prop_assert!(
                    !content.contains("<<<<<<<"),
                    "conflict markers found in '{}' after clean sync --rebase",
                    file
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// I3: Merge never silently drops content.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 32,
        .. ProptestConfig::default()
    })]

    /// Files added by a workspace must appear in default after a successful merge
    /// with the same content.
    #[test]
    fn i3_merge_preserves_content(
        num_files in 1usize..=4,
    ) {
        let repo = TestRepo::new();
        repo.maw_ok(&["ws", "create", "worker"]);

        // Record file content for later comparison.
        let mut expected: Vec<(String, String)> = Vec::new();
        for i in 0..num_files {
            let path = format!("work/file_{i}.txt");
            let content = format!("content for file {i}\nline2\n");
            repo.add_file("worker", &path, &content);
            expected.push((path, content));
        }
        repo.git_in_workspace("worker", &["add", "-A"]);
        repo.git_in_workspace("worker", &["commit", "-m", "add work files"]);

        let out = repo.maw_raw(&["ws", "merge", "worker", "--message", "merge worker"]);
        prop_assert!(
            out.status.success(),
            "merge should succeed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );

        // I3: every file from the workspace should be present in default
        // with the same content.
        for (path, content) in &expected {
            let actual = repo.read_file("default", path);
            prop_assert_eq!(
                actual.as_deref(),
                Some(content.as_str()),
                "file '{}' content mismatch or missing after merge",
                path
            );
        }
    }
}

// ---------------------------------------------------------------------------
// I4: Create-after-destroy starts fresh.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 32,
        .. ProptestConfig::default()
    })]

    /// After destroy + re-create with the same name, the oplog head ref
    /// should point to a DIFFERENT OID than before destroy.
    #[test]
    fn i4_create_after_destroy_starts_fresh(
        num_commits in 0usize..4,
    ) {
        let repo = TestRepo::new();
        repo.maw_ok(&["ws", "create", "phoenix"]);

        for i in 0..num_commits {
            repo.add_file("phoenix", &format!("old_{i}.txt"), &format!("old {i}\n"));
            repo.git_in_workspace("phoenix", &["add", "-A"]);
            repo.git_in_workspace("phoenix", &["commit", "-m", &format!("old {i}")]);
        }

        let head_ref = "refs/manifold/head/phoenix".to_string();
        let old_oid = ref_oid(&repo, &head_ref);

        repo.maw_ok(&["ws", "destroy", "phoenix", "--force"]);

        // After destroy the head ref should be gone (I1 covers this too).
        prop_assert!(
            !ref_exists(&repo, &head_ref),
            "head ref should be gone after destroy"
        );

        // Re-create with same name.
        repo.maw_ok(&["ws", "create", "phoenix"]);

        let new_oid = ref_oid(&repo, &head_ref);

        // If both lifecycles wrote the head ref, the OIDs must differ.
        if let (Some(old), Some(new)) = (&old_oid, &new_oid) {
            prop_assert_ne!(
                old, new,
                "re-created workspace should have a fresh oplog chain"
            );
        }
        // If the old lifecycle never wrote a head ref (0 commits), that's fine —
        // the invariant trivially holds because there's nothing to inherit.
    }
}

// ---------------------------------------------------------------------------
// I5: Sync --rebase preserves commit count.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 32,
        .. ProptestConfig::default()
    })]

    /// After `sync --rebase`, the number of commits ahead of the new epoch
    /// should equal the number of local commits made before the rebase.
    #[test]
    fn i5_sync_rebase_preserves_commit_count(
        local_commits in 1usize..=4,
        epoch_advances in 1usize..=3,
    ) {
        let repo = TestRepo::new();
        repo.maw_ok(&["ws", "create", "feature"]);

        let base_epoch = repo.current_epoch();
        make_local_commits(&repo, "feature", local_commits, "i5");

        let ahead_before = commits_ahead(&repo, "feature", &base_epoch);
        prop_assert_eq!(
            ahead_before, local_commits,
            "expected {} commits ahead before rebase", local_commits
        );

        for i in 0..epoch_advances {
            advance_epoch(&repo, "i5", i);
        }

        let out = repo.maw_raw(&["ws", "sync", "feature", "--rebase"]);
        prop_assert!(
            out.status.success(),
            "sync --rebase should succeed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );

        let new_epoch = repo.current_epoch();
        let ahead_after = commits_ahead(&repo, "feature", &new_epoch);
        prop_assert_eq!(
            ahead_after, local_commits,
            "commit count ahead of new epoch after rebase should equal local_commits"
        );

        // All local files must still be present.
        for i in 0..local_commits {
            let file = format!("local_i5_{i}.txt");
            prop_assert!(
                repo.read_file("feature", &file).is_some(),
                "local file {} missing after sync --rebase",
                file
            );
        }
    }
}

// ---------------------------------------------------------------------------
// I6: gitattributes merge=union produces no conflict markers.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 32,
        .. ProptestConfig::default()
    })]

    /// When `.gitattributes` marks a file as `merge=union`, merging two
    /// workspaces that both append to that file should produce no conflict
    /// markers and contain content from both sides.
    #[test]
    fn i6_union_merge_no_conflict_markers(
        alice_lines in 1usize..=3,
        bob_lines in 1usize..=3,
    ) {
        let repo = TestRepo::new();
        repo.seed_files(&[
            (".gitattributes", "*.log merge=union\n"),
            ("events.log", "header\n"),
        ]);

        repo.maw_ok(&["ws", "create", "alice"]);
        let mut alice_content = String::from("header\n");
        for i in 0..alice_lines {
            writeln!(&mut alice_content, "alice-event-{i}")
                .expect("writing to String cannot fail");
        }
        repo.add_file("alice", "events.log", &alice_content);
        repo.git_in_workspace("alice", &["add", "-A"]);
        repo.git_in_workspace("alice", &["commit", "-m", "alice: append"]);

        repo.maw_ok(&["ws", "create", "bob"]);
        let mut bob_content = String::from("header\n");
        for i in 0..bob_lines {
            writeln!(&mut bob_content, "bob-event-{i}").expect("writing to String cannot fail");
        }
        repo.add_file("bob", "events.log", &bob_content);
        repo.git_in_workspace("bob", &["add", "-A"]);
        repo.git_in_workspace("bob", &["commit", "-m", "bob: append"]);

        let out = repo.maw_raw(&[
            "ws", "merge", "alice", "bob",
            "--destroy", "--message", "merge both",
        ]);
        prop_assert!(
            out.status.success(),
            "union merge should succeed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );

        let merged = repo.read_file("default", "events.log")
            .expect("events.log should exist after merge");

        // I6: no conflict markers.
        prop_assert!(
            !merged.contains("<<<<<<<"),
            "union merge produced conflict markers:\n{merged}"
        );

        // Both sides' content should be present.
        for i in 0..alice_lines {
            prop_assert!(
                merged.contains(&format!("alice-event-{i}")),
                "missing alice-event-{i} in merged:\n{merged}"
            );
        }
        for i in 0..bob_lines {
            prop_assert!(
                merged.contains(&format!("bob-event-{i}")),
                "missing bob-event-{i} in merged:\n{merged}"
            );
        }

        // Header should appear exactly once.
        prop_assert_eq!(
            merged.matches("header\n").count(),
            1,
            "header duplicated in merged:\n{}", merged
        );
    }
}
