# SP4 (bn-gh1p): Layout Engine-Impact Assessment

**Bone**: bn-gh1p (SP4, spike) · parent bn-2yh1 · feeds T3.1 (bn-tmt8), T3.2 (bn-2sw3)
**Date**: 2026-05-17
**Status**: COMPLETE — verdict below

---

## 1. Question Under Test

The v1.0 plan proposes moving from the **current v2 layout**

```
<root>/              bare repo (core.bare=true), NO working tree
<root>/.git/         git data
<root>/.manifold/    maw metadata
<root>/ws/default/   privileged checkout (merge target, push source)
<root>/ws/<name>/    agent worktrees
```

to the **target layout**

```
<root>/                    NORMAL checkout (core.bare=false), IS the merge target
<root>/.git/               git data
<root>/.manifold/          maw metadata (UNCHANGED)
<root>/.maw/worktrees/<n>/ agent worktrees (hidden, .claude/ precedent)
```

The load-bearing claim from the plan is: **this is a RELOCATION of the merge
engine, not a REWRITE.** SP4 validates that claim by (a) enumerating every place
the engine assumes the bare-root / `ws/` layout, classifying each
`trivial-relocation` vs `real-rewrite`, and (b) hand-constructing the target
layout on a throwaway repo and running the full lifecycle with the *same git
plumbing the engine uses*.

---

## 2. Lifecycle Prototype (evidence)

`spike/run-lifecycle.sh` (transcript: `spike/lifecycle.log`) builds the target
layout from scratch and runs **create → commit → merge → privileged-target sync
→ destroy → recover**, mirroring the engine's actual primitives:

- **Worktree create**: `git worktree add <hidden-nested-path> <oid>` where the
  admin key (`.git/worktrees/<name>`) is *independent* of the on-disk path —
  exactly what `maw_git::worktree_impl::worktree_add(name, path)` does (two
  separate parameters; `name` is the admin key, `path` the location).
- **Privileged-target sync**: write the raw merge OID into the target
  worktree's `HEAD` file, `read-tree` to align the index, materialize the diff,
  re-attach the branch symref — a verbatim mirror of
  `update_default_workspace` Step-0 ANCHOR + `sync_target_worktree_to_epoch`.
  For the **root** checkout the HEAD file is simply `<root>/.git/HEAD`.
- **Destroy**: pin `refs/manifold/recovery/<name>/snapshot`, then
  `git worktree remove --force` + `git worktree prune` — mirror of
  `handle_post_merge_destroy`.
- **Recover**: restore from the pinned recovery ref into a fresh worktree.

**Result: all 8 stages passed.** The merged tree, the root checkout reflecting
the merge (incl. the overlapping-file resolution), all `refs/manifold/epoch/*`
and `refs/manifold/recovery/*` refs, and a full recovery round-trip all
succeeded with the agent worktrees living under hidden `.maw/worktrees/`.

Independently verified: `git rev-parse --git-common-dir` (the primitive
`repo_root()` uses) returns the correct root **from the root checkout AND from
a deeply-nested `.maw/worktrees/<name>`** with zero code change — discovery is
layout-agnostic by construction.

---

## 3. Enumeration of Layout Assumptions

Each assumption is classified:

- **trivial-relocation** — a path string / constant / predicate that changes
  mechanically; behaviour identical; no algorithm touched.
- **real-rewrite** — the engine's *logic* (not just a path) must change.

| # | Assumption | Where | Class | Notes |
|---|------------|-------|-------|-------|
| 1 | Workspace dir is `<root>/ws` | `workspace/mod.rs::workspaces_dir()` (single source of truth) | trivial-relocation | One-line change to `.maw/worktrees`. All `workspace_path()` callers route through here. |
| 2 | `~12` production files build `root.join("ws").join(<name>)` *directly* (bypassing the helper) | merge.rs (≥6 sites), status.rs, ref_gc.rs, doctor.rs, list.rs, init.rs, recover.rs, sync/mod.rs, destroy_record.rs, capture.rs, resolve.rs, core/backend/git.rs::`workspaces_dir()` | trivial-relocation (bulk) | Uniform mechanical substitution. **T3.2 should first centralize these on the helper, then change the helper once** — this collapses the entire class to assumption #1. The count is large but the *shape* is identical everywhere (`join("ws").join(name)` → `join(".maw").join("worktrees").join(name)`). |
| 3 | Privileged checkout is `ws/<default>` | `merge.rs:3417 default_ws_path = root.join("ws").join(default_ws)`; `git_cwd()` returns `ws/default` | trivial-relocation | Target becomes `root` itself. `default_ws_path` → `root`. `update_default_workspace` operates on a `&Path`; passing `root` instead of `root/ws/default` is sufficient — **the function body is layout-agnostic** (it opens a `GixRepo` at the path, writes `<gitdir>/HEAD`, resets the index). Proven in prototype stage 5. |
| 4 | `sync_target_worktree_to_epoch` / FF-absorb target update operates on `ws/<name>` | merge.rs:3079 | trivial-relocation | Takes `target_ws_path: &Path`. Behaviour-identical when handed `root`. `ws_repo.git_dir().join("HEAD")` correctly resolves to `<root>/.git/HEAD` for the main worktree (verified). |
| 5 | `handle_post_merge_destroy` skips the target by `name == default_ws` | merge.rs:5273, 5303 | trivial-relocation | The skip predicate is name-based, not path-based. With target=root the same guard ("never destroy the privileged target") still holds; only the *name* the guard compares to changes (or becomes a "is this the root worktree" check). No algorithm change. |
| 6 | Worktree admin key derived from / equal to `ws/<name>` path | `maw-git/worktree_impl.rs::worktree_add` | trivial-relocation | **Already decoupled today.** `worktree_add(name, path)` takes name and path as *independent* args; admin dir is `.git/worktrees/<name>`, path is arbitrary. Hidden nested path needs no change here. Prototype stage 2 confirms. |
| 7 | `repo_root()` discovery (`--git-common-dir` parent, ancestor-walk for `.manifold` + `ws`/`.git`) | `workspace/mod.rs:1696-1746` | trivial-relocation | Primary path (git-common-dir parent) is **already correct** for a non-bare root and for nested worktrees (verified empirically). Only the *fallback* ancestor-walk literal `dir.join("ws").is_dir()` predicate needs updating to also accept the new marker (`.maw/worktrees`); the primary path needs nothing. |
| 8 | `git_cwd()` returns `ws/default` else root | `workspace/mod.rs:1756` | trivial-relocation | Collapses to "return root" (root *is* the checkout). Simpler, not harder. |
| 9 | `core.bare = true` at root; root has no working tree; `clean_root_worktree` wipes the root index/checkout | `init.rs:215,218,373,391`; `upgrade.rs` | **real-rewrite (init/migration only — NOT the merge engine)** | This is the one genuine inversion: target root is a *normal* checkout (`core.bare=false`, root index + working tree are live). `set_bare_mode` and `clean_root_worktree` must be **removed/inverted** in init, and a v2→target **migration** must un-bare the root and materialize the branch there. This is real work but it is **confined to init.rs / upgrade.rs**, i.e. T3.1/T3.2 init+migration scope — it does **not** touch the merge/build/collect/diff3 engine. |
| 10 | `resolve_merge_target` / `MawConfig.default_workspace()` name a `ws/`-resident default workspace | `workspace/mod.rs:163-279`; config | trivial-relocation | Logic is name→branch/epoch resolution; only the meaning of the default target ("the root checkout" instead of "ws/default") changes. `updates_epoch` semantics unchanged. Config can keep a `default_workspace` concept or special-case the sentinel for root. |
| 11 | `backend.list()` filters worktrees to those strictly under `<root>/ws/` with exactly one path component | `core/backend/git.rs:419-454` | trivial-relocation | `strip_prefix(ws_dir)` + `components().len()==1`. Repoint `ws_dir` to `.maw/worktrees`; the root worktree is naturally excluded (it is not under `.maw/worktrees`), which is the desired behaviour (root = target, not an agent ws). Same filter shape. |
| 12 | `.gitignore` excludes `ws/`; metadata (`.manifold/`, recovery refs) layout | AGENTS.md, init `.gitignore` write | trivial-relocation | `.gitignore` swaps `ws/` → `.maw/`. **`.manifold/` and all `refs/manifold/*` are entirely unaffected** — prototype confirms epoch + recovery refs work identically. The Prime Invariant machinery is layout-agnostic. |
| 13 | Test fixtures hard-code `root.join("ws/default")` | many `#[cfg(test)]` blocks across the modules listed in #2 | trivial-relocation (test-only) | Mechanical fixture update; no production behaviour. Volume only, not risk. |

### Tally

- **trivial-relocation: 12** (#1–8, #10–13)
- **real-rewrite: 1** (#9 — and it is *not* in the merge engine; it is init +
  migration, the un-bare-the-root inversion)
- **real-rewrite inside the merge/build/collect/diff3 engine: 0**

---

## 4. Verdict: RELOCATION, not a rewrite

The load-bearing claim **holds**. The merge engine proper —
`build`/`collect`/`merge`/diff3, `update_default_workspace`,
`sync_target_worktree_to_epoch`, `handle_post_merge_destroy`,
FF-absorb — is **layout-agnostic in its logic**. Every one of these operates on
a `&Path` and a set of OIDs/refs; none branches on "am I bare?" or
"is this `ws/`?". Handing them `root` instead of `root/ws/default`, and pointing
the workspace directory at `.maw/worktrees` instead of `ws`, is sufficient.
The lifecycle prototype on the hand-built target layout passed end-to-end,
including the privileged-root-checkout update path and the Prime-Invariant
recovery round-trip.

The single `real-rewrite` (#9) is the **root un-bare + v2 migration** in
`init.rs`/`upgrade.rs`. This is genuine, non-trivial work, but it is *outside*
the merge engine and squarely inside the already-planned T3.1 design / T3.2
init+migration scope. It does not invalidate the "relocation" thesis for the
engine; it is the expected cost of the layout change itself.

**Riskiest assumption: #9** (un-bare the root + materialize the branch there as
part of v2→target migration on a *live* repo with possibly-dirty default).
Risk is concentrated in migrating *existing* v2 repos without violating the
Prime Invariant, **not** in the steady-state engine. T3.1 must specify this
migration precisely (it already lists this as an acceptance criterion).

**Secondary watch-item (cheap to neutralize):** assumption #2 — the ~12 direct
`root.join("ws")` call sites. Not risky individually, but T3.2 should
**centralize them onto `workspaces_dir()` first** so the relocation is one edit,
not twelve, and future drift can't reintroduce a hard-coded `ws/`.

### Go / No-Go for T3.1 (bn-tmt8) and T3.2 (bn-2sw3)

**GO.** The "relocation not rewrite" premise that T3.2 (xl) is sized against is
**validated**. Recommended sequencing for T3.2, grounded in this assessment:

1. Centralize all `root.join("ws")` sites onto `workspaces_dir()` (collapses #2→#1).
2. Flip `workspaces_dir()` → `.maw/worktrees`; flip `git_cwd()`/default-target
   resolution → root; update `backend.list()` prefix + ancestor-walk fallback
   marker (#1, #3, #7, #8, #10, #11) — pure path edits, engine untouched.
3. Init: remove `set_bare_mode`/`clean_root_worktree`; root stays a live
   checkout attached to the branch (#9 — the only real logic change).
4. Migration: v2 `ws/`→target — un-bare root, move/recreate worktrees under
   `.maw/worktrees`, materialize branch at root (#9, T3.1-specified).
5. Fixture sweep (#13).

T3.2 stays **xl** but the xl is dominated by (1) the mechanical-but-broad
centralization/relocation sweep and (5) the test-fixture sweep, plus (3)/(4)
init+migration — **not** by merge-engine surgery. No engine algorithm is
rewritten. The DST/chaos gate (SG1) and the bn-cm63 / bn-2wyh / bn-3bbc
guarantees are unaffected by relocation (they live in engine + `refs/manifold/*`,
both proven layout-agnostic here).

### Implications for SG3 / the v1.0 plan

- The plan's central layout assumption is **de-risked**; no re-scoping of T3.2
  needed on engine grounds.
- The residual risk to surface in T3.1 is the **v2→target migration of live
  repos** (un-bare + branch materialization without losing dirty default work)
  — this is an init/migration problem, addressable with the existing Prime
  Invariant snapshot machinery, not an engine problem.
- Recommend T3.1 explicitly carry forward assumption #2's "centralize first"
  step as an implementation constraint for T3.2, and #9 as the migration's
  hardest sub-task.
