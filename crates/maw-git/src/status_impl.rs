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
    let platform =
        repo.repo
            .status(gix::progress::Discard)
            .map_err(|e| GitError::BackendError {
                message: e.to_string(),
            })?;

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
            Ok(target) => target.to_string_lossy().into_owned().into_bytes(),
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
