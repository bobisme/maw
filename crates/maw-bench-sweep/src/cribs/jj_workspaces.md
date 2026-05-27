# jj-workspaces arm crib

You coordinate multi-agent work using **jj** (Jujutsu), a git-compatible VCS that organizes work via change-ids and op-log. The repo is a colocated jj+git repo at the substrate root. Workspaces are jj's native multi-worktree feature (`jj workspace`). `jj` and `git` are both on `$PATH`.

## Core verbs

- `jj workspace add <path> --name <name>` — make a new jj workspace at `<path>` named `<name>`.
- `jj workspace list` — list all workspaces with their working-copy change-ids.
- `jj workspace update-stale` — refresh a workspace whose working copy is stale (someone else moved the underlying commit).
- `jj log -r 'divergent()'` — show diverged changes (one change-id with multiple commits). **Resolution policy**: rebase the older divergent commit onto the newer with `jj rebase -s <older> -d <newer>` then `jj abandon <older>` to drop the duplicate.
- `jj op log` — inspect the operation log (one op per state mutation).
- `jj op restore <op-id>` — roll the whole repo back to a previous op.
- `jj op integrate <op-id>` — **avoid this** when the op was a background workspace operation; integrating an unrelated workspace's op can produce divergent changes you did not intend. Only `op integrate` when you explicitly want to fold a background mutation into your own op chain.
- `jj diff` — show the working-copy diff vs the parent change.
- `jj new` / `jj commit -m "..."` — create a new change / set the description.
- `jj git push --branch <name>` / `jj git fetch` — sync with the colocated git remote.
- `jj squash` / `jj rebase -d <change>` — recombine changes.

## Op-log inspection (when something is off)

- `jj op log -n 20` shows the last 20 operations, who/what made each.
- `jj op show <op-id>` shows the diff between two op states.
- If you see a "concurrent operations" warning, examine `jj op log` to see what happened; pick the right ancestor with `jj op restore <op-id>` instead of blindly continuing.

## Conflicts

jj's model: conflicts are a first-class state on a change. A conflicted change shows as `??` in `jj log`. Resolve by editing the conflict markers in the working copy, then `jj squash` or commit normally — jj records the resolution and the change becomes non-conflicted.

## What to read first

- This crib (always).
- `jj help workspace` / `jj help op`.
- Run `jj log -r 'divergent()'` and `jj op log -n 5` if anything looks off before acting.
