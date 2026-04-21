//! Advisory filesystem lock used to serialize concurrent `maw ws sync --rebase`
//! runs on a single workspace (bn-1d1g).
//!
//! Without this, two `sync --rebase` processes racing on the same workspace
//! both rewrite `HEAD` / the worktree tree, and whichever loses the race
//! aborts mid-pipeline — typically with
//! `set_head failed: ... No such file or directory` — leaving the workspace
//! in a half-rebased state.
//!
//! # Mechanism
//!
//! We open (create-if-missing, no truncate) a per-workspace lockfile under
//! `<repo_root>/.manifold/locks/rebase/<ws_name>.lock` and call
//! `try_lock_exclusive` on it via [`fs4::fs_std::FileExt`]. The OS releases
//! the advisory lock automatically when the file handle closes — crash-safe,
//! no stale lockfile to clean up.
//!
//! The lockfile itself is intentionally not deleted on drop: a stale *file*
//! (empty, no process holding it) is fine — the next acquirer will reopen
//! it and `flock` succeeds cleanly. Deleting on drop would introduce a
//! narrow race where acquirer A releases, B opens (the old inode), A's
//! unlinker removes the directory entry — B still holds the lock on a
//! now-nameless inode, and C can create a fresh file and lock it
//! independently. By leaving the file in place the lock identity stays
//! stable across acquisitions.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;

/// RAII handle for a workspace's rebase lock.
///
/// Held for the duration of the critical section. Dropping it (on success,
/// error, or panic) closes the file descriptor, which releases the advisory
/// flock held by the kernel.
pub(super) struct WorkspaceRebaseLock {
    /// The locked file. Kept alive for the full critical section — when it
    /// drops, the kernel releases the advisory lock.
    _file: File,
    /// Path retained for diagnostics.
    #[allow(dead_code)]
    path: PathBuf,
}

impl WorkspaceRebaseLock {
    /// Try to acquire the rebase lock for `ws_name` under `root`.
    ///
    /// Returns:
    /// * `Ok(Some(guard))` — we now hold the exclusive lock.
    /// * `Ok(None)` — another process currently holds the lock. Caller
    ///   should emit a friendly error and exit.
    /// * `Err(...)` — unexpected I/O error (cannot create the lock dir,
    ///   cannot open the lockfile, etc.).
    pub(super) fn try_acquire(root: &Path, ws_name: &str) -> io::Result<Option<Self>> {
        let lock_dir = root.join(".manifold").join("locks").join("rebase");
        std::fs::create_dir_all(&lock_dir)?;
        let path = lock_dir.join(format!("{ws_name}.lock"));

        // `create(true)` + `truncate(false)` — create the file if missing,
        // otherwise open-as-is. Truncating would be harmless (the file is
        // empty by design) but we avoid it because a shared lock might be
        // added in the future and truncating under a shared reader is not
        // what callers expect.
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { _file: file, path })),
            Err(e) => {
                // `try_lock_exclusive` surfaces `WouldBlock` when the lock
                // is already held. Anything else (EACCES, EIO, ...) is an
                // unexpected I/O failure — propagate it.
                if e.kind() == io::ErrorKind::WouldBlock {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_succeeds_on_fresh_workspace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let guard = WorkspaceRebaseLock::try_acquire(tmp.path(), "feat")
            .expect("io error")
            .expect("expected to get the lock on a fresh workspace");
        drop(guard);
    }

    #[test]
    fn second_acquire_while_holding_is_blocked() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let first = WorkspaceRebaseLock::try_acquire(tmp.path(), "feat")
            .expect("io error")
            .expect("first acquire should succeed");
        let second = WorkspaceRebaseLock::try_acquire(tmp.path(), "feat").expect("io error");
        assert!(
            second.is_none(),
            "second concurrent acquire on the same ws must fail fast"
        );
        drop(first);
        // After release, a fresh acquire should succeed.
        let third = WorkspaceRebaseLock::try_acquire(tmp.path(), "feat")
            .expect("io error")
            .expect("post-release acquire should succeed");
        drop(third);
    }

    #[test]
    fn different_workspaces_do_not_block_each_other() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let alice = WorkspaceRebaseLock::try_acquire(tmp.path(), "alice")
            .expect("io error")
            .expect("alice lock");
        let bob = WorkspaceRebaseLock::try_acquire(tmp.path(), "bob")
            .expect("io error")
            .expect("bob lock");
        drop(alice);
        drop(bob);
    }

    #[test]
    fn lockfile_path_is_stable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let guard = WorkspaceRebaseLock::try_acquire(tmp.path(), "feat")
            .expect("io error")
            .expect("first acquire");
        let expected = tmp
            .path()
            .join(".manifold")
            .join("locks")
            .join("rebase")
            .join("feat.lock");
        assert_eq!(guard.path, expected);
        drop(guard);
        assert!(
            expected.exists(),
            "lockfile is intentionally left in place after release"
        );
    }
}
