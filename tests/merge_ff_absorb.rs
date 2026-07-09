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

/// bn-1huu / bn-3eew: `maw ws merge --check` must surface out-of-maw commits
/// on trunk (the epoch ref lagging the branch tip) instead of a silent "[OK]
/// Ready to merge". When the drift is a safe fast-forward that no in-flight
/// workspace's touched paths overlap (`ff-absorbable`), the real merge
/// already auto-absorbs it (bn-11ip) — so `--check` must say so
/// *informationally*, with no scolding and no `maw epoch sync` command,
/// instead of the old blanket warning.
#[test]
fn check_surfaces_ff_absorbable_trunk_drift_informationally() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "// lib\n"), ("docs/README.md", "# README\n")]);

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.modify_file("alice", "src/lib.rs", "// lib\n// alice\n");

    // Baseline: epoch == branch tip → ready, no divergence note.
    let clean = repo.maw_ok(&["ws", "merge", "alice", "--into", "default", "--check"]);
    assert!(
        clean.contains("[OK] Ready to merge"),
        "clean check:\n{clean}"
    );
    assert!(
        !clean.contains("not made through maw") && !clean.contains("ahead of the epoch"),
        "no divergence note when epoch == tip:\n{clean}"
    );

    // Two direct (out-of-maw) commits on trunk, touching paths alice never
    // touches. Epoch now lags by 2, but the FF range is safe to absorb.
    push_two_commits_ahead(
        &repo,
        &[
            ("docs/README.md", "# README\n\nupd1\n"),
            ("docs/CHANGES.md", "changes\n"),
        ],
        &["docs: upd1", "docs: changes"],
    );

    // Still mergeable. The NOTE surfaces the drift but stays informational:
    // no "not made through maw" scolding, no `maw epoch sync` advice.
    let text = repo.maw_ok(&["ws", "merge", "alice", "--into", "default", "--check"]);
    assert!(
        text.contains("[OK] Ready to merge"),
        "ff-absorbable check:\n{text}"
    );
    assert!(
        text.contains("trunk is 2 commits ahead of the epoch")
            && text.contains("will be absorbed automatically when you merge"),
        "should use the informational ff-absorbable wording:\n{text}"
    );
    assert!(
        !text.contains("not made through maw"),
        "ff-absorbable NOTE must not use the old scolding wording:\n{text}"
    );
    assert!(
        !text.contains("maw epoch sync"),
        "ff-absorbable NOTE must not advise `maw epoch sync` — it's automatic:\n{text}"
    );

    // JSON surfaces trunk_ahead and the drift_classification for machine
    // consumers (pretty-printed).
    let json = repo.maw_ok(&[
        "ws", "merge", "alice", "--into", "default", "--check", "--format", "json",
    ]);
    let compact: String = json.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        compact.contains("\"trunk_ahead\":2"),
        "JSON should carry trunk_ahead=2:\n{json}"
    );
    assert!(
        compact.contains("\"drift_classification\":\"ff-absorbable\""),
        "JSON should classify the drift as ff-absorbable:\n{json}"
    );
}

/// bn-3eew: when the FF range on trunk overlaps a path an in-flight
/// workspace has touched (`ff-blocked`), `--check` must keep a real warning
/// — auto-advancing the epoch here would silently move the diff3 base under
/// that workspace — and point at `maw epoch sync` / `maw doctor --repair`.
#[test]
fn check_surfaces_ff_blocked_trunk_drift_as_a_warning() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "// lib\n")]);

    // Alice edits the same file the branch will advance out-of-maw.
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.modify_file("alice", "src/lib.rs", "// lib\n// alice\n");

    // Direct commit on main touches src/lib.rs — overlaps with alice, so the
    // FF range can't be safely auto-advanced while alice is in flight.
    push_branch_ahead(
        &repo,
        "src/lib.rs",
        "// lib\n// upstream\n",
        "feat: upstream edit",
    );

    let text = repo.maw_ok(&["ws", "merge", "alice", "--into", "default", "--check"]);
    assert!(
        text.contains("WARNING: trunk is 1 commit ahead of the epoch"),
        "ff-blocked check should warn, not just note:\n{text}"
    );
    assert!(
        text.contains("in-flight workspace touches the same paths"),
        "should name why auto-advance is unsafe:\n{text}"
    );
    assert!(
        text.contains("maw epoch sync") || text.contains("maw doctor --repair"),
        "should point at a reconcile command:\n{text}"
    );
    assert!(
        !text.contains("will be absorbed automatically"),
        "ff-blocked must not use the informational ff-absorbable wording:\n{text}"
    );

    let json = repo.maw_ok(&[
        "ws", "merge", "alice", "--into", "default", "--check", "--format", "json",
    ]);
    let compact: String = json.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        compact.contains("\"trunk_ahead\":1"),
        "JSON should carry trunk_ahead=1:\n{json}"
    );
    assert!(
        compact.contains("\"drift_classification\":\"ff-blocked\""),
        "JSON should classify the drift as ff-blocked:\n{json}"
    );
}

// ---------------------------------------------------------------------------
// bn-rah2: sibling HEAD preservation during FF absorb
// ---------------------------------------------------------------------------

/// bn-rah2 post-merge sibling invariant. Encodes the Prime Invariant ("no
/// committed work is ever lost") for a sibling workspace that sat adjacent to
/// a merge which absorbed out-of-maw trunk commits:
///
/// * `clean_before => clean_after` — a sibling that was clean must stay clean
///   (the field bug left the work as untracked/uncommitted cruft).
/// * every file the sibling had **committed** before is still committed
///   (byte-identical) in its post-merge HEAD tree — whether its HEAD stayed
///   put or was rebased onto the advanced epoch.
fn assert_sibling_work_preserved(
    repo: &TestRepo,
    name: &str,
    clean_before: bool,
    committed_files: &[(&str, &str)],
) {
    if clean_before {
        let status = repo.git_in_workspace(name, &["status", "--short"]);
        assert!(
            status.trim().is_empty(),
            "sibling '{name}' was clean before the merge but is dirty after \
             (work leaked out of its commit):\n{status}"
        );
    }
    for (path, content) in committed_files {
        let got = repo.git_in_workspace(name, &["show", &format!("HEAD:{path}")]);
        assert_eq!(
            &got, content,
            "sibling '{name}': committed file '{path}' not preserved byte-identically \
             in its post-merge HEAD tree"
        );
    }
}

/// Regression test for bn-rah2: merging workspace A with --destroy when the
/// branch has direct commits ahead of epoch must REPLAY sibling workspace B's
/// committed work onto the absorbed epoch — never raw-reset its HEAD. Before
/// the fix, the FF-absorb wrote the branch OID straight to every sibling's
/// HEAD file, orphaning their committed work and leaving it as untracked
/// worktree cruft (`maw ws sync` then hard-failed with "uncommitted changes").
#[test]
fn ff_absorb_replays_committed_ahead_sibling() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "// lib\n"), ("docs/README.md", "# README\n")]);

    // Two workspaces with committed work on disjoint files, both one commit
    // ahead of the same epoch.
    repo.maw_ok(&["ws", "create", "alice"]);
    repo.maw_ok(&["ws", "create", "bob"]);

    repo.add_file("alice", "src/alice.rs", "// alice\n");
    repo.git_in_workspace("alice", &["add", "-A"]);
    repo.git_in_workspace("alice", &["commit", "-m", "feat: alice's work"]);

    repo.add_file("bob", "src/bob.rs", "// bob\n");
    repo.git_in_workspace("bob", &["add", "-A"]);
    repo.git_in_workspace("bob", &["commit", "-m", "feat: bob's work"]);
    let bob_head_before = repo
        .git_in_workspace("bob", &["rev-parse", "HEAD"])
        .trim()
        .to_owned();

    // Push the branch ahead with a docs-only commit (disjoint from both
    // workspaces): the FF-absorb scenario.
    push_branch_ahead(
        &repo,
        "docs/README.md",
        "# README\n\nupdated by direct commit\n",
        "docs: direct commit outside maw",
    );

    let bob_clean_before = repo
        .git_in_workspace("bob", &["status", "--short"])
        .trim()
        .is_empty();
    assert!(bob_clean_before, "bob should be clean before merge");

    // Merge alice with --destroy. The FF absorb runs first, then the merge.
    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "alice",
        "--destroy",
        "--message",
        "feat: merge alice",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "merge should succeed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // The absorb emits a per-sibling NOTE naming bob's replay (loudness).
    assert!(
        stderr.contains("bob: replayed"),
        "absorb output should name bob's replay.\nstderr: {stderr}"
    );

    let bob_head_after = repo
        .git_in_workspace("bob", &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    let base_epoch = repo.epoch0().to_owned();

    // bob was REBASED: HEAD moved to a new commit (not the orphaned original,
    // not the base epoch, not the absorbed branch tip).
    assert_ne!(
        bob_head_after, base_epoch,
        "bob's HEAD must not sit at epoch₀ — that would orphan his work"
    );
    assert_ne!(
        bob_head_after, bob_head_before,
        "bob's HEAD should have advanced to a rebased commit onto the absorbed epoch"
    );

    // Prime Invariant: bob's work survives as COMMITTED content, clean tree.
    assert_sibling_work_preserved(
        &repo,
        "bob",
        bob_clean_before,
        &[("src/bob.rs", "// bob\n")],
    );

    // And bob now sits on top of the integrated epoch (alice's work is visible
    // in his HEAD tree — proof he was rebased forward, not merely left alone).
    assert_eq!(
        repo.git_in_workspace("bob", &["show", "HEAD:src/alice.rs"]),
        "// alice\n",
        "bob should be rebased onto the epoch that already integrated alice's work"
    );

    // The very next protocol step (`maw ws sync bob`) must report up to date,
    // NOT the pre-fix "has uncommitted changes that would be lost by sync".
    let sync = repo.maw_raw(&["ws", "sync", "bob"]);
    let sync_out = format!(
        "{}{}",
        String::from_utf8_lossy(&sync.stdout),
        String::from_utf8_lossy(&sync.stderr)
    );
    assert!(
        sync.status.success() && sync_out.contains("up to date"),
        "`maw ws sync bob` should report up to date after an adjacent absorb.\n{sync_out}"
    );
}

/// A sibling that is DIRTY-only (uncommitted edit on a path disjoint from the
/// FF range, HEAD still at its base epoch) must still absorb cleanly: the FF
/// materializes the absorbed paths and preserves the uncommitted edit. This
/// is the control the committed-ahead replay must not regress.
#[test]
fn ff_absorb_preserves_dirty_only_sibling_edits() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "// lib\n"), ("docs/README.md", "# README\n")]);

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.maw_ok(&["ws", "create", "bob"]);

    repo.add_file("alice", "src/alice.rs", "// alice\n");
    repo.git_in_workspace("alice", &["add", "-A"]);
    repo.git_in_workspace("alice", &["commit", "-m", "feat: alice's work"]);

    // bob has an UNCOMMITTED edit on a disjoint path (no commit → HEAD stays
    // at the base epoch).
    repo.add_file("bob", "src/bob_scratch.rs", "// bob scratch WIP\n");
    let bob_dirty_before = repo.git_in_workspace("bob", &["status", "--short"]);
    assert!(
        bob_dirty_before.contains("bob_scratch.rs"),
        "bob should be dirty before merge: {bob_dirty_before}"
    );

    push_branch_ahead(
        &repo,
        "docs/README.md",
        "# README\n\nupdated by direct commit\n",
        "docs: direct commit outside maw",
    );

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "alice",
        "--destroy",
        "--message",
        "feat: merge alice",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "merge should succeed with a dirty-only sibling.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // bob's uncommitted edit must survive intact.
    assert_eq!(
        repo.read_file("bob", "src/bob_scratch.rs").as_deref(),
        Some("// bob scratch WIP\n"),
        "bob's uncommitted edit must be preserved across the absorb"
    );
    assert!(
        repo.git_in_workspace("bob", &["status", "--short"])
            .contains("bob_scratch.rs"),
        "bob's edit should still be uncommitted after the absorb"
    );
}

/// A sibling that is BOTH committed-ahead AND dirty cannot be replayed (rebase
/// refuses a dirty worktree). The absorb must block the whole merge with the
/// epoch-sync guidance — and leave the sibling's HEAD and the epoch untouched
/// (no half-moved state).
#[test]
fn ff_absorb_blocks_on_committed_ahead_dirty_sibling() {
    let repo = TestRepo::new();
    repo.seed_files(&[("src/lib.rs", "// lib\n"), ("docs/README.md", "# README\n")]);

    repo.maw_ok(&["ws", "create", "alice"]);
    repo.maw_ok(&["ws", "create", "bob"]);

    repo.add_file("alice", "src/alice.rs", "// alice\n");
    repo.git_in_workspace("alice", &["add", "-A"]);
    repo.git_in_workspace("alice", &["commit", "-m", "feat: alice's work"]);

    // bob: committed work AND an additional uncommitted edit (both disjoint
    // from the FF docs path).
    repo.add_file("bob", "src/bob.rs", "// bob\n");
    repo.git_in_workspace("bob", &["add", "-A"]);
    repo.git_in_workspace("bob", &["commit", "-m", "feat: bob's work"]);
    repo.add_file("bob", "src/bob_scratch.rs", "// bob scratch WIP\n");
    let bob_head_before = repo
        .git_in_workspace("bob", &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    let epoch_before = repo.current_epoch();

    push_branch_ahead(
        &repo,
        "docs/README.md",
        "# README\n\nupdated by direct commit\n",
        "docs: direct commit outside maw",
    );

    let out = repo.maw_raw(&[
        "ws",
        "merge",
        "alice",
        "--destroy",
        "--message",
        "feat: merge alice",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let combined = format!("{stdout}{stderr}");
    assert!(
        !out.status.success(),
        "merge must be blocked by a committed-ahead + dirty sibling.\n{combined}"
    );
    assert!(
        combined.contains("diverged") && combined.contains("bob"),
        "block message should surface divergence and name bob.\n{combined}"
    );

    // No half-moved state: bob's HEAD and the epoch are untouched.
    let bob_head_after = repo
        .git_in_workspace("bob", &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    assert_eq!(
        bob_head_after, bob_head_before,
        "a blocked absorb must not move bob's HEAD"
    );
    assert_eq!(
        repo.current_epoch(),
        epoch_before,
        "a blocked absorb must not advance the epoch"
    );
}
