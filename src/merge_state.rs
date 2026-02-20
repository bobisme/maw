//! Merge state machine and persisted merge-state file.
//!
//! The merge state is persisted to `.manifold/merge-state.json` as
//! human-readable JSON. Every write is atomic (write-to-temp + fsync +
//! rename) so a crash never corrupts the file.
//!
//! # Lifecycle
//!
//! ```text
//! Prepare → Build → Validate → Commit → Cleanup → Complete
//!                                  │
//!                                  └→ Aborted
//! ```
//!
//! Any phase can also transition to `Aborted` on unrecoverable error.

#![allow(clippy::missing_errors_doc)]

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::types::{EpochId, GitOid, WorkspaceId};

// ---------------------------------------------------------------------------
// MergePhase
// ---------------------------------------------------------------------------

/// The current phase of the merge state machine.
///
/// Phases progress strictly forward: `Prepare → Build → Validate → Commit →
/// Cleanup → Complete`. The `Aborted` state can be entered from any phase
/// except `Complete`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergePhase {
    /// Freeze inputs and write merge intent.
    Prepare,
    /// Build the merged tree from collected workspace snapshots.
    Build,
    /// Run validation commands against the candidate commit.
    Validate,
    /// Atomically update refs (point of no return).
    Commit,
    /// Post-commit cleanup (remove temp files, update workspace state).
    Cleanup,
    /// Merge completed successfully.
    Complete,
    /// Merge aborted — may include a reason.
    Aborted,
}

impl MergePhase {
    /// Returns `true` if this is a terminal state (`Complete` or `Aborted`).
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Complete | Self::Aborted)
    }

    /// Returns the set of valid next phases from this phase.
    ///
    /// `Aborted` can be reached from any non-terminal phase.
    #[must_use]
    pub const fn valid_transitions(&self) -> &'static [Self] {
        match self {
            Self::Prepare => &[Self::Build, Self::Aborted],
            Self::Build => &[Self::Validate, Self::Aborted],
            Self::Validate => &[Self::Commit, Self::Aborted],
            Self::Commit => &[Self::Cleanup, Self::Aborted],
            Self::Cleanup => &[Self::Complete, Self::Aborted],
            Self::Complete | Self::Aborted => &[],
        }
    }

    /// Check whether transitioning to `next` is valid.
    #[must_use]
    pub fn can_transition_to(&self, next: &Self) -> bool {
        self.valid_transitions().contains(next)
    }
}

impl fmt::Display for MergePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prepare => write!(f, "prepare"),
            Self::Build => write!(f, "build"),
            Self::Validate => write!(f, "validate"),
            Self::Commit => write!(f, "commit"),
            Self::Cleanup => write!(f, "cleanup"),
            Self::Complete => write!(f, "complete"),
            Self::Aborted => write!(f, "aborted"),
        }
    }
}

// ---------------------------------------------------------------------------
// ValidationResult
// ---------------------------------------------------------------------------

/// The result of a single validation command execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandResult {
    /// The command string that was executed.
    pub command: String,
    /// Whether this command passed (exit code 0).
    pub passed: bool,
    /// Exit code (`None` if killed by signal/timeout).
    pub exit_code: Option<i32>,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

/// The outcome of a post-merge validation run.
///
/// When multiple commands are configured, `command_results` contains the
/// per-command outcomes. The top-level fields (`passed`, `exit_code`, etc.)
/// summarize the overall result: `passed` is true only if all commands
/// passed, and `exit_code`/`stdout`/`stderr` reflect the first failing
/// command (or the last command if all passed).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationResult {
    /// Whether the validation passed (all commands exited 0).
    pub passed: bool,
    /// Exit code of the relevant command (`None` if killed by signal/timeout).
    /// For multi-command runs, this is the exit code of the first failing
    /// command, or the exit code of the last command if all passed.
    pub exit_code: Option<i32>,
    /// Captured stdout (from the relevant command).
    pub stdout: String,
    /// Captured stderr (from the relevant command).
    pub stderr: String,
    /// Total wall-clock duration in milliseconds (sum of all commands).
    pub duration_ms: u64,
    /// Per-command results (empty for single-command validation, for
    /// backward compatibility with older merge-state files).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command_results: Vec<CommandResult>,
}

// ---------------------------------------------------------------------------
// MergeStateFile
// ---------------------------------------------------------------------------

/// The persisted merge-state file.
///
/// Written to `.manifold/merge-state.json`. Every mutation is fsynced to
/// disk so a crash always leaves a valid, recoverable file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeStateFile {
    /// Current merge phase.
    pub phase: MergePhase,

    /// Source workspaces being merged.
    pub sources: Vec<WorkspaceId>,

    /// The epoch before this merge started.
    pub epoch_before: EpochId,

    /// Frozen workspace HEAD commit OIDs, recorded during PREPARE.
    ///
    /// Maps each source workspace to its HEAD at the time inputs were frozen.
    /// After PREPARE, these are immutable references — the merge operates on
    /// these exact commits regardless of any concurrent workspace activity.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub frozen_heads: BTreeMap<WorkspaceId, GitOid>,

    /// The candidate commit produced during BUILD (set in Build phase).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch_candidate: Option<GitOid>,

    /// The validation result (set in Validate phase).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_result: Option<ValidationResult>,

    /// The new epoch after COMMIT (set in Commit phase).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch_after: Option<EpochId>,

    /// Unix timestamp (seconds) when the merge started.
    pub started_at: u64,

    /// Unix timestamp (seconds) of the last state update.
    pub updated_at: u64,

    /// Abort reason, if the merge was aborted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort_reason: Option<String>,
}

impl MergeStateFile {
    /// Create a new merge-state file in the `Prepare` phase.
    ///
    /// # Arguments
    /// * `sources` - The workspaces being merged.
    /// * `epoch_before` - The current epoch at the start of the merge.
    /// * `now` - The current Unix timestamp in seconds.
    #[must_use]
    pub const fn new(sources: Vec<WorkspaceId>, epoch_before: EpochId, now: u64) -> Self {
        Self {
            phase: MergePhase::Prepare,
            sources,
            epoch_before,
            frozen_heads: BTreeMap::new(),
            epoch_candidate: None,
            validation_result: None,
            epoch_after: None,
            started_at: now,
            updated_at: now,
            abort_reason: None,
        }
    }

    /// Advance to the next phase, updating the timestamp.
    ///
    /// # Errors
    /// Returns [`MergeStateError::InvalidTransition`] if the transition is
    /// not allowed.
    pub fn advance(&mut self, next: MergePhase, now: u64) -> Result<(), MergeStateError> {
        if !self.phase.can_transition_to(&next) {
            return Err(MergeStateError::InvalidTransition {
                from: self.phase.clone(),
                to: next,
            });
        }
        self.phase = next;
        self.updated_at = now;
        Ok(())
    }

    /// Abort the merge with a reason.
    ///
    /// # Errors
    /// Returns [`MergeStateError::InvalidTransition`] if the merge is
    /// already in a terminal state.
    pub fn abort(&mut self, reason: impl Into<String>, now: u64) -> Result<(), MergeStateError> {
        if self.phase.is_terminal() {
            return Err(MergeStateError::InvalidTransition {
                from: self.phase.clone(),
                to: MergePhase::Aborted,
            });
        }
        self.phase = MergePhase::Aborted;
        self.abort_reason = Some(reason.into());
        self.updated_at = now;
        Ok(())
    }

    /// Serialize to pretty-printed JSON.
    ///
    /// # Errors
    /// Returns [`MergeStateError::Serialize`] on serialization failure.
    pub fn to_json(&self) -> Result<String, MergeStateError> {
        serde_json::to_string_pretty(self).map_err(|e| MergeStateError::Serialize(e.to_string()))
    }

    /// Deserialize from a JSON string.
    ///
    /// # Errors
    /// Returns [`MergeStateError::Deserialize`] on parse failure.
    pub fn from_json(json: &str) -> Result<Self, MergeStateError> {
        serde_json::from_str(json).map_err(|e| MergeStateError::Deserialize(e.to_string()))
    }

    /// Write the merge-state file atomically with fsync.
    ///
    /// 1. Serialize to pretty JSON.
    /// 2. Write to a temporary file in the same directory.
    /// 3. fsync the temporary file.
    /// 4. Rename (atomic on POSIX) over the target path.
    ///
    /// # Errors
    /// Returns [`MergeStateError`] on I/O or serialization failure.
    pub fn write_atomic(&self, path: &Path) -> Result<(), MergeStateError> {
        let json = self.to_json()?;

        let dir = path.parent().ok_or_else(|| {
            MergeStateError::Io(format!("no parent directory for {}", path.display()))
        })?;

        // Write to a temporary file in the same directory (ensures same filesystem)
        let tmp_path = dir.join(".merge-state.tmp");
        let mut file = fs::File::create(&tmp_path)
            .map_err(|e| MergeStateError::Io(format!("create {}: {e}", tmp_path.display())))?;
        file.write_all(json.as_bytes())
            .map_err(|e| MergeStateError::Io(format!("write {}: {e}", tmp_path.display())))?;
        file.sync_all()
            .map_err(|e| MergeStateError::Io(format!("fsync {}: {e}", tmp_path.display())))?;
        drop(file);

        // Atomic rename
        fs::rename(&tmp_path, path).map_err(|e| {
            MergeStateError::Io(format!(
                "rename {} → {}: {e}",
                tmp_path.display(),
                path.display()
            ))
        })?;

        Ok(())
    }

    /// Read a merge-state file from disk.
    ///
    /// # Errors
    /// Returns [`MergeStateError::NotFound`] if the file does not exist.
    /// Returns [`MergeStateError::Deserialize`] if the file is malformed.
    pub fn read(path: &Path) -> Result<Self, MergeStateError> {
        let contents = fs::read_to_string(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                MergeStateError::NotFound(path.to_owned())
            } else {
                MergeStateError::Io(format!("read {}: {e}", path.display()))
            }
        })?;
        Self::from_json(&contents)
    }

    /// Return the default merge-state file path for a `.manifold/` directory.
    #[must_use]
    pub fn default_path(manifold_dir: &Path) -> PathBuf {
        manifold_dir.join("merge-state.json")
    }
}

// ---------------------------------------------------------------------------
// Cleanup + recovery helpers (bd-1lpe.6)
// ---------------------------------------------------------------------------

/// Outcome of crash-recovery dispatch for an interrupted merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// No merge-state file was found.
    NoMergeInProgress,
    /// PREPARE/BUILD were safely aborted by deleting merge-state.
    AbortedPreCommit { from: MergePhase },
    /// VALIDATE should be re-run with frozen inputs.
    RetryValidate,
    /// COMMIT must inspect refs to decide finalize-vs-abort.
    CheckCommit,
    /// CLEANUP should be re-run (idempotent).
    RetryCleanup,
    /// Merge is already terminal; no recovery work needed.
    Terminal { phase: MergePhase },
}

/// Determine recovery behavior from persisted merge-state.
#[must_use]
pub fn recovery_outcome_for_phase(phase: &MergePhase) -> RecoveryOutcome {
    match phase {
        MergePhase::Prepare | MergePhase::Build => RecoveryOutcome::AbortedPreCommit {
            from: phase.clone(),
        },
        MergePhase::Validate => RecoveryOutcome::RetryValidate,
        MergePhase::Commit => RecoveryOutcome::CheckCommit,
        MergePhase::Cleanup => RecoveryOutcome::RetryCleanup,
        MergePhase::Complete | MergePhase::Aborted => RecoveryOutcome::Terminal {
            phase: phase.clone(),
        },
    }
}

/// Execute crash-recovery dispatch from a merge-state file.
///
/// Behavior matches design doc §5.10:
/// - PREPARE/BUILD: abort by removing merge-state
/// - VALIDATE: re-run validation
/// - COMMIT: check refs externally to decide finalize vs abort
/// - CLEANUP: re-run cleanup
pub fn recover_from_merge_state(
    merge_state_path: &Path,
) -> Result<RecoveryOutcome, MergeStateError> {
    let state = match MergeStateFile::read(merge_state_path) {
        Ok(s) => s,
        Err(MergeStateError::NotFound(_)) => return Ok(RecoveryOutcome::NoMergeInProgress),
        Err(e) => return Err(e),
    };

    let outcome = recovery_outcome_for_phase(&state.phase);
    if matches!(
        outcome,
        RecoveryOutcome::AbortedPreCommit { .. } | RecoveryOutcome::RetryCleanup
    ) {
        // Safe and idempotent for PREPARE/BUILD abort and post-commit cleanup completion.
        remove_merge_state_if_exists(merge_state_path)?;
    }

    Ok(outcome)
}

/// Cleanup phase helper:
/// - optionally destroys source workspaces via callback
/// - removes merge-state file
///
/// The operation is idempotent. Re-running it is safe.
pub fn run_cleanup_phase<D>(
    state: &MergeStateFile,
    merge_state_path: &Path,
    destroy_workspaces: bool,
    mut destroy_workspace: D,
) -> Result<(), MergeStateError>
where
    D: FnMut(&WorkspaceId) -> Result<(), MergeStateError>,
{
    if destroy_workspaces {
        for workspace in &state.sources {
            destroy_workspace(workspace)?;
        }
    }

    remove_merge_state_if_exists(merge_state_path)
}

fn remove_merge_state_if_exists(path: &Path) -> Result<(), MergeStateError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(MergeStateError::Io(format!(
            "remove {}: {e}",
            path.display()
        ))),
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors related to merge-state operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeStateError {
    /// Invalid phase transition.
    InvalidTransition {
        /// The current phase.
        from: MergePhase,
        /// The attempted target phase.
        to: MergePhase,
    },
    /// The merge-state file was not found.
    NotFound(PathBuf),
    /// Serialization error.
    Serialize(String),
    /// Deserialization error.
    Deserialize(String),
    /// I/O error (not "not found").
    Io(String),
}

impl fmt::Display for MergeStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTransition { from, to } => {
                write!(f, "invalid merge phase transition: {from} → {to}")
            }
            Self::NotFound(path) => {
                write!(f, "merge-state file not found: {}", path.display())
            }
            Self::Serialize(msg) => write!(f, "merge-state serialize error: {msg}"),
            Self::Deserialize(msg) => write!(f, "merge-state deserialize error: {msg}"),
            Self::Io(msg) => write!(f, "merge-state I/O error: {msg}"),
        }
    }
}

impl std::error::Error for MergeStateError {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
mod tests {
    use super::*;

    fn test_epoch() -> EpochId {
        EpochId::new(&"a".repeat(40)).unwrap()
    }

    fn test_oid() -> GitOid {
        GitOid::new(&"b".repeat(40)).unwrap()
    }

    fn test_sources() -> Vec<WorkspaceId> {
        vec![
            WorkspaceId::new("agent-1").unwrap(),
            WorkspaceId::new("agent-2").unwrap(),
        ]
    }

    // -- MergePhase --

    #[test]
    fn phase_display() {
        assert_eq!(MergePhase::Prepare.to_string(), "prepare");
        assert_eq!(MergePhase::Build.to_string(), "build");
        assert_eq!(MergePhase::Validate.to_string(), "validate");
        assert_eq!(MergePhase::Commit.to_string(), "commit");
        assert_eq!(MergePhase::Cleanup.to_string(), "cleanup");
        assert_eq!(MergePhase::Complete.to_string(), "complete");
        assert_eq!(MergePhase::Aborted.to_string(), "aborted");
    }

    #[test]
    fn phase_is_terminal() {
        assert!(!MergePhase::Prepare.is_terminal());
        assert!(!MergePhase::Build.is_terminal());
        assert!(!MergePhase::Validate.is_terminal());
        assert!(!MergePhase::Commit.is_terminal());
        assert!(!MergePhase::Cleanup.is_terminal());
        assert!(MergePhase::Complete.is_terminal());
        assert!(MergePhase::Aborted.is_terminal());
    }

    #[test]
    fn phase_valid_transitions() {
        // Happy path
        assert!(MergePhase::Prepare.can_transition_to(&MergePhase::Build));
        assert!(MergePhase::Build.can_transition_to(&MergePhase::Validate));
        assert!(MergePhase::Validate.can_transition_to(&MergePhase::Commit));
        assert!(MergePhase::Commit.can_transition_to(&MergePhase::Cleanup));
        assert!(MergePhase::Cleanup.can_transition_to(&MergePhase::Complete));

        // Abort from any non-terminal
        assert!(MergePhase::Prepare.can_transition_to(&MergePhase::Aborted));
        assert!(MergePhase::Build.can_transition_to(&MergePhase::Aborted));
        assert!(MergePhase::Validate.can_transition_to(&MergePhase::Aborted));
        assert!(MergePhase::Commit.can_transition_to(&MergePhase::Aborted));
        assert!(MergePhase::Cleanup.can_transition_to(&MergePhase::Aborted));
    }

    #[test]
    fn phase_invalid_transitions() {
        // Can't skip phases
        assert!(!MergePhase::Prepare.can_transition_to(&MergePhase::Validate));
        assert!(!MergePhase::Prepare.can_transition_to(&MergePhase::Complete));
        assert!(!MergePhase::Build.can_transition_to(&MergePhase::Commit));

        // Can't go backwards
        assert!(!MergePhase::Build.can_transition_to(&MergePhase::Prepare));
        assert!(!MergePhase::Complete.can_transition_to(&MergePhase::Cleanup));

        // Terminal states go nowhere
        assert!(!MergePhase::Complete.can_transition_to(&MergePhase::Aborted));
        assert!(!MergePhase::Aborted.can_transition_to(&MergePhase::Prepare));
    }

    #[test]
    fn phase_serde_roundtrip() {
        let phases = vec![
            MergePhase::Prepare,
            MergePhase::Build,
            MergePhase::Validate,
            MergePhase::Commit,
            MergePhase::Cleanup,
            MergePhase::Complete,
            MergePhase::Aborted,
        ];
        for phase in phases {
            let json = serde_json::to_string(&phase).unwrap();
            let decoded: MergePhase = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, phase, "roundtrip failed for {phase}");
        }
    }

    #[test]
    fn phase_serde_snake_case() {
        let json = serde_json::to_string(&MergePhase::Prepare).unwrap();
        assert_eq!(json, "\"prepare\"");
    }

    // -- MergeStateFile --

    #[test]
    fn new_state_is_prepare() {
        let state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        assert_eq!(state.phase, MergePhase::Prepare);
        assert_eq!(state.sources.len(), 2);
        assert_eq!(state.started_at, 1000);
        assert_eq!(state.updated_at, 1000);
        assert!(state.epoch_candidate.is_none());
        assert!(state.validation_result.is_none());
        assert!(state.epoch_after.is_none());
        assert!(state.abort_reason.is_none());
    }

    #[test]
    fn advance_happy_path() {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);

        state.advance(MergePhase::Build, 1001).unwrap();
        assert_eq!(state.phase, MergePhase::Build);
        assert_eq!(state.updated_at, 1001);

        state.advance(MergePhase::Validate, 1002).unwrap();
        assert_eq!(state.phase, MergePhase::Validate);

        state.advance(MergePhase::Commit, 1003).unwrap();
        assert_eq!(state.phase, MergePhase::Commit);

        state.advance(MergePhase::Cleanup, 1004).unwrap();
        assert_eq!(state.phase, MergePhase::Cleanup);

        state.advance(MergePhase::Complete, 1005).unwrap();
        assert_eq!(state.phase, MergePhase::Complete);
        assert_eq!(state.updated_at, 1005);
    }

    #[test]
    fn advance_invalid_transition() {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        let err = state.advance(MergePhase::Validate, 1001).unwrap_err();
        assert!(matches!(err, MergeStateError::InvalidTransition { .. }));
        // Phase should not change on error
        assert_eq!(state.phase, MergePhase::Prepare);
    }

    #[test]
    fn advance_from_terminal_fails() {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.advance(MergePhase::Build, 1001).unwrap();
        state.advance(MergePhase::Validate, 1002).unwrap();
        state.advance(MergePhase::Commit, 1003).unwrap();
        state.advance(MergePhase::Cleanup, 1004).unwrap();
        state.advance(MergePhase::Complete, 1005).unwrap();

        let err = state.advance(MergePhase::Aborted, 1006).unwrap_err();
        assert!(matches!(err, MergeStateError::InvalidTransition { .. }));
    }

    #[test]
    fn abort_from_prepare() {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.abort("test abort", 1001).unwrap();
        assert_eq!(state.phase, MergePhase::Aborted);
        assert_eq!(state.abort_reason.as_deref(), Some("test abort"));
        assert_eq!(state.updated_at, 1001);
    }

    #[test]
    fn abort_from_build() {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.advance(MergePhase::Build, 1001).unwrap();
        state.abort("build failed", 1002).unwrap();
        assert_eq!(state.phase, MergePhase::Aborted);
        assert_eq!(state.abort_reason.as_deref(), Some("build failed"));
    }

    #[test]
    fn abort_from_terminal_fails() {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.abort("first abort", 1001).unwrap();
        let err = state.abort("double abort", 1002).unwrap_err();
        assert!(matches!(err, MergeStateError::InvalidTransition { .. }));
    }

    // -- JSON serialization --

    #[test]
    fn json_roundtrip_prepare() {
        let state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        let json = state.to_json().unwrap();
        let decoded = MergeStateFile::from_json(&json).unwrap();
        assert_eq!(decoded, state);
    }

    #[test]
    fn json_roundtrip_with_optional_fields() {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.advance(MergePhase::Build, 1001).unwrap();
        state.epoch_candidate = Some(test_oid());
        state.advance(MergePhase::Validate, 1002).unwrap();
        state.validation_result = Some(ValidationResult {
            passed: true,
            exit_code: Some(0),
            stdout: "ok".to_owned(),
            stderr: String::new(),
            duration_ms: 1500,
            command_results: Vec::new(),
        });
        state.advance(MergePhase::Commit, 1003).unwrap();
        state.epoch_after = Some(EpochId::new(&"c".repeat(40)).unwrap());

        let json = state.to_json().unwrap();
        let decoded = MergeStateFile::from_json(&json).unwrap();
        assert_eq!(decoded, state);
    }

    #[test]
    fn json_is_pretty_printed() {
        let state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        let json = state.to_json().unwrap();
        // Pretty-printed JSON has newlines
        assert!(json.contains('\n'));
        // Contains indentation
        assert!(json.contains("  "));
    }

    #[test]
    fn json_omits_none_fields() {
        let state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        let json = state.to_json().unwrap();
        assert!(!json.contains("epoch_candidate"));
        assert!(!json.contains("validation_result"));
        assert!(!json.contains("epoch_after"));
        assert!(!json.contains("abort_reason"));
    }

    #[test]
    fn json_includes_some_fields() {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.advance(MergePhase::Build, 1001).unwrap();
        state.epoch_candidate = Some(test_oid());
        let json = state.to_json().unwrap();
        assert!(json.contains("epoch_candidate"));
        assert!(json.contains(&"b".repeat(40)));
    }

    #[test]
    fn json_deserialize_invalid() {
        let err = MergeStateFile::from_json("not json").unwrap_err();
        assert!(matches!(err, MergeStateError::Deserialize(_)));
    }

    // -- Atomic file I/O --

    #[test]
    fn write_and_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = MergeStateFile::default_path(dir.path());

        let state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.write_atomic(&path).unwrap();

        let loaded = MergeStateFile::read(&path).unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn write_overwrite_preserves_atomicity() {
        let dir = tempfile::tempdir().unwrap();
        let path = MergeStateFile::default_path(dir.path());

        // Write initial state
        let state1 = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state1.write_atomic(&path).unwrap();

        // Overwrite with advanced state
        let mut state2 = MergeStateFile::new(test_sources(), test_epoch(), 2000);
        state2.advance(MergePhase::Build, 2001).unwrap();
        state2.epoch_candidate = Some(test_oid());
        state2.write_atomic(&path).unwrap();

        // Read should return state2
        let loaded = MergeStateFile::read(&path).unwrap();
        assert_eq!(loaded, state2);
    }

    #[test]
    fn read_not_found() {
        let path = PathBuf::from("/tmp/nonexistent-merge-state-test.json");
        let err = MergeStateFile::read(&path).unwrap_err();
        assert!(matches!(err, MergeStateError::NotFound(_)));
    }

    #[test]
    fn read_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("merge-state.json");
        fs::write(&path, "corrupted data").unwrap();
        let err = MergeStateFile::read(&path).unwrap_err();
        assert!(matches!(err, MergeStateError::Deserialize(_)));
    }

    #[test]
    fn default_path() {
        let path = MergeStateFile::default_path(Path::new("/repo/.manifold"));
        assert_eq!(path, PathBuf::from("/repo/.manifold/merge-state.json"));
    }

    #[test]
    fn tmp_file_cleaned_up_after_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = MergeStateFile::default_path(dir.path());

        let state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.write_atomic(&path).unwrap();

        // Temp file should be gone after successful rename
        assert!(!dir.path().join(".merge-state.tmp").exists());
    }

    // -- Validation result --

    #[test]
    fn validation_result_serde() {
        let result = ValidationResult {
            passed: false,
            exit_code: Some(1),
            stdout: "running tests...\n".to_owned(),
            stderr: "FAILED: test_foo\n".to_owned(),
            duration_ms: 5432,
            command_results: Vec::new(),
        };
        let json = serde_json::to_string_pretty(&result).unwrap();
        let decoded: ValidationResult = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, result);
    }

    #[test]
    fn validation_result_timeout() {
        let result = ValidationResult {
            passed: false,
            exit_code: None,
            stdout: String::new(),
            stderr: "killed by timeout".to_owned(),
            duration_ms: 60000,
            command_results: Vec::new(),
        };
        let json = serde_json::to_string_pretty(&result).unwrap();
        let decoded: ValidationResult = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, result);
        assert!(decoded.exit_code.is_none());
    }

    #[test]
    fn validation_result_with_command_results_serde() {
        let result = ValidationResult {
            passed: false,
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "cargo test failed".to_owned(),
            duration_ms: 5000,
            command_results: vec![
                CommandResult {
                    command: "cargo check".to_owned(),
                    passed: true,
                    exit_code: Some(0),
                    stdout: "ok\n".to_owned(),
                    stderr: String::new(),
                    duration_ms: 2000,
                },
                CommandResult {
                    command: "cargo test".to_owned(),
                    passed: false,
                    exit_code: Some(1),
                    stdout: String::new(),
                    stderr: "cargo test failed\n".to_owned(),
                    duration_ms: 3000,
                },
            ],
        };
        let json = serde_json::to_string_pretty(&result).unwrap();
        assert!(json.contains("command_results"));
        let decoded: ValidationResult = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.command_results.len(), 2);
        assert_eq!(decoded.command_results[0].command, "cargo check");
        assert!(decoded.command_results[0].passed);
        assert_eq!(decoded.command_results[1].command, "cargo test");
        assert!(!decoded.command_results[1].passed);
    }

    #[test]
    fn validation_result_backward_compat_no_command_results() {
        // Old merge-state files don't have command_results — should deserialize fine
        let json = r#"{
            "passed": true,
            "exit_code": 0,
            "stdout": "ok",
            "stderr": "",
            "duration_ms": 100
        }"#;
        let decoded: ValidationResult = serde_json::from_str(json).unwrap();
        assert!(decoded.passed);
        assert!(decoded.command_results.is_empty());
    }

    // -- Cleanup + recovery helpers --

    #[test]
    fn cleanup_phase_destroys_sources_and_removes_merge_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = MergeStateFile::default_path(dir.path());

        let state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.write_atomic(&path).unwrap();
        assert!(path.exists());

        let mut destroyed = Vec::new();
        run_cleanup_phase(&state, &path, true, |ws| {
            destroyed.push(ws.as_str().to_owned());
            Ok(())
        })
        .unwrap();

        assert_eq!(destroyed, vec!["agent-1".to_owned(), "agent-2".to_owned()]);
        assert!(!path.exists());
    }

    #[test]
    fn cleanup_phase_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = MergeStateFile::default_path(dir.path());

        let state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.write_atomic(&path).unwrap();

        run_cleanup_phase(&state, &path, false, |_ws| Ok(())).unwrap();
        run_cleanup_phase(&state, &path, false, |_ws| Ok(())).unwrap();

        assert!(!path.exists());
    }

    fn state_in_phase(phase: MergePhase) -> MergeStateFile {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        match phase {
            MergePhase::Prepare => {}
            MergePhase::Build => {
                state.advance(MergePhase::Build, 1001).unwrap();
            }
            MergePhase::Validate => {
                state.advance(MergePhase::Build, 1001).unwrap();
                state.advance(MergePhase::Validate, 1002).unwrap();
            }
            MergePhase::Commit => {
                state.advance(MergePhase::Build, 1001).unwrap();
                state.advance(MergePhase::Validate, 1002).unwrap();
                state.advance(MergePhase::Commit, 1003).unwrap();
            }
            MergePhase::Cleanup => {
                state.advance(MergePhase::Build, 1001).unwrap();
                state.advance(MergePhase::Validate, 1002).unwrap();
                state.advance(MergePhase::Commit, 1003).unwrap();
                state.advance(MergePhase::Cleanup, 1004).unwrap();
            }
            MergePhase::Complete => {
                state.advance(MergePhase::Build, 1001).unwrap();
                state.advance(MergePhase::Validate, 1002).unwrap();
                state.advance(MergePhase::Commit, 1003).unwrap();
                state.advance(MergePhase::Cleanup, 1004).unwrap();
                state.advance(MergePhase::Complete, 1005).unwrap();
            }
            MergePhase::Aborted => {
                state.abort("aborted for test", 1001).unwrap();
            }
        }
        state
    }

    #[test]
    fn recovery_no_merge_state_returns_no_merge_in_progress() {
        let dir = tempfile::tempdir().unwrap();
        let path = MergeStateFile::default_path(dir.path());

        let outcome = recover_from_merge_state(&path).unwrap();
        assert_eq!(outcome, RecoveryOutcome::NoMergeInProgress);
    }

    #[test]
    fn recovery_prepare_aborts_and_deletes_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = MergeStateFile::default_path(dir.path());

        let state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.write_atomic(&path).unwrap();

        let outcome = recover_from_merge_state(&path).unwrap();
        assert_eq!(
            outcome,
            RecoveryOutcome::AbortedPreCommit {
                from: MergePhase::Prepare
            }
        );
        assert!(!path.exists());
    }

    #[test]
    fn recovery_build_aborts_and_deletes_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = MergeStateFile::default_path(dir.path());

        let state = state_in_phase(MergePhase::Build);
        state.write_atomic(&path).unwrap();

        let outcome = recover_from_merge_state(&path).unwrap();
        assert_eq!(
            outcome,
            RecoveryOutcome::AbortedPreCommit {
                from: MergePhase::Build
            }
        );
        assert!(!path.exists());
    }

    #[test]
    fn recovery_commit_requests_ref_check_and_keeps_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = MergeStateFile::default_path(dir.path());

        let state = state_in_phase(MergePhase::Commit);
        state.write_atomic(&path).unwrap();

        let outcome = recover_from_merge_state(&path).unwrap();
        assert_eq!(outcome, RecoveryOutcome::CheckCommit);
        assert!(path.exists());
    }

    #[test]
    fn recovery_validate_requests_rerun_and_keeps_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = MergeStateFile::default_path(dir.path());

        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.advance(MergePhase::Build, 1001).unwrap();
        state.advance(MergePhase::Validate, 1002).unwrap();
        state.write_atomic(&path).unwrap();

        let outcome = recover_from_merge_state(&path).unwrap();
        assert_eq!(outcome, RecoveryOutcome::RetryValidate);
        assert!(path.exists());
    }

    #[test]
    fn recovery_cleanup_requests_rerun_and_deletes_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = MergeStateFile::default_path(dir.path());

        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.advance(MergePhase::Build, 1001).unwrap();
        state.advance(MergePhase::Validate, 1002).unwrap();
        state.advance(MergePhase::Commit, 1003).unwrap();
        state.advance(MergePhase::Cleanup, 1004).unwrap();
        state.write_atomic(&path).unwrap();

        let outcome = recover_from_merge_state(&path).unwrap();
        assert_eq!(outcome, RecoveryOutcome::RetryCleanup);
        assert!(!path.exists());
    }

    #[test]
    fn recovery_precommit_abort_preserves_workspace_files() {
        let dir = tempfile::tempdir().unwrap();
        let workspace_file = dir.path().join("ws").join("agent-1").join("keep.txt");
        fs::create_dir_all(workspace_file.parent().unwrap()).unwrap();
        fs::write(&workspace_file, "important work\n").unwrap();

        let path = MergeStateFile::default_path(dir.path());
        let state = state_in_phase(MergePhase::Build);
        state.write_atomic(&path).unwrap();

        let outcome = recover_from_merge_state(&path).unwrap();
        assert!(matches!(
            outcome,
            RecoveryOutcome::AbortedPreCommit {
                from: MergePhase::Build
            }
        ));
        assert_eq!(
            fs::read_to_string(&workspace_file).unwrap(),
            "important work\n"
        );
    }

    #[test]
    fn recovery_dispatch_is_repeatable_across_phases() {
        for _ in 0..3 {
            let scenarios = vec![
                (MergePhase::Prepare, true),
                (MergePhase::Build, true),
                (MergePhase::Validate, false),
                (MergePhase::Commit, false),
                (MergePhase::Cleanup, true),
            ];

            for (phase, should_delete_state_file) in scenarios {
                let dir = tempfile::tempdir().unwrap();
                let path = MergeStateFile::default_path(dir.path());
                let state = state_in_phase(phase.clone());
                state.write_atomic(&path).unwrap();

                let first = recover_from_merge_state(&path).unwrap();
                let second = recover_from_merge_state(&path).unwrap();

                match phase {
                    MergePhase::Prepare | MergePhase::Build => {
                        assert!(matches!(first, RecoveryOutcome::AbortedPreCommit { .. }));
                    }
                    MergePhase::Validate => assert_eq!(first, RecoveryOutcome::RetryValidate),
                    MergePhase::Commit => assert_eq!(first, RecoveryOutcome::CheckCommit),
                    MergePhase::Cleanup => assert_eq!(first, RecoveryOutcome::RetryCleanup),
                    MergePhase::Complete | MergePhase::Aborted => unreachable!(),
                }

                if should_delete_state_file {
                    assert_eq!(second, RecoveryOutcome::NoMergeInProgress);
                } else {
                    assert_eq!(second, first);
                }
            }
        }
    }

    // -- Error display --

    #[test]
    fn error_display_invalid_transition() {
        let err = MergeStateError::InvalidTransition {
            from: MergePhase::Prepare,
            to: MergePhase::Complete,
        };
        let msg = format!("{err}");
        assert!(msg.contains("prepare"));
        assert!(msg.contains("complete"));
    }

    #[test]
    fn error_display_not_found() {
        let err = MergeStateError::NotFound(PathBuf::from("/foo/bar"));
        let msg = format!("{err}");
        assert!(msg.contains("/foo/bar"));
    }

    // -- Full lifecycle --

    #[test]
    fn full_lifecycle_persist_each_phase() {
        let dir = tempfile::tempdir().unwrap();
        let path = MergeStateFile::default_path(dir.path());

        // Prepare
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.write_atomic(&path).unwrap();

        // Build
        state.advance(MergePhase::Build, 1001).unwrap();
        state.epoch_candidate = Some(test_oid());
        state.write_atomic(&path).unwrap();
        let loaded = MergeStateFile::read(&path).unwrap();
        assert_eq!(loaded.phase, MergePhase::Build);
        assert!(loaded.epoch_candidate.is_some());

        // Validate
        state.advance(MergePhase::Validate, 1002).unwrap();
        state.validation_result = Some(ValidationResult {
            passed: true,
            exit_code: Some(0),
            stdout: "all tests passed".to_owned(),
            stderr: String::new(),
            duration_ms: 850,
            command_results: Vec::new(),
        });
        state.write_atomic(&path).unwrap();

        // Commit
        state.advance(MergePhase::Commit, 1003).unwrap();
        state.epoch_after = Some(EpochId::new(&"c".repeat(40)).unwrap());
        state.write_atomic(&path).unwrap();

        // Cleanup
        state.advance(MergePhase::Cleanup, 1004).unwrap();
        state.write_atomic(&path).unwrap();

        // Complete
        state.advance(MergePhase::Complete, 1005).unwrap();
        state.write_atomic(&path).unwrap();

        // Final read
        let final_state = MergeStateFile::read(&path).unwrap();
        assert_eq!(final_state.phase, MergePhase::Complete);
        assert!(final_state.epoch_candidate.is_some());
        assert!(final_state.validation_result.is_some());
        assert!(final_state.epoch_after.is_some());
        assert_eq!(final_state.started_at, 1000);
        assert_eq!(final_state.updated_at, 1005);
    }
}
