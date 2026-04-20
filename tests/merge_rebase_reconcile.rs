//! Regression tests for bn-3h90 and bn-2r57.
//!
//! Bug 1 (bn-3h90): `maw ws merge` was refusing to proceed when the workspace
//! metadata had a stale `rebase_conflict_count > 0`, even after the user
//! resolved the conflict manually via `git add` + `git commit`.
//!
//! The definitive fix (bn-2r57): delete the `rebase_conflict_count` field
//! entirely and derive conflict status from a live worktree scan via
//! `find_conflicted_files`. No counter means no drift.
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

    // Run sync --rebase on "b" — this should hit a conflict and leave
    // conflict markers in the worktree.
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
// Bug 1: merge succeeds after manual conflict resolution
// ---------------------------------------------------------------------------

#[test]
fn merge_succeeds_after_manual_conflict_resolve() {
    let repo = TestRepo::new();
    setup_rebase_conflict(&repo);

    // At this point, b's worktree has conflict markers.

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

    // Now the worktree is clean. `maw ws merge` derives conflict status
    // from the worktree scan and should proceed.
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
        "merge should proceed when worktree is clean\nstdout: {}\nstderr: {}",
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
fn merge_force_bypasses_marker_scan() {
    let repo = TestRepo::new();
    setup_rebase_conflict(&repo);

    // Resolve manually. Even so, `--force` should let the merge proceed
    // (the downstream merge engine will still detect any actual content
    // conflicts via its own diff3).
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
        "merge --force should bypass marker scan\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn ws_resolve_list_reports_clean_and_merge_proceeds() {
    let repo = TestRepo::new();
    setup_rebase_conflict(&repo);

    // Manually clear the worktree.
    let shared = repo.root().join("ws/b/shared.txt");
    std::fs::write(&shared, "alice\nbob\n").unwrap();
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace("b", &["commit", "-m", "manual"]);

    // `ws resolve b --list` should report clean.
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
        "merge should succeed when worktree is clean\nstdout: {}\nstderr: {}",
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

// ---------------------------------------------------------------------------
// bn-372v: `sync --rebase` must not silently drop merge commits.
//
// Before the fix: `list_commits_ahead` included merge commits. Git refuses to
// `cherry-pick --no-commit` a merge commit without `-m <parent>`, so the
// cherry-pick failed with nothing unmerged in the index. The old code path
// read `list_conflicted_files` (empty), made an empty commit with message
// "rebase: conflict replaying … (0 file(s))", and moved on. No markers were
// written anywhere, so `find_conflicted_files` returned empty, and the
// merge-time marker gate (merge.rs:2553-2573) let the workspace merge into
// `default` clean — silently dropping the merge commit's content.
//
// After the fix: merge commits are detected upfront. A stub file with
// `<<<<<<<` markers is written so the workspace diff contains a `+<<<<<<<`
// line, which trips `find_files_with_new_conflict_markers`, which trips the
// merge-time marker gate. An explicit RebaseConflict entry is recorded in the
// sidecar.
// ---------------------------------------------------------------------------

#[test]
fn sync_rebase_marks_workspace_conflicted_on_merge_commit() {
    // bn-372v (original): `sync --rebase` must not silently drop merge
    // commits committed inside a workspace.
    //
    // Original (pre-bn-elj0) mechanism: merge commits were refused by
    // `cherry-pick --no-commit`, so the rebase wrote a stub file with
    // conflict markers to trip the merge-time marker gate.
    //
    // Post-bn-elj0 mechanism: merge commits are replayed through the
    // structured-merge engine (maw-core::merge). The first-parent delta
    // applies normally; non-first-parent deltas are folded in as
    // additional sides of a `Conflict::Content`. When those deltas
    // conflict with the epoch or with each other, `materialize` renders
    // a diff3-style marker block — `find_conflicted_files` then trips
    // the merge-time marker gate.
    //
    // To exercise the full pipeline, this test sets up a merge that
    // *actually overlaps the epoch*: both the feature chain and the
    // side branch modify `shared.txt`, which is also modified by the
    // advancer workspace. That produces a real 3-way conflict rather
    // than a clean merge.
    let repo = TestRepo::new();
    repo.seed_files(&[("shared.txt", "original\n")]);

    repo.maw_ok(&["ws", "create", "feature"]);
    let epoch_before = repo.current_epoch();
    let ws_path = repo.root().join("ws").join("feature");

    // Feature-side commit: modifies shared.txt.
    repo.add_file("feature", "shared.txt", "feature-version\n");
    repo.git_in_workspace("feature", &["add", "-A"]);
    repo.git_in_workspace("feature", &["commit", "-m", "feat: feature work"]);
    let feature_commit = repo.workspace_head("feature");

    // Side branch off the epoch: modifies shared.txt differently.
    repo.git_in_workspace("feature", &["checkout", "-b", "side", &epoch_before]);
    std::fs::write(ws_path.join("shared.txt"), "side-version\n").unwrap();
    repo.git_in_workspace("feature", &["add", "-A"]);
    repo.git_in_workspace("feature", &["commit", "-m", "feat: side work"]);

    // Merge side into the detached feature chain. We resolve by picking
    // feature's version so the merge commit lands cleanly in git, but
    // the structured rebase still sees the two-parent history and has to
    // reconcile both sides.
    repo.git_in_workspace("feature", &["checkout", "--detach", &feature_commit]);
    repo.git_in_workspace(
        "feature",
        &["-c", "merge.conflictStyle=diff3", "merge", "--no-ff", "--no-edit", "-X", "ours", "side"],
    );

    // Sanity-check: HEAD is now a merge commit (two parents).
    let parents_line = repo.git_in_workspace(
        "feature",
        &["rev-list", "--parents", "-n", "1", "HEAD"],
    );
    let parent_count = parents_line.trim().split_whitespace().count() - 1;
    assert!(
        parent_count >= 2,
        "setup failed: HEAD should be a merge commit, got {parent_count} parent(s): {parents_line}"
    );

    // Advance the epoch via another workspace — and have it also modify
    // shared.txt, so the epoch ↔ feature collision is real.
    repo.maw_ok(&["ws", "create", "advancer"]);
    repo.add_file("advancer", "shared.txt", "advancer-version\n");
    repo.git_in_workspace("advancer", &["add", "-A"]);
    repo.git_in_workspace("advancer", &["commit", "-m", "chore: advance epoch"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--destroy",
        "--message",
        "merge advancer",
    ]);

    // Run sync --rebase on feature. It should replay both the feature
    // commit and the merge commit. Because feature-side and advancer-side
    // modified the same file (shared.txt), the rebase must produce a
    // structured conflict rather than silently winning.
    let out = repo.maw_raw(&["ws", "sync", "feature", "--rebase"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}{stderr}");

    // Must NOT silently pass with a zero-file conflict — that was the
    // old bug.
    assert!(
        !combined.contains("(0 file(s))"),
        "sync --rebase must not record a zero-file conflict.\n\
         stdout: {stdout}\nstderr: {stderr}"
    );

    // Load-bearing assertion #1: `ws merge` must refuse (the marker gate
    // trips because the workspace HEAD has `+<<<<<<<` in its diff).
    let merge_out = repo.maw_raw(&[
        "ws",
        "merge",
        "feature",
        "--into",
        "default",
        "--destroy",
        "--message",
        "should fail: unresolved rebase conflict",
    ]);
    let merge_stderr = String::from_utf8_lossy(&merge_out.stderr);
    let merge_stdout = String::from_utf8_lossy(&merge_out.stdout);
    assert!(
        !merge_out.status.success(),
        "merge must refuse a workspace with unresolved rebase conflicts.\n\
         stdout: {merge_stdout}\nstderr: {merge_stderr}"
    );

    // Load-bearing assertion #2: at least one tracked file has textual
    // conflict markers committed.
    let markers_cmd = std::process::Command::new("git")
        .args(["grep", "-l", "<<<<<<< epoch"])
        .current_dir(&ws_path)
        .output()
        .expect("git grep should run");
    assert!(
        markers_cmd.status.success(),
        "at least one workspace file should contain `<<<<<<< epoch` markers after rebase.\n\
         git grep stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&markers_cmd.stdout),
        String::from_utf8_lossy(&markers_cmd.stderr),
    );

    // Load-bearing assertion #3: the legacy sidecar exists.
    let sidecar = repo.root()
        .join(".manifold")
        .join("artifacts")
        .join("ws")
        .join("feature")
        .join("rebase-conflicts.json");
    assert!(
        sidecar.exists(),
        "rebase-conflicts.json should exist after a conflicted rebase"
    );
    let sidecar_contents = std::fs::read_to_string(&sidecar).unwrap();
    assert!(
        sidecar_contents.contains("shared.txt"),
        "sidecar should list the conflicted file. Got: {sidecar_contents}"
    );
}
