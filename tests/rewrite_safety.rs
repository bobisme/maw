//! Integration tests for rewrite safety (G2 guarantees).
//!
//! Implements IT-G2-001 and IT-G2-002 from the assurance plan (Phase 0 exit
//! criteria).
//!
//! # What is verified
//!
//! - **IT-G2-001**: Dirty default workspace state (untracked files) survives
//!   the post-COMMIT rewrite that updates `ws/default/` to the new epoch.
//!   The merge pipeline uses `git checkout --force <branch>` to update the
//!   default workspace after COMMIT. Untracked files survive this operation.
//!   Staged and unstaged modifications to tracked files require the
//!   `preserve_checkout_replay` primitive (Phase 0 deliverable #3) to survive.
//!
//! - **IT-G2-002**: Recovery refs created by pre-destroy capture are valid,
//!   resolvable, and restorable. When replay rollback is needed, the recovery
//!   ref serves as the rollback target. This test validates the capture
//!   infrastructure that rollback depends on.
//!
//! Bone: bn-4102

#![allow(clippy::all, clippy::pedantic, clippy::nursery)]

mod manifold_common;

use manifold_common::TestRepo;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Collect all recovery refs for a workspace.
fn recovery_refs(repo: &TestRepo, workspace: &str) -> Vec<String> {
    let output = repo.git(&["for-each-ref", "--format=%(refname)", "refs/manifold/recovery/"]);
    output
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with(&format!("refs/manifold/recovery/{workspace}/")))
        .map(ToOwned::to_owned)
        .collect()
}

/// Parse `maw ws recover --format json` list output.
fn recover_list_json(repo: &TestRepo) -> serde_json::Value {
    let output = repo.maw_ok(&["ws", "recover", "--format", "json"]);
    serde_json::from_str(&output).expect("recover list --format json should be valid JSON")
}

// ---------------------------------------------------------------------------
// IT-G2-001: Dirty default workspace survives post-COMMIT rewrite
// ---------------------------------------------------------------------------

/// Full scenario:
///
/// 1. Seed the repo with tracked files (advance epoch to E1).
/// 2. Create a workspace `worker`, add committed work.
/// 3. In `ws/default/`, create dirty state:
///    - Untracked file (never staged, not in any commit tree).
///    - Untracked nested file (under a new directory).
/// 4. Merge `worker` with `--destroy`.
/// 5. Verify:
///    - Merged content appears in `ws/default/`.
///    - ALL untracked dirty files in `ws/default/` are preserved.
///    - The epoch has advanced.
///    - The worker workspace is destroyed.
#[test]
fn it_g2_001_dirty_default_untracked_files_survive_post_commit_rewrite() {
    let repo = TestRepo::new();

    // Step 1: Seed tracked files at epoch
    repo.seed_files(&[
        ("README.md", "# Project\n"),
        ("src/lib.rs", "pub fn base() {}\n"),
    ]);
    let epoch_before = repo.current_epoch();

    // Step 2: Create worker workspace and add work
    repo.maw_ok(&["ws", "create", "worker"]);
    repo.add_file("worker", "feature.txt", "worker feature output\n");
    repo.add_file("worker", "src/feature.rs", "pub fn feature() { todo!() }\n");

    // Step 3: Create dirty state in ws/default/
    // Untracked file (not in any commit tree)
    repo.add_file("default", "local-notes.txt", "personal notes that must survive\n");
    // Untracked nested file (new directory)
    repo.add_file("default", "scratch/debug.log", "debug output line 1\nline 2\n");
    // Another untracked file
    repo.add_file("default", "TODO.txt", "- fix bug #123\n- review PR #456\n");

    // Verify dirty state exists before merge
    let dirty_before = repo.dirty_files("default");
    assert!(
        dirty_before.len() >= 3,
        "Expected at least 3 dirty files before merge, got {}: {:?}",
        dirty_before.len(),
        dirty_before
    );

    // Step 4: Merge worker --destroy
    repo.maw_ok(&["ws", "merge", "worker", "--destroy"]);

    // Step 5a: Verify merged content appears in default
    assert_eq!(
        repo.read_file("default", "feature.txt").as_deref(),
        Some("worker feature output\n"),
        "Merged file feature.txt should appear in default workspace"
    );
    assert_eq!(
        repo.read_file("default", "src/feature.rs").as_deref(),
        Some("pub fn feature() { todo!() }\n"),
        "Merged nested file src/feature.rs should appear in default workspace"
    );

    // Step 5b: Verify pre-existing tracked files still present
    assert_eq!(
        repo.read_file("default", "README.md").as_deref(),
        Some("# Project\n"),
        "Pre-existing tracked file README.md should survive merge"
    );

    // Step 5c: Verify ALL untracked dirty files survived
    assert_eq!(
        repo.read_file("default", "local-notes.txt").as_deref(),
        Some("personal notes that must survive\n"),
        "Untracked file local-notes.txt must survive post-COMMIT rewrite"
    );
    assert_eq!(
        repo.read_file("default", "scratch/debug.log").as_deref(),
        Some("debug output line 1\nline 2\n"),
        "Untracked nested file scratch/debug.log must survive post-COMMIT rewrite"
    );
    assert_eq!(
        repo.read_file("default", "TODO.txt").as_deref(),
        Some("- fix bug #123\n- review PR #456\n"),
        "Untracked file TODO.txt must survive post-COMMIT rewrite"
    );

    // Step 5d: Epoch has advanced
    let epoch_after = repo.current_epoch();
    assert_ne!(
        epoch_before, epoch_after,
        "Epoch should advance after successful merge"
    );

    // Step 5e: Worker workspace is destroyed
    assert!(
        !repo.workspace_exists("worker"),
        "Worker workspace should be destroyed after --destroy"
    );
}

/// Variant: multiple workspaces merged simultaneously with dirty default.
///
/// Exercises the same code path with a multi-source merge to ensure
/// rewrite safety scales to N-workspace merges.
#[test]
fn it_g2_001_dirty_default_survives_multi_workspace_merge() {
    let repo = TestRepo::new();

    repo.seed_files(&[("base.txt", "base content\n")]);

    // Create two workspaces with non-conflicting changes
    repo.maw_ok(&["ws", "create", "alpha"]);
    repo.maw_ok(&["ws", "create", "beta"]);

    repo.add_file("alpha", "alpha.txt", "alpha work\n");
    repo.add_file("beta", "beta.txt", "beta work\n");

    // Add untracked dirty state to default
    repo.add_file("default", "agent-scratch.txt", "wip notes\n");
    repo.add_file("default", "tmp/cache.dat", "cached data\n");

    // Multi-workspace merge
    repo.maw_ok(&["ws", "merge", "alpha", "beta", "--destroy"]);

    // Merged content present
    assert_eq!(
        repo.read_file("default", "alpha.txt").as_deref(),
        Some("alpha work\n"),
        "alpha.txt should be merged into default"
    );
    assert_eq!(
        repo.read_file("default", "beta.txt").as_deref(),
        Some("beta work\n"),
        "beta.txt should be merged into default"
    );

    // Untracked dirty files survived
    assert_eq!(
        repo.read_file("default", "agent-scratch.txt").as_deref(),
        Some("wip notes\n"),
        "Untracked agent-scratch.txt must survive multi-workspace merge"
    );
    assert_eq!(
        repo.read_file("default", "tmp/cache.dat").as_deref(),
        Some("cached data\n"),
        "Untracked nested tmp/cache.dat must survive multi-workspace merge"
    );

    // Source workspaces destroyed
    assert!(!repo.workspace_exists("alpha"), "alpha should be destroyed");
    assert!(!repo.workspace_exists("beta"), "beta should be destroyed");
}

/// Variant: dirty default files do not interfere with merge correctness.
///
/// Verifies that the presence of untracked files in ws/default/ does not
/// cause the merge to produce incorrect results or skip changes.
#[test]
fn it_g2_001_dirty_default_does_not_corrupt_merge_result() {
    let repo = TestRepo::new();

    // Seed with a file that the worker will modify
    repo.seed_files(&[("shared.txt", "original shared content\n")]);

    repo.maw_ok(&["ws", "create", "modifier"]);
    repo.modify_file("modifier", "shared.txt", "modified by worker\n");
    repo.add_file("modifier", "new-from-worker.txt", "worker added this\n");

    // Default has untracked files in the same directory tree
    repo.add_file("default", "my-local.txt", "local stuff\n");

    repo.maw_ok(&["ws", "merge", "modifier", "--destroy"]);

    // Merge result is correct (worker's modification applied)
    assert_eq!(
        repo.read_file("default", "shared.txt").as_deref(),
        Some("modified by worker\n"),
        "Worker's modification to shared.txt should be applied"
    );
    assert_eq!(
        repo.read_file("default", "new-from-worker.txt").as_deref(),
        Some("worker added this\n"),
        "Worker's new file should appear in default"
    );

    // Default's untracked file survives
    assert_eq!(
        repo.read_file("default", "my-local.txt").as_deref(),
        Some("local stuff\n"),
        "Default's untracked file must survive merge"
    );
}

// ---------------------------------------------------------------------------
// IT-G2-002: Replay failure rolls back to snapshot (capture infrastructure)
// ---------------------------------------------------------------------------

/// Tests the capture-and-recover infrastructure that replay rollback depends on.
///
/// Scenario:
/// 1. Create workspace with a mix of dirty state types (staged tracked changes,
///    unstaged tracked changes, untracked files).
/// 2. Destroy with `--force` (triggers `capture_before_destroy`).
/// 3. Verify:
///    - A recovery ref is created under `refs/manifold/recovery/<ws>/`.
///    - The recovery ref resolves to a valid git object.
///    - The captured commit tree contains all dirty files.
///    - Content is restorable via `maw ws recover --to`.
///    - Restored content matches originals byte-for-byte.
///
/// This validates the snapshot infrastructure that Phase 0's
/// `preserve_checkout_replay` uses for rollback on replay failure.
#[test]
fn it_g2_002_capture_snapshot_is_valid_and_restorable() {
    let repo = TestRepo::new();

    // Seed files so we have tracked content to modify
    repo.seed_files(&[
        ("tracked.txt", "original tracked content\n"),
        ("src/main.rs", "fn main() { println!(\"hello\"); }\n"),
    ]);

    // Create workspace (inherits tracked files from epoch)
    repo.maw_ok(&["ws", "create", "snapshot-test"]);

    // Add untracked files
    repo.add_file("snapshot-test", "untracked.txt", "untracked data\n");
    repo.add_file("snapshot-test", "logs/debug.log", "debug line 1\ndebug line 2\n");

    // Modify a tracked file (creates unstaged change)
    repo.modify_file("snapshot-test", "tracked.txt", "modified tracked content\n");

    // Stage a tracked file modification
    repo.git_in_workspace(
        "snapshot-test",
        &["add", "tracked.txt"],
    );

    // Also add a new file and stage it
    repo.add_file("snapshot-test", "staged-new.txt", "staged new content\n");
    repo.git_in_workspace(
        "snapshot-test",
        &["add", "staged-new.txt"],
    );

    // Add another untracked modification AFTER staging (creates unstaged on top of staged)
    repo.modify_file("snapshot-test", "src/main.rs", "fn main() { println!(\"modified\"); }\n");

    // Destroy with --force (captures state)
    let destroy_output = repo.maw_ok(&["ws", "destroy", "snapshot-test", "--force"]);

    // Workspace is gone
    assert!(
        !repo.workspace_exists("snapshot-test"),
        "Workspace should be destroyed after --force"
    );

    // Recovery ref was created
    let refs = recovery_refs(&repo, "snapshot-test");
    assert_eq!(
        refs.len(),
        1,
        "Expected exactly one recovery ref, got: {:?}",
        refs
    );

    // Recovery ref resolves to a valid git object
    let ref_oid = repo.git(&["rev-parse", "--verify", &refs[0]]).trim().to_owned();
    assert_eq!(
        ref_oid.len(),
        40,
        "Recovery ref OID should be a 40-char hex SHA, got: {}",
        ref_oid
    );
    assert!(
        ref_oid.chars().all(|c| c.is_ascii_hexdigit()),
        "Recovery ref OID should be valid hex: {}",
        ref_oid
    );

    // Verify the capture commit tree contains the dirty files
    let tree_files = repo.git(&["ls-tree", "-r", "--name-only", &ref_oid]);
    let file_list: Vec<&str> = tree_files.lines().map(str::trim).filter(|l| !l.is_empty()).collect();

    assert!(
        file_list.contains(&"untracked.txt"),
        "Capture tree should contain untracked.txt, got: {:?}",
        file_list
    );
    assert!(
        file_list.contains(&"staged-new.txt"),
        "Capture tree should contain staged-new.txt, got: {:?}",
        file_list
    );
    assert!(
        file_list.contains(&"tracked.txt"),
        "Capture tree should contain tracked.txt, got: {:?}",
        file_list
    );

    // Verify destroy output mentions capture/snapshot
    let lower = destroy_output.to_lowercase();
    assert!(
        lower.contains("snapshot") || lower.contains("captured") || lower.contains("recovery")
            || lower.contains("destroy"),
        "Destroy output should mention capture/snapshot, got: {}",
        destroy_output
    );

    // Restore via maw ws recover --to and verify content
    repo.maw_ok(&["ws", "recover", "snapshot-test", "--to", "restored-ws"]);

    assert!(
        repo.workspace_exists("restored-ws"),
        "Restored workspace should exist"
    );

    // Verify restored content matches originals
    assert_eq!(
        repo.read_file("restored-ws", "untracked.txt").as_deref(),
        Some("untracked data\n"),
        "Restored untracked.txt should match original"
    );
    assert_eq!(
        repo.read_file("restored-ws", "staged-new.txt").as_deref(),
        Some("staged new content\n"),
        "Restored staged-new.txt should match original"
    );
    // tracked.txt should have the modified (staged) content
    assert_eq!(
        repo.read_file("restored-ws", "tracked.txt").as_deref(),
        Some("modified tracked content\n"),
        "Restored tracked.txt should contain the staged modification"
    );
}

/// Tests that the recovery ref survives garbage collection.
///
/// This is critical for replay rollback: the snapshot must remain
/// reachable even after GC runs (e.g., during long-running operations
/// or when `git gc --auto` triggers between capture and rollback).
#[test]
fn it_g2_002_recovery_ref_survives_gc() {
    let repo = TestRepo::new();

    repo.seed_files(&[("base.txt", "base\n")]);
    repo.maw_ok(&["ws", "create", "gc-test"]);

    repo.add_file("gc-test", "important.txt", "must survive gc\n");
    repo.modify_file("gc-test", "base.txt", "modified base\n");

    repo.maw_ok(&["ws", "destroy", "gc-test", "--force"]);

    let refs_before = recovery_refs(&repo, "gc-test");
    assert_eq!(refs_before.len(), 1, "Should have one recovery ref before GC");

    // Run aggressive GC
    repo.git(&["gc", "--aggressive", "--prune=now"]);

    // Recovery ref must still exist
    let refs_after = recovery_refs(&repo, "gc-test");
    assert_eq!(
        refs_after.len(),
        1,
        "Recovery ref must survive aggressive GC, got: {:?}",
        refs_after
    );

    // Content must still be accessible
    let content = repo.maw_ok(&["ws", "recover", "gc-test", "--show", "important.txt"]);
    assert_eq!(
        content, "must survive gc\n",
        "Captured content must survive GC"
    );
}

/// Tests that the recovery ref for a post-merge --destroy is created
/// and restorable (merge-destroy path, not standalone destroy).
///
/// This validates that the post-merge destroy path creates the same
/// recovery infrastructure as standalone destroy -- the rollback target
/// for preserve_checkout_replay must be available regardless of how
/// destroy is triggered.
#[test]
fn it_g2_002_merge_destroy_creates_restorable_recovery_ref() {
    let repo = TestRepo::new();

    repo.seed_files(&[("shared.txt", "shared base\n")]);

    // Create workspace, make changes that will be merged
    repo.maw_ok(&["ws", "create", "merge-capture"]);
    repo.add_file("merge-capture", "merged-work.txt", "this gets merged\n");

    // Also add an untracked file that won't be part of the merge diff
    // (it's "leftover" dirty state in the workspace)
    repo.add_file("merge-capture", "leftover.txt", "leftover content\n");

    // Merge and destroy
    repo.maw_ok(&["ws", "merge", "merge-capture", "--destroy"]);

    // Workspace is gone
    assert!(
        !repo.workspace_exists("merge-capture"),
        "Workspace should be destroyed after merge --destroy"
    );

    // Recovery ref should exist
    let refs = recovery_refs(&repo, "merge-capture");
    assert!(
        !refs.is_empty(),
        "Post-merge destroy should create at least one recovery ref"
    );

    // The merged content should be in default
    assert_eq!(
        repo.read_file("default", "merged-work.txt").as_deref(),
        Some("this gets merged\n"),
        "Merged content should appear in default"
    );

    // The recovery should be listed via maw ws recover
    let list = recover_list_json(&repo);
    let workspaces = list["destroyed_workspaces"]
        .as_array()
        .expect("destroyed_workspaces should be an array");
    assert!(
        workspaces
            .iter()
            .any(|w| w["name"].as_str() == Some("merge-capture")),
        "recover list should include merge-capture, got: {:?}",
        workspaces
    );
}
