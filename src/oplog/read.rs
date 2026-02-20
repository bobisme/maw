//! Op log read operations: walk the causal chain from the head ref backwards.
//!
//! This module implements the read path of the git-native per-workspace
//! operation log (§5.3):
//!
//! 1. **Read the head ref** `refs/manifold/head/<workspace>` to get the
//!    latest operation blob OID.
//! 2. **Read each blob** via `git cat-file -p <oid>`, deserialize JSON
//!    to [`Operation`].
//! 3. **Walk parents** iteratively (BFS) to reconstruct the full chain.
//!
//! # Walk strategy
//!
//! The walk uses breadth-first search to handle operations with multiple
//! parents (e.g., merge operations). A visited set prevents processing the
//! same operation twice when chains share common ancestors.
//!
//! # Stopping conditions
//!
//! The walk stops when:
//! - An operation has no parents (root of the chain).
//! - A maximum depth is reached (optional, for bounded reads).
//! - A caller-supplied predicate returns `false` (e.g., stop at checkpoints).
//!
//! # Example flow
//! ```text
//! read_head(root, ws)              → Option<GitOid>
//! read_operation(root, oid)        → Operation
//! walk_chain(root, ws, None, None) → Vec<(GitOid, Operation)>
//!   ├── read head ref → oid₃
//!   ├── cat-file oid₃ → op₃ (parent: oid₂)
//!   ├── cat-file oid₂ → op₂ (parent: oid₁)
//!   └── cat-file oid₁ → op₁ (parent: [])
//! ```

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::path::Path;
use std::process::Command;

use crate::model::types::{GitOid, WorkspaceId};
use crate::refs;

use super::types::Operation;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during an op log read.
#[derive(Debug)]
pub enum OpLogReadError {
    /// `git cat-file -p <oid>` failed or returned unexpected output.
    CatFile {
        /// The OID that could not be read.
        oid: String,
        /// Stderr from git.
        stderr: String,
        /// Process exit code, if available.
        exit_code: Option<i32>,
    },

    /// The blob content could not be deserialized as an [`Operation`].
    Deserialize {
        /// The OID whose content failed deserialization.
        oid: String,
        /// The underlying serde error.
        source: serde_json::Error,
    },

    /// I/O error (e.g. spawning git).
    Io(std::io::Error),

    /// A ref operation failed.
    RefError(refs::RefError),

    /// The head ref does not exist (workspace has no op log yet).
    NoHead {
        /// The workspace whose head ref is missing.
        workspace_id: WorkspaceId,
    },
}

impl fmt::Display for OpLogReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CatFile {
                oid,
                stderr,
                exit_code,
            } => {
                write!(f, "`git cat-file -p {oid}` failed")?;
                if let Some(code) = exit_code {
                    write!(f, " (exit code {code})")?;
                }
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                write!(
                    f,
                    "\n  To fix: verify the blob OID exists in the repository \
                     (`git cat-file -t {oid}`)."
                )
            }
            Self::Deserialize { oid, source } => {
                write!(
                    f,
                    "failed to deserialize operation blob {oid}: {source}\n  \
                     To fix: inspect the blob content with `git cat-file -p {oid}`."
                )
            }
            Self::Io(e) => write!(f, "I/O error during op log read: {e}"),
            Self::RefError(e) => write!(f, "ref read failed: {e}"),
            Self::NoHead { workspace_id } => {
                write!(
                    f,
                    "no op log head for workspace '{}' — \
                     the workspace has no recorded operations yet.\n  \
                     To fix: ensure at least one operation has been appended \
                     via `append_operation()`.",
                    workspace_id
                )
            }
        }
    }
}

impl std::error::Error for OpLogReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Deserialize { source, .. } => Some(source),
            Self::Io(e) => Some(e),
            Self::RefError(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for OpLogReadError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<refs::RefError> for OpLogReadError {
    fn from(e: refs::RefError) -> Self {
        Self::RefError(e)
    }
}

// ---------------------------------------------------------------------------
// Core helpers
// ---------------------------------------------------------------------------

/// Read the head ref for a workspace's op log.
///
/// Returns `Some(oid)` if the workspace has at least one operation recorded,
/// or `None` if the head ref does not exist yet.
///
/// # Arguments
/// * `root` — absolute path to the git repository root.
/// * `workspace_id` — the workspace whose log head to read.
///
/// # Errors
/// Returns an error if the ref read itself fails (I/O, git error).
pub fn read_head(
    root: &Path,
    workspace_id: &WorkspaceId,
) -> Result<Option<GitOid>, OpLogReadError> {
    let ref_name = refs::workspace_head_ref(workspace_id.as_str());
    let oid = refs::read_ref(root, &ref_name)?;
    Ok(oid)
}

/// Read a single operation blob from the git object store.
///
/// Runs `git cat-file -p <oid>`, deserializes the JSON content to an
/// [`Operation`].
///
/// # Arguments
/// * `root` — absolute path to the git repository root.
/// * `oid` — the blob OID of the operation to read.
///
/// # Errors
/// Returns an error if git cannot read the blob or if the blob content
/// is not a valid [`Operation`] JSON.
pub fn read_operation(root: &Path, oid: &GitOid) -> Result<Operation, OpLogReadError> {
    let output = Command::new("git")
        .args(["cat-file", "-p", oid.as_str()])
        .current_dir(root)
        .output()?;

    if !output.status.success() {
        return Err(OpLogReadError::CatFile {
            oid: oid.as_str().to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            exit_code: output.status.code(),
        });
    }

    Operation::from_json(&output.stdout).map_err(|e| OpLogReadError::Deserialize {
        oid: oid.as_str().to_owned(),
        source: e,
    })
}

// ---------------------------------------------------------------------------
// Chain walking
// ---------------------------------------------------------------------------

/// Walk the operation chain from the workspace head backwards.
///
/// Returns operations in reverse chronological order (newest first).
/// Each entry is `(oid, operation)` — the blob OID paired with the
/// deserialized operation.
///
/// # Arguments
/// * `root` — absolute path to the git repository root.
/// * `workspace_id` — the workspace whose log to walk.
/// * `max_depth` — optional limit on how many operations to read.
///   `None` reads the entire chain.
/// * `stop_at` — optional predicate. If it returns `false` for an
///   operation, that operation is included but its parents are not
///   explored. Use this to stop at checkpoint operations.
///
/// # Walk order
///
/// BFS from head. Operations with multiple parents (merges) are explored
/// breadth-first. A visited set prevents duplicates when chains converge.
///
/// # Errors
/// - [`OpLogReadError::NoHead`] if the workspace has no op log.
/// - [`OpLogReadError::CatFile`] or [`OpLogReadError::Deserialize`] if a
///   blob cannot be read or parsed.
pub fn walk_chain(
    root: &Path,
    workspace_id: &WorkspaceId,
    max_depth: Option<usize>,
    stop_at: Option<&dyn Fn(&Operation) -> bool>,
) -> Result<Vec<(GitOid, Operation)>, OpLogReadError> {
    let head = read_head(root, workspace_id)?.ok_or_else(|| OpLogReadError::NoHead {
        workspace_id: workspace_id.clone(),
    })?;

    let mut result = Vec::new();
    let mut visited = HashSet::new();
    let mut queue: VecDeque<GitOid> = VecDeque::new();

    queue.push_back(head);

    while let Some(oid) = queue.pop_front() {
        // Check depth limit.
        if let Some(max) = max_depth {
            if result.len() >= max {
                break;
            }
        }

        // Skip already-visited OIDs (prevents duplicates in DAGs).
        if !visited.insert(oid.as_str().to_owned()) {
            continue;
        }

        let op = read_operation(root, &oid)?;

        // Check stop predicate: include this op but don't explore its parents.
        let should_stop = stop_at.as_ref().map(|pred| pred(&op)).unwrap_or(false);

        result.push((oid, op.clone()));

        if should_stop {
            continue;
        }

        // Enqueue parents for BFS traversal.
        for parent_oid in &op.parent_ids {
            if !visited.contains(parent_oid.as_str()) {
                queue.push_back(parent_oid.clone());
            }
        }
    }

    Ok(result)
}

/// Walk the operation chain, reading all operations from head to root.
///
/// Convenience wrapper around [`walk_chain`] with no depth limit and no
/// stop predicate.
///
/// # Errors
/// Same as [`walk_chain`].
pub fn walk_all(
    root: &Path,
    workspace_id: &WorkspaceId,
) -> Result<Vec<(GitOid, Operation)>, OpLogReadError> {
    walk_chain(root, workspace_id, None, None)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::{EpochId, WorkspaceId};
    use crate::oplog::types::{OpPayload, Operation};
    use crate::oplog::write::{append_operation, write_operation_blob};
    use std::fs;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Create a fresh git repo with one commit.
    fn setup_repo() -> (TempDir, GitOid) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        StdCommand::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(root)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(root)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(root)
            .output()
            .unwrap();

        fs::write(root.join("README.md"), "# Test\n").unwrap();
        StdCommand::new("git")
            .args(["add", "README.md"])
            .current_dir(root)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(root)
            .output()
            .unwrap();

        let out = StdCommand::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        let oid_str = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        let oid = GitOid::new(&oid_str).unwrap();

        (dir, oid)
    }

    fn epoch(c: char) -> EpochId {
        EpochId::new(&c.to_string().repeat(40)).unwrap()
    }

    fn ws(name: &str) -> WorkspaceId {
        WorkspaceId::new(name).unwrap()
    }

    fn make_create_op(ws_id: &WorkspaceId) -> Operation {
        Operation {
            parent_ids: vec![],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T12:00:00Z".to_owned(),
            payload: OpPayload::Create { epoch: epoch('a') },
        }
    }

    fn make_describe_op(ws_id: &WorkspaceId, parent: GitOid, message: &str) -> Operation {
        Operation {
            parent_ids: vec![parent],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T13:00:00Z".to_owned(),
            payload: OpPayload::Describe {
                message: message.to_owned(),
            },
        }
    }

    /// Write a chain of N operations and return (oid, op) pairs in order.
    fn write_chain(root: &Path, ws_id: &WorkspaceId, count: usize) -> Vec<(GitOid, Operation)> {
        let mut chain = Vec::new();

        // First op: Create (no parent)
        let op1 = make_create_op(ws_id);
        let oid1 = append_operation(root, ws_id, &op1, None).unwrap();
        chain.push((oid1.clone(), op1));

        // Subsequent ops: Describe with incrementing messages
        let mut prev_oid = oid1;
        for i in 1..count {
            let op = make_describe_op(ws_id, prev_oid.clone(), &format!("step {}", i + 1));
            let oid = append_operation(root, ws_id, &op, Some(&prev_oid)).unwrap();
            chain.push((oid.clone(), op));
            prev_oid = oid;
        }

        chain
    }

    // -----------------------------------------------------------------------
    // read_head
    // -----------------------------------------------------------------------

    #[test]
    fn read_head_no_operations_returns_none() {
        let (dir, _) = setup_repo();
        let ws_id = ws("agent-1");
        let result = read_head(dir.path(), &ws_id).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn read_head_after_one_operation() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("agent-1");

        let op = make_create_op(&ws_id);
        let oid = append_operation(root, &ws_id, &op, None).unwrap();

        let head = read_head(root, &ws_id).unwrap();
        assert_eq!(head, Some(oid));
    }

    #[test]
    fn read_head_after_multiple_operations() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("agent-1");

        let chain = write_chain(root, &ws_id, 3);
        let last_oid = chain.last().unwrap().0.clone();

        let head = read_head(root, &ws_id).unwrap();
        assert_eq!(head, Some(last_oid));
    }

    // -----------------------------------------------------------------------
    // read_operation
    // -----------------------------------------------------------------------

    #[test]
    fn read_operation_round_trip() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("agent-1");

        let op = make_create_op(&ws_id);
        let oid = write_operation_blob(root, &op).unwrap();

        let read_back = read_operation(root, &oid).unwrap();
        assert_eq!(read_back, op);
    }

    #[test]
    fn read_operation_invalid_oid_fails() {
        let (dir, _) = setup_repo();
        let root = dir.path();

        let bad_oid = GitOid::new(&"f".repeat(40)).unwrap();
        let result = read_operation(root, &bad_oid);
        assert!(result.is_err());
        assert!(matches!(result, Err(OpLogReadError::CatFile { .. })));
    }

    // -----------------------------------------------------------------------
    // walk_chain — basic chain walking
    // -----------------------------------------------------------------------

    #[test]
    fn walk_chain_single_op() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("agent-1");

        let chain = write_chain(root, &ws_id, 1);

        let walked = walk_all(root, &ws_id).unwrap();
        assert_eq!(walked.len(), 1);
        assert_eq!(walked[0].0, chain[0].0);
        assert_eq!(walked[0].1, chain[0].1);
    }

    #[test]
    fn walk_chain_five_ops_reverse_order() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("agent-1");

        let chain = write_chain(root, &ws_id, 5);

        let walked = walk_all(root, &ws_id).unwrap();
        assert_eq!(walked.len(), 5);

        // Walked should be newest-first (reverse of write order).
        assert_eq!(walked[0].0, chain[4].0); // newest
        assert_eq!(walked[1].0, chain[3].0);
        assert_eq!(walked[2].0, chain[2].0);
        assert_eq!(walked[3].0, chain[1].0);
        assert_eq!(walked[4].0, chain[0].0); // oldest (root)
    }

    #[test]
    fn walk_chain_preserves_all_operations() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("agent-1");

        let chain = write_chain(root, &ws_id, 5);

        let walked = walk_all(root, &ws_id).unwrap();
        assert_eq!(walked.len(), 5);

        // All operations from the chain should be present (possibly reordered).
        let walked_oids: HashSet<_> = walked
            .iter()
            .map(|(oid, _)| oid.as_str().to_owned())
            .collect();
        for (oid, _) in &chain {
            assert!(
                walked_oids.contains(oid.as_str()),
                "OID {} from chain not found in walked result",
                oid.as_str()
            );
        }
    }

    // -----------------------------------------------------------------------
    // walk_chain — max_depth
    // -----------------------------------------------------------------------

    #[test]
    fn walk_chain_with_max_depth() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("agent-1");

        write_chain(root, &ws_id, 5);

        let walked = walk_chain(root, &ws_id, Some(3), None).unwrap();
        assert_eq!(walked.len(), 3);
    }

    #[test]
    fn walk_chain_max_depth_one() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("agent-1");

        let chain = write_chain(root, &ws_id, 5);

        let walked = walk_chain(root, &ws_id, Some(1), None).unwrap();
        assert_eq!(walked.len(), 1);
        // Should be the head (newest).
        assert_eq!(walked[0].0, chain[4].0);
    }

    #[test]
    fn walk_chain_max_depth_exceeds_chain_length() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("agent-1");

        write_chain(root, &ws_id, 3);

        let walked = walk_chain(root, &ws_id, Some(100), None).unwrap();
        assert_eq!(walked.len(), 3);
    }

    // -----------------------------------------------------------------------
    // walk_chain — stop_at predicate
    // -----------------------------------------------------------------------

    #[test]
    fn walk_chain_stop_at_create() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("agent-1");

        write_chain(root, &ws_id, 5);

        // Stop at Create operations (include them but don't go further).
        let walked = walk_chain(
            root,
            &ws_id,
            None,
            Some(&|op: &Operation| matches!(op.payload, OpPayload::Create { .. })),
        )
        .unwrap();

        // All 5 ops should be present because the Create is at the root
        // and has no parents anyway.
        assert_eq!(walked.len(), 5);
    }

    #[test]
    fn walk_chain_stop_at_describe_step_3() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("agent-1");

        write_chain(root, &ws_id, 5);

        // Stop at the operation that describes "step 3".
        let walked = walk_chain(
            root,
            &ws_id,
            None,
            Some(&|op: &Operation| {
                matches!(&op.payload, OpPayload::Describe { message } if message == "step 3")
            }),
        )
        .unwrap();

        // Should have: step 5 (head), step 4, step 3 (stop here).
        // Step 3's parents are NOT explored.
        assert_eq!(walked.len(), 3);
    }

    // -----------------------------------------------------------------------
    // walk_chain — no head ref
    // -----------------------------------------------------------------------

    #[test]
    fn walk_chain_no_head_returns_error() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("nonexistent");

        let result = walk_all(root, &ws_id);
        assert!(matches!(result, Err(OpLogReadError::NoHead { .. })));
    }

    // -----------------------------------------------------------------------
    // walk_chain — branching (multiple parents)
    // -----------------------------------------------------------------------

    #[test]
    fn walk_chain_merge_op_with_multiple_parents() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("default");

        // Create two independent ops in different "workspaces" (same ws for simplicity).
        let op1 = make_create_op(&ws_id);
        let oid1 = write_operation_blob(root, &op1).unwrap();

        let op2 = Operation {
            parent_ids: vec![],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T12:30:00Z".to_owned(),
            payload: OpPayload::Create { epoch: epoch('b') },
        };
        let oid2 = write_operation_blob(root, &op2).unwrap();

        // Merge op with both as parents.
        let merge_op = Operation {
            parent_ids: vec![oid1.clone(), oid2.clone()],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T15:00:00Z".to_owned(),
            payload: OpPayload::Merge {
                sources: vec![ws("ws-a"), ws("ws-b")],
                epoch_before: epoch('a'),
                epoch_after: epoch('c'),
            },
        };

        // Manually write merge blob and set head ref.
        let merge_oid = write_operation_blob(root, &merge_op).unwrap();
        let ref_name = refs::workspace_head_ref(ws_id.as_str());
        refs::write_ref(root, &ref_name, &merge_oid).unwrap();

        // Walk: should find all 3 operations.
        let walked = walk_all(root, &ws_id).unwrap();
        assert_eq!(walked.len(), 3);

        // First should be the merge (head).
        assert_eq!(walked[0].0, merge_oid);

        // The other two should be oid1 and oid2 in some BFS order.
        let walked_oids: HashSet<_> = walked
            .iter()
            .map(|(oid, _)| oid.as_str().to_owned())
            .collect();
        assert!(walked_oids.contains(oid1.as_str()));
        assert!(walked_oids.contains(oid2.as_str()));
    }

    #[test]
    fn walk_chain_diamond_dag_no_duplicates() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("default");

        // Build a diamond DAG:
        //     root
        //    /    \
        //   a      b
        //    \    /
        //     merge
        let root_op = make_create_op(&ws_id);
        let root_oid = write_operation_blob(root, &root_op).unwrap();

        let op_a = Operation {
            parent_ids: vec![root_oid.clone()],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T13:00:00Z".to_owned(),
            payload: OpPayload::Describe {
                message: "branch a".to_owned(),
            },
        };
        let oid_a = write_operation_blob(root, &op_a).unwrap();

        let op_b = Operation {
            parent_ids: vec![root_oid.clone()],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T13:30:00Z".to_owned(),
            payload: OpPayload::Describe {
                message: "branch b".to_owned(),
            },
        };
        let oid_b = write_operation_blob(root, &op_b).unwrap();

        let merge_op = Operation {
            parent_ids: vec![oid_a.clone(), oid_b.clone()],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T14:00:00Z".to_owned(),
            payload: OpPayload::Merge {
                sources: vec![ws("ws-a"), ws("ws-b")],
                epoch_before: epoch('a'),
                epoch_after: epoch('b'),
            },
        };
        let merge_oid = write_operation_blob(root, &merge_op).unwrap();

        // Set head to merge.
        let ref_name = refs::workspace_head_ref(ws_id.as_str());
        refs::write_ref(root, &ref_name, &merge_oid).unwrap();

        // Walk: should find exactly 4 operations (no duplicates for root_op).
        let walked = walk_all(root, &ws_id).unwrap();
        assert_eq!(
            walked.len(),
            4,
            "diamond DAG should yield 4 unique operations"
        );

        // Verify no duplicate OIDs.
        let oids: Vec<_> = walked
            .iter()
            .map(|(oid, _)| oid.as_str().to_owned())
            .collect();
        let unique: HashSet<_> = oids.iter().cloned().collect();
        assert_eq!(oids.len(), unique.len(), "no duplicate OIDs in walk result");
    }

    // -----------------------------------------------------------------------
    // read_operation content verification
    // -----------------------------------------------------------------------

    #[test]
    fn read_operation_preserves_all_fields() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("agent-1");

        let original = Operation {
            parent_ids: vec![
                GitOid::new(&"a".repeat(40)).unwrap(),
                GitOid::new(&"b".repeat(40)).unwrap(),
            ],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T15:30:00Z".to_owned(),
            payload: OpPayload::Compensate {
                target_op: GitOid::new(&"c".repeat(40)).unwrap(),
                reason: "reverting broken snapshot\nwith newlines".to_owned(),
            },
        };

        let oid = write_operation_blob(root, &original).unwrap();
        let read_back = read_operation(root, &oid).unwrap();

        assert_eq!(read_back.parent_ids, original.parent_ids);
        assert_eq!(read_back.workspace_id, original.workspace_id);
        assert_eq!(read_back.timestamp, original.timestamp);
        assert_eq!(read_back.payload, original.payload);
    }

    // -----------------------------------------------------------------------
    // Error display
    // -----------------------------------------------------------------------

    #[test]
    fn error_display_no_head() {
        let err = OpLogReadError::NoHead {
            workspace_id: ws("agent-1"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("agent-1"));
        assert!(msg.contains("no op log head"));
        assert!(msg.contains("append_operation"));
    }

    #[test]
    fn error_display_cat_file() {
        let err = OpLogReadError::CatFile {
            oid: "abc123".to_owned(),
            stderr: "fatal: not a valid object".to_owned(),
            exit_code: Some(128),
        };
        let msg = format!("{err}");
        assert!(msg.contains("cat-file"));
        assert!(msg.contains("abc123"));
        assert!(msg.contains("128"));
    }

    #[test]
    fn error_display_deserialize() {
        let err = OpLogReadError::Deserialize {
            oid: "deadbeef".to_owned(),
            source: serde_json::from_str::<Operation>("not json").unwrap_err(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("deserialize"));
        assert!(msg.contains("deadbeef"));
    }

    #[test]
    fn error_display_io() {
        let err = OpLogReadError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "git not found",
        ));
        let msg = format!("{err}");
        assert!(msg.contains("I/O error"));
        assert!(msg.contains("git not found"));
    }
}
