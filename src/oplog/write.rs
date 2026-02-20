//! Op log write operations: store operations as git blobs and update head refs.
//!
//! This module implements the write path of the git-native per-workspace
//! operation log (§5.3):
//!
//! 1. **Serialize** an [`Operation`] to canonical JSON.
//! 2. **Write as a git blob** via `git hash-object -w --stdin` — the blob OID
//!    becomes the operation's identity.
//! 3. **Update the head ref** atomically with `git update-ref` using
//!    compare-and-swap (CAS) to guard against concurrent writes.
//!
//! # Single-writer invariant
//!
//! Each workspace has exactly one writer at a time (§5.3 §5.1). The CAS
//! step is therefore a safety net, not a retry loop: if CAS fails, something
//! has gone wrong with the single-writer invariant and the error bubbles up.
//!
//! # Ref layout
//!
//! ```text
//! refs/manifold/head/<workspace>  ← latest operation blob OID
//! ```
//!
//! # Example flow
//! ```text
//! write_operation_blob(root, &op)  → blob_oid
//! append_operation(root, ws, &op, old_head) → blob_oid
//!   ├── hash-object → blob_oid
//!   └── update-ref refs/manifold/head/<ws> <blob_oid> [<old_head>]
//! ```

use std::fmt;
use std::io::Write as IoWrite;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::model::types::{GitOid, WorkspaceId};
use crate::refs as manifold_refs;

use super::types::Operation;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during an op log write.
#[derive(Debug)]
pub enum OpLogWriteError {
    /// Serializing the operation to canonical JSON failed.
    Serialize(serde_json::Error),

    /// `git hash-object -w --stdin` failed or returned unexpected output.
    HashObject {
        /// Stderr from git.
        stderr: String,
        /// Process exit code, if available.
        exit_code: Option<i32>,
    },

    /// The OID returned by `git hash-object` was malformed.
    InvalidOid {
        /// The raw bytes git printed.
        raw: String,
    },

    /// I/O error (e.g. spawning git, writing to its stdin).
    Io(std::io::Error),

    /// CAS failed: the head ref was modified between read and write.
    ///
    /// With the single-writer invariant this should never happen. If it does,
    /// it indicates a bug or a broken invariant upstream.
    CasMismatch {
        /// The workspace whose head ref could not be updated.
        workspace_id: WorkspaceId,
    },

    /// A lower-level ref operation (other than CAS mismatch) failed.
    RefError(manifold_refs::RefError),
}

impl fmt::Display for OpLogWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Serialize(e) => write!(f, "failed to serialize operation to JSON: {e}"),
            Self::HashObject { stderr, exit_code } => {
                write!(f, "`git hash-object` failed")?;
                if let Some(code) = exit_code {
                    write!(f, " (exit code {code})")?;
                }
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                write!(
                    f,
                    "\n  To fix: check that the repository is not bare and that git is available."
                )
            }
            Self::InvalidOid { raw } => {
                write!(
                    f,
                    "`git hash-object` returned an invalid OID: {raw:?} \
                     (expected 40 lowercase hex characters)"
                )
            }
            Self::Io(e) => write!(f, "I/O error during op log write: {e}"),
            Self::CasMismatch { workspace_id } => {
                write!(
                    f,
                    "CAS mismatch on workspace '{workspace_id}' head ref — \
                     the single-writer invariant was violated.\n  \
                     To fix: check that no other process is writing to this workspace."
                )
            }
            Self::RefError(e) => write!(f, "ref update failed: {e}"),
        }
    }
}

impl std::error::Error for OpLogWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Serialize(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::RefError(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for OpLogWriteError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Core helpers
// ---------------------------------------------------------------------------

/// Write an [`Operation`] as a git blob and return its OID.
///
/// Serializes the operation to canonical JSON and pipes it to
/// `git hash-object -w --stdin`. The returned OID is the operation's
/// content-addressed identity.
///
/// # Arguments
/// * `root` — absolute path to the git repository root.
/// * `op` — the operation to store.
///
/// # Errors
/// Returns an error if serialization fails, if git cannot be spawned,
/// or if git fails to write the blob.
pub fn write_operation_blob(root: &Path, op: &Operation) -> Result<GitOid, OpLogWriteError> {
    // 1. Serialize to canonical JSON.
    let json = op.to_canonical_json().map_err(OpLogWriteError::Serialize)?;

    // 2. Spawn `git hash-object -w --stdin` and pipe JSON in.
    let mut child = Command::new("git")
        .args(["hash-object", "-w", "--stdin"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Write JSON to stdin then close it so git sees EOF.
    {
        let stdin = child.stdin.as_mut().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "failed to open git stdin")
        })?;
        stdin.write_all(&json)?;
    } // stdin is dropped here, signalling EOF

    let output = child.wait_with_output()?;

    if !output.status.success() {
        return Err(OpLogWriteError::HashObject {
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            exit_code: output.status.code(),
        });
    }

    // 3. Parse the OID from stdout.
    let raw = String::from_utf8_lossy(&output.stdout);
    let oid_str = raw.trim();

    GitOid::new(oid_str).map_err(|_| OpLogWriteError::InvalidOid {
        raw: oid_str.to_owned(),
    })
}

// ---------------------------------------------------------------------------
// High-level append
// ---------------------------------------------------------------------------

/// Write an operation blob and atomically advance the workspace head ref.
///
/// This is the primary entry point for recording an operation to the log.
/// It performs two steps:
///
/// 1. Call [`write_operation_blob`] to store the operation as a git blob.
/// 2. Update `refs/manifold/head/<workspace>` via compare-and-swap:
///    - If `old_head` is `None` (first operation ever), use the zero OID as
///      the expected old value so git only succeeds if the ref doesn't exist.
///    - If `old_head` is `Some(oid)`, use that OID as the CAS guard.
///
/// Returns the new operation blob OID on success.
///
/// # CAS and the single-writer invariant
///
/// Each workspace is written to by exactly one agent at a time (§5.3).
/// The CAS is therefore a safety net: it should always succeed. A
/// [`OpLogWriteError::CasMismatch`] indicates a broken invariant.
///
/// # Arguments
/// * `root` — absolute path to the git repository root.
/// * `workspace_id` — the workspace whose log is being extended.
/// * `op` — the operation to append.
/// * `old_head` — the current head ref value (`None` for the first operation).
///
/// # Errors
/// Returns an error if the blob write fails, if the ref update fails,
/// or if the CAS guard is violated.
#[allow(clippy::missing_panics_doc)]
pub fn append_operation(
    root: &Path,
    workspace_id: &WorkspaceId,
    op: &Operation,
    old_head: Option<&GitOid>,
) -> Result<GitOid, OpLogWriteError> {
    // Step 1: write the blob.
    let new_oid = write_operation_blob(root, op)?;

    // Step 2: update the head ref atomically.
    let ref_name = manifold_refs::workspace_head_ref(workspace_id.as_str());

    let result = old_head.map_or_else(
        || {
            // First operation: the ref must not yet exist.
            // Use the zero OID as the expected old value.
            let zero = GitOid::new(&"0".repeat(40)).expect("zero OID is valid");
            manifold_refs::write_ref_cas(root, &ref_name, &zero, &new_oid)
        },
        |old_oid| {
            // Subsequent operations: CAS from old → new.
            manifold_refs::write_ref_cas(root, &ref_name, old_oid, &new_oid)
        },
    );

    result.map_err(|e| match e {
        manifold_refs::RefError::CasMismatch { .. } => OpLogWriteError::CasMismatch {
            workspace_id: workspace_id.clone(),
        },
        other => OpLogWriteError::RefError(other),
    })?;

    Ok(new_oid)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::{EpochId, WorkspaceId};
    use crate::oplog::types::{OpPayload, Operation};
    use crate::refs::{read_ref, workspace_head_ref};
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Create a fresh git repo with one commit.
    fn setup_repo() -> (TempDir, GitOid) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(root)
            .output()
            .unwrap();

        fs::write(root.join("README.md"), "# Test\n").unwrap();
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(root)
            .output()
            .unwrap();

        let out = Command::new("git")
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

    // -----------------------------------------------------------------------
    // write_operation_blob
    // -----------------------------------------------------------------------

    #[test]
    fn write_blob_returns_valid_oid() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("agent-1");
        let op = make_create_op(&ws_id);

        let oid = write_operation_blob(root, &op).unwrap();
        // OID should be a valid 40-char hex string
        assert_eq!(oid.as_str().len(), 40);
        assert!(oid
            .as_str()
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn write_blob_is_readable_with_cat_file() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("agent-1");
        let op = make_create_op(&ws_id);

        let oid = write_operation_blob(root, &op).unwrap();

        // `git cat-file -p <oid>` should succeed and return valid JSON
        let out = Command::new("git")
            .args(["cat-file", "-p", oid.as_str()])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git cat-file should succeed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let json_str = String::from_utf8_lossy(&out.stdout);
        // Should parse back to the original operation
        let parsed = Operation::from_json(json_str.as_bytes()).unwrap();
        assert_eq!(parsed, op);
    }

    #[test]
    fn write_blob_is_deterministic() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("agent-1");
        let op = make_create_op(&ws_id);

        let oid1 = write_operation_blob(root, &op).unwrap();
        let oid2 = write_operation_blob(root, &op).unwrap();
        assert_eq!(
            oid1, oid2,
            "same operation must produce the same blob OID (content-addressed)"
        );
    }

    #[test]
    fn write_blob_different_ops_have_different_oids() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("agent-1");

        let op1 = make_create_op(&ws_id);
        let op2 = Operation {
            parent_ids: vec![],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T12:00:00Z".to_owned(),
            payload: OpPayload::Create { epoch: epoch('b') },
        };

        let oid1 = write_operation_blob(root, &op1).unwrap();
        let oid2 = write_operation_blob(root, &op2).unwrap();
        assert_ne!(
            oid1, oid2,
            "different operations must produce different OIDs"
        );
    }

    // -----------------------------------------------------------------------
    // append_operation — first operation (no prior head)
    // -----------------------------------------------------------------------

    #[test]
    fn append_first_op_creates_head_ref() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("agent-1");
        let op = make_create_op(&ws_id);

        let oid = append_operation(root, &ws_id, &op, None).unwrap();

        // The head ref should now point to the blob OID
        let ref_name = workspace_head_ref(ws_id.as_str());
        let head = read_ref(root, &ref_name).unwrap();
        assert_eq!(head, Some(oid));
    }

    #[test]
    fn append_first_op_ref_name_is_correct() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("feature-x");
        let op = make_create_op(&ws_id);

        let oid = append_operation(root, &ws_id, &op, None).unwrap();

        // Verify via git show-ref
        let out = Command::new("git")
            .args(["show-ref", "refs/manifold/head/feature-x"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "show-ref should find the ref: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let output_str = String::from_utf8_lossy(&out.stdout);
        assert!(output_str.contains(oid.as_str()));
    }

    #[test]
    fn append_first_op_fails_if_ref_exists() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("agent-1");
        let op = make_create_op(&ws_id);

        // Create the ref manually to simulate "already exists"
        let oid1 = append_operation(root, &ws_id, &op, None).unwrap();

        // Try to append again with old_head=None (should fail: ref now exists)
        let result = append_operation(root, &ws_id, &op, None);
        assert!(
            matches!(result, Err(OpLogWriteError::CasMismatch { .. })),
            "appending with old_head=None when ref exists should fail with CasMismatch: {result:?}"
        );

        // Ref should still point to the original OID
        let ref_name = workspace_head_ref(ws_id.as_str());
        let head = read_ref(root, &ref_name).unwrap();
        assert_eq!(head, Some(oid1));
    }

    // -----------------------------------------------------------------------
    // append_operation — subsequent operations (with prior head)
    // -----------------------------------------------------------------------

    #[test]
    fn append_second_op_advances_head() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("agent-1");

        // First operation
        let op1 = make_create_op(&ws_id);
        let oid1 = append_operation(root, &ws_id, &op1, None).unwrap();

        // Second operation (parent = oid1)
        let op2 = make_describe_op(&ws_id, oid1.clone(), "implementing feature");
        let oid2 = append_operation(root, &ws_id, &op2, Some(&oid1)).unwrap();

        // Head should now point to oid2
        let ref_name = workspace_head_ref(ws_id.as_str());
        let head = read_ref(root, &ref_name).unwrap();
        assert_eq!(head, Some(oid2));
    }

    #[test]
    fn append_chain_of_three_ops() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("agent-1");

        let op1 = make_create_op(&ws_id);
        let oid1 = append_operation(root, &ws_id, &op1, None).unwrap();

        let op2 = make_describe_op(&ws_id, oid1.clone(), "step 2");
        let oid2 = append_operation(root, &ws_id, &op2, Some(&oid1)).unwrap();

        let op3 = make_describe_op(&ws_id, oid2.clone(), "step 3");
        let oid3 = append_operation(root, &ws_id, &op3, Some(&oid2)).unwrap();

        // Head should now be oid3
        let ref_name = workspace_head_ref(ws_id.as_str());
        let head = read_ref(root, &ref_name).unwrap();
        assert_eq!(head, Some(oid3));

        // Previous blobs are still accessible
        let out = Command::new("git")
            .args(["cat-file", "-t", oid1.as_str()])
            .current_dir(root)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "blob");

        let out = Command::new("git")
            .args(["cat-file", "-t", oid2.as_str()])
            .current_dir(root)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "blob");
    }

    #[test]
    fn cas_mismatch_on_wrong_old_head() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("agent-1");

        // First op
        let op1 = make_create_op(&ws_id);
        let oid1 = append_operation(root, &ws_id, &op1, None).unwrap();

        // Second op (advances head to oid2)
        let op2 = make_describe_op(&ws_id, oid1.clone(), "step 2");
        let oid2 = append_operation(root, &ws_id, &op2, Some(&oid1)).unwrap();

        // Try to append a third op using stale old_head (oid1 instead of oid2)
        let op3 = make_describe_op(&ws_id, oid2.clone(), "step 3");
        let result = append_operation(root, &ws_id, &op3, Some(&oid1));
        assert!(
            matches!(result, Err(OpLogWriteError::CasMismatch { .. })),
            "stale old_head should produce CasMismatch: {result:?}"
        );

        // Head should still be oid2
        let ref_name = workspace_head_ref(ws_id.as_str());
        let head = read_ref(root, &ref_name).unwrap();
        assert_eq!(head, Some(oid2));
    }

    // -----------------------------------------------------------------------
    // blob content verification
    // -----------------------------------------------------------------------

    #[test]
    fn blob_content_is_valid_json() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("default");
        let op = make_create_op(&ws_id);

        let oid = write_operation_blob(root, &op).unwrap();

        let out = Command::new("git")
            .args(["cat-file", "-p", oid.as_str()])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(out.status.success());

        // Content should be valid JSON
        let json_bytes = out.stdout.clone();
        let value: serde_json::Value =
            serde_json::from_slice(&json_bytes).expect("blob content should be valid JSON");

        // Should have the expected top-level keys
        assert!(value.get("workspace_id").is_some());
        assert!(value.get("parent_ids").is_some());
        assert!(value.get("timestamp").is_some());
        assert!(value.get("payload").is_some());
    }

    #[test]
    fn blob_content_round_trips_through_json() {
        let (dir, _) = setup_repo();
        let root = dir.path();
        let ws_id = ws("default");

        // Use a complex operation with multiple parent IDs
        let op = Operation {
            parent_ids: vec![
                GitOid::new(&"a".repeat(40)).unwrap(),
                GitOid::new(&"b".repeat(40)).unwrap(),
            ],
            workspace_id: ws_id.clone(),
            timestamp: "2026-02-19T15:30:00Z".to_owned(),
            payload: OpPayload::Describe {
                message: "implementing the feature\nwith a multiline description".to_owned(),
            },
        };

        let oid = write_operation_blob(root, &op).unwrap();

        let out = Command::new("git")
            .args(["cat-file", "-p", oid.as_str()])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(out.status.success());

        let parsed = Operation::from_json(&out.stdout).unwrap();
        assert_eq!(parsed, op);
    }

    // -----------------------------------------------------------------------
    // Error display
    // -----------------------------------------------------------------------

    #[test]
    fn error_display_cas_mismatch() {
        let err = OpLogWriteError::CasMismatch {
            workspace_id: ws("agent-1"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("agent-1"));
        assert!(msg.contains("single-writer invariant"));
        assert!(msg.contains("CAS mismatch"));
    }

    #[test]
    fn error_display_hash_object() {
        let err = OpLogWriteError::HashObject {
            stderr: "fatal: not a git repo".to_owned(),
            exit_code: Some(128),
        };
        let msg = format!("{err}");
        assert!(msg.contains("hash-object"));
        assert!(msg.contains("128"));
        assert!(msg.contains("fatal: not a git repo"));
    }

    #[test]
    fn error_display_invalid_oid() {
        let err = OpLogWriteError::InvalidOid {
            raw: "not-a-sha".to_owned(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("invalid OID"));
        assert!(msg.contains("not-a-sha"));
    }

    #[test]
    fn error_display_io() {
        let err = OpLogWriteError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "git not found",
        ));
        let msg = format!("{err}");
        assert!(msg.contains("I/O error"));
        assert!(msg.contains("git not found"));
    }
}
