use clap::{Parser, Subcommand};

mod agents;
mod doctor;
mod exec;
mod format;
mod init;
mod jj;
mod jj_intro;
mod push;
mod release;
mod status;
mod tui;
mod upgrade;
mod workspace;

/// Multi-Agent Workspaces coordinator
///
/// maw coordinates multiple AI agents on the same codebase using jj
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
///   maw exec <your-name> -- jj describe -m "feat: what you did"
///   #   ('describe' sets the commit message — like git commit --amend -m)
///   maw exec <your-name> -- jj diff
///
///   # Run other tools in your workspace:
///   maw exec <your-name> -- cargo test
///   maw exec <your-name> -- br list
///
///   # Check all agent work
///   maw ws status
///
/// WORKFLOW:
///
///   1. Create workspace: maw ws create <name>
///   2. Edit files under ws/<name>/ (use absolute paths)
///   3. Save work: maw exec <name> -- jj describe -m "feat: ..."
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

    /// Initialize maw in the current repository
    ///
    /// Ensures jj is initialized and ws/ is gitignored.
    /// Safe to run multiple times.
    Init,

    /// Check system requirements and configuration
    ///
    /// Verifies that required tools (jj) are installed and optional tools
    /// (botbus, beads) are available. Also checks if you're in a jj repository.
    Doctor {
        /// Output format: text, json, pretty (auto-detected from TTY)
        #[arg(long)]
        format: Option<format::OutputFormat>,

        /// Shorthand for --format json
        #[arg(long, hide = true, conflicts_with = "format")]
        json: bool,
    },

    /// Launch the terminal UI
    ///
    /// Interactive interface for managing workspaces, viewing commits,
    /// and coordinating agent work. Inspired by lazygit.
    #[command(name = "ui")]
    Ui,

    /// Quick jj reference for git users
    ///
    /// Shows jj mental model, git command equivalents, and how to push
    /// to GitHub. Designed for agents encountering jj for the first time.
    #[command(name = "jj-intro")]
    JjIntro,

    /// Brief repo and workspace status
    Status(status::StatusArgs),

    /// Upgrade v1 repo (.workspaces/) to v2 bare model (ws/)
    ///
    /// Migrates from the old .workspaces/ layout to the new bare repo model
    /// with ws/ directory, default workspace at ws/default/, and git core.bare = true.
    /// Safe to run multiple times — detects v2 and exits early.
    Upgrade,

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
    Exec(exec::ExecArgs),

    /// Push the main branch to remote
    ///
    /// Pushes the configured branch (default: main) to origin using
    /// `jj git push --bookmark <branch>`. Uses --bookmark explicitly so
    /// the push works from any workspace (including the default workspace
    /// where @ is not an ancestor of the branch).
    ///
    /// Checks sync status first and provides clear error messages if the
    /// branch is behind or doesn't exist.
    ///
    /// If your working copy parent (@-) has unpushed work but the branch
    /// bookmark hasn't been moved yet, use --advance to move it first.
    ///
    /// Configure the branch name in .maw.toml:
    ///   [repo]
    ///   branch = "main"
    Push(push::PushArgs),

    /// Tag and push a release
    ///
    /// One command to replace the manual release sequence:
    ///   1. Advance branch bookmark to @- (your version bump commit)
    ///   2. Push branch to origin
    ///   3. Create jj tag + git tag
    ///   4. Push tag to origin
    ///
    /// Usage: maw release v0.30.0
    ///
    /// Assumes your version bump is already committed. Run this after:
    ///   jj new && <edit version> && jj describe -m "chore: bump to vX.Y.Z"
    Release(release::ReleaseArgs),
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Workspace(cmd) | Commands::Ws(cmd) => workspace::run(cmd),
        Commands::Agents(ref cmd) => agents::run(cmd),
        Commands::Init => init::run(),
        Commands::Upgrade => upgrade::run(),
        Commands::Doctor { format, json } => doctor::run(format::OutputFormat::with_json_flag(format, json)),
        Commands::Ui => tui::run(),
        Commands::JjIntro => jj_intro::run(),
        Commands::Status(ref cmd) => status::run(cmd),
        Commands::Push(args) => push::run(&args),
        Commands::Release(args) => release::run(&args),
        Commands::Exec(args) => exec::run(&args),
    };

    if let Err(e) = result {
        // If the error is an ExitCodeError from exec, propagate the exit code
        // without printing an error message (the child command already printed its own).
        if let Some(exit_err) = e.downcast_ref::<exec::ExitCodeError>() {
            std::process::exit(exit_err.0);
        }
        eprintln!("Error: {e:?}");
        std::process::exit(1);
    }
}
