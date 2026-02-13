use anyhow::{bail, Result};
use serde::Serialize;

use crate::format::OutputFormat;
use crate::jj::run_jj_with_op_recovery;

use super::{check_stale_workspaces, jj_cwd, workspace_path, DEFAULT_WORKSPACE};

#[derive(Serialize)]
pub(crate) struct WorkspaceInfo {
    pub(crate) name: String,
    pub(crate) is_current: bool,
    pub(crate) is_default: bool,
    pub(crate) change_id: String,
    pub(crate) commit_id: String,
    pub(crate) description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<String>,
}

/// Envelope for `maw ws list --format json/toon` output.
/// Wraps the workspace array with an advice array so warnings
/// (stale workspaces, etc.) are machine-readable.
#[derive(Serialize)]
pub(crate) struct WorkspaceListEnvelope {
    pub(crate) workspaces: Vec<WorkspaceInfo>,
    pub(crate) advice: Vec<Advice>,
}

/// A single advisory message (warning, info) embedded in structured output.
#[derive(Serialize)]
pub(crate) struct Advice {
    pub(crate) level: &'static str,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) details: Option<AdviceDetails>,
}

/// Extra details for an advice entry.
#[derive(Serialize)]
pub(crate) struct AdviceDetails {
    pub(crate) workspaces: Vec<String>,
    pub(crate) fix: String,
}

pub(crate) fn list(verbose: bool, format: OutputFormat) -> Result<()> {
    let cwd = jj_cwd()?;
    let output = run_jj_with_op_recovery(&["workspace", "list"], &cwd)?;

    if !output.status.success() {
        bail!(
            "jj workspace list failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let list = String::from_utf8_lossy(&output.stdout);

    if list.trim().is_empty() {
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

    // Parse workspace list into structured data
    let workspaces: Vec<WorkspaceInfo> = match parse_workspace_list(&list, verbose) {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("Warning: Failed to parse workspace list: {e}");
            eprintln!("Falling back to raw text output:");
            println!("{list}");
            return Ok(());
        }
    };

    // Collect stale workspace warnings
    let stale_workspaces = check_stale_workspaces().unwrap_or_default();

    // Handle different output formats
    match format {
        OutputFormat::Text => {
            // Tab-separated with header, agent-friendly format
            if verbose {
                println!("NAME\tCHANGE_ID\tCOMMIT_ID\tDESCRIPTION\tDEFAULT\tPATH");
            } else {
                println!("NAME\tCHANGE_ID\tCOMMIT_ID\tDESCRIPTION\tDEFAULT");
            }
            for ws in &workspaces {
                let default_marker = if ws.is_default { "true" } else { "false" };
                if verbose {
                    let path = ws.path.as_deref().unwrap_or("");
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}",
                        ws.name, ws.change_id, ws.commit_id, ws.description, default_marker, path
                    );
                } else {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        ws.name, ws.change_id, ws.commit_id, ws.description, default_marker
                    );
                }
            }

            // Stale workspace warnings
            if !stale_workspaces.is_empty() {
                println!();
                println!("WARNING: {} stale workspace(s): {}",
                    stale_workspaces.len(),
                    stale_workspaces.join(", ")
                );
                println!("  Fix: maw ws sync --all");
            }

            // Suggested next command
            println!();
            println!("Next: maw exec <name> -- jj describe -m \"feat: ...\"");
        }

        OutputFormat::Pretty => {
            // Colored, human-friendly format with unicode glyphs
            let use_color = format.should_use_color();

            for ws in &workspaces {
                let (glyph, name_style, reset) = if use_color {
                    if ws.is_current {
                        ("\u{25cf}", "\x1b[1;32m", "\x1b[0m")  // Green bold for current
                    } else {
                        ("\u{25cc}", "\x1b[90m", "\x1b[0m")    // Gray for others
                    }
                } else if ws.is_current {
                    ("\u{25cf}", "", "")
                } else {
                    ("\u{25cc}", "", "")
                };

                println!(
                    "{} {}{}{} {} {}",
                    glyph, name_style, ws.name, reset, ws.change_id, ws.description
                );

                if verbose {
                    if let Some(path) = &ws.path {
                        println!("    path: {path}");
                    }
                    println!("    commit: {}", ws.commit_id);
                    if ws.is_default {
                        println!("    default workspace");
                    }
                }
            }

            // Stale workspace warnings
            if !stale_workspaces.is_empty() {
                println!();
                if use_color {
                    println!("\x1b[1;33m\u{25b2} WARNING:\x1b[0m {} stale workspace(s): {}",
                        stale_workspaces.len(),
                        stale_workspaces.join(", ")
                    );
                } else {
                    println!("\u{25b2} WARNING: {} stale workspace(s): {}",
                        stale_workspaces.len(),
                        stale_workspaces.join(", ")
                    );
                }
                println!("  Fix: maw ws sync --all");
            }

            // Suggested next command
            if !workspaces.is_empty() {
                println!();
                if use_color {
                    println!("\x1b[90mNext: maw exec <name> -- jj describe -m \"feat: ...\"\x1b[0m");
                } else {
                    println!("Next: maw exec <name> -- jj describe -m \"feat: ...\"");
                }
            }
        }

        OutputFormat::Json => {
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
                    eprintln!("Falling back to raw text output:");
                    println!("{list}");
                }
            }
        }
    }

    Ok(())
}

/// Parse jj workspace list output into structured data
/// Resilient to format changes - returns error if parsing fails
pub(crate) fn parse_workspace_list(list: &str, include_path: bool) -> Result<Vec<WorkspaceInfo>> {
    let mut workspaces = Vec::new();

    for line in list.lines() {
        // Expected format: "name@: change_id commit_id description"
        // Current workspace has @ marker
        let Some((name_part, rest)) = line.split_once(':') else {
            // Skip lines that don't match expected format
            continue;
        };

        let name_part = name_part.trim();
        let is_current = name_part.contains('@');
        let name = name_part.trim_end_matches('@').trim();

        // Parse rest: "change_id commit_id description..."
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if parts.len() < 2 {
            // Need at least change_id and commit_id
            bail!("Unexpected workspace line format: {line}");
        }

        let change_id = parts[0].to_string();
        let commit_id = parts[1].to_string();
        let description = parts[2..].join(" ");

        let path = if include_path {
            workspace_path(name).ok().and_then(|p| {
                if p.exists() {
                    Some(p.display().to_string())
                } else {
                    None
                }
            })
        } else {
            None
        };

        workspaces.push(WorkspaceInfo {
            name: name.to_string(),
            is_current,
            is_default: name == DEFAULT_WORKSPACE,
            change_id,
            commit_id,
            description,
            path,
        });
    }

    Ok(workspaces)
}
