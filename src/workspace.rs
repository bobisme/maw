use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use glob::Pattern;
use rand::seq::IndexedRandom;
use serde::{Deserialize, Serialize};

use crate::format::OutputFormat;

/// Configuration from .maw.toml
#[derive(Debug, Default, Deserialize)]
struct MawConfig {
    #[serde(default)]
    merge: MergeConfig,
}

/// Merge-specific configuration
#[derive(Debug, Default, Deserialize)]
struct MergeConfig {
    /// Paths to auto-resolve from main during merge conflicts.
    /// Supports glob patterns like ".beads/**" or ".crit/*".
    #[serde(default)]
    auto_resolve_from_main: Vec<String>,
}

impl MawConfig {
    /// Load config from .maw.toml in the repo root, or return defaults if not found.
    fn load(repo_root: &Path) -> Result<Self> {
        let config_path = repo_root.join(".maw.toml");
        if !config_path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;
        toml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", config_path.display()))
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
    /// Creates an isolated jj workspace in .workspaces/<name>/ with its
    /// own working copy (a separate view of the codebase, like a git
    /// worktree but lightweight). All file reads, writes, and edits must
    /// use the absolute workspace path shown after creation.
    ///
    /// After creation:
    ///   1. Edit files under .workspaces/<name>/ (use absolute paths)
    ///   2. Save work: maw ws jj <name> describe -m "feat: ..."
    ///      ('describe' sets the commit message — like git commit --amend -m)
    ///   3. Run other commands: cd /abs/path/.workspaces/<name> && cmd
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
    /// Use this instead of 'cd .workspaces/<name> && jj ...'.
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

    /// Clean up orphaned, stale, or empty workspaces
    ///
    /// Detects problematic workspaces:
    /// - Orphaned: directory exists in .workspaces/ but jj forgot the workspace
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

    /// Merge work from workspaces into default
    ///
    /// Creates a merge commit combining work from the specified workspaces.
    /// Works with one or more workspaces. After merge, check output for
    /// undescribed commits (commits with no message) that may block push.
    ///
    /// Examples:
    ///   maw ws merge alice                 # adopt alice's work
    ///   maw ws merge alice bob             # merge alice and bob's work
    ///   maw ws merge alice bob --destroy   # merge and clean up (non-interactive)
    ///   maw ws merge alice bob --dry-run   # preview merge without committing
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
        WorkspaceCommands::Prune { force, empty } => prune(force, empty),
        WorkspaceCommands::Merge {
            workspaces,
            destroy,
            confirm,
            message,
            dry_run,
        } => merge(&workspaces, destroy, confirm, message.as_deref(), dry_run),
    }
}

fn repo_root() -> Result<PathBuf> {
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
    // If we're inside a workspace (.workspaces/<name>/), walk up
    // to the directory containing .workspaces/.
    for ancestor in root.ancestors() {
        if ancestor.file_name().map_or(false, |n| n == ".workspaces") {
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
    Ok(repo_root()?.join(".workspaces"))
}

fn workspace_path(name: &str) -> Result<PathBuf> {
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

fn create(name: &str, revision: Option<&str>) -> Result<()> {
    ensure_repo_root()?;
    let path = workspace_path(name)?;

    if path.exists() {
        bail!("Workspace already exists at {}", path.display());
    }

    // Ensure .workspaces directory exists
    let ws_dir = workspaces_dir()?;
    std::fs::create_dir_all(&ws_dir)
        .with_context(|| format!("Failed to create {}", ws_dir.display()))?;

    println!("Creating workspace '{name}' at .workspaces/{name} ...");

    // Determine base revision - default to @ so agents see orchestrator's current state
    let base = revision.map_or_else(|| "@".to_string(), ToString::to_string);

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
    println!("  Path:   {}", path.display());
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
    println!("  cd {} && cargo test", path.display());
    println!();
    println!("Note: jj has no staging area — all edits are tracked automatically.");
    println!("Your changes are always in your commit. Use 'describe' to set the message.");

    Ok(())
}

fn destroy(name: &str, confirm: bool) -> Result<()> {
    if name == "default" {
        bail!("Cannot destroy the default workspace");
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
        println!("  Fix: maw ws sync");
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

    Ok("default".to_string())
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

    if update_output.status.success() {
        println!();
        println!("Workspace synced successfully.");
    } else {
        bail!(
            "Failed to sync workspace.\n  Check workspace state: maw ws status\n  Manual fix: jj workspace update-stale"
        );
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
        if ws != "default" {
            if let Err(_) = validate_workspace_name(ws) {
                errors.push(format!("{ws}: invalid workspace name (skipped)"));
                continue;
            }
        }

        let path = if ws == "default" {
            root.clone()
        } else {
            root.join(".workspaces").join(ws)
        };

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
                println!("  ✓ {ws} - synced");
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

    Ok(())
}

fn jj_in_workspace(name: &str, args: &[String]) -> Result<()> {
    let path = workspace_path(name)?;

    if !path.exists() {
        bail!("Workspace '{name}' does not exist at {}", path.display());
    }

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

/// Check for conflicts after merge and auto-resolve paths matching config patterns.
/// Returns true if there are remaining (unresolved) conflicts.
fn auto_resolve_conflicts(root: &Path, config: &MawConfig) -> Result<bool> {
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
            "Auto-resolving {} file(s) from main (via .maw.toml config):",
            auto_resolved.len()
        );
        for file in &auto_resolved {
            // Restore file from main to resolve conflict
            let restore_output = Command::new("jj")
                .args(["restore", "--from", "main", file])
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
) -> Result<()> {
    let ws_to_merge = workspaces.to_vec();

    if ws_to_merge.is_empty() {
        println!("No workspaces to merge.");
        return Ok(());
    }

    // Always run merge from the repo root (default workspace context).
    // If run from inside a workspace, jj new would move that workspace's
    // working copy instead of default's, then workspace forget would orphan
    // the merge commit.
    let root = ensure_repo_root()?;

    // Preview mode: show what the merge would do without committing
    if dry_run {
        return preview_merge(&ws_to_merge, &root);
    }

    if ws_to_merge.len() == 1 {
        println!("Adopting workspace: {}", ws_to_merge[0]);
    } else {
        println!("Merging workspaces: {}", ws_to_merge.join(", "));
    }
    println!();

    // Build revision references using workspace@ syntax
    // This is more reliable than parsing workspace list output
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

    // Create merge commit: jj new ws1@ ws2@ ws3@ -m "message"
    let mut args = vec!["new"];
    for rev in &revisions {
        args.push(rev);
    }
    args.push("-m");
    args.push(&msg);

    let merge_output = Command::new("jj")
        .args(&args)
        .current_dir(&root)
        .output()
        .context("Failed to run jj new")?;

    if !merge_output.status.success() {
        let stderr = String::from_utf8_lossy(&merge_output.stderr);
        bail!(
            "Failed to create merge commit: {}\n  Verify workspaces exist: maw ws list",
            stderr.trim()
        );
    }

    println!("Created merge commit: {msg}");

    // Check for conflicts and auto-resolve if configured
    let config = MawConfig::load(&root)?;
    let has_conflicts = auto_resolve_conflicts(&root, &config)?;

    // Check for empty/undescribed commits that would block push
    let empty_check = Command::new("jj")
        .args([
            "log",
            "--no-graph",
            "-r",
            "description(exact:\"\") & ::@- & ~root()",
            "-T",
            r#"change_id.short() ++ " " ++ if(empty, "(empty)", "(has changes)") ++ "\n""#,
        ])
        .current_dir(&root)
        .output()
        .context("Failed to check for undescribed commits")?;

    let empty_commits = String::from_utf8_lossy(&empty_check.stdout);
    if !empty_commits.trim().is_empty() {
        let count = empty_commits.lines().filter(|l| !l.trim().is_empty()).count();
        println!("WARNING: {count} undescribed commit(s) (no message) in merge ancestry.");
        println!("  jj requires all commits to have descriptions before pushing.");
        println!();
        for line in empty_commits.lines() {
            if !line.trim().is_empty() {
                println!("  ! {line}");
            }
        }
        println!();
        println!("Fix: rebase onto main to skip scaffolding commits:");
        println!("  jj rebase -r @- -d main");
        println!("  ('rebase' moves a commit to a new parent; @- = the merge commit)");
        println!();
        println!("Or give them descriptions:");
        for line in empty_commits.lines() {
            if let Some(id) = line.split_whitespace().next() {
                println!("  jj describe {id} -m \"workspace setup\"");
            }
        }
        println!();
    }

    // Optionally destroy workspaces (but not if there are conflicts!)
    if destroy_after {
        if has_conflicts {
            println!("NOT destroying workspaces due to conflicts.");
            println!("Resolve conflicts first, then run:");
            for ws in &ws_to_merge {
                println!("  maw ws destroy {ws}");
            }
        } else if confirm {
            println!();
            println!("Will destroy {} workspaces:", ws_to_merge.len());
            for ws in &ws_to_merge {
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

            destroy_workspaces(&ws_to_merge, &root)?;
        } else {
            println!();
            destroy_workspaces(&ws_to_merge, &root)?;
        }
    }

    // Show next steps for pushing
    if !has_conflicts {
        println!();
        println!("Next: push to remote:");
        println!("  jj bookmark set main -r @-");
        println!("    (bookmarks = jj's branches; @- = parent of working copy = your merge commit)");
        println!("  jj git push");
    }

    Ok(())
}

fn destroy_workspaces(workspaces: &[String], root: &Path) -> Result<()> {
    println!("Cleaning up workspaces...");
    let ws_dir = root.join(".workspaces");
    for ws in workspaces {
        if ws == "default" {
            println!("  Skipping default workspace");
            continue;
        }
        let path = ws_dir.join(ws);
        let _ = Command::new("jj")
            .args(["workspace", "forget", ws])
            .current_dir(root)
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
    /// Directories in .workspaces/ that jj doesn't know about
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

    // Get directories in .workspaces/
    // Security: validate names and skip symlinks to prevent traversal attacks
    let dir_workspaces: std::collections::HashSet<String> = if ws_dir.exists() {
        std::fs::read_dir(&ws_dir)
            .context("Failed to read .workspaces directory")?
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
        if jj_ws == "default" {
            continue; // default workspace lives at repo root, not in .workspaces/
        }
        if !dir_workspaces.contains(jj_ws) {
            analysis.missing.push(jj_ws.clone());
        }
    }

    // Find empty workspaces (if requested)
    if include_empty {
        for jj_ws in &jj_workspaces {
            if jj_ws == "default" {
                continue;
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
                println!("      Path: {}", path.display());
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
                println!("      Path: {}", path.display());
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

                let is_default = name == "default";
                let marker = if is_default { "*" } else { " " };

                if verbose {
                    println!("{marker} {name}");
                    println!("    {rest}");

                    if !is_default {
                        let path = workspace_path(name)?;
                        if path.exists() {
                            println!("    path: {}", path.display());
                        } else {
                            println!("    path: (missing!)");
                        }
                    }
                    println!();
                } else {
                    println!("{marker} {name}: {rest}");
                }
            }
        }
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

        let path = if include_path && name != "default" {
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
            is_default: name == "default",
            change_id,
            commit_id,
            description,
            path,
        });
    }

    Ok(workspaces)
}
