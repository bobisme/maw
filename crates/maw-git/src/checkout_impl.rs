//! gix-backed checkout and index operations.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use gix::bstr::ByteSlice;

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::*;

pub fn checkout_tree(repo: &GixRepo, oid: GitOid, workdir: &Path) -> Result<(), GitError> {
    let gix_oid = gix::ObjectId::from_bytes_or_panic(oid.as_bytes());

    // If oid is a commit, resolve to its tree.
    let tree_oid = {
        let obj = repo
            .repo
            .find_object(gix_oid)
            .map_err(|e| GitError::NotFound {
                message: format!("object {oid}: {e}"),
            })?;
        match obj.kind {
            gix::object::Kind::Commit => {
                let commit = obj.into_commit();
                commit
                    .tree_id()
                    .map_err(|e| GitError::BackendError {
                        message: format!("failed to get tree from commit {oid}: {e}"),
                    })?
                    .detach()
            }
            gix::object::Kind::Tree => gix_oid,
            other => {
                return Err(GitError::BackendError {
                    message: format!("expected commit or tree, got {other}"),
                });
            }
        }
    };

    // Build index from tree using the high-level API (handles protect_options internally).
    let mut index_file = repo
        .repo
        .index_from_tree(&tree_oid)
        .map_err(|e| GitError::BackendError {
            message: format!("failed to create index from tree {tree_oid}: {e}"),
        })?;

    // Get checkout options from the repository configuration.
    let mut opts = repo
        .repo
        .checkout_options(gix::worktree::stack::state::attributes::Source::IdMapping)
        .map_err(|e| GitError::BackendError {
            message: format!("failed to get checkout options: {e}"),
        })?;
    opts.overwrite_existing = true;
    opts.destination_is_initially_empty = false;

    let objects = repo
        .repo
        .objects
        .clone()
        .into_arc()
        .map_err(|e| GitError::BackendError {
            message: format!("failed to convert object store to Arc: {e}"),
        })?;

    let outcome = gix::worktree::state::checkout(
        &mut index_file,
        workdir,
        objects,
        &gix::progress::Discard,
        &gix::progress::Discard,
        &AtomicBool::new(false),
        opts,
    )
    .map_err(|e| GitError::BackendError {
        message: format!("checkout failed: {e}"),
    })?;

    if !outcome.errors.is_empty() {
        let first = &outcome.errors[0];
        return Err(GitError::BackendError {
            message: format!(
                "checkout had {} error(s), first: {}: {}",
                outcome.errors.len(),
                first.path,
                first.error,
            ),
        });
    }

    Ok(())
}

pub fn read_index(repo: &GixRepo) -> Result<Vec<IndexEntry>, GitError> {
    let index = repo
        .repo
        .open_index()
        .map_err(|e| GitError::BackendError {
            message: format!("failed to open index: {e}"),
        })?;

    let entries = index
        .entries()
        .iter()
        .filter_map(|entry| {
            let path = entry.path(&index).to_str().ok()?.to_owned();
            let mode = gix_mode_to_entry_mode(entry.mode)?;
            let oid = GitOid::from_bytes(entry.id.as_bytes().try_into().ok()?);
            Some(IndexEntry { path, mode, oid })
        })
        .collect();

    Ok(entries)
}

pub fn write_index(repo: &GixRepo, entries: &[IndexEntry]) -> Result<(), GitError> {
    let mut state = gix::index::State::new(repo.repo.object_hash());

    for ie in entries {
        let mode = entry_mode_to_gix_mode(ie.mode);
        let id = gix::ObjectId::from_bytes_or_panic(ie.oid.as_bytes());
        let stat: gix::index::entry::Stat = Default::default();
        let flags = gix::index::entry::Flags::empty();

        state.dangerously_push_entry(stat, id, flags, mode, ie.path.as_str().into());
    }

    state.sort_entries();

    let index_path = repo.repo.index_path();
    let mut index_file = gix::index::File::from_state(state, index_path);
    index_file
        .write(Default::default())
        .map_err(|e| GitError::BackendError {
            message: format!("failed to write index: {e}"),
        })?;

    Ok(())
}

fn gix_mode_to_entry_mode(mode: gix::index::entry::Mode) -> Option<EntryMode> {
    Some(match mode {
        gix::index::entry::Mode::FILE => EntryMode::Blob,
        gix::index::entry::Mode::FILE_EXECUTABLE => EntryMode::BlobExecutable,
        gix::index::entry::Mode::SYMLINK => EntryMode::Link,
        gix::index::entry::Mode::DIR => EntryMode::Tree,
        gix::index::entry::Mode::COMMIT => EntryMode::Commit,
        _ => return None,
    })
}

fn entry_mode_to_gix_mode(mode: EntryMode) -> gix::index::entry::Mode {
    match mode {
        EntryMode::Blob => gix::index::entry::Mode::FILE,
        EntryMode::BlobExecutable => gix::index::entry::Mode::FILE_EXECUTABLE,
        EntryMode::Link => gix::index::entry::Mode::SYMLINK,
        EntryMode::Tree => gix::index::entry::Mode::DIR,
        EntryMode::Commit => gix::index::entry::Mode::COMMIT,
    }
}
