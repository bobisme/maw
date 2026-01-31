use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use rand::seq::IndexedRandom;

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
    },

    /// Show status of current workspace and all agent work
    ///
    /// Displays a comprehensive view of:
    /// - Current workspace state (changes, stale status)
    /// - All agent workspaces and their commits
    /// - Any conflicts that need resolution
    /// - Unmerged work across all workspaces
    Status,

    /// Sync workspace with repository (handle stale working copy)
    ///
    /// Run this at the start of every session. If the working copy is stale
    /// (another workspace modified shared commits, so your files are outdated),
    /// this updates your workspace to match. Safe to run even if not stale.
    Sync,

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
        WorkspaceCommands::List { verbose } => list(verbose),
        WorkspaceCommands::Status => status(),
        WorkspaceCommands::Sync => sync(),
        WorkspaceCommands::Jj { name, args } => jj_in_workspace(&name, &args),
        WorkspaceCommands::Merge {
            workspaces,
            destroy,
            confirm,
            message,
        } => merge(&workspaces, destroy, confirm, message.as_deref()),
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

fn status() -> Result<()> {
    // Get current workspace name
    let current_ws = get_current_workspace()?;

    println!("=== Workspace Status ===");
    println!();

    // Check if stale
    let stale_check = Command::new("jj")
        .args(["status"])
        .output()
        .context("Failed to run jj status")?;

    let status_output = String::from_utf8_lossy(&stale_check.stderr);
    let is_stale = status_output.contains("working copy is stale");

    if is_stale {
        println!("WARNING: Working copy is stale!");
        println!("  (Another workspace changed shared history — your files are outdated.)");
        println!("  Fix: maw ws sync");
        println!();
    }

    // Show current workspace status
    println!("Current: {current_ws}");
    let status_stdout = String::from_utf8_lossy(&stale_check.stdout);
    if status_stdout.trim().is_empty() {
        println!("  (no changes)");
    } else {
        for line in status_stdout.lines() {
            println!("  {line}");
        }
    }
    println!();

    // Get all workspaces and their commits
    println!("=== All Agent Work ===");
    println!();

    let ws_output = Command::new("jj")
        .args(["workspace", "list"])
        .output()
        .context("Failed to run jj workspace list")?;

    let ws_list = String::from_utf8_lossy(&ws_output.stdout);
    for line in ws_list.lines() {
        if let Some((name, rest)) = line.split_once(':') {
            let name = name.trim();
            let is_current = name == current_ws;
            let marker = if is_current { ">" } else { " " };
            println!("{marker} {name}: {}", rest.trim());
        }
    }
    println!();

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

    let conflicts = String::from_utf8_lossy(&log_output.stdout);
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

    // Check for divergent commits (same change ID, multiple commit IDs)
    // This can happen when concurrent jj operations modify the same commit
    let divergent_output = Command::new("jj")
        .args([
            "log",
            "--no-graph",
            "-T",
            r#"if(divergent, change_id.short() ++ " " ++ commit_id.short() ++ " " ++ description.first_line() ++ "\n", "")"#,
        ])
        .output()
        .context("Failed to check for divergent commits")?;

    let divergent = String::from_utf8_lossy(&divergent_output.stdout);
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

    Ok(())
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

fn sync() -> Result<()> {
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

fn merge(
    workspaces: &[String],
    destroy_after: bool,
    confirm: bool,
    message: Option<&str>,
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

    // Check for conflicts
    let status_output = Command::new("jj")
        .args(["status"])
        .current_dir(&root)
        .output()
        .context("Failed to check status")?;

    let status_text = String::from_utf8_lossy(&status_output.stdout);
    let has_conflicts = status_text.contains("conflict");

    println!();
    if has_conflicts {
        println!("WARNING: Merge has conflicts that need resolution.");
        println!("Run `jj status` to see conflicted files.");
    }

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

fn list(verbose: bool) -> Result<()> {
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
        println!("No workspaces found.");
        return Ok(());
    }

    println!("Workspaces:");
    println!();

    for line in list.lines() {
        // Parse: "name: change_id commit_id description"
        if let Some((name, rest)) = line.split_once(':') {
            let name = name.trim();
            let rest = rest.trim();

            let is_default = name == "default";
            let marker = if is_default { "*" } else { " " };

            if verbose {
                println!("{marker} {name}");
                println!("    {rest}");

                // Check if workspace path exists
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

    Ok(())
}
