//! Worktree add/remove/list built from gix primitives.
//!
//! gix does not provide high-level worktree lifecycle APIs.
//! We build them from the documented git worktree format.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::*;

pub fn worktree_add(
    repo: &GixRepo,
    name: &str,
    target: GitOid,
    path: &Path,
) -> Result<(), GitError> {
    let git_dir = repo.repo.git_dir().to_path_buf();
    let admin_dir = git_dir.join("worktrees").join(name);

    // 1. Create admin directory
    std::fs::create_dir_all(&admin_dir).map_err(|e| GitError::BackendError {
        message: format!("failed to create worktree admin dir {}: {e}", admin_dir.display()),
    })?;

    // 2. Write HEAD with target OID (detached HEAD)
    std::fs::write(admin_dir.join("HEAD"), format!("{target}\n")).map_err(|e| {
        GitError::BackendError {
            message: format!("failed to write worktree HEAD: {e}"),
        }
    })?;

    // 3. Write commondir (relative path back to main .git)
    std::fs::write(admin_dir.join("commondir"), "../..\n").map_err(|e| {
        GitError::BackendError {
            message: format!("failed to write worktree commondir: {e}"),
        }
    })?;

    // 4. Write gitdir (absolute path to worktree's .git file)
    let wt_gitfile = path.join(".git");
    let abs_path = std::fs::canonicalize(path.parent().unwrap_or(path))
        .unwrap_or_else(|_| path.to_path_buf());
    let abs_gitfile = if path.is_absolute() {
        wt_gitfile.clone()
    } else {
        abs_path.join(path.file_name().unwrap_or_default()).join(".git")
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
    std::fs::write(
        &wt_gitfile,
        format!("gitdir: {}\n", admin_dir.display()),
    )
    .map_err(|e| GitError::BackendError {
        message: format!("failed to write worktree .git file: {e}"),
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
        .write(Default::default())
        .map_err(|e| GitError::BackendError {
            message: format!("failed to write worktree index: {e}"),
        })?;

    // 9. Checkout the tree to the worktree path
    let mut checkout_index = repo
        .repo
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
        let gitdir_path = std::path::PathBuf::from(gitdir_content.trim());
        // gitdir points to <worktree>/.git, so parent is the worktree root
        if let Some(wt_path) = gitdir_path.parent() {
            if wt_path.exists() {
                std::fs::remove_dir_all(wt_path).map_err(|e| GitError::BackendError {
                    message: format!(
                        "failed to remove worktree dir {}: {e}",
                        wt_path.display()
                    ),
                })?;
            }
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

pub fn worktree_list(repo: &GixRepo) -> Result<Vec<WorktreeInfo>, GitError> {
    let git_dir = repo.repo.git_dir().to_path_buf();
    let worktrees_dir = git_dir.join("worktrees");

    if !worktrees_dir.exists() {
        return Ok(Vec::new());
    }

    let entries =
        std::fs::read_dir(&worktrees_dir).map_err(|e| GitError::BackendError {
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

        let name = entry
            .file_name()
            .to_string_lossy()
            .into_owned();

        // Read HEAD to get current OID and detached state
        let (head_oid, is_detached) = {
            let head_file = entry_path.join("HEAD");
            if head_file.exists() {
                let content = std::fs::read_to_string(&head_file)
                    .ok()
                    .unwrap_or_default();
                let trimmed = content.trim();
                if let Some(ref_target) = trimmed.strip_prefix("ref: ") {
                    // Symbolic ref (e.g., "ref: refs/heads/main") — resolve via repo
                    let oid = crate::types::RefName::new(ref_target)
                        .ok()
                        .and_then(|rn| crate::refs_impl::read_ref(repo, &rn).ok().flatten());
                    (oid, false)
                } else if trimmed.len() == 40 {
                    // Direct hex OID (detached HEAD)
                    let mut bytes = [0u8; 20];
                    let mut valid = true;
                    for i in 0..20 {
                        match u8::from_str_radix(&trimmed[i * 2..i * 2 + 2], 16) {
                            Ok(b) => bytes[i] = b,
                            Err(_) => {
                                valid = false;
                                break;
                            }
                        }
                    }
                    if valid {
                        (Some(GitOid::from_bytes(bytes)), true)
                    } else {
                        (None, true)
                    }
                } else {
                    (None, true)
                }
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
                p.parent().map(|pp| pp.to_path_buf()).unwrap_or(p)
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
