use std::path::Path;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use clap_complete::Shell;

use maw_cli::agents;
use maw_cli::changes;
use maw_cli::doctor;
use maw_cli::epoch;
use maw_cli::epoch_gc;
use maw_cli::exec;
use maw_cli::format;
use maw_cli::init;
use maw_cli::merge_cmd;
use maw_cli::migrate;
use maw_cli::push;
use maw_cli::ref_gc;
use maw_cli::release;
use maw_cli::status;
use maw_cli::telemetry;
use maw_cli::tldr;
use maw_cli::transport;
#[cfg(feature = "tui")]
use maw_cli::tui;
use maw_cli::upgrade;
use maw_cli::vocab_hints;
use maw_cli::workspace;

/// Multi-Agent Workspaces coordinator
///
/// maw coordinates multiple AI agents on the same codebase using
/// Manifold metadata and git worktrees. Each agent gets an isolated
/// workspace under `ws/<name>/` so edits can happen concurrently.
///
/// QUICK START:
///
///   maw ws create <your-name> --from origin/main
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
///   1. Create workspace: maw ws create <name> --from <source>
///   2. Edit files under ws/<name>/ (use absolute paths)
///   3. Save work with git commits in your workspace
///   4. Check status: maw ws status
///   5. Merge work: maw ws merge <name1> <name2> --into default
///   6. Resolve conflicts if needed, then continue
#[derive(Parser)]
#[command(name = "maw")]
#[command(version, about)]
#[command(propagate_version = true)]
// `after_help` is attached at runtime in `parse_cli_with_vocab_hints` so
// `maw --help` shows the same `maw tldr` quick-reference from one source.
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Manage agent workspaces
    ///
    /// Aliases: `ws` (the short canonical short-form used in agent loops
    /// and docs), `worktree`, `wt` (git-fluent aliases — `maw worktree
    /// create alice` and `maw wt create alice` both route to the same
    /// code as `maw ws create alice`; per the 2026-05-25 terminology
    /// decision, workspaces stays canonical in commands, docs, and on-
    /// disk paths, and the worktree/wt aliases give agents who reach
    /// for the git-fluent name from muscle memory a working command
    /// instead of an unrecognized-subcommand error; the alias also
    /// serves as a future-switch escape hatch).
    #[command(subcommand, visible_aliases = ["ws", "worktree", "wt"])]
    Workspace(workspace::WorkspaceCommands),

    /// Alias for 'maw ws list'
    #[command(hide = true, name = "ls")]
    Ls {
        /// Print only workspace names, one per line, no decoration
        #[arg(long)]
        names: bool,
    },

    /// Print the absolute path of a workspace (recipe for `cd`)
    ///
    /// `cd` itself can't persist between tool calls in a sandboxed agent
    /// shell, so this command instead prints the absolute on-disk path of
    /// the given workspace. Use it from a human shell with command
    /// substitution:
    ///
    ///   cd "$(maw cd alice)"
    ///
    /// In agent loops, prefer `maw exec <name> -- <cmd>` (the path-agnostic
    /// interface that survives the absolute-path doubling introduced by
    /// `.maw/workspaces/<name>/` in the consolidated layout — SP5 §6 risk #2).
    #[command(name = "cd", verbatim_doc_comment)]
    Cd {
        /// Workspace name (use "default" for the privileged target).
        name: String,
    },

    /// Manage AGENTS.md instructions
    #[command(subcommand)]
    Agents(agents::AgentsCommands),

    /// Manage tracked feature changes (branch + PR + linked workspaces)
    ///
    /// A change is an explicit, named unit of work (for example `ch-1xr`).
    /// Use `maw changes create ... --from ...` to start one, then merge
    /// workspaces into it with `maw ws merge ... --into change:<change-id>`.
    #[command(subcommand, verbatim_doc_comment)]
    Changes(changes::ChangesCommands),

    /// Initialize maw in the current repository
    ///
    /// Defaults to the consolidated `.maw/` layout for BOTH new and existing
    /// repos (root is a normal checkout, `.maw/workspaces/<name>/` for agents).
    /// On an existing repo this is non-destructive: the root stays the live
    /// checkout, no files are moved, and `.git/` stays a normal directory — it
    /// only adds `.maw/` and a `/.maw/` `.gitignore` entry.
    ///
    /// Pass `--legacy-ws` (or set `MAW_LAYOUT=v2`) to use the legacy v2 bare
    /// layout (bare root + `ws/default/` + `ws/<name>/`); on an existing repo
    /// this DOES restructure — it converts `.git/` to bare and moves your
    /// files into `ws/default/`. `--legacy-ws` is ignored on a repo that is
    /// already consolidated (maw won't downgrade an existing layout).
    ///
    /// Existing maw repos keep whatever layout they already have. Run
    /// `maw migrate` to move a v2 repo to the consolidated layout.
    ///
    /// Safe to run multiple times (idempotent — a re-run never restructures an
    /// already-initialized repo).
    Init {
        /// Use the legacy v2 `ws/` bare layout instead of the consolidated
        /// `.maw/` layout. On an existing repo this moves files into
        /// `ws/default/`; ignored if the repo is already consolidated.
        #[arg(long = "legacy-ws", alias = "v2")]
        legacy_ws: bool,
    },

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

        /// Apply auto-fixes for issues with a known-safe repair path
        /// (currently: `ff_absorbable` epoch drift → `maw epoch sync`
        /// equivalent). Skipped issues are reported as before.
        #[arg(long)]
        repair: bool,
    },

    /// Launch the terminal UI
    ///
    /// Interactive interface for managing workspaces, viewing commits,
    /// and coordinating agent work. Inspired by lazygit.
    #[cfg(feature = "tui")]
    #[command(name = "ui")]
    Ui,

    /// Quick repo and workspace status
    Status(status::StatusArgs),

    /// Upgrade v1 repo (.workspaces/) to v2 bare model (ws/) — DEPRECATED
    ///
    /// Migrates from the old .workspaces/ layout to the new bare repo model
    /// with ws/ directory, default workspace at ws/default/, and a bare
    /// common-dir topology (`repo.git`, root `.git` gitfile).
    /// Safe to run multiple times — detects v2 and exits early.
    ///
    /// NOTE: This is the legacy v1→v2 path. For v2→consolidated `.maw/`
    /// migration, use `maw migrate` (T3.3).
    Upgrade,

    /// Migrate a populated v2 `ws/` repo to the consolidated `.maw/` layout
    ///
    /// Implements the 14-step Prime-Invariant-preserving algorithm from
    /// notes/sg3-layout-design.md §7. Phases:
    ///
    ///   A. Preflight (refuse if a merge is in flight; enumerate worktrees)
    ///   B. Preserve  (pin recovery refs for every workspace)
    ///   C. Relocate  (ws/<name>/ → .maw/workspaces/<name>/)
    ///   D. Un-bare   (core.bare=false, materialize branch at root,
    ///                 decommission ws/default/, move .manifold/ → .maw/manifold/)
    ///   E. Finalize  (write/update root .gitignore, rmdir ws/, verify)
    ///
    /// Crash-safe via .manifold/migration/journal.json (then
    /// .maw/manifold/migration/journal.json after Phase D). Use
    /// `--resume` to continue an interrupted migration. Recovery refs
    /// pinned in Phase B remain available via `maw ws recover` even on
    /// catastrophic failure.
    #[command(verbatim_doc_comment)]
    Migrate {
        /// Resume an interrupted migration from the on-disk journal.
        #[arg(long)]
        resume: bool,
        /// Print the planned actions and exit without mutating the repo.
        #[arg(long)]
        dry_run: bool,
        /// Migrate even if the default workspace has uncommitted changes.
        /// By default migration refuses on a dirty tree (the working copy is
        /// rematerialized at root, so edits would only survive as a recovery
        /// snapshot). With this flag, changes are captured to a recovery ref.
        #[arg(long)]
        allow_dirty: bool,
    },

    /// Run a command inside a workspace directory
    ///
    /// Run any command inside a workspace -- useful for running tools
    /// like `br`, `bv`, `crit`, `cargo`, etc. inside a workspace without
    /// needing persistent `cd`.
    ///
    /// The workspace name is validated (no path traversal). Git
    /// commands auto-sync stale workspaces before running; other
    /// commands run without syncing.
    ///
    /// Examples:
    ///   maw exec alice -- cargo test
    ///   maw exec alice -- br list
    ///   maw exec alice -- ls -la src/
    #[command(verbatim_doc_comment)]
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
    /// heads, epoch pointer) to refs/manifold/* on the remote. This
    /// enables multi-machine Manifold collaboration (Level 2 Git
    /// transport, section 8).
    ///
    /// Configure the branch name in .maw.toml:
    ///   [repo]
    ///   branch = "main"
    #[command(verbatim_doc_comment)]
    Push(push::PushArgs),

    /// Fetch Manifold state from remote (Level 2 Git transport)
    ///
    /// Fetches all Manifold metadata (op logs, workspace heads, epoch
    /// pointer) from the remote under refs/manifold/* and merges remote
    /// op log heads into the local op log DAG.
    ///
    /// Divergent workspace heads are resolved by creating a synthetic
    /// merge operation that includes both chains as parents, preserving
    /// the full causal history.
    ///
    /// Epoch divergence (two machines with conflicting epoch pointers)
    /// is detected and reported but not auto-resolved -- manual recovery
    /// required.
    ///
    /// Use --dry-run to preview what would be merged without changing
    /// refs.
    ///
    /// Examples:
    ///   maw pull --manifold              # pull from origin
    ///   maw pull --manifold upstream     # pull from a named remote
    ///   maw pull --manifold --dry-run    # preview only
    #[command(verbatim_doc_comment)]
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
    #[command(verbatim_doc_comment)]
    Release(release::ReleaseArgs),

    /// Manage the epoch ref
    ///
    /// The epoch tracks which commit workspaces branch from. When the
    /// epoch falls behind the branch (e.g. after direct git commits),
    /// use `maw epoch sync` to resync without the side effects of
    /// `maw init`.
    #[command(subcommand)]
    Epoch(EpochCommands),

    /// Garbage-collect unreferenced epoch snapshots and stale refs
    ///
    /// Two artifacts to keep straight, always pruned together:
    ///   - recovery snapshot: the pinned commit (a `refs/manifold/recovery/*`
    ///     ref) holding a destroyed workspace's content. The ref is what stops
    ///     `git gc` from dropping the snapshot object.
    ///   - destroy record: the `maw ws recover` audit entry pointing at it
    ///     (`.maw/manifold/artifacts/ws/<name>/destroy/`).
    ///
    /// Without flags: removes `.manifold/epochs/e-<oid>` directories that
    /// are no longer referenced by any active workspace, and prunes
    /// dangling `refs/manifold/head/*` oplog head refs for workspaces that
    /// no longer exist (this clears the `maw doctor` "stale head refs"
    /// warning). Head refs owned by a live in-flight merge are preserved.
    ///
    /// With --recovery-snapshots: additionally removes old recovery snapshots
    /// whose commits are older than --older-than days (default: 30), AND prunes
    /// each destroyed workspace's matching destroy record in lockstep so the
    /// two never disagree (a swept ref never leaves a record claiming an
    /// unpinned snapshot). Records whose recovery ref was already swept by an
    /// older `maw gc --refs` are cleaned up too when older than --older-than.
    /// This is what makes `maw doctor`'s "abandoned-with-snapshot" count
    /// actually drop. Newer snapshots (and records for live workspaces) are
    /// kept.
    ///
    /// Examples:
    ///   maw gc                              # epoch GC + dangling head-ref cleanup
    ///   maw gc --dry-run                    # preview the above
    ///   maw gc --recovery-snapshots         # remove old snapshots + their records
    ///   maw gc --recovery-snapshots --dry-run        # preview snapshot cleanup
    ///   maw gc --recovery-snapshots --older-than 7   # remove snapshots older than 7 days
    ///   maw gc --recovery-snapshots --older-than 0   # drain the whole recover queue
    #[command(verbatim_doc_comment)]
    Gc {
        /// Preview removals without deleting anything
        #[arg(long)]
        dry_run: bool,

        /// Also remove recovery snapshots older than --older-than days, pruning
        /// each one's destroy record in lockstep. `--refs` is a deprecated alias.
        #[arg(long = "recovery-snapshots", alias = "refs")]
        recovery_snapshots: bool,

        /// For recovery snapshots: remove if older than this many days (default: 30)
        #[arg(long, default_value = "30")]
        older_than: u64,
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
    #[command(verbatim_doc_comment)]
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },

    /// Manage merge quarantine workspaces
    ///
    /// When post-merge validation fails with `on_failure = "quarantine"`
    /// or `on_failure = "block-quarantine"`, a quarantine workspace is
    /// created containing the candidate merge result. These commands let
    /// you promote (fix-forward) or abandon (discard) the quarantine.
    ///
    /// Examples:
    ///   maw merge list               # list active quarantines
    ///   maw merge promote abc123     # re-validate and commit if green
    ///   maw merge abandon abc123     # discard quarantine workspace
    #[command(subcommand, verbatim_doc_comment)]
    Merge(merge_cmd::MergeCommands),

    /// Quick reference: the common maw commands, grouped by task.
    ///
    /// A short, copy-pasteable cheat-sheet of affirmative usage — the
    /// verb-discoverability mitigation for the `vocabulary_scarcity`
    /// friction cluster (SG4 / bn-1t17). The same text is appended to
    /// `maw --help`. `crib` is a hidden alias (the former name).
    #[command(alias = "crib")]
    Tldr,
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

/// Parse the CLI; on parse failure, augment clap's error output with a
/// vocabulary-scarcity recovery hint before exiting.
///
/// This is the "self-describing output" half of the SG4 verb-discoverability
/// mitigation (bn-1t17): when an agent issues a verb that doesn't exist
/// (`maw ws new`, `maw checkout`, `maw stash`, ...), clap's default output
/// is `error: unrecognized subcommand 'X'` with no guidance — exactly the
/// `vocabulary_scarcity` cluster the SG4 backlog targets. We classify the
/// rejected token against the [`vocab_hints`] table and print a single
/// `did you mean: ...` line plus a universal "tip: --help / crib"
/// discoverability tail before exiting with clap's status code.
///
/// We only inject the hint on parse failure; successful parses are
/// untouched and there is zero overhead on the hot path.
fn parse_cli_with_vocab_hints() -> Cli {
    let raw_args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    // Attach the `maw tldr` quick-reference as the top-level after-help so
    // `maw --help` ends with the same cheat-sheet, sourced from one place.
    let command = Cli::command().after_help(tldr::quick_reference());
    match command.try_get_matches_from(&raw_args) {
        Ok(matches) => Cli::from_arg_matches(&matches).unwrap_or_else(|err| err.exit()),
        Err(err) => {
            // Reproduce clap's own rendering (with colour/exit semantics)
            // first so the user still gets the canonical error.
            let exit_code = err.exit_code();
            let _ = err.print();
            // Then layer our vocabulary-scarcity hint on top, if applicable.
            //
            // Skip arg[0] (the binary). Use string conversions; OsStr
            // tokens that fail UTF-8 simply skip classification (callers
            // see clap's default error alone).
            let token_strs: Vec<String> = raw_args
                .iter()
                .skip(1)
                .filter_map(|s| s.to_str().map(str::to_string))
                .collect();
            let tokens: Vec<&str> = token_strs.iter().map(String::as_str).collect();
            if let Some(hint) = vocab_hints::classify_rejected_verb(&tokens) {
                eprintln!("{}", hint.render());
            }
            // Always emit the universal discovery tail — even tokens we
            // can't classify benefit from the `--help` / `crib` pointer.
            eprintln!("{}", vocab_hints::UNIVERSAL_DISCOVERY_TAIL);
            std::process::exit(exit_code);
        }
    }
}

fn main() {
    let _telemetry = telemetry::init();
    // bn-263u: seed the failpoint registry from `MAW_FP` so the *shipped*
    // binary honours faults the faithful DST tier injects. Compiled away in
    // the default build (no-op `init_from_env`); only the
    // `--features failpoints` binary reads the env var. Zero-overhead release
    // contract preserved.
    maw_core::failpoints::init_from_env();
    let cli = parse_cli_with_vocab_hints();
    emit_migration_notice_if_needed();

    let result = match cli.command {
        Commands::Cd { name } => workspace::resolve_workspace_path_for_cd(&name).map(|path| {
            println!("{}", path.display());
        }),
        Commands::Workspace(cmd) => workspace::run(cmd),
        Commands::Ls { names } => workspace::run(workspace::WorkspaceCommands::List {
            verbose: false,
            check: false,
            format: None,
            json: false,
            names,
        }),
        Commands::Agents(ref cmd) => agents::run(cmd),
        Commands::Changes(ref cmd) => changes::run(cmd),
        Commands::Init { legacy_ws } => init::run_with(&init::InitRunOptions {
            legacy_ws_layout: legacy_ws,
        }),
        Commands::Upgrade => upgrade::run(),
        Commands::Migrate {
            resume,
            dry_run,
            allow_dirty,
        } => migrate::run(&migrate::MigrateOptions {
            resume,
            dry_run,
            allow_dirty,
        }),
        Commands::Doctor {
            format,
            json,
            repair,
        } => doctor::run_with_repair(format::OutputFormat::with_json_flag(format, json), repair),
        #[cfg(feature = "tui")]
        Commands::Ui => tui::run(),
        Commands::Status(ref cmd) => status::run(cmd),
        Commands::Push(args) => push::run(&args),
        Commands::Pull(ref args) => transport::run_pull(args),
        Commands::Release(args) => release::run(&args),
        Commands::Exec(args) => exec::run(&args),
        Commands::Epoch(cmd) => match cmd {
            EpochCommands::Sync => epoch::sync(),
        },
        Commands::Gc {
            dry_run,
            recovery_snapshots,
            older_than,
        } => workspace::repo_root().and_then(|root| {
            epoch_gc::run_cli(&root, dry_run)?;
            if recovery_snapshots {
                ref_gc::run_cli(&root, older_than, dry_run)?;
            } else {
                // bn-cm63: plain `maw gc` self-heals dangling oplog head refs
                // (e.g. leaked by a destroy-vs-merge race) so the documented
                // cleanup actually clears the `maw doctor` warning. The
                // recovery-ref age sweep stays exclusive to `maw gc --recovery-snapshots`.
                ref_gc::run_head_refs_cli(&root, dry_run)?;
            }
            Ok(())
        }),
        Commands::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "maw", &mut std::io::stdout());
            Ok(())
        }
        Commands::Merge(ref cmd) => merge_cmd::run(cmd),
        Commands::Tldr => tldr::run(),
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

    use std::fs;

    use clap::{CommandFactory, Parser};
    use tempfile::tempdir;

    use super::{Cli, Commands, should_emit_migration_notice};

    #[test]
    fn emits_notice_for_jj_only_repo() {
        let dir = tempdir().expect("operation should succeed");
        fs::create_dir_all(dir.path().join(".jj")).expect("operation should succeed");

        assert!(should_emit_migration_notice(dir.path()));
    }

    #[test]
    fn does_not_emit_notice_for_manifold_only_repo() {
        let dir = tempdir().expect("operation should succeed");
        fs::create_dir_all(dir.path().join(".manifold")).expect("operation should succeed");

        assert!(!should_emit_migration_notice(dir.path()));
    }

    #[test]
    fn does_not_emit_notice_when_both_exist() {
        let dir = tempdir().expect("operation should succeed");
        fs::create_dir_all(dir.path().join(".jj")).expect("operation should succeed");
        fs::create_dir_all(dir.path().join(".manifold")).expect("operation should succeed");

        assert!(!should_emit_migration_notice(dir.path()));
    }

    #[test]
    fn does_not_emit_notice_when_neither_exists() {
        let dir = tempdir().expect("operation should succeed");

        assert!(!should_emit_migration_notice(dir.path()));
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
    fn ws_merge_help_describes_supported_into_targets() {
        // T3.4 / bn-1jqo: the canonical subcommand registered with clap is
        // `workspace` (with `ws`, `worktree`, `wt` as visible aliases). The
        // help-walker keys on `get_name()`, which returns the canonical
        // name, so the help path is `maw workspace merge` — `maw ws merge`,
        // `maw worktree merge` and `maw wt merge` all dispatch to the
        // same registered command and share this help text.
        let mut help_texts = Vec::new();
        collect_help_texts(Cli::command(), "maw".to_string(), &mut help_texts);
        let help = help_texts
            .iter()
            .find_map(|(path, help)| (path == "maw workspace merge").then_some(help))
            .expect("maw workspace merge help should be present");

        assert!(
            help.contains(
                "Explicit merge target: default workspace, branch-attached workspace, or active change id"
            ),
            "unexpected help:\n{help}"
        );
        assert!(
            !help.contains("workspace name or change id"),
            "unexpected help:\n{help}"
        );
    }

    #[test]
    fn changes_subcommand_is_registered() {
        let cmd = Cli::command();
        let has_changes = cmd
            .get_subcommands()
            .any(|subcommand| subcommand.get_name() == "changes");
        assert!(
            has_changes,
            "expected 'changes' subcommand to be registered"
        );
    }

    #[test]
    fn ws_merge_requires_into_flag() {
        let result = Cli::try_parse_from(["maw", "ws", "merge", "alice"]);
        assert!(result.is_err(), "merge should require --into");
        let err = result.err().expect("expected parse error").to_string();
        assert!(err.contains("--into"), "error should mention --into: {err}");
    }

    /// bn-2to8: `maw ws recover --to`, `--into`, and `--restore-as` must all
    /// resolve to the same `WorkspaceCommands::Recover { to, .. }` field. The
    /// SG3 R6 friction was agents reaching for `--into` (merge's verb) on
    /// recover and hitting an unrecognized-flag error; the aliases make every
    /// spelling work. Parse-tree equivalence is the strongest static proof.
    #[test]
    fn ws_recover_to_into_restore_as_all_route_to_same_field() {
        use maw_cli::workspace::WorkspaceCommands;

        fn recover_to(flag: &str) -> Option<String> {
            let parsed = Cli::try_parse_from(["maw", "ws", "recover", "alice", flag, "restored"])
                .unwrap_or_else(|e| {
                    panic!("`maw ws recover alice {flag} restored` must parse: {e}")
                });
            let Commands::Workspace(WorkspaceCommands::Recover { to, .. }) = parsed.command else {
                panic!("`{flag}` must route to Commands::Workspace(Recover)");
            };
            to
        }

        assert_eq!(recover_to("--to").as_deref(), Some("restored"));
        assert_eq!(
            recover_to("--into").as_deref(),
            Some("restored"),
            "--into must alias --to"
        );
        assert_eq!(
            recover_to("--restore-as").as_deref(),
            Some("restored"),
            "--restore-as must alias --to"
        );
    }

    #[test]
    fn ws_create_parsing_defers_source_validation_to_runtime() {
        let result = Cli::try_parse_from(["maw", "ws", "create", "alice"]);
        assert!(
            result.is_ok(),
            "create parsing should succeed so runtime can emit actionable source guidance"
        );
    }

    #[test]
    fn ws_create_accepts_from_source() {
        let result = Cli::try_parse_from(["maw", "ws", "create", "alice", "--from", "main"]);
        assert!(result.is_ok(), "create with --from should parse");
    }

    // -----------------------------------------------------------------
    // SG4 / bn-1t17 — verb-discoverability mitigation surface.
    // -----------------------------------------------------------------

    /// `maw tldr` is the headline verb-discoverability surface and must be
    /// registered as a top-level subcommand (parity with `maw doctor`,
    /// `maw status`). Regression guard: if a future refactor drops the
    /// wiring, agents lose their cheat-sheet entry point and the
    /// `vocabulary_scarcity` cluster cannot drop to 0.
    #[test]
    fn tldr_subcommand_is_registered() {
        let cmd = Cli::command();
        let has_tldr = cmd
            .get_subcommands()
            .any(|subcommand| subcommand.get_name() == "tldr");
        assert!(
            has_tldr,
            "expected 'tldr' subcommand to be registered (SG4 / bn-1t17)"
        );
    }

    /// `maw tldr` parses without arguments — agents reach for it cold and
    /// it MUST succeed so they get a usable response, not a clap error.
    #[test]
    fn tldr_parses_with_no_args() {
        let err = Cli::try_parse_from(["maw", "tldr"]).err();
        assert!(
            err.is_none(),
            "`maw tldr` (no args) must parse so agents get a usable response: {}",
            err.map_or_else(String::new, |e| e.to_string()),
        );
    }

    /// `crib` is the former name, kept as a hidden alias so agents (and
    /// the prior `UNIVERSAL_DISCOVERY_TAIL` muscle-memory) don't break.
    /// It must route to the same `Tldr` command.
    #[test]
    fn crib_alias_routes_to_tldr() {
        let parsed =
            Cli::try_parse_from(["maw", "crib"]).expect("`maw crib` alias must still parse");
        assert!(
            matches!(parsed.command, Commands::Tldr),
            "`maw crib` must route to Commands::Tldr"
        );
    }

    /// `maw --help` must end with the `maw tldr` quick-reference — they are
    /// sourced from one place (attached as after-help at runtime). Pin that
    /// the cheat-sheet actually reaches the top-level help text.
    #[test]
    fn top_level_help_includes_tldr_quick_reference() {
        let mut cmd = Cli::command().after_help(maw_cli::tldr::quick_reference());
        let mut buf = Vec::new();
        let _ = cmd.write_long_help(&mut buf);
        let help = String::from_utf8_lossy(&buf);
        assert!(
            help.contains("QUICK REFERENCE") && help.contains("maw ws create"),
            "maw --help must append the tldr quick-reference"
        );
    }

    /// `maw ws new` is the canonical training-data verb (agents reach for
    /// it from git's `worktree add`). The vocabulary-hint classifier MUST
    /// route it to `maw ws create` — otherwise the agent has to guess.
    #[test]
    fn vocab_hint_routes_ws_new_to_ws_create() {
        let hint = maw_cli::vocab_hints::classify_rejected_verb(&["ws", "new"])
            .expect("ws new should classify as a known pitfall");
        assert!(
            hint.suggestion.contains("maw ws create"),
            "ws new should suggest `maw ws create`, got: {:?}",
            hint.suggestion,
        );
    }

    /// Universal-discovery tail names BOTH backstops (`--help` and
    /// `maw tldr`) so even unclassified verbs learn where to look. This
    /// is the "self-describing output" promise from the bn-1t17 brief.
    #[test]
    fn universal_discovery_tail_advertises_both_backstops() {
        let tail = maw_cli::vocab_hints::UNIVERSAL_DISCOVERY_TAIL;
        assert!(tail.contains("maw --help"));
        assert!(tail.contains("maw tldr"));
    }

    // -----------------------------------------------------------------
    // T3.4 / bn-1jqo — workspace-group alias surface.
    //
    // The 2026-05-25 terminology decision keeps `workspaces` canonical
    // in commands, docs, and on-disk paths AND exposes git-fluent
    // aliases (`worktree`, `wt`) so agents who reach for the git verb
    // from muscle memory get a working command instead of an
    // `unrecognized subcommand` error. The aliases are clap-native
    // (one `visible_aliases` annotation on the `Workspace` variant) —
    // they route to the SAME dispatch arm and therefore the SAME code
    // path as `maw ws`. These tests pin that contract so a future
    // refactor cannot silently strip the aliases or fork the dispatch.
    // -----------------------------------------------------------------

    /// All four entry forms (`workspace`, `ws`, `worktree`, `wt`)
    /// must parse a `create <name>` invocation. Equivalence at the
    /// parse-tree level is the strongest "they route to the same code"
    /// guarantee a static test can offer — Cli is one enum, all four
    /// resolve to `Commands::Workspace(WorkspaceCommands::Create { … })`
    /// with identical inner fields.
    #[test]
    fn workspace_group_aliases_all_route_to_same_create() {
        use maw_cli::workspace::WorkspaceCommands;

        fn parse_create(name: &str) -> Cli {
            Cli::try_parse_from(["maw", name, "create", "alice", "--from", "main"])
                .unwrap_or_else(|e| panic!("`maw {name} create alice --from main` must parse: {e}"))
        }

        let canonical = parse_create("workspace");
        let ws = parse_create("ws");
        let worktree = parse_create("worktree");
        let wt = parse_create("wt");

        for parsed in [&canonical, &ws, &worktree, &wt] {
            let Commands::Workspace(WorkspaceCommands::Create { name, .. }) = &parsed.command
            else {
                panic!("alias must route to Commands::Workspace(Create), got something else");
            };
            assert_eq!(
                name.as_deref(),
                Some("alice"),
                "alias must preserve positional name"
            );
        }
    }

    /// Each alias must parse `list` identically too — a single subcommand
    /// is not enough to prove the group-level alias works. This is the
    /// belt-and-braces test that proves the aliases sit at the group
    /// boundary (between `maw` and the subcommand), not on individual
    /// leaf subcommands.
    #[test]
    fn workspace_group_aliases_all_route_to_same_list() {
        for name in ["workspace", "ws", "worktree", "wt"] {
            let parsed = Cli::try_parse_from(["maw", name, "list"])
                .unwrap_or_else(|e| panic!("`maw {name} list` must parse: {e}"));
            assert!(
                matches!(
                    parsed.command,
                    Commands::Workspace(maw_cli::workspace::WorkspaceCommands::List { .. })
                ),
                "`maw {name} list` must route to Commands::Workspace(List)"
            );
        }
    }

    /// The `workspace` subcommand's long-help must advertise all three
    /// aliases (`ws`, `worktree`, `wt`) so agents discover them via
    /// `maw --help` / `maw workspace --help` without needing to read
    /// AGENTS.md first. The `[aliases: …]` block is generated by clap
    /// when `visible_aliases` is set; this test guards against a
    /// regression that changes them to hidden `aliases = …`.
    #[test]
    fn workspace_group_aliases_advertised_in_top_level_help() {
        let mut help_texts = Vec::new();
        collect_help_texts(Cli::command(), "maw".to_string(), &mut help_texts);

        let top_help = help_texts
            .iter()
            .find_map(|(path, help)| (path == "maw").then_some(help))
            .expect("top-level maw help should be present");

        for alias in ["ws", "worktree", "wt"] {
            assert!(
                top_help.contains(alias),
                "`maw --help` must advertise visible alias `{alias}` for the workspace group:\n{top_help}"
            );
        }
    }

    /// Clap stores the aliases on the registered subcommand object as
    /// "visible" aliases. Reading them back from `Cli::command()` is
    /// the static guarantee that the annotation has not been demoted
    /// to a hidden `alias` (which would still parse but vanish from
    /// `--help`, defeating the discoverability half of the contract).
    #[test]
    fn workspace_subcommand_carries_visible_aliases() {
        let cmd = Cli::command();
        let workspace_sub = cmd
            .get_subcommands()
            .find(|s| s.get_name() == "workspace")
            .expect("`workspace` subcommand must be registered");

        let visible_aliases: Vec<&str> = workspace_sub.get_visible_aliases().collect();
        for expected in ["ws", "worktree", "wt"] {
            assert!(
                visible_aliases.contains(&expected),
                "workspace subcommand must carry visible alias `{expected}` (got {visible_aliases:?})"
            );
        }
    }
}
