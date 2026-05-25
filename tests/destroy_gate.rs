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
use std::process::Command;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Collect all recovery refs for a workspace.
fn recovery_refs(repo: &TestRepo, workspace: &str) -> Vec<String> {
    let output = repo.git(&[
        "for-each-ref",
        "--format=%(refname)",
        "refs/manifold/recovery/",
    ]);
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
    repo.add_file(
        "gated-ws",
        "debug/trace.log",
        "trace line 1\ntrace line 2\n",
    );

    // Merge with --destroy
    repo.maw_ok(&[
        "ws",
        "merge",
        "gated-ws",
        "--destroy",
        "--message",
        "test merge",
    ]);

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
    let ref_oid = repo
        .git(&["rev-parse", "--verify", &refs[0]])
        .trim()
        .to_owned();
    assert_eq!(
        ref_oid.len(),
        40,
        "Recovery OID should be 40-char hex: {}",
        ref_oid
    );

    // Captured commit tree includes the workspace files
    let tree_files = repo.git(&["ls-tree", "-r", "--name-only", &ref_oid]);
    let file_list: Vec<&str> = tree_files
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

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
        workspaces
            .iter()
            .any(|w| w["name"].as_str() == Some("gated-ws")),
        "recover list should include gated-ws: {:?}",
        workspaces
    );

    // Verify destroy record has correct reason
    let show = recover_show_json(&repo, "gated-ws");
    let records = show["records"]
        .as_array()
        .expect("records should be an array");
    assert_eq!(records.len(), 1, "Should have exactly one destroy record");
    assert_eq!(
        records[0]["destroy_reason"].as_str(),
        Some("merge_destroy"),
        "Destroy reason should be merge_destroy"
    );

    // Verify file content is recoverable via --show
    let recovered_content =
        repo.maw_ok(&["ws", "recover", "gated-ws", "--show", "scratch-notes.txt"]);
    assert_eq!(
        recovered_content, "agent wip notes\n",
        "Recovered scratch-notes.txt should match original content"
    );
}

#[test]
fn post_merge_destroy_handles_uncapturable_embedded_repo_path() {
    let repo = TestRepo::new();

    repo.seed_files(&[("README.md", "# Project\n")]);
    repo.maw_ok(&["ws", "create", "embedded-git"]);

    // Create merge-ready committed work so merge --destroy has something to merge.
    repo.add_file("embedded-git", "feature.txt", "feature\n");
    repo.git_in_workspace("embedded-git", &["add", "feature.txt"]);
    repo.git_in_workspace("embedded-git", &["commit", "-m", "feat: add feature"]);

    // Create an untracked embedded git directory without a checked-out commit.
    // This historically caused capture to fail at `git add -A`.
    let nested = repo.workspace_path("embedded-git").join(".tmp/sub");
    std::fs::create_dir_all(&nested).expect("create nested directory");
    let init = Command::new("git")
        .args(["init"])
        .current_dir(&nested)
        .output()
        .expect("run nested git init");
    assert!(
        init.status.success(),
        "nested git init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    std::fs::write(nested.join("scratch.txt"), "nested scratch\n").expect("write nested file");

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "embedded-git",
        "--destroy",
        "--message",
        "test merge",
    ]);
    assert!(
        out.status.success(),
        "merge should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Workspace should now be destroyed instead of being preserved due to a
    // capture failure.
    assert!(
        !repo.workspace_exists("embedded-git"),
        "workspace should be destroyed even with uncapturable embedded git path"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("Failed to capture state"),
        "capture should not fail for embedded git path, stderr: {stderr}"
    );
}

#[test]
fn standalone_destroy_force_handles_uncapturable_embedded_repo_path() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "force-embed"]);
    repo.add_file("force-embed", "NOTE.txt", "keep this\n");

    let nested = repo.workspace_path("force-embed").join(".tmp/sub");
    std::fs::create_dir_all(&nested).expect("create nested directory");
    let init = Command::new("git")
        .args(["init"])
        .current_dir(&nested)
        .output()
        .expect("run nested git init");
    assert!(
        init.status.success(),
        "nested git init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    std::fs::write(nested.join("scratch.txt"), "nested scratch\n").expect("write nested file");

    let out = repo.maw_raw(&["ws", "destroy", "force-embed", "--force"]);
    assert!(
        out.status.success(),
        "destroy --force should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !repo.workspace_exists("force-embed"),
        "workspace should be destroyed after force destroy"
    );

    let recovered = repo.maw_ok(&["ws", "recover", "force-embed", "--show", "NOTE.txt"]);
    assert_eq!(recovered, "keep this\n");
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
        !workspaces
            .iter()
            .any(|w| w["name"].as_str() == Some("protected")),
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
    let wip_content = repo.maw_ok(&["ws", "recover", "force-capture", "--show", "wip.txt"]);
    assert_eq!(
        wip_content, "work in progress\n",
        "Recovered wip.txt should match original"
    );

    let base_content = repo.maw_ok(&["ws", "recover", "force-capture", "--show", "base.txt"]);
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
    repo.add_file(
        "extra-dirty",
        "notes/research.md",
        "# Research Notes\n\nFindings...\n",
    );

    // Merge with --destroy
    repo.maw_ok(&[
        "ws",
        "merge",
        "extra-dirty",
        "--destroy",
        "--message",
        "test merge",
    ]);

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
    let records = show["records"]
        .as_array()
        .expect("records should be an array");
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

// ---------------------------------------------------------------------------
// bn-c6l3 (SG4 / destroy-guidance-output): refusal output regression tests
//
// These tests pin the self-describing refusal behavior that targets the
// `ws_destroy_refused` friction cluster
// (`MawVerbAttribution::WsDestroyRefused`). The hardening lives in
// `crates/maw-cli/src/workspace/destroy_guidance.rs`; the unit tests
// there cover the renderer in isolation. These integration tests pin
// the *end-to-end* behavior: a real `maw ws destroy` invocation must
// emit the self-describing message and (with `--format json`) the
// structured payload.
//
// Target metric delta: ≥ 50% reduction in `ws_destroy_refused` cluster
// cost (practical: "reaches 0"). The soft proxy these tests pin is
// "the refusal message tells the agent the right safe command in one
// turn, so a second-turn discovery isn't needed".
// ---------------------------------------------------------------------------

/// Pinned: refusal for a workspace with uncommitted edits leads with
/// the safe "commit then merge" path *before* the `--force` escape
/// hatch. The legacy text led with `--force`, which encouraged the
/// agent to choose the data-loss-shaped action.
#[test]
fn bn_c6l3_refusal_for_dirty_workspace_leads_with_commit_then_merge() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "dirty-ws"]);
    repo.add_file("dirty-ws", "wip.txt", "scratch\n");

    let stderr = repo.maw_fails(&["ws", "destroy", "dirty-ws"]);

    // Names the safe-cleanup vocabulary state.
    assert!(
        stderr.contains("dirty-uncommitted"),
        "Refusal must name the dirty-uncommitted state from the \
         safe-cleanup vocabulary; got:\n{stderr}"
    );

    // Recommends the commit-then-merge path (the safe one).
    assert!(
        stderr.contains("Recommended:"),
        "Refusal must surface a Recommended: action line; got:\n{stderr}"
    );
    assert!(
        stderr.contains("git commit"),
        "Recommended path for dirty workspace must include a commit \
         step; got:\n{stderr}"
    );
    assert!(
        stderr.contains("maw ws merge dirty-ws"),
        "Recommended path must reference merging the workspace; got:\n{stderr}"
    );

    // SAFE path appears before FORCE path.
    let safe_idx = stderr.find("Recommended:").expect("Recommended: present");
    let force_idx = stderr
        .find("Or force-destroy:")
        .expect("force-destroy line present");
    assert!(
        safe_idx < force_idx,
        "Safe path must appear before force path in refusal output; \
         got:\n{stderr}"
    );

    // Prime-Invariant reassurance inline so the agent doesn't need a
    // second turn to verify `--force` is safe.
    assert!(
        stderr.contains("Prime Invariant"),
        "Refusal must inline the Prime-Invariant reassurance; got:\n{stderr}"
    );
    assert!(
        stderr.contains("maw ws recover dirty-ws"),
        "Refusal must include the exact recover command; got:\n{stderr}"
    );

    // Refusal still refuses — the workspace and its file survive.
    assert!(
        repo.workspace_exists("dirty-ws"),
        "Refusal must not delete the workspace"
    );
    assert_eq!(
        repo.read_file("dirty-ws", "wip.txt").as_deref(),
        Some("scratch\n"),
        "Refusal must not alter workspace files"
    );
}

/// Pinned: refusal for a workspace with committed-unintegrated work
/// recommends `maw ws merge <name> --into default --destroy` as the
/// single-command safe path (instead of telling the agent to inspect
/// first and decide between two further options).
#[test]
fn bn_c6l3_refusal_for_committed_work_recommends_merge_and_destroy() {
    let repo = TestRepo::new();

    repo.seed_files(&[("README.md", "# Project\n")]);
    repo.maw_ok(&["ws", "create", "committed-ws"]);
    repo.add_file("committed-ws", "feature.txt", "feature\n");
    repo.git_in_workspace("committed-ws", &["add", "-A"]);
    repo.git_in_workspace("committed-ws", &["commit", "-m", "feat: feature"]);

    let stderr = repo.maw_fails(&["ws", "destroy", "committed-ws"]);

    // Names the safe-cleanup vocabulary state.
    assert!(
        stderr.contains("committed-unintegrated"),
        "Refusal must name the committed-unintegrated state; got:\n{stderr}"
    );

    // Recommends the merge --destroy one-shot.
    assert!(
        stderr.contains("Recommended: maw ws merge committed-ws"),
        "Refusal must recommend the merge path; got:\n{stderr}"
    );
    assert!(
        stderr.contains("--into default"),
        "Recommended merge must specify --into default; got:\n{stderr}"
    );
    assert!(
        stderr.contains("--destroy"),
        "Recommended merge must include --destroy for atomic cleanup; \
         got:\n{stderr}"
    );

    // Workspace not destroyed.
    assert!(
        repo.workspace_exists("committed-ws"),
        "Refusal must not delete the workspace"
    );
}

/// Pinned: `maw ws destroy <name> --format json` emits a parseable
/// `DestroyRefusal` payload to stderr alongside the human-readable
/// bail message. Machine consumers can branch on `lifecycle_state`
/// and `recommended_action_kind` slugs without regex over the text.
#[test]
fn bn_c6l3_refusal_emits_machine_readable_json_under_format_flag() {
    let repo = TestRepo::new();

    repo.seed_files(&[("README.md", "# Project\n")]);
    repo.maw_ok(&["ws", "create", "json-ws"]);
    repo.add_file("json-ws", "code.rs", "// code\n");
    repo.git_in_workspace("json-ws", &["add", "-A"]);
    repo.git_in_workspace("json-ws", &["commit", "-m", "feat: add code"]);

    let out = repo.maw_raw(&["ws", "destroy", "json-ws", "--format", "json"]);
    assert!(
        !out.status.success(),
        "destroy of committed workspace must still refuse"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    // The JSON payload should be embedded in stderr — find the first
    // `{` and parse from there to the matching `}`. We don't rely on
    // exact line ordering because tracing or anyhow may interleave.
    let start = stderr
        .find('{')
        .unwrap_or_else(|| panic!("expected JSON object in stderr; got:\n{stderr}"));
    let json_slice = &stderr[start..];
    // Find the matching brace (the payload is small and self-contained).
    let mut depth = 0i32;
    let mut end = 0;
    for (i, c) in json_slice.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(end > 0, "could not find matching brace in:\n{json_slice}");
    let json_text = &json_slice[..end];
    let v: serde_json::Value = serde_json::from_str(json_text)
        .unwrap_or_else(|e| panic!("destroy refusal JSON must parse (err={e}); got:\n{json_text}"));

    assert_eq!(v["workspace"].as_str(), Some("json-ws"));
    assert_eq!(
        v["lifecycle_state"].as_str(),
        Some("committed-unintegrated"),
        "JSON must carry the safe-cleanup vocabulary slug; got:\n{json_text}"
    );
    assert_eq!(
        v["recommended_action_kind"].as_str(),
        Some("merge-and-destroy"),
        "JSON must carry the recommended action kind slug; got:\n{json_text}"
    );
    assert!(
        v["recommended_action"]
            .as_str()
            .expect("recommended_action present")
            .contains("maw ws merge json-ws"),
        "JSON recommended_action must be a paste-ready command; got:\n{json_text}"
    );
    assert!(
        v["force_safety_note"]
            .as_str()
            .expect("force_safety_note present")
            .contains("Prime Invariant"),
        "JSON force_safety_note must cite the Prime Invariant; got:\n{json_text}"
    );
}
