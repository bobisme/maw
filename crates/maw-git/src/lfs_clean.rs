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
use std::rc::Rc;

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::GitOid;

/// What the cached [`maw_lfs::AttrsMatcher`] was derived from.
///
/// Two calls that compute the same key are guaranteed to produce the same
/// matcher, so the second may reuse the first's result. See
/// [`AttrsCache`] for the full invalidation contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrsKey {
    /// Built from `GixRepo::pending_gitattributes`. That field is only
    /// reachable through `&mut self` setters, both of which drop the
    /// cache, so a single `Pending` key can never span two different
    /// override sets.
    Pending,
    /// Built from the HEAD tree with this OID (`None` = no resolvable
    /// HEAD), falling back to the working directory when that tree
    /// carries no `.gitattributes`.
    Head(Option<gix::ObjectId>),
}

/// Memoized `.gitattributes` resolution for `write_blob_with_path`
/// (bn-2fps).
///
/// Building an [`maw_lfs::AttrsMatcher`] means a full recursive walk of
/// the HEAD tree, plus a full recursive walk of the working directory
/// when HEAD holds no `.gitattributes` at all. Every caller of
/// `write_blob_with_path` loops over files (merge `write_blob_at`,
/// `patch_candidate_tree`, sync/rebase, `worktree_state_commit`,
/// recover), so an N-file operation paid 2N repo-sized walks for a
/// result that is identical every time.
///
/// # Invalidation contract
///
/// The cache is keyed by [`AttrsKey`] and re-derived whenever the key
/// changes:
///
/// * **HEAD moves** — the key carries the HEAD *tree* OID, so a commit
///   that adds, edits, or removes a `.gitattributes` produces a new key
///   on the next call and the matcher is rebuilt. Content-addressing
///   makes this exact: same tree OID ⇒ byte-identical attributes.
/// * **Pending override set/cleared** — `set_pending_gitattributes` and
///   `clear_pending_gitattributes` take `&mut self` and clear the cache
///   outright.
/// * **Working-tree writes** — the workdir fallback reads `.gitattributes`
///   files off disk, which are *not* content-addressed by the key. Any
///   code that writes into the working tree while holding a `GixRepo`
///   must call [`GixRepo::invalidate_attrs_cache`]. `checkout_tree` does
///   this already; external writers (maw-cli materialization, etc.) that
///   reuse a long-lived `GixRepo` across a worktree rewrite should too.
///   Note this only matters when HEAD's tree has no `.gitattributes` and
///   HEAD does not move — in every other case the OID key already covers
///   it.
pub struct AttrsCache {
    key: AttrsKey,
    matcher: Rc<maw_lfs::AttrsMatcher>,
}

// Test-only counter of *actual* matcher builds (cache misses). Lets the
// unit tests below assert that N writes against one HEAD walk the tree
// once, not N times.
#[cfg(test)]
thread_local! {
    static ATTRS_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_attrs_build() {
    ATTRS_BUILDS.with(|c| c.set(c.get() + 1));
}

#[cfg(not(test))]
#[inline]
const fn note_attrs_build() {}

/// Test-only: number of matcher builds on this thread since the last reset.
#[cfg(test)]
pub fn attrs_builds() -> usize {
    ATTRS_BUILDS.with(std::cell::Cell::get)
}

/// Test-only: reset the build counter for this thread.
#[cfg(test)]
pub fn reset_attrs_builds() {
    ATTRS_BUILDS.with(|c| c.set(0));
}

/// Resolve the `.gitattributes` matcher for this repo, reusing the cached
/// one when the derivation inputs are unchanged.
///
/// Failures (unparsable attributes, unreadable workdir, bare repo with no
/// HEAD) resolve to an empty matcher — `is_lfs` then answers `false` for
/// every path and the caller writes a raw blob, exactly as before the
/// cache existed.
fn resolve_attrs(repo: &GixRepo) -> Rc<maw_lfs::AttrsMatcher> {
    let key = if repo.pending_gitattributes.is_some() {
        AttrsKey::Pending
    } else {
        AttrsKey::Head(head_tree_oid(repo))
    };

    if let Some(cached) = repo.attrs_cache.borrow().as_ref()
        && cached.key == key
    {
        return Rc::clone(&cached.matcher);
    }

    let matcher = Rc::new(build_attrs(repo, &key));
    *repo.attrs_cache.borrow_mut() = Some(AttrsCache {
        key,
        matcher: Rc::clone(&matcher),
    });
    matcher
}

/// The OID of HEAD's tree, or `None` when HEAD cannot be resolved
/// (fresh repo, unborn branch, corrupt ref).
fn head_tree_oid(repo: &GixRepo) -> Option<gix::ObjectId> {
    let commit = repo.repo.head_commit().ok()?;
    commit.tree_id().ok().map(gix::Id::detach)
}

/// Build the matcher for `key` from scratch. This is the walk the cache
/// exists to avoid.
fn build_attrs(repo: &GixRepo, key: &AttrsKey) -> maw_lfs::AttrsMatcher {
    note_attrs_build();
    match key {
        AttrsKey::Pending => {
            let entries = repo
                .pending_gitattributes
                .as_ref()
                .map_or_else(Vec::new, Clone::clone);
            match maw_lfs::AttrsMatcher::from_entries(entries) {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!("pending gitattributes parse failed: {e}");
                    maw_lfs::AttrsMatcher::empty()
                }
            }
        }
        AttrsKey::Head(tree_oid) => {
            // HEAD tree first (correct repo-relative paths; works for
            // bare repos), then the workdir fallback for repos whose
            // HEAD carries no attributes (or has no HEAD at all).
            let from_head = tree_oid.and_then(|oid| {
                let tree = repo.repo.find_tree(oid).ok()?;
                maw_lfs::AttrsMatcher::from_gix_tree(&repo.repo, &tree).ok()
            });
            match from_head {
                Some(a) if !a.is_empty() => a,
                _ => attrs_from_workdir(repo),
            }
        }
    }
}

fn attrs_from_workdir(repo: &GixRepo) -> maw_lfs::AttrsMatcher {
    let Some(workdir) = repo.repo.workdir() else {
        return maw_lfs::AttrsMatcher::empty();
    };
    match maw_lfs::AttrsMatcher::from_workdir(workdir) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("lfs attrs load failed: {e} — writing raw blob");
            maw_lfs::AttrsMatcher::empty()
        }
    }
}

pub fn write_blob_with_path(
    repo: &GixRepo,
    data: &[u8],
    rel_path: &str,
) -> Result<GitOid, GitError> {
    // Load .gitattributes for LFS pattern matching (memoized — see
    // `AttrsCache`).
    //
    // Priority:
    // 1. Pending attrs override (set by merge callers when the merge itself
    //    modifies .gitattributes — uses the INCOMING attrs, not HEAD's).
    // 2. HEAD tree (correct repo-relative paths; works for bare repos).
    // 3. Workdir fallback (fresh repo with no HEAD).
    let attrs = resolve_attrs(repo);
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
    //
    // Use the COMMON git dir, not the per-worktree one: LFS objects are
    // shared across every workspace's linked worktree at
    // `<common>/lfs/objects/`, but `GixRepo::git_dir()` returns the private
    // `<common>/worktrees/<name>/` admin dir for any non-default workspace.
    // Writing there instead of the common dir would silo each workspace's
    // clean-filtered objects into their own worktree-private store,
    // invisible to `git_dir()`-unaware readers (bn-1ero fixed the matching
    // smudge-side bug in `checkout_impl.rs`).
    let git_dir = repo.repo.common_dir();
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use crate::GixRepo;
    use crate::repo::GitRepo as _;

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

    /// Set up a repo whose HEAD commits a root `.gitattributes` marking
    /// `*.bin` as LFS. Returns the tempdir and an open `GixRepo`.
    fn repo_with_attrs(rules: &str) -> (tempfile::TempDir, GixRepo) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        git(&root, &["init", "-q", "--initial-branch=main"]);
        git(&root, &["config", "user.email", "t@t.com"]);
        git(&root, &["config", "user.name", "T"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join(".gitattributes"), rules).unwrap();
        git(&root, &["add", ".gitattributes"]);
        git(&root, &["commit", "-qm", "attrs"]);
        let repo = GixRepo::open(&root).unwrap();
        (dir, repo)
    }

    /// bn-2fps: the `.gitattributes` matcher is memoized per HEAD tree
    /// OID. N `write_blob_with_path` calls against one HEAD must walk the
    /// tree exactly once — not N times, which is what made an N-file
    /// snapshot quadratic-ish in repo size.
    #[test]
    fn attrs_matcher_is_built_once_per_head_tree() {
        let (dir, repo) = repo_with_attrs("*.bin filter=lfs diff=lfs merge=lfs -text\n");
        let root = dir.path().to_path_buf();

        super::reset_attrs_builds();
        for i in 0..25 {
            repo.write_blob_with_path(format!("payload {i}\n").as_bytes(), "notes.txt")
                .unwrap();
        }
        assert_eq!(
            super::attrs_builds(),
            1,
            "25 writes against one HEAD must walk the tree once"
        );

        // Moving HEAD (here: a commit that broadens the LFS rules) must
        // invalidate the cache — the key carries the HEAD tree OID.
        fs::write(
            root.join(".gitattributes"),
            "*.bin filter=lfs -text\n*.dat filter=lfs -text\n",
        )
        .unwrap();
        git(&root, &["add", ".gitattributes"]);
        git(&root, &["commit", "-qm", "widen attrs"]);

        let oid = repo
            .write_blob_with_path(b"raw dat content\n", "thing.dat")
            .unwrap();
        assert_eq!(
            super::attrs_builds(),
            2,
            "HEAD moved, so the matcher must be rebuilt exactly once more"
        );

        // ...and the rebuilt matcher must actually see the new rule.
        let blob = git(&root, &["cat-file", "-p", &oid.to_string()]);
        assert!(
            blob.starts_with("version https://git-lfs.github.com/spec/v1"),
            "new HEAD's `*.dat filter=lfs` rule must apply; got:\n{blob}"
        );

        // A second write against the (still unchanged) new HEAD reuses it.
        repo.write_blob_with_path(b"more dat\n", "other.dat")
            .unwrap();
        assert_eq!(super::attrs_builds(), 2, "same HEAD must reuse the matcher");
    }

    /// bn-2fps: the pending-`.gitattributes` override is its own cache
    /// key, and both setters drop the cache, so merge callers never see a
    /// stale matcher across a set/clear boundary.
    #[test]
    fn pending_gitattributes_override_invalidates_cache() {
        let (_dir, mut repo) = repo_with_attrs("*.bin filter=lfs -text\n");

        super::reset_attrs_builds();
        let head_oid = repo.write_blob_with_path(b"plain\n", "a.dat").unwrap();
        assert_eq!(super::attrs_builds(), 1);

        // Incoming merge attrs mark *.dat as LFS instead.
        repo.set_pending_gitattributes(vec![(String::new(), b"*.dat filter=lfs -text\n".to_vec())]);
        let pending_oid = repo.write_blob_with_path(b"plain\n", "a.dat").unwrap();
        assert_eq!(
            super::attrs_builds(),
            2,
            "setting a pending override must rebuild"
        );
        assert_ne!(
            head_oid, pending_oid,
            "pending attrs must turn a.dat into a pointer blob"
        );
        repo.write_blob_with_path(b"plain2\n", "b.dat").unwrap();
        assert_eq!(super::attrs_builds(), 2, "override is cached too");

        // Clearing it must fall back to HEAD's rules again.
        repo.clear_pending_gitattributes();
        let cleared_oid = repo.write_blob_with_path(b"plain\n", "a.dat").unwrap();
        assert_eq!(super::attrs_builds(), 3, "clearing must rebuild");
        assert_eq!(
            head_oid, cleared_oid,
            "after clearing, HEAD's rules apply again"
        );
    }

    /// bn-2fps: a repo with no `.gitattributes` anywhere still resolves
    /// (empty matcher, raw blobs) and still caches — this is the common
    /// case that previously paid a HEAD-tree walk *and* a full workdir
    /// walk per file.
    #[test]
    fn no_gitattributes_anywhere_is_cached_and_writes_raw_blobs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        git(&root, &["init", "-q", "--initial-branch=main"]);
        git(&root, &["config", "user.email", "t@t.com"]);
        git(&root, &["config", "user.name", "T"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("seed.txt"), "seed\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "seed"]);
        let repo = GixRepo::open(&root).unwrap();

        super::reset_attrs_builds();
        let a = repo.write_blob_with_path(b"hello\n", "x.bin").unwrap();
        for _ in 0..10 {
            repo.write_blob_with_path(b"hello\n", "x.bin").unwrap();
        }
        assert_eq!(super::attrs_builds(), 1);
        let plain = repo.write_blob(b"hello\n").unwrap();
        assert_eq!(a, plain, "no LFS rules -> raw blob");
    }

    /// Regression test (bn-1ero, write side): `write_blob_with_path`'s LFS
    /// store lookup must use the COMMON git dir, not the per-worktree one.
    /// Every maw workspace other than "default" is a linked worktree, so a
    /// commit built from within one (e.g. `worktree_state_commit`, or a
    /// merge writer) must land its real content in the SAME shared store
    /// that `checkout_tree`'s smudge pass and `maw push`'s LFS upload step
    /// read from — not a private, invisible-to-everyone-else store under
    /// `<common>/worktrees/<name>/lfs/objects/`.
    #[test]
    fn write_blob_with_path_stores_lfs_object_in_common_dir_from_linked_worktree() {
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
        let head = git(&root, &["rev-parse", "HEAD"]);

        // Linked worktree, detached at HEAD — the shape every non-default
        // maw workspace takes. Skip git-lfs's own smudge so this test
        // doesn't depend on whether the test machine has git-lfs installed.
        let wt = root.join("wt");
        Command::new("git")
            .args([
                "worktree",
                "add",
                "--detach",
                wt.to_str().expect("utf8 path"),
                &head,
            ])
            .current_dir(&root)
            .env("GIT_LFS_SKIP_SMUDGE", "1")
            .output()
            .expect("git worktree add");

        let wt_repo = GixRepo::open(&wt).expect("open worktree repo");
        assert_ne!(
            wt_repo.git_dir(),
            wt_repo.common_dir(),
            "test sanity: a linked worktree's git_dir must differ from common_dir"
        );

        let real_content = b"real content committed from a linked worktree (bn-1ero)\n".to_vec();
        let pointer_oid = wt_repo
            .write_blob_with_path(&real_content, "data.bin")
            .expect("write_blob_with_path");

        let pointer_bytes = wt_repo.read_blob(pointer_oid).expect("read pointer blob");
        assert!(
            maw_lfs::looks_like_pointer(&pointer_bytes),
            "write_blob_with_path should have stored a pointer blob, got: {:?}",
            String::from_utf8_lossy(&pointer_bytes)
        );
        let pointer = maw_lfs::Pointer::parse(&pointer_bytes).expect("parse pointer");

        // The real object must be retrievable from the COMMON store.
        let common_store =
            maw_lfs::Store::open(wt_repo.common_dir()).expect("open common lfs store");
        let mut reader = common_store
            .open_object(&pointer.oid)
            .expect("open_object")
            .expect("object should be present in the COMMON lfs store");
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut buf).expect("read object");
        assert_eq!(buf, real_content);

        // And it must NOT have been written into a private per-worktree
        // store — nothing should have ever created that directory.
        let per_worktree_lfs_dir = wt_repo.git_dir().join("lfs");
        assert!(
            !per_worktree_lfs_dir.exists(),
            "write_blob_with_path must not create a per-worktree lfs store at {}",
            per_worktree_lfs_dir.display()
        );
    }
}
