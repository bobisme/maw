# bn-gjm8 implementation plan — route `sync --rebase` through `maw-core::merge`

**Goal**: Replace the `git cherry-pick` loop in `sync/rebase.rs` with iterative calls into the structured-conflict merge engine already living in `maw-core::merge` and `maw-core::model::conflict`. Close the iteration-order fidelity gap. Eliminate 18 `Command::new("git")` shell-outs in rebase.rs.

## Current state

**`sync/rebase.rs::rebase_workspace`** (700 LOC, 18 shell-outs):
1. `git checkout --detach <new_epoch>` to reset the worktree
2. `git rev-list --reverse <old_epoch>..HEAD` to enumerate commits to replay
3. Per-commit loop:
   - `git cherry-pick --no-commit <sha>`
   - On success: `git commit` with the original message
   - On conflict: `read_conflict_stages` (stages 1/2/3) → sidecar JSON → `relabel_conflict_markers` → `git add --all` → `git commit --allow-empty`
4. `git rev-list --parents` for merge-commit detection (new in bn-372v)

**The fidelity gap**: Once a conflicted iteration collapses stages 1/2/3 to stage-0 via `git add --all`, the structured representation exists only in the sidecar. Subsequent cherry-picks 3-way-merge against marker bytes, not against the conflict's sides.

## Target architecture

The `maw-core::merge` pipeline is already structured: `collect → partition → resolve → build`. Key types:

| Type | Purpose |
|---|---|
| `FileChange` / `PatchSet` | Per-workspace delta against epoch base |
| `partition_by_path(&[PatchSet])` | Splits paths into `unique` vs `shared` |
| `ResolvedChange::{Upsert, Delete}` | Clean resolution outputs |
| `Conflict::{Content, AddAdd, ModifyDelete, DivergentRename}` | Structured conflict with `base`, `sides`, `atoms` |
| `ConflictAtom` with `base_region` + `AtomEdit` per side | Localized conflict within a file |
| `build` module | Turns `Vec<ResolvedChange>` + epoch → merged tree OID + commit |

### The mapping: rebase as iterated 2-way merge

Rebase replaying commits `A → B → C` from old epoch `X` onto new epoch `Y`:

```
Iteration 1: merge(base=X, sides=[Y_delta, A_delta])
  → TreeState₁ (possibly with ConflictAtoms)
Iteration 2: merge(base=TreeState₁, sides=[B_delta])
  → TreeState₂
Iteration 3: merge(base=TreeState₂, sides=[C_delta])
  → TreeState₃
```

Where:
- `Y_delta` = `FileChange`s from `X..Y` (the "new epoch" as a workspace)
- `N_delta` = `FileChange`s from `N^..N` (each workspace commit as its own side)

The *second* and later iterations are unilateral — only one side is applied. On non-conflicted paths in `TreeState_k`, this is trivial. On conflicted paths, the unilateral change must be applied to *every side of the pre-existing conflict*, producing a new conflict whose sides are `old_side + B_delta_for_path`.

This is the core extension needed. Everything else is routing.

### What needs to change in `maw-core::merge`

**(a) A `ConflictTree` representation** that carries between iterations.

Currently the merge engine outputs `Vec<ResolvedChange>` (clean) OR `Vec<Conflict>` (conflicts reported separately). The build step only commits clean trees. For rebase we need a value that represents "a tree, some of whose paths are in structured conflict".

Proposal: a new type in `maw-core::merge::types`:

```rust
pub struct ConflictTree {
    pub clean: BTreeMap<PathBuf, MaterializedEntry>, // path → mode + oid
    pub conflicts: BTreeMap<PathBuf, Conflict>,      // path → structured conflict
    pub base_epoch: EpochId,                         // for ref-tracking
}

pub struct MaterializedEntry {
    pub mode: EntryMode,      // executable bit, symlink, blob/tree/submodule
    pub oid: GitOid,
}
```

`MaterializedEntry` carries `(mode, oid)` rather than oid alone so that executable bits, symlinks, and entry-kind distinctions survive replay — the current shell-based implementation preserves these through `git cherry-pick`, and a naive blob-only representation would silently flatten them. Non-blob entries (submodules, type changes) are explicitly out of V1 scope for *conflict* resolution but must still ride through the clean-path representation without modification; a clean symlink stays a symlink, a clean submodule gitlink stays a submodule.

This can be built once from the new epoch's full tree + iteration-1's conflicts, and subsequently updated in-place by each iteration.

**(b) `apply_unilateral_patchset(tree: ConflictTree, patch: PatchSet) → ConflictTree`**

The new engine operation. For each path in `patch`:
- If path is in `tree.clean`: straightforward — replace blob, or delete.
- If path is in `tree.conflicts`: apply the patch's `FileChange` to *each side* of the conflict's `ConflictAtom`s. The result is a new `Conflict` with updated sides. Sides that converge (both become identical after patching) collapse back to `clean`.
- If path is in neither (new file): add to `tree.clean`.

The per-atom application is the new piece. For `Conflict::Content`, it's: for each `ConflictSide` in the atom, apply the patch's change to the side's content blob and produce a new `ConflictSide`. For `Conflict::AddAdd` / etc., similar logic per variant.

**(c) `materialize(tree: ConflictTree) → MaterializedTree`**

Produces the final git tree to check out. Clean paths write their blob, preserving `mode`. Conflicted paths render diff3-style markers into a new blob (using the same label scheme the current code uses: `<<<<<<< epoch (current)`, `>>>>>>> <ws-name>`).

For V1, `maw ws resolve` remains marker-driven — existing resolver logic (`resolve.rs` scans markers in worktree files) works unchanged against the materialized output. The richer `ConflictTree` sidecar is also written alongside, for diagnostics and for the sibling migration bone.

### What needs to change in `sync/rebase.rs`

After the extension above, the rebase code becomes pure plumbing. **Critically, it still emits one rebased commit per original commit, preserving commit count ahead of epoch and original commit messages** — the observable `sync --rebase` contract is unchanged.

```rust
pub(super) fn rebase_workspace(
    repo: &impl GitRepo,
    ws_name: &str,
    old_epoch: &GitOid,
    new_epoch: &GitOid,
    ws_path: &Path,
    ahead_count: u32,
) -> Result<RebaseOutcome> {
    // 1. Enumerate commits: gix rev-walk, not `git rev-list` shell-out.
    let commits = repo.rev_walk(old_epoch, head, RevWalkOptions::reverse())?;

    // 2. Seed the ConflictTree from the new epoch's full tree, then apply the
    //    first workspace commit as a two-way merge (epoch-delta vs A-delta).
    let y_delta = diff_patchset(repo, old_epoch, new_epoch, "new-epoch")?;
    let a_delta = diff_patchset(repo, commits[0].parent(0), &commits[0], ws_name)?;
    let mut state = merge_engine::two_way(old_epoch, [y_delta, a_delta])?;

    // 3. Commit iteration 1. Parent = new_epoch; message = A's original message.
    let mut parent = new_epoch.clone();
    let (tree1, blobs1) = merge_engine::materialize(&state)?;
    let tree_oid = repo.write_tree(tree1, blobs1)?;
    parent = repo.commit_tree(tree_oid, &[parent], &commits[0].message)?;

    // 4. Iterate remaining commits, applying each as a unilateral patch and
    //    committing per step to preserve commit count + messages.
    for commit in &commits[1..] {
        let delta = diff_patchset(repo, commit.parent(0), commit, ws_name)?;
        state = if commit.is_merge() {
            // bn-372v: merge commits replay both parents as sides natively.
            let other_side = diff_patchset(repo, commit.parent(0), commit.parent(1), ws_name)?;
            merge_engine::apply_merge(state, delta, other_side)?
        } else {
            merge_engine::apply_unilateral_patchset(state, delta)?
        };

        let (tree, blobs) = merge_engine::materialize(&state)?;
        let tree_oid = repo.write_tree(tree, blobs)?;
        parent = repo.commit_tree(tree_oid, &[parent], &commit.message)?;
    }

    // 5. Update worktree and workspace HEAD via gix (no `git checkout --detach`).
    repo.update_head_and_worktree(&parent)?;

    // 6. Write sidecar in new structured form + legacy projection.
    write_rebase_state(ws_path, &state)?;      // new ConflictTree schema
    write_legacy_sidecar(ws_path, &state)?;    // flat RebaseConflicts for V1 resolver compat

    Ok(RebaseOutcome { has_conflicts: !state.conflicts.is_empty(), replayed: commits.len(), .. })
}
```

All 18 `Command::new("git")` calls are gone. The cherry-pick-failure / merge-commit / conflict paths all collapse into the merge engine. Observable behavior — replay order, per-commit history, original messages, commit count ahead of epoch — matches the current implementation.

## Key design questions (need answers before implementation)

1. **Conflict representation evolution**: the existing sidecar schema is `RebaseConflicts { conflicts: Vec<RebaseConflict { path, original_commit, base, ours, theirs } }`. The new `ConflictTree` has richer structure (atoms, regions, multiple sides per atom). Should we:
   - (a) Migrate the sidecar to the new schema and break compatibility, OR
   - (b) Keep writing the flat old schema (derived from the new state) for back-compat with `maw ws resolve`?

   My read: (b) for V1; the internal state is richer, the sidecar is a projection. Then migrate `resolve` in a follow-up.

2. **Three-way merge at the file level**: the merge engine's current partition/resolve path delegates file-level 3-way merging to a merge driver (text, union, ours, regenerate). For rebase we need the same — the per-atom merge of "side + unilateral patch" needs to respect `.gitattributes` merge drivers. The existing `load_stash_replay_attrs` pattern in `working_copy.rs:732-734` is the model.

3. **Merge commits (bn-372v interaction)**: the stub-file mechanism currently used for dropped merge commits is a workaround for cherry-pick refusal. Under the new architecture, merge commits are natively representable as "apply two sides of the merge to the current `ConflictTree`". The bn-372v stub mechanism can be deleted in favor of a proper structured representation. Needs care to preserve the test added for bn-372v.

4. **FileId / rename tracking**: the existing merge engine takes `FileChange::with_identity(file_id, blob)` to do rename-aware merging (§5.8). The rebase diff extraction needs to produce `FileChange`s with `file_id` populated. This means consulting the per-workspace FileId map at each commit boundary. Non-trivial but well-scoped.

5. **gix APIs available for rebase primitives**: need to verify that `maw-git::GitRepo` has:
   - rev-walk with ordering and merge-filter options — likely yes (used elsewhere)
   - per-commit tree diff producing blob-level deltas — needs check
   - write_tree from an in-memory tree map — needs check
   - commit_tree with arbitrary parents + message — likely yes (used in merge build)
   - update_head_and_worktree (checkout in a detached-HEAD-safe way) — currently CLI-only per the gix-migration notes

   If any primitive is missing, it gets added to `maw-git::GitRepo` as part of this work.

## Phasing

**Phase 0 — Surface verification** (DONE, 2026-04-20)

Findings from reading `crates/maw-git/src/repo.rs`, `diff_impl.rs`, `checkout_impl.rs`, `types.rs` and `crates/maw-core/src/model/file_id.rs`:

**Present in `GitRepo`** (sufficient or near-sufficient):
- Arbitrary-parent `create_commit(tree, &[parents], message, update_ref)` ✓
- `write_tree(&[TreeEntry])` with `EntryMode` covering `Blob`, `BlobExecutable`, `Tree`, `Link`, `Commit` (submodule) ✓
- `edit_tree(base, &[TreeEdit])` for in-place path-based edits ✓
- `checkout_tree(commit_or_tree, workdir)` resolves commits to trees and materializes to workdir ✓ (confirmed at `checkout_impl.rs:13-40`)
- `read_blob`, `read_tree`, `read_commit` (returns `CommitInfo { tree_oid, parents, message, author, committer }`) ✓
- `rev_parse`, `merge_base`, `is_ancestor`, `stash_create`, `stash_apply` ✓
- `DiffEntry` carries `old_mode`, `new_mode`, `old_oid`, `new_oid` — mode preservation is already wire-level ✓

**Missing from `GitRepo`** (Phase 6 additions — all small):

1. **Rev-walk API**. No `walk_commits` / `log` / `ancestors` method. Must roll manually by chasing `read_commit(oid).parents`. Worth adding a dedicated `walk_commits(from, to, reverse) → Vec<GitOid>` method. Small (gix has `rev_walk()` ready).
2. **Rename detection in `diff_trees`**. `ChangeType::Renamed { from: PathBuf }` variant is **defined in `types.rs:285`** but **never emitted** — `diff_impl.rs:79-139` only produces `Added`/`Deleted`/`Modified`. gix's rewrite-detection exists (`gix::diff::tree::with_rewrites`); needs wiring. Medium addition, well-bounded.
3. **HEAD-update helper**. `checkout_tree` updates the worktree but not HEAD. Rebase needs to point HEAD at the new commit chain. Either a new method `set_head(oid)` or a combined `checkout_commit(oid)` that does both. Small.

**`.manifold/fileids` is a current-state snapshot**, not a historical per-commit index (confirmed at `file_id.rs:11-12` and `diff.rs:273-277` which loads the map once from the workspace root). The review's concern about historical FileId fidelity is real but has a clean answer: **use tree-level rename detection (point 2 above) instead of historical FileIds**. gix's similarity-based rewrite detection gives us the same rename fidelity that git's shell-based cherry-pick currently relies on. The `FileIdMap` snapshot remains useful for merge (where both sides have a current-state map); it is not needed for rebase where we're diffing two historical commits.

**Outcome**: No prerequisite sibling bone. Phase 6 becomes three small additions to `GitRepo` (walk_commits, rename-aware diff_trees, set_head). Rename fidelity for rebase matches current shell-based behavior (similarity-based, not FileId-based).

**Phase 1 — Scaffolding** (size: s, 1–2 commits)
- Add `ConflictTree` + `MaterializedEntry` types to `maw-core::merge::types` with serde support. Preserve `mode` (not just oid).
- Add `apply_unilateral_patchset` skeleton covering the non-conflicted path only.
- Unit tests: clean-apply on unrelated path; clean-apply on a clean path already in the tree (replace blob); clean-apply deletes a clean path; preservation of `mode` across all three.

**Phase 2 — Conflict-bearing state** (size: m, 2–3 commits)
- Extend `apply_unilateral_patchset` to handle conflict-bearing paths by applying the patch to each `ConflictSide` within each `ConflictAtom`.
- Side-convergence detection (two sides become identical after patching → collapse back to clean).
- Handle the non-blob-conflict cases explicitly: chmod-conflict, type-change-conflict, submodule-boundary — either support or explicit fallback. Do NOT silently flatten.
- Unit tests: patch hits conflicted path; patch causes side convergence → collapse to clean; patch adds new divergence; patch against `ModifyDelete` conflict; patch against `AddAdd` conflict.

**Phase 3 — Historical patch extraction** (size: m, 2–3 commits) — *descoped by Phase 0 findings*

Per Phase 0, we use tree-level rename detection instead of historical FileIdMap. This matches current shell-based rebase behavior (similarity-based rename fidelity) and avoids building per-commit FileId infrastructure.

- Add `diff_patchset(repo, from_oid, to_oid, workspace_id) → PatchSet` in `maw-core::merge` that calls `repo.diff_trees(from, to)` (now rename-aware via Phase 6 addition), and translates `DiffEntry` + blob reads into `FileChange::with_identity(path, kind, content, file_id, blob)`.
- `file_id` is populated via `file_id_from_blob` (the deterministic blob-hash fallback already in `diff.rs:260-266`) when renames aren't detected. When `ChangeType::Renamed { from }` is emitted by `diff_trees`, synthesize a matched `FileId` so the merge engine treats the rename as a single tracked file.
- Tests against fixture repos with adds/modifies/deletes/renames/chmod.

**Phase 4 — Materialization** (size: s, 1 commit)
- `materialize(state) → (tree_entries, new_blobs)` producing a full `BTreeMap<PathBuf, MaterializedEntry>` plus blobs to write. Mode-preserving.
- Diff3 marker rendering for conflicted paths (reuse existing `epoch (current)` / `<ws-name> (workspace changes)` label scheme so V1 `maw ws resolve` scans them unchanged).
- `write_legacy_sidecar` projects `ConflictTree` → legacy `RebaseConflicts` schema for V1 resolver compatibility.

**Phase 5 — Rebase integration** (size: l, 3–4 commits)
- Replace `rebase_workspace` body with the pipeline from this document.
- **Emit one commit per original commit**, chaining parents, preserving original commit messages. Do not collapse to a synthetic commit.
- Delete the stub-file mechanism added in bn-372v — replaced by native merge-commit handling through `apply_merge`.
- Preserve bn-372v's integration test; it should pass with stronger guarantees (dropped content is now represented as real structured conflict, not a stub).
- Delete or refactor `relabel_conflict_markers` (markers now come from `materialize`).
- Remove all 18 `Command::new("git")` calls from rebase.rs.

**Phase 6 — `GitRepo` primitive additions** (size: s–m, scoped by Phase 0)
- Add `GitRepo::walk_commits(from: GitOid, to: GitOid, reverse: bool) → Vec<GitOid>` backed by `gix::rev_walk()`.
- Extend `GitRepo::diff_trees` to emit `ChangeType::Renamed { from }` via `gix::diff::tree::with_rewrites`. Add a `diff_trees_with_renames(old, new, similarity: u32)` variant OR gate on a new options struct so existing callers are unaffected.
- Add `GitRepo::set_head(oid)` (or `checkout_commit(oid)` combining checkout + HEAD update) for detached-HEAD-safe rebase HEAD advancement.
- End state: `rebase.rs` contains zero `Command::new("git")`.

**Phase 7 — Semantics-preservation + fidelity regression tests** (size: s, 1–2 commits)
- Preserve current contract:
  - Commit count ahead of epoch after rebase equals number of original commits replayed.
  - Original commit messages preserved verbatim (parent chaining, not squashing).
  - No silent content drops — each original commit's content is represented either cleanly or in a structured conflict.
  - `mode` preserved (executable bit round-trips; symlinks round-trip).
- Preserve bn-372v: merge commits are natively replayable; the existing bn-372v integration test passes.
- Narrow fidelity property (NOT general ordering-invariance — rebase is order-sensitive by design):
  - Two conflicted commits on *non-overlapping* paths, each landing a unilateral edit against a pre-existing conflict on a *third* path: the third path's structured sides must survive both replays with correct per-side content.
  - An unrelated unilateral edit applied to a pre-existing conflict produces an equivalent `ConflictTree` regardless of when in the sequence it is applied.

## Sibling bone (must ship in same release)

- **`bn-XXXX` — migrate `maw ws resolve` to consume `ConflictTree` sidecar directly**. After gjm8 lands, the resolver still reads markers + legacy sidecar. This follow-up routes it to the richer structured form, enabling per-atom resolution (not just whole-file keep-one-side) and better conflict diagnostics. Blocks the release; filed as a dependency of gjm8's release tag, not as a dependency of gjm8 merging to main.

## Phase 0 follow-ups (optional, not blocking)

Discovered during surface verification but out of scope for gjm8:

- **`ChangeType::Renamed` variant unused**. Adding rename detection to `diff_trees` benefits the whole codebase, not just rebase. Keep the API addition narrow inside gjm8's Phase 6; if downstream work wants to opt in, fine. No blocker.
- **No generic rev-walk**. Other code paths (merge.rs's `rev-list --count`, remnants of the pre-gix migration) could adopt `walk_commits` once it exists. Not gjm8's problem.

## Risk assessment

**High risk on Phase 3**. Everything else is either well-bounded (Phases 1–2, 4, 5, 7) or determined by scout (Phase 0 → 6). Phase 3 is where historical rename fidelity lives, and it's the most likely spot to discover we need a prerequisite bone before gjm8 itself can land. If `.manifold/fileids` is a present-state map (likely, based on how workspace tracking works), historical FileId resolution has no trivial answer — and a silent degradation to path-identity is exactly the kind of hidden regression the reviewer flagged.

**Mitigations**:
- Phase 0 determines Phase 3's cost; we don't commit to a date until after Phase 0.
- Property-based tests in Phase 2 (any patch + any state; applying then resolving = resolving then applying when the patch doesn't touch the conflicted path).
- Preserve bn-372v's integration test verbatim through Phase 5 — if it regresses, the refactor regressed the guarantee.
- Hard cut, no legacy flag (per user direction). Binary compatibility is the pre-1.0 rolling story, not a design constraint.

## Out of scope (not for this bone or sibling)

- Applying the same approach to `maw ws merge`'s stash-replay path in `working_copy.rs` (also uses a `git` shell-based 3-way merge for the "overlapping files" case). Likely a smaller parallel refactor, file as separate bone.
- Full first-class support for non-content conflicts (chmod-conflict, type-change-conflict, submodule-boundary-conflict) — Phase 2 must not *silently* flatten these, but proper resolution UX for them is out of V1.

---

## Disposition of review.1

Review reference: `sync-rebase-through-merge-engine.review.1.md` (written by external agent).

**Accepted in full**:

- **#1 Preserve commit-by-commit semantics** — the original plan's pseudocode collapsed to a single synthetic commit, which would silently break commit count and original messages. Accepted: the revised pseudocode chains parents per iteration with original messages preserved. Phase 5 explicitly calls this out.
- **#4 Preserve mode/type in replay state** — `ConflictTree.clean` changed from `PathBuf → GitOid` to `PathBuf → MaterializedEntry { mode, oid }`. Non-blob entries (submodules, type changes) must not silently flatten; Phase 2 handles them explicitly or falls back.
- **#5 Replace ordering-invariance test with actual contract** — the original "rebase in both orders, assert equivalence" was wrong: rebase is order-sensitive by design. Replaced Phase 7 with concrete contract tests (commit count, messages preserved, no content drops, mode preserved, bn-372v intact) plus a narrower fidelity property (unrelated unilateral edits commute with existing conflicts).

**Accepted with caveat (resolved by Phase 0)**:

- **#2 Phase 3 is under-scoped** — agreed in principle; scoped via Phase 0 scout (2026-04-20). Findings: `.manifold/fileids` is indeed a current-state-only snapshot, but this is **not a blocker** for rebase. The solution is tree-level rename detection in `diff_trees` (gix has rewrite detection ready to wire; `ChangeType::Renamed` is already in `types.rs:285` but unused). This matches current shell-based rebase fidelity (similarity-based renames, not FileId-based). No prerequisite sibling bone required. Phase 3 rescoped to medium; Phase 6 rescoped to small–medium.

**Accepted with user direction overriding the reviewer's V1 conservatism**:

- **#3 Resolver contract** — reviewer recommended "V1 resolver stays marker-driven, sidecar informational only". User directed: "do the migration in this release — doesn't matter if it's a separate bone or not." Compromise: V1 of gjm8 itself keeps the resolver marker-driven and writes both the legacy and new-schema sidecars. A sibling bone migrates `maw ws resolve` to consume the `ConflictTree` sidecar directly, filed as a hard-release-blocking dependency of the gjm8 release tag (not of the gjm8 merge to main). This keeps the refactor unblocked while honoring the in-release-migration constraint.

**User answers to original open questions (carried forward into this plan)**:

- Legacy flag during rollout: **no**. Hard cut.
- Sidecar schema migration in this release: **yes**, via sibling bone.
- Priority: **high**. Proceeding immediately after Phase 0 surface verification.
