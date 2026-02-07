# Eval Report: v2 Bare Repo Model

**Date**: 2026-02-07
**Epic**: bd-z68
**Beads tested**: bd-3t3 (workspace lifecycle), bd-3ez (br compatibility), bd-3vl (v1 vs v2)

## Test Scenarios

### S1: Agent Workspace Lifecycle (bd-3t3)

Create → work → describe → merge → verify files → destroy

| Step | Operation | Result | Friction |
|------|-----------|--------|----------|
| 1 | Orientation (ls, workspace list) | Clean layout, root metadata-only | 0 |
| 2 | `maw ws create test-agent-1` | **FAIL pre-fix**: `Workspace 'default' doesn't have a working-copy commit` | ~~3~~ → 0 |
| 3 | Make changes in workspace | Works fine | 0 |
| 4 | `maw ws jj <name> describe` | Works fine | 0 |
| 5 | `maw ws status` | Shows agent workspaces correctly | 0 |
| 6 | `jj log` from coord | Shows all workspace commits clearly | 0 |
| 7 | `maw status` | Shows agent count, no false positives | 1 |
| 8 | `maw ws merge --destroy` | Merge succeeds, workspace destroyed | 0 |
| 9 | Verify files in coord | **FAIL pre-fix**: coord stale, files not visible | ~~3~~ → 0 |
| 10 | Workspace list after merge | Clean, only coord remains | 0 |

**Pre-fix score**: 6/30 friction points (2 critical failures)
**Post-fix score**: 1/30 friction points (minor: status doesn't show per-workspace changes)

### S2: Two-Agent Merge with Conflict

Alice and Bob both modify `src/main.rs` differently.

| Step | Operation | Result | Friction |
|------|-----------|--------|----------|
| 1 | See state | Clean, 3 workspaces visible | 0 |
| 2 | Check diffs | Clear diffs for both agents | 0 |
| 3 | `maw ws merge alice bob` | Merge creates commit, but conflict not warned | 2 |
| 4 | Check coord for conflicts | Coord was stale (now fixed by update-stale) | ~~2~~ → 0 |
| 5 | Resolve conflict | Straightforward file edit | 0 |
| 6 | Verify resolution | Resolution in child commit, parent still conflicted | 1 |
| 7 | Destroy alice/bob | Clean destruction | 0 |
| 8 | Destroy coord (blocked) | Correctly prevented | 0 |

**Known issue**: Merge output says "Merged to main" even when result has conflicts. This is a pre-existing issue (not v2-specific) but more visible now. Filed as future improvement.

### S3: br Tool Compatibility (bd-3ez)

| Step | Operation | Result | Friction |
|------|-----------|--------|----------|
| 1 | `.beads/` at root | Exists (stale git remnant) | 1 |
| 2 | `.beads/` in coord | Same content (jj-tracked) | 0 |
| 3 | `br create` from root | Writes succeed but invisible to jj | **3** |
| 4 | `br create` from coord | Works correctly, jj tracks changes | 0 |
| 5 | `br list` from root | Shows only root-local beads | 2 |
| 6 | `br list` from coord | Shows workspace beads correctly | 0 |
| 7 | jj visibility | Root changes invisible, coord changes tracked | **3** |
| 8 | Divergence | Root and coord .beads/ drift apart | **3** |

**Root cause**: In v2, root `.beads/` is a stale remnant from before `core.bare=true` was set. Writes there are invisible to jj. Agents MUST run `br` from inside a workspace (e.g., `ws/coord/`).

**Recommendation**:
1. Document that br must be run from workspaces, not root
2. `maw init` / `maw upgrade` should delete root `.beads/` to prevent the trap
3. Future: br could detect bare repos and error with helpful message

## Critical Bugs Found and Fixed

### Bug 1: `maw ws create` fails in v2 bare model

**Symptom**: `Error: Workspace 'default' doesn't have a working-copy commit`
**Root cause**: `jj workspace add -r @` uses `@` which resolves to the default workspace's working copy. In v2, there is no default workspace.
**Fix**: Detect when `@` can't be resolved (no default workspace) and fall back to the configured branch name (e.g., `main`).

### Bug 2: Post-merge coord stale — files not visible

**Symptom**: After `maw ws merge`, coord workspace shows "stale" and merged files are not on disk.
**Root cause**: `jj rebase -r coord@ -d main` runs from repo root. This updates the commit graph but doesn't refresh coord's on-disk working copy because jj only updates the working copy when running from inside the workspace.
**Fix**: After the rebase, run `jj workspace update-stale` from inside the coord workspace directory, then auto-resolve any divergent commits created by the update.

## v1 vs v2 Comparison

| Aspect | v1 | v2 (post-fix) |
|--------|-----|---------------|
| Root directory | Source files + metadata | Metadata only (.git, .jj, ws/, .maw.toml) |
| Concurrent modification risk | **High** — any process can write to root | **Low** — only workspace owners modify their dirs |
| Workspace path length | `.workspaces/alice/src/main.rs` | `ws/alice/src/main.rs` |
| Post-merge file visibility | Works (root is default workspace) | Works (update-stale auto-runs in coord) |
| `maw ws create` from root | Works (@=default) | Works (falls back to branch) |
| Coordination | Implicit (default workspace) | Explicit (ws/coord/) |
| Botbox upgrade safety | **Unsafe** — can modify root files | **Safe** — root has no source files |
| `br` from root | Works (root is tracked) | **Trap** — writes invisible to jj |

## Verdict

v2 bare model is **ready for use** with the two critical bug fixes applied. The remaining issue (br from root) is a documentation/convention issue, not a code blocker.

The architecture achieves its primary goal: eliminating the shared mutable surface (repo root) that caused the 2026-02-05 lost commits incident. Each workspace is now fully isolated, and the coordination workspace is explicit and protected.
