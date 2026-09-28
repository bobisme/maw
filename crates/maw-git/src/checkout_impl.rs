//! gix-backed checkout and index operations.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use gix::bstr::{BString, ByteSlice};

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::{EntryMode, GitOid, IndexEntry};

#[expect(
    clippy::too_many_lines,
    reason = "checkout is a sequential git plumbing operation"
)]
pub fn checkout_tree(repo: &GixRepo, oid: GitOid, workdir: &Path) -> Result<(), GitError> {
    // This rewrites the working tree, which may add/remove/edit
    // `.gitattributes` files that the memoized LFS matcher read off disk
    // (bn-2fps). HEAD need not move, so drop the cache explicitly.
    #[cfg(feature = "lfs")]
    repo.invalidate_attrs_cache();

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
    let tree_paths: HashSet<BString> = index_file
        .entries()
        .iter()
        .map(|entry| entry.path(&index_file).to_owned())
        .collect();

    // Capture the paths tracked by the CURRENT (pre-checkout) on-disk index.
    // `git checkout --force` removes only TRACKED files absent from the target
    // tree; untracked files are preserved. We read the old index now, before
    // it is overwritten with the target index below. If no index is readable,
    // the tracked set is empty → we never delete a file we can't prove was
    // tracked, which is the safe (no-data-loss) default. (bn-29x0)
    let old_tracked: HashSet<BString> = repo.repo.open_index().map_or_else(
        |_| HashSet::new(),
        |idx| {
            idx.entries()
                .iter()
                .map(|entry| entry.path(&idx).to_owned())
                .collect()
        },
    );

    // Get checkout options from the repository configuration.
    let mut opts = repo
        .repo
        .checkout_options(gix::worktree::stack::state::attributes::Source::IdMapping)
        .map_err(|e| GitError::BackendError {
            message: format!("failed to get checkout options: {e}"),
        })?;
    opts.overwrite_existing = true;
    opts.destination_is_initially_empty = false;
    let fs_has_exec_bit = opts.fs.executable_bit;

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

    // bn-2nnuz: gix writes over an existing file in place and only ever ADDS
    // the executable bit (`finalize_entry`), so a target entry of mode 100644
    // that lands on an executable file keeps `+x` — the worktree then shows
    // a mode change and the next commit reverts the target's mode. Clear it,
    // like `git checkout` (which recreates the file with the entry's mode).
    if fs_has_exec_bit {
        clear_stale_exec_bits(&mut index_file, workdir)?;
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
        let Ok(meta) = gix::index::fs::Metadata::from_path_no_follow(&full) else {
            continue;
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
        let mut persisted = gix::index::File::from_state(index_file.into(), index_path);
        persisted
            .write(gix::index::write::Options::default())
            .map_err(|e| GitError::BackendError {
                message: format!("failed to write index after checkout: {e}"),
            })?;
    }

    // Remove tracked working-tree files no longer present in the target tree
    // (matches `git checkout --force`). Untracked files are preserved — see
    // `remove_stale_files` and bn-29x0.
    remove_stale_files(workdir, workdir, &tree_paths, &old_tracked)?;

    Ok(())
}

/// Clear the executable bit of every checked-out regular file whose index
/// entry is a plain (non-executable) blob, refreshing the entry's stat so the
/// chmod's ctime change does not make the file look modified. (bn-2nnuz)
///
/// Only regular files are touched and nothing is followed through a symlink:
/// the leaf is `lstat`ed, and a path with a symlinked (or missing) parent
/// component inside `workdir` is skipped — gix never writes through one.
#[cfg(unix)]
fn clear_stale_exec_bits(
    index_file: &mut gix::index::File,
    workdir: &Path,
) -> Result<(), GitError> {
    use std::os::unix::fs::PermissionsExt;

    let mut fixes: Vec<(usize, gix::index::entry::Stat)> = Vec::new();
    for (idx, entry) in index_file.entries().iter().enumerate() {
        if entry.mode != gix::index::entry::Mode::FILE
            || entry
                .flags
                .contains(gix::index::entry::Flags::SKIP_WORKTREE)
        {
            continue;
        }
        let Ok(rel) = gix::path::try_from_bstr(entry.path(index_file)) else {
            continue;
        };
        let full = workdir.join(&rel);
        let Ok(meta) = std::fs::symlink_metadata(&full) else {
            continue;
        };
        let mode = meta.permissions().mode();
        if !meta.is_file() || mode & 0o111 == 0 || has_symlinked_parent(workdir, &rel) {
            continue;
        }
        std::fs::set_permissions(&full, std::fs::Permissions::from_mode(mode & !0o111)).map_err(
            |e| GitError::BackendError {
                message: format!(
                    "failed to clear executable bit of '{}' after checkout: {e}",
                    rel.display()
                ),
            },
        )?;
        if let Ok(meta) = gix::index::fs::Metadata::from_path_no_follow(&full)
            && let Ok(stat) = gix::index::entry::Stat::from_fs(&meta)
        {
            fixes.push((idx, stat));
        }
    }
    let entries = index_file.entries_mut();
    for (idx, stat) in fixes {
        entries[idx].stat = stat;
    }
    Ok(())
}

#[cfg(not(unix))]
fn clear_stale_exec_bits(
    _index_file: &mut gix::index::File,
    _workdir: &Path,
) -> Result<(), GitError> {
    Ok(())
}

/// Whether any proper parent component of `rel` inside `root` is a symlink
/// (or cannot be `lstat`ed).
#[cfg(unix)]
fn has_symlinked_parent(root: &Path, rel: &Path) -> bool {
    let Some(parent) = rel.parent() else {
        return false;
    };
    let mut current = root.to_path_buf();
    for component in parent.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if !meta.file_type().is_symlink() => {}
            _ => return true,
        }
    }
    false
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
    let target_oid =
        gix::ObjectId::from_hex(target_commit.as_bytes()).map_err(|e| GitError::BackendError {
            message: format!("bad target OID '{target_commit}': {e}"),
        })?;

    // First: restore LFS files that are in the target tree but missing from
    // disk. `git checkout` + git-lfs may skip files entirely when the LFS
    // object isn't in the local store.
    let mut restored: Vec<String> = Vec::new();
    if let Ok(obj) = repo.repo.find_object(target_oid) {
        let tree_id = match obj.kind {
            gix::object::Kind::Commit => obj.into_commit().tree_id().ok().map(gix::Id::detach),
            gix::object::Kind::Tree => Some(target_oid),
            _ => None,
        };
        if let Some(tid) = tree_id
            && let Ok(tree) = repo.repo.find_tree(tid)
        {
            let attrs = maw_lfs::AttrsMatcher::from_workdir(ws_path)
                .unwrap_or_else(|_| maw_lfs::AttrsMatcher::empty());
            restore_missing_lfs_from_tree(&repo, &tree, ws_path, &attrs, "", &mut restored);
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
        .write(gix::index::write::Options::default())
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
    prefix: &str,
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
                restore_missing_lfs_from_tree(
                    repo,
                    &subtree,
                    workdir,
                    attrs,
                    &sub_prefix,
                    restored,
                );
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
        let data = obj.data.clone();
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
pub fn smudge_lfs_pointers_public(
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

    let attrs =
        maw_lfs::AttrsMatcher::from_workdir(workdir).map_err(|e| GitError::BackendError {
            message: format!("lfs attrs: {e}"),
        })?;

    // Open (or create) the LFS store under the COMMON git dir, not the
    // per-worktree git dir. Every maw workspace other than the default one is
    // a linked worktree, whose `git_dir()` is the private
    // `<common>/worktrees/<name>/` admin directory — but LFS objects are
    // fetched/pushed once and shared at `<common>/lfs/objects/`. Using
    // `git_dir()` here silently pointed the smudge pass at an
    // always-empty per-worktree `lfs/objects/` directory, so every object
    // looked "missing from the local store" even when it was present in the
    // real (common) store, and the pointer was left on disk with only a
    // `tracing::warn!` (bn-1ero symptom 1). See `GixRepo::common_dir` and the
    // matching fix in `lfs_clean.rs` (write side).
    let git_dir = repo.repo.common_dir();
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
        let Ok(path_str) = entry.path(index).to_str() else {
            continue;
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
            let blob_oid = entry.id;
            if let Ok(obj) = repo.repo.find_object(blob_oid) {
                let data = obj.data.clone();
                if maw_lfs::git_lfs_decode(&data).is_some() {
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

        // git-lfs decodes only blobs under 1024 bytes as pointers (bn-hcbc8:
        // maw never smudges a longer blob, see `maw_lfs::git_lfs_decode`).
        let Ok(meta) = meta else {
            continue;
        };
        if meta.len() >= maw_lfs::pointer::MAX_POINTER_BYTES as u64 {
            continue;
        }

        let Ok(bytes) = std::fs::read(&full_path) else {
            continue;
        };
        // bn-hcbc8: decode exactly what `git lfs smudge` decodes (lenient
        // for non-canonical pointers; size 0 → empty; extensions and size
        // mismatches leave the pointer, as git-lfs does).
        let mut reader = match store.open_for_smudge(&bytes) {
            Ok(maw_lfs::SmudgeSource::Content { reader, .. }) => reader,
            Ok(maw_lfs::SmudgeSource::NotAPointer) => continue,
            Ok(maw_lfs::SmudgeSource::Unavailable { reason, .. }) => {
                tracing::warn!(path = path_str, "{reason} — pointer left on disk");
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
                let mode_bits = if entry.mode == gix::index::entry::Mode::FILE_EXECUTABLE {
                    0o755
                } else {
                    0o644
                };
                std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(mode_bits))?;
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

/// Walk `dir` and remove files that were **tracked** (`tracked_paths`) but are
/// absent from the target tree (`tree_paths`), relative to `workdir`. This
/// mirrors `git checkout --force`: untracked files (not in `tracked_paths`)
/// are preserved, never deleted. Skips `.git`; removes directories that become
/// empty after cleanup. (bn-29x0: the prior version deleted every path not in
/// the target tree, destroying untracked files on the snapshot-failed fallback
/// path where there is no recovery ref.)
fn remove_stale_files(
    workdir: &Path,
    dir: &Path,
    tree_paths: &HashSet<BString>,
    tracked_paths: &HashSet<BString>,
) -> Result<(), GitError> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };

    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let name = entry.file_name();

        // Never touch .git (file or directory).
        if name == ".git" {
            continue;
        }

        if path.is_dir() {
            remove_stale_files(workdir, &path, tree_paths, tracked_paths)?;
            // Remove directory if it became empty (ignore errors — may not be empty).
            let _ = std::fs::remove_dir(&path);
        } else {
            let Some(rel) = path.strip_prefix(workdir).ok().and_then(|p| {
                gix::path::try_into_bstr(p)
                    .ok()
                    .map(|p| gix::path::to_unix_separators_on_windows(p).into_owned())
            }) else {
                continue;
            };
            // Only remove a file that was tracked AND is gone from the target
            // tree. Untracked files (absent from `tracked_paths`) are preserved.
            if !rel.is_empty() && tracked_paths.contains(&rel) && !tree_paths.contains(&rel) {
                std::fs::remove_file(&path).map_err(|e| GitError::BackendError {
                    message: format!("failed to remove stale file '{}': {e}", rel.to_str_lossy()),
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
///
/// Also appends a reflog entry (bn-20sa): the live incident (bn-1qtj) was
/// forensically blind because `set_head` left no trail. The reflog entry is
/// best-effort — failure to write it does NOT fail the operation.
pub fn set_head(repo: &GixRepo, oid: GitOid) -> Result<(), GitError> {
    use std::io::Write as _;

    let git_dir = repo.repo.git_dir();
    let head_path = git_dir.join("HEAD");
    let tmp_path = git_dir.join("HEAD.maw-tmp");

    // Read old HEAD *before* we overwrite it — used for the reflog entry.
    let old_oid_str = std::fs::read_to_string(&head_path)
        .ok()
        .map(|s| s.trim().to_owned());

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

    // bn-20sa: append a reflog entry so future incidents are forensically
    // traceable. Both the sigil bn-3d4a and maw bn-1qtj incidents were
    // unfindable because set_head left no reflog entry. Best-effort: a write
    // failure here must not fail the operation — the HEAD update already
    // succeeded at this point.
    append_head_reflog(git_dir, old_oid_str.as_deref(), &oid.to_string());

    Ok(())
}

/// Compare-and-swap detached HEAD move (bn-302v).
///
/// Moves HEAD to `new` only if it currently resolves to `expected`, using
/// git's own lock protocol: `HEAD.lock` is created exclusively, HEAD is
/// re-read while the lock is held, the new value is written into the lock
/// file and the lock is renamed over HEAD. A concurrent `git commit` in the
/// same worktree also takes `HEAD.lock` to move a detached HEAD, so it either
/// lands before this call (the re-read sees it and the CAS fails) or fails
/// itself with "Unable to create HEAD.lock" (the agent sees an error). It can
/// never be silently moved off.
///
/// A symbolic HEAD (`ref: refs/heads/x`) is compared by its resolved OID; the
/// branch ref keeps its commits reachable, so detaching it cannot orphan work.
///
/// # Errors
/// * [`GitError::RefConflict`] when `HEAD.lock` already exists (another git
///   operation is in progress; the lock is NOT removed) or HEAD does not
///   resolve to `expected`. HEAD is unchanged.
/// * I/O errors writing the lock or renaming it. HEAD is unchanged.
pub fn set_head_detached_cas(
    repo: &GixRepo,
    expected: GitOid,
    new: GitOid,
) -> Result<(), GitError> {
    use crate::repo::GitRepo as _;
    use std::io::Write as _;

    let git_dir = repo.repo.git_dir();
    let head_path = git_dir.join("HEAD");
    let lock_path = git_dir.join("HEAD.lock");

    let mut lock = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(GitError::RefConflict {
                ref_name: "HEAD".to_owned(),
                message: format!(
                    "{} exists: another git operation is in progress",
                    lock_path.display()
                ),
            });
        }
        Err(e) => return Err(GitError::IoError(e)),
    };
    // From here on the lock is ours: remove it on every non-success path.
    let release = |err: GitError| -> GitError {
        let _ = std::fs::remove_file(&lock_path);
        err
    };

    let raw = match std::fs::read_to_string(&head_path) {
        Ok(r) => r,
        Err(e) => return Err(release(GitError::IoError(e))),
    };
    let raw = raw.trim();
    let current: Option<GitOid> = if raw.starts_with("ref:") {
        repo.rev_parse_opt("HEAD").ok().flatten()
    } else {
        raw.parse().ok()
    };
    if current != Some(expected) {
        return Err(release(GitError::RefConflict {
            ref_name: "HEAD".to_owned(),
            message: format!(
                "expected {expected}, found {}",
                current.map_or_else(|| format!("unreadable ({raw})"), |c| c.to_string())
            ),
        }));
    }

    if let Err(e) = lock
        .write_all(format!("{new}\n").as_bytes())
        .and_then(|()| lock.sync_all())
    {
        return Err(release(GitError::IoError(e)));
    }
    drop(lock);
    if let Err(e) = std::fs::rename(&lock_path, &head_path) {
        return Err(release(GitError::BackendError {
            message: format!(
                "failed to rename {} into place at {}: {e}",
                lock_path.display(),
                head_path.display()
            ),
        }));
    }
    append_head_reflog(git_dir, Some(&expected.to_string()), &new.to_string());
    Ok(())
}

/// Append a reflog entry for the worktree HEAD file.
///
/// Format (git files-backend): `<old> <new> <ident> <ts> <tz>\t<message>\n`
///
/// Called best-effort from [`set_head`] — any I/O failure is silently swallowed
/// so the caller (which has already succeeded in updating HEAD) is not
/// interrupted.
fn append_head_reflog(git_dir: &std::path::Path, old_oid: Option<&str>, new_oid: &str) {
    use std::io::Write as _;

    let zero = "0".repeat(40);
    let old = old_oid.unwrap_or(&zero);

    // Use seconds since epoch for the timestamp; best-effort (fall back to 0).
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());

    // git reflog entry: "old new ident ts tz\tmessage\n"
    // Ident: "maw <maw@localhost>" — the identity that wrote the HEAD.
    let entry = format!("{old} {new_oid} maw <maw@localhost> {ts} +0000\tmaw: set_head (rebase)\n");

    let logs_dir = git_dir.join("logs");
    let log_path = logs_dir.join("HEAD");

    // Create the logs/ directory if it does not exist yet (fresh worktrees
    // may not have one until git itself writes the first reflog entry).
    if !logs_dir.exists() && std::fs::create_dir_all(&logs_dir).is_err() {
        return; // best-effort: give up silently
    }

    // Append mode: create or append.
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = f.write_all(entry.as_bytes());
        // flush but don't fsync — best-effort
    }
}

/// Detach HEAD at `oid` and update the worktree to match.
///
/// Equivalent to `git checkout --detach <oid>` but fully native:
/// - `checkout_tree` materialises the commit's tree into `workdir`.
/// - `set_head` writes the detached HEAD (and a reflog entry, bn-20sa).
///
/// Both steps must succeed; if `checkout_tree` fails, `set_head` is NOT
/// called — the working tree and HEAD are left in their pre-call state.
///
/// Used by `sync_worktree_to_epoch_inner` (the fast-forward sync path) and
/// `advance` (zero-ahead fast-forward). Callers MUST pre-verify that no
/// committed work would be orphaned (the ancestor-refusal guard sits in
/// `sync_worktree_to_epoch_inner`; advance uses `committed_ahead_of_epoch`).
pub fn checkout_detach(repo: &GixRepo, oid: GitOid, workdir: &Path) -> Result<(), GitError> {
    checkout_tree(repo, oid, workdir)?;
    set_head(repo, oid)?;
    Ok(())
}

/// Point HEAD symbolically at `refs/heads/<branch>` and update the worktree.
///
/// Equivalent to `git checkout <branch>` (branch attachment) but fully native:
/// - `checkout_tree` materialises `oid`'s tree into `workdir`.
/// - `set_head_to_branch` writes `ref: refs/heads/<branch>` to HEAD atomically
///   (temp file + rename) and appends a reflog entry.
///
/// Both steps must succeed; if `checkout_tree` fails, HEAD is left unchanged.
///
/// Used by the default-workspace reattach step in `merge.rs`. The branch ref
/// protects commits from orphaning, so no ahead-check is required here.
pub fn checkout_to_branch(
    repo: &GixRepo,
    oid: GitOid,
    workdir: &Path,
    branch: &str,
) -> Result<(), GitError> {
    checkout_tree(repo, oid, workdir)?;
    set_head_to_branch(repo, branch)?;
    Ok(())
}

/// Point HEAD symbolically at `refs/heads/<branch>` (no worktree update).
///
/// Writes `ref: refs/heads/<branch>\n` to the worktree's HEAD file atomically
/// (temp file + rename). Appends a reflog entry so HEAD moves are traceable
/// (matches the bn-20sa convention used by [`set_head`]).
///
/// For a linked worktree, writes to `.git/worktrees/<name>/HEAD`; for a
/// non-worktree repo writes to `.git/HEAD`.
///
/// # Errors
/// Returns a `GitError` if the HEAD file cannot be written.
pub fn set_head_to_branch(repo: &GixRepo, branch: &str) -> Result<(), GitError> {
    use std::io::Write as _;

    let git_dir = repo.repo.git_dir();
    let head_path = git_dir.join("HEAD");
    let tmp_path = git_dir.join("HEAD.maw-tmp");

    // Read old HEAD *before* overwriting — used for the reflog entry.
    let old_oid_str = std::fs::read_to_string(&head_path)
        .ok()
        .map(|s| s.trim().to_owned());

    let full_ref = if branch.starts_with("refs/") {
        branch.to_owned()
    } else {
        format!("refs/heads/{branch}")
    };

    let contents = format!("ref: {full_ref}\n");

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
        let _ = std::fs::remove_file(&tmp_path);
        GitError::BackendError {
            message: format!(
                "failed to rename temp HEAD into place at {}: {e}",
                head_path.display()
            ),
        }
    })?;

    // Best-effort reflog entry — same convention as set_head (bn-20sa).
    // For a symbolic-ref HEAD the "old OID" is whatever string was previously
    // in HEAD (may be a symbolic ref line itself if transitioning from another
    // branch, or a raw OID if transitioning from detached HEAD).
    append_head_reflog(git_dir, old_oid_str.as_deref(), &format!("ref: {full_ref}"));

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
        let stat = gix::index::entry::Stat::default();
        let flags = gix::index::entry::Flags::empty();

        state.dangerously_push_entry(stat, id, flags, mode, ie.path.as_str().into());
    }

    state.sort_entries();

    let index_path = repo.repo.index_path();
    let mut index_file = gix::index::File::from_state(state, index_path);
    index_file
        .write(gix::index::write::Options::default())
        .map_err(|e| GitError::BackendError {
            message: format!("failed to write index: {e}"),
        })?;

    Ok(())
}

const fn gix_mode_to_entry_mode(mode: gix::index::entry::Mode) -> Option<EntryMode> {
    Some(match mode {
        gix::index::entry::Mode::FILE => EntryMode::Blob,
        gix::index::entry::Mode::FILE_EXECUTABLE => EntryMode::BlobExecutable,
        gix::index::entry::Mode::SYMLINK => EntryMode::Link,
        gix::index::entry::Mode::DIR => EntryMode::Tree,
        gix::index::entry::Mode::COMMIT => EntryMode::Commit,
        _ => return None,
    })
}

const fn entry_mode_to_gix_mode(mode: EntryMode) -> gix::index::entry::Mode {
    match mode {
        EntryMode::Blob => gix::index::entry::Mode::FILE,
        EntryMode::BlobExecutable => gix::index::entry::Mode::FILE_EXECUTABLE,
        EntryMode::Link => gix::index::entry::Mode::SYMLINK,
        EntryMode::Tree => gix::index::entry::Mode::DIR,
        EntryMode::Commit => gix::index::entry::Mode::COMMIT,
    }
}

// ---------------------------------------------------------------------------
// Tests for checkout_detach, checkout_to_branch, set_head_to_branch
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use crate::GixRepo;
    use crate::types::{EntryMode, GitOid};

    /// Run a git command in `dir`, panic on failure, return stdout trimmed.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|e| panic!("git {}: {e}", args.join(" ")));
        assert!(
            out.status.success(),
            "git {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Initialise a bare-style repo + a linked worktree for tests.
    ///
    /// Returns `(TempDir, root, worktree_path, commit1_oid, commit2_oid)`.
    ///
    /// Layout:
    ///   root/              ← main repo (non-bare, main branch)
    ///   root/wt/           ← linked worktree detached at commit1
    ///
    /// Both commits have one file each so `checkout_tree` has something to
    /// materialise / remove (exercises stale-file cleanup).
    fn setup_repo_with_linked_worktree() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        String,
        String,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();

        git(&root, &["init", "-q", "--initial-branch=main"]);
        git(&root, &["config", "user.email", "t@t.com"]);
        git(&root, &["config", "user.name", "T"]);
        git(&root, &["config", "commit.gpgsign", "false"]);

        // Commit 1: file1.txt
        fs::write(root.join("file1.txt"), "content1\n").unwrap();
        git(&root, &["add", "file1.txt"]);
        git(&root, &["commit", "-qm", "commit1"]);
        let c1 = git(&root, &["rev-parse", "HEAD"]);

        // Commit 2: add file2.txt (file1.txt still present)
        fs::write(root.join("file2.txt"), "content2\n").unwrap();
        git(&root, &["add", "file2.txt"]);
        git(&root, &["commit", "-qm", "commit2"]);
        let c2 = git(&root, &["rev-parse", "HEAD"]);

        // Create a linked worktree detached at c1.
        let wt = root.join("wt");
        git(
            &root,
            &["worktree", "add", "--detach", wt.to_str().unwrap(), &c1],
        );

        (dir, root, wt, c1, c2)
    }

    /// Read the raw HEAD file content from a worktree.
    fn read_head(wt: &Path) -> String {
        // The worktree's HEAD lives at <wt>/.git (which is a gitfile pointing
        // to the per-worktree admin dir). Parse it to find the actual HEAD.
        let gitfile = wt.join(".git");
        let gitfile_content = fs::read_to_string(&gitfile).unwrap();
        // Content: "gitdir: /repo/.git/worktrees/wt\n"
        let admin_dir = gitfile_content
            .strip_prefix("gitdir: ")
            .unwrap()
            .trim()
            .to_owned();
        let head_path = std::path::PathBuf::from(admin_dir).join("HEAD");
        fs::read_to_string(head_path).unwrap().trim().to_owned()
    }

    /// Assert that a reflog entry was written for the worktree HEAD.
    fn reflog_has_entry(wt: &Path) -> bool {
        let gitfile = wt.join(".git");
        let gitfile_content = fs::read_to_string(&gitfile).unwrap();
        let admin_dir = gitfile_content
            .strip_prefix("gitdir: ")
            .unwrap()
            .trim()
            .to_owned();
        let log_path = std::path::PathBuf::from(admin_dir)
            .join("logs")
            .join("HEAD");
        if !log_path.exists() {
            return false;
        }
        let content = fs::read_to_string(log_path).unwrap();
        !content.trim().is_empty()
    }

    // -----------------------------------------------------------------------
    // set_head_detached_cas tests (bn-302v)
    // -----------------------------------------------------------------------

    fn admin_dir(wt: &Path) -> std::path::PathBuf {
        let content = fs::read_to_string(wt.join(".git")).unwrap();
        std::path::PathBuf::from(content.strip_prefix("gitdir: ").unwrap().trim())
    }

    #[test]
    fn set_head_detached_cas_moves_head_when_expected_matches() {
        let (_dir, _root, wt, c1, c2) = setup_repo_with_linked_worktree();
        let repo = GixRepo::open(&wt).unwrap();
        let (o1, o2): (GitOid, GitOid) = (c1.parse().unwrap(), c2.parse().unwrap());
        repo.set_head_detached_cas(o1, o2).unwrap();
        assert_eq!(read_head(&wt), c2);
        assert!(
            !admin_dir(&wt).join("HEAD.lock").exists(),
            "lock must be released"
        );
        assert!(reflog_has_entry(&wt));
    }

    #[test]
    fn set_head_detached_cas_refuses_when_head_moved() {
        let (_dir, _root, wt, c1, c2) = setup_repo_with_linked_worktree();
        let repo = GixRepo::open(&wt).unwrap();
        let (o1, o2): (GitOid, GitOid) = (c1.parse().unwrap(), c2.parse().unwrap());
        // HEAD is at c1; claim we expected c2.
        let err = repo.set_head_detached_cas(o2, o1).unwrap_err();
        assert!(
            matches!(err, crate::error::GitError::RefConflict { .. }),
            "{err}"
        );
        assert_eq!(read_head(&wt), c1, "HEAD must be unchanged");
        assert!(
            !admin_dir(&wt).join("HEAD.lock").exists(),
            "lock must be released"
        );
    }

    #[test]
    fn set_head_detached_cas_refuses_and_keeps_foreign_head_lock() {
        let (_dir, _root, wt, c1, c2) = setup_repo_with_linked_worktree();
        let repo = GixRepo::open(&wt).unwrap();
        let (o1, o2): (GitOid, GitOid) = (c1.parse().unwrap(), c2.parse().unwrap());
        let lock = admin_dir(&wt).join("HEAD.lock");
        fs::write(&lock, "held by a concurrent git process\n").unwrap();
        let err = repo.set_head_detached_cas(o1, o2).unwrap_err();
        assert!(
            matches!(err, crate::error::GitError::RefConflict { .. }),
            "{err}"
        );
        assert_eq!(read_head(&wt), c1, "HEAD must be unchanged");
        assert!(
            lock.exists(),
            "a lock we did not create must never be removed"
        );
    }

    /// While the CAS holds HEAD.lock, a real `git commit` in the worktree
    /// cannot move HEAD: git refuses on the same lock file.
    #[test]
    fn git_commit_refuses_while_head_lock_is_held() {
        let (_dir, _root, wt, c1, _c2) = setup_repo_with_linked_worktree();
        let lock = admin_dir(&wt).join("HEAD.lock");
        fs::write(&lock, "").unwrap();
        fs::write(wt.join("new.txt"), "x\n").unwrap();
        let add = Command::new("git")
            .args(["add", "new.txt"])
            .current_dir(&wt)
            .status()
            .unwrap();
        assert!(add.success());
        let out = Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=T",
                "commit",
                "-qm",
                "race",
            ])
            .current_dir(&wt)
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "git commit must refuse while HEAD.lock is held"
        );
        assert_eq!(read_head(&wt), c1);
    }

    // -----------------------------------------------------------------------
    // checkout_detach tests
    // -----------------------------------------------------------------------

    /// `checkout_detach` moves HEAD to the target OID (detached) and materialises
    /// the commit's tree into the worktree. File from the old commit absent in
    /// the new one is removed (stale-file cleanup); untracked files are preserved.
    #[test]
    fn checkout_detach_moves_head_and_updates_worktree() {
        let (_dir, _root, wt, _c1, c2) = setup_repo_with_linked_worktree();
        // Worktree starts at c1 (file1.txt present, file2.txt absent).
        assert!(wt.join("file1.txt").exists());
        assert!(!wt.join("file2.txt").exists());

        // Place an untracked file — must survive checkout_detach (bn-29x0).
        fs::write(wt.join("untracked.txt"), "precious\n").unwrap();

        let repo = GixRepo::open(&wt).expect("open worktree repo");
        let oid: GitOid = c2.parse().expect("parse oid");
        super::checkout_detach(&repo, oid, &wt).expect("checkout_detach");

        // HEAD is now the raw OID (detached), not a symbolic ref.
        let head = read_head(&wt);
        assert_eq!(head, c2, "HEAD should be the raw OID after detach");

        // file2.txt from c2 is now present; file1.txt from c1 should still be
        // present (both commits have it, since we only added file2 in commit2).
        assert!(wt.join("file1.txt").exists(), "file1.txt should remain");
        assert!(wt.join("file2.txt").exists(), "file2.txt should appear");

        // Untracked file must survive.
        assert!(
            wt.join("untracked.txt").exists(),
            "untracked file must survive checkout"
        );
        let content = fs::read_to_string(wt.join("untracked.txt")).unwrap();
        assert_eq!(content, "precious\n");

        // Reflog entry must exist.
        assert!(reflog_has_entry(&wt), "reflog entry must be written");
    }

    /// bn-2nnuz: checking out a commit whose entry is `100644` over a worktree
    /// file that is executable (the previous commit had `100755`) clears the
    /// executable bit — both a mode-only change and a mode+content change —
    /// and leaves a clean status; the reverse direction still sets it; a
    /// symlink's target is never chmodded.
    #[cfg(unix)]
    #[test]
    fn checkout_tree_clears_exec_bit_when_target_mode_is_regular() {
        use std::os::unix::fs::PermissionsExt;
        let mode_of = |p: &Path| fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        git(&root, &["init", "-q", "--initial-branch=main"]);
        git(&root, &["config", "user.email", "t@t.com"]);
        git(&root, &["config", "user.name", "T"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        for f in ["mode_only.sh", "mode_content.sh"] {
            fs::write(root.join(f), "#!/bin/sh\n").unwrap();
            fs::set_permissions(root.join(f), fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::write(root.join("plain.txt"), "p\n").unwrap();
        // An executable file OUTSIDE the tree, reached through a tracked symlink.
        let outside = tempfile::tempdir().expect("outside");
        let victim = outside.path().join("victim");
        fs::write(&victim, "x\n").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&victim, root.join("link")).unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "exec"]);
        let c1 = git(&root, &["rev-parse", "HEAD"]);

        git(&root, &["update-index", "--chmod=-x", "mode_only.sh"]);
        fs::write(root.join("mode_content.sh"), "#!/bin/sh\necho hi\n").unwrap();
        git(&root, &["add", "mode_content.sh"]);
        git(&root, &["update-index", "--chmod=-x", "mode_content.sh"]);
        git(&root, &["update-index", "--chmod=+x", "plain.txt"]);
        git(&root, &["commit", "-qm", "flip"]);
        let c2 = git(&root, &["rev-parse", "HEAD"]);

        let wt = root.join("wt");
        git(
            &root,
            &["worktree", "add", "--detach", wt.to_str().unwrap(), &c1],
        );
        assert_eq!(mode_of(&wt.join("mode_only.sh")) & 0o111, 0o111);

        let repo = GixRepo::open(&wt).expect("open worktree repo");
        super::checkout_detach(&repo, c2.parse().unwrap(), &wt).expect("checkout_detach");

        assert_eq!(
            mode_of(&wt.join("mode_only.sh")) & 0o111,
            0,
            "mode-only -x kept +x"
        );
        assert_eq!(
            mode_of(&wt.join("mode_content.sh")) & 0o111,
            0,
            "mode+content -x kept +x"
        );
        assert_ne!(mode_of(&wt.join("plain.txt")) & 0o111, 0, "+x not applied");
        assert_eq!(
            mode_of(&victim) & 0o111,
            0o111,
            "symlink target was chmodded"
        );
        let status = git(&wt, &["status", "--porcelain"]);
        assert!(
            status.is_empty(),
            "worktree must be clean after checkout: {status:?}"
        );
    }

    /// `checkout_detach` back from c2 to c1 removes the file added in c2.
    #[test]
    fn checkout_detach_removes_stale_tracked_files() {
        let (_dir, _root, wt, c1, c2) = setup_repo_with_linked_worktree();
        // Move worktree forward to c2 first (so file2.txt is tracked and present).
        //
        // NB: this must NOT be `git worktree add --force --detach wt c2` —
        // `wt` already exists (populated by `setup_repo_with_linked_worktree`
        // at c1), and `git worktree add`'s target-path check
        // (`check_candidate_path` in builtin/worktree.c: `if (file_exists(path)
        // && !is_empty_dir(path)) die(...)`) is unconditional on `--force` and
        // has been since `--force` was introduced (verified back to git
        // 2.17.0, still true in 2.55.0 per upstream source) — `--force` only
        // ever covered "branch already checked out elsewhere" /
        // "reuse a registered-but-missing worktree's admin dir", never "path
        // exists as a non-empty directory". So this always died with `fatal:
        // '<wt>' already exists`, on every git version. Advance the existing
        // linked worktree in place instead, which is the correct operation
        // for "move an already-checked-out worktree to another commit" and
        // has stable semantics across git versions.
        git(&wt, &["checkout", "--force", &c2]);
        let repo = GixRepo::open(&wt).expect("open");
        let oid_c1: GitOid = c1.parse().expect("parse");
        super::checkout_detach(&repo, oid_c1, &wt).expect("checkout_detach back to c1");
        // file2.txt was tracked in c2 but absent from c1 — must be removed.
        assert!(
            !wt.join("file2.txt").exists(),
            "stale tracked file must be removed"
        );
        assert!(
            wt.join("file1.txt").exists(),
            "file from c1 must be present"
        );
    }

    /// Git paths are byte strings on Unix. Stale-file cleanup must not retain
    /// a deleted tracked file merely because its name is not valid UTF-8.
    #[cfg(unix)]
    #[test]
    fn checkout_detach_removes_non_utf8_stale_tracked_file() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        git(&root, &["init", "-q", "--initial-branch=main"]);
        git(&root, &["config", "user.email", "t@t.com"]);
        git(&root, &["config", "user.name", "T"]);
        git(&root, &["config", "commit.gpgsign", "false"]);

        let invalid_name = OsStr::from_bytes(b"stale-\xff.txt");
        let invalid_path = root.join(invalid_name);
        fs::write(&invalid_path, "stale tracked bytes\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "commit with non-utf8 path"]);
        let with_path = git(&root, &["rev-parse", "HEAD"]);

        fs::remove_file(&invalid_path).unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "delete non-utf8 path"]);
        let without_path = git(&root, &["rev-parse", "HEAD"]);

        let wt = root.join("wt");
        git(
            &root,
            &[
                "worktree",
                "add",
                "--detach",
                wt.to_str().unwrap(),
                &with_path,
            ],
        );
        assert!(wt.join(invalid_name).exists(), "fixture path must exist");

        let repo = GixRepo::open(&wt).expect("open");
        let target: GitOid = without_path.parse().expect("parse target");
        super::checkout_detach(&repo, target, &wt).expect("checkout_detach");

        assert!(
            !wt.join(invalid_name).exists(),
            "a tracked path deleted by the target commit must be removed"
        );
    }

    // -----------------------------------------------------------------------
    // checkout_to_branch tests
    // -----------------------------------------------------------------------

    /// `checkout_to_branch` sets HEAD to a symbolic ref and updates the worktree.
    #[test]
    fn checkout_to_branch_attaches_head_and_updates_worktree() {
        let (_dir, _root, wt, _c1, c2) = setup_repo_with_linked_worktree();
        // Worktree is at c1 detached.
        let repo = GixRepo::open(&wt).expect("open");
        let oid: GitOid = c2.parse().expect("parse");

        // Attach to the 'main' branch (which is at c2 in root).
        super::checkout_to_branch(&repo, oid, &wt, "main").expect("checkout_to_branch");

        // HEAD should be a symbolic ref, not a raw OID.
        let head = read_head(&wt);
        assert_eq!(
            head, "ref: refs/heads/main",
            "HEAD should be symbolic ref after checkout_to_branch"
        );

        // Worktree should be at c2's tree.
        assert!(
            wt.join("file2.txt").exists(),
            "file2.txt from c2 should be present"
        );

        // Reflog entry must exist.
        assert!(reflog_has_entry(&wt), "reflog entry must be written");
    }

    /// `checkout_to_branch` with a full ref path.
    #[test]
    fn checkout_to_branch_full_ref_path() {
        let (_dir, _root, wt, _c1, c2) = setup_repo_with_linked_worktree();
        let repo = GixRepo::open(&wt).expect("open");
        let oid: GitOid = c2.parse().expect("parse");

        super::checkout_to_branch(&repo, oid, &wt, "refs/heads/main")
            .expect("checkout_to_branch full ref");

        let head = read_head(&wt);
        assert_eq!(head, "ref: refs/heads/main");
    }

    // -----------------------------------------------------------------------
    // checkout_force tests (used by git_checkout_force replacement)
    // -----------------------------------------------------------------------

    /// `checkout_force` overwrites tracked modifications (clobbers) and leaves
    /// untracked files alone — equivalent to git checkout --force.
    #[test]
    fn checkout_force_clobbers_tracked_modifications() {
        let (_dir, _root, wt, c1, _c2) = setup_repo_with_linked_worktree();

        // Modify a tracked file.
        fs::write(wt.join("file1.txt"), "modified content\n").unwrap();
        assert_eq!(
            fs::read_to_string(wt.join("file1.txt")).unwrap(),
            "modified content\n"
        );

        // Place an untracked file.
        fs::write(wt.join("untracked2.txt"), "keep me\n").unwrap();

        let repo = GixRepo::open(&wt).expect("open");
        let oid: GitOid = c1.parse().expect("parse");

        // checkout_force should clobber file1.txt back to c1 content.
        super::checkout_tree(&repo, oid, &wt).expect("checkout_tree");
        super::set_head(&repo, oid).expect("set_head");

        let restored = fs::read_to_string(wt.join("file1.txt")).unwrap();
        assert_eq!(restored, "content1\n", "tracked file should be restored");

        // Untracked file preserved.
        assert!(
            wt.join("untracked2.txt").exists(),
            "untracked file must survive"
        );
    }

    // -----------------------------------------------------------------------
    // LFS smudge tests (bn-1ero)
    // -----------------------------------------------------------------------

    /// Build a root repo with `*.bin` tracked as LFS, commit a real-content
    /// blob through maw's own clean-filter writer (so the object lands in the
    /// COMMON `lfs/objects/` store exactly like a real commit/merge would),
    /// then create a linked worktree detached at that commit. Returns
    /// `(TempDir, commit_oid, worktree_path, real_content)`.
    #[cfg(feature = "lfs")]
    fn setup_repo_with_lfs_file_and_linked_worktree()
    -> (tempfile::TempDir, GitOid, std::path::PathBuf, Vec<u8>) {
        use crate::repo::GitRepo as _;
        use crate::types::TreeEdit;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();

        git(&root, &["init", "-q", "--initial-branch=main"]);
        git(&root, &["config", "user.email", "t@t.com"]);
        git(&root, &["config", "user.name", "T"]);
        git(&root, &["config", "commit.gpgsign", "false"]);

        fs::write(
            root.join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();
        git(&root, &["add", ".gitattributes"]);
        git(&root, &["commit", "-qm", "attrs"]);
        let base_oid: GitOid = git(&root, &["rev-parse", "HEAD"])
            .parse()
            .expect("parse base oid");

        let repo = GixRepo::open(&root).expect("open root repo");
        let real_content =
            b"real content for the bn-1ero smudge-in-linked-worktree regression test\n".to_vec();
        let pointer_oid = repo
            .write_blob_with_path(&real_content, "data.bin")
            .expect("write_blob_with_path should store the LFS object + pointer blob");

        let base_commit = repo.read_commit(base_oid).expect("read base commit");
        let new_tree = repo
            .edit_tree(
                base_commit.tree_oid,
                &[TreeEdit::Upsert {
                    path: "data.bin".to_string(),
                    mode: EntryMode::Blob,
                    oid: pointer_oid,
                }],
            )
            .expect("edit_tree");
        let commit_oid = repo
            .create_commit(new_tree, &[base_oid], "add data.bin", None)
            .expect("create_commit");

        // Create the linked worktree with the real git-lfs smudge filter
        // disabled (`GIT_LFS_SKIP_SMUDGE=1` — a real, documented git-lfs
        // config knob), so `data.bin` lands on disk as the raw pointer text
        // regardless of whether the *test machine* happens to have git-lfs
        // installed. That decouples this test from the environment: without
        // this, a machine with git-lfs installed would have `git worktree
        // add` itself resolve the pointer (git-lfs and maw-lfs share the
        // exact same on-disk object layout under `<git-dir>/lfs/objects/`),
        // masking the very bug under test. This is the same starting state a
        // freshly-created maw workspace worktree is in before maw's own
        // native smudge pass (which never shells out to git-lfs) runs.
        let wt = root.join("wt");
        git_no_lfs_smudge(
            &root,
            &[
                "worktree",
                "add",
                "--detach",
                wt.to_str().expect("utf8 path"),
                &commit_oid.to_string(),
            ],
        );

        (dir, commit_oid, wt, real_content)
    }

    /// Like [`git`], but disables git-lfs's own smudge filter for the
    /// duration of the call (see [`setup_repo_with_lfs_file_and_linked_worktree`]).
    #[cfg(feature = "lfs")]
    fn git_no_lfs_smudge(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_LFS_SKIP_SMUDGE", "1")
            .output()
            .unwrap_or_else(|e| panic!("git {}: {e}", args.join(" ")));
        assert!(
            out.status.success(),
            "git {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Regression test (bn-1ero, symptom 1 / test a): `checkout_tree`'s LFS
    /// smudge post-pass must resolve pointer blobs to real content using the
    /// shared **common** git dir, not the per-worktree admin dir. Every maw
    /// workspace other than the default one is a linked worktree —
    /// `GixRepo::git_dir()` there returns `<common>/worktrees/<name>/`, which
    /// has no `lfs/objects/` of its own — so looking the object up there
    /// always missed, even though it was sitting right there in
    /// `<common>/lfs/objects/`. This is exactly the shape
    /// `populate_from_snapshot` (`maw ws recover --to`) uses.
    #[cfg(feature = "lfs")]
    #[test]
    fn checkout_tree_smudges_lfs_pointer_in_linked_worktree_via_common_store() {
        let (_dir, commit_oid, wt, real_content) = setup_repo_with_lfs_file_and_linked_worktree();

        // Confirm the starting state really is unsmudged pointer text (sanity
        // check that the test setup exercises the bug, not a no-op).
        let before = fs::read(wt.join("data.bin")).expect("read data.bin before checkout_tree");
        assert!(
            maw_lfs::looks_like_pointer(&before),
            "test setup sanity: worktree should start with pointer text"
        );

        let wt_repo = GixRepo::open(&wt).expect("open worktree repo");
        super::checkout_tree(&wt_repo, commit_oid, &wt).expect("checkout_tree");

        let on_disk = fs::read(wt.join("data.bin")).expect("read data.bin after checkout_tree");
        assert_eq!(
            on_disk, real_content,
            "checkout_tree must smudge the LFS pointer to real content by reading the \
             COMMON lfs store, not the per-worktree one"
        );
    }

    /// Regression test (bn-1ero, test b): when the LFS object is NOT present
    /// in the local store, `checkout_tree`'s smudge pass must leave the
    /// pointer text on disk untouched (never fabricate content, never crash)
    /// — the fallback behavior this bone explicitly must not weaken.
    #[cfg(feature = "lfs")]
    #[test]
    fn checkout_tree_leaves_pointer_when_lfs_object_missing_from_store() {
        let (dir, commit_oid, wt, _real_content) = setup_repo_with_lfs_file_and_linked_worktree();

        // Delete the object from the COMMON store to simulate "never
        // fetched" / "gc'd locally".
        let root = dir.path();
        let objects_dir = root.join(".git").join("lfs").join("objects");
        std::fs::remove_dir_all(&objects_dir).expect("remove lfs objects dir");

        let before = fs::read(wt.join("data.bin")).expect("read data.bin before checkout_tree");
        assert!(maw_lfs::looks_like_pointer(&before));

        let wt_repo = GixRepo::open(&wt).expect("open worktree repo");
        super::checkout_tree(&wt_repo, commit_oid, &wt).expect("checkout_tree");

        let after = fs::read(wt.join("data.bin")).expect("read data.bin after checkout_tree");
        assert_eq!(
            after, before,
            "with the object missing from the local store, checkout_tree must leave the \
             pointer text exactly as checked out — no fabricated content, no crash"
        );
        assert!(maw_lfs::looks_like_pointer(&after));
    }
}
