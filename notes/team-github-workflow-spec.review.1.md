# Review: Team GitHub Workflow Spec

Reviewer: maw-dev
Date: 2026-03-05

Overall: This is a well-structured spec that correctly identifies the gap between maw's single-integrator model and team PR workflows. The session model, resolution rules, and phased rollout are all sound. Below are concrete issues and improvements, ordered by severity.

---

## 1. "task" vs "session" naming confusion (HIGH)

The CLI surface says `maw task` but the internal model calls everything a "session". This creates a conceptual collision: users already have "tasks" in Asana/Linear/Jira. When someone says "my task" do they mean the external tracker item or the maw session?

**Recommendation:** Rename the command group to `maw session` (matching the internal model), and keep the *task reference* as just a field on a session. This eliminates ambiguity:

```bash
maw session start --task ASANA-123 --branch feat/asana-123-cache
maw session list
maw session use asana-123
maw session finish
```

If `maw task` is strongly preferred for brevity, at minimum the spec should explicitly acknowledge the naming collision and define which "task" means what in each context.

## 2. `index.toml` duplicates session state (HIGH)

The `[open]` table in `index.toml` duplicates the `state` field from individual session files:

```toml
# index.toml
[open]
asana-123 = "open"    # duplicated from sessions/asana-123.toml state field

# sessions/asana-123.toml
state = "open"        # source of truth
```

This creates an invariant that must be maintained across two files. Any crash between writing the session file and updating the index leaves them inconsistent.

**Recommendation:** Remove the `[open]` table. Derive open sessions by scanning `sessions/*.toml` (there will be few files; scanning is cheap). Keep only `current_session` and `by_branch` in the index, since those are cross-session concerns that can't live in individual files.

## 3. Epoch interaction is underspecified (HIGH)

The spec says `task start` will "sync epoch to head branch" (step 5), but doesn't define what this means when:

- Multiple sessions exist on different branches, each diverging from different epochs
- `maw epoch sync` is run while sessions are open
- A session's base branch moves forward (new epoch) while work is in progress

**Recommendation:** Add a section "Epoch / Session Interaction" that defines:

- Whether each session records its starting epoch OID (the schema has `started_from_epoch` which is good, but no spec text explains its purpose)
- What `maw epoch sync` does when sessions are open (does it sync all? only current? error?)
- Whether `task sync` also updates `started_from_epoch`

## 4. `task use` with dirty workspace (MEDIUM)

`task use` switches `ws/default` to a different branch. If the workspace has uncommitted changes (modified files in the worktree), a branch switch will either fail or lose work.

**Recommendation:** Specify behavior explicitly:

- Option A: Refuse to switch if workspace is dirty (safest)
- Option B: Stash changes automatically and note the stash in session metadata
- Option C: Use `maw ws merge` to commit first, then switch

The spec should pick one and document it.

## 5. `task start` with pre-existing branches (MEDIUM)

What happens when:

- The branch `feat/asana-123-cache` already exists locally (from a previous attempt)?
- The branch exists on remote but not locally?
- The branch was previously used by a different (now archived) session?

**Recommendation:** Add explicit behavior for each case:

- Local branch exists: switch to it (or fail with `--resume` hint)
- Remote branch exists, no local: track it
- Branch in archived session: allow reuse (archive is historical, not a reservation)

## 6. Force-push implications of `task sync --rebase` (MEDIUM)

Rebasing a session branch that has been pushed (especially one with an open PR) rewrites history and requires force-push. The spec doesn't mention this.

**Recommendation:**

- `task sync --rebase` should warn if the branch has been pushed
- Should auto-force-push after rebase, or require explicit `maw push --force`
- Document that rebase rewrites PR history (reviewers lose inline comments on rebased commits)
- Consider defaulting to merge (safer for pushed branches) and requiring `--rebase` as opt-in, which resolves open question #2

## 7. `maw pr open` idempotency needs implementation detail (MEDIUM)

Integration test #6 says `pr open` is idempotent, but `gh pr create` fails if a PR already exists for the same head/base combination.

**Recommendation:** Specify the idempotency strategy:

1. Check if PR already exists (`gh pr list --head <branch>`)
2. If yes, update session metadata with existing PR info and skip creation
3. If no, create new PR
4. If PR exists but is closed, either reopen or create new (specify which)

## 8. Locking strategy is vague (MEDIUM)

"Writes guarded with `.manifold/session/.lock`" and "atomic write (temp file + rename)" are mentioned but:

- What kind of lock? Advisory `flock`? PID-file? fcntl?
- What's the timeout/retry behavior?
- maw already has locking patterns for oplog — should this reuse the same mechanism?

**Recommendation:** Specify that session locking reuses maw's existing lock infrastructure (if it exists), or define the concrete mechanism. For multi-agent scenarios, this matters.

## 9. Workspace-session binding edge cases (LOW)

The spec says `maw ws create` stamps `session_id` when a session is active. But:

- What if a user creates a workspace with no session, then later starts a session on the same branch? The workspace has no `session_id`.
- What if a user explicitly wants a workspace outside any session (utility workspace)?

**Recommendation:**

- Unstamped workspaces should be treated as "sessionless" and allowed to merge into any branch (backward compat)
- Add `--no-session` flag to `maw ws create` to explicitly opt out of stamping when a session is active
- The cross-session guard only fires for stamped workspaces, not unstamped ones

## 10. `task abort` should offer branch cleanup (LOW)

The spec says "without deleting branch by default" but doesn't offer a flag to delete.

**Recommendation:** Add `--delete-branch` flag:

```bash
maw task abort asana-123 --delete-branch  # also deletes local+remote branch
```

## 11. `maw pr update` should support draft demotion (LOW)

The spec shows `--ready` for draft-to-ready promotion but not the reverse.

**Recommendation:** Add `--draft` flag for ready-to-draft demotion (supported by `gh pr ready --undo`):

```bash
maw pr update --session asana-123 --draft   # demote to draft
maw pr update --session asana-123 --ready   # promote to ready
```

## 12. Missing: session-aware `maw status` output (LOW)

The "Session visibility" section mentions showing session info in `maw status`, but doesn't specify the format.

**Recommendation:** Add a concrete example of what `maw status` output looks like with sessions:

```text
repo: maw (bare, v2)
session: asana-123 (open, PR #842 draft)
  base: main  head: feat/asana-123-merge-diagnostics
  2 open sessions total
workspace: default (on feat/asana-123-merge-diagnostics)
  workspaces: a123-agent-a, a123-agent-b
```

## 13. Storage location clarification (LOW)

`.manifold/session/` — is this relative to the repo root (`.git/../.manifold/`) or inside the default workspace? Given maw's bare repo layout, this should be explicit.

**Recommendation:** State explicitly: `.manifold/session/` lives at repo root (sibling to `.git/` and `ws/`), not inside any workspace.

---

## Answers to Open Questions

> 1. Should `task start` require explicit `--session` for deterministic naming?

**Yes**, for agent use. Auto-generated IDs create indirection that agents must then resolve. Make `--session` required or derive it deterministically from the task ref (e.g., `ASANA-123` -> `asana-123` via lowercasing + dash normalization). Document the derivation rule.

> 2. Should `task sync` default to `rebase` or `merge`?

**Merge.** Rebase rewrites history and forces force-pushes, which is destructive on shared branches. Make merge the default; rebase is opt-in with `--rebase`.

> 3. Should `maw push` refuse `main` pushes when any open session exists?

**No.** This is too restrictive. Users may have legitimate reasons to push to main (hotfixes, non-session work). Instead, add a **warning** (not an error) when pushing to main with open sessions, suppressible via `--yes` or config.

> 4. Should provider-specific helpers (`maw task claim asana ...`) be added later?

**Yes, much later (if ever).** The current `--provider` + `--url` fields on `task start` are sufficient. Provider-specific API integration is a large surface area with high maintenance cost. Let users claim tasks in their tracker's native UI and just reference them in maw.

---

## Summary of Recommended Changes

| # | Severity | Change |
|---|----------|--------|
| 1 | HIGH | Rename `maw task` to `maw session` (or explicitly address naming collision) |
| 2 | HIGH | Remove `[open]` table from `index.toml`; derive from session files |
| 3 | HIGH | Add "Epoch / Session Interaction" section |
| 4 | MEDIUM | Specify `task use` behavior with dirty workspace |
| 5 | MEDIUM | Specify `task start` behavior with pre-existing branches |
| 6 | MEDIUM | Address force-push implications of `task sync --rebase` |
| 7 | MEDIUM | Specify `pr open` idempotency strategy |
| 8 | MEDIUM | Specify concrete locking mechanism |
| 9 | LOW | Define unstamped workspace behavior and `--no-session` flag |
| 10 | LOW | Add `--delete-branch` to `task abort` |
| 11 | LOW | Add `--draft` to `maw pr update` |
| 12 | LOW | Add concrete `maw status` output example with sessions |
| 13 | LOW | Clarify `.manifold/session/` location relative to repo root |
