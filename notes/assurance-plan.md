# maw Assurance Plan

Date: 2026-02-27
Status: draft
Audience: maintainers, reviewers, agent implementers

This is the single authoritative document for maw assurance work. If an agent
reads only one file, read this one. If this plan conflicts with a subsidiary
note under `notes/assurance/`, this plan wins and the subsidiary must be updated.

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
  versions (currently git 2.40+).

### Assumptions

- **A1**: git ref update operations used by maw (`update-ref --stdin`,
  `symbolic-ref`) are atomic on supported platforms.
- **A2**: atomic rename + fsync semantics hold for the local filesystem class
  used in CI and production (ext4, APFS, btrfs). We do not claim safety on
  NFS or other networked filesystems.
- **A3**: `.manifold/` directory contents are not mutated by external tools
  during maw critical sections.
- **A4**: workspace directories are not concurrently mutated by non-maw
  destructive tooling during merge critical sections.

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
  deterministic artifact/ref locations.
- **searchable**: recoverable content that can be queried by pattern and
  returned as provenanced file chunks without restoring an entire workspace.
- **chunk**: a bounded excerpt from a file in a recovery point (path + line
  range + bytes).

## 4) Guarantees (G1-G6)

Each guarantee has a status reflecting the current implementation reality.
Statuses: **holds** (implemented and tested), **violated** (known code paths
break it), **partial** (some paths hold, others don't), **planned** (specified
but not yet implemented).

| ID | Guarantee | Status |
|----|-----------|--------|
| G1 | **Committed no-loss**: pre-op committed content remains durably reachable from durable or recovery refs after any maw operation. | holds |
| G2 | **Rewrite no-loss**: before any maw-initiated rewrite that can overwrite workspace state, maw must either prove no user work exists or capture recoverability under contract-defined surfaces. | **violated** |
| G3 | **Post-COMMIT monotonicity**: after COMMIT moves refs successfully, later cleanup failures must not undo/obscure the successful commit and must not destroy captured user work. | holds |
| G4 | **Destructive gate**: any operation that can destroy/overwrite workspace state must abort or skip if capture prerequisites fail. "Best effort destroy anyway" is forbidden. | **violated** |
| G5 | **Discoverable recovery**: when recoverable state exists, maw output and `maw ws recover` make it discoverable with executable commands. | partial |
| G6 | **Searchable recovery**: `maw ws recover --search` finds content in pinned recovery snapshots with provenance and bounded snippets. | holds |

### Known violations (must fix before claiming assurance)

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

**G4 violation — best-effort destroy after capture failure**:
`src/workspace/merge.rs:3033` logs a WARNING when `capture_before_destroy()`
fails, then proceeds to destroy the workspace anyway. This is the exact
"best effort destroy anyway" path that G4 forbids.

### What does hold

- **G1**: merge COMMIT uses CAS ref movement (`src/merge/commit.rs`) with
  partial-commit recovery. Recovery refs are pinned before destroy via
  `capture_before_destroy()` (`src/workspace/capture.rs:100`). Integration
  tests in `tests/recovery_capture.rs` verify durability across GC.
- **G3**: COMMIT writes atomic state after both refs move. Cleanup failures
  are post-commit warnings, not commit failures. Tested in
  `tests/crash_recovery.rs`.
- **G6**: `maw ws recover --search` is fully implemented
  (`src/workspace/recover.rs:239`) with deterministic ref-name ordering,
  bounded truncation, provenanced snippets, and stable JSON schema.
  Unit tests cover parser/validator and snippet builder.

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
   - Status: **not implemented**.

The shared `working_copy::preserve_checkout_replay()` primitive described in
the near-proof proposal (`notes/assurance-near-proof-proposal.md` section 5,
Layer A) is the vehicle for implementing steps 1-7. Until it lands, G2 is
violated for any rewrite path that touches dirty workspaces.

## 6) Recovery surfaces and CLI contract

### Durable surfaces

| Surface | Location | Status |
|---------|----------|--------|
| Recovery refs | `refs/manifold/recovery/<workspace>/<timestamp>` | implemented |
| Rewrite artifacts | `.manifold/artifacts/rewrite/<workspace>/<timestamp>/` | **not implemented** (destroy artifacts exist, rewrite artifacts do not) |
| Destroy artifacts | `.manifold/artifacts/ws/<workspace>/destroy/*.json` | implemented |

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
- `maw ws recover --search <pattern>` — search all recovery snapshots
- `maw ws recover <workspace> --search <pattern>` — search filtered to
  workspace

Search options: `--context`, `--max-hits`, `--regex`, `--ignore-case`,
`--text`, `--format`.

## 7) Invariants

Full invariant definitions live in `notes/assurance/invariants.md`. Summary
with implementation status:

| Invariant | Description | Status |
|-----------|-------------|--------|
| I-G1.1 | Committed pre-state reachable from durable or recovery refs post-op | holds |
| I-G1.2 | Rewrite that moves workspace away from non-ancestor pins recovery ref | holds (destroy path) |
| I-G2.1 | Destructive rewrite boundary requires capture or no-work proof | **violated** (merge cleanup) |
| I-G2.2 | Replay failure rolls back to snapshot or aborts safely | **not implemented** |
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

## 8) Test coverage

Test IDs are defined in `notes/assurance/test-matrix.md`. Current reality:

### Implemented

| Test ID | Location | What it covers |
|---------|----------|----------------|
| (IT-G1) | `tests/recovery_capture.rs` | Recovery refs survive GC, repeated destroys preserve history |
| (IT-G3) | `tests/crash_recovery.rs` | Crash at merge phases, idempotent recovery |
| (IT-G5) | `tests/destroy_recover.rs` | End-to-end destroy -> recover lifecycle, JSON output, --show, --to |
| (UT-G6) | `src/workspace/recover.rs` (inline) | Recovery-ref parser/validator, snippet builder context boundaries |

Additional relevant test files: `tests/merge.rs`, `tests/merge_scenarios.rs`,
`tests/workspace_lifecycle.rs`, `tests/concurrent_safety.rs`.

### Not yet implemented (backlog)

| Test ID | What it must cover |
|---------|--------------------|
| IT-G2-001 | Dirty default (staged+unstaged+untracked) survives post-COMMIT rewrite |
| IT-G2-002 | Replay failure rolls back to snapshot; emitted recovery ref/artifact valid |
| UT-G2-001 | Rewrite helper refuses destructive action without capture or no-work proof |
| IT-G4-001 | Post-merge destroy does not delete workspace on capture/status failure |
| UT-G4-001 | Destroy path returns refusal when status/capture preconditions fail |
| IT-G5-001 | Recovery-producing failures print ref+oid+artifact+command fields |
| IT-G5-002 | Emitted recovery command succeeds and restores expected bytes |
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

Failpoint catalog: `notes/assurance/failpoints.md` (26 failpoint IDs across
PREPARE, BUILD, VALIDATE, COMMIT, CLEANUP, DESTROY, RECOVER boundaries).

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

1. `FP_COMMIT_AFTER_EPOCH_CAS` + `FP_COMMIT_BEFORE_BRANCH_CAS` — partial
   commit (epoch moved, branch didn't).
2. `FP_CLEANUP_AFTER_CAPTURE` + `FP_CLEANUP_BEFORE_REPLAY_INDEX` — crash
   between capture and replay.
3. `FP_DESTROY_AFTER_STATUS` + `FP_DESTROY_BEFORE_DELETE` — crash between
   status check and deletion.

### Prerequisites

DST work is blocked on the Phase 0 fix set (section 12). There is no value in
building a simulation framework that exercises code paths known to be broken.
Fix the violations first, then prove the fixes hold under crash injection.

## 10) Formal proof boundary

Formal methods are a stretch goal. They require DST to be operational first
(to validate that formal models match implementation behavior). Explicitly
marking scope and tractability.

### TLA+ — protocol safety (tractable, high value)

Model the PREPARE -> BUILD -> VALIDATE -> COMMIT -> CLEANUP state machine
with crash/restart transitions and ref movement constraints.

Variables: `epoch_ref`, `branch_ref`, `merge_state.phase`,
`workspace_heads`, `workspace_dirty`, `recovery_refs`, `destroy_records`.

Actions: `Prepare`, `Build`, `ValidatePass`, `ValidateFail`, `CommitEpoch`,
`CommitBranch`, `Cleanup`, `Abort`, `Crash`, `Recover`.

Proof obligations:
- Safety: no silent loss under any action sequence.
- Commit atomicity: COMMIT either fully completed or deterministically
  recoverable.
- Liveness: non-failing validations eventually commit under fair scheduling.

This is a bounded model check (not infinite-state proof). Practical for
2-3 workspaces and 10-20 step traces. Estimated effort: 2-3 weeks for
initial spec + check.

### Lean — merge algebra (tractable, moderate value)

Prove pure properties of the merge operator. These proofs operate on abstract
patch sets, not on filesystem/git effects.

Targets:
- Permutation determinism (workspace merge order doesn't change result).
- Idempotence on identical side sets.
- Embedding of non-conflicting side edits into merge result.
- Monotonic conflict exposure (conflicts are explicit data, not hidden drops).

These should mirror existing property tests in `src/merge/determinism_tests.rs`
and `src/merge/pushout_tests.rs` so theorem statements can be cross-checked
against executable fuzzing. Estimated effort: 3-5 weeks for initial theorems.

### What is NOT tractable

- Proving git's internal atomicity guarantees (out of scope; we assume them).
- Proving filesystem semantics end-to-end (out of scope; we assume A2).
- Proving the full Rust implementation correct (too large; DST covers this
  empirically).

## 11) Search JSON contract

Machine output for `maw ws recover --search --format json` is normatively
defined in `notes/assurance/search-schema-v1.md`.

Top-level fields: `pattern`, `workspace_filter`, `ref_filter`, `scanned_refs`,
`hit_count`, `truncated`, `hits`, `advice`.

Per-hit fields: `ref_name`, `workspace`, `timestamp`, `oid`, `oid_short`,
`path`, `line`, `snippet`.

Per-snippet-line: `line`, `text`, `is_match`.

Compatibility policy: additive fields allowed; removals/renames/type changes
require a new versioned schema document (`search-schema-v2.md`).

Status: **implemented and tested**. Schema matches implementation in
`src/workspace/recover.rs`.

## 12) Retention and security

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

## 13) Breakdown and delivery order

Phases are ordered by risk reduction. Each phase lists prerequisites and
deliverables.

### Phase 0: Stop known loss vectors (prerequisite for everything else)

**Prerequisites**: none.

**Deliverables**:
1. Remove `git checkout --force` from `update_default_workspace()`.
2. Implement shared `working_copy::preserve_checkout_replay()` primitive
   (steps 1-7 from section 5).
3. Enforce capture-gate in destroy paths: if `capture_before_destroy()` fails,
   refuse to destroy. Remove the WARNING-and-continue path at
   `src/workspace/merge.rs:3033`.
4. Tests: IT-G2-001, IT-G2-002, UT-G2-001, IT-G4-001, UT-G4-001.

**Exit criteria**: G2 and G4 status change from "violated" to "holds" in this
document.

### Phase 1: Recovery discoverability hardening

**Prerequisites**: Phase 0 (rewrite path must emit recovery surfaces before
we can test discoverability of those surfaces).

**Deliverables**:
1. Enforce output contract (section 6 required fields) on all failure paths.
2. Tests: IT-G5-001, IT-G5-002.
3. Tests: IT-G6-001, IT-G6-002.
4. Search schema compliance check (automated diff against
   `notes/assurance/search-schema-v1.md`).

**Exit criteria**: G5 status changes from "partial" to "holds".

### Phase 2: Failpoint infrastructure + fast DST

**Prerequisites**: Phase 0 (no value in crash-testing known-broken paths).

**Deliverables**:
1. `src/failpoints.rs` — feature-gated macro framework.
2. Instrument COMMIT and CLEANUP boundaries (8 failpoints).
3. Operation trace logger.
4. MVP DST harness with seeded scheduler, crash/restart loop, shrinker.
5. `dst-fast` CI gate (200-500 traces per PR).
6. Tests: DST-G1-001, DST-G3-001.

**Exit criteria**: `dst-fast` passes on PR gate with zero invariant violations.

### Phase 3: Full DST coverage

**Prerequisites**: Phase 2.

**Deliverables**:
1. Instrument remaining boundaries (PREPARE, BUILD, VALIDATE, DESTROY,
   RECOVER — 18 additional failpoints).
2. Tests: DST-G2-001, DST-G4-001.
3. `dst-nightly` CI gate (10k+ traces).
4. `incident-replay` CI gate (historical failure corpus).
5. Persist corpus under `tests/corpus/dst/`.

**Exit criteria**: nightly DST runs without invariant violation for 7
consecutive days.

### Phase 4: Formal methods (stretch)

**Prerequisites**: Phase 2 (DST must exist to cross-validate formal models).

**Deliverables**:
1. TLA+ spec for merge protocol (`formal/tla/`).
2. Lean theorems for merge algebra core (`formal/lean/`).
3. Traceability map: theorem -> source module -> DST invariant check -> CI job.
4. `formal-check` CI gate.
5. `contract-drift` CI gate (doc/code consistency).

**Exit criteria**: TLA+ model check clean for bounded params (3 workspaces,
20 steps). Lean theorems for permutation determinism and conflict monotonicity.

## 14) Maintainer checklist

For any PR touching destructive/rewrite/recovery/search behavior:

1. Update this plan if semantics change (especially the status table in
   section 4 and the invariant table in section 7).
2. Update affected docs in `notes/assurance/`.
3. Update test mappings in `notes/assurance/test-matrix.md`.
4. Ensure CI gates covering impacted claims pass.
5. If adding a new destructive code path: add a failpoint ID to
   `notes/assurance/failpoints.md` and a DST scenario.

## 15) Supporting documents

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
