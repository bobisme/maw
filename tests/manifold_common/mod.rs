//! Manifold v2 test infrastructure — fully git-native.
#![allow(dead_code)]
//!
//! Provides [`TestRepo`], a self-contained Manifold repository in a temporary
//! directory for integration tests. Each `TestRepo` gets a unique `/tmp` dir,
//! runs real git commands, and cleans up on drop.
//!
//! # Design principles
//!
//! - **Git-native**: Uses `git worktree` directly, matching Manifold v2.
//! - **Parallel-safe**: Each `TestRepo` lives in its own `TempDir`.
//! - **Drop-safe**: Temp dirs are deleted when `TestRepo` goes out of scope.
//! - **Ergonomic**: Helpers like `add_file`, `modify_file`, `delete_file` operate
//!   on workspace names — no need to compute paths manually.
//!
//! # Example
//!
//! ```rust,no_run
//! use manifold_common::TestRepo;
//!
//! let repo = TestRepo::new();
//! repo.create_workspace("alice");
//! repo.add_file("alice", "hello.txt", "world");
//! // ... run maw commands against repo.root() ...
//! // Temp dir is cleaned up when `repo` drops.
//! ```

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

// ---------------------------------------------------------------------------
// TestRepo
// ---------------------------------------------------------------------------

/// A self-contained Manifold v2 repository in a temporary directory.
///
/// Creates a git-native Manifold repo (bare root + `ws/default/` worktree)
/// with no legacy VCS dependency. Each instance gets a unique temp dir.
///
/// Implements `Drop` to clean up the temp dir.
pub struct TestRepo {
    /// The temp dir — held to prevent premature cleanup.
    _dir: TempDir,
    /// Absolute path to the repo root (same as `_dir.path()`).
    root: PathBuf,
    /// The epoch₀ commit OID.
    epoch0: String,
}

impl TestRepo {
    /// Create a new Manifold v2 test repo.
    ///
    /// Performs greenfield initialization:
    /// 1. `git init` in a fresh temp dir
    /// 2. Creates an initial empty commit (epoch₀) on `main`
    /// 3. Sets `core.bare = true`
    /// 4. Creates `ws/default/` worktree
    /// 5. Sets `refs/manifold/epoch/current`
    ///
    /// # Panics
    /// Panics if any git command fails.
    #[must_use]
    pub fn new() -> Self {
        let dir = TempDir::new().expect("failed to create temp dir");
        let root = dir.path().to_path_buf();

        // 1. git init
        git_ok(&root, &["init"]);

        // Configure identity for commits
        git_ok(&root, &["config", "user.name", "Test"]);
        git_ok(&root, &["config", "user.email", "test@localhost"]);

        // Disable signing
        git_ok(&root, &["config", "commit.gpgsign", "false"]);
        git_ok(&root, &["config", "tag.gpgsign", "false"]);

        // Ensure we're on `main`
        git_ok(&root, &["checkout", "-B", "main"]);

        // 2. Create epoch₀ (initial empty commit)
        git_ok(
            &root,
            &[
                "commit",
                "--allow-empty",
                "-m",
                "manifold: epoch₀ (initial empty commit)",
            ],
        );

        let epoch0 = git_ok(&root, &["rev-parse", "HEAD"]).trim().to_owned();

        // 3. Set core.bare = true
        git_ok(&root, &["config", "core.bare", "true"]);

        // Remove root index (not needed in bare mode)
        let index = root.join(".git").join("index");
        if index.exists() {
            std::fs::remove_file(&index).expect("failed to remove index");
        }

        // 4. Create .manifold/ structure
        let manifold_dir = root.join(".manifold");
        std::fs::create_dir_all(manifold_dir.join("epochs"))
            .expect("failed to create .manifold/epochs");
        std::fs::create_dir_all(manifold_dir.join("artifacts").join("ws"))
            .expect("failed to create .manifold/artifacts/ws");
        std::fs::create_dir_all(manifold_dir.join("artifacts").join("merge"))
            .expect("failed to create .manifold/artifacts/merge");

        // Write default config
        std::fs::write(
            manifold_dir.join("config.toml"),
            "[repo]\nbranch = \"main\"\n",
        )
        .expect("failed to write config.toml");

        // 5. Set refs/manifold/epoch/current → epoch₀
        git_ok(
            &root,
            &["update-ref", "refs/manifold/epoch/current", &epoch0],
        );

        // 6. Create ws/default/ worktree
        let ws_dir = root.join("ws");
        std::fs::create_dir_all(&ws_dir).expect("failed to create ws/");

        let ws_default = ws_dir.join("default");
        git_ok(
            &root,
            &[
                "worktree",
                "add",
                "--detach",
                ws_default.to_str().unwrap(),
                &epoch0,
            ],
        );

        // Write .gitignore in the default workspace
        let gitignore_content = "# Manifold workspaces\nws/\n\n# Manifold ephemeral data\n.manifold/epochs/\n.manifold/cow/\n.manifold/artifacts/\n";
        std::fs::write(ws_default.join(".gitignore"), gitignore_content)
            .expect("failed to write .gitignore");

        Self {
            _dir: dir,
            root,
            epoch0,
        }
    }

    /// Create a new Manifold v2 test repo with a bare git remote.
    ///
    /// Returns `(repo, remote)`. The remote is configured as `origin` in the repo.
    /// Useful for testing push/fetch operations.
    ///
    /// # Panics
    /// Panics if any git command fails.
    #[must_use]
    pub fn with_remote() -> (Self, TempDir) {
        // Create bare remote
        let remote_dir = TempDir::new().expect("failed to create remote temp dir");
        git_ok(remote_dir.path(), &["init", "--bare"]);

        // Create repo by cloning remote, then set up Manifold
        let dir = TempDir::new().expect("failed to create temp dir");
        let root = dir.path().to_path_buf();

        // Clone the bare remote
        git_ok_in(
            &std::env::temp_dir(),
            &[
                "clone",
                remote_dir.path().to_str().unwrap(),
                root.to_str().unwrap(),
            ],
        );

        // Configure identity
        git_ok(&root, &["config", "user.name", "Test"]);
        git_ok(&root, &["config", "user.email", "test@localhost"]);
        git_ok(&root, &["config", "commit.gpgsign", "false"]);
        git_ok(&root, &["config", "tag.gpgsign", "false"]);

        // Ensure on main
        git_ok(&root, &["checkout", "-B", "main"]);

        // Create epoch₀
        git_ok(
            &root,
            &[
                "commit",
                "--allow-empty",
                "-m",
                "manifold: epoch₀ (initial empty commit)",
            ],
        );

        let epoch0 = git_ok(&root, &["rev-parse", "HEAD"]).trim().to_owned();

        // Push to remote so main exists there
        git_ok(&root, &["push", "-u", "origin", "main"]);

        // Set bare mode
        git_ok(&root, &["config", "core.bare", "true"]);

        let index = root.join(".git").join("index");
        if index.exists() {
            std::fs::remove_file(&index).expect("failed to remove index");
        }

        // Create .manifold/
        let manifold_dir = root.join(".manifold");
        std::fs::create_dir_all(manifold_dir.join("epochs")).unwrap();
        std::fs::create_dir_all(manifold_dir.join("artifacts").join("ws")).unwrap();
        std::fs::create_dir_all(manifold_dir.join("artifacts").join("merge")).unwrap();
        std::fs::write(
            manifold_dir.join("config.toml"),
            "[repo]\nbranch = \"main\"\n",
        )
        .unwrap();

        // Set epoch ref
        git_ok(
            &root,
            &["update-ref", "refs/manifold/epoch/current", &epoch0],
        );

        // Create ws/default/
        let ws_dir = root.join("ws");
        std::fs::create_dir_all(&ws_dir).unwrap();
        let ws_default = ws_dir.join("default");
        git_ok(
            &root,
            &[
                "worktree",
                "add",
                "--detach",
                ws_default.to_str().unwrap(),
                &epoch0,
            ],
        );

        let gitignore_content = "# Manifold workspaces\nws/\n\n# Manifold ephemeral data\n.manifold/epochs/\n.manifold/cow/\n.manifold/artifacts/\n";
        std::fs::write(ws_default.join(".gitignore"), gitignore_content).unwrap();

        let repo = Self {
            _dir: dir,
            root,
            epoch0,
        };
        (repo, remote_dir)
    }

    // -----------------------------------------------------------------------
    // Accessors
    // -----------------------------------------------------------------------

    /// Absolute path to the repo root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The epoch₀ commit OID (40-char hex).
    #[must_use]
    pub fn epoch0(&self) -> &str {
        &self.epoch0
    }

    /// Absolute path to a workspace: `<root>/ws/<name>/`.
    #[must_use]
    pub fn workspace_path(&self, name: &str) -> PathBuf {
        self.root.join("ws").join(name)
    }

    /// Absolute path to the default workspace: `<root>/ws/default/`.
    #[must_use]
    pub fn default_workspace(&self) -> PathBuf {
        self.workspace_path("default")
    }

    /// Read the current epoch OID from `refs/manifold/epoch/current`.
    #[must_use]
    pub fn current_epoch(&self) -> String {
        git_ok(&self.root, &["rev-parse", "refs/manifold/epoch/current"])
            .trim()
            .to_owned()
    }

    // -----------------------------------------------------------------------
    // Workspace operations
    // -----------------------------------------------------------------------

    /// Create a new workspace (git worktree) at the current epoch.
    ///
    /// The workspace is created at `<root>/ws/<name>/`, detached at the
    /// current epoch commit.
    ///
    /// # Panics
    /// Panics if the workspace already exists or git worktree add fails.
    pub fn create_workspace(&self, name: &str) -> PathBuf {
        let ws_path = self.workspace_path(name);
        let epoch = self.current_epoch();

        git_ok(
            &self.root,
            &[
                "worktree",
                "add",
                "--detach",
                ws_path.to_str().unwrap(),
                &epoch,
            ],
        );

        ws_path
    }

    /// Destroy a workspace (remove its git worktree).
    ///
    /// Idempotent — does nothing if the workspace doesn't exist.
    pub fn destroy_workspace(&self, name: &str) {
        let ws_path = self.workspace_path(name);
        if ws_path.exists() {
            let _ = Command::new("git")
                .args(["worktree", "remove", "--force", ws_path.to_str().unwrap()])
                .current_dir(&self.root)
                .output();
        }
        // Prune stale entries
        let _ = Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(&self.root)
            .output();
    }

    /// List workspace names (excluding the bare repo entry).
    ///
    /// Parses `git worktree list --porcelain` and returns names of worktrees
    /// under `ws/`.
    #[must_use]
    pub fn list_workspaces(&self) -> Vec<String> {
        let output = git_ok(&self.root, &["worktree", "list", "--porcelain"]);
        let ws_dir = self.root.join("ws");

        let mut names = Vec::new();
        for block in output.split("\n\n") {
            let block = block.trim();
            if block.is_empty() {
                continue;
            }

            let mut wt_path: Option<PathBuf> = None;
            let mut is_bare = false;

            for line in block.lines() {
                if let Some(p) = line.strip_prefix("worktree ") {
                    wt_path = Some(PathBuf::from(p));
                } else if line.trim() == "bare" {
                    is_bare = true;
                }
            }

            if is_bare {
                continue;
            }

            if let Some(path) = wt_path
                && let Ok(rel) = path.strip_prefix(&ws_dir) {
                    let components: Vec<_> = rel.components().collect();
                    if components.len() == 1
                        && let Some(name) = components[0].as_os_str().to_str() {
                            names.push(name.to_owned());
                        }
                }
        }

        names.sort();
        names
    }

    /// Check if a workspace exists.
    #[must_use]
    pub fn workspace_exists(&self, name: &str) -> bool {
        let ws_path = self.workspace_path(name);
        ws_path.exists() && ws_path.join(".git").exists()
    }

    // -----------------------------------------------------------------------
    // File operations (in workspaces)
    // -----------------------------------------------------------------------

    /// Add a new file to a workspace.
    ///
    /// Creates parent directories as needed. The file content is written
    /// directly to the workspace's working tree.
    ///
    /// # Panics
    /// Panics if the workspace doesn't exist or the write fails.
    pub fn add_file(&self, workspace: &str, rel_path: &str, content: &str) {
        let ws_path = self.workspace_path(workspace);
        assert!(
            ws_path.exists(),
            "workspace '{}' does not exist at {}",
            workspace,
            ws_path.display()
        );

        let file_path = ws_path.join(rel_path);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent).unwrap_or_else(|e| {
                panic!("failed to create dirs for {}: {e}", file_path.display())
            });
        }
        std::fs::write(&file_path, content)
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", file_path.display()));
    }

    /// Modify an existing file in a workspace.
    ///
    /// Overwrites the file content. Functionally identical to `add_file`
    /// but semantically indicates the file should already exist.
    ///
    /// # Panics
    /// Panics if the file doesn't exist, the workspace doesn't exist, or the write fails.
    pub fn modify_file(&self, workspace: &str, rel_path: &str, content: &str) {
        let ws_path = self.workspace_path(workspace);
        let file_path = ws_path.join(rel_path);
        assert!(
            file_path.exists(),
            "file '{rel_path}' does not exist in workspace '{workspace}' — use add_file for new files"
        );
        std::fs::write(&file_path, content)
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", file_path.display()));
    }

    /// Delete a file from a workspace.
    ///
    /// # Panics
    /// Panics if the file doesn't exist or the delete fails.
    pub fn delete_file(&self, workspace: &str, rel_path: &str) {
        let ws_path = self.workspace_path(workspace);
        let file_path = ws_path.join(rel_path);
        assert!(
            file_path.exists(),
            "file '{rel_path}' does not exist in workspace '{workspace}' — nothing to delete"
        );
        std::fs::remove_file(&file_path)
            .unwrap_or_else(|e| panic!("failed to delete {}: {e}", file_path.display()));
    }

    /// Read a file from a workspace. Returns `None` if the file doesn't exist.
    #[must_use]
    pub fn read_file(&self, workspace: &str, rel_path: &str) -> Option<String> {
        let file_path = self.workspace_path(workspace).join(rel_path);
        std::fs::read_to_string(&file_path).ok()
    }

    /// Check if a file exists in a workspace.
    #[must_use]
    pub fn file_exists(&self, workspace: &str, rel_path: &str) -> bool {
        self.workspace_path(workspace).join(rel_path).exists()
    }

    // -----------------------------------------------------------------------
    // Git operations (in workspaces)
    // -----------------------------------------------------------------------

    /// Run `git status --porcelain` in a workspace and return dirty file paths.
    #[must_use]
    pub fn dirty_files(&self, workspace: &str) -> Vec<PathBuf> {
        let ws_path = self.workspace_path(workspace);
        let output = git_ok(&ws_path, &["status", "--porcelain"]);
        parse_porcelain_paths(&output)
    }

    /// Get the HEAD OID of a workspace.
    #[must_use]
    pub fn workspace_head(&self, workspace: &str) -> String {
        let ws_path = self.workspace_path(workspace);
        git_ok(&ws_path, &["rev-parse", "HEAD"]).trim().to_owned()
    }

    /// Run `git diff --name-status <epoch>` in a workspace to see changes
    /// relative to the epoch base.
    #[must_use]
    pub fn diff_vs_epoch(&self, workspace: &str) -> Vec<(String, PathBuf)> {
        let ws_path = self.workspace_path(workspace);
        let epoch = self.current_epoch();
        let output = git_ok(&ws_path, &["diff", "--name-status", &epoch]);

        let mut changes = Vec::new();
        for line in output.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.splitn(2, '\t').collect();
            if parts.len() == 2 {
                changes.push((parts[0].to_owned(), PathBuf::from(parts[1])));
            }
        }
        changes
    }

    // -----------------------------------------------------------------------
    // Epoch operations
    // -----------------------------------------------------------------------

    /// Advance the epoch: commit changes from the default workspace and
    /// update `refs/manifold/epoch/current` AND `refs/heads/main`.
    ///
    /// This simulates a merged result being promoted to a new epoch:
    /// 1. Stages all changes in `ws/default/`
    /// 2. Creates a commit (in detached HEAD mode)
    /// 3. Updates `refs/manifold/epoch/current` to the new commit
    /// 4. Updates `refs/heads/main` to the new commit
    ///
    /// Both refs are kept in sync because the maw merge COMMIT phase uses a
    /// CAS on `refs/heads/main` from `epoch_before` → `epoch_candidate`,
    /// where `epoch_before` is the current epoch. If `main` drifts from the
    /// epoch ref, the CAS fails and the merge cannot complete.
    ///
    /// Returns the new epoch OID.
    ///
    /// # Panics
    /// Panics if there are no changes to commit or git commands fail.
    pub fn advance_epoch(&self, message: &str) -> String {
        let ws_default = self.default_workspace();

        // Stage all changes
        git_ok(&ws_default, &["add", "-A"]);

        // Commit (in detached HEAD — does not update any branch ref)
        git_ok(&ws_default, &["commit", "-m", message]);

        // Read the new commit OID
        let new_oid = git_ok(&ws_default, &["rev-parse", "HEAD"])
            .trim()
            .to_owned();

        // Update epoch ref (Manifold's canonical epoch pointer)
        git_ok(
            &self.root,
            &["update-ref", "refs/manifold/epoch/current", &new_oid],
        );

        // Keep refs/heads/main in sync with the epoch ref.
        // The merge COMMIT phase CAS uses: main (epoch_before) → candidate.
        // Without this, the CAS fails because main lags behind the epoch.
        git_ok(&self.root, &["update-ref", "refs/heads/main", &new_oid]);

        new_oid
    }

    /// Seed the default workspace with some initial files and advance the epoch.
    ///
    /// This is useful for tests that need files to exist at the epoch base
    /// so they can be modified/deleted in agent workspaces.
    ///
    /// Returns the new epoch OID.
    pub fn seed_files(&self, files: &[(&str, &str)]) -> String {
        for (path, content) in files {
            self.add_file("default", path, content);
        }
        self.advance_epoch("chore: seed initial files")
    }

    // -----------------------------------------------------------------------
    // maw CLI helpers
    // -----------------------------------------------------------------------

    /// Run the `maw` binary with arguments, using the repo root as cwd.
    ///
    /// Returns the raw `Output`.
    pub fn maw_raw(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_maw"))
            .args(args)
            .current_dir(&self.root)
            .output()
            .expect("failed to execute maw")
    }

    /// Run `maw` and assert it succeeds. Returns stdout as a string.
    ///
    /// # Panics
    /// Panics with stdout + stderr if the command fails.
    pub fn maw_ok(&self, args: &[&str]) -> String {
        let out = self.maw_raw(args);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "maw {} failed:\nstdout: {stdout}\nstderr: {stderr}",
            args.join(" "),
        );
        stdout.to_string()
    }

    /// Run `maw` and assert it fails. Returns stderr as a string.
    ///
    /// # Panics
    /// Panics if the command succeeds.
    pub fn maw_fails(&self, args: &[&str]) -> String {
        let out = self.maw_raw(args);
        assert!(
            !out.status.success(),
            "Expected maw {} to fail, but it succeeded.\nstdout: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
        );
        String::from_utf8_lossy(&out.stderr).to_string()
    }

    // -----------------------------------------------------------------------
    // Git command helpers
    // -----------------------------------------------------------------------

    /// Run a git command in the repo root. Panics on failure.
    pub fn git(&self, args: &[&str]) -> String {
        git_ok(&self.root, args)
    }

    /// Run a git command in a specific workspace. Panics on failure.
    pub fn git_in_workspace(&self, workspace: &str, args: &[&str]) -> String {
        let ws_path = self.workspace_path(workspace);
        git_ok(&ws_path, args)
    }
}

// ---------------------------------------------------------------------------
// Free-standing git helpers
// ---------------------------------------------------------------------------

/// Run a git command in the given directory and return stdout.
///
/// # Panics
/// Panics with stderr if the command fails.
pub fn git_ok(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run git {}: {e}", args.join(" ")));

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "git {} failed in {}:\nstdout: {stdout}\nstderr: {stderr}",
        args.join(" "),
        dir.display(),
    );
    stdout.to_string()
}

/// Run a git command in the given directory (separate from `git_ok` to avoid
/// confusion with `TestRepo::git`). Used for operations outside the repo root.
fn git_ok_in(dir: &Path, args: &[&str]) -> String {
    git_ok(dir, args)
}

/// Run a git command, returning the `Output` without asserting success.
#[allow(dead_code)]
pub fn git_raw(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run git {}: {e}", args.join(" ")))
}

/// Parse `git status --porcelain` output into file paths.
fn parse_porcelain_paths(output: &str) -> Vec<PathBuf> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            // Porcelain format: XY <path>  (2 status chars + space + path)
            if line.len() > 3 {
                Some(PathBuf::from(&line[3..]))
            } else {
                None
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests for the test infrastructure itself
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repo_creates_valid_structure() {
        let repo = TestRepo::new();

        // Root should exist
        assert!(repo.root().exists());

        // .git/ should exist
        assert!(repo.root().join(".git").is_dir());

        // .manifold/ structure
        assert!(repo.root().join(".manifold").is_dir());
        assert!(repo.root().join(".manifold").join("epochs").is_dir());
        assert!(repo.root().join(".manifold").join("config.toml").is_file());

        // ws/default/ should exist and be a valid worktree
        assert!(repo.default_workspace().is_dir());
        assert!(repo.default_workspace().join(".git").exists());
    }

    #[test]
    fn test_repo_epoch0_is_valid_oid() {
        let repo = TestRepo::new();
        let oid = repo.epoch0();
        assert_eq!(oid.len(), 40, "epoch0 should be 40-char hex OID");
        assert!(
            oid.chars().all(|c| c.is_ascii_hexdigit()),
            "epoch0 should be hex: {oid}"
        );
    }

    #[test]
    fn test_repo_current_epoch_matches_epoch0() {
        let repo = TestRepo::new();
        assert_eq!(
            repo.current_epoch(),
            repo.epoch0(),
            "current epoch should equal epoch0 initially"
        );
    }

    #[test]
    fn test_repo_unique_dirs() {
        let repo1 = TestRepo::new();
        let repo2 = TestRepo::new();
        assert_ne!(
            repo1.root(),
            repo2.root(),
            "each TestRepo should have a unique root"
        );
    }

    #[test]
    fn test_create_workspace() {
        let repo = TestRepo::new();
        let ws_path = repo.create_workspace("alice");

        assert!(ws_path.is_dir());
        assert!(ws_path.join(".git").exists());
        assert!(repo.workspace_exists("alice"));
    }

    #[test]
    fn test_destroy_workspace() {
        let repo = TestRepo::new();
        repo.create_workspace("bob");
        assert!(repo.workspace_exists("bob"));

        repo.destroy_workspace("bob");
        assert!(!repo.workspace_exists("bob"));
    }

    #[test]
    fn test_destroy_nonexistent_workspace_is_noop() {
        let repo = TestRepo::new();
        // Should not panic
        repo.destroy_workspace("nonexistent");
    }

    #[test]
    fn test_list_workspaces() {
        let repo = TestRepo::new();
        let initial = repo.list_workspaces();
        assert!(
            initial.contains(&"default".to_owned()),
            "should list default workspace"
        );

        repo.create_workspace("alice");
        repo.create_workspace("bob");

        let names = repo.list_workspaces();
        assert!(names.contains(&"default".to_owned()));
        assert!(names.contains(&"alice".to_owned()));
        assert!(names.contains(&"bob".to_owned()));
    }

    #[test]
    fn test_add_file() {
        let repo = TestRepo::new();
        repo.create_workspace("agent");
        repo.add_file("agent", "hello.txt", "world");

        let content = std::fs::read_to_string(repo.workspace_path("agent").join("hello.txt"))
            .expect("file should exist");
        assert_eq!(content, "world");
    }

    #[test]
    fn test_add_file_nested() {
        let repo = TestRepo::new();
        repo.create_workspace("agent");
        repo.add_file("agent", "src/main.rs", "fn main() {}");

        assert!(
            repo.workspace_path("agent")
                .join("src")
                .join("main.rs")
                .is_file()
        );
    }

    #[test]
    fn test_modify_file() {
        let repo = TestRepo::new();
        repo.create_workspace("agent");
        repo.add_file("agent", "data.txt", "v1");
        repo.modify_file("agent", "data.txt", "v2");

        let content = repo.read_file("agent", "data.txt").unwrap();
        assert_eq!(content, "v2");
    }

    #[test]
    #[should_panic(expected = "does not exist")]
    fn test_modify_nonexistent_panics() {
        let repo = TestRepo::new();
        repo.create_workspace("agent");
        repo.modify_file("agent", "nope.txt", "data");
    }

    #[test]
    fn test_delete_file() {
        let repo = TestRepo::new();
        repo.create_workspace("agent");
        repo.add_file("agent", "temp.txt", "gone soon");
        assert!(repo.file_exists("agent", "temp.txt"));

        repo.delete_file("agent", "temp.txt");
        assert!(!repo.file_exists("agent", "temp.txt"));
    }

    #[test]
    #[should_panic(expected = "does not exist")]
    fn test_delete_nonexistent_panics() {
        let repo = TestRepo::new();
        repo.create_workspace("agent");
        repo.delete_file("agent", "nope.txt");
    }

    #[test]
    fn test_read_file() {
        let repo = TestRepo::new();
        repo.create_workspace("agent");
        repo.add_file("agent", "readme.md", "# Hello");

        assert_eq!(
            repo.read_file("agent", "readme.md"),
            Some("# Hello".to_owned())
        );
        assert_eq!(repo.read_file("agent", "missing.txt"), None);
    }

    #[test]
    fn test_file_exists() {
        let repo = TestRepo::new();
        repo.create_workspace("agent");
        assert!(!repo.file_exists("agent", "new.txt"));

        repo.add_file("agent", "new.txt", "content");
        assert!(repo.file_exists("agent", "new.txt"));
    }

    #[test]
    fn test_dirty_files() {
        let repo = TestRepo::new();
        repo.create_workspace("agent");

        // Initially no dirty files (besides .gitignore from epoch)
        let dirty = repo.dirty_files("agent");
        assert!(
            dirty.is_empty(),
            "fresh workspace should have no dirty files, got: {dirty:?}"
        );

        // Add a file — should show as dirty
        repo.add_file("agent", "new.txt", "content");
        let dirty = repo.dirty_files("agent");
        assert!(
            dirty.iter().any(|p| p.to_str() == Some("new.txt")),
            "new.txt should be dirty, got: {dirty:?}"
        );
    }

    #[test]
    fn test_workspace_head_matches_epoch() {
        let repo = TestRepo::new();
        repo.create_workspace("agent");

        assert_eq!(
            repo.workspace_head("agent"),
            repo.current_epoch(),
            "new workspace HEAD should match current epoch"
        );
    }

    #[test]
    fn test_seed_files_and_advance_epoch() {
        let repo = TestRepo::new();
        let old_epoch = repo.current_epoch();

        let new_epoch = repo.seed_files(&[
            ("README.md", "# Test Project"),
            ("src/lib.rs", "pub fn hello() {}"),
        ]);

        assert_ne!(old_epoch, new_epoch, "epoch should advance");
        assert_eq!(repo.current_epoch(), new_epoch);

        // Files should exist in default workspace
        assert!(repo.file_exists("default", "README.md"));
        assert!(repo.file_exists("default", "src/lib.rs"));
    }

    #[test]
    fn test_new_workspace_after_seed_has_files() {
        let repo = TestRepo::new();
        repo.seed_files(&[("README.md", "# Hello")]);

        // Create workspace AFTER seeding — should have the seeded files
        repo.create_workspace("agent");

        assert!(
            repo.file_exists("agent", "README.md"),
            "workspace created after epoch advance should have seeded files"
        );
        assert_eq!(
            repo.read_file("agent", "README.md"),
            Some("# Hello".to_owned())
        );
    }

    #[test]
    fn test_workspace_isolation() {
        let repo = TestRepo::new();
        repo.seed_files(&[("shared.txt", "base content")]);

        repo.create_workspace("alice");
        repo.create_workspace("bob");

        // Alice modifies shared.txt
        repo.modify_file("alice", "shared.txt", "alice's version");

        // Bob should still see the original
        assert_eq!(
            repo.read_file("bob", "shared.txt"),
            Some("base content".to_owned()),
            "bob should not see alice's changes"
        );

        // Alice adds a new file
        repo.add_file("alice", "alice-only.txt", "alice");

        // Bob should not have it
        assert!(
            !repo.file_exists("bob", "alice-only.txt"),
            "bob should not see alice's new file"
        );
    }

    #[test]
    fn test_workspace_stale_detection() {
        let repo = TestRepo::new();
        repo.create_workspace("agent");

        let ws_head_before = repo.workspace_head("agent");

        // Advance epoch in default workspace
        repo.add_file("default", "new-file.txt", "content");
        let new_epoch = repo.advance_epoch("chore: add new file");

        // Agent's HEAD should still be the old epoch
        let ws_head_after = repo.workspace_head("agent");
        assert_eq!(
            ws_head_before, ws_head_after,
            "workspace HEAD should not change"
        );
        assert_ne!(
            ws_head_after, new_epoch,
            "workspace should be stale (behind current epoch)"
        );
    }

    #[test]
    fn test_multiple_workspaces_parallel_safe() {
        // Create 5 repos in parallel-like fashion (sequential but unique)
        let repos: Vec<TestRepo> = (0..5).map(|_| TestRepo::new()).collect();

        // Each should have unique paths
        for (i, a) in repos.iter().enumerate() {
            for (j, b) in repos.iter().enumerate() {
                if i != j {
                    assert_ne!(
                        a.root(),
                        b.root(),
                        "repos {i} and {j} should have different roots"
                    );
                }
            }
        }

        // Each should be independently functional
        for (i, repo) in repos.iter().enumerate() {
            let ws_name = format!("agent-{i}");
            repo.create_workspace(&ws_name);
            repo.add_file(&ws_name, "test.txt", &format!("repo {i}"));
            assert_eq!(
                repo.read_file(&ws_name, "test.txt"),
                Some(format!("repo {i}"))
            );
        }
    }

    #[test]
    fn test_with_remote() {
        let (repo, _remote) = TestRepo::with_remote();

        // Should have a valid repo structure
        assert!(repo.root().join(".git").exists());
        assert!(repo.default_workspace().is_dir());

        // Should have origin configured
        let remotes = repo.git(&["remote", "-v"]);
        assert!(remotes.contains("origin"), "should have origin remote");
    }
}
