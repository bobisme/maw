//! `maw undo` — repo-level, oplog-powered undo of the last epoch mutation
//! (bn-117s).
//!
//! The single most confidence-instilling property a VCS-adjacent tool can have
//! (jj's oplog undo): every merge becomes reversible, so every bug becomes an
//! inconvenience instead of a data-loss scare.
//!
//! # What it undoes
//!
//! The most recent *epoch-mutating* operation. In scope: a `ws merge` that
//! advanced the epoch (the must-have). `maw undo` after `maw undo` is a **redo**
//! — the toggle is durable in the op log, so the pair round-trips.
//!
//! # Safety model
//!
//! * Every ref movement goes through the guarded native movers
//!   (`refs::advance_epoch` / `update_refs_atomic`, CAS-checked) — zero raw fs
//!   writes to refs/HEAD (the bn-8flz / bn-29z8 invariant).
//! * The undone merge result is pinned under `refs/manifold/recovery/undo/<ts>`
//!   before the epoch rewinds, so undo is itself undoable (that pin is what
//!   redo replays).
//! * Uncommitted trunk edits are captured before the rewind and re-applied
//!   byte-for-byte afterwards (the bn-1xmk preserve-and-replay contract).
//! * Refusals are self-contained: each says exactly why and what to do.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use maw_core::backend::WorkspaceBackend;
use maw_core::model::layout::LayoutFlavor;
use maw_core::model::types::{EpochId, GitOid, WorkspaceId};
use maw_core::oplog::read::{read_head, read_operation};
use maw_core::oplog::types::{OpPayload, Operation};
use maw_core::refs;

use crate::epoch_lock::EpochLock;
use crate::format::OutputFormat;
use crate::ops_log::{RepoOp, collect_repo_ops};
use crate::workspace::capture::{capture_before_destroy, recovery_ref};
use crate::workspace::destroy_record::{DestroyReason, read_latest_record, write_destroy_record};
use crate::workspace::oplog_runtime::append_operation_with_runtime_checkpoint;
use crate::workspace::{
    MawConfig, get_backend, now_timestamp_iso8601, now_timestamp_iso8601_precise, recover,
    repo_root,
};

/// Workspace that owns the durable repo-level compensation record. The default
/// workspace is never destroyed, so its op log is the stable spine.
const DEFAULT_WS: &str = "default";

/// Reason prefixes that mark a `Compensate` op as produced by `maw undo` /
/// `maw undo` (redo). The prefix is the machine-readable direction tag.
const UNDO_TAG: &str = "maw-undo";
const REDO_TAG: &str = "maw-redo";

/// Which way the next `maw undo` moves the epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    /// Reverse a merge: move the epoch/branch from `epoch_after` back to
    /// `epoch_before` and restore destroyed sources.
    Undo,
    /// Re-apply a previously-undone merge: move forward `epoch_before` →
    /// `epoch_after` and re-destroy the sources.
    Redo,
}

/// A fully-resolved plan for the next `maw undo`.
struct Plan {
    direction: Direction,
    /// OID of the merge operation blob being (un)done (any recorded copy).
    merge_op_id: GitOid,
    /// Epoch before the merge.
    epoch_before: EpochId,
    /// Epoch produced by the merge.
    epoch_after: EpochId,
    /// Source workspaces the merge consumed.
    sources: Vec<WorkspaceId>,
    /// Branch the merge advanced.
    branch: String,
}

impl Plan {
    /// The epoch we are moving *away from* (the current expected epoch).
    const fn moving_from(&self) -> &EpochId {
        match self.direction {
            Direction::Undo => &self.epoch_after,
            Direction::Redo => &self.epoch_before,
        }
    }

    /// The epoch we are moving *to*.
    const fn moving_to(&self) -> &EpochId {
        match self.direction {
            Direction::Undo => &self.epoch_before,
            Direction::Redo => &self.epoch_after,
        }
    }
}

/// Refusals gathered before mutating anything.
#[derive(Default)]
struct Refusals {
    /// Cannot proceed under any flag.
    hard: Vec<String>,
    /// Proceeds only with `--force`.
    force_required: Vec<String>,
}

/// Run `maw undo`.
///
/// # Errors
/// Returns an error if there is nothing to undo, a refusal rail fires, or a
/// ref/worktree mutation fails.
pub fn run(
    op_id: Option<&str>,
    dry_run: bool,
    force: bool,
    format: Option<OutputFormat>,
) -> Result<()> {
    let _format = OutputFormat::resolve(format);
    let root = repo_root()?;

    if dry_run {
        let ops = collect_repo_ops(&root)?;
        let plan = build_plan(&root, &ops, op_id)?;
        let refusals = gather_refusals(&root, &plan)?;
        print_dry_run(&plan, &refusals, force);
        return Ok(());
    }

    // Every real undo is an epoch mutation: hold the repo-level epoch lock for
    // the whole read-modify-write, exactly like `ws merge` / `ws advance`.
    let _lock = EpochLock::acquire(&root, "undo")?;

    let ops = collect_repo_ops(&root)?;
    let plan = build_plan(&root, &ops, op_id)?;
    let refusals = gather_refusals(&root, &plan)?;

    if !refusals.hard.is_empty() {
        bail!("{}", refusal_message(&plan, &refusals.hard, false));
    }
    if !refusals.force_required.is_empty() && !force {
        bail!("{}", refusal_message(&plan, &refusals.force_required, true));
    }

    execute(&root, &plan)
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

/// Resolve the operation to act on and the direction (undo vs redo).
fn build_plan(root: &Path, ops: &[RepoOp], op_id: Option<&str>) -> Result<Plan> {
    let branch = MawConfig::load(root)?.branch().to_owned();

    if let Some(id) = op_id {
        // Targeting a specific op: it must be a merge, and the refusal rails
        // enforce that it is still the current epoch tip.
        let matches: Vec<&RepoOp> = ops
            .iter()
            .filter(|o| o.id.as_str().starts_with(id))
            .collect();
        if matches.is_empty() {
            bail!(
                "No operation matches id '{id}'.\n  \
                 To fix: run `maw ops log` and copy an id from the list."
            );
        }
        if matches.len() > 1 {
            bail!(
                "Operation id '{id}' is ambiguous ({} matches).\n  \
                 To fix: pass more characters of the id from `maw ops log`.",
                matches.len()
            );
        }
        let op = matches[0];
        let OpPayload::Merge {
            sources,
            epoch_before,
            epoch_after,
        } = &op.payload
        else {
            bail!(
                "Operation {} is a '{}', not a merge — only merges can be undone.\n  \
                 To fix: run `maw undo` with no id to undo the last epoch mutation.",
                op.short_id(),
                op.kind()
            );
        };
        return Ok(Plan {
            direction: Direction::Undo,
            merge_op_id: op.id.clone(),
            epoch_before: epoch_before.clone(),
            epoch_after: epoch_after.clone(),
            sources: sources.clone(),
            branch,
        });
    }

    // No id: act on the most recent epoch-mutating op.
    let Some(op) = ops.iter().find(|o| is_epoch_op(&o.payload)) else {
        bail!(
            "Nothing to undo — no epoch mutations recorded.\n  \
             `maw undo` reverses the last `maw ws merge`; there have been none.\n  \
             To fix: run `maw ops log` to see the recorded operations."
        );
    };

    match &op.payload {
        OpPayload::Merge {
            sources,
            epoch_before,
            epoch_after,
        } => Ok(Plan {
            direction: Direction::Undo,
            merge_op_id: op.id.clone(),
            epoch_before: epoch_before.clone(),
            epoch_after: epoch_after.clone(),
            sources: sources.clone(),
            branch,
        }),
        OpPayload::Compensate { target_op, reason } => {
            // Read the merge this compensation referred to (its blob survives
            // even when unreferenced) and toggle the direction.
            let merge = read_operation(root, target_op).with_context(|| {
                format!(
                    "Failed to read the merge operation {} referenced by the last undo",
                    &target_op.as_str()[..target_op.as_str().len().min(12)]
                )
            })?;
            let OpPayload::Merge {
                sources,
                epoch_before,
                epoch_after,
            } = merge.payload
            else {
                bail!("The last compensation does not reference a merge; cannot undo/redo it.");
            };
            let direction = if reason.starts_with(UNDO_TAG) {
                Direction::Redo
            } else {
                // maw-redo (or any other tagged compensate) → undo again.
                Direction::Undo
            };
            Ok(Plan {
                direction,
                merge_op_id: target_op.clone(),
                epoch_before,
                epoch_after,
                sources,
                branch,
            })
        }
        _ => unreachable!("is_epoch_op only matches Merge / tagged Compensate"),
    }
}

/// Is this payload an epoch-level operation that `maw undo` toggles over?
fn is_epoch_op(payload: &OpPayload) -> bool {
    match payload {
        OpPayload::Merge { .. } => true,
        OpPayload::Compensate { reason, .. } => {
            reason.starts_with(UNDO_TAG) || reason.starts_with(REDO_TAG)
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Refusal rails
// ---------------------------------------------------------------------------

fn gather_refusals(root: &Path, plan: &Plan) -> Result<Refusals> {
    let mut r = Refusals::default();
    let expected = plan.moving_from();

    // Rail 1: the epoch must still be where the target op left it. If it has
    // advanced, this is NOT the last epoch mutation — out-of-maw work or a
    // newer merge would be orphaned.
    let current_epoch = refs::read_epoch_current(root)
        .context("Failed to read current epoch")?
        .map(|o| o.as_str().to_owned());
    match current_epoch.as_deref() {
        Some(cur) if cur == expected.as_str() => {}
        Some(cur) => r.hard.push(format!(
            "the epoch has advanced since this merge (now {}, expected {}).\n  \
             Undo only reverses the LAST epoch mutation. Something advanced the epoch \
             on top of it (a newer merge, `maw ws advance`, or `maw epoch sync` after a \
             direct commit).\n  \
             To fix: undo the newer operation first (`maw ops log`), or recover this \
             merge's result from `maw ws recover`.",
            short(cur),
            short(expected.as_str())
        )),
        None => r
            .hard
            .push("no epoch is set (repo not initialised?).".to_owned()),
    }

    // Rail 2: the branch tip must match too (direct commits would be orphaned).
    let branch_ref = format!("refs/heads/{}", plan.branch);
    let branch_tip = refs::read_ref(root, &branch_ref)
        .with_context(|| format!("Failed to read branch ref {branch_ref}"))?
        .map(|o| o.as_str().to_owned());
    if let Some(tip) = branch_tip.as_deref()
        && tip != expected.as_str()
    {
        r.hard.push(format!(
            "branch '{}' has diverged from this merge (tip {}, expected {}).\n  \
             Direct commits were made to the branch outside of maw; undoing would orphan them.\n  \
             To fix: inspect `git -C <repo> log {}` and reconcile before undoing.",
            plan.branch,
            short(tip),
            short(expected.as_str()),
            plan.branch,
        ));
    }

    // Rail 3: pushed-since — undoing rewinds local history below what the
    // remote already has. Overridable with --force.
    if let Some(remote) = remote_branch_tip(root, &plan.branch)
        && (remote == expected.as_str() || is_ancestor(root, expected.as_str(), &remote))
    {
        r.force_required.push(format!(
            "this merge was already pushed to origin/{} (remote is at {}).\n  \
             Undoing rewinds local history below the remote, so the two will diverge and \
             your next push will need --force.\n  \
             To proceed anyway: re-run with `maw undo --force`.",
            plan.branch,
            short(&remote),
        ));
    }

    // Rail 4 (undo only): a source the merge consumed must not have been
    // re-created since. For redo we are re-destroying, so a live source is
    // expected.
    if plan.direction == Direction::Undo {
        let backend = get_backend()?;
        for source in &plan.sources {
            let exists = backend.exists(source);
            let record = read_latest_record(root, source.as_str())?;
            let merge_destroyed = record
                .as_ref()
                .is_some_and(|rec| rec.destroy_reason == DestroyReason::MergeDestroy);
            if exists && merge_destroyed {
                r.hard.push(format!(
                    "source workspace '{}' was re-created after the merge; undo cannot safely \
                     restore it over live work.\n  \
                     To fix: rename or destroy the current '{}' (`maw ws destroy {}`), then re-run \
                     `maw undo`.",
                    source.as_str(),
                    source.as_str(),
                    source.as_str(),
                ));
            }
        }
    }

    Ok(r)
}

fn refusal_message(plan: &Plan, reasons: &[String], force_hint: bool) -> String {
    let verb = match plan.direction {
        Direction::Undo => "undo",
        Direction::Redo => "redo",
    };
    let mut msg = format!("Cannot {verb} merge {}:", plan.short_merge_id());
    for reason in reasons {
        msg.push_str("\n\n  - ");
        msg.push_str(reason);
    }
    if force_hint {
        msg.push_str("\n\n  (Add --force to override the checks above.)");
    }
    msg
}

impl Plan {
    fn short_merge_id(&self) -> &str {
        short(self.merge_op_id.as_str())
    }
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

fn execute(root: &Path, plan: &Plan) -> Result<()> {
    let flavor = LayoutFlavor::detect_with_env(root);
    let default_ws_path = flavor.default_target_path(root, DEFAULT_WS);

    let from = plan.moving_from().clone();
    let to = plan.moving_to().clone();
    let from_oid =
        GitOid::new(from.as_str()).map_err(|e| anyhow::anyhow!("invalid source epoch: {e}"))?;
    let to_oid =
        GitOid::new(to.as_str()).map_err(|e| anyhow::anyhow!("invalid target epoch: {e}"))?;

    // Step 1: capture uncommitted trunk edits BEFORE any ref/tree movement, so
    // they can be re-applied byte-for-byte after the rewind (bn-1xmk contract).
    let dirty = capture_trunk_dirty(&default_ws_path);

    // Step 2: pin the pre-undo trunk state (Prime Invariant: no work is lost).
    // Best-effort — a failure here must not block the undo, but we surface it.
    let pre_state_ref = match capture_before_destroy(&default_ws_path, DEFAULT_WS, &from_oid) {
        Ok(Some(result)) => Some(result.pinned_ref),
        Ok(None) => None,
        Err(e) => {
            eprintln!("  WARNING: could not pin pre-undo trunk snapshot: {e:#}");
            None
        }
    };

    // Step 3: pin the merge result (epoch_after) under refs/manifold/recovery/
    // undo/<ts> so it stays reachable after the epoch rewinds — this is what a
    // subsequent `maw undo` (redo) replays.
    let ea_oid = GitOid::new(plan.epoch_after.as_str())
        .map_err(|e| anyhow::anyhow!("invalid epoch_after: {e}"))?;
    let undo_pin = recovery_ref("undo", &now_timestamp_iso8601_precise());
    if let Err(e) = refs::write_ref(root, &undo_pin, &ea_oid) {
        bail!("Failed to pin the merge result before undoing (aborted, nothing changed): {e}");
    }

    // Step 4: move epoch + branch together, guarded (CAS old==from). The
    // pre-checks proved both are at `from`, so this transaction commits.
    let branch_ref = format!("refs/heads/{}", plan.branch);
    refs::update_refs_atomic(
        root,
        &[
            (refs::EPOCH_CURRENT, &from_oid, &to_oid),
            (branch_ref.as_str(), &from_oid, &to_oid),
        ],
    )
    .with_context(|| {
        format!(
            "Failed to move epoch + branch {} → {} (guarded update). \
             The merge result is still pinned at {undo_pin}.",
            short(from.as_str()),
            short(to.as_str()),
        )
    })?;

    // Step 5: rebuild the default worktree at the target epoch, then re-apply
    // the captured trunk edits so they survive byte-for-byte.
    reset_trunk_worktree(&default_ws_path, to.as_str(), &dirty)?;

    // Keep the default workspace's own epoch ref consistent with the rewind.
    let default_id = WorkspaceId::new(DEFAULT_WS).expect("default is a valid ws id");
    let epoch_ws_ref = refs::workspace_epoch_ref(DEFAULT_WS);
    if let Err(e) = refs::write_ref(root, &epoch_ws_ref, &to_oid) {
        eprintln!("  WARNING: failed to update {epoch_ws_ref}: {e}");
    }

    // Step 6: restore (undo) or re-destroy (redo) the source workspaces.
    let mut source_notes = Vec::new();
    match plan.direction {
        Direction::Undo => restore_sources(root, plan, &mut source_notes)?,
        Direction::Redo => redestroy_sources(root, plan, &to_oid, &mut source_notes)?,
    }

    // Step 7: record the compensation in the durable default op log.
    let comp_id = record_compensation(root, &default_id, plan)?;

    print_success(
        plan,
        &from,
        &to,
        &undo_pin,
        pre_state_ref.as_deref(),
        &source_notes,
        &comp_id,
    );
    Ok(())
}

/// Restore the merge's destroyed source workspaces to their original names.
fn restore_sources(root: &Path, plan: &Plan, notes: &mut Vec<String>) -> Result<()> {
    let backend = get_backend()?;
    for source in &plan.sources {
        if backend.exists(source) {
            // Merge did not destroy it (a `--no-destroy` merge) — leave it.
            notes.push(format!(
                "  {} — already present (not restored)",
                source.as_str()
            ));
            continue;
        }
        match read_latest_record(root, source.as_str())? {
            Some(_record) => {
                // Reuse the tested recovery machinery: recreate the workspace
                // at the (now rewound) epoch and populate it from its
                // destroy-time snapshot. `restore_to` refuses to overwrite a
                // live workspace, which we already excluded above.
                match recover::restore_to(source.as_str(), source.as_str()) {
                    Ok(()) => notes.push(format!("  {} — restored", source.as_str())),
                    Err(e) => {
                        notes.push(format!(
                            "  {} — RESTORE FAILED: {e:#}\n      \
                             recover manually: maw ws recover {}",
                            source.as_str(),
                            source.as_str()
                        ));
                    }
                }
            }
            None => notes.push(format!(
                "  {} — no snapshot on record; cannot restore (was it merged with \
                 --no-destroy and already gc'd?)",
                source.as_str()
            )),
        }
    }
    Ok(())
}

/// Re-destroy the source workspaces that a prior undo restored (redo path).
fn redestroy_sources(
    root: &Path,
    plan: &Plan,
    epoch_after: &GitOid,
    notes: &mut Vec<String>,
) -> Result<()> {
    let backend = get_backend()?;
    let base_epoch = plan.epoch_before.clone();
    for source in &plan.sources {
        if !backend.exists(source) {
            notes.push(format!("  {} — already absent", source.as_str()));
            continue;
        }
        let ws_path = backend.workspace_path(source);
        // Capture + record a fresh destroy record so recovery stays possible,
        // mirroring merge cleanup (but without re-taking the epoch lock, which
        // we already hold).
        let capture = capture_before_destroy(&ws_path, source.as_str(), epoch_after)
            .ok()
            .flatten();
        let final_head = crate::workspace::capture::resolve_head(&ws_path)
            .unwrap_or_else(|_| epoch_after.clone());
        if let Err(e) = write_destroy_record(
            root,
            source.as_str(),
            &base_epoch,
            &final_head,
            capture.as_ref(),
            DestroyReason::MergeDestroy,
        ) {
            tracing::warn!("redo: failed to write destroy record for '{source}': {e}");
        }
        match backend.destroy(source) {
            Ok(()) => notes.push(format!("  {} — re-destroyed", source.as_str())),
            Err(e) => notes.push(format!("  {} — RE-DESTROY FAILED: {e}", source.as_str())),
        }
    }
    Ok(())
}

/// Record the undo/redo as a `Compensate` op in the default workspace op log.
fn record_compensation(root: &Path, default_id: &WorkspaceId, plan: &Plan) -> Result<GitOid> {
    let head = ensure_default_oplog_head(root, default_id, &plan.epoch_before)?;
    let (tag, detail) = match plan.direction {
        Direction::Undo => (
            UNDO_TAG,
            format!(
                "restored epoch {} (undid merge to {})",
                short(plan.epoch_before.as_str()),
                short(plan.epoch_after.as_str())
            ),
        ),
        Direction::Redo => (
            REDO_TAG,
            format!(
                "reapplied merge to epoch {}",
                short(plan.epoch_after.as_str())
            ),
        ),
    };
    let op = Operation {
        parent_ids: vec![head.clone()],
        workspace_id: default_id.clone(),
        timestamp: now_timestamp_iso8601(),
        payload: OpPayload::Compensate {
            target_op: plan.merge_op_id.clone(),
            reason: format!("{tag} merge {}: {detail}", plan.short_merge_id()),
        },
    };
    append_operation_with_runtime_checkpoint(root, default_id, &op, Some(&head))
        .context("Failed to record the undo compensation op")
}

fn ensure_default_oplog_head(
    root: &Path,
    default_id: &WorkspaceId,
    epoch: &EpochId,
) -> Result<GitOid> {
    if let Some(head) =
        read_head(root, default_id).map_err(|e| anyhow::anyhow!("read default op-log head: {e}"))?
    {
        return Ok(head);
    }
    let create = Operation {
        parent_ids: vec![],
        workspace_id: default_id.clone(),
        timestamp: now_timestamp_iso8601(),
        payload: OpPayload::Create {
            epoch: epoch.clone(),
        },
    };
    append_operation_with_runtime_checkpoint(root, default_id, &create, None)
        .context("Failed to bootstrap the default op log")
}

// ---------------------------------------------------------------------------
// Trunk worktree reset with dirty preservation
// ---------------------------------------------------------------------------

/// Capture uncommitted trunk edits (HEAD → worktree), excluding admin trees.
/// `None` bytes means the path was deleted in the worktree.
fn capture_trunk_dirty(ws_path: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    use maw_git::GitRepo;
    let Ok(repo) = maw_git::GixRepo::open(ws_path) else {
        return Vec::new();
    };
    let Ok(entries) = repo.status_head_to_worktree() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries {
        let rel = PathBuf::from(&entry.path);
        if rel
            .components()
            .next()
            .and_then(|c| c.as_os_str().to_str())
            .is_some_and(|first| matches!(first, ".maw" | "repo.git" | ".manifold" | ".git"))
        {
            continue;
        }
        let full = ws_path.join(&rel);
        let bytes = if full.is_file() {
            std::fs::read(&full).ok()
        } else {
            None
        };
        out.push((rel, bytes));
    }
    out
}

/// Reset the default worktree to `target_epoch` (index + tracked files), then
/// re-apply the captured dirty edits so uncommitted trunk work survives.
fn reset_trunk_worktree(
    ws_path: &Path,
    target_epoch: &str,
    dirty: &[(PathBuf, Option<Vec<u8>>)],
) -> Result<()> {
    // `git restore --source=<epoch> --staged --worktree -- .` is the same
    // primitive `maw ws undo` uses to rewind tracked files; there is no direct
    // gix equivalent that also resets the index (TODO(gix)).
    let output = Command::new("git")
        .args([
            "restore",
            "--source",
            target_epoch,
            "--staged",
            "--worktree",
            "--",
            ".",
        ])
        .current_dir(ws_path)
        .output()
        .context("Failed to run git restore to rewind the trunk")?;
    if !output.status.success() {
        bail!(
            "Trunk rewind failed:\n  {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    // Re-apply the user's uncommitted edits on top of the rewound tree.
    for (rel, bytes) in dirty {
        let full = ws_path.join(rel);
        match bytes {
            Some(content) => {
                if let Some(parent) = full.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&full, content)
                    .with_context(|| format!("re-applying trunk edit {}", rel.display()))?;
            }
            None => {
                let _ = std::fs::remove_file(&full);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn short(oid: &str) -> &str {
    &oid[..oid.len().min(12)]
}

/// Read `refs/remotes/origin/<branch>` as a full OID string, if it exists.
fn remote_branch_tip(root: &Path, branch: &str) -> Option<String> {
    let ref_name = format!("refs/remotes/origin/{branch}");
    refs::read_ref(root, &ref_name)
        .ok()
        .flatten()
        .map(|o| o.as_str().to_owned())
}

/// True if `ancestor` is an ancestor of (or equal to) `descendant`.
fn is_ancestor(root: &Path, ancestor: &str, descendant: &str) -> bool {
    if ancestor == descendant {
        return true;
    }
    Command::new("git")
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .current_dir(root)
        .status()
        .is_ok_and(|s| s.success())
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

fn print_dry_run(plan: &Plan, refusals: &Refusals, force: bool) {
    let verb = match plan.direction {
        Direction::Undo => "Undo",
        Direction::Redo => "Redo",
    };
    println!("{verb} plan for merge {}:", plan.short_merge_id());
    println!(
        "  epoch  {} → {}",
        short(plan.moving_from().as_str()),
        short(plan.moving_to().as_str())
    );
    println!(
        "  branch '{}' moves to {}",
        plan.branch,
        short(plan.moving_to().as_str())
    );
    let srcs = plan
        .sources
        .iter()
        .map(WorkspaceId::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    match plan.direction {
        Direction::Undo => println!("  restore sources: [{srcs}]"),
        Direction::Redo => println!("  re-destroy sources: [{srcs}]"),
    }
    println!("  merge result stays reachable via refs/manifold/recovery/undo/<ts>");
    println!();

    if refusals.hard.is_empty() && (refusals.force_required.is_empty() || force) {
        println!("Would proceed. Re-run without --dry-run to apply.");
    } else {
        println!("Would REFUSE:");
        for reason in &refusals.hard {
            println!("  - {reason}");
        }
        for reason in &refusals.force_required {
            if force {
                println!("  - (overridden by --force) {reason}");
            } else {
                println!("  - {reason}");
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn print_success(
    plan: &Plan,
    from: &EpochId,
    to: &EpochId,
    undo_pin: &str,
    pre_state_ref: Option<&str>,
    source_notes: &[String],
    comp_id: &GitOid,
) {
    let verb = match plan.direction {
        Direction::Undo => "Undid",
        Direction::Redo => "Redid",
    };
    println!(
        "{verb} merge {} — epoch {} → {}, branch '{}' updated.",
        plan.short_merge_id(),
        short(from.as_str()),
        short(to.as_str()),
        plan.branch,
    );
    if !source_notes.is_empty() {
        println!("Source workspaces:");
        for note in source_notes {
            println!("{note}");
        }
    }
    println!("Merge result kept at: {undo_pin}  (recover: maw ws recover --ref {undo_pin})");
    if let Some(pre) = pre_state_ref {
        println!("Pre-undo trunk pinned: {pre}");
    }
    println!("Recorded compensation op: {}", short(comp_id.as_str()));
    match plan.direction {
        Direction::Undo => println!("Redo this: maw undo   (it toggles)"),
        Direction::Redo => println!("Undo again: maw undo   (it toggles)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_op_classification() {
        assert!(is_epoch_op(&OpPayload::Merge {
            sources: vec![],
            epoch_before: EpochId::new(&"a".repeat(40)).unwrap(),
            epoch_after: EpochId::new(&"b".repeat(40)).unwrap(),
        }));
        assert!(is_epoch_op(&OpPayload::Compensate {
            target_op: GitOid::new(&"c".repeat(40)).unwrap(),
            reason: "maw-undo merge abc: ...".to_owned(),
        }));
        assert!(is_epoch_op(&OpPayload::Compensate {
            target_op: GitOid::new(&"c".repeat(40)).unwrap(),
            reason: "maw-redo merge abc: ...".to_owned(),
        }));
        // A plain workspace-local compensation (from `maw ws undo`) is NOT an
        // epoch op and must be ignored by `maw undo`.
        assert!(!is_epoch_op(&OpPayload::Compensate {
            target_op: GitOid::new(&"c".repeat(40)).unwrap(),
            reason: "undo: reverted 2 path(s) to base epoch".to_owned(),
        }));
        assert!(!is_epoch_op(&OpPayload::Destroy));
    }

    fn plan(direction: Direction) -> Plan {
        Plan {
            direction,
            merge_op_id: GitOid::new(&"d".repeat(40)).unwrap(),
            epoch_before: EpochId::new(&"a".repeat(40)).unwrap(),
            epoch_after: EpochId::new(&"b".repeat(40)).unwrap(),
            sources: vec![],
            branch: "main".to_owned(),
        }
    }

    #[test]
    fn direction_selects_from_and_to_epochs() {
        let undo = plan(Direction::Undo);
        assert_eq!(undo.moving_from().as_str(), "b".repeat(40));
        assert_eq!(undo.moving_to().as_str(), "a".repeat(40));
        let redo = plan(Direction::Redo);
        assert_eq!(redo.moving_from().as_str(), "a".repeat(40));
        assert_eq!(redo.moving_to().as_str(), "b".repeat(40));
    }

    #[test]
    fn refusal_message_lists_reasons_and_force_hint() {
        let p = plan(Direction::Undo);
        let msg = refusal_message(&p, &["reason one".to_owned()], true);
        assert!(msg.contains("Cannot undo merge"));
        assert!(msg.contains("reason one"));
        assert!(msg.contains("--force"));
    }
}
