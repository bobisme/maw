pub(crate) mod auto_rebase;
pub(crate) mod checks; // pub(crate): advance.rs uses committed_ahead_of_epoch (bn-8flz)
mod cross_target;
mod lock;
pub(crate) mod notice;
pub(crate) mod rebase; // pub(crate): advance.rs uses rebase_workspace (bn-8flz)
pub(crate) mod sanity;

use std::path::Path;

use anyhow::Result;
use tracing::instrument;

use maw_core::backend::WorkspaceBackend;
use maw_core::model::types::WorkspaceId;
use maw_core::refs as manifold_refs;

use super::{MawConfig, get_backend, repo_root};

use checks::{
    SyncOutcome, committed_ahead_of_epoch, is_default_workspace, sync_worktree_to_epoch,
    workspace_has_uncommitted_changes, workspace_name_from_cwd,
};
use cross_target::cross_target_sync_risk;
use rebase::rebase_workspace;

pub use rebase::{
    RebaseConflict, RebaseConflicts, RebaseOutcome, delete_rebase_conflicts, read_rebase_conflicts,
};

// bn-8flz: rebase_workspace and committed_ahead_of_epoch are now pub(crate)
// (promoted from pub(super)) so advance.rs can access them via
// super::sync::rebase::rebase_workspace and super::sync::checks::committed_ahead_of_epoch
// respectively, without needing re-exports here.
// See crates/maw-cli/src/workspace/advance.rs.

/// Verify recorded conflict metadata against reality via the shared
/// effective-conflict-state helper and print [`STALE_CLEAR_NOTICE`] when a
/// stale sidecar (manual resolution committed) was cleared. Verification
/// failures are non-fatal — sync proceeds and just logs a warning.
///
/// Returns the effective conflict state so the caller can surface residual
/// conflicts without re-computing it.
///
/// [`STALE_CLEAR_NOTICE`]: super::conflict_state::STALE_CLEAR_NOTICE
fn report_cleared_stale_sidecar(
    root: &Path,
    ws_name: &str,
    ws_path: &Path,
) -> Option<super::conflict_state::EffectiveConflictState> {
    match super::conflict_state::effective_conflict_state(root, ws_name, ws_path) {
        Ok(state) => {
            if state.cleared_stale_sidecar {
                println!("{}", super::conflict_state::STALE_CLEAR_NOTICE);
            }
            Some(state)
        }
        Err(e) => {
            tracing::warn!(
                workspace = %ws_name,
                error = %e,
                "sync: could not verify effective conflict state"
            );
            None
        }
    }
}

#[instrument]
/// # Errors
///
/// Returns an error if workspace synchronization fails.
#[expect(
    clippy::too_many_lines,
    reason = "sync handles multiple staleness/conflict paths; factoring them out would obscure the control flow"
)]
pub fn sync(name: Option<&str>, all: bool, no_rebase: bool) -> Result<()> {
    if all {
        return sync_all(no_rebase);
    }

    let root = repo_root()?;
    let backend = get_backend()?;

    // Get the current epoch
    let current_epoch = manifold_refs::read_epoch_current(&root)
        .map_err(|e| anyhow::anyhow!("Failed to read current epoch: {e}"))?;

    let Some(current_epoch) = current_epoch else {
        println!("No epoch ref set. Run `maw init` first.");
        return Ok(());
    };

    let workspace_name = name.map_or_else(
        || {
            let cwd = std::env::current_dir().unwrap_or_else(|_| root.clone());
            workspace_name_from_cwd(&root, &cwd)
        },
        ToString::to_string,
    );
    let ws_id = WorkspaceId::new(&workspace_name).map_err(|e| anyhow::anyhow!("{e}"))?;

    if is_default_workspace(&workspace_name) {
        print_default_sync_skip(&root, &workspace_name);
        return Ok(());
    }

    if !backend.exists(&ws_id) {
        println!("Workspace '{workspace_name}' not found.");
        return Ok(());
    }

    // bn-1abp: a user-initiated sync supersedes any pending auto-rebase
    // notice — print it now (still informative) and consume it so it can't
    // pop up, stale, after this sync re-points the workspace.
    notice::print_notice_if_any(&root, &workspace_name);

    let ws_status = backend.status(&ws_id).map_err(|e| anyhow::anyhow!("{e}"))?;
    let ws_path = maw_core::model::layout::LayoutFlavor::detect_with_env(&root)
        .workspace_path(&root, &workspace_name);

    // bn-8zqz: verify recorded conflict metadata against reality REGARDLESS
    // of staleness — a manual resolution commit on a stale workspace must
    // still clear its stale sidecar (the old `!is_stale` guard blocked
    // legitimate clearing). Uses the same shared helper as the merge gate,
    // `ws conflicts`, and `resolve --list`, so all surfaces agree.
    //
    // bn-6xpz: reuse the returned state on the no-op path to surface any
    // residual committed conflicts instead of silently claiming "up to date".
    let conflict_state = report_cleared_stale_sidecar(&root, &workspace_name, &ws_path);

    if !ws_status.is_stale {
        // bn-6xpz: if the workspace is current but carries unresolved
        // committed conflicts (e.g. a quiet sibling auto-rebase just recorded
        // a conflict into it), report them so the caller isn't misled.
        if let Some(ref state) = conflict_state
            && state.is_conflicted()
        {
            let n = state.conflict_count();
            println!(
                "Workspace '{workspace_name}' is up to date, but has {n} unresolved conflict(s):"
            );
            for path in state.unresolved_paths() {
                println!("  - {}", path.display());
            }
            println!("  Resolve: maw ws resolve {workspace_name} --list");
            return Ok(());
        }
        println!("Workspace '{workspace_name}' is up to date.");
        return Ok(());
    }

    // Safety: don't sync over committed work. If the workspace has commits not
    // yet in epoch (diverged after a concurrent merge), default behavior is to
    // rebase those commits onto the new epoch. With --no-rebase, refuse rather
    // than discard committed work — the destructive path is not silent.
    // NOTE: We compare against the workspace's *original* base epoch, not the
    // current epoch. The workspace HEAD is based on the old epoch, so comparing
    // against the new epoch would report 0 commits ahead (HEAD is behind it),
    // causing us to skip the rebase and fast-forward — silently dropping commits.
    match committed_ahead_of_epoch(&ws_path, &ws_status.base_epoch) {
        None => {
            // Could not determine commit count — refuse to sync to prevent data loss.
            println!(
                "WARNING: Could not determine committed work for '{workspace_name}' \
                 (git failed). Refusing to sync to avoid data loss."
            );
            println!("  Check workspace state manually, then retry.");
            return Ok(());
        }
        Some(ahead) if ahead > 0 => {
            if no_rebase {
                anyhow::bail!(
                    "Workspace '{workspace_name}' has {ahead} committed commit(s) ahead of epoch; \
                     --no-rebase would discard committed work.\n  \
                     Run `maw ws sync {workspace_name}` (default rebases) to replay them onto the \
                     new epoch, or destroy and recreate the workspace if you really want to drop \
                     these commits."
                );
            }
            return rebase_workspace(
                &root,
                &workspace_name,
                ws_status.base_epoch.as_str(),
                current_epoch.as_str(),
                &ws_path,
                ahead,
                "sync",
            );
        }
        Some(_) => {}
    }

    if let Some(active_change) = cross_target_sync_risk(
        &root,
        &workspace_name,
        ws_status.base_epoch.as_str(),
        current_epoch.as_str(),
    )? {
        println!(
            "Workspace '{workspace_name}' is behind current epoch, but that epoch tracks active change '{}' ({}) not yet on trunk.",
            active_change.change_id, active_change.change_branch
        );
        println!(
            "  Refusing to sync this unbound workspace to avoid pulling change-only commits into a trunk-targeted flow."
        );
        println!(
            "  To continue change work, create/use a change-bound workspace: maw ws create --change {} <name>",
            active_change.change_id
        );
        println!(
            "  To continue trunk-only work, keep this workspace on its current base and merge with --into default."
        );
        return Ok(());
    }

    println!("Workspace '{workspace_name}' is stale (behind current epoch), syncing...");
    println!();

    // In the git worktree model, "syncing" means updating the worktree's
    // HEAD to point to the current epoch via detached checkout.
    // No CAS guard needed here: `maw ws sync` is user-initiated and the
    // ancestor-refusal pre-flight in sync_worktree_to_epoch_inner is the
    // relevant safety check for this path.
    sync_worktree_to_epoch(&root, &workspace_name, current_epoch.as_str(), None)?;

    println!();
    println!("Workspace synced successfully.");

    Ok(())
}

/// Sync all workspaces at once
#[expect(
    clippy::too_many_lines,
    reason = "sync-all command aggregates per-workspace outcomes for reporting"
)]
fn sync_all(no_rebase: bool) -> Result<()> {
    let root = repo_root()?;
    let backend = get_backend()?;

    let current_epoch = manifold_refs::read_epoch_current(&root)
        .map_err(|e| anyhow::anyhow!("Failed to read current epoch: {e}"))?;

    let Some(current_epoch) = current_epoch else {
        println!("No epoch ref set. Run `maw init` first.");
        return Ok(());
    };

    let workspaces = backend.list().map_err(|e| anyhow::anyhow!("{e}"))?;

    if workspaces.is_empty() {
        println!("No workspaces found.");
        return Ok(());
    }

    let stale_count = workspaces
        .iter()
        .filter(|ws| ws.state.is_stale() && !is_default_workspace(ws.id.as_str()))
        .count();

    if stale_count == 0 {
        println!("All {} workspace(s) are up to date.", workspaces.len());
        return Ok(());
    }

    println!(
        "Syncing {} stale workspace(s) of {} total...",
        stale_count,
        workspaces.len()
    );
    println!();

    let mut synced = 0;
    let mut rebased = 0;
    let mut skipped_with_work: Vec<String> = Vec::new();
    let mut skipped_cross_target: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for ws in &workspaces {
        if !ws.state.is_stale() || is_default_workspace(ws.id.as_str()) {
            continue;
        }

        let name = ws.id.as_str();

        // Per-workspace decision: rebase committed work by default; with
        // --no-rebase, skip workspaces with committed work (no destructive
        // reset). If git fails (None), treat as "has work" to prevent data
        // loss. Compare against the workspace's base epoch (not current
        // epoch) — see committed_ahead_of_epoch docs.
        let ws_path = maw_core::model::layout::LayoutFlavor::detect_with_env(&root)
            .workspace_path(&root, name);
        let ws_status = backend.status(&ws.id).map_err(|e| anyhow::anyhow!("{e}"))?;
        let ahead_count = match committed_ahead_of_epoch(&ws_path, &ws_status.base_epoch) {
            None => {
                skipped_with_work.push(format!(
                    "{name} (could not determine commit count \u{2014} skipped for safety)"
                ));
                continue;
            }
            Some(ahead) if ahead > 0 => {
                if no_rebase {
                    skipped_with_work
                        .push(format!("{name} ({ahead} commit(s) ahead; --no-rebase)"));
                    continue;
                }
                Some(ahead)
            }
            Some(_) => None,
        };

        if let Some(active_change) = cross_target_sync_risk(
            &root,
            name,
            ws_status.base_epoch.as_str(),
            current_epoch.as_str(),
        )? {
            skipped_cross_target.push(format!(
                "{name} (epoch tracks active change '{}' / {})",
                active_change.change_id, active_change.change_branch
            ));
            continue;
        }

        if let Some(ahead) = ahead_count {
            match rebase_workspace(
                &root,
                name,
                ws_status.base_epoch.as_str(),
                current_epoch.as_str(),
                &ws_path,
                ahead,
                "sync-all",
            ) {
                Ok(()) => rebased += 1,
                Err(e) => errors.push(format!("{name}: {e}")),
            }
        } else {
            match sync_worktree_to_epoch(&root, name, current_epoch.as_str(), None) {
                Ok(_) => synced += 1,
                Err(e) => errors.push(format!("{name}: {e}")),
            }
        }
    }

    if !skipped_with_work.is_empty() {
        println!();
        let header = if no_rebase {
            "Skipped (committed work not yet merged; --no-rebase prevents replay):"
        } else {
            "Skipped (committed work not yet merged \u{2014} merge first):"
        };
        println!("{header}");
        for s in &skipped_with_work {
            println!("  - {s}");
        }
    }

    if !skipped_cross_target.is_empty() {
        println!();
        println!("Skipped (cross-target safety; active change epoch not yet on trunk):");
        for s in &skipped_cross_target {
            println!("  - {s}");
        }
    }

    let skipped_total = skipped_with_work.len() + skipped_cross_target.len();

    println!();
    println!(
        "Results: {} synced, {} rebased, {} already current, {} skipped, {} errors",
        synced,
        rebased,
        workspaces.len() - stale_count,
        skipped_total,
        errors.len()
    );

    if skipped_total > 0 {
        println!("Result: INCOMPLETE (safety skips detected; see skipped sections above).");
    }

    if !errors.is_empty() {
        println!();
        println!("Errors:");
        for err in &errors {
            println!("  - {err}");
        }
        anyhow::bail!(
            "sync --all failed for {} workspace(s); resolve listed errors and retry",
            errors.len()
        );
    }

    if skipped_total > 0 {
        anyhow::bail!(
            "sync --all incomplete: {skipped_total} workspace(s) were skipped by safety checks; merge or resolve them, then rerun maw ws sync --all"
        );
    }

    Ok(())
}

/// The default workspace tracks the configured branch and never does a
/// detached-epoch sync — explain why instead of silently no-oping.
fn print_default_sync_skip(root: &Path, workspace_name: &str) {
    let branch =
        MawConfig::load(root).map_or_else(|_| "main".to_string(), |cfg| cfg.branch().to_string());
    println!("Workspace '{workspace_name}' is the default branch workspace (tracks '{branch}').");
    println!("Skipping detached-epoch sync for default workspace.");
}

/// bn-1abp: the stale-epoch warning is useful exactly once per maw
/// invocation. Agents reported it printing repeatedly — including while
/// running the exact `git add` / `git commit` remediation it recommends —
/// which turns a useful signal into noise. This process-local latch
/// guarantees AT MOST ONE stale-workspace warning block per maw process,
/// no matter how many code paths consult [`auto_sync_if_stale`].
static STALE_WARNING_EMITTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Returns `true` exactly once per process; afterwards always `false`.
fn claim_stale_warning_slot() -> bool {
    !STALE_WARNING_EMITTED.swap(true, std::sync::atomic::Ordering::Relaxed)
}

/// Auto-sync a stale workspace before running a command.
/// In the git worktree model, this updates the worktree HEAD to the current epoch.
/// Returns `Ok(())` whether or not it was stale (idempotent).
///
/// The stale-workspace WARNING blocks are emitted at most once per maw
/// process (bn-1abp) — repeated calls stay silent but still skip the sync.
///
/// # Concurrency (bn-29z8 Defect B)
///
/// This function runs as a pre-hook for every `maw exec <ws> -- git ...`
/// invocation. Worker agents batch parallel tool calls, so multiple `git add` /
/// `git commit` commands can be in-flight concurrently. The TOCTOU hazard:
///
/// - `is_stale` → true
/// - `committed_ahead_of_epoch` → 0  (captures HEAD = A)
/// - concurrent `git commit` lands; HEAD moves to B
/// - dirty check → clean  (HEAD = B, index = B)
/// - `sync_worktree_to_epoch` → checkout epoch  (B orphaned)
///
/// Fix: acquire the workspace rebase lock (try-lock only — never block the
/// user's command) BEFORE the ahead-check, capture HEAD under the lock, and
/// pass it as `expected_head_hex` to `sync_worktree_to_epoch`. The CAS check
/// inside that function re-reads HEAD immediately before the checkout and aborts
/// the sync if it moved, preserving the concurrent commit.
///
/// If the lock is already held (another maw process is rebasing this workspace),
/// skip the auto-sync — running the command against a stale workspace is safe.
///
/// Defect C (design): exec auto-sync continues to MUTATE (move HEAD). It
/// already prints what it does ("auto-syncing..."). The alternative — demoting
/// it to a notice — would break the common agent pattern of committing in a
/// stale workspace, so we keep mutation but make it race-safe with the lock+CAS.
///
/// # Errors
///
/// Returns an error if stale workspace synchronization fails.
#[expect(
    clippy::too_many_lines,
    reason = "bn-29z8: the lock+CAS guard adds necessary sequential steps; splitting would obscure the invariant"
)]
pub fn auto_sync_if_stale(name: &str, _path: &Path) -> Result<()> {
    if is_default_workspace(name) {
        return Ok(());
    }

    let root = repo_root()?;
    let backend = get_backend()?;

    let Ok(ws_id) = WorkspaceId::new(name) else {
        return Ok(()); // Invalid name, skip
    };

    if !backend.exists(&ws_id) {
        return Ok(());
    }

    let ws_status = backend.status(&ws_id).map_err(|e| anyhow::anyhow!("{e}"))?;

    if !ws_status.is_stale {
        return Ok(());
    }

    let current_epoch = manifold_refs::read_epoch_current(&root)
        .map_err(|e| anyhow::anyhow!("Failed to read current epoch: {e}"))?;

    let Some(current_epoch) = current_epoch else {
        return Ok(());
    };

    // bn-29z8 Defect B: acquire the per-workspace rebase lock BEFORE reading
    // HEAD and performing the ahead-check. Try-lock only — if another process
    // holds the lock, skip the auto-sync rather than blocking the user's command.
    let ws_path =
        maw_core::model::layout::LayoutFlavor::detect_with_env(&root).workspace_path(&root, name);

    let lock = match lock::WorkspaceRebaseLock::try_acquire(&root, name) {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            // Another maw process is rebasing this workspace. Skip auto-sync
            // — the command runs against the current (possibly stale) HEAD.
            // This is safe: the rebase will complete and the next invocation
            // will see the updated state.
            eprintln!(
                "note: auto-sync for workspace '{name}' skipped (workspace lock held by another process)"
            );
            return Ok(());
        }
        Err(e) => {
            // Lock infrastructure failure — skip auto-sync rather than fail
            // the user's command.
            tracing::warn!(
                workspace = %name,
                error = %e,
                "auto_sync_if_stale: failed to acquire workspace lock; skipping auto-sync"
            );
            return Ok(());
        }
    };

    // Under the lock: perform all checks and capture HEAD atomically.
    // Safety: never auto-sync over committed work. When epoch advances laterally
    // (another workspace merged while this one has commits), the workspace is
    // stale AND has diverged commits. Syncing would wipe those commits.
    // The lead agent must merge this workspace first.
    // NOTE: Compare against base epoch, not current — see bn-18dj.
    match committed_ahead_of_epoch(&ws_path, &ws_status.base_epoch) {
        None => {
            drop(lock);
            if claim_stale_warning_slot() {
                eprintln!(
                    "WARNING: Workspace '{name}' is behind the current epoch (another merge advanced repository state), \
                     but git could not determine commit count. Skipping auto-sync to preserve committed work."
                );
                eprintln!(
                    "  The lead agent should merge this workspace: maw ws merge {name} --into default"
                );
            }
            return Ok(());
        }
        Some(ahead) if ahead > 0 => {
            drop(lock);
            if claim_stale_warning_slot() {
                eprintln!(
                    "WARNING: Workspace '{name}' is behind the current epoch (another merge advanced repository state since \
                     this one was created), and has {ahead} committed commit(s) not yet merged."
                );
                eprintln!("  Skipping auto-sync to preserve committed work.");
                eprintln!(
                    "  The lead agent should merge or rebase this workspace: maw ws merge {name} --into default  or  maw ws sync {name}"
                );
            }
            return Ok(());
        }
        Some(_) => {}
    }

    if let Some(active_change) = cross_target_sync_risk(
        &root,
        name,
        ws_status.base_epoch.as_str(),
        current_epoch.as_str(),
    )? {
        drop(lock);
        if claim_stale_warning_slot() {
            eprintln!(
                "WARNING: Workspace '{name}' is behind current epoch, but epoch tracks active change '{}' ({}) not yet on trunk.",
                active_change.change_id, active_change.change_branch
            );
            eprintln!(
                "  Skipping auto-sync for this unbound workspace to avoid pulling change-only commits into trunk-targeted work."
            );
            eprintln!(
                "  Use a change-bound workspace instead: maw ws create --change {} <name>",
                active_change.change_id
            );
            eprintln!(
                "  If this workspace should stay trunk-only, continue without syncing and merge with --into default."
            );
        }
        return Ok(());
    }

    // Safety: don't auto-sync over uncommitted changes — warn and let the
    // command run against the stale workspace instead of blocking it entirely.
    let is_dirty = workspace_has_uncommitted_changes(&ws_path).unwrap_or(false);
    if is_dirty {
        drop(lock);
        if claim_stale_warning_slot() {
            eprintln!(
                "WARNING: Workspace '{name}' is behind the current epoch, but has uncommitted changes. \
                 Skipping auto-sync to preserve uncommitted work."
            );
            eprintln!("  Commit or stash changes, then run: maw ws sync {name}");
        }
        return Ok(());
    }

    // bn-29z8 Defect B (CAS): capture HEAD OID NOW, under the lock, AFTER all
    // the skip checks passed. This is the "expected" value for the CAS in
    // sync_worktree_to_epoch_inner. If a concurrent git commit lands between
    // here and the checkout (i.e. after we release the lock), the CAS detects
    // the move and skips the sync.
    //
    // Note: the lock guards against concurrent MAW processes; we cannot hold
    // the lock across the git subprocess (that would deadlock sibling auto-
    // rebase). The CAS provides the final safety net for the small window
    // between lock release and the actual checkout syscall.
    let expected_head_hex: Option<String> = {
        use maw_git::GitRepo as _;
        maw_git::GixRepo::open(&ws_path)
            .ok()
            .and_then(|repo| repo.rev_parse_opt("HEAD").ok().flatten())
            .map(|oid| format!("{oid}"))
    };

    // Release the lock before the checkout. The CAS guard in
    // sync_worktree_to_epoch_inner provides the remaining safety.
    drop(lock);

    eprintln!(
        "Workspace '{name}' is behind the current epoch \u{2014} auto-syncing before running command..."
    );

    let outcome = sync_worktree_to_epoch(
        &root,
        name,
        current_epoch.as_str(),
        expected_head_hex.as_deref(),
    )?;

    match outcome {
        SyncOutcome::Synced => {
            eprintln!("Workspace '{name}' synced. Proceeding with command.");
            eprintln!();
        }
        SyncOutcome::SkippedHeadMoved => {
            // Already printed a "note: skipped" line inside sync_worktree_to_epoch.
            // The command proceeds against the now-current HEAD.
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    /// bn-1abp: the stale-warning latch fires exactly once per process.
    /// (No other unit test in this binary touches the latch; the
    /// integration-level guarantee — one warning per `maw` invocation —
    /// is covered in tests/sync.rs.)
    #[test]
    fn stale_warning_slot_claims_exactly_once() {
        assert!(
            super::claim_stale_warning_slot(),
            "first claim should win the slot"
        );
        assert!(
            !super::claim_stale_warning_slot(),
            "second claim must lose the slot"
        );
        assert!(!super::claim_stale_warning_slot());
    }
}
