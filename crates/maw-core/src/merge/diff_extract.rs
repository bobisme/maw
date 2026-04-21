//! Extract a [`PatchSet`] from a commit-range diff (Phase 3 of bn-gjm8).
//!
//! During rebase, we need to replay a workspace's historical commits on top
//! of a new ancestor. For each commit in the range we compute a `PatchSet`
//! relative to its parent, then feed that patch into the structured-merge
//! engine via [`apply_unilateral_patchset`](super::apply::apply_unilateral_patchset).
//!
//! [`diff_patchset`] is the helper that turns a `(from_commit, to_commit)`
//! pair into a [`PatchSet`], resolving tree OIDs, calling
//! [`maw_git::GitRepo::diff_trees_with_renames`] for rename-aware diffing,
//! and loading blob contents via [`maw_git::GitRepo::read_blob`].
//!
//! Renames are emitted as a **pair** of [`FileChange`]s — `Deleted(from)` and
//! `Modified(to)` — both tagged with the same `FileId` (derived from the old
//! blob) so downstream overlap detection can recognize the identity across the
//! move. This is required for rename-vs-modify-at-source scenarios (bn-3525):
//! if only a single `Modified(to)` were emitted, the epoch's independent
//! modification of the renamed-from path would race against the workspace's
//! rename and BOTH paths would be silently dropped from the final tree (the
//! workspace's `Modified(to)` targets a path not in the seeded new-epoch tree,
//! which `apply_unilateral_patchset` previously ignored with a warning; and
//! there was no `Deleted(from)` to remove the stale `from` entry either).
//!
//! Emitting `Deleted(from) + Modified(to)` lets the apply step remove the old
//! path and upsert the new one, and lets `promote_overlaps_to_conflicts` see
//! both sides of the rename so it can follow the rename when the epoch
//! modified `from`.

use std::path::PathBuf;

use maw_git::{ChangeType, DiffEntry, EntryMode as GitEntryMode, GitError, GitRepo};

use super::types::{ChangeKind, EntryMode, FileChange, PatchSet};
use crate::model::diff::file_id_from_blob;
use crate::model::types::{EpochId, GitOid, WorkspaceId};

/// Returns `true` if the given mode is a submodule (gitlink, mode 160000).
///
/// Submodule entries reference commits in another repository — the OID they
/// carry is **not** a blob in the current repo's object store, so blob reads
/// against it will always fail. Treat them as opaque throughout the merge
/// pipeline: preserve the OID as identity, but never dereference it (bn-3hqg).
fn is_submodule(mode: Option<GitEntryMode>) -> bool {
    matches!(mode, Some(GitEntryMode::Commit))
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors returned by [`diff_patchset`].
#[derive(Debug)]
pub enum DiffExtractError {
    /// A [`GitRepo`] call failed.
    RepoError(GitError),
    /// A commit OID could not be resolved to a commit object.
    InvalidOid {
        /// The caller-visible spec that failed (hex form of the OID).
        spec: String,
    },
    /// A git OID string produced by the backend was malformed in a way
    /// `maw_core::GitOid` can't validate (should not happen in practice —
    /// treated as a defensive fallthrough).
    MalformedOid {
        /// The raw string that failed.
        raw: String,
    },
}

impl std::fmt::Display for DiffExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RepoError(e) => write!(f, "git repo error: {e}"),
            Self::InvalidOid { spec } => {
                write!(f, "could not resolve OID {spec:?} to a commit")
            }
            Self::MalformedOid { raw } => write!(f, "malformed git OID {raw:?}"),
        }
    }
}

impl std::error::Error for DiffExtractError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::RepoError(e) => Some(e),
            _ => None,
        }
    }
}

impl From<GitError> for DiffExtractError {
    fn from(e: GitError) -> Self {
        Self::RepoError(e)
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Compute a [`PatchSet`] describing the changes introduced by going from
/// commit `from_oid` to commit `to_oid`.
///
/// `from_oid` may be [`GitOid::ZERO`](maw_git::GitOid::ZERO) to represent a
/// root commit — the diff then runs against the empty tree (every file
/// appears as `Added`). Otherwise `from_oid` is resolved to its tree and
/// `to_oid` likewise, and [`maw_git::GitRepo::diff_trees_with_renames`] is
/// invoked with the caller-supplied `similarity_pct` (50 matches git's
/// default `git diff -M` threshold).
///
/// Each [`DiffEntry`] becomes one or two [`FileChange`]s:
///
/// * `Added`    → `ChangeKind::Added` with `content = Some(blob bytes)` and
///   `blob = Some(new_oid)`. `file_id` is derived from the new blob via
///   [`file_id_from_blob`].
/// * `Modified` → `ChangeKind::Modified` with `content = Some(blob bytes)`
///   and `blob = Some(new_oid)`. `file_id` is derived from the **old** blob
///   so identity is stable across the modification.
/// * `Deleted`  → `ChangeKind::Deleted` with `content = None`, `blob = None`,
///   and `mode = old_mode`. `file_id` is derived from the old blob.
/// * `Renamed`  → a **pair**: `ChangeKind::Deleted` at the old path plus
///   `ChangeKind::Modified` at the new path. Both carry the same `file_id`
///   (derived from the old blob) so identity is preserved across the move.
///   Emitting the explicit delete is required so downstream apply steps
///   (and overlap detection for the rebase pipeline) can see the old path
///   going away — without it, a concurrent epoch modification of the
///   renamed-from path would survive as a ghost entry in the final tree
///   (bn-3525).
///
/// # Errors
///
/// - [`DiffExtractError::RepoError`] — any underlying [`GitRepo`] failure.
/// - [`DiffExtractError::InvalidOid`] — `from_oid` / `to_oid` couldn't be
///   resolved to a commit object.
pub fn diff_patchset(
    repo: &dyn GitRepo,
    from_oid: &GitOid,
    to_oid: &GitOid,
    workspace_id: &WorkspaceId,
    epoch: &EpochId,
    similarity_pct: u32,
) -> Result<PatchSet, DiffExtractError> {
    // Resolve the to-side tree.
    let to_git = core_to_git_oid(to_oid)?;
    let to_commit = repo.read_commit(to_git).map_err(|e| match e {
        GitError::NotFound { .. } => DiffExtractError::InvalidOid {
            spec: to_oid.as_str().to_owned(),
        },
        other => DiffExtractError::RepoError(other),
    })?;
    let to_tree = to_commit.tree_oid;

    // Resolve the from-side tree. A zero OID ⇒ diff against the empty tree
    // (root-commit case).
    let from_tree_opt = if from_oid.as_str() == "0".repeat(40) {
        None
    } else {
        let from_git = core_to_git_oid(from_oid)?;
        let from_commit = repo.read_commit(from_git).map_err(|e| match e {
            GitError::NotFound { .. } => DiffExtractError::InvalidOid {
                spec: from_oid.as_str().to_owned(),
            },
            other => DiffExtractError::RepoError(other),
        })?;
        Some(from_commit.tree_oid)
    };

    let entries = repo.diff_trees_with_renames(from_tree_opt, to_tree, similarity_pct)?;

    let mut changes: Vec<FileChange> = Vec::with_capacity(entries.len());
    for entry in &entries {
        let produced = file_changes_from_entry(repo, entry)?;
        changes.extend(produced);
    }

    Ok(PatchSet::new(workspace_id.clone(), epoch.clone(), changes))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Convert a `maw-core` `GitOid` (hex-string) to a `maw-git` `GitOid` (bytes).
fn core_to_git_oid(oid: &GitOid) -> Result<maw_git::GitOid, DiffExtractError> {
    oid.as_str()
        .parse::<maw_git::GitOid>()
        .map_err(|_| DiffExtractError::MalformedOid {
            raw: oid.as_str().to_owned(),
        })
}

/// Convert a `maw-git` `GitOid` back to the `maw-core` `GitOid`.
fn git_to_core_oid(oid: &maw_git::GitOid) -> Result<GitOid, DiffExtractError> {
    let s = oid.to_string();
    GitOid::new(&s).map_err(|_| DiffExtractError::MalformedOid { raw: s })
}

/// Translate one [`DiffEntry`] into one or two [`FileChange`]s.
///
/// All change kinds produce exactly one `FileChange` except `Renamed`, which
/// produces **two**: a `Deleted` at the old path plus a `Modified` at the new
/// path. See the module docs on [`diff_patchset`] for the `Renamed` rationale
/// (bn-3525).
fn file_changes_from_entry(
    repo: &dyn GitRepo,
    entry: &DiffEntry,
) -> Result<Vec<FileChange>, DiffExtractError> {
    let path = PathBuf::from(&entry.path);

    match &entry.change_type {
        ChangeType::Added => {
            let blob_core = git_to_core_oid(&entry.new_oid)?;
            let file_id = file_id_from_blob(&blob_core);
            let mode = entry.new_mode.map(EntryMode::from);
            // Submodule (gitlink) entries carry a commit OID pointing into a
            // DIFFERENT repository. That OID is not a blob in this repo's
            // object store, so we must NOT call `read_blob`. Treat the entry
            // as opaque: preserve the gitlink SHA as identity (so it flows
            // through to the final tree unchanged) and leave `content` empty
            // (bn-3hqg).
            let content = if is_submodule(entry.new_mode) {
                None
            } else {
                Some(repo.read_blob(entry.new_oid)?)
            };
            Ok(vec![FileChange::with_mode(
                path,
                ChangeKind::Added,
                content,
                Some(file_id),
                Some(blob_core),
                mode,
            )])
        }
        ChangeType::Modified => {
            let old_core = git_to_core_oid(&entry.old_oid)?;
            let blob_core = git_to_core_oid(&entry.new_oid)?;
            let file_id = file_id_from_blob(&old_core);
            let mode = entry.new_mode.map(EntryMode::from);
            // Submodule (gitlink) on either side: don't dereference as a blob.
            // See `Added` branch for the full rationale (bn-3hqg).
            let content = if is_submodule(entry.new_mode) || is_submodule(entry.old_mode) {
                None
            } else {
                Some(repo.read_blob(entry.new_oid)?)
            };
            Ok(vec![FileChange::with_mode(
                path,
                ChangeKind::Modified,
                content,
                Some(file_id),
                Some(blob_core),
                mode,
            )])
        }
        ChangeType::Deleted => {
            let old_core = git_to_core_oid(&entry.old_oid)?;
            let file_id = file_id_from_blob(&old_core);
            // For deletions we surface the old-side mode so downstream code
            // can tell whether the removed path was e.g. a symlink.
            let mode = entry.old_mode.map(EntryMode::from);
            Ok(vec![FileChange::with_mode(
                path,
                ChangeKind::Deleted,
                None,
                Some(file_id),
                None,
                mode,
            )])
        }
        ChangeType::Renamed { from } => {
            // A rename is TWO logical operations downstream even though it's
            // one DiffEntry: the old path must be removed from the tree, and
            // the new path must be added (with the new content). We emit a
            // `Deleted(from)` + `Modified(to)` pair, both tagged with the
            // same FileId (derived from the old blob) so identity is
            // preserved across the move. This is required so the apply step
            // (and the rebase-level overlap detector) see both sides of the
            // move (bn-3525).
            let old_core = git_to_core_oid(&entry.old_oid)?;
            let blob_core = git_to_core_oid(&entry.new_oid)?;
            let file_id = file_id_from_blob(&old_core);
            let new_mode = entry.new_mode.map(EntryMode::from);
            let old_mode = entry.old_mode.map(EntryMode::from);
            // Skip blob read when either side is a submodule (bn-3hqg). A
            // submodule rename is a path change of a gitlink, not a textual
            // file move — treat the content as opaque.
            let content = if is_submodule(entry.new_mode) || is_submodule(entry.old_mode) {
                None
            } else {
                Some(repo.read_blob(entry.new_oid)?)
            };

            let from_path = PathBuf::from(from);

            let delete = FileChange::with_mode(
                from_path,
                ChangeKind::Deleted,
                None,
                Some(file_id),
                None,
                old_mode,
            );
            let modify = FileChange::with_mode(
                path,
                ChangeKind::Modified,
                content,
                Some(file_id),
                Some(blob_core),
                new_mode,
            );
            Ok(vec![delete, modify])
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;
    use crate::model::types::{EpochId, WorkspaceId};

    // ----- Fixture helpers ---------------------------------------------------

    struct Fixture {
        _dir: TempDir,
        root: std::path::PathBuf,
        repo: Box<dyn GitRepo>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            let root = dir.path().to_path_buf();
            run_git(&root, &["init", "--initial-branch=main"]);
            run_git(&root, &["config", "user.name", "Test"]);
            run_git(&root, &["config", "user.email", "test@test.com"]);
            run_git(&root, &["config", "commit.gpgsign", "false"]);
            let repo: Box<dyn GitRepo> = Box::new(maw_git::GixRepo::open(&root).unwrap());
            Self {
                _dir: dir,
                root,
                repo,
            }
        }

        fn commit(&self, msg: &str) -> GitOid {
            run_git(&self.root, &["add", "-A"]);
            run_git(&self.root, &["commit", "-m", msg, "--allow-empty"]);
            self.head()
        }

        fn head(&self) -> GitOid {
            git_rev_parse(&self.root, "HEAD")
        }

        fn write(&self, rel: &str, content: &[u8]) {
            let path = self.root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, content).unwrap();
        }

        fn remove(&self, rel: &str) {
            fs::remove_file(self.root.join(rel)).unwrap();
        }

        fn rename(&self, from: &str, to: &str) {
            let from_p = self.root.join(from);
            let to_p = self.root.join(to);
            if let Some(parent) = to_p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::rename(from_p, to_p).unwrap();
        }

        fn chmod_exec(&self, rel: &str) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let path = self.root.join(rel);
                let mut perms = fs::metadata(&path).unwrap().permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&path, perms).unwrap();
            }
            #[cfg(not(unix))]
            {
                let _ = rel;
                panic!("chmod_exec only supported on unix");
            }
        }

        fn symlink(&self, rel: &str, target: &str) {
            #[cfg(unix)]
            {
                let path = self.root.join(rel);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).unwrap();
                }
                std::os::unix::fs::symlink(target, &path).unwrap();
            }
            #[cfg(not(unix))]
            {
                let _ = (rel, target);
                panic!("symlink only supported on unix");
            }
        }
    }

    fn run_git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_rev_parse(root: &Path, spec: &str) -> GitOid {
        let out = Command::new("git")
            .args(["rev-parse", spec])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git rev-parse {spec} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        GitOid::new(String::from_utf8_lossy(&out.stdout).trim()).unwrap()
    }

    fn ws() -> WorkspaceId {
        WorkspaceId::new("test-ws").unwrap()
    }

    fn epoch_from(oid: &GitOid) -> EpochId {
        EpochId::new(oid.as_str()).unwrap()
    }

    fn zero_oid() -> GitOid {
        GitOid::new(&"0".repeat(40)).unwrap()
    }

    // ----- Tests -------------------------------------------------------------

    #[test]
    fn diff_patchset_add_only() {
        let fx = Fixture::new();
        fx.write("keep.txt", b"seed\n");
        let from = fx.commit("seed");
        fx.write("new.txt", b"hello\n");
        let to = fx.commit("add new.txt");

        let ps = diff_patchset(&*fx.repo, &from, &to, &ws(), &epoch_from(&from), 50).unwrap();
        assert_eq!(ps.change_count(), 1);
        let fc = &ps.changes[0];
        assert_eq!(fc.path, PathBuf::from("new.txt"));
        assert_eq!(fc.kind, ChangeKind::Added);
        assert_eq!(fc.content.as_deref(), Some(&b"hello\n"[..]));
        assert!(fc.blob.is_some());
        assert!(fc.file_id.is_some());
        assert_eq!(fc.mode, Some(EntryMode::Blob));
    }

    #[test]
    fn diff_patchset_modify_only() {
        let fx = Fixture::new();
        fx.write("a.txt", b"v1\n");
        let from = fx.commit("v1");
        fx.write("a.txt", b"v2\n");
        let to = fx.commit("v2");

        let ps = diff_patchset(&*fx.repo, &from, &to, &ws(), &epoch_from(&from), 50).unwrap();
        assert_eq!(ps.change_count(), 1);
        let fc = &ps.changes[0];
        assert_eq!(fc.path, PathBuf::from("a.txt"));
        assert_eq!(fc.kind, ChangeKind::Modified);
        assert_eq!(fc.content.as_deref(), Some(&b"v2\n"[..]));
        assert_eq!(fc.mode, Some(EntryMode::Blob));
        assert!(fc.file_id.is_some());
    }

    #[test]
    fn diff_patchset_delete_only() {
        let fx = Fixture::new();
        fx.write("gone.txt", b"bye\n");
        let from = fx.commit("add gone.txt");
        fx.remove("gone.txt");
        let to = fx.commit("remove gone.txt");

        let ps = diff_patchset(&*fx.repo, &from, &to, &ws(), &epoch_from(&from), 50).unwrap();
        assert_eq!(ps.change_count(), 1);
        let fc = &ps.changes[0];
        assert_eq!(fc.path, PathBuf::from("gone.txt"));
        assert_eq!(fc.kind, ChangeKind::Deleted);
        assert!(fc.content.is_none());
        assert!(fc.blob.is_none());
        assert!(fc.file_id.is_some());
        assert_eq!(fc.mode, Some(EntryMode::Blob));
    }

    #[test]
    fn diff_patchset_rename_exact() {
        let fx = Fixture::new();
        fx.write("old.txt", b"exact-match-content\n");
        let from = fx.commit("add old");
        fx.rename("old.txt", "new.txt");
        let to = fx.commit("rename old → new");

        let ps = diff_patchset(&*fx.repo, &from, &to, &ws(), &epoch_from(&from), 50).unwrap();
        // A rename must surface TWO changes: Deleted(old) + Modified(new).
        // Both share the same FileId (derived from the old blob) so
        // downstream identity tracking can follow the move (bn-3525).
        assert_eq!(
            ps.change_count(),
            2,
            "expected rename to emit delete+modify, got {ps:?}"
        );

        let del = ps
            .changes
            .iter()
            .find(|c| c.path == PathBuf::from("old.txt"))
            .expect("Deleted(old.txt) should be present");
        let modi = ps
            .changes
            .iter()
            .find(|c| c.path == PathBuf::from("new.txt"))
            .expect("Modified(new.txt) should be present");

        assert_eq!(del.kind, ChangeKind::Deleted);
        assert!(del.content.is_none());
        assert!(del.blob.is_none());

        assert_eq!(modi.kind, ChangeKind::Modified);
        assert_eq!(modi.content.as_deref(), Some(&b"exact-match-content\n"[..]));

        // FileId identity is preserved across the rename pair.
        assert_eq!(
            del.file_id, modi.file_id,
            "rename pair must share a FileId so overlap detection can follow identity"
        );
        assert!(modi.file_id.is_some());
    }

    #[test]
    fn diff_patchset_rename_plus_edit() {
        let fx = Fixture::new();
        // Make the content long enough that an 80% overlap is clearly above
        // a 50% similarity threshold but clearly below 100%.
        let seed = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\n";
        fx.write("old.txt", seed.as_bytes());
        let from = fx.commit("seed");
        // Rename + small edit (replace the first line only).
        fx.rename("old.txt", "new.txt");
        let edited = "CHANGED\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\n";
        fx.write("new.txt", edited.as_bytes());
        let to = fx.commit("rename + edit");

        let ps = diff_patchset(&*fx.repo, &from, &to, &ws(), &epoch_from(&from), 50).unwrap();
        // Rename + edit also surfaces TWO changes.
        assert_eq!(
            ps.change_count(),
            2,
            "expected rename+edit to emit delete+modify, got {ps:?}"
        );

        let del = ps
            .changes
            .iter()
            .find(|c| c.path == PathBuf::from("old.txt"))
            .expect("Deleted(old.txt) should be present");
        let modi = ps
            .changes
            .iter()
            .find(|c| c.path == PathBuf::from("new.txt"))
            .expect("Modified(new.txt) should be present");

        assert_eq!(del.kind, ChangeKind::Deleted);
        assert_eq!(modi.kind, ChangeKind::Modified);
        assert_eq!(modi.content.as_deref(), Some(edited.as_bytes()));
        assert_eq!(del.file_id, modi.file_id);
    }

    #[test]
    fn diff_patchset_rename_rejected() {
        let fx = Fixture::new();
        let seed = "alpha\nbeta\ngamma\n";
        fx.write("old.txt", seed.as_bytes());
        let from = fx.commit("seed");
        // "Rename" + near-total rewrite. gix's default rename detection
        // should refuse to match these as a rename and produce delete+add
        // at similarity_pct=50.
        fx.remove("old.txt");
        fx.write(
            "new.txt",
            b"totally unrelated content that shares no lines with old\n",
        );
        let to = fx.commit("delete + unrelated add");

        let ps = diff_patchset(&*fx.repo, &from, &to, &ws(), &epoch_from(&from), 50).unwrap();
        assert_eq!(ps.change_count(), 2);
        let kinds: Vec<_> = ps.changes.iter().map(|c| &c.kind).collect();
        assert!(kinds.contains(&&ChangeKind::Added));
        assert!(kinds.contains(&&ChangeKind::Deleted));
    }

    #[test]
    #[cfg(unix)]
    fn diff_patchset_preserves_executable_bit() {
        let fx = Fixture::new();
        // Seed commit (no exec yet).
        fx.write("script.sh", b"#!/bin/sh\necho hi\n");
        let from = fx.commit("seed");
        // Flip to executable and re-commit.
        fx.chmod_exec("script.sh");
        let to = fx.commit("chmod +x");

        let ps = diff_patchset(&*fx.repo, &from, &to, &ws(), &epoch_from(&from), 50).unwrap();
        assert_eq!(ps.change_count(), 1);
        let fc = &ps.changes[0];
        assert_eq!(fc.path, PathBuf::from("script.sh"));
        assert_eq!(fc.mode, Some(EntryMode::BlobExecutable));
    }

    #[test]
    #[cfg(unix)]
    fn diff_patchset_preserves_symlink() {
        let fx = Fixture::new();
        fx.write("target.txt", b"real\n");
        let from = fx.commit("seed");
        fx.symlink("link", "target.txt");
        let to = fx.commit("add symlink");

        let ps = diff_patchset(&*fx.repo, &from, &to, &ws(), &epoch_from(&from), 50).unwrap();
        // Only the symlink is new — the target was present before.
        let link = ps
            .changes
            .iter()
            .find(|c| c.path == PathBuf::from("link"))
            .expect("symlink change present");
        assert_eq!(link.kind, ChangeKind::Added);
        assert_eq!(link.mode, Some(EntryMode::Link));
    }

    #[test]
    fn diff_patchset_root_commit() {
        let fx = Fixture::new();
        fx.write("a.txt", b"a\n");
        fx.write("sub/b.txt", b"b\n");
        let to = fx.commit("root");

        // Use the zero OID as the sentinel for "diff against the empty tree".
        let ps = diff_patchset(&*fx.repo, &zero_oid(), &to, &ws(), &epoch_from(&to), 50).unwrap();
        assert_eq!(ps.change_count(), 2);
        for c in &ps.changes {
            assert_eq!(c.kind, ChangeKind::Added, "path {:?}", c.path);
            assert!(c.content.is_some());
        }
        let paths: Vec<_> = ps.changes.iter().map(|c| c.path.clone()).collect();
        assert!(paths.contains(&PathBuf::from("a.txt")));
        assert!(paths.contains(&PathBuf::from("sub/b.txt")));
    }

    #[test]
    fn diff_patchset_submodule_added_has_no_content() {
        // bn-3hqg: a workspace that adds a submodule (gitlink, mode 160000)
        // must not cause diff_patchset to try `read_blob` on the gitlink SHA
        // (the SHA points at a commit in another repo — not a blob in ours).
        let fx = Fixture::new();
        fx.write("keep.txt", b"seed\n");
        let from = fx.commit("seed");

        // Build a separate repo to serve as the submodule source.
        let sub_dir = tempfile::TempDir::new().unwrap();
        run_git(sub_dir.path(), &["init", "--initial-branch=main"]);
        run_git(sub_dir.path(), &["config", "user.name", "Test"]);
        run_git(sub_dir.path(), &["config", "user.email", "test@test.com"]);
        run_git(sub_dir.path(), &["config", "commit.gpgsign", "false"]);
        fs::write(sub_dir.path().join("sub.txt"), b"hi\n").unwrap();
        run_git(sub_dir.path(), &["add", "-A"]);
        run_git(sub_dir.path(), &["commit", "-m", "sub init"]);

        // Add as a submodule in the outer repo.
        let sub_path_str = sub_dir.path().to_str().unwrap();
        run_git(
            &fx.root,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub_path_str,
                "subdir",
            ],
        );
        let to = fx.commit("add submodule");

        // This used to fail with `not found: blob <sha>` because diff_patchset
        // tried to `read_blob` the gitlink's commit OID. Now it succeeds, and
        // the produced FileChange carries content=None + mode=Commit.
        let ps = diff_patchset(&*fx.repo, &from, &to, &ws(), &epoch_from(&from), 50)
            .expect("diff_patchset must not try to read_blob the gitlink OID");

        let sub_change = ps
            .changes
            .iter()
            .find(|c| c.path == PathBuf::from("subdir"))
            .expect("submodule change should appear in the patchset");
        assert_eq!(sub_change.kind, ChangeKind::Added);
        assert_eq!(
            sub_change.mode,
            Some(EntryMode::Commit),
            "submodule entry must carry mode=Commit"
        );
        assert!(
            sub_change.content.is_none(),
            "submodule entry must carry no blob content (the OID isn't a blob)"
        );
        assert!(
            sub_change.blob.is_some(),
            "submodule entry still carries the gitlink SHA as identity"
        );
    }

    #[test]
    fn diff_patchset_sets_workspace_id_and_epoch() {
        let fx = Fixture::new();
        fx.write("a.txt", b"v1\n");
        let from = fx.commit("v1");
        fx.write("a.txt", b"v2\n");
        let to = fx.commit("v2");

        let ws_id = WorkspaceId::new("alice").unwrap();
        let epoch = epoch_from(&from);
        let ps = diff_patchset(&*fx.repo, &from, &to, &ws_id, &epoch, 50).unwrap();
        assert_eq!(ps.workspace_id, ws_id);
        assert_eq!(ps.epoch, epoch);
    }
}
