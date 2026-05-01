use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use maw_core::model::types::{GitOid, WorkspaceId};
use maw_core::oplog::checkpoint::{
    CheckpointError, DEFAULT_CHECKPOINT_INTERVAL, materialize_from_checkpoint,
    maybe_write_checkpoint,
};
use maw_core::oplog::read::OpLogReadError;
use maw_core::oplog::types::Operation;
use maw_core::oplog::view::read_patch_set_blob;
use maw_core::oplog::write::{OpLogWriteError, append_operation};
use maw_core::refs;

/// Workspace names for which we've already logged a damaged-oplog warning
/// this session. Prevents per-merge warning spam when the chain has a
/// dangling reference (bn-3h90 Bug 2).
static DAMAGED_OPLOG_WARNED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn first_damaged_oplog_warning_for(ws_name: &str) -> bool {
    let mut guard = DAMAGED_OPLOG_WARNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let seen = guard.get_or_insert_with(HashSet::new);
    let should_warn = seen.insert(ws_name.to_owned());
    drop(guard);
    should_warn
}

pub fn append_operation_with_runtime_checkpoint(
    root: &Path,
    workspace_id: &WorkspaceId,
    op: &Operation,
    old_head: Option<&GitOid>,
) -> Result<GitOid, OpLogWriteError> {
    let new_head = append_operation(root, workspace_id, op, old_head)?;

    match maybe_checkpoint_after_append(root, workspace_id, &new_head) {
        Ok(()) => {}
        Err(CheckpointAppendError::DamagedChain(detail)) => {
            // The op log chain has a dangling blob reference (likely from a
            // destroyed-then-recreated workspace prior to v0.58.3, which
            // didn't clean up `refs/manifold/head/<name>` on destroy).
            // New operations still append fine — we just can't write a new
            // checkpoint until the chain is repaired. Log once per workspace
            // per session to avoid spamming every merge.
            let ws_name = workspace_id.as_str();
            let should_warn = first_damaged_oplog_warning_for(ws_name);
            if should_warn {
                eprintln!(
                    "WARNING: op log chain for workspace '{ws_name}' has a dangling blob reference \
                     — checkpoint writes are disabled for this workspace until repaired. \
                     This is non-fatal; merges will continue to work.\n  \
                     Detail: {detail}\n  \
                     To repair: maw ws repair-oplog {ws_name}"
                );
            }
            // Otherwise silent — don't log on every subsequent merge.
        }
        Err(CheckpointAppendError::Other(err)) => {
            // Any other error — surface on every call (as before).
            eprintln!(
                "WARNING: checkpoint write skipped for workspace '{}': {err}",
                workspace_id.as_str()
            );
        }
    }

    Ok(new_head)
}

enum CheckpointAppendError {
    /// The op log chain contains a dangling blob reference. Non-fatal —
    /// merges still work, but checkpointing is disabled until repaired.
    DamagedChain(String),
    /// Any other checkpoint error (I/O, malformed data, etc.).
    Other(String),
}

fn maybe_checkpoint_after_append(
    root: &Path,
    workspace_id: &WorkspaceId,
    trigger_oid: &GitOid,
) -> Result<(), CheckpointAppendError> {
    let view =
        materialize_from_checkpoint(root, workspace_id, |oid| read_patch_set_blob(root, oid))
            .map_err(|e| classify_checkpoint_error(&e, e.to_string()))?;

    maybe_write_checkpoint(
        root,
        workspace_id,
        &view,
        trigger_oid,
        trigger_oid,
        DEFAULT_CHECKPOINT_INTERVAL,
    )
    .map_err(|e| CheckpointAppendError::Other(e.to_string()))?;

    Ok(())
}

/// Identify dangling-blob errors so we can downgrade the log spam.
const fn classify_checkpoint_error(
    err: &CheckpointError,
    rendered: String,
) -> CheckpointAppendError {
    if let CheckpointError::OpLogRead(OpLogReadError::CatFile { .. }) = err {
        return CheckpointAppendError::DamagedChain(rendered);
    }
    CheckpointAppendError::Other(rendered)
}

/// Repair a workspace's op log by archiving the current head ref and
/// resetting the chain. See `WorkspaceCommands::RepairOplog` for docs.
///
/// Note: this is in `oplog_runtime` because it's the same module that owns
/// the runtime warning for damaged chains — keeping the repair next to the
/// detection path makes them easy to keep in sync.
pub fn repair_oplog(name: &str, dry_run: bool) -> Result<()> {
    let root = super::repo_root()?;

    let ws_id = WorkspaceId::new(name)
        .map_err(|e| anyhow::anyhow!("invalid workspace name '{name}': {e}"))?;

    let head_ref = refs::workspace_head_ref(ws_id.as_str());
    let current_head =
        refs::read_ref(&root, &head_ref).with_context(|| format!("reading {head_ref}"))?;

    let Some(old_head_oid) = current_head else {
        println!(
            "Op log for workspace '{name}' is already reset \
             (no ref at {head_ref}). Nothing to repair."
        );
        return Ok(());
    };

    // Archive the old head under refs/manifold/archive/head/<name>/<ts>
    // so the user can recover the chain if needed.
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let archive_ref = format!("refs/manifold/archive/head/{name}/{ts}");

    if dry_run {
        println!("Would repair op log for workspace '{name}':");
        println!("  Current head: {}", old_head_oid.as_str());
        println!("  Action 1: archive to {archive_ref}");
        println!("  Action 2: delete   {head_ref}");
        println!("  Re-run without --dry-run to apply.");
        return Ok(());
    }

    // 1. Write archive ref pointing at the current head.
    refs::write_ref(&root, &archive_ref, &old_head_oid)
        .with_context(|| format!("writing archive ref {archive_ref}"))?;

    // 2. Delete the workspace head ref. The next op appended for this
    //    workspace will start a fresh chain.
    refs::delete_ref(&root, &head_ref).with_context(|| format!("deleting {head_ref}"))?;

    // 3. Clear the "already warned" set so if the chain gets corrupted
    //    again in this session, the user sees a fresh warning.
    if let Ok(mut guard) = DAMAGED_OPLOG_WARNED.lock()
        && let Some(set) = guard.as_mut()
    {
        set.remove(name);
    }

    println!("Op log for workspace '{name}' repaired.");
    println!("  Archived: {archive_ref} → {}", old_head_oid.as_str());
    println!("  Deleted:  {head_ref}");
    println!();
    println!("The next operation for this workspace will start a fresh chain.");
    println!("Worktree contents, git history, and recovery snapshots are unchanged.");

    if name == "default" {
        println!();
        println!("NOTE: You repaired the default workspace's op log. The next merge");
        println!("      into default will create a new Create op as the chain root.");
    }

    // Quick sanity check: verify that nothing else references the old blob.
    // This is informational only — doesn't affect the repair.
    if let Ok(output) = std::process::Command::new("git")
        .args(["for-each-ref", "--format=%(refname)", "refs/manifold/"])
        .current_dir(&root)
        .output()
        && output.status.success()
    {
        let refs_list = String::from_utf8_lossy(&output.stdout);
        let stale_count = refs_list
            .lines()
            .filter(|r| {
                if r.contains(&format!("refs/manifold/head/{name}"))
                    || r.contains(&format!("refs/manifold/archive/head/{name}"))
                {
                    return false;
                }
                if let Ok(Some(oid)) = refs::read_ref(&root, r.trim()) {
                    oid.as_str() == old_head_oid.as_str()
                } else {
                    false
                }
            })
            .count();
        if stale_count > 0 {
            println!();
            println!("  Note: {stale_count} other ref(s) also point at the old head blob.");
        }
    }

    Ok(())
}
