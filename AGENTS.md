# Agent Guide for MAW

This project uses **maw** for workspace management, **jj** (Jujutsu) for version control, and **beads** for issue tracking.

---

## Quick Start

```bash
# Create your workspace
maw ws create <your-name>
cd .workspaces/<your-name>

# Work - jj tracks changes automatically
# ... edit files ...
jj describe -m "feat: what you're implementing"

# Check status (see all agent work, conflicts, stale warnings)
maw ws status

# When done, merge all work from main workspace
cd /path/to/main/repo
maw ws merge --all --destroy
```

---

## Workspace Commands

| Task | Command |
|------|---------|
| Create workspace | `maw ws create <name>` |
| List workspaces | `maw ws list` |
| Check status | `maw ws status` |
| Handle stale workspace | `maw ws sync` |
| Merge all agent work | `maw ws merge --all` |
| Merge and cleanup | `maw ws merge --all --destroy` |
| Destroy workspace | `maw ws destroy <name>` |

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
# See all commits across all workspaces
jj log --all

# If workspace is stale (main repo changed)
maw ws sync
```

### Handling Conflicts

jj records conflicts in commits rather than blocking. If you see conflicts:

```bash
jj status  # shows conflicted files
# Edit files to resolve (remove conflict markers)
jj describe -m "resolve: merge conflicts"
```

---

## Finishing Work

### Merge All Agent Work

From the main workspace (not an agent workspace):

```bash
# Merge all agent workspaces into one commit
maw ws merge --all

# Or merge and clean up workspaces
maw ws merge --all --destroy

# Or merge specific workspaces
maw ws merge alice bob carol
```

Note: If there are conflicts, workspaces won't be destroyed. Resolve conflicts first.

---

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
- **Co-author**: Include `Co-Authored-By: Claude <noreply@anthropic.com>` in commits
- **Workspace names**: Alphanumeric with hyphens/underscores only (e.g., `alice`, `agent_1`)

---

## Architecture

- Workspaces live in `.workspaces/<name>/`
- Each workspace has its own `.jj/` but shares the backing store
- `.workspaces/` is gitignored
- `jj log --all` shows commits across all workspaces
- Agents never block each other - conflicts are recorded, not blocking
