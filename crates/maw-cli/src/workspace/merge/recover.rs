//! Crash recovery for an unfinished `maw ws merge` journal (bn-1fcox).
//!
//! A merge killed after its COMMIT-phase ref CAS leaves `merge-state.json`
//! in phase `commit` / `cleanup` with the refs already at the merged commit.
//! Before bn-1fcox nothing in production ran the `CheckCommit` /
//! `RetryCleanup` recovery for it: `--abort` refused (committed work), and
//! FF-absorb, `doctor --repair`, `epoch sync`, `undo` and `merge promote` all
//! refuse while such a journal exists, so one crash wedged the repo until
//! someone deleted the file by hand (skipping the merge's CLEANUP: the
//! target worktree was never checked out at the merged commit and
//! `--destroy` sources were never destroyed).
//!
//! Every entry point here runs under the repo epoch lock, which a live
//! `ws merge` holds for its whole run, so a `commit` / `cleanup` journal seen
//! under the lock is provably orphaned. The decision is the pure
//! [`maw_core::merge_state::decide_journal_recovery`] over the live refs
//! ([`maw_core::merge_state::classify_cas_landing`]) — the same functions the
//! Stateright model (`maw-assurance`, `P_RECOVERY_CONVERGES`,
//! `P_TARGET_CHECKED_OUT`) checks:
//!
//! * CAS landed → converge FORWARD: finish CLEANUP exactly as the merge
//!   would have (merge op records, sibling auto-rebase, target checkout,
//!   `--destroy`), then clear the journal. Refs are never re-applied or
//!   rolled back. The target checkout replays dirty target edits exactly as
//!   the live merge does (the replay depends only on the anchor, the merged
//!   commit and the snapshot — bn-28s78). Configured `post_merge` hooks are
//!   NOT run: they are listed with instructions instead (see
//!   [`Finalized::skipped_post_merge_hooks`]).
//! * CAS provably did not land → abort: clear the journal (sources were
//!   never destroyed — destroy runs only after the CAS).
//! * Neither provable → refuse, keep the journal, and say exactly what the
//!   refs look like.

use std::path::Path;

use anyhow::{Result, bail};
use maw_core::merge_state::{
    CasLanding, CasObservation, JournalRecovery, MergePhase, MergeStateError, MergeStateFile,
    classify_cas_landing, decide_journal_recovery,
};
use maw_core::model::types::{EpochId, GitOid, WorkspaceId};
use maw_core::oplog::read::read_operation;
use maw_core::oplog::types::OpPayload;

use super::{
    get_backend, handle_post_merge_destroy, is_ancestor_commit, now_secs, record_merge_operations,
    update_default_workspace,
};
use crate::format::OutputFormat;
use crate::workspace::MawConfig;

/// Who asked for recovery (only changes wording and which phases are
/// handled).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// `maw ws merge --recover`.
    Recover,
    /// `maw ws merge --abort`: same decision; a merge whose commit already
    /// landed cannot be aborted, so it is finalized instead.
    Abort,
    /// The start of the next `maw ws merge` (under its epoch lock). Handles
    /// only `commit` / `cleanup` journals — pre-COMMIT journals stay with
    /// PREPARE's orphan takeover (a live `--plan` may own one; it takes no
    /// epoch lock).
    MergeStart,
}

/// What recovery did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// No journal.
    Nothing,
    /// A journal this trigger does not handle (pre-COMMIT at merge start).
    NotHandled,
    /// Terminal / pre-COMMIT journal cleared (no ref was touched).
    Cleared { phase: MergePhase },
    /// COMMIT journal whose CAS never landed: aborted.
    AbortedNotLanded,
    /// The CAS landed: CLEANUP finished and the journal cleared.
    Finalized(Finalized),
}

/// Details of a forward-converged recovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finalized {
    pub phase: MergePhase,
    pub sources: Vec<String>,
    pub branch: String,
    pub target_workspace: String,
    pub merged_commit: String,
    pub target_checked_out: bool,
    pub destroyed: Vec<String>,
    /// `--destroy` sources kept because their HEAD moved since the merge
    /// froze it (new work) or their destroy failed.
    pub kept: Vec<String>,
    /// Configured `[hooks] post_merge` commands that recovery deliberately
    /// did NOT run (bn-28s78). The live merge runs them as its very last
    /// step, after clearing the journal, so a journal on disk proves they
    /// never ran for this merge. Recovery can happen much later, from a
    /// different command (the next merge's start) and without the merge's
    /// context, so running side-effecting commands then is not safe to do
    /// silently; they are listed for the user to run instead.
    pub skipped_post_merge_hooks: Vec<String>,
    pub notes: Vec<String>,
}

fn journal_path(root: &Path) -> std::path::PathBuf {
    MergeStateFile::default_path(
        &maw_core::model::layout::LayoutFlavor::detect_with_env(root).manifold_dir(root),
    )
}

/// Remove the merge journal and the COMMIT-phase sidecars (the same
/// implementation the live merge's CLEANUP ends with).
fn clear_journal(root: &Path) -> Result<()> {
    let manifold = maw_core::model::layout::LayoutFlavor::detect_with_env(root).manifold_dir(root);
    maw_core::merge_state::clear_merge_journal(&manifold)
        .map_err(|e| anyhow::anyhow!("failed to clear the merge journal: {e}"))
}

fn short(oid: &str) -> &str {
    &oid[..oid.len().min(12)]
}

/// Observed ref state for a journal.
struct Observation {
    branch: String,
    updates_epoch: bool,
    candidate: GitOid,
    epoch: Option<GitOid>,
    branch_head: Option<GitOid>,
    landing: CasLanding,
}

fn observe(root: &Path, state: &MergeStateFile, config: &MawConfig) -> Result<Option<Observation>> {
    let Some(candidate) = state
        .epoch_candidate
        .clone()
        .or_else(|| state.epoch_after.as_ref().map(|e| e.oid().clone()))
    else {
        return Ok(None);
    };
    let branch = state
        .target_branch
        .clone()
        .unwrap_or_else(|| config.branch().to_owned());
    let updates_epoch = state
        .updates_epoch
        .unwrap_or_else(|| branch == config.branch());
    let branch_ref = format!("refs/heads/{branch}");
    let epoch_before = state.epoch_before.oid().clone();

    let mut epoch = maw_core::refs::read_epoch_current(root)?;
    let mut branch_head = maw_core::refs::read_ref(root, &branch_ref)?;

    // Legacy split shape (epoch moved, branch did not): finish the branch
    // move exactly as the COMMIT phase's own recovery does. Unreachable with
    // the atomic 2-ref CAS, kept for journals from older binaries.
    if updates_epoch
        && epoch.as_ref() == Some(&candidate)
        && branch_head.as_ref() == Some(&epoch_before)
    {
        maw::merge::commit::recover_partial_commit_with_branch_base(
            root,
            &branch,
            &epoch_before,
            &epoch_before,
            &candidate,
        )
        .map_err(|e| anyhow::anyhow!("finalizing the branch ref failed: {e}"))?;
        epoch = maw_core::refs::read_epoch_current(root)?;
        branch_head = maw_core::refs::read_ref(root, &branch_ref)?;
    }

    let candidate_in_branch = match &branch_head {
        Some(head) if head == &candidate => true,
        Some(head) => is_ancestor_commit(root, candidate.as_str(), head.as_str())?,
        None => false,
    };
    let landing = classify_cas_landing(CasObservation {
        updates_epoch,
        epoch_at_candidate: epoch.as_ref() == Some(&candidate),
        epoch_at_before: epoch.as_ref() == Some(&epoch_before),
        candidate_in_branch,
    });
    Ok(Some(Observation {
        branch,
        updates_epoch,
        candidate,
        epoch,
        branch_head,
        landing,
    }))
}

/// Run the recovery decision for the journal, if any. Caller holds the epoch
/// lock.
///
/// # Errors
/// On I/O failure, or when the ref state proves neither outcome (the journal
/// is kept and the error says what the refs look like).
pub fn run_locked(root: &Path, trigger: Trigger, text_mode: bool) -> Result<Outcome> {
    let path = journal_path(root);
    let state = match MergeStateFile::read(&path) {
        Ok(s) => s,
        Err(MergeStateError::NotFound(_)) => return Ok(Outcome::Nothing),
        Err(e) => bail!(
            "cannot read the merge journal {}: {e}\n  \
             It cannot drive a recovery. Inspect it; if no `maw ws merge` is running, \
             move it aside and re-run your command.",
            path.display()
        ),
    };
    let post_cas_phase = matches!(state.phase, MergePhase::Commit | MergePhase::Cleanup);
    if trigger == Trigger::MergeStart && !post_cas_phase {
        return Ok(Outcome::NotHandled);
    }

    let config = MawConfig::load(root)?;
    let obs = if post_cas_phase {
        observe(root, &state, &config)?
    } else {
        None
    };
    let landing = obs.as_ref().map_or(CasLanding::Unknown, |o| o.landing);

    match decide_journal_recovery(&state.phase, landing) {
        JournalRecovery::ClearTerminal { phase } | JournalRecovery::ClearPreCommit { phase } => {
            clear_journal(root)?;
            Ok(Outcome::Cleared { phase })
        }
        JournalRecovery::AbortNotLanded => {
            clear_journal(root)?;
            Ok(Outcome::AbortedNotLanded)
        }
        JournalRecovery::FinalizeCommitted { phase } => {
            let obs = obs.expect("a landed CAS was observed");
            finalize(root, &state, phase, &obs, &config, text_mode).map(Outcome::Finalized)
        }
        JournalRecovery::Refuse { phase, reason } => {
            let (epoch, branch_head, candidate, branch) = obs.as_ref().map_or_else(
                || {
                    (
                        "?".to_owned(),
                        "?".to_owned(),
                        "(none recorded)".to_owned(),
                        "?".to_owned(),
                    )
                },
                |o| {
                    (
                        o.epoch
                            .as_ref()
                            .map_or_else(|| "(missing)".to_owned(), |e| e.as_str().to_owned()),
                        o.branch_head
                            .as_ref()
                            .map_or_else(|| "(missing)".to_owned(), |e| e.as_str().to_owned()),
                        o.candidate.as_str().to_owned(),
                        o.branch.clone(),
                    )
                },
            );
            bail!(
                "Cannot recover the interrupted merge (phase: {phase}): {reason}.\n  \
                 Merge sources: {}\n  \
                 Pre-merge epoch/base: {}\n  \
                 Merged commit:        {candidate}\n  \
                 Epoch now:            {epoch}\n  \
                 Branch '{branch}' now: {branch_head}\n  \
                 Nothing was changed. Something other than this merge moved the refs, so \
                 recovery cannot prove whether its commit landed.\n  \
                 Inspect: git log --oneline --graph {candidate} {branch_head}\n  \
                 The merged commit stays readable by its OID. Once you have decided, remove \
                 {} to unblock merges.",
                state
                    .sources
                    .iter()
                    .map(WorkspaceId::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
                state.epoch_before.as_str(),
                path.display()
            )
        }
    }
}

/// Where the target's op log stands relative to this merge's records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetOps {
    /// The Merge op for this merge is not recorded yet.
    Unrecorded,
    /// The Merge op is the head: the post-checkout Snapshot op (if any) is
    /// not recorded yet.
    AtHead,
    /// The Merge op is recorded and followed by the target Snapshot op.
    SnapshotFollows,
}

/// Classify the target's op-log head against this merge.
///
/// bn-1losw: the live merge appends a Snapshot op after the target's Merge op
/// when the target was dirty. Looking only at the head op missed the Merge op
/// behind that Snapshot, so recovering a crash that happened after the
/// checkout recorded the merge a SECOND time.
fn target_ops(root: &Path, target: &WorkspaceId, merged: &GitOid) -> TargetOps {
    let is_this_merge = |oid: &GitOid| {
        matches!(
            read_operation(root, oid).map(|op| op.payload),
            Ok(OpPayload::Merge { epoch_after, .. }) if epoch_after.oid() == merged
        )
    };
    let Ok(Some(head)) = maw_core::oplog::read::read_head(root, target) else {
        return TargetOps::Unrecorded;
    };
    if is_this_merge(&head) {
        return TargetOps::AtHead;
    }
    match read_operation(root, &head) {
        Ok(op) if matches!(op.payload, OpPayload::Snapshot { .. }) => {
            if op.parent_ids.first().is_some_and(is_this_merge) {
                TargetOps::SnapshotFollows
            } else {
                TargetOps::Unrecorded
            }
        }
        _ => TargetOps::Unrecorded,
    }
}

/// Finish CLEANUP for a merge whose CAS landed, then clear the journal.
#[expect(
    clippy::too_many_lines,
    reason = "mirrors the merge's post-CAS sequence step by step"
)]
fn finalize(
    root: &Path,
    state: &MergeStateFile,
    phase: MergePhase,
    obs: &Observation,
    config: &MawConfig,
    text_mode: bool,
) -> Result<Finalized> {
    let path = journal_path(root);
    let candidate = &obs.candidate;
    let sources: Vec<String> = state
        .sources
        .iter()
        .map(|s| s.as_str().to_owned())
        .collect();
    let target_ws = state
        .target_workspace
        .clone()
        .unwrap_or_else(|| config.default_workspace().to_owned());
    let mut notes = Vec::new();

    // Enter CLEANUP first so a crash during recovery resumes as RetryCleanup.
    if state.phase == MergePhase::Commit {
        let mut advanced = state.clone();
        advanced
            .advance(MergePhase::Cleanup, now_secs())
            .map_err(|e| anyhow::anyhow!("advance merge-state: {e}"))?;
        advanced
            .write_atomic(&path)
            .map_err(|e| anyhow::anyhow!("write merge-state: {e}"))?;
    }

    // Merge op records (the merge writes them right after the CAS).
    let epoch_before = EpochId::new(state.epoch_before.as_str())
        .map_err(|e| anyhow::anyhow!("invalid epoch_before in journal: {e}"))?;
    let target_id = WorkspaceId::new(&target_ws).ok();
    if let Some(target_id) = &target_id {
        if target_ops(root, target_id, candidate) == TargetOps::Unrecorded {
            for warning in record_merge_operations(
                root,
                &state.sources,
                Some(target_id),
                &epoch_before,
                candidate,
            ) {
                tracing::warn!("{warning}");
            }
        } else {
            tracing::debug!("merge op already recorded; skipping");
        }
    }

    // Sibling auto-rebase (idempotent: siblings already on the merged
    // commit report up to date; sources are skipped as in-progress).
    let backend = get_backend()?;
    if obs.updates_epoch {
        // bn-15ebo: recovery must not refuse on an invalid config, but it
        // must not silently apply default policy either. Skip the sibling
        // rebase (a later `maw ws sync` catches siblings up) and say so.
        let auto_rebase = match crate::workspace::load_manifold_config(root) {
            Ok(cfg) => cfg.merge.auto_rebase_siblings,
            Err(e) => {
                eprintln!(
                    "  WARNING: invalid maw config ({e:#}); skipping sibling auto-rebase. \
                     Fix the config, then run: maw ws sync --all"
                );
                false
            }
        };
        if auto_rebase {
            let reports = crate::workspace::sync::auto_rebase::auto_rebase_siblings(
                root,
                &backend,
                &target_ws,
                &sources,
                candidate.as_str(),
            );
            for report in reports.iter().filter(|r| {
                !matches!(
                    r.result,
                    crate::workspace::sync::auto_rebase::SiblingResult::SkippedInProgress
                )
            }) {
                notes.push(format!(
                    "sibling {} — {}",
                    report.name,
                    report.result.describe(&report.name)
                ));
            }
        }
    }

    // Target checkout (update_default_workspace). Idempotent anchor: once the
    // target's per-workspace epoch ref already names the merged commit, the
    // checkout ran; anchor there so only real user edits are replayed.
    let flavor = maw_core::model::layout::LayoutFlavor::detect_with_env(root);
    let target_path = flavor.default_target_path(root, &target_ws);
    let target_checked_out = if target_path.exists() {
        let ws_epoch =
            maw_core::refs::read_ref(root, &maw_core::refs::workspace_epoch_ref(&target_ws))
                .ok()
                .flatten();
        let checkout_done = ws_epoch.as_ref() == Some(candidate);
        let anchor_before = if checkout_done {
            candidate.as_str().to_owned()
        } else {
            state.epoch_before.as_str().to_owned()
        };
        // bn-1losw: the live merge records the target's pre-checkout dirty
        // state as a Snapshot op right after its Merge op. Do the same while
        // the checkout has not run (the tree still holds the pre-merge
        // state) and nothing follows the Merge op yet. Recorded BEFORE the
        // checkout so a crash inside it cannot lead a second recovery to
        // snapshot the already-merged tree.
        //
        // bn-15fzo: a pending checkout intent means an interrupted run already
        // snapshotted (and possibly checked out) the tree: it no longer holds
        // the pre-merge state, so a patch set of it would record the merge's
        // own changes as user edits.
        if !checkout_done
            && !super::checkout_intent::pending_for(root, &target_ws, candidate.as_str())?
            && let Some(target_id) = &target_id
            && target_ops(root, target_id, candidate) == TargetOps::AtHead
            && let Ok(patch_set) =
                maw_core::model::diff::compute_patchset(&target_path, &epoch_before)
            && !patch_set.is_empty()
        {
            super::record_target_snapshot_op(root, target_id, &epoch_before, &patch_set);
        }
        // `workspace_base_before` is None here, as in the live merge for
        // the consolidated layout (target = repo root). Only the legacy v2
        // layout (target under ws/) passes the backend's pre-merge base
        // epoch live; `update_default_workspace` then falls back to the
        // target's per-workspace epoch ref, which is the same value unless
        // that ref is missing or lagging behind a HEAD at epoch_before
        // (backend self-heal). Post-CAS, the backend can no longer
        // reproduce the pre-merge answer, so this is left as is (bn-1losw).
        update_default_workspace(
            &target_path,
            &target_ws,
            &obs.branch,
            &anchor_before,
            candidate.as_str(),
            None,
            root,
            obs.updates_epoch,
            text_mode,
            &sources,
        )?;
        true
    } else {
        notes.push(format!(
            "target workspace '{target_ws}' not found at {} — no checkout to finish",
            target_path.display()
        ));
        false
    };

    // `--destroy`: only sources still exactly at the HEAD the merge froze
    // (their committed work is in the merged commit). A source whose HEAD
    // moved since has new work — keep it.
    let mut destroyed = Vec::new();
    let mut kept = Vec::new();
    if state.destroy_after == Some(true) {
        let mut eligible = Vec::new();
        for ws in &state.sources {
            let ws_path = flavor.workspace_path(root, ws.as_str());
            if !ws_path.exists() {
                continue; // already destroyed before the crash
            }
            let head = super::resolve_workspace_head_oid(&ws_path).ok();
            let frozen = state.frozen_heads.get(ws).map(GitOid::as_str);
            if head.is_some() && head.as_deref() == frozen {
                eligible.push(ws.as_str().to_owned());
            } else {
                kept.push(ws.as_str().to_owned());
            }
        }
        if !eligible.is_empty() {
            let outcome = handle_post_merge_destroy(
                &eligible, &target_ws, false, &backend, root, text_mode, false,
            )?;
            for ws in &eligible {
                if !outcome.destroyed.contains(ws) {
                    kept.push(ws.clone());
                }
            }
            destroyed = outcome.destroyed;
        }
    }

    clear_journal(root)?;

    Ok(Finalized {
        phase,
        sources,
        branch: obs.branch.clone(),
        target_workspace: target_ws,
        merged_commit: candidate.as_str().to_owned(),
        target_checked_out,
        destroyed,
        kept,
        skipped_post_merge_hooks: config.hooks.post_merge.clone(),
        notes,
    })
}

/// Human-readable report of a finalize (stderr at merge start, stdout for
/// the explicit commands).
fn finalized_lines(f: &Finalized, trigger: Trigger) -> Vec<String> {
    let mut lines = vec![format!(
        "Recovered an interrupted merge (phase: {}): its commit had already landed.",
        f.phase
    )];
    if trigger == Trigger::Abort {
        lines.push(
            "  It cannot be aborted without discarding committed work, so it was finished \
             instead."
                .to_owned(),
        );
    }
    lines.push(format!(
        "  Merged: {} -> {} @ {}",
        f.sources.join(", "),
        f.branch,
        short(&f.merged_commit)
    ));
    if f.target_checked_out {
        lines.push(format!(
            "  Checked out '{}' at the merged commit.",
            f.target_workspace
        ));
    }
    if !f.destroyed.is_empty() {
        lines.push(format!(
            "  Destroyed (--destroy): {}",
            f.destroyed.join(", ")
        ));
    }
    for ws in &f.kept {
        lines.push(format!(
            "  Kept '{ws}': it changed since the merge froze it (or its destroy failed). \
             Review, then: maw ws destroy {ws}"
        ));
    }
    for note in &f.notes {
        lines.push(format!("  {note}"));
    }
    if !f.skipped_post_merge_hooks.is_empty() {
        lines.push(
            "  Post-merge hooks were NOT run: the interrupted merge never reached them, and \
             recovery does not replay side effects late. To run them now, from the repo root:"
                .to_owned(),
        );
        for cmd in &f.skipped_post_merge_hooks {
            lines.push(format!("    {cmd}"));
        }
    }
    lines.push("  Merge journal cleared. To revert the merge: maw undo".to_owned());
    lines
}

/// Recovery at the start of `maw ws merge` (caller holds the epoch lock).
///
/// Finishes an orphaned post-CAS journal (or aborts one whose CAS never
/// landed) so the new merge's FF-absorb and PREPARE are not blocked by it.
/// Notes go to stderr so `--format json` stdout stays one document.
///
/// # Errors
/// When the journal's ref state cannot be proven either way.
pub fn before_merge(root: &Path) -> Result<()> {
    match run_locked(root, Trigger::MergeStart, false)? {
        Outcome::Nothing | Outcome::NotHandled | Outcome::Cleared { .. } => {}
        Outcome::AbortedNotLanded => eprintln!(
            "NOTE: cleared the journal of an interrupted merge whose commit never landed \
             (refs unchanged, sources untouched)."
        ),
        Outcome::Finalized(f) => {
            for line in finalized_lines(&f, Trigger::MergeStart) {
                eprintln!("{line}");
            }
            eprintln!();
        }
    }
    Ok(())
}

/// `maw ws merge --recover` / `--abort`.
///
/// # Errors
/// On lock timeout, I/O failure, or an unprovable ref state.
pub fn explicit(root: &Path, fmt: OutputFormat, trigger: Trigger) -> Result<()> {
    let label = if trigger == Trigger::Abort {
        "ws merge --abort"
    } else {
        "ws merge --recover"
    };
    let _epoch_lock = crate::epoch_lock::EpochLock::acquire(root, label)?;
    let json = fmt == OutputFormat::Json;
    let text_mode = !json;
    let outcome = run_locked(root, trigger, text_mode)?;
    if json {
        let value = match &outcome {
            Outcome::Nothing | Outcome::NotHandled => serde_json::json!({
                "recovered": false,
                "aborted": false,
                "reason": "no merge-state file",
            }),
            Outcome::Cleared { phase } => serde_json::json!({
                "recovered": true,
                "action": "cleared",
                "aborted": true,
                "phase": phase.to_string(),
            }),
            Outcome::AbortedNotLanded => serde_json::json!({
                "recovered": true,
                "action": "aborted_not_landed",
                "aborted": true,
                "phase": "commit",
            }),
            Outcome::Finalized(f) => serde_json::json!({
                "recovered": true,
                "action": "finalized",
                "aborted": false,
                "phase": f.phase.to_string(),
                "sources": f.sources,
                "branch": f.branch,
                "merged_commit": f.merged_commit,
                "target_workspace": f.target_workspace,
                "target_checked_out": f.target_checked_out,
                "destroyed": f.destroyed,
                "kept": f.kept,
                "post_merge_hooks_skipped": f.skipped_post_merge_hooks,
            }),
        };
        println!("{value}");
        return Ok(());
    }
    match outcome {
        Outcome::Nothing | Outcome::NotHandled => println!(
            "No merge in progress \u{2014} nothing to {}.\n  \
             Next: maw ws merge <workspaces> --into <target> --message \"...\"",
            if trigger == Trigger::Abort {
                "abort"
            } else {
                "recover"
            }
        ),
        Outcome::Cleared { phase } => println!(
            "Cleared the journal of an interrupted merge (was in phase: {phase}).\n  \
             It had not reached COMMIT, so no ref moved and no work was lost.\n  \
             Next: re-run your merge \u{2014} maw ws merge <workspaces> --into <target> \
             --message \"...\""
        ),
        Outcome::AbortedNotLanded => println!(
            "Aborted an interrupted merge (was in phase: commit): its commit never landed.\n  \
             The epoch and branch refs are unchanged and its sources were not destroyed.\n  \
             Next: re-run your merge \u{2014} maw ws merge <workspaces> --into <target> \
             --message \"...\""
        ),
        Outcome::Finalized(f) => {
            for line in finalized_lines(&f, trigger) {
                println!("{line}");
            }
        }
    }
    Ok(())
}
