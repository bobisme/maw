//! Phase 7 (bn-28rd) — lock the observable contract of `maw ws sync --rebase`
//! now that it routes through `maw-core::merge`.
//!
//! These tests codify the invariants the bn-gjm8 refactor was designed to
//! preserve:
//!
//!   * Commit-count parity (a rebase of N commits produces N commits).
//!   * Commit-message preservation (oldest-first, unchanged).
//!   * No content drops on a clean replay.
//!   * File-mode fidelity (executable bit, symlink).
//!   * Structured conflict tree lands in `conflict-tree.json` sidecar.
//!   * The narrow fidelity property: a workspace that commits a conflicting
//!     edit AND an unrelated clean edit in separate commits must end up
//!     with (a) a `Conflict::Content` for the conflicting file and (b) a
//!     clean post-edit value for the unrelated file — i.e. B's trivial
//!     commit does NOT collapse A's structured conflict.

mod manifold_common;

use manifold_common::TestRepo;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Commit every dirty path in `workspace` with `message`.
fn commit_all(repo: &TestRepo, workspace: &str, message: &str) {
    repo.git_in_workspace(workspace, &["add", "-A"]);
    repo.git_in_workspace(workspace, &["commit", "-m", message]);
}

/// Walk the commit chain in `workspace` from HEAD backwards (inclusive) until
/// `boundary` (exclusive) and return the subject line of each commit.
///
/// Uses `git log --format=%s <boundary>..HEAD` so the returned list is
/// newest-first. Callers that want oldest-first should `.reverse()`.
fn commit_subjects(repo: &TestRepo, workspace: &str, boundary: &str) -> Vec<String> {
    let range = format!("{boundary}..HEAD");
    let out = repo.git_in_workspace(workspace, &["log", "--format=%s", &range]);
    out.lines().map(str::to_owned).collect()
}

/// `git rev-list --count <boundary>..HEAD` in a workspace.
fn commits_ahead(repo: &TestRepo, workspace: &str, boundary: &str) -> u32 {
    let range = format!("{boundary}..HEAD");
    let out = repo.git_in_workspace(workspace, &["rev-list", "--count", &range]);
    out.trim()
        .parse::<u32>()
        .unwrap_or_else(|e| panic!("rev-list --count did not produce a number ({out:?}): {e}"))
}

/// Locate a `Conflict::Content` (or any conflict shape) entry in the parsed
/// `conflict-tree.json` for `path`. Returns `None` if not present.
fn find_conflict_entry<'a>(
    sidecar: &'a serde_json::Value,
    path: &str,
) -> Option<&'a serde_json::Value> {
    let conflicts = sidecar.get("conflicts")?.as_object()?;
    conflicts.get(path)
}

// ---------------------------------------------------------------------------
// 1. Commit-count preservation
// ---------------------------------------------------------------------------

#[test]
fn sync_rebase_preserves_commit_count() {
    let repo = TestRepo::new();
    repo.seed_files(&[("main.rs", "fn main() {}\n")]);

    let before_epoch = repo.current_epoch();

    // Workspace with three commits ahead of epoch, not conflicting with
    // whatever the advancer later does.
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("alice", "a.txt", "a-v1\n");
    commit_all(&repo, "alice", "feat: A");
    repo.add_file("alice", "b.txt", "b-v1\n");
    commit_all(&repo, "alice", "fix: B");
    repo.add_file("alice", "c.txt", "c-v1\n");
    commit_all(&repo, "alice", "chore: C");

    assert_eq!(
        commits_ahead(&repo, "alice", &before_epoch),
        3,
        "setup: alice should have 3 commits ahead of the old epoch"
    );

    // Advance the epoch via an unrelated workspace.
    repo.maw_ok(&["ws", "create", "advancer"]);
    repo.add_file("advancer", "unrelated.txt", "advance\n");
    commit_all(&repo, "advancer", "chore: advance");
    repo.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge advancer",
    ]);

    let new_epoch = repo.current_epoch();
    assert_ne!(before_epoch, new_epoch, "setup: epoch should have advanced");

    // Rebase alice.
    let out = repo.maw_raw(&["ws", "sync", "alice", "--rebase"]);
    assert!(
        out.status.success(),
        "sync --rebase failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(
        commits_ahead(&repo, "alice", &new_epoch),
        3,
        "rebase must preserve commit count (3 commits in, 3 commits out)"
    );
}

// ---------------------------------------------------------------------------
// 2. Commit-message preservation
// ---------------------------------------------------------------------------

#[test]
fn sync_rebase_preserves_commit_messages() {
    let repo = TestRepo::new();
    repo.seed_files(&[("main.rs", "fn main() {}\n")]);

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("alice", "a.txt", "a-v1\n");
    commit_all(&repo, "alice", "feat: A");
    repo.add_file("alice", "b.txt", "b-v1\n");
    commit_all(&repo, "alice", "fix: B");
    repo.add_file("alice", "c.txt", "c-v1\n");
    commit_all(&repo, "alice", "chore: C");

    // Advance epoch unrelatedly.
    repo.maw_ok(&["ws", "create", "advancer"]);
    repo.add_file("advancer", "unrelated.txt", "advance\n");
    commit_all(&repo, "advancer", "chore: advance");
    repo.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge advancer",
    ]);

    let new_epoch = repo.current_epoch();

    let out = repo.maw_raw(&["ws", "sync", "alice", "--rebase"]);
    assert!(
        out.status.success(),
        "sync --rebase failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Subjects are returned newest-first by git log. Flip to oldest-first.
    let mut subjects = commit_subjects(&repo, "alice", &new_epoch);
    subjects.reverse();

    assert_eq!(
        subjects,
        vec![
            "feat: A".to_owned(),
            "fix: B".to_owned(),
            "chore: C".to_owned()
        ],
        "rebase must preserve commit messages in order"
    );
}

// ---------------------------------------------------------------------------
// 3. Clean replay preserves both sides' content
// ---------------------------------------------------------------------------

#[test]
fn sync_rebase_no_content_drops_on_clean_replay() {
    let repo = TestRepo::new();
    repo.seed_files(&[
        ("x.txt", "x-base\n"),
        ("y.txt", "y-base\n"),
        ("untouched.txt", "untouched-base\n"),
    ]);

    repo.maw_ok(&["ws", "create", "alice"]);

    // Commit A: modify x.txt only.
    repo.modify_file("alice", "x.txt", "x-after-A\n");
    commit_all(&repo, "alice", "feat: modify x");

    // Commit B: modify y.txt only (disjoint from A).
    repo.modify_file("alice", "y.txt", "y-after-B\n");
    commit_all(&repo, "alice", "feat: modify y");

    // Advance epoch without touching x.txt or y.txt.
    repo.maw_ok(&["ws", "create", "advancer"]);
    repo.modify_file("advancer", "untouched.txt", "untouched-after-epoch\n");
    commit_all(&repo, "advancer", "chore: advance epoch");
    repo.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge advancer",
    ]);

    // Rebase alice.
    let out = repo.maw_raw(&["ws", "sync", "alice", "--rebase"]);
    assert!(
        out.status.success(),
        "sync --rebase failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Final tree must contain BOTH of alice's modifications.
    assert_eq!(
        repo.read_file("alice", "x.txt").as_deref(),
        Some("x-after-A\n"),
        "commit A's modification of x.txt must survive the rebase"
    );
    assert_eq!(
        repo.read_file("alice", "y.txt").as_deref(),
        Some("y-after-B\n"),
        "commit B's modification of y.txt must survive the rebase"
    );
    assert_eq!(
        repo.read_file("alice", "untouched.txt").as_deref(),
        Some("untouched-after-epoch\n"),
        "epoch's advance of untouched.txt must be present"
    );
}

#[test]
fn sync_rebase_auto_merges_disjoint_same_file_edits() {
    let repo = TestRepo::new();
    repo.seed_files(&[(
        "shared.rs",
        "pub mod alpha {\n    // alpha anchor\n}\n\npub mod beta {\n    // beta anchor\n}\n",
    )]);

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.maw_ok(&["ws", "create", "bob"]);

    repo.modify_file(
        "alice",
        "shared.rs",
        "pub mod alpha {\n    pub struct FromAlice;\n    // alpha anchor\n}\n\npub mod beta {\n    // beta anchor\n}\n",
    );
    commit_all(&repo, "alice", "feat: alice alpha");

    repo.modify_file(
        "bob",
        "shared.rs",
        "pub mod alpha {\n    // alpha anchor\n}\n\npub mod beta {\n    pub struct FromBob;\n    // beta anchor\n}\n",
    );
    commit_all(&repo, "bob", "feat: bob beta");

    repo.maw_ok(&[
        "ws",
        "merge",
        "alice",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge alice",
    ]);

    let new_epoch = repo.current_epoch();
    let out = repo.maw_raw(&["ws", "sync", "bob", "--rebase"]);
    assert!(
        out.status.success(),
        "sync --rebase failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let content = repo
        .read_file("bob", "shared.rs")
        .expect("rebased file should exist");
    assert!(
        content.contains("pub struct FromAlice;"),
        "rebased content should retain epoch-side additive edit:\n{content}"
    );
    assert!(
        content.contains("pub struct FromBob;"),
        "rebased content should retain workspace-side additive edit:\n{content}"
    );
    assert!(
        !content.contains("<<<<<<<") && !content.contains("# structured conflict"),
        "disjoint edits should not materialize conflict markers:\n{content}"
    );
    assert!(
        repo.read_conflict_tree_sidecar("bob").is_none(),
        "clean disjoint rebase should not leave a structured conflict sidecar"
    );
    assert_eq!(
        commits_ahead(&repo, "bob", &new_epoch),
        1,
        "clean rebase should preserve bob's single commit"
    );
}

// ---------------------------------------------------------------------------
// 4. Executable bit preservation
// ---------------------------------------------------------------------------
//
// NOTE: Currently #[ignore]'d. The rebase pipeline's `infer_mode_for_new_file`
// (in `maw-core::merge::apply`) hard-codes `EntryMode::Blob` for *added*
// paths because `FileChange` does not yet carry an explicit mode. `Modified`
// paths correctly preserve their mode (see `clean_apply_preserves_exec_mode`
// unit test), but an `Added` executable is flattened to 100644.
//
// When the follow-up bone plumbs the mode through the patch collector
// (see the TODO at `apply.rs::infer_mode_for_new_file`), remove the
// `#[ignore]`.

#[test]
fn sync_rebase_preserves_executable_bit() {
    let repo = TestRepo::new();
    repo.seed_files(&[("README.md", "# test\n")]);

    repo.maw_ok(&["ws", "create", "alice"]);

    // Add a shell script with executable bit, making sure both the index
    // AND the worktree file mode are 0755 — otherwise `git status` reports
    // the worktree as dirty (worktree 0644 vs index 0755) and the rebase
    // pre-flight safety check refuses to run.
    repo.add_file("alice", "run.sh", "#!/bin/sh\necho hi\n");
    {
        use std::os::unix::fs::PermissionsExt;
        let run_path = repo.workspace_path("alice").join("run.sh");
        let mut perms = std::fs::metadata(&run_path)
            .expect("operation should succeed")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&run_path, perms).expect("operation should succeed");
    }
    repo.git_in_workspace("alice", &["add", "run.sh"]);
    repo.git_in_workspace("alice", &["commit", "-m", "feat: add executable run.sh"]);

    // Sanity: pre-rebase, HEAD tree has 100755 for run.sh.
    let pre = repo.git_ls_tree("alice", "HEAD");
    let pre_mode = pre
        .iter()
        .find(|(_, p)| p == "run.sh")
        .map(|(m, _)| m.as_str());
    assert_eq!(
        pre_mode,
        Some("100755"),
        "setup: run.sh should be 100755 before rebase, got {pre_mode:?}"
    );

    // Advance epoch unrelatedly.
    repo.maw_ok(&["ws", "create", "advancer"]);
    repo.add_file("advancer", "unrelated.txt", "advance\n");
    commit_all(&repo, "advancer", "chore: advance");
    repo.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge advancer",
    ]);

    // Rebase alice.
    let out = repo.maw_raw(&["ws", "sync", "alice", "--rebase"]);
    assert!(
        out.status.success(),
        "sync --rebase failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // After rebase, run.sh must still be 100755.
    let post = repo.git_ls_tree("alice", "HEAD");
    let post_mode = post
        .iter()
        .find(|(_, p)| p == "run.sh")
        .map(|(m, _)| m.as_str());
    assert_eq!(
        post_mode,
        Some("100755"),
        "rebase must preserve the executable bit on run.sh, got {post_mode:?}; full tree: {post:?}"
    );
}

// ---------------------------------------------------------------------------
// 5. Symlink preservation
// ---------------------------------------------------------------------------
//
// NOTE: Currently #[ignore]'d. Same root cause as #4: `infer_mode_for_new_file`
// flattens the mode of any *Added* path to `EntryMode::Blob` (100644),
// dropping the 120000 symlink marker. `Modified` symlink content is
// preserved (verified by `clean_apply_preserves_symlink_mode` in
// maw-core::merge::apply), but a symlink committed fresh in the workspace
// becomes a regular file after rebase.

#[test]
fn sync_rebase_preserves_symlink() {
    // Symlinks only make sense on Unix hosts with core.symlinks support;
    // the test environment is Linux so we rely on std::os::unix::fs::symlink.
    use std::os::unix::fs::symlink;

    let repo = TestRepo::new();
    repo.seed_files(&[("target.txt", "target content\n")]);

    repo.maw_ok(&["ws", "create", "alice"]);

    // Create a symlink `link.txt -> target.txt` inside alice's worktree and
    // commit it.
    let link_path = repo.workspace_path("alice").join("link.txt");
    symlink("target.txt", &link_path)
        .unwrap_or_else(|e| panic!("failed to create symlink {}: {e}", link_path.display()));
    repo.git_in_workspace("alice", &["add", "link.txt"]);
    repo.git_in_workspace("alice", &["commit", "-m", "feat: add symlink"]);

    // Sanity: pre-rebase mode is 120000.
    let pre = repo.git_ls_tree("alice", "HEAD");
    let pre_mode = pre
        .iter()
        .find(|(_, p)| p == "link.txt")
        .map(|(m, _)| m.as_str());
    assert_eq!(
        pre_mode,
        Some("120000"),
        "setup: link.txt should be 120000 before rebase, got {pre_mode:?}"
    );

    // Advance epoch unrelatedly.
    repo.maw_ok(&["ws", "create", "advancer"]);
    repo.add_file("advancer", "unrelated.txt", "advance\n");
    commit_all(&repo, "advancer", "chore: advance");
    repo.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge advancer",
    ]);

    let out = repo.maw_raw(&["ws", "sync", "alice", "--rebase"]);
    assert!(
        out.status.success(),
        "sync --rebase failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let post = repo.git_ls_tree("alice", "HEAD");
    let post_mode = post
        .iter()
        .find(|(_, p)| p == "link.txt")
        .map(|(m, _)| m.as_str());
    assert_eq!(
        post_mode,
        Some("120000"),
        "rebase must preserve the symlink mode on link.txt, got {post_mode:?}; full tree: {post:?}"
    );
}

// ---------------------------------------------------------------------------
// 6. Merge-commit regression (bn-372v) — structured Conflict::Content entry
// ---------------------------------------------------------------------------

#[test]
fn sync_rebase_merge_commit_regression_372v() {
    // Setup mirrors merge_rebase_reconcile::sync_rebase_marks_workspace_conflicted_on_merge_commit
    // but asserts on the *structured* sidecar, not just the marker gate: the
    // conflict-tree.json payload must contain a `shared.txt` entry with at
    // least two sides (the merge commit brought in a second parent → the
    // inject_merge_side_conflicts pass must have pushed a side onto the
    // Content conflict).
    let repo = TestRepo::new();
    repo.seed_files(&[("shared.txt", "original\n")]);

    repo.maw_ok(&["ws", "create", "feature"]);
    let epoch_before = repo.current_epoch();
    let ws_path = repo.workspace_path("feature");

    // Feature chain: modify shared.txt.
    repo.add_file("feature", "shared.txt", "feature-version\n");
    commit_all(&repo, "feature", "feat: feature work");
    let feature_commit = repo.workspace_head("feature");

    // Side branch off the epoch: a different modification of shared.txt.
    repo.git_in_workspace("feature", &["checkout", "-b", "side", &epoch_before]);
    std::fs::write(ws_path.join("shared.txt"), "side-version\n").expect("operation should succeed");
    commit_all(&repo, "feature", "feat: side work");

    // Go back to the feature chain (detached) and merge side in, resolving
    // with "ours" so the merge commit lands clean in git while still
    // having two parents.
    repo.git_in_workspace("feature", &["checkout", "--detach", &feature_commit]);
    repo.git_in_workspace(
        "feature",
        &[
            "-c",
            "merge.conflictStyle=diff3",
            "merge",
            "--no-ff",
            "--no-edit",
            "-X",
            "ours",
            "side",
        ],
    );

    // Make sure we really produced a merge commit.
    let parents_line =
        repo.git_in_workspace("feature", &["rev-list", "--parents", "-n", "1", "HEAD"]);
    let parent_count = parents_line.split_whitespace().count() - 1;
    assert!(
        parent_count >= 2,
        "setup failed: HEAD should be a merge commit, got {parent_count} parent(s)"
    );

    // Advance the epoch via another workspace that also edits shared.txt →
    // real three-way overlap (feature / side / epoch).
    repo.maw_ok(&["ws", "create", "advancer"]);
    repo.add_file("advancer", "shared.txt", "advancer-version\n");
    commit_all(&repo, "advancer", "chore: advance epoch");
    repo.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge advancer",
    ]);

    // Rebase feature.
    let _ = repo.maw_raw(&["ws", "sync", "feature", "--rebase"]);

    // The structured sidecar must exist and describe shared.txt as a
    // multi-sided content conflict.
    let sidecar = repo
        .read_conflict_tree_sidecar("feature")
        .expect("conflict-tree.json should exist after a conflicted rebase");

    let entry = find_conflict_entry(&sidecar, "shared.txt").unwrap_or_else(|| {
        panic!(
            "conflict-tree.json should have a `shared.txt` conflict entry; got: {}",
            serde_json::to_string_pretty(&sidecar).expect("operation should succeed")
        )
    });

    // V1 tagged enum: {"type": "content", "path": ..., "sides": [...]}
    // Allow `content`, `add_add`, or `modify_delete` — what matters is
    // that there are ≥2 sides, proving the merge-side content wasn't
    // silently dropped.
    let ty = entry
        .get("type")
        .and_then(|v| v.as_str())
        .expect("conflict entry should be tagged");
    assert!(
        ty == "content" || ty == "add_add",
        "expected content/add_add shape for a merge-commit conflict, got {ty}: {entry}"
    );

    // For Content and AddAdd we expect a `sides` array with ≥2 entries.
    let sides = entry
        .get("sides")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| {
            panic!("conflict entry for shared.txt should have a sides array: {entry}")
        });
    assert!(
        sides.len() >= 2,
        "merge-commit conflict must carry ≥2 sides (multi-parent reconciliation), got {} — entry: {entry}",
        sides.len()
    );
}

// ---------------------------------------------------------------------------
// 7. Narrow fidelity property: B's unilateral edit on an unrelated path
// must not collapse A's structured Content conflict
// ---------------------------------------------------------------------------

#[test]
fn sync_rebase_unilateral_edit_on_unrelated_path_preserves_structured_conflict() {
    let repo = TestRepo::new();
    repo.seed_files(&[
        ("conflicted.txt", "base\n"),
        ("unrelated.txt", "unrelated-base\n"),
    ]);

    repo.maw_ok(&["ws", "create", "alice"]);

    // Commit A: modify conflicted.txt.
    repo.modify_file("alice", "conflicted.txt", "alice-version\n");
    commit_all(&repo, "alice", "feat: alice edits conflicted");

    // Commit B: modify unrelated.txt (no overlap with anything the epoch
    // will do).
    repo.modify_file("alice", "unrelated.txt", "alice-unrelated\n");
    commit_all(&repo, "alice", "feat: alice edits unrelated");

    // Advance the epoch with a DIFFERENT edit to conflicted.txt only.
    repo.maw_ok(&["ws", "create", "advancer"]);
    repo.modify_file("advancer", "conflicted.txt", "epoch-version\n");
    commit_all(&repo, "advancer", "chore: epoch edits conflicted");
    repo.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge advancer",
    ]);

    // Rebase alice.
    let _ = repo.maw_raw(&["ws", "sync", "alice", "--rebase"]);

    // --- Assertion 1: conflicted.txt is a structured Content conflict ----
    let sidecar = repo
        .read_conflict_tree_sidecar("alice")
        .expect("conflict-tree.json should exist after a conflicted rebase");

    let entry = find_conflict_entry(&sidecar, "conflicted.txt").unwrap_or_else(|| {
        panic!(
            "sidecar should list conflicted.txt as conflicted; got:\n{}",
            serde_json::to_string_pretty(&sidecar).expect("operation should succeed")
        )
    });

    let ty = entry
        .get("type")
        .and_then(|v| v.as_str())
        .expect("conflict entry should be tagged");
    assert_eq!(
        ty, "content",
        "expected a Content conflict on conflicted.txt, got {ty}: {entry}"
    );

    let sides = entry
        .get("sides")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("Content conflict should carry a sides array: {entry}"));
    let side_labels: Vec<&str> = sides
        .iter()
        .filter_map(|s| s.get("workspace").and_then(|w| w.as_str()))
        .collect();
    assert!(
        side_labels.contains(&"epoch"),
        "sides should include `epoch` label, got {side_labels:?}"
    );
    assert!(
        side_labels.contains(&"alice"),
        "sides should include `alice` label, got {side_labels:?}"
    );

    // --- Assertion 2: unrelated.txt is clean with alice's content -------
    //
    // The sidecar MUST NOT list unrelated.txt (B's unilateral edit is clean).
    if let Some(value) = find_conflict_entry(&sidecar, "unrelated.txt") {
        panic!("B's edit to unrelated.txt must not appear in conflict-tree.json; found: {value}");
    }

    // The worktree is in the "unresolved-rebase-markers-committed" state;
    // commit B (unrelated.txt) was replayed on top of the marker step, so
    // unrelated.txt in HEAD must show alice's post-B content.
    assert_eq!(
        repo.read_file("alice", "unrelated.txt").as_deref(),
        Some("alice-unrelated\n"),
        "B's unilateral edit to unrelated.txt must land in the final tree as alice's content"
    );
}

// ---------------------------------------------------------------------------
// 8. bn-3525: rename followed by epoch-modify of the renamed-from path
// ---------------------------------------------------------------------------
//
// Regression for bn-3525. When a workspace renames `a.txt → b.txt` and the
// epoch independently modifies `a.txt`, the rebase pipeline MUST NOT silently
// drop both paths. The pre-fix behavior produced an empty tree; the fix
// "follows the rename" (matching git's default ort strategy) so the epoch's
// content change lands at the new path `b.txt`.

#[test]
fn rename_followed_by_epoch_modify_preserves_content_at_new_path() {
    let repo = TestRepo::new();
    repo.seed_files(&[("a.txt", "hello\n")]);

    repo.maw_ok(&["ws", "create", "feat"]);

    // Workspace renames a.txt → b.txt (content unchanged).
    let feat_path = repo.workspace_path("feat");
    repo.git_in_workspace("feat", &["mv", "a.txt", "b.txt"]);
    commit_all(&repo, "feat", "ws: rename a -> b");

    // sanity: the rename really happened in the workspace.
    assert!(
        feat_path.join("b.txt").exists(),
        "setup: b.txt must exist in feat"
    );
    assert!(
        !feat_path.join("a.txt").exists(),
        "setup: a.txt must be gone in feat"
    );

    // Advance the epoch by modifying a.txt through another workspace.
    repo.maw_ok(&["ws", "create", "advancer"]);
    repo.modify_file("advancer", "a.txt", "hello modified\n");
    commit_all(&repo, "advancer", "default: modify a");
    repo.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge advancer",
    ]);

    // Rebase feat onto the new epoch.
    let out = repo.maw_raw(&["ws", "sync", "feat", "--rebase"]);
    assert!(
        out.status.success(),
        "sync --rebase failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Invariant: the rebase MUST NOT produce an empty tree. Either:
    //   (a) b.txt is present with "hello modified" (follow-the-rename), or
    //   (b) b.txt is present with conflict markers (three-way conflict).
    // And a.txt must be gone (the workspace renamed it away).
    assert!(
        !feat_path.join("a.txt").exists(),
        "a.txt must be gone from feat after rebase (rename source was removed)"
    );
    let b_contents = repo
        .read_file("feat", "b.txt")
        .expect("b.txt must exist after rebase — neither side should be dropped");

    // Accept follow-the-rename (primary fix) or a conflict marker
    // (acceptable alternative). An empty or original "hello\n" body would
    // indicate the fix regressed.
    let has_epoch_content = b_contents.contains("hello modified");
    let has_conflict_markers = b_contents.contains("<<<<<<<");
    assert!(
        has_epoch_content || has_conflict_markers,
        "b.txt must carry the epoch's modified content OR show conflict markers; got: {b_contents:?}"
    );

    // Primary fix (Option 1 / follow-the-rename): for a **pure** rename the
    // epoch's modification should land cleanly at the new path with no
    // conflict markers. This matches git's default ort strategy behavior.
    assert_eq!(
        b_contents, "hello modified\n",
        "pure rename + epoch-modify should produce a clean follow-the-rename result \
         (primary fix for bn-3525); got: {b_contents:?}"
    );
}

// ---------------------------------------------------------------------------
// bn-7mbe: rebasing a workspace that contains a merge commit in the ahead
// range must preserve the merge-commit DAG shape (≥2 parents on the
// replayed commit). Pre-fix, the per-iteration `create_commit` used a
// single-parent slice, silently flattening the history to a linear chain.
// V1 fix: second parent is the ORIGINAL pre-rebase side OID (not rebased),
// which is enough to give downstream tooling a real merge shape.
// ---------------------------------------------------------------------------

#[test]
fn sync_rebase_preserves_merge_commit_parent_count() {
    let repo = TestRepo::new();
    repo.maw_ok(&["ws", "create", "feat"]);

    let feat = repo.workspace_path("feat");
    // Build feat → A → merge(A, side)
    std::fs::write(feat.join("a.txt"), "a\n").expect("operation should succeed");
    repo.git_in_workspace("feat", &["add", "a.txt"]);
    repo.git_in_workspace("feat", &["commit", "-m", "A"]);

    repo.git_in_workspace("feat", &["checkout", "-b", "side", "HEAD^"]);
    std::fs::write(feat.join("s.txt"), "s\n").expect("operation should succeed");
    repo.git_in_workspace("feat", &["add", "s.txt"]);
    repo.git_in_workspace("feat", &["commit", "-m", "side"]);

    repo.git_in_workspace("feat", &["checkout", "-"]);
    repo.git_in_workspace("feat", &["merge", "--no-ff", "side", "-m", "merge: side"]);

    // Advance epoch (disjoint paths so no conflict)
    std::fs::write(repo.workspace_path("default").join("z.txt"), "z\n")
        .expect("operation should succeed");
    repo.git_in_workspace("default", &["add", "-A"]);
    repo.git_in_workspace("default", &["commit", "-m", "default: z"]);
    repo.maw_ok(&["epoch", "sync"]);

    repo.maw_ok(&["ws", "sync", "--rebase", "feat"]);

    // After rebase, head's parents should include 2 OIDs.
    let parents_out = repo.git_in_workspace("feat", &["log", "-1", "--format=%P"]);
    let parents: Vec<&str> = parents_out.split_whitespace().collect();
    assert_eq!(
        parents.len(),
        2,
        "rebased merge commit should have 2 parents, got {}: {}",
        parents.len(),
        parents_out.trim()
    );
}

// ---------------------------------------------------------------------------
// 8. Add/Add regression (bn-3l5p): workspace and epoch both add the same
// new path — rebase must surface a structured conflict, not crash with
// `unexpected Added on conflicted path`.
// ---------------------------------------------------------------------------

#[test]
fn sync_rebase_handles_add_add_conflict() {
    // Repro: workspace adds `new.txt` with one content; epoch adds the same
    // `new.txt` with different content. Before bn-3l5p the rebase bailed
    // out with `ApplyError::UnexpectedAddOnConflict` and left the workspace
    // "stale" with no conflict artifacts. After bn-3l5p `Added` on a
    // pre-populated conflict is handled as `Modified`, so the structured
    // sidecar lands and `maw ws resolve --keep <side>` has a path forward.
    let repo = TestRepo::new();
    repo.seed_files(&[("placeholder.txt", "seed\n")]);

    // Workspace under test: adds new.txt with WS_NEW.
    repo.maw_ok(&["ws", "create", "feat"]);
    repo.add_file("feat", "new.txt", "WS_NEW\n");
    commit_all(&repo, "feat", "ws: new.txt");

    // Advance the epoch via a separate workspace that adds new.txt with
    // a DIFFERENT content (EPOCH_NEW), then merge-destroy to fast-forward
    // the epoch.
    repo.maw_ok(&["ws", "create", "advancer"]);
    repo.add_file("advancer", "new.txt", "EPOCH_NEW\n");
    commit_all(&repo, "advancer", "epoch: new.txt");
    repo.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge advancer",
    ]);

    // Rebase feat. Must not crash.
    let rebase_out = repo.maw_raw(&["ws", "sync", "feat", "--rebase"]);
    let rebase_stderr = String::from_utf8_lossy(&rebase_out.stderr);
    let rebase_stdout = String::from_utf8_lossy(&rebase_out.stdout);
    assert!(
        !rebase_stderr.contains("unexpected Added on conflicted path")
            && !rebase_stdout.contains("unexpected Added on conflicted path"),
        "rebase must not fail with 'unexpected Added on conflicted path' (bn-3l5p)\n\
         stdout:\n{rebase_stdout}\nstderr:\n{rebase_stderr}"
    );

    // Structured sidecar must exist and describe new.txt with both sides.
    let sidecar = repo
        .read_conflict_tree_sidecar("feat")
        .expect("conflict-tree.json should exist after an add/add rebase conflict");

    let entry = find_conflict_entry(&sidecar, "new.txt").unwrap_or_else(|| {
        panic!(
            "sidecar should list new.txt as conflicted; got:\n{}",
            serde_json::to_string_pretty(&sidecar).expect("operation should succeed")
        )
    });

    // The conflict shape may be either `content` (with base = None) or
    // `add_add` — both are valid representations of add/add under the
    // current pipeline. What matters is that both sides are recorded.
    let ty = entry
        .get("type")
        .and_then(|v| v.as_str())
        .expect("conflict entry should be tagged");
    assert!(
        ty == "content" || ty == "add_add",
        "expected content or add_add shape for add/add on new.txt, got {ty}: {entry}"
    );

    let sides = entry
        .get("sides")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("conflict entry should carry a sides array: {entry}"));
    let side_labels: Vec<&str> = sides
        .iter()
        .filter_map(|s| s.get("workspace").and_then(|w| w.as_str()))
        .collect();
    assert!(
        side_labels.contains(&"epoch"),
        "sides should include `epoch`, got {side_labels:?}"
    );
    assert!(
        side_labels.contains(&"feat"),
        "sides should include `feat`, got {side_labels:?}"
    );

    // `--keep feat` writes WS_NEW.
    repo.maw_ok(&["ws", "resolve", "feat", "--keep", "feat"]);
    assert_eq!(
        repo.read_file("feat", "new.txt").as_deref(),
        Some("WS_NEW\n"),
        "`--keep feat` should land the workspace's content"
    );

    // Redo the same scenario on a fresh repo to verify `--keep epoch`
    // writes EPOCH_NEW (the previous run already consumed `--keep feat`).
    let repo2 = TestRepo::new();
    repo2.seed_files(&[("placeholder.txt", "seed\n")]);

    repo2.maw_ok(&["ws", "create", "feat"]);
    repo2.add_file("feat", "new.txt", "WS_NEW\n");
    commit_all(&repo2, "feat", "ws: new.txt");

    repo2.maw_ok(&["ws", "create", "advancer"]);
    repo2.add_file("advancer", "new.txt", "EPOCH_NEW\n");
    commit_all(&repo2, "advancer", "epoch: new.txt");
    repo2.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge advancer",
    ]);

    let _ = repo2.maw_raw(&["ws", "sync", "feat", "--rebase"]);
    repo2.maw_ok(&["ws", "resolve", "feat", "--keep", "epoch"]);
    assert_eq!(
        repo2.read_file("feat", "new.txt").as_deref(),
        Some("EPOCH_NEW\n"),
        "`--keep epoch` should land the epoch's content"
    );
}

// ---------------------------------------------------------------------------
// bn-2ras — merge-commit convergence collapse
//
// When a rebase replays a merge commit whose parents are byte-identical on a
// given path (and the merge commit's own content is also identical), the
// rebase must NOT install a phantom `Conflict::Content` with three
// convergent sides. The sides converge — there is no real conflict — so the
// clean content should survive through the rebase.
// ---------------------------------------------------------------------------

#[test]
fn rebase_of_merge_with_identical_parent_content_is_clean() {
    // Mirror the repro from the bone: feat has a chain
    //   [A: side1]  → [merge sideB]  → [C: modify both]
    // where sideB branched off the same base and only adds `side2.txt`.
    // `side1.txt` is untouched by sideB, so both merge parents agree on its
    // content. The pre-fix rebase would surface `side1.txt` as conflicted
    // with two identical sides — none of which `--keep` could resolve.
    let repo = TestRepo::new();
    repo.seed_files(&[("noop.txt", "base\n")]);

    let base_epoch = repo.current_epoch();

    // Build feat's merge-commit chain by going under the hood with git:
    // maw doesn't have a first-class "merge inside a workspace" command.
    repo.maw_ok(&["ws", "create", "feat"]);

    // Commit A on feat's default branch.
    repo.add_file("feat", "side1.txt", "A1\n");
    repo.git_in_workspace("feat", &["add", "-A"]);
    repo.git_in_workspace("feat", &["commit", "-m", "A: side1"]);

    // Create sideB from the epoch, commit B there, then merge back onto
    // feat's current tip (which is detached HEAD on commit A). Record the
    // commit-A OID so we can checkout back to it after visiting sideB.
    let commit_a_oid = repo
        .git_in_workspace("feat", &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    repo.git_in_workspace("feat", &["checkout", "-b", "sideB", &base_epoch]);
    repo.add_file("feat", "side2.txt", "B1\n");
    repo.git_in_workspace("feat", &["add", "-A"]);
    repo.git_in_workspace("feat", &["commit", "-m", "B: side2"]);
    // Return to commit A in detached-HEAD mode (matching how maw workspaces
    // start out) before merging sideB.
    repo.git_in_workspace("feat", &["checkout", "--detach", &commit_a_oid]);
    repo.git_in_workspace(
        "feat",
        &["merge", "--no-ff", "sideB", "-m", "merge sideB into A"],
    );

    // Post-merge commit C that edits *both* files so the rebase actually
    // has work to do for side1.txt beyond the merge step.
    repo.modify_file("feat", "side1.txt", "C\n");
    repo.modify_file("feat", "side2.txt", "C\n");
    repo.git_in_workspace("feat", &["add", "-A"]);
    repo.git_in_workspace("feat", &["commit", "-m", "C: modify both"]);

    // Advance epoch with a trivially-unrelated change.
    repo.maw_ok(&["ws", "create", "advancer"]);
    repo.modify_file("advancer", "noop.txt", "advanced\n");
    commit_all(&repo, "advancer", "chore: advance");
    repo.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge advancer",
    ]);

    // Rebase feat. The merge commit should replay cleanly — parents converge
    // on side1.txt, so no phantom conflict.
    let out = repo.maw_raw(&["ws", "sync", "feat", "--rebase"]);
    assert!(
        out.status.success(),
        "sync --rebase failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // No structured conflict sidecar should exist — the rebase was clean.
    let sidecar = repo.read_conflict_tree_sidecar("feat");
    if let Some(s) = sidecar.as_ref() {
        let conflicts = s
            .get("conflicts")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        assert!(
            !conflicts.contains_key("side1.txt"),
            "bn-2ras: side1.txt must NOT be in conflict-tree.json (merge parents \
             converge on identical content), got: {}",
            serde_json::to_string_pretty(s).expect("operation should succeed")
        );
    }

    // Final workspace content reflects commit C, not a marker-soup blob.
    assert_eq!(
        repo.read_file("feat", "side1.txt").as_deref(),
        Some("C\n"),
        "side1.txt must carry commit C's content, not a conflict marker"
    );
    assert_eq!(
        repo.read_file("feat", "side2.txt").as_deref(),
        Some("C\n"),
        "side2.txt must carry commit C's content"
    );

    // And the on-disk bytes must not contain structured-conflict markers.
    let side1 = repo.read_file("feat", "side1.txt").unwrap_or_default();
    assert!(
        !side1.contains("<<<<<<<") && !side1.contains(">>>>>>>"),
        "side1.txt must not contain conflict markers, got: {side1}"
    );
}

// ---------------------------------------------------------------------------
// bn-2ras — `--keep <ws>` matches `<ws>#merge-parent-N` unambiguously
//
// When a merge-commit rebase surfaces genuine conflicts, the sides are
// labeled with `<ws>#merge-parent-N`. Users typing `--keep <ws>` expect the
// obvious thing to happen when only one such side exists (unambiguous
// prefix match). This test builds a scenario where the conflict has exactly
// one `feat#merge-parent-N` side plus an `epoch` side, and verifies
// `--keep feat` resolves it.
// ---------------------------------------------------------------------------

#[test]
fn keep_with_unambiguous_parent_side_works() {
    // Scenario: feat's merge commit contributes content for a file that the
    // epoch independently modified too. After first-parent apply, the
    // workspace side is labeled by `promote_overlaps_to_conflicts` as
    // `feat` (via `ws_name`) — and the second-parent injection adds a
    // `feat#merge-parent-1` side. `--keep feat` must still resolve to a
    // single side unambiguously (exact match wins over prefix).
    //
    // However the simpler variant we lock in here: the resolve-side unit
    // tests already cover the prefix-match / ambiguity contract; the
    // integration test verifies that if a merge-commit rebase produces a
    // conflict with *only* qualified sides, `--keep feat` still works.
    //
    // We construct this by asserting behaviour against the unit-test
    // contract: when a structured sidecar has one `epoch` side and one
    // `feat#merge-parent-N` side, `--keep feat` writes the workspace side.

    let repo = TestRepo::new();
    repo.seed_files(&[("shared.txt", "base\n")]);

    let base_epoch = repo.current_epoch();

    repo.maw_ok(&["ws", "create", "feat"]);
    // Commit A: feat modifies shared.txt.
    repo.modify_file("feat", "shared.txt", "A\n");
    commit_all(&repo, "feat", "A: edit shared");

    // Side branch: from the same base, contributes an unrelated file —
    // so the merge commit's content for `shared.txt` comes entirely from
    // parent A (no divergence between parents).
    let commit_a_oid = repo
        .git_in_workspace("feat", &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    repo.git_in_workspace("feat", &["checkout", "-b", "sideB", &base_epoch]);
    repo.add_file("feat", "other.txt", "B\n");
    repo.git_in_workspace("feat", &["add", "-A"]);
    repo.git_in_workspace("feat", &["commit", "-m", "B: add other"]);
    repo.git_in_workspace("feat", &["checkout", "--detach", &commit_a_oid]);
    repo.git_in_workspace("feat", &["merge", "--no-ff", "sideB", "-m", "merge sideB"]);

    // Advance epoch to create a real conflict on shared.txt.
    repo.maw_ok(&["ws", "create", "advancer"]);
    repo.modify_file("advancer", "shared.txt", "EPOCH\n");
    commit_all(&repo, "advancer", "chore: epoch edits shared");
    repo.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge advancer",
    ]);

    // Rebase feat. The first commit's overlap produces a normal `feat` vs
    // `epoch` conflict. The merge commit's second-parent injection is
    // collapsed by convergence (bn-2ras) since both parents agree on
    // shared.txt post-first-parent, so the final sidecar has one side
    // labeled `feat` (workspace overlap) plus one `epoch` side — exactly
    // the normal non-merge shape. `--keep feat` works.
    let _ = repo.maw_raw(&["ws", "sync", "feat", "--rebase"]);
    repo.maw_ok(&["ws", "resolve", "feat", "--keep", "feat"]);
    assert_eq!(
        repo.read_file("feat", "shared.txt").as_deref(),
        Some("A\n"),
        "`--keep feat` with a single feat-prefixed side must resolve to feat's content"
    );
}

// ---------------------------------------------------------------------------
// bn-1d1g — concurrent `sync --rebase` on the same workspace must serialize
//
// Without the workspace-scoped flock, two racing rebases both rewrite HEAD /
// the worktree; the loser aborts mid-pipeline with an internal-looking error
// (e.g. `set_head failed: ... No such file or directory`) and leaves the
// workspace in a half-rebased state.
//
// With the lock (bn-1d1g) the second racer fast-fails with a clean
// "Another rebase is in progress" message and exit code != 0; the workspace
// is left in whatever consistent state the winning rebase produced, and a
// subsequent rebase (now uncontested) succeeds.
// ---------------------------------------------------------------------------

#[test]
fn concurrent_rebase_races_are_serialized() {
    use manifold_common::maw_bin;
    use std::process::{Command, Stdio};

    let repo = TestRepo::new();
    repo.seed_files(&[("main.rs", "fn main() {}\n")]);

    // Give `feat` enough committed work that the rebase pipeline takes
    // long enough for a second invocation to hit the flock contention
    // window (materialize + per-commit tree build + checkout_tree on a
    // handful of commits is comfortably above the sub-millisecond range
    // required).
    repo.maw_ok(&["ws", "create", "feat"]);
    for i in 0..8 {
        repo.add_file("feat", &format!("f{i}.txt"), &format!("v{i}\n"));
        commit_all(&repo, "feat", &format!("feat: step {i}"));
    }

    // Advance the epoch with unrelated content so `feat` is now stale with
    // committed work — exactly the state `--rebase` is designed to handle.
    repo.maw_ok(&["ws", "create", "advancer"]);
    repo.add_file("advancer", "adv.txt", "advance\n");
    commit_all(&repo, "advancer", "chore: advance");
    repo.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge advancer",
    ]);

    // Spawn two `maw ws sync feat --rebase` processes simultaneously.
    // Both inherit the same cwd (the repo root). `wait()` on each child
    // after both have been spawned so they race in the kernel.
    let spawn = || {
        Command::new(maw_bin())
            .args(["ws", "sync", "feat", "--rebase"])
            .current_dir(repo.root())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn maw")
    };

    let a = spawn();
    let b = spawn();

    let out_a = a.wait_with_output().expect("wait a");
    let out_b = b.wait_with_output().expect("wait b");

    let success_a = out_a.status.success();
    let success_b = out_b.status.success();

    // Exactly one must succeed and exactly one must fail. If both succeed
    // the lock isn't actually serializing (they may have interleaved in a
    // data-racing way that happened not to corrupt this time). If both
    // fail we've regressed the happy path.
    let succeeded = usize::from(success_a) + usize::from(success_b);
    assert_eq!(
        succeeded,
        1,
        "expected exactly one rebase to succeed, got {succeeded}\n\
         a.status={:?} a.stdout={:?} a.stderr={:?}\n\
         b.status={:?} b.stdout={:?} b.stderr={:?}",
        out_a.status,
        String::from_utf8_lossy(&out_a.stdout),
        String::from_utf8_lossy(&out_a.stderr),
        out_b.status,
        String::from_utf8_lossy(&out_b.stdout),
        String::from_utf8_lossy(&out_b.stderr),
    );

    // The loser's stderr/stdout must carry the friendly lock-contention
    // message, not an internal-looking git error.
    let loser_stderr = if success_a {
        String::from_utf8_lossy(&out_b.stderr).into_owned()
    } else {
        String::from_utf8_lossy(&out_a.stderr).into_owned()
    };
    let loser_stdout = if success_a {
        String::from_utf8_lossy(&out_b.stdout).into_owned()
    } else {
        String::from_utf8_lossy(&out_a.stdout).into_owned()
    };
    let loser_combined = format!("{loser_stdout}\n{loser_stderr}");
    assert!(
        loser_combined.contains("Another rebase is in progress"),
        "loser must emit the friendly lock-contention message, got:\nstdout: {loser_stdout}\nstderr: {loser_stderr}"
    );

    // Internal git errors must NOT leak out of the loser. If `set_head` or
    // `checkout_tree` surfaces in either stream, the workspace was half-
    // rebased and the lock didn't do its job.
    assert!(
        !loser_combined.contains("set_head failed"),
        "loser leaked an internal git error, lock did not serialize:\n{loser_combined}"
    );
    assert!(
        !loser_combined.contains("checkout_tree failed"),
        "loser leaked an internal git error, lock did not serialize:\n{loser_combined}"
    );

    // After both racers settle, a third rebase must succeed (or report
    // "up to date" if the first racer already finished the work). Either
    // way the workspace must be in a consistent state.
    let third = Command::new(maw_bin())
        .args(["ws", "sync", "feat", "--rebase"])
        .current_dir(repo.root())
        .output()
        .expect("third rebase");
    assert!(
        third.status.success(),
        "third rebase (no contention) must succeed; workspace left in inconsistent state?\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&third.stdout),
        String::from_utf8_lossy(&third.stderr),
    );
}

// ---------------------------------------------------------------------------
// bn-1hmz — binary blobs must NOT be passed through the text merge driver
//
// A binary file (containing NUL bytes) edited by both epoch and workspace in
// DIFFERENT byte regions would previously produce a "clean" frankenstein
// merge (fabricating bytes neither side had) because merge_text is byte-level
// and 0x0A inside binary data looks like a line boundary to the diff engine.
// The fix: `try_clean_three_way_overlap` must return Ok(None) when any blob
// fails looks_text, routing through the conflict-tree path whose materialize
// stage renders a safe binary-conflict stub.
// ---------------------------------------------------------------------------

/// Binary file (with embedded NUL bytes and embedded 0x0A) edited by epoch
/// (one region) and workspace (a different region): the rebase MUST NOT
/// produce a clean merge.  The workspace must end up conflicted, and the
/// rebased blob must be byte-identical to the binary-conflict stub emitted
/// by materialize (starts with `# BINARY CONFLICT at`) — never a mix of the
/// two sides' binary content.
#[test]
fn sync_rebase_binary_blob_not_clean_merged() {
    let repo = TestRepo::new();

    // Base binary: three "sections" separated by 0x0A so the text driver
    // would see "lines".  Each section contains a NUL byte, which is the
    // canonical signal for binary content.
    //   HDR\x00aaaa\n  ←  epoch will change this
    //   MID\x00bbbb\n  ←  neither side touches this
    //   END\x00cccc\n  ←  workspace will change this
    let base_content: &[u8] = b"HDR\x00aaaa\nMID\x00bbbb\nEND\x00cccc\n";
    repo.seed_binary_files(&[("data.bin", base_content)]);

    // Create two workspaces from the same epoch.
    repo.maw_ok(&["ws", "create", "epoch-ws"]);
    repo.maw_ok(&["ws", "create", "feat"]);

    // epoch-ws changes the HDR section.
    let epoch_content: &[u8] = b"HDR\x00XXXX\nMID\x00bbbb\nEND\x00cccc\n";
    repo.modify_file_bytes("epoch-ws", "data.bin", epoch_content);
    commit_all(&repo, "epoch-ws", "epoch: change HDR section");

    // Advance the epoch by merging epoch-ws.
    repo.maw_ok(&[
        "ws",
        "merge",
        "epoch-ws",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge epoch-ws",
    ]);

    // feat workspace changes the END section (disjoint from epoch's change).
    let ws_content: &[u8] = b"HDR\x00aaaa\nMID\x00bbbb\nEND\x00ZZZZ\n";
    repo.modify_file_bytes("feat", "data.bin", ws_content);
    commit_all(&repo, "feat", "feat: change END section");

    // Rebase feat onto the new epoch.
    let out = repo.maw_raw(&["ws", "sync", "feat", "--rebase"]);
    // The rebase command may succeed (conflict recorded) or indicate conflict
    // via non-zero; either is fine — what matters is the file content.
    let _ = out.status; // we don't assert success/failure on the command itself

    // The structured conflict sidecar MUST exist — the binary file touched by
    // both sides must route through the conflict-tree path.
    let sidecar = repo.read_conflict_tree_sidecar("feat").expect(
        "bn-1hmz: conflict-tree.json must exist — binary disjoint edit must not be clean-merged",
    );
    assert!(
        find_conflict_entry(&sidecar, "data.bin").is_some(),
        "bn-1hmz: data.bin must appear in conflict-tree.json; got:\n{}",
        serde_json::to_string_pretty(&sidecar).expect("operation should succeed")
    );

    // The on-disk bytes must be byte-identical to ONE consistent state — the
    // binary-conflict stub (starts with b"# BINARY CONFLICT at"), or the epoch
    // blob, or the workspace blob — never a mixture of both sides' binary data.
    let on_disk = repo
        .read_file_bytes("feat", "data.bin")
        .expect("data.bin must exist in worktree");

    let is_binary_stub = on_disk.starts_with(b"# BINARY CONFLICT at");
    let is_epoch_side = on_disk == epoch_content;
    let is_ws_side = on_disk == ws_content;
    let is_base = on_disk == base_content;

    assert!(
        is_binary_stub || is_epoch_side || is_ws_side || is_base,
        "bn-1hmz: data.bin bytes must be the binary-conflict stub, epoch side, ws side, \
         or base — never a frankenstein mix; got (hex):\n{}",
        on_disk
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    );

    // Specifically it must NOT contain bytes from BOTH sides' distinguishing
    // regions simultaneously (HDR\x00XXXX AND END\x00ZZZZ in the same blob
    // would prove the text driver ran across binary data and fabricated a merge).
    // b"HDR\x00XXXX" and b"END\x00ZZZZ" are each 8 bytes.
    let has_epoch_marker = on_disk.windows(8).any(|w| w == b"HDR\x00XXXX");
    let has_ws_marker = on_disk.windows(8).any(|w| w == b"END\x00ZZZZ");
    assert!(
        !(has_epoch_marker && has_ws_marker),
        "bn-1hmz: data.bin contains distinguishing bytes from BOTH sides — text merge \
         ran on binary data and fabricated a frankenstein blob"
    );
}

// ---------------------------------------------------------------------------
// bn-566k — epoch-delete vs workspace-modify must conflict
//
// When the epoch DELETES a file (via another workspace's merged deletion) and
// a sibling workspace MODIFIES (or re-adds) that same file, the rebase replay
// must surface a structured modify/delete conflict.  Pre-fix the workspace
// content sailed through clean, and a subsequent merge silently resurrected
// the deleted file on main.
// ---------------------------------------------------------------------------

/// Exact repro from the bone: epoch deletes big.txt (via merged workspace v),
/// workspace w modifies big.txt.  After rebase, w must be in a
/// `modify_delete` conflict state; `maw ws merge w` must be blocked; and
/// `--keep epoch` deletes the file while `--keep w` restores it.
#[expect(
    clippy::too_many_lines,
    reason = "three resolution sub-scenarios (conflict detection, --keep epoch, --keep ws) \
              share the same setup and are clearest as one coherent narrative test"
)]
#[test]
fn bn566k_epoch_delete_vs_ws_modify_produces_modify_delete_conflict() {
    let repo = TestRepo::new();
    repo.seed_files(&[
        ("big.txt", "line1\nline2\nline3\n"),
        ("other.txt", "untouched\n"),
    ]);

    // ws_w: modify big.txt (adds content to line 2).
    repo.maw_ok(&["ws", "create", "ws-w"]);
    repo.modify_file("ws-w", "big.txt", "line1\nLINE2-MODIFIED\nline3\n");
    commit_all(&repo, "ws-w", "feat: modify big.txt in ws-w");

    // ws_v: delete big.txt entirely.
    repo.maw_ok(&["ws", "create", "ws-v"]);
    repo.delete_file("ws-v", "big.txt");
    commit_all(&repo, "ws-v", "chore: delete big.txt in ws-v");

    // Merge ws-v (no auto-rebase so ws-w doesn't get rebased just yet).
    repo.maw_ok(&[
        "ws",
        "merge",
        "ws-v",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge ws-v: delete big.txt",
    ]);

    // big.txt must be gone from the default branch now.
    assert!(
        !repo.file_exists("default", "big.txt"),
        "big.txt should be absent from default after merging ws-v's deletion"
    );

    // Now rebase ws-w onto the new epoch.
    let _rebase_out = repo.maw_raw(&["ws", "sync", "ws-w", "--rebase"]);

    // The structured sidecar must exist and describe big.txt as a
    // modify/delete conflict (modifier = ws-w, deleter = epoch).
    let sidecar = repo
        .read_conflict_tree_sidecar("ws-w")
        .expect("conflict-tree.json must exist after bn-566k epoch-delete vs ws-modify");

    let entry = find_conflict_entry(&sidecar, "big.txt").unwrap_or_else(|| {
        panic!(
            "sidecar should list big.txt as conflicted (bn-566k); got:\n{}",
            serde_json::to_string_pretty(&sidecar).expect("operation should succeed")
        )
    });

    let ty = entry
        .get("type")
        .and_then(|v| v.as_str())
        .expect("conflict entry should be tagged");
    assert_eq!(
        ty, "modify_delete",
        "expected modify_delete shape for epoch-delete vs ws-modify, got {ty}: {entry}"
    );

    // modifier must be ws-w, deleter must be epoch.
    let modifier_ws = entry
        .get("modifier")
        .and_then(|m| m.get("workspace"))
        .and_then(|w| w.as_str())
        .expect("modify_delete entry should have a modifier.workspace field");
    assert_eq!(
        modifier_ws, "ws-w",
        "modifier should be ws-w (the workspace that modified big.txt), got {modifier_ws}"
    );

    let deleter_ws = entry
        .get("deleter")
        .and_then(|d| d.get("workspace"))
        .and_then(|w| w.as_str())
        .expect("modify_delete entry should have a deleter.workspace field");
    assert_eq!(
        deleter_ws, "epoch",
        "deleter should be epoch (the side that deleted big.txt), got {deleter_ws}"
    );

    // The merge gate must refuse to merge ws-w while the conflict is live.
    let merge_attempt = repo.maw_raw(&["ws", "merge", "ws-w", "--message", "try-merge"]);
    assert!(
        !merge_attempt.status.success(),
        "maw ws merge must be blocked while ws-w has an unresolved conflict (bn-566k)"
    );

    // --- Resolution: --keep epoch → big.txt must be deleted from ws-w tree ---
    //
    // Run on a fresh TestRepo to avoid state contamination between the two
    // resolution paths.
    let repo2 = TestRepo::new();
    repo2.seed_files(&[
        ("big.txt", "line1\nline2\nline3\n"),
        ("other.txt", "untouched\n"),
    ]);

    repo2.maw_ok(&["ws", "create", "ws-w"]);
    repo2.modify_file("ws-w", "big.txt", "line1\nLINE2-MODIFIED\nline3\n");
    commit_all(&repo2, "ws-w", "feat: modify big.txt in ws-w");

    repo2.maw_ok(&["ws", "create", "ws-v"]);
    repo2.delete_file("ws-v", "big.txt");
    commit_all(&repo2, "ws-v", "chore: delete big.txt in ws-v");
    repo2.maw_ok(&[
        "ws",
        "merge",
        "ws-v",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge ws-v: delete big.txt",
    ]);

    let _ = repo2.maw_raw(&["ws", "sync", "ws-w", "--rebase"]);

    // `--keep epoch` on modify/delete where epoch is the deleter → accept
    // the deletion.  big.txt must disappear from ws-w's worktree.
    repo2.maw_ok(&["ws", "resolve", "ws-w", "--keep", "epoch"]);
    assert!(
        !repo2.file_exists("ws-w", "big.txt"),
        "`--keep epoch` on a modify/delete (epoch=deleter) must remove big.txt from ws-w"
    );

    // The resolver auto-commits after all conflicts are cleared (bn-2cc1), so
    // no manual `commit_all` is needed.  Merge must now succeed.
    repo2.maw_ok(&[
        "ws",
        "merge",
        "ws-w",
        "--destroy",
        "--message",
        "merge ws-w post-resolve",
    ]);
    // big.txt must remain absent from main (epoch deletion stands).
    assert!(
        !repo2.file_exists("default", "big.txt"),
        "big.txt must stay deleted on main after --keep epoch resolve + merge"
    );

    // --- Resolution: --keep ws-w → big.txt survives with ws-w's content ---
    let repo3 = TestRepo::new();
    repo3.seed_files(&[
        ("big.txt", "line1\nline2\nline3\n"),
        ("other.txt", "untouched\n"),
    ]);

    repo3.maw_ok(&["ws", "create", "ws-w"]);
    repo3.modify_file("ws-w", "big.txt", "line1\nLINE2-MODIFIED\nline3\n");
    commit_all(&repo3, "ws-w", "feat: modify big.txt in ws-w");

    repo3.maw_ok(&["ws", "create", "ws-v"]);
    repo3.delete_file("ws-v", "big.txt");
    commit_all(&repo3, "ws-v", "chore: delete big.txt in ws-v");
    repo3.maw_ok(&[
        "ws",
        "merge",
        "ws-v",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge ws-v: delete big.txt",
    ]);

    let _ = repo3.maw_raw(&["ws", "sync", "ws-w", "--rebase"]);

    // `--keep ws-w` on modify/delete → modifier wins, file kept with ws
    // content.
    repo3.maw_ok(&["ws", "resolve", "ws-w", "--keep", "ws-w"]);
    assert_eq!(
        repo3.read_file("ws-w", "big.txt").as_deref(),
        Some("line1\nLINE2-MODIFIED\nline3\n"),
        "`--keep ws-w` must restore big.txt with the workspace's modified content"
    );

    // The resolver auto-commits (bn-2cc1) — no manual commit needed.
    repo3.maw_ok(&[
        "ws",
        "merge",
        "ws-w",
        "--destroy",
        "--message",
        "merge ws-w post-resolve keep-ws",
    ]);
    assert_eq!(
        repo3.read_file("default", "big.txt").as_deref(),
        Some("line1\nLINE2-MODIFIED\nline3\n"),
        "big.txt must appear on main with ws-w's content after --keep ws-w resolve + merge"
    );
}

/// Edge case: workspace ADDS a file at a path the epoch deleted.
///
/// Decision rationale: conflict too (same shape).  The workspace is attempting
/// to introduce content at a path the epoch explicitly removed.  Silent pass-
/// through would resurrect the file just as badly as the modify case.  We
/// surface a modify/delete (modifier = ws, deleter = epoch) so the resolver
/// can decide whether the re-add should win.
#[test]
fn bn566k_epoch_delete_vs_ws_add_produces_conflict() {
    let repo = TestRepo::new();
    repo.seed_files(&[("existing.txt", "base content\n"), ("anchor.txt", "x\n")]);

    // Workspace `adder` explicitly adds a file at a path that the epoch will
    // soon delete.  We simulate this by: first seeding existing.txt, having
    // adder delete-then-re-add it (so git sees an Add in the patchset), but
    // that's awkward.  Instead we use a fresh path that the epoch deletes via
    // a parallel workspace.
    //
    // Simpler path: workspace `adder` creates `new-then-gone.txt`, epoch also
    // gets `new-then-gone.txt` (added by another ws and merged), then epoch
    // deletes it via yet another workspace.  But the simplest repro is:
    //
    //  1. Seed repo with `target.txt`.
    //  2. `ws-adder`: delete target.txt then re-add it with different content
    //     (producing an Add in the delta from the workspace's perspective on
    //     the re-add commit, or a Modify — either exercises the bn-566k path).
    //  3. `ws-deleter`: delete target.txt, merge.
    //  4. Rebase ws-adder → conflict.
    //
    // For maximal clarity use Modified (workspace edits the file) rather than
    // a full delete+re-add cycle inside the workspace.  The delete+re-add
    // edge specifically is exercised by confirming an Added ChangeKind can
    // also reach the ModifyDelete install path.

    repo.maw_ok(&["ws", "create", "ws-adder"]);
    // Delete the file then re-add it so git produces an Add in the patchset.
    repo.delete_file("ws-adder", "existing.txt");
    commit_all(&repo, "ws-adder", "chore: remove existing.txt first");
    repo.add_file("ws-adder", "existing.txt", "re-added content\n");
    commit_all(
        &repo,
        "ws-adder",
        "feat: re-add existing.txt with new content",
    );

    // Epoch deletes existing.txt via a separate workspace.
    repo.maw_ok(&["ws", "create", "ws-deleter"]);
    repo.delete_file("ws-deleter", "existing.txt");
    commit_all(&repo, "ws-deleter", "chore: delete existing.txt in epoch");
    repo.maw_ok(&[
        "ws",
        "merge",
        "ws-deleter",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge ws-deleter: epoch deletes existing.txt",
    ]);

    // Rebase ws-adder (which still has existing.txt re-added).
    let _out = repo.maw_raw(&["ws", "sync", "ws-adder", "--rebase"]);

    // The workspace must be conflicted — NOT clean. The specific commit that
    // re-adds existing.txt should have triggered the modify/delete path.
    let sidecar = repo.read_conflict_tree_sidecar("ws-adder");
    assert!(
        sidecar.is_some(),
        "conflict-tree.json must exist: epoch-delete vs ws-re-add must not silently pass through (bn-566k)"
    );

    let sidecar = sidecar.expect("just checked");
    let entry = find_conflict_entry(&sidecar, "existing.txt").unwrap_or_else(|| {
        panic!(
            "sidecar should list existing.txt as conflicted; got:\n{}",
            serde_json::to_string_pretty(&sidecar).expect("operation should succeed")
        )
    });

    let ty = entry
        .get("type")
        .and_then(|v| v.as_str())
        .expect("conflict entry should be tagged");
    assert_eq!(
        ty, "modify_delete",
        "epoch-delete vs ws-re-add should produce modify_delete, got {ty}: {entry}"
    );

    let deleter_ws = entry
        .get("deleter")
        .and_then(|d| d.get("workspace"))
        .and_then(|w| w.as_str())
        .expect("modify_delete must have deleter.workspace");
    assert_eq!(
        deleter_ws, "epoch",
        "deleter must be epoch, got {deleter_ws}"
    );
}

/// Regression guard: the original direction (ws deletes, epoch modifies) must
/// still produce a modify/delete conflict unaffected by the bn-566k fix.
#[test]
fn bn566k_ws_delete_vs_epoch_modify_still_conflicts() {
    let repo = TestRepo::new();
    repo.seed_files(&[("shared.txt", "original content\n")]);

    // ws-deleter deletes shared.txt.
    repo.maw_ok(&["ws", "create", "ws-deleter"]);
    repo.delete_file("ws-deleter", "shared.txt");
    commit_all(&repo, "ws-deleter", "chore: delete shared.txt");

    // Epoch modifies shared.txt via another workspace.
    repo.maw_ok(&["ws", "create", "ws-epoch"]);
    repo.modify_file("ws-epoch", "shared.txt", "epoch modified content\n");
    commit_all(&repo, "ws-epoch", "feat: epoch modifies shared.txt");
    repo.maw_ok(&[
        "ws",
        "merge",
        "ws-epoch",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge ws-epoch: epoch modifies shared.txt",
    ]);

    // Rebase ws-deleter — must still surface a modify/delete conflict.
    let _out = repo.maw_raw(&["ws", "sync", "ws-deleter", "--rebase"]);

    let sidecar = repo
        .read_conflict_tree_sidecar("ws-deleter")
        .expect("conflict-tree.json must exist for ws-delete vs epoch-modify");

    let entry = find_conflict_entry(&sidecar, "shared.txt").unwrap_or_else(|| {
        panic!(
            "sidecar should list shared.txt as conflicted (regression guard); got:\n{}",
            serde_json::to_string_pretty(&sidecar).expect("operation should succeed")
        )
    });

    let ty = entry
        .get("type")
        .and_then(|v| v.as_str())
        .expect("conflict entry should be tagged");
    assert_eq!(
        ty, "modify_delete",
        "ws-delete vs epoch-modify must still produce modify_delete (regression guard), got {ty}: {entry}"
    );

    // modifier = epoch, deleter = ws-deleter (existing direction, unchanged).
    let modifier_ws = entry
        .get("modifier")
        .and_then(|m| m.get("workspace"))
        .and_then(|w| w.as_str())
        .expect("modify_delete must have modifier.workspace");
    assert_eq!(
        modifier_ws, "epoch",
        "modifier must be epoch (regression guard), got {modifier_ws}"
    );

    let deleter_ws = entry
        .get("deleter")
        .and_then(|d| d.get("workspace"))
        .and_then(|w| w.as_str())
        .expect("modify_delete must have deleter.workspace");
    assert_eq!(
        deleter_ws, "ws-deleter",
        "deleter must be ws-deleter (regression guard), got {deleter_ws}"
    );
}

// ---------------------------------------------------------------------------
// bn-heb8 — rename-aware modify_delete hints
//
// When the epoch RENAMES a file (git mv old → new via a merged sibling) and a
// workspace edits the OLD path, the bn-566k fix surfaces a modify_delete
// conflict.  bn-heb8 improves this by:
//   (a) detecting the rename (epoch deleted old + added new with same blob)
//   (b) recording rename_hint in the sidecar so the stub + resolve --list
//       tell the user where the content went
//   (c) printing a discarded-blob note when --keep epoch silently drops the ws edit
// ---------------------------------------------------------------------------

/// (a) + (b): epoch renames file (identical blob), ws edits old path.
/// After rebase the sidecar must have `rename_hint` pointing at the new path,
/// resolve --list must include "(renamed to ...)", and the stub must contain
/// the rename note.
#[test]
fn bn_heb8_epoch_rename_produces_rename_hint_in_sidecar_and_list() {
    let repo = TestRepo::new();
    repo.seed_files(&[
        ("old.txt", "original content\n"),
        ("anchor.txt", "unchanged\n"),
    ]);

    // ws-edit: modify old.txt (workspace edits the OLD path).
    repo.maw_ok(&["ws", "create", "ws-edit"]);
    repo.modify_file("ws-edit", "old.txt", "EDITED content\n");
    commit_all(&repo, "ws-edit", "feat: edit old.txt");

    // ws-rename: rename old.txt → new.txt via git mv (same blob, no content
    // change), then merge into epoch.
    repo.maw_ok(&["ws", "create", "ws-rename"]);
    repo.git_in_workspace("ws-rename", &["mv", "old.txt", "new.txt"]);
    commit_all(&repo, "ws-rename", "chore: rename old.txt to new.txt");
    repo.maw_ok(&[
        "ws",
        "merge",
        "ws-rename",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge ws-rename: rename old.txt -> new.txt",
    ]);

    // old.txt must be absent from default, new.txt must be present.
    assert!(
        !repo.file_exists("default", "old.txt"),
        "old.txt should be absent from default after epoch rename"
    );
    assert!(
        repo.file_exists("default", "new.txt"),
        "new.txt should be present in default after epoch rename"
    );

    // Rebase ws-edit onto the new epoch.
    let _out = repo.maw_raw(&["ws", "sync", "ws-edit", "--rebase"]);

    // The sidecar must exist and list old.txt as a modify_delete.
    let sidecar = repo
        .read_conflict_tree_sidecar("ws-edit")
        .expect("conflict-tree.json must exist after epoch-rename vs ws-edit (bn-heb8)");

    let entry = find_conflict_entry(&sidecar, "old.txt").unwrap_or_else(|| {
        panic!(
            "sidecar should list old.txt as conflicted (bn-heb8); got:\n{}",
            serde_json::to_string_pretty(&sidecar).expect("operation should succeed")
        )
    });

    let ty = entry
        .get("type")
        .and_then(|v| v.as_str())
        .expect("conflict entry should be tagged");
    assert_eq!(
        ty, "modify_delete",
        "epoch-rename vs ws-edit should produce modify_delete, got {ty}: {entry}"
    );

    // (a) rename_hint must be present and point at new.txt.
    let hint = entry
        .get("rename_hint")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            panic!(
                "sidecar should carry rename_hint for epoch-rename case (bn-heb8); entry: {entry}"
            )
        });
    assert_eq!(
        hint, "new.txt",
        "rename_hint should point at new.txt, got {hint}"
    );

    // (b) resolve --list output must include "(renamed to new.txt)".
    let list_out = repo.maw_ok(&["ws", "resolve", "ws-edit", "--list"]);
    assert!(
        list_out.contains("renamed to new.txt") || list_out.contains("renamed to"),
        "`maw ws resolve ws-edit --list` should mention the rename target (bn-heb8); got:\n{list_out}"
    );

    // The marker stub in the worktree must also contain the rename note.
    let stub = repo.read_file("ws-edit", "old.txt").unwrap_or_else(|| {
        panic!("old.txt stub should exist in ws-edit worktree after rebase (bn-heb8)")
    });
    assert!(
        stub.contains("deleted by rename") || stub.contains("now lives at new.txt"),
        "stub should contain rename note (bn-heb8); got:\n{stub}"
    );
}

/// (c) --keep epoch on a `modify_delete` (rename case) must print the
/// discarded-blob note naming the OID and suggest git cat-file.
#[test]
fn bn_heb8_keep_epoch_on_rename_prints_discarded_blob_note() {
    let repo = TestRepo::new();
    repo.seed_files(&[("old.txt", "original\n"), ("anchor.txt", "x\n")]);

    repo.maw_ok(&["ws", "create", "ws-edit"]);
    repo.modify_file("ws-edit", "old.txt", "EDITED\n");
    commit_all(&repo, "ws-edit", "feat: edit old.txt");

    repo.maw_ok(&["ws", "create", "ws-rename"]);
    repo.git_in_workspace("ws-rename", &["mv", "old.txt", "new.txt"]);
    commit_all(&repo, "ws-rename", "chore: rename old.txt to new.txt");
    repo.maw_ok(&[
        "ws",
        "merge",
        "ws-rename",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge ws-rename: rename",
    ]);

    let _ = repo.maw_raw(&["ws", "sync", "ws-edit", "--rebase"]);

    // --keep epoch: run and capture stderr.
    let resolve_out = repo.maw_raw(&["ws", "resolve", "ws-edit", "--keep", "epoch"]);
    let stderr = String::from_utf8_lossy(&resolve_out.stderr);
    assert!(
        resolve_out.status.success(),
        "--keep epoch should succeed; stderr: {stderr}"
    );
    // Must print a note about the discarded edit.
    assert!(
        stderr.contains("discarded workspace edit") || stderr.contains("blob"),
        "--keep epoch should print discarded-blob note (bn-heb8); stderr:\n{stderr}"
    );
    // Note should also mention the rename target.
    assert!(
        stderr.contains("new.txt"),
        "--keep epoch discarded-blob note should mention rename target new.txt (bn-heb8); stderr:\n{stderr}"
    );
}

/// (c) --keep epoch on a PLAIN delete (no rename) must also print the
/// discarded-blob note (both rename and non-rename cases).
#[test]
fn bn_heb8_keep_epoch_on_plain_delete_prints_discarded_blob_note() {
    let repo = TestRepo::new();
    repo.seed_files(&[("gone.txt", "deleteme\n"), ("anchor.txt", "x\n")]);

    // ws-edit modifies gone.txt.
    repo.maw_ok(&["ws", "create", "ws-edit"]);
    repo.modify_file("ws-edit", "gone.txt", "EDITED BEFORE DELETE\n");
    commit_all(&repo, "ws-edit", "feat: edit gone.txt");

    // Epoch deletes gone.txt via a plain deletion (no rename).
    repo.maw_ok(&["ws", "create", "ws-deleter"]);
    repo.delete_file("ws-deleter", "gone.txt");
    commit_all(&repo, "ws-deleter", "chore: delete gone.txt");
    repo.maw_ok(&[
        "ws",
        "merge",
        "ws-deleter",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge ws-deleter: delete gone.txt",
    ]);

    let _ = repo.maw_raw(&["ws", "sync", "ws-edit", "--rebase"]);

    let resolve_out = repo.maw_raw(&["ws", "resolve", "ws-edit", "--keep", "epoch"]);
    let stderr = String::from_utf8_lossy(&resolve_out.stderr);
    assert!(
        resolve_out.status.success(),
        "--keep epoch should succeed on plain delete; stderr: {stderr}"
    );
    assert!(
        stderr.contains("discarded workspace edit") || stderr.contains("blob"),
        "--keep epoch should print discarded-blob note for plain delete (bn-heb8); stderr:\n{stderr}"
    );
    // Must NOT mention a rename target (it was a plain delete).
    assert!(
        !stderr.contains("renamed to") && !stderr.contains("epoch renamed"),
        "--keep epoch on plain delete must NOT print rename note (bn-heb8 no-false-positives); stderr:\n{stderr}"
    );
}

/// No false positives: epoch deletes one file and adds a DIFFERENT file with
/// different content — must NOT produce a `rename_hint`.
#[test]
fn bn_heb8_no_rename_hint_when_blobs_differ() {
    let repo = TestRepo::new();
    repo.seed_files(&[("old.txt", "original content\n"), ("anchor.txt", "x\n")]);

    // ws-edit modifies old.txt.
    repo.maw_ok(&["ws", "create", "ws-edit"]);
    repo.modify_file("ws-edit", "old.txt", "EDITED\n");
    commit_all(&repo, "ws-edit", "feat: edit old.txt");

    // Epoch deletes old.txt and adds new.txt with DIFFERENT content (not a
    // pure rename — blob changed too).
    repo.maw_ok(&["ws", "create", "ws-restructure"]);
    repo.delete_file("ws-restructure", "old.txt");
    // Add new.txt with different content (not old.txt's blob).
    repo.add_file("ws-restructure", "new.txt", "totally different content\n");
    commit_all(&repo, "ws-restructure", "chore: restructure files");
    repo.maw_ok(&[
        "ws",
        "merge",
        "ws-restructure",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge ws-restructure",
    ]);

    let _ = repo.maw_raw(&["ws", "sync", "ws-edit", "--rebase"]);

    let sidecar = repo
        .read_conflict_tree_sidecar("ws-edit")
        .expect("sidecar must exist");
    let entry = find_conflict_entry(&sidecar, "old.txt")
        .unwrap_or_else(|| panic!("old.txt should be conflicted; sidecar: {sidecar:?}"));

    // rename_hint must be absent (blobs differ → not a pure rename).
    let has_hint = entry.get("rename_hint").is_some();
    assert!(
        !has_hint,
        "rename_hint must NOT be set when delete+add have different blobs (bn-heb8 no-false-positives); entry: {entry}"
    );
}

/// Sidecar backward compat: old sidecars without `rename_hint` deserialize cleanly.
#[test]
fn bn_heb8_old_sidecar_without_rename_hint_parses() {
    // Construct a minimal conflict-tree.json that looks like it was written by
    // an older maw version (no rename_hint field in the modify_delete entry).
    // serde(default) must fill it in as None.
    let old_json = r#"{
        "base_epoch": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        "clean": {},
        "conflicts": {
            "src/old.rs": {
                "type": "modify_delete",
                "path": "src/old.rs",
                "file_id": "00000000000000000000000000000042",
                "modifier": {
                    "workspace": "ws-edit",
                    "content": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "timestamp": {
                        "epoch_id": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                        "workspace_id": "ws-edit",
                        "seq": 1,
                        "wall_clock_ms": 1700000000000
                    }
                },
                "deleter": {
                    "workspace": "epoch",
                    "content": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "timestamp": {
                        "epoch_id": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                        "workspace_id": "ws-edit",
                        "seq": 1,
                        "wall_clock_ms": 1700000000000
                    }
                },
                "modified_content": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }
        }
    }"#;

    let tree: maw_core::merge::types::ConflictTree =
        serde_json::from_str(old_json).expect("old sidecar without rename_hint must parse cleanly");

    // The conflict must have loaded with rename_hint == None.
    let conflict = tree
        .conflicts
        .get(std::path::Path::new("src/old.rs"))
        .expect("conflict should be present");
    if let maw_core::model::conflict::Conflict::ModifyDelete { rename_hint, .. } = conflict {
        assert!(
            rename_hint.is_none(),
            "rename_hint should be None for old sidecars; got {rename_hint:?}"
        );
    } else {
        panic!("expected ModifyDelete, got {conflict:?}");
    }
}

// ---------------------------------------------------------------------------
// bn-2dy1 — D/F (Directory/File) path clash detection
//
// When path P is a FILE on one side while files exist under P/ on the other,
// a git tree cannot hold both. The rebase must surface a structured
// ModifyDelete conflict (with a df_hint), NOT silently drop a side and NOT
// abort on the write_blobs_and_build_tree backstop (which wedges the
// workspace: every sync fails, merge blocks on staleness forever).
//
// These tests use the LIVE shape that originally escaped detection: two
// sibling workspaces; merging one triggers the AUTO-REBASE of the other
// (no --no-auto-rebase, no explicit `ws sync --rebase`).
//
// Direction 1: sibling commits FILE `deep`; merged ws commits deep/a/leaf.txt.
// Direction 2: sibling commits clash/sub.txt; merged ws commits FILE `clash`.
// Clean cases: `deep.txt` vs `deep/`, `deep` vs `deeper` — no spurious conflict.
// ---------------------------------------------------------------------------

/// Direction 1, live auto-rebase shape: sibling `feat` holds FILE `deep`;
/// merging `dirws` (which adds `deep/a/leaf.txt`) auto-rebases `feat`.
///
/// Required end state: the auto-rebase SUCCEEDS with a structured
/// `modify_delete` conflict at `deep`; resolve --list shows it; the merge gate
/// blocks until resolved.
#[test]
fn auto_rebase_df_clash_direction1_surfaces_structured_conflict() {
    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base\n")]);

    repo.maw_ok(&["ws", "create", "feat"]);
    repo.add_file("feat", "deep", "ws file content\n");
    commit_all(&repo, "feat", "feat: FILE deep");

    repo.maw_ok(&["ws", "create", "dirws"]);
    repo.add_file("dirws", "deep/a/leaf.txt", "leaf content\n");
    commit_all(&repo, "dirws", "dirws: deep/a/leaf.txt");

    // Merge dirws WITHOUT --no-auto-rebase: feat gets auto-rebased.
    let merge_out = repo.maw_raw(&[
        "ws",
        "merge",
        "dirws",
        "--destroy",
        "--message",
        "merge dirws",
    ]);
    let merge_text = format!(
        "{}{}",
        String::from_utf8_lossy(&merge_out.stdout),
        String::from_utf8_lossy(&merge_out.stderr)
    );
    assert!(
        merge_out.status.success(),
        "merge of dirws must succeed:\n{merge_text}"
    );

    // The auto-rebase must NOT hit the backstop ("D/F clash in rebase output
    // tree ... this is a bug") — that error means the structured-conflict
    // path failed and the workspace is wedged.
    assert!(
        !merge_text.contains("D/F clash in rebase output tree"),
        "bn-2dy1: auto-rebase hit the layer-B backstop instead of producing a \
         structured conflict:\n{merge_text}"
    );

    // Structured sidecar must record the conflict at `deep`.
    let sidecar = repo
        .read_conflict_tree_sidecar("feat")
        .expect("bn-2dy1 direction 1: conflict-tree.json must exist after the auto-rebase");
    let entry = find_conflict_entry(&sidecar, "deep").unwrap_or_else(|| {
        panic!(
            "bn-2dy1 direction 1: sidecar must list 'deep'; got:\n{}",
            serde_json::to_string_pretty(&sidecar).expect("operation should succeed")
        )
    });
    assert_eq!(
        entry.get("type").and_then(|v| v.as_str()),
        Some("modify_delete"),
        "conflict at 'deep' should be modify_delete; got: {entry}"
    );
    assert_eq!(
        entry.get("df_hint").and_then(|v| v.as_str()),
        Some("deep"),
        "conflict at 'deep' should carry df_hint=deep; got: {entry}"
    );

    // resolve --list must show it.
    let list_out = repo.maw_raw(&["ws", "resolve", "feat", "--list"]);
    let list_text = String::from_utf8_lossy(&list_out.stdout).to_string();
    assert!(
        list_text.contains("deep") && list_text.contains("modify_delete"),
        "resolve --list must show the D/F conflict at 'deep'; got:\n{list_text}"
    );

    // Merge gate must BLOCK while unresolved.
    let check = repo.maw_raw(&["ws", "merge", "feat", "--check"]);
    assert!(
        !check.status.success(),
        "merge --check must block while the D/F conflict is unresolved:\n{}",
        String::from_utf8_lossy(&check.stdout)
    );

    // The ws's file content must be present in the rendered stub (no drop).
    let stub = repo
        .read_file("feat", "deep")
        .expect("marker stub at 'deep' should exist in the worktree");
    assert!(
        stub.contains("ws file content"),
        "the workspace's file content must be visible in the conflict stub:\n{stub}"
    );
}

/// Direction 1 resolution, `--keep epoch`: the stub is deleted and the
/// epoch's directory subtree is restored from the epoch commit.
#[test]
fn auto_rebase_df_clash_direction1_keep_epoch_restores_directory() {
    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base\n")]);

    repo.maw_ok(&["ws", "create", "feat"]);
    repo.add_file("feat", "deep", "ws file content\n");
    commit_all(&repo, "feat", "feat: FILE deep");

    repo.maw_ok(&["ws", "create", "dirws"]);
    repo.add_file("dirws", "deep/a/leaf.txt", "leaf content\n");
    commit_all(&repo, "dirws", "dirws: deep/a/leaf.txt");
    repo.maw_ok(&[
        "ws",
        "merge",
        "dirws",
        "--destroy",
        "--message",
        "merge dirws",
    ]);

    repo.maw_ok(&["ws", "resolve", "feat", "--keep", "epoch"]);

    assert_eq!(
        repo.read_file("feat", "deep/a/leaf.txt").as_deref(),
        Some("leaf content\n"),
        "--keep epoch must restore the epoch's directory content"
    );
    assert!(
        !repo.workspace_path("feat").join("deep").is_file(),
        "--keep epoch must remove the workspace's FILE at 'deep'"
    );

    // Merge gate must now pass.
    let check = repo.maw_raw(&["ws", "merge", "feat", "--check"]);
    assert!(
        check.status.success(),
        "merge --check must pass after --keep epoch:\n{}{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );

    // Merging feat must NOT delete the epoch's directory from main.
    repo.maw_ok(&[
        "ws",
        "merge",
        "feat",
        "--destroy",
        "--message",
        "merge feat",
    ]);
    let epoch = repo.current_epoch();
    let files = repo.git_ls_tree("default", &epoch);
    assert!(
        files.iter().any(|(_, p)| p == "deep/a/leaf.txt"),
        "epoch's deep/a/leaf.txt must survive in main after --keep epoch; got: {files:?}"
    );
}

/// Direction 1 resolution, `--keep <ws>`: the workspace's FILE content is
/// written at the collision root and the merge replaces the directory.
#[test]
fn auto_rebase_df_clash_direction1_keep_ws_file_wins() {
    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base\n")]);

    repo.maw_ok(&["ws", "create", "feat"]);
    repo.add_file("feat", "deep", "ws file content\n");
    commit_all(&repo, "feat", "feat: FILE deep");

    repo.maw_ok(&["ws", "create", "dirws"]);
    repo.add_file("dirws", "deep/a/leaf.txt", "leaf content\n");
    commit_all(&repo, "dirws", "dirws: deep/a/leaf.txt");
    repo.maw_ok(&[
        "ws",
        "merge",
        "dirws",
        "--destroy",
        "--message",
        "merge dirws",
    ]);

    repo.maw_ok(&["ws", "resolve", "feat", "--keep", "feat"]);

    assert_eq!(
        repo.read_file("feat", "deep").as_deref(),
        Some("ws file content\n"),
        "--keep feat must write the workspace's file content at 'deep'"
    );

    // The user explicitly chose the FILE side: merging replaces the epoch's
    // directory with the file. The merge gate must allow it (the ws patch's
    // internal `Deleted deep/a/leaf.txt` + `Added deep` restructure is
    // consistent, not a D/F clash).
    let check = repo.maw_raw(&["ws", "merge", "feat", "--check"]);
    assert!(
        check.status.success(),
        "merge --check must pass after --keep feat:\n{}{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
    repo.maw_ok(&[
        "ws",
        "merge",
        "feat",
        "--destroy",
        "--message",
        "merge feat",
    ]);
    let epoch = repo.current_epoch();
    let files = repo.git_ls_tree("default", &epoch);
    assert!(
        files.iter().any(|(_, p)| p == "deep"),
        "FILE 'deep' must be in main after --keep feat; got: {files:?}"
    );
    assert!(
        !files.iter().any(|(_, p)| p == "deep/a/leaf.txt"),
        "the directory side must be gone after the user chose the file; got: {files:?}"
    );
}

/// Direction 2, live auto-rebase shape: sibling `feat` holds `clash/sub.txt`;
/// merging `filews` (which adds FILE `clash`) auto-rebases `feat`.
///
/// The conflict is keyed at the WS child path (`clash/sub.txt`) with
/// `df_hint` = `clash`.
#[test]
fn auto_rebase_df_clash_direction2_surfaces_structured_conflict() {
    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base\n")]);

    repo.maw_ok(&["ws", "create", "feat"]);
    repo.add_file("feat", "clash/sub.txt", "ws sub content\n");
    commit_all(&repo, "feat", "feat: clash/sub.txt");

    repo.maw_ok(&["ws", "create", "filews"]);
    repo.add_file("filews", "clash", "epoch file content\n");
    commit_all(&repo, "filews", "filews: FILE clash");

    let merge_out = repo.maw_raw(&[
        "ws",
        "merge",
        "filews",
        "--destroy",
        "--message",
        "merge filews",
    ]);
    let merge_text = format!(
        "{}{}",
        String::from_utf8_lossy(&merge_out.stdout),
        String::from_utf8_lossy(&merge_out.stderr)
    );
    assert!(
        merge_out.status.success(),
        "merge of filews must succeed:\n{merge_text}"
    );
    assert!(
        !merge_text.contains("D/F clash in rebase output tree"),
        "bn-2dy1: auto-rebase hit the layer-B backstop:\n{merge_text}"
    );

    let sidecar = repo
        .read_conflict_tree_sidecar("feat")
        .expect("bn-2dy1 direction 2: conflict-tree.json must exist after the auto-rebase");
    let entry = find_conflict_entry(&sidecar, "clash/sub.txt").unwrap_or_else(|| {
        panic!(
            "bn-2dy1 direction 2: sidecar must list 'clash/sub.txt'; got:\n{}",
            serde_json::to_string_pretty(&sidecar).expect("operation should succeed")
        )
    });
    assert_eq!(
        entry.get("type").and_then(|v| v.as_str()),
        Some("modify_delete"),
        "conflict should be modify_delete; got: {entry}"
    );
    assert_eq!(
        entry.get("df_hint").and_then(|v| v.as_str()),
        Some("clash"),
        "conflict should carry df_hint=clash; got: {entry}"
    );

    // Merge gate must block.
    let check = repo.maw_raw(&["ws", "merge", "feat", "--check"]);
    assert!(
        !check.status.success(),
        "merge --check must block while the D/F conflict is unresolved"
    );

    // The ws's content must be visible in the stub (no drop).
    let stub = repo
        .read_file("feat", "clash/sub.txt")
        .expect("marker stub at clash/sub.txt should exist");
    assert!(
        stub.contains("ws sub content"),
        "ws content must be visible in the conflict stub:\n{stub}"
    );
}

/// Direction 2 resolution, `--keep epoch`: the ws child stub is deleted and
/// the epoch's FILE at the collision root is restored.
#[test]
fn auto_rebase_df_clash_direction2_keep_epoch_restores_file() {
    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base\n")]);

    repo.maw_ok(&["ws", "create", "feat"]);
    repo.add_file("feat", "clash/sub.txt", "ws sub content\n");
    commit_all(&repo, "feat", "feat: clash/sub.txt");

    repo.maw_ok(&["ws", "create", "filews"]);
    repo.add_file("filews", "clash", "epoch file content\n");
    commit_all(&repo, "filews", "filews: FILE clash");
    repo.maw_ok(&[
        "ws",
        "merge",
        "filews",
        "--destroy",
        "--message",
        "merge filews",
    ]);

    repo.maw_ok(&["ws", "resolve", "feat", "--keep", "epoch"]);

    assert_eq!(
        repo.read_file("feat", "clash").as_deref(),
        Some("epoch file content\n"),
        "--keep epoch must restore the epoch's FILE at 'clash'"
    );
    assert!(
        !repo.workspace_path("feat").join("clash").is_dir(),
        "--keep epoch must remove the workspace's directory at 'clash'"
    );

    let check = repo.maw_raw(&["ws", "merge", "feat", "--check"]);
    assert!(
        check.status.success(),
        "merge --check must pass after --keep epoch:\n{}{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
}

/// Direction 2 resolution, `--keep <ws>`: the ws child content is written and
/// the epoch's FILE stays out; merging replaces the file with the directory.
#[test]
fn auto_rebase_df_clash_direction2_keep_ws_dir_wins() {
    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base\n")]);

    repo.maw_ok(&["ws", "create", "feat"]);
    repo.add_file("feat", "clash/sub.txt", "ws sub content\n");
    commit_all(&repo, "feat", "feat: clash/sub.txt");

    repo.maw_ok(&["ws", "create", "filews"]);
    repo.add_file("filews", "clash", "epoch file content\n");
    commit_all(&repo, "filews", "filews: FILE clash");
    repo.maw_ok(&[
        "ws",
        "merge",
        "filews",
        "--destroy",
        "--message",
        "merge filews",
    ]);

    repo.maw_ok(&["ws", "resolve", "feat", "--keep", "feat"]);

    assert_eq!(
        repo.read_file("feat", "clash/sub.txt").as_deref(),
        Some("ws sub content\n"),
        "--keep feat must write the ws child's content"
    );

    repo.maw_ok(&[
        "ws",
        "merge",
        "feat",
        "--destroy",
        "--message",
        "merge feat",
    ]);
    let epoch = repo.current_epoch();
    let files = repo.git_ls_tree("default", &epoch);
    assert!(
        files.iter().any(|(_, p)| p == "clash/sub.txt"),
        "clash/sub.txt must be in main after --keep feat; got: {files:?}"
    );
    assert!(
        !files.iter().any(|(_, p)| p == "clash"),
        "epoch's FILE 'clash' must be gone after the user chose the directory; got: {files:?}"
    );
}

/// Explicit-sync shape (the original test shape): `ws sync feat --rebase`
/// after a `--no-auto-rebase` merge must surface the same structured
/// conflict — the rebase must SUCCEED (no backstop abort).
#[test]
fn sync_rebase_df_clash_direction1_explicit_sync() {
    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base\n")]);

    repo.maw_ok(&["ws", "create", "feat"]);
    repo.add_file("feat", "clash", "ws file content\n");
    commit_all(&repo, "feat", "feat: add file 'clash'");

    repo.maw_ok(&["ws", "create", "advancer"]);
    repo.add_file("advancer", "clash/sub.txt", "epoch dir content\n");
    commit_all(&repo, "advancer", "epoch: add clash/sub.txt");
    repo.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge advancer",
    ]);

    let rebase_out = repo.maw_raw(&["ws", "sync", "feat", "--rebase"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&rebase_out.stdout),
        String::from_utf8_lossy(&rebase_out.stderr)
    );
    assert!(
        rebase_out.status.success(),
        "explicit sync --rebase must SUCCEED with a structured conflict, not abort:\n{text}"
    );
    assert!(
        !text.contains("D/F clash in rebase output tree"),
        "bn-2dy1: explicit rebase hit the layer-B backstop:\n{text}"
    );

    let sidecar = repo
        .read_conflict_tree_sidecar("feat")
        .expect("conflict-tree.json must exist after the D/F rebase");
    assert!(
        find_conflict_entry(&sidecar, "clash").is_some(),
        "sidecar must list 'clash'; got:\n{}",
        serde_json::to_string_pretty(&sidecar).expect("operation should succeed")
    );
}

/// Clean case: `deep.txt` vs `deep/sub.txt` — different names, no clash.
#[test]
fn sync_rebase_no_false_positive_for_name_with_extension() {
    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base\n")]);

    repo.maw_ok(&["ws", "create", "feat"]);
    repo.add_file("feat", "deep.txt", "file content\n");
    commit_all(&repo, "feat", "feat: add deep.txt");

    repo.maw_ok(&["ws", "create", "advancer"]);
    repo.add_file("advancer", "deep/sub.txt", "dir content\n");
    commit_all(&repo, "advancer", "epoch: add deep/sub.txt");
    repo.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge advancer",
    ]);

    let rebase_out = repo.maw_raw(&["ws", "sync", "feat", "--rebase"]);
    assert!(
        rebase_out.status.success(),
        "clean case: deep.txt vs deep/sub.txt must not conflict (false positive)\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&rebase_out.stdout),
        String::from_utf8_lossy(&rebase_out.stderr),
    );

    let sidecar = repo.read_conflict_tree_sidecar("feat");
    assert!(
        sidecar.is_none(),
        "bn-2dy1 false positive: deep.txt vs deep/sub.txt must NOT produce a \
         conflict sidecar; got: {sidecar:?}"
    );

    assert_eq!(
        repo.read_file("feat", "deep.txt").as_deref(),
        Some("file content\n"),
        "deep.txt must survive the rebase"
    );
    assert_eq!(
        repo.read_file("feat", "deep/sub.txt").as_deref(),
        Some("dir content\n"),
        "deep/sub.txt must survive the rebase"
    );
}

/// Clean case: `deep` vs `deeper` — `deep` is NOT a component-wise prefix.
#[test]
fn sync_rebase_no_false_positive_for_similar_named_files() {
    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base\n")]);

    repo.maw_ok(&["ws", "create", "feat"]);
    repo.add_file("feat", "deep", "deep content\n");
    commit_all(&repo, "feat", "feat: add deep");

    repo.maw_ok(&["ws", "create", "advancer"]);
    repo.add_file("advancer", "deeper", "deeper content\n");
    commit_all(&repo, "advancer", "epoch: add deeper");
    repo.maw_ok(&[
        "ws",
        "merge",
        "advancer",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge advancer",
    ]);

    let rebase_out = repo.maw_raw(&["ws", "sync", "feat", "--rebase"]);
    assert!(
        rebase_out.status.success(),
        "clean case: 'deep' vs 'deeper' must not conflict\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&rebase_out.stdout),
        String::from_utf8_lossy(&rebase_out.stderr),
    );

    let sidecar = repo.read_conflict_tree_sidecar("feat");
    assert!(
        sidecar.is_none(),
        "bn-2dy1 false positive: 'deep' vs 'deeper' must NOT produce a \
         conflict sidecar; got: {sidecar:?}"
    );

    assert_eq!(
        repo.read_file("feat", "deep").as_deref(),
        Some("deep content\n"),
        "'deep' must survive the rebase"
    );
    assert_eq!(
        repo.read_file("feat", "deeper").as_deref(),
        Some("deeper content\n"),
        "'deeper' must survive the rebase"
    );
}
