//! Git worktree backend implementation.
//!
//! Implements [`WorkspaceBackend`] using `git worktree` for workspace
//! isolation. Each workspace is a detached worktree under `ws/<name>/`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{SnapshotResult, WorkspaceBackend, WorkspaceStatus};
use crate::config::ManifoldConfig;
use crate::model::types::{
    EpochId, GitOid, WorkspaceId, WorkspaceInfo, WorkspaceMode, WorkspaceState,
};
use crate::refs as manifold_refs;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors from the git worktree backend.
#[derive(Debug)]
pub enum GitBackendError {
    /// A git command failed.
    GitCommand {
        command: String,
        stderr: String,
        exit_code: Option<i32>,
    },
    /// An I/O error occurred.
    Io(std::io::Error),
    /// Workspace not found.
    NotFound { name: String },
    /// Feature not yet implemented.
    NotImplemented(&'static str),
}

impl fmt::Display for GitBackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitCommand {
                command,
                stderr,
                exit_code,
            } => {
                write!(f, "`{command}` failed")?;
                if let Some(code) = exit_code {
                    write!(f, " (exit code {code})")?;
                }
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::NotFound { name } => write!(f, "workspace '{name}' not found"),
            Self::NotImplemented(method) => write!(f, "{method} not yet implemented"),
        }
    }
}

impl std::error::Error for GitBackendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for GitBackendError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ---------------------------------------------------------------------------
// GitWorktreeBackend
// ---------------------------------------------------------------------------

/// A workspace backend implementation using `git worktree`.
pub struct GitWorktreeBackend {
    /// The root directory of the repository (where .git is).
    root: PathBuf,
}

impl GitWorktreeBackend {
    /// Create a new `GitWorktreeBackend`.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Get the directory where workspaces are stored.
    fn workspaces_dir(&self) -> PathBuf {
        self.root.join("ws")
    }

    /// Run a git command and return its stdout.
    fn git_stdout(&self, args: &[&str]) -> Result<String, GitBackendError> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .map_err(|e| GitBackendError::Io(e))?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(GitBackendError::GitCommand {
                command: format!("git {}", args.join(" ")),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                exit_code: output.status.code(),
            })
        }
    }

    /// Run a git command in a specific directory and return stdout.
    fn git_stdout_in(
        &self,
        dir: &std::path::Path,
        args: &[&str],
    ) -> Result<String, GitBackendError> {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .map_err(|e| GitBackendError::Io(e))?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(GitBackendError::GitCommand {
                command: format!("git {}", args.join(" ")),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                exit_code: output.status.code(),
            })
        }
    }

    /// Run a git command, ignoring output.
    fn git_run(&self, args: &[&str]) -> Result<(), GitBackendError> {
        self.git_stdout(args)?;
        Ok(())
    }

    /// Get the current epoch from `refs/manifold/epoch/current`, if it exists.
    ///
    /// Returns `None` if the ref doesn't exist (e.g., Manifold not yet initialized).
    fn current_epoch_opt(&self) -> Option<EpochId> {
        let output = Command::new("git")
            .args(["rev-parse", "refs/manifold/epoch/current"])
            .current_dir(&self.root)
            .output()
            .ok()?;
        if output.status.success() {
            let oid_str = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            EpochId::new(&oid_str).ok()
        } else {
            None
        }
    }

    /// Count how many commits are reachable from `to_oid` but not from `from_oid`.
    ///
    /// Used to determine how many epoch advancements a workspace is behind.
    /// Returns `None` on error (e.g., either OID is not reachable).
    fn count_commits_between(&self, from_oid: &str, to_oid: &str) -> Option<u32> {
        let range = format!("{from_oid}..{to_oid}");
        let output = Command::new("git")
            .args(["rev-list", "--count", &range])
            .current_dir(&self.root)
            .output()
            .ok()?;
        if output.status.success() {
            String::from_utf8_lossy(&output.stdout).trim().parse().ok()
        } else {
            None
        }
    }

    /// Whether Level 1 workspace refs are enabled in `.manifold/config.toml`.
    ///
    /// Missing config or parse/load failures fall back to enabled.
    fn git_compat_refs_enabled(&self) -> bool {
        let config_path = self.root.join(".manifold").join("config.toml");
        ManifoldConfig::load(&config_path)
            .map(|cfg| cfg.workspace.git_compat_refs)
            .unwrap_or(true)
    }

    /// Refresh `refs/manifold/ws/<name>` to point at a commit representing the
    /// current workspace state.
    ///
    /// Uses `git stash create` to lazily materialize a commit without mutating
    /// the workspace's index or working tree. If there are no local changes,
    /// falls back to `HEAD`.
    fn refresh_workspace_state_ref(
        &self,
        name: &WorkspaceId,
        ws_path: &Path,
    ) -> Result<(), GitBackendError> {
        if !self.git_compat_refs_enabled() {
            return Ok(());
        }

        let ref_name = manifold_refs::workspace_state_ref(name.as_str());

        let stash_oid = self.git_stdout_in(ws_path, &["stash", "create"])?;
        let oid_str = stash_oid.trim();

        let materialized = if oid_str.is_empty() {
            self.git_stdout_in(ws_path, &["rev-parse", "HEAD"])?
        } else {
            stash_oid
        };
        let materialized = materialized.trim();

        let oid = GitOid::new(materialized).map_err(|e| GitBackendError::GitCommand {
            command: "git stash create / git rev-parse HEAD".to_owned(),
            stderr: format!("invalid OID while materializing workspace ref: {e}"),
            exit_code: None,
        })?;

        manifold_refs::write_ref(&self.root, &ref_name, &oid).map_err(|e| {
            GitBackendError::GitCommand {
                command: format!("git update-ref {ref_name} {}", oid.as_str()),
                stderr: e.to_string(),
                exit_code: None,
            }
        })
    }
}

impl WorkspaceBackend for GitWorktreeBackend {
    type Error = GitBackendError;

    fn create(&self, name: &WorkspaceId, epoch: &EpochId) -> Result<WorkspaceInfo, Self::Error> {
        let path = self.workspace_path(name);

        // Idempotency: if valid workspace exists, return it
        if self.exists(name) {
            return Ok(WorkspaceInfo {
                id: name.clone(),
                path,
                epoch: epoch.clone(),
                state: WorkspaceState::Active,
                mode: WorkspaceMode::default(),
            });
        }

        // Cleanup: if directory exists but not a valid workspace, remove it
        if path.exists() {
            std::fs::remove_dir_all(&path)?;
        }

        // Cleanup: if git thinks it exists but directory is gone (prune)
        let _ = Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(&self.root)
            .output();

        // Ensure parent directory exists
        let ws_dir = self.workspaces_dir();
        std::fs::create_dir_all(&ws_dir)?;

        // Create the worktree: git worktree add --detach <path> <commit>
        let path_str = path.to_str().unwrap();
        let output = Command::new("git")
            .args(["worktree", "add", "--detach", path_str, epoch.as_str()])
            .current_dir(&self.root)
            .output()
            .map_err(|e| GitBackendError::Io(e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();

            // Clean up partial state
            if path.exists() {
                let _ = std::fs::remove_dir_all(&path);
            }

            return Err(GitBackendError::GitCommand {
                command: "git worktree add".to_owned(),
                stderr,
                exit_code: output.status.code(),
            });
        }

        Ok(WorkspaceInfo {
            id: name.clone(),
            path,
            epoch: epoch.clone(),
            state: WorkspaceState::Active,
            mode: WorkspaceMode::default(),
        })
    }

    /// Destroy a workspace by removing its git worktree.
    ///
    /// This is atomic and idempotent:
    /// - If the workspace doesn't exist (already destroyed), returns Ok(()).
    /// - Step 1: `git worktree remove --force <path>` (handles dirty worktrees)
    /// - Step 2: If that fails, remove the directory manually and prune.
    /// - Step 3: `git worktree prune` to clean up stale references.
    ///
    /// Each step is individually idempotent, so a crash at any point can be
    /// retried safely.
    fn destroy(&self, name: &WorkspaceId) -> Result<(), Self::Error> {
        let path = self.workspace_path(name);

        // Step 1: Try `git worktree remove --force`
        // --force allows removing even if there are uncommitted changes
        if path.exists() {
            let path_str = path.to_str().unwrap();
            let output = Command::new("git")
                .args(["worktree", "remove", "--force", path_str])
                .current_dir(&self.root)
                .output()
                .map_err(GitBackendError::Io)?;

            if !output.status.success() {
                // Step 2: If `git worktree remove` fails, fall back to manual cleanup.
                // This handles cases where the worktree is in a broken state.
                if path.exists() {
                    std::fs::remove_dir_all(&path)?;
                }
            }
        }

        // Step 3: Prune stale worktree entries. This cleans up the
        // .git/worktrees/<name> administrative directory even if
        // the worktree directory was removed out of band.
        let _ = Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(&self.root)
            .output();

        // Prune Level 1 materialized workspace ref if present.
        let ws_ref = manifold_refs::workspace_state_ref(name.as_str());
        let _ = manifold_refs::delete_ref(&self.root, &ws_ref);

        Ok(())
    }

    /// List all workspaces managed by this backend.
    ///
    /// Parses `git worktree list --porcelain` and filters to worktrees directly
    /// under the `ws/` directory. The main worktree (repo root) and any
    /// non-`ws/` worktrees are excluded.
    ///
    /// Staleness is determined by comparing each workspace's HEAD against
    /// `refs/manifold/epoch/current`. If the epoch ref doesn't exist
    /// (Manifold not yet initialized), all workspaces are reported as Active.
    fn list(&self) -> Result<Vec<WorkspaceInfo>, Self::Error> {
        let output = self.git_stdout(&["worktree", "list", "--porcelain"])?;
        let current_epoch = self.current_epoch_opt();
        let ws_dir = self.workspaces_dir();

        let mut infos = Vec::new();

        // git worktree list --porcelain separates entries with blank lines.
        for block in output.split("\n\n") {
            let block = block.trim();
            if block.is_empty() {
                continue;
            }

            let mut wt_path: Option<PathBuf> = None;
            let mut wt_head: Option<String> = None;
            let mut is_bare = false;

            for line in block.lines() {
                if let Some(p) = line.strip_prefix("worktree ") {
                    wt_path = Some(PathBuf::from(p));
                } else if let Some(h) = line.strip_prefix("HEAD ") {
                    wt_head = Some(h.to_owned());
                } else if line.trim() == "bare" {
                    is_bare = true;
                }
            }

            // Skip bare repo entries (the main git repo root).
            if is_bare {
                continue;
            }

            let (path, head_str) = match (wt_path, wt_head) {
                (Some(p), Some(h)) => (p, h),
                // Missing HEAD means the worktree is in a broken state; skip.
                _ => continue,
            };

            // Only include workspaces directly under ws/ (e.g., ws/agent-1).
            let rel = match path.strip_prefix(&ws_dir) {
                Ok(r) => r,
                Err(_) => continue,
            };

            // Exactly one path component (ws/<name>, not ws/<a>/<b>).
            let components: Vec<_> = rel.components().collect();
            if components.len() != 1 {
                continue;
            }
            let name_str = match components[0].as_os_str().to_str() {
                Some(s) => s,
                None => continue,
            };

            let id = match WorkspaceId::new(name_str) {
                Ok(id) => id,
                // Non-conforming directory name (e.g., uppercase); skip.
                Err(_) => continue,
            };

            let epoch = match EpochId::new(head_str.trim()) {
                Ok(e) => e,
                // Invalid OID (e.g., detached with no commits); skip.
                Err(_) => continue,
            };

            let state = match &current_epoch {
                Some(current) if epoch == *current => WorkspaceState::Active,
                Some(current) => {
                    let behind = self
                        .count_commits_between(epoch.as_str(), current.as_str())
                        .unwrap_or(1);
                    WorkspaceState::Stale {
                        behind_epochs: behind,
                    }
                }
                // No epoch ref: can't determine staleness, assume active.
                None => WorkspaceState::Active,
            };

            infos.push(WorkspaceInfo {
                id,
                path,
                epoch,
                state,
                mode: WorkspaceMode::default(),
            });
        }

        Ok(infos)
    }

    /// Get the current status of a workspace.
    ///
    /// Reports dirty files (modified, added, deleted, untracked) by running
    /// `git status --porcelain` inside the worktree directory.
    ///
    /// Staleness is determined by comparing the workspace's HEAD (the epoch
    /// it was created at) against `refs/manifold/epoch/current`.
    fn status(&self, name: &WorkspaceId) -> Result<WorkspaceStatus, Self::Error> {
        let ws_path = self.workspace_path(name);

        if !ws_path.exists() {
            return Err(GitBackendError::NotFound {
                name: name.as_str().to_owned(),
            });
        }

        // The workspace HEAD is the epoch commit it was created at.
        // Agents make working-tree changes only (no commits), so HEAD is stable.
        let head_str = self.git_stdout_in(&ws_path, &["rev-parse", "HEAD"])?;
        let base_epoch =
            EpochId::new(head_str.trim()).map_err(|e| GitBackendError::GitCommand {
                command: "git rev-parse HEAD".to_owned(),
                stderr: format!("invalid OID from HEAD: {e}"),
                exit_code: None,
            })?;

        // Collect dirty files: tracked modifications + untracked files.
        let status_output = self.git_stdout_in(&ws_path, &["status", "--porcelain"])?;
        let dirty_files = parse_porcelain_status(&status_output);

        // Stale = workspace epoch != current epoch.
        let is_stale = self
            .current_epoch_opt()
            .map(|current| base_epoch != current)
            .unwrap_or(false);

        // Lazily materialize Level 1 workspace state ref for git inspection.
        self.refresh_workspace_state_ref(name, &ws_path)?;

        Ok(WorkspaceStatus::new(base_epoch, dirty_files, is_stale))
    }

    /// Scan a workspace's working directory for changes relative to the base epoch.
    ///
    /// Detects added, modified, and deleted files by comparing the workspace's
    /// working tree against the epoch commit. Also picks up untracked files
    /// as additions.
    ///
    /// # Implementation
    /// 1. `git diff --name-status <epoch> HEAD` — committed changes
    /// 2. `git diff --name-status HEAD` — uncommitted tracked changes
    /// 3. `git ls-files --others --exclude-standard` — untracked files
    fn snapshot(&self, name: &WorkspaceId) -> Result<SnapshotResult, Self::Error> {
        let ws_path = self.workspace_path(name);
        if !ws_path.exists() {
            return Err(GitBackendError::NotFound {
                name: name.as_str().to_owned(),
            });
        }

        // Read the base epoch from the worktree's HEAD
        // (worktree HEAD is set to the epoch commit on creation)
        let head_oid = self.git_stdout_in(&ws_path, &["rev-parse", "HEAD"])?;
        let head_oid = head_oid.trim();

        let mut added = Vec::new();
        let mut modified = Vec::new();
        let mut deleted = Vec::new();

        // 1. Uncommitted changes relative to HEAD (working tree vs index+HEAD)
        let diff_output = self.git_stdout_in(&ws_path, &["diff", "--name-status", head_oid])?;

        parse_name_status(&diff_output, &mut added, &mut modified, &mut deleted);

        // 2. Staged changes not yet reflected (index vs HEAD)
        let staged_output =
            self.git_stdout_in(&ws_path, &["diff", "--name-status", "--cached", head_oid])?;

        parse_name_status(&staged_output, &mut added, &mut modified, &mut deleted);

        // 3. Untracked files (not in .gitignore)
        let untracked_output =
            self.git_stdout_in(&ws_path, &["ls-files", "--others", "--exclude-standard"])?;

        for line in untracked_output.lines() {
            let path = line.trim();
            if !path.is_empty() {
                let p = PathBuf::from(path);
                if !added.contains(&p) {
                    added.push(p);
                }
            }
        }

        // Deduplicate (a file might appear in both staged and unstaged)
        added.sort();
        added.dedup();
        modified.sort();
        modified.dedup();
        deleted.sort();
        deleted.dedup();

        // Remove from modified/deleted if also in added (file was added then modified)
        modified.retain(|p| !added.contains(p));

        // Lazily materialize Level 1 workspace state ref for git inspection.
        self.refresh_workspace_state_ref(name, &ws_path)?;

        Ok(SnapshotResult::new(added, modified, deleted))
    }

    fn workspace_path(&self, name: &WorkspaceId) -> PathBuf {
        self.workspaces_dir().join(name.as_str())
    }

    fn exists(&self, name: &WorkspaceId) -> bool {
        let path = self.workspace_path(name);
        if !path.exists() {
            return false;
        }

        // Check if git worktree list knows about it
        let output = Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(&self.root)
            .output();

        if let Ok(out) = output {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let path_str = path.to_str().unwrap_or_default();
            for line in stdout.lines() {
                if let Some(wt_path) = line.strip_prefix("worktree ") {
                    if wt_path == path_str {
                        return true;
                    }
                }
            }
        }

        false
    }
}

// ---------------------------------------------------------------------------
// Porcelain parsers
// ---------------------------------------------------------------------------

/// A single entry from `git worktree list --porcelain`.
#[cfg(test)]
#[derive(Debug, Default)]
struct WorktreeEntry {
    /// Absolute path to the worktree.
    path: String,
    /// HEAD commit OID (40 hex chars), or `None` if the worktree has no commits.
    head: Option<String>,
    /// Branch name (e.g., `"refs/heads/main"`), or `None` if detached.
    #[allow(dead_code)]
    branch: Option<String>,
}

/// Parse the `--porcelain` output of `git worktree list`.
///
/// Format (one blank line between entries):
/// ```text
/// worktree /absolute/path
/// HEAD <40-char-oid>
/// branch refs/heads/main      ← or "detached"
///
/// worktree /other/path
/// ...
/// ```
#[cfg(test)]
fn parse_worktree_porcelain(raw: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut current = WorktreeEntry::default();
    let mut in_entry = false;

    for line in raw.lines() {
        if line.is_empty() {
            if in_entry && !current.path.is_empty() {
                entries.push(current);
                current = WorktreeEntry::default();
                in_entry = false;
            }
            continue;
        }

        if let Some(path) = line.strip_prefix("worktree ") {
            current.path = path.trim().to_owned();
            in_entry = true;
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            current.head = Some(head.trim().to_owned());
        } else if let Some(branch) = line.strip_prefix("branch ") {
            current.branch = Some(branch.trim().to_owned());
        }
        // "detached" line: no branch, already handled by leaving branch as None
    }

    // Flush the last entry (no trailing blank line).
    if in_entry && !current.path.is_empty() {
        entries.push(current);
    }

    entries
}

/// Parse `git status --porcelain` v1 output to extract dirty file paths.
///
/// Each non-empty line has the format `XY path` where `X` is the index
/// status, `Y` is the working-tree status, and the path starts at position 3.
/// All lines are included (modified, added, deleted, untracked `??`).
///
/// Paths containing spaces are returned verbatim; quoted paths (git uses
/// quoting for special characters) are returned with the quotes stripped.
fn parse_porcelain_status(output: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for line in output.lines() {
        // Minimum valid line: "XY p" (4 chars: 2 status + space + 1 path char)
        if line.len() < 4 {
            continue;
        }
        // Path starts at byte offset 3 (after "XY ").
        let path_str = &line[3..];
        if !path_str.is_empty() {
            // Strip quotes if present (git quotes paths with special chars).
            let path_str = path_str
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(path_str);
            paths.push(PathBuf::from(path_str));
        }
    }
    paths
}
/// Parse `git diff --name-status` output into add/modify/delete lists.
fn parse_name_status(
    output: &str,
    added: &mut Vec<PathBuf>,
    modified: &mut Vec<PathBuf>,
    deleted: &mut Vec<PathBuf>,
) {
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Format: "X\tpath" or "X path"
        let (status, path) = if let Some(rest) =
            line.strip_prefix("A\t").or_else(|| line.strip_prefix("A "))
        {
            ('A', rest.trim())
        } else if let Some(rest) = line.strip_prefix("M\t").or_else(|| line.strip_prefix("M ")) {
            ('M', rest.trim())
        } else if let Some(rest) = line.strip_prefix("D\t").or_else(|| line.strip_prefix("D ")) {
            ('D', rest.trim())
        } else if let Some(rest) = line.strip_prefix("R\t").or_else(|| line.strip_prefix("R ")) {
            // Rename: "R100\told\tnew" — treat old as deleted, new as added
            if let Some((_, new)) = rest.split_once('\t') {
                added.push(PathBuf::from(new.trim()));
            }
            continue;
        } else {
            // Unknown status letter, skip
            continue;
        };

        let p = PathBuf::from(path);
        match status {
            'A' => added.push(p),
            'M' => modified.push(p),
            'D' => deleted.push(p),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Helper: set up a fresh git repo with one commit.
    fn setup_git_repo() -> (TempDir, EpochId) {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();

        Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(root)
            .output()
            .unwrap();

        fs::write(root.join("README.md"), "# Test Repo").unwrap();
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Initial commit"])
            .current_dir(root)
            .output()
            .unwrap();

        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        let oid_str = String::from_utf8(output.stdout).unwrap().trim().to_string();
        let epoch = EpochId::new(&oid_str).unwrap();

        (temp_dir, epoch)
    }

    fn read_ws_ref(root: &std::path::Path, ws: &str) -> Option<String> {
        let ref_name = manifold_refs::workspace_state_ref(ws);
        let out = Command::new("git")
            .args(["rev-parse", &ref_name])
            .current_dir(root)
            .output()
            .unwrap();
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8(out.stdout).unwrap().trim().to_owned())
    }

    // -- create tests --

    #[test]
    fn test_create_workspace() {
        let (temp_dir, epoch) = setup_git_repo();
        let root = temp_dir.path().to_path_buf();
        let backend = GitWorktreeBackend::new(root.clone());
        let ws_name = WorkspaceId::new("test-ws").unwrap();

        let info = backend.create(&ws_name, &epoch).unwrap();
        assert_eq!(info.id, ws_name);
        assert_eq!(info.path, root.join("ws").join("test-ws"));
        assert!(info.path.exists());
        assert!(info.path.join(".git").exists());

        // Idempotency
        let info2 = backend.create(&ws_name, &epoch).unwrap();
        assert_eq!(info2.path, info.path);
    }

    #[test]
    fn test_create_cleanup_stale_directory() {
        let (temp_dir, epoch) = setup_git_repo();
        let root = temp_dir.path().to_path_buf();
        let backend = GitWorktreeBackend::new(root.clone());
        let ws_name = WorkspaceId::new("fail-ws").unwrap();

        let ws_path = root.join("ws").join("fail-ws");
        fs::create_dir_all(&ws_path).unwrap();
        fs::write(ws_path.join("garbage.txt"), "garbage").unwrap();

        let info = backend.create(&ws_name, &epoch).unwrap();
        assert!(info.path.exists());
        assert!(!ws_path.join("garbage.txt").exists());
    }

    // -- exists tests --

    #[test]
    fn test_exists_false_for_nonexistent() {
        let (temp_dir, _epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        assert!(!backend.exists(&WorkspaceId::new("nope").unwrap()));
    }

    #[test]
    fn test_exists_true_after_create() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_name = WorkspaceId::new("exists-ws").unwrap();

        backend.create(&ws_name, &epoch).unwrap();
        assert!(backend.exists(&ws_name));
    }

    // -- workspace_path tests --

    #[test]
    fn test_workspace_path() {
        let (temp_dir, _epoch) = setup_git_repo();
        let root = temp_dir.path().to_path_buf();
        let backend = GitWorktreeBackend::new(root.clone());
        let ws_name = WorkspaceId::new("path-test").unwrap();

        assert_eq!(backend.workspace_path(&ws_name), root.join("ws/path-test"));
    }

    // -- snapshot tests --

    #[test]
    fn test_snapshot_empty() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_name = WorkspaceId::new("snap-empty").unwrap();
        backend.create(&ws_name, &epoch).unwrap();

        let snap = backend.snapshot(&ws_name).unwrap();
        assert!(snap.is_empty(), "no changes expected: {snap:?}");
    }

    #[test]
    fn test_snapshot_added_file() {
        let (temp_dir, epoch) = setup_git_repo();
        let root = temp_dir.path().to_path_buf();
        let backend = GitWorktreeBackend::new(root.clone());
        let ws_name = WorkspaceId::new("snap-add").unwrap();
        let info = backend.create(&ws_name, &epoch).unwrap();

        // Add a new file (untracked)
        fs::write(info.path.join("newfile.txt"), "hello").unwrap();

        let snap = backend.snapshot(&ws_name).unwrap();
        assert_eq!(snap.added.len(), 1, "expected 1 added: {snap:?}");
        assert_eq!(snap.added[0], PathBuf::from("newfile.txt"));
        assert!(snap.modified.is_empty());
        assert!(snap.deleted.is_empty());
    }

    #[test]
    fn test_snapshot_modified_file() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_name = WorkspaceId::new("snap-mod").unwrap();
        let info = backend.create(&ws_name, &epoch).unwrap();

        // Modify existing tracked file
        fs::write(info.path.join("README.md"), "# Modified").unwrap();

        let snap = backend.snapshot(&ws_name).unwrap();
        assert!(snap.added.is_empty(), "no adds: {snap:?}");
        assert_eq!(snap.modified.len(), 1, "expected 1 modified: {snap:?}");
        assert_eq!(snap.modified[0], PathBuf::from("README.md"));
        assert!(snap.deleted.is_empty());
    }

    #[test]
    fn test_snapshot_deleted_file() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_name = WorkspaceId::new("snap-del").unwrap();
        let info = backend.create(&ws_name, &epoch).unwrap();

        // Delete tracked file
        fs::remove_file(info.path.join("README.md")).unwrap();

        let snap = backend.snapshot(&ws_name).unwrap();
        assert!(snap.added.is_empty());
        assert!(snap.modified.is_empty());
        assert_eq!(snap.deleted.len(), 1, "expected 1 deleted: {snap:?}");
        assert_eq!(snap.deleted[0], PathBuf::from("README.md"));
    }

    #[test]
    fn test_snapshot_mixed_changes() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_name = WorkspaceId::new("snap-mix").unwrap();
        let info = backend.create(&ws_name, &epoch).unwrap();

        // Add, modify, delete
        fs::write(info.path.join("new.rs"), "fn main() {}").unwrap();
        fs::write(info.path.join("README.md"), "# Changed").unwrap();
        // Can't delete and add in same snapshot cleanly without more files,
        // so just check add + modify
        let snap = backend.snapshot(&ws_name).unwrap();
        assert_eq!(snap.added.len(), 1);
        assert_eq!(snap.modified.len(), 1);
        assert_eq!(snap.change_count(), 2);
    }

    #[test]
    fn test_snapshot_ignores_gitignored() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_name = WorkspaceId::new("snap-ignore").unwrap();
        let info = backend.create(&ws_name, &epoch).unwrap();

        // Create .gitignore and an ignored file
        fs::write(info.path.join(".gitignore"), "*.log\n").unwrap();
        fs::write(info.path.join("debug.log"), "log data").unwrap();

        let snap = backend.snapshot(&ws_name).unwrap();
        // .gitignore itself should show up as added, but debug.log should not
        let has_log = snap.added.iter().any(|p| p.to_str() == Some("debug.log"));
        assert!(!has_log, "gitignored file should not appear: {snap:?}");
        let has_gitignore = snap.added.iter().any(|p| p.to_str() == Some(".gitignore"));
        assert!(has_gitignore, ".gitignore should appear: {snap:?}");
    }

    #[test]
    fn test_snapshot_nonexistent_workspace() {
        let (temp_dir, _epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_name = WorkspaceId::new("nope").unwrap();

        let err = backend.snapshot(&ws_name).unwrap_err();
        assert!(
            matches!(err, GitBackendError::NotFound { .. }),
            "should be NotFound: {err}"
        );
    }

    #[test]
    fn test_snapshot_materializes_workspace_state_ref() {
        let (temp_dir, epoch) = setup_git_repo();
        let root = temp_dir.path().to_path_buf();
        let backend = GitWorktreeBackend::new(root.clone());
        let ws_name = WorkspaceId::new("snap-ref").unwrap();
        let info = backend.create(&ws_name, &epoch).unwrap();

        fs::write(info.path.join("README.md"), "# changed from workspace").unwrap();
        let _snap = backend.snapshot(&ws_name).unwrap();

        let ref_oid = read_ws_ref(&root, ws_name.as_str()).expect("workspace ref should exist");
        let head_oid = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        let head_oid = String::from_utf8(head_oid.stdout)
            .unwrap()
            .trim()
            .to_owned();
        assert_ne!(
            ref_oid, head_oid,
            "dirty workspace should materialize non-HEAD commit"
        );

        let ref_name = manifold_refs::workspace_state_ref(ws_name.as_str());
        let diff_out = Command::new("git")
            .args(["diff", "--name-only", &format!("HEAD..{ref_name}")])
            .current_dir(&root)
            .output()
            .unwrap();
        let diff = String::from_utf8(diff_out.stdout).unwrap();
        assert!(
            diff.lines().any(|l| l.trim() == "README.md"),
            "diff should include README.md: {diff}"
        );
    }

    #[test]
    fn test_snapshot_skips_workspace_state_ref_when_disabled_in_config() {
        let (temp_dir, epoch) = setup_git_repo();
        let root = temp_dir.path().to_path_buf();
        std::fs::create_dir_all(root.join(".manifold")).unwrap();
        std::fs::write(
            root.join(".manifold").join("config.toml"),
            "[workspace]\ngit_compat_refs = false\n",
        )
        .unwrap();

        let backend = GitWorktreeBackend::new(root.clone());
        let ws_name = WorkspaceId::new("snap-no-ref").unwrap();
        let info = backend.create(&ws_name, &epoch).unwrap();

        fs::write(
            info.path.join("README.md"),
            "# changed with compat disabled",
        )
        .unwrap();
        let _snap = backend.snapshot(&ws_name).unwrap();

        assert!(
            read_ws_ref(&root, ws_name.as_str()).is_none(),
            "workspace ref should not be created when disabled"
        );
    }

    // -- destroy tests --

    #[test]
    fn test_destroy_workspace() {
        let (temp_dir, epoch) = setup_git_repo();
        let root = temp_dir.path().to_path_buf();
        let backend = GitWorktreeBackend::new(root.clone());
        let ws_name = WorkspaceId::new("destroy-ws").unwrap();

        // Create then destroy
        let info = backend.create(&ws_name, &epoch).unwrap();
        assert!(info.path.exists());

        // Materialize Level 1 ref, then ensure destroy prunes it.
        fs::write(info.path.join("README.md"), "# dirty before destroy").unwrap();
        let _ = backend.snapshot(&ws_name).unwrap();
        assert!(read_ws_ref(&root, ws_name.as_str()).is_some());

        backend.destroy(&ws_name).unwrap();
        assert!(!info.path.exists(), "directory should be gone");
        assert!(!backend.exists(&ws_name), "should not exist in git");
        assert!(
            read_ws_ref(&root, ws_name.as_str()).is_none(),
            "workspace ref should be pruned on destroy"
        );
    }

    #[test]
    fn test_destroy_idempotent() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_name = WorkspaceId::new("destroy-idem").unwrap();

        backend.create(&ws_name, &epoch).unwrap();

        // Destroy twice: both should succeed
        backend.destroy(&ws_name).unwrap();
        backend.destroy(&ws_name).unwrap();
    }

    #[test]
    fn test_destroy_never_existed() {
        let (temp_dir, _epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_name = WorkspaceId::new("no-such-ws").unwrap();

        // Destroying a workspace that never existed should succeed (idempotent)
        backend.destroy(&ws_name).unwrap();
    }

    #[test]
    fn test_destroy_with_dirty_files() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_name = WorkspaceId::new("dirty-destroy").unwrap();

        let info = backend.create(&ws_name, &epoch).unwrap();

        // Make dirty changes
        fs::write(info.path.join("dirty.txt"), "uncommitted").unwrap();
        fs::write(info.path.join("README.md"), "modified").unwrap();

        // Should still destroy successfully (--force handles dirty state)
        backend.destroy(&ws_name).unwrap();
        assert!(!info.path.exists());
        assert!(!backend.exists(&ws_name));
    }

    #[test]
    fn test_destroy_manual_dir_removal() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_name = WorkspaceId::new("manual-rm").unwrap();

        let info = backend.create(&ws_name, &epoch).unwrap();

        // Simulate out-of-band directory removal (e.g., crash during previous destroy)
        fs::remove_dir_all(&info.path).unwrap();
        assert!(!info.path.exists());

        // Destroy should still succeed and prune stale git worktree entry
        backend.destroy(&ws_name).unwrap();
        assert!(!backend.exists(&ws_name));
    }

    #[test]
    fn test_create_after_destroy() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_name = WorkspaceId::new("recreate-ws").unwrap();

        // Create, destroy, then create again
        backend.create(&ws_name, &epoch).unwrap();
        backend.destroy(&ws_name).unwrap();
        let info = backend.create(&ws_name, &epoch).unwrap();
        assert!(info.path.exists());
        assert!(backend.exists(&ws_name));
    }

    // -- parse_name_status tests --

    #[test]
    fn test_parse_name_status() {
        let mut added = Vec::new();
        let mut modified = Vec::new();
        let mut deleted = Vec::new();

        let output = "A\tsrc/new.rs\nM\tsrc/main.rs\nD\told.rs\n";
        parse_name_status(output, &mut added, &mut modified, &mut deleted);

        assert_eq!(added, vec![PathBuf::from("src/new.rs")]);
        assert_eq!(modified, vec![PathBuf::from("src/main.rs")]);
        assert_eq!(deleted, vec![PathBuf::from("old.rs")]);
    }

    #[test]
    fn test_parse_name_status_empty() {
        let mut added = Vec::new();
        let mut modified = Vec::new();
        let mut deleted = Vec::new();

        parse_name_status("", &mut added, &mut modified, &mut deleted);
        assert!(added.is_empty());
        assert!(modified.is_empty());
        assert!(deleted.is_empty());
    }

    // -- parse_porcelain_status tests --

    #[test]
    fn test_parse_porcelain_status_empty() {
        let paths = parse_porcelain_status("");
        assert!(paths.is_empty());
    }

    #[test]
    fn test_parse_porcelain_status_modified() {
        let output = " M src/main.rs\n";
        let paths = parse_porcelain_status(output);
        assert_eq!(paths, vec![PathBuf::from("src/main.rs")]);
    }

    #[test]
    fn test_parse_porcelain_status_staged() {
        let output = "M  src/lib.rs\n";
        let paths = parse_porcelain_status(output);
        assert_eq!(paths, vec![PathBuf::from("src/lib.rs")]);
    }

    #[test]
    fn test_parse_porcelain_status_untracked() {
        let output = "?? new_file.txt\n";
        let paths = parse_porcelain_status(output);
        assert_eq!(paths, vec![PathBuf::from("new_file.txt")]);
    }

    #[test]
    fn test_parse_porcelain_status_deleted() {
        let output = " D old_file.rs\n";
        let paths = parse_porcelain_status(output);
        assert_eq!(paths, vec![PathBuf::from("old_file.rs")]);
    }

    #[test]
    fn test_parse_porcelain_status_mixed() {
        let output = " M src/main.rs\n?? untracked.txt\n D gone.rs\n";
        let paths = parse_porcelain_status(output);
        assert_eq!(paths.len(), 3);
        assert!(paths.contains(&PathBuf::from("src/main.rs")));
        assert!(paths.contains(&PathBuf::from("untracked.txt")));
        assert!(paths.contains(&PathBuf::from("gone.rs")));
    }

    #[test]
    fn test_parse_porcelain_status_quoted_path() {
        // git quotes paths with special characters
        let output = "?? \"path with spaces.txt\"\n";
        let paths = parse_porcelain_status(output);
        assert_eq!(paths, vec![PathBuf::from("path with spaces.txt")]);
    }

    // -- list tests --

    #[test]
    fn test_list_empty_no_workspaces() {
        let (temp_dir, _epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());

        let infos = backend.list().unwrap();
        assert!(infos.is_empty(), "no workspaces under ws/ yet: {infos:?}");
    }

    #[test]
    fn test_list_single_workspace() {
        let (temp_dir, epoch) = setup_git_repo();
        let root = temp_dir.path().to_path_buf();
        let backend = GitWorktreeBackend::new(root.clone());
        let ws_name = WorkspaceId::new("list-ws").unwrap();

        backend.create(&ws_name, &epoch).unwrap();

        let infos = backend.list().unwrap();
        assert_eq!(infos.len(), 1, "expected 1 workspace: {infos:?}");
        assert_eq!(infos[0].id, ws_name);
        assert_eq!(infos[0].path, root.join("ws/list-ws"));
        assert_eq!(infos[0].epoch, epoch);
        assert!(infos[0].state.is_active(), "no epoch ref → active");
    }

    #[test]
    fn test_list_multiple_workspaces() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());

        let a = WorkspaceId::new("alpha").unwrap();
        let b = WorkspaceId::new("beta").unwrap();
        backend.create(&a, &epoch).unwrap();
        backend.create(&b, &epoch).unwrap();

        let mut infos = backend.list().unwrap();
        assert_eq!(infos.len(), 2, "expected 2 workspaces: {infos:?}");

        // Sort by name for stable comparison
        infos.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        assert_eq!(infos[0].id.as_str(), "alpha");
        assert_eq!(infos[1].id.as_str(), "beta");
    }

    #[test]
    fn test_list_excludes_repo_root() {
        // The main git worktree (the repo root) should never appear in list().
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_name = WorkspaceId::new("my-ws").unwrap();
        backend.create(&ws_name, &epoch).unwrap();

        let infos = backend.list().unwrap();
        for info in &infos {
            assert_ne!(
                info.path,
                temp_dir.path(),
                "repo root should not appear in list"
            );
        }
    }

    #[test]
    fn test_list_excludes_destroyed_workspace() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());

        let ws_name = WorkspaceId::new("gone-ws").unwrap();
        backend.create(&ws_name, &epoch).unwrap();
        backend.destroy(&ws_name).unwrap();

        let infos = backend.list().unwrap();
        assert!(
            infos.is_empty(),
            "destroyed workspace should not appear: {infos:?}"
        );
    }

    #[test]
    fn test_list_active_when_epoch_matches() {
        let (temp_dir, epoch) = setup_git_repo();
        let root = temp_dir.path().to_path_buf();
        let backend = GitWorktreeBackend::new(root.clone());
        let ws_name = WorkspaceId::new("current-ws").unwrap();
        backend.create(&ws_name, &epoch).unwrap();

        // Set refs/manifold/epoch/current to the same epoch as the workspace
        Command::new("git")
            .args(["update-ref", "refs/manifold/epoch/current", epoch.as_str()])
            .current_dir(&root)
            .output()
            .unwrap();

        let infos = backend.list().unwrap();
        assert_eq!(infos.len(), 1);
        assert!(
            infos[0].state.is_active(),
            "workspace at current epoch should be active: {:?}",
            infos[0].state
        );
    }

    #[test]
    fn test_list_stale_when_epoch_advanced() {
        let (temp_dir, epoch0) = setup_git_repo();
        let root = temp_dir.path().to_path_buf();
        let backend = GitWorktreeBackend::new(root.clone());
        let ws_name = WorkspaceId::new("stale-ws").unwrap();
        backend.create(&ws_name, &epoch0).unwrap();

        // Advance the epoch: make a new commit on the main branch
        let new_file = root.join("advance.md");
        fs::write(&new_file, "epoch 1").unwrap();
        Command::new("git")
            .args(["add", "advance.md"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Advance epoch"])
            .current_dir(&root)
            .output()
            .unwrap();
        let head_out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        let epoch1_str = String::from_utf8(head_out.stdout)
            .unwrap()
            .trim()
            .to_string();

        // Update epoch ref to epoch1
        Command::new("git")
            .args(["update-ref", "refs/manifold/epoch/current", &epoch1_str])
            .current_dir(&root)
            .output()
            .unwrap();

        let infos = backend.list().unwrap();
        assert_eq!(infos.len(), 1);
        assert!(
            infos[0].state.is_stale(),
            "workspace at old epoch should be stale: {:?}",
            infos[0].state
        );
        // Should be 1 epoch behind (one commit separates them)
        if let WorkspaceState::Stale { behind_epochs } = infos[0].state {
            assert_eq!(behind_epochs, 1, "should be 1 epoch behind");
        }
    }

    // -- status tests --

    #[test]
    fn test_status_nonexistent_workspace() {
        let (temp_dir, _epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_name = WorkspaceId::new("no-such").unwrap();

        let err = backend.status(&ws_name).unwrap_err();
        assert!(
            matches!(err, GitBackendError::NotFound { .. }),
            "expected NotFound: {err}"
        );
    }

    #[test]
    fn test_status_clean_workspace() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_name = WorkspaceId::new("clean-ws").unwrap();
        backend.create(&ws_name, &epoch).unwrap();

        let status = backend.status(&ws_name).unwrap();
        assert_eq!(
            status.base_epoch, epoch,
            "base epoch should match creation epoch"
        );
        assert!(
            status.is_clean(),
            "no changes expected: {:?}",
            status.dirty_files
        );
        assert!(!status.is_stale, "no epoch ref yet → not stale");
    }

    #[test]
    fn test_status_modified_file() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_name = WorkspaceId::new("mod-ws").unwrap();
        let info = backend.create(&ws_name, &epoch).unwrap();

        fs::write(info.path.join("README.md"), "# Changed").unwrap();

        let status = backend.status(&ws_name).unwrap();
        assert_eq!(
            status.dirty_count(),
            1,
            "expected 1 dirty file: {:?}",
            status.dirty_files
        );
        assert!(
            status
                .dirty_files
                .iter()
                .any(|p| p == &PathBuf::from("README.md")),
            "README.md should be dirty: {:?}",
            status.dirty_files
        );
    }

    #[test]
    fn test_status_untracked_file() {
        let (temp_dir, epoch) = setup_git_repo();
        let backend = GitWorktreeBackend::new(temp_dir.path().to_path_buf());
        let ws_name = WorkspaceId::new("untracked-ws").unwrap();
        let info = backend.create(&ws_name, &epoch).unwrap();

        fs::write(info.path.join("new_file.txt"), "new").unwrap();

        let status = backend.status(&ws_name).unwrap();
        assert_eq!(status.dirty_count(), 1);
        assert!(
            status
                .dirty_files
                .iter()
                .any(|p| p == &PathBuf::from("new_file.txt")),
            "new_file.txt should be dirty: {:?}",
            status.dirty_files
        );
    }

    #[test]
    fn test_status_not_stale_when_epoch_matches() {
        let (temp_dir, epoch) = setup_git_repo();
        let root = temp_dir.path().to_path_buf();
        let backend = GitWorktreeBackend::new(root.clone());
        let ws_name = WorkspaceId::new("not-stale").unwrap();
        backend.create(&ws_name, &epoch).unwrap();

        // Set epoch ref to the workspace's epoch
        Command::new("git")
            .args(["update-ref", "refs/manifold/epoch/current", epoch.as_str()])
            .current_dir(&root)
            .output()
            .unwrap();

        let status = backend.status(&ws_name).unwrap();
        assert!(
            !status.is_stale,
            "workspace should not be stale when epoch matches"
        );
    }

    #[test]
    fn test_status_stale_when_epoch_advanced() {
        let (temp_dir, epoch0) = setup_git_repo();
        let root = temp_dir.path().to_path_buf();
        let backend = GitWorktreeBackend::new(root.clone());
        let ws_name = WorkspaceId::new("stale-status").unwrap();
        backend.create(&ws_name, &epoch0).unwrap();

        // Advance the epoch
        fs::write(root.join("advance.md"), "epoch 1").unwrap();
        Command::new("git")
            .args(["add", "advance.md"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Advance"])
            .current_dir(&root)
            .output()
            .unwrap();
        let head_out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        let epoch1_str = String::from_utf8(head_out.stdout)
            .unwrap()
            .trim()
            .to_string();

        Command::new("git")
            .args(["update-ref", "refs/manifold/epoch/current", &epoch1_str])
            .current_dir(&root)
            .output()
            .unwrap();

        let status = backend.status(&ws_name).unwrap();
        assert!(
            status.is_stale,
            "workspace should be stale after epoch advance"
        );
        assert_eq!(status.base_epoch, epoch0, "base epoch unchanged");
    }

    // -- parse_worktree_porcelain tests --

    #[test]
    fn test_parse_worktree_porcelain_single() {
        let raw = "worktree /tmp/repo\nHEAD aabbccdd00112233aabbccdd00112233aabbccdd\nbranch refs/heads/main\n\n";
        let entries = parse_worktree_porcelain(raw);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "/tmp/repo");
        assert_eq!(
            entries[0].head.as_deref(),
            Some("aabbccdd00112233aabbccdd00112233aabbccdd")
        );
        assert_eq!(entries[0].branch.as_deref(), Some("refs/heads/main"));
    }

    #[test]
    fn test_parse_worktree_porcelain_multiple() {
        let raw = "worktree /repo\nHEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nbranch refs/heads/main\n\nworktree /repo/ws/agent-1\nHEAD bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\ndetached\n\n";
        let entries = parse_worktree_porcelain(raw);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "/repo");
        assert_eq!(entries[1].path, "/repo/ws/agent-1");
        assert!(
            entries[1].branch.is_none(),
            "detached worktree should have no branch"
        );
    }
    // -- error display tests --

    #[test]
    fn test_error_display() {
        let err = GitBackendError::GitCommand {
            command: "git worktree add".to_owned(),
            stderr: "fatal: bad ref".to_owned(),
            exit_code: Some(128),
        };
        let msg = format!("{err}");
        assert!(msg.contains("git worktree add"));
        assert!(msg.contains("128"));
        assert!(msg.contains("fatal: bad ref"));

        let err = GitBackendError::NotFound {
            name: "missing".to_owned(),
        };
        assert!(format!("{err}").contains("missing"));

        let err = GitBackendError::NotImplemented("destroy");
        assert!(format!("{err}").contains("destroy"));
    }
}
