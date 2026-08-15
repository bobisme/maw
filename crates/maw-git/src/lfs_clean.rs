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
