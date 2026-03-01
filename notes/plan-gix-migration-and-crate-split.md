# Plan: gix migration + crate split

## Context

maw currently shells out to git via `Command::new("git")` for all git operations: 398 call sites across 40 files. This adds subprocess overhead, creates environmental fragility (GPG signing, locale, PATH), makes error handling stringly-typed, and blocks publishing a clean library crate.

[gitoxide/gix](https://github.com/GitoxideLabs/gitoxide) provides pure-Rust git primitives that cover ~95% of what maw needs. The remaining 5% (worktree lifecycle) is buildable from gix primitives.

Simultaneously, maw is a 68k-line single crate. Breaking it into focused crates improves compile times, enables independent versioning, and makes the library surface publishable.

These two efforts reinforce each other: the crate split naturally creates the seams where gix replaces git shell-outs.

## gix capability assessment

Based on source review of gitoxide at ~/repos/gitoxide:

| Operation | gix status | maw call count |
|-----------|-----------|----------------|
| Ref read/write/atomic multi-update | Full (`edit_references()`) | ~60 |
| Rev-parse (revspecs, OID resolution) | Full (`rev_parse_single()`) | ~80 |
| Object read (blob, tree, commit) | Full (`find_object()`, tree traversal) | ~80 |
| Object write (blob, tree, commit) | Full (`write_object()`, `commit_as()`) | ~40 |
| Tree editing | Full (`Editor` with upsert/remove/write) | ~20 |
| Index read/write | Full (`State::from_tree()`, `File::write()`) | ~15 |
| Checkout | Full (`gix-worktree-state::checkout()`) | ~15 |
| Status (dirty detection, file changes) | Full (`status()`, `is_dirty()`) | ~15 |
| Tree-to-tree diff | Full (visitor pattern, rename tracking) | ~20 |
| Config read | Full | ~10 |
| Config write | Incomplete -- write INI directly | ~5 |
| Stash create/apply | Buildable (commit + tree editor + index) | ~15 |
| Worktree add/remove | NOT in gix -- build from primitives | ~20 |
| Push | NOT in high-level API | ~17 |
| **Total** | | **~398** |

**Bottom line**: ~360 of 398 calls have direct gix equivalents. Worktree lifecycle (~20 calls) needs a custom implementation from gix primitives. Push (~17 calls) can use gix-protocol or stay as CLI initially.

## Crate split

### Target structure

```
maw/
  Cargo.toml              (workspace root)
  crates/
    maw-git/              git abstraction layer (gix-backed)
    maw-core/             merge engine, model, refs, oplog, backend
    maw-cli/              clap CLI, workspace commands, format, output
    maw-tui/              ratatui TUI (optional)
    maw-assurance/        DST harness, oracle, Stateright model, Kani proofs
  ws/default/             (development workspace -- still the maw repo itself)
```

### Crate responsibilities

**maw-git** -- git abstraction layer
- Wraps gix behind a trait so the rest of maw never touches git directly
- `GitRepo` trait with methods: `rev_parse()`, `read_ref()`, `write_ref()`, `atomic_ref_update()`, `read_blob()`, `write_blob()`, `read_tree()`, `write_tree()`, `create_commit()`, `checkout_tree()`, `status()`, `is_dirty()`, `diff_trees()`, `worktree_add()`, `worktree_remove()`, `worktree_list()`, `push()`, `stash_create()`, `stash_apply()`
- gix implementation behind the trait
- Fallback to git CLI for push (and anything else gix can't handle yet)
- This crate owns the gix dependency -- nothing else in the workspace imports gix
- Source: new crate, extracted from current `refs.rs`, `transport.rs`, git helpers scattered across modules

**maw-core** -- merge engine and domain model
- `model/` -- types, patch sets, conflicts, ordering, diff, file IDs
- `merge/` -- resolve, build, collect, partition, plan, commit, validate, quarantine, ast_merge, rename, determinism
- `backend/` -- workspace backends (git worktree, copy, overlay, reflink, platform)
- `refs` -- manifold ref namespace (delegates to maw-git for actual git ops)
- `oplog/` -- operation log read/write/checkpoint
- `config.rs` -- maw configuration
- `merge_state.rs` -- merge state machine persistence
- `epoch_gc.rs` -- epoch garbage collection
- `failpoints.rs` -- failpoint macro framework
- Depends on: maw-git
- Source: bulk of current `src/`

**maw-cli** -- binary and workspace commands
- `main.rs` -- clap app, argument parsing
- `workspace/` -- all workspace subcommands (create, destroy, merge, sync, advance, recover, status, diff, etc.)
- `push.rs`, `release.rs` -- push/release workflows
- `doctor.rs` -- health checks
- `status.rs` -- top-level status display
- `v2_init.rs` -- initialization
- `upgrade.rs` -- migration
- `exec.rs` -- command execution in workspaces
- `agents.rs` -- AGENTS.md scaffolding
- `format.rs` -- output formatting (json/text/pretty)
- `error.rs` -- user-facing error display
- Depends on: maw-core, maw-git, maw-tui (optional)
- Source: current CLI-facing modules
- Publishes the `maw` binary

**maw-tui** -- terminal UI (optional)
- `tui/` -- ratatui app, event handling, theme, UI rendering
- Feature-gated in maw-cli
- Depends on: maw-core (for status/model types)
- Source: current `src/tui/`

**maw-assurance** -- test infrastructure (dev-only)
- `assurance/` -- oracle, trace logger, DST model
- Stateright formal model
- Kani proof harnesses
- DST harness and nightly traces
- Contract drift checks
- Depends on: maw-core, maw-git
- NOT published to crates.io -- dev/test only
- Source: current `src/assurance/`, `tests/dst_harness.rs`, `tests/formal_model.rs`

### Dependency graph

```
maw-git          (gix, no other maw crates)
  ^
maw-core         (maw-git)
  ^
maw-cli          (maw-core, maw-git, maw-tui?)
maw-tui          (maw-core)
maw-assurance    (maw-core, maw-git)  [dev-only]
```

## Phased execution

Each phase is independently shippable. Tests pass after every phase. No big bang.

### Phase 0: Workspace Cargo setup
- Convert single crate to Cargo workspace
- Create `crates/` directory with empty crate shells
- Move nothing yet -- just get the workspace compiling with the existing code in a single crate
- Verify: `cargo test`, `cargo build --release`, `just install`

### Phase 1: Extract maw-git (trait + gix implementation)
- Define `GitRepo` trait in maw-git
- Implement with gix for: refs, rev-parse, object read/write, tree ops, index, status
- Implement worktree add/remove from gix primitives
- Implement push via git CLI fallback (wrap `Command::new("git")` for just push)
- Write comprehensive tests against real repos (tempdir)
- Do NOT yet integrate with maw-core -- just the crate and its tests
- Verify: `cargo test -p maw-git`

### Phase 2: Extract maw-core
- Move model/, merge/, backend/, oplog/, refs, config, merge_state, epoch_gc, failpoints into maw-core
- Backend modules depend on maw-git trait instead of `Command::new("git")`
- Wire maw-git into backend/git.rs (biggest single file, 1891 lines, 42 git calls)
- Wire maw-git into refs.rs (16 calls)
- Existing integration tests move to maw-core or stay as workspace-level tests
- Verify: `cargo test -p maw-core`, full test suite still passes

### Phase 3: Extract maw-cli
- Move main.rs, workspace/, push, release, doctor, status, v2_init, upgrade, exec, agents, format, error into maw-cli
- Wire to maw-core and maw-git
- Replace remaining `Command::new("git")` calls with maw-git trait calls
- This is the biggest move but mostly mechanical -- imports change, logic doesn't
- Verify: `cargo test`, `just install`, `maw --version`

### Phase 4: Extract maw-tui
- Move tui/ into maw-tui
- Feature-gate in maw-cli
- Small crate, low risk
- Verify: `cargo test -p maw-tui`, TUI still works

### Phase 5: Extract maw-assurance
- Move assurance/, DST harness, formal model, Kani proofs into maw-assurance
- Keep as dev-dependency / test-only crate
- Verify: `just dst-fast`, `just formal-check`, `just kani-fast`

### Phase 6: Eliminate remaining git CLI calls
- Audit all crates for any remaining `Command::new("git")`
- Replace push with gix-protocol or keep as the sole CLI escape hatch
- Target: zero git CLI calls except push (and even that is negotiable)
- Verify: `grep -r 'Command::new("git")' crates/` shows only push.rs

### Phase 7: Publish
- Publish maw-git, maw-core to crates.io (library crates)
- Update maw-workspaces (CLI) to depend on published crates
- Version alignment across workspace

## Risks and mitigations

**gix is pre-1.0**: API may shift. Mitigation: maw-git trait isolates gix from the rest of maw. If gix breaks, only maw-git changes.

**Worktree creation from primitives**: No prior art in the gix ecosystem. Mitigation: the git worktree format is well-documented and simple (a `.git` file + admin dir + checkout). Build it, test it against real git, keep a CLI fallback.

**Crate split is a big refactor**: 68k lines moving between crates. Mitigation: each phase is independently testable, agents handle mechanical refactors well, and the existing test suite (1100+ tests) catches regressions.

**Push via gix-protocol**: Low-level, less ergonomic than CLI. Mitigation: keep push as CLI initially, migrate later if gix adds high-level push support.

## Non-goals

- Rewriting the merge engine. The merge algebra is correct and verified. Only the git plumbing underneath changes.
- Changing any user-facing behavior. `maw` commands work exactly the same.
- Supporting non-git VCS. maw-git wraps gix, not an abstract VCS layer.

## Effort estimate

| Phase | Scope | Rough size |
|-------|-------|------------|
| Phase 0 | Cargo workspace setup | S |
| Phase 1 | maw-git crate + gix impl | L |
| Phase 2 | maw-core extraction | L |
| Phase 3 | maw-cli extraction | L |
| Phase 4 | maw-tui extraction | S |
| Phase 5 | maw-assurance extraction | M |
| Phase 6 | Eliminate remaining CLI calls | M |
| Phase 7 | Publish | S |
