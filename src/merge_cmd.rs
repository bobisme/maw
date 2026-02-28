//! Top-level `maw merge` subcommand — quarantine lifecycle management.
//!
//! Provides `maw merge promote <merge_id>` and `maw merge abandon <merge_id>`.
//!
//! These commands manage quarantine workspaces created when post-merge
//! validation fails with `on_failure = "quarantine"` or `on_failure =
//! "block-quarantine"`. See [`crate::merge::quarantine`] for the quarantine
//! model.

use anyhow::{Context, Result, bail};
use clap::Subcommand;

use crate::config::ManifoldConfig;
use crate::merge::quarantine::{
    PromoteResult, QuarantineError, abandon_quarantine, list_quarantines, promote_quarantine,
    quarantine_workspace_path,
};
use crate::workspace::{MawConfig, get_backend, repo_root};

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
}

pub fn run(cmd: &MergeCommands) -> Result<()> {
    match cmd {
        MergeCommands::Promote { merge_id } => promote(merge_id),
        MergeCommands::Abandon { merge_id } => abandon(merge_id),
        MergeCommands::List => list(),
    }
}

// ---------------------------------------------------------------------------
// promote
// ---------------------------------------------------------------------------

/// Re-validate and commit a quarantine workspace.
fn promote(merge_id: &str) -> Result<()> {
    let root = repo_root()?;
    let manifold_dir = root.join(".manifold");

    // Read state before promoting (for informational output)
    let state =
        crate::merge::quarantine::QuarantineState::read(&manifold_dir, merge_id).map_err(|e| {
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
            .map(super::model::types::WorkspaceId::as_str)
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
        let config_skip = crate::config::ValidationConfig::default();
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
    let manifold_dir = root.join(".manifold");

    let ws_path = quarantine_workspace_path(&root, merge_id);

    println!("Abandoning quarantine '{merge_id}'...");

    // Try to read state for informational output (non-fatal if missing)
    match crate::merge::quarantine::QuarantineState::read(&manifold_dir, merge_id) {
        Ok(state) => {
            println!(
                "  Sources: {}",
                state
                    .sources
                    .iter()
                    .map(super::model::types::WorkspaceId::as_str)
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
    let manifold_dir = root.join(".manifold");
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
                .map(super::model::types::WorkspaceId::as_str)
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
