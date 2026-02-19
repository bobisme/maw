//! Git ref management for Manifold's `refs/manifold/*` namespace.
//!
//! Provides low-level helpers to read, write, atomically update, and delete
//! git refs used by Manifold. All operations run `git update-ref` (or
//! `git rev-parse`) in the repository root directory.
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

use std::fmt;
use std::path::Path;
use std::process::Command;

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

/// Build the per-workspace head ref name.
///
/// # Example
/// ```
/// assert_eq!(maw::refs::workspace_head_ref("default"),
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
/// assert_eq!(maw::refs::workspace_state_ref("default"),
///            "refs/manifold/ws/default");
/// ```
#[must_use]
pub fn workspace_state_ref(workspace_name: &str) -> String {
    format!("{WORKSPACE_STATE_PREFIX}{workspace_name}")
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
/// Runs `git rev-parse <name>` in `root`. Returns `None` if the ref is
/// missing (git exits non-zero with "unknown revision or path").
///
/// # Errors
/// Returns an error if git cannot be spawned, if git fails for a reason
/// other than a missing ref, or if the returned OID is malformed.
pub fn read_ref(root: &Path, name: &str) -> Result<Option<GitOid>, RefError> {
    let output = Command::new("git")
        .args(["rev-parse", name])
        .current_dir(root)
        .output()?;

    if output.status.success() {
        let raw = String::from_utf8_lossy(&output.stdout);
        let oid_str = raw.trim();
        let oid = GitOid::new(oid_str).map_err(|_| RefError::InvalidOid {
            ref_name: name.to_owned(),
            raw_value: oid_str.to_owned(),
        })?;
        return Ok(Some(oid));
    }

    // Distinguish "ref not found" from other errors.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr_trimmed = stderr.trim();

    // git rev-parse exits 128 with "unknown revision or path" when the ref
    // does not exist. Treat this as "not found" rather than a hard error.
    if stderr_trimmed.contains("unknown revision")
        || stderr_trimmed.contains("ambiguous argument")
        || stderr_trimmed.contains("not a valid object")
    {
        return Ok(None);
    }

    Err(RefError::GitCommand {
        command: format!("git rev-parse {name}"),
        stderr: stderr_trimmed.to_owned(),
        exit_code: output.status.code(),
    })
}

/// Write (create or overwrite) a git ref unconditionally.
///
/// Runs `git update-ref <name> <oid>`. This is equivalent to
/// `git update-ref <name> <new_oid>` without an old-value guard, so it
/// will succeed regardless of the ref's current value.
///
/// For safe concurrent updates, use [`write_ref_cas`] instead.
///
/// # Errors
/// Returns an error if git cannot be spawned or exits non-zero.
pub fn write_ref(root: &Path, name: &str, oid: &GitOid) -> Result<(), RefError> {
    let output = Command::new("git")
        .args(["update-ref", name, oid.as_str()])
        .current_dir(root)
        .output()?;

    if output.status.success() {
        return Ok(());
    }

    Err(RefError::GitCommand {
        command: format!("git update-ref {name} {}", oid.as_str()),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        exit_code: output.status.code(),
    })
}

/// Atomically update a git ref using compare-and-swap (CAS).
///
/// Runs `git update-ref <name> <new_oid> <old_oid>`. Git internally holds
/// a lock on the ref file during the update. The update succeeds only if
/// the ref's current value matches `old_oid`. If it does not match, git
/// exits non-zero and this function returns [`RefError::CasMismatch`].
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
    let output = Command::new("git")
        .args(["update-ref", name, new_oid.as_str(), old_oid.as_str()])
        .current_dir(root)
        .output()?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr_trimmed = stderr.trim();

    // git update-ref prints "cannot lock ref" or "is at ... not ..." when
    // the old-value check fails (CAS mismatch).
    if stderr_trimmed.contains("cannot lock ref")
        || stderr_trimmed.contains("is at")
        || stderr_trimmed.contains("but expected")
    {
        return Err(RefError::CasMismatch {
            ref_name: name.to_owned(),
        });
    }

    Err(RefError::GitCommand {
        command: format!(
            "git update-ref {name} {} {}",
            new_oid.as_str(),
            old_oid.as_str()
        ),
        stderr: stderr_trimmed.to_owned(),
        exit_code: output.status.code(),
    })
}

/// Delete a git ref.
///
/// Runs `git update-ref -d <name>`. Idempotent: if the ref does not exist,
/// git exits successfully (no-op). If you need to guard against concurrent
/// deletion, use [`write_ref_cas`] with the expected OID followed by
/// `delete_ref`.
///
/// # Errors
/// Returns an error if git cannot be spawned or exits non-zero for a
/// reason other than the ref already being absent.
pub fn delete_ref(root: &Path, name: &str) -> Result<(), RefError> {
    let output = Command::new("git")
        .args(["update-ref", "-d", name])
        .current_dir(root)
        .output()?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr_trimmed = stderr.trim();

    // git update-ref -d exits 0 if the ref doesn't exist (no-op).
    // If it exits non-zero, something else went wrong.
    Err(RefError::GitCommand {
        command: format!("git update-ref -d {name}"),
        stderr: stderr_trimmed.to_owned(),
        exit_code: output.status.code(),
    })
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
        assert_eq!(
            workspace_head_ref("default"),
            "refs/manifold/head/default"
        );
        assert_eq!(
            workspace_head_ref("agent-1"),
            "refs/manifold/head/agent-1"
        );
    }

    #[test]
    fn workspace_state_ref_format() {
        assert_eq!(
            workspace_state_ref("default"),
            "refs/manifold/ws/default"
        );
        assert_eq!(
            workspace_state_ref("agent-1"),
            "refs/manifold/ws/agent-1"
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
