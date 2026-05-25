# SG2 Worktrees+Convention Substrate — Hand-Rolled Coordination

**Status:** Frozen with `bn-mit2` (T2.3 adapters). Consumed verbatim by the
`git-worktrees-bare` arm of the SG2 benchmark (`notes/sg2-benchmark-preregistration.md`
§1.3 arm 2; §8.1 worktrees crib).

This document IS the substrate of the worktrees arm. The arm's agent
crib (per §8.1) gives agents these rules as the coordination contract
they should follow. The `WorktreesConventionAdapter` encodes EXACTLY
these rules — nothing more — so the SG2 measurement reflects what plain
git worktrees + a thin written convention give an agent fleet, not a
crypto-maw running under a git skin.

The convention is deliberately small: maw's whole reason for existing is
that this convention is **not enough** for agent coordination. SG2 must
expose that gap honestly — not patch the worktrees arm to be secretly
maw-equivalent.

---

## 1. Layout

```
<root>/repo.git/          ← bare repository (the canonical object store)
<root>/main/              ← integration worktree (the merge target)
<root>/<ws>/              ← per-workspace worktrees (one per agent)
<root>/.coord/            ← convention directory (advisory only)
<root>/.coord/<ws>.claim  ← claim file per active workspace
<root>/.coord/destroyed/  ← post-destroy archives (commit tip + name)
```

The `.coord/` directory is the **only** state the convention introduces.
It is plain-text, agent-readable, and the adapter does not enforce it —
matching what a human team using bare git worktrees would build on their
first afternoon.

---

## 2. Lifecycle rules (binding, agent-readable)

### 2.1 Create

The integration branch is `main` on `<root>/repo.git`. To start work on
task `<id>`:

```
git -C <root>/main worktree add -b <id> <abs-path-to-<root>/<id>> main
git -C <root>/<id> config user.name  <agent-name>
git -C <root>/<id> config user.email <agent-email>
echo "workspace = <id>\nbranch = <id>" > <root>/.coord/<id>.claim
```

Workspace **branch name equals the workspace id** (the convention's only
naming rule). The claim file is advisory: the convention asks agents to
check it before starting work on the same id, but nothing enforces it.

### 2.2 Edit + commit

Plain git inside `<root>/<id>`. The convention says nothing extra — this
is the "agents are git-fluent" assumption.

```
# inside <root>/<id>:
git add -A
git commit -m "<message>"
```

### 2.3 Sync to current `main`

When the integration branch has advanced (someone else's merge landed),
rebase the workspace branch:

```
git -C <root>/<id> rebase main
```

If rebase conflicts, the convention's only recovery rule is **abort and
resolve manually**: `git rebase --abort`, then re-apply edits on top of
the new `main`. The adapter mirrors this: a conflicted rebase aborts and
returns `conflicted=true`. There is no jj-style first-class-conflict
commit recording, no maw-style sidecar — the conflict surface IS the
working-tree CONFLICT markers.

### 2.4 Merge (integration step)

The integration step is an octopus merge into `main`:

```
git -C <root>/main checkout main
git -C <root>/main merge --no-ff -m "merge: <ids>" <id1> <id2> ...
```

If `git merge` reports `CONFLICT`, the convention says **abort the merge**
(do NOT leave the integration branch in a half-merged state):

```
git -C <root>/main merge --abort
```

A conflicted octopus merge counts as a benchmark `wedge_incident` per
pre-reg §1.1: `wedge_incident = divergent-state recovery OR abandoned
committed work OR turns_to_done > 1.5× benign-median`. The convention
does NOT auto-resolve — the agent must `merge --abort`, rebase sources
sequentially, and retry.

### 2.5 Destroy

```
git -C <root>/main worktree remove [--force] <id>
git -C <root>/main rev-parse <id> > <root>/.coord/destroyed/<id>
git -C <root>/main branch -D <id>
rm <root>/.coord/<id>.claim
```

The `destroyed/<id>` file holds the branch tip's commit-id at the moment
of destroy. This is the convention's **entire** recovery surface — the
git reflog plus this commit-id file. No snapshot ref, no quarantine, no
recovery sidecar. If an agent destroys a workspace that contains
unmerged work, the reflog is the only path back, and the next
`git gc` may reclaim the orphan commits. This is exactly the asymmetry
SG2 is built to measure: the cost of NOT having maw's recovery layer.

---

## 3. What the convention explicitly does NOT provide (and why)

The following are deliberate non-features. Adding any of them would make
the worktrees arm a maw-clone and bias the SG2 measurement.

| absent feature                | substitute the agent must use                          |
| ----------------------------- | ------------------------------------------------------ |
| epoch counter / stale flag    | agent must `git fetch` + manually compare HEADs        |
| coordination lock / claim CAS | claim file is advisory; agents may race                |
| structured conflict surface   | git's working-tree CONFLICT markers; agent resolves    |
| destroy snapshot / recovery   | git reflog only; no `maw ws recover` equivalent        |
| `maw status --json`           | `git worktree list --porcelain` + ad-hoc shell        |
| pre-merge conflict check      | none; `git merge` is the only conflict probe          |
| mergeback queue               | agents merge in human-arrival order                    |

---

## 4. Why a "thin convention" rather than raw git worktrees?

Per pre-reg §1.3 arm 2 and `notes/manifold-v2.md` §3: raw worktrees with
zero convention is unfair to the worktrees arm — every shop using bare
git worktrees develops at least a directory layout and a "where do I
push to merge?" rule on day one. The convention encoded here is the
**minimum honest baseline**: a layout, a naming rule, a claim-file
advisory, and a documented integration step. Anything thinner straws-
mans worktrees; anything thicker straw-mans maw.

The convention is committed (this file) BEFORE the first measured SG2
run so the worktrees arm is reviewable and reproducible. It will appear
verbatim in the SG2 publication appendix (per pre-reg §8.1: "All four
cribs are equalized in length and detail … published as appendices to
the report").

---

## 5. Adapter implementation

Encoded by `crates/maw-bench-adapters/src/worktrees_adapter.rs` as
`WorktreesConventionAdapter`. Every method in that file maps 1:1 to a
section here. Any new step added to the adapter that does not have a
corresponding section here is a parity bug; fix it by either removing
the step from the adapter or extending this convention with a written
justification (and re-citing it in `notes/sg2-adapter-parity.md`).
