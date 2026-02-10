# Changelog

All notable changes to maw.

## v0.27.2

- Fix: auto-resolve divergent copies when both are non-empty. After `maw ws sync`, if jj forks a workspace commit into divergent copies that both have file changes, maw now resolves them automatically: identical diffs → abandon non-@, file subset → abandon, superset → squash into @. Previously required ~6 manual jj commands that agents frequently got wrong.

## v0.27.1

- Fix: all jj commands now use `jj_cwd()` helper to run from `ws/default/` instead of bare root. Fixes `maw push`, `maw status`, `maw ws list`, `maw ws create`, `maw ws status`, `maw ws sync`, `maw ws merge`, `maw ws prune`, and `maw ws history` all failing with "working copy is stale" when run from bare root.
- Doctor uses shared `workspace::jj_cwd()` instead of ad-hoc default workspace detection.
- Status removes duplicated `repo_root()` helper, uses `workspace::jj_cwd()`.

## v0.27.0

- New `maw exec <ws> -- <cmd> <args>` command — run any command inside a workspace directory. Validates workspace name (no path traversal), auto-syncs stale workspaces. Generalizes `maw ws jj` to work with any tool (br, bv, crit, cargo, etc.).

## v0.26.1

- Rename "coord" workspace to "default" everywhere — matches jj convention, predictable path (`ws/default/`) for external tools.
- Trim repo root to only `.git/`, `.jj/`, `ws/` — tracked files (`.gitignore`, `.maw.toml`, `.beads/`, etc.) live only in workspaces.
- `MawConfig::load` falls back to `ws/default/.maw.toml` when root copy doesn't exist.
- Fix: post-merge `jj restore` ensures on-disk files in default workspace reflect the merge.
- Fix: `maw ws destroy` and `maw status` run jj from `ws/default/` instead of bare root.
- Fix: `maw init` creates default workspace with `-r main` so source files are present immediately.

## v0.26.0

- **v2 bare repo model**: workspaces moved from `.workspaces/` to `ws/`. Default workspace relocated to `ws/default/`. Repo root is now metadata-only (no source files).
- `maw push` uses `--bookmark` explicitly — works from any workspace.
- `maw ws merge` rebases default workspace onto branch post-merge.
- New `maw init` command sets up bare repo model (forget default ws, core.bare=true, create default).
- New `maw upgrade` command migrates v1 repos to v2 layout.
- Default workspace protected from `maw ws destroy`.

## v0.25.0

- `maw ws jj` now detects stale workspaces and prints a warning with fix command (`maw ws sync`) before running the jj command.
- All workspace path outputs now include trailing `/` for easier copy-paste into file paths.

## v0.24.0

- Add `maw push --advance` flag — moves the branch bookmark to `@-` (parent of working copy) before pushing. Use after committing directly (version bumps, hotfixes). Without the flag, `maw push` now detects unpushed work at `@-` and suggests `--advance`.
- Update all agent docs (CLAUDE.md, AGENTS.md, finish.md) to use `maw push` consistently instead of manual `jj bookmark set` + `jj git push`.

## v0.23.0

- Add `maw push` command — replaces manual `jj bookmark set main -r @-` + `jj git push` workflow. Handles bookmark management, sync checks, and clear error messages.
- Post-merge: rebase default workspace onto branch so on-disk files reflect the merge immediately.
- Add `.maw.toml` config file support with `[merge]` section.
- Add `auto_resolve_from_main` config to auto-resolve conflicts in specified paths (e.g., `.beads/**`) during `maw ws merge`.
- Refine `maw status --status-bar` prompt glyphs and colors for workspace count, change count, and sync warning.
- Add `[repo]` config section with `branch` setting (default: `"main"`) — replaces hardcoded `"main"` throughout.
