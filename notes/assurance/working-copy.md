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
