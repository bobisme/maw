//! The versioned invariant catalog for `maw fsck` (bn-1uot).
//!
//! Each invariant is declared once here. `maw fsck` runs every entry;
//! `maw doctor` runs the subset whose [`Invariant::in_doctor`] is true. Adding
//! a cross-artifact invariant is a matter of adding one struct here and one
//! line to [`catalog`].

use std::collections::HashSet;

use anyhow::Result;
use maw_git::GitRepo as _;

use maw_core::merge_state::{DEFAULT_STALE_AFTER_SECS, MergeStateError, MergeStateFile, Staleness};
use maw_core::model::types::{GitOid as CoreOid, WorkspaceId};
use maw_core::oplog::read::{OpLogReadError, walk_chain};
use maw_core::oplog::types::OpPayload;
use maw_core::refs;

use super::{Ctx, Invariant, Severity, Violation};
use crate::workspace::{destroy_record, recover};

/// Build the full ordered invariant catalog.
///
/// Order is the render order in `maw fsck` output: refs, then workspaces,
/// then destroy/recovery coherence, then oplog, merge-state, locks, and
/// epoch/branch.
#[must_use]
pub fn catalog() -> Vec<Box<dyn Invariant>> {
    vec![
        // refs
        Box::new(RefsManifoldObject),
        Box::new(EpochCurrentResolvable),
        // workspaces
        Box::new(WorkspaceHeadValid),
        Box::new(StaleHeadRefs),
        Box::new(WorktreeBookkeeping),
        Box::new(GhostWorkingCopy),
        // destroy / recovery coherence (bn-3uou)
        Box::new(DanglingSnapshots),
        Box::new(AbandonedWithSnapshot),
        Box::new(DestroyRecordUnpinned),
        Box::new(DestroyLatestPointer),
        // oplog
        Box::new(OplogIntegrity),
        // merge-state (bn-2wyh)
        Box::new(MergeStateInvariant),
        // locks
        Box::new(StaleLocks),
        // epoch / branch
        Box::new(EpochBranchAncestor),
    ]
}

// ---------------------------------------------------------------------------
// Shared object-existence helper
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Kind {
    Commit,
    Blob,
    Any,
}

/// Whether the object `oid` is present in the odb with (at least) the given
/// kind. Hits the object database directly rather than trusting ref
/// resolution.
fn object_present(repo: &maw_git::GixRepo, oid_str: &str, kind: Kind) -> bool {
    let Ok(oid) = oid_str.parse::<maw_git::GitOid>() else {
        return false;
    };
    match kind {
        Kind::Commit => repo.read_commit(oid).is_ok(),
        Kind::Blob => repo.read_blob(oid).is_ok(),
        Kind::Any => {
            repo.read_commit(oid).is_ok()
                || repo.read_blob(oid).is_ok()
                || repo.read_tree(oid).is_ok()
        }
    }
}

/// Active (on-disk) agent workspace names, i.e. directories under the
/// workspaces dir. Does not include the default workspace (which lives at the
/// repo root in the consolidated layout).
fn active_workspace_names(ctx: &Ctx) -> HashSet<String> {
    let mut names = HashSet::new();
    if let Ok(entries) = std::fs::read_dir(ctx.flavor.workspaces_dir(&ctx.root)) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                names.insert(entry.file_name().to_string_lossy().to_string());
            }
        }
    }
    names
}

/// Whether a workspace name currently has a live worktree (an agent workspace
/// dir, or the default workspace which is the root checkout).
fn workspace_is_live(ctx: &Ctx, name: &str) -> bool {
    if name == ctx.default_workspace {
        return true;
    }
    ctx.flavor.workspace_path(&ctx.root, name).exists()
}

// ---------------------------------------------------------------------------
// refs: every refs/manifold/* points at an existing object of the right kind
// ---------------------------------------------------------------------------

struct RefsManifoldObject;

impl RefsManifoldObject {
    fn kind_for(ref_name: &str) -> Kind {
        if ref_name.starts_with(refs::HEAD_PREFIX) {
            // Oplog head refs point at operation *blobs*.
            Kind::Blob
        } else if ref_name == refs::EPOCH_CURRENT
            || ref_name.starts_with(refs::WORKSPACE_STATE_PREFIX)
            || ref_name.starts_with(refs::WORKSPACE_EPOCH_PREFIX)
            || ref_name.starts_with("refs/manifold/recovery/")
        {
            Kind::Commit
        } else {
            Kind::Any
        }
    }
}

impl Invariant for RefsManifoldObject {
    fn id(&self) -> &'static str {
        "refs-manifold-object"
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn description(&self) -> &'static str {
        "every refs/manifold/* ref resolves to an existing object of the right kind"
    }
    fn check(&self, ctx: &Ctx) -> Result<Vec<Violation>> {
        let repo = ctx.open_repo()?;
        let refs = match repo.list_refs("refs/manifold/") {
            Ok(r) => r,
            Err(e) => {
                return Ok(vec![Violation::new(
                    format!(
                        "could not enumerate refs/manifold/* ({e}) — the ref store may be corrupt"
                    ),
                    Some("Inspect: git for-each-ref refs/manifold/".to_string()),
                )]);
            }
        };
        let mut violations = Vec::new();
        for (name, oid) in refs {
            let name = name.as_str().to_string();
            let kind = Self::kind_for(&name);
            if !object_present(&repo, &oid.to_string(), kind) {
                violations.push(Violation::new(
                    format!("{name} → {oid} (object missing from the object store)"),
                    Some(format!(
                        "Inspect: git cat-file -t {oid}. Recover the object or delete the ref \
                         deliberately (fsck will not delete a ref that may pin content)."
                    )),
                ));
            }
        }
        Ok(violations)
    }
}

// ---------------------------------------------------------------------------
// refs: epoch/current parses and its commit exists
// ---------------------------------------------------------------------------

struct EpochCurrentResolvable;

impl Invariant for EpochCurrentResolvable {
    fn id(&self) -> &'static str {
        "epoch-current-resolvable"
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn description(&self) -> &'static str {
        "refs/manifold/epoch/current is set and its commit exists"
    }
    fn check(&self, ctx: &Ctx) -> Result<Vec<Violation>> {
        let epoch = refs::read_epoch_current(&ctx.root)
            .map_err(|e| anyhow::anyhow!("read epoch/current: {e}"))?;
        let Some(epoch) = epoch else {
            // Unset epoch is diagnosed by `check_manifold_initialized` / init;
            // not this invariant's corruption concern.
            return Ok(vec![]);
        };
        let repo = ctx.open_repo()?;
        if object_present(&repo, epoch.as_str(), Kind::Commit) {
            Ok(vec![])
        } else {
            Ok(vec![Violation::new(
                format!(
                    "epoch/current points at {} but that commit is missing from the object store",
                    epoch.as_str()
                ),
                Some(
                    "Investigate: git log --oneline --all; reset the epoch deliberately."
                        .to_string(),
                ),
            )])
        }
    }
}

// ---------------------------------------------------------------------------
// workspaces: each live workspace's head ref (if any) resolves to a commit
// ---------------------------------------------------------------------------

struct WorkspaceHeadValid;

impl Invariant for WorkspaceHeadValid {
    fn id(&self) -> &'static str {
        "workspace-head-valid"
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn description(&self) -> &'static str {
        "each live workspace's state ref resolves to a valid commit"
    }
    fn check(&self, ctx: &Ctx) -> Result<Vec<Violation>> {
        let repo = ctx.open_repo()?;
        let mut violations = Vec::new();
        let mut names = active_workspace_names(ctx);
        names.insert(ctx.default_workspace.clone());
        for name in names {
            let state_ref = refs::workspace_state_ref(&name);
            let oid = refs::read_ref(&ctx.root, &state_ref)
                .map_err(|e| anyhow::anyhow!("read {state_ref}: {e}"))?;
            let Some(oid) = oid else {
                continue;
            };
            if !object_present(&repo, oid.as_str(), Kind::Commit) {
                violations.push(Violation::new(
                    format!("{state_ref} → {} is not a valid commit", oid.as_str()),
                    Some(format!("Inspect: maw ws status {name}")),
                ));
            }
        }
        Ok(violations)
    }
}

// ---------------------------------------------------------------------------
// workspaces: head refs for workspaces that no longer exist (doctor subset)
// ---------------------------------------------------------------------------

/// List `(workspace, head_ref)` for oplog head refs whose workspace dir is gone.
fn stale_head_refs(ctx: &Ctx) -> Result<Vec<String>> {
    let repo = ctx.open_repo()?;
    let head_refs = repo
        .list_refs(refs::HEAD_PREFIX)
        .map_err(|e| anyhow::anyhow!("list head refs: {e}"))?;
    let mut names = Vec::new();
    for (ref_name, _oid) in head_refs {
        let ws = ref_name
            .as_str()
            .strip_prefix(refs::HEAD_PREFIX)
            .unwrap_or("")
            .to_string();
        if ws.is_empty() {
            continue;
        }
        if !workspace_is_live(ctx, &ws) {
            names.push(ws);
        }
    }
    names.sort();
    Ok(names)
}

struct StaleHeadRefs;

impl Invariant for StaleHeadRefs {
    fn id(&self) -> &'static str {
        "stale-head-refs"
    }
    fn severity(&self) -> Severity {
        Severity::Warn
    }
    fn description(&self) -> &'static str {
        "no refs/manifold/head/* ref survives its destroyed workspace"
    }
    fn in_doctor(&self) -> bool {
        true
    }
    fn check(&self, ctx: &Ctx) -> Result<Vec<Violation>> {
        let names = stale_head_refs(ctx)?;
        if names.is_empty() {
            return Ok(vec![]);
        }
        Ok(vec![Violation::new(
            format!(
                "{} stale oplog head ref(s) for non-existent workspace(s): {}",
                names.len(),
                names.join(", ")
            ),
            Some("Fix: maw gc".to_string()),
        )])
    }
}

// ---------------------------------------------------------------------------
// workspaces: git worktree bookkeeping agrees with maw's registry
// ---------------------------------------------------------------------------

struct WorktreeBookkeeping;

impl Invariant for WorktreeBookkeeping {
    fn id(&self) -> &'static str {
        "worktree-bookkeeping"
    }
    fn severity(&self) -> Severity {
        Severity::Warn
    }
    fn description(&self) -> &'static str {
        "git's .git/worktrees/* bookkeeping matches maw's live workspace dirs"
    }
    fn check(&self, ctx: &Ctx) -> Result<Vec<Violation>> {
        let repo = ctx.open_repo()?;
        let worktrees = match repo.worktree_list() {
            Ok(w) => w,
            Err(e) => {
                return Ok(vec![Violation::new(
                    format!("could not list worktrees ({e})"),
                    None,
                )]);
            }
        };
        let mut orphans = Vec::new();
        for wt in worktrees {
            // The main worktree ("main") and any worktree whose path still
            // exists on disk are fine. Flag registered worktrees whose
            // directory has been removed out from under git.
            if wt.name == "main" || wt.path.exists() {
                continue;
            }
            orphans.push(wt.name);
        }
        if orphans.is_empty() {
            return Ok(vec![]);
        }
        orphans.sort();
        Ok(vec![Violation::new(
            format!(
                "{} git worktree registration(s) point at a missing directory: {}",
                orphans.len(),
                orphans.join(", ")
            ),
            Some("Fix: git worktree prune  (then: maw gc)".to_string()),
        )])
    }
}

// ---------------------------------------------------------------------------
// ghost working copy: legacy jj metadata at the repo root (doctor subset)
// ---------------------------------------------------------------------------

struct GhostWorkingCopy;

impl Invariant for GhostWorkingCopy {
    fn id(&self) -> &'static str {
        "ghost-working-copy"
    }
    fn severity(&self) -> Severity {
        Severity::Info
    }
    fn description(&self) -> &'static str {
        "no leftover legacy .jj/working_copy at the repo root"
    }
    fn in_doctor(&self) -> bool {
        true
    }
    fn check(&self, ctx: &Ctx) -> Result<Vec<Violation>> {
        let ghost = ctx.root.join(".jj").join("working_copy");
        if ghost.exists() {
            Ok(vec![Violation::new(
                ".jj/working_copy/ exists at repo root (legacy jj migration residue)",
                Some("Migration cleanup: rm -rf .jj/working_copy/".to_string()),
            )])
        } else {
            Ok(vec![])
        }
    }
}

// ---------------------------------------------------------------------------
// destroy / recovery: dangling snapshot refs (doctor subset)
// ---------------------------------------------------------------------------

struct DanglingSnapshots;

impl Invariant for DanglingSnapshots {
    fn id(&self) -> &'static str {
        "dangling-snapshots"
    }
    fn severity(&self) -> Severity {
        Severity::Warn
    }
    fn description(&self) -> &'static str {
        "no orphaned recovery snapshot refs with no owning destroy record"
    }
    fn in_doctor(&self) -> bool {
        true
    }
    fn check(&self, ctx: &Ctx) -> Result<Vec<Violation>> {
        let dangling = recover::find_dangling_snapshots(&ctx.root)?;
        if dangling.is_empty() {
            return Ok(vec![]);
        }
        Ok(vec![Violation::new(
            format!(
                "{} dangling recovery snapshot ref(s) (superseded or left by crashed/completed \
                 merges)",
                dangling.len()
            ),
            Some("Preview: maw ws recover --gc --dry-run, then: maw ws recover --gc".to_string()),
        )])
    }
}

// ---------------------------------------------------------------------------
// destroy / recovery: abandoned-with-snapshot (doctor subset)
// ---------------------------------------------------------------------------

struct AbandonedWithSnapshot;

impl Invariant for AbandonedWithSnapshot {
    fn id(&self) -> &'static str {
        "abandoned-with-snapshot"
    }
    fn severity(&self) -> Severity {
        Severity::Warn
    }
    fn description(&self) -> &'static str {
        "destroyed workspaces with a still-pinned recovery snapshot (recover queue)"
    }
    fn in_doctor(&self) -> bool {
        true
    }
    fn check(&self, ctx: &Ctx) -> Result<Vec<Violation>> {
        let pinning = recover::classify_destroyed_workspaces(&ctx.root)?;
        if pinning.pinned.is_empty() {
            return Ok(vec![]);
        }
        let first = pinning.pinned.first().expect("non-empty checked above");
        Ok(vec![Violation::new(
            format!(
                "{} destroyed workspace(s) retain a recovery snapshot (no work lost; drain the \
                 recover queue): {}",
                pinning.pinned.len(),
                preview(&pinning.pinned)
            ),
            Some(format!(
                "Restore: maw ws recover {first} --to {first}-restored  |  Prune: maw gc \
                 --recovery-snapshots"
            )),
        )])
    }
}

// ---------------------------------------------------------------------------
// destroy / recovery: destroy-record-unpinned — repairable via re-pin
// ---------------------------------------------------------------------------

struct DestroyRecordUnpinned;

/// A destroyed-workspace record whose claimed recovery ref is gone but whose
/// snapshot object still exists — the safe re-pin target.
struct RepinTarget {
    workspace: String,
    ref_name: String,
    oid: String,
}

/// Resolve re-pin targets for the destroy-record-unpinned set.
///
/// The authoritative "which workspaces are unpinned" classification is
/// [`recover::classify_destroyed_workspaces`] (bn-3uou) — we reuse its
/// `unpinned` list rather than re-deriving it, then resolve, per record, the
/// OID the gone ref claimed and whether that object still exists (re-pinnable)
/// or is also gone (unrecoverable — fsck must decline).
fn unpinned_records(ctx: &Ctx) -> Result<(Vec<RepinTarget>, Vec<String>)> {
    let repo = ctx.open_repo()?;
    let existing: HashSet<String> = repo
        .list_refs("refs/manifold/recovery/")
        .map_err(|e| anyhow::anyhow!("list recovery refs: {e}"))?
        .into_iter()
        .map(|(n, _)| n.as_str().to_string())
        .collect();

    let pinning = recover::classify_destroyed_workspaces(&ctx.root)?;
    let mut repinnable = Vec::new();
    let mut lost = Vec::new();
    for ws in pinning.unpinned {
        for f in destroy_record::list_record_files(&ctx.root, &ws)? {
            let Ok(rec) = destroy_record::read_record(&ctx.root, &ws, &f) else {
                continue;
            };
            let Some(claimed) = rec.recovery_ref() else {
                continue;
            };
            if existing.contains(claimed) {
                continue;
            }
            // The ref is gone. Which OID does the record pin?
            let oid = if rec.snapshot_ref.as_deref() == Some(claimed) {
                rec.snapshot_oid.clone()
            } else {
                Some(rec.final_head.clone())
            };
            match oid {
                Some(oid) if object_present(&repo, &oid, Kind::Commit) => {
                    repinnable.push(RepinTarget {
                        workspace: ws.clone(),
                        ref_name: claimed.to_string(),
                        oid,
                    });
                }
                _ => lost.push(ws.clone()),
            }
        }
    }
    Ok((repinnable, lost))
}

impl Invariant for DestroyRecordUnpinned {
    fn id(&self) -> &'static str {
        "destroy-record-unpinned"
    }
    fn severity(&self) -> Severity {
        Severity::Warn
    }
    fn description(&self) -> &'static str {
        "destroy records whose claimed recovery ref is gone (snapshot at risk)"
    }
    fn in_doctor(&self) -> bool {
        true
    }
    fn is_repairable(&self) -> bool {
        true
    }
    fn check(&self, ctx: &Ctx) -> Result<Vec<Violation>> {
        let (repinnable, lost) = unpinned_records(ctx)?;
        let mut violations = Vec::new();
        for t in repinnable {
            violations.push(Violation::repairable(
                format!(
                    "{}: destroy record claims {} but the ref is gone; snapshot {} still exists \
                     and can be re-pinned",
                    t.workspace,
                    t.ref_name,
                    &t.oid[..t.oid.len().min(12)]
                ),
                Some(
                    "Re-pin the snapshot: maw fsck --repair  |  Or prune the desynced record: \
                     maw gc --recovery-snapshots"
                        .to_string(),
                ),
            ));
        }
        for ws in lost {
            violations.push(Violation::new(
                format!(
                    "{ws}: destroy record claims a recovery snapshot whose ref AND object are \
                     both gone — content is unrecoverable (fsck cannot repair; gc can prune the \
                     record)"
                ),
                Some("Prune the desynced record: maw gc --recovery-snapshots".to_string()),
            ));
        }
        Ok(violations)
    }
    fn repair(&self, ctx: &Ctx, dry_run: bool) -> Result<Vec<String>> {
        let (repinnable, lost) = unpinned_records(ctx)?;
        let mut receipts = Vec::new();
        for t in repinnable {
            if dry_run {
                receipts.push(format!(
                    "would re-pin {} → {} (from destroy record for {})",
                    t.ref_name,
                    &t.oid[..t.oid.len().min(12)],
                    t.workspace
                ));
                continue;
            }
            let oid = CoreOid::new(&t.oid)
                .map_err(|e| anyhow::anyhow!("invalid snapshot oid {}: {e}", t.oid))?;
            refs::write_ref(&ctx.root, &t.ref_name, &oid)
                .map_err(|e| anyhow::anyhow!("re-pin {}: {e}", t.ref_name))?;
            receipts.push(format!(
                "re-pinned {} → {} (recovered snapshot for {})",
                t.ref_name,
                &t.oid[..t.oid.len().min(12)],
                t.workspace
            ));
        }
        for ws in lost {
            receipts.push(format!(
                "declined: {ws}'s snapshot object is gone — unrecoverable, cannot re-pin (use \
                 `maw gc --recovery-snapshots` to prune the record)"
            ));
        }
        Ok(receipts)
    }
}

// ---------------------------------------------------------------------------
// destroy / recovery: latest.json points at an existing record
// ---------------------------------------------------------------------------

struct DestroyLatestPointer;

impl Invariant for DestroyLatestPointer {
    fn id(&self) -> &'static str {
        "destroy-latest-pointer"
    }
    fn severity(&self) -> Severity {
        Severity::Warn
    }
    fn description(&self) -> &'static str {
        "each destroyed workspace's latest.json points at an existing record"
    }
    fn check(&self, ctx: &Ctx) -> Result<Vec<Violation>> {
        let mut violations = Vec::new();
        for ws in destroy_record::list_destroyed_workspaces(&ctx.root)? {
            let Some(pointer) = destroy_record::read_latest_pointer(&ctx.root, &ws)? else {
                continue;
            };
            if destroy_record::read_record(&ctx.root, &ws, &pointer.record).is_err() {
                violations.push(Violation::new(
                    format!(
                        "{ws}: latest.json points at '{}' which does not exist (a directory scan \
                         still finds the real records, so recovery is not blocked)",
                        pointer.record
                    ),
                    Some(format!("Inspect: maw ws recover {ws}")),
                ));
            }
        }
        Ok(violations)
    }
}

// ---------------------------------------------------------------------------
// oplog: entries parse and referenced patch-set OIDs exist
// ---------------------------------------------------------------------------

struct OplogIntegrity;

impl Invariant for OplogIntegrity {
    fn id(&self) -> &'static str {
        "oplog-integrity"
    }
    fn severity(&self) -> Severity {
        Severity::Warn
    }
    fn description(&self) -> &'static str {
        "each workspace oplog parses and its referenced patch-set blobs exist"
    }
    fn check(&self, ctx: &Ctx) -> Result<Vec<Violation>> {
        let repo = ctx.open_repo()?;
        let head_refs = repo
            .list_refs(refs::HEAD_PREFIX)
            .map_err(|e| anyhow::anyhow!("list head refs: {e}"))?;
        let mut violations = Vec::new();
        for (ref_name, _oid) in head_refs {
            let ws = ref_name
                .as_str()
                .strip_prefix(refs::HEAD_PREFIX)
                .unwrap_or("");
            if ws.is_empty() {
                continue;
            }
            let Ok(ws_id) = WorkspaceId::new(ws) else {
                continue;
            };
            let ops = match walk_chain(&ctx.root, &ws_id, None, None) {
                Ok(ops) => ops,
                Err(OpLogReadError::NoHead { .. }) => continue,
                Err(e) => {
                    violations.push(Violation::new(
                        format!("{ws}: oplog is damaged ({e})"),
                        Some(format!("Repair: maw ws repair-oplog {ws}")),
                    ));
                    continue;
                }
            };
            for (op_oid, op) in ops {
                if let OpPayload::Snapshot { patch_set_oid } = &op.payload
                    && !object_present(&repo, patch_set_oid.as_str(), Kind::Blob)
                {
                    violations.push(Violation::new(
                        format!(
                            "{ws}: oplog op {} references patch-set blob {} which is missing",
                            &op_oid.as_str()[..op_oid.as_str().len().min(12)],
                            &patch_set_oid.as_str()[..patch_set_oid.as_str().len().min(12)]
                        ),
                        Some(format!("Repair: maw ws repair-oplog {ws}")),
                    ));
                }
            }
        }
        Ok(violations)
    }
}

// ---------------------------------------------------------------------------
// merge-state: no stale in-flight merge journal without a live process
// ---------------------------------------------------------------------------

struct MergeStateInvariant;

/// Classify the merge-state file into (violation, safe-to-remove) if present.
fn merge_state_status(ctx: &Ctx) -> (Option<Violation>, bool) {
    let manifold = ctx.flavor.manifold_dir(&ctx.root);
    let state_path = MergeStateFile::default_path(&manifold);
    let state = match MergeStateFile::read(&state_path) {
        Err(MergeStateError::NotFound(_)) => return (None, false),
        Err(e) => {
            return (
                Some(Violation::repairable(
                    format!(
                        "merge-state file present but unreadable ({e}) — this wedges all merges"
                    ),
                    Some("Fix: maw ws merge --abort  (or: maw fsck --repair)".to_string()),
                )),
                true,
            );
        }
        Ok(s) => s,
    };

    if state.phase.is_terminal() {
        return (
            Some(Violation::repairable(
                format!("leftover terminal merge-state (phase: {})", state.phase),
                Some("Fix: maw ws merge --abort  (or: maw fsck --repair)".to_string()),
            )),
            true,
        );
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    match state.staleness(now, DEFAULT_STALE_AFTER_SECS) {
        Staleness::Live => (None, false),
        Staleness::Orphaned => (
            Some(Violation::repairable(
                format!(
                    "ORPHANED merge-state (phase: {}, owner process gone) — blocks all future \
                     merges",
                    state.phase
                ),
                Some("Fix: maw ws merge --abort  (or: maw fsck --repair)".to_string()),
            )),
            true,
        ),
        Staleness::Indeterminate => (
            Some(Violation::new(
                format!(
                    "merge-state present (phase: {}) but owner liveness could not be confirmed",
                    state.phase
                ),
                Some("If no merge is running: maw ws merge --abort".to_string()),
            )),
            // Not auto-repairable: we cannot prove the owner is dead.
            false,
        ),
    }
}

impl Invariant for MergeStateInvariant {
    fn id(&self) -> &'static str {
        "merge-state"
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn description(&self) -> &'static str {
        "no stale in-flight merge journal without a live owner process"
    }
    fn in_doctor(&self) -> bool {
        true
    }
    fn is_repairable(&self) -> bool {
        true
    }
    fn check(&self, ctx: &Ctx) -> Result<Vec<Violation>> {
        let (violation, _) = merge_state_status(ctx);
        Ok(violation.into_iter().collect())
    }
    fn repair(&self, ctx: &Ctx, dry_run: bool) -> Result<Vec<String>> {
        let (violation, safe) = merge_state_status(ctx);
        if violation.is_none() {
            return Ok(vec![]);
        }
        if !safe {
            return Ok(vec![
                "declined: merge-state owner liveness could not be confirmed — not removing (run \
                 `maw ws merge --abort` if you are sure no merge is running)"
                    .to_string(),
            ]);
        }
        let manifold = ctx.flavor.manifold_dir(&ctx.root);
        let state_path = MergeStateFile::default_path(&manifold);
        if dry_run {
            return Ok(vec![format!(
                "would remove stale merge-state {}",
                state_path.display()
            )]);
        }
        std::fs::remove_file(&state_path)
            .map_err(|e| anyhow::anyhow!("remove {}: {e}", state_path.display()))?;
        Ok(vec![format!(
            "removed stale merge-state {} (owner process was gone)",
            state_path.display()
        )])
    }
}

// ---------------------------------------------------------------------------
// locks: stale-but-harmless create lockfiles (info only)
// ---------------------------------------------------------------------------

struct StaleLocks;

impl Invariant for StaleLocks {
    fn id(&self) -> &'static str {
        "stale-locks"
    }
    fn severity(&self) -> Severity {
        Severity::Info
    }
    fn description(&self) -> &'static str {
        "advisory lockfiles with no live holder are stale-but-harmless"
    }
    fn check(&self, ctx: &Ctx) -> Result<Vec<Violation>> {
        use fs4::fs_std::FileExt as _;

        let locks_dir = ctx.flavor.manifold_dir(&ctx.root).join("locks");
        if !locks_dir.exists() {
            return Ok(vec![]);
        }
        let mut stale = Vec::new();
        let mut stack = vec![locks_dir];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "lock") {
                    continue;
                }
                // Non-destructively probe: if we can take the advisory lock,
                // no live process holds it — it is a harmless stale file.
                // `try_lock_exclusive` returns `Err(WouldBlock)` when a live
                // holder exists; `Ok(())` means the file is unheld.
                let Ok(file) = std::fs::File::open(&path) else {
                    continue;
                };
                if file.try_lock_exclusive().is_ok() {
                    let _ = file.unlock();
                    stale.push(path.file_name().map_or_else(
                        || path.display().to_string(),
                        |n| n.to_string_lossy().to_string(),
                    ));
                }
            }
        }
        if stale.is_empty() {
            return Ok(vec![]);
        }
        stale.sort();
        Ok(vec![Violation::new(
            format!(
                "{} advisory lockfile(s) with no live holder (harmless; the next acquirer reuses \
                 them): {}",
                stale.len(),
                preview(&stale)
            ),
            None,
        )])
    }
}

// ---------------------------------------------------------------------------
// epoch/branch: configured branch exists and epoch is ancestor-or-equal
// ---------------------------------------------------------------------------

struct EpochBranchAncestor;

impl Invariant for EpochBranchAncestor {
    fn id(&self) -> &'static str {
        "epoch-branch-ancestor"
    }
    fn severity(&self) -> Severity {
        Severity::Warn
    }
    fn description(&self) -> &'static str {
        "the configured branch exists and epoch/current is an ancestor-or-equal of it"
    }
    fn check(&self, ctx: &Ctx) -> Result<Vec<Violation>> {
        use crate::workspace::epoch_drift::{EpochDriftKind, classify_drift};

        let backend = maw_core::backend::git::GitWorktreeBackend::new(ctx.root.clone());
        match classify_drift(&ctx.root, &ctx.branch, &backend) {
            Ok(None) => Ok(vec![]), // epoch unset — covered elsewhere
            Ok(Some(report)) => {
                if matches!(report.kind, EpochDriftKind::Diverged) {
                    Ok(vec![Violation::new(
                        format!(
                            "epoch ({}) is not an ancestor of branch '{}' ({}) — they have forked",
                            report.epoch_short, ctx.branch, report.branch_short
                        ),
                        Some(
                            "Investigate: git log --oneline --all; reset branch or epoch \
                             deliberately."
                                .to_string(),
                        ),
                    )])
                } else {
                    // InSync / FfAbsorbable / FfBlocked all mean epoch IS an
                    // ancestor-or-equal of the branch — the invariant holds.
                    Ok(vec![])
                }
            }
            Err(e) => Ok(vec![Violation::new(
                format!(
                    "could not compare epoch to branch '{}' ({e}) — the branch may not exist",
                    ctx.branch
                ),
                Some(format!(
                    "Ensure the branch exists: git rev-parse {}",
                    ctx.branch
                )),
            )]),
        }
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// `a, b, c (+N more)` preview of a name list.
fn preview(names: &[String]) -> String {
    let head = names
        .iter()
        .take(3)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > 3 {
        format!("{head} (+{} more)", names.len() - 3)
    } else {
        head
    }
}

// ---------------------------------------------------------------------------
// Tests: one violating state per invariant, constructed by hand.
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use maw_core::model::layout::LayoutFlavor;
    use maw_core::model::types::{EpochId, GitOid as CoreOid};
    use tempfile::TempDir;

    use super::*;
    use crate::workspace::capture::{CaptureMode, CaptureResult};
    use crate::workspace::destroy_record::{DestroyReason, write_destroy_record};

    fn git(root: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git run");
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_string()
    }

    /// A git repo (branch `main`) with one commit and a v2 `.manifold/` dir.
    fn setup() -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("tmp");
        let root = dir.path().to_path_buf();
        git(&root, &["init", "-b", "main"]);
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("README.md"), "# test\n").expect("write");
        git(&root, &["add", "README.md"]);
        git(&root, &["commit", "-m", "init"]);
        fs::create_dir_all(root.join(".manifold")).expect("manifold");
        fs::create_dir_all(root.join("ws")).expect("ws");
        (dir, root)
    }

    fn head_oid(root: &Path) -> String {
        git(root, &["rev-parse", "HEAD"])
    }

    /// Write a blob and return its OID (an object that exists but is NOT a
    /// commit — used to force wrong-kind / not-a-commit violations).
    fn blob_oid(root: &Path, content: &str) -> String {
        let out = Command::new("git")
            .args(["hash-object", "-w", "--stdin"])
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write as _;
                child
                    .stdin
                    .take()
                    .expect("stdin")
                    .write_all(content.as_bytes())?;
                child.wait_with_output()
            })
            .expect("hash-object");
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_string()
    }

    fn ctx(root: &Path) -> Ctx {
        Ctx {
            root: root.to_path_buf(),
            flavor: LayoutFlavor::detect_with_env(root),
            git_cwd: root.to_path_buf(),
            branch: "main".to_string(),
            default_workspace: "default".to_string(),
        }
    }

    fn set_epoch(root: &Path, oid: &str) {
        refs::write_epoch_current(root, &CoreOid::new(oid).expect("oid")).expect("epoch");
    }

    fn only(v: Vec<Violation>) -> Violation {
        assert_eq!(
            v.len(),
            1,
            "expected exactly one violation, got {}",
            v.len()
        );
        v.into_iter().next().unwrap()
    }

    // --- refs-manifold-object ---

    #[test]
    fn refs_object_flags_wrong_kind() {
        let (_d, root) = setup();
        let blob = blob_oid(&root, "not a commit");
        // A state ref must be a commit; point it at a blob.
        git(&root, &["update-ref", "refs/manifold/ws/phantom", &blob]);
        let inv = RefsManifoldObject;
        let v = inv.check(&ctx(&root)).expect("check");
        assert!(!v.is_empty(), "wrong-kind ref must be flagged");
        assert!(v[0].detail.contains("phantom"));
        assert_eq!(inv.severity(), Severity::Error);
    }

    #[test]
    fn refs_object_clean_when_all_present() {
        let (_d, root) = setup();
        let head = head_oid(&root);
        git(&root, &["update-ref", "refs/manifold/ws/default", &head]);
        set_epoch(&root, &head);
        let v = RefsManifoldObject.check(&ctx(&root)).expect("check");
        assert!(v.is_empty(), "healthy refs must pass: {v:?}");
    }

    // --- epoch-current-resolvable ---

    #[test]
    fn epoch_current_flags_missing_commit() {
        let (_d, root) = setup();
        let blob = blob_oid(&root, "blob-not-commit");
        set_epoch(&root, &blob);
        let inv = EpochCurrentResolvable;
        let v = only(inv.check(&ctx(&root)).expect("check"));
        assert!(v.detail.contains("missing"), "detail: {}", v.detail);
        assert_eq!(inv.severity(), Severity::Error);
    }

    #[test]
    fn epoch_current_ok_when_set_to_commit() {
        let (_d, root) = setup();
        set_epoch(&root, &head_oid(&root));
        let v = EpochCurrentResolvable.check(&ctx(&root)).expect("check");
        assert!(v.is_empty());
    }

    // --- workspace-head-valid ---

    #[test]
    fn workspace_head_flags_non_commit_state_ref() {
        let (_d, root) = setup();
        fs::create_dir_all(root.join("ws/alice")).expect("ws dir");
        let blob = blob_oid(&root, "x");
        git(&root, &["update-ref", "refs/manifold/ws/alice", &blob]);
        let inv = WorkspaceHeadValid;
        let v = inv.check(&ctx(&root)).expect("check");
        assert!(v.iter().any(|x| x.detail.contains("alice")));
        assert_eq!(inv.severity(), Severity::Error);
    }

    // --- stale-head-refs ---

    #[test]
    fn stale_head_refs_flags_gone_workspace() {
        let (_d, root) = setup();
        let head = head_oid(&root);
        git(&root, &["update-ref", "refs/manifold/head/ghost", &head]);
        let inv = StaleHeadRefs;
        let v = only(inv.check(&ctx(&root)).expect("check"));
        assert!(v.detail.contains("ghost"), "detail: {}", v.detail);
        assert_eq!(inv.severity(), Severity::Warn);
        assert!(inv.in_doctor());
    }

    #[test]
    fn stale_head_refs_ok_for_live_workspace() {
        let (_d, root) = setup();
        fs::create_dir_all(root.join("ws/live")).expect("ws");
        let head = head_oid(&root);
        git(&root, &["update-ref", "refs/manifold/head/live", &head]);
        let v = StaleHeadRefs.check(&ctx(&root)).expect("check");
        assert!(v.is_empty());
    }

    // --- worktree-bookkeeping ---

    #[test]
    fn worktree_bookkeeping_flags_missing_dir() {
        let (_d, root) = setup();
        // Register a real linked worktree, then remove its directory so git's
        // bookkeeping outlives the checkout.
        let wt = root.join("ws/wtree");
        git(&root, &["worktree", "add", wt.to_str().unwrap(), "HEAD"]);
        fs::remove_dir_all(&wt).expect("rm worktree dir");
        let inv = WorktreeBookkeeping;
        let v = inv.check(&ctx(&root)).expect("check");
        assert!(v.iter().any(|x| x.detail.contains("wtree")), "got {v:?}");
        assert_eq!(inv.severity(), Severity::Warn);
    }

    // --- ghost-working-copy ---

    #[test]
    fn ghost_working_copy_flags_legacy_jj() {
        let (_d, root) = setup();
        fs::create_dir_all(root.join(".jj/working_copy")).expect("jj");
        let inv = GhostWorkingCopy;
        let v = only(inv.check(&ctx(&root)).expect("check"));
        assert!(v.detail.contains("working_copy"));
        assert_eq!(inv.severity(), Severity::Info);
        assert!(inv.in_doctor());
    }

    // --- dangling-snapshots ---

    #[test]
    fn dangling_snapshots_flags_orphaned_refs() {
        let (_d, root) = setup();
        let head = head_oid(&root);
        // Two recovery refs for a workspace that does not exist → dangling.
        git(
            &root,
            &[
                "update-ref",
                "refs/manifold/recovery/ghost/2026-01-01T00-00-00Z",
                &head,
            ],
        );
        git(
            &root,
            &[
                "update-ref",
                "refs/manifold/recovery/ghost/2026-01-02T00-00-00Z",
                &head,
            ],
        );
        let inv = DanglingSnapshots;
        let v = only(inv.check(&ctx(&root)).expect("check"));
        assert!(v.detail.contains("dangling"), "detail: {}", v.detail);
        assert_eq!(inv.severity(), Severity::Warn);
        assert!(inv.in_doctor());
    }

    // --- abandoned-with-snapshot ---

    fn seed_destroyed(root: &Path, ws: &str, oid: &str, ts: &str, create_ref: bool) -> String {
        let ref_name = format!("refs/manifold/recovery/{ws}/{ts}");
        if create_ref {
            git(root, &["update-ref", &ref_name, oid]);
        }
        let git_oid = CoreOid::new(oid).expect("oid");
        let capture = CaptureResult {
            commit_oid: git_oid.clone(),
            pinned_ref: ref_name.clone(),
            dirty_paths: vec!["draft.txt".to_string()],
            mode: CaptureMode::WorktreeCapture,
        };
        let base = EpochId::new(&"a".repeat(40)).expect("epoch");
        write_destroy_record(
            root,
            ws,
            &base,
            &git_oid,
            Some(&capture),
            DestroyReason::Destroy,
        )
        .expect("record");
        ref_name
    }

    #[test]
    fn abandoned_with_snapshot_flags_pinned() {
        let (_d, root) = setup();
        let head = head_oid(&root);
        seed_destroyed(&root, "alice", &head, "2026-01-01T00-00-00Z", true);
        let inv = AbandonedWithSnapshot;
        let v = only(inv.check(&ctx(&root)).expect("check"));
        assert!(v.detail.contains("alice"), "detail: {}", v.detail);
        assert_eq!(inv.severity(), Severity::Warn);
        assert!(inv.in_doctor());
    }

    // --- destroy-record-unpinned (+ repair) ---

    #[test]
    fn destroy_record_unpinned_detects_and_repairs() {
        let (_d, root) = setup();
        let head = head_oid(&root);
        // Record claims a recovery ref, but we DON'T create the ref. The
        // snapshot OID (a real commit) still exists → re-pinnable.
        let ref_name = seed_destroyed(&root, "bob", &head, "2026-02-02T00-00-00Z", false);
        let inv = DestroyRecordUnpinned;

        let v = only(inv.check(&ctx(&root)).expect("check"));
        assert!(v.repairable, "unpinned-but-recoverable must be repairable");
        assert!(v.detail.contains("bob"));
        assert!(inv.in_doctor());

        // Ref does not exist yet.
        assert!(
            refs::read_ref(&root, &ref_name).expect("read").is_none(),
            "recovery ref should be absent before repair"
        );

        let receipts = inv.repair(&ctx(&root), false).expect("repair");
        assert!(
            receipts.iter().any(|r| r.contains("re-pinned")),
            "got {receipts:?}"
        );

        // Ref now exists and points at the snapshot commit.
        let repinned = refs::read_ref(&root, &ref_name)
            .expect("read")
            .expect("ref present");
        assert_eq!(repinned.as_str(), head);

        // Second fsck: no longer unpinned (it moved to the pinned/abandoned set).
        let v2 = inv.check(&ctx(&root)).expect("check2");
        assert!(v2.is_empty(), "re-pin must clear the violation: {v2:?}");
    }

    #[test]
    fn destroy_record_unpinned_declines_when_object_gone() {
        let (_d, root) = setup();
        // snapshot_oid references an object that does not exist → unrecoverable.
        let bogus = "d".repeat(40);
        seed_destroyed(&root, "carol", &bogus, "2026-03-03T00-00-00Z", false);
        let inv = DestroyRecordUnpinned;
        let v = only(inv.check(&ctx(&root)).expect("check"));
        assert!(!v.repairable, "a lost snapshot is not repairable");
        assert!(v.detail.contains("unrecoverable"), "detail: {}", v.detail);
        let receipts = inv.repair(&ctx(&root), false).expect("repair");
        assert!(
            receipts.iter().any(|r| r.contains("declined")),
            "got {receipts:?}"
        );
    }

    // --- destroy-latest-pointer ---

    #[test]
    fn destroy_latest_pointer_flags_missing_record() {
        let (_d, root) = setup();
        let head = head_oid(&root);
        seed_destroyed(&root, "dave", &head, "2026-04-04T00-00-00Z", true);
        // Corrupt latest.json to point at a nonexistent record file.
        let latest =
            crate::workspace::destroy_record::destroy_dir(&root, "dave").join("latest.json");
        fs::write(
            &latest,
            r#"{"record":"nope.json","destroyed_at":"2026-04-04T00:00:00Z"}"#,
        )
        .expect("write latest");
        let inv = DestroyLatestPointer;
        let v = only(inv.check(&ctx(&root)).expect("check"));
        assert!(v.detail.contains("dave"), "detail: {}", v.detail);
    }

    // --- oplog-integrity ---

    #[test]
    fn oplog_integrity_flags_damaged_chain() {
        let (_d, root) = setup();
        // A head ref pointing at a blob that is not a valid Operation JSON.
        let blob = blob_oid(&root, "{ not a valid operation }");
        git(&root, &["update-ref", "refs/manifold/head/broken", &blob]);
        let inv = OplogIntegrity;
        let v = only(inv.check(&ctx(&root)).expect("check"));
        assert!(v.detail.contains("broken"), "detail: {}", v.detail);
        assert_eq!(inv.severity(), Severity::Warn);
    }

    // --- merge-state (+ repair) ---

    fn write_terminal_merge_state(root: &Path) -> PathBuf {
        use maw_core::merge_state::MergeStateFile;
        let manifold = root.join(".manifold");
        let mut state = MergeStateFile::new(
            vec![maw_core::model::types::WorkspaceId::new("src").expect("ws")],
            EpochId::new(&"a".repeat(40)).expect("epoch"),
            0,
        );
        state.abort("test cleanup", 1).expect("abort → terminal");
        let path = MergeStateFile::default_path(&manifold);
        state.write_atomic(&path).expect("write");
        path
    }

    #[test]
    fn merge_state_detects_and_repairs_terminal() {
        let (_d, root) = setup();
        let path = write_terminal_merge_state(&root);
        let inv = MergeStateInvariant;

        let v = only(inv.check(&ctx(&root)).expect("check"));
        assert!(v.repairable);
        assert!(inv.in_doctor());
        assert_eq!(inv.severity(), Severity::Error);

        let receipts = inv.repair(&ctx(&root), false).expect("repair");
        assert!(
            receipts.iter().any(|r| r.contains("removed")),
            "got {receipts:?}"
        );
        assert!(!path.exists(), "stale merge-state must be removed");

        let v2 = inv.check(&ctx(&root)).expect("check2");
        assert!(v2.is_empty(), "second fsck clean after repair");
    }

    #[test]
    fn merge_state_dry_run_does_not_remove() {
        let (_d, root) = setup();
        let path = write_terminal_merge_state(&root);
        let inv = MergeStateInvariant;
        let receipts = inv.repair(&ctx(&root), true).expect("dry-run");
        assert!(
            receipts.iter().any(|r| r.contains("would remove")),
            "got {receipts:?}"
        );
        assert!(path.exists(), "dry-run must not delete");
    }

    // --- stale-locks ---

    #[test]
    fn stale_locks_flags_unheld_lockfile() {
        let (_d, root) = setup();
        let locks = root.join(".manifold/locks/create");
        fs::create_dir_all(&locks).expect("locks dir");
        fs::write(locks.join("agent-1.lock"), b"").expect("lock");
        let inv = StaleLocks;
        let v = only(inv.check(&ctx(&root)).expect("check"));
        assert!(v.detail.contains("agent-1.lock"), "detail: {}", v.detail);
        assert_eq!(inv.severity(), Severity::Info);
    }

    // --- epoch-branch-ancestor ---

    #[test]
    fn epoch_branch_ancestor_flags_diverged() {
        let (_d, root) = setup();
        let c0 = head_oid(&root);
        // epoch line diverges from main at c0.
        git(&root, &["checkout", "-b", "epochline"]);
        fs::write(root.join("e.txt"), "epoch\n").expect("write");
        git(&root, &["add", "e.txt"]);
        git(&root, &["commit", "-m", "epoch commit"]);
        let c_epoch = head_oid(&root);
        git(&root, &["checkout", "main"]);
        fs::write(root.join("m.txt"), "main\n").expect("write");
        git(&root, &["add", "m.txt"]);
        git(&root, &["commit", "-m", "main commit"]);
        assert_ne!(c0, c_epoch);
        set_epoch(&root, &c_epoch);

        let inv = EpochBranchAncestor;
        let v = only(inv.check(&ctx(&root)).expect("check"));
        assert!(
            v.detail.contains("forked") || v.detail.contains("ancestor"),
            "detail: {}",
            v.detail
        );
        assert_eq!(inv.severity(), Severity::Warn);
    }

    #[test]
    fn epoch_branch_ancestor_ok_when_in_sync() {
        let (_d, root) = setup();
        set_epoch(&root, &head_oid(&root));
        let v = EpochBranchAncestor.check(&ctx(&root)).expect("check");
        assert!(v.is_empty(), "in-sync epoch must pass: {v:?}");
    }

    // --- healthy repo: --repair is a byte-level no-op ---

    fn refs_snapshot(root: &Path) -> String {
        git(root, &["for-each-ref", "refs/manifold/"])
    }

    #[test]
    fn repair_on_healthy_repo_is_a_noop() {
        let (_d, root) = setup();
        let head = head_oid(&root);
        set_epoch(&root, &head);
        git(&root, &["update-ref", "refs/manifold/ws/default", &head]);
        // A pinned (healthy) recovery snapshot — not a violation to repair.
        seed_destroyed(&root, "alice", &head, "2026-01-01T00-00-00Z", true);

        let before = refs_snapshot(&root);
        // Run every repairable invariant's repair against the healthy repo.
        for inv in catalog() {
            if inv.is_repairable() {
                let _ = inv.repair(&ctx(&root), false).expect("repair");
            }
        }
        let after = refs_snapshot(&root);
        assert_eq!(
            before, after,
            "--repair must not change refs on a healthy repo"
        );
    }
}
