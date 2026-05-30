# bn-3bkn root cause — `maw ws merge --destroy` guts the git dir on the consolidated layout

**Date:** 2026-05-29
**Status:** root cause confirmed (deterministic repro + syscall trace); exact Rust
function to be confirmed by the implementer with a debugger on the repro.
**Repro:** `notes/bn-3bkn-repro.sh` (v2 init → migrate → ws create → ws merge --destroy).

## Reproduction (deterministic)

`bash notes/bn-3bkn-repro.sh` snapshots the git dir after each step:

- after `maw init --legacy-ws`: `repo.git/` healthy (HEAD, config, refs/heads, worktrees, …).
- after `maw migrate` (→ consolidated): still healthy. **Migrate is NOT the culprit.**
- after `maw ws create`: still healthy.
- after **`maw ws merge wsa --into default --destroy --message ...`**: `repo.git/` =
  **`lfs objects refs` only** — HEAD, config, description, hooks, info, logs, index,
  packed-refs, refs/heads, worktrees all gone. `git rev-parse HEAD` → "not a git
  repository". **Exact match to the production incident.**

## Mechanism (syscall-level, via `strace -f -y`)

The merge *commit* succeeds first (epoch advances, branch ref moves — done while the
repo is still healthy). Then `update_default_workspace` (merge.rs:5180) runs to bring
the **default workspace** (which in the consolidated layout **is the repo root**) to
the new epoch. During that, an **in-process maw thread** performs a
**"remove everything not in the target tree" sweep over the repo root**:

```
[pid] unlink(".gitignore")
[pid] unlink(".manifold/events/merge.jsonl")
[pid] unlink("repo.git/COMMIT_EDITMSG")
[pid] unlink("repo.git/HEAD")
[pid] unlink("repo.git/config") ... hooks/* index info/* logs/* refs/heads/main ...
```

Full scope of the sweep: **`repo.git/` (169 entries, recursive), `.manifold/`,
`.gitignore`** — and it **spares all tracked source** (`crates/`, `Cargo.toml`,
`README` untouched). So it keeps target-tree members and deletes non-members. In the
consolidated layout the repo root **contains the untracked admin/git dirs**
(`repo.git/` = the shared common git dir, `.manifold/` = legacy metadata), so the
sweep recursively deletes the git directory itself → repository destroyed.

It removed **untracked** dirs, so it is **NOT** the bn-29x0-fixed
`remove_stale_files` (which only removes *tracked* stale files). This is a
separate/older "not-in-tree" removal in the default-workspace materialization path,
and it has **no exclusion for the admin/git dirs that now live inside the consolidated
root** — a hazard that did not exist in v2 (where the root was metadata-only, not a
checkout being swept). The first failures in the merge log ("Fallback checkout also
failed: … does not appear to be a git repository") are downstream symptoms: by then
the git dir is already gone.

## Why bn-29x0 did not prevent it

bn-29x0 made maw-git `checkout_impl::remove_stale_files` tracked-aware (only removes
files that were tracked AND are absent from the target). But the default-workspace
update here uses `checkout_to` = `git checkout` CLI (working_copy.rs:584) for the
checkout, and a *separate* maw-native sweep removes the not-in-tree extras. That sweep
is the unguarded one. (Same CLASS as bn-29x0, different code path.)

## Fix direction

1. The default-workspace materialization/sweep in the consolidated layout MUST
   hard-exclude the admin/git paths: `.git`, `.maw/`, `repo.git/`, `.manifold/` —
   never delete the resolved `git_dir`/`common_dir` or anything containing it.
2. Add a defensive guard (in the removal primitive) that refuses to `remove_dir_all`
   any path that is, contains, or is contained by the resolved git dir / common dir.
3. Route this path through the bn-29x0-fixed tracked-aware removal, or extend the same
   protection to it.
4. Add `notes/bn-3bkn-repro.sh` (assert repo.git retains HEAD/config/refs/heads/
   worktrees through a consolidated ws merge --destroy) to the DST/integration suite.
5. **Decision:** the consolidated layout must NOT be the v1.0 default until this is
   fixed and proven — revisit the SG3 GO.

## Implementer's fast path to the exact function

Attach a debugger to the repro and break on `unlinkat` for a path matching `repo.git`
(or add a temporary panic in the suspected removal walk). The deleter is in-process
maw code (a thread, per-entry `unlink`/`rmdir`, sparing tracked tree members), reached
from `update_default_workspace` → the default-workspace epoch materialization.
