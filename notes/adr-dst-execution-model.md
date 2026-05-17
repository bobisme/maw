# ADR: DST Execution Model + Failpoint External-Control Mechanism

- **Bone:** bn-imw8 (SP1, spike) — parent goal bn-142y (maw v1.0), sub-goal bn-3nw1 (SG1 DST).
- **Status:** Proposed (spike output — informs bn-263u, bn-kwm7; not production code).
- **Date:** 2026-05-17
- **Decision owners:** lead reviews; downstream implementers are bn-263u (fault layer) and bn-kwm7 (SG1 architecture doc).

---

## Decision (TL;DR)

1. **Execution model: HYBRID, with the in-process model as the workhorse.**
   - **In-process driver (default, ~all volume):** link `maw` (maw-workspaces)
     + maw-core, drive the merge FSM directly, inject faults via the existing
     `maw_core::failpoints::set()`. Bit-exact seed replay, ~24 fault-injected
     op-sequences/sec measured (≈42 ms each, git-backed), zero new mechanism.
   - **Subprocess driver (small, faithful tier):** spawn the real `maw`, crash
     it with a real `SIGKILL` (the bn-cm63 chaos pattern), exercise the real
     recovery path. This is the only thing that can prove crash recovery for
     real (zombie/pid-reaping, owner-liveness, partial fsync, OS-level kill).
     Measured ≈5 s/op — run a curated seed set + permanent regression seeds,
     **not** the soak volume.
2. **Failpoint external-control mechanism: ADD THE ENV BRIDGE to
   `crates/maw-core/src/failpoints.rs`** (gated behind `--features
   failpoints`). The in-process model does **not** need it, but the faithful
   subprocess model **does**: today it can only reliably crash the *one* phase
   that a `sleep` validation widens (proven below — `kill_phase=build` never
   hits its window). A `MAW_FP="NAME=action;..."` parser in `check()` gives
   deterministic in-binary fault points at every boundary and removes the
   sleep-window race. Spec it in bn-263u.
3. **Prerequisite bug (found by this spike, fix included in this workspace):**
   `src/merge/commit.rs::fp_commit` was `const fn` while its
   `--features failpoints` body calls the fallible `fp!()` macro →
   **`cargo build --features failpoints` does not compile on current Rust**
   (E0015/E0658/E0493/`Try`/`FromResidual` not const). The COMMIT-phase
   failpoints (incl. `FP_COMMIT_BETWEEN_CAS_OPS`, the single most dangerous
   boundary) were therefore *uncompilable dead code* — no harness could have
   used them. One-line fix (drop `const`) applied here; tracked as
   **bn-1cww**; bn-263u depends on it.

---

## Context

SG1's hard release gate (bn-142y) is a continuously-run adversarial DST with
two machine-checked oracles (work-loss + state-coherence). Every other SG1
task implements against the execution model chosen here. The open unknowns
the spike had to resolve:

- Link maw-core as a library (fast, deterministic, but can it test real
  process-kill recovery?) **vs** spawn real `maw` (faithful to crash reality,
  slow, hard to make deterministic) **vs** hybrid.
- The failpoint external-control mechanism. Today `FP_*` can ONLY be set via
  `failpoints::set()`, are compiled out without `--features failpoints`, and
  have **no env/IPC bridge**. The chaos session that found bn-cm63 had to
  widen the window with `sleep 5` + external `SIGKILL` because there is no
  deterministic in-process fault control in the *shipped* binary.
- How much of `crates/maw-assurance` is reusable.

The merge FSM under test (`crates/maw-core/src/merge_state.rs`,
`src/merge/{prepare,build_phase,validate,commit}.rs`):
`Prepare → Build → Validate → Commit → Cleanup → Complete | Aborted`.
The dangerous boundary is COMMIT: `src/merge/commit.rs` does a single atomic
two-ref CAS (`refs/manifold/epoch/current` + `refs/heads/<branch>`) with
`FP_COMMIT_BEFORE_BRANCH_CAS` / `FP_COMMIT_BETWEEN_CAS_OPS` /
`FP_COMMIT_AFTER_EPOCH_CAS` straddling it, plus an idempotent
`recover_partial_commit_with_branch_base`.

---

## Prototype & Evidence

Both models are implemented and run end-to-end, seed-reproducibly, under
`ws/bn-imw8/spike/` (standalone manifest; **not** wired into the parent
workspace or shipped).

### In-process (`spike/src/inproc.rs`)

Builds a real git repo, drives the **real** `commit::run_commit_phase_with_branch_base`,
injects a seed-selected `FP_COMMIT_*` fault via `failpoints::set(...,
FailpointAction::Error(...))`, runs the **real**
`recover_partial_commit_with_branch_base`, then checks **Oracle G3**
(epoch ref forward-only → candidate, never backward/unrelated) and
ref-atomicity (epoch == branch).

- 6 seeds (1,2,3,7,42,99) → all **PASS**; covers all three COMMIT faults.
- **Determinism:** same seed ⇒ same fault selection every run (seed 7 ⇒
  always `FP_COMMIT_BETWEEN_CAS_OPS`; seed 42 ⇒ always
  `FP_COMMIT_BEFORE_BRANCH_CAS`).
- **Bit-exact OID replay** is achievable **only after pinning
  `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE`** — without that, commit OIDs embed
  wall-clock time and seed replay is *not* bit-exact (proven: seed 42 gave
  two different candidate OIDs across runs until dates were pinned, then
  identical across 3 runs). **This is a determinism-contract requirement for
  SG1.**
- **Throughput:** 20 fault-injected op-sequences in **0.84 s** (~42 ms each;
  includes real `git` setup subprocesses — a pure in-memory variant is
  faster).

### Subprocess / faithful (`spike/src/subprocess.rs`)

`git init` + `maw init` → real v2 bare repo; `maw ws create rz` + commit;
`[merge.validation] command = "sleep 5"`; `setsid maw ws merge` detached;
poll `<root>/.manifold/merge-state.json` until the seed-selected phase; real
`kill -9 -<pgid>`; bounded recovery retry; **Oracle = Prime Invariant** (the
committed `rz` work is present in `default` after recovery + epoch resolvable).

- Seeds landing on `kill_phase=validate` (3, 8) → **PASS**: real SIGKILL,
  real recovery, no work lost.
- **Finding A — sleep-window can only hit one phase.** Seeds with
  `kill_phase=build` (2,5,11) **never observe the window**: BUILD is sub-20 ms
  and only VALIDATE is widened by `sleep`. The bn-cm63 pattern is structurally
  limited to the widened phase. ⇒ direct motivation for the env bridge.
- **Finding B — real SIGKILL leaves a zombie that masks liveness.** First
  recovery retries refused with *"merge already in progress … owned by a
  running process"* even though the owner pid was provably dead (`ps -p`
  empty), host+boot-id matched, and no procs remained. Root cause isolated:
  `MergeStateFile::owner_liveness()` reads `/proc/<pid>`; a **zombie still
  reads as alive** until reaped. Reaping the harness's own child
  (`Child::wait()`) makes recovery succeed on **attempt 1**. Not a maw bug —
  but a hard requirement that the **faithful harness must reap its children**
  (and/or back off seconds). The in-process model **cannot surface this
  class** (no separate owner process, no `/proc` liveness in play).
- **Throughput:** ≈**5.16 s per op** (mandatory `sleep` window + multi-second
  reaping/backoff). ≈**120× slower per sequence** than in-process.

---

## Rationale

| Criterion | In-process | Subprocess (faithful) |
|---|---|---|
| Determinism | Bit-exact (with pinned git dates) | Outcome-deterministic only; interleaving/instruction non-deterministic by nature |
| Throughput | ~24 seq/s (42 ms) | ~0.2 seq/s (5 s) — **~120×** slower |
| Fault control | Total & precise (`failpoints::set`) at every `fp!` site | Today: only the sleep-widened phase; SIGKILL anywhere but imprecise |
| Crash fidelity | Simulated (clean unwind / panic / abort in-proc) | **Real** OS kill: zombies, pid reuse, owner-liveness, partial fsync, group kill |
| New mechanism needed | **None** | **Env bridge** (to break the sleep-window limit) |
| Oracle reuse | maw-assurance in-proc (no shell) | maw-assurance via subprocess capture |

Neither model alone is sufficient: in-process gives the **soak volume +
shrinkable bit-exact repros** the gate needs but *cannot* prove real
crash-recovery (Findings A/B exist only in the faithful tier); subprocess
proves crash-recovery for real but is far too slow and only outcome-
deterministic. ⇒ **Hybrid**: in-process carries the volume and the shrinker;
a curated faithful seed set (incl. bn-cm63 + the prior lost-commits incident
as permanent regression seeds) provides the real-kill assurance.

---

## maw-assurance reuse map

`crates/maw-assurance/` (lib `maw_assurance`, feature `assurance`;
`oracle.rs` 1428 LoC, `model.rs` 644, `trace.rs` 732). Verdict:
**~75% reusable as-is for the DST oracle layer; the model & some plumbing
need work.**

| Component | Reuse | Notes / gaps |
|---|---|---|
| `oracle.rs` — `AssuranceState`, `capture_state`, `check_g1..g6`, `check_all` | **~90% — reuse directly** | Exactly the Oracle A/B surface. **Gap:** `capture_state` shells out to `git` (`Command::new("git")` ×13) — fine for subprocess tier; for the in-proc soak tier, add a `capture_state_via(&dyn GitRepo)` to avoid 13 forks/iteration (perf, not correctness). G2 needs the recovery-ref convention bn-263u will exercise. |
| `trace.rs` — `TraceLogger`, `TraceEntry`, `TraceOp`, JSONL schema, `InvariantResults` | **~85% — reuse** | JSONL trace/replay format is ready. Same `git`-shelling note. `TraceOp` covers Prepare..Cleanup+Destroy+Recover — extend with the explicit COMMIT sub-steps the in-proc driver exercises. |
| `model.rs` — Stateright model of the FSM (G1/G3/G4) | **~50% — reference, don't reuse verbatim** | Good as the abstract spec & for `stateright` model-checking, but it is an *abstract* model (`Oid=u64`), not a driver. SG1's generator should be cross-checked against it, not built from it. Feature-gated behind `stateright`. |
| Existing harnesses `tests/dst_harness.rs` (1810), `tests/workflow_dst.rs`, `tests/action_workflow_dst.rs`, `tests/dst_support/mod.rs` | **~60% — harvest patterns** | Already seed-driven (`StdRng`), already subprocess (`maw_bin()`), already emits failure/replay bundles (`write_failure_bundle`, replay command, artifact dir). **Gap:** they *simulate* crashes by hand-writing `merge-state.json` (their own header admits "failpoint instrumentation … not yet wired") — exactly what bn-263u replaces with the env bridge + real kills. Reuse the bundle/replay scaffolding and seed plumbing; replace the crash-simulation core. |
| Failpoint registry `failpoints.rs` (`set/clear/check`, `FailpointAction`) | **100% in-proc** | Sufficient for the in-process model with **no change**. Subprocess model needs the **env-bridge addition** (below). |

Net new code SG1 must write (not in maw-assurance today): the
**scenario/condition generator** (shared with SG2 — bn-kwm7's interface), the
**fault-injection layer** (bn-263u), the **shrinker** (bit-exact in-proc
replay makes this tractable), the **env bridge** in `failpoints.rs`, and
`capture_state_via` for the in-proc perf path.

---

## Failpoint external-control mechanism (spec for bn-263u)

**Add to `crates/maw-core/src/failpoints.rs`, gated behind
`#[cfg(feature = "failpoints")]`:**

- A parser `parse_env_spec(&str) -> Vec<(String, FailpointAction)>` for
  `MAW_FP="FP_COMMIT_BETWEEN_CAS_OPS=abort;FP_VALIDATE_AFTER_CHECK=error:msg;FP_BUILD_AFTER_MERGE_COMPUTE=panic;FP_CLEANUP_*=sleep:5000"`.
  Actions map 1:1 to the existing `FailpointAction` (`off`, `error[:msg]`,
  `panic[:msg]`, `abort`, `sleep:<ms>`).
- A one-time loader (e.g. in `check()` via a `OnceLock`, or an explicit
  `init_from_env()` called at maw startup) that seeds `REGISTRY` from
  `MAW_FP` so the **shipped subprocess** honours it.
- The shipped binary must be **built with `--features failpoints`** for the
  faithful tier (a dedicated CI/test build; default release stays clean &
  zero-overhead — `fp!()` still compiles to nothing without the feature).
- `abort`/SIGKILL semantics: keep real `kill -9 -<pgid>` for true crash
  fidelity at `FP_COMMIT_*` / `FP_CLEANUP_*`; the env bridge's job is to make
  the process reach that boundary *deterministically* (replacing the
  sleep-window race), not to replace the kill.
- Determinism contract to encode in bn-kwm7: (a) pin
  `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` for bit-exact OIDs; (b) in-proc tier
  = bit-exact replay; (c) faithful tier = outcome-deterministic only —
  document that interleaving is not replayable and the harness **must reap
  its own killed children** before asserting recovery.

---

## Consequences / impact on downstream bones

- **bn-263u (T1.5 fault injection):** (1) **Depends on the `const fn` fix**
  in `src/merge/commit.rs` (in this workspace) — without it
  `--features failpoints` will not compile and COMMIT failpoints are unusable.
  (2) Implement the **env bridge** above (SP1 chose env-bridge: yes).
  (3) Harness recovery must be a **bounded retry that reaps its own
  children** (Finding B), not a single call. (4) The sleep-window approach is
  retained only as a fallback for the widened phase; the env bridge is the
  primary deterministic crasher (Finding A).
- **bn-kwm7 (T1.1 architecture doc):** record HYBRID (in-proc workhorse +
  faithful tier); the determinism contract (pinned git dates → bit-exact;
  faithful = outcome-only); the maw-assurance reuse map above; the generator
  interface must be drivable by both tiers ("build once, drive two ways").
- **v1.0 plan (bn-142y):** no strategic change. Reinforces the posture
  (Prime Invariant proven by *machine-checked* DST, not hand-testing): the
  spike *already* surfaced two faithful-only behaviours (sleep-window blind
  spot; zombie-masked liveness) that pure in-proc testing would miss — direct
  evidence the faithful tier is non-optional for the gate. Found & fixed a
  latent build-breaker on the exact failpoints the gate depends on.
