//! `maw ws advance <name>` — rebase a persistent workspace onto the latest epoch.
//!
//! Persistent workspaces can survive across epoch advances. When the mainline
//! epoch advances, a persistent workspace becomes stale. `maw ws advance` rebases
//! the workspace's changes (BOTH committed AND uncommitted) onto the new epoch.
//!
//! ## Algorithm (bn-8flz — orphan-safe choke-point)
//!
//! 1. Check that the workspace is persistent (mode = persistent).
//! 2. Get the workspace's current base epoch from `refs/manifold/epoch/ws/<name>`.
//! 3. Get the current epoch from `refs/manifold/epoch/current`.
//! 4. If already up-to-date, exit early.
//! 5. Detect committed-ahead work: count commits in `base_epoch..HEAD`.
//!    - If N > 0: route through `rebase_workspace_run` (the guarded sync replay
//!      path that also handles uncommitted changes via snapshot/replay). This is
//!      the same path `maw ws sync` uses and is guarded by the bn-20sa
//!      never-abandon guard + oplog.
//!    - If N == 0: snapshot uncommitted changes, native `checkout_detach` to
//!      the new epoch (fast-forward), replay snapshot. No orphan risk here.
//! 6. Update the per-workspace epoch ref.
//! 7. Report conflicts if any (working-copy-preserving — left as markers).
//!
//! The fix closes the bn-8flz orphan bug: previously, step 5 was missing
//! entirely — `checkout_to(new_epoch)` ran unconditionally over committed work.

use std::path::Path;

use anyhow::{Context, Result, bail};
use maw_git::GitRepo as _;
use serde::Serialize;

use crate::format::OutputFormat;
use maw_core::model::types::{BaseEpoch, WorkspaceMode};
use maw_core::refs as manifold_refs;

use super::sync::rebase::rebase_workspace as rebase_workspace_for_advance;
use super::working_copy::{
    SnapshotReplayResult, WorkingCopyConflict, cleanup_snapshot, replay_snapshot,
    snapshot_working_copy,
};
use super::{DEFAULT_WORKSPACE, metadata, repo_root, workspace_path};

// ---------------------------------------------------------------------------
// Conflict info
// ---------------------------------------------------------------------------

/// Type alias preserving the original name for backward compatibility.
pub type AdvanceConflict = WorkingCopyConflict;

/// Result of a `maw ws advance` operation.
#[derive(Clone, Debug, Serialize)]
pub struct AdvanceResult {
    /// Name of the workspace that was advanced.
    pub workspace: String,
    /// Old base epoch (OID before advance).
    pub old_epoch: String,
    /// New base epoch (OID after advance).
    pub new_epoch: String,
    /// Whether the advance completed without conflicts.
    pub success: bool,
    /// Files with conflicts (empty on success).
    pub conflicts: Vec<AdvanceConflict>,
    /// Human-readable summary message.
    pub message: String,
    /// bn-2rnq: Prime-Invariant audit result. Omitted when the audit is
    /// disabled (`invariant.audit = false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invariant: Option<super::invariant_audit::AuditReport>,
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Run `maw ws advance <name>`.
///
/// Rebases committed AND uncommitted changes onto the latest epoch.
/// Reports conflicts as structured data if they occur.
///
/// bn-8flz: the orphan bug was that committed-ahead commits were silently
/// overwritten by `checkout_to(new_epoch)`. Fixed by detecting committed work
/// and routing through `rebase_workspace_run` (the guarded sync path).
#[allow(clippy::too_many_lines)]
pub fn advance(name: &str, format: OutputFormat) -> Result<()> {
    if name == DEFAULT_WORKSPACE {
        bail!(
            "Cannot advance the default workspace — it is always up to date.\n  \
             The default workspace is updated automatically during merge."
        );
    }

    let root = repo_root()?;
    // bn-13rc: advance rewrites the per-workspace epoch ref and (for
    // committed-ahead work) replays through the guarded rebase path — take the
    // repo-level epoch lock first, held until this function returns.
    let _epoch_lock = crate::epoch_lock::EpochLock::acquire(&root, "ws advance")?;
    // bn-2rnq: snapshot sibling HEADs before rebasing this workspace.
    let invariant_pre = super::invariant_audit::capture(&root);
    let ws_path = workspace_path(name)?;

    if !ws_path.exists() {
        bail!(
            "Workspace '{name}' not found at {}.\n  \
             Check existing workspaces: maw ws list",
            ws_path.display()
        );
    }

    // Read metadata — advance only works for persistent workspaces.
    let meta = metadata::read(&root, name)
        .with_context(|| format!("Failed to read metadata for workspace '{name}'"))?;

    if meta.mode != WorkspaceMode::Persistent {
        bail!(
            "Workspace '{name}' is ephemeral (the default mode).\n  \
             Only persistent workspaces can be advanced.\n  \
             \n  To create a persistent workspace: maw ws create --from main <name> --persistent\n  \
             To advance a persistent workspace after epoch change: maw ws advance <name>"
        );
    }

    // Read the workspace's *base* epoch from the epoch ref (not HEAD, which may
    // be ahead due to committed-ahead commits). This is the same ref that `sync`
    // uses for committed_ahead_of_epoch: see bn-18dj for why HEAD is wrong here.
    let old_epoch = {
        let epoch_ref = manifold_refs::workspace_epoch_ref(name);
        match manifold_refs::read_ref(&root, &epoch_ref)
            .with_context(|| format!("Failed to read epoch ref for workspace '{name}'"))?
        {
            Some(oid) => oid.as_str().to_owned(),
            None => {
                // No per-workspace epoch ref (pre-migration workspace or first
                // advance). Fall back to HEAD so advance still works.
                get_worktree_head(&ws_path)
                    .with_context(|| format!("Failed to get HEAD of workspace '{name}'"))?
            }
        }
    };

    // Get the current epoch from refs/manifold/epoch/current.
    let current_epoch =
        manifold_refs::read_epoch_current(&root).with_context(|| "Failed to read current epoch")?;

    let Some(current_epoch) = current_epoch else {
        bail!(
            "No epoch ref found. Run `maw init` to initialize the repository.\n  \
             Then retry: maw ws advance {name}"
        );
    };

    // Already up to date?
    if old_epoch == current_epoch.as_str() {
        let report = super::invariant_audit::finish(&root, &invariant_pre, &[name], "ws advance")?;
        if format == OutputFormat::Json {
            let result = AdvanceResult {
                workspace: name.to_owned(),
                old_epoch: old_epoch.clone(),
                new_epoch: old_epoch,
                success: true,
                conflicts: vec![],
                message: format!("Workspace '{name}' is already at the current epoch."),
                invariant: report.is_enabled().then_some(report),
            };
            println!("{}", format.serialize(&result)?);
        } else {
            println!("Workspace '{name}' is already at the current epoch.");
            println!("  Epoch: {}...", &current_epoch.as_str()[..12]);
            println!();
            println!("Nothing to do.");
        }
        return Ok(());
    }

    let new_epoch = current_epoch.as_str().to_owned();
    let old_short = &old_epoch[..12.min(old_epoch.len())];
    let new_short = &new_epoch[..12.min(new_epoch.len())];

    if !matches!(format, OutputFormat::Json) {
        println!("Advancing workspace '{name}'...");
        println!("  From epoch: {old_short}...");
        println!("  To epoch:   {new_short}...");
        println!();
    }

    // bn-8flz: detect committed-ahead work BEFORE any HEAD movement.
    //
    // If the workspace has commits beyond its base epoch, route through
    // `rebase_workspace_run` — the same guarded path used by `maw ws sync`.
    // This path: (a) replays committed commits onto new_epoch, (b) handles
    // uncommitted changes via snapshot/replay internally, (c) writes
    // oplog + epoch ref, (d) applies the bn-20sa never-abandon guard.
    //
    // Previously this check did not exist, so `checkout_to(new_epoch)` ran
    // unconditionally, orphaning committed commits while printing "successfully."
    let base_epoch_typed =
        BaseEpoch::new(&old_epoch).map_err(|e| anyhow::anyhow!("invalid base epoch: {e}"))?;
    let committed_ahead =
        super::sync::checks::committed_ahead_of_epoch(&ws_path, &base_epoch_typed)
            .unwrap_or(u32::MAX); // treat "can't determine" as "has work" → rebase path

    if committed_ahead > 0 {
        // Committed-ahead path: route through rebase_workspace_run.
        // This replays committed commits ONTO new_epoch using the structured
        // merge engine, and also handles uncommitted changes via the rebase
        // path's snapshot. No separate snapshot step needed here.
        if !matches!(format, OutputFormat::Json) {
            println!("  Workspace has {committed_ahead} committed commit(s) ahead of base epoch.");
            println!("  Replaying commits onto new epoch via rebase...");
            println!();
        }

        rebase_workspace_for_advance(
            &root,
            name,
            &old_epoch,
            &new_epoch,
            &ws_path,
            committed_ahead,
            "advance",
        )
        .with_context(|| format!("Failed to rebase workspace '{name}' onto new epoch"))?;

        // Epoch ref is updated by rebase_workspace_run. Emit a success result.
        let report = super::invariant_audit::finish(&root, &invariant_pre, &[name], "ws advance")?;
        let message = format!(
            "Workspace '{name}' advanced (with rebase) from epoch {old_short}... to {new_short}... successfully."
        );
        let result = AdvanceResult {
            workspace: name.to_owned(),
            old_epoch: old_epoch.clone(),
            new_epoch: new_epoch.clone(),
            success: true,
            conflicts: vec![],
            message,
            invariant: report.is_enabled().then_some(report),
        };
        match format {
            OutputFormat::Json => println!("{}", format.serialize(&result)?),
            OutputFormat::Text => print_advance_text(&result),
            OutputFormat::Pretty => print_advance_pretty(&result),
        }
        return Ok(());
    }

    // Fast-forward path (0 committed-ahead): snapshot uncommitted changes, then
    // native checkout_detach to the new epoch, then replay snapshot.
    // This is safe: with 0 committed-ahead, moving HEAD to new_epoch cannot
    // orphan anything.

    // Step 1: Snapshot uncommitted changes (stash create + pinned ref).
    let snapshot = snapshot_working_copy(&ws_path, &root, name)
        .with_context(|| format!("Failed to snapshot workspace '{name}' before advance"))?;

    // Step 2: Native checkout_detach to the new epoch (no shell-out, bn-8flz).
    {
        let repo = maw_git::GixRepo::open(&ws_path)
            .with_context(|| format!("Failed to open repo for advance of workspace '{name}'"))?;
        let epoch_oid = repo
            .rev_parse(&new_epoch)
            .with_context(|| format!("Failed to resolve new epoch '{new_epoch}'"))?;
        if let Err(e) = repo.checkout_detach(epoch_oid, &ws_path) {
            // Checkout failed. The snapshot ref is preserved for recovery.
            if let Some(ref snap) = snapshot {
                eprintln!(
                    "  Snapshot preserved at: {}\n  \
                     To recover: git -C {} stash apply {}",
                    snap.ref_name,
                    ws_path.display(),
                    snap.oid,
                );
            }
            return Err(anyhow::anyhow!(e).context(format!(
                "Failed to checkout new epoch in workspace '{name}'"
            )));
        }
    }

    // Update the per-workspace epoch ref to the new epoch.
    // Silent failure leaves a stale ref → downstream misreports state (bn-3pkx).
    if let Ok(oid) = maw_core::model::types::GitOid::new(&new_epoch) {
        let epoch_ref = manifold_refs::workspace_epoch_ref(name);
        if let Err(e) = manifold_refs::write_ref(&root, &epoch_ref, &oid) {
            tracing::warn!(
                workspace = %name,
                epoch_ref = %epoch_ref,
                oid = %oid,
                error = %e,
                "failed to update workspace epoch ref after advance — downstream commands may see a stale epoch"
            );
        }
    }

    // Step 3: Replay the snapshot if there was one.
    let conflicts = if let Some(ref snapshot) = snapshot {
        match replay_snapshot(&ws_path, snapshot)? {
            SnapshotReplayResult::Clean => {
                // Clean replay — delete the snapshot ref.
                if let Err(e) = cleanup_snapshot(&root, name) {
                    tracing::warn!("failed to clean up snapshot ref: {e}");
                }
                vec![]
            }
            SnapshotReplayResult::Conflicts(c) => {
                // Conflicts — keep snapshot ref as recovery anchor.
                c
            }
        }
    } else {
        vec![]
    };

    let success = conflicts.is_empty();

    let message = if success {
        format!(
            "Workspace '{name}' advanced from epoch {old_short}... to {new_short}... successfully."
        )
    } else {
        format!(
            "Workspace '{name}' advanced from {old_short}... to {new_short}... with {} conflict(s).\n  \
             Resolve conflicts in {}, then continue working.",
            conflicts.len(),
            ws_path.display()
        )
    };

    let report = super::invariant_audit::finish(&root, &invariant_pre, &[name], "ws advance")?;
    let result = AdvanceResult {
        workspace: name.to_owned(),
        old_epoch: old_epoch.clone(),
        new_epoch: new_epoch.clone(),
        success,
        conflicts,
        message,
        invariant: report.is_enabled().then_some(report),
    };

    match format {
        OutputFormat::Json => {
            println!("{}", format.serialize(&result)?);
        }
        OutputFormat::Text => {
            print_advance_text(&result);
        }
        OutputFormat::Pretty => {
            print_advance_pretty(&result);
        }
    }

    if !success {
        // Propagate conflict as non-zero exit for script use.
        bail!("Advance completed with conflicts. Resolve them before continuing.");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Git helpers
// ---------------------------------------------------------------------------

/// Get the HEAD OID of a worktree (the workspace's current base epoch).
fn get_worktree_head(ws_path: &Path) -> Result<String> {
    let repo = maw_git::GixRepo::open(ws_path)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", ws_path.display()))?;
    let oid = repo
        .rev_parse("HEAD")
        .map_err(|e| anyhow::anyhow!("rev_parse HEAD failed: {e}"))?;
    Ok(oid.to_string())
}

// ---------------------------------------------------------------------------
// Output formatters
// ---------------------------------------------------------------------------

fn print_advance_text(result: &AdvanceResult) {
    println!("{}", result.message);
    println!();
    if result.conflicts.is_empty() {
        println!("Next: maw exec {} -- <command>", result.workspace);
    } else {
        println!("Conflicts:");
        for c in &result.conflicts {
            println!("  [{:>20}] {}", c.conflict_type, c.path);
        }
        println!();
        println!("Resolve conflicts manually, then continue working.");
    }
}

fn print_advance_pretty(result: &AdvanceResult) {
    let (green, yellow, bold, gray, reset) =
        ("\x1b[32m", "\x1b[33m", "\x1b[1m", "\x1b[90m", "\x1b[0m");

    if result.success {
        println!("{green}✓{reset} {bold}Advance complete{reset}");
        println!("{}", result.message);
        println!();
        println!(
            "{gray}Next: maw exec {} -- <command>{reset}",
            result.workspace
        );
    } else {
        println!("{yellow}⚠ Advance completed with conflicts{reset}");
        println!("{}", result.message);
        println!();
        println!("{bold}Conflicts:{reset}");
        for c in &result.conflicts {
            println!("  {yellow}[{:>20}]{reset} {}", c.conflict_type, c.path);
        }
        println!();
        println!(
            "Resolve conflicts manually in {bold}{}{reset}",
            result.workspace
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advance_result_success_serialize() {
        let r = AdvanceResult {
            workspace: "my-ws".to_owned(),
            old_epoch: "a".repeat(40),
            new_epoch: "b".repeat(40),
            success: true,
            conflicts: vec![],
            message: "Advanced successfully.".to_owned(),
            invariant: None,
        };
        let json = serde_json::to_string(&r).expect("operation should succeed");
        assert!(json.contains("\"success\":true"));
        assert!(json.contains("\"conflicts\":[]"));
    }

    #[test]
    fn advance_result_conflict_serialize() {
        let r = AdvanceResult {
            workspace: "my-ws".to_owned(),
            old_epoch: "a".repeat(40),
            new_epoch: "b".repeat(40),
            success: false,
            conflicts: vec![AdvanceConflict {
                path: "src/main.rs".to_owned(),
                conflict_type: "content".to_owned(),
            }],
            message: "Conflicts detected.".to_owned(),
            invariant: None,
        };
        let json = serde_json::to_string(&r).expect("operation should succeed");
        assert!(json.contains("\"success\":false"));
        assert!(json.contains("\"conflict_type\":\"content\""));
        assert!(json.contains("\"path\":\"src/main.rs\""));
    }
}
