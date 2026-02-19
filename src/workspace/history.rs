use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::backend::WorkspaceBackend;
use crate::format::OutputFormat;
use crate::model::types::WorkspaceId;

use super::{get_backend, validate_workspace_name};

#[derive(Serialize)]
pub struct HistoryEnvelope {
    pub(crate) workspace: String,
    pub(crate) commits: Vec<HistoryCommit>,
    pub(crate) advice: Vec<serde_json::Value>,
}

#[derive(Clone, Serialize)]
pub struct HistoryCommit {
    pub(crate) commit_id: String,
    pub(crate) timestamp: String,
    pub(crate) description: String,
}

/// Show commit history for a workspace.
///
/// In the git worktree model, workspace "history" is the git log
/// from the worktree's HEAD back to the epoch it was created from.
/// Since workspaces are detached at an epoch and agents don't commit,
/// this mainly shows the epoch commit and any parent history.
pub fn history(name: &str, limit: usize, format: Option<OutputFormat>) -> Result<()> {
    validate_workspace_name(name)?;
    let format = OutputFormat::resolve(format);

    let backend = get_backend()?;
    let ws_id = WorkspaceId::new(name)
        .map_err(|e| anyhow::anyhow!("Invalid workspace name: {e}"))?;

    if !backend.exists(&ws_id) {
        bail!(
            "Workspace '{name}' not found.\n  \
             List workspaces: maw ws list"
        );
    }

    let ws_path = backend.workspace_path(&ws_id);
    let commits = fetch_workspace_commits(&ws_path, limit)?;

    if commits.is_empty() {
        print_empty_history(name, format)?;
    } else {
        print_history(name, &commits, limit, format)?;
    }

    Ok(())
}

/// Fetch commits from git log in the workspace directory.
fn fetch_workspace_commits(
    ws_path: &std::path::Path,
    limit: usize,
) -> Result<Vec<HistoryCommit>> {
    let output = Command::new("git")
        .args([
            "log",
            "--format=%H %ai %s",
            "-n",
            &limit.to_string(),
        ])
        .current_dir(ws_path)
        .output()
        .context("Failed to get workspace history")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to get history: {}", stderr.trim());
    }

    let history = String::from_utf8_lossy(&output.stdout);
    Ok(parse_history_lines(&history))
}

/// Parse git log output lines into `HistoryCommit` structs.
fn parse_history_lines(raw: &str) -> Vec<HistoryCommit> {
    let mut commits = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        // Format: <full-oid> <date> <time> <tz> <subject>
        let parts: Vec<&str> = line.splitn(4, ' ').collect();
        if parts.len() >= 3 {
            let commit_id = parts[0][..12].to_string();
            let timestamp = parts[1].to_string();
            let description = if parts.len() >= 4 {
                // Skip timezone, get subject
                let rest = parts[3..].join(" ");
                // The timezone is embedded in the date format, subject follows
                if let Some((_tz, subject)) = rest.split_once(' ') {
                    subject.to_string()
                } else {
                    rest
                }
            } else {
                "(no description)".to_string()
            };
            commits.push(HistoryCommit {
                commit_id,
                timestamp,
                description,
            });
        }
    }
    commits
}

/// Print output when a workspace has no commits yet.
fn print_empty_history(name: &str, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => {
            let envelope = HistoryEnvelope {
                workspace: name.to_string(),
                commits: vec![],
                advice: vec![],
            };
            println!("{}", format.serialize(&envelope)?);
        }
        OutputFormat::Text => {
            println!("Workspace '{name}' has no commits.");
            println!();
            println!("Next: edit files in the workspace, then merge with maw ws merge {name}");
        }
        OutputFormat::Pretty => {
            println!("Workspace '{name}' has no commits.");
            println!();
            println!("  Edit files in the workspace directory.");
            println!("  Changes are captured during merge.");
        }
    }
    Ok(())
}

/// Print formatted commit history.
fn print_history(
    name: &str,
    commits: &[HistoryCommit],
    limit: usize,
    format: OutputFormat,
) -> Result<()> {
    match format {
        OutputFormat::Json => {
            let envelope = HistoryEnvelope {
                workspace: name.to_string(),
                commits: commits.to_vec(),
                advice: vec![],
            };
            println!("{}", format.serialize(&envelope)?);
        }
        OutputFormat::Text => {
            for commit in commits {
                println!(
                    "{}  {}  {}",
                    commit.commit_id, commit.timestamp, commit.description
                );
            }
            println!();
            println!("Next: maw exec {name} -- git show <commit-id>");
        }
        OutputFormat::Pretty => {
            println!("=== Commit History: {name} ===");
            println!();
            println!("  commit        timestamp         description");
            println!("  ──────────    ────────────────  ────────────────────────");

            for commit in commits {
                println!(
                    "  {}    {}  {}",
                    commit.commit_id, commit.timestamp, commit.description
                );
            }

            println!();
            println!("Showing {} commit(s)", commits.len());

            if commits.len() >= limit {
                println!("  (Use --limit/-n to show more)");
            }
        }
    }
    Ok(())
}
