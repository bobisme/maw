//! gix-backed status and dirty detection.

use gix::bstr::ByteSlice;
use gix::status::index_worktree::iter::Summary;

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::{FileStatus, StatusEntry};

pub fn is_dirty(repo: &GixRepo) -> Result<bool, GitError> {
    repo.repo.is_dirty().map_err(|e| GitError::BackendError {
        message: e.to_string(),
    })
}

pub fn status(repo: &GixRepo) -> Result<Vec<StatusEntry>, GitError> {
    // Force per-file emission for untracked entries (matches the porcelain
    // `git status --porcelain` behaviour and `git ls-files --others
    // --exclude-standard`). The gix default collapses untracked
    // subdirectories into a single directory entry, which loses the leaf
    // paths the workspace backend needs (bn-p5z5).
    let platform = repo
        .repo
        .status(gix::progress::Discard)
        .map_err(|e| GitError::BackendError {
            message: e.to_string(),
        })?
        .untracked_files(gix::status::UntrackedFiles::Files);

    let iter =
        platform
            .into_index_worktree_iter(Vec::new())
            .map_err(|e| GitError::BackendError {
                message: e.to_string(),
            })?;

    let mut entries = Vec::new();
    for item in iter {
        let item = item.map_err(|e| GitError::BackendError {
            message: e.to_string(),
        })?;
        if let Some(entry) = convert_status_item(&item) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// Net working-tree status **relative to HEAD** — the true `git status
/// --porcelain` semantics, *including staged changes*.
///
/// [`status`] only diffs the index against the worktree, so a path that
/// was staged (`git add`) but whose worktree copy still matches the
/// staged blob is invisible to it. Three merge-critical callers must not
/// miss such changes (Prime Invariant: no committed/staged work is ever
/// lost):
///
/// * [`crate::stash_impl::worktree_state_commit`] — quarantine fix-forward
///   and recovery snapshots; missing a staged fix promotes the *unfixed*
///   tree to the epoch.
/// * [`crate::diff_impl::diff_name_status_pairs`] — the workspace
///   backend's `snapshot()`; a staged-only change would be silently
///   dropped from the merge.
/// * `recover::dest_has_uncommitted` — the `--restore-file` safety gate;
///   under-reporting lets a restore clobber staged work without
///   `--force`.
///
/// Combines the HEAD→index (staged) diff with the index→worktree
/// (unstaged + untracked) diff, then reduces each path to its single net
/// status relative to HEAD.
pub fn status_head_to_worktree(repo: &GixRepo) -> Result<Vec<StatusEntry>, GitError> {
    use std::collections::BTreeMap;

    // The default status platform carries `head_tree: Some(None)`, so
    // `into_iter` emits the HEAD→index (staged) half automatically — it is
    // `into_index_worktree_iter` that deliberately suppresses it.
    let platform = repo
        .repo
        .status(gix::progress::Discard)
        .map_err(|e| GitError::BackendError {
            message: e.to_string(),
        })?
        .untracked_files(gix::status::UntrackedFiles::Files);

    let iter = platform
        .into_iter(Vec::new())
        .map_err(|e| GitError::BackendError {
            message: e.to_string(),
        })?;

    // Per path: (staged HEAD→index status, unstaged index→worktree status).
    let mut acc: BTreeMap<String, (Option<FileStatus>, Option<FileStatus>)> = BTreeMap::new();

    for item in iter {
        let item = item.map_err(|e| GitError::BackendError {
            message: e.to_string(),
        })?;
        match item {
            gix::status::Item::IndexWorktree(iw) => {
                if let Some(entry) = convert_status_item(&iw) {
                    acc.entry(entry.path).or_default().1 = Some(entry.status);
                }
            }
            gix::status::Item::TreeIndex(change) => {
                // A Rewrite is the fusion of a deletion of the source and
                // an addition of the destination; record both so the old
                // path is not lost (matches `git diff --name-status`
                // default, i.e. no `-M`).
                if let gix::diff::index::ChangeRef::Rewrite {
                    source_location,
                    location,
                    ..
                } = &change
                {
                    if let Ok(src) = source_location.to_str() {
                        acc.entry(src.to_owned()).or_default().0 = Some(FileStatus::Deleted);
                    }
                    if let Ok(dst) = location.to_str() {
                        acc.entry(dst.to_owned()).or_default().0 = Some(FileStatus::Added);
                    }
                    continue;
                }
                let status = match &change {
                    gix::diff::index::ChangeRef::Addition { .. } => FileStatus::Added,
                    gix::diff::index::ChangeRef::Deletion { .. } => FileStatus::Deleted,
                    gix::diff::index::ChangeRef::Modification { .. } => FileStatus::Modified,
                    gix::diff::index::ChangeRef::Rewrite { .. } => unreachable!("handled above"),
                };
                let (loc, ..) = change.fields();
                if let Ok(path) = loc.to_str() {
                    acc.entry(path.to_owned()).or_default().0 = Some(status);
                }
            }
        }
    }

    let mut entries = Vec::with_capacity(acc.len());
    for (path, (staged, unstaged)) in acc {
        if let Some(status) = net_head_to_worktree(staged, unstaged) {
            entries.push(StatusEntry { path, status });
        }
    }
    Ok(entries)
}

/// Reduce a path's `(HEAD→index, index→worktree)` status pair to its net
/// status relative to HEAD. `None` means the path is identical to HEAD
/// (e.g. added to the index then deleted from the worktree).
const fn net_head_to_worktree(
    staged: Option<FileStatus>,
    unstaged: Option<FileStatus>,
) -> Option<FileStatus> {
    use FileStatus::{Added, Deleted, Modified};
    match (staged, unstaged) {
        // No staged term ⇒ HEAD == index, so the index→worktree status
        // *is* the HEAD→worktree status (Untracked is pre-mapped to Added
        // by `convert_status_item`). No worktree term ⇒ index == worktree,
        // so the staged status *is* the HEAD→worktree status — the case
        // the plain index→worktree `status` missed. Both collapse to
        // "whichever side is populated".
        (None, x) | (x, None) => x,
        // Added to the index, then removed from the worktree → absent from
        // both HEAD and the worktree: no net change.
        (Some(Added), Some(Deleted)) => None,
        // New vs HEAD regardless of later worktree edits.
        (Some(Added), Some(_)) => Some(Added),
        // Staged-modified or staged-deleted, then deleted in the worktree
        // → gone relative to HEAD.
        (Some(Modified | Deleted), Some(Deleted)) => Some(Deleted),
        // Staged-modified and still present, or staged-deleted but
        // re-created/edited in the worktree → present and differs from HEAD.
        (Some(Modified | Deleted), Some(_)) => Some(Modified),
        // `Renamed`/`Untracked` only arise from the index→worktree half;
        // fall back to the staged classification consumers already expect.
        (Some(other), Some(_)) => Some(other),
    }
}

/// Fast status: only check tracked files (index vs worktree), skip dirwalk.
///
/// This avoids walking the entire directory tree for untracked files, which
/// is the dominant cost in large repos. Returns only modifications, deletions,
/// and type changes to tracked files — no untracked files.
pub fn status_tracked_only(repo: &GixRepo) -> Result<Vec<StatusEntry>, GitError> {
    let platform = repo
        .repo
        .status(gix::progress::Discard)
        .map_err(|e| GitError::BackendError {
            message: e.to_string(),
        })?
        .untracked_files(gix::status::UntrackedFiles::None);

    let iter =
        platform
            .into_index_worktree_iter(Vec::new())
            .map_err(|e| GitError::BackendError {
                message: e.to_string(),
            })?;

    let mut entries = Vec::new();
    for item in iter {
        let item = item.map_err(|e| GitError::BackendError {
            message: e.to_string(),
        })?;
        if let Some(entry) = convert_status_item(&item) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// Count dirty tracked files by reading the index and stat-checking entries.
///
/// Reads the index once, does one `stat()` per entry comparing mtime + size
/// against the cached stat data. On stat mismatch, falls back to hashing the
/// file and comparing to the index entry's blob OID — matching git's behavior
/// and avoiding spurious dirty counts after mtime skew (checkout, touch, FS
/// remount).
///
/// Returns the count of tracked files whose content differs from the index.
#[cfg(unix)]
pub fn count_dirty_tracked(repo: &GixRepo) -> Result<usize, GitError> {
    use std::os::unix::fs::MetadataExt;

    let workdir = repo
        .workdir
        .as_ref()
        .ok_or_else(|| GitError::BackendError {
            message: "repository has no working directory".to_string(),
        })?;

    let index = repo.repo.index().map_err(|e| GitError::BackendError {
        message: format!("failed to read index: {e}"),
    })?;

    let hash_kind = repo.repo.object_hash();

    let mut dirty = 0;
    for entry in index.entries() {
        if entry.stage_raw() != 0 {
            dirty += 1;
            continue;
        }

        let path_bytes = entry.path(&index);
        let Ok(path_str) = std::str::from_utf8(path_bytes) else {
            continue;
        };

        let full_path = workdir.join(path_str);
        let Ok(meta) = std::fs::symlink_metadata(&full_path) else {
            dirty += 1;
            continue;
        };

        let stat = entry.stat;
        let size_matches = meta.len() == u64::from(stat.size);
        let mtime_matches = u32::try_from(meta.mtime()).ok() == Some(stat.mtime.secs)
            && u32::try_from(meta.mtime_nsec()).ok() == Some(stat.mtime.nsecs);

        if size_matches && mtime_matches {
            continue;
        }

        // Stat mismatch — fall back to content hashing to avoid spurious
        // positives when mtime drifted but content is unchanged. This is what
        // `git status` does (and why running it "refreshes" the stat cache).
        if stat_matches_by_content(&full_path, &meta, entry.id, hash_kind) {
            continue;
        }

        dirty += 1;
    }

    Ok(dirty)
}

/// Return true if the file's contents hash to `expected_oid` as a blob.
/// Symlinks hash their link target text. Files that can't be read are
/// treated as mismatched (dirty).
#[cfg(unix)]
fn stat_matches_by_content(
    path: &std::path::Path,
    meta: &std::fs::Metadata,
    expected_oid: gix::hash::ObjectId,
    hash_kind: gix::hash::Kind,
) -> bool {
    let data = if meta.file_type().is_symlink() {
        match std::fs::read_link(path) {
            // Hash the raw bytes of the link target. `to_string_lossy()`
            // would replace non-UTF-8 bytes with U+FFFD, mismatching the
            // stored blob OID and spuriously counting the symlink dirty
            // (same class of bug fixed in `worktree_state_commit`, 6627a3ea).
            Ok(target) => {
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStrExt;
                    target.as_os_str().as_bytes().to_vec()
                }
                #[cfg(not(unix))]
                {
                    target.to_string_lossy().into_owned().into_bytes()
                }
            }
            Err(_) => return false,
        }
    } else {
        match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(_) => return false,
        }
    };

    gix::objs::compute_hash(hash_kind, gix::objs::Kind::Blob, &data)
        .is_ok_and(|actual| actual == expected_oid)
}

fn convert_status_item(item: &gix::status::index_worktree::Item) -> Option<StatusEntry> {
    let summary = item.summary()?;
    let path = item.rela_path().to_str().ok()?.to_owned();

    let status = match summary {
        Summary::Added | Summary::IntentToAdd | Summary::Copied => FileStatus::Added,
        Summary::Modified | Summary::TypeChange | Summary::Conflict => FileStatus::Modified,
        Summary::Removed => FileStatus::Deleted,
        Summary::Renamed => FileStatus::Renamed,
    };

    Some(StatusEntry { path, status })
}

/// List untracked files (paths not in the index and not ignored).
///
/// Walks the working tree applying `.gitignore` rules and returns the
/// repo-relative paths that are not currently tracked.
///
/// Replaces: `git ls-files --others --exclude-standard`.
pub fn list_untracked(repo: &GixRepo) -> Result<Vec<String>, GitError> {
    // Force per-file emission. The gix default (`UntrackedFiles::Collapsed`)
    // collapses an untracked subdirectory into a single directory entry
    // (e.g. `newdir/`), losing the leaf paths — exactly the bug bn-p5z5
    // fixed for `status()`. `git ls-files --others --exclude-standard`
    // always emits individual files. Callers (recovery-snapshot capture in
    // `working_copy.rs`, conflict-marker scan in `resolve.rs`) `fs::copy`
    // each returned path, so a collapsed directory entry would abort the
    // pre-destroy recovery snapshot (Prime Invariant: no work is ever lost).
    let platform = repo
        .repo
        .status(gix::progress::Discard)
        .map_err(|e| GitError::BackendError {
            message: format!("failed to start status: {e}"),
        })?
        .untracked_files(gix::status::UntrackedFiles::Files);

    let iter =
        platform
            .into_index_worktree_iter(Vec::new())
            .map_err(|e| GitError::BackendError {
                message: format!("failed to iterate status: {e}"),
            })?;

    let mut paths = Vec::new();
    for item in iter {
        let item = item.map_err(|e| GitError::BackendError {
            message: format!("status item failed: {e}"),
        })?;
        if let gix::status::index_worktree::Item::DirectoryContents { entry, .. } = item
            && matches!(entry.status, gix::dir::entry::Status::Untracked)
            && let Ok(path) = entry.rela_path.to_str()
        {
            paths.push(path.to_owned());
        }
    }
    Ok(paths)
}

#[cfg(test)]
#[allow(
    clippy::items_after_test_module,
    reason = "pre-existing in-file test mod (bn-p5z5); list_untracked follows below"
)]
mod tests_bn_p5z5 {
    //! Regression tests for the workspace backend gix migration (bn-p5z5).

    use tempfile::TempDir;

    use super::*;
    use crate::test_support::init_test_repo_with_commit;
    use crate::types::FileStatus;

    fn setup_repo() -> (TempDir, crate::GixRepo) {
        // bn-5rdz: use shared test-repo helper instead of inline git CLI.
        let (dir, root, _oid) = init_test_repo_with_commit();
        let repo = crate::GixRepo::open(&root).expect("open repo");
        (dir, repo)
    }

    /// Untracked files inside untracked subdirectories must be emitted as
    /// individual leaf paths, matching `git status --porcelain` and
    /// `git ls-files --others --exclude-standard`. The gix default
    /// (`UntrackedFiles::Collapsed`) reports only the parent directory,
    /// which loses leaf paths the workspace snapshot needs.
    #[test]
    fn untracked_in_subdir_appears_as_added() {
        let (dir, repo) = setup_repo();
        let root = dir.path();
        std::fs::create_dir_all(root.join("created")).expect("mkdir created/");
        std::fs::write(root.join("created/new_0.txt"), "hi").expect("write file");
        let entries = status(&repo).expect("status");
        assert!(
            entries
                .iter()
                .filter(|e| e.status == FileStatus::Added)
                .any(|e| e.path == "created/new_0.txt"),
            "expected per-file untracked emission, got: {entries:#?}",
        );
    }

    /// bn-pfh7 (Prime Invariant): a file that was `git add`-ed but whose
    /// worktree copy still equals the staged blob is invisible to the
    /// plain index→worktree `status()`. `status_head_to_worktree` must
    /// still report it as `Added` — otherwise `worktree_state_commit`
    /// (quarantine fix-forward / recovery snapshots) and `snapshot()`
    /// would silently drop the staged work.
    #[test]
    fn staged_only_addition_visible_to_head_to_worktree() {
        let (dir, repo) = setup_repo();
        let root = dir.path();
        std::fs::write(root.join("staged_new.txt"), "fresh content").expect("write file");
        let _ = crate::test_support::git_capture(root, &["add", "staged_new.txt"]);

        // The bug: plain index↔worktree status does NOT see it.
        let plain = status(&repo).expect("status");
        assert!(
            !plain.iter().any(|e| e.path == "staged_new.txt"),
            "precondition: plain status() must miss staged-only adds (got {plain:#?})",
        );

        // The fix: HEAD→worktree status DOES see it as Added.
        let hw = status_head_to_worktree(&repo).expect("status_head_to_worktree");
        assert!(
            hw.iter()
                .any(|e| e.path == "staged_new.txt" && e.status == FileStatus::Added),
            "staged-only addition must be reported as Added, got: {hw:#?}",
        );
    }

    /// bn-pfh7: a tracked file modified and then `git add`-ed (worktree
    /// == index) must surface as `Modified` via `status_head_to_worktree`.
    #[test]
    fn staged_only_modification_visible_to_head_to_worktree() {
        let (dir, repo) = setup_repo();
        let root = dir.path();
        // README.md is the committed seed file from the shared helper.
        std::fs::write(root.join("README.md"), "totally different body").expect("rewrite seed");
        let _ = crate::test_support::git_capture(root, &["add", "README.md"]);

        let plain = status(&repo).expect("status");
        assert!(
            !plain.iter().any(|e| e.path == "README.md"),
            "precondition: plain status() must miss staged-only mods (got {plain:#?})",
        );

        let hw = status_head_to_worktree(&repo).expect("status_head_to_worktree");
        assert!(
            hw.iter()
                .any(|e| e.path == "README.md" && e.status == FileStatus::Modified),
            "staged-only modification must be reported as Modified, got: {hw:#?}",
        );
    }

    /// bn-pfh7: added to the index then removed from the worktree — the
    /// path is absent from both HEAD and the worktree, so the net status
    /// is "no change" and it must NOT be reported (avoids a spurious
    /// add/delete pair leaking into the merge snapshot).
    #[test]
    fn staged_addition_then_worktree_delete_is_net_none() {
        let (dir, repo) = setup_repo();
        let root = dir.path();
        std::fs::write(root.join("ephemeral.txt"), "x").expect("write file");
        let _ = crate::test_support::git_capture(root, &["add", "ephemeral.txt"]);
        std::fs::remove_file(root.join("ephemeral.txt")).expect("rm file");

        let hw = status_head_to_worktree(&repo).expect("status_head_to_worktree");
        assert!(
            !hw.iter().any(|e| e.path == "ephemeral.txt"),
            "staged-add-then-worktree-delete must net to no change, got: {hw:#?}",
        );
    }

    /// `list_untracked` must emit individual leaf paths inside untracked
    /// subdirectories, matching `git ls-files --others --exclude-standard`.
    /// The gix default (`UntrackedFiles::Collapsed`) reports only the
    /// parent directory (`created/`); the recovery-snapshot capture then
    /// `fs::copy`s that directory path and aborts the pre-destroy snapshot
    /// (Prime Invariant: no work is ever lost).
    #[test]
    fn list_untracked_emits_leaf_paths_in_subdirs() {
        let (dir, repo) = setup_repo();
        let root = dir.path();
        std::fs::create_dir_all(root.join("created/sub")).expect("mkdir created/sub");
        std::fs::write(root.join("created/top.txt"), "a").expect("write top");
        std::fs::write(root.join("created/sub/deep.txt"), "b").expect("write deep");

        let untracked = list_untracked(&repo).expect("list_untracked");

        assert!(
            untracked.iter().any(|p| p == "created/top.txt"),
            "expected leaf 'created/top.txt', got: {untracked:#?}",
        );
        assert!(
            untracked.iter().any(|p| p == "created/sub/deep.txt"),
            "expected nested leaf 'created/sub/deep.txt', got: {untracked:#?}",
        );
        assert!(
            !untracked.iter().any(|p| p == "created"
                || p == "created/"
                || p == "created/sub"
                || p == "created/sub/"),
            "must not collapse to a directory entry, got: {untracked:#?}",
        );
    }
}
