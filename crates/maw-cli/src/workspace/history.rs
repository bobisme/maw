use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use maw_core::backend::WorkspaceBackend;
use crate::format::OutputFormat;
use maw_core::model::types::WorkspaceId;
use maw_core::oplog::read::{OpLogReadError, walk_chain};
use maw_core::oplog::types::OpPayload;

use super::{get_backend, repo_root, validate_workspace_name};

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
/// Tries the manifold op log first via `oplog::read::walk_chain`.
/// Falls back to git commit history if no op log exists.
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

    let root = repo_root()?;

    // Try op log first (reads refs/manifold/head/<name> chain via read APIs)
    match fetch_oplog_history(&root, &ws_id, limit) {
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
// Op log history
// ---------------------------------------------------------------------------

/// Walk the op log chain via `oplog::read::walk_chain`.
fn fetch_oplog_history(
    root: &Path,
    ws_id: &WorkspaceId,
    limit: usize,
) -> Result<Vec<OperationEntry>> {
    let chain = match walk_chain(root, ws_id, Some(limit), None) {
        Ok(chain) => chain,
        Err(OpLogReadError::NoHead { .. }) => return Ok(vec![]),
        Err(err) => {
            return Err(anyhow::anyhow!(
                "Failed to read operation log for workspace '{}': {err}",
                ws_id.as_str()
            ));
        }
    };

    Ok(chain
        .into_iter()
        .map(|(oid, op)| OperationEntry {
            oid: oid.as_str().to_owned(),
            op_type: op_type(&op.payload).to_owned(),
            timestamp: op.timestamp,
            summary: summarize_payload(&op.payload),
            workspace_id: op.workspace_id.as_str().to_owned(),
        })
        .collect())
}

const fn op_type(payload: &OpPayload) -> &'static str {
    match payload {
        OpPayload::Create { .. } => "create",
        OpPayload::Destroy => "destroy",
        OpPayload::Snapshot { .. } => "snapshot",
        OpPayload::Merge { .. } => "merge",
        OpPayload::Compensate { .. } => "compensate",
        OpPayload::Describe { .. } => "describe",
        OpPayload::Annotate { .. } => "annotate",
    }
}

/// Summarize an operation payload.
fn summarize_payload(payload: &OpPayload) -> String {
    match payload {
        OpPayload::Create { epoch } => {
            let prefix = &epoch.as_str()[..epoch.as_str().len().min(8)];
            format!("workspace created (epoch {prefix})")
        }
        OpPayload::Destroy => "workspace destroyed".to_owned(),
        OpPayload::Snapshot { patch_set_oid } => {
            let prefix = &patch_set_oid.as_str()[..patch_set_oid.as_str().len().min(8)];
            format!("snapshot (patch {prefix})")
        }
        OpPayload::Merge {
            sources,
            epoch_before,
            epoch_after,
        } => {
            let sources = sources
                .iter()
                .map(WorkspaceId::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            let before = &epoch_before.as_str()[..epoch_before.as_str().len().min(8)];
            let after = &epoch_after.as_str()[..epoch_after.as_str().len().min(8)];
            format!("merge [{sources}] (epoch {before} → {after})")
        }
        OpPayload::Compensate { reason, .. } => format!("undo: {reason}"),
        OpPayload::Describe { message } => {
            let truncated = if message.len() > 60 {
                format!("{}…", &message[..60])
            } else {
                message.clone()
            };
            format!("describe: {truncated}")
        }
        OpPayload::Annotate { key, .. } => format!("annotate: {key}"),
    }
}

// ---------------------------------------------------------------------------
// Git commit history (fallback)
// ---------------------------------------------------------------------------

/// Fetch commits from git log in the workspace directory.
// TODO(gix): GitRepo has no log/rev-walk method. Keep CLI.
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
    use maw_core::model::types::{EpochId, GitOid, WorkspaceId};

    fn epoch(c: char) -> EpochId {
        EpochId::new(&c.to_string().repeat(40)).unwrap()
    }

    fn oid(c: char) -> GitOid {
        GitOid::new(&c.to_string().repeat(40)).unwrap()
    }

    fn ws(name: &str) -> WorkspaceId {
        WorkspaceId::new(name).unwrap()
    }

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
        let summary = summarize_payload(&OpPayload::Create {
            epoch: EpochId::new("aabbccdd00112233445566778899aabbccddeeff").unwrap(),
        });
        assert!(summary.contains("workspace created"));
        assert!(summary.contains("aabbccdd"));
    }

    #[test]
    fn summarize_destroy_payload() {
        assert_eq!(
            summarize_payload(&OpPayload::Destroy),
            "workspace destroyed"
        );
    }

    #[test]
    fn summarize_describe_truncation() {
        let long_msg = "a".repeat(100);
        let summary = summarize_payload(&OpPayload::Describe { message: long_msg });
        assert!(summary.len() < 80);
        assert!(summary.contains('…'));
    }

    #[test]
    fn summarize_describe_short() {
        assert_eq!(
            summarize_payload(&OpPayload::Describe {
                message: "hello world".to_string()
            }),
            "describe: hello world"
        );
    }

    #[test]
    fn summarize_merge_payload() {
        let summary = summarize_payload(&OpPayload::Merge {
            sources: vec![ws("agent-1"), ws("agent-2")],
            epoch_before: epoch('a'),
            epoch_after: epoch('b'),
        });
        assert!(summary.contains("agent-1"));
        assert!(summary.contains("agent-2"));
        assert!(summary.contains("merge"));
    }

    #[test]
    fn summarize_compensate_payload() {
        let summary = summarize_payload(&OpPayload::Compensate {
            target_op: oid('c'),
            reason: "reverted broken change".to_string(),
        });
        assert!(summary.contains("undo"));
        assert!(summary.contains("reverted broken change"));
    }

    #[test]
    fn summarize_snapshot_payload() {
        let summary = summarize_payload(&OpPayload::Snapshot {
            patch_set_oid: oid('d'),
        });
        assert!(summary.contains("snapshot"));
        assert!(summary.contains("dddddddd"));
    }

    #[test]
    fn summarize_annotate_payload() {
        assert_eq!(
            summarize_payload(&OpPayload::Annotate {
                key: "validation".to_string(),
                data: std::collections::BTreeMap::new(),
            }),
            "annotate: validation"
        );
    }
}
