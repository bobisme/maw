//! bn-21cj + bn-8zqz: one source of truth for workspace conflict state.
//!
//! bn-21cj: `maw ws sync <ws>` printed "Rebase complete: 1 commit(s)
//! replayed cleanly" while the workspace HEAD actually contained a committed
//! whole-file structured conflict (a quiet sibling auto-rebase had committed
//! the marker blob + sidecars; a later manual sync replayed that commit onto
//! a newer epoch as ordinary content, so the replay run itself saw no
//! conflicts). Worse, the "clean run" branch deleted the legacy sidecar
//! while placeholder blobs were still in HEAD.
//!
//! bn-8zqz: after an agent MANUALLY resolved committed conflict markers and
//! committed the resolution, the three readers disagreed: `ws conflicts`
//! said clean (merge engine), `merge --check` blocked (raw sidecar), and
//! `resolve --list` agreed with the blocker — while the file had zero
//! markers. Only an extra `maw ws sync` (and only on a non-stale workspace)
//! cleared the stale metadata.
//!
//! The fix: all readers consult `workspace::conflict_state` — sidecars
//! verified against reality (markers on recorded paths, placeholder blobs in
//! HEAD), stale metadata auto-cleared on read, and the post-replay sync
//! summary keyed off the same helper so it always matches `resolve --list`.

mod manifold_common;

use manifold_common::TestRepo;

/// Drive a quiet sibling auto-rebase that commits a structured conflict into
/// workspace `b` (placeholder blob in HEAD + both sidecars written):
/// `a` and `b` edit the same line; merging `a` advances the epoch and
/// auto-rebases `b` into a recorded conflict.
fn setup_committed_conflict_via_auto_rebase(repo: &TestRepo) {
    repo.seed_files(&[("shared.txt", "line1\nshared\nline3\n")]);

    repo.maw_ok(&["ws", "create", "a"]);
    repo.maw_ok(&["ws", "create", "b"]);
    repo.add_file("a", "shared.txt", "line1\nFROM_A\nline3\n");
    repo.git_in_workspace("a", &["commit", "-aqm", "a-change"]);
    repo.add_file("b", "shared.txt", "line1\nFROM_B\nline3\n");
    repo.git_in_workspace("b", &["commit", "-aqm", "b-change"]);

    repo.maw_ok(&[
        "ws",
        "merge",
        "a",
        "--into",
        "default",
        "--message",
        "merge a",
    ]);

    // Auto-rebase must have recorded the conflict for `b`.
    let sidecar = repo
        .read_conflict_tree_sidecar("b")
        .expect("auto-rebase should write conflict-tree.json for 'b'");
    let conflicts = sidecar
        .get("conflicts")
        .and_then(|v| v.as_object())
        .expect("sidecar should have a conflicts object");
    assert!(
        !conflicts.is_empty(),
        "precondition: auto-rebase must record a conflict for 'b'"
    );
}

/// Advance the epoch past `b` without touching it (third workspace edits an
/// unrelated file; auto-rebase disabled so `b` keeps its committed marker
/// blob and goes stale).
fn advance_epoch_without_touching_b(repo: &TestRepo) {
    repo.maw_ok(&["ws", "create", "c"]);
    repo.add_file("c", "other.txt", "unrelated\n");
    repo.git_in_workspace("c", &["add", "-A"]);
    repo.git_in_workspace("c", &["commit", "-qm", "c-change"]);
    repo.maw_ok(&[
        "ws",
        "merge",
        "c",
        "--into",
        "default",
        "--destroy",
        "--no-auto-rebase",
        "--message",
        "merge c",
    ]);
}

// ---------------------------------------------------------------------------
// (a) bn-21cj: replaying committed conflict content must not claim "cleanly"
// ---------------------------------------------------------------------------

#[test]
fn sync_replaying_committed_conflict_does_not_claim_cleanly() {
    let repo = TestRepo::new();
    setup_committed_conflict_via_auto_rebase(&repo);
    advance_epoch_without_touching_b(&repo);

    // Manual sync replays b's marker-laden commit onto the newer epoch.
    // The replay run itself sees no NEW conflicts — the old code printed
    // "replayed cleanly" and deleted the legacy sidecar here.
    let sync = repo.maw_raw(&["ws", "sync", "b"]);
    assert!(
        sync.status.success(),
        "sync should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&sync.stdout),
        String::from_utf8_lossy(&sync.stderr)
    );
    let stdout = String::from_utf8_lossy(&sync.stdout);

    assert!(
        !stdout.contains("replayed cleanly"),
        "sync must NOT say 'replayed cleanly' while committed conflict \
         content sits in HEAD; got:\n{stdout}"
    );
    assert!(
        stdout.contains("unresolved conflict"),
        "sync summary must surface the residual committed conflict; got:\n{stdout}"
    );
    assert!(
        stdout.contains("shared.txt"),
        "sync summary must name the conflicted path; got:\n{stdout}"
    );
    assert!(
        stdout.contains("maw ws resolve b"),
        "sync summary must print the same resolve guidance as the conflicted \
         branch; got:\n{stdout}"
    );

    // The sidecar must be preserved — deleting it would orphan the
    // placeholder blobs in HEAD (bn-28d1 territory).
    assert!(
        repo.read_conflict_tree_sidecar("b").is_some(),
        "structured sidecar must survive the sync"
    );
    let legacy = repo
        .root()
        .join(".manifold/artifacts/ws/b/rebase-conflicts.json");
    assert!(
        legacy.exists(),
        "legacy sidecar must survive the sync (old code deleted it here)"
    );

    // The summary must agree with `resolve --list` ...
    let resolve = repo.maw_ok(&["ws", "resolve", "b", "--list"]);
    assert!(
        resolve.contains("shared.txt"),
        "resolve --list must still show the conflict after sync; got:\n{resolve}"
    );

    // ... and with the merge gate.
    let check = repo.maw_raw(&["ws", "merge", "b", "--into", "default", "--check"]);
    assert!(
        !check.status.success(),
        "merge --check must still block while the conflict is unresolved"
    );
}

// ---------------------------------------------------------------------------
// (b) bn-8zqz: manual resolution commit → all readers agree, no extra sync
// ---------------------------------------------------------------------------

#[test]
fn manual_resolution_commit_unblocks_all_readers_without_sync() {
    let repo = TestRepo::new();
    setup_committed_conflict_via_auto_rebase(&repo);

    // Agent manually resolves the markers and commits — leaving the sidecar
    // stale. NO `maw ws sync` follows.
    let shared = repo.root().join("ws/b/shared.txt");
    std::fs::write(&shared, "line1\nRESOLVED\nline3\n").expect("write resolved content");
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace("b", &["commit", "-qm", "manual: resolve conflict"]);

    // 1. `ws conflicts` must report clean (and clear the stale metadata).
    let conflicts = repo.maw_raw(&["ws", "conflicts", "b"]);
    assert!(
        conflicts.status.success(),
        "ws conflicts should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&conflicts.stdout),
        String::from_utf8_lossy(&conflicts.stderr)
    );
    let conflicts_stdout = String::from_utf8_lossy(&conflicts.stdout);
    assert!(
        conflicts_stdout.contains("No conflicts found"),
        "ws conflicts must report clean after a manual resolution commit; \
         got:\n{conflicts_stdout}"
    );

    // The stale sidecar must be gone after the first read — no `ws sync`
    // required.
    assert!(
        repo.read_conflict_tree_sidecar("b").is_none(),
        "stale sidecar must be auto-cleared by the first reader"
    );

    // 2. `merge --check` must agree (this used to block on the raw sidecar).
    let check = repo.maw_ok(&["ws", "merge", "b", "--into", "default", "--check"]);
    assert!(
        check.contains("Ready to merge"),
        "merge --check must agree the workspace is clean; got:\n{check}"
    );

    // 3. `resolve --list` must agree.
    let resolve = repo.maw_ok(&["ws", "resolve", "b", "--list"]);
    assert!(
        resolve.contains("No conflicted files"),
        "resolve --list must agree the workspace is clean; got:\n{resolve}"
    );

    // 4. And the actual merge must proceed.
    let merge = repo.maw_raw(&[
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
        merge.status.success(),
        "merge must proceed after manual resolution\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&merge.stdout),
        String::from_utf8_lossy(&merge.stderr)
    );

    // The resolved content must have landed.
    let merged = std::fs::read_to_string(repo.root().join("ws/default/shared.txt"))
        .expect("merged file readable");
    assert!(
        merged.contains("RESOLVED"),
        "manual resolution must be what merges; got:\n{merged}"
    );
}

#[test]
fn merge_check_alone_clears_stale_sidecar_and_proceeds() {
    // Same as above but `merge --check` is the FIRST reader — the auto-clear
    // must not depend on running `ws conflicts` (or anything else) first.
    let repo = TestRepo::new();
    setup_committed_conflict_via_auto_rebase(&repo);

    let shared = repo.root().join("ws/b/shared.txt");
    std::fs::write(&shared, "line1\nRESOLVED\nline3\n").expect("write resolved content");
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace("b", &["commit", "-qm", "manual: resolve conflict"]);

    let check = repo.maw_ok(&["ws", "merge", "b", "--into", "default", "--check"]);
    assert!(
        check.contains("Ready to merge"),
        "merge --check run first must clear the stale sidecar and report \
         ready; got:\n{check}"
    );
    assert!(
        repo.read_conflict_tree_sidecar("b").is_none(),
        "stale sidecar must be cleared by merge --check itself"
    );

    let merge = repo.maw_raw(&[
        "ws",
        "merge",
        "b",
        "--into",
        "default",
        "--destroy",
        "--message",
        "merge b",
    ]);
    assert!(
        merge.status.success(),
        "merge must proceed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&merge.stdout),
        String::from_utf8_lossy(&merge.stderr)
    );
}

#[test]
fn sync_clears_stale_sidecar_even_when_workspace_is_stale() {
    // The old clearing helper only ran when the workspace was NOT stale.
    // A manual resolution commit on a STALE workspace must still clear the
    // stale sidecar (and the subsequent replay is genuinely clean).
    let repo = TestRepo::new();
    setup_committed_conflict_via_auto_rebase(&repo);

    // Manual resolution commit in b.
    let shared = repo.root().join("ws/b/shared.txt");
    std::fs::write(&shared, "line1\nRESOLVED\nline3\n").expect("write resolved content");
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace("b", &["commit", "-qm", "manual: resolve conflict"]);

    // Epoch advances; b is now stale AND carries a stale sidecar.
    advance_epoch_without_touching_b(&repo);

    let sync = repo.maw_raw(&["ws", "sync", "b"]);
    assert!(
        sync.status.success(),
        "sync should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&sync.stdout),
        String::from_utf8_lossy(&sync.stderr)
    );
    let stdout = String::from_utf8_lossy(&sync.stdout);
    assert!(
        stdout.contains("Cleared stale conflict metadata"),
        "sync on a STALE workspace must still clear the stale sidecar; \
         got:\n{stdout}"
    );
    assert!(
        repo.read_conflict_tree_sidecar("b").is_none(),
        "stale sidecar must be cleared even when the workspace is stale"
    );

    // After the (genuinely clean) replay the workspace must be mergeable.
    let check = repo.maw_ok(&["ws", "merge", "b", "--into", "default", "--check"]);
    assert!(
        check.contains("Ready to merge"),
        "after sync, the workspace must be mergeable; got:\n{check}"
    );
}

// ---------------------------------------------------------------------------
// (c) placeholder blob in HEAD + deleted sidecar: still blocked, readers agree
// ---------------------------------------------------------------------------

#[test]
fn placeholder_blob_with_deleted_sidecar_blocks_and_readers_agree() {
    let repo = TestRepo::new();
    setup_committed_conflict_via_auto_rebase(&repo);

    // Tamper: delete BOTH sidecars while the placeholder blob is in HEAD.
    let sidecar_dir = repo.root().join(".manifold/artifacts/ws/b");
    let _ = std::fs::remove_file(sidecar_dir.join("conflict-tree.json"));
    let _ = std::fs::remove_file(sidecar_dir.join("rebase-conflicts.json"));
    assert!(repo.read_conflict_tree_sidecar("b").is_none());

    // The merge gate must refuse (bn-28d1 tripwire, not bypassable).
    let merge = repo.maw_raw(&[
        "ws",
        "merge",
        "b",
        "--into",
        "default",
        "--destroy",
        "--force",
        "--message",
        "tampered",
    ]);
    assert!(
        !merge.status.success(),
        "merge must refuse placeholder blobs in HEAD even with --force and \
         no sidecar\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&merge.stdout),
        String::from_utf8_lossy(&merge.stderr)
    );

    // `ws conflicts` must agree (it used to consult a different source).
    let conflicts = repo.maw_raw(&["ws", "conflicts", "b"]);
    assert!(
        !conflicts.status.success(),
        "ws conflicts must flag the placeholder blob\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&conflicts.stdout),
        String::from_utf8_lossy(&conflicts.stderr)
    );
    let conflicts_out = format!(
        "{}{}",
        String::from_utf8_lossy(&conflicts.stdout),
        String::from_utf8_lossy(&conflicts.stderr)
    );
    assert!(
        conflicts_out.contains("shared.txt"),
        "ws conflicts must name the tainted path; got:\n{conflicts_out}"
    );

    // `resolve --list` must agree instead of claiming "no conflicts".
    let resolve = repo.maw_ok(&["ws", "resolve", "b", "--list"]);
    assert!(
        resolve.contains("shared.txt") && resolve.contains("placeholder"),
        "resolve --list must surface the placeholder-blob conflict; \
         got:\n{resolve}"
    );
}
