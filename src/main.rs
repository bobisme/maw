use std::path::Path;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

mod agents;
mod audit;
mod backend;
pub mod failpoints;
mod config;
mod doctor;
mod epoch;
mod epoch_gc;
#[allow(dead_code)]
mod error;
#[allow(dead_code)]
mod eval;
mod exec;
mod format;
#[allow(dead_code)]
mod merge;
mod merge_cmd;
#[allow(dead_code)]
mod merge_state;
#[allow(dead_code)]
mod model;
#[allow(dead_code)]
mod oplog;
mod push;
#[allow(dead_code)]
mod refs;
mod release;
mod status;
mod telemetry;
mod transport;
mod tui;
mod upgrade;
mod v2_init;
mod workspace;

/// Multi-Agent Workspaces coordinator
///
/// maw coordinates multiple AI agents on the same codebase using
/// Manifold metadata and git worktrees. Each agent gets an isolated
/// workspace under `ws/<name>/` so edits can happen concurrently.
///
/// QUICK START:
///
///   maw ws create <your-name>
///
///   # All file operations use the workspace path shown by create.
///   # Run tools inside your workspace:
///   maw exec <your-name> -- cargo test
///   maw exec <your-name> -- git status
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
///   3. Save work with git commits in your workspace
///   4. Check status: maw ws status
///   5. Merge work: maw ws merge <name1> <name2>
///   6. Resolve conflicts if needed, then continue
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
    /// Ensures .manifold/ directory structure is initialized.
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
    /// with ws/ directory, default workspace at ws/default/, and a bare
    /// common-dir topology (`repo.git`, root `.git` gitfile).
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
    /// `git push origin <branch>`. Checks sync status first and provides
    /// clear error messages if the branch is behind or doesn't exist.
    ///
    /// If your working copy parent (@-) has unpushed work but the branch
    /// hasn't been moved yet, use --advance to move it first.
    ///
    /// Use --manifold to also push Manifold metadata (op logs, workspace
    /// heads, epoch pointer) to refs/manifold/* on the remote. This enables
    /// multi-machine Manifold collaboration (Level 2 Git transport, §8).
    ///
    /// Configure the branch name in .maw.toml:
    ///   [repo]
    ///   branch = "main"
    Push(push::PushArgs),

    /// Fetch Manifold state from remote (Level 2 Git transport)
    ///
    /// Fetches all Manifold metadata (op logs, workspace heads, epoch pointer)
    /// from the remote under refs/manifold/* and merges remote op log heads
    /// into the local op log DAG.
    ///
    /// Divergent workspace heads are resolved by creating a synthetic merge
    /// operation that includes both chains as parents, preserving the full
    /// causal history.
    ///
    /// Epoch divergence (two machines with conflicting epoch pointers) is
    /// detected and reported but not auto-resolved — manual recovery required.
    ///
    /// Use --dry-run to preview what would be merged without changing refs.
    ///
    /// Examples:
    ///   maw pull --manifold              # pull from origin
    ///   maw pull --manifold upstream     # pull from a named remote
    ///   maw pull --manifold --dry-run    # preview only
    Pull(transport::PullArgs),

    /// Tag and push a release
    ///
    /// One command to replace the manual release sequence:
    ///   1. Advance branch bookmark to @- (your version bump commit)
    ///   2. Push branch to origin
    ///   3. Create and push git tag
    ///   4. Push tag to origin
    ///
    /// Usage: maw release v0.30.0
    ///
    /// Assumes your version bump is already committed. Run this after:
    ///   <edit version> && git commit -m "chore: bump to vX.Y.Z"
    Release(release::ReleaseArgs),

    /// Manage the epoch ref
    ///
    /// The epoch tracks which commit workspaces branch from. When the
    /// epoch falls behind the branch (e.g. after direct git commits),
    /// use `maw epoch sync` to resync without the side effects of
    /// `maw init`.
    #[command(subcommand)]
    Epoch(EpochCommands),

    /// Garbage-collect unreferenced epoch snapshots
    ///
    /// Removes `.manifold/epochs/e-<oid>` directories that are no longer
    /// referenced by any active workspace and are not the current epoch.
    Gc {
        /// Preview removals without deleting anything
        #[arg(long)]
        dry_run: bool,
    },

    /// Generate shell completions
    ///
    /// Prints shell completion scripts to stdout. Source the output
    /// in your shell config to enable tab completion for maw commands.
    ///
    /// Examples:
    ///   maw completions fish > ~/.config/fish/completions/maw.fish
    ///   maw completions bash > ~/.local/share/bash-completion/completions/maw
    ///   maw completions zsh > ~/.zfunc/_maw
    Completions {
        /// Shell to generate completions for
        shell: Shell,
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

#[derive(Subcommand)]
enum EpochCommands {
    /// Resync epoch ref to the configured branch HEAD
    ///
    /// Advances refs/manifold/epoch/current to match the branch tip.
    /// Use this after making direct git commits outside of maw.
    /// Unlike `maw init`, this only touches the epoch ref.
    Sync,
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

    tracing::warn!("Detected legacy jj repo (.jj/ present, .manifold/ missing)");
    eprintln!("IMPORTANT: maw now uses git worktrees instead of jj workspaces.");
    eprintln!("Next: run `maw init` to bootstrap .manifold/ metadata in this repo.");
    eprintln!("If migrating from v1 (.workspaces/), run `maw upgrade` first.");
    eprintln!("Your repository history is preserved.");
}

fn main() {
    let _telemetry = telemetry::init();
    let cli = Cli::parse();
    emit_migration_notice_if_needed();

    let result = match cli.command {
        Commands::Workspace(cmd) | Commands::Ws(cmd) => workspace::run(cmd),
        Commands::Agents(ref cmd) => agents::run(cmd),
        Commands::Init => v2_init::run(),
        Commands::Upgrade => upgrade::run(),
        Commands::Doctor { format, json } => {
            doctor::run(format::OutputFormat::with_json_flag(format, json))
        }
        Commands::Ui => tui::run(),
        Commands::Status(ref cmd) => status::run(cmd),
        Commands::Push(args) => push::run(&args),
        Commands::Pull(ref args) => transport::run_pull(args),
        Commands::Release(args) => release::run(&args),
        Commands::Exec(args) => exec::run(&args),
        Commands::Epoch(cmd) => match cmd {
            EpochCommands::Sync => epoch::sync(),
        },
        Commands::Gc { dry_run } => epoch_gc::run_cli(dry_run),
        Commands::Completions { shell } => {
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "maw",
                &mut std::io::stdout(),
            );
            Ok(())
        }
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
#[allow(clippy::needless_pass_by_value)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::{Path, PathBuf};

    use clap::CommandFactory;
    use tempfile::tempdir;

    use super::{Cli, should_emit_migration_notice};

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

    fn collect_rs_files(root: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let Ok(entries) = fs::read_dir(root) else {
            return files;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(collect_rs_files(&path));
                continue;
            }

            if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }

        files
    }

    fn has_jj_token(content: &str) -> bool {
        content
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|token| token.eq_ignore_ascii_case("jj") || token.eq_ignore_ascii_case("jujutsu"))
    }

    fn collect_help_texts(
        cmd: clap::Command,
        command_path: String,
        output: &mut Vec<(String, String)>,
    ) {
        let mut renderable = cmd.clone();
        let mut help = Vec::new();
        let _ = renderable.write_long_help(&mut help);
        output.push((
            command_path.clone(),
            String::from_utf8_lossy(&help).into_owned(),
        ));

        for sub in cmd.get_subcommands() {
            let sub = sub.clone();
            let path = if command_path.is_empty() {
                sub.get_name().to_string()
            } else {
                format!("{command_path} {}", sub.get_name())
            };
            collect_help_texts(sub, path, output);
        }
    }

    #[test]
    fn jj_runtime_calls_are_migration_only() {
        let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let files = collect_rs_files(&src_root);
        let mut offenders = Vec::new();

        for file in files {
            if file.ends_with("src/upgrade.rs") {
                continue;
            }

            let Ok(content) = fs::read_to_string(&file) else {
                continue;
            };

            if content.contains("Command::new(\"jj\")") {
                offenders.push(file);
            }
        }

        assert!(
            offenders.is_empty(),
            "Non-migration jj runtime calls found: {offenders:#?}"
        );
    }

    #[test]
    fn help_text_is_jj_free() {
        let mut help_texts = Vec::new();
        collect_help_texts(Cli::command(), "maw".to_string(), &mut help_texts);

        let offenders: Vec<_> = help_texts
            .into_iter()
            .filter_map(|(command_path, help)| has_jj_token(&help).then_some(command_path))
            .collect();

        assert!(
            offenders.is_empty(),
            "Unexpected jj mentions in help output: {offenders:#?}"
        );
    }

    #[test]
    fn no_deprecated_ws_jj_help_text_in_source() {
        let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let files = collect_rs_files(&src_root);
        let deprecated = ["maw", "ws", "jj"].join(" ");
        let mut offenders = Vec::new();

        for file in files {
            let Ok(content) = fs::read_to_string(&file) else {
                continue;
            };

            if content.contains(&deprecated) {
                offenders.push(file);
            }
        }

        assert!(
            offenders.is_empty(),
            "Deprecated ws-jj help text found: {offenders:#?}"
        );
    }

    #[test]
    fn jj_mentions_are_scoped_to_migration_files() {
        let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let files = collect_rs_files(&src_root);

        let allowed: BTreeSet<&str> = ["main.rs", "upgrade.rs", "doctor.rs", "workspace/create.rs"]
            .into_iter()
            .collect();

        let mut offenders = Vec::new();
        for file in files {
            let Ok(content) = fs::read_to_string(&file) else {
                continue;
            };

            if !has_jj_token(&content) {
                continue;
            }

            let Ok(rel) = file.strip_prefix(&src_root) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");

            if !allowed.contains(rel.as_str()) {
                offenders.push(rel);
            }
        }

        assert!(
            offenders.is_empty(),
            "Unexpected jj mentions outside allowlist: {offenders:#?}"
        );
    }
}
