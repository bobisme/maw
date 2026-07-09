use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use maw_git::GitRepo as _;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::changes::store::ChangesStore;
use crate::format::OutputFormat;
use maw::merge::build_phase::{BuildPhaseOutput, run_build_phase};
use maw::merge::collect::collect_snapshots;
use maw::merge::commit::{
    CommitRecovery, CommitResult, recover_partial_commit_with_branch_base,
    run_commit_phase_with_branch_base,
};
use maw::merge::events::{self as merge_events, MergeEventKind};
use maw::merge::last_conflict::{self as merge_last_conflict, LastConflict, LastConflictEntry};
use maw::merge::prepare::run_prepare_phase;
use maw::merge::quarantine::create_quarantine_workspace;
use maw::merge::resolve::{ConflictReason, ConflictRecord};
use maw::merge::validate::{ValidateOutcome, run_validate_phase, write_validation_artifact};
use maw_core::backend::WorkspaceBackend;
use maw_core::config::{ManifoldConfig, MergeDriverKind};
use maw_core::merge::partition::partition_by_path;
use maw_core::merge::plan::{
    DriverInfo, MergePlan, PredictedConflict, ValidationInfo, WorkspaceChange, WorkspaceReport,
    compute_merge_id, write_plan_artifact, write_workspace_report_artifact,
};
use maw_core::merge::types::{ChangeKind, PatchSet as CollectedPatchSet};
use maw_core::merge_state::{
    AbortOutcome, MergePhase, MergeStateFile, abort_merge_state, run_cleanup_phase,
};
use maw_core::model::conflict::ConflictAtom;
use maw_core::model::conflict::Region;
use maw_core::model::patch::{FileId, PatchSet as ModelPatchSet, PatchValue};
use maw_core::model::types::{EpochId, GitOid, WorkspaceId};
use maw_core::oplog::read::read_head;
use maw_core::oplog::types::{OpPayload, Operation};
use tracing::instrument;

use super::capture::capture_before_destroy;
use super::destroy_record::{DestroyReason, write_destroy_record};
use super::{
    MawConfig, get_backend, oplog_runtime::append_operation_with_runtime_checkpoint, repo_root,
};

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
    /// Short deterministic conflict ID, e.g. "cf-k7mx".
    ///
    /// Use this ID with `--resolve cf-k7mx=WORKSPACE` to resolve the conflict inline.
    /// Atom-level IDs (e.g. "cf-k7mx.0") are listed in the `atom_ids` field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

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

    /// Atom-level conflict IDs, one per entry in `atoms`.
    ///
    /// Use these with `--resolve cf-k7mx.0=WORKSPACE` for per-region resolution.
    /// Only workspace name strategies are supported at atom level (not content:).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub atom_ids: Vec<String>,

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
    /// Structured guidance for agents (warnings, follow-up actions).
    pub advice: Vec<MergeAdvice>,
    /// bn-mq6j: names of sibling workspaces that ended this merge's
    /// auto-rebase pass in a conflicted state. Empty when auto-rebase was
    /// disabled, found no siblings to rebase, or none of them conflicted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sibling_conflicts: Vec<String>,
}

/// Structured advice entry for merge JSON output.
#[derive(Debug, Clone, Serialize)]
pub struct MergeAdvice {
    /// Severity level (`info`, `warn`, `error`).
    pub level: &'static str,
    /// Programmatic advice identifier (kebab-case).
    #[serde(rename = "type")]
    pub advice_type: &'static str,
    /// Human-readable recommendation.
    pub message: String,
    /// Optional machine-parseable details.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<MergeAdviceDetails>,
}

/// Optional details attached to a merge advice entry.
#[derive(Debug, Clone, Serialize)]
pub struct MergeAdviceDetails {
    /// The auto-generated merge commit subject used.
    pub commit_subject: String,
    /// Exact command to amend the commit message.
    pub amend_command: String,
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
    /// Template resolve command with all conflict IDs defaulting to the first workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolve_command: Option<String>,
}

/// JSON output for `maw ws merge --dry-run --format json`.
#[derive(Debug, Serialize)]
pub struct MergeDryRunOutput {
    /// Always "dry-run".
    pub status: String,
    /// Always true for this payload.
    pub dry_run: bool,
    /// Source workspaces that would be merged.
    pub workspaces: Vec<String>,
    /// Merge destination (`--into` target).
    pub into: String,
    /// Per-workspace change summaries.
    pub workspace_changes: Vec<DryRunWorkspaceChanges>,
    /// Files changed in more than one workspace.
    pub potential_conflicts: Vec<DryRunPotentialConflict>,
    /// Human-readable summary.
    pub message: String,
    /// Exact command to run the real merge.
    pub to_fix: String,
}

/// Change summary for one workspace in dry-run mode.
#[derive(Debug, Serialize)]
pub struct DryRunWorkspaceChanges {
    pub workspace: String,
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
    pub change_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Potential conflict discovered during dry-run (same path changed by multiple workspaces).
#[derive(Debug, Serialize)]
pub struct DryRunPotentialConflict {
    pub path: String,
    pub workspaces: Vec<String>,
}

// ---------------------------------------------------------------------------
// Conflict IDs (terseid-based, stateless)
// ---------------------------------------------------------------------------

/// A conflict record annotated with a deterministic short ID.
#[derive(Debug)]
struct ConflictWithId {
    /// File-level conflict ID, e.g. "cf-k7mx".
    id: String,
    /// The underlying conflict record from the merge engine.
    record: ConflictRecord,
    /// Atom-level IDs, e.g. `["cf-k7mx.0", "cf-k7mx.1"]`.
    atom_ids: Vec<String>,
}

/// Assign deterministic terseid-based IDs to each conflict.
///
/// File-level ID: `cf-{hash(path, 4)}`.
/// Atom-level ID: `cf-{hash}.{index}`.
///
/// Same path always produces the same ID within a merge attempt.
fn assign_conflict_ids(conflicts: &[ConflictRecord]) -> Vec<ConflictWithId> {
    conflicts
        .iter()
        .map(|record| {
            let path_str = record.path.to_string_lossy();
            let hash = terseid::hash(path_str.as_bytes(), 4);
            let file_id = format!("cf-{hash}");
            let atom_ids: Vec<String> = (0..record.atoms.len())
                .map(|i| {
                    let idx = u32::try_from(i).unwrap_or(u32::MAX);
                    terseid::child_id(&file_id, idx)
                })
                .collect();
            ConflictWithId {
                id: file_id,
                record: record.clone(),
                atom_ids,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Resolution parsing and application
// ---------------------------------------------------------------------------

/// A resolution strategy for a conflict.
#[derive(Debug, Clone)]
enum Resolution {
    /// Keep a specific workspace's version (by name).
    Workspace(String),
    /// Use file content from the given path.
    Content(PathBuf),
}

/// Parse `--resolve ID=STRATEGY` strings into a map.
///
/// Valid formats:
/// - `cf-k7mx=alice` (use alice's version)
/// - `cf-k7mx=content:/path/to/file` (use file content)
/// - `cf-k7mx.0=alice` (atom-level: use alice's version for this region)
///
/// Any value that doesn't start with `content:` is treated as a workspace name.
/// Workspace name validation happens later in `apply_resolutions`.
fn parse_resolutions(raw: &[String]) -> Result<BTreeMap<String, Resolution>> {
    let mut map = BTreeMap::new();
    for entry in raw {
        let (id, strategy) = entry.split_once('=').ok_or_else(|| {
            anyhow::anyhow!(
                "Invalid --resolve format: '{entry}'\n  \
                 Expected: ID=WORKSPACE or ID=content:PATH\n  \
                 Examples: cf-k7mx=alice, cf-k7mx=content:/path/to/file"
            )
        })?;

        let id = id.trim();
        let strategy = strategy.trim();

        if !id.starts_with("cf-") {
            bail!(
                "Invalid conflict ID: '{id}'\n  \
                 Conflict IDs start with 'cf-' (e.g., cf-k7mx)"
            );
        }

        if strategy.is_empty() {
            bail!(
                "Empty resolution for '{id}'\n  \
                 Expected: ID=WORKSPACE or ID=content:PATH"
            );
        }

        let resolution = strategy.strip_prefix("content:").map_or_else(
            || Resolution::Workspace(strategy.to_string()),
            |path| Resolution::Content(PathBuf::from(path)),
        );

        map.insert(id.to_string(), resolution);
    }
    Ok(map)
}

/// Apply resolutions to conflicts, returning resolved file contents and remaining unresolved conflicts.
///
/// For file-level IDs: resolves the entire file.
/// For atom-level IDs: only ours/theirs supported — splices resolved atoms into the file.
#[allow(clippy::type_complexity)]
fn apply_resolutions(
    conflicts: &[ConflictWithId],
    resolutions: &BTreeMap<String, Resolution>,
    workspace_dirs: &BTreeMap<WorkspaceId, PathBuf>,
) -> Result<(BTreeMap<PathBuf, Vec<u8>>, Vec<ConflictWithId>)> {
    let mut resolved_contents: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    let mut remaining: Vec<ConflictWithId> = Vec::new();

    for conflict in conflicts {
        // Check for file-level resolution first
        if let Some(resolution) = resolutions.get(&conflict.id) {
            let content = resolve_file_content(resolution, &conflict.record, workspace_dirs)?;
            resolved_contents.insert(conflict.record.path.clone(), content);
            continue;
        }

        // Check for atom-level resolutions (all atoms must be resolved for the file to count)
        if !conflict.atom_ids.is_empty() {
            let mut all_atoms_resolved = true;
            let mut atom_resolutions: Vec<Option<&Resolution>> = Vec::new();
            for atom_id in &conflict.atom_ids {
                if let Some(res) = resolutions.get(atom_id) {
                    atom_resolutions.push(Some(res));
                } else {
                    all_atoms_resolved = false;
                    atom_resolutions.push(None);
                }
            }

            if all_atoms_resolved {
                let content = resolve_atoms(&conflict.record, &atom_resolutions)?;
                resolved_contents.insert(conflict.record.path.clone(), content);
                continue;
            }

            // Partial atom resolution: check if *any* atoms were targeted
            if atom_resolutions.iter().any(Option::is_some) {
                bail!(
                    "Partial atom resolution for {}: all atoms must be resolved together.\n  \
                     Atoms: {}",
                    conflict.id,
                    conflict.atom_ids.join(", ")
                );
            }
        }

        // No resolution for this conflict — it remains unresolved
        remaining.push(ConflictWithId {
            id: conflict.id.clone(),
            record: conflict.record.clone(),
            atom_ids: conflict.atom_ids.clone(),
        });
    }

    // Check for resolution IDs that don't match any conflict
    for res_id in resolutions.keys() {
        let matches_any = conflicts
            .iter()
            .any(|c| c.id == *res_id || c.atom_ids.iter().any(|a| a == res_id));
        if !matches_any {
            bail!(
                "Unknown conflict ID in --resolve: '{res_id}'\n  \
                 Valid IDs for this merge: {}",
                conflicts
                    .iter()
                    .map(|c| c.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    Ok((resolved_contents, remaining))
}

/// Resolve a whole file using the given strategy.
fn resolve_file_content(
    resolution: &Resolution,
    record: &ConflictRecord,
    workspace_dirs: &BTreeMap<WorkspaceId, PathBuf>,
) -> Result<Vec<u8>> {
    match resolution {
        Resolution::Workspace(name) => {
            let ws_id = WorkspaceId::new(name)
                .map_err(|e| anyhow::anyhow!("Invalid workspace name '{name}': {e}"))?;
            let side = record
                .sides
                .iter()
                .find(|s| s.workspace_id == ws_id)
                .ok_or_else(|| {
                    let available: Vec<_> = record
                        .sides
                        .iter()
                        .map(|s| s.workspace_id.to_string())
                        .collect();
                    anyhow::anyhow!(
                        "Workspace '{name}' is not a side in this conflict.\n  \
                         Available: {}",
                        available.join(", ")
                    )
                })?;
            side.content.clone().ok_or_else(|| {
                let others: Vec<_> = record
                    .sides
                    .iter()
                    .filter(|s| s.workspace_id != ws_id)
                    .map(|s| s.workspace_id.to_string())
                    .collect();
                anyhow::anyhow!(
                    "Workspace '{name}' has no content (deleted) for {}.\n  \
                     Try: {} or content:PATH",
                    record.path.display(),
                    others.join(", ")
                )
            })
        }
        Resolution::Content(path) => {
            // If path is relative, try resolving against each workspace dir
            let abs_path = if path.is_absolute() {
                path.clone()
            } else {
                // Try workspace dirs
                let mut found = None;
                for ws_dir in workspace_dirs.values() {
                    let candidate = ws_dir.join(path);
                    if candidate.exists() {
                        found = Some(candidate);
                        break;
                    }
                }
                found.unwrap_or_else(|| path.clone())
            };
            std::fs::read(&abs_path).with_context(|| {
                format!(
                    "Could not read resolution file: {}\n  \
                     For content: strategy, provide an absolute path or a path relative to a workspace.",
                    abs_path.display()
                )
            })
        }
    }
}

/// Resolve individual atoms within a file, reconstructing the complete content.
/// Extract byte range from a Region, converting line-based regions to byte offsets.
fn region_byte_range(region: &Region, content: &[u8]) -> (u32, u32) {
    let to_u32 = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    match region {
        Region::AstNode {
            start_byte,
            end_byte,
            ..
        } => (*start_byte, *end_byte),
        Region::Lines { start, end } => {
            // Convert 1-indexed line numbers to byte offsets
            let text = std::str::from_utf8(content).unwrap_or("");
            let mut line_starts: Vec<usize> = vec![0];
            for (i, b) in text.bytes().enumerate() {
                if b == b'\n' {
                    line_starts.push(i + 1);
                }
            }
            let s = if *start > 0 {
                line_starts
                    .get((*start - 1) as usize)
                    .copied()
                    .unwrap_or(content.len())
            } else {
                0
            };
            let e = line_starts
                .get((*end - 1) as usize)
                .copied()
                .unwrap_or(content.len());
            (to_u32(s), to_u32(e))
        }
        Region::WholeFile => (0, to_u32(content.len())),
    }
}

/// For each atom, picks a workspace's content based on the resolution.
/// Only workspace name strategies are supported at atom level (content: requires whole-file).
fn resolve_atoms(
    record: &ConflictRecord,
    atom_resolutions: &[Option<&Resolution>],
) -> Result<Vec<u8>> {
    // For atom-level resolution we reconstruct the file by walking the base
    // content in bytes, substituting conflict regions with the chosen side's
    // edit content. Works with both Line and AstNode regions (byte offsets).
    let base = record.base.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "Atom-level resolution requires base content for {}. Use file-level ID instead.",
            record.path.display()
        )
    })?;

    // Validate resolutions and collect sorted atoms
    let mut atoms_sorted: Vec<(
        u32,
        u32,
        &maw_core::model::conflict::ConflictAtom,
        &Resolution,
    )> = Vec::new();
    for (i, atom) in record.atoms.iter().enumerate() {
        let res = atom_resolutions[i].ok_or_else(|| {
            anyhow::anyhow!(
                "Missing resolution for atom {i} of {}",
                record.path.display()
            )
        })?;
        match res {
            Resolution::Workspace(_) => {}
            Resolution::Content(_) => bail!(
                "Atom-level resolution only supports workspace names, not content:PATH.\n  \
                 For content: strategy, use the file-level ID."
            ),
        }
        let (start_byte, end_byte) = region_byte_range(&atom.base_region, base);
        atoms_sorted.push((start_byte, end_byte, atom, res));
    }
    atoms_sorted.sort_by_key(|(start, _, _, _)| *start);

    // Reconstruct: walk base bytes, substituting conflict regions
    let mut result: Vec<u8> = Vec::with_capacity(base.len());
    let mut pos: u32 = 0; // current byte position in base

    for (base_start, base_end, atom, resolution) in &atoms_sorted {
        // Copy base bytes before this region
        let s = pos.min(*base_start) as usize;
        let e = (*base_start as usize).min(base.len());
        result.extend_from_slice(&base[s..e]);

        // Find the matching edit from the chosen workspace
        let ws_name = match resolution {
            Resolution::Workspace(name) => name.as_str(),
            Resolution::Content(_) => unreachable!(),
        };
        let edit = atom.edits.iter().find(|ed| ed.workspace == ws_name);
        if let Some(edit) = edit {
            result.extend_from_slice(edit.content.as_bytes());
        } else {
            // No matching edit — keep the base region as-is
            let rs = (*base_start as usize).min(base.len());
            let re = (*base_end as usize).min(base.len());
            result.extend_from_slice(&base[rs..re]);
        }

        pos = *base_end;
    }

    // Copy remaining base bytes after the last atom
    if (pos as usize) < base.len() {
        result.extend_from_slice(&base[pos as usize..]);
    }

    Ok(result)
}

/// Patch the candidate tree with resolved file contents, producing a new commit OID.
///
/// Pure-gix pipeline (no `read-tree`/`update-index`/`write-tree`):
/// 1. Open the repo and resolve `candidate` to its tree OID via `read_commit`.
/// 2. Write each resolved file as a blob with `write_blob_with_path` (so LFS
///    smudge/clean semantics match the rest of the merge engine).
/// 3. `edit_tree(candidate_tree, &[Upsert ...])` produces a new tree OID.
/// 4. `create_commit` writes the new commit with the candidate's parent.
fn patch_candidate_tree(
    root: &Path,
    candidate: &GitOid,
    resolved: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<GitOid> {
    if resolved.is_empty() {
        return Ok(candidate.clone());
    }

    let repo = maw_git::GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;

    // 1. Resolve candidate commit → tree OID + parent.
    let candidate_git_oid: maw_git::GitOid =
        candidate
            .as_str()
            .parse()
            .map_err(|e: maw_git::OidParseError| {
                anyhow::anyhow!("invalid candidate OID '{}': {e}", candidate.as_str())
            })?;
    let candidate_commit = repo
        .read_commit(candidate_git_oid)
        .map_err(|e| anyhow::anyhow!("failed to read candidate commit {candidate}: {e}"))?;
    let candidate_tree = candidate_commit.tree_oid;

    // 2. Hash resolved contents as blobs and build the TreeEdit list.
    //
    // bn-1tl6: preserve the resolved path's existing mode. The candidate tree
    // already carries the (correct) mode for this path — it was produced by
    // the build phase from the now-mode-aware collect path. Hardcoding
    // `EntryMode::Blob` here silently demoted conflict-resolved executables
    // (100755) and symlinks (120000) to regular files (100644) in the
    // committed merge tree. Look the entry up in the candidate tree and reuse
    // its mode; fall back to `Blob` only if the path is absent there (a
    // genuinely new resolved path, which conflict resolution does not
    // normally produce — every conflicting path existed on at least one side).
    let mut edits: Vec<maw_git::TreeEdit> = Vec::with_capacity(resolved.len());
    for (path, content) in resolved {
        let path_str = path.to_string_lossy().to_string();
        let blob_oid = repo.write_blob_with_path(content, &path_str).map_err(|e| {
            anyhow::anyhow!(
                "Failed to hash resolved content for {}: {e}",
                path.display()
            )
        })?;
        let mode = repo
            .find_entry_at_path(candidate_tree, &path_str)
            .ok()
            .flatten()
            .map_or(maw_git::EntryMode::Blob, |(m, _oid)| m);
        edits.push(maw_git::TreeEdit::Upsert {
            path: path_str,
            mode,
            oid: blob_oid,
        });
    }

    // 3. Apply the edits to the candidate tree to produce a new tree OID.
    let new_tree_oid = repo
        .edit_tree(candidate_tree, &edits)
        .map_err(|e| anyhow::anyhow!("edit_tree failed: {e}"))?;

    // 4. commit-tree with the same parent as the candidate.
    let parent_spec = format!("{candidate}^");
    let parent_git_oid = repo
        .rev_parse(&parent_spec)
        .map_err(|e| anyhow::anyhow!("Failed to get candidate parent: {e}"))?;

    let new_commit_git_oid = repo
        .create_commit(
            new_tree_oid,
            &[parent_git_oid],
            "epoch: merge with conflict resolutions",
            None,
        )
        .map_err(|e| anyhow::anyhow!("commit-tree failed: {e}"))?;

    let new_commit_oid = new_commit_git_oid.to_string();
    GitOid::new(&new_commit_oid)
        .map_err(|e| anyhow::anyhow!("Invalid patched commit OID '{new_commit_oid}': {e}"))
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

/// Map a workspace ID to a display name.
///
/// The synthetic `epoch-delta` workspace (injected for stale-workspace
/// conflict detection) is shown as `"epoch (previous merge)"` so agents
/// and humans understand it represents content from a prior merge, not an
/// actual workspace.
fn workspace_display_name(ws_id: &WorkspaceId) -> String {
    if ws_id.is_epoch_delta() {
        "epoch (previous merge)".to_string()
    } else {
        ws_id.as_str().to_string()
    }
}

/// Convert a `ConflictRecord` from the merge engine into a `ConflictJson`
/// suitable for structured JSON output.
///
/// This bridges the internal merge engine representation to the agent-facing
/// JSON format defined in §6.4 of the design doc.
///
/// When `id_info` is provided, the JSON includes conflict and atom IDs for
/// use with `--resolve`.
#[cfg(test)]
fn conflict_record_to_json(record: &ConflictRecord) -> ConflictJson {
    conflict_record_to_json_with_id(record, None, &[])
}

/// Map a `ConflictReason` to its JSON output fields:
/// `(conflict_type, reason_key, resolution_strategies, suggested_resolution)`.
fn conflict_reason_json_fields(
    reason: &ConflictReason,
) -> (&'static str, &'static str, Vec<String>, String) {
    match reason {
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
        ConflictReason::FileDirectory {
            file_side,
            dir_child_example,
        } => (
            "file_directory",
            "file_directory",
            vec![
                "rename_file".to_string(),
                "rename_directory".to_string(),
                "keep_one_side".to_string(),
            ],
            format!(
                "D/F clash: workspace '{file_side}' has a FILE at this path while another \
                 workspace has files under it as a directory (e.g. '{}'). \
                 Rename either the file or the directory-side files to resolve.",
                dir_child_example.display()
            ),
        ),
    }
}

/// Convert with optional conflict ID and atom IDs.
fn conflict_record_to_json_with_id(
    record: &ConflictRecord,
    id: Option<&str>,
    atom_ids: &[String],
) -> ConflictJson {
    let (conflict_type, reason_key, resolution_strategies, suggested_resolution) =
        conflict_reason_json_fields(&record.reason);

    // Extract workspace names from sides (with epoch-delta display name)
    let workspaces: Vec<String> = record
        .sides
        .iter()
        .map(|s| workspace_display_name(&s.workspace_id))
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
                workspace: workspace_display_name(&s.workspace_id),
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
        id: id.map(String::from),
        conflict_type: conflict_type.to_string(),
        path: record.path.display().to_string(),
        reason: reason_key.to_string(),
        reason_description: record.reason.to_string(),
        workspaces,
        base_content,
        base_is_binary,
        sides,
        atoms: record.atoms.clone(),
        atom_ids: atom_ids.to_vec(),
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
        .map(|s| workspace_display_name(&s.workspace_id))
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

/// Print detailed conflict information with terseid IDs and resolve commands.
fn print_conflict_report(conflicts_with_ids: &[ConflictWithId], ws_names: &[String], into: &str) {
    print_conflict_report_with_resolve(conflicts_with_ids, ws_names, into, None);
}

/// bn-yyx: persist the conflict surface + emit a `ConflictDetected` event.
///
/// This is the *load-bearing* call for the `ws_merge_structured_conflict`
/// friction reduction. After invocation, the agent can recall everything the
/// scrollback showed via:
///
/// - `cat .manifold/artifacts/merge/last-conflict.json`
/// - `maw merge last-conflict`
/// - `maw merge events`
///
/// — without re-issuing `maw ws merge`, which is the wasted-turn this fix
/// targets.
///
/// Both side-channel writes are *best-effort*: a full-disk or permission
/// error here MUST NOT regress merge correctness (Prime Invariant), so we
/// `let _ =` the results and continue. Tracing-level warnings make the
/// failure visible without changing exit codes.
fn persist_merge_conflict_surface(
    manifold_dir: &Path,
    sources: &[String],
    into: &str,
    conflicts_with_ids: &[ConflictWithId],
) {
    let conflict_ids: Vec<String> = conflicts_with_ids.iter().map(|c| c.id.clone()).collect();
    let paths: Vec<String> = conflicts_with_ids
        .iter()
        .map(|c| c.record.path.display().to_string())
        .collect();

    // Append the event log entry first — even if last-conflict write fails,
    // the event log still records that something happened.
    if let Err(e) = merge_events::append_event(
        manifold_dir,
        MergeEventKind::ConflictDetected {
            sources: sources.to_vec(),
            into: into.to_string(),
            conflict_count: conflict_ids.len(),
            conflict_ids: conflict_ids.clone(),
            paths,
        },
    ) {
        tracing::warn!("merge event log write failed: {e}");
    }

    // Persist the snapshot.
    let default_resolve_ws = sources.first().cloned().unwrap_or_default();
    let entries: Vec<LastConflictEntry> = conflicts_with_ids
        .iter()
        .map(|c| LastConflictEntry {
            id: c.id.clone(),
            path: c.record.path.display().to_string(),
            sides: c
                .record
                .sides
                .iter()
                .map(|s| s.workspace_id.as_str().to_string())
                .collect(),
            reason: format!("{}", c.record.reason),
        })
        .collect();
    let recovery_commands = merge_last_conflict::build_recovery_commands(
        sources,
        into,
        &conflict_ids,
        &default_resolve_ws,
    );
    let snapshot = LastConflict {
        schema_version: merge_last_conflict::LAST_CONFLICT_SCHEMA_VERSION,
        ts_unix_ms: merge_events::now_unix_ms(),
        sources: sources.to_vec(),
        into: into.to_string(),
        conflicts: entries,
        recovery_commands,
    };
    if let Err(e) = merge_last_conflict::write(manifold_dir, &snapshot) {
        tracing::warn!("last-conflict snapshot write failed: {e}");
    }
}

/// bn-yyx: best-effort emit of an `IntegrationStarted` event.
fn emit_integration_started(manifold_dir: &Path, sources: &[String], into: &str, check_only: bool) {
    if let Err(e) = merge_events::append_event(
        manifold_dir,
        MergeEventKind::IntegrationStarted {
            sources: sources.to_vec(),
            into: into.to_string(),
            check_only,
        },
    ) {
        tracing::warn!("merge event log write failed: {e}");
    }
}

/// bn-yyx: best-effort emit of an `IntegrationCompleted` event + clear the
/// persisted last-conflict snapshot (the merge succeeded, so the prior
/// conflict is no longer the "latest").
fn emit_integration_completed(
    manifold_dir: &Path,
    sources: &[String],
    into: &str,
    merge_commit: &str,
) {
    if let Err(e) = merge_events::append_event(
        manifold_dir,
        MergeEventKind::IntegrationCompleted {
            sources: sources.to_vec(),
            into: into.to_string(),
            merge_commit: merge_commit.to_string(),
        },
    ) {
        tracing::warn!("merge event log write failed: {e}");
    }
    if let Err(e) = merge_last_conflict::clear(manifold_dir) {
        tracing::warn!("last-conflict clear failed: {e}");
    }
}

/// bn-yyx: best-effort emit of an `IntegrationAborted` event.
fn emit_integration_aborted(manifold_dir: &Path, sources: &[String], into: &str, reason: &str) {
    if let Err(e) = merge_events::append_event(
        manifold_dir,
        MergeEventKind::IntegrationAborted {
            sources: sources.to_vec(),
            into: into.to_string(),
            reason: reason.to_string(),
        },
    ) {
        tracing::warn!("merge event log write failed: {e}");
    }
}

fn print_conflict_report_with_resolve(
    conflicts_with_ids: &[ConflictWithId],
    ws_names: &[String],
    into: &str,
    prebuilt_resolve_args: Option<&[String]>,
) {
    println!();
    println!("BUILD: {} conflict(s) detected.", conflicts_with_ids.len());
    // bn-yyx: anti-retry cue — point the agent at the *recall* verbs FIRST
    // so the next call lands on `maw merge last-conflict` / `maw ws conflicts`
    // / `maw merge events` rather than another `maw ws merge`. Re-issuing the
    // same merge is the wasted-turn the friction cluster attributes; making
    // the alternative more visible than the doomed retry is the fix.
    println!();
    println!("IMPORTANT: do NOT re-run `maw ws merge` to re-discover this");
    println!("conflict — it is recorded out-of-band. Recall it via:");
    let ws_args_for_recall = ws_names.join(" ");
    println!("  maw merge last-conflict                    # full surface (text)");
    println!("  maw merge last-conflict --format json      # machine-parseable");
    println!("  maw merge events --since-last-attempt      # event log tail");
    println!("  maw ws conflicts {ws_args_for_recall} --format json   # re-derive via engine");
    println!();

    for c in conflicts_with_ids {
        let reason = format!("{}", c.record.reason);
        let ws_list: Vec<String> = c
            .record
            .sides
            .iter()
            .map(|s| workspace_display_name(&s.workspace_id))
            .collect();
        println!("  {:<10} {:<40} {}", c.id, c.record.path.display(), reason);
        println!("           Workspaces: {}", ws_list.join(", "));

        // Show content snippets from each side (up to 5 lines each)
        for side in &c.record.sides {
            if let Some(ref content) = side.content {
                let text = String::from_utf8_lossy(content);
                let lines: Vec<&str> = text.lines().collect();
                let preview_lines = 5;
                let truncated = lines.len() > preview_lines;
                let shown: Vec<&str> = lines.iter().take(preview_lines).copied().collect();
                let label = workspace_display_name(&side.workspace_id);
                println!("           [{label}]:");
                for line in &shown {
                    println!("             {line}");
                }
                if truncated {
                    println!(
                        "             ... ({} more lines)",
                        lines.len() - preview_lines
                    );
                }
            }
        }

        if !c.atom_ids.is_empty() {
            println!("           Atoms:");
            for (i, atom) in c.record.atoms.iter().enumerate() {
                let atom_id = &c.atom_ids[i];
                let region_desc = match atom.base_region {
                    Region::Lines { start, end } => format!("lines {start}-{end}"),
                    _ => "region".to_string(),
                };
                let reason_desc = atom.reason.description();
                println!("             {atom_id:<14} {region_desc:<16} {reason_desc}");
            }
        }
        println!();
    }

    // Build the resolve command template
    let ws_args = ws_names.join(" ");
    let default_ws = ws_names.first().map_or("WORKSPACE", |s| s.as_str());
    let resolve_args_owned: Vec<String> = if prebuilt_resolve_args.is_some() {
        Vec::new()
    } else {
        conflicts_with_ids
            .iter()
            .map(|c| format!("--resolve {}={default_ws}", c.id))
            .collect()
    };
    let resolve_args = prebuilt_resolve_args.unwrap_or(resolve_args_owned.as_slice());
    println!("To resolve, re-run with --resolve:");
    println!(
        "  maw ws merge {} --into {} {}",
        ws_args,
        into,
        resolve_args.join(" ")
    );
    println!();
    let default_ws = ws_names.first().map_or("WORKSPACE", |s| s.as_str());
    println!("Or resolve all at once:");
    println!("  maw ws merge {ws_args} --into {into} --resolve-all={default_ws}");
    println!();
    println!("Options:  ID=WORKSPACE | ID=content:PATH");
    println!();
    println!("To inspect full content:  maw ws conflicts {ws_args} --format json");
    println!("Or edit files in a workspace, commit, and re-merge.");
}

fn conflict_retry_message(into_target: &str) -> String {
    let sanitized: String = into_target
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("fix:resolve-merge-conflicts-into-{sanitized}")
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
    /// Number of commits on the target branch that are NOT in the maw epoch
    /// (out-of-maw commits made directly on trunk, e.g. a plain `git commit`
    /// or `git pull`). The epoch ref lags the branch tip by this many commits.
    /// The real merge absorbs them automatically, but `--check` surfaces the
    /// count so the divergence isn't silent (bn-1huu). `0` when the epoch is
    /// at the branch tip or the relationship isn't a clean fast-forward.
    #[serde(default)]
    pub trunk_ahead: u32,
    /// Uncommitted tracked files in the trunk/default workspace at check time.
    /// Informational only — never blocks the merge. Surfaced so a dirty-trunk
    /// merge (which preserves and replays these files, pinning a recovery
    /// snapshot) is not a silent surprise (bn-1xmk).
    #[serde(default)]
    pub dirty_trunk_files: Vec<String>,
    /// Epoch-vs-branch drift classification (kebab-case slug: `in-sync`,
    /// `ff-absorbable`, `ff-blocked`, `diverged`), computed via
    /// [`super::epoch_drift::classify_drift`] when this check would update
    /// the epoch. `None` when the check doesn't touch the epoch (a
    /// change-branch target) or classification wasn't available (e.g. the
    /// epoch ref is unset). Drives the `--check` NOTE wording (bn-3eew):
    /// `ff-absorbable` is informational (the merge auto-absorbs it safely);
    /// `ff-blocked` / `diverged` are a real warning pointing at
    /// `maw epoch sync` / `maw doctor --repair`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drift_classification: Option<String>,
}

/// Workspace info included in check result.
#[derive(Debug, Serialize)]
pub struct CheckWorkspaceInfo {
    pub name: String,
    pub change_id: String,
}

/// Count commits on the target branch that are ahead of the maw epoch — i.e.
/// out-of-maw commits made directly on trunk that left `refs/manifold/epoch/current`
/// lagging the branch tip (bn-1huu). Best-effort: returns `0` on any read/parse
/// failure (never blocks the check), and only counts when the epoch is a strict
/// ancestor of the branch tip (a clean fast-forward — the absorb case); a forked
/// or unrelated epoch is not "N ahead".
fn trunk_commits_ahead_of_epoch(root: &Path, branch_tip: &maw_core::model::types::GitOid) -> u32 {
    let Ok(Some(epoch_oid)) = maw_core::refs::read_epoch_current(root) else {
        return 0;
    };
    if epoch_oid.as_str() == branch_tip.as_str() {
        return 0;
    }
    let Ok(repo) = super::ff_absorb::open_repo(root) else {
        return 0;
    };
    let (Ok(epoch_git), Ok(branch_git)) = (
        epoch_oid.as_str().parse::<maw_git::GitOid>(),
        branch_tip.as_str().parse::<maw_git::GitOid>(),
    ) else {
        return 0;
    };
    // Only the clean fast-forward case (epoch strictly behind branch) is a
    // meaningful "trunk is N ahead" — that's exactly what the merge absorbs.
    match super::ff_absorb::is_strict_ancestor(&repo, &epoch_git, &branch_git) {
        Ok(true) => repo
            .count_commits_between(epoch_git, branch_git)
            .unwrap_or(0),
        _ => 0,
    }
}

pub fn json_not_ready_result(workspaces: &[String], reason: impl Into<String>) -> CheckResult {
    let reason = reason.into();
    let stale = reason.contains(" is stale (behind current epoch)")
        || reason.contains("Stale workspaces cannot be merged/planned");
    CheckResult {
        ready: false,
        conflicts: if stale {
            Vec::new()
        } else {
            vec![ConflictInfo {
                path: String::new(),
                reason,
                sides: Vec::new(),
                line_start: None,
                line_end: None,
            }]
        },
        stale,
        workspace: CheckWorkspaceInfo {
            name: workspaces.first().cloned().unwrap_or_default(),
            change_id: String::new(),
        },
        description: String::new(),
        trunk_ahead: 0,
        dirty_trunk_files: Vec::new(),
        drift_classification: None,
    }
}

fn stale_merge_sources(workspaces: &[String]) -> Result<Vec<String>> {
    let stale_all = super::check_stale_workspaces()?;
    let stale_set: BTreeSet<String> = stale_all.into_iter().collect();
    let stale = workspaces
        .iter()
        .filter(|ws| stale_set.contains((*ws).as_str()))
        .cloned()
        .collect::<Vec<_>>();
    Ok(stale)
}

fn stale_merge_block_message(stale_sources: &[String]) -> String {
    if stale_sources.len() == 1 {
        let ws = &stale_sources[0];
        return format!(
            "Workspace '{ws}' is stale (behind current epoch).\n  \
             To fix: maw ws sync {ws}\n  \
             Persistent workspace alternative: maw ws advance {ws}"
        );
    }

    let list = stale_sources.join(", ");
    format!(
        "Stale workspaces cannot be merged/planned: {list}.\n  \
         To fix: run `maw ws sync <workspace>` for each stale workspace (or `maw ws advance <workspace>` for persistent workspaces), then retry."
    )
}

/// Pre-flight merge check using the new merge engine.
///
/// Runs PREPARE + BUILD without COMMIT to detect conflicts.
/// Returns a `CheckResult` with structured info.
pub fn check_merge(
    workspaces: &[String],
    format: OutputFormat,
    target_workspace: &str,
    target_branch: &str,
    target_change_id: Option<&str>,
    target_updates_epoch: bool,
    force: bool,
) -> Result<()> {
    // bn-yyx: anchor the event log even on dry-run checks so the agent can
    // bound `maw merge events --since` to the latest attempt.
    if let Ok(root) = repo_root() {
        let manifold_dir =
            maw_core::model::layout::LayoutFlavor::detect_with_env(&root).manifold_dir(&root);
        let into_display = target_change_id.unwrap_or(target_workspace).to_string();
        emit_integration_started(&manifold_dir, workspaces, &into_display, true);
    }

    let result = match check_merge_result_for_target(
        workspaces,
        target_workspace,
        target_branch,
        target_change_id,
        target_updates_epoch,
        force,
    ) {
        Ok(result) => result,
        Err(err) if format == OutputFormat::Json => {
            json_not_ready_result(workspaces, err.to_string())
        }
        Err(err) => return Err(err),
    };

    // bn-yyx: if the check surfaced conflicts, persist them so the agent can
    // recall via `maw merge last-conflict` instead of re-running the check.
    if !result.ready
        && !result.conflicts.is_empty()
        && !result.stale
        && let Ok(root) = repo_root()
    {
        let manifold_dir =
            maw_core::model::layout::LayoutFlavor::detect_with_env(&root).manifold_dir(&root);
        let into_display = target_change_id.unwrap_or(target_workspace).to_string();
        // Derive the same `cf-<hash>` terseid `assign_conflict_ids` uses for
        // full merges — so a `--check` snapshot and a follow-up real merge
        // produce stable IDs for the same path.
        let conflict_ids: Vec<String> = result
            .conflicts
            .iter()
            .map(|c| {
                if c.path.is_empty() {
                    "cf-check".to_string()
                } else {
                    format!("cf-{}", terseid::hash(c.path.as_bytes(), 4))
                }
            })
            .collect();
        let paths: Vec<String> = result.conflicts.iter().map(|c| c.path.clone()).collect();
        if let Err(e) = merge_events::append_event(
            &manifold_dir,
            MergeEventKind::ConflictDetected {
                sources: workspaces.to_vec(),
                into: into_display.clone(),
                conflict_count: conflict_ids.len(),
                conflict_ids: conflict_ids.clone(),
                paths,
            },
        ) {
            tracing::warn!("merge event log write failed: {e}");
        }
        let entries: Vec<LastConflictEntry> = result
            .conflicts
            .iter()
            .enumerate()
            .map(|(i, c)| LastConflictEntry {
                id: conflict_ids[i].clone(),
                path: c.path.clone(),
                sides: c.sides.clone(),
                reason: c.reason.clone(),
            })
            .collect();
        let default_resolve_ws = workspaces.first().cloned().unwrap_or_default();
        let recovery_commands = merge_last_conflict::build_recovery_commands(
            workspaces,
            &into_display,
            &conflict_ids,
            &default_resolve_ws,
        );
        let snapshot = LastConflict {
            schema_version: merge_last_conflict::LAST_CONFLICT_SCHEMA_VERSION,
            ts_unix_ms: merge_events::now_unix_ms(),
            sources: workspaces.to_vec(),
            into: into_display,
            conflicts: entries,
            recovery_commands,
        };
        if let Err(e) = merge_last_conflict::write(&manifold_dir, &snapshot) {
            tracing::warn!("last-conflict snapshot write failed: {e}");
        }
    }

    output_check_result(&result, format)
}

fn check_not_ready_reason(result: &CheckResult) -> String {
    if result.stale {
        let ws = &result.workspace.name;
        return format!(
            "workspace '{ws}' is stale; run `maw ws sync {ws}` (or `maw ws advance {ws}` if persistent) and retry"
        );
    }

    if let Some(conflict) = result.conflicts.first() {
        if conflict.path.is_empty() {
            return conflict.reason.clone();
        }
        return format!("conflict at '{}': {}", conflict.path, conflict.reason);
    }

    "not ready".to_owned()
}

/// Run a merge pre-flight check and return the structured result.
///
/// This is the workhorse behind `maw ws merge --check`. It runs PREPARE + BUILD
/// without COMMIT to detect conflicts. Also used by `maw ws list --check` to
/// annotate merge-ready workspaces with conflict counts.
pub fn check_merge_result(workspaces: &[String]) -> Result<CheckResult> {
    let root = repo_root()?;
    let maw_config = MawConfig::load(&root)?;
    let default_ws = maw_config.default_workspace().to_owned();
    let default_branch = maw_config.branch().to_owned();
    check_merge_result_for_target(workspaces, &default_ws, &default_branch, None, true, false)
}

/// List uncommitted *tracked* file changes in a workspace (modified/added/
/// deleted/renamed — not untracked), excluding admin/git trees. Best-effort:
/// returns empty on any error. Feeds `merge --check`'s informational
/// dirty-trunk reporting (bn-1xmk).
fn list_dirty_trunk_tracked_files(ws_path: &Path) -> Vec<String> {
    let Ok(repo) = maw_git::GixRepo::open(ws_path) else {
        return Vec::new();
    };
    let Ok(entries) = repo.status_head_to_worktree() else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter(|e| !matches!(e.status, maw_git::FileStatus::Untracked))
        .map(|e| e.path)
        .filter(|p| {
            let first = p.split('/').next().unwrap_or("");
            !matches!(first, ".maw" | "repo.git" | ".manifold" | ".git")
        })
        .collect()
}

#[expect(
    clippy::too_many_lines,
    reason = "merge check path mirrors the prepare/build pipeline for diagnostics"
)]
fn check_merge_result_for_target(
    workspaces: &[String],
    target_workspace: &str,
    target_branch: &str,
    _target_change_id: Option<&str>,
    target_updates_epoch: bool,
    force: bool,
) -> Result<CheckResult> {
    if workspaces.is_empty() {
        bail!("No workspaces specified for --check");
    }

    let root = repo_root()?;
    let maw_config = MawConfig::load(&root)?;
    let default_ws = maw_config.default_workspace();
    let backend = get_backend()?;
    let branch_ref = format!("refs/heads/{target_branch}");
    let branch_before_oid = maw_core::refs::read_ref(&root, &branch_ref)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Target branch '{target_branch}' does not exist.\n  To fix: create the branch first or repair change metadata, then retry."
        )
    })?;

    // Reject merging the target workspace itself.
    if workspaces.iter().any(|ws| ws == target_workspace) {
        if target_workspace == default_ws {
            bail!("Cannot merge the default workspace — it is the merge target, not a source.");
        }
        bail!(
            "Cannot merge target workspace '{target_workspace}' as a source — it is the merge destination."
        );
    }

    let sources: Vec<WorkspaceId> = workspaces
        .iter()
        .map(|ws| WorkspaceId::new(ws).map_err(|e| anyhow::anyhow!("{e}")))
        .collect::<Result<Vec<_>>>()?;

    let mut workspace_dirs = BTreeMap::new();
    for ws_id in &sources {
        let ws_path = backend.workspace_path(ws_id);
        if !ws_path.exists() {
            // bn-3fhj: distinct MISSING diagnostic for workspaces whose
            // worktree dir was deleted while registry/metadata still
            // advertises them. Surfaces the same recovery hint as `ws list`.
            bail!(
                "MISSING: workspace '{}' worktree dir is gone from disk at {}\n  \
                 The CLI registry still references this workspace.\n  \
                 Fix: maw ws destroy {} --force\n  \
                 Then: maw ws list",
                ws_id,
                ws_path.display(),
                ws_id
            );
        }
        workspace_dirs.insert(ws_id.clone(), ws_path);
    }

    if target_updates_epoch {
        guard_unbound_sources_against_active_change_ancestry(&root, target_branch, workspaces)?;
    }

    // Check staleness
    let stale_sources = stale_merge_sources(workspaces)?;
    let is_stale = !stale_sources.is_empty();

    let primary_ws = &workspaces[0];
    let ws_info = CheckWorkspaceInfo {
        name: primary_ws.clone(),
        change_id: String::new(),
    };

    // bn-1huu: detect out-of-maw commits on trunk (epoch ref lagging the
    // branch tip). Only meaningful when this merge advances the epoch
    // (default-branch merge); change-branch merges don't track the epoch.
    let trunk_ahead = if target_updates_epoch {
        trunk_commits_ahead_of_epoch(&root, &branch_before_oid)
    } else {
        0
    };

    // bn-3eew: classify epoch/branch drift so --check can distinguish
    // "safe, will auto-absorb" from "needs coordination" instead of always
    // printing the same scolding NOTE. Best-effort: any classification
    // failure (or a check that doesn't touch the epoch) leaves this `None`,
    // and the text renderer falls back to the original neutral wording.
    let drift_classification = if target_updates_epoch {
        super::epoch_drift::classify_drift(&root, target_branch, &backend)
            .ok()
            .flatten()
            .map(|report| report.kind.slug().replace('_', "-"))
    } else {
        None
    };

    // bn-1xmk: surface (informational, never blocking) any uncommitted tracked
    // files in the trunk/default workspace. A dirty-trunk merge preserves and
    // replays these and pins a recovery snapshot; listing them here makes that
    // non-silent.
    let dirty_trunk_files = WorkspaceId::new(default_ws)
        .ok()
        .map(|id| backend.workspace_path(&id))
        .map(|p| list_dirty_trunk_tracked_files(&p))
        .unwrap_or_default();

    if is_stale {
        return Ok(CheckResult {
            ready: false,
            conflicts: Vec::new(),
            stale: true,
            workspace: ws_info,
            description: String::new(),
            trunk_ahead,
            dirty_trunk_files,
            drift_classification,
        });
    }

    // bn-qw4i: apply the same source-conflict precondition the real merge
    // applies. Without this, `--check` would report "Ready to merge" for a
    // workspace whose HEAD still carries unresolved structured conflicts,
    // and the actual merge would then refuse. `--check` must be a faithful
    // dry-run: same gates, same diagnostics. Mirror the `--force` bypass
    // semantics exactly (sidecar gate bypassable; HEAD-tree tripwire is
    // not).
    if let Err(err) =
        assert_sources_clean_for_merge(&root, workspaces, &workspace_dirs, force, target_workspace)
    {
        return Ok(CheckResult {
            ready: false,
            conflicts: vec![ConflictInfo {
                path: String::new(),
                reason: err.to_string(),
                sides: Vec::new(),
                line_start: None,
                line_end: None,
            }],
            stale: false,
            workspace: ws_info,
            description: String::new(),
            trunk_ahead,
            dirty_trunk_files,
            drift_classification,
        });
    }

    // Try a BUILD phase to detect conflicts (don't COMMIT)
    let manifold_dir =
        maw_core::model::layout::LayoutFlavor::detect_with_env(&root).manifold_dir(&root);
    let temp_check_dir = tempfile::Builder::new()
        .prefix("check-tmp-")
        .tempdir_in(&manifold_dir)
        .context("Failed to create temp dir for merge check")?;
    let check_dir = temp_check_dir.path().to_path_buf();

    // Run PREPARE in the temp dir
    let prepare_result = run_prepare_phase(&root, &check_dir, &sources, &workspace_dirs);

    // Clean up temp dir
    drop(temp_check_dir);

    match prepare_result {
        Ok(_frozen) => {
            // PREPARE succeeded, now try BUILD
            let temp_build_dir = tempfile::Builder::new()
                .prefix("build-tmp-")
                .tempdir_in(&manifold_dir)
                .context("Failed to create temp dir for build check")?;
            let build_dir = temp_build_dir.path().to_path_buf();
            let frozen = run_prepare_phase(&root, &build_dir, &sources, &workspace_dirs)
                .context("prepare phase failed for build check")?;

            let merge_base_epoch = if target_updates_epoch {
                frozen.epoch
            } else {
                EpochId::new(branch_before_oid.as_str()).map_err(|e| {
                    anyhow::anyhow!(
                        "invalid target branch base OID '{}': {e}",
                        branch_before_oid.as_str()
                    )
                })?
            };
            record_merge_target_context(
                &build_dir,
                target_branch,
                (!target_updates_epoch).then_some(&merge_base_epoch),
            )
            .context("failed to persist merge target context for check")?;

            let build_result = run_build_phase(&root, &build_dir, &backend);
            drop(temp_build_dir);

            match build_result {
                Ok(output) => {
                    let conflicts: Vec<ConflictInfo> = output
                        .conflicts
                        .iter()
                        .map(conflict_record_to_info)
                        .collect();
                    let ready = conflicts.is_empty();
                    Ok(CheckResult {
                        ready,
                        conflicts,
                        stale: false,
                        workspace: ws_info,
                        description: String::new(),
                        trunk_ahead,
                        dirty_trunk_files,
                        drift_classification,
                    })
                }
                Err(e) => Ok(CheckResult {
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
                    trunk_ahead,
                    dirty_trunk_files,
                    drift_classification,
                }),
            }
        }
        Err(e) => Ok(CheckResult {
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
            trunk_ahead,
            dirty_trunk_files,
            drift_classification,
        }),
    }
}

/// Output the check result in the requested format.
#[expect(
    clippy::single_match_else,
    reason = "JSON vs. human rendering reads clearly as a match; the human arm is long"
)]
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
                if result.trunk_ahead > 0 {
                    // bn-1huu / bn-3eew: the epoch lags the branch tip by
                    // out-of-maw commits (a direct `git commit`/`git pull`
                    // on trunk). Real merges already auto-absorb this when
                    // it's a safe fast-forward (`merge.auto_absorb_ff`,
                    // default true). Match the tone to that reality instead
                    // of always scolding: `ff-absorbable` is purely
                    // informational (nothing for the agent to do); anything
                    // else (`ff-blocked`, or classification unavailable) is
                    // a real warning that needs coordination before the
                    // epoch can safely move.
                    let n = result.trunk_ahead;
                    let plural = if n == 1 { "" } else { "s" };
                    match result.drift_classification.as_deref() {
                        Some("ff-absorbable") => {
                            println!(
                                "  NOTE: trunk is {n} commit{plural} ahead of the epoch (will be absorbed automatically when you merge)."
                            );
                        }
                        Some("ff-blocked") => {
                            println!(
                                "  WARNING: trunk is {n} commit{plural} ahead of the epoch, but an in-flight workspace touches the same paths — the epoch can't auto-advance safely."
                            );
                            println!(
                                "  To reconcile: resolve or merge the blocking workspace(s) first, then retry. Or force it: maw epoch sync / maw doctor --repair"
                            );
                        }
                        _ => {
                            // `diverged`, or classification unavailable
                            // (e.g. epoch ref unset, read error): fall back
                            // to the original neutral-but-cautious wording
                            // rather than guessing a tone.
                            println!(
                                "  NOTE: trunk has {n} commit{plural} not made through maw; the epoch is behind the branch tip."
                            );
                            println!(
                                "  The merge will absorb them into the epoch. To reconcile first: maw epoch sync"
                            );
                        }
                    }
                }
            } else if result.stale {
                println!(
                    "[BLOCKED] Workspace is behind the current epoch — another merge advanced repository state since this workspace was created."
                );
                println!(
                    "  Run `maw ws sync {}` to rebase onto the latest epoch, then retry the merge.",
                    result.workspace.name
                );
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

            // bn-1xmk: informational dirty-trunk notice (never blocking). The
            // merge will preserve and replay these files and pin a recovery
            // snapshot; surfacing them here removes the surprise.
            if !result.dirty_trunk_files.is_empty() {
                let n = result.dirty_trunk_files.len();
                let plural = if n == 1 { "" } else { "s" };
                println!(
                    "  NOTE: {n} uncommitted tracked trunk file{plural} will be preserved and replayed across the merge (a recovery snapshot is pinned):"
                );
                for f in &result.dirty_trunk_files {
                    println!("    {f}");
                }
            }
        }
    }

    if result.ready {
        Ok(())
    } else {
        bail!("merge check: {}", check_not_ready_reason(result))
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
#[expect(
    clippy::too_many_lines,
    reason = "plan command emits a complete dry-run artifact in one flow"
)]
pub fn plan_merge(
    workspaces: &[String],
    format: OutputFormat,
    target_workspace: &str,
    target_branch: &str,
    _target_change_id: Option<&str>,
    target_updates_epoch: bool,
) -> Result<()> {
    if workspaces.is_empty() {
        bail!("No workspaces specified for --plan");
    }

    let stale_sources = stale_merge_sources(workspaces)?;
    if !stale_sources.is_empty() {
        bail!("{}", stale_merge_block_message(&stale_sources));
    }

    let root = repo_root()?;
    let maw_config = MawConfig::load(&root)?;
    let default_ws = maw_config.default_workspace();
    let backend = get_backend()?;

    let branch_ref = format!("refs/heads/{target_branch}");
    let branch_before_oid = maw_core::refs::read_ref(&root, &branch_ref)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Target branch '{target_branch}' does not exist.\n  To fix: create the branch first or repair change metadata, then retry."
        )
    })?;

    if workspaces.iter().any(|ws| ws == target_workspace) {
        if target_workspace == default_ws {
            bail!(
                "Cannot plan a merge of the default workspace — it is the merge target, not a source."
            );
        }
        bail!(
            "Cannot plan target workspace '{target_workspace}' as a source — it is the merge destination."
        );
    }

    let manifold_dir =
        maw_core::model::layout::LayoutFlavor::detect_with_env(&root).manifold_dir(&root);
    let manifold_config = ManifoldConfig::load(&manifold_dir.join("config.toml"))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let sources = parse_workspace_ids(workspaces)?;
    validate_workspace_dirs(&sources, &backend)?;

    if target_updates_epoch {
        guard_unbound_sources_against_active_change_ancestry(&root, target_branch, workspaces)?;
    }

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

    let merge_base_epoch = if target_updates_epoch {
        frozen.epoch.clone()
    } else {
        EpochId::new(branch_before_oid.as_str()).map_err(|e| {
            anyhow::anyhow!(
                "invalid target branch base OID '{}': {e}",
                branch_before_oid.as_str()
            )
        })?
    };
    if let Err(e) = record_merge_target_context(
        &manifold_dir,
        target_branch,
        (!target_updates_epoch).then_some(&merge_base_epoch),
    ) {
        let _ = cleanup_plan_merge_state(&manifold_dir);
        bail!("failed to persist merge target context for plan: {e}");
    }

    let build_output = match run_build_phase(&root, &manifold_dir, &backend) {
        Ok(out) => out,
        Err(e) => {
            let _ = cleanup_plan_merge_state(&manifold_dir);
            bail!("BUILD phase failed: {e}");
        }
    };

    let merge_id = compute_merge_id(&merge_base_epoch, &sources, &frozen.heads);
    let driver_infos = build_driver_infos(&touched_paths, &manifold_config);
    let predicted_conflicts = build_predicted_conflicts(&build_output);
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
        epoch_before: merge_base_epoch.as_str().to_owned(),
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
    partition: &maw_core::merge::partition::PartitionResult,
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
fn build_predicted_conflicts(build_output: &BuildPhaseOutput) -> Vec<PredictedConflict> {
    build_output
        .conflicts
        .iter()
        .map(|conflict| {
            let mut sides: Vec<String> = conflict
                .sides
                .iter()
                .map(|s| workspace_display_name(&s.workspace_id))
                .collect();
            sides.sort();
            sides.dedup();
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
    patch_sets: &[maw_core::merge::types::PatchSet],
    frozen: &maw::merge::prepare::FrozenInputs,
    format: OutputFormat,
) {
    match write_plan_artifact(manifold_dir, plan) {
        Ok(path) => {
            if !matches!(format, OutputFormat::Json) {
                println!("Plan artifact: {}", path.display());
            }
        }
        Err(e) => tracing::warn!("Failed to write plan artifact: {e}"),
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
    let manifold_dir =
        maw_core::model::layout::LayoutFlavor::detect_with_env(&root).manifold_dir(&root);
    let temp_check_dir = tempfile::Builder::new()
        .prefix("conflicts-tmp-")
        .tempdir_in(&manifold_dir)
        .context("Failed to create temp dir for conflict check")?;
    let check_dir = temp_check_dir.path().to_path_buf();

    let sources: Vec<WorkspaceId> = workspaces
        .iter()
        .map(|ws| WorkspaceId::new(ws).map_err(|e| anyhow::anyhow!("{e}")))
        .collect::<Result<Vec<_>>>()?;

    // Check that all workspaces exist before running merge logic (bn-2axt)
    for ws_id in &sources {
        if !backend.exists(ws_id) {
            bail!(
                "Workspace '{ws_id}' does not exist\n  Check: maw ws list\n  Fix: maw ws create --from main {ws_id}"
            );
        }
    }

    let mut workspace_dirs = BTreeMap::new();
    for ws_id in &sources {
        workspace_dirs.insert(ws_id.clone(), backend.workspace_path(ws_id));
    }

    // PREPARE
    let prepare_result = run_prepare_phase(&root, &check_dir, &sources, &workspace_dirs);
    drop(temp_check_dir);

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
            let temp_build_dir = tempfile::Builder::new()
                .prefix("build-tmp-")
                .tempdir_in(&manifold_dir)
                .context("Failed to create temp dir for build phase")?;
            let build_dir = temp_build_dir.path().to_path_buf();
            let _ = run_prepare_phase(&root, &build_dir, &sources, &workspace_dirs);
            let result = run_build_phase(&root, &build_dir, &backend);
            drop(temp_build_dir);
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

    let has_conflicts = !build_output.conflicts.is_empty();

    // bn-3h90 follow-up: Even when the merge engine reports no conflicts,
    // a source workspace may have embedded conflict content committed into
    // its HEAD from a prior `sync --rebase` that the user hasn't finished
    // resolving. The merge engine treats marker text as ordinary content,
    // so it wouldn't flag these.
    //
    // bn-8zqz: this used to be an UNFILTERED worktree marker scan, which
    // both false-positived on legitimately-committed marker literals and
    // disagreed with the merge gate / `resolve --list` (different sources
    // of truth). It now reads the same effective conflict state the gate
    // reads — sidecar verified against reality, plus the bn-28d1
    // placeholder tripwire. A stale sidecar (manual resolution committed)
    // is cleared here as a side effect, so this command, `merge --check`,
    // and `resolve --list` immediately agree.
    let mut workspaces_with_markers: Vec<(String, Vec<std::path::PathBuf>)> = Vec::new();
    let conflict_flavor = maw_core::model::layout::LayoutFlavor::detect_with_env(&root);
    for ws_name in workspaces {
        let ws_path = conflict_flavor.workspace_path(&root, ws_name);
        let state = match super::conflict_state::effective_conflict_state(&root, ws_name, &ws_path)
        {
            Ok(state) => state,
            Err(e) => {
                tracing::warn!(
                    workspace = %ws_name,
                    error = %e,
                    "ws conflicts: could not verify effective conflict state"
                );
                continue;
            }
        };
        if state.cleared_stale_sidecar {
            eprintln!(
                "Workspace '{ws_name}': {}",
                super::conflict_state::STALE_CLEAR_NOTICE
            );
        }
        let unresolved = state.unresolved_paths();
        if !unresolved.is_empty() {
            workspaces_with_markers.push((ws_name.clone(), unresolved));
        }
    }

    if !has_conflicts && workspaces_with_markers.is_empty() {
        if format == OutputFormat::Json {
            let out = ConflictsOutput {
                status: "clean".to_string(),
                workspaces: workspaces.to_vec(),
                has_conflicts: false,
                conflict_count: 0,
                conflicts: vec![],
                message: format!(
                    "No conflicts found. {} workspace(s) can be merged cleanly.",
                    workspaces.len()
                ),
                to_fix: None,
            };
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            println!("No conflicts found.");
            println!(
                "{} workspace(s) can be merged cleanly: {}",
                workspaces.len(),
                workspaces.join(", ")
            );
            println!(
                "To merge: maw ws merge {} --into {}",
                workspaces.join(" "),
                default_ws
            );
        }
        return Ok(());
    }

    // Embedded-marker branch: merge engine is clean but at least one
    // workspace has committed conflict markers in its worktree.
    if !workspaces_with_markers.is_empty() && !has_conflicts {
        if format == OutputFormat::Json {
            let marker_msg = workspaces_with_markers
                .iter()
                .map(|(ws, files)| format!("{ws}: {} file(s)", files.len()))
                .collect::<Vec<_>>()
                .join(", ");
            let out = ConflictsOutput {
                status: "embedded_markers".to_string(),
                workspaces: workspaces.to_vec(),
                has_conflicts: true,
                conflict_count: workspaces_with_markers.iter().map(|(_, f)| f.len()).sum(),
                conflicts: vec![],
                message: format!(
                    "Merge engine reports clean, but embedded conflict markers \
                     detected in workspace HEADs: {marker_msg}"
                ),
                to_fix: Some(
                    "Strip markers from the affected files and commit the \
                     resolution, then retry. See `maw ws resolve <name> --list`."
                        .to_string(),
                ),
            };
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            println!(
                "WARNING: No merge conflicts detected, BUT the following workspace(s) \
                 have unresolved conflict markers committed into HEAD (probably from \
                 a prior `maw ws sync`):"
            );
            for (ws_name, marker_files) in &workspaces_with_markers {
                println!("  {ws_name}:");
                for f in marker_files {
                    println!("    - {}", f.display());
                }
            }
            println!();
            println!(
                "To fix: open each file, remove the <<<<<<<, =======, and >>>>>>> \
                 markers (keeping the sides you want), then:"
            );
            for (ws_name, _) in &workspaces_with_markers {
                println!(
                    "  maw exec {ws_name} -- git add -A && maw exec {ws_name} -- git commit -m 'resolve conflicts'"
                );
            }
            println!();
            println!("Then retry the merge.");
        }
        bail!("embedded conflict markers detected in workspace HEAD(s)");
    }

    // Assign terseid IDs to conflicts
    let conflicts_with_ids = assign_conflict_ids(&build_output.conflicts);

    if format == OutputFormat::Json {
        let conflict_jsons: Vec<ConflictJson> = conflicts_with_ids
            .iter()
            .map(|c| conflict_record_to_json_with_id(&c.record, Some(&c.id), &c.atom_ids))
            .collect();
        let ws_args = workspaces.join(" ");
        let resolve_default_ws = workspaces.first().map_or("WORKSPACE", |s| s.as_str());
        let resolve_args: Vec<String> = conflicts_with_ids
            .iter()
            .map(|c| format!("--resolve {}={resolve_default_ws}", c.id))
            .collect();
        let retry_message = conflict_retry_message(default_ws);
        let to_fix = format!(
            "maw ws merge {ws_args} --into {} {} --message {retry_message}",
            default_ws,
            resolve_args.join(" ")
        );
        let out = ConflictsOutput {
            status: "conflict".to_string(),
            workspaces: workspaces.to_vec(),
            has_conflicts: true,
            conflict_count: conflict_jsons.len(),
            conflicts: conflict_jsons,
            message: format!(
                "{} conflict(s) found in {} workspace(s). Resolve them before merging.",
                conflicts_with_ids.len(),
                workspaces.len()
            ),
            to_fix: Some(to_fix),
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        // Reuse the same format as merge conflict output
        print_conflict_report(&conflicts_with_ids, workspaces, default_ws);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Preview merge
// ---------------------------------------------------------------------------

/// Preview what a merge would do without creating any commits.
#[expect(
    clippy::too_many_lines,
    reason = "preview command renders several merge states together"
)]
fn preview_merge(
    workspaces: &[String],
    root: &Path,
    into: &str,
    format: OutputFormat,
) -> Result<()> {
    let backend = get_backend()?;

    let mut workspace_changes = Vec::new();
    let mut workspace_files: Vec<(String, Vec<PathBuf>)> = Vec::new();

    for ws_name in workspaces {
        let ws_id = WorkspaceId::new(ws_name)
            .map_err(|e| anyhow::anyhow!("invalid workspace name '{ws_name}': {e}"))?;

        if !backend.exists(&ws_id) {
            let ws_path = backend.workspace_path(&ws_id);
            bail!(
                "Workspace '{}' does not exist at {}\n  \
                 Check available workspaces: maw ws list",
                ws_id,
                ws_path.display()
            );
        }

        match backend.snapshot(&ws_id) {
            Ok(snapshot) => {
                let added: Vec<String> = snapshot
                    .added
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect();
                let modified: Vec<String> = snapshot
                    .modified
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect();
                let deleted: Vec<String> = snapshot
                    .deleted
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect();
                let files: Vec<PathBuf> = snapshot.all_changed().into_iter().cloned().collect();

                workspace_files.push((ws_name.clone(), files));
                workspace_changes.push(DryRunWorkspaceChanges {
                    workspace: ws_name.clone(),
                    added,
                    modified,
                    deleted,
                    change_count: snapshot.change_count(),
                    error: None,
                });
            }
            Err(e) => {
                bail!("Could not preview workspace '{ws_name}': {e}");
            }
        }
    }

    let mut conflict_paths: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (ws_name, files) in &workspace_files {
        for path in files {
            let path_str = path.display().to_string();
            conflict_paths
                .entry(path_str)
                .or_default()
                .insert(ws_name.clone());
        }
    }
    let potential_conflicts: Vec<DryRunPotentialConflict> = conflict_paths
        .into_iter()
        .filter_map(|(path, workspaces)| {
            if workspaces.len() > 1 {
                Some(DryRunPotentialConflict {
                    path,
                    workspaces: workspaces.into_iter().collect(),
                })
            } else {
                None
            }
        })
        .collect();

    if format == OutputFormat::Json {
        let out = MergeDryRunOutput {
            status: "dry-run".to_string(),
            dry_run: true,
            workspaces: workspaces.to_vec(),
            into: into.to_string(),
            workspace_changes,
            potential_conflicts,
            message: format!(
                "Previewed merge of {} workspace(s); no commits were created.",
                workspaces.len()
            ),
            to_fix: format!("maw ws merge {} --into {into}", workspaces.join(" ")),
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

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

    for ws in &workspace_changes {
        let ws_name = &ws.workspace;
        println!("--- {ws_name} ---");
        if let Some(err) = &ws.error {
            println!("  {err}");
            println!();
            continue;
        }
        if ws.change_count == 0 {
            println!("  (no changes)");
            println!();
            continue;
        }
        for path in &ws.added {
            println!("  A {path}");
        }
        for path in &ws.modified {
            println!("  M {path}");
        }
        for path in &ws.deleted {
            println!("  D {path}");
        }
        println!("  {} file(s) changed", ws.change_count);
        println!();
    }

    // Check for potential conflicts (files modified in multiple workspaces)
    if workspaces.len() > 1 {
        println!("=== Potential Conflicts ===");
        println!();

        if potential_conflicts.is_empty() {
            println!("  (no overlapping changes detected)");
        } else {
            for conflict in &potential_conflicts {
                println!(
                    "  ! {} - modified in {}",
                    conflict.path,
                    conflict.workspaces.join(", ")
                );
            }
            println!();
            println!("  Note: Overlapping files will be resolved via diff3 where possible.");
        }
        println!();
    }

    println!("=== Summary ===");
    println!();
    println!("To perform this merge, run without --dry-run:");
    println!("  maw ws merge {} --into {into}", workspaces.join(" "));
    println!();

    let _ = root; // used implicitly via get_backend()
    Ok(())
}

// ---------------------------------------------------------------------------
// Merge options
// ---------------------------------------------------------------------------

/// Options controlling merge behavior.
#[expect(
    clippy::struct_excessive_bools,
    reason = "CLI options are independent flags"
)]
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
    /// Merge destination workspace name.
    pub target_workspace: &'a str,
    /// Branch to update for the merge destination.
    pub target_branch: &'a str,
    /// Optional change id associated with the merge destination.
    pub target_change_id: Option<&'a str>,
    /// Whether this target should advance the global epoch in addition to the target branch.
    pub target_updates_epoch: bool,
    /// Inline conflict resolutions. Each entry is `ID=STRATEGY`.
    pub resolve: Vec<String>,
    /// Resolve all remaining conflicts to this workspace name.
    /// Individual `--resolve` flags take precedence.
    pub resolve_all: Option<String>,
    /// Show verbose output (full recovery surface details on destroy).
    pub verbose: bool,
    /// Bypass the conflict-marker worktree scan (bn-3h90, bn-2r57).
    ///
    /// Normally `maw ws merge` refuses to proceed if a source workspace
    /// has conflict markers in its worktree (detected by
    /// `find_conflicted_files`). `--force` lets the user override when
    /// markers are false positives (e.g., a test fixture with `<<<<<<<`).
    pub force: bool,
    /// Per-invocation override for `merge.auto_rebase_siblings` (bn-3vf5).
    ///
    /// `Some(true)` forces auto-rebase on regardless of config.
    /// `Some(false)` forces it off (matches `--no-auto-rebase`).
    /// `None` defers to `MawConfig::merge.auto_rebase_siblings`.
    pub auto_rebase_siblings: Option<bool>,
}

// ---------------------------------------------------------------------------
// bn-qw4i: shared merge precondition gate
// ---------------------------------------------------------------------------

/// Refuse to proceed if any source workspace is in an unresolved conflict
/// state.
///
/// Runs the same two gates the real merge applies, in the same order:
///
/// 1. **Sidecar gate (bn-m6ad / bn-3pgl / bn-3oau)** — refuses when the
///    structured `conflict-tree.json` (or legacy `rebase-conflicts.json`)
///    sidecar has any entries. Bypassable by `force = true`.
/// 2. **Tamper-resistance tripwire (bn-28d1)** — refuses when any source
///    workspace's HEAD tree contains a tool-authored conflict placeholder
///    blob. Not bypassable by `force` — committing such a blob into the
///    target branch would corrupt it.
///
/// Both `merge::merge` (the real merge) and
/// `merge::check_merge_result_for_target` (the `--check` dry-run) call this
/// helper so the two paths agree on what "ready to merge" means (bn-qw4i).
///
/// bn-8zqz: both gates now read from the shared effective-conflict-state
/// helper (`super::conflict_state`), which verifies the sidecars against
/// reality. A sidecar whose every recorded conflict was manually resolved
/// and COMMITTED (no markers on the recorded paths, no placeholder blobs in
/// HEAD) is stale metadata: it is auto-cleared on the spot and the gate
/// proceeds — no follow-up `maw ws sync` required.
fn assert_sources_clean_for_merge(
    root: &Path,
    _sources: &[String],
    workspace_dirs: &BTreeMap<WorkspaceId, PathBuf>,
    force: bool,
    into_target: &str,
) -> Result<()> {
    for (ws_id, ws_path) in workspace_dirs {
        let ws_name = ws_id.as_str();
        let state = super::conflict_state::effective_conflict_state(root, ws_name, ws_path)?;

        if state.cleared_stale_sidecar {
            // stderr: the gate also runs under JSON-emitting flows whose
            // stdout must stay parseable.
            eprintln!(
                "Workspace '{ws_name}': {}",
                super::conflict_state::STALE_CLEAR_NOTICE
            );
        }

        // Gate 1 (bn-m6ad/bn-3pgl/bn-3oau): recorded sidecar conflicts with
        // remaining evidence. Bypassable by --force.
        if !force && !state.recorded_paths.is_empty() {
            let file_list = state
                .recorded_paths
                .iter()
                .map(|p| format!("  - {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "Workspace '{ws_name}' has {} unresolved conflict(s):\n\
                 {file_list}\n  \
                 Resolve them: maw ws resolve {ws_name} --list, then --keep <side>\n  \
                 To force merge anyway: maw ws merge {ws_name} --into {into_target} --force",
                state.recorded_paths.len()
            );
        }

        // Gate 2 (bn-28d1): tool-authored placeholder blobs in HEAD. Not
        // bypassable by --force — committing such a blob into the target
        // branch would corrupt it.
        //
        // bn-1etl: split the flagged paths into "still has conflict markers"
        // (genuine unresolved conflict / tampered sidecar — keep the
        // tamper-flavored message) vs "header-only leftover" (the user
        // hand-resolved the `<<<<<<<` markers but forgot to delete the
        // leading `#` header lines before committing — point at that exact
        // fix instead). Both remain hard-blocking and not bypassable by
        // --force.
        if !state.placeholder_paths.is_empty() {
            let (marker_paths, header_only_paths) =
                classify_placeholder_paths(root, ws_path, &state.placeholder_paths);

            if !marker_paths.is_empty() {
                let file_list = marker_paths
                    .iter()
                    .map(|p| format!("  - {}", p.display()))
                    .collect::<Vec<_>>()
                    .join("\n");
                bail!(
                    "Workspace '{ws_name}' has {} path(s) whose HEAD blob contains \
                     tool-authored conflict placeholders:\n\
                     {file_list}\n  \
                     Resolve them: maw ws resolve {ws_name} --list, then --keep <side>\n  \
                     Possible cause: the conflict sidecar was deleted or corrupted. \
                     This check cannot be bypassed by --force because merging placeholder \
                     blobs would corrupt the target branch.",
                    marker_paths.len()
                );
            }

            if !header_only_paths.is_empty() {
                let file_list = header_only_paths
                    .iter()
                    .map(|p| format!("  - {}", p.display()))
                    .collect::<Vec<_>>()
                    .join("\n");
                bail!(
                    "Workspace '{ws_name}' has {} path(s) that appear manually resolved \
                     but still begin with the maw conflict header:\n\
                     {file_list}\n  \
                     Delete the leading '#' header lines (\"# structured conflict at ...\", \
                     \"# base blob: ...\", \"# side ... blob: ...\", or \"# BINARY CONFLICT \
                     at ...\"), commit, and re-run the merge.\n  \
                     This check cannot be bypassed by --force because merging a blob that \
                     still carries the maw conflict header would corrupt the target branch.",
                    header_only_paths.len()
                );
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// bn-1etl: Gate 2 message classification
// ---------------------------------------------------------------------------

/// Classify each of `placeholder_paths` (already known to start with a
/// [`maw_core::merge::materialize::TOOL_PLACEHOLDER_PREFIXES`] entry — see
/// [`find_tool_placeholder_blobs`]) into two buckets, by re-reading the full
/// blob at the workspace's current HEAD and checking whether it still
/// contains a `<<<<<<<` conflict-marker line:
///
/// * `.0` — markers still present (or the blob could not be re-verified):
///   genuine unresolved conflict / tampered sidecar.
/// * `.1` — markers gone, only the header comment lines remain: the user
///   hand-resolved the conflict but forgot to delete the header.
///
/// Fails closed: if HEAD cannot be resolved, the repo cannot be opened, or a
/// specific blob cannot be re-read, the affected path(s) are placed in the
/// marker bucket (`.0`) rather than silently downgrading to the softer
/// message — a classification bug must never weaken the tripwire.
fn classify_placeholder_paths(
    root: &Path,
    ws_path: &Path,
    placeholder_paths: &[PathBuf],
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let classified: Option<Vec<(PathBuf, bool)>> = (|| {
        let head_oid_str = resolve_workspace_head_oid(ws_path).ok()?;
        let head_oid: maw_git::GitOid = head_oid_str.parse().ok()?;
        let repo = maw_git::GixRepo::open(root).ok()?;
        let entries = placeholder_paths
            .iter()
            .map(|p| {
                let has_markers = repo
                    .read_blob_at_path(head_oid, &p.to_string_lossy())
                    .ok()
                    .flatten()
                    .is_none_or(|(_, _, content)| {
                        maw_core::merge::materialize::placeholder_blob_has_markers(&content)
                    });
                (p.clone(), has_markers)
            })
            .collect::<Vec<_>>();
        Some(entries)
    })();

    let Some(classified) = classified else {
        // Could not verify anything (HEAD unresolved / repo unreadable) —
        // fail closed: every path stays in the tamper-flavored bucket.
        return (placeholder_paths.to_vec(), Vec::new());
    };

    let mut marker_paths = Vec::new();
    let mut header_only_paths = Vec::new();
    for (path, has_markers) in classified {
        if has_markers {
            marker_paths.push(path);
        } else {
            header_only_paths.push(path);
        }
    }
    (marker_paths, header_only_paths)
}

// ---------------------------------------------------------------------------
// bn-28d1: tamper-resistance tripwire for the merge gate
// ---------------------------------------------------------------------------

/// Walk the tree at `tree_oid` recursively and return the paths of any blobs
/// whose first bytes match a
/// [`TOOL_PLACEHOLDER_PREFIXES`](maw_core::merge::materialize::TOOL_PLACEHOLDER_PREFIXES)
/// entry.
///
/// These byte sequences are written exclusively by `materialize.rs` when it
/// projects an unresolved conflict into a committable blob — legitimate source
/// code never starts with them. If the sidecar check has already cleared the
/// workspace but the HEAD tree still carries a placeholder blob, it means
/// either:
///
/// * the sidecar was deleted or tampered with while the HEAD was not
///   re-materialized (data corruption), or
/// * a buggy flow wrote a placeholder blob without registering it in the
///   sidecar (logic bug).
///
/// Either way the merge must refuse, because committing those bytes into the
/// default branch silently poisons the resulting tree.
///
/// The scan reads only the first `SNIFF` bytes of each blob, so it stays
/// cheap even on large trees. This is a one-shot gate check, not a hot path.
pub fn find_tool_placeholder_blobs(
    repo: &maw_git::GixRepo,
    tree_oid: maw_git::GitOid,
) -> Result<Vec<PathBuf>> {
    /// Only sniff enough bytes to decide whether the prefix matches. The
    /// longest prefix in `TOOL_PLACEHOLDER_PREFIXES` is a handful of bytes;
    /// 64 is comfortably more than enough and lets the list grow without
    /// revisiting this constant.
    const SNIFF: usize = 64;

    fn walk(
        repo: &maw_git::GixRepo,
        tree_oid: maw_git::GitOid,
        prefix: &Path,
        out: &mut Vec<PathBuf>,
    ) -> Result<()> {
        let entries = repo
            .read_tree(tree_oid)
            .map_err(|e| anyhow::anyhow!("read_tree({tree_oid}) failed: {e}"))?;
        for entry in entries {
            let entry_path = prefix.join(&entry.name);
            match entry.mode {
                maw_git::EntryMode::Tree => {
                    walk(repo, entry.oid, &entry_path, out)?;
                }
                maw_git::EntryMode::Blob | maw_git::EntryMode::BlobExecutable => {
                    // Read the blob and check only the first SNIFF bytes.
                    // We read the whole blob because GitRepo has no
                    // bulk-prefix-read — acceptable at the gate since this
                    // is not a hot path. If profiling shows pain, switch
                    // to `git cat-file --batch-check` or a gix streaming
                    // reader here.
                    let content = repo
                        .read_blob(entry.oid)
                        .map_err(|e| anyhow::anyhow!("read_blob({}) failed: {e}", entry.oid))?;
                    let head = &content[..content.len().min(SNIFF)];
                    if maw_core::merge::materialize::is_tool_placeholder_blob(head) {
                        out.push(entry_path);
                    }
                }
                // Symlinks, submodules — their blob content isn't subject
                // to the text-conflict placeholder format, so skip.
                maw_git::EntryMode::Link | maw_git::EntryMode::Commit => {}
            }
        }
        Ok(())
    }

    let mut out = Vec::new();
    walk(repo, tree_oid, Path::new(""), &mut out)?;
    out.sort();
    Ok(out)
}

// ---------------------------------------------------------------------------
// Fast-forward absorb (bn-11ip)
// ---------------------------------------------------------------------------

/// Outcome of [`reconcile_epoch_with_branch`].
#[allow(dead_code)] // Variant fields are documentary; callers only need the success/failure split.
enum FfReconcile {
    /// Epoch already matches branch; nothing to do.
    InSync,
    /// Epoch was strictly behind branch and the FF range was absorbed; epoch
    /// has been advanced to `new_epoch`.
    Absorbed { new_epoch: GitOid, count: usize },
}

/// Bridge between the existing pre/post-PREPARE divergence checks and the
/// FF-absorb safety predicate (`super::ff_absorb`).
///
/// On entry, `epoch_oid` and `branch_oid` are the OIDs the caller has just
/// observed. The function:
///
/// 1. Returns [`FfReconcile::InSync`] immediately if they match.
/// 2. Bails with the legacy "diverged" error (augmented with the affected
///    workspace list when relevant) if (a) `auto_absorb_ff` is disabled,
///    (b) the divergence is not a pure FF, or (c) a non-target workspace
///    has touched paths that intersect the FF range.
/// 3. Otherwise advances `refs/manifold/epoch/current` from `epoch_oid` to
///    `branch_oid` and returns [`FfReconcile::Absorbed`] with the count of
///    upstream commits in the range.
///
/// `target_workspace_name` is excluded from the safety check — it is the
/// merge target, not a source whose interpretation could change.
#[expect(
    clippy::too_many_lines,
    reason = "linear FF-absorb pipeline; splitting hides the staged sequence"
)]
fn reconcile_epoch_with_branch(
    root: &Path,
    branch: &str,
    target_workspace_name: &str,
    epoch_oid: &GitOid,
    branch_oid: &GitOid,
    auto_absorb_ff: bool,
) -> Result<FfReconcile> {
    if epoch_oid.as_str() == branch_oid.as_str() {
        return Ok(FfReconcile::InSync);
    }

    let bail_diverged = |affected: &[String]| -> Result<FfReconcile> {
        let base = format!(
            "Target branch '{branch}' has diverged from the current epoch.\n\
             \n  Branch:  {}\n  Epoch:   {}\n\
             \n  Direct commits were made to {target_workspace_name} outside of maw.\n\
             To fix: run `maw epoch sync` to absorb them into the epoch, then retry.",
            &branch_oid.as_str()[..12],
            &epoch_oid.as_str()[..12]
        );
        if affected.is_empty() {
            bail!("{base}");
        }
        bail!("{base}\n  Affected workspace(s): {}", affected.join(", "));
    };

    if !auto_absorb_ff {
        return bail_diverged(&[]);
    }

    let repo = super::ff_absorb::open_repo(root)?;
    let epoch_git: maw_git::GitOid = epoch_oid
        .as_str()
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid epoch OID '{}': {e}", epoch_oid.as_str()))?;
    let branch_git: maw_git::GitOid = branch_oid
        .as_str()
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid branch OID '{}': {e}", branch_oid.as_str()))?;

    if !super::ff_absorb::is_strict_ancestor(&repo, &epoch_git, &branch_git)? {
        // Fork divergence (or branch is an ancestor of epoch). Nothing to
        // absorb — keep the original error message verbatim.
        return bail_diverged(&[]);
    }

    let ff_paths = super::ff_absorb::compute_ff_changed_paths(&repo, &epoch_git, &branch_git)?;

    let backend = get_backend()?;
    let workspaces_info = backend
        .list()
        .map_err(|e| anyhow::anyhow!("failed to list workspaces: {e}"))?;

    let mut ws_touched: Vec<super::ff_absorb::WorkspaceTouchedPaths> = Vec::new();
    for info in workspaces_info {
        let name = info.id.as_str();
        if name == target_workspace_name {
            // Target's committed paths between base_epoch and HEAD are the FF
            // range itself when the user committed directly on the target's
            // branch — including them makes the predicate tautologically
            // self-block. What matters for safety is whether the target's
            // *uncommitted* edits would be clobbered by the FF-range checkout
            // in `sync_target_worktree_to_epoch`. Check only those.
            let target_path = maw_core::model::layout::LayoutFlavor::detect_with_env(root)
                .default_target_path(root, target_workspace_name);
            let dirty = dirty_paths_in_workspace(&target_path);
            if !dirty.is_empty() {
                ws_touched.push(super::ff_absorb::WorkspaceTouchedPaths {
                    name: target_workspace_name.to_owned(),
                    paths: dirty,
                });
            }
            continue;
        }
        let touched = super::touched::collect_touched_workspace(&backend, &info.id)?;
        ws_touched.push(super::ff_absorb::WorkspaceTouchedPaths {
            name: touched.workspace,
            paths: touched.touched_paths.into_iter().collect(),
        });
    }

    match super::ff_absorb::evaluate_ff_safety(&ff_paths, &ws_touched) {
        super::ff_absorb::FfAbsorbDecision::Blocked {
            affected_workspaces,
        } => bail_diverged(&affected_workspaces),
        super::ff_absorb::FfAbsorbDecision::Safe => {
            let count = repo
                .walk_commits(epoch_git, branch_git, false)
                .map_or(0, |walk| walk.len());
            maw_core::refs::write_epoch_current(root, branch_oid)
                .map_err(|e| anyhow::anyhow!("failed to advance epoch ref: {e}"))?;

            // Safety predicate guarantees no in-flight workspace has touched
            // a file in the FF range. To keep
            // `is_stale = base_epoch == current_epoch` consistent, bump each
            // workspace's per-workspace epoch ref to the new tip; otherwise
            // the next gate (`stale_merge_sources`) would block the merge we
            // just unblocked.
            //
            // Each workspace's worktree also needs to be FF'd for the
            // absorbed paths only — the merge engine snapshots dirty files
            // by diffing the worktree against the *current* epoch, and a
            // workspace that still holds the pre-absorb content for an FF
            // path would look like it is "reverting" the upstream change.
            // The predicate makes this safe: dirty paths (if any) are
            // disjoint from `ff_paths`, so per-path `git checkout` cannot
            // clobber local edits.
            for ws in &ws_touched {
                let epoch_ref = maw_core::refs::workspace_epoch_ref(&ws.name);
                if let Err(e) = maw_core::refs::write_ref(root, &epoch_ref, branch_oid) {
                    tracing::warn!(
                        workspace = %ws.name,
                        error = %e,
                        "failed to advance workspace epoch ref after FF absorb"
                    );
                }
                let ws_path = maw_core::model::layout::LayoutFlavor::detect_with_env(root)
                    .workspace_path(root, &ws.name);
                sync_ff_paths_in_worktree(&ws_path, &ws.name, branch_oid, &ff_paths);
            }

            // Also advance the target's per-workspace epoch ref. Otherwise
            // the merge's snapshot-replay anchor logic uses the stale ref,
            // treats the FF range as "uncommitted local edits", and double-
            // applies it onto the merge result — silent corruption (bn-3r8s).
            // The non-target loop above advanced sibling refs; the target
            // needs the same treatment.
            let target_epoch_ref = maw_core::refs::workspace_epoch_ref(target_workspace_name);
            if let Err(e) = maw_core::refs::write_ref(root, &target_epoch_ref, branch_oid) {
                tracing::warn!(
                    workspace = %target_workspace_name,
                    error = %e,
                    "failed to advance target workspace epoch ref after FF absorb"
                );
            }

            // Fast-forward the target workspace's worktree to the new branch
            // tip. Without this, the merge BUILD/COMMIT pipeline would later
            // diff the target worktree against the (post-absorb) epoch,
            // observe the FF range as a "missing" delta, and replay it as a
            // snapshot — overwriting the legitimate upstream content. Safety
            // for this is guaranteed by the predicate: the target's dirty
            // paths (if any) were already proven disjoint from the FF range.
            sync_target_worktree_to_epoch(
                &maw_core::model::layout::LayoutFlavor::detect_with_env(root)
                    .default_target_path(root, target_workspace_name),
                target_workspace_name,
                branch_oid,
            );

            eprintln!(
                "Absorbed {count} upstream commit(s) into epoch ({}..{}).",
                &epoch_oid.as_str()[..12],
                &branch_oid.as_str()[..12]
            );
            Ok(FfReconcile::Absorbed {
                new_epoch: branch_oid.clone(),
                count,
            })
        }
    }
}

/// Paths in `ws_path`'s worktree whose content differs from the index/HEAD
/// (modified, added, deleted, or untracked-but-not-ignored).
///
/// Used by the FF-absorb safety check to identify paths that a hard checkout
/// of the FF range would clobber. Returns an empty set on any git error so
/// callers fail closed (treat as "no dirty paths to worry about" — the worst
/// case is the absorb proceeds and a downstream PREPARE-phase dirty check
/// catches genuinely dirty state).
fn dirty_paths_in_workspace(ws_path: &Path) -> std::collections::BTreeSet<PathBuf> {
    use std::collections::BTreeSet;

    let Ok(repo) = maw_git::GixRepo::open(ws_path) else {
        return BTreeSet::new();
    };
    let Ok(entries) = repo.status() else {
        return BTreeSet::new();
    };
    let mut paths = BTreeSet::new();
    for entry in entries {
        // Modified/Added/Deleted/Untracked/Renamed all flag the path as dirty
        // for the FF-absorb safety predicate. Renames only surface the new
        // path here; this loses the legacy "include old name" behaviour, but
        // FF-absorb only ever cares about the *current* worktree state — a
        // rename's old path is no longer present and so cannot collide with
        // the absorbed range.
        paths.insert(PathBuf::from(entry.path));
    }
    paths
}

/// Materialize one tree blob into the worktree **preserving its git mode**.
///
/// FF-absorb previously materialized every path via `std::fs::write`, which
/// always produces a `0644` regular file. That (a) drops the executable bit
/// for `100755` entries and (b) turns a `120000` symlink entry into a
/// regular file whose contents are the raw link target — the exact
/// symlink-corruption class already fixed for `stash_apply`. `git checkout
/// -- <path>` / `git reset --keep` (the commands this code replaced)
/// materialize each path with its recorded mode, so we must too. The mode
/// comes from [`maw_git::GixRepo::read_blob_at_path`].
#[cfg(unix)]
fn ff_materialize_blob(
    full: &Path,
    mode: maw_git::EntryMode,
    content: &[u8],
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    match mode {
        maw_git::EntryMode::Link => {
            let target = std::str::from_utf8(content)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            // symlink(2) fails with EEXIST if anything is already there
            // (regular file from the old buggy write, or a stale link).
            if full.symlink_metadata().is_ok() {
                std::fs::remove_file(full)?;
            }
            std::os::unix::fs::symlink(target, full)
        }
        maw_git::EntryMode::Blob | maw_git::EntryMode::BlobExecutable => {
            // If the destination is currently a symlink, `fs::write` would
            // follow it and clobber the link's target file. Replace it.
            if full
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
            {
                std::fs::remove_file(full)?;
            }
            std::fs::write(full, content)?;
            let bits = if mode == maw_git::EntryMode::BlobExecutable {
                0o755
            } else {
                0o644
            };
            std::fs::set_permissions(full, std::fs::Permissions::from_mode(bits))
        }
        // Not file paths in a name-status / FF-diff set; nothing to write.
        maw_git::EntryMode::Tree | maw_git::EntryMode::Commit => Ok(()),
    }
}

#[cfg(not(unix))]
fn ff_materialize_blob(
    full: &Path,
    _mode: maw_git::EntryMode,
    content: &[u8],
) -> std::io::Result<()> {
    std::fs::write(full, content)
}

/// Materialize (or delete) one FF-absorbed path in `ws_path` from
/// `target_git`'s tree, preserving its git mode. Best-effort: every failure
/// is logged and swallowed (the merge re-snapshots before BUILD).
fn ff_apply_one_path(
    ws_repo: &maw_git::GixRepo,
    ws_name: &str,
    ws_path: &Path,
    target_git: maw_git::GitOid,
    rel: &Path,
) {
    let full = ws_path.join(rel);
    let Some(rel_str) = rel.to_str() else {
        tracing::warn!(
            workspace = %ws_name,
            path = %rel.display(),
            "FF absorb: skipping non-UTF-8 path"
        );
        return;
    };
    // read_blob_at_path yields the entry mode so we can materialize
    // symlinks and the executable bit faithfully — a plain `fs::write`
    // corrupts both (see `ff_materialize_blob`).
    match ws_repo.read_blob_at_path(target_git, rel_str) {
        Ok(Some((mode, _oid, content))) => {
            if let Some(parent) = full.parent()
                && let Err(e) = std::fs::create_dir_all(parent)
            {
                tracing::warn!(
                    workspace = %ws_name,
                    path = %rel.display(),
                    error = %e,
                    "FF absorb: mkdir failed"
                );
                return;
            }
            if let Err(e) = ff_materialize_blob(&full, mode, &content) {
                tracing::warn!(
                    workspace = %ws_name,
                    path = %rel.display(),
                    error = %e,
                    "FF absorb: write failed"
                );
            }
        }
        Ok(None) => {
            // Path was deleted at target_oid; remove from worktree.
            // Use symlink_metadata().is_ok() (not exists(), which follows
            // symlinks and reports false for a *dangling* link): the
            // `git reset --keep` / `git checkout` this replaced removed a
            // stale (possibly broken) symlink, and leaving one behind lets
            // the next merge snapshot re-inject it as an untracked add,
            // undoing the epoch deletion. Matches `stash_apply` /
            // `ff_materialize_blob`.
            if full.symlink_metadata().is_ok()
                && let Err(e) = std::fs::remove_file(&full)
            {
                tracing::warn!(
                    workspace = %ws_name,
                    path = %rel.display(),
                    error = %e,
                    "FF absorb: unlink failed"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                workspace = %ws_name,
                path = %rel.display(),
                error = %e,
                "FF absorb: read blob at target failed"
            );
        }
    }
}

/// Best-effort fast-forward of the merge target's worktree to a new commit
/// after an FF absorb.
///
/// On entry the worktree is detached at the pre-absorb epoch and may have
/// uncommitted edits to paths the FF-absorb safety predicate has already
/// proven disjoint from the FF range. Use `git checkout <oid> -- <path>` for
/// every absorbed path to update only those paths, then move HEAD to the new
/// commit so the workspace tracks the absorbed tip. Failures are logged but
/// non-fatal: the merge can still proceed because the merge engine
/// re-snapshots the target before BUILD.
/// Update a non-target workspace's worktree to reflect an absorbed FF range.
///
/// Two coordinated updates happen:
///
/// 1. `git checkout <oid> -- <ff_paths>` materialises the absorbed content
///    for every path in the FF range. The safety predicate guarantees the
///    workspace has not edited any of these paths, so the checkout is
///    non-destructive.
/// 2. The worktree's `HEAD` is rewritten to point at `target_oid`, leaving
///    the index reset to match. Without this step, downstream merge
///    helpers that compare workspace `HEAD` to the (now-advanced) epoch
///    would treat the workspace as having committed work — and read blobs
///    from `HEAD:<path>` rather than the working copy, dropping
///    uncommitted edits.
///
/// Failures are logged but non-fatal; the merge re-snapshots before BUILD
/// and any drift will surface as a normal merge artefact rather than data
/// loss.
fn sync_ff_paths_in_worktree(
    ws_path: &Path,
    ws_name: &str,
    target_oid: &GitOid,
    ff_paths: &std::collections::BTreeSet<PathBuf>,
) {
    if !ws_path.exists() {
        return;
    }
    let oid = target_oid.as_str();

    // Open the workspace as its own gix repo (per-worktree git dir).
    let ws_repo = match maw_git::GixRepo::open(ws_path) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                workspace = %ws_name,
                error = %e,
                "failed to open workspace repo during FF absorb"
            );
            return;
        }
    };

    // maw-git OID for blob reads (the param is maw-core's String-backed
    // GitOid; read_blob_at_path wants maw-git's byte-backed one).
    let target_git: maw_git::GitOid = match oid.parse() {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(
                workspace = %ws_name,
                error = %e,
                "invalid target OID during FF absorb"
            );
            return;
        }
    };

    if !ff_paths.is_empty() {
        // Materialize each FF-absorbed path from the target tree. The
        // pre-FF safety predicate proved these paths are not user-edited,
        // so overwriting them is non-destructive.
        for rel in ff_paths {
            ff_apply_one_path(&ws_repo, ws_name, ws_path, target_git, rel);
        }
    }

    // Move HEAD to the new epoch without touching the working tree (so
    // uncommitted edits in disjoint paths survive). Mirrors the ANCHOR
    // step of `update_default_workspace`: write the raw OID to the
    // worktree's HEAD file directly, then unstage_all() to align the index.
    let head_path = ws_repo.git_dir().join("HEAD");
    if let Err(e) = std::fs::write(&head_path, format!("{oid}\n")) {
        tracing::warn!(
            workspace = %ws_name,
            path = %head_path.display(),
            error = %e,
            "failed to detach worktree HEAD during FF absorb"
        );
        return;
    }

    // Re-open after HEAD rewrite so unstage_all() sees the new HEAD.
    let ws_repo_post = match maw_git::GixRepo::open(ws_path) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                workspace = %ws_name,
                error = %e,
                "failed to re-open workspace repo after FF HEAD rewrite"
            );
            return;
        }
    };
    let _ = ws_repo_post.unstage_all();
}

#[allow(clippy::too_many_lines)]
fn sync_target_worktree_to_epoch(
    target_ws_path: &Path,
    target_workspace_name: &str,
    target_oid: &GitOid,
) {
    if !target_ws_path.exists() {
        return;
    }
    let oid = target_oid.as_str();

    // Equivalent to `git reset --keep <oid>` in our setting: the caller has
    // already proven that no locally modified path also changed between HEAD
    // and `<oid>` (via `dirty_paths_in_workspace` ∩ FF-range = ∅), so
    // materialising every (HEAD, target) diff path from the target tree is
    // safe and never clobbers user edits in disjoint paths. We then move
    // HEAD and reset the index. Failures are logged non-fatally.
    let ws_repo = match maw_git::GixRepo::open(target_ws_path) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                workspace = %target_workspace_name,
                error = %e,
                "failed to open target workspace repo during FF absorb"
            );
            return;
        }
    };

    let head_oid = match ws_repo.rev_parse_opt("HEAD") {
        Ok(Some(h)) => Some(h),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                workspace = %target_workspace_name,
                error = %e,
                "rev-parse HEAD failed during FF absorb"
            );
            return;
        }
    };
    let target_git: maw_git::GitOid = match oid.parse() {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(
                workspace = %target_workspace_name,
                error = %e,
                "invalid target OID during FF absorb"
            );
            return;
        }
    };
    // Resolve commit OIDs to tree OIDs so diff_trees compares the trees, not
    // the commit headers.
    let head_tree = head_oid.and_then(|h| ws_repo.read_commit(h).ok().map(|c| c.tree_oid));
    let target_tree = match ws_repo.read_commit(target_git).map(|c| c.tree_oid) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                workspace = %target_workspace_name,
                error = %e,
                "read target commit failed during FF absorb"
            );
            return;
        }
    };
    let diff = match ws_repo.diff_trees(head_tree, target_tree) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                workspace = %target_workspace_name,
                error = %e,
                "diff_trees failed during FF absorb"
            );
            return;
        }
    };
    for entry in diff {
        let rel = std::path::PathBuf::from(&entry.path);
        let full = target_ws_path.join(&rel);
        // Mode-faithful materialization (symlink / exec bit), matching the
        // `git reset --keep <oid>` this replaced; a plain write corrupts
        // symlinks and drops the executable bit (see `ff_materialize_blob`).
        match ws_repo.read_blob_at_path(target_git, &entry.path) {
            Ok(Some((mode, _oid, content))) => {
                if let Some(parent) = full.parent()
                    && let Err(e) = std::fs::create_dir_all(parent)
                {
                    tracing::warn!(
                        workspace = %target_workspace_name,
                        path = %rel.display(),
                        error = %e,
                        "FF absorb (target): mkdir failed"
                    );
                    continue;
                }
                if let Err(e) = ff_materialize_blob(&full, mode, &content) {
                    tracing::warn!(
                        workspace = %target_workspace_name,
                        path = %rel.display(),
                        error = %e,
                        "FF absorb (target): write failed"
                    );
                }
            }
            Ok(None) => {
                // symlink_metadata().is_ok() (not exists(), which follows
                // symlinks and is false for a *dangling* link) so a stale
                // broken symlink at an epoch-deleted path is removed, as
                // the replaced `git reset --keep` did. See the matching
                // comment in `ff_apply_one_path`.
                if full.symlink_metadata().is_ok()
                    && let Err(e) = std::fs::remove_file(&full)
                {
                    tracing::warn!(
                        workspace = %target_workspace_name,
                        path = %rel.display(),
                        error = %e,
                        "FF absorb (target): unlink failed"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    workspace = %target_workspace_name,
                    path = %rel.display(),
                    error = %e,
                    "FF absorb (target): read blob failed"
                );
            }
        }
    }
    // Move HEAD and align the index with the new HEAD.
    let head_path = ws_repo.git_dir().join("HEAD");
    if let Err(e) = std::fs::write(&head_path, format!("{oid}\n")) {
        tracing::warn!(
            workspace = %target_workspace_name,
            path = %head_path.display(),
            error = %e,
            "FF absorb (target): failed to write HEAD"
        );
        return;
    }
    if let Ok(repo_post) = maw_git::GixRepo::open(target_ws_path) {
        let _ = repo_post.unstage_all();
    }
}

/// Diagnostic classification of a post-PREPARE branch divergence.
///
/// Runs the same predicate as [`reconcile_epoch_with_branch`] but never
/// modifies state — by the time PREPARE has completed, the merge candidate's
/// parent is pinned to the pre-PREPARE epoch and a mid-flight FF absorb
/// would invalidate it. Returns a suffix to append to the existing
/// "diverged since PREPARE" error message:
///
/// * empty string when the divergence is a fork or a safe FF with nothing
///   useful to add,
/// * a `\n  Affected workspace(s): …` line when a retry would be blocked
///   by in-flight workspaces.
fn classify_post_prepare_divergence(
    root: &Path,
    target_workspace_name: &str,
    epoch_oid: &GitOid,
    branch_oid: &GitOid,
) -> Result<String> {
    if epoch_oid.as_str() == branch_oid.as_str() {
        return Ok(String::new());
    }

    let repo = super::ff_absorb::open_repo(root)?;
    let epoch_git: maw_git::GitOid = epoch_oid
        .as_str()
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid epoch OID '{}': {e}", epoch_oid.as_str()))?;
    let branch_git: maw_git::GitOid = branch_oid
        .as_str()
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid branch OID '{}': {e}", branch_oid.as_str()))?;

    if !super::ff_absorb::is_strict_ancestor(&repo, &epoch_git, &branch_git)? {
        return Ok(String::new());
    }

    let ff_paths = super::ff_absorb::compute_ff_changed_paths(&repo, &epoch_git, &branch_git)?;
    let backend = get_backend()?;
    let workspaces_info = backend
        .list()
        .map_err(|e| anyhow::anyhow!("failed to list workspaces: {e}"))?;

    let mut ws_touched: Vec<super::ff_absorb::WorkspaceTouchedPaths> = Vec::new();
    for info in workspaces_info {
        let name = info.id.as_str();
        if name == target_workspace_name {
            continue;
        }
        let touched = super::touched::collect_touched_workspace(&backend, &info.id)?;
        ws_touched.push(super::ff_absorb::WorkspaceTouchedPaths {
            name: touched.workspace,
            paths: touched.touched_paths.into_iter().collect(),
        });
    }

    Ok(
        match super::ff_absorb::evaluate_ff_safety(&ff_paths, &ws_touched) {
            super::ff_absorb::FfAbsorbDecision::Safe => String::new(),
            super::ff_absorb::FfAbsorbDecision::Blocked {
                affected_workspaces,
            } => format!(
                "\n  Affected workspace(s): {}",
                affected_workspaces.join(", ")
            ),
        },
    )
}

// ---------------------------------------------------------------------------
// Main merge function
// ---------------------------------------------------------------------------

/// Run the merge state machine: PREPARE → BUILD → VALIDATE → COMMIT → CLEANUP.
///
/// This uses the Manifold merge engine and state machine.
#[allow(clippy::too_many_lines)]
#[instrument(skip(opts), fields(workspaces = ?workspaces))]
pub fn merge(workspaces: &[String], opts: &MergeOptions<'_>) -> Result<()> {
    let MergeOptions {
        destroy_after,
        confirm,
        message,
        dry_run,
        format,
        target_workspace,
        target_branch,
        target_change_id,
        target_updates_epoch,
        ref resolve,
        ref resolve_all,
        verbose,
        force: _force,
        auto_rebase_siblings: auto_rebase_override,
    } = *opts;
    // `_force` is not used here — the check site reads `opts.force` directly
    // to avoid a shadowing conflict with the local `force` elsewhere.
    let ws_to_merge = workspaces.to_vec();
    let text_mode = format != OutputFormat::Json;

    macro_rules! textln {
        ($($arg:tt)*) => {
            if text_mode {
                println!($($arg)*);
            }
        };
    }

    if ws_to_merge.is_empty() {
        textln!("No workspaces to merge.");
        return Ok(());
    }

    let root = repo_root()?;
    let maw_config = MawConfig::load(&root)?;
    let default_ws = target_workspace;
    let into_target = target_change_id.unwrap_or(default_ws);
    let branch = target_branch;
    let branch_ref = format!("refs/heads/{branch}");
    let branch_before_oid = maw_core::refs::read_ref(&root, &branch_ref)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Target branch '{branch}' does not exist.
  To fix: create the branch first or repair change metadata, then retry."
        )
    })?;

    // Reconcile epoch with branch when they have diverged. If the branch is
    // strictly ahead of the epoch (a fast-forward) and no in-flight workspace
    // touches a file in the FF range, transparently absorb the upstream
    // commits into the epoch (bn-11ip). Otherwise bail with the legacy
    // "diverged" error, augmented with the affected workspace list when the
    // FF was a candidate but blocked.
    if target_updates_epoch && let Ok(Some(epoch_oid)) = maw_core::refs::read_epoch_current(&root) {
        let manifold_config = ManifoldConfig::load(
            &maw_core::model::layout::LayoutFlavor::detect_with_env(&root)
                .bootstrap_config_path(&root),
        )
        .unwrap_or_default();
        reconcile_epoch_with_branch(
            &root,
            branch,
            default_ws,
            &epoch_oid,
            &branch_before_oid,
            manifold_config.merge.auto_absorb_ff,
        )?;
    }

    // Reject merging the default workspace
    if ws_to_merge.iter().any(|ws| ws == default_ws) {
        bail!(
            "Cannot merge the default workspace \u{2014} it is the merge target, not a source.\n\
             \n  To advance {branch} to include your edits in {default_ws}:\n\
             \n    maw push --advance\n\
             \n  This updates refs/heads/{branch} to the current epoch and pushes."
        );
    }

    if let Some(target_change) = target_change_id {
        for ws_name in &ws_to_merge {
            let ws_meta = super::metadata::read(&root, ws_name).unwrap_or_default();
            if let Some(source_change) = ws_meta.change_id
                && source_change != target_change
            {
                bail!(
                    "Workspace '{ws_name}' belongs to change '{source_change}' and cannot merge into change '{target_change}'.\n  To fix: merge it into its own change target, or recreate workspace with the intended --change."
                );
            }
        }
    }

    let stale_sources = stale_merge_sources(&ws_to_merge)?;
    if !stale_sources.is_empty() {
        bail!("{}", stale_merge_block_message(&stale_sources));
    }

    let backend = get_backend()?;
    let sources = parse_workspace_ids(&ws_to_merge)?;
    validate_workspace_dirs(&sources, &backend)?;

    if dry_run {
        return preview_merge(&ws_to_merge, &root, into_target, format);
    }

    run_hooks(&maw_config.hooks.pre_merge, "pre-merge", &root, true)?;

    if ws_to_merge.len() == 1 {
        textln!("Adopting workspace: {}", ws_to_merge[0]);
    } else {
        textln!("Merging workspaces: {}", ws_to_merge.join(", "));
    }
    textln!();

    // Set up paths (layout-aware: v2 → `<root>/ws/default`, consolidated → root).
    let layout_flavor = maw_core::model::layout::LayoutFlavor::detect_with_env(&root);
    let manifold_dir = layout_flavor.manifold_dir(&root);

    // bn-yyx: record the integration start so the event log carries an
    // anchoring entry the agent can use to bound `maw merge events --since`.
    // bn-1lj2: route through the layout-aware manifold dir (the same one
    // `emit_integration_completed` uses below) so started+completed land in
    // the same oplog under the consolidated `.maw/manifold/`.
    emit_integration_started(&manifold_dir, &ws_to_merge, into_target, false);
    let default_ws_path = layout_flavor.default_target_path(&root, default_ws);
    // In the consolidated layout the privileged target IS the root checkout —
    // there's no `.maw/workspaces/default/` for the backend to inspect, so
    // skip the per-workspace base-epoch probe. The merge engine derives the
    // pre-merge epoch from the current epoch ref directly.
    let target_base_epoch_before = if default_ws_path.exists() && !default_ws_path.eq(&root) {
        let target_ws_id = WorkspaceId::new(default_ws)
            .map_err(|e| anyhow::anyhow!("invalid target workspace '{default_ws}': {e}"))?;
        Some(
            backend
                .status(&target_ws_id)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "failed to inspect target workspace '{default_ws}' before merge: {e}"
                    )
                })?
                .base_epoch
                .as_str()
                .to_owned(),
        )
    } else {
        None
    };
    let workspace_dirs = workspace_dirs_map(&sources, &backend);

    // Refuse merge if any source workspace has unresolved rebase conflicts.
    //
    // The two gates (sidecar + HEAD-tree placeholder tripwire) are extracted
    // into `assert_sources_clean_for_merge` so `--check` (bn-qw4i) and the
    // real merge agree on what "ready to merge" means. See that helper's
    // doc comment for the per-gate rationale (bn-m6ad/bn-3pgl/bn-3oau for
    // the sidecar; bn-28d1 for the tripwire).
    assert_sources_clean_for_merge(
        &root,
        &ws_to_merge,
        &workspace_dirs,
        opts.force,
        into_target,
    )?;

    if target_updates_epoch {
        guard_unbound_sources_against_active_change_ancestry(&root, branch, &ws_to_merge)?;
    }

    // -----------------------------------------------------------------------
    // Phase 1: PREPARE — freeze inputs
    // -----------------------------------------------------------------------
    textln!("PREPARE: Freezing merge inputs...");
    let frozen = run_prepare_phase(&root, &manifold_dir, &sources, &workspace_dirs)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    textln!("  Epoch: {}", &frozen.epoch.as_str()[..12]);
    if !target_updates_epoch {
        textln!(
            "  Target base ({branch}): {}",
            &branch_before_oid.as_str()[..12]
        );
    }
    for (ws_id, head) in &frozen.heads {
        textln!("  {}: {}", ws_id, &head.as_str()[..12]);
    }

    let merge_base_epoch = if target_updates_epoch {
        frozen.epoch.clone()
    } else {
        EpochId::new(branch_before_oid.as_str()).map_err(|e| {
            anyhow::anyhow!(
                "invalid target branch base OID '{}': {e}",
                branch_before_oid.as_str()
            )
        })?
    };

    if let Err(e) = record_merge_target_context(
        &manifold_dir,
        branch,
        (!target_updates_epoch).then_some(&merge_base_epoch),
    ) {
        abort_merge(
            &manifold_dir,
            &format!("failed to persist target merge context: {e}"),
        );
        bail!("Merge PREPARE phase failed: could not persist target merge context: {e}");
    }

    // Persist user-provided commit message into merge-state so the BUILD
    // phase can use it for the candidate commit.
    if let Some(msg) = message {
        let state_path = MergeStateFile::default_path(&manifold_dir);
        if let Ok(mut state) = MergeStateFile::read(&state_path) {
            state.commit_message = Some(msg.to_string());
            if let Err(e) = state.write_atomic(&state_path) {
                tracing::warn!("Failed to persist commit message to merge-state: {e}");
            }
        }
    }

    if let Err(e) = record_snapshot_operations(&root, &backend, &sources) {
        abort_merge(&manifold_dir, &format!("snapshot recording failed: {e}"));
        bail!("Merge PREPARE phase failed: could not record snapshot operations: {e}");
    }

    // -----------------------------------------------------------------------
    // Phase 2: BUILD — collect, partition, resolve, build candidate
    // -----------------------------------------------------------------------
    textln!();
    textln!("BUILD: Running merge engine...");
    let mut build_output = match run_build_phase(&root, &manifold_dir, &backend) {
        Ok(output) => output,
        Err(e) => {
            // Abort: clean up merge-state
            abort_merge(&manifold_dir, &format!("BUILD failed: {e}"));
            bail!("Merge BUILD phase failed: {e}");
        }
    };

    textln!(
        "  {} unique path(s), {} shared path(s), {} resolved",
        build_output.unique_count,
        build_output.shared_count,
        build_output.resolved_count
    );
    textln!("  Candidate: {}", &build_output.candidate.as_str()[..12]);

    // Check for empty merge (no changes detected)
    if build_output.unique_count == 0
        && build_output.shared_count == 0
        && build_output.conflicts.is_empty()
    {
        abort_merge(&manifold_dir, "empty merge (no changes)");

        let ws_list = ws_to_merge.join(", ");

        // Destroy workspaces if requested, even though there's nothing to merge (bn-1zdc).
        if destroy_after {
            if format != OutputFormat::Json {
                textln!();
                textln!("No changes to merge. Destroying workspace(s): {ws_list}");
            }
            handle_post_merge_destroy(
                &ws_to_merge,
                default_ws,
                confirm,
                &backend,
                &root,
                text_mode,
                verbose,
            )?;
            if format == OutputFormat::Json {
                let output = serde_json::json!({
                    "status": "empty",
                    "workspaces": ws_to_merge,
                    "destroyed": true,
                    "message": format!("No changes detected in workspace(s): {ws_list}. Workspace(s) destroyed."),
                });
                println!("{}", serde_json::to_string_pretty(&output)?);
            }
            return Ok(());
        }

        if format == OutputFormat::Json {
            let output = serde_json::json!({
                "status": "empty",
                "workspaces": ws_to_merge,
                "message": format!("No changes detected in workspace(s): {ws_list}"),
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            textln!();
            textln!("No changes detected in workspace(s): {ws_list}");
            textln!("Nothing to merge. Workspaces were not destroyed.");
        }

        bail!("No changes detected in workspace(s): {ws_list}. Nothing to merge.");
    }

    // Check for unresolved conflicts
    if !build_output.conflicts.is_empty() {
        let conflicts_with_ids = assign_conflict_ids(&build_output.conflicts);

        let has_resolutions = !resolve.is_empty() || resolve_all.is_some();
        if has_resolutions {
            // --resolve / --resolve-all mode: apply stateless resolutions
            let mut parsed = match parse_resolutions(resolve) {
                Ok(p) => p,
                Err(e) => {
                    abort_merge(&manifold_dir, &format!("parse resolutions: {e}"));
                    return Err(e);
                }
            };

            // Expand --resolve-all for any conflict not already covered
            if let Some(ws_name) = resolve_all {
                for c in &conflicts_with_ids {
                    parsed
                        .entry(c.id.clone())
                        .or_insert_with(|| Resolution::Workspace(ws_name.clone()));
                }
            }

            let (resolved_contents, remaining) =
                match apply_resolutions(&conflicts_with_ids, &parsed, &workspace_dirs) {
                    Ok(r) => r,
                    Err(e) => {
                        abort_merge(&manifold_dir, &format!("apply resolutions: {e}"));
                        return Err(e);
                    }
                };

            if remaining.is_empty() {
                // All conflicts resolved — patch the candidate tree
                textln!(
                    "  {} conflict(s) resolved via --resolve.",
                    conflicts_with_ids.len()
                );
                let patched = match patch_candidate_tree(
                    &root,
                    &build_output.candidate,
                    &resolved_contents,
                ) {
                    Ok(oid) => oid,
                    Err(e) => {
                        abort_merge(&manifold_dir, &format!("patch tree failed: {e:#}"));
                        bail!("Failed to patch candidate tree with resolutions: {e:#}");
                    }
                };
                textln!("  Patched candidate: {}", &patched.as_str()[..12]);

                // Replace build_output with patched candidate and zero conflicts.
                // Falls through to VALIDATE → COMMIT → CLEANUP below.
                build_output = BuildPhaseOutput {
                    candidate: patched,
                    unique_count: build_output.unique_count,
                    shared_count: build_output.shared_count,
                    resolved_count: build_output.resolved_count + conflicts_with_ids.len(),
                    conflicts: vec![],
                    resolved_paths: build_output.resolved_paths.clone(),
                };
            } else {
                // Some conflicts remain unresolved — report them with IDs
                abort_merge(&manifold_dir, "partially resolved conflicts");

                // bn-yyx: persist the remaining conflict surface so the
                // agent can recall it without re-running `maw ws merge`.
                persist_merge_conflict_surface(
                    &manifold_dir,
                    &ws_to_merge,
                    into_target,
                    &remaining,
                );
                emit_integration_aborted(
                    &manifold_dir,
                    &ws_to_merge,
                    into_target,
                    "partially resolved conflicts",
                );

                let ws_args = ws_to_merge.join(" ");
                let default_ws = ws_to_merge.first().map_or("WORKSPACE", |s| s.as_str());

                // Include already-resolved IDs so agents can copy-paste the full command
                let mut resolve_args: Vec<String> = Vec::new();
                for c in &conflicts_with_ids {
                    if let Some(res) = parsed.get(&c.id) {
                        let val = match res {
                            Resolution::Workspace(name) => name.clone(),
                            Resolution::Content(p) => format!("content:{}", p.display()),
                        };
                        resolve_args.push(format!("--resolve {}={val}", c.id));
                    }
                }
                for c in &remaining {
                    if !parsed.contains_key(&c.id) {
                        resolve_args.push(format!("--resolve {}={default_ws}", c.id));
                    }
                }

                if format == OutputFormat::Json {
                    let conflict_jsons: Vec<ConflictJson> = remaining
                        .iter()
                        .map(|c| {
                            conflict_record_to_json_with_id(&c.record, Some(&c.id), &c.atom_ids)
                        })
                        .collect();
                    let retry_message = conflict_retry_message(into_target);
                    let to_fix = format!(
                        "maw ws merge {ws_args} --into {} {} --message {retry_message}",
                        into_target,
                        resolve_args.join(" ")
                    );
                    let output = MergeConflictOutput {
                        status: "conflict".to_string(),
                        workspaces: ws_to_merge,
                        conflict_count: conflict_jsons.len(),
                        conflicts: conflict_jsons,
                        message: format!(
                            "{} conflict(s) remain after partial resolution. Add more --resolve flags.",
                            remaining.len()
                        ),
                        to_fix: to_fix.clone(),
                        resolve_command: Some(to_fix),
                    };
                    println!("{}", serde_json::to_string_pretty(&output)?);
                } else {
                    textln!(
                        "  {} of {} conflict(s) resolved, {} remaining:",
                        conflicts_with_ids.len() - remaining.len(),
                        conflicts_with_ids.len(),
                        remaining.len()
                    );
                    print_conflict_report_with_resolve(
                        &remaining,
                        &ws_to_merge,
                        into_target,
                        Some(&resolve_args),
                    );
                }

                bail!(
                    "{} conflict(s) remain after partial resolution.",
                    remaining.len()
                );
            }
        } else {
            // No --resolve flags: abort with conflict report including IDs
            abort_merge(&manifold_dir, "unresolved conflicts");

            // bn-yyx: persist the conflict surface + emit ConflictDetected /
            // IntegrationAborted events so the agent can recall the conflict
            // without re-running `maw ws merge`.
            persist_merge_conflict_surface(
                &manifold_dir,
                &ws_to_merge,
                into_target,
                &conflicts_with_ids,
            );
            emit_integration_aborted(
                &manifold_dir,
                &ws_to_merge,
                into_target,
                "unresolved conflicts",
            );

            let ws_args = ws_to_merge.join(" ");
            let default_ws = ws_to_merge.first().map_or("WORKSPACE", |s| s.as_str());
            let resolve_args: Vec<String> = conflicts_with_ids
                .iter()
                .map(|c| format!("--resolve {}={default_ws}", c.id))
                .collect();

            if format == OutputFormat::Json {
                let conflict_jsons: Vec<ConflictJson> = conflicts_with_ids
                    .iter()
                    .map(|c| conflict_record_to_json_with_id(&c.record, Some(&c.id), &c.atom_ids))
                    .collect();
                let retry_message = conflict_retry_message(into_target);
                let to_fix = format!(
                    "maw ws merge {ws_args} --into {} {} --message {retry_message}",
                    into_target,
                    resolve_args.join(" ")
                );
                let output = MergeConflictOutput {
                    status: "conflict".to_string(),
                    workspaces: ws_to_merge,
                    conflict_count: conflict_jsons.len(),
                    conflicts: conflict_jsons,
                    message: format!(
                        "Merge has {} unresolved conflict(s). Resolve them and retry.",
                        build_output.conflicts.len()
                    ),
                    to_fix: to_fix.clone(),
                    resolve_command: Some(to_fix),
                };
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                textln!("  {} unresolved conflict(s)", build_output.conflicts.len());
                print_conflict_report(&conflicts_with_ids, &ws_to_merge, into_target);

                if destroy_after {
                    textln!();
                    textln!("NOT destroying workspaces due to conflicts.");
                }
            }

            bail!(
                "Merge has {} unresolved conflict(s). Resolve them and retry.",
                build_output.conflicts.len()
            );
        }
    }

    // -----------------------------------------------------------------------
    // Phase 3: VALIDATE — run post-merge validation commands
    // -----------------------------------------------------------------------
    let manifold_config = ManifoldConfig::load(&manifold_dir.join("config.toml"))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let validation_config = &manifold_config.merge.validation;

    textln!();
    if validation_config.has_commands() {
        textln!("VALIDATE: Running post-merge validation...");

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
                textln!("  Validation skipped (no commands configured).");
            }
            ValidateOutcome::Passed(r) => {
                textln!("  Validation passed ({}ms).", r.duration_ms);
            }
            ValidateOutcome::PassedWithWarnings(r) => {
                textln!(
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
                textln!(
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

                textln!(
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
                    &merge_base_epoch,
                    build_output.candidate.clone(),
                    branch,
                    r.clone(),
                ) {
                    Ok(qws_path) => {
                        textln!("  Quarantine workspace created: {}", qws_path.display());
                        textln!();
                        if !r.stderr.is_empty() {
                            textln!("  Validation output:");
                            for line in r.stderr.lines().take(10) {
                                eprintln!("    {line}");
                            }
                        }
                        textln!();
                        textln!("Fix the issues in the quarantine workspace:");
                        textln!("  Edit files: {}/", qws_path.display());
                        textln!("  Re-validate and commit: maw merge promote {merge_id}");
                        textln!("  Discard quarantine:     maw merge abandon {merge_id}");
                        textln!();
                        textln!("Source workspaces are preserved (not destroyed).");
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
        textln!();
        textln!("VALIDATE: No validation commands configured — skipping.");
        advance_merge_state(&manifold_dir, MergePhase::Validate)?;
    }

    // -----------------------------------------------------------------------
    // Phase 4: COMMIT — atomically update refs (point of no return)
    // -----------------------------------------------------------------------
    textln!();
    if target_updates_epoch {
        textln!("COMMIT: Advancing epoch...");
    } else {
        textln!("COMMIT: Updating target branch...");
    }

    // Advance merge-state to Commit phase
    advance_merge_state(&manifold_dir, MergePhase::Commit)?;

    // bn-38vw: record `epoch_after` into the merge-state journal BEFORE the
    // ref-advancing CAS — not after. The candidate (= the new epoch commit
    // OID) was already built in BUILD and validated in VALIDATE, so it is a
    // durable commit object regardless of where the refs currently point.
    //
    // Recording it here closes a crash window: previously `epoch_after` was
    // written only AFTER the CAS, so a crash between the CAS (refs advanced —
    // the point of no return) and the journal write left merge-state in
    // `phase=commit` with `epoch_after=None`. Oracle B flags that incoherent
    // shape ("past the point-of-no-return but epoch_after is not recorded")
    // and nothing self-healed it on demand.
    //
    // Journal coherence at EVERY post-build crash point now holds:
    //   * crash AFTER this write, BEFORE the CAS  → phase=commit, refs at old
    //     epoch, epoch_after=candidate. Oracle B only checks epoch_after
    //     resolves to a commit (it does). Recovery (`CheckCommit` →
    //     `recover_partial_commit_with_branch_base`) reads the LIVE refs, sees
    //     NotCommitted, and the caller aborts — no work to orphan (nothing was
    //     committed; the candidate is reachable from no ref but is a transient
    //     build artifact, identical to the pre-fix pre-CAS crash).
    //   * crash AFTER the CAS → refs advanced + epoch_after=candidate.
    //     Recovery converges forward idempotently to AlreadyCommitted /
    //     FinalizedMainRef.
    // The candidate OID written here is identical to the value the post-CAS
    // path used, so this is a pure reordering: no new value is journaled.
    record_epoch_after(&manifold_dir, &build_output.candidate)?;

    let epoch_before_oid = merge_base_epoch.oid().clone();
    // Pre-flight: verify the branch hasn't diverged from the target head
    // captured before PREPARE. If direct commits were made to the branch
    // outside of maw between PREPARE and COMMIT, the branch CAS will fail
    // after the epoch ref has already moved, leaving refs diverged.
    // Detect and abort cleanly before touching any refs.
    if let Ok(Some(current_branch)) = maw_core::refs::read_ref(&root, &branch_ref)
        && current_branch != branch_before_oid
    {
        // Re-evaluate the FF safety predicate purely as a diagnostic: even
        // when the new advance is a strict FF, the merge candidate's parent
        // is already pinned to the pre-PREPARE epoch, so we can't absorb
        // mid-flight. Surface the affected workspaces (if any) so the user
        // understands what a retry would have to drain.
        let ff_diagnostic = classify_post_prepare_divergence(
            &root,
            default_ws,
            &branch_before_oid,
            &current_branch,
        )
        .unwrap_or_default();
        abort_merge(
            &manifold_dir,
            "branch diverged from target head since PREPARE (direct commits detected)",
        );
        bail!(
            "Merge COMMIT aborted: branch '{branch}' has diverged from its pre-merge head since PREPARE.\n  \
                 Expected: {}\n  \
                 Actual:   {}\n  \
                 Cause: commits were made directly to '{branch}' outside of maw.\n  \
                 Fix: retry after synchronizing target branch state, then rerun maw ws merge.\n  \
                 For change branches, `maw changes sync <change-id>` can help before retry.{}",
            &branch_before_oid.as_str()[..12],
            &current_branch.as_str()[..12],
            ff_diagnostic,
        );
    }

    #[allow(clippy::if_not_else, reason = "branch-only path is the simpler arm")]
    if !target_updates_epoch {
        match maw_core::refs::write_ref_cas(
            &root,
            &branch_ref,
            &branch_before_oid,
            &build_output.candidate,
        ) {
            Ok(()) => {
                textln!(
                    "  Branch '{branch}' advanced: {} → {}",
                    &branch_before_oid.as_str()[..12],
                    &build_output.candidate.as_str()[..12]
                );
                textln!("  Epoch unchanged: {}", &frozen.epoch.as_str()[..12]);
            }
            Err(e) => {
                abort_merge(
                    &manifold_dir,
                    &format!("COMMIT failed (branch-only CAS): {e}"),
                );
                bail!("Merge COMMIT phase failed: could not update branch '{branch}': {e}");
            }
        }
    } else {
        match run_commit_phase_with_branch_base(
            &root,
            branch,
            &epoch_before_oid,
            &branch_before_oid,
            &build_output.candidate,
        ) {
            Ok(CommitResult::Committed) => {
                textln!(
                    "  Epoch advanced: {} → {}",
                    &epoch_before_oid.as_str()[..12],
                    &build_output.candidate.as_str()[..12]
                );
                textln!("  Branch '{branch}' updated.");
            }
            Err(maw::merge::commit::CommitError::PartialCommit) => {
                // Epoch ref moved but branch ref didn't — attempt recovery
                textln!("  WARNING: Partial commit — attempting recovery...");
                match recover_partial_commit_with_branch_base(
                    &root,
                    branch,
                    &epoch_before_oid,
                    &branch_before_oid,
                    &build_output.candidate,
                ) {
                    Ok(CommitRecovery::FinalizedMainRef) => {
                        textln!("  Recovery succeeded: branch ref finalized.");
                    }
                    Ok(CommitRecovery::AlreadyCommitted) => {
                        textln!("  Recovery: both refs already updated.");
                    }
                    Ok(CommitRecovery::NotCommitted) => {
                        abort_merge(&manifold_dir, "commit phase failed: neither ref updated");
                        bail!("Merge COMMIT phase failed: could not update refs.");
                    }
                    Err(e) => {
                        // Epoch moved but branch is at an unexpected value —
                        // abort so the merge-state doesn't stay stuck at "commit".
                        abort_merge(
                            &manifold_dir,
                            &format!("partial commit recovery failed: {e}"),
                        );
                        bail!(
                            "Merge COMMIT phase partially applied and recovery failed: {e}\n  \
                             The merge-state has been aborted. Check refs manually:\n  \
                             refs/manifold/epoch/current and refs/heads/{branch}"
                        );
                    }
                }
            }
            Err(e) => {
                abort_merge(&manifold_dir, &format!("COMMIT failed: {e}"));
                bail!("Merge COMMIT phase failed: {e}");
            }
        }
    }

    // bn-38vw: `epoch_after` is now journaled BEFORE the CAS above (see the
    // comment at the start of Phase 4), so the merge-state is coherent at
    // every post-build crash point. No post-CAS write is needed here — the
    // refs and the journal already agree once the CAS has committed.

    // Record merge operations in source workspace histories.
    for warning in
        record_merge_operations(&root, &sources, &merge_base_epoch, &build_output.candidate)
    {
        tracing::warn!("{warning}");
    }

    // -----------------------------------------------------------------------
    // bn-3vf5: auto-rebase sibling workspaces onto the new epoch.
    //
    // Default-on; opt-out via config (`merge.auto_rebase_siblings = false`)
    // or per-invocation via `--no-auto-rebase`. Each sibling: try-lock →
    // skip-rules → call the rebase core (no worktree mutation) → record a
    // result line.
    // -----------------------------------------------------------------------
    // bn-mq6j: names of siblings that ended this auto-rebase pass in a
    // conflicted state, surfaced again in the merge's own final summary
    // (text NOTE line + JSON `sibling_conflicts`) so it isn't buried in the
    // middle of the merge output.
    let mut sibling_conflict_names: Vec<String> = Vec::new();
    if target_updates_epoch {
        let manifold_config_path = maw_core::model::layout::LayoutFlavor::detect_with_env(&root)
            .bootstrap_config_path(&root);
        let manifold_cfg =
            maw_core::config::ManifoldConfig::load(&manifold_config_path).unwrap_or_default();
        let auto_rebase_enabled =
            auto_rebase_override.unwrap_or(manifold_cfg.merge.auto_rebase_siblings);
        if auto_rebase_enabled {
            let reports = super::sync::auto_rebase::auto_rebase_siblings(
                &root,
                &backend,
                target_workspace,
                &ws_to_merge,
                build_output.candidate.as_str(),
            );
            if !reports.is_empty() && text_mode {
                textln!();
                textln!("AUTO-REBASE: replaying sibling workspaces onto new epoch...");
                for report in &reports {
                    textln!(
                        "  {} — {}",
                        report.name,
                        report.result.describe(&report.name)
                    );
                }
            }
            for report in &reports {
                match &report.result {
                    super::sync::auto_rebase::SiblingResult::Failed { reason } => {
                        tracing::warn!(workspace = %report.name, error = %reason, "sibling auto-rebase failed");
                    }
                    super::sync::auto_rebase::SiblingResult::RebasedWithConflicts { .. }
                    | super::sync::auto_rebase::SiblingResult::RebasedWithConflictsRefsOnly {
                        ..
                    } => {
                        sibling_conflict_names.push(report.name.clone());
                    }
                    _ => {}
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Phase 5: CLEANUP — destroy workspaces (if requested), remove merge-state
    // -----------------------------------------------------------------------
    textln!();
    textln!("CLEANUP...");

    // Advance merge-state to Cleanup phase
    advance_merge_state(&manifold_dir, MergePhase::Cleanup)?;

    // Update the default workspace to point to the new epoch.
    // If the default workspace has dirty state, snapshot it before checkout
    // and record a Snapshot op in the default workspace's oplog (§6.1 Step 1).
    if default_ws_path.exists() {
        // Compute patchset BEFORE the checkout so we capture pre-rewrite state.
        let pre_checkout_patchset =
            maw_core::model::diff::compute_patchset(&default_ws_path, &merge_base_epoch).ok();

        // FP: crash before updating the default workspace to the new epoch.
        // A crash here means COMMIT succeeded but the default workspace
        // still points at the old epoch.
        maw::fp!("FP_CLEANUP_BEFORE_DEFAULT_CHECKOUT")?;

        update_default_workspace(
            &default_ws_path,
            default_ws,
            branch,
            epoch_before_oid.as_str(),
            build_output.candidate.as_str(),
            target_base_epoch_before.as_deref(),
            &root,
            target_updates_epoch,
            text_mode,
            &build_output.resolved_paths,
            &ws_to_merge,
        )?;

        // Record a Snapshot op if the default workspace had dirty files.
        if let Some(ref patch_set) = pre_checkout_patchset
            && !patch_set.is_empty()
        {
            let default_ws_id = WorkspaceId::new(default_ws)
                .map_err(|e| anyhow::anyhow!("invalid target workspace '{default_ws}': {e}"))?;

            match write_patch_set_blob(&root, patch_set) {
                Ok(patch_set_oid) => {
                    match ensure_workspace_oplog_head(&root, &default_ws_id, &merge_base_epoch) {
                        Ok(head) => {
                            let snapshot_op = Operation {
                                parent_ids: vec![head.clone()],
                                workspace_id: default_ws_id.clone(),
                                timestamp: super::now_timestamp_iso8601(),
                                payload: OpPayload::Snapshot { patch_set_oid },
                            };

                            if let Err(e) = append_operation_with_runtime_checkpoint(
                                &root,
                                &default_ws_id,
                                &snapshot_op,
                                Some(&head),
                            ) {
                                tracing::warn!(
                                    "Could not record default workspace snapshot op: {e}"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Could not bootstrap default workspace oplog: {e}");
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Could not write default workspace patch-set blob: {e}");
                }
            }
        }
    }

    // Destroy source workspaces if requested
    if destroy_after {
        handle_post_merge_destroy(
            &ws_to_merge,
            default_ws,
            confirm,
            &backend,
            &root,
            text_mode,
            verbose,
        )?;
    }

    // Remove merge-state file
    let merge_state_path = MergeStateFile::default_path(&manifold_dir);
    let state = MergeStateFile::read(&merge_state_path)
        .unwrap_or_else(|_| MergeStateFile::new(sources, merge_base_epoch.clone(), now_secs()));
    run_cleanup_phase(&state, &merge_state_path, false, |_ws| Ok(()))
        .map_err(|e| anyhow::anyhow!("cleanup failed: {e}"))?;

    // Also clean up commit-phase sidecar state files if present.
    // `commit-state.json` is current; `merge-state` is a legacy fallback.
    let abort_flavor = maw_core::model::layout::LayoutFlavor::detect_with_env(&root);
    let abort_manifold = abort_flavor.manifold_dir(&root);
    let commit_state_path = abort_manifold.join("commit-state.json");
    if commit_state_path.exists() {
        let _ = std::fs::remove_file(&commit_state_path);
    }
    let legacy_commit_state_path = abort_manifold.join("merge-state");
    if legacy_commit_state_path.exists() {
        let _ = std::fs::remove_file(&legacy_commit_state_path);
    }

    run_hooks(&maw_config.hooks.post_merge, "post-merge", &root, false)?;

    // Message is always provided by the caller (enforced in mod.rs dispatch).
    let msg = message.expect("merge message must be provided by caller");
    let next_command = target_change_id.map_or_else(
        || {
            if target_updates_epoch {
                "maw push".to_string()
            } else {
                format!("git push origin {branch}")
            }
        },
        |change_id| format!("maw changes pr {change_id} --draft"),
    );

    // bn-yyx: emit IntegrationCompleted + clear last-conflict so a future
    // `maw merge last-conflict` doesn't return a stale snapshot.
    emit_integration_completed(
        &manifold_dir,
        &ws_to_merge,
        into_target,
        build_output.candidate.as_str(),
    );

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
            next: next_command,
            advice: vec![],
            sibling_conflicts: sibling_conflict_names.clone(),
        };
        println!("{}", serde_json::to_string_pretty(&success)?);
    } else {
        textln!();
        textln!("Merged to {branch}: {msg} from {}", ws_to_merge.join(", "));
        if !sibling_conflict_names.is_empty() {
            textln!(
                "NOTE: {} sibling workspace(s) now have conflicts: {}",
                sibling_conflict_names.len(),
                sibling_conflict_names.join(", ")
            );
        }
        textln!();
        if let Some(change_id) = target_change_id {
            textln!("Next: open or update PR for this change:");
            textln!("  maw changes pr {change_id} --draft");
        } else if target_updates_epoch {
            textln!("Next: push to remote:");
            textln!("  maw push");
        } else {
            textln!("Next: push branch to remote:");
            textln!("  git push origin {branch}");
        }
        // bn-1kop: stable, grep-friendly sentinel line. Everything above is
        // free text (the commit message is user-supplied); this final line
        // is a fixed format an agent or script can match on without parsing
        // prose. JSON output already has an unambiguous `status: "success"`
        // field (MergeSuccessOutput) — this is the text-mode equivalent.
        textln!(
            "[OK] merged {} into {branch} @ {}",
            ws_to_merge.join(", "),
            &build_output.candidate.as_str()[..12]
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers: operation-log recording
// ---------------------------------------------------------------------------

/// Record a Snapshot operation for every source workspace.
fn record_snapshot_operations<B: WorkspaceBackend>(
    root: &Path,
    backend: &B,
    sources: &[WorkspaceId],
) -> Result<()> {
    let patch_sets = collect_snapshots(root, backend, sources)
        .map_err(|e| anyhow::anyhow!("collect snapshots: {e}"))?;

    for patch_set in &patch_sets {
        let ws_id = &patch_set.workspace_id;
        let head = ensure_workspace_oplog_head(root, ws_id, &patch_set.epoch)
            .map_err(|e| anyhow::anyhow!("bootstrap workspace '{ws_id}' op log: {e}"))?;

        let model_patch_set = to_model_patch_set(root, patch_set)
            .map_err(|e| anyhow::anyhow!("build patch-set for workspace '{ws_id}': {e}"))?;
        let patch_set_oid = write_patch_set_blob(root, &model_patch_set)
            .map_err(|e| anyhow::anyhow!("write patch-set blob for workspace '{ws_id}': {e}"))?;

        let snapshot_op = Operation {
            parent_ids: vec![head.clone()],
            workspace_id: ws_id.clone(),
            timestamp: super::now_timestamp_iso8601(),
            payload: OpPayload::Snapshot { patch_set_oid },
        };

        append_operation_with_runtime_checkpoint(root, ws_id, &snapshot_op, Some(&head))
            .map_err(|e| anyhow::anyhow!("append snapshot op for workspace '{ws_id}': {e}"))?;
    }

    Ok(())
}

/// Record Merge operations in source workspace histories after a successful COMMIT.
///
/// Returns warning messages instead of failing the already-committed merge.
fn record_merge_operations(
    root: &Path,
    sources: &[WorkspaceId],
    epoch_before: &EpochId,
    epoch_after: &GitOid,
) -> Vec<String> {
    let mut warnings = Vec::new();

    let epoch_after_id = match EpochId::new(epoch_after.as_str()) {
        Ok(epoch) => epoch,
        Err(e) => {
            warnings.push(format!(
                "Failed to record merge operations: invalid epoch_after '{}': {e}",
                epoch_after.as_str()
            ));
            return warnings;
        }
    };

    for ws_id in sources {
        let head = match ensure_workspace_oplog_head(root, ws_id, epoch_before) {
            Ok(head) => head,
            Err(e) => {
                warnings.push(format!(
                    "Could not ensure op-log head for workspace '{ws_id}': {e}"
                ));
                continue;
            }
        };

        let op = Operation {
            parent_ids: vec![head.clone()],
            workspace_id: ws_id.clone(),
            timestamp: super::now_timestamp_iso8601(),
            payload: OpPayload::Merge {
                sources: sources.to_vec(),
                epoch_before: epoch_before.clone(),
                epoch_after: epoch_after_id.clone(),
            },
        };

        if let Err(e) = append_operation_with_runtime_checkpoint(root, ws_id, &op, Some(&head)) {
            warnings.push(format!(
                "Could not append merge op for workspace '{ws_id}': {e}"
            ));
        }
    }

    warnings
}

fn ensure_workspace_oplog_head(
    root: &Path,
    ws_id: &WorkspaceId,
    epoch: &EpochId,
) -> Result<GitOid> {
    if let Some(head) = read_head(root, ws_id).map_err(|e| anyhow::anyhow!("read head: {e}"))? {
        return Ok(head);
    }

    let create_op = Operation {
        parent_ids: vec![],
        workspace_id: ws_id.clone(),
        timestamp: super::now_timestamp_iso8601(),
        payload: OpPayload::Create {
            epoch: epoch.clone(),
        },
    };

    append_operation_with_runtime_checkpoint(root, ws_id, &create_op, None)
        .map_err(|e| anyhow::anyhow!("append create op: {e}"))
}

fn to_model_patch_set(root: &Path, patch_set: &CollectedPatchSet) -> Result<ModelPatchSet> {
    let mut patches = BTreeMap::new();

    for change in &patch_set.changes {
        match change.kind {
            ChangeKind::Added => {
                let blob = change
                    .blob
                    .clone()
                    .or_else(|| {
                        change
                            .content
                            .as_deref()
                            .and_then(|bytes| git_hash_object(root, bytes))
                    })
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "could not compute blob for added path '{}'",
                            change.path.display()
                        )
                    })?;
                let file_id = change
                    .file_id
                    .unwrap_or_else(|| file_id_from_path(&change.path));
                patches.insert(change.path.clone(), PatchValue::Add { blob, file_id });
            }
            ChangeKind::Modified => {
                let base_blob = epoch_blob_oid(root, &patch_set.epoch, &change.path)?;
                let new_blob = change
                    .blob
                    .clone()
                    .or_else(|| {
                        change
                            .content
                            .as_deref()
                            .and_then(|bytes| git_hash_object(root, bytes))
                    })
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "could not compute blob for modified path '{}'",
                            change.path.display()
                        )
                    })?;
                let file_id = change
                    .file_id
                    .unwrap_or_else(|| file_id_from_blob(&base_blob));
                patches.insert(
                    change.path.clone(),
                    PatchValue::Modify {
                        base_blob,
                        new_blob,
                        file_id,
                    },
                );
            }
            ChangeKind::Deleted => {
                // If the file doesn't exist at the epoch commit, it was added
                // then deleted in the workspace (net no-op). Skip it rather
                // than propagating the error from `git rev-parse`.
                let Ok(previous_blob) = epoch_blob_oid(root, &patch_set.epoch, &change.path) else {
                    continue;
                };
                let file_id = change
                    .file_id
                    .unwrap_or_else(|| file_id_from_blob(&previous_blob));
                patches.insert(
                    change.path.clone(),
                    PatchValue::Delete {
                        previous_blob,
                        file_id,
                    },
                );
            }
        }
    }

    Ok(ModelPatchSet {
        base_epoch: patch_set.epoch.clone(),
        patches,
    })
}

fn write_patch_set_blob(root: &Path, patch_set: &ModelPatchSet) -> Result<GitOid> {
    let payload = serde_json::to_vec(patch_set)
        .map_err(|e| anyhow::anyhow!("serialize patch-set JSON: {e}"))?;

    let repo = maw_git::GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;
    let git_oid = maw_git::GitRepo::write_blob(&repo, &payload)
        .map_err(|e| anyhow::anyhow!("write_blob for patch-set failed: {e}"))?;

    GitOid::new(&git_oid.to_string())
        .map_err(|e| anyhow::anyhow!("invalid patch-set blob OID: {e}"))
}

fn git_hash_object(root: &Path, content: &[u8]) -> Option<GitOid> {
    let repo = maw_git::GixRepo::open(root).ok()?;
    let git_oid = maw_git::GitRepo::write_blob(&repo, content).ok()?;
    GitOid::new(&git_oid.to_string()).ok()
}

fn epoch_blob_oid(root: &Path, epoch: &EpochId, path: &Path) -> Result<GitOid> {
    let rev = format!("{}:{}", epoch.as_str(), path.to_string_lossy());
    let repo = maw_git::GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;
    let git_oid = repo
        .rev_parse(&rev)
        .map_err(|e| anyhow::anyhow!("rev-parse '{rev}' failed: {e}"))?;

    GitOid::new(&git_oid.to_string())
        .map_err(|e| anyhow::anyhow!("invalid blob OID for '{}': {e}", path.display()))
}

fn file_id_from_path(path: &Path) -> FileId {
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    FileId::new(u128::from_be_bytes(bytes))
}

fn file_id_from_blob(blob: &GitOid) -> FileId {
    let n = u128::from_str_radix(&blob.as_str()[..32], 16).unwrap_or(0);
    FileId::new(n)
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
    result: &maw_core::merge_state::ValidationResult,
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
fn record_epoch_after(
    manifold_dir: &Path,
    candidate: &maw_core::model::types::GitOid,
) -> Result<()> {
    let state_path = MergeStateFile::default_path(manifold_dir);
    let mut state =
        MergeStateFile::read(&state_path).map_err(|e| anyhow::anyhow!("read merge-state: {e}"))?;
    state.epoch_after = Some(
        maw_core::model::types::EpochId::new(candidate.as_str())
            .map_err(|e| anyhow::anyhow!("invalid candidate OID: {e}"))?,
    );
    state.updated_at = now_secs();
    state
        .write_atomic(&state_path)
        .map_err(|e| anyhow::anyhow!("write merge-state: {e}"))?;
    Ok(())
}

fn record_merge_target_context(
    manifold_dir: &Path,
    target_branch: &str,
    epoch_before_override: Option<&EpochId>,
) -> Result<()> {
    let state_path = MergeStateFile::default_path(manifold_dir);
    let mut state =
        MergeStateFile::read(&state_path).map_err(|e| anyhow::anyhow!("read merge-state: {e}"))?;
    state.target_branch = Some(target_branch.to_owned());
    if let Some(epoch_before) = epoch_before_override {
        state.epoch_before = epoch_before.clone();
    }
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

/// Handle `maw ws merge --abort`.
///
/// Clears an orphaned/stuck `.manifold/merge-state.json` so merges can run
/// again after a killed/OOM'd/panicked/Ctrl-C'd merge (bn-2wyh). Upholds the
/// Prime Invariant: refuses to clear if the merge already passed COMMIT
/// (epoch / target branch advanced to the merged commit), because clearing
/// then could mask committed work.
///
/// # Errors
/// Returns an error on I/O / deserialization failure, or (with a non-zero
/// exit) when the abort is refused for safety.
pub fn abort_in_progress_merge(root: &Path, fmt: OutputFormat) -> Result<()> {
    let manifold_dir =
        maw_core::model::layout::LayoutFlavor::detect_with_env(root).manifold_dir(root);
    let state_path = MergeStateFile::default_path(&manifold_dir);
    let text_mode = fmt != OutputFormat::Json;

    // Observe the current epoch and (if recorded) target branch head so the
    // core abort logic can apply the Prime-Invariant gate without any git
    // dependency of its own.
    let current_epoch = maw_core::refs::read_epoch_current(root)
        .ok()
        .flatten()
        .map(|o| o.as_str().to_owned());

    let current_target_head = match MergeStateFile::read(&state_path) {
        Ok(state) => state.target_branch.and_then(|branch| {
            let branch_ref = format!("refs/heads/{branch}");
            maw_core::refs::read_ref(root, &branch_ref)
                .ok()
                .flatten()
                .map(|o| o.as_str().to_owned())
        }),
        Err(_) => None,
    };

    let outcome = abort_merge_state(
        &state_path,
        current_epoch.as_deref(),
        current_target_head.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!("merge --abort failed to read merge-state: {e}"))?;

    match outcome {
        AbortOutcome::NothingToAbort => {
            if fmt == OutputFormat::Json {
                println!(
                    "{}",
                    serde_json::json!({
                        "aborted": false,
                        "reason": "no merge-state file",
                    })
                );
            } else {
                println!(
                    "No merge in progress \u{2014} nothing to abort.\n  \
                     Next: maw ws merge <workspaces> --into <target> --message \"...\""
                );
            }
            Ok(())
        }
        AbortOutcome::Cleared { from } => {
            if fmt == OutputFormat::Json {
                println!(
                    "{}",
                    serde_json::json!({
                        "aborted": true,
                        "phase": from.to_string(),
                    })
                );
            } else {
                println!(
                    "Cleared orphaned merge-state (was in phase: {from}).\n  \
                     The interrupted merge made no committed changes (pre-COMMIT), so no \
                     work was lost.\n  \
                     Next: re-run your merge \u{2014} maw ws merge <workspaces> --into \
                     <target> --message \"...\""
                );
            }
            Ok(())
        }
        AbortOutcome::RefusedPostCommit { phase, reason } => {
            // Refuse loudly. This is the Prime-Invariant guardrail: do NOT
            // delete state that might be masking committed work.
            if fmt == OutputFormat::Json {
                println!(
                    "{}",
                    serde_json::json!({
                        "aborted": false,
                        "phase": phase.to_string(),
                        "reason": reason,
                    })
                );
            }
            let _ = text_mode;
            bail!(
                "Refused to abort merge: {reason}.\n  \
                 The merge reached phase '{phase}' and may have committed work \u{2014} \
                 clearing merge-state now could orphan it (violates the Prime Invariant).\n  \
                 To inspect what was committed: maw ws recover\n  \
                 Check epoch/branch state: maw status && maw doctor\n  \
                 If you have confirmed the refs did NOT advance, remove \
                 .manifold/merge-state.json manually as a last resort."
            )
        }
    }
}

/// Get current Unix timestamp in seconds.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn is_ancestor_commit(
    ws_path: &Path,
    maybe_ancestor: &str,
    maybe_descendant: &str,
) -> Result<bool> {
    let repo = maw_git::GixRepo::open(ws_path)
        .with_context(|| format!("failed to open repo at {}", ws_path.display()))?;
    let ancestor = repo
        .rev_parse(maybe_ancestor)
        .with_context(|| format!("rev-parse '{maybe_ancestor}' failed"))?;
    let descendant = repo
        .rev_parse(maybe_descendant)
        .with_context(|| format!("rev-parse '{maybe_descendant}' failed"))?;
    repo.is_ancestor(ancestor, descendant)
        .context("is_ancestor failed")
}

fn merge_base_commit(ws_path: &Path, left: &str, right: &str) -> Result<Option<String>> {
    let repo = maw_git::GixRepo::open(ws_path)
        .with_context(|| format!("failed to open repo at {}", ws_path.display()))?;
    let Some(l) = repo.rev_parse_opt(left).context("rev-parse left failed")? else {
        return Ok(None);
    };
    let Some(r) = repo
        .rev_parse_opt(right)
        .context("rev-parse right failed")?
    else {
        return Ok(None);
    };
    let base = repo.merge_base(l, r).context("merge_base failed")?;
    Ok(base.map(|o| o.to_string()))
}

#[derive(Debug, Clone)]
struct ActiveChangeHead {
    change_id: String,
    change_branch: String,
    head_oid: String,
}

pub fn resolve_workspace_head_oid(ws_path: &Path) -> Result<String> {
    let repo = maw_git::GixRepo::open(ws_path)
        .with_context(|| format!("failed to open repo at {}", ws_path.display()))?;
    let oid = repo
        .rev_parse("HEAD")
        .context("failed to resolve workspace HEAD")?;
    Ok(oid.to_string())
}

fn active_change_heads_not_on_branch(
    root: &Path,
    target_branch: &str,
) -> Result<Vec<ActiveChangeHead>> {
    let store = ChangesStore::open(root);
    let active_changes = store
        .list_active_records()
        .context("Failed to read active changes")?;

    let repo = maw_git::GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;
    let target_ref = format!("refs/heads/{target_branch}");
    let target_head = repo
        .rev_parse_opt(&target_ref)
        .map_err(|e| anyhow::anyhow!("failed to read {target_ref}: {e}"))?
        .map(|oid| oid.to_string());

    let mut out = Vec::new();
    for record in active_changes {
        let change_branch = record.git.change_branch.trim().to_string();
        if change_branch.is_empty() || change_branch == target_branch {
            continue;
        }

        let change_ref = format!("refs/heads/{change_branch}");
        let Some(change_head) = repo
            .rev_parse_opt(&change_ref)
            .map_err(|e| anyhow::anyhow!("failed to read {change_ref}: {e}"))?
        else {
            continue;
        };
        let change_head_oid = change_head.to_string();

        if let Some(target_head_oid) = target_head.as_deref()
            && is_ancestor_commit(root, &change_head_oid, target_head_oid)?
        {
            // Already landed on target branch; not a cross-target risk.
            continue;
        }

        out.push(ActiveChangeHead {
            change_id: record.change_id,
            change_branch,
            head_oid: change_head_oid,
        });
    }

    Ok(out)
}

fn guard_unbound_sources_against_active_change_ancestry(
    root: &Path,
    target_branch: &str,
    source_workspaces: &[String],
) -> Result<()> {
    let risky_change_heads = active_change_heads_not_on_branch(root, target_branch)?;
    if risky_change_heads.is_empty() {
        return Ok(());
    }

    let target_ref = format!("refs/heads/{target_branch}");
    let target_head_oid = maw_core::refs::read_ref(root, &target_ref)?;

    for ws_name in source_workspaces {
        let ws_meta = super::metadata::read(root, ws_name).unwrap_or_default();
        if ws_meta.change_id.is_some() {
            continue;
        }

        let ws_path = root.join("ws").join(ws_name);
        let ws_head = resolve_workspace_head_oid(&ws_path)?;

        for change in &risky_change_heads {
            if is_ancestor_commit(root, &change.head_oid, &ws_head)? {
                bail!(
                    "Workspace '{}' is not bound to a change, but its HEAD includes active change '{}' (branch '{}') which is not yet on '{}'.\n  \
                     Refusing merge into '{}' to avoid promoting change-only commits to trunk.\n  \
                     To fix: merge this workspace into its change target, or recreate it with an explicit source that does not include '{}'.",
                    ws_name,
                    change.change_id,
                    change.change_branch,
                    target_branch,
                    target_branch,
                    change.change_branch
                );
            }

            let Some(target_head) = target_head_oid
                .as_ref()
                .map(maw_core::model::types::GitOid::as_str)
            else {
                continue;
            };

            let Some(common_ancestor) = merge_base_commit(root, &change.head_oid, &ws_head)? else {
                continue;
            };

            if !is_ancestor_commit(root, &common_ancestor, target_head)? {
                let short_common = &common_ancestor[..common_ancestor.len().min(12)];
                bail!(
                    "Workspace '{}' is not bound to a change, but its HEAD shares unmerged ancestry with active change '{}' (branch '{}') via commit '{}' not yet on '{}'.\n  \
                     Refusing merge into '{}' to avoid promoting change-only commits to trunk.\n  \
                     To fix: merge this workspace into its change target, or recreate it with an explicit source that does not include '{}'.",
                    ws_name,
                    change.change_id,
                    change.change_branch,
                    short_common,
                    target_branch,
                    target_branch,
                    change.change_branch
                );
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers: workspace management
// ---------------------------------------------------------------------------

/// Update the default workspace to check out the new epoch commit.
///
/// Uses the snapshot-based composable helpers to safely update the default
/// workspace's working copy, preserving uncommitted user work (working-copy-preserving).
///
/// Algorithm:
/// 1. SNAPSHOT — if dirty, capture via `snapshot_working_copy()` (stash create
///    + pinned ref, no stash-stack pollution).
/// 2. CHECKOUT — native `checkout_to()` with branch attachment (tree is clean
///    after snapshot; uses `checkout_tree + set_head_to_branch`, no shell-out).
///    (bn-8flz)
/// 3. REPLAY — `replay_snapshot()` applies the snapshot. Conflicts become
///    markers in the working tree (working-copy-preserving — conflicts are data, not errors).
/// 4. CLEANUP — if replay was clean, delete the snapshot ref. If conflicts,
///    KEEP the ref as a recovery anchor and print conflicted file list.
///
/// CRITICAL: errors and conflicts during replay never abort the cleanup phase.
/// The merge COMMIT has already succeeded — remaining cleanup (workspace
/// destroy, GC, merge-state removal) MUST still run.
#[expect(
    clippy::too_many_arguments,
    reason = "cleanup step needs explicit merge context from earlier phases"
)]
#[expect(
    clippy::too_many_lines,
    reason = "cleanup step preserves failure-handling order after merge commit"
)]
fn update_default_workspace(
    default_ws_path: &Path,
    ws_name: &str,
    branch: &str,
    epoch_before: &str,
    epoch_after: &str,
    workspace_base_before: Option<&str>,
    repo_root: &Path,
    target_updates_epoch: bool,
    text_mode: bool,
    resolved_paths: &[PathBuf],
    source_workspace_names: &[String],
) -> Result<()> {
    use super::working_copy::{
        SnapshotReplayResult, checkout_to, cleanup_snapshot, replay_snapshot_with_merge_protection,
        snapshot_working_copy,
    };

    let record_workspace_epoch = || {
        let Ok(oid) = maw_core::model::types::GitOid::new(epoch_after) else {
            return;
        };
        let epoch_ref = maw_core::refs::workspace_epoch_ref(ws_name);
        if let Err(e) = maw_core::refs::write_ref(repo_root, &epoch_ref, &oid) {
            tracing::warn!(
                "failed to update workspace epoch ref '{}' for '{}': {e}",
                epoch_ref,
                ws_name
            );
        }
    };

    let updated_message = || {
        if target_updates_epoch {
            if ws_name == "default" {
                "Default workspace updated to new epoch.".to_string()
            } else {
                format!("Workspace '{ws_name}' updated to new epoch.")
            }
        } else {
            format!("Workspace '{ws_name}' updated to branch '{branch}'.")
        }
    };

    let fallback_anchor = epoch_before.to_owned();
    let anchor_epoch = if let Some(base_before) = workspace_base_before {
        if base_before == epoch_before {
            base_before.to_owned()
        } else {
            match is_ancestor_commit(default_ws_path, base_before, epoch_before) {
                Ok(true) => base_before.to_owned(),
                Ok(false) => fallback_anchor,
                Err(e) => {
                    tracing::warn!(
                        "failed to verify target workspace base ancestry ({} -> {}): {e}",
                        base_before,
                        epoch_before
                    );
                    fallback_anchor
                }
            }
        }
    } else {
        let ref_name = maw_core::refs::workspace_epoch_ref(ws_name);
        match maw_core::refs::read_ref(repo_root, &ref_name) {
            Ok(Some(ws_epoch_oid)) => {
                let ws_epoch = ws_epoch_oid.as_str();
                if ws_epoch == epoch_before {
                    fallback_anchor
                } else {
                    match is_ancestor_commit(default_ws_path, ws_epoch, epoch_before) {
                        Ok(true) => ws_epoch.to_owned(),
                        Ok(false) => fallback_anchor,
                        Err(e) => {
                            tracing::warn!(
                                "failed to verify default workspace epoch ancestry ({} -> {}): {e}",
                                ws_epoch,
                                epoch_before
                            );
                            fallback_anchor
                        }
                    }
                }
            }
            _ => fallback_anchor,
        }
    };

    // Step 0: ANCHOR — detach HEAD at the default workspace base epoch
    // (or epoch_before fallback) without touching the
    // working tree.
    //
    // The COMMIT phase has already moved the branch ref to the new epoch, but
    // the working tree still has the old epoch's files (possibly with user
    // edits). If we snapshot now, `git stash create` would capture deltas
    // relative to the NEW epoch (the branch's current target), which includes
    // spurious "reversions" of the merge results.
    //
    // We need HEAD at the default workspace's own base epoch so the stash
    // captures only the ACTUAL user changes relative to that workspace state.
    //
    // When the global epoch advances via a non-default target, default may legitimately lag behind
    // epoch_before. Anchoring at epoch_before in that case turns legitimate
    // missing files into synthetic deletions during replay.
    //
    // We can't use `git checkout --detach` because it updates the working
    // tree (which fails with dirty files or destroys them with --force).
    // Instead we write the raw OID to the worktree's HEAD file — the
    // standard git plumbing for detaching without a tree update.
    {
        let ws_repo = maw_git::GixRepo::open(default_ws_path)
            .with_context(|| format!("failed to open repo at {}", default_ws_path.display()))?;
        let head_path = ws_repo.git_dir().join("HEAD");
        std::fs::write(&head_path, format!("{anchor_epoch}\n"))
            .with_context(|| format!("failed to write detached HEAD to {}", head_path.display()))?;

        // Reset the index to match HEAD (anchor_epoch) without touching the
        // working tree. This ensures gix `status()` / `worktree_state_commit`
        // see only user changes. Re-open after the HEAD rewrite so unstage_all
        // observes the new HEAD.
        let ws_repo_post = maw_git::GixRepo::open(default_ws_path).with_context(|| {
            format!(
                "failed to re-open repo at {} after HEAD rewrite",
                default_ws_path.display()
            )
        })?;
        if let Err(e) = ws_repo_post.unstage_all() {
            tracing::warn!("unstage_all failed during anchor step: {e}");
        }
    }

    // bn-1xmk: capture the trunk's uncommitted content in MEMORY before any
    // snapshot/checkout touches the tree. HEAD is now anchored at `anchor_epoch`
    // and the index is reset to it, so this is the true set of user edits
    // relative to the workspace's own base. This map is the authoritative user
    // state and the ultimate backstop for the post-replay fidelity repair
    // below — it does not depend on `git stash create` succeeding.
    let pre_merge_dirty = capture_pre_merge_dirty(default_ws_path);
    if !pre_merge_dirty.is_empty() {
        // Visibility: one loud line so a dirty-trunk merge is never silent.
        eprintln!(
            "  preserving {} uncommitted trunk file(s) across merge (recovery snapshot pinned)",
            pre_merge_dirty.len()
        );
    }

    // Durable recovery ref (bn-1xmk): pinned under refs/manifold/recovery/<ws>/
    // so `maw ws recover` lists it as a "pinned" row. Kept even on a clean
    // replay (subject to normal gc), unlike the ephemeral snapshot ref which is
    // deleted on clean replay and is invisible to recover.
    let mut durable_recovery_ref: Option<String> = None;

    // Step 1: SNAPSHOT — capture dirty state if any.
    let snapshot = match snapshot_working_copy(default_ws_path, repo_root, ws_name) {
        Ok(snap) => snap,
        Err(e) => {
            // Snapshot failed. The COMMIT already succeeded so we must not
            // abort, but we also must not silently lose the user's edits.
            // Pin a durable recovery ref from the in-memory pre-merge content
            // (item a: the stash-None-despite-dirty case now surfaces here),
            // then force-checkout and repair committed-unchanged files from
            // memory so an untouched dirty trunk file survives byte-for-byte.
            eprintln!("  WARNING: snapshot_working_copy failed: {e:#}");
            durable_recovery_ref =
                pin_pre_merge_recovery_ref(repo_root, ws_name, &anchor_epoch, &pre_merge_dirty);
            eprintln!("  Falling back to force checkout (recovering trunk edits from memory)...");
            force_checkout_fallback(default_ws_path, ws_name, branch, text_mode);
            lfs_post_checkout(default_ws_path, epoch_after);
            verify_trunk_replay_fidelity(
                default_ws_path,
                ws_name,
                &pre_merge_dirty,
                &anchor_epoch,
                epoch_after,
                durable_recovery_ref.as_deref(),
            );
            record_workspace_epoch();
            return Ok(());
        }
    };

    // Pin the durable recovery ref from the ephemeral snapshot's commit (the
    // ADMIN-safe stash built by `snapshot_working_copy`) so recovery survives a
    // clean replay too.
    if let Some(snap) = &snapshot {
        durable_recovery_ref =
            pin_recovery_ref_from_oid(repo_root, ws_name, &snap.oid).or_else(|| {
                pin_pre_merge_recovery_ref(repo_root, ws_name, &anchor_epoch, &pre_merge_dirty)
            });
    }

    // Step 2: CHECKOUT — switch to the branch (tree is clean after snapshot).
    if let Err(e) = checkout_to(default_ws_path, branch, Some(branch)) {
        // Checkout failed — fall back to force checkout.
        // This can happen when the merge introduces files that exist as
        // untracked in the (now-cleaned) working tree. `git checkout`
        // refuses to overwrite them, but they were already captured in the
        // snapshot. Force checkout gets us to the right tree; the snapshot
        // replay below will restore the user's versions and surface
        // conflicts. (bn-2fk0)
        tracing::warn!("checkout_to failed, using force checkout: {e:#}");
        force_checkout_fallback(default_ws_path, ws_name, branch, text_mode);
    }

    // LFS post-checkout pass: `checkout_to` now uses the native `checkout_tree`
    // (bn-8flz — no more git checkout CLI). Run our native smudge post-pass to
    // ensure every LFS-tracked file has SOMETHING on disk (real content if
    // object exists, pointer text if not). Pass epoch_after because HEAD may
    // still be at the old epoch if checkout_to failed and we fell through to
    // force_checkout_fallback.
    lfs_post_checkout(default_ws_path, epoch_after);

    // Step 3: REPLAY — if there was a snapshot, replay it.
    let Some(snapshot) = snapshot else {
        // Clean workspace — checkout was enough.
        record_workspace_epoch();
        if text_mode {
            println!("  {}", updated_message());
        }
        return Ok(());
    };

    // Always route through the merge-protection replay when there is a
    // snapshot. It self-delegates to the plain `replay_snapshot` when nothing
    // overlaps, and otherwise runs driver-aware 3-way merges. This closes the
    // mode-(b) gap where a dirty trunk file whose committed content changed
    // (e.g. absorbed out-of-maw commits) was restored by a bare `stash_apply`
    // that bypassed `merge=union` drivers (bn-1xmk).
    let used_merge_protection = true;
    let replay_result = replay_snapshot_with_merge_protection(
        default_ws_path,
        &snapshot,
        resolved_paths,
        &anchor_epoch,
        epoch_after,
        source_workspace_names,
        ws_name,
    );

    match replay_result {
        Ok(SnapshotReplayResult::Clean) => {
            // Step 4a: CLEANUP — replay was clean, delete snapshot ref.
            if let Err(e) = cleanup_snapshot(repo_root, ws_name) {
                // Non-fatal — the ref is harmless, just orphaned.
                tracing::warn!("failed to clean up snapshot ref: {e}");
            }
            if text_mode {
                println!("  {}", updated_message());
                println!("  User work replayed successfully.");
            }
        }
        Ok(SnapshotReplayResult::Conflicts(conflicts)) if used_merge_protection => {
            // Step 4b-protected: Conflicts from merge-vs-local overlap.
            // The merge succeeded and the epoch advanced. These conflicts
            // are between the merge result and uncommitted local edits that
            // were in the target workspace before the merge.
            let source_label = if source_workspace_names.len() == 1 {
                source_workspace_names[0].clone()
            } else {
                source_workspace_names.join(", ")
            };
            eprintln!();
            eprintln!(
                "  WARNING: {} file(s) in '{}' have local-vs-merge conflicts.",
                conflicts.len(),
                ws_name,
            );
            eprintln!();
            eprintln!("  What happened: '{ws_name}' had uncommitted edits to files that");
            eprintln!("  '{source_label}' also modified. Both versions are in conflict markers:");
            eprintln!();
            eprintln!("    <<<<<<< {source_label}   — from the merged workspace");
            eprintln!("    ||||||| base");
            eprintln!("    =======");
            eprintln!("    >>>>>>> {ws_name}   — uncommitted edits in {ws_name}");
            eprintln!();
            for c in &conflicts {
                eprintln!("    [{:>20}] {}", c.conflict_type, c.path);
            }
            eprintln!();
            eprintln!("  To fix (pick one):");
            eprintln!(
                "    maw ws resolve {ws_name} --keep {source_label}    # keep merged version"
            );
            eprintln!("    maw ws resolve {ws_name} --keep {ws_name}    # keep local edits");
            eprintln!("    maw ws resolve {ws_name} --keep both    # keep both sides concatenated");
            eprintln!(
                "    maw ws resolve {ws_name} --list                  # list conflicted files"
            );
            eprintln!();
            eprintln!("  The merge commit is safe — epoch has advanced. These conflicts only");
            eprintln!("  affect the working copy in ws/{ws_name}/.");
            if text_mode {
                println!(
                    "  {} ({} local-vs-merge conflict(s) — see above to resolve).",
                    updated_message(),
                    conflicts.len()
                );
            }
        }
        Ok(SnapshotReplayResult::Conflicts(conflicts)) => {
            // Step 4b: Regular stash-replay conflicts (no merge protection).
            // KEEP snapshot ref as recovery anchor.
            eprintln!(
                "  WARNING: {} file(s) have conflicts after replay onto updated target:",
                conflicts.len()
            );
            for c in &conflicts {
                eprintln!("    [{:>20}] {}", c.conflict_type, c.path);
            }
            eprintln!("  Snapshot preserved at: {}", snapshot.ref_name);
            eprintln!(
                "  To recover clean state: git -C {} stash apply {}",
                default_ws_path.display(),
                snapshot.oid,
            );
            if text_mode {
                println!(
                    "  {} ({} conflict(s) — resolve manually).",
                    updated_message(),
                    conflicts.len()
                );
            }
        }
        Err(e) => {
            // Replay hard-failed. Snapshot ref is kept for recovery.
            eprintln!("  WARNING: replay_snapshot failed: {e:#}");
            eprintln!("  Snapshot preserved at: {}", snapshot.ref_name);
            eprintln!(
                "  To recover: git -C {} stash apply {}",
                default_ws_path.display(),
                snapshot.oid,
            );
            if text_mode {
                println!(
                    "  {} (replay failed, snapshot preserved).",
                    updated_message()
                );
            }
        }
    }

    // bn-1xmk: post-replay fidelity verification. For every trunk file that was
    // uncommitted-dirty before the merge AND whose committed content the merge
    // did NOT change, the user's uncommitted version is the sole authority and
    // must be present on disk byte-for-byte. If the replay produced anything
    // else (the events-journal clobber class), repair it from the in-memory
    // pre-merge content and print a loud, actionable recovery pointer.
    verify_trunk_replay_fidelity(
        default_ws_path,
        ws_name,
        &pre_merge_dirty,
        &anchor_epoch,
        epoch_after,
        durable_recovery_ref.as_deref(),
    );

    record_workspace_epoch();

    Ok(())
}

/// Last-resort force checkout when snapshot/replay fails.
///
/// This is the nuclear option — it destroys any uncommitted changes.
/// Only used when the snapshot-based path itself has errored.
fn force_checkout_fallback(ws_path: &Path, ws_name: &str, branch: &str, text_mode: bool) {
    // Native: checkout_to_branch = checkout_tree + set_head_to_branch (atomic,
    // with reflog). Previously used checkout_tree + raw HEAD file write; now
    // uses the unified primitive so HEAD is written atomically with a reflog
    // entry. (bn-8flz)
    let try_fallback = || -> Result<()> {
        let repo = maw_git::GixRepo::open(ws_path)
            .with_context(|| format!("failed to open workspace repo at {}", ws_path.display()))?;
        let commit = repo
            .rev_parse(branch)
            .with_context(|| format!("rev-parse '{branch}' failed"))?;
        repo.checkout_to_branch(commit, ws_path, branch)
            .with_context(|| format!("checkout_to_branch '{branch}' failed"))?;
        Ok(())
    };

    match try_fallback() {
        Ok(()) => {
            if text_mode {
                println!("  Workspace '{ws_name}' updated via force checkout fallback.");
            }
        }
        Err(e) => {
            eprintln!(
                "  WARNING: Fallback checkout also failed: {e:#}\n  \
                 The merge COMMIT succeeded (refs are updated), but workspace '{}' \
                 working copy could not be checked out.\n  \
                 To fix: git -C {} checkout {branch}",
                ws_name,
                ws_path.display(),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// bn-1xmk: durable trunk-snapshot + honest replay helpers
// ---------------------------------------------------------------------------

/// Capture the trunk's uncommitted content in memory (path -> bytes, or `None`
/// for a path the user deleted). Read against the *current* HEAD, which the
/// caller has already anchored at the workspace's base epoch, so this is the
/// true set of user edits relative to that base. Best-effort: on any read
/// failure the offending path is skipped rather than aborting the merge (the
/// COMMIT has already succeeded).
fn capture_pre_merge_dirty(ws_path: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    let Ok(repo) = maw_git::GixRepo::open(ws_path) else {
        return Vec::new();
    };
    let Ok(entries) = repo.status_head_to_worktree() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries {
        let rel = PathBuf::from(&entry.path);
        // Never treat admin/git trees as recoverable user content.
        if rel
            .components()
            .next()
            .and_then(|c| c.as_os_str().to_str())
            .is_some_and(|first| matches!(first, ".maw" | "repo.git" | ".manifold" | ".git"))
        {
            continue;
        }
        let full = ws_path.join(&rel);
        let bytes = if full.is_file() {
            std::fs::read(&full).ok()
        } else {
            None
        };
        out.push((rel, bytes));
    }
    out
}

/// Pin a durable recovery ref pointing at an existing commit/stash OID under
/// `refs/manifold/recovery/<ws>/<ts>` so `maw ws recover` surfaces it as a
/// pinned row. Returns the ref name on success. Best-effort: logs and returns
/// `None` on failure.
fn pin_recovery_ref_from_oid(repo_root: &Path, ws_name: &str, oid: &str) -> Option<String> {
    let git_oid = match GitOid::new(oid) {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("bn-1xmk: invalid snapshot OID '{oid}' for recovery pin: {e}");
            return None;
        }
    };
    let ref_name = super::capture::recovery_ref(ws_name, &super::now_timestamp_iso8601_precise());
    match maw_core::refs::write_ref(repo_root, &ref_name, &git_oid) {
        Ok(()) => {
            tracing::info!(ref_name = %ref_name, oid = %oid, "bn-1xmk: pinned durable trunk recovery ref");
            Some(ref_name)
        }
        Err(e) => {
            tracing::warn!("bn-1xmk: failed to pin durable recovery ref '{ref_name}': {e}");
            None
        }
    }
}

/// Pin a durable recovery ref built from in-memory pre-merge content, for the
/// case where `snapshot_working_copy` could not produce a stash commit (item a).
/// Applies the user's dirty blobs onto the anchor-epoch tree and commits it, so
/// the edits are recoverable even when `git stash create` refused. Best-effort:
/// returns `None` on any failure.
fn pin_pre_merge_recovery_ref(
    repo_root: &Path,
    ws_name: &str,
    anchor_epoch: &str,
    pre_merge_dirty: &[(PathBuf, Option<Vec<u8>>)],
) -> Option<String> {
    if pre_merge_dirty.is_empty() {
        return None;
    }
    let repo = maw_git::GixRepo::open(repo_root).ok()?;
    let base_tree = repo
        .rev_parse(anchor_epoch)
        .ok()
        .and_then(|c| repo.read_commit(c).ok())
        .map(|c| c.tree_oid)?;

    let mut edits: Vec<maw_git::TreeEdit> = Vec::new();
    for (path, bytes) in pre_merge_dirty {
        let path_str = path.to_string_lossy().replace('\\', "/");
        match bytes {
            Some(bytes) => {
                if let Ok(blob) = repo.write_blob(bytes) {
                    edits.push(maw_git::TreeEdit::Upsert {
                        path: path_str,
                        mode: maw_git::EntryMode::Blob,
                        oid: blob,
                    });
                }
            }
            None => edits.push(maw_git::TreeEdit::Remove { path: path_str }),
        }
    }
    if edits.is_empty() {
        return None;
    }
    let tree = repo.edit_tree(base_tree, &edits).ok()?;
    let anchor_oid = repo.rev_parse(anchor_epoch).ok()?;
    let commit = repo
        .create_commit(
            tree,
            &[anchor_oid],
            "bn-1xmk: pre-merge trunk snapshot (in-memory recovery)",
            None,
        )
        .ok()?;
    pin_recovery_ref_from_oid(repo_root, ws_name, &commit.to_string())
}

/// Post-replay fidelity check: every pre-merge-dirty trunk path whose committed
/// content the merge did NOT change must end up with exactly the user's
/// uncommitted bytes on disk. On any mismatch, repair from memory and print a
/// loud, actionable recovery pointer. This is the guard that turns the
/// events-journal silent-clobber class into an impossible-to-miss, self-healing
/// event (bn-1xmk).
fn verify_trunk_replay_fidelity(
    ws_path: &Path,
    ws_name: &str,
    pre_merge_dirty: &[(PathBuf, Option<Vec<u8>>)],
    anchor_epoch: &str,
    epoch_after: &str,
    recovery_ref: Option<&str>,
) {
    let Ok(repo) = maw_git::GixRepo::open(ws_path) else {
        return;
    };
    for (path, pre_bytes) in pre_merge_dirty {
        // Only files the merge left committed-unchanged are unambiguously owned
        // by the user's uncommitted edits. If the merge changed the committed
        // content, the driver-aware 3-way replay owns the outcome.
        let committed_anchor = repo.read_file_at_commit(anchor_epoch, path).ok().flatten();
        let committed_after = repo.read_file_at_commit(epoch_after, path).ok().flatten();
        if committed_anchor != committed_after {
            continue;
        }

        let full = ws_path.join(path);
        let final_bytes = if full.is_file() {
            std::fs::read(&full).ok()
        } else {
            None
        };
        let matches = match (pre_bytes, &final_bytes) {
            (Some(a), Some(b)) => a == b,
            (None, None) => true,
            _ => false,
        };
        if matches {
            continue;
        }

        // Data-loss detected — repair from the authoritative in-memory content.
        let repaired = pre_bytes.as_ref().map_or_else(
            || !full.is_file() || std::fs::remove_file(&full).is_ok(),
            |bytes| {
                if let Some(parent) = full.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&full, bytes).is_ok()
            },
        );

        eprintln!();
        eprintln!(
            "  WARNING (bn-1xmk): replay did not reproduce your uncommitted edits to '{}'.",
            path.display()
        );
        if repaired {
            eprintln!("  Your version was restored from the in-memory pre-merge snapshot.");
        } else {
            eprintln!("  Automatic repair FAILED — recover it manually (see below).");
        }
        match recovery_ref {
            Some(r) => {
                eprintln!("  Recovery ref: {r}");
                eprintln!("  Inspect:  maw ws recover {ws_name}");
                eprintln!(
                    "  Restore:  maw ws recover --ref {r} --restore-file {}",
                    path.display()
                );
            }
            None => {
                eprintln!("  Inspect recovery snapshots: maw ws recover {ws_name}");
            }
        }
    }
}

/// Run the native LFS smudge post-pass on a workspace after a `git checkout`
/// CLI call. Best-effort: logs and continues on error.
#[cfg(feature = "lfs")]
fn lfs_post_checkout(ws_path: &std::path::Path, target_commit: &str) {
    if let Err(e) = maw_git::lfs_smudge_worktree_at(ws_path, target_commit) {
        tracing::debug!("lfs post-checkout: {e}");
    }
}

#[cfg(not(feature = "lfs"))]
fn lfs_post_checkout(_ws_path: &std::path::Path, _target_commit: &str) {}

/// Handle post-merge workspace destruction with confirmation check.
#[expect(
    clippy::too_many_lines,
    reason = "post-merge destroy handles confirmation, backend cleanup, and reporting"
)]
fn handle_post_merge_destroy(
    ws_to_merge: &[String],
    default_ws: &str,
    confirm: bool,
    backend: &impl WorkspaceBackend<Error: std::fmt::Display>,
    root: &Path,
    text_mode: bool,
    verbose: bool,
) -> Result<()> {
    let ws_to_destroy: Vec<String> = ws_to_merge
        .iter()
        .filter(|ws| ws.as_str() != default_ws)
        .cloned()
        .collect();

    if confirm {
        if text_mode {
            println!();
            println!("Will destroy {} workspace(s):", ws_to_destroy.len());
            for ws in &ws_to_destroy {
                println!("  - {ws}");
            }
            println!();
        }
        print!("Continue? [y/N] ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            if text_mode {
                println!("Aborted. Workspaces kept. Merge commit still exists.");
            }
            return Ok(());
        }
    }

    if text_mode {
        println!("  Cleaning up workspaces...");
    }
    // Belt-and-braces (C4 from sg3-layout-design §2.2): in addition to the
    // by-name guard above, canonicalize the resolved workspace path and
    // refuse to destroy if it canonicalizes to the privileged root. This
    // defends against a future caller passing the root path as a "source"
    // in the consolidated layout (where root IS the merge target).
    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    for ws_name in &ws_to_destroy {
        if ws_name == default_ws {
            if text_mode {
                println!("    Skipping merge target workspace");
            }
            continue;
        }
        let Ok(ws_id) = WorkspaceId::new(ws_name) else {
            eprintln!("    WARNING: Invalid workspace name '{ws_name}', skipping");
            continue;
        };

        // --- Step 1: Get workspace metadata (path + base epoch) ---
        let ws_path = backend.workspace_path(&ws_id);

        // C4 path-based guard: never destroy the repo root, regardless of name.
        let canonical_ws = std::fs::canonicalize(&ws_path).unwrap_or_else(|_| ws_path.clone());
        if canonical_ws == canonical_root {
            if text_mode {
                println!("    Skipping '{ws_name}': resolves to repo root (privileged target)");
            }
            continue;
        }
        let base_epoch = match backend.status(&ws_id) {
            Ok(status) => status.base_epoch.to_epoch_id(),
            Err(e) => {
                eprintln!("    WARNING: Could not check workspace status before destroy: {e}");
                eprintln!(
                    "    Workspace '{ws_name}' preserved for manual cleanup. \
                     The merge itself succeeded."
                );
                // Emit structured recovery failure for agent parsing
                super::capture::emit_recovery_surface_failed(
                    ws_name, &e, true, // commit already succeeded before destroy
                );
                continue;
            }
        };

        // --- Step 2: Capture dirty state and pin recovery ref ---
        maw::fp!("FP_DESTROY_BEFORE_CAPTURE")?;
        let capture_result = capture_before_destroy(&ws_path, ws_name, base_epoch.oid());
        let capture = match capture_result {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "    WARNING: Failed to capture state for '{ws_name}' before destroy: {e}"
                );
                eprintln!(
                    "    Workspace '{ws_name}' preserved for manual cleanup. \
                     The merge itself succeeded."
                );
                eprintln!(
                    "    HINT: To attempt manual recovery, run: \
                     git -C {ws_path} stash list",
                    ws_path = ws_path.display()
                );
                // Emit structured recovery failure for agent parsing
                super::capture::emit_recovery_surface_failed(
                    ws_name, &e, true, // commit already succeeded before destroy
                );
                continue;
            }
        };

        // --- Step 3: Write append-only destroy record ---
        let final_head =
            super::capture::resolve_head(&ws_path).unwrap_or_else(|_| base_epoch.oid().clone());
        let artifact_path_result = write_destroy_record(
            root,
            ws_name,
            &base_epoch,
            &final_head,
            capture.as_ref(),
            DestroyReason::MergeDestroy,
        );
        if let Err(ref e) = artifact_path_result {
            tracing::warn!("Failed to write destroy record for '{ws_name}': {e}");
        }
        maw::fp!("FP_DESTROY_AFTER_RECORD")?;

        if let Some(ref c) = capture {
            if verbose && text_mode {
                println!(
                    "    Captured '{ws_name}' state ({mode}) → {ref_name}",
                    mode = match c.mode {
                        super::capture::CaptureMode::WorktreeCapture => "worktree-snapshot",
                        super::capture::CaptureMode::HeadOnly => "head-only",
                    },
                    ref_name = c.pinned_ref
                );
            }
            if verbose {
                // Emit full recovery surface contract
                super::capture::emit_recovery_surface(
                    ws_name,
                    c,
                    artifact_path_result.as_deref().ok(),
                    true, // commit already succeeded before destroy
                    true, // merge succeeded
                );
            }
        }

        // FP: crash after capture but before workspace deletion.
        // A crash here means the recovery ref is pinned but the workspace
        // still exists on disk.
        maw::fp!("FP_CLEANUP_AFTER_CAPTURE")?;

        // bn-1aey: capture this BEFORE deletion — once the workspace
        // directory is gone, std::env::current_dir() can itself start
        // failing. Reuses the canonical_ws already computed by the C4
        // root-guard above.
        let cwd_was_inside = super::cwd_is_inside(&canonical_ws);

        // --- Step 4: Destroy the workspace ---
        match backend.destroy(&ws_id) {
            Ok(()) => {
                maw::fp!("FP_DESTROY_AFTER_DELETE")?;
                if text_mode {
                    if capture.is_some() {
                        println!(
                            "    Destroyed: {ws_name} (snapshot saved \u{2192} maw ws recover {ws_name})"
                        );
                    } else {
                        println!("    Destroyed: {ws_name}");
                    }
                }
                if cwd_was_inside {
                    eprintln!(
                        "note: your current directory was inside workspace '{ws_name}' which \
                         was just destroyed — cd back to the project root before running more \
                         commands."
                    );
                }
            }
            Err(e) => eprintln!("    WARNING: Failed to destroy {ws_name}: {e}"),
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests — JSON conflict output
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
mod tests {
    use std::path::PathBuf;

    use maw::merge::resolve::{ConflictReason, ConflictRecord, ConflictSide as ResolveSide};
    use maw_core::merge::types::ChangeKind;
    use maw_core::model::conflict::{AtomEdit, ConflictAtom, Region};
    use maw_core::model::types::WorkspaceId;

    use super::*;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn ws_id(name: &str) -> WorkspaceId {
        WorkspaceId::new(name).expect("operation should succeed")
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
        let alice_side = json
            .sides
            .iter()
            .find(|s| s.workspace == "alice")
            .expect("operation should succeed");
        let bob_side = json
            .sides
            .iter()
            .find(|s| s.workspace == "bob")
            .expect("operation should succeed");

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
        let alice = json
            .sides
            .iter()
            .find(|s| s.workspace == "alice")
            .expect("operation should succeed");
        let bob = json
            .sides
            .iter()
            .find(|s| s.workspace == "bob")
            .expect("operation should succeed");

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

        let bob_side = json
            .sides
            .iter()
            .find(|s| s.workspace == "bob")
            .expect("operation should succeed");
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

        let alice_side = json
            .sides
            .iter()
            .find(|s| s.workspace == "alice")
            .expect("operation should succeed");
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
        let json_str =
            serde_json::to_string_pretty(&conflict_json).expect("operation should succeed");
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
        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).expect("operation should succeed");
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
            to_fix: "maw ws merge alice bob --into default".to_string(),
            resolve_command: None,
        };

        let json_str = serde_json::to_string_pretty(&output).expect("operation should succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).expect("operation should succeed");

        assert_eq!(parsed["status"], "conflict");
        assert_eq!(parsed["conflict_count"], 2);
        assert!(parsed["conflicts"].is_array());
        assert_eq!(
            parsed["conflicts"]
                .as_array()
                .expect("operation should succeed")
                .len(),
            2
        );
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
            advice: vec![],
            sibling_conflicts: vec![],
        };

        let json_str = serde_json::to_string_pretty(&output).expect("operation should succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).expect("operation should succeed");

        assert_eq!(parsed["status"], "success");
        assert_eq!(parsed["conflict_count"], 0);
        assert!(
            parsed["conflicts"]
                .as_array()
                .expect("operation should succeed")
                .is_empty()
        );
        assert_eq!(parsed["branch"], "manifold");
        assert_eq!(parsed["next"], "maw push");
        assert_eq!(parsed["unique_count"], 3);
        assert!(parsed["advice"].is_array());
        // sibling_conflicts is skip_serializing_if empty — must not appear.
        assert!(parsed.get("sibling_conflicts").is_none());
    }

    #[test]
    fn merge_success_output_includes_sibling_conflicts_when_present() {
        let output = MergeSuccessOutput {
            status: "success".to_string(),
            workspaces: vec!["alice".to_string()],
            branch: "manifold".to_string(),
            epoch: "a".repeat(40),
            unique_count: 1,
            shared_count: 0,
            resolved_count: 0,
            conflict_count: 0,
            conflicts: vec![],
            message: "Merged to manifold: adopt work from alice".to_string(),
            next: "maw push".to_string(),
            advice: vec![],
            sibling_conflicts: vec!["bob".to_string()],
        };

        let json_str = serde_json::to_string_pretty(&output).expect("operation should succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).expect("operation should succeed");
        assert_eq!(parsed["sibling_conflicts"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["sibling_conflicts"][0], "bob");
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
            to_fix: Some("maw ws merge alice --into default".to_string()),
        };

        let json_str = serde_json::to_string_pretty(&output).expect("operation should succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).expect("operation should succeed");

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

        let json_str = serde_json::to_string_pretty(&output).expect("operation should succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).expect("operation should succeed");

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
        let json_str =
            serde_json::to_string_pretty(&conflict_json).expect("operation should succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).expect("operation should succeed");

        // Agent can identify: WHICH file has a conflict
        assert_eq!(parsed["path"], "src/main.rs");

        // Agent can identify: WHY there is a conflict
        assert!(
            !parsed["reason"]
                .as_str()
                .expect("operation should succeed")
                .is_empty()
        );
        assert!(
            !parsed["reason_description"]
                .as_str()
                .expect("operation should succeed")
                .is_empty()
        );

        // Agent can identify: WHO made each change
        let sides = parsed["sides"]
            .as_array()
            .expect("operation should succeed");
        let workspaces_in_sides: Vec<&str> = sides
            .iter()
            .map(|s| s["workspace"].as_str().expect("operation should succeed"))
            .collect();
        assert!(workspaces_in_sides.contains(&"alice"));
        assert!(workspaces_in_sides.contains(&"bob"));

        // Agent can read: WHAT each side contains
        for side in sides {
            assert!(side["content"].is_string() || side["is_binary"].as_bool().unwrap_or(false));
            assert!(
                !side["workspace"]
                    .as_str()
                    .expect("operation should succeed")
                    .is_empty()
            );
            assert!(
                !side["change"]
                    .as_str()
                    .expect("operation should succeed")
                    .is_empty()
            );
        }

        // Agent has: BASE content for reference
        assert!(parsed["base_content"].is_string());

        // Agent has: LOCALIZED conflict regions (atoms)
        let atoms = parsed["atoms"]
            .as_array()
            .expect("operation should succeed");
        assert!(
            !atoms.is_empty(),
            "atoms should pinpoint the conflict region"
        );
        let atom = &atoms[0];
        assert!(atom["base_region"].is_object());
        assert!(atom["edits"].is_array());
        assert!(atom["reason"].is_object());

        // Agent has: HOW to resolve it
        let strategies = parsed["resolution_strategies"]
            .as_array()
            .expect("operation should succeed");
        assert!(!strategies.is_empty());
        assert!(
            !parsed["suggested_resolution"]
                .as_str()
                .expect("operation should succeed")
                .is_empty()
        );
    }

    /// Verify that missing_base conflicts are also fully parseable.
    #[test]
    fn missing_base_conflict_json_is_parseable() {
        let record = missing_base_record("src/shared.rs");
        let conflict_json = conflict_record_to_json(&record);
        let json_str =
            serde_json::to_string_pretty(&conflict_json).expect("operation should succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).expect("operation should succeed");

        assert_eq!(parsed["reason"], "missing_base");
        assert!(
            parsed["base_content"].is_null(),
            "no base content for missing_base"
        );
        assert!(
            !parsed["sides"]
                .as_array()
                .expect("operation should succeed")
                .is_empty()
        );
        assert!(
            !parsed["suggested_resolution"]
                .as_str()
                .expect("operation should succeed")
                .is_empty()
        );
    }

    // -----------------------------------------------------------------------
    // assign_conflict_ids: determinism and structure
    // -----------------------------------------------------------------------

    #[test]
    fn assign_conflict_ids_deterministic() {
        let records = vec![
            content_record("src/lib.rs", "base", "alice", "bob"),
            add_add_record("src/new.rs"),
        ];
        let ids1 = assign_conflict_ids(&records);
        let ids2 = assign_conflict_ids(&records);

        assert_eq!(ids1.len(), 2);
        assert_eq!(ids2.len(), 2);
        assert_eq!(ids1[0].id, ids2[0].id);
        assert_eq!(ids1[1].id, ids2[1].id);
    }

    #[test]
    fn assign_conflict_ids_have_cf_prefix() {
        let records = vec![content_record("src/lib.rs", "base", "a", "b")];
        let ids = assign_conflict_ids(&records);

        assert!(ids[0].id.starts_with("cf-"), "id should start with cf-");
    }

    #[test]
    fn assign_conflict_ids_atoms_indexed() {
        let records = vec![content_record("src/lib.rs", "base", "a", "b")];
        let ids = assign_conflict_ids(&records);

        // content_record creates 1 atom
        assert_eq!(ids[0].atom_ids.len(), 1);
        assert!(
            ids[0].atom_ids[0].starts_with(&ids[0].id),
            "atom ID should start with file ID"
        );
        assert!(
            ids[0].atom_ids[0].ends_with(".0"),
            "first atom should end with .0"
        );
    }

    #[test]
    fn assign_conflict_ids_different_paths_different_ids() {
        let records = vec![
            content_record("src/lib.rs", "base", "a", "b"),
            content_record("src/main.rs", "base", "a", "b"),
        ];
        let ids = assign_conflict_ids(&records);

        assert_ne!(
            ids[0].id, ids[1].id,
            "different paths should get different IDs"
        );
    }

    // -----------------------------------------------------------------------
    // parse_resolutions: valid and invalid inputs
    // -----------------------------------------------------------------------

    #[test]
    fn parse_resolutions_workspace_name() {
        let raw = vec!["cf-abcd=alice".to_string()];
        let parsed = parse_resolutions(&raw).expect("operation should succeed");

        assert_eq!(parsed.len(), 1);
        assert!(matches!(&parsed["cf-abcd"], Resolution::Workspace(name) if name == "alice"));
    }

    #[test]
    fn parse_resolutions_workspace_name_with_hyphens() {
        let raw = vec!["cf-abcd=my-workspace".to_string()];
        let parsed = parse_resolutions(&raw).expect("operation should succeed");

        assert!(
            matches!(&parsed["cf-abcd"], Resolution::Workspace(name) if name == "my-workspace")
        );
    }

    #[test]
    fn parse_resolutions_content_path() {
        let raw = vec!["cf-abcd=content:/tmp/resolved.rs".to_string()];
        let parsed = parse_resolutions(&raw).expect("operation should succeed");

        assert!(
            matches!(&parsed["cf-abcd"], Resolution::Content(p) if p == Path::new("/tmp/resolved.rs"))
        );
    }

    #[test]
    fn parse_resolutions_atom_level() {
        let raw = vec!["cf-abcd.0=alice".to_string(), "cf-abcd.1=bob".to_string()];
        let parsed = parse_resolutions(&raw).expect("operation should succeed");

        assert_eq!(parsed.len(), 2);
        assert!(matches!(&parsed["cf-abcd.0"], Resolution::Workspace(n) if n == "alice"));
        assert!(matches!(&parsed["cf-abcd.1"], Resolution::Workspace(n) if n == "bob"));
    }

    #[test]
    fn parse_resolutions_invalid_no_equals() {
        let raw = vec!["cf-abcd".to_string()];
        assert!(parse_resolutions(&raw).is_err());
    }

    #[test]
    fn parse_resolutions_invalid_no_prefix() {
        let raw = vec!["abcd=ours".to_string()];
        assert!(parse_resolutions(&raw).is_err());
    }

    #[test]
    fn parse_resolutions_multiple() {
        let raw = vec![
            "cf-aaaa=alice".to_string(),
            "cf-bbbb=bob".to_string(),
            "cf-cccc=content:/tmp/resolved.rs".to_string(),
        ];
        let parsed = parse_resolutions(&raw).expect("operation should succeed");

        assert_eq!(parsed.len(), 3);
        assert!(matches!(&parsed["cf-aaaa"], Resolution::Workspace(n) if n == "alice"));
        assert!(matches!(&parsed["cf-bbbb"], Resolution::Workspace(n) if n == "bob"));
        assert!(matches!(&parsed["cf-cccc"], Resolution::Content(_)));
    }

    // -----------------------------------------------------------------------
    // apply_resolutions: strategy application
    // -----------------------------------------------------------------------

    #[test]
    fn apply_resolutions_first_workspace() {
        let records = vec![add_add_record("new.rs")];
        let conflicts = assign_conflict_ids(&records);
        let id = conflicts[0].id.clone();
        let mut resolutions = BTreeMap::new();
        resolutions.insert(id, Resolution::Workspace("alice".to_string()));

        let ws_dirs = BTreeMap::new();
        let (resolved, remaining) = apply_resolutions(&conflicts, &resolutions, &ws_dirs)
            .expect("operation should succeed");

        assert!(remaining.is_empty());
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[&PathBuf::from("new.rs")], b"alice's version");
    }

    #[test]
    fn apply_resolutions_second_workspace() {
        let records = vec![add_add_record("new.rs")];
        let conflicts = assign_conflict_ids(&records);
        let id = conflicts[0].id.clone();
        let mut resolutions = BTreeMap::new();
        resolutions.insert(id, Resolution::Workspace("bob".to_string()));

        let ws_dirs = BTreeMap::new();
        let (resolved, remaining) = apply_resolutions(&conflicts, &resolutions, &ws_dirs)
            .expect("operation should succeed");

        assert!(remaining.is_empty());
        assert_eq!(resolved[&PathBuf::from("new.rs")], b"bob's version");
    }

    #[test]
    fn apply_resolutions_unresolved_remains() {
        let records = vec![add_add_record("a.rs"), add_add_record("b.rs")];
        let conflicts = assign_conflict_ids(&records);
        let id_a = conflicts[0].id.clone();
        let mut resolutions = BTreeMap::new();
        resolutions.insert(id_a, Resolution::Workspace("alice".to_string()));

        let ws_dirs = BTreeMap::new();
        let (resolved, remaining) = apply_resolutions(&conflicts, &resolutions, &ws_dirs)
            .expect("operation should succeed");

        assert_eq!(resolved.len(), 1);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].record.path, PathBuf::from("b.rs"));
    }

    #[test]
    fn apply_resolutions_unknown_id_errors() {
        let records = vec![add_add_record("a.rs")];
        let conflicts = assign_conflict_ids(&records);
        let mut resolutions = BTreeMap::new();
        resolutions.insert(
            "cf-zzzz".to_string(),
            Resolution::Workspace("alice".to_string()),
        );

        let ws_dirs = BTreeMap::new();
        let result = apply_resolutions(&conflicts, &resolutions, &ws_dirs);
        assert!(result.is_err());
        let err = result.expect_err("operation should fail").to_string();
        assert!(err.contains("Unknown conflict ID"), "error: {err}");
    }

    #[test]
    fn apply_resolutions_workspace_strategy() {
        let records = vec![add_add_record("new.rs")];
        let conflicts = assign_conflict_ids(&records);
        let id = conflicts[0].id.clone();
        let mut resolutions = BTreeMap::new();
        resolutions.insert(id, Resolution::Workspace("bob".to_string()));

        let ws_dirs = BTreeMap::new();
        let (resolved, remaining) = apply_resolutions(&conflicts, &resolutions, &ws_dirs)
            .expect("operation should succeed");

        assert!(remaining.is_empty());
        assert_eq!(resolved[&PathBuf::from("new.rs")], b"bob's version");
    }

    // -----------------------------------------------------------------------
    // conflict_record_to_json_with_id: ID fields in JSON
    // -----------------------------------------------------------------------

    #[test]
    fn conflict_json_includes_id_when_provided() {
        let record = content_record("src/lib.rs", "base", "alice", "bob");
        let json =
            conflict_record_to_json_with_id(&record, Some("cf-test"), &["cf-test.0".to_string()]);

        assert_eq!(json.id.as_deref(), Some("cf-test"));
        assert_eq!(json.atom_ids, vec!["cf-test.0"]);
    }

    #[test]
    fn conflict_json_omits_id_when_none() {
        let record = content_record("src/lib.rs", "base", "alice", "bob");
        let json = conflict_record_to_json(&record);

        assert!(json.id.is_none());
        assert!(json.atom_ids.is_empty());

        // Verify serialization omits the id field
        let json_str = serde_json::to_string(&json).expect("operation should succeed");
        assert!(
            !json_str.contains("\"id\""),
            "id should be omitted when None"
        );
    }

    /// bn-35mr (Prime-Invariant adjacent): when an FF-absorbed path no
    /// longer exists at the target epoch, `ff_apply_one_path` must remove
    /// it from the worktree even when it is currently a *dangling*
    /// symlink. The pre-fix `Path::exists()` guard follows symlinks and
    /// reports `false` for a broken link, leaving a stale symlink that the
    /// next merge snapshot re-injects as an untracked add — undoing the
    /// epoch deletion. `git reset --keep` / `git checkout` (the commands
    /// the gix migration replaced) removed it; `symlink_metadata().is_ok()`
    /// restores that behaviour.
    #[cfg(unix)]
    #[test]
    fn ff_apply_one_path_removes_dangling_symlink_for_deleted_path() {
        // Seeded commit contains README.md but NOT `gone.txt`, so
        // read_blob_at_path(target, "gone.txt") => Ok(None) (deleted at
        // target), exercising the Ok(None) deletion branch.
        let (_dir, root, oid) = maw_git::test_support::init_test_repo_with_commit();
        let repo = maw_git::GixRepo::open(&root).expect("open repo");
        let target_git: maw_git::GitOid = oid.parse().expect("parse oid");

        let link = root.join("gone.txt");
        std::os::unix::fs::symlink("does-not-exist-target", &link)
            .expect("create dangling symlink");
        // Precondition: the link is dangling — exists() (the old guard)
        // sees nothing, but symlink_metadata() (the fix) sees the link.
        assert!(!link.exists(), "symlink target must be absent (dangling)");
        assert!(
            link.symlink_metadata().is_ok(),
            "the symlink itself must exist"
        );

        ff_apply_one_path(
            &repo,
            "test-ws",
            &root,
            target_git,
            std::path::Path::new("gone.txt"),
        );

        assert!(
            link.symlink_metadata().is_err(),
            "dangling symlink at an epoch-deleted path must be removed",
        );
    }
}
