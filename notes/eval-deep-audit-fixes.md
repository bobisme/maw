# Fix Plan: maw Deep Audit (bd-1zyq)

Each fix is minimal and self-contained. Grouped by priority.

---

## Critical Fixes

### Fix C1: Remove `cd` from error messages
**Files**: `src/workspace.rs`
**Effort**: Small (2 string changes)

1. **Line 519-526** (`ensure_repo_root`): Remove `cd` suggestion entirely. The error should just say where the repo root is, not instruct `cd`. Better: make maw work from any directory by resolving root internally (bigger change — defer to separate bead if needed). Minimal fix:
   ```
   "This command must be run from the repo root.\n\
    \n  You are in: {cwd}\n  Repo root:  {root}\n\
    \n  Run maw from the repo root directory."
   ```

2. **Line 697-702** (`create` failure): Replace `cd {path} && jj new` with `maw exec {name} -- jj new -m "wip: {name}"`.

### Fix C2: Update `jj-intro` to use `maw push`/`maw release`
**Files**: `src/jj_intro.rs`
**Effort**: Medium (rewrite "How to Push" section)

Replace the manual 4-step push workflow (lines 49-105) with:
```
## How to Push to GitHub
After merging agent work with `maw ws merge`:
  maw push              # push branch to origin
  maw release v1.0.0    # tag + push a release
```

Keep the mental model explanations (bookmarks, @-) but move them to a "Behind the scenes" subsection.

### Fix C3: Update Output Guidelines to use `maw exec`
**File**: `AGENTS.md`
**Effort**: Small (2 line edits)

- Line 349: Change `maw ws jj agent-a describe` → `maw exec agent-a -- jj describe`
- Line 357: Change `"use maw ws jj <name> for jj commands"` → `"use maw exec <name> -- jj <args> for jj commands"`

### Fix C4: Make `push_tags` verify tag export
**File**: `src/push.rs`
**Effort**: Medium

After `jj git export`, verify the target tag exists in git's ref store before calling `git push --tags`. If missing, warn explicitly:
```
Warning: jj tag 'v0.30.0' was not exported to git.
  Use 'maw release v0.30.0' for reliable tag + push.
```

---

## High Fixes

### Fix H1: Rewrite AGENTS.md Quick Start
**File**: `AGENTS.md`
**Effort**: Small

Replace lines 15-30 Quick Start section:
```markdown
## Quick Start

```bash
# Create your workspace
maw ws create <your-name>

# Edit files using the absolute path shown by create:
#   /path/to/repo/ws/<your-name>/src/main.rs

# Set your commit message (like git commit --amend -m):
maw exec <your-name> -- jj describe -m "feat: what you're implementing"

# Check all agent work:
maw ws status

# When done, merge from repo root:
maw ws merge <your-name> --destroy
```
```

### Fix H2: Remove `maw ws jj` from command table
**File**: `AGENTS.md`
**Effort**: Small

Remove the `Run jj in workspace | maw ws jj` row from the table at line 62. Remove the note at line 80 about `maw ws jj`. Replace with a note that `maw exec <name> -- jj <args>` is the canonical way.

### Fix H3: Replace manual tag/push with `maw release`
**Files**: `AGENTS.md`, `.agents/botbox/finish.md`, `.agents/botbox/worker-loop.md`
**Effort**: Small (string replacements)

In each file, replace:
```
jj tag set vX.Y.Z -r main
git push origin vX.Y.Z
```
with:
```
maw release vX.Y.Z
```

Also update AGENTS.md line 333 (Conventions → Release process).

### Fix H4: Add missing release notes
**File**: `AGENTS.md`
**Effort**: Medium (compile from git log)

Add entries for v0.28.5 through v0.30.2 from the git tag history.

### Fix H5: Update `agents.rs` embedded instructions
**File**: `src/agents.rs`
**Effort**: Medium (rewrite push section)

The `maw_instructions()` function generates AGENTS.md content for downstream projects. Update the "Pushing to Remote" section to use `maw push` instead of manual bookmark/push workflow.

### Fix H6: Fix `create` docstring
**File**: `src/workspace.rs:234`
**Effort**: Trivial

Change:
```rust
///   3. Run other commands: cd /abs/path/ws/<name> && cmd
```
to:
```rust
///   3. Run other commands: maw exec <name> -- <cmd>
```

### Fix H7: Check default workspace diffs before merge restore
**File**: `src/workspace.rs`
**Effort**: Small

Before `jj restore` at line 2567, check if default workspace has non-empty diffs:
```rust
let diff_check = Command::new("jj")
    .args(["diff", "--stat", "-r", &format!("{default_ws}@")])
    .current_dir(&default_ws_path)
    .output();

if let Ok(out) = diff_check {
    let diff = String::from_utf8_lossy(&out.stdout);
    if !diff.trim().is_empty() {
        eprintln!("WARNING: Default workspace has uncommitted changes.");
        eprintln!("  These will be overwritten by the merge restore.");
        eprintln!("  To preserve: maw exec default -- jj commit -m 'wip: save before merge'");
        // Don't restore; let the user handle it
        return Ok(());  // or bail with --force option
    }
}
```

---

## Medium Fixes

### Fix M1: Use absolute path in creation message
**File**: `src/workspace.rs:643`
**Effort**: Trivial

Change:
```rust
println!("Creating workspace '{name}' at ws/{name} ...");
```
to:
```rust
println!("Creating workspace '{name}' at {} ...", path.display());
```

### Fix M2: Remove `maw ws jj` from README
**File**: `README.md:40`
**Effort**: Trivial

Remove the `maw ws jj` row from the command table.

### Fix M3: Update `jj-intro` raw commands section
**File**: `src/jj_intro.rs`
**Effort**: Medium (already covered by C2)

### Fix M4: Fix conflict warning to use `maw exec`
**File**: `src/workspace.rs:2092`
**Effort**: Trivial

Change:
```rust
println!("Run `jj status` to see conflicted files.");
```
to:
```rust
println!("  See: maw exec default -- jj status");
```

### Fix M5: Wrap divergent fix instructions in `maw exec`
**Files**: `src/workspace.rs:1148,1232,1612-1622`
**Effort**: Small

Change all `jj abandon <change-id>/0` instructions to `maw exec <ws> -- jj abandon <change-id>/0`.

### Fix M6: Move bookmark after conflict check
**File**: `src/workspace.rs`
**Effort**: Medium (reorder merge steps)

Move the bookmark advancement (lines 2494-2506) to AFTER `auto_resolve_conflicts` (line 2581). This ensures the branch only advances to a clean commit. If conflicts remain, warn and don't advance:
```
WARNING: Merge has conflicts. Branch bookmark NOT advanced.
  Resolve conflicts, then: jj bookmark set <branch> -r <rev>
```

---

## Implementation Order

Recommended batches:

**Batch 1 (doc fixes — no code changes, immediate ship)**:
- Fix H1 (Quick Start)
- Fix H2 (command table)
- Fix H3 (release process docs)
- Fix C3 (Output Guidelines)
- Fix M2 (README)

**Batch 2 (output string fixes — minimal code changes)**:
- Fix C1 (`cd` in errors)
- Fix H6 (docstring)
- Fix M1 (relative path)
- Fix M4 (conflict warning)
- Fix M5 (divergent instructions)

**Batch 3 (jj-intro + agents.rs overhaul)**:
- Fix C2 (jj-intro)
- Fix H5 (agents.rs embedded instructions)

**Batch 4 (merge safety)**:
- Fix H7 (default workspace dirty check)
- Fix M6 (bookmark after conflict check)

**Batch 5 (git interop)**:
- Fix C4 (tag export verification)

**Batch 6 (release notes)**:
- Fix H4 (missing release notes)
