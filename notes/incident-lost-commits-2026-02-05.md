# Incident Report: Uncommitted Fixes Lost to Merge Conflict

**Date**: 2026-02-05 (discovered 2026-02-07)
**Impact**: 4 bead fixes (bd-1e8, bd-1zd, bd-23u, bd-35o) lost from source tree. Binary had the fixes but source did not.
**Recovery**: Changes recovered from hidden jj snapshot `rsslvxop/4` (commit 07eca0b1).

## What Happened

### Timeline

1. **16:21** — v0.25.0 released. Default workspace on empty commit `rsslvxop` (child of v0.25.0).

2. **~21:00–21:36** — Human-directed session worked on 4 beads in the default workspace, editing `src/workspace.rs`:
   - bd-1e8: Workspace name in stale warnings
   - bd-1zd: `resolve_divergent_working_copy()` function
   - bd-23u: Cascade staleness warning in `sync_all()`
   - bd-35o: `auto_sync_if_stale()` replacing `warn_if_stale()`

   Binary was built and installed with `cargo install --path .`. Eval framework validated the fixes (6 agent runs, all successful). Changes were **never committed** — they existed only in the jj working copy.

3. **~21:53** — Session ran out of context. Changes still uncommitted in the working copy of commit `rsslvxop`.

4. **21:53** — Botbox auto-upgrade process ran in the default workspace. It:
   - Edited `.agents/botbox/` files
   - Ran `jj describe -m "chore: upgrade botbox..."` which snapshotted the working copy (capturing both our workspace.rs changes AND the botbox files) into `rsslvxop`
   - Ran `jj new` to create the next empty commit (`xvlqrnms`)

   At this point, `rsslvxop` contained both our workspace.rs changes and botbox upgrade files, described as a botbox upgrade commit.

5. **~22:04–22:09** — maw-dev bot (a separate agent) worked on bd-3mr ("Add implicit rebase to maw ws merge") in workspace `ember-reef`. This also modified `src/workspace.rs`, adding `sync_stale_workspaces_for_merge()`.

6. **22:11** — bd-3mr was merged: `jj rebase -r ember-reef@ -d main`. This placed `qxsrrzuy` (bd-3mr) directly on top of `wxkkptwo` (v0.25.0), **bypassing** `rsslvxop` and `xvlqrnms` which were on a parallel branch.

7. **22:11** — Default workspace was rebased onto new main: `jj rebase -r @ -d main`. This produced a **CONFLICT** in `workspace.rs` because:
   - Our branch (rsslvxop/xvlqrnms) had ~300 lines of new code (auto_sync, resolve_divergent, etc.)
   - bd-3mr also modified workspace.rs (added sync_stale_workspaces_for_merge)

   Result: `vyptyltq 962d8372 (conflict)`

8. **22:12** — The next botbox auto-upgrade ran in the conflicted default workspace. It resolved the conflict by effectively **dropping our changes** — the snapshot `vyptyltq/2` (d7378554) no longer contains `auto_sync_if_stale` or `resolve_divergent_working_copy`. Only bd-3mr's `sync_stale_workspaces_for_merge` survived (with its `resolve_divergent_working_copy()` call replaced by a comment, since the function was gone).

### Commit Topology

```
wxkkptwo (v0.25.0)
├── rsslvxop (botbox upgrade — CONTAINS our fixes in hidden snapshots)
│   └── xvlqrnms (botbox upgrade — also contains our fixes)
│       └── (dead branch, not on main)
└── qxsrrzuy (bd-3mr — merged directly onto v0.25.0)
    └── vyptyltq (botbox upgrade — conflict resolved, our fixes DROPPED)
        └── xrtyvvvr (botbox upgrade — explicitly removed resolve_divergent call)
            └── ... → kuyowuow (current working copy)
```

## Root Causes

### 1. Uncommitted work in shared workspace
The fixes were in the working copy but never committed. jj's auto-snapshot captured them into a commit, but that commit was also used for botbox upgrade changes, making it impossible to distinguish our intentional code changes from the automated upgrade.

### 2. Automated conflict resolution by unaware agent
The botbox auto-upgrade process resolved a merge conflict in `src/workspace.rs` without understanding the code. It effectively chose one side (bd-3mr's version) and dropped the other (our 4 bead fixes). The botbox upgrade agent has no capability to resolve code conflicts intelligently.

### 3. No protection against concurrent workspace modification
The default workspace was being used for both human-directed development (our session) and automated processes (botbox upgrades, bd-3mr merge). When bd-3mr was merged into main, our uncommitted work was rebased, producing a conflict that no one noticed.

### 4. jj snapshots mix intentional and incidental changes
When the botbox upgrade ran `jj describe`, it snapshotted our workspace.rs changes into what became a "chore: upgrade botbox" commit. The commit description gave no indication that it also contained significant code changes.

## Recovery

Changes were recovered from hidden jj snapshot `rsslvxop/4` (commit 07eca0b1, timestamped 2026-02-05 21:26). This snapshot contains the full workspace.rs with all 4 fixes. The changes were manually re-applied to the current codebase (which includes bd-3mr's sync_stale_workspaces_for_merge).

Key jj commands used for forensics:
- `jj evolog -r <change>` — shows all historical versions of a commit
- `jj file show <path> -r '<change>/N'` — reads files from hidden snapshots
- `jj op show <op-id>` — shows what each operation changed

## Future Fixes Needed (maw)

### 1. `maw ws save` / auto-commit before session end
Add a command (or hook) that commits current working copy state as a WIP commit before a session ends. This prevents the "uncommitted changes in shared workspace" failure mode.

Possible implementation:
- `maw ws save` — describes current commit as "wip: save point" if it has changes
- Pre-session-end hook that auto-saves
- Or: detect uncommitted changes in `maw status` and warn

### 2. Conflict detection in `maw ws sync` / `maw ws merge`
When `jj rebase` produces a conflict, maw should:
- Clearly warn that a conflict exists
- Show which files are conflicted
- Refuse to proceed with automated operations until resolved

Currently, conflicts are silently recorded in the commit and discovered later (or not at all, as in this case).

### 3. Protect default workspace from automated writes during active sessions
Consider a lockfile or session marker that prevents botbox upgrades from running in the default workspace while a development session is active. Alternatively, botbox upgrades should run in their own workspace.

### 4. `maw ws merge` should not produce conflicts silently
When merging a workspace that modifies files also modified in the default workspace's working copy, maw should detect and warn before the merge.

## Suggestions for Botbox Agent Instructions

### 1. Never resolve code conflicts automatically
Add to botbox agent instructions:
```
If `jj status` shows conflicts in source code files (*.rs, *.py, *.js, etc.),
DO NOT attempt to resolve them. Leave the conflict markers and report the issue.
Only resolve conflicts in data files (.beads/issues.jsonl, .agents/botbox/, etc.)
that you understand the format of.
```

### 2. Botbox upgrades should use a dedicated workspace
Instead of running in the default workspace, botbox upgrades should:
```bash
maw ws create botbox-upgrade
# ... apply upgrades in workspace ...
maw ws merge botbox-upgrade --destroy
```
This prevents botbox upgrade snapshots from capturing unrelated working copy changes.

### 3. Check for uncommitted code changes before upgrade
Add to the botbox upgrade script:
```bash
# Before starting upgrade, check if there are uncommitted code changes
code_changes=$(jj diff --stat -- '!.agents/**' '!.beads/**' '!.botbox.json')
if [ -n "$code_changes" ]; then
    echo "WARNING: Uncommitted code changes detected. Skipping upgrade."
    exit 0
fi
```

### 4. Commit before rebasing onto new main
When `maw ws merge` rebases the default workspace, it should first ensure the working copy has no uncommitted changes, or at minimum snapshot them into a clearly-labeled WIP commit before the rebase.

## Lessons Learned

1. **Always commit before ending a session.** jj's auto-snapshot is not a substitute for intentional commits. Uncommitted work can be lost to merge conflicts, rebases, or automated processes.

2. **jj's hidden snapshots are recoverable.** Even after changes are overwritten, `jj evolog` can find previous versions. The forensic capability is excellent — the data was never truly lost, just hard to find.

3. **Shared default workspace is a risk.** Multiple agents (human sessions, botbox upgrades, maw-dev bots) all operate on the default workspace. This creates race conditions. Each automated process should use its own workspace.

4. **The irony is thick.** Our changes to fix divergent commit resolution were themselves lost to a merge conflict — a related but distinct failure mode. The very tool improvements we were building would not have prevented this specific incident, but they share the same root cause: jj's working copy model is complex and agents need more guardrails.
