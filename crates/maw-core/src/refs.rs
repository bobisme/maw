//! Git ref management for Manifold's `refs/manifold/*` namespace.
//!
//! Provides low-level helpers to read, write, atomically update, and delete
//! git refs used by Manifold. All operations delegate to a
//! [`maw_git::GitRepo`] implementation.
//!
//! # Manifold Ref Hierarchy
//!
//! ```text
//! refs/manifold/
//! ├── epoch/
//! │   └── current       ← OID of the current epoch commit
//! ├── head/
//! │   └── <workspace>   ← latest operation OID for each workspace (Phase 2+)
//! └── ws/
//!     └── <workspace>   ← materialized workspace state commit (Level 1 compat)
//! ```
//!
//! # Concurrency
//!
//! [`write_ref_cas`] implements optimistic concurrency control. Git's
//! internal ref locking makes the CAS atomic: if the ref's current value
//! does not match the expected old OID, git rejects the update and the
//! function returns [`RefError::CasMismatch`]. Callers should retry on
//! mismatch.

#![allow(clippy::missing_errors_doc)]

use std::fmt;
use std::path::Path;

use crate::model::types::GitOid;

// ---------------------------------------------------------------------------
// Well-known ref names
// ---------------------------------------------------------------------------

/// The git ref that tracks the current epoch commit.
///
/// Set during `maw init` to epoch₀ (the initial commit), and advanced
/// atomically during epoch promotion.
pub const EPOCH_CURRENT: &str = "refs/manifold/epoch/current";

/// Prefix for per-workspace head refs (used in Phase 2+).
pub const HEAD_PREFIX: &str = "refs/manifold/head/";

/// Prefix for per-workspace materialized state refs (Level 1 compatibility).
pub const WORKSPACE_STATE_PREFIX: &str = "refs/manifold/ws/";

/// Prefix for per-workspace creation epoch refs.
///
/// Stores the epoch a workspace was created at, so that `status()` can
/// distinguish "HEAD advanced because the agent committed" from "HEAD is the
/// epoch" even after the workspace has local commits.
pub const WORKSPACE_EPOCH_PREFIX: &str = "refs/manifold/epoch/ws/";

/// Build the per-workspace head ref name.
///
/// # Example
/// ```
/// assert_eq!(maw_core::refs::workspace_head_ref("default"),
///            "refs/manifold/head/default");
/// ```
#[must_use]
pub fn workspace_head_ref(workspace_name: &str) -> String {
    format!("{HEAD_PREFIX}{workspace_name}")
}

/// Build the per-workspace Level 1 state ref name.
///
/// # Example
/// ```
/// assert_eq!(maw_core::refs::workspace_state_ref("default"),
///            "refs/manifold/ws/default");
/// ```
#[must_use]
pub fn workspace_state_ref(workspace_name: &str) -> String {
    format!("{WORKSPACE_STATE_PREFIX}{workspace_name}")
}

/// Build the per-workspace creation epoch ref name.
///
/// This ref records the epoch a workspace was based on at creation time.
/// Unlike HEAD (which advances when agents commit), this ref stays fixed
/// for the lifetime of the workspace.
///
/// # Example
/// ```
/// assert_eq!(maw_core::refs::workspace_epoch_ref("agent-1"),
///            "refs/manifold/epoch/ws/agent-1");
/// ```
#[must_use]
pub fn workspace_epoch_ref(workspace_name: &str) -> String {
    format!("{WORKSPACE_EPOCH_PREFIX}{workspace_name}")
}

// ---------------------------------------------------------------------------
// OID conversion helpers
// ---------------------------------------------------------------------------

/// Convert a `maw_core` `GitOid` (String-based) to a `maw_git` `GitOid` (byte-based).
fn to_git_oid(oid: &GitOid) -> Result<maw_git::GitOid, RefError> {
    oid.as_str().parse::<maw_git::GitOid>().map_err(|e| RefError::InvalidOid {
        ref_name: String::new(),
        raw_value: e.to_string(),
    })
}

/// Convert a `maw_git` `GitOid` (byte-based) to a `maw_core` `GitOid` (String-based).
fn from_git_oid(oid: maw_git::GitOid) -> Result<GitOid, RefError> {
    let s = oid.to_string();
    GitOid::new(&s).map_err(|_| RefError::InvalidOid {
        ref_name: String::new(),
        raw_value: s,
    })
}

/// Parse a ref name string into a `maw_git::RefName`.
fn to_ref_name(name: &str) -> Result<maw_git::RefName, RefError> {
    maw_git::RefName::new(name).map_err(|e| RefError::GitCommand {
        command: format!("ref name validation: {name}"),
        stderr: e.to_string(),
        exit_code: None,
    })
}

/// Map a `maw_git::GitError` to a `RefError`.
fn map_git_error(name: &str, err: maw_git::GitError) -> RefError {
    match &err {
        maw_git::GitError::RefConflict { ref_name, .. } => RefError::CasMismatch {
            ref_name: ref_name.clone(),
        },
        maw_git::GitError::IoError(e) => RefError::Io(std::io::Error::new(e.kind(), e.to_string())),
        maw_git::GitError::InvalidOid { value, reason } => RefError::InvalidOid {
            ref_name: name.to_owned(),
            raw_value: format!("{value}: {reason}"),
        },
        // gix may report CAS mismatches as BackendError with "should have content"
        maw_git::GitError::BackendError { message }
            if message.contains("should have content")
                || message.contains("cannot lock ref")
                || message.contains("but expected") =>
        {
            RefError::CasMismatch {
                ref_name: name.to_owned(),
            }
        }
        _ => RefError::GitCommand {
            command: name.to_owned(),
            stderr: err.to_string(),
            exit_code: None,
        },
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during ref operations.
#[derive(Debug)]
pub enum RefError {
    /// A git command failed (non-zero exit code).
    GitCommand {
        /// The command that was run (e.g., `"git update-ref ..."`).
        command: String,
        /// Stderr output from git, trimmed.
        stderr: String,
        /// Process exit code, if available.
        exit_code: Option<i32>,
    },
    /// An I/O error spawning git.
    Io(std::io::Error),
    /// Git returned an OID that failed validation.
    InvalidOid {
        /// The ref name that was read.
        ref_name: String,
        /// The raw value returned by git.
        raw_value: String,
    },
    /// CAS failed because the ref's current value differs from `old_oid`.
    ///
    /// The caller should re-read the ref and retry, or bail out.
    CasMismatch {
        /// The ref that could not be updated.
        ref_name: String,
    },
}

impl fmt::Display for RefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitCommand {
                command,
                stderr,
                exit_code,
            } => {
                write!(f, "`{command}` failed")?;
                if let Some(code) = exit_code {
                    write!(f, " (exit code {code})")?;
                }
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            Self::Io(e) => write!(f, "I/O error spawning git: {e}"),
            Self::InvalidOid {
                ref_name,
                raw_value,
            } => {
                write!(
                    f,
                    "invalid OID from `{ref_name}`: {raw_value:?} \
                     (expected 40 lowercase hex characters)"
                )
            }
            Self::CasMismatch { ref_name } => {
                write!(
                    f,
                    "CAS failed for `{ref_name}`: ref was modified concurrently — \
                     read the current value and retry"
                )
            }
        }
    }
}

impl std::error::Error for RefError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        if let Self::Io(e) = self {
            Some(e)
        } else {
            None
        }
    }
}

impl From<std::io::Error> for RefError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Read a git ref and return its OID, or `None` if it does not exist.
///
/// Uses [`maw_git::GitRepo::rev_parse_opt`] for general revspecs (like `HEAD`)
/// and [`maw_git::GitRepo::read_ref`] for proper ref names.
///
/// # Errors
/// Returns an error if the operation fails for a reason other than a missing
/// ref, or if the returned OID is malformed.
pub fn read_ref(root: &Path, name: &str) -> Result<Option<GitOid>, RefError> {
    let repo = open_repo(root)?;
    read_ref_via(&*repo, name)
}

/// Read a git ref via a `GitRepo` trait object.
pub fn read_ref_via(repo: &dyn maw_git::GitRepo, name: &str) -> Result<Option<GitOid>, RefError> {
    // Try as a proper ref name first (refs/... or HEAD etc.)
    if let Ok(ref_name) = to_ref_name(name) {
        match repo.read_ref(&ref_name) {
            Ok(Some(oid)) => return Ok(Some(from_git_oid(oid)?)),
            Ok(None) | Err(maw_git::GitError::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(map_git_error(name, e)),
        }
    }
    // Fall back to rev_parse_opt for arbitrary revspecs
    match repo.rev_parse_opt(name) {
        Ok(Some(oid)) => Ok(Some(from_git_oid(oid)?)),
        Ok(None) => Ok(None),
        Err(e) => Err(map_git_error(name, e)),
    }
}

/// Write (create or overwrite) a git ref unconditionally.
///
/// For safe concurrent updates, use [`write_ref_cas`] instead.
///
/// # Errors
/// Returns an error if the operation fails.
pub fn write_ref(root: &Path, name: &str, oid: &GitOid) -> Result<(), RefError> {
    let repo = open_repo(root)?;
    write_ref_via(&*repo, name, oid)
}

/// Write a git ref via a `GitRepo` trait object.
pub fn write_ref_via(repo: &dyn maw_git::GitRepo, name: &str, oid: &GitOid) -> Result<(), RefError> {
    let ref_name = to_ref_name(name)?;
    let git_oid = to_git_oid(oid)?;
    repo.write_ref(&ref_name, git_oid, "")
        .map_err(|e| map_git_error(name, e))
}

/// Atomically update a git ref using compare-and-swap (CAS).
///
/// The update succeeds only if the ref's current value matches `old_oid`.
/// If it does not match, [`RefError::CasMismatch`] is returned.
///
/// # Concurrency
/// This is the correct primitive for epoch advancement in multi-agent
/// scenarios. Each agent reads the current epoch, does its work, then
/// tries to advance with CAS. If another agent advanced first, the CAS
/// fails and the agent must re-read and retry.
///
/// # Creating a ref that must not exist
/// Pass the zero OID (`0000000000000000000000000000000000000000`) as
/// `old_oid` to succeed only if the ref does not currently exist.
///
/// # Errors
/// - [`RefError::CasMismatch`] — ref was modified concurrently.
/// - [`RefError::GitCommand`] — other git failure.
/// - [`RefError::Io`] — git could not be spawned.
pub fn write_ref_cas(
    root: &Path,
    name: &str,
    old_oid: &GitOid,
    new_oid: &GitOid,
) -> Result<(), RefError> {
    let repo = open_repo(root)?;
    write_ref_cas_via(&*repo, name, old_oid, new_oid)
}

/// CAS update a git ref via a `GitRepo` trait object.
pub fn write_ref_cas_via(
    repo: &dyn maw_git::GitRepo,
    name: &str,
    old_oid: &GitOid,
    new_oid: &GitOid,
) -> Result<(), RefError> {
    let ref_name = to_ref_name(name)?;
    let old = to_git_oid(old_oid)?;
    let new = to_git_oid(new_oid)?;

    // When old_oid is zero (create-only semantics), verify the ref doesn't
    // exist first. gix's MustNotExist may not reliably reject updates to
    // existing refs in all storage backends.
    if old.is_zero()
        && let Ok(Some(_)) = repo.read_ref(&ref_name) {
            return Err(RefError::CasMismatch {
                ref_name: name.to_owned(),
            });
        }

    let edit = maw_git::RefEdit {
        name: ref_name,
        new_oid: new,
        expected_old_oid: old,
    };
    repo.atomic_ref_update(&[edit])
        .map_err(|e| map_git_error(name, e))
}

/// Atomically update multiple refs.
///
/// Each entry is a `(ref_name, old_oid, new_oid)` tuple. All updates are
/// applied in a single transaction: either every ref moves or none does.
///
/// # CAS semantics
/// Each update includes the expected old OID. If any ref's current value
/// does not match its expected old OID, the entire transaction is aborted
/// and [`RefError::CasMismatch`] is returned.
///
/// # Errors
/// - [`RefError::CasMismatch`] — a ref was modified concurrently.
/// - [`RefError::GitCommand`] — other git failure.
/// - [`RefError::Io`] — git could not be spawned or stdin write failed.
pub fn update_refs_atomic(
    root: &Path,
    updates: &[(&str, &GitOid, &GitOid)],
) -> Result<(), RefError> {
    let repo = open_repo(root)?;
    update_refs_atomic_via(&*repo, updates)
}

/// Atomically update multiple refs via a `GitRepo` trait object.
pub fn update_refs_atomic_via(
    repo: &dyn maw_git::GitRepo,
    updates: &[(&str, &GitOid, &GitOid)],
) -> Result<(), RefError> {
    let edits: Vec<maw_git::RefEdit> = updates
        .iter()
        .map(|(name, old_oid, new_oid)| {
            Ok(maw_git::RefEdit {
                name: to_ref_name(name)?,
                new_oid: to_git_oid(new_oid)?,
                expected_old_oid: to_git_oid(old_oid)?,
            })
        })
        .collect::<Result<Vec<_>, RefError>>()?;

    repo.atomic_ref_update(&edits).map_err(|e| {
        // Try to identify which ref failed
        let first_ref = updates.first().map_or("unknown", |u| u.0);
        map_git_error(first_ref, e)
    })
}

/// Delete a git ref.
///
/// Idempotent: if the ref does not exist, this is a no-op.
///
/// # Errors
/// Returns an error if the operation fails for a reason other than the
/// ref already being absent.
pub fn delete_ref(root: &Path, name: &str) -> Result<(), RefError> {
    let repo = open_repo(root)?;
    delete_ref_via(&*repo, name)
}

/// Delete a git ref via a `GitRepo` trait object.
///
/// Idempotent: if the ref does not exist, this is a no-op.
pub fn delete_ref_via(repo: &dyn maw_git::GitRepo, name: &str) -> Result<(), RefError> {
    let ref_name = to_ref_name(name)?;
    match repo.delete_ref(&ref_name) {
        Ok(()) | Err(maw_git::GitError::NotFound { .. }) => Ok(()),
        Err(e) => Err(map_git_error(name, e)),
    }
}

// ---------------------------------------------------------------------------
// Convenience wrappers for Manifold-specific refs
// ---------------------------------------------------------------------------

/// Read `refs/manifold/epoch/current`.
///
/// Returns `None` if the ref has not been set (e.g., before `maw init`).
pub fn read_epoch_current(root: &Path) -> Result<Option<GitOid>, RefError> {
    read_ref(root, EPOCH_CURRENT)
}

/// Write `refs/manifold/epoch/current` unconditionally.
///
/// Used during `maw init` to set the initial epoch₀.
pub fn write_epoch_current(root: &Path, oid: &GitOid) -> Result<(), RefError> {
    write_ref(root, EPOCH_CURRENT, oid)
}

/// Advance `refs/manifold/epoch/current` from `old_epoch` to `new_epoch` via CAS.
///
/// Returns [`RefError::CasMismatch`] if another agent advanced the epoch first.
pub fn advance_epoch(root: &Path, old_epoch: &GitOid, new_epoch: &GitOid) -> Result<(), RefError> {
    write_ref_cas(root, EPOCH_CURRENT, old_epoch, new_epoch)
}

// ---------------------------------------------------------------------------
// Repo opening helper
// ---------------------------------------------------------------------------

/// Open a `GixRepo` for the given root path.
fn open_repo(root: &Path) -> Result<Box<dyn maw_git::GitRepo>, RefError> {
    maw_git::GixRepo::open(root)
        .map(|r| Box::new(r) as Box<dyn maw_git::GitRepo>)
        .map_err(|e| RefError::GitCommand {
            command: format!("open repo at {}", root.display()),
            stderr: e.to_string(),
            exit_code: None,
        })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Create a fresh git repo with one commit and return the HEAD OID.
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

    /// Create a second commit in the repo and return its OID.
    fn add_commit(root: &std::path::Path) -> GitOid {
        fs::write(root.join("extra.txt"), "extra\n").unwrap();
        Command::new("git")
            .args(["add", "extra.txt"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "second"])
            .current_dir(root)
            .output()
            .unwrap();

        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        let oid_str = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        GitOid::new(&oid_str).unwrap()
    }

    // -----------------------------------------------------------------------
    // workspace_head_ref
    // -----------------------------------------------------------------------

    #[test]
    fn workspace_head_ref_format() {
        assert_eq!(workspace_head_ref("default"), "refs/manifold/head/default");
        assert_eq!(workspace_head_ref("agent-1"), "refs/manifold/head/agent-1");
    }

    #[test]
    fn workspace_state_ref_format() {
        assert_eq!(workspace_state_ref("default"), "refs/manifold/ws/default");
        assert_eq!(workspace_state_ref("agent-1"), "refs/manifold/ws/agent-1");
    }

    #[test]
    fn workspace_epoch_ref_format() {
        assert_eq!(
            workspace_epoch_ref("agent-1"),
            "refs/manifold/epoch/ws/agent-1"
        );
        assert_eq!(
            workspace_epoch_ref("default"),
            "refs/manifold/epoch/ws/default"
        );
    }

    // -----------------------------------------------------------------------
    // read_ref
    // -----------------------------------------------------------------------

    #[test]
    fn read_ref_existing() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        // Write a known ref
        Command::new("git")
            .args(["update-ref", "refs/manifold/epoch/current", oid.as_str()])
            .current_dir(root)
            .output()
            .unwrap();

        let result = read_ref(root, "refs/manifold/epoch/current").unwrap();
        assert_eq!(result, Some(oid));
    }

    #[test]
    fn read_ref_missing_returns_none() {
        let (dir, _oid) = setup_repo();
        let root = dir.path();

        let result = read_ref(root, "refs/manifold/does-not-exist").unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn read_ref_head() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        // HEAD always exists
        let result = read_ref(root, "HEAD").unwrap();
        assert_eq!(result, Some(oid));
    }

    // -----------------------------------------------------------------------
    // write_ref
    // -----------------------------------------------------------------------

    #[test]
    fn write_ref_creates_new() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        write_ref(root, EPOCH_CURRENT, &oid).unwrap();

        let result = read_ref(root, EPOCH_CURRENT).unwrap();
        assert_eq!(result, Some(oid));
    }

    #[test]
    fn write_ref_overwrites_existing() {
        let (dir, first_oid) = setup_repo();
        let root = dir.path();
        let second_oid = add_commit(root);

        write_ref(root, EPOCH_CURRENT, &first_oid).unwrap();
        write_ref(root, EPOCH_CURRENT, &second_oid).unwrap();

        let result = read_ref(root, EPOCH_CURRENT).unwrap();
        assert_eq!(result, Some(second_oid));
    }

    // -----------------------------------------------------------------------
    // write_ref_cas
    // -----------------------------------------------------------------------

    #[test]
    fn write_ref_cas_succeeds_with_correct_old_value() {
        let (dir, first_oid) = setup_repo();
        let root = dir.path();
        let second_oid = add_commit(root);

        // Set initial value
        write_ref(root, EPOCH_CURRENT, &first_oid).unwrap();

        // CAS from first → second
        write_ref_cas(root, EPOCH_CURRENT, &first_oid, &second_oid).unwrap();

        let result = read_ref(root, EPOCH_CURRENT).unwrap();
        assert_eq!(result, Some(second_oid));
    }

    #[test]
    fn write_ref_cas_fails_with_wrong_old_value() {
        let (dir, first_oid) = setup_repo();
        let root = dir.path();
        let second_oid = add_commit(root);
        let third_oid = add_commit(root);

        // Set to second_oid
        write_ref(root, EPOCH_CURRENT, &second_oid).unwrap();

        // Try CAS with first_oid as expected old (wrong!)
        let err = write_ref_cas(root, EPOCH_CURRENT, &first_oid, &third_oid).unwrap_err();
        assert!(
            matches!(err, RefError::CasMismatch { .. }),
            "expected CasMismatch, got: {err}"
        );

        // Ref should still be second_oid
        let result = read_ref(root, EPOCH_CURRENT).unwrap();
        assert_eq!(result, Some(second_oid));
    }

    #[test]
    fn write_ref_cas_prevents_concurrent_advance() {
        // Simulate two agents racing to advance epoch.
        // Agent A reads epoch=v1, advances to v2.
        // Agent B reads epoch=v1 (stale), tries to advance to v3.
        // Agent B's CAS should fail because epoch is now v2.
        let (dir, v1) = setup_repo();
        let root = dir.path();
        let v2 = add_commit(root);
        let v3 = add_commit(root);

        write_ref(root, EPOCH_CURRENT, &v1).unwrap();

        // Agent A advances v1 → v2 (succeeds)
        write_ref_cas(root, EPOCH_CURRENT, &v1, &v2).unwrap();

        // Agent B tries v1 → v3 (fails, current is v2)
        let err = write_ref_cas(root, EPOCH_CURRENT, &v1, &v3).unwrap_err();
        assert!(
            matches!(err, RefError::CasMismatch { .. }),
            "agent B should lose the race: {err}"
        );

        // Epoch stayed at v2
        let result = read_ref(root, EPOCH_CURRENT).unwrap();
        assert_eq!(result, Some(v2));
    }

    // -----------------------------------------------------------------------
    // delete_ref
    // -----------------------------------------------------------------------

    #[test]
    fn delete_ref_removes_existing() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        write_ref(root, EPOCH_CURRENT, &oid).unwrap();
        assert!(read_ref(root, EPOCH_CURRENT).unwrap().is_some());

        delete_ref(root, EPOCH_CURRENT).unwrap();
        assert!(read_ref(root, EPOCH_CURRENT).unwrap().is_none());
    }

    #[test]
    fn delete_ref_idempotent() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        write_ref(root, EPOCH_CURRENT, &oid).unwrap();
        delete_ref(root, EPOCH_CURRENT).unwrap();
        // Deleting again should not error
        delete_ref(root, EPOCH_CURRENT).unwrap();
    }

    #[test]
    fn delete_ref_missing_is_noop() {
        let (dir, _) = setup_repo();
        let root = dir.path();

        // Should not error even if the ref never existed
        delete_ref(root, "refs/manifold/nonexistent").unwrap();
    }

    // -----------------------------------------------------------------------
    // Convenience wrappers
    // -----------------------------------------------------------------------

    #[test]
    fn read_epoch_current_missing() {
        let (dir, _) = setup_repo();
        assert!(read_epoch_current(dir.path()).unwrap().is_none());
    }

    #[test]
    fn write_and_read_epoch_current() {
        let (dir, oid) = setup_repo();
        let root = dir.path();

        write_epoch_current(root, &oid).unwrap();
        let result = read_epoch_current(root).unwrap();
        assert_eq!(result, Some(oid));
    }

    #[test]
    fn advance_epoch_happy_path() {
        let (dir, v1) = setup_repo();
        let root = dir.path();
        let v2 = add_commit(root);

        write_epoch_current(root, &v1).unwrap();
        advance_epoch(root, &v1, &v2).unwrap();

        assert_eq!(read_epoch_current(root).unwrap(), Some(v2));
    }

    #[test]
    fn advance_epoch_stale_fails() {
        let (dir, v1) = setup_repo();
        let root = dir.path();
        let v2 = add_commit(root);
        let v3 = add_commit(root);

        write_epoch_current(root, &v2).unwrap();

        // Try to advance from v1 (stale) to v3 — should fail
        let err = advance_epoch(root, &v1, &v3).unwrap_err();
        assert!(
            matches!(err, RefError::CasMismatch { .. }),
            "expected CasMismatch: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // update_refs_atomic
    // -----------------------------------------------------------------------

    #[test]
    fn update_refs_atomic_moves_both_refs() {
        let (dir, v1) = setup_repo();
        let root = dir.path();
        let v2 = add_commit(root);

        write_ref(root, EPOCH_CURRENT, &v1).unwrap();
        write_ref(root, "refs/heads/test-branch", &v1).unwrap();

        update_refs_atomic(
            root,
            &[
                (EPOCH_CURRENT, &v1, &v2),
                ("refs/heads/test-branch", &v1, &v2),
            ],
        )
        .unwrap();

        assert_eq!(read_ref(root, EPOCH_CURRENT).unwrap(), Some(v2.clone()));
        assert_eq!(
            read_ref(root, "refs/heads/test-branch").unwrap(),
            Some(v2)
        );
    }

    #[test]
    fn update_refs_atomic_fails_if_any_ref_stale() {
        let (dir, v1) = setup_repo();
        let root = dir.path();
        let v2 = add_commit(root);
        let v3 = add_commit(root);

        // Set epoch to v2, branch to v1
        write_ref(root, EPOCH_CURRENT, &v2).unwrap();
        write_ref(root, "refs/heads/test-branch", &v1).unwrap();

        // Try atomic update expecting epoch=v1 (wrong!) and branch=v1
        let err = update_refs_atomic(
            root,
            &[
                (EPOCH_CURRENT, &v1, &v3),
                ("refs/heads/test-branch", &v1, &v3),
            ],
        )
        .unwrap_err();

        assert!(
            matches!(err, RefError::CasMismatch { .. }),
            "expected CasMismatch, got: {err}"
        );

        // Neither ref should have moved
        assert_eq!(read_ref(root, EPOCH_CURRENT).unwrap(), Some(v2));
        assert_eq!(
            read_ref(root, "refs/heads/test-branch").unwrap(),
            Some(v1)
        );
    }

    #[test]
    fn update_refs_atomic_single_ref() {
        let (dir, v1) = setup_repo();
        let root = dir.path();
        let v2 = add_commit(root);

        write_ref(root, EPOCH_CURRENT, &v1).unwrap();

        update_refs_atomic(root, &[(EPOCH_CURRENT, &v1, &v2)]).unwrap();

        assert_eq!(read_ref(root, EPOCH_CURRENT).unwrap(), Some(v2));
    }

    // -----------------------------------------------------------------------
    // Error display
    // -----------------------------------------------------------------------

    #[test]
    fn error_display_git_command() {
        let err = RefError::GitCommand {
            command: "git update-ref refs/manifold/epoch/current abc123".to_owned(),
            stderr: "fatal: bad object".to_owned(),
            exit_code: Some(128),
        };
        let msg = format!("{err}");
        assert!(msg.contains("git update-ref"));
        assert!(msg.contains("128"));
        assert!(msg.contains("fatal: bad object"));
    }

    #[test]
    fn error_display_cas_mismatch() {
        let err = RefError::CasMismatch {
            ref_name: "refs/manifold/epoch/current".to_owned(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("CAS failed"));
        assert!(msg.contains("refs/manifold/epoch/current"));
        assert!(msg.contains("concurrently"));
    }

    #[test]
    fn error_display_invalid_oid() {
        let err = RefError::InvalidOid {
            ref_name: "refs/manifold/epoch/current".to_owned(),
            raw_value: "garbage".to_owned(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("invalid OID"));
        assert!(msg.contains("garbage"));
    }
}
