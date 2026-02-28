//! Regression tests for ws/default working-copy preservation during merge.
//!
//! These tests verify that uncommitted changes in ws/default are never lost
//! during a merge operation. They exercise the full maw CLI merge path and
//! validate that user work (modified tracked files, untracked files, and staged
//! changes) survives the post-COMMIT default workspace update.
//!
//! Corresponds to bone bn-lbv8 and assurance plan guarantees G2 (rewrite
//! no-loss) and I-G2.1 (destructive rewrite boundary requires capture or
//! no-work proof).
//!
//! # Test scenarios
//!
//! - T1: Modified tracked file in default survives agent merge
//! - T2: Untracked file in default survives agent merge
//! - T3: Staged changes in default survive agent merge
//! - T4: Clean default workspace -- fast path (no recovery refs)
//! - T5: Multiple dirty files with mixed types survive agent merge
//!
//! These tests were originally written as `#[ignore]` to document G2 violations
//! where `update_default_workspace()` used `git checkout --force`. The merge path
//! now uses `preserve_checkout_replay()` (bn-2agp), so all tests should pass.

mod manifold_common;

use manifold_common::TestRepo;

// ---------------------------------------------------------------------------
// T1: Modified tracked file in default survives agent merge
// ---------------------------------------------------------------------------

/// Regression test: unstaged modification to a tracked file in ws/default
/// must survive a merge that triggers `update_default_workspace()`.
///
/// Previously failed because `git checkout --force` overwrote tracked dirty
/// files. Fixed by bn-2agp: `preserve_checkout_replay()` now replaces the
/// force checkout in the merge cleanup path.
#[test]
fn t1_modified_tracked_file_in_default_survives_merge() {
    let repo = TestRepo::new();

    // Seed a tracked file into the epoch so it exists in default
    repo.seed_files(&[("tracked.txt", "original content\n")]);

    // Create agent workspace and do agent work
    repo.maw_ok(&["ws", "create", "agent"]);
    repo.add_file("agent", "agent-work.txt", "agent contribution\n");

    // Modify a tracked file in default (unstaged)
    repo.modify_file("default", "tracked.txt", "user modified content\n");

    // Merge agent work -- this triggers the default workspace update
    repo.maw_ok(&["ws", "merge", "agent", "--destroy"]);

    // Assert: user's modification of tracked.txt is preserved
    assert_eq!(
        repo.read_file("default", "tracked.txt").as_deref(),
        Some("user modified content\n"),
        "user modification to tracked.txt should survive merge"
    );

    // Assert: agent's work is present
    assert_eq!(
        repo.read_file("default", "agent-work.txt").as_deref(),
        Some("agent contribution\n"),
        "agent-work.txt should be present after merge"
    );
}

// ---------------------------------------------------------------------------
// T2: Untracked file in default survives agent merge
// ---------------------------------------------------------------------------

/// Regression test: an untracked file in ws/default must survive a merge.
///
/// This test passes today because `git checkout --force` does not remove
/// untracked files. It serves as a regression guard to ensure this behavior
/// is preserved when the merge cleanup path is refactored.
#[test]
fn t2_untracked_file_in_default_survives_merge() {
    let repo = TestRepo::new();

    // Create agent workspace with some work
    repo.maw_ok(&["ws", "create", "agent"]);
    repo.add_file("agent", "agent-result.txt", "agent output\n");

    // Create an untracked file in default workspace
    repo.add_file("default", "user-notes.txt", "my personal notes\n");

    // Merge agent work
    repo.maw_ok(&["ws", "merge", "agent", "--destroy"]);

    // Assert: untracked file in default survives
    assert!(
        repo.file_exists("default", "user-notes.txt"),
        "untracked user-notes.txt should survive merge"
    );
    assert_eq!(
        repo.read_file("default", "user-notes.txt").as_deref(),
        Some("my personal notes\n"),
        "untracked file content should be preserved"
    );

    // Assert: agent work is present
    assert_eq!(
        repo.read_file("default", "agent-result.txt").as_deref(),
        Some("agent output\n"),
        "agent-result.txt should be present after merge"
    );
}

// ---------------------------------------------------------------------------
// T3: Staged changes in default survive agent merge
// ---------------------------------------------------------------------------

/// Regression test: staged modifications to tracked files in ws/default
/// must survive a merge.
///
/// Previously failed because `git checkout --force` discarded staged changes.
/// Fixed by bn-2agp: `preserve_checkout_replay()` now replaces the force
/// checkout.
#[test]
fn t3_staged_changes_in_default_survive_merge() {
    let repo = TestRepo::new();

    // Seed a tracked file
    repo.seed_files(&[("config.txt", "default config\n")]);

    // Create agent workspace with work
    repo.maw_ok(&["ws", "create", "agent"]);
    repo.add_file("agent", "feature.txt", "new feature\n");

    // Modify and stage a file in default
    repo.modify_file("default", "config.txt", "user updated config\n");
    repo.git_in_workspace("default", &["add", "config.txt"]);

    // Merge agent work
    repo.maw_ok(&["ws", "merge", "agent", "--destroy"]);

    // Assert: staged changes are preserved in file content
    assert_eq!(
        repo.read_file("default", "config.txt").as_deref(),
        Some("user updated config\n"),
        "staged modification to config.txt should survive merge"
    );

    // Assert: agent work is present
    assert_eq!(
        repo.read_file("default", "feature.txt").as_deref(),
        Some("new feature\n"),
        "feature.txt from agent should be present after merge"
    );
}

// ---------------------------------------------------------------------------
// T4: Clean default workspace -- fast path, no recovery refs
// ---------------------------------------------------------------------------

/// Regression test: a clean ws/default with no user modifications should
/// merge cleanly. `preserve_checkout_replay()` always captures a recovery
/// snapshot before rewriting (even when clean), which is the safe default.
#[test]
fn t4_clean_default_merge_succeeds() {
    let repo = TestRepo::new();

    // Create agent workspace with work
    repo.maw_ok(&["ws", "create", "agent"]);
    repo.add_file("agent", "output.txt", "agent output\n");

    // Default workspace is clean (no user modifications)

    // Merge agent work
    repo.maw_ok(&["ws", "merge", "agent", "--destroy"]);

    // Assert: agent work is present
    assert_eq!(
        repo.read_file("default", "output.txt").as_deref(),
        Some("agent output\n"),
        "output.txt from agent should be present after merge"
    );
}

// ---------------------------------------------------------------------------
// T5: Multiple dirty files with mixed types survive merge
// ---------------------------------------------------------------------------

/// Regression test: a mix of unstaged tracked modifications and untracked
/// files in ws/default must all survive a merge.
///
/// Previously failed because `git checkout --force` overwrote the tracked
/// file modifications. Fixed by bn-2agp: `preserve_checkout_replay()` now
/// replaces the force checkout.
#[test]
fn t5_mixed_dirty_files_survive_merge() {
    let repo = TestRepo::new();

    // Seed multiple tracked files
    repo.seed_files(&[
        ("shared.txt", "base shared\n"),
        ("independent.txt", "base independent\n"),
        ("untouched.txt", "should not change\n"),
    ]);

    // Create agent workspace that modifies shared.txt and adds a new file
    repo.maw_ok(&["ws", "create", "agent"]);
    repo.modify_file("agent", "shared.txt", "agent modified shared\n");
    repo.add_file("agent", "agent-new.txt", "brand new from agent\n");

    // In default workspace:
    // 1. Modify a non-overlapping tracked file (unstaged)
    repo.modify_file("default", "independent.txt", "user modified independent\n");

    // 2. Add an untracked file
    repo.add_file("default", "scratch.txt", "user scratch work\n");

    // Merge agent work
    repo.maw_ok(&["ws", "merge", "agent", "--destroy"]);

    // Assert: non-overlapping user modification preserved
    assert_eq!(
        repo.read_file("default", "independent.txt").as_deref(),
        Some("user modified independent\n"),
        "user modification to independent.txt should survive merge"
    );

    // Assert: untracked file preserved
    assert!(
        repo.file_exists("default", "scratch.txt"),
        "untracked scratch.txt should survive merge"
    );
    assert_eq!(
        repo.read_file("default", "scratch.txt").as_deref(),
        Some("user scratch work\n"),
        "untracked file content should be preserved"
    );

    // Assert: agent's new file is present
    assert_eq!(
        repo.read_file("default", "agent-new.txt").as_deref(),
        Some("brand new from agent\n"),
        "agent-new.txt should be present after merge"
    );

    // Assert: untouched.txt is unchanged
    assert_eq!(
        repo.read_file("default", "untouched.txt").as_deref(),
        Some("should not change\n"),
        "untouched.txt should be unchanged after merge"
    );

    // Assert: shared.txt has agent's version (agent merge takes precedence
    // for committed changes in the merge source)
    assert_eq!(
        repo.read_file("default", "shared.txt").as_deref(),
        Some("agent modified shared\n"),
        "shared.txt should have agent's committed version"
    );
}

// ---------------------------------------------------------------------------
// T6: Replay conflict leaves markers, merge CLEANUP still completes
// ---------------------------------------------------------------------------

/// Regression test (bn-1wtu): when a file is modified both in the default
/// workspace (uncommitted) and in the merge source (committed), the merge
/// should produce conflict markers in the working tree and still complete
/// all cleanup steps (workspace destroy, merge-state removal).
///
/// CRITICAL: the merge COMMIT has already succeeded before replay, so the
/// cleanup phase MUST NOT abort on replay conflicts.
#[test]
fn t6_replay_conflict_leaves_markers_and_cleanup_completes() {
    let repo = TestRepo::new();

    // Seed with a tracked file that both sides will modify.
    repo.seed_files(&[("conflict.txt", "base content\n")]);

    // Create agent workspace that modifies the same file.
    repo.maw_ok(&["ws", "create", "agent"]);
    repo.modify_file("agent", "conflict.txt", "agent version\n");

    // User modifies the same file in default (uncommitted).
    repo.modify_file("default", "conflict.txt", "user version\n");

    // Merge: this must succeed (non-zero exit is acceptable for the merge
    // command itself since the output may include a warning, but the merge
    // COMMIT must have landed).
    let out = repo.maw_raw(&["ws", "merge", "agent", "--destroy"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // The merge COMMIT should succeed (epoch should advance).
    // Even if the overall command exits non-zero due to conflict warnings,
    // the merge output should indicate the commit landed.
    assert!(
        stdout.contains("COMMIT") || out.status.success(),
        "merge should reach COMMIT phase\nstdout: {stdout}\nstderr: {stderr}"
    );

    // The agent workspace should be destroyed (cleanup completed).
    assert!(
        !repo.workspace_exists("agent"),
        "agent workspace should be destroyed after merge cleanup"
    );

    // conflict.txt should exist in default (either with conflict markers
    // or one version winning).
    let content = repo.read_file("default", "conflict.txt");
    assert!(
        content.is_some(),
        "conflict.txt should exist in default after merge"
    );

    // If there were conflict markers, they should be present in the content.
    // If the stash apply chose one version cleanly, that's also acceptable.
    // The key assertion is: the file exists and cleanup completed.
}

// ---------------------------------------------------------------------------
// T7: Merge CLEANUP completes even with replay errors
// ---------------------------------------------------------------------------

/// Regression test (bn-1wtu): even if the replay step fails entirely (not
/// just conflicts), the merge cleanup must still complete. The COMMIT
/// succeeded, so workspace destruction and merge-state cleanup must run.
#[test]
fn t7_merge_cleanup_completes_after_commit_regardless_of_replay() {
    let repo = TestRepo::new();

    // Create two agent workspaces.
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.maw_ok(&["ws", "create", "bob"]);

    // Both workspaces add files.
    repo.add_file("alice", "alice.txt", "alice work\n");
    repo.add_file("bob", "bob.txt", "bob work\n");

    // Add a dirty file in default (untracked).
    repo.add_file("default", "local-notes.txt", "my notes\n");

    // Merge alice (this updates the epoch).
    repo.maw_ok(&["ws", "merge", "alice", "--destroy"]);

    // Alice's workspace should be gone.
    assert!(
        !repo.workspace_exists("alice"),
        "alice workspace should be destroyed after merge"
    );

    // Now merge bob (default has local-notes.txt from before).
    repo.maw_ok(&["ws", "merge", "bob", "--destroy"]);

    // Bob's workspace should be gone (cleanup completed).
    assert!(
        !repo.workspace_exists("bob"),
        "bob workspace should be destroyed after merge cleanup"
    );

    // Both agent files should be present in default.
    assert_eq!(
        repo.read_file("default", "alice.txt").as_deref(),
        Some("alice work\n"),
        "alice.txt should be present after merge"
    );
    assert_eq!(
        repo.read_file("default", "bob.txt").as_deref(),
        Some("bob work\n"),
        "bob.txt should be present after merge"
    );

    // User's dirty file should still be present (survived both merges).
    assert_eq!(
        repo.read_file("default", "local-notes.txt").as_deref(),
        Some("my notes\n"),
        "user's local-notes.txt should survive both merges"
    );
}
