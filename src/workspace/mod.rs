use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde::Deserialize;

use crate::backend::platform;
use crate::backend::{AnyBackend, WorkspaceBackend};
use crate::config::{BackendKind, ManifoldConfig};
use crate::format::OutputFormat;

mod advance;
mod create;
mod diff;
mod history;
mod list;
mod merge;
mod metadata;
mod names;
mod oplog_runtime;
mod overlap;
mod prune;
mod restore;
mod status;
pub mod sync;
mod templates;
mod touched;
mod undo;

// Re-export public API used by other modules
pub use sync::auto_sync_if_stale;

/// Default workspace name -- the persistent workspace used for merging,
/// pushing, and coordination. Lives at ws/default/ in the bare repo model.
const DEFAULT_WORKSPACE: &str = "default";

/// Configuration from .maw.toml
#[derive(Debug, Default, Deserialize)]
pub struct MawConfig {
    #[serde(default)]
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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
    /// Creates an isolated workspace in ws/<name>/ using git worktrees.
    /// Each workspace has its own working copy (a separate view of the
    /// codebase). All file reads, writes, and edits must use the absolute
    /// workspace path shown after creation.
    ///
    /// After creation:
    ///   1. Edit files under ws/<name>/ (use absolute paths)
    ///   2. Run commands: maw exec <name> -- <command>
    ///   3. Changes are captured automatically during merge
    ///
    /// WORKSPACE MODES:
    ///   Ephemeral (default): created from current epoch; must be merged or
    ///   destroyed before the next epoch advance. Common for short-lived tasks.
    ///
    ///   Persistent (--persistent): can survive across epoch advances. Use
    ///   `maw ws advance <name>` to rebase onto the latest epoch when stale.
    ///   Suitable for long-running agent tasks that span multiple epochs.
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

        /// Create a persistent workspace that can survive across epoch advances.
        ///
        /// Persistent workspaces are not destroyed by epoch advancement and
        /// support `maw ws advance <name>` to rebase onto newer epochs.
        /// Staleness is shown in `maw ws list` and `maw ws status`.
        #[arg(long)]
        persistent: bool,

        /// Apply a workspace archetype template (feature, bugfix, refactor, eval, release).
        ///
        /// Templates emit machine-readable defaults in workspace metadata and
        /// `.manifold/workspace-template.json` inside the workspace.
        #[arg(long, value_enum)]
        template: Option<templates::WorkspaceTemplate>,
    },

    /// Remove a workspace
    ///
    /// Removes the workspace: removes the git worktree and deletes the
    /// directory. Merge any important changes first (maw ws merge).
    ///
    /// Non-interactive by default (agents can't respond to prompts).
    /// Use --confirm for interactive confirmation.
    ///
    /// If the workspace has unmerged changes, destroy is refused by default.
    /// Use --force to discard those changes.
    ///
    /// To undo: maw ws restore <name>
    Destroy {
        /// Name of the workspace to destroy
        name: String,

        /// Prompt for confirmation before destroying
        #[arg(short, long)]
        confirm: bool,

        /// Force destroy even if workspace has unmerged local changes
        #[arg(long)]
        force: bool,
    },

    /// Restore a previously destroyed workspace
    ///
    /// Recreates a workspace that was removed with 'maw ws destroy'.
    /// Creates a fresh workspace at the current epoch. Previous working
    /// copy changes are not automatically restored.
    ///
    /// Examples:
    ///   maw ws restore alice    # recreate alice's workspace
    Restore {
        /// Name of the workspace to restore
        name: String,
    },

    /// List all workspaces
    ///
    /// Shows all workspaces with their current status including:
    /// - Epoch (base commit) and staleness state
    /// - Whether the workspace is stale (behind current epoch)
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

    /// List paths touched by a workspace's local changes
    ///
    /// Uses `PatchSet` diffing against the workspace's base epoch and returns
    /// a conservative set of touched paths for conflict prediction.
    /// For renames, both source and destination paths are included.
    ///
    /// Examples:
    ///   maw ws touched alice
    ///   maw ws touched alice --format json
    Touched {
        /// Workspace name
        workspace: String,

        /// Output format: text, json, or pretty
        #[arg(long)]
        format: Option<OutputFormat>,

        /// Shorthand for --format json
        #[arg(long, hide = true, conflicts_with = "format")]
        json: bool,
    },

    /// Show differences for a workspace against default/epoch/branch/revision
    ///
    /// By default compares `<workspace>` against the default workspace state.
    /// Use `--against` to compare against:
    /// - `default` (default behavior)
    /// - `epoch` (refs/manifold/epoch/current)
    /// - `branch:<name>` (e.g., branch:main)
    /// - `oid:<sha>` (or bare sha)
    ///
    /// Formats:
    /// - summary: concise file/status list with counts (default)
    /// - patch: unified diff output
    /// - json: machine-readable metadata + file-level stats
    ///
    /// Examples:
    ///   maw ws diff alice
    ///   maw ws diff alice --against epoch --format json
    ///   maw ws diff alice --against branch:main --format patch
    ///   maw ws diff alice --name-only
    Diff {
        /// Workspace to inspect
        workspace: String,

        /// Compare target: default, epoch, branch:<name>, oid:<sha>, or bare <sha>
        #[arg(long)]
        against: Option<String>,

        /// Output format: summary, patch, or json
        #[arg(long, value_enum, default_value = "summary")]
        format: diff::DiffFormat,

        /// Show changed paths only (one path per line)
        #[arg(long)]
        name_only: bool,

        /// Comma-separated glob filters (e.g., src/**,README*)
        #[arg(long, value_delimiter = ',')]
        paths: Vec<String>,
    },

    /// Predict overlap risk between two workspaces
    ///
    /// Computes touched-path sets for each workspace and reports the
    /// path intersection as a conservative conflict-risk signal.
    ///
    /// Examples:
    ///   maw ws overlap alice bob
    ///   maw ws overlap alice bob --format json
    Overlap {
        /// First workspace name
        ws1: String,

        /// Second workspace name
        ws2: String,

        /// Output format: text, json, or pretty
        #[arg(long)]
        format: Option<OutputFormat>,

        /// Shorthand for --format json
        #[arg(long, hide = true, conflicts_with = "format")]
        json: bool,
    },

    /// Sync workspace with repository (handle stale working copy)
    ///
    /// Run this at the start of every session. If the working copy is stale
    /// (behind the current epoch), this updates your workspace to match.
    /// Safe to run even if not stale.
    ///
    /// Use --all to sync all workspaces at once, useful after epoch
    /// advancement or when multiple workspaces may be stale.
    Sync {
        /// Sync all workspaces instead of just the current one
        #[arg(long)]
        all: bool,
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

    /// Undo local workspace changes via a compensation operation
    ///
    /// Reverts the workspace's unmerged changes back to its base epoch.
    /// Records a `Compensate` operation in the workspace op log so undo
    /// actions are durable and visible in history.
    ///
    /// Examples:
    ///   maw ws undo alice
    Undo {
        /// Name of the workspace to undo
        name: String,
    },

    /// Clean up orphaned, stale, or empty workspaces
    ///
    /// Detects problematic workspaces:
    /// - Orphaned: directory exists in ws/ but not tracked as worktree
    /// - Missing: git tracks the worktree but the directory is gone
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

    /// Reconnect an orphaned workspace directory as a git worktree
    ///
    /// Use this to recover a workspace where the worktree tracking was lost
    /// but the directory still exists. Re-creates the worktree entry.
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

    /// Rebase a persistent workspace onto the latest epoch
    ///
    /// When the mainline epoch advances (a merge is committed), persistent
    /// workspaces become stale. Use this command to rebase the workspace's
    /// uncommitted changes onto the new epoch.
    ///
    /// Only works for workspaces created with `--persistent`. Ephemeral
    /// workspaces should be merged or destroyed instead.
    ///
    /// The advance operation:
    ///   1. Stashes any uncommitted changes in the workspace
    ///   2. Updates the workspace HEAD to the new epoch
    ///   3. Re-applies the stashed changes
    ///   4. Reports any conflicts as structured data
    ///
    /// Examples:
    ///   maw ws advance my-agent              # advance to latest epoch
    ///   maw ws advance my-agent --format json  # machine-parseable output
    ///
    /// On conflict, the workspace is left with conflict markers for manual
    /// resolution. Resolve conflicts in the workspace files, then continue.
    Advance {
        /// Name of the persistent workspace to advance
        name: String,

        /// Output format: text, json, or pretty
        ///
        /// With --format json, emits structured JSON including conflict details
        /// (file paths, conflict types) for automated conflict handling.
        #[arg(long)]
        format: Option<OutputFormat>,

        /// Shorthand for --format json
        #[arg(long, hide = true, conflicts_with = "format")]
        json: bool,
    },

    /// Merge work from workspaces into default
    ///
    /// Creates a merge commit combining work from the specified workspaces.
    /// Works with one or more workspaces. Stale workspaces are automatically
    /// synced before merge to avoid spurious conflicts. After merge, check
    /// output for undescribed commits (commits with no message) that may block push.
    ///
    /// Use --check for a dry-run conflict detection that undoes itself:
    ///   exit 0 = safe to merge, non-zero = blocked (conflicts/stale).
    ///   Combine with --format json for structured output.
    ///
    /// Use --format json to receive structured output for success and conflict
    /// cases. On conflict, the JSON includes per-file conflict details with
    /// workspace attribution, base content, and resolution strategies.
    ///
    /// Examples:
    ///   maw ws merge alice                       # adopt alice's work
    ///   maw ws merge alice bob                   # merge alice and bob's work
    ///   maw ws merge alice bob --destroy         # merge and clean up (non-interactive)
    ///   maw ws merge alice bob --dry-run         # preview merge without committing
    ///   maw ws merge alice bob --plan      # deterministic merge plan (no commit)
    ///   maw ws merge alice bob --plan --json  # machine-parseable plan JSON
    ///   maw ws merge alice --check               # pre-flight: can we merge cleanly?
    ///   maw ws merge alice --check --format json # structured check result
    ///   maw ws merge alice --format json         # structured merge result (success or conflict)
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

        /// Run PREPARE+BUILD+VALIDATE but stop before COMMIT.
        /// Writes deterministic plan to .manifold/artifacts/ and prints a summary.
        /// Combine with --json for machine-parseable output.
        #[arg(long, conflicts_with = "dry_run", conflicts_with = "check")]
        plan: bool,

        /// Pre-flight check: trial rebase to detect conflicts, then undo.
        /// Exit 0 = safe to merge, non-zero = blocked.
        #[arg(long)]
        check: bool,

        /// Output format: text, json, or pretty.
        ///
        /// With --json / --format json: emits structured JSON for both success
        /// and conflict cases. Conflict output includes per-file details with
        /// workspace attribution, base content, sides, localized atoms, and
        /// resolution strategies.
        #[arg(long)]
        format: Option<OutputFormat>,

        /// Shorthand for --format json
        #[arg(long, hide = true, conflicts_with = "format")]
        json: bool,
    },

    /// Show detailed conflict information for workspace(s)
    ///
    /// Runs the merge engine's PREPARE + BUILD phases to detect conflicts
    /// and outputs structured data — without committing anything.
    ///
    /// Useful for agents to inspect conflicts before deciding how to resolve
    /// them. Each conflict includes:
    ///   - File path and conflict type (content, add/add, modify/delete)
    ///   - Each workspace's contribution (change kind + content)
    ///   - Base (common ancestor) content for reference
    ///   - Localized conflict atoms (exact line ranges / AST regions)
    ///   - Suggested resolution strategies
    ///
    /// Examples:
    ///   maw ws conflicts alice                  # show conflicts for alice workspace
    ///   maw ws conflicts alice bob              # show conflicts across both workspaces
    ///   maw ws conflicts alice --format json    # machine-parseable output for agents
    Conflicts {
        /// Workspace names to check for conflicts
        #[arg(required = true)]
        workspaces: Vec<String>,

        /// Output format: text, json, or pretty
        #[arg(long)]
        format: Option<OutputFormat>,

        /// Shorthand for --format json
        #[arg(long, hide = true, conflicts_with = "format")]
        json: bool,
    },
}

#[allow(clippy::too_many_lines)]
pub fn run(cmd: WorkspaceCommands) -> Result<()> {
    match cmd {
        WorkspaceCommands::Create {
            name,
            random,
            revision,
            persistent,
            template,
        } => {
            let name = if random {
                names::generate_workspace_name()
            } else {
                name.expect("name is required unless --random is set")
            };
            create::create(&name, revision.as_deref(), persistent, template)
        }
        WorkspaceCommands::Destroy {
            name,
            confirm,
            force,
        } => create::destroy(&name, confirm, force),
        WorkspaceCommands::Restore { name } => restore::restore(&name),
        WorkspaceCommands::List {
            verbose,
            format,
            json,
        } => list::list(
            verbose,
            OutputFormat::resolve(OutputFormat::with_json_flag(format, json)),
        ),
        WorkspaceCommands::Status { format, json } => status::status(OutputFormat::resolve(
            OutputFormat::with_json_flag(format, json),
        )),
        WorkspaceCommands::Touched {
            workspace,
            format,
            json,
        } => {
            let fmt = OutputFormat::resolve(OutputFormat::with_json_flag(format, json));
            touched::touched(&workspace, fmt)
        }
        WorkspaceCommands::Diff {
            workspace,
            against,
            format,
            name_only,
            paths,
        } => diff::diff(&workspace, against.as_deref(), format, name_only, &paths),
        WorkspaceCommands::Overlap {
            ws1,
            ws2,
            format,
            json,
        } => {
            let fmt = OutputFormat::resolve(OutputFormat::with_json_flag(format, json));
            overlap::overlap(&ws1, &ws2, fmt)
        }
        WorkspaceCommands::Sync { all } => sync::sync(all),
        WorkspaceCommands::History {
            name,
            limit,
            format,
            json,
        } => history::history(&name, limit, OutputFormat::with_json_flag(format, json)),
        WorkspaceCommands::Undo { name } => undo::undo(&name),
        WorkspaceCommands::Prune { force, empty } => prune::prune(force, empty),
        WorkspaceCommands::Attach { name, revision } => create::attach(&name, revision.as_deref()),
        WorkspaceCommands::Advance { name, format, json } => {
            let fmt = OutputFormat::resolve(OutputFormat::with_json_flag(format, json));
            advance::advance(&name, fmt)
        }
        WorkspaceCommands::Merge {
            workspaces,
            destroy,
            confirm,
            message,
            dry_run,
            auto_describe: _,
            plan,
            check,
            format,
            json,
        } => {
            let fmt = OutputFormat::resolve(OutputFormat::with_json_flag(format, json));
            if check {
                return merge::check_merge(&workspaces, fmt);
            }
            if plan {
                return merge::plan_merge(&workspaces, fmt);
            }
            merge::merge(
                &workspaces,
                &merge::MergeOptions {
                    destroy_after: destroy,
                    confirm,
                    message: message.as_deref(),
                    dry_run,
                    format: fmt,
                },
            )
        }
        WorkspaceCommands::Conflicts {
            workspaces,
            format,
            json,
        } => {
            let fmt = OutputFormat::resolve(OutputFormat::with_json_flag(format, json));
            merge::show_conflicts(&workspaces, fmt)
        }
    }
}

pub fn repo_root() -> Result<PathBuf> {
    // First preference: ask git for its common-dir. This is the authoritative
    // answer from any worktree context — git always knows its own common-dir,
    // whether we're at the repo root, inside ws/alice/, or deep in a subtree.
    // The ancestor walk is tried second because it can be fooled: ws/<name>/
    // has a .git gitfile, so if .manifold/ was accidentally created inside a
    // workspace (e.g. by running `maw init` from inside one), the walk would
    // incorrectly return the workspace directory as the repo root.
    let output = Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output();
    if let Ok(output) = output {
        if output.status.success() {
            let common_dir = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
            let mut root = common_dir
                .parent()
                .context("Cannot determine repo root from git common dir")?
                .to_path_buf();

            // Support nested common-dir layouts like <root>/.manifold/git.
            if root.file_name().is_some_and(|name| name == ".manifold") {
                root = root
                    .parent()
                    .context("Cannot determine repo root from nested common dir")?
                    .to_path_buf();
            }

            // repo.git layout: common-dir is <root>/repo.git, so parent is <root>.
            // Standard layout: common-dir is <root>/.git, so parent is <root>.
            // Both cases give us the correct root directly.
            return Ok(root);
        }
    }

    // Fallback: ancestor walk for Manifold markers. Used when git is not
    // available or not in a git repo at all.
    let cwd = std::env::current_dir().context("Could not determine current directory")?;
    if let Some(root) = cwd.ancestors().find(|dir| {
        dir.join(".manifold").is_dir() && (dir.join("ws").is_dir() || dir.join(".git").exists())
    }) {
        return Ok(root.to_path_buf());
    }

    bail!(
        "Not in a Manifold repository. Run `maw init` to initialize one.\n  \
         Current directory: {}",
        cwd.display()
    )
}

/// Return the best directory for running git commands.
///
/// In v2 bare repo model, the repo root has no workspace. This returns
/// `ws/default/` when it exists, falling back to the repo root.
pub fn git_cwd() -> Result<PathBuf> {
    let root = repo_root()?;
    let default_ws = root.join("ws").join("default");
    if default_ws.exists() {
        Ok(default_ws)
    } else {
        Ok(root)
    }
}

/// Resolve the workspace backend from `.manifold/config.toml` and platform capabilities.
///
/// Auto-selects the best backend for the current platform and repo size (§7.5).
/// Falls back to `git-worktree` if detection fails or no `CoW` backend is available.
pub fn get_backend() -> Result<AnyBackend> {
    let root = repo_root()?;

    // Load `.manifold/config.toml` (missing file → all defaults).
    let manifold_config_path = root.join(".manifold").join("config.toml");
    let manifold_config = ManifoldConfig::load(&manifold_config_path).unwrap_or_default();
    let configured_kind = manifold_config.workspace.backend;

    // Detect platform capabilities (cached in .manifold/platform-capabilities).
    let caps = platform::detect_or_load(&root);

    // Estimate repo file count for threshold-based selection.
    let file_count = platform::estimate_repo_file_count(&root).unwrap_or(0);

    // Resolve the concrete backend kind (auto → specific).
    let resolved = platform::resolve_backend_kind(configured_kind, file_count, &caps);

    // Construct and return the backend.
    AnyBackend::from_kind(resolved, root).or_else(|e| {
        // If the resolved backend fails to initialize (e.g., overlay not
        // available despite detection), fall back to git-worktree and warn.
        eprintln!("WARNING: Backend init failed ({e}), falling back to git-worktree");
        AnyBackend::from_kind(BackendKind::GitWorktree, repo_root()?)
    })
}

/// Ensure CWD is the repo root. Mutation commands must run from root
/// to avoid agent confusion about which workspace context they're in.
fn ensure_repo_root() -> Result<PathBuf> {
    let root = repo_root()?;
    let cwd = std::env::current_dir().context("Could not determine current directory")?;

    // Canonicalize both for reliable comparison (handles symlinks, ..)
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.clone());
    let cwd_canon = cwd.canonicalize().unwrap_or_else(|_| cwd.clone());

    // Allow running from repo root or any subdirectory (e.g. ws/default/).
    // This lets agents use `maw exec default -- maw ws create <name>` or
    // run maw commands from inside a workspace directory.
    if !cwd_canon.starts_with(&root_canon) {
        bail!(
            "This command must be run from within the repo.\n\
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
/// A workspace is stale when its base epoch differs from the current epoch.
fn check_stale_workspaces() -> Result<Vec<String>> {
    let backend = get_backend()?;
    let workspaces = backend.list().map_err(|e| anyhow::anyhow!("{e}"))?;

    let stale: Vec<String> = workspaces
        .iter()
        .filter(|ws| ws.state.is_stale())
        .map(|ws| ws.id.as_str().to_string())
        .collect();

    Ok(stale)
}
