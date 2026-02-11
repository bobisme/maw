use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use serde::Deserialize;

use crate::format::OutputFormat;
use crate::jj::run_jj;

mod create;
mod history;
mod list;
mod merge;
mod names;
mod prune;
mod restore;
mod status;
pub mod sync;

// Re-export public API used by other modules
pub use sync::auto_sync_if_stale;

/// Default workspace name -- the persistent workspace used for merging,
/// pushing, and coordination. Lives at ws/default/ in the bare repo model.
const DEFAULT_WORKSPACE: &str = "default";

/// Configuration from .maw.toml
#[derive(Debug, Default, Deserialize)]
pub struct MawConfig {
    #[serde(default)]
    merge: MergeConfig,
    #[serde(default)]
    hooks: HooksConfig,
    #[serde(default)]
    repo: RepoConfig,
}

/// Repository configuration
#[derive(Debug, Deserialize)]
struct RepoConfig {
    /// Branch name to use for main bookmark (default: "main")
    #[serde(default = "RepoConfig::default_branch")]
    branch: String,
    /// Default workspace name (default: "default")
    #[serde(default = "RepoConfig::default_default_workspace")]
    default_workspace: String,
}

impl Default for RepoConfig {
    fn default() -> Self {
        Self {
            branch: "main".to_string(),
            default_workspace: DEFAULT_WORKSPACE.to_string(),
        }
    }
}

impl RepoConfig {
    fn default_branch() -> String {
        "main".to_string()
    }

    fn default_default_workspace() -> String {
        DEFAULT_WORKSPACE.to_string()
    }
}

/// Hook configuration for running commands before/after operations
#[derive(Debug, Default, Deserialize)]
pub struct HooksConfig {
    /// Commands to run before merge. Merge aborts if any command fails (non-zero exit).
    #[serde(default)]
    pub(crate) pre_merge: Vec<String>,
    /// Commands to run after merge. Warnings are shown on failure but don't abort.
    #[serde(default)]
    pub(crate) post_merge: Vec<String>,
}

/// Merge-specific configuration
#[derive(Debug, Default, Deserialize)]
pub struct MergeConfig {
    /// Paths to auto-resolve from main during merge conflicts.
    /// Supports glob patterns like ".beads/**".
    #[serde(default)]
    pub(crate) auto_resolve_from_main: Vec<String>,
}

impl MawConfig {
    /// Load config from .maw.toml.
    ///
    /// Checks repo root first, then falls back to ws/default/.maw.toml
    /// (in bare repos, root has no tracked files -- config lives in workspaces).
    pub fn load(repo_root: &Path) -> Result<Self> {
        let root_config = repo_root.join(".maw.toml");
        let ws_config = repo_root.join("ws").join("default").join(".maw.toml");

        let config_path = if root_config.exists() {
            root_config
        } else if ws_config.exists() {
            ws_config
        } else {
            return Ok(Self::default());
        };

        let content = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;
        toml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", config_path.display()))
    }

    /// The configured branch name (default: "main").
    pub fn branch(&self) -> &str {
        &self.repo.branch
    }

    /// The configured default workspace name (default: "default").
    pub fn default_workspace(&self) -> &str {
        &self.repo.default_workspace
    }
}

/// Workspace subcommands
#[derive(Subcommand)]
pub enum WorkspaceCommands {
    /// Create a new workspace for an agent
    ///
    /// Creates an isolated jj workspace in ws/<name>/ with its
    /// own working copy (a separate view of the codebase, like a git
    /// worktree but lightweight). All file reads, writes, and edits must
    /// use the absolute workspace path shown after creation.
    ///
    /// After creation:
    ///   1. Edit files under ws/<name>/ (use absolute paths)
    ///   2. Save work: maw exec <name> -- jj describe -m "feat: ..."
    ///      ('describe' sets the commit message -- like git commit --amend -m)
    ///   3. Run other commands: maw exec <name> -- cmd
    Create {
        /// Name for the workspace (typically the agent's name)
        #[arg(required_unless_present = "random")]
        name: Option<String>,

        /// Generate a random workspace name
        #[arg(long)]
        random: bool,

        /// Base revision to start from (default: main or @)
        #[arg(short, long)]
        revision: Option<String>,
    },

    /// Remove a workspace
    ///
    /// Removes the workspace: unregisters it from jj and deletes the
    /// directory. Merge any important changes first (maw ws merge).
    ///
    /// Non-interactive by default (agents can't respond to prompts).
    /// Use --confirm for interactive confirmation.
    ///
    /// To undo: maw ws restore <name>
    Destroy {
        /// Name of the workspace to destroy
        name: String,

        /// Prompt for confirmation before destroying
        #[arg(short, long)]
        confirm: bool,
    },

    /// Restore a previously destroyed workspace
    ///
    /// Recovers a workspace that was removed with 'maw ws destroy' by
    /// reverting the forget operation from jj's operation log. The
    /// workspace's commit history and file contents are recovered.
    ///
    /// Only works if the workspace was destroyed via 'maw ws destroy'
    /// (which uses 'jj workspace forget' internally).
    ///
    /// Examples:
    ///   maw ws restore alice    # recover alice's destroyed workspace
    Restore {
        /// Name of the workspace to restore
        name: String,
    },

    /// List all workspaces
    ///
    /// Shows all jj workspaces with their current status including:
    /// - Current commit description
    /// - Whether the workspace is stale (out of date with repo)
    /// - Path to the workspace directory
    List {
        /// Show detailed information
        #[arg(short, long)]
        verbose: bool,

        /// Output format: text, json, or pretty
        ///
        /// If not specified, auto-detects: pretty for TTY, text for pipes.
        /// Can also be set via FORMAT env var.
        #[arg(long)]
        format: Option<OutputFormat>,

        /// Shorthand for --format json
        #[arg(long, hide = true, conflicts_with = "format")]
        json: bool,
    },

    /// Show status of current workspace and all agent work
    ///
    /// Displays a comprehensive view of:
    /// - Current workspace state (changes, stale status)
    /// - All agent workspaces and their commits
    /// - Any conflicts that need resolution
    /// - Unmerged work across all workspaces
    Status {
        /// Output format: text, json, or pretty
        ///
        /// If not specified, auto-detects: pretty for TTY, text for pipes.
        /// Can also be set via FORMAT env var.
        #[arg(long)]
        format: Option<OutputFormat>,

        /// Shorthand for --format json
        #[arg(long, hide = true, conflicts_with = "format")]
        json: bool,
    },

    /// Sync workspace with repository (handle stale working copy)
    ///
    /// Run this at the start of every session. If the working copy is stale
    /// (another workspace modified shared commits, so your files are outdated),
    /// this updates your workspace to match. Safe to run even if not stale.
    ///
    /// Use --all to sync all workspaces at once, useful after `jj git fetch`
    /// or when multiple workspaces may be stale.
    Sync {
        /// Sync all workspaces instead of just the current one
        #[arg(long)]
        all: bool,
    },

    /// Deprecated: use `maw exec <workspace> -- jj <args>` instead.
    #[command(hide = true)]
    Jj {
        /// Workspace name
        name: String,

        /// Arguments to pass to jj
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Show commit history for a workspace
    ///
    /// Displays a timeline of commits made in the specified workspace,
    /// making it easy to understand what work was done and when.
    ///
    /// Examples:
    ///   maw ws history alice           # show commits in alice workspace
    ///   maw ws history alice --limit 5 # show only last 5 commits
    History {
        /// Name of the workspace
        name: String,

        /// Number of commits to show (default: 20)
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,

        /// Output format: text, json, pretty (auto-detected from TTY)
        #[arg(long)]
        format: Option<OutputFormat>,

        /// Shorthand for --format json
        #[arg(long, hide = true, conflicts_with = "format")]
        json: bool,
    },

    /// Clean up orphaned, stale, or empty workspaces
    ///
    /// Detects problematic workspaces:
    /// - Orphaned: directory exists in ws/ but jj forgot the workspace
    /// - Missing: jj tracks the workspace but the directory is gone
    /// - Empty (with --empty): workspace has no changes
    ///
    /// By default, shows what would be pruned (preview mode).
    /// Use --force to actually delete.
    ///
    /// Examples:
    ///   maw ws prune              # preview what would be pruned
    ///   maw ws prune --force      # actually delete orphaned/missing
    ///   maw ws prune --empty      # preview including empty workspaces
    ///   maw ws prune --empty --force  # delete all problematic workspaces
    Prune {
        /// Actually delete workspaces (default: preview only)
        #[arg(long)]
        force: bool,

        /// Also prune workspaces with no changes (empty working copies)
        #[arg(long)]
        empty: bool,
    },

    /// Reconnect an orphaned workspace directory to jj's tracking
    ///
    /// Use this to recover a workspace where 'jj workspace forget' was run
    /// but the directory still exists. Re-adds the workspace to jj's tracking.
    ///
    /// Examples:
    ///   maw ws attach orphaned               # reconnect orphaned directory
    ///   maw ws attach orphaned -r main       # attach at specific revision
    Attach {
        /// Name of the workspace directory to attach
        name: String,

        /// Revision to attach the workspace to (default: main or @)
        #[arg(short, long)]
        revision: Option<String>,
    },

    /// Merge work from workspaces into default
    ///
    /// Creates a merge commit combining work from the specified workspaces.
    /// Works with one or more workspaces. Stale workspaces are automatically
    /// synced before merge to avoid spurious conflicts. After merge, check
    /// output for undescribed commits (commits with no message) that may block push.
    ///
    /// Examples:
    ///   maw ws merge alice                 # adopt alice's work
    ///   maw ws merge alice bob             # merge alice and bob's work
    ///   maw ws merge alice bob --destroy   # merge and clean up (non-interactive)
    ///   maw ws merge alice bob --dry-run   # preview merge without committing
    ///   maw ws merge alice bob --auto-describe  # auto-describe empty scaffolding commits
    Merge {
        /// Workspace names to merge
        #[arg(required = true)]
        workspaces: Vec<String>,

        /// Destroy workspaces after successful merge (non-interactive by default)
        #[arg(long)]
        destroy: bool,

        /// Prompt for confirmation before destroying (use with --destroy)
        #[arg(short, long)]
        confirm: bool,

        /// Custom merge commit message
        #[arg(short, long)]
        message: Option<String>,

        /// Preview the merge without creating any commits
        #[arg(long)]
        dry_run: bool,

        /// Automatically describe empty commits as 'workspace setup'
        #[arg(long)]
        auto_describe: bool,
    },
}

pub fn run(cmd: WorkspaceCommands) -> Result<()> {
    match cmd {
        WorkspaceCommands::Create {
            name,
            random,
            revision,
        } => {
            let name = if random {
                names::generate_workspace_name()
            } else {
                name.expect("name is required unless --random is set")
            };
            create::create(&name, revision.as_deref())
        }
        WorkspaceCommands::Destroy { name, confirm } => create::destroy(&name, confirm),
        WorkspaceCommands::Restore { name } => restore::restore(&name),
        WorkspaceCommands::List { verbose, format, json } => list::list(verbose, OutputFormat::resolve(OutputFormat::with_json_flag(format, json))),
        WorkspaceCommands::Status { format, json } => status::status(OutputFormat::resolve(OutputFormat::with_json_flag(format, json))),
        WorkspaceCommands::Sync { all } => sync::sync(all),
        WorkspaceCommands::Jj { name, args } => {
            let args_str = args.join(" ");
            bail!(
                "`maw ws jj` is deprecated.\n  \
                 Use: maw exec {name} -- jj {args_str}"
            );
        }
        WorkspaceCommands::History { name, limit, format, json } => history::history(&name, limit, OutputFormat::with_json_flag(format, json)),
        WorkspaceCommands::Prune { force, empty } => prune::prune(force, empty),
        WorkspaceCommands::Attach { name, revision } => create::attach(&name, revision.as_deref()),
        WorkspaceCommands::Merge {
            workspaces,
            destroy,
            confirm,
            message,
            dry_run,
            auto_describe: _,
        } => merge::merge(&workspaces, &merge::MergeOptions {
            destroy_after: destroy,
            confirm,
            message: message.as_deref(),
            dry_run,
        }),
    }
}

pub fn repo_root() -> Result<PathBuf> {
    let output = Command::new("jj")
        .args(["root"])
        .output()
        .context("Failed to run jj root")?;
    if !output.status.success() {
        bail!(
            "jj root failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());

    // jj root returns the workspace root, not the repo root.
    // If we're inside a workspace (ws/<name>/), walk up
    // to the directory containing ws/.
    for ancestor in root.ancestors() {
        if ancestor.file_name().is_some_and(|n| n == "ws")
            && let Some(parent) = ancestor.parent() {
                return Ok(parent.to_path_buf());
            }
    }

    Ok(root)
}

/// Return the best directory for running jj commands.
///
/// In v2 bare repo model, the repo root has no jj workspace -- running jj
/// there produces "working copy is stale" errors. This returns `ws/default/`
/// when it exists, falling back to the repo root for v1 repos or pre-init.
pub fn jj_cwd() -> Result<PathBuf> {
    let root = repo_root()?;
    let default_ws = root.join("ws").join("default");
    if default_ws.exists() {
        Ok(default_ws)
    } else {
        Ok(root)
    }
}

/// Ensure CWD is the repo root. Mutation commands must run from root
/// to avoid agent confusion about which workspace context they're in.
fn ensure_repo_root() -> Result<PathBuf> {
    let root = repo_root()?;
    let cwd = std::env::current_dir().context("Could not determine current directory")?;

    // Canonicalize both for reliable comparison (handles symlinks, ..)
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.clone());
    let cwd_canon = cwd.canonicalize().unwrap_or_else(|_| cwd.clone());

    if cwd_canon != root_canon {
        bail!(
            "This command must be run from the repo root.\n\
             \n  You are in: {}\n  Repo root:  {}\n\
             \n  Run from repo root, or use: maw exec <workspace> -- <command>",
            cwd.display(),
            root.display()
        );
    }

    Ok(root)
}

fn workspaces_dir() -> Result<PathBuf> {
    Ok(repo_root()?.join("ws"))
}

pub fn workspace_path(name: &str) -> Result<PathBuf> {
    validate_workspace_name(name)?;
    Ok(workspaces_dir()?.join(name))
}

/// Validate workspace name to prevent path traversal and command injection
fn validate_workspace_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Workspace name cannot be empty");
    }

    if name.starts_with('-') {
        bail!("Workspace name cannot start with '-' (would be interpreted as a flag)");
    }

    if name == "." || name == ".." {
        bail!("Workspace name cannot be '.' or '..'");
    }

    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        bail!("Workspace name cannot contain path separators or null bytes");
    }

    // Only allow alphanumeric, hyphen, underscore
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!(
            "Workspace name must contain only letters, numbers, hyphens, and underscores\n\
             Got: '{name}'"
        );
    }

    Ok(())
}

/// Check all workspaces for staleness and return list of stale workspace names.
/// A workspace is stale when another workspace modified shared history,
/// making the working copy files outdated.
fn check_stale_workspaces() -> Result<Vec<String>> {
    let cwd = jj_cwd()?;
    let ws_dir = workspaces_dir()?;

    // Get all workspaces
    let output = run_jj(&["workspace", "list"], &cwd)?;

    let ws_list = String::from_utf8_lossy(&output.stdout);

    // Parse workspace names
    let workspace_names: Vec<String> = ws_list
        .lines()
        .filter_map(|l| l.split(':').next())
        .map(|s| s.trim().trim_end_matches('@').to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let mut stale = Vec::new();

    for ws in &workspace_names {
        // Validate workspace name (defense-in-depth)
        if validate_workspace_name(ws).is_err() {
            continue;
        }

        let path = ws_dir.join(ws);

        if !path.exists() {
            continue;
        }

        // Check if stale by looking at jj status stderr
        let status = Command::new("jj")
            .args(["status"])
            .current_dir(&path)
            .output();

        if let Ok(out) = status {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("working copy is stale") {
                stale.push(ws.clone());
            }
        }
    }

    Ok(stale)
}
