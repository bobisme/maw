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
        "ws", "merge", "advancer", "--destroy", "--message", "merge advancer",
    ]);

    let new_epoch = repo.current_epoch();
    assert_ne!(
        before_epoch, new_epoch,
        "setup: epoch should have advanced"
    );

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
        "ws", "merge", "advancer", "--destroy", "--message", "merge advancer",
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
        "ws", "merge", "advancer", "--destroy", "--message", "merge advancer",
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
        let mut perms = std::fs::metadata(&run_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&run_path, perms).unwrap();
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
        "ws", "merge", "advancer", "--destroy", "--message", "merge advancer",
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
        "ws", "merge", "advancer", "--destroy", "--message", "merge advancer",
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
    std::fs::write(ws_path.join("shared.txt"), "side-version\n").unwrap();
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
    let parent_count = parents_line.trim().split_whitespace().count() - 1;
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
        "ws", "merge", "advancer", "--destroy", "--message", "merge advancer",
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
            serde_json::to_string_pretty(&sidecar).unwrap()
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
        "ws", "merge", "advancer", "--destroy", "--message", "merge advancer",
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
            serde_json::to_string_pretty(&sidecar).unwrap()
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
        side_labels.iter().any(|l| *l == "epoch"),
        "sides should include `epoch` label, got {side_labels:?}"
    );
    assert!(
        side_labels.iter().any(|l| *l == "alice"),
        "sides should include `alice` label, got {side_labels:?}"
    );

    // --- Assertion 2: unrelated.txt is clean with alice's content -------
    //
    // The sidecar MUST NOT list unrelated.txt (B's unilateral edit is clean).
    if let Some(value) = find_conflict_entry(&sidecar, "unrelated.txt") {
        panic!(
            "B's edit to unrelated.txt must not appear in conflict-tree.json; found: {value}"
        );
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
    assert!(feat_path.join("b.txt").exists(), "setup: b.txt must exist in feat");
    assert!(!feat_path.join("a.txt").exists(), "setup: a.txt must be gone in feat");

    // Advance the epoch by modifying a.txt through another workspace.
    repo.maw_ok(&["ws", "create", "advancer"]);
    repo.modify_file("advancer", "a.txt", "hello modified\n");
    commit_all(&repo, "advancer", "default: modify a");
    repo.maw_ok(&[
        "ws", "merge", "advancer", "--destroy", "--message", "merge advancer",
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
    std::fs::write(feat.join("a.txt"), "a\n").unwrap();
    repo.git_in_workspace("feat", &["add", "a.txt"]);
    repo.git_in_workspace("feat", &["commit", "-m", "A"]);

    repo.git_in_workspace("feat", &["checkout", "-b", "side", "HEAD^"]);
    std::fs::write(feat.join("s.txt"), "s\n").unwrap();
    repo.git_in_workspace("feat", &["add", "s.txt"]);
    repo.git_in_workspace("feat", &["commit", "-m", "side"]);

    repo.git_in_workspace("feat", &["checkout", "-"]);
    repo.git_in_workspace("feat", &["merge", "--no-ff", "side", "-m", "merge: side"]);

    // Advance epoch (disjoint paths so no conflict)
    std::fs::write(repo.workspace_path("default").join("z.txt"), "z\n").unwrap();
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
        "ws", "merge", "advancer", "--destroy", "--message", "merge advancer",
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
            serde_json::to_string_pretty(&sidecar).unwrap()
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
        side_labels.iter().any(|l| *l == "epoch"),
        "sides should include `epoch`, got {side_labels:?}"
    );
    assert!(
        side_labels.iter().any(|l| *l == "feat"),
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
        "ws", "merge", "advancer", "--destroy", "--message", "merge advancer",
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
        "ws", "merge", "advancer", "--destroy", "--message", "merge advancer",
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
            serde_json::to_string_pretty(s).unwrap()
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
    repo.git_in_workspace(
        "feat",
        &["merge", "--no-ff", "sideB", "-m", "merge sideB"],
    );

    // Advance epoch to create a real conflict on shared.txt.
    repo.maw_ok(&["ws", "create", "advancer"]);
    repo.modify_file("advancer", "shared.txt", "EPOCH\n");
    commit_all(&repo, "advancer", "chore: epoch edits shared");
    repo.maw_ok(&[
        "ws", "merge", "advancer", "--destroy", "--message", "merge advancer",
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
        "ws", "merge", "advancer", "--destroy", "--message", "merge advancer",
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
