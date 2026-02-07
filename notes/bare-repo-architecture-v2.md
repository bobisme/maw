# maw Architecture v2: Bare Repo Model

**Bead**: bd-z68
**Date**: 2026-02-07
**Status**: Proposal

## Background

On 2026-02-05, four bead fixes were lost when a merge conflict in the default workspace was silently resolved by an automated botbox upgrade process (see `notes/incident-lost-commits-2026-02-05.md`). The root cause: multiple processes (human session, botbox upgrade, agent merge) all operated on the **default workspace** simultaneously.

The default workspace — the repo root — is a shared mutable surface. Any process running in the repo root directory can modify files that jj snapshots into the default workspace's working copy. This is the fundamental design flaw.

## What I Learned (Investigation)

### jj workspace mechanics

I set up test repos in `/tmp` to explore whether jj supports a "bare" model (no default workspace):

1. **`jj workspace forget default`** — removes the default workspace. After this, `jj status` from the repo root says "No working copy". No jj process will snapshot root files.

2. **Colocated repos still have git files at root** — git maintains its working tree regardless of jj workspaces. Files from `HEAD` remain on disk after forgetting default.

3. **`git config core.bare true`** — tells git "this is a bare repo, no working tree". After setting this, root source files can be safely deleted. Git stops expecting them.

4. **All jj operations work from non-default workspaces:**
   - `jj describe`, `jj commit`, `jj diff` — normal workflow
   - `jj bookmark set main -r <rev>` — bookmark management
   - `jj git push --bookmark main` — push works (must use `--bookmark`, not default revset)
   - `jj git fetch` — fetch works normally
   - `jj rebase` — works from any workspace
   - `git tag v1.0 <hash>` + `git push origin v1.0` — tags work with `core.bare=true`

5. **Default push revset doesn't work** — jj's default push revset is `remote_bookmarks(remote=origin)..@`, which means "bookmarks between origin and my working copy". In a coord workspace, `@` is the coord commit, not main. Must use `jj git push --bookmark main` explicitly.

6. **Workspace names and bookmark names don't conflict** — a workspace named "main" and a bookmark named "main" coexist in separate namespaces.

### Current maw architecture (v1)

Key code paths that assume a default workspace:

- **`workspaces_dir()`** → `repo_root()?.join(".workspaces")` — hardcoded path
- **`sync_all()`** — special-cases `if ws == "default"` to use repo root instead of `.workspaces/default`
- **`jj_in_workspace()`** — maps "default" to repo root
- **`destroy()`** — explicitly prevents destroying default workspace
- **`merge()`** — post-merge rebases default workspace onto branch so root files reflect the merge
- **`push.rs`** — uses `jj git push` (relies on default push revset `..@`)

## Proposal: Architecture v2

### Directory Layout

```
my-project/
  .git/              # colocated git (core.bare=true)
  .jj/               # jj backing store
  .gitignore         # includes "ws/"
  .maw.toml          # maw config
  ws/                # ALL workspaces live here
    coord/           # persistent coordination workspace
      src/
      Cargo.toml
      ...
    alice/           # agent workspace
      src/
      ...
    bob/             # agent workspace
      src/
      ...
```

The repo root contains ONLY metadata: `.git/`, `.jj/`, `.gitignore`, `.maw.toml`, and the `ws/` directory. No source files. No default workspace.

### `ws/` instead of `.workspaces/`

- **Visible**: agents and humans see it in `ls` — the workspace-centric structure is obvious
- **Short**: `ws/coord/src/main.rs` vs `.workspaces/coord/src/main.rs`
- **Not hidden**: hidden dirs suggest "don't touch". `ws/` says "this is where work happens"
- **Gitignored**: `ws/` entry in `.gitignore` (same as `.workspaces/` today)

### The coord workspace

A **persistent workspace** that serves as the coordination point:

- **Merge**: `maw ws merge alice bob` runs from coord, merges agent work
- **Push**: `maw push` runs from coord, pushes main bookmark
- **Inspect**: human can `cd ws/coord/` to see the current state of main
- **Not destroyed**: coord survives `--destroy` on merge

The coord workspace replaces the role the default workspace played, but with a key difference: it's explicitly managed and isolated. No other process (botbox, CI, etc.) writes to it accidentally because it's not the repo root.

### `maw init` (new repos)

```bash
maw init
```

1. `jj git init --colocate` (or detect existing git repo)
2. `jj workspace forget default`
3. `git config core.bare true`
4. Remove root source files (stale git working tree remnants)
5. Create `ws/` directory
6. `jj workspace add ws/coord`
7. Rebase coord workspace onto main branch
8. Write `.gitignore` with `ws/` entry
9. Write `.maw.toml` with default config

Result: clean repo root, coord workspace ready, agents can `maw ws create <name>` to get started.

### `maw push` changes

Current: relies on default push revset (`..@` from repo root)
New: always uses `--bookmark` explicitly

```rust
// Before
Command::new("jj").args(["git", "push"])

// After
Command::new("jj").args(["git", "push", "--bookmark", &branch])
```

Push can run from any workspace. The `--advance` flag moves the bookmark to `@-` of the coord workspace (or specified workspace).

### `maw ws merge` changes

Current: runs from repo root (default workspace), rebases default onto branch post-merge
New: runs from coord workspace, rebases coord onto branch post-merge

The merge algorithm stays the same. The only change is:
- Replace "rebase default workspace onto branch" with "rebase coord workspace onto branch"
- The coord workspace's on-disk files reflect the merge result (same UX benefit)
- Drop the special-casing of "default" throughout

### `maw ws create` / `maw ws destroy`

Workspace directory changes from `.workspaces/` to `ws/`:

```rust
fn workspaces_dir() -> Result<PathBuf> {
    Ok(repo_root()?.join("ws"))
}
```

Destroy now allows destroying any workspace except coord:

```rust
// Before: prevent destroying "default"
// After: prevent destroying "coord"
if name == "coord" {
    bail!("Cannot destroy the coord workspace");
}
```

### `maw ws jj` changes

Remove the default workspace special-casing:

```rust
// Before
if name == "default" {
    repo_root()
} else {
    workspaces_dir()?.join(name)
}

// After
workspaces_dir()?.join(name)
```

### `maw status` changes

The status display currently counts "non-default workspaces". In v2:
- Count all workspaces (they're all in `ws/`)
- Show coord workspace state (branch position, sync status)
- No more "default workspace" in output

### Config changes (`.maw.toml`)

Add workspace directory config for flexibility:

```toml
[repo]
branch = "main"
workspace_dir = "ws"        # default: "ws"
coord_workspace = "coord"   # default: "coord"
```

### Output changes

All `maw ws create` output currently shows `.workspaces/` paths. Update to `ws/`:

```
Before: Workspace 'alice' ready!
        Path: /home/bob/src/maw/.workspaces/alice/

After:  Workspace 'alice' ready!
        Path: /home/bob/src/maw/ws/alice/
```

## Upgrade Plan for Existing Repos

### Automated upgrade: `maw upgrade`

New subcommand that converts a v1 repo to v2:

```bash
maw upgrade
```

Steps:

1. **Preflight checks**
   - Verify jj repo exists
   - Verify no uncommitted changes in default workspace (or auto-commit them as WIP)
   - Verify no active agent workspaces (`maw ws list` — warn if workspaces exist)

2. **Create `ws/` and move workspaces**
   - `mkdir ws/`
   - For each workspace in `.workspaces/`: `jj workspace forget <name>`, move directory to `ws/<name>`, `jj workspace add ws/<name>`
   - Or simpler: destroy all workspaces, create fresh ones in `ws/`

3. **Create coord workspace**
   - `jj workspace add ws/coord`
   - Rebase coord onto main branch

4. **Forget default workspace**
   - Commit any uncommitted changes first
   - `jj workspace forget default`

5. **Set git bare mode**
   - `git config core.bare true`
   - Remove root source files (`src/`, `Cargo.toml`, `README.md`, etc. — everything tracked by git)
   - Keep: `.git/`, `.jj/`, `.gitignore`, `.maw.toml`, `ws/`

6. **Update `.gitignore`**
   - Replace `.workspaces/` with `ws/`
   - Or add `ws/` if `.workspaces/` isn't present

7. **Update `.maw.toml`**
   - Add `workspace_dir = "ws"` if not present

8. **Remove old `.workspaces/`**
   - `rm -rf .workspaces/` (directories were already moved or recreated)

9. **Verify**
   - `jj status` from root → "No working copy"
   - `jj workspace list` → shows coord (and any recreated agent workspaces)
   - `cd ws/coord && jj log --limit 3` → shows expected history

### Upgrade script (standalone)

For repos that don't have the new maw binary yet:

```bash
#!/usr/bin/env bash
set -euo pipefail

# maw v1 → v2 upgrade script
# Run from the repo root

REPO_ROOT="$(pwd)"
BRANCH="${1:-main}"

echo "=== maw v1 → v2 upgrade ==="
echo "Repo: $REPO_ROOT"
echo "Branch: $BRANCH"

# Preflight
if ! jj status &>/dev/null; then
    echo "ERROR: Not a jj repo"
    exit 1
fi

if [ -d ws ]; then
    echo "ERROR: ws/ already exists — already upgraded?"
    exit 1
fi

# Check for uncommitted work
if ! jj diff --stat | grep -q "^$" 2>/dev/null; then
    echo "Committing current working copy changes as WIP..."
    jj commit -m "wip: auto-save before v2 upgrade"
fi

# Destroy existing agent workspaces
echo "Destroying existing agent workspaces..."
for ws_dir in .workspaces/*/; do
    ws_name=$(basename "$ws_dir")
    echo "  Forgetting workspace: $ws_name"
    jj workspace forget "$ws_name" 2>/dev/null || true
done
rm -rf .workspaces

# Create ws/ and coord
echo "Creating ws/coord..."
mkdir -p ws
jj workspace add ws/coord

# Rebase coord onto branch
cd ws/coord
jj rebase -d "$BRANCH" 2>/dev/null || true
cd "$REPO_ROOT"

# Forget default workspace
echo "Forgetting default workspace..."
jj workspace forget default

# Set git bare
echo "Setting git core.bare=true..."
git config core.bare true

# Clean root source files (keep metadata)
echo "Cleaning root source files..."
# Get list of tracked files from git, remove them from root
# But keep: .git, .jj, .gitignore, .maw.toml, ws, .beads, .crit, .agents, notes
for item in *; do
    case "$item" in
        ws|notes) ;; # keep
        *) echo "  Removing: $item"; rm -rf "$item" ;;
    esac
done

# Keep dotfiles except .workspaces
for item in .*; do
    case "$item" in
        .|..|.git|.jj|.gitignore|.maw.toml|.beads|.crit|.agents|.botbox.json) ;; # keep
        .workspaces) echo "  Removing: $item"; rm -rf "$item" ;;
        *) ;; # keep other dotfiles
    esac
done

# Update .gitignore
if grep -q '\.workspaces' .gitignore 2>/dev/null; then
    sed -i 's|\.workspaces/\?|ws/|g' .gitignore
elif ! grep -q '^ws/' .gitignore 2>/dev/null; then
    echo 'ws/' >> .gitignore
fi

echo ""
echo "=== Upgrade complete ==="
echo "Root: $REPO_ROOT"
echo "Coord: $REPO_ROOT/ws/coord/"
echo ""
echo "Verify:"
echo "  jj status                    # → 'No working copy'"
echo "  cd ws/coord && jj status     # → normal workspace"
echo "  jj workspace list            # → coord listed"
echo ""
echo "Next: install maw v2 and use 'maw ws create <name>' to create agent workspaces"
```

## Risks and Mitigations

### `core.bare=true` on non-bare git repo

**Risk**: Unusual git configuration. Some tools may not handle it.
**Mitigation**: jj uses libgit2 internally, not the git CLI, for most operations. Tag push uses git CLI but only needs `git push origin <tag>` which works on bare repos. CI/CD clones from the remote (which is a normal git repo), not from the local bare repo.

### Breaking change for existing workflows

**Risk**: Scripts that assume source files at repo root break.
**Mitigation**: `maw upgrade` is explicit and opt-in. The upgrade script preserves all data. Agents always use absolute workspace paths (already enforced by maw output).

### Coord workspace as single point of coordination

**Risk**: If coord workspace gets corrupted or conflicts, merge/push is blocked.
**Mitigation**: Coord is just a jj workspace — it can be destroyed and recreated (`jj workspace forget coord && jj workspace add ws/coord && cd ws/coord && jj rebase -d main`). No data is stored exclusively in coord.

### Root metadata files (`.beads/`, `.agents/`, `.maw.toml`)

**Risk**: These files live at repo root and are tracked by git. With `core.bare=true`, `git status` won't show changes to them. jj also won't see changes since no workspace tracks the root.
**Decision**: These files must be edited from within a workspace (e.g., `ws/coord/.beads/`), where they'll be properly tracked. The root copies become stale — or better, we delete them from the root and they only exist inside workspace working copies.

Actually, this is the elegant part: since `core.bare=true` means no git working tree at root, and `jj workspace forget default` means no jj working copy at root, the root is truly empty. All files — source code, config, metadata — live in workspaces. The "truth" is in the jj commits, which workspaces materialize on disk.

### Files that maw needs from root

`.maw.toml` needs to be readable from the repo root for `maw init` and `maw push`. Options:
1. Read `.maw.toml` from the coord workspace path instead of repo root
2. Keep a copy at repo root (outside any workspace, manually maintained)
3. Store maw config in `.jj/` directory (not tracked by git)

**Recommendation**: Option 1 — read config from coord workspace. `repo_root()` finds the jj root, `workspaces_dir()` finds `ws/`, and config lives at `ws/coord/.maw.toml` (which is the same file as in any other workspace, since they all share the same jj history).

## Summary of Changes

| Component | v1 (current) | v2 (bare) |
|-----------|-------------|-----------|
| Root directory | Source files + metadata | `.git/`, `.jj/`, `ws/` only |
| Default workspace | Repo root | None (forgotten) |
| Workspace dir | `.workspaces/` | `ws/` |
| Coordination | Default workspace | `ws/coord/` |
| Push mechanism | Default push revset (`..@`) | `--bookmark main` explicit |
| Post-merge rebase | Rebase default onto branch | Rebase coord onto branch |
| Config location | Repo root `.maw.toml` | `ws/coord/.maw.toml` |
| Git mode | Normal (working tree at root) | `core.bare=true` |
| Agent workspace path | `.workspaces/alice/` | `ws/alice/` |
| Botbox upgrades | Run in default workspace | Must create own workspace |

## Implementation Order

1. **Rename `.workspaces/` → `ws/`** — smallest change, biggest path improvement
2. **Add `ws/coord/` concept** — persistent workspace, prevent destruction
3. **Remove default workspace special-casing** — all code paths use `ws/<name>/`
4. **Update `maw push`** — use `--bookmark` explicitly
5. **Update `maw ws merge`** — rebase coord instead of default
6. **Add `maw init --bare`** — new init flow with workspace forget + core.bare
7. **Add `maw upgrade`** — migration script for existing repos
8. **Update docs** — CLAUDE.md, AGENTS.md, all agent instructions
9. **Update `maw status`** — new display for bare model
10. **Eval** — run agent scenarios against v2 to verify UX improvements
