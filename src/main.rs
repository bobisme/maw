use anyhow::Result;
use clap::{Parser, Subcommand};

mod agents;
mod doctor;
mod workspace;

/// Multi-Agent Workflow coordinator
///
/// MAW helps coordinate multiple AI agents working on the same codebase
/// using jj (Jujutsu) workspaces for isolation and concurrent edits.
///
/// Each agent gets its own workspace in .workspaces/<name>/ where they
/// can make changes independently. jj handles merging and conflict
/// detection automatically - agents never block each other.
///
/// QUICK START:
///
///   # Create a workspace for an agent
///   maw ws create alice
///
///   # Agent works in their workspace
///   cd .workspaces/alice
///   # ... make changes ...
///   jj describe -m "feat: implement feature X"
///
///   # See all agent work from any workspace
///   jj log --all
///
///   # When done, destroy the workspace
///   maw ws destroy alice
///
/// WORKFLOW:
///
///   1. Each agent gets a workspace via `maw ws create <name>`
///   2. Agents work independently in .workspaces/<name>/
///   3. jj tracks changes automatically (use `jj describe` to save)
///   4. Check status with `maw ws status`
///   5. Merge all agent work: `maw ws merge --all --destroy`
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

    /// Check system requirements and configuration
    ///
    /// Verifies that required tools (jj) are installed and optional tools
    /// (botbus, beads) are available. Also checks if you're in a jj repository.
    Doctor,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Workspace(cmd) | Commands::Ws(cmd) => workspace::run(cmd),
        Commands::Agents(cmd) => agents::run(cmd),
        Commands::Doctor => doctor::run(),
    }
}
