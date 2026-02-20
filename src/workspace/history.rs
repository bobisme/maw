use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::backend::WorkspaceBackend;
use crate::format::OutputFormat;
use crate::model::types::WorkspaceId;

use super::{get_backend, validate_workspace_name};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct HistoryEnvelope {
    pub(crate) workspace: String,
    /// Op log operations (primary source).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) operations: Vec<OperationEntry>,
    /// Fallback: git commit history (when no op log exists).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) commits: Vec<HistoryCommit>,
    pub(crate) advice: Vec<serde_json::Value>,
}

/// An operation from the manifold op log.
#[derive(Clone, Serialize)]
pub struct OperationEntry {
    /// Git blob OID of this operation.
    pub(crate) oid: String,
    /// Operation type (create, snapshot, merge, etc.).
    pub(crate) op_type: String,
    /// ISO 8601 timestamp.
    pub(crate) timestamp: String,
    /// Human-readable summary of the operation.
    pub(crate) summary: String,
    /// The workspace that performed this operation.
    pub(crate) workspace_id: String,
}

/// A git commit (fallback when no op log exists).
#[derive(Clone, Serialize)]
pub struct HistoryCommit {
    pub(crate) commit_id: String,
    pub(crate) timestamp: String,
    pub(crate) description: String,
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Show history for a workspace.
///
/// Tries the manifold op log first (reads `refs/manifold/head/<name>`
/// and walks the blob chain). Falls back to git commit history if no
/// op log exists.
///
/// # Flags
/// - `--json` / `OutputFormat::Json`: structured JSON envelope
/// - `--limit N`: cap entries (default: 20)
pub fn history(name: &str, limit: usize, format: Option<OutputFormat>) -> Result<()> {
    validate_workspace_name(name)?;
    let format = OutputFormat::resolve(format);

    let backend = get_backend()?;
    let ws_id =
        WorkspaceId::new(name).map_err(|e| anyhow::anyhow!("Invalid workspace name: {e}"))?;

    if !backend.exists(&ws_id) {
        bail!(
            "Workspace '{name}' not found.\n  \
             List workspaces: maw ws list"
        );
    }

    let ws_path = backend.workspace_path(&ws_id);

    // Try op log first (reads refs/manifold/head/<name> chain)
    match fetch_oplog_history(&ws_path, name, limit) {
        Ok(operations) if !operations.is_empty() => {
            print_oplog_history(name, &operations, limit, format)?;
        }
        _ => {
            // Fallback: git commit history
            let commits = fetch_workspace_commits(&ws_path, limit)?;
            if commits.is_empty() {
                print_empty_history(name, format)?;
            } else {
                print_commit_history(name, &commits, limit, format)?;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Op log history — direct git reads (no lib dependency)
// ---------------------------------------------------------------------------

/// Walk the op log chain by reading blobs directly via git cat-file.
///
/// 1. Read `refs/manifold/head/<name>` to get the head blob OID.
/// 2. cat-file blob → parse JSON → extract parent_ids → repeat.
fn fetch_oplog_history(ws_path: &Path, name: &str, limit: usize) -> Result<Vec<OperationEntry>> {
    let ref_name = format!("refs/manifold/head/{name}");

    // Read head ref
    let output = Command::new("git")
        .args(["rev-parse", &ref_name])
        .current_dir(ws_path)
        .output()
        .context("Failed to read op log head ref")?;

    if !output.status.success() {
        return Ok(vec![]); // No op log for this workspace
    }

    let head_oid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if head_oid.is_empty() {
        return Ok(vec![]);
    }

    // Walk the chain
    let mut operations = Vec::new();
    let mut queue = std::collections::VecDeque::new();
    let mut visited = std::collections::HashSet::new();

    queue.push_back(head_oid);

    while let Some(oid) = queue.pop_front() {
        if operations.len() >= limit {
            break;
        }
        if !visited.insert(oid.clone()) {
            continue;
        }

        // Read blob
        let output = Command::new("git")
            .args(["cat-file", "-p", &oid])
            .current_dir(ws_path)
            .output()?;

        if !output.status.success() {
            break; // Corrupt or missing blob
        }

        // Parse JSON
        let json: serde_json::Value = match serde_json::from_slice(&output.stdout) {
            Ok(v) => v,
            Err(_) => break, // Invalid JSON blob
        };

        // Extract fields
        let op_type = json
            .get("payload")
            .and_then(|p| p.get("type"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_owned();

        let timestamp = json
            .get("timestamp")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned();

        let workspace_id = json
            .get("workspace_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned();

        let summary = summarize_payload(&json);

        operations.push(OperationEntry {
            oid: oid.clone(),
            op_type,
            timestamp,
            summary,
            workspace_id,
        });

        // Enqueue parents
        if let Some(parents) = json.get("parent_ids").and_then(serde_json::Value::as_array) {
            for parent in parents {
                if let Some(parent_oid) = parent.as_str() {
                    if !visited.contains(parent_oid) {
                        queue.push_back(parent_oid.to_owned());
                    }
                }
            }
        }
    }

    Ok(operations)
}

/// Summarize an operation payload from raw JSON.
fn summarize_payload(json: &serde_json::Value) -> String {
    let payload = match json.get("payload") {
        Some(p) => p,
        None => return "(no payload)".to_owned(),
    };

    let op_type = payload
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");

    match op_type {
        "create" => {
            let epoch = payload
                .get("epoch")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let prefix = &epoch[..epoch.len().min(8)];
            format!("workspace created (epoch {prefix})")
        }
        "destroy" => "workspace destroyed".to_owned(),
        "snapshot" => {
            let oid = payload
                .get("patch_set_oid")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let prefix = &oid[..oid.len().min(8)];
            format!("snapshot (patch {prefix})")
        }
        "merge" => {
            let sources = payload
                .get("sources")
                .and_then(serde_json::Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let before = payload
                .get("epoch_before")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let after = payload
                .get("epoch_after")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let bp = &before[..before.len().min(8)];
            let ap = &after[..after.len().min(8)];
            format!("merge [{sources}] (epoch {bp} → {ap})")
        }
        "compensate" => {
            let reason = payload
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            format!("undo: {reason}")
        }
        "describe" => {
            let message = payload
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let truncated = if message.len() > 60 {
                format!("{}…", &message[..60])
            } else {
                message.to_owned()
            };
            format!("describe: {truncated}")
        }
        "annotate" => {
            let key = payload
                .get("key")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            format!("annotate: {key}")
        }
        other => format!("{other}"),
    }
}

// ---------------------------------------------------------------------------
// Git commit history (fallback)
// ---------------------------------------------------------------------------

/// Fetch commits from git log in the workspace directory.
fn fetch_workspace_commits(ws_path: &Path, limit: usize) -> Result<Vec<HistoryCommit>> {
    let output = Command::new("git")
        .args(["log", "--format=%H %ai %s", "-n", &limit.to_string()])
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
            let commit_id = parts[0][..parts[0].len().min(12)].to_string();
            let timestamp = parts[1].to_string();
            let description = if parts.len() >= 4 {
                let rest = parts[3..].join(" ");
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

// ---------------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------------

/// Print op log operations.
fn print_oplog_history(
    name: &str,
    operations: &[OperationEntry],
    limit: usize,
    format: OutputFormat,
) -> Result<()> {
    match format {
        OutputFormat::Json => {
            let envelope = HistoryEnvelope {
                workspace: name.to_string(),
                operations: operations.to_vec(),
                commits: vec![],
                advice: vec![],
            };
            println!("{}", format.serialize(&envelope)?);
        }
        OutputFormat::Text => {
            for op in operations {
                println!(
                    "{}  {}  [{}]  {}",
                    &op.oid[..op.oid.len().min(12)],
                    op.timestamp,
                    op.op_type,
                    op.summary,
                );
            }
            println!();
            println!(
                "Showing {} operation(s) for workspace '{name}'",
                operations.len()
            );
        }
        OutputFormat::Pretty => {
            println!("=== Operation History: {name} ===");
            println!();

            for op in operations {
                println!(
                    "  {} │ {} │ {:>10} │ {}",
                    &op.oid[..op.oid.len().min(12)],
                    op.timestamp,
                    op.op_type,
                    op.summary,
                );
            }

            println!();
            println!("  {} operation(s)", operations.len());
            if operations.len() >= limit {
                println!("  (Use --limit/-n to show more)");
            }
        }
    }
    Ok(())
}

/// Print output when a workspace has no history.
fn print_empty_history(name: &str, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => {
            let envelope = HistoryEnvelope {
                workspace: name.to_string(),
                operations: vec![],
                commits: vec![],
                advice: vec![],
            };
            println!("{}", format.serialize(&envelope)?);
        }
        OutputFormat::Text => {
            println!("Workspace '{name}' has no history.");
            println!();
            println!("Next: edit files in the workspace, then merge with maw ws merge {name}");
        }
        OutputFormat::Pretty => {
            println!("Workspace '{name}' has no history.");
            println!();
            println!("  Edit files in the workspace directory.");
            println!("  Changes are captured during merge.");
        }
    }
    Ok(())
}

/// Print git commit history (fallback).
fn print_commit_history(
    name: &str,
    commits: &[HistoryCommit],
    limit: usize,
    format: OutputFormat,
) -> Result<()> {
    match format {
        OutputFormat::Json => {
            let envelope = HistoryEnvelope {
                workspace: name.to_string(),
                operations: vec![],
                commits: commits.to_vec(),
                advice: vec![serde_json::json!(
                    "No operation log found. Showing git commit history instead."
                )],
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
            println!("Note: showing git commit history (no operation log for workspace '{name}')");
            println!("Next: maw exec {name} -- git show <commit-id>");
        }
        OutputFormat::Pretty => {
            println!("=== Commit History: {name} (no operation log — git fallback) ===");
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_history_lines_normal() {
        let raw =
            "abc123def456abc123def456abc123def456ab1234 2026-02-19 12:00:00 +0000 fix: something";
        let commits = parse_history_lines(raw);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].commit_id, "abc123def456");
        assert_eq!(commits[0].timestamp, "2026-02-19");
        assert_eq!(commits[0].description, "fix: something");
    }

    #[test]
    fn parse_history_lines_empty() {
        let commits = parse_history_lines("");
        assert!(commits.is_empty());
    }

    #[test]
    fn parse_history_lines_whitespace() {
        let commits = parse_history_lines("   \n  \n");
        assert!(commits.is_empty());
    }

    #[test]
    fn summarize_create_payload() {
        let json = serde_json::json!({
            "payload": {"type": "create", "epoch": "aabbccdd00112233445566778899aabbccddeeff"},
            "timestamp": "2026-02-19T12:00:00Z",
            "workspace_id": "w1",
            "parent_ids": []
        });
        let summary = summarize_payload(&json);
        assert!(summary.contains("workspace created"));
        assert!(summary.contains("aabbccdd"));
    }

    #[test]
    fn summarize_destroy_payload() {
        let json = serde_json::json!({
            "payload": {"type": "destroy"},
            "timestamp": "2026-02-19T12:00:00Z",
            "workspace_id": "w1",
            "parent_ids": []
        });
        assert_eq!(summarize_payload(&json), "workspace destroyed");
    }

    #[test]
    fn summarize_describe_truncation() {
        let long_msg = "a".repeat(100);
        let json = serde_json::json!({
            "payload": {"type": "describe", "message": long_msg},
            "timestamp": "2026-02-19T12:00:00Z",
            "workspace_id": "w1",
            "parent_ids": []
        });
        let summary = summarize_payload(&json);
        assert!(summary.len() < 80);
        assert!(summary.contains('…'));
    }

    #[test]
    fn summarize_describe_short() {
        let json = serde_json::json!({
            "payload": {"type": "describe", "message": "hello world"},
            "timestamp": "2026-02-19T12:00:00Z",
            "workspace_id": "w1",
            "parent_ids": []
        });
        assert_eq!(summarize_payload(&json), "describe: hello world");
    }

    #[test]
    fn summarize_merge_payload() {
        let json = serde_json::json!({
            "payload": {
                "type": "merge",
                "sources": ["agent-1", "agent-2"],
                "epoch_before": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "epoch_after": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            },
            "timestamp": "2026-02-19T12:00:00Z",
            "workspace_id": "default",
            "parent_ids": []
        });
        let summary = summarize_payload(&json);
        assert!(summary.contains("agent-1"));
        assert!(summary.contains("agent-2"));
        assert!(summary.contains("merge"));
    }

    #[test]
    fn summarize_compensate_payload() {
        let json = serde_json::json!({
            "payload": {
                "type": "compensate",
                "target_op": "cccccccccccccccccccccccccccccccccccccccc",
                "reason": "reverted broken change"
            },
            "timestamp": "2026-02-19T12:00:00Z",
            "workspace_id": "w1",
            "parent_ids": []
        });
        let summary = summarize_payload(&json);
        assert!(summary.contains("undo"));
        assert!(summary.contains("reverted broken change"));
    }

    #[test]
    fn summarize_snapshot_payload() {
        let json = serde_json::json!({
            "payload": {
                "type": "snapshot",
                "patch_set_oid": "dddddddddddddddddddddddddddddddddddddddd"
            },
            "timestamp": "2026-02-19T12:00:00Z",
            "workspace_id": "w1",
            "parent_ids": []
        });
        let summary = summarize_payload(&json);
        assert!(summary.contains("snapshot"));
        assert!(summary.contains("dddddddd"));
    }

    #[test]
    fn summarize_annotate_payload() {
        let json = serde_json::json!({
            "payload": {"type": "annotate", "key": "validation", "data": {}},
            "timestamp": "2026-02-19T12:00:00Z",
            "workspace_id": "w1",
            "parent_ids": []
        });
        assert_eq!(summarize_payload(&json), "annotate: validation");
    }

    #[test]
    fn summarize_unknown_payload() {
        let json = serde_json::json!({
            "payload": {"type": "future-type"},
            "timestamp": "2026-02-19T12:00:00Z",
            "workspace_id": "w1",
            "parent_ids": []
        });
        assert_eq!(summarize_payload(&json), "future-type");
    }

    #[test]
    fn summarize_missing_payload() {
        let json = serde_json::json!({"timestamp": "t"});
        assert_eq!(summarize_payload(&json), "(no payload)");
    }
}
