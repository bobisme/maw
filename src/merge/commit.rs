#![allow(clippy::missing_errors_doc)]

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::model::types::GitOid;
use crate::refs::{self, RefError};

/// Commit-phase state persistence path relative to the repo root.
///
/// This is intentionally distinct from `.manifold/merge-state.json` used by
/// the main merge state machine (`merge_state.rs`).
const MERGE_STATE_REL_PATH: &str = ".manifold/commit-state.json";

/// Result of running the COMMIT phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitResult {
    /// Both refs moved to the candidate commit.
    Committed,
}

/// Recovery result for a partially-applied commit phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitRecovery {
    /// Both refs already point at the candidate commit.
    AlreadyCommitted,
    /// Epoch ref was moved and main ref was finalized during recovery.
    FinalizedMainRef,
    /// Neither ref moved yet.
    NotCommitted,
}

/// COMMIT phase and merge-state errors.
#[derive(Debug)]
pub enum CommitError {
    Ref(RefError),
    Io(std::io::Error),
    Serde(serde_json::Error),
    /// Epoch was advanced but branch ref update failed.
    /// This is recoverable by calling [`recover_partial_commit`].
    PartialCommit,
    /// Ref state does not match any expected crash-recovery shape.
    InconsistentRefState {
        epoch: Option<GitOid>,
        branch: Option<GitOid>,
    },
    /// Injected failpoint fired during commit phase.
    #[cfg(feature = "failpoints")]
    Failpoint(String),
}

impl std::fmt::Display for CommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ref(e) => write!(f, "ref update failed: {e}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Serde(e) => write!(f, "merge-state JSON error: {e}"),
            Self::PartialCommit => write!(
                f,
                "commit phase partially applied: epoch ref moved but branch ref did not"
            ),
            Self::InconsistentRefState { epoch, branch } => write!(
                f,
                "inconsistent ref state during commit recovery (epoch={epoch:?}, branch={branch:?})"
            ),
            #[cfg(feature = "failpoints")]
            Self::Failpoint(msg) => write!(f, "failpoint: {msg}"),
        }
    }
}

impl std::error::Error for CommitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Ref(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::Serde(e) => Some(e),
            Self::PartialCommit | Self::InconsistentRefState { .. } => None,
            #[cfg(feature = "failpoints")]
            Self::Failpoint(_) => None,
        }
    }
}

impl From<RefError> for CommitError {
    fn from(value: RefError) -> Self {
        Self::Ref(value)
    }
}

impl From<std::io::Error> for CommitError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for CommitError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serde(value)
    }
}

/// Persisted merge-state for COMMIT phase recovery.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitStateFile {
    pub phase: CommitPhase,
    pub epoch_before: GitOid,
    pub epoch_candidate: GitOid,
    pub epoch_ref_updated: bool,
    pub branch_ref_updated: bool,
    pub updated_at_unix_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommitPhase {
    Commit,
    Committed,
}

/// Run COMMIT phase:
/// 1. CAS move `refs/manifold/epoch/current` old->new
/// 2. CAS move `refs/heads/<branch>` old->new
/// 3. Persist merge-state at each transition (atomic write + fsync)
pub fn run_commit_phase(
    root: &Path,
    branch: &str,
    epoch_before: &GitOid,
    epoch_candidate: &GitOid,
) -> Result<CommitResult, CommitError> {
    let mut state = CommitStateFile {
        phase: CommitPhase::Commit,
        epoch_before: epoch_before.clone(),
        epoch_candidate: epoch_candidate.clone(),
        epoch_ref_updated: false,
        branch_ref_updated: false,
        updated_at_unix_ms: now_unix_ms(),
    };

    write_merge_state(root, &state)?;

    // FP: crash before the atomic CAS that moves epoch + branch refs.
    fp_commit("FP_COMMIT_BEFORE_BRANCH_CAS")?;

    let branch_ref = format!("refs/heads/{branch}");
    refs::update_refs_atomic(
        root,
        &[
            (refs::EPOCH_CURRENT, epoch_before, epoch_candidate),
            (&branch_ref, epoch_before, epoch_candidate),
        ],
    )?;

    // FP: crash after epoch ref moved — HIGHEST risk point: refs advanced
    // but commit-state.json still says "Commit".
    fp_commit("FP_COMMIT_BETWEEN_CAS_OPS")?;

    // FP: crash after CAS, before final state persistence.
    fp_commit("FP_COMMIT_AFTER_EPOCH_CAS")?;

    state.phase = CommitPhase::Committed;
    state.epoch_ref_updated = true;
    state.branch_ref_updated = true;
    state.updated_at_unix_ms = now_unix_ms();
    write_merge_state(root, &state)?;

    Ok(CommitResult::Committed)
}

/// Crash-recover COMMIT phase by inspecting refs and finalizing if safe.
pub fn recover_partial_commit(
    root: &Path,
    branch: &str,
    epoch_before: &GitOid,
    epoch_candidate: &GitOid,
) -> Result<CommitRecovery, CommitError> {
    let branch_ref = format!("refs/heads/{branch}");
    let epoch = refs::read_ref(root, refs::EPOCH_CURRENT)?;
    let branch_head = refs::read_ref(root, &branch_ref)?;

    if epoch.as_ref() == Some(epoch_candidate) && branch_head.as_ref() == Some(epoch_candidate) {
        return Ok(CommitRecovery::AlreadyCommitted);
    }

    if epoch.as_ref() == Some(epoch_candidate) && branch_head.as_ref() == Some(epoch_before) {
        refs::write_ref_cas(root, &branch_ref, epoch_before, epoch_candidate)?;

        let state = CommitStateFile {
            phase: CommitPhase::Committed,
            epoch_before: epoch_before.clone(),
            epoch_candidate: epoch_candidate.clone(),
            epoch_ref_updated: true,
            branch_ref_updated: true,
            updated_at_unix_ms: now_unix_ms(),
        };
        write_merge_state(root, &state)?;

        return Ok(CommitRecovery::FinalizedMainRef);
    }

    if epoch.as_ref() == Some(epoch_before) && branch_head.as_ref() == Some(epoch_before) {
        return Ok(CommitRecovery::NotCommitted);
    }

    Err(CommitError::InconsistentRefState {
        epoch,
        branch: branch_head,
    })
}

pub fn read_merge_state(root: &Path) -> Result<CommitStateFile, CommitError> {
    let path = merge_state_path(root);
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn merge_state_path(root: &Path) -> PathBuf {
    root.join(MERGE_STATE_REL_PATH)
}

fn write_merge_state(root: &Path, state: &CommitStateFile) -> Result<(), CommitError> {
    let path = merge_state_path(root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let tmp = path.with_extension("tmp");
    let data = serde_json::to_vec_pretty(state)?;

    let mut file = File::create(&tmp)?;
    file.write_all(&data)?;
    file.write_all(b"\n")?;
    file.sync_all()?;

    fs::rename(&tmp, &path)?;

    if let Some(parent) = path.parent() {
        // Fsync parent directory so the rename is durable across power loss.
        let dir = File::open(parent)?;
        dir.sync_all()?;
    }

    Ok(())
}

/// Invoke a failpoint and convert the result to [`CommitError`].
///
/// Without the `failpoints` feature this compiles to a no-op.
const fn fp_commit(_name: &str) -> Result<(), CommitError> {
    #[cfg(feature = "failpoints")]
    {
        crate::fp!(_name).map_err(|e| CommitError::Failpoint(e.to_string()))?;
    }
    Ok(())
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    fn run_git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn setup_repo_with_main() -> (TempDir, GitOid, GitOid) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        run_git(root, &["init"]);
        run_git(root, &["config", "user.name", "Test"]);
        run_git(root, &["config", "user.email", "test@test.com"]);
        run_git(root, &["config", "commit.gpgsign", "false"]);

        fs::write(root.join("README.md"), "hello\n").unwrap();
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", "initial"]);
        run_git(root, &["branch", "-M", "main"]);

        let old = git_oid(root, "HEAD");

        fs::write(root.join("README.md"), "hello world\n").unwrap();
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", "candidate"]);

        let new = git_oid(root, "HEAD");

        // Reset branch and epoch ref to old so COMMIT phase can advance both.
        run_git(root, &["update-ref", "refs/heads/main", old.as_str()]);
        run_git(root, &["update-ref", refs::EPOCH_CURRENT, old.as_str()]);

        (dir, old, new)
    }

    fn git_oid(root: &Path, rev: &str) -> GitOid {
        let out = Command::new("git")
            .args(["rev-parse", rev])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "rev-parse {rev} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        GitOid::new(String::from_utf8_lossy(&out.stdout).trim()).unwrap()
    }

    fn assert_repo_usable(root: &Path) {
        run_git(root, &["fsck", "--no-progress", "--connectivity-only"]);
    }

    fn assert_commit_exists(root: &Path, oid: &GitOid) {
        run_git(
            root,
            &["cat-file", "-e", &format!("{}^{{commit}}", oid.as_str())],
        );
    }

    #[test]
    fn commit_phase_updates_epoch_and_main() {
        let (dir, old, new) = setup_repo_with_main();
        let root = dir.path();

        let result = run_commit_phase(root, "main", &old, &new).unwrap();
        assert_eq!(result, CommitResult::Committed);

        let epoch = refs::read_ref(root, refs::EPOCH_CURRENT).unwrap();
        let main = refs::read_ref(root, "refs/heads/main").unwrap();
        assert_eq!(epoch, Some(new.clone()));
        assert_eq!(main, Some(new.clone()));

        let state = read_merge_state(root).unwrap();
        assert_eq!(state.phase, CommitPhase::Committed);
        assert!(state.epoch_ref_updated);
        assert!(state.branch_ref_updated);

        assert_repo_usable(root);
        assert_commit_exists(root, &old);
        assert_commit_exists(root, &new);
    }

    #[test]
    fn recovery_finalizes_when_only_epoch_moved() {
        let (dir, old, new) = setup_repo_with_main();
        let root = dir.path();

        refs::advance_epoch(root, &old, &new).unwrap();

        let recovery = recover_partial_commit(root, "main", &old, &new).unwrap();
        assert_eq!(recovery, CommitRecovery::FinalizedMainRef);

        let main = refs::read_ref(root, "refs/heads/main").unwrap();
        assert_eq!(main, Some(new.clone()));

        assert_repo_usable(root);
        assert_commit_exists(root, &old);
        assert_commit_exists(root, &new);
    }

    #[test]
    fn recovery_reports_already_committed_when_both_refs_new() {
        let (dir, old, new) = setup_repo_with_main();
        let root = dir.path();

        run_git(root, &["update-ref", refs::EPOCH_CURRENT, new.as_str()]);
        run_git(root, &["update-ref", "refs/heads/main", new.as_str()]);

        let recovery = recover_partial_commit(root, "main", &old, &new).unwrap();
        assert_eq!(recovery, CommitRecovery::AlreadyCommitted);

        assert_repo_usable(root);
        assert_commit_exists(root, &old);
        assert_commit_exists(root, &new);
    }

    #[test]
    fn recovery_reports_not_committed_when_both_refs_old() {
        let (dir, old, new) = setup_repo_with_main();
        let root = dir.path();

        let recovery = recover_partial_commit(root, "main", &old, &new).unwrap();
        assert_eq!(recovery, CommitRecovery::NotCommitted);

        assert_repo_usable(root);
        assert_commit_exists(root, &old);
        assert_commit_exists(root, &new);
    }
}
