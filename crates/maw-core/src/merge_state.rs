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

    /// User-provided commit message for the merge commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_message: Option<String>,

    /// Target branch being updated by this merge.
    ///
    /// Stored to support stale merge-state recovery for non-default target
    /// merges where global epoch may intentionally remain unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_branch: Option<String>,

    /// PID of the process that owns this merge.
    ///
    /// Recorded during PREPARE. Used to detect orphaned merge-state: if the
    /// recorded process is no longer alive (and the boot id matches, so the
    /// pid has not been recycled by a reboot), the merge-state is stale and
    /// can be safely cleared. `None` for merge-state files written by older
    /// maw versions that did not record an owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_pid: Option<u32>,

    /// Hostname of the machine that owns this merge.
    ///
    /// Recorded during PREPARE. A pid is only meaningful on the host that
    /// created it; if the recorded host differs from the current host we
    /// cannot prove the process is dead, so we conservatively keep blocking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_host: Option<String>,

    /// Boot id of the machine that owns this merge (Linux only).
    ///
    /// Recorded during PREPARE from `/proc/sys/kernel/random/boot_id`. Pids
    /// are recycled across reboots, so a recorded pid is only trustworthy if
    /// the boot id still matches. If the boot id changed, the owning process
    /// is provably gone (the machine rebooted) — the merge-state is stale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_boot_id: Option<String>,
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
            commit_message: None,
            target_branch: None,
            owner_pid: None,
            owner_host: None,
            owner_boot_id: None,
        }
    }

    /// Stamp this merge-state with the current process's identity.
    ///
    /// Records the pid, hostname, and (on Linux) the kernel boot id so a
    /// later merge can decide whether this state is owned by a live process
    /// or is an orphan left behind by a killed/OOM'd/panicked merge.
    ///
    /// Call this once during PREPARE, before the state is persisted.
    pub fn stamp_owner(&mut self) {
        self.owner_pid = Some(std::process::id());
        self.owner_host = current_hostname();
        self.owner_boot_id = current_boot_id();
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

    /// Classify the owning process of this merge-state.
    ///
    /// This is the core of orphaned-merge detection. It is deliberately
    /// conservative: it only returns [`Liveness::Dead`] when it can *prove*
    /// the owning process is gone. Anything it cannot prove maps to
    /// [`Liveness::Unknown`], which callers must treat as "keep blocking".
    ///
    /// Decision table:
    /// - No recorded pid (old merge-state) → `Unknown`.
    /// - Recorded host differs from this host → `Unknown` (can't probe a
    ///   pid on another machine).
    /// - Recorded boot id present and differs from this machine's boot id →
    ///   `Dead` (the machine rebooted; the pid cannot be that process).
    /// - pid == our own pid → `Alive` (we are the owner; defensive).
    /// - OS reports the pid as not running → `Dead`.
    /// - OS reports the pid as running → `Alive`.
    /// - Cannot probe (non-Linux, permission, etc.) → `Unknown`.
    #[must_use]
    pub fn owner_liveness(&self) -> Liveness {
        let Some(pid) = self.owner_pid else {
            return Liveness::Unknown;
        };

        // A pid only means something on the host that minted it.
        if let Some(recorded_host) = self.owner_host.as_deref()
            && let Some(this_host) = current_hostname()
            && recorded_host != this_host
        {
            return Liveness::Unknown;
        }

        // A reboot recycles the entire pid space. If we recorded a boot id
        // and it no longer matches, the owning process is provably gone.
        if let Some(recorded_boot) = self.owner_boot_id.as_deref()
            && let Some(this_boot) = current_boot_id()
            && recorded_boot != this_boot
        {
            return Liveness::Dead;
        }

        if pid == std::process::id() {
            return Liveness::Alive;
        }

        match process_is_alive(pid) {
            Some(true) => Liveness::Alive,
            Some(false) => Liveness::Dead,
            None => Liveness::Unknown,
        }
    }

    /// Decide whether this (non-terminal) merge-state is orphaned/stale.
    ///
    /// `now` is the current Unix timestamp in seconds; `stale_after_secs` is
    /// a generous threshold used as a fallback when liveness cannot be
    /// proven (e.g. the merge-state predates owner-pid recording).
    ///
    /// Returns:
    /// - [`Staleness::Live`] — owner process is alive; a real merge is
    ///   running. Keep blocking.
    /// - [`Staleness::Orphaned`] — owner process is provably dead. Safe to
    ///   recover.
    /// - [`Staleness::Indeterminate`] — cannot prove either way. Caller
    ///   keeps blocking but should surface the recovery command.
    #[must_use]
    pub fn staleness(&self, now: u64, stale_after_secs: u64) -> Staleness {
        match self.owner_liveness() {
            Liveness::Alive => Staleness::Live,
            Liveness::Dead => Staleness::Orphaned,
            Liveness::Unknown => {
                // No proof from the pid. Fall back to age: a merge-state
                // that has not been touched for a very long time, with no
                // live owner we could confirm, is almost certainly an
                // orphan. We only auto-recover on age when there is no
                // recorded pid at all (legacy state) — if a pid was
                // recorded but we merely cannot probe it, stay conservative.
                let age = now.saturating_sub(self.updated_at);
                if self.owner_pid.is_none() && age >= stale_after_secs {
                    Staleness::Orphaned
                } else {
                    Staleness::Indeterminate
                }
            }
        }
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

    /// Create the merge-state file exclusively (`O_CREAT` | `O_EXCL`).
    ///
    /// Uses `OpenOptions::create_new(true)` so exactly one writer wins.
    /// Returns `Ok(true)` on success, `Ok(false)` if the file already exists,
    /// and `Err` on any other I/O error.
    ///
    /// The write is crash-safe: data is serialized, written, and fsynced
    /// directly to the target path. Unlike `write_atomic`, there is no
    /// temp+rename dance because the `O_EXCL` flag already guarantees
    /// the file did not exist.
    pub fn write_exclusive(&self, path: &Path) -> Result<bool, MergeStateError> {
        use std::fs::OpenOptions;

        let json = self.to_json()?;

        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut file) => {
                file.write_all(json.as_bytes())
                    .map_err(|e| MergeStateError::Io(format!("write {}: {e}", path.display())))?;
                file.sync_all()
                    .map_err(|e| MergeStateError::Io(format!("fsync {}: {e}", path.display())))?;
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(MergeStateError::Io(format!(
                "create_new {}: {e}",
                path.display()
            ))),
        }
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
// Process-identity / staleness detection (bn-2wyh)
// ---------------------------------------------------------------------------

/// Age (seconds) after which an owner-less merge-state is treated as stale.
///
/// Applies only to legacy merge-state files with *no recorded owner pid*.
/// Generous on purpose: a real merge updates `updated_at` at every phase
/// boundary, so an hour of total silence is far longer than any healthy
/// merge.
pub const DEFAULT_STALE_AFTER_SECS: u64 = 3600;

/// Liveness classification of a merge-state's owning process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    /// The owning process is running.
    Alive,
    /// The owning process is provably gone (dead pid, or machine rebooted).
    Dead,
    /// Cannot prove either way (no pid recorded, foreign host, unprobeable).
    Unknown,
}

/// Result of staleness analysis for a non-terminal merge-state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Staleness {
    /// A live process owns this merge — a real merge is in progress.
    Live,
    /// The owning process is gone — this merge-state is an orphan.
    Orphaned,
    /// Cannot determine; treat conservatively (keep blocking) but surface
    /// the recovery command to the user.
    Indeterminate,
}

/// Read the current machine hostname, if obtainable cheaply.
///
/// Tries `/proc/sys/kernel/hostname` (Linux) then the `HOSTNAME` env var.
/// Returns `None` rather than failing — a missing hostname only widens the
/// "Unknown" (conservative) case.
#[must_use]
pub fn current_hostname() -> Option<String> {
    if let Ok(h) = fs::read_to_string("/proc/sys/kernel/hostname") {
        let h = h.trim();
        if !h.is_empty() {
            return Some(h.to_owned());
        }
    }
    std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty())
}

/// Read the kernel boot id (Linux), if available.
///
/// `/proc/sys/kernel/random/boot_id` changes on every reboot, which lets us
/// detect that a recorded pid belongs to a previous boot (and is therefore
/// dead) even if a new process happens to reuse the same pid number.
#[must_use]
pub fn current_boot_id() -> Option<String> {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Determine whether a process with `pid` is currently alive.
///
/// Linux: checks `/proc/<pid>` existence (no `unsafe`, no extra deps —
/// `unsafe_code` is forbidden workspace-wide so `kill(pid, 0)` is not an
/// option). Returns:
/// - `Some(true)`  — the pid maps to a live process,
/// - `Some(false)` — the pid is definitively not running,
/// - `None`        — cannot tell on this platform (no `/proc`).
#[must_use]
pub fn process_is_alive(pid: u32) -> Option<bool> {
    let proc_root = Path::new("/proc");
    // Only trust /proc-based probing when /proc is actually a procfs mount
    // (it always is on Linux agents, which is maw's target environment).
    if !proc_root.join("self").exists() {
        return None;
    }
    Some(proc_root.join(pid.to_string()).exists())
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

/// Outcome of an explicit `--abort` request against a merge-state file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AbortOutcome {
    /// No merge-state file existed — nothing to abort.
    NothingToAbort,
    /// The merge-state was cleared. `from` is the phase it was in.
    Cleared {
        /// The phase the aborted merge was in.
        from: MergePhase,
    },
    /// Refused: the merge already passed COMMIT (epoch advanced past
    /// `epoch_before`), so clearing could mask partially-committed work.
    /// The caller must inspect refs / run recovery instead of blind abort.
    RefusedPostCommit {
        /// The phase the merge was in.
        phase: MergePhase,
        /// Human-readable reason.
        reason: String,
    },
}

/// Explicitly abort an orphaned/in-progress merge by clearing its
/// merge-state file — but *only* when doing so cannot lose committed work.
///
/// This upholds the Prime Invariant. It is safe to clear merge-state iff the
/// merge never reached COMMIT, i.e. the epoch ref still equals the
/// `epoch_before` recorded when the merge started (and, for non-default
/// targets, the target branch has not moved to the candidate). The caller
/// supplies the *currently observed* epoch (and optionally the target
/// branch head) so this function stays free of git/IO dependencies.
///
/// Arguments:
/// - `merge_state_path`: path to `.manifold/merge-state.json`.
/// - `current_epoch`: the current `refs/manifold/epoch/current` OID hex, if
///   any (None if the epoch ref is missing).
/// - `current_target_head`: the current head of the recorded target branch,
///   if the merge-state recorded a `target_branch` (None otherwise).
///
/// # Errors
/// Returns [`MergeStateError`] on I/O or deserialization failure.
pub fn abort_merge_state(
    merge_state_path: &Path,
    current_epoch: Option<&str>,
    current_target_head: Option<&str>,
) -> Result<AbortOutcome, MergeStateError> {
    let state = match MergeStateFile::read(merge_state_path) {
        Ok(s) => s,
        Err(MergeStateError::NotFound(_)) => return Ok(AbortOutcome::NothingToAbort),
        Err(e) => return Err(e),
    };

    // Terminal states carry no in-progress lock; just remove the file.
    if state.phase.is_terminal() {
        remove_merge_state_if_exists(merge_state_path)?;
        return Ok(AbortOutcome::Cleared { from: state.phase });
    }

    // Prime-Invariant gate: the kill must be provably pre-COMMIT.
    //
    // Pre-COMMIT phases (Prepare/Build/Validate) never touched a ref, so
    // clearing is always safe. For Commit/Cleanup we must verify that the
    // refs did NOT advance to the candidate; if they did, real work was
    // committed and a blind abort could orphan it — refuse and point the
    // user at recovery.
    let post_commit = matches!(state.phase, MergePhase::Commit | MergePhase::Cleanup);
    if post_commit && let Some(candidate) = state.epoch_candidate.as_ref().map(GitOid::as_str) {
        let epoch_at_candidate = current_epoch == Some(candidate);
        let branch_at_candidate = current_target_head == Some(candidate);
        if epoch_at_candidate || branch_at_candidate {
            return Ok(AbortOutcome::RefusedPostCommit {
                phase: state.phase.clone(),
                reason: format!(
                    "merge reached {} and the {} already advanced to the merged commit; \
                     clearing now could orphan committed work",
                    state.phase,
                    if epoch_at_candidate {
                        "epoch"
                    } else {
                        "target branch"
                    }
                ),
            });
        }
    }

    // Epoch-drift gate: even for pre-COMMIT phases, if the epoch has moved
    // away from epoch_before since this merge started, *something* advanced
    // the epoch (another merge, a recovery). Refuse rather than risk
    // clobbering that state — the user can inspect and retry.
    if let Some(observed) = current_epoch
        && observed != state.epoch_before.as_str()
    {
        return Ok(AbortOutcome::RefusedPostCommit {
            phase: state.phase.clone(),
            reason: format!(
                "epoch advanced since this merge started (was {}, now {}); \
                 refusing to clear merge-state to avoid clobbering newer state",
                &state.epoch_before.as_str()[..state.epoch_before.as_str().len().min(12)],
                &observed[..observed.len().min(12)]
            ),
        });
    }

    remove_merge_state_if_exists(merge_state_path)?;
    Ok(AbortOutcome::Cleared { from: state.phase })
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
        EpochId::new(&"a".repeat(40)).expect("operation should succeed")
    }

    fn test_oid() -> GitOid {
        GitOid::new(&"b".repeat(40)).expect("operation should succeed")
    }

    fn test_sources() -> Vec<WorkspaceId> {
        vec![
            WorkspaceId::new("agent-1").expect("operation should succeed"),
            WorkspaceId::new("agent-2").expect("operation should succeed"),
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
            let json = serde_json::to_string(&phase).expect("operation should succeed");
            let decoded: MergePhase =
                serde_json::from_str(&json).expect("operation should succeed");
            assert_eq!(decoded, phase, "roundtrip failed for {phase}");
        }
    }

    #[test]
    fn phase_serde_snake_case() {
        let json = serde_json::to_string(&MergePhase::Prepare).expect("operation should succeed");
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

        state
            .advance(MergePhase::Build, 1001)
            .expect("operation should succeed");
        assert_eq!(state.phase, MergePhase::Build);
        assert_eq!(state.updated_at, 1001);

        state
            .advance(MergePhase::Validate, 1002)
            .expect("operation should succeed");
        assert_eq!(state.phase, MergePhase::Validate);

        state
            .advance(MergePhase::Commit, 1003)
            .expect("operation should succeed");
        assert_eq!(state.phase, MergePhase::Commit);

        state
            .advance(MergePhase::Cleanup, 1004)
            .expect("operation should succeed");
        assert_eq!(state.phase, MergePhase::Cleanup);

        state
            .advance(MergePhase::Complete, 1005)
            .expect("operation should succeed");
        assert_eq!(state.phase, MergePhase::Complete);
        assert_eq!(state.updated_at, 1005);
    }

    #[test]
    fn advance_invalid_transition() {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        let err = state
            .advance(MergePhase::Validate, 1001)
            .expect_err("operation should fail");
        assert!(matches!(err, MergeStateError::InvalidTransition { .. }));
        // Phase should not change on error
        assert_eq!(state.phase, MergePhase::Prepare);
    }

    #[test]
    fn advance_from_terminal_fails() {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state
            .advance(MergePhase::Build, 1001)
            .expect("operation should succeed");
        state
            .advance(MergePhase::Validate, 1002)
            .expect("operation should succeed");
        state
            .advance(MergePhase::Commit, 1003)
            .expect("operation should succeed");
        state
            .advance(MergePhase::Cleanup, 1004)
            .expect("operation should succeed");
        state
            .advance(MergePhase::Complete, 1005)
            .expect("operation should succeed");

        let err = state
            .advance(MergePhase::Aborted, 1006)
            .expect_err("operation should fail");
        assert!(matches!(err, MergeStateError::InvalidTransition { .. }));
    }

    #[test]
    fn abort_from_prepare() {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state
            .abort("test abort", 1001)
            .expect("operation should succeed");
        assert_eq!(state.phase, MergePhase::Aborted);
        assert_eq!(state.abort_reason.as_deref(), Some("test abort"));
        assert_eq!(state.updated_at, 1001);
    }

    #[test]
    fn abort_from_build() {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state
            .advance(MergePhase::Build, 1001)
            .expect("operation should succeed");
        state
            .abort("build failed", 1002)
            .expect("operation should succeed");
        assert_eq!(state.phase, MergePhase::Aborted);
        assert_eq!(state.abort_reason.as_deref(), Some("build failed"));
    }

    #[test]
    fn abort_from_terminal_fails() {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state
            .abort("first abort", 1001)
            .expect("operation should succeed");
        let err = state
            .abort("double abort", 1002)
            .expect_err("operation should fail");
        assert!(matches!(err, MergeStateError::InvalidTransition { .. }));
    }

    // -- JSON serialization --

    #[test]
    fn json_roundtrip_prepare() {
        let state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        let json = state.to_json().expect("operation should succeed");
        let decoded = MergeStateFile::from_json(&json).expect("operation should succeed");
        assert_eq!(decoded, state);
    }

    #[test]
    fn json_roundtrip_with_optional_fields() {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state
            .advance(MergePhase::Build, 1001)
            .expect("operation should succeed");
        state.epoch_candidate = Some(test_oid());
        state
            .advance(MergePhase::Validate, 1002)
            .expect("operation should succeed");
        state.validation_result = Some(ValidationResult {
            passed: true,
            exit_code: Some(0),
            stdout: "ok".to_owned(),
            stderr: String::new(),
            duration_ms: 1500,
            command_results: Vec::new(),
        });
        state
            .advance(MergePhase::Commit, 1003)
            .expect("operation should succeed");
        state.epoch_after = Some(EpochId::new(&"c".repeat(40)).expect("operation should succeed"));

        let json = state.to_json().expect("operation should succeed");
        let decoded = MergeStateFile::from_json(&json).expect("operation should succeed");
        assert_eq!(decoded, state);
    }

    #[test]
    fn json_is_pretty_printed() {
        let state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        let json = state.to_json().expect("operation should succeed");
        // Pretty-printed JSON has newlines
        assert!(json.contains('\n'));
        // Contains indentation
        assert!(json.contains("  "));
    }

    #[test]
    fn json_omits_none_fields() {
        let state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        let json = state.to_json().expect("operation should succeed");
        assert!(!json.contains("epoch_candidate"));
        assert!(!json.contains("validation_result"));
        assert!(!json.contains("epoch_after"));
        assert!(!json.contains("abort_reason"));
    }

    #[test]
    fn json_includes_some_fields() {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state
            .advance(MergePhase::Build, 1001)
            .expect("operation should succeed");
        state.epoch_candidate = Some(test_oid());
        let json = state.to_json().expect("operation should succeed");
        assert!(json.contains("epoch_candidate"));
        assert!(json.contains(&"b".repeat(40)));
    }

    #[test]
    fn json_deserialize_invalid() {
        let err = MergeStateFile::from_json("not json").expect_err("operation should fail");
        assert!(matches!(err, MergeStateError::Deserialize(_)));
    }

    // -- Atomic file I/O --

    #[test]
    fn write_and_read_roundtrip() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let path = MergeStateFile::default_path(dir.path());

        let state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.write_atomic(&path).expect("operation should succeed");

        let loaded = MergeStateFile::read(&path).expect("operation should succeed");
        assert_eq!(loaded, state);
    }

    #[test]
    fn write_overwrite_preserves_atomicity() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let path = MergeStateFile::default_path(dir.path());

        // Write initial state
        let state1 = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state1
            .write_atomic(&path)
            .expect("operation should succeed");

        // Overwrite with advanced state
        let mut state2 = MergeStateFile::new(test_sources(), test_epoch(), 2000);
        state2
            .advance(MergePhase::Build, 2001)
            .expect("operation should succeed");
        state2.epoch_candidate = Some(test_oid());
        state2
            .write_atomic(&path)
            .expect("operation should succeed");

        // Read should return state2
        let loaded = MergeStateFile::read(&path).expect("operation should succeed");
        assert_eq!(loaded, state2);
    }

    #[test]
    fn read_not_found() {
        let path = PathBuf::from("/tmp/nonexistent-merge-state-test.json");
        let err = MergeStateFile::read(&path).expect_err("operation should fail");
        assert!(matches!(err, MergeStateError::NotFound(_)));
    }

    #[test]
    fn read_corrupt_file() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let path = dir.path().join("merge-state.json");
        fs::write(&path, "corrupted data").expect("operation should succeed");
        let err = MergeStateFile::read(&path).expect_err("operation should fail");
        assert!(matches!(err, MergeStateError::Deserialize(_)));
    }

    #[test]
    fn default_path() {
        let path = MergeStateFile::default_path(Path::new("/repo/.manifold"));
        assert_eq!(path, PathBuf::from("/repo/.manifold/merge-state.json"));
    }

    #[test]
    fn tmp_file_cleaned_up_after_write() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let path = MergeStateFile::default_path(dir.path());

        let state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.write_atomic(&path).expect("operation should succeed");

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
        let json = serde_json::to_string_pretty(&result).expect("operation should succeed");
        let decoded: ValidationResult =
            serde_json::from_str(&json).expect("operation should succeed");
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
        let json = serde_json::to_string_pretty(&result).expect("operation should succeed");
        let decoded: ValidationResult =
            serde_json::from_str(&json).expect("operation should succeed");
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
        let json = serde_json::to_string_pretty(&result).expect("operation should succeed");
        assert!(json.contains("command_results"));
        let decoded: ValidationResult =
            serde_json::from_str(&json).expect("operation should succeed");
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
        let decoded: ValidationResult =
            serde_json::from_str(json).expect("operation should succeed");
        assert!(decoded.passed);
        assert!(decoded.command_results.is_empty());
    }

    // -- Cleanup + recovery helpers --

    #[test]
    fn cleanup_phase_destroys_sources_and_removes_merge_state() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let path = MergeStateFile::default_path(dir.path());

        let state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.write_atomic(&path).expect("operation should succeed");
        assert!(path.exists());

        let mut destroyed = Vec::new();
        run_cleanup_phase(&state, &path, true, |ws| {
            destroyed.push(ws.as_str().to_owned());
            Ok(())
        })
        .expect("operation should succeed");

        assert_eq!(destroyed, vec!["agent-1".to_owned(), "agent-2".to_owned()]);
        assert!(!path.exists());
    }

    #[test]
    fn cleanup_phase_is_idempotent() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let path = MergeStateFile::default_path(dir.path());

        let state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.write_atomic(&path).expect("operation should succeed");

        run_cleanup_phase(&state, &path, false, |_ws| Ok(())).expect("operation should succeed");
        run_cleanup_phase(&state, &path, false, |_ws| Ok(())).expect("operation should succeed");

        assert!(!path.exists());
    }

    fn state_in_phase(phase: MergePhase) -> MergeStateFile {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        match phase {
            MergePhase::Prepare => {}
            MergePhase::Build => {
                state
                    .advance(MergePhase::Build, 1001)
                    .expect("operation should succeed");
            }
            MergePhase::Validate => {
                state
                    .advance(MergePhase::Build, 1001)
                    .expect("operation should succeed");
                state
                    .advance(MergePhase::Validate, 1002)
                    .expect("operation should succeed");
            }
            MergePhase::Commit => {
                state
                    .advance(MergePhase::Build, 1001)
                    .expect("operation should succeed");
                state
                    .advance(MergePhase::Validate, 1002)
                    .expect("operation should succeed");
                state
                    .advance(MergePhase::Commit, 1003)
                    .expect("operation should succeed");
            }
            MergePhase::Cleanup => {
                state
                    .advance(MergePhase::Build, 1001)
                    .expect("operation should succeed");
                state
                    .advance(MergePhase::Validate, 1002)
                    .expect("operation should succeed");
                state
                    .advance(MergePhase::Commit, 1003)
                    .expect("operation should succeed");
                state
                    .advance(MergePhase::Cleanup, 1004)
                    .expect("operation should succeed");
            }
            MergePhase::Complete => {
                state
                    .advance(MergePhase::Build, 1001)
                    .expect("operation should succeed");
                state
                    .advance(MergePhase::Validate, 1002)
                    .expect("operation should succeed");
                state
                    .advance(MergePhase::Commit, 1003)
                    .expect("operation should succeed");
                state
                    .advance(MergePhase::Cleanup, 1004)
                    .expect("operation should succeed");
                state
                    .advance(MergePhase::Complete, 1005)
                    .expect("operation should succeed");
            }
            MergePhase::Aborted => {
                state
                    .abort("aborted for test", 1001)
                    .expect("operation should succeed");
            }
        }
        state
    }

    #[test]
    fn recovery_no_merge_state_returns_no_merge_in_progress() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let path = MergeStateFile::default_path(dir.path());

        let outcome = recover_from_merge_state(&path).expect("operation should succeed");
        assert_eq!(outcome, RecoveryOutcome::NoMergeInProgress);
    }

    #[test]
    fn recovery_prepare_aborts_and_deletes_state_file() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let path = MergeStateFile::default_path(dir.path());

        let state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.write_atomic(&path).expect("operation should succeed");

        let outcome = recover_from_merge_state(&path).expect("operation should succeed");
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
        let dir = tempfile::tempdir().expect("operation should succeed");
        let path = MergeStateFile::default_path(dir.path());

        let state = state_in_phase(MergePhase::Build);
        state.write_atomic(&path).expect("operation should succeed");

        let outcome = recover_from_merge_state(&path).expect("operation should succeed");
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
        let dir = tempfile::tempdir().expect("operation should succeed");
        let path = MergeStateFile::default_path(dir.path());

        let state = state_in_phase(MergePhase::Commit);
        state.write_atomic(&path).expect("operation should succeed");

        let outcome = recover_from_merge_state(&path).expect("operation should succeed");
        assert_eq!(outcome, RecoveryOutcome::CheckCommit);
        assert!(path.exists());
    }

    /// bn-38vw: `epoch_after` is now journaled BEFORE the ref-advancing CAS,
    /// so a crash anywhere in the COMMIT phase (refs old OR refs advanced)
    /// leaves a COHERENT merge-state: phase past the point-of-no-return AND
    /// `epoch_after` recorded. This is the shape Oracle B requires — the old
    /// "phase=commit but epoch_after=None" window can no longer exist.
    ///
    /// Both crash points reload to the same journal shape: the only thing
    /// that differs across them is the live ref value (checked by the
    /// commit-recovery path), not the journal. Recovery dispatch for COMMIT
    /// stays `CheckCommit` (inspect the live refs and converge forward
    /// idempotently to `epoch_after`), and the state file is preserved.
    #[test]
    fn bn_38vw_commit_phase_records_epoch_after_before_cas() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let path = MergeStateFile::default_path(dir.path());

        // Reproduce the post-fix write order: advance to Commit, then record
        // epoch_after, THEN (conceptually) perform the ref CAS. We persist at
        // the pre-CAS crash point: phase=Commit, epoch_after already set.
        let epoch_after = EpochId::new(&"c".repeat(40)).expect("operation should succeed");
        let mut state = state_in_phase(MergePhase::Commit);
        state.epoch_candidate = Some(test_oid());
        state.epoch_after = Some(epoch_after.clone());
        state.write_atomic(&path).expect("operation should succeed");

        // Crash AFTER epoch_after journaled, BEFORE the CAS (refs still old).
        // The reloaded journal is already coherent: past-PONR with epoch_after.
        let pre_cas = MergeStateFile::read(&path).expect("operation should succeed");
        assert_eq!(pre_cas.phase, MergePhase::Commit);
        assert_eq!(
            pre_cas.epoch_after.as_ref(),
            Some(&epoch_after),
            "epoch_after must be recorded before the CAS so no past-PONR \
             state is ever missing it (Oracle B coherence)"
        );
        // Recovery for COMMIT inspects the live refs and converges forward.
        let outcome = recover_from_merge_state(&path).expect("operation should succeed");
        assert_eq!(outcome, RecoveryOutcome::CheckCommit);
        assert!(path.exists(), "merge-state preserved for ref inspection");

        // Crash AFTER the CAS (refs advanced): the journal is unchanged —
        // still phase=Commit with epoch_after set — so recovery behaves
        // identically and converges to "already committed" against the refs.
        let post_cas = MergeStateFile::read(&path).expect("operation should succeed");
        assert_eq!(post_cas.phase, MergePhase::Commit);
        assert_eq!(post_cas.epoch_after.as_ref(), Some(&epoch_after));
        let outcome = recover_from_merge_state(&path).expect("operation should succeed");
        assert_eq!(outcome, RecoveryOutcome::CheckCommit);
        assert!(path.exists());
    }

    #[test]
    fn recovery_validate_requests_rerun_and_keeps_state_file() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let path = MergeStateFile::default_path(dir.path());

        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state
            .advance(MergePhase::Build, 1001)
            .expect("operation should succeed");
        state
            .advance(MergePhase::Validate, 1002)
            .expect("operation should succeed");
        state.write_atomic(&path).expect("operation should succeed");

        let outcome = recover_from_merge_state(&path).expect("operation should succeed");
        assert_eq!(outcome, RecoveryOutcome::RetryValidate);
        assert!(path.exists());
    }

    #[test]
    fn recovery_cleanup_requests_rerun_and_deletes_state_file() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let path = MergeStateFile::default_path(dir.path());

        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state
            .advance(MergePhase::Build, 1001)
            .expect("operation should succeed");
        state
            .advance(MergePhase::Validate, 1002)
            .expect("operation should succeed");
        state
            .advance(MergePhase::Commit, 1003)
            .expect("operation should succeed");
        state
            .advance(MergePhase::Cleanup, 1004)
            .expect("operation should succeed");
        state.write_atomic(&path).expect("operation should succeed");

        let outcome = recover_from_merge_state(&path).expect("operation should succeed");
        assert_eq!(outcome, RecoveryOutcome::RetryCleanup);
        assert!(!path.exists());
    }

    #[test]
    fn recovery_precommit_abort_preserves_workspace_files() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let workspace_file = dir.path().join("ws").join("agent-1").join("keep.txt");
        fs::create_dir_all(workspace_file.parent().expect("operation should succeed"))
            .expect("operation should succeed");
        fs::write(&workspace_file, "important work\n").expect("operation should succeed");

        let path = MergeStateFile::default_path(dir.path());
        let state = state_in_phase(MergePhase::Build);
        state.write_atomic(&path).expect("operation should succeed");

        let outcome = recover_from_merge_state(&path).expect("operation should succeed");
        assert!(matches!(
            outcome,
            RecoveryOutcome::AbortedPreCommit {
                from: MergePhase::Build
            }
        ));
        assert_eq!(
            fs::read_to_string(&workspace_file).expect("operation should succeed"),
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
                let dir = tempfile::tempdir().expect("operation should succeed");
                let path = MergeStateFile::default_path(dir.path());
                let state = state_in_phase(phase.clone());
                state.write_atomic(&path).expect("operation should succeed");

                let first = recover_from_merge_state(&path).expect("operation should succeed");
                let second = recover_from_merge_state(&path).expect("operation should succeed");

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
        let dir = tempfile::tempdir().expect("operation should succeed");
        let path = MergeStateFile::default_path(dir.path());

        // Prepare
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.write_atomic(&path).expect("operation should succeed");

        // Build
        state
            .advance(MergePhase::Build, 1001)
            .expect("operation should succeed");
        state.epoch_candidate = Some(test_oid());
        state.write_atomic(&path).expect("operation should succeed");
        let loaded = MergeStateFile::read(&path).expect("operation should succeed");
        assert_eq!(loaded.phase, MergePhase::Build);
        assert!(loaded.epoch_candidate.is_some());

        // Validate
        state
            .advance(MergePhase::Validate, 1002)
            .expect("operation should succeed");
        state.validation_result = Some(ValidationResult {
            passed: true,
            exit_code: Some(0),
            stdout: "all tests passed".to_owned(),
            stderr: String::new(),
            duration_ms: 850,
            command_results: Vec::new(),
        });
        state.write_atomic(&path).expect("operation should succeed");

        // Commit
        state
            .advance(MergePhase::Commit, 1003)
            .expect("operation should succeed");
        state.epoch_after = Some(EpochId::new(&"c".repeat(40)).expect("operation should succeed"));
        state.write_atomic(&path).expect("operation should succeed");

        // Cleanup
        state
            .advance(MergePhase::Cleanup, 1004)
            .expect("operation should succeed");
        state.write_atomic(&path).expect("operation should succeed");

        // Complete
        state
            .advance(MergePhase::Complete, 1005)
            .expect("operation should succeed");
        state.write_atomic(&path).expect("operation should succeed");

        // Final read
        let final_state = MergeStateFile::read(&path).expect("operation should succeed");
        assert_eq!(final_state.phase, MergePhase::Complete);
        assert!(final_state.epoch_candidate.is_some());
        assert!(final_state.validation_result.is_some());
        assert!(final_state.epoch_after.is_some());
        assert_eq!(final_state.started_at, 1000);
        assert_eq!(final_state.updated_at, 1005);
    }

    // -- bn-2wyh: stale / orphaned merge-state detection + abort --

    /// A pid that is essentially certain not to be running. We pick a very
    /// large value above the typical `pid_max`; on Linux `/proc/<pid>` will
    /// not exist for it.
    const DEAD_PID: u32 = 4_000_000_000;

    #[test]
    fn stamp_owner_records_pid() {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        assert!(state.owner_pid.is_none());
        state.stamp_owner();
        assert_eq!(state.owner_pid, Some(std::process::id()));
    }

    #[test]
    fn own_pid_is_alive() {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.stamp_owner();
        // We are obviously running, so our own pid must read as Alive and
        // the merge-state must be classified Live (keep blocking).
        assert_eq!(state.owner_liveness(), Liveness::Alive);
        assert_eq!(
            state.staleness(2000, DEFAULT_STALE_AFTER_SECS),
            Staleness::Live
        );
    }

    #[test]
    fn dead_pid_is_orphaned() {
        // Only meaningful where we can actually probe pids (Linux /proc).
        if process_is_alive(std::process::id()) != Some(true) {
            return;
        }
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.stamp_owner();
        state.owner_pid = Some(DEAD_PID);
        assert_eq!(state.owner_liveness(), Liveness::Dead);
        assert_eq!(
            state.staleness(2000, DEFAULT_STALE_AFTER_SECS),
            Staleness::Orphaned
        );
    }

    #[test]
    fn rebooted_machine_marks_pid_dead() {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.stamp_owner();
        // Force-set a recorded pid; if our own pid happens to still be
        // alive, the boot-id mismatch must still win and report Dead.
        state.owner_pid = Some(std::process::id());
        state.owner_boot_id = Some("00000000-0000-0000-0000-000000000000".to_owned());
        if current_boot_id().is_some() {
            assert_eq!(state.owner_liveness(), Liveness::Dead);
        }
    }

    #[test]
    fn no_owner_pid_recent_is_indeterminate_blocks() {
        // Legacy merge-state (no pid). Recent updated_at → cannot prove
        // stale → Indeterminate (caller keeps blocking).
        let state = MergeStateFile::new(test_sources(), test_epoch(), 5000);
        assert_eq!(state.owner_liveness(), Liveness::Unknown);
        assert_eq!(
            state.staleness(5100, DEFAULT_STALE_AFTER_SECS),
            Staleness::Indeterminate
        );
    }

    #[test]
    fn no_owner_pid_ancient_is_orphaned() {
        // Legacy merge-state untouched for far longer than the threshold →
        // treated as orphaned so it can be auto-recovered.
        let state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        let now = 1000 + DEFAULT_STALE_AFTER_SECS + 1;
        assert_eq!(
            state.staleness(now, DEFAULT_STALE_AFTER_SECS),
            Staleness::Orphaned
        );
    }

    #[test]
    fn foreign_host_is_indeterminate() {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.stamp_owner();
        state.owner_host = Some("definitely-not-this-host-xyzzy".to_owned());
        // Can't probe a pid on another machine → Unknown → still blocks.
        assert_eq!(state.owner_liveness(), Liveness::Unknown);
    }

    #[test]
    fn abort_clears_precommit_and_preserves_epoch() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let path = MergeStateFile::default_path(dir.path());

        // Build-phase orphan (pre-COMMIT), epoch unchanged.
        let state = state_in_phase(MergePhase::Build);
        let epoch_before = state.epoch_before.as_str().to_owned();
        state.write_atomic(&path).expect("operation should succeed");

        let outcome =
            abort_merge_state(&path, Some(&epoch_before), None).expect("operation should succeed");
        assert_eq!(
            outcome,
            AbortOutcome::Cleared {
                from: MergePhase::Build
            }
        );
        assert!(!path.exists(), "merge-state must be removed");
        // Epoch is supplied by caller; abort never touches refs — the
        // Prime Invariant is upheld because we only cleared a pre-COMMIT
        // state and the epoch we observed equals epoch_before.
    }

    #[test]
    fn abort_nothing_when_no_state() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let path = MergeStateFile::default_path(dir.path());
        let outcome = abort_merge_state(&path, None, None).expect("operation should succeed");
        assert_eq!(outcome, AbortOutcome::NothingToAbort);
    }

    #[test]
    fn abort_refuses_when_epoch_advanced_past_epoch_before() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let path = MergeStateFile::default_path(dir.path());

        let state = state_in_phase(MergePhase::Build);
        state.write_atomic(&path).expect("operation should succeed");

        // Observed epoch differs from epoch_before → something advanced the
        // epoch since this merge started. Refuse to clobber it.
        let advanced_epoch = "f".repeat(40);
        let outcome = abort_merge_state(&path, Some(&advanced_epoch), None)
            .expect("operation should succeed");
        assert!(
            matches!(outcome, AbortOutcome::RefusedPostCommit { .. }),
            "expected refusal, got {outcome:?}"
        );
        assert!(path.exists(), "merge-state must be preserved on refusal");
    }

    #[test]
    fn abort_refuses_post_commit_when_epoch_at_candidate() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let path = MergeStateFile::default_path(dir.path());

        // Commit-phase with a candidate; epoch already advanced TO the
        // candidate → the merge committed → refuse (Prime Invariant).
        let mut state = state_in_phase(MergePhase::Commit);
        let candidate = test_oid();
        state.epoch_candidate = Some(candidate.clone());
        state.write_atomic(&path).expect("operation should succeed");

        let outcome = abort_merge_state(&path, Some(candidate.as_str()), None)
            .expect("operation should succeed");
        assert!(
            matches!(outcome, AbortOutcome::RefusedPostCommit { .. }),
            "expected refusal, got {outcome:?}"
        );
        assert!(path.exists());
    }

    #[test]
    fn abort_clears_terminal_state() {
        let dir = tempfile::tempdir().expect("operation should succeed");
        let path = MergeStateFile::default_path(dir.path());

        let state = state_in_phase(MergePhase::Aborted);
        state.write_atomic(&path).expect("operation should succeed");

        let outcome = abort_merge_state(&path, None, None).expect("operation should succeed");
        assert_eq!(
            outcome,
            AbortOutcome::Cleared {
                from: MergePhase::Aborted
            }
        );
        assert!(!path.exists());
    }

    #[test]
    fn process_is_alive_self_true() {
        // Linux agents (maw's target) always have /proc.
        if Path::new("/proc/self").exists() {
            assert_eq!(process_is_alive(std::process::id()), Some(true));
            assert_eq!(process_is_alive(DEAD_PID), Some(false));
        }
    }

    #[test]
    fn merge_state_owner_fields_roundtrip_json() {
        let mut state = MergeStateFile::new(test_sources(), test_epoch(), 1000);
        state.stamp_owner();
        let json = state.to_json().expect("operation should succeed");
        let decoded = MergeStateFile::from_json(&json).expect("operation should succeed");
        assert_eq!(decoded, state);
        assert_eq!(decoded.owner_pid, Some(std::process::id()));
    }

    #[test]
    fn old_merge_state_without_owner_fields_deserializes() {
        // Backward-compat: a pre-bn-2wyh merge-state has no owner_* keys.
        let json = r#"{
            "phase": "build",
            "sources": ["agent-1"],
            "epoch_before": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "started_at": 1000,
            "updated_at": 1000
        }"#;
        let decoded = MergeStateFile::from_json(json).expect("operation should succeed");
        assert!(decoded.owner_pid.is_none());
        assert!(decoded.owner_host.is_none());
        assert_eq!(decoded.owner_liveness(), Liveness::Unknown);
    }
}
