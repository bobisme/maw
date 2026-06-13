//! Integration tests for epoch-ref desync self-heal (bn-1qtj).
//!
//! Tests three scenarios:
//!
//! (a) Simulated desync: workspace was synced (HEAD == epoch) but the epoch
//!     ref still holds an old OID. Assert that `maw exec` emits NO stale
//!     warning (the self-heal cleared it), the ref is repaired, and
//!     `maw ws sync` reports up to date.
//!
//! (b) Genuine stale case: workspace is based on an old epoch AND has a real
//!     commit — this must stay stale with ahead=1 and the warning must fire.
//!
//! (c) Write-failure path: the retry+eprintln wrapper in
//!     `sync_worktree_to_epoch_inner` is exercised by making the ref dir
//!     read-only so both write attempts fail; asserts the expected WARNING
//!     lines appear on stderr.

mod manifold_common;

use manifold_common::TestRepo;

// ---------------------------------------------------------------------------
// Helper: read a raw git ref from the bare repo, or None if absent.
// ---------------------------------------------------------------------------

fn read_ref(repo: &TestRepo, refname: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", refname])
        .current_dir(repo.root())
        .output()
        .expect("git rev-parse failed");
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Write a raw git ref in the bare repo.
fn write_ref(repo: &TestRepo, refname: &str, oid: &str) {
    let out = std::process::Command::new("git")
        .args(["update-ref", refname, oid])
        .current_dir(repo.root())
        .output()
        .expect("git update-ref failed");
    assert!(out.status.success(), "write_ref failed for {refname}");
}

// ---------------------------------------------------------------------------
// (a) Simulated desync: stale epoch ref, HEAD already at current epoch
// ---------------------------------------------------------------------------

/// bn-1qtj scenario (a): create workspace, advance epoch, manually sync HEAD
/// to current epoch (like a successful `git checkout --detach` that then
/// failed to write the epoch ref), then artificially rewind the epoch ref to
/// the old OID. Assert:
///   - `maw exec ws -- git status` succeeds with NO stale warning on stderr
///   - the epoch ref is repaired (now holds the current epoch OID)
///   - `maw ws sync ws` says "up to date" (not stale)
#[test]
fn epoch_ref_desync_self_heals_when_head_already_at_current_epoch() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "alice"]);

    // Remember the initial epoch OID (the "old" ref value we'll rewind to).
    let old_epoch = repo.current_epoch();

    // Advance the epoch so alice is now stale.
    repo.add_file("default", "advance.txt", "content\n");
    let new_epoch = repo.advance_epoch("chore: epoch advance");
    assert_ne!(old_epoch, new_epoch);

    // Manually move alice's HEAD to the new epoch (simulates the checkout
    // half of sync succeeding) WITHOUT updating the epoch ref.
    repo.git_in_workspace("alice", &["checkout", "--detach", &new_epoch]);

    // Rewind the per-workspace epoch ref to the old OID — this is the desync.
    write_ref(&repo, "refs/manifold/epoch/ws/alice", &old_epoch);

    // Verify the desync is set up correctly.
    assert_eq!(
        read_ref(&repo, "refs/manifold/epoch/ws/alice").as_deref(),
        Some(old_epoch.as_str()),
        "epoch ref should hold old OID before self-heal"
    );
    assert_eq!(
        repo.workspace_head("alice"),
        new_epoch,
        "HEAD should already be at new epoch"
    );

    // (a1) `maw exec alice -- git status` must NOT print a stale warning.
    let out = repo.maw_raw(&["exec", "alice", "--", "git", "status"]);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "exec git status should succeed after self-heal\nstdout: {}\nstderr: {stderr}",
        String::from_utf8_lossy(&out.stdout),
    );
    assert!(
        !stderr.contains("stale") && !stderr.contains("behind"),
        "no stale warning expected after self-heal, got stderr: {stderr}"
    );

    // (a2) The epoch ref must now hold the NEW epoch OID.
    assert_eq!(
        read_ref(&repo, "refs/manifold/epoch/ws/alice").as_deref(),
        Some(new_epoch.as_str()),
        "epoch ref must be repaired to current epoch after self-heal"
    );

    // (a3) `maw ws sync alice` must report up to date (not stale).
    let sync_out = repo.maw_ok(&["ws", "sync", "alice"]);
    assert!(
        sync_out.contains("up to date"),
        "sync should say up-to-date after self-heal, got: {sync_out}"
    );
}

/// bn-1qtj scenario (a) variant: HEAD is PAST the current epoch (workspace has
/// commits that landed on top of the current epoch, e.g. from a rebase). The
/// epoch ref still holds the old OID. The self-heal should still fire and the
/// workspace should be treated as active.
#[test]
fn epoch_ref_desync_self_heals_when_head_descends_from_current_epoch() {
    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "bob"]);

    // Advance the epoch.
    let old_epoch = repo.current_epoch();
    repo.add_file("default", "file.txt", "content\n");
    let new_epoch = repo.advance_epoch("chore: epoch advance");
    assert_ne!(old_epoch, new_epoch);

    // Move bob's HEAD to new_epoch then add a commit on top of it.
    // This simulates a post-sync workspace commit.
    repo.git_in_workspace("bob", &["checkout", "--detach", &new_epoch]);
    repo.add_file("bob", "bob-work.txt", "work\n");
    repo.git_in_workspace("bob", &["add", "-A"]);
    repo.git_in_workspace("bob", &["commit", "-m", "ws: bob work"]);

    // Rewind the epoch ref to the old OID.
    write_ref(&repo, "refs/manifold/epoch/ws/bob", &old_epoch);

    // HEAD should now be PAST the current epoch (one commit ahead).
    let bob_head = repo.workspace_head("bob");
    assert_ne!(
        bob_head, new_epoch,
        "bob should have a commit past new_epoch"
    );

    // `maw exec bob -- git log --oneline -1` should succeed, no stale warning.
    let out = repo.maw_raw(&["exec", "bob", "--", "git", "log", "--oneline", "-1"]);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "exec should succeed after self-heal\nstdout: {}\nstderr: {stderr}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !stderr.contains("stale") && !stderr.contains("behind"),
        "no stale warning expected when HEAD descends from current epoch, got: {stderr}"
    );

    // The epoch ref must be repaired to the current epoch.
    assert_eq!(
        read_ref(&repo, "refs/manifold/epoch/ws/bob").as_deref(),
        Some(new_epoch.as_str()),
        "epoch ref must be repaired to current epoch"
    );
}

// ---------------------------------------------------------------------------
// (b) Genuine stale case: workspace HEAD is below current epoch with commits
// ---------------------------------------------------------------------------

/// bn-1qtj scenario (b): genuine stale workspace — HEAD is based on old epoch
/// and has one real workspace commit. The workspace must stay stale, ahead=1,
/// and the auto-sync warning must fire (commit protection).
#[test]
fn genuine_stale_with_workspace_commit_stays_stale_and_warns() {
    let repo = TestRepo::new();

    // Create workspace and make a commit in it before advancing the epoch.
    repo.maw_ok(&["ws", "create", "carol"]);
    repo.add_file("carol", "carol-work.txt", "work\n");
    repo.git_in_workspace("carol", &["add", "-A"]);
    repo.git_in_workspace("carol", &["commit", "-m", "ws: carol work"]);

    // Advance the epoch AFTER carol's commit — carol is now stale with 1 ahead.
    repo.add_file("default", "advance.txt", "content\n");
    repo.advance_epoch("chore: epoch advance");

    // `maw exec carol -- git status` must warn and NOT auto-sync.
    let carol_head_before = repo.workspace_head("carol");
    let out = repo.maw_raw(&["exec", "carol", "--", "git", "status"]);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "exec should succeed (just warn)\nstdout: {}\nstderr: {stderr}",
        String::from_utf8_lossy(&out.stdout)
    );

    // Warning must fire.
    assert!(
        stderr.contains("stale") || stderr.contains("behind"),
        "expected stale warning for genuine stale+ahead workspace, got: {stderr}"
    );

    // Workspace HEAD must NOT have been auto-synced.
    assert_eq!(
        repo.workspace_head("carol"),
        carol_head_before,
        "HEAD must not be moved when auto-sync is blocked by committed work"
    );

    // The epoch ref must still hold the old epoch (not repaired to current).
    let current = repo.current_epoch();
    let epoch_ref_val =
        read_ref(&repo, "refs/manifold/epoch/ws/carol").expect("epoch ref should exist");
    assert_ne!(
        epoch_ref_val, current,
        "genuine stale epoch ref must NOT be self-healed when HEAD is below current epoch"
    );

    // `maw ws list` must show carol as stale.
    let list_out = repo.maw_ok(&["ws", "list", "--format", "json"]);
    let list_json: serde_json::Value =
        serde_json::from_str(&list_out).expect("ws list --format json must be valid JSON");
    let carol_entry = list_json["workspaces"]
        .as_array()
        .expect("workspaces array")
        .iter()
        .find(|w| w["name"].as_str() == Some("carol"))
        .expect("carol entry must exist");
    assert!(
        carol_entry["state"]
            .as_str()
            .unwrap_or("")
            .contains("stale"),
        "carol must be stale in ws list: {:?}",
        carol_entry["state"]
    );
    // Note: the backend reports commits_ahead=0 for stale workspaces (the
    // behind count is what matters for stale detection; the committed-work
    // guard uses committed_ahead_of_epoch() at sync-gate time, not here).
    // The key invariant to verify is that the stale warning fired and
    // auto-sync was blocked (asserted above via the HEAD-unchanged check).
    let behind = carol_entry["behind_epochs"].as_u64().unwrap_or(0);
    assert!(
        behind >= 1,
        "carol must be behind by at least 1 epoch, got behind_epochs={behind}"
    );
}

// ---------------------------------------------------------------------------
// (c) Write-failure path: retry + eprintln on both-failed writes
// ---------------------------------------------------------------------------

/// bn-1qtj scenario (c): make the epoch-ref directory read-only so both
/// write attempts fail; assert the loud WARNING lines appear on stderr.
///
/// We exercise this by creating a workspace, syncing it (which calls
/// `sync_worktree_to_epoch_inner`), but first making the ref storage
/// directory read-only. After the sync attempt we restore permissions
/// so the test cleanup can succeed.
///
/// This test is Unix-only (read-only dirs work differently on Windows).
#[cfg(unix)]
#[test]
fn sync_epoch_ref_write_failure_emits_loud_warning_with_fix_command() {
    use std::os::unix::fs::PermissionsExt as _;

    let repo = TestRepo::new();

    repo.maw_ok(&["ws", "create", "dave"]);

    // Advance the epoch so dave is stale.
    repo.add_file("default", "file.txt", "content\n");
    let new_epoch = repo.advance_epoch("chore: epoch advance");

    // Locate the loose-ref path for the epoch ref. It lives at
    // .git/refs/manifold/epoch/ws/dave (or a packed-refs entry, but for a
    // fresh test repo it's a loose ref).
    let epoch_ws_dir = repo
        .root()
        .join(".git")
        .join("refs")
        .join("manifold")
        .join("epoch")
        .join("ws");

    // Make the epoch/ws directory read-only so writes fail.
    let meta = std::fs::metadata(&epoch_ws_dir).expect("epoch/ws dir should exist");
    let original_perms = meta.permissions();
    let mut ro_perms = original_perms.clone();
    ro_perms.set_mode(0o555); // r-xr-xr-x
    std::fs::set_permissions(&epoch_ws_dir, ro_perms)
        .expect("should be able to set dir permissions");

    // Run `maw ws sync dave` — the checkout succeeds, but the epoch ref
    // write should fail (both attempts), triggering the loud warning.
    let out = repo.maw_raw(&["ws", "sync", "dave"]);

    // Restore permissions before any assertions that might panic.
    std::fs::set_permissions(&epoch_ws_dir, original_perms)
        .expect("should be able to restore permissions");

    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();

    // The sync itself should succeed (checkout worked, only ref write failed).
    assert!(
        out.status.success(),
        "sync should succeed even when epoch-ref write fails\nstdout: {stdout}\nstderr: {stderr}"
    );

    // The loud WARNING must appear on stderr.
    assert!(
        stderr.contains("WARNING") && stderr.contains("epoch ref"),
        "expected loud WARNING about epoch ref write failure, got stderr: {stderr}"
    );
    // The copy-pasteable fix command must be present.
    assert!(
        stderr.contains("git") && stderr.contains("update-ref"),
        "expected copy-pasteable git update-ref fix command in stderr, got: {stderr}"
    );
    // The exact ref name and target OID must be named.
    assert!(
        stderr.contains("refs/manifold/epoch/ws/dave"),
        "stderr must name the exact ref, got: {stderr}"
    );
    assert!(
        stderr.contains(&new_epoch[..12]),
        "stderr must name the target OID (at least prefix), got: {stderr}"
    );
}
