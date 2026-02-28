# Failpoint Plan and Coverage

Canonical doc: `notes/assurance-plan.md`.

Status: breakdown-ready failpoint catalog
Purpose: deterministic crash/error injection for assurance invariants

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
- `FP_CLEANUP_*`
- `FP_DESTROY_*`
- `FP_RECOVER_*`

## 3) Required failpoints (minimum set)

### PREPARE

- `FP_PREPARE_BEFORE_STATE_WRITE`
- `FP_PREPARE_AFTER_STATE_WRITE`

### BUILD

- `FP_BUILD_BEFORE_PHASE_ADVANCE`
- `FP_BUILD_AFTER_PHASE_ADVANCE`
- `FP_BUILD_BEFORE_CANDIDATE_WRITE`
- `FP_BUILD_AFTER_CANDIDATE_WRITE`

### VALIDATE

- `FP_VALIDATE_BEFORE_WORKTREE_ADD`
- `FP_VALIDATE_AFTER_WORKTREE_ADD`
- `FP_VALIDATE_BEFORE_RESULT_WRITE`
- `FP_VALIDATE_AFTER_RESULT_WRITE`
- `FP_VALIDATE_BEFORE_WORKTREE_REMOVE`

### COMMIT

- `FP_COMMIT_BEFORE_STATE_WRITE`
- `FP_COMMIT_AFTER_EPOCH_CAS`
- `FP_COMMIT_BEFORE_BRANCH_CAS`
- `FP_COMMIT_AFTER_BRANCH_CAS`

### CLEANUP / rewrite

- `FP_CLEANUP_BEFORE_CAPTURE`
- `FP_CLEANUP_AFTER_CAPTURE`
- `FP_CLEANUP_BEFORE_RESET_HARD`
- `FP_CLEANUP_AFTER_RESET_HARD`
- `FP_CLEANUP_BEFORE_REPLAY_INDEX`
- `FP_CLEANUP_AFTER_REPLAY_INDEX`
- `FP_CLEANUP_BEFORE_REPLAY_WORKTREE`

### DESTROY paths

- `FP_DESTROY_BEFORE_STATUS`
- `FP_DESTROY_AFTER_STATUS`
- `FP_DESTROY_BEFORE_CAPTURE`
- `FP_DESTROY_AFTER_CAPTURE`
- `FP_DESTROY_BEFORE_DELETE`

### RECOVER/search paths

- `FP_RECOVER_BEFORE_REF_ENUM`
- `FP_RECOVER_AFTER_REF_ENUM`
- `FP_RECOVER_BEFORE_SHOW`

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

- COMMIT pair: `AFTER_EPOCH_CAS` + `BEFORE_BRANCH_CAS`
- CLEANUP pair: `AFTER_CAPTURE` + `BEFORE_REPLAY_*`
- DESTROY pair: `AFTER_STATUS` + `BEFORE_DELETE`

## 6) Trace output (for shrinking and debugging)

Every failing run should emit:

- seed
- failpoint id and action
- operation trace (compact DSL)
- violated invariant id(s)
- minimal repro after shrink

## 7) Ticketization order

1. COMMIT failpoints + recovery checks (highest impact)
2. CLEANUP rewrite failpoints (default workspace risk)
3. DESTROY failpoints (capture gate)
4. SEARCH failpoints (G6 determinism hardening)
