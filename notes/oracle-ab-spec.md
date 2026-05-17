# Oracle A / Oracle B — implementation-ready specification (SP2, bn-3qxi)

Status: **VALIDATED by spike**. Handoff to **bn-1z8q** (T1.3, Oracle A) and
**bn-3ji6** (T1.4, Oracle B).

Spike artefacts:
- `spike/oracle_ab_harness.py` — drives real `maw` create/commit/merge/
  destroy/recover on a throwaway repo; computes both oracles after every
  step; plants + detects both classes of violation. **RESULT: PASS.**
- `spike/cost_scaling.py` — proves the naive Oracle-A primitive scales
  O(history); justifies the mandated incremental design.
- `spike/results.json` — measured per-step costs.

---

## 0. The single most important finding (read this first)

**Oracle A MUST be defined over CONTENT (blob) reachability, NOT over
commit-graph ancestry.** The bone's prose ("every commit ever reachable
from any workspace ref is still reachable …") is the *intent*, but a literal
commit-OID-ancestry implementation **false-positives on every single
merge** and was empirically proven wrong in this spike (harness step 7,
first run).

Why: maw's merge engine **rebuilds the merged tree and emits a fresh epoch
commit**; `maw ws destroy` writes a **fresh recovery snapshot commit**.
Neither is a descendant of the workspace's literal HEAD commit. Concretely
(observed): workspace `bob`'s tip `3b0ae41` shares only the `init` commit
with post-merge `main`; yet bob's authored file `b1.txt` (blob `58c9729`)
**is** present in `main` and in bob's recovery snapshot. No work was lost;
only the commit *identity* changed. An ancestry oracle would scream
work-loss on a perfectly correct merge.

Therefore the durable, computable invariant is: **every blob a workspace
ever authored stays reachable in the object graph from the recovery
frontier.** Commit OIDs are an implementation detail maw deliberately does
not preserve; blob content is the thing the Prime Invariant protects.

---

## 1. Definitions

### 1.1 Frontier root set `F(state)`

The set of git refs from which "still-recoverable" is measured, after a
step:

```
F =  { refs/heads/main }                                   # default history
   ∪ { refs/manifold/recovery/<ws>/<ts>  : all }           # recovery refs
   ∪ { refs/manifold/epoch/current }                       # current epoch
   ∪ { refs/manifold/epoch/ws/<ws>       : all }           # per-ws base epoch
   ∪ { refs/manifold/ws/<ws>             : all }            # materialized state
   ∪ { git rev-parse HEAD  in ws/<ws>/   : ws dir exists }  # extant ws tips
```

Explicitly **excluded** from `F`: `refs/manifold/head/<ws>` — that ref is an
**oplog-head BLOB**, not a commit (confirmed: `cat-file -t` → `blob`,
content `{"workspace_id":…,"payload":{"type":"create",…}}`). It carries no
content and is never a reachability root. It *is* the subject of Oracle B.

### 1.2 Witness set `W` (the historical content set)

`W` = the set of blob OIDs that have **ever** appeared in the tree of any
workspace tip observed at any prior step.

Maintained **incrementally**, never rescanned:
- On each step, for each extant `ws/<x>/`, read `HEAD`. If that tip OID is
  unchanged since the last observation for `<x>` → contribute nothing
  (O(1)). If changed → enumerate the blobs in that tip's tree and union
  them into `W`, tagged with `(ws, step, tip)` for shrink diagnostics.
- Optimisation for T1.3 (not needed for correctness): instead of the full
  tip tree, union only the **workspace delta** blobs (`git diff
  <ws-base-epoch> <ws-tip>` name-status → added/modified blob OIDs). The
  base-epoch tree's blobs are already covered transitively by the epoch
  refs. This shrinks `W` from O(repo) to O(authored content).

### 1.3 Reachable universe `U(F)`

`U(F)` = every object reachable from the frontier roots:
`git rev-list --objects --no-object-names <all F oids>` → set of OIDs
(commits+trees+blobs). Membership-test only; no type filtering needed
because witnesses are known blobs.

---

## 2. ORACLE A — no committed work lost

> **Oracle A holds at a state iff `W ⊆ U(F(state))`.**
>
> i.e. every blob ever authored by any workspace, at any point in the run,
> is still reachable from {default history} ∪ {recovery refs} ∪ {epoch
> refs} ∪ {extant workspace tips}.
>
> A violation is reported as: `blob <oid> authored by <ws@step> is
> unreachable from every frontier root` — this is a **Prime-Invariant
> breach** (irreversibly lost committed content).

Rationale & scope:
- Content, not commit identity (§0). Robust to maw's tree-rebuild merge
  and fresh-snapshot recovery.
- A clean `maw ws destroy --force` pins a recovery commit containing the
  workspace's content → its blobs stay in `U` → Oracle A stays green
  (verified, harness steps 8/11/14).
- A destroy that *fails* to pin recovery, a botched rebase that drops a
  side, or the 2026-02-05 incident class (conflict resolution silently
  dropping one side) all manifest as: an authored blob no longer in
  `U(F)` → Oracle A red (verified, harness step 13; `expect_a=False`).
- False-positive freedom: 12 normal-lifecycle steps incl. 2 merges, 3
  destroys, recover — **zero** false positives (harness steps 1–12, 14, 16).

Edge cases the implementer must handle (all exercised or reasoned in spike):
- **Empty/initial blob** (e.g. README) is in every frontier — trivially in
  `U`. No special-casing.
- **Reachable-but-loose**: a blob can be loose+dangling yet *unreachable*;
  Oracle A correctly fails the instant it leaves `U`, **before** any `git
  gc` — recovery must be by *ref*, not by object survival. Do **not** add
  `git gc` to the oracle; unreachable == lost for invariant purposes
  (matches Prime-Invariant doc: recovery is via `refs/manifold/recovery/*`).
- **Binary / mode-only / rename**: blob OID is content-addressed, so
  rename or mode flip with identical content keeps the same blob OID →
  still in `W`/`U`. A genuine content change creates a new blob; the old
  one only matters if it was ever a *workspace tip*'s content (it was, so
  it stays witnessed — correct: losing an older committed revision is
  still work loss).

### 2.1 Computability — **MANDATORY incremental design for T1.3**

Measured (spike, tiny repo): naive Oracle A ≈ **5.4 ms/step avg, 7.3 ms
max**. But `spike/cost_scaling.py` proves the dominating primitive
`git rev-list --objects <F>` scales **O(history depth)**: 100→8000 commits
took 2.9→20.9 ms and rises linearly. Extrapolated to 1e6 commits the naive
re-enumeration is **~2–3 s per step → O(N²) ≈ days for a 1e6-step run.
UNACCEPTABLE.**

**Required design (T1.3):** maintain a *live reachable-blob set* `U` as
mutable state across steps; do **not** recompute it from scratch.

- Snapshot `F` (ref name → OID) before and after each op (already the
  AssuranceState shape in `crates/maw-assurance/src/oracle.rs`).
- Per step, compute `ΔF` = refs whose OID changed / appeared / vanished.
- For each **added/advanced** root: `git rev-list --objects <new> ^<old>`
  (or `^<all other roots>`) — only the *newly reachable* objects — union
  into `U`.
- For each **removed/retreated** root: the objects it uniquely held may
  have left `U`. Cheapest correct approach: keep a per-root contribution
  is over-engineering; instead recompute `U` **lazily only when a witness
  test fails** — i.e. fast path is "is `b ∈ U`?"; on miss, do one
  authoritative `git rev-list` to confirm true loss before reporting.
  Misses are rare (only at genuine ref deletion / true loss), so amortized
  cost is **O(ΔF) ≈ O(1) per step**, with the expensive full scan paid
  only on a real (or suspected) violation.
- Alternative if even ΔF rev-list is too slow at extreme N: gate Oracle A
  to run every K steps + always on destroy/merge/abort ops (the only ops
  that can lose content), with a full sweep at run end. Acceptable because
  content can only leave `U` via those ops.

Budget handed to T1.3: **amortized ≤ 1 ms/step** at 1e6 steps with the
incremental set + lazy-confirm design; the per-violation full scan
(~seconds) is paid ≤ once (the run stops on first violation and shrinks).

---

## 3. ORACLE B — state coherence (must catch the bn-cm63 class)

Oracle B is a pure predicate over `(refs, ws-dirs, merge-state.json)`. It
holds iff **all** of B1–B4 hold:

- **B1 no-dangling-oplog-head.** For every `refs/manifold/head/<ws>`:
  `ws/<ws>/` exists **OR** `<ws>` ∈ `LiveMergeSources`. Otherwise →
  violation `dangling refs/manifold/head/<ws> for non-existent workspace`.
  *This is exactly the bn-cm63 class* (verified: harness step 15 fires B1;
  `maw doctor` independently emits `[WARN] stale head refs` on the same
  state — ground-truth agreement confirmed).

- **B2 owned-ref symmetry.** Same rule as B1 for the rest of the
  workspace-owned ref set — `refs/manifold/epoch/ws/<ws>` and
  `refs/manifold/ws/<ws>` (source of truth:
  `maw_core::refs::workspace_owned_refs`). A leak of any one of these for a
  gone workspace is the same coherence defect class.
  *Recovery refs are deliberately exempt — they MUST survive destroy.*

- **B3 merge-state coherence.** If `.manifold/merge-state.json` exists and
  `phase ∉ {complete, aborted}`:
  - every `sources[i]` has `ws/<src>/` **or** a `refs/manifold/recovery/
    <src>/*` ref (a source may be legitimately destroyed mid-merge — its
    content must then be pinned in recovery; this is the bn-cm63 *defended*
    path);
  - `epoch_before` resolves to a readable **commit**;
  - if `phase ∈ {commit, cleanup}` then `epoch_after` resolves to a
    readable **commit** (post-point-of-no-return the new epoch must exist).

- **B4 recovery well-formed (no orphaned recovery).** Every
  `refs/manifold/recovery/<ws>/<ts>` resolves to a readable object of type
  **commit**. A recovery ref pointing at a tree/blob/missing object is an
  orphaned/garbage recovery → violation. (This subsumes the existing G5/G6
  discoverability+searchability checks; T1.4 may delegate to or replace
  them.)

### 3.1 `LiveMergeSources` (the bn-cm63 guard — reuse production logic)

A `refs/manifold/head/<ws>` for a gone workspace is **not** a B1 violation
iff a *live* in-flight merge legitimately owns it. "Live" must reuse, not
re-derive, the production classification:

```
maw_core::merge_state::MergeStateFile::read(.manifold/merge-state.json)
  → if phase.is_terminal(): {}                       # no protection
  → staleness(now, DEFAULT_STALE_AFTER_SECS):
       Staleness::Live          → protect state.sources
       Staleness::Orphaned      → {}  (merge will never finish; its head
       Staleness::Indeterminate → {}   refs are genuinely dangling — the
                                        point of self-healing GC, bn-cm63)
```

This is **identical** to `ref_gc.rs::live_merge_source_names`. T1.4 should
call the same `maw_core::merge_state` API so the oracle and the GC guard
can never diverge. (Spike used a pid-liveness approximation; production
must use the real `staleness()`.)

### 3.2 Computability

Oracle B is **O(#refs + |sources|)** — bounded by extant workspaces +
GC-retained recovery refs + merge-state size; **independent of step
count**. Measured: **2.7 ms/step avg, 8.1 ms max** on the spike (cost is
dominated by `cat-file -t` per recovery ref for B4 — batchable via
`git cat-file --batch-check` in T1.4 to push this well under 1 ms).
No incremental design required. Trivially within budget at 1e6 steps.

---

## 4. Why both oracles are needed (orthogonality — empirically shown)

| Planted defect | Oracle A | Oracle B | `maw doctor` |
|---|---|---|---|
| Work-loss (destroy w/o recovery, dropped rebase side) | **RED** | green | n/a |
| bn-cm63 (dangling `head/<ws>`, no ws, no live merge)  | green | **RED** | `[WARN]` |

bn-cm63 was *not* a work-loss bug — merged work landed in `default` and a
recovery snapshot existed; only a coherence ref leaked. **Oracle A alone
would have missed it entirely.** Oracle B is the predicate that makes the
SG1 harness able to catch the bn-cm63 *class* (the bone's hard requirement).
Verified: in the spike each planted defect trips **exactly** its own oracle
and not the other, and Oracle B agrees with `maw doctor` ground truth.

---

## 5. Acceptance criteria — status

| Criterion (bn-3qxi) | Status | Evidence |
|---|---|---|
| Precise written predicates for A & B handed to T1.3/T1.4 | **MET** | this doc §2,§3 |
| Prototype detects planted work-loss | **MET** | harness step 13: A RED, B green |
| Prototype detects planted bn-cm63 | **MET** | harness step 15: A green, B RED; `maw doctor` agrees |
| Zero false positives over normal lifecycle | **MET** | steps 1–12,14,16 all OK (incl. 2 merges, 3 destroys) |
| Per-step cost documented, acceptable or mitigated for ≥1e6 | **MET** | §2.1: naive A is O(N²) UNACCEPTABLE → **mandated** incremental design (amortized ≤1 ms/step); B is O(1)-in-N, fine as-is |

**Overall: PASS.**

---

## 6. Direct instructions for downstream bones

**bn-1z8q (T1.3 Oracle A):**
1. Implement `W` (witness blob set) + `U` (live reachable-blob set) as
   mutable harness state, **not** recomputed per step (§1.2, §2.1).
2. Replace the misnamed `check_g1_reachability` semantics: it currently
   does *commit-ancestry*, which is the proven-wrong model (§0). Either
   re-spec G1 to blob-content reachability or add a new Oracle-A check and
   demote G1.
3. Witness contribution should use workspace **delta** vs base epoch
   (`refs/manifold/epoch/ws/<ws>`), not full tip tree, to bound `|W|`.
4. Acceptance self-test: removing the bn-cm63 fix must NOT make Oracle A
   fail (it's a B-class bug); a deliberately dropped blob MUST make A fail
   with a minimal seed.

**bn-3ji6 (T1.4 Oracle B):**
1. Implement B1–B4 (§3). Reuse `maw_core::refs::workspace_owned_refs` for
   B2 and `maw_core::merge_state::{MergeStateFile,staleness}` for the
   live-merge guard (§3.1) — do not re-derive either.
2. Batch B4 object-type checks via `git cat-file --batch-check`.
3. Acceptance self-test: a synthetic destroy-racing-in-flight-merge state
   (the bn-cm63 reproduction: `refs/manifold/head/<ws>` present,
   `ws/<ws>/` gone, merge-state terminal or absent) must trip B1; and
   Oracle B must agree with `maw doctor`'s stale-head-ref / merge-state
   verdict on a battery of hand-built incoherent states (the existing
   `doctor.rs` checks are the ground-truth oracle).

**Both:** the independent-verifier carve-out (oracle uses git CLI on the
bare `repo.git`, deliberately *not* gix) is correct and must be preserved —
see the `TODO(gix): assurance carveout` comments in `oracle.rs`.
