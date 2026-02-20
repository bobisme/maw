//! BUILD phase of the epoch advancement state machine.
//!
//! Orchestrates the full collect → partition → resolve → build pipeline,
//! operating on frozen inputs from PREPARE. Produces a candidate git commit
//! and records its OID in the merge-state file.
//!
//! # Crash safety
//!
//! - The merge-state file is advanced from `Prepare` to `Build` **before**
//!   any work begins. If a crash occurs during BUILD, recovery sees the
//!   `Build` phase and aborts — safe because no refs were moved.
//! - Git objects written during BUILD (blobs, trees, the candidate commit)
//!   are harmless orphans until COMMIT moves the refs. `git gc` eventually
//!   collects them.
//! - The candidate commit OID is persisted to merge-state with fsync before
//!   returning, so downstream phases can always find it.
//!
//! # Pipeline
//!
//! 1. **Collect** — snapshot each source workspace via the backend.
//! 2. **Partition** — group changed paths into unique (single workspace) vs
//!    shared (multiple workspaces).
//! 3. **Resolve** — auto-merge shared paths via hash equality / diff3.
//! 4. **Drivers** — apply deterministic merge drivers (`regenerate`, `ours`,
//!    `theirs`) for configured path globs.
//! 5. **Build** — apply resolved changes to the epoch tree, produce a new
//!    git tree + commit.

#![allow(clippy::missing_errors_doc)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use glob::Pattern;

use crate::backend::WorkspaceBackend;
use crate::config::{ConfigError, ManifoldConfig, MergeConfig, MergeDriver, MergeDriverKind};
use crate::merge::build::{build_merge_commit, BuildError, ResolvedChange};
use crate::merge::collect::{collect_snapshots, CollectError};
use crate::merge::partition::{partition_by_path, PartitionResult, PathEntry};
#[cfg(not(feature = "ast-merge"))]
use crate::merge::resolve::resolve_partition;
#[cfg(feature = "ast-merge")]
use crate::merge::resolve::resolve_partition_with_ast;
use crate::merge::resolve::{ConflictRecord, ResolveError, ResolveResult};
use crate::merge_state::{MergePhase, MergeStateError, MergeStateFile};
use crate::model::types::{EpochId, GitOid, WorkspaceId};

// ---------------------------------------------------------------------------
// BuildPhaseOutput
// ---------------------------------------------------------------------------

/// Output of a successful BUILD phase.
#[derive(Clone, Debug)]
pub struct BuildPhaseOutput {
    /// The candidate commit OID produced by the merge engine.
    pub candidate: GitOid,
    /// Conflict records for paths that could not be auto-resolved.
    /// Empty if the merge was fully clean.
    pub conflicts: Vec<ConflictRecord>,
    /// Number of changes that were resolved and applied to the tree.
    pub resolved_count: usize,
    /// Number of unique paths (touched by only one workspace).
    pub unique_count: usize,
    /// Number of shared paths (touched by multiple workspaces).
    pub shared_count: usize,
}

// ---------------------------------------------------------------------------
// BuildPhaseError
// ---------------------------------------------------------------------------

/// Errors that can occur during the BUILD phase.
#[derive(Debug)]
pub enum BuildPhaseError {
    /// The merge-state file is not in the expected phase.
    WrongPhase {
        expected: MergePhase,
        actual: MergePhase,
    },
    /// Merge-state I/O or serialization error.
    State(MergeStateError),
    /// Repository config load/parse failure.
    Config(ConfigError),
    /// Workspace snapshot collection failed.
    Collect(CollectError),
    /// Path resolution (hash equality / diff3) failed.
    Resolve(ResolveError),
    /// Git tree/commit construction failed.
    Build(BuildError),
    /// Failed to read base file content from the epoch tree.
    ReadBase { path: PathBuf, detail: String },
    /// Merge driver error (invalid config, unsupported shape, or failed command).
    Driver(String),
}

impl fmt::Display for BuildPhaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongPhase { expected, actual } => {
                write!(
                    f,
                    "BUILD: merge-state in wrong phase (expected {expected}, got {actual})"
                )
            }
            Self::State(e) => write!(f, "BUILD: merge-state error: {e}"),
            Self::Config(e) => write!(f, "BUILD: config error: {e}"),
            Self::Collect(e) => write!(f, "BUILD: collect failed: {e}"),
            Self::Resolve(e) => write!(f, "BUILD: resolve failed: {e}"),
            Self::Build(e) => write!(f, "BUILD: build failed: {e}"),
            Self::ReadBase { path, detail } => {
                write!(
                    f,
                    "BUILD: failed to read base content for {}: {detail}",
                    path.display()
                )
            }
            Self::Driver(detail) => write!(f, "BUILD: merge driver failed: {detail}"),
        }
    }
}

impl std::error::Error for BuildPhaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(e) => Some(e),
            Self::Collect(e) => Some(e),
            Self::Resolve(e) => Some(e),
            Self::Build(e) => Some(e),
            _ => None,
        }
    }
}

impl From<MergeStateError> for BuildPhaseError {
    fn from(e: MergeStateError) -> Self {
        Self::State(e)
    }
}

impl From<ConfigError> for BuildPhaseError {
    fn from(e: ConfigError) -> Self {
        Self::Config(e)
    }
}

impl From<CollectError> for BuildPhaseError {
    fn from(e: CollectError) -> Self {
        Self::Collect(e)
    }
}

impl From<ResolveError> for BuildPhaseError {
    fn from(e: ResolveError) -> Self {
        Self::Resolve(e)
    }
}

impl From<BuildError> for BuildPhaseError {
    fn from(e: BuildError) -> Self {
        Self::Build(e)
    }
}

// ---------------------------------------------------------------------------
// run_build_phase
// ---------------------------------------------------------------------------

/// Execute the BUILD phase of the merge state machine.
///
/// Reads the merge-state file (which must be in `Prepare` phase), then
/// orchestrates the full merge pipeline:
///
/// 1. Advance merge-state to `Build` (fsync).
/// 2. Collect workspace snapshots via the backend.
/// 3. Partition changed paths (unique vs shared).
/// 4. Read base content from the epoch tree for shared paths.
/// 5. Resolve shared paths (hash equality, then diff3).
/// 6. Apply deterministic merge drivers (`regenerate` / `ours` / `theirs`).
/// 7. Build the candidate git tree + commit.
/// 8. Record the candidate OID in merge-state (fsync).
///
/// # Arguments
///
/// * `repo_root` — Path to the git repository root.
/// * `manifold_dir` — Path to the `.manifold/` directory.
/// * `backend` — Workspace backend for snapshot collection.
///
/// # Returns
///
/// A [`BuildPhaseOutput`] containing the candidate commit OID and any
/// unresolved conflicts.
///
/// # Errors
///
/// Returns [`BuildPhaseError`] if the merge-state is in the wrong phase,
/// any pipeline step fails, or the merge-state cannot be persisted.
pub fn run_build_phase<B: WorkspaceBackend>(
    repo_root: &Path,
    manifold_dir: &Path,
    backend: &B,
) -> Result<BuildPhaseOutput, BuildPhaseError> {
    // 1. Read and validate merge-state
    let state_path = MergeStateFile::default_path(manifold_dir);
    let mut state = MergeStateFile::read(&state_path)?;

    if state.phase != MergePhase::Prepare {
        return Err(BuildPhaseError::WrongPhase {
            expected: MergePhase::Prepare,
            actual: state.phase.clone(),
        });
    }

    // Load merge configuration (defaults if file missing).
    let config_path = manifold_dir.join("config.toml");
    let config = ManifoldConfig::load(&config_path)?;

    // 2. Advance to BUILD (fsync — crash after this means recovery aborts)
    let now = now_secs();
    state.advance(MergePhase::Build, now)?;
    state.write_atomic(&state_path)?;

    // Run the pipeline and capture the result. On error, the merge-state
    // stays in Build phase — recovery will abort it.
    let output = run_pipeline(repo_root, backend, &state, &config.merge)?;

    // 8. Record candidate OID in merge-state (fsync)
    state.epoch_candidate = Some(output.candidate.clone());
    state.updated_at = now_secs();
    state.write_atomic(&state_path)?;

    Ok(output)
}

/// Execute the BUILD phase with explicit inputs (for testing).
///
/// Does not read or write merge-state. Runs the full pipeline on the
/// provided inputs and returns the result.
///
/// # Arguments
///
/// * `repo_root` — Path to the git repository root.
/// * `backend` — Workspace backend for snapshot collection.
/// * `epoch` — The epoch commit to use as the merge base.
/// * `sources` — Workspace IDs to merge.
pub fn run_build_phase_with_inputs<B: WorkspaceBackend>(
    repo_root: &Path,
    backend: &B,
    epoch: &EpochId,
    sources: &[WorkspaceId],
) -> Result<BuildPhaseOutput, BuildPhaseError> {
    // 1. Collect snapshots (enriched with FileId + blob OID)
    let patch_sets = collect_snapshots(repo_root, backend, sources)?;

    // 2. Partition
    let partition = partition_by_path(&patch_sets);
    let unique_count = partition.unique_count();
    let shared_count = partition.shared_count();

    // 3. Read base contents for shared paths
    let base_contents = read_base_contents(repo_root, epoch, &partition)?;

    // Merge settings used by resolve + deterministic drivers.
    let merge_config = MergeConfig::default();

    // 4. Resolve shared paths via hash equality / diff3 / AST merge fallback
    let resolve_result = resolve_partition_for_build(&partition, &base_contents, &merge_config)?;

    // 5. Apply deterministic merge drivers
    let (resolved, conflicts) = apply_merge_drivers(
        repo_root,
        epoch,
        sources,
        &partition,
        &base_contents,
        resolve_result,
        &merge_config,
    )?;

    // 6. Build candidate commit
    let candidate = build_merge_commit(repo_root, epoch, sources, &resolved, None)?;

    Ok(BuildPhaseOutput {
        candidate,
        conflicts,
        resolved_count: resolved.len(),
        unique_count,
        shared_count,
    })
}

// ---------------------------------------------------------------------------
// Internal: pipeline execution
// ---------------------------------------------------------------------------

/// The core pipeline logic shared by both `run_build_phase` and
/// `run_build_phase_with_inputs`.
fn run_pipeline<B: WorkspaceBackend>(
    repo_root: &Path,
    backend: &B,
    state: &MergeStateFile,
    merge_config: &MergeConfig,
) -> Result<BuildPhaseOutput, BuildPhaseError> {
    // Collect snapshots from all source workspaces (enriched with FileId + blob OID)
    let patch_sets = collect_snapshots(repo_root, backend, &state.sources)?;

    // Partition changed paths into unique vs shared
    let partition = partition_by_path(&patch_sets);
    let unique_count = partition.unique_count();
    let shared_count = partition.shared_count();

    // Read base (epoch) content for all shared paths
    let base_contents = read_base_contents(repo_root, &state.epoch_before, &partition)?;

    // Resolve shared paths via hash equality / diff3 / AST merge fallback
    let resolve_result = resolve_partition_for_build(&partition, &base_contents, merge_config)?;

    // Apply deterministic merge drivers
    let (resolved, conflicts) = apply_merge_drivers(
        repo_root,
        &state.epoch_before,
        &state.sources,
        &partition,
        &base_contents,
        resolve_result,
        merge_config,
    )?;

    // Build the candidate git tree + commit from resolved changes
    let candidate = build_merge_commit(
        repo_root,
        &state.epoch_before,
        &state.sources,
        &resolved,
        None,
    )?;

    Ok(BuildPhaseOutput {
        candidate,
        conflicts,
        resolved_count: resolved.len(),
        unique_count,
        shared_count,
    })
}

fn resolve_partition_for_build(
    partition: &PartitionResult,
    base_contents: &BTreeMap<PathBuf, Vec<u8>>,
    merge_config: &MergeConfig,
) -> Result<ResolveResult, BuildPhaseError> {
    #[cfg(feature = "ast-merge")]
    {
        let ast_config = crate::merge::ast_merge::AstMergeConfig::from_config(&merge_config.ast);
        resolve_partition_with_ast(partition, base_contents, &ast_config)
            .map_err(BuildPhaseError::from)
    }

    #[cfg(not(feature = "ast-merge"))]
    {
        let _ = merge_config;
        resolve_partition(partition, base_contents).map_err(BuildPhaseError::from)
    }
}

// ---------------------------------------------------------------------------
// Internal: deterministic merge drivers
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct CompiledDriver {
    index: usize,
    pattern: Pattern,
    driver: MergeDriver,
}

fn apply_merge_drivers(
    repo_root: &Path,
    epoch: &EpochId,
    sources: &[WorkspaceId],
    partition: &PartitionResult,
    base_contents: &BTreeMap<PathBuf, Vec<u8>>,
    resolve_result: ResolveResult,
    merge_config: &MergeConfig,
) -> Result<(Vec<ResolvedChange>, Vec<ConflictRecord>), BuildPhaseError> {
    let effective_drivers = merge_config.effective_drivers();
    if effective_drivers.is_empty() {
        return Ok((resolve_result.resolved, resolve_result.conflicts));
    }

    let mut compiled = Vec::with_capacity(effective_drivers.len());
    for (index, driver) in effective_drivers.into_iter().enumerate() {
        let pattern = Pattern::new(&driver.match_glob).map_err(|e| {
            BuildPhaseError::Driver(format!(
                "invalid merge driver glob '{}': {e}",
                driver.match_glob
            ))
        })?;
        compiled.push(CompiledDriver {
            index,
            pattern,
            driver,
        });
    }

    let mut resolved_by_path: BTreeMap<PathBuf, ResolvedChange> = BTreeMap::new();
    for change in resolve_result.resolved {
        resolved_by_path.insert(change.path().clone(), change);
    }
    let mut conflicts = resolve_result.conflicts;
    let mut regenerate_by_driver: BTreeMap<usize, BTreeSet<PathBuf>> = BTreeMap::new();

    for (path, entry) in &partition.unique {
        maybe_apply_driver(
            path,
            std::slice::from_ref(entry),
            base_contents,
            &compiled,
            &mut resolved_by_path,
            &mut conflicts,
            &mut regenerate_by_driver,
        )?;
    }

    for (path, entries) in &partition.shared {
        maybe_apply_driver(
            path,
            entries,
            base_contents,
            &compiled,
            &mut resolved_by_path,
            &mut conflicts,
            &mut regenerate_by_driver,
        )?;
    }

    if !regenerate_by_driver.is_empty() {
        let provisional_resolved: Vec<ResolvedChange> =
            resolved_by_path.values().cloned().collect();
        let provisional_candidate =
            build_merge_commit(repo_root, epoch, sources, &provisional_resolved, None)?;

        let regenerated = run_regenerate_drivers(
            repo_root,
            &provisional_candidate,
            &compiled,
            &regenerate_by_driver,
        )?;

        for change in regenerated {
            resolved_by_path.insert(change.path().clone(), change);
        }
    }

    conflicts.sort_by(|a, b| a.path.cmp(&b.path));

    Ok((resolved_by_path.into_values().collect(), conflicts))
}

#[allow(clippy::too_many_arguments)]
fn maybe_apply_driver(
    path: &Path,
    entries: &[PathEntry],
    base_contents: &BTreeMap<PathBuf, Vec<u8>>,
    compiled: &[CompiledDriver],
    resolved_by_path: &mut BTreeMap<PathBuf, ResolvedChange>,
    conflicts: &mut Vec<ConflictRecord>,
    regenerate_by_driver: &mut BTreeMap<usize, BTreeSet<PathBuf>>,
) -> Result<(), BuildPhaseError> {
    let Some(driver) = select_driver(path, compiled) else {
        return Ok(());
    };

    match driver.driver.kind {
        MergeDriverKind::Ours => {
            let change = ours_change(path, base_contents.get(path));
            resolved_by_path.insert(path.to_path_buf(), change);
            remove_conflict_path(conflicts, path);
        }
        MergeDriverKind::Theirs => {
            let change = theirs_change(path, entries)?;
            resolved_by_path.insert(path.to_path_buf(), change);
            remove_conflict_path(conflicts, path);
        }
        MergeDriverKind::Regenerate => {
            let has_command = driver
                .driver
                .command
                .as_deref()
                .map(str::trim)
                .is_some_and(|cmd| !cmd.is_empty());
            if !has_command {
                return Err(BuildPhaseError::Driver(format!(
                    "regenerate driver for '{}' must set a non-empty command",
                    path.display()
                )));
            }

            regenerate_by_driver
                .entry(driver.index)
                .or_default()
                .insert(path.to_path_buf());
            remove_conflict_path(conflicts, path);
        }
    }

    Ok(())
}

fn select_driver<'a>(path: &Path, compiled: &'a [CompiledDriver]) -> Option<&'a CompiledDriver> {
    compiled
        .iter()
        .find(|driver| driver.pattern.matches_path(path))
}

fn remove_conflict_path(conflicts: &mut Vec<ConflictRecord>, path: &Path) {
    conflicts.retain(|conflict| conflict.path.as_path() != path);
}

fn ours_change(path: &Path, base: Option<&Vec<u8>>) -> ResolvedChange {
    base.map_or_else(
        || ResolvedChange::Delete {
            path: path.to_path_buf(),
        },
        |content| ResolvedChange::Upsert {
            path: path.to_path_buf(),
            content: content.clone(),
        },
    )
}

fn theirs_change(path: &Path, entries: &[PathEntry]) -> Result<ResolvedChange, BuildPhaseError> {
    if entries.len() != 1 {
        return Err(BuildPhaseError::Driver(format!(
            "theirs driver for '{}' requires exactly one workspace change (found {})",
            path.display(),
            entries.len()
        )));
    }

    let entry = &entries[0];
    if entry.is_deletion() {
        return Ok(ResolvedChange::Delete {
            path: path.to_path_buf(),
        });
    }

    let Some(content) = &entry.content else {
        return Err(BuildPhaseError::Driver(format!(
            "theirs driver for '{}' is missing file content from workspace {}",
            path.display(),
            entry.workspace_id.as_str()
        )));
    };

    Ok(ResolvedChange::Upsert {
        path: path.to_path_buf(),
        content: content.clone(),
    })
}

fn run_regenerate_drivers(
    repo_root: &Path,
    candidate: &GitOid,
    compiled: &[CompiledDriver],
    regenerate_by_driver: &BTreeMap<usize, BTreeSet<PathBuf>>,
) -> Result<Vec<ResolvedChange>, BuildPhaseError> {
    let nonce: u64 = rand::random();
    let worktree_path = std::env::temp_dir().join(format!("maw-build-regenerate-{nonce}"));

    create_temp_worktree(repo_root, candidate, &worktree_path)?;

    let result = (|| -> Result<Vec<ResolvedChange>, BuildPhaseError> {
        for (index, paths) in regenerate_by_driver {
            let Some(driver) = compiled.iter().find(|d| d.index == *index) else {
                return Err(BuildPhaseError::Driver(format!(
                    "internal error: missing compiled regenerate driver #{index}"
                )));
            };

            let Some(command) = driver
                .driver
                .command
                .as_deref()
                .map(str::trim)
                .filter(|cmd| !cmd.is_empty())
            else {
                return Err(BuildPhaseError::Driver(format!(
                    "regenerate driver '{}' has no command",
                    driver.driver.match_glob
                )));
            };

            let output = Command::new("sh")
                .args(["-c", command])
                .current_dir(&worktree_path)
                .output()
                .map_err(|e| {
                    BuildPhaseError::Driver(format!(
                        "failed to spawn regenerate command `{command}`: {e}"
                    ))
                })?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                let touched = paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(BuildPhaseError::Driver(format!(
                    "regenerate command failed for [{}]: `{command}` (exit {:?}){}; treated as validation failure",
                    touched,
                    output.status.code(),
                    if stderr.is_empty() {
                        String::new()
                    } else {
                        format!(": {stderr}")
                    }
                )));
            }
        }

        let mut regenerated = Vec::new();
        for paths in regenerate_by_driver.values() {
            for path in paths {
                let full = worktree_path.join(path);
                if full.is_file() {
                    let content = fs::read(&full).map_err(|e| {
                        BuildPhaseError::Driver(format!(
                            "failed to read regenerated file '{}': {e}",
                            path.display()
                        ))
                    })?;
                    regenerated.push(ResolvedChange::Upsert {
                        path: path.clone(),
                        content,
                    });
                } else {
                    regenerated.push(ResolvedChange::Delete { path: path.clone() });
                }
            }
        }

        regenerated.sort_by(|a, b| a.path().cmp(b.path()));
        Ok(regenerated)
    })();

    let cleanup_result = remove_temp_worktree(repo_root, &worktree_path);
    let _ = fs::remove_dir_all(&worktree_path);

    match (result, cleanup_result) {
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
        (Ok(changes), Ok(())) => Ok(changes),
    }
}

fn create_temp_worktree(
    repo_root: &Path,
    candidate: &GitOid,
    worktree_path: &Path,
) -> Result<(), BuildPhaseError> {
    let path = worktree_path.to_string_lossy().to_string();
    let output = Command::new("git")
        .args(["worktree", "add", "--detach", &path, candidate.as_str()])
        .current_dir(repo_root)
        .output()
        .map_err(|e| BuildPhaseError::Driver(format!("spawn git worktree add: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(BuildPhaseError::Driver(format!(
            "git worktree add for regenerate driver failed: {stderr}"
        )));
    }

    Ok(())
}

fn remove_temp_worktree(repo_root: &Path, worktree_path: &Path) -> Result<(), BuildPhaseError> {
    let path = worktree_path.to_string_lossy().to_string();
    let output = Command::new("git")
        .args(["worktree", "remove", "--force", &path])
        .current_dir(repo_root)
        .output()
        .map_err(|e| BuildPhaseError::Driver(format!("spawn git worktree remove: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(BuildPhaseError::Driver(format!(
            "git worktree remove for regenerate driver failed: {stderr}"
        )));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Internal: read base contents from epoch tree
// ---------------------------------------------------------------------------

/// Read file contents from the epoch tree for all touched paths.
///
/// For each path that appears in either `partition.unique` or
/// `partition.shared`, reads file content from the epoch commit via
/// `git show <epoch>:<path>`. Paths that don't exist at the epoch are omitted
/// from the result.
fn read_base_contents(
    repo_root: &Path,
    epoch: &EpochId,
    partition: &PartitionResult,
) -> Result<BTreeMap<PathBuf, Vec<u8>>, BuildPhaseError> {
    let mut base_contents = BTreeMap::new();

    let touched_paths = partition
        .unique
        .iter()
        .map(|(path, _)| path)
        .chain(partition.shared.iter().map(|(path, _)| path));

    for path in touched_paths {
        match read_file_at_epoch(repo_root, epoch, path) {
            Ok(content) => {
                base_contents.insert(path.clone(), content);
            }
            Err(ReadBaseError::NotFound) => {
                // Path doesn't exist at epoch — it's a new file.
            }
            Err(ReadBaseError::GitError(detail)) => {
                return Err(BuildPhaseError::ReadBase {
                    path: path.clone(),
                    detail,
                });
            }
        }
    }

    Ok(base_contents)
}

#[derive(Debug)]
enum ReadBaseError {
    /// File doesn't exist at the epoch commit.
    NotFound,
    /// Git command failed.
    GitError(String),
}

/// Read a single file's content from the epoch commit.
///
/// Uses `git show <epoch>:<path>` which outputs raw file content to stdout.
fn read_file_at_epoch(
    repo_root: &Path,
    epoch: &EpochId,
    path: &Path,
) -> Result<Vec<u8>, ReadBaseError> {
    let spec = format!("{}:{}", epoch.as_str(), path.display());

    let output = Command::new("git")
        .args(["show", &spec])
        .current_dir(repo_root)
        .output()
        .map_err(|e| ReadBaseError::GitError(format!("spawn git show: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // git show returns 128 when the path doesn't exist
        if stderr.contains("does not exist")
            || stderr.contains("path")
            || output.status.code() == Some(128)
        {
            return Err(ReadBaseError::NotFound);
        }
        return Err(ReadBaseError::GitError(format!(
            "git show {spec} failed: {}",
            stderr.trim()
        )));
    }

    Ok(output.stdout)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_secs() -> u64 {
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
    use crate::backend::{SnapshotResult, WorkspaceStatus};
    use crate::merge_state::{recover_from_merge_state, RecoveryOutcome};
    use crate::model::types::WorkspaceInfo;
    use std::fs;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Test git helpers
    // -----------------------------------------------------------------------

    fn run_git(root: &Path, args: &[&str]) -> String {
        let out = StdCommand::new("git")
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

    /// Create a test git repo with initial epoch commit.
    /// Returns (TempDir, epoch_oid).
    /// Epoch tree contains: README.md, lib.rs, src/main.rs.
    fn setup_epoch_repo() -> (TempDir, EpochId) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        run_git(root, &["init"]);
        run_git(root, &["config", "user.name", "Test"]);
        run_git(root, &["config", "user.email", "test@test.com"]);
        run_git(root, &["config", "commit.gpgsign", "false"]);

        fs::write(root.join("README.md"), "# Test Project\n").unwrap();
        fs::write(root.join("lib.rs"), "pub fn lib() {}\n").unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();

        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", "epoch: initial"]);

        let hex = run_git(root, &["rev-parse", "HEAD"]);
        let epoch = EpochId::new(&hex).unwrap();
        run_git(
            root,
            &["update-ref", "refs/manifold/epoch/current", epoch.as_str()],
        );

        (dir, epoch)
    }

    /// Create a merge-state in PREPARE phase and write it.
    fn write_prepare_state(
        manifold_dir: &Path,
        sources: &[WorkspaceId],
        epoch: &EpochId,
    ) -> PathBuf {
        fs::create_dir_all(manifold_dir).unwrap();
        let mut state = MergeStateFile::new(sources.to_vec(), epoch.clone(), 1000);
        for ws in sources {
            state.frozen_heads.insert(ws.clone(), epoch.oid().clone());
        }
        let state_path = MergeStateFile::default_path(manifold_dir);
        state.write_atomic(&state_path).unwrap();
        state_path
    }

    // -----------------------------------------------------------------------
    // Mock backend
    // -----------------------------------------------------------------------

    /// A mock workspace backend that returns pre-configured snapshots.
    /// Files must actually exist on disk at the workspace path for
    /// collect_one to read their content.
    struct MockBackend {
        snapshots: BTreeMap<String, SnapshotResult>,
        statuses: BTreeMap<String, WorkspaceStatus>,
        paths: BTreeMap<String, PathBuf>,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                snapshots: BTreeMap::new(),
                statuses: BTreeMap::new(),
                paths: BTreeMap::new(),
            }
        }

        /// Register a workspace with a snapshot and status.
        /// The caller must create the workspace directory and populate
        /// files before calling collect_snapshots.
        fn add_workspace(
            &mut self,
            name: &str,
            epoch: EpochId,
            snapshot: SnapshotResult,
            ws_path: PathBuf,
        ) {
            self.snapshots.insert(name.to_owned(), snapshot);
            self.statuses
                .insert(name.to_owned(), WorkspaceStatus::new(epoch, vec![], false));
            self.paths.insert(name.to_owned(), ws_path);
        }
    }

    #[derive(Debug)]
    struct MockError(String);

    impl fmt::Display for MockError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "mock: {}", self.0)
        }
    }

    impl std::error::Error for MockError {}

    impl WorkspaceBackend for MockBackend {
        type Error = MockError;

        fn create(
            &self,
            _name: &WorkspaceId,
            _epoch: &EpochId,
        ) -> Result<WorkspaceInfo, Self::Error> {
            Err(MockError("not implemented".into()))
        }

        fn destroy(&self, _name: &WorkspaceId) -> Result<(), Self::Error> {
            Err(MockError("not implemented".into()))
        }

        fn list(&self) -> Result<Vec<WorkspaceInfo>, Self::Error> {
            Ok(vec![])
        }

        fn status(&self, name: &WorkspaceId) -> Result<WorkspaceStatus, Self::Error> {
            self.statuses
                .get(name.as_str())
                .cloned()
                .ok_or_else(|| MockError(format!("workspace {} not found", name)))
        }

        fn snapshot(&self, name: &WorkspaceId) -> Result<SnapshotResult, Self::Error> {
            self.snapshots
                .get(name.as_str())
                .cloned()
                .ok_or_else(|| MockError(format!("workspace {} not found", name)))
        }

        fn workspace_path(&self, name: &WorkspaceId) -> PathBuf {
            self.paths
                .get(name.as_str())
                .cloned()
                .unwrap_or_else(|| PathBuf::from(format!("/tmp/ws/{}", name)))
        }

        fn exists(&self, name: &WorkspaceId) -> bool {
            self.snapshots.contains_key(name.as_str())
        }
    }

    /// Helper: create a workspace directory with files and return the
    /// appropriate SnapshotResult.
    fn make_workspace_with_added_file(
        base: &Path,
        ws_name: &str,
        file_name: &str,
        content: &[u8],
    ) -> (PathBuf, SnapshotResult) {
        let ws_path = base.join(format!("ws/{ws_name}"));
        fs::create_dir_all(&ws_path).unwrap();
        // Create parent dirs if needed
        let full_path = ws_path.join(file_name);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&full_path, content).unwrap();
        let snapshot = SnapshotResult::new(
            vec![PathBuf::from(file_name)], // added
            vec![],                         // modified
            vec![],                         // deleted
        );
        (ws_path, snapshot)
    }

    /// Helper: create a workspace directory with a modified file.
    fn make_workspace_with_modified_file(
        base: &Path,
        ws_name: &str,
        file_name: &str,
        content: &[u8],
    ) -> (PathBuf, SnapshotResult) {
        let ws_path = base.join(format!("ws/{ws_name}"));
        fs::create_dir_all(&ws_path).unwrap();
        let full_path = ws_path.join(file_name);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&full_path, content).unwrap();
        let snapshot = SnapshotResult::new(
            vec![],                         // added
            vec![PathBuf::from(file_name)], // modified
            vec![],                         // deleted
        );
        (ws_path, snapshot)
    }

    /// Helper: create a workspace directory for a deletion.
    fn make_workspace_with_deleted_file(
        base: &Path,
        ws_name: &str,
        file_name: &str,
    ) -> (PathBuf, SnapshotResult) {
        let ws_path = base.join(format!("ws/{ws_name}"));
        fs::create_dir_all(&ws_path).unwrap();
        let snapshot = SnapshotResult::new(
            vec![],                         // added
            vec![],                         // modified
            vec![PathBuf::from(file_name)], // deleted
        );
        (ws_path, snapshot)
    }

    fn write_merge_config(manifold_dir: &Path, contents: &str) {
        fs::create_dir_all(manifold_dir).unwrap();
        fs::write(manifold_dir.join("config.toml"), contents).unwrap();
    }

    fn commit_epoch_file(root: &Path, rel_path: &str, content: &str, message: &str) -> EpochId {
        let full = root.join(rel_path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&full, content).unwrap();
        run_git(root, &["add", rel_path]);
        run_git(root, &["commit", "-m", message]);
        let hex = run_git(root, &["rev-parse", "HEAD"]);
        let epoch = EpochId::new(&hex).unwrap();
        run_git(
            root,
            &["update-ref", "refs/manifold/epoch/current", epoch.as_str()],
        );
        epoch
    }

    // -----------------------------------------------------------------------
    // read_file_at_epoch tests
    // -----------------------------------------------------------------------

    #[test]
    fn read_file_at_epoch_returns_content() {
        let (dir, epoch) = setup_epoch_repo();
        let content = read_file_at_epoch(dir.path(), &epoch, Path::new("README.md")).unwrap();
        assert_eq!(content, b"# Test Project\n");
    }

    #[test]
    fn read_file_at_epoch_returns_not_found_for_missing_path() {
        let (dir, epoch) = setup_epoch_repo();
        let result = read_file_at_epoch(dir.path(), &epoch, Path::new("nonexistent.txt"));
        assert!(matches!(result, Err(ReadBaseError::NotFound)));
    }

    #[test]
    fn read_file_at_epoch_nested_path() {
        let (dir, epoch) = setup_epoch_repo();
        let content = read_file_at_epoch(dir.path(), &epoch, Path::new("src/main.rs")).unwrap();
        assert_eq!(content, b"fn main() {}\n");
    }

    // -----------------------------------------------------------------------
    // read_base_contents tests
    // -----------------------------------------------------------------------

    #[test]
    fn read_base_contents_returns_shared_paths() {
        let (dir, epoch) = setup_epoch_repo();

        // Create patch sets where both workspaces modify README.md
        let ws_a = WorkspaceId::new("ws-a").unwrap();
        let ws_b = WorkspaceId::new("ws-b").unwrap();

        use crate::merge::types::{ChangeKind, FileChange, PatchSet};

        let patch_sets = vec![
            PatchSet::new(
                ws_a.clone(),
                epoch.clone(),
                vec![FileChange::new(
                    PathBuf::from("README.md"),
                    ChangeKind::Modified,
                    Some(b"# Modified by A\n".to_vec()),
                )],
            ),
            PatchSet::new(
                ws_b.clone(),
                epoch.clone(),
                vec![FileChange::new(
                    PathBuf::from("README.md"),
                    ChangeKind::Modified,
                    Some(b"# Modified by B\n".to_vec()),
                )],
            ),
        ];

        let partition = partition_by_path(&patch_sets);
        assert_eq!(partition.shared_count(), 1);

        let base = read_base_contents(dir.path(), &epoch, &partition).unwrap();
        assert_eq!(base.len(), 1);
        assert_eq!(base[&PathBuf::from("README.md")], b"# Test Project\n");
    }

    #[test]
    fn read_base_contents_omits_new_files() {
        let (dir, epoch) = setup_epoch_repo();

        use crate::merge::types::{ChangeKind, FileChange, PatchSet};
        let ws_a = WorkspaceId::new("ws-a").unwrap();
        let ws_b = WorkspaceId::new("ws-b").unwrap();

        let patch_sets = vec![
            PatchSet::new(
                ws_a,
                epoch.clone(),
                vec![FileChange::new(
                    PathBuf::from("new_file.txt"),
                    ChangeKind::Added,
                    Some(b"from A\n".to_vec()),
                )],
            ),
            PatchSet::new(
                ws_b,
                epoch.clone(),
                vec![FileChange::new(
                    PathBuf::from("new_file.txt"),
                    ChangeKind::Added,
                    Some(b"from B\n".to_vec()),
                )],
            ),
        ];

        let partition = partition_by_path(&patch_sets);
        assert_eq!(partition.shared_count(), 1);

        let base = read_base_contents(dir.path(), &epoch, &partition).unwrap();
        assert!(base.is_empty(), "new files not in epoch should be omitted");
    }

    // -----------------------------------------------------------------------
    // run_build_phase tests
    // -----------------------------------------------------------------------

    #[test]
    fn build_phase_wrong_state_rejected() {
        let dir = TempDir::new().unwrap();
        let manifold_dir = dir.path().join(".manifold");
        fs::create_dir_all(&manifold_dir).unwrap();

        let ws = WorkspaceId::new("ws-1").unwrap();
        let epoch = EpochId::new(&"a".repeat(40)).unwrap();

        let mut state = MergeStateFile::new(vec![ws], epoch, 1000);
        state.advance(MergePhase::Build, 1001).unwrap();
        let state_path = MergeStateFile::default_path(&manifold_dir);
        state.write_atomic(&state_path).unwrap();

        let backend = MockBackend::new();
        let result = run_build_phase(dir.path(), &manifold_dir, &backend);
        assert!(matches!(
            result,
            Err(BuildPhaseError::WrongPhase {
                expected: MergePhase::Prepare,
                actual: MergePhase::Build,
            })
        ));
    }

    #[test]
    fn build_phase_merge_state_not_found() {
        let dir = TempDir::new().unwrap();
        let manifold_dir = dir.path().join(".manifold");
        fs::create_dir_all(&manifold_dir).unwrap();

        let backend = MockBackend::new();
        let result = run_build_phase(dir.path(), &manifold_dir, &backend);
        assert!(matches!(result, Err(BuildPhaseError::State(_))));
    }

    #[test]
    fn build_phase_advances_state_and_records_candidate() {
        let (dir, epoch) = setup_epoch_repo();
        let manifold_dir = dir.path().join(".manifold");
        let ws = WorkspaceId::new("ws-1").unwrap();
        let state_path = write_prepare_state(&manifold_dir, &[ws.clone()], &epoch);

        // Empty workspace — no changes
        let ws_path = dir.path().join("ws/ws-1");
        fs::create_dir_all(&ws_path).unwrap();
        let mut backend = MockBackend::new();
        backend.add_workspace(
            "ws-1",
            epoch.clone(),
            SnapshotResult::new(vec![], vec![], vec![]),
            ws_path,
        );

        let output = run_build_phase(dir.path(), &manifold_dir, &backend).unwrap();

        // Merge-state advanced to Build with candidate OID recorded
        let final_state = MergeStateFile::read(&state_path).unwrap();
        assert_eq!(final_state.phase, MergePhase::Build);
        assert_eq!(final_state.epoch_candidate, Some(output.candidate.clone()));
    }

    #[test]
    fn build_phase_crash_recovery_aborts_without_moving_refs() {
        let (dir, epoch) = setup_epoch_repo();
        let manifold_dir = dir.path().join(".manifold");
        let ws = WorkspaceId::new("ws-1").unwrap();
        let state_path = write_prepare_state(&manifold_dir, &[ws.clone()], &epoch);

        let (ws_path, snapshot) = make_workspace_with_added_file(
            dir.path(),
            "ws-1",
            "feature.rs",
            b"pub fn feature() {}\n",
        );
        let mut backend = MockBackend::new();
        backend.add_workspace("ws-1", epoch.clone(), snapshot, ws_path.clone());

        let head_before = run_git(dir.path(), &["rev-parse", "HEAD"]);
        let epoch_before = run_git(dir.path(), &["rev-parse", "refs/manifold/epoch/current"]);

        let output = run_build_phase(dir.path(), &manifold_dir, &backend).unwrap();

        let outcome = recover_from_merge_state(&state_path).unwrap();
        assert_eq!(
            outcome,
            RecoveryOutcome::AbortedPreCommit {
                from: MergePhase::Build
            }
        );
        assert!(!state_path.exists());

        let head_after = run_git(dir.path(), &["rev-parse", "HEAD"]);
        let epoch_after = run_git(dir.path(), &["rev-parse", "refs/manifold/epoch/current"]);
        assert_eq!(head_after, head_before, "BUILD recovery must not move HEAD");
        assert_eq!(
            epoch_after, epoch_before,
            "BUILD recovery must not move epoch ref"
        );

        // Candidate object may exist as an orphan, which is safe.
        run_git(
            dir.path(),
            &[
                "cat-file",
                "-e",
                &format!("{}^{{commit}}", output.candidate.as_str()),
            ],
        );

        assert_eq!(
            fs::read_to_string(ws_path.join("feature.rs")).unwrap(),
            "pub fn feature() {}\n"
        );

        run_git(dir.path(), &["fsck", "--no-progress"]);
    }

    #[test]
    fn build_phase_no_changes_produces_valid_commit() {
        let (dir, epoch) = setup_epoch_repo();
        let manifold_dir = dir.path().join(".manifold");
        let ws = WorkspaceId::new("ws-1").unwrap();
        write_prepare_state(&manifold_dir, &[ws.clone()], &epoch);

        let ws_path = dir.path().join("ws/ws-1");
        fs::create_dir_all(&ws_path).unwrap();
        let mut backend = MockBackend::new();
        backend.add_workspace(
            "ws-1",
            epoch.clone(),
            SnapshotResult::new(vec![], vec![], vec![]),
            ws_path,
        );

        let output = run_build_phase(dir.path(), &manifold_dir, &backend).unwrap();

        assert!(!output.candidate.as_str().is_empty());
        assert!(output.conflicts.is_empty());
        assert_eq!(output.resolved_count, 0);
        assert_eq!(output.unique_count, 0);
        assert_eq!(output.shared_count, 0);
    }

    #[test]
    fn build_phase_adds_new_file() {
        let (dir, epoch) = setup_epoch_repo();
        let manifold_dir = dir.path().join(".manifold");
        let ws = WorkspaceId::new("ws-1").unwrap();
        write_prepare_state(&manifold_dir, &[ws.clone()], &epoch);

        let (ws_path, snapshot) = make_workspace_with_added_file(
            dir.path(),
            "ws-1",
            "feature.rs",
            b"pub fn feature() {}\n",
        );
        let mut backend = MockBackend::new();
        backend.add_workspace("ws-1", epoch.clone(), snapshot, ws_path);

        let output = run_build_phase(dir.path(), &manifold_dir, &backend).unwrap();

        assert!(output.conflicts.is_empty());
        assert_eq!(output.resolved_count, 1);
        assert_eq!(output.unique_count, 1);
        assert_eq!(output.shared_count, 0);

        // Verify file is in the candidate tree
        let tree = run_git(
            dir.path(),
            &["ls-tree", "-r", "--name-only", output.candidate.as_str()],
        );
        assert!(tree.contains("feature.rs"));
        // Original files still present
        assert!(tree.contains("README.md"));
        assert!(tree.contains("lib.rs"));
    }

    #[test]
    fn build_phase_disjoint_two_workspaces() {
        let (dir, epoch) = setup_epoch_repo();
        let manifold_dir = dir.path().join(".manifold");
        let ws_a = WorkspaceId::new("ws-a").unwrap();
        let ws_b = WorkspaceId::new("ws-b").unwrap();
        write_prepare_state(&manifold_dir, &[ws_a.clone(), ws_b.clone()], &epoch);

        let (path_a, snap_a) =
            make_workspace_with_added_file(dir.path(), "ws-a", "feature_a.rs", b"pub fn a() {}\n");
        let (path_b, snap_b) =
            make_workspace_with_added_file(dir.path(), "ws-b", "feature_b.rs", b"pub fn b() {}\n");

        let mut backend = MockBackend::new();
        backend.add_workspace("ws-a", epoch.clone(), snap_a, path_a);
        backend.add_workspace("ws-b", epoch.clone(), snap_b, path_b);

        let output = run_build_phase(dir.path(), &manifold_dir, &backend).unwrap();

        assert!(output.conflicts.is_empty());
        assert_eq!(output.resolved_count, 2);
        assert_eq!(output.unique_count, 2);
        assert_eq!(output.shared_count, 0);

        let tree = run_git(
            dir.path(),
            &["ls-tree", "-r", "--name-only", output.candidate.as_str()],
        );
        assert!(tree.contains("feature_a.rs"));
        assert!(tree.contains("feature_b.rs"));
        assert!(tree.contains("README.md"));
    }

    #[test]
    fn build_phase_identical_modifications_resolve_cleanly() {
        let (dir, epoch) = setup_epoch_repo();
        let manifold_dir = dir.path().join(".manifold");
        let ws_a = WorkspaceId::new("ws-a").unwrap();
        let ws_b = WorkspaceId::new("ws-b").unwrap();
        write_prepare_state(&manifold_dir, &[ws_a.clone(), ws_b.clone()], &epoch);

        let new_content = b"# Updated README\n";
        let (path_a, snap_a) =
            make_workspace_with_modified_file(dir.path(), "ws-a", "README.md", new_content);
        let (path_b, snap_b) =
            make_workspace_with_modified_file(dir.path(), "ws-b", "README.md", new_content);

        let mut backend = MockBackend::new();
        backend.add_workspace("ws-a", epoch.clone(), snap_a, path_a);
        backend.add_workspace("ws-b", epoch.clone(), snap_b, path_b);

        let output = run_build_phase(dir.path(), &manifold_dir, &backend).unwrap();

        // Hash equality short-circuit: identical changes = no conflict
        assert!(output.conflicts.is_empty());
        assert_eq!(output.resolved_count, 1);
        assert_eq!(output.shared_count, 1);

        // Verify content
        let content = run_git(
            dir.path(),
            &["show", &format!("{}:README.md", output.candidate.as_str())],
        );
        assert_eq!(content, "# Updated README");
    }

    #[test]
    fn build_phase_delete_removes_file_from_tree() {
        let (dir, epoch) = setup_epoch_repo();
        let manifold_dir = dir.path().join(".manifold");
        let ws = WorkspaceId::new("ws-1").unwrap();
        write_prepare_state(&manifold_dir, &[ws.clone()], &epoch);

        let (ws_path, snapshot) = make_workspace_with_deleted_file(dir.path(), "ws-1", "lib.rs");

        let mut backend = MockBackend::new();
        backend.add_workspace("ws-1", epoch.clone(), snapshot, ws_path);

        let output = run_build_phase(dir.path(), &manifold_dir, &backend).unwrap();

        assert!(output.conflicts.is_empty());

        let tree = run_git(
            dir.path(),
            &["ls-tree", "-r", "--name-only", output.candidate.as_str()],
        );
        assert!(!tree.contains("lib.rs"), "deleted file must be removed");
        assert!(tree.contains("README.md"), "other files preserved");
    }

    #[test]
    fn build_phase_candidate_parent_is_epoch() {
        let (dir, epoch) = setup_epoch_repo();
        let manifold_dir = dir.path().join(".manifold");
        let ws = WorkspaceId::new("ws-1").unwrap();
        write_prepare_state(&manifold_dir, &[ws.clone()], &epoch);

        let (ws_path, snapshot) =
            make_workspace_with_added_file(dir.path(), "ws-1", "test.txt", b"test content\n");
        let mut backend = MockBackend::new();
        backend.add_workspace("ws-1", epoch.clone(), snapshot, ws_path);

        let output = run_build_phase(dir.path(), &manifold_dir, &backend).unwrap();

        let parent = run_git(
            dir.path(),
            &["rev-parse", &format!("{}^", output.candidate.as_str())],
        );
        assert_eq!(parent, epoch.as_str());
    }

    #[test]
    fn build_phase_is_deterministic() {
        let (dir, epoch) = setup_epoch_repo();
        let ws = WorkspaceId::new("ws-1").unwrap();

        let mut tree_oids = vec![];
        for _ in 0..2 {
            let manifold_dir = dir.path().join(".manifold");
            // Clean up merge-state between runs
            let state_path = MergeStateFile::default_path(&manifold_dir);
            if state_path.exists() {
                fs::remove_file(&state_path).unwrap();
            }
            write_prepare_state(&manifold_dir, &[ws.clone()], &epoch);

            let (ws_path, snapshot) = make_workspace_with_added_file(
                dir.path(),
                "ws-1",
                "new.txt",
                b"deterministic content\n",
            );
            let mut backend = MockBackend::new();
            backend.add_workspace("ws-1", epoch.clone(), snapshot, ws_path);

            let output = run_build_phase(dir.path(), &manifold_dir, &backend).unwrap();
            let tree_oid = run_git(
                dir.path(),
                &[
                    "rev-parse",
                    &format!("{}^{{tree}}", output.candidate.as_str()),
                ],
            );
            tree_oids.push(tree_oid);
        }

        assert_eq!(
            tree_oids[0], tree_oids[1],
            "same inputs must produce same tree OID"
        );
    }

    #[test]
    fn build_phase_three_way_disjoint() {
        let (dir, epoch) = setup_epoch_repo();
        let manifold_dir = dir.path().join(".manifold");
        let ws_a = WorkspaceId::new("ws-a").unwrap();
        let ws_b = WorkspaceId::new("ws-b").unwrap();
        let ws_c = WorkspaceId::new("ws-c").unwrap();
        write_prepare_state(
            &manifold_dir,
            &[ws_a.clone(), ws_b.clone(), ws_c.clone()],
            &epoch,
        );

        let (pa, sa) = make_workspace_with_added_file(dir.path(), "ws-a", "a.txt", b"aaa\n");
        let (pb, sb) = make_workspace_with_added_file(dir.path(), "ws-b", "b.txt", b"bbb\n");
        let (pc, sc) = make_workspace_with_added_file(dir.path(), "ws-c", "c.txt", b"ccc\n");

        let mut backend = MockBackend::new();
        backend.add_workspace("ws-a", epoch.clone(), sa, pa);
        backend.add_workspace("ws-b", epoch.clone(), sb, pb);
        backend.add_workspace("ws-c", epoch.clone(), sc, pc);

        let output = run_build_phase(dir.path(), &manifold_dir, &backend).unwrap();

        assert!(output.conflicts.is_empty());
        assert_eq!(output.resolved_count, 3);
        assert_eq!(output.unique_count, 3);

        let tree = run_git(
            dir.path(),
            &["ls-tree", "-r", "--name-only", output.candidate.as_str()],
        );
        assert!(tree.contains("a.txt"));
        assert!(tree.contains("b.txt"));
        assert!(tree.contains("c.txt"));
    }

    #[test]
    fn build_phase_with_inputs_bypasses_state_file() {
        let (dir, epoch) = setup_epoch_repo();
        let ws = WorkspaceId::new("ws-1").unwrap();

        let (ws_path, snapshot) =
            make_workspace_with_added_file(dir.path(), "ws-1", "hello.txt", b"hello world\n");
        let mut backend = MockBackend::new();
        backend.add_workspace("ws-1", epoch.clone(), snapshot, ws_path);

        // No merge-state file at all — with_inputs doesn't need one
        let output = run_build_phase_with_inputs(dir.path(), &backend, &epoch, &[ws]).unwrap();

        assert!(!output.candidate.as_str().is_empty());
        assert!(output.conflicts.is_empty());
        assert_eq!(output.resolved_count, 1);
    }

    #[test]
    fn build_phase_error_display() {
        let err = BuildPhaseError::WrongPhase {
            expected: MergePhase::Prepare,
            actual: MergePhase::Validate,
        };
        let msg = format!("{err}");
        assert!(msg.contains("wrong phase"));
        assert!(msg.contains("prepare"));
        assert!(msg.contains("validate"));

        let err = BuildPhaseError::ReadBase {
            path: PathBuf::from("src/main.rs"),
            detail: "not found".to_owned(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("src/main.rs"));
        assert!(msg.contains("not found"));
    }

    #[test]
    fn build_phase_mixed_add_modify_delete() {
        let (dir, epoch) = setup_epoch_repo();
        let manifold_dir = dir.path().join(".manifold");
        let ws = WorkspaceId::new("ws-1").unwrap();
        write_prepare_state(&manifold_dir, &[ws.clone()], &epoch);

        // Create workspace with all three change types
        let ws_path = dir.path().join("ws/ws-1");
        fs::create_dir_all(&ws_path).unwrap();

        // Added file
        fs::write(ws_path.join("new.txt"), "new content\n").unwrap();
        // Modified file — write new content at workspace path
        fs::write(ws_path.join("README.md"), "# Updated\n").unwrap();
        // Deleted file — lib.rs (no need to create on disk for delete)

        let snapshot = SnapshotResult::new(
            vec![PathBuf::from("new.txt")],
            vec![PathBuf::from("README.md")],
            vec![PathBuf::from("lib.rs")],
        );

        let mut backend = MockBackend::new();
        backend.add_workspace("ws-1", epoch.clone(), snapshot, ws_path);

        let output = run_build_phase(dir.path(), &manifold_dir, &backend).unwrap();

        assert!(output.conflicts.is_empty());
        assert_eq!(output.resolved_count, 3); // add + modify + delete

        let tree = run_git(
            dir.path(),
            &["ls-tree", "-r", "--name-only", output.candidate.as_str()],
        );
        assert!(tree.contains("new.txt"), "added file present");
        assert!(tree.contains("README.md"), "modified file present");
        assert!(!tree.contains("lib.rs"), "deleted file removed");

        // Verify modified content
        let readme = run_git(
            dir.path(),
            &["show", &format!("{}:README.md", output.candidate.as_str())],
        );
        assert_eq!(readme, "# Updated");
    }

    #[test]
    fn build_phase_regenerate_driver_resolves_cargo_lock_conflict() {
        let (dir, _epoch0) = setup_epoch_repo();
        let _ = commit_epoch_file(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            "epoch: add Cargo.toml",
        );
        let epoch = commit_epoch_file(
            dir.path(),
            "Cargo.lock",
            "# base lock\n",
            "epoch: add Cargo.lock",
        );

        let manifold_dir = dir.path().join(".manifold");
        let ws_a = WorkspaceId::new("ws-a").unwrap();
        let ws_b = WorkspaceId::new("ws-b").unwrap();
        write_prepare_state(&manifold_dir, &[ws_a.clone(), ws_b.clone()], &epoch);

        write_merge_config(
            &manifold_dir,
            r#"[[merge.drivers]]
match = "Cargo.lock"
kind = "regenerate"
command = "printf 're-generated lockfile\n' > Cargo.lock"
"#,
        );

        let (path_a, snap_a) =
            make_workspace_with_modified_file(dir.path(), "ws-a", "Cargo.lock", b"from ws-a\n");
        let (path_b, snap_b) =
            make_workspace_with_modified_file(dir.path(), "ws-b", "Cargo.lock", b"from ws-b\n");

        let mut backend = MockBackend::new();
        backend.add_workspace("ws-a", epoch.clone(), snap_a, path_a);
        backend.add_workspace("ws-b", epoch.clone(), snap_b, path_b);

        let output = run_build_phase(dir.path(), &manifold_dir, &backend).unwrap();
        assert!(output.conflicts.is_empty());

        let lock = run_git(
            dir.path(),
            &["show", &format!("{}:Cargo.lock", output.candidate.as_str())],
        );
        assert_eq!(lock, "re-generated lockfile");
    }

    #[test]
    fn build_phase_regenerate_driver_resolves_generated_artifact_glob() {
        let (dir, _epoch0) = setup_epoch_repo();
        let epoch = commit_epoch_file(
            dir.path(),
            "src/gen/schema.json",
            "{\"version\":0}\n",
            "epoch: add generated artifact",
        );

        let manifold_dir = dir.path().join(".manifold");
        let ws_a = WorkspaceId::new("ws-a").unwrap();
        let ws_b = WorkspaceId::new("ws-b").unwrap();
        write_prepare_state(&manifold_dir, &[ws_a.clone(), ws_b.clone()], &epoch);

        write_merge_config(
            &manifold_dir,
            r#"[[merge.drivers]]
match = "src/gen/**"
kind = "regenerate"
command = "mkdir -p src/gen && printf '{\"version\":42}\n' > src/gen/schema.json"
"#,
        );

        let (path_a, snap_a) = make_workspace_with_modified_file(
            dir.path(),
            "ws-a",
            "src/gen/schema.json",
            b"{\"version\":1}\n",
        );
        let (path_b, snap_b) = make_workspace_with_modified_file(
            dir.path(),
            "ws-b",
            "src/gen/schema.json",
            b"{\"version\":2}\n",
        );

        let mut backend = MockBackend::new();
        backend.add_workspace("ws-a", epoch.clone(), snap_a, path_a);
        backend.add_workspace("ws-b", epoch.clone(), snap_b, path_b);

        let output = run_build_phase(dir.path(), &manifold_dir, &backend).unwrap();
        assert!(output.conflicts.is_empty());

        let generated = run_git(
            dir.path(),
            &[
                "show",
                &format!("{}:src/gen/schema.json", output.candidate.as_str()),
            ],
        );
        assert_eq!(generated, "{\"version\":42}");
    }

    #[test]
    fn build_phase_ours_driver_keeps_epoch_version() {
        let (dir, epoch) = setup_epoch_repo();
        let manifold_dir = dir.path().join(".manifold");
        let ws_a = WorkspaceId::new("ws-a").unwrap();
        let ws_b = WorkspaceId::new("ws-b").unwrap();
        write_prepare_state(&manifold_dir, &[ws_a.clone(), ws_b.clone()], &epoch);

        write_merge_config(
            &manifold_dir,
            r#"[[merge.drivers]]
match = "README.md"
kind = "ours"
"#,
        );

        let (path_a, snap_a) =
            make_workspace_with_modified_file(dir.path(), "ws-a", "README.md", b"# ws-a\n");
        let (path_b, snap_b) =
            make_workspace_with_modified_file(dir.path(), "ws-b", "README.md", b"# ws-b\n");

        let mut backend = MockBackend::new();
        backend.add_workspace("ws-a", epoch.clone(), snap_a, path_a);
        backend.add_workspace("ws-b", epoch.clone(), snap_b, path_b);

        let output = run_build_phase(dir.path(), &manifold_dir, &backend).unwrap();
        assert!(output.conflicts.is_empty());

        let readme = run_git(
            dir.path(),
            &["show", &format!("{}:README.md", output.candidate.as_str())],
        );
        assert_eq!(readme, "# Test Project");
    }

    #[test]
    fn build_phase_theirs_driver_requires_single_workspace() {
        let (dir, epoch) = setup_epoch_repo();
        let manifold_dir = dir.path().join(".manifold");
        let ws_a = WorkspaceId::new("ws-a").unwrap();
        let ws_b = WorkspaceId::new("ws-b").unwrap();
        write_prepare_state(&manifold_dir, &[ws_a.clone(), ws_b.clone()], &epoch);

        write_merge_config(
            &manifold_dir,
            r#"[[merge.drivers]]
match = "README.md"
kind = "theirs"
"#,
        );

        let (path_a, snap_a) =
            make_workspace_with_modified_file(dir.path(), "ws-a", "README.md", b"# ws-a\n");
        let (path_b, snap_b) =
            make_workspace_with_modified_file(dir.path(), "ws-b", "README.md", b"# ws-b\n");

        let mut backend = MockBackend::new();
        backend.add_workspace("ws-a", epoch.clone(), snap_a, path_a);
        backend.add_workspace("ws-b", epoch.clone(), snap_b, path_b);

        let err = run_build_phase(dir.path(), &manifold_dir, &backend).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("requires exactly one workspace"));
    }

    #[test]
    fn build_phase_regenerate_failure_reported_as_validation_failure() {
        let (dir, _epoch0) = setup_epoch_repo();
        let epoch = commit_epoch_file(
            dir.path(),
            "Cargo.lock",
            "# base lock\n",
            "epoch: add Cargo.lock",
        );

        let manifold_dir = dir.path().join(".manifold");
        let ws = WorkspaceId::new("ws-1").unwrap();
        write_prepare_state(&manifold_dir, std::slice::from_ref(&ws), &epoch);

        write_merge_config(
            &manifold_dir,
            r#"[[merge.drivers]]
match = "Cargo.lock"
kind = "regenerate"
command = "exit 19"
"#,
        );

        let (ws_path, snapshot) =
            make_workspace_with_modified_file(dir.path(), "ws-1", "Cargo.lock", b"changed\n");

        let mut backend = MockBackend::new();
        backend.add_workspace("ws-1", epoch.clone(), snapshot, ws_path);

        let err = run_build_phase(dir.path(), &manifold_dir, &backend).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("treated as validation failure"));
    }
}
