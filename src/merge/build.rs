//! BUILD step of the N-way merge pipeline.
//!
//! Takes the epoch commit and a list of resolved file changes, then produces
//! a new git tree object and commit. This is Step 4 in the
//! collect → partition → resolve → **build** pipeline.
//!
//! # Algorithm
//!
//! 1. Read the epoch's full flat file tree via `git ls-tree -r`.
//! 2. Apply resolved changes:
//!    - Upsert (add/modify): write a new blob via `git hash-object -w --stdin`,
//!      update the flat tree map.
//!    - Delete: remove the path from the flat tree map.
//! 3. Reconstruct the git tree hierarchy bottom-up, one directory at a time,
//!    using `git mktree`.
//! 4. Create the merge commit via `git commit-tree -p <epoch> -m <message>`.
//!
//! # Determinism
//!
//! Paths are processed in lexicographic order throughout. The same epoch +
//! resolved changes always produce the same tree OID (blob content and tree
//! structure are identical), which git's content-addressable storage makes
//! unique.

#![allow(clippy::missing_errors_doc)]

use std::collections::{BTreeMap, HashMap};
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::model::types::{EpochId, GitOid, WorkspaceId};

// ---------------------------------------------------------------------------
// ResolvedChange
// ---------------------------------------------------------------------------

/// A resolved file change produced by the merge engine's resolve step.
///
/// After the partition and resolution phase, each touched path results in
/// exactly one `ResolvedChange`. The build step applies these changes to the
/// epoch tree to produce the merged tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedChange {
    /// File was added or modified; `content` is the new file bytes.
    ///
    /// Used for both `Added` and `Modified` changes. The previous content
    /// (if any) is discarded; `content` becomes the new blob.
    Upsert {
        /// Path relative to the repo root.
        path: PathBuf,
        /// New file content (bytes).
        content: Vec<u8>,
    },
    /// File was deleted; the path is removed from the merged tree.
    Delete {
        /// Path relative to the repo root.
        path: PathBuf,
    },
}

impl ResolvedChange {
    /// Return the path this change applies to.
    #[must_use]
    pub const fn path(&self) -> &PathBuf {
        match self {
            Self::Upsert { path, .. } | Self::Delete { path } => path,
        }
    }

    /// Return `true` if this is an upsert (add or modify).
    #[must_use]
    pub const fn is_upsert(&self) -> bool {
        matches!(self, Self::Upsert { .. })
    }

    /// Return `true` if this is a deletion.
    #[must_use]
    pub const fn is_delete(&self) -> bool {
        matches!(self, Self::Delete { .. })
    }
}

// ---------------------------------------------------------------------------
// BuildError
// ---------------------------------------------------------------------------

/// Errors that can occur during the BUILD step.
#[derive(Debug)]
pub enum BuildError {
    /// A git command failed (non-zero exit).
    GitCommand {
        /// The command that was run.
        command: String,
        /// Stderr output (trimmed).
        stderr: String,
        /// Process exit code if available.
        exit_code: Option<i32>,
    },
    /// An I/O error occurred.
    Io(std::io::Error),
    /// A line from `git ls-tree` could not be parsed.
    MalformedLsTree {
        /// The raw line that could not be parsed.
        line: String,
    },
    /// A git OID returned by a command was invalid.
    InvalidOid {
        /// Human-readable context (e.g., "hash-object output").
        context: String,
        /// The raw value returned.
        raw: String,
    },
}

impl std::fmt::Display for BuildError {
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
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::MalformedLsTree { line } => {
                write!(f, "malformed `git ls-tree` output: {line:?}")
            }
            Self::InvalidOid { context, raw } => {
                write!(f, "invalid OID from {context}: {raw:?}")
            }
        }
    }
}

impl std::error::Error for BuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        if let Self::Io(e) = self {
            Some(e)
        } else {
            None
        }
    }
}

impl From<std::io::Error> for BuildError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Build a merge commit from an epoch and resolved changes.
///
/// This is the primary entry point for the BUILD step. It:
/// 1. Reads the epoch's tree to get the baseline file set.
/// 2. Applies `resolved` changes (writing new blobs as needed).
/// 3. Reconstructs the git tree hierarchy using `git mktree`.
/// 4. Creates and returns the OID of a new commit with `epoch` as its parent.
///
/// # Arguments
///
/// * `root` — Repository root (where `.git` lives).
/// * `epoch` — The epoch commit that all workspaces were based on.
/// * `workspace_ids` — IDs of the workspaces that contributed to this merge
///   (used to construct the commit message).
/// * `resolved` — Resolved file changes to apply to the epoch tree.
/// * `message` — Optional custom commit message. If `None`, a default message
///   is generated from `workspace_ids`.
///
/// # Returns
///
/// The OID of the new merge commit. Pass this to [`crate::merge::commit`]
/// to atomically advance `refs/manifold/epoch/current` and the branch ref.
///
/// # Determinism
///
/// Given the same `epoch`, `workspace_ids`, and `resolved` inputs (in any
/// order — this function sorts them internally), the output commit OID is
/// always the same. This follows from git's content-addressable storage:
/// identical tree content → identical OID.
pub fn build_merge_commit(
    root: &Path,
    epoch: &EpochId,
    workspace_ids: &[WorkspaceId],
    resolved: &[ResolvedChange],
    message: Option<&str>,
) -> Result<GitOid, BuildError> {
    // Step 1: Read the epoch tree into a flat map path -> (mode, blob_oid).
    let mut tree = read_epoch_tree(root, epoch)?;

    // Step 2: Apply resolved changes (sorted for determinism).
    let mut sorted = resolved.to_vec();
    sorted.sort_by(|a, b| a.path().cmp(b.path()));

    for change in &sorted {
        match change {
            ResolvedChange::Upsert { path, content } => {
                let blob_oid = write_blob(root, content)?;
                // Regular file mode. We preserve original mode if the file
                // already exists in the tree; otherwise use 100644.
                let mode = tree
                    .get(path)
                    .map_or_else(|| "100644".to_owned(), |(m, _)| m.clone());
                tree.insert(path.clone(), (mode, blob_oid.as_str().to_owned()));
            }
            ResolvedChange::Delete { path } => {
                tree.remove(path);
            }
        }
    }

    // Step 3: Build git tree objects bottom-up.
    let root_tree_oid = build_tree(root, &tree)?;

    // Step 4: Build commit message.
    let commit_msg = message.map_or_else(
        || {
            let mut ws_names: Vec<&str> = workspace_ids.iter().map(WorkspaceId::as_str).collect();
            ws_names.sort_unstable(); // deterministic order
            if ws_names.is_empty() {
                "epoch: merge".to_owned()
            } else {
                format!("epoch: merge {}", ws_names.join(" "))
            }
        },
        str::to_owned,
    );

    // Step 5: Create the commit.
    let commit_oid = create_commit(root, epoch, &root_tree_oid, &commit_msg)?;

    Ok(commit_oid)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// A flat representation of a git tree: path → (mode, `blob_oid`).
///
/// Using `BTreeMap` ensures lexicographic ordering when we iterate,
/// which is required for deterministic tree building.
type FlatTree = BTreeMap<PathBuf, (String, String)>;

/// Read the epoch's flat tree using `git ls-tree -r <epoch>`.
///
/// Returns a `BTreeMap` from relative path to `(mode, blob_oid)`.
fn read_epoch_tree(root: &Path, epoch: &EpochId) -> Result<FlatTree, BuildError> {
    // `-r` recurses into subtrees, giving a fully flat list of blobs.
    // `--full-tree` ensures paths are always relative to the repo root.
    // `--long` is not used — we only need mode, type, OID, and path.
    let output = Command::new("git")
        .args(["ls-tree", "-r", "--full-tree", epoch.as_str()])
        .current_dir(root)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(BuildError::GitCommand {
            command: format!("git ls-tree -r --full-tree {}", epoch.as_str()),
            stderr,
            exit_code: output.status.code(),
        });
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let mut tree = FlatTree::new();

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Format: "<mode> blob <oid>\t<path>"
        // We only include blobs (ignore tree entries — they appear in -r
        // output as recursive entries, but we only want blobs).
        let (meta, path_str) =
            line.split_once('\t')
                .ok_or_else(|| BuildError::MalformedLsTree {
                    line: line.to_owned(),
                })?;

        let parts: Vec<&str> = meta.split_whitespace().collect();
        if parts.len() < 3 {
            return Err(BuildError::MalformedLsTree {
                line: line.to_owned(),
            });
        }

        let mode = parts[0].to_owned();
        let obj_type = parts[1];
        let oid = parts[2].to_owned();

        // Skip tree entries (they are synthesized during build_tree).
        if obj_type != "blob" {
            continue;
        }

        tree.insert(PathBuf::from(path_str), (mode, oid));
    }

    Ok(tree)
}

/// Write a blob object to the git object store.
///
/// Pipes `content` into `git hash-object -w --stdin` and returns the OID.
fn write_blob(root: &Path, content: &[u8]) -> Result<GitOid, BuildError> {
    let mut child = Command::new("git")
        .args(["hash-object", "-w", "--stdin"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Write content to stdin.
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(content).map_err(BuildError::Io)?;
        // stdin is dropped here, closing the pipe.
    }

    let output = child.wait_with_output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(BuildError::GitCommand {
            command: "git hash-object -w --stdin".to_owned(),
            stderr,
            exit_code: output.status.code(),
        });
    }

    let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    GitOid::new(&raw).map_err(|_| BuildError::InvalidOid {
        context: "hash-object output".to_owned(),
        raw,
    })
}

/// Build the full git tree hierarchy from a flat path → (mode, oid) map.
///
/// `git mktree` builds a single tree level from its stdin. For nested paths,
/// we must build bottom-up: deepest subtrees first, then include the
/// resulting tree OIDs as entries in their parents.
///
/// Returns the root tree OID.
fn build_tree(root: &Path, flat: &FlatTree) -> Result<GitOid, BuildError> {
    // Group blobs by their parent directory path.
    // `dir_blobs[dir_path]` = list of (mode, "blob", oid, name) for direct children.
    // `dir_trees[dir_path]` = list of (mode, "tree", oid, name) for subtrees.
    // We use a HashMap<PathBuf, Vec<_>> where the key is the parent directory.

    // Collect all unique directory paths (excluding root "").
    let mut all_dirs: Vec<PathBuf> = vec![PathBuf::new()]; // root = empty path

    for path in flat.keys() {
        let mut current = path.parent().map(PathBuf::from).unwrap_or_default();
        loop {
            if current == PathBuf::new() || all_dirs.contains(&current) {
                break;
            }
            all_dirs.push(current.clone());
            current = current.parent().map(PathBuf::from).unwrap_or_default();
        }
    }

    // Sort directories: deepest first (longest path first) so we build bottom-up.
    all_dirs.sort_by(|a, b| {
        let a_depth = a.components().count();
        let b_depth = b.components().count();
        b_depth.cmp(&a_depth).then(a.cmp(b))
    });

    // Map from directory path → its tree OID (filled in as we process bottom-up).
    let mut tree_oids: HashMap<PathBuf, String> = HashMap::new();

    for dir in &all_dirs {
        // Collect direct-child blob entries for this directory.
        let mut entries: Vec<String> = Vec::new();

        // Blob entries: files directly under `dir`.
        for (path, (mode, oid)) in flat {
            let parent = path.parent().map(PathBuf::from).unwrap_or_default();
            if &parent == dir {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                entries.push(format!("{mode} blob {oid}\t{name}"));
            }
        }

        // Tree entries: subdirectories directly under `dir`.
        for (sub_path, sub_oid) in &tree_oids {
            let parent = sub_path.parent().map(PathBuf::from).unwrap_or_default();
            if &parent == dir {
                let name = sub_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                entries.push(format!("040000 tree {sub_oid}\t{name}"));
            }
        }

        // Sort entries for determinism.
        entries.sort();

        // Build this tree level using `git mktree`.
        let mktree_input = entries.join("\n") + if entries.is_empty() { "" } else { "\n" };
        let tree_oid = run_mktree(root, &mktree_input)?;
        tree_oids.insert(dir.clone(), tree_oid.as_str().to_owned());
    }

    // The root directory (PathBuf::new()) holds the root tree OID.
    let root_oid_str = tree_oids.get(&PathBuf::new()).cloned().unwrap_or_default();

    GitOid::new(&root_oid_str).map_err(|_| BuildError::InvalidOid {
        context: "root tree OID from mktree".to_owned(),
        raw: root_oid_str,
    })
}

/// Run `git mktree` with the given stdin input and return the tree OID.
fn run_mktree(root: &Path, input: &str) -> Result<GitOid, BuildError> {
    let mut child = Command::new("git")
        .args(["mktree"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input.as_bytes()).map_err(BuildError::Io)?;
    }

    let output = child.wait_with_output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(BuildError::GitCommand {
            command: "git mktree".to_owned(),
            stderr,
            exit_code: output.status.code(),
        });
    }

    let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    GitOid::new(&raw).map_err(|_| BuildError::InvalidOid {
        context: "git mktree output".to_owned(),
        raw,
    })
}

/// Create a git commit object.
///
/// Uses `git commit-tree <tree-oid> -p <parent-oid> -m <message>`.
/// Sets `GIT_AUTHOR_DATE` and `GIT_COMMITTER_DATE` to a fixed epoch
/// so that identical inputs produce identical commit OIDs (determinism).
///
/// # Note on identity
///
/// `git commit-tree` uses the repo's `user.name` and `user.email` config for
/// authorship. The timestamps are fixed to ensure deterministic output.
fn create_commit(
    root: &Path,
    parent: &EpochId,
    tree: &GitOid,
    message: &str,
) -> Result<GitOid, BuildError> {
    // Fixed timestamp for determinism: 2020-01-01T00:00:00Z epoch.
    // This makes identical inputs produce identical commit OIDs.
    const FIXED_TIMESTAMP: &str = "1577836800 +0000";

    let output = Command::new("git")
        .args([
            "commit-tree",
            tree.as_str(),
            "-p",
            parent.as_str(),
            "-m",
            message,
        ])
        .current_dir(root)
        .env("GIT_AUTHOR_DATE", FIXED_TIMESTAMP)
        .env("GIT_COMMITTER_DATE", FIXED_TIMESTAMP)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(BuildError::GitCommand {
            command: format!("git commit-tree {} -p {}", tree.as_str(), parent.as_str()),
            stderr,
            exit_code: output.status.code(),
        });
    }

    let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    GitOid::new(&raw).map_err(|_| BuildError::InvalidOid {
        context: "git commit-tree output".to_owned(),
        raw,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::{EpochId, WorkspaceId};
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Set up a fresh git repo with git identity configured.
    fn setup_git_repo() -> (TempDir, EpochId) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        run_git(root, &["init"]);
        run_git(root, &["config", "user.name", "Test"]);
        run_git(root, &["config", "user.email", "test@test.com"]);
        run_git(root, &["config", "commit.gpgsign", "false"]);

        // Initial commit with README.md
        fs::write(root.join("README.md"), "# Test\n").unwrap();
        run_git(root, &["add", "README.md"]);
        run_git(root, &["commit", "-m", "initial"]);

        let oid = git_oid(root, "HEAD");
        let epoch = EpochId::new(oid.as_str()).unwrap();
        (dir, epoch)
    }

    fn run_git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
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
    }

    fn git_oid(root: &Path, rev: &str) -> GitOid {
        let out = Command::new("git")
            .args(["rev-parse", rev])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(out.status.success(), "rev-parse {rev} failed");
        GitOid::new(String::from_utf8_lossy(&out.stdout).trim()).unwrap()
    }

    fn git_file_content(root: &Path, commit: &str, path: &str) -> String {
        let spec = format!("{commit}:{path}");
        let out = Command::new("git")
            .args(["show", &spec])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git show {spec} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn git_ls_tree_flat(root: &Path, commit: &str) -> Vec<String> {
        let out = Command::new("git")
            .args(["ls-tree", "-r", "--full-tree", "--name-only", commit])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(out.status.success(), "git ls-tree failed");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .filter(|l| !l.is_empty())
            .collect()
    }

    fn ws_ids(names: &[&str]) -> Vec<WorkspaceId> {
        names.iter().map(|n| WorkspaceId::new(n).unwrap()).collect()
    }

    // -----------------------------------------------------------------------
    // ResolvedChange tests
    // -----------------------------------------------------------------------

    #[test]
    fn resolved_change_path_upsert() {
        let rc = ResolvedChange::Upsert {
            path: PathBuf::from("foo.rs"),
            content: vec![],
        };
        assert_eq!(rc.path(), &PathBuf::from("foo.rs"));
        assert!(rc.is_upsert());
        assert!(!rc.is_delete());
    }

    #[test]
    fn resolved_change_path_delete() {
        let rc = ResolvedChange::Delete {
            path: PathBuf::from("bar.rs"),
        };
        assert_eq!(rc.path(), &PathBuf::from("bar.rs"));
        assert!(!rc.is_upsert());
        assert!(rc.is_delete());
    }

    // -----------------------------------------------------------------------
    // No changes → tree identical to epoch
    // -----------------------------------------------------------------------

    #[test]
    fn build_with_no_changes_matches_epoch_tree() {
        let (dir, epoch) = setup_git_repo();
        let root = dir.path();

        let commit_oid = build_merge_commit(root, &epoch, &ws_ids(&["alpha"]), &[], None).unwrap();

        // The new commit should have the same tree as the epoch.
        let epoch_tree = git_oid(root, &format!("{}^{{tree}}", epoch.as_str()));
        let new_tree = git_oid(root, &format!("{}^{{tree}}", commit_oid.as_str()));
        assert_eq!(
            epoch_tree, new_tree,
            "empty change-set should preserve tree"
        );
    }

    // -----------------------------------------------------------------------
    // Add a new file
    // -----------------------------------------------------------------------

    #[test]
    fn build_adds_new_file() {
        let (dir, epoch) = setup_git_repo();
        let root = dir.path();

        let resolved = vec![ResolvedChange::Upsert {
            path: PathBuf::from("src/main.rs"),
            content: b"fn main() {}".to_vec(),
        }];

        let commit_oid =
            build_merge_commit(root, &epoch, &ws_ids(&["agent-1"]), &resolved, None).unwrap();

        // File should be present in the new commit.
        let content = git_file_content(root, commit_oid.as_str(), "src/main.rs");
        assert_eq!(content, "fn main() {}");

        // Original file should still be present.
        let readme = git_file_content(root, commit_oid.as_str(), "README.md");
        assert_eq!(readme, "# Test\n");

        // Flat tree should include both files.
        let files = git_ls_tree_flat(root, commit_oid.as_str());
        assert!(files.contains(&"README.md".to_owned()));
        assert!(files.contains(&"src/main.rs".to_owned()));
    }

    // -----------------------------------------------------------------------
    // Modify an existing file
    // -----------------------------------------------------------------------

    #[test]
    fn build_modifies_existing_file() {
        let (dir, epoch) = setup_git_repo();
        let root = dir.path();

        let resolved = vec![ResolvedChange::Upsert {
            path: PathBuf::from("README.md"),
            content: b"# Updated\n".to_vec(),
        }];

        let commit_oid =
            build_merge_commit(root, &epoch, &ws_ids(&["agent-1"]), &resolved, None).unwrap();

        let content = git_file_content(root, commit_oid.as_str(), "README.md");
        assert_eq!(content, "# Updated\n");
    }

    // -----------------------------------------------------------------------
    // Delete a file
    // -----------------------------------------------------------------------

    #[test]
    fn build_deletes_file() {
        let (dir, epoch) = setup_git_repo();
        let root = dir.path();

        let resolved = vec![ResolvedChange::Delete {
            path: PathBuf::from("README.md"),
        }];

        let commit_oid =
            build_merge_commit(root, &epoch, &ws_ids(&["agent-1"]), &resolved, None).unwrap();

        // File should be absent from the new tree.
        let files = git_ls_tree_flat(root, commit_oid.as_str());
        assert!(
            !files.contains(&"README.md".to_owned()),
            "README.md should be deleted: {files:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Mixed: add, modify, delete in one merge
    // -----------------------------------------------------------------------

    #[test]
    fn build_mixed_changes() {
        // Set up epoch with two files: README.md and lib.rs
        let (dir, epoch0) = setup_git_repo();
        let root = dir.path();
        fs::write(root.join("lib.rs"), "pub fn lib() {}\n").unwrap();
        run_git(root, &["add", "lib.rs"]);
        run_git(root, &["commit", "-m", "add lib.rs"]);
        let epoch_oid = git_oid(root, "HEAD");
        let epoch = EpochId::new(epoch_oid.as_str()).unwrap();
        drop(epoch0); // epoch0 not used further

        let resolved = vec![
            ResolvedChange::Upsert {
                path: PathBuf::from("README.md"),
                content: b"# Modified\n".to_vec(),
            },
            ResolvedChange::Delete {
                path: PathBuf::from("lib.rs"),
            },
            ResolvedChange::Upsert {
                path: PathBuf::from("src/new.rs"),
                content: b"pub fn new() {}\n".to_vec(),
            },
        ];

        let commit_oid =
            build_merge_commit(root, &epoch, &ws_ids(&["a", "b"]), &resolved, None).unwrap();

        // README.md modified
        let readme = git_file_content(root, commit_oid.as_str(), "README.md");
        assert_eq!(readme, "# Modified\n");

        // lib.rs deleted
        let files = git_ls_tree_flat(root, commit_oid.as_str());
        assert!(
            !files.contains(&"lib.rs".to_owned()),
            "lib.rs should be gone"
        );

        // src/new.rs added
        assert!(
            files.contains(&"src/new.rs".to_owned()),
            "src/new.rs should be present: {files:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Commit message: auto-generated from workspace IDs
    // -----------------------------------------------------------------------

    #[test]
    fn build_commit_message_default() {
        let (dir, epoch) = setup_git_repo();
        let root = dir.path();

        let commit_oid =
            build_merge_commit(root, &epoch, &ws_ids(&["beta", "alpha"]), &[], None).unwrap();

        let log_out = Command::new("git")
            .args(["log", "--format=%s", "-1", commit_oid.as_str()])
            .current_dir(root)
            .output()
            .unwrap();
        let subject = String::from_utf8_lossy(&log_out.stdout).trim().to_owned();

        // Workspace IDs should be sorted: alpha before beta.
        assert_eq!(subject, "epoch: merge alpha beta");
    }

    #[test]
    fn build_commit_message_custom() {
        let (dir, epoch) = setup_git_repo();
        let root = dir.path();

        let commit_oid =
            build_merge_commit(root, &epoch, &ws_ids(&["a"]), &[], Some("custom: my merge"))
                .unwrap();

        let log_out = Command::new("git")
            .args(["log", "--format=%s", "-1", commit_oid.as_str()])
            .current_dir(root)
            .output()
            .unwrap();
        let subject = String::from_utf8_lossy(&log_out.stdout).trim().to_owned();
        assert_eq!(subject, "custom: my merge");
    }

    // -----------------------------------------------------------------------
    // Parent commit
    // -----------------------------------------------------------------------

    #[test]
    fn build_commit_parent_is_epoch() {
        let (dir, epoch) = setup_git_repo();
        let root = dir.path();

        let commit_oid = build_merge_commit(root, &epoch, &ws_ids(&["ws1"]), &[], None).unwrap();

        // New commit's parent must be the epoch.
        let parent_out = Command::new("git")
            .args(["rev-parse", &format!("{}^", commit_oid.as_str())])
            .current_dir(root)
            .output()
            .unwrap();
        let parent_oid = GitOid::new(String::from_utf8_lossy(&parent_out.stdout).trim()).unwrap();
        assert_eq!(parent_oid, *epoch.oid());
    }

    // -----------------------------------------------------------------------
    // Determinism: same inputs → same OID
    // -----------------------------------------------------------------------

    #[test]
    fn build_is_deterministic() {
        let (dir, epoch) = setup_git_repo();
        let root = dir.path();

        let resolved = vec![
            ResolvedChange::Upsert {
                path: PathBuf::from("a.rs"),
                content: b"fn a() {}".to_vec(),
            },
            ResolvedChange::Upsert {
                path: PathBuf::from("b.rs"),
                content: b"fn b() {}".to_vec(),
            },
        ];

        let oid1 =
            build_merge_commit(root, &epoch, &ws_ids(&["ws-a", "ws-b"]), &resolved, None).unwrap();
        let oid2 =
            build_merge_commit(root, &epoch, &ws_ids(&["ws-a", "ws-b"]), &resolved, None).unwrap();

        assert_eq!(oid1, oid2, "same inputs must produce same commit OID");
    }

    // -----------------------------------------------------------------------
    // Deeply nested paths
    // -----------------------------------------------------------------------

    #[test]
    fn build_handles_nested_paths() {
        let (dir, epoch) = setup_git_repo();
        let root = dir.path();

        let resolved = vec![
            ResolvedChange::Upsert {
                path: PathBuf::from("a/b/c/deep.rs"),
                content: b"fn deep() {}".to_vec(),
            },
            ResolvedChange::Upsert {
                path: PathBuf::from("a/b/c/other.rs"),
                content: b"fn other() {}".to_vec(),
            },
        ];

        let commit_oid =
            build_merge_commit(root, &epoch, &ws_ids(&["ws"]), &resolved, None).unwrap();

        let files = git_ls_tree_flat(root, commit_oid.as_str());
        assert!(
            files.contains(&"a/b/c/deep.rs".to_owned()),
            "nested path should be present: {files:?}"
        );
        assert!(
            files.contains(&"a/b/c/other.rs".to_owned()),
            "nested path should be present: {files:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Empty workspace list
    // -----------------------------------------------------------------------

    #[test]
    fn build_empty_workspace_list_uses_generic_message() {
        let (dir, epoch) = setup_git_repo();
        let root = dir.path();

        let commit_oid = build_merge_commit(root, &epoch, &[], &[], None).unwrap();

        let log_out = Command::new("git")
            .args(["log", "--format=%s", "-1", commit_oid.as_str()])
            .current_dir(root)
            .output()
            .unwrap();
        let subject = String::from_utf8_lossy(&log_out.stdout).trim().to_owned();
        assert_eq!(subject, "epoch: merge");
    }

    // -----------------------------------------------------------------------
    // Delete non-existent file is a no-op
    // -----------------------------------------------------------------------

    #[test]
    fn build_delete_nonexistent_file_is_noop() {
        let (dir, epoch) = setup_git_repo();
        let root = dir.path();

        let resolved = vec![ResolvedChange::Delete {
            path: PathBuf::from("does-not-exist.rs"),
        }];

        // Should succeed (deleting absent path is harmless)
        let commit_oid =
            build_merge_commit(root, &epoch, &ws_ids(&["ws"]), &resolved, None).unwrap();

        // README.md should still be present
        let files = git_ls_tree_flat(root, commit_oid.as_str());
        assert!(
            files.contains(&"README.md".to_owned()),
            "README.md should still be present: {files:?}"
        );
    }

    // -----------------------------------------------------------------------
    // BuildError display
    // -----------------------------------------------------------------------

    #[test]
    fn build_error_display_git_command() {
        let err = BuildError::GitCommand {
            command: "git mktree".to_owned(),
            stderr: "fatal: bad input".to_owned(),
            exit_code: Some(128),
        };
        let msg = format!("{err}");
        assert!(msg.contains("git mktree"), "missing command: {msg}");
        assert!(msg.contains("128"), "missing exit code: {msg}");
        assert!(msg.contains("fatal: bad input"), "missing stderr: {msg}");
    }

    #[test]
    fn build_error_display_malformed_ls_tree() {
        let err = BuildError::MalformedLsTree {
            line: "garbage line".to_owned(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("garbage line"), "missing line: {msg}");
    }

    #[test]
    fn build_error_display_invalid_oid() {
        let err = BuildError::InvalidOid {
            context: "test context".to_owned(),
            raw: "not-an-oid".to_owned(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("test context"), "missing context: {msg}");
        assert!(msg.contains("not-an-oid"), "missing raw: {msg}");
    }
}
