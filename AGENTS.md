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

### How to Make Changes

1. **Create a bone** to track your work: `bn create --title "..." --description "..."`
2. **Create a workspace** for your changes: `maw ws create <bone-id> --from main --description "<bone-title>"` — use the bone ID as workspace name; this gives you `.maw/workspaces/<bone-id>/`
3. **Edit files in your workspace** (`.maw/workspaces/<name>/`), never in the trunk at the repo root
4. **Merge when done**: `edict protocol merge <name> --message "feat: <bone-title>" --agent $AGENT`, then run the steps it prints in order. They record the Seal review, check for uncommitted changes in the same command as the merge, and block `risk:critical` work until a human approves. Run a bare `maw ws merge <name> --into default --destroy --message "feat: <bone-title>"` only for work with no review. Use a conventional commit prefix (`feat:`, `fix:`, `chore:`, etc.); swap `default` for a change id when merging back into a tracked change.
5. **Close the bone**: `bn done <id>`

Do not create git branches manually — `maw ws create` handles branching for you. See [worker-loop.md](.agents/edict/worker-loop.md) for the full triage → start → work → finish cycle.

**All tools have `--help`** with usage examples. When unsure, run `<tool> --help` or `<tool> <command> --help`.

### Conflicts Are Data, Not Errors

`maw ws sync` rebases committed-ahead workspaces onto the latest epoch by default. On conflict it does not abort — it commits labeled conflict markers and leaves the workspace `lifecycle:conflicted` (visible in `maw ws list`). Treat a conflicted workspace as a normal state, not a failure.

- `maw ws resolve <ws> --list` shows conflicts; `--keep epoch|<ws>|both|union` (or `--keep PATH=NAME`) resolves them.
- `maw ws merge` auto-syncs stale sources and accepts `--resolve cf-id=<ws>` / `--resolve-all=<ws>` to resolve inline.
- `maw ws conflicts <ws>` inspects conflict details.
- The one hard gate: merge refuses a source whose HEAD still has unresolved conflict markers (bypass with `--force` only for legitimate marker-like content).

### Directory Structure

This project uses the **root** layout. The project root is the trunk working copy — source files, `.bones/`, config, and `AGENTS.md` live there. Extra agent workspaces live under `.maw/workspaces/`.

```
project-root/              ← trunk working copy (AGENTS.md, .bones/, src/, etc.)
├── src/, AGENTS.md, …     ← your project files, edited here directly
├── .maw/
│   ├── workspaces/
│   │   ├── bn-1abc/       ← agent workspace (named after bone ID)
│   │   └── bn-2def/       ← another agent workspace
│   └── manifold/          ← maw metadata/artifacts
└── .git/                  ← git data
```

**Key rules:**
- The project root is the trunk — bones, config, and project files live here, and you edit them directly
- **Never merge or destroy the `default` workspace.** `default` names the trunk (the repo root); other workspaces merge INTO it, not the other way around.
- Agent workspaces (`.maw/workspaces/<name>/`) are isolated Git worktrees managed by maw
- Use `maw exec <ws> -- <command>` to run commands in a non-default workspace context
- Run `bn ...` directly at the repo root for bones commands (no `maw exec` prefix needed — they always target the trunk)
- Use `maw exec <ws> -- seal ...` for review commands (always in the review's workspace)

### Bones Quick Reference

| Operation | Command |
|-----------|---------|
| Triage (scores) | `bn triage` |
| Next bone | `bn next` |
| Next N bones | `bn next N` (e.g., `bn next 4` for dispatch) |
| Show bone | `bn show <id>` |
| Create | `bn create --title "..." --description "..."` |
| Start work | `bn do <id>` |
| Add comment | `bn bone comment add <id> "message"` |
| Close | `bn done <id>` |
| Add dependency | `bn triage dep add <blocker> --blocks <blocked>` |
| Search | `bn search <query>` |

Identity resolved from `$AGENT` env. No flags needed in agent loops.

### Workspace Quick Reference

| Operation | Command |
|-----------|---------|
| Create workspace | `maw ws create <bone-id> --from main --description "<title>"` |
| List workspaces | `maw ws list` |
| Check merge readiness | `maw ws merge <name> --into default --check` |
| Merge to main | `edict protocol merge <name> --message "feat: <bone-title>" --agent $AGENT`, then run its steps (bare `maw ws merge <name> --into default --destroy` only for work with no review) |
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

**Inspecting a workspace:**
```bash
maw exec <name> -- git status             # what changed (unstaged)
maw exec <name> -- git log --oneline -5   # recent commits
maw ws diff <name>                        # diff vs epoch (maw-native)
```

**Lead agent merge workflow** — after a worker finishes a bone:
1. `maw ws list` — look for `active (+N to merge)` entries
2. `edict protocol merge <name> --message "feat: <bone-title>" --agent $AGENT` — checks the bone, the review gate, `risk:critical`, and conflicts (use conventional commit prefix)
3. Run the steps it prints, in order. With a review they record it (`mark-merged`, commit `.seal/reviews/<review-id>`) and then merge with the clean check in the same command. Do not replace them with a bare `maw ws merge`: that skips the review log, the check, and the `risk:critical` gate. See [merge-check.md](.agents/edict/merge-check.md).

**Workspace safety:**
- Never merge or destroy `default`.
- Always `maw ws merge <name> --into default --check` before `--destroy`.
- Commit workspace changes with `maw exec <name> -- git add -A && maw exec <name> -- git commit -m "..."` before you request review. After the LGTM, commit only the review log.
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

`--reviewers` assigns the approval-gate identity in Seal. It does not spawn a
reviewer. For security review, create a Rite request anchor and explicitly
launch the one-review Daybreak session in
[security-review.md](.agents/edict/security-review.md). Never use an
`@<project>-security` mention: the ambient hook is retired.

```bash
maw exec $WS -- seal reviews request <review-id> --reviewers $PROJECT-security --agent $AGENT
req=$(rite send --agent $AGENT $PROJECT "Dedicated security re-review requested: <review-id> in $WS" -L review-response --format json | jq -r .id)
bn bone comment add <bone-id> "Review anchor: $req for <review-id>"
# Set review_id=<review-id>, ws=$WS, request_anchor=$req, kind=review-response.
# Follow .agents/edict/security-review.md's Launch contract exactly.
```

- Agentbus completion is not approval. Confirm the verdict with
  `maw exec $WS -- seal review <review-id> --format json` before proceeding.
- Agentbus unresolved, blocked, unavailable, or timeout: post one anchored
  `task-blocked` message, release the review claim, and stop. Do not retry by
  mention or scan another review.

**Dedicated reviewers** reply to the supplied request anchor with
`--reply-to "$request_anchor"` and `-L review-done`. A top-level verdict is not
an anchored result.

#### What a review covers

`seal reviews create` finds the fork point of your branch or workspace, so the review
covers every commit of the feature. It prints the range and commit count — check it.
`--base <rev>` sets the range explicitly; `--base <target>~1` reviews the tip commit only.
The base is persisted, so later commits extend the range instead of shifting it.

#### Do not commit code after the LGTM

An approval records the commit it covered. Commit code afterwards and
`seal reviews mark-merged` exits 1: "the approval does not cover the current code".

- **Fix**: get a fresh LGTM. A repeat vote moves the approval onto the new commit.
  Reviewers — that repeat LGTM is what unblocks the merge, so never leave a re-review
  unvoted.
- `--allow-stale-approval` merges past the check. Use it only when the new commits are
  provably outside what was reviewed, and say why in a bone comment.
- Check first: `maw exec $WS -- seal diff <review-id> --format json` reports
  `approval_stale`, `approved_commit` and `uncovered_commits`.

The review log is the one exception. Seal keeps it in `.seal/reviews/<review-id>/` in the
workspace, and the merge destroys the workspace. So, right before the merge:

```bash
maw exec $WS -- git status --porcelain --untracked-files=all          # nothing outside .seal/reviews/<review-id>/
maw exec $WS -- seal reviews mark-merged <review-id> --agent $AGENT   # HEAD is still the approved commit
maw exec $WS -- git add .seal/reviews/<review-id>
maw exec $WS -- git commit -m "chore: seal review <review-id>" -- .seal/reviews/<review-id>
{ out=$(maw exec $WS -- git status --porcelain --untracked-files=all -- . ':(exclude).seal/reviews/<review-id>') \
    && test -z "$out" || { echo "unreviewed changes: stop" >&2; false; }; } \
  && maw ws merge $WS --into default --destroy --message "feat: <bone-title>"   # check again, same command
```

`maw ws merge` also merges uncommitted additions and deletions, which no reviewer saw. If the
status check lists anything else, commit it and get a fresh LGTM. Whoever runs the merge runs these steps. Never run `mark-merged` after the merge, and never
move or delete `.seal/reviews/` to get past `maw ws sync`.

### Bus Communication

Agents communicate via rite channels. You don't need to be expert on everything — ask the right project.

| Operation | Command |
|-----------|---------|
| Send message | `rite send --agent $AGENT <channel> "message" [-L label]` |
| Reply to a message | `rite send --agent $AGENT <channel> "message" --reply-to <msg-id>` |
| Capture the id you sent | `rite send ... --format json \| jq -r .id` |
| Check inbox | `rite inbox --agent $AGENT --channels <ch> [--mark-read]` |
| Wait for an answer | `rite wait --agent $AGENT --reply-to <msg-id> -t 300 --format json` |
| Wait for any mention | `rite wait --mentions --from <agent> -t 120` |
| Read a thread | `rite history --thread <msg-id>` |
| Browse history | `rite history <channel> -n 20` |
| Search messages | `rite search "query" -c <channel>` |

**Project experts**: Each `<project>-dev` is the expert on their project. When stuck on a companion tool (rite, maw, seal, vessel, bn), post a question to its project channel instead of guessing.

#### Threads

Every message you send answers something or starts something. Anchor the answers.

- `--reply-to <id>` anchors a message under a parent. No `--reply-to` means top-level.
- When a hook spawned you, the message that woke you is `$RITE_MESSAGE_ID`. Answer it:
  `rite send --agent $AGENT "$RITE_CHANNEL" "on it" --reply-to "$RITE_MESSAGE_ID"`.
  A lease batch instead sets `$RITE_BATCH_MESSAGE_IDS`, in chronological order. The
  anchor is the LAST id in that list, not the first.
- An anchor that is not in the store yet gives a warning, not an error. The reply links
  up when the parent syncs in.
- `rite history --thread <id>` reads the whole thread from any message in it, and finds
  the channel itself. A thread reported `complete:false` is a fragment — say so, do not
  present it as the whole conversation.
- Your prompt carries the anchor for the current turn. Use that one. Never reuse the
  anchor from an earlier turn.

#### Ask and Wait

Never post a question and hope. Anchor it, then block on the answer:

```bash
id=$(rite send --agent $AGENT <channel> "<question> @<target>" -L feedback --format json | jq -r .id)
rite wait --agent $AGENT --reply-to "$id" -t 300 --format json
```

| Exit | Meaning | What to do |
|------|---------|------------|
| 0 | Answered | Read `.message.body` from the JSON and act on it |
| 1 | Nobody answered in time | Escalate: post one `-L task-blocked` naming the anchor, record it on the bone, move on. **Never re-send the request.** |
| 2 | Not a ULID, or this store never saw it | Fix the id (`rite history <channel> --from $AGENT -n 1 --format json` returns `last_id`). Do not re-send. Add `--allow-missing-parent` only when the parent is still syncing in from another machine. |

`--reply-to` narrows the wait, it never widens it. `--from`, `-c` and `-L` only subtract
candidate answers. With no `-c` every channel counts, so a reply in a DM satisfies the
wait. A reply that arrived before the wait started still counts. Your own reply never
satisfies your own wait.

### Cross-Project Communication

**Don't suffer in silence.** If a tool confuses you or behaves unexpectedly, post to its project channel.

1. Find the project: `rite history projects -n 50` (the #projects channel has project registry entries)
2. Ask and wait — capture the id, then block on the answer:
   ```bash
   id=$(rite send --agent $AGENT <project> "<question> @<project>-dev" -L feedback --format json | jq -r .id)
   rite wait --agent $AGENT --reply-to "$id" -t 300 --format json
   ```
3. For bugs, create bones in their repo first
4. **On exit 1 (no answer), create a local tracking bone** and move on. Record the anchor
   so the next check reads the thread instead of asking again:
   ```bash
   bn create --title "[tracking] <summary>" --tag tracking --kind task \
     --description "Asked #<project>: <question>. Anchor: <id>. Check: rite history --thread <id>"
   ```

See [cross-channel.md](.agents/edict/cross-channel.md) for the full workflow.

### Communication

Use ASD-STE100 Simplified Technical English for prose. Strict compliance is not the goal. Aim for terse, unambiguous language.

Do not apply STE to code, identifiers, commands, marketing copy, essays, or voice-driven writing.

#### Language

- Limit sentences to 20 words.
- Replace semicolons and contractions.
- Use active voice when the actor is known.
- Use plain verbs. Avoid nominalization, phrasal verbs, and "-ing" main verbs.
- Use one consistent name for each thing.

#### rite messages

Keep a channel message to one or two lines. Lead with the subject of the label. The label and the bone ID already carry the context, so do not add status blocks, numbered steps, or closing actions.

- `[task-claim] Working on <bone-id>: <title>`
- `[review-request] Dedicated security review requested: <review-id> for <bone-id>`
- `[task-blocked] Blocked on <thing>: <what unblocks it>`

Anchor an answer with `--reply-to` instead of quoting the message you answer. The anchor
carries the context.

#### Replies to a human

1. Start with a concrete action. Put commands, paths, or snippets first.
2. Number multistep tasks. Give each step one bounded action.
3. Limit lists to five items. Split longer lists by priority.
4. State the current step, what is complete, what remains, and what it waits on.
5. End with the next action, or state what you wait on.

Finish the current issue before you present another. State errors as evidence, cause, and fix.

Do not use preambles, recaps, pleasantries, tangents, emotional error language, or empty hedges.

Never state a time estimate you cannot support. You do not know how long a build, a test run, or another agent takes. Name what you wait on instead.

#### Exceptions

- Explain fully when the user asks for an explanation or a walkthrough.
- Confirm before destructive actions.
- After three failed fixes, state the uncertain assumption and ask one diagnostic question.
- Ask one short question when real ambiguity makes a guess risky.

Before you send, remove announcements, repeated summaries, sidebars, and empty closing questions.

The first line must give the action. The last line must give the result or the next action.

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

- [Launch one exact Daybreak security review](.agents/edict/security-review.md)

- [Merge a worker workspace (protocol merge + conflict recovery)](.agents/edict/merge-check.md)

- [Validate toolchain health](.agents/edict/preflight.md)

- [Ask questions, report bugs, and track responses across projects](.agents/edict/cross-channel.md)

- [Report bugs/features to other projects](.agents/edict/report-issue.md)

- [groom](.agents/edict/groom.md)

<!-- edict:managed-end -->
