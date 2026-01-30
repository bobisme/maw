use anyhow::Result;
use clap::{Parser, Subcommand};

mod agents;
mod doctor;
mod init;
mod tui;
mod workspace;

/// Multi-Agent Workflow coordinator
///
/// MAW coordinates multiple AI agents on the same codebase using jj
/// (Jujutsu), a git-compatible version control system. Each agent gets
/// an isolated working copy (separate view of the codebase) — edit
/// files concurrently without blocking each other.
///
/// KEY DIFFERENCES FROM GIT:
///   - No staging area — jj tracks all changes automatically (no git add)
///   - You're always in a commit — use 'describe' to set the message
///   - Conflicts are recorded in commits, not blocking
///
/// QUICK START:
///
///   maw ws create <your-name>
///
///   # All file operations use the workspace path shown by create.
///   # Run jj commands via maw (works in sandboxed environments):
///   maw ws jj <your-name> describe -m "feat: what you did"
///   #   ('describe' sets the commit message — like git commit --amend -m)
///   maw ws jj <your-name> diff
///
///   # Run other commands with cd:
///   cd /absolute/path/.workspaces/<your-name> && cargo test
///
///   # Check all agent work
///   maw ws status
///
/// WORKFLOW:
///
///   1. Create workspace: maw ws create <name>
///   2. Edit files under .workspaces/<name>/ (use absolute paths)
///   3. Save work: maw ws jj <name> describe -m "feat: ..."
///   4. Check status: maw ws status
///   5. Merge work: maw ws merge <name1> <name2>
///   6. Conflicts are recorded in commits, resolve and continue
#[derive(Parser)]
#[command(name = "maw")]
#[command(version, about)]
#[command(propagate_version = true)]
#[command(after_help = "See 'maw <command> --help' for more information on a specific command.")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Manage agent workspaces
    #[command(subcommand)]
    Workspace(workspace::WorkspaceCommands),

    /// Alias for 'workspace' (shorter to type)
    #[command(subcommand, name = "ws")]
    Ws(workspace::WorkspaceCommands),

    /// Manage AGENTS.md instructions
    #[command(subcommand)]
    Agents(agents::AgentsCommands),

    /// Initialize MAW in the current repository
    ///
    /// Ensures jj is initialized and .workspaces/ is gitignored.
    /// Safe to run multiple times.
    Init,

    /// Check system requirements and configuration
    ///
    /// Verifies that required tools (jj) are installed and optional tools
    /// (botbus, beads) are available. Also checks if you're in a jj repository.
    Doctor,

    /// Launch the terminal UI
    ///
    /// Interactive interface for managing workspaces, viewing commits,
    /// and coordinating agent work. Inspired by lazygit.
    #[command(name = "ui")]
    Ui,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Workspace(cmd) | Commands::Ws(cmd) => workspace::run(cmd),
        Commands::Agents(ref cmd) => agents::run(cmd),
        Commands::Init => init::run(),
        Commands::Doctor => doctor::run(),
        Commands::Ui => tui::run(),
    }
}
