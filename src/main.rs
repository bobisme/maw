use std::path::Path;

use clap::{Parser, Subcommand};

mod agents;
mod backend;
mod config;
mod doctor;
mod epoch_gc;
mod error;
mod exec;
mod format;
mod init;
mod merge;
mod merge_cmd;
mod merge_state;
mod model;
mod push;
mod refs;
mod release;
mod status;
mod tui;
mod upgrade;
mod v2_init;
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
    /// Ensures .manifold/ is initialized and ws/ is gitignored.
    /// Safe to run multiple times.
    Init,

    /// Check system requirements and configuration
    ///
    /// Verifies that required tools (git) are installed and optional tools
    /// (botbus, beads) are available.
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

    /// Quick repo and workspace status
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

    /// Garbage-collect unreferenced epoch snapshots
    ///
    /// Removes `.manifold/epochs/e-<oid>` directories that are no longer
    /// referenced by any active workspace and are not the current epoch.
    Gc {
        /// Preview removals without deleting anything
        #[arg(long)]
        dry_run: bool,
    },

    /// Manage merge quarantine workspaces
    ///
    /// When post-merge validation fails with `on_failure = "quarantine"` or
    /// `on_failure = "block-quarantine"`, a quarantine workspace is created
    /// containing the candidate merge result. These commands let you promote
    /// (fix-forward) or abandon (discard) the quarantine.
    ///
    /// Examples:
    ///   maw merge list               # list active quarantines
    ///   maw merge promote abc123     # re-validate and commit if green
    ///   maw merge abandon abc123     # discard quarantine workspace
    #[command(subcommand)]
    Merge(merge_cmd::MergeCommands),
}

fn should_emit_migration_notice(repo_root: &Path) -> bool {
    repo_root.join(".jj").is_dir() && !repo_root.join(".manifold").exists()
}

fn emit_migration_notice_if_needed() {
    let Ok(repo_root) = workspace::repo_root() else {
        return;
    };

    if !should_emit_migration_notice(&repo_root) {
        return;
    }

    eprintln!("WARNING: Detected legacy jj repo (.jj/ present, .manifold/ missing).");
    eprintln!("IMPORTANT: maw now uses git worktrees instead of jj workspaces.");
    eprintln!("Next: run `maw init` to bootstrap .manifold/ metadata in this repo.");
    eprintln!("If migrating from v1 (.workspaces/), run `maw upgrade` first.");
    eprintln!("Your repository history is preserved.");
}

fn main() {
    let cli = Cli::parse();
    emit_migration_notice_if_needed();

    let result = match cli.command {
        Commands::Workspace(cmd) | Commands::Ws(cmd) => workspace::run(cmd),
        Commands::Agents(ref cmd) => agents::run(cmd),
        Commands::Init => init::run(),
        Commands::Upgrade => upgrade::run(),
        Commands::Doctor { format, json } => {
            doctor::run(format::OutputFormat::with_json_flag(format, json))
        }
        Commands::Ui => tui::run(),
        Commands::Status(ref cmd) => status::run(cmd),
        Commands::Push(args) => push::run(&args),
        Commands::Release(args) => release::run(&args),
        Commands::Exec(args) => exec::run(&args),
        Commands::Gc { dry_run } => epoch_gc::run_cli(dry_run),
        Commands::Merge(ref cmd) => merge_cmd::run(cmd),
    };

    if let Err(e) = result {
        // If the error is an ExitCodeError from exec, propagate the exit code
        // without printing an error message (the child command already printed its own).
        if let Some(exit_err) = e.downcast_ref::<exec::ExitCodeError>() {
            std::process::exit(exit_err.0);
        }
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::should_emit_migration_notice;

    #[test]
    fn emits_notice_for_jj_only_repo() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".jj")).unwrap();

        assert!(should_emit_migration_notice(dir.path()));
    }

    #[test]
    fn does_not_emit_notice_for_manifold_only_repo() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".manifold")).unwrap();

        assert!(!should_emit_migration_notice(dir.path()));
    }

    #[test]
    fn does_not_emit_notice_when_both_exist() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".jj")).unwrap();
        fs::create_dir_all(dir.path().join(".manifold")).unwrap();

        assert!(!should_emit_migration_notice(dir.path()));
    }

    #[test]
    fn does_not_emit_notice_when_neither_exists() {
        let dir = tempdir().unwrap();

        assert!(!should_emit_migration_notice(dir.path()));
    }
}
