use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Subcommand;

/// Workspace subcommands
#[derive(Subcommand)]
pub enum WorkspaceCommands {
    /// Create a new workspace for an agent
    ///
    /// Creates an isolated jj workspace in .workspaces/<name>/ where an agent
    /// can work independently. The workspace shares the repository's backing
    /// store but has its own working copy.
    ///
    /// After creation, the agent should:
    ///   1. cd .workspaces/<name>
    ///   2. Start making changes (jj tracks automatically)
    ///   3. Use 'jj commit' or 'jj describe' to save work
    Create {
        /// Name for the workspace (typically the agent's name)
        name: String,

        /// Base revision to start from (default: main or @)
        #[arg(short, long)]
        revision: Option<String>,
    },

    /// Remove an agent's workspace
    ///
    /// Forgets the workspace from jj and removes the directory.
    /// Make sure any important changes have been committed and
    /// merged before destroying.
    Destroy {
        /// Name of the workspace to destroy
        name: String,

        /// Skip confirmation prompt
        #[arg(short, long)]
        force: bool,
    },

    /// List all workspaces
    ///
    /// Shows all jj workspaces with their current status including:
    /// - Current commit description
    /// - Whether the workspace is stale
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
    /// If the working copy is stale (main repo changed while you were working),
    /// this command runs `jj workspace update-stale` and shows what changed.
    /// Safe to run even if not stale.
    Sync,

    /// Run a jj command in an agent's workspace
    ///
    /// Proxies jj commands into the specified workspace directory.
    /// Useful for sandboxed environments (e.g. Claude Code) where
    /// cd and env vars don't persist between shell calls.
    ///
    /// Only runs jj - not arbitrary commands.
    ///
    /// Examples:
    ///   maw ws jj alice diff
    ///   maw ws jj alice log
    ///   maw ws jj alice describe -m "feat: new feature"
    Jj {
        /// Workspace name to run jj in
        name: String,

        /// Arguments to pass to jj
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Merge work from agent workspaces
    ///
    /// Creates a merge commit combining work from the specified workspaces.
    ///
    /// Examples:
    ///   maw ws merge alice bob             # merge alice and bob's work
    ///   maw ws merge alice bob --destroy   # merge and clean up workspaces
    Merge {
        /// Workspace names to merge
        #[arg(required = true)]
        workspaces: Vec<String>,

        /// Destroy workspaces after successful merge
        #[arg(long)]
        destroy: bool,

        /// Skip confirmation prompt (use with --destroy)
        #[arg(short, long)]
        force: bool,

        /// Custom merge commit message
        #[arg(short, long)]
        message: Option<String>,
    },
}

pub fn run(cmd: WorkspaceCommands) -> Result<()> {
    match cmd {
        WorkspaceCommands::Create { name, revision } => create(&name, revision.as_deref()),
        WorkspaceCommands::Destroy { name, force } => destroy(&name, force),
        WorkspaceCommands::List { verbose } => list(verbose),
        WorkspaceCommands::Status => status(),
        WorkspaceCommands::Sync => sync(),
        WorkspaceCommands::Jj { name, args } => jj_in_workspace(&name, &args),
        WorkspaceCommands::Merge {
            workspaces,
            destroy,
            force,
            message,
        } => merge(&workspaces, destroy, force, message.as_deref()),
    }
}

fn workspaces_dir() -> Result<PathBuf> {
    let current = std::env::current_dir()?;
    Ok(current.join(".workspaces"))
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
    let status = Command::new("jj")
        .args([
            "workspace",
            "add",
            path.to_str().unwrap(),
            "--name",
            name,
            "-r",
            &base,
        ])
        .status()
        .context("Failed to run jj workspace add")?;

    if !status.success() {
        bail!("jj workspace add failed");
    }

    // Create a dedicated commit for this agent to own
    // This prevents divergent commits when multiple agents work concurrently
    let new_status = Command::new("jj")
        .args(["new", "-m", &format!("wip: {name} workspace")])
        .current_dir(&path)
        .status()
        .context("Failed to create agent commit")?;

    if !new_status.success() {
        bail!("Failed to create dedicated commit for workspace");
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
    println!("  Commit: {change_id} (owned by {name})");
    println!("  Path:   .workspaces/{name}");
    println!();
    println!("To start working:");
    println!();
    println!("  cd .workspaces/{name}");
    println!("  # make changes, jj tracks automatically");
    println!("  jj describe -m \"feat: what you're implementing\"");
    println!();

    Ok(())
}

fn destroy(name: &str, force: bool) -> Result<()> {
    let path = workspace_path(name)?;

    if !path.exists() {
        bail!("Workspace does not exist at {}", path.display());
    }

    if !force {
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
        println!("  Run: jj workspace update-stale");
        println!("  Or:  maw ws sync");
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
        println!();
        for line in conflicts.lines() {
            println!("  ! {line}");
        }
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
        println!("  WARNING: These commits have divergent versions (same change, multiple commits).");
        println!("  This usually happens when concurrent operations modified the same commit.");
        println!();
        for line in divergent.lines() {
            if !line.trim().is_empty() {
                println!("  ~ {line}");
            }
        }
        println!();
        println!("  To fix: keep one version and abandon the others:");
        println!("    jj abandon <change-id>/0   # abandon unwanted version");
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

    println!("Workspace is stale, syncing...");
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
        bail!("Failed to sync workspace");
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
    force: bool,
    message: Option<&str>,
) -> Result<()> {
    let ws_to_merge = workspaces.to_vec();

    if ws_to_merge.is_empty() {
        println!("No workspaces to merge.");
        return Ok(());
    }

    if ws_to_merge.len() == 1 {
        println!("Only one workspace to merge. Use `jj rebase` to move it to main.");
        return Ok(());
    }

    println!("Merging workspaces: {}", ws_to_merge.join(", "));
    println!();

    // Build revision references using workspace@ syntax
    // This is more reliable than parsing workspace list output
    let revisions: Vec<String> = ws_to_merge.iter().map(|ws| format!("{ws}@")).collect();

    // Build merge commit message
    let msg = message.map_or_else(
        || format!("merge: combine work from {}", ws_to_merge.join(", ")),
        ToString::to_string,
    );

    // Create merge commit: jj new ws1@ ws2@ ws3@ -m "message"
    let mut args = vec!["new"];
    for rev in &revisions {
        args.push(rev);
    }
    args.push("-m");
    args.push(&msg);

    let status = Command::new("jj")
        .args(&args)
        .status()
        .context("Failed to run jj new")?;

    if !status.success() {
        bail!("Failed to create merge commit");
    }

    println!("Created merge commit: {msg}");

    // Check for conflicts
    let status_output = Command::new("jj")
        .args(["status"])
        .output()
        .context("Failed to check status")?;

    let status_text = String::from_utf8_lossy(&status_output.stdout);
    let has_conflicts = status_text.contains("conflict");

    println!();
    if has_conflicts {
        println!("WARNING: Merge has conflicts that need resolution.");
        println!("Run `jj status` to see conflicted files.");
    }

    // Optionally destroy workspaces (but not if there are conflicts!)
    if destroy_after {
        if has_conflicts {
            println!("NOT destroying workspaces due to conflicts.");
            println!("Resolve conflicts first, then run:");
            for ws in &ws_to_merge {
                println!("  maw ws destroy {ws}");
            }
        } else if !force {
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

            destroy_workspaces(&ws_to_merge)?;
        } else {
            println!();
            destroy_workspaces(&ws_to_merge)?;
        }
    }

    // Show next steps for pushing
    if !has_conflicts {
        println!();
        println!("To push to remote:");
        println!("  jj bookmark set main -r @-");
        println!("  jj git push");
    }

    Ok(())
}

fn destroy_workspaces(workspaces: &[String]) -> Result<()> {
    println!("Cleaning up workspaces...");
    for ws in workspaces {
        let path = workspace_path(ws)?;
        let _ = Command::new("jj")
            .args(["workspace", "forget", ws])
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
