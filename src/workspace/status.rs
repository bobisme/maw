use anyhow::Result;
use serde::Serialize;

use crate::backend::WorkspaceBackend;
use crate::format::OutputFormat;

use super::{get_backend, DEFAULT_WORKSPACE};

#[derive(Serialize)]
pub struct WorkspaceStatus {
    pub(crate) current_workspace: String,
    pub(crate) is_stale: bool,
    pub(crate) has_changes: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) changes: Option<StatusChanges>,
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
        let ws_status = backend.status(&default_ws_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let dirty_files: Vec<String> = ws_status.dirty_files
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
        (ws_status.is_stale, has, changes)
    } else {
        (false, false, None)
    };

    // Build workspace entries
    let workspace_entries: Vec<WorkspaceEntry> = all_workspaces
        .iter()
        .map(|ws| WorkspaceEntry {
            name: ws.id.as_str().to_string(),
            is_default: ws.id.as_str() == default_ws_name,
            epoch: ws.epoch.as_str()[..12].to_string(),
            state: format!("{}", ws.state),
        })
        .collect();

    match format {
        OutputFormat::Text => {
            print_status_text(
                default_ws_name,
                is_stale,
                &changes,
                &workspace_entries,
            );
        }
        OutputFormat::Pretty => {
            print_status_pretty(
                default_ws_name,
                is_stale,
                &changes,
                &workspace_entries,
                format.should_use_color(),
            );
        }
        OutputFormat::Json => {
            let status_data = WorkspaceStatus {
                current_workspace: default_ws_name.to_string(),
                is_stale,
                has_changes,
                changes,
                workspaces: workspace_entries,
            };
            match format.serialize(&status_data) {
                Ok(output) => println!("{output}"),
                Err(e) => {
                    eprintln!("Warning: Failed to serialize status to JSON: {e}");
                    print_status_text(default_ws_name, is_stale, &None, &[]);
                }
            }
        }
    }

    Ok(())
}

/// Print status in compact text format (agent-friendly)
fn print_status_text(
    current_ws: &str,
    is_stale: bool,
    changes: &Option<StatusChanges>,
    workspaces: &[WorkspaceEntry],
) {
    // Current workspace and stale warning
    println!("workspace: {current_ws}");
    if is_stale {
        println!("stale: true");
        println!("  Fix: maw ws sync");
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

    // All workspaces
    println!("workspaces:");
    for ws in workspaces {
        let default_marker = if ws.is_default { "  (default)" } else { "" };
        let stale_marker = if ws.state.contains("stale") { " [stale]" } else { "" };
        println!("  {}  epoch:{}{}{}", ws.name, ws.epoch, stale_marker, default_marker);
    }

    // Next command
    println!();
    println!("Next: maw exec <name> -- <command>");
}

/// Print status in pretty format (colored, human-friendly)
fn print_status_pretty(
    current_ws: &str,
    is_stale: bool,
    changes: &Option<StatusChanges>,
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
        println!("{yellow}\u{25b2} WARNING:{reset} Workspace is stale (behind current epoch)");
        println!("  {gray}Fix: maw ws sync{reset}");
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

    // All workspaces
    println!("{bold}All Workspaces{reset}");
    for ws in workspaces {
        if ws.is_default {
            println!("  {green}\u{25cf} {}{reset} epoch:{} {}", ws.name, ws.epoch, ws.state);
        } else if ws.state.contains("stale") {
            println!("  {yellow}\u{25b2} {}{reset} epoch:{} {}", ws.name, ws.epoch, ws.state);
        } else {
            println!("  {gray}\u{25cc} {}{reset} epoch:{} {}", ws.name, ws.epoch, ws.state);
        }
    }

    // Next command
    println!();
    println!("{gray}Next: maw exec <name> -- <command>{reset}");
}


