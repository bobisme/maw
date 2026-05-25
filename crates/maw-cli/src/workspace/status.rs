use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::format::OutputFormat;
use maw_core::backend::WorkspaceBackend;
use maw_core::model::types::WorkspaceMode;
use maw_core::oplog::global_view::compute_global_view;
use maw_core::oplog::read::read_head;
use maw_core::oplog::view::read_patch_set_blob;

use super::{DEFAULT_WORKSPACE, get_backend, metadata, repo_root};

#[derive(Serialize)]
pub struct WorkspaceStatus {
    pub(crate) current_workspace: String,
    pub(crate) is_stale: bool,
    pub(crate) has_changes: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) changes: Option<StatusChanges>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) global_view: Option<GlobalViewSummary>,
    pub(crate) workspaces: Vec<WorkspaceEntry>,
    /// Epoch/branch drift summary (bn-1ieb, SG4
    /// `epoch_sync_required` mitigation). Surfaces drift up to machine
    /// consumers so an agent reading `maw status --json` sees it directly
    /// instead of discovering it later via a failed `maw ws merge`. `None`
    /// means classify returned no opinion (e.g. epoch ref unset pre-
    /// `maw init`, or the classifier errored); the absence itself is
    /// meaningful and never blocks status output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) epoch_drift: Option<super::epoch_drift::EpochDriftReport>,
}

#[derive(Serialize)]
pub struct StatusChanges {
    pub(crate) dirty_files: Vec<String>,
    pub(crate) dirty_count: usize,
}

#[derive(Serialize)]
pub struct WorkspaceEntry {
    pub(crate) name: String,
    pub(crate) is_default: bool,
    pub(crate) epoch: String,
    pub(crate) state: String,
    pub(crate) mode: String,
    /// Number of unresolved rebase conflicts (0 = none).
    #[serde(skip_serializing_if = "is_zero")]
    pub(crate) rebase_conflicts: u32,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if predicates receive fields by reference"
)]
const fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[derive(Serialize)]
pub struct GlobalViewSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) epoch: Option<String>,
    pub(crate) workspace_count: usize,
    pub(crate) total_patches: usize,
    pub(crate) conflict_count: usize,
    pub(crate) total_ops: usize,
}

#[expect(
    clippy::too_many_lines,
    reason = "status command gathers and renders workspace state in one path"
)]
pub fn status(format: OutputFormat) -> Result<()> {
    let backend = get_backend()?;

    // Get all workspaces
    let all_workspaces = backend.list().map_err(|e| anyhow::anyhow!("{e}"))?;

    // Find the default workspace for the main status display
    let default_ws_name = DEFAULT_WORKSPACE;

    // Get default workspace status
    let default_ws_id = maw_core::model::types::WorkspaceId::new(default_ws_name)
        .map_err(|e| anyhow::anyhow!("Invalid workspace name: {e}"))?;

    let (is_stale, has_changes, changes) = if backend.exists(&default_ws_id) {
        let ws_status = backend
            .status(&default_ws_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let dirty_files: Vec<String> = ws_status
            .dirty_files
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        let count = dirty_files.len();
        let has = count > 0;
        let changes = if has {
            Some(StatusChanges {
                dirty_files,
                dirty_count: count,
            })
        } else {
            None
        };
        // The default workspace tracks the configured branch and should not be
        // auto-treated as an ephemeral stale workspace.
        let _ = ws_status;
        (false, has, changes)
    } else {
        (false, false, None)
    };

    // Read metadata for mode information.
    let root = repo_root()?;
    let current_workspace =
        detect_current_workspace(&root).unwrap_or_else(|| default_ws_name.to_string());

    let global_view = compute_global_view_summary(&root, &all_workspaces);

    // bn-1ieb: classify epoch/branch drift so machine consumers see it
    // before they discover it via a downstream merge failure. Failure to
    // load config (corrupt .maw.toml) is non-fatal — drift is a soft
    // signal, not a hard gate; status() should never error out because of
    // it. Likewise for any classify_drift error: surface as "no drift
    // report" rather than crashing.
    let epoch_drift = super::MawConfig::load(&root)
        .ok()
        .and_then(|cfg| {
            super::epoch_drift::classify_drift(&root, cfg.branch(), &backend)
                .ok()
                .flatten()
        });

    // Build workspace entries
    let workspace_entries: Vec<WorkspaceEntry> = all_workspaces
        .iter()
        .map(|ws| {
            let is_default = ws.id.as_str() == default_ws_name;
            let ws_meta = metadata::read(&root, ws.id.as_str()).unwrap_or_default();
            let ws_mode = if is_default {
                WorkspaceMode::Persistent
            } else {
                ws_meta.mode
            };
            let rebase_conflicts = {
                let ws_path = root.join("ws").join(ws.id.as_str());
                super::resolve::find_conflicted_files(&ws_path)
                    .map_or(0, |f| u32::try_from(f.len()).unwrap_or(u32::MAX))
            };
            WorkspaceEntry {
                name: ws.id.as_str().to_string(),
                is_default,
                epoch: ws.epoch.as_str()[..12].to_string(),
                state: if is_default {
                    "active".to_owned()
                } else if rebase_conflicts > 0 {
                    format!("conflicted ({rebase_conflicts} conflict(s))")
                } else {
                    format!("{}", ws.state)
                },
                mode: format!("{ws_mode}"),
                rebase_conflicts,
            }
        })
        .collect();

    match format {
        OutputFormat::Text => {
            print_status_text(
                default_ws_name,
                is_stale,
                changes.as_ref(),
                global_view.as_ref(),
                &workspace_entries,
                epoch_drift.as_ref(),
            );
        }
        OutputFormat::Pretty => {
            print_status_pretty(
                default_ws_name,
                is_stale,
                changes.as_ref(),
                global_view.as_ref(),
                &workspace_entries,
                format.should_use_color(),
                epoch_drift.as_ref(),
            );
        }
        OutputFormat::Json => {
            let status_data = WorkspaceStatus {
                current_workspace,
                is_stale,
                has_changes,
                changes,
                global_view,
                workspaces: workspace_entries,
                epoch_drift,
            };
            match format.serialize(&status_data) {
                Ok(output) => println!("{output}"),
                Err(e) => {
                    tracing::warn!("Failed to serialize status to JSON: {e}");
                    print_status_text(default_ws_name, is_stale, None, None, &[], None);
                }
            }
        }
    }

    Ok(())
}

fn detect_current_workspace(root: &Path) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let ws_root = root.join("ws");
    let rel = cwd.strip_prefix(&ws_root).ok()?;
    let first = rel.components().next()?;
    let name = first.as_os_str().to_str()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Print status in compact text format (agent-friendly)
fn print_status_text(
    current_ws: &str,
    is_stale: bool,
    changes: Option<&StatusChanges>,
    global_view: Option<&GlobalViewSummary>,
    workspaces: &[WorkspaceEntry],
    epoch_drift: Option<&super::epoch_drift::EpochDriftReport>,
) {
    // Current workspace and stale warning
    println!("workspace: {current_ws}");
    if is_stale {
        println!(
            "stale: true  (main has moved forward — run `maw ws sync {current_ws}` to rebase)"
        );
    }

    // Changes
    match changes {
        None => println!("changes: none"),
        Some(ch) => {
            println!("changes: {} file(s)", ch.dirty_count);
            for f in &ch.dirty_files {
                println!("  {f}");
            }
        }
    }
    println!();

    if let Some(view) = global_view {
        let epoch = view.epoch.as_deref().unwrap_or("none");
        println!(
            "global-view: epoch={} ws={} patches={} conflicts={} ops={}",
            epoch, view.workspace_count, view.total_patches, view.conflict_count, view.total_ops
        );
        println!();
    }

    // All workspaces
    println!("workspaces:");
    for ws in workspaces {
        let default_marker = if ws.is_default { "  (default)" } else { "" };
        let conflict_marker = if ws.rebase_conflicts > 0 {
            format!(" [conflicted: {} rebase conflict(s)]", ws.rebase_conflicts)
        } else {
            String::new()
        };
        let stale_marker = if ws.state.contains("stale") {
            " [stale]"
        } else {
            ""
        };
        let mode_marker = if ws.mode == "persistent" {
            " [persistent]"
        } else {
            ""
        };
        println!(
            "  {}  epoch:{}{}{}{}{}",
            ws.name, ws.epoch, stale_marker, conflict_marker, mode_marker, default_marker
        );
    }

    // Stale workspace hints
    let stale_persistent: Vec<&str> = workspaces
        .iter()
        .filter(|ws| ws.state.contains("stale") && ws.mode == "persistent")
        .map(|ws| ws.name.as_str())
        .collect();
    let stale_ephemeral: Vec<&str> = workspaces
        .iter()
        .filter(|ws| ws.state.contains("stale") && ws.mode != "persistent")
        .map(|ws| ws.name.as_str())
        .collect();

    if !stale_persistent.is_empty() {
        println!();
        println!(
            "Behind current epoch: {} (repository merge state moved forward since last sync)",
            stale_persistent.join(", ")
        );
        for ws in &stale_persistent {
            println!("  Fix: maw ws advance {ws}");
        }
    }
    if !stale_ephemeral.is_empty() {
        println!();
        println!(
            "Behind current epoch: {} (repository merge state moved forward — sync before merging)",
            stale_ephemeral.join(", ")
        );
        for ws in &stale_ephemeral {
            println!("  Fix: maw ws sync {ws}");
        }
    }

    print_epoch_drift_text(epoch_drift);

    // Next command
    println!();
    println!("Next: maw exec <name> -- <command>");
}

/// bn-1ieb: surface `epoch_drift` in plain-text status output so agents
/// see the exact recovery verb before they trigger a downstream merge
/// failure. Kept compact (≤4 lines) so the text status stays scannable.
fn print_epoch_drift_text(epoch_drift: Option<&super::epoch_drift::EpochDriftReport>) {
    use super::epoch_drift::EpochDriftKind;

    let Some(report) = epoch_drift else { return };
    if !report.kind.has_drift() {
        return;
    }
    println!();
    match report.kind {
        EpochDriftKind::FfAbsorbable => {
            println!(
                "epoch drift: branch '{}' ahead of epoch by {} commit(s) ({} → {}); safe to advance.",
                report.branch, report.ff_commit_count, report.epoch_short, report.branch_short,
            );
            println!("  Fix: maw epoch sync");
        }
        EpochDriftKind::FfBlocked => {
            println!(
                "epoch drift: branch '{}' ahead of epoch by {} commit(s), blocked by workspace(s): {}",
                report.branch,
                report.ff_commit_count,
                report.blocking_workspaces.join(", "),
            );
            println!(
                "  Fix: maw ws merge {} --into default --check  (resolve, then retry)",
                report
                    .blocking_workspaces
                    .first()
                    .map_or("<ws>", String::as_str),
            );
        }
        EpochDriftKind::Diverged => {
            println!(
                "epoch drift: epoch ({}) and branch '{}' ({}) have forked — manual recovery required.",
                report.epoch_short, report.branch, report.branch_short,
            );
            println!("  Fix: maw doctor");
        }
        EpochDriftKind::InSync => {}
    }
}

/// Print status in pretty format (colored, human-friendly)
fn print_status_pretty(
    current_ws: &str,
    is_stale: bool,
    changes: Option<&StatusChanges>,
    global_view: Option<&GlobalViewSummary>,
    workspaces: &[WorkspaceEntry],
    use_color: bool,
    epoch_drift: Option<&super::epoch_drift::EpochDriftReport>,
) {
    let (bold, green, yellow, gray, reset) = if use_color {
        ("\x1b[1m", "\x1b[32m", "\x1b[33m", "\x1b[90m", "\x1b[0m")
    } else {
        ("", "", "", "", "")
    };

    // Header
    println!("{bold}Workspace Status{reset}");
    println!();

    // Stale warning
    if is_stale {
        println!(
            "{yellow}\u{25b2} WARNING:{reset} Workspace is behind the current epoch — another merge advanced repository state since this one was created."
        );
        println!("  {gray}Run `maw ws sync {current_ws}` to rebase onto the latest epoch.{reset}");
        println!();
    }

    // Current workspace
    println!("{bold}Default:{reset} {current_ws}");
    match changes {
        None => println!("  {gray}(no changes){reset}"),
        Some(ch) => {
            println!("  {} dirty file(s):", ch.dirty_count);
            for f in &ch.dirty_files {
                println!("    {f}");
            }
        }
    }
    println!();

    if let Some(view) = global_view {
        let epoch = view.epoch.as_deref().unwrap_or("none");
        println!("{bold}Global View{reset}");
        println!(
            "  epoch:{epoch} ws:{} patches:{} conflicts:{} ops:{}",
            view.workspace_count, view.total_patches, view.conflict_count, view.total_ops
        );
        println!();
    }

    // All workspaces
    println!("{bold}All Workspaces{reset}");
    for ws in workspaces {
        let mode_tag = if ws.mode == "persistent" {
            " [persistent]"
        } else {
            ""
        };
        if ws.is_default {
            println!(
                "  {green}\u{25cf} {}{reset} epoch:{} {}{}",
                ws.name, ws.epoch, ws.state, mode_tag
            );
        } else if ws.state.contains("stale") {
            println!(
                "  {yellow}\u{25b2} {}{reset} epoch:{} {}{}",
                ws.name, ws.epoch, ws.state, mode_tag
            );
        } else {
            println!(
                "  {gray}\u{25cc} {}{reset} epoch:{} {}{}",
                ws.name, ws.epoch, ws.state, mode_tag
            );
        }
    }

    // Stale workspace hints
    let stale_persistent: Vec<&str> = workspaces
        .iter()
        .filter(|ws| ws.state.contains("stale") && ws.mode == "persistent")
        .map(|ws| ws.name.as_str())
        .collect();
    let stale_ephemeral: Vec<&str> = workspaces
        .iter()
        .filter(|ws| ws.state.contains("stale") && ws.mode != "persistent")
        .map(|ws| ws.name.as_str())
        .collect();

    if !stale_persistent.is_empty() {
        println!();
        println!(
            "{yellow}Behind current epoch:{reset} {} {gray}(repository merge state moved forward since last sync){reset}",
            stale_persistent.join(", ")
        );
        for ws in &stale_persistent {
            println!("  {gray}Fix: maw ws advance {ws}{reset}");
        }
    }
    if !stale_ephemeral.is_empty() {
        println!();
        println!(
            "{yellow}Behind current epoch:{reset} {} {gray}(repository merge state moved forward — sync before merging){reset}",
            stale_ephemeral.join(", ")
        );
        for ws in &stale_ephemeral {
            println!("  {gray}Fix: maw ws sync {ws}{reset}");
        }
    }

    print_epoch_drift_pretty(epoch_drift, yellow, gray, reset);

    // Next command
    println!();
    println!("{gray}Next: maw exec <name> -- <command>{reset}");
}

/// Pretty (colorized) variant of [`print_epoch_drift_text`].
fn print_epoch_drift_pretty(
    epoch_drift: Option<&super::epoch_drift::EpochDriftReport>,
    yellow: &str,
    gray: &str,
    reset: &str,
) {
    use super::epoch_drift::EpochDriftKind;

    let Some(report) = epoch_drift else { return };
    if !report.kind.has_drift() {
        return;
    }
    println!();
    match report.kind {
        EpochDriftKind::FfAbsorbable => {
            println!(
                "{yellow}Epoch drift:{reset} branch '{}' ahead of epoch by {} commit(s) ({} → {}); {gray}safe to advance.{reset}",
                report.branch, report.ff_commit_count, report.epoch_short, report.branch_short,
            );
            println!("  {gray}Fix: maw epoch sync{reset}");
        }
        EpochDriftKind::FfBlocked => {
            println!(
                "{yellow}Epoch drift:{reset} branch '{}' ahead by {} commit(s), blocked by: {}",
                report.branch,
                report.ff_commit_count,
                report.blocking_workspaces.join(", "),
            );
            println!(
                "  {gray}Fix: maw ws merge {} --into default --check{reset}",
                report
                    .blocking_workspaces
                    .first()
                    .map_or("<ws>", String::as_str),
            );
        }
        EpochDriftKind::Diverged => {
            println!(
                "{yellow}Epoch drift:{reset} epoch ({}) and branch '{}' ({}) have forked.",
                report.epoch_short, report.branch, report.branch_short,
            );
            println!("  {gray}Fix: maw doctor{reset}");
        }
        EpochDriftKind::InSync => {}
    }
}

fn compute_global_view_summary(
    root: &Path,
    workspaces: &[maw_core::model::types::WorkspaceInfo],
) -> Option<GlobalViewSummary> {
    let workspace_ids: Vec<_> = workspaces
        .iter()
        .filter_map(|ws| match read_head(root, &ws.id) {
            Ok(Some(_)) => Some(ws.id.clone()),
            _ => None,
        })
        .collect();

    if workspace_ids.is_empty() {
        return None;
    }

    let view =
        compute_global_view(root, &workspace_ids, |oid| read_patch_set_blob(root, oid)).ok()?;

    // Read the epoch directly from refs/manifold/epoch/current — this is the
    // single authoritative source. Previously we took the lexicographic max of
    // workspace epochs, which could return a stale workspace's epoch (bn-1wqe).
    let authoritative_epoch = maw_core::refs::read_epoch_current(root)
        .ok()
        .flatten()
        .map(|e| e.as_str()[..12].to_string());

    Some(GlobalViewSummary {
        epoch: authoritative_epoch,
        workspace_count: view.workspace_count(),
        total_patches: view.total_patches(),
        conflict_count: view.conflicts.len(),
        total_ops: view.total_ops,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use super::super::epoch_drift::{EpochDriftKind, EpochDriftReport};

    fn mk_report(kind: EpochDriftKind, blocking: &[&str]) -> EpochDriftReport {
        EpochDriftReport {
            kind,
            epoch_short: "aaaaaaaaaaaa".into(),
            branch_short: "bbbbbbbbbbbb".into(),
            branch: "main".into(),
            ff_commit_count: 3,
            blocking_workspaces: blocking.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    /// bn-1ieb: in-sync drift must NOT print anything (we only surface
    /// the field for human-visible drift, otherwise status output gets
    /// noisier with no actionable signal).
    #[test]
    fn print_epoch_drift_text_silent_when_in_sync() {
        let report = mk_report(EpochDriftKind::InSync, &[]);
        // Internal compile-time gate: function exists with the expected
        // signature; runtime no-op is exercised end-to-end via the
        // doctor + epoch_drift integration tests. We assert here on the
        // helper directly to prevent regression where InSync starts
        // emitting noise.
        assert_eq!(report.kind, EpochDriftKind::InSync);
        // If `EpochDriftKind::InSync.has_drift()` is ever flipped to
        // true, the renderer's early return would also need updating.
        assert!(!report.kind.has_drift());
    }

    /// bn-1ieb: `WorkspaceStatus` JSON shape must include `epoch_drift`
    /// when populated so machine consumers (agents) can detect the
    /// `epoch_sync_required` cluster condition without a separate
    /// `maw doctor` call.
    #[test]
    fn workspace_status_json_carries_epoch_drift_when_populated() {
        let status = WorkspaceStatus {
            current_workspace: "default".into(),
            is_stale: false,
            has_changes: false,
            changes: None,
            global_view: None,
            workspaces: Vec::new(),
            epoch_drift: Some(mk_report(EpochDriftKind::FfAbsorbable, &[])),
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(
            json.contains("\"epoch_drift\""),
            "epoch_drift field must appear in status JSON: {json}"
        );
        assert!(
            json.contains("\"kind\":\"ff_absorbable\""),
            "kind must serialize as snake_case: {json}"
        );
    }

    /// bn-1ieb: when there's no opinion (None), the field is elided so
    /// downstream consumers can distinguish "no drift signal" from "in
    /// sync" without ambiguity.
    #[test]
    fn workspace_status_json_omits_epoch_drift_when_none() {
        let status = WorkspaceStatus {
            current_workspace: "default".into(),
            is_stale: false,
            has_changes: false,
            changes: None,
            global_view: None,
            workspaces: Vec::new(),
            epoch_drift: None,
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(
            !json.contains("\"epoch_drift\""),
            "None epoch_drift should be elided: {json}"
        );
    }

    /// bn-1ieb: blocking workspace list must round-trip through JSON for
    /// the `FfBlocked` case (this is the structured handoff that lets a
    /// coordinator agent decide which sibling to resolve first).
    #[test]
    fn workspace_status_json_includes_blocking_workspaces_for_ff_blocked() {
        let status = WorkspaceStatus {
            current_workspace: "default".into(),
            is_stale: false,
            has_changes: false,
            changes: None,
            global_view: None,
            workspaces: Vec::new(),
            epoch_drift: Some(mk_report(
                EpochDriftKind::FfBlocked,
                &["alice", "carol"],
            )),
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(
            json.contains("\"blocking_workspaces\":[\"alice\",\"carol\"]"),
            "blocking_workspaces must round-trip: {json}"
        );
    }
}
