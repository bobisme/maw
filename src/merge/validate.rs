//! VALIDATE phase of the epoch advancement state machine.
//!
//! Materializes the candidate commit into a temporary git worktree, runs
//! the configured validation command(s) with a timeout, and enforces the
//! `on_failure` policy.
//!
//! # Multi-command pipelines
//!
//! When multiple commands are configured (via the `commands` array or both
//! `command` and `commands`), they run in sequence. Execution stops on the
//! first failure. Each command's result is captured individually.
//!
//! # Crash safety
//!
//! If a crash occurs during VALIDATE:
//!
//! - The merge-state file records `Validate` phase.
//! - Recovery re-runs validation (inputs are frozen in PREPARE, so this is
//!   safe and deterministic).
//! - Temp worktrees are cleaned up on recovery.
//!
//! # Process
//!
//! 1. Create a temporary git worktree at the candidate commit.
//! 2. Run validation command(s) via `sh -c` with per-command timeout.
//! 3. Capture stdout, stderr, exit code, and wall-clock duration for each.
//! 4. Record the [`ValidationResult`] in the merge-state file.
//! 5. Write diagnostics to `.manifold/artifacts/merge/<id>/validation.json`.
//! 6. Enforce the [`OnFailure`] policy.
//! 7. Clean up the temporary worktree.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::{OnFailure, ValidationConfig};
use crate::merge_state::{CommandResult, MergeStateError, ValidationResult};
use crate::model::types::GitOid;

// ---------------------------------------------------------------------------
// ValidateOutcome
// ---------------------------------------------------------------------------

/// The outcome of the VALIDATE phase after applying the `on_failure` policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValidateOutcome {
    /// No validation command configured — validation is skipped.
    Skipped,
    /// Validation passed (all commands exited 0).
    Passed(ValidationResult),
    /// Validation failed but policy is `Warn` — merge may continue.
    PassedWithWarnings(ValidationResult),
    /// Validation failed and policy blocks the merge.
    Blocked(ValidationResult),
    /// Validation failed and policy requests quarantine.
    Quarantine(ValidationResult),
    /// Validation failed and policy blocks + quarantines.
    BlockedAndQuarantine(ValidationResult),
}

impl ValidateOutcome {
    /// Returns `true` if the merge should proceed (passed, skipped, or warn).
    #[must_use]
    pub fn may_proceed(&self) -> bool {
        matches!(
            self,
            Self::Skipped | Self::Passed(_) | Self::PassedWithWarnings(_) | Self::Quarantine(_)
        )
    }

    /// Returns `true` if a quarantine workspace should be created.
    #[must_use]
    pub fn needs_quarantine(&self) -> bool {
        matches!(self, Self::Quarantine(_) | Self::BlockedAndQuarantine(_))
    }

    /// Extract the validation result, if any.
    #[must_use]
    pub fn result(&self) -> Option<&ValidationResult> {
        match self {
            Self::Skipped => None,
            Self::Passed(r)
            | Self::PassedWithWarnings(r)
            | Self::Blocked(r)
            | Self::Quarantine(r)
            | Self::BlockedAndQuarantine(r) => Some(r),
        }
    }
}

// ---------------------------------------------------------------------------
// ValidateError
// ---------------------------------------------------------------------------

/// Errors that can occur during the VALIDATE phase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValidateError {
    /// Failed to create the temporary worktree.
    WorktreeCreate(String),
    /// Failed to remove the temporary worktree.
    WorktreeRemove(String),
    /// Failed to spawn the validation command.
    CommandSpawn(String),
    /// Merge-state I/O error.
    State(MergeStateError),
    /// Artifacts I/O error.
    ArtifactWrite(String),
}

impl std::fmt::Display for ValidateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WorktreeCreate(msg) => {
                write!(f, "VALIDATE: failed to create temp worktree: {msg}")
            }
            Self::WorktreeRemove(msg) => {
                write!(f, "VALIDATE: failed to remove temp worktree: {msg}")
            }
            Self::CommandSpawn(msg) => {
                write!(f, "VALIDATE: failed to spawn command: {msg}")
            }
            Self::State(e) => write!(f, "VALIDATE: {e}"),
            Self::ArtifactWrite(msg) => {
                write!(f, "VALIDATE: failed to write artifact: {msg}")
            }
        }
    }
}

impl std::error::Error for ValidateError {}

impl From<MergeStateError> for ValidateError {
    fn from(e: MergeStateError) -> Self {
        Self::State(e)
    }
}

// ---------------------------------------------------------------------------
// Temp worktree helpers
// ---------------------------------------------------------------------------

/// Create a temporary detached git worktree at the given commit.
fn create_temp_worktree(
    repo_root: &Path,
    candidate_oid: &GitOid,
    worktree_path: &Path,
) -> Result<(), ValidateError> {
    let output = Command::new("git")
        .args([
            "worktree",
            "add",
            "--detach",
            &worktree_path.to_string_lossy(),
            candidate_oid.as_str(),
        ])
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| ValidateError::WorktreeCreate(format!("spawn git: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(ValidateError::WorktreeCreate(stderr));
    }

    Ok(())
}

/// Remove a temporary git worktree.
fn remove_temp_worktree(repo_root: &Path, worktree_path: &Path) -> Result<(), ValidateError> {
    let output = Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            &worktree_path.to_string_lossy(),
        ])
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| ValidateError::WorktreeRemove(format!("spawn git: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(ValidateError::WorktreeRemove(stderr));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// run_validate_phase
// ---------------------------------------------------------------------------

/// Execute the VALIDATE phase of the merge state machine.
///
/// 1. If no validation command is configured, return [`ValidateOutcome::Skipped`].
/// 2. Create a temporary git worktree at `candidate_oid`.
/// 3. Run validation command(s) in sequence with per-command timeout.
/// 4. Capture diagnostics (stdout, stderr, exit code, timing) per command.
/// 5. Apply the `on_failure` policy.
/// 6. Clean up the temporary worktree.
///
/// # Arguments
///
/// * `repo_root` - Path to the git repository root.
/// * `candidate_oid` - The candidate merge commit to validate.
/// * `config` - The validation configuration from `.manifold/config.toml`.
///
/// # Returns
///
/// A [`ValidateOutcome`] describing the result and policy decision.
///
/// # Errors
///
/// Returns [`ValidateError`] on worktree or command spawn failures.
pub fn run_validate_phase(
    repo_root: &Path,
    candidate_oid: &GitOid,
    config: &ValidationConfig,
) -> Result<ValidateOutcome, ValidateError> {
    // 1. Check if validation is configured
    let commands = config.effective_commands();
    if commands.is_empty() {
        return Ok(ValidateOutcome::Skipped);
    }

    // 2. Create temp worktree
    let worktree_dir = repo_root.join(".manifold").join("validate-tmp");
    // Clean up any stale worktree from a previous crash
    if worktree_dir.exists() {
        let _ = remove_temp_worktree(repo_root, &worktree_dir);
        // Also try just removing the directory if git worktree remove failed
        let _ = fs::remove_dir_all(&worktree_dir);
    }
    create_temp_worktree(repo_root, candidate_oid, &worktree_dir)?;

    // 3. Run validation commands in sequence
    let result = run_commands_pipeline(&commands, &worktree_dir, config.timeout_seconds);

    // 4. Clean up worktree (best-effort)
    let _ = remove_temp_worktree(repo_root, &worktree_dir);
    let _ = fs::remove_dir_all(&worktree_dir);

    let result = result?;

    // 5. Apply on_failure policy
    Ok(apply_policy(&result, &config.on_failure))
}

/// Run the VALIDATE phase without creating a real git worktree.
///
/// Instead of calling `git worktree`, runs the command(s) in the provided
/// directory. Useful for testing the validation logic without a git repo.
pub fn run_validate_in_dir(
    command: &str,
    working_dir: &Path,
    timeout_seconds: u32,
    on_failure: &OnFailure,
) -> Result<ValidateOutcome, ValidateError> {
    let result = run_commands_pipeline(&[command], working_dir, timeout_seconds)?;
    Ok(apply_policy(&result, on_failure))
}

/// Run multiple validation commands in a directory and return the aggregate
/// result. Useful for testing multi-command pipelines without a git repo.
pub fn run_validate_pipeline_in_dir(
    commands: &[&str],
    working_dir: &Path,
    timeout_seconds: u32,
    on_failure: &OnFailure,
) -> Result<ValidateOutcome, ValidateError> {
    let result = run_commands_pipeline(commands, working_dir, timeout_seconds)?;
    Ok(apply_policy(&result, on_failure))
}

// ---------------------------------------------------------------------------
// Diagnostics / artifacts
// ---------------------------------------------------------------------------

/// Write validation diagnostics to the artifacts directory.
///
/// Writes to `.manifold/artifacts/merge/<merge_id>/validation.json`.
/// The write is atomic (write-to-temp + rename).
///
/// # Arguments
///
/// * `manifold_dir` - Path to the `.manifold/` directory.
/// * `merge_id` - An identifier for this merge (typically the candidate OID
///   or a derived hash).
/// * `result` - The validation result to persist.
///
/// # Errors
///
/// Returns [`ValidateError::ArtifactWrite`] on I/O failure. This is
/// non-fatal — callers may choose to log and continue.
pub fn write_validation_artifact(
    manifold_dir: &Path,
    merge_id: &str,
    result: &ValidationResult,
) -> Result<PathBuf, ValidateError> {
    let artifact_dir = manifold_dir.join("artifacts").join("merge").join(merge_id);
    fs::create_dir_all(&artifact_dir).map_err(|e| {
        ValidateError::ArtifactWrite(format!("create dir {}: {e}", artifact_dir.display()))
    })?;

    let artifact_path = artifact_dir.join("validation.json");
    let tmp_path = artifact_dir.join(".validation.json.tmp");

    let json = serde_json::to_string_pretty(result)
        .map_err(|e| ValidateError::ArtifactWrite(format!("serialize: {e}")))?;

    let mut file = fs::File::create(&tmp_path)
        .map_err(|e| ValidateError::ArtifactWrite(format!("create {}: {e}", tmp_path.display())))?;
    file.write_all(json.as_bytes())
        .map_err(|e| ValidateError::ArtifactWrite(format!("write {}: {e}", tmp_path.display())))?;
    file.sync_all()
        .map_err(|e| ValidateError::ArtifactWrite(format!("fsync {}: {e}", tmp_path.display())))?;
    drop(file);

    fs::rename(&tmp_path, &artifact_path).map_err(|e| {
        ValidateError::ArtifactWrite(format!(
            "rename {} → {}: {e}",
            tmp_path.display(),
            artifact_path.display()
        ))
    })?;

    Ok(artifact_path)
}

// ---------------------------------------------------------------------------
// Internal: command execution pipeline
// ---------------------------------------------------------------------------

/// Run multiple commands in sequence, stopping on first failure.
///
/// Returns a single [`ValidationResult`] summarizing the pipeline, plus
/// per-command [`CommandResult`] entries.
fn run_commands_pipeline(
    commands: &[&str],
    working_dir: &Path,
    timeout_seconds: u32,
) -> Result<ValidationResult, ValidateError> {
    let mut command_results = Vec::with_capacity(commands.len());
    let mut total_duration_ms: u64 = 0;

    for &cmd in commands {
        let cr = run_single_command(cmd, working_dir, timeout_seconds)?;
        total_duration_ms = total_duration_ms.saturating_add(cr.duration_ms);
        let passed = cr.passed;
        command_results.push(cr);

        if !passed {
            break; // Stop on first failure
        }
    }

    // Summarize: top-level fields reflect the first failing command
    // (or the last command if all passed)
    let summary_idx = command_results
        .iter()
        .position(|r| !r.passed)
        .unwrap_or(command_results.len().saturating_sub(1));
    let summary = &command_results[summary_idx];

    let all_passed = command_results.iter().all(|r| r.passed);

    Ok(ValidationResult {
        passed: all_passed,
        exit_code: summary.exit_code,
        stdout: summary.stdout.clone(),
        stderr: summary.stderr.clone(),
        duration_ms: total_duration_ms,
        command_results: if commands.len() > 1 {
            command_results
        } else {
            // For single-command runs, omit per-command results for
            // backward compatibility with existing merge-state files.
            Vec::new()
        },
    })
}

/// Run a single shell command with timeout, capturing all output.
fn run_single_command(
    command: &str,
    working_dir: &Path,
    timeout_seconds: u32,
) -> Result<CommandResult, ValidateError> {
    let timeout = Duration::from_secs(timeout_seconds.into());
    let start = Instant::now();

    let mut child = Command::new("sh")
        .args(["-c", command])
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ValidateError::CommandSpawn(format!("sh -c {command:?}: {e}")))?;

    // Wait with timeout
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let duration = start.elapsed();
                let stdout = child
                    .stdout
                    .take()
                    .map(|mut s| {
                        let mut buf = String::new();
                        std::io::Read::read_to_string(&mut s, &mut buf).unwrap_or(0);
                        buf
                    })
                    .unwrap_or_default();
                let stderr = child
                    .stderr
                    .take()
                    .map(|mut s| {
                        let mut buf = String::new();
                        std::io::Read::read_to_string(&mut s, &mut buf).unwrap_or(0);
                        buf
                    })
                    .unwrap_or_default();

                let exit_code = status.code();
                let passed = exit_code == Some(0);

                break CommandResult {
                    command: command.to_owned(),
                    passed,
                    exit_code,
                    stdout,
                    stderr,
                    duration_ms: duration.as_millis() as u64,
                };
            }
            Ok(None) => {
                // Still running — check timeout
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();

                    break CommandResult {
                        command: command.to_owned(),
                        passed: false,
                        exit_code: None,
                        stdout: String::new(),
                        stderr: format!("killed by timeout after {timeout_seconds}s"),
                        duration_ms: start.elapsed().as_millis() as u64,
                    };
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                return Err(ValidateError::CommandSpawn(format!(
                    "wait for command: {e}"
                )));
            }
        }
    };

    Ok(result)
}

/// Apply the `on_failure` policy to a validation result.
fn apply_policy(result: &ValidationResult, on_failure: &OnFailure) -> ValidateOutcome {
    if result.passed {
        ValidateOutcome::Passed(result.clone())
    } else {
        match on_failure {
            OnFailure::Warn => ValidateOutcome::PassedWithWarnings(result.clone()),
            OnFailure::Block => ValidateOutcome::Blocked(result.clone()),
            OnFailure::Quarantine => ValidateOutcome::Quarantine(result.clone()),
            OnFailure::BlockQuarantine => ValidateOutcome::BlockedAndQuarantine(result.clone()),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- ValidateOutcome --

    #[test]
    fn skipped_may_proceed() {
        assert!(ValidateOutcome::Skipped.may_proceed());
        assert!(!ValidateOutcome::Skipped.needs_quarantine());
        assert!(ValidateOutcome::Skipped.result().is_none());
    }

    #[test]
    fn passed_may_proceed() {
        let r = ValidationResult {
            passed: true,
            exit_code: Some(0),
            stdout: "ok".into(),
            stderr: String::new(),
            duration_ms: 100,
            command_results: Vec::new(),
        };
        let o = ValidateOutcome::Passed(r);
        assert!(o.may_proceed());
        assert!(!o.needs_quarantine());
        assert!(o.result().is_some());
    }

    #[test]
    fn blocked_may_not_proceed() {
        let r = ValidationResult {
            passed: false,
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "fail".into(),
            duration_ms: 200,
            command_results: Vec::new(),
        };
        let o = ValidateOutcome::Blocked(r);
        assert!(!o.may_proceed());
        assert!(!o.needs_quarantine());
    }

    #[test]
    fn quarantine_may_proceed_and_needs_quarantine() {
        let r = ValidationResult {
            passed: false,
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "fail".into(),
            duration_ms: 200,
            command_results: Vec::new(),
        };
        let o = ValidateOutcome::Quarantine(r);
        assert!(o.may_proceed());
        assert!(o.needs_quarantine());
    }

    #[test]
    fn block_quarantine_blocks_and_quarantines() {
        let r = ValidationResult {
            passed: false,
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "fail".into(),
            duration_ms: 200,
            command_results: Vec::new(),
        };
        let o = ValidateOutcome::BlockedAndQuarantine(r);
        assert!(!o.may_proceed());
        assert!(o.needs_quarantine());
    }

    // -- Single command: run_validate_in_dir --

    #[test]
    fn validate_passing_command() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_validate_in_dir("echo hello", dir.path(), 10, &OnFailure::Block).unwrap();
        assert!(matches!(outcome, ValidateOutcome::Passed(_)));
        let result = outcome.result().unwrap();
        assert!(result.passed);
        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout.contains("hello"));
        assert!(result.duration_ms < 5000);
    }

    #[test]
    fn validate_failing_command_block() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_validate_in_dir("exit 1", dir.path(), 10, &OnFailure::Block).unwrap();
        assert!(matches!(outcome, ValidateOutcome::Blocked(_)));
        let result = outcome.result().unwrap();
        assert!(!result.passed);
        assert_eq!(result.exit_code, Some(1));
    }

    #[test]
    fn validate_failing_command_warn() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_validate_in_dir("exit 1", dir.path(), 10, &OnFailure::Warn).unwrap();
        assert!(matches!(outcome, ValidateOutcome::PassedWithWarnings(_)));
        assert!(outcome.may_proceed());
    }

    #[test]
    fn validate_failing_command_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let outcome =
            run_validate_in_dir("exit 1", dir.path(), 10, &OnFailure::Quarantine).unwrap();
        assert!(matches!(outcome, ValidateOutcome::Quarantine(_)));
        assert!(outcome.may_proceed());
        assert!(outcome.needs_quarantine());
    }

    #[test]
    fn validate_failing_command_block_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let outcome =
            run_validate_in_dir("exit 1", dir.path(), 10, &OnFailure::BlockQuarantine).unwrap();
        assert!(matches!(outcome, ValidateOutcome::BlockedAndQuarantine(_)));
        assert!(!outcome.may_proceed());
        assert!(outcome.needs_quarantine());
    }

    #[test]
    fn validate_timeout_kills_command() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_validate_in_dir("sleep 60", dir.path(), 1, &OnFailure::Block).unwrap();
        assert!(matches!(outcome, ValidateOutcome::Blocked(_)));
        let result = outcome.result().unwrap();
        assert!(!result.passed);
        assert!(result.exit_code.is_none()); // killed by timeout
        assert!(result.stderr.contains("timeout"));
        assert!(result.duration_ms >= 1000);
        assert!(result.duration_ms < 5000);
    }

    #[test]
    fn validate_captures_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_validate_in_dir(
            "echo error-output >&2 && exit 1",
            dir.path(),
            10,
            &OnFailure::Block,
        )
        .unwrap();
        let result = outcome.result().unwrap();
        assert!(result.stderr.contains("error-output"));
    }

    #[test]
    fn validate_captures_stdout_and_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_validate_in_dir(
            "echo out-text && echo err-text >&2",
            dir.path(),
            10,
            &OnFailure::Block,
        )
        .unwrap();
        let result = outcome.result().unwrap();
        assert!(result.passed);
        assert!(result.stdout.contains("out-text"));
        assert!(result.stderr.contains("err-text"));
    }

    #[test]
    fn validate_exit_code_nonzero() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_validate_in_dir("exit 42", dir.path(), 10, &OnFailure::Block).unwrap();
        let result = outcome.result().unwrap();
        assert_eq!(result.exit_code, Some(42));
        assert!(!result.passed);
    }

    // -- run_validate_phase skip scenarios --

    #[test]
    fn validate_skipped_when_no_command() {
        let config = ValidationConfig {
            command: None,
            commands: Vec::new(),
            timeout_seconds: 60,
            on_failure: OnFailure::Block,
        };
        let oid = GitOid::new(&"a".repeat(40)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_validate_phase(dir.path(), &oid, &config).unwrap();
        assert!(matches!(outcome, ValidateOutcome::Skipped));
    }

    #[test]
    fn validate_skipped_when_empty_command() {
        let config = ValidationConfig {
            command: Some(String::new()),
            commands: Vec::new(),
            timeout_seconds: 60,
            on_failure: OnFailure::Block,
        };
        let oid = GitOid::new(&"a".repeat(40)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_validate_phase(dir.path(), &oid, &config).unwrap();
        assert!(matches!(outcome, ValidateOutcome::Skipped));
    }

    #[test]
    fn validate_skipped_when_empty_commands_array() {
        let config = ValidationConfig {
            command: None,
            commands: vec![String::new()],
            timeout_seconds: 60,
            on_failure: OnFailure::Block,
        };
        let oid = GitOid::new(&"a".repeat(40)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_validate_phase(dir.path(), &oid, &config).unwrap();
        assert!(matches!(outcome, ValidateOutcome::Skipped));
    }

    #[test]
    fn validate_phase_with_no_command_returns_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let config = ValidationConfig::default();
        let oid = GitOid::new(&"a".repeat(40)).unwrap();
        let outcome = run_validate_phase(dir.path(), &oid, &config).unwrap();
        assert!(matches!(outcome, ValidateOutcome::Skipped));
        assert!(outcome.may_proceed());
    }

    // -- Multi-command pipeline --

    #[test]
    fn pipeline_all_pass() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_validate_pipeline_in_dir(
            &["echo step1", "echo step2", "echo step3"],
            dir.path(),
            10,
            &OnFailure::Block,
        )
        .unwrap();
        assert!(matches!(outcome, ValidateOutcome::Passed(_)));
        let result = outcome.result().unwrap();
        assert!(result.passed);
        assert_eq!(result.command_results.len(), 3);
        assert!(result.command_results.iter().all(|r| r.passed));
        assert_eq!(result.command_results[0].command, "echo step1");
        assert_eq!(result.command_results[1].command, "echo step2");
        assert_eq!(result.command_results[2].command, "echo step3");
    }

    #[test]
    fn pipeline_stops_on_first_failure() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_validate_pipeline_in_dir(
            &["echo ok", "exit 1", "echo should-not-run"],
            dir.path(),
            10,
            &OnFailure::Block,
        )
        .unwrap();
        assert!(matches!(outcome, ValidateOutcome::Blocked(_)));
        let result = outcome.result().unwrap();
        assert!(!result.passed);
        // Only 2 commands ran (the third was skipped)
        assert_eq!(result.command_results.len(), 2);
        assert!(result.command_results[0].passed);
        assert!(!result.command_results[1].passed);
    }

    #[test]
    fn pipeline_first_command_fails() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_validate_pipeline_in_dir(
            &["exit 42", "echo never"],
            dir.path(),
            10,
            &OnFailure::Block,
        )
        .unwrap();
        let result = outcome.result().unwrap();
        assert!(!result.passed);
        assert_eq!(result.exit_code, Some(42));
        assert_eq!(result.command_results.len(), 1);
    }

    #[test]
    fn pipeline_captures_per_command_output() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_validate_pipeline_in_dir(
            &["echo output-a", "echo output-b"],
            dir.path(),
            10,
            &OnFailure::Block,
        )
        .unwrap();
        let result = outcome.result().unwrap();
        assert!(result.command_results[0].stdout.contains("output-a"));
        assert!(result.command_results[1].stdout.contains("output-b"));
    }

    #[test]
    fn pipeline_total_duration_is_sum() {
        let dir = tempfile::tempdir().unwrap();
        let outcome =
            run_validate_pipeline_in_dir(&["true", "true"], dir.path(), 10, &OnFailure::Block)
                .unwrap();
        let result = outcome.result().unwrap();
        let per_cmd_total: u64 = result.command_results.iter().map(|r| r.duration_ms).sum();
        // Total duration should be at least the sum of per-command durations
        assert!(result.duration_ms >= per_cmd_total.saturating_sub(10));
    }

    #[test]
    fn pipeline_timeout_per_command() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_validate_pipeline_in_dir(
            &["echo fast", "sleep 60"],
            dir.path(),
            1,
            &OnFailure::Block,
        )
        .unwrap();
        let result = outcome.result().unwrap();
        assert!(!result.passed);
        assert_eq!(result.command_results.len(), 2);
        assert!(result.command_results[0].passed);
        assert!(!result.command_results[1].passed);
        assert!(result.command_results[1].stderr.contains("timeout"));
    }

    #[test]
    fn pipeline_warn_policy_proceeds() {
        let dir = tempfile::tempdir().unwrap();
        let outcome =
            run_validate_pipeline_in_dir(&["exit 1"], dir.path(), 10, &OnFailure::Warn).unwrap();
        assert!(matches!(outcome, ValidateOutcome::PassedWithWarnings(_)));
        assert!(outcome.may_proceed());
    }

    // -- Single command backward compatibility --

    #[test]
    fn single_command_omits_command_results() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_validate_in_dir("echo hi", dir.path(), 10, &OnFailure::Block).unwrap();
        let result = outcome.result().unwrap();
        // Single-command runs don't populate command_results for backward compat
        assert!(result.command_results.is_empty());
    }

    // -- Artifacts --

    #[test]
    fn write_artifact_creates_directory_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let manifold_dir = dir.path().join(".manifold");

        let result = ValidationResult {
            passed: true,
            exit_code: Some(0),
            stdout: "all tests passed\n".into(),
            stderr: String::new(),
            duration_ms: 1234,
            command_results: Vec::new(),
        };

        let path = write_validation_artifact(&manifold_dir, "test-merge-id", &result).unwrap();
        assert!(path.exists());
        assert_eq!(
            path,
            manifold_dir.join("artifacts/merge/test-merge-id/validation.json")
        );

        // Verify contents
        let contents = fs::read_to_string(&path).unwrap();
        let decoded: ValidationResult = serde_json::from_str(&contents).unwrap();
        assert_eq!(decoded, result);
    }

    #[test]
    fn write_artifact_with_multi_command_results() {
        let dir = tempfile::tempdir().unwrap();
        let manifold_dir = dir.path().join(".manifold");

        let result = ValidationResult {
            passed: false,
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "test failed".into(),
            duration_ms: 5000,
            command_results: vec![
                CommandResult {
                    command: "cargo check".into(),
                    passed: true,
                    exit_code: Some(0),
                    stdout: "ok\n".into(),
                    stderr: String::new(),
                    duration_ms: 2000,
                },
                CommandResult {
                    command: "cargo test".into(),
                    passed: false,
                    exit_code: Some(1),
                    stdout: String::new(),
                    stderr: "test failed\n".into(),
                    duration_ms: 3000,
                },
            ],
        };

        let path = write_validation_artifact(&manifold_dir, "merge-42", &result).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        let decoded: ValidationResult = serde_json::from_str(&contents).unwrap();
        assert_eq!(decoded.command_results.len(), 2);
        assert_eq!(decoded.command_results[0].command, "cargo check");
        assert_eq!(decoded.command_results[1].command, "cargo test");
    }

    #[test]
    fn write_artifact_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let manifold_dir = dir.path().join(".manifold");

        let result1 = ValidationResult {
            passed: false,
            exit_code: Some(1),
            stdout: "first run".into(),
            stderr: String::new(),
            duration_ms: 100,
            command_results: Vec::new(),
        };
        write_validation_artifact(&manifold_dir, "id1", &result1).unwrap();

        let result2 = ValidationResult {
            passed: true,
            exit_code: Some(0),
            stdout: "second run".into(),
            stderr: String::new(),
            duration_ms: 200,
            command_results: Vec::new(),
        };
        let path = write_validation_artifact(&manifold_dir, "id1", &result2).unwrap();

        let decoded: ValidationResult =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(decoded.passed);
        assert!(decoded.stdout.contains("second run"));
    }

    // -- Error display --

    #[test]
    fn validate_rerun_same_inputs_produces_same_decision() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.txt"), "ok\n").unwrap();

        let first =
            run_validate_in_dir("test -f ok.txt", dir.path(), 10, &OnFailure::Block).unwrap();
        let second =
            run_validate_in_dir("test -f ok.txt", dir.path(), 10, &OnFailure::Block).unwrap();

        assert_eq!(first.may_proceed(), second.may_proceed());
        assert_eq!(
            first.result().unwrap().exit_code,
            second.result().unwrap().exit_code
        );
        assert_eq!(
            first.result().unwrap().passed,
            second.result().unwrap().passed
        );
    }

    #[test]
    fn validate_error_display() {
        let e = ValidateError::WorktreeCreate("bad".into());
        assert!(format!("{e}").contains("temp worktree"));
        assert!(format!("{e}").contains("bad"));

        let e = ValidateError::CommandSpawn("oops".into());
        assert!(format!("{e}").contains("spawn command"));

        let e = ValidateError::ArtifactWrite("disk full".into());
        assert!(format!("{e}").contains("artifact"));
        assert!(format!("{e}").contains("disk full"));
    }

    // -- Config integration --

    #[test]
    fn config_effective_commands_single() {
        let config = ValidationConfig {
            command: Some("cargo check".into()),
            commands: Vec::new(),
            timeout_seconds: 60,
            on_failure: OnFailure::Block,
        };
        assert_eq!(config.effective_commands(), vec!["cargo check"]);
    }

    #[test]
    fn config_effective_commands_array() {
        let config = ValidationConfig {
            command: None,
            commands: vec!["cargo check".into(), "cargo test".into()],
            timeout_seconds: 60,
            on_failure: OnFailure::Block,
        };
        assert_eq!(
            config.effective_commands(),
            vec!["cargo check", "cargo test"]
        );
    }

    #[test]
    fn config_effective_commands_both() {
        let config = ValidationConfig {
            command: Some("cargo fmt --check".into()),
            commands: vec!["cargo check".into(), "cargo test".into()],
            timeout_seconds: 60,
            on_failure: OnFailure::Block,
        };
        assert_eq!(
            config.effective_commands(),
            vec!["cargo fmt --check", "cargo check", "cargo test"]
        );
    }

    #[test]
    fn config_effective_commands_filters_empty() {
        let config = ValidationConfig {
            command: Some(String::new()),
            commands: vec![String::new(), "cargo test".into(), String::new()],
            timeout_seconds: 60,
            on_failure: OnFailure::Block,
        };
        assert_eq!(config.effective_commands(), vec!["cargo test"]);
    }

    #[test]
    fn config_has_commands() {
        let empty = ValidationConfig::default();
        assert!(!empty.has_commands());

        let with_cmd = ValidationConfig {
            command: Some("test".into()),
            commands: Vec::new(),
            timeout_seconds: 60,
            on_failure: OnFailure::Block,
        };
        assert!(with_cmd.has_commands());

        let with_cmds = ValidationConfig {
            command: None,
            commands: vec!["test".into()],
            timeout_seconds: 60,
            on_failure: OnFailure::Block,
        };
        assert!(with_cmds.has_commands());
    }
}
