use anyhow::Result;
use serde::Serialize;

use crate::backend::WorkspaceBackend;
use crate::format::OutputFormat;
use crate::model::types::WorkspaceState;
use crate::workspace::templates::TemplateDefaults;

use crate::merge::quarantine::QUARANTINE_NAME_PREFIX;

use super::{DEFAULT_WORKSPACE, get_backend, metadata, repo_root};

#[derive(Serialize)]
pub struct WorkspaceInfo {
    pub(crate) name: String,
    pub(crate) is_default: bool,
    pub(crate) epoch: String,
    pub(crate) state: String,
    pub(crate) mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) behind_epochs: Option<u32>,
    /// Commits in the workspace HEAD that haven't been merged into the epoch yet.
    /// Non-zero means "this workspace has work to merge".
    #[serde(skip_serializing_if = "is_zero")]
    pub(crate) commits_ahead: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) template_defaults: Option<TemplateDefaults>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
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

    // Read metadata for all workspaces to get mode (ephemeral/persistent).
    let root = repo_root()?;

    // Convert backend workspace info to display structs
    let workspaces: Vec<WorkspaceInfo> = backend_workspaces
        .iter()
        .map(|ws| {
            let name = ws.id.as_str().to_string();
            let is_default = name == DEFAULT_WORKSPACE;
            let is_quarantine = name.starts_with(QUARANTINE_NAME_PREFIX);
            let behind = match &ws.state {
                WorkspaceState::Stale { behind_epochs } if !is_default => Some(*behind_epochs),
                _ => None,
            };
            // Read metadata for this workspace (defaults to ephemeral on error/missing).
            let ws_meta = metadata::read(&root, ws.id.as_str()).unwrap_or_default();
            let ws_mode = if is_default {
                crate::model::types::WorkspaceMode::Persistent
            } else {
                ws_meta.mode
            };
            WorkspaceInfo {
                is_default,
                epoch: ws.epoch.as_str()[..12].to_string(),
                // Quarantine workspaces show as "quarantine" regardless of
                // their staleness state — they are a special class of workspace.
                state: if is_quarantine {
                    "quarantine".to_owned()
                } else if is_default {
                    "active".to_owned()
                } else if ws.commits_ahead > 0 {
                    format!("active (+{} to merge)", ws.commits_ahead)
                } else {
                    format!("{}", ws.state)
                },
                mode: format!("{ws_mode}"),
                path: if verbose {
                    Some(ws.path.display().to_string())
                } else {
                    None
                },
                behind_epochs: behind,
                commits_ahead: ws.commits_ahead,
                template: ws_meta.template.map(|t| t.to_string()),
                template_defaults: ws_meta.template_defaults,
                name,
            }
        })
        .collect();

    // Collect stale workspace warnings, split by mode (exclude quarantine workspaces).
    let stale_persistent: Vec<String> = backend_workspaces
        .iter()
        .filter(|ws| {
            ws.state.is_stale()
                && !ws.id.as_str().starts_with(QUARANTINE_NAME_PREFIX)
                && ws.id.as_str() != DEFAULT_WORKSPACE
        })
        .filter(|ws| {
            metadata::read(&root, ws.id.as_str())
                .map(|m| m.mode.is_persistent())
                .unwrap_or(false)
        })
        .map(|ws| ws.id.as_str().to_string())
        .collect();

    let stale_ephemeral: Vec<String> = backend_workspaces
        .iter()
        .filter(|ws| {
            ws.state.is_stale()
                && !ws.id.as_str().starts_with(QUARANTINE_NAME_PREFIX)
                && ws.id.as_str() != DEFAULT_WORKSPACE
        })
        .filter(|ws| {
            metadata::read(&root, ws.id.as_str())
                .map(|m| m.mode.is_ephemeral())
                .unwrap_or(true)
        })
        .map(|ws| ws.id.as_str().to_string())
        .collect();

    // Combined stale list (exclude quarantine, for backwards compatibility)
    let stale_workspaces: Vec<String> = backend_workspaces
        .iter()
        .filter(|ws| {
            ws.state.is_stale()
                && !ws.id.as_str().starts_with(QUARANTINE_NAME_PREFIX)
                && ws.id.as_str() != DEFAULT_WORKSPACE
        })
        .map(|ws| ws.id.as_str().to_string())
        .collect();

    match format {
        OutputFormat::Text => print_list_text(
            &workspaces,
            &stale_workspaces,
            &stale_persistent,
            &stale_ephemeral,
            verbose,
        ),
        OutputFormat::Pretty => print_list_pretty(
            &workspaces,
            &stale_workspaces,
            &stale_persistent,
            &stale_ephemeral,
            format,
            verbose,
        ),
        OutputFormat::Json => print_list_json(
            workspaces,
            stale_workspaces,
            stale_persistent,
            stale_ephemeral,
            format,
        ),
    }

    Ok(())
}

/// Print workspace list in tab-separated text format (agent-friendly).
fn print_list_text(
    workspaces: &[WorkspaceInfo],
    stale: &[String],
    stale_persistent: &[String],
    stale_ephemeral: &[String],
    verbose: bool,
) {
    if verbose {
        println!("NAME\tEPOCH\tSTATE\tMODE\tDEFAULT\tPATH");
    } else {
        println!("NAME\tEPOCH\tSTATE\tMODE\tDEFAULT");
    }
    for ws in workspaces {
        let default_marker = if ws.is_default { "true" } else { "false" };
        if verbose {
            let path = ws.path.as_deref().unwrap_or("");
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                ws.name, ws.epoch, ws.state, ws.mode, default_marker, path
            );
        } else {
            println!(
                "{}\t{}\t{}\t{}\t{}",
                ws.name, ws.epoch, ws.state, ws.mode, default_marker
            );
        }
    }

    print_stale_warning_text(stale, stale_persistent, stale_ephemeral);

    // Surface actionable merge hints for workspaces with committed work.
    let mergeable: Vec<&str> = workspaces
        .iter()
        .filter(|ws| ws.commits_ahead > 0)
        .map(|ws| ws.name.as_str())
        .collect();
    if !mergeable.is_empty() {
        println!();
        for name in &mergeable {
            println!("Merge ready: maw ws merge {name} --destroy");
        }
    }

    println!();
    println!("Next: maw exec <name> -- <command>");
}

/// Print workspace list in colored, human-friendly format.
fn print_list_pretty(
    workspaces: &[WorkspaceInfo],
    stale: &[String],
    stale_persistent: &[String],
    stale_ephemeral: &[String],
    format: OutputFormat,
    verbose: bool,
) {
    let use_color = format.should_use_color();

    for ws in workspaces {
        let is_stale = ws.state.contains("stale");
        let is_persistent = ws.mode == "persistent";
        let has_work = ws.commits_ahead > 0;
        let (glyph, name_style, reset) = if use_color {
            if ws.is_default {
                ("\u{25cf}", "\x1b[1;32m", "\x1b[0m") // Green bold for default
            } else if ws.state == "quarantine" {
                ("\u{26a0}", "\x1b[1;31m", "\x1b[0m") // Red bold for quarantine
            } else if is_stale {
                ("\u{25b2}", "\x1b[1;33m", "\x1b[0m") // Yellow for stale
            } else if has_work {
                ("\u{25b6}", "\x1b[1;36m", "\x1b[0m") // Cyan for ready-to-merge
            } else {
                ("\u{25cc}", "\x1b[90m", "\x1b[0m") // Gray for idle
            }
        } else if ws.is_default {
            ("\u{25cf}", "", "")
        } else if ws.state == "quarantine" {
            ("\u{26a0}", "", "")
        } else if has_work {
            ("\u{25b6}", "", "")
        } else {
            ("\u{25cc}", "", "")
        };

        let mode_tag = if is_persistent { " [persistent]" } else { "" };
        println!(
            "{} {}{}{} {} {}{}",
            glyph, name_style, ws.name, reset, ws.epoch, ws.state, mode_tag
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

    // Stale warnings with mode-specific guidance.
    if !stale_persistent.is_empty() {
        println!();
        if use_color {
            println!(
                "\x1b[1;33m\u{25b2} STALE persistent workspace(s):\x1b[0m {}",
                stale_persistent.join(", ")
            );
        } else {
            println!(
                "\u{25b2} STALE persistent workspace(s): {}",
                stale_persistent.join(", ")
            );
        }
        for ws in stale_persistent {
            println!("  Fix: maw ws advance {ws}");
        }
    }
    if !stale_ephemeral.is_empty() {
        println!();
        if use_color {
            println!(
                "\x1b[1;33m\u{25b2} WARNING: stale ephemeral workspace(s):\x1b[0m {}",
                stale_ephemeral.join(", ")
            );
        } else {
            println!(
                "\u{25b2} WARNING: stale ephemeral workspace(s): {}",
                stale_ephemeral.join(", ")
            );
        }
        println!(
            "  Ephemeral workspaces should be merged or destroyed — they survived an epoch advance."
        );
        println!(
            "  Fix: maw ws sync --all  (to sync) or maw ws merge <name> (to merge and destroy)"
        );
    }

    // Legacy: combined stale notice if nothing split above.
    if stale.is_empty() && !workspaces.is_empty() {
        // Nothing stale.
    } else if stale_persistent.is_empty() && stale_ephemeral.is_empty() && !stale.is_empty() {
        // Fallback for workspaces with unknown mode.
        println!();
        if use_color {
            println!(
                "\x1b[1;33m\u{25b2} WARNING:\x1b[0m {} stale workspace(s): {}",
                stale.len(),
                stale.join(", ")
            );
        } else {
            println!(
                "\u{25b2} WARNING: {} stale workspace(s): {}",
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
    stale_persistent: Vec<String>,
    stale_ephemeral: Vec<String>,
    format: OutputFormat,
) {
    let mut advice = vec![];

    if !stale_persistent.is_empty() {
        advice.push(Advice {
            level: "warn",
            message: format!(
                "{} stale persistent workspace(s): {} — run maw ws advance <name>",
                stale_persistent.len(),
                stale_persistent.join(", ")
            ),
            details: Some(AdviceDetails {
                workspaces: stale_persistent,
                fix: "maw ws advance <name>".to_string(),
            }),
        });
    }

    if !stale_ephemeral.is_empty() {
        advice.push(Advice {
            level: "warn",
            message: format!(
                "{} stale ephemeral workspace(s): {} — survived epoch advance; merge or destroy",
                stale_ephemeral.len(),
                stale_ephemeral.join(", ")
            ),
            details: Some(AdviceDetails {
                workspaces: stale_ephemeral,
                fix: "maw ws sync --all".to_string(),
            }),
        });
    }

    // Fallback advice if stale workspaces exist but weren't categorized.
    if advice.is_empty() && !stale_workspaces.is_empty() {
        advice.push(Advice {
            level: "warn",
            message: format!(
                "{} workspace(s) stale: {}",
                stale_workspaces.len(),
                stale_workspaces.join(", ")
            ),
            details: Some(AdviceDetails {
                workspaces: stale_workspaces,
                fix: "maw ws sync --all".to_string(),
            }),
        });
    }

    let envelope = WorkspaceListEnvelope { workspaces, advice };

    match format.serialize(&envelope) {
        Ok(output) => println!("{output}"),
        Err(e) => {
            tracing::warn!("Failed to serialize to JSON: {e}");
        }
    }
}

/// Print stale workspace warnings for text output mode.
fn print_stale_warning_text(
    stale: &[String],
    stale_persistent: &[String],
    stale_ephemeral: &[String],
) {
    if !stale_persistent.is_empty() {
        println!();
        println!(
            "STALE persistent workspace(s): {}",
            stale_persistent.join(", ")
        );
        for ws in stale_persistent {
            println!("  Fix: maw ws advance {ws}");
        }
    }
    if !stale_ephemeral.is_empty() {
        println!();
        println!(
            "WARNING: stale ephemeral workspace(s): {}",
            stale_ephemeral.join(", ")
        );
        println!("  Survived epoch advance — merge or destroy:");
        println!("  Fix: maw ws sync --all");
    }
    // Fallback for workspaces with unknown mode.
    if stale_persistent.is_empty() && stale_ephemeral.is_empty() && !stale.is_empty() {
        println!();
        println!(
            "WARNING: {} stale workspace(s): {}",
            stale.len(),
            stale.join(", ")
        );
        println!("  Fix: maw ws sync --all");
    }
}
