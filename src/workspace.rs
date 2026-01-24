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
}

pub fn run(cmd: WorkspaceCommands) -> Result<()> {
    match cmd {
        WorkspaceCommands::Create { name, revision } => create(&name, revision.as_deref()),
        WorkspaceCommands::Destroy { name, force } => destroy(&name, force),
        WorkspaceCommands::List { verbose } => list(verbose),
        WorkspaceCommands::Status => status(),
    }
}

fn workspaces_dir() -> Result<PathBuf> {
    let current = std::env::current_dir()?;
    Ok(current.join(".workspaces"))
}

fn workspace_path(name: &str) -> Result<PathBuf> {
    Ok(workspaces_dir()?.join(name))
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

    // Determine base revision
    let base = revision.map_or_else(
        || {
            // Try main, fall back to @
            let main_exists = Command::new("jj")
                .args(["log", "-r", "main", "--no-graph", "-T", "''"])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);

            if main_exists { "main" } else { "@" }.to_string()
        },
        ToString::to_string,
    );

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

    println!();
    println!("Workspace created! To start working:");
    println!();
    println!("  cd .workspaces/{name}");
    println!("  # make changes, jj tracks automatically");
    println!("  jj describe -m \"wip: what you're working on\"");
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
    if !status_stdout.trim().is_empty() {
        for line in status_stdout.lines() {
            println!("  {line}");
        }
    } else {
        println!("  (no changes)");
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
        if line.contains('@') {
            if let Some((name, _)) = line.split_once(':') {
                return Ok(name.trim().to_string());
            }
        }
    }

    Ok("default".to_string())
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
