# maw Assurance Plan

Date: 2026-02-28
Status: draft (validated)
Audience: maintainers, reviewers, agent implementers

This is the single authoritative document for maw assurance work. If an agent
reads only one file, read this one. If this plan conflicts with a subsidiary
note under `notes/assurance/`, this plan wins and the subsidiary must be updated.

**Numbering authority**: This plan uses G1-G6 and I-G*.* numbering. The
near-proof proposal (`notes/assurance-near-proof-proposal.md`) uses an older
G1-G5 scheme with different semantics. The plan's numbering supersedes the
proposal's.

---

## 1) Problem

maw lets multiple agents modify isolated workspaces concurrently, then merges
their changes into a single mainline. This creates a hard safety requirement:

1. Concurrent work must never be silently dropped.
2. Every destructive operation must preserve recoverability.
3. Recovery must be discoverable and actionable by an agent under pressure.
4. "Lost" content must be searchable without restoring entire workspaces.

These are not aspirational goals. They are the minimum bar for a tool that
agents trust with their work product. A system that is "usually right" but
fails at crash boundaries or rewrite edges is worse than one that is honest
about its limits, because it creates false confidence.

## 2) Failure model and assumptions

Safety claims are conditional on explicit assumptions. Claims made without
stating their assumptions are worthless.

### Failure model

- Process crash can happen at any instruction boundary.
- Power loss can happen at any syscall boundary.
- No adversarial disk corruption (bit-rot, malicious filesystem mutation).
- git commands used by maw behave per their documented contracts on supported
  versions (currently git 2.40+). **No runtime version check exists** — this
  is a documentation-only minimum, not enforced.

### Assumptions

- **A1**: git ref update operations used by maw (`update-ref`, `symbolic-ref`)
  are atomic on supported platforms. **Note**: maw uses two separate
  `git update-ref` calls for COMMIT (epoch ref then branch ref), NOT a single
  `--stdin` transaction. Partial commit is structurally possible and handled by
  `recover_partial_commit()` in `src/merge/commit.rs`. Consider migrating to
  `update-ref --stdin` with start/prepare/commit for true two-ref atomicity.
- **A2**: atomic rename + fsync semantics hold for the local filesystem class
  used in CI and production (ext4, APFS, btrfs). We do not claim safety on
  NFS or other networked filesystems. **Weakness**: directory fsync in
  `commit.rs:228` is best-effort (`let _ = dir.sync_all()`) — should be
  mandatory or at least logged on failure.
- **A3**: `.manifold/` directory contents are not mutated by external tools
  during maw critical sections. **Code enforcement: NONE.** No flock, no
  lockfile, no integrity checks. Two concurrent `maw ws merge` operations
  race on `merge-state.json`. The epoch CAS provides some protection (second
  merge fails at ref advance), but state file corruption is possible before
  that point. This is the weakest assumption. Recommendation: add advisory
  flock on `.manifold/merge.lock` during merge critical section.
- **A4**: workspace directories are not concurrently mutated by non-maw
  destructive tooling during merge critical sections. **Code enforcement:
  NONE.** Enforced by convention only. Git worktrees provide ref isolation but
  not file-level protection. An agent writing to `ws/alice/` during
  `maw ws merge alice` can cause inconsistent snapshots.
- **A5** (implicit, newly documented): git's internal `.git/worktrees/<name>/`
  locking prevents concurrent `git worktree add` for the same name from both
  succeeding. maw relies on this for create/destroy concurrency safety but
  does not add its own locking.

We do **not** assume reflog retention for correctness. Reflog-only
reachability is not a safety guarantee.

## 3) Definitions (normative)

These definitions are part of the contract. All guarantee statements use these
terms precisely.

- **user work**: committed + uncommitted tracked (staged and unstaged deltas)
  + untracked non-ignored files. Ignored files are out of scope unless a
  future revision explicitly expands coverage.
- **reachable**: reachable from durable refs (`refs/**`) only.
- **lost**: work present before operation start that is neither present in the
  resulting workspace state nor reachable via contract-defined recovery
  refs/artifacts.
- **recoverable**: restorable via documented maw CLI surfaces and
  deterministic artifact/ref locations. **Note**: `claims.md` expands
  "recoverable" to include searchability and chunk-addressability. This plan
  treats searchability as a separate guarantee (G6). The normative definition
  here is the authority; `claims.md` must be updated to match.
- **searchable**: recoverable content that can be queried by pattern and
  returned as provenanced file chunks without restoring an entire workspace.
- **chunk**: a bounded excerpt from a file in a recovery point (path + line
  range + bytes).
- **replay correctness** (needed for I-G2.2): after a rewrite with replay,
  the expected workspace tree is `target_materialization + user_deltas_applied`.
  The user delta base must be `epoch_before` (from merge state), not post-op
  dirty status. Formal definition belongs in `notes/assurance/working-copy.md`.

## 4) Guarantees (G1-G6)

Each guarantee has a status reflecting the current implementation reality.
Statuses: **holds** (implemented and tested), **violated** (known code paths
break it), **partial** (some paths hold, others don't), **planned** (specified
but not yet implemented).

| ID | Guarantee | Status |
|----|-----------|--------|
| G1 | **Committed no-loss**: pre-op committed content remains durably reachable from durable or recovery refs after any maw operation. | holds (with caveat) |
| G2 | **Rewrite no-loss**: before any maw-initiated rewrite that can overwrite workspace state, maw must either prove no user work exists or capture recoverability under contract-defined surfaces. | **violated** |
| G3 | **Post-COMMIT monotonicity**: after COMMIT moves refs successfully, later cleanup failures must not undo/obscure the successful commit and must not destroy captured user work. | holds |
| G4 | **Destructive gate**: any operation that can destroy/overwrite workspace state must abort or skip if capture prerequisites fail. "Best effort destroy anyway" is forbidden. | **violated** |
| G5 | **Discoverable recovery**: when recoverable state exists, maw output and `maw ws recover` make it discoverable with executable commands. | partial |
| G6 | **Searchable recovery**: `maw ws recover --search` finds content in pinned recovery snapshots with provenance and bounded snippets. | holds |

### Known violations (must fix before claiming assurance)

**G1 caveat — recovery ref collision on same-second captures**:
`now_timestamp_iso8601()` in `src/workspace/mod.rs:1191` has 1-second
resolution. Recovery refs use `refs/manifold/recovery/<workspace>/<timestamp>`.
If two captures for the same workspace occur within the same second,
`refs::write_ref()` in `src/refs.rs:222` overwrites without CAS — the first
recovery ref is silently clobbered, making its commit object unreachable from
durable refs. Fix: use subsecond resolution (millis or nanos), or use
`write_ref_cas` with `0000...` as old_oid (create-only semantics) and retry
on collision.

**G2 violation — post-COMMIT default workspace rewrite**:
`update_default_workspace()` at `src/workspace/merge.rs:2916` uses
`git checkout --force <branch>` without any capture step. If `ws/default/`
has dirty state (staged, unstaged, or untracked non-ignored files) at the
moment of post-COMMIT cleanup, that work is silently destroyed.

Additionally: after COMMIT advances `refs/heads/<branch>`, `ws/default/` can
appear dirty even when the "diff" is non-user content caused by HEAD movement.
Naive `stash -> checkout -> stash pop` would replay old-epoch checkout content
and silently undo the post-COMMIT update. Any fix must anchor user-delta
extraction to `epoch_before` from merge state, not to post-COMMIT dirty status.

**G2 adjacent — sync rewrite without dirty check**:
`sync_worktree_to_epoch()` at `src/workspace/sync.rs:165` uses
`git checkout --detach` (no `--force`), which is safer but can still silently
overwrite tracked files whose workspace version differs from epoch when git
doesn't detect a conflict. Callers check committed-ahead but NOT dirty
uncommitted changes. Minor risk.

**Dead code risk**: `sync_stale_workspaces_for_merge()` at
`src/workspace/sync.rs:368` is `#[allow(dead_code)]` and lacks the
`committed_ahead_of_epoch()` safety check. Would be a G2 violation if activated.

**G4 violation — best-effort destroy after status failure**:
`src/workspace/merge.rs:3006-3023`: when `backend.status()` fails in
`handle_post_merge_destroy()`, the code skips capture entirely and proceeds
to destroy. The workspace may have dirty state that is destroyed without any
recovery ref.

**G4 violation — best-effort destroy after capture failure**:
`src/workspace/merge.rs:3031-3043` logs a WARNING when
`capture_before_destroy()` fails, then proceeds to destroy the workspace
anyway. This is the exact "best effort destroy anyway" path that G4 forbids.

**Design tension for post-merge G4**: The post-merge destroy continues after
capture failure because the *merged content* is already durable via the
committed merge. The content at risk is dirty files accumulated *after* the
workspace's changes were snapshotted for merge — not the merged work itself.
Resolution needed: either narrow the G4 invariant to exclude post-merge
destroy where merged content is already committed, or change the code to
abort post-merge destroy on capture failure.

### What does hold

- **G1**: merge COMMIT uses CAS ref movement (`src/merge/commit.rs`) with
  partial-commit recovery. Recovery refs are pinned before destroy via
  `capture_before_destroy()` (`src/workspace/capture.rs:100`). Integration
  tests in `tests/recovery_capture.rs` verify durability across GC. The
  same-second collision caveat above is the only known weakness.
- **G3**: COMMIT writes atomic state after both refs move. Cleanup failures
  are post-commit warnings, not commit failures. Tested in
  `tests/crash_recovery.rs` (note: tests use reimplemented recovery, not
  production `recover_from_merge_state()` — the invariants tested are valid
  but the production path is not exercised end-to-end).
- **G6**: `maw ws recover --search` is fully implemented
  (`src/workspace/recover.rs:239`) with deterministic ref-name ordering,
  bounded truncation, provenanced snippets, and stable JSON schema.
  Unit tests cover parser/validator and snippet builder. **Schema validated**:
  all 22 field/behavior checks pass against `notes/assurance/search-schema-v1.md`.

## 5) Normative rewrite behavior

For any operation that rewrites workspace content, the implementation must
follow this algorithm. Steps marked with current implementation status.

1. **Derive user deltas from explicit base** (`base_epoch`; merge cleanup
   uses `epoch_before`).
   - staged tracked: `git diff --cached --binary <base_epoch>`
   - unstaged tracked: `git diff --binary`
   - untracked set: `git ls-files --others --exclude-standard`
   - Status: **not implemented** — current code does not extract deltas.

2. **If all deltas are empty**: materialize target directly
   (`git reset --hard <target_ref>`), done.
   - Status: **not implemented** — no fast-path check exists.

3. **If user work exists**: create pinned recovery ref under
   `refs/manifold/recovery/<workspace>/<timestamp>` whose commit tree is a
   byte-for-byte capture of the working copy (tracked + untracked
   non-ignored). Write artifacts under
   `.manifold/artifacts/rewrite/<workspace>/<timestamp>/`.
   - Status: **implemented for destroy path** (`capture_before_destroy()`),
     **not implemented for merge cleanup rewrite path**.
   - Note: `working-copy.md` step 4 specifies `git clean -fd` as part of
     materialization; this plan's section omits it. Subsidiary doc is more
     detailed — align during implementation.

4. **Materialize target** in clean worktree state.
   - Status: current code uses `git checkout --force` (destructive, no prior
     capture).

5. **Replay tracked deltas** deterministically (staged first via
   `git apply --index --3way`, unstaged second via `git apply --3way`).
   - Status: **not implemented**.

6. **Replay/rehydrate untracked content** per policy.
   - Status: **not implemented**.

7. **On replay failure**: rollback to captured snapshot or safe abort before
   destruction.
   - Status: **not implemented**. **Blocking**: no formal definition of
     "correct replay result" exists yet. Must define in `working-copy.md`
     before implementation (see "replay correctness" in section 3).

The shared `working_copy::preserve_checkout_replay()` primitive described in
the near-proof proposal (`notes/assurance-near-proof-proposal.md` section 5,
Layer A) is the vehicle for implementing steps 1-7. Until it lands, G2 is
violated for any rewrite path that touches dirty workspaces.

## 6) Recovery surfaces and CLI contract

### Durable surfaces

| Surface | Location | Status | Notes |
|---------|----------|--------|-------|
| Recovery refs | `refs/manifold/recovery/<workspace>/<timestamp>` | implemented | Uses non-CAS `write_ref()`; same-second collision risk (see G1 caveat) |
| Rewrite artifacts | `.manifold/artifacts/rewrite/<workspace>/<timestamp>/` | **not implemented** | Destroy artifacts exist, rewrite artifacts do not |
| Destroy artifacts | `.manifold/artifacts/ws/<workspace>/destroy/*.json` | implemented | **Best-effort writes** — `merge.rs:3061` logs warning and continues on write failure. Agent relying on `maw ws recover` may find nothing if record write failed. Recovery ref (the critical data) is more reliable. |

### Required output on recovery-producing failures

When maw cannot safely complete a rewrite/destructive operation, output must
include all of:

1. Operation result (aborted / skipped / rolled back).
2. Whether COMMIT already succeeded (if applicable).
3. Snapshot ref and oid.
4. Artifact path (rewrite directory or destroy record).
5. At least one executable recovery command.

Status: **partial**. Destroy path emits ref+oid and recovery hints. Merge
cleanup rewrite path does not emit recovery information because it does not
capture.

### CLI command forms

All of the following are implemented and tested:

- `maw ws recover` — list destroyed workspaces
- `maw ws recover <workspace>` — show destroy history
- `maw ws recover <workspace> --show <path>` — show file from latest snapshot
- `maw ws recover <workspace> --to <new-workspace>` — restore to new workspace
- `maw ws recover --ref <recovery-ref> --show <path>` — show file from
  specific recovery ref
- `maw ws recover --ref <recovery-ref> --to <new-workspace>` — restore from
  specific recovery ref
- `maw ws recover --ref <recovery-ref> --search <pattern>` — search specific
  recovery ref
- `maw ws recover --search <pattern>` — search all recovery snapshots
- `maw ws recover <workspace> --search <pattern>` — search filtered to
  workspace

Search options: `--context`, `--max-hits`, `--regex`, `--ignore-case`,
`--text`, `--format`.

**Known issue**: `restore_to` (`recover.rs:1002-1006`) creates workspace then
populates from snapshot. If populate fails, user is left with an empty
workspace that blocks retry ("already exists"). No automatic rollback.

## 7) Invariants

Full invariant definitions live in `notes/assurance/invariants.md`. Summary
with implementation status:

| Invariant | Description | Status |
|-----------|-------------|--------|
| I-G1.1 | Committed pre-state reachable from durable or recovery refs post-op | holds |
| I-G1.2 | Rewrite that moves workspace away from non-ancestor pins recovery ref | holds (destroy path) |
| I-G2.1 | Destructive rewrite boundary requires capture or no-work proof | **violated** (merge cleanup) |
| I-G2.2 | Replay failure rolls back to snapshot or aborts safely | **not implemented** (blocked: no replay correctness definition) |
| I-G2.3 | Untracked non-ignored files captured in snapshot tree | holds (destroy path) |
| I-G3.1 | COMMIT success remains success despite cleanup failure | holds |
| I-G3.2 | Partial commit (epoch moved, branch didn't) is finalized or reported | holds |
| I-G4.1 | Destroy refuses on status/capture precondition failure | **violated** |
| I-G4.2 | No code path continues destructive action after failed capture | **violated** |
| I-G5.1 | Recovery-producing failures emit ref+oid+artifact+command | partial |
| I-G5.2 | Emitted recovery command executes successfully | partial |
| I-G6.1 | Known strings in snapshot content found by `--search` | holds |
| I-G6.2 | Hits include ref/path/line provenance + bounded snippet | holds |
| I-G6.3 | Deterministic order and truncation for fixed inputs | holds |

**Invariant check implementation feasibility** (from validation):

| Check | Precise enough? | Subprocess? | Risk |
|-------|----------------|-------------|------|
| check_g1_reachability | Yes (DRefs/RRefs overlap needs clarification) | git | Manageable |
| check_g2_rewrite_preservation | Partial | git, fs | **Blocking** (I-G2.2) |
| check_g3_commit_monotonicity | Yes | git | None |
| check_g4_destructive_gate | Yes | fs | Design tension (see G4 above) |
| check_g5_discoverability | Yes | None (I-G5.1), subprocess (I-G5.2) | None |
| check_g6_searchability | Yes | maw CLI | None |

Clarification needed: `invariants.md` defines DRefs as "durable refs in
refs/**" which includes recovery refs, then unions Reach(RRefs) separately.
This is redundant. Intent is probably DRefs = non-recovery refs. Must clarify.

## 8) Test coverage

Test IDs are defined in `notes/assurance/test-matrix.md`. Current reality:

### Implemented

| Test ID | Location | What it covers |
|---------|----------|----------------|
| IT-G1-001 | `tests/recovery_capture.rs` (4 tests) | Recovery refs survive GC, repeated destroys preserve history |
| IT-G3-001 | `tests/crash_recovery.rs` | Crash at merge phases, idempotent recovery. **Caveat**: uses reimplemented recovery, not production `recover_from_merge_state()` path |
| IT-G5-001 | `tests/destroy_recover.rs` (11 tests) | End-to-end destroy -> recover lifecycle, JSON output, --show, --to |
| UT-G6-001 | `src/workspace/recover.rs` (10 inline tests) | Recovery-ref parser/validator, snippet builder context boundaries |
| UT-G1/G4-001 | `src/workspace/capture.rs` (8 inline tests) | Capture primitives: clean/dirty/untracked/committed-ahead |
| UT-G3-001 | `src/merge/commit.rs` (4 inline tests) | CAS commit and partial-commit recovery |
| IT-G5-002 | `tests/restore.rs` (6 tests) | Restore recovery surface end-to-end |
| IT-G2-adj | `tests/sync.rs` (3 tests) | Sync rewrite behavior |
| PT-merge-001 | `src/merge/determinism_tests.rs` (25+ property tests, 100 cases each) | Merge permutation determinism |
| PT-merge-002 | `src/merge/pushout_tests.rs` (1000+ property tests) | Pushout embedding, minimality, commutativity |
| DST-lite-001 | `tests/concurrent_safety.rs` (100-seed randomized) | 5-agent concurrent merge scenarios with data-loss checks, `git fsck` corruption checks, determinism verification. Effectively lightweight DST for the merge pipeline. |

Additional relevant: `tests/merge.rs`, `tests/merge_scenarios.rs`,
`tests/workspace_lifecycle.rs`.

### Not yet implemented (backlog)

| Test ID | What it must cover |
|---------|--------------------|
| IT-G2-001 | Dirty default (staged+unstaged+untracked) survives post-COMMIT rewrite |
| IT-G2-002 | Replay failure rolls back to snapshot; emitted recovery ref/artifact valid |
| UT-G2-001 | Rewrite helper refuses destructive action without capture or no-work proof |
| IT-G4-001 | Post-merge destroy does not delete workspace on capture/status failure |
| UT-G4-001 | Destroy path returns refusal when status/capture preconditions fail |
| IT-G5-003 | Recovery-producing failures print ref+oid+artifact+command fields |
| IT-G5-004 | Emitted recovery command succeeds and restores expected bytes |
| IT-G6-001 | `--search` finds known strings in tracked and untracked snapshot files |
| IT-G6-002 | `--ref ... --show` returns exact bytes for file from hit provenance |
| DST-G1-001 | Random crash interleavings preserve committed reachability |
| DST-G2-001 | Failpoint sweep across capture/reset/replay enforces I-G2.1/2/3 |
| DST-G3-001 | Crash at each COMMIT step satisfies monotonicity |
| DST-G4-001 | Injected capture/status errors never allow destructive fallback |

### CI gates

| Gate | When | Tests | Status |
|------|------|-------|--------|
| `unit` | PR | `UT-*` | **exists** (cargo test) |
| `integration-critical` | PR | `IT-*` | **exists** (cargo test) |
| `dst-fast` | PR | `DST-*` (200-500 traces) | **not implemented** |
| `dst-nightly` | Nightly | `DST-*` (10k+ traces) | **not implemented** |
| `incident-replay` | Nightly | Historical failure corpus | **not implemented** |
| `contract-drift` | Nightly | Doc/code consistency | **not implemented** |
| `formal-check` | Pre-release | TLA+/Lean | **not implemented** |

## 9) Failpoints and DST

Failpoint catalog: `notes/assurance/failpoints.md` (**30 failpoint IDs** across
PREPARE, BUILD, VALIDATE, COMMIT, CLEANUP, DESTROY, RECOVER boundaries).

### Catalog issues found during validation

**Phantom failpoints** (3 entries describing nonexistent code):

| ID | Problem |
|----|---------|
| `FP_CLEANUP_BEFORE_REPLAY_INDEX` | No index replay exists — cleanup uses `git checkout --force` |
| `FP_CLEANUP_AFTER_REPLAY_INDEX` | Same |
| `FP_CLEANUP_BEFORE_REPLAY_WORKTREE` | Same |

These describe a replay-based cleanup that was either planned but never
implemented, or was refactored away. Remove or replace with actual code paths.

**Naming mismatch**: `FP_CLEANUP_BEFORE_RESET_HARD` / `FP_CLEANUP_AFTER_RESET_HARD`
reference `git reset --hard` but actual code does `git checkout --force`. Rename
to match implementation.

**Missing failpoints** (8 proposed, priority-ordered):

| Proposed ID | Location | Risk | Rationale |
|-------------|----------|------|-----------|
| `FP_COMMIT_BETWEEN_CAS_OPS` | `commit.rs:140-142` | **HIGHEST** | Epoch moved, state file doesn't reflect it. Recovery must use ref state as truth. |
| `FP_CAPTURE_BEFORE_PIN` | `capture.rs:271-286` | **HIGH** | Stash commit exists but not pinned to ref. GC = silent data loss. |
| `FP_COMMIT_BEFORE_EPOCH_CAS` | `commit.rs:139` | Medium | Intent recorded, no refs moved yet |
| `FP_CAPTURE_AFTER_STAGE` | `capture.rs:228-239` | Medium | Index dirty but stash commit not created |
| `FP_CLEANUP_BEFORE_DEFAULT_CHECKOUT` | `merge.rs:2484` | Medium | Replaces phantom REPLAY_* failpoints |
| `FP_CLEANUP_BEFORE_STATE_REMOVE` | `merge.rs:2493` | Medium | Stale merge-state.json after all work done |
| `FP_DESTROY_BEFORE_RECORD` | `merge.rs:3061` | Medium | Capture ref exists but no destroy record — recovery listing broken |
| `FP_COMMIT_AFTER_FINAL_STATE_WRITE` | `commit.rs:153` | Low | Both refs moved, state fully written — idempotency test |

### Implementation approach

The failpoint framework does not exist yet. When implemented:

- **Compile-time feature gate**: `#[cfg(feature = "failpoints")]` — zero
  overhead in release builds.
- **Injection macro**: `fp!("FP_COMMIT_AFTER_EPOCH_CAS")` returns `Ok(())`
  normally, injected `Err` or process abort under test.
- **Harness control**: test sets active failpoint sequence by seed. After
  `crash` injection, harness restarts maw context and runs recovery entrypoint.
- **Invariant oracle**: after each transition, run `check_g1..check_g6`
  functions from `notes/assurance/invariants.md` section 4.
- **Shrinking**: failing traces are minimized and persisted under
  `tests/corpus/dst/`. Corpus replayed on every PR.

### High-priority failpoint pairs

These pairs exercise the most dangerous state transitions:

1. `FP_COMMIT_AFTER_EPOCH_CAS` + `FP_COMMIT_BETWEEN_CAS_OPS` — partial
   commit (epoch moved, state file stale, branch not yet moved).
2. `FP_CAPTURE_AFTER_STAGE` + `FP_CAPTURE_BEFORE_PIN` — crash between stash
   creation and ref pinning (commit unreachable, GC can collect).
3. `FP_DESTROY_AFTER_STATUS` + `FP_DESTROY_BEFORE_DELETE` — crash between
   status check and deletion.

### Prerequisites

DST work is blocked on the Phase 0 fix set (section 13). There is no value in
building a simulation framework that exercises code paths known to be broken.
Fix the violations first, then prove the fixes hold under crash injection.

## 10) Concurrency threat model

These threats were identified during validation. They are not yet captured as
guarantees but must be resolved before the plan can claim completeness.

### Concurrent merge race (TOCTOU on merge-state.json)

`prepare.rs:207-244` checks `state_path.exists()` then writes — not atomic.
Two concurrent merges can both pass the existence check; the second overwrites
the first's merge-state. The epoch CAS correctly prevents dual commits, but
the losing merge's state file is clobbered, corrupting recovery state.

Fix: use `O_EXCL` / `O_CREAT` (create-exclusive) for initial merge-state
write, or use a lockfile (`merge-state.lock`) with `flock`.

### `maw push --advance` races with merge COMMIT

`push.rs:241` uses non-CAS `git update-ref` to move the branch ref. If run
concurrently with a merge COMMIT, `--advance` can move the branch ref,
causing the merge's CAS on the branch ref to fail spuriously.

Fix: use CAS for the `--advance` ref update. Consider checking for in-progress
merge state before `--advance`.

### Destroy record / latest.json not atomically linked

`destroy_record.rs:114-122` writes two files sequentially. Crash between the
record write and `latest.json` write orphans the record — `maw ws recover`
reports "no destroy records" even though one exists (the ref-based `--search`
path still works).

### Explicit non-guarantees (document, do not fix)

- **No concurrent merge exclusion**: maw relies on epoch CAS to reject
  concurrent commits, not on preventing concurrent merge attempts. Two merges
  can run in parallel through PREPARE/BUILD/VALIDATE; only one wins at COMMIT.
  The other wastes compute and gets a CAS error.
- **No dirty-state protection during sync**: `maw ws sync` checks for
  committed-ahead work but not for unstaged/untracked changes. Git's own
  conflict detection provides some protection, but this is git's behavior,
  not maw's guarantee.
- **Destroy record writes are best-effort**: Both standalone `destroy()` and
  `handle_post_merge_destroy()` treat destroy record write failures as
  warnings. The recovery ref itself is the critical data.
- **`maw push` does not check for in-progress merges**: Push pushes whatever
  the branch ref points to. During COMMIT, this could push pre-merge state.

## 11) Formal verification boundary

Formal verification is a stretch goal. It requires DST to be operational first
(to validate that models match implementation behavior). We use **Rust-native
tools** to eliminate the spec-to-implementation translation gap.

### Stateright — protocol safety (replaces TLA+)

Model the PREPARE -> BUILD -> VALIDATE -> COMMIT -> CLEANUP state machine
using [Stateright](https://github.com/stateright/stateright), a Rust-native
model checker. The model uses actual maw types (`MergePhase`,
`MergeStateFile`) from `src/merge_state.rs` — no separate spec language.

State variables: `epoch_ref`, `branch_ref`, `merge_state.phase`,
`workspace_heads`, `workspace_dirty`, `recovery_refs`, `destroy_records`.

Actions: `Prepare`, `Build`, `ValidatePass`, `ValidateFail`, `CommitEpoch`,
`CommitBranch`, `Cleanup`, `Abort`, `Crash`, `Recover`.

Safety properties (`always` checks):
- No silent loss under any action sequence.
- Commit atomicity: COMMIT either fully completed or deterministically
  recoverable.
- Destructive gate: no destruction after failed capture.

Liveness (`eventually` checks):
- Non-failing validations eventually commit.

Bounded model check for 2-3 workspaces, 10-20 step traces. Runs via
`cargo test --features assurance`. No external toolchain (Java/TLC)
required. Stateright also provides an interactive web Explorer for
visualizing state space and debugging counterexamples.

### Kani — merge algebra (replaces Lean)

Verify pure properties of the merge operator using
[Kani](https://github.com/model-checking/kani), Amazon's bounded model
checker for Rust. Upgrades existing property tests from random sampling
(proptest, 100 cases) to symbolic exhaustive verification.

Targets:
- Permutation determinism (workspace merge order doesn't change result).
- Idempotence on identical side sets.
- Embedding of non-conflicting side edits into merge result.
- Monotonic conflict exposure (conflicts are explicit data, not hidden drops).

These directly upgrade existing property tests in
`src/merge/determinism_tests.rs` and `src/merge/pushout_tests.rs` by adding
`#[kani::proof]` harnesses alongside the proptests. Both run in CI:
proptests for broad random exploration, Kani for exhaustive bounded proof.

Trade-off vs Lean: Kani gives bounded verification (proves for N<=K) not
universal theorems. But N<=10 covers all realistic merge scenarios, and
operating on actual Rust code eliminates the translation gap entirely.

Runs via `cargo kani`. No external toolchain beyond `kani-verifier`.

### What is NOT tractable

- Proving git's internal atomicity guarantees (out of scope; we assume them).
- Proving filesystem semantics end-to-end (out of scope; we assume A2).
- Proving the full Rust implementation correct (too large; DST covers this
  empirically).
- Universal proofs for all N (Kani is bounded; accept N<=10 as sufficient).

## 12) Search JSON contract

Machine output for `maw ws recover --search --format json` is normatively
defined in `notes/assurance/search-schema-v1.md`.

Top-level fields: `pattern`, `workspace_filter`, `ref_filter`, `scanned_refs`,
`hit_count`, `truncated`, `hits`, `advice`.

Per-hit fields: `ref_name`, `workspace`, `timestamp`, `oid`, `oid_short`,
`path`, `line`, `snippet`.

Per-snippet-line: `line`, `text`, `is_match`.

Compatibility policy: additive fields allowed; removals/renames/type changes
require a new versioned schema document (`search-schema-v2.md`).

Status: **implemented and validated**. All 22 field/type/behavior checks pass
against the schema spec. Determinism verified (3 identical runs).
Truncation boundary correct. Empty-result shape correct.

**Note**: `--search` is implemented in source but not yet included in the
installed release binary (v0.48.0). Needs release.

## 13) Retention and security

### Retention

Default policy: **no automatic pruning** of `refs/manifold/recovery/**` or
`.manifold/artifacts/**`. This is the safe baseline — guarantees G1-G6 remain
unconditional for all retained history.

If pruning is introduced:

1. `maw ws prune` must support `--dry-run`.
2. Must output exact refs/artifacts to be removed.
3. Must require explicit `--confirm` flag.
4. Must write tombstone manifest under `.manifold/artifacts/prune/`.
5. Claims must declare the retention window boundary.

**Recommendation**: do not implement automatic pruning until DST can verify
that pruned state does not violate guarantees. Manual `git update-ref -d`
remains available for operators who understand the implications.

### Search security model

Recovery snapshots may contain sensitive content (credentials, tokens, API
keys) present in untracked files at capture boundaries. The search surface is
a privileged attack surface.

Operational requirements:

1. **Access control**: repository permissions must restrict access to
   `refs/manifold/recovery/**` to trusted operators/agents. maw does not
   implement its own auth layer; it relies on git transport and filesystem
   permissions.
2. **Bounded output by default**: `--context` defaults to 2 lines,
   `--max-hits` defaults to 200. Full byte extraction requires explicit
   `--show` or `--to` commands.
3. **No redaction guarantee**: maw does not scan for or redact secrets in
   search output. Operators must assume hits may include credentials.
4. **Incident response**: if a search result exposes a secret, the incident
   playbook must include immediate rotation of the exposed credential.

### Audit

Recommended audit events (not yet implemented):

- Search invocation: pattern hash, filters, hit count.
- Show invocation: ref, path.
- Restore invocation: ref, new workspace name.

Audit records must not log raw snippet text (may contain secrets).

## 14) Breakdown and delivery order

Phases are ordered by risk reduction. Each phase lists prerequisites and
deliverables.

### Phase 0: Stop known loss vectors (prerequisite for everything else)

**Prerequisites**: none.

**Deliverables**:
1. Fix recovery ref timestamp collision: subsecond resolution or CAS create.
2. Remove `git checkout --force` from `update_default_workspace()`.
3. Implement shared `working_copy::preserve_checkout_replay()` primitive
   (steps 1-7 from section 5). Requires "replay correctness" definition in
   `working-copy.md` first.
4. Enforce capture-gate in destroy paths: if `capture_before_destroy()` fails,
   refuse to destroy. Remove the WARNING-and-continue path at
   `src/workspace/merge.rs:3031-3043`.
5. Fix status-failure destroy path at `src/workspace/merge.rs:3006-3023`.
6. Resolve G4 design tension for post-merge destroy (narrow invariant or
   change code).
7. Add dirty-state check to `sync_worktree_to_epoch()`.
8. Add `restore_to` rollback (destroy workspace on populate failure).
9. Tests: IT-G2-001, IT-G2-002, UT-G2-001, IT-G4-001, UT-G4-001.

**Exit criteria**: G2 and G4 status change from "violated" to "holds". G1
caveat resolved.

### Phase 0.5: Concurrency hardening

**Prerequisites**: none (can run parallel to Phase 0).

**Deliverables**:
1. Replace `exists()` check in merge-state write with `O_EXCL` create or
   `flock`-based lock.
2. Use CAS for `maw push --advance` ref update.
3. Fix destroy record / `latest.json` atomicity (single-file write or
   fallback to directory scan).
4. Document explicit non-guarantees from section 10.

**Exit criteria**: concurrent merge TOCTOU eliminated. Push-merge race
eliminated.

### Phase 1: Recovery discoverability hardening

**Prerequisites**: Phase 0 (rewrite path must emit recovery surfaces before
we can test discoverability of those surfaces).

**Deliverables**:
1. Enforce output contract (section 6 required fields) on all failure paths.
2. Tests: IT-G5-003, IT-G5-004.
3. Tests: IT-G6-001, IT-G6-002.
4. Search schema compliance check (automated diff against
   `notes/assurance/search-schema-v1.md`).
5. Release `--search` in binary (currently source-only).

**Exit criteria**: G5 status changes from "partial" to "holds".

### Phase 2: Failpoint infrastructure + fast DST

**Prerequisites**: Phase 0 (no value in crash-testing known-broken paths).

**Deliverables**:
1. `src/failpoints.rs` — feature-gated macro framework.
2. Clean up failpoint catalog: remove 3 phantom entries, fix naming
   mismatches, add 8 missing failpoints.
3. Instrument COMMIT and CLEANUP boundaries (highest-risk failpoints first:
   `FP_COMMIT_BETWEEN_CAS_OPS`, `FP_CAPTURE_BEFORE_PIN`).
4. Operation trace logger.
5. MVP DST harness with seeded scheduler, crash/restart loop, shrinker.
6. `dst-fast` CI gate (200-500 traces per PR).
7. Tests: DST-G1-001, DST-G3-001.

**Exit criteria**: `dst-fast` passes on PR gate with zero invariant violations.

### Phase 3: Full DST coverage

**Prerequisites**: Phase 2.

**Deliverables**:
1. Instrument remaining boundaries (PREPARE, BUILD, VALIDATE, DESTROY,
   RECOVER — remaining failpoints from catalog).
2. Tests: DST-G2-001, DST-G4-001.
3. `dst-nightly` CI gate (10k+ traces).
4. `incident-replay` CI gate (historical failure corpus).
5. Persist corpus under `tests/corpus/dst/`.

**Exit criteria**: nightly DST runs without invariant violation for 7
consecutive days.

### Phase 4: Formal verification (stretch)

**Prerequisites**: Phase 2 (DST must exist to cross-validate formal models).

**Deliverables**:
1. Stateright model for merge protocol (`src/assurance/model.rs`).
2. Kani proof harnesses for merge algebra (`src/merge/*_tests.rs`).
3. Traceability map: model action / proof -> source module -> DST invariant
   check -> CI job.
4. `formal-check` CI gate (`cargo test --features assurance` +
   `cargo kani`).
5. `contract-drift` CI gate (doc/code consistency).

**Exit criteria**: Stateright model check clean for bounded params
(3 workspaces, 20 steps). Kani proofs verify permutation determinism and
conflict monotonicity for N<=5.

## 15) Subsidiary document alignment

Validation found 15 discrepancies between this plan and subsidiary docs.
High-severity items requiring resolution:

| Issue | Docs | Resolution |
|-------|------|------------|
| G-numbering: proposal uses G1-G5, plan uses G1-G6 | `assurance-near-proof-proposal.md` | Add supersession note to proposal header |
| "recoverable" definition conflict | `claims.md` vs this plan | `claims.md` must drop searchability from "recoverable"; plan separates it as G6 |
| Failpoint count: plan said 26, catalog has 30 | `failpoints.md` vs this plan | Plan corrected to 30. Phase breakdown corrected. |
| G3 wording drift | `claims.md` vs this plan | Align `claims.md` predicate to plan's "undo/obscure" wording |
| Invariant numbering: proposal uses I1-I6, plan uses I-G*.* | `near-proof-proposal.md` | Plan's I-G*.* is authoritative |
| Test IDs in matrix not in plan | `test-matrix.md` | Added missing IDs to plan section 8 |

Medium/low items (track, fix during normal maintenance):
- `search.md` omits `--format` from flag list
- DST directory: proposal says `tests/dst/`, plan says `tests/corpus/dst/`
- `working-copy.md` step 4 adds `git clean -fd` not in plan

## 16) Maintainer checklist

For any PR touching destructive/rewrite/recovery/search behavior:

1. Update this plan if semantics change (especially the status table in
   section 4 and the invariant table in section 7).
2. Update affected docs in `notes/assurance/`.
3. Update test mappings in `notes/assurance/test-matrix.md`.
4. Ensure CI gates covering impacted claims pass.
5. If adding a new destructive code path: add a failpoint ID to
   `notes/assurance/failpoints.md` and a DST scenario.

## 17) Supporting documents

| Document | Purpose |
|----------|---------|
| `notes/assurance/README.md` | Index and code mapping |
| `notes/assurance/claims.md` | Normative contract definitions and guarantees |
| `notes/assurance/working-copy.md` | Rewrite/replay algorithm specification |
| `notes/assurance/recovery-contract.md` | Recovery surface and discoverability requirements |
| `notes/assurance/search.md` | Content search and chunk retrieval contract |
| `notes/assurance/invariants.md` | Machine-checkable invariant predicates |
| `notes/assurance/test-matrix.md` | Claim -> test -> CI mapping |
| `notes/assurance/search-schema-v1.md` | Stable JSON schema for search output |
| `notes/assurance/failpoints.md` | Failpoint IDs and coverage requirements |
| `notes/assurance/retention-and-security.md` | Retention baseline and security policy |
| `notes/assurance-near-proof-proposal.md` | Full proposal with detailed DST/formal design |
