//! Manifold v2 greenfield initialization — git-native.
//!
//! Creates a new Manifold-managed repository from scratch when no `.git/`
//! exists. Sets up a bare-mode git repo, `.manifold/` metadata, epoch₀,
//! and the `ws/default/` workspace via `git worktree`.
//!
//! # Architecture
//!
//! ```text
//! repo-root/
//! ├── .git/              ← git data (core.bare=true after setup)
//! ├── .manifold/         ← manifold metadata
//! │   ├── config.toml
//! │   ├── epochs/
//! │   └── artifacts/
//! ├── ws/
//! │   └── default/       ← main worktree (git worktree)
//! └── .gitignore
//! ```

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::model::layout;
use crate::model::types::EpochId;
use crate::refs as manifold_refs;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during greenfield initialization.
#[derive(Debug)]
pub enum InitError {
    /// A `.git/` directory already exists at the target path.
    AlreadyExists { path: PathBuf },
    /// A git command failed.
    GitCommand {
        command: String,
        stderr: String,
        exit_code: Option<i32>,
    },
    /// Failed to create the `.manifold/` directory structure.
    Layout(io::Error),
    /// Failed to set a git ref.
    RefSet { ref_name: String, message: String },
    /// An I/O error occurred.
    Io(io::Error),
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyExists { path } => {
                write!(
                    f,
                    "git repository already exists at {}\n  \
                     To init from an existing repo, use `maw init` (brownfield mode)",
                    path.display()
                )
            }
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
            Self::Layout(e) => write!(f, "failed to create .manifold/ directory: {e}"),
            Self::RefSet { ref_name, message } => {
                write!(f, "failed to set ref {ref_name}: {message}")
            }
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for InitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Layout(e) | Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for InitError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Result type
// ---------------------------------------------------------------------------

/// Successful result of a greenfield init.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitResult {
    /// Absolute path to the repository root.
    pub repo_root: PathBuf,
    /// Absolute path to the default workspace (`ws/default/`).
    pub default_workspace: PathBuf,
    /// The epoch₀ commit OID (initial empty commit).
    pub epoch0: EpochId,
    /// The branch name (e.g., `"main"`).
    pub branch: String,
}

impl fmt::Display for InitResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Manifold repository initialized!")?;
        writeln!(f)?;
        writeln!(f, "  Root:      {}", self.repo_root.display())?;
        writeln!(f, "  Workspace: {}/", self.default_workspace.display())?;
        writeln!(f, "  Branch:    {}", self.branch)?;
        writeln!(f, "  Epoch₀:    {}", &self.epoch0.as_str()[..12])?;
        writeln!(f)?;
        writeln!(f, "Next steps:")?;
        writeln!(f, "  Workspace path: {}/", self.default_workspace.display())?;
        writeln!(
            f,
            "  maw ws create <agent-name>    # create agent workspace"
        )?;
        writeln!(
            f,
            "  maw ws status                 # check workspace status"
        )
    }
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// Options for greenfield initialization.
#[derive(Clone, Debug)]
pub struct InitOptions {
    /// The main branch name (default: `"main"`).
    pub branch: String,
}

impl Default for InitOptions {
    fn default() -> Self {
        Self {
            branch: "main".to_owned(),
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Initialize a new Manifold repository from scratch (greenfield).
///
/// This is the entry point for `maw init` when no `.git/` exists.
///
/// # Steps
/// 1. Verify no `.git/` exists
/// 2. `git init` — creates a standard git repo
/// 3. Create an initial empty commit on the configured branch
/// 4. Set `core.bare = true` — root becomes metadata-only
/// 5. Remove root working tree files (git index, etc.)
/// 6. Create `.manifold/` directory structure
/// 7. Set `refs/manifold/epoch/current` → initial commit
/// 8. `git worktree add --detach ws/default <commit>` — default workspace
/// 9. Configure `.gitignore` in the default workspace
///
/// # Errors
/// Returns [`InitError`] if any step fails. Partial state may remain
/// on disk — callers should clean up on error if desired.
///
/// # Idempotency
/// This function is NOT idempotent — it errors if `.git/` already exists.
/// Use brownfield init for existing repos.
pub fn greenfield_init(root: &Path, opts: &InitOptions) -> Result<InitResult, InitError> {
    let root = std::fs::canonicalize(root).map_err(InitError::Io)?;

    // 1. Verify no .git/ exists
    let git_dir = root.join(".git");
    if git_dir.exists() {
        return Err(InitError::AlreadyExists { path: git_dir });
    }

    // Ensure root directory exists
    std::fs::create_dir_all(&root)?;

    // 2. git init
    git_init(&root)?;

    // 3. Create initial empty commit
    let epoch0_oid = create_initial_commit(&root, &opts.branch)?;

    // 4. Set core.bare = true
    set_bare_mode(&root)?;

    // 5. Clean up root working tree artifacts
    clean_root_worktree(&root)?;

    // 6. Create .manifold/ directory structure
    layout::init_manifold_dir(&root).map_err(InitError::Layout)?;

    // 7. Set refs/manifold/epoch/current → epoch₀
    set_epoch_ref(&root, &epoch0_oid)?;

    // 8. Create ws/default/ workspace
    let ws_default = create_default_workspace(&root, &epoch0_oid)?;

    // 9. Configure .gitignore in the workspace
    setup_workspace_gitignore(&ws_default)?;

    Ok(InitResult {
        repo_root: root,
        default_workspace: ws_default,
        epoch0: epoch0_oid,
        branch: opts.branch.clone(),
    })
}

// ---------------------------------------------------------------------------
// Internal steps
// ---------------------------------------------------------------------------

/// Run `git init` in the target directory.
fn git_init(root: &Path) -> Result<(), InitError> {
    run_git(root, &["init"])?;
    Ok(())
}

/// Create an initial empty commit on the given branch.
///
/// Uses `git commit --allow-empty` to create an empty commit that serves
/// as epoch₀. Returns the commit OID as an `EpochId`.
fn create_initial_commit(root: &Path, branch: &str) -> Result<EpochId, InitError> {
    // Ensure we're on the right branch (git init may default to "master")
    // Use checkout -B to create/reset the branch
    run_git(root, &["checkout", "-B", branch])?;

    // Configure a committer identity for the initial commit if not set.
    // git commit fails without user.name/email. Use repo-local config
    // so we don't pollute the user's global config.
    ensure_git_identity(root)?;

    // Disable GPG signing for this repo — the initial commit must not
    // require user interaction (e.g., pinentry). Users can re-enable it
    // after init if desired.
    run_git(root, &["config", "commit.gpgsign", "false"])?;

    // Create the empty commit
    run_git(
        root,
        &[
            "commit",
            "--allow-empty",
            "-m",
            "manifold: epoch₀ (initial empty commit)",
        ],
    )?;

    // Read the commit OID
    let oid_str = run_git_stdout(root, &["rev-parse", "HEAD"])?;
    let oid_str = oid_str.trim();

    EpochId::new(oid_str).map_err(|e| InitError::GitCommand {
        command: "git rev-parse HEAD".to_owned(),
        stderr: format!("invalid OID from git: {e}"),
        exit_code: None,
    })
}

/// Ensure git user.name and user.email are set (repo-local).
fn ensure_git_identity(root: &Path) -> Result<(), InitError> {
    // Check if user.name is set
    let name_check = Command::new("git")
        .args(["config", "user.name"])
        .current_dir(root)
        .output()?;

    if !name_check.status.success()
        || String::from_utf8_lossy(&name_check.stdout)
            .trim()
            .is_empty()
    {
        run_git(root, &["config", "user.name", "Manifold"])?;
    }

    let email_check = Command::new("git")
        .args(["config", "user.email"])
        .current_dir(root)
        .output()?;

    if !email_check.status.success()
        || String::from_utf8_lossy(&email_check.stdout)
            .trim()
            .is_empty()
    {
        run_git(root, &["config", "user.email", "manifold@localhost"])?;
    }

    Ok(())
}

/// Set `core.bare = true` so git treats the root as a bare repo.
fn set_bare_mode(root: &Path) -> Result<(), InitError> {
    run_git(root, &["config", "core.bare", "true"])?;
    Ok(())
}

/// Clean up root working tree artifacts after setting bare mode.
///
/// After `core.bare = true`, the index file and any checkout artifacts
/// at root are no longer needed. The working tree lives in `ws/default/`.
fn clean_root_worktree(root: &Path) -> Result<(), InitError> {
    // Remove index file (not needed in bare mode)
    let index = root.join(".git").join("index");
    if index.exists() {
        std::fs::remove_file(&index)?;
    }
    Ok(())
}

/// Set `refs/manifold/epoch/current` to the given epoch commit.
///
/// Delegates to the shared [`crate::refs`] module.
fn set_epoch_ref(root: &Path, epoch: &EpochId) -> Result<(), InitError> {
    let ref_name = manifold_refs::EPOCH_CURRENT;
    manifold_refs::write_ref(root, ref_name, epoch.oid()).map_err(|e| InitError::RefSet {
        ref_name: ref_name.to_owned(),
        message: e.to_string(),
    })
}

/// Create the default workspace at `ws/default/` using `git worktree`.
fn create_default_workspace(root: &Path, epoch: &EpochId) -> Result<PathBuf, InitError> {
    let ws_dir = root.join("ws");
    std::fs::create_dir_all(&ws_dir)?;

    let ws_path = ws_dir.join("default");

    // Use git worktree add --detach to create a workspace not tied to any branch
    run_git(
        root,
        &[
            "worktree",
            "add",
            "--detach",
            ws_path.to_str().unwrap_or("ws/default"),
            epoch.as_str(),
        ],
    )?;

    Ok(ws_path)
}

/// Set up `.gitignore` in the default workspace.
///
/// The workspace needs its own `.gitignore` to exclude `ws/` and
/// `.manifold/` ephemeral directories from version control.
fn setup_workspace_gitignore(ws_path: &Path) -> Result<(), InitError> {
    let gitignore_path = ws_path.join(".gitignore");

    let content = "\
# Manifold workspaces (each agent gets their own worktree)
ws/

# Manifold ephemeral data
.manifold/epochs/
.manifold/cow/
.manifold/artifacts/
";

    std::fs::write(&gitignore_path, content)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Git command helpers
// ---------------------------------------------------------------------------

/// Run a git command and check for success.
fn run_git(root: &Path, args: &[&str]) -> Result<(), InitError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| InitError::GitCommand {
            command: format!("git {}", args.join(" ")),
            stderr: format!("failed to spawn: {e}"),
            exit_code: None,
        })?;

    if output.status.success() {
        Ok(())
    } else {
        Err(InitError::GitCommand {
            command: format!("git {}", args.join(" ")),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            exit_code: output.status.code(),
        })
    }
}

/// Run a git command and return its stdout as a string.
fn run_git_stdout(root: &Path, args: &[&str]) -> Result<String, InitError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| InitError::GitCommand {
            command: format!("git {}", args.join(" ")),
            stderr: format!("failed to spawn: {e}"),
            exit_code: None,
        })?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(InitError::GitCommand {
            command: format!("git {}", args.join(" ")),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            exit_code: output.status.code(),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Helper: verify a git ref exists and points to expected OID.
    fn read_ref(root: &Path, ref_name: &str) -> String {
        let output = Command::new("git")
            .args(["rev-parse", ref_name])
            .current_dir(root)
            .output()
            .expect("git rev-parse should succeed");
        assert!(output.status.success(), "ref {ref_name} should exist");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// Helper: check if a path is a git worktree.
    fn is_worktree(root: &Path, ws_path: &Path) -> bool {
        let output = Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(root)
            .output()
            .expect("git worktree list should succeed");
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout.contains(&ws_path.to_string_lossy().to_string())
    }

    #[test]
    fn greenfield_creates_valid_repo() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("myrepo");
        std::fs::create_dir_all(&root).unwrap();

        let result = greenfield_init(&root, &InitOptions::default()).unwrap();

        // .git/ exists
        assert!(root.join(".git").exists(), ".git/ should exist");

        // Repo root is absolute
        assert!(result.repo_root.is_absolute());

        // Branch is "main"
        assert_eq!(result.branch, "main");
    }

    #[test]
    fn greenfield_creates_manifold_dir() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("myrepo");
        std::fs::create_dir_all(&root).unwrap();

        greenfield_init(&root, &InitOptions::default()).unwrap();

        assert!(root.join(".manifold").is_dir());
        assert!(root.join(".manifold/epochs").is_dir());
        assert!(root.join(".manifold/artifacts").is_dir());
        assert!(root.join(".manifold/artifacts/ws").is_dir());
        assert!(root.join(".manifold/artifacts/merge").is_dir());
        assert!(root.join(".manifold/config.toml").is_file());
    }

    #[test]
    fn greenfield_sets_epoch0() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("myrepo");
        std::fs::create_dir_all(&root).unwrap();

        let result = greenfield_init(&root, &InitOptions::default()).unwrap();

        // refs/manifold/epoch/current exists and matches epoch0
        let ref_oid = read_ref(&result.repo_root, "refs/manifold/epoch/current");
        assert_eq!(ref_oid, result.epoch0.as_str());

        // HEAD also points to epoch0 (the initial commit)
        let head_oid = read_ref(&result.repo_root, "HEAD");
        assert_eq!(head_oid, result.epoch0.as_str());
    }

    #[test]
    fn greenfield_creates_default_workspace() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("myrepo");
        std::fs::create_dir_all(&root).unwrap();

        let result = greenfield_init(&root, &InitOptions::default()).unwrap();

        // ws/default/ exists
        assert!(result.default_workspace.is_dir());
        assert!(result.default_workspace.ends_with("ws/default"));

        // It's a git worktree
        assert!(is_worktree(&result.repo_root, &result.default_workspace));
    }

    #[test]
    fn greenfield_workspace_has_gitignore() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("myrepo");
        std::fs::create_dir_all(&root).unwrap();

        let result = greenfield_init(&root, &InitOptions::default()).unwrap();

        let gitignore = std::fs::read_to_string(result.default_workspace.join(".gitignore"))
            .expect(".gitignore should exist in workspace");
        assert!(gitignore.contains("ws/"), ".gitignore should exclude ws/");
        assert!(
            gitignore.contains(".manifold/"),
            ".gitignore should exclude .manifold/ dirs"
        );
    }

    #[test]
    fn greenfield_rejects_existing_git() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        // Create a .git dir manually
        std::fs::create_dir_all(root.join(".git")).unwrap();

        let err = greenfield_init(root, &InitOptions::default()).unwrap_err();
        assert!(
            matches!(err, InitError::AlreadyExists { .. }),
            "should reject existing .git/: {err}"
        );
    }

    #[test]
    fn greenfield_sets_bare_mode() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("myrepo");
        std::fs::create_dir_all(&root).unwrap();

        let result = greenfield_init(&root, &InitOptions::default()).unwrap();

        let output = Command::new("git")
            .args(["config", "core.bare"])
            .current_dir(&result.repo_root)
            .output()
            .unwrap();
        let val = String::from_utf8_lossy(&output.stdout);
        assert_eq!(val.trim(), "true", "core.bare should be true");
    }

    #[test]
    fn greenfield_custom_branch() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("myrepo");
        std::fs::create_dir_all(&root).unwrap();

        let opts = InitOptions {
            branch: "develop".to_owned(),
        };
        let result = greenfield_init(&root, &opts).unwrap();

        assert_eq!(result.branch, "develop");

        // Branch ref exists
        let branch_oid = read_ref(&result.repo_root, "refs/heads/develop");
        assert_eq!(branch_oid, result.epoch0.as_str());
    }

    #[test]
    fn greenfield_epoch0_is_valid_oid() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("myrepo");
        std::fs::create_dir_all(&root).unwrap();

        let result = greenfield_init(&root, &InitOptions::default()).unwrap();

        // EpochId validates as a proper 40-char hex OID
        assert_eq!(result.epoch0.as_str().len(), 40);
        assert!(result
            .epoch0
            .as_str()
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn greenfield_no_index_in_root() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("myrepo");
        std::fs::create_dir_all(&root).unwrap();

        let result = greenfield_init(&root, &InitOptions::default()).unwrap();

        // Index file should be removed (bare mode)
        assert!(
            !result.repo_root.join(".git/index").exists(),
            "index should be removed in bare mode"
        );
    }

    #[test]
    fn greenfield_workspace_at_correct_commit() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("myrepo");
        std::fs::create_dir_all(&root).unwrap();

        let result = greenfield_init(&root, &InitOptions::default()).unwrap();

        // The workspace HEAD should be at epoch₀
        let ws_head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&result.default_workspace)
            .output()
            .unwrap();
        let ws_oid = String::from_utf8_lossy(&ws_head.stdout).trim().to_owned();
        assert_eq!(ws_oid, result.epoch0.as_str());
    }

    #[test]
    fn greenfield_gitignore_at_root() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("myrepo");
        std::fs::create_dir_all(&root).unwrap();

        greenfield_init(&root, &InitOptions::default()).unwrap();

        // layout::init_manifold_dir creates .gitignore at root
        let gitignore = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(gitignore.contains("ws/"), "root .gitignore should have ws/");
    }

    #[test]
    fn greenfield_display_result() {
        let result = InitResult {
            repo_root: PathBuf::from("/tmp/myrepo"),
            default_workspace: PathBuf::from("/tmp/myrepo/ws/default"),
            epoch0: EpochId::new(&"a".repeat(40)).unwrap(),
            branch: "main".to_owned(),
        };
        let display = format!("{result}");
        assert!(display.contains("Manifold repository initialized"));
        assert!(display.contains("/tmp/myrepo"));
        assert!(display.contains("ws/default"));
        assert!(display.contains("main"));
        assert!(display.contains("aaaaaaaaaaaa")); // first 12 chars
    }

    #[test]
    fn init_error_display() {
        let err = InitError::AlreadyExists {
            path: PathBuf::from("/repo/.git"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("/repo/.git"));
        assert!(msg.contains("already exists"));

        let err = InitError::GitCommand {
            command: "git init".to_owned(),
            stderr: "fatal: something".to_owned(),
            exit_code: Some(128),
        };
        let msg = format!("{err}");
        assert!(msg.contains("git init"));
        assert!(msg.contains("128"));
        assert!(msg.contains("fatal: something"));
    }
}

// ============================================================================
// BROWNFIELD INIT
// ============================================================================

/// Errors that can occur during brownfield initialization.
#[derive(Debug)]
pub enum BrownfieldInitError {
    /// No `.git/` directory found — use greenfield init for new repos.
    NotAGitRepo { path: PathBuf },
    /// The repository has no commits yet. Commit something first.
    EmptyRepo,
    /// A git command failed.
    GitCommand {
        command: String,
        stderr: String,
        exit_code: Option<i32>,
    },
    /// Failed to create the `.manifold/` directory structure.
    Layout(io::Error),
    /// Failed to set a git ref.
    RefSet { ref_name: String, message: String },
    /// An I/O error occurred.
    Io(io::Error),
}

impl fmt::Display for BrownfieldInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAGitRepo { path } => {
                write!(
                    f,
                    "no git repository found at {}\n  \
                     To create a new Manifold repo, use `maw init --new` (greenfield mode)",
                    path.display()
                )
            }
            Self::EmptyRepo => {
                write!(
                    f,
                    "repository has no commits\n  \
                     Make an initial commit first: git add . && git commit -m 'initial commit'"
                )
            }
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
            Self::Layout(e) => write!(f, "failed to create .manifold/ directory: {e}"),
            Self::RefSet { ref_name, message } => {
                write!(f, "failed to set ref {ref_name}: {message}")
            }
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for BrownfieldInitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Layout(e) | Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for BrownfieldInitError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Result and options for brownfield
// ---------------------------------------------------------------------------

/// Successful result of a brownfield init.
#[derive(Clone, Debug)]
pub struct BrownfieldInitResult {
    /// Absolute path to the repository root.
    pub repo_root: PathBuf,
    /// Absolute path to the default workspace (`ws/default/`).
    pub default_workspace: PathBuf,
    /// The epoch₀ OID (the HEAD commit at time of init).
    pub epoch0: EpochId,
    /// The branch name, or `None` if HEAD was detached.
    pub head_branch: Option<String>,
    /// `true` if Manifold was already initialized (idempotent no-op).
    pub already_initialized: bool,
    /// Files with uncommitted changes kept at repo root.
    pub dirty_files_at_root: Vec<PathBuf>,
    /// Count of tracked source files removed from root (now in `ws/default/`).
    pub cleaned_root_files: usize,
}

impl fmt::Display for BrownfieldInitResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.already_initialized {
            writeln!(f, "Manifold already initialized.")?;
            writeln!(f)?;
            writeln!(f, "  Root:      {}", self.repo_root.display())?;
            writeln!(f, "  Workspace: {}/", self.default_workspace.display())?;
            writeln!(f, "  Epoch₀:    {}", &self.epoch0.as_str()[..12])?;
            return Ok(());
        }

        writeln!(f, "Manifold initialized for existing repository!")?;
        writeln!(f)?;
        writeln!(f, "  Root:      {}", self.repo_root.display())?;
        writeln!(f, "  Workspace: {}/", self.default_workspace.display())?;
        if let Some(branch) = &self.head_branch {
            writeln!(f, "  Branch:    {branch}")?;
        } else {
            writeln!(f, "  Branch:    (detached HEAD)")?;
        }
        writeln!(f, "  Epoch₀:    {}", &self.epoch0.as_str()[..12])?;
        if self.cleaned_root_files > 0 {
            writeln!(
                f,
                "  Cleaned:   {} file(s) removed from root (now in ws/default/)",
                self.cleaned_root_files
            )?;
        }
        if !self.dirty_files_at_root.is_empty() {
            writeln!(
                f,
                "  WARNING:   {} file(s) with uncommitted changes kept at root",
                self.dirty_files_at_root.len()
            )?;
        }
        writeln!(f)?;
        writeln!(f, "Next steps:")?;
        writeln!(f, "  Workspace path: {}/", self.default_workspace.display())?;
        writeln!(
            f,
            "  maw ws create <agent-name>    # create agent workspace"
        )?;
        writeln!(
            f,
            "  maw ws status                 # check workspace status"
        )
    }
}

/// Options for brownfield initialization.
#[derive(Clone, Debug)]
pub struct BrownfieldInitOptions {
    /// Remove tracked files from repo root after creating `ws/default/`.
    ///
    /// When enabled (default), tracked source files are moved out of the root
    /// metadata directory and remain accessible in `ws/default/`. Dirty files
    /// are still preserved at root.
    pub clean_root_tracked_files: bool,
}

impl Default for BrownfieldInitOptions {
    fn default() -> Self {
        Self {
            clean_root_tracked_files: true,
        }
    }
}

pub fn run() -> anyhow::Result<()> {
    let root = std::env::current_dir()?;
    let git_dir = root.join(".git");

    if git_dir.exists() {
        let result = brownfield_init(&root, &BrownfieldInitOptions::default())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        println!("{result}");
    } else {
        let result = greenfield_init(&root, &InitOptions::default())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        println!("{result}");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Initialize Manifold in an existing git repository (brownfield).
///
/// Unlike [`greenfield_init`], this function expects a `.git/` to already
/// exist. It preserves all existing history and sets `epoch₀ = HEAD`.
///
/// # Steps
/// 1. Verify `.git/` exists and is a valid repository
/// 2. Idempotency check — if already initialized, return early
/// 3. Detect dirty working tree — warn but continue
/// 4. Read current HEAD as epoch₀
/// 5. Detect detached HEAD — warn but continue
/// 6. Set `core.bare = true`
/// 7. Remove root `.git/index` (bare mode cleanup)
/// 8. Create `.manifold/` directory structure
/// 9. Set `refs/manifold/epoch/current` → HEAD
/// 10. Create `ws/default/` via `git worktree add --detach`
/// 11. Remove tracked source files from root (dirty files are kept with warning)
/// 12. Update root `.gitignore`
///
/// # Errors
/// Returns [`BrownfieldInitError`] if any step fails. Partial state may remain
/// on disk — callers should display the error clearly and advise the user.
///
/// # Idempotency
/// Running this function twice is safe. If `ws/default/` already exists,
/// it returns early with `already_initialized = true`.
pub fn brownfield_init(
    root: &Path,
    opts: &BrownfieldInitOptions,
) -> Result<BrownfieldInitResult, BrownfieldInitError> {
    let root = std::fs::canonicalize(root)?;

    // 1. Verify .git/ exists
    let git_dir = root.join(".git");
    if !git_dir.exists() {
        return Err(BrownfieldInitError::NotAGitRepo { path: git_dir });
    }

    // 2. Verify it is a valid git repo
    bf_verify_git_repo(&root)?;

    // 3. Idempotency check: ws/default/ already exists → already initialized
    let ws_default = root.join("ws").join("default");
    if ws_default.exists() {
        let epoch0 = bf_read_epoch_ref(&root)?;
        let head_branch = bf_detect_head_branch(&root);
        return Ok(BrownfieldInitResult {
            repo_root: root,
            default_workspace: ws_default,
            epoch0,
            head_branch,
            already_initialized: true,
            dirty_files_at_root: Vec::new(),
            cleaned_root_files: 0,
        });
    }

    // 4. Check dirty working tree (warn but proceed)
    let dirty_files = bf_get_dirty_files(&root)?;
    if !dirty_files.is_empty() {
        eprintln!(
            "WARNING: dirty working tree — {} modified file(s):",
            dirty_files.len()
        );
        for f in &dirty_files {
            eprintln!("  M  {}", f.display());
        }
        eprintln!("  Uncommitted changes are kept at root. The committed versions will");
        eprintln!("  appear in ws/default/. Commit your changes to include them in epoch₀.");
    }

    // 5. Get current HEAD as epoch₀
    let epoch0 = bf_get_head_oid(&root)?;

    // 6. Detect detached HEAD (warn but proceed)
    let head_branch = bf_detect_head_branch(&root);
    if head_branch.is_none() {
        eprintln!(
            "WARNING: HEAD is detached. Epoch₀ set to the current detached HEAD ({}).",
            &epoch0.as_str()[..12]
        );
        eprintln!("  To attach HEAD to a branch after init, run inside ws/default/:");
        eprintln!("    git checkout -b <branch-name>");
    }

    // 7. Set core.bare = true
    bf_set_bare_mode(&root)?;

    // 8. Remove root index file (bare mode cleanup)
    bf_clean_root_index(&root)?;

    // 9. Create .manifold/ directory structure
    layout::init_manifold_dir(&root).map_err(BrownfieldInitError::Layout)?;

    // 10. Set refs/manifold/epoch/current → HEAD
    bf_set_epoch_ref(&root, &epoch0)?;

    // 11. Create ws/default/ worktree at HEAD
    let ws_dir = root.join("ws");
    std::fs::create_dir_all(&ws_dir)?;
    bf_create_default_workspace(&root, &epoch0, &ws_default)?;

    // 12. Remove tracked source files from root (skip dirty ones)
    let cleaned_count = if opts.clean_root_tracked_files {
        let dirty_set: std::collections::HashSet<_> = dirty_files.iter().cloned().collect();
        bf_clean_root_tracked_files(&root, &dirty_set)?
    } else {
        0
    };

    Ok(BrownfieldInitResult {
        repo_root: root,
        default_workspace: ws_default,
        epoch0,
        head_branch,
        already_initialized: false,
        dirty_files_at_root: dirty_files,
        cleaned_root_files: cleaned_count,
    })
}

// ---------------------------------------------------------------------------
// Brownfield internal helpers
// ---------------------------------------------------------------------------

/// Verify that the directory is a valid git repository.
fn bf_verify_git_repo(root: &Path) -> Result<(), BrownfieldInitError> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(root)
        .output()
        .map_err(|e| BrownfieldInitError::GitCommand {
            command: "git rev-parse --git-dir".to_owned(),
            stderr: format!("failed to spawn: {e}"),
            exit_code: None,
        })?;

    if !output.status.success() {
        return Err(BrownfieldInitError::GitCommand {
            command: "git rev-parse --git-dir".to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            exit_code: output.status.code(),
        });
    }
    Ok(())
}

/// Read `refs/manifold/epoch/current` — used for idempotency check.
///
/// Delegates to the shared [`crate::refs`] module. Falls back to HEAD if
/// the ref has not been set yet.
fn bf_read_epoch_ref(root: &Path) -> Result<EpochId, BrownfieldInitError> {
    match manifold_refs::read_epoch_current(root) {
        Ok(Some(oid)) => EpochId::new(oid.as_str()).map_err(|e| BrownfieldInitError::GitCommand {
            command: format!("git rev-parse {}", manifold_refs::EPOCH_CURRENT),
            stderr: format!("invalid OID: {e}"),
            exit_code: None,
        }),
        Ok(None) => {
            // Ref doesn't exist yet — fall back to HEAD
            bf_get_head_oid(root)
        }
        Err(e) => Err(BrownfieldInitError::GitCommand {
            command: format!("git rev-parse {}", manifold_refs::EPOCH_CURRENT),
            stderr: e.to_string(),
            exit_code: None,
        }),
    }
}

/// Detect the current branch name. Returns `None` if HEAD is detached.
fn bf_detect_head_branch(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["symbolic-ref", "--short", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;

    if output.status.success() {
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if branch.is_empty() {
            None
        } else {
            Some(branch)
        }
    } else {
        None
    }
}

/// List files with uncommitted changes (modified, added, or deleted in working tree).
fn bf_get_dirty_files(root: &Path) -> Result<Vec<PathBuf>, BrownfieldInitError> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .output()
        .map_err(|e| BrownfieldInitError::GitCommand {
            command: "git status --porcelain".to_owned(),
            stderr: format!("failed to spawn: {e}"),
            exit_code: None,
        })?;

    if !output.status.success() {
        return Err(BrownfieldInitError::GitCommand {
            command: "git status --porcelain".to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            exit_code: output.status.code(),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut dirty = Vec::new();
    for line in stdout.lines() {
        if line.len() < 4 {
            continue;
        }
        // Porcelain format: XY path (first two chars are index/worktree status)
        let worktree_status = line.chars().nth(1).unwrap_or(' ');
        if worktree_status != ' ' && worktree_status != '?' {
            // Modified or deleted in worktree
            let path_part = line[3..].trim();
            dirty.push(PathBuf::from(path_part));
        } else if line.starts_with("??") {
            // Untracked files — not dirty in the git sense, but notable
            // We don't include untracked files in dirty_files since they
            // won't conflict with the worktree checkout.
        } else {
            // Staged changes (index status != ' ')
            let index_status = line.chars().next().unwrap_or(' ');
            if index_status != ' ' && index_status != '?' {
                let path_part = line[3..].trim();
                dirty.push(PathBuf::from(path_part));
            }
        }
    }
    // Deduplicate (a file can appear in both index and worktree columns)
    dirty.sort();
    dirty.dedup();
    Ok(dirty)
}

/// Get current HEAD OID. Returns `EmptyRepo` error if there are no commits.
fn bf_get_head_oid(root: &Path) -> Result<EpochId, BrownfieldInitError> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .map_err(|e| BrownfieldInitError::GitCommand {
            command: "git rev-parse HEAD".to_owned(),
            stderr: format!("failed to spawn: {e}"),
            exit_code: None,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // git rev-parse HEAD fails with "unknown revision" when there are no commits
        if stderr.contains("unknown revision")
            || stderr.contains("ambiguous argument 'HEAD'")
            || stderr.contains("does not have any commits")
        {
            return Err(BrownfieldInitError::EmptyRepo);
        }
        return Err(BrownfieldInitError::GitCommand {
            command: "git rev-parse HEAD".to_owned(),
            stderr: stderr.trim().to_owned(),
            exit_code: output.status.code(),
        });
    }

    let oid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    EpochId::new(&oid).map_err(|e| BrownfieldInitError::GitCommand {
        command: "git rev-parse HEAD".to_owned(),
        stderr: format!("invalid OID from git: {e}"),
        exit_code: None,
    })
}

/// Set `core.bare = true`.
fn bf_set_bare_mode(root: &Path) -> Result<(), BrownfieldInitError> {
    bf_run_git(root, &["config", "core.bare", "true"])
}

/// Remove `.git/index` (bare mode cleanup — no working tree at root).
fn bf_clean_root_index(root: &Path) -> Result<(), BrownfieldInitError> {
    let index = root.join(".git").join("index");
    if index.exists() {
        std::fs::remove_file(&index)?;
    }
    Ok(())
}

/// Set `refs/manifold/epoch/current` to the given epoch commit.
///
/// Delegates to the shared [`crate::refs`] module.
fn bf_set_epoch_ref(root: &Path, epoch: &EpochId) -> Result<(), BrownfieldInitError> {
    let ref_name = manifold_refs::EPOCH_CURRENT;
    manifold_refs::write_ref(root, ref_name, epoch.oid()).map_err(|e| BrownfieldInitError::RefSet {
        ref_name: ref_name.to_owned(),
        message: e.to_string(),
    })
}

/// Create the default workspace at `ws/default/` using `git worktree add`.
fn bf_create_default_workspace(
    root: &Path,
    epoch: &EpochId,
    ws_path: &Path,
) -> Result<(), BrownfieldInitError> {
    bf_run_git(
        root,
        &[
            "worktree",
            "add",
            "--detach",
            ws_path.to_str().unwrap_or("ws/default"),
            epoch.as_str(),
        ],
    )
}

/// Remove tracked source files from root that are NOT in the dirty set.
///
/// Uses `git ls-tree -r --name-only HEAD` to enumerate tracked files, then
/// removes each file from root if it exists and is not dirty. Directories
/// are cleaned up if empty after file removal.
///
/// Returns the count of removed files.
fn bf_clean_root_tracked_files(
    root: &Path,
    dirty: &std::collections::HashSet<PathBuf>,
) -> Result<usize, BrownfieldInitError> {
    // List all files tracked at HEAD
    let output = Command::new("git")
        .args(["ls-tree", "-r", "--name-only", "HEAD"])
        .current_dir(root)
        .output()
        .map_err(|e| BrownfieldInitError::GitCommand {
            command: "git ls-tree -r --name-only HEAD".to_owned(),
            stderr: format!("failed to spawn: {e}"),
            exit_code: None,
        })?;

    if !output.status.success() {
        // Non-fatal — if listing fails, just skip cleanup
        return Ok(0);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut removed = 0usize;

    // Collect directories that may become empty (bottom-up cleanup)
    let mut dirs_to_check: Vec<PathBuf> = Vec::new();

    for name in stdout.lines() {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let rel = PathBuf::from(name);

        // Skip dirty files — leave them at root for the user to handle
        if dirty.contains(&rel) {
            continue;
        }

        // Skip Manifold-owned paths (they belong at root)
        if name.starts_with("ws/") || name.starts_with(".manifold/") || name == ".gitignore" {
            continue;
        }

        let abs = root.join(&rel);
        if abs.exists() && abs.is_file()
            && std::fs::remove_file(&abs).is_ok() {
                removed += 1;
                // Track parent directory for cleanup
                if let Some(parent) = rel.parent()
                    && parent != Path::new("") {
                        let abs_parent = root.join(parent);
                        if !dirs_to_check.contains(&abs_parent) {
                            dirs_to_check.push(abs_parent);
                        }
                    }
            }
    }

    // Remove empty directories (deepest first)
    dirs_to_check.sort_by_key(|b| std::cmp::Reverse(b.components().count()));
    for dir in &dirs_to_check {
        // Only remove if empty
        let _ = std::fs::remove_dir(dir); // silently ignore errors (not empty = fine)
    }

    Ok(removed)
}

/// Run a git command and check for success.
fn bf_run_git(root: &Path, args: &[&str]) -> Result<(), BrownfieldInitError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| BrownfieldInitError::GitCommand {
            command: format!("git {}", args.join(" ")),
            stderr: format!("failed to spawn: {e}"),
            exit_code: None,
        })?;

    if output.status.success() {
        Ok(())
    } else {
        Err(BrownfieldInitError::GitCommand {
            command: format!("git {}", args.join(" ")),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            exit_code: output.status.code(),
        })
    }
}

// ---------------------------------------------------------------------------
// Brownfield tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod brownfield_tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Set up a real git repo with an initial commit and some tracked files.
    fn setup_existing_repo(dir: &Path) -> EpochId {
        // git init
        Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .expect("git init");

        // Set identity
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(dir)
            .output()
            .unwrap();

        // Create files
        fs::write(dir.join("README.md"), "# My Project\n").unwrap();
        fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/lib.rs"), "// lib\n").unwrap();

        // Commit
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(dir)
            .output()
            .expect("initial commit");

        // Read HEAD OID
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .unwrap();
        let oid = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        EpochId::new(&oid).unwrap()
    }

    /// Read a git ref from a directory.
    fn read_ref(root: &Path, ref_name: &str) -> Option<String> {
        let out = Command::new("git")
            .args(["rev-parse", ref_name])
            .current_dir(root)
            .output()
            .ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        } else {
            None
        }
    }

    #[test]
    fn brownfield_rejects_missing_git() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // No .git/ here
        let err = brownfield_init(root, &BrownfieldInitOptions::default()).unwrap_err();
        assert!(
            matches!(err, BrownfieldInitError::NotAGitRepo { .. }),
            "expected NotAGitRepo, got: {err}"
        );
    }

    #[test]
    fn brownfield_rejects_empty_repo() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        // Init git but make no commits
        Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(root)
            .output()
            .unwrap();

        let err = brownfield_init(root, &BrownfieldInitOptions::default()).unwrap_err();
        assert!(
            matches!(err, BrownfieldInitError::EmptyRepo),
            "expected EmptyRepo, got: {err}"
        );
    }

    #[test]
    fn brownfield_creates_manifold_dir() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        setup_existing_repo(root);

        brownfield_init(root, &BrownfieldInitOptions::default()).unwrap();

        assert!(root.join(".manifold").is_dir());
        assert!(root.join(".manifold/epochs").is_dir());
        assert!(root.join(".manifold/artifacts").is_dir());
        assert!(root.join(".manifold/config.toml").is_file());
    }

    #[test]
    fn brownfield_sets_epoch_ref_to_head() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let initial_head = setup_existing_repo(root);

        let result = brownfield_init(root, &BrownfieldInitOptions::default()).unwrap();

        // epoch₀ matches initial HEAD
        assert_eq!(result.epoch0, initial_head);

        // refs/manifold/epoch/current points to HEAD
        let ref_oid = read_ref(root, "refs/manifold/epoch/current").unwrap();
        assert_eq!(ref_oid, initial_head.as_str());
    }

    #[test]
    fn brownfield_creates_default_workspace() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        setup_existing_repo(root);

        let result = brownfield_init(root, &BrownfieldInitOptions::default()).unwrap();

        assert!(result.default_workspace.is_dir());
        assert!(result.default_workspace.ends_with("ws/default"));

        // ws/default/ contains the tracked files
        assert!(result.default_workspace.join("README.md").exists());
        assert!(result.default_workspace.join("main.rs").exists());
        assert!(result.default_workspace.join("src/lib.rs").exists());
    }

    #[test]
    fn brownfield_sets_bare_mode() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        setup_existing_repo(root);

        brownfield_init(root, &BrownfieldInitOptions::default()).unwrap();

        let out = Command::new("git")
            .args(["config", "core.bare"])
            .current_dir(root)
            .output()
            .unwrap();
        let val = String::from_utf8_lossy(&out.stdout);
        assert_eq!(val.trim(), "true", "core.bare should be true");
    }

    #[test]
    fn brownfield_removes_tracked_files_from_root() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        setup_existing_repo(root);

        // Before init: files exist at root
        assert!(root.join("README.md").exists());
        assert!(root.join("main.rs").exists());

        let result = brownfield_init(root, &BrownfieldInitOptions::default()).unwrap();

        // After init: tracked files removed from root (now in ws/default/)
        assert!(
            !root.join("README.md").exists(),
            "README.md should be removed from root"
        );
        assert!(
            !root.join("main.rs").exists(),
            "main.rs should be removed from root"
        );
        assert!(result.cleaned_root_files > 0);

        // But files are present in ws/default/
        assert!(result.default_workspace.join("README.md").exists());
        assert!(result.default_workspace.join("main.rs").exists());
    }

    #[test]
    fn brownfield_can_preserve_root_tracked_files_when_cleanup_disabled() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        setup_existing_repo(root);

        let opts = BrownfieldInitOptions {
            clean_root_tracked_files: false,
        };
        let result = brownfield_init(root, &opts).unwrap();

        // Root tracked files are intentionally preserved.
        assert!(root.join("README.md").exists());
        assert!(root.join("main.rs").exists());
        assert_eq!(result.cleaned_root_files, 0);

        // The default workspace still has the full tracked tree.
        assert!(result.default_workspace.join("README.md").exists());
        assert!(result.default_workspace.join("main.rs").exists());
    }

    #[test]
    fn brownfield_cleans_empty_subdirs() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        setup_existing_repo(root);

        brownfield_init(root, &BrownfieldInitOptions::default()).unwrap();

        // src/ should be removed since all its tracked files were cleaned
        assert!(
            !root.join("src").exists() || fs::read_dir(root.join("src")).unwrap().next().is_none(),
            "src/ should be empty or removed after tracked file cleanup"
        );
    }

    #[test]
    fn brownfield_preserves_gitignore_entries() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        // Set up repo with existing .gitignore
        Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(root)
            .output()
            .unwrap();

        fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::write(root.join("README.md"), "hello\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        brownfield_init(root, &BrownfieldInitOptions::default()).unwrap();

        // .gitignore at root should still have original entries
        let gitignore = fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(gitignore.contains("target/"), "target/ should be preserved");
        assert!(gitignore.contains("*.log"), "*.log should be preserved");
        // Manifold entries should also be present
        assert!(gitignore.contains("ws/"), "ws/ should be added");
    }

    #[test]
    fn brownfield_is_idempotent() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        setup_existing_repo(root);

        let result1 = brownfield_init(root, &BrownfieldInitOptions::default()).unwrap();
        assert!(!result1.already_initialized);

        let result2 = brownfield_init(root, &BrownfieldInitOptions::default()).unwrap();
        assert!(
            result2.already_initialized,
            "second call should be idempotent"
        );
        assert_eq!(result2.epoch0, result1.epoch0);
    }

    #[test]
    fn brownfield_preserves_git_history() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let initial_head = setup_existing_repo(root);

        brownfield_init(root, &BrownfieldInitOptions::default()).unwrap();

        // HEAD should still point to the initial commit
        let head_oid = read_ref(root, "HEAD").unwrap();
        assert_eq!(
            head_oid,
            initial_head.as_str(),
            "git history must be preserved"
        );
    }

    #[test]
    fn brownfield_result_not_initialized_display() {
        let result = BrownfieldInitResult {
            repo_root: PathBuf::from("/tmp/myrepo"),
            default_workspace: PathBuf::from("/tmp/myrepo/ws/default"),
            epoch0: EpochId::new(&"b".repeat(40)).unwrap(),
            head_branch: Some("main".to_owned()),
            already_initialized: false,
            dirty_files_at_root: Vec::new(),
            cleaned_root_files: 5,
        };
        let s = format!("{result}");
        assert!(s.contains("Manifold initialized for existing repository"));
        assert!(s.contains("/tmp/myrepo"));
        assert!(s.contains("ws/default"));
        assert!(s.contains("main"));
        assert!(s.contains("5 file(s)"));
    }

    #[test]
    fn brownfield_result_already_initialized_display() {
        let result = BrownfieldInitResult {
            repo_root: PathBuf::from("/tmp/myrepo"),
            default_workspace: PathBuf::from("/tmp/myrepo/ws/default"),
            epoch0: EpochId::new(&"c".repeat(40)).unwrap(),
            head_branch: None,
            already_initialized: true,
            dirty_files_at_root: Vec::new(),
            cleaned_root_files: 0,
        };
        let s = format!("{result}");
        assert!(s.contains("already initialized"));
    }

    #[test]
    fn brownfield_error_display() {
        let err = BrownfieldInitError::NotAGitRepo {
            path: PathBuf::from("/tmp/norepo/.git"),
        };
        let s = format!("{err}");
        assert!(s.contains("/tmp/norepo/.git"));
        assert!(s.contains("no git repository"));

        let err = BrownfieldInitError::EmptyRepo;
        let s = format!("{err}");
        assert!(s.contains("no commits"));

        let err = BrownfieldInitError::GitCommand {
            command: "git worktree add".to_owned(),
            stderr: "fatal: bad ref".to_owned(),
            exit_code: Some(128),
        };
        let s = format!("{err}");
        assert!(s.contains("git worktree add"));
        assert!(s.contains("128"));
        assert!(s.contains("fatal: bad ref"));
    }

    #[test]
    fn brownfield_removes_root_index() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        setup_existing_repo(root);

        // Before init: index should exist
        assert!(
            root.join(".git/index").exists(),
            "index should exist before init"
        );

        brownfield_init(root, &BrownfieldInitOptions::default()).unwrap();

        assert!(
            !root.join(".git/index").exists(),
            "index should be removed in bare mode"
        );
    }

    #[test]
    fn brownfield_workspace_at_correct_commit() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let initial_head = setup_existing_repo(root);

        let result = brownfield_init(root, &BrownfieldInitOptions::default()).unwrap();

        // ws/default/ HEAD should be at epoch₀
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&result.default_workspace)
            .output()
            .unwrap();
        let ws_oid = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        assert_eq!(ws_oid, initial_head.as_str());
    }

    #[test]
    fn brownfield_detects_head_branch() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        setup_existing_repo(root);

        let result = brownfield_init(root, &BrownfieldInitOptions::default()).unwrap();

        // Should detect the branch (git init defaults to "master" or "main")
        assert!(
            result.head_branch.is_some(),
            "should detect branch from symbolic HEAD"
        );
    }

    #[test]
    fn brownfield_gitignore_updated() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        // Set up repo WITHOUT an existing .gitignore
        Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(root)
            .output()
            .unwrap();
        fs::write(root.join("README.md"), "hi\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        brownfield_init(root, &BrownfieldInitOptions::default()).unwrap();

        let gitignore = fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(gitignore.contains("ws/"), ".gitignore should include ws/");
        assert!(
            gitignore.contains(".manifold/"),
            ".gitignore should include .manifold/ patterns"
        );
    }
}
