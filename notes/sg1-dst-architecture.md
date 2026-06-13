# SG1 DST Architecture — the trust instrument

- **Bone:** bn-kwm7 (T1.1) — parent sub-goal **bn-3nw1** (SG1: Prime-Invariant
  adversarial DST), goal **bn-142y** (maw v1.0).
- **Status:** Proposed (architecture spec; reviewed/merged by lead). This is
  the document every other T1.x task implements against.
- **Date:** 2026-05-17.
- **Consumes (both already landed on main, in `notes/`):**
  - **SP1 — `notes/adr-dst-execution-model.md`** (bn-imw8): the chosen
    execution model + the `MAW_FP` env-bridge mechanism + the maw-assurance
    reuse map + the determinism contract.
  - **SP2 — `notes/oracle-ab-spec.md`** (bn-3qxi): the implementation-ready
    Oracle A / Oracle B predicates + the mandatory incremental design.
- **Grounds against:** `crates/maw-assurance/{oracle,model,trace}.rs`,
  `crates/maw-core/src/{merge_state,failpoints,refs}.rs`, the existing
  `tests/dst_support/`, `tests/corpus/dst/`, `.github/workflows/dst.yml`,
  and the `just sim-*` recipe family.

---

## 0. What SG1 is, in one paragraph

SG1 is the **hard release gate for v1.0** (bn-3nw1). It is a
continuously-run deterministic simulation that generates random *valid*
op-sequences over N maw workspaces, injects faults (including real process
kills), and after **every** step checks two machine-checked oracles:
**Oracle A** (no committed work is ever lost) and **Oracle B** (state
coherence — must catch the bn-cm63 *class*). It is seed-deterministic, and
the first failing seed is automatically shrunk to a minimal repro. v1.0 does
not ship until this gate is green over a published soak campaign (bn-6308).

**Scope caveat (bn-13g1):** the volume tier drives a *model* of maw's
git-object effects, not maw's production HEAD-movement code, so a green
campaign certifies the ref/content invariants of the modeled operations
under fault injection — not "the Prime Invariant holds in maw's actual
code." See **§7.1** before quoting the soak as evidence for any
HEAD-movement / orphaned-commit guarantee.

This document fixes the seams so the eight remaining SG1 tasks can be
implemented in parallel against stable interfaces.

---

## 1. Execution model — adopt SP1's HYBRID decision verbatim

Per **SP1 §Decision (TL;DR)** the execution model is **HYBRID**, and SG1
adopts it without modification:

| Tier | What it is | Role in SG1 | Determinism (SP1) |
|---|---|---|---|
| **In-process (workhorse, ~all volume)** | Link `maw` (maw-workspaces) + maw-core; drive the merge FSM directly; inject faults via `maw_core::failpoints::set()` | Carries the soak volume **and** the shrinker. ~24 fault-injected op-sequences/sec (≈42 ms each). | **Bit-exact** seed replay — *only after* pinning `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` (SP1 §Prototype). |
| **Faithful subprocess (small curated tier)** | Spawn the real `maw`; crash it with real `SIGKILL` (bn-cm63 chaos pattern); exercise the real recovery path | Proves real crash recovery (zombie/pid-reaping, owner-liveness, partial fsync). Run a **curated** seed set + permanent regression seeds, **not** the soak volume. ≈5 s/op. | **Outcome-deterministic only** — interleaving not replayable; the harness **must reap its own killed children** before asserting recovery (SP1 Finding B). |

**Why both are non-optional (SP1 Rationale, restated as an SG1
constraint):** the spike *already* surfaced two faithful-only behaviours
(Finding A: the `sleep`-window can only widen one phase; Finding B:
a zombie masks owner-liveness) that pure in-proc testing structurally
cannot see. The in-proc tier is the only one fast enough for soak volume
and bit-exact shrinking. Therefore SG1's primary loop is in-process; a
small faithful seed set provides the real-kill assurance and pins the
bn-cm63 + 2026-02-05 lost-commits incidents as **permanent regression
seeds** (bn-3ryq).

**Hard determinism contract for SG1 (SP1 §Failpoint mechanism, point (d)):**
1. The harness **pins `GIT_AUTHOR_DATE` and `GIT_COMMITTER_DATE`** (derived
   deterministically from the seed) for every git write it or maw performs.
   Without this, commit OIDs embed wall-clock time and replay is *not*
   bit-exact (SP1 proved seed 42 produced two different candidate OIDs
   until dates were pinned).
2. In-proc tier ⇒ **bit-exact** replay (same seed ⇒ identical op-sequence,
   condition profile, *and* every intermediate OID).
3. Faithful tier ⇒ **outcome-deterministic only**; the harness documents
   that interleaving is not replayable and **reaps its own killed
   children** (`Child::wait()`) before asserting recovery.
4. Every seed that ever fails is captured as a replay bundle and promoted
   to the permanent regression corpus.

---

## 2. The generator interface — "build once, drive two ways"

This is the load-bearing section. The scenario+condition generator
(bn-1f53, T1.2) is the **single shared substrate** consumed by **both**:

- the **cheap in-proc / model driver** (SG1, this sub-goal), and
- the **real-agent driver** (SG2 / T2.1, bn-…) — the agent benchmark.

The acceptance criterion (bn-kwm7) is that the generator interface is
explicitly drivable by both. The design that satisfies this is a
**driver-agnostic plan stream**: the generator emits an abstract,
serializable sequence of *intents* + a *condition profile*; it never calls
maw, never spawns a process, never links the FSM. Each driver *interprets*
the same plan stream in its own substrate.

### 2.1 The contract (stable types — T1.2 implements, T1.3/T1.4/T2.1 consume)

```
/// Pure, deterministic, side-effect-free. Same seed ⇒ byte-identical
/// (ScenarioPlan). Knows the *abstract* model state only (workspace set,
/// per-ws committed/uncommitted flag, in-flight merges, epoch counter) so
/// it can emit only model-valid ops, yet still reach hostile interleavings.
trait ScenarioGenerator {
    fn generate(seed: u64, profile: &ConditionProfile) -> ScenarioPlan;
}

/// Replayable, serializable, the unit of a regression seed.
struct ScenarioPlan {
    seed: u64,
    profile: ConditionProfile,
    steps: Vec<PlannedStep>,        // model-valid by construction
}

struct PlannedStep {
    index: usize,
    op: Op,                         // the abstract operation
    fault: Option<FaultSpec>,       // seed-selected; see §3
    // deterministic git clock for THIS step (the determinism contract):
    git_time: i64,                  // → GIT_AUTHOR_DATE/GIT_COMMITTER_DATE
}

/// Driver-agnostic operation vocabulary. Mirrors maw's surface AND the
/// trace.rs TraceOp / merge FSM phases so both drivers and the oracle
/// speak one language. (Maps 1:1 onto T1.2's bone op list.)
enum Op {
    WsCreate { ws: WsId, from: BaseRef },
    EditFiles { ws: WsId, files: Vec<FileEdit> },   // content is seed-derived
    Commit    { ws: WsId, msg: Seeded },
    Merge     { srcs: Vec<WsId>, into: Target, destroy: bool },
    Sync      { ws: WsId },
    Destroy   { ws: WsId, force: bool },
    Recover   { ws: WsId, to: WsId },
}

/// All seed-driven (bn-1f53 acceptance). One profile = one knob vector.
struct ConditionProfile {
    concurrency_degree: u8,         // parallel in-flight ops
    mid_op_kill_prob: f64,          // P(a step carries a kill fault)
    overlapping_edit_rate: f64,     // P(two ws edit the same path)
    stale_workspace_rate: f64,      // P(a ws is left un-synced across an epoch bump)
}

/// What driver gets handed. The generator is constructed ONCE; the two
/// implementations of this trait are the "two ways".
trait ScenarioDriver {
    type Outcome;
    fn drive(&mut self, plan: &ScenarioPlan) -> Self::Outcome;
}
```

### 2.2 Why this satisfies "build once, drive two ways"

| Concern | In-proc / model driver (SG1) | Real-agent driver (SG2 / T2.1) |
|---|---|---|
| Consumes | the **same** `ScenarioPlan` | the **same** `ScenarioPlan` |
| `Op::Merge{..}` becomes | a direct call into the merge FSM (`merge_state` + `src/merge/*`) | the agent being benchmarked is *asked* to perform the equivalent maw workflow; the harness records what it actually did |
| `FaultSpec` becomes | `failpoints::set()` (in-proc) | `MAW_FP=...` env on the spawned `maw` + real `SIGKILL` (SP1 env bridge) |
| `git_time` becomes | env pin around the in-proc git op | env pin on the spawned process |
| Oracle (A & B) | run in-proc against `&dyn GitRepo` | run via subprocess `git` capture |
| Determinism | bit-exact | scenario-deterministic (the *plan* is identical; the agent's realization and any real kill are not) |

The generator has **zero** knowledge of which driver runs it. SG2's agent
benchmark (T2.1) is then "the same adversarial scenarios, but a real agent
must keep maw coherent under them" — apples-to-apples with SG1 because the
scenario stream is literally the same bytes for a given seed. **This is the
"trust instrument ⇒ benchmark" leverage the v1.0 plan depends on.**

### 2.3 Validity & hostility (bn-1f53 acceptance, restated as an interface
obligation)

The generator MUST:
- emit only ops valid in the *current abstract model state* (no
  destroy-of-nonexistent, no merge-of-no-sources) — it tracks a tiny model
  (the abstract `model.rs` FSM, SP1 reuse map: "cross-check against, not
  build from"); **and**
- still be able to reach **hostile interleavings**: concurrent
  merge+destroy of the same ws (the bn-cm63 repro), stale-ws-into-merge,
  overlapping edits forcing diff3, kill at every FSM boundary.
- be **byte-identical for a given (seed, profile)** across runs and across
  machines (no wall-clock, no HashMap iteration order, no PID, no
  filesystem order in the plan).

---

## 3. Fault-injection layer (integration point for bn-263u, T1.5)

bn-263u owns the implementation; this section fixes the seam the generator
and both drivers use, exactly per **SP1 §Failpoint external-control
mechanism**.

```
enum FaultSpec {
    None,
    Failpoint { name: String, action: FailpointAction },  // e.g. FP_COMMIT_BETWEEN_CAS_OPS
    ProcessKill { at: Op /*phase*/, signal: Signal },      // faithful tier only
}
```

- **In-proc driver:** `FaultSpec::Failpoint` ⇒ `maw_core::failpoints::set(name,
  action)` immediately before the targeted FSM step; cleared after. No new
  mechanism (SP1: registry is 100% reusable in-proc).
- **Faithful driver:** `FaultSpec::Failpoint` ⇒ exported as
  `MAW_FP="NAME=action;..."` on the spawned `maw` (the **env bridge** bn-263u
  adds to `failpoints.rs` behind `--features failpoints`, parser
  `parse_env_spec`). `FaultSpec::ProcessKill` ⇒ real `kill -9 -<pgid>` once
  the env bridge has *deterministically* driven the process to the boundary
  (SP1 Finding A: the `sleep`-window only widens one phase, so the env
  bridge is the primary deterministic crasher; sleep retained only as a
  fallback for the widened phase).
- **Prerequisite (SP1 §3, bn-1cww):** `src/merge/commit.rs::fp_commit` must
  not be `const fn` or `--features failpoints` won't compile and the
  COMMIT-phase failpoints (incl. `FP_COMMIT_BETWEEN_CAS_OPS`, the single
  most dangerous boundary) are dead code. SP1 applied the one-line fix in
  the bn-imw8 workspace; **bn-263u depends on bn-1cww landing on main.**
- **Faithful recovery is a bounded retry that reaps its own children**
  (SP1 Finding B) — not a single call; a zombie reads as alive via
  `/proc/<pid>` until reaped.

Fault sites of record (the FSM boundaries the gate must hammer), from
`merge_state.rs::MergePhase` + `src/merge/{prepare,build_phase,validate,
commit}.rs`: `Prepare → Build → Validate → **Commit** → Cleanup`. COMMIT is
the dangerous one: a single atomic two-ref CAS (`refs/manifold/epoch/
current` + `refs/heads/<branch>`) straddled by `FP_COMMIT_BEFORE_BRANCH_CAS`
/ `FP_COMMIT_BETWEEN_CAS_OPS` / `FP_COMMIT_AFTER_EPOCH_CAS`, with the
idempotent `recover_partial_commit_with_branch_base` as the recovery path.

---

## 4. Oracle A & Oracle B integration points (SP2)

SG1 adopts **SP2's predicates verbatim** (`notes/oracle-ab-spec.md`).
Restated as the integration contract; implementation is bn-1z8q (T1.3) and
bn-3ji6 (T1.4).

### 4.1 Oracle A — *no committed work lost* (SP2 §2)

> **Oracle A holds at a state iff `W ⊆ U(F(state))`** — every blob ever
> authored by any workspace is still reachable from the frontier
> `F = {refs/heads/main} ∪ {recovery refs} ∪ {epoch refs} ∪ {per-ws base
> epoch} ∪ {materialized ws state} ∪ {extant ws tips}`.

Non-negotiables SG1 inherits from SP2:
- **Content (blob) reachability, NOT commit-graph ancestry** (SP2 §0). An
  ancestry oracle false-positives on *every* merge — empirically proven in
  the SP2 spike. The misnamed `oracle.rs::check_g1_reachability`
  (commit-ancestry) is the proven-wrong model; T1.3 re-specs G1 to blob
  reachability or adds a new Oracle-A check and demotes G1.
- **Incremental design is MANDATORY for T1.3** (SP2 §2.1): maintain `W`
  (witness blob set) + `U` (live reachable-blob set) as **mutable harness
  state across steps**; never recompute from scratch (naive is O(N²) ≈ days
  at 1e6 steps). Budget: **amortized ≤ 1 ms/step** at 1e6 steps; the full
  `git rev-list` is paid lazily only on a suspected violation (run stops &
  shrinks on first real one).
- Witness contribution uses the **workspace delta** vs
  `refs/manifold/epoch/ws/<ws>`, not the full tip tree, to bound `|W|`.

### 4.2 Oracle B — *state coherence* (SP2 §3) — catches the bn-cm63 class

> **Oracle B holds iff B1∧B2∧B3∧B4.** B1 no-dangling-oplog-head;
> B2 owned-ref symmetry; B3 merge-state coherence; B4 recovery
> well-formed. (Pure predicate over `(refs, ws-dirs, merge-state.json)`.)

Non-negotiables SG1 inherits from SP2:
- B1 is **exactly the bn-cm63 class** (dangling `refs/manifold/head/<ws>`
  with no ws and no live merge). Oracle A alone would miss bn-cm63 entirely
  (it was a coherence bug, not work-loss). Oracle B is *why* the SG1 gate
  can catch the bn-cm63 *class* — the bone's hard requirement.
- The `LiveMergeSources` guard MUST reuse production logic
  `maw_core::merge_state::{MergeStateFile, staleness}` with
  `DEFAULT_STALE_AFTER_SECS` (SP2 §3.1) — `Staleness::Live` ⇒ protect,
  `Orphaned`/`Indeterminate` ⇒ no protection. Identical to
  `ref_gc.rs::live_merge_source_names`; do **not** re-derive (oracle and GC
  guard must never diverge).
- B2 reuses `maw_core::refs::workspace_owned_refs`. B4 batches object-type
  checks via `git cat-file --batch-check`. Recovery refs are **exempt**
  from B1/B2 (they must survive destroy).
- Oracle B is **O(#refs + |sources|), independent of step count** (SP2
  §3.2) — no incremental design needed.

### 4.3 Where the oracles plug in (the harness seam)

```
loop over plan.steps:
    pre  = capture()                     // AssuranceState / StateSnapshot
    apply(step.op, step.fault)           // via the active ScenarioDriver
    post = capture()
    OracleA.check_incremental(pre, post) // ΔF update of W,U  (SP2 §2.1)
    OracleB.check(post)                  // B1..B4            (SP2 §3)
    trace.record(TraceEntry{op, pre, post, invariants})
    if violation: write_failure_bundle(); stop; shrink()
```

- `capture()` reuses `oracle.rs::capture_state` / `trace.rs::capture_state`
  (~85–90% reusable per SP1 reuse map). **Perf carve-out (SP1 gap):**
  `capture_state` shells out to `git` ×13; for the in-proc soak tier add
  `capture_state_via(&dyn GitRepo)` to avoid 13 forks/iteration
  (correctness unchanged; this is the SP1-identified net-new item).
- **Independent-verifier carve-out is preserved** (SP2 §6 "Both"): the
  oracle uses git **CLI** on the bare `repo.git`, deliberately *not* gix —
  see the `TODO(gix): assurance carveout` comments in `oracle.rs`. The
  oracle must not share gix code paths with the system under test, or a gix
  bug could hide itself.

---

## 5. Seed / determinism contract (consolidated)

Single source of truth for every SG1 task. (Derived from SP1
§Failpoint-mechanism point (d) and the SP2 replayability requirements.)

1. **One seed ⇒ one `ScenarioPlan`** (steps + condition profile + per-step
   `git_time`), byte-identical across runs and machines (bn-1f53
   acceptance; bn-32k3 verifies).
2. **Pinned git clock:** every git write (in-proc or via spawned maw) runs
   with `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` = `step.git_time`
   (seed-derived). Mandatory for bit-exact OIDs (SP1 proof).
3. **In-proc tier:** bit-exact — same seed ⇒ identical op-sequence *and*
   every intermediate OID. This is what makes the shrinker tractable.
4. **Faithful tier:** outcome-deterministic only; interleaving not
   replayable; harness reaps its own killed children before asserting
   recovery (SP1 Finding B).
5. **No nondeterminism sources in harness code:** no `HashMap` iteration in
   plan/trace output (use `BTreeMap`, already the `trace.rs` choice), no
   PID/host/wall-clock in the plan, no filesystem-order dependence.
6. **Failing seed ⇒ permanent regression seed** (feeds bn-3ryq), with the
   bn-cm63 and 2026-02-05 lost-commits incidents seeded *a priori*.

---

## 6. Shrinker (integration point for bn-32k3, T1.6)

Bit-exact in-proc replay (§5.3) is what makes shrinking tractable — SP1
explicitly lists the shrinker as net-new code the in-proc model enables.

- **Input:** a failing `ScenarioPlan` + the tripped oracle + violation.
- **Strategy:** delta-debugging over `plan.steps` — remove/merge steps and
  fault, replay **in-proc** (bit-exact, ~42 ms/iter so thousands of
  shrink iterations are cheap), keep the reduction iff the *same* oracle
  trips with the *same* violation class. Also minimize the
  `ConditionProfile` (lower concurrency, fewer faults) and the fault site.
- **Output:** a minimal `ScenarioPlan` + `minimized_replay_command`,
  written into the existing `FailureBundle`
  (`tests/dst_support/mod.rs::FailureBundle` already has
  `minimized_replay_command`, `trace`, `violations`, `snapshots` fields —
  bn-32k3 fills the minimization, the scaffolding exists).
- The minimal repro is promoted to the regression corpus (bn-3ryq) so the
  exact bug can never silently return.
- Faithful-tier failures shrink in-proc *first* (reproduce the logical
  scenario cheaply), and the faithful kill is re-applied only to confirm.

---

## 7. CI wiring (integration point for bn-1gp4, T1.7)

Build on the existing CI substrate, do not replace it:
`.github/workflows/dst.yml`, the `just sim-*` recipe family
(`sim-run`, `sim-run-print`, `sim-replay-workflow`, `sim-replay-action`,
`sim-shrink-bundle`, `sim-inspect`), `tests/dst_support/` bundle/replay
scaffolding, `tests/corpus/dst/`, and the `DST_ARTIFACT_DIR` artifact
upload — all present and reusable (~60% harvest per SP1 reuse map; replace
only the hand-written crash-simulation core with the real fault layer).

- **Bounded per-commit (PR + push to main):** in-proc tier only, a fixed
  small seed budget + the **entire permanent regression corpus** (bn-3ryq),
  hard wall-clock cap. Must be fast enough to gate every PR. Replaces the
  current `WORKFLOW_DST_TRACES`/`ACTION_DST_TRACES=12` step with the new
  generator+oracle loop.
- **Nightly soak:** large seed budget, in-proc tier for volume + the
  curated **faithful** seed set (real kills). Failing seeds auto-shrunk,
  bundle uploaded as the existing `maw-dst-artifacts`, replay command in
  the job summary (mechanism already wired in `dst.yml`).
- **Faithful tier needs a dedicated build with `--features failpoints`**
  (SP1) — a separate CI job/build; the default release build stays clean
  and zero-overhead (`fp!()` compiles to nothing without the feature).
- **Gate semantics:** any oracle violation = red CI = release-blocking for
  v1.0 (bn-3nw1 is the hard gate). Nightly evidence accumulates into the
  published zero-violation soak record (bn-6308).

---

## 7.1 Coverage boundary — what the in-proc soak does and does NOT exercise (bn-13g1)

**Read this before quoting the soak campaign as evidence.** The in-proc
tier is the *volume* tier, and it earns that volume by driving a **model of
maw's git-object effects**, not maw's production workspace/HEAD-management
code. That trade is deliberate (it is what makes 1e8 op-steps tractable),
but it bounds what a green campaign certifies. Established 2026-06-13 by
tracing the harness end to end while asking "would the soak have caught the
orphaned-commit class we fixed in bn-29z8/1qtj/20sa/8flz?" The answer is
**no, and not narrowly** — for three structural reasons:

1. **The action vocabulary cannot express the main vector.** The generator
   `Op` enum (`crates/maw-scenario/src/lib.rs`) is exactly `WsCreate`,
   `EditFiles`, `Commit`, `Merge`, `Sync`, `Destroy`, `Recover`. There is
   **no `Advance` op**, so `maw ws advance` — the bn-8flz orphan vector (an
   unguarded HEAD-mover that overwrote committed-ahead work while printing
   "advanced successfully") — is ungenerable at any seed or step count.

2. **The in-proc driver reimplements ops with raw git plumbing.** It does
   not call production `maw-core` / `maw-git`:
   - `do_merge` (`crates/maw-assurance/src/in_proc.rs`) synthesizes a merge
     with `git commit-tree` + `update-ref` (the commit message is literally
     `-> default (in-proc-driver)`); it bumps `refs/heads/main` and
     `refs/manifold/epoch/current` to the last source's tip and optionally
     destroys sources. It **never replays siblings and never calls
     `set_head`.**
   - `do_sync` is a literal no-op ("Sync is a no-op at this modelling level
     (no per-ws epoch staleness representation)").

   The orphaned-commit class lived precisely in the code these stubs stand
   in for: `maw_git::set_head` after the empty-replay walk, the
   merge-triggered sibling auto-rebase, and the checkout fast-forward path.
   Because the model never moves a *committed* sibling's HEAD, it cannot
   orphan one — Oracle A/B run against an object graph the buggy code never
   produced. **Re-pinning the soak binary at a post-fix SHA does not change
   this: the gap is the harness, not the pin.**

3. **Sequential, single-process.** `drive()`/`drive_fast()` apply plan
   steps one at a time in one process. The live incidents were
   multi-process races (a concurrent agent merging while a worker's commit
   sat in a sibling). No inter-process `set_head` race is modeled.

**So scope the published claim precisely.** A clean 1e8-op-step campaign
certifies the **ref/content invariants of the modeled operations under
fault injection** — which is real and valuable: Oracle B catches the
bn-cm63 dangling-ref class (the class the instrument was built for) and
Oracle A catches irreversibly lost committed *content*. It does **not**
certify "the Prime Invariant holds in maw's actual HEAD-movement code."
Do not let SG5 (bn-3ctu) or the bn-2yzz gate row imply the soak covers
HEAD-movement or concurrent-agent orphaning.

**Where the orphaned-commit class actually is covered** (cite these, not
the soak, for that class):

- **Production-code regression tests** that drive the real `maw` binary:
  - `tests/advance_orphan_regression_bn_8flz.rs` — `ws advance` preserves
    committed-ahead work; sync routes committed-ahead through the guarded
    rebase path; a source-scan guard asserts no raw `git checkout --detach`
    HEAD-mover shell-outs survive in production.
  - `tests/rebase_never_abandon_bn_20sa.rs` — the `set_head` never-abandon
    guard (uses failpoints to force the empty-walk while HEAD is ahead),
    the `set_head` reflog trail, and `rebase` oplog visibility for both
    explicit and sibling auto-rebase.
- **Field dogfooding** (the sigil bn-3d4a investigation + the live maw-repo
  reproduction) that originally surfaced the class, and the **always-loud
  guards** (bn-20sa never-abandon CAS + reflog; bn-8flz single native
  guarded HEAD-mover choke-point). These — not the volume soak — are the
  primary evidence for the orphaned-commit class.

**Closing the gap in the soak itself** (optional, real T-work, tracked
separately — would force a fresh campaign): add an `Advance` op; make
`do_merge`/`do_sync` invoke production `maw-core` merge/sync (real
`set_head` + sibling auto-rebase) instead of the plumbing model; and ideally
model an interleaved multi-process `set_head` race. This is **not** a v1.0
blocker given the regression-test + guard coverage above; it is the path to
a *higher-confidence* soak. Tracked as **bn-2byw** (follow-up to bn-13g1).

---

## 8. Enumerated remaining SG1 tasks + proposed order

SG1 children (parent bn-3nw1). SP1 (bn-imw8) and SP2 (bn-3qxi) are **done**;
this doc (bn-kwm7, T1.1) is the third. Proposed order and rationale:

| # | Bone | Task | Depends on (effective) | Why this slot |
|---|---|---|---|---|
| 0 | bn-1cww | const-fn fix on `fp_commit` | — | **Unblocks everything failpoints.** SP1 fixed it in its workspace; must land on main. Lands with/before T1.5. |
| 1 | **bn-kwm7** | **T1.1 architecture (this doc)** | bn-imw8 (SP1) | done after this commit; gate for T1.2. |
| 2 | bn-1f53 | T1.2 generator + condition profile (shared substrate) | bn-kwm7 | **Highest leverage.** §2 contract. Unblocks T1.3, T1.4, T2.1. Do first. |
| 3a | bn-1z8q | T1.3 Oracle A (incremental W/U) | bn-1f53 | Parallel with 3b/3c. SP2 §2 + mandatory incremental design. |
| 3b | bn-3ji6 | T1.4 Oracle B (B1–B4) | bn-1f53 | Parallel with 3a/3c. SP2 §3; reuse `merge_state`/`refs`. |
| 3c | bn-263u | T1.5 fault layer (`MAW_FP` env bridge + real kills) | bn-1f53, **bn-1cww** | Parallel with 3a/3b. SP1 env bridge; in `doing`. |
| 4 | bn-32k3 | T1.6 determinism guarantee + shrinker | bn-1f53, bn-1z8q, bn-3ji6, bn-263u | Needs generator + both oracles + faults to shrink against. §6. |
| 5 | bn-1gp4 | T1.7 CI: bounded per-commit + nightly soak | bn-32k3 (+ all above) | §7. Wires the assembled loop into `dst.yml`/`just sim-*`. |
| 6 | bn-3ryq | T1.8 permanent regression-seed corpus | bn-32k3, bn-1gp4 | Needs the shrinker + CI to ingest minimized seeds; seed bn-cm63 + 2026-02-05 a priori. |
| 7 | bn-6308 | T1.9 soak campaign + published zero-violation evidence | all of the above | **The release gate's evidence.** Long-running; the v1.0 cut depends on it. |

**Critical path:** T1.1 → **T1.2** → {T1.3 ∥ T1.4 ∥ T1.5} → T1.6 → T1.7 →
T1.8 → T1.9. T1.2 is the single biggest unblock; the three oracle/fault
tasks fan out in parallel; everything reconverges at the shrinker.

---

## 9. Impact on downstream bones & the v1.0 plan

- **bn-1f53 (T1.2, direct dependent):** its `ScenarioGenerator` /
  `ScenarioPlan` / `Op` / `ConditionProfile` / `ScenarioDriver` types are
  **fixed by §2 of this doc** and are the cross-SG contract. Two concrete
  refinements vs the bone text: (a) the generator must emit a *plan stream*
  (no maw calls) so SG2/T2.1 can reuse it unchanged — make this explicit in
  the bone; (b) `PlannedStep` carries a seed-derived `git_time` — the
  determinism contract is not optional and must be tested by T1.6.
- **bn-1z8q (T1.3):** must re-spec/replace `check_g1_reachability`
  (commit-ancestry is the *proven-wrong* model — SP2 §0) and implement the
  **mandatory incremental** W/U design (SP2 §2.1). This is a heavier task
  than "implement G1" implies; flag size accordingly.
- **bn-3ji6 (T1.4):** must call `maw_core::merge_state::staleness` and
  `refs::workspace_owned_refs` (no re-derivation) and agree with `maw
  doctor` ground truth — add that as an explicit acceptance self-test.
- **bn-263u (T1.5):** add an explicit **depends-on bn-1cww**; without the
  const-fn fix on main, COMMIT failpoints don't compile.
- **bn-1cww:** must be tracked as a real SG1-blocking task and land before
  /with T1.5 (it currently lives only as a fix in the SP1 workspace).
- **v1.0 plan (bn-142y):** **no strategic change.** This doc reinforces the
  posture: the SG1 instrument is *also* the SG2 benchmark substrate (§2.2,
  "build once, drive two ways") — that leverage is now a concrete typed
  contract, not an aspiration. SP1 already produced two faithful-only
  findings + a latent build-breaker on the exact failpoints the gate
  depends on, which is direct evidence the gate is necessary and the
  hybrid model is correctly scoped.

---

## 10. Acceptance criteria — status (bn-kwm7)

| Criterion (bn-kwm7) | Status | Evidence |
|---|---|---|
| Doc reviewed and committed under `notes/` | **MET (committed; review = lead)** | `notes/sg1-dst-architecture.md`, committed in ws/bn-kwm7. |
| References SP1's chosen execution model | **MET** | §1 adopts SP1's HYBRID verbatim (in-proc workhorse + faithful tier); §3 cites the `MAW_FP` env bridge + bn-1cww prereq; §5 the determinism contract. |
| References SP2's oracle predicates | **MET** | §4.1 Oracle A = blob-reachability `W ⊆ U(F)` (not ancestry); §4.2 Oracle B = B1–B4 with the `staleness` guard; incremental design mandated. |
| Generator interface drivable by **both** the cheap model driver (SG1) **and** the real-agent driver (SG2/T2.1) — "build once, drive two ways" | **MET** | §2 defines a driver-agnostic `ScenarioPlan` stream + `ScenarioDriver` trait; §2.2 table shows both drivers consume the *same bytes* per seed. |
| Defines fault-injection layer | **MET** | §3, seamed to bn-263u + the SP1 env bridge. |
| Defines Oracle A & B integration points | **MET** | §4.3 harness seam + capture reuse + verifier carve-out. |
| Defines seed/determinism contract | **MET** | §5 (consolidated, single source of truth). |
| Defines the shrinker | **MET** | §6, built on bit-exact in-proc replay + existing `FailureBundle`. |
| Defines CI wiring | **MET** | §7, built on existing `dst.yml` / `just sim-*` / `tests/dst_support`. |
| Enumerates remaining SG1 tasks + order | **MET** | §8 table + critical path. |
| Coverage boundary scoped honestly (bn-13g1) | **MET** | §7.1 + §0 scope caveat: the in-proc volume tier drives a model, not production HEAD-movement code (no `Advance` op; `do_merge`/`do_sync` are plumbing models; single-process). Orphaned-commit class covered by `tests/advance_orphan_regression_bn_8flz.rs` + `tests/rebase_never_abandon_bn_20sa.rs` + field/guards, not the soak. |

**Overall: PASS** (pending lead review/merge).
