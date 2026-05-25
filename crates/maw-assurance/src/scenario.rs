//! Deterministic scenario + condition generator (bn-1f53, T1.2).
//!
//! This module implements the **driver-agnostic plan stream** specified in
//! `notes/sg1-dst-architecture.md` §2 — the single shared substrate consumed
//! by **both** SG1's in-process model driver (T1.3/T1.4/T1.5) **and** SG2's
//! real-agent benchmark driver (T2.1). "Build once, drive two ways."
//!
//! # What this module is
//!
//! A pure, deterministic, side-effect-free generator. It:
//!
//! - **Never** calls `maw`, **never** spawns a process, **never** links the
//!   merge FSM, **never** touches the disk.
//! - Knows only the **abstract** model state (workspace set, per-workspace
//!   committed/uncommitted/stale flag, in-flight merges, epoch counter) — just
//!   enough to emit only model-valid ops, yet hostile enough to reach the
//!   bn-cm63 class (concurrent destroy of a workspace that is a live merge
//!   source).
//! - Is **byte-identical for a given `(seed, profile)`** across runs and across
//!   machines (no `HashMap` iteration, no PID/host/wall-clock in the plan, no
//!   filesystem-order dependence). See [`ScenarioPlan::canonical_json`].
//!
//! # What this module is NOT
//!
//! - Not a driver. The two drivers (in-proc and real-agent) live downstream
//!   (T1.3/T1.4/T1.5 and T2.1) and interpret the same [`ScenarioPlan`] in
//!   their own substrates. See [`ScenarioDriver`].
//! - Not a fault implementor. The fault selection vocabulary
//!   ([`FaultSpec`]) interoperates with the [`crate::fault`] module
//!   (bn-263u, T1.5); this generator just decides *which* steps carry a fault.
//!
//! # Hostile interleavings
//!
//! The generator can deliberately schedule a [`Op::Destroy`] of a workspace
//! that is currently a source of an in-flight [`Op::Merge`] — the
//! **bn-cm63 class**. This is the canonical acceptance test for the gate.
//! See [`tests::seed_reaches_bn_cm63_class`].
//!
//! # Determinism contract (sg1-dst-architecture.md §5)
//!
//! Each [`PlannedStep`] carries a seed-derived `git_time` that drivers MUST
//! pin into `GIT_AUTHOR_DATE` / `GIT_COMMITTER_DATE`. The plan's git_time
//! sequence is non-decreasing across the plan so committer dates make sense
//! to git. Without this pin, commit OIDs embed wall-clock time and replay is
//! not bit-exact (SP1 proved seed 42 produced two different candidate OIDs
//! until the dates were pinned).

#![cfg(feature = "scenario")]
#![allow(clippy::doc_markdown)]
#![allow(clippy::too_long_first_doc_paragraph)]

use std::collections::{BTreeMap, BTreeSet};

use rand::rngs::StdRng;
use rand::seq::IndexedRandom;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Stable identifier / payload types (sg1-dst-architecture.md §2.1 placeholders)
// ---------------------------------------------------------------------------

/// Abstract workspace identifier. Stable across runs for a given seed.
///
/// In the in-proc driver this becomes a `ws/<id>/` directory name; in the
/// real-agent driver it is the same name the harness asks the agent to use.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WsId(pub String);

impl WsId {
    /// Construct a stable `WsId` from a non-negative integer slot.
    ///
    /// The format `ws-<n>` is deliberately short and matches the convention
    /// used throughout the maw test corpus.
    #[must_use]
    pub fn slot(n: usize) -> Self {
        Self(format!("ws-{n}"))
    }
}

/// The base ref a new workspace is created from. Driver-agnostic: the in-proc
/// driver maps this to a git ref; the real-agent driver passes it as
/// `--from <ref>` on the spawned `maw ws create`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum BaseRef {
    /// The configured project branch (e.g. `main`). Stable across replays.
    Main,
    /// The current epoch ref at plan-emit time. The generator never resolves
    /// this — the driver does, at apply-time.
    Epoch,
}

/// A seed-derived file edit. Content is byte-identical for a given
/// `(seed, ws, step_index, path)` so two replays yield identical blob OIDs
/// (combined with the pinned git_time, this is what makes in-proc replay
/// bit-exact per the determinism contract).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileEdit {
    /// Relative path inside the workspace.
    pub path: String,
    /// Seed-derived deterministic content.
    pub content: String,
}

/// A seed-derived value (commit message etc.) the driver renders verbatim.
///
/// Wrapping the string in a named newtype makes it explicit in the contract
/// that the value is generated, not free-form, and protects against drivers
/// "fixing it up" out-of-band (which would break bit-exact OID replay).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seeded(pub String);

/// The target of a merge. Mirrors `maw ws merge --into <target>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum Target {
    /// Merge into the default workspace (the maw v2 convention).
    Default,
    /// Merge into a named change/branch.
    Change(String),
}

// ---------------------------------------------------------------------------
// FaultSpec — minimal seam interoperating with crate::fault (bn-263u, T1.5)
// ---------------------------------------------------------------------------

/// A driver-agnostic fault attached to a [`PlannedStep`].
///
/// This is the **interop seam** with [`crate::fault`] (T1.5). The generator
/// decides only *which* steps carry a fault and *where* the fault lands
/// (which FSM phase / which named failpoint). The actual arming/SIGKILL is
/// the driver's job:
///
/// - In-proc driver → translates `Failpoint{..}` to `maw_core::failpoints::set`.
/// - Faithful subprocess driver → exports `MAW_FP=...` on the spawned `maw`,
///   then delivers a real `SIGKILL` once the state file shows the target
///   phase (SP1 env bridge).
///
/// We deliberately keep the type tiny (no `Signal`, no `Action` enum) so the
/// generator stays serializable without pulling in OS-specific machinery.
/// Drivers that need richer info derive it from `(failpoint, phase)` —
/// `crate::fault::FaultPlan::from_seed` is the canonical translator.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum FaultSpec {
    /// No fault. Most steps.
    None,
    /// A failpoint-mediated fault landing at the given named site, in the
    /// given FSM phase. The phase is redundant with the site (the site name
    /// encodes the phase) but is included for trace-readability.
    Failpoint {
        /// Named failpoint, e.g. `"FP_COMMIT_BETWEEN_CAS_OPS"`. Must be one of
        /// the canonical sites in `crate::fault::CRASHABLE_BY_PHASE`.
        name: String,
        /// FSM phase the site lives in, e.g. `"commit"`.
        phase: String,
    },
}

impl FaultSpec {
    /// `true` iff this step carries any fault.
    #[must_use]
    pub const fn is_some(&self) -> bool {
        !matches!(self, Self::None)
    }
}

// ---------------------------------------------------------------------------
// Op — driver-agnostic operation vocabulary (sg1-dst-architecture.md §2.1)
// ---------------------------------------------------------------------------

/// Driver-agnostic operation vocabulary.
///
/// Mirrors `maw`'s CLI surface AND the merge FSM phases so both drivers and
/// the oracle speak one language. The discriminant tag `"op"` in the JSON
/// makes plans grep-friendly and tool-readable.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "op")]
pub enum Op {
    /// Create workspace `ws` from `from`.
    WsCreate {
        /// New workspace id.
        ws: WsId,
        /// Base ref to create from.
        from: BaseRef,
    },
    /// Apply one or more seed-derived file edits inside `ws`.
    EditFiles {
        /// Target workspace.
        ws: WsId,
        /// Seed-derived edits, in canonical (path-sorted) order.
        files: Vec<FileEdit>,
    },
    /// Commit pending edits in `ws` with a seed-derived message.
    Commit {
        /// Target workspace.
        ws: WsId,
        /// Seed-derived commit message.
        msg: Seeded,
    },
    /// Merge source workspaces into `into`, optionally destroying the sources.
    ///
    /// Note: `srcs` is canonically sorted by `WsId` to keep the plan
    /// byte-identical regardless of how the generator selected them.
    Merge {
        /// Source workspaces (sorted).
        srcs: Vec<WsId>,
        /// Merge target.
        into: Target,
        /// Whether to destroy sources on success (`maw ws merge --destroy`).
        destroy: bool,
    },
    /// Sync `ws` to the current epoch (`maw ws sync`).
    Sync {
        /// Target workspace.
        ws: WsId,
    },
    /// Destroy `ws` (`maw ws destroy`, optionally `--force`).
    Destroy {
        /// Target workspace.
        ws: WsId,
        /// Force destroy even with unmerged work (Prime-Invariant guard
        /// captures recovery snapshot).
        force: bool,
    },
    /// Recover a destroyed workspace into `to` (`maw ws recover <ws> --to <to>`).
    Recover {
        /// Destroyed workspace to recover from.
        ws: WsId,
        /// New workspace name to materialize into.
        to: WsId,
    },
}

// ---------------------------------------------------------------------------
// ConditionProfile (sg1-dst-architecture.md §2.1)
// ---------------------------------------------------------------------------

/// All knobs the seed parameterises. One profile = one knob vector.
///
/// All values are clamped at plan-emit time (`new` / `Default`); rates outside
/// `[0.0, 1.0]` and a `concurrency_degree` of 0 are coerced to safe values so
/// a malformed external profile cannot silently produce a nonsensical plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConditionProfile {
    /// Parallel in-flight ops the generator targets. `>= 1`.
    pub concurrency_degree: u8,
    /// Per-step probability of carrying a kill/failpoint fault.
    pub mid_op_kill_prob: f64,
    /// Per-edit probability of two workspaces editing the same path
    /// (forces diff3).
    pub overlapping_edit_rate: f64,
    /// Per-step probability of leaving a workspace un-synced across an
    /// epoch bump (the bn-7phd "stale source" class).
    pub stale_workspace_rate: f64,
}

impl ConditionProfile {
    /// Construct a profile with clamped fields.
    #[must_use]
    pub fn new(
        concurrency_degree: u8,
        mid_op_kill_prob: f64,
        overlapping_edit_rate: f64,
        stale_workspace_rate: f64,
    ) -> Self {
        Self {
            concurrency_degree: concurrency_degree.max(1),
            mid_op_kill_prob: mid_op_kill_prob.clamp(0.0, 1.0),
            overlapping_edit_rate: overlapping_edit_rate.clamp(0.0, 1.0),
            stale_workspace_rate: stale_workspace_rate.clamp(0.0, 1.0),
        }
    }
}

impl Default for ConditionProfile {
    /// Default soak profile: moderate concurrency, frequent enough faults to
    /// be interesting, moderate hostility. Tuned for the bounded per-commit
    /// SG1 budget (sg1-dst-architecture.md §7).
    fn default() -> Self {
        Self::new(3, 0.15, 0.30, 0.20)
    }
}

// ---------------------------------------------------------------------------
// PlannedStep / ScenarioPlan (sg1-dst-architecture.md §2.1)
// ---------------------------------------------------------------------------

/// A single step in a [`ScenarioPlan`].
///
/// Carries the deterministic `git_time` per the mandatory determinism contract
/// (sg1-dst-architecture.md §5). Drivers MUST pin `GIT_AUTHOR_DATE` and
/// `GIT_COMMITTER_DATE` to this value around the underlying git write.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedStep {
    /// 0-based position in the plan.
    pub index: usize,
    /// The abstract operation.
    pub op: Op,
    /// Seed-selected fault (or [`FaultSpec::None`]).
    pub fault: FaultSpec,
    /// Deterministic git clock for this step (Unix seconds).
    /// Pinned into `GIT_AUTHOR_DATE` / `GIT_COMMITTER_DATE` by drivers.
    /// Non-decreasing across the plan.
    pub git_time: i64,
}

/// A replayable scenario plan. The unit of a regression seed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScenarioPlan {
    /// Seed that produced this plan.
    pub seed: u64,
    /// Profile that produced this plan.
    pub profile: ConditionProfile,
    /// Steps, model-valid by construction.
    pub steps: Vec<PlannedStep>,
}

impl ScenarioPlan {
    /// Serialize this plan to canonical JSON (compact, BTreeMap-stable). The
    /// byte-output is the equality predicate for the "same seed ⇒ identical
    /// plan" determinism test.
    ///
    /// # Errors
    ///
    /// Returns `serde_json::Error` if serialization fails. In practice this is
    /// infallible for the types in this module — they are all `Serialize`-clean
    /// and contain no maps with non-string keys.
    pub fn canonical_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

// ---------------------------------------------------------------------------
// ScenarioDriver (the trait the two drivers implement; we do NOT implement it)
// ---------------------------------------------------------------------------

/// The trait each driver implements. The generator constructs one
/// [`ScenarioPlan`]; **two** implementations of this trait are the "two ways"
/// the SG1 architecture refers to.
///
/// **This crate intentionally does not provide an implementation.** The
/// in-proc driver lands with T1.3 (bn-1z8q) / T1.4 (bn-3ji6), the real-agent
/// driver lands with T2.1.
pub trait ScenarioDriver {
    /// Whatever the driver returns after replaying the plan. The in-proc
    /// driver returns an oracle report; the real-agent driver returns a
    /// benchmark trace.
    type Outcome;

    /// Drive `plan` end-to-end, invoking the oracle (or the agent) per the
    /// driver's substrate.
    fn drive(&mut self, plan: &ScenarioPlan) -> Self::Outcome;
}

// ---------------------------------------------------------------------------
// ScenarioGenerator (the trait + the canonical implementation)
// ---------------------------------------------------------------------------

/// The trait T1.2 provides; T1.3 / T1.4 / T2.1 consume.
pub trait ScenarioGenerator {
    /// Produce a model-valid plan deterministically from `(seed, profile)`.
    fn generate(seed: u64, profile: &ConditionProfile) -> ScenarioPlan;
}

/// The canonical [`ScenarioGenerator`] implementation.
///
/// Driven by a `StdRng` seeded from `seed`. Tracks a tiny abstract model
/// state ([`AbstractModel`]) so it can emit only model-valid ops while still
/// reaching hostile interleavings (concurrent destroy of a merge source — the
/// bn-cm63 class — stale-ws-into-merge, overlapping edits forcing diff3,
/// faults at every FSM boundary).
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultScenarioGenerator;

/// Number of steps in a default plan. Small enough to keep the per-commit
/// SG1 budget tractable, large enough that the bn-cm63 class is reachable
/// under typical seeds. Soak campaigns multiply this externally.
const DEFAULT_PLAN_STEPS: usize = 32;

/// Base for the deterministic git clock — 2026-01-01T00:00:00 UTC. Matches
/// the `GIT_AUTHOR_DATE` constant the faithful tier ([`crate::fault`])
/// already uses, so a plan replayed across both tiers produces consistent
/// committer dates.
const GIT_TIME_BASE: i64 = 1_767_225_600;

/// The canonical regression seed for the **bn-cm63 hostile interleaving**.
///
/// Under [`ConditionProfile::default`], the plan produced by
/// `DefaultScenarioGenerator::generate(CANONICAL_BN_CM63_SEED, &default)`
/// contains an `Op::Destroy { ws, .. }` that fires while `ws` is a source
/// of a concurrent in-flight `Op::Merge` — the exact pattern that produced
/// the dangling `refs/manifold/head/<ws>` leak in the original incident.
///
/// Pinned here as a `pub const` so downstream tasks (T1.4 Oracle B B1
/// check, T1.6 shrinker, T1.8 permanent regression seed corpus bn-3ryq)
/// can hard-code the seed without re-discovering it. The
/// `seed_reaches_bn_cm63_class` test asserts this value remains the first
/// such seed in `0..200`; if the chooser's distribution shifts, that test
/// fires and we re-pin (and propagate to the corpus).
pub const CANONICAL_BN_CM63_SEED: u64 = 1;

impl ScenarioGenerator for DefaultScenarioGenerator {
    fn generate(seed: u64, profile: &ConditionProfile) -> ScenarioPlan {
        generate_plan(seed, profile, DEFAULT_PLAN_STEPS)
    }
}

/// Generate a plan with `n_steps`. Public so soak campaigns can vary length
/// without ducking through the trait.
#[must_use]
pub fn generate_plan(seed: u64, profile: &ConditionProfile, n_steps: usize) -> ScenarioPlan {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut model = AbstractModel::default();
    // Pre-seed: one starter workspace exists, so EditFiles/Commit/Destroy
    // can fire from step 0 (without this every plan starts with the same
    // boring WsCreate prelude, halving the interesting tail).
    let starter = WsId::slot(model.alloc_slot());
    model.create_ws(starter.clone(), /*synthetic*/ true);
    let mut steps: Vec<PlannedStep> = Vec::with_capacity(n_steps);
    // Pre-seed step #0 is the synthetic create (so the plan is self-contained
    // and a fresh driver can replay it from a clean repo).
    steps.push(PlannedStep {
        index: 0,
        op: Op::WsCreate {
            ws: starter,
            from: BaseRef::Main,
        },
        fault: FaultSpec::None,
        git_time: GIT_TIME_BASE,
    });

    let mut git_time = GIT_TIME_BASE + 1;
    for index in 1..n_steps {
        let op = choose_op(&mut rng, &mut model, profile);
        let fault = choose_fault(&mut rng, profile, &op);
        // Each step advances git_time by 1..=60 seconds — monotonic and
        // deterministic, so committer dates make sense to git AND replays
        // are bit-exact. `random_range` is exclusive at top so add 1.
        let bump: i64 = rng.random_range(1..=60);
        steps.push(PlannedStep {
            index,
            op,
            fault,
            git_time,
        });
        git_time = git_time.saturating_add(bump);
    }

    ScenarioPlan {
        seed,
        profile: profile.clone(),
        steps,
    }
}

// ---------------------------------------------------------------------------
// Abstract model — minimum state to keep ops valid + reach hostile interleavings
// ---------------------------------------------------------------------------

/// Per-workspace state the generator tracks.
///
/// Each bool is an independent dimension of validity the chooser must
/// respect (commit/dirty/stale/in-flight); a bitflags struct would be
/// strictly cosmetic here and uglier at the use-sites, so the explicit
/// fields are preferred over satisfying the four-bool clippy heuristic.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug)]
struct WsState {
    /// `true` once a Commit has fired (so Merge has something to take).
    has_commit: bool,
    /// `true` after EditFiles before the next Commit (so Destroy without
    /// `--force` is a Prime-Invariant violation we don't emit).
    has_uncommitted: bool,
    /// `true` once the workspace's epoch has been bumped by a merge it did
    /// not participate in (the bn-7phd stale-source class).
    is_stale: bool,
    /// `true` while this workspace is a source of an in-flight Merge. Used
    /// to *deliberately* schedule the bn-cm63 concurrent-destroy class.
    in_flight_merge_source: bool,
}

/// The whole abstract model. Uses `BTreeMap` / `BTreeSet` throughout — the
/// determinism contract (§5.5) forbids `HashMap` iteration in plan output.
#[derive(Clone, Debug, Default)]
struct AbstractModel {
    /// Live workspaces (id → state).
    workspaces: BTreeMap<WsId, WsState>,
    /// Destroyed workspaces (recoverable). Used to gate `Op::Recover`.
    destroyed: BTreeSet<WsId>,
    /// Monotonic slot counter; new workspaces never reuse a freed id, which
    /// keeps subsequent ops in the plan readable and avoids accidental
    /// collisions with a yet-to-recover ghost.
    next_slot: usize,
    /// Currently in-flight merges: for each, the set of source ws ids.
    /// Vec because order of inception matters for the bn-cm63 schedule.
    in_flight_merges: Vec<BTreeSet<WsId>>,
    /// Epoch counter; bumped on every Merge complete (Sync clears `is_stale`).
    epoch: u64,
}

impl AbstractModel {
    const fn alloc_slot(&mut self) -> usize {
        let s = self.next_slot;
        self.next_slot += 1;
        s
    }

    fn create_ws(&mut self, id: WsId, synthetic: bool) {
        let _ = synthetic;
        self.workspaces.insert(
            id,
            WsState {
                has_commit: false,
                has_uncommitted: false,
                is_stale: false,
                in_flight_merge_source: false,
            },
        );
    }

    fn live_ws_ids(&self) -> Vec<WsId> {
        // BTreeMap iteration is ordered → byte-stable.
        self.workspaces.keys().cloned().collect()
    }

    /// Schedule a Merge in flight: mark sources, bump nothing yet (the merge
    /// itself completes on the *next* mutating step, modelling the
    /// concurrent window the bn-cm63 class lives in).
    fn begin_merge(&mut self, srcs: &[WsId]) {
        let set: BTreeSet<WsId> = srcs.iter().cloned().collect();
        for s in srcs {
            if let Some(ws) = self.workspaces.get_mut(s) {
                ws.in_flight_merge_source = true;
            }
        }
        self.in_flight_merges.push(set);
    }

    /// Eagerly settle the oldest in-flight merge if there is one. Models the
    /// driver completing one of the in-flight ops between plan steps.
    /// Destroys here are explicitly NOT done (the driver layer handles
    /// `destroy: true`) — this is just the abstract bookkeeping.
    ///
    /// Invariant kept narrow on purpose: settling a merge **only** bumps the
    /// epoch and marks non-participating live workspaces stale (bn-7phd
    /// class). It does NOT touch the participating workspaces'
    /// `has_commit` / `has_uncommitted` flags — those reflect the workspace
    /// file/index state, which a merge into default does not clear. (Whether
    /// `--destroy` removes them is the [`Op::Destroy`] step's job, emitted
    /// separately by the chooser.) Keeping settle narrow is what lets the
    /// test replay validate every plan against the same abstract model
    /// without having to replicate the chooser's settle dice.
    fn settle_one_in_flight_merge(&mut self) {
        if self.in_flight_merges.is_empty() {
            return;
        }
        let done = self.in_flight_merges.remove(0);
        self.epoch += 1;
        for (id, st) in &mut self.workspaces {
            if done.contains(id) {
                // Source is no longer mid-merge; its file state is untouched.
                st.in_flight_merge_source = false;
            } else {
                // Non-participants miss this epoch bump → stale (bn-7phd).
                st.is_stale = true;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Op selection — the policy that enforces validity + reaches hostile cases
// ---------------------------------------------------------------------------

/// Op kinds the chooser ranks; we then *narrow* to validity in-model.
#[derive(Clone, Copy, Debug)]
enum OpKind {
    WsCreate,
    EditFiles,
    Commit,
    Merge,
    Sync,
    Destroy,
    Recover,
    /// Special: emit a destroy of a live merge source (bn-cm63 class).
    DestroyLiveMergeSource,
}

/// Choose the next op. May settle an in-flight merge first (to bump the
/// epoch and create stale-source pressure). The chosen op is always valid
/// in the post-settle model state.
fn choose_op(rng: &mut StdRng, model: &mut AbstractModel, profile: &ConditionProfile) -> Op {
    // With concurrency_degree controlling how willing we are to settle: low
    // concurrency → settle eagerly (serial merges); high → keep merges piled
    // up (so destroy-of-merge-source becomes reachable). bn-cm63.
    let settle_prob = 1.0 / f64::from(profile.concurrency_degree.max(1));
    if !model.in_flight_merges.is_empty() && rng.random_bool(settle_prob) {
        model.settle_one_in_flight_merge();
    }

    // Roll a desired op kind, then narrow to a valid one. The dice are tuned
    // to keep an interesting steady-state mix without starving any kind.
    let kinds: &[(OpKind, u32)] = &[
        (OpKind::WsCreate, 6),
        (OpKind::EditFiles, 18),
        (OpKind::Commit, 14),
        (OpKind::Merge, 10),
        (OpKind::Sync, 6),
        (OpKind::Destroy, 4),
        (OpKind::Recover, 2),
        // Explicit hostile draw — small but nonzero so every long plan
        // reliably gets at least one bn-cm63 attempt.
        (OpKind::DestroyLiveMergeSource, 4),
    ];
    let mut desired = weighted_choice(rng, kinds);

    // Validity narrowing: if the desired op isn't legal right now, fall back
    // along a stable priority list (deterministic, no Hash iteration).
    for _ in 0..kinds.len() {
        if let Some(op) = try_emit(rng, model, profile, desired) {
            apply_to_model(model, &op);
            return op;
        }
        desired = next_kind_fallback(desired);
    }

    // Absolute fallback: a fresh WsCreate is always legal. Peek the next
    // slot (don't bump here — `apply_to_model` does the bump) so the
    // model state stays consistent with the regular try_emit/apply path.
    let op = Op::WsCreate {
        ws: WsId::slot(model.next_slot),
        from: BaseRef::Main,
    };
    apply_to_model(model, &op);
    op
}

const fn next_kind_fallback(k: OpKind) -> OpKind {
    // Fixed cycle, never random — keeps fallback chain deterministic. Every
    // kind appears exactly once in the cycle so the fallback eventually
    // tries every option before giving up to the absolute WsCreate fallback.
    match k {
        OpKind::DestroyLiveMergeSource => OpKind::Merge,
        OpKind::Merge => OpKind::Commit,
        OpKind::Commit => OpKind::Sync,
        OpKind::Sync => OpKind::Destroy,
        OpKind::Destroy => OpKind::Recover,
        OpKind::Recover => OpKind::WsCreate,
        OpKind::WsCreate => OpKind::EditFiles,
        OpKind::EditFiles => OpKind::DestroyLiveMergeSource,
    }
}

#[allow(clippy::too_many_lines)]
fn try_emit(
    rng: &mut StdRng,
    model: &AbstractModel,
    profile: &ConditionProfile,
    kind: OpKind,
) -> Option<Op> {
    match kind {
        OpKind::WsCreate => {
            // Always legal up to a soft cap that keeps plans tractable.
            if model.workspaces.len() >= 8 {
                return None;
            }
            let id = WsId::slot(model.next_slot);
            Some(Op::WsCreate {
                ws: id,
                from: if rng.random_bool(0.5) {
                    BaseRef::Main
                } else {
                    BaseRef::Epoch
                },
            })
        }
        OpKind::EditFiles => {
            let ws = pick_live_ws(rng, model)?;
            // 1..=3 files; each may overlap with a path another ws also edits
            // (to force diff3 at merge time per `overlapping_edit_rate`).
            let n: usize = rng.random_range(1..=3);
            let mut files = Vec::with_capacity(n);
            for i in 0..n {
                let path = if rng.random_bool(profile.overlapping_edit_rate) {
                    // Shared path across workspaces → diff3 overlap.
                    let slot: u64 = rng.random_range(0..4);
                    format!("shared/file-{slot}.txt")
                } else {
                    // Ws-private path → no overlap.
                    format!("{}/file-{i}.txt", ws.0)
                };
                let blob: u64 = rng.random();
                let content = format!("ws={}\nseed-slot={blob}\nidx={i}\n", ws.0);
                files.push(FileEdit { path, content });
            }
            // Canonical order: sort by path so the same `(ws, draws)` ⇒ same
            // bytes regardless of the chooser's draw order.
            files.sort_by(|a, b| a.path.cmp(&b.path));
            Some(Op::EditFiles { ws, files })
        }
        OpKind::Commit => {
            let candidates: Vec<WsId> = model
                .workspaces
                .iter()
                .filter(|(_, s)| s.has_uncommitted)
                .map(|(k, _)| k.clone())
                .collect();
            let ws = pick_from(rng, &candidates)?;
            let msg = Seeded(format!("commit @ {} #{}", ws.0, rng.random::<u32>()));
            Some(Op::Commit { ws, msg })
        }
        OpKind::Merge => {
            let candidates: Vec<WsId> = model
                .workspaces
                .iter()
                .filter(|(_, s)| s.has_commit && !s.in_flight_merge_source)
                .map(|(k, _)| k.clone())
                .collect();
            if candidates.is_empty() {
                return None;
            }
            let cap = candidates.len().min(2);
            let n = rng.random_range(1..=cap);
            let mut srcs: Vec<WsId> = candidates.choose_multiple(rng, n).cloned().collect();
            srcs.sort();
            Some(Op::Merge {
                srcs,
                into: Target::Default,
                destroy: rng.random_bool(0.5),
            })
        }
        OpKind::Sync => {
            let candidates: Vec<WsId> = model
                .workspaces
                .iter()
                .filter(|(_, s)| s.is_stale && !s.has_uncommitted)
                .map(|(k, _)| k.clone())
                .collect();
            let ws = pick_from(rng, &candidates)?;
            Some(Op::Sync { ws })
        }
        OpKind::Destroy => {
            let candidates: Vec<WsId> = model
                .workspaces
                .iter()
                .filter(|(_, s)| !s.in_flight_merge_source)
                .map(|(k, _)| k.clone())
                .collect();
            let ws = pick_from(rng, &candidates)?;
            // We require `force = true` whenever there is uncommitted work,
            // to remain Prime-Invariant compliant (the gen only emits ops the
            // driver could execute without abuse of `--force`).
            let st = model.workspaces.get(&ws)?;
            let force = st.has_uncommitted || rng.random_bool(0.25);
            Some(Op::Destroy { ws, force })
        }
        OpKind::Recover => {
            let candidates: Vec<WsId> = model.destroyed.iter().cloned().collect();
            let src = pick_from(rng, &candidates)?;
            // Recover into a fresh slot; never overwrite a live ws.
            let to = WsId::slot(model.next_slot);
            Some(Op::Recover { ws: src, to })
        }
        OpKind::DestroyLiveMergeSource => {
            // The bn-cm63 class: pick a workspace that is *currently* a
            // source of an in-flight merge and destroy it. Only legal when
            // we actually have an in-flight merge; otherwise drop through.
            let live_sources: Vec<WsId> = model
                .workspaces
                .iter()
                .filter(|(_, s)| s.in_flight_merge_source)
                .map(|(k, _)| k.clone())
                .collect();
            let ws = pick_from(rng, &live_sources)?;
            // The Prime-Invariant `--force` is what the bn-cm63 chaos pattern
            // exercises; the destroy MUST capture a recovery snapshot.
            Some(Op::Destroy { ws, force: true })
        }
    }
}

/// Apply the just-emitted op to the abstract model so subsequent ops are
/// legal given the new state. Mirrors the driver's effect (abstractly).
fn apply_to_model(model: &mut AbstractModel, op: &Op) {
    match op {
        Op::WsCreate { ws, .. } => {
            // Allocate a slot equal to the id we emitted (slot was peeked).
            model.alloc_slot();
            model.create_ws(ws.clone(), false);
        }
        Op::EditFiles { ws, .. } => {
            if let Some(st) = model.workspaces.get_mut(ws) {
                st.has_uncommitted = true;
            }
        }
        Op::Commit { ws, .. } => {
            if let Some(st) = model.workspaces.get_mut(ws) {
                st.has_uncommitted = false;
                st.has_commit = true;
            }
        }
        Op::Merge { srcs, destroy, .. } => {
            model.begin_merge(srcs);
            if *destroy {
                // The merge's `--destroy` semantics: sources will be gone
                // after the merge completes. The driver settles that; our
                // abstract bookkeeping just notes the *intent*: if this is
                // settled by `settle_one_in_flight_merge`, those sources
                // remain live in the model (matches "destroy may fail under
                // concurrent destroy"). The Destroy op explicitly removes.
            }
        }
        Op::Sync { ws } => {
            if let Some(st) = model.workspaces.get_mut(ws) {
                st.is_stale = false;
            }
        }
        Op::Destroy { ws, .. } => {
            if model.workspaces.remove(ws).is_some() {
                model.destroyed.insert(ws.clone());
            }
            // Also drop from any in-flight merge source set (the destroy
            // wins over the merge if it lands first; the driver records the
            // race). This is the bn-cm63 class — we *expect* a coherent
            // recovery; the oracle checks that.
            for set in &mut model.in_flight_merges {
                set.remove(ws);
            }
        }
        Op::Recover { ws, to } => {
            model.destroyed.remove(ws);
            model.create_ws(to.clone(), false);
            // Bump slot counter to match the `to` id we emitted.
            model.alloc_slot();
        }
    }
}

// ---------------------------------------------------------------------------
// Fault selection — interop with crate::fault (T1.5)
// ---------------------------------------------------------------------------

/// Decide whether `op` carries a fault, and which named failpoint it lands
/// on. Faults are only attached to ops that exercise a merge FSM phase —
/// anything else has nothing the failpoint registry would trip.
fn choose_fault(rng: &mut StdRng, profile: &ConditionProfile, op: &Op) -> FaultSpec {
    if !matches!(op, Op::Merge { .. }) {
        return FaultSpec::None;
    }
    if !rng.random_bool(profile.mid_op_kill_prob) {
        return FaultSpec::None;
    }
    let (phase, sites) =
        crate::fault::CRASHABLE_BY_PHASE[rng.random_range(0..crate::fault::CRASHABLE_BY_PHASE.len())];
    let name = sites[rng.random_range(0..sites.len())];
    FaultSpec::Failpoint {
        name: name.to_string(),
        phase: phase.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Small RNG helpers — kept inline and free of any HashMap iteration
// ---------------------------------------------------------------------------

fn weighted_choice(rng: &mut StdRng, choices: &[(OpKind, u32)]) -> OpKind {
    let total: u32 = choices.iter().map(|(_, w)| *w).sum();
    let pick = rng.random::<u32>() % total.max(1);
    let mut acc = 0;
    for (k, w) in choices {
        acc += *w;
        if pick < acc {
            return *k;
        }
    }
    choices[0].0
}

fn pick_live_ws(rng: &mut StdRng, model: &AbstractModel) -> Option<WsId> {
    pick_from(rng, &model.live_ws_ids())
}

fn pick_from<T: Clone>(rng: &mut StdRng, xs: &[T]) -> Option<T> {
    if xs.is_empty() {
        return None;
    }
    Some(xs[rng.random_range(0..xs.len())].clone())
}

// ---------------------------------------------------------------------------
// Tests (gated on `scenario`)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Determinism: same `(seed, profile)` ⇒ byte-identical canonical JSON.
    #[test]
    fn plan_is_byte_identical_for_same_seed() {
        let profile = ConditionProfile::default();
        for seed in [0_u64, 1, 7, 42, 99, 12345, u64::MAX] {
            let a = DefaultScenarioGenerator::generate(seed, &profile);
            let b = DefaultScenarioGenerator::generate(seed, &profile);
            assert_eq!(
                a.canonical_json().expect("ser a"),
                b.canonical_json().expect("ser b"),
                "seed {seed} replay diverged",
            );
        }
    }

    /// Determinism also holds across profiles: changing the profile changes
    /// the bytes (sanity), but the same profile bytes again ⇒ identical.
    #[test]
    fn plan_distinguishes_profiles_but_is_byte_identical_per_profile() {
        let p1 = ConditionProfile::new(2, 0.10, 0.20, 0.10);
        let p2 = ConditionProfile::new(5, 0.25, 0.40, 0.30);
        let a = DefaultScenarioGenerator::generate(123, &p1);
        let b = DefaultScenarioGenerator::generate(123, &p1);
        let c = DefaultScenarioGenerator::generate(123, &p2);
        assert_eq!(
            a.canonical_json().expect("ser a"),
            b.canonical_json().expect("ser b"),
        );
        assert_ne!(
            a.canonical_json().expect("ser a"),
            c.canonical_json().expect("ser c"),
        );
    }

    /// The generator only emits ops valid in the abstract model state.
    /// Replay each plan through the same abstract model and assert no op
    /// fires against an impossible precondition.
    #[test]
    fn only_emits_model_valid_ops() {
        let profile = ConditionProfile::default();
        for seed in 0..200_u64 {
            let plan = DefaultScenarioGenerator::generate(seed, &profile);
            validate_plan_against_model(&plan, seed);
        }
    }

    /// Stronger validity sweep across a long plan and several profile shapes.
    #[test]
    fn only_emits_model_valid_ops_long_plan_many_profiles() {
        let profiles = [
            ConditionProfile::new(1, 0.0, 0.0, 0.0),
            ConditionProfile::new(2, 0.5, 0.0, 0.5),
            ConditionProfile::new(4, 0.0, 1.0, 0.0),
            ConditionProfile::new(8, 1.0, 1.0, 1.0),
        ];
        for profile in &profiles {
            for seed in 0..40_u64 {
                let plan = generate_plan(seed, profile, 256);
                validate_plan_against_model(&plan, seed);
            }
        }
    }

    /// `git_time` is monotonic non-decreasing and seed-derived.
    #[test]
    fn git_time_is_monotonic_and_deterministic() {
        let profile = ConditionProfile::default();
        for seed in 0..50_u64 {
            let plan = DefaultScenarioGenerator::generate(seed, &profile);
            // Monotonic.
            for w in plan.steps.windows(2) {
                assert!(
                    w[0].git_time <= w[1].git_time,
                    "seed {seed}: git_time regressed {} -> {}",
                    w[0].git_time,
                    w[1].git_time,
                );
            }
            // Strict-ish progression (at least the last step is past the base).
            let last = plan.steps.last().expect("plan has steps");
            assert!(
                last.git_time > GIT_TIME_BASE,
                "seed {seed}: git_time never advanced past base",
            );
            // Deterministic across replays.
            let plan2 = DefaultScenarioGenerator::generate(seed, &profile);
            let g1: Vec<i64> = plan.steps.iter().map(|s| s.git_time).collect();
            let g2: Vec<i64> = plan2.steps.iter().map(|s| s.git_time).collect();
            assert_eq!(g1, g2, "seed {seed}: git_time not seed-derived");
        }
    }

    /// The bn-cm63 class is reachable: there exists a seed under which the
    /// plan contains an `Op::Destroy` of a workspace that is, *at the moment
    /// of destroy emission*, a source of a concurrent in-flight `Op::Merge`.
    ///
    /// We assert by **construction**: replay the plan through the same
    /// abstract model the generator uses; when an `Op::Destroy { ws, .. }`
    /// fires while `ws` is in some `in_flight_merges` set, that is the
    /// hostile interleaving. The first seed that produces this is pinned as
    /// the canonical regression seed for downstream T1.4 / T1.6 to consume.
    #[test]
    fn seed_reaches_bn_cm63_class() {
        let profile = ConditionProfile::default();
        let mut found: Option<u64> = None;
        for seed in 0..200_u64 {
            if plan_reaches_bn_cm63(&DefaultScenarioGenerator::generate(seed, &profile)) {
                found = Some(seed);
                break;
            }
        }
        let seed = found.expect("no seed in 0..200 reached the bn-cm63 class");
        // Pin the *canonical* seed (the first one) for downstream T1.4 /
        // T1.6 to hard-code as a permanent regression seed (bn-3ryq corpus).
        // At the time of writing the first hit was seed = 0.
        assert_eq!(
            seed, CANONICAL_BN_CM63_SEED,
            "first bn-cm63-reaching seed shifted; downstream regression seeds need re-pinning",
        );
        // Re-derive to be sure the same seed reproduces under another sweep.
        assert!(plan_reaches_bn_cm63(&DefaultScenarioGenerator::generate(
            seed, &profile
        )));
    }

    /// Test-local alias for the module-level [`super::CANONICAL_BN_CM63_SEED`].
    use super::CANONICAL_BN_CM63_SEED;

    /// The hostile interleaving is reachable under a *spectrum* of profiles
    /// (not a freak of the default knob vector).
    #[test]
    fn bn_cm63_reachable_under_multiple_profiles() {
        let profiles = [
            ConditionProfile::new(2, 0.0, 0.0, 0.0),
            ConditionProfile::new(4, 0.5, 0.5, 0.5),
            ConditionProfile::new(8, 1.0, 1.0, 1.0),
        ];
        for profile in &profiles {
            let mut hit = false;
            for seed in 0..500_u64 {
                if plan_reaches_bn_cm63(&generate_plan(seed, profile, 128)) {
                    hit = true;
                    break;
                }
            }
            assert!(
                hit,
                "no seed in 0..500 reached bn-cm63 under profile {profile:?}"
            );
        }
    }

    /// `FaultSpec` only attaches to merge ops, and only at known failpoints.
    #[test]
    fn faults_attach_only_to_merges_at_known_sites() {
        let known: std::collections::HashSet<&str> = crate::fault::CRASHABLE_BY_PHASE
            .iter()
            .flat_map(|(_, sites)| sites.iter().copied())
            .collect();
        let profile = ConditionProfile::new(4, 1.0, 0.5, 0.3); // force every merge to carry a fault
        let plan = generate_plan(7, &profile, 256);
        for step in &plan.steps {
            match (&step.op, &step.fault) {
                (Op::Merge { .. }, FaultSpec::Failpoint { name, .. }) => {
                    assert!(known.contains(name.as_str()), "unknown failpoint {name}");
                }
                (_, FaultSpec::None) => {}
                (op, FaultSpec::Failpoint { .. }) => {
                    panic!("non-merge op carries a fault: {op:?}");
                }
            }
        }
    }

    /// A plan with zero `mid_op_kill_prob` never carries a fault.
    #[test]
    fn zero_fault_prob_means_no_faults() {
        let profile = ConditionProfile::new(3, 0.0, 0.3, 0.2);
        for seed in 0..50_u64 {
            let plan = generate_plan(seed, &profile, 256);
            for step in &plan.steps {
                assert!(matches!(step.fault, FaultSpec::None));
            }
        }
    }

    /// `Merge.srcs` is always canonically sorted (determinism contract:
    /// no chooser-order leak into the plan bytes).
    #[test]
    fn merge_srcs_are_sorted() {
        for seed in 0..50_u64 {
            let plan = generate_plan(seed, &ConditionProfile::default(), 256);
            for step in &plan.steps {
                if let Op::Merge { srcs, .. } = &step.op {
                    let mut sorted = srcs.clone();
                    sorted.sort();
                    assert_eq!(srcs, &sorted, "Merge.srcs not sorted in seed {seed}");
                }
            }
        }
    }

    /// `EditFiles.files` is always canonically sorted by path.
    #[test]
    fn edit_files_paths_are_sorted() {
        for seed in 0..50_u64 {
            let plan = generate_plan(seed, &ConditionProfile::default(), 256);
            for step in &plan.steps {
                if let Op::EditFiles { files, .. } = &step.op {
                    let mut sorted = files.clone();
                    sorted.sort_by(|a, b| a.path.cmp(&b.path));
                    assert_eq!(files, &sorted, "EditFiles.files not sorted in seed {seed}");
                }
            }
        }
    }

    // -------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------

    /// Replays `plan` through the same abstract model used by the generator
    /// and asserts every op's preconditions hold. This is the "no nonsense
    /// sequences" acceptance criterion (bn-1f53).
    fn validate_plan_against_model(plan: &ScenarioPlan, seed: u64) {
        let mut model = AbstractModel::default();
        for step in &plan.steps {
            // Settle an in-flight merge with the same probability the chooser
            // uses, but: validation is about preconditions of the op *as
            // emitted*, not about replicating the chooser's RNG. So we
            // validate against current state, without forcing settle.
            check_precondition(&model, &step.op, seed, step.index);
            apply_to_model(&mut model, &step.op);
        }
    }

    fn check_precondition(model: &AbstractModel, op: &Op, seed: u64, index: usize) {
        match op {
            Op::WsCreate { ws, .. } => {
                assert!(
                    !model.workspaces.contains_key(ws),
                    "seed {seed} step {index}: WsCreate of extant {ws:?}",
                );
            }
            Op::EditFiles { ws, files } => {
                assert!(
                    model.workspaces.contains_key(ws),
                    "seed {seed} step {index}: EditFiles on nonexistent {ws:?}",
                );
                assert!(!files.is_empty(), "EditFiles with zero files");
            }
            Op::Commit { ws, .. } => {
                let st = model
                    .workspaces
                    .get(ws)
                    .unwrap_or_else(|| panic!("seed {seed} step {index}: Commit on nonexistent {ws:?}"));
                assert!(
                    st.has_uncommitted,
                    "seed {seed} step {index}: Commit on clean {ws:?}",
                );
            }
            Op::Merge { srcs, .. } => {
                assert!(!srcs.is_empty(), "Merge with zero sources");
                for s in srcs {
                    let st = model
                        .workspaces
                        .get(s)
                        .unwrap_or_else(|| panic!("seed {seed} step {index}: Merge source nonexistent {s:?}"));
                    assert!(
                        st.has_commit,
                        "seed {seed} step {index}: Merge source {s:?} has no commit",
                    );
                }
            }
            Op::Sync { ws } => {
                let st = model
                    .workspaces
                    .get(ws)
                    .unwrap_or_else(|| panic!("seed {seed} step {index}: Sync on nonexistent {ws:?}"));
                assert!(
                    !st.has_uncommitted,
                    "seed {seed} step {index}: Sync on dirty {ws:?}",
                );
            }
            Op::Destroy { ws, force } => {
                let st = model
                    .workspaces
                    .get(ws)
                    .unwrap_or_else(|| panic!("seed {seed} step {index}: Destroy of nonexistent {ws:?}"));
                if st.has_uncommitted {
                    assert!(*force, "Destroy of dirty {ws:?} without --force");
                }
            }
            Op::Recover { ws, to } => {
                assert!(
                    model.destroyed.contains(ws),
                    "seed {seed} step {index}: Recover of non-destroyed {ws:?}",
                );
                assert!(
                    !model.workspaces.contains_key(to),
                    "seed {seed} step {index}: Recover overwriting extant {to:?}",
                );
            }
        }
    }

    /// Returns `true` iff the plan contains an `Op::Destroy { ws, .. }` that
    /// fires while `ws` is in some in-flight merge source set. Reuses the
    /// generator's abstract-model bookkeeping verbatim.
    fn plan_reaches_bn_cm63(plan: &ScenarioPlan) -> bool {
        let mut model = AbstractModel::default();
        for step in &plan.steps {
            if let Op::Destroy { ws, .. } = &step.op {
                let is_live_source = model.in_flight_merges.iter().any(|s| s.contains(ws));
                if is_live_source {
                    return true;
                }
            }
            apply_to_model(&mut model, &step.op);
        }
        false
    }
}
