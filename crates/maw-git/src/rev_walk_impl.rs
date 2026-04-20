//! gix-backed revision-walk operations.
//!
//! Implements `walk_commits(from, to, reverse)` with `git rev-list from..to`
//! semantics: commits reachable from `to` that are not reachable from `from`.
//! `from` and any of its ancestors are excluded from the result.

use std::collections::HashSet;

use crate::error::GitError;
use crate::gix_repo::GixRepo;
use crate::types::GitOid;

/// Convert a `GitOid` to a `gix::ObjectId`.
fn to_gix_oid(oid: GitOid) -> gix::ObjectId {
    gix::ObjectId::from_bytes_or_panic(oid.as_bytes())
}

/// Convert a gix object id to our `GitOid`.
fn from_gix_oid(oid: &gix::oid) -> GitOid {
    let mut bytes = [0u8; 20];
    bytes.copy_from_slice(oid.as_bytes());
    GitOid::from_bytes(bytes)
}

/// Collect the set of commits reachable from `tip` (inclusive).
fn collect_reachable(
    repo: &GixRepo,
    tip: gix::ObjectId,
) -> Result<HashSet<gix::ObjectId>, GitError> {
    let walk = repo
        .repo
        .rev_walk([tip])
        .all()
        .map_err(|e| GitError::BackendError {
            message: format!("rev_walk from {tip}: {e}"),
        })?;

    let mut set = HashSet::new();
    for info in walk {
        let info = info.map_err(|e| GitError::BackendError {
            message: format!("rev_walk iteration: {e}"),
        })?;
        set.insert(info.id);
    }
    Ok(set)
}

/// Walk commits in range `from..to`.
///
/// Returns commits reachable from `to` that are NOT reachable from `from`,
/// matching `git rev-list from..to` semantics. When `from == to`, the range
/// is empty.
///
/// `reverse = true` returns oldest-first order (suitable for rebase replay).
/// `reverse = false` returns newest-first (gix's natural rev-walk order).
pub fn walk_commits(
    repo: &GixRepo,
    from: GitOid,
    to: GitOid,
    reverse: bool,
) -> Result<Vec<GitOid>, GitError> {
    if from == to {
        return Ok(Vec::new());
    }

    let from_gix = to_gix_oid(from);
    let to_gix = to_gix_oid(to);

    // Collect everything reachable from `from` so we can exclude it.
    let excluded = collect_reachable(repo, from_gix)?;

    // Walk from `to`, stopping traversal at any commit reachable from `from`.
    // Using `selected` with a filter lets us prune entire subtrees whose root
    // is in the excluded set — this mirrors `git rev-list ^from to` semantics.
    let walk = repo
        .repo
        .rev_walk([to_gix])
        .selected(move |id| !excluded.contains(&id.to_owned()))
        .map_err(|e| GitError::BackendError {
            message: format!("rev_walk from {to_gix}: {e}"),
        })?;

    let mut result: Vec<GitOid> = Vec::new();
    for info in walk {
        let info = info.map_err(|e| GitError::BackendError {
            message: format!("rev_walk iteration: {e}"),
        })?;
        result.push(from_gix_oid(info.id.as_ref()));
    }

    if reverse {
        result.reverse();
    }

    Ok(result)
}
