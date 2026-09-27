//! Stateright model of maw's multi-process protocol (bn-3ppf).
//!
//! This replaces the original single-actor merge phase machine (which had
//! drifted from the real code: it moved the epoch and branch refs in two
//! steps, never made a workspace dirty, and hard-coded ancestry). The model
//! now tracks *content*, not opaque OIDs, so the data-loss classes that have
//! actually shipped (bn-rah2, bn-p3m9, bn-mq3b, bn-29z8, bn-38vw) are
//! expressible and each has a mutation test proving the checker catches it.
//!
//! # Abstraction
//!
//! * **Content.** A repo has [`NPATHS`] paths. A [`Tree`] maps each path to a
//!   *set of atoms* (a `u16` bitmask). An atom is one unit of work (a hunk);
//!   agents only ever *add* atoms, so any atom that disappears from a live
//!   tree was dropped by maw. A 3-way merge is the set analogue of diff3:
//!   `ours ∪ (theirs − base) − (base − theirs)` per path ([`merge3`]) — a
//!   `theirs` that lacks a base atom *deletes* it, exactly how a stale
//!   worktree committed on top of a newer HEAD silently reverts epoch hunks.
//! * **Commits** live in an append-only table (index = OID, parents always
//!   have smaller indices, so the table is topologically ordered). Each commit
//!   carries its ancestor bitset, so ancestry / merge-base are exact.
//! * **Refs:** `refs/manifold/epoch/current` ([`State::epoch`]), the target
//!   branch ([`State::branch`]), per-workspace `HEAD`, per-workspace epoch ref
//!   `refs/manifold/epoch/ws/<name>` ([`Workspace::base`]) and pinned
//!   recovery refs ([`State::recovery`]).
//! * **Journals:** `merge-state.json` ([`MergeJournal`], maw-core
//!   `MergeStateFile`) and `commit-state.json` ([`CommitJournal`],
//!   `src/merge/commit.rs` `CommitStateFile`). Crash recovery dispatches on
//!   the journal phase through the REAL production function
//!   [`maw_core::merge_state::recovery_outcome_for_phase`].
//! * **Locks:** the repo epoch flock (`crates/maw-cli/src/epoch_lock.rs`,
//!   blocking with poll) and the per-workspace rebase flock
//!   (`workspace/sync/lock.rs`, try-lock only). A crash releases every lock
//!   (kernel flock semantics).
//!
//! # Actors
//!
//! Every maw process is an explicit program counter ([`Pc`]) whose steps are
//! interleaved with every other process and with agent actions:
//!
//! * `ws merge` ([`ProcSpec::Merge`]) — epoch lock, FF-absorb reconcile
//!   (`reconcile_epoch_with_branch`: classify → replay committed-ahead
//!   siblings → `write_epoch_current` → per-FF-sibling epoch-ref write,
//!   materialize, `set_head`), PREPARE → BUILD → VALIDATE → COMMIT (phase
//!   write carrying the bn-38vw `epoch_after` in the SAME atomic journal
//!   write (bn-3w2b), commit-state write, ONE atomic 2-ref
//!   CAS via `update_refs_atomic`, or the branch-only `write_ref_cas` path for
//!   `--into <change>`), sibling auto-rebase (try-lock per sibling), CLEANUP.
//! * `ws sync` ([`ProcSpec::Sync`]) — epoch lock, then ws try-lock, refuse
//!   dirty, fast-forward checkout or replay, then epoch-ref write.
//! * `ws destroy` ([`ProcSpec::Destroy`]) — epoch lock, status, refuse or
//!   (with `--force`) capture a recovery snapshot, then remove.
//! * `maw merge promote` ([`ProcSpec::QuarantinePromote`]) — epoch lock,
//!   then ONE atomic 2-ref CAS of epoch + branch from the quarantine's
//!   `epoch_before` to its candidate (bn-3w2b); the pre-fix shape (no lock,
//!   two separate CASes) is [`Mutation::QuarantinePromoteUnlockedSplitCas`].
//! * `auto_sync_if_stale` ([`ProcSpec::AutoSync`]) — NO epoch lock; ws
//!   try-lock only around the decision, released before the checkout, with the
//!   bn-29z8 HEAD CAS + ancestor refusal before the checkout.
//! * Agents (unlocked, always interleavable subject to [`AgentPolicy`]): edit a
//!   file in a workspace, commit in a workspace, commit directly on the target
//!   branch (the FF-absorb trigger).
//! * Crash: any running process may be killed (bounded budget); a crashed
//!   merge is followed by the next invocation's recovery.
//!
//! # Properties
//!
//! See [`ProtocolModel::properties`]. The headline safety properties are
//! [`P_NO_LOST_WORK`] (G1 + G4: every atom ever created stays in a live tip,
//! a live worktree or a recovery ref), [`P_NO_SILENT_REVERT`] (no commit ever
//! drops an atom its parent had), [`P_WS_COHERENT`] (a quiescent workspace's
//! epoch ref is an ancestor of its HEAD and its worktree contains HEAD's
//! content — the precursor of the bn-p3m9/bn-mq3b revert), and
//! [`P_NO_DEADLOCK`] (the lock wait-for graph is acyclic).
//!
//! # Fidelity notes (what is abstracted away)
//!
//! * The default (target) workspace's worktree is not modelled; it is assumed
//!   clean and equal to the branch. Trunk commits advance the branch directly.
//! * One merge process per configuration (a crashed merge is recovered by
//!   the *next* invocation, modelled by [`Action::Recover`]).
//! * Multi-commit replay is collapsed into a single cherry-pick of the
//!   `merge-base(base, HEAD)..HEAD` range ([`rebase_onto`]).
//! * Status/dirty checks immediately followed by a checkout are modelled as
//!   one atomic step (the real code runs the bn-154g hash detector directly
//!   before the checkout). The FF-absorb classification → materialization
//!   window is NOT atomic in the real code and is not atomic here.
//! * Content conflicts do not exist in the set semantics; maw's
//!   conflict-as-data path commits markers and never drops content, so
//!   ignoring it is sound for the loss properties.

#![allow(
    clippy::missing_docs_in_private_items,
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::struct_excessive_bools,
    clippy::match_same_arms,
    clippy::fn_params_excessive_bools,
    // Model code: `s`/`p`/`w`/`i` are the conventional state/proc/workspace
    // names; property fns must be plain `fn` pointers for Stateright.
    clippy::many_single_char_names,
    clippy::missing_const_for_fn
)]

use maw_core::merge_state::{MergePhase, RecoveryOutcome, recovery_outcome_for_phase};
use stateright::{Model, Property};

// ---------------------------------------------------------------------------
// Content primitives
// ---------------------------------------------------------------------------

/// Number of paths in the modelled repository.
pub const NPATHS: usize = 2;

/// A tree: per path, the set of atoms (bitmask) present in that file.
pub type Tree = [u16; NPATHS];

/// Commit id: index into [`State::commits`].
pub type Oid = u8;

/// Process id: index into [`State::procs`].
pub type Pid = u8;

/// Maximum number of commits (ancestor sets are `u64` bitmasks).
const MAX_COMMITS: usize = 64;

/// Per-path 3-way set merge: `ours ∪ (theirs − base) − (base − theirs)`.
///
/// This is diff3 on atom sets: whatever `theirs` added relative to `base` is
/// added, whatever `theirs` removed relative to `base` is removed.
pub fn merge3(base: &Tree, ours: &Tree, theirs: &Tree) -> Tree {
    let mut out = [0u16; NPATHS];
    for p in 0..NPATHS {
        let added = theirs[p] & !base[p];
        let removed = base[p] & !theirs[p];
        out[p] = (ours[p] | added) & !removed;
    }
    out
}

/// Bitmask of paths whose content differs between `a` and `b`.
pub fn diff_paths(a: &Tree, b: &Tree) -> u8 {
    let mut m = 0u8;
    for p in 0..NPATHS {
        if a[p] != b[p] {
            m |= 1 << p;
        }
    }
    m
}

/// `a ⊆ b` per path.
pub fn tree_subset(a: &Tree, b: &Tree) -> bool {
    (0..NPATHS).all(|p| a[p] & !b[p] == 0)
}

/// All atoms in a tree regardless of path.
pub fn tree_atoms(t: &Tree) -> u16 {
    t.iter().fold(0, |acc, x| acc | x)
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// A commit object.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Commit {
    /// The commit's tree.
    pub tree: Tree,
    /// Up to two parents.
    pub parents: [Option<Oid>; 2],
    /// Ancestor bitset, including the commit itself.
    pub ancestors: u64,
}

/// One agent workspace (a git worktree under `.maw/workspaces/<name>`).
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Workspace {
    /// Whether the workspace still exists on disk.
    pub exists: bool,
    /// Worktree `HEAD`.
    pub head: Oid,
    /// Per-workspace epoch ref (`refs/manifold/epoch/ws/<name>`): the base
    /// every merge/destroy diff of this workspace is computed against.
    pub base: Oid,
    /// Working-tree content (committed + uncommitted).
    pub wt: Tree,
}

/// Phase recorded in `merge-state.json`.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum JPhase {
    Prepare,
    Build,
    Validate,
    Commit,
    Cleanup,
}

impl JPhase {
    /// The production enum this phase mirrors.
    pub const fn to_core(self) -> MergePhase {
        match self {
            Self::Prepare => MergePhase::Prepare,
            Self::Build => MergePhase::Build,
            Self::Validate => MergePhase::Validate,
            Self::Commit => MergePhase::Commit,
            Self::Cleanup => MergePhase::Cleanup,
        }
    }
}

/// `merge-state.json` (maw-core `MergeStateFile`).
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct MergeJournal {
    pub phase: JPhase,
    pub epoch_before: Oid,
    pub branch_before: Oid,
    /// `epoch_candidate`, written in BUILD.
    pub candidate: Option<Oid>,
    /// `epoch_after`, written in COMMIT (bn-38vw: before the CAS).
    pub epoch_after: Option<Oid>,
    /// `--into <change>`: branch-only commit, epoch untouched.
    pub into_branch_only: bool,
}

/// `commit-state.json` (`src/merge/commit.rs` `CommitStateFile`).
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct CommitJournal {
    pub committed: bool,
    pub candidate: Oid,
}

/// FF-absorb per-sibling plan (`SiblingPlan` in `reconcile_epoch_with_branch`).
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum Plan {
    /// Not touched by the absorb (already at the target, or nonexistent).
    Untouched,
    /// Committed-ahead, clean: replay onto the absorbed tip.
    Replay,
    /// HEAD at base: materialize, move HEAD, advance the epoch ref. Carries
    /// the dirty-path mask and HEAD observed at classification time (the
    /// bn-302v re-check and HEAD CAS compare against `head`; only the legacy
    /// [`Mutation::FfNoSiblingLockRecheck`] writes with `dirty`).
    FastForward { dirty: u8, head: Oid },
    /// bn-mq3b: dirty in a path that is stale against the target — leave it.
    SkipStaleDirty,
}

/// Program counter of a process. Merge, sync, destroy and auto-sync steps
/// share one enum so the state stays a flat, hashable struct.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum Pc {
    /// Not yet started (blocked on its first lock, if it takes one).
    Start,
    /// Finished (success, refusal, or abort).
    Done,
    /// Killed; locks released, in-memory state lost.
    Crashed,

    // --- ws merge ---------------------------------------------------------
    /// FF-absorb classification (atomic: reads refs + worktree status).
    MReconcile,
    /// Replay committed-ahead sibling `i` (guarded rebase, ws try-lock).
    MReplay(u8),
    /// `write_epoch_current(branch)` — plain write under the epoch lock.
    MWriteEpoch,
    /// FF sibling `i`: write its per-workspace epoch ref (bn-302v: LAST,
    /// then release the sibling lock).
    MFfRef(u8),
    /// FF sibling `i`: try-lock the sibling (skip if held), re-check HEAD and
    /// the fresh dirty set, then materialize the delta paths (bn-302v).
    MFfMat(u8),
    /// FF sibling `i`: `set_head_detached_cas(classified HEAD -> branch)`.
    MFfHead(u8),
    /// PREPARE: stale-source check + write merge-state + freeze inputs.
    MPrepare,
    /// BUILD: candidate commit.
    MBuild,
    /// VALIDATE.
    MValidate,
    /// `enter_commit_phase`: phase = Commit AND `epoch_after` in one atomic
    /// journal write (bn-3w2b).
    MCommitPhase,
    /// MUTATION ONLY ([`Mutation::SplitCommitJournal`]): the pre-bn-3w2b
    /// separate `record_epoch_after` write (still before the CAS).
    MEpochAfter,
    /// Branch pre-flight + write commit-state.json (`Commit`).
    MCommitState,
    /// The ref CAS.
    MCas,
    /// MUTATION ONLY ([`Mutation::SplitCommitCas`]): second half of a split CAS.
    MCasBranch,
    /// commit-state.json (`Committed`).
    MCommitDone,
    /// MUTATION ONLY ([`Mutation::EpochAfterAfterCas`]).
    MLateEpochAfter,
    /// Sibling auto-rebase of workspace `i`.
    MAutoRebase(u8),
    /// `advance_merge_state(Cleanup)`.
    MCleanup,
    /// Remove merge-state, release the epoch lock.
    MFinish,

    // --- ws sync ------------------------------------------------------------
    /// MUTATION ONLY ([`Mutation::ReversedLockOrder`]): holding the ws lock,
    /// wait for the epoch lock.
    SEpochLock,
    /// Decide + checkout/replay (holding epoch lock, takes ws lock).
    SDecide,
    /// Write the per-workspace epoch ref, release locks.
    SEpochRef,

    // --- ws destroy ---------------------------------------------------------
    /// Status + refuse / capture.
    DStatus,
    /// Remove the worktree.
    DRemove,

    // --- auto_sync_if_stale -------------------------------------------------
    /// Try-lock, ahead/dirty checks, capture expected HEAD, release lock.
    AGate,
    /// Dirty re-check + HEAD CAS + ancestor refusal + checkout.
    ASync,
    /// Write the per-workspace epoch ref.
    AEpochRef,

    // --- doctor --repair --------------------------------------------------
    /// `classify_drift` + FF safety predicate, under the epoch lock (bn-32g8);
    /// records the classified epoch and branch OIDs.
    XClassify,
    /// `advance_epoch` CAS from the classified epoch to the classified branch
    /// (a no-op on mismatch). Under [`Mutation::DoctorRepairUnlocked`]: the
    /// pre-bn-32g8 plain `write_epoch_current(branch_reread)`.
    XWrite,

    // --- maw merge promote --------------------------------------------------
    /// Re-validate (always green here) + the ref CAS. Faithful: one atomic
    /// 2-ref CAS under the epoch lock. Mutation: the epoch CAS only.
    QCas,
    /// MUTATION ONLY ([`Mutation::QuarantinePromoteUnlockedSplitCas`]): the
    /// separate branch CAS.
    QCasBranch,
    /// `abandon_quarantine` after a successful promote.
    QCleanup,
}

/// Process runtime state (program counter + locals).
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Proc {
    pub pc: Pc,
    /// FF-absorb plans per workspace (merge only).
    pub plans: Vec<Plan>,
    /// FF-absorb target (= branch observed at merge start) / epoch read by
    /// sync & auto-sync.
    pub target: Oid,
    /// Global FF path set `epoch..branch` (merge only).
    pub ff_paths: u8,
    /// Frozen source worktree / base / head at PREPARE (merge only).
    pub frozen_wt: Tree,
    pub frozen_base: Oid,
    pub frozen_head: Oid,
    /// Epoch/branch before the COMMIT (merge only). Doctor: the epoch its
    /// classification judged (the CAS expected value).
    pub epoch_before: Oid,
    pub branch_before: Oid,
    /// Build candidate (merge only).
    pub candidate: Oid,
    /// Expected HEAD captured under the ws lock (auto-sync only).
    pub head_read: Oid,
}

impl Proc {
    fn new() -> Self {
        Self {
            pc: Pc::Start,
            plans: Vec::new(),
            target: 0,
            ff_paths: 0,
            frozen_wt: [0; NPATHS],
            frozen_base: 0,
            frozen_head: 0,
            epoch_before: 0,
            branch_before: 0,
            candidate: 0,
            head_read: 0,
        }
    }
}

/// Ghost events used by `sometimes` (non-vacuity) properties.
pub mod ev {
    pub const MERGE_COMMITTED: u16 = 1 << 0;
    pub const FF_ABSORBED: u16 = 1 << 1;
    pub const SIBLING_FF: u16 = 1 << 2;
    pub const SIBLING_REPLAYED: u16 = 1 << 3;
    pub const SKIP_STALE_DIRTY: u16 = 1 << 4;
    pub const AUTO_REBASED: u16 = 1 << 5;
    pub const SYNCED: u16 = 1 << 6;
    pub const DESTROYED: u16 = 1 << 7;
    pub const AUTO_SYNCED: u16 = 1 << 8;
    pub const RECOVERED_POST_CAS: u16 = 1 << 9;
    pub const RECOVERED_PRE_CAS: u16 = 1 << 10;
    pub const AGENT_COMMIT: u16 = 1 << 11;
    pub const DESTROYED_DIRTY: u16 = 1 << 12;
    pub const DOCTOR_ADVANCED: u16 = 1 << 13;
    pub const QUARANTINE_PROMOTED: u16 = 1 << 14;
}

/// The full model state.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct State {
    pub commits: Vec<Commit>,
    pub epoch: Oid,
    pub branch: Oid,
    pub ws: Vec<Workspace>,
    /// Pinned recovery refs (sorted, deduplicated).
    pub recovery: Vec<Oid>,
    pub epoch_lock: Option<Pid>,
    pub ws_lock: Vec<Option<Pid>>,
    pub merge_state: Option<MergeJournal>,
    pub commit_state: Option<CommitJournal>,
    pub procs: Vec<Proc>,
    /// Ghost: every atom ever created.
    pub created: u16,
    pub next_atom: u8,
    pub edits_left: u8,
    pub commits_left: u8,
    pub trunk_left: u8,
    pub crashes_left: u8,
    /// Ghost: a crashed merge left a journal recovery could not clear.
    pub stuck: bool,
    /// Ghost: bitset of [`ev`] events that happened.
    pub events: u16,
    /// Ghost: every value `refs/manifold/epoch/current` has ever held.
    pub epochs_seen: u64,
    /// A live quarantine workspace's candidate commit (`maw merge promote`
    /// configurations only). Its worktree keeps the candidate reachable until
    /// a successful promote abandons it.
    pub quarantine: Option<Oid>,
}

impl State {
    fn tree(&self, c: Oid) -> &Tree {
        &self.commits[c as usize].tree
    }

    fn is_ancestor_or_eq(&self, a: Oid, b: Oid) -> bool {
        self.commits[b as usize].ancestors & (1u64 << a) != 0
    }

    fn is_strict_ancestor(&self, a: Oid, b: Oid) -> bool {
        a != b && self.is_ancestor_or_eq(a, b)
    }

    /// A best common ancestor (max index in the common-ancestor set is never
    /// an ancestor of another common ancestor, since parents have smaller
    /// indices).
    fn merge_base(&self, a: Oid, b: Oid) -> Oid {
        let common = self.commits[a as usize].ancestors & self.commits[b as usize].ancestors;
        debug_assert!(common != 0, "all commits descend from the root");
        common.ilog2() as Oid
    }

    /// Create (or reuse an identical) commit.
    fn mk_commit(&mut self, tree: Tree, p1: Oid, p2: Option<Oid>) -> Oid {
        let p2 = p2.filter(|&x| x != p1);
        let parents = [Some(p1), p2];
        if let Some(i) = self
            .commits
            .iter()
            .position(|c| c.tree == tree && c.parents == parents)
        {
            return i as Oid;
        }
        let id = self.commits.len();
        assert!(id < MAX_COMMITS, "model commit table overflow");
        let mut anc = self.commits[p1 as usize].ancestors | (1u64 << id);
        if let Some(p) = p2 {
            anc |= self.commits[p as usize].ancestors;
        }
        self.commits.push(Commit {
            tree,
            parents,
            ancestors: anc,
        });
        id as Oid
    }

    fn dirty_mask(&self, w: usize) -> u8 {
        diff_paths(self.tree(self.ws[w].head), &self.ws[w].wt)
    }

    /// Guarded rebase: cherry-pick `merge_base(base, head)..head` onto `onto`.
    fn rebase_onto(&mut self, w: usize, onto: Oid) -> Oid {
        let head = self.ws[w].head;
        let fork = self.merge_base(self.ws[w].base, head);
        if self.is_ancestor_or_eq(head, onto) {
            return onto;
        }
        let tree = merge3(self.tree(fork), self.tree(onto), self.tree(head));
        self.mk_commit(tree, onto, None)
    }

    fn release_all(&mut self, pid: Pid) {
        if self.epoch_lock == Some(pid) {
            self.epoch_lock = None;
        }
        for l in &mut self.ws_lock {
            if *l == Some(pid) {
                *l = None;
            }
        }
    }

    fn release_ws(&mut self, w: usize, pid: Pid) {
        if self.ws_lock[w] == Some(pid) {
            self.ws_lock[w] = None;
        }
    }

    fn finish(&mut self, pid: Pid) {
        self.release_all(pid);
        self.procs[pid as usize].pc = Pc::Done;
    }

    fn ws_lock_free_for(&self, w: usize, pid: Pid) -> bool {
        self.ws_lock[w].is_none_or(|h| h == pid)
    }
}

/// `rebase_onto` as a free function (documentation anchor).
pub fn rebase_onto(state: &mut State, w: usize, onto: Oid) -> Oid {
    state.rebase_onto(w, onto)
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// A maw process in the configuration.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum ProcSpec {
    /// `maw ws merge <src>` (`into_branch_only` = `--into <change>`).
    Merge {
        src: u8,
        into_branch_only: bool,
        auto_rebase: bool,
    },
    /// `maw ws sync <ws>`.
    Sync { ws: u8 },
    /// `maw ws destroy <ws> [--force]`.
    Destroy { ws: u8, force: bool },
    /// `auto_sync_if_stale(<ws>)` (runs before `maw exec <ws>`).
    AutoSync { ws: u8 },
    /// `maw doctor --repair` → `epoch_drift::auto_advance_if_safe`: take the
    /// epoch lock, classify FF-absorbable drift, CAS the epoch from the
    /// classified epoch to the classified branch tip (bn-32g8). The pre-fix
    /// shape (no lock, plain write) is [`Mutation::DoctorRepairUnlocked`].
    DoctorRepair,
    /// `maw merge promote <id>` of a quarantine created (at init) from a
    /// candidate built on the initial epoch. Faithful = bn-3w2b (epoch lock +
    /// one atomic epoch+branch CAS).
    QuarantinePromote,
}

/// Initial repository shape.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum InitShape {
    /// One empty root commit; epoch = branch = root; every workspace clean at
    /// the root.
    Fresh,
    /// Root `c0`, one epoch `c1` (adds atom 0 on path 0). Epoch = branch =
    /// `c1`. Workspace 0 is current (at `c1`); every other workspace is one
    /// epoch behind (at `c0`) — the "more than one epoch behind after the next
    /// trunk commit" shape behind bn-p3m9 / bn-mq3b.
    SiblingBehind,
}

/// When agents may act on a workspace.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct AgentPolicy {
    /// Agents may edit files while a maw process is mid-way through writing
    /// that workspace (between a decision and the write it guards).
    pub edit_anytime: bool,
    /// Agents may `git commit` while a maw process is mid-way through writing
    /// that workspace.
    pub commit_anytime: bool,
}

impl AgentPolicy {
    /// Agents never race a maw write to *their own* workspace (they may still
    /// act concurrently with every other step and every other workspace).
    pub const QUIESCENT: Self = Self {
        edit_anytime: false,
        commit_anytime: false,
    };
    /// Agents act at any time.
    pub const ANYTIME: Self = Self {
        edit_anytime: true,
        commit_anytime: true,
    };
}

/// Deliberate model breakages used to prove each property is non-vacuous.
/// Each mirrors a real historical bug or a protocol decision.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum Mutation {
    /// Faithful model.
    None,
    /// Destroy `--force` removes the workspace without capturing a snapshot
    /// (G4).
    DestroyWithoutCapture,
    /// bn-mq3b regression: no per-sibling stale-dirty check; a dirty sibling
    /// whose dirty path changed in an earlier epoch is fast-forwarded.
    NoStaleDirtyGuard,
    /// bn-rah2 regression: committed-ahead sibling raw-moved to the absorbed
    /// tip instead of replayed.
    RawMoveInsteadOfReplay,
    /// bn-p3m9 regression: materialize only the global `epoch..branch` paths.
    GlobalFfPathsOnly,
    /// Epoch ref and branch ref moved by two separate CAS steps (the model's
    /// old shape; real code uses one atomic 2-ref transaction).
    SplitCommitCas,
    /// Pre-bn-38vw: `epoch_after` journaled only AFTER the CAS.
    EpochAfterAfterCas,
    /// Lock-order violation: `ws sync` takes the ws lock (blocking) before
    /// the epoch lock, and merge waits (blocking) for sibling ws locks.
    ReversedLockOrder,
    /// Pre-bn-29z8: auto-sync without the HEAD CAS / ancestor refusal.
    AutoSyncNoHeadCas,
    /// `ws sync` without the dirty-worktree refusal.
    SyncIgnoresDirty,
    /// Pre-bn-32g8 `doctor --repair`: no epoch lock, re-read the branch after
    /// classification, plain (non-CAS) `write_epoch_current`.
    DoctorRepairUnlocked,
    /// Pre-bn-3w2b COMMIT journal: `advance_merge_state(Commit)` and
    /// `record_epoch_after` as two separate journal writes.
    SplitCommitJournal,
    /// Pre-bn-3w2b `maw merge promote`: no epoch lock, epoch CAS then a
    /// separate branch CAS.
    QuarantinePromoteUnlockedSplitCas,
    /// Pre-bn-302v FF-absorb sibling order: the per-workspace epoch ref is
    /// written FIRST, then materialize, then `set_head`.
    FfRefBeforeHead,
    /// Pre-bn-302v FF-absorb sibling write: no sibling lock, no re-check,
    /// the classification-time dirty mask, unconditional `set_head`.
    FfNoSiblingLockRecheck,
    /// Pre-bn-302v `doctor --repair`: advances the epoch even while an
    /// unfinished (crashed) `ws merge` journal exists.
    DoctorIgnoresMergeJournal,
    /// Pre-bn-302v `ws merge` FF-absorb: absorbs trunk commits into the epoch
    /// even while a crashed merge's COMMIT/CLEANUP journal exists.
    FfAbsorbIgnoresMergeJournal,
}

/// The protocol model.
#[derive(Clone, Debug)]
pub struct ProtocolModel {
    pub n_ws: u8,
    pub init: InitShape,
    pub procs: Vec<ProcSpec>,
    pub agent_edits: u8,
    pub agent_commits: u8,
    pub trunk_commits: u8,
    pub crashes: u8,
    /// Allow crashes inside the FF-absorb sibling loop.
    pub crash_in_ff_absorb: bool,
    /// VALIDATE may fail.
    pub validate_can_fail: bool,
    pub agents: AgentPolicy,
    pub mutation: Mutation,
    /// Also check Oracle B's strict journal shape ([`P_ORACLE_B_JOURNAL`]).
    pub strict_oracle_b: bool,
    /// Events that must be reachable (`sometimes` properties).
    pub expect: u16,
}

impl ProtocolModel {
    /// A small default configuration: 2 workspaces, fresh repo, no processes.
    pub const fn new(n_ws: u8) -> Self {
        Self {
            n_ws,
            init: InitShape::Fresh,
            procs: Vec::new(),
            agent_edits: 1,
            agent_commits: 1,
            trunk_commits: 0,
            crashes: 0,
            crash_in_ff_absorb: false,
            validate_can_fail: false,
            agents: AgentPolicy::QUIESCENT,
            mutation: Mutation::None,
            strict_oracle_b: false,
            expect: 0,
        }
    }

    fn initial_state(&self) -> State {
        let n = self.n_ws as usize;
        let root = Commit {
            tree: [0; NPATHS],
            parents: [None, None],
            ancestors: 1,
        };
        let mut s = State {
            commits: vec![root],
            epoch: 0,
            branch: 0,
            ws: vec![
                Workspace {
                    exists: true,
                    head: 0,
                    base: 0,
                    wt: [0; NPATHS],
                };
                n
            ],
            recovery: Vec::new(),
            epoch_lock: None,
            ws_lock: vec![None; n],
            merge_state: None,
            commit_state: None,
            procs: self.procs.iter().map(|_| Proc::new()).collect(),
            created: 0,
            next_atom: 0,
            edits_left: self.agent_edits,
            commits_left: self.agent_commits,
            trunk_left: self.trunk_commits,
            crashes_left: self.crashes,
            stuck: false,
            events: 0,
            epochs_seen: 0,
            quarantine: None,
        };
        if self.init == InitShape::SiblingBehind {
            let mut t = [0; NPATHS];
            t[0] = 1;
            s.created = 1;
            s.next_atom = 1;
            let c1 = s.mk_commit(t, 0, None);
            s.epoch = c1;
            s.branch = c1;
            s.epochs_seen = 0;
            s.ws[0] = Workspace {
                exists: true,
                head: c1,
                base: c1,
                wt: t,
            };
        }
        // A quarantine left by an earlier `ws merge` whose validation failed:
        // candidate = initial epoch + one new atom on the last path.
        if let Some(pid) = self
            .procs
            .iter()
            .position(|p| *p == ProcSpec::QuarantinePromote)
        {
            let bit = 1u16 << s.next_atom;
            s.next_atom += 1;
            s.created |= bit;
            let mut t = *s.tree(s.epoch);
            t[NPATHS - 1] |= bit;
            let e = s.epoch;
            let q = s.mk_commit(t, e, None);
            s.quarantine = Some(q);
            s.procs[pid].epoch_before = e;
            s.procs[pid].candidate = q;
        }
        s
    }

    // -- agent policy --------------------------------------------------------

    /// Is some running process mid-way through writing workspace `w`?
    fn touching(&self, s: &State, w: usize) -> bool {
        s.procs
            .iter()
            .zip(&self.procs)
            .any(|(p, spec)| match (spec, p.pc) {
                (
                    ProcSpec::Merge { .. },
                    Pc::MReplay(_)
                    | Pc::MWriteEpoch
                    | Pc::MFfRef(_)
                    | Pc::MFfMat(_)
                    | Pc::MFfHead(_),
                ) => matches!(
                    p.plans.get(w),
                    Some(Plan::Replay | Plan::FastForward { .. })
                ),
                (ProcSpec::Sync { ws }, Pc::SEpochRef) => *ws as usize == w,
                (ProcSpec::Destroy { ws, .. }, Pc::DRemove) => *ws as usize == w,
                (ProcSpec::AutoSync { ws }, Pc::ASync | Pc::AEpochRef) => *ws as usize == w,
                _ => false,
            })
    }

    // -- process steps -------------------------------------------------------

    fn step(&self, s: &mut State, pid: Pid) -> bool {
        match self.procs[pid as usize] {
            ProcSpec::Merge {
                src,
                into_branch_only,
                auto_rebase,
            } => self.step_merge(s, pid, src as usize, into_branch_only, auto_rebase),
            ProcSpec::Sync { ws } => self.step_sync(s, pid, ws as usize),
            ProcSpec::Destroy { ws, force } => self.step_destroy(s, pid, ws as usize, force),
            ProcSpec::AutoSync { ws } => self.step_autosync(s, pid, ws as usize),
            ProcSpec::DoctorRepair => self.step_doctor(s, pid),
            ProcSpec::QuarantinePromote => self.step_promote(s, pid),
        }
    }

    fn next_plan_idx(p: &Proc, from: usize, want_replay: bool) -> Option<u8> {
        (from..p.plans.len())
            .find(|&i| {
                if want_replay {
                    p.plans[i] == Plan::Replay
                } else {
                    matches!(p.plans[i], Plan::FastForward { .. })
                }
            })
            .map(|i| i as u8)
    }

    /// First step of the FF advance of the next `FastForward` sibling at or
    /// after `from` (or PREPARE when there is none).
    fn next_ff(&self, p: &Proc, from: usize) -> Pc {
        Self::next_plan_idx(p, from, false).map_or(Pc::MPrepare, |i| {
            if self.mutation == Mutation::FfRefBeforeHead {
                Pc::MFfRef(i)
            } else {
                Pc::MFfMat(i)
            }
        })
    }

    /// bn-302v: leave FF sibling `w` stale (release its lock if held) and
    /// move on to the next one.
    fn skip_ff(&self, s: &mut State, pid: Pid, w: usize) -> Pc {
        s.release_ws(w, pid);
        self.next_ff(&s.procs[pid as usize], w + 1)
    }

    fn next_auto_rebase(s: &State, from: usize, src: usize) -> Pc {
        (from..s.ws.len())
            .find(|&i| i != src && s.ws[i].exists)
            .map_or(Pc::MCleanup, |i| Pc::MAutoRebase(i as u8))
    }

    fn merge_abort(s: &mut State, pid: Pid) {
        s.merge_state = None;
        s.finish(pid);
    }

    fn step_merge(
        &self,
        s: &mut State,
        pid: Pid,
        src: usize,
        into_branch_only: bool,
        auto_rebase: bool,
    ) -> bool {
        let i = pid as usize;
        let pc = s.procs[i].pc;
        match pc {
            Pc::Start => {
                if s.epoch_lock.is_some() {
                    return false; // blocked on the epoch flock
                }
                s.epoch_lock = Some(pid);
                s.procs[i].pc = Pc::MReconcile;
            }
            Pc::MReconcile => {
                // branch_before_oid is read at merge start; FF-absorb targets it.
                let branch = s.branch;
                s.procs[i].branch_before = branch;
                s.procs[i].target = branch;
                if into_branch_only || s.epoch == branch {
                    s.procs[i].pc = Pc::MPrepare;
                    return true;
                }
                if !s.is_strict_ancestor(s.epoch, branch) {
                    s.finish(pid); // bail_diverged: fork divergence
                    return true;
                }
                // bn-302v: a crashed merge's COMMIT/CLEANUP journal pins
                // `epoch_before` for its recovery; absorbing now would strand
                // it. Refuse (PREPARE would refuse this journal anyway).
                if self.mutation != Mutation::FfAbsorbIgnoresMergeJournal
                    && s.merge_state
                        .as_ref()
                        .is_some_and(|j| matches!(j.phase, JPhase::Commit | JPhase::Cleanup))
                {
                    s.finish(pid);
                    return true;
                }
                let ff_paths = diff_paths(s.tree(s.epoch), s.tree(branch));
                // evaluate_ff_safety: any workspace touching an FF path blocks.
                for w in 0..s.ws.len() {
                    if !s.ws[w].exists {
                        continue;
                    }
                    let base_t = s.tree(s.ws[w].base);
                    let touched =
                        diff_paths(base_t, &s.ws[w].wt) | diff_paths(base_t, s.tree(s.ws[w].head));
                    if touched & ff_paths != 0 {
                        s.finish(pid);
                        return true;
                    }
                }
                // Classify every sibling (SiblingPlan).
                let mut plans = vec![Plan::Untouched; s.ws.len()];
                for (w, plan) in plans.iter_mut().enumerate() {
                    let ws = &s.ws[w];
                    if !ws.exists || ws.base == branch {
                        continue;
                    }
                    let dirty = s.dirty_mask(w);
                    if ws.head == ws.base {
                        let stale = dirty & diff_paths(s.tree(ws.head), s.tree(branch));
                        *plan = if stale != 0 && self.mutation != Mutation::NoStaleDirtyGuard {
                            Plan::SkipStaleDirty
                        } else {
                            Plan::FastForward {
                                dirty,
                                head: ws.head,
                            }
                        };
                    } else if dirty != 0 {
                        // committed-ahead + dirty: blocks the whole absorb.
                        s.finish(pid);
                        return true;
                    } else {
                        *plan = Plan::Replay;
                    }
                }
                if plans.contains(&Plan::SkipStaleDirty) {
                    s.events |= ev::SKIP_STALE_DIRTY;
                }
                let p = &mut s.procs[i];
                p.plans = plans;
                p.ff_paths = ff_paths;
                p.pc = Self::next_plan_idx(p, 0, true).map_or(Pc::MWriteEpoch, Pc::MReplay);
            }
            Pc::MReplay(w) => {
                let w = w as usize;
                if !s.ws_lock_free_for(w, pid) {
                    if self.mutation == Mutation::ReversedLockOrder {
                        return false; // blocking wait (mutation)
                    }
                    s.finish(pid); // try-lock failed: replay error aborts the absorb
                    return true;
                }
                if !s.ws[w].exists || s.dirty_mask(w) != 0 {
                    s.finish(pid); // rebase refuses a dirty worktree → absorb aborted
                    return true;
                }
                let target = s.procs[i].target;
                if self.mutation == Mutation::RawMoveInsteadOfReplay {
                    s.ws[w].head = target;
                } else {
                    s.ws[w].head = s.rebase_onto(w, target);
                }
                s.ws[w].wt = *s.tree(s.ws[w].head);
                s.ws[w].base = target;
                s.events |= ev::SIBLING_REPLAYED;
                let p = &mut s.procs[i];
                p.pc = Self::next_plan_idx(p, w + 1, true).map_or(Pc::MWriteEpoch, Pc::MReplay);
            }
            Pc::MWriteEpoch => {
                s.epoch = s.procs[i].target;
                s.events |= ev::FF_ABSORBED;
                s.procs[i].pc = self.next_ff(&s.procs[i], 0);
            }
            Pc::MFfRef(w) => {
                let wu = w as usize;
                s.ws[wu].base = s.procs[i].target;
                s.procs[i].pc = if self.mutation == Mutation::FfRefBeforeHead {
                    Pc::MFfMat(w)
                } else {
                    s.release_ws(wu, pid);
                    self.next_ff(&s.procs[i], wu + 1)
                };
            }
            Pc::MFfMat(w) => {
                let w = w as usize;
                let target = s.procs[i].target;
                let Plan::FastForward {
                    dirty: dirty_classified,
                    head: head_classified,
                } = s.procs[i].plans[w]
                else {
                    unreachable!("MFfMat only for FastForward plans")
                };
                let own = diff_paths(s.tree(s.ws[w].head), s.tree(target));
                let ff_paths = s.procs[i].ff_paths;
                let dirty = if self.mutation == Mutation::FfNoSiblingLockRecheck {
                    dirty_classified
                } else {
                    // bn-302v: try-lock the sibling (a held lock means another
                    // maw process is rewriting it: skip, leave it stale), then
                    // re-run the classifier on fresh facts.
                    if !s.ws_lock_free_for(w, pid) {
                        s.procs[i].pc = self.skip_ff(s, pid, w);
                        return true;
                    }
                    s.ws_lock[w] = Some(pid);
                    let dirty = s.dirty_mask(w);
                    // The stale-dirty part is the same production
                    // `classify_sibling` guard as at classification, so
                    // `NoStaleDirtyGuard` removes it here too.
                    let stale = dirty & (own | ff_paths) != 0
                        && self.mutation != Mutation::NoStaleDirtyGuard;
                    if !s.ws[w].exists || s.ws[w].head != head_classified || stale {
                        s.procs[i].pc = self.skip_ff(s, pid, w);
                        return true;
                    }
                    dirty
                };
                // sync_ff_paths_in_worktree: ff_paths ∪ (own HEAD→target delta
                // minus the dirty paths).
                let to_apply = if self.mutation == Mutation::GlobalFfPathsOnly {
                    ff_paths
                } else {
                    ff_paths | (own & !dirty)
                };
                let tt = *s.tree(target);
                for (p, t) in tt.iter().enumerate() {
                    if to_apply & (1 << p) != 0 {
                        s.ws[w].wt[p] = *t;
                    }
                }
                s.procs[i].pc = Pc::MFfHead(w as u8);
            }
            Pc::MFfHead(w) => {
                let wu = w as usize;
                let Plan::FastForward { head, .. } = s.procs[i].plans[wu] else {
                    unreachable!("MFfHead only for FastForward plans")
                };
                if self.mutation != Mutation::FfNoSiblingLockRecheck && s.ws[wu].head != head {
                    // bn-302v HEAD CAS failed (a commit landed after the
                    // re-check): HEAD and the epoch ref stay put.
                    s.procs[i].pc = self.skip_ff(s, pid, wu);
                    return true;
                }
                s.ws[wu].head = s.procs[i].target;
                s.events |= ev::SIBLING_FF;
                s.procs[i].pc = if self.mutation == Mutation::FfRefBeforeHead {
                    s.release_ws(wu, pid);
                    self.next_ff(&s.procs[i], wu + 1)
                } else {
                    Pc::MFfRef(w)
                };
            }
            Pc::MPrepare => {
                // stale_merge_sources + run_prepare_phase.
                if !s.ws[src].exists || s.ws[src].base != s.epoch || s.merge_state.is_some() {
                    s.finish(pid);
                    return true;
                }
                let epoch_before = s.epoch;
                let branch_before = s.procs[i].branch_before;
                s.merge_state = Some(MergeJournal {
                    phase: JPhase::Prepare,
                    epoch_before,
                    branch_before,
                    candidate: None,
                    epoch_after: None,
                    into_branch_only,
                });
                let wt = s.ws[src].wt;
                let (b, h) = (s.ws[src].base, s.ws[src].head);
                let p = &mut s.procs[i];
                p.epoch_before = epoch_before;
                p.frozen_wt = wt;
                p.frozen_base = b;
                p.frozen_head = h;
                p.pc = Pc::MBuild;
            }
            Pc::MBuild => {
                let p = s.procs[i].clone();
                let ours = if into_branch_only {
                    p.branch_before
                } else {
                    p.epoch_before
                };
                let tree = merge3(s.tree(p.frozen_base), s.tree(ours), &p.frozen_wt);
                let cand = s.mk_commit(tree, ours, Some(p.frozen_head));
                if let Some(j) = s.merge_state.as_mut() {
                    j.phase = JPhase::Build;
                    j.candidate = Some(cand);
                }
                s.procs[i].candidate = cand;
                s.procs[i].pc = Pc::MValidate;
            }
            Pc::MValidate => {
                if let Some(j) = s.merge_state.as_mut() {
                    j.phase = JPhase::Validate;
                }
                s.procs[i].pc = Pc::MCommitPhase;
            }
            Pc::MCommitPhase => {
                // bn-3w2b `enter_commit_phase`: phase + epoch_after in ONE
                // atomic journal write. The two mutations reproduce the
                // historical shapes (pre-bn-38vw: epoch_after after the CAS;
                // pre-bn-3w2b: a second write before the CAS).
                let together = !matches!(
                    self.mutation,
                    Mutation::EpochAfterAfterCas | Mutation::SplitCommitJournal
                );
                let cand = s.procs[i].candidate;
                if let Some(j) = s.merge_state.as_mut() {
                    j.phase = JPhase::Commit;
                    if together {
                        j.epoch_after = Some(cand);
                    }
                }
                s.procs[i].pc = if self.mutation == Mutation::SplitCommitJournal {
                    Pc::MEpochAfter
                } else {
                    Pc::MCommitState
                };
            }
            Pc::MEpochAfter | Pc::MLateEpochAfter => {
                let cand = s.procs[i].candidate;
                if let Some(j) = s.merge_state.as_mut() {
                    j.epoch_after = Some(cand);
                }
                s.procs[i].pc = if pc == Pc::MEpochAfter {
                    Pc::MCommitState
                } else if auto_rebase && !into_branch_only {
                    Self::next_auto_rebase(s, 0, src)
                } else {
                    Pc::MCleanup
                };
            }
            Pc::MCommitState => {
                // Pre-flight: branch moved since merge start → abort cleanly.
                if s.branch != s.procs[i].branch_before {
                    Self::merge_abort(s, pid);
                    return true;
                }
                if !into_branch_only {
                    s.commit_state = Some(CommitJournal {
                        committed: false,
                        candidate: s.procs[i].candidate,
                    });
                }
                s.procs[i].pc = Pc::MCas;
            }
            Pc::MCas => {
                let p = s.procs[i].clone();
                if into_branch_only {
                    // write_ref_cas(branch, branch_before → candidate)
                    if s.branch != p.branch_before {
                        Self::merge_abort(s, pid);
                        return true;
                    }
                    s.branch = p.candidate;
                } else if self.mutation == Mutation::SplitCommitCas {
                    if s.epoch != p.epoch_before {
                        Self::merge_abort(s, pid);
                        return true;
                    }
                    s.epoch = p.candidate;
                    s.procs[i].pc = Pc::MCasBranch;
                    return true;
                } else {
                    // refs::update_refs_atomic([(epoch, before, cand), (branch, before, cand)])
                    if s.epoch != p.epoch_before || s.branch != p.branch_before {
                        Self::merge_abort(s, pid);
                        return true;
                    }
                    s.epoch = p.candidate;
                    s.branch = p.candidate;
                }
                s.events |= ev::MERGE_COMMITTED;
                s.procs[i].pc = Pc::MCommitDone;
            }
            Pc::MCasBranch => {
                let p = s.procs[i].clone();
                if s.branch == p.branch_before {
                    s.branch = p.candidate;
                }
                s.events |= ev::MERGE_COMMITTED;
                s.procs[i].pc = Pc::MCommitDone;
            }
            Pc::MCommitDone => {
                if let Some(c) = s.commit_state.as_mut() {
                    c.committed = true;
                }
                s.procs[i].pc = if self.mutation == Mutation::EpochAfterAfterCas {
                    Pc::MLateEpochAfter
                } else if auto_rebase && !into_branch_only {
                    Self::next_auto_rebase(s, 0, src)
                } else {
                    Pc::MCleanup
                };
            }
            Pc::MAutoRebase(w) => {
                let w = w as usize;
                let cand = s.procs[i].candidate;
                if !s.ws_lock_free_for(w, pid) {
                    if self.mutation == Mutation::ReversedLockOrder {
                        return false; // blocking wait (mutation)
                    }
                    // SkippedLocked
                } else if s.ws[w].exists && s.dirty_mask(w) == 0 && s.ws[w].base != cand {
                    // SkippedDirty handled by the guard above; otherwise
                    // fast-forward (HEAD at base) or guarded replay.
                    if s.ws[w].head == s.ws[w].base {
                        s.ws[w].head = cand;
                    } else {
                        s.ws[w].head = s.rebase_onto(w, cand);
                    }
                    s.ws[w].wt = *s.tree(s.ws[w].head);
                    s.ws[w].base = cand;
                    s.events |= ev::AUTO_REBASED;
                }
                s.procs[i].pc = Self::next_auto_rebase(s, w + 1, src);
            }
            Pc::MCleanup => {
                if let Some(j) = s.merge_state.as_mut() {
                    j.phase = JPhase::Cleanup;
                }
                s.procs[i].pc = Pc::MFinish;
            }
            Pc::MFinish => {
                s.merge_state = None;
                s.finish(pid);
            }
            _ => unreachable!("merge pc {pc:?}"),
        }
        true
    }

    fn step_sync(&self, s: &mut State, pid: Pid, w: usize) -> bool {
        let i = pid as usize;
        let reversed = self.mutation == Mutation::ReversedLockOrder;
        match s.procs[i].pc {
            Pc::Start => {
                if reversed {
                    // MUTATION: ws lock first (blocking), then the epoch lock.
                    if s.ws_lock[w].is_some() {
                        return false;
                    }
                    s.ws_lock[w] = Some(pid);
                    s.procs[i].pc = Pc::SEpochLock;
                } else {
                    if s.epoch_lock.is_some() {
                        return false;
                    }
                    s.epoch_lock = Some(pid);
                    s.procs[i].pc = Pc::SDecide;
                }
            }
            Pc::SEpochLock => {
                if s.epoch_lock.is_some() {
                    return false;
                }
                s.epoch_lock = Some(pid);
                s.procs[i].pc = Pc::SDecide;
            }
            Pc::SDecide => {
                if !s.ws_lock_free_for(w, pid) {
                    s.finish(pid); // "Another rebase is in progress"
                    return true;
                }
                let ws = &s.ws[w];
                if !ws.exists || ws.base == s.epoch {
                    s.finish(pid);
                    return true;
                }
                if s.dirty_mask(w) != 0 && self.mutation != Mutation::SyncIgnoresDirty {
                    s.finish(pid); // refuse: uncommitted changes
                    return true;
                }
                s.ws_lock[w] = Some(pid);
                let epoch = s.epoch;
                s.procs[i].target = epoch;
                if s.ws[w].head == s.ws[w].base {
                    s.ws[w].head = epoch;
                } else {
                    s.ws[w].head = s.rebase_onto(w, epoch);
                }
                s.ws[w].wt = *s.tree(s.ws[w].head);
                s.procs[i].pc = Pc::SEpochRef;
            }
            Pc::SEpochRef => {
                s.ws[w].base = s.procs[i].target;
                s.events |= ev::SYNCED;
                s.finish(pid);
            }
            pc => unreachable!("sync pc {pc:?}"),
        }
        true
    }

    fn step_destroy(&self, s: &mut State, pid: Pid, w: usize, force: bool) -> bool {
        let i = pid as usize;
        match s.procs[i].pc {
            Pc::Start => {
                if s.epoch_lock.is_some() {
                    return false;
                }
                s.epoch_lock = Some(pid);
                s.procs[i].pc = Pc::DStatus;
            }
            Pc::DStatus => {
                if !s.ws[w].exists {
                    s.finish(pid);
                    return true;
                }
                let ws = &s.ws[w];
                let touched = diff_paths(s.tree(ws.base), &ws.wt) != 0 || s.dirty_mask(w) != 0;
                if touched && !force {
                    s.finish(pid); // DestroyRefusal
                    return true;
                }
                if force && self.mutation != Mutation::DestroyWithoutCapture {
                    // capture_before_destroy: snapshot commit pinned under
                    // refs/manifold/recovery/<ws>/…
                    let (wt, head) = (s.ws[w].wt, s.ws[w].head);
                    let snap = s.mk_commit(wt, head, None);
                    if let Err(pos) = s.recovery.binary_search(&snap) {
                        s.recovery.insert(pos, snap);
                    }
                }
                if s.dirty_mask(w) != 0 {
                    s.events |= ev::DESTROYED_DIRTY;
                }
                s.procs[i].pc = Pc::DRemove;
            }
            Pc::DRemove => {
                s.ws[w].exists = false;
                s.events |= ev::DESTROYED;
                s.finish(pid);
            }
            pc => unreachable!("destroy pc {pc:?}"),
        }
        true
    }

    fn step_autosync(&self, s: &mut State, pid: Pid, w: usize) -> bool {
        let i = pid as usize;
        match s.procs[i].pc {
            Pc::Start => {
                // backend.status(): not stale → nothing to do. Reads the
                // current epoch WITHOUT the epoch lock.
                if !s.ws[w].exists || s.ws[w].base == s.epoch {
                    s.finish(pid);
                    return true;
                }
                s.procs[i].target = s.epoch;
                s.procs[i].pc = Pc::AGate;
            }
            Pc::AGate => {
                // try-lock; committed-ahead / dirty → skip; capture HEAD;
                // release the lock before the checkout.
                if s.ws_lock[w].is_some() || s.ws[w].head != s.ws[w].base || s.dirty_mask(w) != 0 {
                    s.finish(pid);
                    return true;
                }
                s.procs[i].head_read = s.ws[w].head;
                s.procs[i].pc = Pc::ASync;
            }
            Pc::ASync => {
                // sync_worktree_to_epoch_inner: dirty refusal, HEAD CAS,
                // ancestor refusal, then checkout_detach(epoch_read).
                let e = s.procs[i].target;
                if !s.ws[w].exists || s.dirty_mask(w) != 0 {
                    s.finish(pid);
                    return true;
                }
                if self.mutation != Mutation::AutoSyncNoHeadCas
                    && (s.ws[w].head != s.procs[i].head_read
                        || !s.is_ancestor_or_eq(s.ws[w].head, e))
                {
                    s.finish(pid); // SkippedHeadMoved / refusal
                    return true;
                }
                s.ws[w].head = e;
                s.ws[w].wt = *s.tree(e);
                s.procs[i].pc = Pc::AEpochRef;
            }
            Pc::AEpochRef => {
                s.ws[w].base = s.procs[i].target;
                s.events |= ev::AUTO_SYNCED;
                s.finish(pid);
            }
            pc => unreachable!("autosync pc {pc:?}"),
        }
        true
    }

    fn step_doctor(&self, s: &mut State, pid: Pid) -> bool {
        let i = pid as usize;
        let unlocked = self.mutation == Mutation::DoctorRepairUnlocked;
        match s.procs[i].pc {
            Pc::Start => {
                if !unlocked {
                    if s.epoch_lock.is_some() {
                        return false;
                    }
                    s.epoch_lock = Some(pid);
                }
                s.procs[i].pc = Pc::XClassify;
            }
            Pc::XClassify => {
                // bn-302v: refuse while a (crashed) `ws merge` journal exists
                // (mirrors bn-3w2b's promote refusal) — advancing the epoch
                // under it would make its recovery refuse ("epoch advanced
                // since this merge started").
                if s.merge_state.is_some() && self.mutation != Mutation::DoctorIgnoresMergeJournal {
                    s.finish(pid); // AutoAdvanceSkip::MergeInProgress
                    return true;
                }
                let branch = s.branch;
                if !s.is_strict_ancestor(s.epoch, branch) {
                    s.finish(pid); // InSync / Diverged: no-op
                    return true;
                }
                let ff_paths = diff_paths(s.tree(s.epoch), s.tree(branch));
                let blocked = s.ws.iter().any(|ws| {
                    ws.exists && {
                        let base_t = s.tree(ws.base);
                        (diff_paths(base_t, &ws.wt) | diff_paths(base_t, s.tree(ws.head)))
                            & ff_paths
                            != 0
                    }
                });
                if blocked {
                    s.finish(pid); // FfBlocked
                    return true;
                }
                // Faithful: the classified OIDs. Mutation: "re-read the OIDs
                // fresh" — same value at this step, the damage is the missing
                // lock + plain write below.
                s.procs[i].epoch_before = s.epoch;
                s.procs[i].target = branch;
                s.procs[i].pc = Pc::XWrite;
            }
            Pc::XWrite => {
                if unlocked {
                    s.epoch = s.procs[i].target;
                    s.events |= ev::DOCTOR_ADVANCED;
                } else if s.epoch == s.procs[i].epoch_before {
                    // `advance_epoch` CAS succeeded.
                    s.epoch = s.procs[i].target;
                    s.events |= ev::DOCTOR_ADVANCED;
                }
                // else: CasMismatch → error, nothing written.
                s.finish(pid);
            }
            pc => unreachable!("doctor pc {pc:?}"),
        }
        true
    }

    fn step_promote(&self, s: &mut State, pid: Pid) -> bool {
        let i = pid as usize;
        let legacy = self.mutation == Mutation::QuarantinePromoteUnlockedSplitCas;
        match s.procs[i].pc {
            Pc::Start => {
                if !legacy {
                    // bn-3w2b: `maw merge promote` holds the epoch lock for
                    // the whole promote.
                    if s.epoch_lock.is_some() {
                        return false;
                    }
                    s.epoch_lock = Some(pid);
                }
                s.procs[i].pc = Pc::QCas;
            }
            Pc::QCas => {
                let (eb, q) = (s.procs[i].epoch_before, s.procs[i].candidate);
                // bn-3w2b: refuse while a (crashed) `ws merge` journal
                // exists — moving the epoch under it would make its recovery
                // refuse ("epoch advanced since this merge started").
                if !legacy && s.merge_state.is_some() {
                    s.finish(pid); // QuarantineError::MergeInProgress
                    return true;
                }
                if legacy {
                    // refs::advance_epoch (CAS on the epoch only).
                    if s.epoch != eb {
                        s.finish(pid); // CasMismatch → error, quarantine kept
                        return true;
                    }
                    s.epoch = q;
                    s.procs[i].pc = Pc::QCasBranch;
                    return true;
                }
                // refs::update_refs_atomic([(epoch, eb, q), (branch, eb, q)])
                if s.epoch != eb || s.branch != eb {
                    s.finish(pid); // CasMismatch → error, quarantine kept
                    return true;
                }
                s.epoch = q;
                s.branch = q;
                s.events |= ev::QUARANTINE_PROMOTED;
                s.procs[i].pc = Pc::QCleanup;
            }
            Pc::QCasBranch => {
                let (eb, q) = (s.procs[i].epoch_before, s.procs[i].candidate);
                if s.branch != eb {
                    s.finish(pid); // branch CAS failed; epoch already moved
                    return true;
                }
                s.branch = q;
                s.events |= ev::QUARANTINE_PROMOTED;
                s.procs[i].pc = Pc::QCleanup;
            }
            Pc::QCleanup => {
                s.quarantine = None;
                s.finish(pid);
            }
            pc => unreachable!("promote pc {pc:?}"),
        }
        true
    }

    /// Crash recovery of a killed merge, as performed by the next
    /// `maw ws merge` PREPARE (`src/merge/prepare.rs`) and, failing that,
    /// `maw ws merge --abort` (`AbortOutcome`). Phase dispatch goes through
    /// the production `recovery_outcome_for_phase`.
    fn recover(s: &mut State) {
        let Some(j) = s.merge_state.clone() else {
            return;
        };
        match recovery_outcome_for_phase(&j.phase.to_core()) {
            RecoveryOutcome::AbortedPreCommit { .. } | RecoveryOutcome::RetryValidate => {
                // Pre-COMMIT orphan: no ref moved; auto-recovered.
                s.merge_state = None;
                s.events |= ev::RECOVERED_PRE_CAS;
            }
            RecoveryOutcome::CheckCommit | RecoveryOutcome::RetryCleanup => {
                let advanced = j
                    .candidate
                    .is_some_and(|c| (!j.into_branch_only && s.epoch == c) || s.branch == c);
                if advanced {
                    // stale_completed: refs reached the candidate → clear.
                    s.merge_state = None;
                    s.events |= ev::RECOVERED_POST_CAS;
                } else if s.epoch == j.epoch_before {
                    // --abort: epoch never advanced → safe to clear.
                    s.merge_state = None;
                    s.events |= ev::RECOVERED_PRE_CAS;
                } else {
                    s.stuck = true;
                }
            }
            RecoveryOutcome::Terminal { .. } | RecoveryOutcome::NoMergeInProgress => {
                s.merge_state = None;
            }
        }
    }

    /// Which process `pid` is blocked waiting on (wait-for graph edge).
    fn waits_on(&self, s: &State, pid: usize) -> Option<Pid> {
        let p = &s.procs[pid];
        let other = |h: Option<Pid>| h.filter(|&h| h as usize != pid);
        let reversed = self.mutation == Mutation::ReversedLockOrder;
        match (self.procs[pid], p.pc) {
            (ProcSpec::Sync { ws }, Pc::Start) if reversed => other(s.ws_lock[ws as usize]),
            (ProcSpec::Sync { .. }, Pc::SEpochLock) => other(s.epoch_lock),
            (ProcSpec::AutoSync { .. }, _) => None,
            (ProcSpec::DoctorRepair, _) if self.mutation == Mutation::DoctorRepairUnlocked => None,
            (ProcSpec::QuarantinePromote, _)
                if self.mutation == Mutation::QuarantinePromoteUnlockedSplitCas =>
            {
                None
            }
            (_, Pc::Start) => other(s.epoch_lock),
            (ProcSpec::Merge { .. }, Pc::MReplay(w) | Pc::MAutoRebase(w)) if reversed => {
                other(s.ws_lock[w as usize])
            }
            _ => None,
        }
    }
}

/// Actions: one step of a process, a crash, a recovery, or an agent action.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Action {
    /// Advance process `pid` by one step.
    Step(Pid),
    /// VALIDATE fails for merge `pid` (abort).
    ValidateFail(Pid),
    /// Kill process `pid`.
    Crash(Pid),
    /// The next invocation recovers crashed merge `pid`'s journal.
    Recover(Pid),
    /// Agent edits `path` in workspace `ws` (adds a fresh atom).
    Edit { ws: u8, path: u8 },
    /// Agent `git commit -a` in workspace `ws`.
    AgentCommit(u8),
    /// Direct commit on the target branch touching `path`.
    TrunkCommit(u8),
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

/// G1 + G4: every atom ever created is in a live tip (epoch, branch, an
/// existing workspace's HEAD, a recovery ref) or an existing worktree.
pub const P_NO_LOST_WORK: &str = "no lost work (G1+G4)";
/// No commit drops an atom one of its parents had (silent revert).
pub const P_NO_SILENT_REVERT: &str = "no silent revert";
/// A quiescent existing workspace: epoch ref ⊑ HEAD and HEAD's content ⊆
/// worktree.
pub const P_WS_COHERENT: &str = "workspace coherence";
/// G3: the epoch and branch refs are never observed split around a merge
/// candidate (epoch at candidate ⇒ branch contains it, and vice versa).
pub const P_COMMIT_ATOMIC: &str = "commit atomicity (G3)";
/// bn-38vw: if the refs moved to the journaled candidate, `epoch_after` is
/// recorded.
pub const P_JOURNAL_COHERENT: &str = "journal: refs moved => epoch_after recorded";
/// Oracle B's strict shape: phase commit/cleanup ⇒ `epoch_after` recorded.
pub const P_ORACLE_B_JOURNAL: &str = "journal: phase>=commit => epoch_after recorded (Oracle B)";
/// No cycle in the lock wait-for graph.
pub const P_NO_DEADLOCK: &str = "no deadlock (wait-for acyclic)";
/// The epoch only ever moves forward: every value it has held is an ancestor
/// of its current value (no regression by a stale plain write).
pub const P_EPOCH_MONOTONE: &str = "epoch monotone";
/// Crash recovery always clears the journal.
pub const P_RECOVERY_CONVERGES: &str = "crash recovery converges";

fn covered(s: &State) -> u16 {
    let mut c = tree_atoms(s.tree(s.epoch)) | tree_atoms(s.tree(s.branch));
    for w in &s.ws {
        if w.exists {
            c |= tree_atoms(s.tree(w.head)) | tree_atoms(&w.wt);
        }
    }
    for &r in &s.recovery {
        c |= tree_atoms(s.tree(r));
    }
    if let Some(q) = s.quarantine {
        c |= tree_atoms(s.tree(q));
    }
    c
}

fn prop_no_lost_work(_: &ProtocolModel, s: &State) -> bool {
    s.created & !covered(s) == 0
}

fn prop_no_silent_revert(_: &ProtocolModel, s: &State) -> bool {
    s.commits.iter().all(|c| {
        c.parents
            .iter()
            .flatten()
            .all(|&p| tree_subset(s.tree(p), &c.tree))
    })
}

fn prop_ws_coherent(m: &ProtocolModel, s: &State) -> bool {
    s.ws.iter().enumerate().all(|(w, ws)| {
        !ws.exists
            || m.touching(s, w)
            || (s.is_ancestor_or_eq(ws.base, ws.head) && tree_subset(s.tree(ws.head), &ws.wt))
    })
}

fn prop_commit_atomic(m: &ProtocolModel, s: &State) -> bool {
    // `maw merge promote`: the quarantine candidate is never on one of
    // epoch/branch without the other.
    let promote_ok = m.procs.iter().zip(&s.procs).all(|(spec, p)| {
        *spec != ProcSpec::QuarantinePromote
            || s.is_ancestor_or_eq(p.candidate, s.epoch)
                == s.is_ancestor_or_eq(p.candidate, s.branch)
    });
    if !promote_ok {
        return false;
    }
    let Some(j) = &s.merge_state else {
        return true;
    };
    let Some(c) = j.candidate else {
        return true;
    };
    if j.into_branch_only {
        return true;
    }
    let e_at = s.is_ancestor_or_eq(c, s.epoch);
    let b_at = s.is_ancestor_or_eq(c, s.branch);
    e_at == b_at
}

fn prop_journal_coherent(_: &ProtocolModel, s: &State) -> bool {
    let Some(j) = &s.merge_state else {
        return true;
    };
    let Some(c) = j.candidate else {
        return true;
    };
    let moved = (!j.into_branch_only && s.epoch == c) || s.branch == c;
    !moved || j.epoch_after == Some(c)
}

fn prop_oracle_b_journal(_: &ProtocolModel, s: &State) -> bool {
    s.merge_state.as_ref().is_none_or(|j| {
        !matches!(j.phase, JPhase::Commit | JPhase::Cleanup) || j.epoch_after.is_some()
    })
}

fn prop_no_deadlock(m: &ProtocolModel, s: &State) -> bool {
    let n = s.procs.len();
    (0..n).all(|start| {
        let mut cur = start;
        for _ in 0..n {
            match m.waits_on(s, cur) {
                Some(next) => {
                    cur = next as usize;
                    if cur == start {
                        return false;
                    }
                }
                None => return true,
            }
        }
        true
    })
}

fn prop_epoch_monotone(_: &ProtocolModel, s: &State) -> bool {
    let anc = s.commits[s.epoch as usize].ancestors;
    s.epochs_seen & !anc == 0
}

fn prop_recovery_converges(_: &ProtocolModel, s: &State) -> bool {
    !s.stuck
}

macro_rules! event_prop {
    ($name:ident, $bit:expr) => {
        fn $name(_: &ProtocolModel, s: &State) -> bool {
            s.events & $bit != 0
        }
    };
}
event_prop!(ev_merge_committed, ev::MERGE_COMMITTED);
event_prop!(ev_ff_absorbed, ev::FF_ABSORBED);
event_prop!(ev_sibling_ff, ev::SIBLING_FF);
event_prop!(ev_sibling_replayed, ev::SIBLING_REPLAYED);
event_prop!(ev_skip_stale_dirty, ev::SKIP_STALE_DIRTY);
event_prop!(ev_auto_rebased, ev::AUTO_REBASED);
event_prop!(ev_synced, ev::SYNCED);
event_prop!(ev_destroyed, ev::DESTROYED);
event_prop!(ev_auto_synced, ev::AUTO_SYNCED);
event_prop!(ev_recovered_post_cas, ev::RECOVERED_POST_CAS);
event_prop!(ev_recovered_pre_cas, ev::RECOVERED_PRE_CAS);
event_prop!(ev_agent_commit, ev::AGENT_COMMIT);
event_prop!(ev_destroyed_dirty, ev::DESTROYED_DIRTY);
event_prop!(ev_doctor_advanced, ev::DOCTOR_ADVANCED);
event_prop!(ev_quarantine_promoted, ev::QUARANTINE_PROMOTED);

type PropFn = fn(&ProtocolModel, &State) -> bool;

const EVENT_PROPS: &[(u16, &str, PropFn)] = &[
    (
        ev::MERGE_COMMITTED,
        "sometimes: merge committed",
        ev_merge_committed,
    ),
    (
        ev::FF_ABSORBED,
        "sometimes: FF-absorb advanced epoch",
        ev_ff_absorbed,
    ),
    (
        ev::SIBLING_FF,
        "sometimes: sibling fast-forwarded",
        ev_sibling_ff,
    ),
    (
        ev::SIBLING_REPLAYED,
        "sometimes: sibling replayed",
        ev_sibling_replayed,
    ),
    (
        ev::SKIP_STALE_DIRTY,
        "sometimes: stale-dirty sibling skipped",
        ev_skip_stale_dirty,
    ),
    (
        ev::AUTO_REBASED,
        "sometimes: sibling auto-rebased",
        ev_auto_rebased,
    ),
    (ev::SYNCED, "sometimes: ws synced", ev_synced),
    (ev::DESTROYED, "sometimes: ws destroyed", ev_destroyed),
    (ev::AUTO_SYNCED, "sometimes: ws auto-synced", ev_auto_synced),
    (
        ev::RECOVERED_POST_CAS,
        "sometimes: crash recovered post-CAS",
        ev_recovered_post_cas,
    ),
    (
        ev::RECOVERED_PRE_CAS,
        "sometimes: crash recovered pre-CAS",
        ev_recovered_pre_cas,
    ),
    (
        ev::AGENT_COMMIT,
        "sometimes: agent committed",
        ev_agent_commit,
    ),
    (
        ev::DESTROYED_DIRTY,
        "sometimes: dirty ws destroyed",
        ev_destroyed_dirty,
    ),
    (
        ev::DOCTOR_ADVANCED,
        "sometimes: doctor --repair advanced epoch",
        ev_doctor_advanced,
    ),
    (
        ev::QUARANTINE_PROMOTED,
        "sometimes: quarantine promoted",
        ev_quarantine_promoted,
    ),
];

impl Model for ProtocolModel {
    type State = State;
    type Action = Action;

    fn init_states(&self) -> Vec<Self::State> {
        let mut s = self.initial_state();
        s.epochs_seen = 1u64 << s.epoch;
        vec![s]
    }

    fn actions(&self, s: &Self::State, actions: &mut Vec<Self::Action>) {
        for (pid, p) in s.procs.iter().enumerate() {
            let pid = pid as Pid;
            match p.pc {
                Pc::Done => {}
                Pc::Crashed => {
                    if matches!(self.procs[pid as usize], ProcSpec::Merge { .. })
                        && s.merge_state.is_some()
                    {
                        actions.push(Action::Recover(pid));
                    }
                }
                pc => {
                    actions.push(Action::Step(pid));
                    if pc == Pc::MValidate && self.validate_can_fail {
                        actions.push(Action::ValidateFail(pid));
                    }
                    let in_ff = matches!(
                        pc,
                        Pc::MReplay(_)
                            | Pc::MWriteEpoch
                            | Pc::MFfRef(_)
                            | Pc::MFfMat(_)
                            | Pc::MFfHead(_)
                    );
                    if s.crashes_left > 0 && pc != Pc::Start && (self.crash_in_ff_absorb || !in_ff)
                    {
                        actions.push(Action::Crash(pid));
                    }
                }
            }
        }
        for w in 0..s.ws.len() {
            if !s.ws[w].exists {
                continue;
            }
            let busy = self.touching(s, w);
            if s.edits_left > 0 && (self.agents.edit_anytime || !busy) {
                for path in 0..NPATHS {
                    actions.push(Action::Edit {
                        ws: w as u8,
                        path: path as u8,
                    });
                }
            }
            if s.commits_left > 0 && s.dirty_mask(w) != 0 && (self.agents.commit_anytime || !busy) {
                actions.push(Action::AgentCommit(w as u8));
            }
        }
        if s.trunk_left > 0 {
            for path in 0..NPATHS {
                actions.push(Action::TrunkCommit(path as u8));
            }
        }
    }

    fn next_state(&self, s: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut s = s.clone();
        match action {
            Action::Step(pid) => {
                if !self.step(&mut s, pid) {
                    return None; // blocked on a lock
                }
            }
            Action::ValidateFail(pid) => {
                Self::merge_abort(&mut s, pid);
            }
            Action::Crash(pid) => {
                s.release_all(pid);
                s.procs[pid as usize].pc = Pc::Crashed;
                s.crashes_left -= 1;
            }
            Action::Recover(_) => {
                Self::recover(&mut s);
            }
            Action::Edit { ws, path } => {
                let bit = 1u16 << s.next_atom;
                s.next_atom += 1;
                s.created |= bit;
                s.ws[ws as usize].wt[path as usize] |= bit;
                s.edits_left -= 1;
            }
            Action::AgentCommit(ws) => {
                let w = ws as usize;
                let (wt, head) = (s.ws[w].wt, s.ws[w].head);
                s.ws[w].head = s.mk_commit(wt, head, None);
                s.commits_left -= 1;
                s.events |= ev::AGENT_COMMIT;
            }
            Action::TrunkCommit(path) => {
                let bit = 1u16 << s.next_atom;
                s.next_atom += 1;
                s.created |= bit;
                let mut t = *s.tree(s.branch);
                t[path as usize] |= bit;
                let b = s.branch;
                s.branch = s.mk_commit(t, b, None);
                s.trunk_left -= 1;
            }
        }
        s.epochs_seen |= 1u64 << s.epoch;
        Some(s)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        let mut props = vec![
            Property::<Self>::always(P_NO_LOST_WORK, prop_no_lost_work),
            Property::<Self>::always(P_NO_SILENT_REVERT, prop_no_silent_revert),
            Property::<Self>::always(P_WS_COHERENT, prop_ws_coherent),
            Property::<Self>::always(P_COMMIT_ATOMIC, prop_commit_atomic),
            Property::<Self>::always(P_JOURNAL_COHERENT, prop_journal_coherent),
            Property::<Self>::always(P_NO_DEADLOCK, prop_no_deadlock),
            Property::<Self>::always(P_EPOCH_MONOTONE, prop_epoch_monotone),
            Property::<Self>::always(P_RECOVERY_CONVERGES, prop_recovery_converges),
        ];
        if self.strict_oracle_b {
            props.push(Property::<Self>::always(
                P_ORACLE_B_JOURNAL,
                prop_oracle_b_journal,
            ));
        }
        for &(bit, name, f) in EVENT_PROPS {
            if self.expect & bit != 0 {
                props.push(Property::<Self>::sometimes(name, f));
            }
        }
        props
    }
}

// ---------------------------------------------------------------------------
// Canonical configurations (shared by unit tests and tests/formal_model.rs)
// ---------------------------------------------------------------------------

/// Named configurations. The `fast_*` set is small enough for the gate; the
/// `deep_*` set is for `just formal-check` / nightly.
pub mod configs {
    use super::{AgentPolicy, InitShape, Mutation, ProcSpec, ProtocolModel, ev};

    const fn merge(src: u8) -> ProcSpec {
        ProcSpec::Merge {
            src,
            into_branch_only: false,
            auto_rebase: true,
        }
    }

    /// Step 1: one merge actor + agents that edit/commit (dirty workspaces)
    /// + crashes + validation failure + `destroy --force`.
    pub fn fast_merge_crash_destroy() -> ProtocolModel {
        ProtocolModel {
            procs: vec![merge(0), ProcSpec::Destroy { ws: 1, force: true }],
            agent_edits: 2,
            agent_commits: 2,
            crashes: 1,
            validate_can_fail: true,
            expect: ev::MERGE_COMMITTED
                | ev::DESTROYED
                | ev::DESTROYED_DIRTY
                | ev::RECOVERED_POST_CAS
                | ev::RECOVERED_PRE_CAS
                | ev::AUTO_REBASED
                | ev::AGENT_COMMIT,
            ..ProtocolModel::new(2)
        }
    }

    /// Step 1b: branch-only `--into <change>` CAS path with trunk commits
    /// racing the pre-flight and the CAS.
    pub fn fast_merge_into_branch() -> ProtocolModel {
        ProtocolModel {
            procs: vec![ProcSpec::Merge {
                src: 0,
                into_branch_only: true,
                auto_rebase: false,
            }],
            agent_edits: 1,
            agent_commits: 1,
            trunk_commits: 1,
            crashes: 1,
            expect: ev::MERGE_COMMITTED | ev::RECOVERED_POST_CAS,
            ..ProtocolModel::new(1)
        }
    }

    /// Step 2: concurrent merge + sync + auto-sync sharing the epoch lock and
    /// per-workspace locks.
    pub fn fast_concurrent_actors() -> ProtocolModel {
        ProtocolModel {
            init: InitShape::SiblingBehind,
            procs: vec![
                merge(0),
                ProcSpec::Sync { ws: 1 },
                ProcSpec::AutoSync { ws: 1 },
            ],
            agent_edits: 2,
            agent_commits: 1,
            crashes: 1,
            expect: ev::MERGE_COMMITTED | ev::SYNCED | ev::AUTO_SYNCED | ev::AUTO_REBASED,
            ..ProtocolModel::new(2)
        }
    }

    /// Step 2, agents unrestricted: edits and commits may land between any
    /// two steps of sync / auto-sync / auto-rebase on the same workspace.
    pub fn fast_concurrent_actors_agents_anytime() -> ProtocolModel {
        ProtocolModel {
            agents: AgentPolicy::ANYTIME,
            agent_edits: 2,
            expect: ev::MERGE_COMMITTED | ev::SYNCED | ev::AUTO_SYNCED,
            ..fast_concurrent_actors()
        }
    }

    /// Step 2b: destroy + sync + merge racing on the epoch lock.
    pub fn fast_destroy_vs_sync() -> ProtocolModel {
        ProtocolModel {
            init: InitShape::SiblingBehind,
            procs: vec![
                merge(0),
                ProcSpec::Destroy {
                    ws: 1,
                    force: false,
                },
                ProcSpec::Sync { ws: 1 },
            ],
            agent_edits: 1,
            agent_commits: 1,
            crashes: 1,
            expect: ev::MERGE_COMMITTED | ev::SYNCED | ev::DESTROYED,
            ..ProtocolModel::new(2)
        }
    }

    /// Step 3: FF-absorb (trunk commit ahead of the epoch) with a sibling
    /// more than one epoch behind, agents editing/committing it, and a
    /// concurrent auto-sync of that sibling (no epoch lock).
    pub fn fast_ff_absorb() -> ProtocolModel {
        ProtocolModel {
            init: InitShape::SiblingBehind,
            procs: vec![
                merge(0),
                ProcSpec::AutoSync { ws: 1 },
                ProcSpec::Sync { ws: 2 },
            ],
            agent_edits: 1,
            agent_commits: 1,
            trunk_commits: 1,
            crashes: 1,
            expect: ev::MERGE_COMMITTED
                | ev::FF_ABSORBED
                | ev::SIBLING_FF
                | ev::SIBLING_REPLAYED
                | ev::SKIP_STALE_DIRTY
                | ev::AUTO_SYNCED
                | ev::SYNCED,
            ..ProtocolModel::new(3)
        }
    }

    /// Deep: three workspaces, every actor, more agent budget.
    pub fn deep_everything() -> ProtocolModel {
        ProtocolModel {
            init: InitShape::SiblingBehind,
            procs: vec![
                merge(0),
                ProcSpec::Sync { ws: 1 },
                ProcSpec::AutoSync { ws: 2 },
                ProcSpec::Destroy { ws: 2, force: true },
            ],
            agent_edits: 2,
            agent_commits: 1,
            trunk_commits: 1,
            crashes: 1,
            validate_can_fail: true,
            expect: ev::MERGE_COMMITTED | ev::FF_ABSORBED,
            ..ProtocolModel::new(3)
        }
    }

    /// `maw doctor --repair` racing `ws merge` and a direct trunk commit.
    ///
    /// Faithful = the bn-32g8 fix (epoch lock + CAS); the pre-fix shape is
    /// [`Mutation::DoctorRepairUnlocked`].
    pub fn doctor_vs_merge() -> ProtocolModel {
        ProtocolModel {
            procs: vec![merge(0), ProcSpec::DoctorRepair],
            agent_edits: 1,
            agent_commits: 0,
            trunk_commits: 1,
            crashes: 0,
            expect: ev::MERGE_COMMITTED | ev::FF_ABSORBED | ev::DOCTOR_ADVANCED,
            ..ProtocolModel::new(1)
        }
    }

    /// `maw doctor --repair` after a crashed `ws merge` and a trunk commit.
    ///
    /// The crash leaves the merge journal behind. Faithful = bn-302v (doctor refuses
    /// while the journal exists); the pre-fix shape is
    /// [`Mutation::DoctorIgnoresMergeJournal`].
    pub fn doctor_vs_crashed_merge() -> ProtocolModel {
        ProtocolModel {
            crashes: 1,
            expect: ev::MERGE_COMMITTED
                | ev::DOCTOR_ADVANCED
                | ev::RECOVERED_PRE_CAS
                | ev::RECOVERED_POST_CAS,
            ..doctor_vs_merge()
        }
    }

    /// FF-absorb with agents acting at ANY time, racing the sibling loop.
    ///
    /// Was a residual before bn-302v (sibling lock + re-check +
    /// HEAD CAS); the pre-fix shape is [`Mutation::FfNoSiblingLockRecheck`].
    pub fn fast_ff_absorb_agents_anytime() -> ProtocolModel {
        ProtocolModel {
            agents: AgentPolicy::ANYTIME,
            crashes: 0,
            expect: ev::FF_ABSORBED | ev::SIBLING_FF | ev::AGENT_COMMIT | ev::MERGE_COMMITTED,
            ..fast_ff_absorb()
        }
    }

    /// FF-absorb with crashes allowed inside the sibling loop.
    ///
    /// Was a residual before bn-302v (epoch ref written last); the pre-fix shape is
    /// [`Mutation::FfRefBeforeHead`].
    pub fn fast_ff_absorb_crash_in_loop() -> ProtocolModel {
        ProtocolModel {
            crash_in_ff_absorb: true,
            procs: vec![merge(0)],
            expect: ev::FF_ABSORBED | ev::SIBLING_FF | ev::MERGE_COMMITTED,
            ..fast_ff_absorb()
        }
    }

    /// As [`fast_ff_absorb_crash_in_loop`], plus a second `ws merge`.
    ///
    /// The second merge is of the sibling; pre-bn-302v the leading epoch ref turned into a
    /// silent revert of the absorbed range.
    pub fn fast_ff_absorb_crash_then_merge_sibling() -> ProtocolModel {
        ProtocolModel {
            procs: vec![
                merge(0),
                ProcSpec::Merge {
                    src: 1,
                    into_branch_only: false,
                    auto_rebase: false,
                },
            ],
            ..fast_ff_absorb_crash_in_loop()
        }
    }

    /// Faithful single-merge model checked against Oracle B's strict journal
    /// shape. Green since bn-3w2b (phase + `epoch_after` in one journal
    /// write); the pre-fix shape is [`Mutation::SplitCommitJournal`].
    pub fn fast_oracle_b_strict() -> ProtocolModel {
        ProtocolModel {
            strict_oracle_b: true,
            procs: vec![merge(0)],
            agent_edits: 1,
            agent_commits: 0,
            crashes: 1,
            expect: ev::MERGE_COMMITTED | ev::RECOVERED_PRE_CAS | ev::RECOVERED_POST_CAS,
            ..ProtocolModel::new(1)
        }
    }

    /// `maw merge promote` of a quarantine racing `ws merge`.
    ///
    /// The merge FF-absorbs a direct trunk commit. Faithful = bn-3w2b (epoch lock + one
    /// atomic 2-ref CAS); the pre-fix shape is
    /// [`Mutation::QuarantinePromoteUnlockedSplitCas`].
    pub fn fast_quarantine_promote_vs_merge() -> ProtocolModel {
        ProtocolModel {
            procs: vec![merge(0), ProcSpec::QuarantinePromote],
            agent_edits: 1,
            agent_commits: 0,
            trunk_commits: 1,
            crashes: 1,
            expect: ev::MERGE_COMMITTED | ev::FF_ABSORBED | ev::QUARANTINE_PROMOTED,
            ..ProtocolModel::new(1)
        }
    }

    /// Apply a mutation to a configuration.
    pub fn mutated(mut m: ProtocolModel, mutation: Mutation) -> ProtocolModel {
        m.mutation = mutation;
        m.expect = 0;
        m
    }
}

#[cfg(test)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
mod tests {
    use super::*;

    #[test]
    fn merge3_propagates_theirs_deletions() {
        let base = [0b011, 0];
        let ours = [0b111, 0b1];
        let theirs = [0b001, 0];
        assert_eq!(merge3(&base, &ours, &theirs), [0b101, 0b1]);
    }

    #[test]
    fn merge_base_is_fork_point() {
        let m = ProtocolModel::new(1);
        let mut s = m.initial_state();
        let a = s.mk_commit([1, 0], 0, None);
        let b = s.mk_commit([1, 2], a, None);
        let c = s.mk_commit([5, 0], a, None);
        assert_eq!(s.merge_base(b, c), a);
        assert!(s.is_strict_ancestor(a, b));
        assert!(!s.is_ancestor_or_eq(b, c));
    }

    #[test]
    fn ff_absorb_happy_path_fast_forwards_behind_sibling() {
        // SiblingBehind: ws1 at c0, epoch c1; trunk commit on path 1; merge ws0.
        let m = ProtocolModel {
            init: InitShape::SiblingBehind,
            procs: vec![ProcSpec::Merge {
                src: 0,
                into_branch_only: false,
                auto_rebase: true,
            }],
            trunk_commits: 1,
            ..ProtocolModel::new(2)
        };
        let mut s = m.initial_state();
        s = m.next_state(&s, Action::TrunkCommit(1)).unwrap();
        let branch = s.branch;
        for _ in 0..3 {
            s = m.next_state(&s, Action::Step(0)).unwrap(); // lock, reconcile, write epoch
        }
        assert_eq!(s.epoch, branch);
        // Both workspaces are FF siblings (the merge source is a sibling of
        // the absorb too): ref, materialize, head for ws0 then ws1.
        for _ in 0..6 {
            s = m.next_state(&s, Action::Step(0)).unwrap();
        }
        assert_eq!(s.procs[0].pc, Pc::MPrepare);
        assert_eq!(s.ws[1].head, branch);
        assert_eq!(s.ws[1].base, branch);
        assert_eq!(s.ws[1].wt, *s.tree(branch));
    }
}
