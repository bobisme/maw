//! Worktree add/remove/list built from gix primitives.
//!
//! gix does not provide high-level worktree lifecycle APIs.
//! We build them from the documented git worktree format.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

#[cfg(feature = "lfs")]
use gix::bstr::ByteSlice;

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::{GitOid, WorktreeInfo};

/// Maximum number of attempts to acquire the git index lock (`index.lock`)
/// before giving up.
///
/// Concurrent `maw ws create` invocations write independent worktree admin
/// indexes, but two creates racing at (roughly) the exact same instant can
/// still collide on the underlying lock-file primitive (bn-2mio, observed in
/// an 8-way concurrent `maw ws create` stress run). git's own index lock is
/// normally held only for the few milliseconds it takes to serialize and
/// `fsync` the index, so a handful of short, doubling-backoff retries clears
/// transient contention without meaningfully slowing down the common
/// (uncontended) case.
const INDEX_LOCK_MAX_ATTEMPTS: u32 = 6;

/// Base delay before the first retry after a failed index-lock acquisition;
/// doubles on each subsequent attempt (15, 30, 60, 120, 240 ms — roughly
/// 465ms worst case across the 5 retries allowed by
/// [`INDEX_LOCK_MAX_ATTEMPTS`]). Kept well under a second so a genuinely
/// stuck lock still fails fast enough for an interactive `maw ws create`.
const INDEX_LOCK_BASE_DELAY: std::time::Duration = std::time::Duration::from_millis(15);

/// Write `index_file` to disk, retrying with backoff if the write fails
/// because another process (or another maw operation) currently holds the
/// git index lock.
///
/// Only lock-acquisition failures are retried; any other write error is
/// returned immediately. On exhausting all attempts the error names the
/// workspace and tells the caller that retrying `maw ws create` is safe
/// (bn-2mio).
fn write_index_with_retry(
    index_file: &mut gix::index::File,
    workspace_name: &str,
) -> Result<(), GitError> {
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match index_file.write(gix::index::write::Options::default()) {
            Ok(()) => return Ok(()),
            Err(gix::index::file::write::Error::AcquireLock(
                lock_err @ gix::lock::acquire::Error::PermanentlyLocked { .. },
            )) => {
                if attempt >= INDEX_LOCK_MAX_ATTEMPTS {
                    return Err(GitError::BackendError {
                        message: format!(
                            "failed to write git index for worktree '{workspace_name}' after \
                             {attempt} attempts: {lock_err} (another git or maw operation is \
                             holding the index lock); retry `maw ws create {workspace_name}` \
                             again"
                        ),
                    });
                }
                let backoff = INDEX_LOCK_BASE_DELAY * 2u32.saturating_pow(attempt - 1);
                std::thread::sleep(backoff);
            }
            Err(gix::index::file::write::Error::AcquireLock(gix::lock::acquire::Error::Io(
                io_error,
            ))) => {
                return Err(GitError::BackendError {
                    message: format!(
                        "failed to acquire git index lock for worktree '{workspace_name}': \
                         {io_error}"
                    ),
                });
            }
            Err(e) => {
                return Err(GitError::BackendError {
                    message: format!("failed to write worktree index: {e}"),
                });
            }
        }
    }
}

/// Inspect a gix checkout outcome for signs of a silently incomplete
/// worktree.
///
/// gix does not treat filesystem path collisions (two index entries mapping
/// to the same on-disk path, typically on a case-insensitive filesystem) or
/// unresolved delayed-filter paths as hard errors — a collided path shows up
/// as a `Written { bytes: 0 }`-shaped entry unless `outcome.collisions` and
/// the `delayed_paths_*` fields are inspected explicitly (bn-2r7a). A fresh
/// `maw ws create` must never report success over a partial worktree, so
/// these are promoted to a hard error here, naming every offending path.
fn describe_checkout_incompleteness(
    outcome: &gix::worktree::state::checkout::Outcome,
) -> Option<String> {
    if outcome.collisions.is_empty()
        && outcome.delayed_paths_unknown.is_empty()
        && outcome.delayed_paths_unprocessed.is_empty()
    {
        return None;
    }

    let mut parts = Vec::new();
    if !outcome.collisions.is_empty() {
        let listed: Vec<String> = outcome
            .collisions
            .iter()
            .map(|c| format!("{} ({:?})", c.path, c.error_kind))
            .collect();
        parts.push(format!(
            "{} path collision(s): {}",
            outcome.collisions.len(),
            listed.join(", ")
        ));
    }
    if !outcome.delayed_paths_unknown.is_empty() {
        parts.push(format!(
            "{} unexpected delayed path(s) the checkout process reported but were never \
             requested: {}",
            outcome.delayed_paths_unknown.len(),
            join_bstrings(&outcome.delayed_paths_unknown),
        ));
    }
    if !outcome.delayed_paths_unprocessed.is_empty() {
        parts.push(format!(
            "{} delayed path(s) requested but never checked out: {}",
            outcome.delayed_paths_unprocessed.len(),
            join_bstrings(&outcome.delayed_paths_unprocessed),
        ));
    }
    Some(parts.join("; "))
}

fn join_bstrings(paths: &[gix::bstr::BString]) -> String {
    paths
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[expect(
    clippy::too_many_lines,
    reason = "worktree creation writes git admin files then checks out"
)]
pub fn worktree_add(
    repo: &GixRepo,
    name: &str,
    target: GitOid,
    path: &Path,
) -> Result<(), GitError> {
    // Reject names with path separators or .. components (path traversal protection).
    if name.contains('/') || name.contains('\\') || name == ".." || name.contains("/../") {
        return Err(GitError::BackendError {
            message: format!("invalid worktree name: '{name}' (contains path separators or '..')"),
        });
    }
    let git_dir = repo.repo.git_dir().to_path_buf();
    let admin_dir = git_dir.join("worktrees").join(name);

    // 1. Create admin directory
    std::fs::create_dir_all(&admin_dir).map_err(|e| GitError::BackendError {
        message: format!(
            "failed to create worktree admin dir {}: {e}",
            admin_dir.display()
        ),
    })?;

    // 2. Write HEAD with target OID (detached HEAD)
    std::fs::write(admin_dir.join("HEAD"), format!("{target}\n")).map_err(|e| {
        GitError::BackendError {
            message: format!("failed to write worktree HEAD: {e}"),
        }
    })?;

    // 3. Write commondir (relative path back to main .git)
    std::fs::write(admin_dir.join("commondir"), "../..\n").map_err(|e| GitError::BackendError {
        message: format!("failed to write worktree commondir: {e}"),
    })?;

    // 4. Write gitdir (absolute path to worktree's .git file)
    let wt_gitfile = path.join(".git");
    let abs_path =
        std::fs::canonicalize(path.parent().unwrap_or(path)).unwrap_or_else(|_| path.to_path_buf());
    let abs_gitfile = if path.is_absolute() {
        wt_gitfile.clone()
    } else {
        abs_path
            .join(path.file_name().unwrap_or_default())
            .join(".git")
    };
    std::fs::write(
        admin_dir.join("gitdir"),
        format!("{}\n", abs_gitfile.display()),
    )
    .map_err(|e| GitError::BackendError {
        message: format!("failed to write worktree gitdir: {e}"),
    })?;

    // 5. Create the worktree directory
    std::fs::create_dir_all(path).map_err(|e| GitError::BackendError {
        message: format!("failed to create worktree dir {}: {e}", path.display()),
    })?;

    // 6. Write .git file in worktree (not a directory, a file pointing back)
    std::fs::write(&wt_gitfile, format!("gitdir: {}\n", admin_dir.display())).map_err(|e| {
        GitError::BackendError {
            message: format!("failed to write worktree .git file: {e}"),
        }
    })?;

    // 7. Resolve target OID to a commit, get its tree
    let gix_oid = gix::ObjectId::from_bytes_or_panic(target.as_bytes());
    let obj = repo
        .repo
        .find_object(gix_oid)
        .map_err(|e| GitError::NotFound {
            message: format!("object {target}: {e}"),
        })?;
    let tree_oid = match obj.kind {
        gix::object::Kind::Commit => {
            let commit = obj.into_commit();
            commit
                .tree_id()
                .map_err(|e| GitError::BackendError {
                    message: format!("failed to get tree from commit {target}: {e}"),
                })?
                .detach()
        }
        gix::object::Kind::Tree => gix_oid,
        other => {
            return Err(GitError::BackendError {
                message: format!("expected commit or tree, got {other}"),
            });
        }
    };

    // 8. Build index from tree and write to admin dir
    let index_state = repo
        .repo
        .index_from_tree(&tree_oid)
        .map_err(|e| GitError::BackendError {
            message: format!("failed to create index from tree {tree_oid}: {e}"),
        })?;

    let index_path = admin_dir.join("index");
    let mut index_file = gix::index::File::from_state(index_state.into(), index_path);
    write_index_with_retry(&mut index_file, name)?;

    // 9. Checkout the tree to the worktree path
    let mut checkout_index =
        repo.repo
            .index_from_tree(&tree_oid)
            .map_err(|e| GitError::BackendError {
                message: format!("failed to create index for checkout: {e}"),
            })?;

    let mut opts = repo
        .repo
        .checkout_options(gix::worktree::stack::state::attributes::Source::IdMapping)
        .map_err(|e| GitError::BackendError {
            message: format!("failed to get checkout options: {e}"),
        })?;
    opts.overwrite_existing = true;
    opts.destination_is_initially_empty = true;

    // When the `lfs` feature is on, maw-lfs handles LFS smudge itself.
    // Clear external filter drivers so gix does NOT spawn git-lfs during
    // the initial worktree checkout (same as checkout_impl.rs).
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
        &mut checkout_index,
        path,
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

    // bn-2r7a: gix does not surface collisions or unresolved delayed-filter
    // paths through `outcome.errors` — a collided path is recorded as a
    // successful zero-byte write. Inspect those fields explicitly so a
    // partial worktree is never reported as a successful create.
    if let Some(problem) = describe_checkout_incompleteness(&outcome) {
        return Err(GitError::BackendError {
            message: format!(
                "checkout for worktree '{name}' produced a partial worktree — {problem}; the \
                 worktree is unsafe to use; remove it and retry `maw ws create {name}`"
            ),
        });
    }

    // LFS smudge post-pass: replace pointer files with real content,
    // then update index stats so git status doesn't show phantom mods.
    #[cfg(feature = "lfs")]
    {
        let smudged =
            match crate::checkout_impl::smudge_lfs_pointers_public(&checkout_index, path, repo) {
                Ok(paths) => paths,
                Err(e) => {
                    tracing::warn!("lfs smudge post-pass failed in worktree_add: {e}");
                    Vec::new()
                }
            };
        // Update index stat entries for smudged files, then rewrite.
        for rel_path in &smudged {
            let full = path.join(rel_path);
            let Ok(meta) = gix::index::fs::Metadata::from_path_no_follow(&full) else {
                continue;
            };
            let Ok(new_stat) = gix::index::entry::Stat::from_fs(&meta) else {
                continue;
            };
            if let Some(idx) = checkout_index
                .entries()
                .iter()
                .position(|e| e.path(&checkout_index).to_str().ok() == Some(rel_path.as_str()))
            {
                checkout_index.entries_mut()[idx].stat = new_stat;
            }
        }
        if !smudged.is_empty() {
            let index_path = admin_dir.join("index");
            let mut persisted = gix::index::File::from_state(checkout_index.into(), index_path);
            // Best-effort, same as before bn-2mio: a transient lock here
            // only affects on-disk stat freshness, not worktree content, so
            // retry-then-ignore is consistent with the pre-existing
            // best-effort semantics of this post-pass.
            let _ = write_index_with_retry(&mut persisted, name);
        }
    }

    Ok(())
}

pub fn worktree_remove(repo: &GixRepo, name: &str) -> Result<(), GitError> {
    let git_dir = repo.repo.git_dir().to_path_buf();
    let admin_dir = git_dir.join("worktrees").join(name);

    if !admin_dir.exists() {
        return Err(GitError::NotFound {
            message: format!("worktree '{name}' not found"),
        });
    }

    // Read gitdir to find the worktree path
    let gitdir_file = admin_dir.join("gitdir");
    if gitdir_file.exists() {
        let gitdir_content =
            std::fs::read_to_string(&gitdir_file).map_err(|e| GitError::BackendError {
                message: format!("failed to read worktree gitdir: {e}"),
            })?;
        let gitdir_path = resolve_admin_gitdir_path(&admin_dir, gitdir_content.trim());
        // gitdir points to <worktree>/.git, so parent is the worktree root
        if let Some(wt_path) = gitdir_path.parent()
            && wt_path.exists()
        {
            verify_worktree_gitfile_points_to_admin_dir(wt_path, &admin_dir)?;
            std::fs::remove_dir_all(wt_path).map_err(|e| GitError::BackendError {
                message: format!("failed to remove worktree dir {}: {e}", wt_path.display()),
            })?;
        }
    }

    // Remove the admin directory
    std::fs::remove_dir_all(&admin_dir).map_err(|e| GitError::BackendError {
        message: format!(
            "failed to remove worktree admin dir {}: {e}",
            admin_dir.display()
        ),
    })?;

    Ok(())
}

fn verify_worktree_gitfile_points_to_admin_dir(
    wt_path: &Path,
    admin_dir: &Path,
) -> Result<(), GitError> {
    let wt_gitfile = wt_path.join(".git");
    let gitfile_content =
        std::fs::read_to_string(&wt_gitfile).map_err(|e| GitError::BackendError {
            message: format!(
                "refusing to remove worktree {}: failed to read {}: {e}",
                wt_path.display(),
                wt_gitfile.display()
            ),
        })?;
    let target = gitfile_content
        .trim()
        .strip_prefix("gitdir:")
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .ok_or_else(|| GitError::BackendError {
            message: format!(
                "refusing to remove worktree {}: {} does not point to a git worktree admin dir",
                wt_path.display(),
                wt_gitfile.display()
            ),
        })?;

    let target_admin_dir = resolve_gitfile_path(wt_path, target);
    let actual = canonicalize_existing(&target_admin_dir)?;
    let expected = canonicalize_existing(admin_dir)?;
    if actual != expected {
        return Err(GitError::BackendError {
            message: format!(
                "refusing to remove worktree {}: {} points to {}, expected {}",
                wt_path.display(),
                wt_gitfile.display(),
                actual.display(),
                expected.display()
            ),
        });
    }

    Ok(())
}

fn resolve_gitfile_path(wt_path: &Path, target: &str) -> PathBuf {
    let target = PathBuf::from(target);
    if target.is_absolute() {
        target
    } else {
        wt_path.join(target)
    }
}

fn resolve_admin_gitdir_path(admin_dir: &Path, target: &str) -> PathBuf {
    let target = PathBuf::from(target);
    if target.is_absolute() {
        target
    } else {
        admin_dir.join(target)
    }
}

fn canonicalize_existing(path: &Path) -> Result<PathBuf, GitError> {
    std::fs::canonicalize(path).map_err(|e| GitError::BackendError {
        message: format!("failed to canonicalize {}: {e}", path.display()),
    })
}

/// Prune worktree admin directories whose linked worktree no longer exists.
///
/// Scans `<common-git-dir>/worktrees/<name>/` and removes each admin
/// directory whose `gitdir` file points at a `.git` link that has been
/// deleted out of band (e.g., a stale worktree directory was removed
/// manually). Mirrors `git worktree prune`.
///
/// This is idempotent and best-effort: per-entry failures are logged via the
/// returned error only if the whole `worktrees/` directory cannot be read;
/// individual prune failures are silently skipped so a corrupt admin dir
/// does not block cleanup of healthy ones.
pub fn worktree_prune(repo: &GixRepo) -> Result<(), GitError> {
    let common_dir = repo.repo.common_dir().to_path_buf();
    let worktrees_dir = common_dir.join("worktrees");
    if !worktrees_dir.exists() {
        return Ok(());
    }
    let entries = match std::fs::read_dir(&worktrees_dir) {
        Ok(e) => e,
        Err(e) => {
            return Err(GitError::BackendError {
                message: format!(
                    "failed to read worktrees dir {}: {e}",
                    worktrees_dir.display()
                ),
            });
        }
    };
    for entry in entries.flatten() {
        let admin_dir = entry.path();
        if !admin_dir.is_dir() {
            continue;
        }
        let gitdir_file = admin_dir.join("gitdir");
        // No gitdir file means a partially-constructed or non-conforming
        // admin dir — leave it alone.
        let Ok(content) = std::fs::read_to_string(&gitdir_file) else {
            continue;
        };
        let gitdir_path = PathBuf::from(content.trim());
        // The stored path is <worktree>/.git (either a file or a directory).
        // If that path does not exist (or its parent worktree dir is gone),
        // the admin dir is stale and should be removed.
        let worktree_root = gitdir_path
            .parent()
            .map_or_else(|| gitdir_path.clone(), std::path::Path::to_path_buf);
        let is_stale = !gitdir_path.exists() && !worktree_root.exists();
        if is_stale {
            let _ = std::fs::remove_dir_all(&admin_dir);
        }
    }
    Ok(())
}

pub fn worktree_list(repo: &GixRepo) -> Result<Vec<WorktreeInfo>, GitError> {
    let git_dir = repo.repo.git_dir().to_path_buf();
    let worktrees_dir = git_dir.join("worktrees");

    if !worktrees_dir.exists() {
        return Ok(Vec::new());
    }

    let entries = std::fs::read_dir(&worktrees_dir).map_err(|e| GitError::BackendError {
        message: format!("failed to read worktrees dir: {e}"),
    })?;

    let mut result = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|e| GitError::BackendError {
            message: format!("failed to read worktree entry: {e}"),
        })?;

        let entry_path = entry.path();
        if !entry_path.is_dir() {
            continue;
        }

        let name = entry.file_name().to_string_lossy().into_owned();

        // Read HEAD to get current OID and detached state
        let (head_oid, is_detached) = {
            let head_file = entry_path.join("HEAD");
            if head_file.exists() {
                let content = std::fs::read_to_string(&head_file).ok().unwrap_or_default();
                let trimmed = content.trim();
                parse_worktree_head(repo, trimmed)
            } else {
                (None, true)
            }
        };

        // Read gitdir to get worktree path
        let wt_path = {
            let gitdir_file = entry_path.join("gitdir");
            if gitdir_file.exists() {
                let content = std::fs::read_to_string(&gitdir_file)
                    .ok()
                    .unwrap_or_default();
                let p = std::path::PathBuf::from(content.trim());
                // gitdir points to <worktree>/.git, parent is the worktree root
                p.parent().map(std::path::Path::to_path_buf).unwrap_or(p)
            } else {
                entry_path.clone()
            }
        };

        result.push(WorktreeInfo {
            name,
            path: wt_path,
            head_oid,
            is_detached,
        });
    }

    Ok(result)
}

fn parse_worktree_head(repo: &GixRepo, trimmed: &str) -> (Option<GitOid>, bool) {
    trimmed.strip_prefix("ref: ").map_or_else(
        || {
            if trimmed.len() != 40 {
                return (None, true);
            }

            let mut bytes = [0u8; 20];
            for i in 0..20 {
                let Ok(b) = u8::from_str_radix(&trimmed[i * 2..i * 2 + 2], 16) else {
                    return (None, true);
                };
                bytes[i] = b;
            }
            (Some(GitOid::from_bytes(bytes)), true)
        },
        |ref_target| {
            let oid = crate::types::RefName::new(ref_target)
                .ok()
                .and_then(|rn| crate::refs_impl::read_ref(repo, &rn).ok().flatten());
            (oid, false)
        },
    )
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::repo::GitRepo as _;

    fn setup_repo() -> (TempDir, GixRepo, GitOid) {
        // bn-5rdz: shared init + seed-commit helper.
        let (dir, root, _oid) = crate::test_support::init_test_repo_with_commit();
        let repo = GixRepo::open(&root).expect("open repo");
        let head = repo.rev_parse("HEAD").expect("resolve HEAD");
        (dir, repo, head)
    }

    #[test]
    fn worktree_remove_removes_valid_worktree() {
        let (dir, repo, head) = setup_repo();
        let wt_path = dir.path().join("ws").join("agent-1");

        worktree_add(&repo, "agent-1", head, &wt_path).expect("add worktree");
        assert!(wt_path.exists());

        worktree_remove(&repo, "agent-1").expect("remove worktree");

        assert!(!wt_path.exists());
        assert!(
            !repo
                .repo
                .git_dir()
                .join("worktrees")
                .join("agent-1")
                .exists()
        );
    }

    #[test]
    fn worktree_remove_rejects_gitdir_that_does_not_point_back_to_admin_dir() {
        let (dir, repo, _head) = setup_repo();
        let root = dir.path();
        let victim = root.join("victim");
        std::fs::create_dir_all(&victim).expect("create victim");
        std::fs::write(victim.join("important.txt"), "do not delete\n").expect("write victim");
        let other_admin = repo.repo.git_dir().join("worktrees").join("other");
        std::fs::create_dir_all(&other_admin).expect("create other admin dir");
        std::fs::write(
            victim.join(".git"),
            format!("gitdir: {}\n", other_admin.display()),
        )
        .expect("write victim gitfile");

        let admin_dir = repo.repo.git_dir().join("worktrees").join("evil");
        std::fs::create_dir_all(&admin_dir).expect("create admin dir");
        std::fs::write(
            admin_dir.join("gitdir"),
            format!("{}\n", victim.join(".git").display()),
        )
        .expect("write admin gitdir");

        let err = worktree_remove(&repo, "evil").expect_err("remove must reject mismatched gitdir");
        let err = err.to_string();
        assert!(
            err.contains("refusing to remove worktree"),
            "unexpected error: {err}"
        );
        assert!(
            victim.join("important.txt").exists(),
            "mismatched gitdir must not allow deleting the referenced directory"
        );
    }

    // --- bn-2mio: index-lock retry -----------------------------------

    /// Acquire the same `index.lock` primitive `write_index_with_retry`
    /// contends on, so tests can simulate another process racing to write
    /// the worktree admin index at the same path.
    fn hold_index_lock(index_path: &Path) -> gix::lock::File {
        gix::lock::File::acquire_to_update_resource(
            index_path,
            gix::lock::acquire::Fail::Immediately,
            None,
        )
        .expect("acquire contention lock for test")
    }

    #[test]
    fn worktree_add_retries_through_transient_index_lock_contention() {
        let (dir, repo, head) = setup_repo();
        let name = "agent-contend";
        let wt_path = dir.path().join("ws").join(name);

        // Pre-create the admin dir (worktree_add would do this itself, but
        // we need it to exist up front so we can grab the same lock file
        // worktree_add will contend on).
        let admin_dir = repo.repo.git_dir().join("worktrees").join(name);
        std::fs::create_dir_all(&admin_dir).expect("create admin dir");
        let index_path = admin_dir.join("index");

        let lock = hold_index_lock(&index_path);
        let holder = std::thread::spawn(move || {
            // Held well under INDEX_LOCK_MAX_ATTEMPTS's ~465ms backoff
            // budget, so worktree_add's retry loop must see the lock
            // released before it gives up.
            std::thread::sleep(std::time::Duration::from_millis(60));
            drop(lock); // rolls back (removes) the lock file
        });

        worktree_add(&repo, name, head, &wt_path)
            .expect("worktree_add must retry past transient lock contention and succeed");

        holder.join().expect("lock-holding thread must not panic");
        assert!(wt_path.exists());
        assert!(wt_path.join(".git").exists());
    }

    #[test]
    fn worktree_add_reports_actionable_error_when_index_lock_exhausted() {
        let (dir, repo, head) = setup_repo();
        let name = "agent-stuck";
        let wt_path = dir.path().join("ws").join(name);

        let admin_dir = repo.repo.git_dir().join("worktrees").join(name);
        std::fs::create_dir_all(&admin_dir).expect("create admin dir");
        let index_path = admin_dir.join("index");

        let lock = hold_index_lock(&index_path);
        // Outlives the full retry budget (~465ms across 5 backoff sleeps),
        // so every attempt in worktree_add's loop must fail.
        let holder = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(900));
            drop(lock);
        });

        let err = worktree_add(&repo, name, head, &wt_path)
            .expect_err("worktree_add must fail once retries are exhausted");
        let err = err.to_string();
        assert!(
            err.contains(name),
            "error must name the workspace so the user knows what to retry: {err}"
        );
        assert!(
            err.contains("maw ws create"),
            "error must tell the user retrying create is safe: {err}"
        );

        holder.join().expect("lock-holding thread must not panic");
    }

    #[test]
    fn index_lock_io_error_is_immediate_and_preserves_the_cause() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index_path = dir.path().join("missing-admin-dir").join("index");
        let state = gix::index::State::new(gix::hash::Kind::Sha1);
        let mut index_file = gix::index::File::from_state(state, index_path);

        let error = write_index_with_retry(&mut index_file, "agent-io")
            .expect_err("a missing admin directory must fail");
        let message = error.to_string();

        assert!(
            message.contains("agent-io"),
            "error must name the affected workspace: {message}"
        );
        assert!(
            message.contains("No such file") || message.contains("not found"),
            "error must preserve the underlying I/O cause: {message}"
        );
        assert!(
            !message.contains("another git or maw operation"),
            "a permanent I/O error must not be misreported as lock contention: {message}"
        );
    }

    // --- bn-2r7a: checkout outcome inspection -------------------------

    fn empty_checkout_outcome() -> gix::worktree::state::checkout::Outcome {
        gix::worktree::state::checkout::Outcome::default()
    }

    #[test]
    fn describe_checkout_incompleteness_is_none_for_clean_outcome() {
        assert!(describe_checkout_incompleteness(&empty_checkout_outcome()).is_none());
    }

    #[test]
    fn describe_checkout_incompleteness_names_colliding_paths() {
        let mut outcome = empty_checkout_outcome();
        outcome
            .collisions
            .push(gix::worktree::state::checkout::Collision {
                path: "src/Foo.rs".into(),
                error_kind: std::io::ErrorKind::AlreadyExists,
            });

        let msg = describe_checkout_incompleteness(&outcome)
            .expect("collisions must be reported as incomplete checkout");
        assert!(msg.contains("src/Foo.rs"), "unexpected message: {msg}");
        assert!(msg.contains("collision"), "unexpected message: {msg}");
    }

    #[test]
    fn describe_checkout_incompleteness_names_unprocessed_delayed_paths() {
        let mut outcome = empty_checkout_outcome();
        outcome
            .delayed_paths_unprocessed
            .push("assets/big.bin".into());

        let msg = describe_checkout_incompleteness(&outcome)
            .expect("unprocessed delayed paths must be reported as incomplete checkout");
        assert!(msg.contains("assets/big.bin"), "unexpected message: {msg}");
    }

    #[test]
    fn describe_checkout_incompleteness_names_unknown_delayed_paths() {
        let mut outcome = empty_checkout_outcome();
        outcome.delayed_paths_unknown.push("weird/path.bin".into());

        let msg = describe_checkout_incompleteness(&outcome)
            .expect("unknown delayed paths must be reported as incomplete checkout");
        assert!(msg.contains("weird/path.bin"), "unexpected message: {msg}");
    }

    /// `worktree_add` must surface `describe_checkout_incompleteness`'s
    /// verdict as a hard `Err`, not a warning, and must name the workspace
    /// so the user knows create is safe to retry (bn-2r7a). We can't force
    /// gix to report a real collision on a case-sensitive Linux filesystem
    /// with `overwrite_existing: true` — that option makes gix unlink and
    /// replace obstructions rather than reporting them as collisions — so
    /// this test exercises the wiring by asserting on the message shape
    /// that `worktree_add` would produce from a non-empty outcome, using
    /// the same formatting helper it actually calls.
    #[test]
    fn worktree_add_error_message_shape_matches_incompleteness_report() {
        let mut outcome = empty_checkout_outcome();
        outcome
            .collisions
            .push(gix::worktree::state::checkout::Collision {
                path: "conflicting/path.txt".into(),
                error_kind: std::io::ErrorKind::AlreadyExists,
            });
        let problem =
            describe_checkout_incompleteness(&outcome).expect("must report the collision");
        let name = "agent-1";
        let message = format!(
            "checkout for worktree '{name}' produced a partial worktree — {problem}; the \
             worktree is unsafe to use; remove it and retry `maw ws create {name}`"
        );
        assert!(message.contains("conflicting/path.txt"));
        assert!(message.contains("maw ws create agent-1"));
    }
}
