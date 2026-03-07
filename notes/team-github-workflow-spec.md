# Team GitHub Workflow Spec for maw (Changes Model)

Date: 2026-03-05
Status: Draft (updated)
Owner: maw

## Problem

maw currently favors a single-integrator flow into `main`.
Team GitHub workflows need multiple long-running feature branches and PRs in flight at once.

We want:

1. explicit targets (no hidden "current change")
2. `ws/default` to stay on trunk (`main` or configured equivalent)
3. easy single-agent happy path
4. optional multi-agent fan-in per feature

## Core Model

Only two user-facing primitives:

- change: a tracked feature branch + PR lifecycle object
- workspace: an isolated working copy where code is edited

No task/session duality in CLI UX.
Ticket ids (Asana/Jira/Linear) are metadata on a change.

## Command Namespace

Use plural command group for consistency:

- `maw changes ...`

## Design Decisions

- `ws/default` remains trunk-oriented and is not used as implicit feature context.
- Every change operation is explicit by `<change-id>`.
- `maw ws merge` requires `--into` (explicit destination).
- Creating a change auto-creates a primary workspace and records it in metadata.
- Fetch from remote only when `--from` is remote-tracking (for example `origin/main`).

## CLI Spec

## `maw changes create`

Create a new change.

```bash
maw changes create "ASANA-123 improve cache invalidation" --from main
maw changes create "ASANA-123 improve cache invalidation" --from origin/main
```

Required:

- `--from <ws|branch|rev|remote/branch>`

Behavior:

1. resolve source ref from `--from`
2. if `--from` is `remote/branch`, fetch that remote branch first
3. create change id (for example `ch-1xr`)
4. create change branch (for example `feat/ch-1xr-improve-cache-invalidation`)
5. create primary workspace (default name: same as change id, e.g. `ch-1xr`)
6. persist metadata linking `change_id -> branch -> primary_workspace`

Output must include next commands:

```text
Change created: ch-1xr
  Branch: feat/ch-1xr-improve-cache-invalidation
  Primary workspace: /abs/path/ws/ch-1xr/
Next:
  maw exec ch-1xr -- git add -A && maw exec ch-1xr -- git commit -m "..."
  maw ws merge ch-1xr --into ch-1xr --destroy
  maw changes pr ch-1xr --draft
```

## `maw changes list`

```bash
maw changes list
maw changes list --format json
```

Shows id, title, branch, primary workspace, PR state.

## `maw changes show`

```bash
maw changes show ch-1xr
maw changes show ch-1xr --format json
```

Shows full metadata and linked workspaces.

## `maw changes pr`

Create or adopt PR for a change (idempotent by contract).

```bash
maw changes pr ch-1xr
maw changes pr ch-1xr --draft
maw changes pr ch-1xr --ready
maw changes pr ch-1xr --title "feat: ..." --body-file notes/pr.md
```

Idempotency contract:

1. check for existing PR by head/base
2. if open PR exists, adopt/update metadata and succeed
3. if no open PR exists, create PR
4. repeated runs are safe and non-destructive

## `maw changes sync`

Update a change branch with trunk movement.

```bash
maw changes sync ch-1xr
maw changes sync ch-1xr --rebase
```

Default mode: merge latest source branch into change branch.

`--rebase` mode:

- opt-in only
- may require force-with-lease push when branch already published
- command warns; no automatic force push

## `maw changes close`

Close out change metadata after PR merge (or explicit override).

```bash
maw changes close ch-1xr
maw changes close ch-1xr --delete-branch
maw changes close ch-1xr --delete-branch --remote
maw changes close ch-1xr --force
```

Checks:

- PR merged (unless `--force`)
- no unresolved linked workspace conditions

Then:

- archive change metadata
- optionally delete local and remote branch pointers

## Workspace Commands

## `maw ws create`

For additional workspaces beyond the primary one:

```bash
maw ws create a123-agent-b --change ch-1xr
maw ws create scratch --from main
```

Rule:

- require one of:
  - `--change <change-id>` (recommended, bound workspace)
  - `--from <ws|branch|rev|remote/branch>` (unbound or manual binding)

If `--from` is `remote/branch`, fetch first.

## `maw ws merge`

Explicit destination required.

```bash
maw ws merge ch-1xr --into ch-1xr --destroy
maw ws merge a123-agent-a a123-agent-b --into ch-1xr --destroy
maw ws merge hotfix-agent --into default
```

Rules:

- `--into` is required
- target can be:
  - workspace id (`default`, `staging`, ...)
  - change id (`ch-1xr`, resolves to that change branch)
- no implicit merge target

This prevents accidental trunk merges while multiple features are active.

## Metadata Schema

Location: `.manifold/changes/` at repo root.

```text
.manifold/changes/
  index.toml
  active/ch-1xr.toml
  archive/2026-03-05T19-20-11Z-ch-1xr.toml
```

`index.toml` tracks non-duplicated lookup data only:

```toml
schema_version = 1

[by_branch]
"feat/ch-1xr-improve-cache-invalidation" = "ch-1xr"

[by_workspace]
"ch-1xr" = "ch-1xr"
"a123-agent-b" = "ch-1xr"
```

`active/ch-1xr.toml`:

```toml
schema_version = 1
change_id = "ch-1xr"
title = "ASANA-123 improve cache invalidation"
state = "open" # open | review | merged | closed | aborted
created_at = "2026-03-05T19:20:11Z"

[source]
from = "origin/main"
from_oid = "<oid>"

[git]
base_branch = "main"
change_branch = "feat/ch-1xr-improve-cache-invalidation"

[workspaces]
primary = "ch-1xr"
linked = ["ch-1xr", "a123-agent-b"]

[tracker]
provider = "asana"
id = "ASANA-123"
url = "https://app.asana.com/..."

[pr]
number = 842
url = "https://github.com/org/repo/pull/842"
state = "open"
draft = true
```

## Day-to-Day Example

Open PR for ticket A, then start ticket B before A merges.

```bash
# Start change A from remote trunk and get auto primary workspace
maw changes create "ASANA-123 improve cache invalidation" --from origin/main

# Work + commit in primary workspace (assume id ch-1xr)
maw exec ch-1xr -- git add -A
maw exec ch-1xr -- git commit -m "feat: cache invalidation"

# Merge workspace into change branch and open PR
maw ws merge ch-1xr --into ch-1xr --destroy
maw changes pr ch-1xr --draft

# Before PR merges, start change B from latest origin/main
maw changes create "ASANA-456 improve onboarding copy" --from origin/main

# Work and open second PR (assume id ch-2ab)
maw exec ch-2ab -- git add -A
maw exec ch-2ab -- git commit -m "feat: onboarding copy refresh"
maw ws merge ch-2ab --into ch-2ab --destroy
maw changes pr ch-2ab --draft

# Later, sync old branch if main moved
maw changes sync ch-1xr

# After PR merge on GitHub, close metadata
maw changes close ch-1xr --delete-branch --remote
```

## Safety and Compatibility

- `maw ws merge` without `--into` is a hard error (explicit fix shown).
- `maw ws create` without `--change`/`--from`:
  - compatibility path may be temporarily supported with warning
  - target state is explicit requirement
- no committed work loss guarantees remain unchanged (`ws recover`, destroy/sync safety).

## Minimal Code Touchpoints

- `crates/maw-cli/src/changes.rs` (create/list/show/pr/sync/close)
- `crates/maw-cli/src/workspace/mod.rs`
  - require `--into` on merge
  - require `--change` or `--from` on workspace create
- `crates/maw-cli/src/workspace/merge.rs`
  - resolve `--into` target (`workspace` or `change id`)
- `crates/maw-cli/src/status.rs`
  - include open change count and brief PR status
- `crates/maw-cli/src/push.rs`
  - optional future: `maw changes push <id>` wrapper
- `crates/maw-cli/src/workspace/metadata.rs`
  - add `change_id: Option<String>` binding

## CLI Help Draft

Draft help text for implementation and UX review.

Top-level:

```text
$ maw changes --help
Manage tracked feature changes (branch + PR + linked workspaces)

Usage:
  maw changes <command> [options]

Commands:
  create    Create a change from an explicit source and create primary workspace
  list      List active changes
  show      Show detailed change metadata
  pr        Create/adopt/update GitHub PR for a change (idempotent)
  sync      Update a change branch from its source branch
  close     Close/archive a change after PR merge

Examples:
  maw changes create "ASANA-123 improve cache invalidation" --from origin/main
  maw changes pr ch-1xr --draft
  maw changes sync ch-1xr
  maw changes close ch-1xr --delete-branch --remote
```

Create:

```text
$ maw changes create --help
Create a tracked change from an explicit source.

Usage:
  maw changes create <title> --from <ws|branch|rev|remote/branch> [options]

Arguments:
  <title>                 Human title (used for metadata and branch slug)

Required:
  --from <ref>            Source workspace, branch, revision, or remote branch

Options:
  --id <change-id>        Optional explicit change id (default: generated, e.g. ch-1xr)
  --workspace <name>      Primary workspace name (default: same as change id)
  --tracker <provider:id> Tracker reference (example: asana:ASANA-123)
  --tracker-url <url>     Tracker URL
  --format <text|json>    Output format

Behavior:
  - If --from is remote-tracking (example: origin/main), maw fetches that ref first.
  - Creates change metadata, change branch, and primary workspace.
```

List/show:

```text
$ maw changes list --help
List active changes.

Usage:
  maw changes list [--format <text|json>]

$ maw changes show --help
Show details for one change.

Usage:
  maw changes show <change-id> [--format <text|json>]
```

PR:

```text
$ maw changes pr --help
Create or update PR for a change (idempotent).

Usage:
  maw changes pr <change-id> [options]

Options:
  --draft                 Mark PR as draft
  --ready                 Mark PR as ready for review
  --title <text>          Override PR title
  --body-file <path>      Read PR body from file
  --base <branch>         Override base branch (default: change base)
  --format <text|json>    Output format

Idempotency:
  - If an open PR already exists for head/base, maw adopts it and updates metadata.
  - Safe to run repeatedly.
```

Sync:

```text
$ maw changes sync --help
Sync a change branch with upstream movement.

Usage:
  maw changes sync <change-id> [options]

Options:
  --rebase                Rebase instead of merge (may require force-with-lease push)
  --format <text|json>    Output format

Default:
  merge latest source branch into change branch.
```

Close:

```text
$ maw changes close --help
Close and archive a change.

Usage:
  maw changes close <change-id> [options]

Options:
  --delete-branch         Delete local change branch
  --remote                Also delete remote branch (requires --delete-branch)
  --force                 Close even if PR is not merged
  --format <text|json>    Output format
```

Workspace create updates:

```text
$ maw ws create --help
Create a workspace.

Usage:
  maw ws create <name> (--change <change-id> | --from <ws|branch|rev|remote/branch>)

Options:
  --change <change-id>    Bind workspace to a change (recommended)
  --from <ref>            Explicit source for workspace creation
```

Workspace merge updates:

```text
$ maw ws merge --help
Merge one or more workspaces into an explicit target.

Usage:
  maw ws merge <workspace>... --into <workspace|change-id> [options]

Required:
  --into <target>         Merge destination (workspace name or change id)

Options:
  --destroy               Destroy merged workspaces after successful merge
  --check                 Validate merge only; do not commit
  --message <msg>         Commit message override
  --format <text|json>    Output format
```

## Open Questions

1. Should `maw changes close` also destroy archived primary workspace by default, or keep that separate?
2. Should `maw changes pr` support `--reopen` when only closed PR exists for the same branch?
3. Keep `maw change ...` as alias, or standardize only on `maw changes ...`?
