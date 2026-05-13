# Git subprocess inventory (bn-3471)

Refreshed: 2026-05-13
Parent goal: bn-5kad

## Method

Counts are produced from the working tree at the snapshot date with:

```
rg 'Command::new("git")' -n crates src tests benches
```

This regex matches both `Command::new("git")` and `StdCommand::new("git")` because the
latter contains the former as a substring. The two spellings refer to the same shape
of subprocess call (`std::process::Command`); the `Std` alias is used in modules
where another `Command` type is in scope. A separate sweep with
`rg 'StdCommand::new("git")'` confirms the breakdown (24 of the 543 matches use the
`StdCommand::` alias).

Per-call classification is determined by file structure: a match is treated as a
crate-local test fixture if it appears inside a `#[cfg(test)]` block in a crate
source file (the `mod tests { ... }` region at the bottom of the file), and as
production otherwise. Top-level `tests/` files are always test code; everything
under them is integration-test fixture or compatibility assertion.

## Headline counts (2026-05-13)

| Bucket | Calls |
| --- | ---: |
| Total `Command::new("git")` substring matches | **543** |
|   of which `StdCommand::new("git")` alias spellings | 24 |
| Production (replace-now or carveout) | **259** |
| Test code (in-file `#[cfg(test)]` + top-level `tests/` + benches) | **284** |
| Crate `src` and `benches` (production scope) | 471 |
| Top-level `tests/` (always test) | 72 |

Production breakdown (259):

| Sub-bucket | Calls | Notes |
| --- | ---: | --- |
| Production replace-now (local object/ref/index/worktree ops) | **251** | candidates for maw-git/gix migration |
| Production temporary remote carveout (push/fetch protocol) | **8** | see "Carveout" section |

Test code breakdown (284):

| Sub-bucket | Calls | Notes |
| --- | ---: | --- |
| Crate-local `#[cfg(test)]` fixture/setup inside source files | 212 |
| Top-level integration-test fixture/setup in `tests/` | 71 |
| Intentional git-compatibility assertion | 1 | `tests/git_compatibility.rs` only |

The single compat-assertion file is the only place where the test deliberately
runs `git` to assert that maw output is observable via the stock `git` toolchain.
Every other `tests/` hit is fixture/setup (seeding repos, reading refs, hash-
object, rev-parse) and could in principle migrate to maw-git helpers, but that is
strictly lower priority than reducing the production count.

## Definition of done: permanent push/fetch carveouts

The following invocations are intentionally retained as `Command::new("git")`
and must not be counted as "remaining work" against bn-5kad. They are the
**push/fetch protocol carveouts**.

A call qualifies as a permanent carveout only if **all** of the following hold:

1. The first argv after `git` is one of `push`, `fetch`, or `clone`.
2. The call talks to a remote URL (not just local refs).
3. The call is gated by `feature = "transport"` *or* is reachable only through
   the `maw push` / `maw fetch` / `maw lfs push` command surface.
4. The function it lives in has a doc comment that mentions
   "carveout", "transport", "gix-protocol too low-level", or "kept permanently".
5. It is referenced from `docs/git-subprocess-inventory.md` (this file).

Current permanent carveouts (8 production calls):

| File | Lines | Argv |
| --- | --- | --- |
| `crates/maw-cli/src/push.rs` | 61 | `fetch origin --no-tags --quiet` |
| `crates/maw-cli/src/push.rs` | 138 | `push origin <branch>` |
| `crates/maw-cli/src/push.rs` | 463, 469 | `ls-remote` + local `for-each-ref` for tag diff (split: 463 is local — see "Replace-now" below) |
| `crates/maw-cli/src/push.rs` | 505 | `push origin <tag>` |
| `crates/maw-cli/src/transport.rs` | 159 | `push <remote> refs/manifold/* (epoch ref)` |
| `crates/maw-cli/src/transport.rs` | 190 | `push --force <remote> refs/manifold/* (head/ws)` |
| `crates/maw-cli/src/transport.rs` | 378 | `fetch <remote> refs/manifold/*:refs/manifold/remote/*` |
| `crates/maw-git/src/push_impl.rs` | 37 | `push <remote> <local_ref>:<remote_ref>` |
| `crates/maw-git/src/push_impl.rs` | 60 | `push <remote> <tag>` |

(`push.rs:463` is `for-each-ref` listing local tags — strictly a local operation
that should move to gix; classified below under "replace-now". `push.rs:469` is
`ls-remote` and is part of the transport surface.)

Total argv-confirmed permanent carveouts: **8** (not 11 — the local `for-each-ref`
and any other local query calls living next to push/fetch are replace-now, not
carveout).

Any future audit that finds a `Command::new("git").args(["push"|"fetch"|
"clone", ...])` not on this list must either: extend this list with justification,
or be migrated.

## Per-file inventory

Counts split as `production / crate-local-test / total`. Sorted by production
count descending, then total descending. Files under `tests/` and `benches/`
have all their calls classified as test (and so show as `0 / N / N`).

### Production-heavy crate files (the work plan)

| Prod | Test | Total | File |
| ---: | ---: | ---: | --- |
| 22 | 16 | 38 | `crates/maw-cli/src/workspace/working_copy.rs` |
| 15 | 40 | 55 | `crates/maw-cli/src/init.rs` |
| 14 | 0 | 14 | `crates/maw-cli/src/workspace/merge.rs` |
| 13 | 2 | 15 | `crates/maw-cli/src/changes/mod.rs` |
| 9 | 23 | 32 | `crates/maw-cli/src/workspace/recover.rs` |
| 9 | 2 | 11 | `crates/maw-cli/src/push.rs` |
| 7 | 3 | 10 | `crates/maw-assurance/src/oracle.rs` |
| 6 | 0 | 6 | `crates/maw-tui/src/app.rs` |
| 6 | 0 | 6 | `src/merge/quarantine.rs` |
| 5 | 2 | 7 | `src/merge/build_phase.rs` |
| 5 | 0 | 5 | `crates/maw-core/src/backend/copy.rs` |
| 4 | 16 | 20 | `crates/maw-cli/src/workspace/capture.rs` |
| 4 | 16 | 20 | `crates/maw-cli/src/workspace/resolve_structured.rs` |
| 4 | 31 | 35 | `crates/maw-core/src/backend/git.rs` |
| 4 | 0 | 4 | `crates/maw-cli/src/doctor.rs` |
| 4 | 0 | 4 | `crates/maw-cli/src/upgrade.rs` |
| 4 | 1 | 5 | `crates/maw-cli/src/release.rs` |
| 3 | 16 | 19 | `crates/maw-cli/src/transport.rs` |
| 3 | 7 | 10 | `crates/maw-cli/src/workspace/resolve.rs` |
| 3 | 0 | 3 | `crates/maw-cli/src/workspace/create.rs` |
| 3 | 0 | 3 | `crates/maw-cli/src/workspace/diff.rs` |
| 3 | 0 | 3 | `crates/maw-cli/src/workspace/sync/checks.rs` |
| 3 | 0 | 3 | `crates/maw-assurance/src/trace.rs` |
| 2 | 0 | 2 | `benches/workspace_backends.rs` |
| 2 | 0 | 2 | `src/merge/validate.rs` |
| 2 | 0 | 2 | `src/merge/determinism_tests.rs` |
| 2 | 1 | 3 | `src/merge/prepare.rs` |
| 1 | 7 | 8 | `crates/maw-cli/src/ref_gc.rs` |
| 1 | 1 | 2 | `crates/maw-core/src/model/diff.rs` |
| 1 | 0 | 1 | `crates/maw-cli/src/workspace/history.rs` |
| 1 | 0 | 1 | `crates/maw-cli/src/workspace/mod.rs` |
| 1 | 0 | 1 | `crates/maw-cli/src/workspace/oplog_runtime.rs` |
| 1 | 0 | 1 | `crates/maw-cli/src/workspace/sync/cross_target.rs` |
| 1 | 0 | 1 | `crates/maw-cli/src/workspace/undo.rs` |
| 1 | 0 | 1 | `crates/maw-core/src/backend/reflink.rs` |
| 1 | 0 | 1 | `crates/maw-core/src/oplog/view.rs` |
| 1 | 0 | 1 | `crates/maw-git/src/config_impl.rs` |
| 1 | 0 | 1 | `src/merge/resolve.rs` |

### Test-only crate files (in-file `mod tests`)

| Prod | Test | Total | File |
| ---: | ---: | ---: | --- |
| 0 | 17 | 17 | `src/merge/collect.rs` |
| 0 | 13 | 13 | `crates/maw-core/src/oplog/write.rs` |
| 0 | 12 | 12 | `crates/maw-cli/src/epoch_gc.rs` |
| 0 | 11 | 11 | `crates/maw-core/src/refs.rs` |
| 0 | 10 | 10 | `crates/maw-git/src/stash_impl.rs` |
| 0 | 9 | 9 | `crates/maw-core/src/merge/build.rs` |
| 0 | 7 | 7 | `crates/maw-core/src/oplog/read.rs` |
| 0 | 6 | 6 | `crates/maw-core/src/oplog/checkpoint.rs` |
| 0 | 5 | 5 | `crates/maw-cli/src/lfs_push.rs` |
| 0 | 5 | 5 | `crates/maw-git/src/worktree_impl.rs` |
| 0 | 2 | 2 | `crates/maw-core/src/merge/diff_extract.rs` |
| 0 | 2 | 2 | `src/merge/commit.rs` |
| 0 | 1 | 1 | `crates/maw-core/src/merge/materialize.rs` |

In every one of these files the only `git` subprocess calls are in
`#[cfg(test)]` modules. Migrating the production code (which already uses
maw-git) is therefore complete; the remaining work in these files is a
test-fixture cleanup pass and should not block production reduction.

### Top-level `tests/` (integration-test fixture)

All 72 hits are integration-test fixture/setup, except line 13 of
`tests/git_compatibility.rs`, which is a deliberate compat assertion.

| Calls | File | Class |
| ---: | --- | --- |
| 17 | `tests/dst_harness.rs` | fixture |
| 8 | `tests/concurrent_safety.rs` | fixture |
| 6 | `tests/merge_rebase_reconcile.rs` | fixture |
| 5 | `tests/transport.rs` | fixture (push/fetch round-trip) |
| 4 | `tests/manifold_common/mod.rs` | fixture (shared helper) |
| 4 | `tests/lifecycle_properties.rs` | fixture |
| 4 | `tests/merge_scenarios.rs` | fixture |
| 4 | `tests/workflow_dst.rs` | fixture |
| 3 | `tests/action_workflow_dst.rs` | fixture |
| 2 | `tests/destroy_gate.rs` | fixture |
| 2 | `tests/dst_support/mod.rs` | fixture (shared helper) |
| 2 | `tests/release.rs` | fixture |
| 2 | `tests/sync_proptest.rs` | fixture |
| 1 | `tests/auto_rebase_siblings.rs` | fixture |
| 1 | `tests/concurrency_assurance.rs` | fixture |
| 1 | `tests/crash_recovery.rs` | fixture |
| 1 | `tests/git_compatibility.rs` | **compat assertion** |
| 1 | `tests/merge_gate_sidecar.rs` | fixture |
| 1 | `tests/phase0_integration.rs` | fixture |
| 1 | `tests/push.rs` | fixture |
| 1 | `tests/submodule.rs` | fixture |
| 1 | `tests/workspace_undo.rs` | fixture |
| 13 | `crates/maw-git/tests/integration_test.rs` | fixture |

Note that `crates/maw-git/tests/integration_test.rs` lives next to the maw-git
crate (not under top-level `tests/`) but is also an integration test in the
Cargo sense. It runs `git init` etc. against a temp repo before exercising
maw-git APIs.

## Recommended implementation order

Ordering is biased toward hot-path operations users hit on every workspace
action. Each phase aims for a measurable production-call reduction and groups
files that share gix primitives (so the gix-side scaffolding is amortized).

### Phase A — hot-path working-copy / capture / merge / resolve (54 prod calls)

1. `crates/maw-cli/src/workspace/working_copy.rs` — 22 prod
   - Operations: `stash`, `stash pop`, `checkout`, `read-tree`, `diff --name-only`,
     `update-ref`, `hash-object`, `cat-file`, `merge-tree`, `merge-base`. Almost
     every call has a maw-git/gix equivalent. Highest single-file impact.
2. `crates/maw-cli/src/workspace/merge.rs` — 14 prod
   - Operations: `merge-tree`, `update-ref`, `commit-tree`, ref reads. Pair
     with `working_copy.rs` since both consume the same gix `Merger` surface.
3. `crates/maw-cli/src/workspace/capture.rs` — 4 prod
4. `crates/maw-cli/src/workspace/resolve_structured.rs` — 4 prod
5. `crates/maw-cli/src/workspace/resolve.rs` — 3 prod
6. `crates/maw-cli/src/workspace/diff.rs` — 3 prod
7. `crates/maw-cli/src/workspace/create.rs` — 3 prod
8. `crates/maw-cli/src/workspace/sync/checks.rs` — 3 prod
9. Misc workspace stragglers (`history`, `mod`, `oplog_runtime`,
   `sync/cross_target`, `undo`) — 5 prod combined

### Phase B — merge engine in `src/merge/*` (16 prod calls)

10. `src/merge/quarantine.rs` — 6 prod
11. `src/merge/build_phase.rs` — 5 prod
12. `src/merge/validate.rs` — 2 prod
13. `src/merge/determinism_tests.rs` — 2 prod *(misnamed; these are production
    helpers used by `determinism_tests` despite the file name — see notes below)*
14. `src/merge/prepare.rs` — 2 prod
15. `src/merge/resolve.rs` — 1 prod

### Phase C — TUI + assurance + change tracking (29 prod calls)

16. `crates/maw-cli/src/changes/mod.rs` — 13 prod
17. `crates/maw-assurance/src/oracle.rs` — 7 prod
18. `crates/maw-tui/src/app.rs` — 6 prod
19. `crates/maw-assurance/src/trace.rs` — 3 prod

### Phase D — backend + low-level helpers (14 prod calls)

20. `crates/maw-core/src/backend/git.rs` — 4 prod (most are tests already)
21. `crates/maw-core/src/backend/copy.rs` — 5 prod
22. `crates/maw-core/src/backend/reflink.rs` — 1 prod
23. `crates/maw-core/src/oplog/view.rs` — 1 prod
24. `crates/maw-core/src/model/diff.rs` — 1 prod
25. `crates/maw-git/src/config_impl.rs` — 1 prod (replace with `gix-config`)
26. `benches/workspace_backends.rs` — 2 prod (bench harness)

### Phase E — init.rs and admin commands (29 prod calls)

`init.rs` has 15 production calls but they only run during `maw init` (one-shot
admin) and 40 of the 55 hits in the file are inside its `mod tests`. Defer
this until hot-path work is complete.

27. `crates/maw-cli/src/init.rs` — 15 prod (`git init`, `git config`, `git
    update-ref`, etc. for repo bootstrap)
28. `crates/maw-cli/src/release.rs` — 4 prod
29. `crates/maw-cli/src/doctor.rs` — 4 prod
30. `crates/maw-cli/src/upgrade.rs` — 4 prod
31. `crates/maw-cli/src/push.rs` — 2 prod (`for-each-ref` local + rev-list
    count; carveouts excluded)

### Phase F — test fixture cleanup (the long tail)

Once production hits zero (modulo carveouts), tackle test fixtures. Lowest
priority because they do not affect runtime performance and a CLI fixture is
often the clearest representation of the test scenario.

## Notes on movement since the 2026-05-01 snapshot

`bn-5kad`'s prior snapshot reported 522 matches. Current count is **543**
(+21). The drift is composed of:

- **Renamed file**: `crates/maw-cli/src/v2_init.rs` → `crates/maw-cli/src/init.rs`
  (commit `79208cb8 chore: rename module`). Count unchanged at 55. The
  bn-5kad description still references `v2_init.rs` and should be updated.
- **`workspace/recover.rs`**: 28 → 32 (+4). Commit `c1a438a7
  feat(recover): add --restore-file and improve list discoverability (bn-sgm8)`
  added new restore paths that depend on `git show` and `git cat-file`.
- **`src/merge/collect.rs`**: 11 → 17 (+6). Merge-engine improvements
  (`bn-3vf5` auto-rebase siblings, `bn-3bl2` per-workspace baseline) added
  new test fixtures inside `#[cfg(test)]` — production count likely
  unchanged but in-file test fixtures grew.
- **`crates/maw-cli/src/transport.rs`**: 17 → 19 (+2). New in-file tests added.
- **Smaller drift**: A handful of files acquired one or two extra fixture
  calls each from merge-engine work (`bn-103k`, `bn-3vf5`, `bn-3r8s`,
  `bn-2upt`, `bn-3mbj`, `bn-3az5`). None of these increased the
  *production* call count materially.

No file dropped to zero production calls since 2026-05-01. The list of
files with zero production calls (test-only) is the same set as before:
`oplog/write.rs`, `epoch_gc.rs`, `refs.rs`, `oplog/checkpoint.rs`,
`oplog/read.rs`, `merge/build.rs`, `lfs_push.rs`, `stash_impl.rs`,
`worktree_impl.rs`, `merge/materialize.rs`, `merge/diff_extract.rs`,
`merge/commit.rs`, `merge/collect.rs`.

## Notes for the next audit

- The 543-substring count is a useful headline but conflates the
  `Command::new` and `StdCommand::new` spellings. When tracking progress,
  prefer the **production** count (currently 259) and break it down by
  bucket. Carveouts (8) should remain stable; replace-now (251) is the
  number to drive down.
- Files with zero production calls (the "test-only" table) are not work
  items for bn-5kad in the replace-now sense. Treat them as a separate
  long-tail cleanup goal.
- `tests/git_compatibility.rs` should explicitly stay — it asserts external
  `git` tooling continues to interoperate with maw repos. Any future
  hardening should *add* compat assertions, not remove them.
- The audit script lives in `/tmp/classify_git_calls.py` for the duration
  of bn-3471. If it proves useful long-term it should move into `scripts/`.
