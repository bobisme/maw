use std::path::Path;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

use maw_cli::agents;
use maw_cli::changes;
use maw_cli::crib;
use maw_cli::doctor;
use maw_cli::epoch;
use maw_cli::epoch_gc;
use maw_cli::exec;
use maw_cli::format;
use maw_cli::init;
use maw_cli::merge_cmd;
use maw_cli::push;
use maw_cli::ref_gc;
use maw_cli::release;
use maw_cli::status;
use maw_cli::telemetry;
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
#[command(after_help = "See 'maw <command> --help' for more information on a specific command.")]
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
    Ls,

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
    /// Greenfield init defaults to the consolidated `.maw/` layout (root is
    /// a normal checkout, `.maw/workspaces/<name>/` for agents). Pass
    /// `--legacy-ws` (or set `MAW_LAYOUT=v2`) to use the legacy v2 layout
    /// (bare root + `ws/default/` + `ws/<name>/`). Brownfield init on an
    /// existing repo preserves whichever layout is already on disk — use
    /// `maw migrate` (T3.3) to move a v2 repo to the consolidated layout.
    ///
    /// Safe to run multiple times.
    Init {
        /// Use the legacy v2 `ws/` layout instead of the consolidated `.maw/`
        /// layout (greenfield init only).
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

    /// Upgrade v1 repo (.workspaces/) to v2 bare model (ws/)
    ///
    /// Migrates from the old .workspaces/ layout to the new bare repo model
    /// with ws/ directory, default workspace at ws/default/, and a bare
    /// common-dir topology (`repo.git`, root `.git` gitfile).
    /// Safe to run multiple times — detects v2 and exits early.
    Upgrade,

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
    /// Without flags: removes `.manifold/epochs/e-<oid>` directories that
    /// are no longer referenced by any active workspace, and prunes
    /// dangling `refs/manifold/head/*` oplog head refs for workspaces that
    /// no longer exist (this clears the `maw doctor` "stale head refs"
    /// warning). Head refs owned by a live in-flight merge are preserved.
    ///
    /// With --refs: additionally sweeps old `refs/manifold/recovery/*`
    /// refs whose commits are older than --older-than days (default: 30).
    ///
    /// Examples:
    ///   maw gc                    # epoch GC + dangling head-ref cleanup
    ///   maw gc --dry-run          # preview the above
    ///   maw gc --refs             # also sweep old recovery refs
    ///   maw gc --refs --dry-run   # preview ref cleanup
    ///   maw gc --refs --older-than 7  # delete recovery refs older than 7 days
    #[command(verbatim_doc_comment)]
    Gc {
        /// Preview removals without deleting anything
        #[arg(long)]
        dry_run: bool,

        /// Also clean up stale manifold refs (head + recovery)
        #[arg(long)]
        refs: bool,

        /// For recovery refs: delete if older than this many days (default: 30)
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

    /// Print an agent crib sheet — per-agent verb protocol (machine-friendly).
    ///
    /// Designed to be the FIRST call an agent (or its coordinator) makes
    /// at session start: emits the full maw verb surface in a copy-pasteable
    /// form (markdown by default; `--format json` for parseable consumption),
    /// the common vocabulary pitfalls, and the load-bearing "when NOT to
    /// reach for maw" overkill-line. This is the verb-discoverability
    /// mitigation for the `vocabulary_scarcity` friction cluster
    /// (see SG4 / bn-1t17).
    ///
    /// Examples:
    ///   maw crib claude                  # markdown cheat sheet for Claude
    ///   maw crib codex --format json     # JSON for programmatic ingest
    ///   maw crib --overkill-line         # one-line "when NOT to use maw"
    #[command(verbatim_doc_comment)]
    Crib(crib::CribArgs),
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
    match Cli::try_parse_from(&raw_args) {
        Ok(cli) => cli,
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
        Commands::Ls => workspace::run(workspace::WorkspaceCommands::List {
            verbose: false,
            check: false,
            format: None,
            json: false,
        }),
        Commands::Agents(ref cmd) => agents::run(cmd),
        Commands::Changes(ref cmd) => changes::run(cmd),
        Commands::Init { legacy_ws } => init::run_with(&init::InitRunOptions {
            legacy_ws_layout: legacy_ws,
        }),
        Commands::Upgrade => upgrade::run(),
        Commands::Doctor { format, json, repair } => doctor::run_with_repair(
            format::OutputFormat::with_json_flag(format, json),
            repair,
        ),
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
            refs,
            older_than,
        } => workspace::repo_root().and_then(|root| {
            epoch_gc::run_cli(&root, dry_run)?;
            if refs {
                ref_gc::run_cli(&root, older_than, dry_run)?;
            } else {
                // bn-cm63: plain `maw gc` self-heals dangling oplog head refs
                // (e.g. leaked by a destroy-vs-merge race) so the documented
                // cleanup actually clears the `maw doctor` warning. The
                // recovery-ref age sweep stays exclusive to `maw gc --refs`.
                ref_gc::run_head_refs_cli(&root, dry_run)?;
            }
            Ok(())
        }),
        Commands::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "maw", &mut std::io::stdout());
            Ok(())
        }
        Commands::Merge(ref cmd) => merge_cmd::run(cmd),
        Commands::Crib(ref args) => crib::run(args),
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

    /// `maw crib` is the headline verb-discoverability surface and must be
    /// registered as a top-level subcommand (parity with `maw doctor`,
    /// `maw status`). Regression guard: if a future refactor drops the
    /// wiring, agents lose their cheat-sheet entry point and the
    /// `vocabulary_scarcity` cluster cannot drop to 0.
    #[test]
    fn crib_subcommand_is_registered() {
        let cmd = Cli::command();
        let has_crib = cmd
            .get_subcommands()
            .any(|subcommand| subcommand.get_name() == "crib");
        assert!(
            has_crib,
            "expected 'crib' subcommand to be registered (SG4 / bn-1t17)"
        );
    }

    /// `maw crib` parses without arguments (default agent + default format).
    /// This is the "agent reaches for it without knowing the args" case —
    /// it MUST succeed so the agent gets a useful response, not a clap
    /// error.
    #[test]
    fn crib_parses_with_no_args() {
        let err = Cli::try_parse_from(["maw", "crib"]).err();
        assert!(
            err.is_none(),
            "`maw crib` (no args) must parse so agents get a usable response: {}",
            err.map_or_else(String::new, |e| e.to_string()),
        );
    }

    /// `maw crib claude --format json` is the canonical "machine-friendly
    /// per-agent protocol" invocation; pin its parseability so the
    /// integration shape doesn't quietly regress.
    #[test]
    fn crib_with_agent_and_json_format_parses() {
        let err = Cli::try_parse_from(["maw", "crib", "claude", "--format", "json"]).err();
        assert!(
            err.is_none(),
            "`maw crib claude --format json` must parse: {}",
            err.map_or_else(String::new, |e| e.to_string()),
        );
    }

    /// `maw crib --overkill-line` is the one-line "when NOT to use maw"
    /// surface — agents can paste this verbatim into a system prompt to
    /// avoid reaching for nonexistent verbs on tasks that don't need
    /// workspace coordination at all.
    #[test]
    fn crib_overkill_line_flag_parses() {
        let err = Cli::try_parse_from(["maw", "crib", "--overkill-line"]).err();
        assert!(
            err.is_none(),
            "`maw crib --overkill-line` must parse: {}",
            err.map_or_else(String::new, |e| e.to_string()),
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
    /// `maw crib`) so even unclassified verbs learn where to look. This
    /// is the "self-describing output" promise from the bn-1t17 brief.
    #[test]
    fn universal_discovery_tail_advertises_both_backstops() {
        let tail = maw_cli::vocab_hints::UNIVERSAL_DISCOVERY_TAIL;
        assert!(tail.contains("maw --help"));
        assert!(tail.contains("maw crib"));
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
