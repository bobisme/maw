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
locations. Recoverable state must also be *searchable* (agents can locate content
by pattern) and *chunk-addressable* (agents can extract bounded file excerpts
without restoring an entire workspace).

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

### G3: post-COMMIT cleanup monotonicity

After COMMIT moves refs successfully, later cleanup failures must not imply the
commit did not succeed and must not destroy captured user work.

### G4: destructive operation gate

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

## 6) Change control

Any PR that changes destructive/rewrite behavior must update:

- this contract (if semantics change),
- `notes/assurance/working-copy.md` (rewrite semantics),
- `notes/assurance/recovery-contract.md` (recovery surfaces),
- tests proving G1-G6 remain true.
