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
    //
    // Returns the repo-relative paths of files that were smudged so we can
    // update their index stat entries below.
    #[cfg(feature = "lfs")]
    let smudged_paths = match smudge_lfs_pointers(&index_file, workdir, repo) {
        Ok(paths) => paths,
        Err(e) => {
            tracing::warn!("lfs smudge post-pass failed: {e}");
            Vec::new()
        }
    };

    // Update index stat entries for smudged files. After smudge, the
    // on-disk file has different size/mtime than the pointer text that gix
    // checked out. If we don't update the stat cache, `git status` reports
    // every smudged LFS file as "modified" (phantom dirty state).
    #[cfg(feature = "lfs")]
    for rel_path in &smudged_paths {
        let full = workdir.join(rel_path);
        let meta = match gix::index::fs::Metadata::from_path_no_follow(&full) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let Ok(new_stat) = gix::index::entry::Stat::from_fs(&meta) else {
            continue;
        };
        // Find the entry by path and update its stat.
        if let Some(idx) = index_file
            .entries()
            .iter()
            .position(|e| e.path(&index_file).to_str().ok() == Some(rel_path.as_str()))
        {
            index_file.entries_mut()[idx].stat = new_stat;
        }
    }

    // Write the index to disk. gix::worktree::state::checkout updates
    // stat info in the in-memory index but does not persist it.
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

    // Remove working-tree files not present in the target tree.
    // This fulfills the trait contract: "Existing working-tree files not in
    // the tree are removed."
    remove_stale_files(workdir, workdir, &tree_paths)?;

    Ok(())
}

/// Self-contained LFS smudge for an existing worktree: open repo, smudge
/// pointer files, restore any LFS files that are in the HEAD tree but
/// missing from disk (happens when git-lfs smudge fails on `git checkout`),
/// update index stats, rewrite index.
///
/// Used by maw-cli after `git checkout` CLI calls where git-lfs may have
/// failed to smudge files with missing objects.
#[cfg(feature = "lfs")]
pub fn lfs_smudge_worktree_at(ws_path: &Path, target_commit: &str) -> Result<(), GitError> {
    let repo = GixRepo::open(ws_path)?;

    // Resolve the target commit to its tree. This is the tree we WANT on
    // disk, which may differ from HEAD if checkout failed.
    let target_oid = gix::ObjectId::from_hex(target_commit.as_bytes())
        .map_err(|e| GitError::BackendError {
            message: format!("bad target OID '{target_commit}': {e}"),
        })?;

    // First: restore LFS files that are in the target tree but missing from
    // disk. `git checkout` + git-lfs may skip files entirely when the LFS
    // object isn't in the local store.
    let mut restored: Vec<String> = Vec::new();
    if let Ok(obj) = repo.repo.find_object(target_oid) {
        let tree_id = match obj.kind {
            gix::object::Kind::Commit => obj.into_commit().tree_id().ok().map(|t| t.detach()),
            gix::object::Kind::Tree => Some(target_oid),
            _ => None,
        };
        if let Some(tid) = tree_id {
            if let Ok(tree) = repo.repo.find_tree(tid) {
                let attrs = maw_lfs::AttrsMatcher::from_workdir(ws_path)
                    .unwrap_or_else(|_| maw_lfs::AttrsMatcher::empty());
                restore_missing_lfs_from_tree(&repo, &tree, ws_path, &attrs, String::new(), &mut restored);
            }
        }
    }

    // Second: normal smudge pass on files that ARE on disk (replace pointers
    // with real content when the object is in the local store).
    let index = repo.repo.open_index().map_err(|e| GitError::BackendError {
        message: format!("failed to open index: {e}"),
    })?;
    let smudged = smudge_lfs_pointers(&index, ws_path, &repo)?;

    let all_changed: Vec<String> = smudged.into_iter().chain(restored).collect();
    if all_changed.is_empty() {
        return Ok(());
    }

    // Re-read index (may have changed after git add from restores),
    // update stat cache, rewrite.
    let index = repo.repo.open_index().map_err(|e| GitError::BackendError {
        message: format!("failed to re-open index: {e}"),
    })?;
    let mut index_state: gix::index::State = index.into();
    for rel_path in &all_changed {
        let full = ws_path.join(rel_path);
        let Ok(meta) = gix::index::fs::Metadata::from_path_no_follow(&full) else {
            continue;
        };
        let Ok(new_stat) = gix::index::entry::Stat::from_fs(&meta) else {
            continue;
        };
        if let Some(idx) = index_state
            .entries()
            .iter()
            .position(|e| e.path(&index_state).to_str().ok() == Some(rel_path.as_str()))
        {
            index_state.entries_mut()[idx].stat = new_stat;
        }
    }
    let index_path = repo.repo.index_path();
    let mut persisted = gix::index::File::from_state(index_state, index_path);
    persisted
        .write(Default::default())
        .map_err(|e| GitError::BackendError {
            message: format!("failed to rewrite index after smudge: {e}"),
        })?;
    Ok(())
}

/// Walk the HEAD tree and restore any LFS-tracked files that are missing
/// from the working directory. Writes the pointer text from the committed
/// blob to disk and runs `git add <path>` to update the index.
#[cfg(feature = "lfs")]
fn restore_missing_lfs_from_tree(
    repo: &GixRepo,
    tree: &gix::Tree<'_>,
    workdir: &Path,
    attrs: &maw_lfs::AttrsMatcher,
    prefix: String,
    restored: &mut Vec<String>,
) {
    use gix::bstr::ByteSlice;

    for entry_result in tree.iter() {
        let Ok(entry) = entry_result else { continue };
        let name = entry.inner.filename.to_str().unwrap_or("");

        if entry.inner.mode.is_tree() {
            let subtree_id = gix::ObjectId::from(entry.inner.oid);
            if let Ok(subtree) = repo.repo.find_tree(subtree_id) {
                let sub_prefix = if prefix.is_empty() {
                    name.to_owned()
                } else {
                    format!("{prefix}/{name}")
                };
                restore_missing_lfs_from_tree(repo, &subtree, workdir, attrs, sub_prefix, restored);
            }
            continue;
        }

        if !entry.inner.mode.is_blob() {
            continue;
        }

        let rel_path = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };

        if !attrs.is_lfs(&rel_path) {
            continue;
        }

        let full_path = workdir.join(&rel_path);
        if full_path.exists() {
            continue; // Already on disk — the normal smudge pass handles it.
        }

        // File missing from disk. Read the committed blob and write it.
        let blob_id = gix::ObjectId::from(entry.inner.oid);
        let Ok(obj) = repo.repo.find_object(blob_id) else {
            continue;
        };
        let data = obj.data.to_vec();
        if data.is_empty() {
            // Empty file — just touch it.
            if let Some(parent) = full_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&full_path, &data);
            restored.push(rel_path);
            continue;
        }

        // Write whatever the blob contains (pointer text, or raw content
        // if the file was un-LFS'd).
        if let Some(parent) = full_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&full_path, &data).is_ok() {
            tracing::warn!(path = %rel_path, "lfs: restored missing file from tree");
            restored.push(rel_path);
        }
    }
}

/// Crate-internal entry point for the LFS smudge post-pass, callable from
/// `worktree_impl::worktree_add` and `checkout_tree`.
/// Returns the repo-relative paths of files that were smudged.
#[cfg(feature = "lfs")]
pub(crate) fn smudge_lfs_pointers_public(
    index: &gix::index::File,
    workdir: &Path,
    repo: &GixRepo,
) -> Result<Vec<String>, GitError> {
    smudge_lfs_pointers(index, workdir, repo)
}

/// Returns the repo-relative paths of files that were successfully smudged.
#[cfg(feature = "lfs")]
fn smudge_lfs_pointers(
    index: &gix::index::File,
    workdir: &Path,
    repo: &GixRepo,
) -> Result<Vec<String>, GitError> {
    use std::io::Write;

    let mut smudged: Vec<String> = Vec::new();

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

        // If the file doesn't exist on disk (e.g. git-lfs smudge failed
        // during `git checkout` because the object was missing), read the
        // blob from the ODB and write the pointer text to disk. This
        // ensures LFS-tracked files always have SOMETHING on disk.
        let meta = std::fs::metadata(&full_path);
        if meta.is_err() {
            // File missing — read the committed blob (should be pointer text).
            let blob_oid = gix::ObjectId::from(entry.id);
            if let Ok(obj) = repo.repo.find_object(blob_oid) {
                let data = obj.data.to_vec();
                if maw_lfs::looks_like_pointer(&data) {
                    // Ensure parent directory exists.
                    if let Some(parent) = full_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if std::fs::write(&full_path, &data).is_ok() {
                        smudged.push(path_str.to_owned());
                        tracing::warn!(
                            path = path_str,
                            "lfs: restored missing file as pointer stub"
                        );
                    }
                }
            }
            continue;
        }

        // Pointer cap is 1024 bytes per spec; anything larger is real content.
        let meta = meta.unwrap();
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

        match result {
            Ok(()) => smudged.push(path_str.to_owned()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_path);
                tracing::warn!(path = path_str, "lfs smudge write failed: {e}");
            }
        }
    }

    Ok(smudged)
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

/// Update HEAD to point directly at `oid` (detached HEAD).
///
/// For a linked worktree, this updates the per-worktree HEAD file under
/// `.git/worktrees/<name>/HEAD` — the common-dir HEAD is untouched. For a
/// non-worktree repo, it updates `.git/HEAD`.
///
/// Writes the canonical detached-HEAD format: 40 hex bytes followed by a
/// single `\n`. Uses an atomic write (create temp file + rename) so a
/// concurrent reader never sees a partial HEAD.
pub fn set_head(repo: &GixRepo, oid: GitOid) -> Result<(), GitError> {
    use std::io::Write as _;

    let git_dir = repo.repo.git_dir();
    let head_path = git_dir.join("HEAD");
    let tmp_path = git_dir.join("HEAD.maw-tmp");

    let contents = format!("{oid}\n");

    // Atomic write: write temp then rename.
    {
        let mut f = std::fs::File::create(&tmp_path).map_err(|e| GitError::BackendError {
            message: format!("failed to create temp HEAD at {}: {e}", tmp_path.display()),
        })?;
        f.write_all(contents.as_bytes())
            .map_err(|e| GitError::BackendError {
                message: format!("failed to write temp HEAD: {e}"),
            })?;
        f.sync_all().map_err(|e| GitError::BackendError {
            message: format!("failed to fsync temp HEAD: {e}"),
        })?;
    }
    std::fs::rename(&tmp_path, &head_path).map_err(|e| {
        // Best-effort cleanup; swallow the unlink error to surface the rename failure.
        let _ = std::fs::remove_file(&tmp_path);
        GitError::BackendError {
            message: format!(
                "failed to rename temp HEAD into place at {}: {e}",
                head_path.display()
            ),
        }
    })?;

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
