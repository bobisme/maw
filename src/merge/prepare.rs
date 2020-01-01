//! PREPARE phase of the epoch advancement state machine.
//!
//! Freezes all merge inputs (epoch commit + workspace HEAD commits) so that
//! the merge is deterministic regardless of concurrent workspace activity.
//!
//! # Crash safety
//!
//! The merge-state file is written atomically (write-to-temp + fsync +
//! rename). If a crash occurs during PREPARE:
//!
//! - **Before write:** No merge-state file exists → nothing to recover.
//! - **After write:** A valid merge-state in `Prepare` phase exists →
//!   recovery aborts it (safe, no state was changed).
//!
//! # Inputs
//!
//! - The current epoch commit (`refs/manifold/epoch/current`)
//! - The HEAD commit of each source workspace
//!
//! # Outputs
//!
//! - A persisted `merge-state.json` in `Prepare` phase with all OIDs frozen.
//! - A [`FrozenInputs`] struct for downstream phases.

#![allow(clippy::missing_errors_doc)]

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::merge_state::{MergePhase, MergeStateError, MergeStateFile};
use crate::model::types::{EpochId, GitOid, WorkspaceId};
use crate::refs;

// ---------------------------------------------------------------------------
// FrozenInputs
// ---------------------------------------------------------------------------

/// The frozen set of inputs for a merge operation.
///
/// After PREPARE completes, these OIDs are immutable references. The merge
/// engine operates on exactly these commits, regardless of any concurrent
/// workspace activity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenInputs {
    /// The epoch commit that serves as the merge base.
    pub epoch: EpochId,
    /// Map of workspace ID → HEAD commit OID at freeze time.
    pub heads: BTreeMap<WorkspaceId, GitOid>,
}

// ---------------------------------------------------------------------------
// PrepareError
// ---------------------------------------------------------------------------

/// Errors that can occur during the PREPARE phase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrepareError {
    /// No source workspaces provided.
    NoSources,
    /// Failed to read the current epoch ref.
    EpochNotFound(String),
    /// Failed to read a workspace HEAD.
    WorkspaceHeadNotFound {
        workspace: WorkspaceId,
        detail: String,
    },
    /// A merge is already in progress (merge-state file exists).
    MergeAlreadyInProgress,
    /// Invalid OID from git.
    InvalidOid(String),
    /// Merge-state I/O or serialization error.
    State(MergeStateError),
    /// Git command failed.
    GitError(String),
}

impl fmt::Display for PrepareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSources => write!(f, "PREPARE: no source workspaces provided"),
            Self::EpochNotFound(detail) => {
                write!(f, "PREPARE: epoch ref not found: {detail}")
            }
            Self::WorkspaceHeadNotFound { workspace, detail } => {
                write!(
                    f,
                    "PREPARE: HEAD not found for workspace {workspace}: {detail}"
                )
            }
            Self::MergeAlreadyInProgress => {
                write!(
                    f,
                    "PREPARE: merge already in progress (merge-state file exists)"
                )
            }
            Self::InvalidOid(detail) => {
                write!(f, "PREPARE: invalid OID from git: {detail}")
            }
            Self::State(e) => write!(f, "PREPARE: {e}"),
            Self::GitError(detail) => write!(f, "PREPARE: git error: {detail}"),
        }
    }
}

impl std::error::Error for PrepareError {}

impl From<MergeStateError> for PrepareError {
    fn from(e: MergeStateError) -> Self {
        Self::State(e)
    }
}

// ---------------------------------------------------------------------------
// Git helpers
// ---------------------------------------------------------------------------

/// Read the current epoch OID from `refs/manifold/epoch/current`.
///
/// Uses `git rev-parse` in the given repo root directory.
fn read_epoch_ref(repo_root: &Path) -> Result<EpochId, PrepareError> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "refs/manifold/epoch/current"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| PrepareError::GitError(format!("spawn git: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(PrepareError::EpochNotFound(stderr));
    }

    let hex = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    EpochId::new(&hex).map_err(|e| PrepareError::InvalidOid(e.to_string()))
}

/// Read the HEAD commit OID of a workspace directory.
///
/// Uses `git rev-parse HEAD` in the workspace directory.
fn read_workspace_head(
    _repo_root: &Path,
    workspace: &WorkspaceId,
    workspace_dir: &Path,
) -> Result<GitOid, PrepareError> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .current_dir(workspace_dir)
        .output()
        .map_err(|e| PrepareError::WorkspaceHeadNotFound {
            workspace: workspace.clone(),
            detail: format!("spawn git: {e}"),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(PrepareError::WorkspaceHeadNotFound {
            workspace: workspace.clone(),
            detail: stderr,
        });
    }

    let hex = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    GitOid::new(&hex).map_err(|e| PrepareError::InvalidOid(e.to_string()))
}

// ---------------------------------------------------------------------------
// run_prepare_phase
// ---------------------------------------------------------------------------

/// Execute the PREPARE phase of the merge state machine.
///
/// 1. Verify no merge is already in progress.
/// 2. Read the current epoch from `refs/manifold/epoch/current`.
/// 3. For each source workspace, read its HEAD commit.
/// 4. Record all frozen OIDs in a new merge-state file.
/// 5. Write merge-state atomically with fsync.
///
/// # Arguments
///
/// * `repo_root` - Path to the git repository root.
/// * `manifold_dir` - Path to the `.manifold/` directory.
/// * `sources` - The workspace IDs to merge.
/// * `workspace_dirs` - Map of workspace ID → absolute workspace directory path.
///
/// # Returns
///
/// The [`FrozenInputs`] containing the epoch and all workspace HEAD OIDs.
///
/// # Errors
///
/// Returns [`PrepareError`] if any input cannot be read, a merge is already
/// in progress, or the merge-state file cannot be written.
pub fn run_prepare_phase(
    repo_root: &Path,
    manifold_dir: &Path,
    sources: &[WorkspaceId],
    workspace_dirs: &BTreeMap<WorkspaceId, std::path::PathBuf>,
) -> Result<FrozenInputs, PrepareError> {
    // 1. Validate inputs
    if sources.is_empty() {
        return Err(PrepareError::NoSources);
    }

    // 2. Check for in-progress merge
    let state_path = MergeStateFile::default_path(manifold_dir);
    if state_path.exists() {
        // Check if it's a terminal state — if so, we can overwrite
        match MergeStateFile::read(&state_path) {
            Ok(existing) if !existing.phase.is_terminal() => {
                // For Commit/Cleanup phases the COMMIT has already run (refs
                // updated) but the process was killed before the cleanup
                // deleted merge-state.json.  Detect this by checking whether
                // the epoch ref has already advanced to epoch_candidate.  If
                // it has, the previous merge finished — the stale file is safe
                // to overwrite.
                let is_post_commit = matches!(
                    existing.phase,
                    MergePhase::Commit | MergePhase::Cleanup
                );
                let stale_completed = is_post_commit
                    && existing.epoch_candidate.as_ref().is_some_and(|candidate| {
                        refs::read_epoch_current(repo_root)
                            .ok()
                            .flatten()
                            .is_some_and(|current| &current == candidate)
                    });

                if stale_completed {
                    eprintln!(
                        "WARNING: stale merge-state found at phase '{}' but epoch ref already \
                         advanced — previous merge completed without cleanup. Clearing stale state.",
                        existing.phase
                    );
                    // Fall through: overwrite the stale file below
                } else {
                    return Err(PrepareError::MergeAlreadyInProgress);
                }
            }
            _ => {
                // Terminal or corrupt — safe to overwrite
            }
        }
    }

    // 3. Read current epoch
    let epoch = read_epoch_ref(repo_root)?;

    // 4. Read workspace HEADs
    let mut heads = BTreeMap::new();
    for ws_id in sources {
        let ws_dir =
            workspace_dirs
                .get(ws_id)
                .ok_or_else(|| PrepareError::WorkspaceHeadNotFound {
                    workspace: ws_id.clone(),
                    detail: "workspace directory not provided".to_owned(),
                })?;
        let head = read_workspace_head(repo_root, ws_id, ws_dir)?;
        heads.insert(ws_id.clone(), head);
    }

    // 5. Create merge-state
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut state = MergeStateFile::new(sources.to_vec(), epoch.clone(), now);
    state.frozen_heads = heads.clone();

    // 6. Write atomically with fsync
    // Ensure .manifold directory exists
    std::fs::create_dir_all(manifold_dir).map_err(|e| {
        PrepareError::State(MergeStateError::Io(format!(
            "create {}: {e}",
            manifold_dir.display()
        )))
    })?;
    state.write_atomic(&state_path)?;

    Ok(FrozenInputs { epoch, heads })
}

/// Execute the PREPARE phase with an explicit epoch (for testing or
/// when the epoch is already known).
///
/// Same as [`run_prepare_phase`] but skips reading the epoch ref.
pub fn run_prepare_phase_with_epoch(
    manifold_dir: &Path,
    epoch: EpochId,
    sources: &[WorkspaceId],
    heads: BTreeMap<WorkspaceId, GitOid>,
) -> Result<FrozenInputs, PrepareError> {
    if sources.is_empty() {
        return Err(PrepareError::NoSources);
    }

    let state_path = MergeStateFile::default_path(manifold_dir);
    if state_path.exists() {
        match MergeStateFile::read(&state_path) {
            Ok(existing) if !existing.phase.is_terminal() => {
                return Err(PrepareError::MergeAlreadyInProgress);
            }
            _ => {}
        }
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut state = MergeStateFile::new(sources.to_vec(), epoch.clone(), now);
    state.frozen_heads = heads.clone();

    std::fs::create_dir_all(manifold_dir).map_err(|e| {
        PrepareError::State(MergeStateError::Io(format!(
            "create {}: {e}",
            manifold_dir.display()
        )))
    })?;
    state.write_atomic(&state_path)?;

    Ok(FrozenInputs { epoch, heads })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
mod tests {
    use super::*;
    use crate::merge_state::{MergePhase, RecoveryOutcome, recover_from_merge_state};

    fn test_epoch() -> EpochId {
        EpochId::new(&"a".repeat(40)).unwrap()
    }

    fn test_oid(c: char) -> GitOid {
        GitOid::new(&c.to_string().repeat(40)).unwrap()
    }

    fn test_ws(name: &str) -> WorkspaceId {
        WorkspaceId::new(name).unwrap()
    }

    // -- run_prepare_phase_with_epoch --

    #[test]
    fn prepare_freezes_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let manifold_dir = dir.path().join(".manifold");

        let epoch = test_epoch();
        let ws_a = test_ws("agent-a");
        let ws_b = test_ws("agent-b");

        let mut heads = BTreeMap::new();
        heads.insert(ws_a.clone(), test_oid('b'));
        heads.insert(ws_b.clone(), test_oid('c'));

        let sources = vec![ws_a.clone(), ws_b.clone()];
        let frozen =
            run_prepare_phase_with_epoch(&manifold_dir, epoch.clone(), &sources, heads.clone())
                .unwrap();

        assert_eq!(frozen.epoch, epoch);
        assert_eq!(frozen.heads.len(), 2);
        assert_eq!(frozen.heads[&ws_a], test_oid('b'));
        assert_eq!(frozen.heads[&ws_b], test_oid('c'));
    }

    #[test]
    fn prepare_writes_merge_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let manifold_dir = dir.path().join(".manifold");

        let epoch = test_epoch();
        let ws = test_ws("worker-1");
        let mut heads = BTreeMap::new();
        heads.insert(ws.clone(), test_oid('d'));

        let sources = vec![ws.clone()];
        run_prepare_phase_with_epoch(&manifold_dir, epoch.clone(), &sources, heads).unwrap();

        // Verify file exists and contents
        let state_path = MergeStateFile::default_path(&manifold_dir);
        assert!(state_path.exists());

        let state = MergeStateFile::read(&state_path).unwrap();
        assert_eq!(state.phase, MergePhase::Prepare);
        assert_eq!(state.sources, vec![ws.clone()]);
        assert_eq!(state.epoch_before, epoch);
        assert_eq!(state.frozen_heads.len(), 1);
        assert_eq!(state.frozen_heads[&ws], test_oid('d'));
        assert!(state.epoch_candidate.is_none());
        assert!(state.validation_result.is_none());
    }

    #[test]
    fn prepare_rejects_empty_sources() {
        let dir = tempfile::tempdir().unwrap();
        let manifold_dir = dir.path().join(".manifold");

        let result =
            run_prepare_phase_with_epoch(&manifold_dir, test_epoch(), &[], BTreeMap::new());
        assert!(matches!(result, Err(PrepareError::NoSources)));
    }

    #[test]
    fn prepare_rejects_in_progress_merge() {
        let dir = tempfile::tempdir().unwrap();
        let manifold_dir = dir.path().join(".manifold");
        std::fs::create_dir_all(&manifold_dir).unwrap();

        // Write an in-progress merge-state
        let existing = MergeStateFile::new(vec![test_ws("old")], test_epoch(), 1000);
        let state_path = MergeStateFile::default_path(&manifold_dir);
        existing.write_atomic(&state_path).unwrap();

        // Try to prepare — should fail
        let mut heads = BTreeMap::new();
        heads.insert(test_ws("new"), test_oid('e'));
        let result =
            run_prepare_phase_with_epoch(&manifold_dir, test_epoch(), &[test_ws("new")], heads);
        assert!(matches!(result, Err(PrepareError::MergeAlreadyInProgress)));
    }

    #[test]
    fn prepare_overwrites_terminal_state() {
        let dir = tempfile::tempdir().unwrap();
        let manifold_dir = dir.path().join(".manifold");
        std::fs::create_dir_all(&manifold_dir).unwrap();

        // Write a completed merge-state
        let mut existing = MergeStateFile::new(vec![test_ws("old")], test_epoch(), 1000);
        existing.advance(MergePhase::Build, 1001).unwrap();
        existing.advance(MergePhase::Validate, 1002).unwrap();
        existing.advance(MergePhase::Commit, 1003).unwrap();
        existing.advance(MergePhase::Cleanup, 1004).unwrap();
        existing.advance(MergePhase::Complete, 1005).unwrap();
        let state_path = MergeStateFile::default_path(&manifold_dir);
        existing.write_atomic(&state_path).unwrap();

        // Prepare should succeed (overwrite terminal state)
        let ws = test_ws("new-ws");
        let mut heads = BTreeMap::new();
        heads.insert(ws.clone(), test_oid('f'));
        let frozen =
            run_prepare_phase_with_epoch(&manifold_dir, test_epoch(), &[ws.clone()], heads)
                .unwrap();

        assert_eq!(frozen.heads.len(), 1);

        // Verify state file was overwritten
        let state = MergeStateFile::read(&state_path).unwrap();
        assert_eq!(state.phase, MergePhase::Prepare);
        assert_eq!(state.sources, vec![ws]);
    }

    #[test]
    fn prepare_crash_safety_file_is_valid_or_absent() {
        let dir = tempfile::tempdir().unwrap();
        let manifold_dir = dir.path().join(".manifold");

        let ws = test_ws("crash-test");
        let mut heads = BTreeMap::new();
        heads.insert(ws.clone(), test_oid('a'));

        // Before prepare: no file
        let state_path = MergeStateFile::default_path(&manifold_dir);
        assert!(!state_path.exists());

        // After prepare: valid file
        run_prepare_phase_with_epoch(&manifold_dir, test_epoch(), &[ws], heads).unwrap();

        assert!(state_path.exists());
        let state = MergeStateFile::read(&state_path).unwrap();
        assert_eq!(state.phase, MergePhase::Prepare);
    }

    #[test]
    fn prepare_recovery_aborts_and_preserves_workspace_files() {
        let dir = tempfile::tempdir().unwrap();
        let manifold_dir = dir.path().join(".manifold");

        let ws = test_ws("worker-1");
        let ws_file = dir.path().join("ws").join("worker-1").join("result.txt");
        std::fs::create_dir_all(ws_file.parent().unwrap()).unwrap();
        std::fs::write(&ws_file, "worker output\n").unwrap();

        let mut heads = BTreeMap::new();
        heads.insert(ws.clone(), test_oid('c'));
        run_prepare_phase_with_epoch(&manifold_dir, test_epoch(), &[ws], heads).unwrap();

        let state_path = MergeStateFile::default_path(&manifold_dir);
        let outcome = recover_from_merge_state(&state_path).unwrap();
        assert_eq!(
            outcome,
            RecoveryOutcome::AbortedPreCommit {
                from: MergePhase::Prepare
            }
        );
        assert!(!state_path.exists());
        assert_eq!(std::fs::read_to_string(ws_file).unwrap(), "worker output\n");
    }

    #[test]
    fn prepare_creates_manifold_dir() {
        let dir = tempfile::tempdir().unwrap();
        let manifold_dir = dir.path().join("deep").join("nested").join(".manifold");

        let ws = test_ws("ws-1");
        let mut heads = BTreeMap::new();
        heads.insert(ws.clone(), test_oid('b'));

        run_prepare_phase_with_epoch(&manifold_dir, test_epoch(), &[ws], heads).unwrap();

        assert!(manifold_dir.exists());
    }

    #[test]
    fn prepare_records_correct_oids_for_multiple_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let manifold_dir = dir.path().join(".manifold");

        let ws1 = test_ws("ws-1");
        let ws2 = test_ws("ws-2");
        let ws3 = test_ws("ws-3");

        let mut heads = BTreeMap::new();
        heads.insert(ws1.clone(), test_oid('1'));
        heads.insert(ws2.clone(), test_oid('2'));
        heads.insert(ws3.clone(), test_oid('3'));

        let sources = vec![ws1.clone(), ws2.clone(), ws3.clone()];
        let frozen =
            run_prepare_phase_with_epoch(&manifold_dir, test_epoch(), &sources, heads).unwrap();

        // Verify each OID is correctly recorded
        assert_eq!(frozen.heads[&ws1].as_str(), &"1".repeat(40));
        assert_eq!(frozen.heads[&ws2].as_str(), &"2".repeat(40));
        assert_eq!(frozen.heads[&ws3].as_str(), &"3".repeat(40));

        // Verify persisted state matches
        let state_path = MergeStateFile::default_path(&manifold_dir);
        let state = MergeStateFile::read(&state_path).unwrap();
        assert_eq!(state.frozen_heads.len(), 3);
        assert_eq!(state.frozen_heads[&ws1].as_str(), &"1".repeat(40));
    }

    #[test]
    fn prepare_frozen_inputs_are_deterministic() {
        // Run PREPARE twice with same inputs → same frozen outputs
        for _ in 0..2 {
            let dir = tempfile::tempdir().unwrap();
            let manifold_dir = dir.path().join(".manifold");

            let epoch = test_epoch();
            let ws = test_ws("det-test");
            let mut heads = BTreeMap::new();
            heads.insert(ws.clone(), test_oid('d'));

            let frozen =
                run_prepare_phase_with_epoch(&manifold_dir, epoch.clone(), &[ws.clone()], heads)
                    .unwrap();

            assert_eq!(frozen.epoch, epoch);
            assert_eq!(frozen.heads[&ws], test_oid('d'));
        }
    }

    #[test]
    fn prepare_state_serialization_includes_frozen_heads() {
        let dir = tempfile::tempdir().unwrap();
        let manifold_dir = dir.path().join(".manifold");

        let ws = test_ws("serial-test");
        let mut heads = BTreeMap::new();
        heads.insert(ws.clone(), test_oid('e'));

        run_prepare_phase_with_epoch(&manifold_dir, test_epoch(), &[ws], heads).unwrap();

        // Read raw JSON and verify frozen_heads is present
        let state_path = MergeStateFile::default_path(&manifold_dir);
        let raw_json = std::fs::read_to_string(&state_path).unwrap();
        assert!(raw_json.contains("frozen_heads"));
        assert!(raw_json.contains(&"e".repeat(40)));
    }

    #[test]
    fn prepare_error_display() {
        let err = PrepareError::NoSources;
        assert!(format!("{err}").contains("no source workspaces"));

        let err = PrepareError::MergeAlreadyInProgress;
        assert!(format!("{err}").contains("already in progress"));

        let err = PrepareError::EpochNotFound("not found".to_owned());
        assert!(format!("{err}").contains("epoch ref not found"));

        let ws = test_ws("bad-ws");
        let err = PrepareError::WorkspaceHeadNotFound {
            workspace: ws,
            detail: "missing".to_owned(),
        };
        assert!(format!("{err}").contains("bad-ws"));
    }

    // ---------------------------------------------------------------------------
    // Stale post-commit recovery tests (require a real git repo)
    // ---------------------------------------------------------------------------

    fn run_git(root: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn git_oid(root: &Path, rev: &str) -> GitOid {
        let hex = run_git(root, &["rev-parse", rev]);
        GitOid::new(&hex).unwrap()
    }

    /// Create a minimal git repo with one commit and the epoch ref pointing to it.
    /// Returns (TempDir, initial_commit_oid).
    fn setup_git_repo_with_epoch() -> (tempfile::TempDir, GitOid) {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        run_git(root, &["init"]);
        run_git(root, &["config", "user.name", "Test"]);
        run_git(root, &["config", "user.email", "test@test.com"]);
        run_git(root, &["config", "commit.gpgsign", "false"]);
        run_git(root, &["commit", "--allow-empty", "-m", "initial"]);
        let initial = git_oid(root, "HEAD");
        run_git(root, &["update-ref", refs::EPOCH_CURRENT, initial.as_str()]);
        (dir, initial)
    }

    /// Build a stale merge-state.json at the given phase with epoch_candidate set.
    fn write_stale_commit_state(
        manifold_dir: &Path,
        epoch_before: &GitOid,
        epoch_candidate: &GitOid,
        phase: MergePhase,
        ws_name: &str,
    ) {
        std::fs::create_dir_all(manifold_dir).unwrap();
        let eb = EpochId::new(epoch_before.as_str()).unwrap();
        let mut state = MergeStateFile::new(vec![WorkspaceId::new(ws_name).unwrap()], eb, 1000);
        state.advance(MergePhase::Build, 1001).unwrap();
        state.advance(MergePhase::Validate, 1002).unwrap();
        state.advance(MergePhase::Commit, 1003).unwrap();
        state.epoch_candidate = Some(epoch_candidate.clone());
        if phase == MergePhase::Cleanup {
            state.advance(MergePhase::Cleanup, 1004).unwrap();
        }
        state
            .write_atomic(&MergeStateFile::default_path(manifold_dir))
            .unwrap();
    }

    #[test]
    fn prepare_clears_stale_commit_phase_when_epoch_already_advanced() {
        let (dir, epoch_before) = setup_git_repo_with_epoch();
        let root = dir.path();

        // Simulate a merge commit: new commit = epoch_candidate
        run_git(root, &["commit", "--allow-empty", "-m", "merge"]);
        let candidate = git_oid(root, "HEAD");

        // Add workspace as a git worktree
        let ws_name = "stale-ws";
        let ws_path = root.join(ws_name);
        run_git(root, &["worktree", "add", ws_path.to_str().unwrap(), "HEAD"]);

        let manifold_dir = root.join(".manifold");
        write_stale_commit_state(&manifold_dir, &epoch_before, &candidate, MergePhase::Commit, ws_name);

        // Advance epoch ref (previous merge completed its COMMIT)
        run_git(root, &["update-ref", refs::EPOCH_CURRENT, candidate.as_str()]);

        let ws_id = WorkspaceId::new(ws_name).unwrap();
        let mut workspace_dirs = BTreeMap::new();
        workspace_dirs.insert(ws_id.clone(), ws_path);

        // Should succeed: stale state is auto-cleared
        let result = run_prepare_phase(root, &manifold_dir, &[ws_id], &workspace_dirs);
        assert!(result.is_ok(), "expected success clearing stale state, got: {result:?}");

        let new_state = MergeStateFile::read(&MergeStateFile::default_path(&manifold_dir)).unwrap();
        assert_eq!(new_state.phase, MergePhase::Prepare);
    }

    #[test]
    fn prepare_clears_stale_cleanup_phase_when_epoch_already_advanced() {
        let (dir, epoch_before) = setup_git_repo_with_epoch();
        let root = dir.path();

        run_git(root, &["commit", "--allow-empty", "-m", "merge"]);
        let candidate = git_oid(root, "HEAD");

        let ws_name = "stale-ws2";
        let ws_path = root.join(ws_name);
        run_git(root, &["worktree", "add", ws_path.to_str().unwrap(), "HEAD"]);

        let manifold_dir = root.join(".manifold");
        write_stale_commit_state(&manifold_dir, &epoch_before, &candidate, MergePhase::Cleanup, ws_name);

        run_git(root, &["update-ref", refs::EPOCH_CURRENT, candidate.as_str()]);

        let ws_id = WorkspaceId::new(ws_name).unwrap();
        let mut workspace_dirs = BTreeMap::new();
        workspace_dirs.insert(ws_id.clone(), ws_path);

        let result = run_prepare_phase(root, &manifold_dir, &[ws_id], &workspace_dirs);
        assert!(result.is_ok(), "expected success clearing stale cleanup state, got: {result:?}");
    }

    #[test]
    fn prepare_blocks_genuine_in_progress_commit_phase() {
        let (dir, epoch_before) = setup_git_repo_with_epoch();
        let root = dir.path();

        run_git(root, &["commit", "--allow-empty", "-m", "in-flight merge"]);
        let candidate = git_oid(root, "HEAD");

        let ws_name = "active-ws";
        let ws_path = root.join(ws_name);
        run_git(root, &["worktree", "add", ws_path.to_str().unwrap(), "HEAD"]);

        let manifold_dir = root.join(".manifold");
        write_stale_commit_state(&manifold_dir, &epoch_before, &candidate, MergePhase::Commit, ws_name);

        // Epoch ref is still at epoch_before (commit hasn't completed yet)
        // epoch_before is still in refs/manifold/epoch/current from setup

        let ws_id = WorkspaceId::new(ws_name).unwrap();
        let mut workspace_dirs = BTreeMap::new();
        workspace_dirs.insert(ws_id.clone(), ws_path);

        let result = run_prepare_phase(root, &manifold_dir, &[ws_id], &workspace_dirs);
        assert!(
            matches!(result, Err(PrepareError::MergeAlreadyInProgress)),
            "expected MergeAlreadyInProgress, got: {result:?}"
        );
    }
}
