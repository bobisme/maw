//! bn-3hqg — submodule (gitlink) handling during `maw ws sync --rebase`.
//!
//! A submodule is tracked in the parent tree as a mode-160000 entry whose OID
//! is a **commit** in a different repository — not a blob in the parent's
//! object store. The rebase pipeline used to unconditionally `read_blob` any
//! new/modified entry, which blew up with `not found: blob <sha>` the moment
//! it hit a gitlink.
//!
//! The fix treats submodule entries as **opaque**: the gitlink SHA flows
//! through the merge pipeline as the entry's identity, no blob bytes are
//! read, and the final tree preserves the `mode 160000` + gitlink-SHA pair.
//! Submodule-vs-submodule conflicts (both sides bump the gitlink to
//! different SHAs) are not yet handled and bail with a clear error rather
//! than producing an unparseable marker blob.
//!
//! These tests exercise three shapes:
//!
//!   * clean rebase where the workspace added a submodule and the epoch
//!     touched an unrelated file;
//!   * clean rebase where the workspace bumps an already-present submodule;
//!   * the bail path when workspace + epoch bump the same submodule to
//!     different commits;
//!   * the bail path when the workspace deletes a submodule while the epoch
//!     bumps it.

mod manifold_common;

use std::path::Path;
use std::process::Command;

use manifold_common::{TestRepo, git_ok};
use tempfile::TempDir;

/// Build a standalone git repo on disk that can be used as a submodule
/// source. Returns the `TempDir` (must outlive the submodule reference) and
/// the SHA of the single commit in the repo.
fn make_sub_source(initial_content: &str) -> (TempDir, String) {
    let dir = TempDir::new().expect("tempdir for submodule source");
    git_ok(dir.path(), &["init", "-q", "--initial-branch=main"]);
    git_ok(dir.path(), &["config", "user.name", "Test"]);
    git_ok(dir.path(), &["config", "user.email", "test@test.com"]);
    git_ok(dir.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.path().join("sub.txt"), initial_content).expect("write sub.txt");
    git_ok(dir.path(), &["add", "-A"]);
    git_ok(dir.path(), &["commit", "-qm", "sub: initial"]);
    let sha = git_ok(dir.path(), &["rev-parse", "HEAD"]).trim().to_owned();
    (dir, sha)
}

/// Append a new commit to the submodule source repo and return its SHA.
fn add_sub_commit(dir: &Path, new_content: &str, message: &str) -> String {
    std::fs::write(dir.join("sub.txt"), new_content).expect("write sub.txt");
    git_ok(dir, &["add", "-A"]);
    git_ok(dir, &["commit", "-qm", message]);
    git_ok(dir, &["rev-parse", "HEAD"]).trim().to_owned()
}

/// Add a submodule at `rel_path` in `workspace`, pointing at `source_dir`.
///
/// Uses `protocol.file.allow=always` so the local clone goes through without
/// git's file-transport safeguards tripping. Commits the resulting
/// `.gitmodules` + gitlink entry in the workspace.
fn add_submodule_to_workspace(
    repo: &TestRepo,
    workspace: &str,
    rel_path: &str,
    source_dir: &Path,
    commit_message: &str,
) {
    let ws = repo.workspace_path(workspace);
    // `git submodule add` requires file-protocol to be explicitly permitted.
    let status = Command::new("git")
        .args([
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            source_dir.to_str().expect("operation should succeed"),
            rel_path,
        ])
        .current_dir(&ws)
        .status()
        .expect("failed to spawn git submodule add");
    assert!(status.success(), "git submodule add failed");
    git_ok(&ws, &["add", "-A"]);
    git_ok(&ws, &["commit", "-m", commit_message]);
}

/// Update an already-added submodule to track `new_sha`. Stages the gitlink
/// change and commits it in the parent workspace with `message`.
///
/// We bypass the submodule's working-tree checkout entirely and instead
/// directly rewrite the parent's index entry for `rel_path` via
/// `git update-index --cacheinfo 160000,<sha>,<path>`. That is semantically
/// identical to bumping the gitlink and sidesteps the working-tree hassle
/// of fetching the new commit into the submodule's object store, which
/// isn't needed for what we're testing (the rebase pipeline cares only about
/// tree contents, not whether the submodule's checkout is present).
fn bump_submodule_in_workspace(
    repo: &TestRepo,
    workspace: &str,
    rel_path: &str,
    _source_dir: &Path,
    new_sha: &str,
    message: &str,
) {
    let ws = repo.workspace_path(workspace);
    // Rewrite the gitlink entry in the parent's index directly.
    git_ok(
        &ws,
        &[
            "update-index",
            "--cacheinfo",
            &format!("160000,{new_sha},{rel_path}"),
        ],
    );
    git_ok(&ws, &["commit", "-m", message]);
}

/// Remove an already-added submodule from the workspace and commit the delete.
fn remove_submodule_from_workspace(
    repo: &TestRepo,
    workspace: &str,
    rel_path: &str,
    message: &str,
) {
    let ws = repo.workspace_path(workspace);
    git_ok(&ws, &["rm", "-f", rel_path, ".gitmodules"]);
    git_ok(&ws, &["commit", "-m", message]);
}

/// Extract the gitlink SHA for `rel_path` from `git ls-tree HEAD` in a
/// workspace. Returns `None` if the entry isn't a gitlink.
fn gitlink_sha_at(repo: &TestRepo, workspace: &str, rel_path: &str) -> Option<String> {
    let entries = repo.git_ls_tree(workspace, "HEAD");
    for (mode, path) in entries {
        if path == rel_path && mode == "160000" {
            // Re-run ls-tree to grab the OID column (the helper only returns
            // mode+path; we need the SHA here).
            let ws = repo.workspace_path(workspace);
            let raw = git_ok(&ws, &["ls-tree", "HEAD", rel_path]);
            // Format: "<mode> <type> <oid>\t<path>\n"
            let meta = raw.split('\t').next()?;
            let parts: Vec<&str> = meta.split_whitespace().collect();
            if parts.len() >= 3 {
                return Some(parts[2].to_owned());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Workspace adds a submodule; epoch advances on an unrelated file.
/// Rebase must succeed and the submodule must survive at its original SHA.
#[test]
fn rebase_preserves_unchanged_submodule() {
    let repo = TestRepo::new();
    repo.seed_files(&[("f.txt", "base\n")]);

    // Submodule source repo (kept alive via the returned TempDir).
    let (sub_src, sub_sha) = make_sub_source("v1\n");

    // Create workspace and add the submodule.
    repo.maw_ok(&["ws", "create", "feat"]);
    add_submodule_to_workspace(
        &repo,
        "feat",
        "subdir",
        sub_src.path(),
        "feat: add submodule",
    );

    // Sanity: before rebase the workspace really has a gitlink entry.
    assert_eq!(
        gitlink_sha_at(&repo, "feat", "subdir").as_deref(),
        Some(sub_sha.as_str()),
        "setup: workspace should have gitlink at `subdir` with sub_sha"
    );

    // Advance the epoch with an unrelated change.
    repo.modify_file("default", "f.txt", "changed\n");
    repo.advance_epoch("chore: unrelated epoch change");

    // Rebase — must not blow up with `not found: blob <sha>`.
    let stdout = repo.maw_ok(&["ws", "sync", "--rebase", "feat"]);
    assert!(
        stdout.contains("Replayed") || stdout.contains("up to date"),
        "rebase stdout should indicate replay succeeded: {stdout}"
    );

    // After rebase, the submodule gitlink survives unchanged.
    assert_eq!(
        gitlink_sha_at(&repo, "feat", "subdir").as_deref(),
        Some(sub_sha.as_str()),
        "submodule gitlink must be preserved across rebase"
    );

    // The unrelated epoch change is visible in the workspace.
    let contents = repo.read_file("feat", "f.txt").expect("f.txt present");
    assert_eq!(
        contents, "changed\n",
        "unrelated epoch change must land in workspace"
    );
}

/// Workspace bumps an already-present submodule to a new SHA; epoch doesn't
/// touch it. Rebase must preserve the workspace's bumped gitlink.
#[test]
fn rebase_preserves_submodule_when_workspace_bumps_it() {
    let repo = TestRepo::new();
    repo.seed_files(&[("f.txt", "base\n")]);

    // Source repo with an initial commit; note sub_v1 for the starting SHA.
    let (sub_src, _sub_v1) = make_sub_source("v1\n");

    // Add the submodule in `default` at v1, advance the epoch so it's the
    // shared baseline for new workspaces.
    add_submodule_to_workspace(
        &repo,
        "default",
        "subdir",
        sub_src.path(),
        "chore: baseline submodule",
    );
    let epoch_with_sub = repo.advance_epoch("chore: seed submodule baseline");
    assert!(!epoch_with_sub.is_empty());

    // Create workspace off that epoch, bump the submodule to v2.
    repo.maw_ok(&["ws", "create", "feat"]);
    let sub_v2 = add_sub_commit(sub_src.path(), "v2\n", "sub: v2");
    bump_submodule_in_workspace(
        &repo,
        "feat",
        "subdir",
        sub_src.path(),
        &sub_v2,
        "feat: bump submodule to v2",
    );

    // Advance the epoch with an unrelated change so the rebase has something
    // to replay onto.
    repo.modify_file("default", "f.txt", "changed\n");
    repo.advance_epoch("chore: unrelated epoch change");

    // Rebase.
    repo.maw_ok(&["ws", "sync", "--rebase", "feat"]);

    // Workspace's bumped gitlink must be preserved.
    assert_eq!(
        gitlink_sha_at(&repo, "feat", "subdir").as_deref(),
        Some(sub_v2.as_str()),
        "workspace's bumped submodule SHA must survive rebase"
    );
}

/// Workspace and epoch both bump the submodule to different SHAs. The rebase
/// must bail with a clear submodule-conflict error (not `not found: blob`).
#[test]
fn rebase_with_submodule_conflict_bails_cleanly() {
    let repo = TestRepo::new();
    repo.seed_files(&[("f.txt", "base\n")]);

    let (sub_src, _sub_v1) = make_sub_source("v1\n");

    // Seed: add submodule at v1 in default, advance epoch.
    add_submodule_to_workspace(
        &repo,
        "default",
        "subdir",
        sub_src.path(),
        "chore: baseline submodule",
    );
    repo.advance_epoch("chore: seed submodule baseline");

    // Workspace bumps submodule to v2.
    repo.maw_ok(&["ws", "create", "feat"]);
    let sub_v2 = add_sub_commit(sub_src.path(), "v2\n", "sub: v2");
    bump_submodule_in_workspace(
        &repo,
        "feat",
        "subdir",
        sub_src.path(),
        &sub_v2,
        "feat: bump to v2",
    );

    // Epoch (default) concurrently bumps submodule to v3 (different from v2).
    // We drive default directly so `advance_epoch`'s `git add -A` doesn't
    // overwrite our gitlink index rewrite (add -A reads the worktree, which
    // still has v1 checked out — it would re-stage v1).
    let sub_v3 = add_sub_commit(sub_src.path(), "v3\n", "sub: v3");
    let default_ws = repo.workspace_path("default");
    git_ok(
        &default_ws,
        &[
            "update-index",
            "--cacheinfo",
            &format!("160000,{sub_v3},subdir"),
        ],
    );
    git_ok(
        &default_ws,
        &["commit", "-m", "chore: epoch bumps submodule"],
    );
    let new_epoch = git_ok(&default_ws, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    git_ok(
        repo.root(),
        &["update-ref", "refs/manifold/epoch/current", &new_epoch],
    );
    git_ok(repo.root(), &["update-ref", "refs/heads/main", &new_epoch]);
    git_ok(
        repo.root(),
        &["update-ref", "refs/manifold/epoch/ws/default", &new_epoch],
    );

    // Rebase must fail, but with a *clear* error — not with `not found: blob`.
    let stderr = repo.maw_fails(&["ws", "sync", "--rebase", "feat"]);
    assert!(
        !stderr.contains("not found: blob"),
        "submodule conflict must not crash with `not found: blob`. stderr: {stderr}"
    );
    assert!(
        stderr.to_lowercase().contains("submodule"),
        "error message should mention submodule. stderr: {stderr}"
    );
}

/// Workspace deletes a submodule while the epoch bumps it to a new SHA.
/// Rebase must bail cleanly instead of trying to read the gitlink SHA as a
/// blob during generic modify/delete materialization.
#[test]
fn rebase_with_submodule_delete_vs_bump_bails_cleanly() {
    let repo = TestRepo::new();
    repo.seed_files(&[("f.txt", "base\n")]);

    let (sub_src, _sub_v1) = make_sub_source("v1\n");

    add_submodule_to_workspace(
        &repo,
        "default",
        "subdir",
        sub_src.path(),
        "chore: baseline submodule",
    );
    repo.advance_epoch("chore: seed submodule baseline");

    repo.maw_ok(&["ws", "create", "feat"]);
    remove_submodule_from_workspace(&repo, "feat", "subdir", "feat: delete submodule");

    let sub_v2 = add_sub_commit(sub_src.path(), "v2\n", "sub: v2");
    let default_ws = repo.workspace_path("default");
    git_ok(
        &default_ws,
        &[
            "update-index",
            "--cacheinfo",
            &format!("160000,{sub_v2},subdir"),
        ],
    );
    git_ok(
        &default_ws,
        &["commit", "-m", "chore: epoch bumps submodule"],
    );
    let new_epoch = git_ok(&default_ws, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    git_ok(
        repo.root(),
        &["update-ref", "refs/manifold/epoch/current", &new_epoch],
    );
    git_ok(repo.root(), &["update-ref", "refs/heads/main", &new_epoch]);
    git_ok(
        repo.root(),
        &["update-ref", "refs/manifold/epoch/ws/default", &new_epoch],
    );

    let stderr = repo.maw_fails(&["ws", "sync", "--rebase", "feat"]);
    assert!(
        !stderr.contains("not found: blob"),
        "submodule delete-vs-bump must not crash with `not found: blob`. stderr: {stderr}"
    );
    assert!(
        stderr.to_lowercase().contains("submodule"),
        "error message should mention submodule. stderr: {stderr}"
    );
}
