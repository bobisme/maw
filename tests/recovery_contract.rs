//! Integration tests for the recovery output contract.
//!
//! Verifies that all failure paths producing recovery surfaces emit the
//! 5 required fields:
//! 1. Operation result (success/failure)
//! 2. Whether COMMIT succeeded
//! 3. Snapshot ref + oid
//! 4. Artifact path
//! 5. Executable recovery command
//!
//! Bone: bn-11x6

mod manifold_common;

use manifold_common::TestRepo;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The 5 required recovery surface fields that must appear in stderr.
const REQUIRED_FIELDS: &[&str] = &[
    "result:",
    "commit:",
    "snapshot_ref:",
    "snapshot_oid:",
    "recover_cmd:",
];

/// Assert that stderr contains the RECOVERY_SURFACE header and all 5 fields.
fn assert_recovery_surface_present(stderr: &str, workspace: &str) {
    let header = format!("RECOVERY_SURFACE for '{workspace}':");
    assert!(
        stderr.contains(&header),
        "stderr should contain recovery surface header for '{workspace}'.\nstderr:\n{stderr}"
    );

    for field in REQUIRED_FIELDS {
        assert!(
            stderr.contains(field),
            "stderr missing required field '{field}'.\nstderr:\n{stderr}"
        );
    }
}

/// Extract a field value from the recovery surface block in stderr.
fn extract_field<'a>(stderr: &'a str, field_name: &str) -> Option<&'a str> {
    for line in stderr.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(field_name) {
            return Some(rest.trim());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// IT-G5-003: Standalone destroy with dirty workspace emits all 5 fields
// ---------------------------------------------------------------------------

#[test]
fn standalone_destroy_dirty_workspace_emits_recovery_surface() {
    let repo = TestRepo::new();

    repo.create_workspace("contract-dirty");
    repo.add_file("contract-dirty", "important.txt", "critical data\n");

    // Destroy with --force — the recovery surface is emitted on stderr
    let out = repo.maw_raw(&["ws", "destroy", "contract-dirty", "--force"]);
    assert!(
        out.status.success(),
        "destroy should succeed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_recovery_surface_present(&stderr, "contract-dirty");

    // Verify specific field values
    let result = extract_field(&stderr, "result:");
    assert_eq!(result, Some("success"), "operation should report success");

    let commit = extract_field(&stderr, "commit:");
    assert_eq!(commit, Some("no"), "standalone destroy has no commit");

    let snapshot_ref = extract_field(&stderr, "snapshot_ref:");
    assert!(
        snapshot_ref
            .unwrap_or("")
            .starts_with("refs/manifold/recovery/contract-dirty/"),
        "snapshot_ref should be under refs/manifold/recovery/, got: {snapshot_ref:?}"
    );

    let snapshot_oid = extract_field(&stderr, "snapshot_oid:");
    assert!(
        snapshot_oid.unwrap_or("").len() >= 40,
        "snapshot_oid should be a full SHA, got: {snapshot_oid:?}"
    );

    let recover_cmd = extract_field(&stderr, "recover_cmd:");
    assert_eq!(
        recover_cmd,
        Some("maw ws recover contract-dirty"),
        "recover_cmd should be exact"
    );
}

// ---------------------------------------------------------------------------
// IT-G5-003b: Merge --destroy --verbose emits all 5 fields
// ---------------------------------------------------------------------------

#[test]
fn merge_destroy_emits_recovery_surface_for_dirty_workspace() {
    let repo = TestRepo::new();

    // Create workspace, commit some changes, then add uncommitted edits
    repo.create_workspace("merge-contract");
    repo.add_file("merge-contract", "committed.txt", "merged content\n");
    // Also add an uncommitted file that will be captured
    repo.add_file("merge-contract", "leftover.txt", "uncommitted\n");

    // --verbose is required to get full RECOVERY_SURFACE output
    let out = repo.maw_raw(&["ws", "merge", "merge-contract", "--destroy", "--verbose"]);
    assert!(
        out.status.success(),
        "merge --destroy --verbose should succeed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_recovery_surface_present(&stderr, "merge-contract");

    // In the merge --destroy path, commit should have succeeded
    let commit = extract_field(&stderr, "commit:");
    assert_eq!(commit, Some("yes"), "merge --destroy should report commit=yes");

    let result = extract_field(&stderr, "result:");
    assert_eq!(result, Some("success"), "merge should report success");
}

// ---------------------------------------------------------------------------
// IT-G5-003e: Merge --destroy (no --verbose) omits full surface, shows short line
// ---------------------------------------------------------------------------

#[test]
fn merge_destroy_without_verbose_omits_recovery_surface() {
    let repo = TestRepo::new();

    repo.create_workspace("merge-short");
    repo.add_file("merge-short", "committed.txt", "merged content\n");
    repo.add_file("merge-short", "leftover.txt", "uncommitted\n");

    let out = repo.maw_raw(&["ws", "merge", "merge-short", "--destroy"]);
    assert!(
        out.status.success(),
        "merge --destroy should succeed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("RECOVERY_SURFACE"),
        "without --verbose, RECOVERY_SURFACE should not appear on stderr.\nstderr:\n{stderr}"
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Destroyed: merge-short (snapshot saved"),
        "short success line should appear on stdout.\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("maw ws recover merge-short"),
        "short line should include recover command.\nstdout:\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// IT-G5-004: Extract recovery command from output and verify it works
// ---------------------------------------------------------------------------

#[test]
fn recovery_command_from_output_is_executable() {
    let repo = TestRepo::new();

    repo.create_workspace("cmd-test");
    repo.add_file("cmd-test", "recover-me.txt", "precious data\n");

    let out = repo.maw_raw(&["ws", "destroy", "cmd-test", "--force"]);
    assert!(out.status.success());

    let stderr = String::from_utf8_lossy(&out.stderr);
    let recover_cmd = extract_field(&stderr, "recover_cmd:");
    assert!(
        recover_cmd.is_some(),
        "recover_cmd should be present in stderr"
    );

    // The recovery command should be "maw ws recover cmd-test"
    // Execute it to list the destroy records
    let recover_output = repo.maw_ok(&["ws", "recover", "cmd-test", "--format", "json"]);
    let recover_json: serde_json::Value =
        serde_json::from_str(&recover_output).expect("recover output should be valid JSON");

    let records = recover_json["records"]
        .as_array()
        .expect("records should be an array");
    assert!(
        !records.is_empty(),
        "recovery command should find at least one destroy record"
    );

    // Verify we can also use the snapshot_ref from the output to show a file
    let snapshot_ref = extract_field(&stderr, "snapshot_ref:");
    assert!(snapshot_ref.is_some(), "snapshot_ref should be present");

    let show_output = repo.maw_ok(&[
        "ws",
        "recover",
        "--ref",
        snapshot_ref.unwrap(),
        "--show",
        "recover-me.txt",
    ]);
    assert_eq!(
        show_output, "precious data\n",
        "should be able to recover file content using snapshot_ref from output"
    );
}

// ---------------------------------------------------------------------------
// IT-G5-003c: Clean workspace destroy does NOT emit recovery surface
// ---------------------------------------------------------------------------

#[test]
fn clean_workspace_destroy_no_recovery_surface() {
    let repo = TestRepo::new();

    repo.create_workspace("clean-contract");

    let out = repo.maw_raw(&["ws", "destroy", "clean-contract"]);
    assert!(out.status.success());

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("RECOVERY_SURFACE"),
        "clean workspace destroy should NOT emit recovery surface.\nstderr:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// IT-G5-003d: Artifact path field is populated
// ---------------------------------------------------------------------------

#[test]
fn recovery_surface_includes_artifact_path() {
    let repo = TestRepo::new();

    repo.create_workspace("artifact-test");
    repo.add_file("artifact-test", "data.txt", "track me\n");

    let out = repo.maw_raw(&["ws", "destroy", "artifact-test", "--force"]);
    assert!(out.status.success());

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_recovery_surface_present(&stderr, "artifact-test");

    let artifact = extract_field(&stderr, "artifact:");
    assert!(
        artifact.is_some(),
        "artifact field should be present"
    );

    let artifact_val = artifact.unwrap();
    // The artifact path should contain the workspace name and end with .json
    assert!(
        artifact_val.contains("artifact-test") && artifact_val.ends_with(".json"),
        "artifact should be a .json file containing workspace name, got: {artifact_val}"
    );
}
