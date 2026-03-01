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
use std::path::{Path, PathBuf};

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
/// order -- this function sorts them internally), the output tree OID is
/// always the same. This follows from git's content-addressable storage:
/// identical tree content produces an identical tree OID. Commit OIDs will
/// vary because they include a real timestamp.
pub fn build_merge_commit(
    repo: &dyn maw_git::GitRepo,
    epoch: &EpochId,
    workspace_ids: &[WorkspaceId],
    resolved: &[ResolvedChange],
    message: Option<&str>,
) -> Result<GitOid, BuildError> {
    // Step 1: Read the epoch tree into a flat map path -> (mode, blob_oid).
    let mut tree = read_epoch_tree(repo, epoch)?;

    // Step 2: Apply resolved changes (sorted for determinism).
    let mut sorted = resolved.to_vec();
    sorted.sort_by(|a, b| a.path().cmp(b.path()));

    for change in &sorted {
        match change {
            ResolvedChange::Upsert { path, content } => {
                let blob_oid = write_blob(repo, content)?;
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
    let root_tree_oid = build_tree(repo, &tree)?;

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
    let commit_oid = create_commit(repo, epoch, &root_tree_oid, &commit_msg)?;

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

/// Read the epoch's flat tree using `GitRepo::read_tree()` and `read_commit()`.
///
/// Recursively walks the tree to produce a flat `BTreeMap` from relative path
/// to `(mode, blob_oid)`.
fn read_epoch_tree(repo: &dyn maw_git::GitRepo, epoch: &EpochId) -> Result<FlatTree, BuildError> {
    // Resolve the epoch commit to its tree OID.
    let epoch_oid = epoch.as_str().parse::<maw_git::GitOid>().map_err(|_| {
        BuildError::InvalidOid {
            context: "epoch OID parse".to_owned(),
            raw: epoch.as_str().to_owned(),
        }
    })?;
    let commit_info = repo.read_commit(epoch_oid).map_err(|e| {
        BuildError::GitCommand {
            command: format!("read_commit {}", epoch.as_str()),
            stderr: e.to_string(),
            exit_code: None,
        }
    })?;

    let mut tree = FlatTree::new();
    walk_tree_recursive(repo, commit_info.tree_oid, &PathBuf::new(), &mut tree)?;
    Ok(tree)
}

/// Recursively walk a tree object, collecting blobs into a flat map.
fn walk_tree_recursive(
    repo: &dyn maw_git::GitRepo,
    tree_oid: maw_git::GitOid,
    prefix: &Path,
    flat: &mut FlatTree,
) -> Result<(), BuildError> {
    let entries = repo.read_tree(tree_oid).map_err(|e| BuildError::GitCommand {
        command: format!("read_tree {tree_oid}"),
        stderr: e.to_string(),
        exit_code: None,
    })?;

    for entry in entries {
        let entry_path = if prefix == Path::new("") {
            PathBuf::from(&entry.name)
        } else {
            prefix.join(&entry.name)
        };

        match entry.mode {
            maw_git::EntryMode::Tree => {
                // Recurse into subtrees.
                walk_tree_recursive(repo, entry.oid, &entry_path, flat)?;
            }
            _ => {
                // Blob, executable, symlink, gitlink — collect as flat entry.
                let mode_str = match entry.mode {
                    maw_git::EntryMode::Blob => "100644",
                    maw_git::EntryMode::BlobExecutable => "100755",
                    maw_git::EntryMode::Link => "120000",
                    maw_git::EntryMode::Commit => "160000",
                    maw_git::EntryMode::Tree => unreachable!(),
                };
                flat.insert(entry_path, (mode_str.to_owned(), entry.oid.to_string()));
            }
        }
    }
    Ok(())
}

/// Write a blob object to the git object store via `GitRepo::write_blob`.
///
/// Returns the OID of the written blob.
fn write_blob(repo: &dyn maw_git::GitRepo, content: &[u8]) -> Result<GitOid, BuildError> {
    let git_oid = repo.write_blob(content).map_err(|e| BuildError::GitCommand {
        command: "write_blob".to_owned(),
        stderr: e.to_string(),
        exit_code: None,
    })?;
    let oid_str = git_oid.to_string();
    GitOid::new(&oid_str).map_err(|_| BuildError::InvalidOid {
        context: "write_blob output".to_owned(),
        raw: oid_str,
    })
}

/// Build the full git tree hierarchy from a flat path -> (mode, oid) map.
///
/// Uses `GitRepo::write_tree()` to build tree objects bottom-up: deepest
/// subtrees first, then include the resulting tree OIDs as entries in their
/// parents.
///
/// Returns the root tree OID.
fn build_tree(repo: &dyn maw_git::GitRepo, flat: &FlatTree) -> Result<GitOid, BuildError> {
    // Collect all unique directory paths (including root "").
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

    // Map from directory path -> its tree OID (filled in as we process bottom-up).
    let mut tree_oids: HashMap<PathBuf, maw_git::GitOid> = HashMap::new();

    for dir in &all_dirs {
        let mut tree_entries: Vec<maw_git::TreeEntry> = Vec::new();

        // Blob entries: files directly under `dir`.
        for (path, (mode_str, oid_str)) in flat {
            let parent = path.parent().map(PathBuf::from).unwrap_or_default();
            if &parent == dir {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_owned();
                let mode = match mode_str.as_str() {
                    "100755" => maw_git::EntryMode::BlobExecutable,
                    "120000" => maw_git::EntryMode::Link,
                    "160000" => maw_git::EntryMode::Commit,
                    _ => maw_git::EntryMode::Blob,
                };
                let oid = oid_str.parse::<maw_git::GitOid>().map_err(|_| {
                    BuildError::InvalidOid {
                        context: format!("blob OID for {}", path.display()),
                        raw: oid_str.clone(),
                    }
                })?;
                tree_entries.push(maw_git::TreeEntry { name, mode, oid });
            }
        }

        // Tree entries: subdirectories directly under `dir`.
        for (sub_path, sub_oid) in &tree_oids {
            let parent = sub_path.parent().map(PathBuf::from).unwrap_or_default();
            if &parent == dir {
                let name = sub_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_owned();
                tree_entries.push(maw_git::TreeEntry {
                    name,
                    mode: maw_git::EntryMode::Tree,
                    oid: *sub_oid,
                });
            }
        }

        // Sort entries by name for determinism.
        tree_entries.sort_by(|a, b| a.name.cmp(&b.name));

        // Write this tree level.
        let tree_oid = repo.write_tree(&tree_entries).map_err(|e| {
            BuildError::GitCommand {
                command: "write_tree".to_owned(),
                stderr: e.to_string(),
                exit_code: None,
            }
        })?;
        tree_oids.insert(dir.clone(), tree_oid);
    }

    // The root directory (PathBuf::new()) holds the root tree OID.
    let root_git_oid = tree_oids
        .get(&PathBuf::new())
        .copied()
        .ok_or_else(|| BuildError::InvalidOid {
            context: "root tree OID from write_tree".to_owned(),
            raw: String::new(),
        })?;

    let oid_str = root_git_oid.to_string();
    GitOid::new(&oid_str).map_err(|_| BuildError::InvalidOid {
        context: "root tree OID from write_tree".to_owned(),
        raw: oid_str,
    })
}

/// Create a git commit object via `GitRepo::create_commit`.
///
/// Uses the repo's configured `user.name` and `user.email` for authorship.
///
/// # Determinism
///
/// Tree content is still fully deterministic (same inputs produce the same
/// tree OID). Commit OIDs will vary because they include a real timestamp,
/// but this is the expected behavior for merge commits that should reflect
/// when they actually occurred.
fn create_commit(
    repo: &dyn maw_git::GitRepo,
    parent: &EpochId,
    tree: &GitOid,
    message: &str,
) -> Result<GitOid, BuildError> {
    let tree_git_oid = tree.as_str().parse::<maw_git::GitOid>().map_err(|_| {
        BuildError::InvalidOid {
            context: "tree OID parse for create_commit".to_owned(),
            raw: tree.as_str().to_owned(),
        }
    })?;
    let parent_git_oid = parent.as_str().parse::<maw_git::GitOid>().map_err(|_| {
        BuildError::InvalidOid {
            context: "parent OID parse for create_commit".to_owned(),
            raw: parent.as_str().to_owned(),
        }
    })?;

    let commit_git_oid = repo
        .create_commit(tree_git_oid, &[parent_git_oid], message, None)
        .map_err(|e| BuildError::GitCommand {
            command: format!("create_commit tree={} parent={}", tree.as_str(), parent.as_str()),
            stderr: e.to_string(),
            exit_code: None,
        })?;

    let raw = commit_git_oid.to_string();
    GitOid::new(&raw).map_err(|_| BuildError::InvalidOid {
        context: "create_commit output".to_owned(),
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
    fn setup_git_repo() -> (TempDir, EpochId, Box<dyn maw_git::GitRepo>) {
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
        let repo = open_test_repo(root);
        (dir, epoch, repo)
    }

    fn open_test_repo(root: &Path) -> Box<dyn maw_git::GitRepo> {
        Box::new(maw_git::GixRepo::open(root).unwrap())
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
        let (dir, epoch, repo) = setup_git_repo();
        let root = dir.path();

        let commit_oid = build_merge_commit(&*repo, &epoch, &ws_ids(&["alpha"]), &[], None).unwrap();

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
        let (dir, epoch, repo) = setup_git_repo();
        let root = dir.path();

        let resolved = vec![ResolvedChange::Upsert {
            path: PathBuf::from("src/main.rs"),
            content: b"fn main() {}".to_vec(),
        }];

        let commit_oid =
            build_merge_commit(&*repo, &epoch, &ws_ids(&["agent-1"]), &resolved, None).unwrap();

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
        let (dir, epoch, repo) = setup_git_repo();
        let root = dir.path();

        let resolved = vec![ResolvedChange::Upsert {
            path: PathBuf::from("README.md"),
            content: b"# Updated\n".to_vec(),
        }];

        let commit_oid =
            build_merge_commit(&*repo, &epoch, &ws_ids(&["agent-1"]), &resolved, None).unwrap();

        let content = git_file_content(root, commit_oid.as_str(), "README.md");
        assert_eq!(content, "# Updated\n");
    }

    // -----------------------------------------------------------------------
    // Delete a file
    // -----------------------------------------------------------------------

    #[test]
    fn build_deletes_file() {
        let (dir, epoch, repo) = setup_git_repo();
        let root = dir.path();

        let resolved = vec![ResolvedChange::Delete {
            path: PathBuf::from("README.md"),
        }];

        let commit_oid =
            build_merge_commit(&*repo, &epoch, &ws_ids(&["agent-1"]), &resolved, None).unwrap();

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
        let (dir, _epoch0, _repo0) = setup_git_repo();
        let root = dir.path();
        fs::write(root.join("lib.rs"), "pub fn lib() {}\n").unwrap();
        run_git(root, &["add", "lib.rs"]);
        run_git(root, &["commit", "-m", "add lib.rs"]);
        let epoch_oid = git_oid(root, "HEAD");
        let epoch = EpochId::new(epoch_oid.as_str()).unwrap();
        let repo = open_test_repo(root);

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
            build_merge_commit(&*repo, &epoch, &ws_ids(&["a", "b"]), &resolved, None).unwrap();

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
        let (dir, epoch, repo) = setup_git_repo();
        let root = dir.path();

        let commit_oid =
            build_merge_commit(&*repo, &epoch, &ws_ids(&["beta", "alpha"]), &[], None).unwrap();

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
        let (dir, epoch, repo) = setup_git_repo();
        let root = dir.path();

        let commit_oid =
            build_merge_commit(&*repo, &epoch, &ws_ids(&["a"]), &[], Some("custom: my merge"))
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
        let (dir, epoch, repo) = setup_git_repo();
        let root = dir.path();

        let commit_oid = build_merge_commit(&*repo, &epoch, &ws_ids(&["ws1"]), &[], None).unwrap();

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
    // Determinism: same inputs → same tree OID
    // -----------------------------------------------------------------------

    #[test]
    fn build_tree_is_deterministic() {
        let (dir, epoch, repo) = setup_git_repo();
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
            build_merge_commit(&*repo, &epoch, &ws_ids(&["ws-a", "ws-b"]), &resolved, None).unwrap();
        let oid2 =
            build_merge_commit(&*repo, &epoch, &ws_ids(&["ws-a", "ws-b"]), &resolved, None).unwrap();

        // Tree OIDs must be identical (content-addressed).
        let tree1 = git_oid(root, &format!("{}^{{tree}}", oid1.as_str()));
        let tree2 = git_oid(root, &format!("{}^{{tree}}", oid2.as_str()));
        assert_eq!(tree1, tree2, "same inputs must produce same tree OID");
    }

    // -----------------------------------------------------------------------
    // Merge commits use real timestamps (not fixed/synthetic)
    // -----------------------------------------------------------------------

    #[test]
    fn build_commit_uses_real_timestamp() {
        let (dir, epoch, repo) = setup_git_repo();
        let root = dir.path();

        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let commit_oid =
            build_merge_commit(&*repo, &epoch, &ws_ids(&["ws1"]), &[], None).unwrap();

        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Read the author date from the commit.
        let log_out = Command::new("git")
            .args(["log", "--format=%at", "-1", commit_oid.as_str()])
            .current_dir(root)
            .output()
            .unwrap();
        let author_ts: u64 = String::from_utf8_lossy(&log_out.stdout)
            .trim()
            .parse()
            .unwrap();

        assert!(
            author_ts >= before && author_ts <= after,
            "commit timestamp {author_ts} should be between {before} and {after}"
        );
    }

    // -----------------------------------------------------------------------
    // Deeply nested paths
    // -----------------------------------------------------------------------

    #[test]
    fn build_handles_nested_paths() {
        let (dir, epoch, repo) = setup_git_repo();
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
            build_merge_commit(&*repo, &epoch, &ws_ids(&["ws"]), &resolved, None).unwrap();

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
        let (dir, epoch, repo) = setup_git_repo();
        let root = dir.path();

        let commit_oid = build_merge_commit(&*repo, &epoch, &[], &[], None).unwrap();

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
        let (dir, epoch, repo) = setup_git_repo();
        let root = dir.path();

        let resolved = vec![ResolvedChange::Delete {
            path: PathBuf::from("does-not-exist.rs"),
        }];

        // Should succeed (deleting absent path is harmless)
        let commit_oid =
            build_merge_commit(&*repo, &epoch, &ws_ids(&["ws"]), &resolved, None).unwrap();

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
