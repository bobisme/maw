# maw

Project type: cli
Tools: `beads`, `maw`, `crit`, `botbus`, `botty`
Reviewer roles: security

<!-- Add project-specific context below: architecture, conventions, key files, etc. -->

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
| Tag release | `jj tag set vX.Y.Z -r main` then `git push origin vX.Y.Z` |

---

## Release Notes

### v0.25.0

- `maw ws jj` now detects stale workspaces and prints a warning with fix command (`maw ws sync`) before running the jj command (bd-1bi).
- All workspace path outputs now include trailing `/` for easier copy-paste into file paths (bd-3m9).

### v0.24.0

- Add `maw push --advance` flag — moves the branch bookmark to `@-` (parent of working copy) before pushing. Use after committing directly (version bumps, hotfixes). Without the flag, `maw push` now detects unpushed work at `@-` and suggests `--advance`.
- Update all agent docs (CLAUDE.md, AGENTS.md, finish.md) to use `maw push` consistently instead of manual `jj bookmark set` + `jj git push`.

### v0.23.0

- Add `maw push` command — replaces manual `jj bookmark set main -r @-` + `jj git push` workflow. Handles bookmark management, sync checks, and clear error messages.
- Post-merge: rebase default workspace onto branch so on-disk files reflect the merge immediately.
- Add `.maw.toml` config file support with `[merge]` section.
- Add `auto_resolve_from_main` config to auto-resolve conflicts in specified paths (e.g., `.beads/**`) during `maw ws merge`.
- Refine `maw status --status-bar` prompt glyphs and colors for workspace count, change count, and sync warning.
- Add `[repo]` config section with `branch` setting (default: `"main"`) — replaces hardcoded `"main"` throughout.

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
- **Release announcements**: Always use `bus send --no-hooks --agent maw-dev maw "..."` for release announcements. The `--no-hooks` flag prevents auto-spawn hooks from triggering on announcement messages.
- **Release process**: commit via jj → bump version in Cargo.toml + README.md → `maw push --advance` → `jj tag set vX.Y.Z -r main` → `git push origin vX.Y.Z` → `just install` → announce on botbus #maw as maw-dev.

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


<!-- botbox:managed-start -->
## Botbox Workflow

**New here?** Read [worker-loop.md](.agents/botbox/worker-loop.md) first — it covers the complete triage → start → work → finish cycle.

**All tools have `--help`** with usage examples. When unsure, run `<tool> --help` or `<tool> <command> --help`.

### Beads Quick Reference

| Operation | Command |
|-----------|---------|
| View ready work | `br ready` |
| Show bead | `br show <id>` |
| Create | `br create --actor $AGENT --owner $AGENT --title="..." --type=task --priority=2` |
| Start work | `br update --actor $AGENT <id> --status=in_progress --owner=$AGENT` |
| Add comment | `br comments add --actor $AGENT --author $AGENT <id> "message"` |
| Close | `br close --actor $AGENT <id>` |
| Add dependency | `br dep add --actor $AGENT <blocked> <blocker>` |
| Sync | `br sync --flush-only` |

**Required flags**: `--actor $AGENT` on mutations, `--author $AGENT` on comments.

### Workspace Quick Reference

| Operation | Command |
|-----------|---------|
| Create workspace | `maw ws create <name>` |
| List workspaces | `maw ws list` |
| Merge to main | `maw ws merge <name> --destroy` |
| Destroy (no merge) | `maw ws destroy <name>` |
| Run jj in workspace | `maw ws jj <name> <jj-args...>` |

**Avoiding divergent commits**: Each workspace owns ONE commit. Only modify your own.

| Safe | Dangerous |
|------|-----------|
| `jj describe` (your working copy) | `jj describe main -m "..."` |
| `maw ws jj <your-ws> describe -m "..."` | `jj describe <other-change-id>` |

If you see `(divergent)` in `jj log`:
```bash
jj abandon <change-id>/0   # keep one, abandon the divergent copy
```

### Beads Conventions

- Create a bead before starting work. Update status: `open` → `in_progress` → `closed`.
- Post progress comments during work for crash recovery.
- **Push to main** after completing beads (see [finish.md](.agents/botbox/finish.md)).

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
crit reviews request <review-id> --reviewers $PROJECT-security --agent $AGENT
bus send --agent $AGENT $PROJECT "Review requested: <review-id> @$PROJECT-security" -L review-request
```

The @mention triggers the auto-spawn hook for the reviewer.

### Cross-Project Communication

**Don't suffer in silence.** If a tool confuses you or behaves unexpectedly, post to its project channel.

1. Find the project: `bus history projects -n 50` (the #projects channel has project registry entries)
2. Post question or feedback: `bus send --agent $AGENT <project> "..." -L feedback`
3. For bugs, create beads in their repo first
4. **Always create a local tracking bead** so you check back later:
   ```bash
   br create --actor $AGENT --owner $AGENT --title="[tracking] <summary>" --labels tracking --type=task --priority=3
   ```

See [cross-channel.md](.agents/botbox/cross-channel.md) for the full workflow.

### Session Search (optional)

Use `cass search "error or problem"` to find how similar issues were solved in past sessions.


### Design Guidelines

- [CLI tool design for humans, agents, and machines](.agents/botbox/design/cli-conventions.md)

### Workflow Docs

- [Ask questions, report bugs, and track responses across projects](.agents/botbox/cross-channel.md)
- [Close bead, merge workspace, release claims, sync](.agents/botbox/finish.md)
- [groom](.agents/botbox/groom.md)
- [Verify approval before merge](.agents/botbox/merge-check.md)
- [Turn specs/PRDs into actionable beads](.agents/botbox/planning.md)
- [Validate toolchain health](.agents/botbox/preflight.md)
- [Create and validate proposals before implementation](.agents/botbox/proposal.md)
- [Report bugs/features to other projects](.agents/botbox/report-issue.md)
- [Reviewer agent loop](.agents/botbox/review-loop.md)
- [Request a review](.agents/botbox/review-request.md)
- [Handle reviewer feedback (fix/address/defer)](.agents/botbox/review-response.md)
- [Explore unfamiliar code before planning](.agents/botbox/scout.md)
- [Claim bead, create workspace, announce](.agents/botbox/start.md)
- [Find work from inbox and beads](.agents/botbox/triage.md)
- [Change bead status (open/in_progress/blocked/done)](.agents/botbox/update.md)
- [Full triage-work-finish lifecycle](.agents/botbox/worker-loop.md)
<!-- botbox:managed-end -->
