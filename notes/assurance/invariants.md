# Assurance Invariants (G1-G6)

Canonical doc: `notes/assurance-plan.md`.

Status: breakdown-ready spec
Purpose: machine-checkable invariants for DST, integration tests, and formal models

This document converts the contract in `notes/assurance/claims.md` into explicit
predicates with pass/fail evidence.

## 1) Scope and assumptions

Invariants are interpreted under assumptions in `notes/assurance/claims.md`.
They apply to maw-driven operations that can merge, rewrite, destroy, recover,
or search recovery snapshots.

## 2) Operational state model

For each operation `op`, define:

- `pre(op)`: state at operation start
- `post(op)`: state after operation completes (success, warning, or failure)

Observed state sets:

- `DRefs(s)`: durable refs in `refs/**`
- `RRefs(s)`: recovery refs in `refs/manifold/recovery/**`
- `Reach(s)`: commits reachable from `DRefs(s)`
- `Artifacts(s)`: `.manifold/artifacts/**` entries
- `WS(s, w)`: workspace tree/index bytes for workspace `w`

Derived sets:

- `CommittedPre(op)`: commits in `Reach(pre(op))`
- `UserWorkPre(op, w)`: tracked staged + tracked unstaged + untracked non-ignored
  deltas in workspace `w` at operation start

## 3) Invariant catalog

Each invariant is identified as `I-Gx.y` and maps to one or more tests.

### G1: No silent loss of committed work

`I-G1.1 Durable reachability`

- Predicate: `CommittedPre(op) subset Reach(post(op)) union Reach(RRefs(post(op)))`
- Interpretation: committed pre-state content is still reachable from durable or
  recovery refs after the operation.

`I-G1.2 Rewrite pin-before-risk`

- Predicate: if operation can move a workspace away from a non-ancestor commit,
  then a recovery ref for that workspace exists in `post(op)` unless no move
  occurred.

### G2: No silent loss of uncommitted work on rewrites

`I-G2.1 Capture-or-proof gate`

- Predicate: for every destructive rewrite boundary, either
  - `UserWorkPre(op, w)` is empty, or
  - a recovery snapshot ref + metadata artifact exists in `post(op)`.

`I-G2.2 Replay/rollback safety`

- Predicate: if replay fails after materializing target, workspace ends in either
  - successfully replayed user deltas, or
  - rollback to captured snapshot with explicit recovery guidance.

`I-G2.3 Untracked preservation`

- Predicate: if untracked non-ignored files exist in `UserWorkPre(op, w)`, their
  bytes are present in the pinned recovery snapshot commit tree.

### G3: Post-COMMIT monotonicity

`I-G3.1 Commit success monotonic`

- Predicate: if COMMIT ref updates succeeded, later cleanup failures do not
  invalidate committed refs and are reported as post-commit warnings.

`I-G3.2 Partial commit recoverable`

- Predicate: if epoch ref moved but branch ref did not, recovery finalizes branch
  or reports deterministic actionable state; no silent divergence.

### G4: Destructive operation gate

`I-G4.1 Destroy refuses on unknown safety`

- Predicate: if status/capture preconditions fail for destroy path, workspace is
  not destroyed in `post(op)` and output indicates refusal.

`I-G4.2 No best-effort destructive fallback`

- Predicate: no code path continues with destructive action after failed capture
  precondition check.

### G5: Discoverable recovery

`I-G5.1 Recovery surface presence`

- Predicate: recovery-producing failures emit snapshot ref, snapshot oid, and
  artifact location in output.

`I-G5.2 Executable next step`

- Predicate: at least one emitted recovery command executes successfully in test.

### G6: Searchable recovery

`I-G6.1 Search coverage`

- Predicate: known strings inserted into pinned snapshot content (tracked and
  untracked non-ignored) are returned by `maw ws recover --search`.

`I-G6.2 Provenanced chunk output`

- Predicate: each hit returns ref/path/line provenance plus bounded snippet lines.

`I-G6.3 Deterministic truncation/order`

- Predicate: for fixed repo + fixed inputs, search output ordering and
  truncation behavior are stable.

## 4) Check functions (DST/integration oracle)

Harness should expose explicit checks:

- `check_g1_reachability(pre, post)`
- `check_g2_rewrite_preservation(pre, post, workspace)`
- `check_g3_commit_monotonicity(pre, post)`
- `check_g4_destructive_gate(pre, post, workspace)`
- `check_g5_discoverability(output, post)`
- `check_g6_searchability(repo_state, query_cases)`

Each check should produce a structured failure payload with:

- invariant id,
- minimal reproducer seed/trace,
- involved workspace(s),
- expected vs actual evidence.

## 5) Traceability map

- `I-G1.*` -> merge/commit/rewrite refs (`src/merge/commit.rs`, `src/workspace/merge.rs`)
- `I-G2.*` -> rewrite capture/replay (`src/workspace/merge.rs`, `src/workspace/capture.rs`)
- `I-G3.*` -> commit-state recovery (`src/merge/commit.rs`, `src/merge_state.rs`)
- `I-G4.*` -> destroy gate (`src/workspace/merge.rs`, `src/workspace/destroy.rs`)
- `I-G5.*` -> recover UX (`src/workspace/recover.rs`, command output contracts)
- `I-G6.*` -> search/ref flows (`src/workspace/recover.rs`, CLI parsing in `src/workspace/mod.rs`)

## 6) Definition of done for "breakdown ready"

This invariant spec is considered implementation-ready when every invariant has:

1. at least one unit or integration test id in `notes/assurance/test-matrix.md`,
2. at least one DST/failpoint scenario where applicable,
3. CI gating status (required lane) assigned.
