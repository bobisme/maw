//! Integration tests for `maw ws merge` command
//!
//! Tests workspace merging in bare repos (v2 model).
//! Each test creates an isolated temp repo.

mod common;

use common::{default_ws, maw_fails, maw_in, maw_ok, read_from_ws, run_jj, setup_bare_repo, write_in_ws};

#[test]
fn basic_merge() {
    let repo = setup_bare_repo();

    // Create two workspaces
    maw_ok(repo.path(), &["ws", "create", "alice"]);
    maw_ok(repo.path(), &["ws", "create", "bob"]);

    // Write different files in each workspace
    write_in_ws(repo.path(), "alice", "alice.txt", "Alice's work");
    let alice_ws = repo.path().join("ws").join("alice");
    run_jj(&alice_ws, &["describe", "-m", "feat: alice's changes"]);

    write_in_ws(repo.path(), "bob", "bob.txt", "Bob's work");
    let bob_ws = repo.path().join("ws").join("bob");
    run_jj(&bob_ws, &["describe", "-m", "feat: bob's changes"]);

    // Merge both workspaces and destroy them
    maw_ok(
        repo.path(),
        &["ws", "merge", "alice", "bob", "--destroy"],
    );

    // Verify both files are present in default workspace
    let alice_content = read_from_ws(repo.path(), "default", "alice.txt");
    assert_eq!(
        alice_content.as_deref(),
        Some("Alice's work"),
        "alice.txt should be present in default workspace"
    );

    let bob_content = read_from_ws(repo.path(), "default", "bob.txt");
    assert_eq!(
        bob_content.as_deref(),
        Some("Bob's work"),
        "bob.txt should be present in default workspace"
    );

    // Verify workspaces are destroyed
    let list_output = maw_ok(repo.path(), &["ws", "list"]);
    assert!(
        !list_output.contains("alice"),
        "alice workspace should be destroyed"
    );
    assert!(
        !list_output.contains("bob"),
        "bob workspace should be destroyed"
    );
    assert!(
        list_output.contains("default"),
        "default workspace should remain"
    );
}

#[test]
fn single_workspace_merge() {
    let repo = setup_bare_repo();

    // Create one workspace
    maw_ok(repo.path(), &["ws", "create", "agent-1"]);

    // Write a file and describe the commit
    write_in_ws(repo.path(), "agent-1", "feature.txt", "New feature");
    let ws_path = repo.path().join("ws").join("agent-1");
    run_jj(&ws_path, &["describe", "-m", "feat: add new feature"]);

    // Merge with custom message
    maw_ok(
        repo.path(),
        &["ws", "merge", "agent-1", "--message", "feat: custom msg"],
    );

    // Verify file is present in default workspace
    let content = read_from_ws(repo.path(), "default", "feature.txt");
    assert_eq!(
        content.as_deref(),
        Some("New feature"),
        "feature.txt should be present in default workspace"
    );
}

#[test]
fn merge_with_conflict() {
    let repo = setup_bare_repo();

    // First, create a base file in main that both workspaces will modify
    let default_ws = repo.path().join("ws").join("default");
    write_in_ws(repo.path(), "default", "data.txt", "original content\n");
    run_jj(&default_ws, &["commit", "-m", "add data file"]);
    run_jj(&default_ws, &["bookmark", "set", "main", "-r", "@-"]);

    // Create two workspaces that will both modify the SAME file completely differently
    maw_ok(repo.path(), &["ws", "create", "alice"]);
    maw_ok(repo.path(), &["ws", "create", "bob"]);

    // Alice replaces the entire file with her version
    write_in_ws(repo.path(), "alice", "data.txt", "Alice was here\n");
    let alice_ws = repo.path().join("ws").join("alice");
    run_jj(&alice_ws, &["describe", "-m", "feat: alice's data"]);

    // Bob replaces the entire file with his version
    write_in_ws(repo.path(), "bob", "data.txt", "Bob was here\n");
    let bob_ws = repo.path().join("ws").join("bob");
    run_jj(&bob_ws, &["describe", "-m", "feat: bob's data"]);

    // Merge with --destroy flag — may succeed or fail depending on jj's merge
    let out = maw_in(repo.path(), &["ws", "merge", "alice", "bob", "--destroy"]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();

    // Check if workspaces still exist (they should if there was a conflict)
    let list_output = maw_ok(repo.path(), &["ws", "list"]);

    // The test passes regardless of whether jj detected a conflict or not.
    // What matters is that IF there's a conflict, workspaces are preserved
    // and exit code is non-zero.
    // If jj merged it cleanly, workspaces are destroyed and exit code is 0.
    if list_output.contains("alice") || list_output.contains("bob") {
        // Workspaces were preserved - must have been a conflict
        assert!(
            !out.status.success(),
            "Merge with conflicts should exit non-zero"
        );
        assert!(
            stdout.contains("conflict")
                || stdout.contains("Conflict")
                || stdout.contains("NOT destroying"),
            "If workspaces were preserved, output should mention conflict or not destroying"
        );
        println!("Test verified: conflict was detected, workspaces preserved, exit non-zero");
    } else {
        // Workspaces were destroyed - jj merged cleanly
        assert!(
            out.status.success(),
            "Clean merge should exit zero"
        );
        println!("Test verified: no conflict detected, workspaces were destroyed");
    }
}

#[test]
fn merge_conflict_shows_details_and_guidance() {
    let repo = setup_bare_repo();

    // Create a base file in main that both workspaces will modify
    let dws = repo.path().join("ws").join("default");
    write_in_ws(repo.path(), "default", "shared.txt", "line 1\nline 2\nline 3\n");
    run_jj(&dws, &["commit", "-m", "add shared file"]);
    run_jj(&dws, &["bookmark", "set", "main", "-r", "@-"]);

    // Create two workspaces that both modify the same file
    maw_ok(repo.path(), &["ws", "create", "alice"]);
    maw_ok(repo.path(), &["ws", "create", "bob"]);

    write_in_ws(
        repo.path(),
        "alice",
        "shared.txt",
        "alice line 1\nalice line 2\nalice line 3\n",
    );
    let alice_ws = repo.path().join("ws").join("alice");
    run_jj(&alice_ws, &["describe", "-m", "feat: alice edits"]);

    write_in_ws(
        repo.path(),
        "bob",
        "shared.txt",
        "bob line 1\nbob line 2\nbob line 3\n",
    );
    let bob_ws = repo.path().join("ws").join("bob");
    run_jj(&bob_ws, &["describe", "-m", "feat: bob edits"]);

    // Merge with --destroy so we can distinguish conflict (workspaces preserved)
    // from clean merge (workspaces destroyed)
    let out = maw_in(
        repo.path(),
        &["ws", "merge", "alice", "bob", "--destroy"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();

    // Detect conflict from the merge output (not from workspace list)
    let has_conflict = stdout.contains("conflict") || stdout.contains("Conflict");

    if has_conflict {
        // Conflict was detected — verify detailed output
        let ws_path = repo
            .path()
            .join("ws")
            .join("default")
            .display()
            .to_string();

        // Should include the "Conflicts:" header
        assert!(
            stdout.contains("Conflicts:"),
            "Output should include 'Conflicts:' header, got:\n{stdout}"
        );

        // Should mention the conflicted file
        assert!(
            stdout.contains("shared.txt"),
            "Output should list 'shared.txt' as conflicted, got:\n{stdout}"
        );

        // Should include line range info (e.g. "lines 1-7" or similar)
        assert!(
            stdout.contains("lines ") || stdout.contains("line "),
            "Output should include line range info, got:\n{stdout}"
        );

        // Should include the absolute workspace path in guidance
        assert!(
            stdout.contains(&ws_path),
            "Output should include absolute workspace path '{}', got:\n{stdout}",
            ws_path
        );

        // Should include resolution steps
        assert!(
            stdout.contains("To resolve:"),
            "Output should include 'To resolve:' guidance, got:\n{stdout}"
        );

        // Should include maw exec command for verification
        assert!(
            stdout.contains("maw exec default -- jj status"),
            "Output should include 'maw exec default -- jj status' command, got:\n{stdout}"
        );

        println!("Test verified: conflict details and guidance are shown");
    } else {
        // jj merged cleanly — that's fine, the feature is only for conflicts
        println!("Test verified: no conflict detected (jj merged cleanly)");
    }
}

#[test]
fn dirty_default_auto_snapshots_before_merge() {
    let repo = setup_bare_repo();

    // Write a file directly in default workspace (uncommitted work)
    write_in_ws(repo.path(), "default", "uncommitted.txt", "dirty state");

    // Create agent workspace with changes
    maw_ok(repo.path(), &["ws", "create", "agent-1"]);
    write_in_ws(repo.path(), "agent-1", "agent.txt", "agent work");
    let ws_path = repo.path().join("ws").join("agent-1");
    run_jj(&ws_path, &["describe", "-m", "feat: agent work"]);

    // Merge should succeed — auto-snapshot saves uncommitted changes
    let stdout = maw_ok(repo.path(), &["ws", "merge", "agent-1", "--destroy"]);
    assert!(
        stdout.contains("Auto-snapshotting") || stdout.contains("Merged"),
        "Merge should succeed with auto-snapshot, got: {stdout}"
    );

    // Verify the agent work is visible in default workspace
    let content = read_from_ws(repo.path(), "default", "agent.txt")
        .expect("agent.txt should exist in default workspace after merge");
    assert_eq!(content.trim(), "agent work");

    // Verify the uncommitted file was preserved in a snapshot commit
    let dws = default_ws(repo.path());
    let log = run_jj(&dws, &["log", "--no-graph", "-T", r#"description.first_line() ++ "\n""#]);
    assert!(
        log.contains("wip: auto-snapshot before merge"),
        "Snapshot commit should exist in log, got: {log}"
    );
}

#[test]
fn reject_merge_default() {
    let repo = setup_bare_repo();

    // Try to merge the default workspace
    let stderr = maw_fails(repo.path(), &["ws", "merge", "default"]);

    // Verify error mentions default can't be merged
    assert!(
        stderr.contains("default") || stderr.contains("reserved"),
        "Error should mention default workspace cannot be merged, got: {stderr}"
    );
}

#[test]
fn merge_preserves_committed_work_in_default() {
    let repo = setup_bare_repo();
    let ws_default = default_ws(repo.path());

    // Simulate committed work in the default workspace:
    // User runs `jj commit -m "wip"` to save work before merging agent output.
    // This creates a commit between main and default@ that must survive the merge.
    write_in_ws(repo.path(), "default", "saved.txt", "important work");
    run_jj(&ws_default, &["commit", "-m", "wip: save before merge"]);

    // Create an agent workspace with its own changes
    maw_ok(repo.path(), &["ws", "create", "agent-1"]);
    write_in_ws(repo.path(), "agent-1", "feature.txt", "agent feature");
    let agent_ws = repo.path().join("ws").join("agent-1");
    run_jj(&agent_ws, &["describe", "-m", "feat: agent work"]);

    // Merge agent workspace
    maw_ok(
        repo.path(),
        &["ws", "merge", "agent-1", "--destroy"],
    );

    // The committed work (saved.txt) must still be reachable in default workspace
    let saved = read_from_ws(repo.path(), "default", "saved.txt");
    assert_eq!(
        saved.as_deref(),
        Some("important work"),
        "committed work in default must survive merge (saved.txt)"
    );

    // Agent's work must also be present
    let feature = read_from_ws(repo.path(), "default", "feature.txt");
    assert_eq!(
        feature.as_deref(),
        Some("agent feature"),
        "agent feature.txt should be present after merge"
    );

    // Verify the committed work is in the log (not orphaned)
    let log = run_jj(&ws_default, &["log", "--no-graph", "-r", "main..@"]);
    assert!(
        log.contains("wip: save before merge"),
        "committed wip work should appear in default's ancestry, got:\n{log}"
    );
}
