use anyhow::Result;
use clap::{Parser, Subcommand};

mod agents;
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
///   3. jj tracks changes automatically
///   4. Merge agent work with `jj new agent-a agent-b` (creates merge commit)
///   5. Conflicts are recorded in commits, not blocking
///   6. Clean up with `maw ws destroy <name>`
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Workspace(cmd) | Commands::Ws(cmd) => workspace::run(cmd),
        Commands::Agents(cmd) => agents::run(cmd),
    }
}
