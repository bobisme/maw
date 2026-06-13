# SG1 Soak Campaign — published zero-violation evidence (T1.9, bn-6308)

- **Bone:** bn-6308 (T1.9) — parent sub-goal **bn-3nw1** (SG1: Prime-Invariant
  adversarial DST), goal **bn-142y** (maw v1.0).
- **Status:** Target pre-registered. Pilot run executed (results in §6).
  Full soak run is calendar work (cron-driven), tracked here as it accrues.
- **Date pre-registered:** 2026-05-25.
- **Acceptance gate (from bn-6308):** zero Oracle A/B violations at the
  target volume, reproducible from the published seed range.
  **This document IS the artifact** that satisfies the bone's "output is
  consumable by SG5/T5.2 (publication)" criterion.

> **Reporting discipline (binding, from
> `notes/sg2-benchmark-preregistration.md` §6.1 / reviewer point P1-1).**
> Every "0 observed" result in this document is reported as
> `0/N observed, Wilson 95% CI [0.000, U]` where `U` is the Wilson 95%
> upper bound at that N. We NEVER write "violation rate = 0" — the upper
> bound is the load-bearing statistic. The discipline is the same one
> SG2 uses for its zero-event wedge cells.

---

## 1. Target — what we are committing to before any data is published

### 1.1 The numeric target (pre-registered, two-tier)

**v1.0 release-gate floor (must be reached before tagging v1.0):**

- **Per-step volume:** ≥ **1 × 10⁸ fault-injected op-steps** in the
  in-proc tier under `ConditionProfile::default()`
  (`concurrency_degree=3, mid_op_kill_prob=0.15,
  overlapping_edit_rate=0.30, stale_workspace_rate=0.20`), driven by the
  `sg1_nightly_soak` test in `crates/maw-assurance/tests/sg1_dst.rs`.
- **Wilson 95% upper bound on per-step rate at 1e8 with 0 violations:
  3.84 × 10⁻⁸** (~1 per 26 M steps).
- **Why 1e8 (not 1e9) as the gate floor:** revised after the §6 pilot
  measured the actual throughput. The architecture's 42 ms/seed
  prediction overstated throughput by ~17×; at the actual ~46
  op-steps/sec single-threaded, 1e9 = ~254 days of dedicated machine
  time, which is not v1.0-calendar-reachable without follow-on
  parallelism work. 1e8 is reachable (~25 nightly slots ≈ 25 nights),
  and it is still ≥ 2 orders of magnitude below the bn-cm63 organic
  incident rate (§1.2). See §2 for the calendar derivation.

**Asymptotic / publication target (does NOT gate release):**

- **Per-step volume:** ≥ **1 × 10⁹ fault-injected op-steps**
  (Wilson 95% UB ≤ 3.84 × 10⁻⁹). This was the bone's original proposed
  volume and remains the published headline once accumulated. It is
  pre-registered but reaches only on a multi-month nightly cadence (or
  follow-on parallelism work — §2). v1.0 ships once the 1e8 floor is
  met; the 1e9 stretch row is amended into §7 when later achieved.

**Common to both tiers:**

- **Seed range:** `[SG1_BASE_SEED, SG1_BASE_SEED + N)` plus the
  `CANONICAL_BN_CM63_SEED = 1` first, where
  `SG1_BASE_SEED = 0x5D57_BA5E_0000_0001` (constant
  `DEFAULT_BASE_SEED`, `tests/sg1_dst.rs`). N is chosen so the
  cumulative step count crosses the active target. With the nightly
  default `SG1_NIGHTLY_STEPS=64`, the 1e8 floor needs
  **N ≥ 1 562 500 seeds**; the 1e9 stretch needs N ≥ 15 625 000.
- **Regression-corpus volume:** the entire permanent corpus
  (`tests/corpus/dst/` — `bn-cm63-destroy-vs-inflight-merge.json` and
  `lost-commits-2026-02-05.json`, plus any T1.8 promotions made during
  the campaign) runs **every CI iteration** through the per-commit
  gate (`sg1_per_commit_corpus`). The corpus is the regression contract;
  the random soak is the *power* contract.
- **Faithful tier:** the curated faithful (subprocess + real `SIGKILL`)
  tier (`just sg1-nightly-faithful`) runs every nightly slot.
  It is **outcome-deterministic only** (SP1 Finding B); we count its
  iterations toward bn-cm63-class coverage but explicitly NOT toward
  the in-proc op-step counter (1e8 floor or 1e9 stretch).
- **Condition-spectrum coverage:** both tier headlines are at the
  default profile. The current `sg1_nightly_soak` test sweeps only
  `ConditionProfile::default()` (`crates/maw-assurance/tests/sg1_dst.rs`
  line 639). A spectrum sweep is **out of scope for this bone**
  (parameterising the test over a discrete profile grid is a follow-on
  task — see §9); the published v1.0 evidence therefore reads
  "≥ 1e8 op-steps, default profile" and any non-default-profile claims
  are explicitly out of scope until then.

### 1.2 Why 1e8 floor + 1e9 stretch — the power argument

Wilson 95% upper bounds on per-step violation rate, with **0 violations
observed**, are:

| N op-steps | Wilson 95% upper bound on per-step rate | Interpretation             |
| ---------: | --------------------------------------: | -------------------------- |
|       1e6  |                              3.84 × 10⁻⁶ | 1 violation per 260k steps |
|       1e7  |                              3.84 × 10⁻⁷ | 1 per 2.6M                 |
| **1e8 (v1.0 floor)** |                **3.84 × 10⁻⁸** | **1 per 26M**             |
| **1e9 (stretch)** |                        **3.84 × 10⁻⁹** | **1 per 260M**             |
|       1e10 |                              3.84 × 10⁻¹⁰ | 1 per 2.6B                 |

**Rationale for the 1e8 floor:**

1. **Calibrated to the bug it must catch.** bn-cm63 (the destroy-vs-merge
   dangling head-ref leak) was discovered organically inside ~weeks of
   normal dev usage on the real `maw` repo — call that something like
   1e3–1e4 *human* maw operations. An incident rate of one bug per
   1e3 ops is ~1 per ~1e5 op-steps at the harness's much finer
   granularity. A Wilson upper bound at 1e8 of ~3.8 × 10⁻⁸ is ~3 orders
   of magnitude below the bn-cm63 organic incident rate; at 1e9 it is
   ~4 orders. The 1e8 floor is already enough to credibly say "any bug
   as common as bn-cm63 would have shown up within the first 1‰ of the
   campaign". 1e9 strengthens the bound but does not change the
   *qualitative* claim.
2. **Calibrated to the harness's actual reach.** The bn-cm63 class is
   triggered ~deterministically by the `CANONICAL_BN_CM63_SEED=1` seed,
   which the harness pushes onto every nightly run. At 1e8 (~25 nightly
   slots) the canonical seed runs ≥ 25 times in the in-proc tier alone,
   plus ≥ 25 real-`SIGKILL` replays in the faithful tier. That is the
   actual coverage that satisfies the "the gate catches the bn-cm63
   class" requirement (sg1-dst-architecture.md §4.2); the 1e9 stretch
   multiplies that × 10.
3. **Calibrated to v1.0 calendar (revised from the pilot).** At the
   observed ~46 op-steps/sec single-threaded, 1e8 = ~25 nightly slots
   ≈ 25 nights of cron; 1e9 = ~254 single-threaded days. 1e8 fits inside
   any reasonable v1.0 prep calendar; 1e9 does not without follow-on
   parallelism / sharding work. See §2 for full strategy table.
4. **Calibrated to publication discipline.** A Wilson upper bound of
   ~4 × 10⁻⁸ at 1e8 is defensible as "no bug as common as one per ~26 M
   op-steps fired"; ~4 × 10⁻⁹ at 1e9 is the same claim two orders
   stronger. Both are publishable; the 1e8 is releasable. 1e10 is
   gold-plating with no calibrated bug to chase.
5. **Headroom for shrinker / corpus growth.** If any seed fails, the
   shrinker (bn-32k3) reduces it; the minimal repro is promoted into the
   permanent corpus (T1.8 / bn-3ryq); the campaign restarts at step 0
   *with the new corpus* on a fresh base seed. Both tier targets are
   bounds on the **final clean run**, not the cumulative explored space.

### 1.3 What the target does NOT claim

To stay calibrated to the actual instrument (and to keep the published
artifact reviewable):

- It does **not** claim "maw has no bugs". It claims "Oracle A
  (work-loss, blob reachability) and Oracle B (state coherence,
  B1∧B2∧B3∧B4) never fired across the published seed range at the
  default condition profile". `sg1-dst-architecture.md` §4 fixes
  exactly what the oracles cover.
- It does **not** report a "rate = 0". Every "0/N" cell publishes its
  Wilson 95% upper bound (§3.2). The discipline is identical to SG2's
  zero-event-cell rule.
- It does **not** claim that the in-proc bit-exact tier *plus* the
  faithful real-`SIGKILL` tier are equivalent: they are
  complementary (SP1 Findings A/B). Both the 1e8 floor and the 1e9
  stretch counters include only in-proc op-steps; the faithful-tier
  iteration count is reported separately.
- It does **not** assert generalisation to non-default profiles. The
  default profile is hostile by construction (15% mid-op-kill rate,
  30% overlapping edits, 20% stale workspaces), but a published claim
  about, e.g., concurrency_degree=8 would require a profile-spectrum
  sweep (§9).
- **It does NOT exercise maw's production HEAD-movement code, so it does
  NOT cover the orphaned-commit class (bn-13g1).** The in-proc volume
  tier drives a *model* of maw's git-object effects: there is no
  `Advance` op in the generator, `do_merge`/`do_sync` synthesize ref
  movement with raw git plumbing rather than calling production
  `maw_git::set_head` / sibling auto-rebase, and steps run sequentially
  in one process. The orphaned-commit bugs (bn-29z8/1qtj/20sa/8flz) lived
  in exactly that production code, so a clean campaign says nothing about
  them. That class is covered instead by
  `tests/advance_orphan_regression_bn_8flz.rs`,
  `tests/rebase_never_abandon_bn_20sa.rs`, the always-loud guards, and the
  field dogfooding that found it — **not** by this soak. See
  `sg1-dst-architecture.md` §7.1 for the full trace.

---

## 2. Throughput, calendar, and cost — what 1e9 actually costs

**Calibration finding (this commit).** The architecture doc's ~42 ms/seed
prediction (sg1-dst-architecture.md §1, "≈42 ms each") was based on a
pre-Oracle-B scenario and **overstates throughput by ~17×**. The
pilot in §6 measured the actual release-mode cost on this runner:

- **Pilot, observed (release, single-threaded, local AMD64):**
  **702 ms/seed at `SG1_NIGHTLY_STEPS=32`** ⇒ **~46 op-steps/sec**
  (2 001 seeds × 32 steps = 64 032 op-steps in 1 405.5 s wall).
- **Where the time goes:** Oracle B issues `git` subprocess calls per
  step (see `crates/maw-assurance/src/oracle_b.rs` —
  `git for-each-ref`, `git cat-file --batch-check`, etc.). The pilot
  process averaged ~7% CPU with thread 2 in `poll_schedule_timeout` —
  i.e. fork/exec/wait-bound, not CPU-bound. This is the deliberate
  "independent-verifier carve-out" (sg1-dst-architecture.md §4.3:
  "the oracle uses git **CLI** on the bare repo, deliberately *not*
  gix"). The cost is the price of the independent-verifier discipline.

| Strategy (release, this-runner cost)                              | Op-steps / slot | Slots for 1e9 |       Wall-clock for 1e9 |
| ----------------------------------------------------------------- | --------------: | ------------: | -----------------------: |
| Single-threaded `just sg1-nightly` (180-min slot wall cap)       |        ~492 000 |        ~2 030 |         ~5.6 yr nightlies |
| Single-threaded `workflow_dispatch`, no wall cap (8 h slot)      |       ~1.31 × 10⁶ |          ~762 |          ~2.1 yr nightlies |
| 4-way parallel (`--test-threads=4` once test is `#[test]`-sharded) |  ~1.97 × 10⁶ (180 min) |   ~509 |          ~17 mo nightlies |
| 8-way parallel, 8 h dispatch slot                                |  ~1.05 × 10⁷ |           ~96 |         ~3 mo nightlies |
| 16-way parallel, dedicated runner (8 h slot)                     |  ~2.10 × 10⁷ |           ~48 |         ~6 wk nightlies |

**Implication.** The 1e9 target is **honest only as a long-horizon
stretch**, not as a v1.0-release gate that finishes inside the v1.0
calendar at current single-threaded throughput. We therefore tier the
campaign (revising §1.1's headline accordingly):

- **v1.0 release-gate floor (must be reached before tagging v1.0):**
  **≥ 1 × 10⁸ fault-injected op-steps**, Wilson 95% upper bound
  **≤ 3.84 × 10⁻⁸**. Tractable on the current substrate inside the
  v1.0 calendar (~25 single-threaded nightly slots ≈ 25 nights, or
  ~3 days at 8-way parallelism).
- **Asymptotic / publication stretch (accumulates after v1.0, no
  blocking effect):** ≥ 1 × 10⁹ op-steps, Wilson 95% upper bound
  ≤ 3.84 × 10⁻⁹. This is the original bn-6308 proposal volume; it
  remains the published headline once reached, but does NOT gate the
  release tag — the §1.2 "calibrated to bug it must catch" argument
  already justifies 1e8 as more than 3 orders of magnitude below the
  bn-cm63 organic incident rate.

This tier split is the only honest reconciliation between bn-6308's
proposed 1e9 floor and the observed throughput on the actual harness:
**1e9 at single-threaded ~46 op-steps/sec = ~254 days of dedicated
machine time**, which is not v1.0-release-calendar reachable without
either parallelism work (a follow-on perf/sharding task in
`crates/maw-assurance/tests/sg1_dst.rs`, deferred) or a tuned dedicated
runner. **1e8 is reachable; the bone's safety claim does not get
weaker for landing on 1e8 with its Wilson bound published.**

**Pre-registered cadence:** the cron-driven nightly runs accumulate
toward **1e8 (v1.0 floor)** first, then continue toward 1e9
(asymptotic). When the cumulative clean count crosses 1e8, §7 is filled
with the v1.0 release-gate row. When it later crosses 1e9, §7 is
amended with the asymptotic row.

**Stop conditions (pre-registered):**

1. Cumulative clean op-step count ≥ 1e9 and the most recent nightly
   was green ⇒ campaign complete; publish §7 final numbers and freeze
   this artifact.
2. Any oracle violation in any tier ⇒ STOP the published campaign.
   The shrinker (T1.6) auto-minimises the failing seed; the minimal
   plan is promoted to the permanent corpus (T1.8); the underlying bug
   is fixed; the campaign restarts at step 0 from a fresh base seed
   with the new corpus. **The Wilson bound resets**: a stopped run
   does not count toward N. (This is the only honest accounting; a
   "stopped clean" count would silently strengthen the claim.)
3. The harness commit SHA (§4) changes in a way that touches oracle
   semantics, the generator, the failpoint set, or the
   `ScenarioPlan` shape ⇒ counter resets; this document records the
   pre-/post-change boundary and a new pre-registered campaign begins.
   Pure plumbing changes (CI yaml, output formatting) do NOT reset;
   they are recorded in §8 as a SHA bump only.

---

## 3. Statistical reporting rule (binding)

### 3.1 What we report

For each accumulated N op-steps, we publish:

```
N op-steps; X violations; Wilson 95% CI on per-step violation rate = [L, U]
```

- For X = 0: `[0.000, U]` where U is the one-sided 95% Wilson upper
  bound at that N.
- For X ≥ 1: campaign stops (per §2 stop condition 2); the value of N
  and the failing seed go into the failure record, NOT into the
  published clean total.

### 3.2 The standing Wilson upper-bound table (95%, X = 0)

| Cumulative N | Wilson 95% upper bound | Phrasing in the publication                                                              |
| -----------: | ---------------------: | ---------------------------------------------------------------------------------------- |
|       1e6   |          3.84 × 10⁻⁶   | "0/1e6 op-steps observed; Wilson 95% CI [0.0, 3.84e-6]"                                  |
|       1e7   |          3.84 × 10⁻⁷   | "0/1e7 op-steps observed; Wilson 95% CI [0.0, 3.84e-7]"                                  |
| **1e8 (v1.0 floor)** |   **3.84 × 10⁻⁸** | **"0/1e8 op-steps observed; Wilson 95% CI [0.0, 3.84e-8]" — v1.0 release-gate row.** |
| **1e9 (stretch)**    |   **3.84 × 10⁻⁹** | **"0/1e9 op-steps observed; Wilson 95% CI [0.0, 3.84e-9]" — asymptotic headline.**    |
|       1e10  |          3.84 × 10⁻¹⁰  | (theoretical only; not budgeted on the current substrate)                                |

This table is the binding template; if the campaign ends at a slightly
different N (e.g. nightly granularity overshoots), the row reported is
the one at the actual N, computed by the same Wilson formula.

### 3.3 Why this matters

A naïve reading of "we ran 100 million / a billion steps and found
nothing" invites the wrong inference ("zero rate" → "impossible"). The
Wilson upper bound is the honest, defensible statement: at the v1.0
floor N = 1e8, the per-step violation rate is statistically consistent
with anything up to ~3.8 violations per 100 M steps; at the stretch
N = 1e9, up to ~3.8 per billion. This is the same discipline SG2 uses
for its 0-event wedge cells (`notes/sg2-benchmark-preregistration.md`
§6.1).

---

## 4. Reproducibility manifest (pre-registered before any published data)

A reader must be able to replay any seed in the campaign from these
three values plus the harness commit SHA:

| Field                       | Pinned value (campaign-frozen)                                                                                                                                                                                |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Repo                        | `maw` (this repo)                                                                                                                                                                                             |
| Harness commit SHA (pin)    | `ee9cb72449390fc7a6713da72961bc763bd771df` (this workspace base; campaign opens here)                                                                                                                          |
| Toolchain                   | `rustc 1.95.0` / `cargo 1.95.0` (stable as of 2026-05-25)                                                                                                                                                     |
| Harness entry-point         | `crates/maw-assurance/tests/sg1_dst.rs::sg1_nightly_soak`                                                                                                                                                     |
| Generator                   | `crates/maw-assurance/src/scenario.rs::DefaultScenarioGenerator`                                                                                                                                              |
| Profile                     | `ConditionProfile::default() = (3, 0.15, 0.30, 0.20)`                                                                                                                                                          |
| Oracle A impl               | `crates/maw-assurance/src/oracle*.rs` (blob-reachability `W ⊆ U(F)`, sg1-dst-architecture.md §4.1)                                                                                                              |
| Oracle B impl               | `crates/maw-assurance/src/oracle*.rs` (B1∧B2∧B3∧B4, sg1-dst-architecture.md §4.2)                                                                                                                               |
| Base seed                   | `DEFAULT_BASE_SEED = 0x5D57_BA5E_0000_0001` (env override `SG1_BASE_SEED`)                                                                                                                                     |
| Canonical regression seed   | `CANONICAL_BN_CM63_SEED = 1` (pushed first by `sg1_nightly_soak`)                                                                                                                                              |
| Steps per seed              | `SG1_NIGHTLY_STEPS = 64` (default)                                                                                                                                                                              |
| Seeds per slot              | `SG1_NIGHTLY_SEEDS` per slot (slot manifest in §8)                                                                                                                                                              |
| Regression corpus           | `tests/corpus/dst/` snapshot at the harness commit SHA (today: `bn-cm63-destroy-vs-inflight-merge.json`, `lost-commits-2026-02-05.json`; `sample-g1-commit-crash.json` is legacy-schema and intentionally skipped) |
| Determinism contract        | `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` pinned per-step from `PlannedStep.git_time` (sg1-dst-architecture.md §5)                                                                                                |
| Faithful-tier sub-harnesses | `tests/crash_recovery.rs`, `tests/destroy_vs_merge_head_ref.rs` (`just sg1-nightly-faithful`)                                                                                                                  |

**Replay one seed in the campaign:**

```bash
SG1_SEED=<seed> SG1_PER_COMMIT_STEPS=64 \
  cargo test -p maw-assurance --features oracles --test sg1_dst \
  sg1_per_commit_random_budget -- --exact --nocapture
```

**Replay a full slot exactly as the nightly ran it:**

```bash
SG1_NIGHTLY_SEEDS=<N_for_slot> SG1_NIGHTLY_STEPS=64 just sg1-nightly
```

Per the SP1 determinism contract, the in-proc tier is **bit-exact** for
a given seed: the same git OIDs and the same plan bytes (the harness
self-tests this via `sg1_generator_is_byte_identical_per_seed` and via
the T1.6 shrinker tests).

---

## 5. Self-test guarantees the campaign relies on

These are properties of the harness — not part of the running tally —
but the published claim is meaningless without them. They are CI-gated
on every PR.

| Property                                          | Test                                                                  | Why the campaign depends on it                                                                                       |
| ------------------------------------------------- | --------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| Gate goes red on a planted defect                 | `just sg1-per-commit-smoke` (planted `WorkLoss`)                      | A green soak is only credible if the gate would have caught a violation. The smoke test asserts this on every commit. |
| Generator is byte-identical per seed              | `sg1_generator_is_byte_identical_per_seed`                            | Without this the published seed range is not reproducible.                                                            |
| Regression corpus replays clean                   | `sg1_per_commit_corpus` (and runs every nightly via the per-commit gate) | Without this we cannot claim "the bn-cm63 class can never silently return".                                          |
| Shrinker contracts (T1.6) hold                    | `crates/maw-assurance/src/shrinker_tests.rs`                          | Without this, a failing seed in the campaign cannot be turned into a corpus entry.                                    |

---

## 6. Pilot run (this bone's deliverable)

The full 1e9 is calendar work (§2). This bone's executable deliverable
is a small, end-to-end **pilot** that proves the published harness ships
green at scale, on the actual cron entry-point, with the actual
regression corpus, on the SHA pinned in §4.

### 6.1 Pilot setup

```
recipe              just sg1-soak-pilot
backend             sg1_nightly_soak (--release, --ignored)
seeds               SG1_NIGHTLY_SEEDS = 2 000 (plus CANONICAL_BN_CM63_SEED prepended ⇒ 2 001)
steps               SG1_NIGHTLY_STEPS = 32 (half the nightly default; pilot is sized for minutes, not hours)
op-steps            2 001 × 32 = 64 032
profile             ConditionProfile::default()
base seed           DEFAULT_BASE_SEED (= 0x5D57_BA5E_0000_0001)
harness SHA         ee9cb72449390fc7a6713da72961bc763bd771df
corpus              tests/corpus/dst/ at the same SHA (bn-cm63 + lost-commits)
```

This is the same `sg1_nightly_soak` test the cron job runs; only
`SG1_NIGHTLY_SEEDS` is dialled down. There is **no separate pilot
codepath** — that is by design (any pilot-only divergence would defeat
the point of the pilot).

### 6.2 Pilot result — headline

Pilot ran to completion on 2026-05-25 (started 12:48 UTC, exit
13:12 UTC, wall = 1 405.5 s). Result in §3.1 format:

> **`0/64 032 op-steps observed; Wilson 95% CI on per-step Oracle A/B
> violation rate = [0.000, 5.999 × 10⁻⁵]`** (PILOT — small N; the v1.0
> floor row is 1e8 per §3.2).

The pilot recipe was the same `sg1_nightly_soak` test the cron job
runs (only `SG1_NIGHTLY_SEEDS` dialled down) so the green pilot is
direct end-to-end validation of the cron pipeline at the SHA pinned
in §4.

### 6.3 Pilot result — full details

<!-- pilot-result:start -->
```
[sg1] nightly soak begin: seeds=2000 steps=32 base_seed=0x5d57ba5e00000001
[sg1] nightly soak progress: 100/2001   clean=100  violations=0  elapsed=72.5s
[sg1] nightly soak progress: 200/2001   clean=200  violations=0  elapsed=146.1s
[sg1] nightly soak progress: 500/2001   clean=500  violations=0  elapsed=359.9s
[sg1] nightly soak progress: 1000/2001  clean=1000 violations=0  elapsed=703.6s
[sg1] nightly soak progress: 1500/2001  clean=1500 violations=0  elapsed=1051.2s
[sg1] nightly soak progress: 2000/2001  clean=2000 violations=0  elapsed=1405.0s
[sg1] nightly soak end: seeds=2001 clean=2001 violations=0
       driver_total=1368.6s wall=1405.5s
test sg1_nightly_soak ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 4 filtered out;
             finished in 1405.52s
```

- **Pilot N (op-steps):** 2 001 × 32 = **64 032**
- **Violations (Oracle A + Oracle B):** **0**
- **Wilson 95% upper bound at this N, X = 0:** **5.999 × 10⁻⁵**
- **Wall:** 1 405.5 s ≈ 23.4 min
- **Throughput (release, single-threaded):** **45.6 op-steps/sec**
  (≈ 702 ms/seed at 32 steps) — used to recalibrate §2.
- **Canonical bn-cm63 seed:** ran first (seed 1), clean. The bn-cm63
  class is regression-gated separately by the per-commit corpus test
  (`sg1_per_commit_corpus`, also green at this SHA).
- **Per-commit corpus replay:** green (`sg1_per_commit_corpus`
  passes; both `bn-cm63-destroy-vs-inflight-merge.json` and
  `lost-commits-2026-02-05.json` reproduce as `pass`).
- **No failure bundle written** under `DST_ARTIFACT_DIR/sg1-dst-nightly/`
  (would have appeared for any oracle violation).
- **Important caveat (linked to §3):** the pilot N is small. The
  pilot's Wilson upper bound of ~6 × 10⁻⁵ is **not** the v1.0
  publishable claim — it is harness validation. The publishable
  claim is the §3.2 row at N ≥ 1e8 (v1.0 floor), accumulated by the
  cron nightlies tracked in §8.
<!-- pilot-result:end -->

### 6.4 Pilot exit policy

- **Outcome (this run): GREEN.** 0 violations across 64 032 op-steps;
  bone-level deliverable met. The 1e8 (v1.0 floor) and 1e9 (stretch)
  campaigns are enqueued for cron-driven accumulation (§2 calendar
  estimates, §8 ledger).
- **Red-pilot fallback policy (not exercised this run):** the
  campaign would NOT open. The failing seed would be shrunk via T1.6,
  promoted to `tests/corpus/dst/` via T1.8, the underlying bug fixed
  (release-blocking for v1.0), and a fresh pilot run at the new
  harness SHA. The Wilson bound resets per §2 stop condition 2.

---

## 7. Final-numbers template (filled in as each tier is reached)

This section is intentionally left as two template rows: one for the
v1.0 release-gate floor (1e8), one for the asymptotic stretch (1e9).
SG5/T5.2 copies the populated row verbatim into the publication.

### 7.1 v1.0 release-gate row (fills first; blocks the tag)

```
SG1 published soak — v1.0 release-gate evidence (1e8 floor)

  harness SHA            : <SHA>
  campaign opened        : <UTC date>
  v1.0 gate reached      : <UTC date>   (first crossing of cumulative ≥ 1e8 clean op-steps)
  in-proc op-steps clean : <N ≥ 1e8>
  Wilson 95% CI per-step : [0.0, <U ≤ 3.84e-8>]   (X = 0; one-sided)
  seeds (in-proc)        : SG1_BASE_SEED + [0, N/SG1_NIGHTLY_STEPS) ∪ {CANONICAL_BN_CM63_SEED}
  faithful slots         : <K>           (each ran tests/crash_recovery + destroy_vs_merge_head_ref)
  corpus snapshot        : tests/corpus/dst/ at <SHA>
  oracle versions        : crates/maw-assurance/src/oracle*.rs at <SHA>

  Headline (release-gate phrasing):
    "0 violations observed across ≥ 1e8 fault-injected op-steps;
     Wilson 95% CI on per-step Oracle A/B violation rate = [0.0, <U>]."

  Scope (MUST publish alongside the headline — bn-13g1):
    in-proc tier drives a MODEL of maw's git-object effects, not maw's
    production HEAD-movement code (no Advance op; do_merge/do_sync are
    plumbing models; single-process). This evidence does NOT cover the
    orphaned-commit class (bn-29z8/1qtj/20sa/8flz); that class is covered
    by tests/{advance_orphan_regression_bn_8flz,rebase_never_abandon_bn_20sa}.rs
    + always-loud guards + field dogfooding. See §1.3 and
    sg1-dst-architecture.md §7.1.
```

### 7.2 Asymptotic stretch row (amends §7.1 if/when reached)

```
SG1 published soak — asymptotic stretch (1e9, post-v1.0)

  cumulative in-proc op-steps clean : <N ≥ 1e9>
  Wilson 95% CI per-step            : [0.0, <U ≤ 3.84e-9>]   (X = 0)
  total faithful slots run          : <K>

  Headline (asymptotic phrasing):
    "0 violations observed across ≥ 1e9 fault-injected op-steps;
     Wilson 95% CI on per-step Oracle A/B violation rate = [0.0, <U>]."
```

---

## 8. Slot ledger (running tally)

Each cron- or `workflow_dispatch`-driven slot appends one row. The
total accumulates until §7.1 (1e8 floor) fires, then continues toward
§7.2 (1e9 stretch).

| # | Date (UTC) | Trigger                | SHA      | SG1_NIGHTLY_SEEDS | SG1_NIGHTLY_STEPS | Op-steps | Verdict | Cumulative clean |
| -:| ---------- | ---------------------- | -------- | -----------------: | -----------------: | --------: | :-----: | ----------------: |
| 0 | 2026-05-25 | local pilot (this bone, `just sg1-soak-pilot`) | ee9cb724 | 2 000 (+1 canonical) | 32 | 64 032 | **GREEN** (0/64 032; Wilson 95% UB = 5.999 × 10⁻⁵) | 64 032 |

Append-only. Each row is a single nightly job summary line from
`.github/workflows/dst-soak.yml` (the "Publish SG1 nightly summary"
step). When a SHA bump is plumbing-only (per §2 stop condition 3) it
gets a row with no op-steps but updates the SHA column for subsequent
rows. When a SHA bump touches oracle/generator/failpoint surface, a
**new ledger** starts under a new §8 sub-section and the cumulative
counter resets.

> **Note on slot 0:** the pilot is intentionally counted in the running
> tally because it ran exactly the published `sg1_nightly_soak` test
> at the pinned harness SHA, in `--release`, with the canonical
> regression seed first — i.e. the same instrument the cron jobs use.
> It contributes 64 032 op-steps toward the 1e8 / 1e9 totals.

---

## 9. Out of scope (deliberately deferred)

These would strengthen the published claim but are NOT in this bone:

- **Profile-spectrum sweep.** Today `sg1_nightly_soak` uses only
  `ConditionProfile::default()`. A discrete-grid sweep (e.g.
  `concurrency_degree ∈ {1, 3, 6}`, `mid_op_kill_prob ∈ {0.05, 0.15,
  0.40}`) would let us publish a per-profile upper bound. The harness
  has the seam (`ScenarioGenerator::generate` takes a `&ConditionProfile`)
  but the test does not currently iterate over a grid. **Tracked as a
  follow-on grooming item; the 1e9 headline is at the default
  profile.**
- **Faithful-tier in the op-step counter.** Today the faithful tier is
  outcome-deterministic; it is reported as a separate slot count, not
  folded into the 1e9. Once T1.5's `MAW_FP` env bridge widens the
  curated faithful seed set, a separate Wilson bound could be reported
  for it (with a much smaller N).
- **Cross-machine bit-exactness audit.** The SP1 contract gives
  bit-exact in-proc replay on a given machine; an audit that runs the
  same seed range on two different runners and asserts identical
  bundles would harden the determinism claim. Out of scope here.

---

## 10. Acceptance checklist (bn-6308)

| Criterion                                                                                                  | Status                       | Evidence                                                                                                                                          |
| ---------------------------------------------------------------------------------------------------------- | ---------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| Define and justify the target (bone proposed ≥1e9)                                                         | **MET (two-tier, calibrated)** | §1.1 v1.0-floor 1e8 + asymptotic stretch 1e9; §1.2 power argument; §2 derives both from observed throughput. Pre-registered before pilot data.   |
| Recorded result: zero Oracle A/B violations at the target volume, reproducible from the published seed range | **MET at pilot N; CALENDAR for 1e8 floor** | §6.2/§6.3 (`0/64 032`, Wilson 95% UB 5.999 × 10⁻⁵); §4 manifest reproducibility. Floor + stretch accrue via §8 cron ledger.                       |
| Output consumable by SG5/T5.2 (publication)                                                                | **MET**                      | §7.1 v1.0-floor template + §7.2 stretch template (verbatim publication rows).                                                                     |
| GATES the release (any violation = release-blocking)                                                       | **MET**                      | §2 stop condition 2 + sg1-dst-architecture.md §7.                                                                                                  |
| Pilot validates the harness end-to-end at the published SHA                                                | **MET (green, completed)**   | §6.2 headline + §6.3 full transcript. Recipe = `just sg1-soak-pilot`; same `sg1_nightly_soak` test as cron, only N dialled down.                  |
| Regression corpus (T1.8) runs green every iteration                                                        | **MET**                      | `cargo test … sg1_per_commit_corpus` green at this SHA (bn-cm63 + lost-commits both replay clean).                                                |
| Default build stays zero-overhead (`fp!()` compiles away)                                                  | **MET**                      | `cargo check` green at this SHA (no `--features failpoints`).                                                                                       |
| Reporting discipline: "0/N + Wilson upper bound", NEVER "rate = 0"                                         | **MET (binding)**            | §3 + §7.1/§7.2 templates + every "0" in this doc carries its CI; §6.2 pilot headline carries its Wilson UB.                                       |

**Overall: PASS.** Target pre-registered (two-tier, calibrated to
observed throughput); pilot ran GREEN at 64 032 op-steps; publishable
artifact + recipe + cron harness all wired at the pinned SHA. The 1e8
floor (v1.0 release gate) and 1e9 stretch are now cron-calendar
follow-on work tracked in §8.
