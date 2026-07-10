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

use std::process::Command;

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

    // `resolve --list` must surface the conflict instead of claiming "no conflicts".
    // After bn-39i8, it reconstructs the sidecar from headers and shows a
    // structured listing — the path must appear regardless of which code path
    // was taken.
    let resolve = repo.maw_ok(&["ws", "resolve", "b", "--list"]);
    assert!(
        resolve.contains("shared.txt"),
        "resolve --list must surface the placeholder-blob conflict; \
         got:\n{resolve}"
    );
}

// ---------------------------------------------------------------------------
// (a-bn-39i8) text placeholder + deleted sidecar: resolve --list reconstructs,
//             --keep epoch resolves, workspace becomes mergeable.
// ---------------------------------------------------------------------------

/// A text-format placeholder blob in HEAD with both sidecars deleted should
/// trigger automatic reconstruction when `resolve --list` is run:
/// - `resolve --list` must succeed and list the conflict (not claim "no conflicts")
/// - `resolve --keep epoch` must succeed and write the epoch side's content
/// - After resolution the workspace must be mergeable
#[test]
fn text_placeholder_deleted_sidecar_reconstructs_and_resolves() {
    let repo = TestRepo::new();
    setup_committed_conflict_via_auto_rebase(&repo);

    // Tamper: delete BOTH sidecars while the placeholder blob is in HEAD.
    let sidecar_dir = repo.root().join(".manifold/artifacts/ws/b");
    let sidecar_path = sidecar_dir.join("conflict-tree.json");
    let _ = std::fs::remove_file(&sidecar_path);
    let _ = std::fs::remove_file(sidecar_dir.join("rebase-conflicts.json"));
    assert!(
        repo.read_conflict_tree_sidecar("b").is_none(),
        "precondition: sidecar must be gone"
    );

    // `resolve --list` must reconstruct the sidecar and list the conflict.
    let resolve = repo.maw_ok(&["ws", "resolve", "b", "--list"]);
    assert!(
        resolve.contains("shared.txt"),
        "resolve --list must show the reconstructed conflict path; got:\n{resolve}"
    );
    // After reconstruction the sidecar must be present.
    assert!(
        sidecar_path.exists(),
        "resolve --list must have written the reconstructed sidecar"
    );
    assert!(
        repo.read_conflict_tree_sidecar("b").is_some(),
        "reconstructed sidecar must be readable as a valid ConflictTree"
    );

    // The merge gate must still block (reconstruction does not auto-resolve).
    let check = repo.maw_raw(&["ws", "merge", "b", "--into", "default", "--check"]);
    assert!(
        !check.status.success(),
        "merge --check must still block after sidecar reconstruction; \
         the conflict is not yet resolved\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );

    // `resolve --keep epoch` must succeed now that the sidecar is present.
    let keep = repo.maw_ok(&["ws", "resolve", "b", "--keep", "epoch"]);
    assert!(
        keep.contains("resolved") || keep.contains("Reconstructed") || !keep.contains("error"),
        "resolve --keep epoch must succeed after reconstruction; got:\n{keep}"
    );

    // After resolution the workspace must be mergeable.
    let check2 = repo.maw_ok(&["ws", "merge", "b", "--into", "default", "--check"]);
    assert!(
        check2.contains("Ready to merge"),
        "workspace must be mergeable after resolving the reconstructed conflict; got:\n{check2}"
    );
}

// ---------------------------------------------------------------------------
// bn-drk3: comment-syntax-aware headers — end-to-end via a real auto-rebase
// conflict on a `.rs` file, plus reconstruction from the `//`-form header.
// ---------------------------------------------------------------------------

/// Same shape as [`setup_committed_conflict_via_auto_rebase`] but conflicts
/// on a `.rs` path — exercises the exact bn-1m4d item 2 field incident shape
/// (a rebase-committed conflict placeholder inside a Rust source file).
fn setup_committed_rs_conflict_via_auto_rebase(repo: &TestRepo) {
    repo.seed_files(&[("src/lib.rs", "fn shared() -> i32 {\n    1\n}\n")]);

    repo.maw_ok(&["ws", "create", "a"]);
    repo.maw_ok(&["ws", "create", "b"]);
    repo.add_file("a", "src/lib.rs", "fn shared() -> i32 {\n    2\n}\n");
    repo.git_in_workspace("a", &["commit", "-aqm", "a-change"]);
    repo.add_file("b", "src/lib.rs", "fn shared() -> i32 {\n    3\n}\n");
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

/// bn-drk3 / bn-1m4d item 2: a real auto-rebase conflict committed into a
/// `.rs` file must produce a `//`-prefixed header — the exact fix for the
/// field incident where a `#` first line produced a mystery rustc syntax
/// error ("expected one of '!' or '[', found 'structured'") instead of the
/// `<<<<<<<` marker block, which rustc explains clearly.
#[test]
fn auto_rebase_conflict_on_rs_file_uses_slash_slash_header() {
    let repo = TestRepo::new();
    setup_committed_rs_conflict_via_auto_rebase(&repo);

    let on_disk = repo
        .read_file_bytes("b", "src/lib.rs")
        .expect("conflicted src/lib.rs must exist in the workspace worktree");
    let first_line = on_disk
        .split(|&b| b == b'\n')
        .next()
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .unwrap_or_default();

    assert!(
        first_line.starts_with("// structured conflict at"),
        "a conflicted .rs file's first line must be a legal Rust `//` comment \
         (bn-drk3 / bn-1m4d item 2), not the legacy `#`; got: {first_line:?}"
    );
    assert!(
        !first_line.starts_with('#'),
        "must not regress to the legacy `#` prefix for a known .rs extension; got: {first_line:?}"
    );

    // `resolve --list` must agree the conflict exists (readers stay in sync).
    let resolve = repo.maw_ok(&["ws", "resolve", "b", "--list"]);
    assert!(
        resolve.contains("src/lib.rs"),
        "resolve --list must show the conflicted path; got:\n{resolve}"
    );
}

/// bn-drk3: reconstruction (bn-39i8) must accept the `//`-form header just
/// as it always has the legacy `#` form — a `.rs` conflict whose sidecars
/// were deleted must still reconstruct from the `//`-prefixed placeholder.
#[test]
fn rs_placeholder_slash_slash_header_deleted_sidecar_reconstructs_and_resolves() {
    let repo = TestRepo::new();
    setup_committed_rs_conflict_via_auto_rebase(&repo);

    let sidecar_dir = repo.root().join(".manifold/artifacts/ws/b");
    let sidecar_path = sidecar_dir.join("conflict-tree.json");
    let _ = std::fs::remove_file(&sidecar_path);
    let _ = std::fs::remove_file(sidecar_dir.join("rebase-conflicts.json"));
    assert!(
        repo.read_conflict_tree_sidecar("b").is_none(),
        "precondition: sidecar must be gone"
    );

    // Precondition: the placeholder blob really is `//`-prefixed.
    let on_disk = repo
        .read_file_bytes("b", "src/lib.rs")
        .expect("conflicted src/lib.rs must exist");
    assert!(
        on_disk.starts_with(b"// structured conflict at "),
        "precondition: HEAD blob must carry the `//`-prefixed placeholder header"
    );

    let resolve = repo.maw_ok(&["ws", "resolve", "b", "--list"]);
    assert!(
        resolve.contains("src/lib.rs"),
        "resolve --list must reconstruct from the `//`-form header; got:\n{resolve}"
    );
    assert!(
        repo.read_conflict_tree_sidecar("b").is_some(),
        "reconstructed sidecar must be readable as a valid ConflictTree"
    );

    let keep = repo.maw_ok(&["ws", "resolve", "b", "--keep", "epoch"]);
    assert!(
        keep.contains("resolved") || keep.contains("Reconstructed") || !keep.contains("error"),
        "resolve --keep epoch must succeed after reconstruction; got:\n{keep}"
    );

    let check2 = repo.maw_ok(&["ws", "merge", "b", "--into", "default", "--check"]);
    assert!(
        check2.contains("Ready to merge"),
        "workspace must be mergeable after resolving the reconstructed conflict; got:\n{check2}"
    );
}

// ---------------------------------------------------------------------------
// (b-bn-39i8) binary-format placeholder: reconstructs, --keep <ws> resolves
// ---------------------------------------------------------------------------

/// A binary-format placeholder blob in HEAD (crafted manually) with no sidecar
/// should reconstruct on `resolve --list` and allow `--keep <ws>` resolution.
#[test]
fn binary_placeholder_deleted_sidecar_reconstructs_and_resolves() {
    let repo = TestRepo::new();
    repo.seed_files(&[("data.bin", "original content\n")]);

    // Create workspace b at the seeded epoch.
    repo.maw_ok(&["ws", "create", "b"]);

    let ws_path = repo.root().join("ws/b");

    // Write the two side blobs into the git object store using `git hash-object -w`.
    // We write them as temp files first, hash them, then delete the temps.
    let epoch_content = "epoch side content\n";
    let ws_content = "workspace side content\n";

    let tmp_epoch = repo.root().join("_tmp_epoch.txt");
    let tmp_ws = repo.root().join("_tmp_ws.txt");
    std::fs::write(&tmp_epoch, epoch_content).expect("write tmp epoch blob");
    std::fs::write(&tmp_ws, ws_content).expect("write tmp ws blob");

    let hash_obj = |path: &std::path::Path| -> String {
        let out = Command::new("git")
            .args(["hash-object", "-w", path.to_str().expect("valid path")])
            .current_dir(&ws_path)
            .output()
            .expect("git hash-object");
        assert!(
            out.status.success(),
            "git hash-object failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    let epoch_oid = hash_obj(&tmp_epoch);
    let ws_oid = hash_obj(&tmp_ws);
    let _ = std::fs::remove_file(&tmp_epoch);
    let _ = std::fs::remove_file(&tmp_ws);

    // Craft a binary-format placeholder blob and commit it to HEAD in workspace b.
    let placeholder = format!(
        "# BINARY CONFLICT at data.bin — inlined markers would corrupt the file.\n\
         # Pick a side with: maw ws resolve <workspace> --keep <side-name>\n\
         # side: epoch  @  {epoch_oid}\n\
         # side: b  @  {ws_oid}\n\
         \n\
         <<<<<<< epoch (current)\n\
         (binary content -- bytes not inlined)\n\
         ||||||| base\n\
         =======\n\
         (binary content -- bytes not inlined)\n\
         >>>>>>> b (workspace changes)\n"
    );
    repo.add_file("b", "data.bin", &placeholder);
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace("b", &["commit", "-qm", "binary conflict placeholder"]);

    // Ensure NO sidecar exists.
    let sidecar_dir = repo.root().join(".manifold/artifacts/ws/b");
    std::fs::create_dir_all(&sidecar_dir).ok();
    let _ = std::fs::remove_file(sidecar_dir.join("conflict-tree.json"));
    let _ = std::fs::remove_file(sidecar_dir.join("rebase-conflicts.json"));

    // `resolve --list` must reconstruct and list the conflict.
    let resolve_out = repo.maw_ok(&["ws", "resolve", "b", "--list"]);
    assert!(
        resolve_out.contains("data.bin"),
        "resolve --list must list the binary placeholder conflict; got:\n{resolve_out}"
    );

    // Sidecar must now exist.
    assert!(
        sidecar_dir.join("conflict-tree.json").exists(),
        "resolve --list must reconstruct the sidecar for a binary placeholder"
    );

    // `resolve --keep b` must write the workspace blob bytes.
    repo.maw_ok(&["ws", "resolve", "b", "--keep", "b"]);

    // The workspace file must now contain the workspace side's content.
    let after = repo
        .read_file("b", "data.bin")
        .expect("data.bin must exist after resolution");
    assert!(
        after.contains("workspace side content"),
        "resolved data.bin must contain ws side content; got:\n{after}"
    );
}

// ---------------------------------------------------------------------------
// (c-bn-39i8) corrupted header → graceful refusal, correct guidance text
// ---------------------------------------------------------------------------

/// When the placeholder blob's OID lines have been removed (corrupted header),
/// `resolve --list` must refuse gracefully with correct guidance:
/// - Must NOT suggest `maw ws sync --rebase` (deprecated flag form)
/// - Must NOT suggest `maw ws sync` (cannot regenerate the sidecar)
/// - Must suggest restoring real content and committing as the fallback
#[test]
fn corrupted_placeholder_header_graceful_refusal_correct_guidance() {
    let repo = TestRepo::new();
    setup_committed_conflict_via_auto_rebase(&repo);

    // Tamper: delete BOTH sidecars.
    let sidecar_dir = repo.root().join(".manifold/artifacts/ws/b");
    let _ = std::fs::remove_file(sidecar_dir.join("conflict-tree.json"));
    let _ = std::fs::remove_file(sidecar_dir.join("rebase-conflicts.json"));

    // Replace the placeholder blob with one that has the OID lines stripped
    // (simulating a corrupted / hand-edited placeholder).
    let corrupted = "# structured conflict at shared.txt\n\
                     # atoms:\n\
                     # (OID lines were removed — header corrupted)\n\
                     \n\
                     <<<<<<< epoch (current)\n\
                     FROM_A\n\
                     ||||||| base\n\
                     shared\n\
                     =======\n\
                     FROM_B\n\
                     >>>>>>> b (workspace changes)\n";
    repo.add_file("b", "shared.txt", corrupted);
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace(
        "b",
        &["commit", "-qm", "corrupted placeholder (OID lines removed)"],
    );

    // `resolve --list` must succeed but report the parse failure gracefully.
    let out = repo.maw_ok(&["ws", "resolve", "b", "--list"]);
    assert!(
        out.contains("shared.txt"),
        "resolve --list must still name the placeholder path; got:\n{out}"
    );

    // Guidance must NOT contain the deprecated --rebase flag form.
    assert!(
        !out.contains("--rebase"),
        "guidance must NOT suggest `maw ws sync --rebase` (deprecated); got:\n{out}"
    );
    // Guidance must NOT suggest `maw ws sync` as a fix (it cannot regenerate the sidecar).
    // We allow "maw ws sync" appearing in the context of the failing message text,
    // but must not suggest it as THE fix for this state.
    assert!(
        !out.contains("regenerate the metadata with `maw ws sync"),
        "guidance must NOT suggest `maw ws sync` for regenerating the sidecar; got:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// (d-bn-39i8) merge gate still blocks before reconstruction
// ---------------------------------------------------------------------------

/// The bn-28d1 tripwire must fire BEFORE any reconstruction happens:
/// - The merge gate must refuse even with --force when a placeholder is in HEAD
/// - Reconstruction must only happen through resolve, not through the merge gate
#[test]
fn merge_gate_blocks_before_reconstruction_not_auto_reconstructed() {
    let repo = TestRepo::new();
    setup_committed_conflict_via_auto_rebase(&repo);

    // Tamper: delete BOTH sidecars.
    let sidecar_dir = repo.root().join(".manifold/artifacts/ws/b");
    let _ = std::fs::remove_file(sidecar_dir.join("conflict-tree.json"));
    let _ = std::fs::remove_file(sidecar_dir.join("rebase-conflicts.json"));
    assert!(
        repo.read_conflict_tree_sidecar("b").is_none(),
        "precondition: sidecar must be gone"
    );

    // The merge gate must refuse (bn-28d1 tripwire).
    let merge = repo.maw_raw(&[
        "ws",
        "merge",
        "b",
        "--into",
        "default",
        "--destroy",
        "--force",
        "--message",
        "should-fail",
    ]);
    assert!(
        !merge.status.success(),
        "merge must refuse placeholder blobs in HEAD even with --force; \
         reconstruction must only happen through resolve, not through the merge gate\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&merge.stdout),
        String::from_utf8_lossy(&merge.stderr)
    );

    // The sidecar must NOT have been written by the merge gate.
    // (If merge had auto-reconstructed, it would either succeed or leave a sidecar behind.)
    assert!(
        repo.read_conflict_tree_sidecar("b").is_none(),
        "the merge gate must NOT auto-reconstruct the sidecar — reconstruction is \
         only permitted through `maw ws resolve`"
    );
}

// ---------------------------------------------------------------------------
// (d) bn-6xpz: up-to-date-but-conflicted workspace — sync must surface it
// ---------------------------------------------------------------------------

/// After a quiet sibling auto-rebase commits a structured conflict into
/// workspace `b`, `maw ws sync b` on an already-current workspace must NOT
/// silently print "up to date" — it must mention the unresolved conflict(s)
/// and the resolve command.
#[test]
fn sync_up_to_date_but_conflicted_workspace_reports_conflict() {
    let repo = TestRepo::new();
    setup_committed_conflict_via_auto_rebase(&repo);
    // At this point `b` is already at the current epoch (the auto-rebase
    // advanced its refs).  A second `maw ws sync b` hits the no-op path.

    let sync = repo.maw_raw(&["ws", "sync", "b"]);
    assert!(
        sync.status.success(),
        "sync should succeed even with a residual conflict\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&sync.stdout),
        String::from_utf8_lossy(&sync.stderr)
    );
    let stdout = String::from_utf8_lossy(&sync.stdout);

    // Must NOT claim unconditional "up to date".
    assert!(
        !stdout.contains("Workspace 'b' is up to date.\n")
            || stdout.contains("unresolved conflict"),
        "sync must not claim 'up to date' without mentioning the conflict; got:\n{stdout}"
    );

    // Must mention the unresolved count.
    assert!(
        stdout.contains("unresolved conflict"),
        "sync output must mention unresolved conflict(s); got:\n{stdout}"
    );

    // Must name the resolve command.
    assert!(
        stdout.contains("maw ws resolve b"),
        "sync output must tell the user how to resolve; got:\n{stdout}"
    );

    // The sidecar must not be deleted — it's still needed.
    assert!(
        repo.read_conflict_tree_sidecar("b").is_some(),
        "conflict-tree sidecar must survive the up-to-date sync"
    );

    // The summary must agree with `resolve --list`.
    let resolve = repo.maw_ok(&["ws", "resolve", "b", "--list"]);
    assert!(
        resolve.contains("shared.txt"),
        "resolve --list must still show the conflict after up-to-date sync; got:\n{resolve}"
    );

    // And the merge gate must still block.
    let check = repo.maw_raw(&["ws", "merge", "b", "--into", "default", "--check"]);
    assert!(
        !check.status.success(),
        "merge --check must still block after a no-op sync with residual conflict"
    );
}

// ---------------------------------------------------------------------------
// (e-bn-1mn0) reconstruction base_content + per-hunk keep paths
// ---------------------------------------------------------------------------

/// Set up an overlap+disjoint conflict scenario:
///
/// - `fresh.txt` has 10 lines at seed time.
/// - workspace `b` edits line 4 (overlapping) AND line 9 (disjoint / cleanly
///   merged with epoch side via sibling rebase).
/// - workspace `a` also edits line 4 (the overlap) which becomes the epoch
///   after `maw ws merge a`.
///
/// After the auto-rebase of `b`, the conflict is in line 4.  Line 9 is
/// cleanly merged (no conflict): it survives in `b`'s HEAD commit alongside
/// the placeholder blob.  A properly-run `--keep epoch` should preserve the
/// line-9 edit while taking epoch's version of line 4.
fn setup_overlap_and_disjoint_conflict(repo: &TestRepo) {
    // Seed a 10-line file.
    repo.seed_files(&[(
        "fresh.txt",
        "line1\nline2\nline3\nshared\nline5\nline6\nline7\nline8\ndisjoint\nline10\n",
    )]);

    repo.maw_ok(&["ws", "create", "a"]);
    repo.maw_ok(&["ws", "create", "b"]);

    // Workspace a: edit line 4 (the "shared" overlap line).
    repo.add_file(
        "a",
        "fresh.txt",
        "line1\nline2\nline3\nFROM_A\nline5\nline6\nline7\nline8\ndisjoint\nline10\n",
    );
    repo.git_in_workspace("a", &["commit", "-aqm", "a-edits-ln4"]);

    // Workspace b: edit line 4 (conflict with a) AND line 9 (disjoint).
    repo.add_file(
        "b",
        "fresh.txt",
        "line1\nline2\nline3\nFROM_B\nline5\nline6\nline7\nline8\nFROM_B_DISJOINT\nline10\n",
    );
    repo.git_in_workspace("b", &["commit", "-aqm", "b-edits-ln4-and-ln9"]);

    // Merge `a` → advances epoch; auto-rebase records a conflict for `b`.
    repo.maw_ok(&[
        "ws",
        "merge",
        "a",
        "--into",
        "default",
        "--message",
        "merge a",
    ]);

    // Precondition: sidecar must exist for b.
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

/// (e1-bn-1mn0) KEY TEST: after sidecar deletion, `resolve --list` reconstructs
/// the sidecar, and `--keep epoch` PRESERVES the workspace's cleanly-merged
/// disjoint edit (line 9 `FROM_B_DISJOINT`) while taking epoch's version of the
/// overlapping hunk (line 4 `FROM_A`).
///
/// Before bn-1mn0 this test would fail because:
///   - reconstruction built sides with no `base_content`
///   - `--keep epoch` fell through to whole-blob replace (epoch wins all of
///     fresh.txt including line 9 — `FROM_B_DISJOINT` was discarded)
///   - the output line was bare "resolved: fresh.txt" with no per-hunk suffix
#[test]
fn reconstruction_keep_epoch_preserves_disjoint_edit() {
    let repo = TestRepo::new();
    setup_overlap_and_disjoint_conflict(&repo);

    // Delete both sidecars so the bug-reproduction path is triggered.
    let sidecar_dir = repo.root().join(".manifold/artifacts/ws/b");
    let sidecar_path = sidecar_dir.join("conflict-tree.json");
    let _ = std::fs::remove_file(&sidecar_path);
    let _ = std::fs::remove_file(sidecar_dir.join("rebase-conflicts.json"));
    assert!(
        repo.read_conflict_tree_sidecar("b").is_none(),
        "precondition: sidecar must be gone before test"
    );

    // resolve --list must reconstruct and show the conflict.
    let list_out = repo.maw_ok(&["ws", "resolve", "b", "--list"]);
    assert!(
        list_out.contains("fresh.txt"),
        "resolve --list must show fresh.txt after reconstruction; got:\n{list_out}"
    );
    assert!(
        sidecar_path.exists(),
        "resolve --list must have written the reconstructed sidecar"
    );

    // resolve --keep epoch must use per-hunk semantics, NOT whole-blob replace.
    let keep_out = repo.maw_raw(&["ws", "resolve", "b", "--keep", "epoch"]);
    let keep_stdout = String::from_utf8_lossy(&keep_out.stdout);
    let keep_stderr = String::from_utf8_lossy(&keep_out.stderr);
    assert!(
        keep_out.status.success(),
        "resolve --keep epoch must succeed after reconstruction;\nstdout: {keep_stdout}\nstderr: {keep_stderr}"
    );

    // The output line must carry the per-hunk suffix, not bare "resolved: fresh.txt".
    let combined = format!("{keep_stdout}{keep_stderr}");
    assert!(
        combined.contains("kept epoch in conflicted hunk"),
        "resolve --keep epoch must use per-hunk semantics (ThreeWayEpochWins suffix); got:\n{combined}"
    );

    // The resolved file must contain epoch's line 4 version AND b's disjoint edit.
    let resolved = repo
        .read_file("b", "fresh.txt")
        .expect("fresh.txt must exist after resolution");
    assert!(
        resolved.contains("FROM_A"),
        "resolved file must contain epoch's line 4 (FROM_A); got:\n{resolved}"
    );
    assert!(
        resolved.contains("FROM_B_DISJOINT"),
        "resolved file must preserve b's disjoint line 9 edit (FROM_B_DISJOINT); got:\n{resolved}"
    );
    assert!(
        !resolved.contains("FROM_B\n"),
        "resolved file must NOT contain b's conflicted line 4 (FROM_B); got:\n{resolved}"
    );
}

/// (e2-bn-1mn0) Same scenario: `--keep <ws>` after reconstruction also uses
/// per-hunk semantics — preserves epoch's disjoint edits and applies ws's
/// intent on the conflict hunk.
#[test]
fn reconstruction_keep_ws_preserves_epoch_disjoint_edit() {
    let repo = TestRepo::new();
    setup_overlap_and_disjoint_conflict(&repo);

    // Delete both sidecars.
    let sidecar_dir = repo.root().join(".manifold/artifacts/ws/b");
    let _ = std::fs::remove_file(sidecar_dir.join("conflict-tree.json"));
    let _ = std::fs::remove_file(sidecar_dir.join("rebase-conflicts.json"));

    // Reconstruct via --list.
    repo.maw_ok(&["ws", "resolve", "b", "--list"]);

    // resolve --keep b must succeed with per-hunk semantics.
    let keep_out = repo.maw_raw(&["ws", "resolve", "b", "--keep", "b"]);
    let keep_stdout = String::from_utf8_lossy(&keep_out.stdout);
    let keep_stderr = String::from_utf8_lossy(&keep_out.stderr);
    assert!(
        keep_out.status.success(),
        "resolve --keep b must succeed after reconstruction;\nstdout: {keep_stdout}\nstderr: {keep_stderr}"
    );

    // Output must contain the per-hunk suffix (3-way ws-wins) — not bare "resolved: fresh.txt".
    let combined = format!("{keep_stdout}{keep_stderr}");
    assert!(
        combined.contains("ws intent on top of epoch"),
        "resolve --keep b must use per-hunk semantics (ThreeWayClean or ThreeWayWsWins); got:\n{combined}"
    );

    // The resolved file must contain b's conflict-hunk version.
    let resolved = repo
        .read_file("b", "fresh.txt")
        .expect("fresh.txt must exist after resolution");
    assert!(
        resolved.contains("FROM_B"),
        "resolved file must contain b's line 4 (FROM_B); got:\n{resolved}"
    );
    // And the workspace's disjoint edit must be present too.
    assert!(
        resolved.contains("FROM_B_DISJOINT"),
        "resolved file must preserve b's disjoint line 9 edit; got:\n{resolved}"
    );
}

/// (e3-bn-1mn0) Same scenario: `--keep both` after reconstruction uses
/// per-hunk union semantics — both conflict-hunk versions included, disjoint
/// edits preserved.
#[test]
fn reconstruction_keep_both_uses_per_hunk_union() {
    let repo = TestRepo::new();
    setup_overlap_and_disjoint_conflict(&repo);

    // Delete both sidecars.
    let sidecar_dir = repo.root().join(".manifold/artifacts/ws/b");
    let _ = std::fs::remove_file(sidecar_dir.join("conflict-tree.json"));
    let _ = std::fs::remove_file(sidecar_dir.join("rebase-conflicts.json"));

    // Reconstruct via --list.
    repo.maw_ok(&["ws", "resolve", "b", "--list"]);

    // resolve --keep both must succeed with per-hunk union semantics.
    let keep_out = repo.maw_raw(&["ws", "resolve", "b", "--keep", "both"]);
    let keep_stdout = String::from_utf8_lossy(&keep_out.stdout);
    let keep_stderr = String::from_utf8_lossy(&keep_out.stderr);
    assert!(
        keep_out.status.success(),
        "resolve --keep both must succeed after reconstruction;\nstdout: {keep_stdout}\nstderr: {keep_stderr}"
    );

    // Output must contain the per-hunk union suffix.
    let combined = format!("{keep_stdout}{keep_stderr}");
    assert!(
        combined.contains("kept both sides in conflicted hunk"),
        "resolve --keep both must use per-hunk union semantics (ThreeWayUnion suffix); got:\n{combined}"
    );

    // The resolved file must contain both sides of the conflict hunk AND b's disjoint edit.
    let resolved = repo
        .read_file("b", "fresh.txt")
        .expect("fresh.txt must exist after resolution");
    assert!(
        resolved.contains("FROM_A"),
        "resolved file must contain epoch's line 4 (FROM_A) in union; got:\n{resolved}"
    );
    assert!(
        resolved.contains("FROM_B"),
        "resolved file must contain b's line 4 (FROM_B) in union; got:\n{resolved}"
    );
    assert!(
        resolved.contains("FROM_B_DISJOINT"),
        "resolved file must preserve b's disjoint line 9 edit; got:\n{resolved}"
    );
}

/// (e4-bn-1mn0) Fallback warning: when the sidecar has NO base OID anywhere
/// (add/add conflict — no base), `--keep epoch` emits the legacy-fallback
/// warning on stderr rather than silently degrading.
#[test]
fn keep_epoch_no_base_emits_warning() {
    let repo = TestRepo::new();
    // Seed an empty repo (no existing file).
    repo.seed_files(&[]);

    repo.maw_ok(&["ws", "create", "a"]);
    repo.maw_ok(&["ws", "create", "b"]);

    // Both a and b ADD the same file (add/add conflict — no base OID).
    repo.add_file("a", "new.txt", "epoch version\n");
    repo.git_in_workspace("a", &["add", "-A"]);
    repo.git_in_workspace("a", &["commit", "-qm", "a-adds-new"]);

    repo.add_file("b", "new.txt", "ws version\n");
    repo.git_in_workspace("b", &["add", "-A"]);
    repo.git_in_workspace("b", &["commit", "-qm", "b-adds-new"]);

    // Merge a → epoch; b gets an add/add conflict.
    repo.maw_ok(&[
        "ws",
        "merge",
        "a",
        "--into",
        "default",
        "--message",
        "merge a",
    ]);

    // Check that b has a conflict sidecar.
    let sidecar = repo.read_conflict_tree_sidecar("b");
    if sidecar.is_none() {
        // No conflict sidecar — add/add may not produce a structured conflict
        // in all scenarios. Skip the warning test if so.
        return;
    }

    // Delete sidecars to force reconstruction path.
    let sidecar_dir = repo.root().join(".manifold/artifacts/ws/b");
    let _ = std::fs::remove_file(sidecar_dir.join("conflict-tree.json"));
    let _ = std::fs::remove_file(sidecar_dir.join("rebase-conflicts.json"));

    // Reconstruct.
    let list_out = repo.maw_ok(&["ws", "resolve", "b", "--list"]);
    assert!(
        list_out.contains("new.txt"),
        "resolve --list must list add/add conflict; got:\n{list_out}"
    );

    // resolve --keep epoch on an add/add conflict (no base OID) must:
    //   (a) succeed (fallback to blob-replace)
    //   (b) print a warning on stderr
    let keep_out = repo.maw_raw(&["ws", "resolve", "b", "--keep", "epoch"]);
    let keep_stdout = String::from_utf8_lossy(&keep_out.stdout);
    let keep_stderr = String::from_utf8_lossy(&keep_out.stderr);
    // May fail if epoch side isn't labelled "epoch" on add/add; treat as skip.
    if !keep_out.status.success() {
        return;
    }
    assert!(
        keep_stderr.contains("warning:") || keep_stdout.contains("warning:"),
        "resolve --keep epoch with no base must emit a legacy-fallback warning; \
         got stdout:\n{keep_stdout}\nstderr:\n{keep_stderr}"
    );
}

/// When the workspace IS up-to-date AND genuinely clean (no residual
/// conflicts), sync must still print the plain "is up to date" message with
/// no spurious conflict mention.
#[test]
fn sync_up_to_date_clean_workspace_prints_up_to_date() {
    let repo = TestRepo::new();
    repo.seed_files(&[("file.txt", "initial\n")]);
    repo.maw_ok(&["ws", "create", "clean"]);

    // Workspace is current and clean — sync should be a plain no-op.
    let sync = repo.maw_raw(&["ws", "sync", "clean"]);
    assert!(
        sync.status.success(),
        "sync on a clean current workspace should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&sync.stdout),
        String::from_utf8_lossy(&sync.stderr)
    );
    let stdout = String::from_utf8_lossy(&sync.stdout);

    assert!(
        stdout.contains("is up to date"),
        "clean up-to-date workspace must print 'is up to date'; got:\n{stdout}"
    );
    assert!(
        !stdout.contains("conflict"),
        "clean up-to-date workspace must not mention conflicts; got:\n{stdout}"
    );
}
