//! gix-backed checkout and index operations.

use std::collections::HashSet;
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
    let mut index_file =
        repo.repo
            .index_from_tree(&tree_oid)
            .map_err(|e| GitError::BackendError {
                message: format!("failed to create index from tree {tree_oid}: {e}"),
            })?;

    // Collect all paths in the target tree so we can remove stale files after checkout.
    let tree_paths: HashSet<String> = index_file
        .entries()
        .iter()
        .filter_map(|entry| entry.path(&index_file).to_str().ok().map(|s| s.to_owned()))
        .collect();

    // Get checkout options from the repository configuration.
    let mut opts = repo
        .repo
        .checkout_options(gix::worktree::stack::state::attributes::Source::IdMapping)
        .map_err(|e| GitError::BackendError {
            message: format!("failed to get checkout options: {e}"),
        })?;
    opts.overwrite_existing = true;
    opts.destination_is_initially_empty = false;

    // When the `lfs` feature is on, maw-lfs handles LFS smudge/clean itself
    // in a post-pass. Clear external filter drivers here so gix does NOT
    // spawn git-lfs (or any other filter binary) as a subprocess during
    // checkout. Built-in filters (ident, text, eol) remain available.
    #[cfg(feature = "lfs")]
    {
        opts.filters.options_mut().drivers.clear();
    }

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

    // LFS smudge post-pass: replace any LFS pointer files with real content
    // from the local store. Best-effort — logs and continues on errors.
    // Objects missing from the local store stay as pointer text with a warn.
    #[cfg(feature = "lfs")]
    if let Err(e) = smudge_lfs_pointers(&index_file, workdir, repo) {
        tracing::warn!("lfs smudge post-pass failed: {e}");
    }

    // Write the index to disk so `git status` sees checked-out files as
    // tracked. gix::worktree::state::checkout updates stat info in the
    // in-memory index but does not persist it.
    {
        let index_path = repo.repo.index_path();
        let mut persisted =
            gix::index::File::from_state(index_file.into(), index_path);
        persisted
            .write(Default::default())
            .map_err(|e| GitError::BackendError {
                message: format!("failed to write index after checkout: {e}"),
            })?;
    }

    // After smudging, refresh the git index so the stat cache reflects the
    // new file sizes/mtimes. Without this, `git status` reports every
    // smudged LFS file as "modified" (phantom dirty state).
    //
    // We run `git add .` which:
    //  1. Re-stats every tracked file.
    //  2. For changed files, runs the configured clean filter (git-lfs),
    //     converting real content back to pointer text for the index blob.
    //  3. Since clean(real_content) produces the same pointer already in
    //     the index, the blob is unchanged — but the stat cache is updated.
    //
    // This is a one-time post-checkout operation. gix has no stable
    // equivalent for "refresh stat cache + run clean filters".
    #[cfg(feature = "lfs")]
    {
        let _ = std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(workdir)
            .output();
    }

    // Remove working-tree files not present in the target tree.
    // This fulfills the trait contract: "Existing working-tree files not in
    // the tree are removed."
    remove_stale_files(workdir, workdir, &tree_paths)?;

    Ok(())
}

/// LFS smudge: replace pointer files in the working tree with real content
/// from the local LFS store.
///
/// Walks all tree-tracked entries in `index`, checks each against the
/// `.gitattributes` rules just written to `workdir`, and for any LFS-tracked
/// path that currently holds a pointer, overwrites with the real bytes.
/// Objects missing from the local store are left as pointer text with a
/// tracing warning — checkout itself is not failed.
/// Public entry point for the LFS smudge post-pass, callable from
/// `worktree_impl::worktree_add` and other checkout paths.
#[cfg(feature = "lfs")]
pub(crate) fn smudge_lfs_pointers_public(
    index: &gix::index::File,
    workdir: &Path,
    repo: &GixRepo,
) -> Result<(), GitError> {
    smudge_lfs_pointers(index, workdir, repo)
}

#[cfg(feature = "lfs")]
fn smudge_lfs_pointers(
    index: &gix::index::File,
    workdir: &Path,
    repo: &GixRepo,
) -> Result<(), GitError> {
    use std::io::Write;

    let attrs = maw_lfs::AttrsMatcher::from_workdir(workdir).map_err(|e| {
        GitError::BackendError {
            message: format!("lfs attrs: {e}"),
        }
    })?;

    // Open (or create) the LFS store under the git dir.
    let git_dir = repo.repo.git_dir();
    let store = maw_lfs::Store::open(git_dir).map_err(|e| GitError::BackendError {
        message: format!("lfs store: {e}"),
    })?;

    for entry in index.entries() {
        // Only regular files; skip submodules / symlinks / trees.
        let is_file = matches!(
            entry.mode,
            gix::index::entry::Mode::FILE | gix::index::entry::Mode::FILE_EXECUTABLE
        );
        if !is_file {
            continue;
        }
        let path_str = match entry.path(index).to_str() {
            Ok(s) => s,
            Err(_) => continue,
        };
        if !attrs.is_lfs(path_str) {
            continue;
        }

        let full_path = workdir.join(path_str);
        // Pointer cap is 1024 bytes per spec; anything larger is real content.
        let meta = match std::fs::metadata(&full_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.len() > 1024 {
            continue;
        }

        let bytes = match std::fs::read(&full_path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if !maw_lfs::looks_like_pointer(&bytes) {
            continue;
        }
        let pointer = match maw_lfs::Pointer::parse(&bytes) {
            Ok(p) => p,
            Err(_) => continue,
        };

        let mut reader = match store.open_object(&pointer.oid) {
            Ok(Some(r)) => r,
            Ok(None) => {
                tracing::warn!(
                    path = path_str,
                    oid = %pointer.oid_hex(),
                    "lfs object missing from local store — pointer left on disk"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(path = path_str, "lfs store error: {e}");
                continue;
            }
        };

        // Atomic replace: write to sibling tmp file, rename over.
        let tmp_path = full_path.with_extension("maw-lfs-tmp");
        let result = (|| -> std::io::Result<()> {
            let mut out = std::fs::File::create(&tmp_path)?;
            std::io::copy(&mut reader, &mut out)?;
            out.flush()?;
            out.sync_all()?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode_bits =
                    if entry.mode == gix::index::entry::Mode::FILE_EXECUTABLE {
                        0o755
                    } else {
                        0o644
                    };
                std::fs::set_permissions(
                    &tmp_path,
                    std::fs::Permissions::from_mode(mode_bits),
                )?;
            }

            std::fs::rename(&tmp_path, &full_path)?;
            Ok(())
        })();

        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp_path);
            tracing::warn!(path = path_str, "lfs smudge write failed: {e}");
        }
    }

    Ok(())
}

/// Walk `dir` and remove any files whose path relative to `workdir` is not in `tree_paths`.
/// Skips `.git` directories/files. Removes empty directories after file cleanup.
fn remove_stale_files(
    workdir: &Path,
    dir: &Path,
    tree_paths: &HashSet<String>,
) -> Result<(), GitError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        let name = entry.file_name();

        // Never touch .git (file or directory).
        if name == ".git" {
            continue;
        }

        if path.is_dir() {
            remove_stale_files(workdir, &path, tree_paths)?;
            // Remove directory if it became empty (ignore errors — may not be empty).
            let _ = std::fs::remove_dir(&path);
        } else {
            let rel = path
                .strip_prefix(workdir)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if !rel.is_empty() && !tree_paths.contains(&rel) {
                std::fs::remove_file(&path).map_err(|e| GitError::BackendError {
                    message: format!("failed to remove stale file '{}': {e}", rel),
                })?;
            }
        }
    }

    Ok(())
}

pub fn read_index(repo: &GixRepo) -> Result<Vec<IndexEntry>, GitError> {
    let index = repo.repo.open_index().map_err(|e| GitError::BackendError {
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
