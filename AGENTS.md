# Manifold (next-gen maw)

Project type: cli
Tools: `beads`, `maw`, `crit`, `botbus`, `botty`
Reviewer roles: security

## IMPORTANT: This is the Manifold development repo

This repo is a clone of maw on the **`manifold`** branch. It is where we build the next generation of maw — replacing jj with git worktrees and implementing the Manifold architecture described in `notes/manifold-v2.md`.

**Branch rules:**
- Normal development in this repo targets the **`manifold`** branch.
- The `.maw.toml` is configured with `branch = "manifold"`. Use `maw push` and `maw ws merge` normally — they target `manifold` automatically.
- Final cutover is allowed: merge `manifold` into `main`, then retire `~/src/manifold` and continue in `~/src/maw`.
- Avoid raw `jj git push` here — it can push all changed bookmarks, including `main`. Prefer `maw push` / `maw push --advance` while this repo is still active.
- Avoid `jj bookmark set main` / `jj git push --bookmark main` during normal manifold development to prevent accidental main movement.
- **NEVER run `maw release`, `cargo install`, `just install`, or `cargo install --path .`** from this repo. Manifold is in active development and installing it replaces the stable maw binary that other projects depend on. Releases and installs happen from `~/src/maw` on the `main` branch only.

**Repo layout:**
- `~/src/maw` — current maw, ships bugfixes on `main`
- `~/src/manifold` (this repo) — Manifold development on `manifold` branch
- Both repos share the same GitHub remote (`bobisme/maw`)

**Design doc:** `notes/manifold-v2.md` — full architecture, data model, implementation phases.

## Repo model terminology (important)

Use these terms precisely when discussing migration work:

- **v1 model (legacy):** `.workspaces/` layout with jj-centric workspace handling.
- **v2 bare model (legacy transition):** `ws/<name>/` layout with bare-root workflow. This model predates full Manifold metadata adoption.
- **Manifold model (target):** Manifold metadata and transport (`.manifold/`, `refs/manifold/*`) replacing jj-specific coordination paths.

Current repo state is a **hybrid transition**: some commands and docs are still jj-based while Manifold pieces are being integrated. Do not assume "v2" means "fully Manifold".

---

This project uses **maw** for workspace management, **jj** (Jujutsu) for version control, and **beads** for issue tracking.

---

## Quick Start

```bash
# Create your workspace (automatically creates a commit you own)
maw ws create <your-name>

# Work - jj tracks changes automatically (use the absolute path shown by create)
# ... edit files in ws/<your-name>/ ...
maw exec <your-name> -- jj describe -m "feat: what you're implementing"

# Check status (see all agent work, conflicts, stale warnings)
maw ws status

# When done, merge all work from the repo root
maw ws merge alice bob --destroy
```

**Key concept:** Each workspace gets its own commit. You own your commit - no other agent will modify it. This prevents conflicts during concurrent work.

**Note:** Your workspace starts with an empty "wip" commit - this is intentional. The empty commit gives you ownership immediately, preventing divergent commits when multiple agents work concurrently. Just describe or commit your changes as you work; empty commits are naturally handled during merge.

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

| Task | Command |
|------|---------|
| Create workspace | `maw ws create <name>` |
| List workspaces | `maw ws list` |
| Quick status overview | `maw status` |
| Check status | `maw ws status` |
| Handle stale workspace | `maw ws sync` |
| Run any command in workspace | `maw exec <name> -- <cmd> <args>` |
| Merge agent work | `maw ws merge <a> <b>` |
| Merge and cleanup | `maw ws merge <a> <b> --destroy` |
| Destroy workspace | `maw ws destroy <name>` |

Note: Destroy commands are non-interactive by default (agents can't respond to prompts). Use `--confirm` if you want interactive confirmation.

### Running Commands in Workspaces

In sandboxed environments where `cd` doesn't persist between tool calls, use `maw exec` to run any command inside a workspace:

```bash
maw exec alice -- cargo test
maw exec alice -- br list
maw exec alice -- ls -la src/
```

---

## Working in Your Workspace

### Making Changes

jj automatically tracks changes - no `git add` needed.

```bash
# See what you've changed
jj diff
jj status

# Describe your work (saves to current commit)
jj describe -m "feat: description of changes"

# Or commit and start fresh
jj commit -m "feat: completed feature"
```

### Staying in Sync

```bash
# See commits (includes all workspaces by default)
jj log

# See only workspace working copies
jj log -r 'working_copies()'

# If workspace is stale (another workspace modified shared history)
maw ws sync
```

**Important**: Unlike git worktrees, jj workspaces share the entire repo state. If another workspace modifies a commit in your ancestry, your workspace becomes "stale". Always run `maw ws sync` at the start of a session.

### Handling Conflicts

jj records conflicts in commits rather than blocking. If you see conflicts:

```bash
jj status  # shows conflicted files
# Edit files to resolve (remove conflict markers)
jj describe -m "resolve: merge conflicts"
```

### Handling Divergent Commits

Divergent commits are rare with maw because each agent gets their own commit. But if `maw ws status` shows "Divergent Commits":

```bash
# View divergent commits
jj log  # look for (divergent) markers

# Fix by abandoning unwanted versions
jj abandon <change-id>/0   # keep /1, abandon /0
```

**Important**: Only modify your own commits. Don't run `jj describe main` or modify other shared commits - this can cause divergence if another agent does the same concurrently.

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

# Create a crit review (see Crit section below for full details)
crit reviews create --title "feat: description of change"
```

After review is approved:

```bash
crit reviews approve <review_id>
crit reviews merge <review_id>
```

### 3. Version Bump (for releases)

```bash
# Edit Cargo.toml version (e.g., 0.1.0 → 0.2.0)
# Also update the install command version tag in README.md

jj describe -m "chore: bump version to X.Y.Z

Co-Authored-By: <model-name> <model-email>"
```

### 4. Push to Remote

```bash
# If bookmark is already set (e.g., after maw ws merge):
maw push

# If you committed directly and need to advance the branch bookmark:
maw push --advance
```

`maw push` pushes the configured branch to origin with sync checks and clear error messages.
`--advance` moves the branch bookmark to `@-` (parent of working copy) before pushing — use this after committing work directly (not via `maw ws merge`, which sets the bookmark automatically).

**Understanding push output**: When the output says `Changes to push to origin:` followed by branch/bookmark info, **the push has already completed**. This is a confirmation, not a preview.

### 5. Tag the Release

```bash
# Tag and push the release
maw release vX.Y.Z

# Install locally and verify
cargo install --path .
maw --version
```

### First-time Setup (colocated repos)

If `main` bookmark doesn't exist or isn't tracking remote:

```bash
jj bookmark track main@origin  # Track remote main
```

### Troubleshooting

**Push issues**: `maw push` handles bookmark management automatically. If it fails, it will tell you why and how to fix it. For manual recovery: `jj bookmark set main -r @-` then `jj git push`.

**"Bookmark is behind remote"** - Someone else pushed. Pull first: `jj git fetch && jj rebase -d main@origin`.

### Quick Reference

| Stage | Key Commands |
|-------|--------------|
| Merge work | `maw ws merge <a> <b> --destroy` |
| Create review | `crit reviews create --title "..."` |
| Approve/merge review | `crit reviews approve <id> && crit reviews merge <id>` |
| Bump version | Edit `Cargo.toml` + `README.md`, then `jj describe` |
| Push (after merge) | `maw push` |
| Push (after direct commit) | `maw push --advance` |
| Tag release | `maw release vX.Y.Z` |

---

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for release history. Update it as part of every release.

## Issue Tracking with Beads

```bash
# View issues
br list               # All issues
br ready              # Issues ready to work (no blockers)
br show <id>          # Full details

# Work on issues
br update <id> --status=in_progress
br close <id> --reason="Completed"

# Create issues
br create --title="..." --type=task --priority=2
```

### Workflow

1. `br ready` - find actionable work
2. `br update <id> --status=in_progress` - claim it
3. Do the work
4. `br close <id> --reason="Done"` - mark complete

---

## Conventions

- **Commit messages**: Use conventional commits (`feat:`, `fix:`, `docs:`, etc.)
- **Co-author**: Include a model-specific `Co-Authored-By` trailer in commits (`Claude <noreply@anthropic.com>` for Claude models, `Codex <codex@openai.com>` for Codex/OpenAI models)
- **Workspace names**: Lowercase alphanumeric with hyphens/underscores (`agent-1`, `feature-x`)
- **Versioning**: Use semantic versioning. Tag releases with `v` prefix (`v0.1.0`). Update Cargo.toml version and README install command before tagging.
- **Agent identity**: When announcing releases or responding on botbus, use `--agent maw-dev` and post to `#maw` channel.
- **Issue tracking**: Use `br` (beads) for issue tracking. File beads for bugs and feature requests. Triage community feedback from botbus.
- **Release announcements**: Always use `bus send --no-hooks --agent maw-dev maw "..."` for release announcements. The `--no-hooks` flag prevents auto-spawn hooks from triggering on announcement messages.
- **Release process**: commit via jj → bump version in Cargo.toml + README.md → update CHANGELOG.md → `maw push --advance` → `maw release vX.Y.Z` → `just install` → announce on botbus #maw as maw-dev.

---

## Output Guidelines

maw is frequently invoked by agents with **no prior context**. Every piece of tool output must be self-contained and actionable.

**Errors** must include:
- What failed (include stderr from jj when available)
- How to fix it (exact command to run)
- Example: `"jj workspace add failed: {stderr}\n  Check: maw doctor"`

**Success output** must include:
- What happened
- What to do next (exact commands)
- Example: `"Workspace 'agent-a' ready!\n  Path: /abs/path\n  maw exec agent-a -- jj describe -m \"feat: ...\""`

**Principles**:
- Agents can't remember prior output — every message must stand alone
- Include copy-pasteable commands, not just descriptions
- Keep it brief — agents are token-conscious
- Use structured prefixes where appropriate: `WARNING:`, `IMPORTANT:`, `To fix:`, `Next:`
- Assume agents have **zero jj knowledge** — maw is their first contact with jj. Every jj concept (describe, working copy, stale, bookmarks, @- syntax) needs a one-line explanation the first time it appears in a given output context
- All --help text and runtime output must work in **sandboxed environments** where `cd` doesn't persist between tool calls. Never instruct agents to `cd` into a workspace — use `maw exec <name> -- <cmd>` for all commands in workspaces
- All file operation instructions must reference **absolute workspace paths**, not relative ones. Agents use Read/Write/Edit tools with absolute paths, not just bash

---

## Architecture

- **v2 bare repo model**: Workspaces live in `ws/<name>/`
- `ws/default/` is the default workspace (merge target, push source)
- No default workspace — repo root is metadata only (`.git/`, `.jj/`, `ws/`, config files)
- `ws/` is gitignored
- Each workspace is a separate working copy sharing the single `.jj/` backing store
- `jj log` shows commits across all workspaces by default
- Agents never block each other - conflicts are recorded, not blocking


<!-- botbox:managed-start -->
## Botbox Workflow

**New here?** Read [worker-loop.md](.agents/botbox/worker-loop.md) first — it covers the complete triage → start → work → finish cycle.

**All tools have `--help`** with usage examples. When unsure, run `<tool> --help` or `<tool> <command> --help`.

### Directory Structure (maw v2)

This project uses a **bare repo** layout. Source files live in workspaces under `ws/`, not at the project root.

```
project-root/          ← bare repo (no source files here)
├── ws/
│   ├── default/       ← main working copy (AGENTS.md, .beads/, src/, etc.)
│   ├── frost-castle/  ← agent workspace (isolated jj commit)
│   └── amber-reef/    ← another agent workspace
├── .jj/               ← jj repo data
├── .git/              ← git data (core.bare=true)
├── AGENTS.md          ← stub redirecting to ws/default/AGENTS.md
└── CLAUDE.md          ← symlink → AGENTS.md
```

**Key rules:**
- `ws/default/` is the main workspace — beads, config, and project files live here
- **Never merge or destroy the default workspace.** It is where other branches merge INTO, not something you merge.
- Agent workspaces (`ws/<name>/`) are isolated jj commits for concurrent work
- Use `maw exec <ws> -- <command>` to run commands in a workspace context
- Use `maw exec default -- br|bv ...` for beads commands (always in default workspace)
- Use `maw exec <ws> -- crit ...` for review commands (always in the review's workspace)
- Never run `br`, `bv`, `crit`, or `jj` directly — always go through `maw exec`

### Beads Quick Reference

| Operation | Command |
|-----------|---------|
| View ready work | `maw exec default -- br ready` |
| Show bead | `maw exec default -- br show <id>` |
| Create | `maw exec default -- br create --actor $AGENT --owner $AGENT --title="..." --type=task --priority=2` |
| Start work | `maw exec default -- br update --actor $AGENT <id> --status=in_progress --owner=$AGENT` |
| Add comment | `maw exec default -- br comments add --actor $AGENT --author $AGENT <id> "message"` |
| Close | `maw exec default -- br close --actor $AGENT <id>` |
| Add dependency | `maw exec default -- br dep add --actor $AGENT <blocked> <blocker>` |
| Sync | `maw exec default -- br sync --flush-only` |
| Triage (scores) | `maw exec default -- bv --robot-triage` |
| Next bead | `maw exec default -- bv --robot-next` |

**Required flags**: `--actor $AGENT` on mutations, `--author $AGENT` on comments.

### Workspace Quick Reference

| Operation | Command |
|-----------|---------|
| Create workspace | `maw ws create <name>` |
| List workspaces | `maw ws list` |
| Merge to main | `maw ws merge <name> --destroy` |
| Destroy (no merge) | `maw ws destroy <name>` |
| Run jj in workspace | `maw exec <name> -- jj <jj-args...>` |

**Avoiding divergent commits**: Each workspace owns ONE commit. Only modify your own.

| Safe | Dangerous |
|------|-----------|
| `maw ws merge <agent-ws> --destroy` | `maw ws merge default --destroy` (NEVER) |
| `jj describe` (your working copy) | `jj describe main -m "..."` |
| `maw exec <your-ws> -- jj describe -m "..."` | `jj describe <other-change-id>` |

If you see `(divergent)` in `jj log`:
```bash
jj abandon <change-id>/0   # keep one, abandon the divergent copy
```

**Working copy snapshots**: jj auto-snapshots your working copy before most operations (`jj new`, `jj rebase`, etc.). Edits go into the **current** commit automatically. To put changes in a **new** commit, run `jj new` first, then edit files.

**Always pass `-m`**: Commands like `jj commit`, `jj squash`, and `jj describe` open an editor by default. Agents cannot interact with editors, so always pass `-m "message"` explicitly.

### Protocol Quick Reference

Use these commands at protocol transitions to check state and get exact guidance. Each command outputs instructions for the next steps.

| Step | Command | Who | Purpose |
|------|---------|-----|---------|
| Resume | `botbox protocol resume --agent $AGENT` | Worker | Detect in-progress work from previous session |
| Start | `botbox protocol start <bead-id> --agent $AGENT` | Worker | Verify bead is ready, get start commands |
| Review | `botbox protocol review <bead-id> --agent $AGENT` | Worker | Verify work is complete, get review commands |
| Finish | `botbox protocol finish <bead-id> --agent $AGENT` | Worker | Verify review approved, get close/cleanup commands |
| Merge | `botbox protocol merge <workspace> --agent $AGENT` | Lead | Check preconditions, detect conflicts, get merge steps |
| Cleanup | `botbox protocol cleanup --agent $AGENT` | Worker | Check for held resources to release |

All commands support JSON output with `--format json` for parsing. If a command is unavailable or fails (exit code 1), fall back to manual steps documented in [start](.agents/botbox/start.md), [review-request](.agents/botbox/review-request.md), and [finish](.agents/botbox/finish.md).

### Beads Conventions

- Create a bead before starting work. Update status: `open` → `in_progress` → `closed`.
- Post progress comments during work for crash recovery.
- **Run checks before requesting review**: `just check` (or your project's build/test command). Fix any failures before proceeding.
- After finishing a bead, follow [finish.md](.agents/botbox/finish.md). **Workers: do NOT push** — the lead handles merges and pushes.
- **Install locally** after releasing: `maw exec default -- just install`

### Identity

Your agent name is set by the hook or script that launched you. Use `$AGENT` in commands.
For manual sessions, use `<project>-dev` (e.g., `myapp-dev`).

### Claims

When working on a bead, stake claims to prevent conflicts:

```bash
bus claims stake --agent $AGENT "bead://<project>/<id>" -m "<id>"
bus claims stake --agent $AGENT "workspace://<project>/<ws>" -m "<id>"
bus claims release --agent $AGENT --all  # when done
```

### Reviews

Use `@<project>-<role>` mentions to request reviews:

```bash
maw exec $WS -- crit reviews request <review-id> --reviewers $PROJECT-security --agent $AGENT
bus send --agent $AGENT $PROJECT "Review requested: <review-id> @$PROJECT-security" -L review-request
```

The @mention triggers the auto-spawn hook for the reviewer.

### Bus Communication

Agents communicate via bus channels. You don't need to be expert on everything — ask the right project.

| Operation | Command |
|-----------|---------|
| Send message | `bus send --agent $AGENT <channel> "message" [-L label]` |
| Check inbox | `bus inbox --agent $AGENT --channels <ch> [--mark-read]` |
| Wait for reply | `bus wait -c <channel> --mention -t 120` |
| Browse history | `bus history <channel> -n 20` |
| Search messages | `bus search "query" -c <channel>` |

**Conversations**: After sending a question, use `bus wait -c <channel> --mention -t <seconds>` to block until the other agent replies. This enables back-and-forth conversations across channels.

**Project experts**: Each `<project>-dev` is the expert on their project. When stuck on a companion tool (bus, maw, crit, botty, br), post a question to its project channel instead of guessing.

### Cross-Project Communication

**Don't suffer in silence.** If a tool confuses you or behaves unexpectedly, post to its project channel.

1. Find the project: `bus history projects -n 50` (the #projects channel has project registry entries)
2. Post question or feedback: `bus send --agent $AGENT <project> "..." -L feedback`
3. For bugs, create beads in their repo first
4. **Always create a local tracking bead** so you check back later:
   ```bash
   maw exec default -- br create --actor $AGENT --owner $AGENT --title="[tracking] <summary>" --labels tracking --type=task --priority=3
   ```

See [cross-channel.md](.agents/botbox/cross-channel.md) for the full workflow.

### Session Search (optional)

Use `cass search "error or problem"` to find how similar issues were solved in past sessions.


### Design Guidelines


- [CLI tool design for humans, agents, and machines](.agents/botbox/design/cli-conventions.md)



### Workflow Docs


- [Find work from inbox and beads](.agents/botbox/triage.md)

- [Claim bead, create workspace, announce](.agents/botbox/start.md)

- [Change bead status (open/in_progress/blocked/done)](.agents/botbox/update.md)

- [Close bead, merge workspace, release claims, sync](.agents/botbox/finish.md)

- [Full triage-work-finish lifecycle](.agents/botbox/worker-loop.md)

- [Turn specs/PRDs into actionable beads](.agents/botbox/planning.md)

- [Explore unfamiliar code before planning](.agents/botbox/scout.md)

- [Create and validate proposals before implementation](.agents/botbox/proposal.md)

- [Request a review](.agents/botbox/review-request.md)

- [Handle reviewer feedback (fix/address/defer)](.agents/botbox/review-response.md)

- [Reviewer agent loop](.agents/botbox/review-loop.md)

- [Merge a worker workspace (protocol merge + conflict recovery)](.agents/botbox/merge-check.md)

- [Validate toolchain health](.agents/botbox/preflight.md)

- [Ask questions, report bugs, and track responses across projects](.agents/botbox/cross-channel.md)

- [Report bugs/features to other projects](.agents/botbox/report-issue.md)

- [groom](.agents/botbox/groom.md)

<!-- botbox:managed-end -->
