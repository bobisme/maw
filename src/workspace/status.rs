use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::format::OutputFormat;
use crate::jj::run_jj_with_op_recovery;

use super::{jj_cwd, DEFAULT_WORKSPACE};

#[derive(Serialize)]
pub(crate) struct WorkspaceStatus {
    pub(crate) current_workspace: String,
    pub(crate) is_stale: bool,
    pub(crate) has_changes: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) changes: Option<String>,
    pub(crate) workspaces: Vec<WorkspaceEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) conflicts: Vec<ConflictInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) divergent_commits: Vec<DivergentCommitInfo>,
}

#[derive(Serialize)]
pub(crate) struct WorkspaceEntry {
    pub(crate) name: String,
    pub(crate) is_current: bool,
    pub(crate) info: String,
}

#[derive(Serialize)]
pub(crate) struct ConflictInfo {
    pub(crate) change_id: String,
    pub(crate) description: String,
}

#[derive(Serialize)]
pub(crate) struct DivergentCommitInfo {
    pub(crate) change_id: String,
    pub(crate) commit_id: String,
    pub(crate) description: String,
}

pub(crate) fn status(format: OutputFormat) -> Result<()> {
    let cwd = jj_cwd()?;

    // Get current workspace name
    let current_ws = get_current_workspace(&cwd)?;

    // Check if stale
    let stale_check = run_jj_with_op_recovery(&["status"], &cwd)?;

    let status_stderr = String::from_utf8_lossy(&stale_check.stderr);
    let is_stale = status_stderr.contains("working copy is stale");
    let status_stdout = String::from_utf8_lossy(&stale_check.stdout);

    // Get all workspaces and their commits
    let ws_output = run_jj_with_op_recovery(&["workspace", "list"], &cwd)?;

    let ws_list = String::from_utf8_lossy(&ws_output.stdout);

    // Check for conflicts
    let log_output = run_jj_with_op_recovery(
        &[
            "log",
            "--no-graph",
            "-r",
            "conflicts()",
            "-T",
            r#"change_id.short() ++ " " ++ description.first_line() ++ "\n""#,
        ],
        &cwd,
    )?;

    let conflicts_text = String::from_utf8_lossy(&log_output.stdout);

    // Check for divergent commits
    let divergent_output = run_jj_with_op_recovery(
        &[
            "log",
            "--no-graph",
            "-T",
            r#"if(divergent, change_id.short() ++ " " ++ commit_id.short() ++ " " ++ description.first_line() ++ "\n", "")"#,
        ],
        &cwd,
    )?;

    let divergent_text = String::from_utf8_lossy(&divergent_output.stdout);

    // Handle different output formats
    match format {
        OutputFormat::Text => {
            print_status_text(
                &current_ws,
                is_stale,
                &status_stdout,
                &ws_list,
                &conflicts_text,
                &divergent_text,
            );
        }
        OutputFormat::Pretty => {
            print_status_pretty(
                &current_ws,
                is_stale,
                &status_stdout,
                &ws_list,
                &conflicts_text,
                &divergent_text,
                format.should_use_color(),
            );
        }
        OutputFormat::Json => {
            match build_status_struct(
                &current_ws,
                is_stale,
                &status_stdout,
                &ws_list,
                &conflicts_text,
                &divergent_text,
            ) {
                Ok(status_data) => match format.serialize(&status_data) {
                    Ok(output) => println!("{output}"),
                    Err(e) => {
                        eprintln!("Warning: Failed to serialize status to JSON: {e}");
                        eprintln!("Falling back to text output:");
                        print_status_text(
                            &current_ws,
                            is_stale,
                            &status_stdout,
                            &ws_list,
                            &conflicts_text,
                            &divergent_text,
                        );
                    }
                },
                Err(e) => {
                    eprintln!("Warning: Failed to parse status data: {e}");
                    eprintln!("Falling back to text output:");
                    print_status_text(
                        &current_ws,
                        is_stale,
                        &status_stdout,
                        &ws_list,
                        &conflicts_text,
                        &divergent_text,
                    );
                }
            }
        }
    }

    Ok(())
}

/// Print status in compact text format (ID-first, agent-friendly)
fn print_status_text(
    current_ws: &str,
    is_stale: bool,
    status_stdout: &str,
    ws_list: &str,
    conflicts: &str,
    divergent: &str,
) {
    // Current workspace and stale warning
    println!("workspace: {current_ws}");
    if is_stale {
        println!("stale: true");
        println!("  Fix: maw ws sync {current_ws}");
    }

    // Changes
    if status_stdout.trim().is_empty() {
        println!("changes: none");
    } else {
        println!("changes:");
        for line in status_stdout.lines() {
            println!("  {line}");
        }
    }
    println!();

    // All workspaces
    println!("workspaces:");
    for line in ws_list.lines() {
        if let Some((name, rest)) = line.split_once(':') {
            let name = name.trim().trim_end_matches('@');
            let is_current_marker = if name == current_ws { "  (current)" } else { "" };
            println!("  {}  {}{}", name, rest.trim(), is_current_marker);
        }
    }

    // Conflicts
    if !conflicts.trim().is_empty() {
        println!();
        println!("conflicts:");
        for line in conflicts.lines() {
            println!("  {line}");
        }
        println!("  Fix: edit files, then: maw exec <name> -- jj describe -m \"resolve: ...\"");
    }

    // Divergent commits
    if !divergent.trim().is_empty() {
        println!();
        println!("divergent:");
        for line in divergent.lines() {
            if !line.trim().is_empty() {
                println!("  {line}");
            }
        }
        println!("  Fix: maw exec <name> -- jj abandon <change-id>/0");
    }

    // Next command
    println!();
    println!("Next: maw exec {current_ws} -- jj describe -m \"feat: ...\"");
}

/// Print status in pretty format (colored, human-friendly)
fn print_status_pretty(
    current_ws: &str,
    is_stale: bool,
    status_stdout: &str,
    ws_list: &str,
    conflicts: &str,
    divergent: &str,
    use_color: bool,
) {
    let (bold, green, yellow, red, gray, reset) = if use_color {
        ("\x1b[1m", "\x1b[32m", "\x1b[33m", "\x1b[31m", "\x1b[90m", "\x1b[0m")
    } else {
        ("", "", "", "", "", "")
    };

    // Header
    println!("{bold}Workspace Status{reset}");
    println!();

    // Stale warning
    if is_stale {
        println!("{yellow}\u{25b2} WARNING:{reset} Working copy is stale");
        println!("  Another workspace changed shared history \u{2014} your files are outdated");
        println!("  {gray}Fix: maw ws sync {current_ws}{reset}");
        println!();
    }

    // Current workspace
    println!("{bold}Current:{reset} {current_ws}");
    if status_stdout.trim().is_empty() {
        println!("  {gray}(no changes){reset}");
    } else {
        for line in status_stdout.lines() {
            println!("  {line}");
        }
    }
    println!();

    // All workspaces
    println!("{bold}All Workspaces{reset}");
    for line in ws_list.lines() {
        if let Some((name, rest)) = line.split_once(':') {
            let name = name.trim().trim_end_matches('@');
            if name == current_ws {
                println!("  {green}\u{25cf} {name}{reset} {}", rest.trim());
            } else {
                println!("  {gray}\u{25cc} {name}{reset} {}", rest.trim());
            }
        }
    }

    // Conflicts
    if !conflicts.trim().is_empty() {
        println!();
        println!("{bold}Conflicts{reset}");
        println!("  {gray}jj records conflicts in commits \u{2014} you can keep working{reset}");
        for line in conflicts.lines() {
            println!("  {red}!{reset} {line}");
        }
        println!();
        println!("  To resolve: edit conflicted files (look for <<<<<<< markers)");
        println!("  then: {gray}maw exec <name> -- jj describe -m \"resolve: ...\"{reset}");
    }

    // Divergent commits
    if !divergent.trim().is_empty() {
        println!();
        println!("{bold}Divergent Commits{reset}");
        println!("  {yellow}WARNING: Multiple versions of the same commit{reset}");
        for line in divergent.lines() {
            if !line.trim().is_empty() {
                println!("  {yellow}~{reset} {line}");
            }
        }
        println!();
        println!("  Fix: {gray}maw exec <name> -- jj abandon <change-id>/0{reset}");
    }

    // Next command
    println!();
    println!("{gray}Next: maw exec {current_ws} -- jj describe -m \"feat: ...\"{reset}");
}

/// Build structured status data (resilient to parsing failures)
fn build_status_struct(
    current_ws: &str,
    is_stale: bool,
    status_stdout: &str,
    ws_list: &str,
    conflicts_text: &str,
    divergent_text: &str,
) -> Result<WorkspaceStatus> {
    let has_changes = !status_stdout.trim().is_empty();
    let changes = if has_changes {
        Some(status_stdout.to_string())
    } else {
        None
    };

    // Parse workspace list
    let mut workspaces = Vec::new();
    for line in ws_list.lines() {
        if let Some((name, rest)) = line.split_once(':') {
            let name = name.trim().to_string();
            let is_current = name == current_ws;
            workspaces.push(WorkspaceEntry {
                name,
                is_current,
                info: rest.trim().to_string(),
            });
        }
    }

    // Parse conflicts
    let mut conflicts = Vec::new();
    for line in conflicts_text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        if parts.len() >= 2 {
            conflicts.push(ConflictInfo {
                change_id: parts[0].to_string(),
                description: parts[1].to_string(),
            });
        }
    }

    // Parse divergent commits
    let mut divergent_commits = Vec::new();
    for line in divergent_text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        if parts.len() >= 3 {
            divergent_commits.push(DivergentCommitInfo {
                change_id: parts[0].to_string(),
                commit_id: parts[1].to_string(),
                description: parts[2].to_string(),
            });
        }
    }

    Ok(WorkspaceStatus {
        current_workspace: current_ws.to_string(),
        is_stale,
        has_changes,
        changes,
        workspaces,
        conflicts,
        divergent_commits,
    })
}

pub(crate) fn get_current_workspace(cwd: &Path) -> Result<String> {
    // jj workspace list marks current with @
    let output = run_jj_with_op_recovery(&["workspace", "list"], cwd)?;

    let list = String::from_utf8_lossy(&output.stdout);
    for line in list.lines() {
        if line.contains('@')
            && let Some((name, _)) = line.split_once(':') {
                return Ok(name.trim().trim_end_matches('@').to_string());
            }
    }

    Ok(DEFAULT_WORKSPACE.to_string())
}
