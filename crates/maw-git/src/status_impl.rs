//! gix-backed status and dirty detection.

use gix::bstr::ByteSlice;

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::*;

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
/// Much faster than [`status()`] or [`status_tracked_only()`] for large repos:
/// reads the index once, does one `stat()` per entry comparing mtime + size
/// against the cached stat data. No content hashing, no gix status pipeline.
///
/// Returns the count of tracked files whose on-disk stat differs from the index.
#[cfg(unix)]
pub fn count_dirty_tracked(repo: &GixRepo) -> Result<usize, GitError> {
    use std::os::unix::fs::MetadataExt;

    let workdir = repo.workdir.as_ref().ok_or_else(|| GitError::BackendError {
        message: "repository has no working directory".to_string(),
    })?;

    let index = repo.repo.index().map_err(|e| GitError::BackendError {
        message: format!("failed to read index: {e}"),
    })?;

    let mut dirty = 0;
    for entry in index.entries() {
        if entry.stage_raw() != 0 {
            dirty += 1;
            continue;
        }

        let path_bytes = entry.path(&index);
        let path_str = match std::str::from_utf8(path_bytes) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let full_path = workdir.join(path_str);
        match std::fs::symlink_metadata(&full_path) {
            Ok(meta) => {
                let stat = entry.stat;
                if meta.len() != stat.size as u64 {
                    dirty += 1;
                } else if meta.mtime() as u32 != stat.mtime.secs
                    || meta.mtime_nsec() as u32 != stat.mtime.nsecs
                {
                    dirty += 1;
                }
            }
            Err(_) => {
                dirty += 1;
            }
        }
    }

    Ok(dirty)
}

fn convert_status_item(item: &gix::status::index_worktree::Item) -> Option<StatusEntry> {
    let summary = item.summary()?;
    let path = item.rela_path().to_str().ok()?.to_owned();

    use gix::status::index_worktree::iter::Summary;
    let status = match summary {
        Summary::Added | Summary::IntentToAdd | Summary::Copied => FileStatus::Added,
        Summary::Modified | Summary::TypeChange | Summary::Conflict => FileStatus::Modified,
        Summary::Removed => FileStatus::Deleted,
        Summary::Renamed => FileStatus::Renamed,
    };

    Some(StatusEntry { path, status })
}
