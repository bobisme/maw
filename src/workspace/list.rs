use anyhow::Result;
use serde::Serialize;

use crate::backend::WorkspaceBackend;
use crate::format::OutputFormat;
use crate::model::types::WorkspaceState;

use crate::merge::quarantine::QUARANTINE_NAME_PREFIX;

use super::{get_backend, DEFAULT_WORKSPACE};

#[derive(Serialize)]
pub struct WorkspaceInfo {
    pub(crate) name: String,
    pub(crate) is_default: bool,
    pub(crate) epoch: String,
    pub(crate) state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) behind_epochs: Option<u32>,
}

/// Envelope for `maw ws list --format json` output.
#[derive(Serialize)]
pub struct WorkspaceListEnvelope {
    pub(crate) workspaces: Vec<WorkspaceInfo>,
    pub(crate) advice: Vec<Advice>,
}

/// A single advisory message (warning, info) embedded in structured output.
#[derive(Serialize)]
pub struct Advice {
    pub(crate) level: &'static str,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) details: Option<AdviceDetails>,
}

/// Extra details for an advice entry.
#[derive(Serialize)]
pub struct AdviceDetails {
    pub(crate) workspaces: Vec<String>,
    pub(crate) fix: String,
}

pub fn list(verbose: bool, format: OutputFormat) -> Result<()> {
    let backend = get_backend()?;
    let backend_workspaces = backend.list().map_err(|e| anyhow::anyhow!("{e}"))?;

    if backend_workspaces.is_empty() {
        match format {
            OutputFormat::Text | OutputFormat::Pretty => println!("No workspaces found."),
            OutputFormat::Json => {
                let envelope = WorkspaceListEnvelope {
                    workspaces: vec![],
                    advice: vec![],
                };
                println!("{}", format.serialize(&envelope)?);
            }
        }
        return Ok(());
    }

    // Convert backend workspace info to display structs
    let workspaces: Vec<WorkspaceInfo> = backend_workspaces
        .iter()
        .map(|ws| {
            let name = ws.id.as_str().to_string();
            let is_quarantine = name.starts_with(QUARANTINE_NAME_PREFIX);
            let behind = match &ws.state {
                WorkspaceState::Stale { behind_epochs } => Some(*behind_epochs),
                _ => None,
            };
            WorkspaceInfo {
                is_default: name == DEFAULT_WORKSPACE,
                epoch: ws.epoch.as_str()[..12].to_string(),
                // Quarantine workspaces show as "quarantine" regardless of
                // their staleness state — they are a special class of workspace.
                state: if is_quarantine {
                    "quarantine".to_owned()
                } else {
                    format!("{}", ws.state)
                },
                path: if verbose {
                    Some(ws.path.display().to_string())
                } else {
                    None
                },
                behind_epochs: behind,
                name,
            }
        })
        .collect();

    // Collect stale workspace warnings (exclude quarantine workspaces)
    let stale_workspaces: Vec<String> = backend_workspaces
        .iter()
        .filter(|ws| ws.state.is_stale() && !ws.id.as_str().starts_with(QUARANTINE_NAME_PREFIX))
        .map(|ws| ws.id.as_str().to_string())
        .collect();

    match format {
        OutputFormat::Text => print_list_text(&workspaces, &stale_workspaces, verbose),
        OutputFormat::Pretty => print_list_pretty(&workspaces, &stale_workspaces, format, verbose),
        OutputFormat::Json => print_list_json(workspaces, stale_workspaces, format),
    }

    Ok(())
}

/// Print workspace list in tab-separated text format (agent-friendly).
fn print_list_text(workspaces: &[WorkspaceInfo], stale: &[String], verbose: bool) {
    if verbose {
        println!("NAME\tEPOCH\tSTATE\tDEFAULT\tPATH");
    } else {
        println!("NAME\tEPOCH\tSTATE\tDEFAULT");
    }
    for ws in workspaces {
        let default_marker = if ws.is_default { "true" } else { "false" };
        if verbose {
            let path = ws.path.as_deref().unwrap_or("");
            println!(
                "{}\t{}\t{}\t{}\t{}",
                ws.name, ws.epoch, ws.state, default_marker, path
            );
        } else {
            println!(
                "{}\t{}\t{}\t{}",
                ws.name, ws.epoch, ws.state, default_marker
            );
        }
    }

    print_stale_warning_text(stale);

    println!();
    println!("Next: maw exec <name> -- <command>");
}

/// Print workspace list in colored, human-friendly format.
fn print_list_pretty(
    workspaces: &[WorkspaceInfo],
    stale: &[String],
    format: OutputFormat,
    verbose: bool,
) {
    let use_color = format.should_use_color();

    for ws in workspaces {
        let (glyph, name_style, reset) = if use_color {
            if ws.is_default {
                ("\u{25cf}", "\x1b[1;32m", "\x1b[0m")  // Green bold for default
            } else if ws.state == "quarantine" {
                ("\u{26a0}", "\x1b[1;31m", "\x1b[0m")  // Red bold for quarantine
            } else if ws.state.contains("stale") {
                ("\u{25b2}", "\x1b[1;33m", "\x1b[0m")  // Yellow for stale
            } else {
                ("\u{25cc}", "\x1b[90m", "\x1b[0m")    // Gray for others
            }
        } else if ws.is_default {
            ("\u{25cf}", "", "")
        } else if ws.state == "quarantine" {
            ("\u{26a0}", "", "")
        } else {
            ("\u{25cc}", "", "")
        };

        println!(
            "{} {}{}{} {} {}",
            glyph, name_style, ws.name, reset, ws.epoch, ws.state
        );

        if verbose {
            if let Some(path) = &ws.path {
                println!("    path: {path}");
            }
            if ws.is_default {
                println!("    default workspace");
            }
        }
    }

    if !stale.is_empty() {
        println!();
        if use_color {
            println!("\x1b[1;33m\u{25b2} WARNING:\x1b[0m {} stale workspace(s): {}",
                stale.len(),
                stale.join(", ")
            );
        } else {
            println!("\u{25b2} WARNING: {} stale workspace(s): {}",
                stale.len(),
                stale.join(", ")
            );
        }
        println!("  Fix: maw ws sync --all");
    }

    if !workspaces.is_empty() {
        println!();
        if use_color {
            println!("\x1b[90mNext: maw exec <name> -- <command>\x1b[0m");
        } else {
            println!("Next: maw exec <name> -- <command>");
        }
    }
}

/// Print workspace list as JSON with stale-workspace advice.
fn print_list_json(
    workspaces: Vec<WorkspaceInfo>,
    stale_workspaces: Vec<String>,
    format: OutputFormat,
) {
    let advice = if stale_workspaces.is_empty() {
        vec![]
    } else {
        vec![Advice {
            level: "warn",
            message: format!("{} workspace(s) stale: {}", stale_workspaces.len(), stale_workspaces.join(", ")),
            details: Some(AdviceDetails {
                workspaces: stale_workspaces,
                fix: "maw ws sync --all".to_string(),
            }),
        }]
    };

    let envelope = WorkspaceListEnvelope {
        workspaces,
        advice,
    };

    match format.serialize(&envelope) {
        Ok(output) => println!("{output}"),
        Err(e) => {
            eprintln!("Warning: Failed to serialize to JSON: {e}");
        }
    }
}

/// Print stale workspace warnings for text output mode.
fn print_stale_warning_text(stale: &[String]) {
    if !stale.is_empty() {
        println!();
        println!("WARNING: {} stale workspace(s): {}",
            stale.len(),
            stale.join(", ")
        );
        println!("  Fix: maw ws sync --all");
    }
}


