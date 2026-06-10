//! Effective conflict state — ONE source of truth (bn-8zqz / bn-21cj).
//!
//! Historically, four readers each derived "is this workspace conflicted?"
//! from a different signal and could disagree:
//!
//! * the merge gate (`merge::assert_sources_clean_for_merge`) trusted the
//!   structured/legacy sidecars verbatim,
//! * `maw ws conflicts` ran the merge engine plus an unfiltered worktree
//!   marker scan,
//! * `maw ws resolve --list` read the sidecar (then a dirty-working-copy
//!   fallback, bn-lm3i),
//! * `ws list` / `ws status` / `lifecycle:conflicted` read the raw sidecar
//!   count (bn-16x2).
//!
//! The field failure (bn-8zqz): after an agent MANUALLY resolved committed
//! conflict markers and committed the resolution, the sidecar became stale —
//! `ws conflicts` said "clean", `merge --check` blocked on the sidecar, and
//! `resolve --list` agreed with the blocker, while the actual file had zero
//! markers. Only an extra `maw ws sync` (and only on a non-stale workspace)
//! cleared the metadata.
//!
//! This module is the single helper all readers now consult. The rules:
//!
//! 1. A sidecar entry is only a GENUINE conflict if there is still evidence
//!    in the workspace: conflict markers on a sidecar-listed path
//!    (committed or dirty — `find_conflicted_files_filtered`, bn-3oau), or a
//!    tool-authored placeholder blob in HEAD (bn-28d1).
//! 2. Sidecar-with-no-remaining-evidence = RESOLVED (a manual resolution was
//!    committed). The stale sidecar is cleared on the spot (best-effort) so
//!    no follow-up `maw ws sync` is ever required.
//! 3. No-sidecar-but-placeholder-blobs-in-HEAD = UNRESOLVED (the bn-28d1
//!    tamper tripwire). This is never auto-cleared and the merge gate keeps
//!    refusing it even under `--force`.
//! 4. COMMITTED marker literals in ordinary tracked content (docs, test
//!    fixtures) stay invisible: marker evidence is only consulted for paths
//!    the sidecar lists (bn-16x2/bn-m6ad), and the placeholder tripwire
//!    matches exact tool-authored byte prefixes only.
//!
//! The dirty-working-copy fallback (bn-lm3i — genuine conflicts with NO
//! sidecar after a dirty-default merge overlap) intentionally stays in
//! `resolve.rs`; it concerns uncommitted state in the merge TARGET, not the
//! recorded conflict metadata this helper arbitrates.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use maw_git::GitRepo as _;

/// One-line notice printed by read paths when they clear a stale sidecar.
pub const STALE_CLEAR_NOTICE: &str =
    "Cleared stale conflict metadata after a manual resolution commit.";

/// The verified conflict state of a workspace.
#[derive(Clone, Debug, Default)]
pub struct EffectiveConflictState {
    /// Conflict paths recorded by the surviving (non-stale) sidecar.
    /// Empty when no sidecar exists or when it was found stale and cleared.
    pub recorded_paths: Vec<PathBuf>,
    /// Tool-authored placeholder blobs found in the workspace HEAD tree
    /// (bn-28d1). May be non-empty even when no sidecar exists.
    pub placeholder_paths: Vec<PathBuf>,
    /// True when a sidecar existed but every recorded conflict had been
    /// manually resolved and committed (no markers on recorded paths, no
    /// placeholder blobs in HEAD) — the stale sidecar was cleared.
    pub cleared_stale_sidecar: bool,
}

impl EffectiveConflictState {
    /// True when the workspace has any genuine unresolved conflict.
    pub const fn is_conflicted(&self) -> bool {
        !self.recorded_paths.is_empty() || !self.placeholder_paths.is_empty()
    }

    /// Number of genuine unresolved conflicts. Recorded (sidecar-backed)
    /// conflicts take precedence; with no surviving sidecar this falls back
    /// to the count of placeholder blobs in HEAD.
    pub const fn conflict_count(&self) -> usize {
        if self.recorded_paths.is_empty() {
            self.placeholder_paths.len()
        } else {
            self.recorded_paths.len()
        }
    }

    /// Union of all paths with unresolved conflict evidence (sorted, deduped).
    pub fn unresolved_paths(&self) -> Vec<PathBuf> {
        let mut set: BTreeSet<PathBuf> = self.recorded_paths.iter().cloned().collect();
        set.extend(self.placeholder_paths.iter().cloned());
        set.into_iter().collect()
    }
}

/// Full inspection: verify the sidecars against reality AND run the HEAD
/// placeholder tripwire even when no sidecar exists.
///
/// Auto-clears a stale sidecar (rule 2 above) as a side effect; callers
/// should surface [`STALE_CLEAR_NOTICE`] when `cleared_stale_sidecar` is set
/// (to stderr on JSON-emitting paths so machine output stays parseable).
///
/// # Errors
///
/// Returns an error when the workspace HEAD tree cannot be read or the
/// marker verification scan fails — callers on informational paths may
/// degrade gracefully; the merge gate propagates (fail closed).
pub fn effective_conflict_state(
    root: &Path,
    ws_name: &str,
    ws_path: &Path,
) -> Result<EffectiveConflictState> {
    compute(root, ws_name, ws_path, true)
}

/// Cheap variant for high-fan-out surfaces (`ws list`, `maw status`,
/// `ws diff`, destroy previews): identical sidecar verification + auto-clear,
/// but the HEAD placeholder walk is skipped when no sidecar is recorded —
/// so the common no-sidecar case costs one file-existence check.
///
/// Returns the effective conflict count; on verification errors it falls
/// back to the raw sidecar count (conservative: never hides a recorded
/// conflict because verification failed).
pub fn effective_recorded_conflict_count(root: &Path, ws_name: &str, ws_path: &Path) -> u32 {
    compute(root, ws_name, ws_path, false).map_or_else(
        |_| super::resolve::recorded_conflict_count(root, ws_name),
        |state| u32::try_from(state.conflict_count()).unwrap_or(u32::MAX),
    )
}

/// Read the recorded conflict paths from the structured sidecar, falling
/// back to the legacy sidecar — the same priority the merge gate has always
/// used (bn-m6ad/bn-3oau).
fn recorded_sidecar_paths(root: &Path, ws_name: &str) -> Vec<PathBuf> {
    if let Some(tree) = super::resolve_structured::read_conflict_tree_sidecar(root, ws_name) {
        return tree.conflicts.into_keys().collect();
    }
    if let Some(legacy) = super::sync::read_rebase_conflicts(root, ws_name) {
        let set: BTreeSet<PathBuf> = legacy
            .conflicts
            .into_iter()
            .map(|c| PathBuf::from(c.path))
            .collect();
        return set.into_iter().collect();
    }
    Vec::new()
}

/// Scan the workspace HEAD tree for tool-authored placeholder blobs.
///
/// Returns `Ok(None)` when the workspace HEAD cannot be resolved (missing
/// worktree, detached state mid-surgery) — "could not verify", which callers
/// must treat conservatively. Repo/tree read failures propagate.
fn head_placeholder_paths(root: &Path, ws_path: &Path) -> Result<Option<Vec<PathBuf>>> {
    let Ok(head_oid_str) = super::merge::resolve_workspace_head_oid(ws_path) else {
        return Ok(None);
    };
    let head_oid: maw_git::GitOid = head_oid_str
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid workspace HEAD OID '{head_oid_str}': {e}"))?;
    let repo = maw_git::GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;
    let commit = repo
        .read_commit(head_oid)
        .map_err(|e| anyhow::anyhow!("read_commit({head_oid}) failed: {e}"))?;
    Ok(Some(super::merge::find_tool_placeholder_blobs(
        &repo,
        commit.tree_oid,
    )?))
}

fn compute(
    root: &Path,
    ws_name: &str,
    ws_path: &Path,
    scan_head_without_sidecar: bool,
) -> Result<EffectiveConflictState> {
    let recorded = recorded_sidecar_paths(root, ws_name);
    let has_sidecar = !recorded.is_empty();

    // Worktree gone → nothing can be verified. Report the raw sidecar state
    // (conservative) with no placeholder info and no clearing.
    if !ws_path.exists() {
        return Ok(EffectiveConflictState {
            recorded_paths: recorded,
            ..EffectiveConflictState::default()
        });
    }

    // HEAD placeholder tripwire. Skipped on the cheap path when there is no
    // sidecar — the merge gate (full path) still runs it unconditionally.
    let head_scan: Option<Vec<PathBuf>> = if has_sidecar || scan_head_without_sidecar {
        head_placeholder_paths(root, ws_path)?
    } else {
        Some(Vec::new())
    };

    let mut state = EffectiveConflictState {
        recorded_paths: recorded,
        placeholder_paths: head_scan.clone().unwrap_or_default(),
        cleared_stale_sidecar: false,
    };

    if !has_sidecar {
        return Ok(state);
    }

    // Verify the sidecar against reality: do the recorded paths still carry
    // operation-introduced conflict markers (committed or dirty)?
    let tracked: BTreeSet<PathBuf> = state.recorded_paths.iter().cloned().collect();
    let marker_evidence = super::resolve::find_conflicted_files_filtered(ws_path, Some(&tracked))?;

    let verified_clean = marker_evidence.is_empty()
        && matches!(&head_scan, Some(placeholders) if placeholders.is_empty());

    if verified_clean {
        // Manual resolution was committed: the metadata is stale. Clear it
        // right here (best-effort — a read-only filesystem must not turn a
        // read path into an error; the state we return is authoritative
        // either way and the next reader will retry the clear).
        let _ = super::resolve_structured::clear_conflict_sidecars(root, ws_name);
        state.recorded_paths.clear();
        state.cleared_stale_sidecar = true;
    }

    Ok(state)
}
