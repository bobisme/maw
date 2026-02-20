use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::backend::WorkspaceBackend;
use crate::config::{ManifoldConfig, MergeDriverKind};
use crate::format::OutputFormat;
use crate::merge::build_phase::{BuildPhaseOutput, run_build_phase};
use crate::merge::collect::collect_snapshots;
use crate::merge::commit::{
    CommitRecovery, CommitResult, recover_partial_commit, run_commit_phase,
};
use crate::merge::partition::partition_by_path;
use crate::merge::plan::{
    DriverInfo, MergePlan, PredictedConflict, ValidationInfo, WorkspaceChange, WorkspaceReport,
    compute_merge_id, write_plan_artifact, write_workspace_report_artifact,
};
use crate::merge::prepare::run_prepare_phase;
use crate::merge::quarantine::create_quarantine_workspace;
use crate::merge::resolve::{ConflictReason, ConflictRecord};
use crate::merge::validate::{ValidateOutcome, run_validate_phase, write_validation_artifact};
use crate::merge_state::{MergePhase, MergeStateFile, run_cleanup_phase};
use crate::model::conflict::ConflictAtom;
use crate::model::conflict::Region;
use crate::model::types::WorkspaceId;

use super::{DEFAULT_WORKSPACE, MawConfig, get_backend, repo_root};

// ---------------------------------------------------------------------------
// JSON output types for agent-friendly conflict presentation
// ---------------------------------------------------------------------------

/// One side of a conflict — which workspace contributed what content.
///
/// Agents use this to understand who made what change and what the content
/// looks like before deciding on a resolution strategy.
#[derive(Debug, Clone, Serialize)]
pub struct ConflictSideJson {
    /// Workspace that produced this side.
    pub workspace: String,
    /// Change kind: "added", "modified", or "deleted".
    pub change: String,
    /// File content from this workspace as a UTF-8 string.
    ///
    /// `None` for deletions or binary files. Check `is_binary` to
    /// distinguish between a deletion and a binary file.
    pub content: Option<String>,
    /// `true` if the content could not be decoded as UTF-8 (binary file).
    pub is_binary: bool,
}

/// Structured conflict information for one file — agent-parseable.
///
/// Each field gives an agent the information needed to understand and resolve
/// the conflict without prior context. The `suggested_resolution` field
/// provides a plain-language description of the recommended approach.
///
/// # Example JSON
///
/// ```json
/// {
///   "type": "content",
///   "path": "src/main.rs",
///   "reason": "content",
///   "reason_description": "overlapping edits (diff3 conflict)",
///   "workspaces": ["alice", "bob"],
///   "base_content": "original content...",
///   "base_is_binary": false,
///   "sides": [
///     { "workspace": "alice", "change": "modified", "content": "...", "is_binary": false },
///     { "workspace": "bob",   "change": "modified", "content": "...", "is_binary": false }
///   ],
///   "atoms": [
///     { "base_region": {"kind": "lines", "start": 10, "end": 20}, "edits": [...], "reason": {...} }
///   ],
///   "resolution_strategies": ["edit_file_manually", "keep_one_side", "combine_changes"],
///   "suggested_resolution": "Edit the file to resolve overlapping changes from each workspace"
/// }
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct ConflictJson {
    /// Conflict type tag: "content", "`add_add`", "`modify_delete`", or "`missing_base`".
    #[serde(rename = "type")]
    pub conflict_type: String,

    /// Path to the conflicted file (relative to repo root).
    pub path: String,

    /// Conflict reason variant name (`snake_case`).
    ///
    /// One of: "content", "`add_add`", "`modify_delete`", "`missing_base`", "`missing_content`".
    pub reason: String,

    /// Human-readable description of why this conflict occurred.
    pub reason_description: String,

    /// All workspace names involved in this conflict.
    pub workspaces: Vec<String>,

    /// The common ancestor (base) content as a UTF-8 string.
    ///
    /// `None` when there is no common base (add/add conflicts) or when the
    /// base content is binary. Check `base_is_binary` to distinguish.
    pub base_content: Option<String>,

    /// `true` if the base content is binary (not representable as UTF-8).
    pub base_is_binary: bool,

    /// Each workspace's contribution to the conflict.
    pub sides: Vec<ConflictSideJson>,

    /// Localized conflict regions within the file.
    ///
    /// Non-empty for content conflicts where region-level analysis was
    /// possible. Each atom identifies the exact region and the divergent edits.
    /// Empty for add/add and modify/delete conflicts.
    pub atoms: Vec<ConflictAtom>,

    /// Suggested resolution strategies for this conflict type.
    ///
    /// Ordered from most to least recommended. Agents should try the first
    /// strategy unless they have specific context suggesting another.
    pub resolution_strategies: Vec<String>,

    /// Plain-language description of the recommended resolution approach.
    pub suggested_resolution: String,
}

/// JSON output when `maw ws merge` succeeds.
#[derive(Debug, Serialize)]
pub struct MergeSuccessOutput {
    /// Always "success".
    pub status: String,
    /// Workspaces that were merged.
    pub workspaces: Vec<String>,
    /// Branch that was updated.
    pub branch: String,
    /// New epoch OID (git commit hash) after the merge.
    pub epoch: String,
    /// Number of unique (non-conflicting) changes applied.
    pub unique_count: usize,
    /// Number of shared paths that were resolved.
    pub shared_count: usize,
    /// Number of shared paths that were auto-resolved.
    pub resolved_count: usize,
    /// Always 0 for a successful merge.
    pub conflict_count: usize,
    /// Always empty for a successful merge.
    pub conflicts: Vec<ConflictJson>,
    /// Human-readable summary.
    pub message: String,
    /// What to do next.
    pub next: String,
}

/// JSON output when `maw ws merge` is blocked by conflicts.
#[derive(Debug, Serialize)]
pub struct MergeConflictOutput {
    /// Always "conflict".
    pub status: String,
    /// Workspaces involved in the failed merge.
    pub workspaces: Vec<String>,
    /// Number of conflicts found.
    pub conflict_count: usize,
    /// Structured conflict details — one entry per conflicted file.
    pub conflicts: Vec<ConflictJson>,
    /// Human-readable error message.
    pub message: String,
    /// Exact command to retry once conflicts are resolved.
    pub to_fix: String,
}

/// JSON output for `maw ws conflicts <workspaces>`.
#[derive(Debug, Serialize)]
pub struct ConflictsOutput {
    /// "conflict" if conflicts found, "clean" if no conflicts.
    pub status: String,
    /// Workspaces checked.
    pub workspaces: Vec<String>,
    /// `true` if any conflicts were found.
    pub has_conflicts: bool,
    /// Number of conflicting files found.
    pub conflict_count: usize,
    /// Structured conflict details — one entry per conflicted file.
    ///
    /// Empty when `has_conflicts` is false.
    pub conflicts: Vec<ConflictJson>,
    /// Human-readable summary.
    pub message: String,
    /// What to do next (only meaningful when `has_conflicts` is true).
    pub to_fix: Option<String>,
}

// ---------------------------------------------------------------------------
// ConflictRecord → ConflictJson conversion
// ---------------------------------------------------------------------------

/// Convert a `ConflictRecord` from the merge engine into a `ConflictJson`
/// suitable for structured JSON output.
///
/// This bridges the internal merge engine representation to the agent-facing
/// JSON format defined in §6.4 of the design doc.
pub fn conflict_record_to_json(record: &ConflictRecord) -> ConflictJson {
    // Map reason to type tag and description
    let (conflict_type, reason_key, resolution_strategies, suggested_resolution) =
        match &record.reason {
            ConflictReason::AddAddDifferent => (
                "add_add",
                "add_add",
                vec![
                    "keep_one_side".to_string(),
                    "merge_content_manually".to_string(),
                ],
                "Review both versions and choose one, or manually combine them into the file. \
                 Both workspaces independently created this file — pick the canonical version \
                 or merge both contributions."
                    .to_string(),
            ),
            ConflictReason::ModifyDelete => (
                "modify_delete",
                "modify_delete",
                vec!["keep_modified".to_string(), "accept_deletion".to_string()],
                "Decide whether to keep the modified version or accept the deletion. \
                 One workspace modified this file while another deleted it — \
                 choose which intent should win."
                    .to_string(),
            ),
            ConflictReason::Diff3Conflict => (
                "content",
                "content",
                vec![
                    "edit_file_manually".to_string(),
                    "keep_one_side".to_string(),
                    "combine_changes".to_string(),
                ],
                "Edit the file to resolve overlapping changes from each workspace. \
                 The `atoms` field identifies the exact lines/regions that conflict — \
                 review each atom and decide how to combine the edits."
                    .to_string(),
            ),
            ConflictReason::MissingBase => (
                "content",
                "missing_base",
                vec!["manual_resolution".to_string(), "keep_one_side".to_string()],
                "Base content is unavailable — inspect each workspace's version and \
                 manually combine them into the desired result."
                    .to_string(),
            ),
            ConflictReason::MissingContent => (
                "content",
                "missing_content",
                vec!["manual_resolution".to_string()],
                "File content is missing from one or more sides — inspect each \
                 workspace's version and manually produce the correct result."
                    .to_string(),
            ),
        };

    // Extract workspace names from sides
    let workspaces: Vec<String> = record
        .sides
        .iter()
        .map(|s| s.workspace_id.as_str().to_string())
        .collect();

    // Convert sides to JSON-friendly form
    let sides: Vec<ConflictSideJson> = record
        .sides
        .iter()
        .map(|s| {
            let (content, is_binary) = s.content.as_ref().map_or((None, false), |bytes| {
                std::str::from_utf8(bytes)
                    .map_or((None, true), |text| (Some(text.to_string()), false))
            });
            ConflictSideJson {
                workspace: s.workspace_id.as_str().to_string(),
                change: s.kind.to_string(),
                content,
                is_binary,
            }
        })
        .collect();

    // Convert base content
    let (base_content, base_is_binary) = record.base.as_ref().map_or((None, false), |bytes| {
        std::str::from_utf8(bytes).map_or((None, true), |text| (Some(text.to_string()), false))
    });

    ConflictJson {
        conflict_type: conflict_type.to_string(),
        path: record.path.display().to_string(),
        reason: reason_key.to_string(),
        reason_description: record.reason.to_string(),
        workspaces,
        base_content,
        base_is_binary,
        sides,
        atoms: record.atoms.clone(),
        resolution_strategies,
        suggested_resolution,
    }
}

// ---------------------------------------------------------------------------
// Conflict reporting
// ---------------------------------------------------------------------------

/// Convert a [`ConflictRecord`] from the merge engine into a [`ConflictInfo`]
/// suitable for JSON output in [`CheckResult`].
///
/// Extracts:
/// - `path`: the conflicting file path.
/// - `reason`: human-readable conflict classification.
/// - `sides`: sorted list of workspace IDs that contributed conflicting edits.
/// - `line_start`/`line_end`: line range from the first diff3 atom, if available.
fn conflict_record_to_info(record: &ConflictRecord) -> ConflictInfo {
    let path = record.path.display().to_string();
    let reason = format!("{}", record.reason);

    let mut sides: Vec<String> = record
        .sides
        .iter()
        .map(|s| s.workspace_id.as_str().to_owned())
        .collect();
    sides.sort();

    // Extract line range from the first ConflictAtom if the base_region is Lines.
    let (line_start, line_end) = record
        .atoms
        .first()
        .and_then(|atom| {
            if let Region::Lines { start, end } = atom.base_region {
                Some((Some(start), Some(end)))
            } else {
                None
            }
        })
        .unwrap_or((None, None));

    ConflictInfo {
        path,
        reason,
        sides,
        line_start,
        line_end,
    }
}

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
    println!("  1. Examine the conflict details above and determine the correct content");
    println!("  2. Edit the conflicted files in {ws_display}/");
    println!("  3. Re-run the merge: maw ws merge <workspace names>");
    println!();
    println!("  Conflicts are reported by the merge engine, not as markers in files.");
    println!("  Each conflict has a reason (divergent edits, add/add, etc.) to guide resolution.");
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

/// Structured information about one conflict detected during a merge check.
///
/// Included in [`CheckResult::conflicts`] for JSON output. Each record
/// identifies the conflicting file, the reason, and the workspace sides
/// that produced incompatible edits. When diff3 atom data is available,
/// `line_start` and `line_end` narrow the conflict to a line range.
#[derive(Debug, Serialize)]
pub struct ConflictInfo {
    /// Path of the conflicting file, relative to the repo root.
    pub path: String,
    /// Human-readable conflict reason (e.g. "overlapping edits (diff3 conflict)").
    pub reason: String,
    /// Workspace IDs that contributed conflicting edits, sorted alphabetically.
    pub sides: Vec<String>,
    /// First line of the conflict region (1-indexed, inclusive), if known.
    pub line_start: Option<u32>,
    /// One past the last line of the conflict region (exclusive), if known.
    pub line_end: Option<u32>,
}

/// Result of a merge check — structured for JSON output.
#[derive(Debug, Serialize)]
pub struct CheckResult {
    pub ready: bool,
    /// Structured conflict records. One entry per conflicting path.
    pub conflicts: Vec<ConflictInfo>,
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
pub fn check_merge(workspaces: &[String], format: OutputFormat) -> Result<()> {
    if workspaces.is_empty() {
        bail!("No workspaces specified for --check");
    }

    let root = repo_root()?;
    let maw_config = MawConfig::load(&root)?;
    let default_ws = maw_config.default_workspace();
    let backend = get_backend()?;

    // Reject merging the default workspace
    if workspaces.iter().any(|ws| ws == default_ws) {
        bail!("Cannot merge the default workspace — it is the merge target, not a source.");
    }

    // Check staleness
    let stale_workspaces = super::check_stale_workspaces()?;
    let is_stale = workspaces.iter().any(|ws| stale_workspaces.contains(ws));

    let primary_ws = &workspaces[0];
    let ws_info = CheckWorkspaceInfo {
        name: primary_ws.clone(),
        change_id: String::new(), // Not surfaced in Manifold check output.
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
    std::fs::create_dir_all(&check_dir).context("Failed to create temp dir for merge check")?;

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
                    let conflicts: Vec<ConflictInfo> = output
                        .conflicts
                        .iter()
                        .map(conflict_record_to_info)
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
                        conflicts: vec![ConflictInfo {
                            path: String::new(),
                            reason: format!("build failed: {e}"),
                            sides: Vec::new(),
                            line_start: None,
                            line_end: None,
                        }],
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
                conflicts: vec![ConflictInfo {
                    path: String::new(),
                    reason: format!("prepare failed: {e}"),
                    sides: Vec::new(),
                    line_start: None,
                    line_end: None,
                }],
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
                println!(
                    "[BLOCKED] Merge would produce {} conflict(s):",
                    result.conflicts.len()
                );
                for c in &result.conflicts {
                    if c.path.is_empty() {
                        println!("  E {}", c.reason);
                    } else {
                        let sides = if c.sides.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", c.sides.join(", "))
                        };
                        let loc = match (c.line_start, c.line_end) {
                            (Some(s), Some(e)) => format!(" lines {s}..{e}"),
                            _ => String::new(),
                        };
                        println!("  C {}{loc}{sides}: {}", c.path, c.reason);
                    }
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
// Plan merge (--plan [--json])
// ---------------------------------------------------------------------------

/// Run the merge pipeline (PREPARE → BUILD → VALIDATE) without committing.
///
/// Produces a deterministic `MergePlan` JSON describing what the merge *would*
/// do. No refs are updated, no epoch is advanced. Artifacts are written to
/// `.manifold/artifacts/`.
pub fn plan_merge(workspaces: &[String], format: OutputFormat) -> Result<()> {
    if workspaces.is_empty() {
        bail!("No workspaces specified for --plan");
    }

    let root = repo_root()?;
    let maw_config = MawConfig::load(&root)?;
    let default_ws = maw_config.default_workspace();
    let backend = get_backend()?;

    if workspaces.iter().any(|ws| ws == default_ws) {
        bail!(
            "Cannot plan a merge of the default workspace — it is the merge target, not a source."
        );
    }

    let manifold_dir = root.join(".manifold");
    let manifold_config = ManifoldConfig::load(&manifold_dir.join("config.toml"))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let sources = parse_workspace_ids(workspaces)?;
    validate_workspace_dirs(&sources, &backend)?;

    // PREPARE → COLLECT → PARTITION → BUILD
    let frozen = run_prepare_phase(
        &root,
        &manifold_dir,
        &sources,
        &workspace_dirs_map(&sources, &backend),
    )
    .map_err(|e| anyhow::anyhow!("PREPARE failed: {e}"))?;
    let patch_sets = collect_snapshots(&root, &backend, &sources)
        .map_err(|e| anyhow::anyhow!("COLLECT failed: {e}"))?;
    let partition = partition_by_path(&patch_sets);
    let (touched_paths, overlaps) = paths_from_partition(&partition);

    let build_output = match run_build_phase(&root, &manifold_dir, &backend) {
        Ok(out) => out,
        Err(e) => {
            let _ = cleanup_plan_merge_state(&manifold_dir);
            bail!("BUILD phase failed: {e}");
        }
    };

    let merge_id = compute_merge_id(&frozen.epoch, &sources, &frozen.heads);
    let driver_infos = build_driver_infos(&touched_paths, &manifold_config);
    let predicted_conflicts = build_predicted_conflicts(&build_output, &partition);
    let validation_info = build_validation_info(&manifold_config);

    // VALIDATE (optional): run and write artifact, but don't block
    plan_run_validation(
        &root,
        &manifold_dir,
        &merge_id,
        &build_output,
        &manifold_config,
    );

    // Clean up merge-state (plan-only: no COMMIT)
    cleanup_plan_merge_state(&manifold_dir)?;

    let plan = MergePlan {
        merge_id,
        epoch_before: frozen.epoch.as_str().to_owned(),
        sources: {
            let mut sorted: Vec<String> = sources.iter().map(|ws| ws.as_str().to_owned()).collect();
            sorted.sort();
            sorted
        },
        touched_paths,
        overlaps,
        predicted_conflicts,
        drivers: driver_infos,
        validation: validation_info,
    };

    // Write artifacts (non-fatal on failure)
    plan_write_artifacts(&manifold_dir, &plan, &patch_sets, &frozen, format);

    // Output
    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&plan)
                    .map_err(|e| anyhow::anyhow!("serialize plan: {e}"))?
            );
        }
        _ => print_plan_text(&plan),
    }

    Ok(())
}

/// Parse workspace name strings into `WorkspaceId` values.
fn parse_workspace_ids(workspaces: &[String]) -> Result<Vec<WorkspaceId>> {
    workspaces
        .iter()
        .map(|ws| {
            WorkspaceId::new(ws).map_err(|e| anyhow::anyhow!("invalid workspace name '{ws}': {e}"))
        })
        .collect()
}

/// Verify all source workspaces exist on disk.
fn validate_workspace_dirs<B: WorkspaceBackend>(sources: &[WorkspaceId], backend: &B) -> Result<()>
where
    B::Error: std::fmt::Display,
{
    for ws_id in sources {
        let ws_path = backend.workspace_path(ws_id);
        if !ws_path.exists() {
            bail!(
                "Workspace '{}' does not exist at {}\n  \
                 Check available workspaces: maw ws list",
                ws_id,
                ws_path.display()
            );
        }
    }
    Ok(())
}

/// Build a `BTreeMap<WorkspaceId, PathBuf>` of workspace directories.
fn workspace_dirs_map<B: WorkspaceBackend>(
    sources: &[WorkspaceId],
    backend: &B,
) -> BTreeMap<WorkspaceId, PathBuf> {
    sources
        .iter()
        .map(|ws_id| (ws_id.clone(), backend.workspace_path(ws_id)))
        .collect()
}

/// Extract sorted touched paths and overlaps from a partition.
fn paths_from_partition(
    partition: &crate::merge::partition::PartitionResult,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut touched: Vec<PathBuf> = partition
        .unique
        .iter()
        .map(|(p, _)| p.clone())
        .chain(partition.shared.iter().map(|(p, _)| p.clone()))
        .collect();
    touched.sort();
    touched.dedup();

    let overlaps: Vec<PathBuf> = partition.shared.iter().map(|(p, _)| p.clone()).collect();
    (touched, overlaps)
}

/// Build `DriverInfo` entries for each touched path that has a matching driver.
fn build_driver_infos(touched_paths: &[PathBuf], config: &ManifoldConfig) -> Vec<DriverInfo> {
    let effective_drivers = config.merge.effective_drivers();
    let mut infos = Vec::new();
    for path in touched_paths {
        for driver in &effective_drivers {
            let matches = glob::Pattern::new(&driver.match_glob)
                .ok()
                .is_some_and(|p| p.matches_path(path));
            if matches {
                let command = matches!(driver.kind, MergeDriverKind::Regenerate)
                    .then(|| driver.command.clone())
                    .flatten();
                infos.push(DriverInfo {
                    path: path.clone(),
                    kind: driver.kind.to_string(),
                    command,
                });
                break; // First matching driver wins
            }
        }
    }
    infos
}

/// Build `PredictedConflict` entries from the BUILD output.
fn build_predicted_conflicts(
    build_output: &BuildPhaseOutput,
    partition: &crate::merge::partition::PartitionResult,
) -> Vec<PredictedConflict> {
    build_output
        .conflicts
        .iter()
        .map(|conflict| {
            let sides: Vec<String> = partition
                .shared
                .iter()
                .find(|(p, _)| p == &conflict.path)
                .map(|(_, entries)| {
                    entries
                        .iter()
                        .map(|e| e.workspace_id.as_str().to_owned())
                        .collect()
                })
                .unwrap_or_default();
            PredictedConflict {
                path: conflict.path.clone(),
                kind: conflict.reason.to_string(),
                sides,
            }
        })
        .collect()
}

/// Build `ValidationInfo` from config (returns `None` if no commands configured).
fn build_validation_info(config: &ManifoldConfig) -> Option<ValidationInfo> {
    let vc = &config.merge.validation;
    if !vc.has_commands() {
        return None;
    }
    Some(ValidationInfo {
        commands: vc
            .effective_commands()
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
        timeout_seconds: vc.timeout_seconds,
        policy: vc.on_failure.to_string(),
    })
}

/// Run validation in plan mode: write artifact but never block.
fn plan_run_validation(
    root: &Path,
    manifold_dir: &Path,
    merge_id: &str,
    build_output: &BuildPhaseOutput,
    config: &ManifoldConfig,
) {
    let vc = &config.merge.validation;
    if !vc.has_commands() {
        return;
    }
    match run_validate_phase(root, &build_output.candidate, vc) {
        Ok(outcome) => {
            if let Some(result) = outcome.result() {
                let _ = write_validation_artifact(manifold_dir, merge_id, result);
            }
            if !outcome.may_proceed() {
                eprintln!(
                    "  WARNING: Validation would fail — merge would be blocked by policy '{}'",
                    vc.on_failure
                );
            }
        }
        Err(e) => eprintln!("  WARNING: Validation failed to run: {e}"),
    }
}

/// Write plan.json and per-workspace report.json artifacts (non-fatal on failure).
fn plan_write_artifacts(
    manifold_dir: &Path,
    plan: &MergePlan,
    patch_sets: &[crate::merge::types::PatchSet],
    frozen: &crate::merge::prepare::FrozenInputs,
    format: OutputFormat,
) {
    match write_plan_artifact(manifold_dir, plan) {
        Ok(path) => {
            if !matches!(format, OutputFormat::Json) {
                println!("Plan artifact: {}", path.display());
            }
        }
        Err(e) => eprintln!("WARNING: Failed to write plan artifact: {e}"),
    }

    for patch_set in patch_sets {
        let changes: Vec<WorkspaceChange> = patch_set
            .changes
            .iter()
            .map(|c| WorkspaceChange {
                path: c.path.clone(),
                kind: c.kind.to_string(),
            })
            .collect();
        let head = frozen
            .heads
            .get(&patch_set.workspace_id)
            .map(|oid| oid.as_str().to_owned())
            .unwrap_or_default();
        let report = WorkspaceReport {
            workspace_id: patch_set.workspace_id.as_str().to_owned(),
            head,
            changes,
        };
        if let Err(e) = write_workspace_report_artifact(manifold_dir, &report) {
            eprintln!(
                "WARNING: Failed to write workspace report for {}: {e}",
                patch_set.workspace_id
            );
        }
    }
}

/// Print a human-readable plan summary.
fn print_plan_text(plan: &MergePlan) {
    println!("=== Merge Plan (dry run — no commits) ===");
    println!();
    println!("Merge ID:    {}", &plan.merge_id[..16]);
    println!("Epoch:       {}", &plan.epoch_before[..12]);
    println!("Sources:     {}", plan.sources.join(", "));
    println!(
        "Touched:     {} path(s), {} overlap(s)",
        plan.touched_paths.len(),
        plan.overlaps.len()
    );

    if !plan.overlaps.is_empty() {
        println!();
        println!("Overlapping paths (modified in multiple workspaces):");
        for path in &plan.overlaps {
            println!("  ~ {}", path.display());
        }
    }

    if !plan.predicted_conflicts.is_empty() {
        println!();
        println!("Predicted conflicts ({}):", plan.predicted_conflicts.len());
        for conflict in &plan.predicted_conflicts {
            let sides = conflict.sides.join(", ");
            println!(
                "  C {} — {} (sides: {})",
                conflict.path.display(),
                conflict.kind,
                sides
            );
        }
    } else if !plan.overlaps.is_empty() {
        println!();
        println!("  (all overlapping paths resolved cleanly via diff3 or drivers)");
    }

    if !plan.drivers.is_empty() {
        println!();
        println!("Merge drivers:");
        for driver in &plan.drivers {
            if let Some(cmd) = &driver.command {
                println!(
                    "  {} — {} (command: {})",
                    driver.path.display(),
                    driver.kind,
                    cmd
                );
            } else {
                println!("  {} — {}", driver.path.display(), driver.kind);
            }
        }
    }

    if let Some(val) = &plan.validation {
        println!();
        println!("Validation:");
        for cmd in &val.commands {
            println!("  $ {cmd}");
        }
        println!(
            "  Timeout: {}s, Policy: {}",
            val.timeout_seconds, val.policy
        );
    }

    println!();
    if plan.predicted_conflicts.is_empty() {
        println!("[OK] Merge would succeed with no conflicts.");
    } else {
        println!(
            "[BLOCKED] Merge would have {} unresolved conflict(s). Resolve before merging.",
            plan.predicted_conflicts.len()
        );
    }
}

/// Remove the merge-state file created during a plan run.
///
/// In plan mode, we create a merge-state file during PREPARE/BUILD but never
/// advance to COMMIT — so we clean it up manually at the end.
fn cleanup_plan_merge_state(manifold_dir: &Path) -> Result<()> {
    let state_path = MergeStateFile::default_path(manifold_dir);
    if state_path.exists() {
        std::fs::remove_file(&state_path)
            .map_err(|e| anyhow::anyhow!("failed to remove plan merge-state: {e}"))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Conflict inspection (maw ws conflicts)
// ---------------------------------------------------------------------------

/// Show detailed conflict information for the given workspaces.
///
/// Runs PREPARE + BUILD to detect conflicts and outputs structured data.
/// Unlike `check_merge`, this always shows rich conflict details (not just
/// file names) and targets JSON output for agents.
///
/// Text output lists each conflict with its reason and per-workspace sides.
/// JSON output is a [`ConflictsOutput`] value — fully parseable by agents.
#[allow(clippy::too_many_lines)]
pub fn show_conflicts(workspaces: &[String], format: OutputFormat) -> Result<()> {
    if workspaces.is_empty() {
        bail!("No workspaces specified");
    }

    let root = repo_root()?;
    let maw_config = MawConfig::load(&root)?;
    let default_ws = maw_config.default_workspace();
    let backend = get_backend()?;

    if workspaces.iter().any(|ws| ws == default_ws) {
        bail!(
            "Cannot inspect conflicts for the default workspace — \
             specify source workspace names instead."
        );
    }

    // Run PREPARE + BUILD in a temp dir to detect conflicts without committing
    let manifold_dir = root.join(".manifold");
    let check_dir = manifold_dir.join("conflicts-tmp");
    let _ = std::fs::remove_dir_all(&check_dir);
    std::fs::create_dir_all(&check_dir).context("Failed to create temp dir for conflict check")?;

    let sources: Vec<WorkspaceId> = workspaces
        .iter()
        .map(|ws| WorkspaceId::new(ws).map_err(|e| anyhow::anyhow!("{e}")))
        .collect::<Result<Vec<_>>>()?;

    let mut workspace_dirs = BTreeMap::new();
    for ws_id in &sources {
        workspace_dirs.insert(ws_id.clone(), backend.workspace_path(ws_id));
    }

    // PREPARE
    let prepare_result = run_prepare_phase(&root, &check_dir, &sources, &workspace_dirs);
    let _ = std::fs::remove_dir_all(&check_dir);

    let build_output = match prepare_result {
        Err(e) => {
            // Output the error in the requested format
            let msg = format!("prepare phase failed: {e}");
            if format == OutputFormat::Json {
                let out = ConflictsOutput {
                    status: "error".to_string(),
                    workspaces: workspaces.to_vec(),
                    has_conflicts: false,
                    conflict_count: 0,
                    conflicts: vec![],
                    message: msg.clone(),
                    to_fix: None,
                };
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                eprintln!("Error: {msg}");
            }
            bail!("{msg}");
        }
        Ok(_frozen) => {
            // Re-run PREPARE + BUILD
            std::fs::create_dir_all(&check_dir)
                .context("Failed to create temp dir for build phase")?;
            let _ = run_prepare_phase(&root, &check_dir, &sources, &workspace_dirs);
            let result = run_build_phase(&root, &check_dir, &backend);
            let _ = std::fs::remove_dir_all(&check_dir);
            match result {
                Ok(out) => out,
                Err(e) => {
                    let msg = format!("build phase failed: {e}");
                    if format == OutputFormat::Json {
                        let out = ConflictsOutput {
                            status: "error".to_string(),
                            workspaces: workspaces.to_vec(),
                            has_conflicts: false,
                            conflict_count: 0,
                            conflicts: vec![],
                            message: msg.clone(),
                            to_fix: None,
                        };
                        println!("{}", serde_json::to_string_pretty(&out)?);
                    } else {
                        eprintln!("Error: {msg}");
                    }
                    bail!("{msg}");
                }
            }
        }
    };

    let conflict_jsons: Vec<ConflictJson> = build_output
        .conflicts
        .iter()
        .map(conflict_record_to_json)
        .collect();

    let has_conflicts = !conflict_jsons.is_empty();
    let to_fix = if has_conflicts {
        Some(format!("maw ws merge {}", workspaces.join(" ")))
    } else {
        None
    };

    if format == OutputFormat::Json {
        let status = if has_conflicts { "conflict" } else { "clean" }.to_string();
        let message = if has_conflicts {
            format!(
                "{} conflict(s) found in {} workspace(s). Resolve them before merging.",
                conflict_jsons.len(),
                workspaces.len()
            )
        } else {
            format!(
                "No conflicts found. {} workspace(s) can be merged cleanly.",
                workspaces.len()
            )
        };
        let out = ConflictsOutput {
            status,
            workspaces: workspaces.to_vec(),
            has_conflicts,
            conflict_count: conflict_jsons.len(),
            conflicts: conflict_jsons,
            message,
            to_fix,
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        // Text / pretty output
        if has_conflicts {
            println!(
                "{} conflict(s) found across {} workspace(s):",
                conflict_jsons.len(),
                workspaces.len()
            );
            println!();
            for (i, c) in conflict_jsons.iter().enumerate() {
                println!(
                    "  [{}/{}] {} ({}: {})",
                    i + 1,
                    conflict_jsons.len(),
                    c.path,
                    c.reason,
                    c.reason_description
                );
                println!("    Workspaces: {}", c.workspaces.join(", "));
                if !c.atoms.is_empty() {
                    println!("    Conflict regions ({} atom(s)):", c.atoms.len());
                    for atom in &c.atoms {
                        println!("      {}", atom.summary());
                    }
                }
                println!("    Resolution: {}", c.suggested_resolution);
                println!();
            }
            println!("To merge: maw ws merge {}", workspaces.join(" "));
            println!(
                "For JSON: maw ws conflicts {} --format json",
                workspaces.join(" ")
            );
        } else {
            println!("No conflicts found.");
            println!(
                "{} workspace(s) can be merged cleanly: {}",
                workspaces.len(),
                workspaces.join(", ")
            );
            println!("To merge: maw ws merge {}", workspaces.join(" "));
        }
    }

    Ok(())
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
            if let Ok(ws_id) = WorkspaceId::new(ws_name)
                && let Ok(snapshot) = backend.snapshot(&ws_id) {
                    let files: Vec<PathBuf> = snapshot.all_changed().into_iter().cloned().collect();
                    workspace_files.push((ws_name.clone(), files));
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
    /// Output format. `OutputFormat::Json` emits structured JSON for the
    /// conflict report and success summary.
    pub format: OutputFormat,
}

// ---------------------------------------------------------------------------
// Main merge function
// ---------------------------------------------------------------------------

/// Run the merge state machine: PREPARE → BUILD → VALIDATE → COMMIT → CLEANUP.
///
/// This uses the Manifold merge engine and state machine.
#[allow(clippy::too_many_lines)]
pub fn merge(workspaces: &[String], opts: &MergeOptions<'_>) -> Result<()> {
    let MergeOptions {
        destroy_after,
        confirm,
        message,
        dry_run,
        format,
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
            WorkspaceId::new(ws).map_err(|e| anyhow::anyhow!("invalid workspace name '{ws}': {e}"))
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
    println!("  Epoch: {}", &frozen.epoch.as_str()[..12]);
    for (ws_id, head) in &frozen.heads {
        println!("  {}: {}", ws_id, &head.as_str()[..12]);
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
            abort_merge(&manifold_dir, &format!("BUILD failed: {e}"));
            bail!("Merge BUILD phase failed: {e}");
        }
    };

    println!(
        "  {} unique path(s), {} shared path(s), {} resolved",
        build_output.unique_count, build_output.shared_count, build_output.resolved_count
    );
    println!("  Candidate: {}", &build_output.candidate.as_str()[..12]);

    // Check for unresolved conflicts
    if !build_output.conflicts.is_empty() {
        // Abort the merge — conflicts must be resolved first
        abort_merge(&manifold_dir, "unresolved conflicts");

        if format == OutputFormat::Json {
            // Emit structured JSON conflict output for agents
            let conflict_jsons: Vec<ConflictJson> = build_output
                .conflicts
                .iter()
                .map(conflict_record_to_json)
                .collect();
            let to_fix = format!("maw ws merge {}", ws_to_merge.join(" "));
            let output = MergeConflictOutput {
                status: "conflict".to_string(),
                workspaces: ws_to_merge,
                conflict_count: conflict_jsons.len(),
                conflicts: conflict_jsons,
                message: format!(
                    "Merge has {} unresolved conflict(s). Resolve them and retry.",
                    build_output.conflicts.len()
                ),
                to_fix,
            };
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            println!("  {} unresolved conflict(s)", build_output.conflicts.len());
            print_conflict_report(&build_output.conflicts, &default_ws_path, default_ws);

            if destroy_after {
                println!();
                println!("NOT destroying workspaces due to conflicts.");
                println!("Resolve conflicts in the source workspaces, then retry:");
                println!("  maw ws merge {}", ws_to_merge.join(" "));
            }
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

    println!();
    if validation_config.has_commands() {
        println!("VALIDATE: Running post-merge validation...");

        // Advance merge-state to Validate phase
        advance_merge_state(&manifold_dir, MergePhase::Validate)?;

        let validate_outcome =
            match run_validate_phase(&root, &build_output.candidate, validation_config) {
                Ok(outcome) => outcome,
                Err(e) => {
                    abort_merge(&manifold_dir, &format!("VALIDATE error: {e}"));
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
                println!("  Validation passed ({}ms).", r.duration_ms);
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
                abort_merge(&manifold_dir, "validation failed (policy: block)");
                bail!(
                    "Merge validation failed. Fix issues and retry.\n  \
                     Diagnostics: .manifold/artifacts/merge/{}/validation.json",
                    &build_output.candidate.as_str()[..12]
                );
            }
            ValidateOutcome::Quarantine(r) | ValidateOutcome::BlockedAndQuarantine(r) => {
                // Validation failed with quarantine policy — materialize the candidate
                // merge tree into a quarantine workspace so the agent can fix forward.
                let merge_id = &build_output.candidate.as_str()[..12];
                let is_blocked = !validate_outcome.may_proceed();
                let policy_name = if is_blocked {
                    "block+quarantine"
                } else {
                    "quarantine"
                };

                println!(
                    "  Validation FAILED ({}ms) — policy '{policy_name}', creating quarantine workspace...",
                    r.duration_ms
                );

                // Abort merge-state (quarantine is the fix-forward path, not epoch advance)
                abort_merge(
                    &manifold_dir,
                    &format!("validation failed (policy: {policy_name})"),
                );

                match create_quarantine_workspace(
                    &root,
                    &manifold_dir,
                    merge_id,
                    sources,
                    &frozen.epoch,
                    build_output.candidate.clone(),
                    branch,
                    r.clone(),
                ) {
                    Ok(qws_path) => {
                        println!("  Quarantine workspace created: {}", qws_path.display());
                        println!();
                        if !r.stderr.is_empty() {
                            println!("  Validation output:");
                            for line in r.stderr.lines().take(10) {
                                eprintln!("    {line}");
                            }
                        }
                        println!();
                        println!("Fix the issues in the quarantine workspace:");
                        println!("  Edit files: {}/", qws_path.display());
                        println!("  Re-validate and commit: maw merge promote {merge_id}");
                        println!("  Discard quarantine:     maw merge abandon {merge_id}");
                        println!();
                        println!("Source workspaces are preserved (not destroyed).");
                        bail!(
                            "Merge validation failed (policy: {policy_name}).\n  \
                             Quarantine workspace: {}\n  \
                             Diagnostics: .manifold/quarantine/{merge_id}/validation.json\n  \
                             To promote: maw merge promote {merge_id}\n  \
                             To abandon: maw merge abandon {merge_id}",
                            qws_path.display()
                        );
                    }
                    Err(e) => {
                        eprintln!("  WARNING: Failed to create quarantine workspace: {e}");
                        bail!(
                            "Merge validation failed (policy: {policy_name}) and quarantine creation failed: {e}\n  \
                             Diagnostics: .manifold/artifacts/merge/{merge_id}/validation.json"
                        );
                    }
                }
            }
        }

        if !validate_outcome.may_proceed() {
            // Already bailed above for Blocked/BlockedAndQuarantine/Quarantine,
            // but kept as a safety net.
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
            match recover_partial_commit(&root, branch, &epoch_before_oid, &build_output.candidate)
            {
                Ok(CommitRecovery::FinalizedMainRef) => {
                    println!("  Recovery succeeded: branch ref finalized.");
                }
                Ok(CommitRecovery::AlreadyCommitted) => {
                    println!("  Recovery: both refs already updated.");
                }
                Ok(CommitRecovery::NotCommitted) => {
                    abort_merge(&manifold_dir, "commit phase failed: neither ref updated");
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
            abort_merge(&manifold_dir, &format!("COMMIT failed: {e}"));
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
    let state = MergeStateFile::read(&merge_state_path)
        .unwrap_or_else(|_| MergeStateFile::new(sources, frozen.epoch, now_secs()));
    run_cleanup_phase(&state, &merge_state_path, false, |_ws| Ok(()))
        .map_err(|e| anyhow::anyhow!("cleanup failed: {e}"))?;

    // Also clean up commit-phase sidecar state files if present.
    // `commit-state.json` is current; `merge-state` is a legacy fallback.
    let commit_state_path = root.join(".manifold").join("commit-state.json");
    if commit_state_path.exists() {
        let _ = std::fs::remove_file(&commit_state_path);
    }
    let legacy_commit_state_path = root.join(".manifold").join("merge-state");
    if legacy_commit_state_path.exists() {
        let _ = std::fs::remove_file(&legacy_commit_state_path);
    }

    run_hooks(&maw_config.hooks.post_merge, "post-merge", &root, false)?;

    // Generate the merge message for display
    let msg = message.unwrap_or({
        if ws_to_merge.len() == 1 {
            "adopt work"
        } else {
            "combine work"
        }
    });

    if format == OutputFormat::Json {
        let success = MergeSuccessOutput {
            status: "success".to_string(),
            workspaces: ws_to_merge.clone(),
            branch: branch.to_string(),
            epoch: build_output.candidate.as_str().to_string(),
            unique_count: build_output.unique_count,
            shared_count: build_output.shared_count,
            resolved_count: build_output.resolved_count,
            conflict_count: 0,
            conflicts: vec![],
            message: format!("Merged to {branch}: {msg} from {}", ws_to_merge.join(", ")),
            next: "maw push".to_string(),
        };
        println!("{}", serde_json::to_string_pretty(&success)?);
    } else {
        println!();
        println!("Merged to {branch}: {msg} from {}", ws_to_merge.join(", "));
        println!();
        println!("Next: push to remote:");
        println!("  maw push");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers: merge-state management
// ---------------------------------------------------------------------------

/// Advance the merge-state file to the next phase.
fn advance_merge_state(manifold_dir: &Path, next_phase: MergePhase) -> Result<()> {
    let state_path = MergeStateFile::default_path(manifold_dir);
    let mut state =
        MergeStateFile::read(&state_path).map_err(|e| anyhow::anyhow!("read merge-state: {e}"))?;
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
    let mut state =
        MergeStateFile::read(&state_path).map_err(|e| anyhow::anyhow!("read merge-state: {e}"))?;
    state.validation_result = Some(result.clone());
    state.updated_at = now_secs();
    state
        .write_atomic(&state_path)
        .map_err(|e| anyhow::anyhow!("write merge-state: {e}"))?;
    Ok(())
}

/// Record the `epoch_after` in the merge-state file.
fn record_epoch_after(manifold_dir: &Path, candidate: &crate::model::types::GitOid) -> Result<()> {
    let state_path = MergeStateFile::default_path(manifold_dir);
    let mut state =
        MergeStateFile::read(&state_path).map_err(|e| anyhow::anyhow!("read merge-state: {e}"))?;
    state.epoch_after = Some(
        crate::model::types::EpochId::new(candidate.as_str())
            .map_err(|e| anyhow::anyhow!("invalid candidate OID: {e}"))?,
    );
    state.updated_at = now_secs();
    state
        .write_atomic(&state_path)
        .map_err(|e| anyhow::anyhow!("write merge-state: {e}"))?;
    Ok(())
}

/// Abort the merge by writing abort reason and removing merge-state.
fn abort_merge(manifold_dir: &Path, reason: &str) {
    let state_path = MergeStateFile::default_path(manifold_dir);
    if state_path.exists() {
        if let Ok(mut state) = MergeStateFile::read(&state_path) {
            let _ = state.abort(reason, now_secs());
            let _ = state.write_atomic(&state_path);
        }
        // Clean up the merge-state file
        let _ = std::fs::remove_file(&state_path);
    }
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

    if output.status.success() {
        println!("  Default workspace updated to new epoch.");
    } else {
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

// ---------------------------------------------------------------------------
// Tests — JSON conflict output
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::merge::resolve::{ConflictReason, ConflictRecord, ConflictSide as ResolveSide};
    use crate::merge::types::ChangeKind;
    use crate::model::conflict::{AtomEdit, ConflictAtom, Region};
    use crate::model::types::WorkspaceId;

    use super::*;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn ws_id(name: &str) -> WorkspaceId {
        WorkspaceId::new(name).unwrap()
    }

    fn make_side(workspace: &str, kind: ChangeKind, content: Option<Vec<u8>>) -> ResolveSide {
        ResolveSide {
            workspace_id: ws_id(workspace),
            kind,
            content,
        }
    }

    fn content_record(
        path: &str,
        base: &str,
        alice_content: &str,
        bob_content: &str,
    ) -> ConflictRecord {
        ConflictRecord {
            path: PathBuf::from(path),
            base: Some(base.as_bytes().to_vec()),
            sides: vec![
                make_side(
                    "alice",
                    ChangeKind::Modified,
                    Some(alice_content.as_bytes().to_vec()),
                ),
                make_side(
                    "bob",
                    ChangeKind::Modified,
                    Some(bob_content.as_bytes().to_vec()),
                ),
            ],
            reason: ConflictReason::Diff3Conflict,
            atoms: vec![ConflictAtom::line_overlap(
                10,
                15,
                vec![
                    AtomEdit::new("alice", Region::lines(10, 13), "alice's lines"),
                    AtomEdit::new("bob", Region::lines(12, 15), "bob's lines"),
                ],
                "lines 10-15 overlap",
            )],
        }
    }

    fn add_add_record(path: &str) -> ConflictRecord {
        ConflictRecord {
            path: PathBuf::from(path),
            base: None,
            sides: vec![
                make_side(
                    "alice",
                    ChangeKind::Added,
                    Some(b"alice's version".to_vec()),
                ),
                make_side("bob", ChangeKind::Added, Some(b"bob's version".to_vec())),
            ],
            reason: ConflictReason::AddAddDifferent,
            atoms: vec![],
        }
    }

    fn modify_delete_record(path: &str) -> ConflictRecord {
        ConflictRecord {
            path: PathBuf::from(path),
            base: Some(b"original content".to_vec()),
            sides: vec![
                make_side(
                    "alice",
                    ChangeKind::Modified,
                    Some(b"modified content".to_vec()),
                ),
                make_side("bob", ChangeKind::Deleted, None),
            ],
            reason: ConflictReason::ModifyDelete,
            atoms: vec![],
        }
    }

    fn missing_base_record(path: &str) -> ConflictRecord {
        ConflictRecord {
            path: PathBuf::from(path),
            base: None,
            sides: vec![
                make_side(
                    "alice",
                    ChangeKind::Modified,
                    Some(b"alice content".to_vec()),
                ),
                make_side("bob", ChangeKind::Modified, Some(b"bob content".to_vec())),
            ],
            reason: ConflictReason::MissingBase,
            atoms: vec![],
        }
    }

    // -----------------------------------------------------------------------
    // conflict_record_to_json: content conflict (Diff3Conflict)
    // -----------------------------------------------------------------------

    #[test]
    fn content_conflict_json_has_correct_type_and_reason() {
        let record = content_record("src/main.rs", "base", "alice version", "bob version");
        let json = conflict_record_to_json(&record);

        assert_eq!(json.conflict_type, "content");
        assert_eq!(json.reason, "content");
        assert!(!json.reason_description.is_empty());
        assert_eq!(json.path, "src/main.rs");
    }

    #[test]
    fn content_conflict_json_workspace_attribution() {
        let record = content_record("main.rs", "base", "alice version", "bob version");
        let json = conflict_record_to_json(&record);

        // Workspaces should be attributed clearly
        assert_eq!(json.workspaces, vec!["alice", "bob"]);

        // Each side should carry workspace name and change kind
        assert_eq!(json.sides.len(), 2);
        let alice_side = json.sides.iter().find(|s| s.workspace == "alice").unwrap();
        let bob_side = json.sides.iter().find(|s| s.workspace == "bob").unwrap();

        assert_eq!(alice_side.change, "modified");
        assert_eq!(alice_side.content.as_deref(), Some("alice version"));
        assert!(!alice_side.is_binary);

        assert_eq!(bob_side.change, "modified");
        assert_eq!(bob_side.content.as_deref(), Some("bob version"));
        assert!(!bob_side.is_binary);
    }

    #[test]
    fn content_conflict_json_base_content() {
        let record = content_record("lib.rs", "original content here", "alice", "bob");
        let json = conflict_record_to_json(&record);

        // Base content should be included for context
        assert_eq!(json.base_content.as_deref(), Some("original content here"));
        assert!(!json.base_is_binary);
    }

    #[test]
    fn content_conflict_json_has_atoms() {
        let record = content_record("src/lib.rs", "base", "alice", "bob");
        let json = conflict_record_to_json(&record);

        // Atoms localize the conflict to specific regions
        assert!(!json.atoms.is_empty(), "content conflict should have atoms");
        let atom = &json.atoms[0];
        assert_eq!(atom.base_region, Region::lines(10, 15));
        assert_eq!(atom.edits.len(), 2);
    }

    #[test]
    fn content_conflict_json_has_resolution_strategies() {
        let record = content_record("src/lib.rs", "base", "alice", "bob");
        let json = conflict_record_to_json(&record);

        // Resolution strategies help agents decide what to do
        assert!(!json.resolution_strategies.is_empty());
        assert!(
            json.resolution_strategies
                .contains(&"edit_file_manually".to_string())
        );
        assert!(!json.suggested_resolution.is_empty());
    }

    // -----------------------------------------------------------------------
    // conflict_record_to_json: add/add conflict
    // -----------------------------------------------------------------------

    #[test]
    fn add_add_conflict_json_has_correct_type() {
        let record = add_add_record("src/new.rs");
        let json = conflict_record_to_json(&record);

        assert_eq!(json.conflict_type, "add_add");
        assert_eq!(json.reason, "add_add");
        assert_eq!(json.path, "src/new.rs");
    }

    #[test]
    fn add_add_conflict_json_no_base() {
        let record = add_add_record("new.rs");
        let json = conflict_record_to_json(&record);

        // No base content for add/add (file didn't exist before)
        assert!(json.base_content.is_none());
        assert!(!json.base_is_binary);
    }

    #[test]
    fn add_add_conflict_json_both_sides_present() {
        let record = add_add_record("util.rs");
        let json = conflict_record_to_json(&record);

        assert_eq!(json.sides.len(), 2);
        let alice = json.sides.iter().find(|s| s.workspace == "alice").unwrap();
        let bob = json.sides.iter().find(|s| s.workspace == "bob").unwrap();

        assert_eq!(alice.change, "added");
        assert_eq!(alice.content.as_deref(), Some("alice's version"));
        assert_eq!(bob.change, "added");
        assert_eq!(bob.content.as_deref(), Some("bob's version"));
    }

    #[test]
    fn add_add_conflict_json_resolution_strategies() {
        let record = add_add_record("new.rs");
        let json = conflict_record_to_json(&record);

        assert!(
            json.resolution_strategies
                .contains(&"keep_one_side".to_string())
        );
        assert!(
            json.resolution_strategies
                .contains(&"merge_content_manually".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // conflict_record_to_json: modify/delete conflict
    // -----------------------------------------------------------------------

    #[test]
    fn modify_delete_conflict_json_has_correct_type() {
        let record = modify_delete_record("src/old.rs");
        let json = conflict_record_to_json(&record);

        assert_eq!(json.conflict_type, "modify_delete");
        assert_eq!(json.reason, "modify_delete");
    }

    #[test]
    fn modify_delete_conflict_json_deletion_side_has_no_content() {
        let record = modify_delete_record("old.rs");
        let json = conflict_record_to_json(&record);

        let bob_side = json.sides.iter().find(|s| s.workspace == "bob").unwrap();
        assert_eq!(bob_side.change, "deleted");
        assert!(bob_side.content.is_none());
        assert!(!bob_side.is_binary);
    }

    #[test]
    fn modify_delete_conflict_json_resolution_strategies() {
        let record = modify_delete_record("old.rs");
        let json = conflict_record_to_json(&record);

        assert!(
            json.resolution_strategies
                .contains(&"keep_modified".to_string())
        );
        assert!(
            json.resolution_strategies
                .contains(&"accept_deletion".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // conflict_record_to_json: binary content
    // -----------------------------------------------------------------------

    #[test]
    fn binary_content_side_is_flagged() {
        let mut record = content_record("image.png", "base", "alice", "bob");
        // Replace alice's content with non-UTF-8 bytes
        record.sides[0].content = Some(vec![0xFF, 0xFE, 0x00, 0x01]);
        record.base = Some(vec![0xFF, 0xD8, 0xFF, 0xE0]); // JPEG magic bytes

        let json = conflict_record_to_json(&record);

        let alice_side = json.sides.iter().find(|s| s.workspace == "alice").unwrap();
        assert!(alice_side.is_binary, "binary content should be flagged");
        assert!(
            alice_side.content.is_none(),
            "binary content should not be included as text"
        );

        // Base content should also be marked binary
        assert!(json.base_is_binary);
        assert!(json.base_content.is_none());
    }

    // -----------------------------------------------------------------------
    // JSON roundtrip: agent can parse the output
    // -----------------------------------------------------------------------

    #[test]
    fn conflict_json_roundtrip() {
        let record = content_record("src/lib.rs", "base content", "alice edit", "bob edit");
        let conflict_json = conflict_record_to_json(&record);

        // Serialize to JSON
        let json_str = serde_json::to_string_pretty(&conflict_json).unwrap();
        assert!(!json_str.is_empty());

        // Verify key fields are present in the JSON string
        assert!(json_str.contains("\"type\""), "type field must be present");
        assert!(json_str.contains("\"path\""), "path field must be present");
        assert!(
            json_str.contains("\"reason\""),
            "reason field must be present"
        );
        assert!(
            json_str.contains("\"workspaces\""),
            "workspaces field must be present"
        );
        assert!(
            json_str.contains("\"sides\""),
            "sides field must be present"
        );
        assert!(
            json_str.contains("\"atoms\""),
            "atoms field must be present"
        );
        assert!(
            json_str.contains("\"resolution_strategies\""),
            "resolution_strategies must be present"
        );
        assert!(
            json_str.contains("\"suggested_resolution\""),
            "suggested_resolution must be present"
        );
        assert!(
            json_str.contains("\"base_content\""),
            "base_content must be present"
        );
        assert!(
            json_str.contains("\"workspace\""),
            "workspace attribution in sides"
        );

        // Can parse it back as a generic JSON value (roundtrip)
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["type"], "content");
        assert_eq!(parsed["path"], "src/lib.rs");
        assert!(parsed["workspaces"].is_array());
        assert!(parsed["sides"].is_array());
        assert!(parsed["atoms"].is_array());
        assert!(parsed["resolution_strategies"].is_array());
        assert!(parsed["base_content"].is_string());
    }

    #[test]
    fn merge_conflict_output_roundtrip() {
        let records = vec![
            content_record("src/lib.rs", "base", "alice", "bob"),
            add_add_record("src/new.rs"),
        ];
        let conflicts: Vec<ConflictJson> = records.iter().map(conflict_record_to_json).collect();
        let output = MergeConflictOutput {
            status: "conflict".to_string(),
            workspaces: vec!["alice".to_string(), "bob".to_string()],
            conflict_count: conflicts.len(),
            conflicts,
            message: "Merge has 2 unresolved conflict(s). Resolve them and retry.".to_string(),
            to_fix: "maw ws merge alice bob".to_string(),
        };

        let json_str = serde_json::to_string_pretty(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed["status"], "conflict");
        assert_eq!(parsed["conflict_count"], 2);
        assert!(parsed["conflicts"].is_array());
        assert_eq!(parsed["conflicts"].as_array().unwrap().len(), 2);
        assert!(parsed["to_fix"].is_string());

        // Verify the conflicts array has the expected structure
        let first = &parsed["conflicts"][0];
        assert_eq!(first["type"], "content");
        assert!(first["sides"].is_array());
        assert!(first["atoms"].is_array());
        assert!(first["resolution_strategies"].is_array());
    }

    #[test]
    fn merge_success_output_roundtrip() {
        let output = MergeSuccessOutput {
            status: "success".to_string(),
            workspaces: vec!["alice".to_string()],
            branch: "manifold".to_string(),
            epoch: "a".repeat(40),
            unique_count: 3,
            shared_count: 1,
            resolved_count: 1,
            conflict_count: 0,
            conflicts: vec![],
            message: "Merged to manifold: adopt work from alice".to_string(),
            next: "maw push".to_string(),
        };

        let json_str = serde_json::to_string_pretty(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed["status"], "success");
        assert_eq!(parsed["conflict_count"], 0);
        assert!(parsed["conflicts"].as_array().unwrap().is_empty());
        assert_eq!(parsed["branch"], "manifold");
        assert_eq!(parsed["next"], "maw push");
        assert_eq!(parsed["unique_count"], 3);
    }

    #[test]
    fn conflicts_output_roundtrip() {
        let records = vec![content_record("main.rs", "base", "alice", "bob")];
        let conflicts: Vec<ConflictJson> = records.iter().map(conflict_record_to_json).collect();
        let output = ConflictsOutput {
            status: "conflict".to_string(),
            workspaces: vec!["alice".to_string()],
            has_conflicts: true,
            conflict_count: 1,
            conflicts,
            message: "1 conflict(s) found. Resolve them before merging.".to_string(),
            to_fix: Some("maw ws merge alice".to_string()),
        };

        let json_str = serde_json::to_string_pretty(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed["status"], "conflict");
        assert_eq!(parsed["has_conflicts"], true);
        assert_eq!(parsed["conflict_count"], 1);
        assert!(parsed["to_fix"].is_string());
    }

    #[test]
    fn conflicts_output_clean_roundtrip() {
        let output = ConflictsOutput {
            status: "clean".to_string(),
            workspaces: vec!["alice".to_string()],
            has_conflicts: false,
            conflict_count: 0,
            conflicts: vec![],
            message: "No conflicts found.".to_string(),
            to_fix: None,
        };

        let json_str = serde_json::to_string_pretty(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed["status"], "clean");
        assert_eq!(parsed["has_conflicts"], false);
        assert_eq!(parsed["conflict_count"], 0);
        assert!(parsed["to_fix"].is_null());
    }

    // -----------------------------------------------------------------------
    // Agent usability: confirm JSON contains all info to resolve
    // -----------------------------------------------------------------------

    /// This test verifies that an agent receiving only the JSON output can
    /// understand the conflict and determine how to resolve it.
    #[test]
    fn agent_can_understand_conflict_from_json_alone() {
        // Scenario: alice modified src/main.rs, bob modified the same overlapping lines
        let record = content_record(
            "src/main.rs",
            "fn process_order(id: u64) -> Result<Order> {\n    // original implementation\n}",
            "fn process_order(id: u64) -> Result<Order, Error> {\n    // alice's version\n}",
            "fn process_order(id: u64, opts: Options) -> Result<Order> {\n    // bob's version\n}",
        );
        let conflict_json = conflict_record_to_json(&record);
        let json_str = serde_json::to_string_pretty(&conflict_json).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        // Agent can identify: WHICH file has a conflict
        assert_eq!(parsed["path"], "src/main.rs");

        // Agent can identify: WHY there is a conflict
        assert!(!parsed["reason"].as_str().unwrap().is_empty());
        assert!(!parsed["reason_description"].as_str().unwrap().is_empty());

        // Agent can identify: WHO made each change
        let sides = parsed["sides"].as_array().unwrap();
        let workspaces_in_sides: Vec<&str> = sides
            .iter()
            .map(|s| s["workspace"].as_str().unwrap())
            .collect();
        assert!(workspaces_in_sides.contains(&"alice"));
        assert!(workspaces_in_sides.contains(&"bob"));

        // Agent can read: WHAT each side contains
        for side in sides {
            assert!(side["content"].is_string() || side["is_binary"].as_bool().unwrap_or(false));
            assert!(!side["workspace"].as_str().unwrap().is_empty());
            assert!(!side["change"].as_str().unwrap().is_empty());
        }

        // Agent has: BASE content for reference
        assert!(parsed["base_content"].is_string());

        // Agent has: LOCALIZED conflict regions (atoms)
        let atoms = parsed["atoms"].as_array().unwrap();
        assert!(
            !atoms.is_empty(),
            "atoms should pinpoint the conflict region"
        );
        let atom = &atoms[0];
        assert!(atom["base_region"].is_object());
        assert!(atom["edits"].is_array());
        assert!(atom["reason"].is_object());

        // Agent has: HOW to resolve it
        let strategies = parsed["resolution_strategies"].as_array().unwrap();
        assert!(!strategies.is_empty());
        assert!(!parsed["suggested_resolution"].as_str().unwrap().is_empty());
    }

    /// Verify that missing_base conflicts are also fully parseable.
    #[test]
    fn missing_base_conflict_json_is_parseable() {
        let record = missing_base_record("src/shared.rs");
        let conflict_json = conflict_record_to_json(&record);
        let json_str = serde_json::to_string_pretty(&conflict_json).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed["reason"], "missing_base");
        assert!(
            parsed["base_content"].is_null(),
            "no base content for missing_base"
        );
        assert!(!parsed["sides"].as_array().unwrap().is_empty());
        assert!(!parsed["suggested_resolution"].as_str().unwrap().is_empty());
    }
}
