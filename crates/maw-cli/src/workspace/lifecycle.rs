//! Safe-cleanup state vocabulary for the stale-state-self-healing fix
//! (SG4 / bn-221b).
//!
//! Names the workspace-lifecycle states an agent must distinguish to
//! make a safe next-action choice without re-running discovery verbs.
//! The vocabulary is the second leg of the bn-221b mitigation class
//! (alongside the `maw status --json` enrichment and the event-log
//! hook); the friction cluster it targets is `ws_sync_stale_workspace`
//! (`MawVerbAttribution::WsSyncStaleWorkspace`).
//!
//! # Why a named vocabulary
//!
//! Pre-fix, the JSON for a workspace surfaced `state: "active"` or a
//! free-text `state: "stale (behind by N epoch(s))"` and the agent
//! had to *infer* the next action. This cluster's friction is exactly
//! that inference: the agent tries the wrong verb, gets a stale-signal
//! error, then runs `maw ws sync` — a wasted turn the bone is funded
//! to eliminate.
//!
//! Naming the state, and pairing it with an exact `fix_command`, lets
//! the agent's first attempt be the right one.
//!
//! # The seven states
//!
//! The classifier returns *exactly one* of the seven enum variants;
//! priority order is encoded in [`LifecycleState::classify`]. Higher
//! priority wins (e.g., `Missing` beats every other signal because a
//! missing worktree dir invalidates dirty/conflict checks).

use serde::Serialize;

/// Named lifecycle state of a workspace.
///
/// Variants are ordered by classification priority — the highest-priority
/// variant that matches wins. JSON serializes as kebab-case per the
/// safe-cleanup vocabulary spec in the bn-221b mitigation class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LifecycleState {
    /// Worktree dir is gone from disk but the registry still advertises
    /// it. The recovery snapshot may still hold the contents; agent
    /// should consult `maw ws recover` before assuming work was lost.
    Missing,
    /// Unresolved rebase conflict markers live in tracked files. Must
    /// be resolved (or accepted via `maw ws resolve --keep`) before
    /// the workspace can be merged.
    Conflicted,
    /// Workspace's base epoch is behind the current epoch — a stale
    /// state. Next action is `maw ws sync <name>` (ephemeral) or
    /// `maw ws advance <name>` (persistent).
    Stale,
    /// Workspace HEAD has committed work that is not yet on the
    /// integration branch. Next action is `maw ws merge <name>`.
    CommittedUnintegrated,
    /// Workspace has uncommitted edits in the working tree. Next
    /// action is to commit (or stash) before merging.
    DirtyUncommitted,
    /// Workspace is up-to-date with the current epoch and has no
    /// committed nor uncommitted work to integrate. Safe to destroy
    /// without `--force` or to keep idle.
    Clean,
    /// Workspace has been merged into the integration branch and its
    /// committed work is present at the current epoch. Equivalent to
    /// `Clean` for safety purposes but distinguishes the lineage —
    /// the workspace had work and that work landed.
    Integrated,
    /// Workspace was destroyed (typically with `--force` while it had
    /// committed work) and a recovery snapshot is pinned. The
    /// workspace dir no longer exists, but its committed work lives
    /// in a recovery ref and has NOT yet been integrated to the
    /// branch. SG4 `bn-29fi` (destroy-prevention) cue: when this
    /// state is visible, the next action is to recover and merge
    /// the queued work, NOT to destroy another workspace.
    AbandonedWithSnapshot,
}

impl LifecycleState {
    /// Stable slug used in serialization and CLI output. Matches the
    /// serde rename so JSON and printout agree.
    ///
    /// Currently only consumed by tests; callers that need a slug at
    /// runtime should prefer serde (the JSON output is the contract).
    /// Kept on the public surface so future text renderers can use it
    /// without re-deriving the mapping.
    #[allow(dead_code)]
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Conflicted => "conflicted",
            Self::Stale => "stale",
            Self::CommittedUnintegrated => "committed-unintegrated",
            Self::DirtyUncommitted => "dirty-uncommitted",
            Self::Clean => "clean",
            Self::Integrated => "integrated",
            Self::AbandonedWithSnapshot => "abandoned-with-snapshot",
        }
    }

    /// Classify a workspace into a single lifecycle state from the
    /// already-collected discovery signals.
    ///
    /// Priority (highest first):
    /// 1. `AbandonedWithSnapshot` — worktree dir gone AND a pinned
    ///    recovery snapshot exists (bn-29fi destroy-prevention cue:
    ///    more specific than plain `Missing`).
    /// 2. `Missing` — worktree dir gone (invalidates other checks)
    /// 3. `Conflicted` — unresolved conflict markers
    /// 4. `Stale` — base epoch behind current
    /// 5. `CommittedUnintegrated` — commits ahead of epoch
    /// 6. `DirtyUncommitted` — uncommitted edits
    /// 7. `Integrated` — previously-recorded work that landed (currently
    ///    a hint surface; classifier returns this only when
    ///    `commits_ahead == 0` AND the caller passes `was_integrated = true`)
    /// 8. `Clean` — otherwise
    #[must_use]
    pub const fn classify(signals: LifecycleSignals) -> Self {
        if signals.missing {
            if signals.has_pinned_snapshot {
                return Self::AbandonedWithSnapshot;
            }
            return Self::Missing;
        }
        if signals.rebase_conflicts > 0 {
            return Self::Conflicted;
        }
        if signals.is_stale {
            return Self::Stale;
        }
        if signals.commits_ahead > 0 {
            return Self::CommittedUnintegrated;
        }
        if signals.has_uncommitted {
            return Self::DirtyUncommitted;
        }
        if signals.was_integrated {
            return Self::Integrated;
        }
        Self::Clean
    }

    /// Return the recommended next-action command for this state, if
    /// the state has a single obvious one. Returns `None` for
    /// terminal/no-op states (`Clean`, `Integrated`).
    ///
    /// `mode_persistent` selects the right verb for stale workspaces
    /// — persistent workspaces want `maw ws advance`, ephemeral want
    /// `maw ws sync`.
    #[must_use]
    pub fn fix_command(self, ws_name: &str, mode_persistent: bool) -> Option<String> {
        match self {
            Self::Missing => Some(format!("maw ws recover {ws_name}")),
            Self::AbandonedWithSnapshot => {
                // bn-29fi destroy-prevention: the snapshot exists AND
                // typically carries committed-unintegrated work. The
                // mergeback cue is recover-into-new-ws then merge.
                Some(format!("maw ws recover {ws_name} --to {ws_name}-restored"))
            }
            Self::Conflicted => Some(format!("maw ws resolve {ws_name} --list")),
            Self::Stale => {
                if mode_persistent {
                    Some(format!("maw ws advance {ws_name}"))
                } else {
                    Some(format!("maw ws sync {ws_name}"))
                }
            }
            Self::CommittedUnintegrated => {
                Some(format!("maw ws merge {ws_name} --into default --check"))
            }
            Self::DirtyUncommitted => Some(format!("maw exec {ws_name} -- git status")),
            Self::Clean | Self::Integrated => None,
        }
    }
}

/// Discovery signals fed into [`LifecycleState::classify`].
///
/// All fields are non-Option to keep the classifier total. The caller
/// is responsible for resolving "unknown" into a conservative value
/// (e.g., treat unknown rebase-conflict count as 0 so the classifier
/// doesn't promote a healthy workspace to `Conflicted`).
///
/// The four bool fields represent independent discovery dimensions
/// (filesystem presence, conflict-marker presence, cwd dirtiness, and
/// the caller-supplied integrated hint). Collapsing them into a state
/// enum here would re-introduce the priority confusion this module
/// exists to fix.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LifecycleSignals {
    /// Worktree directory is absent from disk.
    pub missing: bool,
    /// Count of files with unresolved conflict markers.
    pub rebase_conflicts: u32,
    /// Base epoch is behind the current authoritative epoch.
    pub is_stale: bool,
    /// Number of commits on the workspace HEAD ahead of its base
    /// epoch — i.e., "work to merge".
    pub commits_ahead: u32,
    /// Workspace has uncommitted edits in the working tree (tracked).
    pub has_uncommitted: bool,
    /// Caller-provided hint: this workspace's committed work has
    /// already landed at the current epoch (used by report renderers
    /// to distinguish a freshly-merged workspace from a never-edited
    /// one). The classifier never sets this from other signals.
    pub was_integrated: bool,
    /// Caller-provided hint (bn-29fi): a pinned recovery snapshot
    /// exists for this workspace (a destroy-record under
    /// `.manifold/artifacts/ws/<name>/destroy/` and/or a recovery ref
    /// under `refs/manifold/recovery/<name>/`). Combined with
    /// `missing = true`, promotes the classification from `Missing`
    /// to `AbandonedWithSnapshot` so the agent's next action is
    /// recover-and-merge (or recover-and-restore), NOT discovery.
    pub has_pinned_snapshot: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals() -> LifecycleSignals {
        LifecycleSignals::default()
    }

    #[test]
    fn missing_beats_every_other_signal() {
        let mut s = signals();
        s.missing = true;
        s.rebase_conflicts = 3;
        s.is_stale = true;
        s.commits_ahead = 2;
        s.has_uncommitted = true;
        assert_eq!(LifecycleState::classify(s), LifecycleState::Missing);
    }

    /// bn-29fi: a missing workspace WITH a pinned recovery snapshot
    /// classifies as `AbandonedWithSnapshot`, not `Missing`. This is
    /// the destroy-prevention cue: the agent sees the more specific
    /// name and reaches for `recover --to` instead of "discovery
    /// then guess".
    #[test]
    fn abandoned_with_snapshot_beats_plain_missing() {
        let mut s = signals();
        s.missing = true;
        s.has_pinned_snapshot = true;
        assert_eq!(
            LifecycleState::classify(s),
            LifecycleState::AbandonedWithSnapshot
        );
    }

    /// bn-29fi: classifier never elevates a present workspace to
    /// `AbandonedWithSnapshot` — the variant is reserved for the
    /// destroyed-but-snapshot-pinned case.
    #[test]
    fn abandoned_with_snapshot_requires_missing() {
        let mut s = signals();
        s.has_pinned_snapshot = true; // present + pinned snapshot
        assert_eq!(LifecycleState::classify(s), LifecycleState::Clean);
    }

    /// bn-29fi: `AbandonedWithSnapshot`'s fix command names the
    /// recover-into-new-workspace path because that's the only
    /// destroy-prevention-correct action (the original workspace dir
    /// is gone, so a same-name recover is itself a creation).
    #[test]
    fn abandoned_with_snapshot_fix_command_is_recover_to() {
        let cmd = LifecycleState::AbandonedWithSnapshot
            .fix_command("alice", false)
            .expect("abandoned-with-snapshot has a fix");
        assert!(cmd.contains("maw ws recover alice"));
        assert!(
            cmd.contains("--to"),
            "fix should name the recover-to path: {cmd}"
        );
    }

    #[test]
    fn conflicted_beats_stale_and_dirty() {
        let mut s = signals();
        s.rebase_conflicts = 1;
        s.is_stale = true;
        s.has_uncommitted = true;
        s.commits_ahead = 4;
        assert_eq!(LifecycleState::classify(s), LifecycleState::Conflicted);
    }

    #[test]
    fn stale_beats_committed_and_dirty() {
        let mut s = signals();
        s.is_stale = true;
        s.commits_ahead = 2;
        s.has_uncommitted = true;
        assert_eq!(LifecycleState::classify(s), LifecycleState::Stale);
    }

    #[test]
    fn committed_beats_dirty() {
        let mut s = signals();
        s.commits_ahead = 1;
        s.has_uncommitted = true;
        assert_eq!(
            LifecycleState::classify(s),
            LifecycleState::CommittedUnintegrated
        );
    }

    #[test]
    fn dirty_beats_clean() {
        let mut s = signals();
        s.has_uncommitted = true;
        assert_eq!(
            LifecycleState::classify(s),
            LifecycleState::DirtyUncommitted
        );
    }

    #[test]
    fn integrated_only_when_explicitly_flagged() {
        let mut s = signals();
        s.was_integrated = true;
        assert_eq!(LifecycleState::classify(s), LifecycleState::Integrated);

        // commits_ahead overrides was_integrated — the workspace has
        // uninclude work *now* even if it landed something earlier.
        s.commits_ahead = 1;
        assert_eq!(
            LifecycleState::classify(s),
            LifecycleState::CommittedUnintegrated
        );
    }

    #[test]
    fn clean_is_the_fallthrough() {
        assert_eq!(LifecycleState::classify(signals()), LifecycleState::Clean);
    }

    #[test]
    fn fix_command_picks_advance_for_persistent_stale() {
        let cmd = LifecycleState::Stale
            .fix_command("agent-x", true)
            .expect("stale has fix");
        assert_eq!(cmd, "maw ws advance agent-x");
    }

    #[test]
    fn fix_command_picks_sync_for_ephemeral_stale() {
        let cmd = LifecycleState::Stale
            .fix_command("agent-x", false)
            .expect("stale has fix");
        assert_eq!(cmd, "maw ws sync agent-x");
    }

    #[test]
    fn fix_command_for_committed_points_at_merge_check() {
        let cmd = LifecycleState::CommittedUnintegrated
            .fix_command("alice", false)
            .expect("committed has fix");
        assert!(cmd.contains("maw ws merge alice"));
        assert!(cmd.contains("--check"));
    }

    #[test]
    fn clean_and_integrated_have_no_fix() {
        assert!(LifecycleState::Clean.fix_command("x", false).is_none());
        assert!(LifecycleState::Integrated.fix_command("x", false).is_none());
    }

    #[test]
    fn slugs_are_stable_and_kebab_case() {
        for state in [
            LifecycleState::Missing,
            LifecycleState::Conflicted,
            LifecycleState::Stale,
            LifecycleState::CommittedUnintegrated,
            LifecycleState::DirtyUncommitted,
            LifecycleState::Clean,
            LifecycleState::Integrated,
            LifecycleState::AbandonedWithSnapshot,
        ] {
            let slug = state.slug();
            // Stable: no internal whitespace, no underscores.
            assert!(!slug.contains(' '));
            assert!(!slug.contains('_'));
            // Round-trips through serde with the same value.
            let json = serde_json::to_string(&state).expect("serializes");
            assert_eq!(json, format!("\"{slug}\""));
        }
    }
}
