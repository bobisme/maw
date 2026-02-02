# Agent Guide for maw

This project uses **maw** for workspace management, **jj** (Jujutsu) for version control, and **beads** for issue tracking.

---

## Quick Start

```bash
# Create your workspace (automatically creates a commit you own)
maw ws create <your-name>
cd .workspaces/<your-name>

# Work - jj tracks changes automatically
# ... edit files ...
jj describe -m "feat: what you're implementing"

# Check status (see all agent work, conflicts, stale warnings)
maw ws status

# When done, merge all work from main workspace
cd /path/to/main/repo
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
| Run jj in workspace | `maw ws jj <name> <args>` |
| Merge agent work | `maw ws merge <a> <b>` |
| Merge and cleanup | `maw ws merge <a> <b> --destroy` |
| Destroy workspace | `maw ws destroy <name>` |

Note: Destroy commands are non-interactive by default (agents can't respond to prompts). Use `--confirm` if you want interactive confirmation.

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

From the main workspace (not an agent workspace):

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

Co-Authored-By: Claude <noreply@anthropic.com>"
```

### 4. Push to Remote

jj commits are "floating" by default - they exist in history but aren't on any branch/bookmark. You must move `main` before pushing:

```bash
# Move main to the merge commit
# @- = parent of working copy (the actual commit with your changes)
jj bookmark set main -r @-

# Verify main is ahead of origin
jj log --limit 3

# Push to GitHub
jj git push
# NOTE: Despite output saying "Changes to push to origin:",
# the push is ALREADY DONE. Do NOT run git push afterwards.
```

### 5. Tag the Release

```bash
# Tag the release
jj tag set vX.Y.Z -r main
git push origin vX.Y.Z

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

**"Nothing to push"** - Bookmark wasn't moved. Check with `jj log` - if your commits aren't ancestors of `main`, run `jj bookmark set main -r <commit>`.

**"Bookmark is behind remote"** - Someone else pushed. Pull first: `jj git fetch && jj rebase -d main@origin`.

### Quick Reference

| Stage | Key Commands |
|-------|--------------|
| Merge work | `maw ws merge <a> <b> --destroy` |
| Create review | `crit reviews create --title "..."` |
| Approve/merge review | `crit reviews approve <id> && crit reviews merge <id>` |
| Bump version | Edit `Cargo.toml` + `README.md`, then `jj describe` |
| Push | `jj bookmark set main -r @-` then `jj git push` |
| Tag release | `jj tag set vX.Y.Z -r main` then `git push origin vX.Y.Z` |

---

## Release Notes

### Unreleased

- Refine `maw status --status-bar` prompt glyphs and colors for workspace count, change count, and sync warning.

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
- **Workspace names**: Lowercase alphanumeric with hyphens/underscores (`agent-1`, `feature-x`)
- **Versioning**: Use semantic versioning. Tag releases with `v` prefix (`v0.1.0`). Update Cargo.toml version and README install command before tagging.
- **Agent identity**: When announcing releases or responding on botbus, use `--agent maw-dev` and post to `#maw` channel.
- **Issue tracking**: Use `br` (beads) for issue tracking. File beads for bugs and feature requests. Triage community feedback from botbus.
- **Release process**: commit via jj → bump version in Cargo.toml + README.md → `jj bookmark set main -r @` → `jj git push` → `jj tag set vX.Y.Z -r main` → `git push origin vX.Y.Z` → `cargo install --path .` → announce on botbus #maw as maw-dev.

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
- Example: `"Workspace 'agent-a' ready!\n  Path: /abs/path\n  maw ws jj agent-a describe -m \"feat: ...\""`

**Principles**:
- Agents can't remember prior output — every message must stand alone
- Include copy-pasteable commands, not just descriptions
- Keep it brief — agents are token-conscious
- Use structured prefixes where appropriate: `WARNING:`, `IMPORTANT:`, `To fix:`, `Next:`
- Assume agents have **zero jj knowledge** — maw is their first contact with jj. Every jj concept (describe, working copy, stale, bookmarks, @- syntax) needs a one-line explanation the first time it appears in a given output context
- All --help text and runtime output must work in **sandboxed environments** where `cd` doesn't persist between tool calls. Never instruct agents to `cd` into a workspace — use `maw ws jj <name>` for jj commands and `cd /absolute/path && cmd` for other commands
- All file operation instructions must reference **absolute workspace paths**, not relative ones. Agents use Read/Write/Edit tools with absolute paths, not just bash

---

## Architecture

- Workspaces live in `.workspaces/<name>/`
- Each workspace is a separate working copy sharing the single `.jj/` backing store
- `.workspaces/` is gitignored
- `jj log` shows commits across all workspaces by default
- Agents never block each other - conflicts are recorded, not blocking

<!-- crit-agent-instructions -->

## Crit: Agent-Centric Code Review

This project uses [crit](https://github.com/anomalyco/botcrit) for distributed code reviews optimized for AI agents.

### Quick Start

```bash
# Initialize crit in the repository (once)
crit init

# Create a review for current change
crit reviews create --title "Add feature X"

# List open reviews
crit reviews list

# Check reviews needing your attention
crit reviews list --needs-review --author $BOTBUS_AGENT

# Show review details
crit reviews show <review_id>
```

### Adding Comments (Recommended)

The simplest way to comment on code - auto-creates threads:

```bash
# Add a comment on a specific line (creates thread automatically)
crit comment <review_id> --file src/main.rs --line 42 "Consider using Option here"

# Add another comment on same line (reuses existing thread)
crit comment <review_id> --file src/main.rs --line 42 "Good point, will fix"

# Comment on a line range
crit comment <review_id> --file src/main.rs --line 10-20 "This block needs refactoring"
```

### Managing Threads

```bash
# List threads on a review
crit threads list <review_id>

# Show thread with context
crit threads show <thread_id>

# Resolve a thread
crit threads resolve <thread_id> --reason "Fixed in latest commit"
```

### Voting on Reviews

```bash
# Approve a review (LGTM)
crit lgtm <review_id> -m "Looks good!"

# Block a review (request changes)
crit block <review_id> -r "Need more test coverage"
```

### Viewing Full Reviews

```bash
# Show full review with all threads and comments
crit review <review_id>

# Show with more context lines
crit review <review_id> --context 5

# List threads with first comment preview
crit threads list <review_id> -v
```

### Approving and Merging

```bash
# Approve a review (changes status to approved)
crit reviews approve <review_id>

# Mark as merged (after jj squash/merge)
# Note: Will fail if there are blocking votes
crit reviews merge <review_id>

# Self-approve and merge in one step (solo workflows)
crit reviews merge <review_id> --self-approve
```

### Agent Best Practices

1. **Set your identity** via environment:
   ```bash
   export BOTBUS_AGENT=my-agent-name
   ```

2. **Check for pending reviews** at session start:
   ```bash
   crit reviews list --needs-review --author $BOTBUS_AGENT
   ```

3. **Check status** to see unresolved threads:
   ```bash
   crit status <review_id> --unresolved-only
   ```

4. **Run doctor** to verify setup:
   ```bash
   crit doctor
   ```

### Output Formats

- Default output is TOON (token-optimized, human-readable)
- Use `--json` flag for machine-parseable JSON output

### Key Concepts

- **Reviews** are anchored to jj Change IDs (survive rebases)
- **Threads** group comments on specific file locations
- **crit comment** is the simple way to leave feedback (auto-creates threads)
- Works across jj workspaces (shared .crit/ in main repo)

<!-- end-crit-agent-instructions -->
