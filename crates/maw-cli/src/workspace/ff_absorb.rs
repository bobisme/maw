//! Fast-forward absorb safety predicate for `maw ws merge`.
//!
//! When the configured target branch has advanced past the epoch (typically
//! because someone ran `git commit` + `git push` directly), the existing
//! merge gate refuses to proceed and asks the user to run `maw epoch sync`.
//!
//! For a strict subset of those cases — where the divergence is a pure
//! fast-forward AND no in-flight workspace touches any file changed in the
//! FF range — advancing the epoch is provably safe. The diff3 base for every
//! in-flight workspace's patches is unchanged for the paths each workspace
//! has touched, so absorbing the FF leaves their merge interpretation
//! identical.
//!
//! This module exposes the pure-function safety predicate
//! [`evaluate_ff_safety`] together with the I/O helper
//! [`compute_ff_changed_paths`] that builds its `ff_paths` argument from
//! commit OIDs.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use maw_git::{ChangeType, GitOid as MawGitOid, GitRepo as _, GixRepo};

/// Outcome of the FF-absorb safety predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FfAbsorbDecision {
    /// Safe to absorb: no in-flight workspace touches any FF path.
    Safe,
    /// Unsafe: the listed workspaces touch at least one path in the FF range.
    ///
    /// Names are sorted, deduplicated, and stable across calls.
    Blocked { affected_workspaces: Vec<String> },
}

/// Touched-path summary for one in-flight workspace.
///
/// `ws_paths` MUST be the workspace's touched-path set computed against its
/// actual `base_epoch` (e.g. from
/// [`super::touched::collect_touched_workspace`]). The predicate does not
/// recompute it; callers are responsible for computing it at decision time
/// (no caching across phases).
#[derive(Debug, Clone)]
pub struct WorkspaceTouchedPaths {
    pub name: String,
    pub paths: BTreeSet<PathBuf>,
}

/// Pure-function safety predicate.
///
/// Returns [`FfAbsorbDecision::Safe`] iff `ff_paths ∩ ws_paths == ∅` for
/// every workspace in `workspaces`. Otherwise returns
/// [`FfAbsorbDecision::Blocked`] with the names of the workspaces that have
/// at least one path in `ff_paths`.
///
/// A workspace with an empty touched-path set is trivially safe (cannot
/// intersect anything).
#[must_use]
pub fn evaluate_ff_safety(
    ff_paths: &BTreeSet<PathBuf>,
    workspaces: &[WorkspaceTouchedPaths],
) -> FfAbsorbDecision {
    if ff_paths.is_empty() {
        return FfAbsorbDecision::Safe;
    }

    let mut affected: BTreeSet<String> = BTreeSet::new();
    for ws in workspaces {
        if ws.paths.iter().any(|p| ff_paths.contains(p)) {
            affected.insert(ws.name.clone());
        }
    }

    if affected.is_empty() {
        FfAbsorbDecision::Safe
    } else {
        FfAbsorbDecision::Blocked {
            affected_workspaces: affected.into_iter().collect(),
        }
    }
}

/// Strict-ancestor predicate: returns `true` iff `epoch != branch` AND
/// `epoch` is reachable from `branch` via parent links.
///
/// `is_ancestor` from gix is non-strict (returns `true` when the two OIDs
/// are equal). This wrapper enforces the strictness the FF-absorb logic
/// requires.
///
/// # Errors
/// Returns an error if the underlying gix walk fails.
pub fn is_strict_ancestor(repo: &GixRepo, epoch: &MawGitOid, branch: &MawGitOid) -> Result<bool> {
    if epoch == branch {
        return Ok(false);
    }
    repo.is_ancestor(*epoch, *branch)
        .map_err(|e| anyhow!("ancestry check failed: {e}"))
}

/// Compute the set of file paths changed in the commit range `epoch..branch`.
///
/// Resolves each commit to its tree and uses the `maw-git` tree-diff helper
/// to enumerate changed paths. For renames, both the old and new path are
/// included in the result — matching the behaviour of
/// [`super::touched::touched_paths_from_patchset`].
///
/// # Errors
/// Returns an error if either commit cannot be resolved or the tree diff
/// fails.
pub fn compute_ff_changed_paths(
    repo: &GixRepo,
    epoch: &MawGitOid,
    branch: &MawGitOid,
) -> Result<BTreeSet<PathBuf>> {
    let epoch_tree = repo
        .read_commit(*epoch)
        .map_err(|e| anyhow!("failed to read epoch commit {epoch}: {e}"))?
        .tree_oid;
    let branch_tree = repo
        .read_commit(*branch)
        .map_err(|e| anyhow!("failed to read branch commit {branch}: {e}"))?
        .tree_oid;

    let entries = repo
        .diff_trees(Some(epoch_tree), branch_tree)
        .map_err(|e| anyhow!("tree diff {epoch}..{branch} failed: {e}"))?;

    let mut paths: BTreeSet<PathBuf> = BTreeSet::new();
    for entry in entries {
        match entry.change_type {
            ChangeType::Added | ChangeType::Modified | ChangeType::Deleted => {
                paths.insert(PathBuf::from(entry.path));
            }
            ChangeType::Renamed { from } => {
                paths.insert(PathBuf::from(entry.path));
                if !from.is_empty() {
                    paths.insert(PathBuf::from(from));
                }
            }
        }
    }
    Ok(paths)
}

/// Open the gix repo at `root` for FF-absorb queries. Convenience wrapper.
///
/// # Errors
/// Returns an error if the repository cannot be opened.
pub fn open_repo(root: &Path) -> Result<GixRepo> {
    GixRepo::open(root).map_err(|e| anyhow!("failed to open repo at {}: {e}", root.display()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn paths<I: IntoIterator<Item = &'static str>>(items: I) -> BTreeSet<PathBuf> {
        items.into_iter().map(PathBuf::from).collect()
    }

    fn ws(name: &str, paths_iter: &[&'static str]) -> WorkspaceTouchedPaths {
        WorkspaceTouchedPaths {
            name: name.to_owned(),
            paths: paths_iter.iter().copied().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn empty_ff_range_is_trivially_safe() {
        let workspaces = vec![ws("alice", &["src/lib.rs"])];
        assert_eq!(
            evaluate_ff_safety(&BTreeSet::new(), &workspaces),
            FfAbsorbDecision::Safe
        );
    }

    #[test]
    fn no_workspaces_is_trivially_safe() {
        let ff = paths(["docs/README.md"]);
        assert_eq!(evaluate_ff_safety(&ff, &[]), FfAbsorbDecision::Safe);
    }

    #[test]
    fn workspace_with_empty_touched_set_does_not_block() {
        let ff = paths(["docs/README.md"]);
        let workspaces = vec![ws("alice", &[])];
        assert_eq!(evaluate_ff_safety(&ff, &workspaces), FfAbsorbDecision::Safe);
    }

    #[test]
    fn disjoint_paths_are_safe() {
        let ff = paths(["docs/README.md", "docs/HOWTO.md"]);
        let workspaces = vec![ws("alice", &["src/lib.rs", "src/main.rs"])];
        assert_eq!(evaluate_ff_safety(&ff, &workspaces), FfAbsorbDecision::Safe);
    }

    #[test]
    fn single_overlapping_workspace_is_blocked() {
        let ff = paths(["src/lib.rs"]);
        let workspaces = vec![ws("alice", &["src/lib.rs"]), ws("bob", &["docs/README.md"])];
        let decision = evaluate_ff_safety(&ff, &workspaces);
        assert_eq!(
            decision,
            FfAbsorbDecision::Blocked {
                affected_workspaces: vec!["alice".to_owned()]
            }
        );
    }

    #[test]
    fn multiple_overlapping_workspaces_are_all_listed() {
        let ff = paths(["src/lib.rs", "Cargo.toml"]);
        let workspaces = vec![
            ws("alice", &["src/lib.rs"]),
            ws("bob", &["docs/README.md"]),
            ws("carol", &["Cargo.toml"]),
        ];
        let decision = evaluate_ff_safety(&ff, &workspaces);
        assert_eq!(
            decision,
            FfAbsorbDecision::Blocked {
                affected_workspaces: vec!["alice".to_owned(), "carol".to_owned()]
            }
        );
    }

    #[test]
    fn affected_list_is_sorted_and_deduplicated() {
        let ff = paths(["src/lib.rs"]);
        // Workspaces deliberately listed in non-sorted order; "alice" only
        // intersects once even though `paths` set has only one entry.
        let workspaces = vec![
            ws("zelda", &["src/lib.rs"]),
            ws("alice", &["src/lib.rs"]),
            ws("mike", &["docs/x.md"]),
        ];
        let decision = evaluate_ff_safety(&ff, &workspaces);
        assert_eq!(
            decision,
            FfAbsorbDecision::Blocked {
                affected_workspaces: vec!["alice".to_owned(), "zelda".to_owned()]
            }
        );
    }

    #[test]
    fn intersection_via_rename_source() {
        // Models the case where the FF range deletes/renames a file the
        // workspace also touches via the original (pre-rename) path.
        let ff = paths(["old/name.rs", "new/name.rs"]);
        let workspaces = vec![ws("alice", &["old/name.rs"])];
        let decision = evaluate_ff_safety(&ff, &workspaces);
        assert_eq!(
            decision,
            FfAbsorbDecision::Blocked {
                affected_workspaces: vec!["alice".to_owned()]
            }
        );
    }
}
