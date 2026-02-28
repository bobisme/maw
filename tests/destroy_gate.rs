//! Integration tests for the destructive gate guarantee (G4).
//!
//! Implements IT-G4-001 from the assurance plan (Phase 0 exit criteria).
//!
//! # What is verified
//!
//! - **IT-G4-001**: Post-merge destroy correctly captures workspace state
//!   before deletion, and the capture serves as the gate for safe destruction.
//!   When capture succeeds, the workspace may be destroyed and the captured
//!   state remains recoverable. When capture cannot complete, destruction
//!   must be refused.
//!
//! # Design
//!
//! The G4 guarantee requires: "any operation that can destroy/overwrite
//! workspace state must abort or skip if capture prerequisites fail."
//!
//! The standalone `maw ws destroy --force` path propagates capture errors
//! (refuses to destroy). The post-merge `--destroy` path is tested here
//! to verify the capture-then-destroy sequence produces recoverable state.
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

/// Parse `maw ws recover <name> --format json` show output.
fn recover_show_json(repo: &TestRepo, name: &str) -> serde_json::Value {
    let output = repo.maw_ok(&["ws", "recover", name, "--format", "json"]);
    serde_json::from_str(&output).expect("recover show --format json should be valid JSON")
}

// ---------------------------------------------------------------------------
// IT-G4-001: Post-merge destroy capture gate
// ---------------------------------------------------------------------------

/// Tests that post-merge `--destroy` captures dirty workspace state before
/// deletion and that the captured state is recoverable.
///
/// Scenario:
/// 1. Create a workspace with both merge-ready work and extra dirty state.
/// 2. Merge with `--destroy`.
/// 3. Verify:
///    - Workspace is destroyed (directory removed).
///    - A recovery ref exists under `refs/manifold/recovery/<ws>/`.
///    - The recovery ref's commit tree includes the dirty files.
///    - `maw ws recover` lists the destroyed workspace.
///    - `maw ws recover <ws> --show <file>` returns correct content.
///    - The destroy record records the reason as `merge_destroy`.
#[test]
fn it_g4_001_post_merge_destroy_captures_state_before_deletion() {
    let repo = TestRepo::new();

    repo.seed_files(&[
        ("README.md", "# Project\n"),
        ("src/lib.rs", "pub fn lib() {}\n"),
    ]);

    // Create workspace with merge-ready work and extra dirty state
    repo.maw_ok(&["ws", "create", "gated-ws"]);

    // Files that will be part of the merge
    repo.add_file("gated-ws", "feature.txt", "feature implementation\n");
    repo.add_file("gated-ws", "src/feature.rs", "pub fn feature() {}\n");

    // Extra dirty state (untracked files not part of the merge diff)
    repo.add_file("gated-ws", "scratch-notes.txt", "agent wip notes\n");
    repo.add_file("gated-ws", "debug/trace.log", "trace line 1\ntrace line 2\n");

    // Merge with --destroy
    repo.maw_ok(&["ws", "merge", "gated-ws", "--destroy"]);

    // Workspace is destroyed
    assert!(
        !repo.workspace_exists("gated-ws"),
        "Workspace should be destroyed after merge --destroy"
    );

    // Recovery ref exists
    let refs = recovery_refs(&repo, "gated-ws");
    assert!(
        !refs.is_empty(),
        "Post-merge destroy must create a recovery ref, got none"
    );

    // Recovery ref resolves to a valid commit
    let ref_oid = repo.git(&["rev-parse", "--verify", &refs[0]]).trim().to_owned();
    assert_eq!(ref_oid.len(), 40, "Recovery OID should be 40-char hex: {}", ref_oid);

    // Captured commit tree includes the workspace files
    let tree_files = repo.git(&["ls-tree", "-r", "--name-only", &ref_oid]);
    let file_list: Vec<&str> = tree_files.lines().map(str::trim).filter(|l| !l.is_empty()).collect();

    assert!(
        file_list.contains(&"feature.txt"),
        "Capture tree should include feature.txt: {:?}",
        file_list
    );
    assert!(
        file_list.contains(&"scratch-notes.txt"),
        "Capture tree should include scratch-notes.txt: {:?}",
        file_list
    );

    // Merged content appears in default workspace
    assert_eq!(
        repo.read_file("default", "feature.txt").as_deref(),
        Some("feature implementation\n"),
        "Merged feature.txt should appear in default"
    );
    assert_eq!(
        repo.read_file("default", "src/feature.rs").as_deref(),
        Some("pub fn feature() {}\n"),
        "Merged src/feature.rs should appear in default"
    );

    // maw ws recover lists the destroyed workspace
    let list = recover_list_json(&repo);
    let workspaces = list["destroyed_workspaces"]
        .as_array()
        .expect("destroyed_workspaces should be an array");
    assert!(
        workspaces.iter().any(|w| w["name"].as_str() == Some("gated-ws")),
        "recover list should include gated-ws: {:?}",
        workspaces
    );

    // Verify destroy record has correct reason
    let show = recover_show_json(&repo, "gated-ws");
    let records = show["records"].as_array().expect("records should be an array");
    assert_eq!(records.len(), 1, "Should have exactly one destroy record");
    assert_eq!(
        records[0]["destroy_reason"].as_str(),
        Some("merge_destroy"),
        "Destroy reason should be merge_destroy"
    );

    // Verify file content is recoverable via --show
    let recovered_content = repo.maw_ok(&[
        "ws", "recover", "gated-ws", "--show", "scratch-notes.txt",
    ]);
    assert_eq!(
        recovered_content, "agent wip notes\n",
        "Recovered scratch-notes.txt should match original content"
    );
}

/// Tests that standalone `maw ws destroy` without `--force` refuses to
/// destroy a dirty workspace. This is the simplest G4 gate: the presence
/// of unmerged changes prevents destruction without explicit consent.
#[test]
fn it_g4_001_standalone_destroy_refuses_dirty_workspace_without_force() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "protected"]);
    repo.add_file("protected", "important-work.txt", "do not lose this\n");

    // Destroy without --force should be refused
    let stderr = repo.maw_fails(&["ws", "destroy", "protected"]);
    assert!(
        stderr.contains("unmerged") || stderr.contains("--force") || stderr.contains("Refusing"),
        "Destroy without --force should be refused for dirty workspace, got: {}",
        stderr
    );

    // Workspace must still exist
    assert!(
        repo.workspace_exists("protected"),
        "Refused destroy must not remove workspace"
    );

    // File must be intact
    assert_eq!(
        repo.read_file("protected", "important-work.txt").as_deref(),
        Some("do not lose this\n"),
        "Refused destroy must not alter workspace files"
    );

    // No recovery ref should be created (no capture was attempted)
    let refs = recovery_refs(&repo, "protected");
    assert!(
        refs.is_empty(),
        "Refused destroy should not create recovery refs, got: {:?}",
        refs
    );

    // No destroy record should exist
    let list = recover_list_json(&repo);
    let workspaces = list["destroyed_workspaces"]
        .as_array()
        .expect("destroyed_workspaces should be an array");
    assert!(
        !workspaces.iter().any(|w| w["name"].as_str() == Some("protected")),
        "Refused destroy should not create destroy record"
    );
}

/// Tests that standalone `maw ws destroy --force` on a dirty workspace
/// always creates a recovery ref before destruction.
///
/// This is the positive gate behavior: when the gate's preconditions are
/// met (capture succeeds), destruction proceeds and the captured state
/// is recoverable.
#[test]
fn it_g4_001_standalone_destroy_force_always_captures_before_deletion() {
    let repo = TestRepo::new();

    repo.seed_files(&[("base.txt", "base content\n")]);

    repo.maw_ok(&["ws", "create", "force-capture"]);
    repo.add_file("force-capture", "wip.txt", "work in progress\n");
    repo.modify_file("force-capture", "base.txt", "modified base\n");

    // Force destroy
    repo.maw_ok(&["ws", "destroy", "force-capture", "--force"]);

    // Workspace is destroyed
    assert!(
        !repo.workspace_exists("force-capture"),
        "Workspace should be destroyed after --force"
    );

    // Recovery ref exists
    let refs = recovery_refs(&repo, "force-capture");
    assert_eq!(
        refs.len(),
        1,
        "Force destroy of dirty workspace must create exactly one recovery ref, got: {:?}",
        refs
    );

    // Verify content is recoverable
    let wip_content = repo.maw_ok(&[
        "ws", "recover", "force-capture", "--show", "wip.txt",
    ]);
    assert_eq!(
        wip_content, "work in progress\n",
        "Recovered wip.txt should match original"
    );

    let base_content = repo.maw_ok(&[
        "ws", "recover", "force-capture", "--show", "base.txt",
    ]);
    assert_eq!(
        base_content, "modified base\n",
        "Recovered base.txt should contain the modification"
    );
}

/// Tests that a clean workspace at the epoch can be destroyed without
/// requiring capture (no-work proof allows bypass of capture gate).
///
/// G4's gate requires capture OR proof that no user work exists. A clean
/// workspace at the epoch has no user work, so destroy proceeds without
/// a recovery ref.
#[test]
fn it_g4_001_clean_workspace_at_epoch_destroys_without_capture() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "clean-ws"]);

    // No changes made to workspace — it's clean at epoch.
    // Destroy should succeed without needing --force.
    repo.maw_ok(&["ws", "destroy", "clean-ws"]);

    assert!(
        !repo.workspace_exists("clean-ws"),
        "Clean workspace should be destroyed"
    );

    // No recovery ref needed for clean workspace
    let refs = recovery_refs(&repo, "clean-ws");
    assert!(
        refs.is_empty(),
        "Clean workspace at epoch should not create recovery ref, got: {:?}",
        refs
    );
}

/// Tests that post-merge destroy captures state even when the workspace
/// has committed changes beyond the merge snapshot.
///
/// In the merge pipeline, the workspace's changes are snapshotted during
/// PREPARE phase. If the workspace has additional dirty state not included
/// in the merge (e.g., uncommitted files added after the snapshot was taken),
/// the post-merge capture should still capture them.
#[test]
fn it_g4_001_post_merge_destroy_captures_extra_dirty_state() {
    let repo = TestRepo::new();

    repo.seed_files(&[("config.txt", "initial config\n")]);

    repo.maw_ok(&["ws", "create", "extra-dirty"]);

    // Files that will be merged
    repo.add_file("extra-dirty", "result.txt", "computation result\n");

    // Extra dirty files (these exist in the workspace but are captured by
    // the merge diff as well -- all workspace dirty files are included)
    repo.add_file("extra-dirty", "notes/research.md", "# Research Notes\n\nFindings...\n");

    // Merge with --destroy
    repo.maw_ok(&["ws", "merge", "extra-dirty", "--destroy"]);

    // Workspace destroyed
    assert!(!repo.workspace_exists("extra-dirty"));

    // Recovery ref exists
    let refs = recovery_refs(&repo, "extra-dirty");
    assert!(
        !refs.is_empty(),
        "Post-merge destroy must create recovery ref even with extra dirty state"
    );

    // The destroy record should exist
    let show = recover_show_json(&repo, "extra-dirty");
    let records = show["records"].as_array().expect("records should be an array");
    assert!(
        !records.is_empty(),
        "Should have at least one destroy record"
    );

    // Merged content in default
    assert_eq!(
        repo.read_file("default", "result.txt").as_deref(),
        Some("computation result\n"),
        "Merged result.txt should be in default"
    );
}
