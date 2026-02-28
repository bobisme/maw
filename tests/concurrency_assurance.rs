//! Phase 0.5 concurrency assurance tests.
//!
//! Exercises the safety properties added by the concurrency hardening bones:
//!
//! - **bn-1a10**: O_EXCL merge-state prevents concurrent merges
//! - **bn-20jn**: Atomic two-ref COMMIT phase (epoch + branch CAS)
//! - **bn-t9cm**: CAS ref updates for push --advance
//! - **bn-qf0b**: Destroy record resilience (tested via unit tests in
//!   `src/workspace/destroy_record.rs` — the module is pub(crate))
//!
//! All tests are deterministic: no timing-dependent races, no sleeps.

#![allow(clippy::all, clippy::pedantic, clippy::nursery)]

use std::collections::BTreeMap;
use std::process::Command;

use tempfile::TempDir;

use maw::merge::commit::{run_commit_phase, CommitResult, recover_partial_commit, CommitRecovery};
use maw::merge::prepare::{run_prepare_phase_with_epoch, PrepareError};
use maw::merge_state::{MergePhase, MergeStateFile};
use maw::model::types::{EpochId, GitOid, WorkspaceId};
use maw::refs;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run a git command in the given directory. Panics on failure.
fn git(root: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn git {}: {e}", args.join(" ")));
    assert!(
        out.status.success(),
        "git {} failed (exit {}):\nstdout: {}\nstderr: {}",
        args.join(" "),
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Create a minimal git repo with one commit. Returns (TempDir, HEAD OID).
fn setup_repo() -> (TempDir, GitOid) {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    git(root, &["init"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@test.com"]);
    git(root, &["config", "commit.gpgsign", "false"]);

    std::fs::write(root.join("README.md"), "# Test\n").unwrap();
    git(root, &["add", "README.md"]);
    git(root, &["commit", "-m", "initial"]);

    let oid_hex = git(root, &["rev-parse", "HEAD"]);
    let oid = GitOid::new(&oid_hex).unwrap();

    (dir, oid)
}

/// Add an empty commit and return the new HEAD OID.
fn add_empty_commit(root: &std::path::Path, msg: &str) -> GitOid {
    git(root, &["commit", "--allow-empty", "-m", msg]);
    let hex = git(root, &["rev-parse", "HEAD"]);
    GitOid::new(&hex).unwrap()
}

/// Create a test EpochId from a repeated character.
fn epoch(c: char) -> EpochId {
    EpochId::new(&c.to_string().repeat(40)).unwrap()
}

/// Create a test GitOid from a repeated character.
fn oid(c: char) -> GitOid {
    GitOid::new(&c.to_string().repeat(40)).unwrap()
}

/// Create a test WorkspaceId.
fn ws(name: &str) -> WorkspaceId {
    WorkspaceId::new(name).unwrap()
}

// ===========================================================================
// 1. O_EXCL merge-state (bn-1a10)
//
// The merge-state file prevents concurrent merges. If a merge-state file
// exists in a non-terminal phase, a new prepare must fail with
// MergeAlreadyInProgress. Terminal states (Complete, Aborted) are safe
// to overwrite.
// ===========================================================================

#[test]
fn merge_state_blocks_concurrent_prepare_when_in_progress() {
    let dir = TempDir::new().unwrap();
    let manifold_dir = dir.path().join(".manifold");

    let epoch_a = epoch('a');
    let ws_first = ws("first");
    let ws_second = ws("second");

    // First prepare succeeds.
    let mut heads = BTreeMap::new();
    heads.insert(ws_first.clone(), oid('b'));
    run_prepare_phase_with_epoch(&manifold_dir, epoch_a.clone(), &[ws_first], heads).unwrap();

    // Second prepare must fail — merge-state is in Prepare phase (non-terminal).
    let mut heads2 = BTreeMap::new();
    heads2.insert(ws_second.clone(), oid('c'));
    let err =
        run_prepare_phase_with_epoch(&manifold_dir, epoch_a, &[ws_second], heads2).unwrap_err();

    assert!(
        matches!(err, PrepareError::MergeAlreadyInProgress),
        "expected MergeAlreadyInProgress, got: {err}"
    );
}

#[test]
fn merge_state_blocks_prepare_during_build_phase() {
    let dir = TempDir::new().unwrap();
    let manifold_dir = dir.path().join(".manifold");
    std::fs::create_dir_all(&manifold_dir).unwrap();

    // Write a merge-state advanced to Build phase.
    let mut existing =
        MergeStateFile::new(vec![ws("old-ws")], epoch('a'), 1000);
    existing.advance(MergePhase::Build, 1001).unwrap();
    existing
        .write_atomic(&MergeStateFile::default_path(&manifold_dir))
        .unwrap();

    // New prepare must fail.
    let mut heads = BTreeMap::new();
    heads.insert(ws("new-ws"), oid('d'));
    let err =
        run_prepare_phase_with_epoch(&manifold_dir, epoch('a'), &[ws("new-ws")], heads)
            .unwrap_err();

    assert!(
        matches!(err, PrepareError::MergeAlreadyInProgress),
        "expected MergeAlreadyInProgress during Build, got: {err}"
    );
}

#[test]
fn merge_state_blocks_prepare_during_validate_phase() {
    let dir = TempDir::new().unwrap();
    let manifold_dir = dir.path().join(".manifold");
    std::fs::create_dir_all(&manifold_dir).unwrap();

    let mut existing =
        MergeStateFile::new(vec![ws("old-ws")], epoch('a'), 1000);
    existing.advance(MergePhase::Build, 1001).unwrap();
    existing.advance(MergePhase::Validate, 1002).unwrap();
    existing
        .write_atomic(&MergeStateFile::default_path(&manifold_dir))
        .unwrap();

    let mut heads = BTreeMap::new();
    heads.insert(ws("new-ws"), oid('d'));
    let err =
        run_prepare_phase_with_epoch(&manifold_dir, epoch('a'), &[ws("new-ws")], heads)
            .unwrap_err();

    assert!(
        matches!(err, PrepareError::MergeAlreadyInProgress),
        "expected MergeAlreadyInProgress during Validate, got: {err}"
    );
}

#[test]
fn merge_state_allows_prepare_after_complete() {
    let dir = TempDir::new().unwrap();
    let manifold_dir = dir.path().join(".manifold");
    std::fs::create_dir_all(&manifold_dir).unwrap();

    // Write a terminal Complete state.
    let mut existing =
        MergeStateFile::new(vec![ws("old-ws")], epoch('a'), 1000);
    existing.advance(MergePhase::Build, 1001).unwrap();
    existing.advance(MergePhase::Validate, 1002).unwrap();
    existing.advance(MergePhase::Commit, 1003).unwrap();
    existing.advance(MergePhase::Cleanup, 1004).unwrap();
    existing.advance(MergePhase::Complete, 1005).unwrap();
    existing
        .write_atomic(&MergeStateFile::default_path(&manifold_dir))
        .unwrap();

    // New prepare should succeed (overwriting terminal state).
    let mut heads = BTreeMap::new();
    heads.insert(ws("new-ws"), oid('e'));
    let result =
        run_prepare_phase_with_epoch(&manifold_dir, epoch('a'), &[ws("new-ws")], heads);

    assert!(
        result.is_ok(),
        "expected prepare to succeed after Complete, got: {result:?}"
    );

    // Verify merge-state was overwritten.
    let state = MergeStateFile::read(&MergeStateFile::default_path(&manifold_dir)).unwrap();
    assert_eq!(state.phase, MergePhase::Prepare);
    assert_eq!(state.sources, vec![ws("new-ws")]);
}

#[test]
fn merge_state_allows_prepare_after_aborted() {
    let dir = TempDir::new().unwrap();
    let manifold_dir = dir.path().join(".manifold");
    std::fs::create_dir_all(&manifold_dir).unwrap();

    // Write a terminal Aborted state.
    let mut existing =
        MergeStateFile::new(vec![ws("old-ws")], epoch('a'), 1000);
    existing.abort("test abort", 1001).unwrap();
    existing
        .write_atomic(&MergeStateFile::default_path(&manifold_dir))
        .unwrap();

    // New prepare should succeed.
    let mut heads = BTreeMap::new();
    heads.insert(ws("new-ws"), oid('f'));
    let result =
        run_prepare_phase_with_epoch(&manifold_dir, epoch('a'), &[ws("new-ws")], heads);

    assert!(
        result.is_ok(),
        "expected prepare to succeed after Aborted, got: {result:?}"
    );
}

// ===========================================================================
// 2. Atomic two-ref COMMIT (bn-20jn)
//
// The COMMIT phase must atomically advance both refs/manifold/epoch/current
// and refs/heads/<branch>. If the epoch CAS fails, neither ref should move.
// ===========================================================================

/// Helper: set up a repo with two commits and both epoch + branch refs at
/// the first commit. Returns (TempDir, old_oid, new_oid).
fn setup_commit_repo() -> (TempDir, GitOid, GitOid) {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    git(root, &["init"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@test.com"]);
    git(root, &["config", "commit.gpgsign", "false"]);

    std::fs::write(root.join("README.md"), "hello\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "initial"]);
    git(root, &["branch", "-M", "main"]);

    let old = GitOid::new(&git(root, &["rev-parse", "HEAD"])).unwrap();

    std::fs::write(root.join("README.md"), "hello world\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "candidate"]);

    let new = GitOid::new(&git(root, &["rev-parse", "HEAD"])).unwrap();

    // Reset both refs to old so the commit phase can advance them.
    git(root, &["update-ref", "refs/heads/main", old.as_str()]);
    git(root, &["update-ref", refs::EPOCH_CURRENT, old.as_str()]);

    (dir, old, new)
}

#[test]
fn commit_phase_advances_both_refs_atomically() {
    let (dir, old, new) = setup_commit_repo();
    let root = dir.path();

    let result = run_commit_phase(root, "main", &old, &new).unwrap();
    assert_eq!(result, CommitResult::Committed);

    // Both refs should now point to the new commit.
    let epoch = refs::read_ref(root, refs::EPOCH_CURRENT).unwrap();
    assert_eq!(epoch, Some(new.clone()), "epoch ref should be at new commit");

    let main = refs::read_ref(root, "refs/heads/main").unwrap();
    assert_eq!(main, Some(new), "main ref should be at new commit");
}

#[test]
fn commit_phase_rejects_stale_epoch() {
    let (dir, old, new) = setup_commit_repo();
    let root = dir.path();

    // Create a third commit to use as a "someone else advanced" value.
    let interloper = add_empty_commit(root, "interloper");
    // Advance the epoch ref behind our back.
    git(root, &["update-ref", refs::EPOCH_CURRENT, interloper.as_str()]);
    // Reset main back to old.
    git(root, &["update-ref", "refs/heads/main", old.as_str()]);

    // COMMIT should fail because the epoch CAS (old -> new) will mismatch.
    let result = run_commit_phase(root, "main", &old, &new);
    assert!(
        result.is_err(),
        "expected commit phase to fail with stale epoch, got: {result:?}"
    );

    // Verify the main branch did NOT move (atomicity guarantee: if epoch
    // CAS fails, nothing changes).
    let main = refs::read_ref(root, "refs/heads/main").unwrap();
    assert_eq!(
        main,
        Some(old),
        "main ref must not move when epoch CAS fails"
    );
}

#[test]
fn commit_recovery_finalizes_partial_commit() {
    // Simulate: epoch ref moved but branch ref still at old.
    let (dir, old, new) = setup_commit_repo();
    let root = dir.path();

    // Manually advance epoch only (simulate crash between the two CAS ops).
    refs::advance_epoch(root, &old, &new).unwrap();

    // Branch is still at old.
    let main_before = refs::read_ref(root, "refs/heads/main").unwrap();
    assert_eq!(main_before, Some(old.clone()));

    // Recovery should finalize by moving main.
    let recovery = recover_partial_commit(root, "main", &old, &new).unwrap();
    assert_eq!(recovery, CommitRecovery::FinalizedMainRef);

    // Now both refs point to new.
    let main_after = refs::read_ref(root, "refs/heads/main").unwrap();
    assert_eq!(main_after, Some(new.clone()));
    let epoch_after = refs::read_ref(root, refs::EPOCH_CURRENT).unwrap();
    assert_eq!(epoch_after, Some(new));
}

#[test]
fn commit_recovery_detects_already_committed() {
    let (dir, old, new) = setup_commit_repo();
    let root = dir.path();

    // Both refs already at new.
    git(root, &["update-ref", refs::EPOCH_CURRENT, new.as_str()]);
    git(root, &["update-ref", "refs/heads/main", new.as_str()]);

    let recovery = recover_partial_commit(root, "main", &old, &new).unwrap();
    assert_eq!(recovery, CommitRecovery::AlreadyCommitted);
}

#[test]
fn commit_recovery_detects_not_committed() {
    let (dir, old, new) = setup_commit_repo();
    let root = dir.path();

    // Both refs still at old.
    let recovery = recover_partial_commit(root, "main", &old, &new).unwrap();
    assert_eq!(recovery, CommitRecovery::NotCommitted);
}

// ===========================================================================
// 3. CAS push --advance (bn-t9cm)
//
// The push advance pattern uses CAS to ensure only one agent can move a
// ref at a time. These tests exercise the CAS primitive that underpins it.
// ===========================================================================

#[test]
fn cas_ref_update_succeeds_with_correct_old_value() {
    let (dir, v1) = setup_repo();
    let root = dir.path();
    let v2 = add_empty_commit(root, "second");

    refs::write_ref(root, refs::EPOCH_CURRENT, &v1).unwrap();

    // CAS from v1 to v2 should succeed.
    refs::write_ref_cas(root, refs::EPOCH_CURRENT, &v1, &v2).unwrap();

    let current = refs::read_ref(root, refs::EPOCH_CURRENT).unwrap();
    assert_eq!(current, Some(v2));
}

#[test]
fn cas_ref_update_fails_with_stale_old_value() {
    let (dir, v1) = setup_repo();
    let root = dir.path();
    let v2 = add_empty_commit(root, "second");
    let v3 = add_empty_commit(root, "third");

    // Set ref to v2.
    refs::write_ref(root, refs::EPOCH_CURRENT, &v2).unwrap();

    // Try CAS with stale v1 as expected old — should fail.
    let err = refs::write_ref_cas(root, refs::EPOCH_CURRENT, &v1, &v3).unwrap_err();
    assert!(
        matches!(err, refs::RefError::CasMismatch { .. }),
        "expected CasMismatch, got: {err}"
    );

    // Ref must remain at v2 (no partial update).
    let current = refs::read_ref(root, refs::EPOCH_CURRENT).unwrap();
    assert_eq!(current, Some(v2));
}

#[test]
fn cas_simulated_two_agent_race() {
    // Agent A and Agent B both read epoch=v1. Agent A advances to v2 first.
    // Agent B tries v1->v3: CAS must fail because current is now v2.
    let (dir, v1) = setup_repo();
    let root = dir.path();
    let v2 = add_empty_commit(root, "agent-a-work");
    let v3 = add_empty_commit(root, "agent-b-work");

    refs::write_ref(root, refs::EPOCH_CURRENT, &v1).unwrap();

    // Agent A wins.
    refs::write_ref_cas(root, refs::EPOCH_CURRENT, &v1, &v2).unwrap();

    // Agent B tries with stale read.
    let err = refs::write_ref_cas(root, refs::EPOCH_CURRENT, &v1, &v3).unwrap_err();
    assert!(
        matches!(err, refs::RefError::CasMismatch { .. }),
        "agent B should lose the race"
    );

    // Epoch is at v2, not v3.
    let current = refs::read_ref(root, refs::EPOCH_CURRENT).unwrap();
    assert_eq!(current, Some(v2));
}

#[test]
fn cas_advance_epoch_wrapper_race() {
    // Exercise the convenience wrapper refs::advance_epoch with a race.
    let (dir, v1) = setup_repo();
    let root = dir.path();
    let v2 = add_empty_commit(root, "work-a");
    let v3 = add_empty_commit(root, "work-b");

    refs::write_epoch_current(root, &v1).unwrap();

    // Agent A advances epoch v1 -> v2.
    refs::advance_epoch(root, &v1, &v2).unwrap();

    // Agent B tries with stale v1.
    let err = refs::advance_epoch(root, &v1, &v3).unwrap_err();
    assert!(
        matches!(err, refs::RefError::CasMismatch { .. }),
        "stale advance_epoch should fail with CasMismatch"
    );

    // Epoch is at v2.
    let current = refs::read_epoch_current(root).unwrap();
    assert_eq!(current, Some(v2));
}

#[test]
fn cas_branch_ref_update_for_push_advance_pattern() {
    // Simulates the push --advance CAS pattern: move refs/heads/main with
    // CAS so a concurrent push is detected.
    let (dir, v1) = setup_repo();
    let root = dir.path();
    let v2 = add_empty_commit(root, "agent-work");
    let v3 = add_empty_commit(root, "concurrent-push");

    let branch_ref = "refs/heads/main";

    // Branch starts at v1.
    refs::write_ref(root, branch_ref, &v1).unwrap();

    // Agent tries to advance main: v1 -> v2.
    refs::write_ref_cas(root, branch_ref, &v1, &v2).unwrap();

    // Meanwhile, another push already advanced main to v2. A second agent
    // that read stale v1 tries v1 -> v3.
    let err = refs::write_ref_cas(root, branch_ref, &v1, &v3).unwrap_err();
    assert!(
        matches!(err, refs::RefError::CasMismatch { .. }),
        "concurrent push advance should fail with CasMismatch"
    );

    // Branch is at v2.
    let current = refs::read_ref(root, branch_ref).unwrap();
    assert_eq!(current, Some(v2));
}

// ===========================================================================
// Combined scenario: merge-state exclusion + CAS refs
//
// End-to-end test of the concurrency invariant: one agent runs a full
// prepare -> commit cycle; a second agent attempting prepare is blocked.
// ===========================================================================

#[test]
fn full_lifecycle_exclusion_and_cas() {
    let (dir, old, new) = setup_commit_repo();
    let root = dir.path();
    let manifold_dir = root.join(".manifold");

    // Agent A: run prepare phase with known epoch.
    let epoch_a = EpochId::new(old.as_str()).unwrap();
    let ws_a = ws("agent-a");
    let mut heads_a = BTreeMap::new();
    heads_a.insert(ws_a.clone(), old.clone());

    let frozen =
        run_prepare_phase_with_epoch(&manifold_dir, epoch_a, &[ws_a], heads_a).unwrap();
    assert_eq!(frozen.epoch.as_str(), old.as_str());

    // Agent B: tries to prepare — blocked by merge-state.
    let ws_b = ws("agent-b");
    let mut heads_b = BTreeMap::new();
    heads_b.insert(ws_b.clone(), new.clone());
    let epoch_b = EpochId::new(old.as_str()).unwrap();

    let err =
        run_prepare_phase_with_epoch(&manifold_dir, epoch_b, &[ws_b], heads_b).unwrap_err();
    assert!(
        matches!(err, PrepareError::MergeAlreadyInProgress),
        "agent B blocked by merge-state"
    );

    // Agent A: run commit phase.
    let result = run_commit_phase(root, "main", &old, &new).unwrap();
    assert_eq!(result, CommitResult::Committed);

    // Both refs at new.
    assert_eq!(
        refs::read_epoch_current(root).unwrap(),
        Some(GitOid::new(new.as_str()).unwrap())
    );
    assert_eq!(
        refs::read_ref(root, "refs/heads/main").unwrap(),
        Some(new)
    );
}
