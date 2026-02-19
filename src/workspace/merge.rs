use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::backend::WorkspaceBackend;
use crate::config::ManifoldConfig;
use crate::format::OutputFormat;
use crate::merge::build_phase::{BuildPhaseOutput, run_build_phase};
use crate::merge::commit::{CommitRecovery, CommitResult, run_commit_phase, recover_partial_commit};
use crate::merge::prepare::run_prepare_phase;
use crate::merge::resolve::ConflictRecord;
use crate::merge::validate::{ValidateOutcome, run_validate_phase, write_validation_artifact};
use crate::merge_state::{
    MergePhase, MergeStateFile, run_cleanup_phase,
};
use crate::model::types::WorkspaceId;

use super::{get_backend, repo_root, MawConfig, DEFAULT_WORKSPACE};

// ---------------------------------------------------------------------------
// Conflict reporting
// ---------------------------------------------------------------------------

/// Print detailed conflict information from the merge engine's conflict records.
fn print_conflict_report(
    conflicts: &[ConflictRecord],
    default_ws_path: &Path,
    default_ws_name: &str,
) {
    println!();
    println!("Conflicts:");
    for conflict in conflicts {
        let reason = format!("{}", conflict.reason);
        println!("  {:<40} {reason}", conflict.path.display());
    }

    let ws_display = default_ws_path.display();
    println!();
    println!("To resolve:");
    println!(
        "  1. Examine the conflict details above and determine the correct content"
    );
    println!(
        "  2. Edit the conflicted files in {ws_display}/"
    );
    println!(
        "  3. Re-run the merge: maw ws merge <workspace names>"
    );
    println!();
    println!(
        "  Conflicts are reported by the merge engine, not as markers in files."
    );
    println!(
        "  Each conflict has a reason (divergent edits, add/add, etc.) to guide resolution."
    );
    let _ = default_ws_name; // used in the path above
}

/// Run pre-merge or post-merge hook commands from .maw.toml.
fn run_hooks(hooks: &[String], hook_type: &str, root: &Path, abort_on_failure: bool) -> Result<()> {
    if hooks.is_empty() {
        return Ok(());
    }

    println!("Running {hook_type} hooks...");

    for (i, cmd) in hooks.iter().enumerate() {
        println!("  [{}/{}] {cmd}", i + 1, hooks.len());

        let output = std::process::Command::new("sh")
            .args(["-c", cmd])
            .current_dir(root)
            .output()
            .with_context(|| format!("Failed to execute hook command: {cmd}"))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stdout.trim().is_empty() {
            for line in stdout.lines() {
                println!("      {line}");
            }
        }
        if !stderr.trim().is_empty() {
            for line in stderr.lines() {
                eprintln!("      {line}");
            }
        }

        if !output.status.success() {
            let exit_code = output.status.code().unwrap_or(-1);
            if abort_on_failure {
                bail!(
                    "{hook_type} hook failed (exit code {exit_code}): {cmd}\n  \
                     Merge aborted. Fix the issue and try again."
                );
            }
            eprintln!("  WARNING: {hook_type} hook failed (exit code {exit_code}): {cmd}");
        }
    }

    println!("{hook_type} hooks complete.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Merge check (pre-flight)
// ---------------------------------------------------------------------------

/// Result of a merge check — structured for JSON output.
#[derive(Debug, Serialize)]
pub struct CheckResult {
    pub ready: bool,
    pub conflicts: Vec<String>,
    pub stale: bool,
    pub workspace: CheckWorkspaceInfo,
    pub description: String,
}

/// Workspace info included in check result.
#[derive(Debug, Serialize)]
pub struct CheckWorkspaceInfo {
    pub name: String,
    pub change_id: String,
}

/// Pre-flight merge check using the new merge engine.
///
/// Runs PREPARE + BUILD without COMMIT to detect conflicts.
/// Returns a `CheckResult` with structured info.
pub fn check_merge(
    workspaces: &[String],
    format: OutputFormat,
) -> Result<()> {
    if workspaces.is_empty() {
        bail!("No workspaces specified for --check");
    }

    let root = repo_root()?;
    let maw_config = MawConfig::load(&root)?;
    let default_ws = maw_config.default_workspace();
    let backend = get_backend()?;

    // Reject merging the default workspace
    if workspaces.iter().any(|ws| ws == default_ws) {
        bail!(
            "Cannot merge the default workspace — it is the merge target, not a source."
        );
    }

    // Check staleness
    let stale_workspaces = super::check_stale_workspaces()?;
    let is_stale = workspaces.iter().any(|ws| stale_workspaces.contains(ws));

    let primary_ws = &workspaces[0];
    let ws_info = CheckWorkspaceInfo {
        name: primary_ws.clone(),
        change_id: String::new(), // Not available without jj
    };

    if is_stale {
        let result = CheckResult {
            ready: false,
            conflicts: Vec::new(),
            stale: true,
            workspace: ws_info,
            description: String::new(),
        };
        return output_check_result(&result, format);
    }

    // Try a BUILD phase to detect conflicts (don't COMMIT)
    let manifold_dir = root.join(".manifold");
    let check_dir = manifold_dir.join("check-tmp");
    let _ = std::fs::remove_dir_all(&check_dir);
    std::fs::create_dir_all(&check_dir)
        .context("Failed to create temp dir for merge check")?;

    let sources: Vec<WorkspaceId> = workspaces
        .iter()
        .map(|ws| WorkspaceId::new(ws).map_err(|e| anyhow::anyhow!("{e}")))
        .collect::<Result<Vec<_>>>()?;

    let mut workspace_dirs = BTreeMap::new();
    for ws_id in &sources {
        workspace_dirs.insert(ws_id.clone(), backend.workspace_path(ws_id));
    }

    // Run PREPARE in the temp dir
    let prepare_result = run_prepare_phase(&root, &check_dir, &sources, &workspace_dirs);

    // Clean up temp dir
    let _ = std::fs::remove_dir_all(&check_dir);

    match prepare_result {
        Ok(_frozen) => {
            // PREPARE succeeded, now try BUILD
            // Re-create dir for build
            std::fs::create_dir_all(&check_dir)
                .context("Failed to create temp dir for build check")?;
            let _ = run_prepare_phase(&root, &check_dir, &sources, &workspace_dirs);
            let build_result = run_build_phase(&root, &check_dir, &backend);
            let _ = std::fs::remove_dir_all(&check_dir);

            match build_result {
                Ok(output) => {
                    let conflicts: Vec<String> = output
                        .conflicts
                        .iter()
                        .map(|c| format!("{}: {}", c.path.display(), c.reason))
                        .collect();
                    let ready = conflicts.is_empty();
                    let result = CheckResult {
                        ready,
                        conflicts,
                        stale: false,
                        workspace: ws_info,
                        description: String::new(),
                    };
                    output_check_result(&result, format)
                }
                Err(e) => {
                    let result = CheckResult {
                        ready: false,
                        conflicts: vec![format!("build failed: {e}")],
                        stale: false,
                        workspace: ws_info,
                        description: String::new(),
                    };
                    output_check_result(&result, format)
                }
            }
        }
        Err(e) => {
            let result = CheckResult {
                ready: false,
                conflicts: vec![format!("prepare failed: {e}")],
                stale: false,
                workspace: ws_info,
                description: String::new(),
            };
            output_check_result(&result, format)
        }
    }
}

/// Output the check result in the requested format.
fn output_check_result(result: &CheckResult, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => {
            println!("{}", format.serialize(result)?);
        }
        _ => {
            if result.ready {
                println!("[OK] Ready to merge");
                println!("  Workspace: {}", result.workspace.name);
                if !result.description.is_empty() {
                    println!("  Description: {}", result.description);
                }
            } else if result.stale {
                println!("[BLOCKED] Workspace is stale — sync before merging");
                println!("  To fix: maw ws sync");
            } else if result.conflicts.is_empty() {
                println!("[BLOCKED] Merge check failed");
            } else {
                println!("[BLOCKED] Merge would produce {} conflict(s):", result.conflicts.len());
                for file in &result.conflicts {
                    println!("  C {file}");
                }
            }
        }
    }

    if result.ready {
        Ok(())
    } else {
        bail!("merge check: not ready")
    }
}

// ---------------------------------------------------------------------------
// Preview merge
// ---------------------------------------------------------------------------

/// Preview what a merge would do without creating any commits.
fn preview_merge(workspaces: &[String], root: &Path) -> Result<()> {
    let backend = get_backend()?;

    println!("=== Merge Preview (dry run) ===");
    println!();

    if workspaces.len() == 1 {
        println!("Would adopt workspace: {}", workspaces[0]);
    } else {
        println!("Would merge workspaces: {}", workspaces.join(", "));
    }
    println!();

    // Show changes in each workspace using the backend snapshot
    println!("=== Changes by Workspace ===");
    println!();

    for ws_name in workspaces {
        println!("--- {ws_name} ---");

        let ws_id = match WorkspaceId::new(ws_name) {
            Ok(id) => id,
            Err(e) => {
                println!("  Invalid workspace name: {e}");
                println!();
                continue;
            }
        };

        match backend.snapshot(&ws_id) {
            Ok(snapshot) => {
                if snapshot.is_empty() {
                    println!("  (no changes)");
                } else {
                    for path in &snapshot.added {
                        println!("  A {}", path.display());
                    }
                    for path in &snapshot.modified {
                        println!("  M {}", path.display());
                    }
                    for path in &snapshot.deleted {
                        println!("  D {}", path.display());
                    }
                    println!("  {} file(s) changed", snapshot.change_count());
                }
            }
            Err(e) => {
                println!("  Could not get changes: {e}");
            }
        }
        println!();
    }

    // Check for potential conflicts (files modified in multiple workspaces)
    if workspaces.len() > 1 {
        println!("=== Potential Conflicts ===");
        println!();

        let mut workspace_files: Vec<(String, Vec<PathBuf>)> = Vec::new();

        for ws_name in workspaces {
            if let Ok(ws_id) = WorkspaceId::new(ws_name) {
                if let Ok(snapshot) = backend.snapshot(&ws_id) {
                    let files: Vec<PathBuf> = snapshot
                        .all_changed()
                        .into_iter()
                        .cloned()
                        .collect();
                    workspace_files.push((ws_name.clone(), files));
                }
            }
        }

        let mut conflict_files: Vec<PathBuf> = Vec::new();
        for i in 0..workspace_files.len() {
            for j in (i + 1)..workspace_files.len() {
                let (ws1, files1) = &workspace_files[i];
                let (ws2, files2) = &workspace_files[j];
                for file in files1 {
                    if files2.contains(file) && !conflict_files.contains(file) {
                        conflict_files.push(file.clone());
                        println!(
                            "  ! {} - modified in both '{ws1}' and '{ws2}'",
                            file.display()
                        );
                    }
                }
            }
        }

        if conflict_files.is_empty() {
            println!("  (no overlapping changes detected)");
        } else {
            println!();
            println!("  Note: Overlapping files will be resolved via diff3 where possible.");
        }
        println!();
    }

    println!("=== Summary ===");
    println!();
    println!("To perform this merge, run without --dry-run:");
    println!("  maw ws merge {}", workspaces.join(" "));
    println!();

    let _ = root; // used implicitly via get_backend()
    Ok(())
}

// ---------------------------------------------------------------------------
// Merge options
// ---------------------------------------------------------------------------

/// Options controlling merge behavior.
pub struct MergeOptions<'a> {
    /// Destroy workspaces after a successful merge.
    pub destroy_after: bool,
    /// Ask for interactive confirmation before destroying workspaces.
    pub confirm: bool,
    /// Custom merge commit message (uses a generated default when `None`).
    pub message: Option<&'a str>,
    /// Preview the merge without creating any commits.
    pub dry_run: bool,
}

// ---------------------------------------------------------------------------
// Main merge function
// ---------------------------------------------------------------------------

/// Run the merge state machine: PREPARE → BUILD → VALIDATE → COMMIT → CLEANUP.
///
/// This replaces the old jj-based merge with the Manifold merge engine.
pub fn merge(
    workspaces: &[String],
    opts: &MergeOptions<'_>,
) -> Result<()> {
    let MergeOptions {
        destroy_after,
        confirm,
        message,
        dry_run,
    } = *opts;
    let ws_to_merge = workspaces.to_vec();

    if ws_to_merge.is_empty() {
        println!("No workspaces to merge.");
        return Ok(());
    }

    let root = repo_root()?;
    let maw_config = MawConfig::load(&root)?;
    let default_ws = maw_config.default_workspace();
    let branch = maw_config.branch();

    // Reject merging the default workspace
    if ws_to_merge.iter().any(|ws| ws == default_ws) {
        bail!(
            "Cannot merge the default workspace \u{2014} it is the merge target, not a source.\n\
             \n  To advance {branch} to include your edits in {default_ws}:\n\
             \n    maw push --advance\n\
             \n  This moves the {branch} bookmark to your latest commit and pushes."
        );
    }

    if dry_run {
        return preview_merge(&ws_to_merge, &root);
    }

    run_hooks(&maw_config.hooks.pre_merge, "pre-merge", &root, true)?;

    if ws_to_merge.len() == 1 {
        println!("Adopting workspace: {}", ws_to_merge[0]);
    } else {
        println!("Merging workspaces: {}", ws_to_merge.join(", "));
    }
    println!();

    // Set up paths
    let manifold_dir = root.join(".manifold");
    let default_ws_path = root.join("ws").join(default_ws);
    let backend = get_backend()?;

    // Convert workspace names to WorkspaceIds
    let sources: Vec<WorkspaceId> = ws_to_merge
        .iter()
        .map(|ws| {
            WorkspaceId::new(ws)
                .map_err(|e| anyhow::anyhow!("invalid workspace name '{ws}': {e}"))
        })
        .collect::<Result<Vec<_>>>()?;

    // Build workspace_dirs map for PREPARE
    let mut workspace_dirs = BTreeMap::new();
    for ws_id in &sources {
        let ws_path = backend.workspace_path(ws_id);
        if !ws_path.exists() {
            bail!(
                "Workspace '{}' does not exist at {}\n  \
                 Check available workspaces: maw ws list",
                ws_id,
                ws_path.display()
            );
        }
        workspace_dirs.insert(ws_id.clone(), ws_path);
    }

    // -----------------------------------------------------------------------
    // Phase 1: PREPARE — freeze inputs
    // -----------------------------------------------------------------------
    println!("PREPARE: Freezing merge inputs...");
    let frozen = run_prepare_phase(&root, &manifold_dir, &sources, &workspace_dirs)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!(
        "  Epoch: {}",
        &frozen.epoch.as_str()[..12]
    );
    for (ws_id, head) in &frozen.heads {
        println!(
            "  {}: {}",
            ws_id,
            &head.as_str()[..12]
        );
    }

    // -----------------------------------------------------------------------
    // Phase 2: BUILD — collect, partition, resolve, build candidate
    // -----------------------------------------------------------------------
    println!();
    println!("BUILD: Running merge engine...");
    let build_output = match run_build_phase(&root, &manifold_dir, &backend) {
        Ok(output) => output,
        Err(e) => {
            // Abort: clean up merge-state
            abort_merge(&manifold_dir, &format!("BUILD failed: {e}"))?;
            bail!("Merge BUILD phase failed: {e}");
        }
    };

    println!(
        "  {} unique path(s), {} shared path(s), {} resolved",
        build_output.unique_count, build_output.shared_count, build_output.resolved_count
    );
    println!(
        "  Candidate: {}",
        &build_output.candidate.as_str()[..12]
    );

    // Check for unresolved conflicts
    if !build_output.conflicts.is_empty() {
        println!(
            "  {} unresolved conflict(s)",
            build_output.conflicts.len()
        );
        print_conflict_report(&build_output.conflicts, &default_ws_path, default_ws);

        // Abort the merge — conflicts must be resolved first
        abort_merge(&manifold_dir, "unresolved conflicts")?;

        if destroy_after {
            println!();
            println!("NOT destroying workspaces due to conflicts.");
            println!("Resolve conflicts in the source workspaces, then retry:");
            println!("  maw ws merge {}", ws_to_merge.join(" "));
        }

        bail!(
            "Merge has {} unresolved conflict(s). Resolve them and retry.",
            build_output.conflicts.len()
        );
    }

    // -----------------------------------------------------------------------
    // Phase 3: VALIDATE — run post-merge validation commands
    // -----------------------------------------------------------------------
    let manifold_config = ManifoldConfig::load(&manifold_dir.join("config.toml"))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let validation_config = &manifold_config.merge.validation;

    if validation_config.has_commands() {
        println!();
        println!("VALIDATE: Running post-merge validation...");

        // Advance merge-state to Validate phase
        advance_merge_state(&manifold_dir, MergePhase::Validate)?;

        let validate_outcome = match run_validate_phase(
            &root,
            &build_output.candidate,
            validation_config,
        ) {
            Ok(outcome) => outcome,
            Err(e) => {
                abort_merge(&manifold_dir, &format!("VALIDATE error: {e}"))?;
                bail!("Merge VALIDATE phase failed: {e}");
            }
        };

        // Write validation artifact for diagnostics
        if let Some(result) = validate_outcome.result() {
            let merge_id = &build_output.candidate.as_str()[..12];
            let _ = write_validation_artifact(&manifold_dir, merge_id, result);
        }

        // Record validation result in merge-state
        if let Some(result) = validate_outcome.result() {
            record_validation_result(&manifold_dir, result)?;
        }

        match &validate_outcome {
            ValidateOutcome::Skipped => {
                println!("  Validation skipped (no commands configured).");
            }
            ValidateOutcome::Passed(r) => {
                println!(
                    "  Validation passed ({}ms).",
                    r.duration_ms
                );
            }
            ValidateOutcome::PassedWithWarnings(r) => {
                println!(
                    "  WARNING: Validation failed ({}ms) but policy is 'warn' — proceeding.",
                    r.duration_ms
                );
                if !r.stderr.is_empty() {
                    for line in r.stderr.lines().take(5) {
                        eprintln!("    {line}");
                    }
                }
            }
            ValidateOutcome::Blocked(r) => {
                println!(
                    "  Validation FAILED ({}ms) — merge blocked by policy.",
                    r.duration_ms
                );
                if !r.stderr.is_empty() {
                    for line in r.stderr.lines().take(10) {
                        eprintln!("    {line}");
                    }
                }
                abort_merge(&manifold_dir, "validation failed (policy: block)")?;
                bail!(
                    "Merge validation failed. Fix issues and retry.\n  \
                     Diagnostics: .manifold/artifacts/merge/{}/validation.json",
                    &build_output.candidate.as_str()[..12]
                );
            }
            ValidateOutcome::Quarantine(r) => {
                println!(
                    "  WARNING: Validation failed ({}ms) — quarantine created, proceeding.",
                    r.duration_ms
                );
            }
            ValidateOutcome::BlockedAndQuarantine(r) => {
                println!(
                    "  Validation FAILED ({}ms) — merge blocked, quarantine created.",
                    r.duration_ms
                );
                abort_merge(&manifold_dir, "validation failed (policy: block+quarantine)")?;
                bail!(
                    "Merge validation failed. Fix issues and retry.\n  \
                     Diagnostics: .manifold/artifacts/merge/{}/validation.json",
                    &build_output.candidate.as_str()[..12]
                );
            }
        }

        if !validate_outcome.may_proceed() {
            // Already bailed above, but just in case
            bail!("Merge validation blocked the merge.");
        }
    } else {
        println!();
        println!("VALIDATE: No validation commands configured — skipping.");
        advance_merge_state(&manifold_dir, MergePhase::Validate)?;
    }

    // -----------------------------------------------------------------------
    // Phase 4: COMMIT — atomically update refs (point of no return)
    // -----------------------------------------------------------------------
    println!();
    println!("COMMIT: Advancing epoch...");

    // Advance merge-state to Commit phase
    advance_merge_state(&manifold_dir, MergePhase::Commit)?;

    let epoch_before_oid = frozen.epoch.oid().clone();
    match run_commit_phase(&root, branch, &epoch_before_oid, &build_output.candidate) {
        Ok(CommitResult::Committed) => {
            println!(
                "  Epoch advanced: {} → {}",
                &epoch_before_oid.as_str()[..12],
                &build_output.candidate.as_str()[..12]
            );
            println!("  Branch '{branch}' updated.");
        }
        Err(crate::merge::commit::CommitError::PartialCommit) => {
            // Epoch ref moved but branch ref didn't — attempt recovery
            println!("  WARNING: Partial commit — attempting recovery...");
            match recover_partial_commit(&root, branch, &epoch_before_oid, &build_output.candidate) {
                Ok(CommitRecovery::FinalizedMainRef) => {
                    println!("  Recovery succeeded: branch ref finalized.");
                }
                Ok(CommitRecovery::AlreadyCommitted) => {
                    println!("  Recovery: both refs already updated.");
                }
                Ok(CommitRecovery::NotCommitted) => {
                    abort_merge(&manifold_dir, "commit phase failed: neither ref updated")?;
                    bail!("Merge COMMIT phase failed: could not update refs.");
                }
                Err(e) => {
                    bail!(
                        "Merge COMMIT phase partially applied and recovery failed: {e}\n  \
                         Manual recovery: check refs/manifold/epoch/current and refs/heads/{branch}"
                    );
                }
            }
        }
        Err(e) => {
            abort_merge(&manifold_dir, &format!("COMMIT failed: {e}"))?;
            bail!("Merge COMMIT phase failed: {e}");
        }
    }

    // Record epoch_after in merge-state
    record_epoch_after(&manifold_dir, &build_output.candidate)?;

    // -----------------------------------------------------------------------
    // Phase 5: CLEANUP — destroy workspaces (if requested), remove merge-state
    // -----------------------------------------------------------------------
    println!();
    println!("CLEANUP...");

    // Advance merge-state to Cleanup phase
    advance_merge_state(&manifold_dir, MergePhase::Cleanup)?;

    // Update the default workspace to point to the new epoch
    if default_ws_path.exists() {
        update_default_workspace(&default_ws_path, &build_output.candidate)?;
    }

    // Destroy source workspaces if requested
    if destroy_after {
        handle_post_merge_destroy(&ws_to_merge, default_ws, confirm, &backend)?;
    }

    // Remove merge-state file
    let merge_state_path = MergeStateFile::default_path(&manifold_dir);
    let state = MergeStateFile::read(&merge_state_path).unwrap_or_else(|_| {
        MergeStateFile::new(sources, frozen.epoch, now_secs())
    });
    run_cleanup_phase(&state, &merge_state_path, false, |_ws| Ok(()))
        .map_err(|e| anyhow::anyhow!("cleanup failed: {e}"))?;

    // Also clean up the commit-phase state file if present
    let commit_state_path = root.join(".manifold").join("merge-state");
    if commit_state_path.exists() {
        let _ = std::fs::remove_file(&commit_state_path);
    }

    run_hooks(&maw_config.hooks.post_merge, "post-merge", &root, false)?;

    // Generate the merge message for display
    let msg = message.unwrap_or_else(|| {
        if ws_to_merge.len() == 1 {
            "adopt work"
        } else {
            "combine work"
        }
    });

    println!();
    println!("Merged to {branch}: {msg} from {}", ws_to_merge.join(", "));
    println!();
    println!("Next: push to remote:");
    println!("  maw push");

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers: merge-state management
// ---------------------------------------------------------------------------

/// Advance the merge-state file to the next phase.
fn advance_merge_state(manifold_dir: &Path, next_phase: MergePhase) -> Result<()> {
    let state_path = MergeStateFile::default_path(manifold_dir);
    let mut state = MergeStateFile::read(&state_path)
        .map_err(|e| anyhow::anyhow!("read merge-state: {e}"))?;
    state
        .advance(next_phase, now_secs())
        .map_err(|e| anyhow::anyhow!("advance merge-state: {e}"))?;
    state
        .write_atomic(&state_path)
        .map_err(|e| anyhow::anyhow!("write merge-state: {e}"))?;
    Ok(())
}

/// Record the validation result in the merge-state file.
fn record_validation_result(
    manifold_dir: &Path,
    result: &crate::merge_state::ValidationResult,
) -> Result<()> {
    let state_path = MergeStateFile::default_path(manifold_dir);
    let mut state = MergeStateFile::read(&state_path)
        .map_err(|e| anyhow::anyhow!("read merge-state: {e}"))?;
    state.validation_result = Some(result.clone());
    state.updated_at = now_secs();
    state
        .write_atomic(&state_path)
        .map_err(|e| anyhow::anyhow!("write merge-state: {e}"))?;
    Ok(())
}

/// Record the epoch_after in the merge-state file.
fn record_epoch_after(manifold_dir: &Path, candidate: &crate::model::types::GitOid) -> Result<()> {
    let state_path = MergeStateFile::default_path(manifold_dir);
    let mut state = MergeStateFile::read(&state_path)
        .map_err(|e| anyhow::anyhow!("read merge-state: {e}"))?;
    state.epoch_after = Some(
        crate::model::types::EpochId::new(candidate.as_str())
            .map_err(|e| anyhow::anyhow!("invalid candidate OID: {e}"))?
    );
    state.updated_at = now_secs();
    state
        .write_atomic(&state_path)
        .map_err(|e| anyhow::anyhow!("write merge-state: {e}"))?;
    Ok(())
}

/// Abort the merge by writing abort reason and removing merge-state.
fn abort_merge(manifold_dir: &Path, reason: &str) -> Result<()> {
    let state_path = MergeStateFile::default_path(manifold_dir);
    if state_path.exists() {
        if let Ok(mut state) = MergeStateFile::read(&state_path) {
            let _ = state.abort(reason, now_secs());
            let _ = state.write_atomic(&state_path);
        }
        // Clean up the merge-state file
        let _ = std::fs::remove_file(&state_path);
    }
    Ok(())
}

/// Get current Unix timestamp in seconds.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// Helpers: workspace management
// ---------------------------------------------------------------------------

/// Update the default workspace to check out the new epoch commit.
///
/// Uses `git checkout` to update the default workspace's working copy
/// to the new epoch commit.
fn update_default_workspace(
    default_ws_path: &Path,
    new_epoch: &crate::model::types::GitOid,
) -> Result<()> {
    // Reset the worktree to the new epoch
    let output = std::process::Command::new("git")
        .args(["checkout", "--force", new_epoch.as_str()])
        .current_dir(default_ws_path)
        .output()
        .context("Failed to update default workspace")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!(
            "WARNING: Failed to update default workspace to new epoch: {}",
            stderr.trim()
        );
        eprintln!(
            "  Manual fix: cd {} && git checkout {}",
            default_ws_path.display(),
            &new_epoch.as_str()[..12]
        );
    } else {
        println!("  Default workspace updated to new epoch.");
    }

    Ok(())
}

/// Handle post-merge workspace destruction with confirmation check.
fn handle_post_merge_destroy(
    ws_to_merge: &[String],
    default_ws: &str,
    confirm: bool,
    backend: &impl WorkspaceBackend<Error: std::fmt::Display>,
) -> Result<()> {
    let ws_to_destroy: Vec<String> = ws_to_merge
        .iter()
        .filter(|ws| ws.as_str() != default_ws)
        .cloned()
        .collect();

    if confirm {
        println!();
        println!("Will destroy {} workspace(s):", ws_to_destroy.len());
        for ws in &ws_to_destroy {
            println!("  - {ws}");
        }
        println!();
        print!("Continue? [y/N] ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Aborted. Workspaces kept. Merge commit still exists.");
            return Ok(());
        }
    }

    println!("  Cleaning up workspaces...");
    for ws_name in &ws_to_destroy {
        if ws_name == DEFAULT_WORKSPACE {
            println!("    Skipping default workspace");
            continue;
        }
        if let Ok(ws_id) = WorkspaceId::new(ws_name) {
            match backend.destroy(&ws_id) {
                Ok(()) => println!("    Destroyed: {ws_name}"),
                Err(e) => eprintln!("    WARNING: Failed to destroy {ws_name}: {e}"),
            }
        }
    }

    Ok(())
}
