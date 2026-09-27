//! Diagnostic for `maw ws destroy` refusals whose content already landed
//! in the epoch under different commit hashes (bn-2eszz).
//!
//! Field report (bn-1ijl): an agent worked around a failed sync by
//! cherry-picking a workspace's commits into a fresh workspace and merging
//! that one. The original workspace then refused `maw ws destroy` with
//! "14 unmerged change(s)" — correct by construction (its commits are not
//! reachable from the epoch), but the agent had no way to tell that the
//! *content* was already integrated.
//!
//! This module only **explains** the refusal. It never changes whether
//! destroy refuses: the caller has already decided to refuse, and
//! `--force` still captures a recovery snapshot. Every failure to inspect
//! the repository yields `None` (no hint) — a missing hint is always safe,
//! a wrong hint is not.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use maw_git::{GitOid, GitRepo as _};
use serde::Serialize;

/// Which check proved the workspace's committed content is already in the
/// epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContentMatch {
    /// The workspace HEAD tree is identical to the epoch tree.
    #[serde(rename = "tree-equals-epoch")]
    TreeEquals,
    /// Every path the workspace changed (base -> HEAD) has identical
    /// content and mode in the epoch.
    #[serde(rename = "changes-present-in-epoch")]
    ChangesPresent,
    /// Every workspace commit has a patch-id equivalent commit in the
    /// epoch history (`git rev-list --cherry-mark`).
    #[serde(rename = "patches-in-epoch")]
    PatchIdsMatch,
}

impl ContentMatch {
    const fn describe(self) -> &'static str {
        match self {
            Self::TreeEquals => "the workspace HEAD tree is identical to the epoch tree",
            Self::ChangesPresent => {
                "every file this workspace changed has identical content in the epoch"
            }
            Self::PatchIdsMatch => {
                "every workspace commit has an equivalent patch (same patch-id) in the epoch"
            }
        }
    }
}

/// Evidence that a refused workspace's content is already in the epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContentAlreadyInEpoch {
    /// The check that matched.
    pub evidence: ContentMatch,
    /// Full OID of the epoch the workspace was compared against.
    pub epoch: String,
    /// Number of workspace commits not reachable from the epoch.
    pub unreachable_commits: u32,
}

impl ContentAlreadyInEpoch {
    /// One-line human explanation used by the text refusal renderer.
    #[must_use]
    pub fn render_note(&self, workspace: &str) -> String {
        let short = self.epoch.get(..12).unwrap_or(&self.epoch);
        format!(
            "NOTE: content already in epoch {short}: {}. The {} workspace commit(s) \
             are not reachable from the epoch (different hashes, e.g. cherry-picked), \
             so maw still counts them as unmerged. \
             If nothing else in '{workspace}' is needed: maw ws destroy {workspace} --force \
             (still captures a recovery snapshot)",
            self.evidence.describe(),
            self.unreachable_commits,
        )
    }
}

/// Decide whether a workspace that destroy is about to refuse has its
/// committed content already in `epoch_oid`.
///
/// Returns `None` whenever the answer is not a confident "yes": dirty
/// worktree, no commits beyond the epoch, any repository read failure, or
/// any workspace change missing from the epoch.
#[must_use]
pub fn detect(
    ws_path: &Path,
    base_epoch_oid: &str,
    epoch_oid: &str,
    dirty_count: usize,
) -> Option<ContentAlreadyInEpoch> {
    // Uncommitted edits are never in the epoch by definition of this check.
    if dirty_count > 0 {
        return None;
    }
    let repo = maw_git::GixRepo::open(ws_path).ok()?;
    let head = repo.rev_parse("HEAD").ok()?;
    let epoch: GitOid = epoch_oid.parse().ok()?;
    let base: GitOid = base_epoch_oid.parse().ok()?;

    // Commits on HEAD that the epoch cannot reach — the reason destroy
    // refused. Zero means reachability is not the problem; no hint.
    if repo.is_ancestor(head, epoch).ok()? {
        return None;
    }
    let merge_base = repo.merge_base(epoch, head).ok()??;
    let unreachable_commits = repo.count_commits_between(merge_base, head).ok()?;
    if unreachable_commits == 0 {
        return None;
    }

    let head_tree = repo.read_commit(head).ok()?.tree_oid;
    let epoch_tree = repo.read_commit(epoch).ok()?.tree_oid;
    let base_tree = repo.read_commit(base).ok()?.tree_oid;

    let evidence = if head_tree == epoch_tree {
        ContentMatch::TreeEquals
    } else if changes_present_in(&repo, base_tree, head_tree, epoch_tree)? {
        ContentMatch::ChangesPresent
    } else if patches_in_epoch(ws_path, epoch_oid)? {
        ContentMatch::PatchIdsMatch
    } else {
        return None;
    };

    Some(ContentAlreadyInEpoch {
        evidence,
        epoch: epoch_oid.to_string(),
        unreachable_commits,
    })
}

/// True iff the workspace changed at least one path (base -> HEAD) and none
/// of those paths differ between the epoch and HEAD.
fn changes_present_in(
    repo: &maw_git::GixRepo,
    base_tree: GitOid,
    head_tree: GitOid,
    epoch_tree: GitOid,
) -> Option<bool> {
    let changed: BTreeSet<String> = repo
        .diff_trees(Some(base_tree), head_tree)
        .ok()?
        .into_iter()
        .map(|e| e.path)
        .collect();
    if changed.is_empty() {
        return Some(false);
    }
    let differs_from_epoch: BTreeSet<String> = repo
        .diff_trees(Some(epoch_tree), head_tree)
        .ok()?
        .into_iter()
        .map(|e| e.path)
        .collect();
    Some(changed.is_disjoint(&differs_from_epoch))
}

/// True iff every commit in `epoch...HEAD` on the HEAD side is marked
/// patch-equivalent (`=`) to a commit on the epoch side. Merge commits have
/// no patch-id and are marked `+`, so they defeat the hint (conservative).
fn patches_in_epoch(ws_path: &Path, epoch_oid: &str) -> Option<bool> {
    let out = Command::new("git")
        .arg("-C")
        .arg(ws_path)
        .args(["rev-list", "--cherry-mark", "--right-only"])
        .arg(format!("{epoch_oid}...HEAD"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8(out.stdout).ok()?;
    let mut any = false;
    for line in stdout.lines().map(str::trim).filter(|l| !l.is_empty()) {
        any = true;
        if !line.starts_with('=') {
            return Some(false);
        }
    }
    Some(any)
}
