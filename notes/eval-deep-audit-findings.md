# Deep Audit Findings: maw v0.30.2

**Bead**: bd-1zyq
**Date**: 2026-02-10
**Scope**: Full code audit + documentation audit + git interop audit + agent simulation

---

## Critical

### C1. `cd` instructions in runtime error messages
**Files**: `src/workspace.rs:522`, `src/workspace.rs:699`

Two error messages tell agents to `cd`, which doesn't persist in sandboxed environments:

**workspace.rs:519-526** — `ensure_repo_root()` error:
```rust
bail!(
    "This command must be run from the repo root.\n\
     \n  You are in: {}\n  Repo root:  {}\n\
     \n  Run: cd {} && maw ...",
    cwd.display(), root.display(), root.display()
);
```

**workspace.rs:697-702** — `create()` jj new failure:
```rust
bail!(
    "Failed to create dedicated commit for workspace: {}\n  ...\n  Try: cd {} && jj new -m \"wip: {name}\"",
    stderr.trim(), path.display()
);
```

**Impact**: Agents in sandboxed environments cannot follow these instructions. The first error is particularly problematic because maw should handle the cwd internally rather than require agents to `cd`.

### C2. `jj-intro` shows manual push workflow, not `maw push`/`maw release`
**File**: `src/jj_intro.rs:49-105`

The embedded jj introduction (shown via `maw jj-intro`) documents a 4-step manual push workflow:
```
jj rebase -r @- -d main
jj bookmark set main -r @-
jj git push
```
This is exactly the workflow `maw push` was created to replace. The same text is also injected by `maw agents init` into AGENTS.md files of downstream projects (see `src/agents.rs`).

**Impact**: This is agents' FIRST contact with pushing. They learn the manual workflow and never discover `maw push` or `maw release`.

### C3. `maw ws jj` still prescribed in Output Guidelines
**File**: `AGENTS.md:349,357`

The Output Guidelines section — which governs how all maw output should work — still says:
- Line 349: Example output shows `maw ws jj agent-a describe -m "feat: ..."`
- Line 357: `"Never instruct agents to cd into a workspace — use maw ws jj <name> for jj commands"`

This is self-contradictory: the guidelines prescribe the deprecated command to future implementers. The code itself correctly uses `maw exec` in all new output.

**Impact**: Any developer following these guidelines would add deprecated `maw ws jj` references to new code.

### C4. Stale local git refs risk in `push.rs` tag pushing
**File**: `src/push.rs:252`

`push_tags()` runs `git push origin --tags` from the repo root. While the branch push correctly uses `jj git push` (which updates the remote), the tag push uses raw git. Since `jj git export` may not reliably export all tags in bare repos (known issue noted in the bead), tags created via `jj tag set` may not be present in git's ref store.

The code does run `jj git export` first (line 240-249), but treats failures as non-fatal warnings. If the export silently fails to export a specific tag, `git push --tags` would push stale or missing tags.

**Impact**: `maw push` (without `--no-tags`) may silently fail to push newly created jj tags.

---

## High

### H1. AGENTS.md Quick Start tells agents to `cd`
**File**: `AGENTS.md:18,28`

```markdown
cd ws/<your-name>     # line 18
cd /path/to/repo/root # line 28
```

This is the first thing agents see. In sandboxed environments, these commands are no-ops, so agents start from a broken mental model.

### H2. `maw ws jj` still in Workspace Commands table
**File**: `AGENTS.md:62,80`

The command reference table still includes:
```
| Run jj in workspace | `maw ws jj <name> <args>` |
```
And line 80: "For jj specifically, `maw ws jj <name> <args>` also works and includes jj-specific safety warnings."

While `maw ws jj` technically still works (as a hidden command that bails with a deprecation message), including it in the primary command table misleads agents.

### H3. Release process still references manual tag/push
**Files**: `AGENTS.md:210-211,242,333`, `.agents/botbox/finish.md:44`, `.agents/botbox/worker-loop.md:159`

Multiple docs still say:
```
jj tag set vX.Y.Z -r main
git push origin vX.Y.Z
```

Should be: `maw release vX.Y.Z` (added in v0.30.0)

### H4. Missing release notes for v0.28.0 through v0.30.2
**File**: `AGENTS.md` Release Notes section

Eight versions of release notes are missing:
- v0.30.2, v0.30.1, v0.30.0 (includes `maw release` command)
- v0.29.3, v0.29.2, v0.29.1, v0.29.0 (format overhaul)
- v0.28.6, v0.28.5 (bare root fixes)

### H5. `jj-intro` embedded in `agents.rs` pushes manual workflow to ALL downstream
**File**: `src/agents.rs:35-142`

The `maw agents init` command injects jj instructions into external projects' AGENTS.md. These instructions include the manual push workflow (bookmark set, git push) rather than `maw push`. Every project that runs `maw agents init` gets outdated instructions.

### H6. `create()` clap docstring contains `cd` instruction
**File**: `src/workspace.rs:234`

```rust
///   3. Run other commands: cd /abs/path/ws/<name> && cmd
```

This is visible in `maw ws create --help`. Should say: `maw exec <name> -- <cmd>`

### H7. Default workspace safety not checked before merge rebase
**File**: `src/workspace.rs:2537-2578`

During merge, the default workspace is rebased onto the new branch and then `jj restore` is run:
```rust
let _ = Command::new("jj")
    .args(["restore"])
    .current_dir(&default_ws_path)
    .output();
```

There is no check for whether the default workspace has non-empty diffs before restoring. If an agent had uncommitted work in the default workspace, `jj restore` would silently overwrite it. The bead specifically calls this out: "Before rebasing/restoring the default workspace during merge, check for non-empty diffs."

---

## Medium

### M1. `workspace.rs:643` uses relative path in creation output
**File**: `src/workspace.rs:643`

```rust
println!("Creating workspace '{name}' at ws/{name} ...");
```

Uses relative path `ws/{name}` while the success message at line 717 correctly uses `path.display()` (absolute). Inconsistent.

### M2. `README.md` still shows `maw ws jj` in command table
**File**: `README.md:40`

The README command table includes `maw ws jj <name> <args>` as a listed command.

### M3. `jj-intro` doesn't mention `maw exec` as primary workflow
**File**: `src/jj_intro.rs:107-116`

The maw-specific section at the end mentions `maw exec` correctly, but the bulk of the document (the "How to Push to GitHub" section) uses raw jj commands without `maw exec` wrappers, suggesting agents should run them directly.

### M4. `auto_resolve_conflicts` runs `jj status` in wrong context
**File**: `src/workspace.rs:2092`

After merge, the warning says:
```
println!("Run `jj status` to see conflicted files.");
```

Should say: `maw exec default -- jj status` (or use absolute path context). An agent following this instruction literally would run `jj status` from whatever directory they happen to be in.

### M5. Divergent commit fix instructions use raw jj
**File**: `src/workspace.rs:1148,1232,1612-1622`

Status output says `Fix: jj abandon <change-id>/0` without wrapping in `maw exec`. Agents in sandboxed environments need the full command.

### M6. Bookmark advancement happens before conflict check in merge
**File**: `src/workspace.rs:2494-2506`

The merge function advances the branch bookmark (step 3, line 2497) before checking for conflicts (line 2581). If the merge has conflicts, the bookmark is already pointing at a conflicted commit. The `auto_resolve_conflicts` function may resolve them, but if it can't, the branch bookmark is left pointing at a bad commit.

Sequence:
1. Rebase workspace commits onto main ✓
2. Squash if multi-workspace ✓
3. **Move main bookmark** ← happens here
4. Abandon scaffolding commits
5. Rebase default workspace
6. **Check for conflicts** ← too late

---

## Low

### L1. `notes/ideas.md:333` references `maw ws jj`
**File**: `notes/ideas.md:333`

Internal notes still reference the deprecated command.

### L2. `src/workspace.rs:234` docstring references `cd`
**File**: `src/workspace.rs:234`

Already captured in H6 but specifically the clap `after_help` style text.

### L3. Inconsistent emoji usage in sync_all
**File**: `src/workspace.rs:1720-1722`

Uses `✓` emoji in sync_all output but the rest of the codebase avoids emojis in text format. Minor inconsistency.

---

## Before/After Examples

### `maw ws create` — Current vs Proposed

**Current output** (line 697-702 on failure):
```
Failed to create dedicated commit for workspace: <stderr>
  The workspace was created but has no dedicated commit.
  Try: cd /home/bob/src/maw/ws/agent-1 && jj new -m "wip: agent-1"
```

**Proposed output**:
```
Failed to create dedicated commit for workspace: <stderr>
  The workspace was created but has no dedicated commit.
  Try: maw exec agent-1 -- jj new -m "wip: agent-1"
```

### `ensure_repo_root()` — Current vs Proposed

**Current output** (line 519-526):
```
This command must be run from the repo root.

  You are in: /home/bob/src/maw/ws/default
  Repo root:  /home/bob/src/maw

  Run: cd /home/bob/src/maw && maw ...
```

**Proposed output**: This error should ideally not exist — maw should resolve its own repo root. But if it must exist:
```
This command must be run from the repo root.

  You are in: /home/bob/src/maw/ws/default
  Repo root:  /home/bob/src/maw

  Run from the repo root: maw ...
  (maw commands work from any directory within the repo)
```

### `maw ws merge` — Conflict warning current vs proposed

**Current** (line 2092):
```
WARNING: Merge has conflicts that need resolution.
Run `jj status` to see conflicted files.
```

**Proposed**:
```
WARNING: Merge has conflicts that need resolution.
  See conflicted files: maw exec default -- jj status
  Resolve: edit files in /home/bob/src/maw/ws/default/, then:
    maw exec default -- jj describe -m "resolve: ..."
```

### `maw ws status` — Divergent fix current vs proposed

**Current** (line 1148):
```
Fix: jj abandon <change-id>/0
```

**Proposed**:
```
Fix: maw exec <workspace> -- jj abandon <change-id>/0
  (abandons one divergent copy, keeping the other)
```

---

## Git Interop Audit Summary

### Commands using `Command::new("git")`

| File | Line | Command | Uses | Assessment |
|------|------|---------|------|------------|
| release.rs | 111 | `git tag <tag> <commit_hash>` | jj-resolved hash | **CORRECT** |
| release.rs | 128 | `git push origin <tag>` | Tag name | **CORRECT** |
| push.rs | 252 | `git push origin --tags --porcelain` | No branch ref | **CORRECT** (but see C4) |
| init.rs | 266,278 | `git config core.bare` | Config only | **CORRECT** |
| init.rs | 312,324 | `git symbolic-ref HEAD` | HEAD management | **CORRECT** |
| upgrade.rs | 270,282 | `git config core.bare` | Config only | **CORRECT** |
| doctor.rs | 338 | `git symbolic-ref HEAD` | Query only | **CORRECT** |
| status.rs | 627 | `git status --porcelain=1` | Status only | **CORRECT** |

**Key finding**: release.rs correctly uses jj-resolved commit hashes (not branch names) for git tag operations. The comment at line 62-64 explicitly documents why:
```rust
// IMPORTANT: We resolve from jj, not git, because the local git ref
// for the branch may be stale (jj git push updates the remote but
// doesn't always export to the local git ref in bare repos).
```

### Merge Workflow Trace

The merge function (`workspace.rs:2343-2633`) executes:

1. `run_hooks(pre_merge)` — hooks can abort
2. `sync_stale_workspaces_for_merge()` — good: prevents spurious conflicts
3. `jj rebase -r <ws>@ -d <branch>` — rebase workspace commits onto main
4. `jj squash --from <others> --into <first>` — squash multi-workspace (if >1)
5. **`jj bookmark set <branch> -r <first>@`** — move bookmark ⚠️ before conflict check
6. Abandon scaffolding commits (scoped to pre-rebase parent IDs — safe)
7. Update-stale default workspace
8. `jj rebase -r default@ -d <branch>` — rebase default onto new main
9. Resolve divergent working copies
10. `jj restore` — write parent's files to disk ⚠️ no dirty-check
11. `auto_resolve_conflicts()` — check/resolve conflicts ⚠️ happens after bookmark move

**Revset for abandon** (line 2516-2518):
```rust
"({id_terms}) & empty() & description(exact:'') & ~ancestors({branch}) & ~root()"
```

This is now correctly scoped: it only targets specific pre-rebase parent commit IDs (`id_terms`), not all empty commits. The `~ancestors(branch)` exclusion prevents abandoning commits that became part of the main line. This was improved from the original concern in the bead.

### Agent Simulation Findings

**S1 (Fresh start)**: `maw ws create myws` → output is excellent. Shows absolute path, change-id, `maw exec` commands. Only issue: creation message uses relative path.

**S2 (Post-merge)**: Files are visible in default workspace after merge (the `jj restore` handles this). Stale warnings are handled by auto-sync.

**S3 (Concurrent work, conflicts)**: Merge correctly syncs stale workspaces first. Conflicts are recorded. However, bookmark is advanced before conflict detection.

**S4 (Release)**: `maw release` works correctly, using jj-resolved commit hashes. But AGENTS.md and jj-intro still document the manual workflow.

**S5 (Recovery)**: `maw ws sync` handles stale + divergent well. Auto-resolution is sophisticated. Instructions in status output use raw `jj` commands without `maw exec` wrapper.
