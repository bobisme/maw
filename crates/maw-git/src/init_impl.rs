//! gix-backed repository initialization and HEAD manipulation helpers.
//!
//! These primitives back the greenfield/brownfield init paths in `maw-cli`.
//! They are kept in a dedicated module so the migration boundary between the
//! init flow and the rest of the trait surface is explicit.

use std::path::Path;

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::GitOid;

/// Initialize a brand-new (non-bare) git repository at `directory`.
///
/// Wraps [`gix::init`]. Fails if `directory/.git` already exists.
/// Intermediate parent directories are created as needed.
///
/// Replaces: `git init <directory>`.
///
/// # Errors
/// Returns a `GitError` if the directory cannot be created or gix cannot
/// initialize the repository (e.g., a `.git` already exists).
pub fn init_repo(directory: &Path) -> Result<GixRepo, GitError> {
    let repo = gix::init(directory).map_err(|e| GitError::BackendError {
        message: format!("git init {}: {e}", directory.display()),
    })?;
    let workdir = repo.workdir().map(std::path::Path::to_path_buf);
    Ok(GixRepo {
        repo,
        workdir,
        #[cfg(feature = "lfs")]
        pending_gitattributes: None,
    })
}

/// Read the short branch name currently referenced by HEAD.
///
/// Returns `Ok(Some(branch))` when HEAD is a symbolic ref pointing at
/// `refs/heads/<branch>`, `Ok(None)` when HEAD is detached (or there is no
/// HEAD yet), and an error only if reading the ref store itself fails.
///
/// Replaces: `git symbolic-ref --short HEAD` and the `--quiet` variant.
///
/// # Errors
/// Returns a `GitError::BackendError` if reading HEAD fails for a reason
/// other than the ref being detached / missing.
pub fn init_head_branch(repo: &GixRepo) -> Result<Option<String>, GitError> {
    match repo.repo.head_name() {
        Ok(Some(full)) => {
            let s = full.as_bstr().to_string();
            let short = s.strip_prefix("refs/heads/").map(str::to_owned);
            Ok(short)
        }
        Ok(None) => Ok(None), // detached
        Err(e) => Err(GitError::BackendError {
            message: format!("read HEAD: {e}"),
        }),
    }
}

/// Point HEAD at `refs/heads/<branch>` symbolically.
///
/// Writes `ref: refs/heads/<branch>\n` to `<git-dir>/HEAD` atomically (temp
/// file + rename). Does not validate that the branch ref already exists —
/// matching `git symbolic-ref` semantics, which allow setting HEAD to a
/// not-yet-created branch.
///
/// Replaces: `git symbolic-ref HEAD refs/heads/<branch>`.
///
/// # Errors
/// Returns a `GitError::BackendError` if `<git-dir>/HEAD` cannot be written.
pub fn init_set_head_to_branch(repo: &GixRepo, branch: &str) -> Result<(), GitError> {
    use std::io::Write as _;

    let git_dir = repo.repo.git_dir();
    let head_path = git_dir.join("HEAD");
    let tmp_path = git_dir.join("HEAD.maw-tmp");

    let contents = format!("ref: refs/heads/{branch}\n");

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

    Ok(())
}

/// Create an empty-tree commit and (optionally) update a branch ref to it.
///
/// Writes an empty tree, then a commit with that tree, no parents, and the
/// given message. When `branch` is provided, `refs/heads/<branch>` is updated
/// to the new commit OID. Useful for bootstrapping a brand-new repository
/// with an initial `--allow-empty` commit.
///
/// Replaces the greenfield-init sequence `git commit --allow-empty -m ...`.
///
/// # Errors
/// Returns a `GitError` if writing the tree, committing, or updating the
/// branch ref fails (for example, when the configured author/committer
/// identity is missing).
pub fn init_create_empty_commit(
    repo: &GixRepo,
    message: &str,
    branch: Option<&str>,
) -> Result<GitOid, GitError> {
    let tree_oid = crate::objects_impl::write_tree(repo, &[])?;
    let ref_name = match branch {
        Some(b) => Some(
            crate::types::RefName::new(&format!("refs/heads/{b}")).map_err(|e| {
                GitError::BackendError {
                    message: format!("invalid branch name '{b}': {e}"),
                }
            })?,
        ),
        None => None,
    };
    crate::objects_impl::create_commit(repo, tree_oid, &[], message, ref_name.as_ref())
}
