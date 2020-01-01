use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};

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
use crate::merge::types::{ChangeKind, PatchSet as CollectedPatchSet};
use crate::merge::validate::{ValidateOutcome, run_validate_phase, write_validation_artifact};
use crate::merge_state::{MergePhase, MergeStateFile, run_cleanup_phase};
use crate::model::conflict::ConflictAtom;
use tracing::instrument;
use crate::model::conflict::Region;
use crate::model::patch::{FileId, PatchSet as ModelPatchSet, PatchValue};
use crate::model::types::{EpochId, GitOid, WorkspaceId};
use crate::oplog::read::read_head;
use crate::oplog::types::{OpPayload, Operation};

use super::{
    DEFAULT_WORKSPACE, MawConfig, get_backend,
    oplog_runtime::append_operation_with_runtime_checkpoint, repo_root,
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
    /// Atom-level IDs, e.g. ["cf-k7mx.0", "cf-k7mx.1"].
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

        let resolution = if let Some(path) = strategy.strip_prefix("content:") {
            Resolution::Content(PathBuf::from(path))
        } else {
            Resolution::Workspace(strategy.to_string())
        };

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
            let content = resolve_file_content(
                resolution,
                &conflict.record,
                workspace_dirs,
            )?;
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
                let content = resolve_atoms(
                    &conflict.record,
                    &atom_resolutions,
                )?;
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
        let matches_any = conflicts.iter().any(|c| {
            c.id == *res_id || c.atom_ids.iter().any(|a| a == res_id)
        });
        if !matches_any {
            bail!(
                "Unknown conflict ID in --resolve: '{res_id}'\n  \
                 Valid IDs for this merge: {}",
                conflicts.iter().map(|c| c.id.as_str()).collect::<Vec<_>>().join(", ")
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
                    let available: Vec<_> = record.sides.iter().map(|s| s.workspace_id.to_string()).collect();
                    anyhow::anyhow!(
                        "Workspace '{name}' is not a side in this conflict.\n  \
                         Available: {}",
                        available.join(", ")
                    )
                })?;
            side.content.clone().ok_or_else(|| {
                let others: Vec<_> = record.sides.iter()
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
            #[allow(clippy::cast_possible_truncation)]
            let e = line_starts
                .get((*end - 1) as usize)
                .copied()
                .unwrap_or(content.len());
            (s as u32, e as u32)
        }
        Region::WholeFile => (0, content.len() as u32),
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
    let mut atoms_sorted: Vec<(u32, u32, &crate::model::conflict::ConflictAtom, &Resolution)> =
        Vec::new();
    for (i, atom) in record.atoms.iter().enumerate() {
        let res = atom_resolutions[i].ok_or_else(|| {
            anyhow::anyhow!("Missing resolution for atom {i} of {}", record.path.display())
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
            _ => unreachable!(),
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
/// Uses a temporary index to handle nested directory structures correctly:
/// 1. `git hash-object -w --stdin` → new blob OIDs for resolved content
/// 2. `git read-tree` → populate temp index from candidate tree
/// 3. `git update-index` → replace blobs for resolved paths
/// 4. `git write-tree` → new tree OID from the patched index
/// 5. `git commit-tree` → new commit (same parent as candidate)
fn patch_candidate_tree(
    root: &Path,
    candidate: &GitOid,
    resolved: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<GitOid> {
    if resolved.is_empty() {
        return Ok(candidate.clone());
    }

    // 1. Hash resolved contents as blobs
    let mut new_blobs: BTreeMap<String, String> = BTreeMap::new();
    for (path, content) in resolved {
        let blob_oid = git_hash_object(root, content).ok_or_else(|| {
            anyhow::anyhow!("Failed to hash resolved content for {}", path.display())
        })?;
        new_blobs.insert(path.to_string_lossy().to_string(), blob_oid.as_str().to_string());
    }

    // 2. Create a temporary index from the candidate tree
    let tmp_index = tempfile::NamedTempFile::new()
        .context("Failed to create temp index file")?;
    let tmp_index_path = tmp_index.path().to_string_lossy().to_string();

    let read_tree = Command::new("git")
        .args(["read-tree", candidate.as_str()])
        .env("GIT_INDEX_FILE", &tmp_index_path)
        .current_dir(root)
        .output()
        .context("Failed to run git read-tree")?;
    if !read_tree.status.success() {
        bail!(
            "git read-tree failed: {}",
            String::from_utf8_lossy(&read_tree.stderr).trim()
        );
    }

    // 3. Update the index with resolved blobs
    for (path, blob_oid) in &new_blobs {
        let cacheinfo = format!("100644,{blob_oid},{path}");
        let update = Command::new("git")
            .args(["update-index", "--add", "--cacheinfo", &cacheinfo])
            .env("GIT_INDEX_FILE", &tmp_index_path)
            .current_dir(root)
            .output()
            .context("Failed to run git update-index")?;
        if !update.status.success() {
            bail!(
                "git update-index failed for {}: {}",
                path,
                String::from_utf8_lossy(&update.stderr).trim()
            );
        }
    }

    // 4. Write the patched tree from the index
    let write_tree = Command::new("git")
        .arg("write-tree")
        .env("GIT_INDEX_FILE", &tmp_index_path)
        .current_dir(root)
        .output()
        .context("Failed to run git write-tree")?;
    if !write_tree.status.success() {
        bail!(
            "git write-tree failed: {}",
            String::from_utf8_lossy(&write_tree.stderr).trim()
        );
    }
    let new_tree_oid = String::from_utf8_lossy(&write_tree.stdout).trim().to_string();

    // 5. commit-tree with the same parent as the candidate
    let parent_output = Command::new("git")
        .args(["rev-parse", &format!("{candidate}^")])
        .current_dir(root)
        .output()
        .context("Failed to get candidate parent")?;
    let parent_oid = String::from_utf8_lossy(&parent_output.stdout).trim().to_string();

    let commit_output = Command::new("git")
        .args([
            "commit-tree",
            &new_tree_oid,
            "-p",
            &parent_oid,
            "-m",
            "epoch: merge with conflict resolutions",
        ])
        .current_dir(root)
        .output()
        .context("Failed to run git commit-tree")?;
    if !commit_output.status.success() {
        bail!(
            "git commit-tree failed: {}",
            String::from_utf8_lossy(&commit_output.stderr).trim()
        );
    }
    let new_commit_oid = String::from_utf8_lossy(&commit_output.stdout)
        .trim()
        .to_string();
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

/// Convert with optional conflict ID and atom IDs.
fn conflict_record_to_json_with_id(
    record: &ConflictRecord,
    id: Option<&str>,
    atom_ids: &[String],
) -> ConflictJson {
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

/// Print detailed conflict information with terseid IDs and resolve commands.
fn print_conflict_report(
    conflicts_with_ids: &[ConflictWithId],
    ws_names: &[String],
) {
    print_conflict_report_with_resolve(conflicts_with_ids, ws_names, None);
}

fn print_conflict_report_with_resolve(
    conflicts_with_ids: &[ConflictWithId],
    ws_names: &[String],
    prebuilt_resolve_args: Option<&[String]>,
) {
    println!();
    println!(
        "BUILD: {} conflict(s) detected.",
        conflicts_with_ids.len()
    );
    println!();

    for c in conflicts_with_ids {
        let reason = format!("{}", c.record.reason);
        let ws_list: Vec<String> = c
            .record
            .sides
            .iter()
            .map(|s| s.workspace_id.as_str().to_string())
            .collect();
        println!(
            "  {:<10} {:<40} {}",
            c.id,
            c.record.path.display(),
            reason
        );
        println!(
            "           Workspaces: {}",
            ws_list.join(", ")
        );

        // Show content snippets from each side (up to 5 lines each)
        for side in &c.record.sides {
            if let Some(ref content) = side.content {
                let text = String::from_utf8_lossy(content);
                let lines: Vec<&str> = text.lines().collect();
                let preview_lines = 5;
                let truncated = lines.len() > preview_lines;
                let shown: Vec<&str> = lines.iter().take(preview_lines).copied().collect();
                let label = side.workspace_id.as_str();
                println!("           [{label}]:");
                for line in &shown {
                    println!("             {line}");
                }
                if truncated {
                    println!("             ... ({} more lines)", lines.len() - preview_lines);
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
                println!(
                    "             {:<14} {:<16} {}",
                    atom_id, region_desc, reason_desc
                );
            }
        }
        println!();
    }

    // Build the resolve command template
    let ws_args = ws_names.join(" ");
    let resolve_args_owned: Vec<String>;
    let resolve_args: &[String] = if let Some(prebuilt) = prebuilt_resolve_args {
        prebuilt
    } else {
        let default_ws = ws_names.first().map_or("WORKSPACE", |s| s.as_str());
        resolve_args_owned = conflicts_with_ids
            .iter()
            .map(|c| format!("--resolve {}={default_ws}", c.id))
            .collect();
        &resolve_args_owned
    };
    println!("To resolve, re-run with --resolve:");
    println!(
        "  maw ws merge {} {}",
        ws_args,
        resolve_args.join(" ")
    );
    println!();
    let default_ws = ws_names.first().map_or("WORKSPACE", |s| s.as_str());
    println!("Or resolve all at once:");
    println!("  maw ws merge {} --resolve-all={default_ws}", ws_args);
    println!();
    println!("Options:  ID=WORKSPACE | ID=content:PATH");
    println!();
    println!(
        "To inspect full content:  maw ws conflicts {} --format json",
        ws_args
    );
    println!("Or edit files in a workspace, commit, and re-merge.");
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
    let temp_check_dir = tempfile::Builder::new().prefix("check-tmp-").tempdir_in(&manifold_dir).context("Failed to create temp dir for merge check")?;
    let check_dir = temp_check_dir.path().to_path_buf();

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
    drop(temp_check_dir);

    match prepare_result {
        Ok(_frozen) => {
            // PREPARE succeeded, now try BUILD
            // Re-create dir for build
            let temp_build_dir = tempfile::Builder::new().prefix("build-tmp-").tempdir_in(&manifold_dir).context("Failed to create temp dir for build check")?;
            let build_dir = temp_build_dir.path().to_path_buf();
            // Propagate second prepare error so build doesn't run on uninitialized dir.
            run_prepare_phase(&root, &build_dir, &sources, &workspace_dirs)
                .context("prepare phase failed for build check")?;
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
    let manifold_dir = root.join(".manifold");
    let temp_check_dir = tempfile::Builder::new().prefix("conflicts-tmp-").tempdir_in(&manifold_dir).context("Failed to create temp dir for conflict check")?;
    let check_dir = temp_check_dir.path().to_path_buf();

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
            let temp_build_dir = tempfile::Builder::new().prefix("build-tmp-").tempdir_in(&manifold_dir).context("Failed to create temp dir for build phase")?;
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

    if !has_conflicts {
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
            println!("To merge: maw ws merge {}", workspaces.join(" "));
        }
        return Ok(());
    }

    // Assign terseid IDs to conflicts
    let conflicts_with_ids = assign_conflict_ids(&build_output.conflicts);

    if format == OutputFormat::Json {
        let conflict_jsons: Vec<ConflictJson> = conflicts_with_ids
            .iter()
            .map(|c| conflict_record_to_json_with_id(&c.record, Some(&c.id), &c.atom_ids))
            .collect();
        let ws_args = workspaces.join(" ");
        let default_ws = workspaces.first().map_or("WORKSPACE", |s| s.as_str());
        let resolve_args: Vec<String> = conflicts_with_ids
            .iter()
            .map(|c| format!("--resolve {}={default_ws}", c.id))
            .collect();
        let to_fix = format!("maw ws merge {ws_args} {}", resolve_args.join(" "));
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
        print_conflict_report(&conflicts_with_ids, &workspaces.to_vec());
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
                && let Ok(snapshot) = backend.snapshot(&ws_id)
            {
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
    /// Inline conflict resolutions. Each entry is `ID=STRATEGY`.
    pub resolve: Vec<String>,
    /// Resolve all remaining conflicts to this workspace name.
    /// Individual `--resolve` flags take precedence.
    pub resolve_all: Option<String>,
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
        ref resolve,
        ref resolve_all,
    } = *opts;
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
    let default_ws = maw_config.default_workspace();
    let branch = maw_config.branch();

    // Reject merging the default workspace
    if ws_to_merge.iter().any(|ws| ws == default_ws) {
        bail!(
            "Cannot merge the default workspace \u{2014} it is the merge target, not a source.\n\
             \n  To advance {branch} to include your edits in {default_ws}:\n\
             \n    maw push --advance\n\
             \n  This updates refs/heads/{branch} to the current epoch and pushes."
        );
    }

    if dry_run {
        return preview_merge(&ws_to_merge, &root);
    }

    run_hooks(&maw_config.hooks.pre_merge, "pre-merge", &root, true)?;

    if ws_to_merge.len() == 1 {
        textln!("Adopting workspace: {}", ws_to_merge[0]);
    } else {
        textln!("Merging workspaces: {}", ws_to_merge.join(", "));
    }
    textln!();

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
    textln!("PREPARE: Freezing merge inputs...");
    let frozen = run_prepare_phase(&root, &manifold_dir, &sources, &workspace_dirs)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    textln!("  Epoch: {}", &frozen.epoch.as_str()[..12]);
    for (ws_id, head) in &frozen.heads {
        textln!("  {}: {}", ws_id, &head.as_str()[..12]);
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
                    parsed.entry(c.id.clone())
                        .or_insert_with(|| Resolution::Workspace(ws_name.clone()));
                }
            }

            let (resolved_contents, remaining) = match apply_resolutions(
                &conflicts_with_ids,
                &parsed,
                &workspace_dirs,
            ) {
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
                };
            } else {
                // Some conflicts remain unresolved — report them with IDs
                abort_merge(&manifold_dir, "partially resolved conflicts");
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
                    let to_fix = format!("maw ws merge {ws_args} {}", resolve_args.join(" "));
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
                    print_conflict_report_with_resolve(&remaining, &ws_to_merge, Some(&resolve_args));
                }

                bail!(
                    "{} conflict(s) remain after partial resolution.",
                    remaining.len()
                );
            }
        } else {
            // No --resolve flags: abort with conflict report including IDs
            abort_merge(&manifold_dir, "unresolved conflicts");

            let ws_args = ws_to_merge.join(" ");
            let default_ws = ws_to_merge.first().map_or("WORKSPACE", |s| s.as_str());
            let resolve_args: Vec<String> = conflicts_with_ids
                .iter()
                .map(|c| format!("--resolve {}={default_ws}", c.id))
                .collect();

            if format == OutputFormat::Json {
                let conflict_jsons: Vec<ConflictJson> = conflicts_with_ids
                    .iter()
                    .map(|c| {
                        conflict_record_to_json_with_id(&c.record, Some(&c.id), &c.atom_ids)
                    })
                    .collect();
                let to_fix = format!("maw ws merge {ws_args} {}", resolve_args.join(" "));
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
                textln!(
                    "  {} unresolved conflict(s)",
                    build_output.conflicts.len()
                );
                print_conflict_report(&conflicts_with_ids, &ws_to_merge);

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
                    &frozen.epoch,
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
    textln!("COMMIT: Advancing epoch...");

    // Advance merge-state to Commit phase
    advance_merge_state(&manifold_dir, MergePhase::Commit)?;

    let epoch_before_oid = frozen.epoch.oid().clone();
    let branch_ref = format!("refs/heads/{branch}");

    // Pre-flight: verify the branch hasn't diverged from the epoch since
    // PREPARE. If direct commits were made to the branch outside of maw
    // between PREPARE and COMMIT, the branch CAS will fail after the epoch
    // ref has already moved, leaving epoch and branch permanently diverged.
    // Detect and abort cleanly before touching any refs.
    if let Ok(Some(current_branch)) = crate::refs::read_ref(&root, &branch_ref) {
        if current_branch != epoch_before_oid {
            abort_merge(
                &manifold_dir,
                "branch diverged from epoch since PREPARE (direct commits detected)",
            );
            bail!(
                "Merge COMMIT aborted: branch '{branch}' has diverged from epoch since PREPARE.\n  \
                 Expected: {}\n  \
                 Actual:   {}\n  \
                 Cause: commits were made directly to '{branch}' outside of maw.\n  \
                 Fix: run `maw init` to resync the epoch ref to the current branch HEAD,\n  \
                 then retry the merge.",
                &epoch_before_oid.as_str()[..12],
                &current_branch.as_str()[..12],
            );
        }
    }

    match run_commit_phase(&root, branch, &epoch_before_oid, &build_output.candidate) {
        Ok(CommitResult::Committed) => {
            textln!(
                "  Epoch advanced: {} → {}",
                &epoch_before_oid.as_str()[..12],
                &build_output.candidate.as_str()[..12]
            );
            textln!("  Branch '{branch}' updated.");
        }
        Err(crate::merge::commit::CommitError::PartialCommit) => {
            // Epoch ref moved but branch ref didn't — attempt recovery
            textln!("  WARNING: Partial commit — attempting recovery...");
            match recover_partial_commit(&root, branch, &epoch_before_oid, &build_output.candidate)
            {
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

    // Record epoch_after in merge-state
    record_epoch_after(&manifold_dir, &build_output.candidate)?;

    // Record merge operations in source workspace histories.
    for warning in record_merge_operations(&root, &sources, &frozen.epoch, &build_output.candidate)
    {
        tracing::warn!("{warning}");
    }

    // -----------------------------------------------------------------------
    // Phase 5: CLEANUP — destroy workspaces (if requested), remove merge-state
    // -----------------------------------------------------------------------
    textln!();
    textln!("CLEANUP...");

    // Advance merge-state to Cleanup phase
    advance_merge_state(&manifold_dir, MergePhase::Cleanup)?;

    // Update the default workspace to point to the new epoch
    if default_ws_path.exists() {
        update_default_workspace(&default_ws_path, &build_output.candidate, text_mode)?;
    }

    // Destroy source workspaces if requested
    if destroy_after {
        handle_post_merge_destroy(&ws_to_merge, default_ws, confirm, &backend, text_mode)?;
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
        textln!();
        textln!("Merged to {branch}: {msg} from {}", ws_to_merge.join(", "));
        textln!();
        textln!("Next: push to remote:");
        textln!("  maw push");
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
                let previous_blob =
                    match epoch_blob_oid(root, &patch_set.epoch, &change.path) {
                        Ok(oid) => oid,
                        Err(_) => continue,
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

    let mut child = Command::new("git")
        .args(["hash-object", "-w", "--stdin"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn git hash-object: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&payload).map_err(|e| {
            anyhow::anyhow!("write patch-set payload to git hash-object stdin: {e}")
        })?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| anyhow::anyhow!("wait for git hash-object: {e}"))?;
    if !output.status.success() {
        bail!(
            "git hash-object -w --stdin failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    GitOid::new(&raw).map_err(|e| anyhow::anyhow!("invalid patch-set blob OID '{raw}': {e}"))
}

fn git_hash_object(root: &Path, content: &[u8]) -> Option<GitOid> {
    let mut child = Command::new("git")
        .args(["hash-object", "-w", "--stdin"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(content);
    }

    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }

    let hex = String::from_utf8(output.stdout).ok()?;
    GitOid::new(hex.trim()).ok()
}

fn epoch_blob_oid(root: &Path, epoch: &EpochId, path: &Path) -> Result<GitOid> {
    let rev = format!("{}:{}", epoch.as_str(), path.to_string_lossy());
    let output = Command::new("git")
        .args(["rev-parse", &rev])
        .current_dir(root)
        .output()
        .map_err(|e| anyhow::anyhow!("spawn git rev-parse for '{}': {e}", path.display()))?;

    if !output.status.success() {
        bail!(
            "git rev-parse {} failed: {}",
            rev,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    GitOid::new(&raw)
        .map_err(|e| anyhow::anyhow!("invalid blob OID '{raw}' for '{}': {e}", path.display()))
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
    text_mode: bool,
) -> Result<()> {
    // Reset the worktree to the new epoch
    let output = std::process::Command::new("git")
        .args(["checkout", "--force", new_epoch.as_str()])
        .current_dir(default_ws_path)
        .output()
        .context("Failed to update default workspace")?;

    if output.status.success() {
        if text_mode {
            println!("  Default workspace updated to new epoch.");
        }
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Failed to update default workspace to new epoch: {}\n  \
             The merge COMMIT succeeded (refs are updated), but the default workspace \
             working copy could not be checked out.\n  \
             To fix: git -C {} checkout {}",
            stderr.trim(),
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
    text_mode: bool,
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
    for ws_name in &ws_to_destroy {
        if ws_name == DEFAULT_WORKSPACE {
            if text_mode {
                println!("    Skipping default workspace");
            }
            continue;
        }
        if let Ok(ws_id) = WorkspaceId::new(ws_name) {
            match backend.destroy(&ws_id) {
                Ok(()) => {
                    if text_mode {
                        println!("    Destroyed: {ws_name}");
                    }
                }
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
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
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
            resolve_command: None,
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

        assert_ne!(ids[0].id, ids[1].id, "different paths should get different IDs");
    }

    // -----------------------------------------------------------------------
    // parse_resolutions: valid and invalid inputs
    // -----------------------------------------------------------------------

    #[test]
    fn parse_resolutions_workspace_name() {
        let raw = vec!["cf-abcd=alice".to_string()];
        let parsed = parse_resolutions(&raw).unwrap();

        assert_eq!(parsed.len(), 1);
        assert!(matches!(&parsed["cf-abcd"], Resolution::Workspace(name) if name == "alice"));
    }

    #[test]
    fn parse_resolutions_workspace_name_with_hyphens() {
        let raw = vec!["cf-abcd=my-workspace".to_string()];
        let parsed = parse_resolutions(&raw).unwrap();

        assert!(matches!(&parsed["cf-abcd"], Resolution::Workspace(name) if name == "my-workspace"));
    }

    #[test]
    fn parse_resolutions_content_path() {
        let raw = vec!["cf-abcd=content:/tmp/resolved.rs".to_string()];
        let parsed = parse_resolutions(&raw).unwrap();

        assert!(matches!(&parsed["cf-abcd"], Resolution::Content(p) if p == Path::new("/tmp/resolved.rs")));
    }

    #[test]
    fn parse_resolutions_atom_level() {
        let raw = vec!["cf-abcd.0=alice".to_string(), "cf-abcd.1=bob".to_string()];
        let parsed = parse_resolutions(&raw).unwrap();

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
        let parsed = parse_resolutions(&raw).unwrap();

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
        let (resolved, remaining) = apply_resolutions(&conflicts, &resolutions, &ws_dirs).unwrap();

        assert!(remaining.is_empty());
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved[&PathBuf::from("new.rs")],
            b"alice's version"
        );
    }

    #[test]
    fn apply_resolutions_second_workspace() {
        let records = vec![add_add_record("new.rs")];
        let conflicts = assign_conflict_ids(&records);
        let id = conflicts[0].id.clone();
        let mut resolutions = BTreeMap::new();
        resolutions.insert(id, Resolution::Workspace("bob".to_string()));

        let ws_dirs = BTreeMap::new();
        let (resolved, remaining) = apply_resolutions(&conflicts, &resolutions, &ws_dirs).unwrap();

        assert!(remaining.is_empty());
        assert_eq!(
            resolved[&PathBuf::from("new.rs")],
            b"bob's version"
        );
    }

    #[test]
    fn apply_resolutions_unresolved_remains() {
        let records = vec![
            add_add_record("a.rs"),
            add_add_record("b.rs"),
        ];
        let conflicts = assign_conflict_ids(&records);
        let id_a = conflicts[0].id.clone();
        let mut resolutions = BTreeMap::new();
        resolutions.insert(id_a, Resolution::Workspace("alice".to_string()));

        let ws_dirs = BTreeMap::new();
        let (resolved, remaining) = apply_resolutions(&conflicts, &resolutions, &ws_dirs).unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].record.path, PathBuf::from("b.rs"));
    }

    #[test]
    fn apply_resolutions_unknown_id_errors() {
        let records = vec![add_add_record("a.rs")];
        let conflicts = assign_conflict_ids(&records);
        let mut resolutions = BTreeMap::new();
        resolutions.insert("cf-zzzz".to_string(), Resolution::Workspace("alice".to_string()));

        let ws_dirs = BTreeMap::new();
        let result = apply_resolutions(&conflicts, &resolutions, &ws_dirs);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
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
        let (resolved, remaining) = apply_resolutions(&conflicts, &resolutions, &ws_dirs).unwrap();

        assert!(remaining.is_empty());
        assert_eq!(
            resolved[&PathBuf::from("new.rs")],
            b"bob's version"
        );
    }

    // -----------------------------------------------------------------------
    // conflict_record_to_json_with_id: ID fields in JSON
    // -----------------------------------------------------------------------

    #[test]
    fn conflict_json_includes_id_when_provided() {
        let record = content_record("src/lib.rs", "base", "alice", "bob");
        let json = conflict_record_to_json_with_id(&record, Some("cf-test"), &["cf-test.0".to_string()]);

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
        let json_str = serde_json::to_string(&json).unwrap();
        assert!(!json_str.contains("\"id\""), "id should be omitted when None");
    }
}
