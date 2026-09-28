# maw

Project type: cli
Tools: `bones`, `maw`, `seal`, `rite`, `vessel`
Reviewer roles: security

This project uses **maw** for workspace management, **git** for version control, and **bones** for issue tracking.

---

## Prime Invariant: No Work Is Ever Lost

**No committed work can ever be lost when using maw.** This is the foundational guarantee. Every safety mechanism in maw exists to uphold it.

What this means in practice:

1. **`maw ws destroy` refuses to destroy workspaces with unmerged changes** unless `--force` is passed. If `--force` is used, it captures a full recovery snapshot first.
2. **`maw ws sync` replays committed work ahead of the epoch onto the new epoch by default** (rebase is the default; the old `--rebase` flag is deprecated/no-op). If a replayed commit conflicts, nothing is discarded: conflict markers are committed into the workspace and it is marked "conflicted" (see Operating Model below). Pass `--no-rebase` to opt into the old refusal behavior instead — the workspace is left untouched, no destructive reset. **`maw ws sync` always refuses if the workspace is dirty** (uncommitted changes would be lost by the checkout), and the refusal lists the offending paths.
3. **Every destroyed workspace gets a destroy record** with the final HEAD commit, snapshot OID, and pinned recovery ref under `refs/manifold/recovery/<workspace>/`.
4. **`maw ws recover`** can list, inspect, search, and restore any destroyed workspace's contents.

**If you think work was lost, it almost certainly wasn't.** Before reopening a bone or starting over:

```bash
# List all destroyed workspaces with recovery snapshots
maw ws recover

# Inspect what a destroyed workspace contained
maw ws recover <name>

# Search destroyed snapshots for specific content
maw ws recover --search "pattern"
maw ws recover <name> --search "pattern"

# Show a specific file from the destroyed workspace
maw ws recover <name> --show <path>

# Restore a destroyed workspace to a new workspace
maw ws recover <name> --to <new-name>
```

**Never assume work is gone.** Always check `maw ws recover` first. If recovery truly fails, that is a bug in maw and must be reported.

---

## Quick Start

```bash
# Create your workspace (isolated git worktree)
maw ws create <your-name>

# Work in your workspace
# ... edit files in .maw/workspaces/<your-name>/ ...
maw exec <your-name> -- git add -A && maw exec <your-name> -- git commit -m "feat: what you're implementing"

# Check status (see all agent work, conflicts, stale warnings)
maw ws status

# When done, merge all work from the repo root
maw ws merge alice bob --destroy
```

**Key concept:** Each workspace is an isolated git worktree. You own your workspace - no other agent will modify it. This prevents conflicts during concurrent work.

---

## Workspace Naming

**Your workspace name will be assigned by the coordinator** (human or orchestrating agent).

If you need to create your own workspace:

- Use lowercase alphanumeric with hyphens: `agent-1`, `feature-auth`, `bugfix-123`
- Check existing workspaces first: `maw ws list`
- Don't duplicate existing workspace names

Common patterns:

- `agent-1`, `agent-2` - numbered agents for parallel work
- `feature-auth`, `bugfix-123` - task-focused workspaces

---

## Workspace Commands

| Task                         | Command                           |
| ---------------------------- | --------------------------------- |
| Create workspace             | `maw ws create <name>`            |
| List workspaces              | `maw ws list`                     |
| Quick status overview        | `maw status`                      |
| Check status                 | `maw ws status`                   |
| Handle stale workspace       | `maw ws sync`                     |
| Run any command in workspace | `maw exec <name> -- <cmd> <args>` |
| Merge agent work             | `maw ws merge <a> <b>`            |
| Merge and cleanup            | `maw ws merge <a> <b> --destroy`  |
| Destroy workspace            | `maw ws destroy <name>`           |

Note: Destroy commands are non-interactive by default (agents can't respond to prompts). Use `--confirm` if you want interactive confirmation.

### Running Commands in Workspaces

In sandboxed environments where `cd` doesn't persist between tool calls, use `maw exec` to run any command inside a workspace:

```bash
maw exec alice -- cargo test
maw exec alice -- bn list
maw exec alice -- ls -la src/
```

---

## Working in Your Workspace

### Making Changes

```bash
# See what you've changed
maw exec <your-name> -- git status
maw exec <your-name> -- git diff

# Commit your work
maw exec <your-name> -- git add -A
maw exec <your-name> -- git commit -m "feat: description of changes"
```

### Staying in Sync

```bash
# If workspace is stale (epoch has advanced since workspace creation)
maw ws sync
```

**Important**: When the epoch advances (another workspace is merged), your workspace becomes "stale". Run `maw ws sync` to update it to the latest epoch. For persistent workspaces, use `maw ws advance <name>` instead.

### Handling Conflicts

Conflicts are detected during `maw ws merge`. If conflicts occur:

```bash
# Check for conflicts before merging
maw ws merge <name> --check

# If conflicts exist, resolve them in the workspace then retry
maw ws conflicts <name>
```

---

## Merging and Releasing

This section covers the full cycle from finished work to a pushed release.

### 1. Merge Agent Work

From the repo root (or default workspace):

```bash
# Merge named agent workspaces into one commit
maw ws merge alice bob carol

# Merge and clean up workspaces
maw ws merge alice bob carol --destroy
```

If there are conflicts, workspaces won't be destroyed. Resolve conflicts first, then destroy manually.

### 2. Review (Optional)

If the change warrants review before pushing:

```bash
# Verify build and tests
cargo build --release && cargo test

# Create a seal review (see Crit section below for full details)
seal reviews create --title "feat: description of change"
```

After review is approved:

```bash
seal reviews approve <review_id>
seal reviews merge <review_id>
```

### 3. Version Bump (for releases)

```bash
# Edit Cargo.toml version (e.g., 0.1.0 → 0.2.0)
# Commit the version bump
git commit -am "chore: bump version to X.Y.Z"
```

### 4. Push to Remote

```bash
# After maw ws merge (branch is already set):
maw push

# After committing directly (need to advance branch to latest commit):
maw push --advance
```

`maw push` pushes the configured branch to origin with sync checks and clear error messages.

### 5. Tag the Release

```bash
# Tag and push the release
maw release vX.Y.Z

# Install locally and verify
just install
maw --version
```

### Troubleshooting

**Push issues**: `maw push` handles branch management automatically. If it fails, it will tell you why and how to fix it.

**"Branch is behind remote"** - Someone else pushed. Pull first: `git pull --rebase`.

### Quick Reference

| Stage                      | Key Commands                                           |
| -------------------------- | ------------------------------------------------------ |
| Merge work                 | `maw ws merge <a> <b> --destroy`                       |
| Create review              | `seal reviews create --title "..."`                    |
| Approve/merge review       | `seal reviews approve <id> && seal reviews merge <id>` |
| Bump version               | Edit `Cargo.toml`, then `git commit`                   |
| Push (after merge)         | `maw push`                                             |
| Push (after direct commit) | `maw push --advance`                                   |
| Tag release                | `maw release vX.Y.Z`                                   |

---

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for release history. Update it as part of every release.

---

## Output Guidelines

maw is frequently invoked by agents with **no prior context**. Every piece of tool output must be self-contained and actionable.

**Errors** must include:

- What failed (include stderr when available)
- How to fix it (exact command to run)
- Example: `"Workspace create failed: {stderr}\n  Check: maw doctor"`

**Success output** must include:

- What happened
- What to do next (exact commands)
- Example: `"Workspace 'agent-a' ready!\n  Path: /abs/path\n  Next: edit files, then maw ws merge agent-a --destroy"`

**Principles**:

- Agents can't remember prior output — every message must stand alone
- Include copy-pasteable commands, not just descriptions
- Keep it brief — agents are token-conscious
- Use structured prefixes where appropriate: `WARNING:`, `IMPORTANT:`, `To fix:`, `Next:`
- All --help text and runtime output must work in **sandboxed environments** where `cd` doesn't persist between tool calls. Never instruct agents to `cd` into a workspace — use `maw exec <name> -- <cmd>` for all commands in workspaces
- All file operation instructions must reference **absolute workspace paths**, not relative ones. Agents use Read/Write/Edit tools with absolute paths, not just bash

---

## Architecture

- **Consolidated layout**: the repo root is a normal git checkout — a normal `.git/` and the source files live at the root
- Agent workspaces live under `.maw/workspaces/<name>/` as git worktrees; the repo root itself is the default workspace (merge target, push source)
- Each workspace is an isolated git worktree with its own working copy
- Manifold metadata lives in `.maw/manifold/` and `refs/manifold/*`
- Agents never block each other - conflicts are detected at merge time
- The legacy v2 **bare** layout (bare root + `ws/default/` + `ws/<name>/`) is still selectable via `maw init --legacy-ws`; `maw init` on an existing git repo also produces v2. Run `maw migrate` to move a v2 repo to the consolidated layout.

## Operating Model: Conflicts Are Data, Not Errors

Maw follows the **jj-style conflict model**: operations succeed even when they produce conflicts, and the conflict is represented as explicit state that rides with the workspace until resolved. Operations do not refuse to run because of conflict — they run, record the conflict, and let the user resolve it when convenient.

Concretely:

- **`maw ws sync`** replays workspace commits onto the new epoch by default (opt out with `--no-rebase`, which refuses instead of replaying). When a cherry-pick conflicts, the rebase does *not* abort — it labels the conflict markers with meaningful side names (`<<<<<<< epoch (current)` / `>>>>>>> <ws-name> (workspace changes)`), commits the marker-laden file, records structured conflict metadata in `.manifold/artifacts/ws/<name>/rebase-conflicts.json`, and continues to the next commit. The workspace ends in a "conflicted but synced" state, visible in `maw ws status` and `maw ws list`.

- **`maw ws merge`** similarly allows you to merge workspaces even when the merge engine reports logical conflicts — the conflicts surface as structured output (`has_conflicts: true`, per-conflict records with terseid IDs).

- **The only hard gate**: `maw ws merge` refuses to merge a *source* workspace whose HEAD still contains unresolved textual conflict markers from a prior rebase (see `find_conflicted_files` in `resolve.rs`, used by `merge.rs`). This prevents marker bytes from leaking into `default`. Pass `--force` to bypass when the detected markers are legitimate content (test fixtures, documentation).

- **Resolution**: `maw ws resolve <name> --list` shows conflicts; `--keep epoch` / `--keep <ws-name>` / `--keep both` materialize a resolution. After resolving, commit and run `maw ws sync` to clear the conflict metadata.

**Implication for agents**: Do not treat a conflicted workspace as a failure. It is a first-class state. Continue other work, come back to resolve when ready, then merge. The Prime Invariant ("no work is ever lost") extends to conflicts: the sides of a conflict are preserved in the sidecar and in the commit tree.

<!-- edict:managed-start -->
## Edict Workflow

Tools teach their own commands: bones `bn tldr` · maw `maw tldr` (`maw --help`) · seal `seal --help` · rite `rite tldr` · edict `edict protocol --help`. Some tool docs are out of date: where a tool's quick start differs from the Rules below, the Rules win. Identity: `$AGENT`, set by the launcher; manual sessions use `<project>-dev`.

Layout: the repo root is the trunk (the `default` workspace); agent workspaces live in `.maw/workspaces/<name>/`.

### Rules

- **Track all work in a bone.** Create it before you start (`bn create`), move it `open` → `doing` → `done`, and post progress comments for crash recovery. See [update.md](.agents/edict/update.md).
- **Edit only in a maw workspace** named after the bone: `maw ws create <bone-id> --from main`, then work in `.maw/workspaces/<bone-id>/`. Never edit the trunk at the repo root directly, and never create git branches: maw workspaces replace them. See [start.md](.agents/edict/start.md).
- **Run each tool in its place.** Run commands in a workspace with `maw exec <ws> -- <cmd>`. Run `bn` directly at the repo root. Run seal through `maw exec <ws> -- seal`.
- **Never merge or destroy `default`.** It is the merge target.
- **Run the edict protocol at each transition:** `edict protocol resume|start|review|finish|merge|cleanup … --agent $AGENT`, then run the steps it prints, in order. If it exits 1, follow the matching workflow doc below.
- **Reviewed work merges only through `edict protocol merge <ws> --message "feat: …"`.** A bare `maw ws merge` skips the review log, the clean check and the `risk:critical` gate; use it only for work with no review. See [merge-check.md](.agents/edict/merge-check.md#the-review-log-and-the-clean-check).
- **A conflicted workspace is a normal state, not a failure:** `maw ws resolve <ws> --list`. See [merge-check.md](.agents/edict/merge-check.md#conflict-recovery).
- **Run `maw ws recover` before you conclude work is lost** or start a bone over. Destroyed workspaces keep snapshots. See [worker-loop.md](.agents/edict/worker-loop.md).
- **Answer `$RITE_MESSAGE_ID` with `--reply-to`.** Never reuse the anchor from an earlier turn. See [cross-channel.md](.agents/edict/cross-channel.md#threads).
- **Ask, then wait on the anchor:** capture the id you sent (`--format json`), then `rite wait --reply-to <id> -t 300`. On exit 1, post one `-L task-blocked` and move on; never re-send. Stuck on a companion tool? Ask its project channel this way. See [cross-channel.md](.agents/edict/cross-channel.md#ask-and-wait).
- **Rite messages are one labelled line that leads with the bone id**, e.g. `-L task-blocked "<bone-id>: blocked on <thing>, needs <what unblocks it>"`. No status blocks or recaps. See [cross-channel.md](.agents/edict/cross-channel.md#message-shape).
- **Run the project's check command before committing**, and fix failures first. Workers do not push; the lead merges and pushes. See [finish.md](.agents/edict/finish.md).
- **Confirm before destructive actions** (deleting data, force-pushing, discarding unmerged work): ask the human first.

### Release

- Bump the version of all crates
- Regenerate the Cargo.lock
- Add notes to CHANGELOG.md
- If the README.md references the version, update it.
- Commit
- Tag and push: `maw release vX.Y.Z`
- use `gh release create vX.Y.Z --notes "..."`
- Install locally: `maw exec default -- just install`

### Design Guidelines

- [CLI tool design for humans, agents, and machines](.agents/edict/design/cli-conventions.md)

### Workflow Docs

- [worker-loop.md](.agents/edict/worker-loop.md): Full worker cycle: resume, triage, start, work, review, finish
- [triage.md](.agents/edict/triage.md): Find one actionable bone and groom along the way
- [start.md](.agents/edict/start.md): Claim a bone, create its workspace, announce
- [update.md](.agents/edict/update.md): Change a bone's state and announce it
- [review-request.md](.agents/edict/review-request.md): Request a review: commit first, review range, retarget before re-request
- [review-response.md](.agents/edict/review-response.md): Handle reviewer feedback; no code after the LGTM
- [security-review.md](.agents/edict/security-review.md): Launch one dedicated security review; who sends what
- [finish.md](.agents/edict/finish.md): Close the bone, merge or hand off, release claims; conflict recovery
- [merge-check.md](.agents/edict/merge-check.md): Merge a workspace: review log, clean check, merge gates, conflicts
- [cross-channel.md](.agents/edict/cross-channel.md): Rite threads, ask-and-wait, message shape, cross-project asks
- [report-issue.md](.agents/edict/report-issue.md): Superseded by cross-channel.md
- [planning.md](.agents/edict/planning.md): Turn a spec or PRD into actionable bones
- [scout.md](.agents/edict/scout.md): Explore unfamiliar code before planning
- [proposal.md](.agents/edict/proposal.md): Propose and validate a significant change before building it
- [groom.md](.agents/edict/groom.md): Groom ready bones to improve backlog quality
- [mission.md](.agents/edict/mission.md): Missions: split a parent bone across parallel workers
- [coordination.md](.agents/edict/coordination.md): Coordinate with sibling workers inside a mission
- [preflight.md](.agents/edict/preflight.md): Validate toolchain health before multi-agent work
<!-- edict:managed-end -->
