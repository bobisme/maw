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
2. **`maw ws sync` refuses to sync workspaces with committed work** ahead of the epoch. It also refuses if the workspace is dirty.
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
# ... edit files in ws/<your-name>/ ...
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

- **Bare repo model**: Workspaces live in `ws/<name>/` as git worktrees
- `ws/default/` is the default workspace (merge target, push source)
- Repo root is metadata only (`.git/`, `.manifold/`, `ws/`, config files) — no source files at root
- `ws/` is gitignored
- Each workspace is an isolated git worktree with its own working copy
- Manifold metadata lives in `.manifold/` and `refs/manifold/*`
- Agents never block each other - conflicts are detected at merge time

<!-- edict:managed-start -->
## Edict Workflow

### How to Make Changes

1. **Create a bone** to track your work: `maw exec default -- bn create --title "..." --description "..."`
2. **Create a workspace** for your changes: `maw ws create <name> --from main` — or use `--change <change-id>` for change-bound work; this gives you `ws/<name>/`
3. **Edit files in your workspace** (`ws/<name>/`), never in `ws/default/`
4. **Merge when done**: `maw ws merge <name> --into default --destroy --message "feat: <bone-title>"` (use conventional commit prefix: `feat:`, `fix:`, `chore:`, etc.; swap `default` for a change id when merging back into a tracked change)
5. **Close the bone**: `maw exec default -- bn done <id>`

Do not create git branches manually — `maw ws create` handles branching for you. See [worker-loop.md](.agents/edict/worker-loop.md) for the full triage → start → work → finish cycle.

**All tools have `--help`** with usage examples. When unsure, run `<tool> --help` or `<tool> <command> --help`.

### Directory Structure (maw v2)

This project uses a **bare repo** layout. Source files live in workspaces under `ws/`, not at the project root.

```
project-root/          ← bare repo (no source files here)
├── ws/
│   ├── default/       ← main working copy (AGENTS.md, .bones/, src/, etc.)
│   ├── frost-castle/  ← agent workspace (isolated Git worktree)
│   └── amber-reef/    ← another agent workspace
├── .manifold/         ← maw metadata/artifacts
├── .git/              ← git data (core.bare=true)
└── AGENTS.md          ← stub redirecting to ws/default/AGENTS.md
```

**Key rules:**
- `ws/default/` is the main workspace — bones, config, and project files live here
- **Never merge or destroy the default workspace.** It is where other branches merge INTO, not something you merge.
- Agent workspaces (`ws/<name>/`) are isolated Git worktrees managed by maw
- Use `maw exec <ws> -- <command>` to run commands in a workspace context
- Use `maw exec default -- bn ...` for bones commands (always in default workspace)
- Use `maw exec <ws> -- seal ...` for review commands (always in the review's workspace)
- Never run `bn` or `seal` directly — always go through `maw exec`
- Do not run `jj`; this workflow is Git + maw.

### Bones Quick Reference

| Operation | Command |
|-----------|---------|
| Triage (scores) | `maw exec default -- bn triage` |
| Next bone | `maw exec default -- bn next` |
| Next N bones | `maw exec default -- bn next N` (e.g., `bn next 4` for dispatch) |
| Show bone | `maw exec default -- bn show <id>` |
| Create | `maw exec default -- bn create --title "..." --description "..."` |
| Start work | `maw exec default -- bn do <id>` |
| Add comment | `maw exec default -- bn bone comment add <id> "message"` |
| Close | `maw exec default -- bn done <id>` |
| Add dependency | `maw exec default -- bn triage dep add <blocker> --blocks <blocked>` |
| Search | `maw exec default -- bn search <query>` |

Identity resolved from `$AGENT` env. No flags needed in agent loops.

### Workspace Quick Reference

| Operation | Command |
|-----------|---------|
| Create workspace | `maw ws create <name> --from main` |
| List workspaces | `maw ws list` |
| Check merge readiness | `maw ws merge <name> --into default --check` |
| Merge to main | `maw ws merge <name> --into default --destroy --message "feat: <bone-title>"` |
| Destroy (no merge) | `maw ws destroy <name>` |
| Run command in workspace | `maw exec <name> -- <command>` |
| Diff workspace vs epoch | `maw ws diff <name>` |
| Check workspace overlap | `maw ws overlap <name1> <name2>` |
| View workspace history | `maw ws history <name>` |
| Sync stale workspace | `maw ws sync <name>` |
| Inspect merge conflicts | `maw ws conflicts <name>` |
| Undo local workspace changes | `maw ws undo <name>` |
| List recovery snapshots | `maw ws recover` |
| Recover destroyed workspace | `maw ws recover <name> --to <new-name>` |
| Search recovery snapshots | `maw ws recover --search <pattern>` |
| Show file from snapshot | `maw ws recover <name> --show <path>` |

**Inspecting a workspace (use git, not jj):**
```bash
maw exec <name> -- git status             # what changed (unstaged)
maw exec <name> -- git log --oneline -5   # recent commits
maw ws diff <name>                        # diff vs epoch (maw-native)
```

**Lead agent merge workflow** — after a worker finishes a bone:
1. `maw ws list` — look for `active (+N to merge)` entries
2. `maw ws merge <name> --into default --check` — verify no conflicts
3. `maw ws merge <name> --into default --destroy --message "feat: <bone-title>"` — merge and clean up (use conventional commit prefix)

**Workspace safety:**
- Never merge or destroy `default`.
- Always `maw ws merge <name> --into default --check` before `--destroy`.
- Commit workspace changes with `maw exec <name> -- git add -A && maw exec <name> -- git commit -m "..."`.
- **No work is ever lost in maw.** Recovery snapshots are created automatically on every destroy. If a workspace was destroyed and you suspect code is missing, ALWAYS run `maw ws recover` before concluding work was lost. Never reopen a bone or start over without checking recovery first.

### Protocol Quick Reference

Use these commands at protocol transitions to check state and get exact guidance. Each command outputs instructions for the next steps.

| Step | Command | Who | Purpose |
|------|---------|-----|---------|
| Resume | `edict protocol resume --agent $AGENT` | Worker | Detect in-progress work from previous session |
| Start | `edict protocol start <bone-id> --agent $AGENT` | Worker | Verify bone is ready, get start commands |
| Review | `edict protocol review <bone-id> --agent $AGENT` | Worker | Verify work is complete, get review commands |
| Finish | `edict protocol finish <bone-id> --agent $AGENT` | Worker | Verify review approved, get close/cleanup commands |
| Merge | `edict protocol merge <workspace> --agent $AGENT` | Lead | Check preconditions, detect conflicts, get merge steps |
| Cleanup | `edict protocol cleanup --agent $AGENT` | Worker | Check for held resources to release |

All commands support JSON output with `--format json` for parsing. If a command is unavailable or fails (exit code 1), fall back to manual steps documented in [start](.agents/edict/start.md), [review-request](.agents/edict/review-request.md), and [finish](.agents/edict/finish.md).

### Bones Conventions

- Create a bone before starting work. Update state: `open` → `doing` → `done`.
- Post progress comments during work for crash recovery.
- **Run checks before committing**: `just check` (or your project's build/test command). Fix any failures before proceeding.
- After finishing a bone, follow [finish.md](.agents/edict/finish.md). **Workers: do NOT push** — the lead handles merges and pushes.

### Release Instructions

- Bump the version of all crates
- Regenerate the Cargo.lock
- Add notes to CHANGELOG.md
- If the README.md references the version, update it.
- Commit
- Tag and push: `maw release vX.Y.Z`
- use `gh release create vX.Y.Z --notes "..."`
- Install locally: `maw exec default -- just install`

### Identity

Your agent name is set by the hook or script that launched you. Use `$AGENT` in commands.
For manual sessions, use `<project>-dev` (e.g., `myapp-dev`).

### Claims

When working on a bone, stake claims to prevent conflicts:

```bash
rite claims stake --agent $AGENT "bone://<project>/<id>" -m "<id>"
rite claims stake --agent $AGENT "workspace://<project>/<ws>" -m "<id>"
rite claims release --agent $AGENT --all  # when done
```

### Reviews

Use `@<project>-<role>` mentions to request reviews:

```bash
maw exec $WS -- seal reviews request <review-id> --reviewers $PROJECT-security --agent $AGENT
rite send --agent $AGENT $PROJECT "Review requested: <review-id> @$PROJECT-security" -L review-request
```

The @mention triggers the auto-spawn hook for the reviewer.

### Bus Communication

Agents communicate via rite channels. You don't need to be expert on everything — ask the right project.

| Operation | Command |
|-----------|---------|
| Send message | `rite send --agent $AGENT <channel> "message" [-L label]` |
| Check inbox | `rite inbox --agent $AGENT --channels <ch> [--mark-read]` |
| Wait for reply | `rite wait -c <channel> --mention -t 120` |
| Browse history | `rite history <channel> -n 20` |
| Search messages | `rite search "query" -c <channel>` |

**Conversations**: After sending a question, use `rite wait -c <channel> --mention -t <seconds>` to block until the other agent replies. This enables back-and-forth conversations across channels.

**Project experts**: Each `<project>-dev` is the expert on their project. When stuck on a companion tool (rite, maw, seal, vessel, bn), post a question to its project channel instead of guessing.

### Cross-Project Communication

**Don't suffer in silence.** If a tool confuses you or behaves unexpectedly, post to its project channel.

1. Find the project: `rite history projects -n 50` (the #projects channel has project registry entries)
2. Post question or feedback: `rite send --agent $AGENT <project> "..." -L feedback`
3. For bugs, create bones in their repo first
4. **Always create a local tracking bone** so you check back later:
   ```bash
   maw exec default -- bn create --title "[tracking] <summary>" --tag tracking --kind task
   ```

See [cross-channel.md](.agents/edict/cross-channel.md) for the full workflow.

### Session Search (optional)

Use `cass search "error or problem"` to find how similar issues were solved in past sessions.


### Design Guidelines


- [CLI tool design for humans, agents, and machines](.agents/edict/design/cli-conventions.md)



### Workflow Docs


- [Find work from inbox and bones](.agents/edict/triage.md)

- [Claim bone, create workspace, announce](.agents/edict/start.md)

- [Change bone state (open/doing/done)](.agents/edict/update.md)

- [Close bone, merge workspace, release claims](.agents/edict/finish.md)

- [Full triage-work-finish lifecycle](.agents/edict/worker-loop.md)

- [Turn specs/PRDs into actionable bones](.agents/edict/planning.md)

- [Explore unfamiliar code before planning](.agents/edict/scout.md)

- [Create and validate proposals before implementation](.agents/edict/proposal.md)

- [Request a review](.agents/edict/review-request.md)

- [Handle reviewer feedback (fix/address/defer)](.agents/edict/review-response.md)

- [Reviewer agent loop](.agents/edict/review-loop.md)

- [Merge a worker workspace (protocol merge + conflict recovery)](.agents/edict/merge-check.md)

- [Validate toolchain health](.agents/edict/preflight.md)

- [Ask questions, report bugs, and track responses across projects](.agents/edict/cross-channel.md)

- [Report bugs/features to other projects](.agents/edict/report-issue.md)

- [groom](.agents/edict/groom.md)

<!-- edict:managed-end -->
