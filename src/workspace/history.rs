use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::format::OutputFormat;

use super::{jj_cwd, validate_workspace_name};

#[derive(Serialize)]
pub(crate) struct HistoryEnvelope {
    pub(crate) workspace: String,
    pub(crate) commits: Vec<HistoryCommit>,
    pub(crate) advice: Vec<serde_json::Value>,
}

#[derive(Serialize)]
pub(crate) struct HistoryCommit {
    pub(crate) change_id: String,
    pub(crate) commit_id: String,
    pub(crate) timestamp: String,
    pub(crate) description: String,
}

/// Show commit history for a workspace
pub(crate) fn history(name: &str, limit: usize, format: Option<OutputFormat>) -> Result<()> {
    let cwd = jj_cwd()?;
    validate_workspace_name(name)?;
    let format = OutputFormat::resolve(format);

    // Check if workspace exists
    let ws_output = Command::new("jj")
        .args(["workspace", "list"])
        .current_dir(&cwd)
        .output()
        .context("Failed to run jj workspace list")?;

    let ws_list = String::from_utf8_lossy(&ws_output.stdout);
    let workspace_exists = ws_list
        .lines()
        .any(|line| {
            line.split(':')
                .next()
                .is_some_and(|n| n.trim().trim_end_matches('@') == name)
        });

    if !workspace_exists {
        bail!(
            "Workspace '{name}' not found.\n  \
             List workspaces: maw ws list"
        );
    }

    // Use revset to get commits specific to this workspace:
    // {name}@:: gets all commits reachable from the workspace's working copy
    // ~::main excludes commits already in main (ancestors of main)
    // This shows commits the workspace has made since diverging from main
    let revset = format!("{name}@:: & ~::main");

    let output = Command::new("jj")
        .args([
            "log",
            "--no-graph",
            "-r",
            &revset,
            "-T",
            r#"change_id.short() ++ " " ++ commit_id.short() ++ " " ++ committer.timestamp().format("%Y-%m-%d %H:%M") ++ " " ++ if(description.first_line(), description.first_line(), "(no description)") ++ "\n""#,
            "-n",
            &limit.to_string(),
        ])
        .current_dir(&cwd)
        .output()
        .context("Failed to get workspace history")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to get history: {}", stderr.trim());
    }

    let history = String::from_utf8_lossy(&output.stdout);

    // Parse commits into structs
    let mut commits = Vec::new();
    for line in history.lines() {
        if line.trim().is_empty() {
            continue;
        }
        // Format: change_id commit_id date time description
        let parts: Vec<&str> = line.splitn(5, ' ').collect();
        if parts.len() >= 4 {
            let change_id = parts[0].to_string();
            let commit_id = parts[1].to_string();
            let date = parts[2];
            let time = parts[3];
            let timestamp = format!("{date} {time}");
            let description = if parts.len() >= 5 {
                parts[4].to_string()
            } else {
                "(no description)".to_string()
            };
            commits.push(HistoryCommit {
                change_id,
                commit_id,
                timestamp,
                description,
            });
        }
    }

    if commits.is_empty() {
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
                println!("Workspace '{name}' has no commits yet.");
                println!();
                println!("Next: maw exec {name} -- jj describe -m \"feat: what you're implementing\"");
            }
            OutputFormat::Pretty => {
                println!("Workspace '{name}' has no commits yet.");
                println!();
                println!("  (Workspace starts with an empty commit for ownership.");
                println!("   Edit files and describe your changes to create history.)");
                println!();
                println!("  Start working:");
                println!("    maw exec {name} -- jj describe -m \"feat: what you're implementing\"");
            }
        }
        return Ok(());
    }

    match format {
        OutputFormat::Json => {
            let envelope = HistoryEnvelope {
                workspace: name.to_string(),
                commits,
                advice: vec![],
            };
            println!("{}", format.serialize(&envelope)?);
        }
        OutputFormat::Text => {
            for commit in &commits {
                println!(
                    "{}  {}  {}  {}",
                    commit.change_id, commit.commit_id, commit.timestamp, commit.description
                );
            }
            println!();
            println!("Next: maw exec {name} -- jj diff -r <change_id>");
        }
        OutputFormat::Pretty => {
            println!("=== Commit History: {name} ===");
            println!();
            println!("  change_id      commit        timestamp         description");
            println!("  ────────────   ──────────    ────────────────  ────────────────────────");

            for commit in &commits {
                println!(
                    "  {}   {}    {}  {}",
                    commit.change_id, commit.commit_id, commit.timestamp, commit.description
                );
            }

            println!();
            println!("Showing {} commit(s)", commits.len());

            if commits.len() >= limit {
                println!("  (Use --limit/-n to show more)");
            }

            println!();
            println!("Tip: View full commit details:");
            println!("  maw exec {name} -- jj show <change-id>");
        }
    }

    Ok(())
}
