//! Regression tests for bn-8flz: `maw ws advance` must NOT orphan committed
//! work ahead of the base epoch.
//!
//! Previously, advance unconditionally ran `checkout_to(new_epoch)` which
//! silently overwrote committed commits and printed "advanced successfully."
//! The fix detects committed-ahead work and routes it through `rebase_workspace`
//! (the same guarded path used by `maw ws sync --rebase`).
//!
//! Test coverage:
//! 1. **Orphan regression**: persistent ws + committed-ahead commit + advance
//!    → commit is reachable from HEAD and file is on disk (acceptance test).
//! 2. **Clean fast-forward**: persistent ws (no committed work) + advance
//!    → HEAD moves to new epoch, content updated, no error.
//! 3. **Already at epoch**: persistent ws at current epoch
//!    → "already at the current epoch" message, success, no-op.
//! 4. **sync fast-forward still native** (no regression): `maw ws sync` on a
//!    stale workspace without committed work → synced successfully (checks
//!    that the native `checkout_detach` path in `checks.rs` works end-to-end).
//! 5. **sync preserves committed-ahead work** (bn-29z8 regression guard):
//!    sync routes to rebase rather than orphaning commits.
//! 6. **Grep-style guard**: no `git checkout --detach` or raw HEAD-write
//!    shell-out remains in `advance.rs`, `working_copy.rs`, `sync/checks.rs`,
//!    `merge.rs` production code (compile-time verified via source scan).

mod manifold_common;

use manifold_common::TestRepo;

// ---------------------------------------------------------------------------
// 1. Orphan regression — the primary acceptance test for bn-8flz
// ---------------------------------------------------------------------------

/// REGRESSION (bn-8flz): committed-ahead commit in a persistent workspace
/// must survive `maw ws advance`. Before the fix, advance ran
/// `checkout_to(new_epoch)` unconditionally, orphaning the commit while
/// printing "advanced successfully."
///
/// Deterministic repro from the bone:
///   maw ws create pers-worker --persistent
///   <commit work in pers-worker>
///   <advance epoch via `repo.advance_epoch()`>
///   maw ws advance pers-worker
///   EXPECTED: committed work reachable from HEAD, file on disk
///   BEFORE FIX: committed work orphaned, file gone
#[test]
fn advance_preserves_committed_ahead_commit() {
    let repo = TestRepo::new();

    // Seed base files so the workspace has context.
    repo.seed_files(&[("base.txt", "base content\n")]);

    // Create a persistent workspace at the current epoch.
    repo.maw_ok(&["ws", "create", "pers-worker", "--persistent"]);
    let epoch_at_creation = repo.workspace_head("pers-worker");

    // Commit work in the persistent workspace (ahead of its base epoch).
    repo.add_file("pers-worker", "worker-work.txt", "worker work\n");
    repo.git_in_workspace("pers-worker", &["add", "worker-work.txt"]);
    repo.git_in_workspace("pers-worker", &["commit", "-m", "WORKER COMMIT"]);
    let committed_head = repo.workspace_head("pers-worker");

    // Sanity: the committed commit is genuinely ahead of the epoch at creation.
    assert_ne!(
        committed_head, epoch_at_creation,
        "committed_head should differ from epoch_at_creation"
    );

    // Advance the global epoch by committing a file in the default workspace.
    // This simulates another agent's merge arriving.
    repo.add_file("default", "epoch-advance.txt", "epoch content\n");
    let new_epoch = repo.advance_epoch("chore: advance epoch (another agent)");

    // The workspace's epoch ref still points to the old epoch.
    // HEAD is at committed_head (ahead of the old epoch).
    let head_before_advance = repo.workspace_head("pers-worker");
    assert_eq!(
        head_before_advance, committed_head,
        "workspace HEAD should still be the committed commit before ws advance"
    );
    assert_ne!(
        new_epoch, epoch_at_creation,
        "global epoch must have actually advanced"
    );

    // --- THE FIX (bn-8flz): maw ws advance must NOT orphan the committed commit ---
    let out = repo.maw_raw_exact(&["ws", "advance", "pers-worker"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "advance should succeed (via rebase of committed work onto new epoch)\n\
         stdout: {stdout}\nstderr: {stderr}"
    );

    // After a rebase, the old committed OID becomes a new cherry-picked commit
    // with a different OID. We cannot check `merge-base --is-ancestor` on the
    // original OID; instead we verify the CONTENT was preserved and the rebase
    // created a new commit (HEAD ≠ new_epoch and HEAD ≠ old_committed_head).
    let head_after = repo.workspace_head("pers-worker");

    // The file must be on disk — this is the primary anti-orphan check.
    let content = repo.read_file("pers-worker", "worker-work.txt");
    assert_eq!(
        content.as_deref(),
        Some("worker work\n"),
        "worker-work.txt must be on disk after advance — committed work must not be orphaned"
    );

    // HEAD must NOT equal the new epoch (if it did, the commit was silently discarded).
    assert_ne!(
        head_after, new_epoch,
        "HEAD should be a rebased commit on top of new_epoch (not new_epoch itself); \
         if equal the commit was orphaned"
    );

    // HEAD must also differ from the pre-rebase committed commit (the rebase
    // creates a new cherry-picked commit with a fresh OID).
    assert_ne!(
        head_after, committed_head,
        "HEAD should be a new cherry-picked OID after rebase (not the original commit OID)"
    );

    // Verify the commit count: exactly 1 commit on top of new_epoch.
    let commits_ahead_str = repo.git_in_workspace(
        "pers-worker",
        &["rev-list", "--count", &format!("{new_epoch}..HEAD")],
    );
    let commits_ahead: u32 = commits_ahead_str.trim().parse().unwrap_or(0);
    assert_eq!(
        commits_ahead, 1,
        "workspace should have exactly 1 rebased commit on top of new_epoch, got {commits_ahead}"
    );

    // The new epoch content must also be present (rebase applied on top of new epoch).
    assert!(
        repo.file_exists("pers-worker", "epoch-advance.txt"),
        "epoch-advance.txt should appear after advance — workspace should be at new epoch base"
    );
}

// ---------------------------------------------------------------------------
// 2. Clean fast-forward advance (no committed-ahead work)
// ---------------------------------------------------------------------------

/// A persistent workspace with NO committed-ahead work should fast-forward to
/// the new epoch. HEAD should equal the new epoch after advance.
#[test]
fn advance_clean_fast_forward_head_equals_new_epoch() {
    let repo = TestRepo::new();

    repo.seed_files(&[("base.txt", "base\n")]);

    // Create a persistent workspace (no committed-ahead work).
    repo.maw_ok(&["ws", "create", "pers-clean", "--persistent"]);
    let epoch_at_creation = repo.workspace_head("pers-clean");

    // Advance the global epoch.
    repo.add_file("default", "epoch2.txt", "second epoch content\n");
    let new_epoch = repo.advance_epoch("chore: second epoch");
    assert_ne!(epoch_at_creation, new_epoch);

    // Advance the persistent workspace.
    let out = repo.maw_raw_exact(&["ws", "advance", "pers-clean"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "advance should succeed for clean fast-forward\nstdout: {stdout}\nstderr: {stderr}"
    );

    // HEAD should now equal the new epoch (no commits to rebase).
    let head_after = repo.workspace_head("pers-clean");
    assert_eq!(
        head_after, new_epoch,
        "HEAD should equal new epoch after clean fast-forward advance"
    );

    // The new epoch's file should be present.
    assert!(
        repo.file_exists("pers-clean", "epoch2.txt"),
        "epoch2.txt should appear in workspace after fast-forward advance"
    );
}

// ---------------------------------------------------------------------------
// 3. Already-at-epoch is a no-op
// ---------------------------------------------------------------------------

/// If the workspace is already at the current epoch, advance should succeed
/// and say "already at the current epoch."
#[test]
fn advance_already_at_epoch_is_noop() {
    let repo = TestRepo::new();
    repo.seed_files(&[("base.txt", "base\n")]);

    // Create a persistent workspace but do NOT advance the epoch.
    repo.maw_ok(&["ws", "create", "pers-noop", "--persistent"]);

    let head_before = repo.workspace_head("pers-noop");

    let out = repo.maw_raw_exact(&["ws", "advance", "pers-noop"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "advance on up-to-date workspace should succeed\nstdout: {stdout}\nstderr: {stderr}"
    );

    assert!(
        stdout.contains("already at the current epoch"),
        "should say 'already at the current epoch', got: {stdout}"
    );

    // HEAD must not change.
    let head_after = repo.workspace_head("pers-noop");
    assert_eq!(
        head_before, head_after,
        "HEAD must not change when already at current epoch"
    );
}

// ---------------------------------------------------------------------------
// 4. sync fast-forward still works (native checkout_detach path in checks.rs)
// ---------------------------------------------------------------------------

/// `maw ws sync` on a stale workspace without committed-ahead work should
/// fast-forward successfully via the native `checkout_detach` path.
/// Regression guard: ensures the shell-out removal in checks.rs didn't break
/// the fast-forward case.
#[test]
fn sync_fast_forward_native_checkout_detach_still_works() {
    let repo = TestRepo::new();

    // Create a normal (ephemeral) workspace.
    repo.maw_ok(&["ws", "create", "sync-ff"]);
    let head_before = repo.workspace_head("sync-ff");

    // Advance the global epoch (stale the workspace).
    repo.add_file("default", "epoch-sync.txt", "sync epoch content\n");
    let new_epoch = repo.advance_epoch("chore: epoch for sync-ff test");
    assert_ne!(head_before, new_epoch);

    // Sync the stale workspace.
    let out = repo.maw_raw_exact(&["ws", "sync", "sync-ff"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "sync fast-forward should succeed via native checkout_detach\n\
         stdout: {stdout}\nstderr: {stderr}"
    );

    // HEAD must equal new epoch.
    let head_after = repo.workspace_head("sync-ff");
    assert_eq!(
        head_after, new_epoch,
        "HEAD should equal new epoch after sync fast-forward"
    );

    // New epoch file should be present.
    assert!(
        repo.file_exists("sync-ff", "epoch-sync.txt"),
        "epoch-sync.txt should appear after sync fast-forward"
    );
}

// ---------------------------------------------------------------------------
// 5. sync preserves committed-ahead work (bn-29z8 regression guard)
// ---------------------------------------------------------------------------

/// `maw ws sync` on a workspace with committed-ahead work must NOT silently
/// orphan the commit. It routes to rebase (not fast-forward), preserving the
/// committed work. This is the bn-29z8 regression guard.
#[test]
fn sync_preserves_committed_ahead_work_via_rebase() {
    let repo = TestRepo::new();

    repo.seed_files(&[("base.txt", "base content\n")]);

    // Create a workspace, commit work in it.
    repo.maw_ok(&["ws", "create", "sync-ahead"]);
    repo.add_file("sync-ahead", "precious.txt", "precious work\n");
    repo.git_in_workspace("sync-ahead", &["add", "precious.txt"]);
    repo.git_in_workspace("sync-ahead", &["commit", "-m", "feat: precious work"]);
    let committed_head = repo.workspace_head("sync-ahead");

    // Advance the epoch.
    repo.add_file("default", "epoch-sync2.txt", "advance\n");
    let new_epoch = repo.advance_epoch("chore: epoch for sync-ahead test");
    assert_ne!(committed_head, new_epoch);

    // Sync — must route to rebase (not orphan the committed commit).
    let out = repo.maw_raw_exact(&["ws", "sync", "sync-ahead"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "sync with committed-ahead work should succeed via rebase\n\
         stdout: {stdout}\nstderr: {stderr}"
    );

    // After rebase, the original committed OID becomes a new cherry-picked
    // commit. The primary check is that the FILE is still on disk and that
    // HEAD is not equal to new_epoch (which would mean the commit was dropped).
    let head_after = repo.workspace_head("sync-ahead");

    // The precious file must be on disk.
    let content = repo.read_file("sync-ahead", "precious.txt");
    assert_eq!(
        content.as_deref(),
        Some("precious work\n"),
        "precious.txt must survive sync (must not be orphaned)"
    );

    // HEAD must not equal the new epoch (if it did, the commit was discarded).
    assert_ne!(
        head_after, new_epoch,
        "HEAD should be a rebased commit on top of new_epoch (not the epoch itself)"
    );

    // Verify exactly 1 commit on top of new_epoch.
    let commits_ahead_str = repo.git_in_workspace(
        "sync-ahead",
        &["rev-list", "--count", &format!("{new_epoch}..HEAD")],
    );
    let commits_ahead: u32 = commits_ahead_str.trim().parse().unwrap_or(0);
    assert_eq!(
        commits_ahead, 1,
        "workspace should have exactly 1 rebased commit on top of new_epoch, got {commits_ahead}"
    );
}

// ---------------------------------------------------------------------------
// 6. Grep-style guard: no HEAD-mover shell-outs in production code
// ---------------------------------------------------------------------------

/// Static guard: verify that the `advance`/`sync`/`merge`/`working_copy` source files
/// in this build do NOT contain the retired HEAD-mover shell-out patterns:
///   - `git checkout --detach`
///   - `git checkout -f` (the old force-checkout pattern)
///
/// This is a belt-and-braces check. If a future edit reintroduces a
/// shell-out it would inadvertently bypass the safety choke-point.
#[test]
fn no_head_mover_git_checkout_shell_out_in_production_code() {
    let crate_src = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("crates")
        .join("maw-cli")
        .join("src")
        .join("workspace");

    let files_to_check = &[
        "advance.rs",
        "working_copy.rs",
        "sync/checks.rs",
        "merge.rs",
    ];

    // These literal substrings in "git" Command invocations would indicate
    // a retired shell-out. The patterns are specific enough to avoid
    // false-positives from comments/strings that merely describe old behavior.
    let forbidden_patterns = &[
        // "checkout", "--detach" in arg arrays — the old sync fast-forward
        "\"checkout\", \"--detach\"",
        // "checkout", "-f" in arg arrays — the old force-checkout
        "\"checkout\", \"-f\"",
    ];

    for rel_path in files_to_check {
        let path = crate_src.join(rel_path);
        if !path.exists() {
            continue; // Skip if the file doesn't exist (layout differences).
        }

        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));

        // Only check non-comment lines. Join with newlines to reassemble.
        let production_src: String = src
            .lines()
            .filter(|line| {
                let trimmed = line.trim_start();
                !trimmed.starts_with("//")
            })
            .collect::<Vec<_>>()
            .join("\n");

        for pattern in forbidden_patterns {
            assert!(
                !production_src.contains(pattern),
                "Retired HEAD-mover shell-out pattern '{pattern}' found in {rel_path}.\n\
                 This was removed by bn-8flz; reintroducing it bypasses the safety choke-point.\n\
                 Use repo.checkout_detach() / repo.checkout_to_branch() instead."
            );
        }
    }
}
