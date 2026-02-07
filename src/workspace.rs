use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use glob::Pattern;
use rand::seq::IndexedRandom;
use serde::{Deserialize, Serialize};

use crate::format::OutputFormat;

/// Default workspace name — the persistent workspace used for merging,
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
struct HooksConfig {
    /// Commands to run before merge. Merge aborts if any command fails (non-zero exit).
    #[serde(default)]
    pre_merge: Vec<String>,
    /// Commands to run after merge. Warnings are shown on failure but don't abort.
    #[serde(default)]
    post_merge: Vec<String>,
}

/// Merge-specific configuration
#[derive(Debug, Default, Deserialize)]
struct MergeConfig {
    /// Paths to auto-resolve from main during merge conflicts.
    /// Supports glob patterns like ".beads/**".
    #[serde(default)]
    auto_resolve_from_main: Vec<String>,
}

impl MawConfig {
    /// Load config from .maw.toml.
    ///
    /// Checks repo root first, then falls back to ws/default/.maw.toml
    /// (in bare repos, root has no tracked files — config lives in workspaces).
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

#[derive(Serialize)]
struct WorkspaceInfo {
    name: String,
    is_current: bool,
    is_default: bool,
    change_id: String,
    commit_id: String,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

#[derive(Serialize)]
struct WorkspaceStatus {
    current_workspace: String,
    is_stale: bool,
    has_changes: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    changes: Option<String>,
    workspaces: Vec<WorkspaceEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    conflicts: Vec<ConflictInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    divergent_commits: Vec<DivergentCommitInfo>,
}

#[derive(Serialize)]
struct WorkspaceEntry {
    name: String,
    is_current: bool,
    info: String,
}

#[derive(Serialize)]
struct ConflictInfo {
    change_id: String,
    description: String,
}

#[derive(Serialize)]
struct DivergentCommitInfo {
    change_id: String,
    commit_id: String,
    description: String,
}

const ADJECTIVES: &[&str] = &[
    "blue", "green", "red", "gold", "silver", "swift", "brave", "calm", "wild", "bold", "keen",
    "wise", "silent", "fierce", "noble", "cosmic", "crystal", "electric", "frozen", "iron",
    "lunar", "mystic", "northern", "radiant", "shadow", "ember", "frost", "storm", "stellar",
    "amber",
];

const NOUNS: &[&str] = &[
    "castle", "forest", "river", "mountain", "eagle", "wolf", "phoenix", "falcon", "hawk",
    "raven", "tiger", "bear", "beacon", "forge", "gateway", "kernel", "oracle", "sentinel",
    "tower", "fox", "owl", "panther", "viper", "crane", "otter", "lynx", "cedar", "oak", "pine",
    "reef",
];

fn generate_workspace_name() -> String {
    let mut rng = rand::rng();
    let adj = ADJECTIVES.choose(&mut rng).unwrap_or(&"swift");
    let noun = NOUNS.choose(&mut rng).unwrap_or(&"agent");
    format!("{adj}-{noun}")
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
    ///   2. Save work: maw ws jj <name> describe -m "feat: ..."
    ///      ('describe' sets the commit message — like git commit --amend -m)
    ///   3. Run other commands: cd /abs/path/ws/<name> && cmd
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
    Destroy {
        /// Name of the workspace to destroy
        name: String,

        /// Prompt for confirmation before destroying
        #[arg(short, long)]
        confirm: bool,
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

        /// Output format: toon (default), json, or text
        #[arg(long, default_value = "toon")]
        format: OutputFormat,
    },

    /// Show status of current workspace and all agent work
    ///
    /// Displays a comprehensive view of:
    /// - Current workspace state (changes, stale status)
    /// - All agent workspaces and their commits
    /// - Any conflicts that need resolution
    /// - Unmerged work across all workspaces
    Status {
        /// Output format: toon (default), json, or text
        #[arg(long, default_value = "toon")]
        format: OutputFormat,
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

    /// Run a jj command in a workspace
    ///
    /// Use this instead of 'cd ws/<name> && jj ...'.
    /// Required in sandboxed environments where cd doesn't persist
    /// between tool calls. Only runs jj — not arbitrary commands.
    ///
    /// Examples:
    ///   maw ws jj alice describe -m "feat: new feature"
    ///   maw ws jj alice diff
    ///   maw ws jj alice log
    Jj {
        /// Workspace name to run jj in
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
                generate_workspace_name()
            } else {
                name.expect("name is required unless --random is set")
            };
            create(&name, revision.as_deref())
        }
        WorkspaceCommands::Destroy { name, confirm } => destroy(&name, confirm),
        WorkspaceCommands::List { verbose, format } => list(verbose, format),
        WorkspaceCommands::Status { format } => status(format),
        WorkspaceCommands::Sync { all } => sync(all),
        WorkspaceCommands::Jj { name, args } => jj_in_workspace(&name, &args),
        WorkspaceCommands::History { name, limit } => history(&name, limit),
        WorkspaceCommands::Prune { force, empty } => prune(force, empty),
        WorkspaceCommands::Attach { name, revision } => attach(&name, revision.as_deref()),
        WorkspaceCommands::Merge {
            workspaces,
            destroy,
            confirm,
            message,
            dry_run,
            auto_describe,
        } => merge(&workspaces, destroy, confirm, message.as_deref(), dry_run, auto_describe),
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
        if ancestor.file_name().map_or(false, |n| n == "ws") {
            if let Some(parent) = ancestor.parent() {
                return Ok(parent.to_path_buf());
            }
        }
    }

    Ok(root)
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
             \n  Run: cd {} && maw ...",
            cwd.display(),
            root.display(),
            root.display()
        );
    }

    Ok(root)
}

fn workspaces_dir() -> Result<PathBuf> {
    Ok(repo_root()?.join("ws"))
}

pub(crate) fn workspace_path(name: &str) -> Result<PathBuf> {
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
    let root = repo_root()?;
    let ws_dir = workspaces_dir()?;

    // Get all workspaces
    let output = Command::new("jj")
        .args(["workspace", "list"])
        .current_dir(&root)
        .output()
        .context("Failed to run jj workspace list")?;

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

/// Print warning about stale workspaces if any are detected.
fn warn_stale_workspaces() {
    match check_stale_workspaces() {
        Ok(stale) if !stale.is_empty() => {
            eprintln!();
            eprintln!(
                "WARNING: {} workspace(s) stale: {}",
                stale.len(),
                stale.join(", ")
            );
            eprintln!("  Fix: maw ws sync --all");
        }
        _ => {}
    }
}

fn create(name: &str, revision: Option<&str>) -> Result<()> {
    let root = ensure_repo_root()?;
    let path = workspace_path(name)?;

    if path.exists() {
        bail!("Workspace already exists at {}", path.display());
    }

    // Ensure ws directory exists
    let ws_dir = workspaces_dir()?;
    std::fs::create_dir_all(&ws_dir)
        .with_context(|| format!("Failed to create {}", ws_dir.display()))?;

    println!("Creating workspace '{name}' at ws/{name} ...");

    // Determine base revision.
    // In v2 bare model, the default workspace is at ws/default/, not root.
    // @ can't resolve from root (no workspace there), so fall back to the
    // configured branch name (e.g. "main").
    let base = if let Some(rev) = revision {
        rev.to_string()
    } else {
        let check = Command::new("jj")
            .args(["log", "-r", "@", "--no-graph", "-T", "change_id.short()", "--no-pager"])
            .current_dir(&root)
            .output();
        match check {
            Ok(o) if o.status.success() => "@".to_string(),
            _ => {
                let config = MawConfig::load(&root).unwrap_or_default();
                config.branch().to_string()
            }
        }
    };

    // Create the workspace
    let output = Command::new("jj")
        .args([
            "workspace",
            "add",
            path.to_str().unwrap(),
            "--name",
            name,
            "-r",
            &base,
        ])
        .current_dir(&root)
        .output()
        .context("Failed to run jj workspace add")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "jj workspace add failed: {}\n  Check: maw doctor\n  Verify name is not already used: maw ws list",
            stderr.trim()
        );
    }

    // Create a dedicated commit for this agent to own
    // This prevents divergent commits when multiple agents work concurrently
    let new_output = Command::new("jj")
        .args(["new", "-m", &format!("wip: {name} workspace")])
        .current_dir(&path)
        .output()
        .context("Failed to create agent commit")?;

    if !new_output.status.success() {
        let stderr = String::from_utf8_lossy(&new_output.stderr);
        bail!(
            "Failed to create dedicated commit for workspace: {}\n  The workspace was created but has no dedicated commit.\n  Try: cd {} && jj new -m \"wip: {name}\"",
            stderr.trim(),
            path.display()
        );
    }

    // Get the new commit's change ID for display
    let change_id = Command::new("jj")
        .args(["log", "-r", "@", "--no-graph", "-T", "change_id.short()"])
        .current_dir(&path)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    println!();
    println!("Workspace '{name}' ready!");
    println!();
    println!("  Commit: {change_id} (your dedicated change — jj's stable ID for this commit)");
    println!("  Path:   {}/", path.display());
    println!();
    println!("  IMPORTANT: All file reads, writes, and edits must use this path.");
    println!("  This is your working directory for ALL operations, not just bash.");
    println!();
    println!("To start working:");
    println!();
    println!("  # Set your commit message (like git commit --amend -m):");
    println!("  maw ws jj {name} describe -m \"feat: what you're implementing\"");
    println!();
    println!("  # View changes (like git diff / git log):");
    println!("  maw ws jj {name} diff");
    println!("  maw ws jj {name} log");
    println!();
    println!("  # Other commands (use absolute workspace path):");
    println!("  cd {}/ && cargo test", path.display());
    println!();
    println!("Note: jj has no staging area — all edits are tracked automatically.");
    println!("Your changes are always in your commit. Use 'describe' to set the message.");

    Ok(())
}

fn destroy(name: &str, confirm: bool) -> Result<()> {
    if name == DEFAULT_WORKSPACE {
        bail!("Cannot destroy the default workspace");
    }
    // Also check config in case default_workspace is customized
    if let Ok(root) = repo_root() {
        if let Ok(config) = MawConfig::load(&root) {
            if name == config.default_workspace() {
                bail!("Cannot destroy the default workspace");
            }
        }
    }

    ensure_repo_root()?;
    let path = workspace_path(name)?;

    if !path.exists() {
        bail!("Workspace does not exist at {}", path.display());
    }

    if confirm {
        println!("About to destroy workspace '{name}' at {}", path.display());
        println!("This will forget the workspace and delete the directory.");
        println!();
        print!("Continue? [y/N] ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    println!("Destroying workspace '{name}'...");

    // Forget from jj (ignore errors if already forgotten)
    let _ = Command::new("jj")
        .args(["workspace", "forget", name])
        .status();

    // Remove directory
    std::fs::remove_dir_all(&path)
        .with_context(|| format!("Failed to remove {}", path.display()))?;

    println!("Workspace destroyed.");
    Ok(())
}

/// Attach (reconnect) an orphaned workspace directory to jj's tracking.
/// An orphaned workspace is one where 'jj workspace forget' was run but
/// the directory still exists in ws/.
fn attach(name: &str, revision: Option<&str>) -> Result<()> {
    if name == DEFAULT_WORKSPACE {
        bail!("Cannot attach the default workspace (it's always tracked)");
    }

    ensure_repo_root()?;
    let root = repo_root()?;
    let path = workspace_path(name)?;

    // Check if directory exists
    if !path.exists() {
        bail!(
            "Workspace directory does not exist at {}\n  \
             The directory must exist to attach it.\n  \
             To create a new workspace: maw ws create {name}",
            path.display()
        );
    }

    // Check if workspace is already tracked by jj
    let ws_output = Command::new("jj")
        .args(["workspace", "list"])
        .current_dir(&root)
        .output()
        .context("Failed to run jj workspace list")?;

    let ws_list = String::from_utf8_lossy(&ws_output.stdout);
    let is_tracked = ws_list.lines().any(|line| {
        line.split(':')
            .next()
            .map(|n| n.trim().trim_end_matches('@') == name)
            .unwrap_or(false)
    });

    if is_tracked {
        bail!(
            "Workspace '{name}' is already tracked by jj.\n  \
             Use 'maw ws sync' if the workspace is stale.\n  \
             Use 'maw ws list' to see all workspaces."
        );
    }

    // Determine the revision to attach to (user-specified or default to configured branch)
    let config = MawConfig::load(&root)?;
    let attach_rev = revision.map_or_else(|| config.branch().to_string(), ToString::to_string);

    println!("Attaching workspace '{name}' at revision {attach_rev}...");

    // jj workspace add requires an empty directory, so we need to:
    // 1. Move existing contents to a temp location
    // 2. Run jj workspace add
    // 3. Move contents back (excluding newly-created .jj)
    let temp_backup = root.join("ws").join(format!(".{name}-attach-backup"));

    // Create backup directory
    std::fs::create_dir_all(&temp_backup)
        .with_context(|| format!("Failed to create backup directory: {}", temp_backup.display()))?;

    // Move all contents (except .jj) to backup
    let entries: Vec<_> = std::fs::read_dir(&path)
        .with_context(|| format!("Failed to read directory: {}", path.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() != ".jj")
        .collect();

    for entry in &entries {
        let src = entry.path();
        let dst = temp_backup.join(entry.file_name());
        std::fs::rename(&src, &dst).with_context(|| {
            format!(
                "Failed to move {} to backup",
                entry.file_name().to_string_lossy()
            )
        })?;
    }

    // Remove the .jj directory (stale workspace metadata)
    let jj_dir = path.join(".jj");
    if jj_dir.exists() {
        std::fs::remove_dir_all(&jj_dir).with_context(|| "Failed to remove stale .jj directory")?;
    }

    // Now the directory should be empty, run jj workspace add
    let output = Command::new("jj")
        .args([
            "workspace",
            "add",
            path.to_str().unwrap(),
            "--name",
            name,
            "-r",
            &attach_rev,
        ])
        .current_dir(&root)
        .output()
        .context("Failed to run jj workspace add")?;

    if !output.status.success() {
        // Restore backup on failure
        for entry in std::fs::read_dir(&temp_backup)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
        {
            let src = entry.path();
            let dst = path.join(entry.file_name());
            let _ = std::fs::rename(&src, &dst);
        }
        let _ = std::fs::remove_dir_all(&temp_backup);

        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Failed to attach workspace: {}\n  \
             Your files have been restored.\n  \
             Try: maw ws destroy {name} && maw ws create {name}",
            stderr.trim()
        );
    }

    // Move contents back from backup
    for entry in std::fs::read_dir(&temp_backup)
        .with_context(|| "Failed to read backup directory")?
        .filter_map(|e| e.ok())
    {
        let src = entry.path();
        let dst = path.join(entry.file_name());
        // If jj created the file, remove it first (jj workspace add populates working copy)
        if dst.exists() {
            if dst.is_dir() {
                std::fs::remove_dir_all(&dst).ok();
            } else {
                std::fs::remove_file(&dst).ok();
            }
        }
        std::fs::rename(&src, &dst).with_context(|| {
            format!(
                "Failed to restore {} from backup",
                entry.file_name().to_string_lossy()
            )
        })?;
    }

    // Clean up backup directory
    std::fs::remove_dir_all(&temp_backup).ok();

    println!();
    println!("Workspace '{name}' attached!");
    println!();
    println!("  Path: {}/", path.display());
    println!();
    println!("  NOTE: Your local files were preserved. They may differ from the");
    println!("  revision's files. Run 'maw ws jj {name} status' to see differences.");
    println!();

    // Check if workspace is stale after attaching
    let status_check = Command::new("jj")
        .args(["status"])
        .current_dir(&path)
        .output();

    if let Ok(status) = status_check {
        let stderr = String::from_utf8_lossy(&status.stderr);
        if stderr.contains("working copy is stale") {
            println!("NOTE: Workspace is stale (files may be outdated).");
            println!("  Fix: maw ws sync {name}");
            println!();
        }
    }

    println!("To continue working:");
    println!("  maw ws jj {name} status");
    println!("  maw ws jj {name} log");

    Ok(())
}

fn status(format: OutputFormat) -> Result<()> {
    // Get current workspace name
    let current_ws = get_current_workspace()?;

    // Check if stale
    let stale_check = Command::new("jj")
        .args(["status"])
        .output()
        .context("Failed to run jj status")?;

    let status_stderr = String::from_utf8_lossy(&stale_check.stderr);
    let is_stale = status_stderr.contains("working copy is stale");
    let status_stdout = String::from_utf8_lossy(&stale_check.stdout);

    // Get all workspaces and their commits
    let ws_output = Command::new("jj")
        .args(["workspace", "list"])
        .output()
        .context("Failed to run jj workspace list")?;

    let ws_list = String::from_utf8_lossy(&ws_output.stdout);

    // Check for conflicts
    let log_output = Command::new("jj")
        .args([
            "log",
            "--no-graph",
            "-r",
            "conflicts()",
            "-T",
            r#"change_id.short() ++ " " ++ description.first_line() ++ "\n""#,
        ])
        .output()
        .context("Failed to check for conflicts")?;

    let conflicts_text = String::from_utf8_lossy(&log_output.stdout);

    // Check for divergent commits
    let divergent_output = Command::new("jj")
        .args([
            "log",
            "--no-graph",
            "-T",
            r#"if(divergent, change_id.short() ++ " " ++ commit_id.short() ++ " " ++ description.first_line() ++ "\n", "")"#,
        ])
        .output()
        .context("Failed to check for divergent commits")?;

    let divergent_text = String::from_utf8_lossy(&divergent_output.stdout);

    // For text format, use the traditional output
    if format == OutputFormat::Text {
        print_status_text(
            &current_ws,
            is_stale,
            &status_stdout,
            &ws_list,
            &conflicts_text,
            &divergent_text,
        );
        return Ok(());
    }

    // For structured formats, parse and serialize
    match build_status_struct(
        &current_ws,
        is_stale,
        &status_stdout,
        &ws_list,
        &conflicts_text,
        &divergent_text,
    ) {
        Ok(status_data) => match format.serialize(&status_data) {
            Ok(output) => println!("{output}"),
            Err(e) => {
                eprintln!("Warning: Failed to serialize status to {format:?}: {}", e);
                eprintln!("Falling back to text output:");
                print_status_text(
                    &current_ws,
                    is_stale,
                    &status_stdout,
                    &ws_list,
                    &conflicts_text,
                    &divergent_text,
                );
            }
        },
        Err(e) => {
            eprintln!("Warning: Failed to parse status data: {}", e);
            eprintln!("Falling back to text output:");
            print_status_text(
                &current_ws,
                is_stale,
                &status_stdout,
                &ws_list,
                &conflicts_text,
                &divergent_text,
            );
        }
    }

    Ok(())
}

/// Print status in traditional text format
fn print_status_text(
    current_ws: &str,
    is_stale: bool,
    status_stdout: &str,
    ws_list: &str,
    conflicts: &str,
    divergent: &str,
) {
    println!("=== Workspace Status ===");
    println!();

    if is_stale {
        println!("WARNING: Working copy is stale!");
        println!("  (Another workspace changed shared history — your files are outdated.)");
        println!("  Fix: maw ws sync {current_ws}");
        println!();
    }

    println!("Current: {current_ws}");
    if status_stdout.trim().is_empty() {
        println!("  (no changes)");
    } else {
        for line in status_stdout.lines() {
            println!("  {line}");
        }
    }
    println!();

    println!("=== All Agent Work ===");
    println!();

    for line in ws_list.lines() {
        if let Some((name, rest)) = line.split_once(':') {
            let name = name.trim();
            let is_current = name == current_ws;
            let marker = if is_current { ">" } else { " " };
            println!("{marker} {name}: {}", rest.trim());
        }
    }
    println!();

    if !conflicts.trim().is_empty() {
        println!("=== Conflicts ===");
        println!("  (jj records conflicts in commits instead of blocking — you can keep working)");
        println!();
        for line in conflicts.lines() {
            println!("  ! {line}");
        }
        println!();
        println!("  To resolve: edit conflicted files (look for <<<<<<< markers),");
        println!("  then set the commit message:");
        println!("    maw ws jj <name> describe -m \"resolve: ...\"");
        println!("    ('describe' = set commit message, like git commit --amend -m)");
        println!();
    }

    if !divergent.trim().is_empty() {
        println!("=== Divergent Commits (needs cleanup) ===");
        println!();
        println!("  WARNING: These commits have divergent versions (same change ID, multiple");
        println!("  commit versions). This happens when two operations modified the same commit");
        println!("  concurrently — rare with maw since each agent owns their own commit.");
        println!();
        for line in divergent.lines() {
            if !line.trim().is_empty() {
                println!("  ~ {line}");
            }
        }
        println!();
        println!("  To fix: keep one version and abandon (delete) the others:");
        println!("    jj abandon <change-id>/0   # remove the unwanted version");
        println!();
    }
}

/// Build structured status data (resilient to parsing failures)
fn build_status_struct(
    current_ws: &str,
    is_stale: bool,
    status_stdout: &str,
    ws_list: &str,
    conflicts_text: &str,
    divergent_text: &str,
) -> Result<WorkspaceStatus> {
    let has_changes = !status_stdout.trim().is_empty();
    let changes = if has_changes {
        Some(status_stdout.to_string())
    } else {
        None
    };

    // Parse workspace list
    let mut workspaces = Vec::new();
    for line in ws_list.lines() {
        if let Some((name, rest)) = line.split_once(':') {
            let name = name.trim().to_string();
            let is_current = name == current_ws;
            workspaces.push(WorkspaceEntry {
                name,
                is_current,
                info: rest.trim().to_string(),
            });
        }
    }

    // Parse conflicts
    let mut conflicts = Vec::new();
    for line in conflicts_text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        if parts.len() >= 2 {
            conflicts.push(ConflictInfo {
                change_id: parts[0].to_string(),
                description: parts[1].to_string(),
            });
        }
    }

    // Parse divergent commits
    let mut divergent_commits = Vec::new();
    for line in divergent_text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        if parts.len() >= 3 {
            divergent_commits.push(DivergentCommitInfo {
                change_id: parts[0].to_string(),
                commit_id: parts[1].to_string(),
                description: parts[2].to_string(),
            });
        }
    }

    Ok(WorkspaceStatus {
        current_workspace: current_ws.to_string(),
        is_stale,
        has_changes,
        changes,
        workspaces,
        conflicts,
        divergent_commits,
    })
}

fn get_current_workspace() -> Result<String> {
    // jj workspace list marks current with @
    let output = Command::new("jj")
        .args(["workspace", "list"])
        .output()
        .context("Failed to run jj workspace list")?;

    let list = String::from_utf8_lossy(&output.stdout);
    for line in list.lines() {
        if line.contains('@')
            && let Some((name, _)) = line.split_once(':') {
                return Ok(name.trim().to_string());
            }
    }

    Ok(DEFAULT_WORKSPACE.to_string())
}

fn sync(all: bool) -> Result<()> {
    if all {
        return sync_all();
    }

    // First check if we're stale
    let status_check = Command::new("jj")
        .args(["status"])
        .output()
        .context("Failed to run jj status")?;

    let stderr = String::from_utf8_lossy(&status_check.stderr);
    let is_stale = stderr.contains("working copy is stale");

    if !is_stale {
        println!("Workspace is up to date.");
        return Ok(());
    }

    println!("Workspace is stale (another workspace changed shared history), syncing...");
    println!();

    // Run update-stale and capture output
    let update_output = Command::new("jj")
        .args(["workspace", "update-stale"])
        .output()
        .context("Failed to run jj workspace update-stale")?;

    // Show the output
    let stdout = String::from_utf8_lossy(&update_output.stdout);
    let stderr = String::from_utf8_lossy(&update_output.stderr);

    if !stdout.trim().is_empty() {
        println!("{stdout}");
    }
    if !stderr.trim().is_empty() {
        // jj often puts useful info in stderr
        for line in stderr.lines() {
            // Skip the "Concurrent modification" noise
            if !line.contains("Concurrent modification") {
                println!("{line}");
            }
        }
    }

    if !update_output.status.success() {
        bail!(
            "Failed to sync workspace.\n  Check workspace state: maw ws status\n  Manual fix: jj workspace update-stale"
        );
    }

    // After sync, check for and auto-resolve divergent commits on our working copy.
    // This happens when update-stale creates a fork of the workspace commit.
    resolve_divergent_working_copy(".")?;

    println!();
    println!("Workspace synced successfully.");

    Ok(())
}

/// After sync, detect and auto-resolve divergent commits on the workspace's
/// working copy. When `jj workspace update-stale` runs, it can fork the
/// workspace commit into multiple versions (e.g. change_id/0 and change_id/1).
/// One typically has the agent's actual work, the other is empty.
/// This function keeps the non-empty version and abandons the rest.
fn resolve_divergent_working_copy(workspace_dir: &str) -> Result<()> {
    // Get the working copy's change ID
    let change_output = Command::new("jj")
        .args(["log", "-r", "@", "--no-graph", "-T", "change_id.short()"])
        .current_dir(workspace_dir)
        .output()
        .context("Failed to get working copy change ID")?;

    let change_id = String::from_utf8_lossy(&change_output.stdout).trim().to_string();
    if change_id.is_empty() {
        return Ok(());
    }

    // Check if this change ID has divergent copies.
    // Must use change_id() revset function — bare change_id errors on divergent changes.
    let revset = format!("change_id({})", change_id);
    let divergent_output = Command::new("jj")
        .args([
            "log",
            "--no-graph",
            "-r",
            &revset,
            "-T",
            r#"if(divergent, commit_id.short() ++ "\n", "")"#,
        ])
        .current_dir(workspace_dir)
        .output()
        .context("Failed to check for divergent commits")?;

    let divergent_text = String::from_utf8_lossy(&divergent_output.stdout);
    let divergent_commits: Vec<&str> = divergent_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();

    if divergent_commits.len() <= 1 {
        // No divergence, nothing to do
        return Ok(());
    }

    println!();
    println!(
        "Detected {} divergent copies of workspace commit {change_id}.",
        divergent_commits.len()
    );
    println!("  (This happens when sync forks your commit. Auto-resolving...)");

    // For each divergent copy, check if it has actual file changes (non-empty diff).
    // Keep the one with changes, abandon the empty ones.
    let mut non_empty = Vec::new();
    let mut empty = Vec::new();

    for commit_id in &divergent_commits {
        // Use `jj diff` (not --stat) because --stat outputs a summary line
        // even for empty commits ("0 files changed..."), making empty detection fail.
        let diff_output = Command::new("jj")
            .args(["diff", "-r", commit_id])
            .current_dir(workspace_dir)
            .output();

        match diff_output {
            Ok(out) if out.status.success() => {
                let diff_text = String::from_utf8_lossy(&out.stdout);
                if diff_text.trim().is_empty() {
                    empty.push(*commit_id);
                } else {
                    non_empty.push(*commit_id);
                }
            }
            _ => {
                // Can't determine, treat as non-empty (safe default)
                non_empty.push(*commit_id);
            }
        }
    }

    if non_empty.is_empty() {
        // All copies are empty — nothing meaningful to keep, leave them as-is.
        // The agent hasn't made changes yet so divergence doesn't lose work.
        println!("  All copies are empty (no work to lose). Abandoning all but current.");
        // Abandon all except the current working copy (@ stays)
        for commit_id in &empty {
            let check_current = Command::new("jj")
                .args([
                    "log",
                    "-r",
                    &format!("@ & {commit_id}"),
                    "--no-graph",
                    "-T",
                    "commit_id.short()",
                ])
                .current_dir(workspace_dir)
                .output();

            let is_current = check_current
                .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
                .unwrap_or(false);

            if !is_current {
                let _ = Command::new("jj")
                    .args(["abandon", commit_id])
                    .current_dir(workspace_dir)
                    .output();
                println!("  Abandoned empty copy: {commit_id}");
            }
        }
        return Ok(());
    }

    // Abandon the empty copies
    for commit_id in &empty {
        let abandon_result = Command::new("jj")
            .args(["abandon", commit_id])
            .current_dir(workspace_dir)
            .output();

        match abandon_result {
            Ok(out) if out.status.success() => {
                println!("  Abandoned empty copy: {commit_id}");
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                eprintln!("  Warning: Failed to abandon {commit_id}: {}", stderr.trim());
            }
            Err(e) => {
                eprintln!("  Warning: Failed to abandon {commit_id}: {e}");
            }
        }
    }

    // If we still have multiple non-empty copies, we can't auto-resolve safely.
    // Warn the agent with actionable instructions.
    if non_empty.len() > 1 {
        println!();
        println!("  WARNING: {} non-empty divergent copies remain.", non_empty.len());
        println!("  Both copies have changes — cannot auto-resolve.");
        println!("  To fix manually:");
        println!("    jj log -r 'change_id({change_id})'  # compare the versions");
        for (i, commit_id) in non_empty.iter().enumerate() {
            println!("    jj diff -r {commit_id}  # version {i}");
        }
        println!("    jj abandon <unwanted-commit-id>  # remove the unwanted one");
    } else {
        // If the non-empty copy isn't our working copy, squash it into @
        let non_empty_id = non_empty[0];
        let check_current = Command::new("jj")
            .args([
                "log",
                "-r",
                &format!("@ & {non_empty_id}"),
                "--no-graph",
                "-T",
                "commit_id.short()",
            ])
            .current_dir(workspace_dir)
            .output();

        let is_current = check_current
            .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
            .unwrap_or(false);

        if !is_current {
            // The working copy is empty but the non-empty copy is elsewhere.
            // Squash the non-empty one into @ to recover the work.
            let squash_result = Command::new("jj")
                .args(["squash", "--from", non_empty_id, "--into", "@"])
                .current_dir(workspace_dir)
                .output();

            match squash_result {
                Ok(out) if out.status.success() => {
                    println!("  Recovered work from {non_empty_id} into working copy.");
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    eprintln!(
                        "  WARNING: Could not auto-recover work: {}\n  \
                         Manual fix: jj squash --from {non_empty_id} --into @",
                        stderr.trim()
                    );
                }
                Err(e) => {
                    eprintln!(
                        "  WARNING: Could not auto-recover work: {e}\n  \
                         Manual fix: jj squash --from {non_empty_id} --into @"
                    );
                }
            }
        } else {
            println!("  Kept non-empty copy: {non_empty_id} (your work is preserved).");
        }
    }

    Ok(())
}

/// Sync all workspaces at once
fn sync_all() -> Result<()> {
    let root = repo_root()?;

    // Get all workspaces
    let output = Command::new("jj")
        .args(["workspace", "list"])
        .output()
        .context("Failed to run jj workspace list")?;

    let ws_list = String::from_utf8_lossy(&output.stdout);

    // Parse workspace names (format: "name@: change_id ..." or "name: change_id ...")
    let workspace_names: Vec<String> = ws_list
        .lines()
        .filter_map(|l| l.split(':').next())
        .map(|s| s.trim().trim_end_matches('@').to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if workspace_names.is_empty() {
        println!("No workspaces found.");
        return Ok(());
    }

    println!("Syncing {} workspace(s)...", workspace_names.len());
    println!();

    let mut synced = 0;
    let mut already_current = 0;
    let mut errors: Vec<String> = Vec::new();

    for ws in &workspace_names {
        // Validate workspace name to prevent path traversal (defense-in-depth)
        if validate_workspace_name(ws).is_err() {
            errors.push(format!("{ws}: invalid workspace name (skipped)"));
            continue;
        }

        let path = root.join("ws").join(ws);

        if !path.exists() {
            errors.push(format!("{ws}: directory missing"));
            continue;
        }

        // Check if stale
        let status = Command::new("jj")
            .args(["status"])
            .current_dir(&path)
            .output()
            .context("Failed to run jj status")?;

        let stderr = String::from_utf8_lossy(&status.stderr);
        if !stderr.contains("working copy is stale") {
            already_current += 1;
            continue;
        }

        // Sync
        let sync_result = Command::new("jj")
            .args(["workspace", "update-stale"])
            .current_dir(&path)
            .output();

        match sync_result {
            Ok(out) if out.status.success() => {
                // Check for and resolve divergent commits after sync
                if let Err(e) = resolve_divergent_working_copy(path.to_str().unwrap_or(".")) {
                    eprintln!("  ✓ {ws} - synced (divergent resolution failed: {e})");
                } else {
                    println!("  ✓ {ws} - synced");
                }
                synced += 1;
            }
            Ok(out) => {
                let err = String::from_utf8_lossy(&out.stderr);
                errors.push(format!("{ws}: {}", err.trim()));
            }
            Err(e) => {
                errors.push(format!("{ws}: {e}"));
            }
        }
    }

    println!();
    println!(
        "Results: {} synced, {} already current, {} errors",
        synced,
        already_current,
        errors.len()
    );

    if !errors.is_empty() {
        println!();
        println!("Errors:");
        for err in &errors {
            println!("  - {err}");
        }
    }

    // Re-check for cascade staleness: syncing one workspace can make others stale again
    if synced > 0 {
        let mut cascade_stale = Vec::new();
        for ws in &workspace_names {
            if validate_workspace_name(ws).is_err() {
                continue;
            }
            let path = root.join("ws").join(ws);
            if !path.exists() {
                continue;
            }
            let status = Command::new("jj")
                .args(["status"])
                .current_dir(&path)
                .output();
            if let Ok(out) = status {
                let stderr = String::from_utf8_lossy(&out.stderr);
                if stderr.contains("working copy is stale") {
                    cascade_stale.push(ws.clone());
                }
            }
        }
        if !cascade_stale.is_empty() {
            println!();
            println!(
                "WARNING: {} workspace(s) became stale again (cascade effect): {}",
                cascade_stale.len(),
                cascade_stale.join(", ")
            );
            println!("  This happens when syncing one workspace modifies shared history.");
            println!("  Re-run: maw ws sync --all");
            println!();
            println!("  Tip: To avoid cascading, sync only your workspace:");
            for ws in &cascade_stale {
                println!("    maw ws sync {ws}");
            }
        }
    }

    Ok(())
}

fn jj_in_workspace(name: &str, args: &[String]) -> Result<()> {
    let path = {
        let p = workspace_path(name)?;
        if !p.exists() {
            bail!("Workspace '{name}' does not exist at {}", p.display());
        }
        p
    };

    // Auto-sync stale workspace before running the command.
    // Stale workspaces cause jj commands to fail with exit code 1.
    // Auto-syncing saves 2-3 tool calls per stale encounter.
    auto_sync_if_stale(name, &path)?;

    // Check for commands that might cause divergent commits
    warn_if_targeting_other_commit(name, args, &path);

    let status = Command::new("jj")
        .args(args)
        .current_dir(&path)
        .status()
        .context("Failed to run jj")?;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}

/// Auto-sync a stale workspace before running a command.
/// If the workspace is stale, runs update-stale + divergent resolution.
/// Returns Ok(()) whether or not it was stale (idempotent).
pub(crate) fn auto_sync_if_stale(name: &str, path: &Path) -> Result<()> {
    let output = Command::new("jj")
        .args(["status"])
        .current_dir(path)
        .output()
        .context("Failed to check workspace status")?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.contains("working copy is stale") {
        return Ok(());
    }

    eprintln!("Workspace '{name}' is stale — auto-syncing before running command...");

    let update_output = Command::new("jj")
        .args(["workspace", "update-stale"])
        .current_dir(path)
        .output()
        .context("Failed to run jj workspace update-stale")?;

    if !update_output.status.success() {
        let err = String::from_utf8_lossy(&update_output.stderr);
        bail!(
            "Auto-sync failed for workspace '{name}': {}\n  \
             Manual fix: maw ws sync {name}",
            err.trim()
        );
    }

    // Resolve any divergent commits created by sync
    resolve_divergent_working_copy(path.to_str().unwrap_or("."))?;

    eprintln!("Workspace '{name}' synced. Proceeding with command.");
    eprintln!();

    Ok(())
}

/// Sync stale workspaces before merge to avoid spurious conflicts.
///
/// When a workspace is stale (its base commit is behind main), merging can produce
/// conflicts even when changes don't overlap - just because the workspace is missing
/// intermediate commits from main. This is especially problematic for append-only
/// files where jj's line-based merge would normally just concatenate.
///
/// This function checks each workspace being merged and syncs any that are stale,
/// ensuring all workspace commits are based on current main before the merge.
fn sync_stale_workspaces_for_merge(workspaces: &[String], root: &Path) -> Result<()> {
    let ws_dir = root.join("ws");
    let mut synced_count = 0;

    for ws in workspaces {
        let ws_path = ws_dir.join(ws);
        if !ws_path.exists() {
            // Workspace directory doesn't exist - the merge will fail later with a clearer error
            continue;
        }

        // Check if workspace is stale
        let status_output = Command::new("jj")
            .args(["status"])
            .current_dir(&ws_path)
            .output()
            .with_context(|| format!("Failed to check status of workspace '{ws}'"))?;

        let stderr = String::from_utf8_lossy(&status_output.stderr);
        if !stderr.contains("working copy is stale") {
            continue;
        }

        // Workspace is stale - sync it
        println!("Syncing stale workspace '{ws}' before merge...");

        let update_output = Command::new("jj")
            .args(["workspace", "update-stale"])
            .current_dir(&ws_path)
            .output()
            .with_context(|| format!("Failed to sync stale workspace '{ws}'"))?;

        if !update_output.status.success() {
            let err = String::from_utf8_lossy(&update_output.stderr);
            bail!(
                "Failed to sync stale workspace '{ws}': {}\n  \
                 Manual fix: maw ws sync\n  \
                 Then retry: maw ws merge {}",
                err.trim(),
                workspaces.join(" ")
            );
        }

        // Resolve any divergent commits created by the sync
        resolve_divergent_working_copy(ws_path.to_str().unwrap_or("."))?;

        synced_count += 1;
    }

    if synced_count > 0 {
        println!(
            "Synced {} stale workspace(s). Proceeding with merge.",
            synced_count
        );
        println!();
    }

    Ok(())
}

/// Warn if a jj command targets a commit outside this workspace's working copy.
/// This helps prevent divergent commits from agents modifying shared commits.
fn warn_if_targeting_other_commit(workspace_name: &str, args: &[String], workspace_path: &Path) {
    // Commands that modify commits and take an optional revision argument
    let modifying_commands = ["describe", "edit", "abandon", "backout"];

    let Some(cmd) = args.first() else {
        return;
    };

    if !modifying_commands.contains(&cmd.as_str()) {
        return;
    }

    // Look for a revision argument (first positional arg that's not a flag)
    // For describe: `jj describe [revision] [-m msg]`
    // If no revision given, it defaults to @ which is safe
    let mut revision_arg: Option<&str> = None;
    let mut skip_next = false;

    for arg in args.iter().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        // Skip flags and their values
        if arg.starts_with('-') {
            // Flags that take a value
            if matches!(arg.as_str(), "-m" | "--message" | "-r" | "--revision") {
                skip_next = true;
            }
            continue;
        }
        // First non-flag arg is the revision
        revision_arg = Some(arg.as_str());
        break;
    }

    // Safe cases: no revision (defaults to @), or explicitly @
    let Some(rev) = revision_arg else {
        return;
    };
    if rev == "@" {
        return;
    }

    // Definitely dangerous: targeting well-known shared refs
    let dangerous_refs = ["main", "master", "trunk", "@-", "@--", "root()"];
    let is_dangerous = dangerous_refs.contains(&rev)
        || rev.starts_with("main@")
        || rev.starts_with("master@")
        || rev.contains("::") // revset ranges
        || rev.contains(".."); // revset ranges

    if is_dangerous {
        eprintln!();
        eprintln!("⚠️  WARNING: '{cmd} {rev}' modifies a commit outside your workspace.");
        eprintln!("   This may cause DIVERGENT COMMITS if others are using that commit.");
        eprintln!();
        eprintln!("   Safe alternative: `maw ws jj {workspace_name} {cmd}` (targets @)");
        eprintln!("   Undo if needed:   `maw ws jj {workspace_name} undo`");
        eprintln!();
        return;
    }

    // For other revision args, check if it matches the workspace's working copy
    // Get the workspace's current change ID
    let output = Command::new("jj")
        .args(["log", "-r", "@", "--no-graph", "-T", "change_id"])
        .current_dir(workspace_path)
        .output();

    let Ok(output) = output else {
        return; // Can't check, skip warning
    };

    let workspace_change_id = String::from_utf8_lossy(&output.stdout);
    let workspace_change_id = workspace_change_id.trim();

    // If the revision matches workspace's change ID (prefix match ok), it's safe
    if !workspace_change_id.is_empty()
        && (workspace_change_id.starts_with(rev) || rev.starts_with(workspace_change_id))
    {
        return;
    }

    // Unknown revision that's not our working copy - warn
    eprintln!();
    eprintln!("⚠️  WARNING: '{cmd} {rev}' may target a commit outside your workspace.");
    eprintln!("   Modifying shared commits causes DIVERGENT COMMITS.");
    eprintln!();
    eprintln!("   Your workspace's change ID: {workspace_change_id}");
    eprintln!("   Safe alternative: `maw ws jj {workspace_name} {cmd}` (targets @)");
    eprintln!("   Undo if needed:   `maw ws jj {workspace_name} undo`");
    eprintln!();
}

/// Show commit history for a workspace
fn history(name: &str, limit: usize) -> Result<()> {
    let root = repo_root()?;
    validate_workspace_name(name)?;

    // Check if workspace exists
    let ws_output = Command::new("jj")
        .args(["workspace", "list"])
        .current_dir(&root)
        .output()
        .context("Failed to run jj workspace list")?;

    let ws_list = String::from_utf8_lossy(&ws_output.stdout);
    let workspace_exists = ws_list
        .lines()
        .any(|line| {
            line.split(':')
                .next()
                .map(|n| n.trim().trim_end_matches('@') == name)
                .unwrap_or(false)
        });

    if !workspace_exists {
        bail!(
            "Workspace '{name}' not found.\n  \
             List workspaces: maw ws list"
        );
    }

    // Use revset to get commits specific to this workspace:
    // {name}@:: gets all commits reachable from the workspace's working copy
    // ~::main excludes commits already in main (ancestors of main)
    // This shows commits the workspace has made since diverging from main
    let revset = format!("{name}@:: & ~::main");

    let output = Command::new("jj")
        .args([
            "log",
            "--no-graph",
            "-r",
            &revset,
            "-T",
            r#"change_id.short() ++ " " ++ commit_id.short() ++ " " ++ committer.timestamp().format("%Y-%m-%d %H:%M") ++ " " ++ if(description.first_line(), description.first_line(), "(no description)") ++ "\n""#,
            "-n",
            &limit.to_string(),
        ])
        .current_dir(&root)
        .output()
        .context("Failed to get workspace history")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to get history: {}", stderr.trim());
    }

    let history = String::from_utf8_lossy(&output.stdout);

    if history.trim().is_empty() {
        println!("Workspace '{name}' has no commits yet.");
        println!();
        println!("  (Workspace starts with an empty commit for ownership.");
        println!("   Edit files and describe your changes to create history.)");
        println!();
        println!("  Start working:");
        println!("    maw ws jj {name} describe -m \"feat: what you're implementing\"");
        return Ok(());
    }

    println!("=== Commit History: {name} ===");
    println!();
    println!("  change_id      commit        timestamp         description");
    println!("  ────────────   ──────────    ────────────────  ────────────────────────");

    for line in history.lines() {
        if line.trim().is_empty() {
            continue;
        }
        // Format: change_id commit_id date time description
        // Note: timestamp is "YYYY-MM-DD HH:MM" (two parts separated by space)
        let parts: Vec<&str> = line.splitn(5, ' ').collect();
        if parts.len() >= 5 {
            let change_id = parts[0];
            let commit_id = parts[1];
            let date = parts[2];
            let time = parts[3];
            let description = parts[4];
            println!("  {change_id}   {commit_id}    {date} {time}  {description}");
        } else if parts.len() == 4 {
            // Might be missing description
            let change_id = parts[0];
            let commit_id = parts[1];
            let date = parts[2];
            let time = parts[3];
            println!("  {change_id}   {commit_id}    {date} {time}  (no description)");
        } else {
            println!("  {line}");
        }
    }

    let line_count = history.lines().filter(|l| !l.trim().is_empty()).count();
    println!();
    println!("Showing {} commit(s)", line_count);

    if line_count >= limit {
        println!("  (Use --limit/-n to show more)");
    }

    println!();
    println!("Tip: View full commit details:");
    println!("  maw ws jj {name} show <change-id>");

    Ok(())
}

/// Check for conflicts after merge and auto-resolve paths matching config patterns.
/// Returns true if there are remaining (unresolved) conflicts.
fn auto_resolve_conflicts(root: &Path, config: &MawConfig, branch: &str) -> Result<bool> {
    // Check for conflicts
    let status_output = Command::new("jj")
        .args(["status"])
        .current_dir(root)
        .output()
        .context("Failed to check status")?;

    let status_text = String::from_utf8_lossy(&status_output.stdout);
    if !status_text.contains("conflict") {
        return Ok(false);
    }

    // Get list of conflicted files
    let conflicted_files = get_conflicted_files(root)?;
    if conflicted_files.is_empty() {
        return Ok(false);
    }

    // Check if we have patterns to auto-resolve
    let patterns = &config.merge.auto_resolve_from_main;
    if patterns.is_empty() {
        println!();
        println!("WARNING: Merge has conflicts that need resolution.");
        println!("Run `jj status` to see conflicted files.");
        return Ok(true);
    }

    // Compile glob patterns
    let compiled_patterns: Vec<Pattern> = patterns
        .iter()
        .filter_map(|p| Pattern::new(p).ok())
        .collect();

    // Find files to auto-resolve
    let mut auto_resolved = Vec::new();
    let mut remaining_conflicts = Vec::new();

    for file in &conflicted_files {
        let matches_pattern = compiled_patterns.iter().any(|pat| pat.matches(file));
        if matches_pattern {
            auto_resolved.push(file.clone());
        } else {
            remaining_conflicts.push(file.clone());
        }
    }

    // Auto-resolve matching files by restoring from main
    if !auto_resolved.is_empty() {
        println!();
        println!(
            "Auto-resolving {} file(s) from {branch} (via .maw.toml config):",
            auto_resolved.len()
        );
        for file in &auto_resolved {
            // Restore file from branch to resolve conflict
            let restore_output = Command::new("jj")
                .args(["restore", "--from", branch, file])
                .current_dir(root)
                .output()
                .context("Failed to restore file from main")?;

            if restore_output.status.success() {
                println!("  ✓ {file}");
            } else {
                let stderr = String::from_utf8_lossy(&restore_output.stderr);
                println!("  ✗ {file}: {}", stderr.trim());
                remaining_conflicts.push(file.clone());
            }
        }
    }

    // Report remaining conflicts
    if !remaining_conflicts.is_empty() {
        println!();
        println!(
            "WARNING: {} conflict(s) remaining that need manual resolution:",
            remaining_conflicts.len()
        );
        for file in &remaining_conflicts {
            println!("  - {file}");
        }
        println!();
        println!("Run `jj status` to see details.");
        return Ok(true);
    }

    println!();
    println!("All conflicts auto-resolved from main.");
    Ok(false)
}

/// Get list of files with conflicts from jj status output.
fn get_conflicted_files(root: &Path) -> Result<Vec<String>> {
    // Use jj status to get conflicted files
    // Format: "C filename" for conflicted files
    let output = Command::new("jj")
        .args(["status"])
        .current_dir(root)
        .output()
        .context("Failed to run jj status")?;

    let status_text = String::from_utf8_lossy(&output.stdout);
    let mut files = Vec::new();

    for line in status_text.lines() {
        // jj status shows conflicts as "C path/to/file"
        if let Some(stripped) = line.strip_prefix("C ") {
            files.push(stripped.trim().to_string());
        }
    }

    Ok(files)
}

/// Run a list of hook commands. Returns Ok(()) if all succeed or hooks are empty.
/// For pre-merge hooks: aborts on first failure.
/// For post-merge hooks: warns but continues on failure.
fn run_hooks(hooks: &[String], hook_type: &str, root: &Path, abort_on_failure: bool) -> Result<()> {
    if hooks.is_empty() {
        return Ok(());
    }

    println!("Running {hook_type} hooks...");

    for (i, cmd) in hooks.iter().enumerate() {
        println!("  [{}/{}] {cmd}", i + 1, hooks.len());

        // Use shell to execute the command (allows pipes, redirects, etc.)
        // Security note: These commands come from .maw.toml which is checked into
        // the repo and controlled by the project owner. This is intentional and
        // similar to how git hooks, npm scripts, and Makefiles work.
        let output = Command::new("sh")
            .args(["-c", cmd])
            .current_dir(root)
            .output()
            .with_context(|| format!("Failed to execute hook command: {cmd}"))?;

        // Show output if any
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stdout.trim().is_empty() {
            for line in stdout.lines() {
                println!("      {line}");
            }
        }
        if !stderr.trim().is_empty() {
            for line in stderr.lines() {
                eprintln!("      {line}");
            }
        }

        if !output.status.success() {
            let exit_code = output.status.code().unwrap_or(-1);
            if abort_on_failure {
                bail!(
                    "{hook_type} hook failed (exit code {exit_code}): {cmd}\n  \
                     Merge aborted. Fix the issue and try again."
                );
            } else {
                eprintln!("  WARNING: {hook_type} hook failed (exit code {exit_code}): {cmd}");
            }
        }
    }

    println!("{hook_type} hooks complete.");
    Ok(())
}

/// Preview what a merge would do without creating any commits.
/// Shows changes in each workspace and potential conflicts.
fn preview_merge(workspaces: &[String], root: &Path) -> Result<()> {
    println!("=== Merge Preview (dry run) ===");
    println!();

    if workspaces.len() == 1 {
        println!("Would adopt workspace: {}", workspaces[0]);
    } else {
        println!("Would merge workspaces: {}", workspaces.join(", "));
    }
    println!();

    // Show changes in each workspace using jj diff --stat
    println!("=== Changes by Workspace ===");
    println!();

    for ws in workspaces {
        println!("--- {} ---", ws);

        // Get diff stats for the workspace using workspace@ syntax
        let diff_output = Command::new("jj")
            .args(["diff", "--stat", "-r", &format!("{ws}@")])
            .current_dir(root)
            .output()
            .with_context(|| format!("Failed to get diff for workspace {ws}"))?;

        if !diff_output.status.success() {
            let stderr = String::from_utf8_lossy(&diff_output.stderr);
            println!("  Could not get changes: {}", stderr.trim());
            println!();
            continue;
        }

        let diff_text = String::from_utf8_lossy(&diff_output.stdout);
        if diff_text.trim().is_empty() {
            println!("  (no changes)");
        } else {
            for line in diff_text.lines() {
                println!("  {line}");
            }
        }
        println!();
    }

    // Check for potential conflicts using files modified in multiple workspaces
    if workspaces.len() > 1 {
        println!("=== Potential Conflicts ===");
        println!();

        // Get files modified in each workspace
        let mut workspace_files: Vec<(String, Vec<String>)> = Vec::new();

        for ws in workspaces {
            let diff_output = Command::new("jj")
                .args(["diff", "--summary", "-r", &format!("{ws}@")])
                .current_dir(root)
                .output()
                .with_context(|| format!("Failed to get diff summary for {ws}"))?;

            if diff_output.status.success() {
                let diff_text = String::from_utf8_lossy(&diff_output.stdout);
                let files: Vec<String> = diff_text
                    .lines()
                    .filter_map(|line| {
                        // Format: "M path/to/file" or "A path/to/file"
                        line.split_whitespace().nth(1).map(|s| s.to_string())
                    })
                    .collect();
                workspace_files.push((ws.clone(), files));
            }
        }

        // Find files modified in multiple workspaces
        let mut conflict_files: Vec<String> = Vec::new();
        for i in 0..workspace_files.len() {
            for j in (i + 1)..workspace_files.len() {
                let (ws1, files1) = &workspace_files[i];
                let (ws2, files2) = &workspace_files[j];
                for file in files1 {
                    if files2.contains(file) && !conflict_files.contains(file) {
                        conflict_files.push(file.clone());
                        println!("  ! {file} - modified in both '{ws1}' and '{ws2}'");
                    }
                }
            }
        }

        if conflict_files.is_empty() {
            println!("  (no overlapping changes detected)");
        } else {
            println!();
            println!("  Note: jj records conflicts in commits instead of blocking.");
            println!("  You can proceed and resolve conflicts after merge if needed.");
        }
        println!();
    }

    println!("=== Summary ===");
    println!();
    println!("To perform this merge, run without --dry-run:");
    println!("  maw ws merge {}", workspaces.join(" "));
    println!();

    Ok(())
}

fn merge(
    workspaces: &[String],
    destroy_after: bool,
    confirm: bool,
    message: Option<&str>,
    dry_run: bool,
    _auto_describe: bool, // No longer needed with linear merge approach
) -> Result<()> {
    let ws_to_merge = workspaces.to_vec();

    if ws_to_merge.is_empty() {
        println!("No workspaces to merge.");
        return Ok(());
    }

    let root = repo_root()?;

    // Load config early for hooks, auto-resolve settings, and branch name
    let config = MawConfig::load(&root)?;
    let branch = config.branch();

    // Preview mode: show what the merge would do without committing
    if dry_run {
        return preview_merge(&ws_to_merge, &root);
    }

    // Run pre-merge hooks (abort on failure)
    run_hooks(&config.hooks.pre_merge, "pre-merge", &root, true)?;

    // Sync stale workspaces before merge to avoid spurious conflicts.
    // When a workspace's base commit is behind main, merging can produce conflicts
    // even when changes don't overlap. Syncing first ensures all workspace commits
    // are based on current main.
    sync_stale_workspaces_for_merge(&ws_to_merge, &root)?;

    if ws_to_merge.len() == 1 {
        println!("Adopting workspace: {}", ws_to_merge[0]);
    } else {
        println!("Merging workspaces: {}", ws_to_merge.join(", "));
    }
    println!();

    // Build revision references using workspace@ syntax
    let revisions: Vec<String> = ws_to_merge.iter().map(|ws| format!("{ws}@")).collect();

    // Build merge commit message
    let msg = message.map_or_else(
        || {
            if ws_to_merge.len() == 1 {
                format!("merge: adopt work from {}", ws_to_merge[0])
            } else {
                format!("merge: combine work from {}", ws_to_merge.join(", "))
            }
        },
        ToString::to_string,
    );

    // NEW APPROACH: Rebase workspace commits directly onto main for linear history
    // This skips scaffolding commits and produces a cleaner graph.

    // Step 1: Rebase all workspace commits onto main
    let revset = revisions.join(" | ");
    let rebase_output = Command::new("jj")
        .args(["rebase", "-r", &revset, "-d", branch])
        .current_dir(&root)
        .output()
        .context("Failed to rebase workspace commits")?;

    if !rebase_output.status.success() {
        let stderr = String::from_utf8_lossy(&rebase_output.stderr);
        bail!(
            "Failed to rebase workspace commits onto {branch}: {}\n  Verify workspaces exist: maw ws list",
            stderr.trim()
        );
    }

    // Step 2: If multiple workspaces, squash them into one commit
    if ws_to_merge.len() > 1 {
        // Squash all but first into the first workspace's commit
        let first_ws = format!("{}@", ws_to_merge[0]);
        let others: Vec<String> = ws_to_merge[1..].iter().map(|ws| format!("{ws}@")).collect();
        let from_revset = others.join(" | ");

        let squash_output = Command::new("jj")
            .args([
                "squash",
                "--from",
                &from_revset,
                "--into",
                &first_ws,
                "-m",
                &msg,
            ])
            .current_dir(&root)
            .output()
            .context("Failed to squash workspace commits")?;

        if !squash_output.status.success() {
            let stderr = String::from_utf8_lossy(&squash_output.stderr);
            bail!("Failed to squash workspace commits: {}", stderr.trim());
        }
    }

    // Step 3: Move main bookmark to the final commit
    let final_rev = format!("{}@", ws_to_merge[0]);
    let bookmark_output = Command::new("jj")
        .args(["bookmark", "set", branch, "-r", &final_rev])
        .current_dir(&root)
        .output()
        .context("Failed to move main bookmark")?;

    if !bookmark_output.status.success() {
        let stderr = String::from_utf8_lossy(&bookmark_output.stderr);
        eprintln!("Warning: Failed to move {branch} bookmark: {}", stderr.trim());
        eprintln!("  Run manually: jj bookmark set {branch} -r {final_rev}");
    }

    // Step 4: Abandon orphaned scaffolding commits (empty, undescribed, not on branch)
    // These are the workspace setup commits that got orphaned by rebase
    let abandon_revset = format!("empty() & description(exact:'') & ~ancestors({branch}) & ~root()");
    let abandon_output = Command::new("jj")
        .args(["abandon", &abandon_revset])
        .current_dir(&root)
        .output();

    if let Ok(output) = abandon_output {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.contains("Abandoned") {
                println!("Cleaned up scaffolding commits.");
            }
        }
    }

    // Step 5: Rebase default workspace onto new branch so on-disk files reflect the merge.
    // The default workspace's working copy is empty (no changes), so this is safe.
    let default_ws = config.default_workspace();
    let default_ws_path = root.join("ws").join(default_ws);
    if default_ws_path.exists() {
        // The merge operations above (squash, bookmark set) ran from root and
        // modified the commit graph. The default workspace may now be stale.
        // Update it BEFORE rebasing to avoid stale errors.
        let _ = Command::new("jj")
            .args(["workspace", "update-stale"])
            .current_dir(&default_ws_path)
            .output();

        let rebase_default = Command::new("jj")
            .args(["rebase", "-r", &format!("{default_ws}@"), "-d", branch])
            .current_dir(&default_ws_path)
            .output();

        if let Ok(output) = rebase_default
            && !output.status.success()
        {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("Warning: Failed to rebase default workspace onto {branch}: {}", stderr.trim());
            eprintln!("  On-disk files may not reflect the merge. Run: jj rebase -r {default_ws}@ -d {branch}");
        }

        // The rebase may have created a divergent commit — auto-resolve it.
        let default_ws_str = default_ws_path.to_string_lossy();
        let _ = resolve_divergent_working_copy(&default_ws_str);

        // Restore on-disk files from the parent commit. After rebasing the
        // working copy onto the new main, the commit tree is correct but
        // on-disk files may be missing. `jj restore` writes the parent's
        // files to disk.
        let _ = Command::new("jj")
            .args(["restore"])
            .current_dir(&default_ws_path)
            .output();

        // Final update-stale to clear any remaining stale state.
        // Operations above may have left the workspace stale again.
        let _ = Command::new("jj")
            .args(["workspace", "update-stale"])
            .current_dir(&default_ws_path)
            .output();
    }

    println!("Merged to {branch}: {msg}");
    let has_conflicts = auto_resolve_conflicts(&root, &config, branch)?;

    // Optionally destroy workspaces (but not if there are conflicts!)
    // Never destroy the default workspace during merge --destroy.
    if destroy_after {
        let ws_to_destroy: Vec<String> = ws_to_merge
            .iter()
            .filter(|ws| ws.as_str() != default_ws)
            .cloned()
            .collect();

        if has_conflicts {
            println!("NOT destroying workspaces due to conflicts.");
            println!("Resolve conflicts first, then run:");
            for ws in &ws_to_destroy {
                println!("  maw ws destroy {ws}");
            }
        } else if confirm {
            println!();
            println!("Will destroy {} workspaces:", ws_to_destroy.len());
            for ws in &ws_to_destroy {
                println!("  - {ws}");
            }
            println!();
            print!("Continue? [y/N] ");
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            if !input.trim().eq_ignore_ascii_case("y") {
                println!("Aborted. Workspaces kept. Merge commit still exists.");
                return Ok(());
            }

            destroy_workspaces(&ws_to_destroy, &root)?;
        } else {
            println!();
            destroy_workspaces(&ws_to_destroy, &root)?;
        }
    }

    // Run post-merge hooks (warn on failure but don't abort)
    run_hooks(&config.hooks.post_merge, "post-merge", &root, false)?;

    // Show next steps for pushing
    if !has_conflicts {
        println!();
        println!("Next: push to remote:");
        println!("  maw push");
    }

    Ok(())
}

fn destroy_workspaces(workspaces: &[String], root: &Path) -> Result<()> {
    println!("Cleaning up workspaces...");
    let ws_dir = root.join("ws");
    // Run jj commands from inside the default workspace to avoid stale
    // root working copy errors in the bare repo model.
    let jj_cwd = ws_dir.join(DEFAULT_WORKSPACE);
    let jj_cwd = if jj_cwd.exists() { &jj_cwd } else { root };
    for ws in workspaces {
        if ws == DEFAULT_WORKSPACE {
            println!("  Skipping default workspace");
            continue;
        }
        let path = ws_dir.join(ws);
        let _ = Command::new("jj")
            .args(["workspace", "forget", ws])
            .current_dir(jj_cwd)
            .status();
        if path.exists() {
            std::fs::remove_dir_all(&path).ok();
        }
        println!("  Destroyed: {ws}");
    }
    Ok(())
}

/// Result of analyzing workspaces for pruning
#[derive(Debug)]
struct PruneAnalysis {
    /// Directories in ws/ that jj doesn't know about
    orphaned: Vec<String>,
    /// Workspaces jj tracks but directories are missing
    missing: Vec<String>,
    /// Workspaces with no changes (empty working copies)
    empty: Vec<String>,
}

fn prune(force: bool, include_empty: bool) -> Result<()> {
    let root = repo_root()?;
    let ws_dir = workspaces_dir()?;

    // Get workspaces jj knows about
    let output = Command::new("jj")
        .args(["workspace", "list"])
        .current_dir(&root)
        .output()
        .context("Failed to run jj workspace list")?;

    if !output.status.success() {
        bail!(
            "jj workspace list failed: {}\n  To fix: ensure you're in a jj repository",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let ws_list = String::from_utf8_lossy(&output.stdout);

    // Parse jj-tracked workspaces (format: "name@: change_id ..." or "name: change_id ...")
    let jj_workspaces: std::collections::HashSet<String> = ws_list
        .lines()
        .filter_map(|l| l.split(':').next())
        .map(|s| s.trim().trim_end_matches('@').to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // Get directories in ws/
    // Security: validate names and skip symlinks to prevent traversal attacks
    let dir_workspaces: std::collections::HashSet<String> = if ws_dir.exists() {
        std::fs::read_dir(&ws_dir)
            .context("Failed to read ws directory")?
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                let path = entry.path();
                // Skip symlinks - they could point anywhere
                if path.is_symlink() {
                    return false;
                }
                path.is_dir()
            })
            .filter_map(|entry| entry.file_name().into_string().ok())
            // Validate name to prevent path traversal
            .filter(|name| validate_workspace_name(name).is_ok())
            .collect()
    } else {
        std::collections::HashSet::new()
    };

    let mut analysis = PruneAnalysis {
        orphaned: Vec::new(),
        missing: Vec::new(),
        empty: Vec::new(),
    };

    // Find orphaned: directories that exist but jj doesn't track
    for dir_name in &dir_workspaces {
        if !jj_workspaces.contains(dir_name) {
            analysis.orphaned.push(dir_name.clone());
        }
    }

    // Find missing: jj tracks but directory doesn't exist
    for jj_ws in &jj_workspaces {
        if !dir_workspaces.contains(jj_ws) {
            analysis.missing.push(jj_ws.clone());
        }
    }

    // Find empty workspaces (if requested)
    if include_empty {
        for jj_ws in &jj_workspaces {
            if jj_ws == DEFAULT_WORKSPACE {
                continue; // don't suggest pruning the default workspace
            }
            // Skip workspaces that are already in orphaned or missing lists
            if analysis.orphaned.contains(jj_ws) || analysis.missing.contains(jj_ws) {
                continue;
            }
            // Check if workspace has changes using jj diff
            let diff_output = Command::new("jj")
                .args(["diff", "--stat", "-r", &format!("{jj_ws}@")])
                .current_dir(&root)
                .output();

            if let Ok(diff) = diff_output {
                if diff.status.success() {
                    let diff_text = String::from_utf8_lossy(&diff.stdout);
                    if diff_text.trim().is_empty() {
                        analysis.empty.push(jj_ws.clone());
                    }
                }
            }
        }
    }

    // Sort for consistent output
    analysis.orphaned.sort();
    analysis.missing.sort();
    analysis.empty.sort();

    // Report findings
    let total_issues = analysis.orphaned.len() + analysis.missing.len() + analysis.empty.len();

    if total_issues == 0 {
        println!("No workspaces need pruning.");
        if !include_empty {
            println!("  (Use --empty to also check for workspaces with no changes)");
        }
        return Ok(());
    }

    if force {
        println!("Pruning workspaces...");
    } else {
        println!("=== Prune Preview ===");
        println!("(Use --force to actually delete)");
    }
    println!();

    // Handle orphaned directories
    if !analysis.orphaned.is_empty() {
        println!(
            "Orphaned ({} directory exists but jj forgot the workspace):",
            analysis.orphaned.len()
        );
        for name in &analysis.orphaned {
            let path = ws_dir.join(name);
            if force {
                // Defense in depth: check symlink again before deletion
                if path.is_symlink() {
                    println!("  ✗ {name}: refused to delete symlink (security)");
                    continue;
                }
                if let Err(e) = std::fs::remove_dir_all(&path) {
                    println!("  ✗ {name}: failed to delete - {e}");
                } else {
                    println!("  ✓ {name}: deleted");
                }
            } else {
                println!("  - {name}");
                println!("      Path: {}/", path.display());
            }
        }
        println!();
    }

    // Handle missing workspaces (jj tracks but no directory)
    if !analysis.missing.is_empty() {
        println!(
            "Missing ({} jj tracks workspace but directory is gone):",
            analysis.missing.len()
        );
        for name in &analysis.missing {
            if force {
                let forget_result = Command::new("jj")
                    .args(["workspace", "forget", name])
                    .current_dir(&root)
                    .output();

                match forget_result {
                    Ok(out) if out.status.success() => {
                        println!("  ✓ {name}: forgotten from jj");
                    }
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        println!("  ✗ {name}: failed to forget - {}", stderr.trim());
                    }
                    Err(e) => {
                        println!("  ✗ {name}: failed to forget - {e}");
                    }
                }
            } else {
                println!("  - {name}");
                println!("      Would run: jj workspace forget {name}");
            }
        }
        println!();
    }

    // Handle empty workspaces
    if !analysis.empty.is_empty() {
        println!(
            "Empty ({} workspaces with no changes):",
            analysis.empty.len()
        );
        for name in &analysis.empty {
            let path = ws_dir.join(name);
            if force {
                // Defense in depth: check symlink before deletion
                if path.is_symlink() {
                    println!("  ✗ {name}: refused to delete symlink (security)");
                    continue;
                }
                // First forget from jj, then delete directory
                let _ = Command::new("jj")
                    .args(["workspace", "forget", name])
                    .current_dir(&root)
                    .status();

                if path.exists() {
                    if let Err(e) = std::fs::remove_dir_all(&path) {
                        println!("  ✗ {name}: failed to delete - {e}");
                    } else {
                        println!("  ✓ {name}: deleted");
                    }
                } else {
                    println!("  ✓ {name}: forgotten");
                }
            } else {
                println!("  - {name}");
                println!("      Path: {}/", path.display());
            }
        }
        println!();
    }

    // Summary
    if force {
        let deleted = analysis.orphaned.len() + analysis.empty.len();
        let forgotten = analysis.missing.len();
        println!(
            "Pruned: {} deleted, {} forgotten from jj",
            deleted, forgotten
        );
    } else {
        println!("=== Summary ===");
        println!(
            "Would prune {} workspace(s): {} orphaned, {} missing, {} empty",
            total_issues,
            analysis.orphaned.len(),
            analysis.missing.len(),
            analysis.empty.len()
        );
        println!();
        println!("To prune:");
        if include_empty {
            println!("  maw ws prune --empty --force");
        } else {
            println!("  maw ws prune --force");
        }
    }

    Ok(())
}

fn list(verbose: bool, format: OutputFormat) -> Result<()> {
    let output = Command::new("jj")
        .args(["workspace", "list"])
        .output()
        .context("Failed to run jj workspace list")?;

    if !output.status.success() {
        bail!(
            "jj workspace list failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let list = String::from_utf8_lossy(&output.stdout);

    if list.trim().is_empty() {
        // Even for structured formats, return a simple message for empty lists
        match format {
            OutputFormat::Text => println!("No workspaces found."),
            OutputFormat::Json => println!("[]"),
            OutputFormat::Toon => println!("[]"),
        }
        return Ok(());
    }

    // For text format, just print the raw jj output (traditional behavior)
    if format == OutputFormat::Text {
        println!("Workspaces:");
        println!();

        for line in list.lines() {
            if let Some((name, rest)) = line.split_once(':') {
                let name = name.trim();
                let rest = rest.trim();

                let is_default = name == DEFAULT_WORKSPACE;
                let marker = if is_default { "*" } else { " " };

                if verbose {
                    println!("{marker} {name}");
                    println!("    {rest}");

                    let path = workspace_path(name)?;
                    if path.exists() {
                        println!("    path: {}/", path.display());
                    } else {
                        println!("    path: (missing!)");
                    }
                    println!();
                } else {
                    println!("{marker} {name}: {rest}");
                }
            }
        }
        // Check for stale workspaces after listing
        warn_stale_workspaces();
        return Ok(());
    }

    // For structured formats (json/toon), parse into data structures
    // Be resilient: if parsing fails, fall back to raw text
    let workspaces: Vec<WorkspaceInfo> = match parse_workspace_list(&list, verbose) {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("Warning: Failed to parse workspace list: {}", e);
            eprintln!("Falling back to raw text output:");
            println!("{list}");
            return Ok(());
        }
    };

    // Serialize to requested format
    match format.serialize(&workspaces) {
        Ok(output) => println!("{output}"),
        Err(e) => {
            eprintln!("Warning: Failed to serialize to {format:?}: {}", e);
            eprintln!("Falling back to raw text output:");
            println!("{list}");
        }
    }

    // Check for stale workspaces after listing
    warn_stale_workspaces();

    Ok(())
}

/// Parse jj workspace list output into structured data
/// Resilient to format changes - returns error if parsing fails
fn parse_workspace_list(list: &str, include_path: bool) -> Result<Vec<WorkspaceInfo>> {
    let mut workspaces = Vec::new();

    for line in list.lines() {
        // Expected format: "name@: change_id commit_id description"
        // Current workspace has @ marker
        let Some((name_part, rest)) = line.split_once(':') else {
            // Skip lines that don't match expected format
            continue;
        };

        let name_part = name_part.trim();
        let is_current = name_part.contains('@');
        let name = name_part.trim_end_matches('@').trim();

        // Parse rest: "change_id commit_id description..."
        let parts: Vec<&str> = rest.trim().split_whitespace().collect();
        if parts.len() < 2 {
            // Need at least change_id and commit_id
            bail!("Unexpected workspace line format: {}", line);
        }

        let change_id = parts[0].to_string();
        let commit_id = parts[1].to_string();
        let description = parts[2..].join(" ");

        let path = if include_path {
            workspace_path(name).ok().and_then(|p| {
                if p.exists() {
                    Some(p.display().to_string())
                } else {
                    None
                }
            })
        } else {
            None
        };

        workspaces.push(WorkspaceInfo {
            name: name.to_string(),
            is_current,
            is_default: name == DEFAULT_WORKSPACE,
            change_id,
            commit_id,
            description,
            path,
        });
    }

    Ok(workspaces)
}
