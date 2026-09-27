//! `maw ops log` — a repo-level, time-ordered view of operations across every
//! workspace op log (bn-117s).
//!
//! Where `maw ws history <name>` shows one workspace's chain, this walks *all*
//! `refs/manifold/head/*` op-log heads, de-duplicates by operation blob OID,
//! and prints them newest-first with a stable short id. That id is what makes
//! `maw undo <op-id>` targetable and the whole surface auditable.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use maw_core::model::types::{GitOid, WorkspaceId};
use maw_core::oplog::read::walk_all;
use maw_core::oplog::types::OpPayload;

use crate::format::OutputFormat;
use crate::workspace::repo_root;

/// Prefix under which every workspace op-log head ref lives.
const HEAD_REF_PREFIX: &str = "refs/manifold/head/";

/// A single operation as seen at the repo level, tagged with the workspace
/// whose op log recorded it.
#[derive(Clone, Debug)]
pub struct RepoOp {
    /// The operation blob OID — its stable identity, shown (abbreviated) in
    /// `maw ops log` and accepted by `maw undo <op-id>`.
    pub id: GitOid,
    /// The workspace whose op log chain this operation was walked from.
    pub workspace: String,
    /// ISO-8601 UTC timestamp recorded on the operation.
    pub timestamp: String,
    /// The operation payload.
    pub payload: OpPayload,
}

impl RepoOp {
    /// The abbreviated (12-char) operation id used in human output.
    #[must_use]
    pub fn short_id(&self) -> &str {
        let s = self.id.as_str();
        &s[..s.len().min(12)]
    }

    /// A stable op-kind slug (`merge`, `compensate`, `snapshot`, …).
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        op_kind(&self.payload)
    }
}

/// Collect every operation reachable from any workspace op-log head, de-duped
/// by blob OID and sorted newest-first.
///
/// Order: by timestamp, then by op-log chain order (an op always sorts before
/// its ancestors — timestamps only have millisecond resolution, so an op and
/// its parent can share one; bn-15fzo), then by id for a deterministic
/// tie-break.
///
/// Best-effort per workspace: a workspace whose head ref dangles (e.g. a
/// destroyed workspace not yet gc'd) is skipped rather than failing the whole
/// listing.
///
/// # Errors
/// Returns an error only if the repository cannot be opened or its refs cannot
/// be listed.
pub fn collect_repo_ops(root: &Path) -> Result<Vec<RepoOp>> {
    use maw_git::GitRepo;
    let repo = maw_git::GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;
    let heads = repo
        .list_refs(HEAD_REF_PREFIX)
        .map_err(|e| anyhow::anyhow!("failed to list op-log head refs: {e}"))?;

    let mut ops: Vec<RepoOp> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut parents: HashMap<String, Vec<String>> = HashMap::new();

    for (ref_name, _oid) in heads {
        let Some(ws_name) = ref_name.as_str().strip_prefix(HEAD_REF_PREFIX) else {
            continue;
        };
        let Ok(ws_id) = WorkspaceId::new(ws_name) else {
            continue;
        };
        // A dangling head (destroyed-but-not-gc'd workspace) fails to walk;
        // skip it so one bad chain never blocks the repo-level view.
        let Ok(chain) = walk_all(root, &ws_id) else {
            continue;
        };
        for (oid, op) in chain {
            if !seen.insert(oid.as_str().to_owned()) {
                continue;
            }
            parents.insert(
                oid.as_str().to_owned(),
                op.parent_ids
                    .iter()
                    .map(|p| p.as_str().to_owned())
                    .collect(),
            );
            ops.push(RepoOp {
                id: oid,
                workspace: ws_name.to_owned(),
                timestamp: op.timestamp,
                payload: op.payload,
            });
        }
    }

    sort_newest_first(&mut ops, &parents);
    Ok(ops)
}

/// Sort newest first. ISO-8601 UTC timestamps sort lexicographically in
/// chronological order; ops sharing a timestamp are ordered by chain
/// generation (a descendant's generation always exceeds its ancestors', so
/// within a chain the later op comes first), and the id is the final
/// deterministic tie-break. Lexicographic on a fixed key, so a total order.
fn sort_newest_first(ops: &mut [RepoOp], parents: &HashMap<String, Vec<String>>) {
    let generations = generations(parents);
    let generation = |op: &RepoOp| generations.get(op.id.as_str()).copied().unwrap_or(0);
    ops.sort_by(|a, b| {
        b.timestamp
            .cmp(&a.timestamp)
            .then_with(|| generation(b).cmp(&generation(a)))
            .then_with(|| b.id.as_str().cmp(a.id.as_str()))
    });
}

/// Longest-path depth of every op from a root of the op DAG (ops whose
/// parents are all outside `parents` have generation 0). Iterative, so a
/// long chain cannot overflow the stack; a (corrupt) cycle is cut at the
/// back edge instead of looping.
fn generations(parents: &HashMap<String, Vec<String>>) -> HashMap<String, usize> {
    let mut done: HashMap<String, usize> = HashMap::new();
    let mut on_stack: HashSet<String> = HashSet::new();
    for start in parents.keys() {
        if done.contains_key(start) {
            continue;
        }
        let mut stack: Vec<(String, bool)> = vec![(start.clone(), false)];
        while let Some((id, expanded)) = stack.pop() {
            if done.contains_key(&id) {
                continue;
            }
            let ps = parents.get(&id).map(Vec::as_slice).unwrap_or_default();
            if expanded {
                on_stack.remove(&id);
                let generation = ps
                    .iter()
                    .filter(|p| parents.contains_key(p.as_str()))
                    .map(|p| done.get(p).map_or(0, |g| g + 1))
                    .max()
                    .unwrap_or(0);
                done.insert(id, generation);
                continue;
            }
            on_stack.insert(id.clone());
            stack.push((id, true));
            for p in ps {
                if parents.contains_key(p) && !done.contains_key(p) && !on_stack.contains(p) {
                    stack.push((p.clone(), false));
                }
            }
        }
    }
    done
}

/// Stable op-kind slug for a payload. Mirrors `workspace::history`'s mapping so
/// the two surfaces never drift.
const fn op_kind(payload: &OpPayload) -> &'static str {
    match payload {
        OpPayload::Create { .. } => "create",
        OpPayload::Destroy => "destroy",
        OpPayload::Snapshot { .. } => "snapshot",
        OpPayload::Merge { .. } => "merge",
        OpPayload::Compensate { .. } => "compensate",
        OpPayload::Describe { .. } => "describe",
        OpPayload::Annotate { .. } => "annotate",
        OpPayload::RebaseReplay { .. } => "rebase-replay",
        OpPayload::ConflictDetected { .. } => "conflict-detected",
        OpPayload::ConflictResolved { .. } => "conflict-resolved",
        OpPayload::Rebase { .. } => "rebase",
    }
}

/// One-line human summary of an operation (the detail column).
fn summarize(payload: &OpPayload) -> String {
    match payload {
        OpPayload::Create { epoch } => format!("epoch {}", short(epoch.as_str())),
        OpPayload::Destroy => "workspace destroyed".to_owned(),
        OpPayload::Snapshot { patch_set_oid } => {
            format!("patch {}", short(patch_set_oid.as_str()))
        }
        OpPayload::Merge {
            sources,
            epoch_before,
            epoch_after,
        } => {
            let srcs = sources
                .iter()
                .map(WorkspaceId::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "[{srcs}] epoch {} → {}",
                short(epoch_before.as_str()),
                short(epoch_after.as_str())
            )
        }
        OpPayload::Compensate { reason, .. } => reason.clone(),
        OpPayload::Describe { message } => truncate(message, 60),
        // bn-2fto: mirror `workspace::history` — an annotation carrying a
        // `summary` string renders it inline so the two surfaces never drift.
        OpPayload::Annotate { key, data } => {
            data.get("summary").and_then(|v| v.as_str()).map_or_else(
                || format!("annotate: {key}"),
                |detail| format!("annotate: {key} — {detail}"),
            )
        }
        OpPayload::RebaseReplay {
            original_commit, ..
        } => format!("replay {}", short(original_commit.as_str())),
        OpPayload::ConflictDetected { path, .. } => format!("conflict: {path}"),
        OpPayload::ConflictResolved { path, .. } => format!("resolved: {path}"),
        OpPayload::Rebase {
            old_epoch,
            new_epoch,
            ..
        } => format!(
            "rebase {} → {}",
            short(old_epoch.as_str()),
            short(new_epoch.as_str())
        ),
    }
}

fn short(oid: &str) -> &str {
    &oid[..oid.len().min(12)]
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Structured JSON representation of a repo-level operation.
#[derive(Serialize)]
struct OpJson {
    id: String,
    kind: &'static str,
    workspace: String,
    timestamp: String,
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sources: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    epoch_before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    epoch_after: Option<String>,
}

impl OpJson {
    fn from_op(op: &RepoOp) -> Self {
        let (sources, epoch_before, epoch_after) = match &op.payload {
            OpPayload::Merge {
                sources,
                epoch_before,
                epoch_after,
            } => (
                Some(sources.iter().map(|s| s.as_str().to_owned()).collect()),
                Some(epoch_before.as_str().to_owned()),
                Some(epoch_after.as_str().to_owned()),
            ),
            _ => (None, None, None),
        };
        Self {
            id: op.id.as_str().to_owned(),
            kind: op.kind(),
            workspace: op.workspace.clone(),
            timestamp: op.timestamp.clone(),
            summary: summarize(&op.payload),
            sources,
            epoch_before,
            epoch_after,
        }
    }
}

/// Run `maw ops log`.
///
/// # Errors
/// Returns an error if the repo root cannot be resolved or its op logs cannot
/// be read.
pub fn run(format: Option<OutputFormat>) -> Result<()> {
    let format = OutputFormat::resolve(format);
    let root = repo_root()?;
    let ops = collect_repo_ops(&root).context("Failed to read repo-level op log")?;

    if format == OutputFormat::Json {
        let json: Vec<OpJson> = ops.iter().map(OpJson::from_op).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json).context("serialize ops log")?
        );
        return Ok(());
    }

    if ops.is_empty() {
        println!("No operations recorded yet.");
        println!("Operations appear here after `maw ws merge`, `maw ws create`, etc.");
        return Ok(());
    }

    for op in &ops {
        println!(
            "{}  {:<12}  {}  ({})  {}",
            op.short_id(),
            op.kind(),
            op.timestamp,
            op.workspace,
            summarize(&op.payload),
        );
    }
    println!();
    println!("Undo the last epoch mutation: maw undo   (preview: maw undo --dry-run)");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use maw_core::model::types::EpochId;
    use maw_core::oplog::types::Operation;
    use maw_core::oplog::write::append_operation;

    fn ws(name: &str) -> WorkspaceId {
        WorkspaceId::new(name).expect("valid ws")
    }

    fn epoch(c: char) -> EpochId {
        EpochId::new(&c.to_string().repeat(40)).expect("valid epoch")
    }

    #[test]
    fn collect_orders_newest_first_and_dedups() {
        let (dir, _root, _oid) = maw_git::test_support::init_test_repo_with_commit();
        let root = dir.path();
        let a = ws("agent-a");

        let create = Operation {
            parent_ids: vec![],
            workspace_id: a.clone(),
            timestamp: "2026-01-01T00:00:00Z".to_owned(),
            payload: OpPayload::Create { epoch: epoch('a') },
        };
        let h1 = append_operation(root, &a, &create, None).expect("append create");
        let merge = Operation {
            parent_ids: vec![h1.clone()],
            workspace_id: a.clone(),
            timestamp: "2026-01-02T00:00:00Z".to_owned(),
            payload: OpPayload::Merge {
                sources: vec![ws("bob")],
                epoch_before: epoch('a'),
                epoch_after: epoch('b'),
            },
        };
        append_operation(root, &a, &merge, Some(&h1)).expect("append merge");

        let ops = collect_repo_ops(root).expect("collect");
        assert_eq!(ops.len(), 2, "two distinct ops");
        assert_eq!(ops[0].kind(), "merge", "newest first");
        assert_eq!(ops[1].kind(), "create");
    }

    /// bn-15fzo: ops written in the same millisecond keep op-log chain order
    /// (newest first), whatever their blob ids.
    #[test]
    fn same_timestamp_ops_keep_chain_order() {
        let (dir, _root, _oid) = maw_git::test_support::init_test_repo_with_commit();
        let root = dir.path();
        let a = ws("agent-a");
        let ts = "2026-01-01T00:00:00.000Z".to_owned();

        let mut head = append_operation(
            root,
            &a,
            &Operation {
                parent_ids: vec![],
                workspace_id: a.clone(),
                timestamp: ts.clone(),
                payload: OpPayload::Create { epoch: epoch('a') },
            },
            None,
        )
        .expect("append create");
        for i in 0..8 {
            let op = Operation {
                parent_ids: vec![head.clone()],
                workspace_id: a.clone(),
                timestamp: ts.clone(),
                payload: OpPayload::Describe {
                    message: format!("step {i}"),
                },
            };
            head = append_operation(root, &a, &op, Some(&head)).expect("append describe");
        }

        let ops = collect_repo_ops(root).expect("collect");
        let order: Vec<String> = ops
            .iter()
            .map(|op| match &op.payload {
                OpPayload::Describe { message } => message.clone(),
                other => op_kind(other).to_owned(),
            })
            .collect();
        let mut expected: Vec<String> = (0..8).rev().map(|i| format!("step {i}")).collect();
        expected.push("create".to_owned());
        assert_eq!(
            order, expected,
            "same-ms ops must list newest-first in chain order"
        );
    }

    #[test]
    fn generations_follow_longest_path_and_survive_cycles() {
        let map = |pairs: &[(&str, &[&str])]| -> HashMap<String, Vec<String>> {
            pairs
                .iter()
                .map(|(k, ps)| {
                    (
                        (*k).to_owned(),
                        ps.iter().map(|p| (*p).to_owned()).collect(),
                    )
                })
                .collect()
        };
        // a <- b <- d, a <- c <- d, and x's parent is outside the set.
        let g = generations(&map(&[
            ("a", &[]),
            ("b", &["a"]),
            ("c", &["a"]),
            ("d", &["b", "c"]),
            ("x", &["missing"]),
        ]));
        assert_eq!(g["a"], 0);
        assert_eq!(g["b"], 1);
        assert_eq!(g["c"], 1);
        assert_eq!(g["d"], 2);
        assert_eq!(g["x"], 0);
        // A corrupt cycle terminates.
        let g = generations(&map(&[("p", &["q"]), ("q", &["p"])]));
        assert_eq!(g.len(), 2);
    }

    #[test]
    fn merge_summary_lists_sources_and_epochs() {
        let payload = OpPayload::Merge {
            sources: vec![ws("alice"), ws("bob")],
            epoch_before: epoch('a'),
            epoch_after: epoch('b'),
        };
        let summary = summarize(&payload);
        assert!(
            summary.contains("alice"),
            "summary names sources: {summary}"
        );
        assert!(summary.contains("bob"));
        assert!(summary.contains('→'), "summary shows epoch transition");
    }
}
