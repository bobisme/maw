# SG2 Substrate Adapter Parity Audit (T2.3 / bn-mit2)

**Status:** Reviewer-readable. Committed before the first SG2 measured
run. Required by `bn-mit2` AC ("adapter parity reviewed — no adapter
does extra work that biases metrics") and pre-reg §8.4.

This document is the **acceptance gate** for the three SG2 substrate
adapters (`maw-bench-adapters` crate). For every scripted op in
`maw_bench_adapters::ScriptedOp` (the equivalence-test surface), each
adapter's step-by-step behaviour is recorded, side by side, with an
**asymmetry justification cell** for any step one adapter performs that
others do not.

The rule: **equivalence is the load-bearing property.** Any operation an
adapter performs that one of the other two adapters does NOT need to
perform must either (a) be essential to that substrate's contract — and
justified here — or (b) be removed from the adapter. There is no third
option.

---

## How to read this table

- **Op**: the abstract operation the bench harness drives.
- **maw / worktrees+convention / jj-workspaces**: the substrate-native
  steps the adapter performs, in order.
- **Asymmetry justification**: when a step exists in one column but not
  another, the cell names *why* it is essential to that substrate's
  contract, not just convenient. If the cell says "convenience", the
  step is a parity bug and must be removed.

---

## Bootstrap (`Adapter::new`)

Bootstrap is NOT a scripted op — it is the per-run substrate setup the
harness amortizes across the run. Included here because the bootstrap is
the single largest source of asymmetry and the adapter parity reviewer
must see it.

| step                                   | maw                                                                                                          | git-worktrees-bare                                                | jj-workspaces                                                                                 |
| -------------------------------------- | ------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------- | --------------------------------------------------------------------------------------------- |
| make tempdir                           | yes                                                                                                          | yes                                                               | yes                                                                                           |
| seed git repo with one commit          | `git init -b main` + identity + README + `git commit -m init`                                                | `git init --bare repo.git` then `git clone repo.git main` + initial commit + push back | `jj git init --colocate` + describe "init" + `jj new` |
| substrate-specific bootstrap           | `maw init` (transforms layout to bare v2 — `.git/`, `repo.git/`, `ws/default/`)                              | NONE (the bare clone IS the substrate layout)                     | NONE (`jj git init --colocate` already established colocated layout)                          |
| pin integration head                   | `maw init` outputs the `main`/`epoch₀` ref                                                                   | `git symbolic-ref HEAD refs/heads/main` on bare repo + push       | `jj bookmark create -r @ main`                                                                |
| **asymmetry justification**            | `maw init` is essential — without it `maw ws create/merge` cannot operate (this is the maw substrate contract) | Bare-repo + cloned worktree IS git-worktrees' substrate; nothing to add | Colocated init is jj's substrate contract; bookmark create is the integration-head equivalent |

The bootstrap cost is **NOT a measured metric**: pre-reg §1.1 measures
per-run agent behaviour, not adapter bootstrap. Each adapter is
constructed once per run before metric collection begins.

---

## `ScriptedOp::Create { ws, base }`

| step                                | maw                                                                                       | git-worktrees-bare                                                                                                  | jj-workspaces                                                                                                                              |
| ----------------------------------- | ----------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| create workspace                    | `maw ws create <ws> --from <base>`                                                        | `git worktree add -b <ws> <abs-path> main`                                                                          | `jj workspace add --name <ws> <abs-path>` + `jj new main -m wip`                                                                           |
| pin per-workspace identity          | inherited from bare-repo config (set in bootstrap)                                        | `git config user.name/email` inside the new worktree (per-worktree config not inherited from bare repo)             | `JJ_USER` / `JJ_EMAIL` env (jj reads them per command)                                                                                     |
| write claim file                    | NONE — maw has built-in coordination                                                      | `echo … > .coord/<ws>.claim`                                                                                        | NONE — jj's op-log IS the coordination record                                                                                              |
| **asymmetry justification**         | `maw ws create` writes `.manifold/...` state files (epoch ref, head ref, lock metadata). These are essential to the maw contract; they replace the claim file and the in-worktree identity dance. | Per-worktree git config is essential — without it, `git commit` fails with "Please tell me who you are". The claim file IS the convention's coordination surface (advisory). | `jj` propagates identity via env on every invocation; bootstrap pinning would be a hidden state file the convention/jj arms don't have. The `jj new main -m wip` step is the jj substrate's documented "fresh @ on top of integration head" pattern, mirroring `git worktree add … main`. |

**No-extra-work check:** maw skips the convention's claim file because
its substrate already records workspaces in `.manifold/`. Worktrees+
convention skips `.manifold/` writes because they are not part of the
substrate. jj skips both because the op-log is the substrate record.
Each step exists where it does because it is the substrate's own
contract — none is added for benchmark convenience.

---

## `ScriptedOp::Edit { ws, path, content }`

| step              | maw                            | git-worktrees-bare             | jj-workspaces                  |
| ----------------- | ------------------------------ | ------------------------------ | ------------------------------ |
| ensure parent dir | `fs::create_dir_all`           | `fs::create_dir_all`           | `fs::create_dir_all`           |
| write file        | `fs::write(<ws>/<path>, ...)`  | `fs::write(<ws>/<path>, ...)`  | `fs::write(<ws>/<path>, ...)`  |

**Strictly equivalent.** No asymmetry — file write is a pure fs op for
every substrate. (jj's auto-snapshot occurs at the next `jj` command,
not on edit; this is jj's contract and matches what an agent driving jj
sees.)

---

## `ScriptedOp::Commit { ws, msg }`

| step                  | maw                                                  | git-worktrees-bare                                | jj-workspaces                                                              |
| --------------------- | ---------------------------------------------------- | ------------------------------------------------- | -------------------------------------------------------------------------- |
| stage                 | `git add -A` (inside `ws/<ws>`)                      | `git add -A`                                      | NONE — jj auto-snapshots on the next `jj` command                          |
| commit                | `git commit -m <msg>`                                | `git commit -m <msg>`                             | `jj describe -m <msg>` (renames @)                                          |
| advance @ / move pointer | NONE — git's HEAD already advanced                  | NONE                                              | `jj new -m wip` (creates a fresh empty @ child, so subsequent edits go to a new commit) |
| publish workspace tip | implicit (branch tip)                                 | implicit (branch tip)                             | `jj bookmark set <ws> -r @-` (publish the just-described change as a bookmark) |
| **asymmetry justification** | maw uses ordinary git inside the worktree — same shape as worktrees+convention; no extra step. | The `git add` + `git commit` pair is git's contract. | `jj describe`+`jj new`+`jj bookmark set` is the jj-equivalent of git's "commit then move on" — three commands because jj's working-copy-as-a-commit model needs them. Skipping `jj new` would leave subsequent edits *amending* the just-committed change (jj's default), which would silently violate the equivalence with git's commit semantics. Skipping the bookmark set would leave the merge step unable to refer to the workspace tip by name. |

---

## `ScriptedOp::Merge { srcs, target, destroy }`

| step                                       | maw                                                                                        | git-worktrees-bare                                                                                                                                            | jj-workspaces                                                                                                                                                                              |
| ------------------------------------------ | ------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| accept `target ∈ {"default", "main"}`      | `--into <target>` (native verb)                                                            | mapped to integration worktree (`main`)                                                                                                                       | mapped to integration workspace (the `main` bookmark)                                                                                                                                       |
| precondition: integration worktree on `main`| automatic (maw owns ws/default)                                                            | explicit `git checkout main`                                                                                                                                  | automatic (the integration workspace's @ is always reachable)                                                                                                                              |
| merge command                              | `maw ws merge <srcs...> --into <target> --message "..."` [+ `--destroy` if asked]          | `git merge --no-ff -m "..." <srcs...>` (octopus)                                                                                                              | `jj new main \| <srcs...> -m "..."` (creates an n-way merge commit) + `jj bookmark set main -r @` + `jj git export`                                                                          |
| conflict semantics                         | maw merge engine surfaces structured conflicts (`has_conflicts: true`); adapter sets `conflicted=true` | `git merge` aborts on conflict; adapter then `git merge --abort` to keep integration clean (the convention's documented rule) and sets `conflicted=true`     | jj records the conflict in the merge commit (first-class conflict); adapter sets `conflicted=true`. Sources are NOT destroyed.                                                              |
| destroy sources (if asked + non-conflict)  | `--destroy` flag handled natively by maw (which captures recovery snapshots)               | per-source `git worktree remove` + `git branch -D <ws>` + archive `<commit-tip>` into `.coord/destroyed/<ws>`                                                | per-source `jj workspace forget <ws>` + `fs::remove_dir_all(<ws-dir>)`                                                                                                                     |
| **asymmetry justification**                | The recovery snapshot is **maw's Prime Invariant in action** (pre-reg §1.2: "maw's `irrecoverable_lost_work` is expected to be ≈0 by design"). Not an extra step — it is the maw substrate. | The `git merge --abort` post-conflict is the convention's documented rule (§2.4 of `sg2-worktrees-convention.md`). The reflog + commit-tip archive is the convention's **entire** recovery surface — explicitly absent of maw-style snapshot refs (this asymmetry is precisely what SG2 measures). | `jj workspace forget` is the jj substrate's documented destroy verb (recoverable via `jj op restore`). The post-forget `fs::remove_dir_all` matches the post-destroy fs state of the other two arms; without it, the equivalence test's "live workspaces is empty" assertion would falsely fail on jj. `jj git export` flips the colocated git HEAD to the merge commit so the integration dir's file walk sees the merged tree (jj's working-copy-as-a-commit model normally lags the git HEAD until export). |

---

## `ScriptedOp::Destroy { ws, force }`

| step                              | maw                                                                                | git-worktrees-bare                                                                                  | jj-workspaces                                                  |
| --------------------------------- | ---------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- | -------------------------------------------------------------- |
| destroy verb                      | `maw ws destroy <ws> [--force]`                                                    | `git worktree remove [--force] <abs-path>` + `git branch -D <ws>` + archive into `.coord/destroyed/` + remove claim file | `jj workspace forget <ws>` + `fs::remove_dir_all(<ws-dir>)` |
| recovery snapshot                 | automatic (always captured, force-independent: maw Prime Invariant)                | reflog + commit-tip archive (`<commit-tip>` written to `.coord/destroyed/<ws>`)                     | op-log entry (recoverable via `jj op restore <op>`)            |
| **asymmetry justification**       | Same as merge: recovery snapshot IS the maw substrate. Without it, maw is a different substrate. | The reflog + tip archive is the convention's documented recovery surface (§2.5 of `sg2-worktrees-convention.md`). | The op-log entry is jj's documented recovery surface. We don't expose it via `StateSnapshot.destroyed_workspaces` because the maw/wt arms do, and adding it would inflate jj's apparent "recovery completeness" — the parity table is explicit that jj's recovery is via op-log only (SP3 §1 already proved this is brittle under wedge). |

---

## `Substrate::state_snapshot`

The substrate-neutral surface. Equivalence tests assert that the
`integrated_files` field is BYTE-IDENTICAL across all three adapters for
the same scripted op stream (modulo the metadata files explicitly
filtered in `tests/equivalence.rs`).

| field                  | maw                                                       | git-worktrees-bare                                  | jj-workspaces                                                                          |
| ---------------------- | --------------------------------------------------------- | --------------------------------------------------- | -------------------------------------------------------------------------------------- |
| `integration_head`     | `"default"`                                               | `"main"`                                            | `"main"`                                                                               |
| `live_workspaces`      | parsed from `maw ws list` (skipping `default`)            | parsed from `git worktree list --porcelain` (skipping `main`) | parsed from `jj workspace list` (skipping `default`)                                  |
| `destroyed_workspaces` | parsed from `maw ws recover` (best-effort; opt-in)        | filenames under `.coord/destroyed/`                 | EMPTY (jj's recovery surface is the op-log; not exposed)                                |
| `integrated_files`     | walk `<root>/ws/default/` (skipping `.git/`, `.jj/`)      | walk `<root>/main/` (skipping `.git/`)              | walk `<root>/repo/` (skipping `.git/`, `.jj/`) — `jj git export` keeps git HEAD synced |

The label difference (`default` vs `main`) is a **substrate-native**
artifact — each substrate names its integration head per its own
conventions. The equivalence test compares `integrated_files` only, not
labels.

---

## Excluded asymmetries (intentional)

These per-adapter artifacts are NOT compared by the equivalence test;
they are documented here for the reviewer audit:

- maw: `.manifold/` directory, `refs/manifold/recovery/<ws>/` refs.
  Substrate-native; excluded by `filter_substrate_metadata` in the
  equivalence test.
- worktrees+convention: `.coord/` directory. Substrate-native (the
  convention's surface); excluded similarly.
- jj-workspaces: `.jj/` directory, divergent change-ids, op-log
  entries. Substrate-native; excluded by `collect_files` skipping `.jj`.
  **The SP3 opfork-wedge** that surfaces via `jj` subprocess errors is
  NOT excluded — it propagates to the harness as
  `SubstrateError::SubprocessFailed`, which T2.2 classifies as
  `counted_substrate_failure` per pre-reg §8.7.

---

## Reviewer checklist (binding)

A reviewer signing off on adapter parity must confirm:

- [ ] Every step in every adapter's source maps to a row in this table.
- [ ] Every cell in the "asymmetry justification" column names the
      substrate contract requiring the step, NOT "convenience".
- [ ] The `tests/equivalence.rs` `filter_substrate_metadata` filter
      matches the "Excluded asymmetries" list above exactly.
- [ ] `notes/sg2-worktrees-convention.md` matches the worktrees column
      step-for-step (the convention is the substrate; the adapter is its
      encoding).
- [ ] The jj adapter contains zero workarounds for the SP3 opfork-wedge
      (`tests/jj_opfork_wedge.rs` `#[ignore]`-gated must reproduce the
      wedge fingerprints).

If any item fails, the offending adapter step is removed or the table is
amended (with the amendment timestamped, before the next measured SG2
run).
