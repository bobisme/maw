//! RESOLVE step of the N-way merge pipeline.
//!
//! Resolves paths touched by multiple workspaces (the `shared` output from
//! [`crate::merge::partition`]) into:
//!
//! - a single [`ResolvedChange`] when we can resolve automatically, or
//! - a structured [`ConflictRecord`] when human intervention is needed.
//!
//! Resolution strategy (Phase 1):
//!
//! 1. **Hash equality**: if all non-deletion variants are byte-identical,
//!    short-circuit.
//! 2. **Special cases**:
//!    - delete/delete => resolved delete
//!    - modify/delete => conflict
//!    - add/add (different content, no base) => conflict
//! 3. **diff3**: for non-identical content with a known base, run deterministic
//!    line merge via `git merge-file -p --diff3`.
//!    - K=2: one diff3 merge
//!    - K>2: deterministic sequential merges against the same base, in sorted
//!      workspace order.
//! 4. **Shifted-code alignment retry**: if diff3 conflicts, detect moved blocks,
//!    normalize variant block positions back toward base ordering, and retry
//!    diff3 once before declaring conflict.
//!
//! The function returns both successful resolutions and conflicts so callers can
//! either proceed directly to BUILD or surface rich conflict diagnostics.

#![allow(clippy::missing_errors_doc)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::model::conflict::{
    AtomEdit, ConflictAtom, ConflictReason as ModelConflictReason, Region,
};
use crate::model::types::WorkspaceId;

#[cfg(feature = "ast-merge")]
use super::ast_merge::{try_ast_merge_with_config, AstMergeConfig, AstMergeResult};

use super::build::ResolvedChange;
use super::partition::{PartitionResult, PathEntry};
use super::types::ChangeKind;

/// Why a shared path could not be auto-resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictReason {
    /// Two or more workspaces added the same path with different content and
    /// there is no base content to merge against.
    AddAddDifferent,
    /// Some workspaces deleted a file while others modified/added it.
    ModifyDelete,
    /// diff3 detected overlapping edits.
    Diff3Conflict,
    /// Non-add shared edits were present but base content was unavailable.
    MissingBase,
    /// A non-deletion entry was missing file content.
    MissingContent,
}

impl std::fmt::Display for ConflictReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AddAddDifferent => write!(f, "add/add with different content"),
            Self::ModifyDelete => write!(f, "modify/delete conflict"),
            Self::Diff3Conflict => write!(f, "overlapping edits (diff3 conflict)"),
            Self::MissingBase => write!(f, "base content missing"),
            Self::MissingContent => write!(f, "entry missing file content"),
        }
    }
}

/// One side of a conflict record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictSide {
    /// Workspace that produced this side.
    pub workspace_id: WorkspaceId,
    /// Change kind from that workspace.
    pub kind: ChangeKind,
    /// New content (`None` for deletions).
    pub content: Option<Vec<u8>>,
}

/// Structured conflict information for one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictRecord {
    /// Path relative to repo root.
    pub path: PathBuf,
    /// Base content from epoch (if file existed).
    pub base: Option<Vec<u8>>,
    /// All workspace sides for this path, sorted by workspace ID.
    pub sides: Vec<ConflictSide>,
    /// Conflict classification.
    pub reason: ConflictReason,
    /// Localized conflict atoms extracted from diff3 conflict markers.
    ///
    /// Non-empty for `Diff3Conflict` records where region-level localization
    /// was successfully extracted. Empty for other conflict reasons (add/add,
    /// modify/delete) or when the diff3 output could not be parsed.
    pub atoms: Vec<ConflictAtom>,
}

/// Output of the RESOLVE phase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolveResult {
    /// Changes that were resolved automatically and can be fed to BUILD.
    pub resolved: Vec<ResolvedChange>,
    /// Paths that still need manual resolution.
    pub conflicts: Vec<ConflictRecord>,
}

impl ResolveResult {
    /// Returns `true` if there are no conflicts.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }
}

/// Errors from invoking external diff3 tooling.
#[derive(Debug)]
pub enum ResolveError {
    /// I/O while writing temp files or spawning commands.
    Io(std::io::Error),
    /// `git merge-file` failed unexpectedly.
    GitCommand {
        /// Command line summary.
        command: String,
        /// Trimmed stderr.
        stderr: String,
        /// Exit code if available.
        exit_code: Option<i32>,
    },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::GitCommand {
                command,
                stderr,
                exit_code,
            } => {
                write!(f, "`{command}` failed")?;
                if let Some(code) = exit_code {
                    write!(f, " (exit {code})")?;
                }
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ResolveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        if let Self::Io(e) = self {
            Some(e)
        } else {
            None
        }
    }
}

impl From<std::io::Error> for ResolveError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

/// Resolve all paths in a partition result.
///
/// `base_contents` maps file paths to epoch-base content for files that existed
/// at the merge base. Missing entries mean the path did not exist in the base.
///
/// Determinism:
/// - shared entries are expected to be sorted by path + workspace ID by
///   `partition_by_path`
/// - output `resolved` and `conflicts` are sorted by path
pub fn resolve_partition(
    partition: &PartitionResult,
    base_contents: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<ResolveResult, ResolveError> {
    let mut resolved: Vec<ResolvedChange> = Vec::new();
    let mut conflicts: Vec<ConflictRecord> = Vec::new();

    // Unique paths: direct passthrough to BUILD changes.
    for (path, entry) in &partition.unique {
        if entry.is_deletion() {
            resolved.push(ResolvedChange::Delete { path: path.clone() });
            continue;
        }

        match &entry.content {
            Some(content) => resolved.push(ResolvedChange::Upsert {
                path: path.clone(),
                content: content.clone(),
            }),
            None => conflicts.push(ConflictRecord {
                path: path.clone(),
                base: base_contents.get(path).cloned(),
                sides: vec![ConflictSide {
                    workspace_id: entry.workspace_id.clone(),
                    kind: entry.kind.clone(),
                    content: None,
                }],
                reason: ConflictReason::MissingContent,
                atoms: vec![],
            }),
        }
    }

    // Shared paths: apply hash-equality / diff3 strategy.
    for (path, entries) in &partition.shared {
        let base = base_contents.get(path).cloned();
        match resolve_shared_path(path, entries, base.as_deref())? {
            SharedOutcome::Resolved(change) => resolved.push(change),
            SharedOutcome::Conflict(conflict) => conflicts.push(conflict),
        }
    }

    resolved.sort_by(|a, b| a.path().cmp(b.path()));
    conflicts.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(ResolveResult {
        resolved,
        conflicts,
    })
}

/// Resolve all paths in a partition result with AST-aware merge support.
///
/// Like [`resolve_partition`] but with an additional AST merge configuration.
/// When AST merge is enabled for a language and diff3 fails, the AST merge
/// layer is tried before emitting a conflict.
///
/// The merge pipeline order for shared paths is:
/// 1. Hash equality
/// 2. diff3 line merge
/// 3. AST-aware merge (if enabled for the file's language)
/// 4. Emit structured conflict
#[cfg(feature = "ast-merge")]
pub fn resolve_partition_with_ast(
    partition: &PartitionResult,
    base_contents: &BTreeMap<PathBuf, Vec<u8>>,
    ast_config: &AstMergeConfig,
) -> Result<ResolveResult, ResolveError> {
    let mut resolved: Vec<ResolvedChange> = Vec::new();
    let mut conflicts: Vec<ConflictRecord> = Vec::new();

    // Unique paths: same as resolve_partition.
    for (path, entry) in &partition.unique {
        if entry.is_deletion() {
            resolved.push(ResolvedChange::Delete { path: path.clone() });
            continue;
        }

        match &entry.content {
            Some(content) => resolved.push(ResolvedChange::Upsert {
                path: path.clone(),
                content: content.clone(),
            }),
            None => conflicts.push(ConflictRecord {
                path: path.clone(),
                base: base_contents.get(path).cloned(),
                sides: vec![ConflictSide {
                    workspace_id: entry.workspace_id.clone(),
                    kind: entry.kind.clone(),
                    content: None,
                }],
                reason: ConflictReason::MissingContent,
                atoms: vec![],
            }),
        }
    }

    // Shared paths: apply hash-equality / diff3 / AST merge strategy.
    for (path, entries) in &partition.shared {
        let base = base_contents.get(path).cloned();
        match resolve_shared_path_with_ast(path, entries, base.as_deref(), ast_config)? {
            SharedOutcome::Resolved(change) => resolved.push(change),
            SharedOutcome::Conflict(conflict) => conflicts.push(conflict),
        }
    }

    resolved.sort_by(|a, b| a.path().cmp(b.path()));
    conflicts.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(ResolveResult {
        resolved,
        conflicts,
    })
}

enum SharedOutcome {
    Resolved(ResolvedChange),
    Conflict(ConflictRecord),
}

fn resolve_shared_path(
    path: &Path,
    entries: &[PathEntry],
    base: Option<&[u8]>,
) -> Result<SharedOutcome, ResolveError> {
    // delete/delete[/...] => resolved delete
    if entries.iter().all(PathEntry::is_deletion) {
        return Ok(SharedOutcome::Resolved(ResolvedChange::Delete {
            path: path.to_path_buf(),
        }));
    }

    // Any deletion mixed with non-deletion => modify/delete conflict.
    let has_delete = entries.iter().any(PathEntry::is_deletion);
    let has_non_delete = entries.iter().any(|e| !e.is_deletion());
    if has_delete && has_non_delete {
        return Ok(SharedOutcome::Conflict(conflict_record(
            path,
            entries,
            base,
            ConflictReason::ModifyDelete,
            vec![],
        )));
    }

    // Remaining cases are all non-deletions; gather bytes.
    let mut variants: Vec<Vec<u8>> = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(content) = &entry.content else {
            return Ok(SharedOutcome::Conflict(conflict_record(
                path,
                entries,
                base,
                ConflictReason::MissingContent,
                vec![],
            )));
        };
        variants.push(content.clone());
    }

    // Hash equality short-circuit.
    //
    // Prefer blob OID comparison when all entries carry one: O(1) per entry
    // instead of O(file size). Fall back to byte equality when OIDs are
    // absent (Phase 1 paths, tests, legacy workspaces).
    if all_blobs_equal(entries) || all_equal(&variants) {
        return Ok(SharedOutcome::Resolved(ResolvedChange::Upsert {
            path: path.to_path_buf(),
            content: variants[0].clone(),
        }));
    }

    // Without base, differing non-delete variants are add/add (or malformed)
    // and cannot be auto-merged in Phase 1.
    let Some(base_bytes) = base else {
        let reason = if entries.iter().all(|e| matches!(e.kind, ChangeKind::Added)) {
            ConflictReason::AddAddDifferent
        } else {
            ConflictReason::MissingBase
        };
        return Ok(SharedOutcome::Conflict(conflict_record(
            path,
            entries,
            None,
            reason,
            vec![],
        )));
    };

    // K-way deterministic merge by folding pairwise diff3 against the same base.
    // Track the workspace names that contributed to the accumulated "ours" side
    // so that ConflictAtom edits can carry meaningful workspace labels.
    let mut merged = variants[0].clone();
    let mut ours_ws_label: String = entries[0].workspace_id.to_string();

    for (i, next) in variants[1..].iter().enumerate() {
        if merged == *next {
            let theirs_ws = &entries[i + 1].workspace_id;
            ours_ws_label = format!("{ours_ws_label}+{theirs_ws}");
            continue;
        }

        let theirs_ws_label = entries[i + 1].workspace_id.to_string();

        match diff3_merge_bytes(base_bytes, &merged, next)? {
            Diff3Outcome::Clean(out) => {
                merged = out;
                ours_ws_label = format!("{ours_ws_label}+{theirs_ws_label}");
            }
            Diff3Outcome::Conflict { marker_output } => {
                if let Some(retried) = retry_with_shifted_alignment(base_bytes, &merged, next)? {
                    merged = retried;
                    ours_ws_label = format!("{ours_ws_label}+{theirs_ws_label}");
                    continue;
                }

                let atoms = parse_diff3_atoms(&marker_output, &ours_ws_label, &theirs_ws_label);
                return Ok(SharedOutcome::Conflict(conflict_record(
                    path,
                    entries,
                    Some(base_bytes),
                    ConflictReason::Diff3Conflict,
                    atoms,
                )));
            }
        }
    }

    Ok(SharedOutcome::Resolved(ResolvedChange::Upsert {
        path: path.to_path_buf(),
        content: merged,
    }))
}

/// Resolve a shared path with AST-aware merge as fallback after diff3.
///
/// Pipeline: hash eq → diff3 → AST merge → conflict.
/// If AST merge is not enabled for this path's language, falls back to diff3 conflict.
#[cfg(feature = "ast-merge")]
#[allow(clippy::too_many_lines)]
fn resolve_shared_path_with_ast(
    path: &Path,
    entries: &[PathEntry],
    base: Option<&[u8]>,
    ast_config: &AstMergeConfig,
) -> Result<SharedOutcome, ResolveError> {
    // delete/delete[/...] => resolved delete
    if entries.iter().all(PathEntry::is_deletion) {
        return Ok(SharedOutcome::Resolved(ResolvedChange::Delete {
            path: path.to_path_buf(),
        }));
    }

    // Any deletion mixed with non-deletion => modify/delete conflict.
    let has_delete = entries.iter().any(PathEntry::is_deletion);
    let has_non_delete = entries.iter().any(|e| !e.is_deletion());
    if has_delete && has_non_delete {
        return Ok(SharedOutcome::Conflict(conflict_record(
            path,
            entries,
            base,
            ConflictReason::ModifyDelete,
            vec![],
        )));
    }

    // Remaining cases are all non-deletions; gather bytes.
    let mut variants: Vec<Vec<u8>> = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(content) = &entry.content else {
            return Ok(SharedOutcome::Conflict(conflict_record(
                path,
                entries,
                base,
                ConflictReason::MissingContent,
                vec![],
            )));
        };
        variants.push(content.clone());
    }

    // Hash equality short-circuit.
    if all_blobs_equal(entries) || all_equal(&variants) {
        return Ok(SharedOutcome::Resolved(ResolvedChange::Upsert {
            path: path.to_path_buf(),
            content: variants[0].clone(),
        }));
    }

    // Without base, differing non-delete variants are add/add.
    let Some(base_bytes) = base else {
        let reason = if entries.iter().all(|e| matches!(e.kind, ChangeKind::Added)) {
            ConflictReason::AddAddDifferent
        } else {
            ConflictReason::MissingBase
        };
        return Ok(SharedOutcome::Conflict(conflict_record(
            path,
            entries,
            None,
            reason,
            vec![],
        )));
    };

    // Try diff3 first.
    let mut merged = variants[0].clone();
    let mut ours_ws_label: String = entries[0].workspace_id.to_string();
    let mut diff3_conflict: Option<(Vec<u8>, String, String)> = None;

    for (i, next) in variants[1..].iter().enumerate() {
        if merged == *next {
            let theirs_ws = &entries[i + 1].workspace_id;
            ours_ws_label = format!("{ours_ws_label}+{theirs_ws}");
            continue;
        }

        let theirs_ws_label = entries[i + 1].workspace_id.to_string();

        match diff3_merge_bytes(base_bytes, &merged, next)? {
            Diff3Outcome::Clean(out) => {
                merged = out;
                ours_ws_label = format!("{ours_ws_label}+{theirs_ws_label}");
            }
            Diff3Outcome::Conflict { marker_output } => {
                diff3_conflict = Some((marker_output, ours_ws_label, theirs_ws_label));
                break;
            }
        }
    }

    // If diff3 succeeded, return the merge.
    if diff3_conflict.is_none() {
        return Ok(SharedOutcome::Resolved(ResolvedChange::Upsert {
            path: path.to_path_buf(),
            content: merged,
        }));
    }

    // diff3 failed. Try AST merge if enabled for this language.
    if let Some(lang) = ast_config.is_enabled_for(path) {
        let ast_variants: Vec<_> = entries
            .iter()
            .zip(variants.iter())
            .map(|(entry, content)| (entry.workspace_id.clone(), content.clone()))
            .collect();

        match try_ast_merge_with_config(base_bytes, &ast_variants, lang, ast_config) {
            AstMergeResult::Clean(ast_merged) => {
                return Ok(SharedOutcome::Resolved(ResolvedChange::Upsert {
                    path: path.to_path_buf(),
                    content: ast_merged,
                }));
            }
            AstMergeResult::Conflict { atoms } => {
                // Use AST conflict atoms instead of diff3 atoms for better diagnostics.
                return Ok(SharedOutcome::Conflict(conflict_record(
                    path,
                    entries,
                    Some(base_bytes),
                    ConflictReason::Diff3Conflict,
                    atoms,
                )));
            }
            AstMergeResult::Unsupported => {
                // Fall through to diff3 conflict.
            }
        }
    }

    // Fall back to diff3 conflict.
    let (marker_output, ours_label, theirs_label) = diff3_conflict.unwrap();
    let atoms = parse_diff3_atoms(&marker_output, &ours_label, &theirs_label);
    Ok(SharedOutcome::Conflict(conflict_record(
        path,
        entries,
        Some(base_bytes),
        ConflictReason::Diff3Conflict,
        atoms,
    )))
}

fn all_equal(contents: &[Vec<u8>]) -> bool {
    contents
        .split_first()
        .is_none_or(|(first, rest)| rest.iter().all(|c| c == first))
}

/// Returns `true` if **all** entries carry a blob OID and all OIDs are equal.
///
/// When this function returns `true`, all entries have identical content by
/// git's content-addressed guarantee — no byte comparison is needed. If any
/// entry is missing a blob OID, returns `false` and the caller falls back to
/// [`all_equal`] (byte comparison).
fn all_blobs_equal(entries: &[PathEntry]) -> bool {
    let mut iter = entries.iter();
    let Some(first) = iter.next() else {
        return true;
    };
    let Some(ref first_blob) = first.blob else {
        return false;
    };
    iter.all(|e| e.blob.as_ref() == Some(first_blob))
}

fn conflict_record(
    path: &Path,
    entries: &[PathEntry],
    base: Option<&[u8]>,
    reason: ConflictReason,
    atoms: Vec<ConflictAtom>,
) -> ConflictRecord {
    ConflictRecord {
        path: path.to_path_buf(),
        base: base.map(std::borrow::ToOwned::to_owned),
        sides: entries
            .iter()
            .map(|entry| ConflictSide {
                workspace_id: entry.workspace_id.clone(),
                kind: entry.kind.clone(),
                content: entry.content.clone(),
            })
            .collect(),
        reason,
        atoms,
    }
}

/// Outcome of a single diff3 merge attempt.
enum Diff3Outcome {
    /// Clean merge — result bytes.
    Clean(Vec<u8>),
    /// Conflicting merge — the raw diff3 marker output (stdout from git merge-file exit 1).
    Conflict { marker_output: Vec<u8> },
}

/// Run `git merge-file -p --diff3` for one 3-way merge.
///
/// Returns:
/// - `Ok(Diff3Outcome::Clean(bytes))` for clean merge (exit 0)
/// - `Ok(Diff3Outcome::Conflict { marker_output })` for conflicts (exit 1)
/// - `Err` for command/runtime failures
fn diff3_merge_bytes(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
) -> Result<Diff3Outcome, ResolveError> {
    // We intentionally use temp files + git merge-file instead of adding a new
    // diff3 crate dependency. BUILD/COMMIT already shell out to git, and this
    // keeps behavior aligned with git's merge semantics.
    let nonce: u64 = rand::random();
    let tmp_dir = std::env::temp_dir().join(format!("maw-resolve-diff3-{nonce}"));
    fs::create_dir_all(&tmp_dir)?;

    let ours_path = tmp_dir.join("ours.tmp");
    let base_path = tmp_dir.join("base.tmp");
    let theirs_path = tmp_dir.join("theirs.tmp");

    fs::write(&ours_path, ours)?;
    fs::write(&base_path, base)?;
    fs::write(&theirs_path, theirs)?;

    let output = Command::new("git")
        .arg("merge-file")
        .arg("-p")
        .arg("--diff3")
        .arg(&ours_path)
        .arg(&base_path)
        .arg(&theirs_path)
        .output()?;

    // Best-effort cleanup.
    let _ = fs::remove_dir_all(&tmp_dir);

    match output.status.code() {
        Some(0) => Ok(Diff3Outcome::Clean(output.stdout)),
        // git merge-file exits with the number of conflict hunks (≥1) when
        // there are conflicts. Any positive exit code means "conflict output in
        // stdout with diff3 markers". Negative codes or None indicate an error.
        Some(n) if n > 0 => Ok(Diff3Outcome::Conflict {
            marker_output: output.stdout,
        }),
        code => Err(ResolveError::GitCommand {
            command: "git merge-file -p --diff3 <ours> <base> <theirs>".to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            exit_code: code,
        }),
    }
}

/// Retry a diff3 merge after normalizing shifted block positions.
///
/// Returns:
/// - `Ok(Some(merged_bytes))` if normalization enabled a clean merge
/// - `Ok(None)` if no normalization opportunity was detected, or retry still conflicts
/// - `Err(..)` if invoking diff3 fails
fn retry_with_shifted_alignment(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
) -> Result<Option<Vec<u8>>, ResolveError> {
    let normalized_ours = normalize_shifted_blocks(base, ours);
    let normalized_theirs = normalize_shifted_blocks(base, theirs);

    // If neither side could be normalized, don't retry.
    if normalized_ours.is_none() && normalized_theirs.is_none() {
        return Ok(None);
    }

    let ours_aligned = normalized_ours.as_deref().unwrap_or(ours);
    let theirs_aligned = normalized_theirs.as_deref().unwrap_or(theirs);

    match diff3_merge_bytes(base, ours_aligned, theirs_aligned)? {
        Diff3Outcome::Clean(out) => Ok(Some(out)),
        Diff3Outcome::Conflict { .. } => Ok(None),
    }
}

/// Detect moved/shifted paragraph-like blocks and reorder them toward base order.
///
/// Heuristic details:
/// - Split into blocks separated by one or more blank lines.
/// - Hash each block and anchor blocks that are unique in both base and variant.
/// - Assign each variant block a sortable rank derived from nearest anchored base blocks.
/// - Reassemble blocks in ranked order.
///
/// This is O(b log b) where b = number of blocks in the file.
#[allow(clippy::option_if_let_else)]
fn normalize_shifted_blocks(base: &[u8], variant: &[u8]) -> Option<Vec<u8>> {
    let base_text = std::str::from_utf8(base).ok()?;
    let variant_text = std::str::from_utf8(variant).ok()?;

    let base_blocks = split_blocks(base_text);
    let variant_blocks = split_blocks(variant_text);

    if base_blocks.len() < 2 || variant_blocks.len() < 2 {
        return None;
    }

    let mut base_positions: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (idx, block) in base_blocks.iter().enumerate() {
        base_positions
            .entry(block_signature(block))
            .or_default()
            .push(idx);
    }

    let mut variant_positions: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (idx, block) in variant_blocks.iter().enumerate() {
        variant_positions
            .entry(block_signature(block))
            .or_default()
            .push(idx);
    }

    let mut anchors: Vec<Option<usize>> = vec![None; variant_blocks.len()];
    for (idx, block) in variant_blocks.iter().enumerate() {
        let signature = block_signature(block);
        let Some(base_pos) = base_positions.get(&signature) else {
            continue;
        };
        let Some(var_pos) = variant_positions.get(&signature) else {
            continue;
        };

        if base_pos.len() == 1 && var_pos.len() == 1 {
            anchors[idx] = Some(base_pos[0]);
        }
    }

    let moved_anchor_count = anchors
        .iter()
        .enumerate()
        .filter(|(idx, anchor)| anchor.is_some_and(|a| a != *idx))
        .count();
    if moved_anchor_count == 0 {
        return None;
    }

    let mut prev_anchor: Vec<Option<usize>> = vec![None; anchors.len()];
    let mut last: Option<usize> = None;
    for (idx, anchor) in anchors.iter().enumerate() {
        if let Some(a) = anchor {
            last = Some(*a);
        }
        prev_anchor[idx] = last;
    }

    let mut next_anchor: Vec<Option<usize>> = vec![None; anchors.len()];
    let mut next: Option<usize> = None;
    for (idx, anchor) in anchors.iter().enumerate().rev() {
        if let Some(a) = anchor {
            next = Some(*a);
        }
        next_anchor[idx] = next;
    }

    let mut ranked: Vec<(i64, usize, &str)> = Vec::with_capacity(variant_blocks.len());
    for (idx, block) in variant_blocks.iter().enumerate() {
        let rank = anchors[idx].map_or_else(
            || {
                prev_anchor[idx].map_or_else(
                    || {
                        next_anchor[idx].map_or_else(
                            || {
                                usize_to_i64(base_blocks.len())
                                    .saturating_mul(4)
                                    .saturating_add(usize_to_i64(idx))
                            },
                            |next| usize_to_i64(next).saturating_mul(4).saturating_add(1),
                        )
                    },
                    |prev| usize_to_i64(prev).saturating_mul(4).saturating_add(3),
                )
            },
            |base_idx| usize_to_i64(base_idx).saturating_mul(4).saturating_add(2),
        );
        ranked.push((rank, idx, block.as_str()));
    }

    ranked.sort_by_key(|(rank, idx, _)| (*rank, *idx));

    let mut normalized = String::new();
    for (_, _, block) in ranked {
        normalized.push_str(block);
    }

    if normalized.as_bytes() == variant {
        None
    } else {
        Some(normalized.into_bytes())
    }
}

fn block_signature(block: &str) -> String {
    block.trim_end().to_owned()
}

fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn usize_to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Split text into paragraph-like blocks separated by blank lines.
fn split_blocks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();

    for segment in text.split_inclusive('\n') {
        let is_blank = segment.trim().is_empty();
        current.push_str(segment);

        if is_blank {
            out.push(std::mem::take(&mut current));
        }
    }

    if !current.is_empty() {
        out.push(current);
    }

    out
}

/// Parse diff3 conflict marker output and extract [`ConflictAtom`]s.
///
/// Each conflict block in the diff3 output (delimited by `<<<<<<<`, `|||||||`,
/// `=======`, `>>>>>>>`) is converted into one [`ConflictAtom`] with:
/// - `base_region`: the line range in the base file covered by this block
/// - `edits`: two [`AtomEdit`]s — one per workspace side
/// - `reason`: [`ModelConflictReason::OverlappingLineEdits`]
///
/// Base line positions are computed by counting context lines and completed
/// base sections as they appear in the output.
///
/// # Marker format (`git merge-file --diff3`)
///
/// ```text
/// <context lines>
/// <<<<<<< ours
/// <ours content>
/// ||||||| base
/// <base content>
/// =======
/// <theirs content>
/// >>>>>>> theirs
/// <context lines>
/// ```
///
/// `ws_ours` and `ws_theirs` are the workspace ID strings used to label each
/// side's [`AtomEdit`].
#[derive(Clone, Copy, PartialEq)]
enum Diff3ParseState {
    Context,
    Ours,
    Base,
    Theirs,
}

#[must_use]
pub fn parse_diff3_atoms(
    marker_output: &[u8],
    ws_ours: &str,
    ws_theirs: &str,
) -> Vec<ConflictAtom> {
    let text = String::from_utf8_lossy(marker_output);
    let lines: Vec<&str> = text.lines().collect();

    let mut state = Diff3ParseState::Context;
    // 1-indexed position in the base file, advancing as we consume context
    // and completed base sections.
    let mut base_line: u32 = 1;

    // Per-block accumulators.
    let mut block_base_start: u32 = 1;
    let mut ours_lines: Vec<&str> = Vec::new();
    let mut base_lines: Vec<&str> = Vec::new();
    let mut theirs_lines: Vec<&str> = Vec::new();

    let mut atoms: Vec<ConflictAtom> = Vec::new();

    for line in &lines {
        if line.starts_with("<<<<<<<") {
            // Start of a new conflict block.
            state = Diff3ParseState::Ours;
            block_base_start = base_line;
            ours_lines.clear();
            base_lines.clear();
            theirs_lines.clear();
        } else if line.starts_with("|||||||") && state == Diff3ParseState::Ours {
            // Transition: ours → base section.
            state = Diff3ParseState::Base;
        } else if *line == "=======" && state == Diff3ParseState::Base {
            // Transition: base → theirs section.
            state = Diff3ParseState::Theirs;
        } else if line.starts_with(">>>>>>>") && state == Diff3ParseState::Theirs {
            // End of conflict block — build the atom.
            let base_len = usize_to_u32(base_lines.len());
            // The base section covers [block_base_start, block_base_start + base_len).
            // If the base section is empty (pure insertion conflict), the region
            // is a zero-length marker at the insertion point.
            let base_region = Region::lines(block_base_start, block_base_start + base_len);

            let description = if base_len == 0 {
                format!("Both sides inserted content at line {block_base_start}")
            } else {
                format!(
                    "Both sides edited lines {}..{}",
                    block_base_start,
                    block_base_start + base_len
                )
            };

            // For the AtomEdit regions we use the base region as an approximation.
            // Exact workspace-version line numbers would require tracking per-file
            // offsets across multiple conflict blocks, which is Phase 2 work.
            let ours_region = Region::lines(
                block_base_start,
                block_base_start + usize_to_u32(ours_lines.len()),
            );
            let theirs_region = Region::lines(
                block_base_start,
                block_base_start + usize_to_u32(theirs_lines.len()),
            );

            let edits = vec![
                AtomEdit::new(ws_ours, ours_region, ours_lines.join("\n")),
                AtomEdit::new(ws_theirs, theirs_region, theirs_lines.join("\n")),
            ];

            atoms.push(ConflictAtom::new(
                base_region,
                edits,
                ModelConflictReason::OverlappingLineEdits { description },
            ));

            // Advance base_line past the base section just consumed.
            base_line += base_len;
            state = Diff3ParseState::Context;
        } else {
            // Accumulate or count lines based on current state.
            match state {
                Diff3ParseState::Context => base_line += 1,
                Diff3ParseState::Ours => ours_lines.push(line),
                Diff3ParseState::Base => base_lines.push(line),
                Diff3ParseState::Theirs => theirs_lines.push(line),
            }
        }
    }

    atoms
}

#[cfg(test)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;
    use crate::merge::partition::PartitionResult;
    use crate::merge::types::ChangeKind;
    use crate::model::types::WorkspaceId;

    fn ws(name: &str) -> WorkspaceId {
        WorkspaceId::new(name).unwrap()
    }

    fn entry(name: &str, kind: ChangeKind, content: Option<&[u8]>) -> PathEntry {
        PathEntry::new(ws(name), kind, content.map(std::borrow::ToOwned::to_owned))
    }

    fn shared_only(path: &str, entries: Vec<PathEntry>) -> PartitionResult {
        PartitionResult {
            unique: vec![],
            shared: vec![(PathBuf::from(path), entries)],
        }
    }

    fn upsert_content(result: &ResolveResult) -> &[u8] {
        match &result.resolved[0] {
            ResolvedChange::Upsert { content, .. } => content,
            ResolvedChange::Delete { .. } => panic!("expected upsert"),
        }
    }

    #[test]
    fn hash_equality_short_circuits_identical_changes() {
        let partition = shared_only(
            "same.txt",
            vec![
                entry("ws-a", ChangeKind::Modified, Some(b"identical\n")),
                entry("ws-b", ChangeKind::Modified, Some(b"identical\n")),
                entry("ws-c", ChangeKind::Modified, Some(b"identical\n")),
            ],
        );

        let mut base = BTreeMap::new();
        base.insert(PathBuf::from("same.txt"), b"old\n".to_vec());

        let result = resolve_partition(&partition, &base).unwrap();
        assert!(result.is_clean());
        assert_eq!(result.resolved.len(), 1);
        assert_eq!(upsert_content(&result), b"identical\n");
    }

    #[test]
    fn diff3_resolves_non_overlapping_edits() {
        let partition = shared_only(
            "doc.txt",
            vec![
                entry("ws-a", ChangeKind::Modified, Some(b"A\nb\nc\n")),
                entry("ws-b", ChangeKind::Modified, Some(b"a\nb\nC\n")),
            ],
        );

        let mut base = BTreeMap::new();
        base.insert(PathBuf::from("doc.txt"), b"a\nb\nc\n".to_vec());

        let result = resolve_partition(&partition, &base).unwrap();
        assert!(result.is_clean());
        assert_eq!(result.resolved.len(), 1);
        assert_eq!(upsert_content(&result), b"A\nb\nC\n");
    }

    #[test]
    fn overlapping_edits_produce_conflict() {
        let partition = shared_only(
            "doc.txt",
            vec![
                entry("ws-a", ChangeKind::Modified, Some(b"a\nB1\nc\n")),
                entry("ws-b", ChangeKind::Modified, Some(b"a\nB2\nc\n")),
            ],
        );

        let mut base = BTreeMap::new();
        base.insert(PathBuf::from("doc.txt"), b"a\nb\nc\n".to_vec());

        let result = resolve_partition(&partition, &base).unwrap();
        assert_eq!(result.resolved.len(), 0);
        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].reason, ConflictReason::Diff3Conflict);
    }

    #[test]
    fn add_add_different_without_base_conflicts() {
        let partition = shared_only(
            "new.txt",
            vec![
                entry("ws-a", ChangeKind::Added, Some(b"hello\n")),
                entry("ws-b", ChangeKind::Added, Some(b"world\n")),
            ],
        );

        let base = BTreeMap::new();

        let result = resolve_partition(&partition, &base).unwrap();
        assert_eq!(result.resolved.len(), 0);
        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].reason, ConflictReason::AddAddDifferent);
    }

    #[test]
    fn modify_delete_conflicts() {
        let partition = shared_only(
            "file.txt",
            vec![
                entry("ws-a", ChangeKind::Modified, Some(b"new\n")),
                entry("ws-b", ChangeKind::Deleted, None),
            ],
        );

        let mut base = BTreeMap::new();
        base.insert(PathBuf::from("file.txt"), b"old\n".to_vec());

        let result = resolve_partition(&partition, &base).unwrap();
        assert_eq!(result.resolved.len(), 0);
        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].reason, ConflictReason::ModifyDelete);
    }

    #[test]
    fn delete_delete_resolves_to_single_delete() {
        let partition = shared_only(
            "gone.txt",
            vec![
                entry("ws-a", ChangeKind::Deleted, None),
                entry("ws-b", ChangeKind::Deleted, None),
            ],
        );

        let mut base = BTreeMap::new();
        base.insert(PathBuf::from("gone.txt"), b"old\n".to_vec());

        let result = resolve_partition(&partition, &base).unwrap();
        assert!(result.is_clean());
        assert_eq!(result.resolved.len(), 1);
        match &result.resolved[0] {
            ResolvedChange::Delete { path } => assert_eq!(path, &PathBuf::from("gone.txt")),
            ResolvedChange::Upsert { .. } => panic!("expected delete"),
        }
    }

    #[test]
    fn k3_merge_resolves_deterministically() {
        // Each change is separated by 4+ unchanged context lines so git
        // merge-file treats them as independent hunks.
        let base_text = b"1\n-\n-\n-\n-\n2\n-\n-\n-\n-\n3\n";
        let partition = shared_only(
            "k3.txt",
            vec![
                entry(
                    "ws-a",
                    ChangeKind::Modified,
                    Some(b"A1\n-\n-\n-\n-\n2\n-\n-\n-\n-\n3\n"),
                ),
                entry(
                    "ws-b",
                    ChangeKind::Modified,
                    Some(b"1\n-\n-\n-\n-\nB2\n-\n-\n-\n-\n3\n"),
                ),
                entry(
                    "ws-c",
                    ChangeKind::Modified,
                    Some(b"1\n-\n-\n-\n-\n2\n-\n-\n-\n-\nC3\n"),
                ),
            ],
        );

        let mut base = BTreeMap::new();
        base.insert(PathBuf::from("k3.txt"), base_text.to_vec());

        let result = resolve_partition(&partition, &base).unwrap();
        assert!(result.is_clean());
        assert_eq!(
            upsert_content(&result),
            b"A1\n-\n-\n-\n-\nB2\n-\n-\n-\n-\nC3\n"
        );
    }

    #[test]
    fn k5_merge_resolves_deterministically() {
        // Each change is separated by 4+ unchanged context lines so git
        // merge-file treats them as independent hunks.
        let base_text = b"1\n-\n-\n-\n-\n2\n-\n-\n-\n-\n3\n-\n-\n-\n-\n4\n-\n-\n-\n-\n5\n";
        let partition = shared_only(
            "k5.txt",
            vec![
                entry(
                    "ws-0",
                    ChangeKind::Modified,
                    Some(b"A\n-\n-\n-\n-\n2\n-\n-\n-\n-\n3\n-\n-\n-\n-\n4\n-\n-\n-\n-\n5\n"),
                ),
                entry(
                    "ws-1",
                    ChangeKind::Modified,
                    Some(b"1\n-\n-\n-\n-\nB\n-\n-\n-\n-\n3\n-\n-\n-\n-\n4\n-\n-\n-\n-\n5\n"),
                ),
                entry(
                    "ws-2",
                    ChangeKind::Modified,
                    Some(b"1\n-\n-\n-\n-\n2\n-\n-\n-\n-\nC\n-\n-\n-\n-\n4\n-\n-\n-\n-\n5\n"),
                ),
                entry(
                    "ws-3",
                    ChangeKind::Modified,
                    Some(b"1\n-\n-\n-\n-\n2\n-\n-\n-\n-\n3\n-\n-\n-\n-\nD\n-\n-\n-\n-\n5\n"),
                ),
                entry(
                    "ws-4",
                    ChangeKind::Modified,
                    Some(b"1\n-\n-\n-\n-\n2\n-\n-\n-\n-\n3\n-\n-\n-\n-\n4\n-\n-\n-\n-\nE\n"),
                ),
            ],
        );

        let mut base = BTreeMap::new();
        base.insert(PathBuf::from("k5.txt"), base_text.to_vec());

        let result = resolve_partition(&partition, &base).unwrap();
        assert!(result.is_clean());
        assert_eq!(
            upsert_content(&result),
            b"A\n-\n-\n-\n-\nB\n-\n-\n-\n-\nC\n-\n-\n-\n-\nD\n-\n-\n-\n-\nE\n"
        );
    }

    #[test]
    fn shifted_function_move_resolves_after_alignment_retry() {
        let base_text = b"fn one() {\n}\n\nfn two() {\n}\n\nfn three() {\n}\n";
        // ws-a moved `three` to the top.
        let moved = b"fn three() {\n}\n\nfn one() {\n}\n\nfn two() {\n}\n";
        // ws-b edited `two` in place.
        let edited = b"fn one() {\n}\n\nfn two() {\n    println!(\"2\");\n}\n\nfn three() {\n}\n";

        // Bare diff3 conflicts on this shifted-code fixture.
        match diff3_merge_bytes(base_text, moved, edited).unwrap() {
            Diff3Outcome::Conflict { .. } => {}
            Diff3Outcome::Clean(_) => {
                panic!("fixture should conflict before shifted alignment retry")
            }
        }

        let partition = shared_only(
            "src/lib.rs",
            vec![
                entry("ws-a", ChangeKind::Modified, Some(moved)),
                entry("ws-b", ChangeKind::Modified, Some(edited)),
            ],
        );
        let mut base = BTreeMap::new();
        base.insert(PathBuf::from("src/lib.rs"), base_text.to_vec());

        let result = resolve_partition(&partition, &base).unwrap();
        assert!(
            result.is_clean(),
            "alignment retry should auto-resolve moved block"
        );

        let merged = String::from_utf8(upsert_content(&result).to_vec()).unwrap();
        assert!(merged.contains("println!(\"2\")"));
        assert!(merged.contains("fn three()"));
    }

    #[test]
    fn shifted_block_normalization_handles_inserted_block_context() {
        let base = b"fn one() {\n    println!(\"1\");\n}\n\nfn two() {\n    println!(\"2\");\n}\n";
        let variant = b"fn two() {\n    println!(\"2\");\n}\n\nfn helper() {\n    println!(\"h\");\n}\n\nfn one() {\n    println!(\"1\");\n}\n";

        let normalized = normalize_shifted_blocks(base, variant).expect("expected normalization");
        let normalized_text = String::from_utf8(normalized).unwrap();

        // Anchored functions should be restored to base-relative order.
        let one_pos = normalized_text.find("fn one()").unwrap();
        let two_pos = normalized_text.find("fn two()").unwrap();
        assert!(
            one_pos < two_pos,
            "fn one should appear before fn two after normalization"
        );
        // Inserted helper block should be preserved.
        assert!(normalized_text.contains("fn helper()"));
    }

    #[test]
    fn alignment_retry_improves_resolution_over_bare_diff3_fixture_set() {
        let base = b"fn one() {\n}\n\nfn two() {\n}\n\nfn three() {\n}\n";

        let fixtures: Vec<(&[u8], &[u8])> = vec![
            // Move + in-place edit: bare diff3 conflict, alignment retry should resolve.
            (
                b"fn three() {\n}\n\nfn one() {\n}\n\nfn two() {\n}\n",
                b"fn one() {\n}\n\nfn two() {\n    println!(\"2\");\n}\n\nfn three() {\n}\n",
            ),
            // Non-overlapping edits are already clean under bare diff3.
            (
                b"fn one() {\n    println!(\"A\");\n}\n\nfn two() {\n}\n\nfn three() {\n}\n",
                b"fn one() {\n}\n\nfn two() {\n    println!(\"B\");\n}\n\nfn three() {\n}\n",
            ),
            // Identical edits are clean in both paths.
            (
                b"fn one() {\n}\n\nfn two() {\n    println!(\"same\");\n}\n\nfn three() {\n}\n",
                b"fn one() {\n}\n\nfn two() {\n    println!(\"same\");\n}\n\nfn three() {\n}\n",
            ),
        ];

        let mut bare_clean = 0usize;
        let mut aligned_clean = 0usize;

        for (ours, theirs) in fixtures {
            if matches!(
                diff3_merge_bytes(base, ours, theirs).unwrap(),
                Diff3Outcome::Clean(_)
            ) {
                bare_clean += 1;
            }
            if retry_with_shifted_alignment(base, ours, theirs)
                .unwrap()
                .is_some()
                || matches!(
                    diff3_merge_bytes(base, ours, theirs).unwrap(),
                    Diff3Outcome::Clean(_)
                )
            {
                aligned_clean += 1;
            }
        }

        assert!(
            aligned_clean > bare_clean,
            "shifted alignment retry should improve clean-merge count over bare diff3"
        );
    }

    #[test]
    fn unique_and_shared_results_are_path_sorted() {
        let partition = PartitionResult {
            unique: vec![(
                PathBuf::from("z.txt"),
                entry("ws-z", ChangeKind::Added, Some(b"z\n")),
            )],
            shared: vec![(
                PathBuf::from("a.txt"),
                vec![
                    entry("ws-a", ChangeKind::Modified, Some(b"A\n")),
                    entry("ws-b", ChangeKind::Modified, Some(b"A\n")),
                ],
            )],
        };

        let mut base = BTreeMap::new();
        base.insert(PathBuf::from("a.txt"), b"old\n".to_vec());

        let result = resolve_partition(&partition, &base).unwrap();
        let paths: Vec<_> = result
            .resolved
            .iter()
            .map(|change| change.path().clone())
            .collect();

        assert_eq!(paths, vec![PathBuf::from("a.txt"), PathBuf::from("z.txt")]);
    }

    // -----------------------------------------------------------------------
    // ConflictAtom extraction tests (bd-15yn.3 acceptance criteria)
    // -----------------------------------------------------------------------

    /// Overlapping edits on the same line produce a `ConflictRecord` with ≥1 atoms.
    #[test]
    fn overlapping_edits_produce_conflict_with_atoms() {
        // Base: "a\nb\nc\n" (3 lines). ws-a changes line 2 to B1, ws-b to B2.
        let partition = shared_only(
            "doc.txt",
            vec![
                entry("ws-a", ChangeKind::Modified, Some(b"a\nB1\nc\n")),
                entry("ws-b", ChangeKind::Modified, Some(b"a\nB2\nc\n")),
            ],
        );

        let mut base = BTreeMap::new();
        base.insert(PathBuf::from("doc.txt"), b"a\nb\nc\n".to_vec());

        let result = resolve_partition(&partition, &base).unwrap();
        assert_eq!(result.conflicts.len(), 1);
        let record = &result.conflicts[0];
        assert_eq!(record.reason, ConflictReason::Diff3Conflict);

        // Must have exactly one ConflictAtom for the single conflicting hunk.
        assert_eq!(
            record.atoms.len(),
            1,
            "expected 1 atom, got {:?}",
            record.atoms
        );

        let atom = &record.atoms[0];
        // The base line that conflicted is line 2 ("b").
        // Region::lines uses exclusive end, so lines(2, 3) = line 2 only.
        assert_eq!(
            atom.base_region,
            crate::model::conflict::Region::lines(2, 3),
            "atom base_region should cover the conflicted base line"
        );

        // Atom must have two edits (one per workspace).
        assert_eq!(atom.edits.len(), 2);

        // Edits carry the correct workspace labels.
        let ws_labels: Vec<&str> = atom.edits.iter().map(|e| e.workspace.as_str()).collect();
        assert!(
            ws_labels.contains(&"ws-a"),
            "expected ws-a in edits: {ws_labels:?}"
        );
        assert!(
            ws_labels.contains(&"ws-b"),
            "expected ws-b in edits: {ws_labels:?}"
        );

        // Edits carry the correct content.
        let content_a = atom.edits.iter().find(|e| e.workspace == "ws-a").unwrap();
        let content_b = atom.edits.iter().find(|e| e.workspace == "ws-b").unwrap();
        assert_eq!(content_a.content, "B1");
        assert_eq!(content_b.content, "B2");
    }

    /// `ConflictAtoms` match the actual conflicting line region in the base file.
    #[test]
    fn diff3_atoms_have_correct_line_ranges() {
        // Base: 5 lines. Lines 3-4 are the conflict zone.
        // ws-a and ws-b both edit lines 3-4 differently.
        let base = b"line1\nline2\nold3\nold4\nline5\n";
        let ours = b"line1\nline2\nnew3a\nnew4a\nline5\n";
        let theirs = b"line1\nline2\nnew3b\nnew4b\nline5\n";

        let partition = shared_only(
            "src.txt",
            vec![
                entry("ws-a", ChangeKind::Modified, Some(ours)),
                entry("ws-b", ChangeKind::Modified, Some(theirs)),
            ],
        );

        let mut base_map = BTreeMap::new();
        base_map.insert(PathBuf::from("src.txt"), base.to_vec());

        let result = resolve_partition(&partition, &base_map).unwrap();
        assert_eq!(result.conflicts.len(), 1);
        let record = &result.conflicts[0];
        assert_eq!(record.atoms.len(), 1);

        let atom = &record.atoms[0];
        // The conflicting region in base spans lines 3 and 4 (1-indexed).
        // Region::lines uses exclusive end: lines(3, 5) = lines 3..5 = lines 3 and 4.
        assert_eq!(
            atom.base_region,
            crate::model::conflict::Region::lines(3, 5),
            "atom should cover base lines 3-4; got {:?}",
            atom.base_region
        );

        // The reason must be OverlappingLineEdits.
        assert_eq!(
            atom.reason.variant_name(),
            "overlapping_line_edits",
            "reason should be overlapping_line_edits"
        );
    }

    /// Multiple conflict blocks in the same file produce one atom per block.
    #[test]
    fn multiple_conflicts_in_same_file_produce_multiple_atoms() {
        // Base: 5 lines. Lines 2 and 4 are each independently conflicted.
        // Need enough context (≥3 lines) between hunks for git merge-file to
        // treat them as separate hunks.
        let base = b"ctx\na\nctx\nctx\nctx\nb\nctx\n";
        // ws-a edits lines 2 and 6 (1-indexed); ws-b edits same lines differently.
        let ours = b"ctx\nA1\nctx\nctx\nctx\nB1\nctx\n";
        let theirs = b"ctx\nA2\nctx\nctx\nctx\nB2\nctx\n";

        let partition = shared_only(
            "multi.txt",
            vec![
                entry("ws-a", ChangeKind::Modified, Some(ours)),
                entry("ws-b", ChangeKind::Modified, Some(theirs)),
            ],
        );

        let mut base_map = BTreeMap::new();
        base_map.insert(PathBuf::from("multi.txt"), base.to_vec());

        let result = resolve_partition(&partition, &base_map).unwrap();
        assert_eq!(result.conflicts.len(), 1);
        let record = &result.conflicts[0];

        // Two separate conflict hunks → two atoms.
        assert_eq!(
            record.atoms.len(),
            2,
            "expected 2 atoms (one per conflict hunk), got {:?}",
            record.atoms
        );

        // Atoms should be for distinct base line ranges.
        let regions: Vec<_> = record.atoms.iter().map(|a| &a.base_region).collect();
        assert_ne!(
            regions[0], regions[1],
            "atoms should cover different base regions"
        );
    }

    /// Non-diff3 conflicts (add/add, modify/delete) have empty atoms.
    #[test]
    fn non_diff3_conflicts_have_empty_atoms() {
        // add/add
        let partition = shared_only(
            "new.txt",
            vec![
                entry("ws-a", ChangeKind::Added, Some(b"hello\n")),
                entry("ws-b", ChangeKind::Added, Some(b"world\n")),
            ],
        );
        let result = resolve_partition(&partition, &BTreeMap::new()).unwrap();
        assert_eq!(
            result.conflicts[0].atoms.len(),
            0,
            "add/add should have no atoms"
        );

        // modify/delete
        let partition2 = shared_only(
            "gone.txt",
            vec![
                entry("ws-a", ChangeKind::Modified, Some(b"new\n")),
                entry("ws-b", ChangeKind::Deleted, None),
            ],
        );
        let mut base = BTreeMap::new();
        base.insert(PathBuf::from("gone.txt"), b"old\n".to_vec());
        let result2 = resolve_partition(&partition2, &base).unwrap();
        assert_eq!(
            result2.conflicts[0].atoms.len(),
            0,
            "modify/delete should have no atoms"
        );
    }

    // -----------------------------------------------------------------------
    // parse_diff3_atoms unit tests
    // -----------------------------------------------------------------------

    /// Parser handles a single conflict block with correct workspace labels.
    #[test]
    fn parse_diff3_atoms_single_block() {
        // Simulated diff3 output for base="b\n", ours="B1\n", theirs="B2\n"
        // with context "a\n" before and "c\n" after.
        let marker_output =
            b"a\n<<<<<<< ours.tmp\nB1\n||||||| base.tmp\nb\n=======\nB2\n>>>>>>> theirs.tmp\nc\n";

        let atoms = parse_diff3_atoms(marker_output, "alice", "bob");
        assert_eq!(atoms.len(), 1, "expected 1 atom");

        let atom = &atoms[0];
        // "a\n" is context (line 1 in base). The base section "b\n" is line 2.
        // Region: lines(2, 3) = just line 2.
        assert_eq!(
            atom.base_region,
            crate::model::conflict::Region::lines(2, 3)
        );
        assert_eq!(atom.edits.len(), 2);

        let alice = atom.edits.iter().find(|e| e.workspace == "alice").unwrap();
        let bob = atom.edits.iter().find(|e| e.workspace == "bob").unwrap();
        assert_eq!(alice.content, "B1");
        assert_eq!(bob.content, "B2");

        assert_eq!(atom.reason.variant_name(), "overlapping_line_edits");
    }

    /// Parser extracts one atom per conflict block when multiple blocks appear.
    #[test]
    fn parse_diff3_atoms_multiple_blocks() {
        // Two conflict blocks separated by 3+ context lines.
        // Base: ctx(1), a(2), ctx(3), ctx(4), ctx(5), b(6), ctx(7)
        let marker_output = concat!(
            "ctx\n",
            "<<<<<<< ours\n",
            "A1\n",
            "||||||| base\n",
            "a\n",
            "=======\n",
            "A2\n",
            ">>>>>>> theirs\n",
            "ctx\n",
            "ctx\n",
            "ctx\n",
            "<<<<<<< ours\n",
            "B1\n",
            "||||||| base\n",
            "b\n",
            "=======\n",
            "B2\n",
            ">>>>>>> theirs\n",
            "ctx\n",
        )
        .as_bytes();

        let atoms = parse_diff3_atoms(marker_output, "ws-a", "ws-b");
        assert_eq!(atoms.len(), 2, "expected 2 atoms");

        // First block: base section = "a" at line 2 (after 1 context line).
        assert_eq!(
            atoms[0].base_region,
            crate::model::conflict::Region::lines(2, 3)
        );
        assert_eq!(
            atoms[0]
                .edits
                .iter()
                .find(|e| e.workspace == "ws-a")
                .unwrap()
                .content,
            "A1"
        );
        assert_eq!(
            atoms[0]
                .edits
                .iter()
                .find(|e| e.workspace == "ws-b")
                .unwrap()
                .content,
            "A2"
        );

        // Second block: base section = "b" at line 6
        // After block 1: base_line = 2 + 1(base-len) = 3
        // ctx(3) → 4, ctx(4) → 5, ctx(5) → 6: block_base_start = 6
        assert_eq!(
            atoms[1].base_region,
            crate::model::conflict::Region::lines(6, 7)
        );
        assert_eq!(
            atoms[1]
                .edits
                .iter()
                .find(|e| e.workspace == "ws-a")
                .unwrap()
                .content,
            "B1"
        );
        assert_eq!(
            atoms[1]
                .edits
                .iter()
                .find(|e| e.workspace == "ws-b")
                .unwrap()
                .content,
            "B2"
        );
    }

    /// Parser returns empty vec for marker output with no conflict blocks.
    #[test]
    fn parse_diff3_atoms_no_conflicts_returns_empty() {
        let clean_output = b"line one\nline two\nline three\n";
        let atoms = parse_diff3_atoms(clean_output, "ws-a", "ws-b");
        assert!(atoms.is_empty(), "clean output should produce no atoms");
    }

    /// K=2 workspace labels appear correctly in atom edits.
    #[test]
    fn diff3_atoms_carry_workspace_labels_k2() {
        let partition = shared_only(
            "doc.txt",
            vec![
                entry("alice", ChangeKind::Modified, Some(b"a\nALICE\nc\n")),
                entry("bob", ChangeKind::Modified, Some(b"a\nBOB\nc\n")),
            ],
        );

        let mut base = BTreeMap::new();
        base.insert(PathBuf::from("doc.txt"), b"a\norig\nc\n".to_vec());

        let result = resolve_partition(&partition, &base).unwrap();
        assert_eq!(result.conflicts.len(), 1);
        let atoms = &result.conflicts[0].atoms;
        assert_eq!(atoms.len(), 1);

        let edit_ws: Vec<&str> = atoms[0]
            .edits
            .iter()
            .map(|e| e.workspace.as_str())
            .collect();
        // "alice" is first in lexicographic order (ours), "bob" is theirs.
        assert!(
            edit_ws.contains(&"alice"),
            "alice should appear as an edit workspace; got {edit_ws:?}"
        );
        assert!(
            edit_ws.contains(&"bob"),
            "bob should appear as an edit workspace; got {edit_ws:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Phase 3: blob OID equality short-circuit
    // -----------------------------------------------------------------------

    /// Helper: build a `PathEntry` with a blob OID for hash-equality tests.
    fn entry_with_blob(
        name: &str,
        kind: ChangeKind,
        content: Option<&[u8]>,
        blob_hex: &str,
    ) -> PathEntry {
        let blob = crate::model::types::GitOid::new(blob_hex).ok();
        PathEntry::with_identity(
            ws(name),
            kind,
            content.map(std::borrow::ToOwned::to_owned),
            None,
            blob,
        )
    }

    /// When all entries share the same blob OID, the resolve should short-circuit
    /// immediately (no byte comparison or diff3 needed).
    #[test]
    fn blob_oid_equality_short_circuits_without_byte_compare() {
        let same_blob = "a".repeat(40);
        let partition = shared_only(
            "file.txt",
            vec![
                entry_with_blob("ws-a", ChangeKind::Modified, Some(b"content\n"), &same_blob),
                entry_with_blob("ws-b", ChangeKind::Modified, Some(b"content\n"), &same_blob),
            ],
        );

        let mut base = BTreeMap::new();
        base.insert(PathBuf::from("file.txt"), b"old\n".to_vec());

        let result = resolve_partition(&partition, &base).unwrap();
        assert!(result.is_clean(), "same blob OID should resolve cleanly");
        assert_eq!(result.resolved.len(), 1);
        assert_eq!(upsert_content(&result), b"content\n");
    }

    /// Blob OID equality works for K=3 (all three have the same blob).
    #[test]
    fn blob_oid_equality_k3_all_same() {
        let same_blob = "b".repeat(40);
        let partition = shared_only(
            "shared.rs",
            vec![
                entry_with_blob(
                    "ws-a",
                    ChangeKind::Modified,
                    Some(b"fn f() {}\n"),
                    &same_blob,
                ),
                entry_with_blob(
                    "ws-b",
                    ChangeKind::Modified,
                    Some(b"fn f() {}\n"),
                    &same_blob,
                ),
                entry_with_blob(
                    "ws-c",
                    ChangeKind::Modified,
                    Some(b"fn f() {}\n"),
                    &same_blob,
                ),
            ],
        );

        let base = BTreeMap::new();
        let result = resolve_partition(&partition, &base).unwrap();
        assert!(result.is_clean());
        assert_eq!(upsert_content(&result), b"fn f() {}\n");
    }

    /// If blob OIDs differ, fall through to byte comparison / diff3.
    #[test]
    fn different_blob_oids_fall_through_to_diff3() {
        let partition = shared_only(
            "diff.txt",
            vec![
                entry_with_blob(
                    "ws-a",
                    ChangeKind::Modified,
                    Some(b"A\nb\nc\n"),
                    &"a".repeat(40),
                ),
                entry_with_blob(
                    "ws-b",
                    ChangeKind::Modified,
                    Some(b"a\nb\nC\n"),
                    &"b".repeat(40),
                ),
            ],
        );

        let mut base = BTreeMap::new();
        base.insert(PathBuf::from("diff.txt"), b"a\nb\nc\n".to_vec());

        let result = resolve_partition(&partition, &base).unwrap();
        // Non-overlapping edits should still auto-resolve via diff3.
        assert!(
            result.is_clean(),
            "non-overlapping edits should auto-resolve"
        );
    }

    /// If one entry is missing a blob OID, fall back to byte comparison.
    #[test]
    fn missing_blob_oid_falls_back_to_byte_equality() {
        // ws-a has a blob OID, ws-b does not — same content.
        let partition = shared_only(
            "mixed.txt",
            vec![
                entry_with_blob(
                    "ws-a",
                    ChangeKind::Modified,
                    Some(b"same content\n"),
                    &"c".repeat(40),
                ),
                // No blob OID — falls back to byte comparison.
                entry("ws-b", ChangeKind::Modified, Some(b"same content\n")),
            ],
        );

        let base = BTreeMap::new();
        let result = resolve_partition(&partition, &base).unwrap();
        // Byte equality still resolves cleanly.
        assert!(
            result.is_clean(),
            "byte equality should resolve cleanly when blob OID missing"
        );
        assert_eq!(upsert_content(&result), b"same content\n");
    }

    /// `all_blobs_equal` returns `true` for a single-entry slice.
    #[test]
    fn all_blobs_equal_single_entry() {
        let entries = vec![entry_with_blob(
            "ws-a",
            ChangeKind::Modified,
            Some(b"x\n"),
            &"d".repeat(40),
        )];
        // Single-entry shared path: should be caught earlier by partition,
        // but verify the helper handles edge case gracefully.
        assert!(all_blobs_equal(&entries));
    }

    /// `all_blobs_equal` returns `false` when any entry has no blob OID.
    #[test]
    fn all_blobs_equal_missing_one_blob_returns_false() {
        let entries = vec![
            entry_with_blob("ws-a", ChangeKind::Modified, Some(b"x\n"), &"e".repeat(40)),
            entry("ws-b", ChangeKind::Modified, Some(b"x\n")), // no blob
        ];
        assert!(!all_blobs_equal(&entries));
    }

    /// `all_blobs_equal` returns `false` when OIDs differ.
    #[test]
    fn all_blobs_equal_different_blobs_returns_false() {
        let entries = vec![
            entry_with_blob("ws-a", ChangeKind::Modified, Some(b"x\n"), &"f".repeat(40)),
            entry_with_blob("ws-b", ChangeKind::Modified, Some(b"y\n"), &"0".repeat(40)),
        ];
        assert!(!all_blobs_equal(&entries));
    }

    // -----------------------------------------------------------------------
    // AST-enhanced resolve pipeline integration tests
    // -----------------------------------------------------------------------

    #[cfg(feature = "ast-merge")]
    mod ast_resolve_tests {
        use super::*;
        use crate::merge::ast_merge::AstMergeConfig;
        use crate::merge::resolve::resolve_partition_with_ast;

        fn shared_rs(path: &str, entries: Vec<PathEntry>) -> PartitionResult {
            PartitionResult {
                unique: vec![],
                shared: vec![(PathBuf::from(path), entries)],
            }
        }

        /// AST merge resolves overlapping-line edits in different functions.
        ///
        /// diff3 reports a conflict because lines overlap (adjacent changes),
        /// but AST merge sees the edits are in different `function_items`.
        #[test]
        fn ast_resolves_different_functions_where_diff3_fails() {
            // Two functions back-to-back with no separating context.
            // diff3 will conflict because the changes are too close together.
            let base = b"fn foo() {\n    old_a();\n}\nfn bar() {\n    old_b();\n}\n";
            let ws_a = b"fn foo() {\n    new_a();\n}\nfn bar() {\n    old_b();\n}\n";
            let ws_b = b"fn foo() {\n    old_a();\n}\nfn bar() {\n    new_b();\n}\n";

            // First verify: plain resolve_partition conflicts.
            let partition = shared_rs(
                "src/lib.rs",
                vec![
                    entry("ws-a", ChangeKind::Modified, Some(ws_a)),
                    entry("ws-b", ChangeKind::Modified, Some(ws_b)),
                ],
            );
            let mut base_map = BTreeMap::new();
            base_map.insert(PathBuf::from("src/lib.rs"), base.to_vec());

            let plain_result = resolve_partition(&partition, &base_map).unwrap();

            // Now try with AST merge enabled.
            let ast_config = AstMergeConfig::all_languages();
            let ast_result =
                resolve_partition_with_ast(&partition, &base_map, &ast_config).unwrap();

            if !plain_result.is_clean() {
                // diff3 conflicted — AST merge should resolve it.
                assert!(
                    ast_result.is_clean(),
                    "AST merge should resolve what diff3 could not: conflicts={:?}",
                    ast_result.conflicts
                );
                let merged = match &ast_result.resolved[0] {
                    ResolvedChange::Upsert { content, .. } => content,
                    _ => panic!("expected upsert"),
                };
                let merged_str = std::str::from_utf8(merged).unwrap();
                assert!(
                    merged_str.contains("new_a"),
                    "merged should contain ws-a's foo change"
                );
                assert!(
                    merged_str.contains("new_b"),
                    "merged should contain ws-b's bar change"
                );
            }
            // If diff3 resolved cleanly (enough context), AST merge should also resolve cleanly.
        }

        /// AST merge produces conflict atoms with `AstNode` regions when same function is modified.
        #[test]
        fn ast_conflict_has_ast_node_regions() {
            let base = b"fn process() {\n    step_1();\n    step_2();\n}\n";
            let ws_a = b"fn process() {\n    step_1_v1();\n    step_2();\n}\n";
            let ws_b = b"fn process() {\n    step_1();\n    step_2_v2();\n}\n";

            let partition = shared_rs(
                "src/processor.rs",
                vec![
                    entry("ws-a", ChangeKind::Modified, Some(ws_a)),
                    entry("ws-b", ChangeKind::Modified, Some(ws_b)),
                ],
            );
            let mut base_map = BTreeMap::new();
            base_map.insert(PathBuf::from("src/processor.rs"), base.to_vec());

            let ast_config = AstMergeConfig::all_languages();
            let result = resolve_partition_with_ast(&partition, &base_map, &ast_config).unwrap();

            // Both ws-a and ws-b modify the same function — should conflict.
            assert_eq!(result.conflicts.len(), 1);
            let record = &result.conflicts[0];
            // Should have AST-level atoms.
            assert!(
                !record.atoms.is_empty(),
                "conflict should have atoms from AST merge"
            );
            let atom = &record.atoms[0];
            assert!(
                matches!(&atom.base_region, Region::AstNode { node_kind, name, .. }
                    if node_kind == "function_item" && name.as_deref() == Some("process")),
                "atom should reference function_item `process`, got: {:?}",
                atom.base_region
            );
        }

        /// AST merge is not used when disabled in config.
        #[test]
        fn ast_merge_disabled_falls_through_to_diff3() {
            let base = b"fn foo() {\n    old_a();\n}\nfn bar() {\n    old_b();\n}\n";
            let ws_a = b"fn foo() {\n    new_a();\n}\nfn bar() {\n    old_b();\n}\n";
            let ws_b = b"fn foo() {\n    old_a();\n}\nfn bar() {\n    new_b();\n}\n";

            let partition = shared_rs(
                "src/lib.rs",
                vec![
                    entry("ws-a", ChangeKind::Modified, Some(ws_a)),
                    entry("ws-b", ChangeKind::Modified, Some(ws_b)),
                ],
            );
            let mut base_map = BTreeMap::new();
            base_map.insert(PathBuf::from("src/lib.rs"), base.to_vec());

            // With no languages enabled, AST merge should not be tried.
            let no_ast_config = AstMergeConfig::default();
            let result = resolve_partition_with_ast(&partition, &base_map, &no_ast_config).unwrap();

            // Should get the same result as plain resolve_partition.
            let plain_result = resolve_partition(&partition, &base_map).unwrap();
            assert_eq!(result.is_clean(), plain_result.is_clean());
        }

        /// AST merge is not used for unsupported file extensions.
        #[test]
        fn ast_merge_skipped_for_unsupported_extension() {
            let base = b"line1\nline2\nline3\n";
            let ws_a = b"LINE1\nline2\nline3\n";
            let ws_b = b"line1\nline2\nLINE3\n";

            let partition = shared_rs(
                "data.json",
                vec![
                    entry("ws-a", ChangeKind::Modified, Some(ws_a)),
                    entry("ws-b", ChangeKind::Modified, Some(ws_b)),
                ],
            );
            let mut base_map = BTreeMap::new();
            base_map.insert(PathBuf::from("data.json"), base.to_vec());

            let ast_config = AstMergeConfig::all_languages();
            let result = resolve_partition_with_ast(&partition, &base_map, &ast_config).unwrap();
            let plain_result = resolve_partition(&partition, &base_map).unwrap();
            assert_eq!(result.is_clean(), plain_result.is_clean());
        }
    }
}
