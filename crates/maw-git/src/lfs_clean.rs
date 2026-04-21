//! LFS clean-filter path: translate real-content blobs into pointer blobs
//! at commit/merge time.
//!
//! This is the write-side counterpart to the smudge post-pass in
//! `checkout_impl.rs`. When a caller writes a blob for a path that is
//! `filter=lfs` tracked, we:
//!
//! 1. Stream the content into `.git/lfs/objects/` (computing sha256).
//! 2. Build the pointer text.
//! 3. Write the **pointer** as the git blob.
//!
//! If the caller already hands us pointer bytes (because they're copying an
//! existing LFS blob from another tree), we pass them through unchanged.

use std::io::Cursor;

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::GitOid;

pub fn write_blob_with_path(
    repo: &GixRepo,
    data: &[u8],
    rel_path: &str,
) -> Result<GitOid, GitError> {
    // Load .gitattributes for LFS pattern matching.
    //
    // Priority:
    // 1. Pending attrs override (set by merge callers when the merge itself
    //    modifies .gitattributes — uses the INCOMING attrs, not HEAD's).
    // 2. HEAD tree (correct repo-relative paths; works for bare repos).
    // 3. Workdir fallback (fresh repo with no HEAD).
    let attrs = if let Some(ref entries) = repo.pending_gitattributes {
        match maw_lfs::AttrsMatcher::from_entries(entries.clone()) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!("pending gitattributes parse failed: {e}");
                return crate::objects_impl::write_blob(repo, data);
            }
        }
    } else {
        match load_attrs_from_head(repo) {
            Ok(a) if !a.is_empty() => a,
            _ => {
                let workdir = match repo.repo.workdir() {
                    Some(w) => w.to_owned(),
                    None => return crate::objects_impl::write_blob(repo, data),
                };
                match maw_lfs::AttrsMatcher::from_workdir(&workdir) {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::warn!("lfs attrs load failed: {e} — writing raw blob");
                        return crate::objects_impl::write_blob(repo, data);
                    }
                }
            }
        }
    };
    if !attrs.is_lfs(rel_path) {
        return crate::objects_impl::write_blob(repo, data);
    }

    // Empty files: store as empty git blobs, not pointers. git-lfs does
    // the same — `git lfs fsck` flags empty-file pointers as non-canonical.
    if data.is_empty() {
        return crate::objects_impl::write_blob(repo, data);
    }

    // Already a pointer? Write as-is (don't double-wrap).
    if maw_lfs::looks_like_pointer(data) {
        return crate::objects_impl::write_blob(repo, data);
    }

    // Clean filter: store real content, build pointer, write pointer blob.
    let git_dir = repo.repo.git_dir();
    let store = maw_lfs::Store::open(git_dir).map_err(|e| GitError::BackendError {
        message: format!("lfs store: {e}"),
    })?;
    let (pointer, _size) =
        store
            .insert_from_reader(Cursor::new(data))
            .map_err(|e| GitError::BackendError {
                message: format!("lfs store insert: {e}"),
            })?;
    let pointer_bytes = pointer.write();
    crate::objects_impl::write_blob(repo, &pointer_bytes)
}

/// Load `.gitattributes` entries from the HEAD tree of a bare repo.
///
/// Walks the HEAD commit's tree recursively, collecting every entry named
/// `.gitattributes`, and builds an [`maw_lfs::AttrsMatcher`] from their
/// blob contents.
fn load_attrs_from_head(repo: &GixRepo) -> Result<maw_lfs::AttrsMatcher, GitError> {
    maw_lfs::AttrsMatcher::from_gix_head(&repo.repo).map_err(|e| GitError::BackendError {
        message: format!("bare repo: failed to load .gitattributes from HEAD: {e}"),
    })
}
