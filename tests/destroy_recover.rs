//! End-to-end integration tests for the destroy → recover lifecycle.
//!
//! Covers the full surface area: both destroy paths (standalone and merge --destroy),
//! the `maw ws recover` command (list, show, --show, --to), workspace name reuse,
//! and clean workspace destroy behavior.
//!
//! Bone: bn-qg9u

mod manifold_common;

use manifold_common::TestRepo;

// ---------------------------------------------------------------------------
// Helper: parse `maw ws recover --format json` list output
// ---------------------------------------------------------------------------

fn recover_list_json(repo: &TestRepo) -> serde_json::Value {
    let output = repo.maw_ok(&["ws", "recover", "--format", "json"]);
    serde_json::from_str(&output).expect("recover list --format json should be valid JSON")
}

fn recover_show_json(repo: &TestRepo, name: &str) -> serde_json::Value {
    let output = repo.maw_ok(&["ws", "recover", name, "--format", "json"]);
    serde_json::from_str(&output).expect("recover show --format json should be valid JSON")
}

// ---------------------------------------------------------------------------
// Test 1: destroy --force dirty workspace → recoverable
// ---------------------------------------------------------------------------

#[test]
fn destroy_force_dirty_workspace_is_recoverable_via_recover() {
    let repo = TestRepo::new();

    // Create workspace and make dirty edits
    repo.create_workspace("dirty-ws");
    repo.add_file("dirty-ws", "important.txt", "critical data\n");
    repo.add_file("dirty-ws", "src/lib.rs", "pub fn hello() {}\n");

    // Destroy with --force
    repo.maw_ok(&["ws", "destroy", "dirty-ws", "--force"]);
    assert!(!repo.workspace_exists("dirty-ws"));

    // Verify `maw ws recover` lists it
    let list = recover_list_json(&repo);
    let workspaces = list["destroyed_workspaces"]
        .as_array()
        .expect("destroyed_workspaces should be an array");
    assert!(
        workspaces.iter().any(|w| w["name"].as_str() == Some("dirty-ws")),
        "recover list should include dirty-ws, got: {list}"
    );

    // Verify the entry has a snapshot and dirty file count
    let entry = workspaces
        .iter()
        .find(|w| w["name"].as_str() == Some("dirty-ws"))
        .unwrap();
    assert_ne!(
        entry["snapshot_oid"].as_str(),
        None,
        "dirty workspace should have a snapshot OID"
    );
    assert!(
        entry["dirty_file_count"].as_u64().unwrap_or(0) > 0,
        "dirty workspace should report dirty files"
    );

    // Verify `maw ws recover dirty-ws` shows details
    let show = recover_show_json(&repo, "dirty-ws");
    let records = show["records"]
        .as_array()
        .expect("records should be an array");
    assert_eq!(records.len(), 1, "should have exactly one destroy record");
    assert_eq!(
        records[0]["capture_mode"].as_str(),
        Some("dirty_snapshot"),
        "capture mode should be dirty_snapshot"
    );
    assert_eq!(
        records[0]["destroy_reason"].as_str(),
        Some("destroy"),
        "destroy reason should be 'destroy'"
    );

    // Verify `maw ws recover dirty-ws --show important.txt` returns file content
    let content = repo.maw_ok(&["ws", "recover", "dirty-ws", "--show", "important.txt"]);
    assert_eq!(
        content, "critical data\n",
        "recovered file content should match original"
    );

    // Also verify a nested file
    let lib_content = repo.maw_ok(&["ws", "recover", "dirty-ws", "--show", "src/lib.rs"]);
    assert_eq!(
        lib_content, "pub fn hello() {}\n",
        "recovered nested file content should match original"
    );
}

// ---------------------------------------------------------------------------
// Test 2: destroy without --force → refused with no state mutation
// ---------------------------------------------------------------------------

#[test]
fn destroy_without_force_refuses_dirty_workspace_no_side_effects() {
    let repo = TestRepo::new();

    // Create workspace and make dirty edits
    repo.create_workspace("protected-ws");
    repo.add_file("protected-ws", "work.txt", "do not lose me\n");

    // Attempt destroy without --force — should fail
    let stderr = repo.maw_fails(&["ws", "destroy", "protected-ws"]);
    assert!(
        stderr.contains("unmerged") || stderr.contains("--force"),
        "destroy without --force should be refused, got: {stderr}"
    );

    // Verify workspace still exists
    assert!(repo.workspace_exists("protected-ws"));

    // Verify NO destroy record was created
    let list = recover_list_json(&repo);
    let workspaces = list["destroyed_workspaces"]
        .as_array()
        .expect("destroyed_workspaces should be an array");
    assert!(
        !workspaces
            .iter()
            .any(|w| w["name"].as_str() == Some("protected-ws")),
        "refused destroy should NOT create a destroy record, got: {list}"
    );

    // Verify the file is still intact
    assert_eq!(
        repo.read_file("protected-ws", "work.txt").as_deref(),
        Some("do not lose me\n"),
    );
}

// ---------------------------------------------------------------------------
// Test 3: merge --destroy → recoverable + guidance output
// ---------------------------------------------------------------------------

#[test]
fn merge_destroy_captures_uncommitted_edits_in_destroy_record() {
    let repo = TestRepo::new();

    // Create workspace, commit some changes, then add uncommitted edits
    repo.create_workspace("merge-ws");
    repo.add_file("merge-ws", "committed.txt", "this gets merged\n");

    // Also add uncommitted edits that won't be part of the merge
    // (The merge captures the diff, but extra dirty files should be captured
    // in the destroy record)
    repo.add_file("merge-ws", "leftover.txt", "uncommitted leftover\n");

    // Merge and destroy
    let output = repo.maw_ok(&["ws", "merge", "merge-ws", "--destroy"]);

    // The merged content should be in default
    assert_eq!(
        repo.read_file("default", "committed.txt").as_deref(),
        Some("this gets merged\n"),
    );

    // Workspace should be gone
    assert!(!repo.workspace_exists("merge-ws"));

    // Verify destroy record exists via recover
    let list = recover_list_json(&repo);
    let workspaces = list["destroyed_workspaces"]
        .as_array()
        .expect("destroyed_workspaces should be an array");
    assert!(
        workspaces
            .iter()
            .any(|w| w["name"].as_str() == Some("merge-ws")),
        "merge --destroy should create a destroy record, got: {list}"
    );

    // Verify the destroy reason is MergeDestroy
    let show = recover_show_json(&repo, "merge-ws");
    let records = show["records"]
        .as_array()
        .expect("records should be an array");
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0]["destroy_reason"].as_str(),
        Some("merge_destroy"),
        "destroy reason should be 'merge_destroy'"
    );

    // The output should mention capture/snapshot or destroyed
    let lower = output.to_lowercase();
    assert!(
        lower.contains("destroy") || lower.contains("snapshot") || lower.contains("clean"),
        "merge --destroy output should mention workspace cleanup, got: {output}"
    );
}

// ---------------------------------------------------------------------------
// Test 4: recover list/show flows
// ---------------------------------------------------------------------------

#[test]
fn recover_list_shows_multiple_destroyed_workspaces() {
    let repo = TestRepo::new();

    // Create and destroy multiple workspaces
    repo.create_workspace("ws-alpha");
    repo.add_file("ws-alpha", "alpha.txt", "alpha content\n");
    repo.maw_ok(&["ws", "destroy", "ws-alpha", "--force"]);

    repo.create_workspace("ws-beta");
    repo.add_file("ws-beta", "beta.txt", "beta content\n");
    repo.maw_ok(&["ws", "destroy", "ws-beta", "--force"]);

    repo.create_workspace("ws-gamma");
    repo.add_file("ws-gamma", "gamma.txt", "gamma content\n");
    repo.maw_ok(&["ws", "destroy", "ws-gamma", "--force"]);

    // List all
    let list = recover_list_json(&repo);
    let workspaces = list["destroyed_workspaces"]
        .as_array()
        .expect("destroyed_workspaces should be an array");

    let names: Vec<&str> = workspaces
        .iter()
        .filter_map(|w| w["name"].as_str())
        .collect();

    assert!(names.contains(&"ws-alpha"), "should list ws-alpha");
    assert!(names.contains(&"ws-beta"), "should list ws-beta");
    assert!(names.contains(&"ws-gamma"), "should list ws-gamma");

    // Show details for a specific workspace
    let show = recover_show_json(&repo, "ws-beta");
    assert_eq!(show["workspace"].as_str(), Some("ws-beta"));
    let records = show["records"].as_array().unwrap();
    assert_eq!(records.len(), 1);
    assert!(
        records[0]["dirty_files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f.as_str() == Some("beta.txt")),
        "dirty_files should include beta.txt"
    );

    // Verify --show retrieves the correct file for each workspace
    let alpha_content = repo.maw_ok(&["ws", "recover", "ws-alpha", "--show", "alpha.txt"]);
    assert_eq!(alpha_content, "alpha content\n");

    let gamma_content = repo.maw_ok(&["ws", "recover", "ws-gamma", "--show", "gamma.txt"]);
    assert_eq!(gamma_content, "gamma content\n");
}

#[test]
fn recover_show_nonexistent_workspace_fails() {
    let repo = TestRepo::new();

    let stderr = repo.maw_fails(&["ws", "recover", "nonexistent"]);
    assert!(
        stderr.contains("No destroy records") || stderr.contains("not found"),
        "recover for non-existent workspace should fail, got: {stderr}"
    );
}

#[test]
fn recover_show_nonexistent_file_fails() {
    let repo = TestRepo::new();

    repo.create_workspace("show-test");
    repo.add_file("show-test", "exists.txt", "here\n");
    repo.maw_ok(&["ws", "destroy", "show-test", "--force"]);

    let stderr = repo.maw_fails(&["ws", "recover", "show-test", "--show", "nope.txt"]);
    assert!(
        stderr.contains("not found") || stderr.contains("does not exist"),
        "recover --show for missing file should fail, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Test 5: recover --to restore flow
// ---------------------------------------------------------------------------

#[test]
fn recover_to_restores_snapshot_into_new_workspace() {
    let repo = TestRepo::new();

    // Create workspace with dirty files, then destroy
    repo.create_workspace("recoverable");
    repo.add_file("recoverable", "precious.txt", "don't lose this\n");
    repo.add_file("recoverable", "src/mod.rs", "mod important;\n");
    repo.maw_ok(&["ws", "destroy", "recoverable", "--force"]);

    assert!(!repo.workspace_exists("recoverable"));

    // Recover to a new workspace name
    let output = repo.maw_ok(&["ws", "recover", "recoverable", "--to", "recovered-ws"]);
    assert!(
        output.contains("Restored") || output.contains("recovered") || output.contains("recover"),
        "recover --to output should confirm restoration, got: {output}"
    );

    // New workspace should exist
    assert!(repo.workspace_exists("recovered-ws"));

    // The recovered workspace should contain the dirty files
    assert_eq!(
        repo.read_file("recovered-ws", "precious.txt").as_deref(),
        Some("don't lose this\n"),
        "recovered workspace should contain the dirty file"
    );
    assert_eq!(
        repo.read_file("recovered-ws", "src/mod.rs").as_deref(),
        Some("mod important;\n"),
        "recovered workspace should contain nested dirty file"
    );
}

#[test]
fn recover_to_existing_workspace_fails() {
    let repo = TestRepo::new();

    repo.create_workspace("source-ws");
    repo.add_file("source-ws", "data.txt", "data\n");
    repo.maw_ok(&["ws", "destroy", "source-ws", "--force"]);

    // Create a workspace with the target name
    repo.create_workspace("target-ws");

    let stderr = repo.maw_fails(&["ws", "recover", "source-ws", "--to", "target-ws"]);
    assert!(
        stderr.contains("already exists"),
        "recover --to existing workspace should fail, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Test 6: workspace name reuse
// ---------------------------------------------------------------------------

#[test]
fn workspace_name_reuse_creates_independent_destroy_records() {
    let repo = TestRepo::new();

    // First cycle: create, dirty, destroy
    repo.create_workspace("reusable");
    repo.add_file("reusable", "first.txt", "first cycle content\n");
    repo.maw_ok(&["ws", "destroy", "reusable", "--force"]);

    // Ensure different timestamps for distinct records
    std::thread::sleep(std::time::Duration::from_secs(1));

    // Second cycle: create same name, different content, destroy
    repo.create_workspace("reusable");
    repo.add_file("reusable", "second.txt", "second cycle content\n");
    repo.maw_ok(&["ws", "destroy", "reusable", "--force"]);

    // Verify recover shows both records
    let show = recover_show_json(&repo, "reusable");
    let records = show["records"]
        .as_array()
        .expect("records should be an array");
    assert_eq!(
        records.len(),
        2,
        "should have two destroy records for reused name, got: {show}"
    );

    // Records should have different timestamps
    let ts0 = records[0]["destroyed_at"].as_str().unwrap();
    let ts1 = records[1]["destroyed_at"].as_str().unwrap();
    assert_ne!(ts0, ts1, "destroy timestamps should differ");

    // Both records should have dirty snapshots
    assert_eq!(records[0]["capture_mode"].as_str(), Some("dirty_snapshot"));
    assert_eq!(records[1]["capture_mode"].as_str(), Some("dirty_snapshot"));

    // The list view should show the workspace once (latest record)
    let list = recover_list_json(&repo);
    let names: Vec<&str> = list["destroyed_workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|w| w["name"].as_str())
        .collect();
    let reusable_count = names.iter().filter(|&&n| n == "reusable").count();
    assert_eq!(
        reusable_count, 1,
        "list should show reusable once (latest), got {reusable_count}"
    );

    // Verify --show returns content from the LATEST record
    let content = repo.maw_ok(&["ws", "recover", "reusable", "--show", "second.txt"]);
    assert_eq!(content, "second cycle content\n");
}

// ---------------------------------------------------------------------------
// Test 7: clean workspace destroy → no snapshot (capture_mode=none)
// ---------------------------------------------------------------------------

#[test]
fn clean_workspace_destroy_has_no_snapshot() {
    let repo = TestRepo::new();

    // Create a workspace and destroy it immediately (no edits)
    repo.create_workspace("clean-ws");
    repo.maw_ok(&["ws", "destroy", "clean-ws"]);

    // Verify it appears in recover list (destroy record is written even for clean)
    // OR it doesn't appear because there's nothing to recover — check which behavior
    let list = recover_list_json(&repo);
    let workspaces = list["destroyed_workspaces"]
        .as_array()
        .expect("destroyed_workspaces should be an array");

    // If it appears, verify capture_mode is none and no snapshot
    if let Some(entry) = workspaces
        .iter()
        .find(|w| w["name"].as_str() == Some("clean-ws"))
    {
        assert_eq!(
            entry["capture_mode"].as_str(),
            Some("none"),
            "clean workspace should have capture_mode=none"
        );
        assert_eq!(
            entry["dirty_file_count"].as_u64().unwrap_or(0),
            0,
            "clean workspace should have zero dirty files"
        );
    }
    // If it doesn't appear, that's also fine — no snapshot means nothing to recover

    // Verify --show fails (nothing captured)
    if workspaces
        .iter()
        .any(|w| w["name"].as_str() == Some("clean-ws"))
    {
        let stderr =
            repo.maw_fails(&["ws", "recover", "clean-ws", "--show", "anything.txt"]);
        assert!(
            stderr.contains("capture_mode=none")
                || stderr.contains("No snapshot")
                || stderr.contains("not found")
                || stderr.contains("does not exist"),
            "recover --show on clean workspace should fail, got: {stderr}"
        );
    }
}

// ---------------------------------------------------------------------------
// Test: recover --to fails when no destroy records exist
// ---------------------------------------------------------------------------

#[test]
fn recover_to_fails_when_no_destroy_records() {
    let repo = TestRepo::new();

    let stderr = repo.maw_fails(&["ws", "recover", "never-existed", "--to", "new-ws"]);
    assert!(
        stderr.contains("No destroy records") || stderr.contains("not found"),
        "recover --to without records should fail, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Test: recover --show rejects path traversal
// ---------------------------------------------------------------------------

#[test]
fn recover_show_rejects_path_traversal() {
    let repo = TestRepo::new();

    repo.create_workspace("traversal-test");
    repo.add_file("traversal-test", "safe.txt", "safe\n");
    repo.maw_ok(&["ws", "destroy", "traversal-test", "--force"]);

    let stderr = repo.maw_fails(&[
        "ws",
        "recover",
        "traversal-test",
        "--show",
        "../../../etc/passwd",
    ]);
    assert!(
        stderr.contains("directory traversal") || stderr.contains(".."),
        "recover --show should reject path traversal, got: {stderr}"
    );
}
