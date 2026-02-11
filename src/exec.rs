use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::workspace;

/// Error indicating the child process exited with a non-zero status.
/// Carries the exit code for the caller to propagate.
#[derive(Debug)]
pub struct ExitCodeError(pub i32);

impl std::fmt::Display for ExitCodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "command exited with code {}", self.0)
    }
}

impl std::error::Error for ExitCodeError {}

/// Run a command inside a workspace directory
///
/// Run any command inside a workspace — useful for running tools
/// like `br`, `bv`, `crit`, `cargo`, etc. inside a workspace without
/// needing persistent `cd`.
///
/// The workspace name is validated (no path traversal). Stale
/// workspaces are auto-synced before running the command.
///
/// Examples:
///   maw exec alice -- cargo test
///   maw exec alice -- br list
///   maw exec alice -- ls -la src/
#[derive(Args, Debug)]
pub struct ExecArgs {
    /// Workspace name
    pub workspace: String,

    /// Command and arguments to run (after --)
    #[arg(last = true, required = true)]
    pub cmd: Vec<String>,
}

pub fn run(args: &ExecArgs) -> Result<()> {
    if args.cmd.is_empty() {
        bail!(
            "No command specified.\n  \
             Usage: maw exec <workspace> -- <command> [args...]\n  \
             Example: maw exec alice -- cargo test"
        );
    }

    let path = workspace::workspace_path(&args.workspace)?;
    if !path.exists() {
        bail!(
            "Workspace '{}' does not exist at {}\n  \
             List workspaces: maw ws list\n  \
             Create one: maw ws create {}",
            args.workspace,
            path.display(),
            args.workspace
        );
    }

    // Auto-sync stale workspace before running
    workspace::auto_sync_if_stale(&args.workspace, &path)?;

    // Block `jj bookmark set <branch>` from non-default workspaces.
    // Setting the shared branch bookmark from an agent workspace causes
    // divergent bookmarks and breaks push for everyone.
    if args.cmd.first().is_some_and(|c| c == "jj") && args.workspace != "default"
        && let Ok(root) = workspace::repo_root()
            && let Ok(config) = workspace::MawConfig::load(&root) {
                let branch = config.branch();
                let rest: Vec<&str> = args.cmd[1..].iter().map(std::string::String::as_str).collect();
                // Look for "bookmark" followed by "set" followed by the branch name
                if let Some(bm_pos) = rest.iter().position(|&a| a == "bookmark")
                    && rest[bm_pos + 1..].contains(&"set")
                        && rest.contains(&branch) {
                            bail!(
                                "Blocked: `jj bookmark set {branch}` from non-default workspace '{ws}'.\n\n\
                                 Setting the '{branch}' bookmark from an agent workspace causes divergent\n\
                                 bookmarks and breaks push for all workspaces.\n\n\
                                 Instead, merge your work into default first:\n\
                                 \n  maw ws merge {ws}\n  maw push\n",
                                branch = branch,
                                ws = args.workspace,
                            );
                        }
            }

    let status = Command::new(&args.cmd[0])
        .args(&args.cmd[1..])
        .current_dir(&path)
        .status()
        .context(format!("Failed to run '{}'", args.cmd[0]))?;

    if !status.success() {
        return Err(ExitCodeError(status.code().unwrap_or(1)).into());
    }

    Ok(())
}
