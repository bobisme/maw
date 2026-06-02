//! `maw ws destroy --dry-run` — predict destroy outcome without
//! touching the workspace. SG4 / bn-29fi (destroy-prevention).
//!
//! # Why this exists
//!
//! `ws_recover_invoked` is, by definition, a wasted-recovery turn: the
//! agent's earlier `destroy` left work behind that then needed
//! rescuing. The destroy-prevention mitigation class (per the SG4 fix
//! backlog) attacks the upstream cause — make it trivial for the agent
//! to *consult before destroying*, so the safer alternative
//! (`maw ws merge --destroy` for committed-unintegrated work) becomes
//! the first attempt.
//!
//! `--dry-run` is the structured pre-flight: one call returns the
//! lifecycle state, whether destroy will refuse, whether it will
//! capture a snapshot, and the exact recommended command. The agent
//! does not need to issue the destroy and parse a refusal message
//! before deciding.
//!
//! # Output contract
//!
//! [`DestroyPreview`] serializes as JSON when `--format json` is
//! passed (the default also yields `Pretty`/`Text` for humans). Field
//! names are stable; new fields are additive.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::format::OutputFormat;
use maw_core::backend::WorkspaceBackend;
use maw_core::model::diff::compute_patchset;
use maw_core::model::types::WorkspaceId;

use super::destroy_record;
use super::lifecycle::{LifecycleSignals, LifecycleState};
use super::{
    DEFAULT_WORKSPACE, MawConfig, ensure_repo_root, get_backend, metadata, workspace_path,
};

/// Structured prediction of a `maw ws destroy` invocation. Returned by
/// the `--dry-run` surface (SG4 bn-29fi).
///
/// The four bool fields are independent prediction dimensions and
/// collapsing them into one enum would obscure the contract — agents
/// consume each field by name. The `struct_excessive_bools` lint is
/// silenced for the same reason it is silenced on
/// `LifecycleSignals` (also four bools, also independent).
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize)]
pub struct DestroyPreview {
    /// Workspace name as supplied by the caller.
    pub workspace: String,
    /// Safe-cleanup vocabulary state for the workspace (bn-221b /
    /// bn-29fi). Lets the agent branch on a stable kebab-case slug.
    pub lifecycle_state: LifecycleState,
    /// What the destroy command will do given the supplied `--force`
    /// flag at preview time. One of:
    /// - `would-destroy` — destroy will proceed cleanly (Clean / Integrated)
    /// - `would-refuse` — destroy will refuse to avoid data loss
    /// - `would-force-snapshot` — destroy with `--force` will proceed
    ///   AND capture a recovery snapshot first
    /// - `already-absent` — the workspace dir is already gone
    pub action: PreviewAction,
    /// True iff the destroy would proceed (i.e. action is
    /// `would-destroy` or `would-force-snapshot` or `already-absent`).
    pub would_proceed: bool,
    /// True iff a recovery snapshot will be captured prior to the
    /// destroy. Mirrors the `--force` snapshot semantics in
    /// `create::destroy`.
    pub would_capture_snapshot: bool,
    /// True iff this destroy would leave committed work in a recovery
    /// snapshot that has NOT been integrated to the branch — i.e. the
    /// agent will likely need `maw ws recover` afterwards to finish
    /// the job. The destroy-prevention cue: if this is `true`, prefer
    /// `recommended_command` (typically `maw ws merge ... --destroy`).
    pub would_need_recovery: bool,
    /// Count of unmerged-change touched paths (matches what the
    /// `Refusing destroy: N unmerged change(s)` message reports).
    pub touched_count: usize,
    /// Exact next-action command the agent should consider. Always
    /// non-empty: even for `would-destroy` (Clean) the field carries
    /// the destroy command itself so output is a single uniform shape.
    pub recommended_command: String,
    /// Human-readable rationale for the `recommended_command`. Short,
    /// one line; renderable as the second line of the text form.
    pub rationale: String,
    /// True iff a prior destroy of this same workspace name left a
    /// pinned snapshot in `.manifold/artifacts/ws/<name>/destroy/`.
    /// Reported even for present workspaces so the agent sees that
    /// recovery context exists from a previous lifecycle. (When the
    /// workspace is missing AND this is true, the lifecycle promotes
    /// to `AbandonedWithSnapshot`.)
    pub has_prior_snapshot: bool,
}

/// Stable kebab-case action slug for [`DestroyPreview::action`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PreviewAction {
    /// Workspace is Clean/Integrated — destroy proceeds, no snapshot
    /// needed.
    WouldDestroy,
    /// Workspace has unmerged work and `--force` was not set — destroy
    /// will refuse.
    WouldRefuse,
    /// `--force` was set AND workspace had unmerged work — destroy
    /// will proceed but capture a snapshot first.
    WouldForceSnapshot,
    /// Workspace directory is already gone; destroy is a no-op (or, if
    /// residual state exists and `--force` is set, will purge it).
    AlreadyAbsent,
}

impl PreviewAction {
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::WouldDestroy => "would-destroy",
            Self::WouldRefuse => "would-refuse",
            Self::WouldForceSnapshot => "would-force-snapshot",
            Self::AlreadyAbsent => "already-absent",
        }
    }
}

/// CLI entry for `maw ws destroy --dry-run`. Pure inspection: no
/// state-mutating call is made.
pub fn preview(name: &str, force: bool, format: Option<OutputFormat>) -> Result<()> {
    if name == DEFAULT_WORKSPACE {
        bail!("Cannot destroy the default workspace");
    }
    let root = ensure_repo_root()?;
    if let Ok(config) = MawConfig::load(&root)
        && name == config.default_workspace()
    {
        bail!("Cannot destroy the default workspace");
    }

    let preview = build_preview(name, force)?;
    let format = OutputFormat::resolve(format);
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&preview)?);
        }
        OutputFormat::Text | OutputFormat::Pretty => {
            render_text(&preview);
        }
    }
    Ok(())
}

/// Build a [`DestroyPreview`] without printing. Exposed so other
/// surfaces (status, doctor) can reuse the prediction without
/// re-deriving it.
pub fn build_preview(name: &str, force: bool) -> Result<DestroyPreview> {
    let root = ensure_repo_root()?;
    let path = workspace_path(name)?;

    // Snapshot-presence check is identical for present + missing
    // workspaces — both want the `has_prior_snapshot` signal.
    let has_prior_snapshot = workspace_has_pinned_snapshot(&root, name);

    if !path.exists() {
        // Already-absent: short-circuit, no backend call.
        let signals = LifecycleSignals {
            missing: true,
            has_pinned_snapshot: has_prior_snapshot,
            ..LifecycleSignals::default()
        };
        let lifecycle_state = LifecycleState::classify(signals);
        let recommended_command = lifecycle_state
            .fix_command(name, false)
            .unwrap_or_else(|| format!("# nothing to do (workspace '{name}' already absent)"));
        let rationale = if has_prior_snapshot {
            "Workspace dir is gone, but a recovery snapshot is pinned. \
                 Recover into a fresh name to inspect/merge its work."
                .to_string()
        } else {
            format!("Workspace '{name}' is already absent on disk; no action needed.")
        };
        return Ok(DestroyPreview {
            workspace: name.to_string(),
            lifecycle_state,
            action: PreviewAction::AlreadyAbsent,
            would_proceed: true,
            would_capture_snapshot: false,
            would_need_recovery: has_prior_snapshot,
            touched_count: 0,
            recommended_command,
            rationale,
            has_prior_snapshot,
        });
    }

    // Workspace exists — gather signals and predict the outcome.
    build_present_preview(name, force, has_prior_snapshot, &path)
}

/// Build a preview for a workspace whose directory exists. Split out
/// of `build_preview` so each leaf is short enough to follow.
fn build_present_preview(
    name: &str,
    force: bool,
    has_prior_snapshot: bool,
    path: &std::path::Path,
) -> Result<DestroyPreview> {
    let root = ensure_repo_root()?;
    let backend = get_backend()?;
    let ws_id =
        WorkspaceId::new(name).map_err(|e| anyhow::anyhow!("Invalid workspace name: {e}"))?;
    let status = backend
        .status(&ws_id)
        .map_err(|e| anyhow::anyhow!("Failed to inspect workspace state: {e}"))?;
    let base_epoch = status.base_epoch.to_epoch_id();
    let touched_count = compute_patchset(path, &base_epoch)
        .map(|patch_set| patch_set.len())
        .map_err(|e| anyhow::anyhow!("Failed to inspect local changes: {e}"))?
        .max(status.dirty_count());

    let signals = LifecycleSignals {
        missing: false,
        // bn-16x2: recorded-conflict sidecar signal (matches `merge --check`),
        // not a tracked-content marker scan.
        rebase_conflicts: super::resolve::recorded_conflict_count(&root, name),
        is_stale: false,  // not relevant for destroy-action prediction
        commits_ahead: 0, // approximated by touched_count for preview
        has_uncommitted: touched_count > 0,
        was_integrated: false,
        has_pinned_snapshot: false, // present ws is never AbandonedWithSnapshot
    };
    let lifecycle_state = LifecycleState::classify(signals);

    // mode_persistent is currently not used by the recommendation
    // policy (we prefer merge-with-destroy for any unmerged work),
    // but reading the metadata is cheap and keeps the call site
    // ready for a future per-mode tweak.
    let _ = metadata::read(&root, name);

    let (action, would_proceed, would_capture_snapshot, would_need_recovery) =
        classify_destroy_action(touched_count, force);
    let (recommended_command, rationale) = recommendation(action, name, touched_count);

    Ok(DestroyPreview {
        workspace: name.to_string(),
        lifecycle_state,
        action,
        would_proceed,
        would_capture_snapshot,
        would_need_recovery,
        touched_count,
        recommended_command,
        rationale,
        has_prior_snapshot,
    })
}

/// Decide the predicted action + the three boolean flags it implies.
const fn classify_destroy_action(
    touched_count: usize,
    force: bool,
) -> (PreviewAction, bool, bool, bool) {
    if touched_count == 0 {
        // Clean (or close enough) — destroy is safe.
        (PreviewAction::WouldDestroy, true, false, false)
    } else if force {
        // Force + dirty: snapshot then destroy. Agent will likely
        // need to recover afterwards.
        (PreviewAction::WouldForceSnapshot, true, true, true)
    } else {
        // Dirty + no --force: destroy refuses.
        (PreviewAction::WouldRefuse, false, false, false)
    }
}

/// Build the `(recommended_command, rationale)` pair from the action.
fn recommendation(action: PreviewAction, name: &str, touched_count: usize) -> (String, String) {
    match action {
        PreviewAction::WouldDestroy => (
            format!("maw ws destroy {name}"),
            format!(
                "Workspace '{name}' is clean ({touched_count} touched paths). Destroy proceeds."
            ),
        ),
        PreviewAction::WouldRefuse => (
            format!("maw ws merge {name} --into default --destroy"),
            format!(
                "Destroy refused: {touched_count} unmerged change(s). \
                 Prefer merge-then-destroy over `--force` to avoid a recover round-trip."
            ),
        ),
        PreviewAction::WouldForceSnapshot => (
            format!("maw ws merge {name} --into default --destroy"),
            format!(
                "Force-destroy with {touched_count} unmerged change(s) WILL capture a \
                 recovery snapshot, but the work will need a separate recover+merge \
                 turn. Merge-with-destroy lands the work in one step."
            ),
        ),
        PreviewAction::AlreadyAbsent => {
            // Handled by the early return in `build_preview`; kept
            // here for total-match exhaustiveness.
            ("# nothing to do".to_string(), String::new())
        }
    }
}

/// Render the human-readable form of a destroy preview. The JSON form
/// is the agent contract; this form is the human-readable companion.
fn render_text(preview: &DestroyPreview) {
    println!("maw ws destroy --dry-run {}", preview.workspace);
    println!("======================================");
    println!("  lifecycle:           {}", preview.lifecycle_state.slug());
    println!("  action:              {}", preview.action.slug());
    println!("  would_proceed:       {}", preview.would_proceed);
    println!(
        "  would_capture_snapshot: {}",
        preview.would_capture_snapshot
    );
    println!("  would_need_recovery: {}", preview.would_need_recovery);
    println!("  touched_count:       {}", preview.touched_count);
    println!("  has_prior_snapshot:  {}", preview.has_prior_snapshot);
    println!();
    println!("Rationale: {}", preview.rationale);
    println!("Recommended: {}", preview.recommended_command);
}

/// True iff a destroy record or recovery ref exists for `name`.
/// Layout-agnostic (uses `destroy_record` helpers, no raw paths).
pub fn workspace_has_pinned_snapshot(root: &std::path::Path, name: &str) -> bool {
    // Fast path: latest pointer.
    if matches!(destroy_record::read_latest_pointer(root, name), Ok(Some(_))) {
        return true;
    }
    // Slower path: any record file.
    destroy_record::list_record_files(root, name).is_ok_and(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_action_slugs_are_kebab_case() {
        for (action, expected) in [
            (PreviewAction::WouldDestroy, "would-destroy"),
            (PreviewAction::WouldRefuse, "would-refuse"),
            (PreviewAction::WouldForceSnapshot, "would-force-snapshot"),
            (PreviewAction::AlreadyAbsent, "already-absent"),
        ] {
            assert_eq!(action.slug(), expected);
            let json = serde_json::to_string(&action).expect("serialize");
            assert_eq!(json, format!("\"{expected}\""));
        }
    }

    #[test]
    fn preview_serializes_with_stable_fields() {
        // Sanity-check that the JSON shape carries every field the
        // agent contract names. Use a hand-built preview because the
        // builder needs a real repo.
        let preview = DestroyPreview {
            workspace: "alice".to_string(),
            lifecycle_state: LifecycleState::DirtyUncommitted,
            action: PreviewAction::WouldRefuse,
            would_proceed: false,
            would_capture_snapshot: false,
            would_need_recovery: false,
            touched_count: 3,
            recommended_command: "maw ws merge alice --into default --destroy".to_string(),
            rationale: "Destroy refused: 3 unmerged change(s).".to_string(),
            has_prior_snapshot: false,
        };
        let json = serde_json::to_string_pretty(&preview).expect("serialize");
        for field in [
            "workspace",
            "lifecycle_state",
            "action",
            "would_proceed",
            "would_capture_snapshot",
            "would_need_recovery",
            "touched_count",
            "recommended_command",
            "rationale",
            "has_prior_snapshot",
        ] {
            assert!(
                json.contains(field),
                "preview JSON is missing field {field:?}:\n{json}"
            );
        }
        // Stable kebab-case slug for the action.
        assert!(json.contains("would-refuse"));
        assert!(json.contains("dirty-uncommitted"));
    }
}
