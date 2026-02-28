# Failpoint Plan and Coverage

Canonical doc: `notes/assurance-plan.md`.

Status: breakdown-ready failpoint catalog
Purpose: deterministic crash/error injection for assurance invariants

Total failpoints: 30
- PREPARE: 2
- BUILD: 4
- VALIDATE: 5
- COMMIT: 7
- CLEANUP: 5
- DESTROY: 4
- RECOVER: 3

## 1) Goals

- exercise every critical boundary where state can be lost, orphaned, or hidden
- verify recovery paths preserve G1-G6 invariants under injected failures
- produce minimal deterministic repro traces

## 2) Failpoint naming

Use uppercase IDs with stable namespace prefixes:

- `FP_PREPARE_*`
- `FP_BUILD_*`
- `FP_VALIDATE_*`
- `FP_COMMIT_*`
- `FP_CAPTURE_*`
- `FP_CLEANUP_*`
- `FP_DESTROY_*`
- `FP_RECOVER_*`

## 3) Required failpoints (minimum set)

### PREPARE

- `FP_PREPARE_BEFORE_STATE_WRITE`
- `FP_PREPARE_AFTER_STATE_WRITE`

### BUILD

- `FP_BUILD_BEFORE_WORKTREE_ADD` (implemented) — before advance to BUILD phase
- `FP_BUILD_AFTER_WORKTREE_ADD` (implemented) — after BUILD phase state written
- `FP_BUILD_BEFORE_MERGE_COMPUTE` (implemented) — before merge pipeline runs
- `FP_BUILD_AFTER_MERGE_COMPUTE` (implemented) — after merge result computed
- `FP_BUILD_BEFORE_CANDIDATE_WRITE` (planned)
- `FP_BUILD_AFTER_CANDIDATE_WRITE` (planned)

### VALIDATE

- `FP_VALIDATE_BEFORE_CHECK` (implemented) — before validation commands run
- `FP_VALIDATE_AFTER_CHECK` (implemented) — after validation completes
- `FP_VALIDATE_BEFORE_WORKTREE_ADD` (planned)
- `FP_VALIDATE_AFTER_WORKTREE_ADD` (planned)
- `FP_VALIDATE_BEFORE_RESULT_WRITE` (planned)
- `FP_VALIDATE_AFTER_RESULT_WRITE` (planned)
- `FP_VALIDATE_BEFORE_WORKTREE_REMOVE` (planned)

### COMMIT

- `FP_COMMIT_BEFORE_STATE_WRITE`
  - Location: `src/merge/commit.rs:137` (before initial `write_merge_state`)
  - Risk: MEDIUM
  - Invariants: G3
  - Description: crash before persisting any commit-phase state; recovery should find no partial state

- `FP_COMMIT_BEFORE_EPOCH_CAS`
  - Location: `src/merge/commit.rs:139` (before `refs::advance_epoch`)
  - Risk: HIGH
  - Invariants: G1, G3
  - Description: crash after state write but before epoch ref CAS; neither ref has moved yet

- `FP_COMMIT_AFTER_EPOCH_CAS`
  - Location: `src/merge/commit.rs:142` (after `refs::advance_epoch`, before branch CAS)
  - Risk: HIGHEST — this is the partial-commit window
  - Invariants: G3 (I-G3.2 partial commit recoverable)
  - Description: epoch ref moved but branch ref has not; recovery must finalize or roll back

- `FP_COMMIT_BETWEEN_CAS_OPS`
  - Location: `src/merge/commit.rs:144-145` (between epoch ref update and branch ref CAS)
  - Risk: HIGHEST
  - Invariants: G1, G3
  - Description: crash in the window where epoch points to the new candidate but branch
    still points to the old commit; `recover_partial_commit` must detect and finalize

- `FP_COMMIT_BEFORE_BRANCH_CAS`
  - Location: `src/merge/commit.rs:145` (before `refs::write_ref_cas` for branch)
  - Risk: HIGHEST
  - Invariants: G3 (I-G3.2)
  - Description: epoch advanced, about to update branch; crash here leaves refs diverged

- `FP_COMMIT_AFTER_BRANCH_CAS`
  - Location: `src/merge/commit.rs:150-153` (after branch CAS succeeded, before final state write)
  - Risk: LOW
  - Invariants: G3
  - Description: both refs moved successfully; crash before final state file update is benign

- `FP_COMMIT_AFTER_FINAL_STATE_WRITE`
  - Location: `src/merge/commit.rs:153` (after final `write_merge_state` with `Committed` phase)
  - Risk: LOW
  - Invariants: G3
  - Description: commit fully persisted; crash here should not affect committed state;
    verifies cleanup phase handles a completed commit-state file correctly

### CAPTURE (workspace capture before destroy)

- `FP_CAPTURE_AFTER_STAGE`
  - Location: `src/workspace/capture.rs:227-231` (after `git add -A` stages all files)
  - Risk: MEDIUM
  - Invariants: G2, G4
  - Description: crash after staging dirty content but before stash commit creation;
    index is modified but no recovery ref exists yet

- `FP_CAPTURE_BEFORE_PIN`
  - Location: `src/workspace/capture.rs:285-287` (after stash/tree commit created, before `refs::write_ref` pins recovery ref)
  - Risk: HIGH
  - Invariants: G1, G2
  - Description: the capture commit object exists in the object store but has no ref
    pointing to it; without a pin, git gc will eventually collect it; violates G1
    reachability guarantee

### CLEANUP / rewrite

- `FP_CLEANUP_BEFORE_CAPTURE`
- `FP_CLEANUP_AFTER_CAPTURE`

- `FP_CLEANUP_BEFORE_DEFAULT_CHECKOUT`
  - Location: `src/workspace/merge.rs:2484` (before `update_default_workspace` calls `git checkout --force`)
  - Risk: MEDIUM
  - Invariants: G3 (I-G3.1 commit success monotonic)
  - Description: crash after COMMIT succeeded but before default workspace working copy
    is updated to the new epoch; committed refs are valid but the default workspace
    shows stale content until re-checkout

- `FP_CLEANUP_AFTER_DEFAULT_CHECKOUT`
  - Location: `src/workspace/merge.rs:2484` (after `update_default_workspace` returns)
  - Risk: LOW
  - Invariants: G3
  - Description: default workspace updated; crash before merge-state removal leaves a
    stale state file that cleanup-on-next-run should handle

- `FP_CLEANUP_BEFORE_STATE_REMOVE`
  - Location: `src/workspace/merge.rs:2492-2497` (before merge-state file removal via `run_cleanup_phase`)
  - Risk: MEDIUM
  - Invariants: G3
  - Description: crash with merge-state file still present after a successful commit;
    next operation must detect the completed merge-state and skip re-execution

### DESTROY paths

- `FP_DESTROY_BEFORE_STATUS` (planned)
- `FP_DESTROY_AFTER_STATUS` (implemented in create.rs)
- `FP_DESTROY_BEFORE_CAPTURE` (implemented in merge.rs)
- `FP_DESTROY_AFTER_CAPTURE` (planned)
- `FP_DESTROY_AFTER_RECORD` (implemented in merge.rs)
- `FP_DESTROY_AFTER_DELETE` (implemented in merge.rs)

- `FP_DESTROY_BEFORE_RECORD`
  - Location: `src/workspace/merge.rs:3058-3061` (before `write_destroy_record`)
  - Risk: MEDIUM
  - Invariants: G4, G5
  - Description: crash after capture succeeded but before the append-only destroy record
    is written; recovery ref exists but destroy audit trail is incomplete

- `FP_DESTROY_BEFORE_DELETE`

### RECOVER/search paths

- `FP_RECOVER_BEFORE_RESTORE` (implemented in recover.rs)
- `FP_RECOVER_BEFORE_SEARCH` (implemented in recover.rs)
- `FP_RECOVER_BEFORE_REF_ENUM` (planned)
- `FP_RECOVER_AFTER_REF_ENUM` (planned)
- `FP_RECOVER_BEFORE_SHOW` (planned)

## 4) Injection behavior

Each failpoint must support at least two deterministic actions:

1. `error`: function returns injected error
2. `crash`: process abort simulation (state persists to disk)

Harness must run restart/recovery flow after `crash` actions.

## 5) Coverage requirements

For each failpoint ID:

- at least one `error` scenario in PR/fast lane
- at least one `crash` scenario in nightly lane
- invariant checks from `notes/assurance/invariants.md` run after recovery

Pairwise requirement for critical sequences:

- COMMIT pair: `AFTER_EPOCH_CAS` + `BEFORE_BRANCH_CAS` (partial-commit window)
- COMMIT pair: `BETWEEN_CAS_OPS` + `AFTER_BRANCH_CAS` (finalization window)
- CAPTURE pair: `AFTER_STAGE` + `BEFORE_PIN` (capture atomicity)
- CLEANUP pair: `AFTER_CAPTURE` + `BEFORE_DEFAULT_CHECKOUT`
- DESTROY pair: `AFTER_STATUS` + `BEFORE_DELETE`

## 6) Trace output (for shrinking and debugging)

Every failing run should emit:

- seed
- failpoint id and action
- operation trace (compact DSL)
- violated invariant id(s)
- minimal repro after shrink

## 7) Ticketization order

1. COMMIT failpoints + recovery checks (highest impact — G1/G3 partial-commit window)
2. CAPTURE failpoints (G1/G2 pin-before-risk)
3. CLEANUP rewrite failpoints (default workspace risk)
4. DESTROY failpoints (capture gate)
5. SEARCH failpoints (G6 determinism hardening)
