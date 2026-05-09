//! Integration tests for FF-absorb on `maw ws merge` (bn-11ip).
//!
//! These tests drive the merge precondition path with a target branch that
//! has been advanced past the epoch by direct commits (the typical "someone
//! ran `git push` outside maw" scenario), and assert that:
//!
//! 1. The FF range is absorbed silently when no in-flight workspace touches
//!    any path it modifies.
//! 2. The merge is blocked with the legacy diverged error — augmented with
//!    an "Affected workspace(s)" line — when an in-flight workspace touches
//!    any of those paths.
//! 3. Fork divergence (where the branch has commits unreachable from the
//!    epoch) is *not* absorbed.

mod manifold_common;

use manifold_common::{TestRepo, git_ok};

/// Advance `refs/heads/main` to a new commit while leaving
/// `refs/manifold/epoch/current` untouched.
///
/// Mirrors the user committing+pushing directly to `main` outside of `maw`.
/// Returns the OID of the new branch tip.
fn push_branch_ahead(repo: &TestRepo, file_path: &str, content: &str, message: &str) -> String {
    let ws_default = repo.default_workspace();

    // Capture refs at start.
    let epoch_before = repo.current_epoch();
    let main_before = git_ok(repo.root(), &["rev-parse", "refs/heads/main"])
        .trim()
        .to_owned();
    assert_eq!(
        epoch_before, main_before,
        "test setup expects epoch and main to match before push_branch_ahead"
    );

    // Edit the file in default and commit on top.
    std::fs::write(ws_default.join(file_path), content)
        .unwrap_or_else(|e| panic!("write {file_path}: {e}"));
    git_ok(&ws_default, &["add", "-A"]);
    git_ok(&ws_default, &["commit", "-m", message]);

    let new_oid = git_ok(&ws_default, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();

    // Advance main but NOT the epoch ref — this is the "direct commits"
    // shape the FF-absorb path is designed to handle.
    git_ok(repo.root(), &["update-ref", "refs/heads/main", &new_oid]);

    // Reset default's HEAD back to the epoch so subsequent `maw` commands
    // see a clean default workspace anchored at the (still-old) epoch ref.
    git_ok(&ws_default, &["reset", "--hard", &epoch_before]);

    new_oid
}

/// Advance the branch with two commits that touch *different* paths than
/// the workspace, used by the multi-commit absorb test.
fn push_two_commits_ahead(repo: &TestRepo, files: &[(&str, &str)], messages: &[&str]) -> String {
    let ws_default = repo.default_workspace();
    let epoch_before = repo.current_epoch();

    for ((path, content), msg) in files.iter().zip(messages.iter()) {
        std::fs::write(ws_default.join(path), content)
            .unwrap_or_else(|e| panic!("write {path}: {e}"));
        git_ok(&ws_default, &["add", "-A"]);
        git_ok(&ws_default, &["commit", "-m", *msg]);
    }

    let new_oid = git_ok(&ws_default, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    git_ok(repo.root(), &["update-ref", "refs/heads/main", &new_oid]);
    git_ok(&ws_default, &["reset", "--hard", &epoch_before]);
    new_oid
}

/// Force the branch to a divergent line of history that is *not* an
/// ancestor of the epoch — i.e. the epoch and branch have a non-trivial
/// merge base. Used by the fork-divergence test.
fn force_branch_to_divergent(
    repo: &TestRepo,
    file_path: &str,
    content: &str,
    message: &str,
) -> String {
    let ws_default = repo.default_workspace();
    let epoch_before = repo.current_epoch();

    // Make a divergent commit on TOP of epoch₀ (the original epoch),
    // then reset main to it. The repo's *current* epoch is at epoch_before
    // (post-seed) so this branch tip is NOT an ancestor of the current
    // epoch — and the current epoch is NOT an ancestor of the branch tip
    // (their merge-base is epoch₀, both have unique commits).
    git_ok(&ws_default, &["checkout", "--detach", repo.epoch0()]);
    std::fs::write(ws_default.join(file_path), content)
        .unwrap_or_else(|e| panic!("write {file_path}: {e}"));
    git_ok(&ws_default, &["add", "-A"]);
    git_ok(&ws_default, &["commit", "-m", message]);
    let divergent = git_ok(&ws_default, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();

    git_ok(repo.root(), &["update-ref", "refs/heads/main", &divergent]);

    // Reset default back to current epoch.
    git_ok(&ws_default, &["reset", "--hard", &epoch_before]);
    git_ok(&ws_default, &["clean", "-fd"]);

    divergent
}

#[test]
fn ff_absorb_succeeds_when_no_workspace_touches_ff_paths() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "// lib\n"), ("docs/README.md", "# README\n")]);

    // Create alice and have her edit code only.
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.modify_file("alice", "src/lib.rs", "// lib\n// alice's tweak\n");

    // Direct commit on main touching docs only.
    push_branch_ahead(
        &repo,
        "docs/README.md",
        "# README\n\nupdated\n",
        "docs: update",
    );

    // Merge alice into default. The branch is ahead but the FF range only
    // touches docs/README.md, which alice does not touch — should absorb
    // silently and proceed.
    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "alice",
        "--destroy",
        "--message",
        "merge alice",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    assert!(
        out.status.success(),
        "merge should succeed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("Absorbed") && stderr.contains("upstream commit"),
        "stderr should announce absorb.\nstderr: {stderr}"
    );

    // Final epoch should reflect alice's merge AND the absorbed docs change.
    assert_eq!(
        repo.read_file("default", "docs/README.md").as_deref(),
        Some("# README\n\nupdated\n"),
        "default should carry the absorbed branch commit\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert_eq!(
        repo.read_file("default", "src/lib.rs").as_deref(),
        Some("// lib\n// alice's tweak\n"),
        "default should carry alice's edit\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
fn ff_absorb_blocks_when_workspace_touches_ff_path() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "// lib\n")]);

    // Alice edits the same file the branch will advance.
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.modify_file("alice", "src/lib.rs", "// lib\n// alice\n");

    // Direct commit on main touches src/lib.rs — overlaps with alice.
    push_branch_ahead(
        &repo,
        "src/lib.rs",
        "// lib\n// upstream\n",
        "feat: upstream edit",
    );

    let stderr = repo.maw_fails(&[
        "ws",
        "merge",
        "alice",
        "--destroy",
        "--message",
        "merge alice",
    ]);

    assert!(
        stderr.contains("diverged from the current epoch"),
        "should keep legacy diverged error.\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("Affected workspace(s):") && stderr.contains("alice"),
        "diverged error should list the affected workspace.\nstderr: {stderr}"
    );
}

#[test]
fn fork_divergence_is_not_absorbed() {
    let repo = TestRepo::new();
    let _ = repo.seed_files(&[("src/lib.rs", "// lib\n")]);

    // No in-flight workspaces. The blocker here is structural: epoch and
    // branch have unique commits, so absorbing would lose the epoch's work.
    force_branch_to_divergent(&repo, "fork.txt", "fork\n", "fork: divergent commit");

    // Create a clean workspace so we have something to merge.
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("alice", "alice.txt", "alice's work\n");

    let stderr = repo.maw_fails(&[
        "ws",
        "merge",
        "alice",
        "--destroy",
        "--message",
        "merge alice",
    ]);

    assert!(
        stderr.contains("diverged from the current epoch"),
        "fork divergence should still trigger legacy error.\nstderr: {stderr}"
    );
    // Fork divergence should NOT report any affected workspaces — the
    // ancestor relationship rules it out before the workspace check runs.
    assert!(
        !stderr.contains("Affected workspace(s):"),
        "fork divergence is not a candidate for absorb; affected list is irrelevant.\nstderr: {stderr}"
    );
}

#[test]
fn ff_absorb_succeeds_when_no_other_workspaces_exist() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "// lib\n"), ("docs/README.md", "# README\n")]);

    // Branch advances; no in-flight workspace exists yet, so safety predicate
    // is trivially satisfied. We then create alice and merge.
    push_branch_ahead(&repo, "docs/README.md", "# README v2\n", "docs: bump");

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("alice", "alice.txt", "alice\n");

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "alice",
        "--destroy",
        "--message",
        "merge alice",
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    assert!(
        out.status.success(),
        "merge should succeed.\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("Absorbed"),
        "absorb should be announced.\nstderr: {stderr}"
    );
    assert_eq!(
        repo.read_file("default", "docs/README.md").as_deref(),
        Some("# README v2\n")
    );
}

#[test]
fn ff_absorb_handles_multiple_commits_in_range() {
    let repo = TestRepo::new();
    repo.seed_files(&[
        ("src/lib.rs", "// lib\n"),
        ("docs/README.md", "# README\n"),
        ("docs/HOWTO.md", "# HOWTO\n"),
    ]);

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.modify_file("alice", "src/lib.rs", "// lib\n// alice\n");

    // Two commits in the FF range, both on docs only.
    push_two_commits_ahead(
        &repo,
        &[
            ("docs/README.md", "# README v2\n"),
            ("docs/HOWTO.md", "# HOWTO v2\n"),
        ],
        &["docs: update README", "docs: update HOWTO"],
    );

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "alice",
        "--destroy",
        "--message",
        "merge alice",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    assert!(
        out.status.success(),
        "merge should succeed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("Absorbed 2 upstream commit"),
        "should announce two absorbed commits.\nstderr: {stderr}"
    );
}

#[test]
fn opt_out_disables_absorb() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "// lib\n"), ("docs/README.md", "# README\n")]);

    // Disable auto-absorb in the manifold config.
    let cfg_path = repo.root().join(".manifold").join("config.toml");
    std::fs::write(
        &cfg_path,
        "[repo]\nbranch = \"main\"\n\n[merge]\nauto_absorb_ff = false\n",
    )
    .expect("write config.toml");

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("alice", "alice.txt", "alice\n");

    push_branch_ahead(&repo, "docs/README.md", "# README v2\n", "docs: bump");

    let stderr = repo.maw_fails(&[
        "ws",
        "merge",
        "alice",
        "--destroy",
        "--message",
        "merge alice",
    ]);

    assert!(
        stderr.contains("diverged from the current epoch"),
        "opt-out should restore legacy diverged error.\nstderr: {stderr}"
    );
    // No in-flight workspace touches docs/README.md, but with auto-absorb
    // off we don't even compute the affected list.
    assert!(
        !stderr.contains("Affected workspace(s):"),
        "opt-out should not surface the affected list.\nstderr: {stderr}"
    );
}

/// Advance `refs/heads/main` AND default's HEAD to a new commit, leaving
/// default's worktree at the new tip — i.e. the direct-commit shape the
/// real bn-28q2 regression reported. Returns the OID of the new branch tip.
fn push_branch_ahead_keeping_default(
    repo: &TestRepo,
    file_path: &str,
    content: &str,
    message: &str,
) -> String {
    let ws_default = repo.default_workspace();
    let epoch_before = repo.current_epoch();
    let main_before = git_ok(repo.root(), &["rev-parse", "refs/heads/main"])
        .trim()
        .to_owned();
    assert_eq!(
        epoch_before, main_before,
        "test setup expects epoch and main to match before push_branch_ahead_keeping_default"
    );

    std::fs::write(ws_default.join(file_path), content)
        .unwrap_or_else(|e| panic!("write {file_path}: {e}"));
    git_ok(&ws_default, &["add", "-A"]);
    git_ok(&ws_default, &["commit", "-m", message]);

    let new_oid = git_ok(&ws_default, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    git_ok(repo.root(), &["update-ref", "refs/heads/main", &new_oid]);
    // NOTE: default is NOT reset; HEAD stays at new_oid. This is the bn-28q2
    // regression shape: target_touched.paths == ff_paths.
    new_oid
}

#[test]
fn ff_absorb_succeeds_when_only_target_committed_directly() {
    // bn-28q2 regression: direct commit on default with no dirty edits
    // elsewhere previously self-blocked because default's touched paths
    // tautologically equalled the FF range.
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "// lib\n"), ("docs/README.md", "# README\n")]);

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.modify_file("alice", "src/lib.rs", "// lib\n// alice's tweak\n");

    push_branch_ahead_keeping_default(
        &repo,
        "docs/README.md",
        "# README\n\nupdated\n",
        "docs: update",
    );

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "alice",
        "--destroy",
        "--message",
        "merge alice",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    assert!(
        out.status.success(),
        "merge should succeed despite default holding the FF commit.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("Absorbed") && stderr.contains("upstream commit"),
        "stderr should announce absorb.\nstderr: {stderr}"
    );
}

#[test]
fn ff_absorb_blocks_when_target_has_dirty_edits_to_ff_path() {
    // Target's UNCOMMITTED edits to an FF-range path would be clobbered by
    // the post-absorb worktree checkout — predicate must still block.
    let repo = TestRepo::new();
    repo.seed_files(&[("docs/README.md", "# README\n")]);

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.add_file("alice", "alice.txt", "alice\n");

    push_branch_ahead_keeping_default(
        &repo,
        "docs/README.md",
        "# README\n\nupdated\n",
        "docs: update",
    );

    // Now make a DIRTY edit to docs/README.md in default, on top of the
    // committed FF tip. The FF range touches docs/README.md too (that's how
    // it got there), so the predicate must catch this and refuse to absorb.
    let ws_default = repo.default_workspace();
    std::fs::write(
        ws_default.join("docs/README.md"),
        "# README\n\nupdated\n\nlocal scratch\n",
    )
    .expect("write dirty edit");

    let stderr = repo.maw_fails(&[
        "ws",
        "merge",
        "alice",
        "--destroy",
        "--message",
        "merge alice",
    ]);

    assert!(
        stderr.contains("diverged from the current epoch"),
        "dirty-on-FF path should fall back to legacy diverged error.\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("Affected workspace(s):") && stderr.contains("default"),
        "should list default as the blocking workspace.\nstderr: {stderr}"
    );
}
