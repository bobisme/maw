use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Args;
use tracing::instrument;

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
/// The workspace name is validated (no path traversal). Git commands auto-sync
/// stale workspaces before execution; other commands run against the workspace
/// as-is.
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

fn should_auto_sync(cmd: &str) -> bool {
    matches!(cmd.rsplit(['/', '\\']).next(), Some("git" | "git.exe"))
}

#[instrument(skip(args), fields(workspace = %args.workspace, cmd = ?args.cmd))]
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
             Create one: maw ws create --from main {}",
            args.workspace,
            path.display(),
            args.workspace
        );
    }

    if should_auto_sync(&args.cmd[0]) {
        workspace::auto_sync_if_stale(&args.workspace, &path)?;
    }

    let mut cmd = Command::new(&args.cmd[0]);
    cmd.args(&args.cmd[1..]).current_dir(&path);

    // Propagate trace context to child process so it joins the same trace
    if let Some(traceparent) = crate::telemetry::current_traceparent() {
        cmd.env("TRACEPARENT", traceparent);
    }

    let status = cmd
        .status()
        .context(format!("Failed to run '{}'", args.cmd[0]))?;

    if !status.success() {
        return Err(ExitCodeError(status.code().unwrap_or(1)).into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::should_auto_sync;

    #[test]
    fn auto_syncs_git_commands() {
        assert!(should_auto_sync("git"));
        assert!(should_auto_sync("/usr/bin/git"));
        assert!(should_auto_sync("C:\\Program Files\\Git\\bin\\git.exe"));
    }

    #[test]
    fn skips_auto_sync_for_non_git_commands() {
        assert!(!should_auto_sync("cargo"));
        assert!(!should_auto_sync("sigil"));
    }
}
