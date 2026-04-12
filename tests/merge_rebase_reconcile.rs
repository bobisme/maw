//! Regression tests for bn-3h90.
//!
//! Bug 1: `maw ws merge` was refusing to proceed when the workspace metadata
//! had a stale `rebase_conflict_count > 0`, even after the user resolved the
//! conflict manually via `git add` + `git commit`. `maw ws conflicts`,
//! `maw ws resolve --list`, and `maw ws sync` all reported the workspace
//! clean — only `maw ws merge` trusted the stale counter.
//!
//! The fix: reconcile the persistent counter against the worktree before
//! blocking. If the worktree has no conflict markers, auto-clear the counter
//! and proceed.
//!
//! Bug 2: `maw ws destroy` didn't delete `refs/manifold/head/<name>`, so a
//! later `maw ws create` with the same name inherited a stale oplog chain.
//! The fix: delete the head ref on destroy.

mod manifold_common;

use manifold_common::TestRepo;

/// Force a rebase conflict by having two workspaces both modify line 1 of
/// the same file, then merge one into default and rebase the other.
fn setup_rebase_conflict(repo: &TestRepo) -> String {
    repo.seed_files(&[("shared.txt", "original\n")]);

    // Workspace "a" modifies line 1.
    repo.maw_ok(&["ws", "create", "a"]);
    repo.add_file("a", "shared.txt", "alice\n");
    repo.git_in_workspace("a", &["add", "-A"]);
    repo.git_in_workspace("a", &["commit", "-m", "alice"]);

    // Workspace "b" modifies line 1 differently.
    repo.maw_ok(&["ws", "create", "b"]);
    repo.add_file("b", "shared.txt", "bob\n");
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace("b", &["commit", "-m", "bob"]);

    // Merge "a" into default, advancing the epoch past where "b" was
    // created. Now "b" is stale and rebase will conflict on shared.txt.
    repo.maw_ok(&[
        "ws",
        "merge",
        "a",
        "--into",
        "default",
        "--destroy",
        "--message",
        "merge a",
    ]);

    // Run sync --rebase on "b" — this should hit a conflict and write
    // rebase_conflict_count > 0 to b's metadata.
    let out = repo.maw_raw(&["ws", "sync", "b", "--rebase"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("conflict") || combined.contains("Conflict"),
        "expected rebase to report conflicts\n{combined}"
    );
    combined
}

// ---------------------------------------------------------------------------
// Bug 1: stale rebase_conflict_count reconciles against worktree
// ---------------------------------------------------------------------------

#[test]
fn merge_reconciles_stale_rebase_conflict_counter_after_manual_resolve() {
    let repo = TestRepo::new();
    setup_rebase_conflict(&repo);

    // At this point, b has rebase_conflict_count > 0 in its metadata and
    // the worktree has conflict markers.

    // Simulate the user manually resolving: strip markers, keep both sides.
    let ws_path = repo.root().join("ws").join("b");
    let shared = ws_path.join("shared.txt");
    let content = std::fs::read_to_string(&shared).unwrap();
    assert!(
        content.contains("<<<<<<<") || content.contains(">>>>>>>"),
        "expected markers before manual resolve: {content}"
    );

    // User-style resolve: just overwrite with something sensible.
    std::fs::write(&shared, "alice\nbob\n").unwrap();
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace("b", &["commit", "-m", "manual: keep both"]);

    // Now the worktree is clean but b's metadata counter is still stale.
    // `maw ws merge` should reconcile and proceed.
    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "b",
        "--into",
        "default",
        "--destroy",
        "--message",
        "merge b after manual resolve",
    ]);
    assert!(
        out.status.success(),
        "merge should auto-reconcile and proceed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Final file should have both sides.
    let final_content = std::fs::read_to_string(repo.root().join("ws/default/shared.txt")).unwrap();
    assert!(final_content.contains("alice"));
    assert!(final_content.contains("bob"));
    assert!(!final_content.contains("<<<<<<<"));
}

#[test]
fn merge_force_bypasses_rebase_conflict_counter() {
    let repo = TestRepo::new();
    setup_rebase_conflict(&repo);

    // Resolve manually but leave markers deliberately. Even so, `--force`
    // should let the merge proceed (the downstream merge engine will still
    // detect any actual content conflicts via its own diff3).
    let shared = repo.root().join("ws/b/shared.txt");
    // Just write a clean value without committing markers.
    std::fs::write(&shared, "alice_forced\n").unwrap();
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace("b", &["commit", "-m", "manual: force test"]);

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "b",
        "--into",
        "default",
        "--destroy",
        "--force",
        "--message",
        "merge b with force",
    ]);
    assert!(
        out.status.success(),
        "merge --force should bypass stale counter\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn ws_resolve_list_clears_stale_counter_on_clean_worktree() {
    let repo = TestRepo::new();
    setup_rebase_conflict(&repo);

    // Manually clear the worktree.
    let shared = repo.root().join("ws/b/shared.txt");
    std::fs::write(&shared, "alice\nbob\n").unwrap();
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace("b", &["commit", "-m", "manual"]);

    // `ws resolve b --list` should report clean AND clear the stale counter.
    let _ = repo.maw_raw(&["ws", "resolve", "b", "--list"]);

    // A subsequent merge should now succeed without `--force`.
    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "b",
        "--into",
        "default",
        "--destroy",
        "--message",
        "after resolve list",
    ]);
    assert!(
        out.status.success(),
        "merge should succeed after resolve --list auto-clears counter\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---------------------------------------------------------------------------
// Bug 2: `ws destroy` deletes refs/manifold/head/<name>
// ---------------------------------------------------------------------------

#[test]
fn destroy_cleans_up_oplog_head_ref() {
    let repo = TestRepo::new();

    // Create a workspace and perform some operations so it has an oplog.
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("alice", "a.txt", "content\n");
    repo.git_in_workspace("alice", &["add", "-A"]);
    repo.git_in_workspace("alice", &["commit", "-m", "alice"]);

    // Verify the head ref exists before destroy.
    let head_ref_before = repo.git(&["rev-parse", "--verify", "refs/manifold/head/alice"]);
    assert!(
        !head_ref_before.trim().is_empty(),
        "head ref should exist before destroy"
    );

    // Destroy the workspace with --force.
    repo.maw_ok(&["ws", "destroy", "alice", "--force"]);

    // Head ref should now be gone.
    let head_after = repo
        .maw_raw_exact(&[
            "--",
            "git",
            "-C",
            repo.root().to_str().unwrap(),
            "rev-parse",
            "--verify",
            "refs/manifold/head/alice",
        ])
        .status
        .success();
    // Use the repo's own git wrapper to test the ref directly.
    let result = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "refs/manifold/head/alice"])
        .current_dir(repo.root())
        .output()
        .unwrap();
    assert!(
        !result.status.success(),
        "head ref should be gone after destroy (got success={}, stdout={}, stderr={})",
        head_after,
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

// ---------------------------------------------------------------------------
// Bug from agent follow-up report: file_has_conflict_markers 256KB limit
// silently missed markers in large files, causing ws conflicts and
// ws resolve --list to falsely report clean.
// ---------------------------------------------------------------------------

#[test]
fn find_conflicted_files_detects_markers_past_256kb() {
    let repo = TestRepo::new();
    repo.maw_ok(&["ws", "create", "a"]);

    // Write a large file (~1MB) with conflict markers at the very end.
    let ws_path = repo.root().join("ws").join("a");
    let large_path = ws_path.join("large.txt");
    let mut content = String::with_capacity(1_100_000);
    for i in 0..50_000 {
        content.push_str(&format!("line {i}\n"));
    }
    // Markers at the END of the file, well past the old 256KB read limit.
    content.push_str(
        "\n<<<<<<< alice\nalice_content\n=======\nbob_content\n>>>>>>> bob\n",
    );
    std::fs::write(&large_path, &content).unwrap();
    assert!(
        std::fs::metadata(&large_path).unwrap().len() > 256 * 1024,
        "test file should exceed 256KB"
    );

    // `ws resolve --list` calls find_conflicted_files internally. It should
    // detect the markers and report them.
    let out = repo.maw_raw(&["ws", "resolve", "a", "--list"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("large.txt") || combined.contains("1 conflicted"),
        "ws resolve --list should detect markers in large file. Got:\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !combined.contains("No conflicted files"),
        "ws resolve --list falsely reported clean. Got: {combined}"
    );
}

#[test]
fn ws_merge_refuses_workspace_with_embedded_markers_in_large_file() {
    let repo = TestRepo::new();
    repo.seed_files(&[("shared.txt", "line1\nline2\n")]);

    repo.maw_ok(&["ws", "create", "a"]);

    // Modify the file AND add a large file with markers committed into HEAD.
    // This simulates the state after `sync --rebase` committed conflict markers.
    let ws_path = repo.root().join("ws").join("a");
    repo.add_file("a", "shared.txt", "line1\nline2\nline3\n");

    let mut large = String::with_capacity(600_000);
    for i in 0..30_000 {
        large.push_str(&format!("entry {i}\n"));
    }
    large.push_str(
        "\n<<<<<<< HEAD\nours version\n=======\ntheirs version\n>>>>>>> other\n",
    );
    std::fs::write(ws_path.join("manifest.txt"), &large).unwrap();

    repo.git_in_workspace("a", &["add", "-A"]);
    repo.git_in_workspace("a", &["commit", "-m", "rebase: conflict replaying (simulated)"]);

    // ws merge should refuse because the worktree has markers.
    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "a",
        "--into",
        "default",
        "--destroy",
        "--message",
        "should fail",
    ]);
    assert!(
        !out.status.success(),
        "merge should refuse workspace with embedded markers\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("manifest.txt") || stderr.contains("conflict marker"),
        "error should mention the marker file: {stderr}"
    );
}

#[test]
fn ws_conflicts_reports_embedded_markers_when_engine_is_clean() {
    let repo = TestRepo::new();
    repo.seed_files(&[("file.txt", "base\n")]);

    repo.maw_ok(&["ws", "create", "a"]);

    // Commit a file with embedded markers but don't modify the "tracked" file.
    // The merge engine will see a clean 1-side modification; our embedded-
    // marker scan should still catch it.
    let ws_path = repo.root().join("ws").join("a");
    let marker_content = "head\n<<<<<<< alice\nx\n=======\ny\n>>>>>>> bob\ntail\n";
    std::fs::write(ws_path.join("dirty.txt"), marker_content).unwrap();
    repo.git_in_workspace("a", &["add", "-A"]);
    repo.git_in_workspace("a", &["commit", "-m", "simulated rebase conflict"]);

    let out = repo.maw_raw(&["ws", "conflicts", "a"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("embedded conflict markers")
            || combined.contains("dirty.txt"),
        "ws conflicts should surface embedded markers. Got:\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !combined.contains("No conflicts found"),
        "ws conflicts should NOT report clean. Got: {combined}"
    );
}

#[test]
fn file_has_conflict_markers_skips_files_over_256mb() {
    // This is a smoke test — we don't actually create a 256MB+ file in a
    // unit test. Just verify that a normal-sized file with markers IS
    // detected (proves the new code path works for typical sizes).
    let repo = TestRepo::new();
    repo.maw_ok(&["ws", "create", "a"]);
    let ws_path = repo.root().join("ws").join("a");
    std::fs::write(
        ws_path.join("ok.txt"),
        "line1\n<<<<<<< foo\nours\n=======\ntheirs\n>>>>>>> bar\nline2\n",
    )
    .unwrap();

    let out = repo.maw_raw(&["ws", "resolve", "a", "--list"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{}{}", stdout, String::from_utf8_lossy(&out.stderr));
    assert!(
        combined.contains("ok.txt") && !combined.contains("No conflicted files"),
        "normal-sized file with markers should be detected: {combined}"
    );
}

#[test]
fn destroy_then_create_same_name_starts_fresh_oplog_chain() {
    let repo = TestRepo::new();

    // First lifecycle: create, touch, destroy.
    repo.maw_ok(&["ws", "create", "worker"]);
    repo.add_file("worker", "first.txt", "first\n");
    repo.git_in_workspace("worker", &["add", "-A"]);
    repo.git_in_workspace("worker", &["commit", "-m", "first"]);
    let first_head = std::process::Command::new("git")
        .args(["rev-parse", "refs/manifold/head/worker"])
        .current_dir(repo.root())
        .output()
        .unwrap();
    let first_head_oid = String::from_utf8_lossy(&first_head.stdout).trim().to_owned();
    assert!(!first_head_oid.is_empty());

    repo.maw_ok(&["ws", "destroy", "worker", "--force"]);

    // Second lifecycle: create with same name — should start fresh, NOT
    // inherit the old chain.
    repo.maw_ok(&["ws", "create", "worker"]);
    let second_head = std::process::Command::new("git")
        .args(["rev-parse", "refs/manifold/head/worker"])
        .current_dir(repo.root())
        .output()
        .unwrap();
    let second_head_oid = String::from_utf8_lossy(&second_head.stdout).trim().to_owned();
    assert!(!second_head_oid.is_empty());

    // The new head must not equal the old head (because the old one was deleted).
    assert_ne!(
        first_head_oid, second_head_oid,
        "recreated workspace should have a fresh oplog chain, not inherit the destroyed one"
    );
}

// ---------------------------------------------------------------------------
// bn-3kcp: destroy iterates the `workspace_owned_refs` set so every ref
// kind a workspace owns is cleaned up — not just the three kinds that
// happened to exist when destroy was first written.
// ---------------------------------------------------------------------------

#[test]
fn destroy_deletes_all_workspace_owned_refs() {
    use maw_core::refs::workspace_owned_refs;

    let repo = TestRepo::new();

    // Create a workspace and perform operations that should populate every
    // ref kind a workspace owns:
    //   - refs/manifold/ws/alice         (materialized state, written on snapshot)
    //   - refs/manifold/epoch/ws/alice   (creation epoch, written at create time)
    //   - refs/manifold/head/alice       (oplog head, written on commits)
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("alice", "a.txt", "content\n");
    repo.git_in_workspace("alice", &["add", "-A"]);
    repo.git_in_workspace("alice", &["commit", "-m", "alice touches a.txt"]);

    // Before destroy: every owned ref exists. (Some may be absent if that
    // particular ref kind hasn't been materialized yet — we only assert at
    // least one exists, because this test's primary goal is the "after"
    // assertion. The `destroy_cleans_up_oplog_head_ref` test already covers
    // the head ref specifically.)
    let owned = workspace_owned_refs("alice");
    assert!(
        owned.len() >= 3,
        "workspace_owned_refs must contain at least 3 entries"
    );

    let mut existed_before = 0;
    for ref_name in &owned {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "--verify", ref_name])
            .current_dir(repo.root())
            .output()
            .unwrap();
        if out.status.success() {
            existed_before += 1;
        }
    }
    assert!(
        existed_before >= 1,
        "at least one owned ref must exist before destroy (got 0 of {}): {:?}",
        owned.len(),
        owned
    );

    // Destroy with --force.
    repo.maw_ok(&["ws", "destroy", "alice", "--force"]);

    // After destroy: every owned ref must be gone, regardless of kind.
    for ref_name in &owned {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "--verify", ref_name])
            .current_dir(repo.root())
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "owned ref {ref_name} still exists after destroy \
             (stdout={}, stderr={})",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
}
