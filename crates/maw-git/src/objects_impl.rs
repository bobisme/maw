//! gix-backed object read/write and tree editing operations.

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::{CommitInfo, EntryMode, GitOid, RefName, TreeEdit, TreeEntry};

/// Convert our `GitOid` to a `gix::ObjectId`.
fn to_gix_oid(oid: GitOid) -> gix::ObjectId {
    gix::ObjectId::from_bytes_or_panic(oid.as_bytes())
}

/// Convert a `gix::ObjectId` to our `GitOid`.
fn from_gix_oid(oid: gix::ObjectId) -> GitOid {
    let bytes: [u8; 20] = oid.as_bytes().try_into().expect("SHA1 is 20 bytes");
    GitOid::from_bytes(bytes)
}

/// Convert a gix `EntryMode` to our `EntryMode`.
const fn from_gix_entry_mode(mode: gix::objs::tree::EntryMode) -> EntryMode {
    match mode.kind() {
        gix::objs::tree::EntryKind::Tree => EntryMode::Tree,
        gix::objs::tree::EntryKind::Blob => EntryMode::Blob,
        gix::objs::tree::EntryKind::BlobExecutable => EntryMode::BlobExecutable,
        gix::objs::tree::EntryKind::Link => EntryMode::Link,
        gix::objs::tree::EntryKind::Commit => EntryMode::Commit,
    }
}

/// Convert our `EntryMode` to a gix `EntryKind`.
const fn to_gix_entry_kind(mode: EntryMode) -> gix::objs::tree::EntryKind {
    match mode {
        EntryMode::Blob => gix::objs::tree::EntryKind::Blob,
        EntryMode::BlobExecutable => gix::objs::tree::EntryKind::BlobExecutable,
        EntryMode::Tree => gix::objs::tree::EntryKind::Tree,
        EntryMode::Link => gix::objs::tree::EntryKind::Link,
        EntryMode::Commit => gix::objs::tree::EntryKind::Commit,
    }
}

pub fn read_blob(repo: &GixRepo, oid: GitOid) -> Result<Vec<u8>, GitError> {
    let gix_oid = to_gix_oid(oid);
    let mut blob = repo
        .repo
        .find_blob(gix_oid)
        .map_err(|e| GitError::NotFound {
            message: format!("blob {oid}: {e}"),
        })?;
    Ok(blob.take_data())
}

pub fn read_tree(repo: &GixRepo, oid: GitOid) -> Result<Vec<TreeEntry>, GitError> {
    let gix_oid = to_gix_oid(oid);
    let tree = repo
        .repo
        .find_tree(gix_oid)
        .map_err(|e| GitError::NotFound {
            message: format!("tree {oid}: {e}"),
        })?;

    let mut entries = Vec::new();
    for result in tree.iter() {
        let entry = result.map_err(|e| GitError::BackendError {
            message: format!("failed to decode tree entry: {e}"),
        })?;
        let oid_bytes: [u8; 20] = entry
            .inner
            .oid
            .as_bytes()
            .try_into()
            .expect("SHA1 is 20 bytes");
        entries.push(TreeEntry {
            name: entry.inner.filename.to_string(),
            mode: from_gix_entry_mode(entry.inner.mode),
            oid: GitOid::from_bytes(oid_bytes),
        });
    }
    Ok(entries)
}

pub fn read_commit(repo: &GixRepo, oid: GitOid) -> Result<CommitInfo, GitError> {
    let gix_oid = to_gix_oid(oid);
    let commit = repo
        .repo
        .find_commit(gix_oid)
        .map_err(|e| GitError::NotFound {
            message: format!("commit {oid}: {e}"),
        })?;

    let decoded = commit.decode().map_err(|e| GitError::BackendError {
        message: format!("failed to decode commit {oid}: {e}"),
    })?;

    let tree_oid = from_gix_oid(decoded.tree());
    let parents = decoded.parents().map(from_gix_oid).collect();
    let message = decoded.message.to_string();

    let author_sig = decoded.author();
    let committer_sig = decoded.committer();

    let author = format!("{} <{}>", author_sig.name, author_sig.email);
    let committer = format!("{} <{}>", committer_sig.name, committer_sig.email);
    let committer_time = committer_sig.seconds();

    Ok(CommitInfo {
        tree_oid,
        parents,
        message,
        author,
        committer,
        committer_time,
    })
}

pub fn write_blob(repo: &GixRepo, data: &[u8]) -> Result<GitOid, GitError> {
    let id = repo
        .repo
        .write_blob(data)
        .map_err(|e| GitError::BackendError {
            message: format!("failed to write blob: {e}"),
        })?;
    Ok(from_gix_oid(id.detach()))
}

pub fn write_tree(repo: &GixRepo, entries: &[TreeEntry]) -> Result<GitOid, GitError> {
    let tree = gix::objs::Tree {
        entries: entries
            .iter()
            .map(|e| gix::objs::tree::Entry {
                mode: to_gix_entry_kind(e.mode).into(),
                filename: e.name.as_str().into(),
                oid: to_gix_oid(e.oid),
            })
            .collect(),
    };
    let id = repo
        .repo
        .write_object(&tree)
        .map_err(|e| GitError::BackendError {
            message: format!("failed to write tree: {e}"),
        })?;
    Ok(from_gix_oid(id.detach()))
}

pub fn create_commit(
    repo: &GixRepo,
    tree: GitOid,
    parents: &[GitOid],
    message: &str,
    update_ref: Option<&RefName>,
) -> Result<GitOid, GitError> {
    let tree_oid = to_gix_oid(tree);
    let parent_oids: Vec<gix::ObjectId> = parents.iter().map(|p| to_gix_oid(*p)).collect();

    let author_sig = repo
        .repo
        .author()
        .ok_or_else(|| GitError::BackendError {
            message: "no author identity configured".to_string(),
        })?
        .map_err(|e| GitError::BackendError {
            message: format!("failed to read author identity: {e}"),
        })?;

    let committer_sig = repo
        .repo
        .committer()
        .ok_or_else(|| GitError::BackendError {
            message: "no committer identity configured".to_string(),
        })?
        .map_err(|e| GitError::BackendError {
            message: format!("failed to read committer identity: {e}"),
        })?;

    if let Some(ref_name) = update_ref {
        let id = repo
            .repo
            .commit_as(
                committer_sig,
                author_sig,
                ref_name.as_str(),
                message,
                tree_oid,
                parent_oids,
            )
            .map_err(|e| GitError::BackendError {
                message: format!("failed to create commit: {e}"),
            })?;
        Ok(from_gix_oid(id.detach()))
    } else {
        let commit = gix::objs::Commit {
            message: message.into(),
            tree: tree_oid,
            author: author_sig.into(),
            committer: committer_sig.into(),
            encoding: None,
            parents: parent_oids.into_iter().collect(),
            extra_headers: Vec::default(),
        };
        let id = repo
            .repo
            .write_object(&commit)
            .map_err(|e| GitError::BackendError {
                message: format!("failed to write commit object: {e}"),
            })?;
        Ok(from_gix_oid(id.detach()))
    }
}

/// Information about a single blob discovered while walking a tree.
///
/// Returned by [`walk_tree_blob_paths`] and [`walk_tree_blobs`]. Paths are
/// slash-separated and relative to the root tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobEntry {
    /// Slash-separated path from the root tree.
    pub path: String,
    /// File mode of the blob (`Blob`, `BlobExecutable`, or `Link`).
    pub mode: EntryMode,
    /// OID of the blob object.
    pub oid: GitOid,
}

/// Resolve a slash-separated `path` inside the tree at `tree_or_commit_oid`.
///
/// If the OID names a commit, its tree is resolved first. Returns `None` if
/// the path is missing.
///
/// Replaces: `git ls-tree -z <tree-or-commit> -- <path>`.
pub fn find_entry_at_path(
    repo: &GixRepo,
    tree_or_commit_oid: GitOid,
    path: &str,
) -> Result<Option<(EntryMode, GitOid)>, GitError> {
    let tree_oid = resolve_to_tree_oid(repo, tree_or_commit_oid)?;
    let mut current_tree =
        repo.repo
            .find_tree(to_gix_oid(tree_oid))
            .map_err(|e| GitError::NotFound {
                message: format!("tree {tree_oid}: {e}"),
            })?;

    let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if components.is_empty() {
        return Ok(None);
    }

    let last_idx = components.len() - 1;
    for (i, component) in components.iter().enumerate() {
        let entries: Vec<_> =
            current_tree
                .iter()
                .collect::<Result<_, _>>()
                .map_err(|e| GitError::BackendError {
                    message: format!("failed to decode tree entries: {e}"),
                })?;
        let needle = component.as_bytes();
        let Some(entry) = entries
            .into_iter()
            .find(|e| AsRef::<[u8]>::as_ref(e.inner.filename) == needle)
        else {
            return Ok(None);
        };

        let mode = from_gix_entry_mode(entry.inner.mode);
        let oid_bytes: [u8; 20] = entry
            .inner
            .oid
            .as_bytes()
            .try_into()
            .expect("SHA1 is 20 bytes");
        let oid = GitOid::from_bytes(oid_bytes);

        if i == last_idx {
            return Ok(Some((mode, oid)));
        }

        // Intermediate component must be a tree.
        if !matches!(mode, EntryMode::Tree) {
            return Ok(None);
        }
        current_tree = repo
            .repo
            .find_tree(to_gix_oid(oid))
            .map_err(|e| GitError::NotFound {
                message: format!("subtree {oid} at component '{component}': {e}"),
            })?;
    }

    Ok(None)
}

/// Read a blob located at `path` inside the tree at `tree_or_commit_oid`.
///
/// Returns the raw blob bytes plus its mode and OID, or `None` if the path
/// is missing or names a non-blob entry (subtree or submodule).
///
/// Replaces: `git show <oid>:<path>` and `git cat-file blob <oid>` combined.
pub fn read_blob_at_path(
    repo: &GixRepo,
    tree_or_commit_oid: GitOid,
    path: &str,
) -> Result<Option<(EntryMode, GitOid, Vec<u8>)>, GitError> {
    let Some((mode, oid)) = find_entry_at_path(repo, tree_or_commit_oid, path)? else {
        return Ok(None);
    };
    if !matches!(
        mode,
        EntryMode::Blob | EntryMode::BlobExecutable | EntryMode::Link
    ) {
        return Ok(None);
    }
    let data = read_blob(repo, oid)?;
    Ok(Some((mode, oid, data)))
}

/// Recursively walk every blob/symlink path reachable from `tree_or_commit_oid`.
///
/// Yields metadata only (no blob content). Paths are returned in tree-walk
/// order. Submodules (gitlinks) are skipped.
///
/// Replaces: `git ls-tree -r --name-only -z <oid>`.
pub fn walk_tree_blob_paths(
    repo: &GixRepo,
    tree_or_commit_oid: GitOid,
) -> Result<Vec<BlobEntry>, GitError> {
    let tree_oid = resolve_to_tree_oid(repo, tree_or_commit_oid)?;
    let mut out = Vec::new();
    let tree = repo
        .repo
        .find_tree(to_gix_oid(tree_oid))
        .map_err(|e| GitError::NotFound {
            message: format!("tree {tree_oid}: {e}"),
        })?;
    walk_blobs_recursive(repo, &tree, "", &mut out)?;
    Ok(out)
}

/// Recursively walk every blob reachable from `tree_or_commit_oid`, invoking
/// `visit` with each blob's path, OID, and raw content.
///
/// If `visit` returns `Err`, traversal stops and the error propagates.
/// Symlinks are included; submodules (gitlinks) are skipped.
///
/// Useful for content-search workloads (replaces `git grep -z -n <oid>`).
pub fn walk_tree_blobs<F>(
    repo: &GixRepo,
    tree_or_commit_oid: GitOid,
    mut visit: F,
) -> Result<(), GitError>
where
    F: FnMut(&BlobEntry, &[u8]) -> Result<(), GitError>,
{
    let entries = walk_tree_blob_paths(repo, tree_or_commit_oid)?;
    for entry in &entries {
        let data = read_blob(repo, entry.oid)?;
        visit(entry, &data)?;
    }
    Ok(())
}

fn resolve_to_tree_oid(repo: &GixRepo, oid: GitOid) -> Result<GitOid, GitError> {
    let gix_oid = to_gix_oid(oid);
    let obj = repo
        .repo
        .find_object(gix_oid)
        .map_err(|e| GitError::NotFound {
            message: format!("object {oid}: {e}"),
        })?;
    match obj.kind {
        gix::object::Kind::Tree => Ok(oid),
        gix::object::Kind::Commit => {
            let commit = obj.into_commit();
            let tree_id = commit.tree_id().map_err(|e| GitError::BackendError {
                message: format!("commit {oid} has no tree: {e}"),
            })?;
            Ok(from_gix_oid(tree_id.detach()))
        }
        other => Err(GitError::BackendError {
            message: format!("expected commit or tree at {oid}, got {other}"),
        }),
    }
}

fn walk_blobs_recursive(
    repo: &GixRepo,
    tree: &gix::Tree<'_>,
    prefix: &str,
    out: &mut Vec<BlobEntry>,
) -> Result<(), GitError> {
    for entry_result in tree.iter() {
        let entry = entry_result.map_err(|e| GitError::BackendError {
            message: format!("failed to decode tree entry: {e}"),
        })?;
        let name =
            String::from_utf8_lossy(AsRef::<[u8]>::as_ref(entry.inner.filename)).into_owned();
        let rel_path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let mode = from_gix_entry_mode(entry.inner.mode);
        let oid_bytes: [u8; 20] = entry
            .inner
            .oid
            .as_bytes()
            .try_into()
            .expect("SHA1 is 20 bytes");
        let oid = GitOid::from_bytes(oid_bytes);

        match mode {
            EntryMode::Tree => {
                let subtree =
                    repo.repo
                        .find_tree(to_gix_oid(oid))
                        .map_err(|e| GitError::NotFound {
                            message: format!("subtree {oid} at '{rel_path}': {e}"),
                        })?;
                walk_blobs_recursive(repo, &subtree, &rel_path, out)?;
            }
            EntryMode::Blob | EntryMode::BlobExecutable | EntryMode::Link => {
                out.push(BlobEntry {
                    path: rel_path,
                    mode,
                    oid,
                });
            }
            EntryMode::Commit => {
                // Submodule / gitlink — skip (matches `git ls-tree -r --name-only`
                // behavior: gitlinks are listed but they have no content; for our
                // recovery use-cases we only care about file content).
            }
        }
    }
    Ok(())
}

pub fn edit_tree(repo: &GixRepo, base: GitOid, edits: &[TreeEdit]) -> Result<GitOid, GitError> {
    let gix_oid = to_gix_oid(base);
    let tree = repo
        .repo
        .find_tree(gix_oid)
        .map_err(|e| GitError::NotFound {
            message: format!("base tree {base}: {e}"),
        })?;

    let mut editor = tree.edit().map_err(|e| GitError::BackendError {
        message: format!("failed to create tree editor: {e}"),
    })?;

    for edit in edits {
        match edit {
            TreeEdit::Upsert { path, mode, oid } => {
                let kind = to_gix_entry_kind(*mode);
                let gix_oid = to_gix_oid(*oid);
                editor.upsert(path.as_str(), kind, gix_oid).map_err(|e| {
                    GitError::BackendError {
                        message: format!("tree edit upsert '{path}': {e}"),
                    }
                })?;
            }
            TreeEdit::Remove { path } => {
                editor
                    .remove(path.as_str())
                    .map_err(|e| GitError::BackendError {
                        message: format!("tree edit remove '{path}': {e}"),
                    })?;
            }
        }
    }

    let new_id = editor.write().map_err(|e| GitError::BackendError {
        message: format!("failed to write edited tree: {e}"),
    })?;
    Ok(from_gix_oid(new_id.detach()))
}

/// Read a file's blob content at a path within a commit's tree.
///
/// Resolves `commit_spec` via gix `rev_parse_single` (so it accepts hex
/// OIDs, ref names, or any other rev spec gix supports), descends into the
/// commit's tree to the entry at `rel_path`, and returns the blob bytes.
///
/// Returns `Ok(None)` if the path does not exist in the commit's tree, the
/// commit cannot be resolved, or the entry at the path is not a blob/link.
/// This mirrors the previous `git show <commit>:<path>` behavior of "missing
/// → None" used by the stash-replay helpers.
///
/// Symlinks are returned as their target text (matching `git show` semantics).
///
/// Replaces: `git show <commit>:<path>`.
pub fn read_file_at_commit(
    repo: &GixRepo,
    commit_spec: &str,
    rel_path: &std::path::Path,
) -> Result<Option<Vec<u8>>, GitError> {
    // Resolve the commit-ish spec to an object id. We don't return errors
    // here — the upstream behavior for missing/invalid commits is `None`.
    let Ok(obj_id) = repo.repo.rev_parse_single(commit_spec) else {
        return Ok(None);
    };

    let Ok(commit) = repo.repo.find_commit(obj_id) else {
        return Ok(None);
    };

    let Ok(tree) = commit.tree() else {
        return Ok(None);
    };

    // Use lookup_entry_by_path so nested paths (a/b/c.txt) resolve via
    // intermediate trees — same semantics as `git show <commit>:<path>`.
    let Ok(Some(entry)) = tree.lookup_entry_by_path(rel_path) else {
        return Ok(None);
    };

    // Only return content for blob-shaped entries (regular files, executables,
    // symlinks). Trees and commit (submodule) entries return None.
    match entry.mode().kind() {
        gix::objs::tree::EntryKind::Blob
        | gix::objs::tree::EntryKind::BlobExecutable
        | gix::objs::tree::EntryKind::Link => {}
        _ => return Ok(None),
    }

    let blob = repo
        .repo
        .find_blob(entry.oid())
        .map_err(|e| GitError::BackendError {
            message: format!("failed to read blob at '{}': {e}", rel_path.display()),
        })?;
    Ok(Some(blob.data.clone()))
}
