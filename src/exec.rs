use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::workspace;

/// Run a command inside a workspace directory
///
/// Like `maw ws jj` but for any command — useful for running tools
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

    let status = Command::new(&args.cmd[0])
        .args(&args.cmd[1..])
        .current_dir(&path)
        .status()
        .context(format!("Failed to run '{}'", args.cmd[0]))?;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}
