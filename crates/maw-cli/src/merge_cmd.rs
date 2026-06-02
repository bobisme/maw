//! Top-level `maw merge` subcommand — quarantine lifecycle management.
//!
//! Provides `maw merge promote <merge_id>` and `maw merge abandon <merge_id>`.
//!
//! These commands manage quarantine workspaces created when post-merge
//! validation fails with `on_failure = "quarantine"` or `on_failure =
//! "block-quarantine"`. See [`maw::merge::quarantine`] for the quarantine
//! model.

use anyhow::{Context, Result, bail};
use clap::Subcommand;

use crate::format::OutputFormat;
use crate::workspace::{MawConfig, get_backend, repo_root};
use maw::merge::events::{self as merge_events, MergeEvent, MergeEventKind};
use maw::merge::last_conflict;
use maw::merge::quarantine::{
    PromoteResult, QuarantineError, abandon_quarantine, list_quarantines, promote_quarantine,
    quarantine_workspace_path,
};
use maw_core::config::ManifoldConfig;

/// `maw merge` subcommands.
#[derive(Subcommand)]
pub enum MergeCommands {
    /// Promote a quarantine workspace to a committed epoch.
    ///
    /// Re-runs validation in the quarantine workspace. If validation
    /// passes, advances the epoch and branch refs and cleans up the
    /// quarantine.
    ///
    /// The quarantine workspace is a normal git worktree -- you can
    /// edit files in it to fix the build failure before running promote.
    ///
    /// Examples:
    ///   maw merge promote abc123def456
    #[command(verbatim_doc_comment)]
    Promote {
        /// Quarantine ID (first 12 characters of the candidate commit OID).
        merge_id: String,
    },

    /// Abandon (discard) a quarantine workspace.
    ///
    /// Removes the quarantine workspace directory and its state file.
    /// Source workspaces are NOT affected -- the merge can be retried
    /// separately with `maw ws merge`.
    ///
    /// This operation is idempotent.
    ///
    /// Examples:
    ///   maw merge abandon abc123def456   # discard quarantine
    #[command(verbatim_doc_comment)]
    Abandon {
        /// Quarantine ID (first 12 characters of the candidate commit OID).
        merge_id: String,
    },

    /// List all active quarantine workspaces.
    ///
    /// Shows all quarantine workspaces with their ID, sources, branch,
    /// and validation failure details.
    ///
    /// Examples:
    ///   maw merge list
    #[command(verbatim_doc_comment)]
    List,

    /// Tail the merge engine's append-only event log (bn-yyx).
    ///
    /// Surfaces `integration_started`, `conflict_detected`,
    /// `integration_completed`, and `integration_aborted` events recorded
    /// by `maw ws merge` and `maw ws merge --check`. Lets an agent recall
    /// what the prior merge attempt did without re-running it.
    ///
    /// Examples:
    ///   maw merge events                       # full log (text)
    ///   maw merge events --format json         # machine-parseable
    ///   maw merge events --since-last-attempt  # bound to current attempt
    ///   maw merge events --since 1735689600000 # UNIX ms cutoff
    #[command(verbatim_doc_comment)]
    Events {
        /// UNIX-milliseconds cutoff; only events with `ts_unix_ms >= since`
        /// are returned.
        #[arg(long)]
        since: Option<i64>,
        /// Convenience: show only events since the most recent
        /// `integration_started`. Equivalent to filtering on the
        /// latest attempt's start time.
        #[arg(long)]
        since_last_attempt: bool,
        /// Output format: text or json (default text).
        #[arg(long)]
        format: Option<OutputFormat>,
    },

    /// Print the persisted "last conflict" surface (bn-yyx).
    ///
    /// After a `maw ws merge` attempt surfaces structured conflicts, the
    /// conflict surface is persisted at
    /// `.manifold/artifacts/merge/last-conflict.json` so an agent can
    /// recall the IDs, paths, sides, and copy-pasteable recovery commands
    /// WITHOUT re-running the merge. This is the primary affordance the
    /// `ws_merge_structured_conflict` friction-cluster mitigation provides.
    ///
    /// Examples:
    ///   maw merge last-conflict                # full surface (text)
    ///   maw merge last-conflict --format json  # machine-parseable
    #[command(verbatim_doc_comment)]
    LastConflict {
        /// Output format: text or json (default text).
        #[arg(long)]
        format: Option<OutputFormat>,
    },

    /// Resume a conflicted merge from the persisted last-conflict (bn-yyx).
    ///
    /// Reads `.manifold/artifacts/merge/last-conflict.json`, re-runs
    /// `maw ws merge` against the original sources + destination, and
    /// applies the supplied `--resolve` / `--resolve-all` strategies. Unlike
    /// re-issuing `maw ws merge` directly, this verb is identified as a
    /// RESUME of the prior attempt — agent benchmark attribution does not
    /// count it as a `ws_merge_structured_conflict` retry.
    ///
    /// Examples:
    ///   maw merge resume --resolve-all=alice
    ///   maw merge resume --resolve cf-aaaa=alice --resolve cf-bbbb=bob
    #[command(verbatim_doc_comment)]
    Resume {
        /// Stateless resolution flag, repeatable.
        /// Form: `cf-xxxx=<workspace>` or `cf-xxxx=content:<path>`.
        #[arg(long = "resolve", value_name = "ID=STRATEGY")]
        resolve: Vec<String>,
        /// Resolve all remaining conflicts by keeping `<workspace>`'s side.
        #[arg(long = "resolve-all", value_name = "WORKSPACE")]
        resolve_all: Option<String>,
        /// Dry-run: print the planned `maw ws merge` command without
        /// executing it. Useful for agents that want to confirm the
        /// derived recovery command before running it.
        #[arg(long)]
        dry_run: bool,
    },
}

/// # Errors
///
/// Returns an error if the selected merge command fails.
pub fn run(cmd: &MergeCommands) -> Result<()> {
    match cmd {
        MergeCommands::Promote { merge_id } => promote(merge_id),
        MergeCommands::Abandon { merge_id } => abandon(merge_id),
        MergeCommands::List => list(),
        MergeCommands::Events {
            since,
            since_last_attempt,
            format,
        } => events_cmd(*since, *since_last_attempt, *format),
        MergeCommands::LastConflict { format } => last_conflict_cmd(*format),
        MergeCommands::Resume {
            resolve,
            resolve_all,
            dry_run,
        } => resume_cmd(resolve, resolve_all.as_deref(), *dry_run),
    }
}

// ---------------------------------------------------------------------------
// promote
// ---------------------------------------------------------------------------

/// Re-validate and commit a quarantine workspace.
fn promote(merge_id: &str) -> Result<()> {
    let root = repo_root()?;
    let manifold_dir =
        maw_core::model::layout::LayoutFlavor::detect_with_env(&root).manifold_dir(&root);

    // Read state before promoting (for informational output)
    let state =
        maw::merge::quarantine::QuarantineState::read(&manifold_dir, merge_id).map_err(|e| {
            if matches!(e, QuarantineError::NotFound { .. }) {
                anyhow::anyhow!(
                    "No quarantine with id '{merge_id}' found.\n  \
                     List active quarantines: maw merge list"
                )
            } else {
                anyhow::anyhow!("{e}")
            }
        })?;

    let ws_path = quarantine_workspace_path(&root, merge_id);

    println!("Promoting quarantine '{merge_id}'...");
    println!();
    println!(
        "  Sources:  {}",
        state
            .sources
            .iter()
            .map(maw_core::model::types::WorkspaceId::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("  Branch:   {}", state.branch);
    println!("  Epoch:    {}", &state.epoch_before.as_str()[..12]);
    println!("  Worktree: {}", ws_path.display());
    println!();

    if !ws_path.exists() {
        bail!(
            "Quarantine worktree not found at {}\n  \
             State file exists but the worktree is missing.\n  \
             To discard: maw merge abandon {merge_id}",
            ws_path.display()
        );
    }

    // Load validation config from .manifold/config.toml
    let config_path = manifold_dir.join("config.toml");
    let manifold_config = ManifoldConfig::load(&config_path)
        .map_err(|e| anyhow::anyhow!("load manifold config: {e}"))?;
    let validation_config = &manifold_config.merge.validation;

    println!("VALIDATE: Re-running validation...");

    if !validation_config.has_commands() {
        println!("  No validation commands configured — treating as passed.");
        // No validation commands means we just commit as-is
        let config_skip = maw_core::config::ValidationConfig::default();
        match promote_quarantine(&root, &manifold_dir, merge_id, &config_skip) {
            Ok(PromoteResult::Committed { new_epoch }) => {
                print_promote_success(merge_id, &new_epoch.as_str()[..12], &state.branch);
                return Ok(());
            }
            Ok(PromoteResult::ValidationFailed {
                validation_result: _,
            }) => {
                unreachable!("no-op validation should always pass");
            }
            Err(e) => bail!("Promote failed: {e}"),
        }
    }

    match promote_quarantine(&root, &manifold_dir, merge_id, validation_config) {
        Ok(PromoteResult::Committed { new_epoch }) => {
            print_promote_success(merge_id, &new_epoch.as_str()[..12], &state.branch);
            Ok(())
        }
        Ok(PromoteResult::ValidationFailed {
            validation_result: r,
        }) => {
            println!(
                "  Validation STILL FAILING ({}ms, exit {:?})",
                r.duration_ms, r.exit_code
            );
            if !r.stderr.is_empty() {
                println!();
                println!("  Validation output:");
                for line in r.stderr.lines().take(15) {
                    eprintln!("    {line}");
                }
            }
            println!();
            println!("Fix the remaining issues and try again:");
            println!("  Edit files: {}/", ws_path.display());
            println!("  Re-try:     maw merge promote {merge_id}");
            println!("  Discard:    maw merge abandon {merge_id}");
            bail!("Quarantine promotion failed: validation still failing.")
        }
        Err(e) => bail!("Promote failed: {e}"),
    }
}

fn print_promote_success(merge_id: &str, new_epoch_short: &str, branch: &str) {
    println!("  Validation passed.");
    println!();
    println!("[OK] Quarantine '{merge_id}' promoted successfully!");
    println!();
    println!("  New epoch: {new_epoch_short}");
    println!("  Branch '{branch}' updated.");
    println!();
    println!("Next: push to remote:");
    println!("  maw push");
}

// ---------------------------------------------------------------------------
// abandon
// ---------------------------------------------------------------------------

/// Discard a quarantine workspace.
fn abandon(merge_id: &str) -> Result<()> {
    let root = repo_root()?;
    let manifold_dir =
        maw_core::model::layout::LayoutFlavor::detect_with_env(&root).manifold_dir(&root);

    let ws_path = quarantine_workspace_path(&root, merge_id);

    println!("Abandoning quarantine '{merge_id}'...");

    // Try to read state for informational output (non-fatal if missing)
    match maw::merge::quarantine::QuarantineState::read(&manifold_dir, merge_id) {
        Ok(state) => {
            println!(
                "  Sources: {}",
                state
                    .sources
                    .iter()
                    .map(maw_core::model::types::WorkspaceId::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Err(QuarantineError::NotFound { .. }) => {
            // Check if workspace still exists (partial cleanup)
            if !ws_path.exists() {
                println!("  Quarantine '{merge_id}' already abandoned.");
                return Ok(());
            }
        }
        Err(e) => {
            eprintln!("  WARNING: Could not read quarantine state: {e}");
        }
    }

    abandon_quarantine(&root, &manifold_dir, merge_id).context("Failed to abandon quarantine")?;

    println!("[OK] Quarantine '{merge_id}' abandoned.");
    println!();
    println!("Source workspaces are preserved.");
    println!("To retry the merge: maw ws merge <workspace...>");

    Ok(())
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// List all active quarantine workspaces.
fn list() -> Result<()> {
    let root = repo_root()?;
    let manifold_dir =
        maw_core::model::layout::LayoutFlavor::detect_with_env(&root).manifold_dir(&root);
    let maw_config = MawConfig::load(&root)?;
    let _backend = get_backend()?;

    let quarantines = list_quarantines(&manifold_dir);

    if quarantines.is_empty() {
        println!("No active quarantine workspaces.");
        return Ok(());
    }

    println!("{} quarantine workspace(s):", quarantines.len());
    println!();

    for q in &quarantines {
        let ws_path = quarantine_workspace_path(&root, &q.merge_id);
        let ws_exists = ws_path.exists();
        let _ = maw_config;

        println!("  ID:       {}", q.merge_id);
        println!(
            "  Sources:  {}",
            q.sources
                .iter()
                .map(maw_core::model::types::WorkspaceId::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!("  Branch:   {}", q.branch);
        println!("  Epoch:    {}", &q.epoch_before.as_str()[..12]);
        println!(
            "  Worktree: {} ({})",
            ws_path.display(),
            if ws_exists { "present" } else { "missing" }
        );
        println!(
            "  Failure:  exit {:?}, {}ms",
            q.validation_result.exit_code, q.validation_result.duration_ms
        );
        if !q.validation_result.stderr.is_empty() {
            let first_line = q.validation_result.stderr.lines().next().unwrap_or("");
            if !first_line.is_empty() {
                println!("  Output:   {first_line}...");
            }
        }
        println!();
        println!("  maw merge promote {}    # fix-forward", q.merge_id);
        println!("  maw merge abandon {}    # discard", q.merge_id);
        println!();
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// events (bn-yyx)
// ---------------------------------------------------------------------------

/// `maw merge events` — tail the merge event log.
fn events_cmd(
    since: Option<i64>,
    since_last_attempt: bool,
    format: Option<OutputFormat>,
) -> Result<()> {
    let root = repo_root()?;
    let manifold_dir =
        maw_core::model::layout::LayoutFlavor::detect_with_env(&root).manifold_dir(&root);
    let all = merge_events::read_events(&manifold_dir).context("read merge event log")?;

    let cutoff = if since_last_attempt {
        // Find the timestamp of the most recent IntegrationStarted.
        all.iter()
            .rev()
            .find(|e| matches!(e.kind, MergeEventKind::IntegrationStarted { .. }))
            .map_or(i64::MIN, |e| e.ts_unix_ms)
    } else {
        since.unwrap_or(i64::MIN)
    };

    let filtered: Vec<&MergeEvent> = all.iter().filter(|e| e.ts_unix_ms >= cutoff).collect();
    let fmt = OutputFormat::resolve(format);
    if fmt == OutputFormat::Json {
        let owned: Vec<MergeEvent> = filtered.into_iter().cloned().collect();
        println!("{}", fmt.serialize(&owned)?);
        return Ok(());
    }

    if filtered.is_empty() {
        println!("No merge events recorded.");
        println!();
        println!("The event log is created on the next `maw ws merge` attempt.");
        return Ok(());
    }

    println!("{} merge event(s):", filtered.len());
    println!();
    for ev in &filtered {
        print_event_line(ev);
    }
    Ok(())
}

fn print_event_line(ev: &MergeEvent) {
    let kind_label = match &ev.kind {
        MergeEventKind::IntegrationStarted { check_only, .. } => {
            if *check_only {
                "integration_started (check_only)"
            } else {
                "integration_started"
            }
        }
        MergeEventKind::ConflictDetected { .. } => "conflict_detected",
        MergeEventKind::IntegrationCompleted { .. } => "integration_completed",
        MergeEventKind::IntegrationAborted { .. } => "integration_aborted",
    };
    let detail = match &ev.kind {
        MergeEventKind::IntegrationStarted { sources, into, .. } => {
            format!("sources=[{}] into={}", sources.join(","), into)
        }
        MergeEventKind::ConflictDetected {
            sources,
            into,
            conflict_count,
            conflict_ids,
            ..
        } => format!(
            "sources=[{}] into={} count={conflict_count} ids=[{}]",
            sources.join(","),
            into,
            conflict_ids.join(",")
        ),
        MergeEventKind::IntegrationCompleted {
            sources,
            into,
            merge_commit,
        } => {
            let short = merge_commit.get(..12).unwrap_or(merge_commit.as_str());
            format!(
                "sources=[{}] into={} commit={short}",
                sources.join(","),
                into
            )
        }
        MergeEventKind::IntegrationAborted {
            sources,
            into,
            reason,
        } => format!(
            "sources=[{}] into={} reason={reason}",
            sources.join(","),
            into
        ),
    };
    println!("  {ts:>13}  {kind_label:<32}  {detail}", ts = ev.ts_unix_ms);
}

// ---------------------------------------------------------------------------
// last-conflict (bn-yyx)
// ---------------------------------------------------------------------------

/// `maw merge last-conflict` — print the persisted last-conflict surface.
fn last_conflict_cmd(format: Option<OutputFormat>) -> Result<()> {
    let root = repo_root()?;
    let manifold_dir =
        maw_core::model::layout::LayoutFlavor::detect_with_env(&root).manifold_dir(&root);
    let snapshot = merge_last_conflict_read(&manifold_dir)?;
    let fmt = OutputFormat::resolve(format);
    let Some(snapshot) = snapshot else {
        if fmt == OutputFormat::Json {
            // Stable shape for agents: `{ "present": false }`.
            println!("{}", serde_json::json!({ "present": false }));
        } else {
            println!("No persisted last-conflict snapshot.");
            println!();
            println!("This is created when `maw ws merge` (or --check) surfaces");
            println!("structured conflicts. After a successful merge it is cleared.");
        }
        return Ok(());
    };

    if fmt == OutputFormat::Json {
        // Wrap so a future schema bump (e.g. adding a `present:true` tag) is
        // a strict addition rather than a re-shaping of the agent-facing form.
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "present": true,
                "snapshot": snapshot,
            }))?
        );
        return Ok(());
    }

    println!("Last conflict (recorded {} ms UTC):", snapshot.ts_unix_ms);
    println!("  Sources: {}", snapshot.sources.join(", "));
    println!("  Into:    {}", snapshot.into);
    println!("  {} conflict(s):", snapshot.conflicts.len());
    for c in &snapshot.conflicts {
        println!(
            "    {id:<14}  {path:<48}  [{sides}]",
            id = c.id,
            path = c.path,
            sides = c.sides.join(", ")
        );
        if !c.reason.is_empty() {
            println!("    {:<14}  reason: {}", "", c.reason);
        }
    }
    println!();
    println!("Recovery commands (copy-paste any one):");
    for cmd in &snapshot.recovery_commands {
        println!("  {cmd}");
    }
    Ok(())
}

fn merge_last_conflict_read(
    manifold_dir: &std::path::Path,
) -> Result<Option<last_conflict::LastConflict>> {
    last_conflict::read(manifold_dir).context("read last-conflict snapshot")
}

// ---------------------------------------------------------------------------
// resume (bn-yyx)
// ---------------------------------------------------------------------------

/// `maw merge resume` — derive a `maw ws merge` invocation from the
/// persisted last-conflict and run it (or print it under `--dry-run`).
fn resume_cmd(resolve: &[String], resolve_all: Option<&str>, dry_run: bool) -> Result<()> {
    let root = repo_root()?;
    let manifold_dir =
        maw_core::model::layout::LayoutFlavor::detect_with_env(&root).manifold_dir(&root);
    let snapshot = merge_last_conflict_read(&manifold_dir)?.ok_or_else(|| {
        anyhow::anyhow!(
            "No persisted last-conflict to resume from.\n  \
             A `maw merge resume` requires a prior `maw ws merge` that surfaced conflicts.\n  \
             Check: maw merge last-conflict\n  \
             Or run a fresh merge: maw ws merge <workspaces> --into <target>"
        )
    })?;

    if resolve.is_empty() && resolve_all.is_none() {
        // Self-describing refusal: agent gets the exact command to retry with.
        let default_ws = snapshot
            .sources
            .first()
            .cloned()
            .unwrap_or_else(|| "WORKSPACE".to_string());
        bail!(
            "maw merge resume requires at least one --resolve or --resolve-all.\n  \
             To keep all of {default_ws}'s sides: maw merge resume --resolve-all={default_ws}\n  \
             To resolve per-conflict: maw merge resume --resolve cf-...=<workspace> ..."
        );
    }

    let mut argv: Vec<String> = Vec::new();
    argv.push("ws".into());
    argv.push("merge".into());
    argv.extend(snapshot.sources.iter().cloned());
    argv.push("--into".into());
    argv.push(snapshot.into.clone());
    for r in resolve {
        argv.push("--resolve".into());
        argv.push(r.clone());
    }
    if let Some(ws) = resolve_all {
        argv.push(format!("--resolve-all={ws}"));
    }

    if dry_run {
        println!("Would run: maw {}", argv.join(" "));
        println!();
        println!("Re-run without --dry-run to actually resume.");
        return Ok(());
    }

    println!(
        "Resuming merge: sources=[{}] into={}",
        snapshot.sources.join(", "),
        snapshot.into
    );
    println!("Invoking: maw {}", argv.join(" "));
    println!();

    // Re-exec ourselves through std::process::Command for a clean process
    // boundary — keeps merge_cmd.rs from importing the workspace::merge
    // private surface, and preserves the existing tracing / failpoints wiring
    // of `maw ws merge`.
    let current_exe =
        std::env::current_exe().context("locate current maw executable for resume re-exec")?;
    let status = std::process::Command::new(current_exe)
        .args(&argv)
        .current_dir(&root)
        .status()
        .context("re-exec `maw ws merge` for resume")?;
    if !status.success() {
        bail!(
            "Resumed `maw ws merge` exited with code {:?}.\n  \
             Check `maw merge last-conflict` for the current surface, then retry with adjusted --resolve flags.",
            status.code()
        );
    }
    Ok(())
}
