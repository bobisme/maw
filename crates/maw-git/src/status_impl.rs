//! gix-backed status and dirty detection.

use gix::bstr::ByteSlice;
use gix::status::index_worktree::iter::Summary;

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::{FileStatus, StatusEntry};

// ---------------------------------------------------------------------------
// Linked-worktree `info/exclude` compatibility (bn-auu5)
// ---------------------------------------------------------------------------
//
// gix loads `info/exclude` from `$GIT_DIR/info/exclude`
// (`assemble_exclude_globals(self.git_dir(), …)` →
// `gix_ignore::Search::from_git_dir`). For a *linked worktree* `$GIT_DIR`
// is `.git/worktrees/<name>`, so gix reads
// `.git/worktrees/<name>/info/exclude` and **misses** the common-dir file
// `.git/info/exclude`.
//
// Real git does the opposite: `setup_standard_excludes` resolves
// `info/exclude` through `git_path("info/exclude")`, and `info/*` is on git's
// common-dir list — so git reads `$GIT_COMMON_DIR/info/exclude`
// (the main `.git/info/exclude`) and ignores the per-worktree one.
//
// The upshot is that an entry a user adds to `.git/info/exclude` is honored by
// `git status` but was still reported as untracked by gix — which made
// `maw ws sync` refuse over "excluded" scratch (mess field report, bn-1m4d
// item 5). We close the gap by additionally honoring the common-dir
// `info/exclude` for linked worktrees, matching git.
//
// This ONLY ever suppresses entries gix classified as **untracked**
// (`Status::Untracked` directory-walk leaves); it never touches tracked,
// staged, modified, or deleted paths — so no committed/staged work can be
// hidden from a merge snapshot (Prime Invariant).

/// A matcher for the common-dir `info/exclude` patterns that gix omits on a
/// linked worktree.
struct CommonInfoExclude {
    search: gix::ignore::Search,
    case: gix::glob::pattern::Case,
}

impl CommonInfoExclude {
    /// Build a matcher, or `None` when there is nothing to add:
    /// * the repo is the main worktree (`git_dir == common_dir` — gix already
    ///   reads `info/exclude` from the right place), or
    /// * the common-dir `info/exclude` is absent or empty.
    fn build(repo: &GixRepo) -> Option<Self> {
        let git_dir = repo.repo.git_dir();
        let common_dir = repo.repo.common_dir();
        // Main worktree: gix's `$GIT_DIR/info/exclude` IS the common-dir file.
        if git_dir == common_dir {
            return None;
        }
        let exclude_path = common_dir.join("info").join("exclude");
        if !exclude_path.is_file() {
            return None;
        }
        let mut buf = Vec::new();
        // `from_git_dir` joins `info/exclude` onto the passed dir itself, so we
        // hand it the common dir directly. `excludes_file = None` because gix's
        // own status already applies `core.excludesFile` / the XDG global.
        let search = gix::ignore::Search::from_git_dir(common_dir, None, &mut buf).ok()?;
        if search.patterns.iter().all(|list| list.patterns.is_empty()) {
            return None;
        }
        // Case-sensitive matching mirrors git on the case-sensitive filesystems
        // maw targets (Linux CI). `info/exclude` is an additive safety net here,
        // so we do not depend on reading `core.ignoreCase`.
        let case = gix::glob::pattern::Case::Sensitive;
        Some(Self { search, case })
    }

    /// Whether `rela_path` (a repo-relative file path) is excluded by the
    /// common-dir `info/exclude`. Mirrors git's top-down directory pruning by
    /// testing every ancestor directory prefix (with `is_dir = true`) plus the
    /// leaf itself; the last matching pattern wins, and a negated pattern
    /// (`!foo`) re-includes.
    fn is_excluded(&self, rela_path: &str) -> bool {
        if rela_path.is_empty() {
            return false;
        }
        let parts: Vec<&str> = rela_path.split('/').collect();
        let n = parts.len();
        let mut excluded = false;
        let mut prefix = String::with_capacity(rela_path.len());
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                prefix.push('/');
            }
            prefix.push_str(part);
            let is_dir = i + 1 < n;
            if let Some(m) = self.search.pattern_matching_relative_path(
                prefix.as_bytes().as_bstr(),
                Some(is_dir),
                self.case,
            ) {
                excluded = !m.pattern.is_negative();
            }
        }
        excluded
    }
}

/// True when `item` is a gix-untracked leaf whose path the common-dir
/// `info/exclude` matcher excludes. Only untracked directory-walk leaves are
/// ever eligible — tracked/staged/modified entries are never suppressed.
fn item_is_common_excluded(
    common: Option<&CommonInfoExclude>,
    item: &gix::status::index_worktree::Item,
) -> bool {
    let Some(common) = common else {
        return false;
    };
    if let gix::status::index_worktree::Item::DirectoryContents { entry, .. } = item
        && matches!(entry.status, gix::dir::entry::Status::Untracked)
        && let Ok(path) = entry.rela_path.to_str()
    {
        return common.is_excluded(path);
    }
    false
}

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

    // bn-auu5: also honor the common-dir `info/exclude` on linked worktrees.
    let common = CommonInfoExclude::build(repo);

    let mut entries = Vec::new();
    for item in iter {
        let item = item.map_err(|e| GitError::BackendError {
            message: e.to_string(),
        })?;
        if item_is_common_excluded(common.as_ref(), &item) {
            continue;
        }
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

    // bn-auu5: also honor the common-dir `info/exclude` on linked worktrees.
    let common = CommonInfoExclude::build(repo);

    for item in iter {
        let item = item.map_err(|e| GitError::BackendError {
            message: e.to_string(),
        })?;
        match item {
            gix::status::Item::IndexWorktree(iw) => {
                if item_is_common_excluded(common.as_ref(), &iw) {
                    continue;
                }
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
    // A conflicted path occupies up to three index entries (stages 1/2/3,
    // with no stage-0 entry). `git status --porcelain` reports such a path
    // *once* (`UU path`), so we must count it once too — not once per stage
    // entry. Index entries are sorted by `(path, stage)`, so all stages of a
    // given conflicted path are contiguous; remembering the last counted
    // conflicted path dedupes them without allocating.
    let mut last_conflict_path: Option<Vec<u8>> = None;
    for entry in index.entries() {
        if entry.stage_raw() != 0 {
            let path_bytes = entry.path(&index);
            if last_conflict_path.as_deref() != Some(path_bytes) {
                dirty += 1;
                last_conflict_path = Some(path_bytes.to_vec());
            }
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

    // bn-auu5: also honor the common-dir `info/exclude` on linked worktrees.
    let common = CommonInfoExclude::build(repo);

    let mut paths = Vec::new();
    for item in iter {
        let item = item.map_err(|e| GitError::BackendError {
            message: format!("status item failed: {e}"),
        })?;
        if let gix::status::index_worktree::Item::DirectoryContents { entry, .. } = item
            && matches!(entry.status, gix::dir::entry::Status::Untracked)
            && let Ok(path) = entry.rela_path.to_str()
            && !common.as_ref().is_some_and(|c| c.is_excluded(path))
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
    use crate::repo::GitRepo as _;
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

    // -----------------------------------------------------------------------
    // bn-auu5: linked-worktree common-dir `info/exclude` compatibility.
    //
    // gix reads `$GIT_DIR/info/exclude`; for a linked worktree that is
    // `.git/worktrees/<name>/info/exclude`, so it MISSES the common-dir
    // `.git/info/exclude` that real git honors (`git_path("info/exclude")` →
    // common dir). This made `maw ws sync` refuse over genuinely-excluded
    // scratch (mess field report bn-1m4d item 5).
    // -----------------------------------------------------------------------

    /// A file matched by the MAIN `.git/info/exclude` must NOT be reported as
    /// untracked from a linked worktree — matching `git status`.
    #[test]
    fn common_dir_info_exclude_honored_on_linked_worktree() {
        let (dir, root, _oid) = init_test_repo_with_commit();
        let wt = root.join("linked-wt");
        let _ = crate::test_support::git_capture(
            &root,
            &[
                "worktree",
                "add",
                "--detach",
                wt.to_str().expect("path"),
                "HEAD",
            ],
        );

        // Pattern lives ONLY in the common-dir info/exclude (what git honors).
        std::fs::write(root.join(".git/info/exclude"), "scratch.tmp\n.test-tmp/\n")
            .expect("write common info/exclude");

        // Untracked scratch in the linked worktree.
        std::fs::write(wt.join("scratch.tmp"), "junk").expect("write scratch");
        std::fs::create_dir_all(wt.join(".test-tmp")).expect("mkdir");
        std::fs::write(wt.join(".test-tmp/corpus.bin"), "c").expect("write corpus");
        // A genuinely-untracked file NOT excluded, as a control.
        std::fs::write(wt.join("keep.txt"), "k").expect("write keep");

        let repo = crate::GixRepo::open(&wt).expect("open linked worktree");

        let untracked = repo.list_untracked().expect("list_untracked");
        assert!(
            untracked.iter().any(|p| p == "keep.txt"),
            "non-excluded untracked file must still be listed, got {untracked:?}",
        );
        assert!(
            !untracked.iter().any(|p| p == "scratch.tmp"),
            "common-dir info/exclude must suppress scratch.tmp, got {untracked:?}",
        );
        assert!(
            !untracked.iter().any(|p| p.starts_with(".test-tmp")),
            "common-dir dir-exclude must suppress .test-tmp/*, got {untracked:?}",
        );

        // status_head_to_worktree (the sync dirty-check source) must agree.
        let hw = status_head_to_worktree(&repo).expect("status_head_to_worktree");
        assert!(
            !hw.iter()
                .any(|e| e.path == "scratch.tmp" || e.path.starts_with(".test-tmp")),
            "sync dirty-check must not surface common-dir-excluded scratch, got {hw:?}",
        );
        assert!(
            hw.iter().any(|e| e.path == "keep.txt"),
            "non-excluded untracked file must still be dirty, got {hw:?}",
        );
        drop(dir);
    }

    /// Regression: on the MAIN worktree, `.git/info/exclude` must still be
    /// honored (gix already reads it there; our added path must not double- or
    /// mis-handle it).
    #[test]
    fn main_worktree_info_exclude_still_honored() {
        let (dir, root, _oid) = init_test_repo_with_commit();
        std::fs::write(root.join(".git/info/exclude"), "ignoreme.txt\n")
            .expect("write info/exclude");
        std::fs::write(root.join("ignoreme.txt"), "x").expect("write");
        std::fs::write(root.join("seeme.txt"), "y").expect("write");

        let repo = crate::GixRepo::open(&root).expect("open");
        let untracked = repo.list_untracked().expect("list_untracked");
        assert!(
            !untracked.iter().any(|p| p == "ignoreme.txt"),
            "got {untracked:?}"
        );
        assert!(
            untracked.iter().any(|p| p == "seeme.txt"),
            "got {untracked:?}"
        );
        drop(dir);
    }

    /// A tracked file that happens to match the common-dir exclude must NOT be
    /// suppressed — excludes only ever apply to untracked files (Prime
    /// Invariant: never hide tracked/staged work from a merge snapshot).
    #[test]
    fn common_dir_exclude_never_suppresses_tracked_changes() {
        let (dir, root, _oid) = init_test_repo_with_commit();
        // Commit a file, THEN exclude its name in the common-dir exclude.
        std::fs::write(root.join("data.log"), "v1\n").expect("write");
        let _ = crate::test_support::git_capture(&root, &["add", "data.log"]);
        let _ = crate::test_support::git_capture(&root, &["commit", "-qm", "add data.log"]);

        let wt = root.join("linked-wt2");
        let _ = crate::test_support::git_capture(
            &root,
            &[
                "worktree",
                "add",
                "--detach",
                wt.to_str().expect("path"),
                "HEAD",
            ],
        );
        std::fs::write(root.join(".git/info/exclude"), "*.log\n").expect("write exclude");

        // Modify the tracked file in the linked worktree.
        std::fs::write(wt.join("data.log"), "v2-modified\n").expect("modify");

        let repo = crate::GixRepo::open(&wt).expect("open");
        let hw = status_head_to_worktree(&repo).expect("status");
        assert!(
            hw.iter()
                .any(|e| e.path == "data.log" && e.status == FileStatus::Modified),
            "tracked modification must be reported despite matching common-dir exclude, got {hw:?}",
        );
        drop(dir);
    }

    /// A conflicted path occupies up to three index entries (stages 1/2/3,
    /// no stage-0). `git status --porcelain` reports it once (`UU path`), so
    /// `count_dirty_tracked` — which feeds the status-bar "N changed" count —
    /// must also count it once, not once per stage entry. Before the fix it
    /// reported a single conflicted file as 3 dirty files.
    #[cfg(unix)]
    #[test]
    fn count_dirty_tracked_counts_conflicted_path_once() {
        use std::process::Command;

        let (dir, root, _oid) = init_test_repo_with_commit();
        let repo = crate::GixRepo::open(&root).expect("open repo");

        let branch =
            crate::test_support::git_capture(&root, &["rev-parse", "--abbrev-ref", "HEAD"]);

        // Base commit introducing the soon-to-conflict file.
        std::fs::write(root.join("conflict.txt"), "base\n").expect("write base");
        let _ = crate::test_support::git_capture(&root, &["add", "conflict.txt"]);
        let _ = crate::test_support::git_capture(&root, &["commit", "-m", "add conflict.txt"]);

        // Divergent edit on a feature branch.
        let _ = crate::test_support::git_capture(&root, &["checkout", "-b", "feature"]);
        std::fs::write(root.join("conflict.txt"), "feature side\n").expect("write feature");
        let _ = crate::test_support::git_capture(&root, &["commit", "-am", "feature edit"]);

        // Conflicting edit back on the original branch.
        let _ = crate::test_support::git_capture(&root, &["checkout", &branch]);
        std::fs::write(root.join("conflict.txt"), "main side\n").expect("write main");
        let _ = crate::test_support::git_capture(&root, &["commit", "-am", "main edit"]);

        // Produce the conflict. `git merge` exits non-zero on conflict, so it
        // must not go through `git_capture` (which asserts success). The index
        // now holds stages 1/2/3 for `conflict.txt` and no stage-0 entry.
        let merge = Command::new("git")
            .args(["merge", "feature"])
            .current_dir(&root)
            .output()
            .expect("spawn git merge");
        assert!(
            !merge.status.success(),
            "expected `git merge feature` to conflict",
        );

        let n = count_dirty_tracked(&repo).expect("count_dirty_tracked");
        assert_eq!(
            n, 1,
            "a single conflicted path must count once (it occupies index \
             stages 1/2/3), not once per stage entry; got {n}",
        );
        drop(dir);
    }
}
