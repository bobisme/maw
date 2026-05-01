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
    index_file
        .write(gix::index::write::Options::default())
        .map_err(|e| GitError::BackendError {
            message: format!("failed to write worktree index: {e}"),
        })?;

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
            let _ = persisted.write(gix::index::write::Options::default());
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
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;
    use crate::repo::GitRepo as _;

    fn setup_repo() -> (TempDir, GixRepo, GitOid) {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();

        Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(root)
            .output()
            .expect("git init");
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(root)
            .output()
            .expect("git config email");
        Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(root)
            .output()
            .expect("git config name");
        std::fs::write(root.join("README.md"), "hello\n").expect("write readme");
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(root)
            .output()
            .expect("git add");
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(root)
            .output()
            .expect("git commit");

        let repo = GixRepo::open(root).expect("open repo");
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
}
