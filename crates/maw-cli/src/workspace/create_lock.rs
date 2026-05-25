//! Advisory filesystem lock used to make `maw ws create <name>` atomic for a
//! given workspace name (bn-3bbc).
//!
//! Without this, `maw ws create` does a classic TOCTOU: it checks whether the
//! workspace path exists, then (if not) calls the backend to `git worktree
//! add` and writes metadata. Under a real race — e.g. an orchestrator
//! dispatching the same workspace name to two agents, or an agent retrying a
//! create — every caller passes the existence check before any of them has
//! finished creating the worktree. They then all run `worktree add` against
//! the *same* path, clobbering each other (`remove_dir_all` in the git
//! backend), and every caller prints the full success banner and exits 0.
//! Depending on timing the workspace can be left `MISSING` and `maw doctor`
//! fails.
//!
//! # Mechanism
//!
//! We open (create-if-missing, no truncate) a per-workspace-name lockfile
//! under `<repo_root>/.manifold/locks/create/<ws_name>.lock` and take a
//! **blocking** exclusive lock on it via [`fs4::fs_std::FileExt`]. This is
//! the same `.manifold/locks/` infrastructure used by `maw ws sync --rebase`
//! (see `sync/lock.rs`); the only difference is that create *blocks* rather
//! than fails-fast: concurrent same-name creates serialize, so exactly one
//! caller wins, performs the real create, and the losers — once they acquire
//! the lock — re-run the existence check (now true) and fail fast with a
//! clear `workspace '<name>' already exists` error instead of a false
//! success banner.
//!
//! The lock is *per workspace name*, so concurrent creates of *different*
//! names never block each other.
//!
//! The OS releases the advisory lock automatically when the file handle
//! closes, so the lock is released on every exit path — success, early
//! error return, or panic — via the RAII `Drop` of the held `File`. This is
//! crash-safe: there is no stale lockfile to clean up.
//!
//! The lockfile itself is intentionally not deleted on drop. A stale *file*
//! (empty, no process holding it) is harmless — the next acquirer reopens it
//! and the lock succeeds cleanly. Deleting on drop would introduce a narrow
//! race where acquirer A releases, B opens the old inode, A's unlinker
//! removes the directory entry — B still holds the lock on a now-nameless
//! inode, and C can create a fresh file and lock it independently. Leaving
//! the file in place keeps the lock identity stable across acquisitions.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;

/// RAII handle for a workspace-name create lock.
///
/// Held for the duration of the create critical section (existence check +
/// backend `worktree add` + metadata write + success banner). Dropping it
/// (on success, error, or panic) closes the file descriptor, which releases
/// the advisory flock held by the kernel.
pub(super) struct WorkspaceCreateLock {
    /// The locked file. Kept alive for the full critical section — when it
    /// drops, the kernel releases the advisory lock.
    _file: File,
    /// Path retained for diagnostics / tests.
    #[allow(dead_code)]
    path: PathBuf,
}

impl WorkspaceCreateLock {
    /// Acquire the create lock for `ws_name` under `root`, blocking until it
    /// is available.
    ///
    /// Blocking (rather than try-and-fail) is deliberate: when two callers
    /// race to create the same name, the loser should *wait* for the winner
    /// to finish and then observe that the workspace now exists, reporting a
    /// clean `already exists` error — not bail out with a transient
    /// "lock busy" message.
    ///
    /// # Errors
    ///
    /// Returns an error if the lock directory cannot be created, the
    /// lockfile cannot be opened, or the kernel reports an unexpected
    /// failure while taking the lock.
    pub(super) fn acquire(root: &Path, ws_name: &str) -> io::Result<Self> {
        let lock_dir = maw_core::model::layout::LayoutFlavor::detect_with_env(root)
            .manifold_dir(root)
            .join("locks")
            .join("create");
        std::fs::create_dir_all(&lock_dir)?;
        let path = lock_dir.join(format!("{ws_name}.lock"));

        // `create(true)` + `truncate(false)` — create the file if missing,
        // otherwise open as-is. The file content is never used; only the
        // advisory lock on the inode matters.
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        // Blocking exclusive lock. Released automatically when `file` drops.
        FileExt::lock_exclusive(&file)?;

        Ok(Self { _file: file, path })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn acquire_succeeds_on_fresh_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let guard = WorkspaceCreateLock::acquire(tmp.path(), "feat").expect("acquire");
        drop(guard);
    }

    #[test]
    fn lockfile_path_is_stable_and_persisted() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let guard = WorkspaceCreateLock::acquire(tmp.path(), "feat").expect("acquire");
        let expected = tmp
            .path()
            .join(".manifold")
            .join("locks")
            .join("create")
            .join("feat.lock");
        assert_eq!(guard.path, expected);
        drop(guard);
        assert!(
            expected.exists(),
            "lockfile is intentionally left in place after release"
        );
    }

    #[test]
    fn different_names_do_not_block_each_other() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let alice = WorkspaceCreateLock::acquire(tmp.path(), "alice").expect("alice");
        // Acquiring a *different* name while holding `alice` must not block.
        let bob = WorkspaceCreateLock::acquire(tmp.path(), "bob").expect("bob");
        drop(alice);
        drop(bob);
    }

    /// The core serialization property: many threads racing to acquire the
    /// same-name lock are serialized — only one is ever inside the critical
    /// section at a time. We assert the max observed concurrency is exactly
    /// 1, which is what makes same-name `maw ws create` atomic.
    #[test]
    fn same_name_acquires_are_mutually_exclusive() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let in_section = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..16 {
            let root = root.clone();
            let in_section = Arc::clone(&in_section);
            let max_seen = Arc::clone(&max_seen);
            handles.push(std::thread::spawn(move || {
                let guard = WorkspaceCreateLock::acquire(&root, "dupe").expect("acquire");
                let now = in_section.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(now, Ordering::SeqCst);
                // Hold the section briefly so overlap would be observable
                // if the lock were not mutually exclusive.
                std::thread::sleep(std::time::Duration::from_millis(5));
                in_section.fetch_sub(1, Ordering::SeqCst);
                drop(guard);
            }));
        }
        for h in handles {
            h.join().expect("thread join");
        }

        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "same-name create lock must serialize: never more than one holder at a time"
        );
    }
}
