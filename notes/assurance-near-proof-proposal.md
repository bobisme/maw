# Proposal: Near-Proof Assurance for maw Concurrency and Recovery

Canonical doc: `notes/assurance-plan.md`.

Date: 2026-02-27
Status: Draft proposal
Audience: maw maintainers, security reviewer, automation owners

Companion contract docs (authoritative):

- `notes/assurance-plan.md` (single entrypoint)

- `notes/assurance/README.md`
- `notes/assurance/claims.md`
- `notes/assurance/working-copy.md`
- `notes/assurance/recovery-contract.md`
- `notes/assurance/search.md`

Breakdown-ready planning docs:

- `notes/assurance/invariants.md`
- `notes/assurance/test-matrix.md`
- `notes/assurance/search-schema-v1.md`
- `notes/assurance/failpoints.md`
- `notes/assurance/retention-and-security.md`

## 1) General problem statement

maw exists to let multiple agents modify different workspaces concurrently, then
merge those changes into a single mainline without data loss.

That sounds simple, but it has a hard safety requirement:

1. concurrent work must not be silently dropped,
2. every destructive operation must preserve recoverability,
3. recovery must be discoverable by an agent under pressure.

In practice, we keep finding edge cases where behavior is "usually right" but
not provably right. This creates false confidence: the system appears safe under
normal workflows, but failure modes still exist at crash boundaries, conflicting
workspace rewrites, and UI/UX recovery surfaces.

The goal of this proposal is to move maw from "tested" to "assurance-driven":

- machine-checked where feasible,
- deterministically stress-tested where proofs are impractical,
- with explicit, testable guarantees and assumptions.


## 2) Brief summary of current implementation

Current architecture and correctness mechanisms:

- Workspace isolation uses git worktrees (`src/backend/git.rs`), with each
  workspace under `ws/<name>/` and shared object store/refs.
- Merge runs as a persisted state machine (`src/workspace/merge.rs`,
  `src/merge_state.rs`):
  PREPARE -> BUILD -> VALIDATE -> COMMIT -> CLEANUP.
- PREPARE freezes epoch + source workspace heads (`src/merge/prepare.rs`).
- BUILD runs collect/partition/resolve/build for candidate commit generation
  (`src/merge/build_phase.rs`).
- VALIDATE materializes candidate in temp worktree and enforces policy
  (`src/merge/validate.rs`).
- COMMIT uses CAS-style ref updates and partial-commit recovery
  (`src/merge/commit.rs`).
- Destroy/recover pathways exist for workspace teardown:
  capture before destroy, pin refs, write destroy records, and `maw ws recover`
  (`src/workspace/capture.rs`, `src/workspace/destroy_record.rs`,
  `src/workspace/recover.rs`).
- There is meaningful property testing already for merge determinism and
  pushout-style correctness constraints in pure merge logic
  (`src/merge/determinism_tests.rs`, `src/merge/pushout_tests.rs`).

This is a strong base. The gap is not "no safety work"; the gap is that safety
is not yet proven end-to-end across real process failures and operator recovery.


## 3) Problems still being observed

### 3.1 Data-preservation gaps at workspace rewrite boundaries

- `update_default_workspace()` still uses `git checkout --force <branch>` in
  cleanup (`src/workspace/merge.rs`). That is a known destructive primitive for
  dirty `ws/default`.
- Similar rewrite paths are not all covered by one shared, verified
  preserve-checkout-replay primitive.

Critical nuance that must be explicit:

- After COMMIT advances `refs/heads/<branch>`, `ws/default` may appear dirty
  even when the "diff" includes non-user changes caused by branch/HEAD movement.
  In that state, naive `stash -> checkout -> stash pop` can replay the old epoch
  checkout and silently undo the post-COMMIT update.
- Any rewrite primitive must therefore anchor user-delta extraction to
  `epoch_before` from merge-state, not to a naive post-COMMIT dirty check.

### 3.2 Model vs implementation assurance gap

- Existing proptests are strongest in pure merge algebra and determinism.
- We do not yet have deterministic crash-interleaving simulation for the full
  merge + workspace rewrite lifecycle.
- We lack a single executable oracle that says, for any trace, whether no work
  was lost and exactly where recovery must be found.

### 3.3 Recoverability discoverability gap

- Recoverability exists in mechanisms (recovery refs, destroy records), but we
  do not currently gate CI on "an agent can discover and execute recovery in one
  or two deterministic commands".
- Current surfaces focus on enumerate/restore workflows; we also need agent-first
  content search across pinned snapshots and chunk retrieval without full restore.
- "Recoverable but hard to discover" still fails operationally.

### 3.4 Assumptions are implicit, not contractual

- We rely on git atomicity/lock semantics, fsync+rename behavior, and stable
  command contracts, but these assumptions are not listed as formal preconditions
  for claims.


## 4) Assurance target: what "as close to proof as possible" means

We should define a contract with explicit assumptions.

### 4.0 Safety vocabulary (normative definitions)

These definitions are part of the contract and should be mirrored in
`notes/assurance/claims.md`.

- **User work**
  - committed work: content reachable from durable refs (`refs/**`), including
    `refs/manifold/recovery/**`;
  - uncommitted tracked work: staged + unstaged tracked-file deltas;
  - uncommitted untracked work: untracked, non-ignored files.
- **Out of scope by default**: ignored files, unless explicitly expanded later.
- **Lost work**: work that existed pre-operation and is neither present in the
  resulting workspace state nor reachable via durable refs/artifacts defined by
  the recovery contract.
- **Reachable**: reachable from durable refs only; reflog-only recovery is not a
  correctness guarantee.
- **Recoverable**: restorable via documented maw CLI surfaces and deterministic
  artifact/ref paths, with machine-checkable steps.

### 4.1 Proposed top-level guarantees

Given declared assumptions (Section 4.2), maw should guarantee:

- G1 No silent loss of committed work:
  If content was reachable from any workspace HEAD before operation start, it is
  reachable from either post-state refs or recorded recovery refs.
- G2 No silent loss of uncommitted workspace work during maw-initiated rewrites:
  Dirty state must be preserved either directly in resulting worktree or in an
  explicit recoverable artifact/ref.
- G3 Crash safety of merge protocol:
  Crash/restart from any failpoint yields either complete commit or a detectable,
  recoverable aborted/in-progress state with deterministic next action.
- G4 Recovery discoverability:
  For every recoverable state, maw CLI must produce actionable recovery
  instructions, and those instructions must succeed in simulation.
- G5 Searchable recovery:
  Recoverable state must be searchable by content across pinned recovery refs,
  and matching chunks must be retrievable with provenance without full restore.

### 4.2 Explicit assumptions (must be documented)

- A1 git ref update operations used by maw are atomic under supported platforms.
- A2 atomic rename + fsync semantics hold for local filesystem class in CI/prod.
- A3 external actors do not mutate `.manifold` internals except through maw.
- A4 workspace directories are not concurrently mutated by non-maw destructive
  tooling during merge critical sections.

These assumptions are realistic and auditable.

### 4.3 Mandatory destructive-operation gate

Any operation that can overwrite or destroy workspace state must satisfy one of:

1. successful pre-capture under the recovery contract, or
2. proof that no user work exists for that operation boundary.

If neither condition is met, the operation must abort or skip safely. It must
never proceed in a "best effort destroy anyway" mode.


## 5) Proposed assurance stack (layered)

No single method is enough. We should combine formal and empirical methods.

### Layer A: Safety-first implementation changes (precondition)

Before deeper proof work:

1. Remove destructive `--force` checkout paths for managed workspaces.
2. Introduce a shared `working_copy` primitive:
   - compute user delta from an explicit base (`epoch_before` when applicable),
   - capture (durable snapshot/ref + deterministic artifacts),
   - materialize target,
   - replay with deterministic semantics,
   - structured conflict/report output.
3. Ensure post-COMMIT cleanup cannot erase evidence of successful commit and
   cannot "flip" merge success to operational failure.
4. Emit stable recovery hints for every failure branch.
5. Enforce capture-gate semantics in destroy paths: if status/capture fails,
   refuse destroy.

Required replay semantics for Layer A (normative):

- **Inputs**: `base_epoch`, `target_ref`, workspace path, policy knobs.
- **Output success**: target materialized + user delta replayed (or explicit
  conflict state with deterministic diagnostics).
- **Output failure**: rollback to captured snapshot or safe abort preserving
  visibility of user work; no silent partial destruction.
- **Artifacts**: `meta.json` + replay inputs/outputs under deterministic
  `.manifold/artifacts/...` paths.

Without this, formalization effort will prove the wrong system.

### Layer B: Executable reference model (Rust)

Build a small in-repo model that represents:

- workspaces,
- file states,
- epoch refs,
- merge-state phases,
- recovery refs/artifacts.

Use it as an oracle for DST traces. The model should be deterministic and fast.

### Layer C: TLA+ protocol model (state-machine proof)

Model control-plane correctness:

- PREPARE/BUILD/VALIDATE/COMMIT/CLEANUP transitions,
- crash/restart transitions,
- ref movement constraints,
- invariants for no-silent-loss and commit atomicity.

TLA+ is the right tool for temporal/concurrency protocol safety.

### Layer D: Lean proofs for pure merge algebra

Lean is appropriate for proving pure properties in the merge algebra, not for
shell/git/filesystem effects.

Good Lean targets:

- determinism under workspace permutation,
- embedding of side edits into merge result or explicit conflict data,
- composition laws for patch-set operations used by merge core.

Lean artifacts should mirror existing property tests so we can cross-check
theorem statements against executable fuzzing.

### Layer E: Deterministic Simulation Testing (DST)

This is the main end-to-end confidence engine for real implementation.

Core characteristics:

- seeded deterministic scheduler,
- failpoint injection at critical operations,
- crash/restart and operation interleavings,
- invariant checks after each step,
- shrinking to minimal failing traces.

### Layer F: Recovery discoverability tests

Treat discoverability as a first-class property:

- parse CLI output for recovery instructions,
- execute suggested command(s),
- verify restored content equivalence.

### Layer G: Operational telemetry and canary runs

- Keep machine-readable operation/recovery logs for postmortems.
- Run nightly heavy DST in CI and periodic canary scenarios on real repos.


## 6) Detailed DST system design

### 6.1 Operation DSL

Define a compact operation language for generated traces, e.g.:

- `CreateWorkspace(name, mode)`
- `EditFile(ws, path, edit_kind, payload)`
- `Commit(ws, msg_kind)`
- `Sync(ws)`
- `Advance(ws)`
- `Merge(sources, opts)`
- `Destroy(ws, mode)`
- `RecoverShow(ws, path)`
- `RecoverRestore(ws, target)`
- `Crash`
- `Restart`

Each operation includes preconditions and expected side effects.

### 6.2 Deterministic scheduler

- Use a reproducible seed.
- Interleave operations from multiple simulated agents.
- Provide deterministic virtual clock and deterministic temp path allocator.

### 6.3 Failpoint map

Add failpoints around critical boundaries:

- before/after merge-state write,
- before/after candidate commit write,
- before/after epoch ref CAS,
- before/after branch ref CAS,
- before/after workspace rewrite capture,
- before/after checkout/replay,
- before/after destroy-record write.

Implementation approach:

- compile-time test feature `failpoints`,
- macro `fp!("name")` that can return injected failure,
- test harness controls active failpoint sequence by seed.

### 6.4 Crash simulation

At each failpoint in a trace:

1. inject failure or simulated process kill,
2. restart maw process context,
3. run recovery entrypoint,
4. continue remaining operations or terminate trace,
5. check invariants.

### 6.5 Invariants and oracles

Check after every transition:

- I1 committed reachability invariant (no committed work lost from durable refs),
- I2 uncommitted preservation invariant across rewrites (tracked + untracked,
  excluding ignored paths unless configured otherwise),
- I3 merge-state/ref consistency invariant (protocol transitions and CAS state),
- I4 capture-gate invariant (destructive path never runs without successful
  capture or explicit proof of no user work),
- I5 recoverability invariant: if work is not in active tree, it must be in
  recovery ref/artifact addressable by maw commands,
- I6 discoverability invariant: CLI output includes actionable next step that
  executes successfully in harness.

### 6.6 Failure shrinking and corpus

- Persist failing seeds and minimized traces under `tests/corpus/dst/`.
- Re-run corpus on every PR.
- Automatically annotate minimal repro with exact failing invariant and file set.

### 6.7 Performance targets

- PR lane: 200-500 traces (< 10 min total).
- Nightly lane: 10k+ traces with full failpoint matrix.
- Weekly deep lane: long-run churn scenarios with heavy edit volume.


## 7) Formal methods plan in detail

### 7.1 TLA+ scope (protocol + crash behavior)

Model variables:

- `epoch_ref`, `branch_ref`,
- `merge_state.phase`, `merge_state.frozen_inputs`,
- `workspace_heads`, `workspace_dirty`,
- `recovery_refs`, `destroy_records`.

Core actions:

- `Prepare`, `Build`, `ValidatePass`, `ValidateFail`, `CommitEpoch`,
  `CommitBranch`, `Cleanup`, `Abort`, `Crash`, `Recover`.

Proof obligations:

- safety: no silent loss under actions,
- refinement-like relation: COMMIT either atomically completed or recoverable,
- liveness under fair scheduler for non-failing validations.

### 7.2 Lean scope (pure merge semantics)

Define pure structures for:

- patch atoms,
- patch-set application,
- conflict representation,
- merge operator.

Theorems to target first:

- permutation determinism,
- idempotence on identical side sets,
- embedding of non-conflicting side edits,
- monotonic conflict exposure (conflicts are explicit data, not hidden drops).

### 7.3 Proof-to-code traceability

Every theorem/invariant links to:

- source module(s),
- DST invariant check implementation,
- CI job enforcing it.


## 8) Recoverability discoverability contract

Define a user-facing contract and test it:

1. On data-risking failure, output must include:
   - `WARNING:` or `ERROR:` prefix,
   - what failed,
   - where preserved data is located,
   - exact recovery command.
2. `maw ws recover` must enumerate all relevant snapshots/records.
3. Suggested recovery command must be executable non-interactively.
4. Restored workspace must contain byte-equivalent content for preserved files.

Required recovery surfaces:

- Git refs: deterministic `refs/manifold/recovery/<workspace>/<timestamp>`.
- Artifacts: deterministic `.manifold/artifacts/...` entries including machine-
  readable metadata linking operation, snapshot OID/ref, and suggested commands.

Contract docs to maintain in-repo:

- `notes/assurance/claims.md`
- `notes/assurance/working-copy.md`
- `notes/assurance/recovery-contract.md`
- `notes/assurance/search.md`

This turns UX into a verifiable safety surface.


## 9) CI gates and release policy

### 9.1 Per-PR required

- unit + integration tests,
- fast DST lane,
- deterministic merge proptests,
- recovery discoverability smoke tests.

### 9.2 Nightly required

- heavy DST failpoint matrix,
- long-run concurrency scenarios,
- regression corpus replay.

### 9.3 Pre-release required

- zero unresolved P0/P1 safety findings,
- TLA+ model check clean for bounded params,
- Lean theorem set for declared merge-core claims complete,
- incident-style replay suite pass (known historical failures).


## 10) Delivery roadmap

### Phase 0 (immediate, 1 week): stop known loss vectors

- eliminate destructive checkout in default rewrite path,
- unify preserve-checkout-replay helper,
- enforce capture-gate behavior in destroy flows,
- add targeted regression tests:
  - dirty default (staged + unstaged + untracked) survives post-COMMIT update,
  - replay failure rolls back to snapshot and emits recovery surfaces,
  - post-merge destroy refuses to destroy if status/capture fails,
  - post-COMMIT cleanup warnings never invalidate successful COMMIT outcome.

### Phase 1 (2 weeks): instrument for DST

- introduce failpoint framework,
- add operation trace logger,
- implement first invariants (I1-I3).

### Phase 2 (3 weeks): MVP DST harness

- operation DSL + seeded scheduler,
- crash/restart loop,
- shrinker + corpus persistence,
- PR/nightly CI lanes.

### Phase 3 (2 weeks): recovery discoverability hardening

- enforce output contract,
- add executable recovery tests,
- wire invariant I4/I5.

### Phase 4 (parallel, 3-5 weeks): formal layer

- TLA+ model of merge protocol,
- Lean module for merge algebra core,
- traceability map from proofs to code/tests.


## 11) Risks and trade-offs

- More engineering overhead in short term.
- Failpoint instrumentation can add code complexity if not disciplined.
- Formal artifacts can drift unless tied to CI and owner rotation.
- "Proof" remains conditional on assumptions about git/filesystem behavior.

This is acceptable: explicit conditional guarantees are much stronger than
implicit confidence.


## 12) Concrete deliverables

1. `notes/assurance/claims.md` with guarantees + assumptions.
2. `notes/assurance/working-copy.md` with normative rewrite/replay semantics.
3. `notes/assurance/recovery-contract.md` + discoverability tests.
4. `notes/assurance/search.md` with searchable recovery + chunk retrieval contract.
5. `tests/dst/` harness with seeded deterministic traces.
6. `src/failpoints.rs` + instrumentation at critical boundaries.
7. `formal/tla/` spec + checked invariants.
8. `formal/lean/` core merge proofs.
9. CI jobs: `dst-fast`, `dst-nightly`, `formal-check`, and contract drift checks.


## 13) Immediate next actions (proposed)

1. Land Phase 0 fix set (including default workspace rewrite safety).
2. Keep the contract docs authoritative and updated before adding more tests:
   - `notes/assurance/claims.md`
   - `notes/assurance/working-copy.md`
   - `notes/assurance/recovery-contract.md`
   - `notes/assurance/search.md`
   - `notes/assurance/README.md`
3. Build minimal DST harness with 20-50 seeds focused on merge/cleanup crash points.
4. Add first discoverability gate: every recovery-producing error must include
   exact command and that command must pass in test.
5. Start TLA+ model once DST MVP is green, then layer Lean on pure merge core.


## 14) Success criteria

We consider this proposal successful when:

- no known silent-loss path remains,
- every historical incident has a deterministic replay test,
- DST runs large seeded matrices without invariant violations,
- recoverability is not only true but operationally obvious,
- formal artifacts and DST agree on declared guarantees.

At that point, maw can make a credible statement: not "we think it is safe",
but "under explicit assumptions, we can demonstrate and continuously verify
that work is preserved and recoverable."
