# maw: Multi-Agent Workspaces

`maw` is a coordination layer for teams running many coding agents in parallel on one repo.
It gives each agent an isolated workspace, tracks agent lifecycle in Manifold metadata, and merges work back deterministically.

![maw](images/maw-card.webp)

## Why maw is awesome

- **Agent-native UX**: agents get directories, files, and JSON output -- not VCS ceremony.
- **Parallel by default**: each agent works in `ws/<name>/` without stepping on others.
- **Deterministic merge flow**: merge outcomes are based on epoch + workspace patch sets, with predictable conflict surfaces.
- **Operational safety**: repairable repo state, health checks (`maw doctor`), and explicit recovery paths.
- **Git-compatible mainline**: normal `git log`, `git bisect`, and remote workflows still work.
- **Zero-loss merge**: snapshot-rebase-replay preserves uncommitted work in the default workspace during merge -- no silent data loss.
- **Formal verification**: merge algebra proven correct with Kani bounded model checking; protocol state machine verified with Stateright.
- **Deterministic simulation testing**: seeded DST harness replays multi-agent merge scenarios with invariant oracles checking all guarantees every trace.
- **AST-aware conflict detection**: tree-sitter powered semantic analysis for Rust, Python, TypeScript, JavaScript, and Go reduces false-positive conflicts.
- **Crash-resilient recovery**: `maw ws recover --search` finds and restores snapshots, dangling refs, and destroyed workspaces. Every destructive operation leaves a recovery anchor.
- **Persistent workspaces**: long-lived agent workspaces survive epoch advances via `maw ws advance`, rebasing uncommitted work onto the new epoch with structured conflict reporting.
- **Machine-readable everything**: `--format json` on every command. Agents parse structured output, humans get pretty tables -- same data, different rendering.

## Why better than raw git worktrees

Git worktrees are a strong primitive; `maw` adds the missing orchestration layer:

- **Workspace lifecycle**: create/list/status/merge/destroy/restore/undo with consistent semantics.
- **Staleness and sync model**: clear stale detection and workspace sync behavior tuned for multi-agent runs.
- **Structured merge diagnostics**: conflict reporting and machine-readable merge/status output.
- **Policy and guardrails**: safer defaults around merge/destroy/push/release workflows.
- **Automation hooks**: `maw exec`, AGENTS scaffolding, and workflow commands designed for agent toolchains.

## Why better than jj workspaces (for this use-case)

`maw` moved to Manifold + git worktrees because agent concurrency needs isolation more than shared global state:

- **No shared op-log contention**: one agent's status/metadata update does not fork global workspace state.
- **Reduced opfork/divergence failure modes**: far fewer global consistency edge cases under high parallelism.
- **Simpler mental model for agents**: workspace directories with git semantics, no jj-specific recovery knowledge required.
- **Migration and repair tooling**: idempotent `maw init` plus `maw doctor` checks for broken/stale workspace registration states.

## The math and algorithms behind Manifold

Manifold's design (see `notes/manifold-v2.md`) treats workspace state as algebraic data, not ad-hoc shell state:

- **Workspace state model**: `WorkspaceState = base_epoch + PatchSet`.
- **PatchSet join semantics**: patch sets over a shared epoch form a deterministic merge reduction by path.
- **Epoch advancement transaction**: merge computes a candidate result, validates it, then advances `refs/manifold/epoch/current` atomically.
- **Per-workspace operation logs**: single-writer causal histories instead of a single shared mutable op DAG.
- **Structured conflicts**: conflict records are data (with optional AST/semantic metadata), not only text markers.

The practical effect: merge cost and conflict analysis scale with touched paths/conflict set, not the entire repo size or total workspace count.

### Verified properties

The merge algebra and protocol aren't just specified -- they're machine-checked:

- **Kani bounded proofs** (`src/merge/kani_proofs.rs`): 13 harnesses verify `classify_shared_path` correctness -- every pair of file operations produces the right merge action. An additional 11 harnesses verify `resolve_entries` algebra properties (commutativity, associativity, idempotence) behind the `kani-slow` feature gate.
- **Stateright model checking** (`tests/formal_model.rs`): the merge protocol state machine (PREPARE → BUILD → VALIDATE → COMMIT → CLEANUP → DESTROY) is explored exhaustively for deadlock freedom, liveness, and safety invariants.
- **Deterministic simulation testing**: seeded DST harness replays multi-agent merge traces with invariant oracles checking guarantees G1-G6 (epoch monotonicity, rewrite no-loss, no phantom files, destructive gate, merge atomicity, recovery completeness) on every step.

## Install

```bash
cargo install maw-workspaces
```

Or from source:

```bash
cargo install --git https://github.com/bobisme/maw
```

Requires Git 2.40+.

## Quick Start

```bash
# In your repo root
maw init
maw doctor

# Optional: scaffold AGENTS guidance
maw agents init

# Create one workspace per agent
maw ws create agent-1
maw ws create agent-2

# Run commands in each workspace
maw exec agent-1 -- cargo test
maw exec agent-2 -- git status

# Inspect and merge when ready
maw ws status
maw ws merge agent-1 agent-2 --destroy
maw push
```

## Typical multi-agent workflow

1. Lead initializes/validates repo with `maw init` + `maw doctor`.
2. Lead creates workspaces for each agent (`maw ws create <name>`).
3. Agents edit files only inside their workspace paths.
4. Agents run tools via `maw exec <name> -- <cmd>`.
5. Lead monitors progress with `maw ws status` / `maw status`.
6. Lead merges completed workspaces with `maw ws merge ... --destroy`.
7. Lead pushes with `maw push` (or `maw push --advance` when needed).
8. Lead tags release with `maw release vX.Y.Z`.

## Core commands

| Command                              | Description                                      |
| ------------------------------------ | ------------------------------------------------ |
| `maw ws create <name>`               | Create isolated workspace for an agent           |
| `maw ws list`                        | List all workspaces with staleness and commit info|
| `maw ws status`                      | Show workspace health, staleness, and conflicts  |
| `maw ws diff <name> [--against ...]` | Compare workspace changes (summary/patch/json)   |
| `maw exec <name> -- <cmd>`           | Run any command inside a workspace               |
| `maw ws merge <a> <b> [--destroy]`   | Merge one or more workspaces into default        |
| `maw ws destroy <name>`              | Remove a workspace (leaves recovery anchor)      |
| `maw ws restore <name>`              | Restore a previously destroyed workspace         |
| `maw ws sync`                        | Sync stale workspace to current epoch            |
| `maw ws advance <name>`              | Rebase persistent workspace onto new epoch       |
| `maw ws conflicts <name>`            | Inspect merge conflicts before resolving         |
| `maw ws overlap <a> <b>`             | Check file overlap between workspaces            |
| `maw ws history <name>`              | View workspace operation history                 |
| `maw ws undo <name>`                 | Undo local workspace changes                     |
| `maw ws recover [--search]`          | Find and restore lost snapshots and workspaces   |
| `maw init`                           | Initialize/repair Manifold repo state            |
| `maw doctor`                         | Validate repo/tool health and migration state    |
| `maw status`                         | Quick repo + workspace summary                   |
| `maw push [--advance]`               | Push configured branch to remote                 |
| `maw release <tag>`                  | Push and tag a release                           |
| `maw agents init`                    | Add maw instructions to AGENTS.md                |

## Configuration

Create `.maw.toml` in repo root (or `ws/default/.maw.toml`) to customize behavior.

### Auto-resolve conflict-prone paths from main

```toml
[merge]
auto_resolve_from_main = [
  ".beads/**",
]
```

### AST-aware semantic conflict diagnostics (tree-sitter)

```toml
[merge.ast]
languages = ["rust", "python", "typescript", "javascript", "go"]
packs = ["core"]
semantic_false_positive_budget_pct = 5
semantic_min_confidence = 70
```
