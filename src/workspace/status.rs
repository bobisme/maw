use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::backend::WorkspaceBackend;
use crate::format::OutputFormat;
use crate::model::types::WorkspaceMode;
use crate::oplog::global_view::compute_global_view;
use crate::oplog::read::read_head;
use crate::oplog::view::read_patch_set_blob;

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

pub fn status(format: OutputFormat) -> Result<()> {
    let backend = get_backend()?;

    // Get all workspaces
    let all_workspaces = backend.list().map_err(|e| anyhow::anyhow!("{e}"))?;

    // Find the default workspace for the main status display
    let default_ws_name = DEFAULT_WORKSPACE;

    // Get default workspace status
    let default_ws_id = crate::model::types::WorkspaceId::new(default_ws_name)
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

    // Build workspace entries
    let workspace_entries: Vec<WorkspaceEntry> = all_workspaces
        .iter()
        .map(|ws| {
            let is_default = ws.id.as_str() == default_ws_name;
            let ws_mode = if is_default {
                WorkspaceMode::Persistent
            } else {
                metadata::read(&root, ws.id.as_str())
                    .map(|m| m.mode)
                    .unwrap_or(WorkspaceMode::Ephemeral)
            };
            WorkspaceEntry {
                name: ws.id.as_str().to_string(),
                is_default,
                epoch: ws.epoch.as_str()[..12].to_string(),
                state: if is_default {
                    "active".to_owned()
                } else {
                    format!("{}", ws.state)
                },
                mode: format!("{ws_mode}"),
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
            };
            match format.serialize(&status_data) {
                Ok(output) => println!("{output}"),
                Err(e) => {
                    tracing::warn!("Failed to serialize status to JSON: {e}");
                    print_status_text(default_ws_name, is_stale, None, None, &[]);
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
) {
    // Current workspace and stale warning
    println!("workspace: {current_ws}");
    if is_stale {
        println!("stale: true  (main has moved forward — run `maw ws sync {current_ws}` to rebase)");
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
            "  {}  epoch:{}{}{}{}",
            ws.name, ws.epoch, stale_marker, mode_marker, default_marker
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
            "Behind main: {} (main moved forward since last sync)",
            stale_persistent.join(", ")
        );
        for ws in &stale_persistent {
            println!("  Fix: maw ws advance {ws}");
        }
    }
    if !stale_ephemeral.is_empty() {
        println!();
        println!(
            "Behind main: {} (main moved forward — rebase before merging)",
            stale_ephemeral.join(", ")
        );
        for ws in &stale_ephemeral {
            println!("  Fix: maw ws sync {ws}");
        }
    }

    // Next command
    println!();
    println!("Next: maw exec <name> -- <command>");
}

/// Print status in pretty format (colored, human-friendly)
fn print_status_pretty(
    current_ws: &str,
    is_stale: bool,
    changes: Option<&StatusChanges>,
    global_view: Option<&GlobalViewSummary>,
    workspaces: &[WorkspaceEntry],
    use_color: bool,
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
        println!("{yellow}\u{25b2} WARNING:{reset} Workspace is behind main — another workspace was merged since this one was created.");
        println!("  {gray}Run `maw ws sync {current_ws}` to rebase onto the latest main.{reset}");
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
            "{yellow}Behind main:{reset} {} {gray}(main moved forward since last sync){reset}",
            stale_persistent.join(", ")
        );
        for ws in &stale_persistent {
            println!("  {gray}Fix: maw ws advance {ws}{reset}");
        }
    }
    if !stale_ephemeral.is_empty() {
        println!();
        println!(
            "{yellow}Behind main:{reset} {} {gray}(main moved forward — rebase before merging){reset}",
            stale_ephemeral.join(", ")
        );
        for ws in &stale_ephemeral {
            println!("  {gray}Fix: maw ws sync {ws}{reset}");
        }
    }

    // Next command
    println!();
    println!("{gray}Next: maw exec <name> -- <command>{reset}");
}

fn compute_global_view_summary(
    root: &Path,
    workspaces: &[crate::model::types::WorkspaceInfo],
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

    // Use the max epoch from the workspace list (backend.list()), not from
    // the oplog-materialized view. The backend reads from
    // refs/manifold/epoch/current which is always authoritative, while the
    // oplog may lag behind after epoch transitions (bn-22fi).
    let authoritative_epoch = workspaces
        .iter()
        .map(|ws| &ws.epoch)
        .max_by(|a, b| a.as_str().cmp(b.as_str()))
        .map(|e| e.as_str()[..12].to_string());

    Some(GlobalViewSummary {
        epoch: authoritative_epoch,
        workspace_count: view.workspace_count(),
        total_patches: view.total_patches(),
        conflict_count: view.conflicts.len(),
        total_ops: view.total_ops,
    })
}
