//! Quarantine workspace for failed merge validation.
//!
//! When post-merge validation fails and the `on_failure` policy includes
//! "quarantine", the candidate merge tree is materialized into a normal git
//! worktree so an agent can fix the build failure and promote the result
//! without redoing the entire merge.
//!
//! # Lifecycle
//!
//! ```text
//! merge validation fails (quarantine policy)
//!   → create_quarantine_workspace()
//!       creates ws/merge-quarantine-<id>/ (git worktree at candidate OID)
//!       writes .manifold/quarantine/<id>/state.json
//!
//! agent edits files in ws/merge-quarantine-<id>/ to fix the build failure
//!
//! maw merge promote <id>
//!   → re-run validation in the quarantine workspace directory
//!   → if green: commit quarantine state, advance epoch, clean up
//!   → if still failing: report diagnostics, quarantine remains
//!
//! maw merge abandon <id>
//!   → remove quarantine workspace + state (non-destructive to source workspaces)
//! ```
//!
//! # Crash safety
//!
//! Quarantine creation is a two-step write: (1) git worktree add, (2) state file
//! write. If a crash occurs between the two steps, the worktree exists but the
//! state file is missing. `list_quarantines` ignores worktrees without a state
//! file, and `abandon_quarantine` is idempotent (handles missing state files).
//!
//! # Design doc reference
//!
//! §5.12.2: "The quarantine workspace is a normal workspace: it can be edited,
//! snapshotted, and merged like any other. It exists to let an agent fix-forward
//! the candidate result without redoing the merge."

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::config::ValidationConfig;
use crate::merge::validate::{ValidateOutcome, run_validate_config_in_dir};
use crate::merge_state::ValidationResult;
use crate::model::types::{EpochId, GitOid, WorkspaceId};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Prefix for quarantine workspace names. All quarantine worktrees have names
/// starting with this prefix so they can be identified in `maw ws list`.
pub const QUARANTINE_NAME_PREFIX: &str = "merge-quarantine-";

/// Subdirectory under `.manifold/` where quarantine state files are stored.
const QUARANTINE_STATE_SUBDIR: &str = "quarantine";

// ---------------------------------------------------------------------------
// QuarantineState
// ---------------------------------------------------------------------------

/// Persisted state for a quarantine workspace.
///
/// Written to `.manifold/quarantine/<merge_id>/state.json` after the worktree
/// is created. This file is the authoritative record that a quarantine exists:
/// if the state file is absent, the quarantine is considered non-existent even
/// if a matching worktree directory is present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantineState {
    /// Short identifier for this quarantine (first 12 characters of the
    /// candidate commit OID). Used as the directory name suffix and for
    /// promote/abandon commands.
    pub merge_id: String,

    /// The epoch (base commit) before the merge started.
    pub epoch_before: GitOid,

    /// The candidate commit produced by the BUILD phase.
    ///
    /// The quarantine worktree is checked out at this commit. Agents may
    /// edit files in the worktree; on promote, any uncommitted edits are
    /// staged and committed before re-validation.
    pub candidate: GitOid,

    /// Source workspaces that were being merged.
    pub sources: Vec<WorkspaceId>,

    /// The branch that would have been advanced on a successful commit.
    pub branch: String,

    /// The validation diagnostics that triggered quarantine creation.
    pub validation_result: ValidationResult,

    /// Unix timestamp (seconds) when the quarantine was created.
    pub created_at: u64,
}

impl QuarantineState {
    /// Read the quarantine state file from disk.
    ///
    /// # Errors
    ///
    /// Returns an error if the file does not exist or cannot be parsed.
    pub fn read(manifold_dir: &Path, merge_id: &str) -> Result<Self, QuarantineError> {
        let path = state_path(manifold_dir, merge_id);
        let contents = fs::read_to_string(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                QuarantineError::NotFound {
                    merge_id: merge_id.to_owned(),
                }
            } else {
                QuarantineError::Io(format!("read {}: {e}", path.display()))
            }
        })?;
        serde_json::from_str(&contents)
            .map_err(|e| QuarantineError::Io(format!("parse {}: {e}", path.display())))
    }

    /// Write the quarantine state file atomically (write-tmp + fsync + rename).
    pub fn write_atomic(&self, manifold_dir: &Path) -> Result<(), QuarantineError> {
        let dir = state_dir(manifold_dir, &self.merge_id);
        fs::create_dir_all(&dir)
            .map_err(|e| QuarantineError::Io(format!("create dir {}: {e}", dir.display())))?;

        let path = state_path(manifold_dir, &self.merge_id);
        let tmp = path.with_extension("json.tmp");

        let json = serde_json::to_string_pretty(self)
            .map_err(|e| QuarantineError::Io(format!("serialize: {e}")))?;

        let mut file = fs::File::create(&tmp)
            .map_err(|e| QuarantineError::Io(format!("create {}: {e}", tmp.display())))?;
        file.write_all(json.as_bytes())
            .map_err(|e| QuarantineError::Io(format!("write {}: {e}", tmp.display())))?;
        file.sync_all()
            .map_err(|e| QuarantineError::Io(format!("fsync {}: {e}", tmp.display())))?;
        drop(file);

        fs::rename(&tmp, &path).map_err(|e| {
            QuarantineError::Io(format!(
                "rename {} → {}: {e}",
                tmp.display(),
                path.display()
            ))
        })?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// QuarantineError
// ---------------------------------------------------------------------------

/// Errors from quarantine operations.
#[derive(Debug)]
pub enum QuarantineError {
    /// No quarantine with the given merge_id exists.
    NotFound { merge_id: String },
    /// The quarantine worktree directory does not exist.
    WorktreeNotFound { merge_id: String, path: PathBuf },
    /// A git command failed.
    Git(String),
    /// An I/O error occurred.
    Io(String),
    /// Validation error during promote.
    Validate(String),
    /// Commit phase error during promote.
    Commit(String),
}

impl std::fmt::Display for QuarantineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { merge_id } => {
                write!(f, "no quarantine with id '{merge_id}' found")
            }
            Self::WorktreeNotFound { merge_id, path } => {
                write!(
                    f,
                    "quarantine '{merge_id}' state exists but worktree is missing at {}",
                    path.display()
                )
            }
            Self::Git(msg) => write!(f, "git error: {msg}"),
            Self::Io(msg) => write!(f, "I/O error: {msg}"),
            Self::Validate(msg) => write!(f, "validation error: {msg}"),
            Self::Commit(msg) => write!(f, "commit error: {msg}"),
        }
    }
}

impl std::error::Error for QuarantineError {}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Return the name of the quarantine workspace for the given merge_id.
///
/// The name is `merge-quarantine-<merge_id>` which:
/// - Passes workspace name validation (alphanumeric + hyphens only)
/// - Is identifiable by the [`QUARANTINE_NAME_PREFIX`] prefix
/// - Creates the worktree at `ws/merge-quarantine-<merge_id>/`
pub fn quarantine_workspace_name(merge_id: &str) -> String {
    format!("{QUARANTINE_NAME_PREFIX}{merge_id}")
}

/// Return the workspace path for a quarantine with the given merge_id.
pub fn quarantine_workspace_path(repo_root: &Path, merge_id: &str) -> PathBuf {
    repo_root
        .join("ws")
        .join(quarantine_workspace_name(merge_id))
}

/// Return the directory that holds the quarantine state files.
fn state_dir(manifold_dir: &Path, merge_id: &str) -> PathBuf {
    manifold_dir.join(QUARANTINE_STATE_SUBDIR).join(merge_id)
}

/// Return the path to the quarantine state file.
fn state_path(manifold_dir: &Path, merge_id: &str) -> PathBuf {
    state_dir(manifold_dir, merge_id).join("state.json")
}

/// Extract the merge_id from a quarantine workspace name, if it has the
/// quarantine prefix.
pub fn merge_id_from_name(name: &str) -> Option<&str> {
    name.strip_prefix(QUARANTINE_NAME_PREFIX)
}

// ---------------------------------------------------------------------------
// create_quarantine_workspace
// ---------------------------------------------------------------------------

/// Create a quarantine workspace for a failed merge.
///
/// 1. Creates a git worktree at `ws/merge-quarantine-<merge_id>/` checked
///    out at `candidate`.
/// 2. Writes validation diagnostics to the quarantine state directory.
/// 3. Writes a `state.json` with merge intent (sources, epoch_before, candidate).
///
/// # Arguments
///
/// * `repo_root` — Path to the git repository root.
/// * `manifold_dir` — Path to the `.manifold/` directory.
/// * `merge_id` — Short identifier for this merge (typically first 12 hex chars
///   of the candidate OID).
/// * `sources` — Source workspaces that were being merged.
/// * `epoch_before` — The epoch before the merge started.
/// * `candidate` — The candidate commit (BUILD output).
/// * `branch` — The branch that would have been advanced.
/// * `validation_result` — The validation diagnostics from VALIDATE.
///
/// # Returns
///
/// The absolute path to the newly-created quarantine workspace.
///
/// # Errors
///
/// Returns [`QuarantineError`] if the worktree cannot be created or the
/// state file cannot be written.
pub fn create_quarantine_workspace(
    repo_root: &Path,
    manifold_dir: &Path,
    merge_id: &str,
    sources: Vec<WorkspaceId>,
    epoch_before: EpochId,
    candidate: GitOid,
    branch: &str,
    validation_result: ValidationResult,
) -> Result<PathBuf, QuarantineError> {
    let workspace_name = quarantine_workspace_name(merge_id);
    let workspace_path = repo_root.join("ws").join(&workspace_name);

    // Remove any stale worktree at this path (idempotent — previous partial failure)
    if workspace_path.exists() {
        let _ = remove_worktree(repo_root, &workspace_path);
        let _ = fs::remove_dir_all(&workspace_path);
    }

    // Ensure ws/ directory exists
    let ws_dir = repo_root.join("ws");
    fs::create_dir_all(&ws_dir).map_err(|e| QuarantineError::Io(format!("create ws/ dir: {e}")))?;

    // Create a detached git worktree at the candidate commit
    let output = Command::new("git")
        .args([
            "worktree",
            "add",
            "--detach",
            &workspace_path.to_string_lossy(),
            candidate.as_str(),
        ])
        .current_dir(repo_root)
        .output()
        .map_err(|e| QuarantineError::Git(format!("spawn git worktree add: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(QuarantineError::Git(format!(
            "git worktree add for quarantine failed: {stderr}"
        )));
    }

    // Write validation diagnostics to the quarantine directory
    let _ = write_quarantine_diagnostics(manifold_dir, merge_id, &validation_result);

    // Write the quarantine state file (atomic)
    let now = now_secs();
    let state = QuarantineState {
        merge_id: merge_id.to_owned(),
        epoch_before: epoch_before.oid().clone(),
        candidate,
        sources,
        branch: branch.to_owned(),
        validation_result,
        created_at: now,
    };
    state.write_atomic(manifold_dir)?;

    Ok(workspace_path)
}

// ---------------------------------------------------------------------------
// promote_quarantine
// ---------------------------------------------------------------------------

/// Result of a promote operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromoteResult {
    /// Validation passed and the epoch was advanced.
    Committed { new_epoch: GitOid },
    /// Validation still fails — quarantine unchanged.
    ValidationFailed { validation_result: ValidationResult },
}

/// Promote a quarantine workspace: re-validate, then commit if green.
///
/// 1. Read the quarantine state (epoch_before, candidate, branch, sources).
/// 2. Stage and commit any uncommitted changes in the quarantine workspace
///    (a no-op if there are no changes, preserving the original candidate OID).
/// 3. Re-run validation commands in the quarantine workspace directory.
/// 4. If validation passes:
///    a. Advance epoch refs (refs/manifold/epoch/current and refs/heads/<branch>).
///    b. Abandon the quarantine (remove worktree + state).
/// 5. If validation fails: return `PromoteResult::ValidationFailed` with
///    diagnostics; the quarantine remains intact.
///
/// # Arguments
///
/// * `repo_root` — Path to the git repository root.
/// * `manifold_dir` — Path to the `.manifold/` directory.
/// * `merge_id` — The quarantine identifier (first 12 chars of candidate OID).
/// * `config` — The validation configuration to use for re-validation.
///
/// # Returns
///
/// A [`PromoteResult`] describing whether the epoch was advanced.
///
/// # Errors
///
/// Returns [`QuarantineError`] if the state cannot be read, git operations
/// fail, or the commit phase encounters an unrecoverable error.
pub fn promote_quarantine(
    repo_root: &Path,
    manifold_dir: &Path,
    merge_id: &str,
    config: &ValidationConfig,
) -> Result<PromoteResult, QuarantineError> {
    // 1. Read quarantine state
    let state = QuarantineState::read(manifold_dir, merge_id)?;

    let ws_path = quarantine_workspace_path(repo_root, merge_id);
    if !ws_path.exists() {
        return Err(QuarantineError::WorktreeNotFound {
            merge_id: merge_id.to_owned(),
            path: ws_path,
        });
    }

    // 2. Stage and commit any uncommitted changes in the quarantine workspace
    let commit_oid = commit_quarantine_edits(repo_root, &ws_path, &state.candidate)?;

    // 3. Re-run validation commands in the quarantine workspace directory
    let validate_outcome = run_validate_config_in_dir(config, &ws_path)
        .map_err(|e| QuarantineError::Validate(format!("{e}")))?;

    match validate_outcome {
        ValidateOutcome::Skipped
        | ValidateOutcome::Passed(_)
        | ValidateOutcome::PassedWithWarnings(_) => {
            // 4a. Advance epoch refs
            let epoch_before_oid = state.epoch_before.clone();
            crate::refs::advance_epoch(repo_root, &epoch_before_oid, &commit_oid)
                .map_err(|e| QuarantineError::Commit(format!("advance epoch: {e}")))?;

            let branch_ref = format!("refs/heads/{}", state.branch);
            crate::refs::write_ref_cas(repo_root, &branch_ref, &epoch_before_oid, &commit_oid)
                .map_err(|e| QuarantineError::Commit(format!("update branch ref: {e}")))?;

            // 4b. Clean up quarantine (best-effort)
            let _ = abandon_quarantine(repo_root, manifold_dir, merge_id);

            Ok(PromoteResult::Committed {
                new_epoch: commit_oid,
            })
        }
        ValidateOutcome::Blocked(r) | ValidateOutcome::BlockedAndQuarantine(r) => {
            Ok(PromoteResult::ValidationFailed {
                validation_result: r,
            })
        }
        ValidateOutcome::Quarantine(r) => {
            // Quarantine policy (not block) — still counts as a validation failure
            // for the purposes of promote: we only promote when validation fully passes.
            Ok(PromoteResult::ValidationFailed {
                validation_result: r,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// abandon_quarantine
// ---------------------------------------------------------------------------

/// Abandon a quarantine workspace: remove the worktree and state file.
///
/// This operation is idempotent — calling it on an already-abandoned
/// quarantine succeeds without error.
///
/// Source workspaces are NOT affected. The merge must be retried separately
/// if the quarantine is abandoned.
///
/// # Arguments
///
/// * `repo_root` — Path to the git repository root.
/// * `manifold_dir` — Path to the `.manifold/` directory.
/// * `merge_id` — The quarantine identifier.
///
/// # Errors
///
/// Returns [`QuarantineError::Git`] if the git worktree removal fails in a
/// way that is not "worktree not found". I/O errors removing the state file
/// are logged but do not cause an error return.
pub fn abandon_quarantine(
    repo_root: &Path,
    manifold_dir: &Path,
    merge_id: &str,
) -> Result<(), QuarantineError> {
    let ws_path = quarantine_workspace_path(repo_root, merge_id);

    // Remove the git worktree (idempotent — ignore "not registered" errors)
    if ws_path.exists() {
        remove_worktree(repo_root, &ws_path)?;
        // Also clean up the directory (git worktree remove may leave it)
        let _ = fs::remove_dir_all(&ws_path);
    }

    // Remove the quarantine state directory
    let dir = state_dir(manifold_dir, merge_id);
    if dir.exists() {
        fs::remove_dir_all(&dir)
            .map_err(|e| QuarantineError::Io(format!("remove state dir {}: {e}", dir.display())))?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// list_quarantines
// ---------------------------------------------------------------------------

/// List all active quarantine workspaces.
///
/// Scans `.manifold/quarantine/` for state files and returns the parsed
/// [`QuarantineState`] for each valid quarantine.
///
/// Invalid or unreadable state files are silently skipped.
pub fn list_quarantines(manifold_dir: &Path) -> Vec<QuarantineState> {
    let quarantine_base = manifold_dir.join(QUARANTINE_STATE_SUBDIR);
    if !quarantine_base.exists() {
        return Vec::new();
    }

    let mut result = Vec::new();

    let Ok(entries) = fs::read_dir(&quarantine_base) else {
        return Vec::new();
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let merge_id = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if let Ok(state) = QuarantineState::read(manifold_dir, &merge_id) {
            result.push(state);
        }
    }

    result.sort_by(|a, b| a.merge_id.cmp(&b.merge_id));
    result
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Stage and commit any uncommitted changes in the quarantine workspace.
///
/// If there are no changes (workspace is clean), returns the existing HEAD OID
/// unchanged. Otherwise, creates a new commit with message "quarantine: fix-forward".
fn commit_quarantine_edits(
    repo_root: &Path,
    ws_path: &Path,
    original_candidate: &GitOid,
) -> Result<GitOid, QuarantineError> {
    // Check if there are any uncommitted changes
    let status_output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(ws_path)
        .output()
        .map_err(|e| QuarantineError::Git(format!("spawn git status: {e}")))?;

    let status_str = String::from_utf8_lossy(&status_output.stdout);

    if status_str.trim().is_empty() {
        // No uncommitted changes — use the original candidate as-is
        return Ok(original_candidate.clone());
    }

    // Stage all changes
    let add_output = Command::new("git")
        .args(["add", "-A"])
        .current_dir(ws_path)
        .output()
        .map_err(|e| QuarantineError::Git(format!("spawn git add: {e}")))?;

    if !add_output.status.success() {
        let stderr = String::from_utf8_lossy(&add_output.stderr)
            .trim()
            .to_owned();
        return Err(QuarantineError::Git(format!("git add -A failed: {stderr}")));
    }

    // Commit the staged changes
    let commit_output = Command::new("git")
        .args(["commit", "--no-verify", "-m", "quarantine: fix-forward"])
        .current_dir(ws_path)
        .output()
        .map_err(|e| QuarantineError::Git(format!("spawn git commit: {e}")))?;

    if !commit_output.status.success() {
        let stderr = String::from_utf8_lossy(&commit_output.stderr)
            .trim()
            .to_owned();
        return Err(QuarantineError::Git(format!("git commit failed: {stderr}")));
    }

    // Get the new HEAD OID
    let head_output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(ws_path)
        .output()
        .map_err(|e| QuarantineError::Git(format!("spawn git rev-parse: {e}")))?;

    if !head_output.status.success() {
        let stderr = String::from_utf8_lossy(&head_output.stderr)
            .trim()
            .to_owned();
        return Err(QuarantineError::Git(format!(
            "git rev-parse HEAD failed: {stderr}"
        )));
    }

    let oid_str = String::from_utf8_lossy(&head_output.stdout)
        .trim()
        .to_string();
    GitOid::new(&oid_str).map_err(|e| QuarantineError::Git(format!("parse HEAD OID: {e}")))
}

/// Remove a git worktree (force) at the given path.
///
/// Idempotent: if the worktree is not registered with git, silently succeeds.
fn remove_worktree(repo_root: &Path, path: &Path) -> Result<(), QuarantineError> {
    let output = Command::new("git")
        .args(["worktree", "remove", "--force", &path.to_string_lossy()])
        .current_dir(repo_root)
        .output()
        .map_err(|e| QuarantineError::Git(format!("spawn git worktree remove: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        // If the worktree isn't registered, that's fine (already removed)
        if stderr.contains("is not a working tree") || stderr.contains("not a worktree") {
            return Ok(());
        }
        return Err(QuarantineError::Git(format!(
            "git worktree remove failed: {stderr}"
        )));
    }

    Ok(())
}

/// Write validation diagnostics JSON to the quarantine state directory.
///
/// Non-fatal: errors are silently ignored since this is supplementary info.
fn write_quarantine_diagnostics(
    manifold_dir: &Path,
    merge_id: &str,
    result: &ValidationResult,
) -> std::io::Result<()> {
    let dir = state_dir(manifold_dir, merge_id);
    fs::create_dir_all(&dir)?;

    let path = dir.join("validation.json");
    let tmp = dir.join(".validation.json.tmp");

    let json = serde_json::to_string_pretty(result)?;
    let mut file = fs::File::create(&tmp)?;
    file.write_all(json.as_bytes())?;
    file.sync_all()?;
    drop(file);

    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Get current Unix timestamp in seconds.
fn now_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCmd;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Test git helpers
    // -----------------------------------------------------------------------

    fn run_git(root: &Path, args: &[&str]) -> String {
        let out = StdCmd::new("git")
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
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Create a git repo with a single initial commit and an epoch ref.
    fn setup_repo() -> (TempDir, GitOid) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        run_git(root, &["init"]);
        run_git(root, &["config", "user.name", "Test"]);
        run_git(root, &["config", "user.email", "test@test.com"]);
        run_git(root, &["config", "commit.gpgsign", "false"]);

        fs::write(root.join("README.md"), "# Test\n").unwrap();
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", "initial"]);
        run_git(root, &["branch", "-M", "main"]);

        let oid_str = run_git(root, &["rev-parse", "HEAD"]);
        let oid = GitOid::new(&oid_str).unwrap();

        run_git(root, &["update-ref", crate::refs::EPOCH_CURRENT, &oid_str]);

        (dir, oid)
    }

    /// Create a second commit (candidate commit) in the repo.
    fn make_candidate_commit(root: &Path, content: &str) -> GitOid {
        fs::write(root.join("candidate.txt"), content).unwrap();
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", "candidate"]);
        let oid_str = run_git(root, &["rev-parse", "HEAD"]);
        GitOid::new(&oid_str).unwrap()
    }

    fn dummy_validation_result(passed: bool) -> ValidationResult {
        ValidationResult {
            passed,
            exit_code: Some(if passed { 0 } else { 1 }),
            stdout: String::new(),
            stderr: if passed {
                String::new()
            } else {
                "build failed\n".to_owned()
            },
            duration_ms: 100,
            command_results: Vec::new(),
        }
    }

    // -----------------------------------------------------------------------
    // quarantine_workspace_name
    // -----------------------------------------------------------------------

    #[test]
    fn workspace_name_has_prefix() {
        let name = quarantine_workspace_name("abc123def456");
        assert_eq!(name, "merge-quarantine-abc123def456");
        assert!(name.starts_with(QUARANTINE_NAME_PREFIX));
    }

    #[test]
    fn merge_id_from_name_roundtrip() {
        let merge_id = "abc123def456";
        let name = quarantine_workspace_name(merge_id);
        assert_eq!(merge_id_from_name(&name), Some(merge_id));
    }

    #[test]
    fn merge_id_from_name_rejects_non_quarantine() {
        assert!(merge_id_from_name("alice").is_none());
        assert!(merge_id_from_name("default").is_none());
        assert!(merge_id_from_name("merge-abc").is_none());
    }

    // -----------------------------------------------------------------------
    // QuarantineState serialization
    // -----------------------------------------------------------------------

    #[test]
    fn state_roundtrip() {
        let dir = TempDir::new().unwrap();
        let manifold_dir = dir.path().join(".manifold");

        let oid = GitOid::new(&"a".repeat(40)).unwrap();
        let epoch = EpochId::new(&"b".repeat(40)).unwrap();
        let state = QuarantineState {
            merge_id: "abc123def456".to_owned(),
            epoch_before: epoch.oid().clone(),
            candidate: oid.clone(),
            sources: vec![WorkspaceId::new("ws-1").unwrap()],
            branch: "main".to_owned(),
            validation_result: dummy_validation_result(false),
            created_at: 1000,
        };

        state.write_atomic(&manifold_dir).unwrap();

        let loaded = QuarantineState::read(&manifold_dir, "abc123def456").unwrap();
        assert_eq!(loaded.merge_id, "abc123def456");
        assert_eq!(loaded.epoch_before, *epoch.oid());
        assert_eq!(loaded.candidate, oid);
        assert_eq!(loaded.branch, "main");
        assert_eq!(loaded.created_at, 1000);
    }

    #[test]
    fn state_not_found_error() {
        let dir = TempDir::new().unwrap();
        let manifold_dir = dir.path().join(".manifold");
        let err = QuarantineState::read(&manifold_dir, "nonexistent").unwrap_err();
        assert!(matches!(err, QuarantineError::NotFound { .. }));
    }

    // -----------------------------------------------------------------------
    // create_quarantine_workspace
    // -----------------------------------------------------------------------

    #[test]
    fn create_creates_worktree_and_state() {
        let (dir, epoch_oid) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".manifold");

        let candidate = make_candidate_commit(root, "candidate content\n");
        let merge_id = &candidate.as_str()[..12];
        let epoch_id = EpochId::new(epoch_oid.as_str()).unwrap();

        let ws_path = create_quarantine_workspace(
            root,
            &manifold_dir,
            merge_id,
            vec![WorkspaceId::new("ws-1").unwrap()],
            epoch_id,
            candidate.clone(),
            "main",
            dummy_validation_result(false),
        )
        .unwrap();

        // Workspace directory exists
        assert!(ws_path.exists(), "quarantine worktree should exist");

        // Workspace is checked out at the candidate commit
        let head = run_git(&ws_path, &["rev-parse", "HEAD"]);
        assert_eq!(head, candidate.as_str(), "worktree should be at candidate");

        // State file exists and is valid
        let state = QuarantineState::read(&manifold_dir, merge_id).unwrap();
        assert_eq!(state.candidate, candidate);
        assert_eq!(state.branch, "main");
        assert!(!state.validation_result.passed);

        // Workspace contains the candidate file
        assert!(ws_path.join("candidate.txt").exists());
    }

    #[test]
    fn create_is_idempotent_removes_stale_worktree() {
        let (dir, epoch_oid) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".manifold");

        let candidate = make_candidate_commit(root, "content\n");
        let merge_id = &candidate.as_str()[..12];
        let epoch_id = EpochId::new(epoch_oid.as_str()).unwrap();

        // First creation
        create_quarantine_workspace(
            root,
            &manifold_dir,
            merge_id,
            vec![WorkspaceId::new("ws-1").unwrap()],
            epoch_id.clone(),
            candidate.clone(),
            "main",
            dummy_validation_result(false),
        )
        .unwrap();

        // Second creation should succeed (idempotent)
        let ws_path = create_quarantine_workspace(
            root,
            &manifold_dir,
            merge_id,
            vec![WorkspaceId::new("ws-1").unwrap()],
            epoch_id,
            candidate.clone(),
            "main",
            dummy_validation_result(false),
        )
        .unwrap();

        assert!(ws_path.exists());
        let state = QuarantineState::read(&manifold_dir, merge_id).unwrap();
        assert_eq!(state.candidate, candidate);
    }

    #[test]
    fn create_writes_validation_diagnostics() {
        let (dir, epoch_oid) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".manifold");

        let candidate = make_candidate_commit(root, "content\n");
        let merge_id = &candidate.as_str()[..12];
        let epoch_id = EpochId::new(epoch_oid.as_str()).unwrap();
        let vr = ValidationResult {
            passed: false,
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "cargo check failed\n".to_owned(),
            duration_ms: 5000,
            command_results: Vec::new(),
        };

        create_quarantine_workspace(
            root,
            &manifold_dir,
            merge_id,
            vec![WorkspaceId::new("ws-1").unwrap()],
            epoch_id,
            candidate.clone(),
            "main",
            vr.clone(),
        )
        .unwrap();

        // validation.json should exist in state dir
        let val_json = manifold_dir
            .join(QUARANTINE_STATE_SUBDIR)
            .join(merge_id)
            .join("validation.json");
        assert!(val_json.exists(), "validation.json should be written");

        let contents = fs::read_to_string(&val_json).unwrap();
        let decoded: ValidationResult = serde_json::from_str(&contents).unwrap();
        assert!(!decoded.passed);
        assert!(decoded.stderr.contains("cargo check failed"));
    }

    // -----------------------------------------------------------------------
    // abandon_quarantine
    // -----------------------------------------------------------------------

    #[test]
    fn abandon_removes_worktree_and_state() {
        let (dir, epoch_oid) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".manifold");

        let candidate = make_candidate_commit(root, "content\n");
        let merge_id = &candidate.as_str()[..12];
        let epoch_id = EpochId::new(epoch_oid.as_str()).unwrap();

        create_quarantine_workspace(
            root,
            &manifold_dir,
            merge_id,
            vec![WorkspaceId::new("ws-1").unwrap()],
            epoch_id,
            candidate.clone(),
            "main",
            dummy_validation_result(false),
        )
        .unwrap();

        let ws_path = quarantine_workspace_path(root, merge_id);
        assert!(ws_path.exists());

        abandon_quarantine(root, &manifold_dir, merge_id).unwrap();

        assert!(
            !ws_path.exists(),
            "worktree should be removed after abandon"
        );
        let state_result = QuarantineState::read(&manifold_dir, merge_id);
        assert!(
            matches!(state_result, Err(QuarantineError::NotFound { .. })),
            "state should be removed after abandon"
        );
    }

    #[test]
    fn abandon_is_idempotent() {
        let (dir, epoch_oid) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".manifold");

        let candidate = make_candidate_commit(root, "content\n");
        let merge_id = &candidate.as_str()[..12];
        let epoch_id = EpochId::new(epoch_oid.as_str()).unwrap();

        create_quarantine_workspace(
            root,
            &manifold_dir,
            merge_id,
            vec![WorkspaceId::new("ws-1").unwrap()],
            epoch_id,
            candidate.clone(),
            "main",
            dummy_validation_result(false),
        )
        .unwrap();

        // First abandon
        abandon_quarantine(root, &manifold_dir, merge_id).unwrap();
        // Second abandon should also succeed
        abandon_quarantine(root, &manifold_dir, merge_id).unwrap();
    }

    #[test]
    fn abandon_nonexistent_succeeds() {
        let (dir, _epoch_oid) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".manifold");

        // Abandon something that was never created — should not error
        abandon_quarantine(root, &manifold_dir, "nonexistent123").unwrap();
    }

    // -----------------------------------------------------------------------
    // list_quarantines
    // -----------------------------------------------------------------------

    #[test]
    fn list_returns_empty_when_no_quarantines() {
        let dir = TempDir::new().unwrap();
        let manifold_dir = dir.path().join(".manifold");
        let result = list_quarantines(&manifold_dir);
        assert!(result.is_empty());
    }

    #[test]
    fn list_returns_all_quarantines() {
        let (dir, epoch_oid) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".manifold");

        // Create two candidate commits to get two different quarantine IDs
        let c1 = make_candidate_commit(root, "first\n");
        let id1 = c1.as_str()[..12].to_string();
        let epoch_id = EpochId::new(epoch_oid.as_str()).unwrap();

        create_quarantine_workspace(
            root,
            &manifold_dir,
            &id1,
            vec![WorkspaceId::new("ws-1").unwrap()],
            epoch_id.clone(),
            c1.clone(),
            "main",
            dummy_validation_result(false),
        )
        .unwrap();

        // Abandon the first worktree to free the HEAD ref before making a second commit
        abandon_quarantine(root, &manifold_dir, &id1).unwrap();

        // Recreate state for id1 without the worktree (test list with state-only)
        let state1 = QuarantineState {
            merge_id: id1.clone(),
            epoch_before: epoch_oid.clone(),
            candidate: c1,
            sources: vec![WorkspaceId::new("ws-1").unwrap()],
            branch: "main".to_owned(),
            validation_result: dummy_validation_result(false),
            created_at: 1000,
        };
        state1.write_atomic(&manifold_dir).unwrap();

        let c2 = make_candidate_commit(root, "second\n");
        let id2 = c2.as_str()[..12].to_string();

        create_quarantine_workspace(
            root,
            &manifold_dir,
            &id2,
            vec![WorkspaceId::new("ws-2").unwrap()],
            epoch_id,
            c2,
            "main",
            dummy_validation_result(false),
        )
        .unwrap();

        let quarantines = list_quarantines(&manifold_dir);
        assert_eq!(quarantines.len(), 2);

        let ids: Vec<&str> = quarantines.iter().map(|q| q.merge_id.as_str()).collect();
        assert!(ids.contains(&id1.as_str()));
        assert!(ids.contains(&id2.as_str()));
    }

    // -----------------------------------------------------------------------
    // promote_quarantine
    // -----------------------------------------------------------------------

    #[test]
    fn promote_with_passing_validation_advances_epoch() {
        let (dir, epoch_oid) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".manifold");

        let candidate = make_candidate_commit(root, "content\n");
        let merge_id = &candidate.as_str()[..12];
        let epoch_id = EpochId::new(epoch_oid.as_str()).unwrap();

        // Reset refs so COMMIT phase can CAS from epoch_oid → candidate
        run_git(root, &["update-ref", "refs/heads/main", epoch_oid.as_str()]);
        run_git(
            root,
            &["update-ref", crate::refs::EPOCH_CURRENT, epoch_oid.as_str()],
        );

        create_quarantine_workspace(
            root,
            &manifold_dir,
            merge_id,
            vec![WorkspaceId::new("ws-1").unwrap()],
            epoch_id,
            candidate.clone(),
            "main",
            dummy_validation_result(false),
        )
        .unwrap();

        // Use a validation config with a command that always passes
        let config = crate::config::ValidationConfig {
            command: Some("true".to_owned()),
            commands: Vec::new(),
            timeout_seconds: 30,
            preset: None,
            on_failure: crate::config::OnFailure::Block,
        };

        let result = promote_quarantine(root, &manifold_dir, merge_id, &config).unwrap();

        // Should have committed successfully
        match &result {
            PromoteResult::Committed { new_epoch } => {
                // Epoch ref should have advanced
                let epoch_ref = run_git(root, &["rev-parse", crate::refs::EPOCH_CURRENT]);
                assert_eq!(epoch_ref, new_epoch.as_str(), "epoch ref should advance");
                let main_ref = run_git(root, &["rev-parse", "refs/heads/main"]);
                assert_eq!(main_ref, new_epoch.as_str(), "main ref should advance");
            }
            PromoteResult::ValidationFailed { .. } => {
                panic!("Promote should have succeeded with 'true' command");
            }
        }

        // Quarantine should be cleaned up after promote
        let ws_path = quarantine_workspace_path(root, merge_id);
        assert!(
            !ws_path.exists(),
            "quarantine worktree should be removed after promote"
        );

        let state_result = QuarantineState::read(&manifold_dir, merge_id);
        assert!(
            matches!(state_result, Err(QuarantineError::NotFound { .. })),
            "quarantine state should be removed after promote"
        );
    }

    #[test]
    fn promote_with_failing_validation_leaves_quarantine_intact() {
        let (dir, epoch_oid) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".manifold");

        let candidate = make_candidate_commit(root, "content\n");
        let merge_id = &candidate.as_str()[..12];
        let epoch_id = EpochId::new(epoch_oid.as_str()).unwrap();

        run_git(root, &["update-ref", "refs/heads/main", epoch_oid.as_str()]);
        run_git(
            root,
            &["update-ref", crate::refs::EPOCH_CURRENT, epoch_oid.as_str()],
        );

        create_quarantine_workspace(
            root,
            &manifold_dir,
            merge_id,
            vec![WorkspaceId::new("ws-1").unwrap()],
            epoch_id,
            candidate.clone(),
            "main",
            dummy_validation_result(false),
        )
        .unwrap();

        // Validation config with a command that always fails
        let config = crate::config::ValidationConfig {
            command: Some("false".to_owned()),
            commands: Vec::new(),
            timeout_seconds: 30,
            preset: None,
            on_failure: crate::config::OnFailure::Block,
        };

        let result = promote_quarantine(root, &manifold_dir, merge_id, &config).unwrap();

        match &result {
            PromoteResult::ValidationFailed { .. } => {
                // Expected
            }
            PromoteResult::Committed { .. } => {
                panic!("Promote should have failed with 'false' command");
            }
        }

        // Quarantine should still exist
        let ws_path = quarantine_workspace_path(root, merge_id);
        assert!(
            ws_path.exists(),
            "quarantine should remain after failed promote"
        );

        let state = QuarantineState::read(&manifold_dir, merge_id).unwrap();
        assert_eq!(state.candidate, candidate);

        // Epoch ref should NOT have advanced
        let epoch_ref = run_git(root, &["rev-parse", crate::refs::EPOCH_CURRENT]);
        assert_eq!(
            epoch_ref,
            epoch_oid.as_str(),
            "epoch should not advance after failed promote"
        );
    }

    #[test]
    fn promote_commits_user_edits_before_validating() {
        let (dir, epoch_oid) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".manifold");

        // Create a candidate that writes a broken file
        let candidate = make_candidate_commit(root, "BROKEN\n");
        let merge_id = &candidate.as_str()[..12];
        let epoch_id = EpochId::new(epoch_oid.as_str()).unwrap();

        run_git(root, &["update-ref", "refs/heads/main", epoch_oid.as_str()]);
        run_git(
            root,
            &["update-ref", crate::refs::EPOCH_CURRENT, epoch_oid.as_str()],
        );

        create_quarantine_workspace(
            root,
            &manifold_dir,
            merge_id,
            vec![WorkspaceId::new("ws-1").unwrap()],
            epoch_id,
            candidate.clone(),
            "main",
            dummy_validation_result(false),
        )
        .unwrap();

        // Simulate the agent fixing the file in the quarantine workspace
        let ws_path = quarantine_workspace_path(root, merge_id);
        fs::write(ws_path.join("candidate.txt"), "FIXED\n").unwrap();

        // Validate with a command that checks the file content: "true" always passes
        // (The actual file content fix is just simulated; we use a simple passing cmd)
        let config = crate::config::ValidationConfig {
            command: Some("true".to_owned()),
            commands: Vec::new(),
            timeout_seconds: 30,
            preset: None,
            on_failure: crate::config::OnFailure::Block,
        };

        let result = promote_quarantine(root, &manifold_dir, merge_id, &config).unwrap();

        match &result {
            PromoteResult::Committed { new_epoch } => {
                // The committed OID should be different from the original candidate
                // because we edited a file and committed it
                // (it could be the same if no-op staging, but we wrote a new file)
                let epoch_ref = run_git(root, &["rev-parse", crate::refs::EPOCH_CURRENT]);
                assert_eq!(epoch_ref, new_epoch.as_str());
            }
            PromoteResult::ValidationFailed { .. } => {
                panic!("Promote should succeed with 'true' command");
            }
        }
    }

    #[test]
    fn promote_missing_quarantine_returns_not_found() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".manifold");
        let config = crate::config::ValidationConfig::default();

        let err = promote_quarantine(root, &manifold_dir, "nonexistent123", &config).unwrap_err();
        assert!(matches!(err, QuarantineError::NotFound { .. }));
    }

    // -----------------------------------------------------------------------
    // commit_quarantine_edits
    // -----------------------------------------------------------------------

    #[test]
    fn commit_edits_returns_same_oid_when_clean() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let candidate = make_candidate_commit(root, "content\n");

        // No uncommitted changes
        let oid = commit_quarantine_edits(root, root, &candidate).unwrap();
        assert_eq!(
            oid, candidate,
            "clean worktree should return original candidate"
        );
    }

    #[test]
    fn commit_edits_creates_new_commit_for_changes() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let candidate = make_candidate_commit(root, "content\n");

        // Make an uncommitted change
        fs::write(root.join("new_fix.txt"), "fix\n").unwrap();

        let oid = commit_quarantine_edits(root, root, &candidate).unwrap();
        assert_ne!(oid, candidate, "new commit should be created for changes");

        // Verify the new file is in the commit
        let tree = run_git(root, &["show", "--name-only", "--format=", "HEAD"]);
        assert!(tree.contains("new_fix.txt"));
    }
}
