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

#![allow(clippy::missing_errors_doc)]

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs;
use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt as _;

use maw_git::{GitRepo as _, GixRepo};

use crate::config::{LanguagePreset, OnFailure, ValidationConfig};
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
    pub const fn may_proceed(&self) -> bool {
        matches!(
            self,
            Self::Skipped | Self::Passed(_) | Self::PassedWithWarnings(_) | Self::Quarantine(_)
        )
    }

    /// Returns `true` if a quarantine workspace should be created.
    #[must_use]
    pub const fn needs_quarantine(&self) -> bool {
        matches!(self, Self::Quarantine(_) | Self::BlockedAndQuarantine(_))
    }

    /// Extract the validation result, if any.
    #[must_use]
    pub const fn result(&self) -> Option<&ValidationResult> {
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

/// Stable admin-directory name for the merge VALIDATE temp worktree.
///
/// VALIDATE uses a single fixed path ([`validate_worktree_dir`]) so a fixed
/// admin name is fine; we prune any stale entry before re-creating.
const VALIDATE_WORKTREE_NAME: &str = "manifold-validate-tmp";

/// Where VALIDATE materializes the candidate: `<manifold_dir>/validate-tmp`
/// (`.manifold/validate-tmp` in v2, `.maw/manifold/validate-tmp` in the
/// consolidated layout).
#[must_use]
pub fn validate_worktree_dir(repo_root: &Path) -> PathBuf {
    crate::model::layout::LayoutFlavor::detect_with_env(repo_root)
        .manifold_dir(repo_root)
        .join("validate-tmp")
}

/// Create a temporary detached git worktree at the given commit.
fn create_temp_worktree(
    repo_root: &Path,
    candidate_oid: &GitOid,
    worktree_path: &Path,
) -> Result<(), ValidateError> {
    let repo = GixRepo::open(repo_root)
        .map_err(|e| ValidateError::WorktreeCreate(format!("open repo: {e}")))?;
    // Idempotent cleanup of any previous attempt's admin dir so worktree_add succeeds.
    let admin_dir = repo
        .common_dir()
        .join("worktrees")
        .join(VALIDATE_WORKTREE_NAME);
    if admin_dir.exists() {
        let _ = std::fs::remove_dir_all(&admin_dir);
    }
    let target: maw_git::GitOid = candidate_oid
        .as_str()
        .parse()
        .map_err(|e| ValidateError::WorktreeCreate(format!("parse candidate oid: {e}")))?;
    repo.worktree_add(VALIDATE_WORKTREE_NAME, target, worktree_path)
        .map_err(|e| ValidateError::WorktreeCreate(e.to_string()))?;
    Ok(())
}

/// Remove a temporary git worktree.
fn remove_temp_worktree(repo_root: &Path, worktree_path: &Path) -> Result<(), ValidateError> {
    let _ = worktree_path; // path is fixed at the well-known admin name
    let repo = GixRepo::open(repo_root)
        .map_err(|e| ValidateError::WorktreeRemove(format!("open repo: {e}")))?;
    let admin_dir = repo
        .common_dir()
        .join("worktrees")
        .join(VALIDATE_WORKTREE_NAME);
    if !admin_dir.exists() {
        return Ok(());
    }
    repo.worktree_remove(VALIDATE_WORKTREE_NAME)
        .map_err(|e| ValidateError::WorktreeRemove(e.to_string()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-language preset auto-detection
// ---------------------------------------------------------------------------

/// Detect the language preset for a project directory by inspecting
/// well-known marker files.
///
/// Detection order (first match wins):
/// 1. `Cargo.toml` → [`LanguagePreset::Rust`]
/// 2. `pyproject.toml` / `setup.py` / `setup.cfg` → [`LanguagePreset::Python`]
/// 3. `tsconfig.json` → [`LanguagePreset::TypeScript`]
///
/// Returns `None` if no known marker is found.
#[must_use]
pub fn detect_language_preset(dir: &Path) -> Option<LanguagePreset> {
    if dir.join("Cargo.toml").exists() {
        return Some(LanguagePreset::Rust);
    }
    if dir.join("pyproject.toml").exists()
        || dir.join("setup.py").exists()
        || dir.join("setup.cfg").exists()
    {
        return Some(LanguagePreset::Python);
    }
    if dir.join("tsconfig.json").exists() {
        return Some(LanguagePreset::TypeScript);
    }
    None
}

/// Resolve the effective command list for a validation config in a given
/// directory, incorporating preset auto-detection.
///
/// Resolution order:
/// 1. Explicit `command`/`commands` (always wins).
/// 2. Named preset (`rust`, `python`, `typescript`).
/// 3. `auto` preset — detect from directory markers.
/// 4. No commands (empty — validation is skipped).
#[must_use]
pub fn resolve_commands(config: &ValidationConfig, worktree_dir: &Path) -> Vec<String> {
    // Explicit commands always take precedence.
    let explicit = config.effective_commands();
    if !explicit.is_empty() {
        return explicit.into_iter().map(str::to_owned).collect();
    }

    // Fall back to preset.
    let preset = match &config.preset {
        None => return Vec::new(),
        Some(LanguagePreset::Auto) => detect_language_preset(worktree_dir),
        Some(p) => Some(p.clone()),
    };

    preset.map_or_else(Vec::new, |p| {
        p.commands().iter().map(|s| (*s).to_owned()).collect()
    })
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
    // 2. Create temp worktree first (needed for preset auto-detection).
    // Layout-aware (bn-ila3): a hardcoded `<root>/.manifold/validate-tmp`
    // created a stray `.manifold/` at the root of every consolidated repo
    // (same class as bn-1lj2).
    let worktree_dir = validate_worktree_dir(repo_root);
    // Clean up any stale worktree from a previous crash
    if worktree_dir.exists() {
        let _ = remove_temp_worktree(repo_root, &worktree_dir);
        // Also try just removing the directory if git worktree remove failed
        let _ = fs::remove_dir_all(&worktree_dir);
    }

    // 1. Resolve commands — includes preset auto-detection against the
    //    worktree dir if `preset = "auto"`.  We need the worktree to exist
    //    for auto-detection, but we only create it when there might be
    //    commands to run. If explicit commands are configured we skip
    //    auto-detection entirely.
    let explicit = config.effective_commands();
    if explicit.is_empty() && config.preset.is_none() {
        return Ok(ValidateOutcome::Skipped);
    }

    create_temp_worktree(repo_root, candidate_oid, &worktree_dir)?;

    // Resolve the full command list (explicit wins; preset is fallback).
    let commands = resolve_commands(config, &worktree_dir);
    if commands.is_empty() {
        // Preset was configured but auto-detection found nothing.
        let _ = remove_temp_worktree(repo_root, &worktree_dir);
        let _ = fs::remove_dir_all(&worktree_dir);
        return Ok(ValidateOutcome::Skipped);
    }

    // 3. Run validation commands in sequence
    crate::fp!("FP_VALIDATE_BEFORE_CHECK")
        .map_err(|e| ValidateError::CommandSpawn(e.to_string()))?;
    let cmd_refs: Vec<&str> = commands.iter().map(String::as_str).collect();
    let result = run_commands_pipeline(&cmd_refs, &worktree_dir, config.timeout_seconds);

    // 4. Clean up worktree (best-effort)
    let _ = remove_temp_worktree(repo_root, &worktree_dir);
    let _ = fs::remove_dir_all(&worktree_dir);

    let result = result?;
    crate::fp!("FP_VALIDATE_AFTER_CHECK")
        .map_err(|e| ValidateError::CommandSpawn(e.to_string()))?;

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

/// Run the full validation config (including preset resolution) in a
/// directory without creating a git worktree.
///
/// This is the testing counterpart of [`run_validate_phase`]: it exercises
/// the complete command-resolution path (explicit → preset → auto-detect)
/// in an isolated temp directory.
pub fn run_validate_config_in_dir(
    config: &ValidationConfig,
    working_dir: &Path,
) -> Result<ValidateOutcome, ValidateError> {
    let commands = resolve_commands(config, working_dir);
    if commands.is_empty() {
        return Ok(ValidateOutcome::Skipped);
    }
    let cmd_refs: Vec<&str> = commands.iter().map(String::as_str).collect();
    let result = run_commands_pipeline(&cmd_refs, working_dir, config.timeout_seconds)?;
    Ok(apply_policy(&result, &config.on_failure))
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
        .unwrap_or_else(|| command_results.len().saturating_sub(1));
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

/// Maximum retained bytes for each validation output stream.
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

/// Maximum wait for pipe readers after the command tree has been terminated.
const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(2);

#[derive(Default)]
struct BoundedCapture {
    bytes: VecDeque<u8>,
    omitted: usize,
}

impl BoundedCapture {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend(chunk);
        while self.bytes.len() > MAX_CAPTURE_BYTES {
            let _ = self.bytes.pop_front();
            self.omitted = self.omitted.saturating_add(1);
        }
    }

    fn render(&self, incomplete_reason: Option<&str>) -> String {
        let retained: Vec<u8> = self.bytes.iter().copied().collect();
        let mut output = String::from_utf8_lossy(&retained).into_owned();
        if self.omitted > 0 {
            output = format!(
                "[output truncated: {} leading bytes omitted]\n{output}",
                self.omitted
            );
        }
        if let Some(reason) = incomplete_reason {
            let _ = write!(output, "\n[output capture incomplete: {reason}]");
        }
        output
    }
}

fn lock_capture(capture: &Mutex<BoundedCapture>) -> std::sync::MutexGuard<'_, BoundedCapture> {
    capture
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn spawn_pipe_drain(
    mut pipe: impl Read + Send + 'static,
) -> (
    Arc<Mutex<BoundedCapture>>,
    mpsc::Receiver<Result<(), String>>,
) {
    let capture = Arc::new(Mutex::new(BoundedCapture::default()));
    let writer = Arc::clone(&capture);
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut chunk = [0_u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => {
                    let _ = done_tx.send(Ok(()));
                    return;
                }
                Ok(read) => lock_capture(&writer).push(&chunk[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => {
                    let _ = done_tx.send(Err(error.to_string()));
                    return;
                }
            }
        }
    });
    (capture, done_rx)
}

fn finish_pipe_drain(
    capture: &Mutex<BoundedCapture>,
    done: &mpsc::Receiver<Result<(), String>>,
    deadline: Instant,
) -> String {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let incomplete_reason = match done.recv_timeout(remaining) {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(format!("pipe read failed: {error}")),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            Some("pipe remained open after process cleanup".to_owned())
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Some("pipe reader stopped without a result".to_owned())
        }
    };
    lock_capture(capture).render(incomplete_reason.as_deref())
}

fn kill_command_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let group = format!("-{}", child.id());
        let killed = Command::new("kill")
            .args(["-KILL", "--", &group])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if killed {
            return;
        }
    }
    let _ = child.kill();
}

/// Run a single shell command with timeout, draining and bounding both streams.
fn run_single_command(
    command: &str,
    working_dir: &Path,
    timeout_seconds: u32,
) -> Result<CommandResult, ValidateError> {
    let timeout = Duration::from_secs(timeout_seconds.into());
    let start = Instant::now();

    let mut process = Command::new("sh");
    process
        .args(["-c", command])
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    process.process_group(0);

    let mut child = process
        .spawn()
        .map_err(|e| ValidateError::CommandSpawn(format!("sh -c {command:?}: {e}")))?;

    let stdout = child.stdout.take().ok_or_else(|| {
        ValidateError::CommandSpawn("validation stdout pipe was not created".to_owned())
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        ValidateError::CommandSpawn("validation stderr pipe was not created".to_owned())
    })?;
    let (stdout_capture, stdout_done) = spawn_pipe_drain(stdout);
    let (stderr_capture, stderr_done) = spawn_pipe_drain(stderr);

    // Wait with timeout
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // A successful shell can leave background descendants holding
                // its pipe handles. Terminate that command tree before the
                // bounded reader wait so collection cannot hang indefinitely.
                kill_command_tree(&mut child);
                let drain_deadline = Instant::now() + PIPE_DRAIN_GRACE;
                let stdout = finish_pipe_drain(&stdout_capture, &stdout_done, drain_deadline);
                let stderr = finish_pipe_drain(&stderr_capture, &stderr_done, drain_deadline);
                let duration = start.elapsed();

                let exit_code = status.code();
                let passed = exit_code == Some(0);

                break CommandResult {
                    command: command.to_owned(),
                    passed,
                    exit_code,
                    stdout,
                    stderr,
                    duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
                };
            }
            Ok(None) => {
                // Still running — check timeout
                if start.elapsed() >= timeout {
                    kill_command_tree(&mut child);
                    let _ = child.wait();

                    let drain_deadline = Instant::now() + PIPE_DRAIN_GRACE;
                    let stdout = finish_pipe_drain(&stdout_capture, &stdout_done, drain_deadline);
                    let mut stderr =
                        finish_pipe_drain(&stderr_capture, &stderr_done, drain_deadline);
                    if !stderr.is_empty() {
                        stderr.push('\n');
                    }
                    let _ = write!(stderr, "killed by timeout after {timeout_seconds}s");

                    break CommandResult {
                        command: command.to_owned(),
                        passed: false,
                        exit_code: None,
                        stdout,
                        stderr,
                        duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                    };
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                kill_command_tree(&mut child);
                let _ = child.wait();
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
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome = run_validate_in_dir("echo hello", dir.path(), 10, &OnFailure::Block)
            .expect("operation should succeed");
        assert!(matches!(outcome, ValidateOutcome::Passed(_)));
        let result = outcome.result().expect("operation should succeed");
        assert!(result.passed);
        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout.contains("hello"));
        assert!(result.duration_ms < 5000);
    }

    #[test]
    fn validate_failing_command_block() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome = run_validate_in_dir("exit 1", dir.path(), 10, &OnFailure::Block)
            .expect("operation should succeed");
        assert!(matches!(outcome, ValidateOutcome::Blocked(_)));
        let result = outcome.result().expect("operation should succeed");
        assert!(!result.passed);
        assert_eq!(result.exit_code, Some(1));
    }

    #[test]
    fn validate_failing_command_warn() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome = run_validate_in_dir("exit 1", dir.path(), 10, &OnFailure::Warn)
            .expect("operation should succeed");
        assert!(matches!(outcome, ValidateOutcome::PassedWithWarnings(_)));
        assert!(outcome.may_proceed());
    }

    #[test]
    fn validate_failing_command_quarantine() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome = run_validate_in_dir("exit 1", dir.path(), 10, &OnFailure::Quarantine)
            .expect("operation should succeed");
        assert!(matches!(outcome, ValidateOutcome::Quarantine(_)));
        assert!(outcome.may_proceed());
        assert!(outcome.needs_quarantine());
    }

    #[test]
    fn validate_failing_command_block_quarantine() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome = run_validate_in_dir("exit 1", dir.path(), 10, &OnFailure::BlockQuarantine)
            .expect("operation should succeed");
        assert!(matches!(outcome, ValidateOutcome::BlockedAndQuarantine(_)));
        assert!(!outcome.may_proceed());
        assert!(outcome.needs_quarantine());
    }

    #[test]
    fn validate_timeout_kills_command() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome = run_validate_in_dir("sleep 60", dir.path(), 1, &OnFailure::Block)
            .expect("operation should succeed");
        assert!(matches!(outcome, ValidateOutcome::Blocked(_)));
        let result = outcome.result().expect("operation should succeed");
        assert!(!result.passed);
        assert!(result.exit_code.is_none()); // killed by timeout
        assert!(result.stderr.contains("timeout"));
        assert!(result.duration_ms >= 1000);
        assert!(result.duration_ms < 5000);
    }

    #[test]
    fn validate_captures_stderr() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome = run_validate_in_dir(
            "echo error-output >&2 && exit 1",
            dir.path(),
            10,
            &OnFailure::Block,
        )
        .expect("operation should succeed");
        let result = outcome.result().expect("operation should succeed");
        assert!(result.stderr.contains("error-output"));
    }

    #[test]
    fn validate_captures_stdout_and_stderr() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome = run_validate_in_dir(
            "echo out-text && echo err-text >&2",
            dir.path(),
            10,
            &OnFailure::Block,
        )
        .expect("operation should succeed");
        let result = outcome.result().expect("operation should succeed");
        assert!(result.passed);
        assert!(result.stdout.contains("out-text"));
        assert!(result.stderr.contains("err-text"));
    }

    #[test]
    fn validate_drains_large_stdout_and_stderr_and_retains_bounded_tails() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome = run_validate_in_dir(
            "yes stdout-data | head -c 1200000; echo STDOUT-END; \
             yes stderr-data | head -c 1200000 >&2; echo STDERR-END >&2",
            dir.path(),
            10,
            &OnFailure::Block,
        )
        .expect("verbose validation must not deadlock");
        let result = outcome.result().expect("validation result");
        assert!(result.passed);
        assert!(result.stdout.contains("STDOUT-END"));
        assert!(result.stderr.contains("STDERR-END"));
        assert!(result.stdout.contains("leading bytes omitted"));
        assert!(result.stderr.contains("leading bytes omitted"));
        assert!(result.stdout.len() <= MAX_CAPTURE_BYTES + 100);
        assert!(result.stderr.len() <= MAX_CAPTURE_BYTES + 100);
    }

    #[test]
    fn validate_verbose_failure_preserves_exit_code_and_final_diagnostics() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome = run_validate_in_dir(
            "yes noise | head -c 1200000 >&2; echo FINAL-DIAGNOSTIC >&2; exit 37",
            dir.path(),
            10,
            &OnFailure::Block,
        )
        .expect("verbose failing validation must complete");
        let result = outcome.result().expect("validation result");
        assert!(!result.passed);
        assert_eq!(result.exit_code, Some(37));
        assert!(result.stderr.contains("FINAL-DIAGNOSTIC"));
        assert!(result.stderr.contains("leading bytes omitted"));
    }

    /// Poll `/proc/<pid>/stat` until the process is gone or a zombie, up to
    /// a 2s deadline, returning the last observed state (`None` = gone).
    ///
    /// Production (`kill_command_tree`) guarantees SIGKILL is *sent* to the
    /// whole process group before validation returns; kill(2) is
    /// asynchronous and the orphaned descendant is reaped by init/subreaper,
    /// so on a loaded machine it can still show 'R' for a moment while it
    /// finishes exiting (bn-2xqt). The deadline is far below the
    /// descendant's own `sleep 60`, so an unkilled descendant still fails.
    #[cfg(target_os = "linux")]
    fn wait_for_descendant_death(pid: &str) -> Option<char> {
        let stat_path = Path::new("/proc").join(pid).join("stat");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let state = std::fs::read_to_string(&stat_path)
                .ok()
                .and_then(|stat| stat.rsplit_once(") ").map(|(_, rest)| rest.to_owned()))
                .and_then(|rest| rest.chars().next());
            if state.is_none_or(|state| state == 'Z') || Instant::now() >= deadline {
                return state;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn validate_timeout_preserves_partial_output_and_kills_descendants() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome = run_validate_in_dir(
            "echo partial-out; echo partial-err >&2; sleep 60 & echo $! > child.pid; wait",
            dir.path(),
            1,
            &OnFailure::Block,
        )
        .expect("timed out validation must return");
        let result = outcome.result().expect("validation result");
        assert!(!result.passed);
        assert_eq!(result.exit_code, None);
        assert!(result.stdout.contains("partial-out"));
        assert!(result.stderr.contains("partial-err"));
        assert!(result.stderr.contains("killed by timeout after 1s"));

        #[cfg(target_os = "linux")]
        {
            let pid = std::fs::read_to_string(dir.path().join("child.pid"))
                .expect("read descendant pid")
                .trim()
                .to_owned();
            let state = wait_for_descendant_death(&pid);
            assert!(
                state.is_none_or(|state| state == 'Z'),
                "validation descendant remained live after timeout: {state:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn validate_success_does_not_wait_for_descendant_held_pipes() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let started = Instant::now();
        let outcome = run_validate_in_dir(
            "sleep 60 & echo $! > child.pid; echo parent-done; exit 0",
            dir.path(),
            10,
            &OnFailure::Block,
        )
        .expect("successful parent must not wait for descendant pipes");
        assert!(started.elapsed() < Duration::from_secs(5));
        let result = outcome.result().expect("validation result");
        assert!(result.passed);
        assert!(result.stdout.contains("parent-done"));
        assert!(result.duration_ms < 5000);

        #[cfg(target_os = "linux")]
        {
            let pid = std::fs::read_to_string(dir.path().join("child.pid"))
                .expect("read descendant pid")
                .trim()
                .to_owned();
            let state = wait_for_descendant_death(&pid);
            assert!(
                state.is_none_or(|state| state == 'Z'),
                "validation descendant remained live after parent exit: {state:?}"
            );
        }
    }

    #[test]
    fn validate_exit_code_nonzero() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome = run_validate_in_dir("exit 42", dir.path(), 10, &OnFailure::Block)
            .expect("operation should succeed");
        let result = outcome.result().expect("operation should succeed");
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
            preset: None,
            on_failure: OnFailure::Block,
        };
        let oid = GitOid::new(&"a".repeat(40)).expect("operation should succeed");
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome =
            run_validate_phase(dir.path(), &oid, &config).expect("operation should succeed");
        assert!(matches!(outcome, ValidateOutcome::Skipped));
    }

    #[test]
    fn validate_skipped_when_empty_command() {
        let config = ValidationConfig {
            command: Some(String::new()),
            commands: Vec::new(),
            timeout_seconds: 60,
            preset: None,
            on_failure: OnFailure::Block,
        };
        let oid = GitOid::new(&"a".repeat(40)).expect("operation should succeed");
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome =
            run_validate_phase(dir.path(), &oid, &config).expect("operation should succeed");
        assert!(matches!(outcome, ValidateOutcome::Skipped));
    }

    #[test]
    fn validate_skipped_when_empty_commands_array() {
        let config = ValidationConfig {
            command: None,
            commands: vec![String::new()],
            timeout_seconds: 60,
            preset: None,
            on_failure: OnFailure::Block,
        };
        let oid = GitOid::new(&"a".repeat(40)).expect("operation should succeed");
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome =
            run_validate_phase(dir.path(), &oid, &config).expect("operation should succeed");
        assert!(matches!(outcome, ValidateOutcome::Skipped));
    }

    /// bn-ila3: in the consolidated layout VALIDATE materializes under
    /// `.maw/manifold/validate-tmp` and never creates a root `.manifold/`.
    #[test]
    fn validate_phase_consolidated_layout_creates_no_root_manifold_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .expect("spawn git");
            assert!(out.status.success(), "git {args:?} failed");
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "T"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "commit.gpgsign", "false"]);
        fs::write(root.join("f.txt"), "x\n").expect("write");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "c"]);
        let oid = GitOid::new(&git(&["rev-parse", "HEAD"])).expect("oid");
        fs::create_dir_all(root.join(".maw").join("manifold")).expect("layout marker");

        // The command records the directory it ran in.
        let mark = root.join("cwd.txt");
        let config = ValidationConfig {
            command: Some(format!("pwd -P > '{}'", mark.display())),
            ..ValidationConfig::default()
        };
        let outcome = run_validate_phase(root, &oid, &config).expect("validate");
        assert!(outcome.may_proceed(), "outcome: {outcome:?}");

        let cwd = fs::read_to_string(&mark).expect("cwd mark");
        let expected = root.join(".maw").join("manifold").join("validate-tmp");
        let canon_expected = fs::canonicalize(root)
            .expect("canon root")
            .join(".maw")
            .join("manifold")
            .join("validate-tmp");
        assert_eq!(PathBuf::from(cwd.trim()), canon_expected);
        assert!(
            !root.join(".manifold").exists(),
            "no stray root .manifold/ in a consolidated repo"
        );
        assert_eq!(validate_worktree_dir(root), expected);
    }

    #[test]
    fn validate_phase_with_no_command_returns_skipped() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let config = ValidationConfig::default();
        let oid = GitOid::new(&"a".repeat(40)).expect("operation should succeed");
        let outcome =
            run_validate_phase(dir.path(), &oid, &config).expect("operation should succeed");
        assert!(matches!(outcome, ValidateOutcome::Skipped));
        assert!(outcome.may_proceed());
    }

    // -- Multi-command pipeline --

    #[test]
    fn pipeline_all_pass() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome = run_validate_pipeline_in_dir(
            &["echo step1", "echo step2", "echo step3"],
            dir.path(),
            10,
            &OnFailure::Block,
        )
        .expect("operation should succeed");
        assert!(matches!(outcome, ValidateOutcome::Passed(_)));
        let result = outcome.result().expect("operation should succeed");
        assert!(result.passed);
        assert_eq!(result.command_results.len(), 3);
        assert!(result.command_results.iter().all(|r| r.passed));
        assert_eq!(result.command_results[0].command, "echo step1");
        assert_eq!(result.command_results[1].command, "echo step2");
        assert_eq!(result.command_results[2].command, "echo step3");
    }

    #[test]
    fn pipeline_stops_on_first_failure() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome = run_validate_pipeline_in_dir(
            &["echo ok", "exit 1", "echo should-not-run"],
            dir.path(),
            10,
            &OnFailure::Block,
        )
        .expect("operation should succeed");
        assert!(matches!(outcome, ValidateOutcome::Blocked(_)));
        let result = outcome.result().expect("operation should succeed");
        assert!(!result.passed);
        // Only 2 commands ran (the third was skipped)
        assert_eq!(result.command_results.len(), 2);
        assert!(result.command_results[0].passed);
        assert!(!result.command_results[1].passed);
    }

    #[test]
    fn pipeline_first_command_fails() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome = run_validate_pipeline_in_dir(
            &["exit 42", "echo never"],
            dir.path(),
            10,
            &OnFailure::Block,
        )
        .expect("operation should succeed");
        let result = outcome.result().expect("operation should succeed");
        assert!(!result.passed);
        assert_eq!(result.exit_code, Some(42));
        assert_eq!(result.command_results.len(), 1);
    }

    #[test]
    fn pipeline_captures_per_command_output() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome = run_validate_pipeline_in_dir(
            &["echo output-a", "echo output-b"],
            dir.path(),
            10,
            &OnFailure::Block,
        )
        .expect("operation should succeed");
        let result = outcome.result().expect("operation should succeed");
        assert!(result.command_results[0].stdout.contains("output-a"));
        assert!(result.command_results[1].stdout.contains("output-b"));
    }

    #[test]
    fn pipeline_total_duration_is_sum() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome =
            run_validate_pipeline_in_dir(&["true", "true"], dir.path(), 10, &OnFailure::Block)
                .expect("operation should succeed");
        let result = outcome.result().expect("operation should succeed");
        let per_cmd_total: u64 = result.command_results.iter().map(|r| r.duration_ms).sum();
        // Total duration should be at least the sum of per-command durations
        assert!(result.duration_ms >= per_cmd_total.saturating_sub(10));
    }

    #[test]
    fn pipeline_timeout_per_command() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome = run_validate_pipeline_in_dir(
            &["echo fast", "sleep 60"],
            dir.path(),
            1,
            &OnFailure::Block,
        )
        .expect("operation should succeed");
        let result = outcome.result().expect("operation should succeed");
        assert!(!result.passed);
        assert_eq!(result.command_results.len(), 2);
        assert!(result.command_results[0].passed);
        assert!(!result.command_results[1].passed);
        assert!(result.command_results[1].stderr.contains("timeout"));
    }

    #[test]
    fn pipeline_warn_policy_proceeds() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome = run_validate_pipeline_in_dir(&["exit 1"], dir.path(), 10, &OnFailure::Warn)
            .expect("operation should succeed");
        assert!(matches!(outcome, ValidateOutcome::PassedWithWarnings(_)));
        assert!(outcome.may_proceed());
    }

    // -- Single command backward compatibility --

    #[test]
    fn single_command_omits_command_results() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let outcome = run_validate_in_dir("echo hi", dir.path(), 10, &OnFailure::Block)
            .expect("operation should succeed");
        let result = outcome.result().expect("operation should succeed");
        // Single-command runs don't populate command_results for backward compat
        assert!(result.command_results.is_empty());
    }

    // -- Artifacts --

    #[test]
    fn write_artifact_creates_directory_and_file() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let manifold_dir = dir.path().join(".manifold");

        let result = ValidationResult {
            passed: true,
            exit_code: Some(0),
            stdout: "all tests passed\n".into(),
            stderr: String::new(),
            duration_ms: 1234,
            command_results: Vec::new(),
        };

        let path = write_validation_artifact(&manifold_dir, "test-merge-id", &result)
            .expect("operation should succeed");
        assert!(path.exists());
        assert_eq!(
            path,
            manifold_dir.join("artifacts/merge/test-merge-id/validation.json")
        );

        // Verify contents
        let contents = fs::read_to_string(&path).expect("operation should succeed");
        let decoded: ValidationResult =
            serde_json::from_str(&contents).expect("operation should succeed");
        assert_eq!(decoded, result);
    }

    #[test]
    fn write_artifact_with_multi_command_results() {
        let dir = tempfile::tempdir().expect("operation should succeed");
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

        let path = write_validation_artifact(&manifold_dir, "merge-42", &result)
            .expect("operation should succeed");
        let contents = fs::read_to_string(&path).expect("operation should succeed");
        let decoded: ValidationResult =
            serde_json::from_str(&contents).expect("operation should succeed");
        assert_eq!(decoded.command_results.len(), 2);
        assert_eq!(decoded.command_results[0].command, "cargo check");
        assert_eq!(decoded.command_results[1].command, "cargo test");
    }

    #[test]
    fn write_artifact_overwrites_existing() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let manifold_dir = dir.path().join(".manifold");

        let result1 = ValidationResult {
            passed: false,
            exit_code: Some(1),
            stdout: "first run".into(),
            stderr: String::new(),
            duration_ms: 100,
            command_results: Vec::new(),
        };
        write_validation_artifact(&manifold_dir, "id1", &result1)
            .expect("operation should succeed");

        let result2 = ValidationResult {
            passed: true,
            exit_code: Some(0),
            stdout: "second run".into(),
            stderr: String::new(),
            duration_ms: 200,
            command_results: Vec::new(),
        };
        let path = write_validation_artifact(&manifold_dir, "id1", &result2)
            .expect("operation should succeed");

        let decoded: ValidationResult =
            serde_json::from_str(&fs::read_to_string(&path).expect("operation should succeed"))
                .expect("operation should succeed");
        assert!(decoded.passed);
        assert!(decoded.stdout.contains("second run"));
    }

    // -- Error display --

    #[test]
    fn validate_rerun_same_inputs_produces_same_decision() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        std::fs::write(dir.path().join("ok.txt"), "ok\n").expect("operation should succeed");

        let first = run_validate_in_dir("test -f ok.txt", dir.path(), 10, &OnFailure::Block)
            .expect("operation should succeed");
        let second = run_validate_in_dir("test -f ok.txt", dir.path(), 10, &OnFailure::Block)
            .expect("operation should succeed");

        assert_eq!(first.may_proceed(), second.may_proceed());
        assert_eq!(
            first.result().expect("operation should succeed").exit_code,
            second.result().expect("operation should succeed").exit_code
        );
        assert_eq!(
            first.result().expect("operation should succeed").passed,
            second.result().expect("operation should succeed").passed
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
            preset: None,
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
            preset: None,
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
            preset: None,
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
            preset: None,
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
            preset: None,
            on_failure: OnFailure::Block,
        };
        assert!(with_cmd.has_commands());

        let with_cmds = ValidationConfig {
            command: None,
            commands: vec!["test".into()],
            timeout_seconds: 60,
            preset: None,
            on_failure: OnFailure::Block,
        };
        assert!(with_cmds.has_commands());
    }

    // -- detect_language_preset --

    #[test]
    fn detect_preset_rust_from_cargo_toml() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n")
            .expect("operation should succeed");
        assert_eq!(
            detect_language_preset(dir.path()),
            Some(LanguagePreset::Rust)
        );
    }

    #[test]
    fn detect_preset_python_from_pyproject_toml() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        std::fs::write(dir.path().join("pyproject.toml"), "[project]\nname=\"x\"\n")
            .expect("operation should succeed");
        assert_eq!(
            detect_language_preset(dir.path()),
            Some(LanguagePreset::Python)
        );
    }

    #[test]
    fn detect_preset_python_from_setup_py() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        std::fs::write(
            dir.path().join("setup.py"),
            "from setuptools import setup\n",
        )
        .expect("operation should succeed");
        assert_eq!(
            detect_language_preset(dir.path()),
            Some(LanguagePreset::Python)
        );
    }

    #[test]
    fn detect_preset_python_from_setup_cfg() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        std::fs::write(dir.path().join("setup.cfg"), "[metadata]\nname=x\n")
            .expect("operation should succeed");
        assert_eq!(
            detect_language_preset(dir.path()),
            Some(LanguagePreset::Python)
        );
    }

    #[test]
    fn detect_preset_typescript_from_tsconfig() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        std::fs::write(dir.path().join("tsconfig.json"), "{}\n").expect("operation should succeed");
        assert_eq!(
            detect_language_preset(dir.path()),
            Some(LanguagePreset::TypeScript)
        );
    }

    #[test]
    fn detect_preset_returns_none_for_unknown_project() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        std::fs::write(dir.path().join("README.md"), "# hello\n")
            .expect("operation should succeed");
        assert_eq!(detect_language_preset(dir.path()), None);
    }

    #[test]
    fn detect_preset_rust_wins_over_python_when_both_present() {
        // Cargo.toml takes precedence (first in detection order)
        let dir = tempfile::tempdir().expect("operation should succeed");
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n")
            .expect("operation should succeed");
        std::fs::write(dir.path().join("pyproject.toml"), "[project]\nname=\"x\"\n")
            .expect("operation should succeed");
        assert_eq!(
            detect_language_preset(dir.path()),
            Some(LanguagePreset::Rust)
        );
    }

    // -- resolve_commands --

    #[test]
    fn resolve_explicit_commands_take_precedence_over_preset() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        // Cargo.toml present — would trigger Rust preset — but explicit command wins.
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n")
            .expect("operation should succeed");
        let config = ValidationConfig {
            command: Some("make test".into()),
            commands: Vec::new(),
            preset: Some(LanguagePreset::Rust),
            timeout_seconds: 60,
            on_failure: OnFailure::Block,
        };
        let cmds = resolve_commands(&config, dir.path());
        assert_eq!(cmds, vec!["make test"]);
    }

    #[test]
    fn resolve_named_preset_rust() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let config = ValidationConfig {
            command: None,
            commands: Vec::new(),
            preset: Some(LanguagePreset::Rust),
            timeout_seconds: 60,
            on_failure: OnFailure::Block,
        };
        let cmds = resolve_commands(&config, dir.path());
        assert_eq!(cmds, vec!["cargo check", "cargo test --no-run"]);
    }

    #[test]
    fn resolve_named_preset_python() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let config = ValidationConfig {
            command: None,
            commands: Vec::new(),
            preset: Some(LanguagePreset::Python),
            timeout_seconds: 60,
            on_failure: OnFailure::Block,
        };
        let cmds = resolve_commands(&config, dir.path());
        assert_eq!(cmds, vec!["python -m py_compile", "pytest -q --co"]);
    }

    #[test]
    fn resolve_named_preset_typescript() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let config = ValidationConfig {
            command: None,
            commands: Vec::new(),
            preset: Some(LanguagePreset::TypeScript),
            timeout_seconds: 60,
            on_failure: OnFailure::Block,
        };
        let cmds = resolve_commands(&config, dir.path());
        assert_eq!(cmds, vec!["tsc --noEmit"]);
    }

    #[test]
    fn resolve_auto_preset_detects_rust() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n")
            .expect("operation should succeed");
        let config = ValidationConfig {
            command: None,
            commands: Vec::new(),
            preset: Some(LanguagePreset::Auto),
            timeout_seconds: 60,
            on_failure: OnFailure::Block,
        };
        let cmds = resolve_commands(&config, dir.path());
        assert_eq!(cmds, vec!["cargo check", "cargo test --no-run"]);
    }

    #[test]
    fn resolve_auto_preset_detects_python() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        std::fs::write(dir.path().join("pyproject.toml"), "[project]\n")
            .expect("operation should succeed");
        let config = ValidationConfig {
            command: None,
            commands: Vec::new(),
            preset: Some(LanguagePreset::Auto),
            timeout_seconds: 60,
            on_failure: OnFailure::Block,
        };
        let cmds = resolve_commands(&config, dir.path());
        assert_eq!(cmds, vec!["python -m py_compile", "pytest -q --co"]);
    }

    #[test]
    fn resolve_auto_preset_detects_typescript() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        std::fs::write(dir.path().join("tsconfig.json"), "{}").expect("operation should succeed");
        let config = ValidationConfig {
            command: None,
            commands: Vec::new(),
            preset: Some(LanguagePreset::Auto),
            timeout_seconds: 60,
            on_failure: OnFailure::Block,
        };
        let cmds = resolve_commands(&config, dir.path());
        assert_eq!(cmds, vec!["tsc --noEmit"]);
    }

    #[test]
    fn resolve_auto_preset_unknown_project_returns_empty() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        // No marker files
        let config = ValidationConfig {
            command: None,
            commands: Vec::new(),
            preset: Some(LanguagePreset::Auto),
            timeout_seconds: 60,
            on_failure: OnFailure::Block,
        };
        let cmds = resolve_commands(&config, dir.path());
        assert!(cmds.is_empty());
    }

    #[test]
    fn resolve_no_preset_no_commands_returns_empty() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let config = ValidationConfig::default();
        let cmds = resolve_commands(&config, dir.path());
        assert!(cmds.is_empty());
    }

    // -- run_validate_config_in_dir (preset integration) --

    #[test]
    fn config_in_dir_skipped_with_no_config() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let config = ValidationConfig::default();
        let outcome =
            run_validate_config_in_dir(&config, dir.path()).expect("operation should succeed");
        assert!(matches!(outcome, ValidateOutcome::Skipped));
    }

    #[test]
    fn config_in_dir_skipped_when_auto_finds_nothing() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        // No marker files — auto-detect returns None → skipped
        let config = ValidationConfig {
            command: None,
            commands: Vec::new(),
            preset: Some(LanguagePreset::Auto),
            timeout_seconds: 60,
            on_failure: OnFailure::Block,
        };
        let outcome =
            run_validate_config_in_dir(&config, dir.path()).expect("operation should succeed");
        assert!(matches!(outcome, ValidateOutcome::Skipped));
    }

    #[test]
    fn config_in_dir_explicit_commands_ignore_preset() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        // Rust preset present but explicit commands win
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n")
            .expect("operation should succeed");
        let config = ValidationConfig {
            command: Some("echo explicit".into()),
            commands: Vec::new(),
            preset: Some(LanguagePreset::Rust),
            timeout_seconds: 10,
            on_failure: OnFailure::Block,
        };
        let outcome =
            run_validate_config_in_dir(&config, dir.path()).expect("operation should succeed");
        assert!(matches!(outcome, ValidateOutcome::Passed(_)));
        let result = outcome.result().expect("operation should succeed");
        assert!(result.stdout.contains("explicit"));
        // Single-command run: command_results empty for backward compat
        assert!(result.command_results.is_empty());
    }

    #[test]
    fn config_in_dir_multi_command_explicit_with_preset_ignored() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let config = ValidationConfig {
            command: None,
            commands: vec!["echo step1".into(), "echo step2".into()],
            preset: Some(LanguagePreset::TypeScript), // ignored — explicit commands present
            timeout_seconds: 10,
            on_failure: OnFailure::Block,
        };
        let outcome =
            run_validate_config_in_dir(&config, dir.path()).expect("operation should succeed");
        assert!(matches!(outcome, ValidateOutcome::Passed(_)));
        let result = outcome.result().expect("operation should succeed");
        assert_eq!(result.command_results.len(), 2);
        assert!(result.command_results[0].stdout.contains("step1"));
        assert!(result.command_results[1].stdout.contains("step2"));
    }

    #[test]
    fn config_in_dir_auto_preset_not_skipped_when_marker_found() {
        // Create a dir with Cargo.toml so auto-detection fires.
        // The Rust preset commands will likely fail (not a real project),
        // but with Warn policy the outcome is PassedWithWarnings (not Skipped).
        let dir = tempfile::tempdir().expect("operation should succeed");
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n")
            .expect("operation should succeed");
        let config = ValidationConfig {
            command: None,
            commands: Vec::new(),
            preset: Some(LanguagePreset::Auto),
            timeout_seconds: 5,
            on_failure: OnFailure::Warn,
        };
        let outcome =
            run_validate_config_in_dir(&config, dir.path()).expect("operation should succeed");
        // Must NOT be skipped — preset was resolved
        assert!(!matches!(outcome, ValidateOutcome::Skipped));
    }
}
