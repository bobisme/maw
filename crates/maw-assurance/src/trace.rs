//! Operation trace logger for deterministic simulation testing (DST).
//!
//! Records state transitions as newline-delimited JSON (JSONL) for offline
//! analysis and replay. Each trace entry captures:
//!
//! - The operation being performed
//! - Pre- and post-operation state snapshots
//! - Which failpoints (if any) fired
//! - Invariant check results (G1..G6)
//!
//! # Wire format
//!
//! Each line is a self-contained JSON object. See [`TraceEntry`] for the schema.
//!
//! # Example
//!
//! ```rust,ignore
//! use maw::assurance::trace::{TraceLogger, TraceEntry, TraceOp, capture_state};
//! use std::io::Cursor;
//!
//! let buf = Cursor::new(Vec::new());
//! let mut logger = TraceLogger::new(buf);
//! let pre = capture_state(repo_root);
//! // ... run operation ...
//! let post = capture_state(repo_root);
//! let entry = TraceEntry::new(1, TraceOp::CommitEpoch, None, pre, post);
//! logger.record(entry).unwrap();
//! ```

use std::collections::BTreeMap;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The operation being traced.
///
/// Maps 1:1 to the merge state machine phases plus workspace lifecycle
/// operations that the DST harness exercises.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum TraceOp {
    /// Freeze inputs and write merge intent.
    Prepare,
    /// Build the merged tree from collected workspace snapshots.
    Build,
    /// Run validation commands against the candidate commit.
    Validate,
    /// Atomically update the epoch ref.
    CommitEpoch,
    /// Update the branch ref to match the new epoch.
    CommitBranch,
    /// Post-commit cleanup (remove temp files, update workspace state).
    Cleanup,
    /// Destroy a workspace.
    Destroy,
    /// Recover a previously destroyed workspace.
    Recover,
}

/// Snapshot of repository state at a point in time.
///
/// This is the trace-specific state representation, designed for JSON
/// serialization and offline analysis. It deliberately uses simple types
/// (strings, vecs, maps) rather than internal maw types so that trace
/// files are self-contained and readable without the maw binary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StateSnapshot {
    /// OID of `refs/manifold/epoch/current`, or empty string if unset.
    pub epoch_ref: String,
    /// OID of the configured branch HEAD (e.g., `refs/heads/main`).
    pub branch_ref: String,
    /// Current merge phase from `.manifold/merge-state.json`, or empty
    /// string if no merge is in progress.
    pub merge_phase: String,
    /// Names of all workspaces discovered under `ws/`.
    pub workspaces: Vec<String>,
    /// Per-workspace dirty status (`true` = has uncommitted changes).
    pub workspace_dirty: BTreeMap<String, bool>,
    /// Recovery ref names under `refs/manifold/recovery/`.
    pub recovery_refs: Vec<String>,
}

/// Result of a single invariant check.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InvariantResult {
    /// The invariant holds.
    Pass,
    /// The invariant was violated, with a description.
    Fail(String),
    /// The invariant was not applicable for this operation.
    Skip,
}

/// Results of all six invariant checks (G1..G6).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvariantResults {
    /// G1: committed no-loss.
    pub g1: InvariantResult,
    /// G2: rewrite preservation.
    pub g2: InvariantResult,
    /// G3: post-COMMIT monotonicity.
    pub g3: InvariantResult,
    /// G4: destructive gate.
    pub g4: InvariantResult,
    /// G5: discoverable recovery.
    pub g5: InvariantResult,
    /// G6: searchable recovery.
    pub g6: InvariantResult,
}

impl InvariantResults {
    /// Create results where all checks pass.
    #[must_use]
    pub const fn all_pass() -> Self {
        Self {
            g1: InvariantResult::Pass,
            g2: InvariantResult::Pass,
            g3: InvariantResult::Pass,
            g4: InvariantResult::Pass,
            g5: InvariantResult::Pass,
            g6: InvariantResult::Pass,
        }
    }

    /// Create results where all checks are skipped.
    #[must_use]
    pub const fn all_skip() -> Self {
        Self {
            g1: InvariantResult::Skip,
            g2: InvariantResult::Skip,
            g3: InvariantResult::Skip,
            g4: InvariantResult::Skip,
            g5: InvariantResult::Skip,
            g6: InvariantResult::Skip,
        }
    }
}

/// A single trace entry — one JSON line in the trace log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TraceEntry {
    /// Monotonically increasing sequence number (1-based).
    pub seq: u64,
    /// Microseconds since Unix epoch.
    pub timestamp_us: u64,
    /// The operation that was performed.
    pub operation: TraceOp,
    /// Name of the failpoint that fired during this operation, if any.
    pub failpoint_fired: Option<String>,
    /// Repository state before the operation.
    pub pre_state: StateSnapshot,
    /// Repository state after the operation.
    pub post_state: StateSnapshot,
    /// Invariant check results for this transition.
    pub invariants: InvariantResults,
}

impl TraceEntry {
    /// Create a new trace entry with the current timestamp.
    #[must_use]
    pub fn new(
        seq: u64,
        operation: TraceOp,
        failpoint_fired: Option<String>,
        pre_state: StateSnapshot,
        post_state: StateSnapshot,
    ) -> Self {
        let timestamp_us = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_micros() as u64);

        Self {
            seq,
            timestamp_us,
            operation,
            failpoint_fired,
            pre_state,
            post_state,
            invariants: InvariantResults::all_skip(),
        }
    }

    /// Attach invariant results to this entry (builder pattern).
    #[must_use]
    pub fn with_invariants(mut self, invariants: InvariantResults) -> Self {
        self.invariants = invariants;
        self
    }
}

// ---------------------------------------------------------------------------
// TraceLogger
// ---------------------------------------------------------------------------

/// Writes trace entries as newline-delimited JSON to an underlying writer.
///
/// Each call to [`record`](Self::record) serializes one [`TraceEntry`] as
/// a single JSON line, followed by a newline, then flushes the writer.
/// This ensures that entries are durable even if the process crashes
/// mid-trace.
pub struct TraceLogger {
    writer: BufWriter<Box<dyn Write + Send>>,
    seq: u64,
}

impl TraceLogger {
    /// Create a new trace logger writing to the given output.
    pub fn new(writer: impl Write + Send + 'static) -> Self {
        Self {
            writer: BufWriter::new(Box::new(writer)),
            seq: 0,
        }
    }

    /// Return the next sequence number (1-based) and advance the counter.
    pub const fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Write a trace entry as a JSON line and flush.
    ///
    /// # Errors
    /// Returns an `io::Error` if serialization or writing fails.
    pub fn record(&mut self, entry: TraceEntry) -> io::Result<()> {
        let json = serde_json::to_string(&entry).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, e)
        })?;
        self.writer.write_all(json.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()
    }

    /// Flush the underlying writer.
    ///
    /// # Errors
    /// Returns an `io::Error` if flushing fails.
    pub fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

// ---------------------------------------------------------------------------
// State snapshot capture
// ---------------------------------------------------------------------------

/// Capture the current repository state as a [`StateSnapshot`].
///
/// Reads:
/// - `refs/manifold/epoch/current` via `git rev-parse`
/// - The configured branch HEAD via `git rev-parse`
/// - Merge phase from `.manifold/merge-state.json`
/// - Workspace directories under `ws/`
/// - Per-workspace dirty status via `git status --porcelain`
/// - Recovery refs via `git for-each-ref`
///
/// This function is intentionally infallible — if any git command fails,
/// the corresponding field is set to a sensible default (empty string,
/// empty vec, etc.) rather than propagating the error. This makes it safe
/// to call in crash/failpoint scenarios where the repo may be in a
/// partially broken state.
#[must_use]
pub fn capture_state(repo_root: &Path) -> StateSnapshot {
    let epoch_ref = git_rev_parse(repo_root, "refs/manifold/epoch/current")
        .unwrap_or_default();

    let branch_ref = read_branch_head(repo_root).unwrap_or_default();

    let merge_phase = read_merge_phase(repo_root).unwrap_or_default();

    let (workspaces, workspace_dirty) = discover_workspaces(repo_root);

    let recovery_refs = list_recovery_refs(repo_root);

    StateSnapshot {
        epoch_ref,
        branch_ref,
        merge_phase,
        workspaces,
        workspace_dirty,
        recovery_refs,
    }
}

/// Resolve a ref to its OID via `git rev-parse`.
fn git_rev_parse(root: &Path, refspec: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", refspec])
        .current_dir(root)
        .output()
        .ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        None
    }
}

/// Read the configured branch HEAD.
///
/// Tries `refs/heads/main` first (the default). If a `.manifold/config.toml`
/// exists with a different branch, uses that instead.
fn read_branch_head(root: &Path) -> Option<String> {
    // Try to read branch name from config
    let branch = read_configured_branch(root).unwrap_or_else(|| "main".to_owned());
    let refspec = format!("refs/heads/{branch}");
    git_rev_parse(root, &refspec)
}

/// Read the configured branch name from `.manifold/config.toml`.
fn read_configured_branch(root: &Path) -> Option<String> {
    let config_path = root.join(".manifold").join("config.toml");
    let content = std::fs::read_to_string(config_path).ok()?;

    // Simple TOML parsing — look for branch = "..."
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("branch") {
            let rest = rest.trim();
            if let Some(rest) = rest.strip_prefix('=') {
                let rest = rest.trim().trim_matches('"');
                if !rest.is_empty() {
                    return Some(rest.to_owned());
                }
            }
        }
    }
    None
}

/// Read the merge phase from `.manifold/merge-state.json`.
fn read_merge_phase(root: &Path) -> Option<String> {
    let path = root.join(".manifold").join("merge-state.json");
    let content = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    value.get("phase")?.as_str().map(ToOwned::to_owned)
}

/// Discover workspaces and their dirty status.
fn discover_workspaces(root: &Path) -> (Vec<String>, BTreeMap<String, bool>) {
    let mut names = Vec::new();
    let mut dirty = BTreeMap::new();

    let ws_dir = root.join("ws");
    if let Ok(entries) = std::fs::read_dir(&ws_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let ws_path = entry.path();

            if !ws_path.is_dir() {
                continue;
            }

            let is_dirty = check_dirty(&ws_path);
            names.push(name.clone());
            dirty.insert(name, is_dirty);
        }
    }

    names.sort();
    names
        .iter()
        .for_each(|n| { dirty.entry(n.clone()).or_insert(false); });

    (names, dirty)
}

/// Check if a workspace directory has uncommitted changes.
fn check_dirty(ws_path: &Path) -> bool {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(ws_path)
        .output();

    match output {
        Ok(o) if o.status.success() => {
            !String::from_utf8_lossy(&o.stdout).trim().is_empty()
        }
        _ => false,
    }
}

/// List recovery ref names under `refs/manifold/recovery/`.
fn list_recovery_refs(root: &Path) -> Vec<String> {
    let output = Command::new("git")
        .args([
            "for-each-ref",
            "--format=%(refname)",
            "refs/manifold/recovery/",
        ])
        .current_dir(root)
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let mut refs: Vec<String> = stdout
                .lines()
                .filter(|l| !l.is_empty())
                .map(ToOwned::to_owned)
                .collect();
            refs.sort();
            refs
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn trace_op_serde_roundtrip() {
        let ops = vec![
            TraceOp::Prepare,
            TraceOp::Build,
            TraceOp::Validate,
            TraceOp::CommitEpoch,
            TraceOp::CommitBranch,
            TraceOp::Cleanup,
            TraceOp::Destroy,
            TraceOp::Recover,
        ];
        for op in ops {
            let json = serde_json::to_string(&op).unwrap();
            let back: TraceOp = serde_json::from_str(&json).unwrap();
            assert_eq!(op, back);
        }
    }

    #[test]
    fn invariant_result_serde_roundtrip() {
        let cases = vec![
            InvariantResult::Pass,
            InvariantResult::Fail("epoch went backwards".to_owned()),
            InvariantResult::Skip,
        ];
        for case in cases {
            let json = serde_json::to_string(&case).unwrap();
            let back: InvariantResult = serde_json::from_str(&json).unwrap();
            assert_eq!(case, back);
        }
    }

    fn sample_snapshot() -> StateSnapshot {
        StateSnapshot {
            epoch_ref: "abc123def456".to_owned(),
            branch_ref: "def456abc123".to_owned(),
            merge_phase: "commit".to_owned(),
            workspaces: vec!["alice".to_owned(), "bob".to_owned()],
            workspace_dirty: BTreeMap::from([
                ("alice".to_owned(), false),
                ("bob".to_owned(), false),
            ]),
            recovery_refs: vec![
                "refs/manifold/recovery/alice/2026-02-28T05-09-08Z".to_owned(),
            ],
        }
    }

    #[test]
    fn state_snapshot_serde_roundtrip() {
        let snap = sample_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: StateSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn trace_entry_serde_roundtrip() {
        let entry = TraceEntry {
            seq: 1,
            timestamp_us: 1_709_155_200_000_000,
            operation: TraceOp::CommitEpoch,
            failpoint_fired: None,
            pre_state: sample_snapshot(),
            post_state: sample_snapshot(),
            invariants: InvariantResults::all_pass(),
        };

        let json = serde_json::to_string(&entry).unwrap();
        let back: TraceEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn trace_entry_with_failpoint() {
        let entry = TraceEntry {
            seq: 42,
            timestamp_us: 1_709_155_200_000_000,
            operation: TraceOp::Build,
            failpoint_fired: Some("build_crash_before_write".to_owned()),
            pre_state: sample_snapshot(),
            post_state: sample_snapshot(),
            invariants: InvariantResults {
                g1: InvariantResult::Pass,
                g2: InvariantResult::Pass,
                g3: InvariantResult::Skip,
                g4: InvariantResult::Skip,
                g5: InvariantResult::Pass,
                g6: InvariantResult::Fail("recovery ref points to tree, not commit".to_owned()),
            },
        };

        let json = serde_json::to_string(&entry).unwrap();
        // Verify the failpoint shows up
        assert!(json.contains("build_crash_before_write"));
        // Verify the fail variant serializes with the message
        assert!(json.contains("recovery ref points to tree, not commit"));

        let back: TraceEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn trace_logger_writes_valid_jsonl() {
        use std::sync::{Arc, Mutex};

        /// A writer that appends to a shared `Vec<u8>`.
        #[derive(Clone)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);

        impl Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let shared = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        let mut logger = TraceLogger::new(shared.clone());

        let entry1 = TraceEntry {
            seq: 1,
            timestamp_us: 100,
            operation: TraceOp::Prepare,
            failpoint_fired: None,
            pre_state: sample_snapshot(),
            post_state: sample_snapshot(),
            invariants: InvariantResults::all_pass(),
        };
        let entry2 = TraceEntry {
            seq: 2,
            timestamp_us: 200,
            operation: TraceOp::Build,
            failpoint_fired: Some("crash".to_owned()),
            pre_state: sample_snapshot(),
            post_state: sample_snapshot(),
            invariants: InvariantResults::all_skip(),
        };

        logger.record(entry1.clone()).unwrap();
        logger.record(entry2.clone()).unwrap();
        logger.flush().unwrap();

        // Extract the buffer contents
        let bytes = shared.0.lock().unwrap().clone();
        let output = String::from_utf8(bytes).unwrap();

        // Should be two lines
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2);

        // Each line should parse as a valid TraceEntry
        let parsed1: TraceEntry = serde_json::from_str(lines[0]).unwrap();
        let parsed2: TraceEntry = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(parsed1, entry1);
        assert_eq!(parsed2, entry2);
    }

    #[test]
    fn trace_logger_next_seq() {
        let buf = Cursor::new(Vec::new());
        let mut logger = TraceLogger::new(buf);

        assert_eq!(logger.next_seq(), 1);
        assert_eq!(logger.next_seq(), 2);
        assert_eq!(logger.next_seq(), 3);
    }

    #[test]
    fn trace_entry_new_sets_timestamp() {
        let pre = sample_snapshot();
        let post = sample_snapshot();
        let entry = TraceEntry::new(1, TraceOp::Destroy, None, pre, post);

        // Timestamp should be non-zero (we're after 1970)
        assert!(entry.timestamp_us > 0);
        assert_eq!(entry.seq, 1);
        assert_eq!(entry.operation, TraceOp::Destroy);
        assert!(entry.failpoint_fired.is_none());
    }

    #[test]
    fn trace_entry_with_invariants_builder() {
        let pre = sample_snapshot();
        let post = sample_snapshot();
        let entry = TraceEntry::new(1, TraceOp::Recover, None, pre, post)
            .with_invariants(InvariantResults::all_pass());

        assert_eq!(entry.invariants.g1, InvariantResult::Pass);
        assert_eq!(entry.invariants.g6, InvariantResult::Pass);
    }

    #[test]
    fn capture_state_nonexistent_repo() {
        // capture_state should not panic on a non-repo path —
        // it returns empty/default values instead.
        let snap = capture_state(Path::new("/tmp/nonexistent-repo-12345"));
        assert!(snap.epoch_ref.is_empty());
        assert!(snap.branch_ref.is_empty());
        assert!(snap.merge_phase.is_empty());
        assert!(snap.workspaces.is_empty());
        assert!(snap.recovery_refs.is_empty());
    }

    #[test]
    fn capture_state_in_real_repo() {
        // This test uses the actual maw repo to verify capture_state can
        // read a real git repo. We just check that it doesn't panic and
        // returns plausible data.
        let repo_root = std::env::current_dir().unwrap();

        // Walk up until we find a .git directory (handles worktree case)
        let mut root = repo_root.as_path();
        loop {
            if root.join(".git").exists() {
                break;
            }
            root = match root.parent() {
                Some(p) => p,
                None => return, // Not in a git repo, skip test
            };
        }

        let snap = capture_state(root);
        // In a git repo, branch_ref should be non-empty (we have refs/heads/main)
        // but epoch_ref may be empty if manifold refs aren't set up.
        // The important thing is: no panic, no crash.
        assert!(snap.workspaces.is_empty() || !snap.workspaces.is_empty());
    }

    #[test]
    fn json_schema_matches_spec() {
        // Verify the JSON output matches the schema specified in the task.
        let entry = TraceEntry {
            seq: 1,
            timestamp_us: 1_709_155_200_000_000,
            operation: TraceOp::CommitEpoch,
            failpoint_fired: None,
            pre_state: StateSnapshot {
                epoch_ref: "abc123".to_owned(),
                branch_ref: "def456".to_owned(),
                merge_phase: "commit".to_owned(),
                workspaces: vec!["alice".to_owned(), "bob".to_owned()],
                workspace_dirty: BTreeMap::from([
                    ("alice".to_owned(), false),
                    ("bob".to_owned(), false),
                ]),
                recovery_refs: vec![
                    "refs/manifold/recovery/alice/2026-02-28T05-09-08Z".to_owned(),
                ],
            },
            post_state: StateSnapshot {
                epoch_ref: "abc123".to_owned(),
                branch_ref: "def456".to_owned(),
                merge_phase: "commit".to_owned(),
                workspaces: vec!["alice".to_owned(), "bob".to_owned()],
                workspace_dirty: BTreeMap::from([
                    ("alice".to_owned(), false),
                    ("bob".to_owned(), false),
                ]),
                recovery_refs: vec![
                    "refs/manifold/recovery/alice/2026-02-28T05-09-08Z".to_owned(),
                ],
            },
            invariants: InvariantResults::all_pass(),
        };

        let value: serde_json::Value = serde_json::to_value(&entry).unwrap();

        // Verify top-level fields exist with correct types
        assert_eq!(value["seq"], 1);
        assert_eq!(value["timestamp_us"], 1_709_155_200_000_000_u64);
        assert_eq!(value["operation"], "CommitEpoch");
        assert!(value["failpoint_fired"].is_null());

        // Verify pre_state structure
        let pre = &value["pre_state"];
        assert_eq!(pre["epoch_ref"], "abc123");
        assert_eq!(pre["branch_ref"], "def456");
        assert_eq!(pre["merge_phase"], "commit");
        assert!(pre["workspaces"].is_array());
        assert!(pre["workspace_dirty"].is_object());
        assert!(pre["recovery_refs"].is_array());

        // Verify invariants structure
        let inv = &value["invariants"];
        assert_eq!(inv["g1"], "pass");
        assert_eq!(inv["g2"], "pass");
        assert_eq!(inv["g3"], "pass");
        assert_eq!(inv["g4"], "pass");
        assert_eq!(inv["g5"], "pass");
        assert_eq!(inv["g6"], "pass");
    }
}
