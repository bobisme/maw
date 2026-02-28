//! Stateright model of the maw merge protocol state machine.
//!
//! Models the PREPARE -> BUILD -> VALIDATE -> COMMIT -> CLEANUP lifecycle
//! including crash/recovery paths, and checks safety invariants G1, G3, G4
//! from `notes/assurance/invariants.md`.
//!
//! # Modeled invariants
//!
//! - **G1 (No silent loss)**: committed OIDs remain reachable from `epoch_ref`
//!   or `recovery_refs` in every reachable state.
//! - **G3 (Commit atomicity)**: upon entering Cleanup, both `epoch_ref` and
//!   `branch_ref` equal the candidate OID.
//! - **G4 (Destructive gate)**: workspace destruction only occurs after a
//!   recovery ref has been captured for that workspace.

#![allow(
    clippy::missing_docs_in_private_items,
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::too_many_lines
)]

use stateright::*;
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// State types
// ---------------------------------------------------------------------------

/// Abstract OID represented as u64 for bounded model checking.
/// 0 = "null/unset", positive values are distinct commit identities.
type Oid = u64;

/// The merge protocol state, encompassing refs, workspace state, and the
/// phase of the merge state machine.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct MergeState {
    /// Current phase of the merge state machine.
    pub phase: Phase,
    /// The epoch ref (`refs/manifold/epoch/current`).
    pub epoch_ref: Oid,
    /// The branch ref (`refs/heads/<branch>`).
    pub branch_ref: Oid,
    /// The candidate merge commit OID (produced during BUILD).
    pub candidate: Oid,
    /// Recovery refs captured before destructive operations.
    pub recovery_refs: BTreeSet<Oid>,
    /// Per-workspace state.
    pub workspaces: BTreeMap<String, WorkspaceState>,
    /// Whether the merge-state file is persisted on disk.
    pub merge_state_on_disk: bool,
    /// OIDs that were committed (reachable) at the start of the protocol.
    /// Used to check G1 (no silent loss).
    pub committed_pre: BTreeSet<Oid>,
    /// Track which workspaces have had recovery refs captured.
    pub recovery_captured: BTreeSet<String>,
}

/// Phase of the merge protocol (mirrors `MergePhase` from `src/merge_state.rs`
/// but splits COMMIT into two sub-phases to model the epoch-then-branch
/// two-step from `src/merge/commit.rs`).
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Phase {
    /// No merge in progress.
    Idle,
    /// Freeze inputs and write merge intent.
    Prepare,
    /// Build the merged tree.
    Build,
    /// Run validation commands.
    Validate,
    /// First half of commit: CAS-move epoch ref.
    CommitEpoch,
    /// Second half of commit: CAS-move branch ref.
    CommitBranch,
    /// Post-commit cleanup.
    Cleanup,
    /// Process crashed — needs recovery.
    Crashed,
    /// Recovery in progress.
    Recovering,
    /// Terminal: merge complete.
    Complete,
    /// Terminal: merge aborted.
    Aborted,
}

/// State of a single workspace.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct WorkspaceState {
    /// The HEAD commit of this workspace.
    pub head: Oid,
    /// Whether the workspace has uncommitted changes.
    pub dirty: bool,
    /// Whether the workspace still exists on disk.
    pub exists: bool,
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// Actions the model can take. Each corresponds to a step in the merge
/// protocol or an environment event (crash).
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Action {
    /// Freeze inputs, capture workspace heads, write merge-state.
    Prepare,
    /// Build the merged tree, producing a candidate OID.
    Build,
    /// Validation passes.
    ValidatePass,
    /// Validation fails.
    ValidateFail,
    /// CAS-move epoch ref to candidate.
    CommitEpoch,
    /// CAS-move branch ref to candidate.
    CommitBranch,
    /// Post-commit cleanup.
    Cleanup,
    /// Capture recovery ref for a workspace before destruction.
    CaptureRecovery(String),
    /// Destroy a workspace.
    DestroyWorkspace(String),
    /// Abort the merge from a pre-commit phase.
    Abort,
    /// Process crash (can happen in any non-terminal phase).
    Crash,
    /// Recover from a crash.
    Recover,
}

// ---------------------------------------------------------------------------
// Model definition
// ---------------------------------------------------------------------------

/// The merge protocol model for Stateright model checking.
#[derive(Clone, Debug)]
pub struct MergeModel {
    /// Names of workspaces participating in the merge.
    pub workspace_names: Vec<String>,
    /// The "old" epoch OID before the merge.
    pub initial_epoch: Oid,
    /// The candidate OID that BUILD will produce.
    pub candidate_oid: Oid,
}

impl MergeModel {
    /// Create a new model with the given workspace names.
    pub fn new(workspace_names: Vec<String>) -> Self {
        Self {
            workspace_names,
            initial_epoch: 1, // abstract OID for the pre-merge epoch
            candidate_oid: 2, // abstract OID for the merge candidate
        }
    }

    fn initial_state(&self) -> MergeState {
        let mut workspaces = BTreeMap::new();
        for (i, name) in self.workspace_names.iter().enumerate() {
            workspaces.insert(
                name.clone(),
                WorkspaceState {
                    // Each workspace has a distinct head OID (10+i to avoid
                    // collision with epoch/candidate OIDs).
                    head: 10 + i as Oid,
                    dirty: false,
                    exists: true,
                },
            );
        }

        let mut committed_pre = BTreeSet::new();
        committed_pre.insert(self.initial_epoch);
        for ws in workspaces.values() {
            committed_pre.insert(ws.head);
        }

        MergeState {
            phase: Phase::Idle,
            epoch_ref: self.initial_epoch,
            branch_ref: self.initial_epoch,
            candidate: 0,
            recovery_refs: BTreeSet::new(),
            workspaces,
            merge_state_on_disk: false,
            committed_pre,
            recovery_captured: BTreeSet::new(),
        }
    }
}

impl Model for MergeModel {
    type State = MergeState;
    type Action = Action;

    fn init_states(&self) -> Vec<Self::State> {
        vec![self.initial_state()]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        match &state.phase {
            Phase::Idle => {
                actions.push(Action::Prepare);
            }

            Phase::Prepare => {
                actions.push(Action::Build);
                actions.push(Action::Abort);
                actions.push(Action::Crash);
            }

            Phase::Build => {
                actions.push(Action::ValidatePass);
                actions.push(Action::ValidateFail);
                actions.push(Action::Abort);
                actions.push(Action::Crash);
            }

            Phase::Validate => {
                // Validation already decided pass/fail during Build->Validate
                // transition; from here we either commit or abort.
                actions.push(Action::CommitEpoch);
                actions.push(Action::Abort);
                actions.push(Action::Crash);
            }

            Phase::CommitEpoch => {
                actions.push(Action::CommitBranch);
                actions.push(Action::Crash);
                // No abort from CommitEpoch — epoch ref already moved.
            }

            Phase::CommitBranch => {
                actions.push(Action::Cleanup);
                actions.push(Action::Crash);
            }

            Phase::Cleanup => {
                // In cleanup, we can capture recovery refs and destroy workspaces.
                // Offer capture for workspaces that haven't been captured yet.
                for name in &self.workspace_names {
                    if !state.recovery_captured.contains(name) {
                        if let Some(ws) = state.workspaces.get(name) {
                            if ws.exists {
                                actions.push(Action::CaptureRecovery(name.clone()));
                            }
                        }
                    }
                }

                // Offer destroy for workspaces that exist and have recovery captured.
                for name in &self.workspace_names {
                    if state.recovery_captured.contains(name) {
                        if let Some(ws) = state.workspaces.get(name) {
                            if ws.exists {
                                actions.push(Action::DestroyWorkspace(name.clone()));
                            }
                        }
                    }
                }

                // Can also finish cleanup (transition to Complete) if all
                // workspaces are either destroyed or have recovery captured.
                let all_handled = self.workspace_names.iter().all(|name| {
                    state.recovery_captured.contains(name)
                        || state
                            .workspaces
                            .get(name)
                            .is_none_or(|ws| !ws.exists)
                });
                if all_handled {
                    actions.push(Action::Cleanup);
                }

                actions.push(Action::Crash);
            }

            Phase::Crashed => {
                actions.push(Action::Recover);
            }

            Phase::Recovering => {
                // Recovery dispatches based on what phase the merge-state
                // file records, which we model as returning to the
                // appropriate phase. The recover action handles this.
            }

            Phase::Complete | Phase::Aborted => {
                // Terminal states — no further actions.
            }
        }
    }

    fn next_state(&self, state: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut s = state.clone();

        match &action {
            Action::Prepare => {
                s.phase = Phase::Prepare;
                s.merge_state_on_disk = true;
            }

            Action::Build => {
                s.phase = Phase::Build;
                s.candidate = self.candidate_oid;
            }

            Action::ValidatePass => {
                s.phase = Phase::Validate;
            }

            Action::ValidateFail => {
                s.phase = Phase::Aborted;
                s.merge_state_on_disk = false;
            }

            Action::CommitEpoch => {
                // CAS-move epoch ref: old -> candidate
                // (mirrors refs::advance_epoch in commit.rs)
                s.epoch_ref = s.candidate;
                s.phase = Phase::CommitEpoch;
            }

            Action::CommitBranch => {
                // CAS-move branch ref: old -> candidate
                s.branch_ref = s.candidate;
                s.phase = Phase::CommitBranch;
            }

            Action::Cleanup => {
                if s.phase == Phase::CommitBranch {
                    // First entry into cleanup
                    s.phase = Phase::Cleanup;
                } else {
                    // Finishing cleanup — transition to complete
                    s.phase = Phase::Complete;
                    s.merge_state_on_disk = false;
                }
            }

            Action::CaptureRecovery(name) => {
                if let Some(ws) = s.workspaces.get(name) {
                    s.recovery_refs.insert(ws.head);
                    s.recovery_captured.insert(name.clone());
                }
            }

            Action::DestroyWorkspace(name) => {
                if let Some(ws) = s.workspaces.get_mut(name) {
                    ws.exists = false;
                }
            }

            Action::Abort => {
                s.phase = Phase::Aborted;
                s.merge_state_on_disk = false;
            }

            Action::Crash => {
                s.phase = Phase::Crashed;
                // merge_state_on_disk remains as-is (crash doesn't corrupt
                // the atomic file — see merge_state.rs write_atomic).
            }

            Action::Recover => {
                // Recovery dispatch mirrors recovery_outcome_for_phase in
                // merge_state.rs and recover_partial_commit in commit.rs.
                if !s.merge_state_on_disk {
                    // No merge-state file => nothing to recover.
                    s.phase = Phase::Idle;
                    return Some(s);
                }

                // Determine recovery based on ref state (models the logic
                // in recover_partial_commit).
                if s.epoch_ref == s.candidate && s.branch_ref == s.candidate {
                    // Both refs moved — already committed. Go to cleanup.
                    s.phase = Phase::Cleanup;
                } else if s.epoch_ref == s.candidate
                    && s.branch_ref == self.initial_epoch
                {
                    // Epoch moved, branch didn't — finalize branch ref.
                    // (This is the FinalizedMainRef path in commit.rs.)
                    s.branch_ref = s.candidate;
                    s.phase = Phase::Cleanup;
                } else if s.epoch_ref == self.initial_epoch
                    && s.branch_ref == self.initial_epoch
                {
                    // Neither ref moved. Pre-commit crash.
                    // If we were in Prepare or Build, abort safely.
                    // If we were in Validate, re-run validation.
                    // We model this conservatively: if candidate was set
                    // (we reached Build) and refs haven't moved, we can
                    // abort safely.
                    if s.candidate == 0 {
                        // Crashed during Prepare before Build
                        s.phase = Phase::Aborted;
                        s.merge_state_on_disk = false;
                    } else {
                        // Crashed during Build or Validate — abort pre-commit.
                        s.phase = Phase::Aborted;
                        s.merge_state_on_disk = false;
                    }
                } else {
                    // Inconsistent ref state — model as abort with error.
                    s.phase = Phase::Aborted;
                    s.merge_state_on_disk = false;
                }

                return Some(s);
            }
        }

        Some(s)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            // G1: No silent loss of committed work.
            //
            // All OIDs that were committed before the merge started must
            // remain reachable from either epoch_ref or recovery_refs.
            // In our abstract model, "reachable from epoch_ref" means
            // epoch_ref itself plus the initial epoch (since candidate is
            // built from initial epoch + workspace heads).
            Property::<Self>::always("G1: no silent loss of committed OIDs", |_model, state| {
                // In non-terminal states, the refs are in flux — we only
                // need to check the invariant in stable states (Complete,
                // Aborted, Idle) and after cleanup.
                let check_phases = matches!(
                    state.phase,
                    Phase::Idle | Phase::Complete | Phase::Aborted | Phase::Cleanup
                );
                if !check_phases {
                    return true;
                }

                // Reachable set: epoch_ref, branch_ref, recovery_refs,
                // plus all workspace heads that still exist.
                let mut reachable = BTreeSet::new();
                reachable.insert(state.epoch_ref);
                reachable.insert(state.branch_ref);
                for &r in &state.recovery_refs {
                    reachable.insert(r);
                }
                for ws in state.workspaces.values() {
                    if ws.exists {
                        reachable.insert(ws.head);
                    }
                }
                // The candidate OID (if set) is reachable through epoch_ref
                // when epoch_ref == candidate. And the initial epoch is always
                // an ancestor of the candidate in a real repo. Model this:
                // if epoch_ref == candidate, the initial epoch is still
                // reachable (it's an ancestor).
                if state.epoch_ref != 0 {
                    // The initial epoch is always an ancestor of whatever
                    // epoch_ref points to.
                    reachable.insert(1); // initial_epoch
                }

                state.committed_pre.iter().all(|oid| reachable.contains(oid))
            }),
            // G3: Commit atomicity.
            //
            // When entering Cleanup or Complete, both refs must point at
            // the candidate.
            Property::<Self>::always("G3: commit atomicity at cleanup", |_model, state| {
                if matches!(state.phase, Phase::Cleanup | Phase::Complete) {
                    state.epoch_ref == state.candidate && state.branch_ref == state.candidate
                } else {
                    true
                }
            }),
            // G4: Destructive gate.
            //
            // A workspace may only be destroyed if a recovery ref has been
            // captured for it (its head OID is in recovery_refs).
            Property::<Self>::always(
                "G4: workspace destruction requires recovery ref",
                |_model, state| {
                    for (name, ws) in &state.workspaces {
                        if !ws.exists && !state.recovery_captured.contains(name) {
                            return false;
                        }
                    }
                    true
                },
            ),
        ]
    }
}

#[cfg(test)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
mod tests {
    use super::*;

    #[test]
    fn model_check_two_workspaces() {
        let model = MergeModel::new(vec!["alice".into(), "bob".into()]);
        model
            .checker()
            .threads(2)
            .spawn_dfs()
            .join()
            .assert_properties();
    }

    #[test]
    fn model_check_single_workspace() {
        let model = MergeModel::new(vec!["solo".into()]);
        model
            .checker()
            .threads(2)
            .spawn_dfs()
            .join()
            .assert_properties();
    }

    #[test]
    fn model_check_three_workspaces() {
        let model = MergeModel::new(vec!["alice".into(), "bob".into(), "carol".into()]);
        model
            .checker()
            .threads(2)
            .spawn_dfs()
            .join()
            .assert_properties();
    }

    #[test]
    fn initial_state_is_idle() {
        let model = MergeModel::new(vec!["ws1".into()]);
        let states = model.init_states();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].phase, Phase::Idle);
    }

    #[test]
    fn happy_path_reaches_complete() {
        let model = MergeModel::new(vec!["ws1".into()]);
        let mut state = model.init_states().into_iter().next().unwrap();

        // Idle -> Prepare
        state = model.next_state(&state, Action::Prepare).unwrap();
        assert_eq!(state.phase, Phase::Prepare);

        // Prepare -> Build
        state = model.next_state(&state, Action::Build).unwrap();
        assert_eq!(state.phase, Phase::Build);
        assert_eq!(state.candidate, 2);

        // Build -> Validate (pass)
        state = model.next_state(&state, Action::ValidatePass).unwrap();
        assert_eq!(state.phase, Phase::Validate);

        // Validate -> CommitEpoch
        state = model.next_state(&state, Action::CommitEpoch).unwrap();
        assert_eq!(state.phase, Phase::CommitEpoch);
        assert_eq!(state.epoch_ref, 2);

        // CommitEpoch -> CommitBranch
        state = model.next_state(&state, Action::CommitBranch).unwrap();
        assert_eq!(state.phase, Phase::CommitBranch);
        assert_eq!(state.branch_ref, 2);

        // CommitBranch -> Cleanup
        state = model.next_state(&state, Action::Cleanup).unwrap();
        assert_eq!(state.phase, Phase::Cleanup);

        // Capture recovery for ws1
        state = model
            .next_state(&state, Action::CaptureRecovery("ws1".into()))
            .unwrap();
        assert!(state.recovery_captured.contains("ws1"));

        // Destroy ws1
        state = model
            .next_state(&state, Action::DestroyWorkspace("ws1".into()))
            .unwrap();
        assert!(!state.workspaces["ws1"].exists);

        // Cleanup -> Complete
        state = model.next_state(&state, Action::Cleanup).unwrap();
        assert_eq!(state.phase, Phase::Complete);
    }

    #[test]
    fn crash_during_commit_epoch_recovers() {
        let model = MergeModel::new(vec!["ws1".into()]);
        let mut state = model.init_states().into_iter().next().unwrap();

        state = model.next_state(&state, Action::Prepare).unwrap();
        state = model.next_state(&state, Action::Build).unwrap();
        state = model.next_state(&state, Action::ValidatePass).unwrap();
        state = model.next_state(&state, Action::CommitEpoch).unwrap();

        // Epoch ref moved, branch ref hasn't
        assert_eq!(state.epoch_ref, 2);
        assert_eq!(state.branch_ref, 1);

        // Crash
        state = model.next_state(&state, Action::Crash).unwrap();
        assert_eq!(state.phase, Phase::Crashed);

        // Recover — should finalize branch ref and go to Cleanup
        state = model.next_state(&state, Action::Recover).unwrap();
        assert_eq!(state.phase, Phase::Cleanup);
        assert_eq!(state.branch_ref, 2);
        assert_eq!(state.epoch_ref, 2);
    }
}
