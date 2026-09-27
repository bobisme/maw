//! Pure decision logic for the FF-absorb sibling reconcile (bn-27n7).
//!
//! `maw ws merge` can absorb a fast-forward of the target branch into the
//! epoch. Every non-target sibling workspace must then be classified before
//! any ref, HEAD or worktree moves. This logic produced four data-loss
//! defects (bn-p3m9, bn-286g/bn-rah2, bn-mq3b, the pre.14 directory/file
//! prefix collision), so it lives here as I/O-free functions that
//! `maw-cli`'s `reconcile_epoch_with_branch` calls, and that the Kani
//! harnesses at the bottom of this file verify over their whole input space.
//!
//! # Path precondition
//!
//! Every path passed to these functions is a git-normalised repository
//! relative path: no leading `/`, no leading `./`, no `..` components. Tree
//! diffs and `status()` from `maw-git` produce paths in that form. Redundant
//! or trailing `/` separators are harmless ([`Path::components`] drops them).
//! A leading `./` is NOT normalised: `./a` has a `CurDir` component, so it
//! does not conflict with `a`. Callers must not pass such paths.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Whether two git paths conflict by equality or directory/file ancestry.
///
/// Two paths conflict iff the component list of one is a prefix of the
/// other's (equal paths included). Components are compared whole, so
/// similar names such as `shape` and `shapely/file.txt` remain independent,
/// while `shape` and `shape/file.txt` conflict (they cannot coexist in one
/// git tree). This is exactly
/// `left == right || left.starts_with(right) || right.starts_with(left)`:
/// [`Path`] equality and [`Path::starts_with`] are both defined over
/// [`Path::components`].
#[must_use]
pub fn paths_conflict(left: &Path, right: &Path) -> bool {
    component_prefix_either(left.components(), right.components())
}

/// Whether one sequence is a prefix of the other (equal sequences
/// included). The component-level core of [`paths_conflict`].
pub fn component_prefix_either<T: PartialEq>(
    mut left: impl Iterator<Item = T>,
    mut right: impl Iterator<Item = T>,
) -> bool {
    loop {
        match (left.next(), right.next()) {
            (None, _) | (_, None) => return true,
            (Some(l), Some(r)) if l == r => {}
            (Some(_), Some(_)) => return false,
        }
    }
}

/// bn-mq3b: the stale-dirty filter for one dirty FF sibling.
///
/// `own_delta` is the sibling's own `HEAD`-tree to absorb-target-tree diff,
/// or `None` when it could not be computed (unreadable repo, `HEAD`, commit
/// or tree). Returns the paths that make a fast-forward of this sibling
/// unsafe:
///
/// - `Some(delta)`: every delta path (in `delta` order) that conflicts with
///   at least one dirty path.
/// - `None`: fails CLOSED. Every dirty path is returned, so a dirty worktree
///   that cannot be proven safe is never advanced.
///
/// An empty result means the fast-forward is provably disjoint from the
/// sibling's uncommitted edits.
#[must_use]
pub fn stale_dirty_filter(
    own_delta: Option<Vec<PathBuf>>,
    dirty: &BTreeSet<PathBuf>,
) -> Vec<PathBuf> {
    stale_dirty_filter_by(own_delta, dirty, |stale, dirty| {
        paths_conflict(stale, dirty)
    })
}

/// [`stale_dirty_filter`] over any element type and conflict relation.
///
/// `conflicts(delta_path, dirty_path)` decides a conflict. The Kani
/// harnesses verify this for every relation over a small domain.
pub fn stale_dirty_filter_by<'a, T, D>(
    own_delta: Option<Vec<T>>,
    dirty: D,
    conflicts: impl Fn(&T, &T) -> bool,
) -> Vec<T>
where
    T: Clone + 'a,
    D: IntoIterator<Item = &'a T> + Copy,
{
    own_delta.map_or_else(
        || dirty.into_iter().cloned().collect(),
        |delta| {
            delta
                .into_iter()
                .filter(|stale_path| {
                    dirty
                        .into_iter()
                        .any(|dirty_path| conflicts(stale_path, dirty_path))
                })
                .collect()
        },
    )
}

/// bn-p3m9: how an FF sibling's own stale delta is split when it is
/// materialized.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OwnDeltaSplit {
    /// Paths outside the global FF range to refresh from the absorb target.
    pub refresh: Vec<PathBuf>,
    /// Paths outside the global FF range left alone because they conflict
    /// with an uncommitted edit.
    pub skipped_dirty: Vec<PathBuf>,
}

/// bn-p3m9: split a sibling's own `HEAD`-to-target delta into the paths to
/// refresh and the dirty-conflicting paths to skip.
///
/// Paths already in `ff_paths` are dropped: the caller materializes the
/// whole FF range anyway, and the FF-absorb safety predicate proved them
/// free of local edits. Every other delta path lands in exactly one list, in
/// input order. A path that conflicts (exactly or by directory/file
/// ancestry) with a dirty path is never refreshed.
#[must_use]
pub fn split_own_delta(
    own_delta: Vec<PathBuf>,
    ff_paths: &BTreeSet<PathBuf>,
    dirty: &BTreeSet<PathBuf>,
) -> OwnDeltaSplit {
    let (refresh, skipped_dirty) = split_own_delta_by(
        own_delta,
        |rel| ff_paths.contains(rel),
        dirty,
        |rel, dirty_path| paths_conflict(rel, dirty_path),
    );
    OwnDeltaSplit {
        refresh,
        skipped_dirty,
    }
}

/// [`split_own_delta`] over any element type, FF-membership test and
/// conflict relation. Returns `(refresh, skipped_dirty)`.
pub fn split_own_delta_by<'a, T, D>(
    own_delta: Vec<T>,
    in_ff: impl Fn(&T) -> bool,
    dirty: D,
    conflicts: impl Fn(&T, &T) -> bool,
) -> (Vec<T>, Vec<T>)
where
    T: 'a,
    D: IntoIterator<Item = &'a T> + Copy,
{
    let mut refresh = Vec::new();
    let mut skipped_dirty = Vec::new();
    for rel in own_delta {
        if in_ff(&rel) {
            continue;
        }
        if dirty
            .into_iter()
            .any(|dirty_path| conflicts(&rel, dirty_path))
        {
            skipped_dirty.push(rel);
            continue;
        }
        refresh.push(rel);
    }
    (refresh, skipped_dirty)
}

/// What the sibling's `HEAD` looks like relative to its base epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiblingHead {
    /// `HEAD` could not be opened or resolved.
    Unreadable,
    /// `HEAD` equals the sibling's base epoch (no committed work ahead).
    AtBase,
    /// `HEAD` differs from the base epoch: committed work ahead of it.
    Ahead,
}

/// The I/O facts about one sibling that the classifier needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SiblingProbe {
    /// State of `HEAD` relative to the base epoch.
    pub head: SiblingHead,
    /// Whether the worktree has uncommitted edits.
    pub dirty: bool,
}

/// Per-sibling FF-absorb decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiblingDecision {
    /// The sibling's base epoch already is the absorb target. Nothing to do.
    AlreadySynced,
    /// Committed-ahead AND dirty: cannot be replayed (rebase refuses a dirty
    /// worktree), so the whole absorb is refused.
    BlockAbsorb,
    /// Committed-ahead and clean: replay its commits onto the absorb target.
    Replay,
    /// Advance the epoch ref and `HEAD` and materialize the absorbed paths.
    FastForward,
    /// bn-mq3b: dirty with uncommitted edits that are stale against the
    /// absorb target. Leave it exactly where it is.
    SkipStaleDirty,
    /// bn-302v: `HEAD` could not be read, so whether committed work sits
    /// ahead of the base epoch is unknown. Fails closed: leave the sibling
    /// exactly where it is (clean or dirty), never fast-forward it.
    SkipUnreadableHead,
}

/// Whether the classifier consults the stale-dirty check for this probe.
///
/// True only for a dirty sibling whose `HEAD` is at its base epoch.
#[must_use]
pub const fn needs_stale_dirty_check(probe: SiblingProbe) -> bool {
    probe.dirty && matches!(probe.head, SiblingHead::AtBase)
}

/// Classify one non-target sibling for the FF-absorb.
///
/// The I/O is supplied lazily, in the order the production code performs
/// it:
///
/// 1. `base_is_branch`: the sibling's base epoch equals the absorb target.
///    If so the sibling is [`SiblingDecision::AlreadySynced`] and neither
///    closure runs (no `HEAD` read, no dirty inspection).
/// 2. `probe`: reads `HEAD` and the dirty state. An error propagates
///    unchanged (the absorb is refused before any mutation).
/// 3. `stale_dirty_nonempty`: runs only when [`needs_stale_dirty_check`]
///    holds. It must report whether [`stale_dirty_filter`] is non-empty.
///
/// Rules:
/// - `HEAD` ahead + dirty: [`SiblingDecision::BlockAbsorb`].
/// - `HEAD` ahead + clean: [`SiblingDecision::Replay`].
/// - `HEAD` unreadable (clean or dirty):
///   [`SiblingDecision::SkipUnreadableHead`] (bn-302v: fails closed; it
///   used to fast-forward a clean sibling, which could detach a `HEAD` that
///   a transient read failure hid, orphaning its commits).
/// - `HEAD` at base + dirty + stale overlap:
///   [`SiblingDecision::SkipStaleDirty`].
/// - otherwise (`HEAD` at base): [`SiblingDecision::FastForward`].
///
/// The same function is the bn-302v re-check: immediately before an FF
/// sibling is written, the caller re-probes it under the sibling lock
/// (with the classification-time `HEAD` as the base) and proceeds only on
/// [`SiblingDecision::FastForward`].
///
/// # Errors
/// Returns the error of `probe`, unchanged.
pub fn classify_sibling<E>(
    base_is_branch: bool,
    probe: impl FnOnce() -> Result<SiblingProbe, E>,
    stale_dirty_nonempty: impl FnOnce() -> bool,
) -> Result<SiblingDecision, E> {
    if base_is_branch {
        return Ok(SiblingDecision::AlreadySynced);
    }
    let probe = probe()?;
    Ok(match probe.head {
        SiblingHead::Ahead => {
            if probe.dirty {
                SiblingDecision::BlockAbsorb
            } else {
                SiblingDecision::Replay
            }
        }
        SiblingHead::Unreadable => SiblingDecision::SkipUnreadableHead,
        SiblingHead::AtBase => {
            if needs_stale_dirty_check(probe) && stale_dirty_nonempty() {
                SiblingDecision::SkipStaleDirty
            } else {
                SiblingDecision::FastForward
            }
        }
    })
}

/// Batch verdict over all sibling decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbsorbVerdict {
    /// No sibling blocks. The caller may execute the per-sibling plans.
    Proceed,
    /// At least one sibling blocks. Carries the indices (ascending) of every
    /// blocking sibling and no mutation plan: the caller must refuse the
    /// absorb before touching any ref, `HEAD` or worktree.
    Block { blocked: Vec<usize> },
}

/// Decide whether the absorb proceeds, given every sibling's decision.
#[must_use]
pub fn absorb_verdict(decisions: &[SiblingDecision]) -> AbsorbVerdict {
    let blocked: Vec<usize> = decisions
        .iter()
        .enumerate()
        .filter(|(_, d)| **d == SiblingDecision::BlockAbsorb)
        .map(|(i, _)| i)
        .collect();
    if blocked.is_empty() {
        AbsorbVerdict::Proceed
    } else {
        AbsorbVerdict::Block { blocked }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<PathBuf> {
        items.iter().map(PathBuf::from).collect()
    }

    fn vec_of(items: &[&str]) -> Vec<PathBuf> {
        items.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn paths_conflict_is_component_prefix() {
        assert!(paths_conflict(Path::new("a"), Path::new("a")));
        assert!(paths_conflict(
            Path::new("shape"),
            Path::new("shape/file.txt")
        ));
        assert!(paths_conflict(
            Path::new("shape/file.txt"),
            Path::new("shape")
        ));
        assert!(!paths_conflict(
            Path::new("shape"),
            Path::new("shapely/file.txt")
        ));
        assert!(!paths_conflict(Path::new("shape"), Path::new("shapes")));
    }

    /// Every path of 1..=3 components over {a, b, ab}, with the component
    /// ids, in plain / doubled-separator / trailing-separator / leading-`./`
    /// spellings. Returns `(path, ids, git_normalised)`.
    fn all_paths_le_3_comps() -> Vec<(PathBuf, Vec<usize>, bool)> {
        const COMPS: [&str; 3] = ["a", "b", "ab"];
        let mut id_lists: Vec<Vec<usize>> = Vec::new();
        for n in 1..=3 {
            let mut cur = vec![0usize; n];
            loop {
                id_lists.push(cur.clone());
                let mut k = 0;
                while k < n && cur[k] == COMPS.len() - 1 {
                    cur[k] = 0;
                    k += 1;
                }
                if k == n {
                    break;
                }
                cur[k] += 1;
            }
        }
        let mut out = Vec::new();
        for ids in id_lists {
            let names: Vec<&str> = ids.iter().map(|&i| COMPS[i]).collect();
            out.push((PathBuf::from(names.join("/")), ids.clone(), true));
            out.push((PathBuf::from(names.join("//")), ids.clone(), true));
            out.push((
                PathBuf::from(format!("{}/", names.join("/"))),
                ids.clone(),
                true,
            ));
            out.push((PathBuf::from(format!("./{}", names.join("/"))), ids, false));
        }
        out
    }

    /// Exhaustive over all pairs of `all_paths_le_3_comps` (24 336 pairs):
    /// the component-walk implementation equals the pre-bn-27n7 expression
    /// on EVERY spelling (behaviour preserved), and equals the
    /// component-prefix spec on git-normalised spellings.
    #[test]
    fn paths_conflict_exhaustive_le_3_comps_matches_legacy_and_spec() {
        let paths = all_paths_le_3_comps();
        for (a, ca, na) in &paths {
            for (b, cb, nb) in &paths {
                let got = paths_conflict(a, b);
                let legacy = a == b || a.starts_with(b) || b.starts_with(a);
                assert_eq!(got, legacy, "{} vs {}", a.display(), b.display());
                if *na && *nb {
                    let spec = ca.starts_with(cb) || cb.starts_with(ca);
                    assert_eq!(got, spec, "{} vs {}", a.display(), b.display());
                }
            }
        }
    }

    #[test]
    fn paths_conflict_does_not_normalise_curdir() {
        // Documented precondition: callers pass git-normalised paths.
        assert!(!paths_conflict(Path::new("./a"), Path::new("a")));
    }

    #[test]
    fn stale_filter_fails_closed_on_unknown_delta() {
        let dirty = set(&["x", "y/z"]);
        assert_eq!(stale_dirty_filter(None, &dirty), vec_of(&["x", "y/z"]));
        assert!(stale_dirty_filter(None, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn stale_filter_keeps_delta_order_and_prefix_conflicts() {
        let dirty = set(&["shape"]);
        let delta = vec_of(&["z", "shape/b", "shapely", "shape/a"]);
        assert_eq!(
            stale_dirty_filter(Some(delta), &dirty),
            vec_of(&["shape/b", "shape/a"])
        );
    }

    #[test]
    fn split_own_delta_partitions() {
        let ff = set(&["in-ff"]);
        let dirty = set(&["d"]);
        let split = split_own_delta(vec_of(&["in-ff", "d/x", "other", "d"]), &ff, &dirty);
        assert_eq!(split.refresh, vec_of(&["other"]));
        assert_eq!(split.skipped_dirty, vec_of(&["d/x", "d"]));
    }

    #[test]
    fn classify_table() {
        use SiblingDecision as D;
        use SiblingHead as H;
        let c = |head, dirty, stale| {
            classify_sibling::<()>(false, || Ok(SiblingProbe { head, dirty }), || stale).unwrap()
        };
        assert_eq!(c(H::Ahead, true, false), D::BlockAbsorb);
        assert_eq!(c(H::Ahead, false, true), D::Replay);
        assert_eq!(c(H::AtBase, false, true), D::FastForward);
        assert_eq!(c(H::AtBase, true, false), D::FastForward);
        assert_eq!(c(H::AtBase, true, true), D::SkipStaleDirty);
        assert_eq!(c(H::Unreadable, true, true), D::SkipUnreadableHead);
        assert_eq!(c(H::Unreadable, true, false), D::SkipUnreadableHead);
        assert_eq!(c(H::Unreadable, false, false), D::SkipUnreadableHead);
        assert_eq!(
            classify_sibling::<()>(true, || panic!("probe"), || panic!("stale")).unwrap(),
            D::AlreadySynced
        );
        assert_eq!(
            classify_sibling::<&str>(false, || Err("io"), || panic!("stale")),
            Err("io")
        );
    }

    #[test]
    fn verdict_lists_blockers() {
        use SiblingDecision as D;
        assert_eq!(absorb_verdict(&[]), AbsorbVerdict::Proceed);
        assert_eq!(
            absorb_verdict(&[D::Replay, D::FastForward, D::SkipStaleDirty]),
            AbsorbVerdict::Proceed
        );
        assert_eq!(
            absorb_verdict(&[D::BlockAbsorb, D::Replay, D::BlockAbsorb]),
            AbsorbVerdict::Block {
                blocked: vec![0, 2]
            }
        );
    }
}

#[cfg(kani)]
#[path = "ff_plan_kani.rs"]
mod kani_proofs;
