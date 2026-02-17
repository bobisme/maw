# Manifold: A Purpose-Built Version Control Layer for Concurrent AI Agents

*Research, theory, and design for the next generation of multi-agent source control.*

*Revision 2 — February 2026*

*Incorporates review corrections and design upgrades.*

---

## 1. The Problem, Precisely Stated

AI coding agents are the new unit of concurrent development. Unlike human
developers — who context-switch slowly, communicate through pull requests, and
rarely have more than 2–3 branches active simultaneously — agent swarms operate
with fundamentally different characteristics:

- **High parallelism:** 6–20 agents working simultaneously on the same codebase
- **Atomic task boundaries:** each agent works on one well-scoped task (a "bead")
  and completes it in minutes, not days
- **No persistent state between tool calls:** agents interact with files via
  absolute paths and Read/Write/Edit tools, not shells
- **Zero VCS knowledge:** agents cannot learn `jj` or `git` idioms; they need
  directories with files in them, period
- **Single-machine locality:** all agents share the same filesystem, same
  hardware, same clock — this is not a distributed-across-the-internet problem

Every existing VCS was designed for humans collaborating across machines and
timezones. maw's experience with jj — where ~2,000 of 9,500 lines exist purely
to compensate for jj's concurrency model — proves that bolting agent concurrency
onto a human-oriented VCS creates an impedance mismatch that dominates the
engineering effort.

We need something new.

### 1.1 Success Criteria (Falsifiable)

"Best VCS for multi-agent programming" is not a goal unless it can fail tests.
These are the measurable properties Manifold must satisfy:

**Concurrency / Safety**

- No global corruption or divergence under adversarial interleavings of agent
  actions. (Testable via randomized concurrent operation sequences.)
- Crash-consistency: power-loss mid-merge cannot lose committed work and cannot
  brick the repo. Recovery is automatic and deterministic.
- Deterministic results for deterministic inputs: same epoch + same workspace
  patch-sets ⇒ same merge result, regardless of operation ordering.

**Performance**

- Workspace create: sub-100 ms for repos up to 30k files. Sub-1s for repos up
  to 1M files (with CoW backends).
- Snapshot: cost proportional to changed files, not repo size.
- N-way merge: cost proportional to touched files + conflict set, not
  proportional to total workspace count.

**Agent UX**

- Agent interface never requires Git/jj semantics. Only: directories, files,
  and JSON structured output.
- Conflicts are structured and localizable — per file, per region, per edit
  atom — not "giant marker soup."
- Merges are previewable: the system can produce a deterministic merge plan
  (touched paths, predicted conflicts, configured validation commands) without
  advancing the epoch.
- Mainline stays green by default: epoch advancement can be gated on
  post-merge validation, and validation failure yields a materialized
  "quarantine" workspace representing the candidate merge result for
  fix-forward workflows.

**Git Compatibility**

- `git log`, `git bisect`, `git blame`, `git grep` on the mainline commit graph
  work normally.
- Any Manifold-only metadata can be ignored without losing mainline code history.
- Manifold state can optionally be pushed/pulled via Git mechanisms under
  `refs/manifold/*` without bespoke servers.

---

## 2. What We Learned from jj

### 2.1 The Architecture

jj (Jujutsu) is a Git-compatible VCS built on several brilliant ideas:

- **Working-copy-as-a-commit:** Every change is immediately part of a commit.
  No staging area, no "uncommitted changes" limbo. The working copy *is* a
  special commit (`@`), snapshotted automatically at the start of every command
  (overridable with `--ignore-working-copy`).

- **Operation log:** Every mutation to the repository creates an immutable
  "operation" object in an append-only DAG. Each operation points to a "view"
  object — a snapshot of all bookmarks, tags, heads, and the working-copy commit
  for *every* workspace. This gives you full undo (`jj op restore`), time travel,
  and lock-free concurrency.

- **First-class conflicts:** When a merge produces conflicts, jj records the
  conflict *in the commit* as structured data — not as text markers in files.
  Conflicted commits can be rebased, merged, and manipulated normally.

- **Content-addressable storage:** Commits, trees, and files are stored by their
  hash (using git's object model as the backend), giving deduplication and
  integrity verification for free.

- **Pluggable backends:** The architecture separates the commit backend, the
  operation backend, the op-heads backend, the index backend, and the
  working-copy backend behind trait interfaces.

### 2.2 Where It Breaks for Agents

The operation log — jj's crown jewel — is also the source of every problem maw
faces. Here's the precise failure mechanism:

1. **Single shared op log.** All workspaces share one operation DAG in
   `.jj/repo/op_store/`. The "view" object inside each operation records the
   working-copy commit for *every* workspace — a monolithic global snapshot.
   This is the concurrency amplifier: a mutation in workspace A's view is
   coupled to workspace B's view via the same snapshot object.

2. **Every command creates an operation.** Even `jj status` snapshots the
   working copy, creating a new operation that extends the DAG. This is by
   design: it's how jj tracks working-copy changes without explicit commits.

3. **Concurrent operations fork the DAG.** When agent Alice runs `jj status` in
   workspace `ws/alice` at the same time agent Bob runs `jj describe` in
   `ws/bob`, both read the same op-head, both create new operations pointing to
   it, and the op log now has two heads — an "opfork."

4. **Opforks cascade.** jj handles opforks by doing a 3-way merge of the view
   objects, but this creates a new merged operation that itself can be forked by
   the next concurrent command. With 6 agents, the op log rapidly becomes a
   tangled DAG, producing "sibling of the working copy's operation" errors,
   divergent commits, and stale workspace states.

5. **Recovery requires the same mechanism that caused the problem.** `jj op
   integrate`, `jj workspace update-stale`, divergent commit resolution — all of
   these are operations that write to the same shared op log, potentially
   forking it further.

The result: **~2,000 lines of maw code** dedicated to detecting, recovering
from, and working around jj's concurrency model. The operation log is both the
disease and the cure.

### 2.3 What to Keep

| Idea | Why it matters for agents |
|------|--------------------------|
| Working-copy-as-a-commit | Agents never need to "add" or "stage" — their edits are always captured |
| First-class conflicts | Conflicts can be recorded as data, not text markers that break parsers |
| Content-addressable objects | Deduplication across workspaces, integrity verification |
| Operation-level undo | The *concept* of undoable operations is valuable; the *implementation* as a shared DAG is not |
| Automatic file tracking | No `.gitignore` dance, no "forgot to add" errors |

### 2.4 What to Discard

| Anti-pattern | Replacement |
|-------------|-------------|
| Single shared operation log | Per-workspace operation logs (single-writer) |
| Global view snapshots (all workspaces in one view) | Per-workspace views, global state computed on read |
| Automatic snapshotting on every command | Explicit snapshot on workspace create, merge, and push |
| Bookmark/branch management exposed to agents | Opaque workspace identifiers; branches are an implementation detail |
| jj CLI as the interface | maw is the only interface; no VCS commands leak to agents |

---

## 3. Surveying the Landscape

### 3.1 Git Worktrees: The Pragmatic Baseline

Git worktrees (available since Git 2.5, 2015) provide isolated working
directories that share a single `.git` repository:

```
repo.git/           ← bare repository
├── objects/         ← shared object store
├── refs/            ← shared refs (some per-worktree, some global)
├── worktrees/
│   ├── alice/       ← per-worktree metadata (HEAD, index, logs)
│   └── bob/
ws/
├── alice/           ← independent working tree, independent HEAD, independent index
└── bob/
```

**Key property:** `git status` in worktree A has *zero effect* on worktree B.
Each worktree has its own HEAD, its own index (staging area), and its own
working tree. There is no shared mutable state for read operations. The only
shared mutable state is the object store and refs, which are protected by
lockfiles that serialize write operations.

**Important nuance:** Git's documentation clarifies that some refs are
per-worktree (HEAD, refs/bisect/*) and some are shared (refs/heads/*,
refs/tags/*). Do not build logic on assumptions about ref scope without
verifying against the documentation.

**Strengths:** Zero concurrent-read issues. Battle-tested for 10+ years. Every
CI system in the world uses them. No additional binary to install.

**Weaknesses:** No first-class conflicts (merge blocks on conflict). No
operation-level undo (only `git reflog` + `git reset`). No automatic tracking
(must `git add`). Merge is `O(n)` serial for `n` worktrees.

### 3.2 Pijul: Category-Theoretic Patches

Pijul represents the most mathematically rigorous approach to version control.
Its core insight, drawn from category theory:

> Version control is about **pushouts** in the category where files are objects
> and patches are morphisms. Two co-initial patches `p: A → B` and `q: A → C`
> merge to a pushout `P` if and only if `P` is the *minimal common state* from
> which anything reachable from `B` or `C` is also reachable from `P`.

In practice, this means:

- **Patches commute.** Applying patch A then patch B gives the same result as
  applying B then A, if the patches are independent. The repository state is
  the *set* of applied patches, not a *sequence* of commits.
- **Conflicts are first-class.** When a pushout doesn't exist in the category of
  "normal" files, the category is extended (via free co-completion) to include
  "conflicted" files — directed graphs of line chunks where ambiguous orderings
  are explicit.
- **Version identity is order-independent.** Since patches commute, version
  identifiers can use commutative functions of patch hashes. Pijul's manual
  describes a discrete-log-based identifier scheme for this purpose.

**Strengths:** Mathematically sound merge semantics. Cherry-picking "just works"
(no commit identity changes). Conflicts are data, not errors.

**Weaknesses:** Small community. Performance challenges with large histories.
Not designed for concurrent local access.

### 3.3 CRDTs: Convergent Replicated Data Types

CRDTs are data structures that can be updated concurrently on multiple replicas
without coordination, guaranteeing eventual convergence. The two main flavors:

**State-based CRDTs (CvRDTs):** Each replica maintains the full state. A merge
function `⊔` (join) combines states. The merge must be associative, commutative,
and idempotent, forming a join-semilattice.

**Operation-based CRDTs (CmRDTs):** Replicas exchange operations, not states.
Operations must be commutative for concurrent operations. Requires exactly-once,
causal-order delivery.

**Delta-state CRDTs:** A hybrid — replicas exchange only the *delta* (recent
changes to state) rather than the full state, combining the bandwidth efficiency
of CmRDTs with the delivery simplicity of CvRDTs.

**Relevant CRDT types for VCS:**

| Type | Use case |
|------|----------|
| G-Set (grow-only set) | Set of known commit/operation IDs |
| 2P-Set / OR-Set | Set of active branches/bookmarks (add + remove) |
| LWW-Register | Last-writer-wins for metadata (descriptions, timestamps) |
| MV-Register | Multi-value register for conflicting bookmark targets |
| Replicated Tree (Kleppmann) | File tree with concurrent move operations |

### 3.4 Merkle-CRDTs: The Synthesis

The Merkle-CRDT work (Sanjuán et al., Protocol Labs / IPFS lineage) synthesizes
Merkle DAGs with CRDTs:

> A **Merkle-Clock** is a Merkle-DAG where each node carries a logical timestamp.
> The DAG structure itself *is* the causal ordering — node A's CID being embedded
> in node B proves that A was known when B was created. No external vector clocks
> or version vectors are needed.

> A **Merkle-CRDT** is a Merkle-Clock whose nodes carry a CRDT payload.
> Synchronization requires broadcasting only the root CID; the receiving replica
> walks the DAG to discover and fetch missing nodes.

This is directly applicable to our problem: each workspace's operation log can
be a Merkle-CRDT — an append-only Merkle DAG where each node carries a
delta-state describing what changed. Global state is computed by merging all
workspace logs, using the DAG structure for causal ordering.

### 3.5 Structured Merge

The Mastery structured merge framework (Zhu et al., JSA 2023) reports 82.26%
merge accuracy on a large dataset of extracted Java merge scenarios, compared
against multiple baseline tools. The core insight: merge conflicts often arise
from *shifted code* (blocks of code moved between locations), and a move-aware
alignment stage before line/AST merge dramatically improves auto-merge success.

This is not theory cosplay — the prevalence of shifted code in real repositories
is measured and reported, and the improvement is demonstrated across tens of
thousands of merge scenarios.

---

## 4. Assumptions, Audited

Before the design: the things that can quietly fail.

### 4.1 "Single-machine locality" is not a free pass

Even on one machine you still have: multiple processes, non-monotonic wall-clock
time (NTP step, VM resume, sleep/wake), file notification races (inotify/fsevents
can drop events), and cross-filesystem semantics (APFS vs ext4 vs NFS mounts).

**Design response:** Manifold must be crash-safe and interleaving-safe, not
merely "distributed-safe." Every mutable operation uses fsync at commit points.
Clocks are clamped monotonically. File change detection uses explicit scanning
at snapshot time, not filesystem notifications.

### 4.2 "Agents have no persistent state between tool calls"

This is a current tooling constraint, not a law of nature. Agent frameworks will
evolve toward cached local state, long-running tool sessions, and remote
sandboxes.

**Design response:** Separate the core repo model (immutable, principled) from
the client protocol (today's tool constraints). Do not bake "tool call
statelessness" into correctness invariants. The repo model should work equally
well for stateless Read/Write/Edit agents and for future long-running agent
sessions.

### 4.3 Staleness is real — own it

The v1 design claimed "no stale workspace concept." This is only true if all
workspaces are strictly ephemeral — created from the current epoch, merged or
killed, never surviving across epoch advances.

In practice, long-running agent tasks exist. A workspace based on epoch₀ while
mainline is now at epoch₇ is *stale* — merging it requires a rebase-like
transformation (apply workspace diff from epoch₀ base onto epoch₇), which can
produce conflicts.

**Design response:** Two workspace lifetime modes:

- **Ephemeral (default):** Created from current epoch, must be merged or
  destroyed before the next epoch advance. This is the common case for agent
  beads.
- **Persistent (opt-in):** Can survive across epochs. Supports explicit
  `maw ws advance <name>` to rebase onto the latest epoch. Staleness is visible
  in `maw ws status` and `maw ws list`. Merge from a stale workspace uses the
  workspace's base epoch as merge base and applies the diff forward.

### 4.4 OverlayFS lowerdir must be immutable

If the OverlayFS lower layer points to `ws/default/` and default advances when
a new epoch is created, existing overlay mounts become semantically stale — the
mount shows the *new* default contents, not the epoch the workspace was created
from.

**Design response:** The OverlayFS lower layer must always be an *immutable
epoch snapshot directory*, not the mutable default workspace:

```
.manifold/epochs/e-{hash}/          ← immutable checkout of epoch tree
ws/alice/                            ← overlay mount
  lowerdir = .manifold/epochs/e-{hash}/
  upperdir = .manifold/cow/alice/upper/
  workdir  = .manifold/cow/alice/work/
```

This also means epoch snapshots must be retained as long as any workspace
references them, and garbage-collected only when all dependent workspaces are
destroyed.

---

## 5. The Design: Manifold

**Manifold** is a purpose-built workspace coordination layer for concurrent AI
agents, sitting on top of git's object storage. It takes jj's best ideas
(operation log, first-class conflicts, content addressing), Pijul's algebraic
insight (patches commute, conflicts are data), and CRDT theory (per-replica
logs, deterministic merge) to build something that *cannot opfork*.

### 5.1 Core Invariant

> **The Isolation Principle:** During normal operation, each workspace writes
> *only* to its own private state. No workspace reads or writes any other
> workspace's mutable state. Global state is computed on demand by a
> deterministic merge of all workspace states.

This is the fundamental difference from jj. In jj, every operation reads the
shared op-head and writes a new one — a shared-mutable-state pattern that
requires serialization. In Manifold, each workspace is a single-writer log.
Coordination happens only at merge time, driven by the lead agent, and uses
CRDT merge semantics — not locks.

### 5.2 Architecture

```
project-root/
├── .git/                    ← git object store + Manifold refs
│   └── refs/
│       └── manifold/
│           ├── head/        ← per-workspace op-log heads (blob refs)
│           │   ├── default
│           │   ├── alice
│           │   └── bob
│           ├── epoch/
│           │   └── current  ← ref to current epoch commit
│           └── ws/          ← optional per-workspace branch refs (Level 1)
│               ├── alice
│               └── bob
├── .manifold/
│   ├── config.toml          ← repo config (branch name, merge settings)
│   ├── fileids              ← FileId ↔ path mapping (see §5.8)
│   ├── merge-state          ← persisted merge state machine (see §5.10)
│   └── epochs/
│       └── e-{hash}/        ← immutable epoch snapshot directories (for CoW)
├── ws/
│   ├── default/             ← main working copy (source files)
│   ├── alice/               ← agent workspace (git worktree or CoW layer)
│   └── bob/                 ← agent workspace
└── .gitignore               ← includes ws/, .manifold/epochs/, .manifold/cow/
```

**Key difference from v1:** The operation log and all Manifold metadata are
stored as Git objects (blobs) referenced by Git refs in `refs/manifold/*`.
There is no separate content-addressed store — Git *is* the single CAS.

### 5.3 Git-Native Operation Log (Per-Workspace)

Each workspace maintains its own append-only operation log. Operations are
stored as **Git blobs** with causal links encoded in the payload. The head of
each workspace's log is a **Git ref** at `refs/manifold/head/<workspace>`.

```rust
// Serialized as canonical JSON, stored as a git blob.
// The operation ID is the git blob's SHA.
struct Operation {
    parent_ids: Vec<GitOid>,        // causal predecessors (blob OIDs)
    workspace_id: WorkspaceId,
    timestamp: OrderingKey,         // see §5.9
    payload: OpPayload,
}

enum OpPayload {
    // Workspace lifecycle
    Create {
        epoch_id: GitOid,           // the epoch commit this workspace is based on
    },
    Destroy,

    // Workspace state (patch-set model — see §5.4)
    Snapshot {
        patches: PatchSet,          // changed paths relative to epoch
    },

    // Epoch advancement (lead-only)
    Merge {
        sources: Vec<WorkspaceId>,
        epoch_before: GitOid,
        epoch_after: GitOid,
        conflicts: Vec<Conflict>,
        strategy: MergeStrategy,
    },

    // Undo (see §5.11)
    Compensate {
        target_op: GitOid,          // the operation being undone
        inverse_patches: PatchSet,  // the inverse diff
    },

    // Metadata
    Describe { message: String },
    Annotate { key: String, value: String },
}
```

**Why git-native (design option A2 from review):**

- **Single CAS.** No redundant content-addressed store alongside Git. Operations
  get Git's packing, integrity checking, and delta compression for free.
- **Debuggable.** `git cat-file -p <op-oid>` shows any operation. `git log
  --all --oneline refs/manifold/head/` shows all workspace heads.
- **Durable.** Git's object store is battle-tested for crash safety.
- **Transportable.** `git push/fetch refs/manifold/*` syncs Manifold state to
  remotes without bespoke protocols.
- **GC via reachability.** Operations are reachable from refs; unreferenced ops
  are collected by `git gc`. No custom GC needed.

**Key properties (unchanged from v1):**

1. **Single-writer.** Each log is written to by exactly one workspace. No
   concurrent writes, no locks needed, no forks possible.
2. **Content-addressed.** Operations are identified by their git blob hash.
3. **Causally ordered via Merkle structure.** If operation B's `parent_ids`
   contains A's OID, then A happened-before B.
4. **Self-verifying.** The hash chain ensures log integrity.

### 5.4 The Patch-Set Model (Replaces Tree-per-Snapshot)

**This is the single most important change from v1.** Instead of each snapshot
containing a full `tree_id` (expensive to compute, implies materializing a
complete git tree), workspace state is represented as a **patch-set relative to
the base epoch**:

```rust
/// A workspace's state is: epoch + PatchSet.
/// The PatchSet records only what changed.
struct PatchSet {
    base_epoch: GitOid,                      // which epoch these patches are relative to
    patches: BTreeMap<PathBuf, PatchValue>,   // sorted for determinism
}

enum PatchValue {
    Add {
        blob: GitOid,
        file_id: FileId,        // stable identity (see §5.8)
    },
    Delete {
        previous_blob: GitOid,
        file_id: FileId,
    },
    Modify {
        base_blob: GitOid,
        new_blob: GitOid,
        file_id: FileId,
    },
    Rename {
        from: PathBuf,
        file_id: FileId,
        new_blob: Option<GitOid>,   // if also modified
    },
}
```

**Why this matters:**

1. **Snapshot cost is proportional to changed paths only.** A workspace that
   modifies 3 files in a 100k-file repo records 3 entries, not a 100k-entry
   tree.

2. **N-way merge becomes "reduce patches by path."** Instead of diffing trees
   pairwise, the merge engine collects all patch-sets, groups by path, and
   resolves each path independently. For disjoint paths this is trivially a
   union. For overlapping paths, it's a targeted merge/conflict.

3. **Git trees are materialized only once** — when producing a new epoch commit.
   During workspace operation, no full tree is ever built.

4. **Cached workspace tree.** If any consumer needs a `tree_id` (e.g., for
   `git diff` or Level 1 inspection), it can be lazily derived from
   `epoch_tree + PatchSet` and cached. But this is optional and never required
   for core operations.

**The join operation for patch-sets sharing the same epoch:**

- **Disjoint paths:** Union. Trivially commutative.
- **Same path, identical PatchValue:** Idempotent. Drop duplicate.
- **Same path, compatible edits (same base, different new blobs):** Apply merge
  driver (diff3 / AST / agent). If clean, produce merged PatchValue.
- **Same path, incompatible edits:** Emit structured conflict (see §5.7).

### 5.5 The Epoch Model

An **epoch** is an immutable snapshot of the codebase at a synchronization
point — the state from which workspaces are derived:

```rust
struct Epoch {
    git_commit: GitOid,         // the git commit on main
    tree: GitOid,               // the git tree object
    parent_epoch: Option<GitOid>,
    created_by: GitOid,         // the Merge operation that created this epoch
}
```

The current epoch is tracked by the git ref `refs/manifold/epoch/current`.

**Lifecycle:**

1. `maw init` → creates epoch₀ from the current git HEAD.
2. `maw ws create alice` → creates workspace `alice` based on epoch₀. Alice's
   working directory starts as a view of epoch₀'s files.
3. Alice edits files → changes accumulate in her working directory.
4. `maw ws merge alice bob` → lead agent computes the merge via the epoch
   advancement transaction (§5.10). If successful, creates epoch₁.
5. New workspaces created after the merge are based on epoch₁.

**Workspace lifetime rules:**

- **Ephemeral workspaces (default):** Based on the current epoch. Must be merged
  or destroyed before the workspace is considered complete. The common case.
- **Persistent workspaces (opt-in):** May survive across epoch advances.
  `maw ws status` shows which epoch the workspace is based on and whether it's
  stale. `maw ws advance <name>` rebases the workspace's patch-set onto the
  latest epoch (recomputing patches against the new base, detecting conflicts).

**Epoch snapshot retention:** Immutable epoch snapshot directories
(`.manifold/epochs/e-{hash}/`) are retained as long as any workspace references
that epoch. Garbage collected when all dependent workspaces are destroyed.

### 5.6 The Global View: Computed, Not Stored

The global repository state is never stored as a single mutable object. It is
**computed on demand** by merging all workspace views:

```rust
fn compute_global_view(workspaces: &[Workspace]) -> GlobalView {
    let mut view = GlobalView::new();
    for ws in workspaces {
        let ws_view = ws.materialize_view();
        view.merge(ws_view);  // CRDT merge: commutative, associative, idempotent
    }
    view
}
```

The merge function forms a **join-semilattice:**

- **Workspace set:** Union (G-Set).
- **Per-workspace state:** The latest PatchSet, ordered by the workspace's
  operation log head.
- **Epoch pointer:** Max — the global epoch is the highest epoch referenced by
  `refs/manifold/epoch/current`.
- **Branch targets:** MV-Register — if two workspaces both claim to advance
  `main`, both targets are recorded (conflict is visible, not hidden).

**Incremental computation:** The global view is cached as a derived artifact
(written via atomic rename, treated as read-only and disposable by workers).
Each workspace's immutable op-log head pointer is the cache key. When any
head advances, the cache is invalidated and recomputed. This is "shared derived
state," not "shared source of truth" — the Isolation Principle is preserved.

**Bounded replay cost:** Per-workspace view checkpoints are written every N
operations (configurable, default 100) or M bytes of patch data. Log compaction
collapses older operations into a single checkpoint operation, preserving
semantics (because operations are immutable and the compacted result is
deterministic). Without this, replay cost grows unboundedly.

### 5.7 Conflict Model: Structured, Localized, Explainable

Conflicts in Manifold are structured data with *explanations*, not just "sides."

```rust
enum Conflict {
    Content {
        path: PathBuf,
        file_id: FileId,
        base: GitOid,
        sides: Vec<ConflictSide>,
        atoms: Vec<ConflictAtom>,   // localized explanation
    },
    AddAdd {
        path: PathBuf,
        sides: Vec<ConflictSide>,
    },
    ModifyDelete {
        path: PathBuf,
        file_id: FileId,
        modifier: WorkspaceId,
        deleter: WorkspaceId,
        modified_content: GitOid,
    },
    DivergentRename {
        file_id: FileId,
        original: PathBuf,
        destinations: Vec<(WorkspaceId, PathBuf)>,
    },
}

struct ConflictSide {
    workspace: WorkspaceId,
    content: GitOid,
    timestamp: OrderingKey,
}

/// A ConflictAtom localizes the conflict to a specific region.
/// This is the difference between "file has conflict" and "two edits
/// overlap at lines 42–67 modifying function `process_order`."
struct ConflictAtom {
    /// The region in the base file where the conflict occurs.
    base_region: Region,
    /// The edits from each side that are incompatible.
    edits: Vec<AtomEdit>,
    /// Why these edits conflict (optional, richer in AST-aware mode).
    reason: ConflictReason,
}

struct AtomEdit {
    workspace: WorkspaceId,
    region: Region,             // line range or AST node span
    content: String,            // the actual edit text
}

enum Region {
    Lines { start: u32, end: u32 },
    AstNode { kind: String, name: Option<String>, span: (u32, u32) },
}

enum ConflictReason {
    OverlappingLineEdits,
    SameAstNodeModified { node_kind: String, node_name: String },
    NonCommutativeEdits,        // edits that produce different results depending on order
    Custom(String),
}
```

**Why ConflictAtoms matter for agents:** An agent receiving "two edits are
non-commutative because both modify AST node `process_order` at lines 42–67"
can resolve the conflict surgically. An agent receiving "file has conflict"
with marker soup cannot.

**Three resolution tiers:**

1. **Auto-resolve (most common):** Non-overlapping changes to the same file.
   Use diff3 with epoch as base. If clean, no conflict.

2. **Structured conflict (uncommon):** Overlapping changes. Record as Conflict
   data with ConflictAtoms. Present to lead agent or human for resolution.

3. **Semantic conflict (rare):** Changes that merge textually but are
   semantically incompatible. Detected by optional post-merge validation
   (`cargo check`, `tsc`, `pytest -q`). Reported as a diagnostic.

**Conflict commits:** Conflicts are purely Manifold metadata. Git commits on
`main` are always clean. This keeps `git log` / `git bisect` / `git blame`
pristine for tooling consumption.

### 5.8 Stable File Identity

Path-only file identity is a dead end for deterministic rename handling. Files
in Manifold have a stable `FileId` that persists across renames:

```rust
/// 128-bit random identifier, assigned on file creation.
/// Survives renames, moves, and copies.
struct FileId(u128);
```

**Mappings:**

- `FileId → content_blob_id` — what the file contains (changes on modify)
- `Path → FileId` — where the file lives (changes on rename/move)

Both mappings are maintained per-epoch and per-workspace-patch-set.

**Consequences:**

- **Rename = PathA → PathB for the same FileId.** No heuristic similarity
  thresholds. Deterministic.
- **Copy = new FileId with same initial blob.** Explicit, not inferred.
- **Concurrent rename + edit:** If workspace A renames `foo.rs → bar.rs` and
  workspace B modifies `foo.rs`, Manifold sees: same FileId, one workspace
  changed the path, one changed the content. Clean merge to `bar.rs` with
  B's edits. Without FileId, this is a delete+add+modify mess.

**Git compatibility:** Git trees remain `path → blob`. The FileId mapping
lives in Manifold metadata (`.manifold/fileids` and as part of the operation
log payloads). Optionally persisted in a sidecar tracked file for portability
across clones without pushing Manifold refs.

**Introduction timeline:** FileId is a data model decision made now, but
implementation is phased. Phase 1 (git worktrees) uses path-only identity.
Phase 2 introduces FileId alongside the patch-set model.

### 5.9 Ordering: Epoch-Workspace-Sequence Keys

For causal ordering within the system, Manifold uses a composite ordering key:

```rust
struct OrderingKey {
    epoch_id: GitOid,           // which epoch (major ordering boundary)
    workspace_id: WorkspaceId,  // tie-breaker across workspaces
    seq: u64,                   // monotonic per-workspace sequence number
    wall_clock: u64,            // milliseconds since Unix epoch (human-readable, not authoritative)
}
```

**Why this instead of pure HLC:** On a single machine, full causal ordering
beyond per-workspace monotonic sequence + workspace ID tie-breaking is rarely
needed. The `(epoch_id, workspace_id, seq)` triple is the authoritative ordering
key. The wall clock is kept as a human-friendly timestamp only — informational,
never used for correctness decisions.

**Backward clock guard (mandatory):** If `wall_clock` would go backward (NTP
step, VM resume), clamp to `max(current_wall_clock, last_seen_wall_clock)`.
This is standard HLC practice and is non-negotiable.

### 5.10 Crash-Safe Epoch Advancement

The merge → new epoch transition is the only serialized operation in Manifold
and the only operation that crosses workspace boundaries. It must be
crash-proof.

**The epoch advancement state machine (persisted to `.manifold/merge-state`):**

```
Phase 1: PREPARE
  - Freeze inputs: source workspace heads + base epoch commit/tree
  - Write merge-intent record to .manifold/merge-state (fsync)
  - All inputs are now immutable references

Phase 2: BUILD
  - Collect patch-sets from all source workspaces
  - Compute merged patch-set (resolve conflicts per §5.7)
  - Write all needed git objects (blobs, trees, commit)
  - Write merge-result record with resulting candidate commit OID (fsync)

Phase 3: VALIDATE (gate)
  - Materialize a temporary checkout of the candidate commit
  - Run configured validation command(s) (see §6.3)
  - Write validation record + diagnostics to .manifold/merge-state (fsync)
  - If validation passes: proceed
  - If validation fails:
      * on_failure = "warn": proceed to COMMIT, but record diagnostics
      * on_failure = "block": stop; do not advance epoch
      * on_failure includes "quarantine": create a quarantined merge workspace
        seeded with the candidate result (see §5.12) and stop

Phase 4: COMMIT (point of no return)
  - Atomically update refs/manifold/epoch/current → new epoch commit
  - Atomically update refs/heads/main → new commit
  - Write merge-committed record (fsync)

Phase 5: CLEANUP
  - Destroy source workspaces (if --destroy policy)
  - GC stale epoch snapshot directories
  - Remove .manifold/merge-state
```

**Crash recovery:** On startup, if `.manifold/merge-state` exists, replay from
the persisted phase:

- Crashed in PREPARE: Abort. No state was changed. Workspaces intact.
- Crashed in BUILD: Abort. Git objects were written but no refs moved.
  Unreferenced objects collected by `git gc`. Workspaces intact.
- Crashed in VALIDATE: Re-run validation. Deterministic because inputs are
  frozen in PREPARE.
- Crashed in COMMIT: Check if `refs/manifold/epoch/current` was updated.
  If yes: merge succeeded, proceed to CLEANUP. If no: abort, workspaces intact.
- Crashed in CLEANUP: Re-run cleanup. Idempotent.

**The commit pointer is the only point of no return.** Everything before it is
safely abortable. Everything after it is idempotent cleanup.

If VALIDATE fails and the configured policy blocks epoch advancement, the
merge-state record is retained as a durable "merge attempt". The merge can
then be resolved via promotion (after fixes) or abandoned (§5.12).

### 5.11 Undo as Compensation Operations

The workspace lattice is monotonic — merges only move forward. But users and
agents still need undo.

**Resolution:** Undo is not deletion from the log. It is a new `Compensate`
operation that applies the inverse patch relative to the current workspace
state:

```
maw ws undo <workspace>

1. Read the workspace's latest Snapshot operation
2. Compute the inverse PatchSet (reverse all additions, deletions, modifications)
3. Append a Compensate operation to the workspace's log
4. Apply the inverse patches to the working directory
```

This preserves monotonic "history growth" (the log only grows) while allowing
content to revert. The undo is itself an undoable operation. Redo is
"compensate the compensation."

For undo of epoch advancement (much rarer): reset `refs/manifold/epoch/current`
and `refs/heads/main` to the previous epoch. This is a ref update, not a log
mutation. Source workspace logs remain intact (they were never deleted, only
marked as merged).

### 5.12 Merge Preview, Quarantine, and Machine-Readable Artifacts

Agent workflows need a way to *reason about* a merge before it moves refs, and
they need a concrete, fixable artifact when a merge is textually clean but the
result is broken.

Manifold therefore supports three related capabilities:

1. **Merge preview (dry-run) producing a deterministic merge plan**
2. **Validation-gated epoch advancement**
3. **Quarantined merge results for fix-forward workflows**

#### 5.12.1 Merge Preview (Deterministic Merge Plan)

`maw ws merge` has a preview mode that runs PREPARE+BUILD+VALIDATE but performs
no COMMIT.

```
maw ws merge alice bob --plan --json
```

**Properties:**

- No refs are updated.
- No epoch advancement occurs.
- Outputs are deterministic for deterministic inputs (frozen epoch + frozen
  workspace heads).

**Output artifact (example schema):**

```json
{
  "merge_id": "m-<opaque>",
  "epoch_before": "<git-oid>",
  "sources": ["alice", "bob"],
  "touched_paths": ["src/x.rs", "Cargo.lock"],
  "overlaps": ["src/x.rs"],
  "predicted_conflicts": [
    {
      "path": "src/x.rs",
      "kind": "Content",
      "sides": ["alice", "bob"]
    }
  ],
  "drivers": [
    {"path": "Cargo.lock", "driver": "regenerate", "command": "cargo generate-lockfile"}
  ],
  "validation": {
    "commands": ["cargo check"],
    "timeout_seconds": 60,
    "policy": "block+quarantine"
  }
}
```

The `merge_id` is stable for the frozen inputs and is used to locate derived
artifacts and (optionally) a quarantine workspace.

Recommended definition (implementation detail, but makes caching and debugging
unambiguous):

```
merge_id = sha256(
  epoch_before ||
  sorted(sources) ||
  [refs/manifold/head/<ws> for ws in sources] ||
  normalized_merge_config
)
```

Where `normalized_merge_config` includes the effective merge driver and
validation settings.

#### 5.12.2 Quarantined Merge Results (Fix-Forward)

If validation fails and the configured policy includes quarantine, Manifold
creates a workspace representing the candidate merge result:

```
ws/merge-quarantine/<merge_id>/
```

This workspace is materialized from the same frozen inputs as the merge plan
and contains:

- The candidate merged file tree (exactly what would have been committed)
- Validation diagnostics captured during VALIDATE
- A pointer to the merge intent (sources, epoch_before, candidate commit OID)

**Important invariants:**

- The epoch is not advanced.
- Source workspaces remain intact unless explicitly destroyed by policy.
- The quarantine workspace is a normal workspace: it can be edited, snapshotted,
  and merged like any other. It exists to let an agent fix-forward the
  candidate result without redoing the merge.

**Promotion path (lead-only):**

```
maw merge promote <merge_id>
```

Promotion re-runs VALIDATE on the quarantined workspace's current state and, if
green, performs COMMIT (advancing epoch + main). Promotion is idempotent and
uses the same persisted merge-state mechanism as normal epoch advancement.

**Abandon path (lead-only):**

```
maw merge abandon <merge_id>
```

Abandon removes the quarantined workspace (if any) and deletes the persisted
merge-state record. Abandon is idempotent.

#### 5.12.3 Derived Artifacts Directory

For agent UX and debugging, Manifold writes derived, disposable artifacts under
`.manifold/artifacts/`. These are *not* sources of truth and can be deleted at
any time.

Recommended layout:

```
.manifold/artifacts/
  ws/<workspace_id>/report.json
  merge/<merge_id>/plan.json
  merge/<merge_id>/validation.json
  merge/<merge_id>/diagnostics.txt
```

Artifacts are written via atomic rename and may be regenerated from git objects
and logs.


---

## 6. Merge Engine

### 6.1 Deterministic N-Way Merge

Merging in Manifold is a **lead-only, serial operation** — never performed by
worker agents. This is a deliberate design choice: merge is the one operation
that crosses workspace boundaries, so it should be the *only* serialization
point.

```
maw ws merge alice bob charlie --destroy

Step 1: Collect
  For each source workspace:
    - Scan working directory for changes since epoch
    - Record Snapshot operation with PatchSet in workspace's log

Step 2: Partition
  Build inverted index: path → [workspaces that touched it]
  - Single-workspace paths: apply directly (no conflict possible)
  - Multi-workspace paths: route to conflict resolution

Step 3: Resolve (per conflicting path)
  For a file touched by K workspaces (K > 1):
  - NOT pairwise diff3 in timestamp order (order-dependent — bad)
  - Instead: compute all K edit-sets from base, resolve once

  Resolution pipeline (stop at first success):
    a) Hash equality: if all K new blobs are identical, done
    b) diff3 line merge: all K versions against epoch base
    c) Shifted-code alignment (Mastery-inspired): detect moved blocks,
       normalize before merge, retry diff3
    d) AST-aware merge (opt-in, per language via tree-sitter):
       parse base + all variants, compute edit scripts as constraints
       over AST nodes, combine constraints, check for cycles.
       Acyclic: topologically apply edits. Cyclic: emit structured conflict
       with SCC as minimal conflicting set
    e) Emit structured Conflict with ConflictAtoms

Step 4: Build
  Start with epoch tree
  Apply all resolved patches
  Produce result git tree + git commit

Step 5: Validate + Commit (via §5.10 state machine)
  Run post-merge validation (if configured)
  If validation passes (or is configured to warn): atomically advance epoch
  If validation blocks: do not advance epoch; optionally materialize quarantine
```

**Determinism guarantee:** For the same set of input patch-sets against the same
epoch, the merge result is identical regardless of workspace creation order,
operation timing, or system state. This is achieved by:

- Sorting paths lexicographically for processing order
- Using file content (blob OID) for resolution, not timestamps
- Deterministic merge algorithms at every level (diff3 is deterministic;
  AST merge uses topological sort with lexicographic tie-breaking)

### 6.2 Merge Pipeline Layering

The merge engine is a pipeline with stop conditions, not a monolithic algorithm:

| Layer | Cost | Success Rate | When |
|-------|------|-------------|------|
| Hash equality | O(1) | Handles identical edits | Always |
| diff3 line merge | O(n) lines | ~95% of real conflicts | Always |
| Shifted-code alignment | O(n log n) | +5-10% over diff3 | If diff3 reports conflict |
| AST-aware structured merge | O(n·m) AST nodes | +5-10% over shifted-code | If enabled for language |
| Agent-assisted resolution | $$$ (LLM call) | Variable | If conflict core is small, file is in supported language, post-merge validation exists |

After every layer, and always after the final merge: run fast validation
(`cargo check`, `tsc`, `pytest -q`, etc.) if configured. If validation fails,
record a semantic conflict diagnostic.

### 6.3 Post-Merge Validation

Optional but strongly recommended. Configured in `.manifold/config.toml`:

```toml
[merge.validation]
command = "cargo check"
timeout_seconds = 60
on_failure = "block+quarantine"  # warn | block | quarantine | block+quarantine
```

Semantic conflicts (textually clean merge that breaks the build) are reported as
diagnostics attached to the Merge operation, not as VCS conflicts.

When `on_failure` includes `quarantine`, a failed validation produces a
quarantined workspace seeded with the candidate merge result (§5.12).

### 6.4 Deterministic Merge Drivers for Lockfiles and Generated Artifacts

Certain files are not meaningfully "merged" by hand (lockfiles, generated code,
snapshots). For these, Manifold supports deterministic drivers.

**Driver kinds:**

- `regenerate`: ignore conflicting edits and re-generate deterministically from
  the merged sources (preferred)
- `ours` / `theirs`: deterministic selection (only for explicitly configured
  paths)

Minimal config shape:

```toml
[[merge.drivers]]
match = "Cargo.lock"
kind = "regenerate"
command = "cargo generate-lockfile"

[[merge.drivers]]
match = "src/gen/**"
kind = "regenerate"
command = "./scripts/gen.sh"
```

Drivers run during BUILD (to produce the candidate tree) and are therefore part
of the deterministic merge result. Any regeneration failures surface as
validation failures and follow the same on-failure policy.

---

## 7. Workspace Isolation Strategies

Each workspace needs an isolated working directory. Strategies ranked by
recommendation:

### 7.1 Git Worktrees (Recommended Default)

```bash
# Detached worktree against the epoch commit (no branch spam):
git worktree add --detach ws/alice <epoch-commit>

# If a ref is needed for Level 1 inspection:
# Use refs/manifold/ws/alice in a single namespace, prune aggressively
```

**Why detached by default:** Creating a branch per workspace
(`-b manifold/ws/alice`) causes branch explosion. With 20 agents creating and
destroying workspaces constantly, you get hundreds of stale branches. Detached
worktrees avoid this. If Level 1 Git compatibility is desired (humans can
`git log refs/manifold/ws/alice`), create refs on demand and prune on workspace
destroy.

**Atomic create/destroy:** Both `maw ws create` and `maw ws destroy` must be
atomic and idempotent:

- **Create:** Write a Manifold Create operation *before* calling `git worktree
  add`. If `git worktree add` fails, the Create operation is a no-op (workspace
  has no directory). On retry, detect existing Create op and resume.
- **Destroy:** Remove the worktree directory first, then update refs, then
  write Destroy operation. If any step fails, retry is safe (removal is
  idempotent, ref deletion is idempotent, Destroy op append is idempotent).

**Workspace identity:** Stable across directory deletion. The workspace exists
in the operation log even if its directory is gone. `maw ws list` shows
destroyed workspaces with their history. This is important for audit trails
and for `maw ws undo` on a recently-destroyed workspace.

### 7.2 Reflink Copy (Btrfs, XFS, APFS)

```bash
cp --reflink=auto -r .manifold/epochs/e-{hash}/ ws/alice/
```

Blocks shared on disk until modified. Effectively instant for any repo size.
No mount, no privileges, no namespace.

**When to prefer over git worktrees:** When repos are very large (>100k files)
and worktree checkout time becomes noticeable. For typical agent workloads
(<30k files), git worktrees are fast enough and simpler.

### 7.3 OverlayFS (Linux, Large Repos)

```bash
# Requires user namespace (kernel >= 5.11) or fuse-overlayfs (kernel >= 4.18).
# Lowerdir MUST be immutable epoch snapshot, NOT ws/default/.
unshare -rm sh -c '
  mount -t overlay overlay \
    -o lowerdir=.manifold/epochs/e-{hash},upperdir=.manifold/cow/alice/upper,workdir=.manifold/cow/alice/work \
    ws/alice
  exec "$SHELL"
'
```

**Practical constraints:** Requires a persistent user namespace (daemon process
or `maw exec` wrapper) to keep the mount alive across tool calls. Adds
complexity. Linux-only. Use only when disk savings justify the overhead.

### 7.4 Full Copy (Fallback)

```bash
cp -r .manifold/epochs/e-{hash}/ ws/alice/
```

Always works. For repos under 10k files, fast enough. Worst case for large
repos but provides the universal fallback.

### 7.5 Strategy Selection

```toml
# .manifold/config.toml
[workspace]
backend = "auto"  # auto | git-worktree | reflink | overlay | copy

# "auto" selection logic:
# 1. git-worktree (always available, default)
# 2. reflink (if filesystem supports it and repo > 30k files)
# 3. overlay (if Linux, kernel >= 5.11, and repo > 100k files)
# 4. copy (fallback)
```

---

## 8. Git Compatibility Levels

Three escalating levels, each a superset of the previous:

### Level 0: Git as Storage (Always)

- Epochs become git commits on `main`.
- Workspaces are outside git (gitignored).
- Manifold is required to manage workspaces.
- `git log`, `git blame`, `git bisect` work on mainline history.

### Level 1: Git as Interoperability (Recommended)

Everything in Level 0, plus:

- Every workspace can be materialized as a git ref
  (`refs/manifold/ws/<name>`) for inspection.
- Humans can `git diff refs/manifold/ws/alice..main` to see what an agent
  changed.
- Debugging is dramatically easier.
- IDE history, blame, bisect work on workspace state.

### Level 2: Git as Transport (Future)

Everything in Level 1, plus:

- Manifold state (op logs, workspace heads, epoch pointers) is pushable/pullable
  via `git push/fetch refs/manifold/*`.
- Enables multi-machine collaboration without bespoke servers.
- Requires that all Manifold metadata is stored as Git objects (satisfied by
  the git-native op log design in §5.3).

---

## 9. Mathematical Foundation

### 9.1 The Workspace Lattice

The set of all possible repository states forms a **join-semilattice** under the
merge operation.

Define:

- **S** = the set of all possible repository states (epoch + patch-sets +
  conflict data)
- **⊔** = merge operation (join)
- **⊥** = the empty repository (bottom element)

Properties:

- **S** is a partially ordered set where `a ≤ b` iff `a ⊔ b = b`
- Every finite subset of **S** has a least upper bound
- Merge is monotonic: if `a ≤ b`, then `a ⊔ c ≤ b ⊔ c` for all `c`

This guarantees: merging is always well-defined, monotonic, and convergent.
No matter what order you merge workspaces, you reach the same final state.

### 9.2 Patches as Morphisms

Adopting Pijul's insight (formalized by Mimram and Di Giusto):

Define a category **Repo** where:
- **Objects** are repository states (epoch tree + conflict data)
- **Morphisms** are patches (patch-sets between states)
- **Composition** is patch application
- **Identity** is the empty patch-set

The merge of two co-initial patches `p: A → B` and `q: A → C` is their
**pushout**: the minimal state `D` with patches `q': B → D` and `p': C → D`
such that `q' ∘ p = p' ∘ q`.

When the pushout doesn't exist (patches conflict), we extend to the **free
finite co-completion** — adding conflicted states as objects. A conflict
resolution is a morphism from the conflicted state to a clean state.

**Executable verification:** This is not just theory. For randomized test
histories:

1. Generate base + N patch-sets.
2. Compute the merge result M.
3. Verify: all sides embed into M (their edits are present or represented as
   conflicts).
4. Verify: no strictly-better M' exists with fewer conflicts while still
   embedding all sides (approximate check via sampling).

This turns the categorical property into a fuzzable CI test.

### 9.3 Delta-State CRDT for File Trees

The file tree is modeled as a delta-state CRDT using the patch-set model (§5.4).
Concurrent operations on disjoint paths commute trivially. Operations on the
same path are resolved by the merge engine (§6.1). Rename/move operations use
stable FileIds (§5.8) to avoid the path-identity confusion that plagues git.

For the tree *structure* (directories, renames, moves), Kleppmann's replicated
tree CRDT algorithm provides the formal foundation for handling concurrent moves
without introducing cycles. However: a tree CRDT is only a good fit for VCS if
files have stable identities — which is why FileId (§5.8) is a prerequisite,
not an optimization.

---

## 10. Design Decisions (Hard Questions, Answered)

| # | Question | Decision | Rationale |
|---|----------|----------|-----------|
| 1 | Workspace lifetime | Default ephemeral; opt-in persistent with explicit advance | Most agent beads are short-lived. Long tasks need advance semantics. |
| 2 | File identity | Stable FileId (128-bit random) | Deterministic renames. Path-only is a dead end. |
| 3 | Conflict commits in Git | Never. Conflicts are Manifold metadata only. | Keep `git log`/`bisect`/`blame` pristine. |
| 4 | Human inspection of agent work | Level 1 (workspace state as Git refs) | Debugging demands it. Cost is minimal. |
| 5 | Recovery contract after power loss | Epoch pointer either moved (merge complete) or didn't (merge abandoned, workspaces intact). Automatic. | The §5.10 state machine makes this deterministic. |
| 6 | Scaling target | 20 agents near-term. 200 design target. 2000 out of scope. | At 2000 you need distributed architecture. Different problem. |
| 7 | Undo semantics | Compensation ops in monotonic log | Lattice stays monotonic. Content reverts. History grows. |
| 8 | Agent interface | Directories + files + JSON. Zero VCS concepts. | The whole point. |
| 9 | Op log storage | Git blobs + refs (option A2) | Single CAS. Debuggable. Transportable. |
| 10 | Ordering | (epoch, workspace, seq) primary. Wall clock informational. | Simpler than HLC for single-machine. Same guarantees. |

---

## 11. Implementation Plan

### Phase 0: Ship the Pragmatic Fix (Now)

**Objective:** Stop the bleeding. Workers stop using jj.

- Ship bd-1h8c (jj-free workers in maw)
- Ship bd-1k4z (worker protocol in botbox: no jj commands)

**Effort:** Already beaded, 1–2 days.

### Phase 1: Git Worktree Backend (2–3 weeks)

**Objective:** Replace jj with git worktrees. Eliminate the opfork problem.

- Implement `WorkspaceBackend` trait in maw
- Implement `GitWorktreeBackend`: create (detached), destroy (atomic/idempotent),
  list, status, merge using `git worktree` commands
- Implement crash-safe merge state machine (§5.10), including VALIDATE gating
  and quarantine materialization (§5.12) — this is Phase 1, not Phase 3.
  Crash safety and "keep mainline green" are day-one requirements.
- Implement merge preview (`--plan --json`) producing a deterministic merge plan
  artifact (§5.12)
- Implement minimal derived artifacts for agent UX:
  - per-workspace change report (`.manifold/artifacts/ws/<id>/report.json`)
  - per-merge plan + validation artifacts (`.manifold/artifacts/merge/<id>/*`)
- Implement deterministic merge drivers (minimal allowlist: lockfiles + known
  generated dirs) (§6.4)
- New repos use git worktrees by default
- Old repos keep jj (backward compatibility)
- Delete sync.rs, simplify merge.rs, remove opfork detection

**Guardrails:**
1. Detached worktrees by default (no branch spam)
2. Atomic + idempotent create/destroy
3. Crash-safe merge with persisted state machine
4. Merge preview is side-effect free (no ref updates)
5. Default validation policy blocks epoch advancement and produces quarantine
   (configurable)

**Effort:** 3–5 beads. The maw CLI API doesn't change.

**Result:** ~2,000 lines of jj recovery code removed. Zero opfork incidents.

### Phase 2: Patch-Set Model + Git-Native Op Log (4–6 weeks)

**Objective:** Add the Manifold data model on top of git worktrees.

- Implement PatchSet and PatchValue types
- Implement FileId (§5.8) — data model + sidecar file
- Implement git-native operation log (operations as git blobs, heads as refs)
- Implement per-workspace view materialization from log replay
- Implement view checkpoints + log compaction
- Implement OrderingKey (§5.9) with wall-clock clamp guard
- Add `maw ws undo` — compensation operations (§5.11)
- Add `maw ws history` — rich per-workspace operation history

**Critical:** Do not build a bespoke op-store. Git blobs + refs from day one.
This shapes GC, replication, durability, and debugging tooling for every
subsequent phase.

**Effort:** 5–8 beads.

**Result:** Operation-level undo. Rich history. FileId for deterministic renames.
Foundation for advanced merge.

### Phase 3: Advanced Merge + Conflict Model (6–8 weeks)

**Objective:** Implement deterministic N-way merge with structured conflicts.

- Implement Conflict with ConflictAtoms (§5.7)
- Implement the patch-set-based N-way merge (§6.1) — reduce by path, not
  pairwise diff
- Implement merge pipeline layering (§6.2): hash equality → diff3 →
  shifted-code alignment → AST merge → agent resolution
- Expand post-merge validation hooks (§6.3): multi-command pipelines, per-language presets, richer diagnostics
- AST-aware merge for Rust, Python, TypeScript via tree-sitter
- Agent-friendly conflict presentation (JSON structured output)

**Build around "patch-set + constraints":** Do not bolt AST-merge onto a
tree-based pipeline as an afterthought. The merge engine operates on patch
atoms, constraints over edit regions, and minimal conflict cores from the
start.

**Effort:** 5–8 beads.

### Phase 4: CoW Workspace Layer (4–6 weeks)

**Objective:** Replace full directory copies with CoW for instant workspace
creation on large repos.

**Prerequisite:** Stable epoch semantics with crash-safe management (Phases 1–2).
Overlay/reflink work is wasted if epochs aren't nailed down.

- Detect platform capabilities (reflink, overlayfs+userns, fallback)
- Implement reflink workspace backend (Btrfs/XFS/APFS)
- Implement OverlayFS workspace backend (Linux, immutable epoch lowerdir)
- Implement strategy auto-selection (§7.5)
- Benchmark: workspace creation time, disk usage, merge performance

**Effort:** 3–5 beads.

### Phase 5: The Full Manifold (Ongoing)

- Epoch garbage collection
- Conflict prediction as scheduling data (expose touched-set for orchestrator)
- `refs/manifold/*` push/pull for Level 2 Git compatibility
- Property-testing merge correctness via pushout contracts (§9.2)
- Workspace templates for specific bead types
- Tree-sitter semantic conflict detection expansion

---

## 12. What's Not In Scope

- **Bloom clocks.** Probabilistic causality structures add complexity with
  minimal win for single-machine operation. Revisit only if Manifold logs are
  pushed across many ephemeral nodes.
- **Full category-theoretic patch algebra.** The categorical framework informs
  the design and provides testable properties (pushout verification), but we
  do not implement a general-purpose patch category engine.
- **Distributed consensus.** 2000+ agents across machines is a fundamentally
  different architecture. Manifold targets single-machine, single-writer-per-
  workspace, serial-merge.
- **Custom filesystem.** We use existing filesystems (ext4, APFS, etc.) and
  their CoW capabilities. We do not build a FUSE filesystem.

---

## 13. References

### Version Control Systems
- jj (Jujutsu): https://github.com/jj-vcs/jj
- jj concurrency docs: https://docs.jj-vcs.dev/latest/technical/concurrency/
- jj operation log: https://docs.jj-vcs.dev/latest/operation-log/
- git worktree: https://git-scm.com/docs/git-worktree
- Pijul: https://pijul.org/
- Pijul theory: https://pijul.org/manual/theory.html

### CRDT Theory
- Shapiro et al., "A comprehensive study of CRDTs" (2011)
- Sanjuán et al., "Merkle-CRDTs: Merkle-DAGs meet CRDTs" (Protocol Labs / IPFS lineage)
- Kleppmann, "A highly-available move operation for replicated trees" (2021)
- Almeida, Baquero, Fonte, "Interval Tree Clocks" (2008)
- Baquero, Preguiça, "Why Logical Clocks are Easy" (ACM Queue, 2016)

### Merge Algorithms
- Zhu et al., "Mastery: Shifted-Code-Aware Structured Merge" (JSA, 2023) — reports 82.26% accuracy on extracted Java merge scenarios
- Mimram, Di Giusto, "A Categorical Theory of Patches" (2013)

### Concurrency Primitives
- Lamport, "Time, Clocks, and the Ordering of Events in a Distributed System" (1978)
- Kulkarni et al., "Logical Physical Clocks and Consistent Snapshots" (HLC, 2014)

### maw Project
- maw-2026-02-16.md — state of the project
- workspace-backend-options.md — backend comparison
- opfork.md — jj opfork technical analysis (in chief repo)
