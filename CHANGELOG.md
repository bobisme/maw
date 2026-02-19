# Changelog

All notable changes to maw.

## v0.35.0

- **Major**: Git-native backend — replaced jj dependency with direct git worktree operations for workspaces, merge, push, status, and release. No more jj workspace commands needed. (bd-1xfd)
- Feat: `maw gc` command — garbage-collect unreferenced epoch snapshots from `.manifold/epochs/`. Supports `--dry-run` preview. (bd-1xu7)
- Feat: New core data model types: `PatchSet`/`PatchValue` (patch-set model §5.4), `FileId` (stable rename-tracking §5.8), `Operation`/`OpPayload` (operation log §5.3). Foundation for Phase 2 merge and op-log systems. (bd-2xxh.1, bd-1v2t.1)
- Internal: Phase 1 integration tests for workspace lifecycle, merge scenarios, and crash recovery.

## v0.34.2

- Fix: `maw ws create`, `maw ws destroy`, and `maw ws attach` now work from any directory within the repo, not just the repo root. Previously failed with "must be run from the repo root" when invoked via `maw exec default -- maw ws create <name>`. (bd-363g)

## v0.34.1

- Fix: `maw ws merge` now snapshots source workspaces before rebasing/squashing. Workers that only used `maw exec` with non-jj commands (e.g. `cargo test`, `br list`) had on-disk edits that weren't in jj's tree — these were silently lost during merge. Now `jj status` is run in each source workspace to trigger a snapshot first. (bd-1lkg)

## v0.34.0

- Feat: `maw exec` no longer runs `jj status` for non-jj commands. Running `maw exec alice -- cargo test` creates zero jj operations, eliminating opforks from concurrent agent workspaces. Auto-sync only triggers when the command starts with `jj`. (bd-1h8c)
- Feat: `maw ws merge` auto-detects and auto-integrates jj operation forks before merging. Up to 5 integration passes for multi-agent opforks. (bd-1h8c)
- Feat: new `check_opfork()` and `auto_integrate()` helpers in jj.rs for programmatic opfork detection and recovery. (bd-2akx)

## v0.33.0

- Feat: `maw ws merge --check` pre-flight conflict detection. Trial-rebases onto main, detects conflicts, then undoes. Exit 0 = safe to merge, non-zero = blocked. Combine with `--format json` for structured output (`ready`, `conflicts`, `stale`, `workspace`, `description`). (bd-pp6o)
- Fix: `maw ws merge` now returns non-zero exit code when conflicts remain. Previously returned 0 with only a WARNING printed. (bd-pp6o)

## v0.32.0

- Fix: `maw push` now runs `jj git export` before the bookmark push, preventing false "Nothing changed" when the op graph has diverged from concurrent workspace operations. (bd-bjr0)
- Fix: `maw exec` blocks `jj bookmark set <branch>` from non-default workspaces. This prevents agents from accidentally forking the jj operation graph by modifying shared bookmarks. Shows a clear error suggesting `maw ws merge` instead. (bd-fs7c)
- Fix: `maw ws sync` detects divergent commits after sync and auto-abandons empty copies. Prevents data loss when `jj workspace update-stale` picks the wrong copy. (bd-3pxf)
- Fix: `maw status` (including `--watch` and `--status-bar`) degrades gracefully on jj sibling operation errors instead of crashing. Shows `OPFORK!` indicator with fix command, resumes normal display when resolved. (bd-etjx)
- Fix: `maw ws list --format text` uses tab-separated columns with a header row for agent parseability. (bd-3757)

## v0.31.2

- Feat: `--json` is now accepted as a hidden alias for `--format json` on all commands that support `--format` (doctor, status, ws list, ws status, ws history). Hidden from `--help`; conflicts with `--format` if both specified. (bd-3i8u)

## v0.31.1

- Feat: `maw ws restore <name>` recovers a workspace after accidental `maw ws destroy`. Uses jj operation log to revert the forget, rematerialize the directory, and restore file content. Multi-strategy fallback ensures recovery even when op revert alone doesn't recreate the directory. (bd-2nyy)
- `maw ws destroy` now shows `To undo: maw ws restore <name>` in its output.

## v0.31.0

- Feat: `maw init` and `maw upgrade` now set `ui.conflict-marker-style = "snapshot"` in jj repo config. The default jj "diff" style uses `%%%%%%%` and `\\\\\\\` markers that break JSON-based editing tools agents use. Snapshot style fully materializes both conflict sides with JSON-safe markers. Requires jj >= 0.38.0. (bd-2m7c)
- Feat: `maw doctor` checks jj version >= 0.38.0 and warns if conflict-marker-style isn't "snapshot".
- Feat: `maw ws merge` now shows conflict file locations with line ranges and actionable resolution guidance when merge results in conflicts. (bd-2vrn)
- Fix: auto-snapshot default workspace before merge rebase to prevent data loss when default has uncommitted changes. (bd-97m6)

## v0.30.5

- Fix: `maw ws merge` now preserves committed work in the default workspace. Previously, intermediate commits between main and default@ were lost during merge rebase — the whole chain is now rebased, not just the tip. (bd-342s)
- Fix: `maw status` (including `--watch` mode) no longer crashes when the default workspace working copy is stale. `collect_status()` now catches stale errors from `jj workspace list` and `jj status`, sets `is_stale=true`, and continues with degraded data. (bd-ubxq)

## v0.30.4

- Fix: `maw init` compatibility with jj 0.37.0 — ghost `.jj/working_copy/` cleanup moved to after `jj workspace add` succeeds, preventing "No such file or directory" errors during init.
- Refactor: split 3100-line `workspace.rs` into `workspace/` module (9 files: mod.rs, merge.rs, sync.rs, status.rs, create.rs, list.rs, prune.rs, history.rs, names.rs).
- Test: add 13 integration tests covering merge (5), sync (3), push (2), and config/status (3) using real jj repos in temp directories. Fix test helpers for jj 0.37.0 (commit `.gitignore` after init, use `--all` for initial push to new remotes).

## v0.30.3

- `maw init` now previews root cleanup before deleting files. Untracked files (not in any jj/git commit) are skipped with a warning instead of silently deleted. Only jj-tracked files — which are recoverable — are removed.
- Extract shared `count_revset()` and `revset_exists()` jj helpers into new `jj.rs` module, replacing duplicate implementations in push.rs and status.rs.

## v0.30.2

- Fix: use `&Path` instead of `&str` for workspace paths in divergent resolution, removing unsafe `.to_str().unwrap_or(".")` fallbacks.
- Fix: check `git push --tags` exit status and report failures/rejected tags.
- Fix: `maw exec` returns `ExitCodeError` instead of calling `process::exit` directly, preserving exit code passthrough.
- Fix: rename misleading `root` parameters to `cwd` in merge helper functions.
- Fix: upgrade change detection uses jj's "no changes" pattern instead of git's "nothing to".
- Internal: pass `jj_cwd` to `push_tags` instead of re-resolving it.

## v0.30.1

- Fix: post-merge abandon now scoped to only orphaned commits from the workspaces being merged. Previously, the broad revset could abandon empty commits from unrelated active workspaces.
- Fix: `get_current_workspace` no longer returns name with `@` suffix.
- Fix: all deprecated `maw ws jj` suggestions replaced with `maw exec`.
- Fix: `--message` now applies to single-workspace merges via `jj describe`.
- Fix: always use `--colocate` for `jj git init` even without existing `.git/`.

## v0.30.0

- New `maw release <tag>` command — tags and pushes in one step. Creates a jj tag, exports to git, pushes the branch, then pushes the tag. Replaces manual `jj tag set` + `git push origin` workflow.
- Fix: `maw release` uses jj-resolved commit hash for the git tag, preventing stale ref issues.
- Fix: `maw init`, `maw upgrade`, and `maw doctor` now set git HEAD to `refs/heads/main` after enabling bare mode. Prevents "HEAD detached" warnings from git tooling.

## v0.29.3

- Fix: text format uses structured output with `[OK]`/`[WARN]` markers for machine-parseable status.

## v0.29.2

- Fix: remove ghost `.jj/working_copy/` directory that causes root pollution in bare repos.
- Fix: `maw init` runs jj from `ws/default/` when root lacks a working copy.

## v0.29.1

- Add `--format` flag to `maw status`, `maw doctor`, and `maw ws history`. Supports `text`, `json`, and `pretty` formats.

## v0.29.0

- Drop `toon` output format, add `pretty` format with automatic TTY detection. `pretty` is now the default for interactive terminals; `text` for pipes.
- Fix: reject `maw ws merge default` with clear error pointing to `maw push --advance`. Prevents silent edit loss from `jj restore` running on the merge target.
- Fix: `maw status` and `maw doctor` warn if unexpected files exist at the bare repo root.
- Fix: allow dotfiles and agent stubs (`.claude/`, `AGENTS.md`, `CLAUDE.md`) at bare repo root without triggering stray-file warnings.

## v0.28.4

- Internal: botbox upgrades only, no user-facing changes.

## v0.28.3

- Deprecate `maw ws jj` — now errors with suggestion to use `maw exec` instead.
- Fix: tests use temp dirs instead of creating workspaces in the live repo.

## v0.28.2

- Redesign `maw status` output with left-aligned glyphs and clearer labels.

## v0.28.1

- `maw push` now pushes git tags by default (no separate `git push origin --tags` needed).

## v0.28.0

- `maw ws list` wraps JSON/text output in a structured envelope with an `advice` array for actionable suggestions.
- Green checkmarks on ideal-state items in `maw status` — helps spot problems at a glance.
- Hide cursor during `maw status --watch`, restore on exit (q/Esc/Ctrl-C).
- Fix: use `\r\n` in watch mode for correct raw-terminal rendering.

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
