# Changelog

All notable changes to maw.

## v0.46.1

### Fixed
- **Default workspace sync semantics**. `maw ws sync` now treats `ws/default` as a persistent branch workspace and skips detached-epoch sync behavior, preventing unwanted HEAD detaches.
- **Brownfield init self-heal for migrated repos**. Idempotent `maw init` now prunes stale git worktree registrations and repairs orphaned `ws/default` worktree linkage when metadata exists but registration is broken.

### Changed
- Ran `rustfmt` across touched Manifold files to keep formatting consistent and reduce future diff noise.

## v0.46.0

### Added
- **Workspace backend auto-selection and benchmarking** (bd-3ca3). `CopyBackend`, `AnyBackend` enum, config-driven `get_backend()`, `auto_select_backend()` per §7.5, criterion benchmark suite.
- **Performance benchmarks eval** (bd-30pm). Snapshot scaling benchmarks (2 repo sizes × 3 change counts), merge partition benchmarks (fixed-total and scaling variants). All §1.1 targets confirmed: create <100ms @30k files, snapshot proportional to changes, merge proportional to touched files.

### Closed (housekeeping)
- bd-3t67 (P1 Epic: Evals — Agent Testing in /tmp) — all 7 eval beads completed
- bd-3iqh (P2 Epic: Phase 4 — CoW Workspace Layer) — all 5 blockers completed
- bd-21zn (P2 Epic: Phase 5 — The Full Manifold) — all 9 blockers completed
- bd-jsw0 (Track 5: Phase 4 CoW backend strategy) — complete
- bd-17nb (Track 6: Phase 5 long-horizon transport) — complete
- **All 502 beads closed** — project roadmap complete through Phase 5

## v0.45.0

### Added
- **Reflink workspace backend** (bd-1rum). `RefLinkBackend` in `src/backend/reflink.rs` — CoW workspace creation via `cp --reflink=auto` from immutable epoch snapshots, with fallback to recursive copy. Full `WorkspaceBackend` trait implementation. 34 tests.
- **OverlayFS workspace backend** (bd-lm5y). `OverlayBackend` in `src/backend/overlay.rs` — zero-copy workspaces via fuse-overlayfs or kernel overlay with user namespaces, immutable epoch lowerdir, per-workspace upper/work dirs, ref-counting for epoch snapshot retention, whiteout detection. 16 tests.
- **Property-testing merge correctness** (bd-1szh). Pushout contract verification in `src/merge/pushout_tests.rs` — 16 property tests covering embedding, minimality, and commutativity with 14,500+ random scenarios per CI run.
- **Expanded tree-sitter semantic conflict detection** (bd-3l3o). Language pack architecture for incremental grammar enablement, semantic conflict rules for symbol lifecycle and signature drift, confidence scoring, machine-readable semantic rationale in conflict output.
- **Workspace templates** (bd-1bei). Template system for bead archetypes with `--template` flag on `maw ws create`.

### Closed (housekeeping)
- bd-3rdu (P2 Epic: Phase 3 — Advanced Merge + Conflict Model) — all 7 blockers completed

## v0.44.0

### Added
- **AST-aware merge** via tree-sitter for Rust, Python, TypeScript (bd-1g5h). Feature-gated `ast-merge` (default on). Falls back from diff3 to AST-level edit scripts for +5-10% merge success.
- **Shifted-code alignment** merge layer (bd-233b). Detects moved code blocks, normalizes positions, retries diff3.
- **Platform CoW detection** (bd-3gzh). Runtime detection of reflink, overlayfs, userns, fuse-overlayfs with auto-selection.
- `maw ws touched` and `maw ws overlap` commands for conflict prediction (bd-3cie). JSON output for orchestrators.

### Closed (housekeeping)
- bd-2yxa (P1 Epic: Phase 1 — Git Worktree Backend) — all 11 blockers completed
- bd-2qfp (P2 Epic: Phase 2 — Patch-Set Model + Git-Native Op Log) — all blockers completed
- bd-18py (Track 1: Phase 1 foundation), bd-e5ej (Track 2: merge safety), bd-3jt0 (Track 3: patch-set model) — all closed
- bd-21sm, bd-29e3, bd-1mjz parent beads and remaining children — all closed

## v0.43.0

### Features
- **Rename-aware merge**: FileId-based rename detection in partition pipeline — reroutes edits to renamed paths, detects divergent renames and rename/delete conflicts (14 tests)
- **Quarantine workspaces**: Failed merge validation creates quarantine workspace for fix-forward; `maw merge promote` and `maw merge abandon` commands (19 tests)
- **Persistent workspace mode**: `--persistent` flag on `maw ws create`, `maw ws advance` rebases onto latest epoch, mode-aware staleness in `maw ws list/status`
- **Concurrent safety eval**: Adversarial interleaving test harness with 5 agents, 100 random scenarios, git fsck integrity checks, determinism validation (29 tests)

## v0.42.0

- Feat: Agent-friendly conflict presentation (JSON structured output) — `ConflictJson` struct with path/reason/workspaces/base_content/sides/atoms/resolution_strategies, `conflict_record_to_json()` conversion, `maw ws merge --format json` structured output for success and conflict cases. 20 tests. (bd-20kb)
- Feat: Upgrade merge engine input from snapshots to PatchSets — FileId and blob OID fields on FileChange/PathEntry, git hash-object enrichment in collect phase, blob-OID equality in resolve phase. 16 new tests. (bd-1mjz.1)
- Feat: Merge preview (`--plan --json`) and derived artifacts — `MergePlan` JSON with deterministic merge_id (SHA-256), `plan_merge()` runs PREPARE+BUILD+VALIDATE without COMMIT, artifacts written to `.manifold/artifacts/`, `--plan` flag on `maw ws merge`. 15 new tests. (bd-1q3f)
- Closed: bd-2hw9 (Phase 1 integration tests, all children done), bd-21sm.3 (eval: conflict detection)
- sha2 dependency added for deterministic merge identifiers

## v0.41.0

- Feat: View checkpoints and log compaction — `src/oplog/checkpoint.rs` with `CheckpointData`/`CheckpointView` serialization, configurable checkpoint intervals, `materialize_from_checkpoint()` for fast replay from latest checkpoint, `compact()` to replace pre-checkpoint chain with synthetic root. 29 tests. (bd-28np.3)
- Feat: ConflictAtoms in merge engine — ConflictRecord now carries `Vec<ConflictAtom>` with line-level conflict localization. `parse_diff3_atoms()` extracts ConflictAtoms from diff3 marker output. Workspace-labeled conflict sides. 8 tests. (bd-15yn.3)
- Feat: Workspace isolation integration tests — 12 tests verifying edit/create/delete/status isolation, 5-workspace concurrent edits, directory isolation, sibling destruction safety, binary files, concurrent create+delete, 50-file bulk test. (bd-2hw9.3)
- Feat: Eval 3-agent parallel disjoint files test + TestRepo::advance_epoch fix to keep refs/heads/main in sync with epoch ref. (bd-21sm.2)
- Closed parent beads: bd-15yn (Conflict model, all 3 children), bd-28np (view materialization, all 3 children). Unblocks merge engine chain (bd-1mjz), conflict presentation (bd-20kb).

## v0.40.0

- Feat: ConflictAtom localization types — replace placeholder with full `Region` enum (Lines, AstNode, WholeFile), `ConflictReason` enum (OverlappingLineEdits, SameAstNodeModified, NonCommutativeEdits, Custom), `AtomEdit` struct (workspace + region + content), and expanded `ConflictAtom` (base_region + edits + reason). Tagged JSON serde. 44 tests. (bd-15yn.2)
- Feat: Global view computation — CRDT merge of per-workspace `MaterializedView`s via `GlobalView` struct. Epoch max, PatchSet pairwise join, destroyed workspace exclusion, cache key validation. Commutative/associative/idempotent. 21 tests. (bd-28np.2)
- Feat: `maw ws undo` — compensation operations via inverse PatchSet. Reads latest Snapshot from op log, computes inverse patches, appends Compensate operation, applies to working directory. Forward/inverse for Add/Delete/Modify/Rename. Redo = undo the undo. 21 tests. (bd-12p7)
- Feat: Workspace lifecycle integration tests — create/list/duplicate/destroy, clean/dirty/stale status assertions. Idempotent destroy. 27 tests. (bd-2hw9.2)

## v0.39.0

- Feat: Structured Conflict model — `Conflict` enum with 4 variants (Content, AddAdd, ModifyDelete, DivergentRename). `ConflictSide` with workspace/content/timestamp, `ConflictAtom` placeholder for region-level conflict localization. Tagged JSON serde. 23 tests. (bd-15yn.1)
- Feat: Per-workspace view materialization — `MaterializedView` struct produced by replaying op log in causal order. Handles all 7 op types (Create, Snapshot, Compensate, Merge, Describe, Annotate, Destroy). Pluggable patch-set reader. `materialize()` and `materialize_from_ops()` APIs. 18 tests. (bd-28np.1)
- Feat: Level 1 Git compatibility — workspace state materialized as `refs/manifold/ws/<name>` via `git stash create`. Config toggle `workspace.git_compat_refs` (default true). Refs pruned on workspace destroy. Enables `git diff refs/manifold/ws/<name>..main` for debugging. 7 tests. (bd-4dsf)

## v0.38.0

- Feat: Stable FileId system — 128-bit random FileId for deterministic rename tracking. FileIdMap with bidirectional path↔id mapping, atomic persistence to `.manifold/fileids`, concurrent rename+edit resolution. 27 tests. (bd-b2y4)
- Feat: OrderingKey with wall-clock clamp guard — composite ordering key `(epoch_id, workspace_id, seq)` for causal ordering. Wall clock clamped monotonically, excluded from Ord. SequenceGenerator for per-workspace sequence+clock management. 23 tests. (bd-1182)
- Feat: Enhanced `maw ws history` — op log first (walks `refs/manifold/head/<name>` blob chain), git commit fallback, JSON/Text/Pretty output formats, payload summarization for all 7 op types. 13 tests. (bd-23w1)
- Feat: Eval scenarios — 5 agent task scenarios with scoring rubric (basic lifecycle, multi-file edit, multi-agent, conflict resolution, read-only inspection). FrictionScore 1-5 scale, RunMetrics collection, EvalReport with target threshold ≤1.5. 19 tests. (bd-29e3.2)

## v0.37.0

- Feat: Op log read — walk the causal chain from head backwards via BFS. Supports max depth limits, stop-at predicates, and diamond DAG deduplication. 21 tests. (bd-1v2t.3)
- Feat: PatchSet computation from working directory diff — `compute_patchset()` builds a PatchSet by parsing `git diff --find-renames --name-status`, collecting untracked files, and verifying blob OIDs. 21 tests. (bd-2xxh.3)
- Feat: Post-merge validation language presets — `LanguagePreset` enum (Rust/Python/TypeScript/Auto) with auto-detection from filesystem markers (Cargo.toml, pyproject.toml, tsconfig.json). Resolution pipeline: explicit → preset → auto-detect. 35+ tests. (bd-1cg0)
- Milestone: Closed bd-2xxh (PatchSet types) and bd-1v2t (operation log) — all children complete. Unblocks downstream: FileId system, Conflict model, OrderingKey, workspace history, view materialization.

## v0.36.0

- Feat: PatchSet join operation — CRDT merge of two patch-sets with commutative, associative, and idempotent properties. Conflict classification with 6 distinct reasons (DivergentAdd, DivergentModify, ModifyDelete, RenameConflict, DivergentRename, Incompatible). 26 tests including property tests. (bd-2xxh.2)
- Feat: Op log write — store operations as git blobs via `git hash-object -w --stdin` with atomic CAS ref updates at `refs/manifold/head/<workspace>`. Single-writer invariant enforced. 16 tests. (bd-1v2t.2)

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
