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
//!
//! The function returns both successful resolutions and conflicts so callers can
//! either proceed directly to BUILD or surface rich conflict diagnostics.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::model::types::WorkspaceId;

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
    pub fn is_clean(&self) -> bool {
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
            )));
        };
        variants.push(content.clone());
    }

    // Hash equality short-circuit.
    if all_equal(&variants) {
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
            path, entries, None, reason,
        )));
    };

    // K-way deterministic merge by folding pairwise diff3 against the same base.
    let mut merged = variants[0].clone();
    for next in &variants[1..] {
        if merged == *next {
            continue;
        }

        match diff3_merge_bytes(base_bytes, &merged, next)? {
            Some(out) => merged = out,
            None => {
                return Ok(SharedOutcome::Conflict(conflict_record(
                    path,
                    entries,
                    Some(base_bytes),
                    ConflictReason::Diff3Conflict,
                )));
            }
        }
    }

    Ok(SharedOutcome::Resolved(ResolvedChange::Upsert {
        path: path.to_path_buf(),
        content: merged,
    }))
}

fn all_equal(contents: &[Vec<u8>]) -> bool {
    contents
        .split_first()
        .is_none_or(|(first, rest)| rest.iter().all(|c| c == first))
}

fn conflict_record(
    path: &Path,
    entries: &[PathEntry],
    base: Option<&[u8]>,
    reason: ConflictReason,
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
    }
}

/// Run `git merge-file -p --diff3` for one 3-way merge.
///
/// Returns:
/// - `Ok(Some(bytes))` for clean merge
/// - `Ok(None)` for merge conflicts (exit code 1)
/// - `Err` for command/runtime failures
fn diff3_merge_bytes(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
) -> Result<Option<Vec<u8>>, ResolveError> {
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
        Some(0) => Ok(Some(output.stdout)),
        Some(1) => Ok(None),
        code => Err(ResolveError::GitCommand {
            command: "git merge-file -p --diff3 <ours> <base> <theirs>".to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            exit_code: code,
        }),
    }
}

#[cfg(test)]
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
}
