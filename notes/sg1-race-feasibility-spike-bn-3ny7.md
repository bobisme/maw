# SG1 race-feasibility spike (bn-3ny7) — finding

**Question (bn-3ny7):** Is a *deterministic, replayable* multi-process
`set_head` race modelable in the SG1 harness? The answer gates bn-2byw
(production-code SG1 tier), which gates v1.0 (bn-3ctu).

**Outcome: C (single-process deterministic interleaving over *real*
production code) — GO**, scoped as a **separate** production-code-linked
test, NOT an extension of the in-proc volume driver. Reject outcome A (a true
multi-process/threaded deterministic scheduler). Outcome B (documented
limitation + regression tests) is the fallback only if C's one prerequisite
proves intractable — it won't; the prerequisite is ~a day of additive,
feature-gated failpoint work that *also* closes an existing regression-test
gap.

Time spent: well under the 1-week cap. Evidence below is from reading the
harness, the failpoint system, the production rebase/`set_head` path, and the
two regression tests (file:line cited).

---

## 1. The premise contains a false assumption — correct it first

The phrase "multi-process `set_head` race" implies a memory/timing data race
between concurrent threads. **It is not.** The orphaned-commit defect
(bn-29z8/1qtj/20sa/8flz) is a **state/logic bug**: the rebase reaches
`set_head` having replayed **0** commits while the workspace HEAD is genuinely
*ahead* of the old epoch — so HEAD is moved to the new epoch and the
committed-ahead work is abandoned.

Concurrency in the field merely **created the precondition state**, it was not
the failure mechanism. The precondition is: (a) a sibling workspace committed
ahead of the epoch, and (b) a *peer* merge that advanced the epoch and
triggered that sibling's auto-rebase. Both are deterministically constructible
in a single process with real maw ops.

Evidence the defect is a state predicate, not a timing race
(`crates/maw-cli/src/workspace/sync/rebase.rs`):

- The walk is `walk_commits(old_git, head_git, true)` (rebase.rs:362). The
  empty case is an early return; the never-abandon guard is a pure in-line
  state check (rebase.rs:805–863):
  - **(a) CAS:** re-read HEAD; if `current_head != head_git` → `SAFETY ABORT`
    ("a concurrent commit landed and would be orphaned").
  - **(b) Non-consumed-work:** `if replayed == 0 && head_git != old_git` (with
    an `is_ancestor` escape) → `SAFETY ABORT`.
- The code's own comment: *"we should never reach this point with replayed==0
  unless the commit walk failed silently"* (rebase.rs ~808). I.e. the
  empty-walk **trigger** is still unproven (consistent with the bn-3d4a
  residual). The guard defends against the *bad state*; it does not explain
  how the walk went empty.

**Implication:** to gain confidence here we need to drive the **guard** under
the adversarial state deterministically — not to reproduce an OS-level race.

---

## 2. Why outcome A (true multi-process deterministic scheduler) is rejected

1. **Unnecessary for the class.** Per §1 the defect is a state bug. A
   deterministic interleaving (§3) reproduces every variant we care about.
2. **Incompatible with the harness's defining constraint.** The in-proc tier
   *requires bit-exact replay* + delta-debug shrinking
   (`crates/maw-assurance/src/shrinker.rs`; `same_class` equivalence in
   `in_proc.rs:166`). Real threads/processes plus advisory `flock` contention
   (`crates/maw-cli/src/workspace/sync/lock.rs`, `fs4` `try_lock_exclusive`)
   are nondeterministic. The architecture already relegates non-replayable
   interleaving to the **faithful SIGKILL tier** (outcome-determinism only,
   sg1-dst-architecture.md §1). A deterministic *multi-process* scheduler over
   real git + flock is effectively a from-scratch concurrency-DST runtime,
   would still not yield bit-exact shrinking, and is far beyond bn-2byw's
   value. Not worth it.

The one thing a single process genuinely *cannot* verify — whether `flock`
actually keeps two real `maw` processes out of the critical section — is an
OS-primitive guarantee, best covered by a tiny dedicated real-subprocess test
(two `maw` procs racing the same ws lock), analogous to the faithful tier, NOT
a general scheduler. Noted as a small companion item (§5), not part of A.

---

## 3. Why outcome C is feasible — and its two concrete prerequisites

Outcome C is the standard DST technique: model concurrency as a **deterministic
interleaving** with injected yield points (TigerBeetle/FoundationDB style).
For the orphan class it is both sufficient and faithful: build the precondition
state with **real** maw-core ops (real `ws create`/commit/`merge` → real
sibling auto-rebase → real `set_head`), then inject the adversarial condition
(HEAD moved between walk and `set_head`, or an empty walk while ahead) at a
deterministic point and assert the guard fires (`SAFETY ABORT`) — or, pre-fix,
that the oracle catches the orphan.

`set_head`, `walk_commits`, and `checkout_tree` are **100% native gix/Rust
I/O, in-process, no subprocess** (`crates/maw-git/src/checkout_impl.rs:583`;
`rev_walk_impl.rs:56`), so production HEAD-movement code *can* run inside a
linked test. The spike surfaces **two bounded prerequisites**:

### Prereq 1 — there is no deterministic interleaving capability today (additive, ~1 day, feature-gated)
- `FailpointAction` is exactly `Off | Error | Panic | Abort | Sleep`
  (`crates/maw-core/src/failpoints.rs:12`). `check()` returns
  `Result<(),String>` — it **cannot** inject a return value, run a registered
  closure, or yield-and-mutate. `Sleep` is wall-clock, not event-based.
- There is **no failpoint in the rebase walk→guard→`set_head` window**. The
  only nearby sites are `FP_AUTO_REBASE_BEFORE_REPLAY` (auto_rebase.rs:275, at
  entry) and `FP_AUTO_SYNC_BEFORE_CHECKOUT` (checks.rs:226) — both outside the
  critical section.
- **Fix:** (a) add a failpoint site between the HEAD-read/walk and `set_head`
  in rebase.rs; (b) extend `FailpointAction` with a deterministic interleaving
  hook — minimally a **callback action** (run a test-registered closure at the
  point, so the test can mutate HEAD/epoch *there*), or targeted actions
  ("force `walk_commits` empty", "force HEAD = `<oid>`"). All feature-gated
  behind `failpoints`; compiles to nothing in release.

### Prereq 2 — it cannot live in the in-proc *volume* driver (so it doesn't reset the campaign)
- `crates/maw-assurance/src/in_proc.rs` drives **subprocess-`git` plumbing**
  on a TempDir repo with **no real per-ws worktrees** (in_proc.rs:778–782) and
  **no linked maw-core/gix** (`do_merge` synthesizes via `commit-tree` +
  `update-ref`, in_proc.rs:598; `do_sync` is a no-op, in_proc.rs:657).
  Production `set_head` needs a real worktree to check out into — it cannot run
  there.
- **Therefore** the race test is a **separate** deterministic
  interleaving/property test that *links* maw-core/maw-git, builds a real
  consolidated repo with real worktrees, and drives real rebase/`set_head`
  with the injected point. This is exactly bn-2byw's "separate tier / property
  loop" reframe — and it leaves the running 1e8 volume campaign (bn-2yzz)
  untouched (no Wilson reset).

---

## 4. Why outcome B is only the fallback (and is partially already needed)

The existing `tests/rebase_never_abandon_bn_20sa.rs`
(`sibling_auto_rebase_does_not_abandon_committed_workspace`, lines 139–215)
reproduces via the **normal happy path** — a real merge + real auto-rebase,
asserting a *replayed twin* survived. It does **not** force the guard's
adversarial branch; the failpoint injection its own doc-comment (lines 26–32)
describes **was never implemented**. So the `SAFETY ABORT` CAS/non-consumed
branches (rebase.rs:805–863) are currently **unexercised by any test**.

Closing that gap needs the *same* injection capability as Prereq 1. So even a
"document the limitation" outcome benefits from the small failpoint work — it
is worth doing independent of the soak. Pure-B (docs only, no injection) leaves
the guard branch untested, which is the weaker position.

---

## 5. Recommended scope handoff to bn-2byw

1. **Do Prereq 1 first (cheap, high-leverage, standalone value):** add the
   in-window failpoint site + a deterministic interleaving/callback action,
   feature-gated. Immediately write a test that forces `current_head != head_git`
   (and `replayed==0 && head_git != old_git`) and asserts `SAFETY ABORT` —
   closing the §4 regression gap.
2. **Build the separate production-code interleaving tier (Prereq 2):** link
   maw-core/maw-git, real consolidated repo + real worktrees, real
   merge/auto-rebase/advance, with the injected interleaving point. Generate
   the bn-8flz committed-ahead-advance and bn-20sa empty-walk-while-ahead
   states through production code (bn-2byw acceptance items 1 & 4).
3. **Companion (not A):** one tiny real two-`maw`-process test asserting the
   `flock` mutual-exclusion holds on a shared ws lock — covers the single OS
   guarantee the deterministic model can't (§2).
4. **Honesty boundary to preserve in §7.1:** outcome C proves the **guard** is
   sound against the modeled adversarial state; it does **not** by itself
   discover *why* the field walk went empty (bn-3d4a residual, still open).
   bn-2byw must not claim to have found the trigger.

**Net:** the race is modelable — as a deterministic single-process interleaving
over real code, in a separate tier — and the only thing standing between us and
it is a small, additive, feature-gated failpoint capability that pays for
itself by also closing a live regression-test gap. A true multi-process
scheduler is neither needed nor compatible with the harness. v1.0 is not held
hostage to research-grade work.
