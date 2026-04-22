### Executive Summary

The plan is directionally good on the core problem: the current `git cherry-pick` loop loses structured conflict information after the first conflicted replay, and consolidating rebase logic onto shared merge primitives is a strong long-term move.

The biggest gaps are semantic, not mechanical. As written, the plan would collapse rebased history into a single synthetic commit, underestimates the missing git/repo primitives needed for per-commit diff extraction and rename fidelity, and is internally inconsistent about whether `maw ws resolve` consumes conflict markers or a richer sidecar schema. It also proposes a correctness test around ordering invariance that does not match rebase semantics, and its proposed `ConflictTree` shape would drop file mode/type information that the current shell-based rebase preserves.

### Proposed Changes

#### [High Impact, Low Effort] Change #1: Preserve commit-by-commit rebase semantics

**Current State:**
The pseudocode in the plan builds a single final commit after replaying all deltas in memory.

**Proposed Change:**
State explicitly that the refactor must preserve one replayed commit per original commit, along with original commit messages and ahead-of-epoch commit count.

**Rationale:**
Current behavior and tests require `sync --rebase` to preserve commit count and original commit messages. A single synthetic commit would be a behavior regression, not just an implementation detail change.

**Benefits:**
- Preserves existing `sync --rebase` semantics.
- Keeps workspace history readable and compatible with existing tests.
- Avoids breaking invariants that rely on commit count after rebase.

**Trade-offs:**
- Requires materialization/commit at each replay step instead of one final commit.

**Implementation Notes:**
Use the structured merge engine to compute each replay step, but still emit one commit per original replayed commit. Keep the original commit message when a replay is clean; keep the explicit conflict commit subjects for conflicted replays.

**Git-Diff:**
```diff
--- /tmp/bn-gjm8-plan.md
+++ /tmp/bn-gjm8-plan.md
@@
-    // 4. Materialize + commit.
-    let materialized = merge_engine::materialize(&state)?;
-    let tree_oid = repo.write_tree(&materialized)?;
-    let commit_oid = repo.commit_tree(tree_oid, parents=[new_epoch], message="rebase: replayed")?;
+    // 4. Materialize + commit THIS replay step.
+    // Preserve one rebased commit per original commit so history shape,
+    // commit count, and commit messages match current `sync --rebase` semantics.
+    let materialized = merge_engine::materialize(&state)?;
+    let tree_oid = repo.write_tree(&materialized)?;
+    let commit_oid = repo.create_commit(
+        tree_oid,
+        &[current_parent],
+        original_commit_message,
+        None,
+    )?;
+    current_parent = commit_oid;
@@
-All 18 `Command::new("git")` calls are gone. The cherry-pick-failure / merge-commit / conflict paths all collapse into the merge engine.
+All 18 `Command::new("git")` calls are gone, but the observable rebase behavior stays the same: replay order, per-commit history, and original commit messages are preserved.
```

---

#### [High Impact, High Effort] Change #2: Re-scope Phase 3 around actual git and FileId gaps

**Current State:**
The plan treats diff extraction as a medium-sized helper and suggests resolving `file_id` by consulting the workspace FileId map at each commit boundary.

**Proposed Change:**
Promote diff extraction and rename fidelity to a first-class design problem. Call out that the current `GitRepo` surface lacks rev-walk/log APIs, `diff_trees()` does not currently emit rename records, and `.manifold/fileids` is a current-tree map rather than a historical per-commit index.

**Rationale:**
This is the biggest under-scoped part of the plan. Without a concrete design here, the refactor risks silently regressing rename-aware replay or degenerating into path-only behavior.

**Benefits:**
- Makes the hardest dependency explicit up front.
- Reduces the risk of discovering mid-implementation that the merge engine cannot reconstruct historical patch sets accurately.
- Forces an explicit decision on rename support vs temporary fallback behavior.

**Trade-offs:**
- Increases apparent scope.
- May require a preparatory bone for `maw-git` and/or FileId history work.

**Implementation Notes:**
Either add a historical patch-extraction API that emits `FileChange::with_identity(...)` for a commit range, or explicitly declare a temporary limitation for rename-aware replay and keep the current path for commits requiring it.

**Git-Diff:**
```diff
--- /tmp/bn-gjm8-plan.md
+++ /tmp/bn-gjm8-plan.md
@@
-**Phase 3 — Diff extraction** (size: m, 1-2 commits)
- - `diff_patchset(repo, from_oid, to_oid, workspace_id) → PatchSet` as a `maw-core::merge` helper.
- - FileId resolution for each entry (consult workspace FileId map or derive from blob equality).
- - Tests against fixture repos with adds/modifies/deletes/renames.
+**Phase 3 — Historical patch extraction + identity fidelity** (size: l, likely prerequisite)
+ - Add `GitRepo` support for commit walking in replay order.
+ - Add historical patch extraction for `from_commit..to_commit` with blob content and modes.
+ - Decide how rename-aware replay gets `FileId` at historical commit boundaries.
+ - Document current limitation: `.manifold/fileids` is a present-state map, not a historical per-commit index.
+ - If rename fidelity cannot be guaranteed in V1, keep a targeted fallback path rather than silently degrading replay correctness.
```

---

#### [High Impact, Low Effort] Change #3: Resolve the sidecar vs marker contract contradiction

**Current State:**
The plan says `materialize(tree)` writes a richer sidecar for `maw ws resolve` to consume, but later says migrating `maw ws resolve` to the new sidecar is out of scope.

**Proposed Change:**
Choose one V1 contract explicitly:
1. `maw ws resolve` remains marker-driven, and the new sidecar is informational only, or
2. `maw ws resolve` is migrated in-scope.

**Rationale:**
The current resolver parses diff3 markers from files and does not consume `rebase-conflicts.json`. Leaving this ambiguous makes the implementation boundary unclear and risks incomplete rollout.

**Benefits:**
- Prevents a split-brain design between worktree markers and sidecar data.
- Makes downstream compatibility requirements explicit.
- Simplifies testing and migration planning.

**Trade-offs:**
- If resolver migration is kept out of scope, V1 will carry some duplicated state.

**Implementation Notes:**
For V1, the minimal path is: keep rendering diff3 markers exactly as today, keep `maw ws resolve` marker-driven, and treat the richer sidecar as future-facing metadata only.

**Git-Diff:**
```diff
--- /tmp/bn-gjm8-plan.md
+++ /tmp/bn-gjm8-plan.md
@@
-**(c) `materialize(tree: ConflictTree) → MaterializedTree`**
-
-Produces the final git tree to check out. Clean paths write their blob. Conflicted paths render diff3-style markers into a new blob (using the same label scheme the current code uses: `<<<<<<< epoch (current)`, `>>>>>>> <ws-name>`). The sidecar JSON stores the structured `ConflictTree` for `maw ws resolve` to consume.
+**(c) `materialize(tree: ConflictTree) → MaterializedTree`**
+
+Produces the final git tree to check out. Clean paths write their blob. Conflicted paths render diff3-style markers into a new blob (using the same label scheme the current code uses: `<<<<<<< epoch (current)`, `>>>>>>> <ws-name>`).
+
+For V1, `maw ws resolve` remains marker-driven. The richer sidecar is written for diagnostics/future migration, but is not yet the resolver's source of truth.
@@
- - Migrating `maw ws resolve` to consume the new `ConflictTree` sidecar directly (instead of the legacy `RebaseConflicts` projection). File as separate bone.
+ - Migrating `maw ws resolve` to consume the new `ConflictTree` sidecar directly. Keep this as a separate bone unless explicitly pulled into this work.
```

---

#### [High Impact, Medium Effort] Change #4: Preserve file mode/type information in the replay state

**Current State:**
`ConflictTree.clean` is proposed as `path -> blob oid` only.

**Proposed Change:**
Extend the clean-state representation to carry at least entry mode in addition to blob OID, and explicitly define behavior for symlinks/submodules/type changes.

**Rationale:**
The current shell-based rebase preserves more than raw blob content. A `path -> blob oid` map is insufficient even for clean paths because executable-bit and non-blob entry semantics can be lost during materialization.

**Benefits:**
- Avoids a silent regression from current git behavior.
- Makes materialization consistent with the rest of the git abstraction layer.
- Creates a cleaner bridge to `TreeEntry` / `EntryMode` APIs already present in `maw-git`.

**Trade-offs:**
- Slightly more state to carry through the pipeline.

**Implementation Notes:**
At minimum, store `(mode, oid)` for clean entries. If non-blob entries remain out of scope for V1, say so explicitly and gate/fallback those cases instead of silently flattening them.

**Git-Diff:**
```diff
--- /tmp/bn-gjm8-plan.md
+++ /tmp/bn-gjm8-plan.md
@@
-pub struct ConflictTree {
-    pub clean: BTreeMap<PathBuf, GitOid>,           // path → blob oid (clean content)
+pub struct ConflictTree {
+    pub clean: BTreeMap<PathBuf, MaterializedEntry>, // path → mode + oid
     pub conflicts: BTreeMap<PathBuf, Conflict>,     // path → structured conflict
     pub base_epoch: EpochId,                        // for ref-tracking
 }
+
+pub struct MaterializedEntry {
+    pub mode: EntryMode,
+    pub oid: GitOid,
+}
@@
- - Extending `ConflictTree` to non-file-content conflicts (chmod, type change, submodule boundary) — current merge engine already doesn't fully handle these.
+ - Full support for chmod/type/submodule conflict resolution may stay out of scope for V1, but the replay state must not silently drop mode/type information for otherwise clean paths.
```

---

#### [Medium Impact, Low Effort] Change #5: Replace the ordering-invariance test with behavior the product actually promises

**Current State:**
Phase 7 proposes asserting equivalent structured conflict output after swapping replay ordering.

**Proposed Change:**
Replace this with tests for the actual contract:
- same number of commits ahead after rebase,
- original commit messages preserved,
- no dropped merge-commit content,
- structured conflict side data survives successive conflicted replays better than the current implementation.

**Rationale:**
Rebase is intentionally order-sensitive because later commits may depend on earlier ones. A commutativity-style test will create false failures for valid histories.

**Benefits:**
- Tests the user-visible guarantee rather than an algebraic property rebase does not have.
- Avoids locking the implementation to an invalid invariant.
- Better aligns with the existing `sync --rebase` test suite.

**Trade-offs:**
- Gives up one elegant property test in favor of several concrete regression tests.

**Implementation Notes:**
If you want a stronger correctness property, scope it narrowly to a replay-independent subset, such as a single pre-existing conflict receiving an unrelated unilateral edit.

**Git-Diff:**
```diff
--- /tmp/bn-gjm8-plan.md
+++ /tmp/bn-gjm8-plan.md
@@
-**Phase 7 — Ordering-invariance test** (size: s, 1 commit)
- - Integration test: rebase workspace with commits `A → B` where both conflict on overlapping regions. Run in both orderings (`A,B` and swap order of operations on the final merge). Assert the structured conflict output is equivalent (same sides, same atoms).
- - This locks in the fidelity property that motivates the whole refactor.
+**Phase 7 — Semantics-preservation + fidelity regression tests** (size: s, 1-2 commits)
+ - Preserve existing guarantees: commit count ahead of epoch is unchanged, original commit messages are preserved, and local content is not dropped.
+ - Preserve bn-372v coverage: merge commits must not be silently dropped.
+ - Add a focused regression showing the new engine retains structured side data across successive conflicted replays better than the current marker-byte replay path.
+ - Avoid asserting replay-order commutativity in the general case; rebase semantics are order-sensitive by design.
```
