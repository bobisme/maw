# T3.1 (bn-tmt8): SG3 Layout Design + v2→New-Layout Migration Plan

**Bone**: bn-tmt8 (T3.1, task, size m) · parent bn-2yh1 (SG3, xl) · depends bn-gh1p (SP4, done)
**Feeds**: bn-2sw3 (T3.2 Implement, xl), bn-3kkl (T3.3 Migration, l), bn-1jqo (T3.4 guardrail, m)
**Date**: 2026-05-17
**Status**: DESIGN — not an implementation. T3.2/T3.3 implement; lead reviews/merges.

This document is the authoritative spec for the SG3 layout change and is
explicitly grounded in SP4's go/no-go (`notes/layout-engine-impact.md`). Every
normative section cites the SP4 assumption number(s) it discharges. SP4's
verdict — **RELOCATION, not rewrite** — is the load-bearing premise and is
taken as given here; this doc turns that verdict into a precise spec.

---

## 0. SP4 Grounding (the premise this design rests on)

SP4 enumerated 13 layout assumptions, classified **12 trivial-relocation
(#1–8, #10–13)** and **1 real-rewrite (#9)**, with **0 real-rewrites inside
the merge/build/collect/diff3 engine**. SP4 hand-built the target layout on a
throwaway repo and ran create→commit→merge→privileged-root-sync→destroy→recover
end-to-end (8/8 stages passed), and independently verified that
`git rev-parse --git-common-dir` (the primitive `repo_root()` uses) returns the
correct root from the root checkout **and** from a deeply-nested
`.maw/worktrees/<name>` with zero code change.

This design therefore:
- Specifies the trivial-relocation set as a **mechanical path/predicate spec**
  (sections 2–6), and
- Specifies #9 — the **un-bare-the-root + v2→new-layout live migration** — as
  the **hardest sub-task**, in concrete-enough detail to feed T3.3 (section 7),
  reusing the **existing Prime-Invariant snapshot machinery** that SP4 named as
  the right tool (`update_default_workspace` Step-0 ANCHOR + `snapshot_working_copy`).

SP4's two carry-forward constraints are adopted verbatim as **normative
implementation constraints** for T3.2 (section 8):
- **C1 (from #2)**: centralize all `root.join("ws")` sites onto
  `workspaces_dir()` / `workspace_path()` **first**, so the relocation is one
  edit, not ~84.
- **C2 (from #9)**: #9 is the migration's hardest sub-task and the residual
  risk for the whole SG3; it is confined to `init.rs` / a rewritten
  `upgrade.rs`, **outside** the merge engine.

---

## 1. Target Layout (normative)

### 1.1 Current v2 layout (what we are leaving)

```
<root>/              bare repo (core.bare=true), NO working tree at root
<root>/.git/         git data (or repo.git/ common-dir topology)
<root>/.manifold/    maw metadata
<root>/ws/default/   privileged checkout (merge target, push source)
<root>/ws/<name>/    agent worktrees
<root>/.gitignore    contains `ws/`
<root>/AGENTS.md     stub redirecting to ws/default/AGENTS.md
```

### 1.2 New layout (what we are moving to)

```
<root>/                      NORMAL checkout (core.bare=false). IS the merge
                             target and push source. The branch is materialized
                             here: src/, Cargo.toml, AGENTS.md (real), .bones/…
<root>/.git/                 git data. core.bare=false. The repo.git/ common-dir
                             topology MAY be retained (see 1.4); the root is the
                             primary (non-linked) worktree of that common-dir.
<root>/.manifold/            maw metadata — UNCHANGED (SP4 #12). All
                             refs/manifold/* unchanged.
<root>/.maw/                 maw admin dir (hidden; .claude/ precedent).
<root>/.maw/worktrees/<name>/ agent worktrees (the relocated `ws/<name>`).
<root>/.gitignore            tracked file; contains `/.maw/` (see section 5).
```

Key inversions vs v2:
1. **Root is no longer bare.** `core.bare=false`; the configured branch is
   checked out at root with a live index + working tree. (SP4 #9.)
2. **The merge target is the root itself**, not `ws/default`. There is **no
   `default` workspace directory**; "default"/the privileged target ≡ the root
   checkout. (SP4 #3, #8, #10.)
3. **Agent worktrees move** from `<root>/ws/<name>` to
   `<root>/.maw/worktrees/<name>`. (SP4 #1, #2, #6, #11.)
4. `.manifold/` and `refs/manifold/*` are **byte-for-byte unaffected**. The
   Prime-Invariant machinery, epoch refs, recovery refs are layout-agnostic.
   (SP4 #12 — confirmed by SP4's recovery round-trip.)

### 1.3 Naming decision: `.maw/worktrees/` (not `.maw/ws/`)

SP4 and SG3's parent bone both name the dir `.maw/worktrees/`. Adopt that
exactly. Rationale:
- `.maw/` is the maw-private admin namespace (cf. `.claude/`, `.jj/`, `.git/`).
  It is a deliberate signal: "maw-managed, do not hand-edit." This *replaces*
  the v2 `ws/`-visibility cue, which is being deliberately retired (the
  guardrail relocates to T3.4 — agent-instruction + path + optional hook, not
  layout).
- `worktrees/` (not `ws/`) so the directory is self-describing and cannot be
  confused with the now-removed top-level `ws/`. It also reads correctly next
  to git's own `.git/worktrees/<name>` admin dirs (the *admin* keys; the
  *checkouts* live in `.maw/worktrees/<name>` — these are independent by SP4
  #6, already decoupled in `worktree_add(name, path)`).
- Future `.maw/` sub-namespaces (e.g. `.maw/cache/`) are reserved but
  out-of-scope here.

`.maw/` is **gitignored** (section 5). It is maw-runtime state, never tracked.

### 1.4 Common-dir topology

The v2 `repo.git/` common-dir normalization (init step 4) is **orthogonal** to
SG3 and is **retained as-is**. `repo_root()` derives the root as
`git-common-dir`'s parent and already handles both `<root>/.git` and
`<root>/repo.git` (and nested `<root>/.manifold/git`) — SP4 #7 verified this is
correct for a non-bare root and for nested worktrees with **zero change** on
the primary path. The only change in this area is the *fallback* ancestor-walk
predicate (section 4).

Because the root becomes the **primary worktree** (non-linked), its HEAD lives
at the common-dir's `HEAD` (e.g. `<root>/repo.git/HEAD` or `<root>/.git/HEAD`),
and its index at `<common-dir>/index`. Agent worktrees remain *linked*
worktrees with per-worktree gitdirs under `<common-dir>/worktrees/<name>/`.
This is exactly what SP4's prototype exercised (privileged-root sync writes the
merge OID into `<root>/.git/HEAD`).

---

## 2. Privileged-target ≡ root checkout (SP4 #3, #4, #5, #8, #10)

### 2.1 The single behavioral substitution

The entire engine treats the privileged target as **a `&Path` plus a branch
name plus a set of OIDs/refs**. SP4 verified none of
`update_default_workspace`, `sync_target_worktree_to_epoch`,
`handle_post_merge_destroy`, FF-absorb, or build/collect/merge/diff3 branches
on "am I bare?" or "is this `ws/`?". The substitution is therefore:

> Wherever the engine computes `default_ws_path = root.join("ws").join(default_ws)`,
> it instead uses `root` itself as the privileged-target path.

Concrete sites (from SP4 #3, verified against current source):

- `workspace/merge.rs:3417`
  `let default_ws_path = root.join("ws").join(default_ws);`
  → `let default_ws_path = target_checkout_path(&root, default_ws)?;`
  where the new helper returns `root` for the privileged/default target and
  `workspace_path(name)` (= `.maw/worktrees/<name>`) for a non-default
  branch-attached `--into <ws>` target. **`update_default_workspace`'s body is
  unchanged** — it already takes `default_ws_path: &Path`, opens a `GixRepo`
  there, writes `<gitdir>/HEAD`, resets the index, snapshots/replays. SP4
  proved this layout-agnostic in prototype stage 5; for the root the
  `ws_repo.git_dir().join("HEAD")` resolves to `<root>/.git/HEAD` (the primary
  worktree's HEAD) — SP4 #4 verified.

- `workspace/merge.rs:3079` `sync_target_worktree_to_epoch(target_ws_path, …)`
  — behaviour-identical when handed `root` (SP4 #4). No body change.

- `workspace/ff_absorb.rs` (FF-absorb target update) — same: operates on the
  `&Path` it is handed.

- `git_cwd()` (`workspace/mod.rs:1756`) currently returns
  `root.join("ws").join("default")` if it exists, else root. **Collapses to:
  return `root`.** (SP4 #8 — "simpler, not harder".) The function keeps its
  signature; its body becomes `repo_root()`.

- `resolve_merge_target` / `resolve_merge_target_workspace`
  (`workspace/mod.rs:163,233`): the sentinel `into == default_workspace`
  branch keeps `updates_epoch: true` and `branch = config.branch()`. The only
  change is that the *path* this target maps to is `root`, not
  `root/ws/default`. The `target_path = root.join("ws").join(into)` existence
  checks at lines 189–190 and 249–250 must route through `workspace_path()`
  (→ `.maw/worktrees/<into>`); for the default sentinel the path check is
  skipped (root always exists). `MawConfig.default_workspace()` keeps its
  default value `"default"` as the **logical sentinel name** of the
  privileged target; it no longer implies a `ws/default` directory. (SP4 #10.)

### 2.2 `handle_post_merge_destroy` skip predicate (SP4 #5)

`handle_post_merge_destroy` (`merge.rs:5262`) filters out the target by
**name** (`ws.as_str() != default_ws`, then a second `ws_name == default_ws`
guard at 5303). This is name-based, not path-based, so it still upholds "never
destroy the privileged target." Two refinements (no algorithm change):
- Keep the name guard (any source named the default sentinel is skipped).
- **Add a belt-and-braces path guard**: also skip if the resolved
  `backend.workspace_path(ws_id)` canonicalizes to `root` (defends against a
  future caller passing the root path as a "source"). This is a 3-line guard,
  not a rewrite, and hardens SP4 #5 against drift.

### 2.3 What does NOT change (SP4 §4)

`build` / `collect` / `merge` / diff3, the COMMIT phase that moves the branch
ref, epoch-delta injection (bn-7phd), the Step-0 ANCHOR detached-HEAD-via-raw-OID
write, snapshot/replay, recovery-ref pinning — **none** of these change. SP4's
recovery round-trip on the hand-built target layout confirms the Prime
Invariant machinery is layout-agnostic. **No SG1/DST regression is possible
from relocation** because the engine algorithms and `refs/manifold/*` are
untouched (SP4 §4 verdict; restated as an explicit non-goal here).

---

## 3. Discovery (`repo_root`) (SP4 #7)

### 3.1 Primary path — NO CHANGE

`repo_root()` (`workspace/mod.rs:1696`) asks
`git rev-parse --path-format=absolute --git-common-dir` and takes the parent
(with the `.manifold` nested-common-dir adjustment). SP4 #7 empirically
verified this returns the correct root **from the non-bare root checkout AND
from a deeply-nested `.maw/worktrees/<name>`** with zero code change. This is
the authoritative path and **must not be touched**. Discovery is
layout-agnostic by construction (git always knows its own common-dir from any
worktree).

### 3.2 Fallback ancestor-walk — ONE predicate edit

The fallback (used only when git is unavailable / not in a repo) at
`workspace/mod.rs:1735-1737`:

```rust
cwd.ancestors().find(|dir| {
    dir.join(".manifold").is_dir() && (dir.join("ws").is_dir() || dir.join(".git").exists())
})
```

Change the marker disjunction to accept the new layout while remaining
backward-tolerant during the migration window:

```rust
cwd.ancestors().find(|dir| {
    dir.join(".manifold").is_dir()
        && (dir.join(".git").exists()
            || dir.join(".maw").join("worktrees").is_dir()
            || dir.join("ws").is_dir())   // tolerated until migration completes
})
```

`.manifold/` remains the primary marker (unchanged, SP4 #12). The `ws/`
disjunct is retained **only** so a half-migrated repo (section 7 failure
window) is still discoverable; T3.3 removes it once migration is proven, or it
can stay indefinitely as harmless tolerance. **The primary path needs nothing**
(SP4 #7).

---

## 4. `workspace_path` resolution (SP4 #1, #2, #6, #11)

### 4.1 Single source of truth — the one real edit (SP4 #1)

`workspaces_dir()` (`workspace/mod.rs:1826`) and the duplicate
`GitBackend::workspaces_dir()` (`core/backend/git.rs:144`) are the single
sources of truth all `workspace_path()` callers route through *in principle*.
The normative change:

```rust
// workspace/mod.rs
fn workspaces_dir() -> Result<PathBuf> {
    Ok(repo_root()?.join(".maw").join("worktrees"))
}
// core/backend/git.rs
fn workspaces_dir(&self) -> PathBuf {
    self.root.join(".maw").join("worktrees")
}
```

`workspace_path(name)` is unchanged (it is `workspaces_dir()?.join(name)` after
validation). All path resolution flows from these two functions.

### 4.2 The ~84-site reality (SP4 #2) — constraint C1

SP4 estimated "~12" direct `root.join("ws")` sites. **Verified actual: ~84
non-test occurrences across 27 files** (`crates/maw-cli/src` +
`crates/maw-core/src`; `grep -rn 'join("ws")'`). SP4's classification is
**still correct** — every one is the identical mechanical shape
`…join("ws").join(<name>)` or `…join("ws")` — but the *volume* is ~7× SP4's
estimate. This sharpens, not weakens, constraint **C1**:

> **C1 (normative for T3.2)**: Before flipping `workspaces_dir()`, T3.2 MUST
> first replace every direct `root.join("ws")[.join(name)]` site (all 27 files;
> non-test and test) with a call to the centralized helper
> (`workspaces_dir()` / `workspace_path()` / `backend.workspace_path()` / the
> new `target_checkout_path()`). Only after centralization is the helper
> flipped once. This collapses SP4 #2 → #1 and makes the relocation a
> single semantic edit; it also prevents future drift from re-hardcoding `ws/`.

Files requiring the C1 sweep (from the grep, all under `crates/`):
`workspace/{mod,merge,status,list,recover,resolve,resolve_structured,capture,destroy_record,create,sync/{mod,checks,rebase,auto_rebase}}.rs`,
`init.rs`, `upgrade.rs`, `ref_gc.rs`, `doctor.rs`, `status.rs`,
`changes/mod.rs`, `core/backend/{git,reflink,overlay,copy}.rs`,
`core/merge/{plan,materialize}.rs`, `core/merge_state.rs`. This list is the
T3.2 work-breakdown for the centralization sweep and **re-sizes the C1 portion
of T3.2 upward** (see section 9).

### 4.3 Worktree admin key (SP4 #6) — NO CHANGE

`maw_git::worktree_impl::worktree_add(name, path)` already takes the admin key
(`name` → `.git/worktrees/<name>`) and the on-disk path independently. The
hidden nested path `<root>/.maw/worktrees/<name>` is a valid `path` arg with
**no change** to the git layer. SP4 prototype stage 2 confirmed
`git worktree add <hidden-nested-path> <oid>` works with the admin key
independent of the on-disk path.

### 4.4 `backend.list()` filter (SP4 #11) — prefix repoint only

`GitBackend::list()` (`core/backend/git.rs:419-454`) filters worktrees to those
`strip_prefix(ws_dir)` with exactly one path component. Once
`self.workspaces_dir()` returns `.maw/worktrees`, the filter shape is
**unchanged**; the root worktree is **naturally excluded** (it is not under
`.maw/worktrees/`), which is precisely correct — the root is the privileged
target, not an agent workspace. SP4 #11 verified this is the desired behaviour
with the same filter shape. The doc-comment "directly under the `ws/`
directory" updates to "`.maw/worktrees/`" (cosmetic).

---

## 5. `.gitignore` semantics (SP4 #12)

### 5.1 What changes

In v2, `.gitignore` ignores `ws/`. In the new layout:
- The tracked `.gitignore` (now a **real tracked file at the materialized
  root**, since root is a normal checkout) ignores **`/.maw/`** (anchored to
  repo root so a nested `.maw` elsewhere is not accidentally ignored).
- The legacy `ws/` entry is **removed** by migration (section 7) once no `ws/`
  exists; greenfield init writes only `/.maw/`.

```gitignore
# maw runtime/admin state — never tracked
/.maw/
```

### 5.2 What does NOT change (SP4 #12)

`.manifold/` ignore semantics and **all `refs/manifold/*`** are entirely
unaffected. SP4 #12 + SP4's recovery round-trip confirm epoch + recovery refs
work identically. `.manifold/` is already gitignored in v2 and stays so.
**The Prime-Invariant ref machinery is layout-agnostic** — restated as a
non-goal: SG3 does not touch any `refs/manifold/*` writer/reader.

### 5.3 Interaction with the now-tracked root

Because root is a live checkout, `.gitignore` itself is a tracked file
materialized at `<root>/.gitignore`. The `/.maw/` rule means agent worktrees
under `.maw/worktrees/` are invisible to `git status` at the root — exactly the
v2 property where `ws/` was invisible. Agent worktrees are *linked git
worktrees* anyway (git already excludes a registered worktree path from the
parent's status), so `/.maw/` is defense-in-depth + keeps `.maw/cache/` (future)
clean. **No source file is ever ignored** — only `/.maw/`.

---

## 6. How the engine updates the privileged root checkout

This is the mechanism SP4 prototype stage 5 mirrored verbatim and passed. It
is **reused unchanged**; only the path handed in changes (root, not
`ws/default`). Restated here as the normative spec because it is the heart of
"target = root":

After the COMMIT phase moves the branch ref to the new epoch,
`update_default_workspace(default_ws_path=root, …)` runs:

1. **Step 0 ANCHOR** (`merge.rs:4983-5024`): write the raw anchor-epoch OID
   into `<root>/.git/HEAD` (the primary worktree's HEAD file via
   `ws_repo.git_dir().join("HEAD")`), then `unstage_all()` to reset the index
   to the anchor without touching the working tree. This detaches HEAD at the
   *root's own base epoch* so the snapshot captures only genuine user edits
   relative to that, not spurious "reversions" of the merge (the bn-1k7n class
   the comment at 4988-4998 documents). **For the root, dirty user edits at
   `<root>/src/...` are the analog of dirty `ws/default` edits — same code,
   same guarantee.**
2. **Step 1 SNAPSHOT** (`merge.rs:5027`): `snapshot_working_copy(root, …)`
   captures any dirty root state into a recovery snapshot ref. (Prime
   Invariant: nothing is lost even mid-update.)
3. **Step 2 CHECKOUT** (`merge.rs:5041`): `checkout_to(root, branch, …)`
   re-attaches HEAD to the branch and materializes the merged tree at root.
4. **Step 3 REPLAY** (`merge.rs:5063-5086`): replay the snapshot
   (`replay_snapshot` or `replay_snapshot_with_merge_protection` when the
   merge resolved overlapping paths), surfacing local-vs-merge conflicts as
   labelled markers (jj-style; conflicts are data).
5. **Step 4 CLEANUP**: drop the snapshot ref on clean replay.

The FF/absorb fast path (`sync_target_worktree_to_epoch`, `ff_absorb.rs`) is
the analogous reset-keep-style materialization when the target is a strict
fast-forward; SP4 #4 verified it is behaviour-identical handed `root`
(`ws_repo.git_dir().join("HEAD")` → `<root>/.git/HEAD`).

**Net**: zero engine-body change; the privileged-root update is the existing
machinery with `root` as the path. SP4 stage 5 is the proof of correctness.

---

## 7. Migration: existing v2 `ws/` repo → new layout (SP4 #9 — the hard part)

This is the **single real-rewrite (SP4 #9)** and the **residual risk for all
of SG3**. It is **outside the merge engine** (init/migration only). It is the
core deliverable for downstream **T3.3 (bn-3kkl, size l)**; this section
specifies it concretely enough to implement.

### 7.1 The problem precisely

A live, populated v2 repo has:
- `core.bare=true` at root; **no working tree / no index at root**.
- A privileged checkout at `<root>/ws/default` that may be **dirty**
  (uncommitted user edits) and may have **committed work** ahead of the epoch.
- N agent worktrees at `<root>/ws/<name>` (each a linked worktree, possibly
  dirty, possibly conflicted).
- `refs/manifold/*` (epoch/current, per-ws state/epoch refs, recovery refs).
- Possibly an **in-flight merge** (the bn-cm63 / bn-2wyh class).

We must end at section 1.2's layout **without violating the Prime Invariant**:
no committed work lost, dirty `ws/default` edits preserved, all
`refs/manifold/*` (especially recovery refs) survive, agent worktrees relocated
intact.

The hard kernel: **un-bare the root and materialize the branch at root while
`ws/default` may be dirty**, then **decommission `ws/default`** without losing
its uncommitted edits. The existing `upgrade.rs` (v1 jj→v2) is the *pattern*
but is jj-era and must be **rewritten** for this git-native v2→new transition.

### 7.2 Reuse the existing Prime-Invariant machinery (SP4's recommendation)

SP4 explicitly states #9 is "addressable with the existing Prime-Invariant
snapshot machinery, not an engine problem." The migration MUST reuse, not
reinvent:
- `snapshot_working_copy(ws/default, repo_root, "default")` → a pinned
  recovery snapshot ref **before** any destructive step (identical to
  `update_default_workspace` Step 1).
- The Step-0 ANCHOR pattern (raw-OID HEAD write + index reset) to capture
  `ws/default`'s dirty delta relative to its *own* base epoch, not the post-
  flip root state.
- `replay_snapshot[_with_merge_protection]` to re-materialize those edits at
  the new root after the flip.
- `refs/manifold/recovery/*` pinning is the existing destroy machinery — it is
  layout-agnostic (SP4 #12) and is the safety net for every step below.

### 7.3 `maw migrate` algorithm (the T3.3 spec)

Implement as a **new `maw migrate` path** (do NOT overload the jj-era
`upgrade.rs`; replace it). Steps, each idempotent and crash-safe:

**Phase A — Preflight & freeze (refuse, don't damage)**
1. Detect layout: already-new (`<root>/.maw/worktrees` exists AND root not
   bare) → no-op success. Half-migrated → resume (Phase E checkpoint). Else v2.
2. **Refuse if an in-flight merge is detected** (reuse the bn-cm63/bn-2wyh
   crashed-merge detection: a live `refs/manifold/merge/*` or
   merge-in-progress sentinel). Message: "Finish or recover the in-flight
   merge first: `maw doctor` / `maw ws recover`." This keeps #9 *out* of the
   merge engine's concurrency surface entirely.
3. Enumerate all worktrees via `worktree_list()`; record
   `(name, path, head_oid, dirty?)`. Snapshot `refs/manifold/*` listing to a
   migration journal at `.manifold/migration/journal.json` (the crash-recovery
   checkpoint).

**Phase B — Preserve everything (Prime Invariant safety net)**
4. For `ws/default` AND **every** agent worktree: if dirty or ahead of its
   recorded epoch, take a pinned recovery snapshot via the **existing**
   `snapshot_working_copy` + recovery-ref machinery (the same refs `maw ws
   recover` reads). This is the unconditional belt: even if every later step
   fails, `maw ws recover` restores all work. Record snapshot OIDs in the
   journal.
5. Capture `ws/default`'s dirty delta specifically using the **Step-0 ANCHOR**
   technique (anchor at `ws/default`'s base epoch, snapshot) so it can be
   replayed at the root post-flip without spurious merge-reversions.

**Phase C — Relocate agent worktrees (trivial; SP4 #6)**
6. For each agent worktree `ws/<name>`: `git worktree move` (or
   forget+re-add at the recorded `head_oid` if move is unsupported for
   detached worktrees) to `<root>/.maw/worktrees/<name>`. Admin key (`name`)
   is unchanged (SP4 #6 — independent of path). `git worktree repair` to fix
   gitlinks. Verify each relocated worktree's HEAD == recorded `head_oid`
   (journal check). `refs/manifold/*` for these are by-name, **untouched**
   (SP4 #12).

**Phase D — Un-bare the root + materialize branch (the #9 kernel)**
7. `git config core.bare false`. (Inverse of `set_bare_mode`; the analog of
   removing init's step 5.)
8. Set the primary worktree HEAD: `git symbolic-ref HEAD refs/heads/<branch>`
   (reuse `upgrade.rs::fix_git_head`'s exact CLI — the symbolic-HEAD primitive
   SP4/the codebase notes is not in the gix trait yet; acceptable, run-once).
9. **Materialize the branch at root using the existing checkout machinery**,
   NOT a raw `git checkout` that could clobber: write the branch-tip OID to
   `<root>/.git/HEAD`-anchor, then `checkout_to(root, branch, …)` exactly as
   `update_default_workspace` Step 2 — root has no prior working tree (was
   bare), so this is a clean materialization with no clobber risk.
10. **Replay the `ws/default` dirty-delta snapshot** from step 5 onto the
    freshly-materialized root via `replay_snapshot_with_merge_protection`.
    Because root was empty (bare) the replay is conflict-free in the common
    case; any conflict is surfaced as labelled markers (jj-style, conflicts
    are data) and the recovery snapshot from step 4 still holds the originals.
11. Decommission the old `ws/default`: `git worktree remove --force` +
    `git worktree prune`. Its content is already (a) materialized at root and
    (b) pinned in a recovery ref — double safety. Update the logical default
    target: `MawConfig.default_workspace()` stays `"default"` as the sentinel;
    it now resolves to `root` (section 2).

**Phase E — Finalize & verify**
12. Rewrite `.gitignore`: replace any `ws/` line with `/.maw/` (or add
    `/.maw/` if absent); commit nothing automatically (let the user/agent
    commit the `.gitignore` change in a normal workspace if it differs from
    the tracked tree — typically the tracked tree already has the right
    ignore once the branch defines it; migration only fixes the *working*
    `.gitignore` so a half-state isn't confusing).
13. `rmdir <root>/ws` if empty; if non-empty (stray files), **warn, do not
    delete** (Prime Invariant: never rm unknown user data) and list them.
14. Update the journal to `complete`. Run an internal `maw doctor` equivalent
    assertion: root non-bare, branch attached, every agent worktree present
    under `.maw/worktrees/` at its recorded HEAD, every pre-migration
    `refs/manifold/*` still present (diff the journal listing). **Acceptance
    gate (matches bn-3kkl AC): `maw doctor` clean + DST oracles pass on the
    migrated repo + nothing in `maw ws recover` indicates loss.**

### 7.4 Crash-safety / idempotency

The `.manifold/migration/journal.json` checkpoint makes every phase resumable:
re-running `maw migrate` reads the journal, skips completed phases, and
re-asserts invariants. Because Phase B pins recovery snapshots for *all* work
*before* any destructive step, **a crash at any point loses nothing** — the
operator runs `maw ws recover` (already layout-agnostic, SP4 #12) and/or
re-runs `maw migrate` to resume. This is the concrete realization of SP4's
"addressable with existing Prime-Invariant snapshot machinery."

### 7.5 Why this stays outside the merge engine (SP4 §4 / C2)

Every primitive above is `init.rs`/migration scope:
`snapshot_working_copy`, `replay_snapshot`, `checkout_to`, recovery-ref
pinning, `worktree move`, `core.bare` config. The merge engine
(build/collect/merge/diff3) is **never invoked** by migration. Phase A
*refuses* if a merge is in flight, so migration and the engine's concurrency
surface never overlap. This is exactly SP4's confinement claim (#9 is
init/migration, 0 engine rewrites), made operational.

---

## 8. Normative Implementation Constraints carried to T3.2 / T3.3

- **C1 (SP4 #2, §"Secondary watch-item")**: Centralize all ~84
  `root.join("ws")` sites onto the helper FIRST (section 4.2 file list);
  flip the helper once afterward. One semantic edit, no drift.
- **C2 (SP4 #9, §"Riskiest assumption")**: #9 (un-bare live root + v2→new
  migration on a possibly-dirty default) is the migration's hardest sub-task
  and SG3's residual risk; confined to init.rs + a rewritten migrate path,
  outside the engine; built on the existing Prime-Invariant snapshot machinery
  (section 7.2/7.3).
- **C3 (SP4 §4, restated as non-goal)**: No `build`/`collect`/`merge`/diff3 /
  `refs/manifold/*` algorithm may be modified by T3.2/T3.3. Any diff touching
  those files' *logic* is out of scope and a review-blocker — relocation only.
- **C4 (SP4 #5 hardening)**: Add the path-based belt-and-braces guard in
  `handle_post_merge_destroy` (section 2.2) alongside the existing name guard.
- **C5 (sequencing, SP4 §Go/No-Go)**: T3.2 order is exactly:
  (1) C1 centralization sweep → (2) flip helper + `git_cwd` + target-resolution
  + `backend.list` prefix + ancestor-walk fallback marker (pure path edits) →
  (3) init: remove `set_bare_mode`/`clean_root_worktree`, root stays a live
  attached checkout → (4) the `maw migrate` path (T3.3) → (5) test-fixture
  sweep (SP4 #13).

---

## 9. Implications for downstream sizing & the v1.0 plan

### 9.1 T3.2 (bn-2sw3, currently xl) — sizing holds, internal mix shifts

SP4's "relocation not rewrite" premise is **validated and unchanged**, so T3.2
stays **xl** with **no engine surgery**. However, T3.1's source audit found the
C1 sweep is **~84 sites across 27 files**, ~7× SP4's "~12" estimate. This does
**not** push T3.2 past xl (the shape is uniform, mechanically substitutable,
and largely tool-assisted), but it **re-weights the xl internally**: the
dominant cost is now (1) the broad C1 centralization sweep + (5) the
test-fixture sweep (SP4 #13), as SP4 predicted, *plus a heavier mechanical
surface than SP4 sized*. Recommendation: T3.2 should land C1 as its own
reviewable commit (mechanical, greppable, zero behaviour change) before any
helper flip, so the behaviour-changing diff is tiny and auditable against C3.

### 9.2 T3.3 (bn-3kkl, currently l) — sizing holds, now fully specified

Section 7 gives T3.3 a concrete 14-step algorithm with crash-safe journaling
and explicit reuse of existing Prime-Invariant primitives, so **l is
appropriate** (it is orchestration of existing machinery, not new safety
infrastructure). The risk is concentrated and *named* (#9 / C2). T3.3's AC
("migrating a populated v2 repo loses nothing; `maw doctor` clean; DST oracles
pass on the migrated repo") maps 1:1 onto Phase B (unconditional pre-snapshot)
+ Phase E step 14 (assertion gate). The j2-era `upgrade.rs` must be **replaced,
not extended** (it is jj-based) — note this in bn-3kkl so T3.3 doesn't sink
time trying to retrofit it.

### 9.3 T3.4 (bn-1jqo) — design dependency satisfied

Section 1.3 records the deliberate retirement of the `ws/` visibility cue and
explicitly hands the guardrail to T3.4 (path-handed-to-agent + AGENTS.md +
optional hook). T3.4 is unblocked design-wise by this doc; no resizing.

### 9.4 v1.0 plan (bn-142y / SG3 bn-2yh1)

- SG3's central layout assumption is **de-risked and now specified**; no
  re-scoping of SG3 on engine grounds (SP4 §"Implications" confirmed).
- The **only** residual SG3 risk is the v2→new live migration (#9 / C2),
  now reduced to a specified, crash-safe, snapshot-backed procedure (section
  7). It is an init/migration risk, not an engine risk — the **SG1 DST gate
  and bn-cm63 / bn-2wyh / bn-3bbc guarantees are unaffected** (engine +
  `refs/manifold/*` proven layout-agnostic by SP4; restated as C3).
- Net effect on v1.0: SG3 remains gated on the **pre-registered ergonomics
  eval (T3.5 bn-1uzn)**, not on engine risk. T3.1 removes engine uncertainty
  from the SG3 critical path; the remaining gates are (a) the C1 sweep volume
  (mechanical), (b) the migration procedure correctness (section 7, testable
  against the AC), and (c) the ergonomics go/no-go (T3.5).

---

## 10. Acceptance-Criteria Self-Check (bn-tmt8)

| AC | Status | Evidence |
|----|--------|----------|
| Design explicitly grounded in SP4's go/no-go and assumption list | **MET** | Section 0 + every normative section cites SP4 assumption #s (#1–13); SP4 verdict (relocation/0-engine-rewrites) is the stated premise; C1–C5 carry SP4's two named constraints verbatim. |
| Migration approach for existing v2 repos specified (feeds T3.3) | **MET** | Section 7: 14-step `maw migrate` algorithm, crash-safe journal, explicit reuse of `snapshot_working_copy`/Step-0 ANCHOR/`replay_snapshot`/recovery refs; #9 isolated as the hard kernel; AC mapped to Phase B+E; jj-era `upgrade.rs` flagged for replacement. Section 9.2 sizes T3.3. |
| New layout specified precisely (root=checkout=target; .maw/worktrees; discovery; workspace_path; .gitignore; privileged-root update) | **MET** | §1 layout; §2 target=root; §3 discovery; §4 workspace_path; §5 .gitignore; §6 privileged-root update mechanism. |

Both formal ACs **MET**.
