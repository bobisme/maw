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
    // Attrs matcher driven by the repo's workdir. If the repo is bare
    // (no workdir), load attrs from the HEAD tree instead.
    let attrs = if let Some(workdir) = repo.repo.workdir() {
        match maw_lfs::AttrsMatcher::from_workdir(&workdir.to_owned()) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!("lfs attrs load failed: {e} — writing raw blob");
                return crate::objects_impl::write_blob(repo, data);
            }
        }
    } else {
        match load_attrs_from_head(repo) {
            Ok(a) => a,
            Err(e) => {
                tracing::debug!("bare repo: no attrs from HEAD: {e} — writing raw blob");
                return crate::objects_impl::write_blob(repo, data);
            }
        }
    };
    if !attrs.is_lfs(rel_path) {
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
    let (pointer, _size) = store
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
    let head_commit = repo
        .repo
        .head_commit()
        .map_err(|e| GitError::BackendError {
            message: format!("bare repo: cannot resolve HEAD commit: {e}"),
        })?;
    let tree = head_commit
        .tree()
        .map_err(|e| GitError::BackendError {
            message: format!("bare repo: cannot read HEAD tree: {e}"),
        })?;

    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    collect_gitattributes_from_tree(repo, &tree, String::new(), &mut entries)?;

    maw_lfs::AttrsMatcher::from_entries(entries).map_err(|e| GitError::BackendError {
        message: format!("bare repo: failed to parse .gitattributes: {e}"),
    })
}

/// Recursively walk a tree, collecting `.gitattributes` blobs.
///
/// `prefix` is the repo-relative directory path with trailing slash (empty
/// for the root tree).
fn collect_gitattributes_from_tree(
    repo: &GixRepo,
    tree: &gix::Tree<'_>,
    prefix: String,
    out: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), GitError> {
    for entry_result in tree.iter() {
        let entry = entry_result.map_err(|e| GitError::BackendError {
            message: format!("tree entry decode: {e}"),
        })?;
        let name = entry.inner.filename.to_string();

        if entry.inner.mode.is_tree() {
            // Recurse into subtree.
            let subtree_id = gix::ObjectId::from(entry.inner.oid);
            let subtree = repo.repo.find_tree(subtree_id).map_err(|e| {
                GitError::BackendError {
                    message: format!("find subtree {subtree_id}: {e}"),
                }
            })?;
            let sub_prefix = format!("{prefix}{name}/");
            collect_gitattributes_from_tree(repo, &subtree, sub_prefix, out)?;
        } else if name == ".gitattributes" {
            let blob_id = gix::ObjectId::from(entry.inner.oid);
            let mut blob =
                repo.repo.find_blob(blob_id).map_err(|e| GitError::BackendError {
                    message: format!("read .gitattributes blob {blob_id}: {e}"),
                })?;
            out.push((prefix.clone(), blob.take_data()));
        }
    }
    Ok(())
}
