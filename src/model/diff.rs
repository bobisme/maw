//! PatchSet computation from working directory diff (§5.4).
//!
//! Builds a [`PatchSet`] by comparing a workspace's working directory against
//! a base epoch commit using `git diff` and `git ls-files`.
//!
//! # Overview
//!
//! [`compute_patchset`] does three things:
//!
//! 1. Runs `git diff --find-renames --name-status <epoch>` in `workspace_path`
//!    to enumerate tracked changes (added, modified, deleted, renamed files).
//! 2. Runs `git ls-files --others --exclude-standard` to collect untracked
//!    files, recording each as an additional [`PatchValue::Add`] entry.
//! 3. For each change, looks up or computes the relevant blob OID(s) using
//!    `git hash-object -w` and `git rev-parse <epoch>:<path>`.
//!
//! # FileId allocation
//!
//! Until the persistent FileId registry (`bd-b2y4`) exists:
//! - **New files** (Add) receive a randomly generated FileId.
//! - **Pre-existing files** (Modify, Delete, Rename) receive a deterministic
//!   FileId derived from the first 16 bytes of the epoch blob OID. This is a
//!   placeholder — the registry will provide true stability.
//!
//! # Example flow
//!
//! ```text
//! compute_patchset(repo_root, workspace_path, &epoch)
//!   ├── git diff --find-renames --name-status <epoch>  → A/M/D/R lines
//!   ├── git ls-files --others --exclude-standard        → untracked paths
//!   ├── git hash-object -w <file>                       → new blob OIDs
//!   └── git rev-parse <epoch>:<path>                    → base blob OIDs
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use rand::Rng;

use crate::model::patch::{FileId, PatchSet, PatchValue};
use crate::model::types::{EpochId, GitOid};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur when computing a [`PatchSet`] from a working dir diff.
#[derive(Debug)]
pub enum DiffError {
    /// A git command failed.
    GitCommand {
        /// The full command string (for diagnostics).
        command: String,
        /// Stderr from git.
        stderr: String,
        /// Process exit code, if available.
        exit_code: Option<i32>,
    },
    /// A git OID returned by a command was malformed.
    InvalidOid {
        /// The raw string git printed.
        raw: String,
    },
    /// An I/O error (e.g. spawning git).
    Io(std::io::Error),
    /// A line in `git diff --name-status` output was malformed.
    MalformedDiffLine(String),
}

impl std::fmt::Display for DiffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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
            Self::InvalidOid { raw } => write!(f, "invalid git OID: {raw:?}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::MalformedDiffLine(line) => write!(f, "malformed diff line: {line:?}"),
        }
    }
}

impl std::error::Error for DiffError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        if let Self::Io(e) = self {
            Some(e)
        } else {
            None
        }
    }
}

impl From<std::io::Error> for DiffError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Internal: parsed diff entry
// ---------------------------------------------------------------------------

/// A single entry parsed from `git diff --find-renames --name-status <epoch>`.
#[derive(Debug, PartialEq, Eq)]
enum DiffEntry {
    /// File was added (did not exist in epoch).
    Added(PathBuf),
    /// File content was changed in place.
    Modified(PathBuf),
    /// File was deleted (no longer present in working dir).
    Deleted(PathBuf),
    /// File was renamed (and optionally also modified).
    Renamed { from: PathBuf, to: PathBuf },
}

// ---------------------------------------------------------------------------
// Internal: git helpers
// ---------------------------------------------------------------------------

/// Run a git command in `dir` and return trimmed stdout, or a [`DiffError`].
fn git_cmd(dir: &Path, args: &[&str]) -> Result<String, DiffError> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
    } else {
        Err(DiffError::GitCommand {
            command: format!("git {}", args.join(" ")),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            exit_code: out.status.code(),
        })
    }
}

/// Parse `git diff --find-renames --name-status <epoch>` output into [`DiffEntry`]s.
///
/// Each non-empty line has the form:
/// - `A\t<path>` — added
/// - `M\t<path>` — modified
/// - `D\t<path>` — deleted
/// - `R<score>\t<from>\t<to>` — renamed (score is a similarity percentage)
fn parse_diff_name_status(output: &str) -> Result<Vec<DiffEntry>, DiffError> {
    let mut entries = Vec::new();
    for line in output.lines() {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        let status = parts.first().copied().unwrap_or("");
        match status {
            "A" if parts.len() == 2 => {
                entries.push(DiffEntry::Added(PathBuf::from(parts[1])));
            }
            "M" if parts.len() == 2 => {
                entries.push(DiffEntry::Modified(PathBuf::from(parts[1])));
            }
            "D" if parts.len() == 2 => {
                entries.push(DiffEntry::Deleted(PathBuf::from(parts[1])));
            }
            s if s.starts_with('R') && parts.len() == 3 => {
                entries.push(DiffEntry::Renamed {
                    from: PathBuf::from(parts[1]),
                    to: PathBuf::from(parts[2]),
                });
            }
            _ => {
                return Err(DiffError::MalformedDiffLine(line.to_owned()));
            }
        }
    }
    Ok(entries)
}

/// Hash a file and write it to the git object store, returning its blob OID.
///
/// Equivalent to `git hash-object -w -- <abs_path>`.
fn hash_object_write(workspace_path: &Path, abs_file: &Path) -> Result<GitOid, DiffError> {
    let path_str = abs_file.to_string_lossy();
    let stdout = git_cmd(workspace_path, &["hash-object", "-w", "--", &path_str])?;
    let trimmed = stdout.trim();
    GitOid::new(trimmed).map_err(|_| DiffError::InvalidOid {
        raw: trimmed.to_owned(),
    })
}

/// Look up the blob OID of `path` in the epoch commit's tree.
///
/// Equivalent to `git rev-parse <epoch>:<path>`.
fn epoch_blob_oid(workspace_path: &Path, epoch: &EpochId, path: &Path) -> Result<GitOid, DiffError> {
    let rev = format!("{}:{}", epoch.as_str(), path.to_string_lossy());
    let stdout = git_cmd(workspace_path, &["rev-parse", &rev])?;
    let trimmed = stdout.trim();
    GitOid::new(trimmed).map_err(|_| DiffError::InvalidOid {
        raw: trimmed.to_owned(),
    })
}

/// Allocate a new random [`FileId`] for a freshly created file.
///
/// NOTE: Until the persistent FileId registry (`bd-b2y4`) exists, FileIds for
/// new files are generated randomly and are not stable across invocations.
/// Future work will replace this with a registry-backed allocation.
fn new_file_id() -> FileId {
    let mut rng = rand::rng();
    FileId::new(rng.random::<u128>())
}

/// Derive a deterministic [`FileId`] from an existing blob OID.
///
/// Used for pre-existing files (Modify, Delete, Rename) to give a stable
/// (within one invocation) FileId derived from the blob. The FileId registry
/// (`bd-b2y4`) will provide true cross-invocation stability.
fn file_id_from_blob(blob: &GitOid) -> FileId {
    // Parse the first 32 hex characters of the OID as a u128.
    let hex = &blob.as_str()[..32];
    // This cannot fail: GitOid is validated to be 40 lowercase hex chars.
    let n = u128::from_str_radix(hex, 16).unwrap_or(0);
    FileId::new(n)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Compute a [`PatchSet`] from a workspace's current working directory state
/// relative to the given base epoch commit.
///
/// # Arguments
///
/// - `workspace_path` — absolute path to the workspace working directory.
/// - `base_epoch` — the epoch commit to diff against (an ancestor of the
///   workspace's current state).
///
/// # What this does
///
/// 1. Runs `git diff --find-renames --name-status <epoch>` to detect tracked
///    changes: added, modified, deleted, and renamed files.
/// 2. Runs `git ls-files --others --exclude-standard` to collect untracked
///    files (new files not yet staged), recording them as `Add` entries.
/// 3. For each change, computes the relevant blob OIDs:
///    - Working-directory file content → `git hash-object -w`
///    - Epoch tree blob → `git rev-parse <epoch>:<path>`
///
/// # FileId note
///
/// FileIds are placeholders until `bd-b2y4` (stable FileId system) is
/// implemented. New files get random FileIds; pre-existing files get a
/// deterministic FileId derived from their base blob OID.
///
/// # Errors
///
/// Returns [`DiffError`] if any git command fails or produces unexpected output.
pub fn compute_patchset(
    workspace_path: &Path,
    base_epoch: &EpochId,
) -> Result<PatchSet, DiffError> {
    let mut patches: BTreeMap<PathBuf, PatchValue> = BTreeMap::new();

    // -----------------------------------------------------------------------
    // Step 1: tracked changes from git diff
    // -----------------------------------------------------------------------
    let diff_out = git_cmd(
        workspace_path,
        &["diff", "--find-renames", "--name-status", base_epoch.as_str()],
    )?;

    let entries = parse_diff_name_status(&diff_out)?;

    for entry in entries {
        match entry {
            DiffEntry::Added(path) => {
                let abs = workspace_path.join(&path);
                let blob = hash_object_write(workspace_path, &abs)?;
                let file_id = new_file_id();
                patches.insert(path, PatchValue::Add { blob, file_id });
            }
            DiffEntry::Modified(path) => {
                let base_blob = epoch_blob_oid(workspace_path, base_epoch, &path)?;
                let abs = workspace_path.join(&path);
                let new_blob = hash_object_write(workspace_path, &abs)?;
                let file_id = file_id_from_blob(&base_blob);
                patches.insert(
                    path,
                    PatchValue::Modify {
                        base_blob,
                        new_blob,
                        file_id,
                    },
                );
            }
            DiffEntry::Deleted(path) => {
                let previous_blob = epoch_blob_oid(workspace_path, base_epoch, &path)?;
                let file_id = file_id_from_blob(&previous_blob);
                patches.insert(path, PatchValue::Delete { previous_blob, file_id });
            }
            DiffEntry::Renamed { from, to } => {
                let base_blob = epoch_blob_oid(workspace_path, base_epoch, &from)?;
                let abs_to = workspace_path.join(&to);
                let new_blob_oid = hash_object_write(workspace_path, &abs_to)?;
                let file_id = file_id_from_blob(&base_blob);
                // Record new_blob only if content changed.
                let new_blob = if new_blob_oid != base_blob {
                    Some(new_blob_oid)
                } else {
                    None
                };
                patches.insert(to, PatchValue::Rename { from, file_id, new_blob });
            }
        }
    }

    // -----------------------------------------------------------------------
    // Step 2: untracked files → Add
    // -----------------------------------------------------------------------
    let untracked_out = git_cmd(
        workspace_path,
        &["ls-files", "--others", "--exclude-standard"],
    )?;

    for line in untracked_out.lines() {
        if line.is_empty() {
            continue;
        }
        let path = PathBuf::from(line);
        // Skip if already handled via the diff (e.g. a staged Add).
        if patches.contains_key(&path) {
            continue;
        }
        let abs = workspace_path.join(&path);
        let blob = hash_object_write(workspace_path, &abs)?;
        let file_id = new_file_id();
        patches.insert(path, PatchValue::Add { blob, file_id });
    }

    Ok(PatchSet {
        base_epoch: base_epoch.clone(),
        patches,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Initialize a bare-minimum git repo in `dir` for testing.
    ///
    /// Configures `user.email` and `user.name` so commits succeed without a
    /// global git config (common in CI environments).
    fn git_init(dir: &Path) {
        run_git(dir, &["init", "-b", "main"]);
        run_git(dir, &["config", "user.email", "test@test.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
    }

    /// Run a git command in `dir`, panicking on failure (test helper only).
    fn run_git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git must be installed");
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            panic!(
                "git {} failed: {}",
                args.join(" "),
                stderr
            );
        }
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Write `content` to `dir/path`, creating parent directories as needed.
    fn write_file(dir: &Path, path: &str, content: &str) {
        let full = dir.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(full, content).unwrap();
    }

    /// Create an initial epoch commit in `dir` and return its OID.
    fn make_epoch(dir: &Path, files: &[(&str, &str)]) -> EpochId {
        for (path, content) in files {
            write_file(dir, path, content);
        }
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-m", "epoch"]);
        let oid = run_git(dir, &["rev-parse", "HEAD"]);
        EpochId::new(&oid).expect("HEAD OID must be valid")
    }

    // -----------------------------------------------------------------------
    // parse_diff_name_status unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_added_line() {
        let input = "A\tsrc/new.rs";
        let entries = parse_diff_name_status(input).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], DiffEntry::Added(PathBuf::from("src/new.rs")));
    }

    #[test]
    fn parse_modified_line() {
        let input = "M\tsrc/lib.rs";
        let entries = parse_diff_name_status(input).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], DiffEntry::Modified(PathBuf::from("src/lib.rs")));
    }

    #[test]
    fn parse_deleted_line() {
        let input = "D\told.rs";
        let entries = parse_diff_name_status(input).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], DiffEntry::Deleted(PathBuf::from("old.rs")));
    }

    #[test]
    fn parse_renamed_line() {
        let input = "R90\told/path.rs\tnew/path.rs";
        let entries = parse_diff_name_status(input).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0],
            DiffEntry::Renamed {
                from: PathBuf::from("old/path.rs"),
                to: PathBuf::from("new/path.rs"),
            }
        );
    }

    #[test]
    fn parse_renamed_r100() {
        // R100 = identical content, just moved
        let input = "R100\tfoo.rs\tbar.rs";
        let entries = parse_diff_name_status(input).unwrap();
        assert_eq!(
            entries[0],
            DiffEntry::Renamed {
                from: PathBuf::from("foo.rs"),
                to: PathBuf::from("bar.rs"),
            }
        );
    }

    #[test]
    fn parse_empty_output() {
        let entries = parse_diff_name_status("").unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_multiple_entries() {
        let input = "A\tnew.rs\nM\told.rs\nD\tgone.rs";
        let entries = parse_diff_name_status(input).unwrap();
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn parse_malformed_line_returns_error() {
        let input = "Z\tunknown_status";
        let result = parse_diff_name_status(input);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // file_id_from_blob
    // -----------------------------------------------------------------------

    #[test]
    fn file_id_from_blob_is_deterministic() {
        let oid = GitOid::new(&"a".repeat(40)).unwrap();
        let id1 = file_id_from_blob(&oid);
        let id2 = file_id_from_blob(&oid);
        assert_eq!(id1, id2);
    }

    #[test]
    fn file_id_from_blob_differs_for_different_blobs() {
        let oid1 = GitOid::new(&"a".repeat(40)).unwrap();
        let oid2 = GitOid::new(&"b".repeat(40)).unwrap();
        assert_ne!(file_id_from_blob(&oid1), file_id_from_blob(&oid2));
    }

    // -----------------------------------------------------------------------
    // Integration tests: compute_patchset with a real git repo
    // -----------------------------------------------------------------------

    #[test]
    fn compute_patchset_empty_working_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        git_init(root);
        write_file(root, "existing.rs", "fn main() {}");
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", "epoch"]);
        let oid = run_git(root, &["rev-parse", "HEAD"]);
        let epoch = EpochId::new(&oid).unwrap();

        // No changes since epoch.
        let ps = compute_patchset(root, &epoch).unwrap();
        assert!(ps.is_empty(), "no changes → empty PatchSet");
        assert_eq!(ps.base_epoch, epoch);
    }

    #[test]
    fn compute_patchset_added_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        git_init(root);
        let epoch = make_epoch(root, &[("base.rs", "// base")]);

        // Stage a new file.
        write_file(root, "new.rs", "fn new() {}");
        run_git(root, &["add", "new.rs"]);

        let ps = compute_patchset(root, &epoch).unwrap();
        assert_eq!(ps.len(), 1);

        let pv = ps.patches.get(&PathBuf::from("new.rs")).expect("new.rs in PatchSet");
        assert!(
            matches!(pv, PatchValue::Add { .. }),
            "expected Add, got {pv:?}"
        );
        if let PatchValue::Add { blob, .. } = pv {
            // Verify OID is valid (40 hex chars).
            assert_eq!(blob.as_str().len(), 40);
        }
    }

    #[test]
    fn compute_patchset_untracked_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        git_init(root);
        let epoch = make_epoch(root, &[("base.rs", "// base")]);

        // Do NOT stage — should be detected via ls-files --others.
        write_file(root, "untracked.txt", "hello");

        let ps = compute_patchset(root, &epoch).unwrap();
        assert_eq!(ps.len(), 1);

        let pv = ps
            .patches
            .get(&PathBuf::from("untracked.txt"))
            .expect("untracked.txt in PatchSet");
        assert!(matches!(pv, PatchValue::Add { .. }));
    }

    #[test]
    fn compute_patchset_modified_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        git_init(root);
        let epoch = make_epoch(root, &[("lib.rs", "fn original() {}")]);

        // Modify and stage.
        write_file(root, "lib.rs", "fn modified() {}");
        run_git(root, &["add", "lib.rs"]);

        let ps = compute_patchset(root, &epoch).unwrap();
        assert_eq!(ps.len(), 1);

        let pv = ps.patches.get(&PathBuf::from("lib.rs")).expect("lib.rs in PatchSet");
        assert!(
            matches!(pv, PatchValue::Modify { .. }),
            "expected Modify, got {pv:?}"
        );
        if let PatchValue::Modify { base_blob, new_blob, .. } = pv {
            // base_blob is the epoch's blob, new_blob is the current content.
            assert_ne!(base_blob, new_blob, "blobs must differ after modification");
            assert_eq!(base_blob.as_str().len(), 40);
            assert_eq!(new_blob.as_str().len(), 40);
        }
    }

    #[test]
    fn compute_patchset_deleted_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        git_init(root);
        let epoch = make_epoch(root, &[("to_delete.rs", "fn gone() {}")]);

        // Delete and stage.
        run_git(root, &["rm", "to_delete.rs"]);

        let ps = compute_patchset(root, &epoch).unwrap();
        assert_eq!(ps.len(), 1);

        let pv = ps
            .patches
            .get(&PathBuf::from("to_delete.rs"))
            .expect("to_delete.rs in PatchSet");
        assert!(
            matches!(pv, PatchValue::Delete { .. }),
            "expected Delete, got {pv:?}"
        );
        if let PatchValue::Delete { previous_blob, .. } = pv {
            assert_eq!(previous_blob.as_str().len(), 40);
        }
    }

    #[test]
    fn compute_patchset_renamed_file_same_content() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        git_init(root);
        // Use content long enough for git to detect the rename.
        let content = "fn example() { println!(\"hello world\"); }\n".repeat(5);
        let epoch = make_epoch(root, &[("old_name.rs", &content)]);

        // Rename without modifying content.
        run_git(root, &["mv", "old_name.rs", "new_name.rs"]);

        let ps = compute_patchset(root, &epoch).unwrap();
        assert_eq!(ps.len(), 1, "rename → one entry at destination path");

        let pv = ps
            .patches
            .get(&PathBuf::from("new_name.rs"))
            .expect("new_name.rs in PatchSet");
        assert!(
            matches!(pv, PatchValue::Rename { .. }),
            "expected Rename, got {pv:?}"
        );
        if let PatchValue::Rename { from, new_blob, .. } = pv {
            assert_eq!(from, &PathBuf::from("old_name.rs"));
            assert!(new_blob.is_none(), "content unchanged → new_blob should be None");
        }
    }

    #[test]
    fn compute_patchset_renamed_file_with_content_change() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        git_init(root);
        // Use content long enough for git to detect the rename.
        let content = "fn example() { println!(\"original content\"); }\n".repeat(5);
        let epoch = make_epoch(root, &[("old.rs", &content)]);

        // Rename and modify content.
        run_git(root, &["mv", "old.rs", "new.rs"]);
        write_file(root, "new.rs", &format!("{content}// modified\n"));
        run_git(root, &["add", "new.rs"]);

        let ps = compute_patchset(root, &epoch).unwrap();
        assert_eq!(ps.len(), 1);

        let pv = ps.patches.get(&PathBuf::from("new.rs")).expect("new.rs in PatchSet");
        assert!(matches!(pv, PatchValue::Rename { .. }), "expected Rename, got {pv:?}");
        if let PatchValue::Rename { from, new_blob, .. } = pv {
            assert_eq!(from, &PathBuf::from("old.rs"));
            assert!(new_blob.is_some(), "content changed → new_blob should be Some");
        }
    }

    #[test]
    fn compute_patchset_multiple_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        git_init(root);
        let epoch = make_epoch(
            root,
            &[
                ("keep.rs", "fn keep() {}"),
                ("modify.rs", "fn modify() {}"),
                ("delete.rs", "fn delete() {}"),
            ],
        );

        // Apply multiple changes.
        write_file(root, "add.rs", "fn add() {}");       // new untracked
        write_file(root, "modify.rs", "fn modified() {}"); // changed
        run_git(root, &["rm", "delete.rs"]);              // deleted
        run_git(root, &["add", "."]);

        let ps = compute_patchset(root, &epoch).unwrap();

        // keep.rs → no entry
        assert!(!ps.patches.contains_key(&PathBuf::from("keep.rs")));

        // add.rs → Add
        assert!(matches!(
            ps.patches.get(&PathBuf::from("add.rs")),
            Some(PatchValue::Add { .. })
        ));

        // modify.rs → Modify
        assert!(matches!(
            ps.patches.get(&PathBuf::from("modify.rs")),
            Some(PatchValue::Modify { .. })
        ));

        // delete.rs → Delete
        assert!(matches!(
            ps.patches.get(&PathBuf::from("delete.rs")),
            Some(PatchValue::Delete { .. })
        ));

        assert_eq!(ps.len(), 3);
    }

    #[test]
    fn compute_patchset_blob_oids_are_correct() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        git_init(root);
        let epoch_id = make_epoch(root, &[("file.rs", "original")]);

        // Get epoch blob OID directly.
        let expected_base_blob = run_git(root, &["rev-parse", &format!("{}:file.rs", epoch_id.as_str())]);

        write_file(root, "file.rs", "modified");
        run_git(root, &["add", "file.rs"]);

        // Get expected new blob OID.
        let expected_new_blob = run_git(root, &["ls-files", "--cached", "-s", "file.rs"]);
        // ls-files -s output: "<mode> <blob> <stage>\t<path>"
        let expected_new_oid: String = expected_new_blob
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .to_owned();

        let ps = compute_patchset(root, &epoch_id).unwrap();
        if let Some(PatchValue::Modify { base_blob, new_blob, .. }) =
            ps.patches.get(&PathBuf::from("file.rs"))
        {
            assert_eq!(base_blob.as_str(), expected_base_blob, "base_blob must match epoch blob");
            assert_eq!(new_blob.as_str(), expected_new_oid, "new_blob must match staged blob");
        } else {
            panic!("expected Modify for file.rs");
        }
    }

    #[test]
    fn compute_patchset_base_epoch_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        git_init(root);
        let epoch = make_epoch(root, &[("a.rs", "content")]);

        write_file(root, "b.rs", "new");
        run_git(root, &["add", "b.rs"]);

        let ps = compute_patchset(root, &epoch).unwrap();
        assert_eq!(ps.base_epoch, epoch, "base_epoch must match the epoch passed in");
    }

    #[test]
    fn compute_patchset_uses_btreemap_ordering() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        git_init(root);
        let epoch = make_epoch(root, &[("placeholder.rs", "x")]);

        // Add files that would sort differently by insertion order vs alpha.
        write_file(root, "z.rs", "z");
        write_file(root, "a.rs", "a");
        write_file(root, "m.rs", "m");
        run_git(root, &["add", "."]);

        let ps = compute_patchset(root, &epoch).unwrap();

        let keys: Vec<_> = ps.patches.keys().collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "PatchSet paths must be in sorted order");
    }
}
