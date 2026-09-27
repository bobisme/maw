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
//!       creates <workspaces>/merge-quarantine-<id>/ (git worktree at candidate
//!       OID; .maw/workspaces/ in the consolidated layout, ws/ in legacy v2)
//!       writes .manifold/quarantine/<id>/state.json
//!
//! agent edits files in <workspaces>/merge-quarantine-<id>/ to fix the build failure
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

use maw_git::{GitRepo as _, GixRepo};
use serde::{Deserialize, Serialize};

use crate::config::ValidationConfig;
use crate::merge::validate::{ValidateOutcome, run_validate_config_in_dir};
use crate::merge_state::ValidationResult;
use crate::model::layout::LayoutFlavor;
use crate::model::types::{EpochId, GitOid, WorkspaceId};
pub use maw_core::merge::quarantine_id::{
    InvalidMergeId, MERGE_ID_MAX_LEN, QUARANTINE_NAME_PREFIX, merge_id_from_name,
    quarantine_workspace_name, validate_merge_id,
};
use maw_core::merge::quarantine_id::{quarantine_state_base, quarantine_state_dir};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Legacy (v2) directory, relative to the repo root, that older maw versions
/// used for quarantine worktrees in every layout. Consolidated repos may still
/// have quarantines there from before bn-1cth; they are found as a fallback.
const LEGACY_WS_DIR: &str = "ws";

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
        check_merge_id(merge_id)?;
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
    #[allow(clippy::missing_errors_doc)]
    pub fn write_atomic(&self, manifold_dir: &Path) -> Result<(), QuarantineError> {
        check_merge_id(&self.merge_id)?;
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
    /// The supplied `merge_id` is not a valid quarantine id (e.g. contains
    /// `/` or `..`). Rejected before any filesystem access.
    InvalidId {
        merge_id: String,
        reason: InvalidMergeId,
    },
    /// No quarantine with the given `merge_id` exists.
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
    /// A `ws merge` journal (`merge-state.json`) is still in progress (live
    /// or crashed). Promote refuses: moving the epoch under it would turn a
    /// recoverable crashed merge into one `ws merge --abort` refuses to
    /// clear (bn-3w2b).
    MergeInProgress { phase: String },
    /// Refused to remove an existing quarantine worktree because no valid
    /// [`RemovalProof`] shows its state is recoverable (bn-jfj2). Nothing was
    /// removed.
    Unpinned { merge_id: String, reason: String },
}

impl std::fmt::Display for QuarantineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidId { merge_id, reason } => {
                write!(
                    f,
                    "invalid quarantine id {merge_id:?}: {reason}\n  \
                     List active quarantines: maw merge list"
                )
            }
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
            Self::MergeInProgress { phase } => write!(
                f,
                "a `maw ws merge` is in progress (merge-state phase: {phase}); \
                 refusing to promote while its journal exists.\n  \
                 Finish or recover it first: maw ws merge --abort"
            ),
            Self::Unpinned { merge_id, reason } => write!(
                f,
                "refusing to remove quarantine '{merge_id}' worktree: its state is not \
                 proven recoverable ({reason}); nothing was removed"
            ),
        }
    }
}

impl std::error::Error for QuarantineError {}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Validate `merge_id`, mapping failure to [`QuarantineError::InvalidId`].
fn check_merge_id(merge_id: &str) -> Result<(), QuarantineError> {
    validate_merge_id(merge_id).map_err(|reason| QuarantineError::InvalidId {
        merge_id: merge_id.to_owned(),
        reason,
    })
}

/// Return the workspace path for a quarantine with the given `merge_id`.
///
/// Layout-aware: `<root>/.maw/workspaces/merge-quarantine-<id>` in the
/// consolidated layout, `<root>/ws/merge-quarantine-<id>` in the legacy v2
/// layout. For an existing quarantine created by an older maw in a
/// consolidated repo, the legacy `<root>/ws/` location is returned when the
/// worktree exists only there.
///
/// # Errors
///
/// Returns [`QuarantineError::InvalidId`] if `merge_id` fails
/// [`validate_merge_id`].
pub fn quarantine_workspace_path(
    repo_root: &Path,
    merge_id: &str,
) -> Result<PathBuf, QuarantineError> {
    check_merge_id(merge_id)?;
    let name = quarantine_workspace_name(merge_id);
    let primary = LayoutFlavor::detect_with_env(repo_root).workspace_path(repo_root, &name);
    if !primary.exists() {
        let legacy = repo_root.join(LEGACY_WS_DIR).join(&name);
        if legacy != primary && legacy.exists() {
            return Ok(legacy);
        }
    }
    Ok(primary)
}

/// Return the directory that holds the quarantine state files.
///
/// Callers must have validated `merge_id` (all public entry points do).
fn state_dir(manifold_dir: &Path, merge_id: &str) -> PathBuf {
    quarantine_state_dir(manifold_dir, merge_id)
}

/// Return the path to the quarantine state file.
fn state_path(manifold_dir: &Path, merge_id: &str) -> PathBuf {
    state_dir(manifold_dir, merge_id).join("state.json")
}

// ---------------------------------------------------------------------------
// create_quarantine_workspace
// ---------------------------------------------------------------------------

/// Create a quarantine workspace for a failed merge.
///
/// 1. Creates a git worktree for workspace `merge-quarantine-<merge_id>` in
///    the layout's workspaces dir (see [`quarantine_workspace_path`]),
///    checked out at `candidate`.
/// 2. Writes validation diagnostics to the quarantine state directory.
/// 3. Writes a `state.json` with merge intent (sources, `epoch_before`, candidate).
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
#[allow(clippy::too_many_arguments)]
pub fn create_quarantine_workspace(
    repo_root: &Path,
    manifold_dir: &Path,
    merge_id: &str,
    sources: Vec<WorkspaceId>,
    epoch_before: &EpochId,
    candidate: GitOid,
    branch: &str,
    validation_result: ValidationResult,
) -> Result<PathBuf, QuarantineError> {
    check_merge_id(merge_id)?;
    let workspace_name = quarantine_workspace_name(merge_id);
    let workspace_path =
        LayoutFlavor::detect_with_env(repo_root).workspace_path(repo_root, &workspace_name);

    // Remove any stale worktree at this path (idempotent — previous partial failure)
    if workspace_path.exists() {
        let _ = remove_worktree(repo_root, &workspace_path);
        let _ = fs::remove_dir_all(&workspace_path);
    }

    // Ensure the workspaces directory exists
    if let Some(ws_dir) = workspace_path.parent() {
        fs::create_dir_all(ws_dir).map_err(|e| {
            QuarantineError::Io(format!("create workspaces dir {}: {e}", ws_dir.display()))
        })?;
    }

    // Create a detached git worktree at the candidate commit.
    let repo =
        GixRepo::open(repo_root).map_err(|e| QuarantineError::Git(format!("open repo: {e}")))?;
    // If a prior partial attempt left an admin dir, remove it so worktree_add succeeds.
    let admin_dir = repo.common_dir().join("worktrees").join(&workspace_name);
    if admin_dir.exists() {
        let _ = std::fs::remove_dir_all(&admin_dir);
    }
    let target: maw_git::GitOid = candidate
        .as_str()
        .parse()
        .map_err(|e| QuarantineError::Git(format!("parse candidate oid: {e}")))?;
    repo.worktree_add(&workspace_name, target, &workspace_path)
        .map_err(|e| {
            QuarantineError::Git(format!("git worktree add for quarantine failed: {e}"))
        })?;

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
/// **Locking (bn-3w2b):** the caller must hold the repo epoch lock for the
/// whole call (`maw merge promote` does), exactly like `ws merge`: the epoch
/// and branch refs are read-modify-written, and FF-absorb's plain
/// `write_epoch_current` relies solely on that lock for exclusion.
///
/// 0. Refuse if a `ws merge` journal is in progress (see
///    [`QuarantineError::MergeInProgress`]).
/// 1. Read the quarantine state (`epoch_before`, candidate, branch, sources).
/// 2. Stage and commit any uncommitted changes in the quarantine workspace
///    (a no-op if there are no changes, preserving the original candidate OID).
/// 3. Re-run validation commands in the quarantine workspace directory.
/// 4. If validation passes:
///    a. Advance `refs/manifold/epoch/current` and `refs/heads/<branch>`
///    from `epoch_before` to the promoted commit in ONE atomic 2-ref CAS
///    (as `ws merge`'s COMMIT does): both move or neither does.
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

    // 0. Refuse while a `ws merge` journal exists (live or crashed). Under
    // the epoch lock no live merge can be running, so this is a crashed
    // merge awaiting recovery; advancing the epoch now would strand it
    // (`--abort` refuses once the epoch moved away from its epoch_before —
    // Stateright `fast_quarantine_promote_vs_merge`, "crash recovery
    // converges").
    let merge_state_path = crate::merge_state::MergeStateFile::default_path(manifold_dir);
    match crate::merge_state::MergeStateFile::read(&merge_state_path) {
        Ok(ms) if !ms.phase.is_terminal() => {
            return Err(QuarantineError::MergeInProgress {
                phase: ms.phase.to_string(),
            });
        }
        Ok(_) | Err(crate::merge_state::MergeStateError::NotFound(_)) => {}
        Err(e) => {
            return Err(QuarantineError::Io(format!(
                "read {}: {e}",
                merge_state_path.display()
            )));
        }
    }

    let ws_path = quarantine_workspace_path(repo_root, merge_id)?;
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
            // 4a. Advance epoch + branch refs in ONE atomic 2-ref CAS
            // (bn-3w2b; mirrors src/merge/commit.rs). The pre-fix shape —
            // no epoch lock, epoch CAS then a separate branch CAS — could
            // leave the refs split (the Stateright
            // `mutation_pre_bn_3w2b_promote_split_cas_breaks_atomicity`) and
            // let a concurrent FF-absorb's plain epoch write overwrite the
            // promoted epoch (`..._promote_unlocked_regresses_epoch`).
            let epoch_before_oid = state.epoch_before.clone();
            let branch_ref = format!("refs/heads/{}", state.branch);
            crate::refs::update_refs_atomic(
                repo_root,
                &[
                    (crate::refs::EPOCH_CURRENT, &epoch_before_oid, &commit_oid),
                    (&branch_ref, &epoch_before_oid, &commit_oid),
                ],
            )
            .map_err(|e| {
                QuarantineError::Commit(format!(
                    "advance epoch + branch '{}' from {}: {e}\n  \
                     The epoch or branch moved since this quarantine was created; \
                     neither ref was changed. Abandon it and re-run the merge: \
                     maw merge abandon {merge_id}\n  \
                     The quarantine's content (including any fix) is commit {}; \
                     abandon pins it first, recover it with: \
                     maw ws recover {}",
                    state.branch,
                    &epoch_before_oid.as_str()[..12],
                    commit_oid.as_str(),
                    quarantine_workspace_name(merge_id)
                ))
            })?;

            // 4b. The quarantine worktree is NOT removed here (bn-jfj2).
            // Validation just ran inside it, so it may hold bytes written
            // after step 2 committed the edits; removing it is the caller's
            // job, via `abandon_quarantine` with a [`RemovalProof`] that the
            // worktree's state is recoverable.
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

/// Evidence that a quarantine worktree's state is recoverable, required by
/// [`abandon_quarantine`] before it deletes the worktree (bn-jfj2).
///
/// Since bn-3rhz promote builds on the quarantine's HEAD, so agents commit
/// fixes inside the quarantine; the worktree's HEAD is then the only ref to
/// those commits. The Prime Invariant forbids deleting it without a recovery
/// point. The recovery machinery (`capture_before_destroy` + destroy record)
/// lives in the CLI crate, so the caller captures and hands the result here;
/// `abandon_quarantine` re-checks the claim against the repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemovalProof {
    /// The worktree state is pinned at `recovery_ref` (under
    /// `refs/manifold/recovery/`), which resolves to `oid`. The worktree's
    /// HEAD must be `oid` or an ancestor of it (a dirty snapshot's parent).
    Pinned {
        /// Full recovery ref name.
        recovery_ref: String,
        /// Commit the ref resolves to.
        oid: GitOid,
    },
    /// The caller verified the worktree has no uncommitted state (including
    /// stat-cache-masked bytes) and its HEAD is reachable from `reachable_from`
    /// (e.g. the branch a promote just advanced). `abandon_quarantine`
    /// re-checks the reachability; cleanliness is the caller's claim.
    CleanAndReachable {
        /// A ref whose tip must have the worktree HEAD as an ancestor.
        reachable_from: String,
    },
}

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
/// * `proof` — Evidence the worktree's state is recoverable. Required (and
///   verified) whenever the worktree still exists; may be `None` only when it
///   is already gone.
///
/// # Errors
///
/// Returns [`QuarantineError::Unpinned`] if the worktree exists and `proof` is
/// missing or does not check out — nothing is removed in that case.
/// Returns [`QuarantineError::Git`] if the git worktree removal fails in a
/// way that is not "worktree not found".
pub fn abandon_quarantine(
    repo_root: &Path,
    manifold_dir: &Path,
    merge_id: &str,
    proof: Option<&RemovalProof>,
) -> Result<(), QuarantineError> {
    let ws_path = quarantine_workspace_path(repo_root, merge_id)?;

    // Remove the git worktree (idempotent — ignore "not registered" errors)
    if ws_path.exists() {
        // bn-jfj2: fail closed — never delete a worktree whose state has not
        // been shown recoverable.
        verify_removal_proof(repo_root, &ws_path, merge_id, proof)?;
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
#[must_use]
pub fn list_quarantines(manifold_dir: &Path) -> Vec<QuarantineState> {
    let quarantine_base = quarantine_state_base(manifold_dir);
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
        if validate_merge_id(&merge_id).is_err() {
            continue;
        }
        if let Ok(state) = QuarantineState::read(manifold_dir, &merge_id) {
            // The state must describe the directory it lives in; a mismatched
            // (hand-edited) id would make promote/abandon address other paths.
            if state.merge_id == merge_id {
                result.push(state);
            }
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
    _repo_root: &Path,
    ws_path: &Path,
    original_candidate: &GitOid,
) -> Result<GitOid, QuarantineError> {
    // Open the quarantine worktree as its own gix repo.
    let ws_repo = GixRepo::open(ws_path)
        .map_err(|e| QuarantineError::Git(format!("open quarantine worktree: {e}")))?;

    // worktree_state_commit() captures all status edits (mod/add/del/untracked)
    // and returns the new commit OID — but, like `git stash create`, it does
    // NOT advance HEAD. The original CLI implementation used `git commit`,
    // which DOES move HEAD, and downstream `git show HEAD` / re-validation
    // depends on that. So after producing the commit we manually advance the
    // detached HEAD to it (parents = [previous HEAD]).
    let new_oid_opt = ws_repo
        .worktree_state_commit("quarantine: fix-forward")
        .map_err(|e| QuarantineError::Git(format!("worktree_state_commit failed: {e}")))?;
    // bn-3rhz: the promoted commit is built on the worktree's CURRENT HEAD,
    // not the recorded candidate. An agent that fixed the quarantine with
    // `git commit` has HEAD ahead of the candidate and a clean worktree;
    // returning the candidate here promoted the unfixed commit (validation
    // had just run on the fixed worktree) and the cleanup then removed the
    // worktree holding the only ref to the fix. HEAD must descend from the
    // candidate: anything else would move the epoch to unrelated history.
    let head = ws_repo
        .rev_parse_opt("HEAD")
        .map_err(|e| QuarantineError::Git(format!("read quarantine HEAD: {e}")))?
        .ok_or_else(|| QuarantineError::Git("quarantine HEAD is unborn".to_owned()))?;
    let candidate_git: maw_git::GitOid = original_candidate
        .as_str()
        .parse()
        .map_err(|e| QuarantineError::Git(format!("parse candidate OID: {e}")))?;
    if head != candidate_git
        && !ws_repo
            .is_ancestor(candidate_git, head)
            .map_err(|e| QuarantineError::Git(format!("ancestry check: {e}")))?
    {
        return Err(QuarantineError::Git(format!(
            "quarantine HEAD {head} does not descend from its candidate {}; \
             refusing to promote unrelated history",
            original_candidate.as_str()
        )));
    }
    let Some(new_head) = new_oid_opt else {
        // No worktree edits — promote HEAD (the candidate, or the agent's
        // commits on top of it).
        return GitOid::new(&head.to_string())
            .map_err(|e| QuarantineError::Git(format!("parse HEAD OID: {e}")));
    };
    // Quarantine worktrees are created detached (HEAD = raw OID), so we
    // advance HEAD by rewriting the file directly. We then rebuild the
    // index from the new HEAD's tree — the old git CLI path ran
    // `git add -A && git commit`, which left the index matching HEAD.
    // Without this step the index would still match the pre-fix-forward
    // commit, causing downstream `git status` / index-vs-HEAD checks to
    // report phantom staged changes.
    //
    // bn-jfj2: the move goes through maw-git's guarded HEAD mover (atomic
    // write under `HEAD.lock` + reflog entry), never a raw `fs::write` that
    // leaves no reflog trail. CAS against the HEAD we just validated so a
    // concurrent `git commit` in the quarantine is never silently moved off.
    ws_repo.set_head_detached_cas(head, new_head).map_err(|e| {
        QuarantineError::Git(format!(
            "failed to advance quarantine HEAD from {head} to {new_head}: {e}"
        ))
    })?;
    ws_repo.unstage_all().map_err(|e| {
        QuarantineError::Git(format!(
            "failed to rebuild index after quarantine commit: {e}"
        ))
    })?;
    GitOid::new(&new_head.to_string())
        .map_err(|e| QuarantineError::Git(format!("parse HEAD OID: {e}")))
}

/// Check a [`RemovalProof`] against the repository before
/// [`abandon_quarantine`] deletes `ws_path` (bn-jfj2).
fn verify_removal_proof(
    repo_root: &Path,
    ws_path: &Path,
    merge_id: &str,
    proof: Option<&RemovalProof>,
) -> Result<(), QuarantineError> {
    let unpinned = |reason: String| QuarantineError::Unpinned {
        merge_id: merge_id.to_owned(),
        reason,
    };
    let Some(proof) = proof else {
        return Err(unpinned(
            "no recovery pin was provided for the existing worktree".to_owned(),
        ));
    };
    let ws_repo =
        GixRepo::open(ws_path).map_err(|e| unpinned(format!("open quarantine worktree: {e}")))?;
    let head = ws_repo
        .rev_parse_opt("HEAD")
        .map_err(|e| unpinned(format!("read quarantine HEAD: {e}")))?
        .ok_or_else(|| unpinned("quarantine HEAD is unborn".to_owned()))?;
    let repo = GixRepo::open(repo_root).map_err(|e| unpinned(format!("open repo: {e}")))?;
    let (anchor_ref, anchor) = match proof {
        RemovalProof::Pinned { recovery_ref, oid } => {
            if !recovery_ref.starts_with("refs/manifold/recovery/") {
                return Err(unpinned(format!(
                    "{recovery_ref} is not a recovery ref (refs/manifold/recovery/)"
                )));
            }
            let resolved = repo
                .rev_parse_opt(recovery_ref)
                .map_err(|e| unpinned(format!("resolve {recovery_ref}: {e}")))?
                .ok_or_else(|| unpinned(format!("{recovery_ref} does not exist")))?;
            if resolved.to_string() != oid.as_str() {
                return Err(unpinned(format!(
                    "{recovery_ref} resolves to {resolved}, not the captured {}",
                    oid.as_str()
                )));
            }
            (recovery_ref.as_str(), resolved)
        }
        RemovalProof::CleanAndReachable { reachable_from } => {
            let tip = repo
                .rev_parse_opt(reachable_from)
                .map_err(|e| unpinned(format!("resolve {reachable_from}: {e}")))?
                .ok_or_else(|| unpinned(format!("{reachable_from} does not exist")))?;
            (reachable_from.as_str(), tip)
        }
    };
    if head != anchor
        && !repo
            .is_ancestor(head, anchor)
            .map_err(|e| unpinned(format!("ancestry check: {e}")))?
    {
        return Err(unpinned(format!(
            "quarantine HEAD {head} is not reachable from {anchor_ref}"
        )));
    }
    Ok(())
}

/// Remove a quarantine git worktree.
///
/// Idempotent: if no admin dir exists at `<git_dir>/worktrees/<basename>`,
/// silently succeeds. The admin-dir name is derived from `path.file_name()`
/// because [`create_quarantine_workspace`] always registers the worktree under
/// the quarantine workspace name (which is the basename of the worktree
/// directory).
fn remove_worktree(repo_root: &Path, path: &Path) -> Result<(), QuarantineError> {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return Err(QuarantineError::Git(format!(
            "invalid quarantine worktree path (no basename): {}",
            path.display()
        )));
    };
    let repo =
        GixRepo::open(repo_root).map_err(|e| QuarantineError::Git(format!("open repo: {e}")))?;
    let admin_dir = repo.common_dir().join("worktrees").join(name);
    if !admin_dir.exists() {
        // Already pruned/never registered — match the prior "not a worktree" branch.
        return Ok(());
    }
    repo.worktree_remove(name)
        .map_err(|e| QuarantineError::Git(format!("git worktree remove failed: {e}")))?;
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
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
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
            .expect("operation should succeed");
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
        let dir = TempDir::new().expect("operation should succeed");
        let root = dir.path();

        run_git(root, &["init"]);
        run_git(root, &["config", "user.name", "Test"]);
        run_git(root, &["config", "user.email", "test@test.com"]);
        run_git(root, &["config", "commit.gpgsign", "false"]);

        fs::write(root.join("README.md"), "# Test\n").expect("operation should succeed");
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", "initial"]);
        run_git(root, &["branch", "-M", "main"]);

        let oid_str = run_git(root, &["rev-parse", "HEAD"]);
        let oid = GitOid::new(&oid_str).expect("operation should succeed");

        run_git(root, &["update-ref", crate::refs::EPOCH_CURRENT, &oid_str]);

        (dir, oid)
    }

    /// Create a second commit (candidate commit) in the repo.
    /// Pin the quarantine worktree's HEAD under a recovery ref and return the
    /// matching [`RemovalProof`] (`None` when the worktree does not exist).
    fn pin(root: &Path, merge_id: &str) -> Option<RemovalProof> {
        let ws = quarantine_workspace_path(root, merge_id).ok()?;
        if !ws.exists() {
            return None;
        }
        let head = run_git(&ws, &["rev-parse", "HEAD"]);
        let recovery_ref = format!(
            "refs/manifold/recovery/{}/test",
            quarantine_workspace_name(merge_id)
        );
        run_git(root, &["update-ref", &recovery_ref, &head]);
        Some(RemovalProof::Pinned {
            recovery_ref,
            oid: GitOid::new(&head).unwrap(),
        })
    }

    fn make_candidate_commit(root: &Path, content: &str) -> GitOid {
        fs::write(root.join("candidate.txt"), content).expect("operation should succeed");
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", "candidate"]);
        let oid_str = run_git(root, &["rev-parse", "HEAD"]);
        GitOid::new(&oid_str).expect("operation should succeed")
    }

    fn dummy_validation_result(passed: bool) -> ValidationResult {
        ValidationResult {
            passed,
            exit_code: Some(i32::from(!passed)),
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
        let dir = TempDir::new().expect("operation should succeed");
        let manifold_dir = dir.path().join(".manifold");

        let oid = GitOid::new(&"a".repeat(40)).expect("operation should succeed");
        let epoch = EpochId::new(&"b".repeat(40)).expect("operation should succeed");
        let state = QuarantineState {
            merge_id: "abc123def456".to_owned(),
            epoch_before: epoch.oid().clone(),
            candidate: oid.clone(),
            sources: vec![WorkspaceId::new("ws-1").expect("operation should succeed")],
            branch: "main".to_owned(),
            validation_result: dummy_validation_result(false),
            created_at: 1000,
        };

        state
            .write_atomic(&manifold_dir)
            .expect("operation should succeed");

        let loaded =
            QuarantineState::read(&manifold_dir, "abc123def456").expect("operation should succeed");
        assert_eq!(loaded.merge_id, "abc123def456");
        assert_eq!(loaded.epoch_before, *epoch.oid());
        assert_eq!(loaded.candidate, oid);
        assert_eq!(loaded.branch, "main");
        assert_eq!(loaded.created_at, 1000);
    }

    #[test]
    fn state_not_found_error() {
        let dir = TempDir::new().expect("operation should succeed");
        let manifold_dir = dir.path().join(".manifold");
        let err =
            QuarantineState::read(&manifold_dir, "nonexistent").expect_err("operation should fail");
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
        let epoch_id = EpochId::new(epoch_oid.as_str()).expect("operation should succeed");

        let ws_path = create_quarantine_workspace(
            root,
            &manifold_dir,
            merge_id,
            vec![WorkspaceId::new("ws-1").expect("operation should succeed")],
            &epoch_id,
            candidate.clone(),
            "main",
            dummy_validation_result(false),
        )
        .expect("operation should succeed");

        // Workspace directory exists
        assert!(ws_path.exists(), "quarantine worktree should exist");

        // Workspace is checked out at the candidate commit
        let head = run_git(&ws_path, &["rev-parse", "HEAD"]);
        assert_eq!(head, candidate.as_str(), "worktree should be at candidate");

        // State file exists and is valid
        let state =
            QuarantineState::read(&manifold_dir, merge_id).expect("operation should succeed");
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
        let epoch_id = EpochId::new(epoch_oid.as_str()).expect("operation should succeed");

        // First creation
        create_quarantine_workspace(
            root,
            &manifold_dir,
            merge_id,
            vec![WorkspaceId::new("ws-1").expect("operation should succeed")],
            &epoch_id,
            candidate.clone(),
            "main",
            dummy_validation_result(false),
        )
        .expect("operation should succeed");

        // Second creation should succeed (idempotent)
        let ws_path = create_quarantine_workspace(
            root,
            &manifold_dir,
            merge_id,
            vec![WorkspaceId::new("ws-1").expect("operation should succeed")],
            &epoch_id,
            candidate.clone(),
            "main",
            dummy_validation_result(false),
        )
        .expect("operation should succeed");

        assert!(ws_path.exists());
        let state =
            QuarantineState::read(&manifold_dir, merge_id).expect("operation should succeed");
        assert_eq!(state.candidate, candidate);
    }

    #[test]
    fn create_writes_validation_diagnostics() {
        let (dir, epoch_oid) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".manifold");

        let candidate = make_candidate_commit(root, "content\n");
        let merge_id = &candidate.as_str()[..12];
        let epoch_id = EpochId::new(epoch_oid.as_str()).expect("operation should succeed");
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
            vec![WorkspaceId::new("ws-1").expect("operation should succeed")],
            &epoch_id,
            candidate.clone(),
            "main",
            vr,
        )
        .expect("operation should succeed");

        // validation.json should exist in state dir
        let val_json = manifold_dir
            .join(maw_core::merge::quarantine_id::QUARANTINE_STATE_SUBDIR)
            .join(merge_id)
            .join("validation.json");
        assert!(val_json.exists(), "validation.json should be written");

        let contents = fs::read_to_string(&val_json).expect("operation should succeed");
        let decoded: ValidationResult =
            serde_json::from_str(&contents).expect("operation should succeed");
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
        let epoch_id = EpochId::new(epoch_oid.as_str()).expect("operation should succeed");

        create_quarantine_workspace(
            root,
            &manifold_dir,
            merge_id,
            vec![WorkspaceId::new("ws-1").expect("operation should succeed")],
            &epoch_id,
            candidate.clone(),
            "main",
            dummy_validation_result(false),
        )
        .expect("operation should succeed");

        let ws_path = quarantine_workspace_path(root, merge_id).unwrap();
        assert!(ws_path.exists());

        abandon_quarantine(root, &manifold_dir, merge_id, pin(root, merge_id).as_ref())
            .expect("operation should succeed");

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

    /// Create a quarantine at a fresh candidate; returns (dir, manifold, id, ws).
    fn quarantine_fixture() -> (TempDir, PathBuf, String, PathBuf) {
        let (dir, epoch_oid) = setup_repo();
        let root = dir.path().to_path_buf();
        let manifold_dir = root.join(".manifold");
        let candidate = make_candidate_commit(&root, "content\n");
        let merge_id = candidate.as_str()[..12].to_owned();
        create_quarantine_workspace(
            &root,
            &manifold_dir,
            &merge_id,
            vec![WorkspaceId::new("ws-1").unwrap()],
            &EpochId::new(epoch_oid.as_str()).unwrap(),
            candidate,
            "main",
            dummy_validation_result(false),
        )
        .unwrap();
        let ws = quarantine_workspace_path(&root, &merge_id).unwrap();
        (dir, manifold_dir, merge_id, ws)
    }

    fn commit_in(ws: &Path, file: &str) -> String {
        fs::write(ws.join(file), "fix\n").unwrap();
        run_git(ws, &["add", file]);
        run_git(
            ws,
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "fix",
            ],
        );
        run_git(ws, &["rev-parse", "HEAD"])
    }

    #[test]
    fn abandon_refuses_existing_worktree_without_proof() {
        let (dir, manifold_dir, merge_id, ws) = quarantine_fixture();
        let err = abandon_quarantine(dir.path(), &manifold_dir, &merge_id, None)
            .expect_err("abandon without proof must refuse (bn-jfj2)");
        assert!(matches!(err, QuarantineError::Unpinned { .. }), "{err}");
        assert!(ws.exists(), "worktree must survive a refused abandon");
        assert!(QuarantineState::read(&manifold_dir, &merge_id).is_ok());
    }

    #[test]
    fn abandon_refuses_proof_that_does_not_cover_head() {
        let (dir, manifold_dir, merge_id, ws) = quarantine_fixture();
        let root = dir.path();
        // Pin the candidate, then commit a fix: the pin no longer covers HEAD.
        let proof = pin(root, &merge_id).unwrap();
        commit_in(&ws, "fix.txt");
        let err = abandon_quarantine(root, &manifold_dir, &merge_id, Some(&proof))
            .expect_err("stale pin must refuse");
        assert!(matches!(err, QuarantineError::Unpinned { .. }), "{err}");
        assert!(ws.exists());

        // A ref outside refs/manifold/recovery/ is not a pin.
        let head = run_git(&ws, &["rev-parse", "HEAD"]);
        run_git(root, &["update-ref", "refs/heads/elsewhere", &head]);
        let bad = RemovalProof::Pinned {
            recovery_ref: "refs/heads/elsewhere".to_owned(),
            oid: GitOid::new(&head).unwrap(),
        };
        assert!(matches!(
            abandon_quarantine(root, &manifold_dir, &merge_id, Some(&bad)),
            Err(QuarantineError::Unpinned { .. })
        ));
        // A proof whose oid disagrees with what the ref resolves to.
        let r = format!(
            "refs/manifold/recovery/{}/x",
            quarantine_workspace_name(&merge_id)
        );
        run_git(root, &["update-ref", &r, &head]);
        let mismatched = RemovalProof::Pinned {
            recovery_ref: r.clone(),
            oid: GitOid::new(&"0".repeat(40)).unwrap(),
        };
        assert!(matches!(
            abandon_quarantine(root, &manifold_dir, &merge_id, Some(&mismatched)),
            Err(QuarantineError::Unpinned { .. })
        ));
        // Clean-and-reachable from a ref that does not contain HEAD.
        let unreachable = RemovalProof::CleanAndReachable {
            reachable_from: "refs/heads/main".to_owned(),
        };
        assert!(matches!(
            abandon_quarantine(root, &manifold_dir, &merge_id, Some(&unreachable)),
            Err(QuarantineError::Unpinned { .. })
        ));
        assert!(
            ws.exists(),
            "every refused proof leaves the worktree intact"
        );

        // The correct pin succeeds.
        let good = RemovalProof::Pinned {
            recovery_ref: r,
            oid: GitOid::new(&head).unwrap(),
        };
        abandon_quarantine(root, &manifold_dir, &merge_id, Some(&good)).expect("abandon");
        assert!(!ws.exists());
    }

    #[test]
    fn commit_edits_moves_head_with_reflog() {
        let (_dir, _m, _id, ws) = quarantine_fixture();
        let before = run_git(&ws, &["rev-parse", "HEAD"]);
        fs::write(ws.join("edit.txt"), "e\n").unwrap();
        let cand = GitOid::new(&before).unwrap();
        let new = commit_quarantine_edits(&ws, &ws, &cand).expect("commit");
        assert_ne!(new.as_str(), before);
        assert_eq!(run_git(&ws, &["rev-parse", "HEAD"]), new.as_str());
        // bn-jfj2: the guarded HEAD mover writes a reflog entry; a raw
        // fs::write of HEAD left none.
        let reflog = run_git(&ws, &["reflog", "--format=%H", "HEAD"]);
        assert!(
            reflog.lines().next() == Some(new.as_str()),
            "HEAD move must be recorded in the reflog:\n{reflog}"
        );
    }

    #[test]
    fn abandon_is_idempotent() {
        let (dir, epoch_oid) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".manifold");

        let candidate = make_candidate_commit(root, "content\n");
        let merge_id = &candidate.as_str()[..12];
        let epoch_id = EpochId::new(epoch_oid.as_str()).expect("operation should succeed");

        create_quarantine_workspace(
            root,
            &manifold_dir,
            merge_id,
            vec![WorkspaceId::new("ws-1").expect("operation should succeed")],
            &epoch_id,
            candidate.clone(),
            "main",
            dummy_validation_result(false),
        )
        .expect("operation should succeed");

        // First abandon
        abandon_quarantine(root, &manifold_dir, merge_id, pin(root, merge_id).as_ref())
            .expect("operation should succeed");
        // Second abandon should also succeed
        abandon_quarantine(root, &manifold_dir, merge_id, pin(root, merge_id).as_ref())
            .expect("operation should succeed");
    }

    #[test]
    fn abandon_nonexistent_succeeds() {
        let (dir, _epoch_oid) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".manifold");

        // Abandon something that was never created — should not error
        abandon_quarantine(
            root,
            &manifold_dir,
            "nonexistent123",
            pin(root, "nonexistent123").as_ref(),
        )
        .expect("operation should succeed");
    }

    // -----------------------------------------------------------------------
    // list_quarantines
    // -----------------------------------------------------------------------

    #[test]
    fn list_returns_empty_when_no_quarantines() {
        let dir = TempDir::new().expect("operation should succeed");
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
        let epoch_id = EpochId::new(epoch_oid.as_str()).expect("operation should succeed");

        create_quarantine_workspace(
            root,
            &manifold_dir,
            &id1,
            vec![WorkspaceId::new("ws-1").expect("operation should succeed")],
            &epoch_id,
            c1.clone(),
            "main",
            dummy_validation_result(false),
        )
        .expect("operation should succeed");

        // Abandon the first worktree to free the HEAD ref before making a second commit
        abandon_quarantine(root, &manifold_dir, &id1, pin(root, &id1).as_ref())
            .expect("operation should succeed");

        // Recreate state for id1 without the worktree (test list with state-only)
        let state1 = QuarantineState {
            merge_id: id1.clone(),
            epoch_before: epoch_oid,
            candidate: c1,
            sources: vec![WorkspaceId::new("ws-1").expect("operation should succeed")],
            branch: "main".to_owned(),
            validation_result: dummy_validation_result(false),
            created_at: 1000,
        };
        state1
            .write_atomic(&manifold_dir)
            .expect("operation should succeed");

        let c2 = make_candidate_commit(root, "second\n");
        let id2 = c2.as_str()[..12].to_string();

        create_quarantine_workspace(
            root,
            &manifold_dir,
            &id2,
            vec![WorkspaceId::new("ws-2").expect("operation should succeed")],
            &epoch_id,
            c2,
            "main",
            dummy_validation_result(false),
        )
        .expect("operation should succeed");

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
        let epoch_id = EpochId::new(epoch_oid.as_str()).expect("operation should succeed");

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
            vec![WorkspaceId::new("ws-1").expect("operation should succeed")],
            &epoch_id,
            candidate.clone(),
            "main",
            dummy_validation_result(false),
        )
        .expect("operation should succeed");

        // Use a validation config with a command that always passes
        let config = crate::config::ValidationConfig {
            command: Some("true".to_owned()),
            commands: Vec::new(),
            timeout_seconds: 30,
            preset: None,
            on_failure: crate::config::OnFailure::Block,
        };

        let result = promote_quarantine(root, &manifold_dir, merge_id, &config)
            .expect("operation should succeed");

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

        // bn-jfj2: promote no longer removes the worktree itself — the caller
        // pins its state and calls `abandon_quarantine` with the proof.
        let ws_path = quarantine_workspace_path(root, merge_id).unwrap();
        assert!(
            ws_path.exists(),
            "promote must leave worktree removal to the caller (bn-jfj2)"
        );

        // The promoted HEAD is on main, so a clean worktree can be removed
        // on the strength of reachability alone.
        let proof = RemovalProof::CleanAndReachable {
            reachable_from: "refs/heads/main".to_owned(),
        };
        abandon_quarantine(root, &manifold_dir, merge_id, Some(&proof)).expect("cleanup");
        assert!(!ws_path.exists());
        let state_result = QuarantineState::read(&manifold_dir, merge_id);
        assert!(
            matches!(state_result, Err(QuarantineError::NotFound { .. })),
            "quarantine state should be removed after cleanup"
        );
    }

    /// Shared setup for the bn-3w2b promote tests: a quarantine whose
    /// candidate sits on `epoch`, with epoch = main = `epoch`.
    fn setup_promotable_quarantine(root: &Path, manifold_dir: &Path) -> (GitOid, String) {
        let epoch_oid = GitOid::new(&run_git(root, &["rev-parse", "HEAD"])).expect("oid");
        let candidate = make_candidate_commit(root, "content\n");
        let merge_id = candidate.as_str()[..12].to_owned();
        run_git(root, &["update-ref", "refs/heads/main", epoch_oid.as_str()]);
        run_git(
            root,
            &["update-ref", crate::refs::EPOCH_CURRENT, epoch_oid.as_str()],
        );
        create_quarantine_workspace(
            root,
            manifold_dir,
            &merge_id,
            vec![WorkspaceId::new("ws-1").expect("ws id")],
            &EpochId::new(epoch_oid.as_str()).expect("epoch id"),
            candidate,
            "main",
            dummy_validation_result(false),
        )
        .expect("create quarantine");
        (epoch_oid, merge_id)
    }

    fn passing_config() -> ValidationConfig {
        crate::config::ValidationConfig {
            command: Some("true".to_owned()),
            commands: Vec::new(),
            timeout_seconds: 30,
            preset: None,
            on_failure: crate::config::OnFailure::Block,
        }
    }

    /// bn-3rhz: an agent that fixes a quarantine with `git commit` (clean
    /// worktree, HEAD ahead of the candidate) must have THAT commit promoted.
    /// Pre-fix, `commit_quarantine_edits` saw no worktree edits and returned
    /// the ORIGINAL candidate: validation ran on the fixed worktree, the
    /// unfixed candidate was promoted, and the cleanup then deleted the
    /// worktree holding the only ref to the fix (Prime Invariant).
    #[test]
    fn promote_uses_commits_made_in_quarantine() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".manifold");
        let (_epoch_oid, merge_id) = setup_promotable_quarantine(root, &manifold_dir);
        let ws_path = quarantine_workspace_path(root, &merge_id).unwrap();

        fs::write(ws_path.join("fix.txt"), "the fix\n").unwrap();
        run_git(&ws_path, &["add", "fix.txt"]);
        run_git(
            &ws_path,
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "fix",
            ],
        );
        let fix = run_git(&ws_path, &["rev-parse", "HEAD"]);

        let result =
            promote_quarantine(root, &manifold_dir, &merge_id, &passing_config()).expect("promote");
        let PromoteResult::Committed { new_epoch } = result else {
            panic!("expected Committed");
        };
        assert_eq!(
            new_epoch.as_str(),
            fix,
            "the agent's fix commit must be promoted"
        );
        assert_eq!(run_git(root, &["rev-parse", "refs/heads/main"]), fix);
        assert_eq!(
            run_git(root, &["show", "refs/heads/main:fix.txt"]),
            "the fix"
        );
    }

    /// bn-3rhz: a quarantine HEAD that does not descend from the candidate
    /// (e.g. reset back to the epoch) must be refused, refs untouched.
    #[test]
    fn promote_refuses_head_not_descending_from_candidate() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".manifold");
        let (epoch_oid, merge_id) = setup_promotable_quarantine(root, &manifold_dir);
        let ws_path = quarantine_workspace_path(root, &merge_id).unwrap();
        run_git(
            &ws_path,
            &["checkout", "-q", "--detach", epoch_oid.as_str()],
        );

        let err = promote_quarantine(root, &manifold_dir, &merge_id, &passing_config())
            .expect_err("promote must refuse unrelated HEAD");
        assert!(matches!(err, QuarantineError::Git(_)), "got {err}");
        assert_eq!(
            run_git(root, &["rev-parse", "refs/heads/main"]),
            epoch_oid.as_str()
        );
        assert_eq!(
            run_git(root, &["rev-parse", crate::refs::EPOCH_CURRENT]),
            epoch_oid.as_str()
        );
    }

    /// bn-3w2b: epoch + branch move in ONE atomic CAS. If the branch moved
    /// since the quarantine was created, NEITHER ref may change (the pre-fix
    /// split CAS advanced the epoch, then failed on the branch, leaving the
    /// refs split) and the quarantine survives.
    #[test]
    fn promote_branch_moved_leaves_both_refs_untouched() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".manifold");
        let (epoch_oid, merge_id) = setup_promotable_quarantine(root, &manifold_dir);

        // A direct trunk commit lands on main after the quarantine was made.
        let tree = run_git(
            root,
            &["rev-parse", &format!("{}^{{tree}}", epoch_oid.as_str())],
        );
        let trunk = run_git(
            root,
            &[
                "commit-tree",
                &tree,
                "-p",
                epoch_oid.as_str(),
                "-m",
                "trunk",
            ],
        );
        run_git(root, &["update-ref", "refs/heads/main", &trunk]);

        let err = promote_quarantine(root, &manifold_dir, &merge_id, &passing_config())
            .expect_err("promote must fail: branch moved");
        assert!(matches!(err, QuarantineError::Commit(_)), "got {err}");

        assert_eq!(
            run_git(root, &["rev-parse", crate::refs::EPOCH_CURRENT]),
            epoch_oid.as_str(),
            "epoch must NOT advance when the branch CAS cannot succeed"
        );
        assert_eq!(run_git(root, &["rev-parse", "refs/heads/main"]), trunk);
        assert!(
            QuarantineState::read(&manifold_dir, &merge_id).is_ok(),
            "quarantine must survive a failed promote"
        );
    }

    /// bn-3w2b: promote refuses while a `ws merge` journal is in progress
    /// (a crashed merge awaiting recovery), touching no ref.
    #[test]
    fn promote_refuses_while_merge_state_in_progress() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".manifold");
        let (epoch_oid, merge_id) = setup_promotable_quarantine(root, &manifold_dir);

        let mut ms = crate::merge_state::MergeStateFile::new(
            vec![WorkspaceId::new("other").expect("ws id")],
            EpochId::new(epoch_oid.as_str()).expect("epoch id"),
            1,
        );
        ms.advance(crate::merge_state::MergePhase::Build, 2)
            .expect("build");
        ms.write_atomic(&crate::merge_state::MergeStateFile::default_path(
            &manifold_dir,
        ))
        .expect("write merge-state");

        let err = promote_quarantine(root, &manifold_dir, &merge_id, &passing_config())
            .expect_err("promote must refuse");
        assert!(
            matches!(err, QuarantineError::MergeInProgress { .. }),
            "got {err}"
        );
        assert_eq!(
            run_git(root, &["rev-parse", crate::refs::EPOCH_CURRENT]),
            epoch_oid.as_str()
        );
        assert_eq!(
            run_git(root, &["rev-parse", "refs/heads/main"]),
            epoch_oid.as_str()
        );
    }

    #[test]
    fn promote_with_failing_validation_leaves_quarantine_intact() {
        let (dir, epoch_oid) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".manifold");

        let candidate = make_candidate_commit(root, "content\n");
        let merge_id = &candidate.as_str()[..12];
        let epoch_id = EpochId::new(epoch_oid.as_str()).expect("operation should succeed");

        run_git(root, &["update-ref", "refs/heads/main", epoch_oid.as_str()]);
        run_git(
            root,
            &["update-ref", crate::refs::EPOCH_CURRENT, epoch_oid.as_str()],
        );

        create_quarantine_workspace(
            root,
            &manifold_dir,
            merge_id,
            vec![WorkspaceId::new("ws-1").expect("operation should succeed")],
            &epoch_id,
            candidate.clone(),
            "main",
            dummy_validation_result(false),
        )
        .expect("operation should succeed");

        // Validation config with a command that always fails
        let config = crate::config::ValidationConfig {
            command: Some("false".to_owned()),
            commands: Vec::new(),
            timeout_seconds: 30,
            preset: None,
            on_failure: crate::config::OnFailure::Block,
        };

        let result = promote_quarantine(root, &manifold_dir, merge_id, &config)
            .expect("operation should succeed");

        match &result {
            PromoteResult::ValidationFailed { .. } => {
                // Expected
            }
            PromoteResult::Committed { .. } => {
                panic!("Promote should have failed with 'false' command");
            }
        }

        // Quarantine should still exist
        let ws_path = quarantine_workspace_path(root, merge_id).unwrap();
        assert!(
            ws_path.exists(),
            "quarantine should remain after failed promote"
        );

        let state =
            QuarantineState::read(&manifold_dir, merge_id).expect("operation should succeed");
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
        let epoch_id = EpochId::new(epoch_oid.as_str()).expect("operation should succeed");

        run_git(root, &["update-ref", "refs/heads/main", epoch_oid.as_str()]);
        run_git(
            root,
            &["update-ref", crate::refs::EPOCH_CURRENT, epoch_oid.as_str()],
        );

        create_quarantine_workspace(
            root,
            &manifold_dir,
            merge_id,
            vec![WorkspaceId::new("ws-1").expect("operation should succeed")],
            &epoch_id,
            candidate.clone(),
            "main",
            dummy_validation_result(false),
        )
        .expect("operation should succeed");

        // Simulate the agent fixing the file in the quarantine workspace
        let ws_path = quarantine_workspace_path(root, merge_id).unwrap();
        fs::write(ws_path.join("candidate.txt"), "FIXED\n").expect("operation should succeed");

        // Validate with a command that checks the file content: "true" always passes
        // (The actual file content fix is just simulated; we use a simple passing cmd)
        let config = crate::config::ValidationConfig {
            command: Some("true".to_owned()),
            commands: Vec::new(),
            timeout_seconds: 30,
            preset: None,
            on_failure: crate::config::OnFailure::Block,
        };

        let result = promote_quarantine(root, &manifold_dir, merge_id, &config)
            .expect("operation should succeed");

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

        let err = promote_quarantine(root, &manifold_dir, "nonexistent123", &config)
            .expect_err("operation should fail");
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
        let oid =
            commit_quarantine_edits(root, root, &candidate).expect("operation should succeed");
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
        fs::write(root.join("new_fix.txt"), "fix\n").expect("operation should succeed");

        let oid =
            commit_quarantine_edits(root, root, &candidate).expect("operation should succeed");
        assert_ne!(oid, candidate, "new commit should be created for changes");

        // Verify the new file is in the commit
        let tree = run_git(root, &["show", "--name-only", "--format=", "HEAD"]);
        assert!(tree.contains("new_fix.txt"));
    }

    // -----------------------------------------------------------------------
    // bn-1cth: merge_id validation + layout-aware paths
    // -----------------------------------------------------------------------

    #[test]
    fn abandon_rejects_traversal_id_and_leaves_target_intact() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".manifold");
        fs::create_dir_all(manifold_dir.join("quarantine")).unwrap();
        // <manifold>/quarantine/../../victim == <root>/victim
        let victim = root.join("victim");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("state.json"), "{}").unwrap();

        let err = abandon_quarantine(root, &manifold_dir, "../../victim", None)
            .expect_err("traversal id must be rejected");
        assert!(matches!(err, QuarantineError::InvalidId { .. }), "{err}");
        assert!(victim.join("state.json").exists(), "victim must survive");
    }

    #[test]
    fn entry_points_reject_invalid_ids() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".manifold");
        let config = crate::config::ValidationConfig::default();
        for bad in ["..", "a/b", "", "UPPER", "-a"] {
            assert!(matches!(
                QuarantineState::read(&manifold_dir, bad),
                Err(QuarantineError::InvalidId { .. })
            ));
            assert!(matches!(
                promote_quarantine(root, &manifold_dir, bad, &config),
                Err(QuarantineError::InvalidId { .. })
            ));
            assert!(matches!(
                quarantine_workspace_path(root, bad),
                Err(QuarantineError::InvalidId { .. })
            ));
        }
    }

    #[test]
    fn consolidated_layout_creates_quarantine_under_maw_workspaces() {
        let (dir, epoch) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".maw").join("manifold");
        fs::create_dir_all(&manifold_dir).unwrap();
        let candidate = make_candidate_commit(root, "c\n");
        let merge_id = &candidate.as_str()[..12];

        let path = create_quarantine_workspace(
            root,
            &manifold_dir,
            merge_id,
            vec![WorkspaceId::new("w1").unwrap()],
            &EpochId::new(epoch.as_str()).unwrap(),
            candidate.clone(),
            "main",
            dummy_validation_result(false),
        )
        .expect("create quarantine");

        let expected = root
            .join(".maw")
            .join("workspaces")
            .join(quarantine_workspace_name(merge_id));
        assert_eq!(path, expected);
        assert!(path.join("candidate.txt").exists());
        assert!(!root.join("ws").exists(), "no legacy ws/ in trunk checkout");
        assert_eq!(quarantine_workspace_path(root, merge_id).unwrap(), expected);

        abandon_quarantine(root, &manifold_dir, merge_id, pin(root, merge_id).as_ref())
            .expect("abandon");
        assert!(!expected.exists());
    }

    #[test]
    fn consolidated_layout_abandon_finds_pre_fix_legacy_ws_quarantine() {
        let (dir, epoch) = setup_repo();
        let root = dir.path();
        let manifold_dir = root.join(".maw").join("manifold");
        fs::create_dir_all(&manifold_dir).unwrap();
        let candidate = make_candidate_commit(root, "c\n");
        let merge_id = &candidate.as_str()[..12];
        let name = quarantine_workspace_name(merge_id);

        // Simulate a quarantine created by an older maw at <root>/ws/<name>.
        let legacy = root.join("ws").join(&name);
        run_git(
            root,
            &[
                "worktree",
                "add",
                "--detach",
                legacy.to_str().unwrap(),
                candidate.as_str(),
            ],
        );
        QuarantineState {
            merge_id: merge_id.to_owned(),
            epoch_before: epoch.clone(),
            candidate: candidate.clone(),
            sources: vec![WorkspaceId::new("w1").unwrap()],
            branch: "main".to_owned(),
            validation_result: dummy_validation_result(false),
            created_at: 0,
        }
        .write_atomic(&manifold_dir)
        .unwrap();

        assert_eq!(quarantine_workspace_path(root, merge_id).unwrap(), legacy);
        abandon_quarantine(root, &manifold_dir, merge_id, pin(root, merge_id).as_ref())
            .expect("abandon");
        assert!(
            !legacy.exists(),
            "legacy quarantine worktree must be removed"
        );
        let wt = run_git(root, &["worktree", "list"]);
        assert!(!wt.contains(&name), "worktree must be unregistered:\n{wt}");
    }
}
