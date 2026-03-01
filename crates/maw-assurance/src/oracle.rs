//! Invariant oracle for the DST harness.
//!
//! Provides six check functions (`check_g1` through `check_g6`) that verify
//! maw's safety guarantees hold across state transitions. Each check takes
//! pre- and/or post-operation snapshots of repository state and returns
//! `Ok(())` if the invariant holds, or `Err(AssuranceViolation)` describing
//! the violation.
//!
//! These checks are designed to be called thousands of times per DST run.
//! Each individual check must complete in under 1 second.
//!
//! # Guarantees checked
//!
//! | Check | Guarantee | Description |
//! |-------|-----------|-------------|
//! | G1 | Committed no-loss | Pre-op OIDs remain reachable post-op |
//! | G2 | Rewrite preservation | Changed workspace HEADs have recovery refs or were clean |
//! | G3 | Post-COMMIT monotonicity | Epoch ref only advances forward |
//! | G4 | Destructive gate | Destroyed workspaces have recovery refs |
//! | G5 | Discoverable recovery | All recovery refs can be listed |
//! | G6 | Searchable recovery | Recovery refs point to valid commits |

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// State types
// ---------------------------------------------------------------------------

/// Snapshot of repository state at a point in time.
///
/// Captured before and after operations to enable invariant checking.
/// The fields correspond to the durable surfaces that maw's guarantees
/// are defined over.
#[derive(Clone, Debug)]
pub struct AssuranceState {
    /// Absolute path to the repository root.
    pub repo_root: PathBuf,
    /// All durable refs (`ref_name` -> OID hex string).
    /// Includes `refs/heads/*`, `refs/manifold/*`, etc.
    pub durable_refs: HashMap<String, String>,
    /// Recovery refs under `refs/manifold/recovery/` (`ref_name` -> OID).
    /// This is a subset of `durable_refs`, extracted for convenience.
    pub recovery_refs: HashMap<String, String>,
    /// Per-workspace status snapshots.
    pub workspaces: HashMap<String, WorkspaceStatus>,
    /// Current merge-state phase, if `merge-state.json` exists.
    pub merge_state_phase: Option<String>,
}

/// Status snapshot for a single workspace.
#[derive(Clone, Debug)]
pub struct WorkspaceStatus {
    /// The HEAD commit OID of this workspace.
    pub head_oid: String,
    /// Whether the workspace has uncommitted changes.
    pub is_dirty: bool,
    /// Whether the workspace directory exists on disk.
    pub exists: bool,
}

// ---------------------------------------------------------------------------
// Violation type
// ---------------------------------------------------------------------------

/// Describes a specific invariant violation detected by an oracle check.
#[derive(Debug)]
pub enum AssuranceViolation {
    /// G1: A commit that was reachable before the operation is no longer
    /// reachable from any durable or recovery ref.
    ReachabilityLost {
        /// The OID that became unreachable.
        oid: String,
        /// The ref that previously made it reachable.
        previous_ref: String,
    },

    /// G2: A workspace HEAD changed but no recovery ref was created and
    /// the workspace was not proven clean.
    RewriteNotPreserved {
        /// The workspace whose HEAD changed without preservation.
        workspace: String,
        /// The old HEAD OID.
        old_head: String,
        /// The new HEAD OID.
        new_head: String,
    },

    /// G3: The epoch ref moved backwards or to a non-descendant commit
    /// after a successful COMMIT phase.
    CommitMonotonicityBroken {
        /// The epoch OID before the operation.
        pre_epoch: String,
        /// The epoch OID after the operation.
        post_epoch: String,
    },

    /// G4: A workspace was destroyed without a corresponding recovery ref.
    DestructiveWithoutRecovery {
        /// The workspace that was destroyed without recovery.
        workspace: String,
        /// The HEAD OID the workspace had before destruction.
        last_head: String,
    },

    /// G5: A recovery ref exists in the ref database but could not be listed
    /// or resolved.
    RecoveryNotDiscoverable {
        /// The recovery ref that could not be discovered.
        ref_name: String,
        /// What went wrong.
        reason: String,
    },

    /// G6: A recovery ref points to an object that is not a valid, readable
    /// commit.
    RecoveryNotSearchable {
        /// The recovery ref with an invalid target.
        ref_name: String,
        /// The OID the ref points to.
        oid: String,
        /// What went wrong when trying to read the commit.
        reason: String,
    },

    /// A git subprocess failed unexpectedly during a check.
    GitError {
        /// Which check was running.
        check: String,
        /// The git command that failed.
        command: String,
        /// Stderr from the command.
        stderr: String,
    },
}

impl fmt::Display for AssuranceViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReachabilityLost { oid, previous_ref } => {
                write!(
                    f,
                    "G1 violation: OID {oid} was reachable via {previous_ref} \
                     before the operation but is no longer reachable from any \
                     durable or recovery ref"
                )
            }
            Self::RewriteNotPreserved {
                workspace,
                old_head,
                new_head,
            } => {
                write!(
                    f,
                    "G2 violation: workspace '{workspace}' HEAD changed from \
                     {old_head} to {new_head} without a recovery ref or \
                     clean-workspace proof"
                )
            }
            Self::CommitMonotonicityBroken {
                pre_epoch,
                post_epoch,
            } => {
                write!(
                    f,
                    "G3 violation: epoch moved from {pre_epoch} to {post_epoch} \
                     but the new epoch is not a descendant of the old one"
                )
            }
            Self::DestructiveWithoutRecovery {
                workspace,
                last_head,
            } => {
                write!(
                    f,
                    "G4 violation: workspace '{workspace}' (HEAD {last_head}) was \
                     destroyed without a corresponding recovery ref"
                )
            }
            Self::RecoveryNotDiscoverable { ref_name, reason } => {
                write!(
                    f,
                    "G5 violation: recovery ref '{ref_name}' is not discoverable: \
                     {reason}"
                )
            }
            Self::RecoveryNotSearchable {
                ref_name,
                oid,
                reason,
            } => {
                write!(
                    f,
                    "G6 violation: recovery ref '{ref_name}' -> {oid} is not a \
                     valid readable commit: {reason}"
                )
            }
            Self::GitError {
                check,
                command,
                stderr,
            } => {
                write!(
                    f,
                    "Git error during {check}: `{command}` failed: {stderr}"
                )
            }
        }
    }
}

impl std::error::Error for AssuranceViolation {}

// ---------------------------------------------------------------------------
// State capture
// ---------------------------------------------------------------------------

/// Snapshot the current repository state for invariant checking.
///
/// Reads all refs, identifies recovery refs, checks workspace directories,
/// and reads merge-state phase if present.
///
/// # Errors
/// Returns an error if git commands fail or the repo root is invalid.
pub fn capture_state(root: &Path) -> Result<AssuranceState, AssuranceViolation> {
    let durable_refs = read_all_refs(root)?;

    let recovery_refs: HashMap<String, String> = durable_refs
        .iter()
        .filter(|(k, _)| k.starts_with("refs/manifold/recovery/"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let workspaces = discover_workspaces(root, &durable_refs);

    let merge_state_phase = read_merge_state_phase(root);

    Ok(AssuranceState {
        repo_root: root.to_path_buf(),
        durable_refs,
        recovery_refs,
        workspaces,
        merge_state_phase,
    })
}

/// Read all refs from the repository.
fn read_all_refs(root: &Path) -> Result<HashMap<String, String>, AssuranceViolation> {
    let output = Command::new("git")
        .args(["for-each-ref", "--format=%(refname) %(objectname)"])
        .current_dir(root)
        .output()
        .map_err(|e| AssuranceViolation::GitError {
            check: "capture_state".to_owned(),
            command: "git for-each-ref".to_owned(),
            stderr: e.to_string(),
        })?;

    if !output.status.success() {
        return Err(AssuranceViolation::GitError {
            check: "capture_state".to_owned(),
            command: "git for-each-ref".to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut refs = HashMap::new();
    for line in stdout.lines() {
        if let Some((ref_name, oid)) = line.split_once(' ') {
            refs.insert(ref_name.to_owned(), oid.to_owned());
        }
    }
    Ok(refs)
}

/// Discover workspaces by looking at `ws/` subdirectories and HEAD refs.
fn discover_workspaces(
    root: &Path,
    refs: &HashMap<String, String>,
) -> HashMap<String, WorkspaceStatus> {
    let mut workspaces = HashMap::new();
    let ws_dir = root.join("ws");

    if ws_dir.is_dir() && let Ok(entries) = std::fs::read_dir(&ws_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let ws_path = entry.path();

            if !ws_path.is_dir() {
                continue;
            }

            let head_oid = read_workspace_head(root, &ws_path);
            let is_dirty = check_workspace_dirty(&ws_path);

            workspaces.insert(
                name,
                WorkspaceStatus {
                    head_oid: head_oid.unwrap_or_default(),
                    is_dirty,
                    exists: true,
                },
            );
        }
    }

    // Also check for workspace head refs that might exist without a directory
    for ref_name in refs.keys() {
        if let Some(ws_name) = ref_name.strip_prefix("refs/manifold/head/") {
            workspaces.entry(ws_name.to_owned()).or_insert(WorkspaceStatus {
                head_oid: refs.get(ref_name).cloned().unwrap_or_default(),
                is_dirty: false,
                exists: false,
            });
        }
    }

    workspaces
}

/// Read the HEAD OID for a workspace directory.
fn read_workspace_head(repo_root: &Path, ws_path: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(ws_path)
        .env("GIT_DIR", repo_root.join(".git"))
        .output()
        .ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        None
    }
}

/// Check if a workspace has dirty (uncommitted) changes.
fn check_workspace_dirty(ws_path: &Path) -> bool {
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

/// Read the merge-state phase from `.manifold/merge-state.json`.
fn read_merge_state_phase(root: &Path) -> Option<String> {
    let state_path = root.join(".manifold").join("merge-state.json");
    let content = std::fs::read_to_string(state_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    value.get("phase")?.as_str().map(ToOwned::to_owned)
}

// ---------------------------------------------------------------------------
// Check functions
// ---------------------------------------------------------------------------

/// **G1 — Committed no-loss**: every OID reachable via durable refs before
/// the operation must still be reachable via durable or recovery refs after.
///
/// Uses `git merge-base --is-ancestor` to verify that each pre-state ref
/// target is an ancestor of (or equal to) at least one post-state ref target.
///
/// # Errors
/// Returns `AssuranceViolation::ReachabilityLost` if any pre-op OID is no
/// longer reachable.
pub fn check_g1_reachability(
    pre: &AssuranceState,
    post: &AssuranceState,
) -> Result<(), AssuranceViolation> {
    let root = &post.repo_root;

    for (ref_name, pre_oid) in &pre.durable_refs {
        // Check if this OID is reachable from any post-state ref
        let reachable = is_oid_reachable(root, pre_oid, &post.durable_refs)?;
        if !reachable {
            return Err(AssuranceViolation::ReachabilityLost {
                oid: pre_oid.clone(),
                previous_ref: ref_name.clone(),
            });
        }
    }

    Ok(())
}

/// Check if an OID is an ancestor of any ref target in the given ref set.
fn is_oid_reachable(
    root: &Path,
    oid: &str,
    refs: &HashMap<String, String>,
) -> Result<bool, AssuranceViolation> {
    // Fast path: if the OID is directly pointed to by any ref, it's reachable.
    if refs.values().any(|v| v == oid) {
        return Ok(true);
    }

    // Slow path: check ancestry against each ref target.
    for ref_oid in refs.values() {
        if ref_oid == oid {
            return Ok(true);
        }
        let output = Command::new("git")
            .args(["merge-base", "--is-ancestor", oid, ref_oid])
            .current_dir(root)
            .output()
            .map_err(|e| AssuranceViolation::GitError {
                check: "check_g1_reachability".to_owned(),
                command: format!("git merge-base --is-ancestor {oid} {ref_oid}"),
                stderr: e.to_string(),
            })?;

        if output.status.success() {
            return Ok(true);
        }
        // Exit code 1 means "not ancestor" — continue checking other refs.
        // Any other exit code is a real error.
        if output.status.code() != Some(1) {
            return Err(AssuranceViolation::GitError {
                check: "check_g1_reachability".to_owned(),
                command: format!("git merge-base --is-ancestor {oid} {ref_oid}"),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
    }

    Ok(false)
}

/// **G2 -- Rewrite preservation**.
///
/// If any workspace HEAD changed between pre and post state, verify that
/// `recovery_refs` contains a ref for that workspace OR the workspace was
/// clean (no dirty files, no-work proof).
///
/// # Errors
/// Returns `AssuranceViolation::RewriteNotPreserved` if a workspace HEAD
/// changed without preservation.
pub fn check_g2_rewrite_preservation(
    pre: &AssuranceState,
    post: &AssuranceState,
) -> Result<(), AssuranceViolation> {
    for (ws_name, pre_ws) in &pre.workspaces {
        // Skip workspaces that didn't exist or weren't tracked
        if pre_ws.head_oid.is_empty() {
            continue;
        }

        let post_ws = post.workspaces.get(ws_name);

        // Check if the HEAD changed (workspace gone entirely counts as changed)
        let head_changed = post_ws.is_none_or(|pw| pw.head_oid != pre_ws.head_oid);

        if !head_changed {
            continue;
        }

        // If the workspace was clean (not dirty) and still exists, this is a
        // no-work proof: the rewrite didn't destroy any user content.
        if !pre_ws.is_dirty && post_ws.is_some() {
            continue;
        }

        // Check for a recovery ref for this workspace
        let has_recovery = post
            .recovery_refs
            .keys()
            .any(|ref_name| {
                // Recovery refs are formatted as:
                // refs/manifold/recovery/<workspace>/<timestamp>
                ref_name
                    .strip_prefix("refs/manifold/recovery/")
                    .is_some_and(|rest| rest.starts_with(&format!("{ws_name}/")))
            });

        if !has_recovery {
            let new_head = post_ws.map_or_else(
                || "<destroyed>".to_owned(),
                |pw| pw.head_oid.clone(),
            );
            return Err(AssuranceViolation::RewriteNotPreserved {
                workspace: ws_name.clone(),
                old_head: pre_ws.head_oid.clone(),
                new_head,
            });
        }
    }

    Ok(())
}

/// **G3 — Post-COMMIT monotonicity**: if `merge-state` phase was "committed"
/// (or later) in the pre-state, the epoch ref in post-state must equal or
/// descend from the pre-state epoch ref.
///
/// This ensures that a successful COMMIT is never undone by subsequent
/// cleanup failures.
///
/// # Errors
/// Returns `AssuranceViolation::CommitMonotonicityBroken` if the epoch
/// moved backwards.
pub fn check_g3_commit_monotonicity(
    pre: &AssuranceState,
    post: &AssuranceState,
) -> Result<(), AssuranceViolation> {
    // Only relevant if pre-state was in or past the COMMIT phase
    let was_committed = pre
        .merge_state_phase
        .as_deref()
        .is_some_and(|phase| matches!(phase, "commit" | "cleanup" | "complete"));

    if !was_committed {
        return Ok(());
    }

    let epoch_ref = "refs/manifold/epoch/current";
    let Some(pre_epoch) = pre.durable_refs.get(epoch_ref) else {
        return Ok(()); // No epoch ref — nothing to check
    };
    let Some(post_epoch) = post.durable_refs.get(epoch_ref) else {
        // Epoch ref disappeared after COMMIT — this is a violation
        return Err(AssuranceViolation::CommitMonotonicityBroken {
            pre_epoch: pre_epoch.clone(),
            post_epoch: "<missing>".to_owned(),
        });
    };

    // Same OID is fine
    if pre_epoch == post_epoch {
        return Ok(());
    }

    // Post epoch must be a descendant of pre epoch
    let output = Command::new("git")
        .args(["merge-base", "--is-ancestor", pre_epoch, post_epoch])
        .current_dir(&post.repo_root)
        .output()
        .map_err(|e| AssuranceViolation::GitError {
            check: "check_g3_commit_monotonicity".to_owned(),
            command: format!(
                "git merge-base --is-ancestor {pre_epoch} {post_epoch}"
            ),
            stderr: e.to_string(),
        })?;

    if output.status.success() {
        Ok(())
    } else {
        Err(AssuranceViolation::CommitMonotonicityBroken {
            pre_epoch: pre_epoch.clone(),
            post_epoch: post_epoch.clone(),
        })
    }
}

/// **G4 — Destructive gate**: any workspace that existed in pre-state but
/// not in post-state must have a corresponding recovery ref.
///
/// # Errors
/// Returns `AssuranceViolation::DestructiveWithoutRecovery` if a workspace
/// was destroyed without a recovery ref.
pub fn check_g4_destructive_gate(
    pre: &AssuranceState,
    post: &AssuranceState,
) -> Result<(), AssuranceViolation> {
    for (ws_name, pre_ws) in &pre.workspaces {
        if !pre_ws.exists {
            continue;
        }

        let still_exists = post
            .workspaces
            .get(ws_name)
            .is_some_and(|pw| pw.exists);

        if still_exists {
            continue;
        }

        // Workspace was destroyed — check for recovery ref
        let has_recovery = post
            .recovery_refs
            .keys()
            .any(|ref_name| {
                ref_name
                    .strip_prefix("refs/manifold/recovery/")
                    .is_some_and(|rest| rest.starts_with(&format!("{ws_name}/")))
            });

        if !has_recovery {
            return Err(AssuranceViolation::DestructiveWithoutRecovery {
                workspace: ws_name.clone(),
                last_head: pre_ws.head_oid.clone(),
            });
        }
    }

    Ok(())
}

/// **G5 — Discoverable recovery**: every recovery ref can be listed
/// (existence check). Verifies that each ref in `recovery_refs` actually
/// resolves to a valid OID via `git rev-parse`.
///
/// Only requires the post-state (checks current repo state).
///
/// # Errors
/// Returns `AssuranceViolation::RecoveryNotDiscoverable` if a recovery ref
/// cannot be resolved.
pub fn check_g5_discoverability(
    post: &AssuranceState,
) -> Result<(), AssuranceViolation> {
    for (ref_name, expected_oid) in &post.recovery_refs {
        let output = Command::new("git")
            .args(["rev-parse", "--verify", ref_name])
            .current_dir(&post.repo_root)
            .output()
            .map_err(|e| AssuranceViolation::GitError {
                check: "check_g5_discoverability".to_owned(),
                command: format!("git rev-parse --verify {ref_name}"),
                stderr: e.to_string(),
            })?;

        if !output.status.success() {
            return Err(AssuranceViolation::RecoveryNotDiscoverable {
                ref_name: ref_name.clone(),
                reason: format!(
                    "git rev-parse --verify failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            });
        }

        let actual_oid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if actual_oid != *expected_oid {
            return Err(AssuranceViolation::RecoveryNotDiscoverable {
                ref_name: ref_name.clone(),
                reason: format!(
                    "expected OID {expected_oid} but got {actual_oid}"
                ),
            });
        }
    }

    Ok(())
}

/// **G6 — Searchable recovery**: recovery refs point to valid, readable
/// commits. Verifies each recovery ref target is a commit object that can
/// be read with `git cat-file -t`.
///
/// Only requires the post-state (checks current repo state).
///
/// # Errors
/// Returns `AssuranceViolation::RecoveryNotSearchable` if a recovery ref
/// points to a non-commit or unreadable object.
pub fn check_g6_searchability(
    post: &AssuranceState,
) -> Result<(), AssuranceViolation> {
    for (ref_name, oid) in &post.recovery_refs {
        let output = Command::new("git")
            .args(["cat-file", "-t", oid])
            .current_dir(&post.repo_root)
            .output()
            .map_err(|e| AssuranceViolation::GitError {
                check: "check_g6_searchability".to_owned(),
                command: format!("git cat-file -t {oid}"),
                stderr: e.to_string(),
            })?;

        if !output.status.success() {
            return Err(AssuranceViolation::RecoveryNotSearchable {
                ref_name: ref_name.clone(),
                oid: oid.clone(),
                reason: format!(
                    "git cat-file -t failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            });
        }

        let obj_type = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if obj_type != "commit" {
            return Err(AssuranceViolation::RecoveryNotSearchable {
                ref_name: ref_name.clone(),
                oid: oid.clone(),
                reason: format!("object is a '{obj_type}', expected 'commit'"),
            });
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Convenience: run all checks
// ---------------------------------------------------------------------------

/// Run all six invariant checks and return the first violation found,
/// or `Ok(())` if all pass.
///
/// For checks that require both pre and post state (G1-G4), both must be
/// provided. G5 and G6 only use `post`.
///
/// # Errors
/// Returns the first `AssuranceViolation` encountered across G1-G6.
pub fn check_all(
    pre: &AssuranceState,
    post: &AssuranceState,
) -> Result<(), AssuranceViolation> {
    check_g1_reachability(pre, post)?;
    check_g2_rewrite_preservation(pre, post)?;
    check_g3_commit_monotonicity(pre, post)?;
    check_g4_destructive_gate(pre, post)?;
    check_g5_discoverability(post)?;
    check_g6_searchability(post)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helpers for building mock states
    // -----------------------------------------------------------------------

    /// Fake OID strings for testing (valid 40-char hex).
    const OID_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OID_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn empty_state() -> AssuranceState {
        AssuranceState {
            repo_root: PathBuf::from("/tmp/test-repo"),
            durable_refs: HashMap::new(),
            recovery_refs: HashMap::new(),
            workspaces: HashMap::new(),
            merge_state_phase: None,
        }
    }

    fn state_with_refs(refs: Vec<(&str, &str)>) -> AssuranceState {
        let durable_refs: HashMap<String, String> = refs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();

        let recovery_refs: HashMap<String, String> = durable_refs
            .iter()
            .filter(|(k, _)| k.starts_with("refs/manifold/recovery/"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        AssuranceState {
            repo_root: PathBuf::from("/tmp/test-repo"),
            durable_refs,
            recovery_refs,
            workspaces: HashMap::new(),
            merge_state_phase: None,
        }
    }

    fn add_workspace(state: &mut AssuranceState, name: &str, head: &str, dirty: bool, exists: bool) {
        state.workspaces.insert(
            name.to_owned(),
            WorkspaceStatus {
                head_oid: head.to_owned(),
                is_dirty: dirty,
                exists,
            },
        );
    }

    // -----------------------------------------------------------------------
    // G2 — Rewrite preservation (mock-based, no git needed)
    // -----------------------------------------------------------------------

    #[test]
    fn g2_pass_no_head_change() {
        let mut pre = state_with_refs(vec![("refs/heads/main", OID_A)]);
        add_workspace(&mut pre, "default", OID_A, false, true);

        let mut post = state_with_refs(vec![("refs/heads/main", OID_A)]);
        add_workspace(&mut post, "default", OID_A, false, true);

        assert!(check_g2_rewrite_preservation(&pre, &post).is_ok());
    }

    #[test]
    fn g2_pass_head_changed_but_clean() {
        let mut pre = state_with_refs(vec![("refs/heads/main", OID_A)]);
        add_workspace(&mut pre, "default", OID_A, false, true);

        let mut post = state_with_refs(vec![("refs/heads/main", OID_B)]);
        add_workspace(&mut post, "default", OID_B, false, true);

        // Clean workspace -> no-work proof -> ok
        assert!(check_g2_rewrite_preservation(&pre, &post).is_ok());
    }

    #[test]
    fn g2_pass_head_changed_dirty_with_recovery() {
        let mut pre = state_with_refs(vec![("refs/heads/main", OID_A)]);
        add_workspace(&mut pre, "alice", OID_A, true, true);

        let mut post = state_with_refs(vec![
            ("refs/heads/main", OID_B),
            ("refs/manifold/recovery/alice/2025-01-01T00-00-00Z", OID_A),
        ]);
        add_workspace(&mut post, "alice", OID_B, false, true);

        assert!(check_g2_rewrite_preservation(&pre, &post).is_ok());
    }

    #[test]
    fn g2_violation_head_changed_dirty_no_recovery() {
        let mut pre = state_with_refs(vec![("refs/heads/main", OID_A)]);
        add_workspace(&mut pre, "alice", OID_A, true, true);

        let mut post = state_with_refs(vec![("refs/heads/main", OID_B)]);
        add_workspace(&mut post, "alice", OID_B, false, true);

        let err = check_g2_rewrite_preservation(&pre, &post).unwrap_err();
        assert!(matches!(err, AssuranceViolation::RewriteNotPreserved { .. }));
        let msg = format!("{err}");
        assert!(msg.contains("alice"));
        assert!(msg.contains("G2 violation"));
    }

    #[test]
    fn g2_pass_workspace_destroyed_with_recovery() {
        let mut pre = state_with_refs(vec![("refs/heads/main", OID_A)]);
        add_workspace(&mut pre, "alice", OID_A, true, true);

        let post = state_with_refs(vec![
            ("refs/heads/main", OID_B),
            ("refs/manifold/recovery/alice/2025-01-01T00-00-00Z", OID_A),
        ]);
        // alice not in post workspaces -> destroyed

        assert!(check_g2_rewrite_preservation(&pre, &post).is_ok());
    }

    #[test]
    fn g2_violation_workspace_destroyed_dirty_no_recovery() {
        let mut pre = state_with_refs(vec![("refs/heads/main", OID_A)]);
        add_workspace(&mut pre, "alice", OID_A, true, true);

        let post = state_with_refs(vec![("refs/heads/main", OID_B)]);
        // alice not in post workspaces and no recovery ref

        let err = check_g2_rewrite_preservation(&pre, &post).unwrap_err();
        assert!(matches!(err, AssuranceViolation::RewriteNotPreserved { .. }));
    }

    // -----------------------------------------------------------------------
    // G3 — Commit monotonicity (mock-based, no git for non-committed phases)
    // -----------------------------------------------------------------------

    #[test]
    fn g3_pass_not_committed() {
        let mut pre = state_with_refs(vec![("refs/manifold/epoch/current", OID_A)]);
        pre.merge_state_phase = Some("prepare".to_owned());

        let post = state_with_refs(vec![("refs/manifold/epoch/current", OID_A)]);

        // Not in commit phase -> always passes
        assert!(check_g3_commit_monotonicity(&pre, &post).is_ok());
    }

    #[test]
    fn g3_pass_same_epoch() {
        let mut pre = state_with_refs(vec![("refs/manifold/epoch/current", OID_A)]);
        pre.merge_state_phase = Some("commit".to_owned());

        let post = state_with_refs(vec![("refs/manifold/epoch/current", OID_A)]);

        assert!(check_g3_commit_monotonicity(&pre, &post).is_ok());
    }

    #[test]
    fn g3_violation_epoch_disappeared() {
        let mut pre = state_with_refs(vec![("refs/manifold/epoch/current", OID_A)]);
        pre.merge_state_phase = Some("commit".to_owned());

        let post = empty_state();

        let err = check_g3_commit_monotonicity(&pre, &post).unwrap_err();
        assert!(matches!(
            err,
            AssuranceViolation::CommitMonotonicityBroken { .. }
        ));
        let msg = format!("{err}");
        assert!(msg.contains("G3 violation"));
        assert!(msg.contains("<missing>"));
    }

    #[test]
    fn g3_pass_no_phase() {
        let pre = state_with_refs(vec![("refs/manifold/epoch/current", OID_A)]);
        // No merge_state_phase -> not committed -> skip
        let post = state_with_refs(vec![("refs/manifold/epoch/current", OID_B)]);

        assert!(check_g3_commit_monotonicity(&pre, &post).is_ok());
    }

    #[test]
    fn g3_pass_cleanup_phase() {
        // cleanup is past commit, so monotonicity should be enforced
        let mut pre = state_with_refs(vec![("refs/manifold/epoch/current", OID_A)]);
        pre.merge_state_phase = Some("cleanup".to_owned());

        let post = state_with_refs(vec![("refs/manifold/epoch/current", OID_A)]);

        assert!(check_g3_commit_monotonicity(&pre, &post).is_ok());
    }

    // -----------------------------------------------------------------------
    // G4 — Destructive gate (mock-based, no git needed)
    // -----------------------------------------------------------------------

    #[test]
    fn g4_pass_no_destruction() {
        let mut pre = state_with_refs(vec![("refs/heads/main", OID_A)]);
        add_workspace(&mut pre, "alice", OID_A, false, true);

        let mut post = state_with_refs(vec![("refs/heads/main", OID_A)]);
        add_workspace(&mut post, "alice", OID_A, false, true);

        assert!(check_g4_destructive_gate(&pre, &post).is_ok());
    }

    #[test]
    fn g4_pass_destroyed_with_recovery() {
        let mut pre = state_with_refs(vec![("refs/heads/main", OID_A)]);
        add_workspace(&mut pre, "alice", OID_A, false, true);

        let post = state_with_refs(vec![
            ("refs/heads/main", OID_A),
            ("refs/manifold/recovery/alice/2025-01-01T00-00-00Z", OID_A),
        ]);
        // alice not in post workspaces -> destroyed

        assert!(check_g4_destructive_gate(&pre, &post).is_ok());
    }

    #[test]
    fn g4_violation_destroyed_without_recovery() {
        let mut pre = state_with_refs(vec![("refs/heads/main", OID_A)]);
        add_workspace(&mut pre, "alice", OID_A, false, true);

        let post = state_with_refs(vec![("refs/heads/main", OID_A)]);
        // alice not in post workspaces and no recovery ref

        let err = check_g4_destructive_gate(&pre, &post).unwrap_err();
        assert!(matches!(
            err,
            AssuranceViolation::DestructiveWithoutRecovery { .. }
        ));
        let msg = format!("{err}");
        assert!(msg.contains("alice"));
        assert!(msg.contains("G4 violation"));
    }

    #[test]
    fn g4_pass_workspace_never_existed() {
        let mut pre = state_with_refs(vec![("refs/heads/main", OID_A)]);
        // Workspace known via ref but exists=false (no directory)
        add_workspace(&mut pre, "ghost", OID_A, false, false);

        let post = state_with_refs(vec![("refs/heads/main", OID_A)]);

        // Ghost workspace didn't exist on disk -> no destruction
        assert!(check_g4_destructive_gate(&pre, &post).is_ok());
    }

    // -----------------------------------------------------------------------
    // G5 — Discoverability (uses real git)
    // -----------------------------------------------------------------------

    #[test]
    fn g5_pass_no_recovery_refs() {
        let post = empty_state();
        assert!(check_g5_discoverability(&post).is_ok());
    }

    #[test]
    fn g5_pass_with_real_git_repo() {
        let dir = setup_test_repo();
        let root = dir.path();
        let head_oid = git_head_oid(root);

        // Create a recovery ref
        git_cmd(root, &[
            "update-ref",
            "refs/manifold/recovery/test/2025-01-01T00-00-00Z",
            &head_oid,
        ]);

        let mut post = empty_state();
        post.repo_root = root.to_path_buf();
        post.recovery_refs.insert(
            "refs/manifold/recovery/test/2025-01-01T00-00-00Z".to_owned(),
            head_oid,
        );

        assert!(check_g5_discoverability(&post).is_ok());
    }

    #[test]
    fn g5_violation_ref_not_resolvable() {
        let dir = setup_test_repo();
        let root = dir.path();

        // Claim a recovery ref exists but don't actually create it
        let mut post = empty_state();
        post.repo_root = root.to_path_buf();
        post.recovery_refs.insert(
            "refs/manifold/recovery/phantom/2025-01-01T00-00-00Z".to_owned(),
            OID_A.to_owned(),
        );

        let err = check_g5_discoverability(&post).unwrap_err();
        assert!(matches!(
            err,
            AssuranceViolation::RecoveryNotDiscoverable { .. }
        ));
        let msg = format!("{err}");
        assert!(msg.contains("G5 violation"));
    }

    // -----------------------------------------------------------------------
    // G6 — Searchability (uses real git)
    // -----------------------------------------------------------------------

    #[test]
    fn g6_pass_no_recovery_refs() {
        let post = empty_state();
        assert!(check_g6_searchability(&post).is_ok());
    }

    #[test]
    fn g6_pass_valid_commit() {
        let dir = setup_test_repo();
        let root = dir.path();
        let head_oid = git_head_oid(root);

        git_cmd(root, &[
            "update-ref",
            "refs/manifold/recovery/test/2025-01-01T00-00-00Z",
            &head_oid,
        ]);

        let mut post = empty_state();
        post.repo_root = root.to_path_buf();
        post.recovery_refs.insert(
            "refs/manifold/recovery/test/2025-01-01T00-00-00Z".to_owned(),
            head_oid,
        );

        assert!(check_g6_searchability(&post).is_ok());
    }

    #[test]
    fn g6_violation_points_to_tree_not_commit() {
        let dir = setup_test_repo();
        let root = dir.path();

        // Get the tree OID of HEAD (not a commit)
        let output = Command::new("git")
            .args(["rev-parse", "HEAD^{tree}"])
            .current_dir(root)
            .output()
            .unwrap();
        let tree_oid = String::from_utf8_lossy(&output.stdout).trim().to_owned();

        // Pin recovery ref to the tree OID
        git_cmd(root, &[
            "update-ref",
            "refs/manifold/recovery/test/2025-01-01T00-00-00Z",
            &tree_oid,
        ]);

        let mut post = empty_state();
        post.repo_root = root.to_path_buf();
        post.recovery_refs.insert(
            "refs/manifold/recovery/test/2025-01-01T00-00-00Z".to_owned(),
            tree_oid,
        );

        let err = check_g6_searchability(&post).unwrap_err();
        assert!(matches!(
            err,
            AssuranceViolation::RecoveryNotSearchable { .. }
        ));
        let msg = format!("{err}");
        assert!(msg.contains("G6 violation"));
        assert!(msg.contains("tree"));
    }

    #[test]
    fn g6_violation_invalid_oid() {
        let dir = setup_test_repo();
        let root = dir.path();

        // Can't pin a ref to a nonexistent object in git, but we can
        // construct a state that claims a recovery ref with a bad OID
        let mut post = empty_state();
        post.repo_root = root.to_path_buf();
        post.recovery_refs.insert(
            "refs/manifold/recovery/test/2025-01-01T00-00-00Z".to_owned(),
            "0000000000000000000000000000000000000000".to_owned(),
        );

        let err = check_g6_searchability(&post).unwrap_err();
        assert!(matches!(
            err,
            AssuranceViolation::RecoveryNotSearchable { .. }
        ));
    }

    // -----------------------------------------------------------------------
    // G1 — Reachability (uses real git)
    // -----------------------------------------------------------------------

    #[test]
    fn g1_pass_same_refs() {
        let dir = setup_test_repo();
        let root = dir.path();
        let head_oid = git_head_oid(root);

        let pre = AssuranceState {
            repo_root: root.to_path_buf(),
            durable_refs: HashMap::from([
                ("refs/heads/main".to_owned(), head_oid),
            ]),
            recovery_refs: HashMap::new(),
            workspaces: HashMap::new(),
            merge_state_phase: None,
        };

        let post = pre.clone();

        assert!(check_g1_reachability(&pre, &post).is_ok());
    }

    #[test]
    fn g1_pass_ref_advanced() {
        let dir = setup_test_repo();
        let root = dir.path();
        let first_oid = git_head_oid(root);

        // Add a second commit
        std::fs::write(root.join("extra.txt"), "extra\n").unwrap();
        git_cmd(root, &["add", "extra.txt"]);
        git_cmd(root, &["commit", "-m", "second"]);
        let second_oid = git_head_oid(root);

        let pre = AssuranceState {
            repo_root: root.to_path_buf(),
            durable_refs: HashMap::from([
                ("refs/heads/main".to_owned(), first_oid),
            ]),
            recovery_refs: HashMap::new(),
            workspaces: HashMap::new(),
            merge_state_phase: None,
        };

        let post = AssuranceState {
            repo_root: root.to_path_buf(),
            durable_refs: HashMap::from([
                ("refs/heads/main".to_owned(), second_oid),
            ]),
            recovery_refs: HashMap::new(),
            workspaces: HashMap::new(),
            merge_state_phase: None,
        };

        // first_oid is ancestor of second_oid -> still reachable
        assert!(check_g1_reachability(&pre, &post).is_ok());
    }

    #[test]
    fn g1_violation_orphaned_oid() {
        let dir = setup_test_repo();
        let root = dir.path();
        let first_oid = git_head_oid(root);

        // Create a second commit on a branch, then delete the branch
        git_cmd(root, &["checkout", "-b", "temp"]);
        std::fs::write(root.join("temp.txt"), "temp\n").unwrap();
        git_cmd(root, &["add", "temp.txt"]);
        git_cmd(root, &["commit", "-m", "temp"]);
        let temp_oid = git_head_oid(root);
        git_cmd(root, &["checkout", "main"]);
        git_cmd(root, &["branch", "-D", "temp"]);

        let pre = AssuranceState {
            repo_root: root.to_path_buf(),
            durable_refs: HashMap::from([
                ("refs/heads/main".to_owned(), first_oid.clone()),
                ("refs/heads/temp".to_owned(), temp_oid),
            ]),
            recovery_refs: HashMap::new(),
            workspaces: HashMap::new(),
            merge_state_phase: None,
        };

        let post = AssuranceState {
            repo_root: root.to_path_buf(),
            durable_refs: HashMap::from([
                ("refs/heads/main".to_owned(), first_oid),
            ]),
            recovery_refs: HashMap::new(),
            workspaces: HashMap::new(),
            merge_state_phase: None,
        };

        // temp_oid is no longer reachable from any post-state ref
        let err = check_g1_reachability(&pre, &post).unwrap_err();
        assert!(matches!(err, AssuranceViolation::ReachabilityLost { .. }));
        let msg = format!("{err}");
        assert!(msg.contains("G1 violation"));
    }

    #[test]
    fn g1_pass_orphaned_oid_saved_by_recovery_ref() {
        let dir = setup_test_repo();
        let root = dir.path();
        let first_oid = git_head_oid(root);

        // Create a second commit on a branch
        git_cmd(root, &["checkout", "-b", "temp"]);
        std::fs::write(root.join("temp.txt"), "temp\n").unwrap();
        git_cmd(root, &["add", "temp.txt"]);
        git_cmd(root, &["commit", "-m", "temp"]);
        let temp_oid = git_head_oid(root);
        git_cmd(root, &["checkout", "main"]);
        git_cmd(root, &["branch", "-D", "temp"]);

        // But pin it via recovery ref
        git_cmd(root, &[
            "update-ref",
            "refs/manifold/recovery/temp/2025-01-01T00-00-00Z",
            &temp_oid,
        ]);

        let pre = AssuranceState {
            repo_root: root.to_path_buf(),
            durable_refs: HashMap::from([
                ("refs/heads/main".to_owned(), first_oid.clone()),
                ("refs/heads/temp".to_owned(), temp_oid.clone()),
            ]),
            recovery_refs: HashMap::new(),
            workspaces: HashMap::new(),
            merge_state_phase: None,
        };

        let post = AssuranceState {
            repo_root: root.to_path_buf(),
            durable_refs: HashMap::from([
                ("refs/heads/main".to_owned(), first_oid),
                (
                    "refs/manifold/recovery/temp/2025-01-01T00-00-00Z".to_owned(),
                    temp_oid.clone(),
                ),
            ]),
            recovery_refs: HashMap::from([(
                "refs/manifold/recovery/temp/2025-01-01T00-00-00Z".to_owned(),
                temp_oid,
            )]),
            workspaces: HashMap::new(),
            merge_state_phase: None,
        };

        // temp_oid is reachable via recovery ref
        assert!(check_g1_reachability(&pre, &post).is_ok());
    }

    // -----------------------------------------------------------------------
    // check_all
    // -----------------------------------------------------------------------

    #[test]
    fn check_all_pass_empty_states() {
        let dir = setup_test_repo();
        let root = dir.path();

        let state = AssuranceState {
            repo_root: root.to_path_buf(),
            durable_refs: HashMap::new(),
            recovery_refs: HashMap::new(),
            workspaces: HashMap::new(),
            merge_state_phase: None,
        };

        assert!(check_all(&state, &state).is_ok());
    }

    // -----------------------------------------------------------------------
    // Display tests
    // -----------------------------------------------------------------------

    #[test]
    fn violation_display_messages() {
        let violations = [AssuranceViolation::ReachabilityLost {
                oid: OID_A.to_owned(),
                previous_ref: "refs/heads/main".to_owned(),
            },
            AssuranceViolation::RewriteNotPreserved {
                workspace: "alice".to_owned(),
                old_head: OID_A.to_owned(),
                new_head: OID_B.to_owned(),
            },
            AssuranceViolation::CommitMonotonicityBroken {
                pre_epoch: OID_A.to_owned(),
                post_epoch: OID_B.to_owned(),
            },
            AssuranceViolation::DestructiveWithoutRecovery {
                workspace: "alice".to_owned(),
                last_head: OID_A.to_owned(),
            },
            AssuranceViolation::RecoveryNotDiscoverable {
                ref_name: "refs/manifold/recovery/test/ts".to_owned(),
                reason: "missing".to_owned(),
            },
            AssuranceViolation::RecoveryNotSearchable {
                ref_name: "refs/manifold/recovery/test/ts".to_owned(),
                oid: OID_A.to_owned(),
                reason: "not a commit".to_owned(),
            },
            AssuranceViolation::GitError {
                check: "test".to_owned(),
                command: "git foo".to_owned(),
                stderr: "bar".to_owned(),
            }];

        let expected_prefixes = [
            "G1 violation", "G2 violation", "G3 violation",
            "G4 violation", "G5 violation", "G6 violation",
            "Git error",
        ];

        for (v, prefix) in violations.iter().zip(expected_prefixes.iter()) {
            let msg = format!("{v}");
            assert!(
                msg.contains(prefix),
                "expected '{prefix}' in: {msg}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test repo helpers (real git)
    // -----------------------------------------------------------------------

    fn setup_test_repo() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        git_cmd(root, &["init"]);
        git_cmd(root, &["config", "user.name", "Test"]);
        git_cmd(root, &["config", "user.email", "test@test.com"]);
        git_cmd(root, &["config", "commit.gpgsign", "false"]);

        std::fs::write(root.join("README.md"), "# Test\n").unwrap();
        git_cmd(root, &["add", "README.md"]);
        git_cmd(root, &["commit", "-m", "initial"]);

        // Ensure branch is named 'main'
        git_cmd(root, &["branch", "-M", "main"]);

        dir
    }

    fn git_cmd(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_head_oid(root: &Path) -> String {
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }
}
