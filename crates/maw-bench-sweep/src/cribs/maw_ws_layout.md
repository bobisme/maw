# maw arm crib (current v2 `ws/` layout)

You coordinate multi-agent work using **maw**, a workspace-management tool built on git. The repo is laid out as a bare-style root containing `.git/` and a `ws/` directory; every workspace is an isolated git worktree under `ws/<name>/`. The default integration target lives at `ws/default/`. The `maw` binary is on `$PATH`.

## Core verbs

- `maw ws create <name>` — make a new workspace (worktree) at `ws/<name>/`.
- `maw ws list` — list all workspaces with their state (active / stale / conflicted).
- `maw ws sync` — bring the current workspace up to the latest epoch (rebases workspace commits onto integration head). Use when you see `stale` in `maw ws list`.
- `maw ws diff <name>` — show the workspace's changes vs the epoch base.
- `maw ws merge <a> [b ...] --into default --check` — dry-run a merge; reports conflicts without changing state.
- `maw ws merge <a> [b ...] --into default --destroy` — merge workspace(s) into default and destroy the source(s) on success.
- `maw ws destroy <name>` — drop the workspace (Prime Invariant: a recovery snapshot is captured automatically; with `--force` even on unmerged work).
- `maw ws recover` — list / restore destroyed workspaces from their recovery snapshots.
- `maw ws resolve <name> --list` / `--keep epoch|<name>|both` — resolve conflict markers left by `maw ws sync`.
- `maw doctor` — substrate-health probe.

## Running commands inside a workspace

The sandbox does not persist `cd` between tool calls. Use `maw exec <workspace> -- <cmd>` to run any command inside a workspace:

```
maw exec alice -- git status
maw exec alice -- git add -A
maw exec alice -- git commit -m "feat: …"
```

## Conflicts are data, not errors

- `maw ws sync` does not abort on conflict — it commits the marker-laden file, records structured conflict metadata under `.manifold/`, and continues. The workspace ends in a "conflicted-but-synced" state visible in `maw ws status`. Resolve via `maw ws resolve`.
- `maw ws merge` refuses only one thing: a source workspace whose HEAD still contains unresolved textual conflict markers. Pass `--force` only when markers are legitimate content (test fixtures, docs).

## Prime Invariant

No committed work is ever lost. `maw ws destroy` always captures a recovery snapshot under `refs/manifold/recovery/<name>/`; `maw ws recover` can list, inspect, search, and restore the contents of any destroyed workspace. If you suspect work was lost, run `maw ws recover` before reopening the task.

## What to read first

- This crib (always).
- `maw --help` and `maw ws --help` for the full surface.
- `maw doctor` if anything looks broken before you start.
