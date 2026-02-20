//! Integration tests for Level 2 Git transport (push/pull refs/manifold/*).
//!
//! Tests the full push/pull round-trip between two repos via a shared bare
//! remote, verifying that Manifold state (op logs, workspace heads, epoch
//! pointer) is correctly synchronized.

mod manifold_common;

use std::path::Path;
use std::process::Command;

use manifold_common::{TestRepo, git_ok};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read a git ref in the given repo, returning the OID or None.
fn read_ref(repo: &Path, ref_name: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", ref_name])
        .current_dir(repo)
        .output()
        .expect("failed to run git rev-parse");
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

/// Write a git ref unconditionally.
fn write_ref(repo: &Path, ref_name: &str, oid: &str) {
    git_ok(repo, &["update-ref", ref_name, oid]);
}

/// Create an empty blob and return its OID (simulates an op log head).
fn create_test_blob(repo: &Path, content: &str) -> String {
    use std::io::Write;
    let mut child = Command::new("git")
        .args(["hash-object", "-w", "--stdin"])
        .current_dir(repo)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn git hash-object");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(content.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("failed to wait for git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Create a git commit (not a blob) and return its OID.
/// Used for epoch and ws refs which must be commits for merge-base to work.
#[allow(dead_code)]
fn create_test_commit(repo: &Path, message: &str) -> String {
    let out = Command::new("git")
        .args(["commit", "--allow-empty", "-m", message])
        .current_dir(repo)
        .output()
        .expect("failed to create commit");
    assert!(out.status.success(), 
        "Failed to create commit: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    git_ok(repo, &["rev-parse", "HEAD"]).trim().to_string()
}

/// Set up a second Manifold repo by cloning the bare remote.
/// Returns (`local_root`, _`temp_dir_holder`).
fn clone_from_bare(
    bare_remote: &Path,
    ws_name: &str,
) -> (std::path::PathBuf, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().expect("failed to create temp dir for clone");
    let root = dir.path().to_path_buf();

    git_ok(
        &std::env::temp_dir(),
        &[
            "clone",
            bare_remote.to_str().unwrap(),
            root.to_str().unwrap(),
        ],
    );

    git_ok(&root, &["config", "user.name", "Test"]);
    git_ok(&root, &["config", "user.email", "test@localhost"]);
    git_ok(&root, &["config", "commit.gpgsign", "false"]);

    // Initialize Manifold layout in the clone.
    let manifold_dir = root.join(".manifold");
    std::fs::create_dir_all(manifold_dir.join("epochs")).unwrap();
    std::fs::create_dir_all(manifold_dir.join("artifacts").join("ws")).unwrap();
    std::fs::create_dir_all(manifold_dir.join("artifacts").join("merge")).unwrap();
    std::fs::write(
        manifold_dir.join("config.toml"),
        "[repo]\nbranch = \"main\"\n",
    )
    .unwrap();

    // Create ws/ layout.
    let ws_dir = root.join("ws");
    std::fs::create_dir_all(&ws_dir).unwrap();
    let ws_default = ws_dir.join(ws_name);
    let head = git_ok(&root, &["rev-parse", "HEAD"]).trim().to_string();
    git_ok(
        &root,
        &[
            "worktree",
            "add",
            "--detach",
            ws_default.to_str().unwrap(),
            &head,
        ],
    );

    (root, dir)
}

// ---------------------------------------------------------------------------
// Test: push_manifold_refs pushes all refs/manifold/* to remote
// ---------------------------------------------------------------------------

#[test]
fn push_manifold_refs_sends_all_manifold_refs_to_remote() {
    let (repo_a, bare_remote) = TestRepo::with_remote();
    let remote_path = bare_remote.path();
    let root = repo_a.root();

    // Set up some manifold refs in repo A.
    let blob1 = create_test_blob(root, r#"{"parent_ids":[],"workspace_id":"ws-a","timestamp":"2026-02-19T12:00:00Z","payload":{"type":"destroy"}}"#);
    let blob2 = create_test_blob(root, r#"{"parent_ids":[],"workspace_id":"ws-b","timestamp":"2026-02-19T12:00:00Z","payload":{"type":"destroy"}}"#);

    write_ref(root, "refs/manifold/head/ws-a", &blob1);
    write_ref(root, "refs/manifold/head/ws-b", &blob2);

    // Set epoch ref to the existing epoch0 commit.
    let epoch0 = repo_a.epoch0();
    write_ref(root, "refs/manifold/epoch/current", epoch0);

    // Before push: remote should have no manifold refs.
    assert!(
        read_ref(remote_path, "refs/manifold/head/ws-a").is_none(),
        "Remote should not have manifold refs before push"
    );

    // Push manifold refs.
    let out = Command::new(env!("CARGO_BIN_EXE_maw"))
        .args(["push", "--manifold", "--no-tags"])
        .current_dir(root)
        .output()
        .expect("failed to run maw push --manifold");

    assert!(out.status.success(), 
        "maw push --manifold failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // After push: remote should have the manifold refs.
    let remote_ws_a = read_ref(remote_path, "refs/manifold/head/ws-a");
    let remote_ws_b = read_ref(remote_path, "refs/manifold/head/ws-b");
    let remote_epoch = read_ref(remote_path, "refs/manifold/epoch/current");

    assert_eq!(
        remote_ws_a.as_deref(),
        Some(blob1.as_str()),
        "Remote ws-a head should match local"
    );
    assert_eq!(
        remote_ws_b.as_deref(),
        Some(blob2.as_str()),
        "Remote ws-b head should match local"
    );
    assert_eq!(
        remote_epoch.as_deref(),
        Some(epoch0),
        "Remote epoch should match local"
    );
}

// ---------------------------------------------------------------------------
// Test: pull_manifold_refs fast-forwards local from remote
// ---------------------------------------------------------------------------

#[test]
fn pull_manifold_refs_fast_forwards_workspace_head() {
    let (repo_a, bare_remote) = TestRepo::with_remote();
    let remote_path = bare_remote.path();
    let root_a = repo_a.root();

    // Set up a manifold head ref on remote directly (simulating machine A pushed it).
    let blob = create_test_blob(root_a, r#"{"parent_ids":[],"workspace_id":"agent-1","timestamp":"2026-02-19T12:00:00Z","payload":{"type":"destroy"}}"#);

    // Push the blob object to the bare remote so it exists there.
    // We push the refs/manifold/* from repo A which has the blob.
    write_ref(root_a, "refs/manifold/head/agent-1", &blob);

    // Use git push directly to put the ref in the bare remote.
    git_ok(
        root_a,
        &[
            "push",
            "--force",
            "origin",
            "refs/manifold/head/agent-1:refs/manifold/head/agent-1",
        ],
    );

    // Clone from remote to create "machine B" without any local manifold refs.
    let (root_b, _dir_b) = clone_from_bare(remote_path, "default");

    // Before pull: machine B has no manifold head refs.
    assert!(
        read_ref(&root_b, "refs/manifold/head/agent-1").is_none(),
        "Machine B should not have agent-1 head before pull"
    );

    // Pull manifold refs.
    let out = Command::new(env!("CARGO_BIN_EXE_maw"))
        .args(["pull", "--manifold", "origin"])
        .current_dir(&root_b)
        .output()
        .expect("failed to run maw pull --manifold");

    assert!(out.status.success(), 
        "maw pull --manifold failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // After pull: machine B should have the head ref.
    let local_head = read_ref(&root_b, "refs/manifold/head/agent-1");
    assert_eq!(
        local_head.as_deref(),
        Some(blob.as_str()),
        "Machine B should have fast-forwarded agent-1 head from remote"
    );
}

// ---------------------------------------------------------------------------
// Test: round-trip push + pull produces equivalent state
// ---------------------------------------------------------------------------

#[test]
fn round_trip_push_pull_produces_equivalent_state() {
    let (repo_a, bare_remote) = TestRepo::with_remote();
    let remote_path = bare_remote.path();
    let root_a = repo_a.root();
    let epoch0 = repo_a.epoch0().to_string();

    // Set up manifold state on machine A: two workspace heads + epoch.
    let head_ws1 = create_test_blob(
        root_a,
        r#"{"parent_ids":[],"workspace_id":"ws-1","timestamp":"2026-02-19T12:00:00Z","payload":{"type":"destroy"}}"#,
    );
    let head_ws2 = create_test_blob(
        root_a,
        r#"{"parent_ids":[],"workspace_id":"ws-2","timestamp":"2026-02-19T13:00:00Z","payload":{"type":"destroy"}}"#,
    );

    write_ref(root_a, "refs/manifold/head/ws-1", &head_ws1);
    write_ref(root_a, "refs/manifold/head/ws-2", &head_ws2);
    write_ref(root_a, "refs/manifold/epoch/current", &epoch0);

    // Push from machine A.
    let push_out = Command::new(env!("CARGO_BIN_EXE_maw"))
        .args(["push", "--manifold", "--no-tags"])
        .current_dir(root_a)
        .output()
        .expect("failed to run maw push --manifold");

    assert!(
        push_out.status.success(),
        "maw push --manifold failed: {}",
        String::from_utf8_lossy(&push_out.stderr)
    );

    // Clone to machine B.
    let (root_b, _dir_b) = clone_from_bare(remote_path, "default");

    // Pull to machine B.
    let pull_out = Command::new(env!("CARGO_BIN_EXE_maw"))
        .args(["pull", "--manifold", "origin"])
        .current_dir(&root_b)
        .output()
        .expect("failed to run maw pull --manifold");

    assert!(
        pull_out.status.success(),
        "maw pull --manifold failed: {}",
        String::from_utf8_lossy(&pull_out.stderr)
    );

    // Verify: machine B state matches machine A state.
    let b_ws1 = read_ref(&root_b, "refs/manifold/head/ws-1");
    let b_ws2 = read_ref(&root_b, "refs/manifold/head/ws-2");
    let b_epoch = read_ref(&root_b, "refs/manifold/epoch/current");

    assert_eq!(
        b_ws1.as_deref(),
        Some(head_ws1.as_str()),
        "Round-trip: ws-1 head should match"
    );
    assert_eq!(
        b_ws2.as_deref(),
        Some(head_ws2.as_str()),
        "Round-trip: ws-2 head should match"
    );
    assert_eq!(
        b_epoch.as_deref(),
        Some(epoch0.as_str()),
        "Round-trip: epoch should match"
    );
}

// ---------------------------------------------------------------------------
// Test: pull merges divergent op log heads
// ---------------------------------------------------------------------------

#[test]
fn pull_creates_merge_op_for_divergent_heads() {
    let (repo_a, bare_remote) = TestRepo::with_remote();
    let remote_path = bare_remote.path();
    let root_a = repo_a.root();

    // Create initial head blob and push to remote.
    let initial_blob = create_test_blob(
        root_a,
        r#"{"parent_ids":[],"workspace_id":"agent-x","timestamp":"2026-02-19T10:00:00Z","payload":{"type":"destroy"}}"#,
    );
    write_ref(root_a, "refs/manifold/head/agent-x", &initial_blob);
    git_ok(
        root_a,
        &[
            "push",
            "--force",
            "origin",
            "refs/manifold/head/agent-x:refs/manifold/head/agent-x",
        ],
    );

    // Clone to machine B.
    let (root_b, _dir_b) = clone_from_bare(remote_path, "default");

    // Machine B fast-forwards to get the initial head.
    let pull_out1 = Command::new(env!("CARGO_BIN_EXE_maw"))
        .args(["pull", "--manifold", "origin"])
        .current_dir(&root_b)
        .output()
        .expect("failed to run first pull");
    assert!(pull_out1.status.success(), "First pull failed");

    assert_eq!(
        read_ref(&root_b, "refs/manifold/head/agent-x").as_deref(),
        Some(initial_blob.as_str()),
        "Machine B should have initial head after first pull"
    );

    // Now machine A advances: creates a new op on top of initial.
    let remote_advance_blob = create_test_blob(
        root_a,
        &format!(
            r#"{{"parent_ids":["{initial_blob}"],"workspace_id":"agent-x","timestamp":"2026-02-19T11:00:00Z","payload":{{"type":"destroy"}}}}"#
        ),
    );
    write_ref(root_a, "refs/manifold/head/agent-x", &remote_advance_blob);
    git_ok(
        root_a,
        &[
            "push",
            "--force",
            "origin",
            "refs/manifold/head/agent-x:refs/manifold/head/agent-x",
        ],
    );

    // Machine B also advances independently (divergence!).
    let local_advance_blob = create_test_blob(
        &root_b,
        &format!(
            r#"{{"parent_ids":["{initial_blob}"],"workspace_id":"agent-x","timestamp":"2026-02-19T11:30:00Z","payload":{{"type":"destroy"}}}}"#
        ),
    );
    write_ref(
        &root_b,
        "refs/manifold/head/agent-x",
        &local_advance_blob,
    );

    // Now machine B pulls: local and remote have diverged from initial_blob.
    // pull should create a merge op with both as parents.
    let pull_out2 = Command::new(env!("CARGO_BIN_EXE_maw"))
        .args(["pull", "--manifold", "origin"])
        .current_dir(&root_b)
        .output()
        .expect("failed to run diverged pull");
    assert!(
        pull_out2.status.success(),
        "Diverged pull failed: {}",
        String::from_utf8_lossy(&pull_out2.stderr)
    );

    // After diverged pull: the head should have moved to a NEW merge op,
    // not be either of the diverged heads.
    let merged_head = read_ref(&root_b, "refs/manifold/head/agent-x")
        .expect("head should exist after merge");

    assert_ne!(
        merged_head, local_advance_blob,
        "Head should be new merge op, not local-only advance"
    );
    assert_ne!(
        merged_head, remote_advance_blob,
        "Head should be new merge op, not remote-only advance"
    );

    // Verify the merge op blob has both diverged heads as parents.
    let blob_content = Command::new("git")
        .args(["cat-file", "-p", &merged_head])
        .current_dir(&root_b)
        .output()
        .expect("failed to read merged head blob");
    let blob_str = String::from_utf8_lossy(&blob_content.stdout).to_string();

    assert!(
        blob_str.contains(&local_advance_blob),
        "Merge op should include local head as parent: {blob_str}"
    );
    assert!(
        blob_str.contains(&remote_advance_blob),
        "Merge op should include remote head as parent: {blob_str}"
    );
}

// ---------------------------------------------------------------------------
// Test: pull without --manifold flag gives clear error
// ---------------------------------------------------------------------------

#[test]
fn pull_without_manifold_flag_gives_clear_error() {
    let repo = TestRepo::new();

    let out = Command::new(env!("CARGO_BIN_EXE_maw"))
        .args(["pull"])
        .current_dir(repo.root())
        .output()
        .expect("failed to run maw pull");

    assert!(
        !out.status.success(),
        "maw pull without --manifold should fail"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--manifold"),
        "Error should mention --manifold flag: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Test: push --manifold with no manifold refs gives helpful message
// ---------------------------------------------------------------------------

#[test]
fn push_manifold_with_no_refs_gives_helpful_message() {
    let (repo, _bare) = TestRepo::with_remote();

    // Remove the epoch ref so there are truly no manifold refs.
    let _ = Command::new("git")
        .args(["update-ref", "-d", "refs/manifold/epoch/current"])
        .current_dir(repo.root())
        .output();

    let out = Command::new(env!("CARGO_BIN_EXE_maw"))
        .args(["push", "--manifold", "--no-tags"])
        .current_dir(repo.root())
        .output()
        .expect("failed to run maw push --manifold");

    // Should succeed (exit 0) with an informational message.
    assert!(
        out.status.success(),
        "maw push --manifold should succeed even with no refs:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("nothing to push") || stdout.contains("No refs"),
        "Should give informational message: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// Test: dry-run doesn't modify any refs
// ---------------------------------------------------------------------------

#[test]
fn pull_dry_run_does_not_modify_refs() {
    let (repo_a, bare_remote) = TestRepo::with_remote();
    let remote_path = bare_remote.path();
    let root_a = repo_a.root();

    // Push a manifold head to the remote.
    let blob = create_test_blob(
        root_a,
        r#"{"parent_ids":[],"workspace_id":"dry-ws","timestamp":"2026-02-19T12:00:00Z","payload":{"type":"destroy"}}"#,
    );
    write_ref(root_a, "refs/manifold/head/dry-ws", &blob);
    git_ok(
        root_a,
        &[
            "push",
            "origin",
            "refs/manifold/head/dry-ws:refs/manifold/head/dry-ws",
        ],
    );

    // Clone to machine B.
    let (root_b, _dir_b) = clone_from_bare(remote_path, "default");

    // Dry-run pull.
    let out = Command::new(env!("CARGO_BIN_EXE_maw"))
        .args(["pull", "--manifold", "--dry-run", "origin"])
        .current_dir(&root_b)
        .output()
        .expect("failed to run maw pull --manifold --dry-run");

    assert!(
        out.status.success(),
        "dry-run pull should succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // After dry-run: no refs should have been set.
    let local_head = read_ref(&root_b, "refs/manifold/head/dry-ws");
    assert!(
        local_head.is_none(),
        "dry-run should not modify refs: got {local_head:?}"
    );

    // But staging refs might exist (still cleaned up even in dry-run).
    // Main assertion: local ref was NOT updated.
    assert!(
        local_head.is_none(),
        "Dry-run must leave local refs unchanged"
    );
}
