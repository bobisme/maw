# Agent Guide for MAW

This project uses **jj** (Jujutsu) for version control, **botbus** for coordination, and **beads** for issue tracking.

---

## Getting Started

### 1. Create Your Workspace

```bash
# From the main maw directory
./scripts/agent-workspace.sh create <your-name>
cd .workspaces/<your-name>
```

This creates an isolated workspace where you can make changes without affecting other agents.

### 2. Set Your Identity

```bash
export BOTBUS_AGENT=<your-name>
botbus register --name <your-name> --description "Brief description of your role"
```

### 3. Announce Your Intent

Before starting work, tell others what you're doing:

```bash
botbus send general "Starting work on <task description>"
botbus claim "path/to/files/**" -m "Working on <feature>"
```

## Working

### Making Changes

Work normally - edit files, run tests, etc. jj automatically tracks changes.

```bash
# See what you've changed
jj diff

# See the commit graph
jj log

# Commit your work (creates a new commit on top)
jj commit -m "feat: description of changes"

# Or describe the current working copy commit
jj describe -m "wip: what I'm working on"
```

### Staying in Sync

```bash
# See all commits across all workspaces
jj log --all

# Pull in changes from main
jj rebase -d main
```

### Handling Conflicts

jj records conflicts in commits rather than blocking. If you see conflict markers:

```bash
# See what's conflicted
jj status

# Resolve conflicts in your editor, then
jj resolve  # or just edit the files directly

# The conflict is resolved when you commit/describe
```

## Finishing

### Landing Your Changes

```bash
# Squash your work into a clean commit
jj squash

# Move your bookmark to main (or create a PR)
jj bookmark set main -r @
```

### Cleanup

```bash
# Release your claims
botbus release --all

# Announce completion
botbus send general "Finished <task>. Ready for review."

# Optionally destroy your workspace
cd ../..  # back to maw root
./scripts/agent-workspace.sh destroy <your-name>
```

## Observing Other Agents

From any workspace:

```bash
# Watch real-time coordination
botbus ui

# See all commits from all agents
jj log --all

# See jj operation history
jj op log
```

## Quick Reference

| Task | Command |
|------|---------|
| Create workspace | `./scripts/agent-workspace.sh create <name>` |
| Register | `botbus register --name <name> --description "..."` |
| Claim files | `botbus claim "pattern/**" -m "reason"` |
| See changes | `jj diff` |
| Commit | `jj commit -m "message"` |
| See all work | `jj log --all` |
| Release claims | `botbus release --all` |
| Watch activity | `botbus ui` |

## Conventions

- **Commit messages**: Use conventional commits (`feat:`, `fix:`, `docs:`, etc.)
- **Co-author**: Include `Co-Authored-By: Claude <noreply@anthropic.com>` in commits
- **Claims**: Claim before editing, release when done
- **Communication**: Announce start/finish of significant work
- **Conflicts**: Resolve promptly, don't let them pile up

## Architecture Notes

### Why jj + botbus?

| Need | Tool | Why |
|------|------|-----|
| File isolation | jj workspaces | No disk duplication, instant creation |
| Concurrent edits | jj | Lock-free by design, merges operations |
| Intent/claims | botbus | Semantic "I own this module" not just file locks |
| Observability | botbus TUI | Watch agents coordinate in real-time |
| Conflict handling | jj | Records conflicts in commits, resolve later |
| History/undo | jj operation log | See what each agent did, undo mistakes |

### Workspace Internals

- Workspaces live in `.workspaces/<name>/`
- Each has its own `.jj/` but shares the backing store with main
- Main repo gitignores `.workspaces/` so nested working copies don't pollute commits
- `jj log --all` from any workspace shows all commits across all workspaces

---

## Issue Tracking with Beads

This project uses [beads](https://github.com/Dicklesworthstone/beads_rust) for issue tracking. Issues are stored in `.beads/` and tracked in jj/git.

### Essential Commands

```bash
# View issues
br list               # All issues
br ready              # Issues ready to work (no blockers)
br show <id>          # Full issue details with dependencies

# Create and update
br create --title="..." --type=task --priority=2
br update <id> --status=in_progress
br close <id> --reason="Completed"

# Sync to file (for version control)
br sync --flush-only
jj commit -m "chore: update beads"
```

### Workflow Pattern

1. **Find work**: Run `br ready` to see actionable issues
2. **Claim**: Use `br update <id> --status=in_progress`
3. **Work**: Implement the task
4. **Complete**: Use `br close <id> --reason="Done"`
5. **Sync**: Run `br sync --flush-only`, then commit

### Issue Quality

When creating issues, include:
- **Clear title**: What needs to be done
- **Description**: Context and acceptance criteria (`--description="..."`)
- **Labels**: Categorize with `--add-label` (e.g., `cleanup`, `tooling`, `docs`)
- **Priority**: 1 (highest) to 5 (lowest)
- **Type**: `task`, `bug`, `feature`, `epic`

### Using bv for Analysis

`bv` provides a TUI and robot-friendly commands for dependency analysis:

```bash
bv --robot-insights   # Graph metrics (PageRank, critical path, cycles)
bv --robot-plan       # Execution plan with parallel tracks
bv --robot-priority   # Priority recommendations with reasoning
bv --robot-help       # All AI-facing commands
```

Use these instead of parsing JSONL manually - bv computes dependency graphs correctly.

<!-- bv-agent-instructions-v1 -->

---

## Beads Workflow Integration

This project uses [beads_viewer](https://github.com/Dicklesworthstone/beads_viewer) for issue tracking. Issues are stored in `.beads/` and tracked in git.

### Essential Commands

```bash
# View issues (launches TUI - avoid in automated sessions)
bv

# CLI commands for agents (use these instead)
bd ready              # Show issues ready to work (no blockers)
bd list --status=open # All open issues
bd show <id>          # Full issue details with dependencies
bd create --title="..." --type=task --priority=2
bd update <id> --status=in_progress
bd close <id> --reason="Completed"
bd close <id1> <id2>  # Close multiple issues at once
bd sync               # Commit and push changes
```

### Workflow Pattern

1. **Start**: Run `bd ready` to find actionable work
2. **Claim**: Use `bd update <id> --status=in_progress`
3. **Work**: Implement the task
4. **Complete**: Use `bd close <id>`
5. **Sync**: Always run `bd sync` at session end

### Key Concepts

- **Dependencies**: Issues can block other issues. `bd ready` shows only unblocked work.
- **Priority**: P0=critical, P1=high, P2=medium, P3=low, P4=backlog (use numbers, not words)
- **Types**: task, bug, feature, epic, question, docs
- **Blocking**: `bd dep add <issue> <depends-on>` to add dependencies

### Session Protocol

**Before ending any session, run this checklist:**

```bash
git status              # Check what changed
git add <files>         # Stage code changes
bd sync                 # Commit beads changes
git commit -m "..."     # Commit code
bd sync                 # Commit any new beads changes
git push                # Push to remote
```

### Best Practices

- Check `bd ready` at session start to find available work
- Update status as you work (in_progress → closed)
- Create new issues with `bd create` when you discover tasks
- Use descriptive titles and set appropriate priority/type
- Always `bd sync` before ending session

<!-- end-bv-agent-instructions -->

<!-- maw-agent-instructions-v1 -->

## Multi-Agent Workflow with MAW

This project uses MAW for coordinating multiple agents via jj workspaces.

### Quick Reference

| Task | Command |
|------|---------|
| Create workspace | `maw ws create <name>` |
| List workspaces | `maw ws list` |
| Destroy workspace | `maw ws destroy <name>` |
| See all work | `jj log --all` |
| Update stale workspace | `jj workspace update-stale` |

### Starting Work

```bash
# Create your workspace
maw ws create <your-name>
cd .workspaces/<your-name>

# Start working - jj tracks changes automatically
# Describe what you're doing
jj describe -m "wip: implementing feature X"
```

### During Work

```bash
# See your changes
jj diff
jj status

# Commit and continue (creates new empty working copy)
jj commit -m "feat: add feature X"

# See what other agents are doing
jj log --all
```

### Handling Stale Workspace

If you see "working copy is stale", run:

```bash
jj workspace update-stale
```

This happens when the main repo changes while you're working.

### Finishing Work

```bash
# From the main workspace, merge your work
cd /path/to/main/repo
jj new <your-change-id> <other-agent-change-id>  # merge multiple
# or
jj rebase -r <your-change-id> -d main  # rebase onto main

# Clean up your workspace
maw ws destroy <your-name>
```

### Resolving Conflicts

jj records conflicts in commits rather than blocking. If you see conflicts:

```bash
jj status  # shows conflicted files
# Edit the files to resolve (remove conflict markers)
jj describe -m "resolve: merge conflicts from feature X and Y"
```

<!-- end-maw-agent-instructions -->
