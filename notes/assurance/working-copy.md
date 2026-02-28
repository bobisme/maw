# Working Copy Rewrite Semantics

Canonical doc: `notes/assurance-plan.md`.

Status: normative behavior for preserve/materialize/replay

This document defines how maw rewrites a workspace to a new target commit/ref
without silently losing user work.

## 1) Why the naive approach is wrong

After merge COMMIT, maw advances `refs/heads/<branch>` to the new epoch. In
`ws/default`, `HEAD` is usually a symref to that branch. That means `HEAD` can
resolve to new epoch while files on disk still reflect old epoch.

If code then runs `stash -> checkout -> stash pop`, stash may capture old epoch
checkout content as if it were user changes, and replay can accidentally restore
old epoch files over new epoch.

Conclusion: post-COMMIT rewrite logic must not use naive dirty-state replay.

## 2) Inputs and outputs

### Inputs

- `base_epoch`: commit representing current on-disk baseline for delta extraction
  (`epoch_before` for merge cleanup);
- `target_ref`: branch/ref/oid to materialize;
- workspace path;
- policy settings (strict or warn-only behavior where applicable).

### Output success

- workspace materialized at `target_ref`;
- user deltas replayed (or explicit conflict state with diagnostics);
- recovery metadata recorded when capture was required.

### Output failure

- operation aborts safely or rolls back to captured snapshot;
- no silent partial destruction;
- recovery information emitted deterministically.

## 3) Normative algorithm

1. Derive user deltas from explicit base:
   - staged tracked delta: `git diff --cached --binary <base_epoch>`
   - unstaged tracked delta: `git diff --binary`
   - untracked set (names): `git ls-files --others --exclude-standard`
2. If all deltas/manifests are empty:
   - materialize target directly (`git reset --hard <target_ref>` or equivalent)
   - done.
3. If any user work exists:
   - create durable recovery snapshot ref under
     `refs/manifold/recovery/<workspace>/<timestamp>` whose commit tree is a
     byte-for-byte capture of the entire working copy at the boundary
     (tracked + untracked non-ignored);
   - write artifacts under `.manifold/artifacts/rewrite/<workspace>/<timestamp>/`
     (metadata + deltas; artifacts do not need to duplicate full file bytes
     if the snapshot ref exists).

   The snapshot ref is the canonical source for **byte recovery** and for
   **content search** (agents can `grep` the snapshot without restoring a
   workspace).
4. Materialize target state into a clean working copy:
   - `git reset --hard <target_ref>` (or equivalent)
   - `git clean -fd` (remove untracked non-ignored files before replay)
5. Replay tracked deltas deterministically:
   - staged first: `git apply --index --3way -`
   - unstaged second: `git apply --3way -`
6. Rehydrate untracked files (when policy requires immediate replay) by
   extracting bytes from the snapshot tree and writing them back as *untracked*
   files (do not silently stage them). If immediate replay is not possible,
   keep explicit recovery command(s) and metadata so an agent/operator can
   recover selectively.
7. If replay step fails:
   - rollback to captured snapshot (or abort before destructive step if possible),
   - emit warning/error with ref, oid, artifact path, and exact recovery command.

## 4) Artifact requirements

For any capture-required rewrite, artifacts must include at minimum:

- `meta.json`: operation id, workspace, base epoch, target ref, snapshot ref/oid,
  and recommended recovery command(s);
- `index.patch`: staged tracked delta payload (can be empty file);
- `worktree.patch`: unstaged tracked delta payload (can be empty file);
- `untracked.json`: list of untracked paths observed pre-rewrite (names only;
  file bytes are recoverable from the snapshot ref).

## 5) Conflict and rollback semantics

- Conflict during replay is explicit state, never silent data drop.
- Rollback must prefer preserving user visibility of work over forcing target
  materialization.
- Post-COMMIT cleanup must not convert a committed merge into apparent failure.

## 6) Required tests

Minimum required tests for any implementation of this spec:

- dirty default with staged + unstaged + untracked survives rewrite;
- replay failure triggers rollback and preserved recoverability surfaces;
- no-user-work fast path materializes target cleanly;
- emitted recovery commands execute successfully in harness.
- content search over recovery snapshots finds known strings in both tracked and
  untracked files.

## 7) Replay Correctness Predicate

This section defines the normative correctness condition for
`preserve_checkout_replay()`. Any implementation claiming correct replay MUST
satisfy this predicate.

### Inputs

Given:

- **B** (`base_epoch`): the commit representing the on-disk baseline at the
  moment user deltas are extracted;
- **T** (`target_ref`): the commit/ref being materialized;
- **S** (`staged_delta`): the set of staged tracked changes relative to B
  (i.e., `git diff --cached --binary B`);
- **U** (`unstaged_delta`): the set of unstaged tracked changes relative to the
  index (i.e., `git diff --binary`);
- **F** (`untracked_set`): the set of untracked non-ignored files present before
  the operation, with their byte content.

### Per-path correctness

For each path P in the workspace after replay completes:

1. **Untracked (P in F):** content must match pre-operation untracked content
   byte-for-byte. Untracked files must not be staged by replay.

2. **Staged delta only (P in S, P not in U):** content in the index equals the
   result of a 3-way merge with base=B(P), ours=S(P), theirs=T(P). The
   worktree file matches the index for this path.

3. **Unstaged delta only (P in U, P not in S):** content in the worktree equals
   the result of a 3-way merge with base=B(P), ours=U(P), theirs=T(P). The
   index contains T(P) for this path.

4. **Both staged and unstaged (P in S and P in U):** the staged delta is
   applied first to the index (3-way merge base=B(P), ours=S(P), theirs=T(P)),
   then the unstaged delta is applied to the worktree (3-way merge base=B(P),
   ours=U(P), theirs=T(P)). Index and worktree may differ for this path (this
   is the normal git state for files with both staged and unstaged changes).

5. **No user delta (P only in T):** content equals T(P) exactly, in both index
   and worktree.

6. **User-deleted path (tracked deletion in S or U, P in B but not intended in
   worktree):** P must not exist in the worktree after replay. If the deletion
   was staged, P must also be absent from the index.

### Replay failure

A replay is considered failed if any of the following occur:

- Any `git apply --3way` invocation exits non-zero (conflict during delta
  application).
- After all apply steps, `git status --porcelain` reports any conflict markers
  (UU, AA, DD prefixes).

On failure:

1. Rollback: restore the workspace to the captured recovery snapshot
   (pre-operation state). The workspace must be byte-equivalent to the state
   before the rewrite began.
2. Emit: recovery ref, artifact path, and an executable recovery command (per
   the recovery contract in `recovery-contract.md`).
3. The merge COMMIT (if any) is NOT reverted -- the epoch has advanced, but the
   workspace is restored to its pre-operation content so no user work is lost.

### Replay success

A replay is considered successful when all of the following hold:

- All staged deltas from S applied without conflict.
- All unstaged deltas from U applied without conflict.
- `git status --porcelain` shows no UU/AA/DD conflict markers.
- Every path in F (untracked set) is present in the worktree with its original
  byte content, and none of these paths are staged.

### Anchor invariant

User delta extraction MUST use `epoch_before` from the merge state as the base
commit (B), NOT the post-COMMIT dirty working copy status.

Rationale: after a merge COMMIT advances the epoch, `HEAD` resolves to the new
epoch commit. If delta extraction computes diffs against this new HEAD instead
of the pre-merge baseline, the diff will contain the entire old-epoch checkout
content as "user changes." Replaying such a diff reintroduces old-epoch files
over the new-epoch target, silently corrupting the workspace.

The anchor invariant ensures that B is always the commit that was materialized
on disk when the user made their changes, so the extracted deltas represent
genuine user intent rather than epoch-transition artifacts.
