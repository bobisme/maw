# maw Assurance Claims

Canonical doc: `notes/assurance-plan.md`.

Status: normative contract draft
Owner: maw maintainers

This document defines the safety contract maw claims to uphold. Anything not
explicitly listed here is out of contract.

## 1) Definitions

### Repo root

Directory containing `.manifold/` and git storage for this repo.

### Epoch

The commit currently identified by `refs/manifold/epoch/current`.

### User work

For a given workspace at operation start:

- committed work: content reachable from durable refs under `refs/**`,
  including `refs/manifold/recovery/**`;
- uncommitted tracked work: staged and unstaged deltas on tracked paths;
- uncommitted untracked work: non-ignored files not in index/HEAD.

Ignored files are out of scope unless a future revision expands this contract.

### Reachable

Reachable from durable refs only. Reflog-only reachability is not a correctness
guarantee.

### Recoverable

Restorable through documented maw CLI surfaces plus deterministic artifact/ref
locations. **Note**: searchability is a separate guarantee (G6), not part of
the definition of "recoverable". See `assurance-plan.md` section 3 for the
normative definition.

### Chunk

A bounded excerpt from a file in a recovery point (path + line range + bytes),
returned by maw without requiring a full workspace restore.

### Searchable

A recovery point is searchable if maw can deterministically search its file
contents (including untracked non-ignored files captured in snapshots) and
return matching chunks with provenance (workspace/ref + path + line numbers).

### Lost work

Work present before operation start that is neither:

- present in resulting workspace state, nor
- reachable through contract-defined recovery refs/artifacts.

## 2) Failure model assumptions

We assume:

- process crash can happen at any instruction boundary;
- power loss can happen at syscall boundaries;
- no adversarial disk corruption;
- git commands used by maw behave per supported version contracts.

We do not assume reflog retention for correctness.

## 3) Global guarantees

### G1: no silent loss of committed work

maw does not move a worktree away from committed state that is not already
durably reachable unless it first pins recoverability through durable refs.

### G2: no silent loss of uncommitted work on managed rewrites

Before any maw-initiated rewrite that can overwrite state, maw must either:

1. prove no user work exists for this boundary, or
2. capture recoverability under contract-defined ref/artifact surfaces.

### G3: post-COMMIT monotonicity

After COMMIT moves refs successfully, later cleanup failures must not
undo/obscure the successful commit and must not destroy captured user work.

### G4: Destructive gate

Any operation that can destroy/overwrite workspace state must abort or skip if
capture prerequisites fail. "Best effort destroy anyway" is forbidden.

### G5: discoverable recovery

When recoverable state exists, maw output and `maw ws recover` must make it
discoverable with executable commands.

### G6: searchable recovery

When recoverable state exists, maw must provide deterministic content search
across recovery points and must be able to show matching chunks without
restoring an entire workspace. Search must cover the captured snapshot
contents, including untracked non-ignored files.

## 4) Required enforcement points

The following are proof obligations for code + tests:

- default workspace update after merge COMMIT;
- post-merge destroy (`--destroy`) and explicit workspace destroy;
- any checkout/reset/detach flow that can orphan or overwrite state;
- sync/advance flows that rewrite worktree content.

## 5) Evidence required in CI

Claims are valid only while the following pass:

- deterministic simulation invariants for G1-G6;
- crash/failpoint replay suite over merge and rewrite boundaries;
- recoverability discoverability tests executing emitted recovery commands.

## 6) Explicit non-guarantees

The following behaviors are known limitations, not bugs. They are documented
here so that operators and agents have correct expectations. See
`notes/assurance-plan.md` section 10 (concurrency threat model) for the full
analysis.

### NG1: no concurrent merge exclusion

maw does not prevent multiple merge operations from running in parallel. Two
`maw ws merge` invocations can both proceed through PREPARE, BUILD, and
VALIDATE concurrently. Only one will succeed at COMMIT, because the epoch CAS
(`refs/manifold/epoch/current`) rejects stale callers. The losing merge wastes
compute and receives a CAS error.

**Implication for operators**: concurrent merges are safe in terms of
correctness (no data loss), but the losing merge's work is discarded. If
merge latency matters, coordinate merge invocations externally.

### NG2: no dirty-state protection during sync

`maw ws sync` checks whether a workspace has committed-ahead work (commits
not yet in the epoch) before syncing. It does **not** check for unstaged or
untracked changes. If a workspace has uncommitted modifications, `sync` will
proceed and git's own merge/checkout machinery provides some conflict
detection -- but this is git's behavior, not a maw guarantee. Uncommitted
untracked files are particularly vulnerable: git will silently overwrite them
if the incoming epoch introduces a file at the same path.

**Planned fix**: bn-34dg adds explicit dirty-state detection to
`sync_worktree_to_epoch()`. Until that lands, commit or stash all work before
running `maw ws sync`.

### NG3: destroy record writes are best-effort

When a workspace is destroyed (standalone `maw ws destroy` or post-merge
`--destroy`), maw writes a destroy record to `.manifold/destroy-records/` and
updates `latest.json`. Both standalone `destroy()` and
`handle_post_merge_destroy()` treat failures in this write as warnings, not
errors. The operation continues and the workspace is removed.

**Why this is acceptable**: the recovery ref
(`refs/manifold/recovery/<workspace>`) is the critical data for recovering
destroyed workspace content. The destroy record is metadata (timestamp, reason,
original HEAD) that aids discoverability but is not required for restoration.
`maw ws recover --search` can locate recovery refs even when destroy records
are missing.

### NG4: maw push does not check for in-progress merges

`maw push` pushes whatever the configured branch ref currently points to. It
does not check whether a merge COMMIT is in progress. If `maw push` runs
during the window between a merge's epoch CAS and branch ref update, it could
push the pre-merge branch state.

**Implication for operators**: do not run `maw push` concurrently with
`maw ws merge`. The merge operation is fast (sub-second COMMIT phase), so the
race window is small, but no interlock prevents it.

## 7) Change control

Any PR that changes destructive/rewrite behavior must update:

- this contract (if semantics change),
- `notes/assurance/working-copy.md` (rewrite semantics),
- `notes/assurance/recovery-contract.md` (recovery surfaces),
- tests proving G1-G6 remain true.
